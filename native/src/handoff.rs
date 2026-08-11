//! CONVERSATION HANDOFF: continue an existing Claude/Codex CLI chat as a Rudder agent.
//!
//! The chat you are already having in a terminal holds all the context — the files
//! you looked at, the decisions you made, the plan you agreed on. Retyping that into
//! the task bar loses it. A handoff instead FORKS that conversation into a Rudder
//! pane: the agent opens with the full prior transcript in memory, in its own jj
//! workspace, and its work merges like any other worker.
//!
//! Two directions, one launcher:
//!   * PULL — `/handoff` in the dashboard lists recent conversations for this repo.
//!   * PUSH — `rudder handoff "<next step>"` from inside the live chat queues a
//!     request under `.rudder/handoffs/`, which the dashboard drains on its poll.
//!
//! Handoffs always FORK (never plain `--resume`): the source conversation is usually
//! still open in the user's terminal, and two processes appending to one transcript
//! would corrupt it.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::Backend;

/// How far into a transcript to look for its opening prompt. The first user turn is
/// always in the first handful of lines; the caps only bound a pathological file.
const HEAD_SCAN_LINES: usize = 400;
/// How many Codex rollouts to open before giving up. Codex's session tree is global
/// and grows into the thousands; this bounds a refresh that finds nothing.
const CODEX_SCAN_LIMIT: usize = 600;
const HEAD_SCAN_BYTES: u64 = 256 * 1024;
/// Queued requests older than this are stale (the dashboard was closed for a day);
/// silently dropping them beats resurrecting yesterday's chat as a live agent.
const REQUEST_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// Where a handed-off conversation continues.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HandoffTarget {
    /// A fresh jj workspace: an isolated, mergeable worker row. The default —
    /// handed-off work should merge like every other worker.
    Worker,
    /// The main checkout, like `/ask`: edits land directly in the user's tree.
    Here,
}

impl HandoffTarget {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "worker" | "isolated" | "workspace" => Some(Self::Worker),
            "here" | "main" | "checkout" => Some(Self::Here),
            _ => None,
        }
    }
}

/// Split `/resume`'s arguments into where it should run, which conversation, and
/// what to say next.
///
/// The default is an isolated worker, matching what bare task input does: a chat
/// adopted from outside the dashboard is new work, and new work gets reviewed and
/// merged like everything else. `--here` (or `--main`) opts out and continues the
/// conversation in the main checkout, which is what you want when the chat was
/// already about the files sitting in your tree. The flag is accepted on either
/// side of the id, because the palette inserts the id and the user types after it.
pub(crate) fn parse_resume_args(rest: &str) -> (HandoffTarget, String, String) {
    let mut target = HandoffTarget::Worker;
    let mut remaining = rest.trim();
    // Leading flags, then the id.
    let mut session_id = String::new();
    while let Some((token, tail)) = next_token(remaining) {
        remaining = tail;
        if is_here_flag(token) {
            target = HandoffTarget::Here;
            continue;
        }
        session_id = token.to_string();
        break;
    }
    // A trailing flag is the likelier spelling: the palette inserts the id and
    // leaves the cursor after it. Only the token IMMEDIATELY after the id counts,
    // so "--here" further into a sentence stays part of the instruction.
    if let Some((token, tail)) = next_token(remaining) {
        if is_here_flag(token) {
            target = HandoffTarget::Here;
            remaining = tail;
        }
    }
    (target, session_id, remaining.trim().to_string())
}

fn is_here_flag(token: &str) -> bool {
    matches!(token, "--here" | "--main" | "-here" | "-main")
}

/// The next whitespace-delimited token and the untouched remainder after it. The
/// remainder keeps its own spacing: it becomes an instruction handed to an agent.
fn next_token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if text.is_empty() {
        return None;
    }
    Some(match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], &text[index..]),
        None => (text, ""),
    })
}

/// One queued `rudder handoff` request read off `.rudder/handoffs/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandoffRequest {
    pub(crate) session_id: String,
    pub(crate) backend: Backend,
    pub(crate) target: HandoffTarget,
    pub(crate) instruction: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) created_at_ms: Option<u64>,
}

/// A resumable conversation offered by the `/handoff` picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConversationCandidate {
    pub(crate) session_id: String,
    pub(crate) backend: Backend,
    /// The conversation's opening user prompt, cleaned of tool/system wrappers.
    pub(crate) title: String,
    /// The model that conversation was running on, as the backend recorded it.
    /// None when the backend does not write one down (opencode's session list).
    pub(crate) model: Option<String>,
    /// The directory the conversation ran in, as the backend recorded it. This is
    /// what tells `main` apart from a chat started in a subfolder, and it is read
    /// from the transcript rather than inferred from the project folder name,
    /// which encodes `/` as `-` and cannot be decoded back without ambiguity.
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) modified: SystemTime,
}

/// Where a conversation ran, phrased for a picker row: `main` for the dashboard's
/// own checkout, the repo-relative directory for a chat started in a subfolder.
/// None when the backend recorded nothing, so the row simply omits the field
/// rather than guessing.
pub(crate) fn origin_label(cwd: Option<&Path>, dashboard_root: &Path) -> Option<String> {
    let cwd = cwd?;
    let root = std::fs::canonicalize(dashboard_root).unwrap_or_else(|_| dashboard_root.to_path_buf());
    let resolved = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if resolved == root || cwd == dashboard_root {
        return Some("main".to_string());
    }
    // Try both spellings for the same reason the Codex scan does: canonicalizing
    // resolves symlinks, but the transcript recorded whatever the shell was in.
    let relative = resolved
        .strip_prefix(&root)
        .ok()
        .or_else(|| cwd.strip_prefix(dashboard_root).ok())?;
    let shown = relative.display().to_string();
    (!shown.is_empty()).then_some(shown)
}

/// Is this conversation one of Rudder's own agent panes? Those live under
/// `.rudder-worktrees` and are branched in place with `b`, so they do not belong
/// in a picker whose whole purpose is adopting chats from OUTSIDE the dashboard.
pub(crate) fn is_worktree_conversation(cwd: &Path) -> bool {
    cwd.components()
        .any(|component| component.as_os_str() == ".rudder-worktrees")
}

/// A session id is spliced onto a `claude`/`codex` command line, and handoff requests
/// are files any process can drop into the repo. Accept only id-shaped strings.
pub(crate) fn valid_session_id(value: &str) -> bool {
    let value = value.trim();
    (8..=128).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Parse one queued request. Returns None for anything malformed or stale — a bad
/// file must never launch an agent, and must not wedge the queue either (the caller
/// deletes what it cannot parse).
pub(crate) fn parse_handoff_request(
    value: &serde_json::Value,
    now_ms: u64,
) -> Option<HandoffRequest> {
    let session_id = value
        .get("sessionId")
        .or_else(|| value.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| valid_session_id(id))?
        .to_string();
    let backend = value
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .map_or(Some(Backend::Claude), |raw| {
            Backend::parse(&raw.to_ascii_lowercase())
        })?;
    let target = value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .map_or(Some(HandoffTarget::Worker), HandoffTarget::parse)?;
    let instruction = value
        .get("instruction")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string);
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string);
    let created_at_ms = value
        .get("createdAt")
        .and_then(serde_json::Value::as_u64)
        .filter(|stamp| *stamp > 0);
    if let Some(created) = created_at_ms {
        if now_ms.saturating_sub(created) > REQUEST_MAX_AGE.as_millis() as u64 {
            return None;
        }
    }
    Some(HandoffRequest {
        session_id,
        backend,
        target,
        instruction,
        title,
        created_at_ms,
    })
}

/// Recent Claude conversations recorded for `cwd` (and its subdirectories), newest
/// first. Claude names each project folder after the directory the session ran in,
/// so a chat started in `src/` lives in a sibling folder — both are this repo's.
///
/// What is EXCLUDED matters as much as what is found. The picker is useless if it
/// is full of Rudder's own machinery: sessions inside `.rudder-worktrees` (already
/// agent panes — `b` branches those in place), the session ids of live agent rows,
/// and the one-shot `claude -p` calls Rudder makes for titles and completion notes.
pub(crate) fn recent_claude_conversations(
    cwd: &Path,
    limit: usize,
    exclude: &std::collections::HashSet<String>,
) -> Vec<ConversationCandidate> {
    let Some(home) = crate::cloudio::user_home_dir() else {
        return Vec::new();
    };
    recent_claude_conversations_in(&home.join(".claude").join("projects"), cwd, limit, exclude)
}

pub(crate) fn recent_claude_conversations_in(
    projects: &Path,
    cwd: &Path,
    limit: usize,
    exclude: &std::collections::HashSet<String>,
) -> Vec<ConversationCandidate> {
    if limit == 0 {
        return Vec::new();
    }
    let encoded = crate::encode_claude_project_dir(cwd);
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(projects) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let in_repo = name == encoded || name.starts_with(&format!("{encoded}-"));
        if !in_repo || name.contains("-rudder-worktrees-") {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let path = session.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            // Same cloud-offload guard as the Codex scan: a symlinked
            // transcript can block indefinitely while macOS asks a hung file
            // provider to materialize it. Skip; recency is what pickers want.
            if session.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            let modified = session
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((modified, path));
        }
    }
    files.sort_by(|left, right| right.0.cmp(&left.0));
    // Open a few times the requested count: the newest transcripts are often
    // Rudder's own one-shot calls, and dropping them must not empty the list.
    files.truncate(limit.saturating_mul(4));

    let mut interactive: Vec<ConversationCandidate> = Vec::new();
    let mut all: Vec<ConversationCandidate> = Vec::new();
    for (modified, path) in files {
        let Some(session_id) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|id| valid_session_id(id) && !exclude.contains(id))
        else {
            continue;
        };
        let Some(head) = scan_transcript_head(&path) else {
            continue;
        };
        let candidate = ConversationCandidate {
            session_id,
            backend: Backend::Claude,
            title: head.title,
            model: head.model,
            cwd: head.cwd,
            modified,
        };
        if head.interactive && interactive.len() < limit {
            interactive.push(candidate.clone());
        }
        if all.len() < limit {
            all.push(candidate);
        }
    }
    // Fall back to the unfiltered list rather than showing nothing: if Claude ever
    // stops recording the interactive marker, a noisy picker beats an empty one.
    if interactive.is_empty() {
        all
    } else {
        interactive
    }
}

/// Recent Codex conversations for `cwd`, newest first.
///
/// Codex writes one rollout JSONL per session under `~/.codex/sessions/<y>/<m>/<d>/`.
/// The first line is `session_meta` (id + cwd); the title is the first turn whose
/// role is `user` — `developer` turns are the harness's own context blocks.
pub(crate) fn recent_codex_conversations(cwd: &Path, limit: usize) -> Vec<ConversationCandidate> {
    let Some(home) = crate::cloudio::user_home_dir() else {
        return Vec::new();
    };
    recent_codex_conversations_in(&home.join(".codex").join("sessions"), cwd, limit)
}

pub(crate) fn recent_codex_conversations_in(
    root: &Path,
    cwd: &Path,
    limit: usize,
) -> Vec<ConversationCandidate> {
    if limit == 0 {
        return Vec::new();
    }
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_jsonl_files(root, &mut files, 0);
    files.sort_by(|left, right| right.0.cmp(&left.0));
    let target = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // Codex keeps ONE global session tree, so the newest rollouts are mostly other
    // repositories'. Filtering must therefore happen BEFORE any truncation: taking
    // "the newest N files" first showed this repo nothing while 52 of its
    // conversations sat further down the list.
    // Match on BOTH spellings of the path. Canonicalizing resolves symlinks
    // (`/tmp` -> `/private/tmp` on macOS), but the rollout recorded whatever the
    // shell was in — a substring test cannot canonicalize, so try each.
    let mut needles: Vec<String> = Vec::new();
    for path in [target.clone(), cwd.to_path_buf()] {
        let path = path.display().to_string();
        needles.push(format!("\"cwd\":\"{path}"));
        needles.push(format!("\"cwd\": \"{path}"));
    }
    let mut candidates: Vec<ConversationCandidate> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut examined = 0_usize;
    for (modified, path) in files {
        if candidates.len() >= limit || examined >= CODEX_SCAN_LIMIT {
            break;
        }
        examined += 1;
        // Cheap reject first: `session_meta` puts cwd in the first few hundred bytes,
        // while the rest of that line is the entire Codex system prompt.
        let Some(prefix) = read_head(&path, 4096) else {
            continue;
        };
        if !needles.iter().any(|needle| prefix.contains(needle)) {
            continue;
        }
        let Some(raw) = read_head(&path, HEAD_SCAN_BYTES as usize) else {
            continue;
        };
        let Some(head) = parse_codex_session_head(&raw) else {
            continue;
        };
        // The prefix match is a substring test ("/repo" also matches "/repo-other");
        // confirm on the parsed path.
        let session_cwd = std::fs::canonicalize(&head.cwd).unwrap_or(PathBuf::from(&head.cwd));
        if !session_cwd.starts_with(&target) && !Path::new(&head.cwd).starts_with(cwd) {
            continue;
        }
        // Rudder's own agent panes are branched with `b`, not adopted with
        // /resume. Claude's scan has always dropped them; Codex's did not, so a
        // worker's chat could be offered back as if it were the user's own.
        if is_worktree_conversation(&session_cwd) {
            continue;
        }
        // Resuming a Codex session writes ANOTHER rollout file with the same id;
        // the newest one wins (files are walked newest-first).
        if !seen.insert(head.session_id.clone()) {
            continue;
        }
        candidates.push(ConversationCandidate {
            session_id: head.session_id,
            backend: Backend::Codex,
            title: head.title,
            model: head.model,
            cwd: Some(PathBuf::from(&head.cwd)),
            modified,
        });
    }
    candidates
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip symlinks entirely: cloud-offload tools stow old rollouts behind
        // symlinks into a CloudStorage mount, and reading one can block
        // indefinitely while macOS asks a hung provider to materialize it.
        // This scan feeds the /resume palette on keystrokes; omitting an
        // archived chat beats wedging the dashboard. (lstat semantics — the
        // entry's file_type does not follow the link.)
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_jsonl_files(&path, out, depth + 1);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push((modified, path));
    }
}

/// Is this user turn the harness injecting an instructions file rather than a
/// person typing? Codex prefixes it with `# <FILE> instructions for <path>`.
pub(crate) fn is_injected_instructions(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with('#') && head.split_once(" instructions for ").is_some()
}

/// Prompts RUDDER writes to a backend itself. Claude's one-shot calls are caught
/// structurally (no session-mode marker), but a `codex exec` rollout is
/// indistinguishable from a real chat by metadata — `originator: codex-tui`,
/// `source: cli`, same as a person typing. So these are matched by their opening
/// text, which Rudder controls. Keep in sync with `tasks.rs`.
pub(crate) fn is_rudder_generated_prompt(text: &str) -> bool {
    const RUDDER_PROMPTS: [&str; 2] = [
        "Summarize this coding agent task for a compact sidebar label.",
        "A coding agent finished the task below but did not file a structured report.",
    ];
    let head = text.trim_start();
    RUDDER_PROMPTS.iter().any(|prompt| head.starts_with(prompt))
}

/// What the picker needs from a Codex rollout: which session, where it ran, and
/// what the human opened it with.
pub(crate) struct CodexSessionHead {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) title: String,
    /// From the rollout's `turn_context`, which records the model the turn ran on.
    pub(crate) model: Option<String>,
}

pub(crate) fn parse_codex_session_head(raw: &str) -> Option<CodexSessionHead> {
    let mut session_id = String::new();
    let mut cwd = String::new();
    let mut model: Option<String> = None;
    let mut title: Option<String> = None;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        let payload = value.get("payload");
        if kind == Some("session_meta") {
            if let Some(payload) = payload {
                // A subagent thread is the model talking to itself, like Claude's
                // sidechains: never a conversation to hand off.
                if payload.get("thread_source").and_then(serde_json::Value::as_str)
                    == Some("subagent")
                {
                    return None;
                }
                session_id = payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                cwd = payload
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
            continue;
        }
        if session_id.is_empty() {
            continue;
        }
        let Some(payload) = payload else { continue };
        if kind == Some("turn_context") {
            if model.is_none() {
                model = payload
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(ToString::to_string);
            }
            continue;
        }
        if payload.get("type").and_then(serde_json::Value::as_str) != Some("message")
            || payload.get("role").and_then(serde_json::Value::as_str) != Some("user")
        {
            continue;
        }
        let text = payload
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let cleaned = strip_wrapper_blocks(&text);
        // Codex opens every session by sending the repo's instructions file as a
        // USER turn ("# AGENTS.md instructions for /path" + an <INSTRUCTIONS>
        // block). Titling the conversation with that gives every row in the picker
        // the same useless name; the human's first turn is the one after it.
        if cleaned.is_empty()
            || is_injected_instructions(&cleaned)
            || is_rudder_generated_prompt(&cleaned)
        {
            continue;
        }
        if !valid_session_id(&session_id) || cwd.is_empty() {
            return None;
        }
        title = Some(cleaned);
        // `turn_context` can follow the first user turn, so keep reading a little
        // for the model rather than stopping the moment the title is known.
        if model.is_some() {
            break;
        }
    }
    let title = title?;
    Some(CodexSessionHead {
        session_id,
        cwd,
        title,
        model,
    })
}

/// What a (possibly partial) session id resolved to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionLookup {
    Found {
        backend: Backend,
        session_id: String,
    },
    /// The prefix matches more than one session; the user must be more specific.
    Ambiguous(Vec<String>),
    Missing,
}

/// Resolve a session id that may have been TRUNCATED to the real one.
///
/// Every TUI that displays a session id cuts it to fit the pane, so what a user
/// copies is routinely a prefix ("019f7cde-6d6f-7be0-8359-67e" — 27 of 36 chars).
/// Accepting that and handing it to the backend fails at launch with "No saved
/// session found with ID …", which reads like Rudder losing the conversation.
/// Resolving the prefix here turns a dead end into the thing the user meant.
pub(crate) fn resolve_session(cwd: &Path, partial: &str) -> SessionLookup {
    let partial = partial.trim();
    if !valid_session_id(partial) {
        return SessionLookup::Missing;
    }
    // An exact hit wins outright: no scanning, no ambiguity.
    if crate::claude_transcript_path(cwd, partial).is_some() {
        return SessionLookup::Found {
            backend: Backend::Claude,
            session_id: partial.to_string(),
        };
    }
    let mut matches: Vec<(Backend, String)> = Vec::new();
    for (backend, id) in claude_session_ids_starting_with(cwd, partial)
        .into_iter()
        .map(|id| (Backend::Claude, id))
        .chain(
            codex_session_ids_starting_with(partial)
                .into_iter()
                .map(|id| (Backend::Codex, id)),
        )
        .chain(
            opencode_session_ids_starting_with(partial)
                .into_iter()
                .map(|id| (Backend::Opencode, id)),
        )
    {
        if id == partial {
            return SessionLookup::Found {
                backend,
                session_id: id,
            };
        }
        if !matches.iter().any(|(_, seen)| seen == &id) {
            matches.push((backend, id));
        }
    }
    match matches.len() {
        0 => SessionLookup::Missing,
        1 => {
            let (backend, session_id) = matches.remove(0);
            SessionLookup::Found {
                backend,
                session_id,
            }
        }
        _ => SessionLookup::Ambiguous(matches.into_iter().map(|(_, id)| id).collect()),
    }
}

fn claude_session_ids_starting_with(cwd: &Path, partial: &str) -> Vec<String> {
    let Some(home) = crate::cloudio::user_home_dir() else {
        return Vec::new();
    };
    claude_session_ids_starting_with_in(&home.join(".claude").join("projects"), cwd, partial)
}

pub(crate) fn claude_session_ids_starting_with_in(
    projects: &Path,
    cwd: &Path,
    partial: &str,
) -> Vec<String> {
    let encoded = crate::encode_claude_project_dir(cwd);
    let Ok(entries) = std::fs::read_dir(projects) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name != encoded && !name.starts_with(&format!("{encoded}-")) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let path = session.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.starts_with(partial) && valid_session_id(stem) {
                found.push(stem.to_string());
            }
        }
    }
    found
}

/// Codex names each rollout `rollout-<timestamp>-<session id>.jsonl`, so the id can
/// be read off the filename without opening 1200 files.
fn codex_session_ids_starting_with(partial: &str) -> Vec<String> {
    let Some(home) = crate::cloudio::user_home_dir() else {
        return Vec::new();
    };
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_jsonl_files(&home.join(".codex").join("sessions"), &mut files, 0);
    let mut found = Vec::new();
    for (_, path) in files {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(id) = codex_session_id_from_file_stem(stem) else {
            continue;
        };
        if id.starts_with(partial) && !found.contains(&id) {
            found.push(id);
        }
    }
    found
}

/// The trailing UUID of `rollout-2026-07-19T17-12-59-019f7cde-…-67e68c2deec6`.
pub(crate) fn codex_session_id_from_file_stem(stem: &str) -> Option<String> {
    let parts: Vec<&str> = stem.rsplitn(6, '-').collect();
    if parts.len() < 6 {
        return None;
    }
    // rsplitn yields the tail first; the id is those five groups back in order.
    let id = parts[..5]
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("-");
    let groups: Vec<&str> = id.split('-').collect();
    let shaped = groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, group)| group.len() == *len)
        && groups
            .iter()
            .all(|group| group.chars().all(|ch| ch.is_ascii_hexdigit()));
    shaped.then_some(id)
}

fn opencode_session_ids_starting_with(partial: &str) -> Vec<String> {
    let Some(raw) = opencode_session_list_json(200) else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|session| session.get("id").and_then(serde_json::Value::as_str))
        .filter(|id| id.starts_with(partial))
        .map(ToString::to_string)
        .collect()
}

/// Does `~/.codex/sessions` hold a rollout for this id? Codex records the session id
/// in the FILENAME, so a walk beats opening every transcript. Used to tell a real
/// Codex session apart from a mistyped id, which would otherwise launch a workspace
/// for a conversation that does not exist.
pub(crate) fn codex_session_exists(session_id: &str) -> bool {
    let Some(home) = crate::cloudio::user_home_dir() else {
        return false;
    };
    codex_session_exists_in(&home.join(".codex").join("sessions"), session_id)
}

pub(crate) fn codex_session_exists_in(root: &Path, session_id: &str) -> bool {
    fn walk(dir: &Path, needle: &str, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if walk(&path, needle, depth + 1) {
                    return true;
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(needle))
            {
                return true;
            }
        }
        false
    }
    valid_session_id(session_id) && walk(root, session_id.trim(), 0)
}

/// Recent opencode conversations for `cwd`, newest first.
///
/// opencode keeps its sessions in a database rather than per-project transcript
/// files, and ships the query: `opencode session list --format json` reports id,
/// title, directory and update time. Rudder shells out for it (throttled, never on
/// the render path) instead of reading a schema it does not own.
pub(crate) fn recent_opencode_conversations(cwd: &Path, limit: usize) -> Vec<ConversationCandidate> {
    let Some(raw) = opencode_session_list_json(limit.saturating_mul(4).max(limit)) else {
        return Vec::new();
    };
    parse_opencode_sessions(&raw, cwd, limit)
}

/// Is this a real opencode session? Guards `/handoff <id>` against a mistyped id
/// creating a workspace for a conversation that cannot be forked.
pub(crate) fn opencode_session_exists(session_id: &str) -> bool {
    if !valid_session_id(session_id) {
        return false;
    }
    let Some(raw) = opencode_session_list_json(200) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| {
            let sessions = value.as_array()?.clone();
            Some(sessions.iter().any(|session| {
                session.get("id").and_then(serde_json::Value::as_str) == Some(session_id.trim())
            }))
        })
        .unwrap_or(false)
}

fn opencode_session_list_json(limit: usize) -> Option<String> {
    // Tests must not shell out to a binary that may not be installed, and no unit
    // test needs the live list — `parse_opencode_sessions` is tested directly.
    if cfg!(test) {
        return None;
    }
    let output = std::process::Command::new(crate::launch::opencode_program())
        .args(["session", "list", "--format", "json", "-n"])
        .arg(limit.to_string())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `opencode session list --format json`, keeping the sessions that ran in
/// this repository (opencode records each session's directory).
pub(crate) fn parse_opencode_sessions(
    raw: &str,
    cwd: &Path,
    limit: usize,
) -> Vec<ConversationCandidate> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let Some(sessions) = value.as_array() else {
        return Vec::new();
    };
    let mut candidates: Vec<ConversationCandidate> = sessions
        .iter()
        .filter_map(|session| {
            let directory = session.get("directory").and_then(serde_json::Value::as_str)?;
            if !Path::new(directory).starts_with(cwd) || is_worktree_conversation(Path::new(directory))
            {
                return None;
            }
            let session_id = session.get("id").and_then(serde_json::Value::as_str)?;
            if !valid_session_id(session_id) {
                return None;
            }
            let title = session
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())?;
            let updated = session
                .get("updated")
                .or_else(|| session.get("created"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(ConversationCandidate {
                session_id: session_id.to_string(),
                backend: Backend::Opencode,
                title: title.to_string(),
                // opencode's session list carries no model, and Rudder does not
                // read its database: better blank than invented.
                model: None,
                cwd: Some(PathBuf::from(directory)),
                modified: SystemTime::UNIX_EPOCH + Duration::from_millis(updated),
            })
        })
        .collect();
    candidates.sort_by(|left, right| right.modified.cmp(&left.modified));
    candidates.truncate(limit);
    candidates
}

/// The opening prompt of one specific conversation, used to label a handed-off row.
pub(crate) fn conversation_title(cwd: &Path, session_id: &str) -> Option<String> {
    let path = crate::claude_transcript_path(cwd, session_id)?;
    Some(scan_transcript_head(&path)?.title)
}

/// Read at most `max_bytes` of a file as lossy UTF-8, for the parsers that want the
/// head as one string rather than a line stream.
fn read_head(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0_u8; max_bytes];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// What the picker needs from a transcript, read in ONE bounded pass from the top.
pub(crate) struct TranscriptHead {
    /// The conversation's opening user prompt.
    pub(crate) title: String,
    /// The model on the first assistant reply — what the chat is actually running.
    pub(crate) model: Option<String>,
    /// Whether this was a REPL session a human talked to. Claude records its mode at
    /// session start; the one-shot `claude -p` calls Rudder makes internally (task
    /// titles, completion notes, the plan decomposer) do not, which is exactly how
    /// they are kept out of the picker.
    pub(crate) interactive: bool,
    /// The directory the session ran in. Claude stamps it on every entry.
    pub(crate) cwd: Option<PathBuf>,
}

fn scan_transcript_head(path: &Path) -> Option<TranscriptHead> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(std::io::Read::take(file, HEAD_SCAN_BYTES));
    scan_transcript_lines(reader.lines().map_while(Result::ok))
}

/// The head scan over already-read lines, so the rules are testable without a file.
pub(crate) fn scan_transcript_lines(
    lines: impl Iterator<Item = impl AsRef<str>>,
) -> Option<TranscriptHead> {
    let mut interactive = false;
    let mut title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    for line in lines.take(HEAD_SCAN_LINES) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.as_ref()) else {
            continue;
        };
        interactive |= is_interactive_marker(&value);
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
        }
        if title.is_none() {
            // The mode markers are written before the first user turn, so by the
            // time a title exists the interactive question is already answered.
            title = user_prompt_from_entry(&value)
                .filter(|prompt| !is_rudder_generated_prompt(prompt));
        }
        if model.is_none() {
            model = value
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string);
        }
        // The model lands on the first assistant reply, just after the opening
        // prompt; stop once all three are known rather than reading the whole head.
        if title.is_some() && model.is_some() && cwd.is_some() {
            break;
        }
    }
    title.map(|title| TranscriptHead {
        title,
        model,
        interactive,
        cwd,
    })
}

fn is_interactive_marker(value: &serde_json::Value) -> bool {
    matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("mode") | Some("permission-mode")
    )
}

/// One transcript entry's user prose, or None when it is not a real user turn.
/// Skips sidechain (subagent) turns, meta entries, and the wrapper blocks Claude
/// Code injects around user text (`<system-reminder>`, slash-command echoes, the
/// local-command caveat).
fn user_prompt_from_entry(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("user") {
        return None;
    }
    if flag(value, "isSidechain") || flag(value, "isMeta") {
        return None;
    }
    let text = user_message_text(value.get("message")?);
    let cleaned = strip_wrapper_blocks(&text);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn flag(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// `message.content` is either a plain string or a block array; only text blocks
/// carry user prose (tool results are noise for a title).
fn user_message_text(message: &serde_json::Value) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop `<tag>…</tag>` wrapper blocks (possibly multi-line, possibly with
/// attributes) and caveat lines, then collapse what is left into a single line.
pub(crate) fn strip_wrapper_blocks(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut closing: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(tag) = closing.clone() {
            if trimmed.ends_with(&tag) {
                closing = None;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("Caveat:") {
            continue;
        }
        if let Some(tag) = opening_tag(trimmed) {
            let close = format!("</{tag}>");
            if !trimmed.contains(&close) {
                closing = Some(close);
            }
            continue;
        }
        kept.push(trimmed);
    }
    kept.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The tag name of a line that OPENS an XML-ish wrapper block: `<system-reminder>`,
/// and attributed forms such as `<teammate-message teammate_id="lead">`.
fn opening_tag(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('<')?;
    let end = rest.find('>')?;
    let inside = &rest[..end];
    let name = inside.split_whitespace().next().unwrap_or_default();
    if name.is_empty() || name.starts_with('/') {
        return None;
    }
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        .then_some(name)
}

/// Compact age for the picker ("just now", "12m", "3h", "2d").
pub(crate) fn relative_age(modified: SystemTime, now: SystemTime) -> String {
    let seconds = now
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs();
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The orientation note prepended to a handoff's first prompt. The forked
/// conversation remembers the MAIN checkout it was having; it now runs in an
/// isolated workspace at a different path, and saying so up front prevents it from
/// hunting for edits it "already made" in files it can no longer see.
pub(crate) fn worker_orientation(workspace: &Path, instruction: &str) -> String {
    format!(
        "You are continuing this same conversation, but now inside a Rudder worker workspace at {} \
(an isolated jj workspace of the same repository, seeded from the current change). Paths and \
uncommitted edits may differ from the checkout we were just in — re-read a file before assuming \
its contents. Rudder merges this workspace back when the work is done.\n\nNext: {}",
        workspace.display(),
        instruction.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_user_prompt_skips_wrappers_sidechains_and_meta() {
        let raw = [
            r#"{"type":"mode","mode":"normal"}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta noise"}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent task"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<system-reminder>\nignore me\n</system-reminder>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"add a handoff command"}}"#,
        ]
        .join("\n");
        assert_eq!(
            scan_transcript_lines(raw.lines()).map(|head| head.title).as_deref(),
            Some("add a handoff command")
        );
    }

    #[test]
    fn first_user_prompt_reads_text_blocks_and_ignores_tool_results() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"noise"},{"type":"text","text":"fix the diff pane"}]}}"#;
        assert_eq!(
            scan_transcript_lines(raw.lines()).map(|head| head.title).as_deref(),
            Some("fix the diff pane")
        );
    }

    #[test]
    fn strip_wrapper_blocks_drops_multiline_blocks_and_caveats() {
        let text = "Caveat: local command\n<command-name>/model</command-name>\n<system-reminder>\nline one\nline two\n</system-reminder>\nreal   question here";
        assert_eq!(strip_wrapper_blocks(text), "real question here");
    }

    #[test]
    fn parse_handoff_request_defaults_to_a_claude_worker() {
        let value = serde_json::json!({ "sessionId": "3503304e-5818-45d5-8b5b-4ea15a857e09" });
        let request = parse_handoff_request(&value, 0).expect("parsed");
        assert_eq!(request.backend, Backend::Claude);
        assert_eq!(request.target, HandoffTarget::Worker);
        assert_eq!(request.instruction, None);
    }

    #[test]
    fn parse_handoff_request_rejects_shell_shaped_session_ids() {
        // The id is spliced onto a claude/codex command line; anything but an
        // id-shaped token must be refused rather than escaped-and-hoped-for.
        for bad in ["", "short", "abc; rm -rf /", "../../etc/passwd"] {
            let value = serde_json::json!({ "sessionId": bad });
            assert!(
                parse_handoff_request(&value, 0).is_none(),
                "rejected {bad:?}"
            );
        }
    }

    #[test]
    fn parse_handoff_request_drops_stale_queue_entries() {
        let value = serde_json::json!({
            "sessionId": "3503304e-5818-45d5-8b5b-4ea15a857e09",
            "createdAt": 1_000_u64,
        });
        let fresh = 1_000 + REQUEST_MAX_AGE.as_millis() as u64;
        assert!(parse_handoff_request(&value, fresh).is_some(), "at the edge");
        assert!(
            parse_handoff_request(&value, fresh + 1).is_none(),
            "past the edge"
        );
    }

    #[test]
    fn parse_handoff_request_reads_target_and_instruction() {
        let value = serde_json::json!({
            "sessionId": "3503304e-5818-45d5-8b5b-4ea15a857e09",
            "backend": "codex",
            "target": "here",
            "instruction": "  ship it  ",
        });
        let request = parse_handoff_request(&value, 0).expect("parsed");
        assert_eq!(request.backend, Backend::Codex);
        assert_eq!(request.target, HandoffTarget::Here);
        assert_eq!(request.instruction.as_deref(), Some("ship it"));
    }

    /// Build a `~/.claude/projects` tree; returns (root, projects, cwd).
    fn scratch_projects(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "rudder-handoff-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        (root.join("projects"), root.join("repo"), root)
    }

    /// One transcript: `interactive` writes the session-mode marker a REPL records
    /// and a one-shot `claude -p` call does not.
    fn write_transcript(dir: &Path, session_id: &str, prompt: &str, interactive: bool) {
        write_transcript_from(dir, session_id, prompt, interactive, None);
    }

    /// Same, but stamping the directory Claude recorded the session in — which is
    /// what the picker reads to say `main` or a subfolder.
    fn write_transcript_from(
        dir: &Path,
        session_id: &str,
        prompt: &str,
        interactive: bool,
        cwd: Option<&Path>,
    ) {
        std::fs::create_dir_all(dir).expect("create project dir");
        let mut lines = Vec::new();
        if interactive {
            lines.push(r#"{"type":"mode","mode":"normal"}"#.to_string());
        }
        let stamp = cwd
            .map(|path| format!(r#""cwd":{},"#, serde_json::json!(path.display().to_string())))
            .unwrap_or_default();
        lines.push(format!(
            r#"{{{stamp}"type":"user","message":{{"role":"user","content":"{prompt}"}}}}"#
        ));
        std::fs::write(dir.join(format!("{session_id}.jsonl")), lines.join("\n")).expect("write");
    }

    fn titles(candidates: &[ConversationCandidate]) -> Vec<&str> {
        candidates
            .iter()
            .map(|candidate| candidate.title.as_str())
            .collect()
    }

    #[test]
    fn recent_conversations_scan_subdirs_but_skip_rudder_worktrees() {
        let (projects, cwd, root) = scratch_projects("scan");
        let encoded = crate::encode_claude_project_dir(&cwd);
        write_transcript(
            &projects.join(&encoded),
            "11111111-1111-1111-1111-111111111111",
            "repo root chat",
            true,
        );
        write_transcript(
            &projects.join(format!("{encoded}-src")),
            "22222222-2222-2222-2222-222222222222",
            "subdirectory chat",
            true,
        );
        write_transcript(
            &projects.join(format!("{encoded}--rudder-worktrees-n0")),
            "33333333-3333-3333-3333-333333333333",
            "worker pane chat",
            true,
        );
        write_transcript(
            &projects.join("-Users-someone-else"),
            "44444444-4444-4444-4444-444444444444",
            "someone else's repo",
            true,
        );

        let found =
            recent_claude_conversations_in(&projects, &cwd, 10, &std::collections::HashSet::new());
        let titles = titles(&found);
        assert!(titles.contains(&"repo root chat"), "{titles:?}");
        assert!(titles.contains(&"subdirectory chat"), "{titles:?}");
        assert!(
            !titles.contains(&"worker pane chat"),
            "rudder's own agent sessions are not handoff candidates: {titles:?}"
        );
        assert!(
            !titles.contains(&"someone else's repo"),
            "other repos stay out: {titles:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_claude_candidate_remembers_which_directory_it_ran_in() {
        let (projects, cwd, root) = scratch_projects("origin");
        let encoded = crate::encode_claude_project_dir(&cwd);
        write_transcript_from(
            &projects.join(&encoded),
            "11111111-1111-1111-1111-111111111111",
            "repo root chat",
            true,
            Some(&cwd),
        );
        write_transcript_from(
            &projects.join(format!("{encoded}-src")),
            "22222222-2222-2222-2222-222222222222",
            "subdirectory chat",
            true,
            Some(&cwd.join("src")),
        );

        let found =
            recent_claude_conversations_in(&projects, &cwd, 10, &std::collections::HashSet::new());
        let origin = |title: &str| {
            found
                .iter()
                .find(|candidate| candidate.title == title)
                .and_then(|candidate| origin_label(candidate.cwd.as_deref(), &cwd))
        };

        assert_eq!(origin("repo root chat").as_deref(), Some("main"));
        assert_eq!(origin("subdirectory chat").as_deref(), Some("src"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_args_take_a_here_flag_on_either_side_of_the_id() {
        // Default: an isolated worker, same as bare task input.
        let (target, id, instruction) = parse_resume_args(" abc12345 write the tests ");
        assert_eq!(target, HandoffTarget::Worker);
        assert_eq!(id, "abc12345");
        assert_eq!(instruction, "write the tests");

        // Trailing is the likelier spelling: the palette inserts the id and the
        // user keeps typing after it.
        let (target, id, instruction) = parse_resume_args("abc12345 --here write the tests");
        assert_eq!(target, HandoffTarget::Here);
        assert_eq!(id, "abc12345");
        assert_eq!(instruction, "write the tests");

        let (target, id, _) = parse_resume_args("--main abc12345");
        assert_eq!(target, HandoffTarget::Here);
        assert_eq!(id, "abc12345");

        // Deeper in the sentence it is the user's prose, not a switch: an agent
        // told to "explain why --here exists" must still run where it was sent.
        let (target, _, instruction) = parse_resume_args("abc12345 explain why --here exists");
        assert_eq!(target, HandoffTarget::Worker);
        assert_eq!(instruction, "explain why --here exists");
    }

    #[test]
    fn origin_label_names_main_and_subdirectories() {
        let root = std::env::temp_dir().join(format!("rudder-origin-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("create");

        assert_eq!(origin_label(Some(&root), &root).as_deref(), Some("main"));
        assert_eq!(
            origin_label(Some(&root.join("src")), &root).as_deref(),
            Some("src")
        );
        // Nothing recorded means nothing shown; the row must not invent a place.
        assert_eq!(origin_label(None, &root), None);
        // Outside the repo entirely: no relative reading exists, so no label.
        assert_eq!(origin_label(Some(Path::new("/elsewhere")), &root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn opencode_scan_drops_rudders_own_worker_chats() {
        // Claude's scan has always dropped worker panes; opencode's kept them, so
        // an agent's own chat could be offered back as if the user had started it.
        let raw = serde_json::json!([
            {"id": "aaaaaaaa-1111", "title": "my own chat", "directory": "/repo", "updated": 20},
            {"id": "bbbbbbbb-2222", "title": "worker pane chat",
             "directory": "/repo/.rudder-worktrees/repo-abc/n0", "updated": 30},
        ])
        .to_string();

        let found = parse_opencode_sessions(&raw, Path::new("/repo"), 10);

        assert_eq!(titles(&found), vec!["my own chat"]);
        assert_eq!(found[0].cwd.as_deref(), Some(Path::new("/repo")));
    }

    #[test]
    fn recent_conversations_hide_rudders_own_one_shot_calls_and_live_agents() {
        let (projects, cwd, root) = scratch_projects("noise");
        let dir = projects.join(crate::encode_claude_project_dir(&cwd));
        write_transcript(
            &dir,
            "11111111-1111-1111-1111-111111111111",
            "a real conversation",
            true,
        );
        // Rudder titles tasks and writes completion notes with `claude -p`. Those
        // transcripts land in the same folder and would otherwise bury the picker.
        write_transcript(
            &dir,
            "22222222-2222-2222-2222-222222222222",
            "Summarize this coding agent task for a compact sidebar label.",
            false,
        );
        // A live agent pane's own session: already on screen, branch it with `b`.
        write_transcript(
            &dir,
            "33333333-3333-3333-3333-333333333333",
            "an agent already running in a pane",
            true,
        );
        let mine = std::collections::HashSet::from([
            "33333333-3333-3333-3333-333333333333".to_string()
        ]);

        let found = recent_claude_conversations_in(&projects, &cwd, 10, &mine);

        assert_eq!(titles(&found), vec!["a real conversation"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_rollout_head_yields_the_session_its_cwd_and_the_humans_first_turn() {
        // `developer` turns are the harness's own context blocks (permissions, app
        // context); titling a conversation with those would make the picker useless.
        let raw = [
            r#"{"type":"session_meta","payload":{"id":"019cb763-9efd-7320-b337-64233ca29d6e","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>\nsandbox stuff\n</permissions instructions>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"port the cache to the new store"}]}}"#,
        ]
        .join("\n");

        let head = parse_codex_session_head(&raw).expect("parsed");

        assert_eq!(head.session_id, "019cb763-9efd-7320-b337-64233ca29d6e");
        assert_eq!(head.cwd, "/repo");
        assert_eq!(head.title, "port the cache to the new store");
    }

    #[test]
    fn codex_session_ids_are_read_off_the_rollout_filename() {
        assert_eq!(
            codex_session_id_from_file_stem(
                "rollout-2026-07-19T17-12-59-019f7cde-6d6f-7be0-8359-67e68c2deec6"
            )
            .as_deref(),
            Some("019f7cde-6d6f-7be0-8359-67e68c2deec6")
        );
        // A timestamp alone is not a session id.
        assert_eq!(codex_session_id_from_file_stem("rollout-2026-07-19T17-12-59"), None);
        assert_eq!(codex_session_id_from_file_stem("nonsense"), None);
    }

    #[test]
    fn a_truncated_session_id_resolves_to_the_real_one() {
        // Every TUI cuts the id to fit its pane, so users paste a PREFIX. Handing
        // that prefix to the backend fails with "No saved session found with ID …",
        // which reads like Rudder losing the conversation.
        let root = std::env::temp_dir().join(format!(
            "rudder-handoff-prefix-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let cwd = root.join("repo");
        let projects = root.join("projects");
        let dir = projects.join(crate::encode_claude_project_dir(&cwd));
        write_transcript(
            &dir,
            "3503304e-5818-45d5-8b5b-4ea15a857e09",
            "the chat I want back",
            true,
        );

        let ids = claude_session_ids_starting_with_in(&projects, &cwd, "3503304e-5818-45d5");

        assert_eq!(ids, vec!["3503304e-5818-45d5-8b5b-4ea15a857e09".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_titles_skip_the_harness_and_rudders_own_prompts() {
        // Codex opens every session with the repo's instructions file as a USER
        // turn, and Rudder itself drives `codex exec` for task titles — neither is
        // a conversation a human would recognize in a picker.
        let raw = [
            r#"{"type":"session_meta","payload":{"id":"019cb763-9efd-7320-b337-64233ca29d6e","cwd":"/repo"}}"#,
            // r##"…"## because the JSON contains `"#` (from "# AGENTS.md"), which
            // would close a plain r#"…"# raw string.
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /repo\n<INSTRUCTIONS>\nbe good\n</INSTRUCTIONS>"}]}}"##,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize this coding agent task for a compact sidebar label. Return exactly one JSON object"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"rudder eats too much CPU"}]}}"#,
        ]
        .join("\n");

        let head = parse_codex_session_head(&raw).expect("parsed");

        assert_eq!(head.title, "rudder eats too much CPU");
    }

    #[test]
    fn codex_subagent_threads_are_not_conversations() {
        // The model talking to itself, like Claude's sidechains.
        let raw = [
            r#"{"type":"session_meta","payload":{"id":"019cb763-9efd-7320-b337-64233ca29d6e","cwd":"/repo","thread_source":"subagent"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"audit the routing layer"}]}}"#,
        ]
        .join("\n");

        assert!(parse_codex_session_head(&raw).is_none());
    }

    #[test]
    fn one_resumed_codex_session_is_one_row() {
        // Resuming writes a NEW rollout file under the SAME session id; the picker
        // showed the same conversation four times before this.
        let root = std::env::temp_dir().join(format!(
            "rudder-handoff-codex-dupe-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let day = root.join("2026").join("07").join("20");
        std::fs::create_dir_all(&day).expect("create session dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        let rollout = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019cb763-9efd-7320-b337-64233ca29d6e\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"the long running chat\"}}]}}}}",
            repo.to_string_lossy()
        );
        for part in 0..4 {
            std::fs::write(day.join(format!("rollout-{part}.jsonl")), &rollout).expect("write");
        }

        let found = recent_codex_conversations_in(&root, &repo, 10);

        assert_eq!(found.len(), 1, "one session, one row: {found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_rollouts_outside_this_repo_are_not_candidates() {
        let root = std::env::temp_dir().join(format!(
            "rudder-handoff-codex-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let day = root.join("2026").join("03").join("03");
        std::fs::create_dir_all(&day).expect("create session dir");
        let rollout = |id: &str, cwd: &str, prompt: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{prompt}\"}}]}}}}"
            )
        };
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        std::fs::write(
            day.join("rollout-here.jsonl"),
            rollout(
                "019cb763-9efd-7320-b337-64233ca29d6e",
                repo.to_string_lossy().as_ref(),
                "in this repo",
            ),
        )
        .expect("write");
        std::fs::write(
            day.join("rollout-elsewhere.jsonl"),
            rollout(
                "019cb763-9efd-7320-b337-64233ca29d6f",
                "/somewhere/else",
                "another repo",
            ),
        )
        .expect("write");

        let found = recent_codex_conversations_in(&root, &repo, 10);

        assert_eq!(
            found
                .iter()
                .map(|candidate| candidate.title.as_str())
                .collect::<Vec<_>>(),
            vec!["in this repo"]
        );
        assert_eq!(found[0].backend, Backend::Codex);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_sessions_are_found_under_a_pile_of_other_repos_rollouts() {
        // Codex writes every repository's sessions into ONE tree. Truncating to the
        // newest N files before filtering hid this repo's conversations entirely.
        let root = std::env::temp_dir().join(format!(
            "rudder-handoff-codex-pile-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let day = root.join("2026").join("03").join("03");
        std::fs::create_dir_all(&day).expect("create session dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        let rollout = |id: &str, cwd: &str, prompt: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{prompt}\"}}]}}}}"
            )
        };
        // The one that matters is written FIRST, so it is the OLDEST of the pile.
        std::fs::write(
            day.join("rollout-mine.jsonl"),
            rollout(
                "019cb763-9efd-7320-b337-64233ca29d6e",
                repo.to_string_lossy().as_ref(),
                "the conversation I want back",
            ),
        )
        .expect("write");
        for index in 0..40 {
            std::fs::write(
                day.join(format!("rollout-other-{index}.jsonl")),
                rollout(
                    &format!("019cb763-9efd-7320-b337-64233ca29{index:03}"),
                    "/somewhere/else",
                    "another repo's chat",
                ),
            )
            .expect("write");
        }

        let found = recent_codex_conversations_in(&root, &repo, 8);

        assert_eq!(
            found
                .iter()
                .map(|candidate| candidate.title.as_str())
                .collect::<Vec<_>>(),
            vec!["the conversation I want back"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn opencode_sessions_are_filtered_to_this_repo_and_ordered_by_recency() {
        let raw = serde_json::json!([
            {
                "id": "ses_0000000000000000000000001",
                "title": "older chat here",
                "updated": 1_000_u64,
                "directory": "/repo",
            },
            {
                "id": "ses_0000000000000000000000002",
                "title": "newest chat here",
                "updated": 9_000_u64,
                "directory": "/repo/src",
            },
            {
                "id": "ses_0000000000000000000000003",
                "title": "someone else's repo",
                "updated": 9_999_u64,
                "directory": "/other",
            },
            // No title yet (a session opened but never used) — nothing to show.
            {
                "id": "ses_0000000000000000000000004",
                "updated": 9_998_u64,
                "directory": "/repo",
            },
        ])
        .to_string();

        let found = parse_opencode_sessions(&raw, Path::new("/repo"), 10);

        assert_eq!(
            found
                .iter()
                .map(|candidate| candidate.title.as_str())
                .collect::<Vec<_>>(),
            vec!["newest chat here", "older chat here"]
        );
        assert!(found.iter().all(|candidate| candidate.backend == Backend::Opencode));
    }

    #[test]
    fn recent_conversations_fall_back_rather_than_showing_nothing() {
        // If Claude ever stops recording the interactive marker, every transcript
        // looks like a one-shot call. A noisy picker still beats an empty one.
        let (projects, cwd, root) = scratch_projects("fallback");
        let dir = projects.join(crate::encode_claude_project_dir(&cwd));
        write_transcript(
            &dir,
            "11111111-1111-1111-1111-111111111111",
            "unmarked but real",
            false,
        );

        let found =
            recent_claude_conversations_in(&projects, &cwd, 10, &std::collections::HashSet::new());

        assert_eq!(titles(&found), vec!["unmarked but real"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn now_millis() -> u128 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }
}
