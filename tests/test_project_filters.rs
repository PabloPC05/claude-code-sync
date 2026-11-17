//! Include and exclude patterns select whole projects: a session's sidecar
//! directory (tool results, subagents) and a project's memory travel exactly
//! when the transcripts they belong to do, on push and on pull.
//!
//! Serialized: the environment overrides are process-global.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use claude_code_sync::filter::FilterConfig;
use claude_code_sync::sync::{pull_history, push_history, SyncState};
use claude_code_sync::VerbosityLevel;
use serial_test::serial;
use tempfile::TempDir;

const KEPT_SESSION: &str = "0b9a8c3e-58f4-4b47-9d43-3a1f6f2d7e10";
const OTHER_SESSION: &str = "7f3c2a1d-1e0b-4c55-8d2e-9a6b5c4d3e21";

/// One simulated machine: its own `.claude`, config dir and home, sharing a
/// sync repository.
struct Machine {
    _root: TempDir,
    claude: PathBuf,
    config: PathBuf,
    home: PathBuf,
}

impl Machine {
    fn new(repo: &Path, filter: &FilterConfig) -> Machine {
        let root = TempDir::new().unwrap();
        let claude = root.path().join("home/.claude");
        let config = root.path().join("cfg");
        let home = root.path().join("home");
        fs::create_dir_all(claude.join("projects")).unwrap();
        fs::create_dir_all(config.join("claude-code-sync")).unwrap();
        let state = SyncState {
            sync_repo_path: repo.to_path_buf(),
            has_remote: false,
            is_cloned_repo: false,
        };
        fs::write(
            config.join("claude-code-sync/state.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        fs::write(
            config.join("claude-code-sync/config.toml"),
            toml::to_string_pretty(filter).unwrap(),
        )
        .unwrap();
        Machine {
            _root: root,
            claude,
            config,
            home,
        }
    }

    fn activate(&self) {
        std::env::set_var("CLAUDE_CODE_SYNC_CLAUDE_DIR", &self.claude);
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", &self.config);
        std::env::set_var("CLAUDE_CODE_SYNC_HOME", &self.home);
    }

    fn projects(&self) -> PathBuf {
        self.claude.join("projects")
    }

    /// A project with one session, its sidecar directory and a memory file.
    fn seed_project(&self, encoded: &str, cwd: &str, session: &str) {
        let project = self.projects().join(encoded);
        fs::create_dir_all(project.join(session).join("tool-results")).unwrap();
        fs::create_dir_all(project.join(session).join("subagents")).unwrap();
        fs::create_dir_all(project.join("memory")).unwrap();
        fs::write(
            project.join(format!("{session}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u-{session}\",\"sessionId\":\"{session}\",\
                 \"timestamp\":\"2026-11-17T10:00:00Z\",\"cwd\":\"{cwd}\"}}\n"
            ),
        )
        .unwrap();
        fs::write(
            project.join(session).join("tool-results/result.txt"),
            "big output",
        )
        .unwrap();
        fs::write(
            project.join(session).join("subagents/agent-a1.jsonl"),
            format!(
                "{{\"type\":\"user\",\"uuid\":\"a-{session}\",\"sessionId\":\"{session}\",\
                 \"timestamp\":\"2026-11-17T10:01:00Z\",\"cwd\":\"{cwd}\"}}\n"
            ),
        )
        .unwrap();
        fs::write(project.join("memory/MEMORY.md"), "# notes\n").unwrap();
    }
}

struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl EnvRestore {
    fn capture() -> Self {
        EnvRestore(
            [
                "CLAUDE_CODE_SYNC_CLAUDE_DIR",
                "CLAUDE_CODE_SYNC_CONFIG_DIR",
                "CLAUDE_CODE_SYNC_HOME",
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

fn new_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    claude_code_sync::scm::init(repo.path()).unwrap();
    repo
}

fn files_under(dir: &Path) -> Vec<String> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn push() {
    push_history(
        None,
        false,
        None,
        false,
        false,
        VerbosityLevel::Quiet,
        &HashSet::new(),
        &[],
    )
    .unwrap();
}

fn only_kept() -> FilterConfig {
    FilterConfig {
        include_patterns: vec!["*-w-kept*".to_string()],
        ..Default::default()
    }
}

#[test]
#[serial]
fn a_push_leaves_every_file_of_a_project_the_patterns_leave_out() {
    let _env = EnvRestore::capture();
    let repo = new_repo();
    let machine = Machine::new(repo.path(), &only_kept());
    machine.activate();
    machine.seed_project("-w-kept", "/w/kept", KEPT_SESSION);
    machine.seed_project("-w-other", "/w/other", OTHER_SESSION);

    push();

    let pushed = files_under(&repo.path().join("projects"));
    assert!(
        pushed.iter().all(|f| !f.contains("-w-other")),
        "nothing of the excluded project reaches the repository: {pushed:#?}"
    );
    for expected in [
        format!("-w-kept/{KEPT_SESSION}.jsonl"),
        format!("-w-kept/{KEPT_SESSION}/tool-results/result.txt"),
        format!("-w-kept/{KEPT_SESSION}/subagents/agent-a1.jsonl"),
        "-w-kept/memory/MEMORY.md".to_string(),
    ] {
        assert!(
            pushed.contains(&expected.replace('/', std::path::MAIN_SEPARATOR_STR)),
            "{expected} travels with its project: {pushed:#?}"
        );
    }
}

#[test]
#[serial]
fn a_pull_takes_only_the_projects_the_patterns_select() {
    let _env = EnvRestore::capture();
    let repo = new_repo();

    // Another machine, without patterns, publishes both projects.
    let other = Machine::new(repo.path(), &FilterConfig::default());
    other.activate();
    other.seed_project("-w-kept", "/w/kept", KEPT_SESSION);
    other.seed_project("-w-other", "/w/other", OTHER_SESSION);
    push();
    assert!(files_under(&repo.path().join("projects"))
        .iter()
        .any(|f| f.contains("-w-other")));

    let this = Machine::new(repo.path(), &only_kept());
    this.activate();
    pull_history(false, None, false, VerbosityLevel::Quiet, false).unwrap();

    let pulled = files_under(&this.projects());
    assert!(
        !this.projects().join("-w-other").exists(),
        "nothing of the excluded project is pulled: {pulled:#?}"
    );
    assert!(this
        .projects()
        .join(format!("-w-kept/{KEPT_SESSION}.jsonl"))
        .is_file());
    assert!(this
        .projects()
        .join(format!("-w-kept/{KEPT_SESSION}/tool-results/result.txt"))
        .is_file());
    assert!(this.projects().join("-w-kept/memory/MEMORY.md").is_file());
}
