use std::{
    fmt, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::CertificateDer;

use crate::{
    ALPN,
    auth::{authentication_payload, sign_challenge},
    known_hosts::{StrictHostKeyChecking, verify_server_certificate},
    negotiation::{NegotiatedProtocol, ProtocolSupport, client_hello, validate_server_hello},
    protocol::{
        AbortUploadRequest, AttachRequest, AttachResponse, BeginDownloadRequest,
        BeginDownloadResponse, BeginUploadRequest, CloseRequest, CommitUploadRequest,
        FileCapabilitiesRequest, FileCapabilitiesResponse, FileChunkResponse, FileListRequest,
        FileListResponse, FileStatRequest, FileStatResponse, ListRequest, MakeDirectoryRequest,
        QueryUploadRequest, ReadFileChunkRequest, RemoveFileRequest, RenameFileRequest, Request,
        Response, SpawnRequest, UploadStatusResponse, WireMessage, WriteFileChunkRequest,
        read_message, request, response, wire_message, write_message,
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
}

#[derive(Clone)]
struct ReconnectConfig {
    remote: SocketAddr,
    server_name: String,
    trust: ServerTrust,
    identity: PathBuf,
    username: String,
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
        let negotiated =
            authenticate(&connection, &reconnect.identity, &reconnect.username).await?;
        Ok(Self {
            _endpoint: endpoint,
            connection,
            reconnect,
            negotiated,
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

    pub async fn close(&self, terminal_id: String) -> Result<String> {
        let response = self
            .unary(request::Command::Close(CloseRequest {
                terminal_id,
                workspace_id: String::new(),
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
) -> Result<NegotiatedProtocol> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let client_hello = client_hello(username, &ProtocolSupport::command_line_client());
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
