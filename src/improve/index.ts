import path from "node:path";
import { promises as fs } from "node:fs";
import { loadConfig, loadProjects } from "../state.js";
import { findRepoRoot } from "../git.js";
import { ensureDir, readJson, shortenHome } from "../util.js";
import { collectSessions, computeMetrics } from "./collect.js";
import { dedupeAgainstLedger, mineFindings } from "./mine.js";
import { rankFindings, targetMetricFor } from "./rank.js";
import { buildContextPack, prepareWorktree, removeWorktree, runImprovementAgent } from "./propose.js";
import { gateSummary, runGates } from "./gate.js";
import { judgePanel, panelSummary } from "./judge.js";
import { shipProposal } from "./ship.js";
import { scheduleCommand } from "./schedule.js";
import {
  AGENT_RUN_FLAT_USD,
  SpendMeter,
  acquireCycleLock,
  appendJsonl,
  ledgerPath,
  loadWatermark,
  logsDir,
  metricsPath,
  readJsonl,
  releaseCycleLock,
  reportsDir,
  resolveImproveSettings,
  saveWatermark,
  titleKey,
  type Finding,
  type ImproveSettings,
  type LedgerEntry,
  type MetricsSnapshot,
} from "./state.js";

type ImproveArgs = {
  args: string[];
  flags: { dryRun?: boolean; json?: boolean };
};

export async function runImprove(parsed: ImproveArgs): Promise<void> {
  const sub = parsed.args[0] ?? "run";
  switch (sub) {
    case "run":
      await runCycle({
        dryRun: Boolean(parsed.flags.dryRun),
        budgetUsd: parseBudgetArg(parsed.args),
      });
      return;
    case "status":
      await printStatus();
      return;
    case "report":
      await printReport(parsed.args[1]);
      return;
    case "schedule":
      await scheduleCommand(parsed.args[1] ?? "status");
      return;
    default:
      throw new Error("Usage: rudder improve [run [--dry-run] [--budget-usd N] | status | report [date] | schedule install|uninstall|status]");
  }
}

function parseBudgetArg(args: string[]): number | undefined {
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i] ?? "";
    if (arg === "--budget-usd") {
      const value = Number.parseFloat(args[i + 1] ?? "");
      if (Number.isFinite(value) && value > 0) return value;
    }
    if (arg.startsWith("--budget-usd=")) {
      const value = Number.parseFloat(arg.slice("--budget-usd=".length));
      if (Number.isFinite(value) && value > 0) return value;
    }
  }
  return undefined;
}

async function runCycle(opts: { dryRun: boolean; budgetUsd?: number }): Promise<void> {
  const config = await loadConfig();
  const settings = resolveImproveSettings(config, await resolveRudderRepo());
  if (opts.budgetUsd) settings.budgetUsd = opts.budgetUsd;
  if (!settings.enabled) {
    console.log("improve: disabled (RUDDER_IMPROVE=0 or improve.enabled=false); nothing to do.");
    return;
  }
  if (!(await acquireCycleLock())) {
    console.log("improve: another cycle holds the lock; skipping.");
    return;
  }

  const cycleId = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  const meter = new SpendMeter(settings.budgetUsd);
  const lines: string[] = [];
  const note = (line: string) => {
    lines.push(line);
    console.log(`improve: ${line}`);
  };

  try {
    note(`cycle ${cycleId} starting (autonomy=${settings.autonomy}${opts.dryRun ? ", dry-run" : ""}, budget $${settings.budgetUsd})`);

    // ---- Collect ----------------------------------------------------------
    const watermark = await loadWatermark();
    const { sessions, nextWatermark } = await collectSessions(settings, watermark);
    const snapshot = computeMetrics(cycleId, sessions);
    await appendJsonl(metricsPath(), snapshot);
    note(`collected ${sessions.length} new session(s) across projects`);

    const ledger = await readJsonl<LedgerEntry>(ledgerPath());

    // ---- Mine + rank -------------------------------------------------------
    let selected: Finding[] = [];
    let banked: Finding[] = [];
    if (sessions.length > 0) {
      const findings = dedupeAgainstLedger(
        await mineFindings({
          cycleId,
          sessions,
          snapshot,
          ledger,
          model: settings.minerModel,
          advisorModel: settings.advisorModel,
          meter,
        }),
        ledger,
      );
      const ranked = rankFindings(findings, settings.maxFindings);
      selected = ranked.selected;
      banked = ranked.banked;
      note(`mined ${findings.length} finding(s): ${selected.length} selected, ${banked.length} banked`);
    } else {
      note("no new sessions; skipping mining");
    }
    for (const finding of banked) {
      await recordLedger(cycleId, finding, "banked", `score=${finding.score} frequency=${finding.frequency}`);
    }

    // ---- Observe / dry-run stop early --------------------------------------
    if (opts.dryRun || settings.autonomy === "observe") {
      for (const finding of selected) {
        await recordLedger(cycleId, finding, "reported", `score=${finding.score} frequency=${finding.frequency}`);
        note(`would propose: [${finding.class}] ${finding.title}`);
      }
      await finishCycle(cycleId, settings, snapshot, lines, meter);
      await saveWatermark(nextWatermark);
      return;
    }

    // ---- Propose → gate → judge → ship, sequentially ------------------------
    for (const finding of selected) {
      if (!meter.canAfford(AGENT_RUN_FLAT_USD + 0.5)) {
        await recordLedger(cycleId, finding, "banked", `budget exhausted at $${meter.spentUsd().toFixed(2)}; frequency=${finding.frequency}`);
        note(`budget exhausted; banked: ${finding.title}`);
        continue;
      }
      note(`proposing: [${finding.class}] ${finding.title}`);
      const history = ledger.filter((entry) => entry.titleKey === titleKey(finding.title));
      let worktreeInfo: { worktree: string; branch: string; baseRef: string };
      try {
        worktreeInfo = await prepareWorktree(settings, finding.id);
      } catch (error) {
        await recordLedger(cycleId, finding, "agent-failed", `worktree setup failed: ${String(error).slice(0, 300)}`);
        note(`worktree setup failed: ${String(error)}`);
        continue;
      }
      const contextPack = buildContextPack({ finding, history, snapshot });

      try {
        meter.addFlat(AGENT_RUN_FLAT_USD);
        let { proposal } = await runImprovementAgent({ settings, finding, ...worktreeInfo, contextPack });
        if (!proposal) {
          await recordLedger(cycleId, finding, "agent-failed", `agent produced no diff; frequency=${finding.frequency}`);
          note("agent produced no diff (finding may be non-actionable); see agent log");
          await removeWorktree(settings, worktreeInfo.worktree, worktreeInfo.branch);
          continue;
        }

        let gate = await runGates(proposal);
        if (!gate.passed) {
          note(`gates failed (${gateSummary(gate)}); retrying once with failure context`);
          meter.addFlat(AGENT_RUN_FLAT_USD);
          const failure = gate.steps.find((step) => step.code !== 0);
          const retry = await runImprovementAgent({
            settings,
            finding,
            ...worktreeInfo,
            contextPack,
            extraNote: `Your previous attempt failed the deterministic gate "${failure?.name}". Failure output tail:\n${failure?.failureTail ?? ""}\nFix the failure while keeping the improvement; leave the worktree committed and clean.`,
          });
          proposal = retry.proposal ?? proposal;
          gate = await runGates(proposal);
        }
        if (!gate.passed) {
          await recordLedger(cycleId, finding, "gated-out", `${gateSummary(gate)}; frequency=${finding.frequency}`);
          note(`gated out: ${gateSummary(gate)}`);
          await removeWorktree(settings, worktreeInfo.worktree, worktreeInfo.branch);
          continue;
        }
        note(`gates passed (${gateSummary(gate)})`);

        const panel = await judgePanel({
          proposal,
          gate,
          model: settings.judgeModel,
          advisorModel: settings.advisorModel,
          meter,
        });
        note(`judge panel: ${panelSummary(panel)}`);
        if (!panel.ship) {
          const reason = panel.votes.map((v) => `${v.lens}: ${v.notes}`).join(" | ").slice(0, 500);
          await recordLedger(cycleId, finding, "judge-rejected", `${reason}; frequency=${finding.frequency}`);
          await removeWorktree(settings, worktreeInfo.worktree, worktreeInfo.branch);
          continue;
        }

        const shipped = await shipProposal({ settings, proposal });
        if (shipped.status === "shipped") {
          await recordLedger(cycleId, finding, "shipped", proposal.resultNote?.summary ?? proposal.diffStat, {
            version: shipped.version,
            targetMetric: targetMetricFor(finding.class),
            metricAtShip: metricValue(snapshot, targetMetricFor(finding.class)),
          });
          note(`SHIPPED ${shipped.version}: ${finding.title}`);
        } else if (shipped.status === "branch-pushed") {
          await recordLedger(cycleId, finding, "branch-pushed", proposal.resultNote?.summary ?? proposal.diffStat, {
            branch: shipped.branch,
          });
          note(`pushed branch ${shipped.branch} for review`);
        } else {
          await recordLedger(cycleId, finding, "push-conflict", `${shipped.detail}; frequency=${finding.frequency}`);
          note(`push conflict: ${shipped.detail}`);
        }
        await removeWorktree(settings, worktreeInfo.worktree, worktreeInfo.branch);
      } catch (error) {
        await recordLedger(cycleId, finding, "agent-failed", `${String(error).slice(0, 300)}; frequency=${finding.frequency}`);
        note(`proposal errored: ${String(error)}`);
        await removeWorktree(settings, worktreeInfo.worktree, worktreeInfo.branch);
      }
    }

    await checkOutcomes(cycleId);
    await finishCycle(cycleId, settings, snapshot, lines, meter);
    await saveWatermark(nextWatermark);
  } finally {
    await releaseCycleLock();
  }
}

async function recordLedger(
  cycleId: string,
  finding: Finding,
  status: LedgerEntry["status"],
  detail: string,
  extra: Partial<LedgerEntry> = {},
): Promise<void> {
  await appendJsonl(ledgerPath(), {
    ts: new Date().toISOString(),
    cycleId,
    findingId: finding.id,
    titleKey: titleKey(finding.title),
    title: finding.title,
    class: finding.class,
    status,
    detail: detail.slice(0, 800),
    ...extra,
  } satisfies LedgerEntry);
}

// ---------------------------------------------------------------------------
// Outcome verification (§8 of the design): a shipped change only counts once
// the targeted metric moved. Compare mean metric across snapshots before vs
// after ship time, once at least a week and 3 post-ship snapshots have passed.
// ---------------------------------------------------------------------------

const OUTCOME_MIN_AGE_MS = 7 * 24 * 60 * 60 * 1000;

async function checkOutcomes(cycleId: string): Promise<void> {
  const ledger = await readJsonl<LedgerEntry>(ledgerPath());
  const metrics = await readJsonl<MetricsSnapshot>(metricsPath());
  const judged = new Set(ledger.filter((e) => e.status === "outcome").map((e) => e.findingId));
  for (const entry of ledger) {
    if (entry.status !== "shipped" || judged.has(entry.findingId) || !entry.targetMetric) continue;
    if (Date.now() - Date.parse(entry.ts) < OUTCOME_MIN_AGE_MS) continue;
    const before = metrics.filter((m) => m.ts < entry.ts && m.runsSeen > 0).slice(-6);
    const after = metrics.filter((m) => m.ts > entry.ts && m.runsSeen > 0);
    if (before.length < 3 || after.length < 3) continue;
    const mean = (rows: MetricsSnapshot[]) =>
      rows.reduce((sum, row) => sum + metricValue(row, entry.targetMetric as string), 0) / rows.length;
    const beforeMean = mean(before);
    const afterMean = mean(after);
    const delta = beforeMean === 0 ? 0 : (afterMean - beforeMean) / beforeMean;
    const outcome = delta <= -0.1 ? "confirmed" : delta >= 0.1 ? "regressed" : "no-effect";
    await appendJsonl(ledgerPath(), {
      ts: new Date().toISOString(),
      cycleId,
      findingId: entry.findingId,
      titleKey: entry.titleKey,
      title: entry.title,
      class: entry.class,
      status: "outcome",
      detail: `${entry.targetMetric}: ${beforeMean.toFixed(4)} -> ${afterMean.toFixed(4)} (${(delta * 100).toFixed(1)}%) for ${entry.version ?? "?"}`,
      outcome,
    } satisfies LedgerEntry);
  }
}

function metricValue(snapshot: MetricsSnapshot, key: string): number {
  const value = (snapshot as unknown as Record<string, unknown>)[key];
  return typeof value === "number" ? value : 0;
}

// ---------------------------------------------------------------------------
// Reporting + status
// ---------------------------------------------------------------------------

async function finishCycle(
  cycleId: string,
  settings: ImproveSettings,
  snapshot: MetricsSnapshot,
  lines: string[],
  meter: SpendMeter,
): Promise<void> {
  await ensureDir(reportsDir());
  const reportPath = path.join(reportsDir(), `${cycleId}.md`);
  const body = [
    `# Improvement cycle ${cycleId}`,
    "",
    `- autonomy: ${settings.autonomy}`,
    `- estimated spend: $${meter.spentUsd().toFixed(2)} of $${settings.budgetUsd}`,
    `- metrics: ${JSON.stringify(snapshot)}`,
    "",
    "## Log",
    ...lines.map((line) => `- ${line}`),
    "",
    `Agent + gate logs: ${shortenHome(logsDir())}`,
  ].join("\n");
  await fs.writeFile(reportPath, `${body}\n`, "utf8");
  console.log(`improve: cycle report ${shortenHome(reportPath)} (est. spend $${meter.spentUsd().toFixed(2)})`);
}

async function printStatus(): Promise<void> {
  const metrics = await readJsonl<MetricsSnapshot>(metricsPath());
  const ledger = await readJsonl<LedgerEntry>(ledgerPath());
  const latest = metrics.at(-1);
  console.log(`cycles recorded: ${metrics.length}`);
  if (latest) {
    console.log(`last cycle ${latest.cycleId}: ${latest.runsSeen} runs, failedRate ${latest.failedRate}, steerRate ${latest.steerRate}, mergeConflictRate ${latest.mergeConflictRate}`);
  }
  const shipped = ledger.filter((entry) => entry.status === "shipped");
  const open = ledger.filter((entry) => entry.status === "branch-pushed");
  console.log(`shipped: ${shipped.length}, branches awaiting review: ${open.length}, ledger entries: ${ledger.length}`);
  for (const entry of ledger.slice(-8)) {
    console.log(`  ${entry.ts} [${entry.status}] ${entry.title}${entry.version ? ` (${entry.version})` : ""}`);
  }
}

async function printReport(date?: string): Promise<void> {
  let file: string | undefined;
  try {
    const entries = (await fs.readdir(reportsDir())).filter((name) => name.endsWith(".md")).sort();
    file = date ? entries.find((name) => name.startsWith(date)) : entries.at(-1);
  } catch {
    file = undefined;
  }
  if (!file) {
    console.log("improve: no cycle reports yet. Run `rudder improve run`.");
    return;
  }
  console.log(await fs.readFile(path.join(reportsDir(), file), "utf8"));
}

/**
 * Find the rudder checkout to improve: an explicit config path wins; else the
 * current repo when it is a rudder checkout; else a registered project whose
 * package.json says @viraatdas/rudder.
 */
async function resolveRudderRepo(): Promise<string> {
  const isRudder = async (dir: string) =>
    (await readJson<{ name?: string }>(path.join(dir, "package.json")))?.name === "@viraatdas/rudder";
  const root = findRepoRoot(process.cwd());
  if (root && (await isRudder(root))) return root;
  for (const project of await loadProjects()) {
    if (project.repoRoot && (await isRudder(project.repoRoot))) return project.repoRoot;
  }
  return process.cwd();
}
