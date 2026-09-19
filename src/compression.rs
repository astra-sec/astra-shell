//! Negotiated, independently decodable payloads. Offsets/digests refer to raw
//! bytes, never the compressed representation. No dictionaries or shared state.

use std::io::Read;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tokio::{sync::Semaphore, task::JoinHandle};

use crate::protocol::{FileChunkResponse, PayloadEncoding, WriteFileChunkRequest};

const MIN_INPUT_BYTES: usize = 1024;
const MIN_SAVING_BYTES: usize = 64;
const MAX_WINDOW_LOG: u32 = 23; // 8 MiB, including malicious frame headers.
static TERMINAL_JOBS: Semaphore = Semaphore::const_new(1);
static FILE_JOBS: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Copy)]
pub(crate) enum WorkClass {
    Terminal,
    File,
}

/// Separate bounded pools keep bulk compression from occupying every blocking
/// thread. The permit lives in the closure even if its async caller is dropped.
pub(crate) async fn run<T: Send + 'static>(
    class: WorkClass,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let pool = match class {
        WorkClass::Terminal => &TERMINAL_JOBS,
        WorkClass::File => &FILE_JOBS,
    };
    let permit = pool.acquire().await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .context("payload worker failed")?
}

/// Owned by the attachment, not by a cancelable next_event() future. Input and
/// resize may interrupt awaiting this job without losing a received keyframe.
pub(crate) struct Background<T>(JoinHandle<Result<T>>);

impl<T: Send + 'static> Background<T> {
    pub(crate) fn terminal(work: impl FnOnce() -> Result<T> + Send + 'static) -> Self {
        Self(tokio::spawn(run(WorkClass::Terminal, work)))
    }

    pub(crate) async fn finish(&mut self) -> Result<T> {
        (&mut self.0)
            .await
            .context("terminal payload worker failed")?
    }
}

impl<T> Drop for Background<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct EncodedPayload {
    pub(crate) data: Vec<u8>,
    pub(crate) encoding: i32,
    pub(crate) uncompressed_size: u32,
}

impl EncodedPayload {
    pub(crate) fn encode(data: Vec<u8>, enabled: bool, maximum: usize) -> Result<Self> {
        ensure!(data.len() <= maximum, "payload exceeds its raw size limit");
        if enabled && data.len() >= MIN_INPUT_BYTES {
            let compressed = zstd::bulk::compress(&data, 1)?;
            // Include room for metadata overhead; avoid spending bandwidth on
            // negligible gains. Decisions are per block, not file extension.
            let saving = MIN_SAVING_BYTES.max(data.len().div_ceil(20));
            if compressed.len().saturating_add(saving) <= data.len() {
                return Ok(Self {
                    data: compressed,
                    encoding: PayloadEncoding::Zstd as i32,
                    uncompressed_size: u32::try_from(data.len())?,
                });
            }
        }
        Ok(Self {
            data,
            encoding: PayloadEncoding::None as i32,
            uncompressed_size: 0,
        })
    }

    pub(crate) fn validate_header(
        encoding: i32,
        uncompressed_size: u32,
        wire_size: usize,
        enabled: bool,
        maximum: usize,
    ) -> Result<()> {
        ensure!(
            wire_size <= maximum,
            "encoded payload exceeds its size limit"
        );
        match PayloadEncoding::try_from(encoding).context("unknown payload encoding")? {
            PayloadEncoding::None => {
                ensure!(
                    uncompressed_size == 0,
                    "raw payload has compression metadata"
                );
            }
            PayloadEncoding::Zstd => {
                ensure!(enabled, "zstd payload was not negotiated");
                ensure!(
                    uncompressed_size > 0 && uncompressed_size as usize <= maximum,
                    "uncompressed payload exceeds its size limit"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn decode(self, enabled: bool, maximum: usize, digest: &[u8]) -> Result<Vec<u8>> {
        ensure!(digest.len() == 32, "payload digest is invalid");
        let data = self.decode_bytes(enabled, maximum)?;
        ensure!(
            Sha256::digest(&data).as_slice() == digest,
            "payload digest does not match"
        );
        Ok(data)
    }

    fn decode_bytes(self, enabled: bool, maximum: usize) -> Result<Vec<u8>> {
        Self::validate_header(
            self.encoding,
            self.uncompressed_size,
            self.data.len(),
            enabled,
            maximum,
        )?;
        let data = if self.encoding == PayloadEncoding::Zstd as i32 {
            // Require exactly one standard frame. Do not accept skippable frames,
            // trailing bytes, concatenated frames, or unbounded decompression.
            ensure!(
                self.data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
                "invalid zstd frame magic"
            );
            let mut decoder =
                zstd::stream::read::Decoder::with_buffer(self.data.as_slice())?.single_frame();
            decoder.window_log_max(MAX_WINDOW_LOG)?;
            let expected = self.uncompressed_size as usize;
            let mut decoded = Vec::with_capacity(expected.min(64 * 1024));
            decoder
                .by_ref()
                .take(expected as u64 + 1)
                .read_to_end(&mut decoded)?;
            ensure!(
                decoded.len() == expected,
                "decompressed payload size does not match"
            );
            ensure!(
                decoder.finish().is_empty(),
                "zstd payload has trailing data"
            );
            decoded
        } else {
            self.data
        };
        Ok(data)
    }
}

pub(crate) fn encode_upload(
    mut chunk: WriteFileChunkRequest,
    enabled: bool,
) -> Result<WriteFileChunkRequest> {
    ensure!(
        chunk.encoding == 0 && chunk.uncompressed_size == 0,
        "upload API requires raw bytes"
    );
    let payload = EncodedPayload::encode(chunk.data, enabled, crate::files::MAX_FILE_CHUNK_SIZE)?;
    chunk.data = payload.data;
    chunk.encoding = payload.encoding;
    chunk.uncompressed_size = payload.uncompressed_size;
    Ok(chunk)
}

pub(crate) fn decode_upload(
    mut chunk: WriteFileChunkRequest,
    enabled: bool,
) -> Result<WriteFileChunkRequest> {
    chunk.data = EncodedPayload {
        data: chunk.data,
        encoding: chunk.encoding,
        uncompressed_size: chunk.uncompressed_size,
    }
    // FileService::write_chunk verifies the raw SHA-256 before any mutation.
    // Do not hash every uploaded MiB twice in the transport and storage layers.
    .decode_bytes(enabled, crate::files::MAX_FILE_CHUNK_SIZE)?;
    chunk.encoding = 0;
    chunk.uncompressed_size = 0;
    Ok(chunk)
}

pub(crate) fn encode_download(
    mut chunk: FileChunkResponse,
    enabled: bool,
) -> Result<FileChunkResponse> {
    let payload = EncodedPayload::encode(chunk.data, enabled, crate::files::MAX_FILE_CHUNK_SIZE)?;
    chunk.data = payload.data;
    chunk.encoding = payload.encoding;
    chunk.uncompressed_size = payload.uncompressed_size;
    Ok(chunk)
}

pub(crate) fn decode_download(
    mut chunk: FileChunkResponse,
    enabled: bool,
) -> Result<FileChunkResponse> {
    chunk.data = EncodedPayload {
        data: chunk.data,
        encoding: chunk.encoding,
        uncompressed_size: chunk.uncompressed_size,
    }
    .decode(enabled, crate::files::MAX_FILE_CHUNK_SIZE, &chunk.sha256)?;
    chunk.encoding = 0;
    chunk.uncompressed_size = 0;
    Ok(chunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{FileService, MAX_FILE_CHUNK_SIZE};
    use crate::protocol::BeginUploadRequest;
    use rand::{RngCore, SeedableRng};

    fn digest(data: &[u8]) -> Vec<u8> {
        Sha256::digest(data).to_vec()
    }

    #[test]
    fn independent_frames_round_trip_and_raw_fallback_is_exact() {
        let mut noise = vec![0; MAX_FILE_CHUNK_SIZE];
        rand::rngs::StdRng::seed_from_u64(42).fill_bytes(&mut noise);
        for data in [
            vec![],
            vec![b'x'; 100],
            noise,
            vec![b'x'; MAX_FILE_CHUNK_SIZE],
        ] {
            for enabled in [false, true] {
                let payload =
                    EncodedPayload::encode(data.clone(), enabled, MAX_FILE_CHUNK_SIZE).unwrap();
                let compressed = enabled && data == vec![b'x'; MAX_FILE_CHUNK_SIZE];
                assert_eq!(payload.encoding, i32::from(compressed));
                if !compressed {
                    assert_eq!(payload.data, data);
                    assert_eq!(payload.uncompressed_size, 0);
                }
                assert_eq!(
                    payload
                        .decode(enabled, MAX_FILE_CHUNK_SIZE, &digest(&data))
                        .unwrap(),
                    data
                );
            }
        }
    }

    #[test]
    fn rejects_unnegotiated_unknown_and_inconsistent_encodings() {
        let raw = vec![42; 4096];
        let make = || EncodedPayload::encode(raw.clone(), true, 4096).unwrap();
        assert!(make().decode(false, 4096, &digest(&raw)).is_err());
        let mut unknown = make();
        unknown.encoding = 9;
        assert!(unknown.decode(true, 4096, &digest(&raw)).is_err());
        let mut inconsistent = EncodedPayload::encode(raw.clone(), false, 4096).unwrap();
        inconsistent.uncompressed_size = 4096;
        assert!(inconsistent.decode(true, 4096, &digest(&raw)).is_err());
    }

    #[test]
    fn rejects_corruption_truncation_trailing_bytes_and_concatenation() {
        let raw = vec![42; 4096];
        let make = || EncodedPayload::encode(raw.clone(), true, 4096).unwrap();
        for removed in 1..5 {
            let mut payload = make();
            payload.data.truncate(payload.data.len() - removed);
            assert!(payload.decode(true, 4096, &digest(&raw)).is_err());
        }
        let mut trailing = make();
        trailing.data.push(0);
        assert!(trailing.decode(true, 4096, &digest(&raw)).is_err());
        let mut concatenated = make();
        concatenated.data.extend(make().data);
        assert!(concatenated.decode(true, 4096, &digest(&raw)).is_err());
        let mut corrupted = make();
        corrupted.data[0] ^= 1;
        assert!(corrupted.decode(true, 4096, &digest(&raw)).is_err());
        assert!(make().decode(true, 4096, &[0; 32]).is_err());
        assert!(make().decode(true, 4096, &[]).is_err());
    }

    #[test]
    fn bounds_declared_size_actual_output_and_decoder_window() {
        let raw = vec![42; 4096];
        for declared in [0, 10, 4095, 4097, u32::MAX] {
            let mut payload = EncodedPayload::encode(raw.clone(), true, 4096).unwrap();
            payload.uncompressed_size = declared;
            assert!(payload.decode(true, 4096, &digest(&raw)).is_err());
        }
        assert!(EncodedPayload::encode(raw.clone(), true, 4095).is_err());
        assert!(
            EncodedPayload::encode(raw.clone(), false, 4096)
                .unwrap()
                .decode(true, 4095, &digest(&raw))
                .is_err()
        );
        // Valid single-segment frame declaring 16 MiB. The advertised raw size
        // is small, so only the decoder's window bound protects this path.
        let oversized_window = EncodedPayload {
            data: vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 0, 1, 1, 0, 0],
            encoding: 1,
            uncompressed_size: 1,
        };
        let error = oversized_window
            .decode(true, 4096, &digest(&[0]))
            .unwrap_err();
        assert!(error.to_string().contains("memory"), "{error:#}");
    }

    #[test]
    fn compressed_upload_resumes_after_restart_and_accepts_raw_replay() {
        let root = tempfile::tempdir().unwrap();
        let raw = vec![b'x'; MAX_FILE_CHUNK_SIZE + 1234];
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let begin = BeginUploadRequest {
            transfer_id: transfer_id.clone(),
            path: b"result.txt".to_vec(),
            size: raw.len() as u64,
            sha256: digest(&raw),
            mode: 0o600,
            overwrite: false,
        };
        let request = |offset: usize, bytes: &[u8]| WriteFileChunkRequest {
            transfer_id: transfer_id.clone(),
            offset: offset as u64,
            data: bytes.to_vec(),
            sha256: digest(bytes),
            ..Default::default()
        };
        let service = FileService::new(root.path().to_owned()).unwrap();
        service.begin_upload(begin.clone()).unwrap();
        let first = request(0, &raw[..MAX_FILE_CHUNK_SIZE]);
        let wire = encode_upload(first.clone(), true).unwrap();
        assert_eq!(wire.encoding, 1);
        assert!(wire.data.len() < 1024);
        assert!(service.write_chunk(wire.clone()).is_err()); // never write compressed bytes
        assert_eq!(
            service
                .write_chunk(decode_upload(wire, true).unwrap())
                .unwrap()
                .committed_offset,
            MAX_FILE_CHUNK_SIZE as u64
        );
        drop(service);
        let restarted = FileService::new(root.path().to_owned()).unwrap();
        assert_eq!(
            restarted.begin_upload(begin).unwrap().committed_offset,
            MAX_FILE_CHUNK_SIZE as u64
        );
        // The connection can negotiate differently after recovery; compression
        // bytes are not part of the idempotency identity.
        assert_eq!(
            restarted.write_chunk(first).unwrap().committed_offset,
            MAX_FILE_CHUNK_SIZE as u64
        );
        let wire = encode_upload(
            request(MAX_FILE_CHUNK_SIZE, &raw[MAX_FILE_CHUNK_SIZE..]),
            true,
        )
        .unwrap();
        restarted
            .write_chunk(decode_upload(wire, true).unwrap())
            .unwrap();
        restarted.commit_upload(&transfer_id).unwrap();
        assert_eq!(std::fs::read(root.path().join("result.txt")).unwrap(), raw);
    }

    #[tokio::test]
    async fn background_decode_survives_cancellation_of_the_waiter() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut job = Background::terminal(move || {
            wait.recv()?;
            Ok(42)
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), job.finish())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(job.finish().await.unwrap(), 42);
    }

    #[tokio::test]
    async fn canceled_work_keeps_its_permit_until_the_blocking_job_finishes() {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(run(WorkClass::File, move || {
            let _ = started.send(());
            wait.recv()?;
            Ok(())
        }));
        ready.await.unwrap();
        first.abort();
        let mut second = tokio::spawn(run(WorkClass::File, || Ok(42)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut second)
                .await
                .is_err()
        );
        // The terminal queue is independent of a busy file codec.
        assert_eq!(run(WorkClass::Terminal, || Ok(7)).await.unwrap(), 7);
        release.send(()).unwrap();
        assert_eq!(second.await.unwrap().unwrap(), 42);
    }

    #[test]
    #[ignore = "manual release-mode payload codec benchmark"]
    fn payload_codec_benchmark() {
        use prost::Message;
        use std::time::Instant;
        let mut engine =
            crate::terminal_engine::TerminalEngine::new(50, 200, 128, Box::new(std::io::sink()))
                .unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for row in 0..50 {
            let text: String = (0..199)
                .map(|_| (33 + rng.next_u32() % 94) as u8 as char)
                .collect();
            engine.advance(format!("\x1b[{};1H{text}", row + 1).as_bytes());
        }
        let state = engine.semantic_viewport().unwrap().encode_to_vec();
        let mut noise = vec![0; MAX_FILE_CHUNK_SIZE];
        rng.fill_bytes(&mut noise);
        for (name, raw) in [
            ("varied-ascii-keyframe", state),
            ("repeated-text-file", vec![b'x'; MAX_FILE_CHUNK_SIZE]),
            ("random-file", noise),
        ] {
            let mut enc = Vec::new();
            let mut dec = Vec::new();
            let digest = digest(&raw);
            let mut wire_bytes = 0;
            let mut encoding = 0;
            for round in 0..220 {
                let input = raw.clone(); // exclude input copying from codec timing
                let start = Instant::now();
                let wire = EncodedPayload::encode(input, true, 8 * MAX_FILE_CHUNK_SIZE).unwrap();
                let encode_time = start.elapsed();
                wire_bytes = wire.data.len();
                encoding = wire.encoding;
                let start = Instant::now();
                let restored = wire.decode(true, 8 * MAX_FILE_CHUNK_SIZE, &digest).unwrap();
                let decode_time = start.elapsed();
                assert_eq!(restored, raw);
                if round >= 20 {
                    enc.push(encode_time.as_micros());
                    dec.push(decode_time.as_micros());
                }
            }
            enc.sort_unstable();
            dec.sort_unstable();
            println!(
                "{name}: raw={} wire={wire_bytes} encoding={encoding}, encode_us p50={} p95={}, decode+sha256_us p50={} p95={}",
                raw.len(),
                enc[100],
                enc[189],
                dec[100],
                dec[189]
            );
        }
    }
}
