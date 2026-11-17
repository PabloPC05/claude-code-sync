//! Parallel branches: what a pull does with a transcript two machines both
//! continued.
//!
//! Claude Code shows and resumes only the latest leaf of a transcript, so the
//! two continuations cannot share one file: the older one would vanish from
//! the conversation. Instead the transcript is reconciled by entry (an entry's
//! `uuid`, or its exact line when it has none):
//!
//! - every local entry is in the remote copy: take the remote copy;
//! - every remote entry is here already: keep the local copy;
//! - only one side added messages, the other only metadata (a title, a
//!   `last-prompt` record): the message side's file plus the other side's
//!   metadata lines;
//! - both added messages: the side whose first new entry is older keeps the
//!   session, and the other side's file becomes a new session — same
//!   content, `sessionId` rewritten, titled `<title> (rama paralela)`.
//!
//! Every outcome is a pure function of the two files: no host names, no
//! clocks. Two machines that reconcile the same pair write the same bytes,
//! so they converge and the next sync has nothing to do.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Namespace of the UUIDv5 a parallel branch is named with. Never change it:
/// both machines must derive the same id for the same branch.
pub const BRANCH_NAMESPACE: Uuid = Uuid::from_u128(0x5c1e_7a0b_4d2f_4e8a_9b36_c0d1_e2f3_a4b5);

/// Appended to the title of the session a branch becomes.
pub const BRANCH_TITLE_SUFFIX: &str = " (rama paralela)";

/// The longest first-message excerpt a branch title is made of.
const TITLE_EXCERPT_CHARS: usize = 60;

/// Which copy of a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// How a local transcript and the sync repository's copy of it settle.
#[derive(Debug, PartialEq, Eq)]
pub enum Reconciled {
    /// Byte for byte the same.
    Same,
    /// Every local entry is in the remote copy: take it (fast-forward).
    TakeRemote,
    /// Every remote entry is already here: keep the local copy.
    KeepLocal,
    /// One side added messages, the other only metadata: both in one file.
    Combined(Vec<u8>),
    /// Both sides added messages. The winner's file stays the session; the
    /// loser's becomes the session `branch_id`, holding `branch`.
    Split {
        winner: Side,
        branch_id: String,
        branch: Vec<u8>,
    },
}

/// What identifies an entry across copies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum EntryKey {
    Uuid(String),
    Line(Vec<u8>),
}

/// One non-blank line of a transcript, with what reconciling needs of it.
struct Entry<'a> {
    text: &'a [u8],
    key: EntryKey,
    uuid: Option<String>,
    kind: Option<String>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

/// The fields of an entry reconciling reads; the rest is skipped unparsed.
#[derive(serde::Deserialize)]
struct Probe {
    #[serde(rename = "type")]
    kind: Option<serde_json::Value>,
    uuid: Option<serde_json::Value>,
    timestamp: Option<serde_json::Value>,
}

fn entries(bytes: &[u8]) -> Vec<Entry<'_>> {
    bytes
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .map(|text| {
            let probe = serde_json::from_slice::<Probe>(text).ok();
            let field = |pick: fn(&Probe) -> &Option<serde_json::Value>| {
                probe
                    .as_ref()
                    .and_then(|p| pick(p).as_ref())
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            let uuid = field(|p| &p.uuid).filter(|u| !u.is_empty());
            let kind = field(|p| &p.kind);
            let timestamp = field(|p| &p.timestamp)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                .map(|t| t.with_timezone(&chrono::Utc));
            let key = match &uuid {
                Some(uuid) => EntryKey::Uuid(uuid.clone()),
                None => EntryKey::Line(text.to_vec()),
            };
            Entry {
                text,
                key,
                uuid,
                kind,
                timestamp,
            }
        })
        .collect()
}

/// Settle the local copy of the transcript of session `session_id` against
/// the sync repository's copy.
pub fn reconcile(session_id: &str, local: &[u8], remote: &[u8]) -> Reconciled {
    if local == remote {
        return Reconciled::Same;
    }
    let local_entries = entries(local);
    let remote_entries = entries(remote);
    let local_keys: HashSet<&EntryKey> = local_entries.iter().map(|e| &e.key).collect();
    let remote_keys: HashSet<&EntryKey> = remote_entries.iter().map(|e| &e.key).collect();
    let local_only: Vec<&Entry> = local_entries
        .iter()
        .filter(|e| !remote_keys.contains(&e.key))
        .collect();
    let remote_only: Vec<&Entry> = remote_entries
        .iter()
        .filter(|e| !local_keys.contains(&e.key))
        .collect();

    if local_only.is_empty() {
        return Reconciled::TakeRemote;
    }
    if remote_only.is_empty() {
        return Reconciled::KeepLocal;
    }

    let has_messages = |only: &[&Entry]| only.iter().any(|e| e.uuid.is_some());
    match (has_messages(&local_only), has_messages(&remote_only)) {
        (true, true) => split(session_id, local, &local_only, remote, &remote_only),
        (true, false) => combine(Side::Local, local, &remote_only),
        (false, true) => combine(Side::Remote, remote, &local_only),
        (false, false) => {
            // Metadata on both sides: either order is right, so take the one
            // both machines agree on.
            let joined = |only: &[&Entry]| -> Vec<u8> {
                only.iter().flat_map(|e| e.text.iter().copied()).collect()
            };
            if joined(&local_only) <= joined(&remote_only) {
                combine(Side::Local, local, &remote_only)
            } else {
                combine(Side::Remote, remote, &local_only)
            }
        }
    }
}

/// `base`'s file with the other side's new metadata lines after it. A
/// `last-prompt` record is left out: resuming continues from the leaf the
/// file's last one names, and that must stay the base side's own.
fn combine(base_side: Side, base: &[u8], extra: &[&Entry]) -> Reconciled {
    let extra: Vec<&&Entry> = extra
        .iter()
        .filter(|e| e.kind.as_deref() != Some("last-prompt"))
        .collect();
    if extra.is_empty() {
        return match base_side {
            Side::Local => Reconciled::KeepLocal,
            Side::Remote => Reconciled::TakeRemote,
        };
    }
    let mut combined = with_final_newline(base);
    for entry in extra {
        combined.extend_from_slice(entry.text);
        combined.push(b'\n');
    }
    Reconciled::Combined(combined)
}

/// The earliest moment among a side's new entries, and its first new message.
fn first_new(only: &[&Entry]) -> (Option<chrono::DateTime<chrono::Utc>>, Option<String>) {
    let earliest = only.iter().filter_map(|e| e.timestamp).min();
    let first_uuid = only.iter().find_map(|e| e.uuid.clone());
    (earliest, first_uuid)
}

fn split(
    session_id: &str,
    local: &[u8],
    local_only: &[&Entry],
    remote: &[u8],
    remote_only: &[&Entry],
) -> Reconciled {
    let (local_time, local_uuid) = first_new(local_only);
    let (remote_time, remote_uuid) = first_new(remote_only);
    let local_first = match (local_time, remote_time) {
        (Some(l), Some(r)) if l != r => l < r,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        // Same instant, or no time on either side: the smaller uuid.
        _ => local_uuid <= remote_uuid,
    };
    let (winner, loser_bytes, loser_uuid) = if local_first {
        (Side::Local, remote, remote_uuid)
    } else {
        (Side::Remote, local, local_uuid)
    };
    let branch_id = branch_session_id(session_id, loser_uuid.as_deref().unwrap_or_default());
    let branch = branch_transcript(session_id, &branch_id, loser_bytes);
    Reconciled::Split {
        winner,
        branch_id,
        branch,
    }
}

/// The id of the session a branch of `session_id` becomes: UUIDv5 over the
/// session and the first new message of the branch.
pub fn branch_session_id(session_id: &str, first_new_uuid: &str) -> String {
    let name = format!("{session_id}:{first_new_uuid}");
    Uuid::new_v5(&BRANCH_NAMESPACE, name.as_bytes()).to_string()
}

/// The branch's own transcript: the losing side's file, every entry of
/// `session_id` moved to `branch_id`, and a title naming it a branch.
pub fn branch_transcript(session_id: &str, branch_id: &str, loser: &[u8]) -> Vec<u8> {
    let mut branch = Vec::with_capacity(loser.len() + 128);
    for line in loser.split_inclusive(|&b| b == b'\n') {
        let rewritten = std::str::from_utf8(line)
            .ok()
            .and_then(|text| rewrite_session_id(text, session_id, branch_id));
        match rewritten {
            Some(text) => branch.extend_from_slice(text.as_bytes()),
            None => branch.extend_from_slice(line),
        }
    }
    let mut branch = with_final_newline(&branch);

    let title = format!("{}{BRANCH_TITLE_SUFFIX}", session_title(loser, session_id));
    let title_json = serde_json::to_string(&title).expect("a string always serializes");
    branch.extend_from_slice(
        format!(
            "{{\"type\":\"custom-title\",\"customTitle\":{title_json},\"sessionId\":\"{branch_id}\"}}\n"
        )
        .as_bytes(),
    );
    branch
}

fn with_final_newline(bytes: &[u8]) -> Vec<u8> {
    let mut owned = bytes.to_vec();
    if !owned.is_empty() && !owned.ends_with(b"\n") {
        owned.push(b'\n');
    }
    owned
}

/// The line with its top-level `"sessionId": "<from>"` value replaced by
/// `to`, everything else byte for byte as it was; `None` when the entry
/// belongs to another session or has none. A `sessionId` nested deeper (a
/// tool result quoting one) is not the entry's own and is left alone.
pub fn rewrite_session_id(line: &str, from: &str, to: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i = string_end(bytes, i)?;
                if depth != 1 || &bytes[start + 1..i - 1] != b"sessionId" {
                    continue;
                }
                let mut j = skip_whitespace(bytes, i);
                if bytes.get(j) != Some(&b':') {
                    continue;
                }
                j = skip_whitespace(bytes, j + 1);
                if bytes.get(j) != Some(&b'"') {
                    return None;
                }
                let value_end = string_end(bytes, j)?;
                if &bytes[j + 1..value_end - 1] != from.as_bytes() {
                    return None;
                }
                return Some(format!("{}{to}{}", &line[..=j], &line[value_end - 1..]));
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The index just past the JSON string starting at `start` (a `"`).
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// A session's title as the resume picker shows it: the last custom title,
/// else the last generated one, else the start of its first message.
pub fn session_title(transcript: &[u8], session_id: &str) -> String {
    let mut custom = None;
    let mut generated = None;
    let mut first_message = None;
    for line in transcript.split(|&b| b == b'\n') {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let text = |field: &str| {
            entry
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        match entry.get("type").and_then(|t| t.as_str()) {
            Some("custom-title") => custom = text("customTitle").or(custom),
            Some("ai-title") => generated = text("aiTitle").or(generated),
            Some("user") if first_message.is_none() => first_message = user_text(&entry),
            _ => {}
        }
    }
    custom
        .or(generated)
        .or_else(|| first_message.map(|text| text.chars().take(TITLE_EXCERPT_CHARS).collect()))
        .unwrap_or_else(|| session_id.to_string())
}

/// What the user typed in a user entry, whitespace collapsed; `None` for the
/// entries Claude Code adds itself (caveats, command wrappers, tool results).
fn user_text(entry: &serde_json::Value) -> Option<String> {
    if entry.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }
    let content = entry.get("message")?.get("content")?;
    let text = match content {
        serde_json::Value::String(text) => text.as_str(),
        serde_json::Value::Array(blocks) => blocks.iter().find_map(|block| {
            (block.get("type").and_then(|t| t.as_str()) == Some("text"))
                .then(|| block.get("text").and_then(|t| t.as_str()))
                .flatten()
        })?,
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let wrapper = text.starts_with("<command-") || text.starts_with("<local-command-");
    (!text.is_empty() && !wrapper).then_some(text)
}

/// The session of a top-level transcript, `<project>/<session>.jsonl`
/// relative to a projects directory. Subagent transcripts (`agent-*.jsonl`,
/// and everything inside a session's own directory) are not sessions one
/// resumes, and keep the smart merge.
pub fn top_level_session_id(relative: &Path) -> Option<&str> {
    let mut parts = relative.components();
    parts.next()?;
    let file = Path::new(parts.next()?.as_os_str());
    if parts.next().is_some() || file.extension()? != "jsonl" {
        return None;
    }
    let stem = file.file_stem()?.to_str()?;
    (!stem.starts_with("agent-")).then_some(stem)
}

/// The session a file under a projects directory belongs to: the stem of a
/// top-level transcript, or the session directory a sidecar sits in.
pub fn owning_session_id(relative: &Path) -> Option<&str> {
    let mut parts = relative.components();
    parts.next()?;
    let name = parts.next()?.as_os_str().to_str()?;
    Some(name.strip_suffix(".jsonl").unwrap_or(name))
}

/// Sessions running on this machine right now: `~/.claude/sessions/*.json`
/// records whose `pid` is alive. Their transcripts are being written, so a
/// pull leaves them for a later sync.
pub fn live_session_ids(claude_home: &Path) -> HashSet<String> {
    let Ok(records) = std::fs::read_dir(claude_home.join("sessions")) else {
        return HashSet::new();
    };
    records
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| std::fs::read(&p).ok())
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter_map(|record| {
            let pid = u32::try_from(record.get("pid")?.as_u64()?).ok()?;
            let session = record.get("sessionId")?.as_str()?.to_string();
            process_is_alive(pid).then_some(session)
        })
        .collect()
}

/// Whether a process with this id exists. Unknown counts as alive: wrongly
/// deferring a session costs one sync, wrongly rewriting it costs messages.
#[cfg(unix)]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks existence and permission without delivering anything.
    // SAFETY: kill(2) with signal 0 has no effect on the target process.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
pub fn process_is_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    match std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH", "/FO", "CSV"])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
        Err(_) => true,
    }
}

#[cfg(not(any(unix, windows)))]
pub fn process_is_alive(_pid: u32) -> bool {
    true
}

/// Write a branch transcript to `path`, settling it against a copy already
/// there: the same bytes change nothing, a copy holding fewer entries is
/// replaced, one holding more is kept, and one that diverged splits again.
/// Returns the paths written. A branch whose session is live here
/// (`is_live`) is left alone and named in `deferred`.
pub fn write_branch(
    path: &Path,
    branch_id: &str,
    content: &[u8],
    is_live: &dyn Fn(&str) -> bool,
    deferred: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let existing = match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };
    let Some(existing) = existing else {
        write_file(path, content)?;
        return Ok(vec![path.to_path_buf()]);
    };
    if existing == content {
        return Ok(Vec::new());
    }
    if is_live(branch_id) {
        deferred.push(branch_id.to_string());
        return Ok(Vec::new());
    }
    match reconcile(branch_id, &existing, content) {
        Reconciled::Same | Reconciled::KeepLocal => Ok(Vec::new()),
        Reconciled::TakeRemote => {
            write_file(path, content)?;
            Ok(vec![path.to_path_buf()])
        }
        Reconciled::Combined(bytes) => {
            write_file(path, &bytes)?;
            Ok(vec![path.to_path_buf()])
        }
        Reconciled::Split {
            winner,
            branch_id: next_id,
            branch,
        } => {
            let mut written = Vec::new();
            if winner == Side::Remote {
                write_file(path, content)?;
                written.push(path.to_path_buf());
            }
            let next_path = path.with_file_name(format!("{next_id}.jsonl"));
            written.extend(write_branch(
                &next_path, &next_id, &branch, is_live, deferred,
            )?);
            Ok(written)
        }
    }
}

/// Write a whole file through a temporary sibling and a rename, so Claude
/// Code never reads half of it.
pub fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("No parent directory for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(temporary.path(), content)?;
    temporary
        .persist(path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Copy a session's sidecar directories (`<session>/`: tool results,
/// subagents) into the branch's, `sources` in order of preference. A file
/// the destination has, or that `already_coming` says another path brings,
/// is not copied.
pub fn copy_sidecars(
    sources: &[&Path],
    destination: &Path,
    already_coming: &dyn Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut copied = Vec::new();
    for source in sources {
        if !source.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(source)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let Ok(relative) = entry.path().strip_prefix(source) else {
                continue;
            };
            let target = destination.join(relative);
            if target.exists() || already_coming(relative) {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            copied.push(target);
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "11111111-1111-4111-8111-111111111111";

    fn message(uuid: &str, parent: Option<&str>, minute: u32, text: &str) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            "{{\"parentUuid\":{parent},\"type\":\"user\",\"message\":{{\"role\":\"user\",\
             \"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\"timestamp\":\"2026-11-17T10:{minute:02}:00.000Z\",\
             \"sessionId\":\"{S}\",\"cwd\":\"/home/mateo/proyectos/jarvis\"}}\n"
        )
    }

    fn last_prompt(leaf: &str) -> String {
        format!("{{\"type\":\"last-prompt\",\"leafUuid\":\"{leaf}\",\"sessionId\":\"{S}\"}}\n")
    }

    fn file(lines: &[String]) -> Vec<u8> {
        lines.concat().into_bytes()
    }

    fn prefix() -> Vec<String> {
        vec![
            message("p1", None, 0, "hola, revisa el parser"),
            message("p2", Some("p1"), 1, "y los tests"),
            last_prompt("p2"),
        ]
    }

    fn with(mut lines: Vec<String>, more: &[String]) -> Vec<u8> {
        lines.extend_from_slice(more);
        file(&lines)
    }

    #[test]
    fn a_remote_copy_with_everything_local_and_more_is_taken() {
        let local = file(&prefix());
        let remote = with(
            prefix(),
            &[message("r1", Some("p2"), 5, "sigue"), last_prompt("r1")],
        );
        assert_eq!(reconcile(S, &local, &remote), Reconciled::TakeRemote);
        assert_eq!(reconcile(S, &remote, &local), Reconciled::KeepLocal);
        assert_eq!(reconcile(S, &local, &local), Reconciled::Same);
    }

    #[test]
    fn two_continuations_split_and_the_older_one_keeps_the_session() {
        let local = with(
            prefix(),
            &[
                message("l1", Some("p2"), 7, "rama local"),
                last_prompt("l1"),
            ],
        );
        let remote = with(
            prefix(),
            &[
                message("r1", Some("p2"), 5, "rama remota"),
                last_prompt("r1"),
            ],
        );

        let Reconciled::Split {
            winner,
            branch_id,
            branch,
        } = reconcile(S, &local, &remote)
        else {
            panic!("a true divergence splits");
        };

        assert_eq!(winner, Side::Remote, "the remote branch started first");
        assert_eq!(branch_id, branch_session_id(S, "l1"));
        let text = String::from_utf8(branch).unwrap();
        assert!(text.contains("rama local") && !text.contains("rama remota"));
        assert!(!text.contains(S), "every entry moved to the branch: {text}");
        assert!(text.ends_with(&format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"hola, revisa el parser (rama paralela)\",\
             \"sessionId\":\"{branch_id}\"}}\n"
        )));
    }

    #[test]
    fn both_machines_compute_the_same_split() {
        let a = with(
            prefix(),
            &[message("a1", Some("p2"), 5, "desde A"), last_prompt("a1")],
        );
        let b = with(
            prefix(),
            &[message("b1", Some("p2"), 9, "desde B"), last_prompt("b1")],
        );

        let on_a = reconcile(S, &a, &b);
        let on_b = reconcile(S, &b, &a);

        let (
            Reconciled::Split {
                winner: winner_a,
                branch_id: id_a,
                branch: branch_a,
            },
            Reconciled::Split {
                winner: winner_b,
                branch_id: id_b,
                branch: branch_b,
            },
        ) = (on_a, on_b)
        else {
            panic!("both split");
        };
        assert_eq!(winner_a, Side::Local, "A's branch is older");
        assert_eq!(winner_b, Side::Remote, "seen from B, the same branch wins");
        assert_eq!(id_a, id_b);
        assert_eq!(branch_a, branch_b, "byte for byte");
    }

    #[test]
    fn a_tie_in_time_goes_to_the_smaller_uuid() {
        let a = with(prefix(), &[message("bbb", Some("p2"), 5, "A")]);
        let b = with(prefix(), &[message("aaa", Some("p2"), 5, "B")]);
        let Reconciled::Split {
            winner, branch_id, ..
        } = reconcile(S, &a, &b)
        else {
            panic!("split");
        };
        assert_eq!(winner, Side::Remote);
        assert_eq!(branch_id, branch_session_id(S, "bbb"));
    }

    #[test]
    fn after_a_split_each_file_resumes_from_its_own_leaf() {
        let local = with(
            prefix(),
            &[message("l1", Some("p2"), 7, "local"), last_prompt("l1")],
        );
        let remote = with(
            prefix(),
            &[message("r1", Some("p2"), 5, "remota"), last_prompt("r1")],
        );
        let Reconciled::Split { winner, branch, .. } = reconcile(S, &local, &remote) else {
            panic!("split");
        };
        let session = match winner {
            Side::Local => local.clone(),
            Side::Remote => remote.clone(),
        };

        for file in [&session, &branch] {
            let lines: Vec<serde_json::Value> = file
                .split(|&b| b == b'\n')
                .filter_map(|l| serde_json::from_slice(l).ok())
                .collect();
            let leaf = lines
                .iter()
                .rev()
                .find(|e| e["type"] == "last-prompt")
                .and_then(|e| e["leafUuid"].as_str())
                .expect("a last-prompt record");
            assert!(
                lines.iter().any(|e| e["uuid"] == leaf),
                "the last last-prompt names {leaf}, which this file holds"
            );
        }
        let branch_text = String::from_utf8(branch).unwrap();
        assert!(branch_text.contains("\"type\":\"last-prompt\",\"leafUuid\":\"l1\""));
    }

    #[test]
    fn a_title_change_on_one_side_does_not_fork_the_conversation() {
        let title = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"Parser\",\"sessionId\":\"{S}\"}}\n"
        );
        let renamed = with(prefix(), std::slice::from_ref(&title));
        let continued = with(
            prefix(),
            &[message("c1", Some("p2"), 5, "sigue"), last_prompt("c1")],
        );

        let expected = with(
            prefix(),
            &[
                message("c1", Some("p2"), 5, "sigue"),
                last_prompt("c1"),
                title,
            ],
        );
        assert_eq!(
            reconcile(S, &renamed, &continued),
            Reconciled::Combined(expected.clone())
        );
        assert_eq!(
            reconcile(S, &continued, &renamed),
            Reconciled::Combined(expected)
        );
    }

    #[test]
    fn a_stray_last_prompt_on_one_side_never_moves_the_other_sides_leaf() {
        let reopened = with(prefix(), &[last_prompt("p1")]);
        let continued = with(
            prefix(),
            &[message("c1", Some("p2"), 5, "sigue"), last_prompt("c1")],
        );
        assert_eq!(reconcile(S, &reopened, &continued), Reconciled::TakeRemote);
        assert_eq!(reconcile(S, &continued, &reopened), Reconciled::KeepLocal);
    }

    #[test]
    fn only_the_entrys_own_session_id_is_rewritten() {
        let line = format!(
            "{{\"type\":\"user\",\"toolUseResult\":{{\"sessionId\":\"{S}\"}},\"sessionId\":\"{S}\",\"x\":\"\\\"sessionId\\\":\\\"{S}\\\"\"}}"
        );
        let rewritten = rewrite_session_id(&line, S, "NEW").unwrap();
        assert_eq!(
            rewritten,
            line.replacen(
                &format!("}},\"sessionId\":\"{S}\""),
                "},\"sessionId\":\"NEW\"",
                1
            )
        );
        assert_eq!(rewrite_session_id(&line, "other", "NEW"), None);
        assert_eq!(rewrite_session_id("{\"type\":\"summary\"}", S, "NEW"), None);
        assert_eq!(rewrite_session_id("not json", S, "NEW"), None);
    }

    #[test]
    fn the_branch_title_prefers_the_custom_then_the_generated_title() {
        let ai = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"Arreglar parser\",\"sessionId\":\"{S}\"}}\n"
        );
        let custom = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"Mi parser\",\"sessionId\":\"{S}\"}}\n"
        );
        assert_eq!(
            session_title(&with(prefix(), std::slice::from_ref(&ai)), S),
            "Arreglar parser"
        );
        assert_eq!(
            session_title(&with(prefix(), &[custom, ai]), S),
            "Mi parser"
        );
        let long = message("x", None, 0, &"palabra ".repeat(20));
        assert_eq!(session_title(long.as_bytes(), S).chars().count(), 60);
        assert_eq!(session_title(b"", S), S);
    }

    #[test]
    fn top_level_transcripts_are_told_from_subagents() {
        assert_eq!(top_level_session_id(Path::new("p/abc.jsonl")), Some("abc"));
        assert_eq!(top_level_session_id(Path::new("p/agent-1.jsonl")), None);
        assert_eq!(
            top_level_session_id(Path::new("p/abc/subagents/a.jsonl")),
            None
        );
        assert_eq!(top_level_session_id(Path::new("abc.jsonl")), None);
        assert_eq!(
            owning_session_id(Path::new("p/abc/tool-results/x.txt")),
            Some("abc")
        );
        assert_eq!(owning_session_id(Path::new("p/abc.jsonl")), Some("abc"));
    }

    #[test]
    fn this_process_is_live_and_a_stale_record_is_not() {
        let home = tempfile::TempDir::new().unwrap();
        let sessions = home.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("1.json"),
            format!(
                "{{\"pid\":{},\"sessionId\":\"live\",\"status\":\"busy\"}}",
                std::process::id()
            ),
        )
        .unwrap();
        std::fs::write(
            sessions.join("2.json"),
            "{\"pid\":2147480000,\"sessionId\":\"gone\"}",
        )
        .unwrap();

        let live = live_session_ids(home.path());
        assert!(live.contains("live"));
        assert!(!live.contains("gone"));
    }

    #[test]
    fn writing_a_branch_twice_changes_nothing_and_a_longer_copy_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.jsonl");
        let mut deferred = Vec::new();
        let short = file(&prefix());
        let long = with(prefix(), &[message("n1", Some("p2"), 9, "más")]);

        assert_eq!(
            write_branch(&path, "b", &short, &|_| false, &mut deferred)
                .unwrap()
                .len(),
            1
        );
        assert!(write_branch(&path, "b", &short, &|_| false, &mut deferred)
            .unwrap()
            .is_empty());
        assert_eq!(
            write_branch(&path, "b", &long, &|_| false, &mut deferred)
                .unwrap()
                .len(),
            1
        );
        assert!(write_branch(&path, "b", &short, &|_| false, &mut deferred)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), long);
        assert!(deferred.is_empty());
    }

    /// Two machines and a sync repository, reduced to files: a sync pulls
    /// every repository transcript into the machine (reconciling, splitting,
    /// writing branches) and then pushes every machine transcript back.
    #[test]
    fn three_rounds_of_sync_on_two_machines_converge() {
        use std::collections::BTreeMap;
        type Files = BTreeMap<String, Vec<u8>>;

        fn sync(machine: &mut Files, repo: &mut Files) -> bool {
            let mut changed = false;
            for (name, remote) in repo.clone() {
                let Some(local) = machine.get(&name).cloned() else {
                    machine.insert(name, remote);
                    changed = true;
                    continue;
                };
                let session = name.trim_end_matches(".jsonl");
                match reconcile(session, &local, &remote) {
                    Reconciled::Same | Reconciled::KeepLocal => {}
                    Reconciled::TakeRemote => {
                        machine.insert(name, remote);
                        changed = true;
                    }
                    Reconciled::Combined(bytes) => {
                        machine.insert(name, bytes);
                        changed = true;
                    }
                    Reconciled::Split {
                        winner,
                        branch_id,
                        branch,
                    } => {
                        if winner == Side::Remote {
                            machine.insert(name, remote);
                        }
                        let branch_name = format!("{branch_id}.jsonl");
                        match machine.get(&branch_name) {
                            Some(existing) if *existing == branch => {}
                            Some(existing) => {
                                assert_eq!(
                                    reconcile(&branch_id, existing, &branch),
                                    Reconciled::TakeRemote,
                                    "a branch only ever grows"
                                );
                                machine.insert(branch_name, branch);
                            }
                            None => {
                                machine.insert(branch_name, branch);
                            }
                        }
                        changed = true;
                    }
                }
            }
            for (name, bytes) in machine.iter() {
                if repo.get(name) != Some(bytes) {
                    repo.insert(name.clone(), bytes.clone());
                    changed = true;
                }
            }
            changed
        }

        let name = format!("{S}.jsonl");
        let mut repo = Files::new();
        let mut a = Files::from([(name.clone(), file(&prefix()))]);
        let mut b = Files::new();
        sync(&mut a, &mut repo);
        sync(&mut b, &mut repo);
        assert_eq!(a, b);

        // Both continue the same chat; B a minute earlier.
        a.insert(
            name.clone(),
            with(
                prefix(),
                &[message("a1", Some("p2"), 8, "A sigue"), last_prompt("a1")],
            ),
        );
        b.insert(
            name.clone(),
            with(
                prefix(),
                &[message("b1", Some("p2"), 7, "B sigue"), last_prompt("b1")],
            ),
        );

        for _round in 0..3 {
            sync(&mut a, &mut repo);
            sync(&mut b, &mut repo);
        }

        assert_eq!(a, b, "both machines hold the same files");
        assert_eq!(a.len(), 2, "the session and one branch");
        assert!(String::from_utf8_lossy(&a[&name]).contains("B sigue"));
        assert!(!sync(&mut a, &mut repo), "nothing left to do on A");
        assert!(!sync(&mut b, &mut repo), "nothing left to do on B");
    }
}
