#![allow(unused_imports)]
//! Mouse-to-coordinate mapping, text selection, clipboard, and terminal-cell styling.
use super::*;

pub(crate) fn selection_point_from_mouse(mouse: MouseEvent, area: Rect) -> SelectionPoint {
    SelectionPoint {
        row: mouse
            .row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1)) as usize,
        col: mouse
            .column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1)) as usize,
    }
}

pub(crate) fn task_selection_point_from_mouse(
    app: &App,
    mouse: MouseEvent,
    area: Rect,
) -> Option<SelectionPoint> {
    if !rect_contains(area, mouse.column, mouse.row) {
        return None;
    }
    let width = task_inner_width(area);
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, width);
    let input_start = task_visible_input_start(app, area, &input_lines);
    let visible_count = task_visible_input_count(app, area, input_lines.len());
    let rel_row = mouse.row.saturating_sub(area.y) as usize;
    if rel_row >= visible_count {
        return None;
    }
    let row = input_start.saturating_add(rel_row);
    if row >= input_lines.len() {
        return None;
    }
    Some(SelectionPoint {
        row,
        col: mouse
            .column
            .saturating_sub(area.x)
            .min(width.saturating_sub(1)) as usize,
    })
}

pub(crate) fn agent_index_from_mouse(app: &App, mouse: MouseEvent, area: Rect) -> Option<usize> {
    let inner = block_inner(area);
    if !rect_contains(inner, mouse.column, mouse.row) {
        return None;
    }
    // Resolve through the row map render_agents recorded for the last frame, so the
    // hit-test always matches the drawn layout. (The old path assumed agents start
    // at a fixed header offset and occupy fixed row counts; both drifted from the
    // renderer and clicks selected the wrong agent.)
    let row = mouse.row.saturating_sub(inner.y) as usize;
    app.agent_row_map.get(row).copied().flatten()
}

pub(crate) fn task_visible_input_start(app: &App, area: Rect, input_lines: &[String]) -> usize {
    let width = task_inner_width(area);
    let (cursor_line, _) = task_cursor_position(&app.task_input, app.task_cursor, width);
    let max_input_lines = task_visible_input_count(app, area, input_lines.len());
    let input_start = if input_lines.len() > max_input_lines {
        cursor_line.saturating_sub(max_input_lines.saturating_sub(1))
    } else {
        0
    };
    input_start.min(input_lines.len().saturating_sub(1))
}

pub(crate) fn task_visible_input_count(app: &App, area: Rect, input_line_count: usize) -> usize {
    // Keep legacy task-selection math aligned with task_pane_height.
    let default_hint = crate::render::task_default_hint(app);
    let hint = app.notice.as_deref().unwrap_or(default_hint);
    let hint_line_count = wrap_text(hint, task_inner_width(area)).len().max(1);
    (area.height.max(1) as usize)
        .saturating_sub(hint_line_count)
        .max(1)
        .min(input_line_count.max(1))
}

pub(crate) fn task_inner_width(area: Rect) -> u16 {
    area.width.max(1)
}

pub(crate) fn task_cursor_from_selection_point(
    value: &str,
    point: SelectionPoint,
    width: u16,
) -> usize {
    let lines = wrap_input_text(value, width);
    let mut cursor = 0;
    for (row, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if row == point.row {
            return cursor + point.col.min(line_len);
        }
        cursor += line_len;
    }
    value.chars().count()
}

pub(crate) fn normalize_selection(selection: WorkerSelection) -> NormalizedSelection {
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
    NormalizedSelection { start, end }
}

pub(crate) fn selection_is_empty(selection: NormalizedSelection) -> bool {
    selection.start == selection.end
}

pub(crate) fn selection_for_row(
    selection: Option<NormalizedSelection>,
    row: usize,
) -> Option<(usize, usize)> {
    let selection = selection?;
    if row < selection.start.row || row > selection.end.row {
        return None;
    }
    let start_col = if row == selection.start.row {
        selection.start.col
    } else {
        0
    };
    let end_col = if row == selection.end.row {
        selection.end.col
    } else {
        usize::MAX
    };
    Some((start_col, end_col))
}

pub(crate) fn selected_text_from_lines(lines: &[String], selection: WorkerSelection) -> String {
    let selection = normalize_selection(selection);
    let mut selected = Vec::new();
    for row in selection.start.row..=selection.end.row {
        let Some(line) = lines.get(row) else {
            continue;
        };
        let char_len = line.chars().count();
        let start = if row == selection.start.row {
            selection.start.col.min(char_len)
        } else {
            0
        };
        let end = if row == selection.end.row {
            selection.end.col.saturating_add(1).min(char_len)
        } else {
            char_len
        };
        selected.push(slice_chars(line, start, end));
    }
    selected.join("\n")
}

pub(crate) fn slice_chars(value: &str, start: usize, end: usize) -> String {
    value
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

pub(crate) fn copy_text_to_clipboard(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        return run_clipboard_command("pbcopy", &[], text);
    }

    #[cfg(target_os = "windows")]
    {
        return run_clipboard_command("clip", &[], text);
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let commands: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        for (command, args) in commands {
            if run_clipboard_command(command, args, text).is_ok() {
                return Ok(());
            }
        }
        bail!("no clipboard command found");
    }
}

pub(crate) fn run_clipboard_command(command: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("failed to write to {command}"))?;
    }
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {command}"))?;
    if !status.success() {
        bail!("{command} exited with {status}");
    }
    Ok(())
}

pub(crate) fn styled_terminal_line(
    cells: Vec<StyledTerminalCell>,
    selection: Option<(usize, usize)>,
    cursor_col: Option<usize>,
) -> Line<'static> {
    let column_count = cursor_col
        .map(|col| col.saturating_add(1))
        .unwrap_or(cells.len())
        .max(cells.len());
    let spans = (0..column_count)
        .map(|col| {
            let cell = cells
                .get(col)
                .cloned()
                .unwrap_or_else(|| plain_terminal_cell(" ".to_string()));
            let mut style = terminal_cell_style(&cell);
            if selection.is_some_and(|(start, end)| col >= start && col <= end) {
                // Light-theme text selection: a pale teal highlight with ink text,
                // not a heavy dark block.
                style = style.fg(INK).bg(SURFACE_SEL);
            }
            if cursor_col == Some(col) {
                style = cursor_cell_style();
            }
            Span::styled(cell.contents, style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// A solid block cursor for the light theme: paper text on a teal accent fill, so
/// it reads as a clear caret on white.
pub(crate) fn cursor_cell_style() -> Style {
    Style::default().fg(PAPER).bg(ACCENT)
}

pub(crate) fn plain_terminal_cell(contents: String) -> StyledTerminalCell {
    StyledTerminalCell {
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

pub(crate) fn styled_plain_line(
    text: &str,
    style: Style,
    selection: Option<(usize, usize)>,
) -> Line<'static> {
    let Some((start, end)) = selection else {
        return Line::from(Span::styled(text.to_string(), style));
    };
    let mut spans = Vec::new();
    let chars = text.chars().collect::<Vec<_>>();
    let clamped_start = start.min(chars.len());
    let clamped_end = end.saturating_add(1).min(chars.len());
    if clamped_start > 0 {
        spans.push(Span::styled(
            chars[..clamped_start].iter().collect::<String>(),
            style,
        ));
    }
    if clamped_end > clamped_start {
        spans.push(Span::styled(
            chars[clamped_start..clamped_end].iter().collect::<String>(),
            style.fg(INK).bg(SURFACE_SEL),
        ));
    }
    if clamped_end < chars.len() {
        spans.push(Span::styled(
            chars[clamped_end..].iter().collect::<String>(),
            style,
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
    }
    Line::from(spans)
}

pub(crate) fn terminal_cell_style(cell: &StyledTerminalCell) -> Style {
    let (fg, bg) = if cell.inverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    let mut style = app_style();
    if let Some(color) = map_vt100_color(fg) {
        style = style.fg(color);
    }
    if let Some(color) = map_vt100_color(bg) {
        style = style.bg(color);
    }
    let mut modifier = Modifier::empty();
    if cell.bold {
        modifier |= Modifier::BOLD;
    }
    if cell.dim {
        modifier |= Modifier::DIM;
    }
    if cell.italic {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline {
        modifier |= Modifier::UNDERLINED;
    }
    style.add_modifier(modifier)
}

pub(crate) fn map_vt100_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(match index {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::White,
            _ => Color::Indexed(index),
        }),
        vt100::Color::Rgb(red, green, blue) => Some(Color::Rgb(red, green, blue)),
    }
}
