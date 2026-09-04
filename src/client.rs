use std::{
    collections::HashMap,
    fmt, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::CertificateDer;
use tokio::sync::watch;

use crate::{
    ALPN,
    auth::{authentication_payload, sign_challenge},
    known_hosts::{StrictHostKeyChecking, verify_server_certificate},
    negotiation::{
        CAPABILITY_DATAGRAM_STATE, NegotiatedProtocol, ProtocolSupport, client_hello,
        validate_server_hello,
    },
    protocol::{
        AbortUploadRequest, AttachRequest, AttachResponse, BeginDownloadRequest,
        BeginDownloadResponse, BeginUploadRequest, CloseRequest, CommitUploadRequest,
        CreateWorkspaceRequest, FileCapabilitiesRequest, FileCapabilitiesResponse,
        FileChunkResponse, FileListRequest, FileListResponse, FileStatRequest, FileStatResponse,
        LeaseChanged, ListRequest, ListTerminalsRequest, ListWorkspacesRequest,
        MakeDirectoryRequest, QueryUploadRequest, ReadFileChunkRequest, RemoveFileRequest,
        RenameFileRequest, Request, Resize, Response, SpawnRequest, TerminalCommand, TerminalEvent,
        TerminalViewportDatagram, UploadStatusResponse, WireMessage, WorkspaceInfo,
        WriteFileChunkRequest, read_message, request, response, terminal_command, terminal_event,
        wire_message, write_message,
    },
    terminal_state_v2::{HistoryPage, HistoryPageRequest},
    terminal_streaming::{
        ApplyDisposition, HistoryPageAssembler, StreamingReplica, TerminalStateAssembler,
    },
};

#[derive(Debug)]
pub struct ServerResponseError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for ServerResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServerResponseError {}

#[derive(Clone, Debug)]
pub enum ServerTrust {
    /// Validate TLS against exactly this certificate. Kept for scripted and
    /// centrally provisioned deployments.
    PinnedCertificate(PathBuf),
    /// Use SSH-style trust on first use, scoped to the destination host and port.
    KnownHosts {
        host: String,
        port: u16,
        file: PathBuf,
        policy: StrictHostKeyChecking,
    },
}

#[derive(Debug)]
struct DeferredServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl DeferredServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for DeferredServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // The leaf certificate is checked against Astra's known-hosts file as
        // soon as the QUIC handshake completes and before authentication starts.
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct AstraClient {
    _endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    reconnect: ReconnectConfig,
    negotiated: NegotiatedProtocol,
    datagrams: DatagramRouter,
}

type DatagramRoute = (String, String);

#[derive(Clone)]
struct DatagramRouter {
    routes: Arc<Mutex<HashMap<DatagramRoute, watch::Sender<Option<DatagramDelivery>>>>>,
}

#[derive(Clone)]
enum DatagramDelivery {
    Datagram(Box<TerminalViewportDatagram>),
    Closed(String),
}

impl DatagramRouter {
    fn new(connection: quinn::Connection) -> Self {
        let routes = Arc::new(Mutex::new(HashMap::<
            DatagramRoute,
            watch::Sender<Option<DatagramDelivery>>,
        >::new()));
        let task_routes = routes.clone();
        tokio::spawn(async move {
            loop {
                let delivery = match connection.read_datagram().await {
                    Ok(payload) => match TerminalViewportDatagram::decode(payload) {
                        Ok(datagram)
                            if !datagram.terminal_id.is_empty()
                                && !datagram.attachment_id.is_empty()
                                && datagram.diff.is_some() =>
                        {
                            DatagramDelivery::Datagram(Box::new(datagram))
                        }
                        Ok(_) => {
                            continue;
                        }
                        // A malformed or future-version datagram is equivalent to
                        // a lost frame. A later cumulative delta or reliable
                        // keyframe repairs it, so it must not tear down every
                        // attachment sharing this QUIC connection.
                        Err(_) => continue,
                    },
                    Err(error) => {
                        let message = format!("terminal datagram connection ended: {error}");
                        let mut routes = task_routes.lock().expect("datagram routes poisoned");
                        routes.retain(|_, sender| {
                            if sender.is_closed() {
                                return false;
                            }
                            sender.send_replace(Some(DatagramDelivery::Closed(message.clone())));
                            true
                        });
                        break;
                    }
                };
                let DatagramDelivery::Datagram(datagram) = &delivery else {
                    break;
                };
                let route = (datagram.terminal_id.clone(), datagram.attachment_id.clone());
                let mut routes = task_routes.lock().expect("datagram routes poisoned");
                if let Some(sender) = routes.get(&route) {
                    if sender.is_closed() {
                        routes.remove(&route);
                    } else {
                        // Latest-state-wins: a renderer can be arbitrarily slow
                        // without building an unbounded FIFO of obsolete frames.
                        sender.send_replace(Some(delivery));
                    }
                }
            }
        });
        Self { routes }
    }

    fn register(&self, route: DatagramRoute) -> Result<watch::Receiver<Option<DatagramDelivery>>> {
        let (sender, receiver) = watch::channel(None);
        let mut routes = self.routes.lock().expect("datagram routes poisoned");
        if routes.get(&route).is_some_and(|sender| !sender.is_closed()) {
            bail!("terminal attachment already has a datagram consumer")
        }
        routes.insert(route, sender);
        Ok(receiver)
    }

    fn unregister(&self, route: &DatagramRoute) {
        self.routes
            .lock()
            .expect("datagram routes poisoned")
            .remove(route);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStateDelivery {
    ReliableKeyframe,
    Datagram,
}

pub enum StreamingAttachmentEvent {
    State {
        state: Box<crate::terminal_state_v2::State>,
        delivery: TerminalStateDelivery,
    },
    Exited(i32),
    Error(String),
    Interactive(bool),
    LeaseChanged(LeaseChanged),
    ClipboardWrite(crate::protocol::ClipboardWrite),
    HistoryPage(Box<HistoryPage>),
}

pub struct StreamingAttachment {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    terminal_id: String,
    attachment_id: String,
    lease_id: String,
    lease_ttl: Option<std::time::Duration>,
    next_lease_renewal: Option<tokio::time::Instant>,
    next_sequence: u64,
    state_assembler: TerminalStateAssembler,
    history_assembler: HistoryPageAssembler,
    replica: StreamingReplica,
    datagram_receiver: watch::Receiver<Option<DatagramDelivery>>,
    datagram_router: DatagramRouter,
    route: DatagramRoute,
}

impl Drop for StreamingAttachment {
    fn drop(&mut self) {
        self.datagram_router.unregister(&self.route);
    }
}

#[derive(Clone)]
struct ReconnectConfig {
    remote: SocketAddr,
    server_name: String,
    trust: ServerTrust,
    identity: PathBuf,
    username: String,
    support: ProtocolSupport,
}

impl Drop for AstraClient {
    fn drop(&mut self) {
        self.connection.close(0_u32.into(), b"client done");
    }
}

impl AstraClient {
    pub async fn connect(
        remote: SocketAddr,
        server_name: &str,
        trust: &ServerTrust,
        identity: &Path,
        username: &str,
    ) -> Result<Self> {
        Self::connect_with_config(ReconnectConfig {
            remote,
            server_name: server_name.to_owned(),
            trust: trust.clone(),
            identity: identity.to_path_buf(),
            username: username.to_owned(),
            support: ProtocolSupport::command_line_client(),
        })
        .await
    }

    pub async fn connect_with_support(
        remote: SocketAddr,
        server_name: &str,
        trust: &ServerTrust,
        identity: &Path,
        username: &str,
        support: ProtocolSupport,
    ) -> Result<Self> {
        Self::connect_with_config(ReconnectConfig {
            remote,
            server_name: server_name.to_owned(),
            trust: trust.clone(),
            identity: identity.to_path_buf(),
            username: username.to_owned(),
            support,
        })
        .await
    }

    async fn connect_with_config(reconnect: ReconnectConfig) -> Result<Self> {
        let mut tls = match &reconnect.trust {
            ServerTrust::PinnedCertificate(server_certificate) => {
                let mut roots = rustls::RootCertStore::empty();
                roots
                    .add(CertificateDer::from(
                        fs::read(server_certificate).with_context(|| {
                            format!(
                                "failed to read server certificate {}",
                                server_certificate.display()
                            )
                        })?,
                    ))
                    .context("invalid server certificate")?;
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth()
            }
            ServerTrust::KnownHosts { .. } => rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(DeferredServerVerification::new())
                .with_no_client_auth(),
        };
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(tls).context("invalid QUIC client TLS configuration")?,
        ));
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        transport.max_idle_timeout(Some(
            std::time::Duration::from_secs(15)
                .try_into()
                .expect("15 second QUIC idle timeout is valid"),
        ));
        client_config.transport_config(Arc::new(transport));
        let bind: SocketAddr = if reconnect.remote.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(client_config);
        let connection = endpoint
            .connect(reconnect.remote, &reconnect.server_name)?
            .await
            .with_context(|| format!("failed to connect to {}", reconnect.remote))?;
        if let ServerTrust::KnownHosts {
            host,
            port,
            file,
            policy,
        } = &reconnect.trust
            && let Err(error) =
                verify_connection_certificate(&connection, host, *port, file, *policy)
        {
            connection.close(1_u32.into(), b"host certificate rejected");
            return Err(error);
        }
        let negotiated = authenticate(
            &connection,
            &reconnect.identity,
            &reconnect.username,
            &reconnect.support,
        )
        .await?;
        let datagrams = DatagramRouter::new(connection.clone());
        Ok(Self {
            _endpoint: endpoint,
            connection,
            reconnect,
            negotiated,
            datagrams,
        })
    }

    pub async fn reconnect(&self) -> Result<Self> {
        Self::connect_with_config(self.reconnect.clone()).await
    }

    pub fn negotiated_protocol(&self) -> &NegotiatedProtocol {
        &self.negotiated
    }

    pub async fn list(&self) -> Result<Vec<crate::protocol::TerminalInfo>> {
        let response = self.unary(request::Command::List(ListRequest {})).await?;
        match response.result {
            Some(response::Result::List(list)) => Ok(list.terminals),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to list"),
        }
    }

    pub async fn spawn(&self, request: SpawnRequest) -> Result<crate::protocol::TerminalInfo> {
        let response = self.unary(request::Command::Spawn(request)).await?;
        match response.result {
            Some(response::Result::Spawn(spawn)) => spawn
                .terminal
                .context("server returned an empty spawn response"),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to spawn"),
        }
    }

    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let response = self
            .unary(request::Command::ListWorkspaces(ListWorkspacesRequest {}))
            .await?;
        match response.result {
            Some(response::Result::WorkspaceList(list)) => Ok(list.workspaces),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to list workspaces"),
        }
    }

    pub async fn create_workspace(&self, name: String) -> Result<WorkspaceInfo> {
        let response = self
            .unary(request::Command::CreateWorkspace(CreateWorkspaceRequest {
                name,
            }))
            .await?;
        match response.result {
            Some(response::Result::Workspace(workspace)) => workspace
                .workspace
                .context("server returned an empty workspace response"),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to create workspace"),
        }
    }

    pub async fn list_terminals(
        &self,
        workspace_id: String,
        include_exited: bool,
    ) -> Result<Vec<crate::protocol::TerminalInfo>> {
        let response = self
            .unary(request::Command::ListTerminals(ListTerminalsRequest {
                workspace_id,
                include_exited,
            }))
            .await?;
        match response.result {
            Some(response::Result::TerminalList(list)) => Ok(list.terminals),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to list terminals"),
        }
    }

    pub async fn close(&self, terminal_id: String) -> Result<String> {
        self.close_in_workspace(String::new(), terminal_id).await
    }

    pub async fn close_in_workspace(
        &self,
        workspace_id: String,
        terminal_id: String,
    ) -> Result<String> {
        let response = self
            .unary(request::Command::Close(CloseRequest {
                terminal_id,
                workspace_id,
            }))
            .await?;
        match response.result {
            Some(response::Result::Ack(ack)) => Ok(ack.message),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to close"),
        }
    }

    pub async fn attach(
        &self,
        terminal_id: String,
        read_only: bool,
        takeover: bool,
        resume_token: String,
    ) -> Result<(quinn::SendStream, quinn::RecvStream, AttachResponse)> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        let request_id = uuid::Uuid::new_v4().to_string();
        write_message(
            &mut send,
            &WireMessage::new(wire_message::Body::Request(Request {
                request_id: request_id.clone(),
                command: Some(request::Command::Attach(AttachRequest {
                    terminal_id,
                    read_only,
                    takeover,
                    resume_token,
                    workspace_id: String::new(),
                })),
            })),
        )
        .await?;
        let response = require_response(&mut recv, &request_id).await?;
        match response.result {
            Some(response::Result::Attach(attach)) => Ok((send, recv, attach)),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to attach"),
        }
    }

    pub async fn attach_streaming(
        &self,
        request: AttachRequest,
    ) -> Result<(StreamingAttachment, AttachResponse)> {
        if !self.negotiated.has(CAPABILITY_DATAGRAM_STATE, 1) {
            bail!("terminal datagram state was not negotiated")
        }
        if request.workspace_id.is_empty() {
            bail!("streaming attachment requires a workspace ID")
        }
        let (send, recv, attached) = self.attach_request(request).await?;
        let terminal_id = attached
            .terminal
            .as_ref()
            .context("server returned an attach response without terminal identity")?
            .id
            .clone();
        let attachment_id = attached
            .attachment
            .as_ref()
            .context("server returned an attach response without attachment identity")?
            .id
            .clone();
        let route = (terminal_id.clone(), attachment_id.clone());
        let datagram_receiver = self.datagrams.register(route.clone())?;
        let lease_ttl = (!attached.lease_id.is_empty() && attached.lease_ttl_ms > 0)
            .then(|| std::time::Duration::from_millis(u64::from(attached.lease_ttl_ms)));
        let attachment = StreamingAttachment {
            send,
            recv,
            terminal_id: terminal_id.clone(),
            attachment_id: attachment_id.clone(),
            lease_id: attached.lease_id.clone(),
            next_lease_renewal: lease_ttl.map(next_lease_renewal),
            lease_ttl,
            next_sequence: 1,
            state_assembler: TerminalStateAssembler::default(),
            history_assembler: HistoryPageAssembler::default(),
            replica: StreamingReplica::for_route(terminal_id, attachment_id)?,
            datagram_receiver,
            datagram_router: self.datagrams.clone(),
            route,
        };
        Ok((attachment, attached))
    }

    async fn attach_request(
        &self,
        request: AttachRequest,
    ) -> Result<(quinn::SendStream, quinn::RecvStream, AttachResponse)> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        let request_id = uuid::Uuid::new_v4().to_string();
        write_message(
            &mut send,
            &WireMessage::new(wire_message::Body::Request(Request {
                request_id: request_id.clone(),
                command: Some(request::Command::Attach(request)),
            })),
        )
        .await?;
        let response = require_response(&mut recv, &request_id).await?;
        match response.result {
            Some(response::Result::Attach(attach)) => Ok((send, recv, attach)),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to attach"),
        }
    }

    pub async fn file_capabilities(&self) -> Result<FileCapabilitiesResponse> {
        let response = self
            .file_unary(request::Command::FileCapabilities(
                FileCapabilitiesRequest {},
            ))
            .await?;
        match response.result {
            Some(response::Result::FileCapabilities(capabilities)) => Ok(capabilities),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to file capabilities"),
        }
    }

    pub async fn file_stat(
        &self,
        path: Vec<u8>,
        follow_symlinks: bool,
    ) -> Result<FileStatResponse> {
        let response = self
            .file_unary(request::Command::FileStat(FileStatRequest {
                path,
                follow_symlinks,
            }))
            .await?;
        match response.result {
            Some(response::Result::FileStat(stat)) => Ok(stat),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to file stat"),
        }
    }

    pub async fn file_list(
        &self,
        path: Vec<u8>,
        cursor: Vec<u8>,
        limit: u32,
    ) -> Result<FileListResponse> {
        let response = self
            .file_unary(request::Command::FileList(FileListRequest {
                path,
                cursor,
                limit,
            }))
            .await?;
        match response.result {
            Some(response::Result::FileList(list)) => Ok(list),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to file list"),
        }
    }

    pub async fn begin_upload(&self, request: BeginUploadRequest) -> Result<UploadStatusResponse> {
        self.upload_status(request::Command::BeginUpload(request))
            .await
    }

    pub async fn write_file_chunk(
        &self,
        request: WriteFileChunkRequest,
    ) -> Result<UploadStatusResponse> {
        self.upload_status(request::Command::WriteFileChunk(request))
            .await
    }

    pub async fn query_upload(&self, transfer_id: String) -> Result<UploadStatusResponse> {
        self.upload_status(request::Command::QueryUpload(QueryUploadRequest {
            transfer_id,
        }))
        .await
    }

    pub async fn commit_upload(&self, transfer_id: String) -> Result<UploadStatusResponse> {
        self.upload_status(request::Command::CommitUpload(CommitUploadRequest {
            transfer_id,
        }))
        .await
    }

    pub async fn abort_upload(&self, transfer_id: String) -> Result<UploadStatusResponse> {
        self.upload_status(request::Command::AbortUpload(AbortUploadRequest {
            transfer_id,
        }))
        .await
    }

    pub async fn begin_download(
        &self,
        path: Vec<u8>,
        want_sha256: bool,
    ) -> Result<BeginDownloadResponse> {
        let response = self
            .file_unary(request::Command::BeginDownload(BeginDownloadRequest {
                path,
                want_sha256,
            }))
            .await?;
        match response.result {
            Some(response::Result::BeginDownload(download)) => Ok(download),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to begin download"),
        }
    }

    pub async fn read_file_chunk(
        &self,
        request: ReadFileChunkRequest,
    ) -> Result<FileChunkResponse> {
        let response = self
            .file_unary(request::Command::ReadFileChunk(request))
            .await?;
        match response.result {
            Some(response::Result::FileChunk(chunk)) => Ok(chunk),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to file read"),
        }
    }

    pub async fn make_directory(&self, path: Vec<u8>) -> Result<String> {
        self.file_ack(request::Command::MakeDirectory(MakeDirectoryRequest {
            path,
        }))
        .await
    }

    pub async fn remove_file(&self, path: Vec<u8>) -> Result<String> {
        self.file_ack(request::Command::RemoveFile(RemoveFileRequest { path }))
            .await
    }

    pub async fn rename_file(
        &self,
        source: Vec<u8>,
        destination: Vec<u8>,
        overwrite: bool,
    ) -> Result<String> {
        self.file_ack(request::Command::RenameFile(RenameFileRequest {
            source,
            destination,
            overwrite,
        }))
        .await
    }

    async fn upload_status(&self, command: request::Command) -> Result<UploadStatusResponse> {
        let response = self.file_unary(command).await?;
        match response.result {
            Some(response::Result::UploadStatus(status)) => Ok(status),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to upload operation"),
        }
    }

    async fn file_ack(&self, command: request::Command) -> Result<String> {
        let response = self.file_unary(command).await?;
        match response.result {
            Some(response::Result::Ack(ack)) => Ok(ack.message),
            Some(response::Result::Error(error)) => Err(server_response_error(error)),
            _ => bail!("server returned the wrong response to file operation"),
        }
    }

    async fn unary(&self, command: request::Command) -> Result<Response> {
        self.unary_with_priority(command, 0).await
    }

    async fn file_unary(&self, command: request::Command) -> Result<Response> {
        // Quinn schedules higher numeric priorities first. File traffic stays below terminal
        // streams so a large upload cannot make interactive input feel sluggish.
        self.unary_with_priority(command, -10).await
    }

    async fn unary_with_priority(
        &self,
        command: request::Command,
        priority: i32,
    ) -> Result<Response> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        send.set_priority(priority)?;
        let request_id = uuid::Uuid::new_v4().to_string();
        write_message(
            &mut send,
            &WireMessage::new(wire_message::Body::Request(Request {
                request_id: request_id.clone(),
                command: Some(command),
            })),
        )
        .await?;
        send.finish()?;
        require_response(&mut recv, &request_id).await
    }
}

async fn next_datagram(
    receiver: &mut watch::Receiver<Option<DatagramDelivery>>,
) -> Option<DatagramDelivery> {
    receiver.changed().await.ok()?;
    receiver.borrow_and_update().clone()
}

impl StreamingAttachment {
    pub fn terminal_id(&self) -> &str {
        &self.terminal_id
    }

    pub fn attachment_id(&self) -> &str {
        &self.attachment_id
    }

    pub fn current_state(&self) -> Option<&crate::terminal_state_v2::State> {
        self.replica.current()
    }

    pub async fn next_event(&mut self) -> Result<StreamingAttachmentEvent> {
        loop {
            enum Incoming {
                Reliable(Result<Option<Box<WireMessage>>>),
                Datagram(Option<DatagramDelivery>),
            }
            let lease_renewal = self.next_lease_renewal;
            let wait_for_lease_renewal = async move {
                if let Some(deadline) = lease_renewal {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let incoming = tokio::select! {
                message = read_message(&mut self.recv) => {
                    Incoming::Reliable(message.map(|message| message.map(Box::new)))
                },
                datagram = next_datagram(&mut self.datagram_receiver) => {
                    Incoming::Datagram(datagram)
                },
                _ = wait_for_lease_renewal => {
                    self.renew_lease().await?;
                    continue;
                },
            };
            match incoming {
                Incoming::Datagram(Some(DatagramDelivery::Datagram(datagram))) => {
                    match self.replica.apply_datagram(&datagram)? {
                        ApplyDisposition::Applied => {
                            self.send_state_ack().await?;
                            return Ok(StreamingAttachmentEvent::State {
                                state: Box::new(
                                    self.replica
                                        .current()
                                        .context("applied terminal state disappeared")?
                                        .clone(),
                                ),
                                delivery: TerminalStateDelivery::Datagram,
                            });
                        }
                        ApplyDisposition::Stale => continue,
                        ApplyDisposition::MissingBase => {
                            let repair = self.replica.repair_request(&datagram)?;
                            self.send_state_control(terminal_command::Command::StateRepair(repair))
                                .await?;
                            continue;
                        }
                    }
                }
                Incoming::Datagram(Some(DatagramDelivery::Closed(message))) => bail!(message),
                Incoming::Datagram(None) => bail!("terminal datagram router stopped"),
                Incoming::Reliable(Err(error)) => return Err(error),
                Incoming::Reliable(Ok(None)) => bail!("attachment stream ended"),
                Incoming::Reliable(Ok(Some(message))) => {
                    let WireMessage { body } = *message;
                    let Some(wire_message::Body::TerminalEvent(event)) = body else {
                        bail!("unexpected message on streaming attachment")
                    };
                    self.validate_event_target(&event)?;
                    match event.event {
                        Some(terminal_event::Event::SemanticStateChunk(chunk)) => {
                            if let Some(state) = self.state_assembler.push(chunk)? {
                                match self.replica.apply_keyframe(state)? {
                                    ApplyDisposition::Applied => {
                                        self.send_state_ack().await?;
                                        return Ok(StreamingAttachmentEvent::State {
                                            state: Box::new(
                                                self.replica
                                                    .current()
                                                    .context("applied terminal state disappeared")?
                                                    .clone(),
                                            ),
                                            delivery: TerminalStateDelivery::ReliableKeyframe,
                                        });
                                    }
                                    ApplyDisposition::Stale => {
                                        self.send_state_ack().await?;
                                    }
                                    ApplyDisposition::MissingBase => {
                                        unreachable!("a reliable keyframe does not need a base")
                                    }
                                }
                            }
                        }
                        Some(terminal_event::Event::Exited(code)) => {
                            return Ok(StreamingAttachmentEvent::Exited(code));
                        }
                        Some(terminal_event::Event::Error(message)) => {
                            return Ok(StreamingAttachmentEvent::Error(message));
                        }
                        Some(terminal_event::Event::Interactive(interactive)) => {
                            return Ok(StreamingAttachmentEvent::Interactive(interactive));
                        }
                        Some(terminal_event::Event::LeaseChanged(change)) => {
                            self.lease_id.clone_from(&change.lease_id);
                            if change.read_only
                                || change.lease_id.is_empty()
                                || change.lease_ttl_ms == 0
                            {
                                self.lease_ttl = None;
                                self.next_lease_renewal = None;
                            } else {
                                let ttl = std::time::Duration::from_millis(u64::from(
                                    change.lease_ttl_ms,
                                ));
                                self.lease_ttl = Some(ttl);
                                self.next_lease_renewal = Some(next_lease_renewal(ttl));
                            }
                            return Ok(StreamingAttachmentEvent::LeaseChanged(change));
                        }
                        Some(terminal_event::Event::ClipboardWrite(write)) => {
                            return Ok(StreamingAttachmentEvent::ClipboardWrite(write));
                        }
                        Some(terminal_event::Event::HistoryPageChunk(chunk)) => {
                            if let Some(page) = self.history_assembler.push(chunk)? {
                                return Ok(StreamingAttachmentEvent::HistoryPage(Box::new(page)));
                            }
                        }
                        Some(terminal_event::Event::SemanticStateDiffChunk(_)) => {
                            bail!("server mixed reliable semantic diffs with datagram state")
                        }
                        Some(terminal_event::Event::ViewportDatagram(_)) => {
                            bail!("server forwarded an internal datagram bridge frame")
                        }
                        Some(terminal_event::Event::Output(_))
                        | Some(terminal_event::Event::Snapshot(_)) => {
                            bail!("server sent legacy terminal output to a streaming attachment")
                        }
                        None => continue,
                    }
                }
            }
        }
    }

    pub async fn send_input(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.renew_lease_if_due().await?;
        let sequence = self.take_sequence()?;
        self.send_command(sequence, terminal_command::Command::Input(bytes))
            .await
    }

    pub async fn resize(&mut self, size: Resize) -> Result<()> {
        self.renew_lease_if_due().await?;
        let sequence = self.take_sequence()?;
        self.send_command(sequence, terminal_command::Command::Resize(size))
            .await
    }

    pub async fn request_history(&mut self, request: HistoryPageRequest) -> Result<()> {
        let sequence = self.take_sequence()?;
        self.send_command(sequence, terminal_command::Command::HistoryPage(request))
            .await
    }

    pub async fn detach(mut self) -> Result<()> {
        self.send_command(0, terminal_command::Command::Detach(true))
            .await?;
        self.send.finish()?;
        Ok(())
    }

    fn validate_event_target(&self, event: &TerminalEvent) -> Result<()> {
        if event.terminal_id != self.terminal_id || event.attachment_id != self.attachment_id {
            bail!("server sent a terminal event for the wrong attachment")
        }
        Ok(())
    }

    fn take_sequence(&mut self) -> Result<u64> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .context("terminal command sequence overflowed")?;
        Ok(sequence)
    }

    async fn send_state_ack(&mut self) -> Result<()> {
        let ack = self
            .replica
            .state_ack()
            .context("terminal replica has no state to acknowledge")?;
        match self
            .send_state_control(terminal_command::Command::StateAck(ack))
            .await
        {
            Ok(()) => Ok(()),
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::NotConnected
                    )
                }) =>
            {
                // A final reliable State can be immediately followed by
                // Exited and STOP_SENDING. The committed state remains valid;
                // the next read delivers the terminal's closure.
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn renew_lease_if_due(&mut self) -> Result<()> {
        if self
            .next_lease_renewal
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            self.renew_lease().await?;
        }
        Ok(())
    }

    async fn renew_lease(&mut self) -> Result<()> {
        let Some(ttl) = self.lease_ttl else {
            self.next_lease_renewal = None;
            return Ok(());
        };
        if self.lease_id.is_empty() {
            self.lease_ttl = None;
            self.next_lease_renewal = None;
            return Ok(());
        }
        let sequence = self.take_sequence()?;
        self.send_command(
            sequence,
            terminal_command::Command::LeaseControl(crate::protocol::LeaseControl {
                action: crate::protocol::LeaseControlAction::Renew as i32,
            }),
        )
        .await?;
        self.next_lease_renewal = Some(next_lease_renewal(ttl));
        Ok(())
    }

    async fn send_state_control(&mut self, command: terminal_command::Command) -> Result<()> {
        self.send_command(0, command).await
    }

    async fn send_command(
        &mut self,
        sequence: u64,
        command: terminal_command::Command,
    ) -> Result<()> {
        write_message(
            &mut self.send,
            &WireMessage::new(wire_message::Body::TerminalCommand(TerminalCommand {
                terminal_id: self.terminal_id.clone(),
                lease_id: self.lease_id.clone(),
                sequence,
                attachment_id: self.attachment_id.clone(),
                command: Some(command),
            })),
        )
        .await
    }
}

fn next_lease_renewal(ttl: std::time::Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + ttl.div_f32(2.0).max(std::time::Duration::from_secs(1))
}

fn server_response_error(error: crate::protocol::ErrorResponse) -> anyhow::Error {
    ServerResponseError {
        code: error.code,
        message: error.message,
    }
    .into()
}

fn verify_connection_certificate(
    connection: &quinn::Connection,
    host: &str,
    port: u16,
    known_hosts_file: &Path,
    policy: StrictHostKeyChecking,
) -> Result<()> {
    let identity = connection
        .peer_identity()
        .context("server did not present a TLS certificate")?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow!("QUIC backend returned an unexpected server identity type"))?;
    let leaf = certificates
        .first()
        .context("server presented an empty TLS certificate chain")?;
    verify_server_certificate(host, port, leaf.as_ref(), known_hosts_file, policy)?;
    Ok(())
}

async fn authenticate(
    connection: &quinn::Connection,
    identity: &Path,
    username: &str,
    support: &ProtocolSupport,
) -> Result<NegotiatedProtocol> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let client_hello = client_hello(username, support);
    write_message(
        &mut send,
        &WireMessage::new(wire_message::Body::ClientHello(client_hello.clone())),
    )
    .await?;
    let hello = match read_message(&mut recv).await? {
        Some(WireMessage {
            body: Some(wire_message::Body::ServerHello(hello)),
        }) => hello,
        _ => bail!("server did not send ServerHello"),
    };
    let negotiated = validate_server_hello(&client_hello, &hello)?;
    let payload = authentication_payload(&hello.challenge, username, &hello.server_instance);
    let (public_key, signature_pem) = sign_challenge(identity, &payload)?;
    write_message(
        &mut send,
        &WireMessage::new(wire_message::Body::AuthRequest(
            crate::protocol::AuthRequest {
                public_key,
                signature_pem,
            },
        )),
    )
    .await?;
    send.finish()?;
    match read_message(&mut recv).await? {
        Some(WireMessage {
            body: Some(wire_message::Body::AuthResult(result)),
        }) if result.ok => Ok(negotiated),
        Some(WireMessage {
            body: Some(wire_message::Body::AuthResult(result)),
        }) if !result.error_code.is_empty() => {
            bail!("{}: {}", result.error_code, result.message)
        }
        Some(WireMessage {
            body: Some(wire_message::Body::AuthResult(result)),
        }) => bail!("authentication failed: {}", result.message),
        _ => bail!("server did not return AuthResult"),
    }
}

async fn require_response(recv: &mut quinn::RecvStream, request_id: &str) -> Result<Response> {
    match read_message(recv).await? {
        Some(WireMessage {
            body: Some(wire_message::Body::Response(response)),
        }) if response.request_id == request_id => Ok(response),
        Some(WireMessage {
            body: Some(wire_message::Body::Response(response)),
        }) => Err(anyhow!(
            "response request ID mismatch: expected {request_id}, got {}",
            response.request_id
        )),
        _ => bail!("server did not return a Response"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    use anyhow::Context;
    use ssh_key::{Algorithm, LineEnding, PrivateKey};

    use super::*;
    use crate::{
        accounts::SystemAccount,
        protocol::AttachRequest,
        resources::ResourcePolicy,
        server::{ServerMode, ServerOptions, ServerPaths, initialize_state, serve},
        terminal_state_v2::State,
    };

    fn visible_text(state: &State) -> String {
        state
            .primary
            .iter()
            .chain(state.alternate.iter())
            .flat_map(|screen| screen.included_rows.iter())
            .flat_map(|row| row.cells.iter())
            .map(|cell| cell.grapheme.as_str())
            .collect()
    }

    async fn wait_for_state(
        attachment: &mut StreamingAttachment,
        expected_delivery: Option<TerminalStateDelivery>,
        text: Option<&str>,
    ) -> Result<State> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match attachment.next_event().await? {
                    StreamingAttachmentEvent::State { state, delivery }
                        if expected_delivery.is_none_or(|expected| delivery == expected)
                            && text.is_none_or(|text| visible_text(&state).contains(text)) =>
                    {
                        return Ok(*state);
                    }
                    StreamingAttachmentEvent::Exited(code) => {
                        bail!("terminal exited before expected state with status {code}")
                    }
                    StreamingAttachmentEvent::Error(message) => bail!(message),
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for streamed terminal state")?
    }

    async fn wait_for_history(attachment: &mut StreamingAttachment) -> Result<HistoryPage> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match attachment.next_event().await? {
                    StreamingAttachmentEvent::HistoryPage(page) => return Ok(*page),
                    StreamingAttachmentEvent::Exited(code) => {
                        bail!("terminal exited before history arrived with status {code}")
                    }
                    StreamingAttachmentEvent::Error(message) => bail!(message),
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for terminal history")?
    }

    async fn wait_for_exit(attachment: &mut StreamingAttachment) -> Result<i32> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match attachment.next_event().await? {
                    StreamingAttachmentEvent::Exited(code) => return Ok(code),
                    StreamingAttachmentEvent::Error(message) => bail!(message),
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for terminal exit")?
    }

    #[tokio::test]
    async fn datagram_mailbox_keeps_only_the_latest_viewport() {
        let (sender, mut receiver) = watch::channel(None);
        for generation in 1..=100 {
            sender.send_replace(Some(DatagramDelivery::Datagram(Box::new(
                TerminalViewportDatagram {
                    terminal_id: "terminal".into(),
                    attachment_id: "attachment".into(),
                    diff: Some(crate::protocol::TerminalStateDiff {
                        target_generation: generation,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ))));
        }

        let Some(DatagramDelivery::Datagram(datagram)) = next_datagram(&mut receiver).await else {
            panic!("latest viewport datagram was not delivered")
        };
        assert_eq!(datagram.diff.unwrap().target_generation, 100);
        assert!(!receiver.has_changed().unwrap());
    }

    #[tokio::test]
    async fn semantic_client_routes_two_live_terminals_over_one_quic_connection() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = ServerPaths::new(temporary.path().join("state"));
        initialize_state(&paths)?;

        let mut rng = ssh_key::rand_core::OsRng;
        let identity = PrivateKey::random(&mut rng, Algorithm::Ed25519)?;
        let identity_path = temporary.path().join("id_ed25519");
        fs::write(&identity_path, identity.to_openssh(LineEnding::LF)?)?;
        fs::set_permissions(&identity_path, fs::Permissions::from_mode(0o600))?;
        fs::write(
            &paths.authorized_keys,
            format!("{}\n", identity.public_key().to_openssh()?),
        )?;

        let session_root = temporary.path().join("home");
        fs::create_dir(&session_root)?;
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let listen = reservation.local_addr()?;
        drop(reservation);
        let server = tokio::spawn(serve(ServerOptions {
            listen,
            paths: paths.clone(),
            mode: ServerMode::Rootless {
                session_root: session_root.clone(),
            },
            resource_policy: ResourcePolicy::default(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let username = SystemAccount::current()?.username;
        let trust = ServerTrust::PinnedCertificate(paths.cert.clone());
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            AstraClient::connect_with_support(
                listen,
                "localhost",
                &trust,
                &identity_path,
                &username,
                ProtocolSupport::rust_semantic_client(),
            ),
        )
        .await
        .context("timed out connecting semantic client")??;
        assert!(
            client
                .negotiated_protocol()
                .has(CAPABILITY_DATAGRAM_STATE, 1)
        );
        let workspace = client
            .list_workspaces()
            .await?
            .into_iter()
            .find(|workspace| workspace.is_default)
            .context("server has no default workspace")?;

        let spawn = |name: &str| SpawnRequest {
            name: name.into(),
            argv: vec!["/bin/cat".into()],
            cwd: String::new(),
            rows: 6,
            cols: 40,
            term: "xterm-256color".into(),
            environment: vec![],
            workspace_id: workspace.id.clone(),
        };
        let first = client.spawn(spawn("first")).await?;
        let second = client.spawn(spawn("second")).await?;
        let first_terminal_id = first.id.clone();
        let attach = |terminal_id: String| AttachRequest {
            terminal_id,
            read_only: false,
            takeover: false,
            resume_token: String::new(),
            workspace_id: workspace.id.clone(),
        };
        let (mut first_attachment, first_attached) =
            client.attach_streaming(attach(first.id)).await?;
        let (mut second_attachment, _) = client.attach_streaming(attach(second.id)).await?;

        let first_keyframe = wait_for_state(
            &mut first_attachment,
            Some(TerminalStateDelivery::ReliableKeyframe),
            None,
        )
        .await?;
        let second_keyframe = wait_for_state(
            &mut second_attachment,
            Some(TerminalStateDelivery::ReliableKeyframe),
            None,
        )
        .await?;
        first_attachment.send_input(b"route-one\n".to_vec()).await?;
        second_attachment
            .send_input(b"route-two\n".to_vec())
            .await?;

        let first_delta = wait_for_state(
            &mut first_attachment,
            Some(TerminalStateDelivery::Datagram),
            Some("route-one"),
        )
        .await?;
        let second_delta = wait_for_state(
            &mut second_attachment,
            Some(TerminalStateDelivery::Datagram),
            Some("route-two"),
        )
        .await?;
        assert!(first_delta.generation > first_keyframe.generation);
        assert!(second_delta.generation > second_keyframe.generation);
        assert!(!visible_text(&first_delta).contains("route-two"));
        assert!(!visible_text(&second_delta).contains("route-one"));

        first_attachment
            .send_input(b"h0\nh1\nh2\nh3\nh4\nh5\nh6\nh7\n".to_vec())
            .await?;
        let history_target = wait_for_state(&mut first_attachment, None, Some("h7")).await?;
        let history_before = history_target
            .primary
            .as_ref()
            .and_then(|screen| screen.included_start.clone())
            .context("live viewport has no history anchor")?;
        first_attachment
            .request_history(HistoryPageRequest {
                epoch: history_target.epoch,
                before: Some(history_before),
                maximum_rows: 4,
            })
            .await?;
        let history = wait_for_history(&mut first_attachment).await?;
        assert!(!history.included_rows.is_empty());
        assert!(history.included_rows.len() <= 4);

        second_attachment.send_input(vec![0x04]).await?;
        assert_eq!(wait_for_exit(&mut second_attachment).await?, 0);
        drop(second_attachment);

        // Reconnect the same persistent terminal over a fresh authenticated
        // QUIC connection and resume its input lease. The new attachment must
        // start from a reliable viewport and then return to datagram updates.
        client
            .connection
            .close(0_u32.into(), b"integration test connection loss");
        let reconnected = client.reconnect().await?;
        let (mut resumed_attachment, resumed) = reconnected
            .attach_streaming(AttachRequest {
                terminal_id: first_terminal_id,
                read_only: false,
                takeover: false,
                resume_token: first_attached.resume_token,
                workspace_id: workspace.id,
            })
            .await?;
        assert!(!resumed.lease_id.is_empty());
        drop(first_attachment);
        drop(client);
        wait_for_state(
            &mut resumed_attachment,
            Some(TerminalStateDelivery::ReliableKeyframe),
            Some("h7"),
        )
        .await?;
        resumed_attachment
            .send_input(b"after-reconnect\n".to_vec())
            .await?;
        wait_for_state(
            &mut resumed_attachment,
            Some(TerminalStateDelivery::Datagram),
            Some("after-reconnect"),
        )
        .await?;
        resumed_attachment.send_input(vec![0x04]).await?;
        assert_eq!(wait_for_exit(&mut resumed_attachment).await?, 0);
        drop(resumed_attachment);
        drop(reconnected);
        server.abort();
        let _ = server.await;
        Ok(())
    }
}
