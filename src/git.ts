import fsp from "node:fs/promises";
import path from "node:path";
import type { MergeStrategy, RunRecord } from "./types.js";
import { runCommand, runCommandSync } from "./util.js";
import { listRuns, saveRunRecord } from "./state.js";
import { jjDiff, jjStatus } from "./jj.js";

export type RebaseResult = {
  success: boolean;
  baseRef: string;
  conflictedFiles: string[];
  error?: string;
};

// ---------------------------------------------------------------------------
// Externally-git contract + plumbing.
//
// Rudder's internal run isolation/merge runs on jj (see src/jj.ts). git.ts now
// only owns the surface where Rudder still talks plain git: discovering the repo
// root, reporting branch/commit, and producing status/diff for the Hunk review
// surface. Legacy runs created before the jj switch (run.vcs === "git") still
// merge/sync through the git helpers below, guarded by the vcs mode.
// ---------------------------------------------------------------------------

export function findRepoRoot(cwd = process.cwd()): string {
  const result = runCommandSync("git", ["rev-parse", "--show-toplevel"], {
    cwd,
    allowFailure: true,
  });
  if (result.code === 0 && result.stdout.trim()) {
    return result.stdout.trim();
  }
  return path.resolve(cwd);
}

export function isGitRepo(cwd: string): boolean {
  return (
    runCommandSync("git", ["rev-parse", "--is-inside-work-tree"], {
      cwd,
      allowFailure: true,
    }).stdout.trim() === "true"
  );
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

// VCS-aware projections used by brain.ts and the worker. jj runs read jj
// status/diff; legacy git runs read git status/diff.
export async function workspaceStatus(run: RunRecord): Promise<string[]> {
  return (run.vcs ?? "git") === "jj"
    ? await jjStatus(run.worktree.path)
    : await gitStatus(run.worktree.path);
}

export async function workspaceDiff(run: RunRecord): Promise<string> {
  return (run.vcs ?? "git") === "jj"
    ? await jjDiff(run.worktree.path)
    : await gitDiff(run.worktree.path);
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

export async function worktreeList(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["worktree", "list"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim();
}

export async function removeGitWorktree(repoRoot: string, targetPath: string, force = false): Promise<void> {
  await runCommand("git", ["worktree", "remove", ...(force ? ["--force"] : []), targetPath], {
    cwd: repoRoot,
    allowFailure: force,
  });
}

// ---------------------------------------------------------------------------
// Legacy git run isolation/merge/sync.
//
// These remain only to keep runs created before the jj switch
// (run.vcs === "git") mergeable. No NEW runs reach this path; run-manager routes
// jj runs to src/jj.ts and only falls back here when run.vcs === "git".
// ---------------------------------------------------------------------------

export async function mergeGitRunIntoCurrentBranch(
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

export async function syncGitRunWorktree(run: RunRecord, baseBranch: string): Promise<RunRecord> {
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
