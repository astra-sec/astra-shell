use std::{fs, net::SocketAddr, path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::CertificateDer;

use crate::{
    ALPN, PROTOCOL_VERSION,
    auth::{authentication_payload, sign_challenge},
    protocol::{
        AttachRequest, AttachResponse, CloseRequest, ListRequest, Request, Response, SpawnRequest,
        WireMessage, read_message, request, response, wire_message, write_message,
    },
};

pub struct AstraClient {
    _endpoint: quinn::Endpoint,
    connection: quinn::Connection,
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
        server_certificate: &Path,
        identity: &Path,
        username: &str,
    ) -> Result<Self> {
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
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(tls).context("invalid QUIC client TLS configuration")?,
        ));
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
        client_config.transport_config(Arc::new(transport));
        let bind: SocketAddr = if remote.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let mut endpoint = quinn::Endpoint::client(bind)?;
        endpoint.set_default_client_config(client_config);
        let connection = endpoint
            .connect(remote, server_name)?
            .await
            .with_context(|| format!("failed to connect to {remote}"))?;
        authenticate(&connection, identity, username).await?;
        Ok(Self {
            _endpoint: endpoint,
            connection,
        })
    }

    pub async fn list(&self) -> Result<Vec<crate::protocol::TerminalInfo>> {
        let response = self.unary(request::Command::List(ListRequest {})).await?;
        match response.result {
            Some(response::Result::List(list)) => Ok(list.terminals),
            Some(response::Result::Error(error)) => {
                bail!("{}: {}", error.code, error.message)
            }
            _ => bail!("server returned the wrong response to list"),
        }
    }

    pub async fn spawn(&self, request: SpawnRequest) -> Result<crate::protocol::TerminalInfo> {
        let response = self.unary(request::Command::Spawn(request)).await?;
        match response.result {
            Some(response::Result::Spawn(spawn)) => spawn
                .terminal
                .context("server returned an empty spawn response"),
            Some(response::Result::Error(error)) => {
                bail!("{}: {}", error.code, error.message)
            }
            _ => bail!("server returned the wrong response to spawn"),
        }
    }

    pub async fn close(&self, terminal_id: String) -> Result<String> {
        let response = self
            .unary(request::Command::Close(CloseRequest { terminal_id }))
            .await?;
        match response.result {
            Some(response::Result::Ack(ack)) => Ok(ack.message),
            Some(response::Result::Error(error)) => {
                bail!("{}: {}", error.code, error.message)
            }
            _ => bail!("server returned the wrong response to close"),
        }
    }

    pub async fn attach(
        &self,
        terminal_id: String,
        read_only: bool,
        takeover: bool,
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
                })),
            })),
        )
        .await?;
        let response = require_response(&mut recv, &request_id).await?;
        match response.result {
            Some(response::Result::Attach(attach)) => Ok((send, recv, attach)),
            Some(response::Result::Error(error)) => {
                bail!("{}: {}", error.code, error.message)
            }
            _ => bail!("server returned the wrong response to attach"),
        }
    }

    async fn unary(&self, command: request::Command) -> Result<Response> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
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

async fn authenticate(
    connection: &quinn::Connection,
    identity: &Path,
    username: &str,
) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_message(
        &mut send,
        &WireMessage::new(wire_message::Body::ClientHello(
            crate::protocol::ClientHello {
                protocol_version: PROTOCOL_VERSION,
                username: username.into(),
            },
        )),
    )
    .await?;
    let hello = match read_message(&mut recv).await? {
        Some(WireMessage {
            body: Some(wire_message::Body::ServerHello(hello)),
        }) => hello,
        _ => bail!("server did not send ServerHello"),
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!(
            "server protocol version {} is incompatible with client version {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        )
    }
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
        }) if result.ok => Ok(()),
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
