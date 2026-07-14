//! grove — agentic Rust build tooling.
//!
//! Built for the workflow where many AI agents each work in their own git worktree
//! and all need fast, isolated builds. grove gives every worktree an isolated build
//! lane seeded copy-on-write from one warm canonical, routes `check`/`test` to only
//! the packages a diff touches, and keeps the shared cache self-bounding on disk.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use grove::api::Grove;
use grove::{cache, claim, config, impact, project, status, task, watch, worktree};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "A build cache and worktree manager for Rust repos worked on by many agents at once."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Type-check the affected packages (routed from the git diff, or an explicit -p).
    Check {
        #[arg(short, long)]
        package: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        files: Option<String>,
    },
    /// Test the affected packages (routed), or one package's target with -p.
    Test {
        #[arg(short, long)]
        package: Option<String>,
        #[arg(long)]
        lib: bool,
        #[arg(long)]
        test: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        files: Option<String>,
    },
    /// Manage the shared cache.
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Prewarm every worktree's lane from the canonical, then watch for new
    /// worktrees and prewarm them the moment they appear (runs until interrupted).
    Watch,
    /// Manage the pool of agent worktrees.
    Worktree {
        #[command(subcommand)]
        action: WorktreeCmd,
    },
    /// Claim a scope of the repo (paths or crate:<name>) so a swarm avoids overlap.
    Claim {
        #[arg(long, default_value = "agent")]
        agent: String,
        #[arg(long, default_value = "")]
        task: String,
        #[arg(long)]
        branch: Option<String>,
        /// Proceed even if the scope overlaps another agent's claim.
        #[arg(long)]
        force: bool,
        /// Paths (repo-relative) or `crate:<name>` entries.
        #[arg(required = true)]
        scope: Vec<String>,
    },
    /// Release this agent's claims (all, or those overlapping the given scope).
    Release {
        #[arg(long, default_value = "agent")]
        agent: String,
        scope: Vec<String>,
    },
    /// Show what every agent is currently working on.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        watch: bool,
    },
    /// Manage a durable agent task and its claimed scope.
    Task {
        #[command(subcommand)]
        action: TaskCmd,
    },
    /// Run a command inside a seeded lane (e.g. a verify script), with the lane's
    /// isolated target/build dirs set. Use --tag for an independent, non-blocking lane.
    Exec {
        #[arg(long, default_value = "")]
        tag: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show the resolved configuration and where the config file lives.
    Config,
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Atomically claim scope and create a durable task record.
    Begin {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        task: String,
        #[arg(long, required = true, num_args = 1..)]
        scope: Vec<String>,
    },
    /// Run a command in the task's isolated tagged lane.
    Exec {
        #[arg(long)]
        task_id: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Mark a task finished and release its claim.
    Finish {
        #[arg(long)]
        task_id: String,
    },
    /// Preserve a task and its work while releasing its claim.
    Abandon {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// Assign a fresh, prewarmed worktree on its own branch; prints its path.
    Acquire {
        #[arg(long, default_value = "agent")]
        agent: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Return a worktree: salvage its work to its branch, remove it, drop its lane.
    Release {
        /// Path of the worktree to release.
        path: String,
    },
    /// List every grove-managed worktree, its lease owner, staleness, and dirty state.
    List,
    /// Reclaim abandoned worktrees (idle past the TTL, or already gone).
    Reap {
        #[arg(long)]
        ttl: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Collapse a worktree branch's commits since its base into one clean commit.
    Squash {
        /// Path of the worktree to squash.
        path: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Build the workspace and promote it to the canonical all lanes seed from.
    Warm,
    /// Promote the current lane to the canonical.
    Promote,
    /// Show cache disk usage and lanes.
    Status,
    /// Reclaim orphaned lanes and evict LRU lanes to clear the disk watermark.
    Gc,
}

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
    let root = cache::cache_root();
    match cli.cmd {
        Cmd::Check {
            package,
            base,
            files,
        } => build(&root, Op::Check, package, base, files),
        Cmd::Test {
            package,
            lib,
            test,
            base,
            files,
        } => build(&root, Op::Test { lib, test }, package, base, files),
        Cmd::Cache { action } => match action {
            CacheCmd::Warm => cache_warm(&root),
            CacheCmd::Promote => cache_promote(&root),
            CacheCmd::Status => {
                println!("{}", serde_json::to_string_pretty(&cache::status(&root))?);
                Ok(0)
            }
            CacheCmd::Gc => {
                println!("{}", serde_json::to_string_pretty(&cache::gc(&root))?);
                Ok(0)
            }
        },
        Cmd::Watch => {
            let ws = detect_workspace();
            let repo = project::repo_identity(&ws);
            watch::watch(&root, &ws, &repo)?;
            Ok(0)
        }
        Cmd::Worktree { action } => worktree_cmd(&root, action),
        Cmd::Exec { tag, command } => exec(&root, &tag, command),
        Cmd::Config => {
            let path = config::global_path();
            let present = path.as_ref().map(|p| p.exists()).unwrap_or(false);
            println!(
                "config file: {} ({})",
                path.map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown)".into()),
                if present {
                    "present"
                } else {
                    "not created; using defaults"
                }
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "cache_root": cache::cache_root().display().to_string(),
                    "min_free_gb": cache::min_free_floor() / (1024 * 1024 * 1024),
                    "max_canonical_gb": cache::max_canonical_gb()
                        .map(|gb| gb.to_string())
                        .unwrap_or_else(|| "(unbounded)".into()),
                    "reap_ttl_secs": worktree::reap_ttl(),
                    "claim_ttl_secs": claim::claim_ttl(),
                    "keep_debuginfo": config::keep_debuginfo(),
                    "require_cow": config::require_cow(),
                    "worktree_root": config::get().worktree_root.clone()
                        .unwrap_or_else(|| "(per-repo default)".into()),
                }))?
            );
            Ok(0)
        }
        Cmd::Claim {
            agent,
            task,
            branch,
            force,
            scope,
        } => {
            let repo = project::repo_identity(&detect_workspace());
            let req = claim::ClaimRequest {
                root: &root,
                repo: &repo,
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
        Cmd::Release { agent, scope } => {
            let repo = project::repo_identity(&detect_workspace());
            let outcome = claim::release(&root, &repo, &agent, &scope)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(0)
        }
        Cmd::Status { json, watch } => status_cmd(&root, json, watch),
        Cmd::Task { action } => task_cmd(&root, action),
    }
}

fn worktree_cmd(root: &Path, action: WorktreeCmd) -> Result<i32> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    match action {
        WorktreeCmd::Acquire {
            agent,
            branch,
            base,
        } => {
            let req = worktree::AcquireRequest {
                root,
                cwd: &cwd,
                agent,
                branch,
                base,
            };
            println!("{}", worktree::acquire(&req)?.display());
            Ok(0)
        }
        WorktreeCmd::Release { path } => {
            let outcome = worktree::release(root, Path::new(&path))?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(0)
        }
        WorktreeCmd::List => {
            println!("{}", serde_json::to_string_pretty(&worktree::list(root))?);
            Ok(0)
        }
        WorktreeCmd::Reap { ttl, dry_run } => {
            let report =
                worktree::reap(root, &cwd, ttl.unwrap_or_else(worktree::reap_ttl), dry_run)?;
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

fn task_cmd(root: &Path, action: TaskCmd) -> Result<i32> {
    let workspace = detect_workspace();
    let repo = project::repo_identity(&workspace);
    match action {
        TaskCmd::Begin {
            agent,
            task,
            scope,
        } => {
            let outcome = task::begin(task::Begin {
                root,
                workspace: &workspace,
                agent,
                description: task,
                scope,
            })?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(match outcome {
                task::BeginOutcome::Begun { .. } => 0,
                task::BeginOutcome::Conflict { .. } => 1,
            })
        }
        TaskCmd::Exec { task_id, command } => task::exec(root, &repo, &task_id, &command),
        TaskCmd::Finish { task_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&task::finish(root, &repo, &task_id)?)?
            );
            Ok(0)
        }
        TaskCmd::Abandon { task_id, reason } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&task::abandon(root, &repo, &task_id, reason)?)?
            );
            Ok(0)
        }
    }
}

fn status_cmd(root: &Path, json: bool, watch: bool) -> Result<i32> {
    loop {
        let report = status::report(root, &detect_workspace())?;
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

enum Op {
    Check,
    Test { lib: bool, test: Option<String> },
}

enum Route {
    Full,
    None,
    Packages(Vec<String>),
}

fn build(
    root: &Path,
    op: Op,
    package: Option<String>,
    base: Option<String>,
    files: Option<String>,
) -> Result<i32> {
    let grove = Grove::with_root(root.to_path_buf(), &cwd()?);
    let ws = grove.workspace();

    // Cheap reclamation before every build keeps the cache self-bounding.
    cache::reclaim_stale(root);
    cache::enforce_watermark(root);

    let args = match &op {
        Op::Check => match explicit_or_route(ws, &package, base, files)? {
            Selection::Explicit(pkg) => vec![s("check"), s("--locked"), s("-p"), pkg],
            Selection::Routed(Route::Full) => workspace_check_args(),
            Selection::Routed(Route::None) => {
                eprintln!("grove: no affected packages; nothing to check");
                return Ok(0);
            }
            Selection::Routed(Route::Packages(pkgs)) => {
                let mut a = vec![s("check"), s("--locked")];
                for p in pkgs {
                    a.push(s("-p"));
                    a.push(p);
                }
                a
            }
        },
        Op::Test { lib, test } => match explicit_or_route(ws, &package, base, files)? {
            Selection::Explicit(pkg) => {
                let mut a = vec![s("nextest"), s("run"), s("--locked"), s("-p"), pkg];
                if *lib {
                    a.push(s("--lib"));
                }
                if let Some(t) = test {
                    a.push(s("--test"));
                    a.push(t.clone());
                }
                a.push(s("--no-tests"));
                a.push(s("fail"));
                a
            }
            Selection::Routed(Route::Full) => vec![
                s("nextest"),
                s("run"),
                s("--workspace"),
                s("--locked"),
                s("--no-tests"),
                s("pass"),
            ],
            Selection::Routed(Route::None) => {
                eprintln!("grove: no affected packages; nothing to test");
                return Ok(0);
            }
            Selection::Routed(Route::Packages(pkgs)) => {
                let mut a = vec![s("nextest"), s("run"), s("--locked")];
                for p in pkgs {
                    a.push(s("-p"));
                    a.push(p);
                }
                a.push(s("--no-tests"));
                a.push(s("pass"));
                a
            }
        },
    };

    let lane = grove.seeded_lane()?;
    // Reclaim worktrees agents abandoned, so cleanup happens without a running daemon.
    // We already hold this worktree's lane, so reap skips it (its lock is taken) and
    // only removes others idle past the TTL.
    if let Ok(report) = worktree::reap(root, ws, worktree::reap_ttl(), false) {
        for w in &report.reaped {
            eprintln!("grove: reaped abandoned worktree {}", w.path);
        }
    }
    run_cargo(ws, &args, &lane)
}

enum Selection {
    Explicit(String),
    Routed(Route),
}

fn explicit_or_route(
    ws: &Path,
    package: &Option<String>,
    base: Option<String>,
    files: Option<String>,
) -> Result<Selection> {
    if let Some(pkg) = package {
        return Ok(Selection::Explicit(pkg.clone()));
    }
    let plan = if let Some(files) = files {
        let list: Vec<String> = files
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        impact::plan(ws, &list)?
    } else {
        let changed = impact::changed_files(ws, base.as_deref().unwrap_or("HEAD"))?;
        impact::plan(ws, &changed)?
    };
    Ok(Selection::Routed(if plan.full {
        Route::Full
    } else if plan.packages.is_empty() {
        Route::None
    } else {
        Route::Packages(plan.packages.into_iter().collect())
    }))
}

fn cache_warm(root: &Path) -> Result<i32> {
    let grove = Grove::with_root(root.to_path_buf(), &cwd()?);
    let ws = grove.workspace();
    let lane = grove.seeded_lane()?;

    // Check mode is required: it is the common fast-feedback loop and a failing check
    // means the workspace does not build, so there is nothing worth warming.
    let check = run_cargo(ws, &workspace_check_args(), &lane)?;
    if check != 0 {
        eprintln!("grove: workspace check failed; not warming the canonical");
        return Ok(check);
    }
    // Test binaries are best-effort: a project whose tests do not currently compile
    // still gets a warm check canonical instead of no canonical at all.
    let test_args = vec![
        s("nextest"),
        s("run"),
        s("--workspace"),
        s("--no-run"),
        s("--locked"),
    ];
    if run_cargo(ws, &test_args, &lane)? != 0 {
        eprintln!("grove: test binaries did not build; warming a check-only canonical");
    }
    grove.promote(&lane)?;
    cache::enforce_canonical_budget(root);
    println!("grove: canonical warmed at {}", grove.canonical().display());
    Ok(0)
}

fn cache_promote(root: &Path) -> Result<i32> {
    let grove = Grove::with_root(root.to_path_buf(), &cwd()?);
    let lane = grove.lane()?;
    grove.promote(&lane)?;
    cache::enforce_canonical_budget(root);
    println!(
        "grove: promoted lane to canonical at {}",
        grove.canonical().display()
    );
    Ok(0)
}

fn run_cargo(ws: &Path, args: &[String], lane: &cache::Lane) -> Result<i32> {
    let mut cmd = Command::new("cargo");
    cmd.args(args).current_dir(ws);
    cache::apply_env(&mut cmd, lane);
    let status = cmd.status().context("running cargo")?;
    Ok(status.code().unwrap_or(1))
}

fn run_in_lane(ws: &Path, program: &str, args: &[String], lane: &cache::Lane) -> Result<i32> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(ws);
    cache::apply_env(&mut cmd, lane);
    let status = cmd.status().with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

/// Seed a lane and run an arbitrary command in it, with the lane's target/build dirs
/// exported. This is how grove hosts a verify script that needs a stable isolated
/// target dir, without a separate cache tool.
fn exec(root: &Path, tag: &str, command: Vec<String>) -> Result<i32> {
    let grove = Grove::with_root(root.to_path_buf(), &cwd()?);
    cache::reclaim_stale(root);
    cache::enforce_watermark(root);
    let lane = grove.seeded_tagged_lane(tag)?;

    let (program, args) = command.split_first().context("exec requires a command")?;
    run_in_lane(grove.workspace(), program, args, &lane)
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().context("resolving current directory")
}

fn detect_workspace() -> PathBuf {
    project::workspace(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Targets grove checks and warms: every target except benches. Benches commonly need
/// nightly (`#![feature(test)]`) and are off an agent's path, so `--all-targets` — which
/// includes them — breaks `cache warm` on a stable toolchain. Warm and check must use
/// the identical set, or a seeded lane recompiles the difference.
fn workspace_check_args() -> Vec<String> {
    [
        "check",
        "--workspace",
        "--lib",
        "--bins",
        "--tests",
        "--examples",
        "--locked",
    ]
    .iter()
    .map(|v| v.to_string())
    .collect()
}

fn s(v: &str) -> String {
    v.to_string()
}
