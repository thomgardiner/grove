//! Durable receipt storage and execution for verification profiles.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::portable::PortableInputs;
use crate::{cache, git, impact, snapshot, task};
const TAIL_BYTES: usize = 8 * 1024;
pub(super) use grove_core::verification::{Checkout, Evidence, LaneIdentity, Receipt, Run};

pub(super) struct ReceiptContext<'a> {
    pub(super) root: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) repo: &'a str,
    pub(super) task: Option<&'a task::Task>,
    pub(super) profile: &'a str,
    pub(super) run_id: &'a str,
    pub(super) profile_sha256: &'a str,
    pub(super) input_digests: &'a BTreeMap<String, String>,
    pub(super) command_index: usize,
    pub(super) required: bool,
    pub(super) lane_tag: &'a str,
    pub(super) lane: &'a cache::Lane,
    pub(super) portable: Option<&'a PortableInputs>,
    pub(super) portable_env: Option<&'a [String]>,
}

/// Hash every declared profile input. Missing paths refuse verification.
/// Map keys are repo-relative paths with `/` separators.
pub(super) fn input_digests(
    workspace: &Path,
    patterns: &[String],
) -> Result<BTreeMap<String, String>> {
    let workspace = fs::canonicalize(workspace).context("canonicalizing workspace")?;
    let mut digests = BTreeMap::new();
    for pattern in patterns {
        for (rel, path) in expand_input(&workspace, pattern)? {
            let bytes = fs::read(&path).with_context(|| format!("reading input {rel}"))?;
            let mut hash = Sha256::new();
            hash.update(b"grove.verification-input.v1\0");
            hash.update(rel.as_bytes());
            hash.update([0]);
            hash.update(&bytes);
            digests.insert(rel, crate::hex(&hash.finalize()));
        }
    }
    Ok(digests)
}

fn expand_input(workspace: &Path, pattern: &str) -> Result<Vec<(String, PathBuf)>> {
    let pattern = pattern.trim_start_matches("./");
    if pattern.is_empty() || pattern.contains('\0') {
        bail!("invalid verification input path");
    }
    if Path::new(pattern).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("verification input {pattern:?} is not a relative file path");
    }
    if let Some(dir) = pattern.strip_suffix("/**") {
        if dir.is_empty() {
            bail!("verification input \"/**\" is not allowed; declare a subdirectory");
        }
        let root = workspace.join(dir);
        let meta = fs::symlink_metadata(&root)
            .with_context(|| format!("verification input directory {dir:?} is missing"))?;
        if meta.file_type().is_symlink() {
            bail!("verification input directory {dir:?} must not be a symlink");
        }
        if !meta.is_dir() {
            bail!("verification input directory {dir:?} is missing");
        }
        let root_canon = fs::canonicalize(&root)
            .with_context(|| format!("canonicalizing input directory {dir:?}"))?;
        if !root_canon.starts_with(workspace) {
            bail!("verification input directory {dir:?} escapes the workspace");
        }
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.with_context(|| format!("walking input directory {dir}"))?;
            let ft = entry.file_type();
            if ft.is_symlink() {
                bail!(
                    "verification input refuses symlink {}",
                    entry.path().display()
                );
            }
            if !ft.is_file() {
                continue;
            }
            let path = fs::canonicalize(entry.path())
                .with_context(|| format!("canonicalizing input file {}", entry.path().display()))?;
            if !path.starts_with(workspace) {
                bail!(
                    "verification input file escapes the workspace: {}",
                    path.display()
                );
            }
            let rel = path
                .strip_prefix(workspace)
                .with_context(|| format!("input escaped workspace: {}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            files.push((rel, path));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if files.is_empty() {
            bail!("verification input directory {dir:?} contains no files");
        }
        return Ok(files);
    }
    let path = workspace.join(pattern);
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("verification input {pattern:?} is missing"))?;
    if !canonical.starts_with(workspace) {
        bail!("verification input {pattern:?} escapes the workspace");
    }
    if !canonical.is_file() {
        bail!("verification input {pattern:?} is not a regular file");
    }
    Ok(vec![(pattern.replace('\\', "/"), canonical)])
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(super) fn checkout(workspace: &Path) -> Checkout {
    Checkout {
        head: git::capture(workspace, &["rev-parse", "HEAD"]).ok(),
        changed_paths: impact::changed_files(workspace, "HEAD").unwrap_or_default(),
        branch: git::capture(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok(),
        workspace: cache::canonical_path(workspace)
            .to_string_lossy()
            .into_owned(),
    }
}

fn bounded_tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]).into_owned()
}

fn test_count(output: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(output);
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        for window in words.windows(3) {
            if window[1] == "tests"
                && window[2].trim_end_matches(':') == "run"
                && let Ok(count) = window[0].parse()
            {
                return Some(count);
            }
        }
        for window in words.windows(3) {
            if window[0] == "running"
                && window[2].starts_with("test")
                && let Ok(count) = window[1].parse()
            {
                return Some(count);
            }
        }
    }
    None
}

fn is_test_runner(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "nextest" || arg == "test")
}

fn explicitly_selected_tests(argv: &[String]) -> bool {
    argv.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-p" | "--package"
                | "--test"
                | "--lib"
                | "--bin"
                | "--example"
                | "--bench"
                | "-E"
                | "--filter-expr"
                | "--exact"
        )
    })
}

/// Keep structured report output parseable: profile command streams are diagnostics,
/// while stdout is reserved for the final JSON receipt report.
fn emit(output: &[u8]) {
    let _ = std::io::stderr().write_all(output);
}

pub(super) fn execute(
    context: &ReceiptContext<'_>,
    argv: &[String],
    allow_zero_tests: bool,
) -> Result<Receipt> {
    let started_at = now_secs();
    let started = Instant::now();
    let state = checkout(context.workspace);
    let input = snapshot::capture(context.workspace)?;
    let input = snapshot::persist(context.root, context.repo, &input)?;
    let (program, args) = argv
        .split_first()
        .context("verification command has no argv")?;
    let mut command = Command::new(program);
    if let Some(names) = context.portable_env {
        command.args(super::portable::command_args(argv, context.lane));
        super::portable::configure_command(&mut command, names, context.lane);
    } else {
        command.args(args);
        cache::apply_env(&mut command, context.lane);
    }
    command.current_dir(context.workspace);
    let output = command.output();
    let (exit_code, interrupted, stdout, stderr) = match output {
        Ok(output) => (
            output.status.code(),
            output.status.code().is_none(),
            output.stdout,
            output.stderr,
        ),
        Err(error) => (None, true, Vec::new(), error.to_string().into_bytes()),
    };
    emit(&stdout);
    emit(&stderr);
    let mut combined = stdout.clone();
    combined.extend_from_slice(&stderr);
    let count = test_count(&combined);
    let zero_selected = is_test_runner(argv)
        && explicitly_selected_tests(argv)
        && count == Some(0)
        && !allow_zero_tests;
    if zero_selected {
        eprintln!("grove: selected test command ran zero tests; refusing a successful receipt");
    }
    let output = snapshot::capture(context.workspace)?;
    let output = snapshot::persist(context.root, context.repo, &output)?;
    let unchanged = input == output;
    if !unchanged {
        eprintln!(
            "grove: verification command changed workspace content; refusing a successful receipt"
        );
    }
    let passed = exit_code == Some(0) && !interrupted && !zero_selected && unchanged;
    let receipt = Receipt {
        schema_version: grove_core::verification::RECEIPT_SCHEMA_VERSION,
        repository: context.repo.to_string(),
        task_id: context.task.map(|task| task.id.clone()),
        agent: context.task.map(|task| task.agent.clone()),
        task: context.task.map(|task| task.description.clone()),
        profile: context.profile.to_string(),
        run_id: context.run_id.to_string(),
        profile_sha256: context.profile_sha256.to_string(),
        input_digests: context.input_digests.clone(),
        command_index: context.command_index,
        required: context.required,
        evidence: Some(Evidence {
            checkout: state,
            input,
            output,
            portable: context.portable.map(serde_json::to_value).transpose()?,
        }),
        lane: LaneIdentity {
            tag: context.lane_tag.to_string(),
            path: context.lane.dir.to_string_lossy().into_owned(),
            policy_sha256: context.lane.policy_sha256.clone(),
        },
        argv: argv.to_vec(),
        started_at,
        ended_at: now_secs(),
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        exit_code,
        interrupted,
        test_count: count,
        passed,
        stdout_tail: bounded_tail(&stdout),
        stderr_tail: bounded_tail(&stderr),
    };
    grove_core::verification::write_receipt(context.root, context.repo, &receipt)?;
    Ok(receipt)
}

pub(super) fn receipts(root: &Path, repo: &str) -> Result<Vec<Receipt>> {
    grove_core::verification::receipts(root, repo)
}

pub(super) fn complete_run(root: &Path, repo: &str, run: &Run) -> Result<()> {
    grove_core::verification::complete_run(root, repo, run)
}

pub(super) fn runs(root: &Path, repo: &str) -> Result<Vec<Run>> {
    grove_core::verification::runs(root, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nextest_and_cargo_test_counts() {
        assert_eq!(test_count(b"Summary: 12 tests run: 12 passed"), Some(12));
        assert_eq!(test_count(b"running 3 tests"), Some(3));
    }

    #[test]
    fn recognizes_explicit_test_selection() {
        assert!(explicitly_selected_tests(&[
            "cargo".into(),
            "test".into(),
            "--test".into(),
            "x".into()
        ]));
        assert!(!explicitly_selected_tests(&[
            "cargo".into(),
            "test".into(),
            "--workspace".into()
        ]));
    }

    #[test]
    fn input_digests_hash_files_and_refuse_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("script.sh"), b"true\n").unwrap();
        fs::create_dir_all(dir.path().join("suite")).unwrap();
        fs::write(dir.path().join("suite/a.txt"), b"a").unwrap();
        fs::write(dir.path().join("suite/b.txt"), b"b").unwrap();

        let digests = input_digests(dir.path(), &["script.sh".into(), "suite/**".into()]).unwrap();
        assert_eq!(digests.len(), 3);
        assert!(digests.contains_key("script.sh"));
        assert!(digests.contains_key("suite/a.txt"));
        assert!(digests.contains_key("suite/b.txt"));

        let before = digests["script.sh"].clone();
        fs::write(dir.path().join("script.sh"), b"false\n").unwrap();
        let after = input_digests(dir.path(), &["script.sh".into()]).unwrap();
        assert_ne!(before, after["script.sh"]);

        let err = input_digests(dir.path(), &["missing.sh".into()]).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err:#}");
        let escape = input_digests(dir.path(), &["../outside".into()]).unwrap_err();
        assert!(
            escape.to_string().contains("..") || escape.to_string().contains("missing"),
            "{escape:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn input_digests_refuse_symlinked_dirs_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("suite")).unwrap();
        let err = input_digests(dir.path(), &["suite/**".into()]).unwrap_err();
        assert!(
            err.to_string().contains("symlink") || err.to_string().contains("escapes"),
            "{err:#}"
        );

        fs::create_dir_all(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            dir.path().join("real/link.txt"),
        )
        .unwrap();
        let err = input_digests(dir.path(), &["real/**".into()]).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err:#}");
    }
}
