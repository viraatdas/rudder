// Renaming or moving a repo directory must not strand its recorded runs.
// The incident: `mv aws-v2 libra` left run.json records pointing at the dead
// root, so every relaunch spawned into a nonexistent cwd and exited 1 — and
// the record found in the field had repoRoot ALREADY rewritten by a later
// save while workspace.path still said the old name, so healing cannot key on
// the stored root alone.
import assert from "node:assert/strict";
import test from "node:test";

import { healMovedRepoPaths } from "../dist/state.js";

test("heals both-stale records via the stored-root prefix", () => {
  const record = {
    id: "r1",
    repoRoot: "/old/place/aws-v2",
    workspace: { path: "/old/place/aws-v2/.rudder-workspaces/aws-v2-abc/task-def" },
    task: "keep /old/place/aws-v2 mentioned in prose untouched? no — prefix only",
  };
  healMovedRepoPaths(record, "/new/place/libra");
  assert.equal(record.repoRoot, "/new/place/libra");
  assert.equal(
    record.workspace.path,
    "/new/place/libra/.rudder-workspaces/aws-v2-abc/task-def",
  );
});

test("heals the field-observed shape: repoRoot new, nested paths stale", () => {
  const record = {
    id: "r2",
    repoRoot: "/new/place/libra",
    workspace: { path: "/old/place/aws-v2/.rudder-workspaces/aws-v2-abc/task-def" },
  };
  healMovedRepoPaths(record, "/new/place/libra");
  assert.equal(
    record.workspace.path,
    "/new/place/libra/.rudder-workspaces/aws-v2-abc/task-def",
    "the .rudder-workspaces marker rebases even when the stored root is useless as a key",
  );
});

test("a record that is already true is left byte-identical", () => {
  const record = {
    id: "r3",
    repoRoot: "/new/place/libra",
    workspace: { path: "/new/place/libra/.rudder-workspaces/libra-abc/task-def" },
  };
  const before = JSON.stringify(record);
  healMovedRepoPaths(record, "/new/place/libra");
  assert.equal(JSON.stringify(record), before);
});
