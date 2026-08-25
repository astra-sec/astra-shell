use anyhow::{Context, Result, bail};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;
pub const LOCALE_ENVIRONMENT_VARIABLES: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
    "LC_ALL",
];

#[derive(Clone, PartialEq, Message)]
pub struct WireMessage {
    #[prost(oneof = "wire_message::Body", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub body: Option<wire_message::Body>,
}

pub mod wire_message {
    use super::*;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Body {
        #[prost(message, tag = "1")]
        ServerHello(ServerHello),
        #[prost(message, tag = "2")]
        AuthRequest(AuthRequest),
        #[prost(message, tag = "3")]
        AuthResult(AuthResult),
        #[prost(message, tag = "4")]
        Request(Request),
        #[prost(message, tag = "5")]
        Response(Response),
        #[prost(message, tag = "6")]
        TerminalCommand(TerminalCommand),
        #[prost(message, tag = "7")]
        TerminalEvent(TerminalEvent),
        #[prost(message, tag = "8")]
        ClientHello(ClientHello),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHello {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(string, tag = "2")]
    pub username: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerHello {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub challenge: Vec<u8>,
    #[prost(string, tag = "3")]
    pub server_instance: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AuthRequest {
    #[prost(string, tag = "1")]
    pub public_key: String,
    #[prost(string, tag = "2")]
    pub signature_pem: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AuthResult {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(
        oneof = "request::Command",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29"
    )]
    pub command: Option<request::Command>,
}

pub mod request {
    use super::*;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Command {
        #[prost(message, tag = "10")]
        List(ListRequest),
        #[prost(message, tag = "11")]
        Spawn(SpawnRequest),
        #[prost(message, tag = "12")]
        Attach(AttachRequest),
        #[prost(message, tag = "13")]
        Close(CloseRequest),
        #[prost(message, tag = "14")]
        FileCapabilities(FileCapabilitiesRequest),
        #[prost(message, tag = "15")]
        FileStat(FileStatRequest),
        #[prost(message, tag = "16")]
        FileList(FileListRequest),
        #[prost(message, tag = "17")]
        BeginUpload(BeginUploadRequest),
        #[prost(message, tag = "18")]
        WriteFileChunk(WriteFileChunkRequest),
        #[prost(message, tag = "19")]
        QueryUpload(QueryUploadRequest),
        #[prost(message, tag = "20")]
        CommitUpload(CommitUploadRequest),
        #[prost(message, tag = "21")]
        AbortUpload(AbortUploadRequest),
        #[prost(message, tag = "22")]
        BeginDownload(BeginDownloadRequest),
        #[prost(message, tag = "23")]
        ReadFileChunk(ReadFileChunkRequest),
        #[prost(message, tag = "24")]
        MakeDirectory(MakeDirectoryRequest),
        #[prost(message, tag = "25")]
        RemoveFile(RemoveFileRequest),
        #[prost(message, tag = "26")]
        RenameFile(RenameFileRequest),
        #[prost(message, tag = "27")]
        GitStatus(GitStatusRequest),
        #[prost(message, tag = "28")]
        WatchFiles(WatchFilesRequest),
        #[prost(message, tag = "29")]
        RenameTerminal(RenameTerminalRequest),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ListRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct SpawnRequest {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, repeated, tag = "2")]
    pub argv: Vec<String>,
    #[prost(string, tag = "3")]
    pub cwd: String,
    #[prost(uint32, tag = "4")]
    pub rows: u32,
    #[prost(uint32, tag = "5")]
    pub cols: u32,
    #[prost(string, tag = "6")]
    pub term: String,
    #[prost(message, repeated, tag = "7")]
    pub environment: Vec<EnvironmentVariable>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EnvironmentVariable {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AttachRequest {
    #[prost(string, tag = "1")]
    pub terminal_id: String,
    #[prost(bool, tag = "2")]
    pub read_only: bool,
    #[prost(bool, tag = "3")]
    pub takeover: bool,
    #[prost(string, tag = "4")]
    pub resume_token: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CloseRequest {
    #[prost(string, tag = "1")]
    pub terminal_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameTerminalRequest {
    #[prost(string, tag = "1")]
    pub terminal_id: String,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileCapabilitiesRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct FileStatRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(bool, tag = "2")]
    pub follow_symlinks: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileListRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub cursor: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub limit: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginUploadRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
    #[prost(bytes = "vec", tag = "2")]
    pub path: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub sha256: Vec<u8>,
    #[prost(bool, tag = "5")]
    pub overwrite: bool,
    #[prost(uint32, tag = "6")]
    pub mode: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteFileChunkRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub sha256: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct QueryUploadRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommitUploadRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AbortUploadRequest {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginDownloadRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(bool, tag = "2")]
    pub want_sha256: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadFileChunkRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub snapshot: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(uint32, tag = "4")]
    pub length: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct MakeDirectoryRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RemoveFileRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameFileRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub source: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub destination: Vec<u8>,
    #[prost(bool, tag = "3")]
    pub overwrite: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct GitStatusRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WatchFilesRequest {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub paths: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(string, tag = "1")]
    pub request_id: String,
    #[prost(
        oneof = "response::Result",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22"
    )]
    pub result: Option<response::Result>,
}

pub mod response {
    use super::*;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "10")]
        List(ListResponse),
        #[prost(message, tag = "11")]
        Spawn(SpawnResponse),
        #[prost(message, tag = "12")]
        Attach(AttachResponse),
        #[prost(message, tag = "13")]
        Ack(AckResponse),
        #[prost(message, tag = "14")]
        Error(ErrorResponse),
        #[prost(message, tag = "15")]
        FileCapabilities(FileCapabilitiesResponse),
        #[prost(message, tag = "16")]
        FileStat(FileStatResponse),
        #[prost(message, tag = "17")]
        FileList(FileListResponse),
        #[prost(message, tag = "18")]
        UploadStatus(UploadStatusResponse),
        #[prost(message, tag = "19")]
        BeginDownload(BeginDownloadResponse),
        #[prost(message, tag = "20")]
        FileChunk(FileChunkResponse),
        #[prost(message, tag = "21")]
        GitStatus(GitStatusResponse),
        #[prost(message, tag = "22")]
        FileChanges(FileChangesResponse),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct FileCapabilitiesResponse {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub max_chunk_size: u32,
    #[prost(bool, tag = "3")]
    pub resumable_uploads: bool,
    #[prost(bool, tag = "4")]
    pub atomic_upload_commit: bool,
    #[prost(bool, tag = "5")]
    pub chunk_sha256: bool,
    #[prost(bool, tag = "6")]
    pub file_watch_events: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum FileKind {
    Unspecified = 0,
    Regular = 1,
    Directory = 2,
    Symlink = 3,
    Other = 4,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileMetadata {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub name: Vec<u8>,
    #[prost(enumeration = "FileKind", tag = "3")]
    pub kind: i32,
    #[prost(uint64, tag = "4")]
    pub size: u64,
    #[prost(uint32, tag = "5")]
    pub mode: u32,
    #[prost(int64, tag = "6")]
    pub modified_unix_ns: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileStatResponse {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<FileMetadata>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileListResponse {
    #[prost(message, repeated, tag = "1")]
    pub entries: Vec<FileMetadata>,
    #[prost(bytes = "vec", tag = "2")]
    pub next_cursor: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UploadStatusResponse {
    #[prost(string, tag = "1")]
    pub transfer_id: String,
    #[prost(bytes = "vec", tag = "2")]
    pub path: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(uint64, tag = "4")]
    pub committed_offset: u64,
    #[prost(string, tag = "5")]
    pub state: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginDownloadResponse {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<FileMetadata>,
    #[prost(bytes = "vec", tag = "2")]
    pub snapshot: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub sha256: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub max_chunk_size: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileChunkResponse {
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub sha256: Vec<u8>,
    #[prost(bool, tag = "4")]
    pub eof: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct GitFileStatus {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(string, tag = "2")]
    pub index_status: String,
    #[prost(string, tag = "3")]
    pub worktree_status: String,
    #[prost(bytes = "vec", tag = "4")]
    pub original_path: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GitStatusResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub repository_root: Vec<u8>,
    #[prost(string, tag = "2")]
    pub branch: String,
    #[prost(bool, tag = "3")]
    pub detached: bool,
    #[prost(uint32, tag = "4")]
    pub ahead: u32,
    #[prost(uint32, tag = "5")]
    pub behind: u32,
    #[prost(message, repeated, tag = "6")]
    pub files: Vec<GitFileStatus>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileChange {
    #[prost(bytes = "vec", tag = "1")]
    pub path: Vec<u8>,
    #[prost(string, tag = "2")]
    pub kind: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileChangesResponse {
    #[prost(message, repeated, tag = "1")]
    pub changes: Vec<FileChange>,
    #[prost(bool, tag = "2")]
    pub rescan_required: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalInfo {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, repeated, tag = "3")]
    pub argv: Vec<String>,
    #[prost(string, tag = "4")]
    pub cwd: String,
    #[prost(string, tag = "5")]
    pub status: String,
    #[prost(int32, optional, tag = "6")]
    pub exit_code: Option<i32>,
    #[prost(uint32, tag = "7")]
    pub rows: u32,
    #[prost(uint32, tag = "8")]
    pub cols: u32,
    #[prost(uint64, tag = "9")]
    pub display_id: u64,
    #[prost(string, optional, tag = "10")]
    pub custom_name: Option<String>,
    #[prost(bool, optional, tag = "11")]
    pub interactive: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListResponse {
    #[prost(message, repeated, tag = "1")]
    pub terminals: Vec<TerminalInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SpawnResponse {
    #[prost(message, optional, tag = "1")]
    pub terminal: Option<TerminalInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalSnapshot {
    #[prost(uint32, tag = "1")]
    pub rows: u32,
    #[prost(uint32, tag = "2")]
    pub cols: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub contents: Vec<u8>,
    #[prost(bool, tag = "4")]
    pub alternate_screen: bool,
    #[prost(bytes = "vec", tag = "5")]
    pub normal_contents: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AttachResponse {
    #[prost(message, optional, tag = "1")]
    pub terminal: Option<TerminalInfo>,
    #[prost(string, tag = "2")]
    pub lease_id: String,
    #[prost(bool, tag = "3")]
    pub read_only: bool,
    #[prost(bytes = "vec", tag = "4")]
    pub history: Vec<u8>,
    #[prost(string, tag = "5")]
    pub resume_token: String,
    #[prost(message, optional, tag = "6")]
    pub snapshot: Option<TerminalSnapshot>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AckResponse {
    #[prost(string, tag = "1")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalCommand {
    #[prost(string, tag = "1")]
    pub terminal_id: String,
    #[prost(string, tag = "2")]
    pub lease_id: String,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    #[prost(oneof = "terminal_command::Command", tags = "10, 11, 12")]
    pub command: Option<terminal_command::Command>,
}

pub mod terminal_command {
    use super::*;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Command {
        #[prost(bytes, tag = "10")]
        Input(Vec<u8>),
        #[prost(message, tag = "11")]
        Resize(Resize),
        #[prost(bool, tag = "12")]
        Detach(bool),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Resize {
    #[prost(uint32, tag = "1")]
    pub rows: u32,
    #[prost(uint32, tag = "2")]
    pub cols: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalEvent {
    #[prost(string, tag = "1")]
    pub terminal_id: String,
    #[prost(oneof = "terminal_event::Event", tags = "10, 11, 12, 13, 14")]
    pub event: Option<terminal_event::Event>,
}

pub mod terminal_event {
    use super::TerminalSnapshot;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(bytes, tag = "10")]
        Output(Vec<u8>),
        #[prost(int32, tag = "11")]
        Exited(i32),
        #[prost(string, tag = "12")]
        Error(String),
        #[prost(bool, tag = "13")]
        Interactive(bool),
        #[prost(message, tag = "14")]
        Snapshot(TerminalSnapshot),
    }
}

impl WireMessage {
    pub fn new(body: wire_message::Body) -> Self {
        Self { body: Some(body) }
    }
}

pub async fn write_message<W>(writer: &mut W, message: &WireMessage) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut payload = Vec::with_capacity(message.encoded_len());
    message.encode(&mut payload)?;
    if payload.len() > MAX_FRAME_SIZE {
        bail!("frame is too large: {} bytes", payload.len());
    }
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R>(reader: &mut R) -> Result<Option<WireMessage>>
where
    R: AsyncRead + Unpin,
{
    let length = match reader.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if length > MAX_FRAME_SIZE {
        bail!("peer sent oversized frame: {length} bytes");
    }
    let mut payload = vec![0; length];
    reader
        .read_exact(&mut payload)
        .await
        .context("truncated protocol frame")?;
    Ok(Some(WireMessage::decode(payload.as_slice())?))
}

pub async fn require_message<R>(reader: &mut R) -> Result<WireMessage>
where
    R: AsyncRead + Unpin,
{
    read_message(reader)
        .await?
        .context("peer closed the stream before sending a message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_without_display_id_decodes_as_legacy_zero() {
        // TerminalInfo{id: "legacy"} encoded before protobuf field 9 existed.
        let encoded = [0x0a, 0x06, b'l', b'e', b'g', b'a', b'c', b'y'];
        let terminal = TerminalInfo::decode(encoded.as_slice()).unwrap();
        assert_eq!(terminal.id, "legacy");
        assert_eq!(terminal.display_id, 0);
    }
}
