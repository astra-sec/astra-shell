use std::{
    ffi::{CStr, CString},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
#[cfg(not(target_vendor = "apple"))]
use nix::unistd::getgrouplist;
use nix::unistd::{Gid, Uid, User};

#[derive(Clone, Debug)]
pub struct SystemAccount {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
    pub supplementary_groups: Vec<u32>,
}

impl SystemAccount {
    pub fn lookup(username: &str) -> Result<Self> {
        if username.is_empty() || username.len() > 256 || username.contains('\0') {
            bail!("invalid Unix username")
        }
        let user = User::from_name(username)
            .context("system user lookup failed")?
            .with_context(|| format!("unknown Unix user {username}"))?;
        let c_username = CString::new(user.name.as_bytes())?;
        let supplementary_groups = supplementary_groups(&c_username, user.gid)?;
        Ok(Self {
            username: user.name,
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            home: user.dir,
            shell: user.shell,
            supplementary_groups,
        })
    }

    pub fn current() -> Result<Self> {
        let uid = Uid::effective();
        let user = User::from_uid(uid)
            .context("current system user lookup failed")?
            .context("effective UID has no passwd entry")?;
        Self::lookup(&user.name)
    }
}

#[cfg(not(target_vendor = "apple"))]
fn supplementary_groups(username: &CStr, gid: Gid) -> Result<Vec<u32>> {
    Ok(getgrouplist(username, gid)
        .context("supplementary group lookup failed")?
        .into_iter()
        .map(Gid::as_raw)
        .collect())
}

#[cfg(target_vendor = "apple")]
fn supplementary_groups(username: &CStr, gid: Gid) -> Result<Vec<u32>> {
    let mut groups = Vec::<nix::libc::c_int>::with_capacity(8);
    loop {
        let capacity = groups.capacity();
        let mut count = nix::libc::c_int::try_from(capacity)
            .context("supplementary group list is too large")?;
        // SAFETY: `groups` has capacity for `count` entries and the OS only
        // initializes that buffer. The length is set after a successful call.
        let result = unsafe {
            nix::libc::getgrouplist(
                username.as_ptr(),
                gid.as_raw() as nix::libc::c_int,
                groups.as_mut_ptr(),
                &mut count,
            )
        };
        if result >= 0 {
            let count = usize::try_from(count).context("invalid supplementary group count")?;
            if count > capacity {
                bail!("supplementary group lookup returned an oversized result")
            }
            // SAFETY: `getgrouplist` reported that it initialized `count`
            // entries, and the bounds check above keeps them within capacity.
            unsafe { groups.set_len(count) };
            return groups
                .into_iter()
                .map(|group| u32::try_from(group).context("invalid supplementary group ID"))
                .collect();
        }

        let requested = usize::try_from(count).unwrap_or(0);
        let next_capacity = requested.max(capacity.saturating_mul(2));
        if next_capacity <= capacity {
            bail!("supplementary group list is too large")
        }
        groups.reserve(next_capacity);
    }
}

pub fn effective_uid() -> u32 {
    Uid::effective().as_raw()
}

pub fn authorized_key_files(
    account: &SystemAccount,
    override_directory: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let candidates = if let Some(directory) = override_directory {
        check_secure_path(directory, account.uid, true)?;
        vec![directory.join(&account.username)]
    } else {
        let ssh_directory = account.home.join(".ssh");
        check_secure_path(&account.home, account.uid, true)?;
        check_secure_path(&ssh_directory, account.uid, true)?;
        vec![
            ssh_directory.join("authorized_keys"),
            ssh_directory.join("authorized_keys2"),
        ]
    };

    let mut files = Vec::new();
    for path in candidates {
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                check_secure_path(&path, account.uid, false)?;
                files.push(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if files.is_empty() {
        bail!("no authorized_keys file is configured for the requested user")
    }
    Ok(files)
}

fn check_secure_path(path: &Path, user_uid: u32, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{} must not be a symbolic link", path.display())
    }
    if directory && !metadata.is_dir() {
        bail!("{} is not a directory", path.display())
    }
    if !directory && !metadata.is_file() {
        bail!("{} is not a regular file", path.display())
    }
    if metadata.uid() != user_uid && metadata.uid() != 0 {
        bail!(
            "{} is owned by UID {}, expected UID {} or root",
            path.display(),
            metadata.uid(),
            user_uid
        )
    }
    if metadata.mode() & 0o022 != 0 {
        bail!("{} is writable by group or other users", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn override_directory_uses_username_and_strict_modes() {
        let account = SystemAccount::current().unwrap();
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key_file = directory.path().join(&account.username);
        fs::write(&key_file, "ssh-ed25519 AAAA test\n").unwrap();
        fs::set_permissions(&key_file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            authorized_key_files(&account, Some(directory.path())).unwrap(),
            vec![key_file.clone()]
        );

        fs::set_permissions(&key_file, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(authorized_key_files(&account, Some(directory.path())).is_err());
    }
}
