//! Application-owned stream identity and QoS, shared by client, gateway and worker.
//! COMPAT-04 keeps Request-first classification only for peers that did not
//! negotiate transport.stream_channels v1.

use anyhow::{Context, Result, ensure};
use prost::Message;
use tokio::io::AsyncRead;

use crate::protocol::{
    Request, StreamChannelKind, StreamHello, WireMessage, read_message_limited, request,
    wire_message,
};

pub const MAX_STREAM_HELLO_SIZE: usize = 1024;
pub const MAX_STREAM_ID_BYTES: usize = 128;

impl StreamChannelKind {
    /// Backend priorities are advisory; identity and correctness never depend on them.
    pub fn priority(self) -> i32 {
        match self {
            Self::Control => 20,
            Self::Terminal => 10,
            Self::File => -10,
            Self::Unspecified => 0,
        }
    }
}

pub fn request_channel(request: &Request) -> Result<(StreamChannelKind, &str)> {
    use StreamChannelKind::{Control, File, Terminal};
    use request::Command;
    let command = request.command.as_ref().context("request has no command")?;
    Ok(match command {
        Command::Attach(attach) => (Terminal, &attach.terminal_id),
        Command::BeginUpload(upload) => (File, &upload.transfer_id),
        Command::WriteFileChunk(chunk) => (File, &chunk.transfer_id),
        Command::QueryUpload(upload) => (File, &upload.transfer_id),
        Command::CommitUpload(upload) => (File, &upload.transfer_id),
        Command::AbortUpload(upload) => (File, &upload.transfer_id),
        Command::FileCapabilities(_)
        | Command::FileStat(_)
        | Command::FileList(_)
        | Command::BeginDownload(_)
        | Command::ReadFileChunk(_)
        | Command::MakeDirectory(_)
        | Command::RemoveFile(_)
        | Command::RenameFile(_)
        | Command::GitStatus(_)
        | Command::WatchFiles(_) => (File, ""),
        Command::List(_)
        | Command::Spawn(_)
        | Command::Close(_)
        | Command::RenameTerminal(_)
        | Command::ListWorkspaces(_)
        | Command::CreateWorkspace(_)
        | Command::RenameWorkspace(_)
        | Command::DeleteWorkspace(_)
        | Command::ListTerminals(_)
        | Command::ListAttachments(_) => (Control, ""),
    })
}

pub fn hello_for_request(request: &Request, epoch: u64) -> Result<StreamHello> {
    let (kind, handle) = request_channel(request)?;
    let hello = StreamHello {
        version: 1,
        channel_kind: kind as i32,
        handle: handle.to_owned(),
        epoch,
        request_id: request.request_id.clone(),
    };
    validate_hello(&hello, epoch)?;
    Ok(hello)
}

pub fn validate_hello(hello: &StreamHello, expected_epoch: u64) -> Result<StreamChannelKind> {
    ensure!(hello.version == 1, "unsupported stream hello version");
    ensure!(
        expected_epoch != 0 && hello.epoch == expected_epoch,
        "stream epoch mismatch"
    );
    let kind =
        StreamChannelKind::try_from(hello.channel_kind).context("unknown stream channel kind")?;
    ensure!(
        kind != StreamChannelKind::Unspecified,
        "unspecified stream channel kind"
    );
    ensure!(
        !hello.request_id.is_empty() && hello.request_id.len() <= MAX_STREAM_ID_BYTES,
        "invalid stream request ID length"
    );
    ensure!(
        hello.handle.len() <= MAX_STREAM_ID_BYTES,
        "invalid stream handle length"
    );
    ensure!(
        kind != StreamChannelKind::Terminal || !hello.handle.is_empty(),
        "missing terminal handle"
    );
    ensure!(
        kind != StreamChannelKind::Control || hello.handle.is_empty(),
        "control stream has a handle"
    );
    ensure!(
        WireMessage::new(wire_message::Body::StreamHello(hello.clone())).encoded_len()
            <= MAX_STREAM_HELLO_SIZE,
        "oversized stream hello"
    );
    Ok(kind)
}

pub fn validate_request(hello: &StreamHello, request: &Request, epoch: u64) -> Result<()> {
    validate_hello(hello, epoch)?;
    ensure!(
        hello == &hello_for_request(request, epoch)?,
        "stream hello does not match request"
    );
    Ok(())
}

pub async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R, epoch: u64) -> Result<StreamHello> {
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_message_limited(reader, MAX_STREAM_HELLO_SIZE),
    )
    .await
    .context("stream hello timed out")??
    .context("stream ended before StreamHello")?;
    let Some(wire_message::Body::StreamHello(hello)) = first.body else {
        anyhow::bail!("expected StreamHello before Request");
    };
    validate_hello(&hello, epoch)?;
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AttachRequest, ListRequest, WriteFileChunkRequest};

    #[test]
    fn channel_identity_binds_kind_handle_epoch_and_request_id() {
        let request = Request {
            request_id: "request".into(),
            command: Some(request::Command::Attach(AttachRequest {
                terminal_id: "terminal".into(),
                ..Default::default()
            })),
        };
        let hello = hello_for_request(&request, 42).unwrap();
        assert_eq!(hello.channel_kind, StreamChannelKind::Terminal as i32);
        validate_request(&hello, &request, 42).unwrap();
        for invalid in [
            StreamHello {
                version: 2,
                ..hello.clone()
            },
            StreamHello {
                channel_kind: 99,
                ..hello.clone()
            },
            StreamHello {
                channel_kind: StreamChannelKind::File as i32,
                ..hello.clone()
            },
            StreamHello {
                handle: "other".into(),
                ..hello.clone()
            },
            StreamHello {
                epoch: 43,
                ..hello.clone()
            },
            StreamHello {
                request_id: "other".into(),
                ..hello.clone()
            },
            StreamHello {
                request_id: "x".repeat(129),
                ..hello.clone()
            },
            StreamHello {
                handle: "界".repeat(43),
                ..hello.clone()
            },
        ] {
            assert!(validate_request(&invalid, &request, 42).is_err());
        }
        assert!(validate_hello(&hello, 0).is_err());
    }

    #[test]
    fn control_and_file_mapping_use_declared_priority_and_transfer_handle() {
        let mut request = Request {
            request_id: "request".into(),
            command: Some(request::Command::List(ListRequest {})),
        };
        let control = hello_for_request(&request, 1).unwrap();
        assert!(control.handle.is_empty());
        request.command = Some(request::Command::WriteFileChunk(WriteFileChunkRequest {
            transfer_id: "transfer".into(),
            ..Default::default()
        }));
        let file = hello_for_request(&request, 1).unwrap();
        assert_eq!(file.handle, "transfer");
        assert!(StreamChannelKind::Control.priority() > StreamChannelKind::Terminal.priority());
        assert!(StreamChannelKind::Terminal.priority() > StreamChannelKind::File.priority());
    }

    #[tokio::test]
    async fn oversized_preamble_is_rejected_before_payload_read() {
        let mut header = &1025_u32.to_be_bytes()[..];
        assert!(
            read_hello(&mut header, 42)
                .await
                .unwrap_err()
                .to_string()
                .contains("oversized")
        );
    }
}
