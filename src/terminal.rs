use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    database::Database,
    protocol::{SpawnRequest, TerminalInfo},
};

const HISTORY_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(i32),
    Error(String),
}

#[derive(Debug)]
struct Lease {
    id: String,
    last_sequence: u64,
}

pub struct Terminal {
    info: RwLock<TerminalInfo>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send>>,
    history: Mutex<VecDeque<u8>>,
    events: broadcast::Sender<PtyEvent>,
    lease: Mutex<Option<Lease>>,
    database: Database,
}

impl Terminal {
    pub fn info(&self) -> TerminalInfo {
        self.info.read().expect("terminal info poisoned").clone()
    }

    pub fn snapshot_and_subscribe(&self) -> (Vec<u8>, broadcast::Receiver<PtyEvent>) {
        // The PTY reader publishes while holding this same lock. Subscribing
        // before copying therefore gives an atomic boundary: bytes are either
        // in the snapshot or in the receiver, never lost or duplicated.
        let history = self.history.lock().expect("terminal history poisoned");
        let receiver = self.events.subscribe();
        let snapshot = history.iter().copied().collect();
        (snapshot, receiver)
    }

    pub fn acquire_lease(&self, read_only: bool, takeover: bool) -> Result<String> {
        if read_only {
            return Ok(String::new());
        }
        let mut lease = self.lease.lock().expect("terminal lease poisoned");
        if lease.is_some() && !takeover {
            bail!("terminal already has an input lease owner; use --read-only or --takeover")
        }
        let id = Uuid::new_v4().to_string();
        *lease = Some(Lease {
            id: id.clone(),
            last_sequence: 0,
        });
        Ok(id)
    }

    pub fn release_lease(&self, lease_id: &str) {
        if lease_id.is_empty() {
            return;
        }
        let mut lease = self.lease.lock().expect("terminal lease poisoned");
        if lease.as_ref().is_some_and(|lease| lease.id == lease_id) {
            *lease = None;
        }
    }

    pub fn write_input(&self, lease_id: &str, sequence: u64, bytes: &[u8]) -> Result<()> {
        let mut lease = self.validate_lease(lease_id, sequence)?;
        let mut writer = self.writer.lock().expect("terminal writer poisoned");
        writer.write_all(bytes)?;
        writer.flush()?;
        lease
            .as_mut()
            .expect("validated lease disappeared")
            .last_sequence = sequence;
        Ok(())
    }

    pub fn resize(&self, lease_id: &str, sequence: u64, rows: u32, cols: u32) -> Result<()> {
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            bail!("terminal dimensions must be between 1 and 1000")
        }
        let mut lease = self.validate_lease(lease_id, sequence)?;
        self.master
            .lock()
            .expect("terminal master poisoned")
            .resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        {
            let mut info = self.info.write().expect("terminal info poisoned");
            info.rows = rows;
            info.cols = cols;
        }
        self.database.update_size(&self.info().id, rows, cols)?;
        lease
            .as_mut()
            .expect("validated lease disappeared")
            .last_sequence = sequence;
        Ok(())
    }

    fn validate_lease(
        &self,
        lease_id: &str,
        sequence: u64,
    ) -> Result<std::sync::MutexGuard<'_, Option<Lease>>> {
        let lease = self.lease.lock().expect("terminal lease poisoned");
        let current = lease
            .as_ref()
            .ok_or_else(|| anyhow!("terminal has no active input lease"))?;
        if current.id != lease_id {
            bail!("stale or invalid input lease")
        }
        if sequence <= current.last_sequence {
            bail!("duplicate or out-of-order terminal command")
        }
        Ok(lease)
    }

    pub fn kill(&self) -> Result<()> {
        self.child
            .lock()
            .expect("terminal child poisoned")
            .kill()
            .context("failed to kill terminal process")
    }
}

#[derive(Clone)]
pub struct TerminalManager {
    terminals: Arc<RwLock<HashMap<String, Arc<Terminal>>>>,
    database: Database,
    session_root: PathBuf,
}

impl TerminalManager {
    pub fn new(database: Database, session_root: PathBuf) -> Result<Self> {
        let session_root = session_root
            .canonicalize()
            .with_context(|| format!("invalid session root {}", session_root.display()))?;
        Ok(Self {
            terminals: Arc::new(RwLock::new(HashMap::new())),
            database,
            session_root,
        })
    }

    pub fn list(&self) -> Result<Vec<TerminalInfo>> {
        self.database.list_terminals()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        self.terminals
            .read()
            .expect("terminal registry poisoned")
            .get(id)
            .cloned()
    }

    pub fn spawn(&self, request: SpawnRequest) -> Result<Arc<Terminal>> {
        let rows = if request.rows == 0 { 24 } else { request.rows };
        let cols = if request.cols == 0 { 80 } else { request.cols };
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            bail!("terminal dimensions must be between 1 and 1000")
        }

        let cwd = self.resolve_cwd(&request.cwd)?;
        let argv = if request.argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
        } else {
            request.argv
        };
        if argv[0].is_empty() {
            bail!("argv[0] cannot be empty")
        }

        let id = Uuid::new_v4().to_string();
        let name = if request.name.is_empty() {
            format!("terminal-{}", &id[..8])
        } else {
            request.name
        };
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to allocate PTY")?;

        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("ASTRA_TERMINAL_ID", &id);
        let mut child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to spawn {}", argv[0]))?;
        drop(pty.slave);
        let reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let (events, _) = broadcast::channel(1024);
        let info = TerminalInfo {
            id: id.clone(),
            name,
            argv,
            cwd: cwd.to_string_lossy().into_owned(),
            status: "running".into(),
            exit_code: None,
            rows,
            cols,
        };
        if let Err(error) = self.database.insert_terminal(&info) {
            let _ = child.kill();
            return Err(error);
        }

        let terminal = Arc::new(Terminal {
            info: RwLock::new(info),
            master: Mutex::new(pty.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            history: Mutex::new(VecDeque::with_capacity(HISTORY_LIMIT)),
            events,
            lease: Mutex::new(None),
            database: self.database.clone(),
        });
        self.terminals
            .write()
            .expect("terminal registry poisoned")
            .insert(id, terminal.clone());
        start_reader(terminal.clone(), reader);
        start_child_monitor(terminal.clone());
        Ok(terminal)
    }

    fn resolve_cwd(&self, requested: &str) -> Result<PathBuf> {
        let candidate = if requested.is_empty() {
            self.session_root.clone()
        } else {
            let path = Path::new(requested);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.session_root.join(path)
            }
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("invalid cwd {}", candidate.display()))?;
        if !canonical.starts_with(&self.session_root) {
            bail!(
                "cwd {} is outside configured session root {}",
                canonical.display(),
                self.session_root.display()
            )
        }
        Ok(canonical)
    }
}

fn start_reader(terminal: Arc<Terminal>, mut reader: Box<dyn Read + Send>) {
    std::thread::Builder::new()
        .name(format!("astra-pty-{}", &terminal.info().id[..8]))
        .spawn(move || {
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        let chunk = buffer[..length].to_vec();
                        let mut history =
                            terminal.history.lock().expect("terminal history poisoned");
                        let overflow = history
                            .len()
                            .saturating_add(chunk.len())
                            .saturating_sub(HISTORY_LIMIT);
                        let to_trim = overflow.min(history.len());
                        history.drain(..to_trim);
                        history.extend(&chunk);
                        let _ = terminal.events.send(PtyEvent::Output(chunk));
                    }
                    Err(error) => {
                        let _ = terminal.events.send(PtyEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
        })
        .expect("failed to start PTY reader thread");
}

fn start_child_monitor(terminal: Arc<Terminal>) {
    tokio::spawn(async move {
        loop {
            let result = terminal
                .child
                .lock()
                .expect("terminal child poisoned")
                .try_wait();
            match result {
                Ok(Some(status)) => {
                    let code = status.exit_code() as i32;
                    {
                        let mut info = terminal.info.write().expect("terminal info poisoned");
                        info.status = "exited".into();
                        info.exit_code = Some(code);
                    }
                    let _ =
                        terminal
                            .database
                            .update_status(&terminal.info().id, "exited", Some(code));
                    let _ = terminal.events.send(PtyEvent::Exited(code));
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(error) => {
                    let message = format!("failed to wait for terminal: {error}");
                    let _ = terminal.events.send(PtyEvent::Error(message));
                    break;
                }
            }
        }
    });
}
