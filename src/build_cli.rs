use crate::cli::CacheCmd;
use anyhow::{Context, Result};
use grove::api::Grove;
use grove::{cache, config, impact, worktree};
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
        let lane = grove.seeded_lane()?;
        if let Ok(report) = worktree::reap(root, workspace, config.reap(), false) {
            for worktree in &report.reaped {
                eprintln!("grove: reaped abandoned worktree {}", worktree.path);
            }
        }
        cargo(workspace, &args, &lane)
    })
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
        CacheCmd::Warm => warm(&grove),
        CacheCmd::Promote => promote(&grove),
        CacheCmd::Status { details } => {
            let status = grove.status(details);
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(0)
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
        let lane = grove.lane()?;
        grove.promote(&lane)?;
        println!(
            "grove: promoted lane to canonical at {}",
            grove.canonical().display()
        );
        Ok(0)
    })
}

fn cargo(workspace: &Path, args: &[String], lane: &cache::Lane) -> Result<i32> {
    let mut command = Command::new("cargo");
    command.args(args).current_dir(workspace);
    cache::apply_env(&mut command, lane);
    let status = command.status().context("running cargo")?;
    Ok(status.code().unwrap_or(1))
}

fn run(workspace: &Path, program: &str, args: &[String], lane: &cache::Lane) -> Result<i32> {
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
        let lane = grove.seeded_tagged_lane(tag)?;
        let (program, args) = command.split_first().context("exec requires a command")?;
        let code = run(grove.workspace(), program, args, &lane)?;
        if !tag.is_empty() {
            cache::discard(lane);
        }
        Ok(code)
    })
}

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
    .map(|value| value.to_string())
    .collect()
}

fn s(value: &str) -> String {
    value.to_string()
}
