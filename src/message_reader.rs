use crate::protocol::{MAX_FRAME_SIZE, WireMessage};
use anyhow::{Result, bail};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Keep this parser outside `select!`: cancellation of `read` preserves every
/// consumed header/payload byte. The one-shot `read_message` helper must only be
/// used when a cancelled read also discards the stream.
#[derive(Default)]
pub struct MessageReader {
    header: [u8; 4],
    header_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl MessageReader {
    pub async fn read<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<WireMessage>> {
        self.read_limited(reader, MAX_FRAME_SIZE).await
    }

    async fn read_limited<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        limit: usize,
    ) -> Result<Option<WireMessage>> {
        while self.header_read < self.header.len() {
            let count = reader.read(&mut self.header[self.header_read..]).await?;
            if count == 0 {
                if self.header_read == 0 {
                    return Ok(None);
                }
                bail!("truncated protocol frame header");
            }
            self.header_read += count;
        }
        let length = u32::from_be_bytes(self.header) as usize;
        if length > limit.min(MAX_FRAME_SIZE) {
            bail!("peer sent oversized frame: {length} bytes");
        }
        self.payload.resize(length, 0);
        while self.payload_read < length {
            let count = reader.read(&mut self.payload[self.payload_read..]).await?;
            if count == 0 {
                bail!("truncated protocol frame payload");
            }
            self.payload_read += count;
        }
        let decoded = WireMessage::decode(self.payload.as_slice());
        self.header_read = 0;
        self.payload_read = 0;
        self.payload.clear();
        Ok(Some(decoded?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TerminalCommand, terminal_command, wire_message};
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn framed_read_survives_cancellation_at_every_byte_boundary() {
        let message = WireMessage::new(wire_message::Body::TerminalCommand(TerminalCommand {
            terminal_id: "test".into(),
            command: Some(terminal_command::Command::Input(b"hello".to_vec())),
            ..Default::default()
        }));
        let payload = message.encode_to_vec();
        let bytes = [(payload.len() as u32).to_be_bytes().as_slice(), &payload].concat();
        for split in 1..bytes.len() {
            let (mut sender, mut receiver) = tokio::io::duplex(1024);
            let mut reader = MessageReader::default();
            sender.write_all(&bytes[..split]).await.unwrap();
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(1),
                    reader.read(&mut receiver)
                )
                .await
                .is_err()
            );
            sender.write_all(&bytes[split..]).await.unwrap();
            sender.write_all(&bytes).await.unwrap();
            assert_eq!(
                reader.read(&mut receiver).await.unwrap(),
                Some(message.clone())
            );
            assert_eq!(
                reader.read(&mut receiver).await.unwrap(),
                Some(message.clone())
            );
            sender.shutdown().await.unwrap();
            assert!(reader.read(&mut receiver).await.unwrap().is_none());
        }
    }
}
