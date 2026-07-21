#[cfg(windows)]
#[test]
fn git_arguments_drop_only_drive_and_unc_verbatim_prefixes() {
    assert_eq!(argument(Path::new(r"\\?\C:\src\grove")), r"C:\src\grove");
    assert_eq!(
        argument(Path::new(r"\\?\UNC\server\share\grove")),
        r"\\server\share\grove"
    );
    assert_eq!(argument(Path::new(r"C:\src\grove")), r"C:\src\grove");
    assert_eq!(
        argument(Path::new(r"\\?\Volume{abc}\grove")),
        r"\\?\Volume{abc}\grove"
    );
}
