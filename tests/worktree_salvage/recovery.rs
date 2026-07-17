use super::*;

fn git_path(worktree: &Path, name: &str) -> PathBuf {
    let path = PathBuf::from(git_text(worktree, &["rev-parse", "--git-path", name]));
    if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    }
}

#[test]
fn dry_run_refuses_an_exact_salvage_after_worktree_drift() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("tracked.txt"), "first\n").unwrap();
    let branch_ref = format!("refs/heads/{}", fixture.branch);
    let branch = git_path(&fixture.worktree, &branch_ref);
    let mut lock_name = branch.file_name().unwrap().to_os_string();
    lock_name.push(".lock");
    let lock = branch.with_file_name(lock_name);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "concurrent branch writer").unwrap();

    fixture.release_error();
    assert!(!salvage_refs(&fixture.repo).is_empty());
    fs::remove_file(lock).unwrap();
    fs::write(fixture.worktree.join("tracked.txt"), "later\n").unwrap();

    let report = worktree::reap(&fixture.cache, &fixture.repo, 0, true).unwrap();

    assert!(report.reaped.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("worktree changed"));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("tracked.txt")).unwrap(),
        "later\n"
    );
}
