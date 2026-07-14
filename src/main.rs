//! grove — agentic Rust build tooling.
//!
//! Built for the workflow where many AI agents each work in their own git worktree
//! and all need fast, isolated builds. grove gives every worktree an isolated build
//! lane seeded copy-on-write from one warm canonical, routes `check`/`test` to only
//! the packages a diff touches, and keeps the shared cache self-bounding on disk.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use grove::{cache, impact};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "Agentic Rust build tooling: CoW worktree lanes, git-diff smart routing, a self-bounding shared cache."
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
                let reclaimed = cache::reclaim_stale(&root);
                let evicted = cache::enforce_watermark(&root);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "reclaimed": reclaimed,
                        "evicted": evicted,
                    }))?
                );
                Ok(0)
            }
        },
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
    let ws = detect_workspace();
    let tc = toolchain(&ws);
    let repo = repo_identity(&ws);
    let ws_str = ws.to_string_lossy().into_owned();

    // Cheap reclamation before every build keeps the cache self-bounding.
    cache::reclaim_stale(root);
    cache::enforce_watermark(root);

    let args = match &op {
        Op::Check => match explicit_or_route(&ws, &package, base, files)? {
            Selection::Explicit(pkg) => vec![s("check"), s("--locked"), s("-p"), pkg],
            Selection::Routed(Route::Full) => {
                vec![
                    s("check"),
                    s("--workspace"),
                    s("--all-targets"),
                    s("--locked"),
                ]
            }
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
        Op::Test { lib, test } => match explicit_or_route(&ws, &package, base, files)? {
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

    let lane = cache::acquire(root, &ws_str, &tc)?;
    let canonical = cache::canonical_dir(root, &repo, &tc);
    cache::seed(&lane, &canonical)?;
    run_cargo(&ws, &args, &lane)
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
    let ws = detect_workspace();
    let tc = toolchain(&ws);
    let repo = repo_identity(&ws);
    let ws_str = ws.to_string_lossy().into_owned();

    let lane = cache::acquire(root, &ws_str, &tc)?;
    let canonical = cache::canonical_dir(root, &repo, &tc);
    cache::seed(&lane, &canonical)?;

    // Seed both modes the lanes use: check-mode metadata cannot reuse build-mode
    // artifacts, and building test binaries activates dev-dep features.
    for args in [
        vec![
            s("check"),
            s("--workspace"),
            s("--all-targets"),
            s("--locked"),
        ],
        vec![
            s("nextest"),
            s("run"),
            s("--workspace"),
            s("--no-run"),
            s("--locked"),
        ],
    ] {
        let code = run_cargo(&ws, &args, &lane)?;
        if code != 0 {
            return Ok(code);
        }
    }
    cache::promote(&lane, &canonical)?;
    println!("grove: canonical warmed at {}", canonical.display());
    Ok(0)
}

fn cache_promote(root: &Path) -> Result<i32> {
    let ws = detect_workspace();
    let tc = toolchain(&ws);
    let repo = repo_identity(&ws);
    let ws_str = ws.to_string_lossy().into_owned();
    let lane = cache::acquire(root, &ws_str, &tc)?;
    let canonical = cache::canonical_dir(root, &repo, &tc);
    cache::promote(&lane, &canonical)?;
    println!(
        "grove: promoted lane to canonical at {}",
        canonical.display()
    );
    Ok(0)
}

fn run_cargo(ws: &Path, args: &[String], lane: &cache::Lane) -> Result<i32> {
    let mut cmd = Command::new("cargo");
    cmd.args(args).current_dir(ws);
    cmd.env("CARGO_TARGET_DIR", &lane.target_dir);
    // Keep intermediates in the lane too, or a project's own `build.build-dir`
    // config leaks them to a shared dir and the lane isolates nothing.
    cmd.env("CARGO_BUILD_BUILD_DIR", &lane.build_dir);
    // Agents never need backtraces or dSYM, so drop debuginfo: a large, safe
    // incremental-build win that would be wrong to force on a human's lane.
    cmd.env("CARGO_PROFILE_DEV_DEBUG", "0");
    cmd.env("CARGO_PROFILE_TEST_DEBUG", "0");
    if cfg!(target_os = "macos") {
        cmd.env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "off");
        cmd.env("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO", "off");
    }
    let status = cmd.status().context("running cargo")?;
    Ok(status.code().unwrap_or(1))
}

fn detect_workspace() -> PathBuf {
    if let Ok(out) = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
    {
        if out.status.success() {
            let manifest = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(dir) = Path::new(&manifest).parent() {
                if !manifest.is_empty() {
                    return dir.to_path_buf();
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn toolchain(ws: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(ws.join("rust-toolchain.toml")) {
        if let Some(chan) = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("channel")
                .and_then(|rest| rest.split('"').nth(1))
        }) {
            return chan.to_string();
        }
    }
    std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".to_string())
}

fn repo_identity(ws: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(ws)
        .output()
    {
        if out.status.success() {
            let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !common.is_empty() {
                return ws.join(common).to_string_lossy().into_owned();
            }
        }
    }
    ws.to_string_lossy().into_owned()
}

fn s(v: &str) -> String {
    v.to_string()
}
