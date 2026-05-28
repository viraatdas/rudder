import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  mergeRunIntoCurrentBranch,
  resolveRebaseBaseRef,
  syncRunWorktree,
} from "../dist/git.js";
import {
  createRunRecord,
  loadRunRecord,
} from "../dist/state.js";

test("sync persists commit preparation failures before returning", async (t) => {
  const repo = await setupRepo(t);
  const run = await createRunRecord({
    id: "run-sync-failure",
    repoRoot: repo,
    task: "sync failure",
    backend: "claude",
    targetBranch: "main",
    baseCommit: git(repo, "rev-parse", "HEAD"),
    useWorktree: true,
    worktreeBranch: "run-sync-failure",
    worktreePath: repo,
  });
  run.status = "completed";
  await makeRebaseInProgress(repo);

  const synced = await syncRunWorktree(run, "main");
  const saved = await loadRunRecord(repo, run.id);

  assert.equal(synced.status, "failed");
  assert.equal(synced.sync?.status, "failed");
  assert.match(synced.sync?.error || "", /unfinished rebase/);
  assert.equal(saved?.status, "failed");
  assert.equal(saved?.sync?.status, "failed");
});

test("merge persists commit preparation failures before returning", async (t) => {
  const repo = await setupRepo(t);
  const run = await createRunRecord({
    id: "run-merge-failure",
    repoRoot: repo,
    task: "merge failure",
    backend: "claude",
    targetBranch: "main",
    baseCommit: git(repo, "rev-parse", "HEAD"),
    useWorktree: true,
    worktreeBranch: "run-merge-failure",
    worktreePath: repo,
  });
  run.status = "completed";
  await makeRebaseInProgress(repo);

  const merged = await mergeRunIntoCurrentBranch(run, false, "rebase");
  const saved = await loadRunRecord(repo, run.id);

  assert.equal(merged.status, "failed");
  assert.equal(merged.merge?.status, "failed");
  assert.match(merged.merge?.error || "", /unfinished rebase/);
  assert.equal(saved?.status, "failed");
  assert.equal(saved?.merge?.status, "failed");
});

test("rebase base resolution prefers the checked-out local branch", async (t) => {
  const repo = await setupRepo(t);
  const remote = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-rebase-remote-"));
  t.after(async () => {
    await fsp.rm(remote, { recursive: true, force: true });
  });
  git(remote, "init", "--bare");
  git(repo, "remote", "add", "origin", remote);
  git(repo, "push", "-u", "origin", "main");
  await fsp.writeFile(path.join(repo, "file.txt"), "base\nlocal target commit\n");
  git(repo, "add", "file.txt");
  git(repo, "commit", "-m", "local target commit");

  const baseRef = await resolveRebaseBaseRef(repo, "main");

  assert.equal(baseRef, "main");
});

async function setupRepo(t) {
  const temp = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-rebase-first-"));
  t.after(async () => {
    await fsp.rm(temp, { recursive: true, force: true });
  });
  git(temp, "init", "-b", "main");
  git(temp, "config", "user.email", "test@example.com");
  git(temp, "config", "user.name", "Test User");
  await fsp.writeFile(path.join(temp, "file.txt"), "base\n");
  git(temp, "add", "file.txt");
  git(temp, "commit", "-m", "base");
  git(temp, "branch", "run-sync-failure");
  git(temp, "branch", "run-merge-failure");
  return temp;
}

async function makeRebaseInProgress(repo) {
  const rawPath = git(repo, "rev-parse", "--git-path", "rebase-merge");
  const rebasePath = path.isAbsolute(rawPath) ? rawPath : path.join(repo, rawPath);
  await fsp.mkdir(rebasePath, { recursive: true });
}

function git(cwd, ...args) {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}
