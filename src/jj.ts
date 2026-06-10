import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import type { RunRecord } from "./types.js";
import {
  commandExists,
  MissingToolError,
  pathExistsSync,
  runCommand,
  runCommandSync,
  shortHash,
} from "./util.js";
import { saveRunRecord, worktreePath } from "./state.js";

// Minimum jj version we are confident in. Older releases may lack flags we use
// (e.g. `jj workspace add -r`); we warn rather than throw so an older jj still
// works through the documented fallbacks.
const MIN_JJ = "0.21.0";

export type MergeNodeResult = {
  mergeChangeId: string;
  opId: string;
  conflictedFiles: string[];
};

/**
 * Hard requirement: jj must be installed. Throws MissingToolError otherwise.
 */
export function ensureJj(): void {
  if (!commandExists("jj")) {
    throw new MissingToolError("jj");
  }
}

/**
 * Parse `jj --version`. Returns the version string (e.g. "0.40.0") or "".
 * Warns (does not throw) when below MIN_JJ.
 */
export async function jjVersion(): Promise<string> {
  const result = await runCommand("jj", ["--version"], { allowFailure: true });
  const match = result.stdout.match(/(\d+)\.(\d+)\.(\d+)/);
  const version = match ? match[0] : "";
  if (version && compareVersions(version, MIN_JJ) < 0) {
    console.warn(
      `[rudder] jj ${version} is older than the recommended ${MIN_JJ}; some workspace flags may fall back.`,
    );
  }
  return version;
}

/**
 * Idempotently colocate a jj repo with the git repo at repoRoot. If already a
 * jj repo, no-op. Otherwise runs `jj git init --colocate`.
 */
export async function ensureColocated(repoRoot: string): Promise<void> {
  ensureJj();
  if (isJjRepo(repoRoot)) {
    return;
  }
  await runCommand("jj", ["git", "init", "--colocate"], { cwd: repoRoot });
}

export function isJjRepo(cwd: string): boolean {
  if (!commandExists("jj")) {
    return false;
  }
  const result = runCommandSync("jj", ["root"], { cwd, allowFailure: true });
  if (result.code === 0 && result.stdout.trim()) {
    return samePath(result.stdout.trim(), cwd);
  }
  return hasLocalJjMarker(cwd);
}

export function findJjRoot(cwd: string): string {
  if (commandExists("jj")) {
    const result = runCommandSync("jj", ["root"], { cwd, allowFailure: true });
    if (result.code === 0 && result.stdout.trim()) {
      return result.stdout.trim();
    }
  }
  const marker = findJjMarker(cwd);
  return marker ? path.dirname(marker) : path.resolve(cwd);
}

/**
 * jj refuses to operate on a STALE workspace — one whose working-copy commit was
 * moved by a concurrent operation in another workspace (every command in any
 * workspace advances the shared op log). The command exits non-zero with a
 * "working copy is stale" message and a hint to run `jj workspace update-stale`.
 * In a multi-worker run this happens routinely, so any jj read against a worker
 * workspace must be able to recover instead of falsely reporting "no changes"
 * (which marks a good run failed) or failing a merge.
 */
function isStaleWorkingCopy(text: string): boolean {
  return /working copy is stale|stale working copy/i.test(text);
}

/**
 * Re-point a stale workspace at its recorded working-copy commit. jj snapshots
 * the on-disk working copy as part of this, so a worker's changes are preserved.
 * Returns true when the workspace is no longer stale afterwards.
 */
async function recoverStaleWorkspace(workspacePath: string): Promise<boolean> {
  const result = await runCommand("jj", ["workspace", "update-stale"], {
    cwd: workspacePath,
    allowFailure: true,
  });
  return result.code === 0;
}

/**
 * Run a read-only jj command in a workspace, transparently recovering once from a
 * stale working copy. This keeps the verifier's change-detection and the merge's
 * change-id reads honest while workers run concurrently in sibling workspaces.
 */
async function jjReadInWorkspace(
  workspacePath: string,
  args: string[],
): Promise<{ stdout: string; stderr: string; code: number }> {
  let result = await runCommand("jj", args, { cwd: workspacePath, allowFailure: true });
  if (result.code !== 0 && isStaleWorkingCopy(result.stderr)) {
    await recoverStaleWorkspace(workspacePath);
    result = await runCommand("jj", args, { cwd: workspacePath, allowFailure: true });
  }
  return result;
}

/**
 * The change id of `@` (the working-copy commit) in the given workspace.
 */
export async function currentJjChangeId(workspacePath: string): Promise<string> {
  const result = await jjReadInWorkspace(workspacePath, ["log", "--no-graph", "-r", "@", "-T", "change_id"]);
  return result.code === 0 ? result.stdout.trim().split(/\s+/)[0] ?? "" : "";
}

export async function jjStatus(workspacePath: string): Promise<string[]> {
  const result = await jjReadInWorkspace(workspacePath, ["status"]);
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
  // A sibling workspace can make this one stale mid-verification; an unrecovered
  // read returns an empty diff and the verifier flags a healthy run.
  const stat = await jjReadInWorkspace(workspacePath, ["diff", "--stat"]);
  const patch = await jjReadInWorkspace(workspacePath, ["diff", "--git"]);
  return [
    status.length ? `status:\n${status.join("\n")}` : "",
    stat.stdout.trim(),
    patch.stdout.trim(),
  ]
    .filter(Boolean)
    .join("\n\n");
}

/**
 * Create a new empty change parented on the given changes. Reads the new change
 * id back by querying a unique marker embedded in the description, never by
 * scraping stdout. Returns the new change id.
 */
export async function createEmptyChange(params: {
  repoRoot: string;
  parents: string[];
  description: string;
}): Promise<string> {
  ensureJj();
  const marker = `rudder-node:${shortHash(`${params.description}:${Date.now()}:${Math.random()}`)}`;
  const description = `${marker} ${params.description}`.trim();
  const args = ["new", ...params.parents.filter(Boolean), "-m", description, "--no-edit"];
  await runCommand("jj", args, { cwd: params.repoRoot });
  // jj normalizes descriptions to end with a trailing newline, and
  // `description(exact:...)` matches the full stored description, so the revset
  // literal must include the trailing "\n".
  const exact = JSON.stringify(`${description}\n`);
  const result = await runCommand(
    "jj",
    ["log", "--no-graph", "-r", `description(exact:${exact})`, "-T", "change_id"],
    { cwd: params.repoRoot, allowFailure: true },
  );
  let changeId = result.stdout.trim().split(/\s+/)[0] ?? "";
  if (!changeId) {
    // Fallback: match on the unique marker as a substring.
    const bySubstring = await runCommand(
      "jj",
      ["log", "--no-graph", "-r", `description(substring:${JSON.stringify(marker)})`, "-T", "change_id"],
      { cwd: params.repoRoot, allowFailure: true },
    );
    changeId = bySubstring.stdout.trim().split(/\s+/)[0] ?? "";
  }
  if (!changeId) {
    throw new Error(`Could not read back the change id for the new empty change (${marker}).`);
  }
  return changeId;
}

export async function describeChange(repoRoot: string, changeId: string, message: string): Promise<void> {
  await runCommand("jj", ["describe", changeId, "-m", message], { cwd: repoRoot });
}

/**
 * Create a jj workspace for a node/run. Workspace name matches the original
 * rudder regex `rudder-<id.slice(0,14)>-<shortHash(id).slice(0,6)>`. If
 * `atChangeId` is given, parent the working-copy commit on it; falls back to
 * `jj workspace add` + `jj edit <atChangeId>` if `-r` is unsupported.
 */
export async function createNodeWorkspace(params: {
  repoRoot: string;
  nodeId?: string;
  runId?: string;
  task?: string;
  atChangeId?: string;
}): Promise<{ workspaceName: string; path: string }> {
  ensureJj();
  const id = params.nodeId ?? params.runId;
  if (!id) {
    throw new Error("createNodeWorkspace requires nodeId or runId.");
  }
  const workspaceName = workspaceNameFor(id);
  const targetPath = worktreePath(params.repoRoot, params.runId ?? id, params.task);
  await fsp.mkdir(path.dirname(targetPath), { recursive: true });

  const baseArgs = ["workspace", "add", targetPath, "--name", workspaceName];
  if (params.atChangeId) {
    const withRevision = await runCommand("jj", [...baseArgs, "-r", params.atChangeId], {
      cwd: params.repoRoot,
      allowFailure: true,
    });
    if (withRevision.code === 0) {
      return { workspaceName, path: targetPath };
    }
    // Older jj without `workspace add -r`: add, then edit onto the change.
    await runCommand("jj", baseArgs, { cwd: params.repoRoot });
    await runCommand("jj", ["edit", params.atChangeId], { cwd: targetPath, allowFailure: true });
    return { workspaceName, path: targetPath };
  }

  await runCommand("jj", baseArgs, { cwd: params.repoRoot });
  return { workspaceName, path: targetPath };
}

/**
 * Phase-1 entry point used by run-manager. Delegates to createNodeWorkspace.
 */
export async function createRunJjWorkspace(params: {
  repoRoot: string;
  runId: string;
  task: string;
}): Promise<{ workspaceName: string; path: string }> {
  return await createNodeWorkspace({
    repoRoot: params.repoRoot,
    runId: params.runId,
    task: params.task,
  });
}

function workspaceNameFor(id: string): string {
  return `rudder-${id.slice(0, 14)}-${shortHash(id).slice(0, 6)}`;
}

/**
 * Merge a run's jj change into the current (main) workspace `@`. Captures the
 * operation id before the merge and records it on merge.operationId. On
 * conflict sets merge.status="conflict" + conflictedFiles + mergeChangeId.
 */
export async function mergeJjRunIntoCurrentWorkspace(run: RunRecord, allowDirty = false): Promise<RunRecord> {
  if (!run.worktree.workspaceName) {
    throw new Error("Run has no jj workspace name to merge.");
  }
  ensureJjRepo(run.repoRoot);
  ensureJjRepo(run.worktree.path);
  // Rudder writes its own files (.rudder/ is already filtered by jjStatus; RUDDER.md
  // and DECISIONS.md are the live coordination surfaces) into the main workspace
  // continuously, so they must not count as "dirty" and block a merge.
  if (!allowDirty) {
    const dirty = (await jjStatus(run.repoRoot)).filter((line) => !isMergeIgnorablePath(line));
    if (dirty.length > 0) {
      throw new Error("Target workspace is dirty. Squash/abandon changes or pass --allow-dirty.");
    }
  }
  // --allow-dirty (which the TUI always passes) skips the dirty gate above, but a
  // merge parented on a still-conflicted @ (e.g. the user declined the resolver
  // after a previous conflicted merge) nests conflicts, so this guard is
  // unconditional.
  const preexistingConflicts = await jjConflictedFiles(run.repoRoot);
  if (preexistingConflicts.length > 0) {
    const error = `integration workspace already has unresolved conflicts (${preexistingConflicts.join(", ")}); resolve them before merging another run`;
    await markMergeFailed(run, error);
    throw new Error(error);
  }
  const opIdBefore = await currentOpId(run.repoRoot);
  run.merge = {
    status: "not-started",
    attemptedAt: new Date().toISOString(),
    targetBranch: (await currentJjChangeId(run.repoRoot)) || "@",
    operationId: opIdBefore || undefined,
  };
  await saveRunRecord(run);

  const sourceChangeId = await currentJjChangeId(run.worktree.path);
  if (!sourceChangeId) {
    await markMergeFailed(run, "Could not determine jj change id for run workspace.");
    throw new Error("Could not determine jj change id for run workspace.");
  }
  const message = `rudder: ${run.task.slice(0, 72)}`;
  const merge = await mergeNode({
    repoRoot: run.repoRoot,
    nodeChangeId: sourceChangeId,
    intoChangeId: "@",
    message,
  });
  if (!merge.mergeChangeId) {
    const error = "jj merge failed.";
    await markMergeFailed(run, error);
    throw new Error(error);
  }

  if (merge.conflictedFiles.length === 0) {
    run.status = "merged";
    run.merge = {
      ...run.merge,
      status: "merged",
      mergeChangeId: merge.mergeChangeId,
      operationId: merge.opId || run.merge?.operationId,
    };
    await saveRunRecord(run);
    return run;
  }

  run.status = "merge-conflict";
  run.merge = {
    ...run.merge,
    status: "conflict",
    conflictedFiles: merge.conflictedFiles,
    mergeChangeId: merge.mergeChangeId,
    operationId: merge.opId || run.merge?.operationId,
    error: "jj merge created conflicts.",
  };
  await saveRunRecord(run);
  return run;
}

/**
 * Merge two changes into a new change. Captures the op id (after the merge),
 * returns the new merge change id and any conflicted files. jj merges never
 * fail on conflict; the conflict is recorded in the merge change.
 */
export async function mergeNode(params: {
  repoRoot: string;
  nodeChangeId: string;
  intoChangeId: string;
  message: string;
}): Promise<MergeNodeResult> {
  ensureJjRepo(params.repoRoot);
  const merge = await runCommand(
    "jj",
    ["new", params.intoChangeId, params.nodeChangeId, "-m", params.message],
    { cwd: params.repoRoot, allowFailure: true },
  );
  if (merge.code !== 0) {
    throw new Error(merge.stderr.trim() || merge.stdout.trim() || "jj merge failed.");
  }
  const mergeChangeId = await currentJjChangeId(params.repoRoot);
  const opId = await currentOpId(params.repoRoot);
  const conflictedFiles = await jjConflictedFiles(params.repoRoot);
  return { mergeChangeId, opId, conflictedFiles };
}

export async function jjConflictedFiles(workspacePath: string): Promise<string[]> {
  // Stale-recovering read: a stale failure here would falsely report "no
  // conflicts" to the merge/sync paths.
  const result = await jjReadInWorkspace(workspacePath, ["resolve", "--list"]);
  return result.code === 0 ? parseJjConflictedFiles(result.stdout) : [];
}

/**
 * Alias matching the plan's API name.
 */
export async function listConflicts(workspacePath: string): Promise<string[]> {
  return await jjConflictedFiles(workspacePath);
}

export function parseJjConflictedFiles(stdout: string): string[] {
  return stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => line.replace(/^[-*]\s+/, "").replace(/^conflict in\s+/i, "").trim())
    .filter((line) => line && !isRudderMetadataPath(line));
}

/**
 * The id of the latest operation in the op log. Recovers once from a stale
 * workspace; if the id still cannot be read, warns and returns "" instead of
 * throwing — merges must proceed, they just lose their undo waypoint.
 */
export async function currentOpId(repoRoot: string): Promise<string> {
  const result = await jjReadInWorkspace(repoRoot, ["op", "log", "--no-graph", "-n", "1", "-T", "id.short()"]);
  const opId = result.code === 0 ? result.stdout.trim().split(/\s+/)[0] ?? "" : "";
  if (!opId) {
    console.warn("rudder: could not capture jj op id; this merge will not be undoable via rudder undo");
  }
  return opId;
}

/**
 * Restore the repo to an earlier operation. One call rewinds all workspaces and
 * refs atomically (op restore is global).
 */
export async function undoToOp(repoRoot: string, opId: string): Promise<void> {
  await runCommand("jj", ["op", "restore", opId], { cwd: repoRoot });
}

export async function undoLast(repoRoot: string): Promise<void> {
  await runCommand("jj", ["undo"], { cwd: repoRoot });
}

/**
 * Export jj changes to the colocated git repo. Call after each merge, before
 * opening a diff review, and before a PR.
 */
export async function exportToGit(repoRoot: string): Promise<void> {
  await runCommand("jj", ["git", "export"], { cwd: repoRoot, allowFailure: true });
}

export async function createBookmark(repoRoot: string, name: string, atChangeId: string): Promise<void> {
  const create = await runCommand("jj", ["bookmark", "create", name, "-r", atChangeId], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (create.code !== 0) {
    // Bookmark may already exist; move it to the target change.
    await runCommand("jj", ["bookmark", "set", name, "-r", atChangeId, "--allow-backwards"], {
      cwd: repoRoot,
    });
  }
}

export async function pushBookmark(repoRoot: string, name: string): Promise<void> {
  const push = await runCommand("jj", ["git", "push", "--bookmark", name, "--allow-new"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (push.code !== 0) {
    // jj 0.40 drops `--allow-new`; a plain `--bookmark` push handles new bookmarks.
    await runCommand("jj", ["git", "push", "--bookmark", name], { cwd: repoRoot });
  }
}

/**
 * Forget a jj workspace and remove its directory.
 */
export async function forgetWorkspace(params: {
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

export async function removeRunWorkspace(run: RunRecord): Promise<void> {
  if (!run.worktree.enabled) {
    return;
  }
  if (!run.worktree.workspaceName) {
    throw new Error("Run has no jj workspace name to remove.");
  }
  await forgetWorkspace({
    repoRoot: run.repoRoot,
    workspaceName: run.worktree.workspaceName,
    workspacePath: run.worktree.path,
  });
}

/**
 * Rebase a run's node change onto the trunk. jj records conflicts in place and
 * never blocks, so there is no rebase-in-progress dance.
 */
export async function syncRunWorkspace(run: RunRecord, baseBranch: string): Promise<RunRecord> {
  if (!run.worktree.workspaceName) {
    throw new Error("Run has no jj workspace to sync.");
  }
  ensureJjRepo(run.repoRoot);
  const nodeChange = run.worktree.jjChangeId || (await currentJjChangeId(run.worktree.path));
  if (!nodeChange) {
    run.sync = {
      ...run.sync,
      status: "failed",
      attemptedAt: new Date().toISOString(),
      baseBranch,
      error: "Could not determine jj change id for run workspace.",
    };
    await saveRunRecord(run);
    return run;
  }
  const trunk = baseBranch && baseBranch !== "HEAD" ? baseBranch : run.targetBranch || "@";
  run.sync = {
    status: "not-started",
    attemptedAt: new Date().toISOString(),
    baseBranch: trunk,
  };
  await saveRunRecord(run);

  const rebase = await runCommand("jj", ["rebase", "-s", nodeChange, "-d", trunk], {
    cwd: run.repoRoot,
    allowFailure: true,
  });
  if (rebase.code !== 0) {
    // jj 0.40 replaced `-d`/`--destination` with `-o`/`--onto`.
    const onto = await runCommand("jj", ["rebase", "-s", nodeChange, "--onto", trunk], {
      cwd: run.repoRoot,
      allowFailure: true,
    });
    if (onto.code !== 0) {
      run.sync = {
        ...run.sync,
        status: "failed",
        error: onto.stderr.trim() || onto.stdout.trim() || `Rebase onto ${trunk} failed.`,
      };
      await saveRunRecord(run);
      return run;
    }
  }

  const conflicted = await jjConflictedFiles(run.worktree.path);
  run.sync = {
    ...run.sync,
    status: conflicted.length ? "conflict" : "synced",
    conflictedFiles: conflicted,
  };
  await saveRunRecord(run);
  return run;
}

function ensureJjRepo(cwd: string): void {
  if (!commandExists("jj")) {
    throw new MissingToolError("jj");
  }
  const result = runCommandSync("jj", ["root"], { cwd, allowFailure: true });
  if (result.code !== 0 || !result.stdout.trim()) {
    throw new Error(`Expected ${cwd} to be inside a jj repository.`);
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

/// A jj-status line that names a Rudder-managed file (a status path or a `C path`
/// conflict line), which should never count toward the target-dirty merge gate.
function isMergeIgnorablePath(line: string): boolean {
  const path = line.replace(/^[ACMDR?]\s+/, "").replace(/^C\s+/, "").trim();
  return (
    isRudderMetadataPath(path) ||
    path === "RUDDER.md" ||
    path === "DECISIONS.md" ||
    path.endsWith("/RUDDER.md") ||
    path.endsWith("/DECISIONS.md")
  );
}

async function markMergeFailed(run: RunRecord, error: string): Promise<void> {
  run.merge = {
    ...run.merge,
    status: "failed",
    error,
  };
  await saveRunRecord(run);
}

function compareVersions(a: string, b: string): number {
  const pa = a.split(".").map((n) => Number.parseInt(n, 10) || 0);
  const pb = b.split(".").map((n) => Number.parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
    const diff = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (diff !== 0) {
      return diff < 0 ? -1 : 1;
    }
  }
  return 0;
}
