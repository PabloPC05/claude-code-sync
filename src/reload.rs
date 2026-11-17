//! Reloading a stale Claude Code window on its own (`guard`, 0.8.0).
//!
//! 0.7.0 blocks a prompt typed in a window that is behind and tells the
//! person to type `/resume <id>`. Here the guard does it for them: the
//! prompt hook closes that Claude and the shell function `claude` (which
//! `guard install` puts in the shell profiles, see [`crate::wrapper`])
//! starts it again with `claude --resume <id> "<their text>"`. The window
//! blinks, loads the chat fresh from disk and their message is sent.
//!
//! The two halves talk through a request file,
//! `<config dir>/guard/reload/<shell id>.json`:
//!
//! 1. the hook ([`stale_response`]) writes it and ends the Claude process,
//!    but only when that Claude is a direct child of the shell that runs the
//!    wrapper (`CLAUDE_GUARD_SHELL` names the shell): a nested Claude, started
//!    by Claude itself, inherits the variable and must never kill anything;
//! 2. the wrapper, when its Claude returns, runs `guard take-reload`
//!    ([`take_reload`]), which consumes the request and prints the arguments
//!    for the new run.
//!
//! Ending a window abruptly is harmless to the transcript: at most a
//! `last-prompt` and a `cost-state` line get appended.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The wrapper sets this, for the Claude it starts, to the id of its shell.
pub const SHELL_ENV: &str = "CLAUDE_GUARD_SHELL";

/// Prompts longer than this (in characters) are not put on the command line:
/// Windows limits it. They go to [`LAST_PROMPT_FILE`] instead.
pub const MAX_PROMPT_CHARS: usize = 8000;

/// Where a prompt too long for the command line is kept.
pub const LAST_PROMPT_FILE: &str = "last-prompt.txt";

/// A request nobody picked up for this long is stale (the hook died between
/// writing it and ending Claude): a later, unrelated exit must not reopen it.
const MAX_REQUEST_AGE: Duration = Duration::from_secs(120);

/// How long a window gets to end on SIGTERM before it is killed.
#[cfg(unix)]
const GRACE: Duration = Duration::from_millis(1500);

/// What the hook asks the wrapper to start again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub session_id: String,
    /// What the person typed in the stale window.
    pub prompt: String,
    pub cwd: String,
    pub created_at: String,
}

/// What a stale window's prompt shows when it is being reloaded.
pub fn reload_reason(session_id: &str) -> String {
    format!(
        "Recargando este chat con lo último de tu otro PC; tu mensaje se enviará solo. Si no se recarga, escribe /resume {session_id}."
    )
}

/// `<config dir>/guard/reload`, not created.
pub fn reload_dir() -> Result<PathBuf> {
    Ok(crate::config::ConfigManager::config_dir()?
        .join("guard")
        .join("reload"))
}

fn request_path(shell: u32) -> Result<PathBuf> {
    Ok(reload_dir()?.join(format!("{shell}.json")))
}

/// Write the request for the wrapper whose shell is `shell`.
pub fn write_request(shell: u32, request: &Request) -> Result<()> {
    let bytes = serde_json::to_vec(request)?;
    crate::branches::write_file(&request_path(shell)?, &bytes)
}

/// Delete the request for `shell`, if there is one.
pub fn discard_request(shell: u32) {
    if let Ok(path) = request_path(shell) {
        let _ = std::fs::remove_file(path);
    }
}

/// Read and delete the request for `shell`. `None` when there is none, when
/// it is unreadable or when it is older than [`MAX_REQUEST_AGE`].
pub fn consume_request(shell: u32) -> Option<Request> {
    let path = request_path(shell).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    let _ = std::fs::remove_file(&path);
    if age.is_some_and(|age| age > MAX_REQUEST_AGE) {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// The hook side
// ---------------------------------------------------------------------------

/// The shell id in `CLAUDE_GUARD_SHELL`, when the window `pid` is a direct
/// child of that shell and is alive; `None` otherwise (nothing may be ended).
fn shell_of_window(pid: u32, env: Option<&str>) -> Option<u32> {
    let shell = env?.trim().parse::<u32>().ok().filter(|&s| s > 0)?;
    if !crate::branches::process_is_alive(pid) {
        return None;
    }
    (crate::stale::parent_of(pid) == Some(shell)).then_some(shell)
}

/// What the prompt hook prints as the reason when window `pid` is behind
/// chat `session_id` and `prompt` was typed in it (`block` is the 0.7.0
/// reason, `/resume <id>`).
///
/// When the window was started by the `claude` wrapper (see the module
/// docs) it is ended and a request to start it again is left: the reload
/// reason is returned. In every other case, or when ending it fails, nothing
/// is left behind and `block` comes back.
pub fn stale_response(
    pid: u32,
    session_id: &str,
    prompt: &str,
    cwd: &str,
    block: String,
) -> String {
    stale_response_with(pid, session_id, prompt, cwd, block, terminate)
}

fn stale_response_with(
    pid: u32,
    session_id: &str,
    prompt: &str,
    cwd: &str,
    block: String,
    end: impl FnOnce(u32) -> bool,
) -> String {
    let Some(shell) = shell_of_window(pid, std::env::var(SHELL_ENV).ok().as_deref()) else {
        return block;
    };
    let request = Request {
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        cwd: cwd.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    if let Err(e) = write_request(shell, &request) {
        log::debug!("reload request: {e:#}");
        discard_request(shell);
        return block;
    }
    if end(pid) {
        reload_reason(session_id)
    } else {
        discard_request(shell);
        block
    }
}

/// End the process `pid`. Unix: SIGTERM, then SIGKILL when it is still there
/// after 1.5 s. `true` when it is gone or was signalled.
#[cfg(unix)]
pub fn terminate(pid: u32) -> bool {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if raw <= 1 {
        return false;
    }
    // SAFETY: kill(2) only delivers a signal to a process id.
    let sent = unsafe { libc::kill(raw, libc::SIGTERM) } == 0;
    if !sent {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    let started = std::time::Instant::now();
    while started.elapsed() < GRACE {
        if !crate::branches::process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if crate::branches::process_is_alive(pid) {
        // SAFETY: as above.
        unsafe { libc::kill(raw, libc::SIGKILL) };
    }
    true
}

/// End the process `pid`: OpenProcess(PROCESS_TERMINATE) and
/// TerminateProcess.
#[cfg(windows)]
pub fn terminate(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    // SAFETY: the handle is checked, used for one call and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let ended = TerminateProcess(handle, 1) != 0;
        CloseHandle(handle);
        ended
    }
}

#[cfg(not(any(unix, windows)))]
pub fn terminate(_pid: u32) -> bool {
    false
}

// ---------------------------------------------------------------------------
// The wrapper side: `guard take-reload`
// ---------------------------------------------------------------------------

/// Flags of `claude` that carry over to the relaunch and take no value.
const KEEP_BOOLEAN: [&str; 7] = [
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--strict-mcp-config",
    "--verbose",
    "--ide",
    "--chrome",
    "--no-chrome",
];

/// Flags that carry over with exactly one value.
const KEEP_VALUE: [&str; 5] = [
    "--model",
    "--permission-mode",
    "--settings",
    "--setting-sources",
    "--agent",
];

/// Flags that carry over with every value up to the next flag (Claude Code
/// reads them as variadic and repeatable).
const KEEP_MANY: [&str; 2] = ["--add-dir", "--mcp-config"];

/// The part of the original `claude` command line worth keeping: a
/// whitelist. Everything else (`-c`, `--continue`, `-r`, `--resume`,
/// `--fork-session`, `--session-id`, the typed prompt, flags this version
/// does not know) is dropped, because the relaunch is about one chat.
pub fn kept_args(original: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut i = 0;
    while i < original.len() {
        let arg = original[i].as_str();
        i += 1;
        if arg == "--" {
            break;
        }
        let (name, has_value) = match arg.split_once('=') {
            Some((name, _)) if arg.starts_with("--") => (name, true),
            _ => (arg, false),
        };
        if KEEP_BOOLEAN.contains(&arg) {
            kept.push(arg.to_string());
        } else if KEEP_VALUE.contains(&name) || KEEP_MANY.contains(&name) {
            if has_value {
                kept.push(arg.to_string());
            } else if KEEP_VALUE.contains(&name) {
                if let Some(value) = original.get(i) {
                    kept.push(arg.to_string());
                    kept.push(value.clone());
                    i += 1;
                }
            } else {
                let values: Vec<&String> = original[i..]
                    .iter()
                    .take_while(|v| !v.starts_with('-'))
                    .collect();
                if !values.is_empty() {
                    kept.push(arg.to_string());
                    i += values.len();
                    kept.extend(values.into_iter().cloned());
                }
            }
        }
    }
    kept
}

/// The arguments of the relaunch: the kept ones, then `--resume <id>`, then
/// the prompt when there is one. A prompt that starts with `-` goes after a
/// `--` so Claude Code does not read it as a flag.
pub fn relaunch_args(original: &[String], session_id: &str, prompt: Option<&str>) -> Vec<String> {
    let mut args = kept_args(original);
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        if prompt.starts_with('-') {
            args.push("--".to_string());
        }
        args.push(prompt.to_string());
    }
    args
}

/// Escape one argument for Windows PowerShell's legacy native-argument
/// passing (Windows PowerShell 5.1, and PowerShell 7 before 7.3), so that the
/// program receives the exact text.
///
/// Legacy passing wraps an argument in double quotes when it has whitespace
/// outside quotes, and otherwise leaves it as it is: quotes inside are not
/// escaped for the program. The program then reads the command line by the
/// `CommandLineToArgvW` rules, so this does what they ask: backslashes
/// before a `"` are doubled and the quote gets a backslash, and a trailing
/// run of backslashes is doubled when the argument gets wrapped (it would
/// escape the closing quote). An empty argument becomes `""`, because
/// legacy passing drops an empty one altogether.
pub fn escape_ps_legacy(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    let wrapped = arg.chars().any(char::is_whitespace);
    let mut out = String::with_capacity(arg.len() + 2);
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                out.push_str(&"\\".repeat(backslashes + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    if wrapped {
        out.push_str(&"\\".repeat(backslashes));
    }
    out
}

/// How `take-reload` prints the arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A JSON array of strings, ASCII only (`\uXXXX` for everything else, so
    /// a console that decodes the output with another code page cannot
    /// garble it).
    Json,
    /// Each argument followed by a NUL byte.
    Nul,
    /// One line per argument: its UTF-8 bytes in base64.
    B64,
}

/// `args` as bytes in `format`.
pub fn render(args: &[String], format: Format) -> Vec<u8> {
    match format {
        Format::Nul => {
            let mut out = Vec::new();
            for arg in args {
                out.extend_from_slice(arg.as_bytes());
                out.push(0);
            }
            out
        }
        Format::B64 => {
            use base64::Engine;
            let mut out = String::new();
            for arg in args {
                out.push_str(&base64::engine::general_purpose::STANDARD.encode(arg.as_bytes()));
                out.push('\n');
            }
            out.into_bytes()
        }
        Format::Json => {
            let json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
            let mut out = String::with_capacity(json.len());
            for c in json.chars() {
                if c.is_ascii() {
                    out.push(c);
                } else {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            out.push('\n');
            out.into_bytes()
        }
    }
}

/// The arguments for the relaunch of shell `shell`, or `None` when no reload
/// was asked. Consumes the request. A prompt over [`MAX_PROMPT_CHARS`] is
/// left out of the arguments and saved to `<reload dir>/last-prompt.txt`
/// (the note says where).
pub fn take_reload(
    shell: u32,
    original: &[String],
    ps_legacy: bool,
    note: &mut dyn Write,
) -> Option<Vec<String>> {
    let request = consume_request(shell)?;
    let mut prompt = Some(request.prompt.as_str());
    if request.prompt.chars().count() > MAX_PROMPT_CHARS {
        prompt = None;
        let saved = reload_dir().ok().map(|dir| dir.join(LAST_PROMPT_FILE));
        match saved
            .as_deref()
            .map(|path| save_prompt(path, &request.prompt))
        {
            Some(Ok(())) => {
                let _ = writeln!(
                    note,
                    "claude-code-sync: your message is too long for the command line; it is saved in {}",
                    saved.unwrap().display()
                );
            }
            _ => {
                let _ = writeln!(
                    note,
                    "claude-code-sync: your message is too long for the command line and could not be saved"
                );
            }
        }
    }
    let mut args = relaunch_args(original, &request.session_id, prompt);
    if ps_legacy {
        args = args.iter().map(|a| escape_ps_legacy(a)).collect();
    }
    Some(args)
}

fn save_prompt(path: &Path, prompt: &str) -> Result<()> {
    crate::branches::write_file(path, prompt.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// `guard take-reload`.
pub fn take_reload_command(
    shell: u32,
    ps_legacy: bool,
    format: Format,
    original: &[String],
) -> Result<()> {
    if let Some(args) = take_reload(shell, original, ps_legacy, &mut std::io::stderr()) {
        let mut out = std::io::stdout().lock();
        out.write_all(&render(&args, format))?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stale::{HOOK_PID_ENV, HOOK_SHELL_PARENT_ENV};
    use serial_test::serial;
    use std::process::{Child, Command, Stdio};

    const ID: &str = "7a1f0c52-3b9e-4d64-8f21-0c5d9e7b1a33";

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // -- arguments --------------------------------------------------------

    #[test]
    fn the_whitelist_keeps_options_and_drops_the_chat_selection() {
        assert_eq!(
            relaunch_args(&v(&["--model", "haiku", "--continue"]), ID, Some("hola")),
            v(&["--model", "haiku", "--resume", ID, "hola"])
        );
        let original = v(&[
            "--model",
            "opus",
            "--permission-mode",
            "plan",
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--add-dir",
            "/a",
            "/b",
            "--settings",
            "s.json",
            "--setting-sources",
            "user,project",
            "--strict-mcp-config",
            "--mcp-config",
            "m1.json",
            "m2.json",
            "--verbose",
            "--agent",
            "reviewer",
            "--ide",
            "--chrome",
            "--no-chrome",
        ]);
        assert_eq!(kept_args(&original), original);
    }

    #[test]
    fn everything_outside_the_whitelist_is_dropped() {
        let original = v(&[
            "-c",
            "--continue",
            "-r",
            "other-id",
            "--resume",
            "--resume=x",
            "--fork-session",
            "--session-id",
            "abc",
            "--append-system-prompt",
            "be brief",
            "--output-format=json",
            "positional prompt",
            "--model",
            "haiku",
        ]);
        assert_eq!(kept_args(&original), v(&["--model", "haiku"]));
        assert_eq!(
            relaunch_args(&v(&["-c"]), ID, None),
            v(&["--resume", ID]),
            "no prompt, none appended"
        );
        assert_eq!(relaunch_args(&[], ID, Some("")), v(&["--resume", ID]));
    }

    #[test]
    fn the_equals_forms_and_repeated_variadics_are_kept() {
        assert_eq!(
            kept_args(&v(&[
                "--model=haiku",
                "--permission-mode=plan",
                "--add-dir=/x",
                "--add-dir",
                "/y",
                "--add-dir",
                "/z",
                "--settings={\"a\":1}",
            ])),
            v(&[
                "--model=haiku",
                "--permission-mode=plan",
                "--add-dir=/x",
                "--add-dir",
                "/y",
                "--add-dir",
                "/z",
                "--settings={\"a\":1}",
            ])
        );
    }

    #[test]
    fn a_flag_missing_its_value_and_text_after_double_dash_are_dropped() {
        assert_eq!(kept_args(&v(&["--verbose", "--model"])), v(&["--verbose"]));
        assert_eq!(kept_args(&v(&["--add-dir"])), Vec::<String>::new());
        assert_eq!(
            kept_args(&v(&["--add-dir", "--verbose"])),
            v(&["--verbose"])
        );
        assert_eq!(
            kept_args(&v(&["--verbose", "--", "--model", "x"])),
            v(&["--verbose"])
        );
        // An option's value that looks like a flag is still its value.
        assert_eq!(
            kept_args(&v(&["--settings", "-weird.json"])),
            v(&["--settings", "-weird.json"])
        );
    }

    #[test]
    fn a_prompt_that_looks_like_a_flag_follows_a_double_dash() {
        assert_eq!(
            relaunch_args(&[], ID, Some("-5 grados")),
            v(&["--resume", ID, "--", "-5 grados"])
        );
    }

    // -- PowerShell legacy escaping ---------------------------------------

    /// What a program reading its command line by the `CommandLineToArgvW`
    /// rules gets from the text legacy PowerShell builds: `arg` wrapped in
    /// quotes when it has whitespace outside quotes, as it does.
    fn argv_of_one(escaped: &str) -> String {
        // PowerShell's NeedQuotes: whitespace while not inside an unescaped
        // pair of quotes.
        let mut quotes = 0;
        let mut after_backslash = false;
        let mut needs = false;
        for c in escaped.chars() {
            if c == '"' && !after_backslash {
                quotes += 1;
            } else if c.is_whitespace() && quotes % 2 == 0 {
                needs = true;
            }
            after_backslash = c == '\\';
        }
        let line = if needs {
            format!("\"{escaped}\"")
        } else {
            escaped.to_string()
        };
        // CommandLineToArgvW for a single argument.
        let chars: Vec<char> = line.chars().collect();
        let mut out = String::new();
        let mut in_quotes = false;
        let mut i = 0;
        while i < chars.len() {
            let mut backslashes = 0;
            while i < chars.len() && chars[i] == '\\' {
                backslashes += 1;
                i += 1;
            }
            if i < chars.len() && chars[i] == '"' {
                out.push_str(&"\\".repeat(backslashes / 2));
                if backslashes % 2 == 1 {
                    out.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
                i += 1;
            } else {
                out.push_str(&"\\".repeat(backslashes));
                if i < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        }
        assert!(!in_quotes, "unbalanced quotes in {line:?}");
        out
    }

    #[test]
    fn legacy_escaping_of_each_case() {
        assert_eq!(escape_ps_legacy("haiku"), "haiku");
        assert_eq!(escape_ps_legacy("two words"), "two words");
        assert_eq!(escape_ps_legacy(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_ps_legacy(r#"a\"b"#), r#"a\\\"b"#);
        assert_eq!(escape_ps_legacy(r"C:\dir\"), r"C:\dir\", "no whitespace");
        assert_eq!(escape_ps_legacy(r"C:\my dir\"), r"C:\my dir\\");
        assert_eq!(escape_ps_legacy("a\\\\ b"), "a\\\\ b");
        assert_eq!(escape_ps_legacy(""), "\"\"");
        assert_eq!(escape_ps_legacy("línea\nnueva"), "línea\nnueva");
        assert_eq!(escape_ps_legacy("año é 😀"), "año é 😀");
        assert_eq!(escape_ps_legacy(r#"\\""#), r#"\\\\\""#);
    }

    #[test]
    fn what_legacy_escaping_produces_is_what_the_program_receives() {
        let cases = [
            "plain",
            "two words",
            "tab\tsep",
            "new\nline",
            "trailing newline\n",
            r#"say "hi" now"#,
            r#""quoted""#,
            r#"a\"b c"#,
            r"back\slash",
            r"back\slash and space\",
            r"ends\\",
            r"ends with space and slashes \\",
            r#"\\"#,
            r#"\"#,
            r#"x\\"y z"#,
            "",
            "ñandú café é",
            "emoji 😀 y 🇪🇸",
            r#"Tengo "comillas" y C:\ruta con espacio\ y ñ 😀
y otra línea\"#,
        ];
        for case in cases {
            assert_eq!(argv_of_one(&escape_ps_legacy(case)), case, "{case:?}");
        }
    }

    // -- output -----------------------------------------------------------

    #[test]
    fn the_formats_carry_every_argument_exactly() {
        let args = v(&["--resume", ID, "línea\n\"comillas\" \\ 😀 \u{0}x"]);
        let nul = render(&args, Format::Nul);
        let back: Vec<String> = nul
            .split(|&b| b == 0)
            .take(args.len())
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect();
        // The NUL inside the last argument is the one thing it cannot hold.
        assert_eq!(&back[..2], &args[..2]);
        assert!(nul.ends_with(&[0]));
        let args = v(&["--resume", ID, "línea\n\"comillas\" \\ 😀 x", ""]);
        let nul = render(&args, Format::Nul);
        assert_eq!(nul.iter().filter(|&&b| b == 0).count(), args.len());

        let json = String::from_utf8(render(&args, Format::Json)).unwrap();
        assert!(json.is_ascii(), "{json}");
        assert!(json.ends_with('\n'));
        assert_eq!(serde_json::from_str::<Vec<String>>(&json).unwrap(), args);
        assert!(json.contains("\\ud83d\\ude00"));

        use base64::Engine;
        let b64 = String::from_utf8(render(&args, Format::B64)).unwrap();
        let back: Vec<String> = b64
            .lines()
            .map(|l| {
                String::from_utf8(base64::engine::general_purpose::STANDARD.decode(l).unwrap())
                    .unwrap()
            })
            .collect();
        assert_eq!(back, args);
        assert!(b64.is_ascii());
    }

    // -- requests ---------------------------------------------------------

    /// A config directory in a temp dir, and the environment pointing at it.
    struct Env {
        _tmp: tempfile::TempDir,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Env {
        fn new() -> Env {
            let tmp = tempfile::TempDir::new().unwrap();
            let names = [
                "CLAUDE_CODE_SYNC_CONFIG_DIR",
                SHELL_ENV,
                HOOK_SHELL_PARENT_ENV,
                HOOK_PID_ENV,
            ];
            let saved = names
                .into_iter()
                .map(|n| (n, std::env::var(n).ok()))
                .collect();
            std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", tmp.path().join("cfg"));
            for name in [SHELL_ENV, HOOK_SHELL_PARENT_ENV, HOOK_PID_ENV] {
                std::env::remove_var(name);
            }
            Env { _tmp: tmp, saved }
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

    fn request(prompt: &str) -> Request {
        Request {
            session_id: ID.to_string(),
            prompt: prompt.to_string(),
            cwd: "/work".to_string(),
            created_at: "2026-11-17T10:00:00Z".to_string(),
        }
    }

    #[test]
    #[serial]
    fn a_request_is_written_then_consumed_once() {
        let _env = Env::new();
        write_request(4242, &request("hola")).unwrap();
        assert!(reload_dir().unwrap().join("4242.json").exists());
        assert_eq!(consume_request(777), None, "another shell's is not touched");
        assert_eq!(consume_request(4242), Some(request("hola")));
        assert!(!reload_dir().unwrap().join("4242.json").exists());
        assert_eq!(consume_request(4242), None);
    }

    #[test]
    #[serial]
    fn an_old_or_unreadable_request_is_dropped() {
        let _env = Env::new();
        write_request(5, &request("hola")).unwrap();
        let path = reload_dir().unwrap().join("5.json");
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(consume_request(5), None);
        assert!(!path.exists(), "it is deleted anyway");

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(consume_request(5), None);
        assert!(!path.exists());
    }

    #[test]
    #[serial]
    fn take_reload_prints_the_relaunch_and_nothing_without_a_request() {
        let _env = Env::new();
        let mut note = Vec::new();
        assert_eq!(
            take_reload(9, &v(&["--model", "haiku"]), false, &mut note),
            None
        );
        write_request(9, &request("con \"comillas\"")).unwrap();
        assert_eq!(
            take_reload(9, &v(&["--model", "haiku", "--continue"]), false, &mut note),
            Some(v(&["--model", "haiku", "--resume", ID, "con \"comillas\""]))
        );
        assert_eq!(take_reload(9, &[], false, &mut note), None, "consumed");

        write_request(9, &request("con \"comillas\"")).unwrap();
        assert_eq!(
            take_reload(9, &[], true, &mut note),
            Some(v(&["--resume", ID, r#"con \"comillas\""#]))
        );
        assert!(note.is_empty());
    }

    #[test]
    #[serial]
    fn a_long_prompt_goes_to_a_file_and_the_note_says_where() {
        let _env = Env::new();
        let long = "ñ".repeat(MAX_PROMPT_CHARS + 1);
        write_request(3, &request(&long)).unwrap();
        let mut note = Vec::new();
        let args = take_reload(3, &v(&["--model", "haiku"]), false, &mut note).unwrap();
        assert_eq!(args, v(&["--model", "haiku", "--resume", ID]));
        let file = reload_dir().unwrap().join(LAST_PROMPT_FILE);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), long);
        let note = String::from_utf8(note).unwrap();
        assert!(note.contains(&file.display().to_string()), "{note}");

        // Exactly the limit still goes on the command line.
        let edge = "ñ".repeat(MAX_PROMPT_CHARS);
        write_request(3, &request(&edge)).unwrap();
        let args = take_reload(3, &[], false, &mut Vec::new()).unwrap();
        assert_eq!(args, v(&["--resume", ID, &edge]));
    }

    // -- the hook ---------------------------------------------------------

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

    fn block() -> String {
        "BLOCK".to_string()
    }

    fn pending(shell: u32) -> bool {
        reload_dir().unwrap().join(format!("{shell}.json")).exists()
    }

    #[test]
    #[serial]
    fn without_the_wrapper_the_prompt_is_blocked_and_nothing_is_ended() {
        let _env = Env::new();
        let mut child = long_child();
        std::env::set_var(HOOK_SHELL_PARENT_ENV, "4242");
        // No CLAUDE_GUARD_SHELL: a Claude the wrapper did not start.
        let reason = stale_response(child.id(), ID, "hola", "/w", block());
        assert_eq!(reason, "BLOCK");
        assert!(!pending(4242));
        assert!(child.try_wait().unwrap().is_none(), "still running");
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    #[serial]
    fn a_shell_that_is_not_the_windows_parent_ends_nothing() {
        // A nested Claude inherits the variable from the one above it.
        let _env = Env::new();
        let mut child = long_child();
        std::env::set_var(SHELL_ENV, "4242");
        std::env::set_var(HOOK_SHELL_PARENT_ENV, "9999");
        let reason = stale_response(child.id(), ID, "hola", "/w", block());
        assert_eq!(reason, "BLOCK");
        assert!(!pending(4242) && !pending(9999));
        assert!(child.try_wait().unwrap().is_none());
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    #[serial]
    fn a_bad_shell_id_or_a_dead_window_ends_nothing() {
        let _env = Env::new();
        let mut child = long_child();
        std::env::set_var(HOOK_SHELL_PARENT_ENV, "4242");
        for bad in ["", "abc", "0", "-5"] {
            std::env::set_var(SHELL_ENV, bad);
            assert_eq!(stale_response(child.id(), ID, "x", "/w", block()), "BLOCK");
        }
        assert!(child.try_wait().unwrap().is_none());
        child.kill().ok();
        child.wait().ok();

        std::env::set_var(SHELL_ENV, "4242");
        assert_eq!(stale_response(child.id(), ID, "x", "/w", block()), "BLOCK");
        assert!(!pending(4242));
    }

    #[test]
    #[serial]
    fn the_wrappers_own_window_is_ended_and_the_request_is_left() {
        let _env = Env::new();
        let mut child = long_child();
        std::env::set_var(SHELL_ENV, "4242");
        std::env::set_var(HOOK_SHELL_PARENT_ENV, "4242");
        let reason = stale_response(child.id(), ID, "hola \"mundo\"", "/w", block());
        assert_eq!(reason, reload_reason(ID));
        assert!(reason.contains(&format!("/resume {ID}")));
        assert!(reason.starts_with("Recargando este chat"));
        // The window is gone.
        let status = child.wait().unwrap();
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert!(status.signal().is_some(), "ended by a signal: {status:?}");
        }
        let request = consume_request(4242).expect("the request is there");
        assert_eq!(request.session_id, ID);
        assert_eq!(request.prompt, "hola \"mundo\"");
        assert_eq!(request.cwd, "/w");
        assert!(!request.created_at.is_empty());
    }

    #[test]
    #[serial]
    fn when_ending_the_window_fails_the_request_goes_and_the_prompt_is_blocked() {
        let _env = Env::new();
        let mut child = long_child();
        std::env::set_var(SHELL_ENV, "4242");
        std::env::set_var(HOOK_SHELL_PARENT_ENV, "4242");
        let reason = stale_response_with(child.id(), ID, "hola", "/w", block(), |_| {
            assert!(pending(4242), "the request is written before ending");
            false
        });
        assert_eq!(reason, "BLOCK");
        assert!(!pending(4242));
        child.kill().ok();
        child.wait().ok();
    }

    #[cfg(unix)]
    #[test]
    fn terminate_ends_a_process_and_knows_one_that_is_already_gone() {
        let mut child = long_child();
        assert!(terminate(child.id()));
        child.wait().unwrap();
        // Never a process id that could be a system one.
        assert!(!terminate(0));
        assert!(!terminate(1));
    }
}
