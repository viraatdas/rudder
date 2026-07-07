import path from "node:path";
import { promises as fs } from "node:fs";
import { readJson, runCommand } from "../util.js";
import { SURFACE_MAP } from "./mine.js";
import {
  execStep,
  logsDir,
  tail,
  worktreesDir,
  type Finding,
  type ImproveSettings,
  type LedgerEntry,
  type MetricsSnapshot,
  type StepResult,
} from "./state.js";

export type Proposal = {
  finding: Finding;
  worktree: string;
  branch: string;
  baseRef: string;
  changedFiles: string[];
  diffStat: string;
  diffText: string;
  resultNote: {
    summary: string;
    changeClass: "prompt-only" | "logic";
    testsRun?: string;
    risks?: string;
  } | null;
  agentLogPath: string;
};

/**
 * Create an isolated git worktree of the rudder checkout for one finding.
 * Plain git worktrees are safe on the jj-colocated repo: jj does not track
 * them and the loop only ever uses git plumbing inside them.
 */
export async function prepareWorktree(
  settings: ImproveSettings,
  findingId: string,
): Promise<{ worktree: string; branch: string; baseRef: string }> {
  const repo = settings.repoPath;
  const pkg = await readJson<{ name?: string }>(path.join(repo, "package.json"));
  if (pkg?.name !== "@viraatdas/rudder") {
    throw new Error(`improve.repoPath ${repo} is not a rudder checkout (package name ${pkg?.name ?? "missing"})`);
  }

  await runCommand("git", ["fetch", "origin", "main"], { cwd: repo, allowFailure: true });
  const originMain = await runCommand("git", ["rev-parse", "--verify", "origin/main"], {
    cwd: repo,
    allowFailure: true,
  });
  const baseRef = originMain.code === 0 ? "origin/main" : "HEAD";

  const branch = `improve/${findingId}`;
  const worktree = path.join(worktreesDir(), findingId);
  await fs.rm(worktree, { recursive: true, force: true });
  await runCommand("git", ["worktree", "prune"], { cwd: repo, allowFailure: true });
  await runCommand("git", ["branch", "-D", branch], { cwd: repo, allowFailure: true });
  await runCommand("git", ["worktree", "add", "-b", branch, worktree, baseRef], { cwd: repo });
  return { worktree, branch, baseRef };
}

export async function removeWorktree(settings: ImproveSettings, worktree: string, branch: string): Promise<void> {
  await runCommand("git", ["worktree", "remove", "--force", worktree], {
    cwd: settings.repoPath,
    allowFailure: true,
  });
  await runCommand("git", ["branch", "-D", branch], { cwd: settings.repoPath, allowFailure: true });
}

/**
 * The context pack: everything an autonomous agent needs to fix this finding
 * well. It runs inside a full rudder worktree, so the pack points at the
 * canonical context (AGENTS.md, the design doc, real paths, prior attempts)
 * instead of pasting whole files.
 */
export function buildContextPack(params: {
  finding: Finding;
  history: LedgerEntry[];
  snapshot: MetricsSnapshot;
}): string {
  const { finding } = params;
  const evidence = finding.evidence
    .map((item, i) => `  ${i + 1}. [${item.project} run ${item.runId}] ${item.excerpt}`)
    .join("\n");
  const priorAttempts = params.history
    .filter((entry) => ["judge-rejected", "gated-out", "agent-failed", "push-conflict"].includes(entry.status))
    .slice(-3)
    .map((entry) => `  - ${entry.ts} ${entry.status}: ${entry.detail ?? "no detail"}`)
    .join("\n");

  return `You are an autonomous maintenance engineer working on Rudder itself, dispatched by Rudder's continual improvement loop (docs/continual-improvement.md). You are in an isolated git worktree of the rudder repo on branch-per-finding; nothing you do here touches the user's checkout.

FIRST, before changing anything: read AGENTS.md in the repo root end to end. It is the engineering reference and the source of truth for architecture, conventions, invariants, and gotchas. Respect it over your instincts.

FINDING (mined from real usage telemetry)
- id: ${finding.id}
- class: ${finding.class}
- severity: ${finding.severity}/5, seen in ${finding.frequency} recent session(s)
- title: ${finding.title}
- detail: ${finding.detail}

EVIDENCE (redacted excerpts from real sessions)
${evidence || "  (none beyond the metrics)"}

SUSPECTED SURFACES (verify before trusting; the miner can be wrong)
${finding.suspectedSurfaces.map((s) => `  - ${s}`).join("\n") || "  (none suggested)"}

${SURFACE_MAP}

CURRENT CYCLE METRICS
${JSON.stringify(params.snapshot)}

PRIOR ATTEMPTS ON THIS FINDING (do not repeat what already failed)
${priorAttempts || "  (none)"}

REQUIREMENTS
1. Diagnose the root cause in the actual code before writing a fix. If the finding is wrong or not actionable, make NO changes and say why in the result file.
2. Keep the diff minimal and surgical. Match surrounding style. Follow repo conventions from AGENTS.md (atomic writes via writeJson/updateJson, no em dashes in copy, signals wiring invariant, parity fixtures, etc.).
3. Add or update tests that lock the fix in: tests/*.test.mjs for TypeScript, native/src/app_tests.rs for the TUI.
4. Verify your own work: run \`npm run check\`, and the relevant test suite(s) for what you touched.
5. Commit your work yourself with a clear message: git add -A && git commit. Leave the worktree clean.
6. Write a file .rudder-improve-result.json at the worktree root: {"summary": "...", "changeClass": "prompt-only" | "logic", "testsRun": "...", "risks": "..."}. changeClass is prompt-only when you changed only prompt/instruction text.
7. NEVER: bump the version, touch package.json version or tags, push, publish, or edit files outside this worktree.`;
}

export async function runImprovementAgent(params: {
  settings: ImproveSettings;
  finding: Finding;
  worktree: string;
  branch: string;
  baseRef: string;
  contextPack: string;
  extraNote?: string;
}): Promise<{ proposal: Proposal | null; agentResult: StepResult }> {
  const agentLogPath = path.join(logsDir(), `${params.finding.id}-agent.log`);
  const args = ["-p", "--dangerously-skip-permissions"];
  if (params.settings.workerModel) {
    args.push("--model", params.settings.workerModel);
  }
  const prompt = params.extraNote
    ? `${params.contextPack}\n\nADDITIONAL CONTEXT FROM THE PREVIOUS ATTEMPT\n${params.extraNote}`
    : params.contextPack;
  const agentResult = await execStep({
    command: "claude",
    args,
    cwd: params.worktree,
    timeoutMs: params.settings.agentTimeoutMs,
    stdin: prompt,
    logFile: agentLogPath,
  });

  // Read (then drop) the agent's structured result note so it never lands in
  // the commit history.
  const resultPath = path.join(params.worktree, ".rudder-improve-result.json");
  const rawNote = await readJson<{
    summary?: string;
    changeClass?: string;
    testsRun?: string;
    risks?: string;
  }>(resultPath);
  await fs.rm(resultPath, { force: true });
  const resultNote = rawNote
    ? {
        summary: String(rawNote.summary ?? "").slice(0, 800),
        changeClass: rawNote.changeClass === "prompt-only" ? ("prompt-only" as const) : ("logic" as const),
        testsRun: rawNote.testsRun ? String(rawNote.testsRun).slice(0, 400) : undefined,
        risks: rawNote.risks ? String(rawNote.risks).slice(0, 400) : undefined,
      }
    : null;

  // Commit any uncommitted leftovers so the diff below is complete.
  const status = await runCommand("git", ["status", "--porcelain"], { cwd: params.worktree });
  if (status.stdout.trim()) {
    await runCommand("git", ["add", "-A"], { cwd: params.worktree });
    await runCommand(
      "git",
      ["commit", "-m", `improve: ${params.finding.title}\n\nAutomated fix for ${params.finding.id} (continual improvement loop).`],
      { cwd: params.worktree, allowFailure: true },
    );
  }

  const changed = await runCommand("git", ["diff", "--name-only", `${params.baseRef}..HEAD`], {
    cwd: params.worktree,
  });
  const changedFiles = changed.stdout.split("\n").map((line) => line.trim()).filter(Boolean);
  if (changedFiles.length === 0) {
    return { proposal: null, agentResult };
  }
  const diffStat = await runCommand("git", ["diff", "--stat", `${params.baseRef}..HEAD`], {
    cwd: params.worktree,
  });
  const diffText = await runCommand("git", ["diff", `${params.baseRef}..HEAD`], { cwd: params.worktree });

  return {
    agentResult,
    proposal: {
      finding: params.finding,
      worktree: params.worktree,
      branch: params.branch,
      baseRef: params.baseRef,
      changedFiles,
      diffStat: diffStat.stdout.trim(),
      diffText: tail(diffText.stdout, 120_000),
      resultNote,
      agentLogPath,
    },
  };
}
