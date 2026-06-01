//! Live planner transcript: parse a backend's JSON event stream (Claude
//! `--output-format stream-json` or Codex `exec --json`) incrementally out of the
//! orchestrator PTY's `output_log`, into (1) a human-readable conversation
//! transcript shown live while planning and (2) the reconstructed assistant TEXT
//! that the `RUDDER_PLAN_TASKS` parser reads. This is what lets the orchestrator
//! pane stream the model thinking + inspecting files + writing the plan, instead
//! of sitting on a silent spinner while `claude -p` hides its thinking.
//!
//! The two backends emit different envelopes but we dispatch both off the line's
//! `type` field (their type strings do not collide), feeding one transcript model.
//! Mirrors the text-accumulation semantics of `src/backends.ts` `textFromBackendData`.

use serde_json::Value;

use crate::tasks::strip_ansi_for_plan;

/// Kind of a rendered transcript entry. Drives styling in the orchestrator pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanEntryKind {
    /// The model's reasoning (compact, dim). Never fed to the DAG parser.
    Thinking,
    /// Streamed assistant narration / the plan being written.
    Text,
    /// A tool/inspection step, e.g. "Reading mathutils.py", "Grep TODO".
    Tool,
    /// System/init or a backend error line.
    System,
    /// A user follow-up turn the human typed into the orchestrator chat.
    UserTurn,
}

#[derive(Debug, Clone)]
pub(crate) struct PlanEntry {
    pub(crate) kind: PlanEntryKind,
    pub(crate) text: String,
}

/// Incremental planner stream parser. One per orchestrator `AgentRun`.
pub(crate) struct PlanStreamState {
    /// Concatenated assistant TEXT across all turns. The RUDDER_PLAN_TASKS block is
    /// plain assistant text, so this is what `extract_rudder_plan_tasks` reads.
    assistant_text: String,
    /// Ordered, human-readable transcript for the live pane.
    transcript: Vec<PlanEntry>,
    /// The backend session/thread id, captured from the stream so a refine can
    /// `--resume` (Claude) / `exec resume` (Codex) the same conversation.
    session_id: Option<String>,
    /// True once the CURRENT turn produced streaming text deltas; gates the
    /// assistant-message/result fallbacks so we never double-append (mirrors
    /// `sawStreamingText` in backends.ts).
    saw_streaming_text: bool,
    /// Byte offset into `assistant_text` marking the start of the current turn.
    /// Block detection only considers text at/after this, so a refine's re-emitted
    /// block is captured rather than the previous turn's stale block.
    parse_baseline: usize,
    /// Bytes of the latest snapshot already split into complete lines.
    consumed: usize,
}

const MAX_TRANSCRIPT: usize = 400;
const MAX_THINKING_CHARS: usize = 2000;

impl Default for PlanStreamState {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanStreamState {
    pub(crate) fn new() -> Self {
        Self {
            assistant_text: String::new(),
            transcript: Vec::new(),
            session_id: None,
            saw_streaming_text: false,
            parse_baseline: 0,
            consumed: 0,
        }
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn transcript(&self) -> &[PlanEntry] {
        &self.transcript
    }

    /// Full reconstructed assistant text (all turns). Kept for tests/diagnostics.
    #[allow(dead_code)]
    pub(crate) fn assistant_text(&self) -> &str {
        &self.assistant_text
    }

    /// The text the RUDDER_PLAN_TASKS parser should read: only the CURRENT turn
    /// (at/after the per-turn baseline), so a stale prior block is not re-captured.
    pub(crate) fn parse_text(&self) -> &str {
        let start = self.parse_baseline.min(self.assistant_text.len());
        &self.assistant_text[start..]
    }

    pub(crate) fn has_text(&self) -> bool {
        !self.assistant_text.is_empty()
    }

    /// Begin a new turn: record the user's follow-up in the transcript and move the
    /// parse baseline to the current end so detection waits for the revised block.
    pub(crate) fn begin_user_turn(&mut self, feedback: &str) {
        let trimmed = feedback.trim();
        if !trimmed.is_empty() {
            self.push(PlanEntryKind::UserTurn, trimmed);
        }
        self.parse_baseline = self.assistant_text.len();
        self.saw_streaming_text = false;
    }

    /// Re-point ingest at a NEW underlying terminal (a refine relaunches a fresh PTY)
    /// WITHOUT dropping the accumulated transcript/text/session, so the orchestrator
    /// pane keeps the conversation instead of wiping. The next ingest reads the new
    /// terminal's output_log from the start. Pair with `begin_user_turn`.
    pub(crate) fn rebind_stream(&mut self) {
        self.consumed = 0;
    }

    /// Feed the full current PTY output_log snapshot; only the bytes past the last
    /// processed line are parsed. Returns true if anything changed. Resilient to the
    /// 200KB ring draining from the front (rebuilds from the current snapshot).
    pub(crate) fn ingest(&mut self, snapshot: &str) -> bool {
        if snapshot.len() < self.consumed {
            // Front-drain or a fresh terminal: rebuild from what's still present.
            self.assistant_text.clear();
            self.transcript.clear();
            self.saw_streaming_text = false;
            self.parse_baseline = 0;
            self.consumed = 0;
        }
        let tail = &snapshot[self.consumed..];
        if tail.is_empty() {
            return false;
        }
        let mut offset = 0usize;
        let mut changed = false;
        while let Some(nl) = tail[offset..].find('\n') {
            let line = &tail[offset..offset + nl];
            self.ingest_line(line);
            offset += nl + 1;
            changed = true;
        }
        self.consumed += offset;
        changed
    }

    fn ingest_line(&mut self, raw: &str) {
        // A single newline-delimited chunk can carry several CARRIAGE-RETURN-separated
        // terminal redraws (codex/claude print a status bar like "12s · 432 tokens"
        // that repaints in place) followed by a real JSON event. Split on '\r' and
        // ONLY ever act on a segment that parses as a JSON object. Terminal chrome is
        // dropped, never shown as transcript — that is what keeps the pane clean.
        for segment in raw.split('\r') {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                continue;
            }
            let candidate = if trimmed.starts_with('{') {
                trimmed.to_string()
            } else {
                let stripped = strip_ansi_for_plan(trimmed);
                let stripped = stripped.trim();
                // The JSON event may sit after a chrome prefix on the same segment;
                // start at the first brace. Pure chrome (no brace) is dropped.
                match stripped.find('{') {
                    Some(brace) => stripped[brace..].to_string(),
                    None => continue,
                }
            };
            if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
                self.dispatch(&value);
            }
            // Non-JSON segments are intentionally dropped (no raw fallback).
        }
    }

    fn dispatch(&mut self, value: &Value) {
        let typ = value.get("type").and_then(Value::as_str).unwrap_or("");
        match typ {
            // ---- Claude stream-json ----
            "system" => {
                if let Some(sid) = value.get("session_id").and_then(Value::as_str) {
                    self.capture_session(sid);
                }
            }
            "stream_event" => self.claude_stream_event(value.get("event")),
            "assistant" => {
                let message = value.get("message");
                if !self.saw_streaming_text {
                    let text = text_from_assistant_message(message);
                    if !text.is_empty() {
                        self.push_text(&text);
                    }
                }
                self.tools_from_assistant_message(message);
            }
            "result" => {
                if value.get("subtype").and_then(Value::as_str) == Some("success") {
                    if let Some(result) = value.get("result").and_then(Value::as_str) {
                        if !self.saw_streaming_text {
                            self.push_text(result);
                        } else {
                            // The `result` event carries the AUTHORITATIVE full text.
                            // Streaming deltas can drop the tail (the PTY ring buffer
                            // truncates, or partial-message events are lossy), which
                            // truncated the post-plan human summary mid-sentence. If
                            // the result strictly EXTENDS what we streamed this turn,
                            // append only the missing suffix so the full summary lands
                            // without double-appending.
                            let start = self.parse_baseline.min(self.assistant_text.len());
                            let turn = self.assistant_text[start..].to_string();
                            if result.len() > turn.len() && result.starts_with(&turn) {
                                let tail = result[turn.len()..].to_string();
                                self.push_text(&tail);
                            }
                        }
                    }
                }
            }
            // ---- Codex exec --json ----
            "thread.started" => {
                if let Some(id) = value.get("thread_id").and_then(Value::as_str) {
                    self.capture_session(id);
                }
            }
            "item.completed" | "item.started" => {
                self.codex_item(value.get("item"), typ == "item.completed");
            }
            "turn.started" | "turn.completed" | "turn.failed" => {}
            _ => {
                if let Some(msg) = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                {
                    self.push(PlanEntryKind::System, msg);
                }
            }
        }
    }

    fn claude_stream_event(&mut self, event: Option<&Value>) {
        let Some(event) = event else { return };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "content_block_delta" => {
                let Some(delta) = event.get("delta") else { return };
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                if dtype == "text_delta" {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        self.saw_streaming_text = true;
                        self.push_text(text);
                    }
                } else if dtype == "thinking_delta" {
                    if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                        self.push_thinking(thinking);
                    }
                }
            }
            "content_block_start" => {
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let target = tool_target(block.get("input"));
                        self.push_tool(name, &target);
                    }
                }
            }
            _ => {}
        }
    }

    fn tools_from_assistant_message(&mut self, message: Option<&Value>) {
        let Some(content) = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let target = tool_target(block.get("input"));
                self.push_tool(name, &target);
            }
        }
    }

    fn codex_item(&mut self, item: Option<&Value>, completed: bool) {
        let Some(item) = item else { return };
        let itype = item.get("type").and_then(Value::as_str).unwrap_or("");
        match itype {
            "agent_message" | "final_answer" | "message" => {
                if completed {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        self.push_text(text);
                    }
                }
            }
            "reasoning" | "agent_reasoning" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    self.push_thinking(text);
                } else if let Some(text) = item.get("summary").and_then(Value::as_str) {
                    self.push_thinking(text);
                }
            }
            "command_execution" | "local_shell_call" | "exec_command" => {
                let cmd = item
                    .get("command")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("text").and_then(Value::as_str))
                    .unwrap_or("");
                self.push_tool("Bash", cmd);
            }
            "file_change" | "patch" | "apply_patch" => {
                self.push(PlanEntryKind::Tool, "Editing files");
            }
            "mcp_tool_call" => {
                let name = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
                self.push_tool(name, "");
            }
            "error" => {
                if let Some(text) = item.get("message").and_then(Value::as_str) {
                    self.push(PlanEntryKind::System, text);
                }
            }
            _ => {}
        }
    }

    fn capture_session(&mut self, id: &str) {
        if self.session_id.is_none() && !id.trim().is_empty() {
            self.session_id = Some(id.to_string());
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.assistant_text.push_str(text);
        // Coalesce consecutive streamed text into one transcript entry.
        if let Some(last) = self.transcript.last_mut() {
            if last.kind == PlanEntryKind::Text {
                last.text.push_str(text);
                return;
            }
        }
        self.push(PlanEntryKind::Text, text);
    }

    fn push_thinking(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.transcript.last_mut() {
            if last.kind == PlanEntryKind::Thinking {
                if last.text.len() < MAX_THINKING_CHARS {
                    last.text.push_str(text);
                }
                return;
            }
        }
        self.push(PlanEntryKind::Thinking, text);
    }

    fn push_tool(&mut self, name: &str, target: &str) {
        let verb = match name {
            "Read" => "Reading",
            "Grep" => "Grep",
            "Glob" => "Glob",
            "LS" => "Listing",
            "WebSearch" => "Searching",
            "WebFetch" => "Fetching",
            "Bash" => "Running",
            other => other,
        };
        let line = if target.trim().is_empty() {
            verb.to_string()
        } else {
            format!("{verb} {}", short_target(target))
        };
        self.push(PlanEntryKind::Tool, &line);
    }

    fn push(&mut self, kind: PlanEntryKind, text: &str) {
        self.transcript.push(PlanEntry {
            kind,
            text: text.to_string(),
        });
        if self.transcript.len() > MAX_TRANSCRIPT {
            let overflow = self.transcript.len() - MAX_TRANSCRIPT;
            self.transcript.drain(0..overflow);
        }
    }
}

/// Mirror of `textFromAssistantMessage` (backends.ts): join the text blocks of an
/// assistant message; used as the fallback when no streaming deltas arrived.
fn text_from_assistant_message(message: Option<&Value>) -> String {
    let Some(content) = message.and_then(|m| m.get("content")) else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(array) = content.as_array() else {
        return String::new();
    };
    array
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                block.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Derive a short, human display target from a tool's input JSON.
fn tool_target(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in ["file_path", "path", "pattern", "query", "url", "command"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// Trim a target to a basename-ish, length-capped form for the transcript.
fn short_target(target: &str) -> String {
    let base = target.rsplit('/').next().unwrap_or(target);
    let base = base.trim();
    if base.chars().count() > 48 {
        let truncated: String = base.chars().take(47).collect();
        format!("{truncated}…")
    } else {
        base.to_string()
    }
}
