use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Result;
use astra_shell::{
    resources::{DEFAULT_TERMINAL_HISTORY_ROWS, ResourceLimits, ResourcePolicy},
    server::{ServerMode, ServerOptions, ServerPaths, initialize_state, serve},
    worker::{DEFAULT_WORKER_IDLE_TIMEOUT_SECONDS, serve_worker},
};
use clap::{Args, Parser, Subcommand};

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "astrad", version, about = "Astra persistent terminal daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a local QUIC host certificate and initialize the metadata store.
    Init {
        #[arg(long, default_value = "state")]
        state_dir: PathBuf,
    },
    /// Run the daemon in the foreground.
    Serve {
        #[arg(long, default_value = "127.0.0.1:4433")]
        listen: SocketAddr,
        #[arg(long, default_value = "state")]
        state_dir: PathBuf,
        /// Enable system-account routing and one unprivileged worker per UID.
        #[arg(long)]
        managed: bool,
        /// Test/deployment override: read keys from DIR/USERNAME instead of ~/.ssh.
        #[arg(long, requires = "managed")]
        authorized_keys_dir: Option<PathBuf>,
        /// Rootless session root, or an explicit managed-mode test override.
        #[arg(long)]
        session_root: Option<PathBuf>,
        /// Stop an empty managed worker after this many idle seconds; 0 disables recycling.
        #[arg(long, default_value_t = DEFAULT_WORKER_IDLE_TIMEOUT_SECONDS)]
        worker_idle_timeout_seconds: u64,
        #[command(flatten)]
        resources: ResourceArgs,
    },
    /// Internal per-user process started by the managed gateway.
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        session_root: PathBuf,
        #[arg(long)]
        expected_uid: u32,
        #[arg(long)]
        idle_timeout_seconds: u64,
        #[command(flatten)]
        resources: UserResourceArgs,
        #[command(flatten)]
        terminal_resources: TerminalResourceArgs,
    },
}

#[derive(Clone, Debug, Args)]
struct ResourceArgs {
    #[command(flatten)]
    global: GlobalResourceArgs,
    #[command(flatten)]
    user: UserResourceArgs,
    #[command(flatten)]
    terminal: TerminalResourceArgs,
}

impl ResourceArgs {
    fn policy(self) -> Result<ResourcePolicy> {
        let policy = ResourcePolicy {
            global: self.global.limits()?,
            user: self.user.limits()?,
            terminal_base_memory_bytes: to_bytes(
                self.terminal.terminal_base_memory_mib,
                "terminal base memory",
            )?,
            terminal_cell_memory_bytes: self.terminal.terminal_cell_memory_bytes,
            terminal_history_rows: self.terminal.terminal_history_rows,
            terminal_history_bytes: to_bytes(
                self.terminal.terminal_history_mib,
                "terminal history",
            )?,
        };
        policy.validate()?;
        Ok(policy)
    }
}

#[derive(Clone, Debug, Args)]
struct GlobalResourceArgs {
    #[arg(long, default_value_t = 1_024)]
    max_global_connections: u64,
    #[arg(long, default_value_t = 8_192)]
    max_global_streams: u64,
    #[arg(long, default_value_t = 64)]
    max_global_workers: u64,
    #[arg(long, default_value_t = 4_096)]
    max_global_terminals: u64,
    #[arg(long, default_value_t = 16_384)]
    max_global_attachments: u64,
    #[arg(long, default_value_t = 16_384)]
    max_global_terminal_memory_mib: u64,
    #[arg(long, default_value_t = 32_768)]
    max_global_history_mib: u64,
    #[arg(long, default_value_t = 16_384)]
    max_global_file_handles: u64,
    #[arg(long, default_value_t = 1_024)]
    max_global_uploads: u64,
    #[arg(long, default_value_t = 524_288)]
    max_global_upload_mib: u64,
}

impl GlobalResourceArgs {
    fn limits(self) -> Result<ResourceLimits> {
        Ok(ResourceLimits {
            connections: self.max_global_connections,
            streams: self.max_global_streams,
            workers: self.max_global_workers,
            terminals: self.max_global_terminals,
            attachments: self.max_global_attachments,
            terminal_memory_bytes: to_bytes(
                self.max_global_terminal_memory_mib,
                "global terminal memory",
            )?,
            history_bytes: to_bytes(self.max_global_history_mib, "global history")?,
            file_handles: self.max_global_file_handles,
            uploads: self.max_global_uploads,
            upload_bytes: to_bytes(self.max_global_upload_mib, "global upload")?,
        })
    }
}

#[derive(Clone, Debug, Args)]
struct UserResourceArgs {
    #[arg(long, default_value_t = 8)]
    max_user_connections: u64,
    #[arg(long, default_value_t = 256)]
    max_user_streams: u64,
    #[arg(long, default_value_t = 64)]
    max_user_terminals: u64,
    #[arg(long, default_value_t = 256)]
    max_user_attachments: u64,
    #[arg(long, default_value_t = 256)]
    max_user_terminal_memory_mib: u64,
    #[arg(long, default_value_t = 512)]
    max_user_history_mib: u64,
    #[arg(long, default_value_t = 256)]
    max_user_file_handles: u64,
    #[arg(long, default_value_t = 16)]
    max_user_uploads: u64,
    #[arg(long, default_value_t = 8_192)]
    max_user_upload_mib: u64,
}

impl UserResourceArgs {
    fn limits(self) -> Result<ResourceLimits> {
        Ok(ResourceLimits {
            connections: self.max_user_connections,
            streams: self.max_user_streams,
            workers: 1,
            terminals: self.max_user_terminals,
            attachments: self.max_user_attachments,
            terminal_memory_bytes: to_bytes(
                self.max_user_terminal_memory_mib,
                "user terminal memory",
            )?,
            history_bytes: to_bytes(self.max_user_history_mib, "user history")?,
            file_handles: self.max_user_file_handles,
            uploads: self.max_user_uploads,
            upload_bytes: to_bytes(self.max_user_upload_mib, "user upload")?,
        })
    }
}

#[derive(Clone, Debug, Args)]
struct TerminalResourceArgs {
    #[arg(long, default_value_t = 4)]
    terminal_base_memory_mib: u64,
    #[arg(long, default_value_t = 64)]
    terminal_cell_memory_bytes: u64,
    #[arg(long, default_value_t = DEFAULT_TERMINAL_HISTORY_ROWS)]
    terminal_history_rows: u64,
    #[arg(long, default_value_t = 8)]
    terminal_history_mib: u64,
}

fn worker_policy(user: UserResourceArgs, terminal: TerminalResourceArgs) -> Result<ResourcePolicy> {
    let user = user.limits()?;
    let policy = ResourcePolicy {
        global: user,
        user,
        terminal_base_memory_bytes: to_bytes(
            terminal.terminal_base_memory_mib,
            "terminal base memory",
        )?,
        terminal_cell_memory_bytes: terminal.terminal_cell_memory_bytes,
        terminal_history_rows: terminal.terminal_history_rows,
        terminal_history_bytes: to_bytes(terminal.terminal_history_mib, "terminal history")?,
    };
    policy.validate()?;
    Ok(policy)
}

fn to_bytes(mebibytes: u64, label: &str) -> Result<u64> {
    mebibytes
        .checked_mul(MIB)
        .ok_or_else(|| anyhow::anyhow!("{label} limit overflows bytes"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "astra_shell=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run().await {
        eprintln!("astrad: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Init { state_dir } => {
            let paths = ServerPaths::new(state_dir);
            initialize_state(&paths)?;
            println!("initialized {}", paths.state_dir.display());
            println!("host certificate: {}", paths.cert.display());
            println!("authorized keys: {}", paths.authorized_keys.display());
            Ok(())
        }
        Command::Serve {
            listen,
            state_dir,
            managed,
            authorized_keys_dir,
            session_root,
            worker_idle_timeout_seconds,
            resources,
        } => {
            let resource_policy = resources.policy()?;
            let mode = if managed {
                ServerMode::Managed {
                    authorized_keys_directory: authorized_keys_dir,
                    session_root_override: session_root,
                    worker_idle_timeout: Duration::from_secs(worker_idle_timeout_seconds),
                }
            } else {
                ServerMode::Rootless {
                    session_root: session_root.unwrap_or(std::env::current_dir()?),
                }
            };
            serve(ServerOptions {
                listen,
                paths: ServerPaths::new(state_dir),
                mode,
                resource_policy,
            })
            .await
        }
        Command::Worker {
            socket,
            state_dir,
            session_root,
            expected_uid,
            idle_timeout_seconds,
            resources,
            terminal_resources,
        } => {
            let resource_policy = worker_policy(resources, terminal_resources)?;
            serve_worker(
                socket,
                state_dir,
                session_root,
                expected_uid,
                Duration::from_secs(idle_timeout_seconds),
                resource_policy,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_rejects_zero_or_inverted_resource_policy() {
        let parsed = Cli::try_parse_from(["astrad", "serve", "--max-user-terminals", "0"]).unwrap();
        let Command::Serve { resources, .. } = parsed.command else {
            panic!("expected serve command")
        };
        assert!(resources.policy().is_err());

        for arguments in [
            ["--terminal-history-rows", "0"],
            ["--terminal-history-rows", "1000001"],
            ["--terminal-history-mib", "1025"],
        ] {
            let parsed =
                Cli::try_parse_from(["astrad", "serve", arguments[0], arguments[1]]).unwrap();
            let Command::Serve { resources, .. } = parsed.command else {
                panic!("expected serve command")
            };
            assert!(resources.policy().is_err());
        }

        let parsed = Cli::try_parse_from([
            "astrad",
            "serve",
            "--max-global-terminals",
            "1",
            "--max-user-terminals",
            "2",
        ])
        .unwrap();
        let Command::Serve { resources, .. } = parsed.command else {
            panic!("expected serve command")
        };
        assert!(resources.policy().is_err());
    }

    #[test]
    fn hidden_worker_receives_assigned_user_capacity() {
        let parsed = Cli::try_parse_from([
            "astrad",
            "worker",
            "--socket",
            "/tmp/astra-test.sock",
            "--state-dir",
            "/tmp/astra-test-state",
            "--session-root",
            "/tmp",
            "--expected-uid",
            "501",
            "--idle-timeout-seconds",
            "60",
            "--max-user-terminals",
            "32",
            "--terminal-history-mib",
            "4",
            "--terminal-history-rows",
            "20000",
        ])
        .unwrap();
        let Command::Worker {
            resources,
            terminal_resources,
            ..
        } = parsed.command
        else {
            panic!("expected worker command")
        };
        let policy = worker_policy(resources, terminal_resources).unwrap();
        assert_eq!(policy.user.terminals, 32);
        assert_eq!(policy.terminal_history_rows, 20_000);
        assert_eq!(policy.terminal_history_bytes, 4 * MIB);
        assert_eq!(policy.global, policy.user);
    }
}
