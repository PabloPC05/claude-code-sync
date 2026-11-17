//! Automatic sync: a timer that runs `autosync run` every few minutes, plus
//! Claude Code hooks (see [`HOOKS`]): a sync as soon as a turn finishes or a
//! chat closes, and the guard that stops a prompt typed in a window that holds
//! an out-of-date chat (see `crate::stale`).
//!
//! - macOS: a LaunchAgent, loaded with `launchctl bootstrap gui/<uid>`.
//! - Linux: a systemd user service and timer (a crontab line when systemd
//!   is not there).
//! - Windows: a Task Scheduler task running a `.vbs` wrapper through
//!   `wscript.exe`, so no console window ever flashes.
//!
//! `autosync run` never prompts, exits 0 when there is nothing to do, and
//! takes the machine's sync lock: when the timer and the hook fire together,
//! the second run finds the lock taken and quietly leaves.
//!
//! Everything this module writes is produced by pure functions of the paths
//! and settings involved, which the tests check for every platform; only
//! `install`, `uninstall` and `status` touch the machine.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde_json::value::RawValue;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The LaunchAgent label, also its plist's file name.
pub const LAUNCHD_LABEL: &str = "io.github.claude-code-sync.autosync";

/// The systemd user units' base name.
pub const SYSTEMD_UNIT: &str = "claude-code-sync-autosync";

/// The Task Scheduler task's name.
pub const WINDOWS_TASK: &str = "claude-code-sync autosync";

/// A detached `autosync run`: what the Windows task's `.vbs` runs, and what
/// the `SessionEnd` hook ran before 0.6.0 (still recognized, so `install`
/// replaces it and `uninstall` removes it).
pub const DETACH_ARGS: [&str; 3] = ["autosync", "run", "--detach"];

/// The shell-form command line an earlier build of this fork wrote, still
/// recognized so `install` replaces it and `uninstall` removes it.
const LEGACY_HOOK_COMMAND_SUFFIX: &str = "autosync run --detach";

/// A Claude Code hook this module installs: the event, the arguments after
/// the binary (which also tell it apart from everyone else's hooks), and the
/// seconds Claude Code gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSpec {
    pub event: &'static str,
    pub args: [&'static str; 2],
    pub timeout_seconds: u64,
}

/// The hooks `autosync install` adds. `SessionEnd` hooks share a 1.5 s
/// budget unless one asks for more; each of these returns in milliseconds,
/// and the timeouts only cover a slow disk.
pub const HOOKS: [HookSpec; 4] = [
    HookSpec {
        event: "UserPromptSubmit",
        args: ["hook", "prompt"],
        timeout_seconds: 5,
    },
    HookSpec {
        event: "SessionStart",
        args: ["hook", "session-start"],
        timeout_seconds: 5,
    },
    HookSpec {
        event: "SessionEnd",
        args: ["hook", "session-end"],
        timeout_seconds: 10,
    },
    HookSpec {
        event: "Stop",
        args: ["hook", "stop"],
        timeout_seconds: 5,
    },
];

/// The retention `autosync install` asks Claude Code for: it otherwise
/// deletes transcripts older than 30 days on its own, on one machine.
pub const RETENTION_DAYS: u64 = 3650;

/// Default timer interval, in minutes.
pub const DEFAULT_INTERVAL_MINUTES: u32 = 10;

/// The longest interval `schtasks /SC MINUTE` accepts, and so the limit on
/// every platform.
pub const MAX_INTERVAL_MINUTES: u32 = 1439;

/// Environment passed through to the timer's runs when set at install time,
/// so a timer run reads the same configuration as the install did.
const PASSED_THROUGH_ENV: &[&str] = &[
    "CLAUDE_CODE_SYNC_CONFIG_DIR",
    "CLAUDE_CODE_SYNC_CLAUDE_DIR",
    "CLAUDE_CODE_SYNC_HOME",
];

/// The operating systems autosync knows how to schedule on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// Where the pieces live on one machine.
#[derive(Debug, Clone)]
pub struct Layout {
    pub platform: Platform,
    /// The binary the timer and the hook run.
    pub exe: PathBuf,
    pub home: PathBuf,
    /// claude-code-sync's config directory (Windows keeps the `.vbs` here).
    pub config_dir: PathBuf,
    /// `~/.config` or `$XDG_CONFIG_HOME` (Linux: where systemd user units go).
    pub xdg_config_home: PathBuf,
    /// Claude Code's `settings.json`.
    pub claude_settings: PathBuf,
    /// The user id `launchctl` addresses the GUI domain by (macOS).
    pub uid: u32,
    /// `PATH` and the passed-through variables, for the timer's runs.
    pub env: Vec<(String, String)>,
}

impl Layout {
    /// This machine's layout.
    pub fn detect() -> Result<Layout> {
        let home = crate::sync::discovery::machine_home_dir().context("No home directory")?;
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let mut env = Vec::new();
        if let Ok(path) = std::env::var("PATH") {
            env.push(("PATH".to_string(), path));
        }
        for name in PASSED_THROUGH_ENV {
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    env.push((name.to_string(), value));
                }
            }
        }
        Ok(Layout {
            platform: Platform::current(),
            exe: std::env::current_exe().context("Cannot tell where this binary is")?,
            home,
            config_dir: crate::config::ConfigManager::config_dir()?,
            xdg_config_home,
            claude_settings: crate::sync::discovery::claude_home_dir()?.join("settings.json"),
            uid: current_uid(),
            env,
        })
    }

    /// Where autosync's runs write their output.
    pub fn log_path(&self) -> PathBuf {
        match self.platform {
            Platform::MacOs => self.home.join("Library/Logs/claude-code-sync.log"),
            Platform::Linux | Platform::Windows => self.config_dir.join("autosync.log"),
        }
    }

    pub fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"))
    }

    pub fn systemd_dir(&self) -> PathBuf {
        self.xdg_config_home.join("systemd/user")
    }

    pub fn vbs_path(&self) -> PathBuf {
        self.config_dir.join("autosync.vbs")
    }

    fn exe_text(&self) -> String {
        self.exe.to_string_lossy().into_owned()
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid(2) cannot fail and has no side effects.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// A command the install or uninstall runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommand {
    pub args: Vec<String>,
    /// Failing is expected sometimes (unloading what is not loaded).
    pub may_fail: bool,
}

impl PlannedCommand {
    fn new(args: &[&str], may_fail: bool) -> Self {
        PlannedCommand {
            args: args.iter().map(|a| a.to_string()).collect(),
            may_fail,
        }
    }

    /// The command as one could type it, for `--dry-run`.
    pub fn display(&self) -> String {
        self.args
            .iter()
            .map(|a| {
                if a.contains(' ') || a.contains('"') {
                    format!("'{a}'")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Everything one install or uninstall does to the machine, before it does
/// any of it.
#[derive(Debug, Default)]
pub struct Plan {
    /// Files to write, with their bytes.
    pub writes: Vec<(PathBuf, Vec<u8>)>,
    /// Files to delete.
    pub removals: Vec<PathBuf>,
    /// Directories that must exist first.
    pub directories: Vec<PathBuf>,
    pub commands: Vec<PlannedCommand>,
    /// The new `settings.json`, when it changes, and what changed in it.
    pub settings: Option<(String, Vec<String>)>,
}

// ---------------------------------------------------------------------------
// Generated files
// ---------------------------------------------------------------------------

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The LaunchAgent: `autosync run` at load and every `interval_minutes`.
pub fn launchd_plist(layout: &Layout, interval_minutes: u32) -> String {
    let exe = xml_escape(&layout.exe_text());
    let log = xml_escape(&layout.log_path().to_string_lossy());
    let mut env = String::new();
    for (name, value) in &layout.env {
        env.push_str(&format!(
            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
            xml_escape(name),
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCHD_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>autosync</string>
		<string>run</string>
	</array>
	<key>StartInterval</key>
	<integer>{seconds}</integer>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
</dict>
</plist>
"#,
        seconds = u64::from(interval_minutes) * 60,
    )
}

/// A word of a systemd unit line, quoted: `%` is a specifier there.
fn systemd_quote(word: &str) -> String {
    let escaped = word
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// The systemd user service: one `autosync run`.
pub fn systemd_service(layout: &Layout) -> String {
    let log = layout.log_path().to_string_lossy().replace('%', "%%");
    let mut env = String::new();
    for (name, value) in &layout.env {
        env.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{name}={value}"))
        ));
    }
    format!(
        "[Unit]\n\
         Description=Sync Claude Code history (claude-code-sync autosync)\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} autosync run\n\
         {env}\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n",
        systemd_quote(&layout.exe_text()),
    )
}

/// The systemd user timer that starts the service every `interval_minutes`.
pub fn systemd_timer(interval_minutes: u32) -> String {
    format!(
        "[Unit]\n\
         Description=Run claude-code-sync autosync every {interval_minutes} minutes\n\
         \n\
         [Timer]\n\
         OnBootSec=2min\n\
         OnUnitActiveSec={interval_minutes}min\n\
         Unit={SYSTEMD_UNIT}.service\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// Quote a word for a POSIX shell.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The crontab line for a Linux machine without systemd.
pub fn crontab_line(layout: &Layout, interval_minutes: u32) -> String {
    let schedule = if interval_minutes < 60 {
        format!("*/{interval_minutes} * * * *")
    } else {
        format!("0 */{} * * *", (interval_minutes / 60).clamp(1, 23))
    };
    format!(
        "{schedule} {} autosync run >> {} 2>&1",
        shell_quote(&layout.exe_text()),
        shell_quote(&layout.log_path().to_string_lossy())
    )
}

/// The `.vbs` the Windows task runs: the binary, with no window at all.
pub fn windows_vbs(layout: &Layout) -> String {
    // A VBScript string doubles its quotes; the outer pair quotes the path.
    let command =
        format!("\"{}\" {}", layout.exe_text(), DETACH_ARGS.join(" ")).replace('"', "\"\"");
    format!(
        "' claude-code-sync autosync: one sync, with no console window.\r\n\
         ' Written by `claude-code-sync autosync install`, removed by `autosync uninstall`.\r\n\
         CreateObject(\"WScript.Shell\").Run \"{command}\", 0, False\r\n"
    )
}

/// UTF-16LE with a byte-order mark: the encoding `wscript.exe` reads any
/// path in, whatever the machine's code page (a user named Iñaki).
pub fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// `schtasks` arguments creating the task: every `interval_minutes`, for
/// the current user, running the `.vbs` through `wscript.exe`.
pub fn schtasks_create(layout: &Layout, interval_minutes: u32) -> PlannedCommand {
    let action = format!("wscript.exe \"{}\"", layout.vbs_path().to_string_lossy());
    let interval = interval_minutes.to_string();
    PlannedCommand::new(
        &[
            "schtasks",
            "/Create",
            "/SC",
            "MINUTE",
            "/MO",
            &interval,
            "/TN",
            WINDOWS_TASK,
            "/TR",
            &action,
            "/F",
        ],
        false,
    )
}

/// A hook, in Claude Code's exec form (2.1.139 and later): the binary's
/// absolute path and its arguments, run without a shell. The same entry
/// works on every OS — no quoting for Git Bash, `cmd.exe` or PowerShell to
/// get wrong, whatever the path holds.
pub fn hook_entry(exe: &Path, spec: &HookSpec) -> Value {
    serde_json::json!({
        "type": "command",
        "command": exe.to_string_lossy(),
        "args": spec.args,
        "timeout": spec.timeout_seconds,
    })
}

// ---------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------

/// A JSON object's members in file order, each value as its own text, so a
/// rewrite leaves every member it does not touch exactly as it was.
struct OrderedObject(Vec<(String, Box<RawValue>)>);

impl<'de> serde::Deserialize<'de> for OrderedObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Members;
        impl<'de> serde::de::Visitor<'de> for Members {
            type Value = OrderedObject;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<OrderedObject, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry::<String, Box<RawValue>>()? {
                    members.push(member);
                }
                Ok(OrderedObject(members))
            }
        }
        deserializer.deserialize_map(Members)
    }
}

/// `settings.json` as members in order, ready to change a few of them.
pub(crate) struct Settings {
    members: Vec<(String, String)>,
    trailing_newline: bool,
}

impl Settings {
    pub(crate) fn parse(original: Option<&str>) -> Result<Settings> {
        let text = original.unwrap_or("").trim_start_matches('\u{feff}');
        if text.trim().is_empty() {
            return Ok(Settings {
                members: Vec::new(),
                trailing_newline: true,
            });
        }
        let object: OrderedObject = serde_json::from_str(text)
            .context("Claude Code's settings.json is not a JSON object; fix it first")?;
        Ok(Settings {
            members: object
                .0
                .into_iter()
                .map(|(key, value)| (key, value.get().to_string()))
                .collect(),
            trailing_newline: text.ends_with('\n'),
        })
    }

    pub(crate) fn get(&self, key: &str) -> Option<Value> {
        self.members
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, raw)| serde_json::from_str(raw).ok())
    }

    /// Set a member, in place when it exists, at the end otherwise.
    pub(crate) fn set(&mut self, key: &str, value: &Value) {
        let text = indent_continuation(&serde_json::to_string_pretty(value).expect("serializes"));
        match self.members.iter_mut().find(|(k, _)| k == key) {
            Some((_, raw)) => *raw = text,
            None => self.members.push((key.to_string(), text)),
        }
    }

    pub(crate) fn remove(&mut self, key: &str) {
        self.members.retain(|(k, _)| k != key);
    }

    /// Two-space indentation, the way Claude Code writes the file.
    pub(crate) fn render(&self) -> String {
        let mut out = String::from("{");
        for (index, (key, raw)) in self.members.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let key = serde_json::to_string(key).expect("a string serializes");
            out.push_str(&format!("\n  {key}: {raw}"));
        }
        out.push_str(if self.members.is_empty() { "}" } else { "\n}" });
        if self.trailing_newline {
            out.push('\n');
        }
        out
    }
}

/// Indent every line but the first by two spaces: a pretty-printed value
/// placed one level deep.
fn indent_continuation(pretty: &str) -> String {
    pretty.replace('\n', "\n  ")
}

/// Whether a command is this tool's binary: named `claude-code-sync` (with
/// or without `.exe`, whatever its directory or version suffix), or the
/// `exe` an install is placing. The new hooks' arguments (`hook prompt`)
/// are short enough for another tool to use too.
pub(crate) fn is_our_binary(command: &str, exe: Option<&Path>) -> bool {
    if exe.is_some_and(|exe| Path::new(command) == exe) {
        return true;
    }
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    name.to_ascii_lowercase().starts_with("claude-code-sync")
}

/// Whether a hook entry under `event` is one autosync installed: the exec
/// form of that event's hook, or — under `SessionEnd`, where 0.5.0 put it —
/// the detached run, in exec form or the shell form an earlier build wrote.
pub(crate) fn is_our_hook(event: &str, hook: &Value, exe: Option<&Path>) -> bool {
    let args: Option<Vec<Option<&str>>> = hook
        .get("args")
        .and_then(|args| args.as_array())
        .map(|args| args.iter().map(|a| a.as_str()).collect());
    let ours = hook
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| is_our_binary(c, exe));
    let exec_form = ours
        && args.as_ref().is_some_and(|args| {
            HOOKS
                .iter()
                .filter(|spec| spec.event == event)
                .any(|spec| args.iter().copied().eq(spec.args.map(Some)))
        });
    let legacy = event == "SessionEnd"
        && (args
            .as_ref()
            .is_some_and(|args| args.iter().copied().eq(DETACH_ARGS.map(Some)))
            || hook
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.trim_end().ends_with(LEGACY_HOOK_COMMAND_SUFFIX)));
    exec_form || legacy
}

/// Remove autosync's hooks from a `hooks` object; returns the events one
/// was removed from.
fn remove_our_hooks(
    hooks: &mut serde_json::Map<String, Value>,
    exe: Option<&Path>,
) -> Vec<&'static str> {
    let mut events = Vec::new();
    for spec in &HOOKS {
        let event = spec.event;
        let Some(Value::Array(groups)) = hooks.get_mut(event) else {
            continue;
        };
        let mut removed = false;
        groups.retain_mut(|group| {
            let Some(Value::Array(entries)) = group.get_mut("hooks") else {
                return true;
            };
            let before = entries.len();
            entries.retain(|hook| !is_our_hook(event, hook, exe));
            if entries.len() == before {
                return true;
            }
            removed = true;
            !entries.is_empty()
        });
        if removed && groups.is_empty() {
            hooks.remove(event);
        }
        if removed {
            events.push(event);
        }
    }
    events
}

/// `settings.json` after an install: the retention raised to
/// [`RETENTION_DAYS`] when lower or missing, and — unless `hook` is `None`
/// (`--no-hooks`) — exactly one hook of ours per event of [`HOOKS`], running
/// the binary at `hook` (see [`hook_entry`]).
/// Every other member, and every other hook, is left as it was. `None` when
/// nothing changes; otherwise the new text and what changed.
pub fn settings_after_install(
    original: Option<&str>,
    hook: Option<&Path>,
) -> Result<Option<(String, Vec<String>)>> {
    let mut settings = Settings::parse(original)?;
    let mut changes = Vec::new();

    let retention = settings.get("cleanupPeriodDays");
    let current = retention.as_ref().and_then(Value::as_u64);
    if current.is_none_or(|days| days < RETENTION_DAYS) {
        settings.set("cleanupPeriodDays", &Value::from(RETENTION_DAYS));
        changes.push(match current {
            Some(days) => format!(
                "cleanupPeriodDays {days} -> {RETENTION_DAYS} (Claude Code would delete older transcripts on its own)"
            ),
            None => format!(
                "cleanupPeriodDays set to {RETENTION_DAYS} (Claude Code's default deletes transcripts after 30 days)"
            ),
        });
    }

    let hooks_before = settings.get("hooks");
    let mut hooks = match &hooks_before {
        Some(Value::Object(hooks)) => hooks.clone(),
        Some(_) => {
            bail!("settings.json has a \"hooks\" member that is not an object; fix it first")
        }
        None => serde_json::Map::new(),
    };
    let removed = remove_our_hooks(&mut hooks, hook);
    if let Some(exe) = hook {
        for spec in &HOOKS {
            let ours = serde_json::json!({ "hooks": [hook_entry(exe, spec)] });
            let groups = hooks
                .entry(spec.event)
                .or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(groups) = groups else {
                bail!(
                    "settings.json has a \"hooks.{}\" that is not a list; fix it first",
                    spec.event
                );
            };
            groups.push(ours);
        }
    }
    let hooks_changed = match &hooks_before {
        Some(before) => *before != Value::Object(hooks.clone()),
        None => !hooks.is_empty(),
    };
    if hooks_changed {
        if hooks.is_empty() {
            settings.remove("hooks");
        } else {
            settings.set("hooks", &Value::Object(hooks));
        }
        match hook {
            Some(exe) => {
                for spec in &HOOKS {
                    changes.push(format!(
                        "{} hook {}: {} {}",
                        spec.event,
                        if removed.contains(&spec.event) {
                            "updated"
                        } else {
                            "added"
                        },
                        exe.display(),
                        spec.args.join(" ")
                    ));
                }
            }
            None => {
                for event in &removed {
                    changes.push(format!("{event} hook removed (--no-hooks)"));
                }
            }
        }
    }

    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some((settings.render(), changes)))
}

/// `settings.json` after an uninstall: autosync's hooks removed, nothing
/// else — the retention stays raised, since lowering it would let Claude
/// Code delete transcripts. `None` when there is no hook of ours.
pub fn settings_after_uninstall(original: Option<&str>) -> Result<Option<(String, Vec<String>)>> {
    let Some(original) = original else {
        return Ok(None);
    };
    let mut settings = Settings::parse(Some(original))?;
    let Some(Value::Object(mut hooks)) = settings.get("hooks") else {
        return Ok(None);
    };
    let removed = remove_our_hooks(&mut hooks, None);
    if removed.is_empty() {
        return Ok(None);
    }
    if hooks.is_empty() {
        settings.remove("hooks");
    } else {
        settings.set("hooks", &Value::Object(hooks));
    }
    Ok(Some((
        settings.render(),
        removed
            .iter()
            .map(|event| format!("{event} hook removed"))
            .collect(),
    )))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// What `autosync install` does on this layout. `settings_text` is the
/// current `settings.json`, if any.
pub fn install_plan(
    layout: &Layout,
    interval_minutes: u32,
    hooks: bool,
    settings_text: Option<&str>,
) -> Result<Plan> {
    if !(1..=MAX_INTERVAL_MINUTES).contains(&interval_minutes) {
        bail!("--interval-minutes must be between 1 and {MAX_INTERVAL_MINUTES}");
    }
    let mut plan = Plan::default();
    if let Some(log_dir) = layout.log_path().parent() {
        plan.directories.push(log_dir.to_path_buf());
    }
    match layout.platform {
        Platform::MacOs => {
            let plist = layout.plist_path();
            let domain = format!("gui/{}", layout.uid);
            let service = format!("{domain}/{LAUNCHD_LABEL}");
            plan.directories.push(plist.parent().unwrap().to_path_buf());
            plan.writes.push((
                plist.clone(),
                launchd_plist(layout, interval_minutes).into_bytes(),
            ));
            plan.commands.push(PlannedCommand::new(
                &["launchctl", "bootout", &service],
                true,
            ));
            plan.commands.push(PlannedCommand::new(
                &["launchctl", "bootstrap", &domain, &plist.to_string_lossy()],
                false,
            ));
        }
        Platform::Linux => {
            let dir = layout.systemd_dir();
            plan.directories.push(dir.clone());
            plan.writes.push((
                dir.join(format!("{SYSTEMD_UNIT}.service")),
                systemd_service(layout).into_bytes(),
            ));
            plan.writes.push((
                dir.join(format!("{SYSTEMD_UNIT}.timer")),
                systemd_timer(interval_minutes).into_bytes(),
            ));
            plan.commands.push(PlannedCommand::new(
                &["systemctl", "--user", "daemon-reload"],
                false,
            ));
            plan.commands.push(PlannedCommand::new(
                &[
                    "systemctl",
                    "--user",
                    "enable",
                    "--now",
                    &format!("{SYSTEMD_UNIT}.timer"),
                ],
                false,
            ));
        }
        Platform::Windows => {
            plan.directories.push(layout.config_dir.clone());
            plan.writes
                .push((layout.vbs_path(), utf16le_with_bom(&windows_vbs(layout))));
            plan.commands
                .push(schtasks_create(layout, interval_minutes));
        }
    }
    if hooks && crate::guard::has_guard_hooks(settings_text) {
        bail!(
            "The Syncthing guard's hooks are installed in settings.json. Run `claude-code-sync \
             guard uninstall` first (or install autosync with --no-hooks): git mode and \
             Syncthing mode at once would fight over the same transcripts."
        );
    }
    let hook = hooks.then_some(layout.exe.as_path());
    plan.settings = settings_after_install(settings_text, hook)?;
    Ok(plan)
}

/// What `autosync uninstall` does on this layout.
pub fn uninstall_plan(layout: &Layout, settings_text: Option<&str>) -> Result<Plan> {
    let mut plan = Plan::default();
    match layout.platform {
        Platform::MacOs => {
            let service = format!("gui/{}/{LAUNCHD_LABEL}", layout.uid);
            plan.commands.push(PlannedCommand::new(
                &["launchctl", "bootout", &service],
                true,
            ));
            plan.removals.push(layout.plist_path());
        }
        Platform::Linux => {
            plan.commands.push(PlannedCommand::new(
                &[
                    "systemctl",
                    "--user",
                    "disable",
                    "--now",
                    &format!("{SYSTEMD_UNIT}.timer"),
                ],
                true,
            ));
            let dir = layout.systemd_dir();
            plan.removals
                .push(dir.join(format!("{SYSTEMD_UNIT}.service")));
            plan.removals
                .push(dir.join(format!("{SYSTEMD_UNIT}.timer")));
            plan.commands.push(PlannedCommand::new(
                &["systemctl", "--user", "daemon-reload"],
                true,
            ));
        }
        Platform::Windows => {
            plan.commands.push(PlannedCommand::new(
                &["schtasks", "/Delete", "/TN", WINDOWS_TASK, "/F"],
                true,
            ));
            plan.removals.push(layout.vbs_path());
        }
    }
    plan.settings = settings_after_uninstall(settings_text)?;
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Doing it
// ---------------------------------------------------------------------------

fn read_settings(layout: &Layout) -> Result<Option<String>> {
    match std::fs::read(&layout.claude_settings) {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes).with_context(|| {
            format!("{} is not UTF-8", layout.claude_settings.display())
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("Failed to read {}", layout.claude_settings.display()))
        }
    }
}

fn print_plan(plan: &Plan, layout: &Layout) {
    for dir in &plan.directories {
        println!("  {} {}", "mkdir".dimmed(), dir.display());
    }
    for (path, bytes) in &plan.writes {
        println!("  {} {}", "write".green(), path.display());
        if layout.platform == Platform::Windows && path.extension().is_some_and(|e| e == "vbs") {
            println!("{}", "    (UTF-16LE with BOM)".dimmed());
            for line in windows_vbs(layout).lines() {
                println!("    {line}");
            }
        } else {
            for line in String::from_utf8_lossy(bytes).lines() {
                println!("    {line}");
            }
        }
    }
    for path in &plan.removals {
        println!("  {} {}", "delete".red(), path.display());
    }
    for command in &plan.commands {
        println!("  {} {}", "run".cyan(), command.display());
    }
    if let Some((_, changes)) = &plan.settings {
        println!("  {} {}", "edit".green(), layout.claude_settings.display());
        for change in changes {
            println!("    {change}");
        }
    }
}

fn run_command(command: &PlannedCommand) -> Result<bool> {
    let output = std::process::Command::new(&command.args[0])
        .args(&command.args[1..])
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(true),
        Ok(output) if command.may_fail => {
            log::debug!(
                "{} failed (expected sometimes): {}",
                command.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(false)
        }
        Ok(output) => bail!(
            "{} failed: {}",
            command.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(_) if command.may_fail => Ok(false),
        Err(e) => Err(e).with_context(|| format!("Failed to run {}", command.display())),
    }
}

fn apply_files(plan: &Plan, layout: &Layout) -> Result<()> {
    for dir in &plan.directories {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    for (path, bytes) in &plan.writes {
        std::fs::write(path, bytes)
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    for path in &plan.removals {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to delete {}", path.display()))
            }
        }
    }
    if let Some((text, changes)) = &plan.settings {
        if let Some(parent) = layout.claude_settings.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // UTF-8 without a BOM: `String` bytes are exactly that.
        crate::branches::write_file(&layout.claude_settings, text.as_bytes())?;
        for change in changes {
            println!("  {} {}", "✓".green(), change);
        }
    }
    Ok(())
}

/// `autosync install`.
pub fn install(interval_minutes: u32, hooks: bool, dry_run: bool) -> Result<()> {
    let layout = Layout::detect()?;
    let settings = read_settings(&layout)?;
    let plan = install_plan(&layout, interval_minutes, hooks, settings.as_deref())?;

    if dry_run {
        println!("{}", "autosync install --dry-run would:".bold());
        print_plan(&plan, &layout);
        if layout.platform == Platform::Linux {
            println!(
                "  {} without systemd, it would print this crontab line instead:\n    {}",
                "note".dimmed(),
                crontab_line(&layout, interval_minutes)
            );
        }
        return Ok(());
    }

    if !crate::handlers::is_initialized()? {
        println!(
            "{} claude-code-sync is not initialized yet; autosync runs will do nothing until \
             `claude-code-sync init` is done.",
            "!".yellow()
        );
    }
    apply_files(&plan, &layout)?;

    let mut scheduled = true;
    for command in &plan.commands {
        let systemd = command.args.first().is_some_and(|c| c == "systemctl");
        match run_command(command) {
            Ok(_) => {}
            Err(e) if systemd => {
                log::warn!("{e:#}");
                scheduled = false;
                println!(
                    "{} systemd user services are not available here ({}). Add this line to \
                     your crontab (crontab -e) instead:\n  {}",
                    "!".yellow(),
                    e.to_string().lines().next().unwrap_or_default(),
                    crontab_line(&layout, interval_minutes)
                );
                break;
            }
            Err(e) => return Err(e),
        }
    }

    if scheduled {
        println!(
            "{} autosync every {interval_minutes} minute(s): {}",
            "✓".green(),
            match layout.platform {
                Platform::MacOs => format!("LaunchAgent {}", layout.plist_path().display()),
                Platform::Linux => format!("systemd user timer {SYSTEMD_UNIT}.timer"),
                Platform::Windows => format!("Task Scheduler task \"{WINDOWS_TASK}\""),
            }
        );
    }
    if hooks {
        println!(
            "{} every finished turn and every closed chat syncs right away; a chat left open \
             on an out-of-date version is guarded (hooks: {})",
            "✓".green(),
            HOOKS.map(|spec| spec.event).join(", ")
        );
    }
    println!("  Log: {}", layout.log_path().display());
    Ok(())
}

/// `autosync uninstall`.
pub fn uninstall(dry_run: bool) -> Result<()> {
    let layout = Layout::detect()?;
    let settings = read_settings(&layout)?;
    let plan = uninstall_plan(&layout, settings.as_deref())?;
    if dry_run {
        println!("{}", "autosync uninstall --dry-run would:".bold());
        print_plan(&plan, &layout);
        return Ok(());
    }
    for command in &plan.commands {
        run_command(command)?;
    }
    apply_files(&plan, &layout)?;
    println!(
        "{} autosync removed (cleanupPeriodDays is left as it is)",
        "✓".green()
    );
    Ok(())
}

/// `autosync status`.
pub fn status() -> Result<()> {
    let layout = Layout::detect()?;
    println!("{}", "=== Autosync ===".bold().cyan());
    let (installed, scheduler) = match layout.platform {
        Platform::MacOs => {
            let loaded = run_command(&PlannedCommand::new(
                &[
                    "launchctl",
                    "print",
                    &format!("gui/{}/{LAUNCHD_LABEL}", layout.uid),
                ],
                true,
            ))
            .unwrap_or(false);
            (
                layout.plist_path().is_file(),
                format!(
                    "LaunchAgent {} ({})",
                    layout.plist_path().display(),
                    if loaded { "loaded" } else { "not loaded" }
                ),
            )
        }
        Platform::Linux => {
            let active = run_command(&PlannedCommand::new(
                &[
                    "systemctl",
                    "--user",
                    "is-active",
                    "--quiet",
                    &format!("{SYSTEMD_UNIT}.timer"),
                ],
                true,
            ))
            .unwrap_or(false);
            (
                layout
                    .systemd_dir()
                    .join(format!("{SYSTEMD_UNIT}.timer"))
                    .is_file(),
                format!(
                    "systemd user timer {SYSTEMD_UNIT}.timer ({})",
                    if active { "active" } else { "inactive" }
                ),
            )
        }
        Platform::Windows => {
            let exists = run_command(&PlannedCommand::new(
                &["schtasks", "/Query", "/TN", WINDOWS_TASK],
                true,
            ))
            .unwrap_or(false);
            (
                exists,
                format!(
                    "Task Scheduler task \"{WINDOWS_TASK}\" ({})",
                    if exists { "present" } else { "absent" }
                ),
            )
        }
    };
    println!(
        "  Timer: {}",
        if installed {
            scheduler.green()
        } else {
            "not installed".yellow()
        }
    );

    let settings: Option<Value> = read_settings(&layout)?
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok());
    for spec in &HOOKS {
        let hooked = settings
            .as_ref()
            .and_then(|s| s.pointer(&format!("/hooks/{}", spec.event)))
            .and_then(|groups| groups.as_array())
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|hooks| hooks.iter().any(|h| is_our_hook(spec.event, h, None)))
                })
            });
        println!(
            "  {} hook: {}",
            spec.event,
            if hooked {
                "installed".green()
            } else {
                "not installed".yellow()
            }
        );
    }
    let retention = settings
        .as_ref()
        .and_then(|s| s.get("cleanupPeriodDays"))
        .and_then(|d| d.as_u64());
    println!(
        "  Claude Code retention: {}",
        match retention {
            Some(days) if days >= RETENTION_DAYS => format!("{days} days").green(),
            Some(days) => format!("{days} days (older transcripts are deleted locally)").yellow(),
            None => "30 days (default; older transcripts are deleted locally)".yellow(),
        }
    );
    let running = crate::sync_lock::SyncLock::try_acquire()?.is_none();
    if running {
        println!("  {}", "A sync is running right now".cyan());
    }
    println!("  Log: {}", layout.log_path().display());
    Ok(())
}

/// `autosync run`: one non-interactive sync, unless another run holds the
/// lock. With `detach`, start that in the background and return at once.
///
/// A run that finds the lock taken leaves the flag `sync.rerun` and goes:
/// the holder, once its sync is done, syncs again (at most
/// [`crate::sync_lock::MAX_EXTRA_ROUNDS`] more times), so a turn that
/// finished during a sync is pushed without waiting for the timer.
pub fn run(detach: bool) -> Result<()> {
    if detach {
        return spawn_detached();
    }
    let lock = match crate::sync_lock::SyncLock::try_acquire()? {
        Some(lock) => lock,
        None => {
            crate::sync_lock::request_rerun()?;
            // The holder may have finished between the two calls: then its
            // last look at the flag is past, and this run does the sync.
            match crate::sync_lock::SyncLock::try_acquire()? {
                Some(lock) => lock,
                None => {
                    log::info!("autosync: another sync is running; it will sync again when done");
                    return Ok(());
                }
            }
        }
    };
    if !crate::handlers::is_initialized()? {
        log::info!("autosync: not initialized (run `claude-code-sync init`); nothing to do");
        return Ok(());
    }

    std::env::set_var(crate::interactive_conflict::NONINTERACTIVE_ENV, "1");

    // Whoever asked before this run took the lock is served by it.
    crate::sync_lock::take_rerun()?;
    let rounds = sync_with_reruns(|| {
        println!(
            "--- autosync {} ---",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        crate::sync::sync_bidirectional(None, None, false, false, crate::VerbosityLevel::Quiet)
    })?;
    log::debug!("autosync: {rounds} round(s)");
    drop(lock);
    Ok(())
}

/// Run `sync`, then again for as long as another run asked for it while it
/// ran, up to [`crate::sync_lock::MAX_EXTRA_ROUNDS`] more. Returns the
/// number of rounds run. Held under the sync lock.
fn sync_with_reruns(mut sync: impl FnMut() -> Result<()>) -> Result<u32> {
    let mut rounds = 1;
    sync()?;
    while rounds <= crate::sync_lock::MAX_EXTRA_ROUNDS && crate::sync_lock::take_rerun()? {
        rounds += 1;
        sync()?;
    }
    Ok(rounds)
}

/// Start `autosync run` in its own process group, output appended to the
/// log, and return without waiting: what the `SessionEnd` hook and the
/// Windows task launch.
pub fn spawn_detached() -> Result<()> {
    let layout = Layout::detect()?;
    let log_path = layout.log_path();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open {}", log_path.display()))?;

    let spawn = |leave_job: bool| -> std::io::Result<std::process::Child> {
        let mut command = std::process::Command::new(&layout.exe);
        command
            .args(["autosync", "run"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?);
        detach(&mut command, leave_job);
        command.spawn()
    };
    // Leaving the parent's job object is refused when the job forbids it;
    // then the child stays in it, as any other would.
    spawn(true)
        .or_else(|_| spawn(false))
        .with_context(|| format!("Failed to start {}", layout.exe.display()))?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command, _leave_job: bool) {
    use std::os::unix::process::CommandExt;
    // Its own session: closing the terminal or ending Claude Code does not
    // take the sync down with it.
    // SAFETY: setsid(2) is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command, leave_job: bool) {
    use std::os::windows::process::CommandExt;
    // Its own console, with no window, and its own process group: Claude
    // Code's console closing or a Ctrl+C there never reaches it, and the git
    // it starts inherits the windowless console, so nothing flashes on
    // screen. Not DETACHED_PROCESS: Windows ignores CREATE_NO_WINDOW beside
    // it, and a process with no console makes every console program it
    // starts (git, at each step of a sync) open a window of its own.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    if leave_job {
        // Out of a job Claude Code may run its hooks in, which can be set
        // to end every process in it when Claude Code exits.
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    command.creation_flags(flags);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut std::process::Command, _leave_job: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(platform: Platform) -> Layout {
        match platform {
            Platform::MacOs => Layout {
                platform,
                exe: PathBuf::from("/Users/mateo/.cargo/bin/claude-code-sync"),
                home: PathBuf::from("/Users/mateo"),
                config_dir: PathBuf::from(
                    "/Users/mateo/Library/Application Support/claude-code-sync",
                ),
                xdg_config_home: PathBuf::from("/Users/mateo/.config"),
                claude_settings: PathBuf::from("/Users/mateo/.claude/settings.json"),
                uid: 501,
                env: vec![(
                    "PATH".to_string(),
                    "/opt/homebrew/bin:/usr/bin:/bin".to_string(),
                )],
            },
            Platform::Linux => Layout {
                platform,
                exe: PathBuf::from("/home/mateo/bin/claude code sync"),
                home: PathBuf::from("/home/mateo"),
                config_dir: PathBuf::from("/home/mateo/.config/claude-code-sync"),
                xdg_config_home: PathBuf::from("/home/mateo/.config"),
                claude_settings: PathBuf::from("/home/mateo/.claude/settings.json"),
                uid: 1000,
                env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
            },
            // Stand-ins: the Windows path text is what matters, and a
            // `PathBuf` keeps it verbatim on any platform.
            Platform::Windows => Layout {
                platform,
                exe: PathBuf::from(
                    r"C:\Users\Mateo Bodenlle\AppData\Local\Programs\claude-code-sync\claude-code-sync.exe",
                ),
                home: PathBuf::from(r"C:\Users\Mateo Bodenlle"),
                config_dir: PathBuf::from(r"C:\Users\Iñaki\AppData\Roaming\claude-code-sync"),
                xdg_config_home: PathBuf::from(r"C:\Users\Mateo Bodenlle\.config"),
                claude_settings: PathBuf::from(r"C:\Users\Mateo Bodenlle\.claude\settings.json"),
                uid: 0,
                env: Vec::new(),
            },
        }
    }

    // launchd/systemd/cron files carry Unix paths; only built on Unix.
    #[cfg(unix)]
    #[test]
    fn the_launch_agent_runs_at_load_and_every_interval_with_a_log() {
        let mut mac = layout(Platform::MacOs);
        mac.env.push((
            "CLAUDE_CODE_SYNC_CONFIG_DIR".to_string(),
            "/a&b".to_string(),
        ));
        let plist = launchd_plist(&mac, 15);

        assert!(plist.contains("<string>io.github.claude-code-sync.autosync</string>"));
        assert!(plist.contains(
            "<array>\n\t\t<string>/Users/mateo/.cargo/bin/claude-code-sync</string>\n\
             \t\t<string>autosync</string>\n\t\t<string>run</string>\n\t</array>"
        ));
        assert!(plist.contains("<key>StartInterval</key>\n\t<integer>900</integer>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains(
            "<key>StandardOutPath</key>\n\t<string>/Users/mateo/Library/Logs/claude-code-sync.log</string>"
        ));
        assert!(plist.contains("<key>StandardErrorPath</key>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin:/bin</string>"));
        assert!(plist.contains("<string>/a&amp;b</string>"), "XML-escaped");
    }

    // launchd/systemd/cron files carry Unix paths; only built on Unix.
    #[cfg(unix)]
    #[test]
    fn the_mac_install_bootstraps_the_agent_in_the_gui_domain() {
        let plan = install_plan(&layout(Platform::MacOs), 10, true, None).unwrap();
        let commands: Vec<String> = plan.commands.iter().map(PlannedCommand::display).collect();
        assert_eq!(
            commands,
            vec![
                "launchctl bootout gui/501/io.github.claude-code-sync.autosync",
                "launchctl bootstrap gui/501 /Users/mateo/Library/LaunchAgents/io.github.claude-code-sync.autosync.plist",
            ]
        );
        assert!(plan.commands[0].may_fail && !plan.commands[1].may_fail);
        assert_eq!(
            plan.writes[0].0,
            PathBuf::from(
                "/Users/mateo/Library/LaunchAgents/io.github.claude-code-sync.autosync.plist"
            )
        );

        let removal = uninstall_plan(&layout(Platform::MacOs), None).unwrap();
        assert_eq!(removal.removals, vec![plan.writes[0].0.clone()]);
        assert_eq!(
            removal.commands[0].display(),
            "launchctl bootout gui/501/io.github.claude-code-sync.autosync"
        );
    }

    // launchd/systemd/cron files carry Unix paths; only built on Unix.
    #[cfg(unix)]
    #[test]
    fn the_systemd_units_run_one_sync_on_a_timer() {
        let linux = layout(Platform::Linux);
        let service = systemd_service(&linux);
        assert!(service.contains("Type=oneshot\n"));
        assert!(service.contains("ExecStart=\"/home/mateo/bin/claude code sync\" autosync run\n"));
        assert!(service.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(service
            .contains("StandardOutput=append:/home/mateo/.config/claude-code-sync/autosync.log\n"));

        let timer = systemd_timer(10);
        assert!(timer.contains("OnUnitActiveSec=10min\n"));
        assert!(timer.contains("Unit=claude-code-sync-autosync.service\n"));
        assert!(timer.contains("WantedBy=timers.target\n"));

        let plan = install_plan(&linux, 10, false, None).unwrap();
        let commands: Vec<String> = plan.commands.iter().map(PlannedCommand::display).collect();
        assert_eq!(
            commands,
            vec![
                "systemctl --user daemon-reload",
                "systemctl --user enable --now claude-code-sync-autosync.timer",
            ]
        );
        assert_eq!(
            plan.writes
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/home/mateo/.config/systemd/user/claude-code-sync-autosync.service"),
                PathBuf::from("/home/mateo/.config/systemd/user/claude-code-sync-autosync.timer"),
            ]
        );
    }

    #[test]
    fn a_percent_sign_is_escaped_in_systemd_units() {
        assert_eq!(systemd_quote(r#"a%b"c\d"#), r#""a%%b\"c\\d""#);
    }

    // launchd/systemd/cron files carry Unix paths; only built on Unix.
    #[cfg(unix)]
    #[test]
    fn without_systemd_a_crontab_line_does_the_same() {
        let linux = layout(Platform::Linux);
        assert_eq!(
            crontab_line(&linux, 10),
            "*/10 * * * * '/home/mateo/bin/claude code sync' autosync run >> \
             '/home/mateo/.config/claude-code-sync/autosync.log' 2>&1"
        );
        assert!(crontab_line(&linux, 120).starts_with("0 */2 * * * "));
    }

    #[test]
    fn the_windows_task_runs_a_hidden_vbs_with_paths_quoted() {
        let windows = layout(Platform::Windows);
        let vbs = windows_vbs(&windows);
        assert!(vbs.contains(
            "CreateObject(\"WScript.Shell\").Run \"\"\"C:\\Users\\Mateo Bodenlle\\AppData\\Local\\\
             Programs\\claude-code-sync\\claude-code-sync.exe\"\" autosync run --detach\", 0, False\r\n"
        ));

        let bytes = utf16le_with_bom(&vbs);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "a byte-order mark");
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), vbs);

        // `PathBuf::join` uses this platform's separator; on Windows the
        // path is all backslashes. What is checked here is the quoting.
        let vbs_path = windows.vbs_path();
        assert!(vbs_path.starts_with(r"C:\Users\Iñaki\AppData\Roaming\claude-code-sync"));
        let create = schtasks_create(&windows, 10);
        assert_eq!(
            create.args,
            vec![
                "schtasks".to_string(),
                "/Create".to_string(),
                "/SC".to_string(),
                "MINUTE".to_string(),
                "/MO".to_string(),
                "10".to_string(),
                "/TN".to_string(),
                "claude-code-sync autosync".to_string(),
                "/TR".to_string(),
                format!("wscript.exe \"{}\"", vbs_path.display()),
                "/F".to_string(),
            ]
        );

        let plan = install_plan(&windows, 10, true, None).unwrap();
        assert_eq!(plan.writes[0].0, vbs_path);
        assert_eq!(&plan.writes[0].1[..2], &[0xFF, 0xFE]);
        let removal = uninstall_plan(&windows, None).unwrap();
        assert_eq!(
            removal.commands[0].args,
            vec![
                "schtasks",
                "/Delete",
                "/TN",
                "claude-code-sync autosync",
                "/F"
            ]
        );
    }

    #[test]
    fn an_interval_out_of_range_is_refused() {
        assert!(install_plan(&layout(Platform::MacOs), 0, true, None).is_err());
        assert!(install_plan(&layout(Platform::Windows), 1440, true, None).is_err());
    }

    #[test]
    fn the_hooks_run_the_binary_without_a_shell() {
        let exe =
            r"C:\Users\Mateo Bodenlle\AppData\Local\Programs\claude-code-sync\claude-code-sync.exe";
        let expected = [
            ("UserPromptSubmit", ["hook", "prompt"], 5),
            ("SessionStart", ["hook", "session-start"], 5),
            ("SessionEnd", ["hook", "session-end"], 10),
            ("Stop", ["hook", "stop"], 5),
        ];
        assert_eq!(HOOKS.len(), expected.len());
        for (spec, (event, args, timeout)) in HOOKS.iter().zip(expected) {
            assert_eq!(spec.event, event);
            assert_eq!(
                hook_entry(Path::new(exe), spec),
                serde_json::json!({
                    "type": "command",
                    "command": exe,
                    "args": args,
                    "timeout": timeout,
                })
            );
        }
        let unusual = "/home/o'neil/Iñaki $HOME/claude-code-sync";
        assert_eq!(
            hook_entry(Path::new(unusual), &HOOKS[0])["command"],
            unusual,
            "the path as it is: nothing to quote without a shell"
        );
    }

    const EXISTING: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": [
      "Bash(git status)"
    ]
  },
  "cleanupPeriodDays": 30,
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "say bye"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "check.sh"
          }
        ]
      }
    ]
  },
  "zeta": true
}
"#;

    /// Each hook of `event` as one line: its command, then its arguments.
    fn hook_commands(text: &str, event: &str) -> Vec<String> {
        let value: Value = serde_json::from_str(text).unwrap();
        value["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                    .map(|h| {
                        let mut line = h["command"].as_str().unwrap().to_string();
                        for arg in h["args"].as_array().into_iter().flatten() {
                            line.push(' ');
                            line.push_str(arg.as_str().unwrap());
                        }
                        line
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `SessionEnd` hooks only.
    fn session_end(text: &str) -> Vec<String> {
        hook_commands(text, "SessionEnd")
    }

    #[test]
    fn the_install_adds_our_hooks_beside_the_others_and_keeps_transcripts() {
        let ours = Path::new("/x/claude-code-sync");
        let (text, changes) = settings_after_install(Some(EXISTING), Some(ours))
            .unwrap()
            .unwrap();

        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["cleanupPeriodDays"], 3650);
        assert_eq!(
            session_end(&text),
            vec!["say bye", "/x/claude-code-sync hook session-end"]
        );
        for (event, line) in [
            ("UserPromptSubmit", "/x/claude-code-sync hook prompt"),
            ("SessionStart", "/x/claude-code-sync hook session-start"),
            ("Stop", "/x/claude-code-sync hook stop"),
        ] {
            assert_eq!(hook_commands(&text, event), vec![line], "{event}");
        }
        let added = &value["hooks"]["SessionEnd"][1]["hooks"][0];
        assert_eq!(
            added,
            &hook_entry(ours, &HOOKS[2]),
            "the exec form, no shell"
        );
        assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(value["model"], "opus");
        assert_eq!(changes.len(), 5, "{changes:?}");
        assert!(changes[0].contains("cleanupPeriodDays 30 -> 3650"));
        assert!(changes[1].starts_with("UserPromptSubmit hook added"));

        // Members keep their order and untouched ones their exact text.
        let order: Vec<usize> = [
            "\"model\"",
            "\"permissions\"",
            "\"cleanupPeriodDays\"",
            "\"hooks\"",
            "\"zeta\"",
        ]
        .iter()
        .map(|key| text.find(key).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(text.starts_with(
            "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(git status)\"\n    ]\n  },\n  \"cleanupPeriodDays\": 3650,"
        ));
        assert!(text.ends_with("  \"zeta\": true\n}\n"));
        assert!(!text.starts_with('\u{feff}'));

        // A second install changes nothing; a new binary path replaces ours.
        assert_eq!(
            settings_after_install(Some(&text), Some(ours)).unwrap(),
            None
        );
        let moved = Path::new("/y/claude-code-sync");
        let (moved_text, _) = settings_after_install(Some(&text), Some(moved))
            .unwrap()
            .unwrap();
        assert_eq!(
            session_end(&moved_text),
            vec!["say bye", "/y/claude-code-sync hook session-end"]
        );
        assert_eq!(
            hook_commands(&moved_text, "Stop"),
            vec!["/y/claude-code-sync hook stop"]
        );

        // Uninstall removes ours only and leaves the retention raised.
        let (after, changes) = settings_after_uninstall(Some(&text)).unwrap().unwrap();
        let value: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(changes.len(), 4, "{changes:?}");
        assert_eq!(session_end(&after), vec!["say bye"]);
        for event in ["UserPromptSubmit", "SessionStart", "Stop"] {
            assert!(value["hooks"].get(event).is_none(), "{event} is gone");
        }
        assert_eq!(value["cleanupPeriodDays"], 3650);
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "check.sh"
        );
        assert_eq!(settings_after_uninstall(Some(&after)).unwrap(), None);
    }

    #[test]
    fn the_shell_form_an_earlier_build_wrote_is_replaced_and_removed() {
        let legacy = r#"{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "'/x/claude-code-sync' autosync run --detach",
            "timeout": 30
          },
          {
            "type": "command",
            "command": "say bye"
          }
        ]
      }
    ]
  },
  "cleanupPeriodDays": 3650
}
"#;
        let (installed, changes) =
            settings_after_install(Some(legacy), Some(Path::new("/x/claude-code-sync")))
                .unwrap()
                .unwrap();
        assert_eq!(
            session_end(&installed),
            vec!["say bye", "/x/claude-code-sync hook session-end"],
            "the old detached run is replaced, not kept beside the new hook"
        );
        let session_end_change = changes
            .iter()
            .find(|c| c.starts_with("SessionEnd"))
            .expect("a SessionEnd change");
        assert!(session_end_change.contains("updated"), "{changes:?}");

        let (uninstalled, _) = settings_after_uninstall(Some(legacy)).unwrap().unwrap();
        assert_eq!(session_end(&uninstalled), vec!["say bye"]);
        let (both_gone, _) = settings_after_uninstall(Some(&installed)).unwrap().unwrap();
        assert_eq!(session_end(&both_gone), vec!["say bye"]);
    }

    #[test]
    fn a_longer_retention_is_left_alone_and_no_hooks_still_raises_a_short_one() {
        let long = "{\n  \"cleanupPeriodDays\": 5000\n}\n";
        assert_eq!(settings_after_install(Some(long), None).unwrap(), None);

        let (text, changes) = settings_after_install(Some("{}"), None).unwrap().unwrap();
        assert_eq!(text, "{\n  \"cleanupPeriodDays\": 3650\n}");
        assert_eq!(changes.len(), 1, "{changes:?}");

        let (fresh, _) = settings_after_install(None, Some(Path::new("/x/claude-code-sync")))
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&fresh).unwrap();
        assert_eq!(value["cleanupPeriodDays"], 3650);
        assert_eq!(
            session_end(&fresh),
            vec!["/x/claude-code-sync hook session-end"]
        );
    }

    #[test]
    fn a_byte_order_mark_is_dropped_and_no_hooks_removes_ours() {
        let with_ours = settings_after_install(
            Some("\u{feff}{\"cleanupPeriodDays\": 3650}"),
            Some(Path::new("/x/claude-code-sync")),
        )
        .unwrap()
        .unwrap()
        .0;
        assert!(!with_ours.starts_with('\u{feff}'));
        let (without, changes) = settings_after_install(Some(&with_ours), None)
            .unwrap()
            .unwrap();
        for event in ["UserPromptSubmit", "SessionStart", "SessionEnd", "Stop"] {
            assert!(hook_commands(&without, event).is_empty(), "{event}");
        }
        assert_eq!(changes.len(), 4, "{changes:?}");
        assert!(changes[0].contains("removed"));
        assert!(!without.contains("\"hooks\""), "{without}");
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_is_refused() {
        assert!(settings_after_install(Some("[1]"), None).is_err());
        assert!(settings_after_install(
            Some("{\"hooks\": 3}"),
            Some(Path::new("/x/claude-code-sync"))
        )
        .is_err());
    }

    #[test]
    fn the_exec_form_of_0_5_0_is_replaced_and_other_tools_hooks_are_not_touched() {
        let before = r#"{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/old/claude-code-sync",
            "args": ["autosync", "run", "--detach"],
            "timeout": 10
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "/usr/bin/other-tool", "args": ["hook", "prompt"] },
          { "type": "command", "command": "notify-send done" }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "/usr/bin/other-tool", "args": ["hook", "prompt"] }
        ]
      }
    ]
  },
  "cleanupPeriodDays": 3650
}
"#;
        let (text, _) =
            settings_after_install(Some(before), Some(Path::new("/new/claude-code-sync")))
                .unwrap()
                .unwrap();
        assert_eq!(
            session_end(&text),
            vec!["/new/claude-code-sync hook session-end"]
        );
        assert_eq!(
            hook_commands(&text, "Stop"),
            vec![
                "/usr/bin/other-tool hook prompt",
                "notify-send done",
                "/new/claude-code-sync hook stop"
            ],
            "another tool's `hook prompt` under Stop is not ours"
        );
        assert_eq!(
            hook_commands(&text, "UserPromptSubmit"),
            vec![
                "/usr/bin/other-tool hook prompt",
                "/new/claude-code-sync hook prompt"
            ],
            "ours goes beside theirs, whatever its arguments"
        );
        let (after, _) = settings_after_uninstall(Some(&text)).unwrap().unwrap();
        assert!(session_end(&after).is_empty());
        assert_eq!(
            hook_commands(&after, "Stop"),
            vec!["/usr/bin/other-tool hook prompt", "notify-send done"]
        );
        assert_eq!(
            hook_commands(&after, "UserPromptSubmit"),
            vec!["/usr/bin/other-tool hook prompt"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_sync_is_repeated_for_a_request_made_while_it_ran_up_to_three_times() {
        let config = tempfile::TempDir::new().unwrap();
        let previous = std::env::var("CLAUDE_CODE_SYNC_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", config.path());

        // No request: one round.
        let mut calls = 0;
        assert_eq!(
            sync_with_reruns(|| {
                calls += 1;
                Ok(())
            })
            .unwrap(),
            1
        );
        assert_eq!(calls, 1);

        // One request during the first round: two rounds.
        let mut calls = 0;
        let rounds = sync_with_reruns(|| {
            calls += 1;
            if calls == 1 {
                crate::sync_lock::request_rerun()?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!((rounds, calls), (2, 2));

        // A request during every round: the first and three more, no more.
        let mut calls = 0;
        let rounds = sync_with_reruns(|| {
            calls += 1;
            crate::sync_lock::request_rerun()?;
            Ok(())
        })
        .unwrap();
        assert_eq!((rounds, calls), (4, 4));
        assert!(
            crate::sync_lock::take_rerun().unwrap(),
            "the last request waits for the timer"
        );

        // A failed sync stops at once.
        assert!(sync_with_reruns(|| anyhow::bail!("boom")).is_err());

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_run_that_finds_the_lock_taken_leaves_the_flag_and_goes() {
        let config = tempfile::TempDir::new().unwrap();
        let previous = std::env::var("CLAUDE_CODE_SYNC_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", config.path());

        let holder = crate::sync_lock::SyncLock::try_acquire().unwrap().unwrap();
        run(false).unwrap();
        assert!(
            crate::sync_lock::take_rerun().unwrap(),
            "the run asked the holder for another round"
        );
        drop(holder);

        match previous {
            Some(v) => std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR"),
        }
    }
}
