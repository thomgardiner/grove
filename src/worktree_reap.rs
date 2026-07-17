use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::worktree_acquire::RepoContext;
use super::worktree_release;
use super::{
    Lease, activity, find_lease, leases, now_secs, preflight_salvage, reconcile, remove_worktree,
    repo_context, repo_git_lock, salvage_work,
};
use crate::{cache, git, task};

#[derive(Serialize)]
pub struct Reaped {
    pub path: String,
    pub branch: String,
    pub agent: String,
    /// The ref the worktree's uncommitted work was salvaged to, if it had any.
    pub saved_to: Option<String>,
    pub reason: String,
}

#[derive(Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct ReapReport {
    pub reaped: Vec<Reaped>,
    pub skipped: Vec<Skipped>,
    pub dry_run: bool,
}

fn reaped_of(lease: &Lease, saved_to: Option<String>, reason: String) -> Reaped {
    Reaped {
        path: lease.workspace.clone(),
        branch: lease.branch.clone(),
        agent: lease.agent.clone(),
        saved_to,
        reason,
    }
}

fn skipped(lease: &Lease, reason: String, task_ids: Vec<String>) -> Skipped {
    Skipped {
        path: lease.workspace.clone(),
        reason,
        task_ids,
    }
}

fn block(report: &mut ReapReport, lease: &Lease, reason: String) {
    report.skipped.push(skipped(lease, reason, Vec::new()));
}

fn workspace(lease: &Lease) -> PathBuf {
    cache::canonical_path(Path::new(&lease.workspace))
}

fn duplicates(leases: &[(PathBuf, Lease)]) -> BTreeSet<PathBuf> {
    let mut counts = BTreeMap::<PathBuf, usize>::new();
    for (_, lease) in leases {
        *counts.entry(workspace(lease)).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(workspace, count)| (count > 1).then_some(workspace))
        .collect()
}

fn authority(
    lease: &Lease,
    duplicates: &BTreeSet<PathBuf>,
    report: &mut ReapReport,
) -> Option<PathBuf> {
    let path = PathBuf::from(&lease.workspace);
    let canonical = workspace(lease);
    let reason = if path.as_os_str() != canonical.as_os_str() {
        Some("lease workspace is not canonical; refusing cleanup authority")
    } else if duplicates.contains(&canonical) {
        Some("multiple grove leases name this workspace; refusing ambiguous cleanup authority")
    } else {
        None
    };
    if let Some(reason) = reason {
        block(report, lease, reason.to_string());
        return None;
    }
    Some(path)
}

fn ownership(root: &Path, repo: &str, lease: &Lease, report: &mut ReapReport) -> bool {
    let blockers = match task::blockers(root, repo, Path::new(&lease.workspace)) {
        Ok(blockers) => blockers,
        Err(error) => {
            report.skipped.push(skipped(
                lease,
                format!("task ownership is unknown; left in place: {error:#}"),
                Vec::new(),
            ));
            return false;
        }
    };
    if blockers.ids().is_empty() {
        return true;
    }
    report.skipped.push(skipped(
        lease,
        format!(
            "nonterminal tasks block cleanup: {}",
            blockers.ids().join(", ")
        ),
        blockers.ids().to_vec(),
    ));
    false
}

fn same_generation(expected: &Lease, current: &Lease) -> bool {
    expected.workspace == current.workspace
        && expected.branch == current.branch
        && expected.agent == current.agent
        && expected.toolchain == current.toolchain
        && expected.repo == current.repo
        && expected.created_at == current.created_at
        && expected.generation == current.generation
        && expected.base_oid == current.base_oid
        && expected.materialization == current.materialization
}

fn current_candidate(
    root: &Path,
    file: &Path,
    expected: &Lease,
    ttl: u64,
    report: &mut ReapReport,
) -> Result<Option<(Lease, String)>> {
    let Some((current_file, current)) = find_lease(root, &expected.workspace)? else {
        block(
            report,
            expected,
            "cleanup authority disappeared while acquiring lifecycle locks".to_string(),
        );
        return Ok(None);
    };
    if current_file != file || !same_generation(expected, &current) {
        block(
            report,
            expected,
            "cleanup authority changed while acquiring lifecycle locks".to_string(),
        );
        return Ok(None);
    }
    let idle = now_secs().saturating_sub(activity(root, &current));
    if idle < ttl {
        block(
            report,
            &current,
            format!("lease renewed while cleanup waited: idle {idle}s, below ttl {ttl}s"),
        );
        return Ok(None);
    }
    Ok(Some((current, format!("idle {idle}s"))))
}

fn orphan_locked(
    root: &Path,
    ctx: &RepoContext,
    file: &Path,
    lease: &Lease,
    dry_run: bool,
    report: &mut ReapReport,
) -> Result<()> {
    let blockers =
        match worktree_release::locked_preflight(root, lease, Path::new(&lease.workspace), None) {
            Ok(blockers) => blockers,
            Err(error) => {
                block(
                    report,
                    lease,
                    format!("cleanup would be blocked; left in place: {error}"),
                );
                return Ok(());
            }
        };
    if dry_run {
        report
            .reaped
            .push(reaped_of(lease, None, "orphaned".to_string()));
        return Ok(());
    }
    fs::remove_file(file)?;
    cache::reclaim_stale(root);
    let _ = git::run(&ctx.main_root, &["worktree", "prune"]);
    crate::events::record(
        root,
        &ctx.repo_id,
        "worktree.reaped",
        serde_json::json!({"path": lease.workspace, "branch": lease.branch, "agent": lease.agent, "reason": "orphaned"}),
    );
    report
        .reaped
        .push(reaped_of(lease, None, "orphaned".to_string()));
    drop(blockers);
    Ok(())
}

type Candidate<'a> = (&'a Path, &'a Lease, &'a str);

fn preserve(lease: &Lease, report: &mut ReapReport) -> Option<Option<String>> {
    let saved = match salvage_work(Path::new(&lease.workspace), lease) {
        Ok(saved) => saved,
        Err(error) => {
            block(
                report,
                lease,
                format!("could not salvage work, left in place: {error}"),
            );
            return None;
        }
    };
    match preflight_salvage(Path::new(&lease.workspace), lease) {
        Ok(()) => Some(saved),
        Err(error) => {
            block(
                report,
                lease,
                format!("final removal check failed, left in place: {error}"),
            );
            None
        }
    }
}

fn worktree_locked(
    root: &Path,
    ctx: &RepoContext,
    (file, lease, reason): Candidate<'_>,
    dry_run: bool,
    report: &mut ReapReport,
) -> Result<()> {
    let blockers =
        match worktree_release::locked_preflight(root, lease, Path::new(&lease.workspace), None) {
            Ok(blockers) => blockers,
            Err(error) => {
                block(
                    report,
                    lease,
                    format!("cleanup would be blocked; left in place: {error}"),
                );
                return Ok(());
            }
        };
    if dry_run {
        report
            .reaped
            .push(reaped_of(lease, None, reason.to_string()));
        return Ok(());
    }
    let Some(saved) = preserve(lease, report) else {
        return Ok(());
    };
    if let Err(error) = remove_worktree(&ctx.main_root, Path::new(&lease.workspace)) {
        block(
            report,
            lease,
            format!("removal blocked, left in place: {error}"),
        );
        return Ok(());
    }
    fs::remove_file(file)?;
    cache::reclaim_stale(root);
    crate::events::record(
        root,
        &ctx.repo_id,
        "worktree.reaped",
        serde_json::json!({"path": lease.workspace, "branch": lease.branch, "agent": lease.agent, "reason": &reason, "saved_to": &saved}),
    );
    let reaped = reaped_of(lease, saved, reason.to_string());
    report.reaped.push(reaped);
    drop(blockers);
    Ok(())
}

fn stale(
    root: &Path,
    ctx: &RepoContext,
    (file, lease, _): Candidate<'_>,
    ttl: u64,
    dry_run: bool,
    report: &mut ReapReport,
) -> Result<()> {
    let Some(_lifecycle) =
        cache::lifecycle_try_exclusive(root, &lease.repo, Path::new(&lease.workspace))?
    else {
        block(
            report,
            lease,
            "active build or tagged lane holds the workspace".to_string(),
        );
        return Ok(());
    };
    let _git = repo_git_lock(root, &lease.repo)?;
    let Some((lease, reason)) = current_candidate(root, file, lease, ttl, report)? else {
        return Ok(());
    };
    if Path::new(&lease.workspace).exists() {
        worktree_locked(root, ctx, (file, &lease, &reason), dry_run, report)?;
    } else {
        orphan_locked(root, ctx, file, &lease, dry_run, report)?;
    }
    Ok(())
}

/// Salvage and reclaim expired leases for the repository containing `cwd`.
pub fn reap(root: &Path, cwd: &Path, ttl: u64, dry_run: bool) -> Result<ReapReport> {
    let ctx = repo_context(cwd)?;
    if !dry_run {
        reconcile(root, &ctx)?;
    }
    let now = now_secs();
    let mut report = ReapReport {
        dry_run,
        ..Default::default()
    };
    let leases = leases(root);
    let duplicates = duplicates(&leases);
    for (lease_file, lease) in leases {
        if lease.repo != ctx.repo_id {
            continue;
        }
        let Some(_path) = authority(&lease, &duplicates, &mut report) else {
            continue;
        };
        if !ownership(root, &ctx.repo_id, &lease, &mut report) {
            continue;
        }
        let idle = now.saturating_sub(activity(root, &lease));
        if idle < ttl {
            continue;
        }
        let reason = format!("idle {idle}s");
        stale(
            root,
            &ctx,
            (&lease_file, &lease, &reason),
            ttl,
            dry_run,
            &mut report,
        )?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_generation_distinguishes_same_second_reacquisitions() {
        let mut first: Lease = serde_json::from_value(serde_json::json!({
            "workspace": "/tmp/worktree", "branch": "grove/agent", "agent": "agent",
            "toolchain": "stable", "repo": "/tmp/repo/.git", "created_at": 1,
            "generation": "first", "base_oid": "abc"
        }))
        .unwrap();
        let mut second = first.clone();
        second.generation = "second".into();
        assert!(!same_generation(&first, &second));
        first.generation = "second".into();
        assert!(same_generation(&first, &second));
    }
}
