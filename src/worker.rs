use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use nix::unistd::{Gid, Uid, chown};
use tokio::{
    io::{AsyncWriteExt, copy},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::Mutex,
    task::JoinSet,
};
use tracing::{info, warn};

use crate::{
    accounts::{SystemAccount, effective_uid},
    files::FileService,
    negotiation::{NegotiatedProtocol, selections},
    process_lock::ProcessLock,
    protocol::{WireMessage, WorkerStreamHello, wire_message, write_message},
    server::handle_worker_request,
    terminal::TerminalManager,
};

#[derive(Debug)]
pub struct WorkerRouter {
    users_root: PathBuf,
    session_root_override: Option<PathBuf>,
    idle_timeout: Duration,
    start_lock: Mutex<()>,
}

pub const DEFAULT_WORKER_IDLE_TIMEOUT_SECONDS: u64 = 10 * 60;
const MAX_WORKER_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

impl WorkerRouter {
    pub fn new(
        state_dir: &Path,
        session_root_override: Option<PathBuf>,
        idle_timeout: Duration,
    ) -> Result<Arc<Self>> {
        let users_root = state_dir.join("users");
        fs::create_dir_all(&users_root)?;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o711))?;
        fs::set_permissions(&users_root, fs::Permissions::from_mode(0o711))?;
        Ok(Arc::new(Self {
            users_root,
            session_root_override,
            idle_timeout,
            start_lock: Mutex::new(()),
        }))
    }

    pub async fn proxy_stream(
        &self,
        account: &SystemAccount,
        mut quic_send: quinn::SendStream,
        mut quic_recv: quinn::RecvStream,
        first_message: WireMessage,
        negotiated: NegotiatedProtocol,
    ) -> Result<()> {
        let worker = self.connect(account).await?;
        let (mut worker_recv, mut worker_send) = worker.into_split();
        write_message(
            &mut worker_send,
            &WireMessage::new(wire_message::Body::WorkerStreamHello(WorkerStreamHello {
                protocol_version: negotiated.version,
                capabilities: selections(&negotiated),
            })),
        )
        .await?;
        write_message(&mut worker_send, &first_message).await?;
        let client_to_worker = async {
            copy(&mut quic_recv, &mut worker_send).await?;
            worker_send.shutdown().await
        };
        let worker_to_client = async {
            copy(&mut worker_recv, &mut quic_send).await?;
            quic_send.shutdown().await
        };
        tokio::try_join!(client_to_worker, worker_to_client)?;
        Ok(())
    }

    async fn connect(&self, account: &SystemAccount) -> Result<UnixStream> {
        let socket = self.socket_path(account.uid);
        if let Ok(stream) = UnixStream::connect(&socket).await {
            return Ok(stream);
        }

        let _guard = self.start_lock.lock().await;
        if let Ok(stream) = UnixStream::connect(&socket).await {
            return Ok(stream);
        }
        let user_state = self.prepare_user_state(account)?;
        self.spawn_worker(account, &user_state, &socket)?;
        let mut last_error = None;
        for _ in 0..100 {
            match UnixStream::connect(&socket).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(last_error
            .context("user worker did not create its control socket")?
            .into())
    }

    fn socket_path(&self, uid: u32) -> PathBuf {
        self.users_root.join(uid.to_string()).join("session.sock")
    }

    fn prepare_user_state(&self, account: &SystemAccount) -> Result<PathBuf> {
        let user_state = self.users_root.join(account.uid.to_string());
        match fs::symlink_metadata(&user_state) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "{} is not a safe user state directory",
                        user_state.display()
                    )
                }
                if metadata.uid() != account.uid {
                    bail!(
                        "{} is owned by UID {}, expected {}",
                        user_state.display(),
                        metadata.uid(),
                        account.uid
                    )
                }
                if metadata.mode() & 0o077 != 0 {
                    bail!("{} must have mode 0700", user_state.display())
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&user_state)?;
                fs::set_permissions(&user_state, fs::Permissions::from_mode(0o700))?;
                if effective_uid() == 0 {
                    chown(
                        &user_state,
                        Some(Uid::from_raw(account.uid)),
                        Some(Gid::from_raw(account.gid)),
                    )?;
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(user_state)
    }

    fn spawn_worker(
        &self,
        account: &SystemAccount,
        user_state: &Path,
        socket: &Path,
    ) -> Result<()> {
        let current_uid = effective_uid();
        if current_uid != 0 && current_uid != account.uid {
            bail!(
                "managed mode must run as root to serve UID {}; current effective UID is {}",
                account.uid,
                current_uid
            )
        }
        let session_root = self
            .session_root_override
            .clone()
            .unwrap_or_else(|| account.home.clone());
        let executable = worker_executable()?;
        #[cfg(target_os = "linux")]
        let mut command = {
            let supplementary_groups = account
                .supplementary_groups
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let mut command = Command::new("/usr/bin/setpriv");
            command
                .arg(format!("--reuid={}", account.uid))
                .arg(format!("--regid={}", account.gid))
                .arg(format!("--groups={supplementary_groups}"))
                .arg("--")
                .arg(executable);
            command
        };
        #[cfg(not(target_os = "linux"))]
        let mut command = Command::new(executable);
        command
            .arg("worker")
            .arg("--socket")
            .arg(socket)
            .arg("--state-dir")
            .arg(user_state)
            .arg("--session-root")
            .arg(&session_root)
            .arg("--expected-uid")
            .arg(account.uid.to_string())
            .arg("--idle-timeout-seconds")
            .arg(self.idle_timeout.as_secs().to_string())
            .env_clear()
            .env("HOME", &account.home)
            .env("USER", &account.username)
            .env("LOGNAME", &account.username)
            .env("SHELL", &account.shell)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);

        #[cfg(not(target_os = "linux"))]
        if current_uid == 0 && account.uid != 0 {
            install_child_credentials(&mut command, account);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start worker for {}", account.username))?;
        let username = account.username.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => info!(%username, "user worker exited"),
                Ok(status) => warn!(%username, %status, "user worker failed"),
                Err(error) => warn!(%username, %error, "failed to wait for user worker"),
            }
        });
        Ok(())
    }
}

fn worker_executable() -> Result<PathBuf> {
    // Resolve the real path before the child drops privileges. Some hardened
    // procfs configurations deny exec through /proc/self/exe after setuid,
    // even when the underlying installed binary is executable by the user.
    Ok(std::env::current_exe()?)
}

#[cfg(not(target_os = "linux"))]
fn install_child_credentials(command: &mut Command, account: &SystemAccount) {
    let uid = account.uid as nix::libc::uid_t;
    let gid = account.gid as nix::libc::gid_t;
    let groups: Vec<nix::libc::gid_t> = account
        .supplementary_groups
        .iter()
        .copied()
        .map(|group| group as nix::libc::gid_t)
        .collect();
    // SAFETY: the closure only invokes async-signal-safe credential syscalls
    // using memory allocated before fork. Credential changes occur in the
    // child immediately before exec and cannot affect the gateway process.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_vendor = "apple")]
            let group_count = groups.len().try_into().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "supplementary group list is too large",
                )
            })?;
            #[cfg(not(target_vendor = "apple"))]
            let group_count = groups.len();
            if nix::libc::setgroups(group_count, groups.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::setgid(gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub async fn serve_worker(
    socket: PathBuf,
    state_dir: PathBuf,
    session_root: PathBuf,
    expected_uid: u32,
    idle_timeout: Duration,
) -> Result<()> {
    let actual_uid = effective_uid();
    if actual_uid != expected_uid {
        bail!(
            "worker credential mismatch: expected UID {expected_uid}, running as UID {actual_uid}"
        )
    }
    let _worker_lock = ProcessLock::acquire(&state_dir.join("worker.lock"))?;
    let pid_file = state_dir.join("worker.pid");
    fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    fs::set_permissions(&pid_file, fs::Permissions::from_mode(0o600))?;
    let manager = TerminalManager::new(session_root)?;
    let files = FileService::new(manager.session_root().to_path_buf())?;
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind worker socket {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut requests = JoinSet::new();
    let mut idle_state = WorkerIdleState::default();
    let check_interval = idle_timeout.min(MAX_WORKER_IDLE_CHECK_INTERVAL);
    loop {
        tokio::select! {
            biased;
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                idle_state.mark_active();
                let manager = manager.clone();
                let files = files.clone();
                requests.spawn(async move {
                    let (recv, send) = stream.into_split();
                    handle_worker_request(manager, files, send, recv).await
                });
            }
            completed = requests.join_next(), if !requests.is_empty() => {
                match completed {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => warn!(%error, "worker request failed"),
                    Some(Err(error)) => warn!(%error, "worker request task failed"),
                    None => {}
                }
                idle_state.observe(
                    requests.is_empty() && !manager.has_active_terminals(),
                    std::time::Instant::now(),
                );
            }
            _ = tokio::time::sleep(check_interval), if !idle_timeout.is_zero() => {
                let now = std::time::Instant::now();
                let empty = requests.is_empty() && !manager.has_active_terminals();
                if idle_state.should_exit(empty, now, idle_timeout) {
                    info!(
                        idle_seconds = idle_timeout.as_secs(),
                        "empty user worker reached its idle timeout; exiting"
                    );
                    remove_runtime_file(&socket, "worker socket");
                    remove_runtime_file(&pid_file, "worker PID file");
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct WorkerIdleState {
    idle_since: Option<std::time::Instant>,
}

impl WorkerIdleState {
    fn mark_active(&mut self) {
        self.idle_since = None;
    }

    fn observe(&mut self, empty: bool, now: std::time::Instant) {
        if empty {
            self.idle_since.get_or_insert(now);
        } else {
            self.mark_active();
        }
    }

    fn should_exit(&mut self, empty: bool, now: std::time::Instant, timeout: Duration) -> bool {
        if timeout.is_zero() {
            self.mark_active();
            return false;
        }
        self.observe(empty, now);
        self.idle_since
            .is_some_and(|idle_since| now.duration_since(idle_since) >= timeout)
    }
}

fn remove_runtime_file(path: &Path, description: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!(%error, path = %path.display(), %description, "failed to clean up worker runtime file")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_exits_only_after_continuous_empty_timeout() {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(10);
        let mut state = WorkerIdleState::default();

        assert!(!state.should_exit(true, start, timeout));
        assert!(!state.should_exit(true, start + Duration::from_secs(9), timeout));
        assert!(state.should_exit(true, start + timeout, timeout));
    }

    #[test]
    fn activity_restarts_worker_idle_timer() {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(10);
        let mut state = WorkerIdleState::default();

        assert!(!state.should_exit(true, start, timeout));
        assert!(!state.should_exit(false, start + Duration::from_secs(9), timeout));
        assert!(!state.should_exit(true, start + Duration::from_secs(10), timeout));
        assert!(!state.should_exit(true, start + Duration::from_secs(19), timeout));
        assert!(state.should_exit(true, start + Duration::from_secs(20), timeout));
    }

    #[test]
    fn zero_worker_idle_timeout_disables_recycling() {
        let start = std::time::Instant::now();
        let mut state = WorkerIdleState::default();

        assert!(!state.should_exit(true, start, Duration::ZERO));
        assert!(!state.should_exit(true, start + Duration::from_secs(86_400), Duration::ZERO));
    }

    #[tokio::test]
    async fn in_flight_request_prevents_worker_recycling() {
        let temporary = tempfile::tempdir().unwrap();
        let state_dir = temporary.path().join("state");
        let session_root = temporary.path().join("home");
        let socket = state_dir.join("session.sock");
        fs::create_dir(&state_dir).unwrap();
        fs::create_dir(&session_root).unwrap();

        let worker = tokio::spawn(serve_worker(
            socket.clone(),
            state_dir.clone(),
            session_root,
            effective_uid(),
            Duration::from_millis(50),
        ));
        for _ in 0..20 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let request = UnixStream::connect(&socket).await.unwrap();

        tokio::time::sleep(Duration::from_millis(175)).await;
        assert!(!worker.is_finished());

        drop(request);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("empty worker did not exit after its idle timeout")
            .unwrap()
            .unwrap();
        assert!(!socket.exists());
        assert!(!state_dir.join("worker.pid").exists());
    }
}
