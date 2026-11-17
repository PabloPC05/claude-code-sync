//! Windows that hold an out-of-date chat.
//!
//! Claude Code loads a transcript once and keeps the conversation in memory.
//! When a pull appends the other machine's turns to a chat that is open here
//! (see `crate::branches::live_fast_forward`), that window does not know: its
//! next message would continue from its old leaf and fork the file, and
//! closing it with `/exit` appends entries off the old leaf that make Claude
//! Code resume the old branch.
//!
//! So the pull leaves a marker, `<config dir>/stale/<session id>.json`, naming
//! the windows (process ids) that are behind. Then:
//!
//! - the `UserPromptSubmit` hook blocks a prompt typed in such a window and
//!   says to `/resume` ([`stale_for`]);
//! - the `SessionStart` hook clears the window's marker: it loaded the file
//!   again ([`refresh`]);
//! - when the window closes, what it appended off the old leaf is taken out
//!   of the transcript ([`ended`], or [`sweep`] at the next sync for a window
//!   that died without saying).
//!
//! Hooks run without the sync lock, so markers change under a small lock of
//! their own.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Overrides [`claude_ancestors`]: the process id a hook treats as Claude
/// Code's. For tests.
pub const HOOK_PID_ENV: &str = "CLAUDE_CODE_SYNC_HOOK_PID";

/// One chat whose open windows are behind the transcript on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub session_id: String,
    /// Absolute path of the transcript.
    pub transcript: PathBuf,
    /// The windows that hold the old version.
    pub pids: Vec<u32>,
    /// The leaf those windows are on.
    pub fork_leaf: String,
    /// The newest message a pull appended.
    pub remote_tip: String,
    pub updated_at: String,
}

/// `<config dir>/stale`, not created.
fn stale_dir() -> Result<PathBuf> {
    Ok(crate::config::ConfigManager::config_dir()?.join("stale"))
}

/// The marker's file, or `None` when the session id could name something
/// else than a file in the directory (it comes from a hook's input).
fn marker_path(dir: &Path, session_id: &str) -> Option<PathBuf> {
    let plain = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    plain.then(|| dir.join(format!("{session_id}.json")))
}

/// Whether the directory holds any marker: what keeps the prompt hook from
/// doing any work on a machine with nothing stale.
fn has_markers(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|x| x == "json"))
    })
}

fn read_marker(path: &Path) -> Option<Marker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn read_markers(dir: &Path) -> Vec<Marker> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut markers: Vec<Marker> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| read_marker(&p))
        .collect();
    markers.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    markers
}

fn write_marker(dir: &Path, marker: &Marker) -> Result<()> {
    let path = marker_path(dir, &marker.session_id)
        .with_context(|| format!("Not a usable session id: {}", marker.session_id))?;
    let mut text = serde_json::to_string_pretty(marker)?;
    text.push('\n');
    crate::branches::write_file(&path, text.as_bytes())
}

fn delete_marker(dir: &Path, session_id: &str) {
    if let Some(path) = marker_path(dir, session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Held while markers change. The sync and the hooks both change them, and
/// only the sync holds the sync lock.
struct MarkerLock {
    _file: File,
}

impl MarkerLock {
    fn take() -> Result<MarkerLock> {
        let path = crate::config::ConfigManager::ensure_config_dir()?.join("stale.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("Failed to lock {}", path.display()))?;
        Ok(MarkerLock { _file: file })
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn alive(pid: u32) -> bool {
    crate::branches::process_is_alive(pid)
}

/// Record that the windows `live_pids` hold an old version of the chat:
/// `fork_leaf` is the leaf they are on and `remote_tip` the newest message a
/// pull appended after it. An existing marker keeps its `fork_leaf` (the
/// oldest leaf any window is on), gains the new pids beside the ones still
/// alive, and takes the new `remote_tip`.
pub fn mark(
    session_id: &str,
    transcript: &Path,
    live_pids: &[u32],
    fork_leaf: &str,
    remote_tip: &str,
) -> Result<Marker> {
    let dir = stale_dir()?;
    let _lock = MarkerLock::take()?;
    let existing = marker_path(&dir, session_id).and_then(|p| read_marker(&p));
    let mut pids: Vec<u32> = existing
        .iter()
        .flat_map(|m| m.pids.iter().copied())
        .filter(|&pid| alive(pid))
        .chain(live_pids.iter().copied())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    let marker = Marker {
        session_id: session_id.to_string(),
        transcript: transcript.to_path_buf(),
        pids,
        fork_leaf: existing.map_or_else(|| fork_leaf.to_string(), |m| m.fork_leaf),
        remote_tip: remote_tip.to_string(),
        updated_at: now(),
    };
    write_marker(&dir, &marker)?;
    Ok(marker)
}

/// The window `pid` loaded its chat again: it is no longer behind. Removed
/// from every marker; a marker left with no window is deleted.
pub fn refresh(pid: u32) -> Result<()> {
    let dir = stale_dir()?;
    if !has_markers(&dir) {
        return Ok(());
    }
    let _lock = MarkerLock::take()?;
    for mut marker in read_markers(&dir) {
        if !marker.pids.contains(&pid) {
            continue;
        }
        marker.pids.retain(|&p| p != pid);
        if marker.pids.is_empty() {
            delete_marker(&dir, &marker.session_id);
        } else {
            marker.updated_at = now();
            write_marker(&dir, &marker)?;
        }
    }
    Ok(())
}

/// Take a closed window's leftovers out of its transcript, and delete the
/// marker. `Ok(true)` when the transcript was changed.
fn repair_marker(marker: &Marker) -> Result<bool> {
    use crate::branches::{repair_stale_branch, Repair};
    let bytes = match std::fs::read(&marker.transcript) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read {}", marker.transcript.display()))
        }
    };
    match repair_stale_branch(
        &marker.session_id,
        &bytes,
        &marker.fork_leaf,
        &marker.remote_tip,
    ) {
        Repair::Unchanged => Ok(false),
        Repair::Rewritten(main) => {
            crate::branches::write_file(&marker.transcript, &main)?;
            Ok(true)
        }
        Repair::Split {
            main,
            branch_id,
            branch,
        } => {
            let project = marker
                .transcript
                .parent()
                .context("A transcript has a directory")?;
            // The branch first: a failure in between leaves the messages in
            // both files, never in neither.
            let mut deferred = Vec::new();
            crate::branches::write_branch(
                &project.join(format!("{branch_id}.jsonl")),
                &branch_id,
                &branch,
                &|_| false,
                &mut deferred,
            )?;
            crate::branches::copy_sidecars(
                &[&project.join(&marker.session_id)],
                &project.join(&branch_id),
                &|_| false,
            )?;
            crate::branches::write_file(&marker.transcript, &main)?;
            log::info!(
                "A window closed on an old version of {}: its own messages are now session {branch_id}",
                marker.session_id
            );
            Ok(true)
        }
    }
}

/// Whether a window other than `except` holds the session now.
fn live_elsewhere(session_id: &str, except: Option<u32>) -> bool {
    let Ok(home) = crate::sync::discovery::claude_home_dir() else {
        // Unknown counts as live: rewriting a live file costs messages,
        // waiting costs one sync.
        return true;
    };
    crate::branches::live_pids_for(&home, session_id)
        .into_iter()
        .any(|p| Some(p) != except)
}

/// How long a closing window waits for a running sync before it leaves the
/// repair to the next one. The `SessionEnd` hook has 10 s; a `claude
/// --continue` typed right after closing must already see the repaired chat.
const ENDED_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(7);

/// The sync lock, waiting up to `wait` for a running sync to finish.
fn sync_lock_within(wait: std::time::Duration) -> Result<Option<crate::sync_lock::SyncLock>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Some(lock) = crate::sync_lock::SyncLock::try_acquire()? {
            return Ok(Some(lock));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// SessionEnd: the window `pid` is closing. Removed from its markers; when
/// no window is left behind on a chat, the transcript is repaired and the
/// marker deleted.
///
/// The repair needs the sync lock, so it never rewrites a file a pull is
/// writing; it waits up to [`ENDED_LOCK_WAIT`] for a running sync, and past
/// that leaves the repair to the next one's [`sweep`].
pub fn ended(pid: u32) -> Result<()> {
    let dir = stale_dir()?;
    if !has_markers(&dir) {
        return Ok(());
    }
    if !read_markers(&dir).iter().any(|m| m.pids.contains(&pid)) {
        return Ok(());
    }
    // Sync lock first, marker lock second: the order a sync takes them in
    // (its sweep and its marks run under the sync lock), so neither waits
    // on the other.
    let sync = sync_lock_within(ENDED_LOCK_WAIT)?;
    let _lock = MarkerLock::take()?;
    for mut marker in read_markers(&dir) {
        if !marker.pids.contains(&pid) {
            continue;
        }
        marker.pids.retain(|&p| p != pid && alive(p));
        let settled = marker.pids.is_empty()
            && !live_elsewhere(&marker.session_id, Some(pid))
            && sync.is_some()
            && {
                repair_marker(&marker)?;
                true
            };
        if settled {
            delete_marker(&dir, &marker.session_id);
        } else {
            marker.updated_at = now();
            write_marker(&dir, &marker)?;
        }
    }
    Ok(())
}

/// Start of every sync, under the sync lock: repair the chats whose
/// windows are all gone (closed without a SessionEnd, or while a sync ran).
/// A chat some other window holds is left for later.
pub fn sweep() -> Result<()> {
    let dir = stale_dir()?;
    if !has_markers(&dir) {
        return Ok(());
    }
    let _lock = MarkerLock::take()?;
    for mut marker in read_markers(&dir) {
        let before = marker.pids.len();
        marker.pids.retain(|&p| alive(p));
        if marker.pids.is_empty() && !live_elsewhere(&marker.session_id, None) {
            if let Err(e) = repair_marker(&marker) {
                log::warn!("Could not repair {}: {e:#}", marker.transcript.display());
                continue;
            }
            delete_marker(&dir, &marker.session_id);
        } else if marker.pids.len() != before {
            marker.updated_at = now();
            write_marker(&dir, &marker)?;
        }
    }
    Ok(())
}

/// The marker of `session_id` when one of `ancestors` (the processes above a
/// hook) is a window it names that is still alive.
pub fn stale_for(session_id: &str, ancestors: &[u32]) -> Option<Marker> {
    let dir = stale_dir().ok()?;
    let marker = read_marker(&marker_path(&dir, session_id)?)?;
    marker
        .pids
        .iter()
        .any(|pid| ancestors.contains(pid) && alive(*pid))
        .then_some(marker)
}

/// Whether any marker exists: the prompt hook's fast path.
pub fn any_marker() -> bool {
    stale_dir().is_ok_and(|dir| has_markers(&dir))
}

/// The parent and the grandparent of this process: Claude Code, as seen by
/// a hook it runs (directly, or through a shell). `CLAUDE_CODE_SYNC_HOOK_PID`
/// replaces the answer.
pub fn claude_ancestors() -> Vec<u32> {
    if let Some(pid) = std::env::var(HOOK_PID_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        return vec![pid];
    }
    ancestors_of_this_process()
}

#[cfg(unix)]
fn ancestors_of_this_process() -> Vec<u32> {
    let parent = std::os::unix::process::parent_id();
    let mut found = vec![parent];
    // Best effort: `ps` is on every Unix a Claude Code runs on.
    let grandparent = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &parent.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        });
    if let Some(grandparent) = grandparent.filter(|&g| g > 0 && g != parent) {
        found.push(grandparent);
    }
    found
}

#[cfg(windows)]
fn ancestors_of_this_process() -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // pid -> parent pid, from one snapshot of the process table.
    let mut table: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    // SAFETY: the snapshot handle is checked and closed; the entry is a
    // plain struct whose size field is set before the calls that read it.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            table.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    let mut found = Vec::new();
    let mut current = std::process::id();
    for _ in 0..2 {
        match table.get(&current).copied() {
            Some(parent) if parent > 0 && !found.contains(&parent) => {
                found.push(parent);
                current = parent;
            }
            _ => break,
        }
    }
    found
}

#[cfg(not(any(unix, windows)))]
fn ancestors_of_this_process() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::process::{Child, Command, Stdio};

    const S: &str = "22222222-2222-4222-8222-222222222222";

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
            let names = [
                "CLAUDE_CODE_SYNC_CONFIG_DIR",
                "CLAUDE_CODE_SYNC_CLAUDE_DIR",
                HOOK_PID_ENV,
            ];
            let saved = names
                .into_iter()
                .map(|n| (n, std::env::var(n).ok()))
                .collect();
            std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", tmp.path().join("cfg"));
            std::env::set_var("CLAUDE_CODE_SYNC_CLAUDE_DIR", &claude);
            std::env::remove_var(HOOK_PID_ENV);
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

        /// Say that `pid` is a window on `S`.
        fn open_window(&self, pid: u32) {
            let dir = self.claude.join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{pid}.json")),
                format!("{{\"pid\":{pid},\"sessionId\":\"{S}\"}}"),
            )
            .unwrap();
        }

        fn marker_file(&self) -> PathBuf {
            stale_dir().unwrap().join(format!("{S}.json"))
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

    /// A pid that is not running: a child that was started and reaped.
    fn dead_pid() -> u32 {
        let mut child = long_child();
        let pid = child.id();
        kill_in_place(&mut child);
        pid
    }

    fn kill_in_place(child: &mut Child) {
        child.kill().ok();
        child.wait().ok();
    }

    fn entry(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            "{{\"parentUuid\":{parent},\"type\":\"{kind}\",\"message\":{{\"role\":\"{kind}\",\
             \"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\"sessionId\":\"{S}\"}}\n"
        )
    }

    /// p1 - p2, continued to r2 on the other PC, then `/exit` noise off p2.
    fn exited_transcript() -> String {
        [
            entry("p1", None, "user", "hola"),
            entry("p2", Some("p1"), "assistant", "buenas"),
            entry("r1", Some("p2"), "user", "sigue"),
            entry("r2", Some("r1"), "assistant", "claro"),
            entry(
                "x1",
                Some("p2"),
                "user",
                "<command-name>/exit</command-name>",
            ),
            format!("{{\"type\":\"last-prompt\",\"leafUuid\":\"p2\",\"sessionId\":\"{S}\"}}\n"),
        ]
        .concat()
    }

    #[test]
    #[serial]
    fn a_marker_keeps_its_fork_leaf_and_gathers_the_live_pids() {
        let env = Env::new();
        let (a, b) = (long_child(), long_child());
        let gone = dead_pid();

        let first = mark(S, &env.transcript(), &[a.id(), gone], "p2", "r2").unwrap();
        assert_eq!(
            (first.fork_leaf.as_str(), first.remote_tip.as_str()),
            ("p2", "r2")
        );
        // The next append: a later leaf and tip, one more window.
        let second = mark(S, &env.transcript(), &[b.id()], "r2", "r4").unwrap();
        assert_eq!(second.fork_leaf, "p2", "the oldest leaf stays");
        assert_eq!(second.remote_tip, "r4");
        let mut expected = vec![a.id(), b.id()];
        expected.sort_unstable();
        assert_eq!(second.pids, expected, "both live windows, sorted");
        assert!(!second.pids.contains(&gone), "a dead window is dropped");

        let on_disk: Marker =
            serde_json::from_slice(&std::fs::read(env.marker_file()).unwrap()).unwrap();
        assert_eq!(on_disk, second);
        assert_eq!(on_disk.transcript, env.transcript());
        kill(a);
        kill(b);
    }

    #[test]
    #[serial]
    fn the_guard_finds_a_marker_through_a_live_ancestor_only() {
        let env = Env::new();
        let window = long_child();
        mark(S, &env.transcript(), &[window.id()], "p2", "r2").unwrap();

        assert!(any_marker());
        assert!(stale_for(S, &[window.id()]).is_some());
        assert!(stale_for(S, &[1, window.id()]).is_some(), "any ancestor");
        assert!(
            stale_for(S, &[window.id() + 1_000_000]).is_none(),
            "another window"
        );
        assert!(stale_for("another-chat", &[window.id()]).is_none());
        assert!(stale_for("../escape", &[window.id()]).is_none());

        std::env::set_var(HOOK_PID_ENV, window.id().to_string());
        assert_eq!(claude_ancestors(), vec![window.id()]);
        std::env::remove_var(HOOK_PID_ENV);

        kill(window);
        assert!(
            stale_for(S, &[dead_pid()]).is_none(),
            "a window that is gone is not behind"
        );
        drop(env);
    }

    #[test]
    #[serial]
    fn refresh_clears_one_window_and_deletes_a_marker_left_with_none() {
        let env = Env::new();
        let (a, b) = (long_child(), long_child());
        mark(S, &env.transcript(), &[a.id(), b.id()], "p2", "r2").unwrap();

        refresh(a.id()).unwrap();
        assert!(stale_for(S, &[a.id()]).is_none());
        assert!(
            stale_for(S, &[b.id()]).is_some(),
            "the other window is still behind"
        );
        refresh(b.id()).unwrap();
        assert!(!env.marker_file().exists(), "no window left");
        refresh(b.id()).unwrap();
        kill(a);
        kill(b);
    }

    #[test]
    #[serial]
    fn the_last_window_to_close_repairs_the_transcript_and_the_marker_goes() {
        let env = Env::new();
        let (a, b) = (long_child(), long_child());
        std::fs::write(env.transcript(), exited_transcript()).unwrap();
        env.open_window(a.id());
        env.open_window(b.id());
        mark(S, &env.transcript(), &[a.id(), b.id()], "p2", "r2").unwrap();

        // One of two closes: the file is not touched, a window is still behind.
        ended(a.id()).unwrap();
        std::fs::remove_file(env.claude.join("sessions").join(format!("{}.json", a.id()))).unwrap();
        kill_in_place(&mut { a });
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            exited_transcript()
        );
        assert!(env.marker_file().exists());

        // The other one: repaired.
        ended(b.id()).unwrap();
        let fixed = std::fs::read_to_string(env.transcript()).unwrap();
        assert!(!fixed.contains("/exit") && !fixed.contains("last-prompt"));
        assert!(fixed.contains("\"uuid\":\"r2\""));
        assert!(!env.marker_file().exists());
        kill(b);
    }

    #[test]
    #[serial]
    fn ended_waits_when_a_sync_is_running_and_the_sweep_finishes_the_job() {
        let env = Env::new();
        std::fs::write(env.transcript(), exited_transcript()).unwrap();
        let window = long_child();
        mark(S, &env.transcript(), &[window.id()], "p2", "r2").unwrap();

        let sync = crate::sync_lock::SyncLock::try_acquire().unwrap().unwrap();
        ended(window.id()).unwrap();
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            exited_transcript(),
            "a sync holds the lock: not rewritten now"
        );
        assert!(env.marker_file().exists());
        drop(sync);
        kill(window);

        sweep().unwrap();
        assert!(!std::fs::read_to_string(env.transcript())
            .unwrap()
            .contains("/exit"));
        assert!(!env.marker_file().exists());
    }

    #[test]
    #[serial]
    fn the_sweep_repairs_a_window_that_died_without_saying_and_spares_live_ones() {
        let env = Env::new();
        std::fs::write(env.transcript(), exited_transcript()).unwrap();

        // A window is alive: nothing happens.
        let window = long_child();
        env.open_window(window.id());
        mark(S, &env.transcript(), &[window.id()], "p2", "r2").unwrap();
        sweep().unwrap();
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            exited_transcript()
        );
        assert!(env.marker_file().exists());

        // It dies, but another window opened the chat meanwhile: still held.
        let other = long_child();
        env.open_window(other.id());
        kill(window);
        sweep().unwrap();
        assert_eq!(
            std::fs::read_to_string(env.transcript()).unwrap(),
            exited_transcript(),
            "a live file is never rewritten"
        );
        assert!(env.marker_file().exists());

        // The last one goes: repaired.
        kill(other);
        sweep().unwrap();
        assert!(!std::fs::read_to_string(env.transcript())
            .unwrap()
            .contains("/exit"));
        assert!(!env.marker_file().exists());
    }

    #[test]
    #[serial]
    fn a_real_message_in_the_old_window_becomes_a_parallel_session_on_repair() {
        let env = Env::new();
        let text = [
            entry("p1", None, "user", "hola"),
            entry("p2", Some("p1"), "assistant", "buenas"),
            entry("r1", Some("p2"), "user", "sigue"),
            entry("r2", Some("r1"), "assistant", "claro"),
            entry("s1", Some("p2"), "user", "escrito en la ventana vieja"),
        ]
        .concat();
        std::fs::write(env.transcript(), &text).unwrap();
        mark(S, &env.transcript(), &[dead_pid()], "p2", "r2").unwrap();

        sweep().unwrap();
        let branch = crate::branches::branch_session_id(S, "s1");
        let main = std::fs::read_to_string(env.transcript()).unwrap();
        let parallel =
            std::fs::read_to_string(env.project.join(format!("{branch}.jsonl"))).unwrap();
        assert!(main.contains("\"uuid\":\"r2\"") && !main.contains("ventana vieja"));
        assert!(parallel.contains("ventana vieja") && !parallel.contains("\"uuid\":\"r2\""));
        assert!(parallel.contains("(rama paralela)"));
        assert!(!env.marker_file().exists());
    }

    #[test]
    #[serial]
    fn a_missing_transcript_or_nothing_stale_just_drops_the_marker() {
        let env = Env::new();
        mark(S, &env.transcript(), &[dead_pid()], "p2", "r2").unwrap();
        sweep().unwrap();
        assert!(!env.marker_file().exists(), "no transcript, no marker");

        // An abrupt close appended only a last-prompt: nothing to repair.
        let text = [
            entry("p1", None, "user", "hola"),
            entry("p2", Some("p1"), "assistant", "buenas"),
            entry("r1", Some("p2"), "user", "sigue"),
            format!("{{\"type\":\"last-prompt\",\"leafUuid\":\"p2\",\"sessionId\":\"{S}\"}}\n"),
        ]
        .concat();
        std::fs::write(env.transcript(), &text).unwrap();
        mark(S, &env.transcript(), &[dead_pid()], "p2", "r1").unwrap();
        sweep().unwrap();
        assert_eq!(std::fs::read_to_string(env.transcript()).unwrap(), text);
        assert!(!env.marker_file().exists());
    }

    #[test]
    #[serial]
    fn without_markers_everything_returns_at_once() {
        let _env = Env::new();
        assert!(!any_marker());
        refresh(1).unwrap();
        ended(1).unwrap();
        sweep().unwrap();
        assert!(stale_for(S, &[1]).is_none());
        let ancestors = claude_ancestors();
        assert!(!ancestors.is_empty());
    }
}
