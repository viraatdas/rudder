//! Official, backend-native completion signals.
//!
//! Instead of SCRAPING a worker's PTY chrome to GUESS when a turn ended (the
//! fragile string heuristics in `detect.rs`), we wire each backend's OWN
//! lifecycle hooks to write a tiny signal file that the poll loop reads as the
//! AUTHORITATIVE done/idle source. Rudder never guesses completion from terminal chrome.
//!
//! - **Claude Code** (`code.claude.com/docs/en/hooks`): a `Stop` hook fires when
//!   the turn ends; a `Notification` hook with matcher `idle_prompt` fires when it
//!   pauses for input. Wired via `claude --settings <file>`.
//! - **Codex** (`developers.openai.com/codex`): the `notify` program fires
//!   `agent-turn-complete`. Wired via `-c notify=[<script>]`. (Codex's newer
//!   `[hooks]` are trust-gated, so unusable for a headless child; `notify` needs
//!   no trust and is the right fit.)
//!
//! Both write `<rudder_home>/signals/<run_id>.json` = `{"state":"done"|"input"}`.

#![allow(unused_imports)]
use super::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalState {
    /// The worker finished its turn (Claude `Stop` / Codex `agent-turn-complete`).
    Done,
    /// The worker paused for input (Claude `Notification` matcher `idle_prompt`).
    Input,
}

/// Extra launch wiring for a worker so it emits official completion signals.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerSignals {
    /// Path to write into `claude --settings <path>` (Claude workers only).
    pub(crate) claude_settings: Option<PathBuf>,
    /// Path to point Codex `notify` at (Codex workers only).
    pub(crate) codex_notify: Option<PathBuf>,
}

fn rudder_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            return Some(value);
        }
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".rudder"))
}

pub(crate) fn signals_dir() -> Option<PathBuf> {
    Some(rudder_home()?.join("signals"))
}

pub(crate) fn signal_path(run_id: &str) -> Option<PathBuf> {
    Some(signals_dir()?.join(format!("{run_id}.json")))
}

/// Whether official-signal hooks were wired for this run (its `--settings` /
/// notify config file exists on disk). Interactive workers without this wiring
/// fail visibly instead of guessing completion from terminal output.
pub(crate) fn worker_has_config(run_id: &str, backend: Backend) -> bool {
    let Some(dir) = signals_dir() else {
        return false;
    };
    let file = match backend {
        Backend::Claude => dir.join(format!("{run_id}-claude.json")),
        Backend::Codex => dir.join(format!("{run_id}-notify.sh")),
    };
    file.exists()
}

/// Read the latest signal a worker wrote, if any.
pub(crate) fn read_signal(run_id: &str) -> Option<SignalState> {
    let body = std::fs::read_to_string(signal_path(run_id)?).ok()?;
    parse_signal_state(&body)
}

/// Remove a consumed signal so it cannot re-fire on a later turn of the same
/// live agent (the worker writes a fresh one when its next turn ends).
pub(crate) fn clear_signal(run_id: &str) {
    if let Some(path) = signal_path(run_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove every per-run signal artifact (the signal file plus the Claude `--settings`
/// / Codex notify config) when a run is deleted: nothing else ever prunes them, so
/// `<rudder_home>/signals/` would otherwise accumulate dead runs forever. Best-effort.
pub(crate) fn cleanup_run_signals(run_id: &str) {
    let Some(dir) = signals_dir() else {
        return;
    };
    let _ = std::fs::remove_file(dir.join(format!("{run_id}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-claude.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-notify.sh")));
}

/// Remove stale signal/config files that no run deletion ever reached. Best-effort.
pub(crate) fn cleanup_old_signals(max_age: Duration) -> usize {
    let Some(dir) = signals_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return 0;
    };
    let mut removed = 0_usize;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// Tolerant parse of the signal JSON body into a state.
pub(crate) fn parse_signal_state(body: &str) -> Option<SignalState> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    match value.get("state").and_then(|s| s.as_str())? {
        "done" => Some(SignalState::Done),
        "input" => Some(SignalState::Input),
        _ => None,
    }
}

/// POSIX-sh one-liner that writes `{"state":"<state>"}` to `signal`, creating the
/// parent dir. Single-quoted so the path is taken literally; this runs as a hook
/// command, so it stays tiny (no node/shell startup beyond `sh -c`). Writes to a
/// pid-suffixed temp and `mv`s into place: rename is atomic on the same
/// filesystem, so the TUI poll loop can never read a torn half-written JSON
/// (a bare `printf >` left exactly that window at the moment that matters most).
fn write_signal_command(signal: &Path, state: &str) -> String {
    let path = signal.display();
    format!(
        "mkdir -p '{dir}' && printf '{{\"state\":\"{state}\"}}' > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}'",
        dir = signal
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )
}

/// The `claude --settings` JSON: a `Stop` hook (turn ended) and a `Notification`
/// hook matched to `idle_prompt` (paused for input), each writing the signal file.
/// When `fast_mode` is set, also carries `fastMode: true` — the SAME settings key
/// Claude Code's own `/fast` uses — so a launched worker runs in native fast mode
/// (Opus, accelerated output) instead of being faked with a reduced effort level.
pub(crate) fn claude_settings_json(signal: &Path, fast_mode: bool) -> String {
    let mut settings = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{ "type": "command", "command": write_signal_command(signal, "done") }]
            }],
            "Notification": [{
                "matcher": "idle_prompt",
                "hooks": [{ "type": "command", "command": write_signal_command(signal, "input") }]
            }]
        }
    });
    if fast_mode {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("fastMode".to_string(), serde_json::Value::Bool(true));
        }
    }
    settings.to_string()
}

/// The Codex `notify` script: Codex calls it with the event JSON as `$1`. We only
/// act on `agent-turn-complete` (the turn-ended event), writing the done signal.
pub(crate) fn codex_notify_script(signal: &Path) -> String {
    let path = signal.display();
    let dir = signal
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "#!/bin/sh\n# Rudder Codex completion signal (notify program).\n# Temp + mv so the TUI never reads a torn half-written signal.\ncase \"$1\" in\n  *agent-turn-complete*)\n    mkdir -p '{dir}' && printf '{{\"state\":\"done\"}}' > '{path}'.$$.tmp && mv -f '{path}'.$$.tmp '{path}' ;;\nesac\n"
    )
}

/// Whether this backend+mode is an INTERACTIVE worker that idles between turns
/// (so it needs a hook to signal completion). Headless planner modes run
/// `claude -p` / `codex exec` and complete via process exit, so they are excluded.
pub(crate) fn worker_wants_signals(backend: Backend, mode: AgentMode) -> bool {
    let _ = backend;
    matches!(
        mode,
        AgentMode::Execute | AgentMode::Main | AgentMode::ReviewAll | AgentMode::OneOff
    )
}

/// Write the per-run hook config files (clearing any stale signal from a prior
/// turn so it can't be misread as this turn's completion) and return the launch
/// wiring. The poll loop surfaces config-generation failure as a lifecycle error.
pub(crate) fn prepare_worker_signals(run_id: &str, backend: Backend) -> WorkerSignals {
    let Some(dir) = signals_dir() else {
        return WorkerSignals::default();
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return WorkerSignals::default();
    }
    // Clear a stale signal from a previous turn of this same run.
    if let Some(signal) = signal_path(run_id) {
        let _ = std::fs::remove_file(&signal);
        match backend {
            Backend::Claude => {
                let settings = dir.join(format!("{run_id}-claude.json"));
                // Carry the persisted /fast toggle into the worker's settings (Claude
                // fast mode is a settings flag, not a launch flag, so this is how it
                // reaches a spawned worker).
                let fast_mode = crate::config::fast_mode_enabled();
                if std::fs::write(&settings, claude_settings_json(&signal, fast_mode)).is_ok() {
                    return WorkerSignals {
                        claude_settings: Some(settings),
                        codex_notify: None,
                    };
                }
            }
            Backend::Codex => {
                let script = dir.join(format!("{run_id}-notify.sh"));
                if std::fs::write(&script, codex_notify_script(&signal)).is_ok() {
                    set_executable(&script);
                    return WorkerSignals {
                        claude_settings: None,
                        codex_notify: Some(script),
                    };
                }
            }
        }
    }
    WorkerSignals::default()
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Append the official-signal launch flags to an already-built worker command.
/// Claude gets `--settings <file>` (Stop + idle Notification hooks); Codex gets a
/// trailing `-c notify=["<script>"]` that OVERRIDES the earlier `notify=[]` baked
/// by the Codex capability profile (last `-c` wins). No-op for non-worker
/// modes. Config-generation failure is surfaced by the poll loop.
pub(crate) fn augment_worker_command(
    command: &mut TerminalCommand,
    backend: Backend,
    mode: AgentMode,
    run_id: &str,
) {
    if !worker_wants_signals(backend, mode) {
        return;
    }
    let signals = prepare_worker_signals(run_id, backend);
    match backend {
        Backend::Claude => {
            if let Some(path) = signals.claude_settings {
                command.args.push("--settings".to_string());
                command.args.push(path.display().to_string());
            }
        }
        Backend::Codex => {
            if let Some(path) = signals.codex_notify {
                // Replace the `notify=[]` that the Codex worker profile baked
                // in (its preceding `-c` stays), so this works regardless of how Codex
                // resolves duplicate `-c` keys.
                let replacement = format!("notify=[\"{}\"]", path.display());
                if let Some(slot) = command
                    .args
                    .iter_mut()
                    .find(|arg| arg.as_str() == "notify=[]")
                {
                    *slot = replacement;
                } else {
                    command.args.push("-c".to_string());
                    command.args.push(replacement);
                }
            }
        }
    }
}
