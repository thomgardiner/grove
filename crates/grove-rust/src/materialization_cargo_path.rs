use super::Roots;
use anyhow::{Context as _, Result};
use std::path::Path;

pub(super) fn repo_path(value: &str, roots: &Roots) -> String {
    let normalized = logical(value);
    let Some(suffix) = normalized.strip_prefix(&roots[0]) else {
        return normalized;
    };
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return normalized;
    }
    format!("$REPO{suffix}")
}

#[rustfmt::skip]
pub(super) fn repo_id(value: &str, roots: &Roots) -> String { roots.iter().fold(logical(value), |value, root| value.replace(root, "$REPO")) }

#[cfg(not(windows))]
pub(super) fn logical(value: &str) -> String {
    value.into()
}

#[cfg(windows)]
pub(super) fn logical(value: &str) -> String {
    windows(value)
}

#[cfg(any(windows, test))]
fn windows(value: &str) -> String {
    let value = value
        .replace('\\', "/")
        .replace("file:////?/UNC/", "file://")
        .replace("file:////?/", "file:///");
    if let Some(suffix) = value.strip_prefix("//?/UNC/") {
        format!("//{suffix}")
    } else if let Some(suffix) = value.strip_prefix("//?/") {
        suffix.into()
    } else {
        value
    }
}

#[rustfmt::skip]
pub(super) fn text(path: &Path) -> Result<String> { path.to_str().context("Cargo fingerprint path is not UTF-8").map(logical) }

pub(super) fn file_url(path: &Path) -> Result<String> {
    let mut encoded = String::new();
    for byte in text(path)?.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    #[cfg(windows)]
    if let Some(unc) = encoded.strip_prefix("//") {
        return Ok(format!("file://{unc}"));
    }
    Ok(if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    })
}

#[cfg(test)]
mod tests {
    use super::windows;

    #[test]
    fn windows_drive_and_unc_forms_share_logical_paths() {
        assert_eq!(windows(r"C:\src\grove"), windows(r"\\?\C:\src\grove"));
        assert_eq!(
            windows(r"\\server\share\grove"),
            windows(r"\\?\UNC\server\share\grove")
        );
        assert_eq!(
            windows("path+file:///C:/src/grove#grove@0.3.2"),
            windows("path+file:////?/C:/src/grove#grove@0.3.2")
        );
        assert_eq!(
            windows("path+file://server/share/grove#grove@0.3.2"),
            windows("path+file:////?/UNC/server/share/grove#grove@0.3.2")
        );
    }
}
