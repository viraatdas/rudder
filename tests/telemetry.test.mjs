import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import { projectHash, sanitizeProperties, TELEMETRY_NOTICE, telemetryEnabled } from "../dist/analytics.js";
import { issueBody, issueTitle, parseGithubSlug, redactText, submitFeedback } from "../dist/feedback.js";
import { isInternalCommand } from "../dist/main.js";

// Nothing in this file may reach the network. `submitFeedback` is exercised with
// telemetry off and issue filing disabled, so the assertions are about the local
// artifact and the redaction — not about PostHog or gh.
process.env.RUDDER_TELEMETRY = "0";

// ---------------------------------------------------------------------------
// Redaction. This is the whole basis for telemetry being acceptable in a dev
// tool, so it gets the most tests.
// ---------------------------------------------------------------------------

test("redactText: home directories and absolute paths never leave the machine", () => {
  const home = "/Users/someone";
  assert.equal(
    redactText("merge failed in /Users/someone/code/secret-startup/src/auth.ts", home),
    "merge failed in <path>",
  );
  assert.equal(
    redactText("worktree /private/var/folders/xy/T/rudder-abc is stale", home),
    "worktree <path> is stale",
  );
  // Short, non-path slashes survive: "and/or" is prose, not a filesystem path.
  assert.equal(redactText("merge and/or rebase", home), "merge and/or rebase");
});

test("redactText: token-shaped strings are replaced, not shipped", () => {
  for (const secret of [
    "sk-abcdefghijklmnop",
    "ghp_abcdefghijklmnop",
    "phc_abcdefghijklmnop",
    "xoxb-abcdefghijklmnop",
  ]) {
    const out = redactText(`my key is ${secret} ok`);
    assert.ok(!out.includes(secret), `${secret} was redacted: ${out}`);
    assert.ok(out.includes("<redacted>"), out);
  }
});

test("sanitizeProperties: a call site that passes a path or a token loses that property", () => {
  const clean = sanitizeProperties({
    backend: "claude",
    cwd: "/Users/someone/code/thing",
    token: "phc_abcdefghijklmnop",
    agents: 3,
    ok: true,
    missing: undefined,
  });
  assert.deepEqual(clean, { backend: "claude", agents: 3, ok: true });
});

test("projectHash: identifies a repo without naming it, and is stable", () => {
  const first = projectHash("/Users/someone/code/secret-startup");
  assert.equal(first, projectHash("/Users/someone/code/secret-startup"));
  assert.notEqual(first, projectHash("/Users/someone/code/other"));
  assert.match(first, /^[0-9a-f]{12}$/);
  assert.ok(!first.includes("secret"));
});

// ---------------------------------------------------------------------------
// The off switch. If this ever regresses, the product is lying to its users.
// ---------------------------------------------------------------------------

test("telemetryEnabled: RUDDER_TELEMETRY=0 wins over everything", async () => {
  assert.equal(await telemetryEnabled(), false);
  assert.match(TELEMETRY_NOTICE, /rudder telemetry off/);
});

// ---------------------------------------------------------------------------
// Feedback reports.
// ---------------------------------------------------------------------------

test("submitFeedback: the local copy is written first and survives every other failure", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "rudder-feedback-"));
  try {
    const report = await submitFeedback({
      text: "merge asked twice and did nothing",
      repoRoot: root,
      issue: false,
      context: {
        backend: "claude",
        model: "opus",
        agents: 3,
        agentsRunning: 1,
        notices: ["merge failed: /Users/someone/code/app/src/x.ts", "second", "third", "fourth"],
        lastError: "jj workspace failed at /Users/someone/code/app",
      },
    });

    const saved = JSON.parse(await readFile(report.localPath, "utf8"));
    assert.equal(saved.text, "merge asked twice and did nothing");
    // Only the last three notices, all with paths stripped.
    assert.equal(saved.context.notices.length, 3);
    assert.ok(!JSON.stringify(saved.context).includes("/Users/someone"), JSON.stringify(saved.context));
    assert.ok(!saved.context.lastError.includes("/Users/someone"));
    // Honest accounting of what did NOT happen.
    assert.ok(report.skipped.some((reason) => reason.includes("telemetry is off")), report.skipped.join("; "));
    assert.ok(report.skipped.some((reason) => reason.includes("--no-issue")), report.skipped.join("; "));
    assert.equal(report.issueUrl, undefined);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("submitFeedback: an empty report is refused rather than filed", async () => {
  await assert.rejects(() => submitFeedback({ text: "   ", issue: false }), /Nothing to send/);
});

test("issueTitle / issueBody: a scannable title and a context block with no code in it", () => {
  assert.equal(issueTitle("merge asked twice\nand then did nothing"), "merge asked twice");
  const body = issueBody("merge asked twice", {
    backend: "codex",
    model: "gpt-5.6-sol",
    agents: 2,
    notices: ["merge failed", "retried"],
  });
  assert.match(body, /- backend: codex/);
  assert.match(body, /- model: gpt-5\.6-sol/);
  assert.match(body, /Recent notices:/);
  assert.match(body, /_Filed by `rudder feedback`\._/);
});

test("an unknown internal command is inert, not a task", () => {
  // The dashboard shells out to `rudder __event`, resolving `rudder` from PATH —
  // which can be an OLDER install than the binary calling it. Without this the
  // fallback would spawn a real agent named "__event dashboard_opened …".
  assert.equal(isInternalCommand("__event"), true);
  assert.equal(isInternalCommand("__does_not_exist_yet"), true);
  assert.equal(isInternalCommand("merge"), false);
  assert.equal(isInternalCommand(undefined), false);
});

test("feedbackRepo: a fork files its own issues", () => {
  assert.equal(parseGithubSlug("https://github.com/viraatdas/rudder.git"), "viraatdas/rudder");
  assert.equal(parseGithubSlug("git@github.com:someone/fork.git"), "someone/fork");
  assert.equal(parseGithubSlug("https://gitlab.com/someone/thing.git"), undefined);
});
