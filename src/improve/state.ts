import path from "node:path";
import { promises as fs } from "node:fs";
import { spawn } from "node:child_process";
import { commandEnv, ensureDir, pathExists, readJson, rudderHome, writeJson } from "../util.js";
import type { RudderConfig } from "../types.js";

// ---------------------------------------------------------------------------
// The continual improvement loop's on-disk state lives under ~/.rudder/improve.
// Everything here is append-only JSONL or atomic JSON via writeJson, matching
// the repo persistence conventions. See docs/continual-improvement.md.
// ---------------------------------------------------------------------------

export function improveHome(): string {
  return path.join(rudderHome(), "improve");
}

export function watermarkPath(): string {
  return path.join(improveHome(), "watermark.json");
}

export function metricsPath(): string {
  return path.join(improveHome(), "metrics.jsonl");
}

export function ledgerPath(): string {
  return path.join(improveHome(), "ledger.jsonl");
}

export function reportsDir(): string {
  return path.join(improveHome(), "reports");
}

export function logsDir(): string {
  return path.join(improveHome(), "logs");
}

export function worktreesDir(): string {
  return path.join(improveHome(), "worktrees");
}

export function cycleLockDir(): string {
  return path.join(improveHome(), "cycle.lock");
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type ImproveAutonomy = "observe" | "propose" | "ship";

export type ImproveSettings = {
  enabled: boolean;
  autonomy: ImproveAutonomy;
  budgetUsd: number;
  maxFindings: number;
  /** The rudder checkout the loop improves. */
  repoPath: string;
  /** Remote slug guard: ship only when `origin` matches. */
  allowedRemote: string;
  excludeProjects: string[];
  minerModel: string;
  judgeModel: string;
  /**
   * Advisor tool model for mining/judging calls (Anthropic advisor pattern:
   * cheap executor + high-intelligence advisor). Empty string disables.
   */
  advisorModel: string;
  /** Model passed to the improvement agent CLI; empty = CLI default. */
  workerModel: string;
  agentTimeoutMs: number;
};

export const IMPROVE_DEFAULTS = {
  // The loop may prepare a complete, reviewed branch by default, but publishing
  // a globally installed CLI is an explicit trust decision. Users who want the
  // old unattended behavior can opt in with `improve.autonomy: "ship"`.
  autonomy: "propose" as ImproveAutonomy,
  budgetUsd: 5,
  maxFindings: 3,
  allowedRemote: "viraatdas/rudder",
  minerModel: "claude-sonnet-4-6",
  judgeModel: "claude-sonnet-4-6",
  advisorModel: "claude-fable-5",
  agentTimeoutMs: 25 * 60 * 1000,
};

export function resolveImproveSettings(config: RudderConfig, repoFallback: string): ImproveSettings {
  const raw = config.improve ?? {};
  const envDisabled = process.env.RUDDER_IMPROVE === "0";
  return {
    enabled: !envDisabled && raw.enabled !== false,
    autonomy: raw.autonomy ?? IMPROVE_DEFAULTS.autonomy,
    budgetUsd: raw.budgetUsd ?? IMPROVE_DEFAULTS.budgetUsd,
    maxFindings: raw.maxFindings ?? IMPROVE_DEFAULTS.maxFindings,
    repoPath: raw.repoPath ?? repoFallback,
    allowedRemote: raw.allowedRemote ?? IMPROVE_DEFAULTS.allowedRemote,
    excludeProjects: raw.excludeProjects ?? [],
    minerModel: raw.minerModel ?? IMPROVE_DEFAULTS.minerModel,
    judgeModel: raw.judgeModel ?? IMPROVE_DEFAULTS.judgeModel,
    advisorModel: raw.advisorModel ?? IMPROVE_DEFAULTS.advisorModel,
    workerModel: raw.workerModel ?? "",
    agentTimeoutMs: raw.agentTimeoutMs ?? IMPROVE_DEFAULTS.agentTimeoutMs,
  };
}

export type SessionRecord = {
  project: string;
  repoRoot: string;
  runId: string;
  task: string;
  backend: string;
  model?: string;
  effort?: string;
  status: string;
  createdAt: string;
  updatedAt: string;
  durationMs?: number;
  steerCount: number;
  autoSteerCount: number;
  tokensIn: number;
  tokensOut: number;
  verifierSatisfied?: boolean;
  verifierMissing?: string[];
  mergeStatus?: string;
  /** True when the run hit an integration conflict at ANY point, even one a
   * resolver already fixed by collect time (see collect.ts runHadMergeConflict). */
  hadMergeConflict?: boolean;
  errorExcerpts: string[];
};

export type MetricsSnapshot = {
  ts: string;
  cycleId: string;
  runsSeen: number;
  failedRate: number;
  cancelledRate: number;
  steerRate: number;
  mergeConflictRate: number;
  verifierMissRate: number;
  medianDurationMs: number;
  totalTokensIn: number;
  totalTokensOut: number;
};

export type FindingClass = "prompt" | "orchestration" | "ux" | "perf" | "crash" | "other";

export type Finding = {
  id: string;
  class: FindingClass;
  title: string;
  detail: string;
  evidence: Array<{ project: string; runId: string; excerpt: string }>;
  severity: number;
  frequency: number;
  suspectedSurfaces: string[];
  score?: number;
};

export type LedgerStatus =
  | "reported"
  | "banked"
  | "agent-failed"
  | "gated-out"
  | "judge-rejected"
  | "branch-pushed"
  | "push-conflict"
  | "shipped"
  | "outcome";

export type LedgerEntry = {
  ts: string;
  cycleId: string;
  findingId: string;
  titleKey: string;
  title: string;
  class: string;
  status: LedgerStatus;
  detail?: string;
  version?: string;
  branch?: string;
  targetMetric?: string;
  metricAtShip?: number;
  outcome?: "confirmed" | "no-effect" | "regressed";
};

export function titleKey(title: string): string {
  return title
    .toLowerCase()
    .replace(/[^a-z0-9 ]+/g, "")
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 8)
    .join("-");
}

// ---------------------------------------------------------------------------
// JSONL helpers
// ---------------------------------------------------------------------------

export async function appendJsonl(filePath: string, value: unknown): Promise<void> {
  await ensureDir(path.dirname(filePath));
  await fs.appendFile(filePath, `${JSON.stringify(value)}\n`, "utf8");
}

export async function readJsonl<T>(filePath: string): Promise<T[]> {
  try {
    const raw = await fs.readFile(filePath, "utf8");
    const out: T[] = [];
    for (const line of raw.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        out.push(JSON.parse(trimmed) as T);
      } catch {
        // skip torn/corrupt lines; the files are append-only diagnostics
      }
    }
    return out;
  } catch {
    return [];
  }
}

// ---------------------------------------------------------------------------
// Watermark: per-project newest run updatedAt already consumed.
// ---------------------------------------------------------------------------

/**
 * Values are epoch millis going forward; string entries (ISO or millis
 * strings) are legacy formats from before the watermark was normalized, read
 * through collect.ts watermarkValueMs.
 */
export type Watermark = { version: 1; projects: Record<string, string | number> };

export async function loadWatermark(): Promise<Watermark> {
  const existing = await readJson<Watermark>(watermarkPath());
  if (existing?.version === 1 && existing.projects) {
    return existing;
  }
  return { version: 1, projects: {} };
}

export async function saveWatermark(mark: Watermark): Promise<void> {
  await ensureDir(improveHome());
  await writeJson(watermarkPath(), mark);
}

// ---------------------------------------------------------------------------
// Single-instance cycle lock (mkdir lock, stale takeover), the same shape as
// .rudder/integrate.lock. Not reentrant.
// ---------------------------------------------------------------------------

const LOCK_STALE_MS = 6 * 60 * 60 * 1000;

export async function acquireCycleLock(): Promise<boolean> {
  await ensureDir(improveHome());
  const lock = cycleLockDir();
  try {
    await fs.mkdir(lock);
    await fs.writeFile(path.join(lock, "pid"), String(process.pid), "utf8");
    return true;
  } catch {
    try {
      const stat = await fs.stat(lock);
      if (Date.now() - stat.mtimeMs > LOCK_STALE_MS) {
        await fs.rm(lock, { recursive: true, force: true });
        await fs.mkdir(lock);
        await fs.writeFile(path.join(lock, "pid"), String(process.pid), "utf8");
        return true;
      }
    } catch {
      // fall through: someone else holds it
    }
    return false;
  }
}

export async function releaseCycleLock(): Promise<void> {
  await fs.rm(cycleLockDir(), { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// Spend meter: a coarse budget guardrail, not accounting. Model calls are
// estimated at chars/4 tokens against a small price table; agent runs are a
// flat estimate because the CLI's own auth (subscription) hides real cost.
// ---------------------------------------------------------------------------

const PRICE_PER_MTOK: Array<{ match: RegExp; inUsd: number; outUsd: number }> = [
  { match: /haiku/i, inUsd: 1, outUsd: 5 },
  { match: /sonnet/i, inUsd: 3, outUsd: 15 },
  { match: /opus|fable/i, inUsd: 5, outUsd: 25 },
];

export const AGENT_RUN_FLAT_USD = 0.75;

export class SpendMeter {
  private spent = 0;
  constructor(private readonly budgetUsd: number) {}

  addModelCall(model: string, inChars: number, outChars: number): void {
    this.addModelTokens(model, inChars / 4, outChars / 4);
  }

  /** Exact token accounting (used when the API reports usage.iterations). */
  addModelTokens(model: string, inputTokens: number, outputTokens: number): void {
    const price = PRICE_PER_MTOK.find((p) => p.match.test(model)) ?? PRICE_PER_MTOK[1];
    this.spent += (inputTokens / 1_000_000) * price.inUsd + (outputTokens / 1_000_000) * price.outUsd;
  }

  addFlat(usd: number): void {
    this.spent += usd;
  }

  spentUsd(): number {
    return this.spent;
  }

  remainingUsd(): number {
    return Math.max(0, this.budgetUsd - this.spent);
  }

  /** True when there is room for at least `usd` more estimated spend. */
  canAfford(usd: number): boolean {
    return this.spent + usd <= this.budgetUsd;
  }
}

// ---------------------------------------------------------------------------
// execStep: spawn with a hard timeout and captured output, used for gates and
// the improvement agent (util.runCommand has no timeout support).
// ---------------------------------------------------------------------------

export type StepResult = { code: number; timedOut: boolean; output: string };

export async function execStep(params: {
  command: string;
  args: string[];
  cwd: string;
  timeoutMs: number;
  stdin?: string;
  env?: NodeJS.ProcessEnv;
  logFile?: string;
}): Promise<StepResult> {
  return await new Promise<StepResult>((resolve) => {
    let settled = false;
    let timedOut = false;
    let output = "";
    const finish = (code: number) => {
      if (settled) return;
      settled = true;
      if (params.logFile) {
        void ensureDir(path.dirname(params.logFile)).then(() =>
          fs.appendFile(
            params.logFile as string,
            `\n===== ${params.command} ${params.args.join(" ")} (exit ${code}${timedOut ? ", timed out" : ""}) =====\n${output}`,
            "utf8",
          ),
        );
      }
      resolve({ code, timedOut, output });
    };
    let child;
    try {
      child = spawn(params.command, params.args, {
        cwd: params.cwd,
        env: commandEnv(params.env),
        stdio: [params.stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"],
      });
    } catch (error) {
      output = String(error);
      finish(127);
      return;
    }
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        child.kill("SIGTERM");
        setTimeout(() => {
          try {
            child.kill("SIGKILL");
          } catch {
            // already gone
          }
        }, 5000).unref();
      } catch {
        // already gone
      }
    }, params.timeoutMs);
    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      output += chunk;
    });
    child.stderr?.on("data", (chunk: string) => {
      output += chunk;
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      output += `\n${String(error)}`;
      finish(127);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      finish(timedOut ? 124 : (code ?? 1));
    });
    if (params.stdin !== undefined) {
      try {
        child.stdin?.write(params.stdin);
        child.stdin?.end();
      } catch {
        // child error handler will fire
      }
    }
  });
}

export function tail(text: string, maxChars: number): string {
  if (text.length <= maxChars) return text;
  return `…${text.slice(text.length - maxChars)}`;
}

export async function fileExists(filePath: string): Promise<boolean> {
  return await pathExists(filePath);
}
