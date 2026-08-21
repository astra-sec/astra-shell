use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use base64ct::{Base64Unpadded, Encoding};
use sha2::{Digest, Sha256};

const CERTIFICATE_ALGORITHM: &str = "x509-cert-sha256";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StrictHostKeyChecking {
    Yes,
    #[default]
    Ask,
    AcceptNew,
    No,
}

impl FromStr for StrictHostKeyChecking {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "yes" | "on" | "true" => Ok(Self::Yes),
            "ask" => Ok(Self::Ask),
            "accept-new" => Ok(Self::AcceptNew),
            "no" | "off" | "false" => Ok(Self::No),
            _ => bail!(
                "invalid StrictHostKeyChecking value {value:?}; expected yes, ask, accept-new, or no"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostTrustResult {
    Known,
    Added,
    AcceptedWithoutSaving,
}

pub fn default_known_hosts_file() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(directory).join("astra/known_hosts"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context(
            "cannot locate the user configuration directory; use -o UserKnownHostsFile=PATH",
        )?;
    Ok(home.join(".config/astra/known_hosts"))
}

pub fn certificate_fingerprint(certificate: &[u8]) -> String {
    let digest = Sha256::digest(certificate);
    format!("SHA256:{}", Base64Unpadded::encode_string(&digest))
}

pub fn verify_server_certificate(
    host: &str,
    port: u16,
    certificate: &[u8],
    known_hosts_file: &Path,
    policy: StrictHostKeyChecking,
) -> Result<HostTrustResult> {
    verify_server_certificate_with_confirmation(
        host,
        port,
        certificate,
        known_hosts_file,
        policy,
        prompt_for_confirmation,
    )
}

fn verify_server_certificate_with_confirmation<F>(
    host: &str,
    port: u16,
    certificate: &[u8],
    known_hosts_file: &Path,
    policy: StrictHostKeyChecking,
    confirm: F,
) -> Result<HostTrustResult>
where
    F: FnOnce(&str, &str) -> Result<bool>,
{
    if certificate.is_empty() {
        bail!("server presented an empty TLS certificate")
    }
    let host_key = host_key(host, port)?;
    let fingerprint = certificate_fingerprint(certificate);
    let parent = prepare_parent_directory(known_hosts_file)?;
    let lock_path = lock_path(known_hosts_file, &parent)?;
    let _lock = KnownHostsLock::acquire(&lock_path)?;
    let contents = read_known_hosts(known_hosts_file)?;

    let mut expected = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        let entry = line.split_once('#').map_or(line, |(entry, _)| entry).trim();
        if entry.is_empty() {
            continue;
        }
        let fields: Vec<_> = entry.split_whitespace().collect();
        if fields.first() != Some(&host_key.as_str()) {
            continue;
        }
        if fields.len() < 3 {
            bail!(
                "malformed Astra known-host entry for {host_key} at {}:{}",
                known_hosts_file.display(),
                line_number + 1
            )
        }
        if fields[1] != CERTIFICATE_ALGORITHM {
            continue;
        }
        if fields[2] == fingerprint {
            return Ok(HostTrustResult::Known);
        }
        expected.push(fields[2].to_owned());
    }

    if !expected.is_empty() && policy != StrictHostKeyChecking::No {
        bail!(
            "REMOTE HOST CERTIFICATE HAS CHANGED for {host_key}\nexpected {}, but the server presented {fingerprint}\nremove the stale entry from {} only after verifying the server through a trusted channel",
            expected.join(" or "),
            known_hosts_file.display()
        )
    }

    if !expected.is_empty() {
        eprintln!(
            "WARNING: host certificate for {host_key} changed to {fingerprint}; accepting it because StrictHostKeyChecking=no"
        );
        return Ok(HostTrustResult::AcceptedWithoutSaving);
    }

    match policy {
        StrictHostKeyChecking::Yes => bail!(
            "no trusted host certificate for {host_key} in {}; connect once with the default interactive policy or use -o StrictHostKeyChecking=accept-new",
            known_hosts_file.display()
        ),
        StrictHostKeyChecking::Ask => {
            if !confirm(&host_key, &fingerprint)? {
                bail!("host certificate was not trusted")
            }
            append_known_host(known_hosts_file, &contents, &host_key, &fingerprint)?;
            eprintln!(
                "Warning: permanently added {host_key} ({fingerprint}) to Astra known hosts."
            );
            Ok(HostTrustResult::Added)
        }
        StrictHostKeyChecking::AcceptNew => {
            append_known_host(known_hosts_file, &contents, &host_key, &fingerprint)?;
            eprintln!(
                "Warning: permanently added {host_key} ({fingerprint}) to Astra known hosts."
            );
            Ok(HostTrustResult::Added)
        }
        StrictHostKeyChecking::No => {
            eprintln!(
                "Warning: accepting unrecorded host certificate for {host_key} ({fingerprint}) because StrictHostKeyChecking=no."
            );
            Ok(HostTrustResult::AcceptedWithoutSaving)
        }
    }
}

fn host_key(host: &str, port: u16) -> Result<String> {
    let host = host.trim();
    if host.is_empty()
        || host.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '#'
        })
    {
        bail!("invalid host name for known-host verification")
    }
    let normalized = match host.parse::<IpAddr>() {
        Ok(address) => address.to_string(),
        Err(_) => host.trim_end_matches('.').to_ascii_lowercase(),
    };
    if normalized.is_empty() {
        bail!("invalid host name for known-host verification")
    }
    Ok(format!("[{normalized}]:{port}"))
}

fn prepare_parent_directory(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir().context("failed to find current directory")?);
    let existed = parent.exists();
    if !existed {
        fs::create_dir_all(&parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        fs::set_permissions(
            &parent,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .with_context(|| format!("failed to secure {}", parent.display()))?;
    }
    let metadata = fs::symlink_metadata(&parent)
        .with_context(|| format!("failed to inspect {}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "known-hosts parent {} is not a real directory",
            parent.display()
        )
    }
    Ok(parent)
}

fn lock_path(path: &Path, parent: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("known-hosts path must have a valid file name")?;
    Ok(parent.join(format!("{file_name}.lock")))
}

fn read_known_hosts(path: &Path) -> Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", path.display()));
        }
    };
    validate_known_hosts_file(path, &file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(contents)
}

fn validate_known_hosts_file(path: &Path, file: &fs::File) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Astra known-hosts path {} is not a regular file",
            path.display()
        )
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if metadata.uid() != nix::unistd::geteuid().as_raw() {
            bail!(
                "Astra known-hosts file {} has the wrong owner",
                path.display()
            )
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            bail!(
                "Astra known-hosts file {} is group/other writable ({:o})",
                path.display(),
                mode & 0o777
            )
        }
    }
    Ok(())
}

fn append_known_host(path: &Path, contents: &str, host_key: &str, fingerprint: &str) -> Result<()> {
    let mut updated = contents.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(host_key);
    updated.push(' ');
    updated.push_str(CERTIFICATE_ALGORITHM);
    updated.push(' ');
    updated.push_str(fingerprint);
    updated.push('\n');
    atomic_write(path, updated.as_bytes())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("known-hosts path must have a valid file name")?;
    let temporary = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(contents)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn prompt_for_confirmation(host_key: &str, fingerprint: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        let tty = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context(
                "cannot ask for host confirmation without a terminal; use -o StrictHostKeyChecking=accept-new for an unattended first connection",
            )?;
        let mut writer = tty
            .try_clone()
            .context("failed to open terminal for host confirmation")?;
        writeln!(
            writer,
            "The authenticity of Astra host {host_key} cannot be established."
        )?;
        writeln!(writer, "X.509 certificate fingerprint is {fingerprint}.")?;
        write!(
            writer,
            "Are you sure you want to continue connecting (yes/no)? "
        )?;
        writer.flush()?;
        let mut answer = String::new();
        BufReader::new(tty).read_line(&mut answer)?;
        Ok(answer.trim().eq_ignore_ascii_case("yes"))
    }
    #[cfg(not(unix))]
    {
        let _ = (host_key, fingerprint);
        Err(anyhow::anyhow!(
            "interactive host confirmation is not supported on this platform; use -o StrictHostKeyChecking=accept-new"
        ))
    }
}

#[cfg(unix)]
struct KnownHostsLock(fs::File);

#[cfg(unix)]
impl KnownHostsLock {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
            bail!(
                "known-hosts lock {} is not a safe regular file",
                path.display()
            )
        }
        file.set_permissions(
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )?;
        // SAFETY: flock operates on the valid descriptor, which remains alive in this guard.
        if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self(file))
    }
}

#[cfg(unix)]
impl Drop for KnownHostsLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is valid until this drop implementation returns.
        let _ = unsafe { nix::libc::flock(self.0.as_raw_fd(), nix::libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
struct KnownHostsLock;

#[cfg(not(unix))]
impl KnownHostsLock {
    fn acquire(_path: &Path) -> Result<Self> {
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_new_records_and_then_recognizes_certificate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("astra/known_hosts");
        assert_eq!(
            verify_server_certificate(
                "EXAMPLE.com.",
                4433,
                b"certificate one",
                &path,
                StrictHostKeyChecking::AcceptNew,
            )
            .unwrap(),
            HostTrustResult::Added
        );
        assert_eq!(
            verify_server_certificate(
                "example.com",
                4433,
                b"certificate one",
                &path,
                StrictHostKeyChecking::Yes,
            )
            .unwrap(),
            HostTrustResult::Known
        );
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[example.com]:4433 x509-cert-sha256 SHA256:"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn changed_certificate_is_rejected_without_overwriting_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_hosts");
        verify_server_certificate(
            "127.0.0.1",
            4433,
            b"original",
            &path,
            StrictHostKeyChecking::AcceptNew,
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let error = verify_server_certificate(
            "127.0.0.1",
            4433,
            b"replacement",
            &path,
            StrictHostKeyChecking::AcceptNew,
        )
        .unwrap_err();
        assert!(error.to_string().contains("HAS CHANGED"));
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn yes_rejects_unknown_and_ask_can_confirm_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_hosts");
        assert!(
            verify_server_certificate(
                "::1",
                8443,
                b"certificate",
                &path,
                StrictHostKeyChecking::Yes,
            )
            .is_err()
        );
        assert_eq!(
            verify_server_certificate_with_confirmation(
                "::1",
                8443,
                b"certificate",
                &path,
                StrictHostKeyChecking::Ask,
                |host, fingerprint| {
                    assert_eq!(host, "[::1]:8443");
                    assert!(fingerprint.starts_with("SHA256:"));
                    Ok(true)
                },
            )
            .unwrap(),
            HostTrustResult::Added
        );
    }
}
