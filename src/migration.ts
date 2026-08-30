import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import type { JsonValue, RunRecord } from "./types.js";
import { adoptLegacyWorkspaceKey } from "./state.js";
import { pathExists, readJson, shortHash, slugPrefix } from "./util.js";

export type MigrationDecision = "migrate" | "migrate-fresh" | "stay" | "stop";

export type MigrationCandidate = {
  runId: string;
  task: string;
  taskSummary?: string;
  backend: string;
  status: string;
  workspacePath: string;
  workspaceBranch?: string;
  sessionId?: string;
  sessionJsonlPath?: string;
  reason: "resumable" | "no-session" | "no-jsonl" | "unsupported-backend" | "missing-workspace" | "not-running";
  decision: MigrationDecision;
};

export type MigrationPlan = {
  candidates: MigrationCandidate[];
  migrated: MigrationCandidate[];
  stayedLocal: MigrationCandidate[];
};

export type MigrationManifestEntry = {
  runId: string;
  task: string;
  taskSummary?: string;
  backend: string;
  model?: string;
  sessionId: string;
  /** Path of the workspace on the local machine. */
  localWorkspacePath: string;
  /** Relative path (under repo root / snapshot stage) the workspace maps to on cloud. */
  cloudWorkspaceRelativePath: string;
  /** Relative path inside the snapshot tar where the session jsonl is staged. */
  sessionJsonlSnapshotPath: string;
  workspaceBranch?: string;
  createdAt?: string;
  /**
   * Prompt-engineered handoff for fresh restarts (when no resumable session
   * exists). Includes the original task and recent user steering so the new
   * agent has context, not just the bare task.
   */
  freshPrompt?: string;
};

/**
 * Build a compact "you're picking up from a paused session" prompt for a
 * migrated agent whose conversation can't be replayed verbatim.
 */
export function buildFreshHandoffPrompt(candidate: MigrationCandidate, recentTurns: Array<{ prompt: string; source: string }>): string {
  const lines: string[] = [];
  lines.push("Resuming a paused agent session. Carry on the work below.");
  lines.push("");
  lines.push(`Original task: ${candidate.task || candidate.taskSummary || candidate.runId}`);
  const userTurns = recentTurns.filter((t) => t.source === "user").slice(-3);
  if (userTurns.length > 1) {
    lines.push("");
    lines.push("Recent user steering (most recent last):");
    for (const turn of userTurns) {
      const compact = turn.prompt.replace(/\s+/g, " ").trim().slice(0, 240);
      if (compact) {
        lines.push(`- ${compact}`);
      }
    }
  }
  lines.push("");
  lines.push("The earlier conversation transcript is not available. Re-establish context from the repo state and continue.");
  return lines.join("\n");
}

export type MigrationSnapshotManifest = {
  version: 1;
  createdAt: string;
  agents: MigrationManifestEntry[];
};

const RESUMABLE_BACKENDS = new Set(["claude"]);

/**
 * Discover candidate local agents that could be migrated to the cloud.
 *
 * "Running" is interpreted loosely: any run record whose workspace still exists
 * on disk and whose backend is resumable. We can't tell whether the PTY is
 * literally alive (the dashboard quits before this runs), but the Claude
 * session jsonl is persisted, so anything with a session id and an existing
 * jsonl can be resumed in the cloud via `claude --resume <id>`.
 */
export async function findMigrationCandidates(repoRoot: string): Promise<MigrationCandidate[]> {
  const runsDir = path.join(repoRoot, ".rudder", "runs");
  const entries = await fsp.readdir(runsDir, { withFileTypes: true }).catch((err: NodeJS.ErrnoException) => {
    // A missing runs dir means "no candidates"; surface real access failures
    // (EACCES/EIO/ENOTDIR) instead of masking them as an empty result.
    if (err && err.code === "ENOENT") {
      return [];
    }
    throw err;
  });
  const out: MigrationCandidate[] = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) {
      continue;
    }
    const runJsonPath = path.join(runsDir, entry.name, "run.json");
    const record = await readJson<RunRecord>(runJsonPath);
    if (!record || record.id !== entry.name) {
      continue;
    }
    // Read run.json directly (no loadRunRecord side effects), so promote the
    // pre-rename `worktree` key here too or every legacy record classifies as
    // "not isolated" and silently never migrates.
    adoptLegacyWorkspaceKey(record);
    const candidate = await classify(record);
    if (candidate) {
      out.push(candidate);
    }
  }
  return out;
}

async function classify(record: RunRecord): Promise<MigrationCandidate | null> {
  const status = record.status;
  // Only consider agents the user might care about migrating: anything that is
  // not already terminal. We include "running"/"steering"/"verifying"/"created".
  if (
    status === "completed"
    || status === "merged"
    || status === "cancelled"
    || status === "paused"
    || status === "failed"
    // Already living in the cloud — re-migrating would clone the session.
    || status === "migrated"
  ) {
    return null;
  }
  const workspacePath = record.workspace?.path;
  // Main/orchestrator/cloud-command records point at the repo root even though
  // they are not isolated workers. Only migrate real workspaces.
  if (!record.workspace?.enabled || !workspacePath) {
    return null;
  }
  const workspaceExists = await pathExists(workspacePath);
  const base: Omit<MigrationCandidate, "reason" | "decision"> = {
    runId: record.id,
    task: record.task,
    taskSummary: record.taskSummary,
    backend: record.backend,
    status,
    workspacePath,
    workspaceBranch: record.workspace?.branch,
    sessionId: record.session?.nativeSessionId,
  };
  if (!workspaceExists) {
    return { ...base, reason: "missing-workspace", decision: "stay" };
  }
  if (!RESUMABLE_BACKENDS.has(record.backend)) {
    // Non-Claude backends can't resume a conversation, but we can still move
    // them to the cloud by restarting fresh from the original task.
    return { ...base, reason: "unsupported-backend", decision: "migrate-fresh" };
  }
  const sessionId = record.session?.nativeSessionId;
  if (!sessionId) {
    return { ...base, reason: "no-session", decision: "migrate-fresh" };
  }
  const jsonl = claudeSessionJsonlPath(workspacePath, sessionId);
  if (!(await pathExists(jsonl))) {
    return { ...base, reason: "no-jsonl", sessionJsonlPath: jsonl, decision: "migrate-fresh" };
  }
  return { ...base, sessionJsonlPath: jsonl, reason: "resumable", decision: "migrate" };
}

/**
 * Encode an absolute filesystem path the way Claude Code does for its session
 * jsonl directory layout: every non-alphanumeric, non-hyphen character becomes
 * a single hyphen, including the leading slash.
 */
export function encodeClaudeProjectsCwd(absolutePath: string): string {
  return absolutePath.replace(/[^A-Za-z0-9-]/g, "-");
}

export function claudeSessionJsonlPath(cwd: string, sessionId: string): string {
  return path.join(
    os.homedir(),
    ".claude",
    "projects",
    encodeClaudeProjectsCwd(path.resolve(cwd)),
    `${sessionId}.jsonl`,
  );
}

/**
 * Cloud-side absolute workspace path that the supervisor will stage a migrated
 * agent's workspace at. Mirrors the layout in supervisor.mjs.
 */
export function cloudWorkspacePath(repoName: string): string {
  return path.posix.join("/workspace", repoName);
}

export function cloudWorkspaceAbsolutePath(repoName: string, runId: string, task?: string): string {
  // Mirrors src/state.ts workspacePath() but rooted at the cloud workspace.
  return path.posix.join("/workspace", ".rudder-workspaces", repoName, cloudWorkspaceDirName(runId, task));
}

function cloudWorkspaceDirName(runId: string, task?: string): string {
  const slug = slugPrefix(task ?? runId, "task");
  return `${slug}-${shortHash(runId).slice(0, 8)}`;
}

export function formatCandidateReason(candidate: MigrationCandidate): string {
  switch (candidate.reason) {
    case "resumable":
      return "resumable";
    case "no-session":
      return "no Claude session id";
    case "no-jsonl":
      return "Claude session jsonl missing locally";
    case "unsupported-backend":
      return `${candidate.backend} not resumable`;
    case "missing-workspace":
      return "workspace gone";
    case "not-running":
      return "not running";
    default:
      return "skipped";
  }
}

export function applyDefaultDecisions(candidates: MigrationCandidate[]): MigrationPlan {
  const migrated = candidates.filter((c) => c.decision === "migrate" || c.decision === "migrate-fresh");
  const stayedLocal = candidates.filter((c) => c.decision !== "migrate" && c.decision !== "migrate-fresh");
  return { candidates, migrated, stayedLocal };
}

export function migrationSummary(plan: MigrationPlan): string {
  const lines: string[] = [];
  lines.push(`${plan.migrated.length} agent${plan.migrated.length === 1 ? "" : "s"} will move to cloud, ${plan.stayedLocal.length} will stay local.`);
  for (const c of plan.migrated) {
    const short = (c.taskSummary || c.task || c.runId).slice(0, 72);
    const tag = c.decision === "migrate-fresh" ? "move*" : "move ";
    lines.push(`  ${tag} ${c.runId}  ${short}`);
  }
  for (const c of plan.stayedLocal) {
    const short = (c.taskSummary || c.task || c.runId).slice(0, 60);
    lines.push(`  stay  ${c.runId}  ${short}  (${formatCandidateReason(c)})`);
  }
  if (plan.migrated.some((c) => c.decision === "migrate-fresh")) {
    lines.push("  (* fresh restart in cloud; conversation not preserved)");
  }
  return lines.join("\n");
}

export function summaryAsJson(plan: MigrationPlan): JsonValue {
  return {
    moved: plan.migrated.length,
    stayedLocal: plan.stayedLocal.length,
    agents: plan.candidates.map((c) => ({
      runId: c.runId,
      backend: c.backend,
      decision: c.decision,
      reason: c.reason,
      task: c.taskSummary || c.task,
    })),
  };
}
