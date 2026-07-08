import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

// The cloud/ subproject has its own build (see AGENTS.md section 11). In a
// checkout where it was never built (a fresh worktree, for example), skip
// instead of erroring so `npm test` stays green without a network install.
const slackModuleUrl = new URL("../cloud/dist/slack.js", import.meta.url);
const cloudBuilt = existsSync(fileURLToPath(slackModuleUrl));
const test = cloudBuilt
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud/dist not built (npm --prefix cloud run build)" }, fn);
const {
  DEFAULT_SLACK_CHANNEL,
  formatOutputForSlack,
  parseSlackCommand,
  slackConfigFromEnv,
  stripAnsi,
  verifySlackSignature,
} = cloudBuilt ? await import(slackModuleUrl.href) : {};

test("default channel is the shared Exla channel", () => {
  assert.equal(DEFAULT_SLACK_CHANNEL, "C0B78TDLM5G");
  const cfg = slackConfigFromEnv({});
  assert.equal(cfg.channel, "C0B78TDLM5G");
  assert.equal(cfg.enabled, false);
});

test("slackConfigFromEnv reads token + override channel", () => {
  const cfg = slackConfigFromEnv({
    SLACK_BOT_TOKEN: "xoxb-abc",
    SLACK_SIGNING_SECRET: "s3cret",
    RUDDER_SLACK_CHANNEL: "C999",
  });
  assert.equal(cfg.enabled, true);
  assert.equal(cfg.channel, "C999");
  assert.equal(cfg.signingSecret, "s3cret");
});

test("verifySlackSignature accepts a correct v0 signature", () => {
  const secret = "8f742231b10e8888abcd99yyyzzz85a5";
  const body = JSON.stringify({ type: "event_callback" });
  const timestamp = "1700000000";
  const sig = "v0=" + createHmac("sha256", secret).update(`v0:${timestamp}:${body}`).digest("hex");
  const ok = verifySlackSignature({
    signingSecret: secret,
    timestamp,
    signature: sig,
    rawBody: body,
    now: 1700000000 * 1000,
  });
  assert.equal(ok, true);
});

test("verifySlackSignature rejects tampered body, stale ts, and missing secret", () => {
  const secret = "abc";
  const body = "real";
  const timestamp = "1700000000";
  const sig = "v0=" + createHmac("sha256", secret).update(`v0:${timestamp}:${body}`).digest("hex");
  // tampered body
  assert.equal(verifySlackSignature({ signingSecret: secret, timestamp, signature: sig, rawBody: "fake", now: 1700000000 * 1000 }), false);
  // stale timestamp (> 5 min)
  assert.equal(verifySlackSignature({ signingSecret: secret, timestamp, signature: sig, rawBody: body, now: (1700000000 + 999) * 1000 }), false);
  // no secret -> fail closed
  assert.equal(verifySlackSignature({ signingSecret: "", timestamp, signature: sig, rawBody: body, now: 1700000000 * 1000 }), false);
});

test("stripAnsi removes CSI/OSC/control bytes", () => {
  const raw = "\x1b[2J\x1b[1;31mhello\x1b[0m\x1b]0;title\x07 world\x07";
  const clean = stripAnsi(raw);
  assert.equal(clean.includes("\x1b"), false);
  assert.match(clean, /hello/);
  assert.match(clean, /world/);
});

test("formatOutputForSlack code-fences and tails long output", () => {
  assert.equal(formatOutputForSlack(""), "_(no output yet)_");
  const long = "x".repeat(5000);
  const out = formatOutputForSlack(long, 100);
  assert.ok(out.startsWith("```"));
  assert.ok(out.includes("…"));
  assert.ok(out.length < 200);
});

test("parseSlackCommand handles list/help/talk/output/stop", () => {
  assert.deepEqual(parseSlackCommand("<@U123> list", { inThread: false }), { action: "list" });
  assert.deepEqual(parseSlackCommand("help", { inThread: false }), { action: "help" });
  assert.deepEqual(parseSlackCommand("talk cloud-foo do the thing", { inThread: false }), {
    action: "talk",
    id: "cloud-foo",
    message: "do the thing",
  });
  assert.deepEqual(parseSlackCommand("output cloud-bar", { inThread: false }), { action: "output", id: "cloud-bar" });
  assert.deepEqual(parseSlackCommand("stop cloud-bar", { inThread: false }), { action: "stop", id: "cloud-bar" });
});

test("parseSlackCommand treats bare thread messages as replies", () => {
  assert.deepEqual(parseSlackCommand("ship it", { inThread: true }), { action: "thread-reply", message: "ship it" });
  // first-token-as-id only applies outside a thread
  assert.deepEqual(parseSlackCommand("cloud-foo keep going", { inThread: false }), {
    action: "talk",
    id: "cloud-foo",
    message: "keep going",
  });
});
