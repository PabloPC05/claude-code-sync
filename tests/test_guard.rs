//! `claude-code-sync guard ...`, driven as Claude Code and Syncthing do: the
//! built binary, the hook's JSON on stdin, a live process standing for the
//! window (`CLAUDE_CODE_SYNC_HOOK_PID`), and files replaced with a temporary
//! file and a rename between the calls. No repository, no `init`: the
//! config directory starts empty.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use claude_code_sync::guard;
use serde_json::Value;
use tempfile::TempDir;

const SESSION: &str = "7a1f0c52-3b9e-4d64-8f21-0c5d9e7b1a33";

struct Env {
    tmp: TempDir,
    window: Child,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("claude/projects/-p")).unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        Env {
            tmp,
            window: long_child(),
        }
    }

    fn claude(&self) -> PathBuf {
        self.tmp.path().join("claude")
    }

    fn project(&self) -> PathBuf {
        self.claude().join("projects/-p")
    }

    fn transcript(&self) -> PathBuf {
        self.project().join(format!("{SESSION}.jsonl"))
    }

    fn windows_dir(&self) -> PathBuf {
        self.tmp.path().join("cfg/claude-code-sync/guard/windows")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code-sync"));
        command
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.tmp.path().join("cfg"))
            .env("CLAUDE_CODE_SYNC_CLAUDE_DIR", self.claude())
            .env("CLAUDE_CODE_SYNC_HOME", self.tmp.path().join("home"))
            .env("HOME", self.tmp.path().join("home"))
            .env_remove("CLAUDE_CODE_SYNC_HOOK_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `guard hook <event>` as the window would run it.
    fn hook(&self, event: &str, stdin: &str) -> Output {
        let mut command = self.command();
        command
            .args(["guard", "hook", event])
            .env("CLAUDE_CODE_SYNC_HOOK_PID", self.window.id().to_string())
            .stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn input(&self) -> String {
        format!(
            "{{\"session_id\":\"{SESSION}\",\"transcript_path\":{},\"hook_event_name\":\"x\"}}",
            serde_json::to_string(&self.transcript().to_string_lossy()).unwrap()
        )
    }

    fn input_with_source(&self, source: &str) -> String {
        format!(
            "{{\"session_id\":\"{SESSION}\",\"transcript_path\":{},\"source\":\"{source}\"}}",
            serde_json::to_string(&self.transcript().to_string_lossy()).unwrap()
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn jsonl_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.project())
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
        self.window.kill().ok();
        self.window.wait().ok();
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn entry(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
    let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
    let content = if kind == "user" {
        format!("\"{text}\"")
    } else {
        format!("[{{\"type\":\"text\",\"text\":\"{text}\"}}]")
    };
    format!(
        "{{\"parentUuid\":{parent},\"type\":\"{kind}\",\"message\":{{\"role\":\"{kind}\",\
         \"content\":{content}}},\"uuid\":\"{uuid}\",\"timestamp\":\"2026-01-01T10:00:00.000Z\",\
         \"sessionId\":\"{SESSION}\"}}\n"
    )
}

fn pc_a() -> String {
    entry("p1", None, "user", "hola") + &entry("p2", Some("p1"), "assistant", "buenas")
}

fn pc_b_turns() -> String {
    entry("r1", Some("p2"), "user", "sigue") + &entry("r2", Some("r1"), "assistant", "claro")
}

/// What `/exit` appends off the window's own leaf.
fn exit_noise(parent: &str) -> String {
    format!(
        "{{\"parentUuid\":\"{parent}\",\"isMeta\":true,\"type\":\"user\",\"message\":\
         {{\"role\":\"user\",\"content\":\"<local-command-caveat>x</local-command-caveat>\"}},\
         \"uuid\":\"n1\",\"sessionId\":\"{SESSION}\"}}\n"
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
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// What Syncthing does: a temporary file, renamed over the original.
fn syncthing_replaces(path: &Path, text: &str) {
    let temporary = path.with_file_name(".syncthing.tmp");
    std::fs::write(&temporary, text).unwrap();
    std::fs::rename(&temporary, path).unwrap();
}

fn last_message_uuid(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type")
                .is_some_and(|t| t == "user" || t == "assistant")
        })
        .filter_map(|v| v.get("uuid")?.as_str().map(str::to_string))
        .next_back()
        .unwrap()
}

#[test]
fn an_append_keeps_the_file_identity_and_a_temp_file_rename_changes_it() {
    // Also the check of the Windows code: volume serial and file index.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("chat.jsonl");
    std::fs::write(&path, "one\n").unwrap();
    let original = guard::file_identity(&path).expect("this platform can tell files apart");

    append(&path, "two\n");
    assert!(original.same_file(&guard::file_identity(&path).unwrap()));
    assert_eq!(
        guard::file_identity(&path).unwrap().ino,
        original.ino,
        "an append is the same file"
    );

    syncthing_replaces(&path, "one\ntwo\nthree\n");
    let replaced = guard::file_identity(&path).unwrap();
    assert!(!original.same_file(&replaced));
    assert!(
        (replaced.dev, replaced.ino) != (original.dev, original.ino)
            || replaced.birth != original.birth,
        "{original:?} {replaced:?}"
    );

    // The next append to the new file keeps the new identity.
    append(&path, "four\n");
    assert!(replaced.same_file(&guard::file_identity(&path).unwrap()));
}

#[test]
fn the_other_pcs_turns_block_the_prompt_until_the_window_loads_them() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();

    // The window starts and types a prompt: nothing to say.
    let started = env.hook("start", &env.input_with_source("startup"));
    assert!(started.status.success());
    assert_eq!(stdout(&started), "");
    assert!(env
        .windows_dir()
        .join(format!("{}.json", env.window.id()))
        .is_file());
    let prompt = env.hook("prompt", &env.input());
    assert!(prompt.status.success());
    assert_eq!(stdout(&prompt), "");

    // Its own turn: appended to the same file, with and without a Stop.
    append(&env.transcript(), &entry("a1", Some("p2"), "user", "otra"));
    assert_eq!(stdout(&env.hook("prompt", &env.input())), "");
    assert_eq!(stdout(&env.hook("stop", &env.input())), "");

    // Syncthing brings the other PC's turns (its file has not seen a1).
    syncthing_replaces(
        &env.transcript(),
        &(pc_a() + &entry("a1", Some("p2"), "user", "otra") + &pc_b_turns()),
    );
    let blocked = env.hook("prompt", &env.input());
    assert!(blocked.status.success());
    let decision: Value = serde_json::from_str(stdout(&blocked).trim()).unwrap();
    assert_eq!(decision["decision"], "block");
    assert!(decision["reason"]
        .as_str()
        .unwrap()
        .contains(&format!("/resume {SESSION}")));
    // A Stop in the meantime does not make the window fresh.
    env.hook("stop", &env.input());
    assert!(stdout(&env.hook("prompt", &env.input())).contains("block"));

    // /resume <id> runs SessionStart again.
    env.hook("start", &env.input_with_source("resume"));
    assert_eq!(stdout(&env.hook("prompt", &env.input())), "");
}

#[test]
fn a_replace_that_only_adds_metadata_does_not_block() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();
    env.hook("start", &env.input_with_source("startup"));
    syncthing_replaces(
        &env.transcript(),
        &(pc_a()
            + &format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"t\",\"sessionId\":\"{SESSION}\"}}\n"
            )),
    );
    assert_eq!(stdout(&env.hook("prompt", &env.input())), "");
}

#[test]
fn closing_a_window_that_was_behind_keeps_the_other_pcs_turns_last() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();
    env.hook("start", &env.input_with_source("startup"));

    syncthing_replaces(&env.transcript(), &(pc_a() + &pc_b_turns()));
    // /exit, in the old window.
    append(&env.transcript(), &exit_noise("p2"));
    assert_eq!(last_message_uuid(&env.transcript()), "n3");

    let ended = env.hook("end", &env.input());
    assert!(ended.status.success());
    assert_eq!(stdout(&ended), "");
    assert_eq!(last_message_uuid(&env.transcript()), "r2");
    assert_eq!(env.jsonl_files(), vec![format!("{SESSION}.jsonl")]);
    assert!(
        !env.windows_dir()
            .join(format!("{}.json", env.window.id()))
            .exists(),
        "the window's record is gone"
    );
}

#[test]
fn closing_a_window_with_real_stale_messages_makes_a_parallel_session() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();
    env.hook("start", &env.input_with_source("startup"));
    syncthing_replaces(&env.transcript(), &(pc_a() + &pc_b_turns()));
    append(
        &env.transcript(),
        &(entry("s1", Some("p2"), "user", "mi duda") + &entry("s2", Some("s1"), "assistant", "ok")),
    );
    env.hook("end", &env.input());
    assert_eq!(last_message_uuid(&env.transcript()), "r2");
    assert_eq!(env.jsonl_files().len(), 2);
    let all: String = env
        .jsonl_files()
        .iter()
        .map(|f| std::fs::read_to_string(env.project().join(f)).unwrap())
        .collect();
    assert!(all.contains("(rama paralela)") && all.contains("mi duda"));
}

#[test]
fn a_window_that_never_fell_behind_is_left_alone_when_it_closes() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();
    env.hook("start", &env.input_with_source("startup"));
    append(&env.transcript(), &exit_noise("p2"));
    let before = std::fs::read(env.transcript()).unwrap();
    env.hook("end", &env.input());
    assert_eq!(std::fs::read(env.transcript()).unwrap(), before);
}

#[test]
fn hooks_sweep_the_conflict_copies_of_their_project_and_the_history() {
    let env = Env::new();
    std::fs::write(env.transcript(), pc_a()).unwrap();
    let copy = env.project().join(format!(
        "{SESSION}.sync-conflict-20260102-120000-ABCDEFG.jsonl"
    ));
    std::fs::write(&copy, pc_a() + &pc_b_turns()).unwrap();
    let history = env.claude().join("history.jsonl");
    std::fs::write(&history, "{\"timestamp\":2,\"display\":\"b\"}\n").unwrap();
    std::fs::write(
        env.claude()
            .join("history.sync-conflict-20260102-120000-ABCDEFG.jsonl"),
        "{\"timestamp\":1,\"display\":\"a\"}\n",
    )
    .unwrap();
    // Another project's copy is not this hook's business.
    let elsewhere = env.claude().join("projects/-q");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let other_copy = elsewhere.join(format!(
        "{SESSION}.sync-conflict-20260102-120000-ABCDEFG.jsonl"
    ));
    std::fs::write(elsewhere.join(format!("{SESSION}.jsonl")), pc_a()).unwrap();
    std::fs::write(&other_copy, pc_a() + &pc_b_turns()).unwrap();

    // The window is live (a record is made from its first hook), so the
    // copy is appended to, not renamed over.
    std::fs::create_dir_all(env.claude().join("sessions")).unwrap();
    std::fs::write(
        env.claude()
            .join("sessions")
            .join(format!("{}.json", env.window.id())),
        format!(
            "{{\"pid\":{},\"sessionId\":\"{SESSION}\"}}",
            env.window.id()
        ),
    )
    .unwrap();
    let before = guard::file_identity(&env.transcript()).unwrap();
    env.hook("start", &env.input_with_source("startup"));

    assert!(!copy.exists());
    assert_eq!(
        std::fs::read_to_string(env.transcript()).unwrap(),
        pc_a() + &pc_b_turns()
    );
    assert!(before.same_file(&guard::file_identity(&env.transcript()).unwrap()));
    assert_eq!(
        std::fs::read_to_string(&history).unwrap(),
        "{\"timestamp\":1,\"display\":\"a\"}\n{\"timestamp\":2,\"display\":\"b\"}\n"
    );
    assert!(other_copy.exists());
    // The window was live on a file that moved on: its prompt is blocked.
    assert!(stdout(&env.hook("prompt", &env.input())).contains("block"));

    // `guard status` counts what is left; `guard sweep` settles it.
    let status = env.run(&["guard", "status"]);
    assert!(status.status.success());
    let text = stdout(&status);
    assert!(text.contains("Windows recorded: 1"), "{text}");
    assert!(text.contains("Conflict copies waiting: 1"), "{text}");
    let swept = env.run(&["guard", "sweep"]);
    assert!(swept.status.success(), "{}", stdout(&swept));
    assert!(!other_copy.exists());
    assert_eq!(
        std::fs::read_to_string(elsewhere.join(format!("{SESSION}.jsonl"))).unwrap(),
        pc_a() + &pc_b_turns()
    );
}

#[test]
fn every_hook_exits_zero_and_says_nothing_on_bad_input() {
    let env = Env::new();
    for event in ["prompt", "start", "stop", "end"] {
        for input in ["", "not json", "{}", "{\"session_id\":\"x\"}"] {
            let output = env.hook(event, input);
            assert!(output.status.success(), "{event} {input:?}");
            assert_eq!(stdout(&output), "", "{event} {input:?}");
        }
    }
    // A transcript that does not exist (yet) is no reason to complain.
    let output = env.hook("prompt", &env.input());
    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}

#[test]
fn install_status_and_uninstall_edit_settings_json_without_a_repository() {
    let env = Env::new();
    let settings = env.claude().join("settings.json");
    std::fs::write(
        &settings,
        "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"Stop\": [ { \"hooks\": [ { \"type\": \"command\", \"command\": \"/bin/mine\" } ] } ]\n  }\n}\n",
    )
    .unwrap();
    let before = std::fs::read_to_string(&settings).unwrap();

    let dry = env.run(&["guard", "install", "--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(stdout(&dry).contains("cleanupPeriodDays"));
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);

    let installed = env.run(&["guard", "install"]);
    assert!(installed.status.success());
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(value["cleanupPeriodDays"], 3650);
    assert_eq!(value["model"], "opus");
    assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert_eq!(
        value["hooks"]["SessionEnd"][0]["hooks"][0]["args"],
        serde_json::json!(["guard", "hook", "end"])
    );
    // No sync repository or config was needed, or made.
    assert!(!env
        .tmp
        .path()
        .join("cfg/claude-code-sync/state.json")
        .exists());

    let status = env.run(&["guard", "status"]);
    assert!(status.status.success());
    let text = stdout(&status);
    assert!(text.contains("UserPromptSubmit hook"), "{text}");
    assert!(!text.contains("not installed"), "{text}");
    assert!(text.contains("3650 days"), "{text}");

    let removed = env.run(&["guard", "uninstall"]);
    assert!(removed.status.success());
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert!(value["hooks"].get("SessionStart").is_none());
    assert_eq!(value["cleanupPeriodDays"], 3650);
}

#[test]
fn install_refuses_while_the_autosync_hooks_are_there() {
    let env = Env::new();
    let settings = env.claude().join("settings.json");
    let autosync = "{ \"hooks\": { \"Stop\": [ { \"hooks\": [ { \"type\": \"command\", \
        \"command\": \"/opt/claude-code-sync\", \"args\": [\"hook\", \"stop\"] } ] } ] } }\n";
    std::fs::write(&settings, autosync).unwrap();
    let output = env.run(&["guard", "install"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("autosync uninstall"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), autosync);
}
