#![allow(unused_imports)]
//! Desktop notifications delivered THROUGH the terminal emulator (OSC 777).
//!
//! Workers run inside Rudder's embedded terminal, so a backend's own desktop
//! notification escapes (Claude emits OSC 777 "needs your permission") are
//! consumed by our VT parser and never reach the real terminal. The dashboard
//! therefore re-emits its own notification at each lifecycle transition it
//! owns: entering review, failing, and a worker latching a question/permission
//! wait. Ghostty, kitty and other urxvt-notify-aware emulators render these as
//! native OS notifications per tab; emulators that don't understand OSC 777
//! ignore it silently.
//!
//! Focus gating: the tab the user is looking at never notifies — they can see
//! the row change color. Focus is tracked via the terminal's focus-report mode
//! (CSI ?1004), flipped by `Event::FocusGained`/`FocusLost` in the event loop.
//! The default is "focused" so a session that never receives focus events
//! (emulator without mode 1004) stays silent rather than spamming.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

static TERMINAL_FOCUSED: AtomicBool = AtomicBool::new(true);

pub(crate) fn set_terminal_focused(focused: bool) {
    TERMINAL_FOCUSED.store(focused, Ordering::Relaxed);
}

pub(crate) fn terminal_focused() -> bool {
    TERMINAL_FOCUSED.load(Ordering::Relaxed)
}

/// Longest title/body we ship: notification centers truncate anyway, and an
/// oversized OSC risks being dropped whole by intermediaries.
const MAX_FIELD_CHARS: usize = 120;

/// Strip everything that could terminate or corrupt the OSC payload: control
/// characters (ESC/BEL end the sequence early) and, for the title, semicolons
/// (the field separator — a `;` in the title would shift the body).
fn sanitize_field(value: &str, strip_semicolons: bool) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_FIELD_CHARS + 3));
    for (count, ch) in value.chars().enumerate() {
        if count >= MAX_FIELD_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() {
            out.push(' ');
        } else if strip_semicolons && ch == ';' {
            out.push(',');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// The raw escape sequence for one notification. `ESC ] 777 ; notify ; title ;
/// body BEL`, wrapped in a tmux passthrough envelope when running under tmux
/// (tmux swallows unknown OSCs otherwise; inside the envelope every ESC is
/// doubled per the DCS tmux; protocol).
pub(crate) fn build_notification_sequence(title: &str, body: &str, under_tmux: bool) -> String {
    let title = sanitize_field(title, true);
    let body = sanitize_field(body, false);
    let osc = format!("\x1b]777;notify;{title};{body}\x07");
    if under_tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else {
        osc
    }
}

/// Emit one desktop notification via the outer terminal. No-op when disabled
/// (`/notify off`), when this tab currently has focus (the user can already
/// see the dashboard), or in tests. Safe to write directly: event handling and
/// drawing share one thread, so this never interleaves with a frame.
pub(crate) fn notify_desktop(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    if !desktop_notifications_enabled() {
        return;
    }
    if terminal_focused() {
        return;
    }
    let seq = build_notification_sequence(title, body, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn repo_display_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "rudder".to_string())
}

/// One lifecycle notification for a run: "Rudder · <repo>" + "<event> — <task>".
/// The repo name distinguishes tabs when several dashboards are open.
pub(crate) fn notify_run(run: &AgentRun, event: &str) {
    let repo = repo_display_name(&run.cwd);
    let summary = run.task_summary.trim();
    let body = if summary.is_empty() {
        event.to_string()
    } else {
        format!("{event} — {summary}")
    };
    notify_desktop(&format!("Rudder · {repo}"), &body);
}

/// Compose the (title, body) for a notification the WORKER itself emitted
/// (Claude Code's own OSC 777/9, captured by the embedded terminal's scanner).
/// The backend's wording is kept verbatim — that is the point of forwarding —
/// with the repo appended to the title and the task appended to the body so
/// several tabs and several agents stay tellable-apart.
pub(crate) fn compose_worker_notification(
    cwd: &Path,
    task_summary: &str,
    note: &TerminalNotification,
) -> (String, String) {
    let repo = repo_display_name(cwd);
    let title = if note.title.trim().is_empty() {
        format!("Rudder · {repo}")
    } else {
        format!("{} · {repo}", note.title.trim())
    };
    let summary = task_summary.trim();
    let body = if summary.is_empty() {
        note.body.clone()
    } else if note.body.trim().is_empty() {
        summary.to_string()
    } else {
        format!("{} — {summary}", note.body.trim())
    };
    (title, body)
}

pub(crate) fn forward_worker_notification(
    cwd: &Path,
    task_summary: &str,
    note: &TerminalNotification,
) {
    let (title, body) = compose_worker_notification(cwd, task_summary, note);
    notify_desktop(&title, &body);
}
