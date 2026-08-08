//! Impact-routed cargo through a Grove lane.

use crate::cache::{CacheHost, Lane};
use crate::impact::{Plan, plan};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub enum CargoVerb {
    Check,
    Build,
    Clippy,
    Test,
}

impl CargoVerb {
    fn label(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
            Self::Clippy => "clippy",
            Self::Test => "test",
        }
    }
}

fn apply_incremental(cmd: &mut Command) {
    if std::env::var_os("CARGO_INCREMENTAL").is_some() {
        return;
    }
    if std::env::var_os("RUSTC_WRAPPER").is_some()
        || std::env::var_os("RUSTC_WORKSPACE_WRAPPER").is_some()
    {
        return;
    }
    cmd.env("CARGO_INCREMENTAL", "1");
}

fn spawn_with_lane(
    workspace: &Path,
    lane: &Lane,
    program: &OsStr,
    args: &[impl AsRef<OsStr>],
) -> Result<i32> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(workspace);
    apply_incremental(&mut cmd);
    lane.apply(&mut cmd);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", program.to_string_lossy()))?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn run_cargo(workspace: &Path, lane: &Lane, args: &[&str]) -> Result<i32> {
    spawn_with_lane(workspace, lane, "cargo".as_ref(), args)
}

fn extend_packages(args: &mut Vec<String>, plan: &Plan) {
    if plan.full {
        args.push("--workspace".into());
        return;
    }
    for package in &plan.packages {
        args.push("-p".into());
        args.push(package.clone());
    }
}

fn nextest_available() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("cargo")
            .args(["nextest", "--version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn verb_argv(verb: CargoVerb) -> Vec<String> {
    match verb {
        CargoVerb::Check => vec!["check".into()],
        CargoVerb::Build => vec!["build".into()],
        CargoVerb::Clippy => vec!["clippy".into()],
        CargoVerb::Test => {
            if nextest_available() {
                vec![
                    "nextest".into(),
                    "run".into(),
                    "--no-tests".into(),
                    "pass".into(),
                ]
            } else {
                vec!["test".into()]
            }
        }
    }
}

/// Impact-routed cargo. `-p` wins over the plan.
/// Prove (`base=auto`) adds `--locked` when `Cargo.lock` exists; edit mode does not.
pub fn cargo_impact(
    workspace: &Path,
    verb: CargoVerb,
    base: &str,
    package: Option<&str>,
) -> Result<i32> {
    let start = Instant::now();
    let mut args = verb_argv(verb);

    if let Some(pkg) = package {
        args.push("-p".into());
        args.push(pkg.into());
        eprintln!("grove: {} -p {pkg}", verb.label());
    } else {
        let plan = plan(workspace, base)?;
        if plan.packages.is_empty() && !plan.full {
            eprintln!(
                "grove: nothing to {} (base {}; use --base auto to verify tree)",
                verb.label(),
                plan.base
            );
            if std::env::var_os("GROVE_VERBOSE").is_some() {
                eprintln!("grove: {:.3}s", start.elapsed().as_secs_f64());
            }
            return Ok(0);
        }
        if plan.full {
            eprintln!("grove: {} --workspace (base {})", verb.label(), plan.base);
        } else {
            eprintln!(
                "grove: {} {} (base {})",
                verb.label(),
                plan.packages.join(" "),
                plan.base
            );
        }
        extend_packages(&mut args, &plan);
    }

    // Prove only: edit loops must be free to bump Cargo.lock.
    if base == "auto" && workspace.join("Cargo.lock").is_file() {
        args.push("--locked".into());
    }

    let host = CacheHost::open(workspace)?;
    let lane = host.lane(workspace)?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_cargo(workspace, &lane, &refs)?;
    eprintln!(
        "grove: {} {:.3}s  target={}",
        verb.label(),
        start.elapsed().as_secs_f64(),
        lane.target_dir.display()
    );
    if code == 0 {
        host.hygiene_log(workspace);
    }
    Ok(code)
}

pub fn cargo_check(workspace: &Path, base: &str, package: Option<&str>) -> Result<i32> {
    cargo_impact(workspace, CargoVerb::Check, base, package)
}

pub fn cargo_build(workspace: &Path, base: &str, package: Option<&str>) -> Result<i32> {
    cargo_impact(workspace, CargoVerb::Build, base, package)
}

pub fn cargo_clippy(workspace: &Path, base: &str, package: Option<&str>) -> Result<i32> {
    cargo_impact(workspace, CargoVerb::Clippy, base, package)
}

pub fn cargo_test(workspace: &Path, base: &str, package: Option<&str>) -> Result<i32> {
    cargo_impact(workspace, CargoVerb::Test, base, package)
}

pub fn exec(workspace: &Path, argv: &[String]) -> Result<i32> {
    if argv.is_empty() {
        bail!("grove exec needs a command after --");
    }
    let host = CacheHost::open(workspace)?;
    let lane = host.lane(workspace)?;
    let start = Instant::now();
    let code = spawn_with_lane(workspace, &lane, argv[0].as_ref(), &argv[1..])?;
    eprintln!(
        "grove: exec {:.3}s  target={}",
        start.elapsed().as_secs_f64(),
        lane.target_dir.display()
    );
    if code == 0 {
        host.hygiene_log(workspace);
    }
    Ok(code)
}

pub fn warm(workspace: &Path) -> Result<i32> {
    CacheHost::open(workspace)?.warm(workspace)
}

/// Bash/zsh exports so bare cargo / rust-analyzer use this lane's target dir.
pub fn env_exports(workspace: &Path) -> Result<String> {
    let host = CacheHost::open(workspace)?;
    let lane = host.lane(workspace)?;
    let mut exports = String::new();
    exports.push_str(&format!(
        "export CARGO_TARGET_DIR={}\n",
        shell_quote(&lane.target_dir.display().to_string())
    ));
    exports.push_str("unset CARGO_BUILD_BUILD_DIR\n");
    if std::env::var_os("RUSTC_WRAPPER").is_none()
        && std::env::var_os("RUSTC_WORKSPACE_WRAPPER").is_none()
    {
        exports.push_str("export CARGO_INCREMENTAL=1\n");
    }
    Ok(exports)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
