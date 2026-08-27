use std::{
    collections::{BTreeMap, HashMap},
    ffi::CString,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::fs;

use anyhow::{Context, Result, anyhow, bail};
use astra_wezterm_term::{Clipboard, ClipboardSelection as EngineClipboardSelection};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use crate::protocol::{
    EnvironmentVariable, LOCALE_ENVIRONMENT_VARIABLES, SpawnRequest, TerminalInfo,
    TerminalLifecycle, TerminalSnapshot,
};
use crate::{
    terminal_engine::TerminalEngine,
    terminal_state_v2::{HistoryPage, HistoryPageRequest, State},
};

// Match tmux's default history-limit: enough context for normal use without
// letting many wide panes retain unbounded cell grids.
const SCREEN_SCROLLBACK_ROWS: usize = 2_000;
const EXITED_TERMINAL_RETENTION: Duration = Duration::from_secs(60);
const MAX_TERM_LENGTH: usize = 64;
const MAX_LOCALE_VALUE_LENGTH: usize = 256;
const MAX_PROGRAM_TITLE_LENGTH: usize = 512;
const SAFE_BASE_ENVIRONMENT: &[&str] = &["HOME", "USER", "LOGNAME", "SHELL", "PATH"];
const MAX_CLIPBOARD_WRITE_BYTES: usize = 256 * 1024;
const UTF8_LOCALE_FALLBACKS: &[&str] = &["C.UTF-8", "C.utf8", "UTF-8", "en_US.UTF-8"];

struct PreparedTerminalEnvironment {
    term: String,
    locale: Vec<EnvironmentVariable>,
    used_locale_fallback: bool,
}

#[derive(Clone, Debug)]
pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(i32),
    Error(String),
    Interactive(bool),
    ClipboardWrite {
        selection: ClipboardSelection,
        contents: Option<Vec<u8>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

#[derive(Clone, Debug)]
pub struct LeaseEvent {
    pub revoked_lease_id: String,
}

#[derive(Debug)]
struct Lease {
    id: String,
    resume_token: String,
    last_sequence: u64,
}

pub struct LeaseGrant {
    pub lease_id: String,
    pub resume_token: String,
}

pub struct Terminal {
    info: RwLock<TerminalInfo>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send>>,
    shell_pid: Option<i32>,
    engine: Mutex<TerminalEngine>,
    events: broadcast::Sender<PtyEvent>,
    lease_events: broadcast::Sender<LeaseEvent>,
    lease: Mutex<Option<Lease>>,
}

impl Terminal {
    pub fn info(&self) -> TerminalInfo {
        // Lock in the same engine -> info order used by resize so title reads
        // cannot deadlock with a concurrent geometry update.
        let program_title = self
            .engine
            .lock()
            .expect("terminal engine poisoned")
            .program_title()
            .map(str::to_owned);
        let mut info = self.info.read().expect("terminal info poisoned").clone();
        if let Some(program_title) = program_title
            .as_deref()
            .and_then(|title| clean_program_title(title.as_bytes()))
        {
            info.name = program_title;
        }
        info
    }

    pub fn rename(&self, name: String) {
        let cleaned = name.trim();
        self.info
            .write()
            .expect("terminal info poisoned")
            .custom_name = (!cleaned.is_empty()).then(|| cleaned.to_owned());
    }

    pub fn snapshot_and_subscribe(
        &self,
    ) -> Result<(TerminalSnapshot, broadcast::Receiver<PtyEvent>)> {
        // The PTY reader publishes while holding this same lock. Subscribing
        // before rendering therefore gives an atomic boundary: output is
        // represented either by the snapshot or by the receiver, never lost
        // or duplicated.
        let mut engine = self.engine.lock().expect("terminal engine poisoned");
        let receiver = self.events.subscribe();
        Ok((engine.legacy_snapshot()?, receiver))
    }

    pub fn semantic_state_and_subscribe(&self) -> Result<(State, broadcast::Receiver<PtyEvent>)> {
        let mut engine = self.engine.lock().expect("terminal engine poisoned");
        let receiver = self.events.subscribe();
        Ok((engine.semantic_state()?, receiver))
    }

    pub fn semantic_state(&self) -> Result<State> {
        self.engine
            .lock()
            .expect("terminal engine poisoned")
            .semantic_state()
    }

    pub fn history_page(
        &self,
        request_id: u64,
        request: &HistoryPageRequest,
    ) -> Result<HistoryPage> {
        self.engine
            .lock()
            .expect("terminal engine poisoned")
            .history_page(request_id, request)
    }

    pub fn subscribe_to_leases(&self) -> broadcast::Receiver<LeaseEvent> {
        self.lease_events.subscribe()
    }

    pub fn acquire_lease(
        &self,
        read_only: bool,
        takeover: bool,
        resume_token: &str,
    ) -> Result<LeaseGrant> {
        if read_only {
            return Ok(LeaseGrant {
                lease_id: String::new(),
                resume_token: String::new(),
            });
        }
        let mut lease = self.lease.lock().expect("terminal lease poisoned");
        let revoked_lease_id = lease.as_ref().map(|lease| lease.id.clone());
        if !resume_token.is_empty()
            && lease
                .as_ref()
                .is_some_and(|current| current.resume_token == resume_token)
        {
            let lease_id = Uuid::new_v4().to_string();
            *lease = Some(Lease {
                id: lease_id.clone(),
                resume_token: resume_token.to_owned(),
                last_sequence: 0,
            });
            if let Some(revoked_lease_id) = revoked_lease_id {
                let _ = self.lease_events.send(LeaseEvent { revoked_lease_id });
            }
            return Ok(LeaseGrant {
                lease_id,
                resume_token: resume_token.to_owned(),
            });
        }
        if lease.is_some() && !takeover {
            bail!("terminal already has an input lease owner; use --read-only or --takeover")
        }
        let lease_id = Uuid::new_v4().to_string();
        let resume_token = if lease.is_none() && !resume_token.is_empty() {
            resume_token.to_owned()
        } else {
            Uuid::new_v4().to_string()
        };
        *lease = Some(Lease {
            id: lease_id.clone(),
            resume_token: resume_token.clone(),
            last_sequence: 0,
        });
        if let Some(revoked_lease_id) = revoked_lease_id {
            let _ = self.lease_events.send(LeaseEvent { revoked_lease_id });
        }
        Ok(LeaseGrant {
            lease_id,
            resume_token,
        })
    }

    pub fn owns_lease(&self, lease_id: &str) -> bool {
        !lease_id.is_empty()
            && self
                .lease
                .lock()
                .expect("terminal lease poisoned")
                .as_ref()
                .is_some_and(|lease| lease.id == lease_id)
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

    pub fn resize(
        &self,
        lease_id: &str,
        sequence: u64,
        rows: u32,
        cols: u32,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<()> {
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            bail!("terminal dimensions must be between 1 and 1000")
        }
        if (pixel_width == 0) != (pixel_height == 0)
            || pixel_width > u16::MAX as u32
            || pixel_height > u16::MAX as u32
        {
            bail!("terminal pixel dimensions must both be zero or between 1 and 65535")
        }
        let mut lease = self.validate_lease(lease_id, sequence)?;
        // Resize the server grid before notifying the PTY. The foreground
        // process may redraw immediately after TIOCSWINSZ, and those bytes must
        // be parsed using the new geometry.
        let mut engine = self.engine.lock().expect("terminal engine poisoned");
        engine.resize(rows, cols, pixel_width, pixel_height)?;
        self.master
            .lock()
            .expect("terminal master poisoned")
            .resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: pixel_width as u16,
                pixel_height: pixel_height as u16,
            })?;
        {
            let mut info = self.info.write().expect("terminal info poisoned");
            info.rows = rows;
            info.cols = cols;
        }
        lease
            .as_mut()
            .expect("validated lease disappeared")
            .last_sequence = sequence;
        drop(engine);
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
    next_display_id: Arc<Mutex<u64>>,
    session_root: PathBuf,
}

impl TerminalManager {
    pub fn new(session_root: PathBuf) -> Result<Self> {
        let session_root = session_root
            .canonicalize()
            .with_context(|| format!("invalid session root {}", session_root.display()))?;
        Ok(Self {
            terminals: Arc::new(RwLock::new(HashMap::new())),
            next_display_id: Arc::new(Mutex::new(1)),
            session_root,
        })
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        let mut terminals: Vec<_> = self
            .terminals
            .read()
            .expect("terminal registry poisoned")
            .values()
            .map(|terminal| terminal.info())
            .filter(|terminal| terminal.status == "running")
            .collect();
        terminals.sort_by_key(|terminal| terminal.display_id);
        terminals
    }

    pub fn session_root(&self) -> &Path {
        &self.session_root
    }

    pub fn has_active_terminals(&self) -> bool {
        self.terminals
            .read()
            .expect("terminal registry poisoned")
            .values()
            .any(|terminal| terminal.info().status == "running")
    }

    pub fn get(&self, selector: &str) -> Option<Arc<Terminal>> {
        let terminals = self.terminals.read().expect("terminal registry poisoned");
        if let Some(terminal) = terminals.get(selector) {
            return Some(terminal.clone());
        }
        let display_id = parse_display_id(selector)?;
        terminals
            .values()
            .find(|terminal| terminal.info().display_id == display_id)
            .cloned()
    }

    pub fn spawn(&self, request: SpawnRequest) -> Result<Arc<Terminal>> {
        let default_shell = request.argv.is_empty();
        let rows = if request.rows == 0 { 24 } else { request.rows };
        let cols = if request.cols == 0 { 80 } else { request.cols };
        if !(1..=1000).contains(&rows) || !(1..=1000).contains(&cols) {
            bail!("terminal dimensions must be between 1 and 1000")
        }

        let terminal_environment =
            prepare_terminal_environment(&request.term, &request.environment)?;
        let cwd = self.resolve_cwd(&request.cwd)?;
        let argv = if request.argv.is_empty() {
            vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
        } else {
            request.argv
        };
        if argv[0].is_empty() {
            bail!("argv[0] cannot be empty")
        }

        let display_id = self.allocate_display_id()?;
        let id = Uuid::new_v4().to_string();
        let name = if request.name.is_empty() {
            format!("terminal-{display_id}")
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
        enable_iutf8(pty.master.as_ref())?;

        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        command.cwd(&cwd);
        command.env_clear();
        for &name in SAFE_BASE_ENVIRONMENT {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if command.get_env("PATH").is_none() {
            command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        }
        command.env("TERM", &terminal_environment.term);
        for variable in &terminal_environment.locale {
            command.env(&variable.name, &variable.value);
        }
        command.env("ASTRA_TERMINAL_ID", &id);
        if terminal_environment.used_locale_fallback {
            warn!(
                terminal_id = %id,
                "client locale is missing, unavailable, or not UTF-8; using a server UTF-8 fallback"
            );
        }
        let child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to spawn {}", argv[0]))?;
        let shell_pid = child.process_id().map(|pid| pid as i32);
        drop(pty.slave);
        let reader = pty.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pty.master.take_writer()?));
        let (events, _) = broadcast::channel(1024);
        let (lease_events, _) = broadcast::channel(16);
        let info = TerminalInfo {
            id: id.clone(),
            name,
            argv,
            cwd: cwd.to_string_lossy().into_owned(),
            status: "running".into(),
            exit_code: None,
            rows,
            cols,
            display_id,
            custom_name: None,
            interactive: Some(true),
            workspace_id: request.workspace_id,
            lifecycle: TerminalLifecycle::Running as i32,
        };
        let terminal = Arc::new(Terminal {
            info: RwLock::new(info),
            master: Mutex::new(pty.master),
            writer: writer.clone(),
            child: Mutex::new(child),
            shell_pid,
            engine: Mutex::new(initial_terminal_engine(
                rows,
                cols,
                default_shell,
                writer,
                events.clone(),
            )?),
            events,
            lease_events,
            lease: Mutex::new(None),
        });
        self.terminals
            .write()
            .expect("terminal registry poisoned")
            .insert(id, terminal.clone());
        start_reader(terminal.clone(), reader);
        start_child_monitor(terminal.clone(), self.terminals.clone());
        start_foreground_monitor(terminal.clone());
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

    fn allocate_display_id(&self) -> Result<u64> {
        let mut next = self
            .next_display_id
            .lock()
            .expect("terminal display ID counter poisoned");
        let display_id = *next;
        *next = display_id
            .checked_add(1)
            .context("terminal display ID space exhausted")?;
        Ok(display_id)
    }
}

fn start_foreground_monitor(terminal: Arc<Terminal>) {
    tokio::spawn(async move {
        let Some(shell_pid) = terminal.shell_pid else {
            return;
        };
        loop {
            if terminal.info().status != "running" {
                return;
            }
            let interactive = terminal
                .master
                .lock()
                .expect("terminal master poisoned")
                .process_group_leader()
                .is_some_and(|foreground_pid| foreground_pid == shell_pid);
            let changed = {
                let mut info = terminal.info.write().expect("terminal info poisoned");
                let changed = info.interactive != Some(interactive);
                info.interactive = Some(interactive);
                changed
            };
            if changed {
                let _ = terminal.events.send(PtyEvent::Interactive(interactive));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

fn parse_display_id(selector: &str) -> Option<u64> {
    let display_id = selector.parse().ok()?;
    (display_id != 0).then_some(display_id)
}

struct HostReplyWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for HostReplyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("terminal writer poisoned")
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().expect("terminal writer poisoned").flush()
    }
}

struct HostClipboard {
    events: broadcast::Sender<PtyEvent>,
}

impl Clipboard for HostClipboard {
    fn set_contents(
        &self,
        selection: EngineClipboardSelection,
        data: Option<String>,
    ) -> Result<()> {
        if data
            .as_ref()
            .is_some_and(|contents| contents.len() > MAX_CLIPBOARD_WRITE_BYTES)
        {
            bail!("OSC 52 clipboard write exceeds the 256 KiB host-effect limit")
        }
        let selection = match selection {
            EngineClipboardSelection::Clipboard => ClipboardSelection::Clipboard,
            EngineClipboardSelection::PrimarySelection => ClipboardSelection::Primary,
        };
        let _ = self.events.send(PtyEvent::ClipboardWrite {
            selection,
            contents: data.map(String::into_bytes),
        });
        Ok(())
    }
}

fn initial_terminal_engine(
    rows: u32,
    cols: u32,
    include_welcome: bool,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    events: broadcast::Sender<PtyEvent>,
) -> Result<TerminalEngine> {
    let mut engine = TerminalEngine::new(
        rows,
        cols,
        SCREEN_SCROLLBACK_ROWS,
        Box::new(HostReplyWriter(writer)),
    )?;
    let clipboard: Arc<dyn Clipboard> = Arc::new(HostClipboard { events });
    engine.set_clipboard(&clipboard);
    if include_welcome && let Some(banner) = system_welcome_banner() {
        engine.advance(&banner);
    }
    Ok(engine)
}

fn clean_program_title(title: &[u8]) -> Option<String> {
    let title = String::from_utf8_lossy(title);
    let cleaned: String = title
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_PROGRAM_TITLE_LENGTH)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(target_os = "linux")]
fn system_welcome_banner() -> Option<Vec<u8>> {
    let os_release = fs::read_to_string("/etc/os-release").ok()?;
    let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    build_welcome_banner(&os_release, &kernel_release, std::env::consts::ARCH)
}

#[cfg(not(target_os = "linux"))]
fn system_welcome_banner() -> Option<Vec<u8>> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn build_welcome_banner(
    os_release: &str,
    kernel_release: &str,
    architecture: &str,
) -> Option<Vec<u8>> {
    let pretty_name = os_release_value(os_release, "PRETTY_NAME")?;
    let kernel_release = safe_banner_field(kernel_release)?;
    let architecture = safe_banner_field(architecture)?;
    Some(
        format!("Welcome to {pretty_name} (GNU/Linux {kernel_release} {architecture})\r\n\r\n")
            .into_bytes(),
    )
}

#[cfg(any(target_os = "linux", test))]
fn os_release_value(contents: &str, key: &str) -> Option<String> {
    let encoded = contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then_some(value.trim())
    })?;
    if encoded.starts_with('"') != encoded.ends_with('"')
        || encoded.starts_with('\'') != encoded.ends_with('\'')
    {
        return None;
    }
    let decoded = if encoded.len() >= 2 && encoded.starts_with('"') && encoded.ends_with('"') {
        let mut value = String::with_capacity(encoded.len() - 2);
        let mut escaped = false;
        for character in encoded[1..encoded.len() - 1].chars() {
            if escaped {
                value.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                value.push(character);
            }
        }
        if escaped {
            return None;
        }
        value
    } else if encoded.len() >= 2 && encoded.starts_with('\'') && encoded.ends_with('\'') {
        encoded[1..encoded.len() - 1].to_owned()
    } else {
        encoded.to_owned()
    };
    safe_banner_field(&decoded)
}

#[cfg(any(target_os = "linux", test))]
fn safe_banner_field(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn prepare_terminal_environment(
    requested_term: &str,
    requested_locale: &[EnvironmentVariable],
) -> Result<PreparedTerminalEnvironment> {
    let term = if requested_term.is_empty() {
        "xterm-256color".to_owned()
    } else {
        requested_term.to_owned()
    };
    if term.len() > MAX_TERM_LENGTH
        || !term
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
    {
        bail!("invalid TERM value")
    }
    if requested_locale.len() > LOCALE_ENVIRONMENT_VARIABLES.len() {
        bail!("too many locale environment variables")
    }

    let mut locale = BTreeMap::new();
    for variable in requested_locale {
        if !LOCALE_ENVIRONMENT_VARIABLES.contains(&variable.name.as_str()) {
            bail!("environment variable {} is not allowed", variable.name)
        }
        if variable.value.is_empty()
            || variable.value.len() > MAX_LOCALE_VALUE_LENGTH
            || !variable
                .value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.@:".contains(&byte))
        {
            bail!("invalid value for locale variable {}", variable.name)
        }
        if locale
            .insert(variable.name.clone(), variable.value.clone())
            .is_some()
        {
            bail!("duplicate locale variable {}", variable.name)
        }
    }

    let effective_ctype = locale
        .get("LC_ALL")
        .or_else(|| locale.get("LC_CTYPE"))
        .or_else(|| locale.get("LANG"));
    let locale_is_usable = effective_ctype.is_some_and(|value| locale_name_is_utf8(value))
        && locale
            .iter()
            .filter(|(name, _)| name.as_str() != "LANGUAGE")
            .all(|(_, value)| locale_is_available(value));

    let (locale, used_locale_fallback) = if locale_is_usable {
        (
            locale
                .into_iter()
                .map(|(name, value)| EnvironmentVariable { name, value })
                .collect(),
            false,
        )
    } else {
        let fallback = UTF8_LOCALE_FALLBACKS
            .iter()
            .copied()
            .find(|value| locale_is_available(value))
            .context("server has no supported UTF-8 locale")?;
        (
            vec![
                EnvironmentVariable {
                    name: "LANG".into(),
                    value: fallback.into(),
                },
                EnvironmentVariable {
                    name: "LC_CTYPE".into(),
                    value: fallback.into(),
                },
            ],
            true,
        )
    };

    Ok(PreparedTerminalEnvironment {
        term,
        locale,
        used_locale_fallback,
    })
}

fn locale_name_is_utf8(value: &str) -> bool {
    value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .windows(4)
        .any(|window| window == b"utf8")
}

#[cfg(unix)]
fn locale_is_available(value: &str) -> bool {
    let Ok(value) = CString::new(value) else {
        return false;
    };
    // SAFETY: newlocale reads the NUL-terminated string and returns an
    // independent locale object. It does not mutate the process-global locale.
    let locale = unsafe {
        nix::libc::newlocale(nix::libc::LC_ALL_MASK, value.as_ptr(), std::ptr::null_mut())
    };
    if locale.is_null() {
        return false;
    }
    // SAFETY: locale is a valid object returned by newlocale and is freed once.
    unsafe { nix::libc::freelocale(locale) };
    true
}

#[cfg(not(unix))]
fn locale_is_available(value: &str) -> bool {
    locale_name_is_utf8(value)
}

#[cfg(unix)]
fn enable_iutf8(master: &dyn MasterPty) -> Result<()> {
    let descriptor = master
        .as_raw_fd()
        .context("PTY backend does not expose a terminal descriptor")?;
    // SAFETY: descriptor is owned by master and remains valid for both calls.
    // tcgetattr initializes the termios structure before it is read.
    unsafe {
        let mut attributes = std::mem::MaybeUninit::<nix::libc::termios>::uninit();
        if nix::libc::tcgetattr(descriptor, attributes.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to read PTY attributes");
        }
        let mut attributes = attributes.assume_init();
        attributes.c_iflag |= nix::libc::IUTF8;
        if nix::libc::tcsetattr(descriptor, nix::libc::TCSANOW, &attributes) != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to enable UTF-8 PTY input handling");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn enable_iutf8(_master: &dyn MasterPty) -> Result<()> {
    Ok(())
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
                        let mut engine = terminal.engine.lock().expect("terminal engine poisoned");
                        engine.advance(&chunk);
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

fn start_child_monitor(
    terminal: Arc<Terminal>,
    terminals: Arc<RwLock<HashMap<String, Arc<Terminal>>>>,
) {
    tokio::spawn(async move {
        let terminal_id = terminal.info().id;
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
                    let _ = terminal.events.send(PtyEvent::Exited(code));
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(error) => {
                    terminal
                        .info
                        .write()
                        .expect("terminal info poisoned")
                        .status = "lost".into();
                    let message = format!("failed to wait for terminal: {error}");
                    let _ = terminal.events.send(PtyEvent::Error(message));
                    break;
                }
            }
        }
        tokio::time::sleep(EXITED_TERMINAL_RETENTION).await;
        terminals
            .write()
            .expect("terminal registry poisoned")
            .remove(&terminal_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_text(screen: &crate::terminal_state_v2::Screen) -> String {
        screen
            .included_rows
            .iter()
            .flat_map(|row| &row.cells)
            .map(|cell| cell.grapheme.as_str())
            .collect()
    }

    fn variable(name: &str, value: &str) -> EnvironmentVariable {
        EnvironmentVariable {
            name: name.into(),
            value: value.into(),
        }
    }

    fn available_utf8_locale() -> &'static str {
        UTF8_LOCALE_FALLBACKS
            .iter()
            .copied()
            .find(|value| locale_is_available(value))
            .expect("tests require a UTF-8 locale")
    }

    #[test]
    fn allocates_nonzero_monotonic_display_ids_without_wrapping() {
        let directory = tempfile::tempdir().unwrap();
        let manager = TerminalManager::new(directory.path().to_path_buf()).unwrap();
        assert_eq!(manager.allocate_display_id().unwrap(), 1);
        assert_eq!(manager.allocate_display_id().unwrap(), 2);

        *manager.next_display_id.lock().unwrap() = u64::MAX;
        assert!(manager.allocate_display_id().is_err());
        assert_eq!(*manager.next_display_id.lock().unwrap(), u64::MAX);
    }

    #[test]
    fn authoritative_engine_exports_full_tui_state_and_legacy_compatibility_view() {
        let mut engine =
            TerminalEngine::new(8, 30, SCREEN_SCROLLBACK_ROWS, Box::new(std::io::sink())).unwrap();
        engine.advance(b"\x1b[2J\x1b[Hshell history\r\n$ codex");
        engine.advance(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[1;36mCodex\x1b[0m");
        engine.resize(10, 36, 0, 0).unwrap();
        engine.advance(b"\x1b[2J\x1b[H\x1b[1;36mCodex\x1b[0m\r\n");
        // Split a multi-byte wide character exactly as separate PTY reads can.
        let wide = "状态：运行中 ✅".as_bytes();
        engine.advance(&wide[..4]);
        engine.advance(&wide[4..]);
        engine.advance(
            b"\x1b[5;4H\x1b[38;5;214mworking\x1b[3m...\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h",
        );

        let state = engine.semantic_state().unwrap();
        assert_eq!(state.rows, 10);
        assert_eq!(state.cols, 36);
        assert_eq!(
            state.active_screen,
            crate::terminal_state_v2::ScreenKind::Alternate as i32
        );
        let primary_text = semantic_text(state.primary.as_ref().unwrap());
        let alternate_text = semantic_text(state.alternate.as_ref().unwrap());
        assert!(primary_text.contains("shellhistory"), "{primary_text:?}");
        assert!(
            alternate_text.contains("状态：运行中"),
            "{alternate_text:?}"
        );
        assert!(
            state
                .alternate
                .as_ref()
                .unwrap()
                .included_rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.grapheme == "状" && cell.width == 2)
        );
        let modes = state.modes.as_ref().unwrap();
        assert!(modes.application_cursor_keys);
        assert!(modes.bracketed_paste);
        assert_eq!(
            modes.mouse_encoding,
            crate::terminal_state_v2::MouseEncoding::Sgr as i32
        );

        let snapshot = engine.legacy_snapshot().unwrap();
        assert!(snapshot.alternate_screen);
        assert!(!snapshot.normal_contents.is_empty());
        assert!(snapshot.contents.starts_with(b"\x1b[2J\x1b[H"));
    }

    #[test]
    fn semantic_reset_discards_stale_contents_before_snapshot_serialization() {
        let mut engine =
            TerminalEngine::new(4, 16, SCREEN_SCROLLBACK_ROWS, Box::new(std::io::sink())).unwrap();
        engine.advance(b"stale client data\r\nthat must vanish");
        engine.advance(b"\x1bc\x1b[2;3Hauthoritative");

        let state = engine.semantic_state().unwrap();
        let text = semantic_text(state.primary.as_ref().unwrap());
        assert!(text.contains("authoritative"));
        assert!(!text.contains("stale"));
        let snapshot = engine.legacy_snapshot().unwrap();
        assert!(snapshot.contents.starts_with(b"\x1b[2J\x1b[H"));
    }

    #[test]
    fn captures_and_sanitizes_program_reported_terminal_titles() {
        let mut engine = TerminalEngine::new(24, 80, 0, Box::new(std::io::sink())).unwrap();
        assert_eq!(engine.program_title(), None);
        engine.advance(b"\x1b]0;xy@rome:~/Projects/astra\x07");
        assert_eq!(
            engine
                .program_title()
                .and_then(|title| clean_program_title(title.as_bytes()))
                .as_deref(),
            Some("xy@rome:~/Projects/astra")
        );

        assert_eq!(
            clean_program_title(b"  Codex\x01 Session  ").as_deref(),
            Some("Codex Session")
        );
        engine.advance(b"\x1b]2;   \x07");
        assert_eq!(clean_program_title(engine.title().as_bytes()), None);
    }

    #[test]
    fn osc_52_is_a_bounded_write_only_host_effect() {
        let (events, _) = broadcast::channel(8);
        let mut receiver = events.subscribe();
        let clipboard: Arc<dyn Clipboard> = Arc::new(HostClipboard {
            events: events.clone(),
        });
        let mut engine = TerminalEngine::new(24, 80, 0, Box::new(std::io::sink())).unwrap();
        engine.set_clipboard(&clipboard);

        engine.advance(b"\x1b]52;c;aGVsbG8=\x07");
        assert!(matches!(
            receiver.try_recv().unwrap(),
            PtyEvent::ClipboardWrite {
                selection: ClipboardSelection::Clipboard,
                contents: Some(contents),
            } if contents == b"hello"
        ));

        engine.advance(b"\x1b]52;c;?\x07");
        assert!(receiver.try_recv().is_err());
        assert!(
            clipboard
                .set_contents(
                    EngineClipboardSelection::Clipboard,
                    Some("x".repeat(MAX_CLIPBOARD_WRITE_BYTES + 1)),
                )
                .is_err()
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn resumes_matching_writer_by_rotating_lease_without_losing_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let manager = TerminalManager::new(directory.path().to_path_buf()).unwrap();
        let terminal = manager
            .spawn(SpawnRequest {
                name: "lease-test".into(),
                argv: vec!["/bin/cat".into()],
                cwd: String::new(),
                rows: 24,
                cols: 80,
                term: "xterm-256color".into(),
                environment: Vec::new(),
                workspace_id: String::new(),
            })
            .unwrap();
        assert!(manager.has_active_terminals());

        let first = terminal.acquire_lease(false, false, "").unwrap();
        let mut lease_events = terminal.subscribe_to_leases();
        let resumed = terminal
            .acquire_lease(false, false, &first.resume_token)
            .unwrap();
        let revoked = lease_events.recv().await.unwrap();
        assert_eq!(revoked.revoked_lease_id, first.lease_id);
        assert_ne!(first.lease_id, resumed.lease_id);
        assert_eq!(first.resume_token, resumed.resume_token);
        assert!(!terminal.owns_lease(&first.lease_id));
        assert!(terminal.owns_lease(&resumed.lease_id));
        assert!(
            terminal
                .write_input(&first.lease_id, 1, b"stale\n")
                .is_err()
        );

        terminal.release_lease(&first.lease_id);
        assert!(terminal.acquire_lease(false, false, "").is_err());
        terminal
            .write_input(&resumed.lease_id, 1, b"resume\n")
            .unwrap();

        terminal.release_lease(&resumed.lease_id);
        terminal.kill().unwrap();
    }

    #[test]
    fn accepts_only_nonzero_decimal_display_id_selectors() {
        assert_eq!(parse_display_id("1"), Some(1));
        assert_eq!(parse_display_id(&u64::MAX.to_string()), Some(u64::MAX));
        assert_eq!(parse_display_id("0"), None);
        assert_eq!(parse_display_id("-1"), None);
        assert_eq!(parse_display_id("01x"), None);
        assert_eq!(
            parse_display_id("f3b5592c-1146-4f09-812e-c2621863c747"),
            None
        );
    }

    #[test]
    fn preserves_an_available_client_utf8_locale_and_term() {
        let locale = available_utf8_locale();
        let prepared = prepare_terminal_environment(
            "tmux-256color",
            &[
                variable("LANG", locale),
                variable("LC_CTYPE", locale),
                variable("LC_TIME", "C"),
            ],
        )
        .unwrap();
        assert_eq!(prepared.term, "tmux-256color");
        assert!(!prepared.used_locale_fallback);
        assert!(
            prepared
                .locale
                .iter()
                .any(|entry| entry.name == "LC_CTYPE" && entry.value == locale)
        );
    }

    #[test]
    fn falls_back_when_client_locale_is_missing_non_utf8_or_unavailable() {
        for requested in [
            Vec::new(),
            vec![variable("LANG", "C")],
            vec![variable("LANG", "astra_MISSING.UTF-8")],
        ] {
            let prepared = prepare_terminal_environment("", &requested).unwrap();
            assert_eq!(prepared.term, "xterm-256color");
            assert!(prepared.used_locale_fallback);
            assert!(
                prepared
                    .locale
                    .iter()
                    .any(|entry| { entry.name == "LC_CTYPE" && locale_name_is_utf8(&entry.value) })
            );
        }
    }

    #[test]
    fn rejects_unapproved_or_malformed_environment() {
        assert!(
            prepare_terminal_environment(
                "xterm-256color",
                &[variable("LD_PRELOAD", "/tmp/library.so")],
            )
            .is_err()
        );
        assert!(
            prepare_terminal_environment(
                "xterm-256color",
                &[variable("LANG", "C.UTF-8"), variable("LANG", "C.UTF-8")],
            )
            .is_err()
        );
        assert!(prepare_terminal_environment("bad term", &[]).is_err());
    }

    #[test]
    fn builds_linux_welcome_banner_from_system_metadata() {
        let banner = build_welcome_banner(
            "NAME=Ubuntu\nPRETTY_NAME=\"Ubuntu 24.04.4 LTS\"\n",
            "7.0.0-29-generic\n",
            "x86_64",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(banner).unwrap(),
            "Welcome to Ubuntu 24.04.4 LTS (GNU/Linux 7.0.0-29-generic x86_64)\r\n\r\n"
        );
    }

    #[test]
    fn rejects_multiline_or_oversized_banner_metadata() {
        assert!(build_welcome_banner("PRETTY_NAME=Bad\u{7}Name", "1.0", "x86_64").is_none());
        assert!(build_welcome_banner("PRETTY_NAME=Linux", "bad\nrelease\n", "x86_64").is_none());
        assert!(
            build_welcome_banner(&format!("PRETTY_NAME={}", "x".repeat(257)), "1.0", "x86_64")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn enables_utf8_input_handling_on_the_pty() {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        enable_iutf8(pty.master.as_ref()).unwrap();
        let descriptor = pty.master.as_raw_fd().unwrap();
        // SAFETY: descriptor remains owned by pty for the duration of the call.
        unsafe {
            let mut attributes = std::mem::MaybeUninit::<nix::libc::termios>::uninit();
            assert_eq!(nix::libc::tcgetattr(descriptor, attributes.as_mut_ptr()), 0);
            assert_ne!(attributes.assume_init().c_iflag & nix::libc::IUTF8, 0);
        }
    }
}
