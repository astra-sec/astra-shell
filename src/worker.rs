use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use nix::unistd::{Gid, Uid, chown};
use prost::Message;
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
    negotiation::{CAPABILITY_DATAGRAM_STATE, NegotiatedProtocol, selections},
    process_lock::ProcessLock,
    protocol::{
        WireMessage, WorkerStreamHello, read_message, terminal_event, wire_message, write_message,
    },
    resources::{ResourceAccount, ResourceGovernor, ResourcePolicy, ResourceReservation},
    server::handle_worker_request,
};

pub struct WorkerRouter {
    users_root: PathBuf,
    session_root_override: Option<PathBuf>,
    idle_timeout: Duration,
    start_lock: Mutex<()>,
    resources: ResourceGovernor,
    resource_policy: ResourcePolicy,
    worker_capacities: Arc<Mutex<HashMap<u32, ResourceReservation>>>,
}

pub(crate) struct WorkerProxyStream {
    pub(crate) first_message: WireMessage,
    pub(crate) negotiated: NegotiatedProtocol,
    pub(crate) connection_id: String,
    pub(crate) connection: quinn::Connection,
}

pub const DEFAULT_WORKER_IDLE_TIMEOUT_SECONDS: u64 = 10 * 60;
const MAX_WORKER_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

impl WorkerRouter {
    pub fn new(
        state_dir: &Path,
        session_root_override: Option<PathBuf>,
        idle_timeout: Duration,
        resources: ResourceGovernor,
        resource_policy: ResourcePolicy,
    ) -> Result<Arc<Self>> {
        resource_policy.validate()?;
        let users_root = state_dir.join("users");
        fs::create_dir_all(&users_root)?;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o711))?;
        fs::set_permissions(&users_root, fs::Permissions::from_mode(0o711))?;
        Ok(Arc::new(Self {
            users_root,
            session_root_override,
            idle_timeout,
            start_lock: Mutex::new(()),
            resources,
            resource_policy,
            worker_capacities: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    pub(crate) async fn proxy_stream(
        &self,
        account: &SystemAccount,
        mut quic_send: quinn::SendStream,
        mut quic_recv: quinn::RecvStream,
        request: WorkerProxyStream,
    ) -> Result<()> {
        let WorkerProxyStream {
            first_message,
            negotiated,
            connection_id,
            connection,
        } = request;
        let worker = self.connect(account).await?;
        let (mut worker_recv, mut worker_send) = worker.into_split();
        write_message(
            &mut worker_send,
            &WireMessage::new(wire_message::Body::WorkerStreamHello(WorkerStreamHello {
                protocol_version: negotiated.version,
                capabilities: selections(&negotiated),
                connection_id,
                maximum_datagram_size: worker_datagram_payload_limit(&negotiated, &connection)?,
            })),
        )
        .await?;
        write_message(&mut worker_send, &first_message).await?;
        let client_to_worker = async {
            copy(&mut quic_recv, &mut worker_send).await?;
            worker_send.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let worker_to_client =
            forward_worker_messages(&mut worker_recv, &mut quic_send, &connection);
        tokio::try_join!(client_to_worker, worker_to_client)?;
        Ok(())
    }

    async fn connect(&self, account: &SystemAccount) -> Result<UnixStream> {
        self.ensure_worker_capacity(account).await?;
        match self.connect_with_capacity(account).await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                self.worker_capacities.lock().await.remove(&account.uid);
                Err(error)
            }
        }
    }

    async fn connect_with_capacity(&self, account: &SystemAccount) -> Result<UnixStream> {
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

    async fn ensure_worker_capacity(&self, account: &SystemAccount) -> Result<()> {
        let mut capacities = self.worker_capacities.lock().await;
        if capacities.contains_key(&account.uid) {
            return Ok(());
        }
        let reservation = self
            .resources
            .account(&account.username)?
            .reserve(self.resource_policy.worker_capacity_claim())?;
        capacities.insert(account.uid, reservation);
        Ok(())
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
        append_worker_resource_policy(&mut command, &self.resource_policy);

        #[cfg(not(target_os = "linux"))]
        if current_uid == 0 && account.uid != 0 {
            install_child_credentials(&mut command, account);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start worker for {}", account.username))?;
        let username = account.username.clone();
        let uid = account.uid;
        let worker_capacities = self.worker_capacities.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => info!(%username, "user worker exited"),
                Ok(status) => warn!(%username, %status, "user worker failed"),
                Err(error) => warn!(%username, %error, "failed to wait for user worker"),
            }
            worker_capacities.lock().await.remove(&uid);
        });
        Ok(())
    }
}

fn worker_datagram_payload_limit(
    negotiated: &NegotiatedProtocol,
    connection: &quinn::Connection,
) -> Result<u32> {
    if !negotiated.has(CAPABILITY_DATAGRAM_STATE, 1) {
        return Ok(0);
    }
    connection
        .max_datagram_size()
        .map(u32::try_from)
        .transpose()?
        .context("terminal datagrams were negotiated without QUIC transport support")
}

async fn forward_worker_messages<R, W>(
    worker_recv: &mut R,
    client_send: &mut W,
    connection: &quinn::Connection,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = read_message(worker_recv).await? {
        let datagram = match &message.body {
            Some(wire_message::Body::TerminalEvent(event)) => match &event.event {
                Some(terminal_event::Event::ViewportDatagram(datagram)) => {
                    ensure!(
                        !event.terminal_id.is_empty()
                            && !event.attachment_id.is_empty()
                            && event.terminal_id == datagram.terminal_id
                            && event.attachment_id == datagram.attachment_id
                            && datagram.diff.is_some(),
                        "worker emitted an inconsistent terminal datagram route"
                    );
                    Some(datagram.clone())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(datagram) = datagram {
            let encoded = datagram.encode_to_vec();
            let Some(maximum) = connection.max_datagram_size() else {
                // A worker emits a bounded reliable re-key independently, so
                // a path capability change is handled as local datagram loss.
                continue;
            };
            if encoded.len() > maximum {
                continue;
            }
            match connection.send_datagram(encoded.into()) {
                Ok(())
                | Err(quinn::SendDatagramError::TooLarge)
                | Err(quinn::SendDatagramError::UnsupportedByPeer)
                | Err(quinn::SendDatagramError::Disabled) => {}
                Err(quinn::SendDatagramError::ConnectionLost(error)) => return Err(error.into()),
            }
        } else {
            write_message(client_send, &message).await?;
        }
    }
    client_send.shutdown().await?;
    Ok(())
}

fn append_worker_resource_policy(command: &mut Command, policy: &ResourcePolicy) {
    const MIB: u64 = 1024 * 1024;
    command
        .arg("--max-user-connections")
        .arg(policy.user.connections.to_string())
        .arg("--max-user-streams")
        .arg(policy.user.streams.to_string())
        .arg("--max-user-terminals")
        .arg(policy.user.terminals.to_string())
        .arg("--max-user-attachments")
        .arg(policy.user.attachments.to_string())
        .arg("--max-user-terminal-memory-mib")
        .arg((policy.user.terminal_memory_bytes / MIB).to_string())
        .arg("--max-user-history-mib")
        .arg((policy.user.history_bytes / MIB).to_string())
        .arg("--max-user-file-handles")
        .arg(policy.user.file_handles.to_string())
        .arg("--max-user-uploads")
        .arg(policy.user.uploads.to_string())
        .arg("--max-user-upload-mib")
        .arg((policy.user.upload_bytes / MIB).to_string())
        .arg("--terminal-base-memory-mib")
        .arg((policy.terminal_base_memory_bytes / MIB).to_string())
        .arg("--terminal-cell-memory-bytes")
        .arg(policy.terminal_cell_memory_bytes.to_string())
        .arg("--terminal-history-rows")
        .arg(policy.terminal_history_rows.to_string())
        .arg("--terminal-history-mib")
        .arg((policy.terminal_history_bytes / MIB).to_string());
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
    resource_policy: ResourcePolicy,
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
    resource_policy.validate()?;
    let resources =
        ResourceAccount::standalone(format!("worker UID {expected_uid}"), resource_policy.user)?;
    let manager = crate::session::SessionManager::with_resources(
        session_root,
        state_dir.join("session-catalog.pb"),
        resources.clone(),
        resource_policy,
    )?;
    let files = FileService::with_resources(manager.session_root().to_path_buf(), resources)?;
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
                    Some(Ok(Err(error))) => warn!(error = ?error, "worker request failed"),
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
    use std::net::SocketAddr;

    use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    use super::*;
    use crate::protocol::{AuthResult, TerminalEvent, TerminalStateDiff, TerminalViewportDatagram};

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
    async fn worker_capacity_is_unique_per_uid_and_globally_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        fs::create_dir(&state).unwrap();
        let mut policy = ResourcePolicy::default();
        policy.global = policy.user;
        let resources = ResourceGovernor::new(&policy).unwrap();
        let router =
            WorkerRouter::new(&state, None, Duration::from_secs(60), resources, policy).unwrap();
        let account = SystemAccount::current().unwrap();
        router.ensure_worker_capacity(&account).await.unwrap();
        router.ensure_worker_capacity(&account).await.unwrap();
        assert_eq!(router.worker_capacities.lock().await.len(), 1);

        let mut second = account.clone();
        second.uid = second.uid.wrapping_add(1);
        second.username = format!("{}-quota-test", second.username);
        let error = router
            .ensure_worker_capacity(&second)
            .await
            .expect_err("second user worker should exceed global capacity");
        assert!(
            error
                .downcast_ref::<crate::resources::QuotaExceeded>()
                .is_some()
        );
        assert_eq!(router.worker_capacities.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn worker_bridge_keeps_control_reliable_and_lifts_viewports_to_datagrams() -> Result<()> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let certificate = cert.der().clone();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        let server_config =
            quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?));
        let server_endpoint =
            quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>()?)?;

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(certificate.to_vec()))?;
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client_config =
            quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_tls)?));
        let mut client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse::<SocketAddr>()?)?;
        client_endpoint.set_default_client_config(client_config);
        let client_connecting =
            client_endpoint.connect(server_endpoint.local_addr()?, "localhost")?;
        let server_incoming = server_endpoint
            .accept()
            .await
            .context("server endpoint closed before accepting")?;
        let (client_connection, server_connection) =
            tokio::try_join!(client_connecting, server_incoming)?;

        let legacy = NegotiatedProtocol {
            version: crate::PROTOCOL_VERSION,
            capabilities: std::collections::BTreeMap::new(),
        };
        assert_eq!(
            worker_datagram_payload_limit(&legacy, &server_connection)?,
            0,
            "an unnegotiated transport MTU must not leak into a legacy worker stream"
        );
        let semantic = NegotiatedProtocol {
            version: crate::PROTOCOL_VERSION,
            capabilities: std::collections::BTreeMap::from([(
                CAPABILITY_DATAGRAM_STATE.to_owned(),
                1,
            )]),
        };
        assert!(worker_datagram_payload_limit(&semantic, &server_connection)? > 0);

        let reliable = WireMessage::new(wire_message::Body::AuthResult(AuthResult {
            ok: true,
            message: "control".into(),
            error_code: String::new(),
        }));
        let viewport = TerminalViewportDatagram {
            terminal_id: "terminal".into(),
            attachment_id: "attachment".into(),
            diff: Some(TerminalStateDiff {
                epoch: vec![1; 16],
                base_generation: 1,
                target_generation: 2,
                ..Default::default()
            }),
            inherited_fields: 0,
        };
        let bridged = WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
            terminal_id: "terminal".into(),
            attachment_id: "attachment".into(),
            event: Some(terminal_event::Event::ViewportDatagram(Box::new(
                viewport.clone(),
            ))),
        }));
        let mut oversized_viewport = viewport.clone();
        oversized_viewport
            .diff
            .as_mut()
            .expect("test viewport has a diff")
            .epoch = vec![1; server_connection.max_datagram_size().unwrap() + 1];
        let oversized = WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
            terminal_id: "terminal".into(),
            attachment_id: "attachment".into(),
            event: Some(terminal_event::Event::ViewportDatagram(Box::new(
                oversized_viewport,
            ))),
        }));
        let reliable_tail = WireMessage::new(wire_message::Body::AuthResult(AuthResult {
            ok: true,
            message: "after-pmtu-shrink".into(),
            error_code: String::new(),
        }));
        let (mut worker_writer, mut worker_reader) = tokio::io::duplex(16 * 1024);
        let (mut reliable_reader, mut reliable_writer) = tokio::io::duplex(16 * 1024);
        let producer = async {
            write_message(&mut worker_writer, &reliable).await?;
            write_message(&mut worker_writer, &bridged).await?;
            write_message(&mut worker_writer, &oversized).await?;
            write_message(&mut worker_writer, &reliable_tail).await?;
            worker_writer.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let forwarding =
            forward_worker_messages(&mut worker_reader, &mut reliable_writer, &server_connection);
        let (producer_result, forwarding_result) = tokio::join!(producer, forwarding);
        producer_result?;
        forwarding_result?;

        assert_eq!(read_message(&mut reliable_reader).await?, Some(reliable));
        assert_eq!(
            read_message(&mut reliable_reader).await?,
            Some(reliable_tail)
        );
        assert!(read_message(&mut reliable_reader).await?.is_none());
        let datagram =
            tokio::time::timeout(Duration::from_secs(1), client_connection.read_datagram())
                .await
                .context("worker viewport was not forwarded as a QUIC datagram")??;
        assert_eq!(TerminalViewportDatagram::decode(datagram)?, viewport);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), client_connection.read_datagram())
                .await
                .is_err(),
            "oversized worker viewport unexpectedly reached QUIC"
        );

        client_connection.close(0_u32.into(), b"test complete");
        server_connection.close(0_u32.into(), b"test complete");
        Ok(())
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
            ResourcePolicy::default(),
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
