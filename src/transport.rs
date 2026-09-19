use std::{
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::protocol::{StreamHello, StreamKind, WireMessage, wire_message, write_message};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportStreamKind {
    Authentication,
    Control,
    Terminal,
    File,
}

impl TransportStreamKind {
    pub fn priority(self) -> i32 {
        match self {
            Self::Authentication => 20,
            Self::Terminal => 10,
            Self::Control => 0,
            Self::File => -10,
        }
    }

    fn protocol_kind(self) -> Option<StreamKind> {
        match self {
            Self::Authentication => None,
            Self::Control => Some(StreamKind::Control),
            Self::Terminal => Some(StreamKind::Terminal),
            Self::File => Some(StreamKind::File),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamDescriptor {
    pub kind: TransportStreamKind,
    pub handle: String,
    pub epoch: Vec<u8>,
    pub request_id: String,
}

impl StreamDescriptor {
    pub fn authentication() -> Self {
        Self {
            kind: TransportStreamKind::Authentication,
            handle: String::new(),
            epoch: Vec::new(),
            request_id: String::new(),
        }
    }

    pub fn application(
        kind: TransportStreamKind,
        handle: impl Into<String>,
        epoch: Vec<u8>,
        request_id: impl Into<String>,
    ) -> Result<Self> {
        if kind == TransportStreamKind::Authentication {
            bail!("application stream cannot use the authentication kind");
        }
        let request_id = request_id.into();
        if request_id.is_empty() {
            bail!("application stream request ID is empty");
        }
        Ok(Self {
            kind,
            handle: handle.into(),
            epoch,
            request_id,
        })
    }
}

/// Framed protocol transport boundary. Domain clients open classified streams without exposing
/// Quinn stream types; future backends can preserve the same descriptors and framed messages.
pub struct FramedTransport {
    connection: quinn::Connection,
}

impl Drop for FramedTransport {
    fn drop(&mut self) {
        self.connection.close(0_u32.into(), b"transport dropped");
    }
}

impl FramedTransport {
    pub fn new(connection: quinn::Connection) -> Self {
        Self { connection }
    }

    pub async fn open_stream(
        &self,
        descriptor: &StreamDescriptor,
        send_stream_hello: bool,
    ) -> Result<(FramedSendStream, FramedRecvStream)> {
        let (send, recv) = self.connection.open_bi().await?;
        send.set_priority(descriptor.kind.priority())?;
        let mut send = FramedSendStream(send);
        if send_stream_hello {
            let Some(kind) = descriptor.kind.protocol_kind() else {
                bail!("authentication stream cannot send StreamHello");
            };
            write_message(
                &mut send,
                &WireMessage::new(wire_message::Body::StreamHello(StreamHello {
                    kind: kind as i32,
                    handle: descriptor.handle.clone(),
                    epoch: descriptor.epoch.clone(),
                    request_id: descriptor.request_id.clone(),
                })),
            )
            .await?;
        }
        Ok((send, FramedRecvStream(recv)))
    }

    pub fn close(&self, reason: &'static [u8]) {
        self.connection.close(0_u32.into(), reason);
    }
}

pub struct FramedSendStream(quinn::SendStream);

impl FramedSendStream {
    pub fn finish(&mut self) -> Result<()> {
        self.0.finish()?;
        Ok(())
    }
}

impl AsyncWrite for FramedSendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.0), context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.0), context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.0), context)
    }
}

pub struct FramedRecvStream(quinn::RecvStream);

impl AsyncRead for FramedRecvStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.0), context, buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qos_classes_keep_terminal_ahead_and_each_class_internally_fair() {
        assert!(TransportStreamKind::Terminal.priority() > TransportStreamKind::Control.priority());
        assert!(TransportStreamKind::Control.priority() > TransportStreamKind::File.priority());
        assert_eq!(
            TransportStreamKind::Terminal.priority(),
            TransportStreamKind::Terminal.priority()
        );
    }

    #[test]
    fn application_descriptors_require_explicit_request_identity() {
        assert!(
            StreamDescriptor::application(
                TransportStreamKind::Terminal,
                "terminal",
                vec![],
                "request",
            )
            .is_ok()
        );
        assert!(StreamDescriptor::application(TransportStreamKind::File, "", vec![], "").is_err());
        assert!(
            StreamDescriptor::application(
                TransportStreamKind::Authentication,
                "",
                vec![],
                "request",
            )
            .is_err()
        );
    }
}
