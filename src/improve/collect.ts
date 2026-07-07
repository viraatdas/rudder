import path from "node:path";
import { promises as fs } from "node:fs";
import { loadProjects, runRecordPath, runsDir, verifierPath } from "../state.js";
import { readJson } from "../util.js";
import { redactTaskSummarySecrets } from "../task-summary.js";
import type { RunRecord, VerificationResult } from "../types.js";
import type { ImproveSettings, MetricsSnapshot, SessionRecord, Watermark } from "./state.js";

const MAX_SESSIONS_PER_PROJECT = 200;
const MAX_ERROR_EXCERPTS = 3;
const ERROR_EXCERPT_CHARS = 300;
const EVENTS_TAIL_BYTES = 64 * 1024;

/**
 * Walk registered projects and build redacted session records for every run
 * newer than the watermark. Pure telemetry read; never writes into a project.
 */
export async function collectSessions(
  settings: ImproveSettings,
  watermark: Watermark,
): Promise<{ sessions: SessionRecord[]; nextWatermark: Watermark }> {
  const projects = await loadProjects();
  const sessions: SessionRecord[] = [];
  const nextWatermark: Watermark = { version: 1, projects: { ...watermark.projects } };

  for (const project of projects) {
    if (!project.repoRoot) continue;
    if (settings.excludeProjects.some((pattern) => matchesGlobish(project.repoRoot, pattern))) {
      continue;
    }
    let exists = false;
    try {
      exists = (await fs.stat(project.repoRoot)).isDirectory();
    } catch {
      exists = false;
    }
    if (!exists) continue;

    const since = watermark.projects[project.repoRoot] ?? "";
    const runs = await readRunsRaw(project.repoRoot);
    const fresh = runs
      .filter((run) => (run.updatedAt ?? "") > since)
      // Skip runs still in flight; their story is not over yet.
      .filter((run) => !["created", "running", "steering", "verifying"].includes(run.status))
      .sort((a, b) => (a.updatedAt < b.updatedAt ? -1 : 1))
      .slice(-MAX_SESSIONS_PER_PROJECT);

    for (const run of fresh) {
      sessions.push(await sessionFromRun(project.name ?? project.slug ?? project.repoRoot, run));
    }
    const newest = fresh.at(-1)?.updatedAt;
    if (newest && newest > (nextWatermark.projects[project.repoRoot] ?? "")) {
      nextWatermark.projects[project.repoRoot] = newest;
    }
  }
  return { sessions, nextWatermark };
}

/**
 * Read run records WITHOUT the state.ts load path: loadRunRecord fires the
 * background LLM task summarizer per record, which both costs model calls and
 * rewrites run.json (bumping updatedAt past the watermark). Telemetry
 * collection must be a pure read.
 */
async function readRunsRaw(repoRoot: string): Promise<RunRecord[]> {
  let entries: string[] = [];
  try {
    const dirents = await fs.readdir(runsDir(repoRoot), { withFileTypes: true });
    entries = dirents.filter((entry) => entry.isDirectory()).map((entry) => entry.name);
  } catch {
    return [];
  }
  const runs = await Promise.all(entries.map((id) => readJson<RunRecord>(runRecordPath(repoRoot, id))));
  return runs.filter((run): run is RunRecord => Boolean(run?.id && run?.status && run?.updatedAt));
}

/**
 * Whether a run experienced an integration conflict at ANY point, not just one
 * still unresolved at collect time. `status`/`merge.status` are LIVE state:
 * once a resolver or re-merge succeeds they are rewritten to merged, which is
 * why counting them alone reported mergeConflictRate 0 while resolver-labeled
 * sessions were everywhere. Reads, in order of reliability: the durable
 * `hadMergeConflict` marker, the live conflict state, `resolverFor` (the
 * daemon's resolver runs), and finally the native resolver task labels, which
 * are the only trace on records written before `hadMergeConflict` existed
 * (formats set in native/src/main.rs start_conflict_resolution_agent).
 */
export function runHadMergeConflict(
  run: Pick<RunRecord, "status" | "merge" | "sync" | "hadMergeConflict" | "resolverFor" | "task" | "taskSummary">,
): boolean {
  if (run.hadMergeConflict) return true;
  if (run.status === "merge-conflict") return true;
  if (run.merge?.status === "conflict" || run.sync?.status === "conflict") return true;
  if (run.resolverFor) return true;
  if (/^(?:merge|rebase) conflicts → /.test(run.taskSummary ?? "")) return true;
  return /^Resolve (?:merge|rebase) conflicts(?::|$)/.test(run.task ?? "");
}

// Timestamps outside [Rudder's existence, now + a day of clock skew] are
// sentinel or corrupt stamps, not real run times.
const EARLIEST_PLAUSIBLE_RUN_MS = Date.parse("2024-01-01T00:00:00Z");
const MAX_CLOCK_SKEW_MS = 24 * 60 * 60 * 1000;

/**
 * Parse a run timestamp into epoch ms, rejecting implausible values. Run
 * records carry two formats: ISO strings (the TS writers, util.nowIso) and
 * epoch-millisecond strings (the native TUI's now_stamp in
 * native/src/gitio.rs), which Date.parse cannot read. Anything implausible is
 * rejected rather than letting Date.parse guess: native test fixtures once
 * leaked run.json records with the sentinel createdAt "1" into a real
 * .rudder/runs dir, Date.parse("1") is 2001-01-01, and the resulting 25-year
 * durations dominated medianDurationMs for the whole cycle.
 */
export function parseRunTimestampMs(value: string | undefined): number | undefined {
  if (!value) return undefined;
  const ms = /^\d+$/.test(value) ? Number(value) : Date.parse(value);
  if (!Number.isFinite(ms)) return undefined;
  if (ms < EARLIEST_PLAUSIBLE_RUN_MS || ms > Date.now() + MAX_CLOCK_SKEW_MS) return undefined;
  return ms;
}

async function sessionFromRun(project: string, run: RunRecord): Promise<SessionRecord> {
  const steerCount = (run.turns ?? []).filter(
    (turn) => turn.source === "steerer" || turn.source === "regoal",
  ).length;
  let durationMs: number | undefined;
  const startMs = parseRunTimestampMs(run.process?.startedAt ?? run.createdAt);
  const endMs = parseRunTimestampMs(run.process?.endedAt ?? run.updatedAt);
  if (startMs !== undefined && endMs !== undefined && endMs >= startMs) {
    durationMs = endMs - startMs;
  }

  let verifierSatisfied: boolean | undefined;
  let verifierMissing: string[] | undefined;
  const verification =
    run.verification ??
    (await readJson<VerificationResult>(verifierPath(run.repoRoot, run.id))) ??
    undefined;
  if (verification && typeof verification.satisfied === "boolean") {
    verifierSatisfied = verification.satisfied;
    verifierMissing = (verification.missing ?? []).slice(0, 5).map(redactAndTrim);
  }

  const hadMergeConflict = runHadMergeConflict(run);
  const interesting =
    run.status === "failed" ||
    run.status === "cancelled" ||
    hadMergeConflict ||
    verifierSatisfied === false;

  return {
    project,
    repoRoot: run.repoRoot,
    runId: run.id,
    task: redactAndTrim(run.taskSummary ?? run.task ?? "", 160),
    backend: run.backend,
    model: run.model,
    effort: run.effort,
    status: run.status,
    createdAt: run.createdAt,
    updatedAt: run.updatedAt,
    durationMs,
    steerCount,
    autoSteerCount: run.autoSteer?.count ?? 0,
    tokensIn: run.tokens?.input ?? 0,
    tokensOut: run.tokens?.output ?? 0,
    verifierSatisfied,
    verifierMissing,
    mergeStatus: run.merge?.status,
    hadMergeConflict,
    errorExcerpts: interesting ? await readErrorExcerpts(run) : [],
  };
}

/**
 * Read the tail of a run's events.ndjson and pull out error-shaped events.
 * Redaction happens here, before anything reaches a model.
 */
async function readErrorExcerpts(run: RunRecord): Promise<string[]> {
  const eventsFile = path.join(run.repoRoot, ".rudder", "runs", run.id, "events.ndjson");
  let raw = "";
  try {
    const handle = await fs.open(eventsFile, "r");
    try {
      const stat = await handle.stat();
      const start = Math.max(0, stat.size - EVENTS_TAIL_BYTES);
      const buffer = Buffer.alloc(stat.size - start);
      await handle.read(buffer, 0, buffer.length, start);
      raw = buffer.toString("utf8");
    } finally {
      await handle.close();
    }
  } catch {
    return [];
  }

  const excerpts: string[] = [];
  for (const line of raw.split("\n").reverse()) {
    if (excerpts.length >= MAX_ERROR_EXCERPTS) break;
    const trimmed = line.trim();
    if (!trimmed) continue;
    let event: { type?: string; text?: string; message?: string } | null = null;
    try {
      event = JSON.parse(trimmed) as { type?: string; text?: string; message?: string };
    } catch {
      continue;
    }
    const type = event?.type ?? "";
    if (!/error|fail/i.test(type)) continue;
    const text = event?.text ?? event?.message ?? "";
    if (!text.trim()) continue;
    excerpts.push(`[${type}] ${redactAndTrim(text, ERROR_EXCERPT_CHARS)}`);
  }
  return excerpts;
}

export function computeMetrics(cycleId: string, sessions: SessionRecord[]): MetricsSnapshot {
  const n = sessions.length;
  const rate = (count: number) => (n === 0 ? 0 : round4(count / n));
  const failed = sessions.filter((s) => s.status === "failed").length;
  const cancelled = sessions.filter((s) => s.status === "cancelled").length;
  const conflicts = sessions.filter(
    (s) => s.hadMergeConflict || s.status === "merge-conflict" || s.mergeStatus === "conflict",
  ).length;
  const verifierMisses = sessions.filter((s) => s.verifierSatisfied === false).length;
  const steered = sessions.filter((s) => s.steerCount > 0).length;
  const durations = sessions
    .map((s) => s.durationMs)
    .filter((d): d is number => typeof d === "number")
    .sort((a, b) => a - b);
  return {
    ts: new Date().toISOString(),
    cycleId,
    runsSeen: n,
    failedRate: rate(failed),
    cancelledRate: rate(cancelled),
    steerRate: rate(steered),
    mergeConflictRate: rate(conflicts),
    verifierMissRate: rate(verifierMisses),
    medianDurationMs: durations.length ? durations[Math.floor(durations.length / 2)] : 0,
    totalTokensIn: sessions.reduce((sum, s) => sum + s.tokensIn, 0),
    totalTokensOut: sessions.reduce((sum, s) => sum + s.tokensOut, 0),
  };
}

function redactAndTrim(value: string, maxChars = 240): string {
  const redacted = redactTaskSummarySecrets(value).replace(/\s+/g, " ").trim();
  return redacted.length > maxChars ? `${redacted.slice(0, maxChars - 1)}…` : redacted;
}

function round4(value: number): number {
  return Math.round(value * 10000) / 10000;
}

/** Minimal glob support: `*` wildcards only, matched against the repo path. */
function matchesGlobish(value: string, pattern: string): boolean {
  const regex = new RegExp(
    `^${pattern.split("*").map(escapeRegExp).join(".*")}$`,
  );
  return regex.test(value);
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
