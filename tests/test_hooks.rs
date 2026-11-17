//! `claude-code-sync hook <event>`, run as Claude Code runs it: the built
//! binary, the hook's JSON on stdin, the window's pid as its parent (here,
//! `CLAUDE_CODE_SYNC_HOOK_PID`, or this very process for `whoami`).
//!
//! Serialized: the markers live under an environment-selected directory.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use claude_code_sync::hooks::block_reason;
use claude_code_sync::stale;
use serial_test::serial;
use tempfile::TempDir;

const SESSION: &str = "7a1f0c52-3b9e-4d64-8f21-0c5d9e7b1a33";

struct Env {
    tmp: TempDir,
    saved: Vec<(&'static str, Option<String>)>,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let names = [
            "CLAUDE_CODE_SYNC_CONFIG_DIR",
            "CLAUDE_CODE_SYNC_CLAUDE_DIR",
            "CLAUDE_CODE_SYNC_HOME",
            "CLAUDE_CODE_SYNC_HOOK_PID",
        ];
        let saved = names.iter().map(|n| (*n, std::env::var(n).ok())).collect();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", tmp.path().join("cfg"));
        std::env::set_var("CLAUDE_CODE_SYNC_CLAUDE_DIR", tmp.path().join("claude"));
        std::env::set_var("CLAUDE_CODE_SYNC_HOME", tmp.path().join("home"));
        std::env::remove_var("CLAUDE_CODE_SYNC_HOOK_PID");
        std::fs::create_dir_all(tmp.path().join("claude")).unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        Env { tmp, saved }
    }

    fn transcript(&self) -> PathBuf {
        self.tmp
            .path()
            .join("claude/projects/-p")
            .join(format!("{SESSION}.jsonl"))
    }

    /// Where a detached run writes its output: the file the spawn creates.
    fn autosync_log(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.tmp
                .path()
                .join("home/Library/Logs/claude-code-sync.log")
        } else {
            self.tmp.path().join("cfg/claude-code-sync/autosync.log")
        }
    }

    /// The binary with this environment, and `pid` as the window's pid.
    fn hook(&self, event: &str, stdin: &str, window: Option<u32>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code-sync"));
        command
            .args(["hook", event])
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.tmp.path().join("cfg"))
            .env(
                "CLAUDE_CODE_SYNC_CLAUDE_DIR",
                self.tmp.path().join("claude"),
            )
            .env("CLAUDE_CODE_SYNC_HOME", self.tmp.path().join("home"))
            .env("HOME", self.tmp.path().join("home"))
            .env_remove("CLAUDE_CODE_SYNC_HOOK_PID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(pid) = window {
            command.env("CLAUDE_CODE_SYNC_HOOK_PID", pid.to_string());
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
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

fn prompt_input() -> String {
    format!("{{\"session_id\":\"{SESSION}\",\"prompt\":\"hola\",\"hook_event_name\":\"UserPromptSubmit\"}}")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `hook whoami` is run from this process, so its parent is this process:
/// the same lookup the real hooks make for Claude Code (and, on Windows, the
/// toolhelp snapshot instead of `getppid`).
#[test]
#[serial]
fn whoami_prints_the_parent_first() {
    let env = Env::new();
    let output = env.hook("whoami", "", None);
    assert!(output.status.success());
    let text = stdout(&output);
    let first: u32 = text.lines().next().expect("a line").trim().parse().unwrap();
    assert_eq!(first, std::process::id());
    for line in text.lines() {
        assert!(line.trim().parse::<u32>().is_ok(), "only pids: {text}");
    }

    // The override replaces the answer.
    let overridden = env.hook("whoami", "", Some(4242));
    assert_eq!(stdout(&overridden).trim(), "4242");
}

#[test]
#[serial]
fn the_prompt_hook_blocks_only_a_window_that_is_behind() {
    let env = Env::new();
    let window = long_child();
    let mut window = scopeguard(window);

    // Nothing stale: no output at all, exit 0.
    let quiet = env.hook("prompt", &prompt_input(), Some(window.id()));
    assert!(quiet.status.success());
    assert_eq!(stdout(&quiet), "");

    stale::mark(SESSION, &env.transcript(), &[window.id()], "p2", "r2").unwrap();

    let blocked = env.hook("prompt", &prompt_input(), Some(window.id()));
    assert!(
        blocked.status.success(),
        "a block is JSON, not an exit code"
    );
    let json: serde_json::Value = serde_json::from_str(stdout(&blocked).trim()).unwrap();
    assert_eq!(json["decision"], "block");
    assert_eq!(json["reason"], block_reason(SESSION));
    assert!(block_reason(SESSION).contains(&format!("/resume {SESSION}")));

    // Another window, another chat, and bad input all pass, silently.
    for (input, pid) in [
        (prompt_input(), Some(window.id() + 1_000_000)),
        (
            prompt_input().replace(SESSION, "otro-chat"),
            Some(window.id()),
        ),
        ("not json".to_string(), Some(window.id())),
        (String::new(), Some(window.id())),
    ] {
        let output = env.hook("prompt", &input, pid);
        assert!(output.status.success());
        assert_eq!(stdout(&output), "", "{input}");
    }
    window.kill_now();
}

#[test]
#[serial]
fn session_start_clears_the_marker_then_starts_a_sync() {
    let env = Env::new();
    let mut window = scopeguard(long_child());
    stale::mark(SESSION, &env.transcript(), &[window.id()], "p2", "r2").unwrap();
    assert!(stale::stale_for(SESSION, &[window.id()]).is_some());
    assert!(!env.autosync_log().exists());

    let output = env.hook(
        "session-start",
        &format!("{{\"session_id\":\"{SESSION}\",\"source\":\"resume\"}}"),
        Some(window.id()),
    );
    assert!(output.status.success());
    assert_eq!(stdout(&output), "", "prints nothing");
    assert!(
        stale::stale_for(SESSION, &[window.id()]).is_none(),
        "the window reloaded: no longer behind"
    );
    assert!(!stale::any_marker());
    assert!(
        env.autosync_log().exists(),
        "a detached autosync run was started"
    );

    // And the next prompt goes through.
    let prompt = env.hook("prompt", &prompt_input(), Some(window.id()));
    assert_eq!(stdout(&prompt), "");
    window.kill_now();
}

#[test]
#[serial]
fn session_end_and_stop_start_a_sync_and_print_nothing() {
    let env = Env::new();
    let output = env.hook("stop", "{}", Some(1));
    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(env.autosync_log().exists(), "a turn's end starts a run");

    std::fs::remove_file(env.autosync_log()).unwrap();
    let output = env.hook("session-end", "{}", Some(1));
    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(env.autosync_log().exists(), "a chat's end starts a run");
}

#[test]
#[serial]
fn session_end_repairs_what_the_closing_window_appended() {
    let env = Env::new();
    let window = long_child();
    let mut window = scopeguard(window);
    let user = |uuid: &str, parent: Option<&str>, text: &str| {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(
            "{{\"parentUuid\":{parent},\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\"sessionId\":\"{SESSION}\"}}\n"
        )
    };
    let text = [
        user("p1", None, "hola"),
        user("r1", Some("p1"), "sigue"),
        user("x1", Some("p1"), "<command-name>/exit</command-name>"),
    ]
    .concat();
    std::fs::create_dir_all(env.transcript().parent().unwrap()).unwrap();
    std::fs::write(env.transcript(), &text).unwrap();
    stale::mark(SESSION, &env.transcript(), &[window.id()], "p1", "r1").unwrap();

    let output = env.hook("session-end", "{}", Some(window.id()));
    assert!(output.status.success());
    let fixed = std::fs::read_to_string(env.transcript()).unwrap();
    assert!(fixed.contains("\"uuid\":\"r1\"") && !fixed.contains("/exit"));
    assert!(!stale::any_marker());
    window.kill_now();
}

#[test]
#[serial]
fn every_error_path_exits_zero() {
    let env = Env::new();
    // A config directory that is a file, not a directory.
    std::fs::write(env.tmp.path().join("cfg"), "x").unwrap();
    for event in ["prompt", "session-start", "session-end", "stop", "whoami"] {
        let output = env.hook(event, "garbage", Some(1));
        assert!(output.status.success(), "{event}: {:?}", output.status);
    }
}

/// A child that is killed when the test ends, however it ends.
struct Window(Option<Child>);

fn scopeguard(child: Child) -> Window {
    Window(Some(child))
}

impl Window {
    fn id(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }

    fn kill_now(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.kill_now();
    }
}
