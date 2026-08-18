// ---------------------------------------------------------------------------
// CONVERSATION HANDOFF (push side).
//
// `rudder handoff "<next step>"` is meant to be run from INSIDE the chat you are
// already having — a plain `claude`/`codex` session in another terminal, or the
// agent in this one. It queues that conversation for the dashboard, which forks it
// into an agent pane holding the entire discussion: the files you looked at, the
// decisions you made, the plan you agreed on. Nothing is retyped.
//
// The queue is a directory of small JSON files under `.rudder/handoffs/`. The
// dashboard drains it on its poll (native/src/handoff.rs), so this command works
// whether Rudder is already open or is started afterwards.
// ---------------------------------------------------------------------------

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";

import { findRepoRoot } from "./git.js";
import { AGENT_WORKSPACES_DIR, LEGACY_AGENT_WORKSPACES_DIR } from "./state.js";
import { encodeClaudeProjectsCwd } from "./migration.js";

export type HandoffBackend = "claude" | "codex" | "opencode";

export interface HandoffRequest {
  requestId: string;
  sessionId: string;
  backend: HandoffBackend;
  /** "worker" = isolated jj workspace (mergeable); "here" = the main checkout. */
  target: "worker" | "here";
  instruction?: string;
  title?: string;
  sourceCwd: string;
  createdAt: number;
}

export interface HandoffArgs {
  sessionId?: string;
  backend?: HandoffBackend;
  instruction: string;
  here: boolean;
  list: boolean;
}

export interface ConversationCandidate {
  sessionId: string;
  title: string;
  modifiedMs: number;
  path: string;
}

/** Transcripts grow to tens of megabytes; the opening prompt is at the top. */
const TITLE_SCAN_BYTES = 256 * 1024;

export function parseHandoffArgs(args: string[]): HandoffArgs {
  const words: string[] = [];
  let sessionId: string | undefined;
  let backend: HandoffBackend | undefined;
  let here = false;
  let list = false;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i] ?? "";
    if (arg === "--here" || arg === "--main") {
      here = true;
      continue;
    }
    if (arg === "--list" || arg === "--ls") {
      list = true;
      continue;
    }
    if (arg === "--claude" || arg === "--codex" || arg === "--opencode") {
      backend = arg.slice(2) as HandoffBackend;
      continue;
    }
    if (arg === "--session" || arg === "-s") {
      sessionId = args[i + 1];
      i += 1;
      continue;
    }
    if (arg.startsWith("--session=")) {
      sessionId = arg.slice("--session=".length);
      continue;
    }
    words.push(arg);
  }
  return { sessionId, backend, instruction: words.join(" ").trim(), here, list };
}

/**
 * A session id is spliced onto a `claude`/`codex` command line by the dashboard.
 * Accept only id-shaped strings — the same rule the Rust side enforces on read.
 */
export function isValidSessionId(value: string): boolean {
  const trimmed = value.trim();
  return trimmed.length >= 8 && trimmed.length <= 128 && /^[A-Za-z0-9_-]+$/.test(trimmed);
}

/**
 * Drop the wrapper blocks Claude Code injects around real user text
 * (`<system-reminder>…`, slash-command echoes, the local-command caveat) and
 * collapse what is left onto one line. Mirrors strip_wrapper_blocks in
 * native/src/handoff.rs.
 */
export function stripWrapperBlocks(text: string): string {
  const kept: string[] = [];
  let closing: string | undefined;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (closing) {
      if (line.endsWith(closing)) {
        closing = undefined;
      }
      continue;
    }
    if (!line || line.startsWith("Caveat:")) {
      continue;
    }
    // Wrapper tags may carry attributes: <teammate-message teammate_id="lead">.
    const opening = /^<([A-Za-z0-9_-]+)(\s[^>]*)?>/.exec(line);
    if (opening) {
      const close = `</${opening[1]}>`;
      if (!line.includes(close)) {
        closing = close;
      }
      continue;
    }
    kept.push(line);
  }
  return kept.join(" ").split(/\s+/).filter(Boolean).join(" ");
}

/** The conversation's opening user prompt: what a human would call the chat. */
export function firstUserPrompt(raw: string): string | undefined {
  for (const line of raw.split("\n")) {
    const prompt = userPromptFromLine(line);
    if (prompt) {
      return prompt;
    }
  }
  return undefined;
}

/**
 * One transcript entry's user prose, or undefined when it is not a real user turn.
 * Skips sidechain (subagent) turns, meta entries, and the wrapper blocks Claude
 * Code injects around user text.
 */
function userPromptFromLine(line: string): string | undefined {
  let value: Record<string, unknown>;
  try {
    value = JSON.parse(line) as Record<string, unknown>;
  } catch {
    return undefined;
  }
  if (value.type !== "user" || value.isSidechain === true || value.isMeta === true) {
    return undefined;
  }
  const message = value.message as { content?: unknown } | undefined;
  const content = message?.content;
  let text = "";
  if (typeof content === "string") {
    text = content;
  } else if (Array.isArray(content)) {
    text = content
      .filter((block): block is { type: string; text: string } =>
        Boolean(block) && (block as { type?: string }).type === "text",
      )
      .map((block) => block.text)
      .join(" ");
  }
  const cleaned = stripWrapperBlocks(text);
  return cleaned && !isRudderGeneratedPrompt(cleaned) ? cleaned : undefined;
}

/**
 * What the listing needs from a transcript, read from its head.
 *
 * `interactive` is whether a human talked to a REPL: Claude records its session
 * mode at start, and the one-shot `claude -p` calls Rudder makes internally (task
 * titles, completion notes) do not — which is how they stay out of the listing.
 */
export function scanTranscriptHead(raw: string): { title: string; interactive: boolean } | undefined {
  let interactive = false;
  for (const line of raw.split("\n")) {
    if (/"type"\s*:\s*"(mode|permission-mode)"/.test(line)) {
      interactive = true;
    }
    const title = userPromptFromLine(line);
    if (title) {
      return { title, interactive };
    }
  }
  return undefined;
}

function readHead(file: string, maxBytes: number): string {
  let handle: number | undefined;
  try {
    handle = fs.openSync(file, "r");
    const buffer = Buffer.alloc(maxBytes);
    const read = fs.readSync(handle, buffer, 0, maxBytes, 0);
    return buffer.subarray(0, read).toString("utf8");
  } catch {
    return "";
  } finally {
    if (handle !== undefined) {
      fs.closeSync(handle);
    }
  }
}

export function claudeProjectsDir(): string {
  return path.join(os.homedir(), ".claude", "projects");
}

/**
 * Recent Claude conversations recorded for `cwd` and its subdirectories, newest
 * first. Claude names each project folder after the directory the session ran in,
 * so a chat started in `src/` lives in a sibling folder — still this repo's.
 * Rudder's own agent sessions (under `.rudder-workspaces`) are not candidates.
 */
export function recentClaudeConversations(
  cwd: string,
  limit = 10,
  projects = claudeProjectsDir(),
  exclude: ReadonlySet<string> = new Set(),
): ConversationCandidate[] {
  const encoded = encodeClaudeProjectsCwd(path.resolve(cwd));
  let dirs: string[];
  try {
    dirs = fs.readdirSync(projects);
  } catch {
    return [];
  }
  const files: { file: string; modifiedMs: number }[] = [];
  for (const name of dirs) {
    const inRepo = name === encoded || name.startsWith(`${encoded}-`);
    if (!inRepo || name.includes("-rudder-workspaces-") || name.includes("-rudder-worktrees-")) {
      continue;
    }
    let sessions: string[];
    try {
      sessions = fs.readdirSync(path.join(projects, name));
    } catch {
      continue;
    }
    for (const session of sessions) {
      if (!session.endsWith(".jsonl")) {
        continue;
      }
      const file = path.join(projects, name, session);
      try {
        files.push({ file, modifiedMs: fs.statSync(file).mtimeMs });
      } catch {
        // Raced with a delete; skip it.
      }
    }
  }
  files.sort((left, right) => right.modifiedMs - left.modifiedMs);
  const interactive: ConversationCandidate[] = [];
  const all: ConversationCandidate[] = [];
  // Open a few times the requested count: the newest transcripts are often
  // Rudder's own one-shot calls, and dropping them must not empty the list.
  for (const { file, modifiedMs } of files.slice(0, limit * 4)) {
    const sessionId = path.basename(file, ".jsonl");
    if (!isValidSessionId(sessionId) || exclude.has(sessionId)) {
      continue;
    }
    const head = scanTranscriptHead(readHead(file, TITLE_SCAN_BYTES));
    if (!head) {
      continue;
    }
    const candidate = { sessionId, title: head.title, modifiedMs, path: file };
    if (head.interactive && interactive.length < limit) {
      interactive.push(candidate);
    }
    if (all.length < limit) {
      all.push(candidate);
    }
  }
  // A noisy listing beats an empty one if the interactive marker ever disappears.
  return interactive.length > 0 ? interactive : all;
}

export function claudeTranscriptPath(cwd: string, sessionId: string, projects = claudeProjectsDir()): string | undefined {
  const direct = path.join(projects, encodeClaudeProjectsCwd(path.resolve(cwd)), `${sessionId}.jsonl`);
  if (fs.existsSync(direct)) {
    return direct;
  }
  // A chat started in a subdirectory (or a differently-encoded path) still belongs
  // to this machine; find it by id rather than refusing the handoff.
  let dirs: string[];
  try {
    dirs = fs.readdirSync(projects);
  } catch {
    return undefined;
  }
  for (const name of dirs) {
    const candidate = path.join(projects, name, `${sessionId}.jsonl`);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

/**
 * The conversation this command is running inside. Claude Code exports its session
 * id to every process it spawns, which is the whole trick: an agent asked to "hand
 * this off to rudder" can run the command with no arguments and mean THIS chat.
 * Falls back to the newest transcript recorded for the directory.
 */
export function resolveClaudeSessionId(cwd: string, env: NodeJS.ProcessEnv = process.env): string | undefined {
  const fromEnv = (env.CLAUDE_CODE_SESSION_ID ?? env.CLAUDE_SESSION_ID ?? "").trim();
  if (isValidSessionId(fromEnv)) {
    return fromEnv;
  }
  return recentClaudeConversations(cwd, 1)[0]?.sessionId;
}

/**
 * Newest opencode session for `cwd`. opencode keeps sessions in a database and
 * ships the query, so Rudder asks it rather than reading a schema it does not own.
 * Running this from inside an opencode chat resolves to THAT chat: it is the one
 * that was updated most recently.
 */
export function resolveOpencodeSessionId(cwd: string): string | undefined {
  return recentOpencodeConversations(cwd, 1)[0]?.sessionId;
}

export function recentOpencodeConversations(cwd: string, limit = 10): ConversationCandidate[] {
  let raw: string;
  try {
    raw = execFileSync("opencode", ["session", "list", "--format", "json", "-n", String(limit * 4)], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 10_000,
    });
  } catch {
    return [];
  }
  return parseOpencodeSessions(raw, cwd, limit);
}

export function parseOpencodeSessions(raw: string, cwd: string, limit = 10): ConversationCandidate[] {
  let sessions: unknown;
  try {
    sessions = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(sessions)) {
    return [];
  }
  const root = path.resolve(cwd);
  return sessions
    .filter((session): session is Record<string, unknown> => Boolean(session) && typeof session === "object")
    .filter((session) => {
      const directory = session.directory;
      return typeof directory === "string" && path.resolve(directory).startsWith(root);
    })
    .filter((session) => typeof session.id === "string" && isValidSessionId(session.id))
    .map((session) => ({
      sessionId: String(session.id),
      title: typeof session.title === "string" ? session.title.trim() : "",
      modifiedMs: typeof session.updated === "number" ? session.updated : 0,
      path: typeof session.directory === "string" ? session.directory : cwd,
    }))
    .filter((candidate) => candidate.title.length > 0)
    .sort((left, right) => right.modifiedMs - left.modifiedMs)
    .slice(0, limit);
}

/** Newest Codex session recorded for `cwd`. Codex sessions live in one global tree. */
export function resolveCodexSessionId(cwd: string, root = codexSessionsDir()): string | undefined {
  return recentCodexConversations(cwd, 1, root)[0]?.sessionId;
}

export function codexSessionsDir(): string {
  return path.join(os.homedir(), ".codex", "sessions");
}

/**
 * Recent Codex conversations for `cwd`, newest first. Codex keeps ONE global
 * rollout tree, so this filters by the recorded cwd BEFORE taking the newest few —
 * the newest rollouts overall are usually other repositories'. Mirrors
 * `recent_codex_conversations_in` in native/src/handoff.rs.
 */
export function recentCodexConversations(
  cwd: string,
  limit = 10,
  root = codexSessionsDir(),
): ConversationCandidate[] {
  const target = path.resolve(cwd);
  const files: { file: string; modifiedMs: number }[] = [];
  const walk = (dir: string, depth: number): void => {
    if (depth > 8) {
      return;
    }
    let entries: fs.Dirent[];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full, depth + 1);
        continue;
      }
      if (!entry.name.endsWith(".jsonl")) {
        continue;
      }
      try {
        files.push({ file: full, modifiedMs: fs.statSync(full).mtimeMs });
      } catch {
        // Raced with a delete; skip it.
      }
    }
  };
  walk(root, 0);
  files.sort((left, right) => right.modifiedMs - left.modifiedMs);

  const found: ConversationCandidate[] = [];
  // Resuming a session writes ANOTHER rollout under the same id; newest wins.
  const seen = new Set<string>();
  let examined = 0;
  for (const { file, modifiedMs } of files) {
    if (found.length >= limit || examined >= 600) {
      break;
    }
    examined += 1;
    // Cheap reject first: `session_meta` puts cwd in the first few hundred bytes,
    // while the rest of that line is the entire Codex system prompt.
    const prefix = readHead(file, 4096);
    if (!prefix.includes(`"cwd":"${target}`) && !prefix.includes(`"cwd": "${target}`)) {
      continue;
    }
    const head = parseCodexSessionHead(readHead(file, TITLE_SCAN_BYTES));
    if (!head || !path.resolve(head.cwd).startsWith(target)) {
      continue;
    }
    if (seen.has(head.sessionId)) {
      continue;
    }
    seen.add(head.sessionId);
    found.push({ sessionId: head.sessionId, title: head.title, modifiedMs, path: file });
  }
  return found;
}

/**
 * A Codex rollout's identity: the `session_meta` line carries the id and cwd, and
 * the title is the first turn whose role is `user` — `developer` turns are the
 * harness's own context blocks.
 */
export function parseCodexSessionHead(
  raw: string,
): { sessionId: string; cwd: string; title: string } | undefined {
  let sessionId = "";
  let cwd = "";
  for (const line of raw.split("\n")) {
    let value: Record<string, unknown>;
    try {
      value = JSON.parse(line) as Record<string, unknown>;
    } catch {
      continue;
    }
    const payload = (value.payload ?? {}) as Record<string, unknown>;
    if (value.type === "session_meta") {
      // A subagent thread is the model talking to itself: not a conversation.
      if (payload.thread_source === "subagent") {
        return undefined;
      }
      sessionId = typeof payload.id === "string" ? payload.id : "";
      cwd = typeof payload.cwd === "string" ? payload.cwd : "";
      continue;
    }
    if (!sessionId || payload.type !== "message" || payload.role !== "user") {
      continue;
    }
    const content = Array.isArray(payload.content) ? payload.content : [];
    const text = content
      .map((block) => (isRecordLike(block) && typeof block.text === "string" ? block.text : ""))
      .join(" ");
    const title = stripWrapperBlocks(text);
    // Codex opens every session by sending the repo's instructions file as a USER
    // turn; the human's first turn is the one after it.
    if (!title || isInjectedInstructions(title) || isRudderGeneratedPrompt(title)) {
      continue;
    }
    if (!isValidSessionId(sessionId) || !cwd) {
      return undefined;
    }
    return { sessionId, cwd, title };
  }
  return undefined;
}

/** `# AGENTS.md instructions for /path` — the harness talking, not a person. */
export function isInjectedInstructions(text: string): boolean {
  const head = text.trimStart();
  return head.startsWith("#") && head.includes(" instructions for ");
}

/**
 * Prompts RUDDER writes itself. A `codex exec` rollout looks exactly like a real
 * chat in its metadata (`originator: codex-tui`, `source: cli`), so these are
 * matched by their opening text — which Rudder controls. Mirrors
 * `is_rudder_generated_prompt` in native/src/handoff.rs.
 */
export function isRudderGeneratedPrompt(text: string): boolean {
  const head = text.trimStart();
  return [
    "Summarize this coding agent task for a compact sidebar label.",
    "A coding agent finished the task below but did not file a structured report.",
  ].some((prompt) => head.startsWith(prompt));
}

function isRecordLike(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object";
}

/** `.rudder/` lives at the dashboard root, never inside an agent's workspace. */
export function dashboardRoot(cwd = process.cwd()): string {
  const root = findRepoRoot(cwd);
  for (const dir of [AGENT_WORKSPACES_DIR, LEGACY_AGENT_WORKSPACES_DIR]) {
    const index = root.indexOf(`${path.sep}${dir}${path.sep}`);
    if (index !== -1) {
      return root.slice(0, index);
    }
  }
  return root;
}

export function handoffQueueDir(repoRoot: string): string {
  return path.join(repoRoot, ".rudder", "handoffs");
}

export function writeHandoffRequest(repoRoot: string, request: HandoffRequest): string {
  const dir = handoffQueueDir(repoRoot);
  fs.mkdirSync(dir, { recursive: true });
  // Filename order is queue order on the reading side, so lead with the stamp.
  const file = path.join(dir, `${request.createdAt}-${request.requestId.slice(0, 8)}.json`);
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, `${JSON.stringify(request, null, 2)}\n`);
  fs.renameSync(temp, file);
  return file;
}

export async function runHandoff(args: string[], backendFlag?: string): Promise<void> {
  const parsed = parseHandoffArgs(args);
  const cwd = process.cwd();
  const repoRoot = dashboardRoot(cwd);

  if (parsed.list) {
    const candidates = [
      ...recentClaudeConversations(repoRoot, 10),
      ...recentCodexConversations(repoRoot, 10),
      ...recentOpencodeConversations(repoRoot, 10),
    ]
      .sort((left, right) => right.modifiedMs - left.modifiedMs)
      .slice(0, 10);
    if (candidates.length === 0) {
      console.log("No conversations recorded for this repository yet.");
      return;
    }
    console.log("Recent conversations in this repository:\n");
    for (const candidate of candidates) {
      const when = new Date(candidate.modifiedMs).toLocaleString();
      console.log(`  ${candidate.sessionId}  ${when}`);
      console.log(`    ${candidate.title.slice(0, 100)}`);
    }
    console.log('\nHand one off with: rudder handoff --session <id> "<next step>"');
    return;
  }

  const backend: HandoffBackend =
    parsed.backend ??
    (backendFlag === "codex" || backendFlag === "opencode" ? backendFlag : undefined) ??
    (process.env.CODEX_SESSION_ID || process.env.CODEX_HOME_SESSION ? "codex" : "claude");

  let sessionId = parsed.sessionId?.trim();
  if (!sessionId) {
    // With no id, "the conversation this command is running inside" is the newest
    // one this backend recorded for the directory.
    sessionId =
      backend === "codex"
        ? resolveCodexSessionId(cwd)
        : backend === "opencode"
          ? resolveOpencodeSessionId(cwd)
          : resolveClaudeSessionId(cwd);
  }
  if (!sessionId) {
    throw new Error(
      `No ${backend} conversation found to hand off. Run this from inside the chat, or pass --session <id> (list them with \`rudder handoff --list\`).`,
    );
  }
  if (!isValidSessionId(sessionId)) {
    throw new Error(`Not a session id: ${sessionId}`);
  }

  let title: string | undefined;
  if (backend === "claude") {
    const transcript = claudeTranscriptPath(cwd, sessionId);
    if (!transcript) {
      throw new Error(
        `No Claude transcript found for session ${sessionId}. List the ones Rudder can see with \`rudder handoff --list\`.`,
      );
    }
    title = firstUserPrompt(readHead(transcript, TITLE_SCAN_BYTES));
  }

  const request: HandoffRequest = {
    requestId: randomUUID(),
    sessionId,
    backend,
    target: parsed.here ? "here" : "worker",
    instruction: parsed.instruction || undefined,
    title,
    sourceCwd: cwd,
    createdAt: Date.now(),
  };
  writeHandoffRequest(repoRoot, request);

  const where = request.target === "here" ? "the main checkout" : "an isolated worker";
  const label = title ? `“${title.slice(0, 60)}”` : `session ${sessionId.slice(0, 8)}…`;
  console.log(`Queued ${label} for handoff into ${where}.`);
  console.log(
    "The dashboard forks the conversation (the original chat is untouched) within a second; run `rudder` if it is not open.",
  );
}
