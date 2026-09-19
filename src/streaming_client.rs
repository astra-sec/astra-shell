//! Renderer-independent streaming sessions. One shared client serializes QUIC
//! reconnects; each terminal retains its workspace/terminal/resume identity.
//! Only validated latest state is published, while ACKs and lease renewals run
//! even when the UI does not consume events. Input is never replayed on recovery.

use crate::{
    client::{
        AstraClient, ServerResponseError, StreamingAttachment, StreamingAttachmentEvent,
        TerminalStateDelivery,
    },
    protocol::{AttachRequest, AttachResponse, Resize},
    terminal_state_v2::{HistoryPageRequest, State},
};
use anyhow::{Context, Result, bail, ensure};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Synchronizing,
    Live {
        incarnation: u64,
    },
    Recovering {
        attempt: u32,
        retry_after: Duration,
        reason: String,
    },
}

pub enum LiveTerminalEvent {
    Terminal(StreamingAttachmentEvent),
    Connection(ConnectionState),
}

/// All terminals attached through this object reuse one authenticated QUIC
/// connection, including after reconnection. No reconnection kills a terminal.
#[derive(Clone)]
pub struct StreamingConnection {
    client: Arc<Mutex<AstraClient>>,
}

impl StreamingConnection {
    pub fn new(client: AstraClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }

    pub async fn attach(
        &self,
        mut request: AttachRequest,
    ) -> Result<(LiveTerminal, AttachResponse)> {
        let (attachment, response) = self
            .client
            .lock()
            .await
            .attach_streaming(request.clone())
            .await?;
        request
            .terminal_id
            .clone_from(&response.terminal.as_ref().context("missing terminal")?.id);
        request.resume_token.clone_from(&response.resume_token);
        request.takeover = false; // never take another user's lease during recovery
        let (state_send, state_recv) = watch::channel(None);
        let (status_send, status_recv) = watch::channel(ConnectionState::Synchronizing);
        let (event_send, event_recv) = mpsc::channel(32);
        let (command_send, command_recv) = mpsc::channel(32);
        let client = self.client.clone();
        let task = tokio::spawn(async move {
            let mut driver = Driver {
                client,
                request,
                commands: command_recv,
                states: state_send,
                status: status_send,
                events: event_send,
                size: None,
                incarnation: 1,
            };
            if let Err(error) = driver.run(attachment).await {
                // This is terminal/fatal information, not a frame FIFO. Never
                // block ACK/lease processing behind a non-consuming renderer.
                let _ = driver.events.try_send(Err(error));
            }
        });
        Ok((
            LiveTerminal {
                states: state_recv,
                status: status_recv,
                events: event_recv,
                commands: command_send,
                task,
            },
            response,
        ))
    }
}

type PublishedState = Option<(Arc<State>, TerminalStateDelivery)>;

pub struct LiveTerminal {
    states: watch::Receiver<PublishedState>,
    status: watch::Receiver<ConnectionState>,
    events: mpsc::Receiver<Result<StreamingAttachmentEvent>>,
    commands: mpsc::Sender<Action>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LiveTerminal {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LiveTerminal {
    pub fn connection_state(&self) -> ConnectionState {
        self.status.borrow().clone()
    }
    pub fn current_state(&self) -> Option<Arc<State>> {
        self.states
            .borrow()
            .as_ref()
            .map(|(state, _)| state.clone())
    }

    pub async fn next_event(&mut self) -> Result<LiveTerminalEvent> {
        // Watch receivers are cancellation safe and retain only one state.
        loop {
            tokio::select! {
                biased;
                result = self.states.changed(), if self.states.has_changed().unwrap_or(false) || !self.task.is_finished() => {
                    if result.is_ok()
                        && let Some((state, delivery)) = self.states.borrow_and_update().clone() {
                            return Ok(LiveTerminalEvent::Terminal(StreamingAttachmentEvent::State { state: Box::new((*state).clone()), delivery }));
                    }
                }
                event = self.events.recv() => {
                    return Ok(LiveTerminalEvent::Terminal(event.context("streaming session ended")??));
                }
                result = self.status.changed() => {
                    result.context("streaming session ended")?;
                    return Ok(LiveTerminalEvent::Connection(self.status.borrow_and_update().clone()));
                }
            }
        }
    }

    pub async fn send_input(&self, bytes: Vec<u8>) -> Result<()> {
        ensure!(bytes.len() <= 64 * 1024, "input batch is too large");
        let ConnectionState::Live { incarnation } = self.connection_state() else {
            bail!("disconnected input discarded; input is never replayed");
        };
        self.submit(ActionKind::Input { incarnation, bytes }).await
    }

    pub async fn resize(&self, size: Resize) -> Result<()> {
        self.submit(ActionKind::Resize(size)).await
    }
    pub async fn request_history(&self, request: HistoryPageRequest) -> Result<()> {
        self.submit(ActionKind::History(request)).await
    }
    pub async fn detach(self) -> Result<()> {
        self.submit(ActionKind::Detach).await
    }

    async fn submit(&self, kind: ActionKind) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .try_send(Action { kind, reply })
            .map_err(|_| anyhow::anyhow!("terminal command queue unavailable"))?;
        result.await.context("streaming session ended")?
    }
}

struct Action {
    kind: ActionKind,
    reply: oneshot::Sender<Result<()>>,
}
enum ActionKind {
    Input { incarnation: u64, bytes: Vec<u8> },
    Resize(Resize),
    History(HistoryPageRequest),
    Detach,
}

struct Driver {
    client: Arc<Mutex<AstraClient>>,
    request: AttachRequest,
    commands: mpsc::Receiver<Action>,
    states: watch::Sender<PublishedState>,
    status: watch::Sender<ConnectionState>,
    events: mpsc::Sender<Result<StreamingAttachmentEvent>>,
    size: Option<Resize>,
    incarnation: u64,
}

impl Driver {
    async fn run(&mut self, mut attachment: StreamingAttachment) -> Result<()> {
        loop {
            let resize_error = if let Some(size) = &self.size
                && !self.request.read_only
            {
                attachment.resize(size.clone()).await.err()
            } else {
                None
            };
            let failure = if let Some(error) = resize_error {
                error
            } else {
                loop {
                    tokio::select! {
                        event = attachment.next_event() => match event {
                            Ok(StreamingAttachmentEvent::State { state, delivery }) => {
                                self.states.send_replace(Some((Arc::from(state), delivery)));
                                self.status.send_if_modified(|status| {
                                    let live = ConnectionState::Live { incarnation: self.incarnation };
                                    if *status == live { false } else { *status = live; true }
                                });
                            }
                            Ok(event) => {
                                if let StreamingAttachmentEvent::LeaseChanged(change) = &event {
                                    self.request.read_only = change.read_only;
                                }
                                let exited = matches!(event, StreamingAttachmentEvent::Exited(_));
                                self.events.try_send(Ok(event)).map_err(|_| anyhow::anyhow!("terminal control event consumer is too slow"))?;
                                if exited { return Ok(()); }
                            }
                            Err(error) => break error,
                        },
                        action = self.commands.recv() => {
                            let Some(action) = action else { return Ok(()); };
                            let result = match action.kind {
                                ActionKind::Input { incarnation, bytes } => {
                                    if incarnation != self.incarnation {
                                        let _ = action.reply.send(Err(anyhow::anyhow!("stale input discarded after recovery")));
                                        continue;
                                    }
                                    attachment.send_input(bytes).await
                                }
                                ActionKind::Resize(size) => {
                                    self.size = Some(size.clone());
                                    if self.request.read_only { Ok(()) } else { attachment.resize(size).await }
                                }
                                ActionKind::History(request) => attachment.request_history(request).await,
                                ActionKind::Detach => {
                                    let result = attachment.detach().await;
                                    let _ = action.reply.send(result);
                                    return Ok(());
                                }
                            };
                            let failure = result.as_ref().err().map(|error| anyhow::anyhow!("{error:#}"));
                            let _ = action.reply.send(result);
                            if let Some(error) = failure { break error; }
                        }
                    }
                }
            };
            drop(attachment);
            self.incarnation = self
                .incarnation
                .checked_add(1)
                .context("connection incarnation overflow")?;
            attachment = self.recover(failure).await?;
        }
    }

    async fn recover(&mut self, mut error: anyhow::Error) -> Result<StreamingAttachment> {
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            let delay = retry_delay(attempt);
            self.status.send_replace(ConnectionState::Recovering {
                attempt,
                retry_after: delay,
                reason: format!("{error:#}"),
            });
            // One shared lock covers connection replacement and authentication;
            // the other attachments reuse the replacement, not parallel QUICs.
            let client = self.client.clone();
            let request = self.request.clone();
            let recovery = async move {
                tokio::time::sleep(delay).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut client = client.lock().await;
                    if client.connection_lost() {
                        *client = client.reconnect().await?;
                    }
                    client.attach_streaming(request).await
                })
                .await
                .context("reconnection attempt timed out")?
            };
            tokio::pin!(recovery);
            let outcome = loop {
                tokio::select! {
                    // Drain offline commands before installing a recovered session.
                    biased;
                    action = self.commands.recv() => {
                        let Some(action) = action else { bail!("terminal closed during recovery"); };
                        match action.kind {
                            ActionKind::Resize(size) => { self.size = Some(size); let _ = action.reply.send(Ok(())); }
                            ActionKind::Detach => { let _ = action.reply.send(Ok(())); bail!("terminal detached during recovery"); }
                            _ => { let _ = action.reply.send(Err(anyhow::anyhow!("disconnected command discarded; input is never replayed"))); }
                        }
                    }
                    result = &mut recovery => break result,
                }
            };
            match outcome {
                Ok((attachment, response)) => {
                    self.request.resume_token = response.resume_token;
                    self.status.send_replace(ConnectionState::Synchronizing);
                    return Ok(attachment);
                }
                Err(failure) => {
                    if is_permanent_recovery_error(&failure) {
                        return Err(failure);
                    }
                    error = failure;
                }
            }
        }
    }
}

fn retry_delay(attempt: u32) -> Duration {
    // Per-attachment jitter avoids synchronized retries after Wi-Fi returns.
    let ceiling = 250u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(5))
        .min(5_000);
    Duration::from_millis(rand::random_range(ceiling / 2..=ceiling))
}

fn is_permanent_recovery_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServerResponseError>()
        .is_some_and(|error| {
            !matches!(
                error.code.as_str(),
                "quota_exceeded" | "lease_conflict" | "unavailable" | "busy"
            )
        })
}
