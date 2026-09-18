//! Message-by-message scrolling of a Claude Code worker's normal view, driven
//! by what Rudder sees on the worker's screen.
//!
//! Rudder can't compute message boundaries itself: Claude Code is a fullscreen
//! TUI on the alternate screen, so the pane has no scrollback to scan. Its
//! normal view does scroll (PageUp = half a page, mouse wheel = one line, and
//! Claude asks for SGR mouse reporting so Rudder can send wheel events), but
//! it has no "previous message" key, and its transcript pager's `{`/`}` step
//! by *prompt* — useless for a worker, which gets one prompt and then a long
//! turn of tool calls (that shipped: the first press jumped to the top and the
//! rest did nothing). Switching into that pager was also a mode change the
//! user didn't want.
//!
//! So Rudder walks the normal view the way a person would: page up, look at
//! what came into view, and if a message start (`⏺` or a `❯` prompt) that was
//! above the old top of the screen is now visible, wheel down one line at a
//! time until it sits at the top. If nothing came into view, page up again.
//! This module holds the byte constants and the pure screen math; the state
//! machine that sends the keys and waits for repaints lives in main.rs
//! (`ClaudeMessageNav`).
//!
//! Measured against claude 2.1.27x through Rudder's own pane: while scrolled,
//! Claude pins the current prompt as a sticky line on row 0 and shows a "Jump
//! to bottom" hint on the last content row; the first wheel event after a
//! change of direction is dropped; a message scrolled past the top slides
//! under the sticky line (it isn't blanked, unlike in the transcript pager).

/// Footer text Claude Code's fullscreen renderer shows while its transcript
/// view (Ctrl+O) is open. Alt+V never opens that view any more, but Alt+B
/// still reads this off the screen to close it with `q` first if the user
/// opened it by hand — Ctrl+End alone is a no-op in that view.
pub(crate) const CLAUDE_TRANSCRIPT_VIEW_MARKER: &str = "Showing detailed transcript";

/// PageUp: half a page up in Claude Code's normal view.
pub(crate) const CLAUDE_PAGE_UP_BYTES: &[u8] = b"\x1b[5~";

/// SGR mouse wheel up at cell 1;1: one line up in the normal view.
pub(crate) const CLAUDE_LINE_UP_BYTES: &[u8] = b"\x1b[<64;1;1M";

/// SGR mouse wheel down at cell 1;1: one line down in the normal view.
pub(crate) const CLAUDE_LINE_DOWN_BYTES: &[u8] = b"\x1b[<65;1;1M";

/// Alt+B from the normal view: Ctrl+End, which Claude Code binds to "jump to
/// the latest message and resume auto-follow". Sent twice: a single Ctrl+End
/// can land the renderer in a transitional state (cursor moves but auto-follow
/// doesn't fully re-engage) before a second one lands cleanly — sending both
/// from one keypress means you never have to notice.
pub(crate) const CLAUDE_JUMP_TO_LATEST_BYTES: &[u8] = b"\x1b[1;5F\x1b[1;5F";

/// Alt+B with the transcript view open: `q` closes it (Ctrl+End does NOT —
/// it's a no-op in that view), then the same doubled Ctrl+End as above.
pub(crate) const CLAUDE_TRANSCRIPT_EXIT_TO_LATEST_BYTES: &[u8] = b"q\x1b[1;5F\x1b[1;5F";

/// Whether a row begins a message: an assistant/tool block (`⏺`, column 0)
/// or a user prompt (`❯`). Tool results (`  ⎿`), continuation lines and the
/// timestamp header above assistant text are all indented or unmarked and
/// don't count.
pub(crate) fn is_claude_message_start(line: &str) -> bool {
    line.starts_with('⏺') || line.starts_with("❯ ") || line.starts_with("❯\u{a0}")
}

/// Where a Claude Code screen's transcript ends and its own chrome begins:
/// the horizontal rule above the input box. Everything from there down is
/// the input box, the status bar and the token count — not transcript. The
/// empty input box reads `❯ `, which otherwise looks exactly like a user
/// message and made every screen appear to hold a message start.
pub(crate) fn claude_content_end(screen: &[String]) -> usize {
    let is_rule = |line: &String| {
        let trimmed = line.trim();
        trimmed.chars().filter(|c| *c == '─').count() >= 10
            && trimmed.chars().all(|c| c == '─' || c == ' ')
    };
    let last_rule = screen.iter().rposition(is_rule);
    let Some(last_rule) = last_rule else {
        return screen.len();
    };
    // The input box sits between two rules; step over it to the upper one.
    screen[..last_rule]
        .iter()
        .rposition(is_rule)
        .filter(|upper| last_rule - upper <= 4)
        .unwrap_or(last_rule)
}

/// Rows of `screen` that begin a message.
pub(crate) fn claude_message_start_rows(screen: &[String]) -> Vec<usize> {
    screen[..claude_content_end(screen)]
        .iter()
        .enumerate()
        .filter(|(_, line)| is_claude_message_start(line))
        .map(|(row, _)| row)
        .collect()
}

/// How many rows the view moved up between `before` and `after`: where the
/// content that was at the top of `before` now sits in `after`.
///
/// Message-start lines are matched by text first: each one visible in both
/// screens votes for the shift that maps its old row to its new row, and the
/// best-supported shift wins (ties go to the larger shift). A line that
/// didn't move (the sticky prompt on row 0) never votes. Falling back, a run
/// of three rows from the first non-blank one, then two — never one, since
/// blank separators recur all over a transcript. `None` when the old top
/// can't be found — the caller then treats the whole new screen as newly
/// revealed.
pub(crate) fn claude_page_shift(before: &[String], after: &[String]) -> Option<usize> {
    let mut votes: Vec<(usize, usize)> = Vec::new(); // (shift, support)
    for (old_row, line) in before.iter().enumerate() {
        if !is_claude_message_start(line) {
            continue;
        }
        for new_row in (old_row + 1..after.len()).filter(|&row| &after[row] == line) {
            let shift = new_row - old_row;
            match votes.iter_mut().find(|(s, _)| *s == shift) {
                Some((_, support)) => *support += 1,
                None => votes.push((shift, 1)),
            }
        }
    }
    if let Some((shift, _)) = votes.iter().max_by_key(|(shift, support)| (*support, *shift)) {
        return Some(*shift);
    }

    let first = before.iter().position(|line| !line.trim().is_empty())?;
    let available = &before[first..];
    [3usize, 2].iter().find_map(|&len| {
        if available.len() < len {
            return None;
        }
        let pattern = &available[..len];
        (first..after.len().saturating_sub(len - 1))
            .find(|&row| {
                pattern
                    .iter()
                    .enumerate()
                    .all(|(offset, line)| after.get(row + offset) == Some(line))
            })
            .map(|row| row - first)
    })
}

/// After a page up from `before` to `after`: the row in `after` of the nearest
/// message start that was above the old top of the screen — the one to scroll
/// to the top next — or `None` if nothing new begins a message on this page
/// (page up again).
///
/// "Above the old top" is row < shift, plus row == shift when the old top row
/// was NOT that message start — it was hidden under the sticky prompt line,
/// or rendered blank because it was cut off — so it's newly revealed all the
/// same. A row that reads the same as it did before the page-up (the sticky
/// prompt itself) is never chosen.
pub(crate) fn claude_previous_message_row(before: &[String], after: &[String]) -> Option<usize> {
    let shift = claude_page_shift(before, after);
    let revealed = shift.unwrap_or(after.len());
    claude_message_start_rows(after)
        .into_iter()
        .filter(|&row| row < revealed || (shift.is_some() && row == revealed))
        .filter(|&row| before.get(row) != Some(&after[row]))
        .filter(|&row| !(row == revealed && before.first() == Some(&after[row])))
        .max()
}

/// Whether a page-up actually moved the view: the top half of the screen
/// changed. Bottom rows churn on their own — the turn spinner and clock
/// ("✻ Cooked for 1s · done 12:41 AM"), the token count, the "Jump to
/// bottom" hint — so "any cell changed" is not evidence of a scroll (a walk
/// once sent 14 PageUps into an unmovable, fully visible chat because of it),
/// while even a one-line scroll changes the top half.
pub(crate) fn claude_view_moved(before: &[String], after: &[String]) -> bool {
    let rows = before.len().min(after.len());
    let top = (rows / 2).max(1);
    before[..top.min(before.len())] != after[..top.min(after.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    #[test]
    fn message_starts_are_bullets_and_prompts_only() {
        assert!(is_claude_message_start("⏺ It printed alpha."));
        assert!(is_claude_message_start("⏺ Bash(echo alpha)"));
        assert!(is_claude_message_start("❯ Run these as SEPARATE Bash tool calls"));
        assert!(!is_claude_message_start("  ⎿  alpha"));
        assert!(!is_claude_message_start("  which word it printed. Then say done."));
        assert!(!is_claude_message_start("                     01:37 PM claude-haiku-4-5"));
        assert!(!is_claude_message_start("✻ Sautéed for 9s · done 1:36 PM"));
        assert!(!is_claude_message_start(""));
    }

    #[test]
    fn page_shift_finds_the_old_top_further_down() {
        let before = screen(&["", "⏺ It printed echo.", "", "⏺ Bash(echo foxtrot)", "  ⎿  foxtrot"]);
        let after = screen(&["⏺ Bash(echo echo)", "  ⎿  echo", "", "⏺ It printed echo.", "", "⏺ Bash(echo foxtrot)"]);
        // "⏺ It printed echo." was row 1, now row 3: the view moved up 2.
        assert_eq!(claude_page_shift(&before, &after), Some(2));
    }

    #[test]
    fn page_shift_skips_blank_top_rows_and_needs_a_multi_row_match() {
        let before = screen(&["", "", "  body a", "  body b"]);
        let after = screen(&["  z", "  y", "", "", "  body a", "  body b"]);
        assert_eq!(claude_page_shift(&before, &after), Some(2));
        // A single body row is too ambiguous to match on its own.
        assert_eq!(claude_page_shift(&screen(&["", "  body a"]), &after), None);
        // No overlap at all.
        assert_eq!(claude_page_shift(&before, &screen(&["  q", "  x", "  y"])), None);
    }

    #[test]
    fn page_shift_ignores_the_sticky_prompt_that_never_moves() {
        // Captured from claude 2.1.276 through Rudder's pane: while scrolled,
        // the current prompt is pinned to row 0 of both screens. It must not
        // vote for "shift 0"; the real content pins shift 4.
        let before = screen(&[
            "❯ Run these as SEPARATE Bash tool calls", "⏺ Bash(echo echo)", "  ⎿  echo", "",
            "⏺ It printed echo.", "", "⏺ Bash(echo foxtrot)", "  ⎿  foxtrot", "", "⏺ It printed foxtrot.",
        ]);
        let after = screen(&[
            "❯ Run these as SEPARATE Bash tool calls", "  ⎿  delta", "", "⏺ It printed delta.", "",
            "⏺ Bash(echo echo)", "  ⎿  echo", "", "⏺ It printed echo.", "",
        ]);
        assert_eq!(claude_page_shift(&before, &after), Some(4));
        // Nearest newly revealed message start: "⏺ It printed delta." (row 3).
        // The sticky prompt on row 0 is unchanged and never a candidate.
        assert_eq!(claude_previous_message_row(&before, &after), Some(3));
    }

    #[test]
    fn previous_message_includes_the_row_hidden_under_the_sticky_prompt() {
        // Old row 0 was the sticky prompt; after paging up by 4 the message
        // that had been sitting under it ("⏺ Bash(echo delta)") is at row 4
        // — exactly the shift — and IS the nearest previous message.
        let before = screen(&[
            "❯ Run these", "  ⎿  delta", "", "⏺ It printed delta.", "", "⏺ Bash(echo echo)", "  ⎿  echo",
        ]);
        let after = screen(&[
            "❯ Run these", "", "⏺ It printed charlie.", "", "⏺ Bash(echo delta)", "  ⎿  delta", "",
            "⏺ It printed delta.",
        ]);
        assert_eq!(claude_page_shift(&before, &after), Some(4));
        assert_eq!(claude_previous_message_row(&before, &after), Some(4));
    }

    #[test]
    fn previous_message_ignores_starts_that_were_already_on_screen() {
        // A message start that was on the old top row (fully visible, not
        // under a sticky line) sits exactly at the shift boundary and must
        // not be chosen again.
        let before = screen(&["⏺ message 9", "  body 46", "  body 47"]);
        let after = screen(&["  body 44", "  body 45", "⏺ message 9", "  body 46", "  body 47"]);
        assert_eq!(claude_page_shift(&before, &after), Some(2));
        assert_eq!(claude_previous_message_row(&before, &after), None);
    }

    #[test]
    fn previous_message_treats_an_unmatched_page_as_all_new() {
        let before = screen(&["  body 90", "  body 91"]);
        let after = screen(&["⏺ message 1", "  body", "⏺ message 2", "  body"]);
        assert_eq!(claude_previous_message_row(&before, &after), Some(2));
    }

    #[test]
    fn content_ends_above_the_input_box() {
        // A real 2.1.277 footer: rule, empty input box, rule, status lines.
        let rows = screen(&[
            "⏺ the answer", "  body", "", "✻ Crunched for 1m 9s · done 1:21 PM", "",
            "──────────────────────────────────────", "❯ ", "──────────────────────────────────────",
            "  $0.18 | Haiku 4.5 | rudder:main | +0 -0 | 2m", "  ⏵⏵ bypass permissions on", "  80500 tokens",
        ]);
        assert_eq!(claude_content_end(&rows), 5);
        // The empty input box must never count as a user message.
        assert_eq!(claude_message_start_rows(&rows), vec![0]);
        // No chrome at all: everything is content.
        let bare = screen(&["⏺ a", "  b"]);
        assert_eq!(claude_content_end(&bare), 2);
    }

    #[test]
    fn view_moved_ignores_churn_in_the_bottom_half() {
        let before = screen(&["❯ test", "⏺ hi", "", "  body", "", "✻ Cooked for 1s · done 12:41 AM", "  40185 tokens", "❯"]);
        let mut spinner_only = before.clone();
        spinner_only[5] = "✻ Baked for 2s · done 12:41 AM".to_string();
        spinner_only[6] = "  40190 tokens".to_string();
        assert!(!claude_view_moved(&before, &spinner_only));
        let mut scrolled = before.clone();
        scrolled.rotate_right(1);
        assert!(claude_view_moved(&before, &scrolled));
    }

    #[test]
    fn page_shift_prefers_the_best_supported_then_larger_shift() {
        // Two identical "⏺ Bash(ls)" lines make shift 3 and shift 12 each
        // plausible from that line alone; "⏺ Read(a)" only fits shift 12.
        let before = screen(&["⏺ Bash(ls)", "  ⎿  x", "⏺ Read(a)", "  body"]);
        let mut after: Vec<String> = vec![String::new(); 16];
        after[3] = "⏺ Bash(ls)".to_string();
        after[12] = "⏺ Bash(ls)".to_string();
        after[14] = "⏺ Read(a)".to_string();
        assert_eq!(claude_page_shift(&before, &after), Some(12));
    }
}
