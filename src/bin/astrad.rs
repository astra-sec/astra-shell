use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Result;
use astra_shell::{
    server::{ServerMode, ServerOptions, ServerPaths, initialize_state, serve},
    worker::{DEFAULT_WORKER_IDLE_TIMEOUT_SECONDS, serve_worker},
};
use clap::{Parser, Subcommand};

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
    },
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
        } => {
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
            })
            .await
        }
        Command::Worker {
            socket,
            state_dir,
            session_root,
            expected_uid,
            idle_timeout_seconds,
        } => {
            serve_worker(
                socket,
                state_dir,
                session_root,
                expected_uid,
                Duration::from_secs(idle_timeout_seconds),
            )
            .await
        }
    }
}
