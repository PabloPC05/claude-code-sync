//! Syncthing mode: `claude-code-sync guard`.
//!
//! Here Syncthing, not git, carries `~/.claude/projects` between two PCs. It
//! knows nothing about chats, and a Claude Code window keeps its transcript
//! in memory after loading it, so three things go wrong when a chat is
//! continued on one PC while a window on the other still has it open:
//!
//! - the window's next message forks the file off its old leaf and hides the
//!   other PC's turns;
//! - closing the window (`/exit`) appends entries off that old leaf, and
//!   `claude --continue` then shows the old branch;
//! - both PCs changing a file while apart leave a `*.sync-conflict-*` copy
//!   Claude Code never shows.
//!
//! The guard is a handful of Claude Code hooks (`guard hook <event>`), no
//! daemon, no network, no git:
//!
//! - every window has a record, `<config dir>/guard/windows/<pid>.json`: the
//!   identity of its transcript file (Syncthing replaces a file by writing a
//!   temporary one and renaming it over; Claude's own appends never change
//!   the identity), its leaf, and every uuid it had loaded;
//! - `UserPromptSubmit` ([`verdict`]) blocks a prompt when the file was
//!   replaced and now holds messages the window never saw, or, in a window
//!   the `claude` shell function started, closes it so the function reopens
//!   the chat with the typed text ([`crate::reload`]);
//! - `SessionStart` ([`record`]) records what the window loaded;
//! - `SessionEnd` ([`ended`]) takes what the window appended off its old
//!   leaf out of the transcript, with the repair a git-mode pull uses;
//! - every hook sweeps the conflict copies ([`sweep_conflicts`]).

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::branches::{self, Reconciled, Repair, Side};

// ---------------------------------------------------------------------------
// File identity
// ---------------------------------------------------------------------------

/// What tells one file from another that took its name.
///
/// Unix: device and inode. Windows: volume serial number and file index.
/// `birth` (creation time, where the file system keeps one) covers an inode
/// a later replacement may be handed again; it only counts when both sides
/// have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birth: Option<u64>,
}

impl FileId {
    /// Whether both describe the same file.
    pub fn same_file(&self, other: &FileId) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && match (self.birth, other.birth) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// What a hook needs to know about the open transcript.
#[derive(Debug, Clone, Copy)]
struct Stat {
    id: Option<FileId>,
    len: u64,
    mtime_ns: u64,
}

fn nanos(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as u64)
}

#[cfg(unix)]
fn device_and_index(_file: &File, meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn device_and_index(file: &File, _meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: the handle belongs to `file`, open for the whole call, and the
    // information struct is plain data the call fills in.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0 {
            return None;
        }
        Some((
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn device_and_index(_file: &File, _meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn stat(file: &File) -> Result<Stat> {
    let meta = file.metadata()?;
    let id = device_and_index(file, &meta).map(|(dev, ino)| FileId {
        dev,
        ino,
        birth: meta.created().ok().and_then(nanos),
    });
    Ok(Stat {
        id,
        len: meta.len(),
        mtime_ns: meta.modified().ok().and_then(nanos).unwrap_or(0),
    })
}

/// The identity of the file at `path`, or `None` when it is missing or the
/// platform cannot tell.
pub fn file_identity(path: &Path) -> Option<FileId> {
    stat(&File::open(path).ok()?).ok()?.id
}

// ---------------------------------------------------------------------------
// Scanning a transcript
// ---------------------------------------------------------------------------

/// FNV-1a over a uuid: what a record keeps instead of the uuid (a 64-bit
/// collision among a chat's few thousand entries is not a thing to plan for).
fn hash_uuid(uuid: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in uuid.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The uuids a window has loaded, as sorted hashes. In a record they are
/// one hex string, 16 digits each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known(Vec<u64>);

impl Known {
    pub fn contains(&self, uuid: &str) -> bool {
        self.0.binary_search(&hash_uuid(uuid)).is_ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn extend<'a>(&mut self, entries: impl IntoIterator<Item = &'a Entry>) {
        self.0.extend(entries.into_iter().map(|e| e.hash));
        self.0.sort_unstable();
        self.0.dedup();
    }
}

impl Serialize for Known {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut text = String::with_capacity(self.0.len() * 16);
        for hash in &self.0 {
            let _ = write!(text, "{hash:016x}");
        }
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Known {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() % 16 != 0 || !text.is_ascii() {
            return Err(serde::de::Error::custom(
                "known: not a list of 16-digit hashes",
            ));
        }
        let mut hashes = Vec::with_capacity(text.len() / 16);
        for chunk in text.as_bytes().chunks(16) {
            let digits = std::str::from_utf8(chunk).map_err(serde::de::Error::custom)?;
            hashes.push(u64::from_str_radix(digits, 16).map_err(serde::de::Error::custom)?);
        }
        hashes.sort_unstable();
        hashes.dedup();
        Ok(Known(hashes))
    }
}

/// One entry of a transcript that has a uuid.
#[derive(Debug, Clone)]
struct Entry {
    uuid: String,
    hash: u64,
    parent: Option<String>,
    /// A message a person would miss (see `branches::is_real_message`).
    real: bool,
}

/// What reading a transcript's lines found.
struct Scanned {
    entries: Vec<Entry>,
    /// Bytes read up to the end of the last complete line.
    consumed: usize,
}

impl Scanned {
    /// The newest real message, in file order.
    fn leaf(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.real)
            .map(|e| e.uuid.clone())
    }
}

/// The fields of a line the guard reads; the rest is skipped unparsed.
#[derive(Deserialize)]
struct Probe {
    #[serde(rename = "type")]
    kind: Option<Value>,
    uuid: Option<Value>,
    #[serde(rename = "parentUuid")]
    parent: Option<Value>,
}

/// The entries of `bytes`, which starts at a line boundary. A last line
/// without its newline counts only when it is whole JSON: Claude Code may be
/// writing it right now.
fn scan(bytes: &[u8]) -> Scanned {
    let mut entries = Vec::new();
    let mut consumed = 0;
    for raw in bytes.split_inclusive(|&b| b == b'\n') {
        let complete = raw.ends_with(b"\n");
        let line = raw.strip_suffix(b"\n").unwrap_or(raw);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            if complete {
                consumed += raw.len();
            }
            continue;
        }
        match serde_json::from_slice::<Probe>(line) {
            Ok(probe) => {
                consumed += raw.len();
                let text = |v: &Option<Value>| {
                    v.as_ref()
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                };
                let Some(uuid) = text(&probe.uuid) else {
                    continue;
                };
                let real = match probe.kind.as_ref().and_then(|k| k.as_str()) {
                    Some("assistant") => true,
                    Some("user") => serde_json::from_slice::<Value>(line)
                        .is_ok_and(|v| branches::is_real_message(&v)),
                    _ => false,
                };
                entries.push(Entry {
                    hash: hash_uuid(&uuid),
                    parent: text(&probe.parent),
                    uuid,
                    real,
                });
            }
            // A damaged complete line is skipped, a half-written one waits.
            Err(_) if complete => consumed += raw.len(),
            Err(_) => break,
        }
    }
    Scanned { entries, consumed }
}

/// The real messages of `entries` the window has not loaded, in file order.
fn unseen<'a>(entries: &'a [Entry], known: &Known) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|e| e.real && !known.contains(&e.uuid))
        .collect()
}

/// The newest message of the other PC's turns among `unseen` (file order).
///
/// Syncthing brings the other PC's turns in with the replaced file, and the
/// window appends its own after them, so the first unseen message starts the
/// turns that must stay: the tip is the newest unseen message that descends
/// from it. Without anything the window appended, that is simply the newest
/// unseen message.
fn remote_tip(entries: &[Entry], unseen: &[&Entry]) -> Option<String> {
    let first = unseen.first()?;
    let parents: HashMap<&str, Option<&str>> = entries
        .iter()
        .map(|e| (e.uuid.as_str(), e.parent.as_deref()))
        .collect();
    let descends = |entry: &Entry| {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = Some(entry.uuid.as_str());
        while let Some(uuid) = current {
            if uuid == first.uuid {
                return true;
            }
            if !seen.insert(uuid) {
                return false;
            }
            current = parents.get(uuid).copied().flatten();
        }
        false
    };
    unseen
        .iter()
        .rev()
        .find(|e| descends(e))
        .map(|e| e.uuid.clone())
}

// ---------------------------------------------------------------------------
// Window records
// ---------------------------------------------------------------------------

/// What one Claude Code window is known to have loaded of its chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    pub session_id: String,
    pub transcript: PathBuf,
    /// The transcript file as the window loaded it; `None` while the chat
    /// has no file yet (or the platform cannot tell).
    pub identity: Option<FileId>,
    pub len: u64,
    /// How much of the file `known` and `leaf` account for: the end of its
    /// last complete line.
    pub parsed: u64,
    pub mtime_ns: u64,
    /// The newest real message the window has: where its next message hangs.
    pub leaf: Option<String>,
    /// Every uuid the window has loaded or written.
    pub known: Known,
    /// Set when a sweep appended the other PC's turns to the file under a
    /// live window (same identity, so the file alone would not tell): the
    /// newest message appended. Only `start` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<String>,
    pub updated_at: String,
}

/// `<config dir>/guard/windows`, not created.
pub fn windows_dir() -> Result<PathBuf> {
    Ok(crate::config::ConfigManager::config_dir()?
        .join("guard")
        .join("windows"))
}

fn record_path(pid: u32) -> Result<PathBuf> {
    Ok(windows_dir()?.join(format!("{pid}.json")))
}

/// The record of window `pid`, if it has a readable one.
pub fn load(pid: u32) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(record_path(pid).ok()?).ok()?).ok()
}

fn save(record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    branches::write_file(&record_path(record.pid)?, &bytes)
}

fn delete(pid: u32) {
    if let Ok(path) = record_path(pid) {
        let _ = std::fs::remove_file(path);
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn read_all(file: &mut File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// What a window that loaded `transcript` right now has.
fn snapshot(pid: u32, session_id: &str, transcript: &Path) -> Result<Record> {
    let mut record = Record {
        pid,
        session_id: session_id.to_string(),
        transcript: transcript.to_path_buf(),
        identity: None,
        len: 0,
        parsed: 0,
        mtime_ns: 0,
        leaf: None,
        known: Known::default(),
        behind: None,
        updated_at: now(),
    };
    match File::open(transcript) {
        Ok(mut file) => {
            let st = stat(&file)?;
            let bytes = read_all(&mut file)?;
            let scanned = scan(&bytes);
            record.identity = st.id;
            record.len = bytes.len() as u64;
            record.parsed = scanned.consumed as u64;
            record.mtime_ns = st.mtime_ns;
            record.leaf = scanned.leaf();
            record.known.extend(&scanned.entries);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to open {}", transcript.display()))
        }
    }
    Ok(record)
}

/// Write or replace window `pid`'s record from the transcript as it is now:
/// the window has just loaded it (`SessionStart`).
pub fn record(pid: u32, session_id: &str, transcript: &Path) -> Result<()> {
    save(&snapshot(pid, session_id, transcript)?)
}

/// The record of `pid` when it is for this chat's transcript.
fn record_for(pid: u32, session_id: &str, transcript: &Path) -> Option<Record> {
    load(pid).filter(|r| r.session_id == session_id && r.transcript == transcript)
}

/// The prompt hook's answer for window `pid`: `Some(reason)` when the prompt
/// must be blocked because the window is behind its transcript.
///
/// - no record for this chat: the window is recorded as it is now, `None`;
/// - same file identity: the window's own appends (even a turn interrupted
///   with Esc, which writes messages and no `Stop`): the record follows,
///   `None`;
/// - another file under the name (Syncthing replaced it): the real messages
///   in it the window never loaded, if any, block the prompt and leave the
///   record alone; metadata only (a title, a `last-prompt`) refreshes it.
///
/// Nothing that goes wrong blocks a prompt.
pub fn verdict(pid: u32, session_id: &str, transcript: &Path) -> Option<String> {
    match verdict_inner(pid, session_id, transcript) {
        Ok(reason) => reason,
        Err(e) => {
            log::debug!("guard verdict {pid}: {e:#}");
            None
        }
    }
}

fn verdict_inner(pid: u32, session_id: &str, transcript: &Path) -> Result<Option<String>> {
    let Some(mut rec) = record_for(pid, session_id, transcript) else {
        record(pid, session_id, transcript)?;
        return Ok(None);
    };
    let mut file = match File::open(transcript) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to open {}", transcript.display()))
        }
    };
    let st = stat(&file)?;
    let (Some(then), Some(now_id)) = (rec.identity, st.id) else {
        // No file when the window started (the chat had no message yet), or
        // an identity the platform cannot give: nothing to compare against.
        record(pid, session_id, transcript)?;
        return Ok(None);
    };

    if then.same_file(&now_id) {
        if rec.behind.is_some() {
            return Ok(Some(crate::hooks::block_reason(session_id)));
        }
        if st.len == rec.len && st.mtime_ns == rec.mtime_ns {
            return Ok(None);
        }
        // Only appended, by the window itself: read what is new.
        let from = if st.len >= rec.parsed { rec.parsed } else { 0 };
        file.seek(SeekFrom::Start(from))?;
        let bytes = read_all(&mut file)?;
        let scanned = scan(&bytes);
        rec.known.extend(&scanned.entries);
        if let Some(leaf) = scanned.leaf() {
            rec.leaf = Some(leaf);
        }
        rec.parsed = from + scanned.consumed as u64;
        rec.len = from + bytes.len() as u64;
        rec.mtime_ns = st.mtime_ns;
        rec.updated_at = now();
        save(&rec)?;
        return Ok(None);
    }

    let bytes = read_all(&mut file)?;
    let scanned = scan(&bytes);
    if !unseen(&scanned.entries, &rec.known).is_empty() {
        return Ok(Some(crate::hooks::block_reason(session_id)));
    }
    rec.identity = st.id;
    rec.len = bytes.len() as u64;
    rec.parsed = scanned.consumed as u64;
    rec.mtime_ns = st.mtime_ns;
    rec.known.extend(&scanned.entries);
    if let Some(leaf) = scanned.leaf() {
        rec.leaf = Some(leaf);
    }
    rec.updated_at = now();
    save(&rec)?;
    Ok(None)
}

/// What closing a window did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The window had no record for this chat.
    NoRecord,
    /// The transcript was left as it was.
    Untouched,
    /// What the window appended off its old leaf was only noise: removed.
    Cleaned,
    /// What the window appended held real messages: they are now the
    /// session with this id.
    Branched(String),
    /// The window held real messages but another window holds the chat
    /// too: left for that one's own end.
    Deferred,
}

/// SessionEnd: the window `pid` is closing.
///
/// When the transcript was replaced under it (or a sweep appended to it) and
/// holds messages it never loaded, what it appended off its old leaf is
/// taken out: `/exit`'s entries (Rewritten), or real messages, which become
/// a `(rama paralela)` session (Split). While another window of this PC
/// holds the chat, only noise is removed. The window's record is deleted.
pub fn ended(pid: u32, session_id: &str, transcript: &Path) -> Result<Ended> {
    let Some(rec) = record_for(pid, session_id, transcript) else {
        return Ok(Ended::NoRecord);
    };
    let outcome = repair_for(&rec, pid);
    delete(pid);
    outcome
}

fn repair_for(rec: &Record, pid: u32) -> Result<Ended> {
    let mut file = match File::open(&rec.transcript) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Ended::Untouched),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to open {}", rec.transcript.display()))
        }
    };
    let st = stat(&file)?;
    let replaced = match (rec.identity, st.id) {
        (Some(then), Some(now_id)) => !then.same_file(&now_id),
        _ => false,
    };
    if !replaced && rec.behind.is_none() {
        return Ok(Ended::Untouched);
    }
    let bytes = read_all(&mut file)?;
    drop(file);
    let others_hold_it = crate::sync::discovery::claude_home_dir()
        .map(|home| {
            branches::live_pids_for(&home, &rec.session_id)
                .into_iter()
                .any(|p| p != pid)
        })
        // Unknown counts as held: dropping a live file's entries costs
        // messages, waiting costs one close.
        .unwrap_or(true);
    let _lock = GuardLock::take_within(std::time::Duration::from_secs(3));

    let Some(mut plan) = plan_repair(rec, &bytes) else {
        return Ok(Ended::Untouched);
    };
    // Re-read right before writing: the window (or Syncthing) may have
    // written since. Once more is enough; the next sweep covers the rest.
    let again = std::fs::read(&rec.transcript)
        .with_context(|| format!("Failed to read {}", rec.transcript.display()))?;
    if again != bytes {
        plan = match plan_repair(rec, &again) {
            Some(plan) => plan,
            None => return Ok(Ended::Untouched),
        };
    }
    match plan {
        Repair::Unchanged => Ok(Ended::Untouched),
        Repair::Rewritten(main) => {
            branches::write_file(&rec.transcript, &main)?;
            log::info!(
                "Removed what a window appended off its old leaf of {}",
                rec.session_id
            );
            Ok(Ended::Cleaned)
        }
        Repair::Split { .. } if others_hold_it => Ok(Ended::Deferred),
        Repair::Split {
            main,
            branch_id,
            branch,
        } => {
            let project = rec
                .transcript
                .parent()
                .context("A transcript has a directory")?;
            // The branch first: a failure in between leaves the messages in
            // both files, never in neither.
            let mut deferred = Vec::new();
            branches::write_branch(
                &project.join(format!("{branch_id}.jsonl")),
                &branch_id,
                &branch,
                &|_| false,
                &mut deferred,
            )?;
            branches::copy_sidecars(
                &[&project.join(&rec.session_id)],
                &project.join(&branch_id),
                &|_| false,
            )?;
            branches::write_file(&rec.transcript, &main)?;
            log::info!(
                "A window closed on an old version of {}: its own messages are now session {branch_id}",
                rec.session_id
            );
            Ok(Ended::Branched(branch_id))
        }
    }
}

/// The repair of `bytes`, for the window of `rec`; `None` when there is
/// nothing the window did not load, or no leaf to fork from.
fn plan_repair(rec: &Record, bytes: &[u8]) -> Option<Repair> {
    let scanned = scan(bytes);
    let fresh = unseen(&scanned.entries, &rec.known);
    let tip = remote_tip(&scanned.entries, &fresh)?;
    let leaf = rec.leaf.as_deref()?;
    Some(branches::repair_stale_branch(
        &rec.session_id,
        bytes,
        leaf,
        &tip,
    ))
}

/// The windows of this PC whose records `guard` keeps: sorted pids.
pub fn record_pids() -> Vec<u32> {
    let Ok(dir) = windows_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "json" {
                return None;
            }
            path.file_stem()?.to_str()?.parse::<u32>().ok()
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// Delete the records of windows that are gone; returns how many.
pub fn prune_dead_records() -> usize {
    let mut removed = 0;
    for pid in record_pids() {
        if !branches::process_is_alive(pid) {
            delete(pid);
            removed += 1;
        }
    }
    removed
}

/// A window with no record is recorded as it is now, so call this just
/// before a sweep appends under it: its record then says what it had loaded.
fn ensure_records(pids: &[u32], session_id: &str, transcript: &Path) {
    for &pid in pids {
        if record_for(pid, session_id, transcript).is_none() {
            if let Err(e) = record(pid, session_id, transcript) {
                log::debug!("guard record {pid}: {e:#}");
            }
        }
    }
}

/// A sweep appended the other PC's turns to `session_id`'s transcript while
/// the windows `pids` hold it: they are behind from now on.
fn mark_behind(pids: &[u32], session_id: &str, transcript: &Path, tip: &str) {
    for &pid in pids {
        if let Some(mut rec) = record_for(pid, session_id, transcript) {
            rec.behind = Some(tip.to_string());
            rec.updated_at = now();
            if let Err(e) = save(&rec) {
                log::debug!("guard mark {pid}: {e:#}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A lock for what rewrites transcripts
// ---------------------------------------------------------------------------

/// Held while a sweep or a repair writes: hooks of several windows run at
/// once, and two sweeps appending the same entries would double them.
struct GuardLock {
    _file: File,
}

impl GuardLock {
    fn take_within(wait: std::time::Duration) -> Option<GuardLock> {
        let path = crate::config::ConfigManager::ensure_config_dir()
            .ok()?
            .join("guard.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(GuardLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(_)) => return None,
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict copies
// ---------------------------------------------------------------------------

/// The marker Syncthing puts in the name of the copy it sets aside.
const CONFLICT_MARKER: &str = ".sync-conflict-";

/// The name a Syncthing conflict copy stands for:
/// `<stem>.sync-conflict-YYYYMMDD-HHMMSS-<7charDeviceId>.<ext>` is a copy of
/// `<stem>.<ext>` (older versions leave the device id out). `None` when the
/// name is not one.
pub fn conflict_original(name: &str) -> Option<String> {
    let at = name.find(CONFLICT_MARKER)?;
    let rest = &name.as_bytes()[at + CONFLICT_MARKER.len()..];
    let digits = |bytes: &[u8]| bytes.iter().all(u8::is_ascii_digit);
    if rest.len() < 15 || !digits(&rest[..8]) || rest[8] != b'-' || !digits(&rest[9..15]) {
        return None;
    }
    let mut end = 15;
    if rest.get(15) == Some(&b'-') {
        let id = rest[16..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if id != 7 {
            return None;
        }
        end = 16 + id;
    }
    let tail = &name[at + CONFLICT_MARKER.len() + end..];
    Some(format!("{}{}", &name[..at], tail))
}

/// What a sweep did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Conflict copies settled and deleted.
    pub resolved: Vec<PathBuf>,
    /// Conflict copies left for a later sweep (a live window holds the chat
    /// and the copy is not a plain continuation).
    pub deferred: Vec<PathBuf>,
}

impl SweepReport {
    fn absorb(&mut self, other: SweepReport) {
        self.resolved.extend(other.resolved);
        self.deferred.extend(other.deferred);
    }
}

/// The conflict copies in `dir` (all of its tree), with the name they stand
/// for, sorted by path.
fn list_conflicts(dir: &Path, recursive: bool) -> Vec<(PathBuf, String)> {
    let mut found: Vec<(PathBuf, String)> = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .into_iter()
        // Syncthing's own folders (.stfolder, .stversions) hold no chats.
        .filter_entry(|e| {
            !(e.file_type().is_dir()
                && e.depth() > 0
                && e.file_name().to_string_lossy().starts_with(".st"))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let original = conflict_original(e.file_name().to_str()?)?;
            Some((e.into_path(), original))
        })
        .collect();
    found.sort();
    found
}

/// The lines of a file, without their line breaks; blank ones left out.
fn lines_of(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .collect()
}

/// `main` with the lines of `other` it lacks (exact text) after it; `None`
/// when there are none.
fn union_append(main: &[u8], other: &[u8]) -> Option<Vec<u8>> {
    let mut seen: HashSet<&[u8]> = lines_of(main).into_iter().collect();
    let extra: Vec<&[u8]> = lines_of(other)
        .into_iter()
        .filter(|l| seen.insert(l))
        .collect();
    if extra.is_empty() {
        return None;
    }
    let mut merged = main.to_vec();
    if !merged.is_empty() && !merged.ends_with(b"\n") {
        merged.push(b'\n');
    }
    for line in extra {
        merged.extend_from_slice(line);
        merged.push(b'\n');
    }
    Some(merged)
}

/// The numeric `timestamp` of a `history.jsonl` line.
fn history_timestamp(line: &[u8]) -> Option<i128> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let ts = value.get("timestamp")?;
    ts.as_i64()
        .map(i128::from)
        .or_else(|| ts.as_f64().map(|f| f as i128))
}

/// The union of two `history.jsonl` files: every line once, stable-sorted by
/// `timestamp` (the first file's lines first among equal ones); lines
/// without a timestamp keep their relative order, after the rest.
fn union_history(main: &[u8], other: &[u8]) -> Vec<u8> {
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut stamped: Vec<(i128, &[u8])> = Vec::new();
    let mut unstamped: Vec<&[u8]> = Vec::new();
    for line in lines_of(main).into_iter().chain(lines_of(other)) {
        if !seen.insert(line) {
            continue;
        }
        match history_timestamp(line) {
            Some(ts) => stamped.push((ts, line)),
            None => unstamped.push(line),
        }
    }
    stamped.sort_by_key(|(ts, _)| *ts);
    let mut merged = Vec::new();
    for line in stamped.iter().map(|(_, l)| *l).chain(unstamped) {
        merged.extend_from_slice(line);
        merged.push(b'\n');
    }
    merged
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Settle the conflict copy of one top-level session transcript: `main` is
/// the local copy, the conflict the remote one. `Ok(true)` when the copy is
/// settled (and may be deleted), `Ok(false)` when it waits.
fn settle_session(session_id: &str, main: &Path, conflict: &Path, home: &Path) -> Result<bool> {
    let theirs = std::fs::read(conflict)
        .with_context(|| format!("Failed to read {}", conflict.display()))?;
    let Some(ours) = read_optional(main)? else {
        branches::write_file(main, &theirs)?;
        return Ok(true);
    };

    let live = branches::live_pids_for(home, session_id);
    if !live.is_empty() {
        // The window keeps its own file position: append, never rewrite.
        if let Some(appended) = branches::live_fast_forward(&ours, &theirs) {
            ensure_records(&live, session_id, main);
            branches::append_file(main, &appended.bytes)?;
            if let Some(tip) = &appended.remote_tip {
                mark_behind(&live, session_id, main, tip);
            }
            return Ok(true);
        }
        // A copy with nothing new needs no write; anything else waits.
        return Ok(matches!(
            branches::reconcile(session_id, &ours, &theirs),
            Reconciled::Same | Reconciled::KeepLocal
        ));
    }

    match branches::reconcile(session_id, &ours, &theirs) {
        Reconciled::Same | Reconciled::KeepLocal => {}
        Reconciled::TakeRemote => branches::write_file(main, &theirs)?,
        Reconciled::Combined(bytes) => branches::write_file(main, &bytes)?,
        Reconciled::Split {
            winner,
            branch_id,
            branch,
        } => {
            let project = main.parent().context("A transcript has a directory")?;
            let live_ids = branches::live_session_ids(home);
            let mut deferred = Vec::new();
            // The branch first, then the session: a failure in between
            // leaves the messages in both files, never in neither.
            branches::write_branch(
                &project.join(format!("{branch_id}.jsonl")),
                &branch_id,
                &branch,
                &|id| live_ids.contains(id),
                &mut deferred,
            )?;
            branches::copy_sidecars(
                &[&project.join(session_id)],
                &project.join(&branch_id),
                &|_| false,
            )?;
            if winner == Side::Remote {
                branches::write_file(main, &theirs)?;
            }
        }
    }
    Ok(true)
}

/// A sidecar `.jsonl` (subagent transcript and the like): the union.
fn settle_union(main: &Path, conflict: &Path) -> Result<bool> {
    let theirs = std::fs::read(conflict)
        .with_context(|| format!("Failed to read {}", conflict.display()))?;
    match read_optional(main)? {
        None => branches::write_file(main, &theirs)?,
        Some(ours) => {
            if let Some(merged) = union_append(&ours, &theirs) {
                branches::write_file(main, &merged)?;
            }
        }
    }
    Ok(true)
}

/// Settle the conflict copies under `dir` (a project folder, or the whole
/// projects folder); `projects` is the projects folder, which says what is a
/// session transcript. Deterministic in the files' contents, so both PCs
/// that settle the same pair end up with the same bytes.
fn sweep_under(projects: &Path, dir: &Path, home: &Path) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    for (conflict, original) in list_conflicts(dir, true) {
        let Some(parent) = conflict.parent() else {
            continue;
        };
        let main = parent.join(&original);
        if main.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let session = main
            .strip_prefix(projects)
            .ok()
            .and_then(branches::top_level_session_id);
        let settled = match session {
            Some(session_id) => settle_session(session_id, &main, &conflict, home),
            None => settle_union(&main, &conflict),
        };
        match settled {
            Ok(true) => {
                std::fs::remove_file(&conflict)
                    .with_context(|| format!("Failed to delete {}", conflict.display()))?;
                log::info!("Settled {}", conflict.display());
                report.resolved.push(conflict);
            }
            Ok(false) => report.deferred.push(conflict),
            Err(e) => log::warn!("Could not settle {}: {e:#}", conflict.display()),
        }
    }
    Ok(report)
}

/// Settle the Syncthing conflict copies under `dir` (a project folder under
/// `~/.claude/projects`, or the folder itself):
///
/// - a session transcript (`<project>/<session>.jsonl`) is reconciled like a
///   git-mode pull does, the main file as local and the copy as remote:
///   the copy that only adds takes over, one that adds messages on both
///   sides becomes a `(rama paralela)` session. A chat open in a window
///   here is only appended to (the window is then behind), and a copy that
///   is not a plain continuation waits;
/// - any other `.jsonl` (subagents, sidecars) becomes the union of both:
///   the main file's lines in order, then the copy's lines it lacks;
/// - copies of other files stay where they are.
///
/// Settled copies are deleted.
pub fn sweep_conflicts(dir: &Path) -> Result<SweepReport> {
    if list_conflicts(dir, true).is_empty() {
        return Ok(SweepReport::default());
    }
    let home = crate::sync::discovery::claude_home_dir()?;
    let _lock = GuardLock::take_within(std::time::Duration::from_secs(5));
    sweep_under(&home.join("projects"), dir, &home)
}

/// Settle `~/.claude/history.sync-conflict-*.jsonl`: the union of both
/// files' lines, each once, in `timestamp` order.
pub fn sweep_history() -> Result<SweepReport> {
    let home = crate::sync::discovery::claude_home_dir()?;
    let pending: Vec<(PathBuf, String)> = list_conflicts(&home, false)
        .into_iter()
        .filter(|(_, original)| original == "history.jsonl")
        .collect();
    let mut report = SweepReport::default();
    if pending.is_empty() {
        return Ok(report);
    }
    let _lock = GuardLock::take_within(std::time::Duration::from_secs(5));
    for (conflict, original) in pending {
        let main = home.join(original);
        let theirs = std::fs::read(&conflict)
            .with_context(|| format!("Failed to read {}", conflict.display()))?;
        let ours = read_optional(&main)?.unwrap_or_default();
        let merged = union_history(&ours, &theirs);
        if merged != ours {
            branches::write_file(&main, &merged)?;
        }
        std::fs::remove_file(&conflict)
            .with_context(|| format!("Failed to delete {}", conflict.display()))?;
        report.resolved.push(conflict);
    }
    Ok(report)
}

/// `guard sweep`: every project, and the history.
pub fn sweep_all() -> Result<SweepReport> {
    let home = crate::sync::discovery::claude_home_dir()?;
    let mut report = sweep_conflicts(&home.join("projects"))?;
    report.absorb(sweep_history()?);
    Ok(report)
}

/// The conflict copies waiting under `~/.claude/projects` and next to
/// `history.jsonl`.
pub fn pending_conflicts() -> Result<Vec<PathBuf>> {
    let home = crate::sync::discovery::claude_home_dir()?;
    let mut pending: Vec<PathBuf> = list_conflicts(&home.join("projects"), true)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    pending.extend(
        list_conflicts(&home, false)
            .into_iter()
            .filter(|(_, original)| original == "history.jsonl")
            .map(|(path, _)| path),
    );
    Ok(pending)
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// The hook events `guard hook` handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `UserPromptSubmit`: block a prompt typed in a window that is behind.
    Prompt,
    /// `SessionStart`: the window loaded its chat; record it.
    Start,
    /// `Stop`: a turn finished; follow the window's own appends.
    Stop,
    /// `SessionEnd`: repair what a window behind appended.
    End,
}

/// The windows a hook is running for: the ancestors of the hook that hold
/// `session_id`. Those with a record of this chat come first, then those
/// Claude Code lists as running it, and, with neither, the direct parent.
fn windows_for(session_id: &str, ancestors: &[u32]) -> Vec<u32> {
    let recorded: Vec<u32> = ancestors
        .iter()
        .copied()
        .filter(|&pid| load(pid).is_some_and(|r| r.session_id == session_id))
        .collect();
    if !recorded.is_empty() {
        return recorded;
    }
    if let Ok(home) = crate::sync::discovery::claude_home_dir() {
        let live = branches::live_pids_for(&home, session_id);
        let running: Vec<u32> = ancestors
            .iter()
            .copied()
            .filter(|pid| live.contains(pid))
            .collect();
        if !running.is_empty() {
            return running;
        }
    }
    ancestors.first().copied().into_iter().collect()
}

/// Run a hook. Never fails and never panics outward.
pub fn run_hook(event: Event) {
    let outcome = std::panic::catch_unwind(|| {
        if let Err(e) = handle(event) {
            log::debug!("guard hook {event:?}: {e:#}");
        }
    });
    drop(outcome);
}

fn handle(event: Event) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .take(1 << 20)
        .read_to_string(&mut input)
        .ok();
    let input: Value = serde_json::from_str(&input).context("The hook input is not JSON")?;
    let text = |key: &str| input.get(key).and_then(|v| v.as_str());
    let session_id = text("session_id").context("No session_id")?;
    let transcript = PathBuf::from(text("transcript_path").context("No transcript_path")?);
    let windows = windows_for(session_id, &crate::stale::claude_ancestors());

    match event {
        Event::Prompt => {
            if let Some((pid, reason)) = windows
                .iter()
                .find_map(|&pid| verdict(pid, session_id, &transcript).map(|r| (pid, r)))
            {
                // Started by the `claude` wrapper `guard install` puts in
                // the shell profiles: close this window and let the wrapper
                // reopen it on the chat. Otherwise, block with /resume.
                let cwd = text("cwd").map(str::to_string).unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                let reason = crate::reload::stale_response(
                    pid,
                    session_id,
                    text("prompt").unwrap_or(""),
                    &cwd,
                    reason,
                );
                println!("{}", crate::hooks::block_json(&reason));
            }
        }
        Event::Start => {
            // A compaction keeps the window's memory: it is no fresher than
            // before, so it follows the file like a turn does.
            let kept_in_memory = text("source") == Some("compact");
            for &pid in &windows {
                if kept_in_memory {
                    let _ = verdict(pid, session_id, &transcript);
                } else if let Err(e) = record(pid, session_id, &transcript) {
                    log::debug!("guard record {pid}: {e:#}");
                }
            }
            prune_dead_records();
            sweep_project_of(&transcript);
            if let Err(e) = sweep_history() {
                log::debug!("guard history sweep: {e:#}");
            }
        }
        Event::Stop => {
            // Not `record`: a window that is behind and finishes a turn
            // (one that started before the file was replaced) is no fresher.
            for &pid in &windows {
                let _ = verdict(pid, session_id, &transcript);
            }
            sweep_project_of(&transcript);
        }
        Event::End => {
            for &pid in &windows {
                if let Err(e) = ended(pid, session_id, &transcript) {
                    log::debug!("guard ended {pid}: {e:#}");
                }
            }
            sweep_project_of(&transcript);
        }
    }
    Ok(())
}

fn sweep_project_of(transcript: &Path) {
    if let Some(project) = transcript.parent() {
        if let Err(e) = sweep_conflicts(project) {
            log::debug!("guard sweep {}: {e:#}", project.display());
        }
    }
}

// ---------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------

/// A Claude Code hook `guard install` adds: the event, the last argument
/// (`guard hook <arg>`) and the seconds Claude Code gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSpec {
    pub event: &'static str,
    pub arg: &'static str,
    pub timeout_seconds: u64,
}

impl HookSpec {
    pub fn args(&self) -> [&'static str; 3] {
        ["guard", "hook", self.arg]
    }
}

/// The hooks `guard install` adds.
pub const HOOKS: [HookSpec; 4] = [
    HookSpec {
        event: "UserPromptSubmit",
        arg: "prompt",
        timeout_seconds: 5,
    },
    HookSpec {
        event: "SessionStart",
        arg: "start",
        timeout_seconds: 10,
    },
    HookSpec {
        event: "Stop",
        arg: "stop",
        timeout_seconds: 10,
    },
    HookSpec {
        event: "SessionEnd",
        arg: "end",
        timeout_seconds: 10,
    },
];

/// Claude Code deletes transcripts older than 30 days by default, and
/// Syncthing would carry the deletion to the other PC.
pub const RETENTION_DAYS: u64 = crate::autosync::RETENTION_DAYS;

/// A hook in Claude Code's exec form: the binary and its arguments, run
/// without a shell.
pub fn hook_entry(exe: &Path, spec: &HookSpec) -> Value {
    serde_json::json!({
        "type": "command",
        "command": exe.to_string_lossy(),
        "args": spec.args(),
        "timeout": spec.timeout_seconds,
    })
}

fn is_guard_hook(event: &str, hook: &Value, exe: Option<&Path>) -> bool {
    let ours = hook
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| crate::autosync::is_our_binary(c, exe));
    let args: Option<Vec<&str>> = hook
        .get("args")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().map(|x| x.as_str()).collect());
    ours && args.is_some_and(|args| {
        HOOKS
            .iter()
            .filter(|spec| spec.event == event)
            .any(|spec| args == spec.args())
    })
}

/// Every hook entry under `event` in a `hooks` object.
fn hook_entries<'a>(hooks: &'a serde_json::Map<String, Value>, event: &str) -> Vec<&'a Value> {
    hooks
        .get(event)
        .and_then(|groups| groups.as_array())
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks")?.as_array())
        .flatten()
        .collect()
}

/// The events 0.6.0's autosync mode has a hook under.
fn autosync_events(hooks: &serde_json::Map<String, Value>) -> Vec<&'static str> {
    crate::autosync::HOOKS
        .iter()
        .map(|spec| spec.event)
        .filter(|event| {
            hook_entries(hooks, event)
                .into_iter()
                .any(|hook| crate::autosync::is_our_hook(event, hook, None))
        })
        .collect()
}

/// Whether `settings_text` holds one of the guard's hooks.
pub fn has_guard_hooks(settings_text: Option<&str>) -> bool {
    let Some(Value::Object(root)) = settings_text
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
    else {
        return false;
    };
    let Some(Value::Object(hooks)) = root.get("hooks") else {
        return false;
    };
    HOOKS.iter().any(|spec| {
        hook_entries(hooks, spec.event)
            .into_iter()
            .any(|hook| is_guard_hook(spec.event, hook, None))
    })
}

/// Remove the guard's hooks from a `hooks` object; the events one was
/// removed from.
fn remove_guard_hooks(
    hooks: &mut serde_json::Map<String, Value>,
    exe: Option<&Path>,
) -> Vec<&'static str> {
    let mut events = Vec::new();
    for spec in &HOOKS {
        let event = spec.event;
        let Some(Value::Array(groups)) = hooks.get_mut(event) else {
            continue;
        };
        let mut removed = false;
        groups.retain_mut(|group| {
            let Some(Value::Array(entries)) = group.get_mut("hooks") else {
                return true;
            };
            let before = entries.len();
            entries.retain(|hook| !is_guard_hook(event, hook, exe));
            if entries.len() == before {
                return true;
            }
            removed = true;
            !entries.is_empty()
        });
        if removed && groups.is_empty() {
            hooks.remove(event);
        }
        if removed && !events.contains(&event) {
            events.push(event);
        }
    }
    events
}

/// `settings.json` after `guard install`: `cleanupPeriodDays` raised to
/// [`RETENTION_DAYS`] when lower or missing, and exactly one hook of ours per
/// event of [`HOOKS`], running the binary at `exe`. Every other member and
/// hook stays as it was. `None` when nothing changes; otherwise the new text
/// and what changed. Refuses while 0.6.0's autosync hooks are present: two
/// modes at once would fight over the same transcripts.
pub fn settings_after_install(
    original: Option<&str>,
    exe: &Path,
) -> Result<Option<(String, Vec<String>)>> {
    let mut settings = crate::autosync::Settings::parse(original)?;
    let mut changes = Vec::new();

    let hooks_before = settings.get("hooks");
    let mut hooks = match &hooks_before {
        Some(Value::Object(hooks)) => hooks.clone(),
        Some(_) => {
            bail!("settings.json has a \"hooks\" member that is not an object; fix it first")
        }
        None => serde_json::Map::new(),
    };
    let autosync = autosync_events(&hooks);
    if !autosync.is_empty() {
        bail!(
            "The autosync hooks ({}) are installed in settings.json. Run `claude-code-sync \
             autosync uninstall` first: git mode and Syncthing mode at once would fight over the \
             same transcripts.",
            autosync.join(", ")
        );
    }

    let current = settings
        .get("cleanupPeriodDays")
        .as_ref()
        .and_then(Value::as_u64);
    if current.is_none_or(|days| days < RETENTION_DAYS) {
        settings.set("cleanupPeriodDays", &Value::from(RETENTION_DAYS));
        changes.push(match current {
            Some(days) => format!(
                "cleanupPeriodDays {days} -> {RETENTION_DAYS} (Claude Code would delete older transcripts, and Syncthing would carry the deletion to the other PC)"
            ),
            None => format!(
                "cleanupPeriodDays set to {RETENTION_DAYS} (Claude Code's default deletes transcripts after 30 days, and Syncthing would carry the deletion to the other PC)"
            ),
        });
    }

    let removed = remove_guard_hooks(&mut hooks, Some(exe));
    for spec in &HOOKS {
        let ours = serde_json::json!({ "hooks": [hook_entry(exe, spec)] });
        let groups = hooks
            .entry(spec.event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            bail!(
                "settings.json has a \"hooks.{}\" that is not a list; fix it first",
                spec.event
            );
        };
        groups.push(ours);
    }
    if hooks_before.as_ref() != Some(&Value::Object(hooks.clone())) {
        settings.set("hooks", &Value::Object(hooks));
        for spec in &HOOKS {
            changes.push(format!(
                "{} hook {}: {} {}",
                spec.event,
                if removed.contains(&spec.event) {
                    "updated"
                } else {
                    "added"
                },
                exe.display(),
                spec.args().join(" ")
            ));
        }
    }

    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some((settings.render(), changes)))
}

/// `settings.json` after `guard uninstall`: the guard's hooks removed,
/// nothing else (the retention stays raised). `None` when there is none.
pub fn settings_after_uninstall(original: Option<&str>) -> Result<Option<(String, Vec<String>)>> {
    let Some(original) = original else {
        return Ok(None);
    };
    let mut settings = crate::autosync::Settings::parse(Some(original))?;
    let Some(Value::Object(mut hooks)) = settings.get("hooks") else {
        return Ok(None);
    };
    let removed = remove_guard_hooks(&mut hooks, None);
    if removed.is_empty() {
        return Ok(None);
    }
    if hooks.is_empty() {
        settings.remove("hooks");
    } else {
        settings.set("hooks", &Value::Object(hooks));
    }
    Ok(Some((
        settings.render(),
        removed
            .iter()
            .map(|event| format!("{event} hook removed"))
            .collect(),
    )))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn settings_path() -> Result<PathBuf> {
    Ok(crate::sync::discovery::claude_home_dir()?.join("settings.json"))
}

fn read_settings(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(Some(String::from_utf8(bytes).with_context(|| {
                format!("{} is not UTF-8", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn write_settings(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // UTF-8 without a BOM: `str` bytes are exactly that.
    branches::write_file(path, text.as_bytes())
}

/// `guard install`.
pub fn install(dry_run: bool) -> Result<()> {
    let path = settings_path()?;
    let exe = std::env::current_exe().context("Cannot tell where this binary is")?;
    let original = read_settings(&path)?;
    let plan = settings_after_install(original.as_deref(), &exe)?;
    let Some((text, changes)) = plan else {
        println!("{} the guard is already installed", "✓".green());
        return Ok(());
    };
    if dry_run {
        println!("{}", "guard install --dry-run would:".bold());
        println!("  {} {}", "edit".green(), path.display());
        for change in &changes {
            println!("    {change}");
        }
        return Ok(());
    }
    write_settings(&path, &text)?;
    for change in &changes {
        println!("  {} {change}", "✓".green());
    }
    println!(
        "{} the guard is on: windows already open pick the hooks up in a new session",
        "✓".green()
    );
    Ok(())
}

/// `guard uninstall`.
pub fn uninstall(dry_run: bool) -> Result<()> {
    let path = settings_path()?;
    let original = read_settings(&path)?;
    let Some((text, changes)) = settings_after_uninstall(original.as_deref())? else {
        println!("The guard's hooks are not installed");
        return Ok(());
    };
    if dry_run {
        println!("{}", "guard uninstall --dry-run would:".bold());
        println!("  {} {}", "edit".green(), path.display());
        for change in &changes {
            println!("    {change}");
        }
        return Ok(());
    }
    write_settings(&path, &text)?;
    for change in &changes {
        println!("  {} {change}", "✓".green());
    }
    println!(
        "{} guard removed (cleanupPeriodDays is left as it is)",
        "✓".green()
    );
    Ok(())
}

/// `guard status`.
pub fn status() -> Result<()> {
    let path = settings_path()?;
    println!("{}", "=== Guard (Syncthing mode) ===".bold().cyan());
    let settings: Option<Value> = read_settings(&path)?
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok());
    let hooks = settings
        .as_ref()
        .and_then(|s| s.get("hooks"))
        .and_then(|h| h.as_object());
    for spec in &HOOKS {
        let installed = hooks.is_some_and(|hooks| {
            hook_entries(hooks, spec.event)
                .into_iter()
                .any(|hook| is_guard_hook(spec.event, hook, None))
        });
        println!(
            "  {} hook (guard hook {}): {}",
            spec.event,
            spec.arg,
            if installed {
                "installed".green()
            } else {
                "not installed".yellow()
            }
        );
    }
    if let Some(events) = hooks.map(autosync_events).filter(|e| !e.is_empty()) {
        println!(
            "  {} autosync hooks are installed too ({}): run `autosync uninstall`",
            "!".yellow(),
            events.join(", ")
        );
    }
    let retention = settings
        .as_ref()
        .and_then(|s| s.get("cleanupPeriodDays"))
        .and_then(|d| d.as_u64());
    println!(
        "  Claude Code retention: {}",
        match retention {
            Some(days) if days >= RETENTION_DAYS => format!("{days} days").green(),
            Some(days) => format!("{days} days (older transcripts are deleted locally)").yellow(),
            None => "30 days (default; older transcripts are deleted locally)".yellow(),
        }
    );
    prune_dead_records();
    println!("  Windows recorded: {}", record_pids().len());
    let pending = pending_conflicts()?;
    println!(
        "  Conflict copies waiting: {}",
        if pending.is_empty() {
            "0".green()
        } else {
            pending.len().to_string().yellow()
        }
    );
    for path in &pending {
        println!("    {}", path.display());
    }
    Ok(())
}

/// `guard sweep`.
pub fn sweep() -> Result<()> {
    let report = sweep_all()?;
    for path in &report.resolved {
        println!("  {} {}", "settled".green(), path.display());
    }
    for path in &report.deferred {
        println!(
            "  {} {} (a window holds the chat; it settles when that closes)",
            "waiting".yellow(),
            path.display()
        );
    }
    if report.resolved.is_empty() && report.deferred.is_empty() {
        println!("No conflict copies to settle");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::process::{Child, Command, Stdio};

    const S: &str = "33333333-3333-4333-8333-333333333333";

    /// Config and Claude directories in a temp dir, and the environment
    /// pointing at them.
    struct Env {
        _tmp: tempfile::TempDir,
        claude: PathBuf,
        project: PathBuf,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Env {
        fn new() -> Env {
            let tmp = tempfile::TempDir::new().unwrap();
            let claude = tmp.path().join("claude");
            let project = claude.join("projects").join("-p");
            std::fs::create_dir_all(&project).unwrap();
            let names = ["CLAUDE_CODE_SYNC_CONFIG_DIR", "CLAUDE_CODE_SYNC_CLAUDE_DIR"];
            let saved = names
                .into_iter()
                .map(|n| (n, std::env::var(n).ok()))
                .collect();
            std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", tmp.path().join("cfg"));
            std::env::set_var("CLAUDE_CODE_SYNC_CLAUDE_DIR", &claude);
            Env {
                _tmp: tmp,
                claude,
                project,
                saved,
            }
        }

        fn transcript(&self) -> PathBuf {
            self.project.join(format!("{S}.jsonl"))
        }

        /// Say that `pid` is a window on `session`.
        fn open_window(&self, pid: u32, session: &str) {
            let dir = self.claude.join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{pid}.json")),
                format!("{{\"pid\":{pid},\"sessionId\":\"{session}\"}}"),
            )
            .unwrap();
        }

        fn jsonl_files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.project)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".jsonl"))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn long_child() -> Child {
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn kill(mut child: Child) {
        child.kill().ok();
        child.wait().ok();
    }

    fn entry_at(uuid: &str, parent: Option<&str>, kind: &str, text: &str, time: &str) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        let content = if kind == "user" {
            format!("\"{text}\"")
        } else {
            format!("[{{\"type\":\"text\",\"text\":\"{text}\"}}]")
        };
        format!(
            "{{\"parentUuid\":{parent},\"type\":\"{kind}\",\"message\":{{\"role\":\"{kind}\",\
             \"content\":{content}}},\"uuid\":\"{uuid}\",\"timestamp\":\"{time}\",\
             \"sessionId\":\"{S}\"}}\n"
        )
    }

    fn entry(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
        entry_at(uuid, parent, kind, text, "2026-01-01T10:00:00.000Z")
    }

    /// What `/exit` appends off the window's leaf.
    fn exit_noise(parent: &str) -> String {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"isMeta\":true,\"type\":\"user\",\"message\":\
             {{\"role\":\"user\",\"content\":\"<local-command-caveat>x</local-command-caveat>\"}},\
             \"uuid\":\"n1\",\"sessionId\":\"{S}\"}}\n"
        ) + &entry(
            "n2",
            Some("n1"),
            "user",
            "<command-name>/exit</command-name>",
        ) + &entry(
            "n3",
            Some("n2"),
            "user",
            "<local-command-stdout>Bye!</local-command-stdout>",
        )
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// What Syncthing does to a file it brings in.
    fn replace(path: &Path, text: &str) {
        branches::write_file(path, text.as_bytes()).unwrap();
    }

    fn p1_p2() -> String {
        entry("p1", None, "user", "hola") + &entry("p2", Some("p1"), "assistant", "buenas")
    }

    fn r1_r2() -> String {
        entry("r1", Some("p2"), "user", "sigue") + &entry("r2", Some("r1"), "assistant", "claro")
    }

    fn last_message_uuid(path: &Path) -> String {
        let text = std::fs::read_to_string(path).unwrap();
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| {
                v.get("type")
                    .is_some_and(|t| t == "user" || t == "assistant")
            })
            .filter_map(|v| v.get("uuid")?.as_str().map(str::to_string))
            .next_back()
            .unwrap()
    }

    // ---- identity -------------------------------------------------------

    #[test]
    fn an_append_keeps_the_identity_and_a_rename_replace_changes_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(&path, "a\n").unwrap();
        let before = file_identity(&path).expect("an identity on this platform");
        append(&path, "b\n");
        assert!(before.same_file(&file_identity(&path).unwrap()));
        replace(&path, "a\nb\nc\n");
        assert!(!before.same_file(&file_identity(&path).unwrap()));
        assert!(file_identity(&tmp.path().join("missing")).is_none());
    }

    // ---- verdict --------------------------------------------------------

    #[test]
    #[serial]
    fn the_windows_own_appends_never_block() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None, "recorded now");
        // A turn interrupted with Esc: real messages, no Stop in between.
        append(&env.transcript(), &r1_r2());
        assert_eq!(verdict(10, S, &env.transcript()), None);
        let record = load(10).unwrap();
        assert_eq!(record.leaf.as_deref(), Some("r2"), "the record follows");
        assert!(record.known.contains("r1"));
    }

    #[test]
    #[serial]
    fn a_replaced_file_with_new_messages_blocks_with_the_resume_command() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None);
        let before = load(10).unwrap();

        replace(&env.transcript(), &(p1_p2() + &r1_r2()));
        let reason = verdict(10, S, &env.transcript()).expect("blocked");
        assert!(reason.contains(&format!("/resume {S}")), "{reason}");
        assert_eq!(load(10).unwrap(), before, "the record is left alone");
        assert!(
            verdict(10, S, &env.transcript()).is_some(),
            "still blocked until the window reloads"
        );
    }

    #[test]
    #[serial]
    fn a_replaced_file_with_metadata_only_does_not_block() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None);
        let before = load(10).unwrap();

        let meta = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"x\",\"sessionId\":\"{S}\"}}\n\
             {{\"type\":\"last-prompt\",\"leafUuid\":\"p2\",\"sessionId\":\"{S}\"}}\n\
             {{\"type\":\"system\",\"uuid\":\"sys1\",\"parentUuid\":\"p2\"}}\n"
        );
        replace(&env.transcript(), &(p1_p2() + &meta));
        assert_eq!(verdict(10, S, &env.transcript()), None);
        let after = load(10).unwrap();
        assert!(!after.identity.unwrap().same_file(&before.identity.unwrap()));
        assert!(after.known.contains("sys1"), "the record was refreshed");
        assert_eq!(verdict(10, S, &env.transcript()), None);
    }

    #[test]
    #[serial]
    fn start_records_again_and_the_block_lifts() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        verdict(10, S, &env.transcript());
        replace(&env.transcript(), &(p1_p2() + &r1_r2()));
        assert!(verdict(10, S, &env.transcript()).is_some());

        record(10, S, &env.transcript()).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None);
        assert_eq!(load(10).unwrap().leaf.as_deref(), Some("r2"));
    }

    #[test]
    #[serial]
    fn a_record_of_another_chat_is_replaced_and_a_late_file_adopted() {
        let env = Env::new();
        let other = env.project.join("other.jsonl");
        std::fs::write(&other, p1_p2()).unwrap();
        record(10, "other", &other).unwrap();
        // The window moved to this chat (/resume): recorded now.
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None);
        assert_eq!(load(10).unwrap().session_id, S);

        // A chat with no file yet when the window started.
        std::fs::remove_file(env.transcript()).unwrap();
        record(11, S, &env.transcript()).unwrap();
        assert_eq!(load(11).unwrap().identity, None);
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(11, S, &env.transcript()), None);
        assert!(load(11).unwrap().identity.is_some());
    }

    #[test]
    #[serial]
    fn a_turn_that_ends_behind_does_not_hide_it_but_start_does() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        verdict(10, S, &env.transcript());
        replace(&env.transcript(), &(p1_p2() + &r1_r2()));
        // Stop runs the same follow-the-file logic as the prompt hook.
        let _ = verdict(10, S, &env.transcript());
        assert!(verdict(10, S, &env.transcript()).is_some());
    }

    #[test]
    #[serial]
    fn an_unchanged_file_is_not_parsed_again() {
        let env = Env::new();
        // 10 MB: 20000 entries of 500 bytes.
        let filler = "x".repeat(400);
        let mut text = String::new();
        let mut parent: Option<String> = None;
        for i in 0..20_000 {
            let uuid = format!("u{i}");
            text.push_str(&entry(&uuid, parent.as_deref(), "assistant", &filler));
            parent = Some(uuid);
        }
        std::fs::write(env.transcript(), &text).unwrap();
        assert_eq!(verdict(10, S, &env.transcript()), None);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(verdict(10, S, &env.transcript()), None);
        }
        let each = started.elapsed() / 20;
        assert!(
            each < std::time::Duration::from_millis(100),
            "an unchanged 10 MB transcript took {each:?} per verdict"
        );
        // The record stays small: 16 hex digits an entry, not the uuids.
        let size = std::fs::metadata(record_path(10).unwrap()).unwrap().len();
        assert!(size < 20_000 * 17 + 1024, "{size}");
    }

    #[test]
    fn known_round_trips_as_one_hex_string() {
        let mut known = Known::default();
        known.extend(&scan(entry("a", None, "user", "x").as_bytes()).entries);
        known.extend(&scan(entry("b", Some("a"), "assistant", "y").as_bytes()).entries);
        let json = serde_json::to_string(&known).unwrap();
        assert_eq!(json.len(), 2 + 32);
        let back: Known = serde_json::from_str(&json).unwrap();
        assert_eq!(back, known);
        assert!(back.contains("a") && back.contains("b") && !back.contains("c"));
        assert!(serde_json::from_str::<Known>("\"abc\"").is_err());
    }

    // ---- ended ----------------------------------------------------------

    /// Window 10 loaded p1-p2; the other PC continued to r2 (replacing the
    /// file); then the window's `extra` hangs off its own leaf p2.
    fn window_behind(env: &Env, extra: &str) {
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        record(10, S, &env.transcript()).unwrap();
        replace(&env.transcript(), &(p1_p2() + &r1_r2()));
        append(&env.transcript(), extra);
    }

    #[test]
    #[serial]
    fn exit_noise_after_a_replace_is_removed_and_no_session_is_made() {
        let env = Env::new();
        window_behind(
            &env,
            &(exit_noise("p2")
                + &format!(
                    "{{\"type\":\"last-prompt\",\"leafUuid\":\"p2\",\"sessionId\":\"{S}\"}}\n"
                )),
        );
        assert_eq!(last_message_uuid(&env.transcript()), "n3", "the bug");

        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::Cleaned);
        assert_eq!(last_message_uuid(&env.transcript()), "r2");
        assert_eq!(env.jsonl_files(), vec![format!("{S}.jsonl")]);
        assert!(load(10).is_none(), "the record is deleted");
    }

    #[test]
    #[serial]
    fn real_stale_messages_become_a_parallel_session() {
        let env = Env::new();
        window_behind(
            &env,
            &(entry("s1", Some("p2"), "user", "mi duda")
                + &entry("s2", Some("s1"), "assistant", "tu respuesta")),
        );

        let outcome = ended(10, S, &env.transcript()).unwrap();
        let Ended::Branched(branch_id) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            last_message_uuid(&env.transcript()),
            "r2",
            "the other PC's turns stay"
        );
        let main = std::fs::read_to_string(env.transcript()).unwrap();
        assert!(!main.contains("\"s1\""));
        assert_eq!(env.jsonl_files().len(), 2);
        let branch =
            std::fs::read_to_string(env.project.join(format!("{branch_id}.jsonl"))).unwrap();
        assert!(branch.contains("(rama paralela)"), "{branch}");
        assert!(branch.contains("mi duda") && branch.contains("tu respuesta"));
        assert!(!branch.contains("\"r1\""), "only the window's own branch");
        assert!(branch.contains(&branch_id));
    }

    #[test]
    #[serial]
    fn a_window_that_only_appended_leaves_the_transcript_alone() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        record(10, S, &env.transcript()).unwrap();
        append(&env.transcript(), &exit_noise("p2"));
        let before = std::fs::read(env.transcript()).unwrap();

        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::Untouched);
        assert_eq!(std::fs::read(env.transcript()).unwrap(), before);
        assert!(load(10).is_none());
        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::NoRecord);
    }

    #[test]
    #[serial]
    fn a_replace_with_metadata_only_leaves_the_transcript_alone() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        record(10, S, &env.transcript()).unwrap();
        replace(
            &env.transcript(),
            &(p1_p2()
                + &format!("{{\"type\":\"mode\",\"sessionId\":\"{S}\"}}\n")
                + &exit_noise("p2")),
        );
        let before = std::fs::read(env.transcript()).unwrap();
        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::Untouched);
        assert_eq!(std::fs::read(env.transcript()).unwrap(), before);
    }

    #[test]
    #[serial]
    fn another_live_window_only_loses_noise_never_a_split() {
        let env = Env::new();
        let other = long_child();
        env.open_window(other.id(), S);

        window_behind(&env, &exit_noise("p2"));
        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::Cleaned);
        assert_eq!(last_message_uuid(&env.transcript()), "r2");

        window_behind(
            &env,
            &(entry("s1", Some("p2"), "user", "mi duda")
                + &entry("s2", Some("s1"), "assistant", "ok")),
        );
        let before = std::fs::read(env.transcript()).unwrap();
        assert_eq!(ended(10, S, &env.transcript()).unwrap(), Ended::Deferred);
        assert_eq!(std::fs::read(env.transcript()).unwrap(), before);
        assert_eq!(env.jsonl_files().len(), 1, "no branch under a live window");
        kill(other);
    }

    #[test]
    #[serial]
    fn the_other_pcs_turns_win_even_when_the_window_appended_after_them() {
        let env = Env::new();
        window_behind(
            &env,
            &(entry("s1", Some("p2"), "user", "tarde")
                + &entry("s2", Some("s1"), "assistant", "tarde")
                + &exit_noise("s2")),
        );
        // The newest unseen message in file order is the window's own.
        let scanned = scan(&std::fs::read(env.transcript()).unwrap());
        let fresh = unseen(&scanned.entries, &load(10).unwrap().known);
        assert_eq!(fresh.last().unwrap().uuid, "s2");
        assert_eq!(remote_tip(&scanned.entries, &fresh).as_deref(), Some("r2"));
    }

    // ---- conflict names -------------------------------------------------

    #[test]
    fn conflict_names_say_what_they_copy() {
        let original = |n: &str| conflict_original(n);
        assert_eq!(
            original("abc.sync-conflict-20260101-101010-ABCDEFG.jsonl").as_deref(),
            Some("abc.jsonl")
        );
        assert_eq!(
            original("history.sync-conflict-20260101-101010-ABCDEFG.jsonl").as_deref(),
            Some("history.jsonl")
        );
        assert_eq!(
            original("a.b.sync-conflict-20260101-101010.md").as_deref(),
            Some("a.b.md"),
            "older Syncthing: no device id"
        );
        assert_eq!(
            original("noext.sync-conflict-20260101-101010-ABCDEFG").as_deref(),
            Some("noext")
        );
        assert_eq!(original("abc.jsonl"), None);
        assert_eq!(original("abc.sync-conflict-2026.jsonl"), None);
        assert_eq!(original("abc.sync-conflict-20260101-101010-AB.jsonl"), None);
    }

    // ---- sweep ----------------------------------------------------------

    fn conflict_of(env: &Env, session: &str) -> PathBuf {
        env.project.join(format!(
            "{session}.sync-conflict-20260102-120000-ABCDEFG.jsonl"
        ))
    }

    #[test]
    #[serial]
    fn a_conflict_copy_that_only_adds_takes_over() {
        let env = Env::new();
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        std::fs::write(conflict_of(&env, S), p1_p2() + &r1_r2()).unwrap();

        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            p1_p2() + &r1_r2()
        );
        assert!(!conflict_of(&env, S).exists());
        // Swept from the projects folder as well.
        std::fs::write(conflict_of(&env, S), p1_p2()).unwrap();
        assert_eq!(
            sweep_conflicts(&env.claude.join("projects"))
                .unwrap()
                .resolved
                .len(),
            1
        );
    }

    #[test]
    #[serial]
    fn a_diverged_conflict_copy_splits_into_a_parallel_session() {
        let env = Env::new();
        // Local continued later than the copy: the copy's branch is older.
        let local = p1_p2()
            + &entry_at(
                "a1",
                Some("p2"),
                "user",
                "local",
                "2026-01-03T10:00:00.000Z",
            );
        let remote = p1_p2()
            + &entry_at(
                "b1",
                Some("p2"),
                "user",
                "remoto",
                "2026-01-02T10:00:00.000Z",
            );
        std::fs::write(env.transcript(), &local).unwrap();
        std::fs::write(conflict_of(&env, S), &remote).unwrap();

        sweep_conflicts(&env.project).unwrap();
        assert!(!conflict_of(&env, S).exists());
        let files = env.jsonl_files();
        assert_eq!(files.len(), 2, "{files:?}");
        // The older branch (the copy's) keeps the session.
        assert_eq!(std::fs::read_to_string(env.transcript()).unwrap(), remote);
        let branch = files.iter().find(|f| **f != format!("{S}.jsonl")).unwrap();
        let branch = std::fs::read_to_string(env.project.join(branch)).unwrap();
        assert!(
            branch.contains("(rama paralela)") && branch.contains("local"),
            "{branch}"
        );
    }

    #[test]
    #[serial]
    fn a_diverged_copy_with_the_newer_branch_keeps_the_main_file() {
        let env = Env::new();
        let local = p1_p2()
            + &entry_at(
                "a1",
                Some("p2"),
                "user",
                "local",
                "2026-01-02T10:00:00.000Z",
            );
        let remote = p1_p2()
            + &entry_at(
                "b1",
                Some("p2"),
                "user",
                "remoto",
                "2026-01-03T10:00:00.000Z",
            );
        std::fs::write(env.transcript(), &local).unwrap();
        std::fs::write(conflict_of(&env, S), &remote).unwrap();
        sweep_conflicts(&env.project).unwrap();
        assert_eq!(std::fs::read_to_string(env.transcript()).unwrap(), local);
        assert_eq!(env.jsonl_files().len(), 2);
    }

    #[test]
    #[serial]
    fn a_live_chat_is_only_appended_to_and_its_window_is_behind() {
        let env = Env::new();
        let window = long_child();
        env.open_window(window.id(), S);
        std::fs::write(env.transcript(), p1_p2()).unwrap();
        assert_eq!(verdict(window.id(), S, &env.transcript()), None);
        let inode = file_identity(&env.transcript()).unwrap();
        std::fs::write(conflict_of(&env, S), p1_p2() + &r1_r2()).unwrap();

        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            p1_p2() + &r1_r2()
        );
        assert!(
            inode.same_file(&file_identity(&env.transcript()).unwrap()),
            "appended, not replaced"
        );
        let reason = verdict(window.id(), S, &env.transcript()).expect("the window is behind");
        assert!(reason.contains(&format!("/resume {S}")));
        // Closing it takes the noise off.
        append(&env.transcript(), &exit_noise("p2"));
        assert_eq!(
            ended(window.id(), S, &env.transcript()).unwrap(),
            Ended::Cleaned
        );
        assert_eq!(last_message_uuid(&env.transcript()), "r2");
        kill(window);
    }

    #[test]
    #[serial]
    fn a_live_chat_whose_copy_is_not_a_plain_continuation_waits() {
        let env = Env::new();
        let window = long_child();
        env.open_window(window.id(), S);
        let local = p1_p2() + &entry("a1", Some("p2"), "user", "local");
        let remote = p1_p2() + &entry("b1", Some("p2"), "user", "remoto");
        std::fs::write(env.transcript(), &local).unwrap();
        std::fs::write(conflict_of(&env, S), &remote).unwrap();

        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.deferred, vec![conflict_of(&env, S)]);
        assert!(conflict_of(&env, S).exists());
        assert_eq!(std::fs::read_to_string(env.transcript()).unwrap(), local);

        // Once the window is gone the next sweep settles it.
        kill(window);
        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(env.jsonl_files().len(), 2);
    }

    #[test]
    #[serial]
    fn a_live_chat_whose_copy_adds_nothing_loses_the_copy() {
        let env = Env::new();
        let window = long_child();
        env.open_window(window.id(), S);
        std::fs::write(env.transcript(), p1_p2() + &r1_r2()).unwrap();
        std::fs::write(conflict_of(&env, S), p1_p2()).unwrap();
        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            p1_p2() + &r1_r2()
        );
        kill(window);
    }

    #[test]
    #[serial]
    fn history_copies_are_united_in_timestamp_order() {
        let env = Env::new();
        let history = env.claude.join("history.jsonl");
        std::fs::write(
            &history,
            "{\"timestamp\":30,\"display\":\"c\"}\n{\"timestamp\":10,\"display\":\"a\"}\nsin fecha\n",
        )
        .unwrap();
        std::fs::write(
            env.claude
                .join("history.sync-conflict-20260102-120000-ABCDEFG.jsonl"),
            "{\"timestamp\":20,\"display\":\"b\"}\n{\"timestamp\":10,\"display\":\"a\"}\notra sin fecha\n",
        )
        .unwrap();

        let report = sweep_history().unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&history).unwrap(),
            "{\"timestamp\":10,\"display\":\"a\"}\n{\"timestamp\":20,\"display\":\"b\"}\n\
             {\"timestamp\":30,\"display\":\"c\"}\nsin fecha\notra sin fecha\n"
        );
        assert_eq!(
            std::fs::read_dir(&env.claude).unwrap().count(),
            2,
            "history.jsonl and projects"
        );
    }

    #[test]
    fn the_history_union_is_the_same_whichever_pc_computes_it() {
        let a = b"{\"timestamp\":2,\"d\":\"x\"}\n{\"timestamp\":1,\"d\":\"y\"}\n";
        let b = b"{\"timestamp\":2,\"d\":\"z\"}\n{\"timestamp\":3,\"d\":\"w\"}\n";
        // Both PCs hold the same pair: the winner as the file, the loser as
        // the copy, so the inputs are the same on both.
        assert_eq!(union_history(a, b), union_history(a, b));
        assert_eq!(
            String::from_utf8(union_history(a, b)).unwrap(),
            "{\"timestamp\":1,\"d\":\"y\"}\n{\"timestamp\":2,\"d\":\"x\"}\n\
             {\"timestamp\":2,\"d\":\"z\"}\n{\"timestamp\":3,\"d\":\"w\"}\n"
        );
    }

    #[test]
    #[serial]
    fn subagent_copies_are_united_line_by_line() {
        let env = Env::new();
        let dir = env.project.join(S).join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-1.jsonl"), "uno\ndos\n").unwrap();
        std::fs::write(
            dir.join("agent-1.sync-conflict-20260102-120000-ABCDEFG.jsonl"),
            "dos\ntres\nuno\ncuatro",
        )
        .unwrap();

        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report.resolved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("agent-1.jsonl")).unwrap(),
            "uno\ndos\ntres\ncuatro\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(env.jsonl_files().len(), 0, "no session was made");
    }

    #[test]
    #[serial]
    fn copies_of_other_files_are_left_alone() {
        let env = Env::new();
        let memory = env.project.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "a").unwrap();
        let copy = memory.join("MEMORY.sync-conflict-20260102-120000-ABCDEFG.md");
        std::fs::write(&copy, "b").unwrap();
        let stray = env
            .project
            .join("x.sync-conflict-20260102-120000-ABCDEFG.txt");
        std::fs::write(&stray, "c").unwrap();

        let report = sweep_conflicts(&env.project).unwrap();
        assert_eq!(report, SweepReport::default());
        assert_eq!(std::fs::read_to_string(copy).unwrap(), "b");
        assert!(stray.exists());
        assert_eq!(pending_conflicts().unwrap().len(), 2);
    }

    // ---- settings.json --------------------------------------------------

    fn exe() -> PathBuf {
        PathBuf::from("/opt/ccs/claude-code-sync")
    }

    #[test]
    fn install_adds_the_hooks_and_the_retention_and_keeps_the_rest() {
        let original = r#"{
  "model": "opus",
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "/usr/bin/other", "args": ["hook", "prompt"] } ] }
    ],
    "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "x" } ] } ]
  }
}
"#;
        let (text, changes) = settings_after_install(Some(original), &exe())
            .unwrap()
            .unwrap();
        let settings: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(settings["cleanupPeriodDays"], 3650);
        assert_eq!(settings["model"], "opus");
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "x"
        );
        assert_eq!(
            settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "/usr/bin/other",
            "a foreign hook is left alone"
        );
        let prompt = &settings["hooks"]["UserPromptSubmit"][1]["hooks"][0];
        assert_eq!(
            prompt,
            &serde_json::json!({
                "type": "command", "command": "/opt/ccs/claude-code-sync",
                "args": ["guard", "hook", "prompt"], "timeout": 5
            })
        );
        for (event, arg, timeout) in [
            ("SessionStart", "start", 10),
            ("Stop", "stop", 10),
            ("SessionEnd", "end", 10),
        ] {
            let hook = &settings["hooks"][event][0]["hooks"][0];
            assert_eq!(hook["args"], serde_json::json!(["guard", "hook", arg]));
            assert_eq!(hook["timeout"], timeout);
        }
        assert!(changes.iter().any(|c| c.contains("cleanupPeriodDays")));
        assert!(has_guard_hooks(Some(&text)));
        assert!(!has_guard_hooks(Some(original)));

        // Idempotent.
        assert!(settings_after_install(Some(&text), &exe())
            .unwrap()
            .is_none());
    }

    #[test]
    fn install_keeps_a_higher_retention() {
        let (text, changes) = settings_after_install(Some("{\"cleanupPeriodDays\": 9999}"), &exe())
            .unwrap()
            .unwrap();
        let settings: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(settings["cleanupPeriodDays"], 9999);
        assert!(!changes.iter().any(|c| c.contains("cleanupPeriodDays")));
        let (text, _) = settings_after_install(Some("{\"cleanupPeriodDays\": 30}"), &exe())
            .unwrap()
            .unwrap();
        assert!(text.contains("3650"));
    }

    #[test]
    fn install_refuses_while_autosync_hooks_are_installed() {
        let autosync = r#"{ "hooks": { "SessionEnd": [ { "hooks": [
            { "type": "command", "command": "/opt/ccs/claude-code-sync", "args": ["hook", "session-end"], "timeout": 10 } ] } ] } }"#;
        let error = settings_after_install(Some(autosync), &exe()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("autosync uninstall"), "{message}");

        let legacy = r#"{ "hooks": { "SessionEnd": [ { "hooks": [
            { "type": "command", "command": "/opt/ccs/claude-code-sync autosync run --detach" } ] } ] } }"#;
        assert!(settings_after_install(Some(legacy), &exe()).is_err());
        let exec_legacy = r#"{ "hooks": { "SessionEnd": [ { "hooks": [
            { "type": "command", "command": "/opt/ccs/claude-code-sync", "args": ["autosync", "run", "--detach"] } ] } ] } }"#;
        assert!(settings_after_install(Some(exec_legacy), &exe()).is_err());
    }

    #[test]
    fn uninstall_removes_only_the_guards_hooks() {
        let original = r#"{ "cleanupPeriodDays": 30, "hooks": { "Stop": [
            { "hooks": [ { "type": "command", "command": "/usr/bin/mine" } ] } ] } }"#;
        let (installed, _) = settings_after_install(Some(original), &exe())
            .unwrap()
            .unwrap();
        let (text, changes) = settings_after_uninstall(Some(&installed)).unwrap().unwrap();
        let settings: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(settings["cleanupPeriodDays"], 3650, "the retention stays");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/usr/bin/mine"
        );
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(settings["hooks"].get("SessionStart").is_none());
        assert_eq!(changes.len(), 4);
        assert!(settings_after_uninstall(Some(&text)).unwrap().is_none());
        assert!(settings_after_uninstall(None).unwrap().is_none());

        let (bare, _) = settings_after_install(None, &exe()).unwrap().unwrap();
        let (emptied, _) = settings_after_uninstall(Some(&bare)).unwrap().unwrap();
        let settings: Value = serde_json::from_str(&emptied).unwrap();
        assert!(settings.get("hooks").is_none(), "{emptied}");
    }
}
