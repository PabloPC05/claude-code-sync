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

/// Whether pushing the local transcript over the sync repository's copy
/// would drop entries only the repository has: true when the repository
/// copy holds entries the local file lacks, unless they already sit in this
/// machine's parallel branch of the session (a pull that split it, keeping
/// the local continuation as the session, moved them there).
///
/// Such a transcript belongs to a session a pull deferred (live here) or
/// one this machine has not pulled yet; the push leaves the repository copy
/// alone, and the next pull fast-forwards or splits it.
pub fn repository_is_ahead(local_path: &Path, local: &[u8], repository: &[u8]) -> bool {
    let session_id = local_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    match reconcile(session_id, local, repository) {
        Reconciled::Same | Reconciled::KeepLocal => false,
        Reconciled::Split {
            winner: Side::Local,
            branch_id,
            branch,
        } => {
            let here = local_path.with_file_name(format!("{branch_id}.jsonl"));
            !std::fs::read(here).is_ok_and(|existing| {
                matches!(
                    reconcile(&branch_id, &existing, &branch),
                    Reconciled::Same | Reconciled::KeepLocal
                )
            })
        }
        Reconciled::TakeRemote | Reconciled::Combined(_) | Reconciled::Split { .. } => true,
    }
}

/// What a live session's transcript receives when a pull fast-forwards it.
#[derive(Debug, PartialEq, Eq)]
pub struct Appended {
    /// The bytes to append: a newline when the local file lacks its final
    /// one, then every remote entry the local file lacks, in remote order.
    pub bytes: Vec<u8>,
    /// The newest message entry the local file had before the append: the
    /// leaf the window that holds this session in memory is on. `None`
    /// when nothing in `bytes` is a message, or the local file had none.
    pub fork_leaf: Option<String>,
    /// The newest message entry appended.
    pub remote_tip: Option<String>,
}

impl Appended {
    /// The `(fork_leaf, remote_tip)` a window's stale marker records; `None`
    /// when no message was appended, so no window is out of date.
    pub fn stale_span(&self) -> Option<(&str, &str)> {
        Some((self.fork_leaf.as_deref()?, self.remote_tip.as_deref()?))
    }
}

/// Whether an entry is a user or assistant message.
fn is_message_entry(entry: &Entry) -> bool {
    entry.uuid.is_some() && matches!(entry.kind.as_deref(), Some("user" | "assistant"))
}

/// What a pull may append to the transcript of a session running here, when
/// the remote copy only added to the local one: every local entry is in the
/// remote copy, or the local side added metadata only and the remote added
/// messages. `None` for anything else (identical, the local side ahead, both
/// sides added messages, only metadata on both): those stay deferred.
///
/// Appending, never rewriting: the live process keeps its own file position
/// and a rename would orphan it. The entries are the remote's lacking ones
/// by `uuid` (or exact line when they have none), in the remote file's order.
pub fn live_fast_forward(local: &[u8], remote: &[u8]) -> Option<Appended> {
    if local == remote {
        return None;
    }
    let local_entries = entries(local);
    let remote_entries = entries(remote);
    let local_keys: HashSet<&EntryKey> = local_entries.iter().map(|e| &e.key).collect();
    let remote_keys: HashSet<&EntryKey> = remote_entries.iter().map(|e| &e.key).collect();
    let local_only: Vec<&Entry> = local_entries
        .iter()
        .filter(|e| !remote_keys.contains(&e.key))
        .collect();
    let mut seen: HashSet<&EntryKey> = HashSet::new();
    let remote_only: Vec<&Entry> = remote_entries
        .iter()
        .filter(|e| !local_keys.contains(&e.key) && seen.insert(&e.key))
        .collect();
    if remote_only.is_empty() {
        return None;
    }
    // The local side may have added metadata, never a message.
    if local_only.iter().any(|e| e.uuid.is_some()) {
        return None;
    }
    // Metadata on both sides is not a fast-forward either way.
    if !local_only.is_empty() && !remote_only.iter().any(|e| e.uuid.is_some()) {
        return None;
    }

    let mut bytes = Vec::new();
    if !local.is_empty() && !local.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    for entry in &remote_only {
        bytes.extend_from_slice(entry.text);
        bytes.push(b'\n');
    }
    let fork_leaf = local_entries
        .iter()
        .rev()
        .find(|e| is_message_entry(e))
        .and_then(|e| e.uuid.clone());
    let remote_tip = remote_only
        .iter()
        .rev()
        .find(|e| is_message_entry(e))
        .and_then(|e| e.uuid.clone());
    Some(Appended {
        bytes,
        fork_leaf,
        remote_tip,
    })
}

/// What closing a stale window did to a transcript, and what it takes to
/// undo.
#[derive(Debug, PartialEq, Eq)]
pub enum Repair {
    /// Nothing to remove.
    Unchanged,
    /// Only noise (what `/exit` appends) hung off the old leaf: the file
    /// without it.
    Rewritten(Vec<u8>),
    /// Real messages hung off the old leaf: they become the session
    /// `branch_id`, and `main` is the session without them.
    Split {
        main: Vec<u8>,
        branch_id: String,
        branch: Vec<u8>,
    },
}

/// Text a user entry starts with when Claude Code wrote it, not the person.
const NOISE_PREFIXES: [&str; 4] = [
    "<command-name>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
];

/// Whether an entry with a uuid is a message a person would miss: any
/// assistant entry, and any user entry that is not a command wrapper (a
/// tool result counts as real). Everything else is noise.
fn is_real_message(entry: &serde_json::Value) -> bool {
    match entry.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => true,
        Some("user") => {
            if entry.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
                return false;
            }
            let content = entry.get("message").and_then(|m| m.get("content"));
            let text = match content {
                Some(serde_json::Value::String(text)) => Some(text.as_str()),
                Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|block| {
                    (block.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .then(|| block.get("text").and_then(|t| t.as_str()))
                        .flatten()
                }),
                _ => None,
            };
            !text.is_some_and(|text| {
                let text = text.trim_start();
                NOISE_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
            })
        }
        _ => false,
    }
}

/// A transcript line with what repairing reads of it.
struct RepairLine<'a> {
    text: &'a [u8],
    uuid: Option<String>,
    value: Option<serde_json::Value>,
}

/// Take a window's leftovers out of a transcript.
///
/// A window holding `session_id` was out of date since a pull appended the
/// other machine's turns after `fork_leaf` (its own leaf) up to `remote_tip`.
/// What the window wrote after that hangs off `fork_leaf` on a branch of its
/// own; Claude Code resumes the branch with the newest message, so that
/// branch must stop being the newest one:
///
/// - `kept`: the child of `fork_leaf` on the way to `remote_tip`, and all its
///   descendants;
/// - `stale`: every other descendant of `fork_leaf`.
///
/// If `stale` holds only noise (the entries `/exit` appends, and anything
/// that is not a user or assistant message) the file loses it and the
/// `last-prompt` records naming `fork_leaf` or a stale entry. If it holds a
/// real message, that branch is moved to a session of its own
/// (`<title> (rama paralela)`). Branches that do not fork at `fork_leaf`
/// (Claude Code's own rewinds and edits) are never touched.
pub fn repair_stale_branch(
    session_id: &str,
    bytes: &[u8],
    fork_leaf: &str,
    remote_tip: &str,
) -> Repair {
    let lines: Vec<RepairLine> = bytes
        .split_inclusive(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .map(|text| {
            let value = serde_json::from_slice::<serde_json::Value>(text).ok();
            let uuid = value
                .as_ref()
                .and_then(|v| v.get("uuid"))
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
                .map(str::to_string);
            RepairLine { text, uuid, value }
        })
        .collect();

    let mut parents: std::collections::HashMap<&str, Option<&str>> =
        std::collections::HashMap::new();
    let mut children: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for line in &lines {
        let (Some(uuid), Some(value)) = (&line.uuid, &line.value) else {
            continue;
        };
        if parents.contains_key(uuid.as_str()) {
            continue;
        }
        let parent = value
            .get("parentUuid")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty());
        parents.insert(uuid, parent);
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(uuid);
        }
    }

    // The child of the fork on the way to the remote tip.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut current = remote_tip;
    let first_kept = loop {
        if !visited.insert(current) {
            return Repair::Unchanged;
        }
        match parents.get(current) {
            Some(Some(parent)) if *parent == fork_leaf => break current,
            Some(Some(parent)) => current = parent,
            _ => return Repair::Unchanged,
        }
    };

    let descendants = |root: &str| -> HashSet<&str> {
        let mut found: HashSet<&str> = HashSet::new();
        let mut queue: Vec<&str> = children.get(root).cloned().unwrap_or_default();
        while let Some(next) = queue.pop() {
            if found.insert(next) {
                queue.extend(children.get(next).into_iter().flatten().copied());
            }
        }
        found
    };
    let mut kept = descendants(first_kept);
    kept.insert(first_kept);
    let stale: HashSet<&str> = descendants(fork_leaf)
        .into_iter()
        .filter(|uuid| !kept.contains(uuid))
        .collect();
    if stale.is_empty() {
        return Repair::Unchanged;
    }

    let first_real = lines.iter().find_map(|line| {
        let uuid = line.uuid.as_deref().filter(|u| stale.contains(u))?;
        line.value
            .as_ref()
            .filter(|v| is_real_message(v))
            .map(|_| uuid)
    });

    let names_stale_leaf = |line: &RepairLine| {
        line.value.as_ref().is_some_and(|v| {
            v.get("type").and_then(|t| t.as_str()) == Some("last-prompt")
                && v.get("leafUuid")
                    .and_then(|l| l.as_str())
                    .is_some_and(|leaf| leaf == fork_leaf || stale.contains(leaf))
        })
    };
    let main: Vec<u8> = lines
        .iter()
        .filter(|line| {
            !line.uuid.as_deref().is_some_and(|u| stale.contains(u)) && !names_stale_leaf(line)
        })
        .flat_map(|line| line.text.iter().copied())
        .collect();

    let Some(first_real) = first_real else {
        return Repair::Rewritten(main);
    };
    let branch_id = branch_session_id(session_id, first_real);
    let without_kept: Vec<u8> = lines
        .iter()
        .filter(|line| !line.uuid.as_deref().is_some_and(|u| kept.contains(u)))
        .flat_map(|line| line.text.iter().copied())
        .collect();
    let branch = branch_transcript(session_id, &branch_id, &without_kept);
    Repair::Split {
        main,
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

/// Sessions running on this machine right now, as `(pid, session id)`:
/// `~/.claude/sessions/*.json` records whose `pid` is alive. Their
/// transcripts are being written, so a pull never rewrites them.
pub fn live_sessions(claude_home: &Path) -> Vec<(u32, String)> {
    let Ok(records) = std::fs::read_dir(claude_home.join("sessions")) else {
        return Vec::new();
    };
    records
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| std::fs::read(&p).ok())
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter_map(|record| {
            let pid = u32::try_from(record.get("pid")?.as_u64()?).ok()?;
            let session = record.get("sessionId")?.as_str()?.to_string();
            process_is_alive(pid).then_some((pid, session))
        })
        .collect()
}

/// The ids of the sessions running on this machine right now.
pub fn live_session_ids(claude_home: &Path) -> HashSet<String> {
    live_sessions(claude_home)
        .into_iter()
        .map(|(_, session)| session)
        .collect()
}

/// The live processes (Claude Code windows) holding `session_id`, sorted.
pub fn live_pids_for(claude_home: &Path, session_id: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = live_sessions(claude_home)
        .into_iter()
        .filter(|(_, session)| session == session_id)
        .map(|(pid, _)| pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
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

/// How many times a write retries on Windows, where an antivirus scan or
/// an indexer can hold a file open for a moment, and how long it waits.
const WINDOWS_RETRIES: u32 = 5;
const WINDOWS_RETRY_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether an I/O error is a Windows sharing or lock violation.
fn is_sharing_violation(error: &std::io::Error) -> bool {
    cfg!(windows)
        && (matches!(error.raw_os_error(), Some(5 | 32 | 33))
            || error.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Write a whole file through a temporary sibling and a rename, so Claude
/// Code never reads half of it. On Windows the rename is retried a few
/// times when the target is momentarily held open.
pub fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("No parent directory for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(temporary.path(), content)?;
    let mut attempt = 0;
    loop {
        match temporary.persist(path) {
            Ok(_) => return Ok(()),
            Err(e) if attempt < WINDOWS_RETRIES && is_sharing_violation(&e.error) => {
                attempt += 1;
                temporary = e.file;
                std::thread::sleep(WINDOWS_RETRY_WAIT);
            }
            Err(e) => return Err(e).with_context(|| format!("Failed to write {}", path.display())),
        }
    }
}

/// Append `bytes` to an existing file, in one write with the file opened in
/// append mode: what a live transcript receives, never a rewrite. On
/// Windows a sharing violation is retried a few times; the error that
/// remains is the caller's to answer (a pull defers the session).
pub fn append_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut attempt = 0;
    loop {
        let result = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(bytes)
            });
        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt < WINDOWS_RETRIES && is_sharing_violation(&e) => {
                attempt += 1;
                std::thread::sleep(WINDOWS_RETRY_WAIT);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to append to {}", path.display()))
            }
        }
    }
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
    fn a_push_never_drops_entries_only_the_repository_has() {
        let dir = tempfile::TempDir::new().unwrap();
        let local_path = dir.path().join(format!("{S}.jsonl"));
        let local = with(
            prefix(),
            &[message("l1", Some("p2"), 9, "local"), last_prompt("l1")],
        );
        let behind = file(&prefix());
        let other_turn = with(
            prefix(),
            &[message("r1", Some("p2"), 5, "remota"), last_prompt("r1")],
        );

        assert!(
            !repository_is_ahead(&local_path, &local, &behind),
            "the repository is behind"
        );
        assert!(!repository_is_ahead(&local_path, &local, &local));
        assert!(
            repository_is_ahead(&local_path, &behind, &other_turn),
            "a turn this machine has not pulled"
        );
        assert!(
            repository_is_ahead(&local_path, &local, &other_turn),
            "a diverged copy this machine has not split"
        );

        // Once a pull split it and kept the local continuation as the
        // session, the other turn lives in the branch here: push is safe.
        let Reconciled::Split {
            winner: Side::Remote,
            ..
        } = reconcile(S, &local, &other_turn)
        else {
            panic!("the remote turn is older, so it keeps the session");
        };
        let later_turn = with(
            prefix(),
            &[
                message("r2", Some("p2"), 12, "remota tarde"),
                last_prompt("r2"),
            ],
        );
        let Reconciled::Split {
            winner: Side::Local,
            branch_id,
            branch,
        } = reconcile(S, &local, &later_turn)
        else {
            panic!("the local turn is older, so it keeps the session");
        };
        assert!(repository_is_ahead(&local_path, &local, &later_turn));
        std::fs::write(dir.path().join(format!("{branch_id}.jsonl")), &branch).unwrap();
        assert!(!repository_is_ahead(&local_path, &local, &later_turn));
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

    // --- live fast-forward and the repair of a stale window ----------------

    fn reply(uuid: &str, parent: &str, minute: u32, text: &str) -> String {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\
             \"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}},\"uuid\":\"{uuid}\",\
             \"timestamp\":\"2026-11-17T10:{minute:02}:00.000Z\",\"sessionId\":\"{S}\"}}\n"
        )
    }

    fn tool_result(uuid: &str, parent: &str) -> String {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\
             \"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"ok\"}}]}},\
             \"uuid\":\"{uuid}\",\"sessionId\":\"{S}\"}}\n"
        )
    }

    fn noise(uuid: &str, parent: &str, text: &str, meta: bool) -> String {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"isMeta\":{meta},\"type\":\"user\",\"message\":{{\"role\":\"user\",\
             \"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\"sessionId\":\"{S}\"}}\n"
        )
    }

    /// The three entries `/exit` appends, off `leaf`.
    fn exit_noise(leaf: &str) -> Vec<String> {
        vec![
            noise("x1", leaf, "<local-command-caveat>Caveat: ...", true),
            noise("x2", "x1", "<command-name>/exit</command-name>", false),
            noise(
                "x3",
                "x2",
                "<local-command-stdout>Bye!</local-command-stdout>",
                false,
            ),
            last_prompt(leaf),
        ]
    }

    #[test]
    fn a_remote_that_only_added_is_appended_in_remote_order() {
        let local = file(&prefix());
        let remote = with(
            prefix(),
            &[
                message("r1", Some("p2"), 5, "sigue"),
                reply("r2", "r1", 6, "claro"),
                last_prompt("r2"),
            ],
        );
        let appended = live_fast_forward(&local, &remote).expect("a fast-forward");
        let mut grown = local.clone();
        grown.extend_from_slice(&appended.bytes);
        assert_eq!(
            grown, remote,
            "the local file plus the append is the remote"
        );
        assert_eq!(appended.fork_leaf.as_deref(), Some("p2"));
        assert_eq!(appended.remote_tip.as_deref(), Some("r2"));
        assert_eq!(appended.stale_span(), Some(("p2", "r2")));
    }

    #[test]
    fn the_append_starts_with_the_newline_a_file_lacks() {
        let mut local = file(&prefix());
        local.pop();
        let remote = with(prefix(), &[message("r1", Some("p2"), 5, "sigue")]);
        let appended = live_fast_forward(&local, &remote).unwrap();
        assert_eq!(appended.bytes[0], b'\n');
        let mut grown = local.clone();
        grown.extend_from_slice(&appended.bytes);
        assert_eq!(grown, remote);
    }

    #[test]
    fn local_metadata_does_not_stop_a_fast_forward_but_a_local_message_does() {
        let remote = with(
            prefix(),
            &[message("r1", Some("p2"), 5, "sigue"), last_prompt("r1")],
        );
        let mut with_title = prefix();
        with_title.push(format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"mi chat\",\"sessionId\":\"{S}\"}}\n"
        ));
        let local = file(&with_title);
        let appended = live_fast_forward(&local, &remote).expect("metadata only locally");
        assert_eq!(appended.remote_tip.as_deref(), Some("r1"));
        assert!(!String::from_utf8_lossy(&appended.bytes).contains("mi chat"));

        let diverged = with(prefix(), &[message("l1", Some("p2"), 7, "mio")]);
        assert_eq!(
            live_fast_forward(&diverged, &remote),
            None,
            "both added messages"
        );
        assert_eq!(
            live_fast_forward(&remote, &local),
            None,
            "the local side is ahead"
        );
        assert_eq!(live_fast_forward(&local, &local), None, "identical");
    }

    #[test]
    fn metadata_on_both_sides_is_not_a_fast_forward() {
        let title = |t: &str| {
            format!("{{\"type\":\"custom-title\",\"customTitle\":\"{t}\",\"sessionId\":\"{S}\"}}\n")
        };
        let local = with(prefix(), &[title("a")]);
        let remote = with(prefix(), &[title("b")]);
        assert_eq!(live_fast_forward(&local, &remote), None);
    }

    #[test]
    fn a_metadata_only_addition_appends_without_a_stale_span() {
        let local = file(&prefix());
        let remote = with(
            prefix(),
            &[format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"nuevo\",\"sessionId\":\"{S}\"}}\n"
            )],
        );
        let appended = live_fast_forward(&local, &remote).unwrap();
        assert_eq!(
            appended.stale_span(),
            None,
            "no message: no window is behind"
        );
        assert!(String::from_utf8_lossy(&appended.bytes).contains("nuevo"));
    }

    /// A chat both machines share up to `p2`, which the other PC continued to
    /// `r2`; the old window then closed with `/exit`.
    fn exited() -> Vec<String> {
        let mut lines = prefix();
        lines.extend([
            message("r1", Some("p2"), 5, "sigue"),
            reply("r2", "r1", 6, "claro"),
            last_prompt("r2"),
        ]);
        lines.extend(exit_noise("p2"));
        lines
    }

    #[test]
    fn exit_noise_off_the_old_leaf_is_taken_out() {
        let bytes = file(&exited());
        let Repair::Rewritten(fixed) = repair_stale_branch(S, &bytes, "p2", "r2") else {
            panic!("only noise hangs off the old leaf");
        };
        let expected = [
            message("p1", None, 0, "hola, revisa el parser"),
            message("p2", Some("p1"), 1, "y los tests"),
            message("r1", Some("p2"), 5, "sigue"),
            reply("r2", "r1", 6, "claro"),
            last_prompt("r2"),
        ];
        assert_eq!(String::from_utf8(fixed).unwrap(), expected.concat());
    }

    #[test]
    fn a_real_message_off_the_old_leaf_becomes_a_branch() {
        let mut lines = prefix();
        lines.extend([
            message("r1", Some("p2"), 5, "sigue"),
            reply("r2", "r1", 6, "claro"),
            last_prompt("r2"),
            message("s1", Some("p2"), 9, "desde la ventana vieja"),
            reply("s2", "s1", 10, "respuesta vieja"),
            last_prompt("s2"),
        ]);
        let bytes = file(&lines);
        let Repair::Split {
            main,
            branch_id,
            branch,
        } = repair_stale_branch(S, &bytes, "p2", "r2")
        else {
            panic!("a real stale message splits");
        };
        assert_eq!(branch_id, branch_session_id(S, "s1"));
        let main = String::from_utf8(main).unwrap();
        assert!(main.contains("\"uuid\":\"r2\"") && !main.contains("desde la ventana vieja"));
        assert!(!main.contains("\"uuid\":\"s2\"") && !main.contains("\"leafUuid\":\"s2\""));
        let branch = String::from_utf8(branch).unwrap();
        assert!(branch.contains("desde la ventana vieja") && branch.contains("respuesta vieja"));
        assert!(
            branch.contains("\"uuid\":\"p2\""),
            "the shared history comes along"
        );
        assert!(!branch.contains("\"uuid\":\"r1\"") && !branch.contains("\"uuid\":\"r2\""));
        assert!(!branch.contains(S), "moved to the branch's own session id");
        assert!(branch.ends_with(&format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"hola, revisa el parser (rama paralela)\",\
             \"sessionId\":\"{branch_id}\"}}\n"
        )));
    }

    #[test]
    fn a_tool_result_counts_as_a_real_message_and_system_entries_as_noise() {
        let mut lines = prefix();
        lines.extend([
            message("r1", Some("p2"), 5, "sigue"),
            tool_result("s1", "p2"),
        ]);
        assert!(matches!(
            repair_stale_branch(S, &file(&lines), "p2", "r1"),
            Repair::Split { .. }
        ));

        let mut lines = prefix();
        lines.extend([
            message("r1", Some("p2"), 5, "sigue"),
            format!(
                "{{\"parentUuid\":\"p2\",\"type\":\"system\",\"subtype\":\"x\",\"uuid\":\"s1\",\"sessionId\":\"{S}\"}}\n"
            ),
        ]);
        assert!(matches!(
            repair_stale_branch(S, &file(&lines), "p2", "r1"),
            Repair::Rewritten(_)
        ));
    }

    #[test]
    fn branches_that_do_not_fork_at_the_old_leaf_are_never_touched() {
        // A rewind: a second child of p1, with its own continuation.
        let mut lines = prefix();
        lines.extend([
            message("w1", Some("p1"), 3, "otra pregunta (rewind)"),
            reply("w2", "w1", 4, "otra respuesta"),
            message("r1", Some("p2"), 5, "sigue"),
            reply("r2", "r1", 6, "claro"),
        ]);
        lines.extend(exit_noise("p2"));
        let bytes = file(&lines);
        let Repair::Rewritten(fixed) = repair_stale_branch(S, &bytes, "p2", "r2") else {
            panic!("the noise goes");
        };
        let fixed = String::from_utf8(fixed).unwrap();
        for kept in ["w1", "w2", "r1", "r2", "p1", "p2"] {
            assert!(fixed.contains(&format!("\"uuid\":\"{kept}\"")), "{kept}");
        }
        assert!(!fixed.contains("\"uuid\":\"x1\""));

        // Nothing hangs off the leaf: nothing to do.
        let clean = file(&[prefix(), vec![message("r1", Some("p2"), 5, "sigue")]].concat());
        assert_eq!(
            repair_stale_branch(S, &clean, "p2", "r1"),
            Repair::Unchanged
        );
    }

    #[test]
    fn a_tip_that_does_not_descend_from_the_leaf_changes_nothing() {
        let bytes = file(&exited());
        assert_eq!(
            repair_stale_branch(S, &bytes, "p2", "nope"),
            Repair::Unchanged
        );
        assert_eq!(
            repair_stale_branch(S, &bytes, "p2", "p2"),
            Repair::Unchanged
        );
        assert_eq!(
            repair_stale_branch(S, &bytes, "x1", "r2"),
            Repair::Unchanged
        );
    }

    #[test]
    fn a_lone_last_prompt_is_left_alone_when_nothing_was_appended() {
        // An abrupt close: only last-prompt and cost-state.
        let mut lines = prefix();
        lines.extend([message("r1", Some("p2"), 5, "sigue"), last_prompt("p2")]);
        assert_eq!(
            repair_stale_branch(S, &file(&lines), "p2", "r1"),
            Repair::Unchanged
        );
    }

    #[test]
    fn the_repaired_file_ends_on_the_remote_branch() {
        // After the repair, the newest message entry is the remote tip again.
        let Repair::Rewritten(fixed) = repair_stale_branch(S, &file(&exited()), "p2", "r2") else {
            panic!();
        };
        let last_message = entries(&fixed)
            .into_iter()
            .rev()
            .find(|e| is_message_entry(e))
            .and_then(|e| e.uuid);
        assert_eq!(last_message.as_deref(), Some("r2"));
    }

    #[test]
    fn append_file_adds_to_the_end_and_write_file_replaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        write_file(&path, b"uno\n").unwrap();
        append_file(&path, b"dos\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"uno\ndos\n");
        assert!(append_file(&dir.path().join("missing.jsonl"), b"x").is_err());
    }
}
