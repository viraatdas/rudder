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
        // Capture the run's token usage from the backend's own session log at
        // the moment the turn completes (interactive PTYs expose no usage
        // stream). Persisted to run.json as `tokens` by save_native_run_record
        // so telemetry has a cost signal; without this native runs report zero.
        refresh_run_token_usage(run);
        run.needs_permission = false;
        run.needs_user_input = false;
        // The ONLY ping: a task entering review. Done == the review bucket, and this
        // branch fires exactly once per Running->Done transition (selection/repaint
        // never reaches here), so it rings once per agent. A genuine reopen->recomplete
        // re-enters review — "a new thing" — and rings again, which is intended.
        // Pinned planners (orchestrator / plan-mode front-end) are excluded: their
        // completion is a planning transition, not work awaiting your review.
        if !run.is_pinned_planner() {
            play_completion_sound();
        }
        // Reflect the transition in the workspace's jj description so `jj log`
        // (and any agent reading it) sees this node reach review. Best-effort,
        // jj runs only; skipped for the main agent and in tests (no workspace).
        describe_workspace_status(run, "done");
    }
}

pub(crate) fn terminal_looks_ready_for_input_from_lines(
    backend: Backend,
    lines: &[String],
) -> bool {
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
        // opencode's idle footer: the status/help hints it draws under the composer.
        Backend::Opencode => {
            lower.contains("/help")
                || lower.contains("ctrl+")
                || lower.contains("tab ")
                || lower.contains("· opencode")
        }
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
            // Status bar pattern: "<model> <effort> ... · <cwd>". Anchored to the
            // bar SHAPE so ordinary output containing " · " and a slash (e.g.
            // "foo · bar /usr/lib") cannot masquerade as the idle status bar — a
            // false "ready" there marks a working Codex agent done mid-turn.
            looks_like_codex_status_bar(&lower)
        }
    }
}

/// True when `lower` is Codex's bottom status bar, which ends with the cwd after a
/// trailing " · " separator. We require that final segment to be a single path-like
/// token (starts with `~/` or `/`, no embedded spaces) so it matches the real bar
/// and not arbitrary prose that merely contains " · " and a slash somewhere.
fn looks_like_codex_status_bar(lower: &str) -> bool {
    if lower.contains("interrupt") {
        return false;
    }
    let Some((_, tail)) = lower.rsplit_once(" \u{00b7} ") else {
        return false;
    };
    let tail = tail.trim();
    (tail.starts_with("~/") || tail.starts_with('/')) && !tail.contains(' ')
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
    // The cursor sits at the bottom of a real prompt, so the trailing rows are
    // blank. When they are NOT (and the pane is taller than the recent window),
    // there is content below the last non-empty line: the agent is emitting
    // output, not waiting. Only an explicit numbered selection menu overrides this.
    let cursor_near_bottom = tail_blanks > 0 || lines.len() <= 6;
    if !cursor_near_bottom && !has_numbered_menu_pattern(&recent_rev) {
        return false;
    }

    // A trailing '?' is a question prompt ONLY when the cursor is idle at the
    // bottom. Previously any line ending in '?' returned true, so ordinary
    // mid-turn prose ("So what does this do?") spuriously flipped the worker into
    // the amber needs-input state. Gating on `cursor_near_bottom` keeps real
    // questions (which end at an idle prompt) while dropping rhetorical ones.
    if cursor_near_bottom && last.trim_end().ends_with('?') {
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
        || lower.contains("working...")
        || lower.contains("running...")
        // Spinner timers like "Thinking (15s)" / "Working (3s)": the marker is
        // immediately followed by an elapsed-time DIGIT. Requiring the digit avoids
        // matching prose such as "running (CI)" or "thinking (about X)", which would
        // otherwise hold a genuinely-idle worker out of completion detection.
        || busy_timer_after(&lower, "thinking (")
        || busy_timer_after(&lower, "working (")
        || busy_timer_after(&lower, "running (")
}

/// True when `marker` (e.g. "thinking (") appears in `lower` immediately followed
/// by a digit — the shape of a spinner's elapsed-time readout, not free prose.
fn busy_timer_after(lower: &str, marker: &str) -> bool {
    lower.split(marker).skip(1).any(|rest| {
        rest.trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
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
        Backend::Opencode => {
            line == ">"
                || line.starts_with("> ")
                || line.starts_with("❯ ")
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
