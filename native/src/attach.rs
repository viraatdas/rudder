//! `rudder attach`: a second terminal that mirrors one live agent pane.
//!
//! The dashboard is the single owner of every agent PTY (see AGENTS.md §7,
//! the single-writer invariant), so another tab cannot open the PTY itself.
//! Instead the dashboard listens on a unix socket under `.rudder/` and each
//! attached terminal is a thin client: the dashboard renders the pane the same
//! way it renders its own worker pane (`styled_line_window_snapshot`, so the
//! view, the scrollback and ⌥v/⌥b are exactly what the dashboard would show)
//! and streams changed rows as ANSI text; the client draws them and sends
//! keys back. It is tmux's shape: one server, many attached terminals, and the
//! session survives every client going away.
//!
//! Sizing follows the pane. A pane has one size, and the dashboard resizes the
//! SELECTED agent to its worker area on every frame; an agent that is attached
//! but not selected in the dashboard is resized to the attached terminal
//! instead, so a side-by-side split of unselected agents renders at full width.
//!
//! Wire format: newline-delimited JSON both ways (`ClientMsg` / `ServerMsg`).

use super::*;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

pub(crate) const ATTACH_SOCKET_NAME: &str = "attach.sock";
/// Frames queued per client before the dashboard stops waiting for it. A
/// full frame is idempotent, so a client that falls behind is simply sent a
/// full frame once it catches up (`needs_full`) rather than stalling the loop.
const CLIENT_QUEUE: usize = 256;
/// A wheel notch, in rows, the same feel as the dashboard's worker pane.
pub(crate) const ATTACH_WHEEL_ROWS: isize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum ClientMsg {
    /// Attach to one agent. `selector` is a run id, a node id, a 1-based index
    /// into the list `List` returns, or a case-insensitive substring of the
    /// task title.
    Attach {
        selector: String,
        rows: u16,
        cols: u16,
    },
    List,
    Input {
        bytes: Vec<u8>,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Nav {
        op: NavOp,
    },
    Detach,
    /// Open a full second view of the dashboard (every pane, the task bar,
    /// new agents), not one agent's pane. See view.rs.
    Dashboard {
        rows: u16,
        cols: u16,
    },
    /// A terminal event from a dashboard view, applied as if typed here.
    Event {
        event: Event,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NavOp {
    /// ⌥v in the dashboard: the previous question, top row.
    PreviousQuestion,
    /// ⌥b: back to the live bottom.
    Latest,
    /// Scroll the view by `ATTACH_WHEEL_ROWS` (wheel, ⌥k/⌥j).
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentSummary {
    pub(crate) index: usize,
    pub(crate) run_id: String,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum ServerMsg {
    Hello {
        run_id: String,
        label: String,
        rows: u16,
        cols: u16,
    },
    Agents {
        agents: Vec<AgentSummary>,
    },
    /// Changed rows of the pane as ANSI text, `(row, text)`. `full` means the
    /// client should clear first: the size changed or it fell behind.
    Frame {
        rows: u16,
        cols: u16,
        full: bool,
        lines: Vec<(u16, String)>,
        cursor: Option<(u16, u16)>,
    },
    Notice {
        text: String,
    },
    /// A dashboard view's output: terminal bytes to write as they are.
    Screen {
        data: String,
    },
    Bye {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Server side (lives in the dashboard process)
// ---------------------------------------------------------------------------

pub(crate) enum AttachEvent {
    Connected { id: u64, tx: SyncSender<String> },
    Message { id: u64, msg: ClientMsg },
    Disconnected { id: u64 },
}

/// The listening socket plus the channel its threads feed. Threads never
/// touch `App`: every message crosses this channel and is applied by the main
/// loop (`App::service_attach_clients`).
///
/// The socket itself lives under `<rudder home>/attach/<hash>.sock`, because a
/// unix socket path is limited to ~104 bytes and a checkout's path is not;
/// `<repo>/.rudder/attach.sock` is a symlink to it, which is what clients
/// look for and read.
pub(crate) struct AttachServer {
    link: PathBuf,
    path: PathBuf,
    rx: Receiver<AttachEvent>,
}

impl AttachServer {
    /// Serve `<repo>/.rudder/attach.sock` (`link`). A socket whose owner is
    /// gone is replaced; one that still answers means another dashboard owns
    /// this checkout, and this one does not serve attachments.
    pub(crate) fn start(link: PathBuf, waker: Option<PtyOutputWaker>) -> Result<Self> {
        let path = attach_socket_target(&link)?;
        for dir in [link.parent(), path.parent()].into_iter().flatten() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(anyhow!("another dashboard is already serving {}", link.display()));
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind {}", path.display()))?;
        if std::fs::symlink_metadata(&link).is_ok() {
            let _ = std::fs::remove_file(&link);
        }
        std::os::unix::fs::symlink(&path, &link)
            .with_context(|| format!("link {} -> {}", link.display(), path.display()))?;
        let (tx, rx) = mpsc::channel::<AttachEvent>();
        let next_id = Arc::new(AtomicU64::new(1));
        std::thread::Builder::new()
            .name("rudder-attach-accept".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else {
                        continue;
                    };
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    Self::serve_connection(id, stream, tx.clone(), waker.clone());
                }
            })
            .context("spawn attach accept thread")?;
        Ok(Self { link, path, rx })
    }

    fn serve_connection(
        id: u64,
        stream: UnixStream,
        events: mpsc::Sender<AttachEvent>,
        waker: Option<PtyOutputWaker>,
    ) {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let (out_tx, out_rx) = mpsc::sync_channel::<String>(CLIENT_QUEUE);
        let _ = std::thread::Builder::new()
            .name(format!("rudder-attach-writer-{id}"))
            .spawn(move || {
                while let Ok(line) = out_rx.recv() {
                    if writer.write_all(line.as_bytes()).is_err()
                        || writer.write_all(b"\n").is_err()
                    {
                        break;
                    }
                }
                let _ = writer.shutdown(std::net::Shutdown::Both);
            });
        let wake = move || {
            if let Some(waker) = waker.as_ref() {
                waker();
            }
        };
        if events
            .send(AttachEvent::Connected { id, tx: out_tx })
            .is_err()
        {
            return;
        }
        wake();
        let _ = std::thread::Builder::new()
            .name(format!("rudder-attach-reader-{id}"))
            .spawn(move || {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let Ok(msg) = serde_json::from_str::<ClientMsg>(&line) else {
                        continue;
                    };
                    if events.send(AttachEvent::Message { id, msg }).is_err() {
                        return;
                    }
                    wake();
                }
                let _ = events.send(AttachEvent::Disconnected { id });
                wake();
            });
    }

    pub(crate) fn try_recv(&self) -> Option<AttachEvent> {
        self.rx.try_recv().ok()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.link
    }
}

impl Drop for AttachServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.link);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The short path the socket is actually bound at, keyed by the checkout.
fn attach_socket_target(link: &Path) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let home = crate::signals::rudder_home().ok_or_else(|| anyhow!("no HOME for the attach socket"))?;
    let canonical = link
        .parent()
        .and_then(|dir| dir.parent())
        .map(|repo| repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()))
        .unwrap_or_else(|| link.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let name = format!("{:016x}.sock", hasher.finish());
    let path = home.join("attach").join(&name);
    // Even the rudder home can be too deep (a relocated RUDDER_HOME); a short
    // per-user directory under /tmp always fits.
    if path.as_os_str().len() < 100 {
        return Ok(path);
    }
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from(format!("/tmp/rudder-{uid}")).join(name))
}

/// What to `connect()` to for a socket path a client found: the symlink's
/// target (short enough for a unix socket) rather than the link itself.
pub(crate) fn resolve_attach_socket(path: &Path) -> PathBuf {
    std::fs::read_link(path).unwrap_or_else(|_| path.to_path_buf())
}

/// One attached terminal, as the dashboard sees it.
pub(crate) struct AttachClient {
    pub(crate) id: u64,
    tx: SyncSender<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// The rows this client currently shows, so only changed ones are sent.
    last_lines: Vec<String>,
    last_cursor: Option<(u16, u16)>,
    last_generation: Option<u64>,
    needs_full: bool,
    gone: bool,
    /// A full dashboard view instead of a single pane (`rudder` in a second
    /// tab). `None` for a pane attachment. Taken out while it is in use.
    pub(crate) dashboard: Option<Box<DashboardView>>,
}

impl AttachClient {
    pub(crate) fn new(id: u64, tx: SyncSender<String>) -> Self {
        Self {
            id,
            tx,
            run_id: None,
            rows: 0,
            cols: 0,
            last_lines: Vec::new(),
            last_cursor: None,
            last_generation: None,
            needs_full: true,
            gone: false,
            dashboard: None,
        }
    }

    /// The send queue overflowed since the last check; a dashboard view has
    /// to repaint from scratch.
    pub(crate) fn take_needs_full(&mut self) -> bool {
        std::mem::take(&mut self.needs_full)
    }

    pub(crate) fn is_gone(&self) -> bool {
        self.gone
    }

    pub(crate) fn set_size(&mut self, rows: u16, cols: u16) {
        if (rows, cols) != (self.rows, self.cols) {
            self.rows = rows;
            self.cols = cols;
            self.needs_full = true;
        }
    }

    pub(crate) fn send(&mut self, msg: &ServerMsg) {
        if self.gone {
            return;
        }
        let Ok(line) = serde_json::to_string(msg) else {
            return;
        };
        match self.tx.try_send(line) {
            Ok(()) => {}
            // Behind: drop this one and send a full frame once there is room.
            Err(TrySendError::Full(_)) => self.needs_full = true,
            Err(TrySendError::Disconnected(_)) => self.gone = true,
        }
    }

    /// Push the pane's current view. Cheap when nothing changed: the pane's
    /// render generation is checked before any row is rendered.
    pub(crate) fn push_frame(&mut self, terminal: &mut TerminalPane) {
        if self.gone || self.rows == 0 || self.cols == 0 {
            return;
        }
        let generation = terminal.render_generation();
        if !self.needs_full && self.last_generation == Some(generation) {
            return;
        }
        let (start, rows) = terminal.styled_line_window_snapshot(self.rows as usize);
        let mut lines: Vec<String> = rows.iter().map(|row| styled_row_to_ansi(row)).collect();
        lines.resize(self.rows as usize, String::new());
        let cursor = terminal.cursor();
        let cursor = (terminal.scrollback() == 0
            && cursor.visible
            && (cursor.row as usize) >= start)
            .then(|| ((cursor.row as usize - start) as u16, cursor.col));

        let full = self.needs_full || self.last_lines.len() != lines.len();
        let changed: Vec<(u16, String)> = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| full || self.last_lines.get(*index) != Some(line))
            .map(|(index, line)| (index as u16, line.clone()))
            .collect();
        if !full && changed.is_empty() && cursor == self.last_cursor {
            self.last_generation = Some(generation);
            return;
        }
        let queued_full = self.needs_full;
        self.needs_full = false;
        self.send(&ServerMsg::Frame {
            rows: self.rows,
            cols: self.cols,
            full,
            lines: changed,
            cursor,
        });
        if self.needs_full {
            // The send found the queue full; keep the old picture so the next
            // full frame diffs from nothing.
            self.needs_full = queued_full || true;
            return;
        }
        self.last_lines = lines;
        self.last_cursor = cursor;
        self.last_generation = Some(generation);
    }
}

/// One row of styled cells as ANSI text: SGR changes only where the style
/// changes, reset at the end. Wide glyphs are one cell here and two columns on
/// the client's terminal, exactly as on the pane.
pub(crate) fn styled_row_to_ansi(cells: &[StyledTerminalCell]) -> String {
    let mut out = String::with_capacity(cells.len() + 16);
    let mut current: Option<CellStyle> = None;
    for cell in cells {
        let style = CellStyle::of(cell);
        if current != Some(style) {
            style.write_sgr(&mut out);
            current = Some(style);
        }
        out.push_str(cell.contents.as_str());
    }
    if current.is_some_and(|style| !style.is_plain()) {
        out.push_str("\x1b[0m");
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn of(cell: &StyledTerminalCell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline,
            inverse: cell.inverse,
        }
    }

    fn is_plain(&self) -> bool {
        matches!(self.fg, vt100::Color::Default)
            && matches!(self.bg, vt100::Color::Default)
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
    }

    fn write_sgr(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.dim {
            out.push_str(";2");
        }
        if self.italic {
            out.push_str(";3");
        }
        if self.underline {
            out.push_str(";4");
        }
        if self.inverse {
            out.push_str(";7");
        }
        match self.fg {
            vt100::Color::Default => {}
            vt100::Color::Idx(n) if n < 8 => {
                let _ = write!(out, ";{}", 30 + n);
            }
            vt100::Color::Idx(n) if n < 16 => {
                let _ = write!(out, ";{}", 90 + (n - 8));
            }
            vt100::Color::Idx(n) => {
                let _ = write!(out, ";38;5;{n}");
            }
            vt100::Color::Rgb(r, g, b) => {
                let _ = write!(out, ";38;2;{r};{g};{b}");
            }
        }
        match self.bg {
            vt100::Color::Default => {}
            vt100::Color::Idx(n) if n < 8 => {
                let _ = write!(out, ";{}", 40 + n);
            }
            vt100::Color::Idx(n) if n < 16 => {
                let _ = write!(out, ";{}", 100 + (n - 8));
            }
            vt100::Color::Idx(n) => {
                let _ = write!(out, ";48;5;{n}");
            }
            vt100::Color::Rgb(r, g, b) => {
                let _ = write!(out, ";48;2;{r};{g};{b}");
            }
        }
        out.push('m');
    }
}

/// Where a dashboard for the checkout containing `start` would listen: the
/// nearest ancestor that has a `.rudder/` directory.
pub(crate) fn find_attach_socket(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let candidate = current.join(".rudder").join(ATTACH_SOCKET_NAME);
        if candidate.exists() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Client side (`rudder-native attach`)
// ---------------------------------------------------------------------------

/// `rudder-native attach [--socket <path>] [<selector>]`.
pub(crate) fn run_attach_client(args: &[String]) -> Result<()> {
    let mut socket: Option<PathBuf> = std::env::var_os("RUDDER_ATTACH_SOCKET").map(PathBuf::from);
    let mut selector: Option<String> = None;
    let mut dashboard = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => socket = iter.next().map(PathBuf::from),
            "--dashboard" => dashboard = true,
            "-h" | "--help" => {
                println!("{ATTACH_USAGE}");
                return Ok(());
            }
            other => selector = Some(other.to_string()),
        }
    }
    let socket = match socket {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir().context("current directory")?;
            find_attach_socket(&cwd).ok_or_else(|| {
                anyhow!("no rudder dashboard is running for this checkout (no .rudder/{ATTACH_SOCKET_NAME} up the tree). Start `rudder` in another tab first.")
            })?
        }
    };
    let stream = UnixStream::connect(resolve_attach_socket(&socket)).with_context(|| {
        format!(
            "the dashboard at {} is not answering; it may have quit",
            socket.display()
        )
    })?;
    if dashboard {
        return run_dashboard_client(stream);
    }
    let mut reader = BufReader::new(stream.try_clone().context("clone socket")?);
    let mut writer = stream;

    let selector = match selector {
        Some(selector) => selector,
        None => pick_agent(&mut reader, &mut writer)?,
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    send_client(&mut writer, &ClientMsg::Attach { selector, rows, cols })?;
    let hello = read_server(&mut reader)?;
    let label = match hello {
        ServerMsg::Hello { label, .. } => label,
        ServerMsg::Bye { reason } => return Err(anyhow!("{reason}")),
        other => return Err(anyhow!("unexpected reply from the dashboard: {other:?}")),
    };

    let done = Arc::new(AtomicBool::new(false));
    let bye_reason: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let done = Arc::clone(&done);
        let bye_reason = Arc::clone(&bye_reason);
        std::thread::spawn(move || {
            let mut line = String::new();
            let mut out = io::stdout();
            let (mut term_cols, mut term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim_end()) else {
                    continue;
                };
                match msg {
                    ServerMsg::Frame {
                        full,
                        lines,
                        cursor,
                        ..
                    } => {
                        if let Ok(size) = crossterm::terminal::size() {
                            (term_cols, term_rows) = size;
                        }
                        let _ = term_cols;
                        let mut buf = String::new();
                        buf.push_str("\x1b[?25l");
                        if full {
                            buf.push_str("\x1b[0m\x1b[2J");
                        }
                        for (row, text) in lines {
                            if row >= term_rows {
                                continue;
                            }
                            use std::fmt::Write as _;
                            let _ = write!(buf, "\x1b[{};1H{}\x1b[0m\x1b[K", row + 1, text);
                        }
                        match cursor {
                            Some((row, col)) if row < term_rows => {
                                use std::fmt::Write as _;
                                let _ = write!(buf, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
                            }
                            _ => {}
                        }
                        let _ = out.write_all(buf.as_bytes());
                        let _ = out.flush();
                    }
                    ServerMsg::Notice { text } => {
                        let _ = write!(out, "\x1b[s\x1b[{};1H\x1b[0;7m {text} \x1b[0m\x1b[K\x1b[u", term_rows);
                        let _ = out.flush();
                    }
                    ServerMsg::Bye { reason } => {
                        if let Ok(mut slot) = bye_reason.lock() {
                            *slot = Some(reason);
                        }
                        break;
                    }
                    ServerMsg::Hello { .. }
                    | ServerMsg::Agents { .. }
                    | ServerMsg::Screen { .. } => {}
                }
            }
            done.store(true, Ordering::Release);
        });
    }

    enable_raw_mode()?;
    {
        let mut stdout = io::stdout();
        // No autowrap: a pane wider than this terminal is clipped at the right
        // edge instead of wrapping and pushing every row down.
        let _ = execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        );
        let _ = enable_rudder_mouse_capture(&mut stdout);
        let _ = stdout.write_all(b"\x1b[?7l");
        let _ = set_terminal_title(&mut stdout, &format!("\u{25c9} {label}"));
        let _ = stdout.flush();
    }
    let result = attach_input_loop(&mut writer, &done);
    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?7h\x1b[?25h\x1b[0m");
        let _ = disable_rudder_mouse_capture(&mut stdout);
        let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = stdout.flush();
    }
    let _ = disable_raw_mode();
    result?;
    match bye_reason.lock().ok().and_then(|slot| slot.clone()) {
        Some(reason) => println!("rudder attach: {reason}"),
        None => println!("rudder attach: detached from {label}"),
    }
    Ok(())
}

/// A full second view of the running dashboard. The dashboard renders this
/// view (its own selection, focus and task bar over the shared agents) and
/// streams the terminal bytes; this side only draws them and forwards input.
fn run_dashboard_client(stream: UnixStream) -> Result<()> {
    let reader = BufReader::new(stream.try_clone().context("clone socket")?);
    let mut writer = stream;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    enable_raw_mode()?;
    {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            ),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        );
        let _ = enable_rudder_mouse_capture(&mut stdout);
        let _ = set_terminal_title(&mut stdout, &dashboard_view_title());
        let _ = stdout.flush();
    }

    let done = Arc::new(AtomicBool::new(false));
    let bye_reason: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let done = Arc::clone(&done);
        let bye_reason = Arc::clone(&bye_reason);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut line = String::new();
            let mut out = io::stdout();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                match serde_json::from_str::<ServerMsg>(line.trim_end()) {
                    Ok(ServerMsg::Screen { data }) => {
                        let _ = out.write_all(data.as_bytes());
                        let _ = out.flush();
                    }
                    Ok(ServerMsg::Bye { reason }) => {
                        if let Ok(mut slot) = bye_reason.lock() {
                            *slot = Some(reason);
                        }
                        break;
                    }
                    _ => {}
                }
            }
            done.store(true, Ordering::Release);
        });
    }

    let result = (|| -> Result<()> {
        send_client(&mut writer, &ClientMsg::Dashboard { rows, cols })?;
        while !done.load(Ordering::Acquire) {
            if !crossterm::event::poll(Duration::from_millis(100))? {
                continue;
            }
            let msg = match crossterm::event::read()? {
                Event::Resize(cols, rows) => ClientMsg::Resize { rows, cols },
                Event::Key(key) if key.kind == KeyEventKind::Release => continue,
                // Focus belongs to the tab the dashboard itself runs in
                // (it gates desktop notifications there).
                Event::FocusGained | Event::FocusLost => continue,
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => continue,
                event => ClientMsg::Event { event },
            };
            if send_client(&mut writer, &msg).is_err() {
                break;
            }
        }
        Ok(())
    })();

    {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?25h\x1b[0m");
        let _ = disable_rudder_mouse_capture(&mut stdout);
        let _ = execute!(
            stdout,
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = set_terminal_title(&mut stdout, "");
        let _ = stdout.flush();
    }
    let _ = disable_raw_mode();
    result?;
    match bye_reason.lock().ok().and_then(|slot| slot.clone()) {
        Some(reason) => println!("rudder: {reason}"),
        None => println!("rudder: the dashboard went away"),
    }
    Ok(())
}

/// Tab title for a second view: the repo name, marked as a view so it is not
/// mistaken for the tab that owns the agents.
fn dashboard_view_title() -> String {
    let cwd = std::env::current_dir()
        .map(|path| repo_root(&path))
        .unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("\u{25ce} {name}")
}

const ATTACH_USAGE: &str = "rudder attach [<agent>]
rudder attach --dashboard

Mirror one live agent pane of the dashboard running in this checkout.
<agent> is a run id, a node id, a number from the list, or part of the title;
omit it to pick from a list. --dashboard opens a full second view of the
dashboard instead (what plain `rudder` does when one is already running).

Keys: everything goes to the agent, except
  ^W q / ^W d   detach (the agent keeps running in the dashboard)
  \u{2325}v / \u{2325}b       previous question / back to the latest message
  \u{2325}k / \u{2325}j       scroll up / down (or the mouse wheel)";

fn send_client(writer: &mut UnixStream, msg: &ClientMsg) -> Result<()> {
    let line = serde_json::to_string(msg)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_server(reader: &mut BufReader<UnixStream>) -> Result<ServerMsg> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(anyhow!("the dashboard closed the connection"));
        }
        if let Ok(msg) = serde_json::from_str::<ServerMsg>(line.trim_end()) {
            return Ok(msg);
        }
    }
}

fn pick_agent(reader: &mut BufReader<UnixStream>, writer: &mut UnixStream) -> Result<String> {
    send_client(writer, &ClientMsg::List)?;
    let agents = match read_server(reader)? {
        ServerMsg::Agents { agents } => agents,
        ServerMsg::Bye { reason } => return Err(anyhow!("{reason}")),
        other => return Err(anyhow!("unexpected reply from the dashboard: {other:?}")),
    };
    if agents.is_empty() {
        return Err(anyhow!("the dashboard has no live agent panes to attach to"));
    }
    if agents.len() == 1 {
        return Ok(agents[0].run_id.clone());
    }
    println!("Live agents:");
    for agent in &agents {
        println!(
            "  {:>2}  {:<40} {:<10} {}",
            agent.index,
            truncate_chars(&agent.label, 40),
            agent.backend,
            agent.status
        );
    }
    print!("Attach to which one? [1-{}] ", agents.len());
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    let choice: usize = answer
        .parse()
        .map_err(|_| anyhow!("not a number: {answer:?}"))?;
    agents
        .iter()
        .find(|agent| agent.index == choice)
        .map(|agent| agent.run_id.clone())
        .ok_or_else(|| anyhow!("no agent numbered {choice}"))
}

fn attach_input_loop(writer: &mut UnixStream, done: &AtomicBool) -> Result<()> {
    let mut leader = false;
    while !done.load(Ordering::Acquire) {
        if !crossterm::event::poll(Duration::from_millis(100))? {
            continue;
        }
        let event = crossterm::event::read()?;
        let msg = match event {
            Event::Resize(cols, rows) => Some(ClientMsg::Resize { rows, cols }),
            Event::Paste(text) => Some(ClientMsg::Input {
                bytes: bracketed_paste_bytes(&text),
            }),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => Some(ClientMsg::Nav { op: NavOp::Up }),
                MouseEventKind::ScrollDown => Some(ClientMsg::Nav { op: NavOp::Down }),
                _ => None,
            },
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if leader {
                    leader = false;
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('d')) {
                        send_client(writer, &ClientMsg::Detach)?;
                        return Ok(());
                    }
                    // Any other key after ^W is just that key.
                }
                match attach_key_action(key) {
                    AttachKey::Leader => {
                        leader = true;
                        None
                    }
                    AttachKey::Nav(op) => Some(ClientMsg::Nav { op }),
                    AttachKey::Input(bytes) => Some(ClientMsg::Input { bytes }),
                    AttachKey::Ignore => None,
                }
            }
            _ => None,
        };
        if let Some(msg) = msg {
            send_client(writer, &msg)?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttachKey {
    Leader,
    Nav(NavOp),
    Input(Vec<u8>),
    Ignore,
}

/// The same chords the dashboard's worker pane honours, including the
/// typographic characters macOS terminals send for Option+key.
pub(crate) fn attach_key_action(key: KeyEvent) -> AttachKey {
    let alt = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META);
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('w')) {
        return AttachKey::Leader;
    }
    let nav = match key.code {
        KeyCode::Char('v' | 'V') if alt => Some(NavOp::PreviousQuestion),
        KeyCode::Char('\u{221a}') => Some(NavOp::PreviousQuestion),
        KeyCode::Char('b' | 'B') if alt => Some(NavOp::Latest),
        KeyCode::Char('\u{222b}') => Some(NavOp::Latest),
        KeyCode::Char('k' | 'K') if alt => Some(NavOp::Up),
        KeyCode::Char('\u{02da}') => Some(NavOp::Up),
        KeyCode::Char('j' | 'J') if alt => Some(NavOp::Down),
        KeyCode::Char('\u{2206}') => Some(NavOp::Down),
        _ => None,
    };
    if let Some(op) = nav {
        return AttachKey::Nav(op);
    }
    match terminal_bytes_for_key(key) {
        Some(bytes) => AttachKey::Input(bytes),
        None => AttachKey::Ignore,
    }
}
