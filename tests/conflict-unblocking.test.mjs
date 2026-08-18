import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { execFileSync } from "node:child_process";

import { autoResolveMechanicalConflicts, jjConflictedFiles } from "../dist/jj.js";

function jj(repo, args) {
  return execFileSync("jj", args, { cwd: repo, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

function haveJj() {
  try {
    execFileSync("jj", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

async function realJjRepo(t) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-conflict-"));
  const repo = path.join(root, "repo");
  await fsp.mkdir(repo, { recursive: true });
  jj(repo, ["git", "init"]);
  jj(repo, ["config", "set", "--repo", "user.name", "Rudder Test"]);
  jj(repo, ["config", "set", "--repo", "user.email", "test@example.com"]);
  t.after(async () => {
    await fsp.rm(root, { recursive: true, force: true }).catch(() => {});
  });
  return repo;
}

// The exact shape that froze offstage: two agents each append their own entry to
// the shared DECISIONS.md, the merge conflicts, and every LATER merge is then
// refused -- so finished nodes pile up in review and everything hard-depending
// on them never launches. Uses real jj, not the fake harness, because the point
// is that a genuine jj conflict is cleared.
test("a real DECISIONS.md conflict is cleared instead of freezing integration", { skip: !haveJj() }, async (t) => {
  const repo = await realJjRepo(t);

  await fsp.writeFile(path.join(repo, "DECISIONS.md"), "# Decisions\n\n- **What:** base entry\n");
  jj(repo, ["describe", "-m", "base"]);
  jj(repo, ["new", "-m", "agent-a"]);
  const base = jj(repo, ["log", "--no-graph", "-r", "@-", "-T", "change_id.short()"]).trim();

  await fsp.writeFile(
    path.join(repo, "DECISIONS.md"),
    "# Decisions\n\n- **What:** base entry\n\n- **What:** agent A picked zod\n",
  );
  const agentA = jj(repo, ["log", "--no-graph", "-r", "@", "-T", "change_id.short()"]).trim();

  jj(repo, ["new", base, "-m", "agent-b"]);
  await fsp.writeFile(
    path.join(repo, "DECISIONS.md"),
    "# Decisions\n\n- **What:** base entry\n\n- **What:** agent B pinned commander\n",
  );
  const agentB = jj(repo, ["log", "--no-graph", "-r", "@", "-T", "change_id.short()"]).trim();

  // The merge Rudder performs when integrating a node.
  jj(repo, ["new", agentA, agentB, "-m", "integration"]);

  const before = await jjConflictedFiles(repo);
  assert.deepEqual(before, ["DECISIONS.md"], "the merge really does conflict");

  const remaining = await autoResolveMechanicalConflicts(repo);
  assert.deepEqual(remaining, [], "nothing is left blocking integration");

  const merged = await fsp.readFile(path.join(repo, "DECISIONS.md"), "utf8");
  assert.ok(merged.includes("agent A picked zod"), "agent A's decision survives");
  assert.ok(merged.includes("agent B pinned commander"), "agent B's decision survives");
  assert.ok(!merged.includes("<<<<<<<"), "no conflict markers are written back");
  assert.deepEqual(await jjConflictedFiles(repo), [], "jj agrees the conflict is gone");
});

// The other half of the contract: a genuine disagreement in source code must
// still stop, because guessing at it is worse than escalating to a resolver.
test("a real source conflict is NOT auto-resolved", { skip: !haveJj() }, async (t) => {
  const repo = await realJjRepo(t);

  await fsp.writeFile(path.join(repo, "router.ts"), "export const lane = 'base';\n");
  jj(repo, ["describe", "-m", "base"]);
  jj(repo, ["new", "-m", "agent-a"]);
  const base = jj(repo, ["log", "--no-graph", "-r", "@-", "-T", "change_id.short()"]).trim();

  await fsp.writeFile(path.join(repo, "router.ts"), "export const lane = 'container';\n");
  const agentA = jj(repo, ["log", "--no-graph", "-r", "@", "-T", "change_id.short()"]).trim();

  jj(repo, ["new", base, "-m", "agent-b"]);
  await fsp.writeFile(path.join(repo, "router.ts"), "export const lane = 'vm';\n");
  const agentB = jj(repo, ["log", "--no-graph", "-r", "@", "-T", "change_id.short()"]).trim();

  jj(repo, ["new", agentA, agentB, "-m", "integration"]);

  assert.deepEqual(await jjConflictedFiles(repo), ["router.ts"]);
  assert.deepEqual(
    await autoResolveMechanicalConflicts(repo),
    ["router.ts"],
    "a real product disagreement is escalated, never guessed at",
  );
});
