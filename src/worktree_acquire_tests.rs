use super::*;
use tempfile::tempdir;

#[test]
fn intent_is_durable_before_the_git_callback_runs() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git::run(&repo, &["init", "-q"]).unwrap();
    git::run(&repo, &["config", "user.email", "t@example.com"]).unwrap();
    git::run(&repo, &["config", "user.name", "grove-test"]).unwrap();
    fs::write(repo.join("file"), "x").unwrap();
    git::run(&repo, &["add", "."]).unwrap();
    git::run(&repo, &["commit", "-q", "-m", "init"]).unwrap();
    let root = base.path().join("cache");
    let request = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "agent".into(),
        branch: None,
        base: "HEAD".into(),
    };

    let error = acquire_with(&request, None, |ctx, _, _, _, _| {
        assert_eq!(read_intents(&root, &ctx.repo_id)?.len(), 1);
        bail!("injected Git failure")
    })
    .unwrap_err();

    assert!(error.to_string().contains("injected Git failure"));
    let ctx = repo_context(&repo).unwrap();
    assert_eq!(read_intents(&root, &ctx.repo_id).unwrap().len(), 1);
}
