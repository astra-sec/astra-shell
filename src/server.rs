use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use prost::Message;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{error, info, trace, warn};

use crate::{
    ALPN,
    accounts::{SystemAccount, authorized_key_files, effective_uid},
    auth::{authentication_payload, verify_authorized_key, verify_authorized_keys},
    files::{FileResult, FileService},
    negotiation::{
        CAPABILITY_CLIPBOARD_WRITE, CAPABILITY_HISTORY_PAGING, CAPABILITY_INPUT_LEASE,
        CAPABILITY_SEMANTIC_STATE, CAPABILITY_SESSION_OBJECTS, NegotiatedProtocol, ProtocolSupport,
        negotiate_client_hello, selections, validate_worker_selection,
    },
    process_lock::ProcessLock,
    protocol::{
        AckResponse, AttachResponse, AttachmentListResponse, AttachmentRole, AuthResult,
        ClipboardSelection as WireClipboardSelection, ClipboardWrite, ErrorResponse,
        HistoryPageChunk, LeaseChanged, LeaseControlAction, ListResponse, Response, ServerHello,
        SpawnResponse, TerminalEvent, TerminalListResponse, TerminalStateChunk, WireMessage,
        WorkerStreamHello, WorkspaceListResponse, read_message, request, response,
        terminal_command, terminal_event, wire_message, write_message,
    },
    resources::{
        QuotaExceeded, ResourceAccount, ResourceClaim, ResourceGovernor, ResourcePolicy,
        ResourceReservation,
    },
    session::SessionManager,
    terminal::{ClipboardSelection, INPUT_LEASE_TTL, LeaseRevocationReason, PtyEvent, Terminal},
    terminal_state_v2::{
        HistoryPage, MAX_ENCODED_HISTORY_PAGE_BYTES, MAX_ENCODED_STATE_BYTES, State,
    },
    worker::WorkerRouter,
};

#[derive(Clone, Debug)]
pub struct ServerPaths {
    pub state_dir: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub authorized_keys: PathBuf,
    pub instance_id: PathBuf,
}

impl ServerPaths {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            cert: state_dir.join("host-cert.der"),
            key: state_dir.join("host-key.der"),
            authorized_keys: state_dir.join("authorized_keys"),
            instance_id: state_dir.join("instance-id"),
            state_dir,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ServerMode {
    Rootless {
        session_root: PathBuf,
    },
    Managed {
        authorized_keys_directory: Option<PathBuf>,
        session_root_override: Option<PathBuf>,
        worker_idle_timeout: std::time::Duration,
    },
}

#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub paths: ServerPaths,
    pub mode: ServerMode,
    pub resource_policy: ResourcePolicy,
}

#[derive(Clone)]
struct ServerState {
    mode: ModeState,
    instance_id: String,
    resources: ResourceGovernor,
}

#[derive(Clone)]
enum ModeState {
    Rootless {
        account: SystemAccount,
        manager: SessionManager,
        files: FileService,
        authorized_keys: PathBuf,
    },
    Managed {
        router: Arc<WorkerRouter>,
        authorized_keys_directory: Option<PathBuf>,
    },
}

#[derive(Clone)]
enum ConnectionBackend {
    Local {
        manager: SessionManager,
        files: FileService,
    },
    Worker {
        router: Arc<WorkerRouter>,
        account: SystemAccount,
    },
}

const TERMINAL_STATE_CHUNK_BYTES: usize = 512 * 1024;

pub fn initialize_state(paths: &ServerPaths) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("failed to create {}", paths.state_dir.display()))?;
    secure_state_directory(&paths.state_dir)?;

    match (paths.cert.exists(), paths.key.exists()) {
        (false, false) => {
            let rcgen::CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["localhost".into(), "astra.local".into()])?;
            write_new_file(&paths.cert, cert.der(), 0o644)?;
            write_new_file(&paths.key, &signing_key.serialize_der(), 0o600)?;
        }
        (true, true) => {}
        _ => bail!(
            "incomplete host identity in {}: certificate and private key must both exist or both be absent",
            paths.state_dir.display()
        ),
    }
    if !paths.authorized_keys.exists() {
        write_new_file(
            &paths.authorized_keys,
            b"# Rootless mode only. Managed mode reads each Unix user's authorized_keys.\n",
            0o600,
        )?;
    }
    if !paths.instance_id.exists() {
        write_new_file(
            &paths.instance_id,
            format!("{}\n", uuid::Uuid::new_v4()).as_bytes(),
            0o644,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn secure_state_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("state path {} must be a real directory", path.display())
    }
    let expected_uid = effective_uid();
    if metadata.uid() != expected_uid {
        bail!(
            "state directory {} is owned by UID {}, expected daemon UID {}",
            path.display(),
            metadata.uid(),
            expected_uid
        )
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_state_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn write_new_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW);
    std::io::Write::write_all(&mut options.open(path)?, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_new_file(path: &Path, contents: &[u8], _mode: u32) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    std::io::Write::write_all(&mut options.open(path)?, contents)?;
    Ok(())
}

pub async fn serve(options: ServerOptions) -> Result<()> {
    options.resource_policy.validate()?;
    ensure_initialized(&options.paths)?;
    if matches!(&options.mode, ServerMode::Managed { .. }) {
        validate_managed_gateway_state(&options.paths)?;
    }
    let _daemon_lock = ProcessLock::acquire(&options.paths.state_dir.join("gateway.lock"))?;
    let resources = ResourceGovernor::new(&options.resource_policy)?;
    let mode = match options.mode {
        ServerMode::Rootless { session_root } => {
            let account = SystemAccount::current()?;
            let resource_account = resources.account(&account.username)?;
            let files =
                FileService::with_resources(session_root.clone(), resource_account.clone())?;
            ModeState::Rootless {
                account,
                manager: SessionManager::with_resources(
                    session_root,
                    options.paths.state_dir.join("session-catalog.pb"),
                    resource_account,
                    options.resource_policy.clone(),
                )?,
                files,
                authorized_keys: options.paths.authorized_keys.clone(),
            }
        }
        ServerMode::Managed {
            authorized_keys_directory,
            session_root_override,
            worker_idle_timeout,
        } => ModeState::Managed {
            router: WorkerRouter::new(
                &options.paths.state_dir,
                session_root_override,
                worker_idle_timeout,
                resources.clone(),
                options.resource_policy.clone(),
            )?,
            authorized_keys_directory,
        },
    };
    let instance_id = fs::read_to_string(&options.paths.instance_id)?
        .trim()
        .to_owned();
    let state = ServerState {
        mode,
        instance_id,
        resources,
    };

    let certificate = CertificateDer::from(fs::read(&options.paths.cert)?);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(fs::read(&options.paths.key)?));
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    let transport = Arc::get_mut(&mut server_config.transport)
        .context("server transport configuration is unexpectedly shared")?;
    transport.max_concurrent_bidi_streams(128_u32.into());
    transport.max_concurrent_uni_streams(0_u8.into());
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(15)
            .try_into()
            .expect("15 second QUIC idle timeout is valid"),
    ));

    let endpoint = quinn::Endpoint::server(server_config, options.listen)?;
    info!(listen = %endpoint.local_addr()?, "astrad listening");
    println!("LISTEN {}", endpoint.local_addr()?);

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(state, incoming).await {
                warn!(%error, "connection ended with an error");
            }
        });
    }
    Ok(())
}

fn ensure_initialized(paths: &ServerPaths) -> Result<()> {
    for path in [&paths.cert, &paths.key, &paths.instance_id] {
        if !path.exists() {
            bail!(
                "server state is incomplete (missing {}); run `astrad init --state-dir {}`",
                path.display(),
                paths.state_dir.display()
            )
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_managed_gateway_state(paths: &ServerPaths) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let expected_uid = effective_uid();
    let state = fs::symlink_metadata(&paths.state_dir)
        .with_context(|| format!("failed to inspect {}", paths.state_dir.display()))?;
    if state.file_type().is_symlink() || !state.is_dir() {
        bail!(
            "managed gateway state {} must be a real directory",
            paths.state_dir.display()
        )
    }
    if state.uid() != expected_uid {
        bail!(
            "managed gateway state {} is owned by UID {}, expected daemon UID {}",
            paths.state_dir.display(),
            state.uid(),
            expected_uid
        )
    }
    if state.mode() & 0o022 != 0 {
        bail!(
            "managed gateway state {} must not be writable by group or other users",
            paths.state_dir.display()
        )
    }

    let key = fs::symlink_metadata(&paths.key)
        .with_context(|| format!("failed to inspect {}", paths.key.display()))?;
    if key.file_type().is_symlink() || !key.is_file() {
        bail!(
            "managed gateway host key {} must be a regular file",
            paths.key.display()
        )
    }
    if key.uid() != expected_uid {
        bail!(
            "managed gateway host key {} is owned by UID {}, expected daemon UID {}",
            paths.key.display(),
            key.uid(),
            expected_uid
        )
    }
    if key.mode() & 0o077 != 0 {
        bail!(
            "managed gateway host key {} must not be accessible by group or other users",
            paths.key.display()
        )
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_managed_gateway_state(_paths: &ServerPaths) -> Result<()> {
    bail!("managed mode requires Unix accounts and process credentials")
}

async fn handle_connection(state: ServerState, incoming: quinn::Incoming) -> Result<()> {
    let connection = incoming.await.context("QUIC handshake failed")?;
    let remote = connection.remote_address();
    let (username, fingerprint, backend, negotiated, connection_resources, _connection_reservation) =
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            authenticate_connection(&state, &connection),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                connection.close(0x101_u32.into(), b"authentication timeout");
                bail!("client authentication timed out")
            }
        };
    info!(
        %remote,
        %username,
        %fingerprint,
        protocol_version = negotiated.version,
        capabilities = negotiated.capabilities.len(),
        "client authenticated"
    );
    let connection_id = uuid::Uuid::new_v4().to_string();

    loop {
        let (mut send, mut recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let backend = backend.clone();
        let negotiated = negotiated.clone();
        let connection_id = connection_id.clone();
        let connection_resources = connection_resources.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                let first_message = read_message(&mut recv)
                    .await?
                    .context("request stream ended before its first message")?;
                let _gateway_stream_reservation =
                    if matches!(&backend, ConnectionBackend::Worker { .. }) {
                        match connection_resources.reserve(ResourceClaim::stream()) {
                            Ok(reservation) => Some(reservation),
                            Err(error) => {
                                reject_request_for_quota(&mut send, &first_message, error).await?;
                                return Ok(());
                            }
                        }
                    } else {
                        None
                    };
                if is_file_request(&first_message) {
                    send.set_priority(-10)?;
                }
                match backend {
                    ConnectionBackend::Local { manager, files } => {
                        handle_worker_message(
                            manager,
                            files,
                            send,
                            recv,
                            first_message,
                            negotiated,
                            connection_id,
                        )
                        .await
                    }
                    ConnectionBackend::Worker { router, account } => {
                        router
                            .proxy_stream(
                                &account,
                                send,
                                recv,
                                first_message,
                                negotiated,
                                connection_id,
                            )
                            .await
                    }
                }
            }
            .await;
            if let Err(error) = result {
                warn!(error = %format!("{error:#}"), "request stream failed");
            }
        });
    }
}

async fn authenticate_connection(
    state: &ServerState,
    connection: &quinn::Connection,
) -> Result<(
    String,
    String,
    ConnectionBackend,
    NegotiatedProtocol,
    ResourceAccount,
    ResourceReservation,
)> {
    let (mut auth_send, mut auth_recv, first_message) =
        accept_authentication_stream(connection).await?;
    let hello = match first_message {
        WireMessage {
            body: Some(wire_message::Body::ClientHello(hello)),
        } => hello,
        _ => bail!("expected ClientHello"),
    };
    if hello.username.is_empty() {
        bail!("ClientHello has no target Unix username")
    }
    let negotiated = negotiate_client_hello(&hello, &ProtocolSupport::runtime())?;
    let username = hello.username;

    let challenge: [u8; 32] = rand::random();
    write_message(
        &mut auth_send,
        &WireMessage::new(wire_message::Body::ServerHello(ServerHello {
            protocol_version: negotiated.version,
            challenge: challenge.to_vec(),
            server_instance: state.instance_id.clone(),
            capabilities: selections(&negotiated),
        })),
    )
    .await?;
    let payload = authentication_payload(&challenge, &username, &state.instance_id);
    let auth_request = match read_message(&mut auth_recv).await? {
        Some(WireMessage {
            body: Some(wire_message::Body::AuthRequest(request)),
        }) => request,
        _ => return Err(anyhow!("expected AuthRequest")),
    };
    let authentication = authenticate_user(
        &state.mode,
        &username,
        &auth_request.public_key,
        &auth_request.signature_pem,
        &payload,
    );

    let (fingerprint, backend) = match authentication {
        Ok(authenticated) => authenticated,
        Err(error) => {
            let _ = write_message(
                &mut auth_send,
                &WireMessage::new(wire_message::Body::AuthResult(AuthResult {
                    ok: false,
                    message: "public key authentication failed".into(),
                    error_code: String::new(),
                })),
            )
            .await;
            let _ = auth_send.finish();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), auth_send.stopped()).await;
            connection.close(0x100_u32.into(), b"authentication failed");
            return Err(error.context("client authentication failed"));
        }
    };

    // Reserve the connection before acknowledging authentication. A client that receives a
    // successful AuthResult is allowed to start application streams immediately; rejecting the
    // quota afterwards turns a permanent condition into an opaque transport reset and causes an
    // automatic reconnect loop.
    let connection_resources = state.resources.account(&username)?;
    let connection_reservation = match connection_resources.reserve(ResourceClaim::connection()) {
        Ok(reservation) => reservation,
        Err(error) => {
            write_message(
                &mut auth_send,
                &WireMessage::new(wire_message::Body::AuthResult(
                    connection_quota_auth_result(&error),
                )),
            )
            .await?;
            auth_send.finish()?;
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), auth_send.stopped()).await;
            connection.close(0x102_u32.into(), b"connection quota exceeded");
            return Err(error.into());
        }
    };
    write_message(
        &mut auth_send,
        &WireMessage::new(wire_message::Body::AuthResult(AuthResult {
            ok: true,
            message: fingerprint.clone(),
            error_code: String::new(),
        })),
    )
    .await?;
    auth_send.finish()?;
    Ok((
        username,
        fingerprint,
        backend,
        negotiated,
        connection_resources,
        connection_reservation,
    ))
}

fn connection_quota_auth_result(error: &QuotaExceeded) -> AuthResult {
    AuthResult {
        ok: false,
        message: error.to_string(),
        error_code: "connection_quota_exceeded".into(),
    }
}

async fn accept_authentication_stream(
    connection: &quinn::Connection,
) -> Result<(quinn::SendStream, quinn::RecvStream, WireMessage)> {
    // Apple's NWConnectionGroup reserves an empty client-initiated bidi stream
    // for the group data flow, so the first application stream can have index
    // 1. Read a small bounded set concurrently and use the first stream that
    // actually sends a protocol frame. The outer authentication timeout still
    // bounds the lifetime of this work.
    const MAX_PENDING_STREAMS: usize = 8;
    let mut pending = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = connection.accept_bi(), if pending.len() < MAX_PENDING_STREAMS => {
                let (send, mut recv) = accepted
                    .context("client did not open authentication stream")?;
                trace!(stream_id = %recv.id(), "accepted authentication candidate stream");
                pending.spawn(async move {
                    let message = read_message(&mut recv).await;
                    (send, recv, message)
                });
            }
            completed = pending.join_next(), if !pending.is_empty() => {
                let (send, recv, message) = completed
                    .context("authentication stream reader set ended unexpectedly")?
                    .context("authentication stream reader task failed")?;
                match message? {
                    Some(message) => {
                        trace!(stream_id = %recv.id(), "received first authentication stream frame");
                        // Keep unread reserved streams alive for the connection;
                        // dropping them here would reset a Network.framework
                        // group flow that the Apple client still owns.
                        pending.detach_all();
                        return Ok((send, recv, message));
                    }
                    None => continue,
                }
            }
        }
    }
}

fn authenticate_user(
    mode: &ModeState,
    username: &str,
    public_key: &str,
    signature_pem: &str,
    payload: &[u8],
) -> Result<(String, ConnectionBackend)> {
    match mode {
        ModeState::Rootless {
            account,
            manager,
            files,
            authorized_keys,
        } => {
            if username != account.username {
                bail!(
                    "rootless daemon belongs to Unix user {}, not {}",
                    account.username,
                    username
                )
            }
            let fingerprint =
                verify_authorized_key(authorized_keys, public_key, signature_pem, payload)?;
            Ok((
                fingerprint,
                ConnectionBackend::Local {
                    manager: manager.clone(),
                    files: files.clone(),
                },
            ))
        }
        ModeState::Managed {
            router,
            authorized_keys_directory,
        } => {
            let account = SystemAccount::lookup(username)?;
            if effective_uid() != 0 && effective_uid() != account.uid {
                bail!("non-root managed daemon cannot authenticate a different Unix UID")
            }
            let files = authorized_key_files(&account, authorized_keys_directory.as_deref())?;
            let fingerprint = verify_authorized_keys(&files, public_key, signature_pem, payload)?;
            Ok((
                fingerprint,
                ConnectionBackend::Worker {
                    router: router.clone(),
                    account,
                },
            ))
        }
    }
}

pub(crate) async fn handle_worker_request<W, R>(
    manager: SessionManager,
    files: FileService,
    send: W,
    mut recv: R,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let hello = read_message(&mut recv)
        .await?
        .context("request stream ended before its first message")?;
    let (negotiated, connection_id) = match hello {
        WireMessage {
            body:
                Some(wire_message::Body::WorkerStreamHello(WorkerStreamHello {
                    protocol_version,
                    capabilities,
                    connection_id,
                })),
        } => (
            validate_worker_selection(
                protocol_version,
                &capabilities,
                &ProtocolSupport::runtime(),
            )?,
            connection_id,
        ),
        _ => bail!("expected WorkerStreamHello as first worker stream message"),
    };
    let parsed_connection_id =
        uuid::Uuid::parse_str(&connection_id).context("worker connection ID is not a UUID")?;
    ensure!(
        parsed_connection_id.to_string() == connection_id,
        "worker connection ID is not canonical"
    );
    let request = read_message(&mut recv)
        .await?
        .context("worker stream ended before its request")?;
    handle_worker_message(
        manager,
        files,
        send,
        recv,
        request,
        negotiated,
        connection_id,
    )
    .await
}

async fn handle_worker_message<W, R>(
    manager: SessionManager,
    files: FileService,
    mut send: W,
    recv: R,
    first_message: WireMessage,
    negotiated: NegotiatedProtocol,
    connection_id: String,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let file_request = is_file_request(&first_message);
    let request = match first_message {
        WireMessage {
            body: Some(wire_message::Body::Request(request)),
        } => request,
        _ => bail!("expected Request as first stream message"),
    };
    let request_id = request.request_id.clone();
    let resource_account = manager.resource_account();
    let _stream_resources = match resource_account.reserve(ResourceClaim::stream()) {
        Ok(reservation) => reservation,
        Err(error) => {
            send_error(&mut send, request_id, "quota", error.into()).await?;
            send.shutdown().await?;
            return Ok(());
        }
    };
    let _file_resources = if file_request {
        match resource_account.reserve(ResourceClaim::file_handle()) {
            Ok(reservation) => Some(reservation),
            Err(error) => {
                send_error(&mut send, request_id, "quota", error.into()).await?;
                send.shutdown().await?;
                return Ok(());
            }
        }
    } else {
        None
    };
    let session_objects = negotiated.has(CAPABILITY_SESSION_OBJECTS, 1);
    match request.command {
        Some(request::Command::List(_)) => {
            let terminals = manager.list_legacy_terminals();
            send_response(
                &mut send,
                request_id,
                response::Result::List(ListResponse { terminals }),
            )
            .await?;
        }
        Some(request::Command::Spawn(spawn)) => match manager.spawn(spawn, session_objects) {
            Ok(terminal) => {
                send_response(
                    &mut send,
                    request_id,
                    response::Result::Spawn(SpawnResponse {
                        terminal: Some(terminal.info()),
                    }),
                )
                .await?;
            }
            Err(error) => send_domain_error(&mut send, request_id, "spawn", error).await?,
        },
        Some(request::Command::Close(close)) => {
            match manager.get_terminal(&close.workspace_id, &close.terminal_id, session_objects) {
                Ok(Some(terminal)) => match terminal.kill() {
                    Ok(()) => {
                        send_response(
                            &mut send,
                            request_id,
                            response::Result::Ack(AckResponse {
                                message: "terminal process signalled".into(),
                            }),
                        )
                        .await?;
                    }
                    Err(error) => send_error(&mut send, request_id, "terminal", error).await?,
                },
                Ok(None) => {
                    send_error(
                        &mut send,
                        request_id,
                        "not_found",
                        anyhow!("terminal is not active"),
                    )
                    .await?
                }
                Err(error) => send_error(&mut send, request_id, "workspace", error).await?,
            }
        }
        Some(request::Command::RenameTerminal(rename)) => {
            match manager.get_terminal(&rename.workspace_id, &rename.terminal_id, session_objects) {
                Ok(Some(terminal)) => {
                    terminal.rename(rename.name);
                    send_response(
                        &mut send,
                        request_id,
                        response::Result::Ack(AckResponse {
                            message: "terminal renamed".into(),
                        }),
                    )
                    .await?;
                }
                Ok(None) => {
                    send_error(
                        &mut send,
                        request_id,
                        "not_found",
                        anyhow!("terminal is not active"),
                    )
                    .await?
                }
                Err(error) => send_error(&mut send, request_id, "workspace", error).await?,
            }
        }
        Some(request::Command::Attach(attach)) => {
            let terminal = match manager.get_terminal(
                &attach.workspace_id,
                &attach.terminal_id,
                session_objects,
            ) {
                Ok(Some(terminal)) => terminal,
                Ok(None) => {
                    send_error(
                        &mut send,
                        request_id,
                        "not_found",
                        anyhow!("terminal is not active in this daemon"),
                    )
                    .await?;
                    send.shutdown().await?;
                    return Ok(());
                }
                Err(error) => {
                    send_error(&mut send, request_id, "workspace", error).await?;
                    send.shutdown().await?;
                    return Ok(());
                }
            };
            handle_attach(
                manager,
                terminal,
                request_id,
                attach.read_only,
                attach.takeover,
                attach.resume_token,
                send,
                recv,
                negotiated,
                connection_id,
            )
            .await?;
            return Ok(());
        }
        Some(request::Command::FileCapabilities(_)) => {
            send_response(
                &mut send,
                request_id,
                response::Result::FileCapabilities(files.capabilities()),
            )
            .await?;
        }
        Some(request::Command::FileStat(stat)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.stat(stat)).await?;
            send_file_result(&mut send, request_id, result, response::Result::FileStat).await?;
        }
        Some(request::Command::FileList(list)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.list(list)).await?;
            send_file_result(&mut send, request_id, result, response::Result::FileList).await?;
        }
        Some(request::Command::GitStatus(status)) => {
            let result = files.git_status(status).await;
            send_file_result(&mut send, request_id, result, response::Result::GitStatus).await?;
        }
        Some(request::Command::WatchFiles(watch)) => {
            let mut subscription = match files.watch_files(watch) {
                Ok(subscription) => subscription,
                Err(error) => {
                    send_file_result(
                        &mut send,
                        request_id,
                        Err(error),
                        response::Result::FileChanges,
                    )
                    .await?;
                    send.shutdown().await?;
                    return Ok(());
                }
            };
            loop {
                let changes = subscription.next().await;
                let failed = changes.is_err();
                send_file_result(
                    &mut send,
                    request_id.clone(),
                    changes,
                    response::Result::FileChanges,
                )
                .await?;
                if failed {
                    break;
                }
            }
        }
        Some(request::Command::BeginUpload(begin)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.begin_upload(begin)).await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::UploadStatus,
            )
            .await?;
        }
        Some(request::Command::WriteFileChunk(chunk)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.write_chunk(chunk)).await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::UploadStatus,
            )
            .await?;
        }
        Some(request::Command::QueryUpload(query)) => {
            let service = files.clone();
            let result =
                tokio::task::spawn_blocking(move || service.query_upload(&query.transfer_id))
                    .await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::UploadStatus,
            )
            .await?;
        }
        Some(request::Command::CommitUpload(commit)) => {
            let service = files.clone();
            let result =
                tokio::task::spawn_blocking(move || service.commit_upload(&commit.transfer_id))
                    .await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::UploadStatus,
            )
            .await?;
        }
        Some(request::Command::AbortUpload(abort)) => {
            let service = files.clone();
            let result =
                tokio::task::spawn_blocking(move || service.abort_upload(&abort.transfer_id))
                    .await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::UploadStatus,
            )
            .await?;
        }
        Some(request::Command::BeginDownload(begin)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.begin_download(begin)).await?;
            send_file_result(
                &mut send,
                request_id,
                result,
                response::Result::BeginDownload,
            )
            .await?;
        }
        Some(request::Command::ReadFileChunk(chunk)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.read_chunk(chunk)).await?;
            send_file_result(&mut send, request_id, result, response::Result::FileChunk).await?;
        }
        Some(request::Command::MakeDirectory(directory)) => {
            let service = files.clone();
            let result =
                tokio::task::spawn_blocking(move || service.make_directory(&directory.path))
                    .await?;
            send_file_ack(&mut send, request_id, result, "directory created").await?;
        }
        Some(request::Command::RemoveFile(remove)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || service.remove(&remove.path)).await?;
            send_file_ack(&mut send, request_id, result, "file removed").await?;
        }
        Some(request::Command::RenameFile(rename)) => {
            let service = files.clone();
            let result = tokio::task::spawn_blocking(move || {
                service.rename(&rename.source, &rename.destination, rename.overwrite)
            })
            .await?;
            send_file_ack(&mut send, request_id, result, "file renamed").await?;
        }
        Some(request::Command::ListWorkspaces(_)) if session_objects => {
            let workspace = manager.workspace(&manager.default_workspace_id())?;
            send_response(
                &mut send,
                request_id,
                response::Result::WorkspaceList(WorkspaceListResponse {
                    workspaces: vec![workspace],
                }),
            )
            .await?;
        }
        Some(
            request::Command::CreateWorkspace(_)
            | request::Command::RenameWorkspace(_)
            | request::Command::DeleteWorkspace(_),
        ) if session_objects => {
            send_error(
                &mut send,
                request_id,
                "unsupported",
                anyhow!("multiple Workspaces are not enabled; use the default Workspace"),
            )
            .await?;
        }
        Some(request::Command::ListTerminals(list)) if session_objects => {
            match manager.list_terminals(&list.workspace_id, list.include_exited) {
                Ok(terminals) => {
                    send_response(
                        &mut send,
                        request_id,
                        response::Result::TerminalList(TerminalListResponse { terminals }),
                    )
                    .await?;
                }
                Err(error) => send_error(&mut send, request_id, "workspace", error).await?,
            }
        }
        Some(request::Command::ListAttachments(list)) if session_objects => {
            match manager.list_attachments(&list.workspace_id, &list.terminal_id) {
                Ok(attachments) => {
                    send_response(
                        &mut send,
                        request_id,
                        response::Result::AttachmentList(AttachmentListResponse { attachments }),
                    )
                    .await?;
                }
                Err(error) => send_error(&mut send, request_id, "workspace", error).await?,
            }
        }
        Some(
            request::Command::ListWorkspaces(_)
            | request::Command::CreateWorkspace(_)
            | request::Command::RenameWorkspace(_)
            | request::Command::DeleteWorkspace(_)
            | request::Command::ListTerminals(_)
            | request::Command::ListAttachments(_),
        ) => {
            send_error(
                &mut send,
                request_id,
                "unsupported",
                anyhow!("session.objects capability was not negotiated"),
            )
            .await?;
        }
        None => {
            send_error(
                &mut send,
                request_id,
                "protocol",
                anyhow!("request has no command"),
            )
            .await?
        }
    }
    send.shutdown().await?;
    Ok(())
}

fn is_file_request(message: &WireMessage) -> bool {
    matches!(
        message,
        WireMessage {
            body: Some(wire_message::Body::Request(crate::protocol::Request {
                command: Some(
                    request::Command::FileCapabilities(_)
                        | request::Command::FileStat(_)
                        | request::Command::FileList(_)
                        | request::Command::BeginUpload(_)
                        | request::Command::WriteFileChunk(_)
                        | request::Command::QueryUpload(_)
                        | request::Command::CommitUpload(_)
                        | request::Command::AbortUpload(_)
                        | request::Command::BeginDownload(_)
                        | request::Command::ReadFileChunk(_)
                        | request::Command::MakeDirectory(_)
                        | request::Command::RemoveFile(_)
                        | request::Command::RenameFile(_)
                        | request::Command::GitStatus(_)
                        | request::Command::WatchFiles(_)
                ),
                ..
            })),
        }
    )
}

async fn reject_request_for_quota(
    send: &mut quinn::SendStream,
    message: &WireMessage,
    error: QuotaExceeded,
) -> Result<()> {
    if let WireMessage {
        body: Some(wire_message::Body::Request(request)),
    } = message
    {
        send_error(send, request.request_id.clone(), "quota", error.into()).await?;
    }
    send.shutdown().await?;
    Ok(())
}

async fn send_file_result<W, T>(
    send: &mut W,
    request_id: String,
    result: FileResult<T>,
    wrap: impl FnOnce(T) -> response::Result,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match result {
        Ok(value) => send_response(send, request_id, wrap(value)).await,
        Err(error) => {
            tracing::error!(error = %error, code = error.code, "file request failed");
            send_response(
                send,
                request_id,
                response::Result::Error(ErrorResponse {
                    code: error.code.into(),
                    message: error.message,
                }),
            )
            .await
        }
    }
}

async fn send_file_ack<W>(
    send: &mut W,
    request_id: String,
    result: FileResult<()>,
    message: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    send_file_result(send, request_id, result, |_| {
        response::Result::Ack(AckResponse {
            message: message.into(),
        })
    })
    .await
}

async fn handle_attach<W, R>(
    manager: SessionManager,
    terminal: Arc<Terminal>,
    request_id: String,
    read_only: bool,
    takeover: bool,
    resume_token: String,
    mut send: W,
    mut recv: R,
    negotiated: NegotiatedProtocol,
    connection_id: String,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut lease_events = terminal.subscribe_to_leases();
    let input_lease = negotiated.has(CAPABILITY_INPUT_LEASE, 1);
    let lease = match terminal.acquire_lease(
        read_only,
        takeover,
        &resume_token,
        input_lease.then_some(INPUT_LEASE_TTL),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            send_error(&mut send, request_id, "lease_conflict", error).await?;
            send.shutdown().await?;
            return Ok(());
        }
    };
    let info = terminal.info();
    let mut attachment = match manager.register_attachment(&connection_id, &info, read_only) {
        Ok(attachment) => attachment,
        Err(error) => {
            terminal.release_lease(&lease.lease_id);
            send_domain_error(&mut send, request_id, "attachment", error).await?;
            send.shutdown().await?;
            return Ok(());
        }
    };
    attachment.set_state(crate::protocol::AttachmentState::Snapshotting)?;
    let attachment_info = attachment.info();
    let semantic = negotiated.has(CAPABILITY_SEMANTIC_STATE, 2);
    let session_objects = negotiated.has(CAPABILITY_SESSION_OBJECTS, 1);
    let history_paging = semantic && negotiated.has(CAPABILITY_HISTORY_PAGING, 1);
    let clipboard_write = semantic && negotiated.has(CAPABILITY_CLIPBOARD_WRITE, 1);
    let (snapshot, initial_state, mut events) = if semantic {
        let (state, events) = terminal.semantic_state_and_subscribe()?;
        (None, Some(state), events)
    } else {
        let (snapshot, events) = terminal.snapshot_and_subscribe()?;
        (Some(snapshot), None, events)
    };
    send_response(
        &mut send,
        request_id,
        response::Result::Attach(AttachResponse {
            terminal: Some(info.clone()),
            lease_id: lease.lease_id.clone(),
            read_only,
            history: Vec::new(),
            resume_token: lease.resume_token.clone(),
            snapshot,
            attachment: Some(attachment_info.clone()),
            lease_ttl_ms: lease
                .ttl
                .map(|ttl| u32::try_from(ttl.as_millis()).unwrap_or(u32::MAX))
                .unwrap_or(0),
        }),
    )
    .await?;
    if let Some(state) = initial_state {
        write_terminal_state(&mut send, &info.id, &attachment_info.id, &state).await?;
    }
    attachment.set_state(crate::protocol::AttachmentState::Live)?;

    if info.status != "running" {
        write_terminal_event(
            &mut send,
            &info.id,
            &attachment_info.id,
            terminal_event::Event::Exited(info.exit_code.unwrap_or(1)),
        )
        .await?;
        terminal.release_lease(&lease.lease_id);
        send.shutdown().await?;
        return Ok(());
    }

    let result: Result<()> = async {
        loop {
            let lease_deadline = terminal.lease_deadline(&lease.lease_id);
            let wait_for_lease_expiry = async {
                if let Some(deadline) = lease_deadline {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                incoming = read_message(&mut recv) => {
                    match incoming? {
                        Some(WireMessage { body: Some(wire_message::Body::TerminalCommand(command)) }) => {
                            validate_terminal_command_target(
                                &command,
                                &info.id,
                                &attachment_info.id,
                                session_objects,
                            )?;
                            match command.command {
                                Some(terminal_command::Command::Input(bytes)) => {
                                    terminal.write_input(&command.lease_id, command.sequence, &bytes)?;
                                }
                                Some(terminal_command::Command::Resize(size)) => {
                                    terminal.resize(
                                        &command.lease_id,
                                        command.sequence,
                                        size.rows,
                                        size.cols,
                                        size.pixel_width,
                                        size.pixel_height,
                                    )?;
                                }
                                Some(terminal_command::Command::HistoryPage(request)) => {
                                    if !history_paging {
                                        bail!("history page requested without negotiated capability")
                                    }
                                    let page = terminal.history_page(command.sequence, &request)?;
                                    write_history_page(
                                        &mut send,
                                        &info.id,
                                        &attachment_info.id,
                                        &page,
                                    ).await?;
                                }
                                Some(terminal_command::Command::LeaseControl(control)) => {
                                    if !input_lease {
                                        bail!("lease control requested without negotiated capability")
                                    }
                                    match LeaseControlAction::try_from(control.action) {
                                        Ok(LeaseControlAction::Renew) => terminal.renew_lease(
                                            &command.lease_id,
                                            command.sequence,
                                            INPUT_LEASE_TTL,
                                        )?,
                                        Ok(LeaseControlAction::Release) => terminal
                                            .release_lease_command(&command.lease_id, command.sequence)?,
                                        Ok(LeaseControlAction::Unspecified) | Err(_) => {
                                            bail!("invalid lease control action")
                                        }
                                    }
                                }
                                Some(terminal_command::Command::Detach(_)) | None => break,
                            }
                        }
                        Some(_) => bail!("unexpected message on attach stream"),
                        None => break,
                    }
                }
                lease_event = lease_events.recv(), if !lease.lease_id.is_empty() => {
                    match lease_event {
                        Ok(event) if event.revoked_lease_id == lease.lease_id => {
                            attachment.set_role(AttachmentRole::Viewer)?;
                            let reason = match event.reason {
                                LeaseRevocationReason::TakenOver => "taken_over",
                                LeaseRevocationReason::Expired => "expired",
                                LeaseRevocationReason::Released => "released",
                            };
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                &attachment_info.id,
                                terminal_event::Event::LeaseChanged(LeaseChanged {
                                    read_only: true,
                                    reason: reason.into(),
                                    lease_id: event.revoked_lease_id,
                                    lease_ttl_ms: 0,
                                }),
                            ).await?;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = wait_for_lease_expiry, if input_lease && !lease.lease_id.is_empty() => {
                    terminal.expire_lease_if_due(&lease.lease_id);
                }
                event = events.recv() => {
                    match event {
                        Ok(PtyEvent::Output(bytes)) => {
                            if semantic {
                                let state = terminal.semantic_state()?;
                                write_terminal_state(
                                    &mut send,
                                    &info.id,
                                    &attachment_info.id,
                                    &state,
                                ).await?;
                            } else {
                                write_terminal_event(
                                    &mut send,
                                    &info.id,
                                    &attachment_info.id,
                                    terminal_event::Event::Output(bytes),
                                ).await?;
                            }
                        }
                        Ok(PtyEvent::Exited(code)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                &attachment_info.id,
                                terminal_event::Event::Exited(code),
                            ).await?;
                            break;
                        }
                        Ok(PtyEvent::Error(message)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                &attachment_info.id,
                                terminal_event::Event::Error(message),
                            ).await?;
                        }
                        Ok(PtyEvent::Interactive(interactive)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                &attachment_info.id,
                                terminal_event::Event::Interactive(interactive),
                            ).await?;
                        }
                        Ok(PtyEvent::ClipboardWrite { selection, contents }) => {
                            if clipboard_write {
                                let selection = match selection {
                                    ClipboardSelection::Clipboard => {
                                        WireClipboardSelection::Clipboard
                                    }
                                    ClipboardSelection::Primary => WireClipboardSelection::Primary,
                                };
                                write_terminal_event(
                                    &mut send,
                                    &info.id,
                                    &attachment_info.id,
                                    terminal_event::Event::ClipboardWrite(ClipboardWrite {
                                        selection: selection as i32,
                                        clear: contents.is_none(),
                                        contents: contents.unwrap_or_default(),
                                    }),
                                ).await?;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // A tmux-style authoritative grid lets a slow client
                            // recover exactly instead of continuing after a gap
                            // in the byte stream.
                            attachment.set_state(crate::protocol::AttachmentState::Snapshotting)?;
                            if semantic {
                                let (state, replacement) = terminal.semantic_state_and_subscribe()?;
                                events = replacement;
                                write_terminal_state(
                                    &mut send,
                                    &info.id,
                                    &attachment_info.id,
                                    &state,
                                ).await?;
                            } else {
                                let (snapshot, replacement) = terminal.snapshot_and_subscribe()?;
                                events = replacement;
                                write_terminal_event(
                                    &mut send,
                                    &info.id,
                                    &attachment_info.id,
                                    terminal_event::Event::Snapshot(snapshot),
                                ).await?;
                            }
                            attachment.set_state(crate::protocol::AttachmentState::Live)?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        let _ = write_terminal_event(
            &mut send,
            &info.id,
            &attachment_info.id,
            terminal_event::Event::Error(error.to_string()),
        )
        .await;
    }
    terminal.release_lease(&lease.lease_id);
    let _ = send.shutdown().await;
    result
}

fn validate_terminal_command_target(
    command: &crate::protocol::TerminalCommand,
    terminal_id: &str,
    attachment_id: &str,
    session_objects: bool,
) -> Result<()> {
    ensure!(
        command.terminal_id == terminal_id,
        "terminal command targets the wrong terminal"
    );
    if session_objects {
        ensure!(
            command.attachment_id == attachment_id,
            "terminal command targets the wrong attachment"
        );
    }
    Ok(())
}

async fn send_response<W>(send: &mut W, request_id: String, result: response::Result) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_message(
        send,
        &WireMessage::new(wire_message::Body::Response(Response {
            request_id,
            result: Some(result),
        })),
    )
    .await
}

async fn send_error<W>(
    send: &mut W,
    request_id: String,
    code: &str,
    error: anyhow::Error,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    error!(%error, code, "request failed");
    send_response(
        send,
        request_id,
        response::Result::Error(ErrorResponse {
            code: code.into(),
            message: error.to_string(),
        }),
    )
    .await
}

async fn send_domain_error<W>(
    send: &mut W,
    request_id: String,
    default_code: &str,
    error: anyhow::Error,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let code = if error.downcast_ref::<QuotaExceeded>().is_some() {
        "quota"
    } else {
        default_code
    };
    send_error(send, request_id, code, error).await
}

async fn write_terminal_event<W>(
    send: &mut W,
    terminal_id: &str,
    attachment_id: &str,
    event: terminal_event::Event,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_message(
        send,
        &WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
            terminal_id: terminal_id.into(),
            attachment_id: attachment_id.into(),
            event: Some(event),
        })),
    )
    .await
}

fn terminal_state_chunks(state: &State) -> Result<Vec<TerminalStateChunk>> {
    let encoded = state.encode_to_vec();
    if encoded.len() > MAX_ENCODED_STATE_BYTES {
        bail!("terminal state exceeds the semantic state transport limit")
    }
    let transfer_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let digest = Sha256::digest(&encoded).to_vec();
    let chunk_count = encoded.len().max(1).div_ceil(TERMINAL_STATE_CHUNK_BYTES);
    let total_size = u32::try_from(encoded.len())?;
    let chunk_count = u32::try_from(chunk_count)?;
    let mut chunks = Vec::with_capacity(chunk_count as usize);
    for chunk_index in 0..chunk_count {
        let start = chunk_index as usize * TERMINAL_STATE_CHUNK_BYTES;
        let end = (start + TERMINAL_STATE_CHUNK_BYTES).min(encoded.len());
        chunks.push(TerminalStateChunk {
            transfer_id: transfer_id.clone(),
            chunk_index,
            chunk_count,
            total_size,
            sha256: digest.clone(),
            data: encoded[start..end].to_vec(),
        });
    }
    Ok(chunks)
}

fn history_page_chunks(page: &HistoryPage) -> Result<Vec<HistoryPageChunk>> {
    let encoded = page.encode_to_vec();
    if encoded.len() > MAX_ENCODED_HISTORY_PAGE_BYTES {
        bail!("terminal history page exceeds the transport limit")
    }
    let transfer_id = uuid::Uuid::new_v4().as_bytes().to_vec();
    let digest = Sha256::digest(&encoded).to_vec();
    let chunk_count = encoded.len().max(1).div_ceil(TERMINAL_STATE_CHUNK_BYTES);
    let total_size = u32::try_from(encoded.len())?;
    let chunk_count = u32::try_from(chunk_count)?;
    let mut chunks = Vec::with_capacity(chunk_count as usize);
    for chunk_index in 0..chunk_count {
        let start = chunk_index as usize * TERMINAL_STATE_CHUNK_BYTES;
        let end = (start + TERMINAL_STATE_CHUNK_BYTES).min(encoded.len());
        chunks.push(HistoryPageChunk {
            transfer_id: transfer_id.clone(),
            chunk_index,
            chunk_count,
            total_size,
            sha256: digest.clone(),
            data: encoded[start..end].to_vec(),
        });
    }
    Ok(chunks)
}

async fn write_terminal_state<W>(
    send: &mut W,
    terminal_id: &str,
    attachment_id: &str,
    state: &State,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for chunk in terminal_state_chunks(state)? {
        write_terminal_event(
            send,
            terminal_id,
            attachment_id,
            terminal_event::Event::SemanticStateChunk(chunk),
        )
        .await?;
    }
    Ok(())
}

async fn write_history_page<W>(
    send: &mut W,
    terminal_id: &str,
    attachment_id: &str,
    page: &HistoryPage,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    for chunk in history_page_chunks(page)? {
        write_terminal_event(
            send,
            terminal_id,
            attachment_id,
            terminal_event::Event::HistoryPageChunk(chunk),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::protocol::{FileCapabilitiesRequest, ListRequest, Request};
    use crate::resources::ResourceAccount;
    use crate::terminal_engine::TerminalEngine;

    #[test]
    fn formal_terminal_commands_are_fenced_by_attachment_identity() {
        let mut command = crate::protocol::TerminalCommand {
            terminal_id: "terminal".into(),
            attachment_id: "attachment".into(),
            ..Default::default()
        };
        assert!(validate_terminal_command_target(&command, "terminal", "attachment", true).is_ok());
        command.attachment_id = "stale".into();
        assert!(
            validate_terminal_command_target(&command, "terminal", "attachment", true).is_err()
        );
        assert!(
            validate_terminal_command_target(&command, "terminal", "attachment", false).is_ok()
        );
        command.terminal_id = "wrong".into();
        assert!(
            validate_terminal_command_target(&command, "terminal", "attachment", false).is_err()
        );
    }

    #[test]
    fn initialization_creates_private_state_without_overwriting_partial_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("state"));
        initialize_state(&paths).unwrap();
        assert_eq!(
            fs::metadata(&paths.state_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::remove_file(&paths.key).unwrap();
        assert!(initialize_state(&paths).is_err());
    }

    #[test]
    fn initialization_rejects_a_symlink_state_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let actual = temporary.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let state = temporary.path().join("state");
        symlink(&actual, &state).unwrap();
        assert!(initialize_state(&ServerPaths::new(state)).is_err());
    }

    #[tokio::test]
    async fn stream_quota_returns_protocol_error_without_running_the_request() {
        let mut policy = ResourcePolicy::default();
        policy.user.streams = 1;
        let error = quota_response(
            policy,
            ResourceClaim::stream(),
            request::Command::List(ListRequest {}),
        )
        .await;
        assert_eq!(error.code, "quota");
        assert!(error.message.contains("streams"));
    }

    #[test]
    fn connection_quota_is_a_structured_authentication_failure() {
        let mut limits = ResourcePolicy::default().user;
        limits.connections = 1;
        let resources = ResourceAccount::standalone("test user", limits).unwrap();
        let _existing = resources.reserve(ResourceClaim::connection()).unwrap();
        let error = match resources.reserve(ResourceClaim::connection()) {
            Ok(_) => panic!("second connection reservation unexpectedly succeeded"),
            Err(error) => error,
        };

        let result = connection_quota_auth_result(&error);

        assert!(!result.ok);
        assert_eq!(result.error_code, "connection_quota_exceeded");
        assert!(result.message.contains("connections"));
        assert!(result.message.contains("limit 1"));
    }

    #[tokio::test]
    async fn file_handle_quota_returns_protocol_error_before_file_service_work() {
        let mut policy = ResourcePolicy::default();
        policy.user.file_handles = 1;
        let error = quota_response(
            policy,
            ResourceClaim::file_handle(),
            request::Command::FileCapabilities(FileCapabilitiesRequest {}),
        )
        .await;
        assert_eq!(error.code, "quota");
        assert!(error.message.contains("file_handles"));
    }

    async fn quota_response(
        policy: ResourcePolicy,
        existing: ResourceClaim,
        command: request::Command,
    ) -> ErrorResponse {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("home");
        fs::create_dir(&root).unwrap();
        let resources = ResourceAccount::standalone("test user", policy.user).unwrap();
        let _existing = resources.reserve(existing).unwrap();
        let manager = SessionManager::with_resources(
            root.clone(),
            temporary.path().join("session-catalog.pb"),
            resources.clone(),
            policy,
        )
        .unwrap();
        let files = FileService::with_resources(root, resources).unwrap();
        let (client, server) = tokio::io::duplex(16 * 1024);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let request_id = "quota-request".to_owned();
        let first_message = WireMessage::new(wire_message::Body::Request(Request {
            request_id: request_id.clone(),
            command: Some(command),
        }));
        handle_worker_message(
            manager,
            files,
            server_write,
            server_read,
            first_message,
            NegotiatedProtocol {
                version: crate::PROTOCOL_VERSION,
                capabilities: BTreeMap::new(),
            },
            uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        let response = read_message(&mut client_read)
            .await
            .unwrap()
            .expect("quota response missing");
        let wire_message::Body::Response(response) = response.body.unwrap() else {
            panic!("expected Response")
        };
        assert_eq!(response.request_id, request_id);
        let response::Result::Error(error) = response.result.unwrap() else {
            panic!("expected ErrorResponse")
        };
        error
    }

    #[test]
    fn semantic_state_transport_fragments_with_shared_identity_and_digest() {
        let mut engine = TerminalEngine::new(2, 8, 8, Box::new(std::io::sink())).unwrap();
        engine.advance(b"hello");
        let mut state = engine.semantic_state().unwrap();
        state.title = "x".repeat(TERMINAL_STATE_CHUNK_BYTES + 1);
        let encoded = state.encode_to_vec();
        let chunks = terminal_state_chunks(&state).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].chunk_index, 1);
        assert_eq!(chunks[0].transfer_id.len(), 16);
        assert_eq!(chunks[0].transfer_id, chunks[1].transfer_id);
        assert_eq!(chunks[0].sha256, Sha256::digest(&encoded).to_vec());
        assert_eq!(chunks.concat_data(), encoded);
    }

    #[test]
    fn history_page_transport_preserves_validated_payload() {
        let mut engine = TerminalEngine::new(2, 8, 8, Box::new(std::io::sink())).unwrap();
        engine.advance(b"zero\r\none\r\ntwo\r\nthree");
        let state = engine.semantic_state().unwrap();
        let request = crate::terminal_state_v2::HistoryPageRequest {
            epoch: state.epoch,
            before: state
                .primary
                .unwrap()
                .included_rows
                .last()
                .unwrap()
                .start
                .clone(),
            maximum_rows: 2,
        };
        let page = engine.history_page(1, &request).unwrap();
        let encoded = page.encode_to_vec();
        let chunks = history_page_chunks(&page).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].transfer_id.len(), 16);
        assert_eq!(chunks[0].sha256, Sha256::digest(&encoded).to_vec());
        assert_eq!(chunks.concat_data(), encoded);
    }

    trait ChunkTestData {
        fn concat_data(&self) -> Vec<u8>;
    }

    impl ChunkTestData for [TerminalStateChunk] {
        fn concat_data(&self) -> Vec<u8> {
            self.iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect()
        }
    }

    impl ChunkTestData for [HistoryPageChunk] {
        fn concat_data(&self) -> Vec<u8> {
            self.iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect()
        }
    }
}
