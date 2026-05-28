import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  createRunJjWorkspace,
  detectVcsMode,
  mergeRunIntoCurrentBranch,
  parseJjConflictedFiles,
  parseJjStatus,
  removeJjWorkspace,
} from "../dist/git.js";
import { createRunRecord, loadRunRecord, saveRunRecord } from "../dist/state.js";
import { cleanupRuns, deleteRun } from "../dist/run-manager.js";
import { commandExists, pathExists } from "../dist/util.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");

test("parseJjStatus keeps real changes and ignores Rudder metadata", () => {
  const status = parseJjStatus([
    "Working copy changes:",
    "M src/git.ts",
    "A .rudder/runs/123/run.json",
    "Conflicts:",
    "src/conflicted.ts",
    "Working copy : abcdef (no description set)",
    "Parent commit: 123456 main",
    "",
  ].join("\n"));

  assert.deepEqual(status, ["M src/git.ts", "C src/conflicted.ts"]);
  assert.deepEqual(parseJjStatus("The working copy has no changes.\n"), []);
});

test("parseJjConflictedFiles strips common conflict prefixes", () => {
  assert.deepEqual(parseJjConflictedFiles("Conflict in src/a.ts\n* src/b.ts\n"), ["src/a.ts", "src/b.ts"]);
});

test("auto detection does not select jj from .jj alone when jj is unavailable", { skip: commandExists("jj") ? "system jj is installed" : false }, async (t) => {
  const temp = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-jj-missing-"));
  const oldPath = process.env.PATH;
  t.after(async () => {
    process.env.PATH = oldPath;
    await fsp.rm(temp, { recursive: true, force: true });
  });
  const bin = path.join(temp, "bin");
  const repo = path.join(temp, "repo");
  await fsp.mkdir(path.join(repo, ".jj"), { recursive: true });
  await fsp.mkdir(bin, { recursive: true });
  process.env.PATH = bin;

  assert.equal(detectVcsMode(repo), "git");
  assert.throws(
    () => detectVcsMode(repo, "jj"),
    (error) => error instanceof Error && error.name === "MissingToolError" && /jj is not installed/.test(error.message),
  );
});

test("detectVcsMode respects auto detection and explicit overrides", async (t) => {
  const env = await setupFakeJj(t);

  assert.equal(detectVcsMode(env.repo), "jj");
  assert.equal(detectVcsMode(env.repo, "git"), "git");

  process.env.JJ_ROOT_FAIL = "1";
  assert.throws(() => detectVcsMode(env.repo, "jj"), /not inside a jj repository/);
});

test("auto detection ignores an enclosing jj repo for a nested git repo", async (t) => {
  const env = await setupFakeJj(t);
  const nestedGit = path.join(env.repo, "nested-git");
  await fsp.mkdir(path.join(nestedGit, ".git"), { recursive: true });

  assert.equal(detectVcsMode(nestedGit), "git");
  assert.throws(() => detectVcsMode(nestedGit, "jj"), /not inside a jj repository/);
});

test("createRunJjWorkspace and removeJjWorkspace use jj workspace commands", async (t) => {
  const env = await setupFakeJj(t);

  const workspace = await createRunJjWorkspace({
    repoRoot: env.repo,
    runId: "20260102030405-repeatable",
    task: "fix tests",
  });

  assert.match(workspace.workspaceName, /^rudder-20260102030405-[a-f0-9]{6}$/);
  assert.equal(await pathExists(workspace.path), true);

  await removeJjWorkspace({
    repoRoot: env.repo,
    workspaceName: workspace.workspaceName,
    workspacePath: workspace.path,
  });

  assert.equal(await pathExists(workspace.path), false);
  const log = await readLog(env.log);
  assert.match(log, /workspace add .* --name rudder-20260102030405-/);
  assert.match(log, /workspace forget rudder-20260102030405-/);
});

test("jj merge creates a merge change from current workspace and run change", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "run-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  process.env.JJ_SOURCE_WORKSPACE = workspace;
  process.env.JJ_SOURCE_CHANGE = "sourcechange";
  process.env.JJ_TARGET_CHANGE = "targetchange";
  process.env.JJ_STATUS_OUTPUT = "The working copy has no changes.\n";
  process.env.JJ_RESOLVE_LIST = "";

  const run = await createRunRecord({
    id: "run-merge",
    repoRoot: env.repo,
    task: "merge jj work",
    backend: "claude",
    targetBranch: "targetchange",
    baseCommit: "basechange",
    vcs: "jj",
    useWorktree: true,
    worktreePath: workspace,
    worktreeWorkspaceName: "rudder-run-merge",
  });

  const merged = await mergeRunIntoCurrentBranch(run);

  assert.equal(merged.status, "merged");
  assert.equal(merged.merge?.status, "merged");
  assert.match(await readLog(env.log), /\|new @ sourcechange -m rudder: merge jj work/);
});

test("deleteRun --mergeFirst leaves a clear failed merge record instead of deleting on jj merge failure", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "delete-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  process.env.JJ_SOURCE_WORKSPACE = workspace;
  process.env.JJ_SOURCE_CHANGE = "sourcechange";
  process.env.JJ_STATUS_OUTPUT = "The working copy has no changes.\n";
  process.env.JJ_NEW_FAIL = "1";

  await createRunRecord({
    id: "run-delete",
    repoRoot: env.repo,
    task: "delete after merge",
    backend: "claude",
    targetBranch: "targetchange",
    baseCommit: "basechange",
    vcs: "jj",
    useWorktree: true,
    worktreePath: workspace,
    worktreeWorkspaceName: "rudder-run-delete",
  });

  await withCwd(env.repo, async () => {
    await assert.rejects(
      deleteRun("run-delete", { mergeFirst: true, silent: true }),
      /Merge failed for run-delete; run was not deleted\. merge exploded/,
    );
  });

  const latest = await loadRunRecord(env.repo, "run-delete");
  assert.equal(latest?.merge?.status, "failed");
  assert.match(latest?.merge?.error ?? "", /merge exploded/);
  assert.equal(await pathExists(workspace), true);
});

test("cleanup removes completed jj workspaces without requiring merged status", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "cleanup-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  const run = await createRunRecord({
    id: "run-cleanup",
    repoRoot: env.repo,
    task: "cleanup jj",
    backend: "claude",
    targetBranch: "targetchange",
    baseCommit: "basechange",
    vcs: "jj",
    useWorktree: true,
    worktreePath: workspace,
    worktreeWorkspaceName: "rudder-run-cleanup",
  });
  run.status = "completed";
  await saveRunRecord(run);

  await withSilencedConsole(async () => {
    await withCwd(env.repo, async () => {
      await cleanupRuns(false);
    });
  });

  assert.equal(await pathExists(workspace), false);
  assert.match(await readLog(env.log), /workspace forget rudder-run-cleanup/);
});

async function setupFakeJj(t) {
  const temp = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-jj-"));
  const bin = path.join(temp, "bin");
  const repo = path.join(temp, "repo");
  const log = path.join(temp, "jj.log");
  await fsp.mkdir(bin, { recursive: true });
  await fsp.mkdir(path.join(repo, ".jj"), { recursive: true });
  await fsp.writeFile(path.join(bin, "jj"), fakeJjScript(), { mode: 0o755 });

  const oldEnv = {
    PATH: process.env.PATH,
    RUDDER_HOME: process.env.RUDDER_HOME,
    JJ_ROOT: process.env.JJ_ROOT,
    JJ_ROOT_FAIL: process.env.JJ_ROOT_FAIL,
    JJ_LOG: process.env.JJ_LOG,
    JJ_SOURCE_WORKSPACE: process.env.JJ_SOURCE_WORKSPACE,
    JJ_SOURCE_CHANGE: process.env.JJ_SOURCE_CHANGE,
    JJ_TARGET_CHANGE: process.env.JJ_TARGET_CHANGE,
    JJ_STATUS_OUTPUT: process.env.JJ_STATUS_OUTPUT,
    JJ_DIFF_OUTPUT: process.env.JJ_DIFF_OUTPUT,
    JJ_RESOLVE_LIST: process.env.JJ_RESOLVE_LIST,
    JJ_NEW_FAIL: process.env.JJ_NEW_FAIL,
  };

  process.env.PATH = `${bin}:${process.env.PATH ?? ""}`;
  process.env.RUDDER_HOME = path.join(temp, "home");
  process.env.JJ_ROOT = repo;
  process.env.JJ_LOG = log;
  delete process.env.JJ_ROOT_FAIL;
  delete process.env.JJ_SOURCE_WORKSPACE;
  delete process.env.JJ_SOURCE_CHANGE;
  delete process.env.JJ_TARGET_CHANGE;
  delete process.env.JJ_STATUS_OUTPUT;
  delete process.env.JJ_DIFF_OUTPUT;
  delete process.env.JJ_RESOLVE_LIST;
  delete process.env.JJ_NEW_FAIL;

  t.after(async () => {
    for (const [key, value] of Object.entries(oldEnv)) {
      if (value === undefined) {
        delete process.env[key];
      } else {
        process.env[key] = value;
      }
    }
    process.chdir(repoRoot);
    await fsp.rm(temp, { recursive: true, force: true });
  });

  return { temp, repo, log };
}

function fakeJjScript() {
  return `#!/bin/sh
set -eu
if [ -n "\${JJ_LOG:-}" ]; then
  printf '%s|%s\\n' "$(pwd)" "$*" >> "\${JJ_LOG}"
fi

case "\${1:-}" in
  --version)
    echo "jj 0.31.0-test"
    exit 0
    ;;
  root)
    if [ "\${JJ_ROOT_FAIL:-0}" = "1" ]; then
      echo "not a jj repo" >&2
      exit 1
    fi
    echo "\${JJ_ROOT:-$(pwd)}"
    exit 0
    ;;
  workspace)
    case "\${2:-}" in
      add)
        mkdir -p "\${3:?missing destination}/.jj"
        exit 0
        ;;
      forget)
        exit 0
        ;;
    esac
    ;;
  status)
    if [ "\${JJ_STATUS_OUTPUT+x}" = "x" ]; then
      printf '%b' "\${JJ_STATUS_OUTPUT}"
    else
      echo "The working copy has no changes."
    fi
    exit 0
    ;;
  diff)
    if [ "\${JJ_DIFF_OUTPUT+x}" = "x" ]; then
      printf '%b' "\${JJ_DIFF_OUTPUT}"
    fi
    exit 0
    ;;
  log)
    if [ -n "\${JJ_SOURCE_WORKSPACE:-}" ]; then
      current_pwd=$(pwd -P)
      source_pwd=$(cd "\${JJ_SOURCE_WORKSPACE}" && pwd -P)
    else
      current_pwd=""
      source_pwd="__unset__"
    fi
    if [ -n "\${JJ_SOURCE_WORKSPACE:-}" ] && [ "\${current_pwd}" = "\${source_pwd}" ]; then
      echo "\${JJ_SOURCE_CHANGE:-sourcechange}"
    else
      echo "\${JJ_TARGET_CHANGE:-targetchange}"
    fi
    exit 0
    ;;
  new)
    if [ "\${JJ_NEW_FAIL:-0}" = "1" ]; then
      echo "merge exploded" >&2
      exit 42
    fi
    exit 0
    ;;
  resolve)
    if [ "\${2:-}" = "--list" ]; then
      if [ "\${JJ_RESOLVE_LIST+x}" = "x" ]; then
        printf '%b' "\${JJ_RESOLVE_LIST}"
      fi
      exit 0
    fi
    ;;
esac

echo "unexpected jj $*" >&2
exit 1
`;
}

async function readLog(log) {
  return await fsp.readFile(log, "utf8").catch(() => "");
}

async function withCwd(cwd, fn) {
  const previous = process.cwd();
  process.chdir(cwd);
  try {
    return await fn();
  } finally {
    process.chdir(previous);
  }
}

async function withSilencedConsole(fn) {
  const original = console.log;
  console.log = () => undefined;
  try {
    return await fn();
  } finally {
    console.log = original;
  }
}
