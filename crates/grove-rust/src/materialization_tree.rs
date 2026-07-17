use crate::claim::claim_scope::normalize_scope;
use crate::git;
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Metrics {
    pub(super) tracked_files: u64,
    pub(super) git_blob_bytes: u64,
    pub(super) working_files: u64,
    pub(super) working_logical_bytes: u64,
}

#[derive(Debug, Clone)]
pub(super) struct Entry {
    path: String,
    git_bytes: u64,
    working_bytes: Option<u64>,
}

impl Entry {
    pub(super) fn blob(path: &str, bytes: u64) -> Self {
        Self {
            path: path.into(),
            git_bytes: bytes,
            working_bytes: Some(bytes),
        }
    }

    #[cfg(test)]
    pub(super) fn measured(path: &str, git_bytes: u64, working_bytes: u64) -> Self {
        Self {
            path: path.into(),
            git_bytes,
            working_bytes: Some(working_bytes),
        }
    }
}

pub(super) struct Tree {
    root: PathBuf,
    entries: Vec<Entry>,
    pub(super) dirs: BTreeSet<String>,
}

impl Tree {
    pub(super) fn load(workspace: &Path, base_oid: &str) -> Result<Self> {
        let root = PathBuf::from(git::capture(workspace, &["rev-parse", "--show-toplevel"])?);
        let root = fs::canonicalize(root).context("canonicalizing Git repository root")?;
        let output = Command::new("git")
            .args(["ls-tree", "-r", "-z", "--long", "--full-tree", base_oid])
            .current_dir(&root)
            .output()
            .context("spawning git ls-tree for materialization planning")?;
        if !output.status.success() {
            bail!(
                "git ls-tree failed while planning materialization: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        let output = String::from_utf8(output.stdout).context("Git tree paths are not UTF-8")?;
        let sparse = sparse_absent(&root)?;
        let entries = output
            .split_terminator('\0')
            .map(|record| parse_entry(record, &root, &sparse))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::with_root(root, entries))
    }

    pub(super) fn new(entries: Vec<Entry>) -> Self {
        Self::with_root(PathBuf::new(), entries)
    }

    fn with_root(root: PathBuf, entries: Vec<Entry>) -> Self {
        let mut dirs = BTreeSet::from([".".into()]);
        for entry in &entries {
            let mut parent = parent(&entry.path);
            while parent != "." {
                dirs.insert(parent.clone());
                parent = parent
                    .rsplit_once('/')
                    .map_or(".".into(), |(dir, _)| dir.into());
            }
        }
        Self {
            root,
            entries,
            dirs,
        }
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn relative(&self, path: &Path) -> Result<Option<String>> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("canonicalizing materialization input {}", path.display()))?;
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return Ok(None);
        };
        Ok(Some(normalize_scope(&relative.to_string_lossy())?))
    }

    pub(super) fn contains(&self, path: &str) -> bool {
        self.entries.iter().any(|entry| entry.path == path)
    }

    pub(super) fn cone(&self, scope: &str) -> Result<String> {
        let scope = normalize_scope(scope)?;
        if self.dirs.contains(&scope) {
            return Ok(scope);
        }
        if self.entries.iter().any(|entry| entry.path == scope) {
            return Ok(parent(&scope));
        }
        let mut candidate = scope;
        loop {
            candidate = candidate
                .rsplit_once('/')
                .map_or(".".into(), |(dir, _)| dir.into());
            if self.dirs.contains(&candidate) {
                return Ok(candidate);
            }
        }
    }

    pub(super) fn metrics(&self, cones: &[String]) -> Result<Metrics> {
        let entries = self
            .entries
            .iter()
            .filter(|entry| selected(&entry.path, cones));
        metrics(entries)
    }

    pub(super) fn full(&self) -> Result<Metrics> {
        metrics(self.entries.iter())
    }
}

fn parse_entry(record: &str, root: &Path, sparse: &BTreeSet<String>) -> Result<Entry> {
    let (metadata, path) = record
        .split_once('\t')
        .context("malformed git ls-tree record")?;
    let mut fields = metadata.split_whitespace();
    let _mode = fields.next().context("git tree entry has no mode")?;
    let kind = fields.next().context("git tree entry has no kind")?;
    let _oid = fields.next().context("git tree entry has no object ID")?;
    let size = fields.next().context("git tree entry has no size")?;
    let normalized = normalize_scope(path)?;
    if normalized != path || normalized == "." {
        bail!("Git tree path is not canonical: {path:?}")
    }
    let (git_bytes, working_bytes) = match kind {
        "blob" => {
            let git_bytes = size.parse().context("invalid Git blob size")?;
            let working_bytes = match fs::symlink_metadata(root.join(path)) {
                Ok(working) => Some(working.len()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && sparse.contains(path) =>
                {
                    Some(git_bytes)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading working-file metadata for {path:?}"));
                }
            };
            (git_bytes, working_bytes)
        }
        "commit" if size == "-" => (0, None),
        _ => bail!("unsupported Git tree entry kind {kind:?}"),
    };
    Ok(Entry {
        path: path.into(),
        git_bytes,
        working_bytes,
    })
}

fn sparse_absent(root: &Path) -> Result<BTreeSet<String>> {
    let output = Command::new("git")
        .args(["-c", "core.quotePath=false", "ls-files", "-v", "-z"])
        .current_dir(root)
        .output()
        .context("spawning git ls-files for sparse materialization entries")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed while reading sparse materialization entries: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let output = String::from_utf8(output.stdout).context("Git index paths are not UTF-8")?;
    Ok(output
        .split_terminator('\0')
        .filter_map(|entry| entry.strip_prefix("S "))
        .map(str::to_string)
        .collect())
}

fn metrics<'a>(entries: impl Iterator<Item = &'a Entry>) -> Result<Metrics> {
    let mut result = Metrics {
        tracked_files: 0,
        git_blob_bytes: 0,
        working_files: 0,
        working_logical_bytes: 0,
    };
    for entry in entries {
        result.tracked_files = result
            .tracked_files
            .checked_add(1)
            .context("tracked file count overflow")?;
        result.git_blob_bytes = result
            .git_blob_bytes
            .checked_add(entry.git_bytes)
            .context("Git blob byte count overflow")?;
        if let Some(bytes) = entry.working_bytes {
            result.working_files = result
                .working_files
                .checked_add(1)
                .context("working file count overflow")?;
            result.working_logical_bytes = result
                .working_logical_bytes
                .checked_add(bytes)
                .context("working logical byte count overflow")?;
        }
    }
    Ok(result)
}

pub(super) fn parent(path: &str) -> String {
    path.rsplit_once('/')
        .map_or(".".into(), |(dir, _)| dir.into())
}

pub(super) fn covers(cone: &str, path: &str) -> bool {
    cone == "."
        || path == cone
        || path
            .strip_prefix(cone)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn selected(path: &str, cones: &[String]) -> bool {
    !path.contains('/')
        || cones
            .iter()
            .any(|cone| covers(cone, path) || covers(&parent(path), cone))
}
