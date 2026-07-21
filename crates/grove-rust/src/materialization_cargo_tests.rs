#[cfg(windows)]
#[test]
fn verbatim_drive_metadata_uses_the_logical_repo_root() {
    let roots = ["C:/src/grove".into(), "file:///C:/src/grove".into()];

    assert_eq!(text(Path::new(r"\\?\C:\src\grove")).unwrap(), roots[0]);
    assert_eq!(file_url(Path::new(r"\\?\C:\src\grove")).unwrap(), roots[1]);
    assert_eq!(
        repo_path(r"C:\src\grove\crates\grove-rust", &roots),
        "$REPO/crates/grove-rust"
    );
    assert_eq!(
        repo_path(r"\\?\C:\src\grove\crates\grove-rust", &roots),
        "$REPO/crates/grove-rust"
    );
    let id = repo_id(
        "path+file:///C:/src/grove#grove-rust@0.3.2",
        &roots,
    );
    assert!(id.contains("$REPO"));
    assert_eq!(
        repo_id(
            "path+file:////?/C:/src/grove#grove-rust@0.3.2",
            &roots
        ),
        id
    );
}

#[cfg(windows)]
#[test]
fn verbatim_unc_metadata_uses_the_logical_repo_root() {
    let roots = [
        "//server/share/grove".into(),
        "file://server/share/grove".into(),
    ];

    assert_eq!(
        text(Path::new(r"\\?\UNC\server\share\grove")).unwrap(),
        roots[0]
    );
    assert_eq!(
        file_url(Path::new(r"\\?\UNC\server\share\grove")).unwrap(),
        roots[1]
    );
    assert_eq!(
        repo_path(r"\\server\share\grove\crates\grove-rust", &roots),
        "$REPO/crates/grove-rust"
    );
    assert_eq!(
        repo_path(
            r"\\?\UNC\server\share\grove\crates\grove-rust",
            &roots
        ),
        "$REPO/crates/grove-rust"
    );
    let id = repo_id(
        "path+file://server/share/grove#grove-rust@0.3.2",
        &roots,
    );
    assert!(id.contains("$REPO"));
    assert_eq!(
        repo_id(
            "path+file:////?/UNC/server/share/grove#grove-rust@0.3.2",
            &roots
        ),
        id
    );
}

#[cfg(unix)]
#[test]
fn unix_metadata_preserves_literal_backslashes() {
    let roots = ["/repo".into(), "file:///repo".into()];
    let path = r"/tmp/vendor\crate/Cargo.toml";

    assert_eq!(repo_path(path, &roots), path);
}

#[cfg(unix)]
#[test]
fn unix_file_urls_preserve_double_slash_paths() {
    assert_eq!(
        file_url(Path::new("//server/share/grove")).unwrap(),
        "file:////server/share/grove"
    );
}
