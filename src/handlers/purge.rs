//! `claude-code-sync purge`: delete transcripts past the retention window from
//! the machine and the sync repository together.

use anyhow::{bail, Result};
use colored::Colorize;
use std::path::Path;

use crate::filter::FilterConfig;
use crate::purge::{self, PurgePlan, PurgeReport};
use crate::sync::{discovery::claude_projects_dir, SyncState};

/// Run a purge the user asked for.
///
/// Nothing is deleted until the plan has been shown: `dry_run` stops there, a
/// terminal asks for confirmation, and anywhere else `--yes` is required, so a
/// script cannot delete transcripts by accident.
pub fn handle_purge(older_than_days: Option<u32>, dry_run: bool, assume_yes: bool) -> Result<()> {
    let state = SyncState::load()?;
    let filter = FilterConfig::load()?;
    let claude_dir = crate::sync::discovery::claude_home_dir()?;
    let configured = older_than_days.or(filter.purge_older_than_days);
    let days = purge::retention_days(&claude_dir, configured);

    let plan = plan_in_scope(
        &filter,
        &claude_projects_dir()?,
        &state.sync_repo_path.join(&filter.sync_subdirectory),
        days,
    )?;

    describe(&plan);
    if plan.is_empty() || dry_run {
        return Ok(());
    }

    if !confirmed(&plan, assume_yes) {
        println!("  {}", "Nothing was deleted.".yellow());
        return Ok(());
    }

    // Opened and checked before anything is deleted: a repository that cannot
    // record the removal must stop the purge while every file still exists.
    let repo = purge::open_sync_repo(&state.sync_repo_path)?;
    purge::ensure_nothing_uncommitted(repo.as_ref())?;

    let report = purge::apply(&plan)?;
    let committed = purge::commit_removals(repo.as_ref(), &plan);
    report_outcome(&report, matches!(committed, Ok(true)));
    committed?;
    Ok(())
}

/// [`purge::plan`] limited to the sessions the include and exclude patterns
/// select: a project left out of sync is left alone by its purge too. A
/// local copy is judged where it is; a repository copy where it would land
/// on this machine.
fn plan_in_scope(
    filter: &FilterConfig,
    claude_projects: &Path,
    repo_projects: &Path,
    days: u32,
) -> Result<PurgePlan> {
    let mut plan = purge::plan(claude_projects, repo_projects, days)?;
    if filter.include_patterns.is_empty() && filter.exclude_patterns.is_empty() {
        return Ok(plan);
    }
    let selected = |path: &Path| -> bool {
        if path.starts_with(claude_projects) {
            return filter.patterns_select(claude_projects, path);
        }
        let Ok(relative) = path.strip_prefix(repo_projects) else {
            return false;
        };
        let local =
            crate::project_map::split_project_path(relative).and_then(|(project, inside)| {
                crate::project_map::local_project_dir(filter, claude_projects, project)
                    .map(|dir| dir.join(inside))
            });
        match local {
            Some(local) => filter.patterns_select(claude_projects, &local),
            None => filter.patterns_select(repo_projects, path),
        }
    };
    plan.targets.retain(|target| {
        target
            .paths
            .iter()
            .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
            .any(|path| selected(path))
    });
    Ok(plan)
}

/// Purge as part of a sync, when the user has turned that on. There is no
/// prompt — enabling `purge_after_sync` is the consent — and the removals are
/// left staged for the push that follows.
pub fn purge_after_sync(filter: &FilterConfig, repo_root: &Path) -> Result<()> {
    if !filter.purge_after_sync {
        return Ok(());
    }
    let claude_dir = crate::sync::discovery::claude_home_dir()?;
    let days = purge::retention_days(&claude_dir, filter.purge_older_than_days);
    let plan = plan_in_scope(
        filter,
        &claude_projects_dir()?,
        &repo_root.join(&filter.sync_subdirectory),
        days,
    )?;
    if plan.is_empty() {
        return Ok(());
    }

    let repo = purge::open_sync_repo(repo_root)?;
    purge::ensure_nothing_uncommitted(repo.as_ref())?;

    println!(
        "  {} {} transcripts older than {} days...",
        "Purging".cyan(),
        plan.targets.len(),
        plan.retention_days
    );
    let report = purge::apply(&plan)?;
    report_outcome(&report, false);
    Ok(())
}

fn describe(plan: &PurgePlan) {
    println!(
        "{}",
        format!(
            "=== Purge: transcripts older than {} days ===",
            plan.retention_days
        )
        .bold()
        .cyan()
    );
    if let Some(cutoff) = plan.cutoff {
        println!("  {} {}", "Cutoff:".bold(), cutoff.format("%Y-%m-%d"));
    }
    if plan.undated > 0 {
        println!(
            "  {} {} sessions have no readable date and are never purged",
            "•".dimmed(),
            plan.undated
        );
    }
    if plan.unreadable > 0 {
        println!(
            "  {} {} transcripts could not be read and were left alone",
            "•".dimmed(),
            plan.unreadable
        );
    }
    if plan.is_empty() {
        println!("  {} Nothing has aged out", "✓".green());
        return;
    }

    println!(
        "  {} {} sessions, {:.1} MB, oldest {} days",
        "•".cyan(),
        plan.targets.len(),
        plan.total_bytes() as f64 / (1024.0 * 1024.0),
        plan.oldest_age_days()
    );
    for target in plan.targets.iter().take(10) {
        println!(
            "    {} {} — last active {} ({} days)",
            "-".dimmed(),
            target.session_id,
            target.last_activity.format("%Y-%m-%d"),
            target.age_days
        );
    }
    if plan.targets.len() > 10 {
        println!("    {} and {} more", "-".dimmed(), plan.targets.len() - 10);
    }
    println!(
        "  {}",
        "Deleted from this machine AND from the sync repository (git history keeps them)".dimmed()
    );
}

fn confirmed(plan: &PurgePlan, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    if !crate::interactive_conflict::is_interactive() {
        println!(
            "  {} Re-run with {} to delete, or {} to see the list again",
            "!".yellow(),
            "--yes".bold(),
            "--dry-run".bold()
        );
        return false;
    }
    inquire::Confirm::new(&format!(
        "Delete {} sessions here and in the sync repo?",
        plan.targets.len()
    ))
    .with_default(false)
    .prompt()
    .unwrap_or(false)
}

fn report_outcome(report: &PurgeReport, committed: bool) {
    println!(
        "  {} Purged {} sessions ({} files, {:.1} MB)",
        "✓".green(),
        report.sessions,
        report.files,
        report.bytes as f64 / (1024.0 * 1024.0)
    );
    if committed {
        println!(
            "    {}",
            "Committed in the sync repository; push to share the removal".dimmed()
        );
    }
    for failure in &report.failures {
        println!("  {} {}", "!".yellow(), failure);
    }
}

/// `--older-than` may lengthen the retention window, never shorten it below
/// [`purge::MINIMUM_RETENTION_DAYS`].
pub fn validate_older_than(days: Option<u32>) -> Result<()> {
    if let Some(days) = days {
        if days < purge::MINIMUM_RETENTION_DAYS {
            bail!(
                "--older-than must be at least {} days",
                purge::MINIMUM_RETENTION_DAYS
            );
        }
    }
    Ok(())
}
