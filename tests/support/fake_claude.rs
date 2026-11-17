//! A stand-in for `claude` that tests/test_wrapper.rs compiles with rustc and
//! puts first on PATH (as `claude`, `claude.exe` on Windows).
//!
//! Every run appends one JSON line to `FAKE_CLAUDE_LOG`: its arguments and
//! the value of `CLAUDE_GUARD_SHELL` (null when unset). On its first run (no
//! `FAKE_CLAUDE_STATE` file yet; every run when `FAKE_CLAUDE_ALWAYS` is set)
//! it also asks for a reload, as the prompt hook does: it copies
//! `FAKE_CLAUDE_REQUEST_TEMPLATE` to `<FAKE_CLAUDE_REQUEST_DIR>/<shell>.json`,
//! then exits, as a window ended by the hook does.

use std::io::Write;

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let shell = std::env::var("CLAUDE_GUARD_SHELL").ok();
    let log = std::env::var("FAKE_CLAUDE_LOG").expect("FAKE_CLAUDE_LOG");
    let line = format!(
        "{{\"args\":[{}],\"shell\":{}}}\n",
        args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(","),
        shell.as_deref().map_or("null".to_string(), quote)
    );
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();

    let state = std::env::var("FAKE_CLAUDE_STATE").expect("FAKE_CLAUDE_STATE");
    let first = !std::path::Path::new(&state).exists();
    if first {
        std::fs::write(&state, "ran").unwrap();
    }
    if first || std::env::var_os("FAKE_CLAUDE_ALWAYS").is_some() {
        if let Some(shell) = shell {
            let template = std::fs::read(std::env::var("FAKE_CLAUDE_REQUEST_TEMPLATE").unwrap())
                .unwrap();
            let dir = std::path::PathBuf::from(std::env::var("FAKE_CLAUDE_REQUEST_DIR").unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{shell}.json")), template).unwrap();
        }
    }
}
