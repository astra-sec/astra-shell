#![cfg(unix)]

use anyhow::{Context, Result};
use astra_shell::{
    accounts::SystemAccount,
    resources::ResourcePolicy,
    server::{ServerMode, ServerOptions, ServerPaths, initialize_state, serve},
    terminal_engine::TerminalEngine,
    terminal_state_v2::ScreenKind,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    time::Duration,
};

struct Cleanup<F: FnMut()>(F);
impl<F: FnMut()> Drop for Cleanup<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

async fn wait_text(
    receiver: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    display: &mut TerminalEngine,
    expected: &str,
) -> Result<()> {
    let mut last_text = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = display.semantic_viewport()?;
            let screen = if state.active_screen == ScreenKind::Alternate as i32 {
                state.alternate.as_ref()
            } else {
                state.primary.as_ref()
            }
            .unwrap();
            let text = screen
                .included_rows
                .iter()
                .map(|row| {
                    let mut line = String::new();
                    let mut column = 0;
                    for cell in &row.cells {
                        while column < cell.column {
                            line.push(' ');
                            column += 1;
                        }
                        line.push_str(&cell.grapheme);
                        column = cell.column + cell.width;
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n");
            last_text.clone_from(&text);
            if text.contains(expected) {
                return Ok(());
            }
            let bytes = receiver.recv().await.with_context(|| {
                format!("CLI exited before display {expected:?}; last screen: {last_text}")
            })?;
            display.advance(&bytes);
        }
    })
    .await
    .with_context(|| {
        format!("timed out waiting for CLI display {expected:?}; last screen: {last_text}")
    })?
}

/// Exercise the built CLI inside a real PTY, not only its library API. This
/// covers raw input, semantic rendering, SIGWINCH, remote stty size, and exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_cli_is_interactive_and_propagates_window_size() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let paths = ServerPaths::new(temporary.path().join("state"));
    initialize_state(&paths)?;
    let identity = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)?;
    let key_path = temporary.path().join("id_ed25519");
    fs::write(&key_path, identity.to_openssh(LineEnding::LF)?)?;
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    fs::write(
        &paths.authorized_keys,
        format!("{}\n", identity.public_key().to_openssh()?),
    )?;
    let home = temporary.path().join("home");
    fs::create_dir(&home)?;
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let listen = reservation.local_addr()?;
    drop(reservation);
    let server = tokio::spawn(serve(ServerOptions {
        listen,
        paths: paths.clone(),
        mode: ServerMode::Rootless { session_root: home },
        resource_policy: ResourcePolicy::default(),
    }));
    let _server_cleanup = Cleanup(|| server.abort());
    tokio::time::sleep(Duration::from_millis(100)).await;

    let pty = native_pty_system().openpty(PtySize {
        rows: 31,
        cols: 103,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_astra"));
    command.args([
        "--streaming",
        "-p",
        &listen.port().to_string(),
        "--server-cert",
    ]);
    command.arg(&paths.cert);
    command.arg("-i");
    command.arg(&key_path);
    command.arg(format!("{}@127.0.0.1", SystemAccount::current()?.username));
    command.args(["new", "--attach", "--", "/bin/sh", "-c", "printf '__READY__\\n'; while IFS= read -r line; do [ \"$line\" = quit ] && exit 0; stty size; printf 'ECHO:%s\\n' \"$line\"; done"]);
    command.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(command)?;
    let mut killer = child.clone_killer();
    let _client_cleanup = Cleanup(move || {
        let _ = killer.kill();
    });
    drop(pty.slave);
    let mut input = pty.master.take_writer()?;
    let mut output = pty.master.try_clone_reader()?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
    let reader = std::thread::spawn(move || {
        let mut bytes = [0u8; 32 * 1024];
        while let Ok(count) = output.read(&mut bytes) {
            if count == 0 || sender.blocking_send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut display = TerminalEngine::new(31, 103, 128, Box::new(std::io::sink()))?;
    wait_text(&mut receiver, &mut display, "__READY__").await?;
    input.write_all(b"hello\r")?;
    input.flush()?;
    wait_text(&mut receiver, &mut display, "ECHO:hello").await?;
    wait_text(&mut receiver, &mut display, "31 103").await?;
    pty.master.resize(PtySize {
        rows: 42,
        cols: 132,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    display.resize(42, 132, 0, 0)?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    input.write_all(b"resized\r")?;
    input.flush()?;
    wait_text(&mut receiver, &mut display, "42 132").await?;
    input.write_all(b"quit\r")?;
    input.flush()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "CLI did not exit successfully");
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("CLI did not terminate")??;
    drop(input);
    drop(pty.master);
    drop(receiver);
    reader.join().expect("PTY reader panicked");
    Ok(())
}
