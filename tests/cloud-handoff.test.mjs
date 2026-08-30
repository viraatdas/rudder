import assert from "node:assert/strict";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { buildSingleRunPlan, copyRudderState } from "../dist/cloud.js";
import { findMigrationCandidates } from "../dist/migration.js";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function candidate(runId, decision) {
  return {
    runId,
    task: `task for ${runId}`,
    backend: "claude",
    status: "running",
    workspacePath: `/tmp/ws-${runId}`,
    reason: decision === "migrate" ? "resumable" : "no-session",
    decision,
  };
}

test("single-run handoff plan moves only the requested run", () => {
  const candidates = [
    candidate("run-a", "migrate"),
    candidate("run-b", "migrate"),
    candidate("run-c", "migrate-fresh"),
  ];
  const plan = buildSingleRunPlan(candidates, "run-b");
  assert.deepEqual(plan.migrated.map((c) => c.runId), ["run-b"]);
  assert.deepEqual(plan.stayedLocal.map((c) => c.runId).sort(), ["run-a", "run-c"]);
  // The requested run keeps its own decision (fresh restarts stay fresh).
  const fresh = buildSingleRunPlan(candidates, "run-c");
  assert.equal(fresh.migrated[0]?.decision, "migrate-fresh");
});

test("rudder state snapshot can be scoped to a single run's record", async () => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-handoff-"));
  const stage = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-handoff-stage-"));
  try {
    for (const runId of ["run-a", "run-b"]) {
      const dir = path.join(root, ".rudder", "runs", runId);
      await fsp.mkdir(dir, { recursive: true });
      await fsp.writeFile(path.join(dir, "run.json"), JSON.stringify({ id: runId }));
    }
    const scoped = await copyRudderState(root, stage, ["run-a"]);
    assert.equal(scoped.runs, 1);
    assert.ok(fs.existsSync(path.join(stage, ".rudder", "runs", "run-a", "run.json")));
    assert.ok(
      !fs.existsSync(path.join(stage, ".rudder", "runs", "run-b", "run.json")),
      "a single-run handoff must not ship other runs' records: the cloud dashboard would resume agents that are still running locally",
    );

    const all = await copyRudderState(root, await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-handoff-all-")));
    assert.equal(all.runs, 2, "unscoped fleet migration still ships every record");
  } finally {
    await fsp.rm(root, { recursive: true, force: true });
    await fsp.rm(stage, { recursive: true, force: true });
  }
});

test("a migrated run is never re-offered for migration", async () => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-remigrate-"));
  try {
    const ws = path.join(root, "ws");
    await fsp.mkdir(ws, { recursive: true });
    const dir = path.join(root, ".rudder", "runs", "run-m");
    await fsp.mkdir(dir, { recursive: true });
    await fsp.writeFile(
      path.join(dir, "run.json"),
      JSON.stringify({
        id: "run-m",
        task: "implement the thing",
        backend: "claude",
        status: "migrated",
        workspace: { enabled: true, path: ws },
      }),
    );
    const candidates = await findMigrationCandidates(root);
    assert.equal(
      candidates.length,
      0,
      "an agent already living in the cloud must not be re-offered — every `rudder cloud` used to re-upload it forever",
    );
  } finally {
    await fsp.rm(root, { recursive: true, force: true });
  }
});

// The migration payload crosses three codebases: the CLI writes it
// (src/cloud.ts + src/migration.ts), the worker supervisor stages it
// (cloud/worker/supervisor.mjs), and the cloud dashboard resumes from it
// (native/src/gitio.rs). A renamed field on one side silently no-ops the whole
// resume — that exact bug shipped once ("worktree" vs "workspace") — so pin
// the shared vocabulary here.
test("migration payload field names agree across CLI, worker, and dashboard", async () => {
  const cli = await fsp.readFile(path.join(repoRoot, "src", "cloud.ts"), "utf8");
  const migration = await fsp.readFile(path.join(repoRoot, "src", "migration.ts"), "utf8");
  const supervisor = await fsp.readFile(path.join(repoRoot, "cloud", "worker", "supervisor.mjs"), "utf8");
  const gitio = await fsp.readFile(path.join(repoRoot, "native", "src", "gitio.rs"), "utf8");

  // Manifest entry fields the CLI writes must be what the supervisor reads.
  for (const field of ["cloudWorkspaceRelativePath", "sessionJsonlSnapshotPath", "workspaceBranch"]) {
    assert.ok(migration.includes(field), `src/migration.ts defines ${field}`);
    assert.ok(supervisor.includes(field), `supervisor.mjs reads ${field}`);
  }
  // Staging directories inside the snapshot tarball. (Session jsonls need no
  // directory contract: the worker resolves them via the manifest's
  // sessionJsonlSnapshotPath hint, checked above.)
  assert.ok(cli.includes(`"migrated-workspaces"`), "src/cloud.ts stages migrated-workspaces/");
  assert.ok(supervisor.includes("migrated-workspaces"), "supervisor.mjs unpacks migrated-workspaces/");
  // The worker's .rudder/migration.json summary feeds the dashboard's resume.
  assert.ok(supervisor.includes("workspacePath"), "supervisor.mjs writes workspacePath");
  assert.ok(gitio.includes("workspacePath"), "gitio.rs reads workspacePath");
});
