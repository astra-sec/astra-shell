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
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

use crate::protocol::{
    EnvironmentVariable, LOCALE_ENVIRONMENT_VARIABLES, SpawnRequest, TerminalInfo, TerminalSnapshot,
};

// Match tmux's default history-limit: enough context for normal use without
// letting many wide panes retain unbounded cell grids.
const SCREEN_SCROLLBACK_ROWS: usize = 2_000;
const EXITED_TERMINAL_RETENTION: Duration = Duration::from_secs(60);
const MAX_TERM_LENGTH: usize = 64;
const MAX_LOCALE_VALUE_LENGTH: usize = 256;
const SAFE_BASE_ENVIRONMENT: &[&str] = &["HOME", "USER", "LOGNAME", "SHELL", "PATH"];
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
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send>>,
    shell_pid: Option<i32>,
    // Like tmux's pane grid, this parser is the daemon's authoritative terminal
    // state. Attachments receive a rendering of this grid instead of replaying
    // an arbitrary suffix of the PTY byte stream.
    screen: Mutex<vt100::Parser>,
    events: broadcast::Sender<PtyEvent>,
    lease: Mutex<Option<Lease>>,
}

impl Terminal {
    pub fn info(&self) -> TerminalInfo {
        self.info.read().expect("terminal info poisoned").clone()
    }

    pub fn rename(&self, name: String) {
        let cleaned = name.trim();
        self.info
            .write()
            .expect("terminal info poisoned")
            .custom_name = (!cleaned.is_empty()).then(|| cleaned.to_owned());
    }

    pub fn snapshot_and_subscribe(&self) -> (TerminalSnapshot, broadcast::Receiver<PtyEvent>) {
        // The PTY reader publishes while holding this same lock. Subscribing
        // before rendering therefore gives an atomic boundary: output is
        // represented either by the snapshot or by the receiver, never lost
        // or duplicated.
        let mut screen = self.screen.lock().expect("terminal screen poisoned");
        let receiver = self.events.subscribe();
        (render_snapshot(&mut screen), receiver)
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
        Ok(LeaseGrant {
            lease_id,
            resume_token,
        })
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
        // Resize the server grid before notifying the PTY. The foreground
        // process may redraw immediately after TIOCSWINSZ, and those bytes must
        // be parsed using the new geometry.
        let mut screen = self.screen.lock().expect("terminal screen poisoned");
        screen.screen_mut().set_size(rows as u16, cols as u16);
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
        lease
            .as_mut()
            .expect("validated lease disappeared")
            .last_sequence = sequence;
        drop(screen);
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
            display_id,
            custom_name: None,
            interactive: Some(true),
        };
        let terminal = Arc::new(Terminal {
            info: RwLock::new(info),
            master: Mutex::new(pty.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            shell_pid,
            screen: Mutex::new(initial_terminal_screen(rows, cols, default_shell)),
            events,
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

fn initial_terminal_screen(rows: u32, cols: u32, include_welcome: bool) -> vt100::Parser {
    let mut screen = vt100::Parser::new(rows as u16, cols as u16, SCREEN_SCROLLBACK_ROWS);
    if include_welcome && let Some(banner) = system_welcome_banner() {
        screen.process(&banner);
    }
    screen
}

fn render_snapshot(parser: &mut vt100::Parser) -> TerminalSnapshot {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let alternate_screen = screen.alternate_screen();
    let contents = screen.state_formatted();
    // vt100 intentionally exposes only the active grid. Leave through 1049 so
    // the normal grid's saved cursor and attributes are restored, render it,
    // then switch back through mode 47 (which does not clear the alternate
    // grid) and replay its already-captured state. This preserves both grids
    // and the exact state that a later 1049l must restore.
    let normal_contents = if alternate_screen {
        parser.process(b"\x1b[?1049l");
        let normal = parser.screen().state_formatted();
        parser.process(b"\x1b[?47h");
        parser.process(&contents);
        normal
    } else {
        Vec::new()
    };
    TerminalSnapshot {
        rows: u32::from(rows),
        cols: u32::from(cols),
        contents,
        alternate_screen,
        normal_contents,
    }
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
                        let mut screen = terminal.screen.lock().expect("terminal screen poisoned");
                        screen.process(&chunk);
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

    fn restore_snapshot(snapshot: &TerminalSnapshot) -> vt100::Parser {
        let mut restored = vt100::Parser::new(
            snapshot.rows as u16,
            snapshot.cols as u16,
            SCREEN_SCROLLBACK_ROWS,
        );
        restored.process(b"\x1bc");
        if snapshot.alternate_screen {
            restored.process(&snapshot.normal_contents);
            restored.process(b"\x1b[?1049h");
        }
        restored.process(&snapshot.contents);
        restored
    }

    fn assert_active_screens_equal(left: &vt100::Parser, right: &vt100::Parser) {
        let left = left.screen();
        let right = right.screen();
        assert_eq!(left.size(), right.size());
        assert_eq!(left.contents(), right.contents());
        assert_eq!(left.cursor_position(), right.cursor_position());
        assert_eq!(left.alternate_screen(), right.alternate_screen());
        assert_eq!(left.application_keypad(), right.application_keypad());
        assert_eq!(left.application_cursor(), right.application_cursor());
        assert_eq!(left.bracketed_paste(), right.bracketed_paste());
        assert_eq!(left.mouse_protocol_mode(), right.mouse_protocol_mode());
        assert_eq!(
            left.mouse_protocol_encoding(),
            right.mouse_protocol_encoding()
        );
        assert_eq!(left.fgcolor(), right.fgcolor());
        assert_eq!(left.bgcolor(), right.bgcolor());
        assert_eq!(left.bold(), right.bold());
        assert_eq!(left.italic(), right.italic());
        for row in 0..left.size().0 {
            assert_eq!(left.row_wrapped(row), right.row_wrapped(row));
            for col in 0..left.size().1 {
                assert_eq!(left.cell(row, col), right.cell(row, col));
            }
        }
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
    fn snapshot_round_trips_full_screen_tui_wide_cells_modes_and_resize() {
        let mut source = vt100::Parser::new(8, 30, SCREEN_SCROLLBACK_ROWS);
        source.process(b"\x1b[2J\x1b[Hshell history\r\n$ codex");
        source.process(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[1;36mCodex\x1b[0m");
        source.screen_mut().set_size(10, 36);
        source.process(b"\x1b[2J\x1b[H\x1b[1;36mCodex\x1b[0m\r\n");
        // Split a multi-byte wide character exactly as separate PTY reads can.
        let wide = "状态：运行中 ✅".as_bytes();
        source.process(&wide[..4]);
        source.process(&wide[4..]);
        source.process(
            b"\x1b[5;4H\x1b[38;5;214mworking\x1b[3m...\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h",
        );

        let snapshot = render_snapshot(&mut source);
        let mut restored = restore_snapshot(&snapshot);
        assert!(snapshot.alternate_screen);
        assert!(!snapshot.normal_contents.is_empty());
        assert_active_screens_equal(&source, &restored);

        // The hidden normal grid must also survive. This is what becomes
        // visible when Codex/vim exits its alternate screen after reconnect.
        source.process(b"\x1b[?1049l");
        restored.process(b"\x1b[?1049l");
        assert_active_screens_equal(&source, &restored);
    }

    #[test]
    fn snapshot_replaces_stale_client_contents_instead_of_replaying_history() {
        let mut source = vt100::Parser::new(4, 16, SCREEN_SCROLLBACK_ROWS);
        source.process(b"\x1b[2J\x1b[2;3Hauthoritative");
        let snapshot = render_snapshot(&mut source);

        let mut restored = vt100::Parser::new(4, 16, SCREEN_SCROLLBACK_ROWS);
        restored.process(b"stale client data\r\nthat must vanish");
        restored.process(b"\x1bc");
        restored.process(&snapshot.contents);
        assert_active_screens_equal(&source, &restored);
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
            })
            .unwrap();

        let first = terminal.acquire_lease(false, false, "").unwrap();
        let resumed = terminal
            .acquire_lease(false, false, &first.resume_token)
            .unwrap();
        assert_ne!(first.lease_id, resumed.lease_id);
        assert_eq!(first.resume_token, resumed.resume_token);
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
