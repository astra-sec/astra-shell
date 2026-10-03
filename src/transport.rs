//! Transport boundary for Astra's length-prefixed protocol streams.
//!
//! The application owns framing, negotiation and channel semantics. Adapters
//! provide ordered byte streams and optional unreliable datagrams; they must not
//! interpret terminal, session or file messages. Quinn is the current adapter,
//! not a type exposed by the application-facing connection/stream API.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::Result;
use prost::bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub type TransportConnection = Arc<dyn FramedTransport>;
pub type BidirectionalStream = (FramedSendStream, FramedRecvStream);

/// A multiplexed connection on which Astra's existing framing is carried.
/// Implementations must make `accept_bi`, `open_bi` and `read_datagram`
/// cancellation-safe, and closing a connection must wake its pending I/O.
pub trait FramedTransport: Send + Sync {
    fn open_bi(&self) -> TransportFuture<'_, BidirectionalStream>;
    /// A peer's application-level connection close is normal end-of-input.
    fn accept_bi(&self) -> TransportFuture<'_, Option<BidirectionalStream>>;
    fn read_datagram(&self) -> TransportFuture<'_, Bytes>;
    fn send_datagram(&self, payload: Bytes) -> Result<DatagramDisposition>;
    fn max_datagram_size(&self) -> Option<usize>;
    fn close(&self, code: u32, reason: &[u8]);
    fn is_closed(&self) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramDisposition {
    Queued,
    /// Path MTU/support changed. Existing latest-state/repair logic treats this
    /// as packet loss; a lost connection is still returned as an error.
    Dropped,
}

/// Optional stream controls. Priority is advisory, never application ordering.
pub trait TransportWrite: AsyncWrite + Unpin + Send {
    fn set_priority(&self, _priority: i32) -> Result<()> {
        Ok(())
    }

    /// Wait for delivery/stop if the transport supports that distinction.
    /// `None` does not promise a peer application acknowledgment.
    fn stopped(&mut self) -> TransportFuture<'_, Option<u64>> {
        Box::pin(async { Ok(None) })
    }
}

pub struct FramedSendStream(Box<dyn TransportWrite>);

impl FramedSendStream {
    pub fn new(stream: impl TransportWrite + 'static) -> Self {
        Self(Box::new(stream))
    }

    /// End this sending direction without closing the peer's receive direction.
    pub async fn finish(&mut self) -> Result<()> {
        self.shutdown().await?;
        Ok(())
    }

    pub fn set_priority(&self, priority: i32) -> Result<()> {
        self.0.set_priority(priority)
    }

    pub fn stopped(&mut self) -> TransportFuture<'_, Option<u64>> {
        self.0.stopped()
    }
}

impl AsyncWrite for FramedSendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

pub struct FramedRecvStream {
    reader: Box<dyn AsyncRead + Unpin + Send>,
    diagnostic_id: Option<u64>,
}

impl FramedRecvStream {
    pub fn new(reader: impl AsyncRead + Unpin + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            diagnostic_id: None,
        }
    }

    /// A backend stream ID is diagnostic only, never a channel/terminal handle.
    pub fn diagnostic_id(&self) -> Option<u64> {
        self.diagnostic_id
    }
}

impl AsyncRead for FramedRecvStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.reader).poll_read(cx, buffer)
    }
}

/// Quinn-specific connection adapter. TLS/endpoint setup lives at the client
/// and listener boundaries; after setup business code receives only the trait.
pub struct QuinnTransport {
    connection: quinn::Connection,
}

impl QuinnTransport {
    pub fn new(connection: quinn::Connection) -> Self {
        Self { connection }
    }

    pub fn into_connection(self) -> TransportConnection {
        Arc::new(self)
    }
}

fn quinn_stream((send, recv): (quinn::SendStream, quinn::RecvStream)) -> BidirectionalStream {
    let diagnostic_id = Some(u64::from(recv.id()));
    let mut recv = FramedRecvStream::new(recv);
    recv.diagnostic_id = diagnostic_id;
    (FramedSendStream::new(send), recv)
}

impl FramedTransport for QuinnTransport {
    fn open_bi(&self) -> TransportFuture<'_, BidirectionalStream> {
        Box::pin(async { Ok(quinn_stream(self.connection.open_bi().await?)) })
    }

    fn accept_bi(&self) -> TransportFuture<'_, Option<BidirectionalStream>> {
        Box::pin(async {
            match self.connection.accept_bi().await {
                Ok(stream) => Ok(Some(quinn_stream(stream))),
                Err(quinn::ConnectionError::ApplicationClosed { .. }) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn read_datagram(&self) -> TransportFuture<'_, Bytes> {
        Box::pin(async { Ok(self.connection.read_datagram().await?) })
    }

    fn send_datagram(&self, payload: Bytes) -> Result<DatagramDisposition> {
        match self.connection.send_datagram(payload) {
            Ok(()) => Ok(DatagramDisposition::Queued),
            Err(
                quinn::SendDatagramError::TooLarge
                | quinn::SendDatagramError::UnsupportedByPeer
                | quinn::SendDatagramError::Disabled,
            ) => Ok(DatagramDisposition::Dropped),
            Err(quinn::SendDatagramError::ConnectionLost(error)) => Err(error.into()),
        }
    }

    fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    fn close(&self, code: u32, reason: &[u8]) {
        self.connection.close(code.into(), reason);
    }

    fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }
}

impl TransportWrite for quinn::SendStream {
    fn set_priority(&self, priority: i32) -> Result<()> {
        quinn::SendStream::set_priority(self, priority)?;
        Ok(())
    }

    fn stopped(&mut self) -> TransportFuture<'_, Option<u64>> {
        Box::pin(async { Ok(quinn::SendStream::stopped(self).await?.map(u64::from)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AuthResult, WireMessage, read_message, wire_message, write_message};

    impl TransportWrite for tokio::io::WriteHalf<tokio::io::DuplexStream> {}

    #[tokio::test]
    async fn framed_protocol_works_without_quinn_and_preserves_half_close() -> Result<()> {
        // A deliberately tiny pipe forces partial reads/writes and backpressure.
        let (left, right) = tokio::io::duplex(3);
        let (left_recv, left_send) = tokio::io::split(left);
        let (right_recv, right_send) = tokio::io::split(right);
        let mut left_send = FramedSendStream::new(left_send);
        let mut left_recv = FramedRecvStream::new(left_recv);
        let mut right_send = FramedSendStream::new(right_send);
        let mut right_recv = FramedRecvStream::new(right_recv);
        let message = WireMessage::new(wire_message::Body::AuthResult(AuthResult {
            ok: true,
            message: "transport-independent framing".into(),
            error_code: String::new(),
            stream_epoch: 0,
        }));
        let client = async {
            write_message(&mut left_send, &message).await?;
            left_send.finish().await?;
            assert_eq!(read_message(&mut left_recv).await?, Some(message.clone()));
            assert!(read_message(&mut left_recv).await?.is_none());
            Ok::<_, anyhow::Error>(())
        };
        let server = async {
            assert_eq!(read_message(&mut right_recv).await?, Some(message.clone()));
            assert!(read_message(&mut right_recv).await?.is_none());
            write_message(&mut right_send, &message).await?;
            right_send.finish().await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::try_join!(client, server)
        })
        .await??;
        Ok(())
    }
}
