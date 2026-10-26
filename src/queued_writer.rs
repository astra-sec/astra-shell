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
    sync::{Notify, mpsc},
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
    drained: Arc<Notify>,
}

impl QueuedWriter {
    /// Wait outside the input handler branches; this is transport backpressure,
    /// not a terminal ACK or another state buffer.
    pub(crate) async fn wait_until_drained(&self) {
        loop {
            let notified = self.drained.notified();
            if self.queued.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) fn queued_writer<W: AsyncWrite + Unpin>(
    writer: &mut W,
) -> (QueuedWriter, impl Future<Output = anyhow::Result<()>> + '_) {
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(1024);
    let queued = Arc::new(AtomicUsize::new(0));
    let drained = Arc::new(Notify::new());
    let producer = QueuedWriter {
        sender: Some(sender),
        queued: queued.clone(),
        drained: drained.clone(),
    };
    let drain = async move {
        while let Some(bytes) = receiver.recv().await {
            writer.write_all(&bytes).await?;
            if queued.fetch_sub(bytes.len(), Ordering::AcqRel) == bytes.len() {
                drained.notify_one();
            }
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
    let drained = Arc::new(Notify::new());
    let producer = QueuedWriter {
        sender: Some(sender),
        queued: queued.clone(),
        drained: drained.clone(),
    };
    let task = tokio::spawn(async move {
        while let Some(bytes) = receiver.recv().await {
            writer.write_all(&bytes).await?;
            if queued.fetch_sub(bytes.len(), Ordering::AcqRel) == bytes.len() {
                drained.notify_one();
            }
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
        // Use the stable CAS primitive across toolchains that spell the update
        // helper differently; preserve the same checked limit and memory ordering.
        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            let Some(next) = current
                .checked_add(bytes.len())
                .filter(|n| *n <= MAX_QUEUED_BYTES)
            else {
                // Reject excess traffic instead of blocking input/ACK/lease work.
                return Poll::Ready(Err(io::Error::other(
                    "reliable attachment output budget exceeded",
                )));
            };
            match self.queued.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
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
    async fn drain_gate_is_cancel_safe_and_does_not_claim_peer_acknowledgment() {
        let (mut sink, mut peer) = tokio::io::duplex(1);
        let (mut producer, drain) = queued_writer(&mut sink);
        producer.write_all(b"frame").await.unwrap();
        // Cancelling this scheduling wait leaves the input handler free to run.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                producer.wait_until_drained()
            )
            .await
            .is_err()
        );
        let handler = async {
            producer.wait_until_drained().await;
            assert_eq!(producer.queued.load(Ordering::Acquire), 0);
            producer.shutdown().await.unwrap();
        };
        let receive = async {
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"frame");
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let (result, (), ()) = tokio::join!(drain, handler, receive);
            result.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn exact_byte_budget_rejects_excess_and_releases_after_drain() {
        let mut sink = tokio::io::sink();
        let (mut producer, drain) = queued_writer(&mut sink);
        let usage = producer.queued.clone();
        producer
            .write_all(&vec![0; MAX_QUEUED_BYTES])
            .await
            .unwrap();
        let error = producer.write_all(&[1]).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "reliable attachment output budget exceeded"
        );
        assert_eq!(usage.load(Ordering::Acquire), MAX_QUEUED_BYTES);
        producer.shutdown().await.unwrap();
        drain.await.unwrap();
        assert_eq!(usage.load(Ordering::Acquire), 0);
    }

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
