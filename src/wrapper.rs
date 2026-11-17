//! The `claude` shell function `guard install` puts in the shell profiles.
//!
//! It starts the real Claude Code with `CLAUDE_GUARD_SHELL` naming its shell
//! (for that one child only) and, when Claude returns, asks `guard
//! take-reload` whether the prompt hook ended it to reload a stale chat; if
//! so it starts Claude again with the arguments that printed (see
//! [`crate::reload`]). At most 5 times in a row.
//!
//! The function lives between two marker lines, so installing replaces it
//! and uninstalling removes it, and nothing else in a profile is ever
//! touched. Files are handled as bytes: their encoding, byte order mark and
//! line endings stay as they are.

use anyhow::{bail, Context, Result};
use base64::Engine;
use colored::Colorize;
use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> claude-code-sync guard >>>";
pub const END: &str = "# <<< claude-code-sync guard <<<";

/// Tests only: the profile files to use instead of the real ones, separated
/// like `PATH` (`.ps1` files get the PowerShell function, the others the
/// bash/zsh one).
pub const PROFILE_FILES_ENV: &str = "CLAUDE_CODE_SYNC_PROFILE_FILES";

/// Which language a profile is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    PowerShell,
    /// bash and zsh
    Posix,
}

/// A profile file the function goes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub path: PathBuf,
    pub flavor: Flavor,
}

// ---------------------------------------------------------------------------
// The function
// ---------------------------------------------------------------------------

/// `'` -> `'\''`, inside single quotes.
fn posix_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The binary's path as the POSIX function runs it. Git Bash on Windows
/// accepts `C:/dir/file.exe`, not backslashes in a single-quoted word.
fn posix_exe(exe: &Path) -> String {
    let text = exe.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// A PowerShell expression for a string: a single-quoted literal, or, when
/// it has non-ASCII characters, decoded from base64 (Windows PowerShell 5.1
/// reads a profile without a byte order mark as ANSI).
fn powershell_string(text: &str) -> String {
    if text.is_ascii() {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        format!("[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))")
    }
}

/// The function for bash and zsh, markers included, ending in a newline.
pub fn posix_block(exe: &Path) -> String {
    let exe = posix_quote(&posix_exe(exe));
    format!(
        r#"{BEGIN}
# Reopens a stale Claude Code window on its chat. Added by
# `claude-code-sync guard install`; `guard uninstall` removes it.
claude() {{
  local _ccs_real _ccs_id _ccs_args _ccs_next _ccs_a _ccs_i _ccs_rc _ccs_print
  if [ -n "${{ZSH_VERSION:-}}" ]; then
    _ccs_real=$(whence -p claude)
  else
    _ccs_real=$(type -P claude)
  fi
  if [ -z "$_ccs_real" ]; then
    echo 'claude: not found' >&2
    return 127
  fi
  _ccs_print=0
  for _ccs_a in "$@"; do
    case "$_ccs_a" in -p|--print) _ccs_print=1 ;; esac
  done
  if [ "$_ccs_print" = 1 ]; then
    "$_ccs_real" "$@"
    return $?
  fi
  _ccs_id=$$
  if [ -r "/proc/$$/winpid" ]; then
    _ccs_id=$(cat "/proc/$$/winpid")
  fi
  _ccs_args=("$@")
  _ccs_i=0
  while [ "$_ccs_i" -lt 5 ]; do
    _ccs_i=$((_ccs_i + 1))
    CLAUDE_GUARD_SHELL=$_ccs_id "$_ccs_real" "${{_ccs_args[@]}}"
    _ccs_rc=$?
    _ccs_next=()
    while IFS= read -r -d '' _ccs_a; do
      _ccs_next+=("$_ccs_a")
    done < <({exe} guard take-reload --shell "$_ccs_id" --format nul -- "$@")
    [ "${{#_ccs_next[@]}}" -gt 0 ] || break
    _ccs_args=("${{_ccs_next[@]}}")
  done
  if [ -t 0 ]; then
    stty sane 2>/dev/null
  fi
  return $_ccs_rc
}}
{END}
"#
    )
}

/// The function for Windows PowerShell 5.1 and PowerShell 7+, markers
/// included, ending in a newline.
///
/// The new arguments come back as one base64 line per argument: unlike JSON
/// they cannot be altered on the way (PowerShell 7 before 7.5 turns a string
/// that looks like a date into a `DateTime`), and they are ASCII, so no
/// console code page garbles them.
pub fn powershell_block(exe: &Path) -> String {
    let exe = powershell_string(&exe.to_string_lossy());
    format!(
        r#"{BEGIN}
# Reopens a stale Claude Code window on its chat. Added by
# `claude-code-sync guard install`; `guard uninstall` removes it.
function claude {{
  $real = (Get-Command -CommandType Application claude -ErrorAction SilentlyContinue | Select-Object -First 1).Source
  if (-not $real) {{ Write-Error 'claude: not found'; return }}
  $orig = @($args)
  if (($orig -contains '-p') -or ($orig -contains '--print')) {{ & $real @orig; return }}
  $ccs = {exe}
  $legacy = ($PSVersionTable.PSVersion.Major -lt 7) -or (($PSVersionTable.PSVersion.Major -eq 7) -and ($PSVersionTable.PSVersion.Minor -lt 3)) -or ($PSNativeCommandArgumentPassing -eq 'Legacy')
  $extra = @()
  if ($legacy) {{ $extra = @('--ps-legacy') }}
  $argv = $orig
  for ($i = 0; $i -lt 5; $i++) {{
    $env:CLAUDE_GUARD_SHELL = "$PID"
    try {{ & $real @argv }} finally {{ Remove-Item Env:CLAUDE_GUARD_SHELL -ErrorAction SilentlyContinue }}
    $next = & $ccs guard take-reload --shell $PID --format b64 @extra -- @orig
    if (-not $next) {{ break }}
    $argv = @($next | ForEach-Object {{ [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($_)) }})
  }}
}}
{END}
"#
    )
}

/// The function for `profile`.
pub fn block_for(profile: &Profile, exe: &Path) -> String {
    match profile.flavor {
        Flavor::PowerShell => powershell_block(exe),
        Flavor::Posix => posix_block(exe),
    }
}

// ---------------------------------------------------------------------------
// Editing a profile
// ---------------------------------------------------------------------------

/// The bytes of each line of `bytes`, with the offset it starts at and the
/// offset after its newline.
fn lines(bytes: &[u8]) -> Vec<(usize, usize, &[u8])> {
    let mut found = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| start + p + 1);
        let mut content = &bytes[start..end];
        if let Some(rest) = content.strip_suffix(b"\n") {
            content = rest;
        }
        if let Some(rest) = content.strip_suffix(b"\r") {
            content = rest;
        }
        found.push((start, end, content));
        start = end;
    }
    found
}

/// Where the marked block is: from its first byte to after its last line's
/// newline. `None` when there is none; an error when only one marker is
/// there.
fn find_block(bytes: &[u8], path: &Path) -> Result<Option<(usize, usize)>> {
    let all = lines(bytes);
    let begin = all.iter().position(|l| l.2 == BEGIN.as_bytes());
    let Some(begin) = begin else {
        if all.iter().any(|l| l.2 == END.as_bytes()) {
            bail!(
                "{} has the end marker of the claude-code-sync guard block but not its start; fix it by hand",
                path.display()
            );
        }
        return Ok(None);
    };
    let Some(end) = all[begin..].iter().position(|l| l.2 == END.as_bytes()) else {
        bail!(
            "{} has the start marker of the claude-code-sync guard block but not its end; fix it by hand",
            path.display()
        );
    };
    Ok(Some((all[begin].0, all[begin + end].1)))
}

/// `original` with `block` in it: replacing the marked block there, or after
/// everything else (a blank line apart). Line endings follow the file's.
pub fn with_block(original: &[u8], block: &str, path: &Path) -> Result<Vec<u8>> {
    let crlf = original.windows(2).any(|w| w == b"\r\n");
    let block = if crlf {
        block.replace('\n', "\r\n")
    } else {
        block.to_string()
    };
    let newline: &[u8] = if crlf { b"\r\n" } else { b"\n" };
    let mut out = Vec::with_capacity(original.len() + block.len() + 4);
    match find_block(original, path)? {
        Some((from, to)) => {
            out.extend_from_slice(&original[..from]);
            out.extend_from_slice(block.as_bytes());
            out.extend_from_slice(&original[to..]);
        }
        None => {
            out.extend_from_slice(original);
            if !original.is_empty() {
                if !original.ends_with(b"\n") {
                    out.extend_from_slice(newline);
                }
                out.extend_from_slice(newline);
            }
            out.extend_from_slice(block.as_bytes());
        }
    }
    Ok(out)
}

/// `original` without the marked block (and the blank line before it, which
/// [`with_block`] added). `None` when there is no block.
pub fn without_block(original: &[u8], path: &Path) -> Result<Option<Vec<u8>>> {
    let Some((mut from, to)) = find_block(original, path)? else {
        return Ok(None);
    };
    let before = &original[..from];
    for blank in [&b"\r\n"[..], &b"\n"[..]] {
        // A blank line is a newline that follows another newline (or the
        // start of the file is not touched).
        if before.ends_with(blank) {
            let rest = &before[..before.len() - blank.len()];
            if rest.ends_with(b"\n") {
                from -= blank.len();
                break;
            }
        }
    }
    let mut out = original[..from].to_vec();
    out.extend_from_slice(&original[to..]);
    Ok(Some(out))
}

/// Whether `bytes` has the block.
pub fn has_block(bytes: &[u8]) -> bool {
    find_block(bytes, Path::new("")).ok().flatten().is_some()
}

// ---------------------------------------------------------------------------
// Where profiles are
// ---------------------------------------------------------------------------

fn flavor_of(path: &Path) -> Flavor {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ps1"))
    {
        Flavor::PowerShell
    } else {
        Flavor::Posix
    }
}

/// Whether `name` is an executable on `PATH`.
fn on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}

/// The profile files the function goes into.
///
/// Windows: `profile.ps1` of Windows PowerShell and of PowerShell 7, under
/// the Documents folder (OneDrive's, when it redirects it). Elsewhere:
/// `~/.bashrc` and `~/.zshrc`, for the shells that exist.
pub fn profiles() -> Result<Vec<Profile>> {
    if let Some(list) = std::env::var_os(PROFILE_FILES_ENV).filter(|v| !v.is_empty()) {
        return Ok(std::env::split_paths(&list)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|path| Profile {
                flavor: flavor_of(&path),
                path,
            })
            .collect());
    }
    if cfg!(windows) {
        let documents = dirs::document_dir().context("Cannot tell where Documents is")?;
        return Ok(["WindowsPowerShell", "PowerShell"]
            .into_iter()
            .map(|dir| Profile {
                path: documents.join(dir).join("profile.ps1"),
                flavor: Flavor::PowerShell,
            })
            .collect());
    }
    let home =
        crate::sync::discovery::machine_home_dir().context("Failed to get home directory")?;
    let mut found = Vec::new();
    for (shell, rc) in [("bash", ".bashrc"), ("zsh", ".zshrc")] {
        if on_path(shell) {
            found.push(Profile {
                path: home.join(rc),
                flavor: Flavor::Posix,
            });
        }
    }
    Ok(found)
}

fn read_profile(path: &Path) -> Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// What installing did to one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Added,
    Updated,
    Unchanged,
}

/// Put the function in `profile` (creating the file and its folder).
pub fn install_in(profile: &Profile, exe: &Path, dry_run: bool) -> Result<Outcome> {
    let original = read_profile(&profile.path)?;
    let updated = with_block(&original, &block_for(profile, exe), &profile.path)?;
    let outcome = if updated == original {
        Outcome::Unchanged
    } else if has_block(&original) {
        Outcome::Updated
    } else {
        Outcome::Added
    };
    if outcome != Outcome::Unchanged && !dry_run {
        crate::branches::write_file(&profile.path, &updated)?;
    }
    Ok(outcome)
}

/// Take the function out of `profile`; whether it was there.
pub fn uninstall_from(profile: &Profile, dry_run: bool) -> Result<bool> {
    let original = read_profile(&profile.path)?;
    let Some(updated) = without_block(&original, &profile.path)? else {
        return Ok(false);
    };
    if !dry_run {
        crate::branches::write_file(&profile.path, &updated)?;
    }
    Ok(true)
}

/// Whether `profile` has the function.
pub fn is_installed(profile: &Profile) -> bool {
    std::fs::read(&profile.path).is_ok_and(|bytes| has_block(&bytes))
}

// ---------------------------------------------------------------------------
// Commands' share
// ---------------------------------------------------------------------------

/// Whether PowerShell's execution policy keeps a profile from loading.
pub fn policy_blocks_profile(policy: &str) -> bool {
    matches!(
        policy.trim().to_ascii_lowercase().as_str(),
        "restricted" | "allsigned"
    )
}

/// The effective execution policy of Windows PowerShell, if it can be told.
fn execution_policy() -> Option<String> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-ExecutionPolicy",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let policy = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !policy.is_empty()).then_some(policy)
}

/// On Windows: report the execution policy and warn when it stops a profile
/// from loading. Does not change it.
fn report_execution_policy() {
    if !cfg!(windows) {
        return;
    }
    match execution_policy() {
        Some(policy) if policy_blocks_profile(&policy) => {
            println!(
                "  {} PowerShell's execution policy is {policy}: it will not load the profile, so the claude function stays inactive. Fix it with:",
                "!".yellow()
            );
            println!("      Set-ExecutionPolicy -Scope CurrentUser RemoteSigned");
        }
        Some(policy) => println!("  PowerShell execution policy: {policy}"),
        None => println!("  PowerShell execution policy: could not be read"),
    }
}

/// The wrapper's part of `guard install`.
pub fn install(dry_run: bool) -> Result<()> {
    let exe = std::env::current_exe().context("Cannot tell where this binary is")?;
    let profiles = profiles()?;
    if profiles.is_empty() {
        println!("No bash or zsh found: the claude function was not installed");
        return Ok(());
    }
    if dry_run {
        println!("{}", "guard install --dry-run would:".bold());
    }
    for profile in &profiles {
        let outcome = install_in(profile, &exe, dry_run)?;
        let what = match (outcome, dry_run) {
            (Outcome::Unchanged, _) => "already up to date in",
            (Outcome::Added, false) => "added the claude function to",
            (Outcome::Updated, false) => "updated the claude function in",
            (Outcome::Added, true) => "add the claude function to",
            (Outcome::Updated, true) => "update the claude function in",
        };
        println!("  {} {what} {}", "✓".green(), profile.path.display());
    }
    if !dry_run {
        println!(
            "{} the claude function is active in terminals opened from now on (a stale window reloads on its own)",
            "✓".green()
        );
    }
    report_execution_policy();
    Ok(())
}

/// The wrapper's part of `guard uninstall`.
pub fn uninstall(dry_run: bool) -> Result<()> {
    for profile in profiles()? {
        if uninstall_from(&profile, dry_run)? {
            println!(
                "  {} {} the claude function from {}",
                "✓".green(),
                if dry_run { "remove" } else { "removed" },
                profile.path.display()
            );
        }
    }
    Ok(())
}

/// The wrapper's part of `guard status`.
pub fn status() -> Result<()> {
    let profiles = profiles()?;
    if profiles.is_empty() {
        println!("  claude function: no bash or zsh found");
    }
    for profile in &profiles {
        println!(
            "  claude function in {}: {}",
            profile.path.display(),
            if is_installed(profile) {
                "installed".green()
            } else {
                "not installed".yellow()
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from("/opt/ccs/claude-code-sync")
    }

    fn path() -> &'static Path {
        Path::new("profile")
    }

    #[test]
    fn the_blocks_are_delimited_and_name_the_binary() {
        for block in [posix_block(&exe()), powershell_block(&exe())] {
            assert!(block.starts_with(&format!("{BEGIN}\n")));
            assert!(block.ends_with(&format!("{END}\n")));
            assert!(block.contains("/opt/ccs/claude-code-sync"));
            assert!(block.contains("take-reload"));
            assert!(block.contains("CLAUDE_GUARD_SHELL"));
            assert!(find_block(block.as_bytes(), path()).unwrap().is_some());
        }
        assert!(powershell_block(&exe()).contains("--ps-legacy"));
        assert!(posix_block(&exe()).contains("--format nul"));
    }

    #[test]
    fn a_path_with_quotes_or_non_ascii_is_quoted_for_each_shell() {
        let odd = Path::new("/home/o'neil/ñ/claude-code-sync");
        assert!(posix_block(odd).contains(r"'/home/o'\''neil/ñ/claude-code-sync'"));
        let ps = powershell_block(odd);
        assert!(ps.is_ascii());
        assert!(ps.contains("FromBase64String"));
        let quoted = powershell_block(Path::new(r"C:\Users\o'neil\ccs.exe"));
        assert!(quoted.contains(r"'C:\Users\o''neil\ccs.exe'"));
    }

    #[test]
    fn installing_appends_to_a_file_and_leaves_the_rest() {
        let block = posix_block(&exe());
        let out = with_block(b"export A=1\nalias x=y\n", &block, path()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("export A=1\nalias x=y\n\n# >>>"));
        assert!(text.ends_with(&block));
        // A file without a final newline gets one first.
        let out = with_block(b"export A=1", &block, path()).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .starts_with("export A=1\n\n# >>>"));
        // An empty file is just the block.
        assert_eq!(with_block(b"", &block, path()).unwrap(), block.as_bytes());
    }

    #[test]
    fn installing_twice_changes_nothing_and_a_new_binary_replaces_the_block() {
        let first = with_block(b"before\n", &posix_block(&exe()), path()).unwrap();
        let again = with_block(&first, &posix_block(&exe()), path()).unwrap();
        assert_eq!(first, again);

        let moved = posix_block(Path::new("/elsewhere/ccs"));
        let mut file = first.clone();
        file.extend_from_slice(b"after\n");
        let replaced = with_block(&file, &moved, path()).unwrap();
        let text = String::from_utf8(replaced).unwrap();
        assert!(text.starts_with("before\n\n# >>>"));
        assert!(text.ends_with("# <<< claude-code-sync guard <<<\nafter\n"));
        assert!(text.contains("/elsewhere/ccs"));
        assert!(!text.contains("/opt/ccs"));
        assert_eq!(text.matches(BEGIN).count(), 1);
    }

    #[test]
    fn uninstalling_restores_the_file_and_is_idempotent() {
        let block = posix_block(&exe());
        for original in [&b""[..], b"a\n", b"a\nb\n\nc\n", b"\n\nx\n"] {
            let installed = with_block(original, &block, path()).unwrap();
            let removed = without_block(&installed, path()).unwrap().unwrap();
            assert_eq!(removed, original, "for {original:?}");
            assert_eq!(without_block(&removed, path()).unwrap(), None);
        }
        // Text after the block stays.
        let mut file = with_block(b"a\n", &block, path()).unwrap();
        file.extend_from_slice(b"z\n");
        assert_eq!(
            without_block(&file, path()).unwrap().unwrap(),
            b"a\nz\n".to_vec()
        );
    }

    #[test]
    fn crlf_files_keep_their_line_endings_and_a_byte_order_mark_stays() {
        let block = powershell_block(&exe());
        let original = b"\xef\xbb\xbf$x = 1\r\n";
        let installed = with_block(original, &block, path()).unwrap();
        assert!(installed.starts_with(b"\xef\xbb\xbf$x = 1\r\n\r\n# >>>"));
        // Every newline is CRLF.
        let text = String::from_utf8(installed.clone()).unwrap();
        assert_eq!(text.matches('\n').count(), text.matches("\r\n").count());
        assert_eq!(with_block(&installed, &block, path()).unwrap(), installed);
        assert_eq!(
            without_block(&installed, path()).unwrap().unwrap(),
            original.to_vec()
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_left_byte_for_byte() {
        let original = b"# caf\xe9\n";
        let installed = with_block(original, &posix_block(&exe()), path()).unwrap();
        assert!(installed.starts_with(original));
        assert_eq!(
            without_block(&installed, path()).unwrap().unwrap(),
            original.to_vec()
        );
    }

    #[test]
    fn half_a_block_is_an_error_not_a_guess() {
        let only_start = format!("a\n{BEGIN}\nstuff\n");
        assert!(with_block(only_start.as_bytes(), "x\n", path()).is_err());
        assert!(without_block(only_start.as_bytes(), path()).is_err());
        let only_end = format!("a\n{END}\n");
        assert!(without_block(only_end.as_bytes(), path()).is_err());
    }

    #[test]
    fn install_and_uninstall_work_on_files_and_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let rc = Profile {
            path: dir.path().join("sub").join(".bashrc"),
            flavor: Flavor::Posix,
        };
        assert!(!is_installed(&rc));
        assert_eq!(install_in(&rc, &exe(), true).unwrap(), Outcome::Added);
        assert!(!rc.path.exists());
        assert_eq!(install_in(&rc, &exe(), false).unwrap(), Outcome::Added);
        assert!(is_installed(&rc));
        assert_eq!(install_in(&rc, &exe(), false).unwrap(), Outcome::Unchanged);
        let other = Path::new("/new/place/ccs");
        assert_eq!(install_in(&rc, other, false).unwrap(), Outcome::Updated);
        assert!(uninstall_from(&rc, true).unwrap());
        assert!(is_installed(&rc));
        assert!(uninstall_from(&rc, false).unwrap());
        assert!(!is_installed(&rc));
        assert!(!uninstall_from(&rc, false).unwrap());
        assert_eq!(std::fs::read(&rc.path).unwrap(), b"");
    }

    #[test]
    fn the_flavor_follows_the_extension_and_the_policy_check_knows_its_values() {
        assert_eq!(flavor_of(Path::new("a/profile.ps1")), Flavor::PowerShell);
        assert_eq!(flavor_of(Path::new("a/PROFILE.PS1")), Flavor::PowerShell);
        assert_eq!(flavor_of(Path::new("a/.zshrc")), Flavor::Posix);
        for blocked in ["Restricted", "AllSigned\r\n", " restricted "] {
            assert!(policy_blocks_profile(blocked), "{blocked}");
        }
        for fine in ["RemoteSigned", "Unrestricted", "Bypass", "Undefined", ""] {
            assert!(!policy_blocks_profile(fine), "{fine}");
        }
    }
}
