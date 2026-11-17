//! A chat left open on one PC while the other continues it: the pull appends
//! the other PC's turns to the open chat's file (never rewriting it), marks
//! the window as out of date, the prompt guard blocks it, `/resume` (the
//! SessionStart hook) clears the mark, and what a stale window appends when
//! it closes is taken out again.
//!
//! Serialized: the environment overrides are process-global.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use claude_code_sync::branches::branch_session_id;
use claude_code_sync::filter::FilterConfig;
use claude_code_sync::project_map::encode_project_path;
use claude_code_sync::sync::{init_sync_repo, sync_bidirectional};
use claude_code_sync::{hooks, stale, VerbosityLevel};
use serial_test::serial;
use tempfile::TempDir;

struct Machine {
    claude: PathBuf,
    config: PathBuf,
    home: PathBuf,
    repo: PathBuf,
}

impl Machine {
    fn new(root: &Path, name: &str, home: &str, remote: &Path) -> Machine {
        let machine_root = root.join(name);
        let home = machine_root.join(home);
        let machine = Machine {
            claude: home.join(".claude"),
            config: machine_root.join("cfg"),
            repo: machine_root.join("sync-repo"),
            home,
        };
        fs::create_dir_all(machine.claude.join("projects")).unwrap();
        fs::create_dir_all(machine.proyectos()).unwrap();
        machine.activate();
        init_sync_repo(&machine.repo, Some(remote.to_str().unwrap())).unwrap();
        let mut filter = FilterConfig::default();
        filter
            .root_map
            .insert("proyectos".to_string(), machine.proyectos());
        filter.save().unwrap();
        machine
    }

    fn activate(&self) {
        std::env::set_var("HOME", &self.home);
        std::env::set_var("CLAUDE_CODE_SYNC_HOME", &self.home);
        std::env::set_var("CLAUDE_CODE_SYNC_CLAUDE_DIR", &self.claude);
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", &self.config);
    }

    fn proyectos(&self) -> PathBuf {
        self.home.join("proyectos")
    }

    /// Where Claude Code keeps the transcripts of the project at `relative`.
    fn project_dir(&self, relative: &str) -> PathBuf {
        let mut path = self.proyectos();
        for part in relative.split('/') {
            path.push(part);
        }
        self.claude
            .join("projects")
            .join(encode_project_path(&path))
    }

    fn transcript(&self, relative: &str, session: &str) -> PathBuf {
        self.project_dir(relative).join(format!("{session}.jsonl"))
    }

    fn cwd(&self, relative: &str) -> String {
        let mut path = self.proyectos();
        for part in relative.split('/') {
            path.push(part);
        }
        path.to_string_lossy().into_owned()
    }

    fn sync(&self) {
        self.activate();
        sync_bidirectional(None, None, false, false, VerbosityLevel::Quiet).unwrap();
    }

    fn append(&self, relative: &str, session: &str, lines: &[String]) {
        let path = self.transcript(relative, session);
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        for line in lines {
            text.push_str(line);
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Every file under `~/.claude/projects`, with its bytes.
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let projects = self.claude.join("projects");
        walkdir::WalkDir::new(&projects)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                (
                    e.path().strip_prefix(&projects).unwrap().to_path_buf(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect()
    }

    /// The transcripts of one project: file name -> bytes.
    fn transcripts(&self, relative: &str) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(self.project_dir(relative))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect()
    }
}

fn message(
    session: &str,
    cwd: &str,
    uuid: &str,
    parent: Option<&str>,
    minute: u32,
    text: &str,
) -> String {
    let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
    let cwd = serde_json::to_string(cwd).unwrap();
    format!(
        "{{\"parentUuid\":{parent},\"isSidechain\":false,\"type\":\"user\",\
         \"message\":{{\"role\":\"user\",\"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\
         \"timestamp\":\"2026-11-17T10:{minute:02}:00.000Z\",\"cwd\":{cwd},\
         \"sessionId\":\"{session}\",\"version\":\"2.1.326\"}}\n"
    )
}

fn last_prompt(session: &str, leaf: &str) -> String {
    format!("{{\"type\":\"last-prompt\",\"leafUuid\":\"{leaf}\",\"sessionId\":\"{session}\"}}\n")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl EnvRestore {
    fn capture() -> Self {
        EnvRestore(
            [
                "HOME",
                "CLAUDE_CODE_SYNC_HOME",
                "CLAUDE_CODE_SYNC_CLAUDE_DIR",
                "CLAUDE_CODE_SYNC_CONFIG_DIR",
                "CLAUDE_CODE_SYNC_HOOK_PID",
            ]
            .into_iter()
            .map(|name| (name, std::env::var(name).ok()))
            .collect(),
        )
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

const SESSION: &str = "3d6f8a2b-1c4e-4f7a-9b2d-5e8c1a3f6b90";
const PROJECT: &str = "jarvis";
const REPO_PROJECT: &str = "@proyectos@jarvis";

fn assistant(session: &str, uuid: &str, parent: &str, minute: u32, text: &str) -> String {
    format!(
        "{{\"parentUuid\":\"{parent}\",\"isSidechain\":false,\"type\":\"assistant\",\
         \"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}},\
         \"uuid\":\"{uuid}\",\"timestamp\":\"2026-11-17T10:{minute:02}:00.000Z\",\
         \"sessionId\":\"{session}\"}}\n"
    )
}

/// What `/exit` appends in the window that closes: three user entries off
/// its own leaf, and a `last-prompt` naming it.
fn exit_noise(session: &str, leaf: &str) -> Vec<String> {
    let user = |uuid: &str, parent: &str, meta: bool, text: &str| {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"isSidechain\":false,\"isMeta\":{meta},\"type\":\"user\",\
             \"message\":{{\"role\":\"user\",\"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\
             \"timestamp\":\"2026-11-17T11:00:00.000Z\",\"sessionId\":\"{session}\"}}\n"
        )
    };
    vec![
        user(
            "x1",
            leaf,
            true,
            "<local-command-caveat>Caveat: the messages below...",
        ),
        user("x2", "x1", false, "<command-name>/exit</command-name>"),
        user(
            "x3",
            "x2",
            false,
            "<local-command-stdout>Goodbye!</local-command-stdout>",
        ),
        last_prompt(session, leaf),
    ]
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

/// A chat open on the laptop (its window is `window`), idle, while the
/// desktop continued it by two entries, `r1` and `r2`, and pushed.
struct Scene {
    _tmp: TempDir,
    _env: EnvRestore,
    remote: PathBuf,
    laptop: Machine,
    desktop: Machine,
    window: Child,
}

impl Scene {
    /// `initial`: the chat's lines on both PCs before; its leaf is `leaf`.
    fn new(initial: impl Fn(&str) -> Vec<String>, leaf: &str) -> Scene {
        let env = EnvRestore::capture();
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &["init", "--bare", "--quiet", remote.to_str().unwrap()],
        );
        let laptop = Machine::new(tmp.path(), "A", "Users/mateo", &remote);
        let desktop = Machine::new(tmp.path(), "B", "home/mateo", &remote);

        laptop.append(PROJECT, SESSION, &initial(&laptop.cwd(PROJECT)));
        laptop.sync();
        desktop.sync();

        // The window on the laptop: its record in ~/.claude/sessions.
        let window = long_child();
        let sessions = laptop.claude.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join(format!("{}.json", window.id())),
            format!(
                "{{\"pid\":{},\"sessionId\":\"{SESSION}\",\"status\":\"idle\"}}",
                window.id()
            ),
        )
        .unwrap();

        let cwd = desktop.cwd(PROJECT);
        desktop.append(
            PROJECT,
            SESSION,
            &[
                message(SESSION, &cwd, "r1", Some(leaf), 20, "sigo en el otro PC"),
                assistant(SESSION, "r2", "r1", 21, "claro, seguimos"),
                last_prompt(SESSION, "r2"),
            ],
        );
        desktop.sync();
        Scene {
            _tmp: tmp,
            _env: env,
            remote,
            laptop,
            desktop,
            window,
        }
    }

    fn remote_head(&self) -> String {
        git(&self.remote, &["rev-parse", "HEAD"])
    }

    /// The window closes: what Claude Code does (the transcript gets
    /// `appended`), then the SessionEnd hook, then the process ends.
    fn close_window(mut self, appended: &[String]) -> Scene {
        self.laptop.append(PROJECT, SESSION, appended);
        self.laptop.activate();
        fs::remove_file(
            self.laptop
                .claude
                .join("sessions")
                .join(format!("{}.json", self.window.id())),
        )
        .unwrap();
        stale::ended(self.window.id()).unwrap();
        self.window.kill().ok();
        self.window.wait().ok();
        self
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        self.window.kill().ok();
        self.window.wait().ok();
    }
}

fn initial(cwd: &str) -> Vec<String> {
    vec![
        message(SESSION, cwd, "p1", None, 0, "hola, revisa el parser"),
        assistant(SESSION, "p2", "p1", 1, "vale"),
        last_prompt(SESSION, "p2"),
    ]
}

/// The uuid of the last user or assistant entry of a transcript.
fn newest_message(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| matches!(v["type"].as_str(), Some("user" | "assistant")))
        .filter_map(|v| v["uuid"].as_str().map(str::to_string))
        .next_back()
}

fn hook_input() -> String {
    format!("{{\"session_id\":\"{SESSION}\",\"hook_event_name\":\"UserPromptSubmit\"}}")
}

#[test]
#[serial]
fn an_open_idle_chat_receives_the_other_pcs_turns_by_appending() {
    let s = Scene::new(initial, "p2");
    let path = s.laptop.transcript(PROJECT, SESSION);
    let before = fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&before).contains("sigo en el otro PC"));

    s.laptop.sync();

    let after = fs::read(&path).unwrap();
    assert!(
        after.starts_with(&before),
        "the file's prefix is byte-identical: it was appended to"
    );
    let appended = String::from_utf8_lossy(&after[before.len()..]).into_owned();
    assert!(appended.contains("sigo en el otro PC") && appended.contains("claro, seguimos"));
    assert_eq!(newest_message(&after).as_deref(), Some("r2"));
    assert_eq!(
        s.laptop.transcripts(PROJECT).len(),
        1,
        "no other file, nothing renamed"
    );

    // The window is marked out of date: where it was, and where the chat is.
    s.laptop.activate();
    let marker = stale::stale_for(SESSION, &[s.window.id()]).expect("a marker for the window");
    assert_eq!(marker.fork_leaf, "p2");
    assert_eq!(marker.remote_tip, "r2");
    assert_eq!(marker.pids, vec![s.window.id()]);
    assert_eq!(marker.transcript, std::path::absolute(&path).unwrap());

    // Nothing more to do, and nothing to publish.
    let head = s.remote_head();
    let snapshot = s.laptop.snapshot();
    s.laptop.sync();
    s.desktop.sync();
    assert_eq!(s.laptop.snapshot(), snapshot);
    assert_eq!(s.remote_head(), head);
    assert_eq!(
        fs::read(s.desktop.transcript(PROJECT, SESSION)).unwrap(),
        fs::read(s.laptop.transcript(PROJECT, SESSION)).unwrap(),
        "both PCs hold the same chat"
    );
}

#[test]
#[serial]
fn the_guard_blocks_the_stale_window_until_it_resumes() {
    let s = Scene::new(initial, "p2");
    s.laptop.sync();
    s.laptop.activate();

    // The hook of the stale window (its pid is the hook's parent).
    std::env::set_var("CLAUDE_CODE_SYNC_HOOK_PID", s.window.id().to_string());
    let ancestors = stale::claude_ancestors();
    assert_eq!(ancestors, vec![s.window.id()]);
    let reason = hooks::prompt_verdict(&hook_input(), &ancestors).expect("blocked");
    let json: serde_json::Value = serde_json::from_str(&hooks::block_json(&reason)).unwrap();
    assert_eq!(json["decision"], "block");
    assert!(json["reason"]
        .as_str()
        .unwrap()
        .contains(&format!("/resume {SESSION}")));

    // Another window, or another chat, is not.
    assert_eq!(hooks::prompt_verdict(&hook_input(), &[1]), None);
    assert_eq!(
        hooks::prompt_verdict("{\"session_id\":\"otro-chat\"}", &ancestors),
        None
    );
    assert_eq!(hooks::prompt_verdict("no json", &ancestors), None);

    // `/resume` reloads the chat: SessionStart clears the marker.
    for pid in &ancestors {
        stale::refresh(*pid).unwrap();
    }
    assert_eq!(hooks::prompt_verdict(&hook_input(), &ancestors), None);
    assert!(!stale::any_marker());

    // Closing it now leaves the file as it is.
    let s = s.close_window(&[]);
    let after = fs::read(s.laptop.transcript(PROJECT, SESSION)).unwrap();
    assert_eq!(newest_message(&after).as_deref(), Some("r2"));
}

#[test]
#[serial]
fn closing_the_stale_window_makes_the_other_pcs_branch_the_newest_again() {
    let s = Scene::new(initial, "p2");
    s.laptop.sync();
    let head = s.remote_head();
    let path = s.laptop.transcript(PROJECT, SESSION);

    // `/exit` in the old window: noise off the old leaf, after the turns.
    let s = s.close_window(&exit_noise(SESSION, "p2"));
    let repaired = fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&repaired);
    assert!(!text.contains("/exit") && !text.contains("local-command"));
    assert_eq!(
        newest_message(&repaired).as_deref(),
        Some("r2"),
        "a `claude --continue` shows the other PC's turns"
    );
    assert!(!stale::any_marker());

    for _ in 0..2 {
        s.laptop.sync();
        s.desktop.sync();
    }
    assert_eq!(
        fs::read(&path).unwrap(),
        fs::read(s.desktop.transcript(PROJECT, SESSION)).unwrap()
    );
    assert_eq!(
        newest_message(&fs::read(&path).unwrap()).as_deref(),
        Some("r2")
    );
    assert_eq!(
        s.remote_head(),
        head,
        "nothing to push: the noise never left"
    );
    assert_eq!(s.laptop.transcripts(PROJECT).len(), 1);
}

#[test]
#[serial]
fn a_window_closed_without_saying_is_repaired_at_the_next_sync() {
    let s = Scene::new(initial, "p2");
    s.laptop.sync();
    let path = s.laptop.transcript(PROJECT, SESSION);

    // The terminal is killed: no SessionEnd, only the noise (and the pid
    // gone).
    let mut s = s;
    s.laptop
        .append(PROJECT, SESSION, &exit_noise(SESSION, "p2"));
    s.window.kill().ok();
    s.window.wait().ok();
    assert!(String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("/exit"));

    s.laptop.sync();
    let text = fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&text).contains("/exit"));
    assert_eq!(newest_message(&text).as_deref(), Some("r2"));
}

#[test]
#[serial]
fn what_a_stale_window_really_wrote_becomes_a_parallel_session_on_both_pcs() {
    let s = Scene::new(initial, "p2");
    s.laptop.sync();
    let cwd = s.laptop.cwd(PROJECT);

    // Typed in the old window (the hook was not there to stop it), then `/exit`.
    let mut closing = vec![
        message(
            SESSION,
            &cwd,
            "s1",
            Some("p2"),
            30,
            "escrito en la ventana vieja",
        ),
        assistant(SESSION, "s2", "s1", 31, "respuesta desde la memoria vieja"),
        last_prompt(SESSION, "s2"),
    ];
    closing.extend(exit_noise(SESSION, "s2"));
    let s = s.close_window(&closing);

    let branch = branch_session_id(SESSION, "s1");
    let on_laptop = s.laptop.transcripts(PROJECT);
    let main = String::from_utf8_lossy(&on_laptop[&format!("{SESSION}.jsonl")]).into_owned();
    let parallel = String::from_utf8_lossy(&on_laptop[&format!("{branch}.jsonl")]).into_owned();
    assert!(main.contains("sigo en el otro PC") && !main.contains("ventana vieja"));
    assert_eq!(
        newest_message(main.as_bytes()).as_deref(),
        Some("r2"),
        "the session resumes on the other PC's turns"
    );
    assert!(parallel.contains("ventana vieja") && !parallel.contains("sigo en el otro PC"));
    assert!(parallel.contains("(rama paralela)"));

    // Both PCs end with the same two sessions, and then nothing changes.
    for _ in 0..2 {
        s.laptop.sync();
        s.desktop.sync();
    }
    let on_desktop = s.desktop.transcripts(PROJECT);
    assert_eq!(s.laptop.transcripts(PROJECT), on_desktop);
    assert_eq!(on_desktop.len(), 2, "the session and its parallel branch");
    assert!(
        String::from_utf8_lossy(&on_desktop[&format!("{branch}.jsonl")])
            .contains("(rama paralela)")
    );
    assert!(
        remote_file(&s.remote, REPO_PROJECT, &format!("{branch}.jsonl")).contains("ventana vieja"),
        "the branch was pushed"
    );
    let head = s.remote_head();
    s.laptop.sync();
    s.desktop.sync();
    assert_eq!(s.remote_head(), head);
}

#[test]
#[serial]
fn a_rewind_branch_is_left_untouched_by_the_repair() {
    // The chat has a rewind: w1 is a second child of p1.
    let with_rewind = |cwd: &str| {
        vec![
            message(SESSION, cwd, "p1", None, 0, "hola, revisa el parser"),
            message(SESSION, cwd, "w1", Some("p1"), 1, "pregunta reescrita"),
            assistant(SESSION, "w2", "w1", 2, "respuesta a la reescrita"),
            assistant(SESSION, "p2", "p1", 3, "vale"),
            message(SESSION, cwd, "p3", Some("p2"), 4, "sigue"),
            last_prompt(SESSION, "p3"),
        ]
    };
    let s = Scene::new(with_rewind, "p3");
    s.laptop.sync();
    let s = s.close_window(&exit_noise(SESSION, "p3"));

    let text = fs::read_to_string(s.laptop.transcript(PROJECT, SESSION)).unwrap();
    for uuid in ["p1", "w1", "w2", "p2", "p3", "r1", "r2"] {
        assert!(
            text.contains(&format!("\"uuid\":\"{uuid}\"")),
            "{uuid} stays"
        );
    }
    assert!(!text.contains("/exit"));
    assert_eq!(newest_message(text.as_bytes()).as_deref(), Some("r2"));
    assert_eq!(s.laptop.transcripts(PROJECT).len(), 1, "no branch is made");
}

#[test]
#[serial]
fn a_chat_both_pcs_continued_is_still_deferred_while_open() {
    // The laptop's window did write: the chat diverged, so it is not appended to.
    let s = Scene::new(initial, "p2");
    let cwd = s.laptop.cwd(PROJECT);
    s.laptop.append(
        PROJECT,
        SESSION,
        &[
            message(
                SESSION,
                &cwd,
                "a1",
                Some("p2"),
                40,
                "turno de la ventana abierta",
            ),
            last_prompt(SESSION, "a1"),
        ],
    );
    let before = fs::read(s.laptop.transcript(PROJECT, SESSION)).unwrap();
    s.laptop.sync();
    assert_eq!(
        fs::read(s.laptop.transcript(PROJECT, SESSION)).unwrap(),
        before,
        "untouched"
    );
    s.laptop.activate();
    assert!(!stale::any_marker(), "no marker: nothing was appended");
}

/// A file of the repository, read from the bare remote.
fn remote_file(remote: &Path, project: &str, name: &str) -> String {
    let output = Command::new("git")
        .args(["--git-dir", remote.to_str().unwrap()])
        .args(["show", &format!("HEAD:projects/{project}/{name}")])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
