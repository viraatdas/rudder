import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import { createSpec, renderContract, verifyRun } from "./brain.js";
import { ensureRudderCodexBinary } from "./codex-binary.js";
import { getBackend } from "./backends.js";
import { nativeAgentCommand } from "./native-agents.js";
import { mergeGeneratedRudderMd, withRudderMdLock } from "./rudder-md.js";
import {
  createRunRecord,
  agentContextPath,
  ensureProjectRuntimeIgnored,
  eventsPath,
  listRuns,
  loadConfig,
  loadRunRecord,
  outputPath,
  registerProject,
  rememberBackendSelection,
  resolveRun,
  runDir,
  saveRunRecord,
  stopRequestPath,
} from "./state.js";
import type { BackendId, EffortLevel, JsonValue, MergeStrategy, RunRecord, RudderEvent, VerificationResult } from "./types.js";
import {
  appendEvent,
} from "./state.js";
import {
  activeRunsForCheckout,
  currentBranch,
  findRepoRoot,
  mergeGitRunIntoCurrentBranch,
  processAlive,
  runProcessAlive,
  removeGitWorktree,
  syncGitRunWorktree,
} from "./git.js";
import {
  createRunJjWorkspace,
  closestLocalBookmark,
  currentJjChangeId,
  ensureColocated,
  ensureJj,
  exportToGit,
  mergeJjRunIntoCurrentWorkspace,
  removeRunWorkspace as removeJjRunWorkspace,
  syncRunWorkspace as syncJjRunWorkspace,
} from "./jj.js";
import {
  commandExists,
  ensureDir,
  isRecord,
  isTty,
  MissingToolError,
  newRunId,
  nowIso,
  pathExists,
  runCommand,
  shortenHome,
  textFromAssistantMessage,
  writeJson,
} from "./util.js";
import { taskDisplayLabel } from "./task-summary.js";
import { captureSharedContextFromInput, redactSharedSecretValues, SHARED_CONTEXT_FILE, syncSharedContextToWorkspaces } from "./surfaces.js";

// The pause before an automatic steering pass. Overridable via
// RUDDER_AUTO_STEER_DELAY_MS so the end-to-end test can drain a DAG quickly
// instead of waiting 10s per node; defaults to 10s for real runs.
const AUTO_STEER_DELAY_MS = Number(process.env.RUDDER_AUTO_STEER_DELAY_MS) || 10_000;

function missingBackendError(backend: BackendId, healthMessage: string): MissingToolError {
  return new MissingToolError(backend, healthMessage);
}

export async function startRun(params: {
  task: string;
  backend?: BackendId;
  model?: string;
  effort?: EffortLevel;
  detach?: boolean;
  worktree?: boolean;
  queue?: boolean;
  json?: boolean;
  exitOnComplete?: boolean;
  watchSignal?: AbortSignal;
  quiet?: boolean;
  silent?: boolean;
  view?: "default" | "shell";
}): Promise<RunRecord> {
  const repoRoot = findRepoRoot();
  await registerProject(repoRoot).catch(() => undefined);
  await captureSharedContextFromInput(repoRoot, params.task).catch(() => false);
  const config = await loadConfig();
  const backend = params.backend ?? config.lastUsedBackend ?? config.defaultBackend;
  if (!commandExists(backend)) {
    throw new MissingToolError(backend);
  }
  const model =
    params.model ??
    (backend === "claude"
      ? config.backends.claude?.model
      : backend === "codex"
        ? config.backends.codex?.model
        : config.backends.acpx?.model);
  const effort = params.effort ?? effortForBackend(backend, config);

  ensureJj();
  await ensureColocated(repoRoot);
  await ensureProjectRuntimeIgnored(repoRoot);

  const active = await activeRunsForCheckout(repoRoot, repoRoot);
  if (params.queue && active.length > 0) {
    throw new Error("Queue mode is not implemented yet; omit --queue to create a workspace run.");
  }
  const useWorktree = Boolean(params.worktree || active.length > 0);
  const baseCommit = await baseRevision(repoRoot);
  const targetBranch = await targetRevision(repoRoot);
  const id = newRunId(params.task);
  const worktreeInfo = useWorktree
    ? await createRunWorkspace({ repoRoot, runId: id, task: params.task })
    : { path: repoRoot, workspaceName: undefined, jjChangeId: undefined };
  const run = await createRunRecord({
    id,
    repoRoot,
    task: params.task,
    backend,
    model,
    effort,
    targetBranch,
    baseCommit,
    vcs: "jj",
    useWorktree,
    worktreeWorkspaceName: worktreeInfo.workspaceName,
    worktreeJjChangeId: worktreeInfo.jjChangeId,
    worktreePath: worktreeInfo.path,
  });
  await emit(run, {
    ts: nowIso(),
    runId: run.id,
    type: "run.created",
    message: useWorktree
      ? `Created jj workspace ${shortenHome(worktreeInfo.path)}`
      : "Created run in current checkout",
  });
  await writeAgentContext(repoRoot);

  await rememberBackendSelection({
    backend,
    model: params.model,
    effort: params.effort,
    updateModel: params.model !== undefined,
    updateEffort: params.effort !== undefined,
  });
  const attemptId = newAttemptId();
  const pid = spawnWorker(repoRoot, run.id, attemptId);
  run.process = {
    pid,
    controllerPid: pid,
    attemptId,
    startedAt: nowIso(),
  };
  run.status = "running";
  await saveRunRecord(run);

  if (params.json) {
    console.log(JSON.stringify(run, null, 2));
    return run;
  }
  if (!params.quiet && !params.silent) {
    console.log(`Started ${run.id}`);
    console.log(`  backend: ${backend}${model ? ` (${model})` : ""}`);
    console.log(`  mode:    ${useWorktree ? `worktree ${shortenHome(worktreeInfo.path)}` : "current checkout"}`);
  }
  if (params.detach || !isTty()) {
    if (params.silent) {
      return run;
    }
    if (params.quiet) {
      console.log(`Started ${run.id} in background. Use /watch ${run.id} or rudder watch ${run.id}.`);
    } else {
      console.log(`  watch:   rudder watch ${run.id}`);
    }
    return run;
  }
  await watchRun({
    repoRoot,
    runId: run.id,
    follow: true,
    exitOnComplete: params.exitOnComplete,
    signal: params.watchSignal,
    view: params.view,
  });
  return run;
}

export async function continueRun(params: {
  runId: string;
  prompt: string;
  interrupt?: boolean;
  silent?: boolean;
}): Promise<RunRecord> {
  const repoRoot = findRepoRoot();
  const run = await loadRunRecord(repoRoot, params.runId);
  if (!run) {
    throw new Error(`Run not found: ${params.runId}`);
  }
  if (run.mode && run.mode !== "execute") {
    throw new Error("Only headless execute runs can be continued from the standalone board.");
  }
  if (isActiveStatus(run.status) && run.status !== "steering" && !params.interrupt) {
    throw new Error("That agent is still running. Wait for it to finish before sending another message.");
  }
  if (params.interrupt) {
    await interruptWorkerAttempt(repoRoot, run);
  }
  const prompt = params.prompt.trim();
  if (!prompt) {
    throw new Error("Missing prompt.");
  }
  const ts = nowIso();
  const attemptId = newAttemptId();
  run.status = "running";
  run.currentPrompt = prompt;
  run.lastUserInputAt = ts;
  run.turns = [...(run.turns ?? []), { ts, prompt, source: "user" }];
  run.autoSteer = { count: 0, max: run.autoSteer?.max ?? 2 };
  const pid = spawnWorker(repoRoot, run.id, attemptId);
  run.process = {
    pid,
    controllerPid: pid,
    attemptId,
    startedAt: ts,
  };
  await saveRunRecord(run);
  await emit(run, {
    ts,
    runId: run.id,
    type: "run.continued",
    message: params.interrupt ? "User interrupted and redirected the agent" : "User continued the agent",
    data: { prompt },
  });
  await writeAgentContext(repoRoot);
  if (!params.silent) {
    console.log(`Continued ${run.id}`);
  }
  return run;
}

async function interruptWorkerAttempt(repoRoot: string, owned: RunRecord): Promise<void> {
  const known = new Set<number>();
  const controllerGroups = new Set<number>();
  const collect = (run: RunRecord | null): void => {
    if (run?.process?.controllerPid && run.process.controllerPid !== process.pid) {
      controllerGroups.add(run.process.controllerPid);
    }
    for (const pid of [run?.process?.controllerPid, run?.process?.backendPid, run?.process?.pid]) {
      if (pid && pid !== process.pid) known.add(pid);
    }
  };
  collect(owned);

  const terminate = async (signal: NodeJS.Signals, timeoutMs: number): Promise<boolean> => {
    const deadline = Date.now() + timeoutMs;
    const signalled = new Set<number>();
    const signalledGroups = new Set<number>();
    const stopped = (): boolean =>
      [...known].every((pid) => !processAlive(pid)) &&
      [...controllerGroups].every((pid) => !processGroupAlive(pid));
    while (Date.now() < deadline) {
      const latest = await loadRunRecord(repoRoot, owned.id);
      if (latest && sameAttempt(latest, owned)) collect(latest);
      for (const pid of controllerGroups) {
        if (!processGroupAlive(pid) || signalledGroups.has(pid)) continue;
        signalProcessGroup(pid, signal);
        signalledGroups.add(pid);
      }
      for (const pid of known) {
        if (!processAlive(pid) || signalled.has(pid)) continue;
        try {
          process.kill(pid, signal);
        } catch {
          // It exited between processAlive and kill.
        }
        signalled.add(pid);
      }
      if (stopped()) {
        // Let a controller that just spawned a backend publish that pid before
        // declaring the old attempt fully quiesced.
        await delay(75);
        const settled = await loadRunRecord(repoRoot, owned.id);
        if (settled && sameAttempt(settled, owned)) collect(settled);
        if (stopped()) return true;
      }
      await delay(50);
    }
    return stopped();
  };

  if (await terminate("SIGTERM", 2_000)) return;
  if (await terminate("SIGKILL", 1_000)) return;
  throw new Error("Could not stop the previous worker cleanly; redirect was not started.");
}

function processGroupAlive(controllerPid: number): boolean {
  if (process.platform === "win32") return processAlive(controllerPid);
  try {
    process.kill(-controllerPid, 0);
    return true;
  } catch {
    return false;
  }
}

function signalProcessGroup(controllerPid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(process.platform === "win32" ? controllerPid : -controllerPid, signal);
  } catch {
    // The process group exited between the liveness check and signal.
  }
}

/**
 * The detached `rudder __worker` spawn primitive. Both startRun/continueRun and
 * the scheduler launch workers through this one path so the spawn shape (flags,
 * detached, stderr capture, unref) is defined in exactly one place. Returns the
 * spawned worker pid (or undefined if the platform did not assign one).
 */
export function spawnWorker(repoRoot: string, runId: string, attemptId?: string): number | undefined {
  let stderrFd: number | undefined;
  try {
    fs.mkdirSync(runDir(repoRoot, runId), { recursive: true });
    stderrFd = fs.openSync(path.join(runDir(repoRoot, runId), "spawn-stderr.log"), "w", 0o600);
  } catch {
    stderrFd = undefined;
  }
  try {
    const worker = spawn(process.execPath, [
      process.argv[1] ?? "",
      "__worker",
      "--repo",
      repoRoot,
      "--run",
      runId,
      ...(attemptId ? ["--attempt", attemptId] : []),
    ], {
      cwd: repoRoot,
      detached: true,
      stdio: stderrFd === undefined ? "ignore" : ["ignore", "ignore", stderrFd],
    });
    worker.unref();
    return worker.pid;
  } finally {
    if (stderrFd !== undefined) {
      try {
        fs.closeSync(stderrFd);
      } catch {
        // ignore
      }
    }
  }
}

export function newAttemptId(): string {
  return randomUUID();
}

async function loadOwnedAttempt(
  repoRoot: string,
  runId: string,
  attemptId?: string,
): Promise<RunRecord | null> {
  for (let i = 0; i < (attemptId ? 600 : 1); i += 1) {
    const run = await loadRunRecord(repoRoot, runId);
    if (!run) return null;
    if (!attemptId || run.process?.attemptId === attemptId) return run;
    // startRun/continueRun spawn immediately before their atomic run.json save.
    // Give the cross-process JSON lock a bounded handoff window instead of
    // letting the child read the previous attempt and execute the wrong prompt.
    await delay(25);
  }
  return null;
}

function sameAttempt(left: RunRecord, right: RunRecord): boolean {
  const leftId = left.process?.attemptId;
  const rightId = right.process?.attemptId;
  if (leftId || rightId) return Boolean(leftId && rightId && leftId === rightId);
  return left.process?.startedAt === right.process?.startedAt;
}

async function attemptOwnsRun(repoRoot: string, run: RunRecord): Promise<boolean> {
  const latest = await loadRunRecord(repoRoot, run.id);
  return Boolean(latest && sameAttempt(latest, run));
}

function saveOwnedRun(run: RunRecord): Promise<boolean> {
  const expectedAttemptId = run.process?.attemptId;
  const expectedStartedAt = run.process?.startedAt;
  return saveRunRecord(
    run,
    expectedAttemptId
      ? { expectedAttemptId }
      : expectedStartedAt
        ? { expectedStartedAt }
        : undefined,
  );
}

export async function workerRun(repoRoot: string, runId: string, attemptId?: string): Promise<void> {
  const run = await loadOwnedAttempt(repoRoot, runId, attemptId);
  if (!run) {
    // A superseded worker may start after a redirect has already installed a
    // newer attempt. Exiting quietly is the ownership contract, not an error.
    if (attemptId && await loadRunRecord(repoRoot, runId)) {
      return;
    }
    throw new Error(`Run not found: ${runId}`);
  }
  try {
    let current = run;
    while (true) {
      const pass = await runBackendPass(current);
      current = pass.run;
      if (!(await attemptOwnsRun(repoRoot, current))) {
        await writeAgentContext(repoRoot);
        return;
      }
      if (pass.exitCode !== 0) {
        if (await newerWorkerOwnsRun(repoRoot, current)) {
          await writeAgentContext(repoRoot);
          return;
        }
        current.status = "failed";
        if (!(await saveOwnedRun(current))) return;
        await emit(current, {
          ts: nowIso(),
          runId,
          type: "run.failed",
          message: `Backend exited with ${pass.exitCode}`,
        });
        await writeAgentContext(repoRoot);
        return;
      }

      current.status = "verifying";
      if (!(await saveOwnedRun(current))) return;
      const verification = await verifyRun(current);
      if (!(await attemptOwnsRun(repoRoot, current))) return;
      current.verification = verification;
      if (!(await saveOwnedRun(current))) return;
      await emit(current, {
        ts: nowIso(),
        runId,
        type: "verifier.result",
        message: verification.notes,
        data: verification as unknown as JsonValue,
      });

      if (shouldAutoSteer(current, verification)) {
        const waitingSince = nowIso();
        current.status = "steering";
        current.autoSteer = {
          count: current.autoSteer?.count ?? 0,
          max: current.autoSteer?.max ?? 2,
          waitingSince,
        };
        if (!(await saveOwnedRun(current))) return;
        await emit(current, {
          ts: waitingSince,
          runId,
          type: "steerer.waiting",
          message: "Waiting 10 seconds before automatic steering",
        });
        await writeAgentContext(repoRoot);
        await delay(AUTO_STEER_DELAY_MS);
        const latest = await loadRunRecord(repoRoot, runId);
        if (!latest) {
          return;
        }
        if (!sameAttempt(latest, current)) {
          return;
        }
        if (latest.lastUserInputAt && latest.lastUserInputAt > waitingSince) {
          if (latest.status !== "running") {
            latest.status = "completed";
            if (!(await saveOwnedRun(latest))) return;
            await emit(latest, {
              ts: nowIso(),
              runId,
              type: "run.completed",
              message: "Run completed; user follow-up took over steering",
            });
          }
          await writeAgentContext(repoRoot);
          return;
        }
        const steeringPrompt = buildSteeringPrompt(verification);
        latest.currentPrompt = steeringPrompt;
        latest.turns = [...(latest.turns ?? []), { ts: nowIso(), prompt: steeringPrompt, source: "steerer" }];
        latest.autoSteer = {
          count: (latest.autoSteer?.count ?? 0) + 1,
          max: latest.autoSteer?.max ?? 2,
        };
        latest.status = "running";
        if (!(await saveOwnedRun(latest))) return;
        await emit(latest, {
          ts: nowIso(),
          runId,
          type: "steerer.prompt",
          message: "Automatic steering prompt sent",
          data: { prompt: steeringPrompt },
        });
        current = latest;
        continue;
      }

      current.status = verification.shouldContinue ? "failed" : "completed";
      if (!(await saveOwnedRun(current))) return;
      await emit(current, {
        ts: nowIso(),
        runId,
        type: current.status === "completed" ? "run.completed" : "run.failed",
        message:
          current.status === "completed"
            ? "Run completed"
            : `Run failed verification: ${verification.missing.join("; ")}`,
      });
      await writeAgentContext(repoRoot);
      return;
    }
  } catch (error) {
    if (await newerWorkerOwnsRun(repoRoot, run)) {
      await writeAgentContext(repoRoot);
      return;
    }
    const message = error instanceof Error ? error.message : String(error);
    run.status = "failed";
    run.process = {
      ...(run.process ?? {}),
      endedAt: nowIso(),
      exitCode: 1,
      signal: null,
    };
    if (!(await saveOwnedRun(run))) return;
    await emit(run, {
      ts: nowIso(),
      runId,
      type: "run.failed",
      message,
    });
    await writeAgentContext(repoRoot);
  }
}

async function newerWorkerOwnsRun(repoRoot: string, staleRun: RunRecord): Promise<boolean> {
  const latest = await loadRunRecord(repoRoot, staleRun.id);
  if (latest?.process?.attemptId && staleRun.process?.attemptId) {
    return latest.process.attemptId !== staleRun.process.attemptId;
  }
  return Boolean(
    latest &&
      isActiveStatus(latest.status) &&
      latest.process?.startedAt &&
      staleRun.process?.startedAt &&
      latest.process.startedAt !== staleRun.process.startedAt,
  );
}

async function runBackendPass(run: RunRecord): Promise<{ run: RunRecord; exitCode: number }> {
  if (run.mode && run.mode !== "execute") {
    throw new Error("Only execute runs use the background worker.");
  }
  const spec = await createSpec(run);
  await emit(run, {
    ts: nowIso(),
    runId: run.id,
    type: "planner.spec",
    message: "Planner contract created",
    data: spec as unknown as JsonValue,
  });
  const backend = getBackend(run.backend);
  const health = await backend.verify();
  if (!health.ok) {
    throw missingBackendError(run.backend, health.message);
  }
  const exitCode = await backend.run(
    {
      run,
      prompt: run.currentPrompt ?? run.task,
      contract: renderContract(spec),
    },
    async (event) => {
      await emit(run, event);
    },
  );
  run.process = {
    ...(run.process ?? {}),
    endedAt: nowIso(),
    exitCode,
    signal: null,
  };
  await saveOwnedRun(run);
  return { run, exitCode };
}

function shouldAutoSteer(run: RunRecord, verification: VerificationResult): boolean {
  if (run.mode === "plan") {
    return false;
  }
  if (run.status === "cancelled" || run.status === "merged" || run.status === "merge-conflict") {
    return false;
  }
  const max = run.autoSteer?.max ?? 2;
  const count = run.autoSteer?.count ?? 0;
  if (count >= max) {
    return false;
  }
  // Steer ONLY when the local verifier flagged a real gap (no file changes, or an
  // unmet acceptance criterion). A verified-clean pass (missing=[], !shouldContinue)
  // has already met its bar, so re-running the model on it is pure cost + a 10s wait
  // with nothing to fix. This used to also fire on the FIRST pass whenever the run
  // produced any changes, which meant EVERY successful node ran the backend twice.
  return verification.shouldContinue || verification.missing.length > 0;
}

function buildSteeringPrompt(verification: VerificationResult): string {
  const missing = verification.missing.length
    ? `\nKnown gaps:\n${verification.missing.map((item) => `- ${item}`).join("\n")}`
    : "";
  return [
    "Rudder automatic steering check:",
    "Pause and review the work you just completed.",
    "What remaining tasks are there, if any?",
    "Does the implementation look correct and scoped?",
    "Were the relevant tests/checks run? If not, run the smallest useful checks now or explain why not.",
    "If anything is missing, fix it. If everything is done, say that clearly and do not make unnecessary changes.",
    missing,
  ]
    .filter(Boolean)
    .join("\n");
}

export async function writeAgentContext(repoRoot: string): Promise<void> {
  const runs = await listRuns(repoRoot);
  const context = classifyContextRuns(runs);
  const lines = [
    "# RUDDER - Orchestrated Run Status   (read-only; re-read at the top of every significant step)",
    "",
    "This file is generated by Rudder. It is not user-authored repo documentation. Use it to coordinate with other Rudder agents in this checkout.",
    "",
    `Updated: ${nowIso()}`,
    "",
    "## Global job snapshot",
    ...formatRunSnapshot(context),
    "",
    "## Active local Rudder agents",
    ...(context.active.length
      ? context.active.map((run) => formatAgentContextRun(run))
      : ["- None"]),
    "",
    "## Ready local Rudder agents",
    ...(context.ready.length
      ? context.ready.map((run) => formatAgentContextRun(run))
      : ["- None"]),
    "",
    "## Completed local Rudder agents",
    ...(context.completed.length
      ? context.completed.slice(0, 12).map((run) => formatAgentContextRun(run))
      : ["- None"]),
    "",
  ];
  await writeRudderContextFiles(repoRoot, runs, `${lines.join("\n")}\n`);
}

type ContextRunBuckets = {
  active: RunRecord[];
  ready: RunRecord[];
  completed: RunRecord[];
  counts: {
    total: number;
    running: number;
    waiting: number;
    ready: number;
    mergeReady: number;
    merged: number;
    failed: number;
    stopped: number;
    pendingStarts: number;
    claude: number;
    codex: number;
    acpx: number;
  };
};

function classifyContextRuns(runs: RunRecord[]): ContextRunBuckets {
  const active: RunRecord[] = [];
  const ready: RunRecord[] = [];
  const completed: RunRecord[] = [];
  const counts: ContextRunBuckets["counts"] = {
    total: runs.length,
    running: 0,
    waiting: 0,
    ready: 0,
    mergeReady: 0,
    merged: 0,
    failed: 0,
    stopped: 0,
    pendingStarts: 0,
    claude: 0,
    codex: 0,
    acpx: 0,
  };

  for (const run of runs) {
    if (run.backend === "claude") counts.claude += 1;
    if (run.backend === "codex") counts.codex += 1;
    if (run.backend === "acpx") counts.acpx += 1;

    if (isContextRunningStatus(run.status)) counts.running += 1;
    if (isContextWaitingStatus(run.status)) counts.waiting += 1;
    if (run.status === "created") counts.pendingStarts += 1;
    if (run.status === "merged") counts.merged += 1;
    if (run.status === "failed") counts.failed += 1;
    if (run.status === "cancelled") counts.stopped += 1;

    if (isContextActiveStatus(run.status)) {
      active.push(run);
    }
    if (isContextReadyStatus(run.status)) {
      ready.push(run);
      counts.ready += 1;
      if (runHasMergeSource(run)) {
        counts.mergeReady += 1;
      }
    }
    if (isContextCompletedStatus(run.status)) {
      completed.push(run);
    }
  }

  return { active, ready, completed, counts };
}

function formatRunSnapshot(context: ContextRunBuckets): string[] {
  const { counts } = context;
  return [
    `- totals: total=${counts.total} running=${counts.running} waiting=${counts.waiting} done=${counts.ready} merged=${counts.merged} failed=${counts.failed} stopped=${counts.stopped} pending-starts=${counts.pendingStarts}`,
    `- active-now: running=${counts.running} waiting=${counts.waiting} review-ready=${counts.ready} merge-ready=${counts.mergeReady} pending-starts=${counts.pendingStarts}`,
    `- completed: merged=${counts.merged} failed=${counts.failed} stopped=${counts.stopped}`,
    `- backends: claude=${counts.claude} codex=${counts.codex} acpx=${counts.acpx}`,
    `- ready-to-act: review-ready=${counts.ready} merge-ready=${counts.mergeReady}`,
    `- archived: terminal-history=${context.completed.length}`,
  ];
}

function formatAgentContextRun(run: RunRecord): string {
  const location = run.worktree.enabled
    ? `${workspaceKind(run)}=${shortenHome(run.worktree.path)}`
    : "current checkout";
  const prompt = run.currentPrompt && run.currentPrompt !== run.task ? ` current="${previewAgentContext(run.currentPrompt, 140)}"` : "";
  return `- ${run.id}: ${run.status}, ${run.backend}, ${location}, task="${previewAgentContext(run.task, 140)}"${prompt}`;
}

function previewAgentContext(value: string, max: number): string {
  const normalized = redactSharedSecretValues(value).replace(/\s+/g, " ").replace(/"/g, '\\"').trim();
  return normalized.length > max ? `${normalized.slice(0, max)}...` : normalized;
}

async function writeRudderContextFiles(repoRoot: string, runs: RunRecord[], content: string): Promise<void> {
  await ensureLine(path.join(repoRoot, ".gitignore"), "RUDDER.md");
  await ensureLine(path.join(repoRoot, ".gitignore"), SHARED_CONTEXT_FILE);
  // Worktrees live INSIDE the project at <repo>/.rudder-worktrees; ignore them in the
  // user's repo so each worker checkout does not show up as untracked files (Rust parity:
  // gitio.rs write_rudder_context).
  await ensureLine(path.join(repoRoot, ".gitignore"), ".rudder-worktrees/");
  const workspaces = new Set<string>([repoRoot]);
  for (const run of runs) {
    if (run.worktree.path && await pathExists(run.worktree.path)) {
      workspaces.add(run.worktree.path);
    }
  }
  // RUDDER.md has concurrent writer processes (Rust TUI, other CLI invocations,
  // the daemon); the lock keeps this read-modify-write from dropping their output.
  await withRudderMdLock(repoRoot, async () => {
    for (const workspace of workspaces) {
      await ensureRudderExcluded(workspace);
      await ensureDir(path.dirname(agentContextPath(workspace)));
      const filePath = agentContextPath(workspace);
      const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
      await fsp.writeFile(filePath, mergeGeneratedRudderMd(existing, content), "utf8");
    }
  });
  await syncSharedContextToWorkspaces(repoRoot, workspaces);
}

async function ensureRudderExcluded(workspace: string): Promise<void> {
  const result = await runCommand("git", ["rev-parse", "--git-path", "info/exclude"], {
    cwd: workspace,
    allowFailure: true,
  });
  const excludePath = result.stdout.trim();
  if (!excludePath) {
    return;
  }
  await ensureLine(path.resolve(workspace, excludePath), "RUDDER.md");
  await ensureLine(path.resolve(workspace, excludePath), SHARED_CONTEXT_FILE);
}

async function ensureLine(filePath: string, line: string): Promise<void> {
  const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
  const lines = existing.split(/\r?\n/).map((item) => item.trim());
  if (lines.includes(line)) {
    return;
  }
  const prefix = existing && !existing.endsWith("\n") ? "\n" : "";
  await ensureDir(path.dirname(filePath));
  await fsp.appendFile(filePath, `${prefix}${line}\n`, "utf8");
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function createRunWorkspace(params: {
  repoRoot: string;
  runId: string;
  task: string;
}): Promise<{ path: string; workspaceName?: string; jjChangeId?: string }> {
  const workspace = await createRunJjWorkspace(params);
  const jjChangeId = await currentJjChangeId(workspace.path);
  return {
    path: workspace.path,
    workspaceName: workspace.workspaceName,
    jjChangeId: jjChangeId || undefined,
  };
}

// Route through the run's recorded vcs. New runs are always jj; legacy git
// worktree runs (run.vcs === "git") still merge/sync through the git helpers.
async function mergeRunIntoCurrentBranch(
  run: RunRecord,
  allowDirty: boolean,
  strategy: MergeStrategy,
): Promise<RunRecord> {
  if ((run.vcs ?? "git") === "jj") {
    return await mergeJjRunIntoCurrentWorkspace(run, allowDirty);
  }
  return await mergeGitRunIntoCurrentBranch(run, allowDirty, strategy);
}

async function syncRunWorktree(run: RunRecord, baseBranch: string): Promise<RunRecord> {
  if ((run.vcs ?? "git") === "jj") {
    return await syncJjRunWorkspace(run, baseBranch);
  }
  return await syncGitRunWorktree(run, baseBranch);
}

async function removeRunWorkspace(run: RunRecord, force = true): Promise<void> {
  if ((run.vcs ?? "git") === "jj") {
    await removeJjRunWorkspace(run);
    return;
  }
  if (!run.worktree.enabled) {
    return;
  }
  await removeGitWorktree(run.repoRoot, run.worktree.path, force);
}

async function baseRevision(repoRoot: string): Promise<string> {
  return (await currentJjChangeId(repoRoot)) || "";
}

async function targetRevision(repoRoot: string): Promise<string> {
  const branch = await currentBranch(repoRoot);
  if (branch && branch !== "HEAD") return branch;
  return (await closestLocalBookmark(repoRoot)) || "main";
}

function effortForBackend(backend: BackendId, config: Awaited<ReturnType<typeof loadConfig>>): EffortLevel | undefined {
  if (backend === "claude") {
    return config.backends.claude?.effort;
  }
  if (backend === "codex") {
    return config.backends.codex?.reasoningEffort ?? config.backends.codex?.effort;
  }
  return config.backends.acpx?.reasoningEffort ?? config.backends.acpx?.effort;
}

function shortRunTask(run: RunRecord): string {
  return taskDisplayLabel(run, 34) || "agent";
}

function workspaceKind(run: RunRecord): string {
  return (run.vcs ?? "git") === "jj" ? "jj-workspace" : "worktree";
}

export async function statusRuns(options?: { json?: boolean }): Promise<void> {
  const repoRoot = findRepoRoot();
  const runs = await listRuns(repoRoot);
  const active = runs.filter((run) => isActiveStatus(run.status));
  if (options?.json) {
    console.log(JSON.stringify({ repoRoot, active, runs }, null, 2));
    return;
  }
  console.log(`Repo: ${repoRoot}`);
  if (active.length === 0) {
    console.log("No active runs.");
    return;
  }
  for (const run of active) {
    const alive = runProcessAlive(run) ? "alive" : "stale";
    console.log(`${run.id}  ${run.status}  ${alive}  ${run.backend}  ${run.task}`);
  }
}

export async function listProjectRuns(options?: { json?: boolean }): Promise<void> {
  const repoRoot = findRepoRoot();
  const runs = await listRuns(repoRoot);
  if (options?.json) {
    console.log(JSON.stringify(runs, null, 2));
    return;
  }
  for (const run of runs) {
    const wt = run.worktree.enabled ? ` ${workspaceKind(run)}=${shortenHome(run.worktree.path)}` : "";
    console.log(`${run.id}  ${run.status}  ${run.backend}${wt}  ${run.task}`);
  }
}

export async function watchRun(params: {
  repoRoot?: string;
  runId?: string;
  follow?: boolean;
  exitOnComplete?: boolean;
  signal?: AbortSignal;
  view?: "default" | "shell";
}): Promise<void> {
  const repoRoot = params.repoRoot ?? findRepoRoot();
  const run = await resolveRun(repoRoot, params.runId);
  if (!run) {
    throw new Error("No runs found.");
  }
  const file = eventsPath(repoRoot, run.id);
  await waitForFile(file);
  let offset = 0;
  const initial = await fsp.readFile(file, "utf8").catch(() => "");
  offset = Buffer.byteLength(initial);
  const view = params.view ?? "default";
  const renderer = createEventRenderer(view);
  renderer.print(initial);
  if (!params.follow) {
    return;
  }
  const alreadyDone = await loadRunRecord(repoRoot, run.id);
  if (alreadyDone && !isActiveStatus(alreadyDone.status)) {
    if (params.exitOnComplete !== false) {
      process.exitCode = terminalExitCode(alreadyDone.status);
    }
    return;
  }
  if (view === "default") {
    console.log(`Watching ${run.id}. Ctrl-C detaches; use 'rudder stop ${run.id}' to cancel.`);
  }
  await followFile(file, offset, async (chunk) => {
    renderer.print(chunk);
    const latest = await loadRunRecord(repoRoot, run.id);
    if (latest && !isActiveStatus(latest.status)) {
      if (params.exitOnComplete === false) {
        renderer.finish();
        return false;
      }
      renderer.finish();
      process.exitCode = terminalExitCode(latest.status);
      process.exit();
    }
    return true;
  }, params.signal);
}

export async function printLogs(runId?: string, follow = false): Promise<void> {
  const repoRoot = findRepoRoot();
  const run = await resolveRun(repoRoot, runId);
  if (!run) {
    throw new Error("No runs found.");
  }
  const file = outputPath(repoRoot, run.id);
  if (!(await pathExists(file))) {
    return;
  }
  let offset = 0;
  await new Promise<void>((resolve, reject) => {
    const stream = fs.createReadStream(file);
    stream.on("data", (chunk) => {
      offset += Buffer.isBuffer(chunk) ? chunk.length : Buffer.byteLength(chunk);
      process.stdout.write(chunk);
    });
    stream.on("error", reject);
    stream.on("end", resolve);
  });
  if (follow) {
    await followFile(file, offset, async (chunk) => {
      process.stdout.write(chunk);
    });
  }
}

export async function stopRun(runId: string, options?: { silent?: boolean }): Promise<void> {
  const repoRoot = findRepoRoot();
  const run = await loadRunRecord(repoRoot, runId);
  if (!run) {
    throw new Error(`Run not found: ${runId}`);
  }
  // The native dashboard owns interactive PTYs in another process. Record the
  // stop intent first so its poll loop observes cancellation before child-exit
  // handling can classify our SIGTERM/SIGKILL as a failure. Headless workers
  // also tolerate the marker; it is removed by the native owner when present.
  await writeJson(stopRequestPath(repoRoot, runId), { requestedAt: nowIso() });
  await interruptWorkerAttempt(repoRoot, run);
  run.status = "cancelled";
  run.process = {
    ...(run.process ?? {}),
    // Invalidate every callback still carrying the old ownership token.
    attemptId: newAttemptId(),
    endedAt: nowIso(),
    signal: "SIGTERM",
  };
  await saveRunRecord(run);
  await emit(run, { ts: nowIso(), runId, type: "run.cancelled", message: "Run cancelled" });
  await writeAgentContext(repoRoot);
  if (!options?.silent) {
    console.log(`Stopped ${runId}`);
  }
}

export async function mergeRun(runId: string, allowDirty = false, options?: { silent?: boolean }): Promise<RunRecord> {
  const repoRoot = findRepoRoot();
  const run = await loadRunRecord(repoRoot, runId);
  if (!run) {
    throw new Error(`Run not found: ${runId}`);
  }
  const config = await loadConfig();
  const merged = await mergeRunIntoCurrentBranch(run, allowDirty, config.mergeStrategy);
  await emit(merged, {
    ts: nowIso(),
    runId,
    type: "merge.result",
    message: mergeResultMessage(merged),
    data: (merged.merge ?? {}) as unknown as JsonValue,
  });
  if (merged.merge?.status === "merged") {
    if ((merged.vcs ?? "git") === "jj") {
      await exportToGit(repoRoot);
    }
    await writeAgentContext(repoRoot);
    if (!options?.silent) {
      console.log(`Merged ${runId}`);
    }
    return merged;
  }
  await writeAgentContext(repoRoot);
  if (!options?.silent) {
    if (merged.merge?.status === "conflict") {
      const kind = merged.merge.conflictKind === "rebase" ? "Rebase conflict" : "Merge conflict";
      console.log(`${kind} for ${runId}`);
      for (const file of merged.merge?.conflictedFiles ?? []) {
        console.log(`  ${file}`);
      }
      if (merged.merge.conflictKind === "rebase") {
        console.log(`Resolve in ${shortenHome(merged.worktree.path)}, run git rebase --continue, then retry rudder merge ${runId}.`);
      }
    } else {
      console.log(`Merge failed for ${runId}: ${merged.merge?.error ?? "unknown error"}`);
    }
  }
  return merged;
}

export async function syncRun(runId?: string, options?: { silent?: boolean }): Promise<RunRecord> {
  const repoRoot = findRepoRoot();
  const run = await resolveRun(repoRoot, runId);
  if (!run) {
    throw new Error("No runs found.");
  }
  const hasIsolation = run.worktree.enabled && (run.worktree.workspaceName || run.worktree.branch);
  if (!hasIsolation) {
    throw new Error(`Run ${run.id} has no workspace to sync.`);
  }
  if (isActiveStatus(run.status)) {
    throw new Error(`Run ${run.id} is still active; wait for it to finish before syncing.`);
  }
  if (run.status === "merged") {
    throw new Error(`Run ${run.id} is already merged.`);
  }
  const current = await currentBranch(repoRoot);
  const baseBranch = current === "HEAD" ? run.targetBranch : current;
  const synced = await syncRunWorktree(run, baseBranch);
  await emit(synced, {
    ts: nowIso(),
    runId: synced.id,
    type: "sync.result",
    message: syncResultMessage(synced),
    data: (synced.sync ?? {}) as unknown as JsonValue,
  });
  await writeAgentContext(repoRoot);
  if (!options?.silent) {
    if (synced.sync?.status === "synced") {
      console.log(`Synced ${synced.id} with ${synced.sync.baseBranch ?? synced.targetBranch}`);
    } else if (synced.sync?.status === "conflict") {
      console.log(`Rebase conflict for ${synced.id}`);
      for (const file of synced.sync.conflictedFiles ?? []) {
        console.log(`  ${file}`);
      }
      console.log(`Resolve in ${shortenHome(synced.worktree.path)}, run git rebase --continue, then retry rudder sync ${synced.id}.`);
    } else {
      console.log(`Sync failed for ${synced.id}: ${synced.sync?.error ?? "unknown error"}`);
    }
  }
  return synced;
}

export async function deleteRun(runId: string, options?: { mergeFirst?: boolean; force?: boolean; silent?: boolean }): Promise<void> {
  const repoRoot = findRepoRoot();
  const run = await loadRunRecord(repoRoot, runId);
  if (!run) {
    throw new Error(`Run not found: ${runId}`);
  }
  let mergeError: unknown;
  if (options?.mergeFirst) {
    try {
      await mergeRun(runId, false, { silent: true });
    } catch (error) {
      mergeError = error;
    }
  }
  const latest = await loadRunRecord(repoRoot, runId) ?? run;
  if (options?.mergeFirst && latest.merge?.status === "conflict") {
    throw new Error(`Merge conflict for ${runId}; resolve it before deleting the run.`);
  }
  if (mergeError && !options?.force) {
    const message = mergeError instanceof Error ? mergeError.message : String(mergeError);
    throw new Error(`Merge failed for ${runId}; run was not deleted. ${message}`);
  }
  await interruptWorkerAttempt(repoRoot, latest);
  if (latest.worktree.enabled) {
    await removeRunWorkspace(latest, options?.force ?? true).catch(() => undefined);
  }
  await fsp.rm(runDir(repoRoot, runId), { recursive: true, force: true });
  await writeAgentContext(repoRoot);
  if (!options?.silent) {
    console.log(`Deleted ${runId}`);
  }
}

function mergeResultMessage(run: RunRecord): string {
  if (run.merge?.status === "merged") {
    return run.merge.strategy === "rebase"
      ? "Rebased and fast-forward merged successfully"
      : "Merged successfully";
  }
  if (run.merge?.status === "conflict") {
    const files = (run.merge.conflictedFiles ?? []).join(", ") || "unknown files";
    if (run.merge.conflictKind === "rebase") {
      return `Rebase conflict before merge: ${files}. Resolve in the worktree, run git rebase --continue, then retry merge.`;
    }
    return `Merge conflict: ${files}`;
  }
  if (run.merge?.status === "failed") {
    return `Merge failed: ${run.merge.error ?? "unknown error"}`;
  }
  return "Merge did not complete";
}

function syncResultMessage(run: RunRecord): string {
  if (run.sync?.status === "synced") {
    return `Synced with ${run.sync.baseBranch ?? run.targetBranch}`;
  }
  if (run.sync?.status === "conflict") {
    const files = (run.sync.conflictedFiles ?? []).join(", ") || "unknown files";
    return `Rebase conflict while syncing: ${files}. Resolve in the worktree, run git rebase --continue, then retry sync.`;
  }
  if (run.sync?.status === "failed") {
    return `Sync failed: ${run.sync.error ?? "unknown error"}`;
  }
  return "Sync did not complete";
}

export async function cleanupRuns(force = false): Promise<void> {
  const repoRoot = findRepoRoot();
  const runs = await listRuns(repoRoot);
  for (const run of runs) {
    if (!canCleanupRun(run, force)) {
      continue;
    }
    try {
      await removeRunWorkspace(run, force);
      console.log(`Removed ${shortenHome(run.worktree.path)}`);
    } catch {
      // Best-effort cleanup matches the existing git worktree behavior.
    }
  }
  await writeAgentContext(repoRoot);
}

function canCleanupRun(run: RunRecord, force: boolean): boolean {
  if (!run.worktree.enabled) {
    return false;
  }
  if (force) {
    return true;
  }
  if ((run.vcs ?? "git") === "jj") {
    return run.status === "merged" || run.status === "completed";
  }
  return run.status === "merged";
}

async function emit(run: RunRecord, event: RudderEvent): Promise<void> {
  await ensureDir(runDir(run.repoRoot, run.id));
  await appendEvent(run.repoRoot, event);
}

async function waitForFile(file: string): Promise<void> {
  for (let i = 0; i < 50; i += 1) {
    if (await pathExists(file)) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

function createEventRenderer(view: "default" | "shell"): {
  print(raw: string): void;
  finish(): void;
} {
  let sawStreamingText = false;
  let partialOpen = false;
  return {
    print(raw: string): void {
      for (const line of raw.split(/\r?\n/)) {
        if (!line.trim()) {
          continue;
        }
        try {
          const event = JSON.parse(line) as RudderEvent;
          if (view === "shell") {
            const rendered = renderShellEvent(event, {
              sawStreamingText,
              partialOpen,
            });
            sawStreamingText = rendered.sawStreamingText;
            partialOpen = rendered.partialOpen;
            if (rendered.text) {
              if (rendered.inline) {
                process.stdout.write(rendered.text);
              } else {
                if (partialOpen) {
                  process.stdout.write("\n");
                  partialOpen = false;
                }
                console.log(rendered.text);
              }
            }
            continue;
          }
          printDefaultEvent(event);
        } catch {
          console.log(line);
        }
      }
    },
    finish(): void {
      if (partialOpen) {
        process.stdout.write("\n");
        partialOpen = false;
      }
    },
  };
}

function printDefaultEvent(event: RudderEvent): void {
  if (event.type === "backend.output" || event.type === "backend.error") {
    if (event.message) {
      console.log(event.message);
    } else if (event.data) {
      const text = formatBackendData(event.data);
      if (text) {
        console.log(text);
      }
    }
  } else if (event.message) {
    console.log(`[rudder] ${event.message}`);
  }
}

function renderShellEvent(
  event: RudderEvent,
  state: { sawStreamingText: boolean; partialOpen: boolean },
): { text?: string; inline?: boolean; sawStreamingText: boolean; partialOpen: boolean } {
  let sawStreamingText = state.sawStreamingText;
  let partialOpen = state.partialOpen;
  if (event.type === "run.created") {
    const message = event.message?.startsWith("Created jj workspace ")
      ? event.message.replace("Created jj workspace ", "workspace ")
      : event.message?.startsWith("Created worktree ")
        ? event.message.replace("Created worktree ", "worktree ")
        : undefined;
    return { text: message, sawStreamingText, partialOpen };
  }
  if (event.type === "planner.spec") {
    return { sawStreamingText, partialOpen };
  }
  if (event.type === "run.started") {
    const command = objectField(event.data, "command");
    return {
      text: command ? `running ${command}` : undefined,
      sawStreamingText,
      partialOpen,
    };
  }
  if (event.type === "backend.output" || event.type === "backend.error") {
    const rendered = renderBackendForShell(event.data ?? event.message, event.type === "backend.error", sawStreamingText);
    if (rendered.sawStreamingText) {
      sawStreamingText = true;
    }
    if (rendered.inline) {
      partialOpen = true;
    } else if (rendered.text) {
      partialOpen = false;
    }
    return { ...rendered, sawStreamingText, partialOpen };
  }
  if (event.type === "backend.exit") {
    return { sawStreamingText, partialOpen };
  }
  if (event.type === "verifier.result") {
    const missing = Array.isArray((event.data as { missing?: unknown } | undefined)?.missing)
      ? ((event.data as { missing: unknown[] }).missing.filter((item) => typeof item === "string") as string[])
      : [];
    return {
      text: missing.length ? `verification needs follow-up: ${missing.join("; ")}` : undefined,
      sawStreamingText,
      partialOpen,
    };
  }
  if (event.type === "run.completed") {
    return { text: "done", sawStreamingText, partialOpen };
  }
  if (event.type === "run.failed") {
    return { text: event.message ? `failed: ${event.message}` : "failed", sawStreamingText, partialOpen };
  }
  if (event.type === "run.cancelled") {
    return { text: "cancelled", sawStreamingText, partialOpen };
  }
  if (event.type === "merge.result") {
    return { text: event.message, sawStreamingText, partialOpen };
  }
  return { sawStreamingText, partialOpen };
}

function renderBackendForShell(
  data: unknown,
  stderr: boolean,
  sawStreamingText: boolean,
): { text?: string; inline?: boolean; sawStreamingText?: boolean } {
  if (typeof data === "string") {
    return { text: stderr ? `error: ${data}` : data };
  }
  if (!data || typeof data !== "object") {
    return {};
  }
  const record = data as Record<string, unknown>;
  if (record.type === "stream_event" && isRecord(record.event)) {
    const event = record.event;
    if (event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string") {
      return { text: event.delta.text, inline: true, sawStreamingText: true };
    }
    return {};
  }
  if (record.type === "assistant") {
    if (sawStreamingText) {
      return {};
    }
    const text = textFromAssistantMessage(record.message);
    return text ? { text } : {};
  }
  if (record.type === "result") {
    if (record.subtype === "success") {
      const text = typeof record.result === "string" && !sawStreamingText ? record.result.trim() : "";
      return text ? { text } : {};
    }
    const errors = Array.isArray(record.errors) ? record.errors.filter((item) => typeof item === "string") : [];
    return { text: `error: ${errors.join(", ") || String(record.subtype ?? "unknown")}` };
  }
  if (record.type === "system") {
    return {};
  }
  return {};
}

function objectField(data: unknown, key: string): string | undefined {
  return isRecord(data) && typeof data[key] === "string" ? data[key] : undefined;
}


function formatBackendData(data: unknown): string {
  if (typeof data === "string") {
    return data;
  }
  if (data && typeof data === "object") {
    const record = data as Record<string, unknown>;
    if (record.type === "system" || record.type === "rate_limit_event" || record.type === "tool_use_summary") {
      return "";
    }
    if (record.type === "stream_event" && isRecord(record.event)) {
      const event = record.event;
      if (event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string") {
        return event.delta.text;
      }
      return "";
    }
    if (record.type === "assistant") {
      return textFromAssistantMessage(record.message);
    }
    if (record.type === "result") {
      if (record.subtype === "success" && typeof record.result === "string") {
        return record.result;
      }
      if (Array.isArray(record.errors)) {
        return record.errors.filter((item) => typeof item === "string").join(", ");
      }
    }
    for (const key of ["message", "text", "content", "delta"]) {
      if (typeof record[key] === "string") {
        return record[key];
      }
    }
    if (typeof record.type === "string") {
      return `[${record.type}] ${JSON.stringify(record)}`;
    }
  }
  return JSON.stringify(data);
}

async function followFile(
  file: string,
  startOffset: number,
  onChunk: (chunk: string) => Promise<boolean | void>,
  signal?: AbortSignal,
): Promise<void> {
  let offset = startOffset;
  while (!signal?.aborted) {
    const stat = await fsp.stat(file).catch(() => null);
    if (stat && stat.size > offset) {
      const handle = await fsp.open(file, "r");
      try {
        const length = stat.size - offset;
        const buffer = Buffer.alloc(length);
        await handle.read(buffer, 0, length, offset);
        offset = stat.size;
        const keepGoing = await onChunk(buffer.toString("utf8"));
        if (keepGoing === false) {
          return;
        }
      } finally {
        await handle.close();
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 350));
  }
}

function isActiveStatus(status: RunRecord["status"]): boolean {
  return status === "created" || status === "running" || status === "steering" || status === "verifying";
}

function isContextActiveStatus(status: RunRecord["status"]): boolean {
  return isActiveStatus(status);
}

function isContextRunningStatus(status: RunRecord["status"]): boolean {
  return status === "running" || status === "verifying";
}

function isContextWaitingStatus(status: RunRecord["status"]): boolean {
  return status === "steering";
}

function isContextReadyStatus(status: RunRecord["status"]): boolean {
  return status === "completed" || status === "merge-conflict";
}

function isContextCompletedStatus(status: RunRecord["status"]): boolean {
  return status === "merged" || status === "failed" || status === "cancelled";
}

function runHasMergeSource(run: RunRecord): boolean {
  return Boolean(
    run.worktree.enabled &&
      (run.worktree.path || run.worktree.branch || run.worktree.workspaceName || run.worktree.jjChangeId),
  );
}

function terminalExitCode(status: RunRecord["status"]): number {
  return status === "completed" || status === "merged" ? 0 : 1;
}
