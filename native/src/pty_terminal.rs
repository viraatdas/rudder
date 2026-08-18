//! PTY-backed terminal pane abstraction for the native Rudder app.
//!
//! `TerminalPane` intentionally keeps the process/PTY plumbing separate from
//! the rest of the app. It currently uses `portable-pty` for cross-platform PTY
//! creation and `vt100` for a plain-text terminal screen buffer. The public API
//! is small enough that the buffer backend can be replaced with
//! `alacritty_terminal` later without forcing app layout code to change.

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_SCROLLBACK_LINES: usize = 2_000;

/// Callback invoked by a pane's reader thread each time it hands fresh PTY bytes
/// to the output channel, so the main event loop can wake and drain/redraw them
/// immediately instead of waiting for its next poll tick. The closure is expected
/// to coalesce (cheap, idempotent) — it may fire once per 8KB read burst.
pub type PtyOutputWaker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 {
            return Err(anyhow!("terminal size must be non-zero"));
        }

        Ok(Self { rows, cols })
    }

    fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl TerminalCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn with_args(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPaneOptions {
    pub size: TerminalSize,
    pub cwd: Option<PathBuf>,
    pub scrollback_lines: usize,
    pub term: String,
    /// Render ONLY the live screen (the current vt100 grid), never accumulated
    /// scrollback. Set for the interactive plan-mode front-end: agent TUIs render
    /// inline (no alt-screen), so its startup welcome + redraw frames otherwise pile
    /// up in region_scrollback and the pane shows stacked duplicate banners. The
    /// model redraws its full visible UI each turn, so the live screen alone is the
    /// correct view; we trade PageUp history (irrelevant here) for a clean pane.
    pub live_screen_only: bool,
}

impl Default for TerminalPaneOptions {
    fn default() -> Self {
        Self {
            size: TerminalSize::default(),
            cwd: None,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            term: "xterm-256color".to_string(),
            live_screen_only: false,
        }
    }
}

pub struct TerminalPane {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    reader_thread: Option<JoinHandle<()>>,
    parser: vt100::Parser,
    size: TerminalSize,
    alternate_history: Vec<Vec<String>>,
    alternate_history_offset: usize,
    last_alternate_snapshot: Vec<String>,
    alternate_history_limit: usize,
    region_scrollback: Vec<Vec<StyledTerminalCell>>,
    region_scrollback_offset: usize,
    region_scrollback_limit: usize,
    tracked_scroll_region: Option<(u16, u16)>,
    ansi_state: AnsiTrackState,
    styled_lines_cache: Option<Vec<Vec<StyledTerminalCell>>>,
    output_log: String,
    output_log_limit: usize,
    live_screen_only: bool,
    /// Optional waker shared with the reader thread. Kept behind a mutex so the
    /// app can install it after the pane is spawned (see [`set_output_waker`]).
    output_waker: Arc<Mutex<Option<PtyOutputWaker>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AnsiTrackState {
    Ground,
    Escape,
    Csi(Vec<u8>),
}

/// The number of bytes a single cell's grapheme cluster can occupy.
///
/// This mirrors `vt100`'s own fixed-size cell storage (`CONTENT_BYTES = 22`).
/// Because the parser physically cannot hand us more than that, inline storage
/// of the same width is lossless: there is no heap-fallback case to get wrong.
const CELL_CONTENT_BYTES: usize = 22;

/// One cell's text, stored inline instead of in a `String`.
///
/// The grid is re-materialized from the parser on every frame that follows new
/// output, so a `String` here meant one heap allocation per cell — roughly 9,300
/// mallocs per frame for a full-screen pane, which profiling showed dominating
/// render time. Inline storage makes the whole cell `Copy`, so rebuilding a row
/// and cloning the visible window are both plain memory copies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CellContents {
    bytes: [u8; CELL_CONTENT_BYTES],
    len: u8,
}

impl CellContents {
    pub const SPACE: Self = Self::from_ascii(b' ');

    const fn from_ascii(byte: u8) -> Self {
        let mut bytes = [0u8; CELL_CONTENT_BYTES];
        bytes[0] = byte;
        Self { bytes, len: 1 }
    }

    /// Copies `text` inline. Anything that would not fit is truncated on a char
    /// boundary rather than panicking or corrupting UTF-8; vt100 cannot produce
    /// such a value, so this only guards future callers.
    pub fn new(text: &str) -> Self {
        let mut end = text.len().min(CELL_CONTENT_BYTES);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0u8; CELL_CONTENT_BYTES];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Self {
            bytes,
            len: end as u8,
        }
    }

    pub fn from_char(value: char) -> Self {
        let mut buffer = [0u8; 4];
        Self::new(value.encode_utf8(&mut buffer))
    }

    pub fn as_str(&self) -> &str {
        // Always valid: every constructor copies from an existing `&str` and
        // only ever cuts on a char boundary.
        str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Text for a ratatui `Span`, which wants `Cow<'static, str>`.
    ///
    /// The overwhelmingly common cell is a single ASCII byte, and those are
    /// served from a static table — so the render path allocates nothing at all
    /// for ordinary text, and only pays for wide characters and emoji.
    pub fn span_text(&self) -> Cow<'static, str> {
        if self.len == 1 {
            if let Some(text) = ascii_str(self.bytes[0]) {
                return Cow::Borrowed(text);
            }
        }
        Cow::Owned(self.as_str().to_string())
    }
}

/// `[0, 1, 2, ..., 127]`, so any single ASCII byte can be handed out as a
/// `&'static str` slice of length one without allocating.
static ASCII_BYTES: [u8; 128] = {
    let mut table = [0u8; 128];
    let mut index = 0;
    while index < 128 {
        table[index] = index as u8;
        index += 1;
    }
    table
};

fn ascii_str(byte: u8) -> Option<&'static str> {
    let index = byte as usize;
    let slice = ASCII_BYTES.get(index..index.checked_add(1)?)?;
    str::from_utf8(slice).ok()
}

impl std::ops::Deref for CellContents {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for CellContents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl PartialEq<str> for CellContents {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CellContents {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for CellContents {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// A `Copy` cell with no heap tail: rebuilding a row and cloning the visible
/// window are both memory copies, and a scrollback row costs exactly its cells.
/// The assertion is here so a future field cannot quietly reintroduce a pointer
/// (and with it, a per-cell allocation) without someone noticing.
const _: () = assert!(std::mem::size_of::<StyledTerminalCell>() == 36);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledTerminalCell {
    pub contents: CellContents,
    pub fg: vt100::Color,
    pub bg: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

impl TerminalPane {
    /// Spawn the user's shell when `command` is `None`, or spawn the supplied
    /// program and arguments when it is `Some`.
    ///
    /// Output is collected by a background reader thread. Call
    /// [`drain_output`](Self::drain_output) to feed pending bytes into the
    /// terminal buffer, then [`visible_lines_snapshot`](Self::visible_lines_snapshot)
    /// to render the current screen as plain text without draining again.
    pub fn spawn_shell_or_command(
        command: Option<TerminalCommand>,
        options: TerminalPaneOptions,
    ) -> Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(options.size.pty_size())
            .context("failed to open PTY")?;

        let mut builder = command_builder(command, &options.term)?;
        if let Some(cwd) = &options.cwd {
            builder.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .context("failed to spawn command in PTY")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;
        let (output_tx, output_rx) = mpsc::channel();
        let output_waker: Arc<Mutex<Option<PtyOutputWaker>>> = Arc::new(Mutex::new(None));
        let reader_waker = Arc::clone(&output_waker);
        let reader_thread = thread::Builder::new()
            .name("rudder-pty-reader".to_string())
            .spawn(move || {
                let mut buf = [0_u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if output_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            // Nudge the main event loop so freshly-arrived bytes are
                            // drained and drawn immediately, instead of waiting up to a
                            // full tick. The waker itself coalesces, so firing per read
                            // burst is cheap; a poisoned lock just skips the wake and
                            // falls back to the tick cadence.
                            if let Ok(guard) = reader_waker.lock() {
                                if let Some(waker) = guard.as_ref() {
                                    waker();
                                }
                            }
                        }
                        // A transient interruption (e.g. EINTR from a signal) is NOT
                        // end-of-stream: retry rather than tearing down the reader, or a
                        // still-running agent's output would freeze and completion would
                        // hang on the scrape fallback. Only a real EOF/error ends the loop.
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("failed to start PTY reader thread")?;

        Ok(Self {
            master: pair.master,
            child,
            writer,
            output_rx,
            reader_thread: Some(reader_thread),
            parser: vt100::Parser::new(
                options.size.rows,
                options.size.cols,
                options.scrollback_lines,
            ),
            size: options.size,
            alternate_history: Vec::new(),
            alternate_history_offset: 0,
            last_alternate_snapshot: Vec::new(),
            alternate_history_limit: options.scrollback_lines.max(DEFAULT_ROWS as usize),
            region_scrollback: Vec::new(),
            region_scrollback_offset: 0,
            region_scrollback_limit: options.scrollback_lines,
            tracked_scroll_region: None,
            ansi_state: AnsiTrackState::Ground,
            styled_lines_cache: None,
            output_log: String::new(),
            output_log_limit: 200_000,
            live_screen_only: options.live_screen_only,
            output_waker,
        })
    }

    /// Install (or replace) the waker the reader thread fires after each output
    /// send. Called by the app right after spawning a pane so PTY output wakes the
    /// main loop; panes never given a waker simply fall back to the tick cadence.
    pub fn set_output_waker(&self, waker: PtyOutputWaker) {
        if let Ok(mut guard) = self.output_waker.lock() {
            *guard = Some(waker);
        }
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        let write_result = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush());
        match write_result {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Some(reason) = self.child_exit_summary() {
                    Err(anyhow!("agent process exited ({reason})"))
                } else {
                    Err(anyhow::Error::new(err).context("failed to write input to PTY"))
                }
            }
        }
    }

    fn child_exit_summary(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.exit_code();
                if status.success() {
                    Some("exit 0".to_string())
                } else {
                    Some(describe_exit_code(code))
                }
            }
            _ => None,
        }
    }

    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<()> {
        self.master
            .resize(size.pty_size())
            .context("failed to resize PTY")?;
        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.size = size;
        self.last_alternate_snapshot.clear();
        self.tracked_scroll_region = None;
        // Region scrollback rows were captured at the old column width; rendering
        // them under the new geometry splices mismatched-width rows. Drop the
        // captured history and offset so the post-resize view is consistent.
        self.region_scrollback.clear();
        self.region_scrollback_offset = 0;
        self.invalidate_render_cache();
        Ok(())
    }

    /// Drain all currently available process output into the terminal buffer.
    ///
    /// The returned bytes are raw terminal output for logging/debugging. The
    /// in-memory buffer is updated before this method returns.
    pub fn drain_output(&mut self) -> Vec<u8> {
        let mut drained = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.process_output_chunk(&chunk);
            self.append_output_log(&chunk);
            drained.extend_from_slice(&chunk);
        }
        if !drained.is_empty() {
            self.invalidate_render_cache();
        }
        drained
    }

    /// Return the current visible terminal rows as plain text.
    ///
    /// This omits attributes, cursor shape, selection state, and image/graphics
    /// protocols. Those are the main reasons the interface is isolated: a later
    /// `alacritty_terminal` backend can preserve richer cell metadata while
    /// keeping callers on this app-facing API.
    pub fn visible_lines(&mut self) -> Vec<String> {
        self.drain_output();
        self.visible_lines_snapshot()
    }

    pub fn visible_lines_snapshot(&self) -> Vec<String> {
        // Plan-mode front-end: always the live screen, no scrollback (see the field doc).
        if self.live_screen_only {
            return self.current_visible_lines_snapshot();
        }
        if let Some(snapshot) = self.alternate_history_snapshot() {
            return snapshot.clone();
        }

        if self.region_scrollback_offset == 0 {
            return self.current_visible_lines_snapshot();
        }

        self.region_scrollback_view(self.current_visible_lines_snapshot(), |cells| {
            styled_cells_to_plain(cells)
        })
    }

    pub fn output_log_snapshot(&self) -> &str {
        &self.output_log
    }

    fn current_visible_lines_snapshot(&self) -> Vec<String> {
        self.parser
            .screen()
            .rows(0, self.size.cols)
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect()
    }

    pub fn styled_lines(&mut self) -> Vec<Vec<StyledTerminalCell>> {
        self.drain_output();
        self.styled_lines_snapshot()
    }

    pub fn styled_lines_snapshot(&mut self) -> Vec<Vec<StyledTerminalCell>> {
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        self.styled_lines_cache.clone().unwrap_or_default()
    }

    pub fn styled_line_window_snapshot(
        &mut self,
        height: usize,
    ) -> (usize, Vec<Vec<StyledTerminalCell>>) {
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        let Some(rows) = self.styled_lines_cache.as_ref() else {
            return (0, Vec::new());
        };
        let start = rows.len().saturating_sub(height);
        (start, rows[start..].iter().take(height).cloned().collect())
    }

    fn compute_styled_lines_snapshot(&self) -> Vec<Vec<StyledTerminalCell>> {
        // Plan-mode front-end: always the live screen, no scrollback (see the field doc).
        if self.live_screen_only {
            return self.current_styled_screen_rows();
        }
        if let Some(snapshot) = self.alternate_history_snapshot() {
            return snapshot
                .iter()
                .map(|line| {
                    line.chars()
                        .map(|ch| StyledTerminalCell::plain(CellContents::from_char(ch)))
                        .collect()
                })
                .collect();
        }

        let current = self.current_styled_screen_rows();
        if self.region_scrollback_offset > 0 {
            return self.region_scrollback_view(current, Clone::clone);
        }

        current
    }

    fn current_styled_screen_rows(&self) -> Vec<Vec<StyledTerminalCell>> {
        let mut rows = Vec::with_capacity(self.size.rows as usize);

        for row in 0..self.size.rows {
            rows.push(self.styled_screen_row(row));
        }

        rows
    }

    fn styled_screen_row(&self, row: u16) -> Vec<StyledTerminalCell> {
        let screen = self.parser.screen();
        let mut cells = Vec::with_capacity(self.size.cols as usize);
        for col in 0..self.size.cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = if cell.has_contents() {
                CellContents::new(cell.contents())
            } else {
                CellContents::SPACE
            };
            cells.push(StyledTerminalCell {
                contents,
                fg: cell.fgcolor(),
                bg: cell.bgcolor(),
                bold: cell.bold(),
                dim: cell.dim(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
            });
        }

        trim_styled_cells(&mut cells);
        cells
    }

    fn region_scrollback_view<T, F>(&self, current: Vec<T>, map_history: F) -> Vec<T>
    where
        T: Clone,
        F: Fn(&Vec<StyledTerminalCell>) -> T,
    {
        let height = self.size.rows as usize;
        let total = self.region_scrollback.len() + current.len();
        let end = total.saturating_sub(self.region_scrollback_offset);
        let start = end.saturating_sub(height);
        let mut rows = Vec::with_capacity(height);

        for index in start..end {
            if index < self.region_scrollback.len() {
                rows.push(map_history(&self.region_scrollback[index]));
            } else if let Some(row) = current.get(index - self.region_scrollback.len()) {
                rows.push(row.clone());
            }
        }

        rows
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    pub fn scrollback(&self) -> usize {
        if self.parser.screen().alternate_screen() {
            return self.alternate_history_offset;
        }
        self.parser.screen().scrollback() + self.region_scrollback_offset
    }

    pub fn wants_sgr_mouse_events(&mut self) -> bool {
        self.drain_output();
        self.wants_sgr_mouse_events_snapshot()
    }

    pub fn wants_sgr_mouse_events_snapshot(&self) -> bool {
        let screen = self.parser.screen();
        screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
    }

    pub fn uses_alternate_screen(&mut self) -> bool {
        self.drain_output();
        self.uses_alternate_screen_snapshot()
    }

    pub fn uses_alternate_screen_snapshot(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    pub fn scrollback_by(&mut self, rows: isize) {
        if self.parser.screen().alternate_screen() {
            self.capture_alternate_snapshot();
            let max_offset = self.alternate_history.len().saturating_sub(1);
            let before = self.alternate_history_offset;
            self.alternate_history_offset = if rows.is_negative() {
                self.alternate_history_offset
                    .saturating_sub(rows.unsigned_abs())
            } else {
                self.alternate_history_offset.saturating_add(rows as usize)
            }
            .min(max_offset);
            if self.alternate_history_offset != before {
                self.invalidate_render_cache();
            }
            return;
        }

        let before = self.scrollback();
        if rows.is_negative() {
            let mut remaining = rows.unsigned_abs();
            if self.region_scrollback_offset > 0 {
                let consumed = remaining.min(self.region_scrollback_offset);
                self.region_scrollback_offset -= consumed;
                remaining -= consumed;
            }
            if remaining > 0 {
                let parser_before = self.parser.screen().scrollback();
                self.parser
                    .screen_mut()
                    .set_scrollback(parser_before.saturating_sub(remaining));
            }
        } else if rows > 0 {
            let mut remaining = rows as usize;
            if self.region_scrollback_offset == 0 {
                let parser_before = self.parser.screen().scrollback();
                self.parser
                    .screen_mut()
                    .set_scrollback(parser_before.saturating_add(remaining));
                let parser_after = self.parser.screen().scrollback();
                remaining = remaining.saturating_sub(parser_after.saturating_sub(parser_before));
            }
            if remaining > 0 && !self.region_scrollback.is_empty() {
                self.region_scrollback_offset = self
                    .region_scrollback_offset
                    .saturating_add(remaining)
                    .min(self.region_scrollback.len());
            }
        }

        if self.scrollback() != before {
            self.invalidate_render_cache();
        }
    }

    pub fn reset_scrollback(&mut self) {
        if self.alternate_history_offset != 0
            || self.region_scrollback_offset != 0
            || self.parser.screen().scrollback() != 0
        {
            self.alternate_history_offset = 0;
            self.region_scrollback_offset = 0;
            self.parser.screen_mut().set_scrollback(0);
            self.invalidate_render_cache();
        }
    }

    pub fn cursor(&self) -> TerminalCursor {
        let (row, col) = self.parser.screen().cursor_position();
        TerminalCursor {
            row,
            col,
            visible: !self.parser.screen().hide_cursor(),
        }
    }

    pub fn child_process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        self.child.try_wait().context("failed to poll child status")
    }

    /// Explicitly terminate and reap the PTY process group. Lifecycle callers
    /// use this instead of relying on Drop so the state transition and process
    /// teardown happen in a visible, deterministic order.
    pub fn terminate_and_wait(&mut self) {
        self.terminate_child();
    }
}

impl StyledTerminalCell {
    fn plain(contents: CellContents) -> Self {
        Self {
            contents,
            fg: vt100::Color::Default,
            bg: vt100::Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

fn trim_styled_cells(cells: &mut Vec<StyledTerminalCell>) {
    while cells
        .last()
        .is_some_and(|cell| cell.contents == " " && cell.bg == vt100::Color::Default)
    {
        cells.pop();
    }
}

fn styled_cells_to_plain(cells: &[StyledTerminalCell]) -> String {
    let mut text = String::new();
    for cell in cells {
        text.push_str(cell.contents.as_str());
    }
    text
}

impl TerminalPane {
    fn process_output_chunk(&mut self, chunk: &[u8]) {
        for byte in chunk {
            self.capture_top_origin_scroll_region_line_before(*byte);
            self.parser.process(&[*byte]);
            self.track_ansi_byte(*byte);
        }
        self.capture_alternate_snapshot();
    }

    fn capture_top_origin_scroll_region_line_before(&mut self, byte: u8) {
        if byte != b'\n' {
            return;
        }

        let Some((top, bottom)) = self.tracked_scroll_region else {
            return;
        };
        if top != 0 || bottom >= self.size.rows.saturating_sub(1) {
            return;
        }

        let (cursor_row, _) = self.parser.screen().cursor_position();
        if cursor_row != bottom {
            return;
        }

        let cells = self.styled_screen_row(top);
        if cells.is_empty() {
            return;
        }
        self.push_region_scrollback(cells);
    }

    fn push_region_scrollback(&mut self, cells: Vec<StyledTerminalCell>) {
        self.region_scrollback.push(cells);
        if self.region_scrollback_offset > 0 {
            self.region_scrollback_offset = self
                .region_scrollback_offset
                .saturating_add(1)
                .min(self.region_scrollback.len());
        }

        if self.region_scrollback.len() > self.region_scrollback_limit {
            let overflow = self.region_scrollback.len() - self.region_scrollback_limit;
            self.region_scrollback.drain(0..overflow);
            self.region_scrollback_offset = self.region_scrollback_offset.saturating_sub(overflow);
        }
        self.invalidate_render_cache();
    }

    fn track_ansi_byte(&mut self, byte: u8) {
        let state = std::mem::replace(&mut self.ansi_state, AnsiTrackState::Ground);
        self.ansi_state = match state {
            AnsiTrackState::Ground if byte == 0x1b => AnsiTrackState::Escape,
            AnsiTrackState::Ground => AnsiTrackState::Ground,
            AnsiTrackState::Escape if byte == b'[' => AnsiTrackState::Csi(Vec::new()),
            AnsiTrackState::Escape if byte == 0x1b => AnsiTrackState::Escape,
            AnsiTrackState::Escape => AnsiTrackState::Ground,
            AnsiTrackState::Csi(mut bytes) => {
                bytes.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.apply_tracked_csi(&bytes);
                    AnsiTrackState::Ground
                } else {
                    AnsiTrackState::Csi(bytes)
                }
            }
        };
    }

    fn apply_tracked_csi(&mut self, bytes: &[u8]) {
        if bytes.last() != Some(&b'r') {
            return;
        }
        let params = &bytes[..bytes.len().saturating_sub(1)];
        if params.is_empty() {
            self.tracked_scroll_region = None;
            return;
        }
        if params
            .iter()
            .any(|byte| !byte.is_ascii_digit() && *byte != b';')
        {
            return;
        }

        let text = String::from_utf8_lossy(params);
        let mut parts = text.split(';');
        let top = parts
            .next()
            .and_then(|part| part.parse::<u16>().ok())
            .unwrap_or(1);
        let bottom = parts
            .next()
            .and_then(|part| part.parse::<u16>().ok())
            .unwrap_or(self.size.rows);
        if top == 0 || bottom == 0 || top > bottom {
            return;
        }

        let top = top.saturating_sub(1).min(self.size.rows.saturating_sub(1));
        let bottom = bottom
            .saturating_sub(1)
            .min(self.size.rows.saturating_sub(1));
        self.tracked_scroll_region = Some((top, bottom));
    }

    fn capture_alternate_snapshot(&mut self) {
        if !self.parser.screen().alternate_screen() {
            self.alternate_history_offset = 0;
            self.last_alternate_snapshot.clear();
            return;
        }

        let snapshot = self.current_visible_lines_snapshot();
        if snapshot == self.last_alternate_snapshot {
            return;
        }

        self.last_alternate_snapshot = snapshot.clone();
        self.alternate_history.push(snapshot);
        self.invalidate_render_cache();
        if self.alternate_history.len() > self.alternate_history_limit {
            let overflow = self.alternate_history.len() - self.alternate_history_limit;
            self.alternate_history.drain(0..overflow);
            self.alternate_history_offset = self.alternate_history_offset.saturating_sub(overflow);
            self.invalidate_render_cache();
        }
    }

    fn alternate_history_snapshot(&self) -> Option<&Vec<String>> {
        if !self.parser.screen().alternate_screen() || self.alternate_history_offset == 0 {
            return None;
        }
        let index = self
            .alternate_history
            .len()
            .checked_sub(1 + self.alternate_history_offset)?;
        self.alternate_history.get(index)
    }

    /// Drop the materialized styled-cell grid so the next snapshot rebuilds it.
    /// Called on every path that changes what the screen shows; also public so
    /// benchmarks can measure a cold rebuild without faking terminal output.
    pub fn invalidate_render_cache(&mut self) {
        self.styled_lines_cache = None;
    }

    fn append_output_log(&mut self, chunk: &[u8]) {
        self.output_log.push_str(&String::from_utf8_lossy(chunk));
        if self.output_log.len() > self.output_log_limit {
            let overflow = self.output_log.len() - self.output_log_limit;
            let drain_to = self
                .output_log
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= overflow)
                .unwrap_or(overflow);
            self.output_log.drain(..drain_to);
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        self.terminate_child();
        // Join the reader thread instead of detaching it. Once the child and
        // all of its descendants are gone the slave PTY is fully closed, so the
        // blocking `reader.read` returns EOF/EIO and the loop exits promptly.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

/// A shell reports a signal death as 128 + signal number, so an agent killed by
/// SIGTERM surfaces as the bare number 143 — which tells the reader nothing about
/// what happened, and in particular does not say that the agent was KILLED rather
/// than that its work failed. Name the signal so the death note is diagnosable.
pub fn describe_exit_code(code: u32) -> String {
    let signal = match code {
        130 => Some("SIGINT"),
        137 => Some("SIGKILL"),
        139 => Some("SIGSEGV"),
        143 => Some("SIGTERM"),
        _ => None,
    };
    match signal {
        // SIGKILL is overwhelmingly the OOM killer on a dev machine; SIGTERM is a
        // deliberate stop, from Rudder's own teardown, a system shutdown/sleep, or
        // a manual kill. Both mean the work was interrupted, not that it errored.
        Some("SIGKILL") => "killed (SIGKILL) - usually the OS out-of-memory killer".to_string(),
        Some("SIGTERM") => {
            "terminated (SIGTERM) - stopped by Rudder, a system shutdown, or a manual kill"
                .to_string()
        }
        Some(name) => format!("killed by {name}"),
        None => format!("exit {code}"),
    }
}

impl TerminalPane {
    /// Tear down the spawned agent and everything it spawned, then reap it.
    ///
    /// `portable-pty` runs `setsid()` in the child's `pre_exec`, so the child
    /// is a session/process-group leader and its pgid equals its pid. Signaling
    /// the negative pgid hits the agent *and* its descendants (tool shells, MCP
    /// servers, `node`), which otherwise orphan and keep holding the jj/git
    /// workspace and the slave PTY open. We must also `wait()` the direct child
    /// or it lingers as a zombie for the lifetime of the process.
    fn terminate_child(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.process_id() {
                if pid > 1 {
                    let pgid = pid as libc::pid_t;
                    // SAFETY: a plain kill(2) on a process-group id; no memory
                    // is touched. Errors (group already gone) are ignored.
                    unsafe {
                        libc::kill(-pgid, libc::SIGTERM);
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command_builder(command: Option<TerminalCommand>, term: &str) -> Result<CommandBuilder> {
    match command {
        Some(command) => {
            if command.program.is_empty() {
                return Err(anyhow!("command program must not be empty"));
            }

            let mut builder = CommandBuilder::new(command.program);
            builder.args(command.args);
            for (key, value) in command.env {
                if is_term_env_key(&key) {
                    continue;
                }
                builder.env(key, value);
            }
            builder.env("TERM", term);
            Ok(builder)
        }
        None => {
            let shell = default_shell();
            if shell.is_empty() {
                return Err(anyhow!("could not determine default shell"));
            }
            let mut builder = CommandBuilder::new(shell);
            builder.env("TERM", term);
            Ok(builder)
        }
    }
}

#[cfg(windows)]
fn is_term_env_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("TERM")
}

#[cfg(not(windows))]
fn is_term_env_key(key: &str) -> bool {
    key == "TERM"
}

#[cfg(windows)]
fn default_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}

#[cfg(not(windows))]
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_rejects_zero_dimensions() {
        assert!(TerminalSize::new(0, 80).is_err());
        assert!(TerminalSize::new(24, 0).is_err());
    }

    #[test]
    fn visible_lines_are_plain_text_after_vt100_sequences() {
        let mut pane_buffer = vt100::Parser::new(2, 10, 10);
        pane_buffer.process(b"\x1b[31mred\x1b[0m\r\nplain");

        let lines: Vec<_> = pane_buffer
            .screen()
            .rows(0, 10)
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect();

        assert_eq!(lines, vec!["red", "plain"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn output_waker_fires_when_child_produces_output() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let woken = Arc::new(AtomicBool::new(false));
        // Delay the child's first output so the waker is reliably installed before
        // any bytes arrive (otherwise a fast `echo` could send + check the waker
        // slot while it is still None, and the test would flake).
        let pane = TerminalPane::spawn_shell_or_command(
            Some(TerminalCommand::with_args(
                "/bin/sh",
                ["-lc", "sleep 0.2; echo hi"],
            )),
            TerminalPaneOptions::default(),
        )
        .expect("spawn test pty");

        let woken_for_waker = Arc::clone(&woken);
        let waker: PtyOutputWaker = Arc::new(move || {
            woken_for_waker.store(true, Ordering::SeqCst);
        });
        pane.set_output_waker(waker);

        // The reader thread should fire the waker once the child's output arrives.
        let mut fired = false;
        for _ in 0..100 {
            if woken.load(Ordering::SeqCst) {
                fired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(fired, "waker was not invoked after child produced output");
    }

    #[test]
    fn term_env_key_matching_uses_platform_case_rules() {
        assert!(is_term_env_key("TERM"));

        #[cfg(windows)]
        {
            assert!(is_term_env_key("term"));
            assert!(is_term_env_key("Term"));
        }

        #[cfg(not(windows))]
        {
            assert!(!is_term_env_key("term"));
            assert!(!is_term_env_key("Term"));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn top_origin_scroll_region_history_scrolls_like_terminal_scrollback() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf '\\033[1;3r\\033[3;1H\\r\\nhistory001\\r\\nhistory002\\r\\nhistory003\\r\\nhistory004\\r\\nhistory005\\033[r'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("history005")
            {
                break;
            }
        }

        assert_eq!(pane.scrollback(), 0);
        pane.scrollback_by(2);
        let output = pane.visible_lines_snapshot().join("\n");
        assert!(output.contains("history001"), "output was {output:?}");
        assert!(output.contains("history005"), "output was {output:?}");
    }

    #[cfg(not(windows))]
    #[test]
    fn live_screen_only_renders_just_the_current_screen_no_scrollback() {
        // Emulate Claude Code's inline redraws: print far more lines than the screen
        // holds. A normal pane accumulates them in region_scrollback (which is what
        // stacked the duplicate welcome banners); a live_screen_only pane shows ONLY
        // the current screen, so the pane stays clean.
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "for i in $(seq 1 40); do printf 'line%02d\\r\\n' \"$i\"; done; printf 'TAIL'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                live_screen_only: true,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("TAIL") {
                break;
            }
        }

        // Only the live screen: exactly `rows` lines, no accumulated history.
        let lines = pane.visible_lines_snapshot();
        assert_eq!(
            lines.len(),
            5,
            "live screen is exactly the row count: {lines:?}"
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("TAIL"),
            "shows the latest screen: {joined:?}"
        );
        assert!(
            !joined.contains("line01"),
            "early lines scrolled off, not stacked: {joined:?}"
        );
        // Scrolling back is a no-op: there is no captured history to reveal.
        assert_eq!(pane.scrollback(), 0);
        pane.scrollback_by(10);
        assert!(
            !pane.visible_lines_snapshot().join("\n").contains("line01"),
            "live_screen_only never exposes scrollback history"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn pty_child_normalizes_term_without_dropping_case_distinct_env_on_unix() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf 'term=%s\\r\\nTerm=%s\\r\\nTERM=%s\\r\\n' \"$term\" \"$Term\" \"$TERM\"; sleep 1",
            ],
        )
        .with_env("term", "lowercase")
        .with_env("Term", "mixedcase")
        .with_env("TERM", "xterm-kitty");
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 40 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("TERM=xterm-256color")
            {
                break;
            }
        }

        let output = pane.visible_lines_snapshot().join("\n");
        assert!(output.contains("term=lowercase"), "output was {output:?}");
        assert!(output.contains("Term=mixedcase"), "output was {output:?}");
        assert!(
            output.contains("TERM=xterm-256color"),
            "output was {output:?}"
        );
        assert!(
            !output.contains("term=xterm-kitty"),
            "output was {output:?}"
        );
    }
}
