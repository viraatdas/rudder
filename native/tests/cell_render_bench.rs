//! Microbenchmark for the styled-cell render path.
//!
//! A `sample` profile of a live dashboard attributed ~41% of render time to
//! `compute_styled_lines_snapshot` -> `styled_screen_row` and the window clone in
//! `styled_line_window_snapshot`, almost all of it inside malloc/free. This
//! measures exactly that pair so the cost is a number instead of a guess.
//!
//! Ignored by default because it is a timing test, not a correctness test:
//!   cargo test --manifest-path native/Cargo.toml --release \
//!     --test cell_render_bench -- --ignored --nocapture

use std::time::{Duration, Instant};

use rudder_native::pty_terminal::{
    TerminalCommand, TerminalPane, TerminalPaneOptions, TerminalSize,
};

const ROWS: u16 = 50;
const COLS: u16 = 200;
const ITERATIONS: u32 = 2_000;

/// Fills the screen with styled text: every line carries a different SGR color
/// and is nearly full width, which is the realistic shape of agent output (a
/// blank screen would measure nothing, since trailing blanks are trimmed).
fn fill_screen_command() -> TerminalCommand {
    TerminalCommand::with_args(
        "sh",
        [
            "-lc",
            "i=0; while [ $i -lt 60 ]; do \
               printf '\\033[1;3%dm' $((i % 8)); \
               awk 'BEGIN{s=\"\";for(j=0;j<190;j++)s=s \"x\";print s}'; \
               printf '\\033[0m'; \
               i=$((i+1)); \
             done; sleep 30",
        ],
    )
}

#[cfg(not(windows))]
#[test]
#[ignore = "timing benchmark; run explicitly with --ignored --nocapture"]
fn styled_snapshot_rebuild_cost() {
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(fill_screen_command()),
        TerminalPaneOptions {
            size: TerminalSize {
                rows: ROWS,
                cols: COLS,
            },
            scrollback_lines: 2_000,
            ..Default::default()
        },
    )
    .expect("spawn benchmark pty");

    // Wait for the screen to actually fill; measuring an empty grid would report
    // a flattering number that has nothing to do with the profile.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(25));
        pane.drain_output();
        let filled = pane
            .visible_lines_snapshot()
            .iter()
            .filter(|line| line.len() > 100)
            .count();
        if filled >= ROWS as usize - 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "benchmark screen never filled: {filled} wide rows"
        );
    }

    // Warm up so the first-touch page faults are not counted as render cost.
    for _ in 0..50 {
        pane.invalidate_render_cache();
        let _ = pane.styled_line_window_snapshot(ROWS as usize);
    }

    // Cold: what a frame costs after new output invalidated the cache. This is
    // the path the profiler caught.
    let started = Instant::now();
    let mut cells = 0usize;
    for _ in 0..ITERATIONS {
        pane.invalidate_render_cache();
        let (_start, rows) = pane.styled_line_window_snapshot(ROWS as usize);
        cells += rows.iter().map(Vec::len).sum::<usize>();
    }
    let cold = started.elapsed() / ITERATIONS;
    let cells_per_frame = cells / ITERATIONS as usize;

    // Warm: the cache survives, so this isolates the window clone alone. The gap
    // between the two is the actual grid rebuild.
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let (_start, rows) = pane.styled_line_window_snapshot(ROWS as usize);
        std::hint::black_box(&rows);
    }
    let warm = started.elapsed() / ITERATIONS;

    println!(
        "size_of StyledTerminalCell = {} bytes, CellContents = {} bytes",
        std::mem::size_of::<rudder_native::pty_terminal::StyledTerminalCell>(),
        std::mem::size_of::<rudder_native::pty_terminal::CellContents>(),
    );
    println!(
        "cells/frame {cells_per_frame}\n\
         cold (rebuild + window clone): {cold:?}  ({:.1} ns/cell)\n\
         warm (window clone only):      {warm:?}  ({:.1} ns/cell)\n\
         rebuild alone:                 {:?}",
        cold.as_nanos() as f64 / cells_per_frame.max(1) as f64,
        warm.as_nanos() as f64 / cells_per_frame.max(1) as f64,
        cold.saturating_sub(warm),
    );
}
