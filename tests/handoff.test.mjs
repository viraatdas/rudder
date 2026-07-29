import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import { normalizeEffortForBackend } from "../dist/effort.js";
import {
  firstUserPrompt,
  isValidSessionId,
  parseHandoffArgs,
  parseOpencodeSessions,
  recentClaudeConversations,
  resolveClaudeSessionId,
  stripWrapperBlocks,
  writeHandoffRequest,
} from "../dist/handoff.js";
import { encodeClaudeProjectsCwd } from "../dist/migration.js";

const userLine = (text, extra = {}) =>
  `${JSON.stringify({ type: "user", message: { role: "user", content: text }, ...extra })}\n`;

// ---------------------------------------------------------------------------
// Titles: what the dashboard shows for a conversation. The transcript is full of
// wrappers Claude Code injects around real user text; a title made of those is
// useless for picking the right chat.
// ---------------------------------------------------------------------------

test("firstUserPrompt: skips meta, sidechain, and wrapper-only turns", () => {
  const raw = [
    JSON.stringify({ type: "mode", mode: "normal" }),
    JSON.stringify({ type: "user", isMeta: true, message: { role: "user", content: "meta noise" } }),
    JSON.stringify({
      type: "user",
      isSidechain: true,
      message: { role: "user", content: "subagent task" },
    }),
    JSON.stringify({
      type: "user",
      message: { role: "user", content: "<system-reminder>\nignore me\n</system-reminder>" },
    }),
    JSON.stringify({ type: "user", message: { role: "user", content: "add a handoff command" } }),
  ].join("\n");
  assert.equal(firstUserPrompt(raw), "add a handoff command");
});

test("firstUserPrompt: reads text blocks and ignores tool results", () => {
  const raw = JSON.stringify({
    type: "user",
    message: {
      role: "user",
      content: [
        { type: "tool_result", content: "noise" },
        { type: "text", text: "fix the diff pane" },
      ],
    },
  });
  assert.equal(firstUserPrompt(raw), "fix the diff pane");
});

test("stripWrapperBlocks: drops multi-line blocks and local-command caveats", () => {
  const text = [
    "Caveat: local command",
    "<command-name>/model</command-name>",
    "<system-reminder>",
    "line one",
    "line two",
    "</system-reminder>",
    "real   question here",
  ].join("\n");
  assert.equal(stripWrapperBlocks(text), "real question here");
});

// ---------------------------------------------------------------------------
// Argument shape.
// ---------------------------------------------------------------------------

test("parseHandoffArgs: flags are stripped, the rest is the instruction", () => {
  const parsed = parseHandoffArgs(["--here", "--session", "abc12345", "now", "write", "the", "tests"]);
  assert.equal(parsed.here, true);
  assert.equal(parsed.sessionId, "abc12345");
  assert.equal(parsed.instruction, "now write the tests");
  assert.equal(parsed.list, false);
});

test("isValidSessionId: only id-shaped tokens reach a command line", () => {
  assert.equal(isValidSessionId("3503304e-5818-45d5-8b5b-4ea15a857e09"), true);
  for (const bad of ["", "short", "abc; rm -rf /", "../../etc/passwd"]) {
    assert.equal(isValidSessionId(bad), false, `rejected ${JSON.stringify(bad)}`);
  }
});

// ---------------------------------------------------------------------------
// Discovery + queueing.
// ---------------------------------------------------------------------------

test("recentClaudeConversations: this repo's chats, newest first, without Rudder's own agents", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "rudder-handoff-"));
  try {
    const projects = path.join(root, "projects");
    const cwd = path.join(root, "repo");
    const encoded = encodeClaudeProjectsCwd(cwd);
    const dirs = {
      repo: path.join(projects, encoded),
      sub: path.join(projects, `${encoded}-src`),
      worker: path.join(projects, `${encoded}--rudder-worktrees-n0`),
      other: path.join(projects, "-Users-someone-else"),
    };
    for (const dir of Object.values(dirs)) {
      await mkdir(dir, { recursive: true });
    }
    // Written oldest -> newest so mtime order is deterministic.
    await writeFile(path.join(dirs.other, "44444444-4444-4444-4444-444444444444.jsonl"), userLine("other repo"));
    await writeFile(path.join(dirs.worker, "33333333-3333-3333-3333-333333333333.jsonl"), userLine("worker pane"));
    await writeFile(path.join(dirs.repo, "11111111-1111-1111-1111-111111111111.jsonl"), userLine("older repo chat"));
    await new Promise((resolve) => setTimeout(resolve, 10));
    await writeFile(path.join(dirs.sub, "22222222-2222-2222-2222-222222222222.jsonl"), userLine("newer subdir chat"));

    const found = recentClaudeConversations(cwd, 10, projects);
    assert.deepEqual(
      found.map((candidate) => candidate.title),
      ["newer subdir chat", "older repo chat"],
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolveClaudeSessionId: prefers the session this command is running inside", () => {
  const sessionId = "3503304e-5818-45d5-8b5b-4ea15a857e09";
  assert.equal(
    resolveClaudeSessionId(process.cwd(), { CLAUDE_CODE_SESSION_ID: sessionId }),
    sessionId,
  );
});

test("parseOpencodeSessions: this repo's opencode chats, newest first", () => {
  const raw = JSON.stringify([
    { id: "ses_0000000000000000000000001", title: "older chat", updated: 1000, directory: "/repo" },
    { id: "ses_0000000000000000000000002", title: "newest chat", updated: 9000, directory: "/repo/src" },
    { id: "ses_0000000000000000000000003", title: "other repo", updated: 9999, directory: "/other" },
    { id: "ses_0000000000000000000000004", updated: 9998, directory: "/repo" },
  ]);
  assert.deepEqual(
    parseOpencodeSessions(raw, "/repo").map((candidate) => candidate.title),
    ["newest chat", "older chat"],
  );
});

test("opencode carries no reasoning effort: it has no flag to pass one to", () => {
  assert.equal(normalizeEffortForBackend("opencode", "high"), undefined);
  assert.equal(normalizeEffortForBackend("claude", "high"), "high");
  assert.equal(normalizeEffortForBackend("codex", "max"), "xhigh");
});

test("parseHandoffArgs: --opencode selects the opencode conversation source", () => {
  assert.equal(parseHandoffArgs(["--opencode", "ship it"]).backend, "opencode");
  assert.equal(parseHandoffArgs(["--codex"]).backend, "codex");
  assert.equal(parseHandoffArgs(["ship it"]).backend, undefined);
});

test("writeHandoffRequest: queue files sort by time and land whole", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "rudder-handoff-queue-"));
  try {
    const request = {
      requestId: "abcdef12-0000-0000-0000-000000000000",
      sessionId: "3503304e-5818-45d5-8b5b-4ea15a857e09",
      backend: "claude",
      target: "worker",
      instruction: "now write the tests",
      title: "add a handoff command",
      sourceCwd: root,
      createdAt: 1_700_000_000_000,
    };
    const file = writeHandoffRequest(root, request);
    assert.ok(
      path.basename(file).startsWith("1700000000000-"),
      `filename leads with the stamp so the dashboard drains in order: ${file}`,
    );
    // No temp file left behind: the dashboard globs *.json and must never read a
    // half-written request.
    const written = JSON.parse(await readFile(file, "utf8"));
    assert.deepEqual(written, request);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
