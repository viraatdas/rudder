import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { startBoardDaemon } from "../dist/board/daemon.js";
import {
  classifyCommand,
  localDay,
  parseClaudeLog,
  parseClaudeQuota,
  parseCodexLog,
  parseCodexQuota,
  parsePsOutput,
  parseVmStat,
  priceFor,
  projectFor,
  rangeSince,
  scanTokenUsage,
} from "../dist/usage.js";

async function tmpdir(t, name) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), `rudder-usage-${name}-`));
  t.after(() => fsp.rm(root, { recursive: true, force: true }).catch(() => {}));
  return root;
}

function iso(daysAgo, hour = 12) {
  const d = new Date();
  d.setDate(d.getDate() - daysAgo);
  d.setHours(hour, 0, 0, 0);
  return d.toISOString();
}

function claudeLine({ ts, id, requestId, model, cwd, session, input, output, cw, cr }) {
  return JSON.stringify({
    type: "assistant",
    timestamp: ts,
    cwd,
    requestId,
    sessionId: session,
    message: {
      id,
      model,
      usage: {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cw,
        cache_read_input_tokens: cr,
      },
    },
  });
}

test("pricing covers every family the CLIs launch and refuses to price the unknown", () => {
  assert.deepEqual(priceFor("claude-fable-5-1"), { input: 15, output: 75, cacheWrite: 18.75, cacheRead: 1.5 });
  assert.deepEqual(priceFor("claude-opus-5"), priceFor("claude-fable-5-1"));
  assert.equal(priceFor("claude-sonnet-5").input, 3);
  assert.equal(priceFor("claude-haiku-4-5-20251001").output, 4);
  assert.equal(priceFor("gpt-5.6-sol").input, 10);
  assert.equal(priceFor("gpt-6-astra").output, 30);
  assert.equal(priceFor("mystery-model"), null, "unknown models cost nothing rather than a made-up rate");
});

test("a streamed Claude message that appears several times is counted once, with its final usage", async (t) => {
  const dir = await tmpdir(t, "claude-dedupe");
  const file = path.join(dir, "session.jsonl");
  const ts = iso(1);
  const base = { ts, id: "msg_1", requestId: "req_1", model: "claude-sonnet-5", cwd: "/repo", session: "s1" };
  await fsp.writeFile(
    file,
    [
      claudeLine({ ...base, input: 10, output: 5, cw: 0, cr: 0 }),
      claudeLine({ ...base, input: 10, output: 40, cw: 100, cr: 200 }),
      claudeLine({ ...base, id: "msg_2", requestId: "req_2", input: 1, output: 1, cw: 0, cr: 0 }),
      JSON.stringify({ type: "user", timestamp: ts, message: { role: "user" } }),
      "not json at all",
    ].join("\n"),
  );
  const events = await parseClaudeLog(file);
  assert.equal(events.length, 2);
  assert.deepEqual(
    { input: events[0].input, output: events[0].output, cw: events[0].cacheWrite, cr: events[0].cacheRead },
    { input: 10, output: 40, cw: 100, cr: 200 },
    "the last occurrence wins",
  );
  assert.equal(events[0].sessionId, "s1");
  assert.equal(events[0].cwd, "/repo");
});

test("Codex cumulative token counters are turned into per-response deltas", async (t) => {
  const dir = await tmpdir(t, "codex-deltas");
  const file = path.join(dir, "rollout.jsonl");
  const t0 = iso(1, 9);
  const t1 = iso(1, 10);
  const t2 = iso(1, 11);
  const count = (ts, input, cached, output) =>
    JSON.stringify({
      type: "event_msg",
      timestamp: ts,
      payload: {
        type: "token_count",
        info: {
          total_token_usage: { input_tokens: input, cached_input_tokens: cached, output_tokens: output },
        },
      },
    });
  await fsp.writeFile(
    file,
    [
      JSON.stringify({ type: "session_meta", payload: { cwd: "/repo", id: "codex-1", timestamp: t0 } }),
      JSON.stringify({ type: "turn_context", timestamp: t0, payload: { model: "gpt-5.6-sol" } }),
      count(t0, 1000, 800, 50),
      count(t1, 1600, 1300, 120),
      // Counter reset (compaction / resumed thread): count the new total from zero.
      count(t2, 300, 0, 10),
    ].join("\n"),
  );
  const events = await parseCodexLog(file);
  assert.equal(events.length, 3);
  assert.deepEqual(
    events.map((e) => [e.input, e.cacheRead, e.output]),
    [
      [200, 800, 50],
      [100, 500, 70],
      [300, 0, 10],
    ],
  );
  assert.ok(events.every((e) => e.model === "gpt-5.6-sol" && e.sessionId === "codex-1" && e.cwd === "/repo"));
});

test("the scan buckets by local day, prices at list rates, and maps cwd onto registered projects", async (t) => {
  const root = await tmpdir(t, "scan");
  const claudeRoot = path.join(root, "claude-projects", "-repo");
  const codexRoot = path.join(root, "codex-sessions", "2026", "09", "09");
  await fsp.mkdir(claudeRoot, { recursive: true });
  await fsp.mkdir(codexRoot, { recursive: true });

  const recent = iso(0, 8);
  const older = iso(3, 8);
  const ancient = iso(20, 8);
  await fsp.writeFile(
    path.join(claudeRoot, "a.jsonl"),
    [
      claudeLine({ ts: recent, id: "m1", requestId: "r1", model: "claude-opus-5", cwd: "/repo", session: "s-a", input: 1_000_000, output: 0, cw: 0, cr: 0 }),
      claudeLine({ ts: older, id: "m2", requestId: "r2", model: "claude-opus-5", cwd: "/repo/sub", session: "s-a", input: 0, output: 0, cw: 0, cr: 1_000_000 }),
      claudeLine({ ts: ancient, id: "m3", requestId: "r3", model: "claude-opus-5", cwd: "/repo", session: "s-old", input: 5_000_000, output: 0, cw: 0, cr: 0 }),
    ].join("\n"),
  );
  await fsp.writeFile(
    path.join(codexRoot, "rollout.jsonl"),
    [
      JSON.stringify({ type: "session_meta", payload: { cwd: "/elsewhere/thing", id: "c-1", timestamp: recent } }),
      JSON.stringify({ type: "turn_context", timestamp: recent, payload: { model: "gpt-5.6-sol" } }),
      JSON.stringify({
        type: "event_msg",
        timestamp: recent,
        payload: { type: "token_count", info: { total_token_usage: { input_tokens: 1_000_000, cached_input_tokens: 0, output_tokens: 0 } } },
      }),
    ].join("\n"),
  );

  const report = await scanTokenUsage({
    range: "7d",
    claudeProjectsRoot: path.join(root, "claude-projects"),
    codexSessionsRoot: path.join(root, "codex-sessions"),
    projects: [{ slug: "repo", name: "Repo", repoRoot: "/repo" }],
  });

  assert.equal(report.range, "7d");
  assert.equal(report.days.length, 7, "one bucket per day in range, including empty ones");
  assert.equal(report.days[report.days.length - 1].day, localDay(new Date()));
  // 1M opus input = $15; 1M opus cache read = $1.50; 1M gpt input = $10. The
  // 20-day-old message is outside the range and must not count.
  assert.ok(Math.abs(report.providers.claude.cost - 16.5) < 1e-9, `claude cost ${report.providers.claude.cost}`);
  assert.ok(Math.abs(report.providers.codex.cost - 10) < 1e-9, `codex cost ${report.providers.codex.cost}`);
  assert.ok(Math.abs(report.total.cacheSavings - 13.5) < 1e-9, "cache read saved (15 − 1.5) per M");
  assert.equal(report.total.calls, 3);

  const today = report.days.find((d) => d.day === localDay(recent));
  assert.ok(today && Math.abs(today.claude.cost - 15) < 1e-9, "today's bucket has today's message only");

  assert.equal(report.models.length, 2);
  assert.equal(report.models[0].model, "claude-opus-5", "sorted by cost");
  assert.equal(report.models[0].sessions, 1);

  const repo = report.projects.find((p) => p.slug === "repo");
  assert.ok(repo, "cwd under the repo root maps to the registered project");
  assert.ok(Math.abs(repo.totals.cost - 16.5) < 1e-9);
  assert.equal(repo.sessions.length, 1);
  assert.equal(repo.sessions[0].id, "s-a");
  const other = report.projects.find((p) => p.slug === null);
  assert.equal(other.name, "thing", "unregistered cwd falls back to its basename");
});

test("rangeSince starts at local midnight N-1 days back so a 7d range holds seven calendar days", () => {
  const now = new Date("2026-09-09T15:30:00");
  const since = rangeSince("7d", now);
  assert.equal(localDay(since), "2026-09-03");
  assert.equal(since.getHours(), 0);
  assert.equal(localDay(rangeSince("30d", now)), "2026-08-11");
});

test("Claude quota meters come from the OAuth usage body and the plan from the profile tier", () => {
  const usage = {
    five_hour: { utilization: 14.0, resets_at: "2026-09-10T02:30:00+00:00" },
    seven_day: { utilization: 17.4, resets_at: "2026-09-13T23:00:00+00:00" },
    seven_day_opus: null,
    extra_usage: { is_enabled: false, utilization: 0 },
  };
  const profile = { account: { email: "me@example.com" }, organization: { rate_limit_tier: "default_claude_max_20x" } };
  const quota = parseClaudeQuota(usage, profile, "max");
  assert.equal(quota.email, "me@example.com");
  assert.equal(quota.plan, "Max 20x");
  assert.deepEqual(
    quota.windows.map((w) => [w.label, w.percent]),
    [
      ["Session (5h)", 14],
      ["Weekly", 17],
    ],
  );
  assert.equal(quota.error, null);

  const bare = parseClaudeQuota({}, null, "pro");
  assert.equal(bare.plan, "Pro");
  assert.match(bare.error, /does not expose limits/);
});

test("Codex quota meters label windows by length and include the extra per-model limits", () => {
  const body = {
    email: "me@example.com",
    plan_type: "pro",
    rate_limit: {
      primary_window: { used_percent: 9, limit_window_seconds: 604800, reset_at: 1789586739 },
      secondary_window: null,
    },
    additional_rate_limits: [
      {
        limit_name: "GPT-5.3-Codex-Spark",
        rate_limit: {
          primary_window: { used_percent: 0, limit_window_seconds: 18000, reset_at: 1789009709 },
          secondary_window: { used_percent: 3, limit_window_seconds: 604800, reset_at: 1789596509 },
        },
      },
    ],
  };
  const quota = parseCodexQuota(body);
  assert.equal(quota.email, "me@example.com");
  assert.equal(quota.plan, "Pro");
  assert.deepEqual(
    quota.windows.map((w) => [w.label, w.percent]),
    [
      ["Weekly", 9],
      ["GPT-5.3-Codex-Spark · Session (5h)", 0],
      ["GPT-5.3-Codex-Spark · Weekly", 3],
    ],
  );
  assert.equal(quota.windows[0].resetsAt, new Date(1789586739 * 1000).toISOString());
});

test("ps output is reduced to agent processes, classified and sorted by cpu", () => {
  const text = [
    "  101   1  0.0  1000 01:00 /sbin/launchd",
    "  202 101 12.5 400000 02:30 /Users/x/.local/share/claude/versions/2.1.267 --settings /tmp/s.json",
    "  303 101  3.0 200000 00:10 node /opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js resume abc",
    "  404 202 40.0  90000 00:05 /Users/x/code/rudder/dist/native/rudder-native",
    "  505 101  0.5  50000 00:01 /Users/x/.opencode/bin/opencode",
    "  606 101  1.0  70000 00:01 /Applications/Safari.app/Contents/MacOS/Safari",
  ].join("\n");
  const rows = parsePsOutput(text);
  assert.deepEqual(
    rows.map((r) => [r.pid, r.kind, r.cpu]),
    [
      [404, "rudder", 40],
      [202, "claude", 12.5],
      [303, "codex", 3],
      [505, "opencode", 0.5],
    ],
  );
  assert.equal(rows[1].rss, 400000 * 1024, "rss is reported in KB by ps");
  assert.equal(classifyCommand("/usr/bin/vim"), "other");
  // The executable decides, never the argv: a Claude worker launched by rudder
  // carries rudder paths in its arguments and was filed under rudder.
  assert.equal(classifyCommand("claude --permission-mode bypassPermissions --settings /Users/x/.rudder/signals/run-claude.json"), "claude");
  assert.equal(classifyCommand("node /Users/x/.local/lib/node_modules/@viraatdas/rudder/dist/index.js --cwd /Users/x/code/claude"), "rudder");
  assert.equal(classifyCommand("/Users/x/code/rudder/dist/native/rudder-native"), "rudder");
  assert.equal(classifyCommand("/Applications/Safari.app/Contents/MacOS/Safari https://rudder.viraat.dev"), "other");
});

test("vm_stat pages that macOS can reclaim count as available memory", () => {
  const text = [
    "Mach Virtual Memory Statistics: (page size of 16384 bytes)",
    "Pages free:                               10000.",
    "Pages active:                            500000.",
    "Pages inactive:                          200000.",
    "Pages speculative:                        30000.",
    "Pages throttled:                              0.",
    "Pages wired down:                        300000.",
    "Pages purgeable:                          20000.",
  ].join("\n");
  assert.equal(parseVmStat(text), (10000 + 200000 + 30000 + 20000) * 16384);
  assert.equal(parseVmStat("garbage"), null);
});

test("the board serves the usage page and its read-only api", async (t) => {
  const root = await tmpdir(t, "daemon");
  const repo = path.join(root, "repo");
  const home = path.join(root, "home");
  await fsp.mkdir(repo, { recursive: true });
  await fsp.mkdir(home, { recursive: true });
  const saved = { RUDDER_HOME: process.env.RUDDER_HOME, CLAUDE_CONFIG_DIR: process.env.CLAUDE_CONFIG_DIR, CODEX_HOME: process.env.CODEX_HOME };
  process.env.RUDDER_HOME = home;
  // Point both CLIs' homes at empty dirs so the scan is instant and hermetic.
  process.env.CLAUDE_CONFIG_DIR = path.join(root, "claude");
  process.env.CODEX_HOME = path.join(root, "codex");
  t.after(() => {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  });
  const daemon = await startBoardDaemon({ port: 0, repoRoot: repo });
  t.after(() => daemon.close());

  const shell = await (await fetch(`${daemon.url}/usage`)).text();
  assert.match(shell, /__RUDDER_VIEW__ = "usage"/);
  assert.match(shell, /<title>rudder · usage<\/title>/);

  const tokens = await (await fetch(`${daemon.url}/api/usage/tokens?range=7d`)).json();
  assert.equal(tokens.range, "7d");
  assert.equal(tokens.days.length, 7);
  assert.equal(tokens.total.cost, 0);
  assert.equal(tokens.scannedFiles, 0);

  const bogus = await (await fetch(`${daemon.url}/api/usage/tokens?range=1y`)).json();
  assert.equal(bogus.range, "30d", "unknown ranges fall back to the default");

  const machine = await (await fetch(`${daemon.url}/api/usage/machine`)).json();
  assert.ok(machine.totalMem > 0);
  assert.ok(Array.isArray(machine.processes));
  assert.ok(machine.byKind.rudder.count >= 0);
});

test("a project registered at a parent directory does not swallow the repos beneath it", () => {
  const projects = [
    { slug: "code", name: "code", repoRoot: "/u/code" },
    { slug: "rudder", name: "rudder", repoRoot: "/u/code/rudder" },
  ];
  const roots = { "/u/code/rudder": "/u/code/rudder", "/u/code/rudder/native": "/u/code/rudder", "/u/code/libra": "/u/code/libra", "/u/code/libra/x": "/u/code/libra", "/u/code": null, "/u/scratch": null };
  const repoRootOf = (cwd) => roots[cwd] ?? null;
  assert.equal(projectFor("/u/code/rudder/native", projects, repoRootOf).slug, "rudder");
  const libra = projectFor("/u/code/libra/x", projects, repoRootOf);
  assert.deepEqual(libra, { key: "/u/code/libra", name: "libra", slug: null }, "an unregistered repo is its own workspace");
  assert.equal(projectFor("/u/code", projects, repoRootOf).slug, "code", "the parent itself still maps to its project");
  assert.deepEqual(projectFor("/u/scratch", projects, repoRootOf), { key: "/u/scratch", name: "scratch", slug: null });
  assert.equal(
    projectFor("/u/code/rudder/.rudder-workspaces/agent-1/native", projects, (p) => p).slug,
    "rudder",
    "a worker workspace (its own jj checkout) counts for the repo that owns it",
  );
  assert.deepEqual(
    projectFor("/u/code/libra/.rudder-workspaces/w1", projects, (p) => p),
    { key: "/u/code/libra", name: "libra", slug: null },
  );
});
