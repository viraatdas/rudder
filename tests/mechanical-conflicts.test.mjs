import assert from "node:assert/strict";
import test from "node:test";

import {
  containsConflictMarkers,
  mechanicalMergeFor,
  mergeDecisionsSides,
  mergePackageJsonSides,
} from "../dist/jj.js";

test("DECISIONS.md conflicts union every side's entries instead of blocking", () => {
  // The dominant conflict in a real fleet: Rudder tells every worker to append
  // to one shared file, so N agents appending at the same EOF anchor collide.
  // Observed in the field as a FOUR-sided conflict that froze a whole plan.
  const merged = mergeDecisionsSides([
    "# Decisions\n\n- **What:** use zod\n- **What:** pin commander\n",
    "# Decisions\n\n- **What:** use zod\n- **What:** router owns tokenize\n",
    "# Decisions\n\n- **What:** lockfile is regenerated\n",
  ]);
  assert.ok(merged);
  for (const entry of ["use zod", "pin commander", "router owns tokenize", "lockfile is regenerated"]) {
    assert.ok(merged.includes(entry), `kept "${entry}"`);
  }
  assert.equal(
    merged.match(/use zod/g).length,
    1,
    "an entry present on two sides is not duplicated",
  );
  assert.ok(!containsConflictMarkers(merged));
});

test("a side that is itself conflicted never leaks markers into the resolution", () => {
  // Writing a "resolution" that still carries markers would snapshot literal
  // <<<<<<< lines into history, in the one file every agent is told to read.
  const merged = mergeDecisionsSides([
    "# Decisions\n\n- **What:** clean entry\n",
    "<<<<<<< conflict 1 of 1\n- **What:** poisoned\n>>>>>>> side\n",
  ]);
  assert.ok(merged);
  assert.ok(!containsConflictMarkers(merged), "markers are filtered out, not copied through");
  assert.ok(merged.includes("clean entry"));
});

test("package.json conflicts deep-merge dependency maps", () => {
  const merged = mergePackageJsonSides([
    JSON.stringify({ name: "app", dependencies: { zod: "^3" }, scripts: { test: "vitest" } }),
    JSON.stringify({ name: "app", dependencies: { commander: "^14" }, scripts: { build: "tsc" } }),
  ]);
  assert.ok(merged);
  const value = JSON.parse(merged);
  assert.deepEqual(value.dependencies, { zod: "^3", commander: "^14" });
  assert.deepEqual(value.scripts, { test: "vitest", build: "tsc" });
});

test("an unparseable package.json side escalates instead of being guessed at", () => {
  assert.equal(
    mergePackageJsonSides([JSON.stringify({ name: "app" }), "{ not json"]),
    undefined,
    "a broken side is a real conflict, not a mechanical one",
  );
});

test("only known-mechanical files are auto-resolved", () => {
  // Source code is NEVER mechanically resolved: guessing at a real product
  // disagreement is worse than stopping.
  assert.equal(mechanicalMergeFor("index.ts", ["a", "b"]), undefined);
  assert.equal(mechanicalMergeFor("router.test.ts", ["a", "b"]), undefined);
  assert.ok(mechanicalMergeFor(".gitignore", ["dist/\n", "node_modules/\n"]));
  assert.ok(mechanicalMergeFor("package-lock.json", ["{}", '{"a":1}']));
});

test(".gitignore conflicts union their rules without duplicating", () => {
  const merged = mechanicalMergeFor(".gitignore", ["dist/\nnode_modules/\n", "node_modules/\n.rudder/\n"]);
  assert.ok(merged);
  const lines = merged.split("\n").filter(Boolean);
  assert.deepEqual(lines, ["dist/", "node_modules/", ".rudder/"]);
});

test("a run whose merge conflicted can retry integration instead of being stuck 'active'", async () => {
  // The offstage stall: n2's work was finished and its merge had conflicted, but
  // its run status still read "running", so `rudder merge` refused it forever --
  // the only way to retry integration was the command the guard blocked. Five
  // nodes sat like this, and n6 hard-depended on one of them.
  const { mergeRun } = await import("../dist/run-manager.js");
  const fsp = await import("node:fs/promises");
  const os = await import("node:os");
  const path = await import("node:path");
  const { createRunRecord, saveRunRecord } = await import("../dist/state.js");

  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-blocked-"));
  const repo = path.join(root, "repo");
  await fsp.mkdir(repo, { recursive: true });
  const previousHome = process.env.RUDDER_HOME;
  process.env.RUDDER_HOME = path.join(root, "home");
  const previousCwd = process.cwd();
  process.chdir(repo);

  try {
    const run = await createRunRecord({
      id: "blocked-run",
      repoRoot: repo,
      task: "headless lane",
      backend: "claude",
      targetBranch: "main",
      baseCommit: "abc",
      vcs: "jj",
      useWorkspace: true,
      workspacePath: repo,
    });
    run.status = "running";
    run.merge = { status: "conflict", conflictedFiles: ["DECISIONS.md"] };
    await saveRunRecord(run);

    // It must get PAST the active guard. What it fails on afterwards is
    // environment (no real jj repo here); the guard itself is the contract.
    const error = await mergeRun("blocked-run", true, { silent: true }).then(
      () => null,
      (err) => err,
    );
    if (error) {
      assert.doesNotMatch(
        String(error.message),
        /is still active/,
        "a conflict-blocked run is no longer treated as a live agent",
      );
    }

    // A genuinely live run with no recorded merge is still refused.
    const live = await createRunRecord({
      id: "live-run",
      repoRoot: repo,
      task: "still working",
      backend: "claude",
      targetBranch: "main",
      baseCommit: "abc",
      vcs: "jj",
      useWorkspace: true,
      workspacePath: repo,
    });
    live.status = "running";
    await saveRunRecord(live);
    await assert.rejects(
      () => mergeRun("live-run", true, { silent: true }),
      /is still active/,
      "a live agent is still protected from a mid-edit merge",
    );
  } finally {
    process.chdir(previousCwd);
    if (previousHome === undefined) delete process.env.RUDDER_HOME;
    else process.env.RUDDER_HOME = previousHome;
    await fsp.rm(root, { recursive: true, force: true }).catch(() => {});
  }
});
