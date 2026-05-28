import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import type { MergeStrategy, RunRecord, VcsMode } from "./types.js";
import {
  commandExists,
  MissingToolError,
  pathExistsSync,
  runCommand,
  runCommandSync,
  shortHash,
  slugPrefix,
} from "./util.js";
import { listRuns, saveRunRecord, worktreePath } from "./state.js";

export type RebaseResult = {
  success: boolean;
  baseRef: string;
  conflictedFiles: string[];
  error?: string;
};

export function findRepoRoot(cwd = process.cwd()): string {
  const result = runCommandSync("git", ["rev-parse", "--show-toplevel"], {
    cwd,
    allowFailure: true,
  });
  if (result.code === 0 && result.stdout.trim()) {
    return result.stdout.trim();
  }
  return findJjRoot(cwd);
}

export function isGitRepo(cwd: string): boolean {
  return (
    runCommandSync("git", ["rev-parse", "--is-inside-work-tree"], {
      cwd,
      allowFailure: true,
    }).stdout.trim() === "true"
  );
}

export function isJjRepo(cwd: string): boolean {
  if (!commandExists("jj")) {
    return false;
  }
  const result = runCommandSync("jj", ["root"], {
    cwd,
    allowFailure: true,
  });
  if (result.code === 0 && result.stdout.trim()) {
    return samePath(result.stdout.trim(), cwd);
  }
  return hasLocalJjMarker(cwd);
}

export function findJjRoot(cwd: string): string {
  if (commandExists("jj")) {
    const result = runCommandSync("jj", ["root"], {
      cwd,
      allowFailure: true,
    });
    if (result.code === 0 && result.stdout.trim()) {
      return result.stdout.trim();
    }
  }
  const marker = findJjMarker(cwd);
  return marker ? path.dirname(marker) : path.resolve(cwd);
}

export function detectVcsMode(repoRoot: string, configured?: VcsMode): VcsMode {
  if (configured === "git") {
    return "git";
  }
  if (configured === "jj") {
    ensureJjRepoRoot(repoRoot);
    return "jj";
  }
  return isJjRepo(repoRoot) ? "jj" : "git";
}

export async function currentBranch(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["branch", "--show-current"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim() || "HEAD";
}

export async function currentCommit(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["rev-parse", "HEAD"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim() || "";
}

export async function currentJjChangeId(workspacePath: string): Promise<string> {
  const result = await runCommand("jj", ["log", "--no-graph", "-r", "@", "-T", 'change_id ++ "\n"'], {
    cwd: workspacePath,
    allowFailure: true,
  });
  return result.code === 0 ? result.stdout.trim().split(/\s+/)[0] ?? "" : "";
}

export async function worktreeBaseCommit(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["rev-parse", "main"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim() || await currentCommit(repoRoot);
}

export async function gitStatus(repoRoot: string): Promise<string[]> {
  const result = await runCommand("git", ["status", "--short"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout
    .split(/\r?\n/)
    .map((line) => line.trimEnd())
    .filter(Boolean)
    .filter((line) => {
      const file = line.slice(3).trim();
      return file !== ".rudder" && !file.startsWith(".rudder/");
    });
}

export async function gitDiff(repoRoot: string): Promise<string> {
  const status = await gitStatus(repoRoot);
  const result = await runCommand("git", ["diff", "--stat", "--", "."], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const patch = await runCommand("git", ["diff", "--", "."], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return [
    status.length ? `status:\n${status.join("\n")}` : "",
    result.stdout.trim(),
    patch.stdout.trim(),
  ]
    .filter(Boolean)
    .join("\n\n");
}

export async function hasChanges(repoRoot: string): Promise<boolean> {
  return (await gitStatus(repoRoot)).length > 0;
}

export async function jjStatus(workspacePath: string): Promise<string[]> {
  const result = await runCommand("jj", ["status"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  return result.code === 0 ? parseJjStatus(result.stdout) : [];
}

export function parseJjStatus(stdout: string): string[] {
  const lines: string[] = [];
  let inConflictSection = false;
  for (const raw of stdout.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) {
      continue;
    }
    if (/^working copy changes:?$/i.test(line)) {
      inConflictSection = false;
      continue;
    }
    if (/^(conflicts|unresolved conflicts|there are unresolved conflicts)(:|$)/i.test(line)) {
      inConflictSection = true;
      continue;
    }
    if (isJjStatusMetadata(line)) {
      continue;
    }
    const statusPath = statusPathFromSummaryLine(line);
    if (statusPath) {
      if (!isRudderMetadataPath(statusPath)) {
        lines.push(line);
      }
      continue;
    }
    if (inConflictSection) {
      const conflictPath = line.replace(/^[-*]\s+/, "").replace(/^conflict in\s+/i, "").trim();
      if (conflictPath && !isRudderMetadataPath(conflictPath)) {
        lines.push(`C ${conflictPath}`);
      }
    }
  }
  return lines;
}

export async function jjDiff(workspacePath: string): Promise<string> {
  const status = await jjStatus(workspacePath);
  const stat = await runCommand("jj", ["diff", "--stat"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  const patch = await runCommand("jj", ["diff", "--git"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  return [
    status.length ? `status:\n${status.join("\n")}` : "",
    stat.stdout.trim(),
    patch.stdout.trim(),
  ]
    .filter(Boolean)
    .join("\n\n");
}

export async function workspaceStatus(run: RunRecord): Promise<string[]> {
  return (run.vcs ?? "git") === "jj" ? await jjStatus(run.worktree.path) : await gitStatus(run.worktree.path);
}

export async function workspaceDiff(run: RunRecord): Promise<string> {
  return (run.vcs ?? "git") === "jj" ? await jjDiff(run.worktree.path) : await gitDiff(run.worktree.path);
}

export async function runHasChanges(run: RunRecord): Promise<boolean> {
  return (await workspaceStatus(run)).length > 0;
}

export function processAlive(pid: number | undefined): boolean {
  if (!pid) {
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export async function activeRunsForCheckout(repoRoot: string, checkoutPath: string): Promise<RunRecord[]> {
  const runs = await listRuns(repoRoot);
  return runs.filter((run) => {
    if (!["created", "running", "verifying"].includes(run.status)) {
      return false;
    }
    if (path.resolve(run.worktree.path) !== path.resolve(checkoutPath)) {
      return false;
    }
    return processAlive(run.process?.pid);
  });
}

export async function createRunWorktree(params: {
  repoRoot: string;
  runId: string;
  task: string;
  baseCommit: string;
}): Promise<{ branch: string; path: string }> {
  const taskSlug = slugPrefix(params.task, "task");
  const branch = `rudder/${taskSlug}-${shortHash(params.runId).slice(0, 8)}`;
  const targetPath = worktreePath(params.repoRoot, params.runId, params.task);
  await fsp.mkdir(path.dirname(targetPath), { recursive: true });

  await runCommand("git", ["branch", branch, params.baseCommit], {
    cwd: params.repoRoot,
    allowFailure: true,
  });
  await runCommand("git", ["worktree", "add", targetPath, branch], {
    cwd: params.repoRoot,
  });
  return { branch, path: targetPath };
}

export async function createRunJjWorkspace(params: {
  repoRoot: string;
  runId: string;
  task: string;
}): Promise<{ workspaceName: string; path: string }> {
  ensureJjRepo(params.repoRoot);
  const workspaceName = `rudder-${params.runId.slice(0, 14)}-${shortHash(params.runId).slice(0, 6)}`;
  const targetPath = worktreePath(params.repoRoot, params.runId, params.task);
  await fsp.mkdir(path.dirname(targetPath), { recursive: true });

  await runCommand("jj", ["workspace", "add", targetPath, "--name", workspaceName], {
    cwd: params.repoRoot,
  });
  return { workspaceName, path: targetPath };
}

export async function removeWorktree(repoRoot: string, targetPath: string, force = false): Promise<void> {
  await runCommand("git", ["worktree", "remove", ...(force ? ["--force"] : []), targetPath], {
    cwd: repoRoot,
    allowFailure: force,
  });
}

export async function removeJjWorkspace(params: {
  repoRoot: string;
  workspaceName: string;
  workspacePath: string;
}): Promise<void> {
  if (commandExists("jj")) {
    await runCommand("jj", ["workspace", "forget", params.workspaceName], {
      cwd: params.repoRoot,
      allowFailure: true,
    }).catch(() => undefined);
  }
  await fsp.rm(params.workspacePath, { recursive: true, force: true });
}

export async function removeRunWorkspace(run: RunRecord, force = false): Promise<void> {
  if (!run.worktree.enabled) {
    return;
  }
  if ((run.vcs ?? "git") === "jj") {
    if (!run.worktree.workspaceName) {
      throw new Error("Run has no jj workspace name to remove.");
    }
    await removeJjWorkspace({
      repoRoot: run.repoRoot,
      workspaceName: run.worktree.workspaceName,
      workspacePath: run.worktree.path,
    });
    return;
  }
  await removeWorktree(run.repoRoot, run.worktree.path, force);
}

export async function mergeRunIntoCurrentBranch(
  run: RunRecord,
  allowDirty = false,
  strategy: MergeStrategy = "merge",
): Promise<RunRecord> {
  if ((run.vcs ?? "git") === "jj") {
    return await mergeJjRunIntoCurrentWorkspace(run, allowDirty);
  }
  return await mergeGitRunIntoCurrentBranch(run, allowDirty, strategy);
}

async function mergeGitRunIntoCurrentBranch(
  run: RunRecord,
  allowDirty = false,
  strategy: MergeStrategy = "merge",
): Promise<RunRecord> {
  if (!run.worktree.branch) {
    throw new Error("Run has no worktree branch to merge.");
  }
  if (!allowDirty && (await hasChanges(run.repoRoot))) {
    throw new Error("Target branch is dirty. Commit/stash changes or pass --allow-dirty.");
  }
  const targetBranch = await currentTargetBranch(run);
  run.merge = {
    status: "not-started",
    attemptedAt: new Date().toISOString(),
    targetBranch,
    strategy,
  };
  await saveRunRecord(run);

  const commitFailure = await commitWorktreeChangesSafely(run, "merge");
  if (commitFailure) {
    return commitFailure;
  }

  if (strategy === "rebase") {
    const rebase = await rebaseWorktreeOntoBase({
      repoRoot: run.repoRoot,
      worktreePath: run.worktree.path,
      baseBranch: targetBranch,
    });
    if (!rebase.success) {
      run.status = "merge-conflict";
      run.merge = {
        ...run.merge,
        status: "conflict",
        conflictKind: "rebase",
        conflictedFiles: rebase.conflictedFiles,
        error: rebase.error,
      };
      await saveRunRecord(run);
      return run;
    }

    const fastForward = await runCommand("git", ["merge", "--ff-only", run.worktree.branch], {
      cwd: run.repoRoot,
      allowFailure: true,
    });
    if (fastForward.code === 0) {
      run.status = "merged";
      run.merge = {
        ...run.merge,
        status: "merged",
      };
      await saveRunRecord(run);
      return run;
    }

    const conflicted = await conflictedFiles(run.repoRoot);
    run.status = conflicted.length ? "merge-conflict" : "failed";
    run.merge = {
      ...run.merge,
      status: conflicted.length ? "conflict" : "failed",
      conflictKind: "merge",
      conflictedFiles: conflicted,
      error: gitError(fastForward),
    };
    await saveRunRecord(run);
    return run;
  }

  const merge = await runCommand("git", ["merge", "--no-ff", run.worktree.branch], {
    cwd: run.repoRoot,
    allowFailure: true,
  });
  if (merge.code === 0) {
    run.status = "merged";
    run.merge = {
      ...run.merge,
      status: "merged",
    };
    await saveRunRecord(run);
    return run;
  }

  const conflicted = await conflictedFiles(run.repoRoot);
  run.status = "merge-conflict";
  run.merge = {
    ...run.merge,
    status: "conflict",
    conflictKind: "merge",
    conflictedFiles: conflicted,
    error: gitError(merge),
  };
  await saveRunRecord(run);
  return run;
}

export async function syncRunWorktree(run: RunRecord, baseBranch: string): Promise<RunRecord> {
  if (!run.worktree.branch) {
    throw new Error("Run has no worktree branch to sync.");
  }
  const targetBranch = baseBranch.trim() === "HEAD" ? run.targetBranch : baseBranch;
  const previousStatus = run.status;
  const previousSync = run.sync;
  run.sync = {
    status: "not-started",
    attemptedAt: new Date().toISOString(),
    baseBranch: targetBranch,
  };
  await saveRunRecord(run);

  const commitFailure = await commitWorktreeChangesSafely(run, "sync");
  if (commitFailure) {
    return commitFailure;
  }

  const rebase = await rebaseWorktreeOntoBase({
    repoRoot: run.repoRoot,
    worktreePath: run.worktree.path,
    baseBranch: targetBranch,
  });
  if (rebase.success) {
    run.sync = {
      ...run.sync,
      status: "synced",
      conflictedFiles: [],
    };
    if (
      previousStatus === "merge-conflict" &&
      (run.merge?.conflictKind === "rebase" || previousSync?.status === "conflict")
    ) {
      run.status = "completed";
      if (run.merge?.conflictKind === "rebase") {
        run.merge = {
          ...run.merge,
          status: "not-started",
          conflictedFiles: undefined,
          error: undefined,
        };
      }
    } else {
      run.status = previousStatus;
    }
    await saveRunRecord(run);
    return run;
  }

  const conflict = rebase.conflictedFiles.length > 0;
  run.sync = {
    ...run.sync,
    status: conflict ? "conflict" : "failed",
    conflictedFiles: rebase.conflictedFiles,
    error: rebase.error,
  };
  if (conflict && (previousStatus === "completed" || previousStatus === "merge-conflict")) {
    run.status = "merge-conflict";
  } else {
    run.status = previousStatus;
  }
  await saveRunRecord(run);
  return run;
}

export async function rebaseWorktreeOntoBase(params: {
  repoRoot: string;
  worktreePath: string;
  baseBranch: string;
}): Promise<RebaseResult> {
  const baseRef = await resolveRebaseBaseRef(params.repoRoot, params.baseBranch);
  const rebase = await runCommand("git", ["rebase", baseRef], {
    cwd: params.worktreePath,
    allowFailure: true,
  });
  if (rebase.code === 0) {
    return {
      success: true,
      baseRef,
      conflictedFiles: [],
    };
  }
  return {
    success: false,
    baseRef,
    conflictedFiles: await conflictedFiles(params.worktreePath),
    error: gitError(rebase) || `Rebase onto ${baseRef} failed.`,
  };
}

export async function resolveRebaseBaseRef(repoRoot: string, baseBranch: string): Promise<string> {
  const branch = baseBranch.trim();
  if (!branch || branch === "HEAD") {
    return await currentCommit(repoRoot);
  }

  if (await refExists(repoRoot, branch)) {
    return branch;
  }

  const hasOrigin = (await runCommand("git", ["remote", "get-url", "origin"], {
    cwd: repoRoot,
    allowFailure: true,
  })).code === 0;
  let fetched = false;
  if (hasOrigin) {
    const fetch = await runCommand("git", ["fetch", "origin", branch], {
      cwd: repoRoot,
      allowFailure: true,
    });
    fetched = fetch.code === 0;
  }

  const remoteRef = `refs/remotes/origin/${branch}`;
  if (await refExists(repoRoot, remoteRef)) {
    return remoteRef;
  }
  if (fetched && await refExists(repoRoot, "FETCH_HEAD")) {
    return "FETCH_HEAD";
  }
  return await currentCommit(repoRoot);
}

export async function mergeJjRunIntoCurrentWorkspace(run: RunRecord, allowDirty = false): Promise<RunRecord> {
  if (!run.worktree.workspaceName) {
    throw new Error("Run has no jj workspace name to merge.");
  }
  ensureJjRepo(run.repoRoot);
  ensureJjRepo(run.worktree.path);
  if (!allowDirty && (await jjStatus(run.repoRoot)).length > 0) {
    throw new Error("Target workspace is dirty. Squash/abandon changes or pass --allow-dirty.");
  }
  run.merge = {
    status: "not-started",
    attemptedAt: new Date().toISOString(),
    targetBranch: await currentJjChangeId(run.repoRoot) || "@",
  };
  await saveRunRecord(run);

  const sourceChangeId = await currentJjChangeId(run.worktree.path);
  if (!sourceChangeId) {
    await markMergeFailed(run, "Could not determine jj change id for run workspace.");
    throw new Error("Could not determine jj change id for run workspace.");
  }
  const message = `rudder: ${run.task.slice(0, 72)}`;
  const merge = await runCommand("jj", ["new", "@", sourceChangeId, "-m", message], {
    cwd: run.repoRoot,
    allowFailure: true,
  });
  if (merge.code !== 0) {
    const error = merge.stderr.trim() || merge.stdout.trim() || "jj merge failed.";
    await markMergeFailed(run, error);
    throw new Error(error);
  }

  const conflicted = await jjConflictedFiles(run.repoRoot);
  if (conflicted.length === 0) {
    run.status = "merged";
    run.merge = {
      ...run.merge,
      status: "merged",
    };
    await saveRunRecord(run);
    return run;
  }

  run.status = "merge-conflict";
  run.merge = {
    ...run.merge,
    status: "conflict",
    conflictedFiles: conflicted,
    error: "jj merge created conflicts.",
  };
  await saveRunRecord(run);
  return run;
}

async function commitWorktreeChanges(run: RunRecord): Promise<void> {
  const unresolved = await conflictedFiles(run.worktree.path);
  if (unresolved.length > 0) {
    throw new Error(`Worktree has unresolved conflicts: ${unresolved.join(", ")}`);
  }
  if (await rebaseInProgress(run.worktree.path)) {
    throw new Error(`Worktree has an unfinished rebase. Resolve it in ${run.worktree.path}, run git rebase --continue, then retry.`);
  }
  if (!(await hasChanges(run.worktree.path))) {
    return;
  }
  await runCommand("git", ["add", "-A"], {
    cwd: run.worktree.path,
  });
  const message = `rudder: ${run.task.slice(0, 72)}`;
  const commit = await runCommand("git", ["commit", "-m", message], {
    cwd: run.worktree.path,
    allowFailure: true,
  });
  if (commit.code !== 0) {
    const stillChanged = await hasChanges(run.worktree.path);
    if (stillChanged) {
      throw new Error(commit.stderr.trim() || commit.stdout.trim() || "Failed to commit worktree changes.");
    }
  }
}

async function commitWorktreeChangesSafely(run: RunRecord, phase: "merge" | "sync"): Promise<RunRecord | null> {
  try {
    await commitWorktreeChanges(run);
    return null;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    run.status = "failed";
    if (phase === "merge") {
      run.merge = {
        ...run.merge,
        status: "failed",
        error: message,
      };
    } else {
      run.sync = {
        ...run.sync,
        status: "failed",
        error: message,
      };
    }
    await saveRunRecord(run);
    return run;
  }
}

async function currentTargetBranch(run: RunRecord): Promise<string> {
  const branch = await currentBranch(run.repoRoot);
  return branch === "HEAD" ? run.targetBranch : branch;
}

async function refExists(repoRoot: string, ref: string): Promise<boolean> {
  const result = await runCommand("git", ["rev-parse", "--verify", `${ref}^{commit}`], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.code === 0;
}

async function rebaseInProgress(worktree: string): Promise<boolean> {
  const gitPaths = await Promise.all([
    gitPath(worktree, "rebase-merge"),
    gitPath(worktree, "rebase-apply"),
  ]);
  for (const candidate of gitPaths) {
    if (!candidate) {
      continue;
    }
    try {
      await fsp.access(candidate);
      return true;
    } catch {
      // Continue checking the other rebase state path.
    }
  }
  return false;
}

async function gitPath(worktree: string, name: string): Promise<string | null> {
  const result = await runCommand("git", ["rev-parse", "--git-path", name], {
    cwd: worktree,
    allowFailure: true,
  });
  const value = result.stdout.trim();
  return result.code === 0 && value ? path.resolve(worktree, value) : null;
}

function gitError(result: { stdout: string; stderr: string }): string {
  return result.stderr.trim() || result.stdout.trim();
}

export async function conflictedFiles(repoRoot: string): Promise<string[]> {
  const result = await runCommand("git", ["diff", "--name-only", "--diff-filter=U"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

export async function jjConflictedFiles(workspacePath: string): Promise<string[]> {
  const result = await runCommand("jj", ["resolve", "--list"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  return result.code === 0 ? parseJjConflictedFiles(result.stdout) : [];
}

export function parseJjConflictedFiles(stdout: string): string[] {
  return stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => line.replace(/^[-*]\s+/, "").replace(/^conflict in\s+/i, "").trim())
    .filter((line) => line && !isRudderMetadataPath(line));
}

export async function worktreeList(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["worktree", "list"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim();
}

function ensureJjRepo(cwd: string): void {
  if (!commandExists("jj")) {
    throw new MissingToolError("jj");
  }
  const result = runCommandSync("jj", ["root"], {
    cwd,
    allowFailure: true,
  });
  if (result.code !== 0 || !result.stdout.trim()) {
    throw new Error(`Configured vcs is "jj", but ${cwd} is not inside a jj repository.`);
  }
}

function ensureJjRepoRoot(cwd: string): void {
  if (!commandExists("jj")) {
    throw new MissingToolError("jj");
  }
  const result = runCommandSync("jj", ["root"], {
    cwd,
    allowFailure: true,
  });
  if (result.code !== 0 || !result.stdout.trim() || !samePath(result.stdout.trim(), cwd)) {
    throw new Error(`Configured vcs is "jj", but ${cwd} is not inside a jj repository.`);
  }
}

function hasLocalJjMarker(cwd: string): boolean {
  const marker = path.join(path.resolve(cwd), ".jj");
  try {
    return fs.statSync(marker).isDirectory();
  } catch {
    return false;
  }
}

function findJjMarker(cwd: string): string | null {
  let current = path.resolve(cwd);
  while (true) {
    const marker = path.join(current, ".jj");
    if (pathExistsSync(marker)) {
      try {
        if (fs.statSync(marker).isDirectory()) {
          return marker;
        }
      } catch {
        // Keep walking if the marker cannot be inspected.
      }
    }
    const parent = path.dirname(current);
    if (parent === current) {
      return null;
    }
    current = parent;
  }
}

function samePath(left: string, right: string): boolean {
  const normalize = (value: string) => {
    const resolved = path.resolve(value);
    try {
      return fs.realpathSync(resolved);
    } catch {
      return resolved;
    }
  };
  return normalize(left) === normalize(right);
}

function isJjStatusMetadata(line: string): boolean {
  return (
    /^the working copy has no changes\.?$/i.test(line) ||
    /^the working copy is clean\.?$/i.test(line) ||
    /^no changes\.?$/i.test(line) ||
    /^working copy\s*:/i.test(line) ||
    /^parent commit\s*:/i.test(line) ||
    /^current operation\s*:/i.test(line) ||
    /^no conflicts found\.?$/i.test(line)
  );
}

function statusPathFromSummaryLine(line: string): string | null {
  const match = line.match(/^[A-Z?]{1,2}\s+(.+)$/);
  return match?.[1]?.trim() || null;
}

function isRudderMetadataPath(filePath: string): boolean {
  const normalized = filePath.replace(/\\/g, "/").replace(/^["']|["']$/g, "");
  return normalized === ".rudder" || normalized.startsWith(".rudder/");
}

async function markMergeFailed(run: RunRecord, error: string): Promise<void> {
  run.merge = {
    ...run.merge,
    status: "failed",
    error,
  };
  await saveRunRecord(run);
}
