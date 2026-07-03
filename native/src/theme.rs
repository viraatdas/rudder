//! Central truecolor theme for the Rudder dashboard.
//!
//! The default color mode leaves the terminal foreground/background untouched so
//! Rudder blends with the user's tab bar and terminal theme. A saved `paper` mode
//! keeps the previous pure-white canvas for users who prefer that look. Accent,
//! status, and selection colors remain explicit in both modes.
#![allow(dead_code)]
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColorMode {
    Terminal,
    Paper,
}

impl ColorMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match normalize_color_mode(value).as_str() {
            "terminal" | "term" | "default" | "native" | "transparent" | "terminalbackground"
            | "terminalbg" | "bg" => Some(Self::Terminal),
            "paper" | "white" | "light" => Some(Self::Paper),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Paper => "paper",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Terminal => "terminal background",
            Self::Paper => "paper white",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Terminal => Self::Paper,
            Self::Paper => Self::Terminal,
        }
    }
}

fn normalize_color_mode(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

thread_local! {
    static CURRENT_COLOR_MODE: std::cell::Cell<ColorMode> =
        const { std::cell::Cell::new(ColorMode::Terminal) };
}

pub(crate) fn set_color_mode(mode: ColorMode) {
    CURRENT_COLOR_MODE.with(|current| current.set(mode));
}

pub(crate) fn color_mode() -> ColorMode {
    CURRENT_COLOR_MODE.with(std::cell::Cell::get)
}

pub(crate) fn terminal_background_mode() -> bool {
    color_mode() == ColorMode::Terminal
}

// --- Core palette. PAPER is used as the optional paper-mode canvas and as
// high-contrast text on filled accent badges.
/// The optional paper-mode canvas.
pub(crate) const PAPER: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
/// Primary text: near-black ink (matches the site's #1a1a1a).
pub(crate) const INK: Color = Color::Rgb(0x1A, 0x1A, 0x1A);
/// Primary focus accent: a teal that reads cleanly on white.
pub(crate) const ACCENT: Color = Color::Rgb(0x0B, 0x74, 0x7C);
/// Slightly deeper teal used as the fill behind the focused pane title.
pub(crate) const ACCENT_DEEP: Color = Color::Rgb(0x0B, 0x6E, 0x74);
/// Amber secondary accent (amber-700), legible on white.
pub(crate) const ACCENT_2: Color = Color::Rgb(0xB4, 0x53, 0x09);
/// Readable cool gray for secondary text.
pub(crate) const MUTED: Color = Color::Rgb(0x6B, 0x72, 0x80);
/// Light gray for hairline borders and de-emphasized text on white.
pub(crate) const FAINT: Color = Color::Rgb(0xA6, 0xAD, 0xB8);
/// Pale teal background for the selection highlight (with ink text on top).
pub(crate) const SURFACE_SEL: Color = Color::Rgb(0xD9, 0xEE, 0xF0);

// --- Legacy const names, re-pointed to the new palette ---
// Kept so render.rs inline modals, selection.rs highlights, and the status
// helpers below continue to compile and automatically adopt the new look.
pub(crate) const FOCUS_COLOR: Color = ACCENT;
pub(crate) const INACTIVE_COLOR: Color = FAINT;
pub(crate) const MODEL_COLOR: Color = Color::Rgb(0x7E, 0x22, 0xCE); // purple-700
pub(crate) const RUNNING_COLOR: Color = Color::Rgb(0xB4, 0x53, 0x09); // amber-700
pub(crate) const DONE_COLOR: Color = Color::Rgb(0x15, 0x80, 0x3D); // green-700
pub(crate) const FAILED_COLOR: Color = Color::Rgb(0xC0, 0x39, 0x2B); // red-600
pub(crate) const CLOUD_COLOR: Color = ACCENT; // teal

// --- DAG node status colors. Distinct + quiet so no state reads as gray, all
// legible on white. ---
pub(crate) const ST_PLANNED: Color = Color::Rgb(0x47, 0x55, 0x69); // slate-600
pub(crate) const ST_READY: Color = ACCENT; // teal
pub(crate) const ST_RUNNING: Color = RUNNING_COLOR; // amber
pub(crate) const ST_REVIEW: Color = Color::Rgb(0xA2, 0x1C, 0xAF); // magenta-700
pub(crate) const ST_BLOCKED: Color = Color::Rgb(0xC2, 0x41, 0x0C); // orange-700
pub(crate) const ST_MERGED: Color = DONE_COLOR; // green
pub(crate) const ST_FAILED: Color = FAILED_COLOR; // red

/// Filled-circle glyph used as a colored status badge in the agent list.
pub(crate) const BADGE: &str = "\u{25CF}"; // ●

/// Color for an agent's status. Done and Merged are a visible green.
pub(crate) fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Running => ST_RUNNING,
        AgentStatus::Done | AgentStatus::Merged => ST_MERGED,
        AgentStatus::Failed => ST_FAILED,
        AgentStatus::Stopped => MUTED,
    }
}

/// Section-header style ("main" / "worktrees" / "merged"): a quiet, thin label.
/// No bold (the theme is thin); hierarchy comes from color + spacing.
pub(crate) fn header_style(focused: bool) -> Style {
    Style::default().fg(if focused { MUTED } else { FAINT })
}
