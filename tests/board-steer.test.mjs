import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { RudderBus } from "../dist/bus.js";
import { getBoardToken, startBoardDaemon } from "../dist/board/daemon.js";
import { updateGraph } from "../dist/graph.js";
import { createRunRecord, loadRunRecord, registerProject, saveRunRecord } from "../dist/state.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const packageRoot = path.resolve(here, "..");
const cli = path.join(packageRoot, "dist", "index.js");

async function poll(fn, timeoutMs = 15_000, intervalMs = 50) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  return null;
}

async function fixture(t, name) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), `rudder-board-${name}-`));
  const repo = path.join(root, "repo");
  const home = path.join(root, "home");
  await fsp.mkdir(repo, { recursive: true });
  await fsp.mkdir(home, { recursive: true });
  const previousHome = process.env.RUDDER_HOME;
  process.env.RUDDER_HOME = home;
  t.after(async () => {
    if (previousHome === undefined) delete process.env.RUDDER_HOME;
    else process.env.RUDDER_HOME = previousHome;
    await fsp.rm(root, { recursive: true, force: true }).catch(() => {});
  });
  const project = await registerProject(repo);
  return { root, repo, home, project };
}

async function seedReviewNode(repo) {
  const run = await createRunRecord({
    id: "review-run",
    repoRoot: repo,
    task: "Review the current behavior",
    backend: "claude",
    targetBranch: "main",
    baseCommit: "base",
    vcs: "jj",
    useWorkspace: false,
    workspacePath: repo,
  });
  run.status = "completed";
  await saveRunRecord(run);
  await updateGraph(repo, (graph) => {
    graph.nodes["stable-node"] = {
      id: "stable-node",
      title: "Review the current behavior",
      prompt: run.task,
      backend: "claude",
      status: "review",
      runId: run.id,
      deps: [],
      source: "planner",
      createdAt: run.createdAt,
      updatedAt: run.updatedAt,
    };
    return graph;
  });
  return run;
}

test("projector board queues review feedback and new tasks for the native owner", async (t) => {
  const { root, repo, project } = await fixture(t, "projector");
  await seedReviewNode(repo);
  const server = await startBoardDaemon({
    port: 0,
    repoRoot: repo,
    bus: new RudderBus(),
    controlMode: "projector",
  });
  t.after(() => server.close());

  const headers = {
    "content-type": "application/json",
    "x-rudder-token": getBoardToken(),
  };
  const steer = await fetch(`${server.url}/api/projects/${project.slug}/tasks/stable-node/steer`, {
    method: "POST",
    headers,
    body: JSON.stringify({
      requestId: "projector-request-0001",
      instruction: "Cover the empty state before we merge.",
    }),
  });
  assert.equal(steer.status, 202);
  const receipt = await steer.json();
  assert.equal(receipt.status, "queued");
  assert.equal(receipt.requestId, "projector-request-0001");
  const duplicateSteer = await fetch(`${server.url}/api/projects/${project.slug}/tasks/stable-node/steer`, {
    method: "POST",
    headers,
    body: JSON.stringify({
      requestId: "projector-request-0001",
      instruction: "Cover the empty state before we merge.",
    }),
  });
  assert.equal(duplicateSteer.status, 202, "same request id is accepted without a second inbox mutation");

  const create = await fetch(`${server.url}/api/projects/${project.slug}/tasks`, {
    method: "POST",
    headers,
    body: JSON.stringify({ requestId: "projector-task-0001", prompt: "Add a keyboard-accessibility pass." }),
  });
  assert.equal(create.status, 202);

  const merge = await fetch(`${server.url}/api/projects/${project.slug}/tasks/stable-node/merge`, {
    method: "POST",
    headers,
  });
  assert.equal(merge.status, 202, "projector merge is queued for the native owner");

  const inbox = path.join(repo, ".rudder", "steer");
  const requests = await Promise.all(
    (await fsp.readdir(inbox)).filter((name) => name.endsWith(".json"))
      .map(async (name) => JSON.parse(await fsp.readFile(path.join(inbox, name), "utf8"))),
  );
  assert.equal(requests.length, 3, "browser actions are durable native-owner requests");
  assert.ok(requests.some((r) => r.kind === "steer" && r.taskId === "stable-node"));
  assert.ok(requests.some((r) => r.kind === "task" && r.taskId === "conductor"));
  assert.ok(requests.some((r) => r.kind === "merge" && r.taskId === "stable-node"));

  const state = await fetch(`${server.url}/api/projects/${project.slug}/state`).then((r) => r.json());
  const node = state.nodes.find((candidate) => candidate.id === "stable-node");
  assert.ok(node, "graph node keeps its stable id after launch");
  assert.equal(node.runId, "review-run");

  const otherRepo = path.join(root, "other-repo");
  await fsp.mkdir(otherRepo, { recursive: true });
  const other = await registerProject(otherRepo);
  const foreignShell = await fetch(`${server.url}/rudder/${other.slug}`).then((r) => r.text());
  assert.match(foreignShell, /__RUDDER_CAN_MUTATE__ = false/, "foreign project is visibly read-only");
  const foreignMutation = await fetch(`${server.url}/api/projects/${other.slug}/tasks`, {
    method: "POST",
    headers,
    body: JSON.stringify({ prompt: "This must not reach the owner for another repo." }),
  });
  assert.equal(foreignMutation.status, 409, "a daemon never mutates a non-owner repository");
});

test("standalone board redirects a running worker and returns it through review", { timeout: 40_000 }, async (t) => {
  const { repo, home, project } = await fixture(t, "scheduler");
  const git = (args) => spawnSync("git", args, { cwd: repo, stdio: "ignore" });
  assert.equal(git(["init", "-q"]).status, 0);
  assert.equal(git(["config", "user.email", "board@rudder.test"]).status, 0);
  assert.equal(git(["config", "user.name", "Board Test"]).status, 0);
  await fsp.writeFile(path.join(repo, "README.md"), "# board steering\n");
  assert.equal(git(["add", "README.md"]).status, 0);
  assert.equal(git(["commit", "-q", "-m", "initial"]).status, 0);

  const run = await createRunRecord({
    id: "running-run",
    repoRoot: repo,
    task: "Explain the repository layout without editing files",
    backend: "claude",
    targetBranch: "main",
    baseCommit: "HEAD",
    vcs: "jj",
    useWorkspace: false,
    workspacePath: repo,
  });
  const firstAttempt = "initial-attempt";
  run.status = "running";
  run.process = { attemptId: firstAttempt, startedAt: new Date().toISOString() };
  await saveRunRecord(run);
  await updateGraph(repo, (graph) => {
    graph.nodes["running-node"] = {
      id: "running-node",
      title: "Explain the repository layout",
      prompt: run.task,
      backend: "claude",
      status: "running",
      runId: run.id,
      deps: [],
      source: "planner",
      createdAt: run.createdAt,
      updatedAt: run.updatedAt,
    };
    return graph;
  });

  const env = {
    ...process.env,
    RUDDER_HOME: home,
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_FAKE_BACKEND: "1",
    RUDDER_FAKE_BACKEND_DELAY_MS: "2500",
  };
  const firstWorker = spawn(
    process.execPath,
    [cli, "__worker", "--repo", repo, "--run", run.id, "--attempt", firstAttempt],
    { cwd: repo, env, stdio: "ignore" },
  );
  run.process.pid = firstWorker.pid;
  await saveRunRecord(run, { expectedAttemptId: firstAttempt });
  t.after(() => {
    if (firstWorker.exitCode === null) firstWorker.kill("SIGKILL");
  });

  const board = spawn(process.execPath, [cli, "board", "--no-open", "--port", "0"], {
    cwd: repo,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let boardLog = "";
  board.stdout.on("data", (chunk) => { boardLog += chunk; });
  board.stderr.on("data", (chunk) => { boardLog += chunk; });
  t.after(() => {
    if (board.exitCode === null) board.kill("SIGTERM");
  });

  const url = await poll(() => boardLog.match(/listening on (http:\/\/127\.0\.0\.1:\d+)/)?.[1]);
  assert.ok(url, `board did not start:\n${boardLog}`);
  const shell = await fetch(`${url}/rudder/${project.slug}`).then((r) => r.text());
  const token = shell.match(/__RUDDER_TOKEN__ = "([a-f0-9]+)"/)?.[1];
  assert.ok(token, "shell exposes its per-daemon mutation token");

  const response = await fetch(`${url}/api/projects/${project.slug}/tasks/running-node/steer`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-rudder-token": token },
    body: JSON.stringify({
      requestId: "headless-request-0001",
      instruction: "Focus the explanation on the web control path.",
    }),
  });
  const receipt = await response.json();
  assert.equal(response.status, 200, JSON.stringify(receipt));
  assert.equal(receipt.status, "accepted");
  assert.equal(receipt.mode, "redirected");
  const duplicateResponse = await fetch(`${url}/api/projects/${project.slug}/tasks/running-node/steer`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-rudder-token": token },
    body: JSON.stringify({
      requestId: "headless-request-0001",
      instruction: "Focus the explanation on the web control path.",
    }),
  });
  const duplicateReceipt = await duplicateResponse.json();
  assert.equal(duplicateResponse.status, 200);
  assert.equal(duplicateReceipt.requestId, "headless-request-0001");

  const redirected = await poll(async () => {
    const current = await loadRunRecord(repo, run.id);
    return current?.turns?.some((turn) => turn.prompt === "Focus the explanation on the web control path.")
      ? current
      : null;
  });
  assert.ok(redirected, "the same run records the browser direction as its next turn");
  assert.equal(
    redirected.turns.filter((turn) => turn.prompt === "Focus the explanation on the web control path.").length,
    1,
    "retrying the same request id does not append or launch a duplicate turn",
  );
  assert.equal(redirected.id, run.id);
  assert.equal(redirected.workspace.path, repo, "steering preserves the workspace");
  assert.notEqual(redirected.process?.attemptId, firstAttempt, "a fresh attempt owns redirected state");
  run.status = "failed";
  const staleSaved = await saveRunRecord(run, { expectedAttemptId: firstAttempt });
  assert.equal(staleSaved, false, "the superseded worker cannot overwrite the redirected attempt");
  const afterStaleWrite = await loadRunRecord(repo, run.id);
  assert.equal(afterStaleWrite?.process?.attemptId, redirected.process?.attemptId);
  assert.notEqual(afterStaleWrite?.status, "failed");

  const reviewed = await poll(async () => {
    const graph = JSON.parse(await fsp.readFile(path.join(repo, ".rudder", "graph.json"), "utf8"));
    return graph.nodes["running-node"]?.status === "review" ? graph.nodes["running-node"] : null;
  }, 20_000, 100);
  assert.ok(reviewed, `redirected worker did not return to review:\n${boardLog}`);

  const reviewResponse = await fetch(`${url}/api/projects/${project.slug}/tasks/running-node/steer`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-rudder-token": token },
    body: JSON.stringify({ instruction: "Add one concise example before merging." }),
  });
  const reviewReceipt = await reviewResponse.json();
  assert.equal(reviewResponse.status, 200, JSON.stringify(reviewReceipt));
  assert.equal(reviewReceipt.mode, "resumed", "Review feedback resumes rather than interrupting");
  const reopenedGraph = JSON.parse(await fsp.readFile(path.join(repo, ".rudder", "graph.json"), "utf8"));
  assert.equal(reopenedGraph.nodes["running-node"].status, "running", "request changes reopens the card");

  const reviewedAgain = await poll(async () => {
    const current = await loadRunRecord(repo, run.id);
    const graph = JSON.parse(await fsp.readFile(path.join(repo, ".rudder", "graph.json"), "utf8"));
    const hasTurn = current?.turns?.some((turn) => turn.prompt === "Add one concise example before merging.");
    return hasTurn && graph.nodes["running-node"]?.status === "review" ? current : null;
  }, 20_000, 100);
  assert.ok(reviewedAgain, "review feedback runs as another turn and returns through Review again");
  const finalState = await fetch(`${url}/api/projects/${project.slug}/state`).then((r) => r.json());
  const finalNode = finalState.nodes.find((candidate) => candidate.id === "running-node");
  assert.equal(finalNode?.runId, run.id, "stable graph identity still points at the same run");
  assert.deepEqual(
    finalNode?.updates.map((update) => update.instruction),
    [
      "Focus the explanation on the web control path.",
      "Add one concise example before merging.",
    ],
    "the issue drawer receives the persisted update thread",
  );
  const inboxFiles = await fsp.readdir(path.join(repo, ".rudder", "steer")).catch(() => []);
  assert.equal(inboxFiles.filter((name) => name.endsWith(".json")).length, 0, "headless steer never strands an inbox file");
});

test("fleet nudge queues one inbox request for every in-flight worker at once", async (t) => {
  // "keep it going, I lost internet for a bit" has to be ONE action, not one
  // steer per node. The daemon writes a single kind:"nudge-all" request and the
  // native owner resolves the target set, because only it knows which rows still
  // hold a live terminal and which have to be resumed.
  const { repo, project } = await fixture(t, "nudge");
  await seedReviewNode(repo);
  const server = await startBoardDaemon({
    port: 0,
    repoRoot: repo,
    bus: new RudderBus(),
    controlMode: "projector",
  });
  t.after(() => server.close());

  const headers = {
    "content-type": "application/json",
    "x-rudder-token": getBoardToken(),
  };
  const url = `${server.url}/api/projects/${project.slug}/nudge-all`;

  const empty = await fetch(url, {
    method: "POST",
    headers,
    body: JSON.stringify({ requestId: "nudge-request-0001", instruction: "   " }),
  });
  assert.equal(empty.status, 400, "an empty nudge is rejected before it reaches the inbox");

  const nudge = await fetch(url, {
    method: "POST",
    headers,
    body: JSON.stringify({
      requestId: "nudge-request-0001",
      instruction: "keep it going, I lost internet for a bit",
    }),
  });
  assert.equal(nudge.status, 202);
  assert.equal((await nudge.json()).requestId, "nudge-request-0001");

  const inbox = path.join(repo, ".rudder", "steer");
  const requests = await Promise.all(
    (await fsp.readdir(inbox)).filter((name) => name.endsWith(".json"))
      .map(async (name) => JSON.parse(await fsp.readFile(path.join(inbox, name), "utf8"))),
  );
  assert.equal(requests.length, 1, "one fleet request, not one per worker");
  assert.equal(requests[0].kind, "nudge-all");
  assert.equal(requests[0].instruction, "keep it going, I lost internet for a bit");

  const unauthorized = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ instruction: "should never reach the fleet" }),
  });
  assert.ok(unauthorized.status >= 400, "a fleet nudge is behind the same token guard as a steer");
});
