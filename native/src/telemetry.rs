//! Product events and `/feedback`, emitted from the dashboard.
//!
//! The dashboard has no HTTP client and should never grow one for this: a stalled
//! POST inside the render loop is a worse bug than any missing metric. Instead it
//! shells out to `rudder __event` / `rudder feedback` DETACHED, so one telemetry
//! implementation (src/analytics.ts) serves both halves of Rudder and the TUI
//! only ever pays a fork.
//!
//! Every emit here is best-effort and silent. If the CLI is missing, telemetry is
//! off, or the network is down, the dashboard behaves exactly the same.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Fire one product event. Properties must be small, non-identifying scalars —
/// see the rules at the top of src/analytics.ts, which enforces them again.
pub(crate) fn emit_event(name: &str, properties: serde_json::Value) {
    if cfg!(test) {
        return;
    }
    let Some(rudder) = crate::locate_rudder_cli() else {
        return;
    };
    let mut command = Command::new(rudder);
    command.arg("__event").arg(name);
    if !properties.is_null() {
        command.arg("--props").arg(properties.to_string());
    }
    spawn_detached(command);
}

/// Hand a feedback report to the CLI, which owns the local copy, the usage event,
/// and the GitHub issue. The context is passed as a FILE so a long report never
/// hits an argv limit and never lands in a process listing.
pub(crate) fn submit_feedback(
    repo_root: &Path,
    context: serde_json::Value,
) -> std::io::Result<PathBuf> {
    let dir = repo_root.join(".rudder").join("feedback");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("pending-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec_pretty(&context)?)?;
    if cfg!(test) {
        return Ok(path);
    }
    let Some(rudder) = crate::locate_rudder_cli() else {
        return Ok(path);
    };
    let mut command = Command::new(rudder);
    command.arg("feedback").arg("--context").arg(&path);
    command.current_dir(repo_root);
    spawn_detached(command);
    Ok(path)
}

fn spawn_detached(mut command: Command) {
    if let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap off-thread so a fire-and-forget emit never leaves a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
