//! A bounded nonblocking handoff to one reliable stream writer. The attachment
//! state machine must continue reading input/ACK/lease commands while the peer
//! applies transport backpressure. State keyframes are additionally ACK-gated.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

// One maximum state, one final state at exit, and bounded control/history data.
const MAX_QUEUED_BYTES: usize = 20 * 1024 * 1024;

/// A handler error can itself have queued a final protocol Error event. Drain
/// it before returning; a sink error, however, cancels the handler immediately.
pub(crate) async fn run_with_drain(
    handler: impl Future<Output = anyhow::Result<()>>,
    drain: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    tokio::pin!(handler, drain);
    tokio::select! {
        result = &mut handler => { drain.await?; result }
        result = &mut drain => { result?; handler.await }
    }
}

pub(crate) struct QueuedWriter {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
}

pub(crate) fn queued_writer<W: AsyncWrite + Unpin>(
    writer: &mut W,
) -> (QueuedWriter, impl Future<Output = anyhow::Result<()>> + '_) {
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(1024);
    let queued = Arc::new(AtomicUsize::new(0));
    let producer = QueuedWriter {
        sender: Some(sender),
        queued: queued.clone(),
    };
    let drain = async move {
        while let Some(bytes) = receiver.recv().await {
            writer.write_all(&bytes).await?;
            queued.fetch_sub(bytes.len(), Ordering::AcqRel);
        }
        writer.shutdown().await?;
        Ok(())
    };
    (producer, drain)
}

pub(crate) fn owned_writer<W: AsyncWrite + Unpin + Send + 'static>(
    mut writer: W,
) -> (QueuedWriter, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(1024);
    let queued = Arc::new(AtomicUsize::new(0));
    let producer = QueuedWriter {
        sender: Some(sender),
        queued: queued.clone(),
    };
    let task = tokio::spawn(async move {
        while let Some(bytes) = receiver.recv().await {
            writer.write_all(&bytes).await?;
            queued.fetch_sub(bytes.len(), Ordering::AcqRel);
        }
        writer.shutdown().await?;
        Ok(())
    });
    (producer, task)
}

impl AsyncWrite for QueuedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Some(sender) = &self.sender else {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        };
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes.len())
                    .filter(|n| *n <= MAX_QUEUED_BYTES)
            })
            .is_err()
        {
            // Excessive reliable control/history traffic must not consume
            // unbounded memory or suspend the lease/control task indefinitely.
            return Poll::Ready(Err(io::Error::other(
                "reliable attachment output budget exceeded",
            )));
        }
        if sender.try_send(bytes.to_vec()).is_err() {
            self.queued.fetch_sub(bytes.len(), Ordering::AcqRel);
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.sender.as_ref().is_none_or(|s| s.is_closed()) {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.sender.take();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_blocked_sink_does_not_block_producer_and_drains_in_order() {
        let (mut sink, mut peer) = tokio::io::duplex(1);
        let (mut producer, drain) = queued_writer(&mut sink);
        let send = async {
            producer.write_all(b"hello").await.unwrap();
            producer.write_all(b"world").await.unwrap();
            producer.shutdown().await.unwrap();
            // Neither enqueue waited for the peer to read even one byte.
        };
        tokio::time::timeout(std::time::Duration::from_millis(100), send)
            .await
            .unwrap();
        let receive = async {
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            bytes
        };
        let (result, bytes) = tokio::join!(drain, receive);
        result.unwrap();
        assert_eq!(bytes, b"helloworld");

        let (mut sink, mut peer) = tokio::io::duplex(1);
        let (mut producer, drain) = queued_writer(&mut sink);
        let handler = async move {
            producer.write_all(b"protocol error").await?;
            anyhow::bail!("handler failed");
        };
        let receive = async {
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            bytes
        };
        let (result, bytes) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(run_with_drain(handler, drain), receive)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().to_string(), "handler failed");
        assert_eq!(bytes, b"protocol error");
    }
}
