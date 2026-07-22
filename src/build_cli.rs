use crate::cli::CacheCmd;
use anyhow::{Context, Result};
use grove::api::Grove;
use grove::{cache, config, fingerprint, impact, project, seed, worktree};
use std::path::Path;
use std::process::Command;

pub(crate) enum Op {
    Check,
    Test { lib: bool, test: Option<String> },
}

enum Route {
    Full,
    None,
    Packages(Vec<String>),
}

enum Selection {
    Explicit(String),
    Routed(Route),
}

pub(crate) fn build(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    op: Op,
    package: Option<String>,
    base: Option<String>,
    files: Option<String>,
) -> Result<i32> {
    let grove = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone());
    let workspace = grove.workspace();
    let selection = select(workspace, &package, base, files)?;
    let args = match &op {
        Op::Check => match &selection {
            Selection::Explicit(package) => {
                vec![s("check"), s("--locked"), s("-p"), package.clone()]
            }
            Selection::Routed(Route::Full) => workspace_check_args(),
            Selection::Routed(Route::None) => {
                eprintln!("grove: no affected packages; nothing to check");
                return Ok(0);
            }
            Selection::Routed(Route::Packages(packages)) => {
                let mut args = vec![s("check"), s("--locked")];
                for package in packages {
                    args.push(s("-p"));
                    args.push(package.clone());
                }
                args
            }
        },
        Op::Test { lib, test } => match &selection {
            Selection::Explicit(package) => {
                let mut args = vec![
                    s("nextest"),
                    s("run"),
                    s("--locked"),
                    s("-p"),
                    package.clone(),
                ];
                if *lib {
                    args.push(s("--lib"));
                }
                if let Some(test) = test {
                    args.push(s("--test"));
                    args.push(test.clone());
                }
                args.push(s("--no-tests"));
                args.push(s("fail"));
                args
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
            Selection::Routed(Route::Packages(packages)) => {
                let mut args = vec![s("nextest"), s("run"), s("--locked")];
                for package in packages {
                    args.push(s("-p"));
                    args.push(package.clone());
                }
                args.push(s("--no-tests"));
                args.push(s("pass"));
                args
            }
        },
    };
    materialize(root, workspace, &selection)?;

    grove.maintain(|| {
        let started = std::time::Instant::now();
        let (lane, origin) = grove.seeded_lane_origin()?;
        if let Ok(report) = worktree::reap(root, workspace, config.reap(), false) {
            for worktree in &report.reaped {
                eprintln!("grove: reaped abandoned worktree {}", worktree.path);
            }
        }
        let code = cargo(workspace, &args, &lane)?;
        // Elapsed and lane origin only. Grove has no counterfactual for what
        // plain Cargo would have cost here, so it never claims time saved.
        eprintln!(
            "grove: {} in {:.1}s ({})",
            routed(&selection),
            started.elapsed().as_secs_f64(),
            origin.label()
        );
        Ok(code)
    })
}

/// What this invocation actually built, so the routing is visible rather than
/// implied.
fn routed(selection: &Selection) -> String {
    match selection {
        Selection::Explicit(package) => format!("package {package}"),
        Selection::Routed(Route::Full) => "whole workspace".to_string(),
        Selection::Routed(Route::None) => "nothing".to_string(),
        Selection::Routed(Route::Packages(packages)) => match packages.len() {
            1 => format!("1 affected package ({})", packages[0]),
            count => format!("{count} affected packages"),
        },
    }
}

fn select(
    workspace: &Path,
    package: &Option<String>,
    base: Option<String>,
    files: Option<String>,
) -> Result<Selection> {
    if let Some(package) = package {
        return Ok(Selection::Explicit(package.clone()));
    }
    let plan = if let Some(files) = files {
        let files = files
            .split(',')
            .filter(|file| !file.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        impact::plan(workspace, &files)?
    } else {
        let changed = impact::changed_files(workspace, base.as_deref().unwrap_or("HEAD"))?;
        impact::plan(workspace, &changed)?
    };
    Ok(Selection::Routed(if plan.full {
        Route::Full
    } else if plan.packages.is_empty() {
        Route::None
    } else {
        Route::Packages(plan.packages.into_iter().collect())
    }))
}

fn materialize(root: &Path, workspace: &Path, selection: &Selection) -> Result<()> {
    match selection {
        Selection::Explicit(package) => {
            worktree::expand(root, workspace, &[format!("crate:{package}")])?;
        }
        Selection::Routed(Route::Full) => {
            worktree::full(root, workspace)?;
        }
        Selection::Routed(Route::Packages(packages)) => {
            let scopes: Vec<_> = packages
                .iter()
                .map(|package| format!("crate:{package}"))
                .collect();
            worktree::expand(root, workspace, &scopes)?;
        }
        Selection::Routed(Route::None) => {}
    }
    Ok(())
}

pub(crate) fn cache(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    action: CacheCmd,
) -> Result<i32> {
    let grove = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone());
    match action {
        CacheCmd::Warm | CacheCmd::Promote if !project::is_cargo_workspace(workspace) => {
            eprintln!(
                "grove: not a Cargo workspace; there is no build to warm or promote \
                 (the coordination surface needs no warm cache)"
            );
            Ok(1)
        }
        CacheCmd::Warm => warm(&grove),
        CacheCmd::Promote => promote(&grove),
        CacheCmd::Status { details } => {
            let status = grove.status(details);
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(0)
        }
        CacheCmd::Explain => {
            let explanation = cache::explain(root, workspace, config);
            println!("{}", serde_json::to_string_pretty(&explanation)?);
            Ok(0)
        }
        CacheCmd::Cow => {
            let probe = seed::probe_cow(root);
            let available = probe.status == seed::CowProbeStatus::Supported;
            println!("{}", serde_json::to_string_pretty(&probe)?);
            Ok(i32::from(!available))
        }
        CacheCmd::Gc => {
            println!("{}", serde_json::to_string_pretty(&grove.gc())?);
            Ok(0)
        }
    }
}

fn warm(grove: &Grove) -> Result<i32> {
    let workspace = grove.workspace();
    worktree::full(grove.root(), workspace)?;
    grove.maintain(|| {
        let lane = grove.seeded_lane()?;
        let check = cargo(workspace, &workspace_check_args(), &lane)?;
        if check != 0 {
            eprintln!("grove: workspace check failed; not warming the canonical");
            return Ok(check);
        }
        let tests = vec![
            s("nextest"),
            s("run"),
            s("--workspace"),
            s("--no-run"),
            s("--locked"),
        ];
        if cargo(workspace, &tests, &lane)? != 0 {
            eprintln!("grove: test binaries did not build; warming a check-only canonical");
        }
        grove.promote(&lane)?;
        println!("grove: canonical warmed at {}", grove.canonical().display());
        Ok(0)
    })
}

fn promote(grove: &Grove) -> Result<i32> {
    grove.maintain(|| {
        let lane = grove.seeded_lane()?;
        grove.promote(&lane)?;
        println!(
            "grove: promoted lane to canonical at {}",
            grove.canonical().display()
        );
        Ok(0)
    })
}

fn cargo(workspace: &Path, args: &[String], lane: &cache::Lane) -> Result<i32> {
    let code = run(workspace, "cargo", args, lane)?;
    if code == 0 {
        cache::succeed(lane)?;
    }
    Ok(code)
}

/// Explain what the next build would rebuild, and why. Runs the routed check in
/// the real lane with Cargo's fingerprint logging on, so the answer describes
/// this workspace's actual cache state rather than a reconstruction of it.
pub(crate) fn why_rebuilt(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    package: Option<String>,
    fresh: bool,
    json: bool,
) -> Result<i32> {
    let grove = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone());
    let workspace = grove.workspace().to_path_buf();
    let selection = select(&workspace, &package, None, None)?;
    let mut args = vec![s("check"), s("--locked")];
    match &selection {
        Selection::Explicit(package) => {
            args.push(s("-p"));
            args.push(package.clone());
        }
        Selection::Routed(Route::Full) => args.push(s("--workspace")),
        Selection::Routed(Route::None) => {
            eprintln!("grove: no affected packages; nothing would rebuild");
            return Ok(0);
        }
        Selection::Routed(Route::Packages(packages)) => {
            for package in packages {
                args.push(s("-p"));
                args.push(package.clone());
            }
        }
    }
    materialize(root, &workspace, &selection)?;
    grove.maintain(|| {
        // A throwaway tagged lane, discarded afterwards, so every --fresh run
        // measures a genuinely cold seed rather than the previous run's warmth.
        let (lane, origin) = if fresh {
            if !grove.published() {
                eprintln!(
                    "grove: no canonical is published, so there is nothing to seed from; \
                     run `grove cache warm` first"
                );
                return Ok(1);
            }
            grove.seeded_tagged_lane_origin(SEED_CHECK_TAG)?
        } else {
            grove.seeded_lane_origin()?
        };
        cache::prepare(&lane)?;
        let mut command = Command::new("cargo");
        command.args(&args).current_dir(&workspace);
        // JSON artifacts on stdout are the authoritative reused/rebuilt count;
        // the fingerprint log on stderr only explains units that had a stale
        // fingerprint to begin with.
        command.arg("--message-format=json");
        cache::apply_env(&mut command, &lane);
        command.env(fingerprint::LOG_ENV.0, fingerprint::LOG_ENV.1);
        let output = command.output().context("running cargo")?;
        let counts = fingerprint::freshness(&String::from_utf8_lossy(&output.stdout));
        let units = fingerprint::parse(&String::from_utf8_lossy(&output.stderr));
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "lane_origin": origin.label(),
                    "reused": counts.reused,
                    "rebuilt": counts.rebuilt,
                    "explained": units,
                }))?
            );
        } else {
            println!(
                "grove: {} of {} unit(s) rebuilt, {} reused ({})",
                counts.rebuilt,
                counts.total(),
                counts.reused,
                origin.label()
            );
            for unit in &units {
                println!("  {} [{}]: {}", unit.package, unit.kind, unit.explanation);
            }
            // A rebuild Cargo never called dirty had nothing cached to compare
            // against, which is the shape of a lane that seeded badly.
            if units.is_empty() && counts.rebuilt > 0 {
                println!(
                    "  no unit was stale: this lane had nothing cached to reuse, \
                     so the canonical did not seed it"
                );
            }
        }
        let code = if output.status.success() { 0 } else { 1 };
        if fresh {
            // Discard so the next --fresh run is cold again; a retained lane
            // would report a warm reuse and hide exactly the regression this
            // mode exists to catch.
            cache::discard(lane);
        }
        Ok(code)
    })
}

/// Tag for the throwaway lane `--fresh` seeds and then discards.
const SEED_CHECK_TAG: &str = "seed-check";

fn run(workspace: &Path, program: &str, args: &[String], lane: &cache::Lane) -> Result<i32> {
    cache::prepare(lane)?;
    let mut command = Command::new(program);
    command.args(args).current_dir(workspace);
    cache::apply_env(&mut command, lane);
    let status = command
        .status()
        .with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn exec(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    tag: &str,
    command: Vec<String>,
) -> Result<i32> {
    let grove = Grove::command(
        root.to_path_buf(),
        workspace.to_path_buf(),
        config.clone(),
        &command,
    );
    worktree::full(root, grove.workspace())?;
    grove.maintain(|| {
        let canonical_ready = grove.published();
        let lane = if canonical_ready {
            grove.seeded_tagged_lane(tag)?
        } else {
            let lane = grove.bootstrap_lane()?;
            eprintln!(
                "grove: verified canonical unavailable; using serialized unverified bootstrap lane {}",
                lane.dir.display()
            );
            lane
        };
        let (program, args) = command.split_first().context("exec requires a command")?;
        let code = run(grove.workspace(), program, args, &lane)?;
        if !canonical_ready {
            worktree::touch(root, grove.workspace())?;
        }
        // A seeded tagged lane is disposable; the bootstrap lane is the workspace's
        // only warm state (seeding may have refused a policy-mismatched canonical
        // and fallen back to it), so its build must survive this command.
        if canonical_ready && !tag.is_empty() && !cache::is_bootstrap(&lane) {
            cache::discard(lane);
        }
        Ok(code)
    })
}

fn workspace_check_args() -> Vec<String> {
    // Not `--all-targets`: bench targets may be nightly-only (`#![feature(test)]`,
    // E0554 on stable), and static `--lib`/`--bins` filters hard-error on packages
    // without that target kind. Plain check produces exactly the artifacts seeded
    // `cargo check` runs reuse; the warm nextest step covers test targets.
    ["check", "--workspace", "--locked"]
        .iter()
        .map(|value| value.to_string())
        .collect()
}

fn s(value: &str) -> String {
    value.to_string()
}
