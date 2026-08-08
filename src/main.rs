//! Grove CLI.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "grove",
    version,
    about = "Local cargo host: warm seed, impact-scoped checks, worktree lanes",
    long_about = "Grove keeps cargo off per-branch ./target: one seed target per repo+toolchain, \
checks scoped to git changes, worktree lanes that hardlink registry artifacts from the seed.\n\n\
Edit:  grove check\n\
Prove: grove check --base auto"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// `cargo check --workspace` into this checkout's lane.
    Warm,
    /// Impact-routed `cargo check` (default edit-loop command).
    Check {
        #[arg(short = 'p', long)]
        package: Option<String>,
        /// Diff base: `HEAD` = dirty only; `auto` = vs origin/main.
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Impact-routed `cargo build`.
    Build {
        #[arg(short = 'p', long)]
        package: Option<String>,
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Impact-routed `cargo clippy`.
    Clippy {
        #[arg(short = 'p', long)]
        package: Option<String>,
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Impact-routed nextest / `cargo test`.
    Test {
        #[arg(short = 'p', long)]
        package: Option<String>,
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
    /// Any command with the lane target + incremental env.
    Exec {
        #[arg(last = true, allow_hyphen_values = true, required = true)]
        argv: Vec<String>,
    },
    /// Print bash/zsh `export …` lines so cargo / rust-analyzer use the same lane.
    Env,
    /// Git worktree acquire / release / list.
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Host + lane status (does not materialize).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Packages the next check/build would touch.
    Plan {
        #[arg(long, default_value = "HEAD")]
        base: String,
        #[arg(long)]
        json: bool,
    },
    /// Agent skill (install / print body).
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Reclaim dead lanes / enforce budget (seed wipe needs --seed --force).
    Clean {
        #[arg(long)]
        dry_run: bool,
        /// Remove this repo's seed (requires --force).
        #[arg(long)]
        seed: bool,
        #[arg(long)]
        force: bool,
        /// Remove grove-stamped worktree dirs not in `git worktree list`.
        #[arg(long)]
        orphans: bool,
        /// Sweep idle lanes under every repo host (default on auto hygiene).
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Write SKILL.md into harness skill dirs.
    Install,
    /// Print the packaged skill markdown.
    Print,
    /// Short note about the embedded skill.
    Path,
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// Create a worktree, or reattach if it already exists for this agent.
    Acquire {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// Discard existing worktree (even if dirty) and recreate.
        #[arg(long)]
        force: bool,
    },
    Release {
        path: PathBuf,
        /// Discard uncommitted work in the worktree.
        #[arg(long)]
        force: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => ExitCode::from(code.min(255) as u8),
        Err(e) => {
            eprintln!("grove: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn try_main() -> Result<i32> {
    let cli = Cli::parse();
    let ws = env::current_dir()?;
    match cli.cmd {
        Cmd::Warm => grove::warm(&ws),
        Cmd::Check { package, base } => grove::cargo_check(&ws, &base, package.as_deref()),
        Cmd::Build { package, base } => grove::cargo_build(&ws, &base, package.as_deref()),
        Cmd::Clippy { package, base } => grove::cargo_clippy(&ws, &base, package.as_deref()),
        Cmd::Test { package, base } => grove::cargo_test(&ws, &base, package.as_deref()),
        Cmd::Exec { argv } => {
            if argv.is_empty() {
                bail!("usage: grove exec -- <command>...");
            }
            grove::exec(&ws, &argv)
        }
        Cmd::Env => {
            print!("{}", grove::env_exports(&ws)?);
            Ok(0)
        }
        Cmd::Worktree { cmd } => match cmd {
            WorktreeCmd::Acquire {
                agent,
                branch,
                base,
                force,
            } => {
                let path = grove::WorktreeManager::open(&ws)?.acquire(
                    &agent,
                    branch.as_deref(),
                    base.as_deref(),
                    force,
                )?;
                println!("{}", path.display());
                Ok(0)
            }
            WorktreeCmd::Release { path, force } => {
                let out = grove::WorktreeManager::open(&ws)?.release(&path, force)?;
                println!("{}", serde_json::to_string(&out)?);
                Ok(0)
            }
            WorktreeCmd::List { json } => {
                let list = grove::WorktreeManager::open(&ws)?.list()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    for w in list {
                        match &w.branch {
                            Some(b) => println!("{}\t{b}", w.path),
                            None => println!("{}\t(detached)", w.path),
                        }
                    }
                }
                Ok(0)
            }
        },
        Cmd::Status { json } => {
            let st = grove::CacheHost::open(&ws)?.status(&ws)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!("workspace\t{}", st.workspace);
                println!("cache_root\t{}", st.cache_root);
                println!("repo_slug\t{}", st.repo_slug);
                println!("toolchain\t{}", st.toolchain);
                println!("lane\t{}", st.lane);
                println!("target_dir\t{}", st.target_dir);
            }
            Ok(0)
        }
        Cmd::Plan { base, json } => {
            let plan = grove::plan(&ws, &base)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else if plan.full {
                println!("full workspace (base {})", plan.base);
            } else if plan.packages.is_empty() {
                println!("no affected packages (base {})", plan.base);
            } else {
                for p in &plan.packages {
                    println!("{p}");
                }
            }
            Ok(0)
        }
        Cmd::Skill { cmd } => match cmd {
            SkillCmd::Install => {
                let paths = grove::skill_install()?;
                for p in &paths {
                    println!("{}", p.display());
                }
                eprintln!("grove: skill installed to {} location(s)", paths.len());
                Ok(0)
            }
            SkillCmd::Print => {
                grove::print_skill();
                Ok(0)
            }
            SkillCmd::Path => {
                println!("{}", grove::skill_describe());
                Ok(0)
            }
        },
        Cmd::Clean {
            dry_run,
            seed,
            force,
            orphans,
            all,
        } => {
            let host = grove::CacheHost::open(&ws)?;
            let mut opts = grove::CleanOpts::from_env();
            opts.dry_run = dry_run;
            opts.lanes = true;
            opts.seed = seed;
            opts.force = force;
            opts.all = all;
            opts.drop_dead_lanes = true;
            opts.protect_host = Some(host.host_root().to_path_buf());
            opts.protect_lane_keys = grove::live_lane_keys(&ws)?;
            opts.protect_lane_keys
                .push(grove::CacheHost::worktree_lane_key_for(&ws));
            let mut report = host.clean(&opts)?;
            if orphans {
                let o = host.clean_orphan_worktrees(&ws, dry_run)?;
                report.worktree_dirs_removed += o.worktree_dirs_removed;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(o.bytes_reclaimed);
                report.paths.extend(o.paths);
            }
            if dry_run {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                eprintln!(
                    "grove: clean lanes={} orphans={} seeds={} {}",
                    report.lanes_removed,
                    report.worktree_dirs_removed,
                    report.seeds_removed,
                    grove::format_bytes(report.bytes_reclaimed)
                );
                for p in &report.paths {
                    eprintln!("  removed {p}");
                }
                if report.over_budget_bytes > 0 {
                    eprintln!(
                        "grove: still {} over GROVE_CACHE_MAX_GB (live seed alone); \
                         raise the cap or `grove clean --seed --force` when idle",
                        grove::format_bytes(report.over_budget_bytes)
                    );
                }
            }
            Ok(0)
        }
    }
}
