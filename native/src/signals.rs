//! Official, backend-native completion signals.
//!
//! Instead of SCRAPING a worker's PTY chrome to GUESS when a turn ended (the
//! fragile string heuristics in `detect.rs`), we wire each backend's OWN
//! lifecycle hooks to write a tiny signal file that the poll loop reads as the
//! AUTHORITATIVE done/idle source. The PTY scrape stays only as a fallback for
//! older CLI versions or when hooks are unavailable, so this is strictly additive
//! (worst case = today's behaviour).
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
/// notify config file exists on disk). When true the poll loop must WAIT for the
/// Stop hook / notify and NOT let the PTY-scrape flip the worker to review
/// mid-turn; when false (older CLI / hooks unavailable) the scrape is the only
/// completion source and stays active.
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

/// Read the latest signal a worker wrote, if any. Returns None when no signal
/// file exists (caller falls back to the PTY-scrape heuristics).
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
/// command, so it stays tiny (no node/shell startup beyond `sh -c`).
fn write_signal_command(signal: &Path, state: &str) -> String {
    let path = signal.display();
    format!(
        "mkdir -p '{dir}' && printf '{{\"state\":\"{state}\"}}' > '{path}'",
        dir = signal
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    )
}

/// The `claude --settings` JSON: a `Stop` hook (turn ended) and a `Notification`
/// hook matched to `idle_prompt` (paused for input), each writing the signal file.
pub(crate) fn claude_settings_json(signal: &Path) -> String {
    serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{ "type": "command", "command": write_signal_command(signal, "done") }]
            }],
            "Notification": [{
                "matcher": "idle_prompt",
                "hooks": [{ "type": "command", "command": write_signal_command(signal, "input") }]
            }]
        }
    })
    .to_string()
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
        "#!/bin/sh\n# Rudder Codex completion signal (notify program).\ncase \"$1\" in\n  *agent-turn-complete*)\n    mkdir -p '{dir}' && printf '{{\"state\":\"done\"}}' > '{path}' ;;\nesac\n"
    )
}

/// Whether this backend+mode is an INTERACTIVE worker that idles between turns
/// (so it needs a hook to signal completion). Headless planner modes run
/// `claude -p` / `codex exec` and complete via process exit, so they are excluded.
pub(crate) fn worker_wants_signals(backend: Backend, mode: AgentMode) -> bool {
    let _ = backend;
    matches!(
        mode,
        AgentMode::Execute | AgentMode::Main | AgentMode::ReviewAll
    )
}

/// Write the per-run hook config files (clearing any stale signal from a prior
/// turn so it can't be misread as this turn's completion) and return the launch
/// wiring. Best-effort: on any IO failure the worker simply runs without signals
/// and the PTY-scrape fallback takes over.
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
                if std::fs::write(&settings, claude_settings_json(&signal)).is_ok() {
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
/// by `push_codex_rudder_config_overrides` (last `-c` wins). No-op for non-worker
/// modes or when config generation failed (PTY-scrape fallback then applies).
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
                // Replace the `notify=[]` that push_codex_rudder_config_overrides baked
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
