// End-to-end orchestrator integration test.
//
// Unlike scheduler.test.mjs / graph.test.mjs (which exercise the PURE decision
// functions in isolation), this test drives the WHOLE user-facing workflow
// through the real CLI binaries against a throwaway git+jj repo:
//
//   rudder plan "<task>"   -> real planner -> .rudder/graph.json (a DAG)
//   rudder board --no-open -> real daemon + scheduler drains the DAG:
//       schedule -> isolated jj workspace -> worker -> verify -> jj merge ->
//       unblock dependents -> merge them too.
//
// Determinism without auth or a real model comes from two TEST-ONLY env hooks
// baked into the production code (see callTextModel + getBackend):
//   RUDDER_FAKE_MODEL_OUTPUT  the planner returns this canned plan block
//   RUDDER_FAKE_BACKEND=1     each worker applies [[FAKE_FILE:..]] edits + exits 0
// and RUDDER_AUTO_STEER_DELAY_MS shrinks the post-pass steering wait so the DAG
// drains in seconds.
//
// This is the regression net for the class of bugs unit tests miss: the
// integration being broken while every component test passes (status thrash,
// workers writing to the main repo, a soft edge where hard was needed).

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const cli = path.join(repoRoot, "dist", "index.js");

const jjAvailable = spawnSync("jj", ["--version"], { stdio: "ignore" }).status === 0;

// A FAKE_FILE directive the fake backend writes verbatim into the worker's
// isolated workspace. Embedded in a node's prompt so the canned plan fully
// describes the work, no real model needed.
const fakeFile = (rel, body) => `\n[[FAKE_FILE:${rel}]]\n${body}\n[[/FAKE_FILE]]`;

const wrapPlan = (tasks) =>
  `Here is the plan.\nRUDDER_PLAN_TASKS_START\n${JSON.stringify({ tasks })}\nRUDDER_PLAN_TASKS_END\nThis split is safe.\n`;

function cliSync(args, opts = {}) {
  return spawnSync(process.execPath, [cli, ...args], {
    cwd: opts.cwd,
    env: { ...process.env, ...opts.env },
    encoding: "utf8",
    timeout: opts.timeout ?? 60_000,
  });
}

const readJson = async (file) => JSON.parse(await fsp.readFile(file, "utf8"));

async function poll(predicate, { timeoutMs = 90_000, intervalMs = 400 } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  return null;
}

// Stand up an isolated repo + RUDDER_HOME, register cleanup, and return handles.
async function makeWorkspace(t) {
  const work = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-e2e-"));
  const repo = path.join(work, "repo");
  const home = path.join(work, "home");
  await fsp.mkdir(repo, { recursive: true });
  await fsp.mkdir(home, { recursive: true });

  // `version: 1` is REQUIRED: loadConfig ignores a config without it and silently
  // falls back to defaults (reviewGate "manual"), which would never auto-merge.
  await fsp.writeFile(
    path.join(home, "config.json"),
    JSON.stringify({ version: 1, orchestrator: { reviewGate: "auto", maxParallel: 2 } }),
  );
  // Non-empty auth store so `maybeOnboard` short-circuits in the subprocess.
  await fsp.writeFile(
    path.join(home, "auth-profiles.json"),
    JSON.stringify({ profiles: {}, activeProfileId: null }),
  );

  const git = (args) =>
    assert.equal(spawnSync("git", args, { cwd: repo, stdio: "ignore" }).status, 0, `git ${args.join(" ")}`);
  git(["init", "-q"]);
  git(["config", "user.email", "e2e@rudder.test"]);
  git(["config", "user.name", "Rudder E2E"]);
  await fsp.writeFile(path.join(repo, "README.md"), "# e2e fixture\n");
  git(["add", "-A"]);
  git(["commit", "-q", "-m", "initial"]);

  const baseEnv = {
    RUDDER_HOME: home,
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_LEGACY_TMUX: "1",
    NO_COLOR: "1",
  };
  const graphFile = path.join(repo, ".rudder", "graph.json");

  t.after(async () => {
    await fsp.rm(work, { recursive: true, force: true }).catch(() => {});
  });

  return { work, repo, home, baseEnv, graphFile };
}

// Run `rudder plan` with the fake model, then drive `rudder board` until the DAG
// drains (every node merged/failed). Returns the final graph + board log; the
// board is killed in t.after. Asserts the plan succeeded.
async function planAndDrain(t, ws, tasks) {
  const planFile = path.join(ws.work, `plan-${Math.abs(hash(JSON.stringify(tasks)))}.txt`);
  await fsp.writeFile(planFile, wrapPlan(tasks));
  const planned = cliSync(["plan", "do the work"], {
    cwd: ws.repo,
    env: { ...ws.baseEnv, RUDDER_FAKE_MODEL_OUTPUT: `@${planFile}` },
  });
  assert.equal(planned.status, 0, `rudder plan failed:\n${planned.stdout}\n${planned.stderr}`);
  assert.ok(fs.existsSync(ws.graphFile), "rudder plan wrote .rudder/graph.json");

  const port = 30000 + (process.pid % 9000) + (t.fakePortOffset ?? 0);
  const board = spawn(process.execPath, [cli, "board", "--no-open", "--port", String(port)], {
    cwd: ws.repo,
    env: { ...process.env, ...ws.baseEnv, RUDDER_FAKE_BACKEND: "1", RUDDER_AUTO_STEER_DELAY_MS: "150" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let boardLog = "";
  board.stdout.on("data", (c) => (boardLog += c));
  board.stderr.on("data", (c) => (boardLog += c));
  t.after(async () => {
    if (board.exitCode === null) {
      board.kill("SIGTERM");
      await new Promise((r) => setTimeout(r, 300));
      if (board.exitCode === null) board.kill("SIGKILL");
    }
  });

  const drained = await poll(async () => {
    // Fail fast if the board process died rather than polling the full timeout.
    if (board.exitCode !== null) {
      throw new Error(`board exited early (code ${board.exitCode}). log:\n${boardLog}`);
    }
    const g = await readJson(ws.graphFile).catch(() => null);
    if (!g) return null;
    const ns = Object.values(g.nodes);
    return ns.every((n) => n.status === "merged" || n.status === "failed") ? g : null;
  }, { timeoutMs: 60_000 });
  assert.ok(drained, `DAG did not drain within timeout. board log:\n${boardLog}`);
  const rudderMd = await fsp.readFile(path.join(ws.repo, "RUDDER.md"), "utf8");
  assert.match(rudderMd, /<!-- RUDDER_GENERATED_START -->/, "the board writes the live RUDDER.md generated block");
  assert.match(rudderMd, /RUDDER .* Orchestrated Run Status/, "RUDDER.md is the plan/status surface for workers");
  return { graph: drained, boardLog };
}

function hash(s) {
  let h = 0;
  for (let i = 0; i < s.length; i += 1) h = (Math.imul(31, h) + s.charCodeAt(i)) | 0;
  return h;
}

const jjShow = (repo, rev, file) =>
  spawnSync("jj", ["file", "show", "-r", rev, file], { cwd: repo, encoding: "utf8" });

// ---------------------------------------------------------------------------
// Scenario 1: a HARD edge. The test node imports the impl, so it must wait for
// the impl to merge before it can launch. Exercises serialized ordering.
// ---------------------------------------------------------------------------
test("e2e: hard-edge DAG serializes, isolates workers, and merges through jj", { skip: !jjAvailable, timeout: 180_000 }, async (t) => {
  const ws = await makeWorkspace(t);
  const { graph } = await planAndDrain(t, ws, [
    {
      id: "n0",
      title: "impl-mathutils",
      prompt: "Create mathutils.py with an add function." + fakeFile("mathutils.py", "def add(a, b):\n    return a + b"),
      goal: "add mathutils.add",
      success: "mathutils.py defines add()",
      deps: [],
    },
    {
      id: "n1",
      title: "test-mathutils",
      prompt:
        "Create test_mathutils.py that imports add from mathutils." +
        fakeFile("test_mathutils.py", "from mathutils import add\n\n\ndef test_add():\n    assert add(1, 2) == 3"),
      goal: "add a pytest for add()",
      success: "test_mathutils.py imports add()",
      deps: [{ on: "n0", type: "hard", why: "the test imports add() which n0 defines" }],
    },
  ]);

  const nodes = Object.values(graph.nodes);
  assert.equal(nodes.length, 2, "two nodes");
  const impl = nodes.find((n) => n.title === "impl-mathutils");
  const tst = nodes.find((n) => n.title === "test-mathutils");
  assert.ok(impl && tst, "both nodes present by title");

  // The plan modeled the dependency as a single justified HARD edge.
  const edges = Object.values(graph.edges);
  assert.equal(edges.length, 1, "exactly one edge");
  assert.equal(edges[0].type, "hard", "edge is HARD");
  assert.equal(edges[0].from, impl.id, "edge FROM impl");
  assert.equal(edges[0].to, tst.id, "edge TO test");
  assert.ok(edges[0].why, "hard edge is justified");

  // Both merged through a real jj merge (a status flip alone can't advance the trunk).
  assert.equal(impl.status, "merged", `impl merged (got ${impl.status})`);
  assert.equal(tst.status, "merged", `test merged (got ${tst.status})`);
  assert.ok(graph.integrationChangeId, "merging advanced the integration trunk");

  // Each worker ran in its OWN isolated workspace, never the main repo.
  const implRun = await readJson(path.join(ws.repo, ".rudder", "runs", impl.runId, "run.json"));
  const tstRun = await readJson(path.join(ws.repo, ".rudder", "runs", tst.runId, "run.json"));
  for (const [label, r] of [["impl", implRun], ["test", tstRun]]) {
    assert.ok(r.worktree?.path, `${label} has a worktree`);
    assert.notEqual(path.resolve(r.worktree.path), path.resolve(ws.repo), `${label} ran OUTSIDE the main repo`);
  }
  assert.notEqual(path.resolve(implRun.worktree.path), path.resolve(tstRun.worktree.path), "distinct workspaces");

  // HARD edge respected: the test worker started only after the impl finished.
  assert.ok(implRun.process?.endedAt && tstRun.process?.startedAt, "both runs recorded timing");
  assert.ok(
    new Date(tstRun.process.startedAt).getTime() >= new Date(implRun.process.endedAt).getTime(),
    `hard dep violated: test started ${tstRun.process.startedAt} before impl ended ${implRun.process.endedAt}`,
  );

  // Merged CONTENT really flowed into the integration trunk.
  const implShow = jjShow(ws.repo, graph.integrationChangeId, "mathutils.py");
  assert.equal(implShow.status, 0, `jj read mathutils.py:\n${implShow.stderr}`);
  assert.match(implShow.stdout, /def add\(a, b\)/, "merged impl present");
  const tstShow = jjShow(ws.repo, graph.integrationChangeId, "test_mathutils.py");
  assert.equal(tstShow.status, 0, `jj read test_mathutils.py:\n${tstShow.stderr}`);
  assert.match(tstShow.stdout, /from mathutils import add/, "merged test present");

  // The user-facing status surface stays healthy after the run.
  const status = cliSync(["status", "--json"], { cwd: ws.repo, env: ws.baseEnv });
  assert.equal(status.status, 0, `rudder status failed:\n${status.stderr}`);
});

// ---------------------------------------------------------------------------
// Scenario 2: two INDEPENDENT nodes (no edges). They schedule in parallel and
// both fan in to the integration trunk. Exercises concurrent launch + fan-in
// merge (a different path than the serialized hard-edge case).
// ---------------------------------------------------------------------------
test("e2e: independent nodes run in parallel and both fan in to the trunk", { skip: !jjAvailable, timeout: 180_000 }, async (t) => {
  t.fakePortOffset = 1; // avoid colliding with scenario 1's port within a run
  const ws = await makeWorkspace(t);
  const { graph } = await planAndDrain(t, ws, [
    {
      id: "n0",
      title: "module-alpha",
      prompt: "Create alpha.py." + fakeFile("alpha.py", "ALPHA = 1"),
      goal: "create alpha.py",
      success: "alpha.py exists",
      deps: [],
    },
    {
      id: "n1",
      title: "module-beta",
      prompt: "Create beta.py." + fakeFile("beta.py", "BETA = 2"),
      goal: "create beta.py",
      success: "beta.py exists",
      deps: [],
    },
  ]);

  const nodes = Object.values(graph.nodes);
  assert.equal(nodes.length, 2, "two nodes");
  assert.equal(Object.values(graph.edges).length, 0, "no edges — fully independent work");
  assert.ok(nodes.every((n) => n.status === "merged"), `both merged (got ${nodes.map((n) => n.status).join(",")})`);
  assert.ok(graph.integrationChangeId, "integration trunk advanced");

  // Both independent changes fanned in: both files coexist in the merged trunk.
  for (const file of ["alpha.py", "beta.py"]) {
    const shown = jjShow(ws.repo, graph.integrationChangeId, file);
    assert.equal(shown.status, 0, `jj read ${file}:\n${shown.stderr}`);
  }
  const alpha = jjShow(ws.repo, graph.integrationChangeId, "alpha.py");
  assert.match(alpha.stdout, /ALPHA = 1/, "alpha content merged");
  const beta = jjShow(ws.repo, graph.integrationChangeId, "beta.py");
  assert.match(beta.stdout, /BETA = 2/, "beta content merged");
});
