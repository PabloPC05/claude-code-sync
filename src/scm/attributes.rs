//! `.gitattributes` in the sync repository.
//!
//! Every machine only ever appends to a transcript, to the prompt history and
//! to a memory index. Git cannot know that: when two machines both appended
//! between syncs, it reports a content conflict at the end of the file and the
//! pull stops with nothing merged. Git's own `union` merge driver keeps both
//! sides' lines instead, in no meaningful order. claude-code-sync puts them
//! back in order on the way into `~/.claude`: the prompt history and memory
//! indexes through `artifacts::union_jsonl` / `artifacts::memory_index`, and
//! session transcripts through the smart merge (`merge.rs`), which drops
//! duplicate UUIDs and sorts by timestamp. The next push writes that ordered
//! copy back to the repository.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const SYNC_ATTRIBUTE_RULES: &[&str] = &[
    "* text=auto eol=lf",
    "*.pdf binary",
    "*.bat text eol=crlf",
    "*.cmd text eol=crlf",
    "*.jsonl merge=union",
    "MEMORY.md merge=union",
];

/// Write the sync rules into the repository's `.gitattributes`.
///
/// Returns whether anything was added, so a caller can commit only a real
/// change.
pub fn ensure_sync_attributes(repo_path: &Path) -> Result<bool> {
    let rules: Vec<String> = SYNC_ATTRIBUTE_RULES.iter().map(|s| s.to_string()).collect();
    ensure_lines(&repo_path.join(".gitattributes"), &rules)
}

/// The rule that takes session transcripts out of the union merge:
/// `<sync_subdirectory>/*/*.jsonl -merge`, later than `*.jsonl merge=union`
/// and so overriding it.
///
/// Two machines that both continued one session must not have their lines
/// interleaved into one file: Claude Code would show only the latest
/// branch, and the other machine's messages would vanish from view. With
/// the rule, git reports the file as a conflict instead, and the pull
/// settles it the way it settles any diverged transcript (see
/// `crate::branches`). Subagent transcripts, one level deeper, and the
/// prompt history keep the union merge.
///
/// `None` for a subdirectory a pattern cannot spell plainly.
pub fn transcript_rule(sync_subdirectory: &str) -> Option<String> {
    let subdirectory = sync_subdirectory.replace('\\', "/");
    let subdirectory = subdirectory.trim_matches('/');
    let plain = !subdirectory.is_empty()
        && !subdirectory.starts_with(['!', '#'])
        && !subdirectory
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '*' | '?' | '[' | ']' | '"' | '\\'));
    plain.then(|| format!("{subdirectory}/*/*.jsonl -merge"))
}

/// Write [`transcript_rule`] into the repository's `.gitattributes`.
/// Returns whether anything was added.
pub fn ensure_transcript_rule(repo_path: &Path, sync_subdirectory: &str) -> Result<bool> {
    let Some(rule) = transcript_rule(sync_subdirectory) else {
        log::warn!(
            "sync_subdirectory {sync_subdirectory:?} cannot be written as a .gitattributes \
             pattern; transcripts two machines changed stay union-merged by git"
        );
        return Ok(false);
    };
    ensure_lines(&repo_path.join(".gitattributes"), &[rule])
}

/// Append every line that the file does not already contain, creating it if
/// needed. Returns whether the file changed.
pub fn ensure_lines(path: &Path, lines: &[String]) -> Result<bool> {
    let mut content = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let mut added = false;
    for line in lines {
        if content.lines().any(|existing| existing == line) {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(line);
        content.push('\n');
        added = true;
    }

    if added {
        fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    }

    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_sync_rules_are_written_once() {
        let repo = TempDir::new().unwrap();

        assert!(ensure_sync_attributes(repo.path()).unwrap());
        let written = fs::read_to_string(repo.path().join(".gitattributes")).unwrap();
        assert!(written.contains("* text=auto eol=lf"));
        assert!(written.contains("*.jsonl merge=union"));
        assert!(written.contains("MEMORY.md merge=union"));

        assert!(
            !ensure_sync_attributes(repo.path()).unwrap(),
            "a second run has nothing to add"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join(".gitattributes")).unwrap(),
            written
        );
    }

    #[test]
    fn transcripts_leave_the_union_merge_and_subagents_stay_in_it() {
        assert_eq!(
            transcript_rule("projects").as_deref(),
            Some("projects/*/*.jsonl -merge")
        );
        assert_eq!(
            transcript_rule("a/b/").as_deref(),
            Some("a/b/*/*.jsonl -merge")
        );
        assert_eq!(transcript_rule("my projects"), None);
        assert_eq!(transcript_rule("*"), None);

        let repo = TempDir::new().unwrap();
        ensure_sync_attributes(repo.path()).unwrap();
        assert!(ensure_transcript_rule(repo.path(), "projects").unwrap());
        assert!(!ensure_transcript_rule(repo.path(), "projects").unwrap());
        let written = fs::read_to_string(repo.path().join(".gitattributes")).unwrap();
        let union = written.find("*.jsonl merge=union").unwrap();
        let unset = written.find("projects/*/*.jsonl -merge").unwrap();
        assert!(union < unset, "the later line wins in .gitattributes");

        let check = |path: &str| {
            let output = std::process::Command::new("git")
                .args(["check-attr", "merge", "--", path])
                .current_dir(repo.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(check("projects/p/s.jsonl").ends_with("merge: unset\n"));
        assert!(check("projects/p/s/subagents/a.jsonl").ends_with("merge: union\n"));
        assert!(check("artifacts/prompt-history/history.jsonl").ends_with("merge: union\n"));
    }

    #[test]
    fn rules_are_added_beside_what_is_already_there() {
        let repo = TempDir::new().unwrap();
        let attributes = repo.path().join(".gitattributes");
        fs::write(&attributes, "*.png filter=lfs diff=lfs merge=lfs -text").unwrap();

        assert!(ensure_sync_attributes(repo.path()).unwrap());

        let written = fs::read_to_string(&attributes).unwrap();
        assert!(written.starts_with("*.png filter=lfs"));
        assert!(written.contains("*.jsonl merge=union"));
    }
}
