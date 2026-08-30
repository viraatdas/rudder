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
    /// The worker finished its turn (Claude `Stop` / Codex `agent-turn-complete`
    /// / opencode root-session `session.idle`).
    Done,
    /// The worker paused with a question (Claude `Notification` matcher
    /// `idle_prompt`).
    Input,
    /// The worker is blocked on an approval prompt (Claude `Notification`
    /// matcher `permission_request` / opencode `permission.updated`). Distinct
    /// from `Input`: a question needs an answer, a permission needs a decision,
    /// and the dashboard colours them differently.
    Permission,
    /// The human answered and the worker went back to work (Claude
    /// `UserPromptSubmit` / opencode `permission.replied`).
    ///
    /// This is the counterpart the protocol was missing. `Input` and
    /// `Permission` latch — they have to, or a row would drop its "needs you"
    /// badge the moment the prompt scrolled out of the visible pane — and
    /// nothing ever lifted the latch, so answering an agent left it flagged
    /// until its NEXT turn ended.
    Working,
}

/// Extra launch wiring for a worker so it emits official completion signals.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerSignals {
    /// Path to write into `claude --settings <path>` (Claude workers only).
    pub(crate) claude_settings: Option<PathBuf>,
    /// Path to point Codex `notify` at (Codex workers only).
    pub(crate) codex_notify: Option<PathBuf>,
    /// Config file to hand opencode through `OPENCODE_CONFIG` (opencode workers
    /// only). It registers the generated plugin that reports turn-end.
    pub(crate) opencode_config: Option<PathBuf>,
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
    // Ask "were hooks wired for this RUN", not "for this run under the backend it
    // claims right now". A row's backend can change after launch (switching model
    // provider rewrites it), and keying the check on the current value made the
    // poll loop look for a file that was never going to exist — then KILL a
    // perfectly healthy worker for "lifecycle hooks were not installed".
    // The run id is unique, so any of the three configs proves the wiring ran.
    let _ = backend;
    [
        dir.join(format!("{run_id}-claude.json")),
        dir.join(format!("{run_id}-notify.sh")),
        dir.join(format!("{run_id}-opencode.json")),
    ]
    .iter()
    .any(|file| file.exists())
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
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-opencode.json")));
    let _ = std::fs::remove_file(dir.join(format!("{run_id}-opencode.js")));
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
        "permission" => Some(SignalState::Permission),
        "working" => Some(SignalState::Working),
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
            "Notification": [
                {
                    "matcher": "idle_prompt",
                    "hooks": [{ "type": "command", "command": write_signal_command(signal, "input") }]
                },
                {
                    // Claude's OTHER notification matcher. Only `idle_prompt` was
                    // wired, so a worker blocked on a tool-approval prompt — the
                    // single most common reason an agent stops dead — had no
                    // native signal at all and was left to the string heuristics.
                    "matcher": "permission_request",
                    "hooks": [{ "type": "command", "command": write_signal_command(signal, "permission") }]
                }
            ],
            // The human answered: back to work, clear the latch. Fires once per
            // human turn, so it costs one tiny write — unlike PreToolUse, which
            // would fire on every tool call.
            "UserPromptSubmit": [{
                "hooks": [{ "type": "command", "command": write_signal_command(signal, "working") }]
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

/// The opencode plugin that reports turn-end, generated per run.
///
/// opencode has no hook flag like Claude's `--settings` or Codex's `notify`; it has
/// a plugin API instead. A plugin's `event` hook sees the session and permission
/// lifecycle — exactly the states Rudder's signal file carries. The plugin is
/// loaded from Rudder's own directory via `OPENCODE_CONFIG`, never written into
/// the worker's workspace, so it can never show up in the diff Rudder merges.
///
/// Two things this has to get right that the first version did not:
///
/// 1. **`session.idle` fires for SUBAGENT sessions too.** opencode's `Session`
///    carries a `parentID`, and a child session finishing its work idles just
///    like the root one. Reporting `done` on those marked the whole run complete
///    the first time the agent delegated anything. We resolve the session and
///    ignore any that has a parent.
/// 2. **`permission.updated` has a counterpart, `permission.replied`.** Only the
///    ask was wired, so the waiting state latched on and never lifted.
///
/// Every lookup fails OPEN: if the session cannot be resolved for any reason
/// (older opencode, changed client shape, server hiccup) we report anyway. A
/// wrong guess about a subagent is a cosmetic early "done"; a silent plugin is a
/// worker that never completes at all, which is far worse.
///
/// Writes to a pid-suffixed temp and renames, like the other two backends, so the
/// poll loop can never read a torn signal.
pub(crate) fn opencode_plugin_js(signal: &Path) -> String {
    let path = signal.display();
    format!(
        r#"// Generated by Rudder. Reports opencode turn state to the dashboard.
import {{ writeFileSync, renameSync, mkdirSync }} from "node:fs";
import {{ dirname }} from "node:path";

const SIGNAL = "{path}";

function report(state) {{
  try {{
    mkdirSync(dirname(SIGNAL), {{ recursive: true }});
    const temp = SIGNAL + "." + process.pid + ".tmp";
    writeFileSync(temp, JSON.stringify({{ state }}));
    renameSync(temp, SIGNAL);
  }} catch {{
    // The dashboard falls back to process exit; never break the agent over this.
  }}
}}

export const RudderSignal = async ({{ client }}) => {{
  // Sessions we have already classified. A run emits many events per session,
  // and resolving each one would put an HTTP round-trip on the event path.
  const isSubagent = new Map();

  async function rootSession(sessionID) {{
    if (!sessionID) return true; // Nothing to check against: fail open.
    if (isSubagent.has(sessionID)) return !isSubagent.get(sessionID);
    let child = false;
    try {{
      const result = await client.session.get({{ path: {{ id: sessionID }} }});
      const session = result?.data ?? result;
      child = Boolean(session?.parentID);
    }} catch {{
      child = false; // Fail open: report rather than go silent.
    }}
    isSubagent.set(sessionID, child);
    return !child;
  }}

  return {{
    event: async ({{ event }}) => {{
      const type = event?.type;
      const sessionID = event?.properties?.sessionID;
      if (type === "session.idle") {{
        // A delegated subagent going idle is not this run finishing its turn.
        if (await rootSession(sessionID)) report("done");
      }} else if (type === "permission.updated") {{
        if (await rootSession(sessionID)) report("permission");
      }} else if (type === "permission.replied") {{
        // The human decided. Back to work — this is what lifts the latch.
        if (await rootSession(sessionID)) report("working");
      }}
    }},
  }};
}};
"#
    )
}

/// The `OPENCODE_CONFIG` payload that loads the generated plugin. An absolute path
/// is used deliberately: plugin entries resolve differently depending on where the
/// config lives, and the worker's cwd is a jj workspace we do not want to touch.
pub(crate) fn opencode_config_json(plugin: &Path) -> String {
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "plugin": [plugin.display().to_string()],
    })
    .to_string()
}

/// Whether this backend+mode is an INTERACTIVE worker that idles between turns
/// (so it needs a hook to signal completion). Headless planner modes run
/// `claude -p` / `codex exec` and complete via process exit, so they are excluded.
pub(crate) fn worker_wants_signals(backend: Backend, mode: AgentMode) -> bool {
    let _ = backend;
    matches!(
        mode,
        AgentMode::Execute
            | AgentMode::Main
            | AgentMode::ReviewAll
            | AgentMode::OneOff
            // The gam reviewer needs hooks for BOTH reasons a worker ever does.
            // Its turn end is what poll_gam_pairs waits on (it routes on the
            // peer reaching AgentStatus::Done, which only a completion signal
            // sets), and the lifecycle sweep kills any non-orchestrator pane
            // that has no config installed. Leaving this mode out did not
            // degrade the reviewer, it killed it within a tick of spawning --
            // silently, in every pair, since the feature shipped.
            | AgentMode::GamAdversarial
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
                        ..WorkerSignals::default()
                    };
                }
            }
            Backend::Codex => {
                let script = dir.join(format!("{run_id}-notify.sh"));
                if std::fs::write(&script, codex_notify_script(&signal)).is_ok() {
                    set_executable(&script);
                    return WorkerSignals {
                        codex_notify: Some(script),
                        ..WorkerSignals::default()
                    };
                }
            }
            Backend::Opencode => {
                let plugin = dir.join(format!("{run_id}-opencode.js"));
                let config = dir.join(format!("{run_id}-opencode.json"));
                if std::fs::write(&plugin, opencode_plugin_js(&signal)).is_ok()
                    && std::fs::write(&config, opencode_config_json(&plugin)).is_ok()
                {
                    return WorkerSignals {
                        opencode_config: Some(config),
                        ..WorkerSignals::default()
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
        Backend::Opencode => {
            if let Some(path) = signals.opencode_config {
                // opencode reads its config from this env var; the config names the
                // generated plugin that reports turn-end.
                command
                    .env
                    .push(("OPENCODE_CONFIG".to_string(), path.display().to_string()));
            }
        }
    }
}
