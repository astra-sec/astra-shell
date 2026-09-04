//! Latest-state-wins synchronization for the live terminal viewport.
//!
//! Rust peers negotiate this data plane as `terminal.datagram_state` v1. A
//! reliable semantic keyframe establishes a committed base; cumulative QUIC
//! DATAGRAM deltas then update only the live viewport. Reliable history paging
//! remains independent, and repair/re-key messages guarantee convergence after
//! loss, reordering, or receiver eviction.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{
    protocol::{
        HistoryPageChunk, TerminalStateAck, TerminalStateChunk, TerminalStateDiff,
        TerminalStateRepairRequest, TerminalViewportDatagram,
    },
    terminal_state_v2::{self, HistoryPage, State},
    terminal_sync::{AckDisposition, apply_terminal_state_diff, terminal_state_diff},
};

const DEFAULT_RETAINED_GENERATIONS: usize = 128;
const MAX_TRANSFER_CHUNKS: usize = 4_096;
const INHERIT_STYLES: u32 = 1 << 0;
const INHERIT_HYPERLINKS: u32 = 1 << 1;
const INHERIT_MODES: u32 = 1 << 2;
const INHERIT_TITLE: u32 = 1 << 3;
const INHERIT_WORKING_DIRECTORY: u32 = 1 << 4;
const INHERIT_PALETTE: u32 = 1 << 5;
const KNOWN_INHERIT_MASK: u32 = INHERIT_STYLES
    | INHERIT_HYPERLINKS
    | INHERIT_MODES
    | INHERIT_TITLE
    | INHERIT_WORKING_DIRECTORY
    | INHERIT_PALETTE;

impl TerminalViewportDatagram {
    fn diff(&self) -> Result<&TerminalStateDiff> {
        self.diff
            .as_ref()
            .context("terminal viewport delta has no semantic diff")
    }

    #[cfg(test)]
    fn target_generation(&self) -> Result<u64> {
        Ok(self.diff()?.target_generation)
    }
}

/// A reliable keyframe or a lossy cumulative delta. Deltas always identify an
/// exact retained base generation and never depend on the preceding datagram.
#[derive(Clone)]
pub(crate) enum StreamingUpdate {
    ReliableKeyframe(State),
    DatagramDelta(TerminalViewportDatagram),
}

#[derive(Default)]
pub struct TerminalStateAssembler {
    transfer: TransferAssembler,
}

#[derive(Default)]
pub struct HistoryPageAssembler {
    transfer: TransferAssembler,
}

#[derive(Default)]
struct TransferAssembler {
    pending: Option<PendingTransfer>,
}

struct PendingTransfer {
    transfer_id: Vec<u8>,
    total_size: usize,
    sha256: Vec<u8>,
    chunks: Vec<Option<Vec<u8>>>,
    received_size: usize,
    received_chunks: usize,
}

impl TerminalStateAssembler {
    pub fn push(&mut self, chunk: TerminalStateChunk) -> Result<Option<State>> {
        let encoded = self.transfer.push(
            TransferChunk::from(chunk),
            terminal_state_v2::MAX_ENCODED_STATE_BYTES,
        )?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let state = State::decode(encoded.as_slice())?;
        terminal_state_v2::validate(&state).context("received terminal state is invalid")?;
        Ok(Some(state))
    }
}

impl HistoryPageAssembler {
    pub fn push(&mut self, chunk: HistoryPageChunk) -> Result<Option<HistoryPage>> {
        let encoded = self.transfer.push(
            TransferChunk::from(chunk),
            terminal_state_v2::MAX_ENCODED_HISTORY_PAGE_BYTES,
        )?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let page = HistoryPage::decode(encoded.as_slice())?;
        terminal_state_v2::validate_history_page(&page)
            .context("received terminal history page is invalid")?;
        Ok(Some(page))
    }
}

struct TransferChunk {
    transfer_id: Vec<u8>,
    chunk_index: u32,
    chunk_count: u32,
    total_size: u32,
    sha256: Vec<u8>,
    data: Vec<u8>,
}

impl From<TerminalStateChunk> for TransferChunk {
    fn from(chunk: TerminalStateChunk) -> Self {
        Self {
            transfer_id: chunk.transfer_id,
            chunk_index: chunk.chunk_index,
            chunk_count: chunk.chunk_count,
            total_size: chunk.total_size,
            sha256: chunk.sha256,
            data: chunk.data,
        }
    }
}

impl From<HistoryPageChunk> for TransferChunk {
    fn from(chunk: HistoryPageChunk) -> Self {
        Self {
            transfer_id: chunk.transfer_id,
            chunk_index: chunk.chunk_index,
            chunk_count: chunk.chunk_count,
            total_size: chunk.total_size,
            sha256: chunk.sha256,
            data: chunk.data,
        }
    }
}

impl TransferAssembler {
    fn push(&mut self, chunk: TransferChunk, maximum_size: usize) -> Result<Option<Vec<u8>>> {
        ensure!(
            chunk.transfer_id.len() == 16,
            "terminal transfer ID is invalid"
        );
        ensure!(
            chunk.sha256.len() == 32,
            "terminal transfer digest is invalid"
        );
        let chunk_count = usize::try_from(chunk.chunk_count)?;
        let chunk_index = usize::try_from(chunk.chunk_index)?;
        let total_size = usize::try_from(chunk.total_size)?;
        ensure!(
            (1..=MAX_TRANSFER_CHUNKS).contains(&chunk_count),
            "terminal transfer chunk count is invalid"
        );
        ensure!(
            chunk_index < chunk_count,
            "terminal transfer chunk index is invalid"
        );
        ensure!(total_size <= maximum_size, "terminal transfer is too large");
        if let Some(pending) = &self.pending {
            ensure!(
                pending.transfer_id == chunk.transfer_id,
                "terminal transfer changed before completion"
            );
        } else {
            ensure!(
                chunk_index == 0,
                "terminal transfer started after its first chunk"
            );
            self.pending = Some(PendingTransfer {
                transfer_id: chunk.transfer_id.clone(),
                total_size,
                sha256: chunk.sha256.clone(),
                chunks: vec![None; chunk_count],
                received_size: 0,
                received_chunks: 0,
            });
        }
        let pending = self
            .pending
            .as_mut()
            .context("terminal transfer disappeared")?;
        ensure!(
            pending.transfer_id == chunk.transfer_id
                && pending.total_size == total_size
                && pending.sha256 == chunk.sha256
                && pending.chunks.len() == chunk_count,
            "terminal transfer metadata changed"
        );
        if let Some(existing) = &pending.chunks[chunk_index] {
            ensure!(
                existing == &chunk.data,
                "terminal transfer chunk changed on retransmission"
            );
            return Ok(None);
        }
        pending.received_size = pending
            .received_size
            .checked_add(chunk.data.len())
            .context("terminal transfer size overflowed")?;
        ensure!(
            pending.received_size <= pending.total_size,
            "terminal transfer exceeds declared size"
        );
        pending.received_chunks += 1;
        pending.chunks[chunk_index] = Some(chunk.data);
        if pending.received_chunks != pending.chunks.len() {
            return Ok(None);
        }

        let pending = self
            .pending
            .take()
            .context("terminal transfer disappeared")?;
        let encoded = pending
            .chunks
            .into_iter()
            .map(|chunk| chunk.context("terminal transfer has a missing chunk"))
            .collect::<Result<Vec<_>>>()?
            .concat();
        ensure!(
            encoded.len() == pending.total_size,
            "terminal transfer size does not match"
        );
        ensure!(
            Sha256::digest(&encoded).as_slice() == pending.sha256,
            "terminal transfer digest does not match"
        );
        Ok(Some(encoded))
    }
}

impl StreamingUpdate {
    #[cfg(test)]
    pub(crate) fn target_generation(&self) -> u64 {
        match self {
            Self::ReliableKeyframe(state) => state.generation,
            Self::DatagramDelta(delta) => delta
                .target_generation()
                .expect("constructed viewport delta has a semantic diff"),
        }
    }
}

/// Projects a full semantic state down to the two live viewports. Primary
/// scrollback remains available through the existing reliable history paging
/// protocol and is intentionally excluded from live updates.
pub(crate) fn viewport_state(mut state: State) -> Result<State> {
    terminal_state_v2::validate(&state).context("full terminal state is invalid")?;
    let primary = state
        .primary
        .as_mut()
        .context("primary terminal screen is missing")?;
    let start = usize::try_from(primary.viewport_start)?;
    ensure!(
        start + state.rows as usize <= primary.included_rows.len(),
        "primary viewport is not included in terminal state"
    );
    primary.included_rows = primary.included_rows[start..start + state.rows as usize].to_vec();
    primary.viewport_start = 0;
    primary.included_start = primary
        .included_rows
        .first()
        .and_then(|row| row.start.clone());
    primary.included_end = primary
        .included_rows
        .last()
        .and_then(|row| row.start.clone());
    let mut used_styles = BTreeSet::new();
    let mut used_hyperlinks = BTreeSet::new();
    for screen in [&state.primary, &state.alternate].into_iter().flatten() {
        for cell in screen.included_rows.iter().flat_map(|row| row.cells.iter()) {
            if cell.style_id != 0 {
                used_styles.insert(cell.style_id);
            }
            if cell.hyperlink_id != 0 {
                used_hyperlinks.insert(cell.hyperlink_id);
            }
        }
    }
    state.styles.retain(|style| used_styles.contains(&style.id));
    state
        .hyperlinks
        .retain(|hyperlink| used_hyperlinks.contains(&hyperlink.id));
    validate_viewport_state(&state)?;
    Ok(state)
}

fn validate_viewport_state(state: &State) -> Result<()> {
    terminal_state_v2::validate(state).context("terminal viewport state is invalid")?;
    let expected_rows = state.rows as usize;
    let primary = state
        .primary
        .as_ref()
        .context("primary screen is missing")?;
    let alternate = state
        .alternate
        .as_ref()
        .context("alternate screen is missing")?;
    ensure!(
        primary.viewport_start == 0 && primary.included_rows.len() == expected_rows,
        "primary live state contains scrollback"
    );
    ensure!(
        alternate.viewport_start == 0 && alternate.included_rows.len() == expected_rows,
        "alternate live state is not exactly one viewport"
    );
    Ok(())
}

/// Server-side state window. Unlike `StateSyncWindow`, it can prepare an
/// arbitrary number of newer generations without waiting for an ACK. Every
/// datagram is cumulative from `base`, so losing an intermediate generation
/// does not break the chain.
pub(crate) struct StreamingStateWindow {
    terminal_id: String,
    attachment_id: String,
    base: State,
    base_acknowledged: bool,
    sent: BTreeMap<u64, State>,
    latest_target: Option<State>,
    retained_generations: usize,
}

impl StreamingStateWindow {
    pub(crate) fn with_route(
        initial_keyframe: State,
        terminal_id: String,
        attachment_id: String,
    ) -> Result<Self> {
        ensure!(
            !terminal_id.is_empty() && !attachment_id.is_empty(),
            "terminal viewport datagram route is empty"
        );
        Self::with_retained_generations(
            initial_keyframe,
            terminal_id,
            attachment_id,
            DEFAULT_RETAINED_GENERATIONS,
        )
    }

    fn with_retained_generations(
        initial_keyframe: State,
        terminal_id: String,
        attachment_id: String,
        retained_generations: usize,
    ) -> Result<Self> {
        ensure!(
            retained_generations > 0,
            "retained generation limit is zero"
        );
        validate_viewport_state(&initial_keyframe)?;
        let mut sent = BTreeMap::new();
        sent.insert(initial_keyframe.generation, initial_keyframe.clone());
        Ok(Self {
            terminal_id,
            attachment_id,
            base: initial_keyframe,
            base_acknowledged: false,
            sent,
            latest_target: None,
            retained_generations,
        })
    }

    /// Prepares the newest live state immediately. A delta is used only when
    /// it fits the supplied QUIC DATAGRAM payload budget and is smaller than a
    /// reliable keyframe. Oversized states are coalesced as the latest pending
    /// target; the transport's bounded keyframe timer calls `rekey_latest`
    /// instead of allowing a reliable-stream backlog to grow per generation.
    pub(crate) fn prepare_update(
        &mut self,
        latest: State,
        datagram_payload_budget: usize,
    ) -> Result<Option<StreamingUpdate>> {
        validate_viewport_state(&latest)?;
        if latest.epoch == self.base.epoch && latest.generation <= self.latest_generation() {
            return Ok(None);
        }
        if latest.epoch != self.base.epoch {
            return self.install_keyframe(latest).map(Some);
        }
        if !self.base_acknowledged {
            self.latest_target = Some(latest);
            return Ok(None);
        }

        let update = self.update_from_base(&latest, datagram_payload_budget)?;
        if matches!(update, StreamingUpdate::ReliableKeyframe(_)) {
            self.latest_target = Some(latest);
            return Ok(None);
        }
        self.remember(latest);
        Ok(Some(update))
    }

    /// Rebuilds the latest unacknowledged update from the current base. This
    /// is used by the quiet-tail timer: if the final datagram was lost and no
    /// more PTY output arrives, the last state is still retransmitted.
    pub(crate) fn retry_latest(
        &mut self,
        datagram_payload_budget: usize,
    ) -> Result<Option<StreamingUpdate>> {
        let Some(latest) = self.latest_target.clone() else {
            return Ok(None);
        };
        if latest.epoch == self.base.epoch && latest.generation <= self.base.generation {
            self.latest_target = None;
            return Ok(None);
        }
        if latest.epoch != self.base.epoch {
            return self.install_keyframe(latest).map(Some);
        }
        if !self.base_acknowledged {
            return Ok(None);
        }
        let update = self.update_from_base(&latest, datagram_payload_budget)?;
        if matches!(update, StreamingUpdate::ReliableKeyframe(_)) {
            return self.install_keyframe(latest).map(Some);
        }
        Ok(Some(update))
    }

    /// Promotes the newest unacknowledged state to a reliable keyframe. The
    /// transport calls this after its bounded datagram retry interval expires,
    /// or when the peer reports that the named base is no longer retained.
    pub(crate) fn rekey_latest(&mut self) -> Result<Option<StreamingUpdate>> {
        let Some(latest) = self.latest_target.clone() else {
            return Ok(None);
        };
        if latest.epoch == self.base.epoch && latest.generation <= self.base.generation {
            self.latest_target = None;
            return Ok(None);
        }
        self.install_keyframe(latest).map(Some)
    }

    pub(crate) fn repair(
        &mut self,
        request: &TerminalStateRepairRequest,
    ) -> Result<Option<StreamingUpdate>> {
        ensure!(
            request.epoch.len() == terminal_state_v2::EPOCH_BYTES,
            "terminal state repair epoch is invalid"
        );
        ensure!(
            request.missing_base_generation > 0,
            "terminal state repair base generation is zero"
        );
        ensure!(
            request.newest_seen_generation >= request.missing_base_generation,
            "terminal state repair generation range is reversed"
        );
        ensure!(
            request.epoch == self.base.epoch,
            "terminal state repair epoch changed"
        );
        ensure!(
            request.missing_base_generation <= self.latest_generation(),
            "terminal state repair names a future base"
        );
        match self.rekey_latest()? {
            Some(update) => Ok(Some(update)),
            None => {
                self.base_acknowledged = false;
                Ok(Some(StreamingUpdate::ReliableKeyframe(self.base.clone())))
            }
        }
    }

    pub(crate) fn has_pending_update(&self) -> bool {
        self.latest_target.as_ref().is_some_and(|state| {
            state.epoch != self.base.epoch || state.generation > self.base.generation
        })
    }

    pub(crate) fn base_is_acknowledged(&self) -> bool {
        self.base_acknowledged
    }

    /// Accepts an ACK for any retained generation, not just a single in-flight
    /// frame. Advancing the base makes following cumulative deltas smaller.
    pub(crate) fn acknowledge(&mut self, ack: &TerminalStateAck) -> Result<AckDisposition> {
        ensure!(
            ack.epoch.len() == terminal_state_v2::EPOCH_BYTES,
            "terminal state ACK epoch is invalid"
        );
        ensure!(ack.generation > 0, "terminal state ACK generation is zero");
        if ack.epoch == self.base.epoch && ack.generation <= self.base.generation {
            if ack.generation == self.base.generation {
                self.base_acknowledged = true;
            }
            if self
                .latest_target
                .as_ref()
                .is_some_and(|state| state.generation <= ack.generation)
            {
                self.latest_target = None;
            }
            return Ok(AckDisposition::Duplicate);
        }
        ensure!(
            ack.epoch == self.base.epoch,
            "terminal state ACK epoch changed"
        );
        let Some(acknowledged) = self.sent.get(&ack.generation).cloned() else {
            // A delayed ACK may name a generation evicted from the bounded
            // sender window. It cannot advance the base, but it is still a
            // valid stale observation rather than a reason to kill the
            // attachment. Generations beyond anything prepared remain a
            // protocol error.
            ensure!(
                ack.generation <= self.latest_generation(),
                "terminal state ACK names a future generation"
            );
            return Ok(AckDisposition::Duplicate);
        };
        self.base = acknowledged;
        self.base_acknowledged = true;
        self.sent
            .retain(|generation, _| *generation >= ack.generation);
        if self
            .latest_target
            .as_ref()
            .is_some_and(|state| state.generation <= ack.generation)
        {
            self.latest_target = None;
        }
        Ok(AckDisposition::Accepted)
    }

    fn update_from_base(
        &self,
        latest: &State,
        datagram_payload_budget: usize,
    ) -> Result<StreamingUpdate> {
        let delta =
            compact_viewport_datagram(&self.terminal_id, &self.attachment_id, &self.base, latest)?;
        if delta.encoded_len() <= datagram_payload_budget
            && delta.encoded_len() < latest.encoded_len()
        {
            Ok(StreamingUpdate::DatagramDelta(delta))
        } else {
            Ok(StreamingUpdate::ReliableKeyframe(latest.clone()))
        }
    }

    fn install_keyframe(&mut self, state: State) -> Result<StreamingUpdate> {
        validate_viewport_state(&state)?;
        self.base = state.clone();
        self.base_acknowledged = false;
        self.sent.clear();
        self.remember(state.clone());
        Ok(StreamingUpdate::ReliableKeyframe(state))
    }

    fn remember(&mut self, state: State) {
        self.latest_target = Some(state.clone());
        self.sent.insert(state.generation, state);
        while self.sent.len() > self.retained_generations {
            let Some(oldest) = self.sent.keys().next().copied() else {
                break;
            };
            if oldest == self.base.generation {
                let Some(next) = self
                    .sent
                    .range((oldest + 1)..)
                    .next()
                    .map(|(generation, _)| *generation)
                else {
                    break;
                };
                self.sent.remove(&next);
            } else {
                self.sent.remove(&oldest);
            }
        }
    }

    fn latest_generation(&self) -> u64 {
        self.latest_target
            .as_ref()
            .map_or(self.base.generation, |state| state.generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applied,
    Stale,
    MissingBase,
}

/// Client-side atomic replica used by the experiment. It retains recent bases
/// so a newer cumulative datagram can be applied even when earlier datagrams
/// were lost or arrive later.
pub struct StreamingReplica {
    terminal_id: String,
    attachment_id: String,
    current: Option<State>,
    retained: BTreeMap<u64, State>,
    pinned_base_generation: Option<u64>,
    retained_generations: usize,
}

impl Default for StreamingReplica {
    fn default() -> Self {
        Self {
            terminal_id: String::new(),
            attachment_id: String::new(),
            current: None,
            retained: BTreeMap::new(),
            pinned_base_generation: None,
            retained_generations: DEFAULT_RETAINED_GENERATIONS,
        }
    }
}

impl StreamingReplica {
    pub fn for_route(terminal_id: String, attachment_id: String) -> Result<Self> {
        ensure!(
            !terminal_id.is_empty() && !attachment_id.is_empty(),
            "terminal viewport datagram route is empty"
        );
        Ok(Self {
            terminal_id,
            attachment_id,
            ..Self::default()
        })
    }

    #[cfg(test)]
    pub(crate) fn apply(&mut self, update: &StreamingUpdate) -> Result<ApplyDisposition> {
        match update {
            StreamingUpdate::ReliableKeyframe(state) => self.apply_keyframe(state.clone()),
            StreamingUpdate::DatagramDelta(delta) => self.apply_datagram(delta),
        }
    }

    pub fn current(&self) -> Option<&State> {
        self.current.as_ref()
    }

    pub fn state_ack(&self) -> Option<TerminalStateAck> {
        self.current.as_ref().map(|state| TerminalStateAck {
            epoch: state.epoch.clone(),
            generation: state.generation,
        })
    }

    pub fn repair_request(
        &self,
        datagram: &TerminalViewportDatagram,
    ) -> Result<TerminalStateRepairRequest> {
        ensure!(
            datagram.terminal_id == self.terminal_id
                && datagram.attachment_id == self.attachment_id,
            "terminal viewport datagram targets the wrong attachment"
        );
        let diff = datagram.diff()?;
        let newest_seen_generation = self
            .current
            .as_ref()
            .filter(|state| state.epoch == diff.epoch)
            .map_or(diff.target_generation, |state| {
                state.generation.max(diff.target_generation)
            });
        Ok(TerminalStateRepairRequest {
            epoch: diff.epoch.clone(),
            missing_base_generation: diff.base_generation,
            newest_seen_generation,
        })
    }

    pub fn apply_keyframe(&mut self, state: State) -> Result<ApplyDisposition> {
        validate_viewport_state(&state)?;
        if self.current.as_ref().is_some_and(|current| {
            current.epoch == state.epoch && current.generation >= state.generation
        }) {
            return Ok(ApplyDisposition::Stale);
        }
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.epoch != state.epoch)
        {
            self.retained.clear();
        }
        self.pinned_base_generation = Some(state.generation);
        self.remember(state.clone());
        self.current = Some(state);
        Ok(ApplyDisposition::Applied)
    }

    pub fn apply_datagram(&mut self, delta: &TerminalViewportDatagram) -> Result<ApplyDisposition> {
        ensure!(
            delta.terminal_id == self.terminal_id && delta.attachment_id == self.attachment_id,
            "terminal viewport datagram targets the wrong attachment"
        );
        let diff = delta.diff()?;
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.epoch != diff.epoch)
        {
            return Ok(ApplyDisposition::Stale);
        }
        if self.current.as_ref().is_some_and(|current| {
            current.epoch == diff.epoch && current.generation >= diff.target_generation
        }) {
            return Ok(ApplyDisposition::Stale);
        }
        let Some(base) = self.retained.get(&diff.base_generation) else {
            return Ok(ApplyDisposition::MissingBase);
        };
        if base.epoch != diff.epoch {
            return Ok(ApplyDisposition::MissingBase);
        }
        let target = apply_compact_viewport_delta(base, delta)?;
        validate_viewport_state(&target)?;
        self.pinned_base_generation = Some(diff.base_generation);
        self.remember(target.clone());
        self.current = Some(target);
        Ok(ApplyDisposition::Applied)
    }

    fn remember(&mut self, state: State) {
        self.retained.insert(state.generation, state);
        while self.retained.len() > self.retained_generations {
            let removable = self
                .retained
                .keys()
                .copied()
                .find(|generation| Some(*generation) != self.pinned_base_generation);
            let Some(generation) = removable else {
                break;
            };
            self.retained.remove(&generation);
        }
    }
}

fn compact_viewport_datagram(
    terminal_id: &str,
    attachment_id: &str,
    base: &State,
    target: &State,
) -> Result<TerminalViewportDatagram> {
    ensure!(
        !terminal_id.is_empty() && !attachment_id.is_empty(),
        "terminal viewport datagram route is empty"
    );
    let mut diff = terminal_state_diff(base, target)?;
    let metadata = diff
        .target_metadata
        .as_mut()
        .context("terminal state diff target metadata is missing")?;
    let mut inherited_fields = 0;
    if metadata.styles == base.styles {
        metadata.styles.clear();
        inherited_fields |= INHERIT_STYLES;
    }
    if metadata.hyperlinks == base.hyperlinks {
        metadata.hyperlinks.clear();
        inherited_fields |= INHERIT_HYPERLINKS;
    }
    if metadata.modes == base.modes {
        metadata.modes = None;
        inherited_fields |= INHERIT_MODES;
    }
    if metadata.title == base.title {
        metadata.title.clear();
        inherited_fields |= INHERIT_TITLE;
    }
    if metadata.working_directory == base.working_directory {
        metadata.working_directory.clear();
        inherited_fields |= INHERIT_WORKING_DIRECTORY;
    }
    if metadata.palette == base.palette {
        metadata.palette = None;
        inherited_fields |= INHERIT_PALETTE;
    }
    Ok(TerminalViewportDatagram {
        terminal_id: terminal_id.to_owned(),
        attachment_id: attachment_id.to_owned(),
        diff: Some(diff),
        inherited_fields,
    })
}

fn apply_compact_viewport_delta(base: &State, delta: &TerminalViewportDatagram) -> Result<State> {
    ensure!(
        delta.inherited_fields & !KNOWN_INHERIT_MASK == 0,
        "terminal viewport delta has unknown inherited fields"
    );
    let mut diff = delta.diff()?.clone();
    let metadata = diff
        .target_metadata
        .as_mut()
        .context("terminal viewport delta target metadata is missing")?;
    if delta.inherited_fields & INHERIT_STYLES != 0 {
        ensure!(
            metadata.styles.is_empty(),
            "inherited styles are also encoded"
        );
        metadata.styles = base.styles.clone();
    }
    if delta.inherited_fields & INHERIT_HYPERLINKS != 0 {
        ensure!(
            metadata.hyperlinks.is_empty(),
            "inherited hyperlinks are also encoded"
        );
        metadata.hyperlinks = base.hyperlinks.clone();
    }
    if delta.inherited_fields & INHERIT_MODES != 0 {
        ensure!(metadata.modes.is_none(), "inherited modes are also encoded");
        metadata.modes = base.modes.clone();
    }
    if delta.inherited_fields & INHERIT_TITLE != 0 {
        ensure!(metadata.title.is_empty(), "inherited title is also encoded");
        metadata.title.clone_from(&base.title);
    }
    if delta.inherited_fields & INHERIT_WORKING_DIRECTORY != 0 {
        ensure!(
            metadata.working_directory.is_empty(),
            "inherited working directory is also encoded"
        );
        metadata
            .working_directory
            .clone_from(&base.working_directory);
    }
    if delta.inherited_fields & INHERIT_PALETTE != 0 {
        ensure!(
            metadata.palette.is_none(),
            "inherited palette is also encoded"
        );
        metadata.palette = base.palette.clone();
    }
    apply_terminal_state_diff(base, &diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_engine::TerminalEngine;

    const DATAGRAM_BUDGET: usize = 1_200;
    const TERMINAL_ID: &str = "terminal-1";
    const ATTACHMENT_ID: &str = "attachment-1";

    fn viewport(engine: &mut TerminalEngine) -> State {
        viewport_state(engine.semantic_state().unwrap()).unwrap()
    }

    fn sender(initial: State) -> StreamingStateWindow {
        let ack = TerminalStateAck {
            epoch: initial.epoch.clone(),
            generation: initial.generation,
        };
        let mut sender = StreamingStateWindow::with_route(
            initial,
            TERMINAL_ID.to_owned(),
            ATTACHMENT_ID.to_owned(),
        )
        .unwrap();
        sender.acknowledge(&ack).unwrap();
        sender
    }

    fn receiver() -> StreamingReplica {
        StreamingReplica::for_route(TERMINAL_ID.to_owned(), ATTACHMENT_ID.to_owned()).unwrap()
    }

    fn state_chunks(state: &State, chunk_bytes: usize) -> Vec<TerminalStateChunk> {
        let encoded = state.encode_to_vec();
        let digest = Sha256::digest(&encoded).to_vec();
        let transfer_id = vec![9; 16];
        let chunk_count = encoded.len().div_ceil(chunk_bytes) as u32;
        encoded
            .chunks(chunk_bytes)
            .enumerate()
            .map(|(index, data)| TerminalStateChunk {
                transfer_id: transfer_id.clone(),
                chunk_index: index as u32,
                chunk_count,
                total_size: encoded.len() as u32,
                sha256: digest.clone(),
                data: data.to_vec(),
            })
            .collect()
    }

    fn history_chunks(page: &HistoryPage, chunk_bytes: usize) -> Vec<HistoryPageChunk> {
        let encoded = page.encode_to_vec();
        let digest = Sha256::digest(&encoded).to_vec();
        let transfer_id = vec![7; 16];
        let chunk_count = encoded.len().div_ceil(chunk_bytes) as u32;
        encoded
            .chunks(chunk_bytes)
            .enumerate()
            .map(|(index, data)| HistoryPageChunk {
                transfer_id: transfer_id.clone(),
                chunk_index: index as u32,
                chunk_count,
                total_size: encoded.len() as u32,
                sha256: digest.clone(),
                data: data.to_vec(),
            })
            .collect()
    }

    #[test]
    fn reliable_keyframe_assembler_is_atomic_and_order_tolerant() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        engine.advance(b"assembled state");
        let state = viewport(&mut engine);
        let chunks = state_chunks(&state, state.encoded_len().div_ceil(3));
        assert!(chunks.len() >= 3);

        let mut assembler = TerminalStateAssembler::default();
        assert!(assembler.push(chunks[0].clone()).unwrap().is_none());
        assert!(assembler.push(chunks[2].clone()).unwrap().is_none());
        let mut completed = None;
        for chunk in chunks.iter().skip(1).filter(|chunk| chunk.chunk_index != 2) {
            completed = assembler.push(chunk.clone()).unwrap().or(completed);
        }
        assert_eq!(completed.as_ref(), Some(&state));
    }

    #[test]
    fn reliable_keyframe_assembler_rejects_corruption_and_metadata_changes() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        engine.advance(b"validated state");
        let state = viewport(&mut engine);
        let chunks = state_chunks(&state, state.encoded_len().div_ceil(2));

        let mut changed = chunks[0].clone();
        let mut assembler = TerminalStateAssembler::default();
        assembler.push(changed.clone()).unwrap();
        changed.data.push(0);
        assert!(assembler.push(changed).is_err());

        let mut assembler = TerminalStateAssembler::default();
        assembler.push(chunks[0].clone()).unwrap();
        let mut replacement = chunks[0].clone();
        replacement.transfer_id = vec![8; 16];
        assert!(assembler.push(replacement).is_err());

        let mut corrupted = state_chunks(&state, state.encoded_len());
        corrupted[0].sha256[0] ^= 0xff;
        assert!(
            TerminalStateAssembler::default()
                .push(corrupted.remove(0))
                .is_err()
        );
    }

    #[test]
    fn reliable_history_assembler_validates_and_publishes_one_complete_page() {
        let mut engine = TerminalEngine::new(2, 20, 32, Box::new(std::io::sink())).unwrap();
        engine.advance(b"zero\r\none\r\ntwo\r\nthree\r\nfour");
        let state = engine.semantic_state().unwrap();
        let page = engine
            .history_page(
                11,
                &terminal_state_v2::HistoryPageRequest {
                    epoch: state.epoch,
                    before: state.primary.unwrap().included_start,
                    maximum_rows: 3,
                },
            )
            .unwrap();
        let chunks = history_chunks(&page, page.encoded_len().div_ceil(2));
        let mut assembler = HistoryPageAssembler::default();
        let mut completed = None;
        for chunk in chunks {
            completed = assembler.push(chunk).unwrap().or(completed);
        }
        assert_eq!(completed.as_ref(), Some(&page));
    }

    #[test]
    fn live_keyframe_excludes_scrollback() {
        let mut engine = TerminalEngine::new(24, 80, 512, Box::new(std::io::sink())).unwrap();
        for index in 0..400 {
            engine.advance(format!("history {index:03}\r\n").as_bytes());
        }
        let full = engine.semantic_state().unwrap();
        let live = viewport_state(full.clone()).unwrap();
        assert!(full.primary.as_ref().unwrap().included_rows.len() > full.rows as usize);
        assert_eq!(
            live.primary.as_ref().unwrap().included_rows.len(),
            live.rows as usize
        );
        assert_eq!(live.primary.as_ref().unwrap().viewport_start, 0);
        assert!(live.encoded_len() * 4 < full.encoded_len());
        println!(
            "terminal-streaming keyframe: full={}B viewport={}B reduction={:.1}%",
            full.encoded_len(),
            live.encoded_len(),
            100.0 * (full.encoded_len() - live.encoded_len()) as f64 / full.encoded_len() as f64
        );
    }

    #[test]
    fn live_keyframe_tables_only_describe_visible_cells() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        for index in 0..80 {
            let red = index * 3 % 256;
            let green = index * 5 % 256;
            let blue = index * 7 % 256;
            engine.advance(
                format!("\x1b[38;2;{red};{green};{blue}mhistory {index}\x1b[0m\r\n").as_bytes(),
            );
        }
        engine.advance(b"\x1b[0mvisible");
        let full = engine.semantic_state().unwrap();
        let live = viewport_state(full.clone()).unwrap();
        assert!(full.styles.len() > live.styles.len());

        let referenced_styles = live
            .primary
            .iter()
            .chain(live.alternate.iter())
            .flat_map(|screen| screen.included_rows.iter())
            .flat_map(|row| row.cells.iter())
            .filter_map(|cell| (cell.style_id != 0).then_some(cell.style_id))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            live.styles
                .iter()
                .map(|style| style.id)
                .collect::<BTreeSet<_>>(),
            referenced_styles
        );
    }

    #[test]
    fn unchanged_metadata_is_not_repeated_in_each_datagram() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let base = viewport(&mut engine);
        engine.advance(b"x");
        let target = viewport(&mut engine);
        let original = terminal_state_diff(&base, &target).unwrap();
        let compact =
            compact_viewport_datagram(TERMINAL_ID, ATTACHMENT_ID, &base, &target).unwrap();
        let restored = apply_compact_viewport_delta(&base, &compact).unwrap();
        assert_eq!(restored, target);
        assert!(compact.encoded_len() <= DATAGRAM_BUDGET);
        assert!(compact.encoded_len() * 2 < original.encoded_len());
        println!(
            "terminal-streaming delta: original={}B compact-with-route={}B reduction={:.1}%",
            original.encoded_len(),
            compact.encoded_len(),
            100.0 * (original.encoded_len() - compact.encoded_len()) as f64
                / original.encoded_len() as f64
        );
    }

    #[test]
    fn sender_produces_new_generations_without_waiting_for_ack() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial.clone());
        let mut generations = Vec::new();
        let mut bases = Vec::new();
        for text in [b"a".as_slice(), b"b", b"c", b"d"] {
            engine.advance(text);
            let update = sender
                .prepare_update(viewport(&mut engine), DATAGRAM_BUDGET)
                .unwrap()
                .unwrap();
            generations.push(update.target_generation());
            let StreamingUpdate::DatagramDelta(delta) = update else {
                panic!("small viewport update should fit one datagram")
            };
            bases.push(delta.diff().unwrap().base_generation);
        }
        assert!(generations.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(bases.iter().all(|base| *base == initial.generation));
    }

    #[test]
    fn deltas_wait_only_for_the_reliable_keyframe_commit() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = StreamingStateWindow::with_route(
            initial.clone(),
            TERMINAL_ID.to_owned(),
            ATTACHMENT_ID.to_owned(),
        )
        .unwrap();
        engine.advance(b"arrived before keyframe commit");
        assert!(
            sender
                .prepare_update(viewport(&mut engine), DATAGRAM_BUDGET)
                .unwrap()
                .is_none()
        );
        assert!(sender.has_pending_update());
        sender
            .acknowledge(&TerminalStateAck {
                epoch: initial.epoch,
                generation: initial.generation,
            })
            .unwrap();
        assert!(sender.base_is_acknowledged());
        assert!(
            sender
                .retry_latest(DATAGRAM_BUDGET)
                .unwrap()
                .is_some_and(|update| matches!(update, StreamingUpdate::DatagramDelta(_)))
        );
    }

    #[test]
    fn dropped_and_reordered_deltas_still_converge_to_latest_state() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial.clone());
        let mut receiver = receiver();
        assert_eq!(
            receiver
                .apply(&StreamingUpdate::ReliableKeyframe(initial.clone()))
                .unwrap(),
            ApplyDisposition::Applied
        );

        let mut updates = Vec::new();
        for text in [b"one".as_slice(), b" two", b" three", b" four"] {
            engine.advance(text);
            updates.push(
                sender
                    .prepare_update(viewport(&mut engine), DATAGRAM_BUDGET)
                    .unwrap()
                    .unwrap(),
            );
        }
        let expected = viewport(&mut engine);

        // Simulate generations 1 and 3 being lost, then generation 2 arriving
        // after generation 4. Only the newest complete state should commit.
        assert_eq!(
            receiver.apply(&updates[3]).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(
            receiver.apply(&updates[1]).unwrap(),
            ApplyDisposition::Stale
        );
        assert_eq!(receiver.current(), Some(&expected));
    }

    #[test]
    fn every_bounded_loss_subset_converges_when_the_latest_delta_arrives() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial.clone());
        let mut updates = Vec::new();
        for text in ["a", "b", "c", "d", "e", "f", "g", "latest"] {
            engine.advance(text.as_bytes());
            updates.push(
                sender
                    .prepare_update(viewport(&mut engine), DATAGRAM_BUDGET)
                    .unwrap()
                    .unwrap(),
            );
        }
        let expected = viewport(&mut engine);
        let latest = updates.last().unwrap();

        // Exhaust all 2^7 subsets of the intermediate generations. Deliver
        // each selected subset newest-first and duplicate it, then deliver the
        // latest cumulative delta. No pattern may corrupt or block convergence.
        for mask in 0_u16..(1 << (updates.len() - 1)) {
            let mut receiver = receiver();
            receiver
                .apply(&StreamingUpdate::ReliableKeyframe(initial.clone()))
                .unwrap();
            for (index, update) in updates[..updates.len() - 1].iter().enumerate().rev() {
                if mask & (1 << index) != 0 {
                    receiver.apply(update).unwrap();
                    receiver.apply(update).unwrap();
                }
            }
            receiver.apply(latest).unwrap();
            assert_eq!(receiver.current(), Some(&expected), "loss mask {mask:#09b}");
        }
    }

    #[test]
    fn ack_can_advance_the_base_while_newer_generations_are_in_flight() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial.clone());
        let mut receiver = receiver();
        receiver
            .apply(&StreamingUpdate::ReliableKeyframe(initial))
            .unwrap();

        engine.advance(b"one");
        let first_state = viewport(&mut engine);
        let first = sender
            .prepare_update(first_state.clone(), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();
        assert_eq!(receiver.apply(&first).unwrap(), ApplyDisposition::Applied);

        engine.advance(b" two");
        let _newer_in_flight = sender
            .prepare_update(viewport(&mut engine), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();
        assert_eq!(
            sender
                .acknowledge(&TerminalStateAck {
                    epoch: first_state.epoch.clone(),
                    generation: first_state.generation,
                })
                .unwrap(),
            AckDisposition::Accepted
        );

        engine.advance(b" three");
        let expected = viewport(&mut engine);
        let newest = sender
            .prepare_update(expected.clone(), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();
        let StreamingUpdate::DatagramDelta(delta) = &newest else {
            panic!("small update should remain a datagram")
        };
        assert_eq!(
            delta.diff().unwrap().base_generation,
            first_state.generation
        );
        assert_eq!(receiver.apply(&newest).unwrap(), ApplyDisposition::Applied);
        assert_eq!(receiver.current(), Some(&expected));
    }

    #[test]
    fn delayed_ack_for_an_evicted_generation_is_harmless() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let epoch = initial.epoch.clone();
        let mut sender = StreamingStateWindow::with_retained_generations(
            initial.clone(),
            TERMINAL_ID.to_owned(),
            ATTACHMENT_ID.to_owned(),
            2,
        )
        .unwrap();
        sender
            .acknowledge(&TerminalStateAck {
                epoch: epoch.clone(),
                generation: initial.generation,
            })
            .unwrap();

        engine.advance(b"one");
        let first = viewport(&mut engine);
        let evicted_generation = first.generation;
        sender.prepare_update(first, DATAGRAM_BUDGET).unwrap();
        engine.advance(b"two");
        let second = viewport(&mut engine);
        sender.prepare_update(second, DATAGRAM_BUDGET).unwrap();

        assert_eq!(
            sender
                .acknowledge(&TerminalStateAck {
                    epoch: epoch.clone(),
                    generation: evicted_generation,
                })
                .unwrap(),
            AckDisposition::Duplicate
        );
        let future_generation = sender.latest_generation() + 1;
        assert!(
            sender
                .acknowledge(&TerminalStateAck {
                    epoch,
                    generation: future_generation,
                })
                .is_err()
        );
    }

    #[test]
    fn quiet_tail_retry_recovers_the_last_lost_datagram() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial.clone());
        let mut receiver = receiver();
        receiver
            .apply(&StreamingUpdate::ReliableKeyframe(initial))
            .unwrap();

        engine.advance(b"last output before silence");
        let expected = viewport(&mut engine);
        let dropped = sender
            .prepare_update(expected.clone(), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();
        assert!(matches!(dropped, StreamingUpdate::DatagramDelta(_)));

        let retry = sender.retry_latest(DATAGRAM_BUDGET).unwrap().unwrap();
        assert_eq!(receiver.apply(&retry).unwrap(), ApplyDisposition::Applied);
        assert_eq!(receiver.current(), Some(&expected));
        let ack = TerminalStateAck {
            epoch: expected.epoch.clone(),
            generation: expected.generation,
        };
        assert_eq!(sender.acknowledge(&ack).unwrap(), AckDisposition::Accepted);
        assert!(sender.retry_latest(DATAGRAM_BUDGET).unwrap().is_none());
    }

    #[test]
    fn reliable_rekey_recovers_when_the_receiver_no_longer_has_the_base() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial);
        engine.advance(b"new state");
        let expected = viewport(&mut engine);
        let delta = sender
            .prepare_update(expected.clone(), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();

        let mut receiver = receiver();
        assert_eq!(
            receiver.apply(&delta).unwrap(),
            ApplyDisposition::MissingBase
        );
        let keyframe = sender.rekey_latest().unwrap().unwrap();
        assert!(matches!(keyframe, StreamingUpdate::ReliableKeyframe(_)));
        assert_eq!(
            receiver.apply(&keyframe).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(receiver.current(), Some(&expected));
    }

    #[test]
    fn explicit_missing_base_repair_immediately_returns_a_reliable_keyframe() {
        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial);
        engine.advance(b"repair target");
        let expected = viewport(&mut engine);
        let delta = sender
            .prepare_update(expected.clone(), DATAGRAM_BUDGET)
            .unwrap()
            .unwrap();
        let StreamingUpdate::DatagramDelta(datagram) = delta else {
            panic!("small update should remain a datagram")
        };

        let receiver = receiver();
        let request = receiver.repair_request(&datagram).unwrap();
        let repaired = sender.repair(&request).unwrap().unwrap();
        let StreamingUpdate::ReliableKeyframe(state) = repaired else {
            panic!("repair did not produce a reliable keyframe")
        };
        assert_eq!(state, expected);
        assert!(!sender.base_is_acknowledged());
    }

    #[test]
    fn oversized_updates_coalesce_until_the_reliable_keyframe_tick() {
        let mut engine = TerminalEngine::new(24, 80, 128, Box::new(std::io::sink())).unwrap();
        let initial = viewport(&mut engine);
        let mut sender = sender(initial);
        for index in 0..24 {
            engine
                .advance(format!("row {index:02} with a large replacement payload\r\n").as_bytes());
        }
        let expected = viewport(&mut engine);
        assert!(
            sender
                .prepare_update(expected.clone(), 64)
                .unwrap()
                .is_none()
        );
        engine.advance(b"newer state replaces the pending keyframe");
        let newest = viewport(&mut engine);
        assert!(sender.prepare_update(newest.clone(), 64).unwrap().is_none());
        let keyframe = sender.rekey_latest().unwrap().unwrap();
        let StreamingUpdate::ReliableKeyframe(state) = keyframe else {
            panic!("keyframe timer did not promote the pending state")
        };
        assert_eq!(state, newest);
        assert_ne!(state, expected);
    }

    #[tokio::test]
    async fn latest_delta_round_trips_over_a_real_quic_datagram() -> Result<()> {
        use std::{net::SocketAddr, sync::Arc};

        use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let certificate = cert.der().clone();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        let server_config =
            quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?));
        let server_endpoint =
            quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>()?)?;

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(certificate.to_vec()))?;
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client_config =
            quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_tls)?));
        let mut client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse::<SocketAddr>()?)?;
        client_endpoint.set_default_client_config(client_config);

        let server_address = server_endpoint.local_addr()?;
        let client_connecting = client_endpoint.connect(server_address, "localhost")?;
        let server_incoming = server_endpoint
            .accept()
            .await
            .context("server endpoint closed before accepting")?;
        let (client_connection, server_connection) =
            tokio::try_join!(client_connecting, server_incoming)?;
        let payload_budget = server_connection
            .max_datagram_size()
            .context("QUIC endpoint did not negotiate datagram support")?;

        let mut engine = TerminalEngine::new(6, 40, 128, Box::new(std::io::sink()))?;
        let initial = viewport(&mut engine);
        let mut sender = StreamingStateWindow::with_route(
            initial.clone(),
            TERMINAL_ID.to_owned(),
            ATTACHMENT_ID.to_owned(),
        )?;
        sender.acknowledge(&TerminalStateAck {
            epoch: initial.epoch.clone(),
            generation: initial.generation,
        })?;
        let mut receiver =
            StreamingReplica::for_route(TERMINAL_ID.to_owned(), ATTACHMENT_ID.to_owned())?;
        receiver.apply(&StreamingUpdate::ReliableKeyframe(initial))?;

        // Omit three earlier updates to model loss before the transport. The
        // fourth update must still reconstruct because it is cumulative.
        for text in [b"lost-1 ".as_slice(), b"lost-2 ", b"lost-3 "] {
            engine.advance(text);
            let _ = sender.prepare_update(viewport(&mut engine), payload_budget)?;
        }
        engine.advance(b"delivered");
        let expected = viewport(&mut engine);
        let update = sender
            .prepare_update(expected.clone(), payload_budget)?
            .context("latest viewport update was not prepared")?;
        let StreamingUpdate::DatagramDelta(delta) = update else {
            anyhow::bail!("loopback delta did not fit negotiated QUIC datagram size")
        };
        let encoded = delta.encode_to_vec();
        assert!(encoded.len() <= payload_budget);
        server_connection.send_datagram(encoded.clone().into())?;
        let received = client_connection.read_datagram().await?;
        let decoded = TerminalViewportDatagram::decode(received)?;
        assert_eq!(
            receiver.apply(&StreamingUpdate::DatagramDelta(decoded))?,
            ApplyDisposition::Applied
        );
        assert_eq!(receiver.current(), Some(&expected));
        println!(
            "terminal-streaming QUIC datagram: negotiated={}B payload={}B generation={}",
            payload_budget,
            encoded.len(),
            expected.generation
        );

        client_connection.close(0_u32.into(), b"test complete");
        server_connection.close(0_u32.into(), b"test complete");
        Ok(())
    }
}
