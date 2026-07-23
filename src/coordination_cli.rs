use crate::cli::{InspectCmd, TaskCmd, WorktreeCmd};
use anyhow::Result;
use grove::{config, project, recovery, status, task, verify, worktree};
use std::path::Path;

pub(crate) fn inspect(root: &Path, workspace: &Path, action: InspectCmd) -> Result<i32> {
    match action {
        InspectCmd::Acquire { task_id, ttl_secs } => {
            let report = grove::inspection::acquire(root, workspace, &task_id, ttl_secs)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        InspectCmd::Exec {
            capsule_id,
            timeout_secs,
            command,
        } => {
            let report =
                grove::inspection::exec(root, workspace, &capsule_id, &command, timeout_secs)?;
            let code = report.domain_exit();
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(code)
        }
        InspectCmd::Release { capsule_id } => {
            let report = grove::inspection::release(root, workspace, &capsule_id)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        InspectCmd::Reap { dry_run } => {
            let report = grove::inspection::reap(root, workspace, dry_run)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        InspectCmd::Worker { .. } => unreachable!("inspection worker dispatches before config"),
    }
}

pub(crate) fn worktree(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    action: WorktreeCmd,
) -> Result<i32> {
    match action {
        WorktreeCmd::Acquire {
            agent,
            branch,
            base,
            materialize,
        } => {
            let request = worktree::AcquireRequest {
                root,
                cwd: workspace,
                agent,
                branch,
                base,
            };
            let path = if materialize.is_empty() {
                worktree::bind(&request, config)?
            } else {
                worktree::scoped(&request, &materialize, config)?
            };
            println!("{}", path.display());
            Ok(0)
        }
        WorktreeCmd::Expand { path, scope } => {
            worktree::expand(root, Path::new(&path), &scope)?;
            Ok(0)
        }
        WorktreeCmd::Full { path } => {
            worktree::full(root, Path::new(&path))?;
            Ok(0)
        }
        WorktreeCmd::Release { path } => {
            let outcome = worktree::release(root, Path::new(&path))?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(0)
        }
        WorktreeCmd::List { all, json } => {
            let mut worktrees = worktree::list(root);
            if !all {
                let repo = project::repo_identity(workspace);
                worktrees.retain(|info| info.repo == repo);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&worktrees)?);
            } else if worktrees.is_empty() {
                let scope = if all { "" } else { " for this repository" };
                println!("no managed worktrees{scope}");
            } else {
                for info in &worktrees {
                    let state = if !info.exists {
                        "missing"
                    } else if info.dirty {
                        "dirty"
                    } else {
                        "clean"
                    };
                    let mut line = format!(
                        "{:<16} {:<24} {state:<7} idle={}s age={}s {}",
                        info.agent, info.branch, info.idle_secs, info.age_secs, info.path
                    );
                    if all {
                        line.push_str(&format!(" repo={}", info.repo));
                    }
                    println!("{line}");
                }
            }
            Ok(0)
        }
        WorktreeCmd::Heartbeat { path } => match worktree::heartbeat(root, Path::new(&path)) {
            Ok(lease) => {
                println!("{}", serde_json::to_string_pretty(&lease)?);
                Ok(0)
            }
            Err(error) => {
                eprintln!("grove: {error:#}");
                Ok(1)
            }
        },
        WorktreeCmd::Reap { ttl, dry_run } => {
            let report = worktree::reap(
                root,
                workspace,
                ttl.unwrap_or_else(|| config.reap()),
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        WorktreeCmd::Squash {
            path,
            base,
            message,
        } => {
            let outcome =
                worktree::squash(root, Path::new(&path), base.as_deref(), message.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(0)
        }
    }
}

pub(crate) fn task(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    action: TaskCmd,
) -> Result<i32> {
    let repo = project::repo_identity(workspace);
    match action {
        TaskCmd::Begin {
            agent,
            task: description,
            scope,
            claim_group,
        } => {
            let outcome = task::begin(task::Begin {
                root,
                workspace,
                agent,
                description,
                scope,
                claim_group,
            })?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(match outcome {
                task::BeginOutcome::Begun { .. } => 0,
                task::BeginOutcome::Conflict { .. } => 1,
            })
        }
        TaskCmd::Exec {
            task_id,
            capability,
            timeout_secs,
            command,
        } => task::exec(
            root,
            &repo,
            &task_id,
            &command,
            timeout_secs,
            capability.into(),
        ),
        TaskCmd::Finish {
            task_id,
            expected_source_sha256,
            allow_unverified,
            accept_policy,
        } => {
            let outcome = verify::finish_bound(
                root,
                &repo,
                config,
                &task_id,
                expected_source_sha256.as_deref(),
                allow_unverified.as_deref(),
                accept_policy.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(match outcome {
                verify::FinishOutcome::Finished(_) => 0,
                verify::FinishOutcome::Refused(_) => 1,
            })
        }
        TaskCmd::Status {
            task_id,
            active,
            json,
        } => {
            let report = status::task_report(root, workspace, task_id.as_deref(), active)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                status::print_tasks(&report);
            }
            Ok(0)
        }
        TaskCmd::Abandon { task_id, reason } => {
            let outcome = task::abandon(root, &repo, &task_id, reason)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(0)
        }
        TaskCmd::Reap { ttl, dry_run } => {
            let report = recovery::reap(
                root,
                workspace,
                ttl.unwrap_or_else(|| config.claim()),
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
    }
}

pub(crate) fn status(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    json: bool,
    watch: bool,
) -> Result<i32> {
    loop {
        let report = status::bound(root, workspace, config)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            status::print(&report);
        }
        if !watch {
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser;
    use grove::cache;
    use std::fs;
    use tempfile::tempdir;

    fn heartbeat_action(path: &Path) -> WorktreeCmd {
        let cli = Cli::try_parse_from(["grove", "worktree", "heartbeat", path.to_str().unwrap()])
            .unwrap();
        let Cmd::Worktree { action } = cli.cmd else {
            panic!("heartbeat argv parsed as another command")
        };
        action
    }

    fn write_lease(root: &Path, workspace: &Path) {
        let workspace = workspace.to_string_lossy().into_owned();
        let lease = worktree::Lease {
            workspace: workspace.clone(),
            branch: "grove/agent".into(),
            agent: "agent".into(),
            toolchain: "stable".into(),
            repo: "/repo/.git".into(),
            created_at: 1,
            generation: "fixture".into(),
            last_activity: 0,
            base_oid: "abc".into(),
            materialization: None,
        };
        let leases = root.join("leases");
        fs::create_dir_all(&leases).unwrap();
        cache::write_atomic(
            &leases.join(format!("{}.json", cache::lane_id(&workspace, "stable"))),
            &serde_json::to_vec_pretty(&lease).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn worktree_heartbeat_dispatches_and_refuses_unknown_path() {
        let base = tempdir().unwrap();
        let root = base.path().join("cache");
        let workspace = base.path().join("worktree");
        fs::create_dir_all(&workspace).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        write_lease(&root, &workspace);

        assert_eq!(
            worktree(
                &root,
                &workspace,
                &config::Config::default(),
                heartbeat_action(&workspace),
            )
            .unwrap(),
            0
        );

        let unknown = base.path().join("unknown");
        fs::create_dir_all(&unknown).unwrap();
        assert_eq!(
            worktree(
                &root,
                &workspace,
                &config::Config::default(),
                heartbeat_action(&unknown),
            )
            .unwrap(),
            1
        );
    }
}
