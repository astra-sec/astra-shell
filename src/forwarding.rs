//! FWD-01: a connection-scoped TCP bridge, never tied to a process or terminal.
use crate::{
    negotiation::{CAPABILITY_STREAM_CHANNELS, CAPABILITY_TCP_FORWARD, NegotiatedProtocol},
    protocol::{self, *},
    resources::{ResourceAccount, ResourceClaim},
};
use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

pub const MAX_DATA: usize = 32 * 1024;
pub const MAX_WIRE: usize = MAX_DATA + 64;
pub const BUFFER_RESERVATION: u64 = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// Bound stalled writes; an otherwise idle TCP session may remain connected.
const IO_TIMEOUT: Duration = Duration::from_secs(300);

pub fn validate_open(open: &OpenTcpForwardRequest) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(&open.mapping_id).is_ok(),
        "invalid mapping ID"
    );
    ensure!(
        !open.host.is_empty()
            && open.host.len() <= 253
            && !open
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control()),
        "invalid target host"
    );
    ensure!((1..=65535).contains(&open.port), "invalid target port");
    Ok(())
}

async fn response<W: AsyncWrite + Unpin>(
    send: &mut W,
    id: String,
    result: response::Result,
) -> Result<()> {
    protocol::write_message(
        send,
        &WireMessage::new(wire_message::Body::Response(Response {
            request_id: id,
            result: Some(result),
        })),
    )
    .await
}
async fn reject<W: AsyncWrite + Unpin>(
    send: &mut W,
    id: String,
    code: &str,
    message: String,
) -> Result<()> {
    response(
        send,
        id,
        response::Result::Error(ErrorResponse {
            code: code.into(),
            message,
        }),
    )
    .await?;
    send.shutdown().await?;
    Ok(())
}
pub async fn serve<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    send: &mut W,
    mut recv: R,
    id: String,
    open: OpenTcpForwardRequest,
    negotiated: &NegotiatedProtocol,
    resources: &ResourceAccount,
) -> Result<()> {
    if !negotiated.has(CAPABILITY_TCP_FORWARD, 1) || !negotiated.has(CAPABILITY_STREAM_CHANNELS, 1)
    {
        return reject(
            send,
            id,
            "unsupported",
            "TCP forwarding was not negotiated".into(),
        )
        .await;
    }
    if let Err(error) = validate_open(&open) {
        return reject(send, id, "invalid_target", error.to_string()).await;
    }
    let _reservation = match resources.reserve(ResourceClaim {
        forward_connections: 1,
        forward_buffer_bytes: BUFFER_RESERVATION,
        ..Default::default()
    }) {
        Ok(reservation) => reservation,
        Err(error) => return reject(send, id, "quota", error.to_string()).await,
    };
    // Do not retain a socket across flows. The OS resolves the endpoint anew for
    // every connection, so restarting a service needs no mapping update.
    let connect = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((open.host.as_str(), open.port as u16)),
    );
    let socket = tokio::select! {
        result = connect => match result {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => return reject(send, id, "target_unavailable", error.to_string()).await,
            Err(_) => return reject(send, id, "target_timeout", "Target connection timed out".into()).await,
        },
        _ = protocol::read_message_limited(&mut recv, MAX_WIRE) => bail!("forward cancelled or data sent before acknowledgment"),
    };
    socket.set_nodelay(true)?;
    response(
        send,
        id,
        response::Result::Ack(AckResponse {
            message: "connected".into(),
        }),
    )
    .await?;
    let (mut target_read, mut target_write) = socket.into_split();
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let incoming = async {
        loop {
            let wire = protocol::read_message_limited(&mut recv, MAX_WIRE)
                .await?
                .context("forward ended without EOF")?;
            let Some(wire_message::Body::ForwardFrame(frame)) = wire.body else {
                bail!("expected ForwardFrame")
            };
            ensure!(
                frame.data.len() <= MAX_DATA && (frame.eof == frame.data.is_empty()),
                "invalid forwarding frame"
            );
            if frame.eof {
                target_write.shutdown().await?;
                let _ = eof_tx.send(());
                // Keep reading for cancellation while the target may still be
                // producing its response. Transport FIN is not directional EOF.
                let _ = protocol::read_message_limited(&mut recv, MAX_WIRE).await?;
                bail!("forward ended or sent data after EOF");
            }
            tokio::time::timeout(IO_TIMEOUT, target_write.write_all(&frame.data)).await??;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let outgoing = async {
        let mut buffer = vec![0; MAX_DATA];
        loop {
            let count = target_read.read(&mut buffer).await?;
            let wire = WireMessage::new(wire_message::Body::ForwardFrame(ForwardFrame {
                data: buffer[..count].to_vec(),
                eof: count == 0,
            }));
            tokio::time::timeout(IO_TIMEOUT, protocol::write_message(send, &wire)).await??;
            if count == 0 {
                break;
            }
        }
        eof_rx.await?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! { result = incoming => result?, result = outgoing => result? }
    send.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    fn account() -> ResourceAccount {
        ResourceAccount::standalone("test", crate::resources::ResourceLimits::user_defaults())
            .unwrap()
    }
    fn negotiated() -> NegotiatedProtocol {
        NegotiatedProtocol {
            version: 2,
            capabilities: [
                (CAPABILITY_TCP_FORWARD.into(), 1),
                (CAPABILITY_STREAM_CHANNELS.into(), 1),
            ]
            .into(),
        }
    }
    async fn flow(
        port: u16,
        resources: ResourceAccount,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
        let (client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            serve(
                &mut write,
                read,
                "test".into(),
                OpenTcpForwardRequest {
                    mapping_id: uuid::Uuid::new_v4().to_string(),
                    host: "127.0.0.1".into(),
                    port: port.into(),
                },
                &negotiated(),
                &resources,
            )
            .await
        });
        (client, task)
    }
    async fn frame(stream: &mut tokio::io::DuplexStream, data: Vec<u8>, eof: bool) {
        write_message(
            stream,
            &WireMessage::new(wire_message::Body::ForwardFrame(ForwardFrame { data, eof })),
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn restart_same_endpoint_and_half_close_preserve_response() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        drop(listener);
        let resources = account();
        for marker in [b"first".to_vec(), b"replacement".to_vec()] {
            let listener = TcpListener::bind(address).await?;
            let expected = marker.clone();
            let target = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                let mut input = Vec::new();
                socket.read_to_end(&mut input).await?;
                ensure!(input == vec![42; 32768], "input corrupted");
                socket.write_all(&expected).await?;
                socket.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            });
            let (mut stream, bridge) = flow(address.port(), resources.clone()).await;
            assert!(matches!(
                read_message(&mut stream).await?.unwrap().body,
                Some(wire_message::Body::Response(Response {
                    result: Some(response::Result::Ack(_)),
                    ..
                }))
            ));
            frame(&mut stream, vec![42; 32768], false).await;
            frame(&mut stream, vec![], true).await;
            let mut received = Vec::new();
            loop {
                let Some(wire_message::Body::ForwardFrame(frame)) =
                    read_message(&mut stream).await?.unwrap().body
                else {
                    bail!("unexpected response")
                };
                if frame.eof {
                    break;
                }
                received.extend(frame.data);
            }
            assert_eq!(received, marker);
            bridge.await??;
            target.await??;
            // No listener between service generations: only this flow fails.
            let (mut stream, failed) = flow(address.port(), resources.clone()).await;
            assert!(
                matches!(read_message(&mut stream).await?.unwrap().body, Some(wire_message::Body::Response(Response { result: Some(response::Result::Error(ErrorResponse { code, .. })), .. })) if code == "target_unavailable")
            );
            failed.await??;
        }
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_and_malformed_frames_release_quota() -> Result<()> {
        let mut limits = crate::resources::ResourceLimits::user_defaults();
        limits.forward_connections = 1;
        let resources = ResourceAccount::standalone("one", limits)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        for malformed in [false, true] {
            let (mut stream, bridge) = flow(listener.local_addr()?.port(), resources.clone()).await;
            let (_target, _) = listener.accept().await?;
            read_message(&mut stream).await?;
            let (mut rejected, denied) =
                flow(listener.local_addr()?.port(), resources.clone()).await;
            assert!(
                matches!(read_message(&mut rejected).await?.unwrap().body, Some(wire_message::Body::Response(Response { result: Some(response::Result::Error(ErrorResponse { code, .. })), .. })) if code == "quota")
            );
            denied.await??;
            if malformed {
                frame(&mut stream, vec![1], true).await;
            } else {
                frame(&mut stream, vec![], true).await;
            }
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_secs(2), bridge)
                    .await??
                    .is_err()
            );
            let _available = resources.reserve(ResourceClaim {
                forward_connections: 1,
                ..Default::default()
            })?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn oversized_frame_header_is_rejected_before_payload() -> Result<()> {
        let resources = account();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let (mut stream, bridge) = flow(listener.local_addr()?.port(), resources.clone()).await;
        let (_target, _) = listener.accept().await?;
        read_message(&mut stream).await?;
        stream.write_u32((MAX_WIRE + 1) as u32).await?;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), bridge)
                .await??
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_targets_and_oversized_identifiers() {
        let mut open = OpenTcpForwardRequest {
            mapping_id: uuid::Uuid::new_v4().to_string(),
            host: "localhost".into(),
            port: 3000,
        };
        assert!(validate_open(&open).is_ok());
        open.port = 65536;
        assert!(validate_open(&open).is_err());
        open.port = 3000;
        open.host = "bad host".into();
        assert!(validate_open(&open).is_err());
        open.host = "localhost".into();
        open.mapping_id = "x".repeat(129);
        assert!(validate_open(&open).is_err());
    }
}
