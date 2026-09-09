// Renders the usage dashboard's pure views server-side with real-shaped
// payloads. The browser bundle is excluded from tsc, so this is what catches a
// view that throws on a loaded report (which the page shows as an endless
// "scanning session logs…", never as an error).
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";
import { h } from "preact";
import { render } from "preact-render-to-string";

import { parseClaudeQuota, parseCodexQuota, scanTokenUsage } from "../dist/usage.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");

async function loadViews(t) {
  // Inside the repo so the bundle's bare `preact` imports resolve to the SAME
  // copy the test renders with (two preact instances would break hooks).
  const cache = path.join(root, "node_modules", ".cache", "rudder-usage-ui");
  await fsp.mkdir(cache, { recursive: true });
  const dir = await fsp.mkdtemp(path.join(cache, "run-"));
  t.after(() => fsp.rm(dir, { recursive: true, force: true }).catch(() => {}));
  const outfile = path.join(dir, "usage.mjs");
  await esbuild.build({
    entryPoints: [path.join(root, "src/board/ui/usage.tsx")],
    bundle: true,
    format: "esm",
    platform: "node",
    jsx: "automatic",
    jsxImportSource: "preact",
    external: ["preact", "preact/hooks", "preact/jsx-runtime"],
    outfile,
    logLevel: "silent",
  });
  return import(outfile);
}

function totals(cost, input = 1000, output = 100, cacheRead = 500) {
  return { input, output, cacheWrite: 50, cacheRead, cost, cacheSavings: cost / 10, calls: 3 };
}

function fixtureReport() {
  return {
    range: "7d",
    since: "2026-09-03T07:00:00.000Z",
    generatedAt: "2026-09-09T20:00:00.000Z",
    days: ["2026-09-03", "2026-09-04", "2026-09-05", "2026-09-06", "2026-09-07", "2026-09-08", "2026-09-09"].map((day, i) => ({
      day,
      claude: totals(i * 3),
      codex: totals(i),
    })),
    providers: { claude: totals(63), codex: totals(21) },
    total: totals(84),
    models: [
      { provider: "claude", model: "claude-fable-5-1", totals: totals(60), sessions: 4 },
      { provider: "codex", model: "gpt-5.6-sol", totals: totals(21), sessions: 2 },
      { provider: "claude", model: "claude-haiku-4-5-20251001", totals: totals(3), sessions: 1 },
    ],
    projects: [
      {
        key: "/u/code/rudder",
        name: "rudder",
        slug: "rudder",
        totals: totals(70),
        sessions: [
          { id: "545b98cc-d01c-4dd5-807a-3b92b50f1755", provider: "claude", model: "claude-fable-5-1", startedAt: "2026-09-09T18:00:00.000Z", lastAt: "2026-09-09T19:30:00.000Z", cwd: "/u/code/rudder", totals: totals(50) },
          { id: "01a08829-b97c-7f90-a664-39bda3a27eae", provider: "codex", model: "gpt-5.6-sol", startedAt: "2026-09-08T18:00:00.000Z", lastAt: "2026-09-08T19:30:00.000Z", cwd: "/u/code/rudder", totals: totals(20) },
        ],
      },
      { key: "/u/code/libra", name: "libra", slug: null, totals: totals(14), sessions: [] },
    ],
    scannedFiles: 12,
    pricingNote: "Estimated at API list rates.",
  };
}

test("the token report view renders a loaded report without throwing", async (t) => {
  const { TokenReportView } = await loadViews(t);
  const html = render(h(TokenReportView, { report: fixtureReport(), error: null, range: "7d", onRange: () => {} }));
  assert.match(html, /At API rates/);
  assert.match(html, /\$84/, "the total at list rates");
  assert.match(html, /fable-5-1/, "model rows use the short model label");
  assert.match(html, /gpt-5\.6-sol/);
  assert.match(html, /<svg class="daily-chart"/, "the daily chart is inline SVG");
  assert.equal((html.match(/class="bar-claude"/g) ?? []).length, 7, "one claude bar per day");
  assert.match(html, /href="\/rudder\/rudder"/, "a registered workspace links to its board");
  assert.match(html, /libra/, "an unregistered workspace is still listed");
  assert.match(html, /Estimated at API list rates\./);
});

test("the token report view shows loading and error states rather than a blank section", async (t) => {
  const { TokenReportView } = await loadViews(t);
  const loading = render(h(TokenReportView, { report: null, error: null, range: "30d", onRange: () => {} }));
  assert.match(loading, /scanning session logs/);
  const failed = render(h(TokenReportView, { report: null, error: "boom", range: "30d", onRange: () => {} }));
  assert.match(failed, /usage failed: boom/);
  assert.doesNotMatch(failed, /scanning session logs/);
});

test("the token report view renders a report produced by the real scanner, including an empty one", async (t) => {
  const { TokenReportView } = await loadViews(t);
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-usage-empty-"));
  t.after(() => fsp.rm(dir, { recursive: true, force: true }).catch(() => {}));
  const report = await scanTokenUsage({
    range: "90d",
    claudeProjectsRoot: path.join(dir, "claude"),
    codexSessionsRoot: path.join(dir, "codex"),
    projects: [],
  });
  const html = render(h(TokenReportView, { report, error: null, range: "90d", onRange: () => {} }));
  assert.match(html, /no usage in this range/);
  assert.match(html, /no workspaces in this range/);
  assert.equal((html.match(/class="bar-codex"/g) ?? []).length, 90);
});

test("quota cards render meters, plan pills, and sign-in errors", async (t) => {
  const { QuotaCard } = await loadViews(t);
  const now = Date.parse("2026-09-09T20:00:00Z");
  const claude = {
    provider: "claude",
    fetchedAt: new Date(now).toISOString(),
    ...parseClaudeQuota(
      { five_hour: { utilization: 22, resets_at: "2026-09-10T02:30:00+00:00" }, seven_day: { utilization: 91, resets_at: "2026-09-13T23:00:00+00:00" } },
      { account: { email: "me@example.com" }, organization: { rate_limit_tier: "default_claude_max_20x" } },
      "max",
    ),
  };
  const html = render(h(QuotaCard, { account: claude, now }));
  assert.match(html, /Max 20x/);
  assert.match(html, /me@example\.com/);
  assert.match(html, /aria-valuenow="22"/);
  assert.match(html, /meter-hot/, "a nearly exhausted window is highlighted");
  assert.match(html, /resets in 6h 30m/);

  const codex = { provider: "codex", fetchedAt: new Date(now).toISOString(), ...parseCodexQuota({ plan_type: "pro", rate_limit: { primary_window: { used_percent: 15, limit_window_seconds: 604800, reset_at: 1789586739 } } }) };
  assert.match(render(h(QuotaCard, { account: codex, now })), /Weekly/);

  const signedOut = { provider: "codex", email: null, plan: null, windows: [], error: "not signed in — run `codex login`", fetchedAt: new Date(now).toISOString() };
  const out = render(h(QuotaCard, { account: signedOut, now }));
  assert.match(out, /quota-card-error/);
  assert.match(out, /codex login/);
});

test("the machine view renders the RAM split and process table", async (t) => {
  const { MachineReportView } = await loadViews(t);
  const gb = 1 << 30;
  const report = {
    at: "2026-09-09T20:00:00.000Z",
    cpus: 12,
    load: [2.5, 2.1, 1.9],
    totalMem: 64 * gb,
    freeMem: 20 * gb,
    rudder: { cpu: 1.5, rss: 300 * (1 << 20), pid: 42 },
    processes: [
      { pid: 42, ppid: 1, kind: "rudder", cpu: 1.5, rss: 300 * (1 << 20), elapsed: "01:02:03", command: "/x/rudder-native" },
      { pid: 43, ppid: 42, kind: "claude", cpu: 40, rss: 2 * gb, elapsed: "00:10", command: "/x/claude --settings /tmp/s.json" },
    ],
    byKind: {
      rudder: { cpu: 1.5, rss: 300 * (1 << 20), count: 1 },
      claude: { cpu: 40, rss: 2 * gb, count: 1 },
      codex: { cpu: 0, rss: 0, count: 0 },
      opencode: { cpu: 0, rss: 0, count: 0 },
      other: { cpu: 0, rss: 0, count: 0 },
    },
  };
  const html = render(h(MachineReportView, { report }));
  assert.match(html, /Machine resources/);
  assert.match(html, /2\.00 GB/, "agent memory");
  assert.match(html, /44\.00 GB \/ 64\.00 GB/, "system RAM in use");
  assert.match(html, /load 2\.50 2\.10 1\.90 on 12 cores/);
  assert.match(html, /claude --settings/);
  assert.match(html, /ram-seg ram-agents/);
});
