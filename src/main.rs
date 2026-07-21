//! grove — agentic Rust build tooling.
//!
//! Built for the workflow where many AI agents each work in their own git worktree
//! and all need fast, isolated builds. grove gives every worktree an isolated build
//! lane seeded copy-on-write from one warm canonical, routes `check`/`test` to only
//! the packages a diff touches, and keeps the shared cache self-bounding on disk.

mod build_cli;
mod cli;
mod coordination_cli;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{ArtifactCmd, Cli, Cmd, ReleaseCmd, VerifyCmd};
use grove::api::Grove;
use grove::{
    cache, claim, config, doctor, impact, init, project, release, topology, verify, watch, worktree,
};
use std::path::{Path, PathBuf};

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("grove: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let workspace = detect_workspace();
    let config = config::Config::resolve(&workspace);
    let root = config.root();
    match cli.cmd {
        Cmd::Check {
            package,
            base,
            files,
        } => build_cli::build(
            &root,
            &workspace,
            &config,
            build_cli::Op::Check,
            package,
            base,
            files,
        ),
        Cmd::Test {
            package,
            lib,
            test,
            base,
            files,
        } => build_cli::build(
            &root,
            &workspace,
            &config,
            build_cli::Op::Test { lib, test },
            package,
            base,
            files,
        ),
        Cmd::Cache { action } => build_cli::cache(&root, &workspace, &config, action),
        Cmd::Watch => {
            let repo = project::repo_identity(&workspace);
            watch::watch(&root, &workspace, &repo, config.reap())?;
            Ok(0)
        }
        Cmd::Worktree { action } => coordination_cli::worktree(&root, &workspace, &config, action),
        Cmd::Exec { tag, command } => build_cli::exec(&root, &workspace, &config, &tag, command),
        Cmd::Config => config_cmd(&root, &workspace, &config),
        Cmd::Doctor => {
            if !project::is_cargo_workspace(&workspace) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "workspace": workspace,
                        "rust": null,
                        "note": "not a Cargo workspace: grove's coordination surface \
                                 (worktrees, claims, tasks, verify) works here; the Rust \
                                 acceleration suite is idle",
                    }))?
                );
                return Ok(0);
            }
            let report = doctor::report(&workspace)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        Cmd::Init => {
            let report = init::init(&workspace)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        Cmd::Claim {
            agent,
            task,
            branch,
            force,
            scope,
        } => {
            let repo = project::repo_identity(&workspace);
            let req = claim::ClaimRequest {
                root: &root,
                repo: &repo,
                workspace: Some(&workspace),
                agent,
                task,
                scope,
                branch,
                force,
            };
            let outcome = claim::claim(&req)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(match outcome {
                claim::ClaimOutcome::Granted { .. } => 0,
                claim::ClaimOutcome::Conflict { .. } => 1,
            })
        }
        Cmd::Release { action } => release_cmd(&root, &workspace, &config, action),
        Cmd::Status { json, watch } => {
            coordination_cli::status(&root, &workspace, &config, json, watch)
        }
        Cmd::Task { action } => coordination_cli::task(&root, &workspace, &config, action),
        Cmd::Verify {
            action: Some(VerifyCmd::Query { profile }),
            ..
        } => {
            let report = verify::query(&root, &workspace, &config, &profile)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        Cmd::Verify {
            action: None,
            profile,
            task_id,
        } => {
            let profile = profile.context("verify requires a profile")?;
            let report = verify::run(&root, &workspace, &config, &profile, task_id.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(if report.passed { 0 } else { 1 })
        }
        Cmd::Plan {
            base,
            json,
            topology,
            partition,
        } => plan_cmd(&workspace, &base, json, topology, partition),
        Cmd::Artifact { action } => artifact_cmd(&root, &workspace, &config, action),
    }
}

fn config_cmd(root: &Path, workspace: &Path, config: &config::Config) -> Result<i32> {
    let global = config::global_path();
    let repository =
        config::Config::repository(workspace).unwrap_or_else(|| workspace.join(".grove.toml"));
    let reserve = cache::reserve(root, config);
    let worktrees = worktree::placement(root, workspace, config).ok();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "workspace": workspace,
            "global_config": {
                "path": global.as_ref(),
                "present": global.as_ref().is_some_and(|path| path.exists()),
            },
            "repository_config": {
                "path": &repository,
                "present": repository.exists(),
            },
            "effective": {
                "cache_root": root,
                "min_free_gb": reserve / (1024 * 1024 * 1024),
                "min_free_bytes": reserve,
                "max_canonical_gb": config.budget(),
                "worktree_root": worktrees,
                "reap_ttl_secs": config.reap(),
                "claim_ttl_secs": config.claim(),
                "governor_mode": config.governor(),
                "cpu_slots": config.slots(),
                "max_builders": config.builders(),
                "keep_debuginfo": config.debuginfo(),
                "require_cow": config.cow(),
            },
        }))?
    );
    Ok(0)
}

fn plan_cmd(
    workspace: &Path,
    base: &str,
    json: bool,
    topology: bool,
    partition: bool,
) -> Result<i32> {
    if topology {
        let map = topology::topology(workspace)?;
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(0);
    }
    if partition {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .context("reading scope sets from stdin")?;
        let sets: Vec<topology::ScopeSet> =
            serde_json::from_str(&input).context("parsing scope sets JSON")?;
        let verdict = topology::partition(workspace, &sets)?;
        println!("{}", serde_json::to_string_pretty(&verdict)?);
        // Conflicts are a domain refusal: the partition needs revision.
        return Ok(if verdict.conflicts.is_empty() { 0 } else { 1 });
    }
    let files = impact::changed_files(workspace, base)?;
    let plan = impact::plan(workspace, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else if plan.full {
        println!("full workspace verification");
    } else if plan.groups.is_empty() {
        println!("no affected packages");
    } else {
        for (index, group) in plan.groups.iter().enumerate() {
            println!("group {}: {}", index + 1, group.packages.join(", "));
        }
    }
    Ok(0)
}

fn artifact_cmd(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    action: ArtifactCmd,
) -> Result<i32> {
    match action {
        ArtifactCmd::Export {
            tag,
            source,
            to,
            task_id,
            allow_unverified,
        } => {
            let grove = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone());
            let exported = verify::export(
                &grove,
                &tag,
                Path::new(&source),
                Path::new(&to),
                task_id.as_deref(),
                allow_unverified,
            )?;
            println!("{}", serde_json::to_string_pretty(&exported)?);
            Ok(0)
        }
    }
}

fn release_cmd(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    action: ReleaseCmd,
) -> Result<i32> {
    match action {
        ReleaseCmd::Claims { agent, scope } => {
            let repo = project::repo_identity(workspace);
            let outcome = claim::release(root, &repo, Some(workspace), &agent, &scope)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        ReleaseCmd::Freeze {
            task_id,
            profile,
            artifacts,
            out,
        } => {
            let report = release::freeze(
                root,
                workspace,
                config,
                &task_id,
                &profile,
                &artifacts,
                Path::new(&out),
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(0)
}

pub(crate) fn detect_workspace() -> PathBuf {
    project::workspace(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
