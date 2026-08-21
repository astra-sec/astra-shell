use std::{
    io::{IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use astra_shell::{
    client::AstraClient,
    protocol::{
        Resize, SpawnRequest, TerminalCommand, WireMessage, read_message, terminal_command,
        terminal_event, wire_message, write_message,
    },
};
use clap::{Args, Parser, Subcommand};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::AsyncReadExt;

#[derive(Debug, Parser)]
#[command(name = "astra", version, about = "Astra persistent terminal client")]
struct Cli {
    /// Server UDP port, equivalent to ssh -p.
    #[arg(short = 'p', long, default_value_t = 4433)]
    port: u16,
    /// TLS server name. Defaults to astra.local for astrad-generated certificates.
    #[arg(long)]
    server_name: Option<String>,
    #[arg(long, default_value = "state/host-cert.der")]
    server_cert: PathBuf,
    /// OpenSSH private key. Defaults to ~/.ssh/id_ed25519.
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
    /// List known terminals, including exited and daemon-lost records.
    List,
    /// Create a terminal. Arguments after `--` are executed directly, without a shell.
    New(NewArgs),
    /// Attach to a running terminal.
    Attach(AttachArgs),
    /// Terminate a running terminal.
    Close { terminal_id: String },
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
    let client = AstraClient::connect(
        address,
        &server_name,
        &cli.server_cert,
        &identity,
        &username,
    )
    .await?;
    match cli.command {
        None => {
            let terminal = client
                .spawn(SpawnRequest {
                    name: String::new(),
                    argv: Vec::new(),
                    cwd: String::new(),
                    rows: 24,
                    cols: 80,
                })
                .await?;
            attach_terminal(&client, terminal.id, false, false).await?;
        }
        Some(Command::List) => {
            let terminals = client.list().await?;
            if terminals.is_empty() {
                println!("No terminals.");
            } else {
                println!("{:<36}  {:<12}  {:<8}  COMMAND", "ID", "NAME", "STATUS");
                for terminal in terminals {
                    println!(
                        "{:<36}  {:<12}  {:<8}  {}",
                        terminal.id,
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
                })
                .await?;
            println!("{}", terminal.id);
            if attach {
                attach_terminal(&client, terminal.id, false, false).await?;
            }
        }
        Some(Command::Attach(arguments)) => {
            attach_terminal(
                &client,
                arguments.terminal_id,
                arguments.read_only,
                arguments.takeover,
            )
            .await?;
        }
        Some(Command::Close { terminal_id }) => {
            println!("{}", client.close(terminal_id).await?)
        }
    }
    Ok(())
}

async fn attach_terminal(
    client: &AstraClient,
    terminal_id: String,
    read_only: bool,
    takeover: bool,
) -> Result<()> {
    let (mut send, mut recv, attached) = client
        .attach(terminal_id.clone(), read_only, takeover)
        .await?;
    std::io::stdout().write_all(&attached.history)?;
    std::io::stdout().flush()?;

    let interactive = std::io::stdin().is_terminal() && !read_only;
    let _raw_guard = if interactive {
        enable_raw_mode().context("failed to enter raw terminal mode")?;
        Some(RawModeGuard)
    } else {
        None
    };
    let mut stdin = tokio::io::stdin();
    let mut input = [0_u8; 16 * 1024];
    let mut sequence = 1_u64;
    let mut window_changes = window_change_source()?;

    if !read_only {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let _ = send_terminal_command(
            &mut send,
            TerminalCommand {
                terminal_id: terminal_id.clone(),
                lease_id: attached.lease_id.clone(),
                sequence,
                command: Some(terminal_command::Command::Resize(Resize {
                    rows: rows as u32,
                    cols: cols as u32,
                })),
            },
        )
        .await;
        sequence += 1;
    }

    loop {
        tokio::select! {
            incoming = read_message(&mut recv) => {
                match incoming? {
                    Some(WireMessage { body: Some(wire_message::Body::TerminalEvent(event)) }) => {
                        match event.event {
                            Some(terminal_event::Event::Output(bytes)) => {
                                std::io::stdout().write_all(&bytes)?;
                                std::io::stdout().flush()?;
                            }
                            Some(terminal_event::Event::Exited(code)) => {
                                eprintln!("\r\n[astra: terminal exited with status {code}]");
                                let _ = send.finish();
                                let _ = read_message(&mut recv).await;
                                break;
                            }
                            Some(terminal_event::Event::Error(message)) => {
                                eprintln!("\r\n[astra: {message}]");
                            }
                            None => {}
                        }
                    }
                    Some(_) => bail!("unexpected message on attach stream"),
                    None => break,
                }
            }
            read = stdin.read(&mut input), if !read_only => {
                let length = read?;
                if length == 0 {
                    let _ = send_terminal_command(
                        &mut send,
                        TerminalCommand {
                            terminal_id: terminal_id.clone(),
                            lease_id: attached.lease_id.clone(),
                            sequence,
                            command: Some(terminal_command::Command::Detach(true)),
                        },
                    ).await;
                    let _ = send.finish();
                    break;
                }
                if interactive && input[..length].contains(&0x1d) {
                    let _ = send_terminal_command(
                        &mut send,
                        TerminalCommand {
                            terminal_id: terminal_id.clone(),
                            lease_id: attached.lease_id.clone(),
                            sequence,
                            command: Some(terminal_command::Command::Detach(true)),
                        },
                    ).await;
                    let _ = send.finish();
                    break;
                }
                send_terminal_command(
                    &mut send,
                    TerminalCommand {
                        terminal_id: terminal_id.clone(),
                        lease_id: attached.lease_id.clone(),
                        sequence,
                        command: Some(terminal_command::Command::Input(input[..length].to_vec())),
                    },
                ).await?;
                sequence += 1;
            }
            _ = wait_for_window_change(&mut window_changes), if interactive => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                send_terminal_command(
                    &mut send,
                    TerminalCommand {
                        terminal_id: terminal_id.clone(),
                        lease_id: attached.lease_id.clone(),
                        sequence,
                        command: Some(terminal_command::Command::Resize(Resize {
                            rows: rows as u32,
                            cols: cols as u32,
                        })),
                    },
                ).await?;
                sequence += 1;
            }
        }
    }
    Ok(())
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

fn default_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
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
    let identity = home.join(".ssh/id_ed25519");
    if identity.is_file() {
        Ok(identity)
    } else {
        bail!(
            "no supported default SSH identity found at {}; pass one with -i",
            identity.display()
        )
    }
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
        assert!(matches!(cli.command, Some(Command::List)));
    }

    #[test]
    fn uses_generated_certificate_name_for_ip_destinations() {
        assert_eq!(inferred_server_name("203.0.113.7"), "astra.local");
    }
}
