use anyhow::{Context, Result};
use colored::Colorize;
use inquire::Confirm;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::branches::{Reconciled, Side};
use crate::conflict::{ConflictDetector, ConflictResolution};
use crate::filter::FilterConfig;
use crate::history::{
    ConversationSummary, OperationHistory, OperationRecord, OperationType, SyncOperation,
};
use crate::interactive_conflict;
use crate::later_timestamps::keep_later_timestamps;
use crate::parser::ConversationSession;
use crate::report::{save_conflict_report, ConflictReport};
use crate::scm;
use crate::undo::Snapshot;

use super::discovery::{claude_home_dir, claude_projects_dir, discover_sessions, warn_large_files};
use super::state::SyncState;
use super::MAX_CONVERSATIONS_TO_DISPLAY;

/// The file in `~/.claude/projects` a transcript in the sync repository
/// belongs to, or `None` when this machine has no project for it.
///
/// This is the identity a pull pairs on. Two transcripts that share an
/// interior session id — a session resumed in another project, and every
/// subagent of a session — are different conversations living in different
/// files, and only the file says which copy is which.
pub(crate) fn local_destination(
    remote_session: &ConversationSession,
    remote_projects_dir: &Path,
    claude_dir: &Path,
    filter: &FilterConfig,
) -> Option<PathBuf> {
    let remote_relative = remote_session
        .file_path
        .strip_prefix(remote_projects_dir)
        .ok()?;
    let (repo_project_dir, inside_project) =
        crate::project_map::split_project_path(remote_relative)?;
    let local_project_dir =
        crate::project_map::local_project_dir(filter, claude_dir, repo_project_dir)?;

    Some(local_project_dir.join(inside_project))
}

/// A resolution saying how a transcript was reconciled.
fn reconciled(outcome: &str) -> ConflictResolution {
    ConflictResolution::Reconciled {
        outcome: outcome.to_string(),
    }
}

/// How the transcripts that differed were settled, for the summary.
#[derive(Debug, Default)]
struct TranscriptCounts {
    fast_forwarded: usize,
    kept_local: usize,
    combined: usize,
    branched: usize,
}

/// Writes what reconciling transcripts decides, and remembers it: which
/// files it wrote (the pass over non-conflicting sessions settles against
/// those instead of overwriting them) and which live sessions it left alone.
struct BranchWriter<'a> {
    live: &'a HashSet<String>,
    written: HashSet<PathBuf>,
    deferred: BTreeSet<String>,
    counts: TranscriptCounts,
}

impl<'a> BranchWriter<'a> {
    fn new(live: &'a HashSet<String>) -> Self {
        BranchWriter {
            live,
            written: HashSet::new(),
            deferred: BTreeSet::new(),
            counts: TranscriptCounts::default(),
        }
    }

    /// Whether the file (relative to the projects directory) belongs to a
    /// session running here; if so it is remembered as deferred.
    fn defer_if_live(&mut self, relative: &Path) -> bool {
        match crate::branches::owning_session_id(relative) {
            Some(session) if self.live.contains(session) => {
                self.deferred.insert(session.to_string());
                true
            }
            _ => false,
        }
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        crate::branches::write_file(path, bytes)?;
        self.written.insert(path.to_path_buf());
        Ok(())
    }

    /// Settle a repository transcript against a local file this pull
    /// already wrote. Returns whether the file changed.
    fn settle_branch(&mut self, path: &Path, session_id: &str, remote: &[u8]) -> Result<bool> {
        let live = self.live;
        let mut deferred = Vec::new();
        let written = crate::branches::write_branch(
            path,
            session_id,
            remote,
            &|id| live.contains(id),
            &mut deferred,
        )?;
        self.deferred.extend(deferred);
        let changed = !written.is_empty();
        self.written.extend(written);
        Ok(changed)
    }

    /// Split a session both machines continued: the winner's file stays
    /// `session_id`, the loser's becomes the session `branch_id` beside it,
    /// with a copy of the session's sidecar directory.
    fn split(
        &mut self,
        conflict: &mut crate::conflict::Conflict,
        session_id: &str,
        winner: Side,
        branch_id: &str,
        branch: &[u8],
        remote_bytes: &[u8],
    ) -> Result<()> {
        let local_project = conflict.local_file.parent().unwrap_or(Path::new("."));
        let remote_project = conflict.remote_file.parent().unwrap_or(Path::new("."));
        if winner == Side::Remote {
            self.write(&conflict.local_file, remote_bytes)?;
        }

        let branch_path = local_project.join(format!("{branch_id}.jsonl"));
        self.settle_branch(&branch_path, branch_id, branch)?;

        // The branch's sidecar: the losing side's files first. What the
        // repository already holds for the branch arrives with this pull.
        let local_sidecar = local_project.join(session_id);
        let remote_sidecar = remote_project.join(session_id);
        let sources = match winner {
            Side::Remote => [local_sidecar.as_path(), remote_sidecar.as_path()],
            Side::Local => [remote_sidecar.as_path(), local_sidecar.as_path()],
        };
        let remote_branch_sidecar = remote_project.join(branch_id);
        let copied = crate::branches::copy_sidecars(
            &sources,
            &local_project.join(branch_id),
            &|relative| remote_branch_sidecar.join(relative).exists(),
        )?;
        self.written.extend(copied);

        let loser = match winner {
            Side::Local => "remote",
            Side::Remote => "local",
        };
        println!(
            "  {} {} was continued on both machines: the {loser} continuation is now session {}",
            "⑂".cyan(),
            session_id,
            branch_id.cyan()
        );
        conflict.resolution = reconciled(&format!(
            "parallel branch: the {loser} continuation is now session {branch_id}"
        ));
        self.counts.branched += 1;
        Ok(())
    }

    fn print_counts(&self) {
        let counts = &self.counts;
        if counts.fast_forwarded + counts.kept_local + counts.combined + counts.branched == 0 {
            return;
        }
        println!(
            "  {} Transcripts: {} fast-forwarded, {} kept local, {} combined, {} parallel branch(es)",
            "✓".green(),
            counts.fast_forwarded,
            counts.kept_local,
            counts.combined,
            counts.branched
        );
    }

    /// Said at every verbosity: a deferred session is a sync not yet done.
    fn print_deferred(&self) {
        if self.deferred.is_empty() {
            return;
        }
        println!(
            "  {} Deferred {} live session(s); they sync once closed",
            "⏸".yellow(),
            self.deferred.len()
        );
        log::info!(
            "Deferred live sessions: {}",
            self.deferred.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
}

/// A transcript both sides of a sync-repository merge changed: settled the
/// way a pull settles it, so the repository never holds two machines'
/// continuations interleaved in one file. The losing continuation is
/// written beside it as its own session, for the next push to commit.
///
/// `None` for anything that is not a session transcript in the projects
/// directory, or that one side deleted: those keep their usual handling.
fn settle_transcript_conflict(
    file: &scm::ConflictedFile,
    repo_root: &Path,
    filter: &FilterConfig,
) -> Result<Option<scm::ConflictChoice>> {
    let (Some(local), Some(remote)) = (&file.local, &file.remote) else {
        return Ok(None);
    };
    let Ok(relative) = Path::new(&file.path).strip_prefix(&filter.sync_subdirectory) else {
        return Ok(None);
    };
    let Some(session_id) = crate::branches::top_level_session_id(relative) else {
        return Ok(None);
    };
    let project_dir = repo_root
        .join(&filter.sync_subdirectory)
        .join(relative.parent().unwrap_or(Path::new("")));

    let merged = match crate::branches::reconcile(session_id, local, remote) {
        Reconciled::Same | Reconciled::KeepLocal => local.clone(),
        Reconciled::TakeRemote => remote.clone(),
        Reconciled::Combined(bytes) => bytes,
        Reconciled::Split {
            winner,
            branch_id,
            branch,
        } => {
            let mut deferred = Vec::new();
            crate::branches::write_branch(
                &project_dir.join(format!("{branch_id}.jsonl")),
                &branch_id,
                &branch,
                &|_| false,
                &mut deferred,
            )?;
            crate::branches::copy_sidecars(
                &[&project_dir.join(session_id)],
                &project_dir.join(&branch_id),
                &|_| false,
            )?;
            println!(
                "  {} {}: continued on both machines, split into a parallel branch {}",
                "⑂".cyan(),
                file.path,
                branch_id
            );
            match winner {
                Side::Local => local.clone(),
                Side::Remote => remote.clone(),
            }
        }
    };
    Ok(Some(scm::ConflictChoice::WriteMerged(merged)))
}

/// The sync repository's transcripts this machine takes, judged by the
/// include and exclude patterns at the place each would land here: the
/// patterns are written for this machine's paths, and a repository
/// directory named by a root or a project id spells the project otherwise.
/// One with no place here is judged by its repository path, so the pull
/// can name it among the unmapped ones.
pub(crate) fn discover_repo_sessions(
    remote_projects_dir: &Path,
    claude_dir: &Path,
    filter: &FilterConfig,
) -> Result<Vec<ConversationSession>> {
    let sessions = discover_sessions(remote_projects_dir, &filter.without_path_patterns())?;
    Ok(sessions
        .into_iter()
        .filter(|session| {
            match local_destination(session, remote_projects_dir, claude_dir, filter) {
                Some(destination) => filter.patterns_select(claude_dir, &destination),
                None => filter.patterns_select(remote_projects_dir, &session.file_path),
            }
        })
        .collect())
}

fn settle_conflict(
    file: &scm::ConflictedFile,
    can_ask: bool,
    filter: &FilterConfig,
    repo_root: &Path,
) -> Result<scm::ConflictChoice> {
    if let Some(choice) = settle_transcript_conflict(file, repo_root, filter)? {
        return Ok(choice);
    }

    let differing_only_in_dates = match (&file.local, &file.remote) {
        (Some(local), Some(remote)) => keep_later_timestamps(local, remote),
        _ => None,
    };
    if let Some(merged) = differing_only_in_dates {
        println!("  {} {}: kept the later dates", "✓".green(), file.path);
        return Ok(scm::ConflictChoice::WriteMerged(merged));
    }

    if !can_ask {
        return Ok(scm::ConflictChoice::AbortMerge);
    }
    crate::merge_tool::resolve_conflict(&filter.merge_tool, filter.prefer_merge_tool, file)
}

/// Whether the artifact counts say anything worth a line: a kept-local-only
/// pull has zeros everywhere else, and all-zero rows read as "artifacts did
/// nothing". ONE rule for the early counts line and the summary row.
fn artifact_counts_say_something(report: &crate::artifacts::engine::ArtifactReport) -> bool {
    report.total_added() > 0 || report.total_modified() > 0 || report.total_deleted() > 0
}

/// Run a pull. Returns the number of artifact files kept local — what
/// `sync` gates its push step on, straight from the apply that just ran
/// instead of a second plan (which would re-read everything and could
/// fail after the pull has already mutated the machine).
pub fn pull_history(
    fetch_remote: bool,
    branch: Option<&str>,
    interactive: bool,
    verbosity: crate::VerbosityLevel,
    cancel_is_error: bool,
) -> Result<crate::artifacts::engine::ArtifactReport> {
    use crate::VerbosityLevel;

    if verbosity != VerbosityLevel::Quiet {
        println!("{}", "Pulling Claude Code history...".cyan().bold());
    }

    let state = SyncState::load()?;
    let repo = scm::open(&state.sync_repo_path)?;
    let filter = FilterConfig::load()?;
    let claude_dir = claude_projects_dir()?;

    // Get the current branch name for operation record
    let branch_name = branch
        .map(|s| s.to_string())
        .or_else(|| repo.current_branch().ok())
        .unwrap_or_else(|| "main".to_string());

    super::commit_sync_attributes(
        repo.as_ref(),
        &state.sync_repo_path,
        &filter.sync_subdirectory,
    )?;

    // Fetch from remote if configured
    if fetch_remote && state.has_remote {
        println!("  {} from remote...", "Fetching".cyan());

        // Merging a stale sync repository into ~/.claude looks like a
        // successful pull and silently loses whatever the remote holds, so a
        // remote that cannot be reached or reconciled stops the pull instead.
        let resolve_conflict = |file: &scm::ConflictedFile| {
            let can_ask = interactive_conflict::is_interactive();
            settle_conflict(file, can_ask, &filter, &state.sync_repo_path)
        };
        repo.pull("origin", &branch_name, &resolve_conflict)
            .context(
                "Nothing was merged into ~/.claude. Pull again in a terminal to choose \
                 a version of each file both machines changed, or fix the remote — or \
                 run `claude-code-sync pull --fetch-remote false` to merge only what is \
                 already in the local sync repository.",
            )?;
        println!("  {} Pulled from origin/{}", "✓".green(), branch_name);

        // A first pull into a repository with no commits of its own sets the
        // uncommitted rules aside; restore and commit them now.
        super::commit_sync_attributes(
            repo.as_ref(),
            &state.sync_repo_path,
            &filter.sync_subdirectory,
        )?;
    }

    // Sessions running here now: their transcripts are not rewritten.
    let live_sessions = crate::branches::live_session_ids(&claude_home_dir()?);

    // Discover local sessions
    println!("  {} local sessions...", "Discovering".cyan());
    let local_sessions = discover_sessions(&claude_dir, &filter)?;
    println!(
        "  {} {} local sessions",
        "Found".green(),
        local_sessions.len()
    );

    // Discover remote sessions
    let remote_projects_dir = state.sync_repo_path.join(&filter.sync_subdirectory);
    println!("  {} remote sessions...", "Discovering".cyan());
    let remote_sessions = discover_repo_sessions(&remote_projects_dir, &claude_dir, &filter)?;
    println!(
        "  {} {} remote sessions",
        "Found".green(),
        remote_sessions.len()
    );

    // ============================================================================
    // CONFLICT DETECTION (moved before snapshot for efficiency)
    // ============================================================================
    // Detect conflicts FIRST so we only backup files that will be modified
    if verbosity != VerbosityLevel::Quiet {
        println!("  {} conflicts...", "Detecting".cyan());
    }
    let local_by_path: HashMap<&Path, &ConversationSession> = local_sessions
        .iter()
        .map(|session| (session.file_path.as_path(), session))
        .collect();
    let paired: Vec<(&ConversationSession, &ConversationSession)> = remote_sessions
        .iter()
        .filter_map(|remote| {
            let destination =
                local_destination(remote, &remote_projects_dir, &claude_dir, &filter)?;
            let local = local_by_path.get(destination.as_path())?;
            Some((*local, remote))
        })
        .collect();

    let mut detector = ConflictDetector::new();
    detector.detect(&paired);

    // ============================================================================
    // ARTIFACT PULL PLAN (read-only, so the snapshot below can cover it)
    // ============================================================================
    let mut artifact_plan =
        crate::artifacts::engine::plan_pull(&claude_home_dir()?, &state.sync_repo_path, &filter)?;

    // ============================================================================
    // SNAPSHOT CREATION: Only backup files that will actually change
    // ============================================================================
    // Optimization: Only backup local files that have conflicts and will be merged,
    // plus artifact files this pull will overwrite. Files that are new (remote-only)
    // or unchanged don't need backup — created artifact paths are recorded as
    // deleted_files so undo removes them again.
    // This reduces snapshot size from potentially gigabytes to typically <1MB.
    // Only snapshot when the apply will actually change machine state:
    // conflicted conversations, artifact writes, or shared-record writes
    // (a pull whose only effect is creating a record still needs its
    // undo; a fully no-op pull must not churn a snapshot). Kept-local
    // files count as writes because an interactive apply may take the
    // repository copy over the local edit.
    // The same effective flag apply_pull gates prompts on: `-i` without a
    // TTY (cron, CI) cannot write kept-local files, and must not mint a
    // throwaway snapshot on every run for an unresolved edit.
    let prompts_possible = interactive && crate::interactive_conflict::is_interactive();
    // The snapshot object stays at hand after the apply: the apply learns
    // which creates actually EXECUTED, and the snapshot's undo-delete
    // list must be narrowed to them before the record is written.
    let mut created_snapshot: Option<(crate::undo::Snapshot, PathBuf)> = None;
    // Set once the snapshot is taken (and left None when nothing will
    // touch the machine); the narrowing below re-saves the snapshot
    // file in place — a failed narrowing pins the snapshot and warns
    // rather than dropping the undo hint.
    let snapshot_path: Option<PathBuf>;
    let mut session_unsnapshotted: Vec<std::path::PathBuf> = Vec::new();
    // Set when a shared record was unreadable at snapshot time and is
    // therefore not carried: if the apply still moves its keys (the file
    // became readable again), the undo cannot restore them and the
    // restored files will read as locally edited — the user needs the
    // recovery path, not just the snapshot-time note.
    let mut record_not_carried = false;
    if detector.has_conflicts() || artifact_plan.changes_machine_state(prompts_possible) {
        let mut files_to_snapshot: Vec<PathBuf> = detector
            .conflicts()
            .iter()
            .map(|c| c.local_file.clone())
            .collect();
        files_to_snapshot.extend(artifact_plan.paths_to_snapshot(prompts_possible));
        // The fail-open record contract: an unreadable shared record (a
        // root-owned or 0o000 file, say) reads as EMPTY everywhere else —
        // letting it abort the whole pull at snapshot time would make
        // every sync on that machine fatal over a file the records layer
        // itself ignores. Drop it from the snapshot instead: the undo
        // then treats the record as not carried and says so.
        //
        // Only the EXACT record paths this plan can write — the SAME
        // collapsed predicates the snapshot gate and the concurrent-sync
        // warnings use (a record is either carried or created, and each
        // pair collapses to one predicate): matching by name alone would
        // also drop an ordinary artifact that happens to share a
        // record's file name (the decoy case).
        let claude_dir = claude_home_dir()?;
        let declared_record_paths: Vec<PathBuf> = [
            artifact_plan
                .can_write_bases(prompts_possible)
                .then(|| crate::artifacts::bases::record_path(&claude_dir)),
            artifact_plan
                .rewrites_tracked
                .then(|| crate::artifacts::tracked::record_path(&claude_dir)),
        ]
        .into_iter()
        .flatten()
        .collect();
        files_to_snapshot.retain(|path| {
            if !declared_record_paths.contains(path) {
                return true;
            }
            // Open, not read: the bytes are discarded here (the snapshot
            // reads the file itself moments later) — only the
            // absent/unreadable/readable triage matters.
            match std::fs::File::open(path) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => {
                    // log::warn reaches the log file at any verbosity; the
                    // console line honors Quiet like its neighbors.
                    record_not_carried = true;
                    log::warn!(
                        "unreadable shared record {} — not snapshotted; undo pull cannot \
                         restore its entries",
                        path.display()
                    );
                    if verbosity != VerbosityLevel::Quiet {
                        println!(
                            "  {} unreadable shared record {} — not snapshotted; {}",
                            "⚠".yellow(),
                            path.display(),
                            "undo pull cannot restore its entries".yellow()
                        );
                    }
                    false
                }
            }
        });

        println!(
            "  {} snapshot of {} files to be modified...",
            "Creating".cyan(),
            files_to_snapshot.len()
        );

        // Check for large conversation files and warn users
        warn_large_files(&files_to_snapshot);

        // Create snapshot of ONLY files this pull will modify
        let mut snapshot = Snapshot::create(
            OperationType::Pull,
            files_to_snapshot.iter(),
            None, // No git manager needed for pull snapshots
        )
        .context("Failed to create snapshot before pull")?;

        // Artifact files the pull will create: undo deletes them again.
        snapshot.deleted_files = artifact_plan.created_paths();

        // The shared records (bases, tracked) are never deleted or
        // restored wholesale — other repositories may hold newer state in
        // the same files. Declare which ones this snapshot carries (or may
        // create); `undo pull` restores just this repository's entry.
        snapshot.attach_record_bookkeeping(&artifact_plan, &claude_dir, prompts_possible);

        // Save snapshot to disk
        let path = snapshot
            .save_to_disk(None)
            .context("Failed to save snapshot to disk")?;

        if verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} Snapshot created: {} ({} files)",
                "✓".green(),
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                files_to_snapshot.len()
            );
        }

        // Whatever the snapshot could not read is now on the plan: the
        // apply will refuse to modify those files — nothing is modified
        // without a backup, so a permission error at snapshot time that
        // recovers before the apply cannot become an unbackupable
        // modification.
        artifact_plan.unsnapshotted = snapshot
            .unreadable_files
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        // Session-merge needs the same refusal the artifact apply gets:
        // a conversation file whose backup failed is not rewritten by the
        // merge either.
        session_unsnapshotted = snapshot
            .unreadable_files
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        created_snapshot = Some((snapshot, path.clone()));
        snapshot_path = Some(path);
    } else {
        println!(
            "  {} Nothing to snapshot - skipping (no files change on disk)",
            "✓".green()
        );
        snapshot_path = None;
    };

    // ============================================================================
    // SHOW SUMMARY AND INTERACTIVE CONFIRMATION
    // ============================================================================
    if verbosity != VerbosityLevel::Quiet {
        println!();
        println!("{}", "Pull Summary:".bold().cyan());
        println!("  {} Local sessions: {}", "•".cyan(), local_sessions.len());
        println!(
            "  {} Remote sessions: {}",
            "•".cyan(),
            remote_sessions.len()
        );
        println!();
    }

    // Show detailed file list in verbose mode
    if verbosity == VerbosityLevel::Verbose {
        println!("{}", "Remote sessions to be pulled:".bold());
        for (idx, session) in remote_sessions.iter().enumerate().take(20) {
            let relative_path = session
                .file_path
                .strip_prefix(&remote_projects_dir)
                .unwrap_or(&session.file_path);

            println!(
                "  {}. {} ({} messages)",
                idx + 1,
                relative_path.display(),
                session.message_count()
            );
        }
        if remote_sessions.len() > 20 {
            println!("  ... and {} more", remote_sessions.len() - 20);
        }
        println!();
    }

    // Interactive confirmation
    if interactive && interactive_conflict::is_interactive() {
        let confirm = Confirm::new("Pull?")
            .with_default(true)
            .prompt()
            .context("Failed to get confirmation")?;

        if !confirm {
            println!("\n{}", "Pull cancelled.".yellow());
            // Under `sync`, a cancelled pull must abort the whole command:
            // returning "nothing kept" would send the gate straight into
            // a full push that publishes local edits over the
            // repository's newer versions. A bare pull stays a friendly
            // Ok — cancelling is a normal outcome there.
            // The snapshot was written before the confirmation: it is
            // owned by no operation record now (the pull never applied,
            // no record is written) — delete it instead of leaving an
            // orphan for age/count cleanup to find.
            if let Some(orphan) = snapshot_path.as_ref() {
                let _ = std::fs::remove_file(orphan);
            }
            if cancel_is_error {
                return Err(anyhow::anyhow!("Pull cancelled"));
            }
            return Ok(crate::artifacts::engine::ArtifactReport::default());
        }
    }

    // ============================================================================
    // CONFLICT RESOLUTION (detection already done above)
    // ============================================================================
    // Track affected conversations for operation record
    let mut affected_conversations: Vec<ConversationSummary> = Vec::new();
    let mut branches = BranchWriter::new(&live_sessions);

    if detector.has_conflicts() {
        println!(
            "  {} {} transcript(s) differ from the sync repository",
            "!".yellow(),
            detector.conflict_count()
        );

        let remote_by_path: HashMap<&Path, &ConversationSession> = remote_sessions
            .iter()
            .map(|session| (session.file_path.as_path(), session))
            .collect();

        // `-i` in a terminal asks about every diverged session instead of
        // splitting it; anything else settles on its own.
        let ask = interactive && interactive_conflict::is_interactive();
        let mut to_ask: Vec<crate::conflict::Conflict> = Vec::new();
        let mut smart_merge_success_count = 0;
        let mut smart_merge_attempts = 0;
        let mut smart_merge_failed_conflicts = Vec::new();

        for conflict in detector.conflicts_mut() {
            // No backup, no merge (see `session_unsnapshotted`): the
            // conflict is left unresolved — reported as failed — instead
            // of overwriting a file nothing could restore.
            if session_unsnapshotted.contains(&conflict.local_file) {
                // Not added to the failed-conflicts list either: the
                // interactive resolver would rewrite the same unbacked
                // file. The conflict stays detected-but-unresolved, said
                // in the log.
                log::warn!(
                    "Leaving conflict {} unresolved (no pre-pull backup could be taken)",
                    conflict.session_id
                );
                continue;
            }
            let relative = conflict
                .local_file
                .strip_prefix(&claude_dir)
                .unwrap_or(&conflict.local_file)
                .to_path_buf();
            // A running session is still being written: never rewrite it
            // under Claude Code. Its turn comes on a later sync.
            if branches.defer_if_live(&relative) {
                continue;
            }
            // The two transcripts this conflict is between, by their own paths:
            // an interior session id is shared by every subagent of a session,
            // and by a session resumed in another project.
            let (Some(local_session), Some(remote_session)) = (
                local_by_path.get(conflict.local_file.as_path()),
                remote_by_path.get(conflict.remote_file.as_path()),
            ) else {
                continue;
            };

            let local_bytes = std::fs::read(&conflict.local_file)
                .with_context(|| format!("Failed to read {}", conflict.local_file.display()))?;
            let remote_bytes = std::fs::read(&conflict.remote_file)
                .with_context(|| format!("Failed to read {}", conflict.remote_file.display()))?;
            let resumable = crate::branches::top_level_session_id(&relative).map(str::to_string);
            let session_id = resumable.as_deref().unwrap_or(&conflict.session_id);

            match crate::branches::reconcile(session_id, &local_bytes, &remote_bytes) {
                Reconciled::Same | Reconciled::KeepLocal => {
                    branches.counts.kept_local += 1;
                    conflict.resolution = reconciled("kept local: it holds every remote entry");
                }
                Reconciled::TakeRemote => {
                    branches.write(&conflict.local_file, &remote_bytes)?;
                    branches.counts.fast_forwarded += 1;
                    conflict.resolution = reconciled("fast-forwarded to the remote copy");
                }
                Reconciled::Combined(bytes) => {
                    branches.write(&conflict.local_file, &bytes)?;
                    branches.counts.combined += 1;
                    conflict.resolution =
                        reconciled("combined: one side's messages, the other's metadata");
                }
                Reconciled::Split {
                    winner,
                    branch_id,
                    branch,
                } => {
                    let Some(session_id) = resumable.as_deref() else {
                        // A subagent transcript is not resumed on its own:
                        // it keeps the smart merge.
                        smart_merge_attempts += 1;
                        match conflict.smart_merge_into_local_file(local_session, remote_session) {
                            Ok(()) => {
                                smart_merge_success_count += 1;
                                if let crate::conflict::ConflictResolution::SmartMerge {
                                    ref stats,
                                } = conflict.resolution
                                {
                                    println!(
                                        "  {} Smart merged {} ({} local + {} remote = {} total, {} branches)",
                                        "✓".green(),
                                        conflict.session_id,
                                        stats.local_messages,
                                        stats.remote_messages,
                                        stats.merged_messages,
                                        stats.branches_detected
                                    );
                                }
                            }
                            Err(e) => {
                                log::warn!("Smart merge failed for {}: {}", conflict.session_id, e);
                                log::info!("Falling back to manual resolution...");
                                smart_merge_failed_conflicts.push(conflict.clone());
                            }
                        }
                        continue;
                    };
                    if ask {
                        to_ask.push(conflict.clone());
                        continue;
                    }
                    branches.split(
                        conflict,
                        session_id,
                        winner,
                        &branch_id,
                        &branch,
                        &remote_bytes,
                    )?;
                }
            }
        }

        if smart_merge_attempts > 0 {
            println!(
                "  {} Successfully smart merged {}/{} subagent transcripts",
                "✓".green(),
                smart_merge_success_count,
                smart_merge_attempts
            );
        }

        if !to_ask.is_empty() {
            println!(
                "\n{} {} session(s) were continued on both machines",
                "→".cyan(),
                to_ask.len()
            );
            let local_by_id: HashMap<String, &ConversationSession> = to_ask
                .iter()
                .filter_map(|c| local_by_path.get(c.local_file.as_path()))
                .map(|s| (s.session_id.clone(), *s))
                .collect();
            let remote_by_id: HashMap<String, &ConversationSession> = to_ask
                .iter()
                .filter_map(|c| remote_by_path.get(c.remote_file.as_path()))
                .map(|s| (s.session_id.clone(), *s))
                .collect();
            let mut result = interactive_conflict::resolve_transcript_conflicts_interactive(
                &mut to_ask,
                &local_by_id,
                &remote_by_id,
            )?;
            for conflict in result.parallel_branch.iter_mut() {
                let local_bytes = std::fs::read(&conflict.local_file)?;
                let remote_bytes = std::fs::read(&conflict.remote_file)?;
                let session_id = conflict.session_id.clone();
                if let Reconciled::Split {
                    winner,
                    branch_id,
                    branch,
                } = crate::branches::reconcile(&session_id, &local_bytes, &remote_bytes)
                {
                    branches.split(
                        conflict,
                        &session_id,
                        winner,
                        &branch_id,
                        &branch,
                        &remote_bytes,
                    )?;
                }
            }
            let renames = interactive_conflict::apply_resolutions(
                &result,
                &remote_sessions,
                &claude_dir,
                &remote_projects_dir,
            )?;
            for (_, renamed) in &renames {
                branches.written.insert(renamed.clone());
            }
            // The report shows what was chosen for each.
            for chosen in result.all() {
                if let Some(conflict) = detector
                    .conflicts_mut()
                    .iter_mut()
                    .find(|c| c.local_file == chosen.local_file)
                {
                    conflict.resolution = chosen.resolution.clone();
                }
            }
        }

        // If some smart merges failed, handle them with interactive/keep-both resolution
        let renames = if !smart_merge_failed_conflicts.is_empty() {
            println!(
                "  {} {} conflicts require manual resolution",
                "!".yellow(),
                smart_merge_failed_conflicts.len()
            );

            // Check if we can run interactively
            let use_interactive = crate::interactive_conflict::is_interactive();

            if use_interactive {
                // Interactive conflict resolution for failed merges
                println!(
                    "\n{} Running in interactive mode for remaining conflicts",
                    "→".cyan()
                );

                let resolution_result = crate::interactive_conflict::resolve_conflicts_interactive(
                    &mut smart_merge_failed_conflicts,
                )?;

                // Apply the resolutions
                crate::interactive_conflict::apply_resolutions(
                    &resolution_result,
                    &remote_sessions,
                    &claude_dir,
                    &remote_projects_dir,
                )?
            } else {
                // Non-interactive mode: use "keep both" strategy for failed merges
                println!(
                    "\n{} Using automatic conflict resolution (keep both versions)",
                    "→".cyan()
                );

                let mut renames = Vec::new();

                println!("\n{}", "Conflict Resolution:".yellow().bold());
                for conflict in &smart_merge_failed_conflicts {
                    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let conflict_suffix = format!("conflict-{timestamp}");

                    if let Ok(renamed_path) = conflict.clone().resolve_keep_both(&conflict_suffix) {
                        let relative_renamed = renamed_path
                            .strip_prefix(&claude_dir)
                            .unwrap_or(&renamed_path);
                        println!(
                            "  {} remote version saved as: {}",
                            "→".yellow(),
                            relative_renamed.display().to_string().cyan()
                        );

                        // Find and write the remote session
                        if let Some(session) = remote_sessions
                            .iter()
                            .find(|s| s.session_id == conflict.session_id)
                        {
                            session.copy_to(&renamed_path)?;
                        }

                        renames.push((conflict.remote_file.clone(), renamed_path));
                    }
                }

                renames
            }
        } else {
            Vec::new()
        };

        // Track all conflicts in affected conversations
        for (_original_path, renamed_path) in &renames {
            let relative_path = renamed_path
                .strip_prefix(&claude_dir)
                .unwrap_or(renamed_path)
                .to_string_lossy()
                .to_string();

            // Find the session ID from the renamed path
            if let Some(session) = remote_sessions.iter().find(|s| {
                let session_file = s.file_path.file_name();
                let renamed_file = renamed_path.file_name();
                // Try to match based on session ID in filename
                session_file
                    .and_then(|f| f.to_str())
                    .and_then(|name| name.split('-').next())
                    == renamed_file
                        .and_then(|f| f.to_str())
                        .and_then(|name| name.split('-').next())
            }) {
                match ConversationSummary::new(
                    session.session_id.clone(),
                    relative_path.clone(),
                    session.latest_timestamp().map(str::to_string),
                    session.message_count(),
                    SyncOperation::Conflict,
                ) {
                    Ok(summary) => affected_conversations.push(summary),
                    Err(e) => log::warn!(
                        "Failed to create summary for conflict {}: {}",
                        relative_path,
                        e
                    ),
                }
            }
        }

        branches.print_counts();

        // Save conflict report
        let report = ConflictReport::from_conflicts(detector.conflicts());
        save_conflict_report(&report)?;

        println!(
            "\n{} View details with: claude-code-sync report",
            "Hint:".cyan()
        );
    } else {
        println!("  {} No conflicts detected", "✓".green());
    }

    // ============================================================================
    // MERGE NON-CONFLICTING SESSIONS
    // ============================================================================
    println!("  {} non-conflicting sessions...", "Merging".cyan());
    let mut merged_count = 0;
    let mut added_count = 0;
    let mut modified_count = 0;
    let mut unchanged_count = 0;
    let mut skipped_no_local_match = 0;
    let mut skipped_by_project = crate::project_map::SkippedByProject::new();

    for remote_session in &remote_sessions {
        let remote_relative = remote_session
            .file_path
            .strip_prefix(&remote_projects_dir)
            .unwrap_or(&remote_session.file_path);

        let destination =
            local_destination(remote_session, &remote_projects_dir, &claude_dir, &filter);
        let Some(dest_path) = destination else {
            match crate::project_map::split_project_path(remote_relative) {
                Some((repo_project_dir, _)) => {
                    skipped_no_local_match += 1;
                    skipped_by_project
                        .entry(repo_project_dir.to_string())
                        .or_default()
                        .push(remote_session.file_path.clone());
                }
                None => log::warn!(
                    "Skipping {} (not a transcript inside a project directory)",
                    remote_session.file_path.display()
                ),
            }
            continue;
        };

        // A conflicting transcript is merged above, not overwritten here.
        if detector
            .conflicts()
            .iter()
            .any(|conflict| conflict.local_file == dest_path)
        {
            continue;
        }

        let relative_path_for_tracking = dest_path
            .strip_prefix(&claude_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| remote_relative.to_path_buf());

        // Determine operation type based on local state
        let operation = if let Some(local) = local_by_path.get(dest_path.as_path()) {
            if local.content_hash() == remote_session.content_hash() {
                unchanged_count += 1;
                SyncOperation::Unchanged
            } else if branches.defer_if_live(&relative_path_for_tracking) {
                continue;
            } else {
                modified_count += 1;
                SyncOperation::Modified
            }
        } else if dest_path.exists() {
            // Not there when this pull looked, there now: a branch the
            // split above derived — the same one the other machine pushed
            // — or a file that appeared meanwhile. Settle, never overwrite.
            if branches.defer_if_live(&relative_path_for_tracking) {
                continue;
            }
            let remote_bytes = std::fs::read(&remote_session.file_path)?;
            let stem = remote_session.session_id.clone();
            if branches.settle_branch(&dest_path, &stem, &remote_bytes)? {
                modified_count += 1;
                merged_count += 1;
            } else {
                unchanged_count += 1;
            }
            continue;
        } else {
            added_count += 1;
            SyncOperation::Added
        };

        // Copy file if it's not unchanged
        if operation != SyncOperation::Unchanged {
            remote_session.copy_to(&dest_path)?;
            merged_count += 1;
        }

        // Track all sessions (including unchanged) in affected conversations
        let relative_path_str = relative_path_for_tracking.to_string_lossy().to_string();
        match ConversationSummary::new(
            remote_session.session_id.clone(),
            relative_path_str.clone(),
            remote_session.latest_timestamp().map(str::to_string),
            remote_session.message_count(),
            operation,
        ) {
            Ok(summary) => affected_conversations.push(summary),
            Err(e) => log::warn!("Failed to create summary for {}: {}", relative_path_str, e),
        }
    }

    println!("  {} Merged {} sessions", "✓".green(), merged_count);

    crate::project_map::merge_skipped(&mut skipped_by_project, &artifact_plan.unmapped_projects);
    for line in crate::project_map::skipped_project_warnings(
        &skipped_by_project,
        filter.warn_each_skipped_file,
    ) {
        log::warn!("{line}");
    }

    // ============================================================================
    // APPLY ARTIFACT PULL PLAN (remote wins; snapshot already covers changes)
    // ============================================================================
    let artifact_report = crate::artifacts::engine::apply_pull(&artifact_plan, interactive)?;
    // Undo deletes only what the apply actually CREATED: a user-written
    // file that appeared mid-pull is deliberately skipped (a push
    // publishes it) and must survive the undo.
    if let Some((snapshot, path)) = created_snapshot.as_mut() {
        let executed: Vec<String> = artifact_report
            .created_abs_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        // Only a SKIPPED create needs the rewrite (it spares the skipped
        // path from the undo's delete list); an unchanged list skips the
        // full re-serialize of what can be megabytes entirely.
        if executed != snapshot.deleted_files {
            snapshot.deleted_files = executed;
            if let Err(e) = snapshot.save_to_disk(path.parent()) {
                // The re-save failed, so the on-disk snapshot keeps its
                // PLANNED delete list — undo could then delete a file the
                // pull never wrote (a mid-pull create). Deleting the file
                // would discard the only backups of files the pull DID
                // overwrite — a worse, unconditional loss. Keep it, pin
                // it in memory if the re-save allows, and say exactly
                // what to check before undoing.
                let reason = "the snapshot could not be updated; undo pull would restore \
                              the overwritten files but its delete list was NOT narrowed — \
                              check it for files this pull never wrote before undoing";
                log::warn!("Snapshot undo-delete list not narrowed: {e}; {reason}");
                if verbosity != VerbosityLevel::Quiet {
                    println!("  {} {}", "⚠".yellow(), reason.yellow());
                }
                // Pin it so the regular cleanup spares the sole backups
                // — the re-save may have failed transiently, so one
                // pinned re-write is worth attempting; if even that
                // fails, the warning above is all the user gets.
                snapshot.pinned = true;
                if let Err(pin_err) = snapshot.save_to_disk(path.parent()) {
                    log::warn!(
                        "Snapshot could not be pinned either: {pin_err} — it may be \
                         deleted by the regular cleanup"
                    );
                }
            }
        }
    }
    // The counts line only when it says something: a kept-local-only
    // pull has zeros everywhere else, and "0 created, 0 overwritten,
    // 0 deleted" reads as "artifacts did nothing". The kept-local hint
    // itself must survive every outcome — it IS the pull's result.
    if !artifact_plan.is_empty() && artifact_counts_say_something(&artifact_report) {
        println!(
            "  {} Artifacts: {} created, {} overwritten locally, {} deleted locally",
            "✓".green(),
            artifact_report.total_added(),
            artifact_report.total_modified(),
            artifact_report.total_deleted()
        );
    }
    if verbosity != VerbosityLevel::Quiet && artifact_report.total_kept_local() > 0 {
        let kept = artifact_report.total_kept_local();
        let clean = artifact_report.kept_local_clean;
        let plural = if kept == 1 { "" } else { "s" };
        if clean == kept {
            // Every keep protected nothing — a write that failed, an edit
            // reverted while the pull ran. `sync`'s push skips these files
            // per-file, so the older local bytes cannot overwrite a newer
            // repository copy — but a BARE `push` has no skip set and
            // would republish the older local bytes over the repo's newer
            // version. Say both facts.
            println!(
                "  {} {} file{plural} held local with no edit to publish — \
                 sync holds them back; a bare push would republish their older bytes",
                "✋".yellow(),
                kept,
            );
        } else if clean > 0 {
            // Mixed: the dirty subset has real local edits to
            // publish; the clean subset matches the base. No command
            // selectively publishes one subset: a bare `push`
            // publishes the edits AND republishes the clean files'
            // older bytes over the repo's newer versions. Advise
            // pulling again first so the transient ones settle, then
            // pushing.
            println!(
                "  {} {} file{plural} kept local: {} edited, {} transient — \
                 pull again to settle the transient ones, then push to publish the edits",
                "✋".yellow(),
                kept,
                kept - clean,
                clean,
            );
        } else {
            println!(
                "  {} {} file{plural} kept local: edited here since the last sync — push to publish",
                "✋".yellow(),
                kept,
            );
        }
    }
    // KeptLocalDelete: the repo's deletion was honored (the local
    // edit is gone) — the standalone `pull` is the place to mention
    // it because the next `sync` would have nothing to publish for
    // these files. Skipping this block when the previous kept-local
    // block already ran would suppress the deletion notice.
    let kept_local_deletes = artifact_report.total_kept_local_deletes();
    if verbosity != VerbosityLevel::Quiet && kept_local_deletes > 0 {
        let plural = if kept_local_deletes == 1 { "" } else { "s" };
        println!(
            "  {} {} file{plural} removed locally: the repo's deletion was honored — the next pull will prune the record",
            "🗑".yellow(),
            kept_local_deletes,
        );
    }
    // Only when THIS pull has a snapshot: a snapshotless record (the
    // demotion above) is skipped by undo, which would silently target
    // the previous pull and revert changes the user did not ask about.
    if (artifact_report.total_modified() > 0 || artifact_report.total_deleted() > 0)
        && snapshot_path.is_some()
    {
        println!("    {}", "Undo with: claude-code-sync undo pull".dimmed());
    }
    // The snapshot gate predicts at plan time; the apply re-decides from a
    // fresh load. When a concurrent same-repo sync made the apply move
    // record keys the snapshot never carried, this pull's undo cannot
    // restore that record's pre-pull state — say so instead of leaving a
    // silently weaker undo.
    // `prompts_possible`, not `interactive`: the snapshot gate and the
    // declarations used it, and `pull -i` without a TTY mints nothing —
    // evaluating the kept-local term here would suppress the warning
    // exactly when it applies.
    // ONE shape for both records: the apply moved keys the snapshot's
    // declarations never carried (a concurrent sync's commits), so this
    // pull's undo cannot restore that record's pre-pull state. The bases
    // gate keeps the interactive term (the snapshot gate used
    // prompts_possible, and `pull -i` without a TTY mints nothing);
    // tracked writes are fully predictable, so rewrites_tracked alone.
    let warn_concurrent = |keys_moved: &[String], carried: bool, record: &str| {
        if verbosity != VerbosityLevel::Quiet && !keys_moved.is_empty() && !carried {
            println!(
                "  {} a concurrent sync changed the artifact {record} record during this pull; {}",
                "⚠".yellow(),
                "undo pull cannot restore its pre-pull state".yellow()
            );
        }
    };
    if record_not_carried
        && (!artifact_report.bases_keys_written.is_empty()
            || !artifact_report.tracked_keys_written.is_empty())
    {
        // The record the apply DID move is the one the snapshot cannot
        // restore: after an undo, the restored files read as locally
        // edited (their bases still say post-pull). One push re-records
        // the true bases and clears it — say so instead of leaving the
        // warned-but-destructive path to be discovered.
        println!(
            "  {} a shared record was unreadable this pull and the apply still updated it; \
             after an undo, run {} to re-record the bases (restored files would otherwise \
             read as locally edited)",
            "⚠".yellow(),
            "claude-code-sync push".cyan()
        );
    }
    warn_concurrent(
        &artifact_report.bases_keys_written,
        artifact_plan.can_write_bases(prompts_possible),
        "base",
    );
    warn_concurrent(
        &artifact_report.tracked_keys_written,
        artifact_plan.rewrites_tracked,
        "tracked",
    );
    // ============================================================================
    // CREATE AND SAVE OPERATION RECORD
    // ============================================================================
    let mut operation_record = OperationRecord::new(
        OperationType::Pull,
        Some(branch_name.clone()),
        affected_conversations.clone(),
    );

    // Attach the snapshot path to the operation record (only if we created one)
    operation_record.snapshot_path = snapshot_path;
    operation_record.artifact_counts = artifact_report.counts.clone();
    // Exactly the keys the apply moved in each shared record: the undo
    // restores those and only those — a superset would revert entries a
    // later push legitimately recorded. None on older records reads as
    // the snapshot's full declared set (the conservative superset).
    operation_record.bases_keys_written = Some(artifact_report.bases_keys_written.clone());
    operation_record.tracked_keys_written = Some(artifact_report.tracked_keys_written.clone());
    // Undo scopes its artifact-record surgery to this repository alone.
    operation_record.repo_path = Some(state.sync_repo_path.clone());

    // Load operation history and add this operation
    let mut history = match OperationHistory::load() {
        Ok(h) => h,
        Err(e) => {
            log::warn!("Failed to load operation history: {}", e);
            log::info!("Creating new history...");
            OperationHistory::default()
        }
    };

    if let Err(e) = history.add_operation(operation_record) {
        log::warn!("Failed to save operation to history: {}", e);
        log::info!("Pull completed successfully, but history was not updated.");
    }

    // ============================================================================
    // DISPLAY SUMMARY TO USER
    // ============================================================================
    println!("\n{}", "=== Pull Summary ===".bold().cyan());

    // Show operation statistics
    let conflict_count = detector.conflict_count();
    let stats_msg = format!(
        "  {} Added    {} Modified    {} Conflicts    {} Unchanged",
        format!("{added_count}").green(),
        format!("{modified_count}").cyan(),
        format!("{conflict_count}").yellow(),
        format!("{unchanged_count}").dimmed(),
    );
    println!("{stats_msg}");
    if skipped_no_local_match > 0 {
        println!(
            "  {} Skipped sessions (no local match): {}",
            "!".yellow(),
            skipped_no_local_match
        );
    }
    branches.print_deferred();
    // Same zero-noise rule as the earlier counts line (plus unchanged,
    // which the summary row carries): a kept-local-only pull prints its
    // KEEP hint, not an all-zeros summary row.
    if artifact_counts_say_something(&artifact_report) || artifact_plan.unchanged > 0 {
        println!(
            "  {} Artifacts: {} added, {} modified, {} deleted, {} unchanged",
            "•".cyan(),
            artifact_report.total_added(),
            artifact_report.total_modified(),
            artifact_report.total_deleted(),
            artifact_plan.unchanged
        );
    }
    println!();

    // Group conversations by project (top-level directory)
    let mut by_project: HashMap<String, Vec<&ConversationSummary>> = HashMap::new();
    for conv in &affected_conversations {
        // Skip unchanged conversations in detailed output
        if conv.operation == SyncOperation::Unchanged {
            continue;
        }

        let project = conv
            .project_path
            .split('/')
            .next()
            .unwrap_or("unknown")
            .to_string();
        by_project.entry(project).or_default().push(conv);
    }

    // Display conversations grouped by project
    if !by_project.is_empty() {
        println!("{}", "Affected Conversations:".bold());

        let mut projects: Vec<_> = by_project.keys().collect();
        projects.sort();

        for project in projects {
            let conversations = &by_project[project];
            println!("\n  {} {}/", "Project:".bold(), project.cyan());

            for conv in conversations.iter().take(MAX_CONVERSATIONS_TO_DISPLAY) {
                let operation_str = match conv.operation {
                    SyncOperation::Added => "ADD".green(),
                    SyncOperation::Modified => "MOD".cyan(),
                    SyncOperation::Conflict => "CONFLICT".yellow(),
                    SyncOperation::Unchanged => "---".dimmed(),
                };

                let timestamp_str = conv
                    .timestamp
                    .as_ref()
                    .and_then(|t| {
                        // Extract just the date portion for compact display
                        t.split('T').next()
                    })
                    .unwrap_or("unknown");

                println!(
                    "    {} {} ({}msg, {})",
                    operation_str,
                    conv.project_path,
                    conv.message_count,
                    timestamp_str.dimmed()
                );
            }

            if conversations.len() > MAX_CONVERSATIONS_TO_DISPLAY {
                println!(
                    "    {} ... and {} more conversations",
                    "...".dimmed(),
                    conversations.len() - MAX_CONVERSATIONS_TO_DISPLAY
                );
            }
        }
    }

    super::print_artifact_changes(&artifact_report);

    println!("\n{}", "Pull complete!".green().bold());

    // Clean up old snapshots automatically
    if let Err(e) = crate::undo::cleanup_old_snapshots(None, false) {
        log::warn!("Failed to cleanup old snapshots: {}", e);
    }

    Ok(artifact_report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflicted(local: Option<&str>, remote: Option<&str>) -> scm::ConflictedFile {
        scm::ConflictedFile {
            path: "artifacts/plugins/known_marketplaces.json".to_string(),
            base: None,
            local: local.map(|text| text.as_bytes().to_vec()),
            remote: remote.map(|text| text.as_bytes().to_vec()),
        }
    }

    #[test]
    fn a_conflict_settles_without_asking_only_when_the_dates_alone_differ() {
        let filter = FilterConfig::default();
        let cases = [
            (
                "dates alone differ",
                conflicted(
                    Some("\"lastUpdated\": \"2026-10-01T06:00:01.741Z\""),
                    Some("\"lastUpdated\": \"2026-10-01T06:50:50.567Z\""),
                ),
                scm::ConflictChoice::WriteMerged(
                    b"\"lastUpdated\": \"2026-10-01T06:50:50.567Z\"".to_vec(),
                ),
            ),
            (
                "content differs too",
                conflicted(
                    Some("a 2026-10-01T06:00:01Z"),
                    Some("b 2026-10-01T06:00:02Z"),
                ),
                scm::ConflictChoice::AbortMerge,
            ),
            (
                "deleted on one side",
                conflicted(Some("a"), None),
                scm::ConflictChoice::AbortMerge,
            ),
        ];

        for (name, file, expected) in cases {
            let choice = settle_conflict(&file, false, &filter, Path::new("/repo")).unwrap();
            assert_eq!(choice, expected, "{name}");
        }
    }
    fn git_in(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn transcript_line(uuid: &str, parent: &str, minute: u32, text: &str) -> String {
        format!(
            "{{\"parentUuid\":\"{parent}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\
             \"content\":\"{text}\"}},\"uuid\":\"{uuid}\",\"timestamp\":\"2026-11-17T10:{minute:02}:00.000Z\",\
             \"sessionId\":\"{SESSION}\"}}\n"
        )
    }

    const SESSION: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn a_transcript_both_machines_pushed_is_split_rather_than_interleaved() {
        let root = tempfile::TempDir::new().unwrap();
        git_in(root.path(), &["init", "--bare", "--quiet", "origin"]);
        let transcript = format!("projects/-p/{SESSION}.jsonl");
        let prefix = transcript_line("p1", "none", 0, "hola");

        let first = root.path().join("first");
        let second = root.path().join("second");
        git_in(root.path(), &["clone", "--quiet", "origin", "first"]);
        git_in(&first, &["config", "user.name", "First"]);
        git_in(&first, &["config", "user.email", "first@local"]);
        crate::scm::attributes::ensure_sync_attributes(&first).unwrap();
        crate::scm::attributes::ensure_transcript_rule(&first, "projects").unwrap();
        std::fs::create_dir_all(first.join("projects/-p")).unwrap();
        std::fs::write(first.join(&transcript), &prefix).unwrap();
        let machine = scm::open(&first).unwrap();
        machine.stage_all().unwrap();
        machine.commit("start").unwrap();
        let branch = machine.current_branch().unwrap();
        machine.push("origin", &branch).unwrap();

        git_in(root.path(), &["clone", "--quiet", "origin", "second"]);
        git_in(&second, &["config", "user.name", "Second"]);
        git_in(&second, &["config", "user.email", "second@local"]);
        let from_second = format!("{prefix}{}", transcript_line("b1", "p1", 5, "desde B"));
        std::fs::write(second.join(&transcript), &from_second).unwrap();
        let other = scm::open(&second).unwrap();
        other.stage_all().unwrap();
        other.commit("B continues").unwrap();
        other.push("origin", &branch).unwrap();

        let from_first = format!("{prefix}{}", transcript_line("a1", "p1", 9, "desde A"));
        std::fs::write(first.join(&transcript), &from_first).unwrap();
        machine.stage_all().unwrap();
        machine.commit("A continues").unwrap();

        let filter = FilterConfig::default();
        let resolve = |file: &scm::ConflictedFile| settle_conflict(file, false, &filter, &first);
        machine.pull("origin", &branch, &resolve).unwrap();

        assert_eq!(
            std::fs::read_to_string(first.join(&transcript)).unwrap(),
            from_second,
            "the continuation that started first keeps the session, uninterleaved"
        );
        let branch_id = crate::branches::branch_session_id(SESSION, "a1");
        let branch_file = first.join(format!("projects/-p/{branch_id}.jsonl"));
        let branch_text = std::fs::read_to_string(&branch_file).unwrap();
        assert!(branch_text.contains("desde A") && !branch_text.contains("desde B"));
        assert!(!branch_text.contains(SESSION));
        assert!(!machine.has_unfinished_merge(), "the merge is committed");
    }
}
