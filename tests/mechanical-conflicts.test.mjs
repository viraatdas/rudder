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
