#!/usr/bin/env node
// Guide loop: watch a live Rudder session and steer it toward a product goal.
//
// Every tick this builds a deterministic digest of the session (run rows, DAG,
// plan queue, agent signal states, transcript tails, git state, and what the
// previous ticks already said), hands it to a Sonnet guide, and applies the
// guide's steer decisions through Rudder's steer inbox (.rudder/steer/*.json),
// which the running native TUI polls once a second and injects into the target
// agent's live PTY.
//
// The model decides WHAT to say. This script owns the rails: which targets are
// addressable, how many steers per tick, per-target cooldown, duplicate
// suppression, and the durable ledger of what was sent and what was delivered.
//
//   node scripts/guide-loop.mjs --repo ~/code/wagerpals --goals goals.md
//   node scripts/guide-loop.mjs --repo ~/code/wagerpals --once --dry-run

import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";

const DEFAULTS = {
  interval: 420,
  model: "sonnet",
  maxSteersPerTick: 2,
  cooldownMinutes: 12,
  hourlyBudget: 6,
  guideTimeoutMs: 10 * 60 * 1000,
};

// ---------------------------------------------------------------------------
// args
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const out = { flags: new Set(), opts: {} };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const next = argv[i + 1];
    if (next && !next.startsWith("--")) {
      out.opts[key] = next;
      i += 1;
    } else {
      out.flags.add(key);
    }
  }
  return out;
}

function expandHome(p) {
  if (!p) return p;
  return p.startsWith("~") ? path.join(os.homedir(), p.slice(1)) : p;
}

// ---------------------------------------------------------------------------
// small io helpers
// ---------------------------------------------------------------------------

function readJsonSync(file, fallback = null) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return fallback;
  }
}

function readLinesTail(file, maxLines) {
  try {
    const raw = fs.readFileSync(file, "utf8");
    const lines = raw.split("\n").filter((line) => line.trim().length > 0);
    return lines.slice(-maxLines);
  } catch {
    return [];
  }
}

function trim(text, max) {
  if (typeof text !== "string") return "";
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max)}…` : flat;
}

function ageLabel(ms) {
  if (!Number.isFinite(ms) || ms < 0) return "?";
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  return `${hours}h${mins % 60}m ago`;
}

function pidAlive(pid) {
  if (!pid) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function git(repo, args) {
  try {
    return execFileSync("git", args, { cwd: repo, encoding: "utf8", timeout: 15000 }).trim();
  } catch {
    return "";
  }
}

// ---------------------------------------------------------------------------
// session digest
// ---------------------------------------------------------------------------

const CLAUDE_PROJECTS = path.join(os.homedir(), ".claude", "projects");
const SIGNALS_DIR = path.join(os.homedir(), ".rudder", "signals");

/** Rudder's live per-agent state: "input" = blocked on a question, "done" = finished its turn. */
function signalState(runId) {
  const value = readJsonSync(path.join(SIGNALS_DIR, `${runId}.json`));
  return value?.state ?? null;
}

/**
 * The last few things an agent actually did, read off its Claude Code
 * transcript. Statuses say an agent is "running"; only the transcript says
 * whether it is building the right thing.
 */
function transcriptTail(sessionId, maxEvents = 14) {
  if (!sessionId) return [];
  let file = null;
  try {
    for (const dir of fs.readdirSync(CLAUDE_PROJECTS)) {
      const candidate = path.join(CLAUDE_PROJECTS, dir, `${sessionId}.jsonl`);
      if (fs.existsSync(candidate)) {
        file = candidate;
        break;
      }
    }
  } catch {
    return [];
  }
  if (!file) return [];

  const events = [];
  for (const line of readLinesTail(file, 400)) {
    let entry;
    try {
      entry = JSON.parse(line);
    } catch {
      continue;
    }
    const content = entry?.message?.content;
    if (!Array.isArray(content)) continue;
    for (const block of content) {
      if (block?.type === "text" && block.text?.trim()) {
        events.push(`say: ${trim(block.text, 220)}`);
      } else if (block?.type === "tool_use") {
        const input = block.input ?? {};
        const detail =
          input.file_path ?? input.command ?? input.pattern ?? input.prompt ?? input.description ?? "";
        events.push(`tool ${block.name}: ${trim(String(detail), 120)}`);
      }
    }
  }
  const mtime = (() => {
    try {
      return fs.statSync(file).mtimeMs;
    } catch {
      return 0;
    }
  })();
  const tail = events.slice(-maxEvents);
  return { events: tail, quietFor: ageLabel(Date.now() - mtime) };
}

function collectRuns(repo) {
  const runsDir = path.join(repo, ".rudder", "runs");
  let dirs = [];
  try {
    dirs = fs.readdirSync(runsDir);
  } catch {
    return [];
  }
  const now = Date.now();
  const rows = [];
  for (const dir of dirs) {
    const record = readJsonSync(path.join(runsDir, dir, "run.json"));
    if (!record?.id) continue;
    const updatedAt = Number(record.updatedAt ?? record.createdAt ?? 0);
    // Anything untouched for a day belongs to an earlier session, not this one.
    if (now - updatedAt > 24 * 60 * 60 * 1000) continue;
    const tail = transcriptTail(record.session?.nativeSessionId);
    rows.push({
      id: record.id,
      node: record.nodeId ?? null,
      status: record.status,
      phase: record.lifecyclePhase ?? null,
      mode: record.mode,
      backend: record.backend,
      model: record.model,
      effort: record.effort,
      summary: record.taskSummary ?? trim(record.task, 90),
      task: trim(record.task, 400),
      currentPrompt: trim(record.currentPrompt, 240),
      doneSummary: trim(record.doneSummary, 300),
      delivery: record.delivery?.status ?? null,
      worktree: record.worktree?.path ?? null,
      turns: Array.isArray(record.turns) ? record.turns.length : 0,
      lastUpdate: ageLabel(now - updatedAt),
      idleMinutes: Math.round((now - updatedAt) / 60000),
      signal: signalState(record.id),
      pid: record.process?.pid ?? null,
      alive: pidAlive(record.process?.pid),
      recent: tail.events ?? [],
      transcriptQuietFor: tail.quietFor ?? null,
    });
  }
  rows.sort((a, b) => a.idleMinutes - b.idleMinutes);
  return rows;
}

function collectDigest(repo, memory) {
  const stateDir = path.join(repo, ".rudder");
  const runs = collectRuns(repo);
  const graph = readJsonSync(path.join(stateDir, "graph.json"), {});
  const planQueue = readJsonSync(path.join(stateDir, "plan-queue.json"), { plans: [] });

  const activity = readLinesTail(path.join(stateDir, "activity.jsonl"), 200)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
  const narration = activity.filter((entry) => entry.kind !== "heartbeat").slice(-25);
  const lastHeartbeat = activity.filter((entry) => entry.kind === "heartbeat").slice(-1)[0] ?? null;

  const doneDir = path.join(stateDir, "done");
  const done = [];
  try {
    for (const file of fs.readdirSync(doneDir).slice(-12)) {
      const report = readJsonSync(path.join(doneDir, file));
      if (report) done.push({ node: file.replace(/\.json$/, ""), ...report });
    }
  } catch {
    /* no done reports yet */
  }

  const nodes = Object.entries(graph.nodes ?? {}).map(([id, node]) => ({
    id,
    status: node.status ?? node.state ?? null,
    title: trim(node.title ?? node.task ?? "", 100),
  }));

  return {
    repo,
    now: new Date().toISOString(),
    sessionLive: runs.some((run) => run.alive),
    git: {
      branch: git(repo, ["rev-parse", "--abbrev-ref", "HEAD"]),
      dirtyFiles: git(repo, ["status", "--porcelain"]).split("\n").filter(Boolean).length,
      recentCommits: git(repo, ["log", "--oneline", "-12"]).split("\n").filter(Boolean),
    },
    plans: (planQueue.plans ?? []).map((plan) => ({
      id: plan.id,
      awaitingApproval: plan.awaiting_approval,
      finalGate: plan.final_gate_status,
      planSummary: trim(plan.plan_summary, 600),
      planned: plan.planned_nodes?.length ?? 0,
      launched: plan.launched_node_ids?.length ?? 0,
      merged: plan.merged_node_ids?.length ?? 0,
      request: trim(plan.plan_request, 1200),
    })),
    graphNodes: nodes,
    graphEdges: graph.edges ?? {},
    runs,
    doneReports: done,
    narration,
    lastHeartbeat,
    previousTicks: memory.ticks.slice(-4),
    openWatchlist: memory.watchlist ?? [],
    steerOutcomes: memory.pendingReceipts ?? [],
  };
}

// ---------------------------------------------------------------------------
// guide memory + ledger
// ---------------------------------------------------------------------------

function loadMemory(stateDir) {
  const memory = readJsonSync(path.join(stateDir, "state.json"), null);
  return {
    ticks: memory?.ticks ?? [],
    watchlist: memory?.watchlist ?? [],
    sentSteers: memory?.sentSteers ?? [],
    pendingReceipts: memory?.pendingReceipts ?? [],
    alerts: memory?.alerts ?? [],
  };
}

async function saveMemory(stateDir, memory) {
  await fsp.mkdir(stateDir, { recursive: true });
  const trimmed = {
    ticks: memory.ticks.slice(-12),
    watchlist: memory.watchlist.slice(0, 20),
    sentSteers: memory.sentSteers.slice(-40),
    pendingReceipts: memory.pendingReceipts.slice(-20),
    alerts: (memory.alerts ?? []).slice(-20),
  };
  await fsp.writeFile(path.join(stateDir, "state.json"), `${JSON.stringify(trimmed, null, 2)}\n`);
}

async function appendLedger(stateDir, entry) {
  await fsp.mkdir(stateDir, { recursive: true });
  await fsp.appendFile(
    path.join(stateDir, "ledger.jsonl"),
    `${JSON.stringify({ ts: new Date().toISOString(), ...entry })}\n`,
  );
}

/** Read receipts Rudder wrote for steers sent on earlier ticks, so the guide learns what landed. */
function collectReceipts(repo, pending) {
  const dir = path.join(repo, ".rudder", "steer-receipts");
  const settled = [];
  const stillPending = [];
  for (const item of pending) {
    const receipt = readJsonSync(path.join(dir, `${item.requestId}.json`));
    if (receipt && receipt.status && receipt.status !== "processing") {
      settled.push({ ...item, outcome: receipt.status, detail: receipt.error ?? receipt.detail ?? null });
    } else {
      stillPending.push(item);
    }
  }
  return { settled, stillPending };
}

// ---------------------------------------------------------------------------
// the guide call
// ---------------------------------------------------------------------------

const GUIDE_CONTRACT = `You are the guide for a live Rudder fleet: a conductor agent plus worker agents
building a product in one repo. You do not write code. You read the session
state, judge whether the fleet is actually converging on the product goal, and
either stay silent or send a small number of steers.

You are the only thing standing between "agents are busy" and "the product is
good". Judge OUTCOMES, not activity: is the app shippable, does the flow work
end to end, is the design good, is anything in the goal file quietly dropped?

Available tools are read-only (Read, Grep, Glob, and read-only git/ls/rg via
Bash). Use them to verify claims before you steer: if an agent says a screen is
done, open the file. A steer built on an unverified claim is worse than silence.

Steer targets:
- "conductor" — the orchestrator. Steering it starts NEW work that folds into
  the plan (a missed requirement, a whole area nobody owns, a re-plan). Use
  sparingly; it costs a planning pass.
- a run id or node id from the digest (e.g. "n3") — text goes straight into that
  agent's live prompt. Use for course corrections, unblocking a question, or
  raising the quality bar on work in flight.

Rules:
- Silence is the default and a valid answer. Steer only when the session is
  measurably off-goal, blocked, repeating itself, or about to ship something bad.
- Never steer an agent that is mid-flow and on track just to encourage it.
- An agent whose signal is "input" is BLOCKED asking a question — read its
  transcript tail and answer it concretely; that is the highest-value steer.
- Each instruction: concrete, specific, under 120 words, names files/screens/
  behaviors, states the acceptance bar. No praise, no meta-talk about being an
  overseer, no "please".
- Do not repeat an instruction already in previousTicks unless the receipt shows
  it failed to deliver or the agent visibly ignored it.
- Prefer one excellent steer over two mediocre ones.

Some blocks only a human can clear: a plan parked awaiting approval, an agent
asking the user a question, a credential the fleet does not have. No steer fixes
those — set "alertUser" and the human gets pinged. Leave it null otherwise; an
alert that fires every tick is an alert nobody reads.

Reply with ONLY a fenced json block:

\`\`\`json
{
  "assessment": "2-4 sentences: what the fleet is actually doing and whether it is converging",
  "risks": ["short, specific"],
  "steers": [{"target": "conductor|<run-id>|<node-id>", "why": "why now", "instruction": "the exact text to inject"}],
  "alertUser": "one line the human must see now, or null",
  "watchlist": ["things to check next tick"]
}
\`\`\`
Set "steers" to [] when nothing is worth saying.`;

function buildPrompt(goals, digest, maxSteers) {
  return [
    GUIDE_CONTRACT,
    "",
    `You may send at most ${maxSteers} steer(s) this tick.`,
    "",
    "# Product goal (the bar the fleet is being held to)",
    goals.trim(),
    "",
    "# Session digest (JSON)",
    "```json",
    JSON.stringify(digest, null, 1),
    "```",
  ].join("\n");
}

function extractJson(text) {
  const fenced = text.match(/```json\s*([\s\S]*?)```/i) ?? text.match(/```\s*([\s\S]*?)```/);
  const candidate = fenced ? fenced[1] : text.slice(text.indexOf("{"), text.lastIndexOf("}") + 1);
  try {
    return JSON.parse(candidate);
  } catch {
    return null;
  }
}

function runWithStdin(command, args, { cwd, timeoutMs, input }) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd, env: { ...process.env, CLAUDE_CODE_DISABLE_AUTOUPDATER: "1" } });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`${command} timed out after ${Math.round(timeoutMs / 1000)}s`));
    }, timeoutMs);
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) resolve(stdout);
      else reject(new Error(`${command} exited ${code}: ${trim(stderr, 400)}`));
    });
    // A killed or early-exiting child makes this write fail with EPIPE. Without a
    // handler that is an unhandled 'error' event, which takes the whole loop down
    // — one timed-out tick must not end the watch.
    child.stdin.on("error", () => {});
    child.stdin.end(input);
  });
}

async function callGuide({ repo, model, prompt, timeoutMs }) {
  // The prompt goes over stdin: --allowedTools is variadic, so a trailing
  // positional prompt would be parsed as one more tool name.
  const args = [
    "--print",
    "--model",
    model,
    "--output-format",
    "json",
    // The guide needs the repo and nothing else; loading the user's MCP servers
    // costs ~30s of startup per tick and buys nothing here.
    "--strict-mcp-config",
    "--mcp-config",
    '{"mcpServers":{}}',
    "--allowedTools",
    "Read",
    "Grep",
    "Glob",
    "Bash(git log:*)",
    "Bash(git status:*)",
    "Bash(git diff:*)",
    "Bash(git show:*)",
    "Bash(ls:*)",
    "Bash(rg:*)",
    "Bash(find:*)",
    "Bash(cat:*)",
  ];
  const stdout = await runWithStdin("claude", args, { cwd: repo, timeoutMs, input: prompt });
  let payload;
  try {
    payload = JSON.parse(stdout);
  } catch {
    payload = stdout;
  }
  // --output-format json is either the result object or the whole message array
  // depending on CLI version; the final `result` entry is what we want.
  const final = Array.isArray(payload)
    ? payload.filter((entry) => entry?.type === "result").slice(-1)[0] ?? {}
    : payload ?? {};
  const text = typeof final.result === "string" ? final.result : String(stdout);
  return { text, cost: final.total_cost_usd ?? null, decision: extractJson(text) };
}

// ---------------------------------------------------------------------------
// rails + delivery
// ---------------------------------------------------------------------------

function hashInstruction(text) {
  let hash = 0;
  for (const ch of text) hash = (hash * 31 + ch.charCodeAt(0)) | 0;
  return `h${(hash >>> 0).toString(36)}`;
}

/**
 * The model proposes; this decides. Targets must exist, each target has a
 * cooldown, identical instructions never fire twice inside an hour, and the
 * whole loop has an hourly ceiling so a confused guide cannot spam a fleet.
 */
function applyRails({ steers, digest, memory, config }) {
  const now = Date.now();
  const known = new Set(["conductor", "orchestrator"]);
  for (const run of digest.runs) {
    known.add(run.id);
    if (run.node) known.add(run.node);
  }
  const recent = memory.sentSteers.filter((entry) => now - entry.at < 60 * 60 * 1000);
  const accepted = [];
  const rejected = [];

  for (const steer of steers ?? []) {
    const target = String(steer?.target ?? "").trim();
    const instruction = String(steer?.instruction ?? "").trim();
    const reason = (why) => rejected.push({ target, why, instruction: trim(instruction, 120) });

    if (!instruction) {
      reason("empty instruction");
      continue;
    }
    if (!known.has(target)) {
      reason(`unknown target (${target})`);
      continue;
    }
    if (accepted.length >= config.maxSteersPerTick) {
      reason("over per-tick cap");
      continue;
    }
    if (recent.length + accepted.length >= config.hourlyBudget) {
      reason("over hourly budget");
      continue;
    }
    const lastToTarget = recent.filter((entry) => entry.target === target).sort((a, b) => b.at - a.at)[0];
    if (lastToTarget && now - lastToTarget.at < config.cooldownMinutes * 60 * 1000) {
      reason(`cooldown (${config.cooldownMinutes}m) on ${target}`);
      continue;
    }
    const digest64 = hashInstruction(instruction);
    if (recent.some((entry) => entry.hash === digest64)) {
      reason("duplicate of a recent steer");
      continue;
    }
    accepted.push({ target, instruction, why: trim(steer.why, 200), hash: digest64 });
  }
  return { accepted, rejected };
}

/**
 * Ping the human for blocks no steer can clear (approval gates, questions aimed
 * at the user). Rate-limited hard: the same alert never repeats inside an hour,
 * and any alert at most every 20 minutes, because an alert that fires every tick
 * stops being read.
 */
function maybeAlertUser(alert, memory, label) {
  const text = trim(alert, 220);
  if (!text || text === "null") return null;
  const now = Date.now();
  const hash = hashInstruction(text);
  const last = memory.alerts ?? [];
  if (last.some((entry) => entry.hash === hash && now - entry.at < 60 * 60 * 1000)) return null;
  if (last.some((entry) => now - entry.at < 20 * 60 * 1000)) return null;
  memory.alerts = [...last, { at: now, hash, text }].slice(-20);
  try {
    const escaped = text.replace(/["\\]/g, " ");
    execFileSync("osascript", [
      "-e",
      `display notification "${escaped}" with title "Rudder guide — ${label}" sound name "Submarine"`,
    ]);
  } catch {
    /* notifications are best-effort */
  }
  return text;
}

async function deliverSteer(repo, steer) {
  const dir = path.join(repo, ".rudder", "steer");
  await fsp.mkdir(dir, { recursive: true });
  const stamp = Date.now();
  const requestId = `guide-${stamp}-${Math.random().toString(36).slice(2, 8)}`;
  const body = {
    requestId,
    taskId: steer.target,
    instruction: steer.instruction,
    kind: "steer",
    source: "guide-loop",
  };
  const file = path.join(dir, `${stamp}-${requestId}.json`);
  const tmp = `${file}.tmp`;
  await fsp.writeFile(tmp, `${JSON.stringify(body, null, 2)}\n`);
  // Rename so the TUI's poll never reads a half-written request.
  await fsp.rename(tmp, file);
  return requestId;
}

// ---------------------------------------------------------------------------
// tick
// ---------------------------------------------------------------------------

async function tick({ repo, goals, stateDir, config }) {
  const memory = loadMemory(stateDir);

  const { settled, stillPending } = collectReceipts(repo, memory.pendingReceipts);
  memory.pendingReceipts = stillPending;
  for (const item of settled) {
    await appendLedger(stateDir, { event: "receipt", ...item });
  }

  const digest = collectDigest(repo, { ...memory, pendingReceipts: settled });

  if (!digest.sessionLive) {
    console.log(`[guide] ${new Date().toLocaleTimeString()} no live Rudder session in ${repo} — skipping tick`);
    await saveMemory(stateDir, memory);
    return { skipped: true };
  }

  const prompt = buildPrompt(goals, digest, config.maxSteersPerTick);
  let guide;
  try {
    guide = await callGuide({ repo, model: config.model, prompt, timeoutMs: config.guideTimeoutMs });
  } catch (error) {
    console.error(`[guide] guide call failed: ${error.message}`);
    await appendLedger(stateDir, { event: "error", message: error.message });
    return { error: true };
  }

  const decision = guide.decision;
  if (!decision) {
    console.error("[guide] guide returned no parseable JSON");
    await appendLedger(stateDir, { event: "unparseable", raw: trim(guide.text, 2000) });
    return { error: true };
  }

  const { accepted, rejected } = applyRails({
    steers: decision.steers,
    digest,
    memory,
    config,
  });

  const delivered = [];
  for (const steer of accepted) {
    if (config.dryRun) {
      delivered.push({ ...steer, requestId: "dry-run" });
      continue;
    }
    const requestId = await deliverSteer(repo, steer);
    delivered.push({ ...steer, requestId });
    memory.sentSteers.push({ at: Date.now(), target: steer.target, hash: steer.hash });
    memory.pendingReceipts.push({ requestId, target: steer.target, instruction: trim(steer.instruction, 200) });
  }

  const alerted = maybeAlertUser(decision.alertUser, memory, path.basename(repo));

  memory.watchlist = Array.isArray(decision.watchlist) ? decision.watchlist : memory.watchlist;
  memory.ticks.push({
    at: new Date().toISOString(),
    assessment: trim(decision.assessment, 700),
    risks: (decision.risks ?? []).slice(0, 6),
    steers: delivered.map((item) => ({ target: item.target, instruction: trim(item.instruction, 240) })),
    alerted,
  });
  await saveMemory(stateDir, memory);
  await appendLedger(stateDir, {
    event: "tick",
    assessment: decision.assessment,
    risks: decision.risks ?? [],
    delivered,
    rejected,
    alerted,
    cost: guide.cost,
    rows: digest.runs.length,
  });

  const stamp = new Date().toLocaleTimeString();
  console.log(`\n[guide] ${stamp} — ${digest.runs.length} row(s), ${digest.graphNodes.length} node(s)`);
  console.log(`  assessment: ${trim(decision.assessment, 400)}`);
  for (const risk of (decision.risks ?? []).slice(0, 4)) console.log(`  risk: ${trim(risk, 200)}`);
  if (alerted) console.log(`  ALERTED USER: ${alerted}`);
  for (const item of delivered) {
    console.log(`  ${config.dryRun ? "would steer" : "steered"} ${item.target}: ${trim(item.instruction, 220)}`);
  }
  for (const item of rejected) console.log(`  held back (${item.why}): ${item.instruction}`);
  if (!delivered.length && !rejected.length) console.log("  no steer (on track)");
  return { ok: true };
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main() {
  const { flags, opts } = parseArgs(process.argv.slice(2));
  const repo = path.resolve(expandHome(opts.repo ?? process.cwd()));
  if (!fs.existsSync(path.join(repo, ".rudder"))) {
    console.error(`[guide] ${repo} has no .rudder directory — is this a Rudder project?`);
    process.exit(1);
  }
  const stateDir = expandHome(
    opts["state-dir"] ?? path.join(os.homedir(), ".rudder", "guide", path.basename(repo)),
  );
  const goalsPath = expandHome(opts.goals ?? path.join(stateDir, "goals.md"));
  if (!fs.existsSync(goalsPath)) {
    console.error(`[guide] no goals file at ${goalsPath} — write one (what "good" means for this product)`);
    process.exit(1);
  }
  const goals = fs.readFileSync(goalsPath, "utf8");

  const config = {
    model: opts.model ?? DEFAULTS.model,
    maxSteersPerTick: Number(opts["max-steers"] ?? DEFAULTS.maxSteersPerTick),
    cooldownMinutes: Number(opts.cooldown ?? DEFAULTS.cooldownMinutes),
    hourlyBudget: Number(opts["hourly-budget"] ?? DEFAULTS.hourlyBudget),
    guideTimeoutMs: DEFAULTS.guideTimeoutMs,
    dryRun: flags.has("dry-run"),
  };
  const interval = Number(opts.interval ?? DEFAULTS.interval) * 1000;

  console.log(
    `[guide] watching ${repo} (model=${config.model}, every ${interval / 1000}s, ` +
      `max ${config.maxSteersPerTick}/tick, ${config.dryRun ? "DRY RUN" : "live steering"})`,
  );
  console.log(`[guide] goals: ${goalsPath}`);
  console.log(`[guide] ledger: ${path.join(stateDir, "ledger.jsonl")}`);

  let stopping = false;
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => {
      stopping = true;
      console.log("\n[guide] stopping");
      process.exit(0);
    });
  }

  for (;;) {
    try {
      await tick({ repo, goals, stateDir, config });
    } catch (error) {
      console.error(`[guide] tick failed: ${error.stack ?? error.message}`);
      await appendLedger(stateDir, { event: "crash", message: String(error.message) });
    }
    if (flags.has("once") || stopping) return;
    await new Promise((resolve) => setTimeout(resolve, interval));
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
