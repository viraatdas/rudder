import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { countActiveAgents } from "../cloud/worker/active-runs.mjs";

function writeRun(root, id, value) {
  const dir = path.join(root, ".rudder", "runs", id);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "run.json"), value);
}

test("cloud heartbeat counts only agents that are actively working", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-active-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const [id, status] of [
    ["created", "created"], ["running", "RUNNING"], ["steering", "steering"],
    ["verifying", "verifying"], ["paused", "paused"], ["review", "completed"],
    ["merged", "merged"], ["failed", "failed"],
  ]) writeRun(root, id, JSON.stringify({ status }));
  writeRun(root, "corrupt", "not json");
  fs.writeFileSync(path.join(root, ".rudder", "runs", "not-a-directory"), "ignored");
  assert.equal(countActiveAgents(root), 4);
  assert.equal(countActiveAgents(path.join(root, "missing")), 0);
});
