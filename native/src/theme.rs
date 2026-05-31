//! Central truecolor theme for the Rudder dashboard.
//!
//! Brand-anchored (warm teal accent, warm grays) to replace the flat, washed-out
//! look that came from DarkGray dominating every unfocused surface and an
//! invisible `Color::Gray` for completed agents. Every color the dashboard draws
//! lives here so the palette is one edit away. The legacy const names
//! (`FOCUS_COLOR`, `INACTIVE_COLOR`, ...) are kept and re-pointed so existing call
//! sites in `render.rs`/`selection.rs` pick up the new look unchanged.
#![allow(dead_code)]
use super::*;

// --- Brand-anchored core palette (tuned for a dark terminal background) ---
/// Primary focus accent: brand teal, lifted for contrast on a dark terminal.
pub(crate) const ACCENT: Color = Color::Rgb(0x2A, 0xB7, 0xC0);
/// Exact brand teal (#006d75) used as a fill behind the focused pane title.
pub(crate) const ACCENT_DEEP: Color = Color::Rgb(0x00, 0x6D, 0x75);
/// Amber secondary accent (brand #934e04 lifted).
pub(crate) const ACCENT_2: Color = Color::Rgb(0xE0, 0x9A, 0x2B);
/// Warm off-white primary text (brand paper #f5f3ee, inverted for dark bg).
pub(crate) const INK: Color = Color::Rgb(0xEC, 0xE8, 0xDE);
/// Readable warm gray for secondary text (replaces the old DarkGray/Gray).
pub(crate) const MUTED: Color = Color::Rgb(0x9A, 0x93, 0x84);
/// Faint warm gray for unfocused borders and de-emphasized text.
pub(crate) const FAINT: Color = Color::Rgb(0x6B, 0x65, 0x59);
/// Teal-tinted background for the selection highlight.
pub(crate) const SURFACE_SEL: Color = Color::Rgb(0x24, 0x33, 0x35);

// --- Legacy const names, re-pointed to the new palette ---
// Kept so render.rs inline modals, selection.rs highlights, and the status
// helpers below continue to compile and automatically adopt the new colors.
pub(crate) const FOCUS_COLOR: Color = ACCENT;
pub(crate) const INACTIVE_COLOR: Color = FAINT;
pub(crate) const MODEL_COLOR: Color = Color::Rgb(0xB6, 0x6F, 0xB0); // magenta, lifted
pub(crate) const RUNNING_COLOR: Color = Color::Rgb(0xE0, 0x9A, 0x2B); // amber
pub(crate) const DONE_COLOR: Color = Color::Rgb(0x4F, 0xB0, 0x6A); // green: done is now VISIBLE
pub(crate) const FAILED_COLOR: Color = Color::Rgb(0xD8, 0x4B, 0x4B); // softer red
pub(crate) const CLOUD_COLOR: Color = ACCENT; // teal

// --- DAG node status colors (used from Phase 3 on; defined here for one source
// of truth). Vivid + distinct so no state reads as gray. ---
pub(crate) const ST_PLANNED: Color = Color::Rgb(0x8A, 0x9B, 0xB8); // slate-blue
pub(crate) const ST_READY: Color = ACCENT; // teal
pub(crate) const ST_RUNNING: Color = RUNNING_COLOR; // amber
pub(crate) const ST_REVIEW: Color = Color::Rgb(0xB6, 0x6F, 0xB0); // magenta
pub(crate) const ST_BLOCKED: Color = Color::Rgb(0xCC, 0x6A, 0x3A); // burnt orange
pub(crate) const ST_MERGED: Color = DONE_COLOR; // green
pub(crate) const ST_FAILED: Color = FAILED_COLOR; // red

/// Filled-circle glyph used as a colored status badge in the agent list.
pub(crate) const BADGE: &str = "\u{25CF}"; // ●

/// Color for an agent's status. Done and Merged are a visible green (the old
/// `Color::Gray` made completed agents disappear into the background).
pub(crate) fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Running => ST_RUNNING,
        AgentStatus::Done | AgentStatus::Merged => ST_MERGED,
        AgentStatus::Failed => ST_FAILED,
        AgentStatus::Stopped => MUTED,
    }
}

/// Section-header style ("main" / "worktrees" / "merged"): bold + readable so the
/// list has real hierarchy instead of three near-invisible gray labels.
pub(crate) fn header_style(focused: bool) -> Style {
    Style::default()
        .fg(if focused { MUTED } else { FAINT })
        .add_modifier(Modifier::BOLD)
}
