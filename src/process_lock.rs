use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

pub struct ProcessLock {
    _file: fs::File,
}

impl ProcessLock {
    #[cfg(unix)]
    pub fn acquire(path: &Path) -> Result<Self> {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        };

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("failed to open process lock {}", path.display()))?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == crate::accounts::effective_uid()
                && metadata.mode() & 0o022 == 0,
            "unsafe process lock {}",
            path.display()
        );
        // SAFETY: flock only reads the valid file descriptor and does not
        // retain the pointer or descriptor beyond this call. The File remains
        // alive in ProcessLock for the entire lock lifetime.
        let result =
            unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            bail!("process lock {} is already held: {error}", path.display())
        }
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("failed to acquire process lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_links_and_never_changes_their_target_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::write(&target, "original").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = temporary.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(ProcessLock::acquire(&link).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(&target, &link).unwrap();
        assert!(ProcessLock::acquire(&link).is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn refuses_a_second_holder() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("daemon.lock");
        let _first = ProcessLock::acquire(&path).unwrap();
        assert!(ProcessLock::acquire(&path).is_err());
    }
}
