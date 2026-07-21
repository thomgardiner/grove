use super::Admission;

#[cfg(unix)]
pub(super) fn setup(file: &std::fs::File, admission: Admission<'_>) -> std::io::Result<bool> {
    use fs2::FileExt;

    match admission {
        Admission::Wait => file.lock_exclusive().map(|()| true),
        Admission::Try => attempt(file),
        Admission::Until(cancelled) => loop {
            if cancelled() {
                return Ok(false);
            }
            if attempt(file)? {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        },
    }
}

#[cfg(unix)]
fn attempt(file: &std::fs::File) -> std::io::Result<bool> {
    use fs2::FileExt;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    }
}
