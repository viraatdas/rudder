//! PTY-backed terminal pane abstraction for the native Rudder app.
//!
//! `TerminalPane` intentionally keeps the process/PTY plumbing separate from
//! the rest of the app. It currently uses `portable-pty` for cross-platform PTY
//! creation and `vt100` for a plain-text terminal screen buffer. The public API
//! is small enough that the buffer backend can be replaced with
//! `alacritty_terminal` later without forcing app layout code to change.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_SCROLLBACK_LINES: usize = 2_000;

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
}

impl Default for TerminalPaneOptions {
    fn default() -> Self {
        Self {
            size: TerminalSize::default(),
            cwd: None,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            term: "xterm-256color".to_string(),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AnsiTrackState {
    Ground,
    Escape,
    Csi(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledTerminalCell {
    pub contents: String,
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
                        }
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
        })
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
                    Some(format!("exit {code}"))
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
        if self.styled_lines_cache.is_none() {
            self.styled_lines_cache = Some(self.compute_styled_lines_snapshot());
        }
        self.styled_lines_cache.clone().unwrap_or_default()
    }

    fn compute_styled_lines_snapshot(&self) -> Vec<Vec<StyledTerminalCell>> {
        if let Some(snapshot) = self.alternate_history_snapshot() {
            return snapshot
                .iter()
                .map(|line| {
                    line.chars()
                        .map(|ch| StyledTerminalCell::plain(ch.to_string()))
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
                cell.contents().to_string()
            } else {
                " ".to_string()
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
        let screen = self.parser.screen();
        screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
    }

    pub fn uses_alternate_screen(&mut self) -> bool {
        self.drain_output();
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
}

impl StyledTerminalCell {
    fn plain(contents: String) -> Self {
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
        text.push_str(&cell.contents);
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

    fn invalidate_render_cache(&mut self) {
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
        let _ = self.child.kill();
        let _ = self.reader_thread.take();
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
        assert!(
            output.contains("term=lowercase"),
            "output was {output:?}"
        );
        assert!(
            output.contains("Term=mixedcase"),
            "output was {output:?}"
        );
        assert!(
            output.contains("TERM=xterm-256color"),
            "output was {output:?}"
        );
        assert!(!output.contains("term=xterm-kitty"), "output was {output:?}");
    }
}
