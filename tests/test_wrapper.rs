//! The `claude` shell function, run by real shells.
//!
//! A fake `claude` (tests/support/fake_claude.rs, compiled with rustc) stands
//! first on PATH. On its first run it leaves the reload request the prompt
//! hook would leave and exits, as a window ended by the hook does; the
//! function must then start it again with `--resume <id> "<text>"` and the
//! exact text, whatever quotes, backslashes, newlines and non-ASCII it holds.
//!
//! Windows PowerShell 5.1 and PowerShell 7 on Windows, pwsh anywhere, bash
//! and zsh on Unix: each shell that is installed runs; the others are
//! skipped. Nothing here touches the real home or config directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use claude_code_sync::wrapper;
use serde_json::Value;

const SESSION: &str = "7a1f0c52-3b9e-4d64-8f21-0c5d9e7b1a33";

/// Spaces, double quotes, a backslash before a quote, a path that ends in a
/// backslash, a newline, a trailing backslash, ñ, é and an emoji.
const PROMPT: &str = "dice \"hola mundo\" y \\\"esto\\\", ruta C:\\mi carpeta\\ y \
                      fin\\\nsegunda línea: ñandú, café, 😀 \\";

const EXE: &str = env!("CARGO_BIN_EXE_claude-code-sync");

/// The fake `claude`, built once per test run.
fn fake_claude() -> &'static Path {
    static FAKE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &FAKE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let name = if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            };
            let out = dir.path().join(name);
            let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
            let status = Command::new(rustc)
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/support/fake_claude.rs"
                ))
                .args(["--edition", "2021", "-o"])
                .arg(&out)
                .status()
                .expect("rustc is needed to build the fake claude");
            assert!(status.success(), "the fake claude did not build");
            (dir, out)
        })
        .1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    /// Windows PowerShell 5.1
    PowerShell51,
    /// PowerShell 7+
    Pwsh,
    /// PowerShell 7+ forced to the legacy native-argument passing
    PwshLegacy,
}

impl Shell {
    fn program(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::PowerShell51 => "powershell",
            Shell::Pwsh | Shell::PwshLegacy => "pwsh",
        }
    }

    fn is_powershell(self) -> bool {
        !matches!(self, Shell::Bash | Shell::Zsh)
    }

    fn available(self) -> bool {
        // Legacy argument passing follows the Windows command-line rules only
        // on Windows: PowerShell on Unix splits its own string differently.
        if matches!(self, Shell::PowerShell51 | Shell::PwshLegacy) && !cfg!(windows) {
            return false;
        }
        if cfg!(windows) && !self.is_powershell() {
            // bash and zsh on Windows would be Git Bash, whose paths differ;
            // the profile installer does not target them there.
            return false;
        }
        let home = tempfile::tempdir().unwrap();
        Command::new(self.program())
            .env("HOME", home.path())
            .env("POWERSHELL_TELEMETRY_OPTOUT", "1")
            .arg(if self.is_powershell() {
                "-NoProfile"
            } else {
                "--version"
            })
            .args(if self.is_powershell() {
                vec!["-Command", "exit 0"]
            } else {
                vec![]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

struct Run {
    dir: tempfile::TempDir,
    stdout: String,
    stderr: String,
}

impl Run {
    fn log(&self) -> Vec<(Vec<String>, Option<String>)> {
        let text = std::fs::read_to_string(self.dir.path().join("log.jsonl")).unwrap_or_default();
        text.lines()
            .map(|line| {
                let value: Value = serde_json::from_str(line).unwrap();
                let args = value["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect();
                (args, value["shell"].as_str().map(str::to_string))
            })
            .collect()
    }
}

/// Run `claude <args>` in `shell` with the function installed. `first_run`
/// false means the fake has run before, so it asks for no reload.
fn run(shell: Shell, args: &str, always: bool, first_run: bool) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let template = serde_json::json!({
        "session_id": SESSION,
        "prompt": PROMPT,
        "cwd": root.to_string_lossy(),
        "created_at": "2026-11-17T10:00:00Z",
    });
    std::fs::write(root.join("request.json"), template.to_string()).unwrap();
    if !first_run {
        std::fs::write(root.join("state"), "ran").unwrap();
    }

    let exe = Path::new(EXE);
    let function = if shell.is_powershell() {
        wrapper::powershell_block(exe)
    } else {
        wrapper::posix_block(exe)
    };
    let mut command = Command::new(shell.program());
    let fake_dir = fake_claude().parent().unwrap().to_path_buf();
    if shell.is_powershell() {
        let prelude = if shell == Shell::PwshLegacy {
            "$PSNativeCommandArgumentPassing = 'Legacy'\n"
        } else {
            ""
        };
        let script =
            format!("{prelude}{function}\nclaude {args}\n\"after=[$env:CLAUDE_GUARD_SHELL]\"\n");
        // -EncodedCommand: UTF-16LE in base64, immune to quoting and code pages.
        use base64::Engine;
        let utf16: Vec<u8> = script
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    } else {
        let script =
            format!("{function}\nclaude {args}\necho \"after=[${{CLAUDE_GUARD_SHELL:-}}]\"\n");
        if shell == Shell::Zsh {
            command.arg("-f");
        } else {
            command.args(["--noprofile", "--norc"]);
        }
        command.args(["-c", &script]);
    }
    let path = std::env::join_paths(
        std::iter::once(fake_dir).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    command
        .env("PATH", path)
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", root.join("cfg"))
        .env("HOME", root.join("home"))
        .env("CLAUDE_CODE_SYNC_HOME", root.join("home"))
        .env("POWERSHELL_TELEMETRY_OPTOUT", "1")
        .env("FAKE_CLAUDE_LOG", root.join("log.jsonl"))
        .env("FAKE_CLAUDE_STATE", root.join("state"))
        .env("FAKE_CLAUDE_REQUEST_TEMPLATE", root.join("request.json"))
        .env(
            "FAKE_CLAUDE_REQUEST_DIR",
            root.join("cfg").join("claude-code-sync/guard/reload"),
        )
        .env_remove("CLAUDE_GUARD_SHELL")
        .env_remove("FAKE_CLAUDE_ALWAYS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if always {
        command.env("FAKE_CLAUDE_ALWAYS", "1");
    }
    let output = command.output().expect("the shell runs");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        dir,
    }
}

fn shells() -> Vec<Shell> {
    let found: Vec<Shell> = [
        Shell::Bash,
        Shell::Zsh,
        Shell::PowerShell51,
        Shell::Pwsh,
        Shell::PwshLegacy,
    ]
    .into_iter()
    .filter(|s| s.available())
    .collect();
    eprintln!("shells tested: {found:?}");
    if found.is_empty() {
        eprintln!("no shell to test the wrapper with");
    }
    found
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_reload_reopens_the_chat_with_the_exact_text() {
    for shell in shells() {
        let run = run(shell, "--model haiku --continue", false, true);
        let log = run.log();
        let context = format!("{shell:?}\nstdout: {}\nstderr: {}", run.stdout, run.stderr);
        assert_eq!(log.len(), 2, "{context}\n{log:?}");
        assert_eq!(
            log[0].0,
            strings(&["--model", "haiku", "--continue"]),
            "{context}"
        );
        assert_eq!(
            log[1].0,
            strings(&["--model", "haiku", "--resume", SESSION, PROMPT]),
            "{context}"
        );
        // The variable is set for each Claude, for the same shell, and gone after.
        assert!(log[0].1.is_some(), "{context}");
        assert_eq!(log[0].1, log[1].1, "{context}");
        assert!(run.stdout.contains("after=[]"), "{context}");
        // The request was consumed.
        let reload = run.dir.path().join("cfg/claude-code-sync/guard/reload");
        let left: Vec<_> = std::fs::read_dir(&reload)
            .map(|d| d.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{context}\n{left:?}");
    }
}

#[test]
fn a_normal_exit_runs_claude_once() {
    for shell in shells() {
        let run = run(shell, "--model haiku", false, false);
        let log = run.log();
        let context = format!("{shell:?}\nstdout: {}\nstderr: {}", run.stdout, run.stderr);
        assert_eq!(log.len(), 1, "{context}");
        assert_eq!(log[0].0, strings(&["--model", "haiku"]), "{context}");
        assert!(run.stdout.contains("after=[]"), "{context}");
    }
}

#[test]
fn print_mode_never_gets_the_variable_and_is_never_reloaded() {
    for shell in shells() {
        for flag in ["-p", "--print"] {
            let run = run(shell, &format!("{flag} hola"), false, true);
            let log = run.log();
            let context = format!(
                "{shell:?} {flag}\nstdout: {}\nstderr: {}",
                run.stdout, run.stderr
            );
            assert_eq!(log.len(), 1, "{context}");
            assert_eq!(log[0].0, strings(&[flag, "hola"]), "{context}");
            assert_eq!(log[0].1, None, "{context}");
        }
    }
}

#[test]
fn reloads_in_a_row_stop_at_five() {
    for shell in shells() {
        let run = run(shell, "--verbose", true, true);
        let log = run.log();
        let context = format!("{shell:?}\nstdout: {}\nstderr: {}", run.stdout, run.stderr);
        assert_eq!(log.len(), 5, "{context}");
        assert_eq!(log[0].0, strings(&["--verbose"]), "{context}");
        assert_eq!(
            log[4].0,
            strings(&["--verbose", "--resume", SESSION, PROMPT]),
            "{context}"
        );
    }
}
