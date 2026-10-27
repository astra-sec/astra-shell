//! Bounded, independently encoded cumulative viewport transfers for v3.
//! No retry queue is owned here: each prepared body is emitted and dropped.

use anyhow::{Context, Result, ensure};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::protocol::{
    TerminalViewportDatagram, TerminalViewportFragment, TerminalViewportFragmentEncoding,
};

pub(crate) const MAX_WORKING_BYTES: usize = 65_535;
pub(crate) const MAX_FRAGMENTS: u32 = 128;
const ID_DOMAIN: &[u8] = b"astra-viewport-v3";

#[derive(Clone)]
pub(crate) struct ViewportTransfer {
    terminal_id: String,
    attachment_id: String,
    header: TerminalViewportFragment,
    encoded: Vec<u8>,
}

impl ViewportTransfer {
    /// `None` means the existing reliable keyframe path must be used. The
    /// comparison includes every outer fragment byte and uses the existing
    /// raw keyframe size criterion; it does not recompress a second State.
    pub(crate) fn prepare(
        delta: TerminalViewportDatagram,
        payload_budget: usize,
        keyframe_bytes: usize,
    ) -> Result<Option<Self>> {
        let raw_length = delta.encoded_len();
        if raw_length == 0 || raw_length >= MAX_WORKING_BYTES || payload_budget == 0 {
            return Ok(None);
        }
        ensure!(delta.fragment.is_none(), "nested viewport fragment");
        ensure!(
            !delta.terminal_id.is_empty() && !delta.attachment_id.is_empty(),
            "empty viewport route"
        );
        let diff = delta
            .diff
            .as_ref()
            .context("viewport transfer has no diff")?;
        ensure!(
            diff.epoch.len() == 16
                && diff.base_generation > 0
                && diff.target_generation > diff.base_generation,
            "invalid viewport transfer generation"
        );
        let maximum_encoded = (MAX_WORKING_BYTES - raw_length - 1) / 2;
        if maximum_encoded == 0 {
            return Ok(None);
        }
        let raw = delta.encode_to_vec();
        let digest = Sha256::digest(&raw).to_vec();
        let compressed = if raw.len() >= 1024 {
            // Never allocate compress_bound(raw) before checking admission: an
            // incompressible body could exceed the shared scratch budget.
            let mut bytes = vec![0; maximum_encoded];
            match zstd::bulk::Compressor::new(1)?.compress_to_buffer(&raw, bytes.as_mut_slice()) {
                Ok(size) => {
                    bytes.truncate(size);
                    Some(bytes)
                }
                // Compression is optional. A bounded destination that cannot
                // hold it falls back to raw (if admitted) or a reliable State.
                Err(_) => None,
            }
        } else {
            None
        };
        let (encoding, encoded) = match compressed {
            Some(bytes)
                if bytes.len().saturating_add(64.max(raw.len().div_ceil(20))) <= raw.len() =>
            {
                (TerminalViewportFragmentEncoding::Zstd, bytes)
            }
            _ => (TerminalViewportFragmentEncoding::Raw, raw),
        };
        if !working_bytes_fit(raw_length, encoded.len()) {
            return Ok(None);
        }
        let mut transfer = Self {
            terminal_id: delta.terminal_id.clone(),
            attachment_id: delta.attachment_id.clone(),
            header: TerminalViewportFragment {
                epoch: diff.epoch.clone(),
                base_generation: diff.base_generation,
                target_generation: diff.target_generation,
                transfer_id: vec![0; 16],
                index: 0,
                count: 1,
                total_encoded: u32::try_from(encoded.len())?,
                raw_length: u32::try_from(raw_length)?,
                encoding: encoding as i32,
                sha256: digest,
                data: Vec::new(),
            },
            encoded,
        };
        for count in 1..=MAX_FRAGMENTS.min(transfer.header.total_encoded) {
            transfer.header.count = count;
            let mut wire_bytes = 0usize;
            let mut fits = true;
            for index in 0..count {
                let size = transfer.packet_len(index);
                if size > payload_budget {
                    fits = false;
                    break;
                }
                wire_bytes = wire_bytes
                    .checked_add(size)
                    .context("viewport wire size overflow")?;
            }
            if fits {
                if wire_bytes >= keyframe_bytes {
                    return Ok(None);
                }
                transfer.header.transfer_id = transfer_id(&transfer.header);
                return Ok(Some(transfer));
            }
        }
        Ok(None)
    }

    pub(crate) fn count(&self) -> u32 {
        self.header.count
    }

    #[cfg(test)]
    pub(crate) fn target_generation(&self) -> u64 {
        self.header.target_generation
    }

    pub(crate) fn route(&self) -> (&str, &str) {
        (&self.terminal_id, &self.attachment_id)
    }

    pub(crate) fn packet(&self, index: u32) -> TerminalViewportDatagram {
        assert!(index < self.header.count);
        let mut fragment = self.header.clone();
        fragment.index = index;
        let range = balanced_range(index, self.header.count, self.encoded.len());
        fragment.data = self.encoded[range].to_vec();
        TerminalViewportDatagram {
            terminal_id: self.terminal_id.clone(),
            attachment_id: self.attachment_id.clone(),
            fragment: Some(Box::new(fragment)),
            ..Default::default()
        }
    }

    fn packet_len(&self, index: u32) -> usize {
        // Count exact protobuf framing without allocating prospective payloads.
        // Field8 and field11 tags both occupy one byte; index0 is omitted.
        let mut header = self.header.clone();
        header.index = index;
        let data_length = balanced_range(index, header.count, self.encoded.len()).len();
        let inner = header.encoded_len() + 1 + varint_len(data_length) + data_length;
        let outer_route = 1
            + varint_len(self.terminal_id.len())
            + self.terminal_id.len()
            + 1
            + varint_len(self.attachment_id.len())
            + self.attachment_id.len();
        outer_route + 1 + varint_len(inner) + inner
    }

    #[cfg(test)]
    pub(crate) fn decode_for_test(&self) -> Result<TerminalViewportDatagram> {
        let payload_encoding =
            match TerminalViewportFragmentEncoding::try_from(self.header.encoding)? {
                TerminalViewportFragmentEncoding::Raw => crate::protocol::PayloadEncoding::None,
                TerminalViewportFragmentEncoding::Zstd => crate::protocol::PayloadEncoding::Zstd,
            };
        let raw = crate::compression::EncodedPayload {
            data: self.encoded.clone(),
            encoding: payload_encoding as i32,
            uncompressed_size: if payload_encoding == crate::protocol::PayloadEncoding::Zstd {
                self.header.raw_length
            } else {
                0
            },
        }
        .decode(true, MAX_WORKING_BYTES, &self.header.sha256)?;
        ensure!(
            raw.len() == self.header.raw_length as usize,
            "wrong raw length"
        );
        Ok(TerminalViewportDatagram::decode(raw.as_slice())?)
    }
}

fn working_bytes_fit(raw: usize, encoded: usize) -> bool {
    raw > 0
        && encoded > 0
        && encoded
            .checked_mul(2)
            .and_then(|value| value.checked_add(raw))
            .and_then(|value| value.checked_add(1))
            .is_some_and(|value| value <= MAX_WORKING_BYTES)
}

fn balanced_range(index: u32, count: u32, total: usize) -> std::ops::Range<usize> {
    // The sender establishes count<=128 and total<=65535 before this call.
    (index as usize * total / count as usize)..((index as usize + 1) * total / count as usize)
}

fn transfer_id(header: &TerminalViewportFragment) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(ID_DOMAIN);
    hash.update(&header.sha256);
    for value in [
        header.encoding as u32,
        header.total_encoded,
        header.raw_length,
        header.count,
    ] {
        hash.update(value.to_be_bytes());
    }
    hash.finalize()[..16].to_vec()
}

fn varint_len(mut value: usize) -> usize {
    let mut bytes = 1;
    while value >= 128 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TerminalStateDiff;

    fn delta(title: String) -> TerminalViewportDatagram {
        TerminalViewportDatagram {
            terminal_id: "11111111-1111-4111-8111-111111111111".into(),
            attachment_id: "22222222-2222-4222-8222-222222222222".into(),
            diff: Some(TerminalStateDiff {
                epoch: vec![3; 16],
                base_generation: 7,
                target_generation: 9,
                target_metadata: Some(crate::terminal_state_v2::State {
                    title,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn balanced_intervals_and_checked_scratch_bound() {
        assert_eq!(
            (0..3).map(|i| balanced_range(i, 3, 10)).collect::<Vec<_>>(),
            vec![0..3, 3..6, 6..10]
        );
        assert!(working_bytes_fit(65532, 1));
        assert!(!working_bytes_fit(65533, 1));
        assert!(!working_bytes_fit(usize::MAX, 1));
        assert!(!working_bytes_fit(1, usize::MAX));
        assert!(!working_bytes_fit(0, 1));
        assert!(!working_bytes_fit(1, 0));
    }

    #[test]
    fn raw_and_zstd_packets_obey_exact_budget_and_round_trip() {
        for input in [delta("raw".into()), delta("compressible".repeat(300))] {
            for budget in [256, 512, 1200] {
                let transfer = ViewportTransfer::prepare(input.clone(), budget, usize::MAX)
                    .unwrap()
                    .unwrap();
                assert_eq!(transfer.decode_for_test().unwrap(), input);
                let mut body = Vec::new();
                for i in 0..transfer.count() {
                    let packet = transfer.packet(i);
                    assert_eq!(packet.encoded_len(), transfer.packet_len(i));
                    assert!(packet.encoded_len() <= budget);
                    let part = packet.fragment.unwrap();
                    assert_eq!(part.transfer_id, transfer_id(&part));
                    body.extend_from_slice(&part.data);
                }
                assert_eq!(body, transfer.encoded);
                if transfer.count() > 1 {
                    let mut less = transfer.clone();
                    less.header.count -= 1;
                    assert!(
                        (0..less.count()).any(|i| less.packet_len(i) > budget),
                        "not smallest count"
                    );
                }
            }
        }
    }

    #[test]
    fn identity_is_deterministic_and_binds_layout() {
        let input = delta("deterministic".repeat(25));
        let a = ViewportTransfer::prepare(input.clone(), 256, usize::MAX)
            .unwrap()
            .unwrap();
        let b = ViewportTransfer::prepare(input.clone(), 256, usize::MAX)
            .unwrap()
            .unwrap();
        let c = ViewportTransfer::prepare(input, 1200, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(a.packet(0), b.packet(0));
        assert_ne!(a.header.count, c.header.count);
        assert_ne!(a.header.transfer_id, c.header.transfer_id);
        let mut changed = a.header.clone();
        changed.raw_length += 1;
        assert_ne!(transfer_id(&changed), a.header.transfer_id);
    }

    #[test]
    fn maximum_fragment_count_and_varint_lengths_are_exact() {
        let input = delta("x".repeat(700));
        let mut transfer = ViewportTransfer::prepare(input, 1200, usize::MAX)
            .unwrap()
            .unwrap();
        transfer.header.count = MAX_FRAGMENTS;
        transfer.header.transfer_id = transfer_id(&transfer.header);
        let mut bytes = Vec::new();
        for index in 0..MAX_FRAGMENTS {
            let packet = transfer.packet(index);
            assert_eq!(packet.encoded_len(), transfer.packet_len(index));
            let fragment = packet.fragment.unwrap();
            assert!(!fragment.data.is_empty());
            bytes.extend_from_slice(&fragment.data);
        }
        assert_eq!(bytes, transfer.encoded);
    }

    fn noisy_title(length: usize) -> String {
        let mut seed = 0x12345678u32;
        (0..length)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                char::from(32 + (seed % 95) as u8)
            })
            .collect()
    }

    #[test]
    fn excessive_fragment_count_and_scratch_need_use_reliable_fallback() {
        let input = delta(noisy_title(8000));
        let transfer = ViewportTransfer::prepare(input.clone(), 1200, usize::MAX)
            .unwrap()
            .unwrap();
        let mut beyond = transfer.clone();
        beyond.header.count = MAX_FRAGMENTS * 2;
        let budget = (0..beyond.count())
            .map(|index| beyond.packet_len(index))
            .max()
            .unwrap();
        assert!(
            ViewportTransfer::prepare(input, budget, usize::MAX)
                .unwrap()
                .is_none()
        );
        assert!(
            ViewportTransfer::prepare(delta(noisy_title(30000)), 1200, usize::MAX)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn impossible_budget_and_nonbeneficial_transfer_fall_back() {
        let input = delta("x".into());
        assert!(
            ViewportTransfer::prepare(input.clone(), 0, usize::MAX)
                .unwrap()
                .is_none()
        );
        assert!(
            ViewportTransfer::prepare(input.clone(), 1, usize::MAX)
                .unwrap()
                .is_none()
        );
        let one = ViewportTransfer::prepare(input.clone(), usize::MAX, usize::MAX)
            .unwrap()
            .unwrap();
        assert!(
            ViewportTransfer::prepare(input, usize::MAX, one.packet(0).encoded_len())
                .unwrap()
                .is_none()
        );
        assert!(
            ViewportTransfer::prepare(delta("x".repeat(MAX_WORKING_BYTES)), 1200, usize::MAX)
                .unwrap()
                .is_none()
        );
    }
}
