//! Canonical project identity: an `id -> absolute path` table, configured per
//! machine, so one project syncs under one name when home directories, user
//! names or checkout paths differ.
//!
//! Beside it, a root map: `id -> absolute folder`, also per machine. Every
//! project under a mapped folder syncs as `@<id>@<path below the folder>`, so
//! a whole tree of projects — new ones included — needs no setup at all. The
//! machine's home directory is the implicit root `home`.
//!
//! Resolution order in both directions: the project map, then the root map
//! (longest matching root), then `use_project_name_only`, then the encoded
//! directory name. The project map comes first because it survives a rename
//! and stays unambiguous when two checkouts share a folder name.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::filter::FilterConfig;
use crate::sync::discovery::{extract_project_name, find_local_project_by_name};

/// Canonical id -> absolute project path, as configured per machine.
pub type ProjectMap = BTreeMap<String, PathBuf>;

/// Root id -> absolute folder, as configured per machine. A project below a
/// root syncs under `@<id>@<path relative to the root>`.
pub type RootMap = BTreeMap<String, PathBuf>;

/// The implicit root: this machine's home directory, unless
/// `auto_home_root` is off or `home` is mapped explicitly.
pub const HOME_ROOT_ID: &str = "home";

/// Opens and closes the root id in a repo directory name: `@<id>@<rel>`.
const ROOT_MARKER: char = '@';

/// The longest directory name Claude Code writes before it shortens one.
const MAX_ENCODED_LENGTH: usize = 200;

/// Encode a project path the way Claude Code names `~/.claude/projects/<dir>`.
///
/// Claude Code derives the directory with a JavaScript regex, which runs over
/// UTF-16 code units. A byte-wise version emits a different name for any
/// non-ASCII path, and files then land in a directory Claude Code never reads.
pub fn encode_project_path(path: &Path) -> String {
    encode_path_str(&path.to_string_lossy())
}

/// [`encode_project_path`] over the path as text, so a Windows path can be
/// encoded (and tested) on any platform.
///
/// A name longer than 200 units is cut at 200 and suffixed with `-` and the
/// base-36 absolute value of a 32-bit hash of the whole original path — the
/// `(h << 5) - h + unit` string hash Claude Code computes over UTF-16 units.
pub fn encode_path_str(path: &str) -> String {
    let units: Vec<u16> = path.encode_utf16().collect();
    let encoded: String = units
        .iter()
        .map(|&unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => byte as char,
            _ => '-',
        })
        .collect();
    // One ASCII character per UTF-16 unit, so byte and unit lengths agree.
    if encoded.len() <= MAX_ENCODED_LENGTH {
        return encoded;
    }

    let mut hash: i32 = 0;
    for &unit in &units {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit));
    }
    // `Math.abs` of i32::MIN is 2147483648, which `unsigned_abs` keeps.
    format!(
        "{}-{}",
        &encoded[..MAX_ENCODED_LENGTH],
        to_base36(u64::from(hash.unsigned_abs()))
    )
}

/// `Number.prototype.toString(36)` for a non-negative integer.
fn to_base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base-36 digits are ASCII")
}

/// The spelling that encodes to the directory Claude Code created: no trailing
/// separator, no `.` components.
pub fn normalize_project_path(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Reject a map that cannot be applied: an id becomes a directory name in the
/// sync repository, a path must be absolute, and a path spelled differently
/// from the project's own (a trailing slash) encodes to a different directory.
pub fn validate(map: &ProjectMap) -> Result<()> {
    for (id, path) in map {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
            bail!("project_map id {id:?} must be a plain directory name");
        }
        if !path.is_absolute() {
            bail!(
                "project_map entry {id:?} must be an absolute path, got {}",
                path.display()
            );
        }
        // Compared as text: `Path` equality ignores a trailing separator,
        // `encode_project_path` turns it into another dash.
        if normalize_project_path(path).to_string_lossy() != path.to_string_lossy() {
            bail!(
                "project_map entry {id:?} must be written exactly as the project's path, got {} \
                 (try {})",
                path.display(),
                normalize_project_path(path).display()
            );
        }
    }
    ensure_no_encoding_collisions(map)
}

/// Reject a map in which two ids encode to one directory: their files would
/// merge under both ids, and nothing could separate them afterwards.
pub fn ensure_no_encoding_collisions(map: &ProjectMap) -> Result<()> {
    let mut seen: BTreeMap<String, &String> = BTreeMap::new();
    for (id, path) in map {
        let encoded = encode_project_path(path);
        if let Some(previous) = seen.insert(encoded.clone(), id) {
            bail!("project_map entries {previous:?} and {id:?} both encode to {encoded:?}");
        }
    }
    Ok(())
}

/// The canonical id of a local encoded project directory, when one is mapped.
pub fn canonical_id(map: &ProjectMap, encoded_dir: &str) -> Option<String> {
    map.iter()
        .find(|(_, path)| encode_project_path(path) == encoded_dir)
        .map(|(id, _)| id.clone())
}

/// Characters a root id may not contain: it becomes part of one directory
/// name on every operating system, and `@` delimits it.
const FORBIDDEN_ROOT_ID_CHARS: &[char] = &['/', '\\', '@', '%', ':', '*', '?', '"', '<', '>', '|'];

/// Reject a root map that cannot be applied: an id that is not a plain name,
/// or a folder that is not an absolute path.
pub fn validate_roots(roots: &RootMap) -> Result<()> {
    for (id, path) in roots {
        let plain = !id.is_empty()
            && id != "."
            && id != ".."
            && !id.chars().any(|c| {
                FORBIDDEN_ROOT_ID_CHARS.contains(&c) || c.is_whitespace() || c.is_control()
            });
        if !plain {
            bail!(
                "root_map id {id:?} must be a plain name (no spaces, path separators or any of {})",
                FORBIDDEN_ROOT_ID_CHARS.iter().collect::<String>()
            );
        }
        if !path.is_absolute() {
            bail!(
                "root_map entry {id:?} must be an absolute path, got {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// The roots this machine resolves: the configured ones, plus `home` for the
/// home directory unless it is mapped explicitly or `auto_home_root` is off.
pub fn effective_roots(filter: &FilterConfig) -> RootMap {
    let mut roots = filter.root_map.clone();
    if filter.auto_home_root && !roots.contains_key(HOME_ROOT_ID) {
        if let Some(home) = crate::sync::discovery::machine_home_dir() {
            roots.insert(HOME_ROOT_ID.to_string(), home);
        }
    }
    roots
}

/// Whether a path is spelled the Windows way: a drive letter (`C:\`, `c:/`)
/// or a UNC share (`\\\\server\\share`). Decided from the text, so a Windows
/// path is handled the same on every platform.
pub fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    drive || path.starts_with(r"\\")
}

/// A path's components, ignoring repeated and trailing separators. Windows
/// paths split on both separators; elsewhere a backslash is an ordinary
/// character of a file name.
fn path_components(path: &str, windows: bool) -> Vec<&str> {
    path.split(|c| c == '/' || (windows && c == '\\'))
        .filter(|part| !part.is_empty())
        .collect()
}

/// Whether two path components name the same folder: case-insensitively on
/// Windows (`c:` and `C:` included), exactly elsewhere.
fn same_component(a: &str, b: &str, windows: bool) -> bool {
    if windows {
        a == b || a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// `path` relative to `root`, components joined with `/`, or `None` when the
/// path is not inside the root. The root itself is `Some("")`.
pub fn relative_to_root(root: &str, path: &str) -> Option<String> {
    let windows = is_windows_path(root);
    if is_windows_path(path) != windows {
        return None;
    }
    if !windows && !(root.starts_with('/') && path.starts_with('/')) {
        return None;
    }
    let root_parts = path_components(root, windows);
    let path_parts = path_components(path, windows);
    if path_parts.len() < root_parts.len() {
        return None;
    }
    let inside = root_parts
        .iter()
        .zip(&path_parts)
        .all(|(a, b)| same_component(a, b, windows));
    inside.then(|| path_parts[root_parts.len()..].join("/"))
}

/// The repo directory name of a project at `path` under the longest root
/// that contains it, or `None` when no root does.
pub fn root_repo_dir_name(roots: &RootMap, path: &str) -> Option<String> {
    let mut best: Option<(usize, &str, String)> = None;
    for (id, root) in roots {
        let root = root.to_string_lossy();
        let Some(relative) = relative_to_root(&root, path) else {
            continue;
        };
        let depth = path_components(&root, is_windows_path(&root)).len();
        if best
            .as_ref()
            .is_none_or(|(best_depth, _, _)| depth > *best_depth)
        {
            best = Some((depth, id, relative));
        }
    }
    best.map(|(_, id, relative)| root_dir_name(id, &relative))
}

/// `@<id>@<escaped relative path>`: one directory name, valid on every OS.
pub fn root_dir_name(id: &str, relative: &str) -> String {
    format!(
        "{ROOT_MARKER}{id}{ROOT_MARKER}{}",
        escape_relative_path(relative)
    )
}

/// Split a repo directory name written by [`root_dir_name`] into its root id
/// and its relative path (`/`-separated), or `None` for any other name.
pub fn parse_root_dir_name(name: &str) -> Option<(&str, String)> {
    let rest = name.strip_prefix(ROOT_MARKER)?;
    let (id, escaped) = rest.split_once(ROOT_MARKER)?;
    if id.is_empty() {
        return None;
    }
    Some((id, unescape_relative_path(escaped)?))
}

/// Characters a directory name cannot hold on some OS, written as `%XX`:
/// `%` itself, both separators, and what Windows forbids in a name.
const ESCAPED_CHARS: &[char] = &['%', '/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Escape a `/`-separated relative path into one portable path component.
/// A trailing dot or space is escaped too: Windows drops it from a name.
pub fn escape_relative_path(relative: &str) -> String {
    let mut escaped = String::with_capacity(relative.len());
    let last = relative.chars().count().saturating_sub(1);
    for (index, c) in relative.chars().enumerate() {
        let trailing_dot_or_space = index == last && (c == '.' || c == ' ');
        if ESCAPED_CHARS.contains(&c) || c.is_ascii_control() || trailing_dot_or_space {
            escaped.push_str(&format!("%{:02X}", c as u32));
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// Undo [`escape_relative_path`]. `None` for a malformed escape.
pub fn unescape_relative_path(escaped: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(escaped.len());
    let mut rest = escaped.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The absolute path of `relative` (`/`-separated) under `root`, joined with
/// the root's own separator and spelled as the root is configured. `None`
/// when the relative path would climb out of the root.
pub fn root_project_path(root: &str, relative: &str) -> Option<String> {
    let windows = is_windows_path(root);
    let separator = if windows { "\\" } else { "/" };
    let parts: Vec<&str> = relative.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|part| *part == "." || *part == "..") {
        return None;
    }
    if parts.is_empty() {
        return Some(root.to_string());
    }
    // `/` and `C:\` keep their separator: trimming leaves "" and "C:".
    let base = root.trim_end_matches(|c| c == '/' || (windows && c == '\\'));
    Some(format!("{base}{separator}{}", parts.join(separator)))
}

/// The repo directory name a local encoded project directory takes from the
/// project map or the root map, or `None` when neither places it.
pub fn mapped_repo_dir_name(
    filter: &FilterConfig,
    projects_dir: &Path,
    encoded_dir: &str,
) -> Option<String> {
    if let Some(id) = canonical_id(&filter.project_map, encoded_dir) {
        return Some(id);
    }
    let roots = effective_roots(filter);
    if roots.is_empty() {
        return None;
    }
    let path = project_path_from_transcripts(&projects_dir.join(encoded_dir), encoded_dir, &roots)?;
    root_repo_dir_name(&roots, &path)
}

/// The project folder an encoded directory stands for: a `cwd` one of its
/// transcripts records that encodes back to the directory's name. A session
/// that changed directory records other `cwd`s as well; those are skipped.
///
/// A project that has only been pulled here so far records another
/// machine's paths, none of which encodes to this directory. Then each
/// recorded path's trailing components are tried under each of this
/// machine's roots — the part below a root is what both machines share —
/// and a candidate whose encoding is exactly the directory's name is it.
///
/// Answers are remembered for the life of the process, since discovery asks
/// once per file in a directory: a found path for good (a directory's name
/// pins its path), a miss until the directory's transcripts change.
pub fn project_path_from_transcripts(
    project_dir: &Path,
    encoded_dir: &str,
    roots: &RootMap,
) -> Option<String> {
    use std::io::BufRead;

    type Lookup = BTreeMap<PathBuf, Result<String, (usize, u64)>>;
    static LOOKUPS: OnceLock<Mutex<Lookup>> = OnceLock::new();
    let lookups = LOOKUPS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let remembered = lookups.lock().ok()?.get(project_dir).cloned();
    if let Some(Ok(path)) = &remembered {
        return Some(path.clone());
    }

    let mut transcripts: Vec<(PathBuf, u64)> = std::fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| {
            let metadata = e.metadata().ok()?;
            metadata.is_file().then(|| (e.path(), metadata.len()))
        })
        .collect();
    transcripts.sort();
    let fingerprint = (
        transcripts.len(),
        transcripts.iter().map(|(_, len)| len).sum::<u64>(),
    );
    if remembered == Some(Err(fingerprint)) {
        return None;
    }

    let mut recorded: Vec<String> = Vec::new();
    let mut found = None;
    'transcripts: for (transcript, _) in &transcripts {
        let Ok(file) = std::fs::File::open(transcript) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines().map_while(|l| l.ok()) {
            // Most lines are not worth parsing for this.
            if !line.contains("\"cwd\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(cwd) = entry.get("cwd").and_then(|cwd| cwd.as_str()) else {
                continue;
            };
            if recorded.iter().any(|seen| seen == cwd) {
                continue;
            }
            if encode_path_str(cwd) == encoded_dir {
                found = Some(cwd.to_string());
                break 'transcripts;
            }
            recorded.push(cwd.to_string());
        }
    }
    let found = found.or_else(|| path_under_a_root(&recorded, roots, encoded_dir));

    if let Ok(mut lookups) = lookups.lock() {
        let answer = found.clone().ok_or(fingerprint);
        lookups.insert(project_dir.to_path_buf(), answer);
    }
    found
}

/// The path under one of `roots` that a recorded `cwd`'s trailing
/// components make and that encodes to `encoded_dir`, longest first.
fn path_under_a_root(recorded: &[String], roots: &RootMap, encoded_dir: &str) -> Option<String> {
    for cwd in recorded {
        let parts = path_components(cwd, is_windows_path(cwd));
        for root in roots.values() {
            let root = root.to_string_lossy();
            for start in 0..=parts.len() {
                let Some(candidate) = root_project_path(&root, &parts[start..].join("/")) else {
                    continue;
                };
                if encode_path_str(&candidate) == encoded_dir {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// The directory name a local encoded project directory takes in the sync
/// repo, from its name alone. The root map needs the project's real path,
/// which only its transcripts know: [`repo_dir_name_in`] consults it.
pub fn repo_dir_name(filter: &FilterConfig, encoded_dir: &str) -> String {
    if let Some(id) = canonical_id(&filter.project_map, encoded_dir) {
        return id;
    }
    if filter.use_project_name_only {
        return extract_project_name(encoded_dir).to_string();
    }
    encoded_dir.to_string()
}

/// The directory name a project directory under `projects_dir` takes in the
/// sync repo: the project map, then the root map, then the name-only and
/// encoded fallbacks. Sessions, their sidecar directories and every
/// attachment of the project go through here, so they share one name.
///
/// In `use_project_name_only` mode the name comes from the working directory
/// a transcript in it records, the same name its sessions are pushed under:
/// the encoded directory alone cannot tell `shop-web` from `shop/web`, and
/// its last dash-separated piece would file this project's memory and
/// attachments apart from its sessions.
pub fn repo_dir_name_in(filter: &FilterConfig, projects_dir: &Path, encoded_dir: &str) -> String {
    if let Some(name) = mapped_repo_dir_name(filter, projects_dir, encoded_dir) {
        return name;
    }
    if filter.use_project_name_only {
        if let Some(name) = project_name_from_transcripts(&projects_dir.join(encoded_dir)) {
            return name;
        }
    }
    repo_dir_name(filter, encoded_dir)
}

/// The folder name of the first `cwd` a transcript directly inside
/// `project_dir` records, if any does.
fn project_name_from_transcripts(project_dir: &Path) -> Option<String> {
    use std::io::BufRead;

    let mut transcripts: Vec<PathBuf> = std::fs::read_dir(project_dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    transcripts.sort();

    for transcript in transcripts {
        let Ok(file) = std::fs::File::open(&transcript) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines().map_while(|l| l.ok()) {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(name) = entry
                .get("cwd")
                .and_then(|cwd| cwd.as_str())
                .and_then(|cwd| Path::new(cwd).file_name())
                .and_then(|name| name.to_str())
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The local project directory a sync-repo directory name belongs to, or
/// `None` when this machine has no destination for it.
pub fn local_project_dir(
    filter: &FilterConfig,
    projects_dir: &Path,
    repo_dir_name: &str,
) -> Option<PathBuf> {
    if let Some(path) = filter.project_map.get(repo_dir_name) {
        return Some(projects_dir.join(encode_project_path(path)));
    }
    if let Some((root_id, relative)) = parse_root_dir_name(repo_dir_name) {
        // Another machine's root: placed only if this machine maps it too.
        let roots = effective_roots(filter);
        let root = roots.get(root_id)?;
        let path = root_project_path(&root.to_string_lossy(), &relative)?;
        return Some(projects_dir.join(encode_path_str(&path)));
    }
    if filter.use_project_name_only {
        return find_local_project_by_name(projects_dir, repo_dir_name);
    }
    // An existing directory, or an encoded path Claude Code would create.
    // Anything else is another machine's canonical id and has no destination
    // here until this machine maps it too.
    let candidate = projects_dir.join(repo_dir_name);
    if candidate.is_dir() || is_encoded_project_path(repo_dir_name) {
        return Some(candidate);
    }
    None
}

/// Files a pull has no local project for, grouped by the sync-repo directory
/// name they came from.
pub type SkippedByProject = BTreeMap<String, Vec<PathBuf>>;

/// Split a path relative to the sync repo's projects directory into the
/// project directory it belongs to and the path inside that project. A file
/// lying beside the project directories belongs to no project, and nothing on
/// this machine can take it.
pub fn split_project_path(relative: &Path) -> Option<(&str, &Path)> {
    let mut parts = relative.components();
    let project = parts.next()?.as_os_str().to_str()?;
    let inside_project = parts.as_path();
    if inside_project.as_os_str().is_empty() {
        return None;
    }
    Some((project, inside_project))
}

/// Fold one set of misses into another, so a project whose sessions and whose
/// attachments were both skipped is reported once rather than twice.
pub fn merge_skipped(into: &mut SkippedByProject, more: &SkippedByProject) {
    for (project, files) in more {
        into.entry(project.clone())
            .or_default()
            .extend(files.iter().cloned());
    }
}

/// What to do about a project this machine cannot place, in both warning modes.
const MAPPING_REMEDY: &str = "Map each one: claude-code-sync config --map-project <id>=<path>.";

/// The remedy for the misses: map the project, or for a project another
/// machine filed under a root, map that root here.
fn mapping_remedy(skipped: &SkippedByProject) -> String {
    let mut roots: Vec<&str> = skipped
        .keys()
        .filter_map(|project| parse_root_dir_name(project).map(|(id, _)| id))
        .collect();
    roots.dedup();
    let has_projects = skipped.keys().any(|p| parse_root_dir_name(p).is_none());
    let root_remedy = (!roots.is_empty()).then(|| {
        format!(
            "Map each root ({}): claude-code-sync config --map-root <root>=<path>.",
            roots.join(", ")
        )
    });
    match (has_projects, root_remedy) {
        (true, Some(root_remedy)) => format!("{MAPPING_REMEDY} {root_remedy}"),
        (false, Some(root_remedy)) => root_remedy,
        (_, None) => MAPPING_REMEDY.to_string(),
    }
}

/// The warning lines for files no local project claims: one line naming every
/// project by default, one line per file when `each_file` is set.
///
/// A project this machine has not mapped holds every file it ever had, and a
/// line each buries the one remedy under hundreds of repetitions of it.
pub fn skipped_project_warnings(skipped: &SkippedByProject, each_file: bool) -> Vec<String> {
    if skipped.is_empty() {
        return Vec::new();
    }

    if each_file {
        let mut lines: Vec<String> = skipped
            .iter()
            .flat_map(|(project, files)| {
                files.iter().map(move |file| {
                    format!(
                        "Skipping {} (no local project for '{project}')",
                        file.display()
                    )
                })
            })
            .collect();
        lines.push(mapping_remedy(skipped));
        return lines;
    }

    let total_files: usize = skipped.values().map(Vec::len).sum();
    let project_list: Vec<String> = skipped
        .iter()
        .map(|(project, files)| format!("{project} ({})", files.len()))
        .collect();

    vec![format!(
        "Skipped {total_files} file(s) with no local project: {}. {} \
         A line per file: config --warn-each-skipped-file true.",
        project_list.join(", "),
        mapping_remedy(skipped)
    )]
}

/// Whether a directory name is an encoded project path. Encoding an absolute
/// path puts a dash where its root is: `/home/user/app` becomes
/// `-home-user-app`, `C:\src\app` becomes `C--src-app`.
fn is_encoded_project_path(name: &str) -> bool {
    name.starts_with('-') || name.get(1..3) == Some("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(entries: &[(&str, &str)]) -> FilterConfig {
        FilterConfig {
            project_map: entries
                .iter()
                .map(|(id, path)| ((*id).to_string(), PathBuf::from(path)))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn encodes_every_non_alphanumeric_unit_as_a_dash() {
        assert_eq!(
            encode_project_path(Path::new("/home/user/.config-dir")),
            "-home-user--config-dir"
        );
        assert_eq!(
            encode_project_path(Path::new("/home/username/projects/site.example")),
            "-home-username-projects-site-example"
        );
    }

    #[test]
    fn encodes_by_utf16_unit_not_by_byte() {
        assert_eq!(encode_project_path(Path::new("a-\u{17e}_b")), "a---b");
    }

    #[test]
    fn a_long_path_is_cut_and_hashed_like_claude_code_does() {
        // Ground truth: the directory Claude Code 2.1.326 itself created.
        let path = "/Users/pablo/sim-mateo/rutalarga/carpeta-con-nombre-muy-largo-01/\
                    carpeta-con-nombre-muy-largo-02/carpeta-con-nombre-muy-largo-03/\
                    carpeta-con-nombre-muy-largo-04/carpeta-con-nombre-muy-largo-05/\
                    carpeta-con-nombre-muy-largo-06/Proyecto_\u{d1}and\u{fa}.v2";
        assert_eq!(
            encode_path_str(path),
            "-Users-pablo-sim-mateo-rutalarga-carpeta-con-nombre-muy-largo-01-\
             carpeta-con-nombre-muy-largo-02-carpeta-con-nombre-muy-largo-03-\
             carpeta-con-nombre-muy-largo-04-carpeta-con-nombre-muy-largo-05-carpeta-lxv0h1"
        );
    }

    #[test]
    fn a_path_of_exactly_two_hundred_units_is_left_whole() {
        let path = format!("/{}", "a".repeat(199));
        assert_eq!(encode_path_str(&path).len(), 200);
        assert!(!encode_path_str(&path).contains("a-"));
        let longer = format!("/{}", "a".repeat(200));
        assert!(encode_path_str(&longer).len() > 201);
    }

    #[test]
    fn the_hash_of_i32_min_is_its_absolute_value() {
        assert_eq!(to_base36(u64::from(i32::MIN.unsigned_abs())), "zik0zk");
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
    }

    #[test]
    fn a_windows_path_encodes_on_any_platform() {
        assert_eq!(
            encode_path_str(r"C:\Users\Mateo\proyectos\jarvis"),
            "C--Users-Mateo-proyectos-jarvis"
        );
    }

    #[test]
    fn mapped_project_uses_its_canonical_id_in_the_repo() {
        let filter = mapped(&[("shop", "/home/user/projects/shop-web")]);
        let encoded = "-home-user-projects-shop-web";
        assert_eq!(repo_dir_name(&filter, encoded), "shop");
    }

    #[test]
    fn mapped_id_resolves_to_this_machines_path_even_when_renamed() {
        let filter = mapped(&[("shop", "/home/username/work/shop")]);
        let projects = PathBuf::from("/home/username/.claude/projects");
        assert_eq!(
            local_project_dir(&filter, &projects, "shop"),
            Some(projects.join("-home-username-work-shop"))
        );
    }

    #[test]
    fn unmapped_project_keeps_the_encoded_directory() {
        let filter = FilterConfig::default();
        let encoded = "-home-user-projects-other";
        assert_eq!(repo_dir_name(&filter, encoded), encoded);
        let projects = PathBuf::from("/home/user/.claude/projects");
        assert_eq!(
            local_project_dir(&filter, &projects, encoded),
            Some(projects.join(encoded))
        );
    }

    #[test]
    fn unmapped_project_still_falls_back_to_name_only() {
        let filter = FilterConfig {
            use_project_name_only: true,
            ..Default::default()
        };
        assert_eq!(repo_dir_name(&filter, "-home-user-work-myapp"), "myapp");
    }

    #[test]
    fn name_only_files_attachments_under_the_same_name_as_sessions() {
        let projects = tempfile::TempDir::new().unwrap();
        let encoded = "-home-user-shop-web";
        std::fs::create_dir_all(projects.path().join(encoded).join("memory")).unwrap();
        std::fs::write(
            projects.path().join(encoded).join("s1.jsonl"),
            "{\"type\":\"summary\"}\n{\"type\":\"user\",\"cwd\":\"/home/user/shop-web\"}\n",
        )
        .unwrap();
        let filter = FilterConfig {
            use_project_name_only: true,
            ..Default::default()
        };

        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), encoded),
            "shop-web",
            "the folder name, not its last dash-separated piece"
        );
        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), "-home-user-no-transcripts"),
            "transcripts",
            "without a transcript it falls back to the old guess"
        );
    }

    #[test]
    fn the_map_wins_over_name_only_so_two_checkouts_stay_apart() {
        let mut filter = mapped(&[("work-myapp", "/home/user/work/myapp")]);
        filter.use_project_name_only = true;
        assert_eq!(
            repo_dir_name(&filter, "-home-user-work-myapp"),
            "work-myapp"
        );
        assert_eq!(repo_dir_name(&filter, "-home-user-personal-myapp"), "myapp");
    }

    #[test]
    fn an_id_that_is_not_a_plain_directory_name_is_refused() {
        for bad in ["../escape", "nested/id", "", "."] {
            let map: ProjectMap = [(bad.to_string(), PathBuf::from("/home/user/app"))]
                .into_iter()
                .collect();
            assert!(validate(&map).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_path_spelled_differently_from_the_encoded_one_is_refused() {
        let map: ProjectMap = [("app".to_string(), PathBuf::from("/home/user/work/app/"))]
            .into_iter()
            .collect();
        assert!(
            validate(&map).is_err(),
            "a trailing slash encodes differently"
        );
        assert_eq!(
            normalize_project_path(Path::new("/home/user/work/app/")),
            PathBuf::from("/home/user/work/app")
        );
    }

    #[test]
    fn another_machines_canonical_id_has_no_destination_here() {
        let filter = FilterConfig::default();
        let projects = PathBuf::from("/home/user/.claude/projects");

        for id in ["app", "a-b", "work-myapp"] {
            assert_eq!(local_project_dir(&filter, &projects, id), None, "{id}");
        }
        for encoded in ["-home-user-app", "-x", "C--src-app"] {
            assert_eq!(
                local_project_dir(&filter, &projects, encoded),
                Some(projects.join(encoded)),
                "{encoded}"
            );
        }
    }

    #[test]
    fn a_project_directory_this_machine_already_has_is_a_destination() {
        let projects = tempfile::tempdir().unwrap();
        std::fs::create_dir(projects.path().join("app")).unwrap();

        assert_eq!(
            local_project_dir(&FilterConfig::default(), projects.path(), "app"),
            Some(projects.path().join("app")),
            "an existing directory is one Claude Code reads, whatever its name"
        );
    }

    #[test]
    fn a_relative_project_path_is_refused() {
        let map: ProjectMap = [("app".to_string(), PathBuf::from("work/app"))]
            .into_iter()
            .collect();
        assert!(validate(&map).is_err());
    }

    fn skipped(entries: &[(&str, &[&str])]) -> SkippedByProject {
        entries
            .iter()
            .map(|(project, files)| {
                let paths = files.iter().map(PathBuf::from).collect();
                ((*project).to_string(), paths)
            })
            .collect()
    }

    #[test]
    fn every_miss_is_combined_into_one_warning() {
        let misses = skipped(&[
            (
                "shop",
                &["/repo/projects/shop/a.png", "/repo/projects/shop/b.png"],
            ),
            ("blog", &["/repo/projects/blog/c.png"]),
        ]);

        let lines = skipped_project_warnings(&misses, false);

        assert_eq!(
            lines.len(),
            1,
            "one warning, however many files were missed"
        );
        assert!(lines[0].contains("Skipped 3 file(s)"));
        assert!(lines[0].contains("blog (1)"));
        assert!(lines[0].contains("shop (2)"));
        assert!(
            lines[0].contains("--map-project"),
            "the remedy stays visible"
        );
    }

    #[test]
    fn each_file_gets_its_own_warning_when_asked() {
        let misses = skipped(&[
            (
                "shop",
                &["/repo/projects/shop/a.png", "/repo/projects/shop/b.png"],
            ),
            ("blog", &["/repo/projects/blog/c.png"]),
        ]);

        let lines = skipped_project_warnings(&misses, true);

        assert_eq!(lines.len(), 4, "one line per skipped file, plus the remedy");
        assert_eq!(
            lines[0],
            "Skipping /repo/projects/blog/c.png (no local project for 'blog')"
        );
        assert!(
            lines[3].contains("--map-project"),
            "the remedy stays visible in per-file mode too"
        );
    }

    #[test]
    fn a_projects_sessions_and_attachments_share_one_warning() {
        let mut skipped_sessions = skipped(&[("shop", &["/repo/projects/shop/sess-1.jsonl"])]);
        let skipped_attachments = skipped(&[("shop", &["/repo/projects/shop/diagram.png"])]);

        merge_skipped(&mut skipped_sessions, &skipped_attachments);
        let lines = skipped_project_warnings(&skipped_sessions, false);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Skipped 2 file(s)"));
        assert!(lines[0].contains("shop (2)"), "named once, counted twice");
    }

    #[test]
    fn nothing_skipped_warns_about_nothing() {
        let nothing = SkippedByProject::new();

        assert!(skipped_project_warnings(&nothing, false).is_empty());
        assert!(skipped_project_warnings(&nothing, true).is_empty());
    }

    fn rooted(entries: &[(&str, &str)]) -> FilterConfig {
        FilterConfig {
            root_map: entries
                .iter()
                .map(|(id, path)| ((*id).to_string(), PathBuf::from(path)))
                .collect(),
            auto_home_root: false,
            ..Default::default()
        }
    }

    fn roots(entries: &[(&str, &str)]) -> RootMap {
        rooted(entries).root_map
    }

    /// Where machine `to` files the project that machine `from` keeps at
    /// `path`: the repo name `from` pushes, resolved by `to`.
    fn carried(from: &RootMap, path: &str, to: &RootMap) -> Option<String> {
        let repo_name = root_repo_dir_name(from, path)?;
        let (id, relative) = parse_root_dir_name(&repo_name)?;
        let root = to.get(id)?;
        root_project_path(&root.to_string_lossy(), &relative).map(|p| encode_path_str(&p))
    }

    #[test]
    fn a_mac_project_lands_in_the_windows_encoded_directory_and_back() {
        let mac = roots(&[("proyectos", "/Users/mateo/proyectos")]);
        let windows = roots(&[("proyectos", r"C:\Users\Mateo\proyectos")]);

        assert_eq!(
            root_repo_dir_name(&mac, "/Users/mateo/proyectos/jarvis").as_deref(),
            Some("@proyectos@jarvis")
        );
        assert_eq!(
            carried(&mac, "/Users/mateo/proyectos/jarvis", &windows).as_deref(),
            Some("C--Users-Mateo-proyectos-jarvis")
        );
        assert_eq!(
            carried(&windows, r"C:\Users\Mateo\proyectos\jarvis", &mac).as_deref(),
            Some("-Users-mateo-proyectos-jarvis")
        );
    }

    #[test]
    fn a_nested_windows_project_lands_on_linux_and_back() {
        let windows = roots(&[("proyectos", r"C:\Users\Mateo\proyectos")]);
        let linux = roots(&[("proyectos", "/home/mateo/proyectos")]);
        let path = r"C:\Users\Mateo\proyectos\clientes\acme\web";

        assert_eq!(
            root_repo_dir_name(&windows, path).as_deref(),
            Some("@proyectos@clientes%2Facme%2Fweb")
        );
        assert_eq!(
            carried(&windows, path, &linux).as_deref(),
            Some("-home-mateo-proyectos-clientes-acme-web")
        );
        assert_eq!(
            carried(&linux, "/home/mateo/proyectos/clientes/acme/web", &windows).as_deref(),
            Some("C--Users-Mateo-proyectos-clientes-acme-web")
        );
    }

    #[test]
    fn a_linux_project_lands_on_a_mac_and_back() {
        let linux = roots(&[("code", "/home/mateo/code")]);
        let mac = roots(&[("code", "/Users/mateo/Developer")]);

        assert_eq!(
            carried(&linux, "/home/mateo/code/api", &mac).as_deref(),
            Some("-Users-mateo-Developer-api")
        );
        assert_eq!(
            carried(&mac, "/Users/mateo/Developer/api", &linux).as_deref(),
            Some("-home-mateo-code-api")
        );
    }

    #[test]
    fn different_profile_names_and_a_onedrive_documents_folder_meet_in_one_name() {
        let first = roots(&[("proyectos", r"C:\Users\Mateo\Documents\proyectos")]);
        let second = roots(&[(
            "proyectos",
            r"C:\Users\mateo.bodenlle\OneDrive\Documentos\proyectos",
        )]);

        assert_eq!(
            carried(
                &first,
                r"C:\Users\Mateo\Documents\proyectos\jarvis",
                &second
            )
            .as_deref(),
            Some("C--Users-mateo-bodenlle-OneDrive-Documentos-proyectos-jarvis")
        );
    }

    #[test]
    fn the_longest_matching_root_wins() {
        let both = roots(&[
            ("home", "/home/mateo"),
            ("proyectos", "/home/mateo/proyectos"),
        ]);

        assert_eq!(
            root_repo_dir_name(&both, "/home/mateo/proyectos/jarvis").as_deref(),
            Some("@proyectos@jarvis")
        );
        assert_eq!(
            root_repo_dir_name(&both, "/home/mateo/otros/notas").as_deref(),
            Some("@home@otros%2Fnotas")
        );
        assert_eq!(root_repo_dir_name(&both, "/srv/app"), None);
    }

    #[test]
    fn a_root_matches_whole_folders_only() {
        let short = roots(&[("p", "/home/mateo/proy")]);
        assert_eq!(root_repo_dir_name(&short, "/home/mateo/proyectos/x"), None);
    }

    #[test]
    fn windows_roots_match_case_insensitively_drive_letter_included() {
        let windows = roots(&[("proyectos", r"C:\Users\Mateo\Proyectos")]);

        assert_eq!(
            root_repo_dir_name(&windows, r"c:\users\mateo\proyectos\Jarvis").as_deref(),
            Some("@proyectos@Jarvis"),
            "the relative part keeps the project's own spelling"
        );
        assert_eq!(
            root_repo_dir_name(&windows, "C:/Users/MATEO/Proyectos/jarvis").as_deref(),
            Some("@proyectos@jarvis"),
            "forward slashes are separators on Windows too"
        );

        let unix = roots(&[("proyectos", "/Users/Mateo/Proyectos")]);
        assert_eq!(
            root_repo_dir_name(&unix, "/users/mateo/proyectos/jarvis"),
            None,
            "Unix paths stay case-sensitive"
        );
    }

    #[test]
    fn a_windows_project_materializes_with_the_configured_root_spelling() {
        let configured = rooted(&[("proyectos", "C:/Users/Mateo/proyectos")]);
        let projects = PathBuf::from("/claude/projects");

        assert_eq!(
            local_project_dir(&configured, &projects, "@proyectos@jarvis"),
            Some(projects.join("C--Users-Mateo-proyectos-jarvis"))
        );
        assert_eq!(
            root_project_path(r"C:\Users\Mateo\proyectos", "a/b").as_deref(),
            Some(r"C:\Users\Mateo\proyectos\a\b")
        );
        assert_eq!(root_project_path("C:\\", "a").as_deref(), Some(r"C:\a"));
        assert_eq!(root_project_path("/", "a").as_deref(), Some("/a"));
    }

    #[test]
    fn the_root_itself_is_a_project_too() {
        let linux = roots(&[("proyectos", "/home/mateo/proyectos")]);
        let windows = rooted(&[("proyectos", r"C:\Users\Mateo\proyectos")]);

        assert_eq!(
            root_repo_dir_name(&linux, "/home/mateo/proyectos").as_deref(),
            Some("@proyectos@")
        );
        assert_eq!(
            local_project_dir(&windows, Path::new("/p"), "@proyectos@"),
            Some(Path::new("/p").join("C--Users-Mateo-proyectos"))
        );
    }

    #[test]
    fn escaping_round_trips_and_yields_one_portable_component() {
        for relative in [
            "plain",
            "a/b/c",
            "100%/done",
            r#"we\ird:name*?"<>|"#,
            "trailing.",
            "trailing ",
            "Proyecto_\u{d1}and\u{fa}.v2/espa\u{f1}ol",
            "",
        ] {
            let escaped = escape_relative_path(relative);
            let invalid = &ESCAPED_CHARS[1..];
            assert!(
                !escaped.contains(invalid),
                "{escaped:?} must be one valid component on every OS"
            );
            assert!(!escaped.ends_with('.') && !escaped.ends_with(' '));
            assert_eq!(unescape_relative_path(&escaped).as_deref(), Some(relative));
            let name = root_dir_name("r", relative);
            assert_eq!(
                parse_root_dir_name(&name),
                Some(("r", relative.to_string()))
            );
        }
        assert_eq!(escape_relative_path("a/b%c"), "a%2Fb%25c");
    }

    #[test]
    fn non_ascii_project_names_travel_intact() {
        let mac = roots(&[("proyectos", "/Users/mateo/proyectos")]);
        let windows = roots(&[("proyectos", r"C:\Users\Mateo\proyectos")]);
        let path = "/Users/mateo/proyectos/Proyecto_\u{d1}and\u{fa}.v2";

        assert_eq!(
            root_repo_dir_name(&mac, path).as_deref(),
            Some("@proyectos@Proyecto_\u{d1}and\u{fa}.v2")
        );
        assert_eq!(
            carried(&mac, path, &windows).as_deref(),
            Some("C--Users-Mateo-proyectos-Proyecto--and--v2")
        );
    }

    #[test]
    fn names_that_are_not_root_names_are_left_to_the_other_rules() {
        for name in ["-home-x", "shop", "@@x", "@id", "@id@%zz", "@id@%4"] {
            assert_eq!(parse_root_dir_name(name), None, "{name}");
        }
        assert_eq!(
            local_project_dir(&rooted(&[("p", "/home/x")]), Path::new("/c"), "@p@..%2Fetc"),
            None,
            "a relative path cannot climb out of its root"
        );
    }

    #[test]
    fn another_machines_root_has_no_destination_until_mapped_here() {
        let filter = rooted(&[("proyectos", "/home/mateo/proyectos")]);
        let projects = PathBuf::from("/home/mateo/.claude/projects");

        assert_eq!(local_project_dir(&filter, &projects, "@trabajo@api"), None);
        assert_eq!(
            local_project_dir(&filter, &projects, "@proyectos@api"),
            Some(projects.join("-home-mateo-proyectos-api"))
        );
    }

    #[test]
    fn a_projects_real_path_comes_from_a_cwd_that_encodes_to_its_directory() {
        let projects = tempfile::TempDir::new().unwrap();
        let encoded = "-home-mateo-proyectos-shop-web";
        std::fs::create_dir_all(projects.path().join(encoded)).unwrap();
        std::fs::write(
            projects.path().join(encoded).join("s1.jsonl"),
            "{\"type\":\"summary\"}\n\
             {\"type\":\"user\",\"cwd\":\"/home/mateo/proyectos/shop-web/src\"}\n\
             {\"type\":\"user\",\"cwd\":\"/home/mateo/proyectos/shop-web\"}\n",
        )
        .unwrap();
        let filter = rooted(&[("proyectos", "/home/mateo/proyectos")]);

        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), encoded),
            "@proyectos@shop-web",
            "the cwd recorded after a `cd` does not encode to the directory and is skipped"
        );
        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), "-home-mateo-proyectos-empty"),
            "-home-mateo-proyectos-empty",
            "without a transcript the encoded name stays"
        );
    }

    #[test]
    fn a_project_only_pulled_so_far_is_placed_under_this_machines_root() {
        // The transcripts came from a Windows machine; nothing here has
        // recorded this machine's own path yet.
        let projects = tempfile::TempDir::new().unwrap();
        let encoded = "-home-mateo-proyectos-clientes-acme";
        std::fs::create_dir_all(projects.path().join(encoded)).unwrap();
        std::fs::write(
            projects.path().join(encoded).join("s1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"C:\\\\Users\\\\Mateo\\\\proyectos\\\\clientes\\\\acme\"}\n",
        )
        .unwrap();
        let filter = rooted(&[("proyectos", "/home/mateo/proyectos")]);

        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), encoded),
            "@proyectos@clientes%2Facme"
        );
    }

    #[test]
    fn the_project_map_beats_the_root_map() {
        let projects = tempfile::TempDir::new().unwrap();
        let encoded = "-home-mateo-proyectos-shop";
        std::fs::create_dir_all(projects.path().join(encoded)).unwrap();
        std::fs::write(
            projects.path().join(encoded).join("s1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/home/mateo/proyectos/shop\"}\n",
        )
        .unwrap();
        let mut filter = rooted(&[("proyectos", "/home/mateo/proyectos")]);
        filter.project_map.insert(
            "tienda".to_string(),
            PathBuf::from("/home/mateo/proyectos/shop"),
        );

        assert_eq!(
            repo_dir_name_in(&filter, projects.path(), encoded),
            "tienda"
        );
    }

    #[test]
    fn the_home_directory_is_an_implicit_root_unless_turned_off() {
        let mut filter = FilterConfig::default();
        assert!(filter.auto_home_root);
        let home = crate::sync::discovery::machine_home_dir().unwrap();
        assert_eq!(effective_roots(&filter).get(HOME_ROOT_ID), Some(&home));

        filter
            .root_map
            .insert(HOME_ROOT_ID.to_string(), PathBuf::from("/elsewhere"));
        assert_eq!(
            effective_roots(&filter).get(HOME_ROOT_ID),
            Some(&PathBuf::from("/elsewhere")),
            "an explicit `home` overrides the implicit one"
        );

        filter.root_map.clear();
        filter.auto_home_root = false;
        assert!(effective_roots(&filter).is_empty());
    }

    #[test]
    fn a_root_id_must_be_a_plain_name() {
        for bad in ["", ".", "..", "a/b", "a@b", "a b", "a%b", "c:"] {
            let map: RootMap = [(bad.to_string(), PathBuf::from("/home/x"))]
                .into_iter()
                .collect();
            assert!(validate_roots(&map).is_err(), "{bad:?} must be refused");
        }
        let relative: RootMap = [("p".to_string(), PathBuf::from("rel/dir"))]
            .into_iter()
            .collect();
        assert!(validate_roots(&relative).is_err());
        let fine: RootMap = [("proyectos".to_string(), PathBuf::from("/home/x"))]
            .into_iter()
            .collect();
        assert!(validate_roots(&fine).is_ok());
    }

    #[test]
    fn a_project_filed_under_an_unmapped_root_says_to_map_the_root() {
        let misses = skipped(&[("@trabajo@api", &["/repo/projects/@trabajo@api/s.jsonl"])]);
        let lines = skipped_project_warnings(&misses, false);
        assert!(lines[0].contains("--map-root"), "{}", lines[0]);
        assert!(lines[0].contains("trabajo"), "{}", lines[0]);
        assert!(!lines[0].contains("--map-project"), "{}", lines[0]);
    }

    #[test]
    fn colliding_ids_are_refused() {
        let map: ProjectMap = [
            ("one".to_string(), PathBuf::from("/home/user/app")),
            ("two".to_string(), PathBuf::from("/home/user/app")),
        ]
        .into_iter()
        .collect();
        assert!(ensure_no_encoding_collisions(&map).is_err());
        assert!(ensure_no_encoding_collisions(&ProjectMap::new()).is_ok());
    }
}
