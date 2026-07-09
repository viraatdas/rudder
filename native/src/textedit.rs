#![allow(unused_imports)]
//! Task/worker input editing: prompt drafts, history nav, and cursor/word/char ops.
use super::*;

pub(crate) fn record_agent_prompt(run: &mut AgentRun, prompt: String, source: &str) {
    let ts = now_stamp();
    if source == "user" {
        run.last_user_input_at = ts.clone();
    }
    run.current_prompt = prompt.clone();
    run.turns.push(AgentTurn {
        ts,
        prompt,
        source: source.to_string(),
    });
}

pub(crate) fn update_worker_prompt_draft_for_key(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
    key: KeyEvent,
    capture_as_prompt: bool,
) -> Option<String> {
    match key.code {
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            return finish_worker_prompt_draft(draft, cursor, is_prompt);
        }
        KeyCode::Char('u') | KeyCode::Char('U')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            draft.clear();
            *cursor = 0;
            *is_prompt = false;
            return None;
        }
        KeyCode::Char('w') | KeyCode::Char('W')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            delete_previous_word_at(draft, cursor);
            return None;
        }
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                draft.clear();
                *cursor = 0;
                *is_prompt = false;
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                delete_previous_word_at(draft, cursor);
            } else {
                delete_char_before_cursor(draft, cursor);
            }
        }
        KeyCode::Delete => delete_char_at_cursor(draft, *cursor),
        KeyCode::Left => {
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                *cursor = previous_word_position(draft, *cursor);
            } else {
                *cursor = (*cursor).saturating_sub(1);
            }
        }
        KeyCode::Right => {
            let len = draft.chars().count();
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                *cursor = next_word_position(draft, *cursor);
            } else {
                *cursor = (*cursor + 1).min(len);
            }
        }
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = draft.chars().count(),
        KeyCode::Char(ch)
            if !key.modifiers.intersects(
                KeyModifiers::ALT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::SUPER
                    | KeyModifiers::META,
            ) =>
        {
            if draft.is_empty() {
                *is_prompt = capture_as_prompt;
            }
            insert_char_at_cursor(draft, cursor, ch);
        }
        _ => {}
    }

    None
}

pub(crate) fn update_worker_prompt_draft_for_paste(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
    text: &str,
    capture_as_prompt: bool,
) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut previous_was_carriage_return = false;

    for ch in text.chars() {
        match ch {
            '\r' => {
                if let Some(prompt) = finish_worker_prompt_draft(draft, cursor, is_prompt) {
                    prompts.push(prompt);
                }
                previous_was_carriage_return = true;
            }
            '\n' if previous_was_carriage_return => {
                previous_was_carriage_return = false;
            }
            '\n' => {
                if let Some(prompt) = finish_worker_prompt_draft(draft, cursor, is_prompt) {
                    prompts.push(prompt);
                }
                previous_was_carriage_return = false;
            }
            '\u{7f}' | '\u{8}' => {
                delete_char_before_cursor(draft, cursor);
                previous_was_carriage_return = false;
            }
            _ if ch.is_control() => {
                previous_was_carriage_return = false;
            }
            _ => {
                if draft.is_empty() {
                    *is_prompt = capture_as_prompt;
                }
                insert_char_at_cursor(draft, cursor, ch);
                previous_was_carriage_return = false;
            }
        }
    }

    prompts
}

pub(crate) fn finish_worker_prompt_draft(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
) -> Option<String> {
    let prompt = draft.trim().to_string();
    let should_record = *is_prompt;
    draft.clear();
    *cursor = 0;
    *is_prompt = false;
    if prompt.is_empty() || !should_record {
        None
    } else {
        Some(prompt)
    }
}

pub(crate) fn previous_task_history_entry(
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut String,
    current_input: &str,
) -> Option<String> {
    if history.is_empty() {
        return None;
    }

    let next_index = match *index {
        Some(current) => current
            .min(history.len().saturating_sub(1))
            .saturating_sub(1),
        None => {
            *draft = current_input.to_string();
            history.len().saturating_sub(1)
        }
    };
    *index = Some(next_index);
    history.get(next_index).cloned()
}

pub(crate) fn next_task_history_entry(
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut String,
) -> Option<String> {
    let current = (*index)?.min(history.len().saturating_sub(1));
    if current + 1 < history.len() {
        let next_index = current + 1;
        *index = Some(next_index);
        return history.get(next_index).cloned();
    }

    *index = None;
    Some(std::mem::take(draft))
}

pub(crate) fn insert_char_at_cursor(input: &mut String, cursor: &mut usize, ch: char) {
    let byte_index = byte_index_for_char(input, *cursor);
    input.insert(byte_index, ch);
    *cursor += 1;
}

pub(crate) fn insert_str_at_cursor(input: &mut String, cursor: &mut usize, value: &str) {
    let byte_index = byte_index_for_char(input, *cursor);
    input.insert_str(byte_index, value);
    *cursor += value.chars().count();
}

pub(crate) fn delete_char_before_cursor(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = byte_index_for_char(input, cursor.saturating_sub(1));
    let end = byte_index_for_char(input, *cursor);
    input.replace_range(start..end, "");
    *cursor -= 1;
}

pub(crate) fn delete_char_at_cursor(input: &mut String, cursor: usize) {
    if cursor >= input.chars().count() {
        return;
    }
    let start = byte_index_for_char(input, cursor);
    let end = byte_index_for_char(input, cursor + 1);
    input.replace_range(start..end, "");
}

pub(crate) fn truncate_at_cursor(input: &mut String, cursor: usize) {
    let start = byte_index_for_char(input, cursor);
    input.truncate(start);
}

pub(crate) fn delete_previous_word_at(input: &mut String, cursor: &mut usize) {
    let start_char = previous_word_position(input, *cursor);
    if start_char == *cursor {
        return;
    }
    let start = byte_index_for_char(input, start_char);
    let end = byte_index_for_char(input, *cursor);
    input.replace_range(start..end, "");
    *cursor = start_char;
}

pub(crate) fn previous_word_position(input: &str, cursor: usize) -> usize {
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].is_whitespace() {
        index -= 1;
    }
    index
}

pub(crate) fn next_word_position(input: &str, cursor: usize) -> usize {
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    while index < chars.len() && !chars[index].is_whitespace() {
        index += 1;
    }
    index
}

pub(crate) fn byte_index_for_char(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(input.len())
}

/// One collapsed paste in the task draft: the full pasted `text`, remembered
/// behind a compact `placeholder` chip (e.g. "[Pasted #1 +60 lines]") that sits
/// inline in the input string. The chip is plain text in the draft, so it renders,
/// moves, and deletes like any other text; the chunk just lets us expand it back to
/// the real content when the user re-pastes (toggle) or submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PastedChunk {
    pub id: usize,
    pub text: String,
    pub placeholder: String,
}

/// A pasted string is worth collapsing into a chip when it spans multiple lines or
/// is long. Short single-line pastes (a URL, a path, a word) insert verbatim so the
/// chip never gets in the way of ordinary editing.
pub(crate) fn paste_should_collapse(text: &str) -> bool {
    text.contains('\n') || text.chars().count() > 200
}

/// The chip label shown in place of a collapsed paste. Multi-line pastes report
/// their line count ("+60 lines"); a long single-line paste reports characters
/// instead of a misleading "+1 lines".
pub(crate) fn paste_placeholder_label(id: usize, text: &str) -> String {
    let newlines = text.matches('\n').count();
    if newlines > 0 {
        let lines = newlines + 1;
        format!("[Pasted #{id} +{lines} lines]")
    } else {
        let chars = text.chars().count();
        format!("[Pasted #{id} +{chars} chars]")
    }
}

/// The char index at which `needle` first appears in `haystack`, or `None`.
fn find_char_index(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .find(needle)
        .map(|byte_index| haystack[..byte_index].chars().count())
}

/// Replace `len_chars` characters starting at char index `at` with `replacement`,
/// leaving the cursor at the end of the inserted text.
fn replace_span(
    input: &mut String,
    cursor: &mut usize,
    at: usize,
    len_chars: usize,
    replacement: &str,
) {
    let start = byte_index_for_char(input, at);
    let end = byte_index_for_char(input, at + len_chars);
    input.replace_range(start..end, replacement);
    *cursor = at + replacement.chars().count();
}

/// Apply a paste of `text` to the task draft with paste-collapsing:
/// - Non-collapsible text inserts verbatim at the cursor.
/// - New collapsible text becomes a "[Pasted #N …]" chip at the cursor; the full
///   text is remembered in `chunks`.
/// - Re-pasting text that already has a chip TOGGLES it in place: a collapsed chip
///   expands inline to the full text, and an already-expanded one re-collapses. If
///   the chip was edited away, the re-paste is treated as a brand-new paste.
pub(crate) fn apply_task_paste(
    input: &mut String,
    cursor: &mut usize,
    chunks: &mut Vec<PastedChunk>,
    text: &str,
) {
    // An empty draft means every prior chip is gone (cleared elsewhere), so start
    // fresh — this keeps chip numbering at #1 without hunting every draft-clear site.
    if input.is_empty() {
        chunks.clear();
    }
    if !paste_should_collapse(text) {
        insert_str_at_cursor(input, cursor, text);
        return;
    }
    if let Some(chunk) = chunks.iter().find(|chunk| chunk.text == text).cloned() {
        if let Some(at) = find_char_index(input, &chunk.placeholder) {
            // Collapsed → expand: swap the chip for the full text.
            replace_span(
                input,
                cursor,
                at,
                chunk.placeholder.chars().count(),
                &chunk.text,
            );
            return;
        }
        if let Some(at) = find_char_index(input, &chunk.text) {
            // Expanded → collapse: swap the full text back for the chip.
            replace_span(
                input,
                cursor,
                at,
                chunk.text.chars().count(),
                &chunk.placeholder,
            );
            return;
        }
        // Neither form is present — the chip was edited/deleted. Fall through and
        // insert a fresh chip for this paste.
    }
    let id = chunks.iter().map(|chunk| chunk.id).max().unwrap_or(0) + 1;
    let placeholder = paste_placeholder_label(id, text);
    chunks.push(PastedChunk {
        id,
        text: text.to_string(),
        placeholder: placeholder.clone(),
    });
    insert_str_at_cursor(input, cursor, &placeholder);
}

/// Expand every collapsed paste chip in `input` back to its full text, so a
/// submitted command/task carries the real content whether or not the user
/// expanded the chips on screen.
pub(crate) fn expand_pasted_chips(input: &str, chunks: &[PastedChunk]) -> String {
    let mut out = input.to_string();
    for chunk in chunks {
        if out.contains(&chunk.placeholder) {
            out = out.replace(&chunk.placeholder, &chunk.text);
        }
    }
    out
}
