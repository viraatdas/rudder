import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  createNodeWorkspace,
  createRunJjWorkspace,
  currentJjChangeId,
  currentOpId,
  forgetWorkspace,
  isJjRepo,
  jjDiff,
  mergeJjRunIntoCurrentWorkspace,
  parseJjConflictedFiles,
  parseJjStatus,
} from "../dist/jj.js";
import { createRunRecord } from "../dist/state.js";
import { pathExists } from "../dist/util.js";

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

test("createRunJjWorkspace names the workspace with the rudder prefix", async (t) => {
  const env = await setupFakeJj(t);

  const workspace = await createRunJjWorkspace({
    repoRoot: env.repo,
    runId: "20260102030405-repeatable",
    task: "fix tests",
  });

  assert.match(workspace.workspaceName, /^rudder-20260102030405-[a-f0-9]{6}$/);
  assert.equal(await pathExists(workspace.path), true);

  await forgetWorkspace({
    repoRoot: env.repo,
    workspaceName: workspace.workspaceName,
    workspacePath: workspace.path,
  });

  assert.equal(await pathExists(workspace.path), false);
  const log = await readLog(env.log);
  assert.match(log, /workspace add .* --name rudder-20260102030405-/);
  assert.match(log, /workspace forget rudder-20260102030405-/);
});

test("createNodeWorkspace parents the workspace on a change id when given", async (t) => {
  const env = await setupFakeJj(t);

  const workspace = await createNodeWorkspace({
    repoRoot: env.repo,
    nodeId: "node-abc",
    atChangeId: "basechange",
  });

  assert.match(workspace.workspaceName, /^rudder-node-abc-[a-f0-9]{6}$/);
  assert.match(await readLog(env.log), /workspace add .* --name rudder-node-abc-[a-f0-9]{6} -r basechange/);
});

test("currentJjChangeId reads the @ change id", async (t) => {
  const env = await setupFakeJj(t);
  process.env.JJ_TARGET_CHANGE = "targetchange";

  assert.equal(await currentJjChangeId(env.repo), "targetchange");
});

test("isJjRepo detects the fake jj repo", async (t) => {
  const env = await setupFakeJj(t);
  assert.equal(isJjRepo(env.repo), true);
});

test("currentOpId reads the latest operation id", async (t) => {
  const env = await setupFakeJj(t);
  process.env.JJ_OP_ID = "op12345";

  assert.equal(await currentOpId(env.repo), "op12345");
});

test("jj merge captures merge.operationId and merges cleanly", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "run-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  process.env.JJ_SOURCE_WORKSPACE = workspace;
  process.env.JJ_SOURCE_CHANGE = "sourcechange";
  process.env.JJ_TARGET_CHANGE = "targetchange";
  process.env.JJ_OP_ID = "opmerge99";
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

  const merged = await mergeJjRunIntoCurrentWorkspace(run);

  assert.equal(merged.status, "merged");
  assert.equal(merged.merge?.status, "merged");
  assert.equal(merged.merge?.operationId, "opmerge99");
  assert.match(await readLog(env.log), /\|new @ sourcechange -m rudder: merge jj work/);
});

test("jj merge records conflicted files and mergeChangeId on conflict", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "conflict-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  process.env.JJ_SOURCE_WORKSPACE = workspace;
  process.env.JJ_SOURCE_CHANGE = "sourcechange";
  process.env.JJ_TARGET_CHANGE = "targetchange";
  process.env.JJ_OP_ID = "opconflict";
  process.env.JJ_STATUS_OUTPUT = "The working copy has no changes.\n";
  process.env.JJ_RESOLVE_LIST = "Conflict in src/a.ts\n";

  const run = await createRunRecord({
    id: "run-conflict",
    repoRoot: env.repo,
    task: "conflict jj work",
    backend: "claude",
    targetBranch: "targetchange",
    baseCommit: "basechange",
    vcs: "jj",
    useWorktree: true,
    worktreePath: workspace,
    worktreeWorkspaceName: "rudder-run-conflict",
  });

  const merged = await mergeJjRunIntoCurrentWorkspace(run);

  assert.equal(merged.status, "merge-conflict");
  assert.equal(merged.merge?.status, "conflict");
  assert.deepEqual(merged.merge?.conflictedFiles, ["src/a.ts"]);
  assert.equal(merged.merge?.operationId, "opconflict");
});

test("jj merge refuses when the integration workspace already has unresolved conflicts", async (t) => {
  const env = await setupFakeJj(t);
  const workspace = path.join(env.temp, "guard-workspace");
  await fsp.mkdir(path.join(workspace, ".jj"), { recursive: true });
  process.env.JJ_SOURCE_WORKSPACE = workspace;
  process.env.JJ_SOURCE_CHANGE = "sourcechange";
  process.env.JJ_TARGET_CHANGE = "targetchange";
  process.env.JJ_STATUS_OUTPUT = "The working copy has no changes.\n";
  process.env.JJ_RESOLVE_LIST_PRE = "Conflict in src/x.ts\n";

  const run = await createRunRecord({
    id: "run-guard",
    repoRoot: env.repo,
    task: "guard jj work",
    backend: "claude",
    targetBranch: "targetchange",
    baseCommit: "basechange",
    vcs: "jj",
    useWorktree: true,
    worktreePath: workspace,
    worktreeWorkspaceName: "rudder-run-guard",
  });

  // allowDirty=true mirrors the TUI; the conflict guard must fire regardless.
  await assert.rejects(
    mergeJjRunIntoCurrentWorkspace(run, true),
    /already has unresolved conflicts \(src\/x\.ts\); resolve them before merging another run/,
  );
  assert.equal(run.merge?.status, "failed");
  assert.match(run.merge?.error ?? "", /unresolved conflicts/);
  // No merge change may be created on top of a conflicted @.
  assert.doesNotMatch(await readLog(env.log), /\|new /);
});

test("jjDiff recovers once from a stale working copy", async (t) => {
  const env = await setupFakeJj(t);
  process.env.JJ_STATUS_OUTPUT = "M src/a.ts\n";
  process.env.JJ_DIFF_OUTPUT = "diff payload\n";
  process.env.JJ_DIFF_STALE_ONCE = "1";

  const diff = await jjDiff(env.repo);

  assert.match(diff, /diff payload/);
  assert.match(await readLog(env.log), /workspace update-stale/);
});

test("currentOpId recovers once from a stale working copy", async (t) => {
  const env = await setupFakeJj(t);
  process.env.JJ_OP_ID = "oprecovered";
  process.env.JJ_OP_STALE_ONCE = "1";

  assert.equal(await currentOpId(env.repo), "oprecovered");
  assert.match(await readLog(env.log), /workspace update-stale/);
});

test("currentOpId warns and returns empty when the op id cannot be captured", async (t) => {
  const env = await setupFakeJj(t);
  process.env.JJ_OP_FAIL = "1";
  const warn = t.mock.method(console, "warn", () => {});

  assert.equal(await currentOpId(env.repo), "");
  const messages = warn.mock.calls.map((call) => String(call.arguments[0] ?? ""));
  assert.ok(
    messages.some((message) => /could not capture jj op id.*not be undoable via rudder undo/.test(message)),
    `expected undo-waypoint warning, got: ${JSON.stringify(messages)}`,
  );
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
    JJ_OP_ID: process.env.JJ_OP_ID,
    JJ_STATUS_OUTPUT: process.env.JJ_STATUS_OUTPUT,
    JJ_DIFF_OUTPUT: process.env.JJ_DIFF_OUTPUT,
    JJ_RESOLVE_LIST: process.env.JJ_RESOLVE_LIST,
    JJ_RESOLVE_LIST_PRE: process.env.JJ_RESOLVE_LIST_PRE,
    JJ_DIFF_STALE_ONCE: process.env.JJ_DIFF_STALE_ONCE,
    JJ_OP_STALE_ONCE: process.env.JJ_OP_STALE_ONCE,
    JJ_OP_FAIL: process.env.JJ_OP_FAIL,
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
  delete process.env.JJ_OP_ID;
  delete process.env.JJ_STATUS_OUTPUT;
  delete process.env.JJ_DIFF_OUTPUT;
  delete process.env.JJ_RESOLVE_LIST;
  delete process.env.JJ_RESOLVE_LIST_PRE;
  delete process.env.JJ_DIFF_STALE_ONCE;
  delete process.env.JJ_OP_STALE_ONCE;
  delete process.env.JJ_OP_FAIL;
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
# Per-test scratch base for state markers (stale-once flags, merge-ran flag).
MARK="\${JJ_LOG:-/tmp/rudder-fake-jj}"

case "\${1:-}" in
  --version)
    echo "jj 0.40.0-test"
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
      update-stale)
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
    if [ "\${JJ_DIFF_STALE_ONCE:-0}" = "1" ] && [ ! -e "\${MARK}.diffstale" ]; then
      : > "\${MARK}.diffstale"
      echo "Error: The working copy is stale" >&2
      exit 1
    fi
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
  op)
    if [ "\${2:-}" = "log" ]; then
      if [ "\${JJ_OP_FAIL:-0}" = "1" ]; then
        echo "op log exploded" >&2
        exit 1
      fi
      if [ "\${JJ_OP_STALE_ONCE:-0}" = "1" ] && [ ! -e "\${MARK}.opstale" ]; then
        : > "\${MARK}.opstale"
        echo "Error: The working copy is stale" >&2
        exit 1
      fi
      echo "\${JJ_OP_ID:-op000000}"
      exit 0
    fi
    ;;
  new)
    if [ "\${JJ_NEW_FAIL:-0}" = "1" ]; then
      echo "merge exploded" >&2
      exit 42
    fi
    : > "\${MARK}.merged"
    exit 0
    ;;
  resolve)
    if [ "\${2:-}" = "--list" ]; then
      if [ ! -e "\${MARK}.merged" ]; then
        # Pre-merge conflict state: what \`@\` carried before any \`jj new\` ran.
        printf '%b' "\${JJ_RESOLVE_LIST_PRE:-}"
        exit 0
      fi
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
