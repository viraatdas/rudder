// Slack glue for Rudder Cloud.
//
// Rudder Cloud uses a single Slack channel as the "main panel" for every cloud
// instance attached to the account. Each instance (sail) gets its own thread in
// that channel: launching announces the instance as a thread root, the agent's
// output streams back into the thread, and replying in the thread sends your
// message straight to that instance's agent.
//
// This module is deliberately dependency-free (only node:crypto) so it can be
// unit-tested without a running control plane. The server wires these helpers
// to the live worker WebSocket channels.

import { createHmac, timingSafeEqual } from "node:crypto";

export type SlackConfig = {
  botToken: string;
  signingSecret: string;
  channel: string;
  enabled: boolean;
};

// The Exla shared channel. Override per-deploy with RUDDER_SLACK_CHANNEL.
export const DEFAULT_SLACK_CHANNEL = "C0B78TDLM5G";

export function slackConfigFromEnv(env: NodeJS.ProcessEnv = process.env): SlackConfig {
  const botToken = (env.SLACK_BOT_TOKEN || "").trim();
  const signingSecret = (env.SLACK_SIGNING_SECRET || "").trim();
  const channel = (env.RUDDER_SLACK_CHANNEL || DEFAULT_SLACK_CHANNEL).trim();
  return {
    botToken,
    signingSecret,
    channel,
    // Fail closed: Slack is only "enabled" when BOTH the bot token and the signing
    // secret are present, so inbound events are never processed without a verifiable
    // signature (an unset signing secret used to skip verification entirely).
    enabled: Boolean(botToken && signingSecret),
  };
}

// Verify a Slack request signature (v0 scheme). Returns false when the secret is
// unset so an unconfigured deploy fails closed for inbound events.
export function verifySlackSignature(params: {
  signingSecret: string;
  timestamp: string | undefined;
  signature: string | undefined;
  rawBody: string;
  now?: number;
}): boolean {
  const { signingSecret, timestamp, signature, rawBody } = params;
  if (!signingSecret || !timestamp || !signature) {
    return false;
  }
  const ts = Number(timestamp);
  if (!Number.isFinite(ts)) {
    return false;
  }
  // Reject anything older than 5 minutes to blunt replay attacks.
  const now = params.now ?? Date.now();
  if (Math.abs(now / 1000 - ts) > 60 * 5) {
    return false;
  }
  const base = `v0:${timestamp}:${rawBody}`;
  const expected = `v0=${createHmac("sha256", signingSecret).update(base).digest("hex")}`;
  const a = Buffer.from(expected);
  const b = Buffer.from(signature);
  if (a.length !== b.length) {
    return false;
  }
  return timingSafeEqual(a, b);
}

// OSC sequences (e.g. window-title sets): ESC ] ... BEL or ESC backslash.
const OSC_RE = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;
// CSI sequences: ESC [ params intermediates final.
const CSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;
// Other single ESC-prefixed escapes (charset selects, ESC =, ESC >, etc.).
const ESC_RE = /\x1b[@-Z\\-_=>()#][0-9;]*/g;
// Lone control bytes left over (keep \t and \n).
const CTRL_RE = /[\x00-\x08\x0b-\x1f\x7f]/g;

export function stripAnsi(text: string): string {
  return text
    .replace(OSC_RE, "")
    .replace(CSI_RE, "")
    .replace(ESC_RE, "")
    .replace(CTRL_RE, "");
}

// Turn raw terminal output into something legible in a Slack code block: strip
// ANSI, collapse runs of blank lines, drop trailing whitespace, and keep only
// the last `maxChars` so we never blow past Slack's 3000-char section limit.
export function formatOutputForSlack(raw: string, maxChars = 2600): string {
  const cleaned = stripAnsi(raw)
    .split("\n")
    .map((line) => line.replace(/\s+$/g, ""))
    .join("\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
  if (!cleaned) {
    return "_(no output yet)_";
  }
  const tail = cleaned.length > maxChars ? `…\n${cleaned.slice(cleaned.length - maxChars)}` : cleaned;
  return "```\n" + tail + "\n```";
}

// chat.postMessage. Returns the posted message ts (thread root) on success.
export async function postSlackMessage(params: {
  botToken: string;
  channel: string;
  text: string;
  threadTs?: string;
  fetchImpl?: typeof fetch;
}): Promise<{ ok: boolean; ts?: string; error?: string }> {
  const fetchImpl = params.fetchImpl ?? fetch;
  try {
    const res = await fetchImpl("https://slack.com/api/chat.postMessage", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${params.botToken}`,
        "Content-Type": "application/json; charset=utf-8",
      },
      body: JSON.stringify({
        channel: params.channel,
        text: params.text,
        thread_ts: params.threadTs,
        unfurl_links: false,
        unfurl_media: false,
      }),
    });
    const data = (await res.json()) as { ok?: boolean; ts?: string; error?: string };
    if (!data.ok) {
      return { ok: false, error: data.error || "post_failed" };
    }
    return { ok: true, ts: data.ts };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? error.message : String(error) };
  }
}

export type SlackCommand =
  | { action: "list" }
  | { action: "help" }
  | { action: "repos" }
  | { action: "launch"; repo: string; task: string }
  | { action: "talk"; id: string; message: string }
  | { action: "output"; id: string }
  | { action: "stop"; id: string }
  | { action: "pause"; id: string }
  | { action: "resume"; id: string }
  | { action: "thread-reply"; message: string };

// Parse an @mention / slash-command body into an instruction. Bot user mentions
// (`<@U123>`) are stripped first. `inThread` flips a bare message into a reply
// routed at the thread's instance instead of requiring an explicit id.
export function parseSlackCommand(rawText: string, opts: { inThread: boolean }): SlackCommand {
  const text = rawText.replace(/<@[A-Z0-9]+>/gi, "").trim();
  const lower = text.toLowerCase();
  if (!text) {
    return opts.inThread ? { action: "thread-reply", message: "" } : { action: "help" };
  }
  if (lower === "list" || lower === "ls" || lower === "instances") {
    return { action: "list" };
  }
  if (lower === "help" || lower === "?") {
    return { action: "help" };
  }
  if (lower === "repos" || lower === "repositories" || lower === "snapshots") {
    return { action: "repos" };
  }
  // `launch <repo> <task…>` — start a brand-new cloud agent from the repo's most
  // recent snapshot. Matched before the generic id-first fallback so `run rudder
  // fix the tests` never reads "rudder" as an instance id.
  const launchMatch = text.match(/^(?:launch|run|start)\s+(\S+)\s+([\s\S]+)$/i);
  if (launchMatch) {
    return { action: "launch", repo: launchMatch[1], task: launchMatch[2].trim() };
  }
  const talkMatch = text.match(/^(?:talk|tell|send|msg)\s+(\S+)\s+([\s\S]+)$/i);
  if (talkMatch) {
    return { action: "talk", id: talkMatch[1], message: talkMatch[2].trim() };
  }
  const outputMatch = text.match(/^(?:output|logs?|tail)\s+(\S+)$/i);
  if (outputMatch) {
    return { action: "output", id: outputMatch[1] };
  }
  const stopMatch = text.match(/^(?:stop|kill|cancel)\s+(\S+)$/i);
  if (stopMatch) {
    return { action: "stop", id: stopMatch[1] };
  }
  const pauseMatch = text.match(/^pause\s+(\S+)$/i);
  if (pauseMatch) {
    return { action: "pause", id: pauseMatch[1] };
  }
  const resumeMatch = text.match(/^(?:resume|wake)\s+(\S+)$/i);
  if (resumeMatch) {
    return { action: "resume", id: resumeMatch[1] };
  }
  // `@rudder cloud-foo do the thing` — first token is an instance id.
  const idFirst = text.match(/^(cloud-[a-z0-9-]+|[a-z0-9]{6,})\s+([\s\S]+)$/i);
  if (idFirst && !opts.inThread) {
    return { action: "talk", id: idFirst[1], message: idFirst[2].trim() };
  }
  if (opts.inThread) {
    return { action: "thread-reply", message: text };
  }
  return { action: "help" };
}

export const SLACK_HELP_TEXT = [
  "*Rudder Cloud* — talk to your cloud agents right here.",
  "",
  "• Reply in an instance's thread to send a message to that agent.",
  "• `launch <repo> <task>` — start a new cloud agent from the repo's latest snapshot",
  "• `repos` — show repos with a snapshot ready to launch from",
  "• `list` — show running cloud instances",
  "• `talk <id> <message>` — send a message to an instance",
  "• `output <id>` — show an instance's latest output",
  "• `pause <id>` / `resume <id>` — suspend or wake an instance",
  "• `stop <id>` — stop an instance",
  "",
  "Talking to a paused instance wakes it automatically and delivers your",
  "message once it reconnects.",
  "",
  "You can also spin one up from your terminal — it auto-announces here:",
  "```\nrudder cloud \"fix the failing tests\"\n```",
].join("\n");
