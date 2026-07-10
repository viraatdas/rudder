import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { captureCloudEnv, copyProjectEnvFiles } from "../dist/cloud.js";
import { findMigrationCandidates } from "../dist/migration.js";

test("fleet migration copies project dotenv files into the staged workspace", async () => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-cloud-env-"));
  const source = path.join(root, "source");
  const target = path.join(root, "target");
  await fsp.mkdir(path.join(source, "packages", "web"), { recursive: true });
  await fsp.mkdir(path.join(source, "node_modules", "ignored"), { recursive: true });
  await fsp.writeFile(path.join(source, ".env"), "DATABASE_URL=postgres://local\n");
  await fsp.writeFile(path.join(source, ".env.local"), "PRIVATE_TOKEN=secret\n");
  await fsp.writeFile(path.join(source, "packages", "web", ".env.production"), "PUBLIC_URL=https://example.test\n");
  await fsp.writeFile(path.join(source, "node_modules", "ignored", ".env"), "NOPE=1\n");
  await fsp.writeFile(path.join(source, "not-env.txt"), "ignored\n");

  const copied = await copyProjectEnvFiles(source, target);

  assert.equal(copied, 3);
  assert.equal(await fsp.readFile(path.join(target, ".env"), "utf8"), "DATABASE_URL=postgres://local\n");
  assert.equal(await fsp.readFile(path.join(target, ".env.local"), "utf8"), "PRIVATE_TOKEN=secret\n");
  assert.equal(
    await fsp.readFile(path.join(target, "packages", "web", ".env.production"), "utf8"),
    "PUBLIC_URL=https://example.test\n",
  );
  await assert.rejects(fsp.access(path.join(target, "node_modules", "ignored", ".env")));
  await fsp.rm(root, { recursive: true, force: true });
});

test("migration candidates include live isolated workers but exclude the conductor", async () => {
  const repo = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-cloud-candidates-"));
  const runs = path.join(repo, ".rudder", "runs");
  const workerPath = path.join(repo, "worker");
  await fsp.mkdir(workerPath, { recursive: true });

  async function writeRun(id, overrides) {
    const dir = path.join(runs, id);
    await fsp.mkdir(dir, { recursive: true });
    await fsp.writeFile(path.join(dir, "run.json"), JSON.stringify({
      id,
      status: "running",
      mode: "execute",
      task: id,
      backend: "codex",
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString(),
      repoRoot: repo,
      targetBranch: "main",
      baseCommit: "abc",
      worktree: { enabled: true, path: workerPath },
      ...overrides,
    }));
  }

  await writeRun("worker", {});
  await writeRun("orchestrator", {
    mode: "rudder-plan",
    worktree: { enabled: false, path: repo },
  });
  await writeRun("completed", { status: "completed" });

  const candidates = await findMigrationCandidates(repo);
  assert.deepEqual(candidates.map((candidate) => candidate.runId), ["worker"]);
  assert.equal(candidates[0].decision, "migrate-fresh");
  await fsp.rm(repo, { recursive: true, force: true });
});

test("fleet migration captures arbitrary inherited env but keeps the control blocklist", () => {
  const name = "RUDDER_TEST_CUSTOM_RUNTIME_VALUE";
  const previous = process.env[name];
  process.env[name] = "needed-in-cloud";
  try {
    assert.equal(captureCloudEnv()[name], undefined, "ordinary snapshots stay allowlisted");
    assert.equal(captureCloudEnv(true)[name], "needed-in-cloud");
    assert.equal(captureCloudEnv(true).PATH, undefined);
    assert.equal(captureCloudEnv(true).RUDDER_CLOUD_TOKEN, undefined);
  } finally {
    if (previous === undefined) delete process.env[name];
    else process.env[name] = previous;
  }
});
