use std::{
    fs,
    io::{IsTerminal, Write},
    net::SocketAddr,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use astra_shell::{
    client::{AstraClient, ServerResponseError, ServerTrust},
    known_hosts::{StrictHostKeyChecking, default_known_hosts_file},
    protocol::{
        AttachResponse, BeginDownloadResponse, BeginUploadRequest, EnvironmentVariable, FileKind,
        FileMetadata, LOCALE_ENVIRONMENT_VARIABLES, ReadFileChunkRequest, Resize, SpawnRequest,
        TerminalCommand, TerminalSnapshot, WireMessage, WriteFileChunkRequest, read_message,
        terminal_command, terminal_event, wire_message, write_message,
    },
};
use clap::{Args, Parser, Subcommand};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "astra", version, about = "Astra persistent terminal client")]
struct Cli {
    /// Server UDP port, equivalent to ssh -p.
    #[arg(short = 'p', long, default_value_t = 4433)]
    port: u16,
    /// TLS server name. Defaults to astra.local for astrad-generated certificates.
    #[arg(long)]
    server_name: Option<String>,
    /// Pin a provisioned DER certificate instead of using Astra known hosts.
    #[arg(long)]
    server_cert: Option<PathBuf>,
    /// SSH-style option. Supported: StrictHostKeyChecking and UserKnownHostsFile.
    #[arg(short = 'o', value_name = "OPTION", action = clap::ArgAction::Append)]
    ssh_options: Vec<String>,
    /// Override Astra's known-hosts path (normally ~/.config/astra/known_hosts).
    #[arg(long)]
    known_hosts_file: Option<PathBuf>,
    /// OpenSSH Ed25519 or RSA private key. Defaults to id_ed25519, then id_rsa.
    #[arg(short = 'i', long)]
    identity: Option<PathBuf>,
    /// Target Unix account, equivalent to the user in `ssh user@host`.
    #[arg(short = 'l', long)]
    user: Option<String>,
    /// Destination in SSH form: [USER@]HOST.
    #[arg(value_name = "[USER@]HOST")]
    destination: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List terminals that are active in the current daemon or user worker.
    List {
        /// Include canonical UUIDs used for automatic resume and diagnostics.
        #[arg(long)]
        long: bool,
    },
    /// Create a terminal. Arguments after `--` are executed directly, without a shell.
    New(NewArgs),
    /// Attach to a running terminal by short ID or canonical UUID.
    Attach(AttachArgs),
    /// Terminate a running terminal by short ID or canonical UUID.
    Close {
        #[arg(value_name = "TERMINAL")]
        terminal_id: String,
    },
    /// Browse and transfer files through Astra Files/1 on the existing QUIC connection.
    #[command(name = "files", alias = "file")]
    Files(FileArgs),
}

#[derive(Debug, Args)]
struct FileArgs {
    #[command(subcommand)]
    command: FileCommand,
}

#[derive(Debug, Subcommand)]
enum FileCommand {
    /// Show the server's Astra Files protocol capabilities.
    Capabilities,
    /// List a remote directory.
    Ls {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show metadata for a remote path.
    Stat {
        path: PathBuf,
        #[arg(long)]
        no_follow: bool,
    },
    /// Upload a local file with automatic reconnect and resume.
    Put {
        local: PathBuf,
        remote: PathBuf,
        #[arg(short, long)]
        overwrite: bool,
    },
    /// Download a remote file with automatic reconnect and resume.
    Get {
        remote: PathBuf,
        local: Option<PathBuf>,
        #[arg(short, long)]
        overwrite: bool,
    },
    /// Create a remote directory.
    Mkdir { path: PathBuf },
    /// Remove a remote file, symlink, or empty directory.
    Rm { path: PathBuf },
    /// Rename a remote file or directory.
    Mv {
        source: PathBuf,
        destination: PathBuf,
        #[arg(short, long)]
        overwrite: bool,
    },
}

#[derive(Debug, Args)]
struct NewArgs {
    #[arg(long, default_value = "")]
    name: String,
    #[arg(long, default_value = "")]
    cwd: String,
    #[arg(long, default_value_t = 24)]
    rows: u32,
    #[arg(long, default_value_t = 80)]
    cols: u32,
    #[arg(long)]
    attach: bool,
    #[arg(last = true)]
    argv: Vec<String>,
}

#[derive(Debug, Args)]
struct AttachArgs {
    #[arg(value_name = "TERMINAL")]
    terminal_id: String,
    #[arg(long)]
    read_only: bool,
    #[arg(long)]
    takeover: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("astra: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let ssh_options = parse_ssh_options(&cli.ssh_options)?;
    let destination = parse_destination(&cli.destination)?;
    let username = cli
        .user
        .as_deref()
        .or(destination.username.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(default_username);
    let address = resolve_address(&destination.host, cli.port).await?;
    let server_name = cli
        .server_name
        .unwrap_or_else(|| inferred_server_name(&destination.host));
    let identity = select_identity(cli.identity.as_deref())?;
    let trust = match cli.server_cert {
        Some(certificate) => ServerTrust::PinnedCertificate(certificate),
        None => {
            let configured_file = cli.known_hosts_file.or(ssh_options.user_known_hosts_file);
            let known_hosts_file = match configured_file {
                Some(path) => expand_home_path(&path)?,
                None => default_known_hosts_file()?,
            };
            ServerTrust::KnownHosts {
                host: destination.host.clone(),
                port: cli.port,
                file: known_hosts_file,
                policy: ssh_options.strict_host_key_checking,
            }
        }
    };
    let client = AstraClient::connect(address, &server_name, &trust, &identity, &username).await?;
    match cli.command {
        None => {
            let terminal = client
                .spawn(SpawnRequest {
                    name: String::new(),
                    argv: Vec::new(),
                    cwd: String::new(),
                    rows: 24,
                    cols: 80,
                    term: client_term()?,
                    environment: client_locale_environment()?,
                })
                .await?;
            attach_terminal(client, terminal.id, false, false).await?;
        }
        Some(Command::List { long }) => {
            let terminals = client.list().await?;
            if terminals.is_empty() {
                println!("No terminals.");
            } else if long {
                println!(
                    "{:<6}  {:<36}  {:<12}  {:<8}  COMMAND",
                    "ID", "UUID", "NAME", "STATUS"
                );
                for terminal in terminals {
                    println!(
                        "{:<6}  {:<36}  {:<12}  {:<8}  {}",
                        display_id(&terminal),
                        terminal.id,
                        truncate(&terminal.name, 12),
                        terminal.status,
                        shell_join(&terminal.argv),
                    );
                }
            } else {
                println!("{:<6}  {:<12}  {:<8}  COMMAND", "ID", "NAME", "STATUS");
                for terminal in terminals {
                    println!(
                        "{:<6}  {:<12}  {:<8}  {}",
                        terminal_reference(&terminal),
                        truncate(&terminal.name, 12),
                        terminal.status,
                        shell_join(&terminal.argv),
                    );
                }
            }
        }
        Some(Command::New(arguments)) => {
            let attach = arguments.attach;
            let terminal = client
                .spawn(SpawnRequest {
                    name: arguments.name,
                    argv: arguments.argv,
                    cwd: arguments.cwd,
                    rows: arguments.rows,
                    cols: arguments.cols,
                    term: client_term()?,
                    environment: client_locale_environment()?,
                })
                .await?;
            println!("{}", terminal_reference(&terminal));
            if attach {
                attach_terminal(client, terminal.id, false, false).await?;
            }
        }
        Some(Command::Attach(arguments)) => {
            attach_terminal(
                client,
                arguments.terminal_id,
                arguments.read_only,
                arguments.takeover,
            )
            .await?;
        }
        Some(Command::Close { terminal_id }) => {
            println!("{}", client.close(terminal_id).await?)
        }
        Some(Command::Files(arguments)) => run_file_command(client, arguments.command).await?,
    }
    Ok(())
}

async fn run_file_command(mut client: AstraClient, command: FileCommand) -> Result<()> {
    match command {
        FileCommand::Capabilities => {
            let capabilities = client.file_capabilities().await?;
            println!("Astra Files/{}", capabilities.version);
            println!("max chunk: {} bytes", capabilities.max_chunk_size);
            println!("resumable uploads: {}", capabilities.resumable_uploads);
            println!(
                "atomic upload commit: {}",
                capabilities.atomic_upload_commit
            );
            println!("chunk SHA-256: {}", capabilities.chunk_sha256);
        }
        FileCommand::Ls { path } => {
            let path = remote_path_bytes(&path);
            let mut cursor = Vec::new();
            loop {
                let page = client.file_list(path.clone(), cursor, 200).await?;
                for entry in page.entries {
                    print_file_metadata(&entry);
                }
                if page.next_cursor.is_empty() {
                    break;
                }
                cursor = page.next_cursor;
            }
        }
        FileCommand::Stat { path, no_follow } => {
            let stat = client
                .file_stat(remote_path_bytes(&path), !no_follow)
                .await?;
            let metadata = stat
                .metadata
                .context("server returned an empty file stat response")?;
            print_file_metadata(&metadata);
            println!("path: {}", String::from_utf8_lossy(&metadata.path));
            println!("modified: {} ns since epoch", metadata.modified_unix_ns);
        }
        FileCommand::Put {
            local,
            remote,
            overwrite,
        } => upload_file(&mut client, &local, &remote, overwrite).await?,
        FileCommand::Get {
            remote,
            local,
            overwrite,
        } => {
            let local = match local {
                Some(path) => path,
                None => PathBuf::from(
                    remote
                        .file_name()
                        .context("remote path has no file name; provide a local destination")?,
                ),
            };
            download_file(&mut client, &remote, &local, overwrite).await?;
        }
        FileCommand::Mkdir { path } => {
            println!("{}", client.make_directory(remote_path_bytes(&path)).await?);
        }
        FileCommand::Rm { path } => {
            println!("{}", client.remove_file(remote_path_bytes(&path)).await?);
        }
        FileCommand::Mv {
            source,
            destination,
            overwrite,
        } => {
            println!(
                "{}",
                client
                    .rename_file(
                        remote_path_bytes(&source),
                        remote_path_bytes(&destination),
                        overwrite,
                    )
                    .await?
            );
        }
    }
    Ok(())
}

async fn upload_file(
    client: &mut AstraClient,
    local: &Path,
    remote: &Path,
    overwrite: bool,
) -> Result<()> {
    let metadata = fs::metadata(local)
        .with_context(|| format!("failed to inspect local file {}", local.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", local.display())
    }
    let local_path = local.to_path_buf();
    let expected_sha256 =
        tokio::task::spawn_blocking(move || sha256_local_file(&local_path)).await??;
    let capabilities = client.file_capabilities().await?;
    if capabilities.version != 1
        || !capabilities.resumable_uploads
        || !capabilities.atomic_upload_commit
        || !capabilities.chunk_sha256
    {
        bail!("server does not provide the required Astra Files/1 upload capabilities")
    }
    let chunk_size = usize::try_from(capabilities.max_chunk_size)
        .unwrap_or(1024 * 1024)
        .clamp(1, 1024 * 1024);
    let request = BeginUploadRequest {
        transfer_id: Uuid::new_v4().to_string(),
        path: remote_path_bytes(remote),
        size: metadata.len(),
        sha256: expected_sha256,
        overwrite,
        mode: metadata.permissions().mode() & 0o777,
    };
    let mut status = begin_upload_with_reconnect(client, &request).await?;
    if status.state == "completed" {
        println!("already uploaded: {}", remote.display());
        return Ok(());
    }
    let mut file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("failed to open local file {}", local.display()))?;
    let mut offset = status.committed_offset;
    while offset < request.size {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let remaining =
            usize::try_from((request.size - offset).min(chunk_size as u64)).unwrap_or(chunk_size);
        let mut data = vec![0_u8; remaining];
        file.read_exact(&mut data).await.with_context(|| {
            format!(
                "local file {} changed while being uploaded",
                local.display()
            )
        })?;
        let chunk = WriteFileChunkRequest {
            transfer_id: request.transfer_id.clone(),
            offset,
            sha256: Sha256::digest(&data).to_vec(),
            data,
        };
        status = write_chunk_with_reconnect(client, &request, chunk).await?;
        offset = status.committed_offset;
        eprint!(
            "\rUploading {}: {offset}/{} bytes",
            local.display(),
            request.size
        );
        std::io::stderr().flush()?;
    }
    let committed = commit_upload_with_reconnect(client, &request).await?;
    eprintln!();
    if committed.state != "completed" {
        bail!("server did not complete upload {}", request.transfer_id)
    }
    println!("uploaded {} -> {}", local.display(), remote.display());
    Ok(())
}

async fn download_file(
    client: &mut AstraClient,
    remote: &Path,
    local: &Path,
    overwrite: bool,
) -> Result<()> {
    if local.exists() && !overwrite {
        bail!(
            "local destination {} already exists; pass --overwrite to replace it",
            local.display()
        )
    }
    let remote_path = remote_path_bytes(remote);
    let mut download = begin_download_with_reconnect(client, remote_path.clone()).await?;
    let metadata = download
        .metadata
        .clone()
        .context("server returned download metadata without a file")?;
    if FileKind::try_from(metadata.kind).unwrap_or(FileKind::Unspecified) != FileKind::Regular {
        bail!("remote path {} is not a regular file", remote.display())
    }
    if download.sha256.len() != 32 || download.snapshot.len() != 32 {
        bail!("server returned incomplete download integrity metadata")
    }
    let (temporary, snapshot_file) = download_temporary_paths(local)?;
    prepare_download_temporary(&temporary, &snapshot_file, &download.snapshot).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&temporary)
        .await
        .with_context(|| format!("failed to open {}", temporary.display()))?;
    let mut offset = file.metadata().await?.len();
    if offset > metadata.size {
        file.set_len(0).await?;
        offset = 0;
    }
    let chunk_size = usize::try_from(download.max_chunk_size)
        .unwrap_or(1024 * 1024)
        .clamp(1, 1024 * 1024);
    while offset < metadata.size {
        let request = ReadFileChunkRequest {
            path: remote_path.clone(),
            snapshot: download.snapshot.clone(),
            offset,
            length: u32::try_from(chunk_size).unwrap_or(1024 * 1024),
        };
        let (next_download, chunk) =
            read_chunk_with_reconnect(client, remote_path.clone(), &download, request).await?;
        download = next_download;
        if chunk.offset != offset || chunk.data.is_empty() {
            bail!("server returned a non-progressing download chunk")
        }
        if Sha256::digest(&chunk.data).as_slice() != chunk.sha256.as_slice() {
            bail!("downloaded chunk failed SHA-256 verification")
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(&chunk.data).await?;
        offset += chunk.data.len() as u64;
        eprint!(
            "\rDownloading {}: {offset}/{} bytes",
            remote.display(),
            metadata.size
        );
        std::io::stderr().flush()?;
    }
    file.sync_all().await?;
    drop(file);
    let verify_path = temporary.clone();
    let local_sha256 =
        tokio::task::spawn_blocking(move || sha256_local_file(&verify_path)).await??;
    if local_sha256 != download.sha256 {
        bail!("complete download failed SHA-256 verification; partial file was retained")
    }
    fs::set_permissions(
        &temporary,
        fs::Permissions::from_mode(metadata.mode & 0o777),
    )?;
    fs::rename(&temporary, local).with_context(|| {
        format!(
            "failed to commit download {} to {}",
            temporary.display(),
            local.display()
        )
    })?;
    let _ = fs::remove_file(snapshot_file);
    eprintln!();
    println!("downloaded {} -> {}", remote.display(), local.display());
    Ok(())
}

async fn begin_upload_with_reconnect(
    client: &mut AstraClient,
    request: &BeginUploadRequest,
) -> Result<astra_shell::protocol::UploadStatusResponse> {
    loop {
        match client.begin_upload(request.clone()).await {
            Ok(status) => return Ok(status),
            Err(error) if is_server_error(&error) => return Err(error),
            Err(error) => reconnect_file_client(client, &error).await?,
        }
    }
}

async fn write_chunk_with_reconnect(
    client: &mut AstraClient,
    begin: &BeginUploadRequest,
    chunk: WriteFileChunkRequest,
) -> Result<astra_shell::protocol::UploadStatusResponse> {
    match client.write_file_chunk(chunk).await {
        Ok(status) => Ok(status),
        Err(error) if is_server_error(&error) => Err(error),
        Err(error) => {
            reconnect_file_client(client, &error).await?;
            begin_upload_with_reconnect(client, begin).await
        }
    }
}

async fn commit_upload_with_reconnect(
    client: &mut AstraClient,
    begin: &BeginUploadRequest,
) -> Result<astra_shell::protocol::UploadStatusResponse> {
    loop {
        match client.commit_upload(begin.transfer_id.clone()).await {
            Ok(status) => return Ok(status),
            Err(error) if is_server_error(&error) => return Err(error),
            Err(error) => {
                reconnect_file_client(client, &error).await?;
                let status = begin_upload_with_reconnect(client, begin).await?;
                if status.state == "completed" {
                    return Ok(status);
                }
            }
        }
    }
}

async fn begin_download_with_reconnect(
    client: &mut AstraClient,
    path: Vec<u8>,
) -> Result<BeginDownloadResponse> {
    loop {
        match client.begin_download(path.clone(), true).await {
            Ok(download) => return Ok(download),
            Err(error) if is_server_error(&error) => return Err(error),
            Err(error) => reconnect_file_client(client, &error).await?,
        }
    }
}

async fn read_chunk_with_reconnect(
    client: &mut AstraClient,
    path: Vec<u8>,
    download: &BeginDownloadResponse,
    request: ReadFileChunkRequest,
) -> Result<(
    BeginDownloadResponse,
    astra_shell::protocol::FileChunkResponse,
)> {
    let mut current = download.clone();
    loop {
        match client.read_file_chunk(request.clone()).await {
            Ok(chunk) => return Ok((current, chunk)),
            Err(error) if is_server_error(&error) => return Err(error),
            Err(error) => {
                reconnect_file_client(client, &error).await?;
                let resumed = begin_download_with_reconnect(client, path.clone()).await?;
                if resumed.snapshot != download.snapshot || resumed.sha256 != download.sha256 {
                    bail!("remote file changed while reconnecting; download was not resumed")
                }
                current = resumed;
            }
        }
    }
}

async fn reconnect_file_client(client: &mut AstraClient, error: &anyhow::Error) -> Result<()> {
    eprintln!("\n[astra: file connection lost ({error:#}); reconnecting]");
    let mut delay = Duration::from_millis(250);
    loop {
        match client.reconnect().await {
            Ok(next) => {
                *client = next;
                eprintln!("[astra: file connection restored]");
                return Ok(());
            }
            Err(error) => eprintln!("[astra: file reconnect failed: {error:#}]"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

fn is_server_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ServerResponseError>().is_some()
}

fn remote_path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

fn print_file_metadata(metadata: &FileMetadata) {
    let kind = match FileKind::try_from(metadata.kind).unwrap_or(FileKind::Unspecified) {
        FileKind::Regular => '-',
        FileKind::Directory => 'd',
        FileKind::Symlink => 'l',
        FileKind::Other | FileKind::Unspecified => '?',
    };
    println!(
        "{}{mode:03o}  {size:>12}  {name}",
        kind,
        mode = metadata.mode & 0o777,
        size = metadata.size,
        name = String::from_utf8_lossy(&metadata.name),
    );
}

fn sha256_local_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let length = std::io::Read::read(&mut file, &mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(digest.finalize().to_vec())
}

fn download_temporary_paths(destination: &Path) -> Result<(PathBuf, PathBuf)> {
    let name = destination
        .file_name()
        .context("local download destination has no file name")?;
    let parent = destination.parent().unwrap_or(Path::new("."));
    let mut temporary_name = name.to_os_string();
    temporary_name.push(".astra-part");
    let temporary = parent.join(temporary_name);
    let mut snapshot_name = temporary.as_os_str().to_os_string();
    snapshot_name.push(".snapshot");
    Ok((temporary, PathBuf::from(snapshot_name)))
}

async fn prepare_download_temporary(
    temporary: &Path,
    snapshot_file: &Path,
    snapshot: &[u8],
) -> Result<()> {
    let reusable = match tokio::fs::read(snapshot_file).await {
        Ok(existing) => existing == snapshot && temporary.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !reusable {
        match tokio::fs::remove_file(temporary).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    tokio::fs::write(snapshot_file, snapshot).await?;
    Ok(())
}

async fn attach_terminal(
    mut client: AstraClient,
    terminal_selector: String,
    read_only: bool,
    takeover: bool,
) -> Result<()> {
    let (mut send, mut recv, attached) = client
        .attach(terminal_selector, read_only, takeover, String::new())
        .await?;
    let terminal_id = attached
        .terminal
        .as_ref()
        .context("server returned an attach response without terminal identity")?
        .id
        .clone();
    let mut resume_token = attached.resume_token.clone();
    render_attached_screen(&attached, false)?;

    let interactive = std::io::stdin().is_terminal() && !read_only;
    let _raw_guard = if interactive {
        enable_raw_mode().context("failed to enter raw terminal mode")?;
        Some(RawModeGuard)
    } else {
        None
    };
    let mut stdin = tokio::io::stdin();
    let mut input = [0_u8; 16 * 1024];
    let mut window_changes = window_change_source()?;
    let mut lease_id = attached.lease_id;

    'attachment: loop {
        let mut sequence = 1_u64;
        let initial_disconnect = if !read_only {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            match send_terminal_command(
                &mut send,
                TerminalCommand {
                    terminal_id: terminal_id.clone(),
                    lease_id: lease_id.clone(),
                    sequence,
                    command: Some(terminal_command::Command::Resize(Resize {
                        rows: rows as u32,
                        cols: cols as u32,
                    })),
                },
            )
            .await
            {
                Ok(()) => {
                    sequence += 1;
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };

        let disconnect = if let Some(error) = initial_disconnect {
            error
        } else {
            loop {
                let disconnect = tokio::select! {
                    incoming = read_message(&mut recv) => {
                        match incoming {
                            Ok(Some(WireMessage { body: Some(wire_message::Body::TerminalEvent(event)) })) => {
                                if event.terminal_id != terminal_id {
                                    return Err(anyhow!("server sent an event for the wrong terminal"));
                                }
                                match event.event {
                                    Some(terminal_event::Event::Output(bytes)) => {
                                        std::io::stdout().write_all(&bytes)?;
                                        std::io::stdout().flush()?;
                                    }
                                    Some(terminal_event::Event::Exited(code)) => {
                                        eprintln!("\r\n[astra: terminal exited with status {code}]");
                                        let _ = send.finish();
                                        let _ = read_message(&mut recv).await;
                                        return Ok(());
                                    }
                                    Some(terminal_event::Event::Error(message)) => {
                                        eprintln!("\r\n[astra: {message}]");
                                    }
                                    Some(terminal_event::Event::Interactive(_)) => {}
                                    Some(terminal_event::Event::Snapshot(snapshot)) => {
                                        render_snapshot_to_stdout(&snapshot, true)?;
                                    }
                                    None => {}
                                }
                                continue;
                            }
                            Ok(Some(_)) => return Err(anyhow!("unexpected message on attach stream")),
                            Ok(None) => anyhow!("attachment stream ended"),
                            Err(error) => error,
                        }
                    }
                    read = stdin.read(&mut input), if !read_only => {
                        let length = read?;
                        if length == 0 || (interactive && input[..length].contains(&0x1d)) {
                            let _ = send_terminal_command(
                                &mut send,
                                TerminalCommand {
                                    terminal_id: terminal_id.clone(),
                                    lease_id: lease_id.clone(),
                                    sequence,
                                    command: Some(terminal_command::Command::Detach(true)),
                                },
                            ).await;
                            let _ = send.finish();
                            return Ok(());
                        }
                        match send_terminal_command(
                            &mut send,
                            TerminalCommand {
                                terminal_id: terminal_id.clone(),
                                lease_id: lease_id.clone(),
                                sequence,
                                command: Some(terminal_command::Command::Input(input[..length].to_vec())),
                            },
                        ).await {
                            Ok(()) => {
                                sequence += 1;
                                continue;
                            }
                            Err(error) => error,
                        }
                    }
                    _ = wait_for_window_change(&mut window_changes), if interactive => {
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        match send_terminal_command(
                            &mut send,
                            TerminalCommand {
                                terminal_id: terminal_id.clone(),
                                lease_id: lease_id.clone(),
                                sequence,
                                command: Some(terminal_command::Command::Resize(Resize {
                                    rows: rows as u32,
                                    cols: cols as u32,
                                })),
                            },
                        ).await {
                            Ok(()) => {
                                sequence += 1;
                                continue;
                            }
                            Err(error) => error,
                        }
                    }
                };
                break disconnect;
            }
        };
        eprintln!("\r\n[astra: connection lost ({disconnect:#}); reconnecting]");
        let (next_client, next_send, next_recv, next_attached) = {
            let reconnect = reconnect_terminal(&client, &terminal_id, read_only, &resume_token);
            tokio::pin!(reconnect);
            if read_only {
                reconnect.await?
            } else {
                let mut warned_about_dropped_input = false;
                loop {
                    tokio::select! {
                        result = &mut reconnect => break result?,
                        read = stdin.read(&mut input) => {
                            let length = read?;
                            if length == 0
                                || (interactive
                                    && (input[..length].contains(&0x1d)
                                        || input[..length].contains(&0x03)))
                            {
                                return Ok(());
                            }
                            if !warned_about_dropped_input {
                                eprintln!(
                                    "\r[astra: input entered while disconnected is being discarded]"
                                );
                                warned_about_dropped_input = true;
                            }
                        }
                    }
                }
            }
        };
        client = next_client;
        send = next_send;
        recv = next_recv;
        lease_id = next_attached.lease_id.clone();
        if !next_attached.resume_token.is_empty() {
            resume_token = next_attached.resume_token.clone();
        }
        render_attached_screen(&next_attached, interactive)?;
        continue 'attachment;
    }
}

fn render_attached_screen(attached: &AttachResponse, reset: bool) -> Result<()> {
    if let Some(snapshot) = &attached.snapshot {
        render_snapshot_to_stdout(snapshot, true)
    } else {
        if reset {
            std::io::stdout().write_all(b"\x1bc")?;
        }
        std::io::stdout().write_all(&attached.history)?;
        std::io::stdout().flush()?;
        Ok(())
    }
}

fn render_snapshot_to_stdout(snapshot: &TerminalSnapshot, reset: bool) -> Result<()> {
    if reset {
        std::io::stdout().write_all(b"\x1bc")?;
    }
    if snapshot.alternate_screen {
        std::io::stdout().write_all(&snapshot.normal_contents)?;
        std::io::stdout().write_all(b"\x1b[?1049h")?;
    }
    std::io::stdout().write_all(&snapshot.contents)?;
    std::io::stdout().flush()?;
    Ok(())
}

async fn reconnect_terminal(
    previous: &AstraClient,
    terminal_id: &str,
    read_only: bool,
    resume_token: &str,
) -> Result<(
    AstraClient,
    quinn::SendStream,
    quinn::RecvStream,
    AttachResponse,
)> {
    let mut delay = Duration::from_millis(250);
    loop {
        match previous.reconnect().await {
            Ok(client) => match client.list().await {
                Ok(terminals) if !terminals.iter().any(|terminal| terminal.id == terminal_id) => {
                    bail!("terminal is no longer active on the server")
                }
                Ok(_) => match client
                    .attach(
                        terminal_id.to_owned(),
                        read_only,
                        false,
                        resume_token.to_owned(),
                    )
                    .await
                {
                    Ok((send, recv, attached)) => {
                        return Ok((client, send, recv, attached));
                    }
                    Err(error) => {
                        eprintln!(
                            "\r[astra: server reachable; waiting to resume terminal ({error:#})]"
                        );
                    }
                },
                Err(error) => {
                    eprintln!("\r[astra: reconnect check failed: {error:#}]");
                }
            },
            Err(error) => {
                eprintln!("\r[astra: reconnect failed: {error:#}]");
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

async fn send_terminal_command(
    send: &mut quinn::SendStream,
    command: TerminalCommand,
) -> Result<()> {
    write_message(
        send,
        &WireMessage::new(wire_message::Body::TerminalCommand(command)),
    )
    .await
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn truncate(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn shell_join(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| {
            if argument
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_./-".contains(character))
            {
                argument.clone()
            } else {
                format!("{:?}", argument)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_id(terminal: &astra_shell::protocol::TerminalInfo) -> String {
    match terminal.display_id {
        0 => "-".into(),
        display_id => display_id.to_string(),
    }
}

fn terminal_reference(terminal: &astra_shell::protocol::TerminalInfo) -> String {
    match terminal.display_id {
        0 => terminal.id.clone(),
        display_id => display_id.to_string(),
    }
}

fn default_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn client_term() -> Result<String> {
    match std::env::var_os("TERM") {
        Some(value) => value
            .into_string()
            .map_err(|_| anyhow!("TERM is not valid UTF-8")),
        None => Ok("xterm-256color".into()),
    }
}

fn client_locale_environment() -> Result<Vec<EnvironmentVariable>> {
    let mut environment = Vec::new();
    for &name in LOCALE_ENVIRONMENT_VARIABLES {
        let Some(value) = std::env::var_os(name) else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| anyhow!("{name} is not valid UTF-8"))?;
        if !value.is_empty() {
            environment.push(EnvironmentVariable {
                name: name.into(),
                value,
            });
        }
    }
    Ok(environment)
}

#[derive(Debug, Default, Eq, PartialEq)]
struct SshOptions {
    strict_host_key_checking: StrictHostKeyChecking,
    user_known_hosts_file: Option<PathBuf>,
}

fn parse_ssh_options(options: &[String]) -> Result<SshOptions> {
    let mut parsed = SshOptions::default();
    for option in options {
        let option = option.trim();
        let separator = option
            .find('=')
            .or_else(|| option.find(char::is_whitespace))
            .with_context(|| format!("SSH-style option {option:?} must be KEY=VALUE"))?;
        let key = option[..separator].trim().to_ascii_lowercase();
        let value = option[separator + 1..].trim();
        if value.is_empty() {
            bail!("SSH-style option {option:?} has an empty value")
        }
        match key.as_str() {
            "stricthostkeychecking" => {
                parsed.strict_host_key_checking = value.parse()?;
            }
            "userknownhostsfile" => {
                parsed.user_known_hosts_file = Some(PathBuf::from(value));
            }
            _ => bail!(
                "unsupported SSH-style option {:?}; Astra currently supports StrictHostKeyChecking and UserKnownHostsFile",
                &option[..separator]
            ),
        }
    }
    Ok(parsed)
}

fn expand_home_path(path: &Path) -> Result<PathBuf> {
    let Ok(remainder) = path.strip_prefix("~") else {
        return Ok(path.to_owned());
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot expand ~ because the user home directory is unknown")?;
    Ok(home.join(remainder))
}

#[derive(Debug, PartialEq, Eq)]
struct Destination {
    username: Option<String>,
    host: String,
}

fn parse_destination(value: &str) -> Result<Destination> {
    let (username, host) = match value.rsplit_once('@') {
        Some((username, host)) => {
            if username.is_empty() {
                bail!("destination username cannot be empty")
            }
            (Some(username.to_owned()), host)
        }
        None => (None, value),
    };
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        bail!("destination host cannot be empty")
    }
    Ok(Destination {
        username,
        host: host.to_owned(),
    })
}

async fn resolve_address(host: &str, port: u16) -> Result<SocketAddr> {
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve {host}"))?
        .collect();
    addresses
        .iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| addresses.first().copied())
        .ok_or_else(|| anyhow!("{host} did not resolve to an IP address"))
}

fn inferred_server_name(_host: &str) -> String {
    "astra.local".into()
}

fn select_identity(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot locate the user home directory; pass an identity with -i")?;
    for name in ["id_ed25519", "id_rsa"] {
        let identity = home.join(".ssh").join(name);
        if identity.is_file() {
            return Ok(identity);
        }
    }
    bail!(
        "no supported default SSH identity found at {}/.ssh/id_ed25519 or id_rsa; pass one with -i",
        home.display()
    )
}

#[cfg(unix)]
type WindowChangeSource = tokio::signal::unix::Signal;

#[cfg(unix)]
fn window_change_source() -> Result<WindowChangeSource> {
    Ok(tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::window_change(),
    )?)
}

#[cfg(unix)]
async fn wait_for_window_change(source: &mut WindowChangeSource) {
    let _ = source.recv().await;
}

#[cfg(not(unix))]
struct WindowChangeSource;

#[cfg(not(unix))]
fn window_change_source() -> Result<WindowChangeSource> {
    Ok(WindowChangeSource)
}

#[cfg(not(unix))]
async fn wait_for_window_change(_source: &mut WindowChangeSource) {
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_style_destination() {
        assert_eq!(
            parse_destination("mimi@127.0.0.1").unwrap(),
            Destination {
                username: Some("mimi".into()),
                host: "127.0.0.1".into(),
            }
        );
        assert_eq!(
            parse_destination("[::1]").unwrap(),
            Destination {
                username: None,
                host: "::1".into(),
            }
        );
    }

    #[test]
    fn parses_destination_before_optional_subcommand() {
        let cli = Cli::try_parse_from(["astra", "-p", "4443", "mimi@localhost", "list"]).unwrap();
        assert_eq!(cli.port, 4443);
        assert_eq!(cli.destination, "mimi@localhost");
        assert!(cli.server_cert.is_none());
        assert!(matches!(cli.command, Some(Command::List { long: false })));

        let cli = Cli::try_parse_from(["astra", "mimi@localhost", "list", "--long"]).unwrap();
        assert!(matches!(cli.command, Some(Command::List { long: true })));
    }

    #[test]
    fn parses_supported_ssh_host_options() {
        let cli = Cli::try_parse_from([
            "astra",
            "-oStrictHostKeyChecking=accept-new",
            "-o",
            "UserKnownHostsFile=~/astra_hosts",
            "mimi@localhost",
        ])
        .unwrap();
        let options = parse_ssh_options(&cli.ssh_options).unwrap();
        assert_eq!(
            options.strict_host_key_checking,
            StrictHostKeyChecking::AcceptNew
        );
        assert_eq!(
            options.user_known_hosts_file,
            Some(PathBuf::from("~/astra_hosts"))
        );
    }

    #[test]
    fn uses_generated_certificate_name_for_ip_destinations() {
        assert_eq!(inferred_server_name("203.0.113.7"), "astra.local");
    }
}
