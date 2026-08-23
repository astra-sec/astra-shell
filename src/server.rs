use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{error, info, trace, warn};

use crate::{
    ALPN, PROTOCOL_VERSION,
    accounts::{SystemAccount, authorized_key_files, effective_uid},
    auth::{authentication_payload, verify_authorized_key, verify_authorized_keys},
    files::{FileResult, FileService},
    process_lock::ProcessLock,
    protocol::{
        AckResponse, AttachResponse, AuthResult, ErrorResponse, ListResponse, Response,
        ServerHello, SpawnResponse, TerminalEvent, WireMessage, read_message, request, response,
        terminal_command, terminal_event, wire_message, write_message,
    },
    terminal::{PtyEvent, Terminal, TerminalManager},
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
    },
}

#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub paths: ServerPaths,
    pub mode: ServerMode,
}

#[derive(Clone)]
struct ServerState {
    mode: ModeState,
    instance_id: String,
}

#[derive(Clone)]
enum ModeState {
    Rootless {
        account: SystemAccount,
        manager: TerminalManager,
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
        manager: TerminalManager,
        files: FileService,
    },
    Worker {
        router: Arc<WorkerRouter>,
        account: SystemAccount,
    },
}

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
    ensure_initialized(&options.paths)?;
    if matches!(&options.mode, ServerMode::Managed { .. }) {
        validate_managed_gateway_state(&options.paths)?;
    }
    let _daemon_lock = ProcessLock::acquire(&options.paths.state_dir.join("gateway.lock"))?;
    let mode = match options.mode {
        ServerMode::Rootless { session_root } => {
            let files = FileService::new(session_root.clone())?;
            ModeState::Rootless {
                account: SystemAccount::current()?,
                manager: TerminalManager::new(session_root)?,
                files,
                authorized_keys: options.paths.authorized_keys.clone(),
            }
        }
        ServerMode::Managed {
            authorized_keys_directory,
            session_root_override,
        } => ModeState::Managed {
            router: WorkerRouter::new(&options.paths.state_dir, session_root_override)?,
            authorized_keys_directory,
        },
    };
    let instance_id = fs::read_to_string(&options.paths.instance_id)?
        .trim()
        .to_owned();
    let state = ServerState { mode, instance_id };

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
    let (username, fingerprint, backend) = match tokio::time::timeout(
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
    info!(%remote, %username, %fingerprint, "client authenticated");

    loop {
        let (send, mut recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let backend = backend.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                let first_message = read_message(&mut recv)
                    .await?
                    .context("request stream ended before its first message")?;
                if is_file_request(&first_message) {
                    send.set_priority(-10)?;
                }
                match backend {
                    ConnectionBackend::Local { manager, files } => {
                        handle_worker_message(manager, files, send, recv, first_message).await
                    }
                    ConnectionBackend::Worker { router, account } => {
                        router
                            .proxy_stream(&account, send, recv, first_message)
                            .await
                    }
                }
            }
            .await;
            if let Err(error) = result {
                warn!(%error, "request stream failed");
            }
        });
    }
}

async fn authenticate_connection(
    state: &ServerState,
    connection: &quinn::Connection,
) -> Result<(String, String, ConnectionBackend)> {
    let (mut auth_send, mut auth_recv, first_message) =
        accept_authentication_stream(connection).await?;
    let username = match first_message {
        WireMessage {
            body: Some(wire_message::Body::ClientHello(hello)),
        } if hello.protocol_version == PROTOCOL_VERSION && !hello.username.is_empty() => {
            hello.username
        }
        WireMessage {
            body: Some(wire_message::Body::ClientHello(hello)),
        } if hello.protocol_version != PROTOCOL_VERSION => bail!(
            "client protocol version {} is incompatible with server version {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        ),
        WireMessage {
            body: Some(wire_message::Body::ClientHello(_)),
        } => bail!("ClientHello has no target Unix username"),
        _ => bail!("expected ClientHello"),
    };

    let challenge: [u8; 32] = rand::random();
    write_message(
        &mut auth_send,
        &WireMessage::new(wire_message::Body::ServerHello(ServerHello {
            protocol_version: PROTOCOL_VERSION,
            challenge: challenge.to_vec(),
            server_instance: state.instance_id.clone(),
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
        Ok(authenticated) => {
            write_message(
                &mut auth_send,
                &WireMessage::new(wire_message::Body::AuthResult(AuthResult {
                    ok: true,
                    message: authenticated.0.clone(),
                })),
            )
            .await?;
            auth_send.finish()?;
            authenticated
        }
        Err(error) => {
            let _ = write_message(
                &mut auth_send,
                &WireMessage::new(wire_message::Body::AuthResult(AuthResult {
                    ok: false,
                    message: "public key authentication failed".into(),
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
    Ok((username, fingerprint, backend))
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
    manager: TerminalManager,
    files: FileService,
    send: W,
    mut recv: R,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let first_message = read_message(&mut recv)
        .await?
        .context("request stream ended before its first message")?;
    handle_worker_message(manager, files, send, recv, first_message).await
}

async fn handle_worker_message<W, R>(
    manager: TerminalManager,
    files: FileService,
    mut send: W,
    recv: R,
    first_message: WireMessage,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let request = match first_message {
        WireMessage {
            body: Some(wire_message::Body::Request(request)),
        } => request,
        _ => bail!("expected Request as first stream message"),
    };
    let request_id = request.request_id.clone();
    match request.command {
        Some(request::Command::List(_)) => {
            let terminals = manager.list();
            send_response(
                &mut send,
                request_id,
                response::Result::List(ListResponse { terminals }),
            )
            .await?;
        }
        Some(request::Command::Spawn(spawn)) => match manager.spawn(spawn) {
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
            Err(error) => send_error(&mut send, request_id, "spawn", error).await?,
        },
        Some(request::Command::Close(close)) => match manager.get(&close.terminal_id) {
            Some(terminal) => match terminal.kill() {
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
            None => {
                send_error(
                    &mut send,
                    request_id,
                    "not_found",
                    anyhow!("terminal is not active"),
                )
                .await?
            }
        },
        Some(request::Command::Attach(attach)) => {
            let Some(terminal) = manager.get(&attach.terminal_id) else {
                send_error(
                    &mut send,
                    request_id,
                    "not_found",
                    anyhow!("terminal is not active in this daemon"),
                )
                .await?;
                send.shutdown().await?;
                return Ok(());
            };
            handle_attach(
                terminal,
                request_id,
                attach.read_only,
                attach.takeover,
                attach.resume_token,
                send,
                recv,
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
                ),
                ..
            })),
        }
    )
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
    terminal: Arc<Terminal>,
    request_id: String,
    read_only: bool,
    takeover: bool,
    resume_token: String,
    mut send: W,
    mut recv: R,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let lease = match terminal.acquire_lease(read_only, takeover, &resume_token) {
        Ok(lease) => lease,
        Err(error) => {
            send_error(&mut send, request_id, "lease_conflict", error).await?;
            send.shutdown().await?;
            return Ok(());
        }
    };
    let (history, mut events) = terminal.snapshot_and_subscribe();
    let info = terminal.info();
    send_response(
        &mut send,
        request_id,
        response::Result::Attach(AttachResponse {
            terminal: Some(info.clone()),
            lease_id: lease.lease_id.clone(),
            read_only,
            history,
            resume_token: lease.resume_token.clone(),
        }),
    )
    .await?;

    if info.status != "running" {
        write_terminal_event(
            &mut send,
            &info.id,
            terminal_event::Event::Exited(info.exit_code.unwrap_or(1)),
        )
        .await?;
        terminal.release_lease(&lease.lease_id);
        send.shutdown().await?;
        return Ok(());
    }

    let result: Result<()> = async {
        loop {
            tokio::select! {
                incoming = read_message(&mut recv) => {
                    match incoming? {
                        Some(WireMessage { body: Some(wire_message::Body::TerminalCommand(command)) }) => {
                            if command.terminal_id != info.id {
                                bail!("terminal command targets the wrong terminal")
                            }
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
                                    )?;
                                }
                                Some(terminal_command::Command::Detach(_)) | None => break,
                            }
                        }
                        Some(_) => bail!("unexpected message on attach stream"),
                        None => break,
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(PtyEvent::Output(bytes)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                terminal_event::Event::Output(bytes),
                            ).await?;
                        }
                        Ok(PtyEvent::Exited(code)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                terminal_event::Event::Exited(code),
                            ).await?;
                            break;
                        }
                        Ok(PtyEvent::Error(message)) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                terminal_event::Event::Error(message),
                            ).await?;
                        }
                        Err(broadcast_error) => {
                            write_terminal_event(
                                &mut send,
                                &info.id,
                                terminal_event::Event::Error(format!(
                                    "attachment fell behind terminal output: {broadcast_error}"
                                )),
                            ).await?;
                        }
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
            terminal_event::Event::Error(error.to_string()),
        )
        .await;
    }
    terminal.release_lease(&lease.lease_id);
    let _ = send.shutdown().await;
    result
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

async fn write_terminal_event<W>(
    send: &mut W,
    terminal_id: &str,
    event: terminal_event::Event,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_message(
        send,
        &WireMessage::new(wire_message::Body::TerminalEvent(TerminalEvent {
            terminal_id: terminal_id.into(),
            event: Some(event),
        })),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

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
}
