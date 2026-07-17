use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path};

use super::Lease;
#[path = "worktree_salvage_archive.rs"]
mod archive;
#[path = "worktree_salvage_git.rs"]
mod git;
use archive::{Archive, prepare, salvage_ref};
use git::{
    Objects, Sparse, add_worktree, add_worktree_isolated, bytes, check_index_lock, index_path,
    install_index, optional_text, recover_index_lock, run, sparse, status, temp_index,
    temp_objects, text, text_isolated,
};

struct State {
    head: String,
    head_tree: String,
    index: Vec<u8>,
    index_tree: String,
    status: Vec<u8>,
    sparse: Sparse,
    tree: String,
}

pub(super) fn salvage_work(worktree: &Path, lease: &Lease) -> Result<Option<String>> {
    let current_head = text(worktree, &["rev-parse", "--verify", "HEAD"], None)?;
    let located = archive::locate(worktree, lease, &current_head)?;
    recover_index_lock(worktree, located.is_some())?;
    let initial_status = status(worktree)?;
    validate(worktree, &initial_status)?;
    let current = capture(worktree)?;
    validate(worktree, &current.status)?;
    if let Some(located) = located
        && (located.exact || clean(&current) || pending_after_move(&current, &located.archive))
    {
        finish(worktree, lease, &located.archive)?;
        return Ok(Some(located.archive.reference));
    }
    if clean(&current) {
        return Ok(None);
    }
    validate(worktree, &current.status)?;
    let reference = salvage_ref(lease, &current_head);
    let prepared = prepare(worktree, reference)?;
    prove_unchanged(worktree, &prepared.archive.original)?;
    archive::create_ref(
        worktree,
        &prepared.archive.reference,
        &prepared.state_commit,
    )?;
    finish(worktree, lease, &prepared.archive)?;
    Ok(Some(prepared.archive.reference))
}

pub(super) fn preflight(worktree: &Path, lease: &Lease) -> Result<()> {
    let current_head = text(worktree, &["rev-parse", "--verify", "HEAD"], None)?;
    let has_archive = archive::recovery_exists(worktree, lease, &current_head)?;
    check_index_lock(worktree, has_archive)?;
    let status = status(worktree)?;
    validate(worktree, &status)?;
    if has_archive {
        let objects = temp_objects(worktree)?;
        if let Some(exact) = archive::exact_isolated(worktree, lease, &current_head, &objects)? {
            let current = capture_with(worktree, Some(&objects))?;
            validate(worktree, &current.status)?;
            if !same_state(&current, &exact.original) {
                bail!("worktree changed after salvage was prepared; left in place")
            }
        }
    }
    Ok(())
}

fn clean(state: &State) -> bool {
    state.index_tree == state.head_tree && state.tree == state.head_tree
}

fn pending_after_move(current: &State, archive: &Archive) -> bool {
    current.head == archive.snapshot
        && current.index == archive.original.index
        && current.index_tree == archive.original.index_tree
        && current.sparse == archive.original.sparse
        && current.tree == archive.original.tree
}

fn finish(worktree: &Path, lease: &Lease, archive: &Archive) -> Result<()> {
    let branch_ref = format!("refs/heads/{}", lease.branch);
    let current = text(worktree, &["rev-parse", "--verify", &branch_ref], None)?;
    if current == archive.snapshot {
        let state = capture(worktree)?;
        validate(worktree, &state.status)?;
        if clean(&state) {
            return Ok(());
        }
    }
    if current == archive.original.head {
        prove_unchanged(worktree, &archive.original)?;
        run(
            worktree,
            &[
                "update-ref",
                &branch_ref,
                &archive.snapshot,
                &archive.original.head,
            ],
            None,
            None,
        )
        .context("advancing the leased branch to its preserved worktree snapshot")?;
    } else if current != archive.snapshot {
        bail!("leased branch changed after salvage was prepared; preserved ref left intact")
    }
    prove_worktree(worktree, archive)?;
    install_index(worktree, &archive.original.index, &archive.clean_index)?;
    let final_state = capture(worktree)?;
    validate(worktree, &final_state.status)?;
    if !clean(&final_state) {
        bail!("worktree changed while salvage was completing; preserved ref left intact")
    }
    Ok(())
}

fn prove_worktree(worktree: &Path, archive: &Archive) -> Result<()> {
    let current = capture(worktree)?;
    if current.tree != archive.original.tree {
        bail!("worktree content changed after salvage was prepared; preserved ref left intact")
    }
    if current.head == archive.snapshot && current.index != archive.original.index {
        bail!("index changed after salvage was prepared; preserved ref left intact")
    }
    if current.sparse != archive.original.sparse {
        bail!("sparse-checkout state changed after salvage was prepared; preserved ref left intact")
    }
    Ok(())
}

fn prove_unchanged(worktree: &Path, expected: &State) -> Result<()> {
    let actual = capture(worktree)?;
    if !same_state(&actual, expected) {
        bail!("worktree changed while salvage was being prepared; left in place")
    }
    Ok(())
}

fn same_state(actual: &State, expected: &State) -> bool {
    actual.head == expected.head
        && actual.head_tree == expected.head_tree
        && actual.index == expected.index
        && actual.index_tree == expected.index_tree
        && actual.status == expected.status
        && actual.sparse == expected.sparse
        && actual.tree == expected.tree
}

fn capture(worktree: &Path) -> Result<State> {
    capture_with(worktree, None)
}

fn capture_with(worktree: &Path, objects: Option<&Objects>) -> Result<State> {
    let head = text(worktree, &["rev-parse", "--verify", "HEAD"], None)?;
    let head_tree = text(worktree, &["rev-parse", "--verify", "HEAD^{tree}"], None)?;
    let index_path = index_path(worktree)?;
    let index = fs::read(&index_path)
        .with_context(|| format!("reading live Git index {}", index_path.display()))?;
    let status = status(worktree)?;
    let sparse = sparse(worktree)?;
    let temp = temp_index(&index_path, &index)?;
    let index_tree = match objects {
        Some(objects) => text_isolated(worktree, &["write-tree"], Some(temp.path()), objects)?,
        None => text(worktree, &["write-tree"], Some(temp.path()))?,
    };
    match objects {
        Some(objects) => add_worktree_isolated(worktree, temp.path(), objects)?,
        None => add_worktree(worktree, temp.path())?,
    }
    let tree = match objects {
        Some(objects) => text_isolated(worktree, &["write-tree"], Some(temp.path()), objects)?,
        None => text(worktree, &["write-tree"], Some(temp.path()))?,
    };
    Ok(State {
        head,
        head_tree,
        index,
        index_tree,
        status,
        sparse,
        tree,
    })
}

fn validate(worktree: &Path, status: &[u8]) -> Result<()> {
    if status
        .split(|byte| *byte == 0)
        .any(|record| record.starts_with(b"u "))
    {
        bail!("unresolved index entries cannot be cleaned automatically; worktree left intact")
    }
    if has_intent_to_add(worktree)? {
        bail!("intent-to-add entries cannot be cleaned automatically; worktree left intact")
    }
    validate_gitlinks(worktree)?;
    if optional_text(worktree, &["config", "--bool", "--get", "index.sparse"])?.as_deref()
        == Some("true")
    {
        bail!("sparse indexes cannot be salvaged automatically; worktree left intact")
    }
    refuse_metadata_collision(worktree)?;
    refuse_ignored(worktree)?;
    validate_untracked(worktree)
}

fn validate_gitlinks(worktree: &Path) -> Result<()> {
    let listed = bytes(worktree, &["ls-files", "--stage", "-z"], None)?;
    for record in listed
        .split(|byte| *byte == 0)
        .filter(|record| record.starts_with(b"160000 "))
    {
        let split = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed Git index gitlink")?;
        let path = std::str::from_utf8(&record[split + 1..])
            .context("salvage requires UTF-8 repository paths")?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            bail!("Git returned unsafe submodule path {path:?}")
        }
        let full = worktree.join(relative);
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading submodule {path:?}")),
        };
        if metadata.is_dir()
            && fs::read_dir(&full)
                .with_context(|| format!("reading submodule {path:?}"))?
                .next()
                .transpose()?
                .is_none()
        {
            continue;
        }
        bail!("submodule state cannot be salvaged automatically; worktree left intact")
    }
    Ok(())
}

fn has_intent_to_add(worktree: &Path) -> Result<bool> {
    let debug = bytes(worktree, &["ls-files", "--debug", "-z"], None)?;
    for line in debug.split(|byte| *byte == b'\n') {
        let Some(start) = line
            .windows(b"flags: ".len())
            .position(|window| window == b"flags: ")
        else {
            continue;
        };
        let flags = std::str::from_utf8(&line[start + b"flags: ".len()..])
            .ok()
            .and_then(|value| u32::from_str_radix(value.trim(), 16).ok());
        if flags.is_some_and(|value| value & 0x2000_0000 != 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn refuse_metadata_collision(worktree: &Path) -> Result<()> {
    let listed = bytes(
        worktree,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            archive::META,
        ],
        None,
    )?;
    if !listed.is_empty() || fs::symlink_metadata(worktree.join(archive::META)).is_ok() {
        bail!(
            "{} is reserved for salvage evidence; worktree left intact",
            archive::META
        )
    }
    Ok(())
}

fn refuse_ignored(worktree: &Path) -> Result<()> {
    let listed = bytes(
        worktree,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
        None,
    )?;
    let Some(raw) = listed
        .split(|byte| *byte == 0)
        .find(|path| !path.is_empty())
    else {
        return Ok(());
    };
    let path = String::from_utf8_lossy(raw);
    bail!("ignored path {path:?} cannot be salvaged automatically; worktree left intact")
}

fn validate_untracked(worktree: &Path) -> Result<()> {
    let listed = bytes(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
    )?;
    for raw in listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw).context("salvage requires UTF-8 repository paths")?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            bail!("Git returned unsafe untracked path {path:?}")
        }
        let kind = fs::symlink_metadata(worktree.join(relative))
            .with_context(|| format!("reading untracked path {path:?}"))?
            .file_type();
        if !kind.is_file() && !kind.is_symlink() {
            bail!("special untracked path {path:?} cannot be salvaged automatically")
        }
    }
    Ok(())
}
