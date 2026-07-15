use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Component;
use std::path::Path;
#[cfg(any(test, unix))]
use std::path::PathBuf;

#[path = "release_publish_pin.rs"]
mod pin;

pub(super) struct Publication {
    destination: super::Destination,
    #[cfg(test)]
    stage: PathBuf,
    #[cfg(unix)]
    stage_name: OsString,
    #[cfg(unix)]
    out_name: OsString,
    published: bool,
    #[cfg(unix)]
    stage_pin: pin::PinnedDirectory,
    #[cfg(unix)]
    stage_parent: pin::PinnedDirectory,
}

impl Publication {
    pub(super) fn prepare(
        root: &Path,
        workspace: &Path,
        destination: super::Destination,
    ) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (root, workspace, destination);
            bail!("secure frozen-release publication is not supported on this platform")
        }
        #[cfg(unix)]
        {
            destination.matches_parent()?;
            let base = pin::stage_base(root, workspace)?;
            let (stage, pin) = pin::create_stage(&base)?;
            let out_name = leaf(destination.resolved(), "release bundle")?;
            Self::pin(destination, stage, pin, base, out_name)
        }
    }

    pub(super) fn stage_file(&self) -> Result<&File> {
        #[cfg(unix)]
        {
            self.stage_parent.matches("release stage parent")?;
            self.stage_pin.matches("release stage")?;
            Ok(&self.stage_pin.file)
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            bail!("secure frozen-release publication is not supported on this platform")
        }
    }

    #[cfg(test)]
    pub(super) fn stage_path(&self) -> &Path {
        &self.stage
    }

    pub(super) fn output(&self) -> &Path {
        self.destination.visible()
    }

    pub(super) fn publish(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.before_publish()?;
            publish(self)?;
            self.after_publish()?;
            self.published = true;
            Ok(())
        }
        #[cfg(not(unix))]
        bail!("secure frozen-release publication is not supported on this platform")
    }

    #[cfg(unix)]
    fn pin(
        destination: super::Destination,
        stage: PathBuf,
        stage_pin: pin::PinnedDirectory,
        stage_parent: pin::PinnedDirectory,
        out_name: OsString,
    ) -> Result<Self> {
        if stage_parent.file.metadata()?.dev() != destination.parent_file().metadata()?.dev() {
            bail!("release bundle destination must share a filesystem with the Grove cache")
        }
        Ok(Self {
            destination,
            #[cfg(unix)]
            stage_name: leaf(&stage, "release stage")?,
            #[cfg(test)]
            stage,
            out_name,
            published: false,
            stage_pin,
            stage_parent,
        })
    }

    #[cfg(unix)]
    fn before_publish(&self) -> Result<()> {
        self.stage_parent.matches("release stage parent")?;
        self.destination.matches_parent()?;
        self.stage_pin.matches("release stage")?;
        Ok(())
    }

    #[cfg(unix)]
    fn after_publish(&self) -> Result<()> {
        self.destination.matches_parent()?;
        self.stage_pin
            .matches_path(self.destination.resolved(), "published release bundle")
    }

    #[cfg(unix)]
    fn cleanup(&self) {
        if self.stage_parent.matches("release stage parent").is_ok()
            && self.stage_pin.matches("release stage").is_ok()
        {
            let _ = super::cleanup::clear(&self.stage_pin.file);
            if self.stage_parent.matches("release stage parent").is_ok()
                && self.stage_pin.matches("release stage").is_ok()
            {
                super::cleanup::remove_empty(&self.stage_parent.file, Path::new(&self.stage_name));
            }
        }
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        if !self.published {
            #[cfg(unix)]
            self.cleanup();
        }
    }
}

#[cfg(unix)]
fn leaf(path: &Path, what: &str) -> Result<OsString> {
    let name = path
        .file_name()
        .context(format!("{what} needs a directory name"))?;
    if !matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) {
        bail!("{what} needs one normal directory name")
    }
    Ok(name.to_os_string())
}

#[cfg(unix)]
fn publish(publication: &Publication) -> Result<()> {
    use rustix::fs::{RenameFlags, renameat_with};

    renameat_with(
        &publication.stage_parent.file,
        Path::new(&publication.stage_name),
        publication.destination.parent_file(),
        Path::new(&publication.out_name),
        RenameFlags::NOREPLACE,
    )
    .with_context(|| format!("publishing {}", publication.destination.visible().display()))
}

#[cfg(test)]
mod tests {
    use super::{super::release_destination, Publication};
    use std::{fs, os::unix::fs::symlink};
    use tempfile::tempdir;

    #[test]
    fn destination_keeps_resolved_parent_after_symlink_retarget() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let link = dir.path().join("out");
        symlink(&first, &link).unwrap();

        let destination = release_destination(&workspace, &link.join("bundle")).unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();

        assert!(Publication::prepare(dir.path(), &workspace, destination).is_err());
        assert!(!first.join("bundle").exists());
        assert!(!second.join("bundle").exists());
    }

    #[test]
    fn publication_never_cleans_a_swapped_stage_leaf() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let parent = dir.path().join("out");
        let victim = dir.path().join("victim");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&victim).unwrap();
        let sentinel = victim.join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        let destination = release_destination(&workspace, &parent.join("bundle")).unwrap();
        let mut publication = Publication::prepare(dir.path(), &workspace, destination).unwrap();
        let stage = publication.stage_path().to_path_buf();
        fs::remove_dir(&stage).unwrap();
        symlink(&victim, &stage).unwrap();

        assert!(publication.publish().is_err());
        drop(publication);
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    }
}
