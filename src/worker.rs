use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Weak},
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
        StreamHello, WireMessage, WorkerStreamHello, read_message, terminal_event, wire_message,
        write_message,
    },
    resources::{ResourceAccount, ResourceGovernor, ResourcePolicy, ResourceReservation},
    server::handle_worker_request,
    transport::{FramedRecvStream, FramedSendStream, TransportConnection},
};

pub struct WorkerRouter {
    users_root: PathBuf,
    session_root_override: Option<PathBuf>,
    idle_timeout: Duration,
    start_lock: Mutex<()>,
    discovery_locks: Mutex<HashMap<u32, Weak<Mutex<()>>>>,
    resources: ResourceGovernor,
    resource_policy: ResourcePolicy,
    worker_capacities: Arc<Mutex<HashMap<u32, WorkerCapacity>>>,
}

struct WorkerCapacity {
    _reservation: ResourceReservation,
    identity: Option<crate::worker_identity::WorkerIdentity>,
    peer_pid: Option<i32>,
}

fn validate_directory(path: &Path, uid: u32, forbidden: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == uid
            && metadata.mode() & forbidden == 0,
        "unsafe worker directory {}",
        path.display()
    );
    Ok(())
}

fn process_exists(pid: i32) -> bool {
    // Signal 0 performs an existence/permission check and never sends a signal.
    unsafe {
        nix::libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::ESRCH)
    }
}

pub(crate) struct WorkerProxyStream {
    pub(crate) first_message: WireMessage,
    pub(crate) negotiated: NegotiatedProtocol,
    pub(crate) stream_epoch: u64,
    pub(crate) stream_hello: Option<StreamHello>,
    pub(crate) connection_id: String,
    pub(crate) connection: TransportConnection,
    pub(crate) worker_identity: Arc<Mutex<Option<crate::worker_identity::WorkerIdentity>>>,
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
        validate_directory(state_dir, effective_uid(), 0o022)?;
        match fs::create_dir(&users_root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        validate_directory(&users_root, effective_uid(), 0o022)?;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o711))?;
        fs::set_permissions(&users_root, fs::Permissions::from_mode(0o711))?;
        Ok(Arc::new(Self {
            users_root,
            session_root_override,
            idle_timeout,
            start_lock: Mutex::new(()),
            discovery_locks: Mutex::new(HashMap::new()),
            resources,
            resource_policy,
            worker_capacities: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    pub async fn recover_existing(&self) -> Result<()> {
        for entry in fs::read_dir(&self.users_root)? {
            let entry = entry?;
            let Some(uid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            validate_directory(&entry.path(), uid, 0o077)?;
            if let Some(mut stream) = self.connect_existing(uid).await? {
                let user = nix::unistd::User::from_uid(Uid::from_raw(uid))?
                    .context("active worker refers to an unknown Unix UID")?;
                let account = SystemAccount::lookup(&user.name)?;
                let peer = stream.peer_cred()?;
                let identity = crate::worker_identity::discover(&mut stream, uid).await?;
                if let Some(pid) = peer.pid() {
                    ensure!(
                        pid > 0 && pid as u32 == identity.pid,
                        "worker peer PID mismatch"
                    );
                }
                self.ensure_worker_capacity(&account).await?;
                let mut capacities = self.worker_capacities.lock().await;
                let capacity = capacities
                    .get_mut(&uid)
                    .context("missing worker reservation")?;
                capacity.identity = Some(identity);
                capacity.peer_pid = peer.pid();
            }
        }
        Ok(())
    }

    pub(crate) async fn proxy_stream(
        &self,
        account: &SystemAccount,
        mut client_send: FramedSendStream,
        mut client_recv: FramedRecvStream,
        request: WorkerProxyStream,
    ) -> Result<()> {
        let WorkerProxyStream {
            first_message,
            negotiated,
            stream_epoch,
            stream_hello,
            connection_id,
            connection,
            worker_identity,
        } = request;
        let (worker, identity) = match self.connect(account).await {
            Ok(worker) => worker,
            Err(error) => {
                connection.close(
                    0x105,
                    b"worker unavailable or incompatible; reconnect required",
                );
                return Err(error);
            }
        };
        if let Err(error) =
            crate::worker_identity::pin(&mut *worker_identity.lock().await, identity)
        {
            connection.close(
                0x105,
                b"worker epoch invalidated; reconnect and enumerate sessions",
            );
            return Err(error);
        }
        let is_forward = matches!(&first_message.body,
            Some(wire_message::Body::Request(request))
                if matches!(request.command, Some(crate::protocol::request::Command::OpenTcpForward(_))));
        let (mut worker_recv, mut worker_send) = worker.into_split();
        write_message(
            &mut worker_send,
            &WireMessage::new(wire_message::Body::WorkerStreamHello(WorkerStreamHello {
                protocol_version: negotiated.version,
                capabilities: selections(&negotiated),
                connection_id,
                maximum_datagram_size: worker_datagram_payload_limit(&negotiated, &connection)?,
                stream_epoch,
                stream_hello,
            })),
        )
        .await?;
        write_message(&mut worker_send, &first_message).await?;
        let client_to_worker = relay_worker_input(&mut client_recv, &mut worker_send);
        let worker_to_client = async {
            if is_forward {
                // Forwarding has no datagrams. Preserve backpressure with the small
                // copy buffer instead of allocating terminal frame/queue capacity.
                copy(&mut worker_recv, &mut client_send).await?;
                client_send.shutdown().await?;
                Ok(())
            } else {
                forward_worker_messages(
                    &mut worker_recv,
                    &mut client_send,
                    &connection,
                    negotiated
                        .capabilities
                        .get(CAPABILITY_DATAGRAM_STATE)
                        .copied()
                        .unwrap_or(0),
                    negotiated.has(crate::negotiation::CAPABILITY_SEMANTIC_STATE, 2)
                        && !negotiated.has(crate::negotiation::CAPABILITY_STATE_ACK, 1),
                )
                .await
            }
        };
        tokio::try_join!(client_to_worker, worker_to_client)?;
        Ok(())
    }

    async fn connect(
        &self,
        account: &SystemAccount,
    ) -> Result<(UnixStream, crate::worker_identity::WorkerIdentity)> {
        // Serialize discovery/accounting per UID; failure or cancellation of one
        // probe must not release a concurrent successful probe's reservation.
        let gate = {
            let mut locks = self.discovery_locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(&account.uid).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(account.uid, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        let _discovery = gate.lock().await;
        self.ensure_worker_capacity(account).await?;
        let result = async {
            let mut stream = self.connect_with_capacity(account).await?;
            let peer = stream.peer_cred()?;
            ensure!(peer.uid() == account.uid, "worker peer UID mismatch");
            let identity = crate::worker_identity::discover(&mut stream, account.uid).await?;
            if let Some(pid) = peer.pid() {
                ensure!(
                    pid > 0 && pid as u32 == identity.pid,
                    "worker peer PID mismatch"
                );
            }
            if let Some(capacity) = self.worker_capacities.lock().await.get_mut(&account.uid) {
                capacity.identity = Some(identity);
                capacity.peer_pid = peer.pid();
            }
            Ok((stream, identity))
        }
        .await;
        if result.is_err() {
            let mut capacities = self.worker_capacities.lock().await;
            // A failed probe cannot release the reservation of an established worker.
            if capacities
                .get(&account.uid)
                .is_some_and(|c| c.identity.is_none())
            {
                capacities.remove(&account.uid);
            }
        }
        result
    }

    async fn connect_existing(&self, uid: u32) -> Result<Option<UnixStream>> {
        let socket = self.socket_path(uid);
        match fs::symlink_metadata(&socket) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_socket()
                    && metadata.uid() == uid
                    && metadata.mode() & 0o077 == 0,
                "unsafe worker socket {}",
                socket.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                ensure!(stream.peer_cred()?.uid() == uid, "worker peer UID mismatch");
                Ok(Some(stream))
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn connect_with_capacity(&self, account: &SystemAccount) -> Result<UnixStream> {
        // Validate before even attempting the cached socket fast path.
        let user_state = self.prepare_user_state(account)?;
        if let Some(stream) = self.connect_existing(account.uid).await? {
            return Ok(stream);
        }
        let _guard = self.start_lock.lock().await;
        // Root-owned parent means the user cannot replace this arbitration inode.
        let lock_path = self.users_root.join(format!(".{}.start.lock", account.uid));
        let mut startup = None;
        for _ in 0..100 {
            match ProcessLock::acquire(&lock_path) {
                Ok(lock) => {
                    startup = Some(lock);
                    break;
                }
                Err(error) => {
                    if let Some(stream) = self.connect_existing(account.uid).await? {
                        return Ok(stream);
                    }
                    if !lock_path.exists() {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        let _startup = startup.context("worker startup arbitration timed out")?;
        if let Some(stream) = self.connect_existing(account.uid).await? {
            return Ok(stream);
        }
        self.spawn_worker(account, &user_state, &self.socket_path(account.uid))?;
        for _ in 0..100 {
            if let Some(stream) = self.connect_existing(account.uid).await? {
                return Ok(stream);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bail!("user worker did not create its control socket")
    }

    async fn ensure_worker_capacity(&self, account: &SystemAccount) -> Result<()> {
        let mut capacities = self.worker_capacities.lock().await;
        // Reap only kernel-verified dead PIDs; never infer liveness from a PID file or missing socket.
        capacities.retain(|_, capacity| capacity.peer_pid.is_none_or(process_exists));
        if capacities.contains_key(&account.uid) {
            return Ok(());
        }
        let reservation = self
            .resources
            .account(&account.username)?
            .reserve(self.resource_policy.worker_capacity_claim())?;
        capacities.insert(
            account.uid,
            WorkerCapacity {
                _reservation: reservation,
                identity: None,
                peer_pid: None,
            },
        );
        Ok(())
    }

    fn socket_path(&self, uid: u32) -> PathBuf {
        self.users_root.join(uid.to_string()).join("session.sock")
    }

    fn prepare_user_state(&self, account: &SystemAccount) -> Result<PathBuf> {
        validate_directory(&self.users_root, effective_uid(), 0o022)?;
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
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => info!(%username, "user worker exited"),
                Ok(status) => warn!(%username, %status, "user worker failed"),
                Err(error) => warn!(%username, %error, "failed to wait for user worker"),
            }
            // Capacity is reaped using the established kernel peer PID on the next admission.
            // A losing or delayed child exit must never remove a replacement worker's reservation.
        });
        Ok(())
    }
}

fn worker_datagram_payload_limit(
    negotiated: &NegotiatedProtocol,
    connection: &TransportConnection,
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

async fn relay_worker_input<R, W>(client_recv: &mut R, worker_send: &mut W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    copy(client_recv, worker_send).await?;
    match worker_send.shutdown().await {
        // The worker can reply and close before we observe the client's FIN.
        // Darwin then reports ENOTCONN for shutdown, even with unread response
        // bytes. Do not let try_join cancel the response drain in that case.
        // Actual copy/write failures above still abort both directions.
        Err(error) if error.kind() == std::io::ErrorKind::NotConnected => Ok(()),
        result => Ok(result?),
    }
}

async fn forward_worker_messages<R, W>(
    worker_recv: &mut R,
    client_send: &mut W,
    connection: &TransportConnection,
    datagram_version: u32,
    independent_full_states: bool,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (mut queued, drain) = crate::queued_writer::queued_writer(client_send);
    crate::queued_writer::run_with_drain(
        async move {
            forward_worker_messages_inner(
                worker_recv,
                &mut queued,
                connection,
                datagram_version,
                independent_full_states,
            )
            .await
        },
        drain,
    )
    .await
}

async fn forward_worker_messages_inner<R>(
    worker_recv: &mut R,
    client_send: &mut crate::queued_writer::QueuedWriter,
    connection: &TransportConnection,
    datagram_version: u32,
    independent_full_states: bool,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    while let Some(message) = read_message(worker_recv).await? {
        let datagram = match &message.body {
            Some(wire_message::Body::TerminalEvent(event)) => match &event.event {
                Some(terminal_event::Event::ViewportDatagram(datagram)) => {
                    ensure!(
                        !event.terminal_id.is_empty()
                            && !event.attachment_id.is_empty()
                            && event.terminal_id == datagram.terminal_id
                            && event.attachment_id == datagram.attachment_id,
                        "worker emitted an inconsistent terminal datagram route"
                    );
                    validate_worker_datagram(datagram, datagram_version)?;
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
            connection.send_datagram(encoded.into())?;
        } else {
            write_message(client_send, &message).await?;
            // Propagate reliable backpressure to the worker socket. The client
            // input relay is an independent future and continues while this waits.
            if independent_full_states {
                client_send.wait_until_drained().await;
            }
        }
    }
    client_send.shutdown().await?;
    Ok(())
}

// Validate the opaque worker envelope; terminal state remains solely in the worker.
fn validate_worker_datagram(
    packet: &crate::protocol::TerminalViewportDatagram,
    version: u32,
) -> Result<()> {
    ensure!(
        (1..=3).contains(&version),
        "worker datagram was not negotiated"
    );
    if let Some(fragment) = &packet.fragment {
        ensure!(
            version == 3
                && packet.diff.is_none()
                && packet.inherited_fields == 0
                && !packet.sparse_rows
                && packet.primary_patches.is_empty()
                && packet.alternate_patches.is_empty(),
            "invalid or unnegotiated worker fragment"
        );
        let total = u64::from(fragment.total_encoded);
        let raw = u64::from(fragment.raw_length);
        let count = u64::from(fragment.count);
        let index = u64::from(fragment.index);
        ensure!(
            fragment.epoch.len() == 16
                && fragment.transfer_id.len() == 16
                && fragment.sha256.len() == 32
                && fragment.base_generation > 0
                && fragment.target_generation > fragment.base_generation
                && count > 0
                && count <= 128
                && index < count
                && total >= count
                && raw > 0
                && raw + 2 * total < u64::from(u16::MAX)
                && matches!(fragment.encoding, 0 | 1),
            "invalid worker fragment bounds"
        );
        ensure!(
            fragment.data.len() as u64 == (index + 1) * total / count - index * total / count,
            "invalid worker fragment interval"
        );
        ensure!(
            fragment.encoding != 0 || raw == total,
            "invalid raw worker fragment length"
        );
    } else {
        ensure!(
            version < 3 && packet.diff.is_some(),
            "worker viewport format differs from negotiated version"
        );
    }
    Ok(())
}

fn append_worker_resource_policy(command: &mut Command, policy: &ResourcePolicy) {
    const MIB: u64 = 1024 * 1024;
    command
        .arg("--max-user-connections")
        .arg(policy.user.connections.to_string())
        .arg("--max-user-forward-connections")
        .arg(policy.user.forward_connections.to_string())
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
    // This entry point runs only in the dedicated worker process, before services start.
    unsafe {
        nix::libc::umask(0o077);
    }
    let actual_uid = effective_uid();
    if actual_uid != expected_uid {
        bail!(
            "worker credential mismatch: expected UID {expected_uid}, running as UID {actual_uid}"
        )
    }
    let _worker_lock = ProcessLock::acquire(&state_dir.join("worker.lock"))?;
    let identity = crate::worker_identity::WorkerIdentity::new(expected_uid);
    let pid_file = state_dir.join("worker.pid");
    fs::write(&pid_file, format!("{}\n", std::process::id()))?;
    fs::set_permissions(&pid_file, fs::Permissions::from_mode(0o600))?;
    resource_policy.validate()?;
    let resources =
        ResourceAccount::standalone(format!("worker UID {expected_uid}"), resource_policy.user)?;
    let maximum_requests = usize::try_from(resource_policy.user.streams)?;
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
                let (mut stream, _) = accepted?;
                if requests.len() >= maximum_requests { continue; }
                let peer_uid = stream.peer_cred()?.uid();
                if peer_uid != 0 && peer_uid != expected_uid { continue; }
                idle_state.mark_active();
                let manager = manager.clone();
                let files = files.clone();
                requests.spawn(async move {
                    crate::worker_identity::introduce(&mut stream, identity).await?;
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
    async fn peer_close_after_reply_does_not_cancel_reply_drain() -> Result<()> {
        use tokio::io::AsyncReadExt;
        let (gateway, mut worker) = UnixStream::pair()?;
        worker.write_all(b"complete worker response").await?;
        drop(worker); // Worker replied and closed before the gateway handles client FIN.
        let (mut recv, mut send) = gateway.into_split();
        let mut client_fin = tokio::io::empty();
        let response = async {
            // Deliberately leave reply bytes unread until the FIN side is polled.
            tokio::task::yield_now().await;
            let mut data = Vec::new();
            recv.read_to_end(&mut data).await?;
            Ok::<_, anyhow::Error>(data)
        };
        let (_, response) =
            tokio::try_join!(relay_worker_input(&mut client_fin, &mut send), response)?;
        assert_eq!(response, b"complete worker response");
        Ok(())
    }

    #[tokio::test]
    async fn closed_worker_still_rejects_unsent_request_bytes() -> Result<()> {
        let (mut gateway, worker) = UnixStream::pair()?;
        drop(worker);
        let mut input = b"unsent request bytes".as_slice();
        assert!(relay_worker_input(&mut input, &mut gateway).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn discovery_rejects_unsafe_paths_before_reuse() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir()?;
        let state = temporary.path().join("state");
        fs::create_dir(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        let unsafe_root = temporary.path().join("elsewhere");
        fs::create_dir(&unsafe_root)?;
        symlink(&unsafe_root, state.join("users"))?;
        let policy = ResourcePolicy::default();
        assert!(
            WorkerRouter::new(
                &state,
                None,
                Duration::from_secs(60),
                ResourceGovernor::new(&policy)?,
                policy.clone()
            )
            .is_err()
        );
        fs::remove_file(state.join("users"))?;
        fs::create_dir(state.join("users"))?;
        fs::set_permissions(state.join("users"), fs::Permissions::from_mode(0o700))?;
        let router = WorkerRouter::new(
            &state,
            None,
            Duration::from_secs(60),
            ResourceGovernor::new(&policy)?,
            policy,
        )?;
        let account = SystemAccount::current()?;
        let user_state = router.prepare_user_state(&account)?;
        fs::set_permissions(&user_state, fs::Permissions::from_mode(0o777))?;
        assert!(router.connect_with_capacity(&account).await.is_err());
        fs::set_permissions(&user_state, fs::Permissions::from_mode(0o700))?;
        let socket = router.socket_path(account.uid);
        fs::write(&socket, "not a socket")?;
        assert!(router.connect_existing(account.uid).await.is_err());
        fs::remove_file(&socket)?;
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666))?;
        assert!(router.connect_existing(account.uid).await.is_err());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        assert!(router.connect_existing(account.uid).await?.is_some());
        drop(listener);
        fs::remove_file(&socket)?;
        symlink(temporary.path().join("absent"), &socket)?;
        assert!(router.connect_existing(account.uid).await.is_err());
        Ok(())
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
        let server_connection =
            crate::transport::QuinnTransport::new(server_connection).into_connection();

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
            stream_epoch: 0,
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
            ..Default::default()
        };
        let bridged = WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
            terminal_id: "terminal".into(),
            attachment_id: "attachment".into(),
            event: Some(terminal_event::Event::ViewportDatagram(Box::new(
                viewport.clone(),
            ))),
        }));
        let mut dense = viewport.clone();
        dense.diff.as_mut().unwrap().target_metadata = Some(crate::terminal_state_v2::State {
            title: (0u64..500)
                .map(|i| format!("{:016x}", i.wrapping_mul(6364136223846793005)))
                .collect(),
            ..Default::default()
        });
        let transfer =
            crate::terminal_datagram_v3::ViewportTransfer::prepare(dense, 256, usize::MAX)?
                .unwrap();
        assert!(transfer.count() > 1);
        let fragments: Vec<_> = (0..transfer.count()).map(|i| transfer.packet(i)).collect();
        for packet in &fragments {
            validate_worker_datagram(packet, 3)?;
            assert!(validate_worker_datagram(packet, 2).is_err());
        }
        for malformed in 0..6 {
            let mut packet = fragments[0].clone();
            let fragment = packet.fragment.as_mut().unwrap();
            match malformed {
                0 => fragment.count = 129,
                1 => fragment.index = fragment.count,
                2 => fragment.raw_length = 65535,
                3 => fragment.data.push(0),
                4 => fragment.encoding = 42,
                _ => packet.diff = viewport.diff.clone(),
            }
            assert!(validate_worker_datagram(&packet, 3).is_err());
        }
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
            stream_epoch: 0,
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
        let forwarding = forward_worker_messages(
            &mut worker_reader,
            &mut reliable_writer,
            &server_connection,
            2,
            false,
        );
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
        assert!(validate_worker_datagram(&viewport, 3).is_err());
        let (mut worker_writer, mut worker_reader) = tokio::io::duplex(16 * 1024);
        let (mut reliable_reader, mut reliable_writer) = tokio::io::duplex(16 * 1024);
        let producer = async {
            for packet in &fragments {
                let event = WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
                    terminal_id: packet.terminal_id.clone(),
                    attachment_id: packet.attachment_id.clone(),
                    event: Some(terminal_event::Event::ViewportDatagram(Box::new(
                        packet.clone(),
                    ))),
                }));
                write_message(&mut worker_writer, &event).await?;
            }
            worker_writer.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let (written, forwarded) = tokio::join!(
            producer,
            forward_worker_messages(
                &mut worker_reader,
                &mut reliable_writer,
                &server_connection,
                3,
                false
            )
        );
        written?;
        forwarded?;
        assert!(read_message(&mut reliable_reader).await?.is_none());
        for expected in &fragments {
            let packet =
                tokio::time::timeout(Duration::from_secs(1), client_connection.read_datagram())
                    .await??;
            assert_eq!(&TerminalViewportDatagram::decode(packet)?, expected);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), client_connection.read_datagram())
                .await
                .is_err(),
            "oversized worker viewport unexpectedly reached QUIC"
        );

        // Exercise the managed no-ACK branch with more than the entire output
        // quota over time, a blocked/slow sink, and concurrent reverse input.
        let (mut source, mut worker_output) = tokio::io::duplex(1024);
        let (mut destination, mut client_output) = tokio::io::duplex(1024);
        let (mut client_input, mut input_reader) = tokio::io::duplex(16);
        let (mut input_writer, mut worker_input) = tokio::io::duplex(16);
        let (input_seen, wait_input) = tokio::sync::oneshot::channel();
        let produce = async {
            for index in 0..96 {
                let frame = WireMessage::new(wire_message::Body::AuthResult(AuthResult {
                    ok: true,
                    message: format!("{index:03}{}", "x".repeat(256 * 1024)),
                    ..Default::default()
                }));
                write_message(&mut source, &frame).await?;
            }
            source.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let consume = async {
            wait_input.await?; // The sink stays blocked until reverse input succeeds.
            tokio::time::sleep(Duration::from_millis(50)).await;
            for index in 0..96 {
                let frame = read_message(&mut destination)
                    .await?
                    .context("truncated frame")?;
                let Some(wire_message::Body::AuthResult(frame)) = frame.body else {
                    bail!("wrong frame")
                };
                assert_eq!(
                    frame.message,
                    format!("{index:03}{}", "x".repeat(256 * 1024))
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(read_message(&mut destination).await?.is_none());
            Ok::<(), anyhow::Error>(())
        };
        let inputs = async {
            use tokio::io::AsyncReadExt;
            client_input.write_all(b"input").await?;
            client_input.shutdown().await?;
            let mut bytes = [0; 5];
            worker_input.read_exact(&mut bytes).await?;
            assert_eq!(&bytes, b"input");
            input_seen.send(()).unwrap();
            Ok::<(), anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(
                produce,
                consume,
                inputs,
                relay_worker_input(&mut input_reader, &mut input_writer),
                forward_worker_messages(
                    &mut worker_output,
                    &mut client_output,
                    &server_connection,
                    0,
                    true
                )
            )
        })
        .await
        .context("managed no-ACK backpressure timed out")??;

        client_connection.close(0_u32.into(), b"test complete");
        server_connection.close(0, b"test complete");
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
