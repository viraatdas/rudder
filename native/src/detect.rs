#![allow(unused_imports)]
//! Heuristics that read worker output to detect idle/permission/prompt states.
use super::*;

/// Whether post-completion output should re-open a finished agent as a NEW turn.
/// True only when the user sent input (a keystroke) at/after the agent completed;
/// output with no input since completion is a repaint (e.g. the resize when you
/// highlight a finished agent) and must NOT flip done -> in-progress.
pub(crate) fn post_completion_output_is_new_turn(
    last_input_at: Option<Instant>,
    completed_at: Option<Instant>,
) -> bool {
    last_input_at.is_some_and(|input| completed_at.is_none_or(|done| input >= done))
}

/// True if the recent visible lines show a busy spinner ("esc to interrupt" etc.).
/// Used as a backstop to reopen an agent that was mis-flagged done while still
/// working: a live spinner is unambiguous proof the turn never actually ended,
/// whereas an idle resize/repaint shows no spinner.
pub(crate) fn recent_lines_look_busy(lines: &[String]) -> bool {
    lines
        .iter()
        .rev()
        .take(14)
        .any(|line| looks_busy(line.as_str()))
}

pub(crate) fn mark_run_done(run: &mut AgentRun) {
    if run.status != AgentStatus::Done {
        run.status = AgentStatus::Done;
        run.completed_at = Some(Instant::now());
        run.needs_permission = false;
        run.permission_notified = false;
        run.needs_user_input = false;
        run.user_input_notified = false;
        // The ONLY ping: a task entering review. Done == the review bucket, and this
        // branch fires exactly once per Running->Done transition (selection/repaint
        // never reaches here), so it rings once per agent. A genuine reopen->recomplete
        // re-enters review — "a new thing" — and rings again, which is intended.
        // Pinned planners (orchestrator / plan-mode front-end) are excluded: their
        // completion is a planning transition, not work awaiting your review.
        if !run.is_pinned_planner() {
            play_completion_sound();
        }
    }
}

pub(crate) fn terminal_looks_ready_for_input_from_lines(backend: Backend, lines: &[String]) -> bool {
    if terminal_needs_permission_from_lines(lines) {
        return false;
    }
    if terminal_needs_user_input_from_lines(lines) {
        // Waiting on a question, not "done".
        return false;
    }

    let recent = lines
        .iter()
        .rev()
        .take(12)
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if recent.iter().any(|line| looks_busy(line)) {
        return false;
    }

    // Strongest signal: the agent's idle chrome footer is visible. That footer
    // is only rendered while the agent is sitting at its prompt waiting for
    // input, so it never appears mid-turn. Falls back to the prompt-char
    // heuristic if we don't see chrome (older claude versions, raw shells).
    if recent
        .iter()
        .any(|line| looks_like_idle_chrome(backend, line))
    {
        return true;
    }

    recent
        .iter()
        .any(|line| looks_like_agent_prompt(backend, line))
}

/// Returns true if the given line looks like static footer/chrome text that
/// the agent only renders while idle at its prompt.
pub(crate) fn looks_like_idle_chrome(backend: Backend, line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let common = [
        "shift+tab to cycle",
        "shift+tab for plan",
        "bypass permissions on",
        "bypass permissions off",
        "/help for commands",
        "tab to switch agent",
        "press up to edit",
        "esc to clear",
    ];
    if common.iter().any(|c| lower.contains(c)) {
        return true;
    }
    match backend {
        Backend::Claude => lower.contains("(shift+tab"),
        Backend::Codex => {
            // Codex idle markers seen in the wild:
            //   "Worked for 4m 46s"        - turn-end summary line
            //   "/ps to view"              - background-jobs hint
            //   "/stop to close"           - same hint, separate token
            //   "/ for commands"           - slash-help hint
            //   "ctrl+j newline"           - input-area hint
            //   "<model> <effort> ... · <cwd>"  - bottom status bar
            if lower.contains("worked for ")
                || lower.contains("/ps to view")
                || lower.contains("/stop to close")
                || lower.contains("/ for commands")
                || lower.contains("ctrl+j newline")
            {
                return true;
            }
            // Status bar pattern: contains " · " AND a path token starting with
            // "~/" or "/", but is NOT a busy "interrupt" line.
            !lower.contains("interrupt")
                && lower.contains(" \u{00b7} ")
                && (lower.contains(" ~/") || lower.contains(" /"))
        }
    }
}

pub(crate) fn terminal_needs_user_input_from_lines(lines: &[String]) -> bool {
    // Collect the most recent non-empty normalized lines (top of list = most recent).
    let recent_rev = lines
        .iter()
        .rev()
        .map(|line| normalize_terminal_line(line))
        .filter(|line| !line.is_empty())
        .take(6)
        .collect::<Vec<_>>();
    if recent_rev.is_empty() {
        return false;
    }

    let last = recent_rev[0].as_str();
    // Guard against false positives from chatty multi-paragraph output: the
    // closing line should be short.
    if last.chars().count() > 120 {
        return false;
    }

    // Suppress when the agent is clearly mid-work.
    if recent_rev.iter().any(|line| looks_busy(line)) {
        return false;
    }

    // Tail cursor heuristic: most of the trailing screen rows must be empty.
    // (visible_lines_snapshot returns the whole pane; if the cursor sits at
    // the bottom of the screen, the last rows will be blank.)
    let tail_blanks = lines
        .iter()
        .rev()
        .take(3)
        .filter(|line| normalize_terminal_line(line).is_empty())
        .count();
    if tail_blanks == 0 && lines.len() > 6 {
        // Cursor isn't near the bottom — likely just chatty output, not a prompt.
        // Still allow it through if the last line clearly ends in '?'.
        if !last.trim_end().ends_with('?') {
            // Also allow numbered menu pattern in the last 6 rows.
            if !has_numbered_menu_pattern(&recent_rev) {
                return false;
            }
        }
    }

    if last.trim_end().ends_with('?') {
        return true;
    }

    let lower = last.to_ascii_lowercase();
    let cues = [
        "what would you like",
        "how should i",
        "which would you",
        "can you clarify",
        "please confirm",
        "choose one",
        "select",
    ];
    if cues.iter().any(|cue| lower.contains(cue)) {
        return true;
    }

    if has_numbered_menu_pattern(&recent_rev) {
        return true;
    }

    false
}

pub(crate) fn has_numbered_menu_pattern(recent_rev: &[String]) -> bool {
    // recent_rev holds the most recent non-empty lines (most recent first).
    // Require at least two DISTINCT numeric options (1 and 2, or 1 and 3, etc.)
    // so a real numbered list inside agent output doesn't trip us. Also accept
    // a leading "❯ N." (cursor) plus another N. as a strong selection signal.
    let mut seen_indices = std::collections::HashSet::new();
    let mut saw_cursor_option = false;
    for line in recent_rev.iter().take(8) {
        let stripped = line
            .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{276f}' || c == '\u{25b8}');
        if line.starts_with("\u{276f} ") || line.starts_with("\u{25b8} ") {
            saw_cursor_option = true;
        }
        for n in 1u8..=9 {
            let prefix_dot = format!("{n}.");
            let prefix_paren = format!("{n})");
            if stripped.starts_with(&prefix_dot) || stripped.starts_with(&prefix_paren) {
                seen_indices.insert(n);
            }
        }
    }
    seen_indices.len() >= 2 || (saw_cursor_option && !seen_indices.is_empty())
}

pub(crate) fn terminal_needs_permission_from_lines(lines: &[String]) -> bool {
    let recent = lines
        .iter()
        .rev()
        .take(14)
        .map(|line| normalize_terminal_line(line))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if recent.is_empty() {
        return false;
    }
    // Strong signal: agent's yes/no permission menu shape. Claude and codex
    // both render permission prompts as a numbered selection list ending in
    // "Yes" / "No" options, often with a "(esc)" hint. This is rock-solid
    // when present and language-agnostic to natural-language keyword soup.
    if looks_like_yes_no_menu(&recent) {
        return true;
    }
    let text = recent.iter().rev().cloned().collect::<Vec<_>>().join("\n");

    permission_text_needs_attention(&text)
}

/// True if the most recent lines look like an agent's yes/no permission menu:
/// a leading "❯ 1. Yes" with a follow-on "2. No..." nearby.
pub(crate) fn looks_like_yes_no_menu(recent_rev: &[String]) -> bool {
    let mut saw_yes_option = false;
    let mut saw_no_option = false;
    for line in recent_rev.iter().take(8) {
        let lower = line.to_ascii_lowercase();
        let stripped = lower.trim_start_matches(|c: char| {
            c.is_whitespace() || c == '\u{276f}' || c == '>' || c == '*'
        });
        if (stripped.starts_with("1.") || stripped.starts_with("1)")) && stripped.contains("yes") {
            saw_yes_option = true;
        }
        if (stripped.starts_with("2.") || stripped.starts_with("2)"))
            && (stripped.contains("no")
                || stripped.contains("don't")
                || stripped.contains("do not"))
        {
            saw_no_option = true;
        }
    }
    saw_yes_option && saw_no_option
}

pub(crate) fn permission_text_needs_attention(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let text = text.as_str();

    let has_permission_word = contains_any_word(
        &text,
        &[
            "permission",
            "approval",
            "approve",
            "allow",
            "authorize",
            "authorization",
            "confirmation",
            "proceed",
            "deny",
        ],
    );
    if !has_permission_word {
        return false;
    }

    let asks_decision =
        contains_any_phrase(&text, &["do you want", "would you like", "are you sure"])
            && contains_any_word(
                &text,
                &["allow", "approve", "run", "execute", "continue", "proceed"],
            );
    let approves_action = contains_any_word(&text, &["allow", "approve", "authorize"])
        && contains_any_word(
            &text,
            &[
                "command",
                "tool",
                "edit",
                "write",
                "file",
                "access",
                "execution",
                "network",
                "shell",
                "operation",
            ],
        );
    let approval_request = contains_any_word(
        &text,
        &["permission", "approval", "authorization", "confirmation"],
    ) && contains_any_word(
        &text,
        &[
            "required",
            "needed",
            "need",
            "request",
            "requested",
            "requesting",
            "waiting",
            "prompt",
        ],
    ) && contains_approval_response(&text);
    let key_prompt = contains_word(&text, "press")
        && contains_any_word(&text, &["y", "yes", "enter", "return"])
        && contains_any_word(&text, &["allow", "approve", "continue", "proceed"]);
    let yes_no_prompt = (contains_word(&text, "yes") || contains_word(&text, "no"))
        && contains_any_word(&text, &["approve", "deny", "allow"]);

    asks_decision || approves_action || approval_request || key_prompt || yes_no_prompt
}

pub(crate) fn looks_busy(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // Be specific about what "busy" looks like to avoid false positives on
    // status lines that incidentally contain words like "running" (e.g.
    // codex's "1 background terminal running · /ps to view"). The agents'
    // actual busy spinners always advertise an interrupt key.
    lower.contains("esc to interrupt")
        || lower.contains("ctrl-c to interrupt")
        || lower.contains("ctrl+c to interrupt")
        || lower.contains("thinking...")
        || lower.contains("thinking (")
        || lower.contains("working...")
        || lower.contains("working (")
        || lower.contains("running...")
        || lower.contains("running (")
}

pub(crate) fn looks_like_agent_prompt(backend: Backend, line: &str) -> bool {
    match backend {
        Backend::Claude => {
            line == ">"
                || line.starts_with("> ")
                || line.starts_with("❯ ")
                || line.starts_with("› ")
                || line.contains("Type a message")
        }
        Backend::Codex => {
            line == "›"
                || line.starts_with("› ")
                || line.starts_with("> ")
                || line.contains("Type a message")
        }
    }
}

pub(crate) fn normalize_terminal_line(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn contains_any_phrase(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

pub(crate) fn contains_any_word(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| contains_word(text, word))
}

pub(crate) fn contains_approval_response(text: &str) -> bool {
    contains_any_word(
        text,
        &["approve", "allow", "deny", "enter", "return", "press"],
    ) || contains_word(text, "yes")
        || contains_word(text, "no")
}

pub(crate) fn contains_word(text: &str, word: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == word)
}

