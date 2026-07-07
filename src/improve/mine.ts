import { callAdvisedTextModel } from "./advisor.js";
import {
  titleKey,
  type Finding,
  type FindingClass,
  type LedgerEntry,
  type MetricsSnapshot,
  type SessionRecord,
  type SpendMeter,
} from "./state.js";

const MAX_SESSIONS_IN_PROMPT = 30;
const MAX_FINDINGS_FROM_MODEL = 8;

/**
 * A condensed surface map so the miner proposes real file paths instead of
 * hallucinated ones. Keep in sync with AGENTS.md section 2 when the layout
 * moves.
 */
export const SURFACE_MAP = `Rudder surface map (repo-relative paths):
- src/run-manager.ts    run lifecycle: start, worker, merge/sync routing
- src/backends.ts       claude/codex/acpx adapters, spawn + event streaming
- src/brain.ts          worker spec/contract rendering, verifyRun
- src/planner.ts        headless DAG planner + plan block parsing
- src/scheduler.ts      headless DAG scheduler (daemon), merge serialization
- src/jj.ts             jj workspace create/merge/rebase/undo substrate
- src/state.ts          run records, config, projects registry persistence
- src/surfaces.ts       DECISIONS.md / completion notes / shared context
- src/board/daemon.ts   web board HTTP+SSE server
- src/cloud.ts          Rudder Cloud client
- native/src/main.rs    Rust TUI: App state machine, poll loop, scheduling
- native/src/tasks.rs   prompt construction, orchestrator system prompt
- native/src/launch.rs  agent launch/resume command building
- native/src/signals.rs completion signal hooks (Stop hook / notify)
- native/src/detect.rs  idle/permission output heuristics (fallback only)
- native/src/render.rs  TUI rendering`;

const MINER_SYSTEM = `You are the triage stage of Rudder's continual improvement loop.
Rudder is a terminal app that runs coding agents (Claude Code / Codex) in parallel jj worktrees with an orchestrator DAG.
You receive telemetry summaries of recent Rudder sessions. Your job is to identify friction caused by RUDDER ITSELF (its prompts, orchestration, scheduling, merge handling, UX, performance, crashes), NOT problems in the user's own projects or model quality.
Return findings as a single fenced json block: an array of objects with fields:
  class: one of "prompt" | "orchestration" | "ux" | "perf" | "crash" | "other"
  title: short imperative description of the defect (not the fix)
  detail: 2-4 sentences: what happens, why it is Rudder's fault, what a fix might touch
  evidence: array of {project, runId, excerpt} referencing the sessions given
  severity: 1-5 (5 = data loss / task failure, 1 = cosmetic)
  frequency: how many given sessions show it
  suspectedSurfaces: repo-relative file paths from the surface map
Only report findings supported by the evidence. Fewer, well-supported findings beat many speculative ones. If nothing is attributable to Rudder, return [].`;

export async function mineFindings(params: {
  cycleId: string;
  sessions: SessionRecord[];
  snapshot: MetricsSnapshot;
  ledger: LedgerEntry[];
  model: string;
  advisorModel: string;
  meter: SpendMeter;
}): Promise<Finding[]> {
  const worst = [...params.sessions].sort((a, b) => frictionScore(b) - frictionScore(a));
  const sample = worst.slice(0, MAX_SESSIONS_IN_PROMPT);
  if (sample.length === 0) return [];

  const user = [
    "Cycle metrics snapshot:",
    JSON.stringify(params.snapshot),
    "",
    SURFACE_MAP,
    "",
    `Sessions (worst-first, ${sample.length} of ${params.sessions.length} new):`,
    JSON.stringify(sample, null, 1),
  ].join("\n");

  if (!params.meter.canAfford(0.1)) return [];
  // Advisor pattern: the executor (minerModel) does the bulk generation and
  // consults the advisor for judgment; usage.iterations meters both sides.
  const output = await callAdvisedTextModel({
    executorModel: params.model,
    advisorModel: params.advisorModel,
    system: MINER_SYSTEM,
    user,
    maxTokens: 4096,
    timeoutMs: 240000,
    meter: params.meter,
  });

  const parsed = parseFindingsJson(output).slice(0, MAX_FINDINGS_FROM_MODEL);
  const findings = parsed.map((raw, index) => normalizeFinding(raw, params.cycleId, index));
  return dedupeAgainstLedger(findings, params.ledger);
}

export function frictionScore(session: SessionRecord): number {
  let score = 0;
  if (session.status === "failed") score += 5;
  if (session.status === "cancelled") score += 3;
  if (session.status === "merge-conflict" || session.mergeStatus === "conflict") score += 4;
  if (session.verifierSatisfied === false) score += 4;
  score += Math.min(3, session.steerCount);
  score += Math.min(2, session.autoSteerCount);
  score += session.errorExcerpts.length;
  return score;
}

/** Parse the model's findings array out of a fenced block or bare JSON. */
export function parseFindingsJson(output: string): Array<Record<string, unknown>> {
  const fenced = output.match(/```(?:json)?\s*([\s\S]*?)```/);
  const candidates = [fenced?.[1], output];
  for (const candidate of candidates) {
    if (!candidate) continue;
    const start = candidate.indexOf("[");
    const end = candidate.lastIndexOf("]");
    if (start < 0 || end <= start) continue;
    try {
      const value = JSON.parse(candidate.slice(start, end + 1));
      if (Array.isArray(value)) {
        return value.filter((item): item is Record<string, unknown> => typeof item === "object" && item !== null);
      }
    } catch {
      // try the next candidate
    }
  }
  return [];
}

const FINDING_CLASSES: FindingClass[] = ["prompt", "orchestration", "ux", "perf", "crash", "other"];

function normalizeFinding(raw: Record<string, unknown>, cycleId: string, index: number): Finding {
  const classValue = typeof raw.class === "string" && FINDING_CLASSES.includes(raw.class as FindingClass)
    ? (raw.class as FindingClass)
    : "other";
  const evidence = Array.isArray(raw.evidence)
    ? raw.evidence
        .filter((item): item is Record<string, unknown> => typeof item === "object" && item !== null)
        .slice(0, 5)
        .map((item) => ({
          project: String(item.project ?? ""),
          runId: String(item.runId ?? ""),
          excerpt: String(item.excerpt ?? "").slice(0, 400),
        }))
    : [];
  return {
    id: `f-${cycleId}-${index}`,
    class: classValue,
    title: String(raw.title ?? "untitled finding").slice(0, 160),
    detail: String(raw.detail ?? "").slice(0, 1200),
    evidence,
    severity: clampInt(raw.severity, 1, 5, 2),
    frequency: clampInt(raw.frequency, 1, 1000, 1),
    suspectedSurfaces: Array.isArray(raw.suspectedSurfaces)
      ? raw.suspectedSurfaces.map((s) => String(s)).slice(0, 6)
      : [],
  };
}

/**
 * Drop findings the ledger already knows: anything shipped or currently
 * banked under the same title key, and anything previously rejected unless
 * its frequency has at least doubled since the rejection was recorded.
 */
export function dedupeAgainstLedger(findings: Finding[], ledger: LedgerEntry[]): Finding[] {
  const byKey = new Map<string, LedgerEntry[]>();
  for (const entry of ledger) {
    const list = byKey.get(entry.titleKey) ?? [];
    list.push(entry);
    byKey.set(entry.titleKey, list);
  }
  return findings.filter((finding) => {
    const history = byKey.get(titleKey(finding.title)) ?? [];
    if (history.length === 0) return true;
    if (history.some((e) => e.status === "shipped" || e.status === "branch-pushed")) {
      return false;
    }
    const rejected = history.filter(
      (e) => e.status === "judge-rejected" || e.status === "gated-out" || e.status === "agent-failed",
    );
    if (rejected.length > 0) {
      const lastFrequency = extractFrequency(rejected.at(-1)?.detail ?? "");
      return finding.frequency >= Math.max(2, lastFrequency * 2);
    }
    return true;
  });
}

function extractFrequency(detail: string): number {
  const match = detail.match(/frequency=(\d+)/);
  return match ? Number.parseInt(match[1], 10) : 1;
}

function clampInt(value: unknown, min: number, max: number, fallback: number): number {
  const parsed = typeof value === "number" ? Math.round(value) : Number.parseInt(String(value), 10);
  if (!Number.isFinite(parsed)) return fallback;
  return Math.min(max, Math.max(min, parsed));
}
