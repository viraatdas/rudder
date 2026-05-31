#![allow(unused_imports)]
//! Encoding key and mouse events into terminal byte sequences.
use super::*;

pub(crate) fn is_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn terminal_bytes_for_key(key: KeyEvent) -> Option<Vec<u8>> {
    if is_copy_key(key) {
        return None;
    }

    let bytes = match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_char_bytes(ch)?
            } else if key.modifiers.contains(KeyModifiers::SUPER) {
                return None;
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                let mut bytes = vec![0x1b];
                bytes.extend(ch.to_string().into_bytes());
                bytes
            } else {
                ch.to_string().into_bytes()
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => b"\x1b[13;2u".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                vec![0x15]
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\x7f".to_vec()
            } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                vec![0x17]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => b"\x1bb".to_vec(),
        KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => b"\x1bf".to_vec(),
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => b"\x1b[1;5D".to_vec(),
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => b"\x1b[1;5C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => modified_arrow("A", key.modifiers),
        KeyCode::Down => modified_arrow("B", key.modifiers),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

pub(crate) fn bracketed_paste_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + "\x1b[200~\x1b[201~".len());
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub(crate) fn mouse_event_to_sgr(mouse: MouseEvent, area: Rect) -> Option<Vec<u8>> {
    if !rect_contains(area, mouse.column, mouse.row) {
        return None;
    }

    let x = mouse.column.saturating_sub(area.x).saturating_add(1);
    let y = mouse.row.saturating_sub(area.y).saturating_add(1);
    let (button, press) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), true),
        MouseEventKind::Up(_) => (3, false),
        MouseEventKind::Drag(button) => (mouse_button_code(button) + 32, true),
        MouseEventKind::Moved => (35, true),
        MouseEventKind::ScrollUp => (64, true),
        MouseEventKind::ScrollDown => (65, true),
        MouseEventKind::ScrollLeft => (66, true),
        MouseEventKind::ScrollRight => (67, true),
    };
    let button = button + mouse_modifier_code(mouse.modifiers);

    Some(
        format!(
            "\x1b[<{};{};{}{}",
            button,
            x,
            y,
            if press { "M" } else { "m" }
        )
        .into_bytes(),
    )
}

pub(crate) fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

pub(crate) fn mouse_modifier_code(modifiers: KeyModifiers) -> u16 {
    let mut code = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    code
}

pub(crate) fn control_char_bytes(ch: char) -> Option<Vec<u8>> {
    let lower = ch.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some(vec![(lower as u8) - b'a' + 1]);
    }
    match ch {
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

pub(crate) fn modified_arrow(final_byte: &str, modifiers: KeyModifiers) -> Vec<u8> {
    let modifier_code = if modifiers.contains(KeyModifiers::CONTROL) {
        Some(5)
    } else if modifiers.contains(KeyModifiers::ALT) {
        Some(3)
    } else {
        None
    };
    match modifier_code {
        Some(code) => format!("\x1b[1;{code}{final_byte}").into_bytes(),
        None => format!("\x1b[{final_byte}").into_bytes(),
    }
}
