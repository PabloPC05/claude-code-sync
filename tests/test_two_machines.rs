//! Two machines, one remote: a laptop with a macOS-style home and a desktop
//! with a Linux-style one, each mapping its own projects folder to the root
//! `proyectos`. Sessions created on one land in the other's correctly encoded
//! directories; a session continued on one fast-forwards on the other; a
//! session continued on both splits into the same session and parallel
//! branch on both, after which a sync changes nothing.
//!
//! Serialized: the environment overrides are process-global.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use claude_code_sync::branches::branch_session_id;
use claude_code_sync::filter::FilterConfig;
use claude_code_sync::project_map::encode_project_path;
use claude_code_sync::sync::{init_sync_repo, sync_bidirectional};
use claude_code_sync::VerbosityLevel;
use serial_test::serial;
use tempfile::TempDir;

const JARVIS: &str = "3d6f8a2b-1c4e-4f7a-9b2d-5e8c1a3f6b90";
const ACME: &str = "9a1b2c3d-4e5f-4a6b-8c7d-0e1f2a3b4c5d";
const LONG: &str = "c0ffee00-1234-4abc-8def-0123456789ab";

/// A long folder name: the project's encoded directory is cut at 200
/// characters and hashed, differently on each machine.
const LONG_NAME: &str = "un-proyecto-con-un-nombre-largo-de-verdad-para-pasar-de-los-doscientos-\
                         caracteres-de-la-codificacion-de-claude-code";

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

#[test]
#[serial]
fn two_machines_with_different_homes_share_sessions_and_converge_after_diverging() {
    let _env = EnvRestore::capture();
    let tmp = TempDir::new().unwrap();
    let remote = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &["init", "--bare", "--quiet", remote.to_str().unwrap()],
    );

    let laptop = Machine::new(tmp.path(), "A", "Users/mateo", &remote);
    let desktop = Machine::new(tmp.path(), "B", "home/mateo", &remote);

    // --- Sessions created on the laptop reach the desktop ----------------
    let long_project = format!("clientes/{LONG_NAME}");
    for (project, session) in [
        ("jarvis", JARVIS),
        ("clientes/acme", ACME),
        (long_project.as_str(), LONG),
    ] {
        let cwd = laptop.cwd(project);
        laptop.append(
            project,
            session,
            &[
                message(session, &cwd, &format!("{session}-p1"), None, 0, "hola"),
                message(
                    session,
                    &cwd,
                    &format!("{session}-p2"),
                    Some(&format!("{session}-p1")),
                    1,
                    "sigue",
                ),
                last_prompt(session, &format!("{session}-p2")),
            ],
        );
    }
    let sidecar = laptop
        .project_dir("jarvis")
        .join(JARVIS)
        .join("tool-results/r1.txt");
    fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    fs::write(&sidecar, "resultado grande").unwrap();
    assert!(
        laptop.project_dir(&long_project).file_name().unwrap().len() > 200,
        "the long project's directory is hashed"
    );

    laptop.sync();
    desktop.sync();

    let repo_projects = desktop.repo.join("projects");
    assert!(repo_projects.join("@proyectos@jarvis").is_dir());
    assert!(repo_projects.join("@proyectos@clientes%2Facme").is_dir());
    for (project, session) in [
        ("jarvis", JARVIS),
        ("clientes/acme", ACME),
        (long_project.as_str(), LONG),
    ] {
        assert_eq!(
            fs::read(desktop.transcript(project, session)).unwrap(),
            fs::read(laptop.transcript(project, session)).unwrap(),
            "{project} lands in the desktop's own encoded directory"
        );
    }
    assert!(desktop
        .project_dir("jarvis")
        .to_string_lossy()
        .contains("-home-mateo-proyectos-jarvis"));
    assert_eq!(
        fs::read_to_string(
            desktop
                .project_dir("jarvis")
                .join(JARVIS)
                .join("tool-results/r1.txt")
        )
        .unwrap(),
        "resultado grande",
        "a session's sidecar files travel under the same project"
    );

    // --- Continued on the desktop: the laptop fast-forwards ---------------
    let desktop_cwd = desktop.cwd("jarvis");
    desktop.append(
        "jarvis",
        JARVIS,
        &[
            message(
                JARVIS,
                &desktop_cwd,
                "b1",
                Some(&format!("{JARVIS}-p2")),
                3,
                "desde el sobremesa",
            ),
            last_prompt(JARVIS, "b1"),
        ],
    );
    desktop.sync();
    laptop.sync();
    assert_eq!(
        fs::read(laptop.transcript("jarvis", JARVIS)).unwrap(),
        fs::read(desktop.transcript("jarvis", JARVIS)).unwrap(),
        "the laptop took the desktop's longer copy as it was"
    );
    assert_eq!(
        laptop.transcripts("jarvis").len(),
        1,
        "no branch for a fast-forward"
    );

    // --- Continued on both: one session and one parallel branch, on both ---
    let laptop_cwd = laptop.cwd("jarvis");
    laptop.append(
        "jarvis",
        JARVIS,
        &[
            message(
                JARVIS,
                &laptop_cwd,
                "a2",
                Some("b1"),
                10,
                "rama del portatil",
            ),
            last_prompt(JARVIS, "a2"),
        ],
    );
    desktop.append(
        "jarvis",
        JARVIS,
        &[
            message(
                JARVIS,
                &desktop_cwd,
                "b2",
                Some("b1"),
                5,
                "rama del sobremesa",
            ),
            last_prompt(JARVIS, "b2"),
        ],
    );
    for _ in 0..2 {
        laptop.sync();
        desktop.sync();
    }

    let on_laptop = laptop.transcripts("jarvis");
    let on_desktop = desktop.transcripts("jarvis");
    assert_eq!(on_laptop, on_desktop, "both machines hold the same files");
    let branch = branch_session_id(JARVIS, "a2");
    assert_eq!(on_laptop.keys().cloned().collect::<Vec<_>>(), {
        let mut names = vec![format!("{JARVIS}.jsonl"), format!("{branch}.jsonl")];
        names.sort();
        names
    });
    let session = String::from_utf8(on_laptop[&format!("{JARVIS}.jsonl")].clone()).unwrap();
    let parallel = String::from_utf8(on_laptop[&format!("{branch}.jsonl")].clone()).unwrap();
    assert!(session.contains("rama del sobremesa") && !session.contains("rama del portatil"));
    assert!(parallel.contains("rama del portatil") && !parallel.contains("rama del sobremesa"));
    assert!(!parallel.contains(&format!("\"sessionId\":\"{JARVIS}\"")));
    assert!(parallel.ends_with(&format!(
        "{{\"type\":\"custom-title\",\"customTitle\":\"hola (rama paralela)\",\"sessionId\":\"{branch}\"}}\n"
    )));
    assert!(
        desktop
            .project_dir("jarvis")
            .join(&branch)
            .join("tool-results/r1.txt")
            .is_file(),
        "the branch has the session's sidecar too"
    );

    // Each project is one directory in the repository and on each machine:
    // nothing was pushed back under a machine's own encoded name.
    let mut repo_dirs: Vec<String> = fs::read_dir(desktop.repo.join("projects"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    repo_dirs.sort();
    let mut expected = vec![
        "@proyectos@jarvis".to_string(),
        "@proyectos@clientes%2Facme".to_string(),
        format!("@proyectos@clientes%2F{LONG_NAME}"),
    ];
    expected.sort();
    assert_eq!(repo_dirs, expected);
    for machine in [&laptop, &desktop] {
        let mut local: Vec<PathBuf> = fs::read_dir(machine.claude.join("projects"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        local.sort();
        let mut wanted = vec![
            machine.project_dir("jarvis"),
            machine.project_dir("clientes/acme"),
            machine.project_dir(&long_project),
        ];
        wanted.sort();
        assert_eq!(local, wanted, "no stray directories");
    }

    // --- And then a sync changes nothing anywhere -------------------------
    let remote_head = git(&remote, &["rev-parse", "HEAD"]);
    let laptop_before = laptop.snapshot();
    let desktop_before = desktop.snapshot();
    laptop.sync();
    desktop.sync();
    assert_eq!(
        laptop.snapshot(),
        laptop_before,
        "nothing changes on the laptop"
    );
    assert_eq!(
        desktop.snapshot(),
        desktop_before,
        "nothing changes on the desktop"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "HEAD"]),
        remote_head,
        "nothing new to push"
    );
}
