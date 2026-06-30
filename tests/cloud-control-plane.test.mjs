import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cloudDir = path.join(repoRoot, "cloud");
const require = createRequire(path.join(cloudDir, "package.json"));
const Database = require("better-sqlite3");

const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function freePort() {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function waitFor(fn, { timeout = 8_000, interval = 25 } = {}) {
  const started = Date.now();
  for (;;) {
    try {
      const result = await fn();
      if (result) return result;
    } catch {
      // Keep polling while the child starts.
    }
    if (Date.now() - started > timeout) throw new Error("waitFor timed out");
    await sleep(interval);
  }
}

async function startControlPlane(t, flyBase) {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-control-test-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    env: {
      ...process.env,
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      RUDDER_FLY_APP_NAME: "test-workers",
      FLY_API_TOKEN: "fly-test-token",
      FLY_API_HOSTNAME: flyBase,
      RUDDER_WORKSPACE_SWEEP_MS: "0",
      RUDDER_IDLE_PAUSE_MS: "0",
      SLACK_BOT_TOKEN: "",
      SLACK_SIGNING_SECRET: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let logs = "";
  child.stdout.on("data", (chunk) => { logs += chunk.toString(); });
  child.stderr.on("data", (chunk) => { logs += chunk.toString(); });
  t.after(async () => {
    if (child.exitCode === null) {
      child.kill("SIGTERM");
      await new Promise((resolve) => {
        const timer = setTimeout(() => {
          child.kill("SIGKILL");
          resolve();
        }, 3_000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
    fs.rmSync(dataDir, { recursive: true, force: true });
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  return { base, dbPath, logs: () => logs };
}

function seedToken(db, token, accountId = "account-test") {
  const now = new Date().toISOString();
  db.prepare(
    "insert into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(token), accountId, "owner@example.com", now, now);
}

function openWebSocket(url, token) {
  const ws = new WebSocket(url, { headers: { authorization: `Bearer ${token}` } });
  return new Promise((resolve, reject) => {
    ws.once("open", () => resolve(ws));
    ws.once("error", reject);
  });
}

test("worker replacement is race-free and output survives disconnect", async (t) => {
  const fly = http.createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
  });
  await new Promise((resolve) => fly.listen(0, "127.0.0.1", resolve));
  t.after(() => fly.close());
  const flyBase = `http://127.0.0.1:${fly.address().port}`;
  const server = await startControlPlane(t, flyBase);

  const cliToken = "rdr_control_cli";
  const workerToken = "rdrw_control_worker";
  const id = "cloud-reconnect";
  const accountId = "account-reconnect";
  const now = new Date().toISOString();
  const db = new Database(server.dbPath);
  seedToken(db, cliToken, accountId);
  db.prepare(`
    insert into rudder_sails
      (id, account_id, status, runtime, worker_token_hash, last_activity_at, created_at, updated_at)
    values (?, ?, 'running', 'fly', ?, ?, ?, ?)
  `).run(id, accountId, tokenHash(workerToken), now, now, now);
  db.close();

  const worker1 = await openWebSocket(`${server.base.replace("http", "ws")}/api/rudder/sail/${id}/worker`, workerToken);
  const client = await openWebSocket(`${server.base.replace("http", "ws")}/api/rudder/sail/${id}/attach`, cliToken);
  const statuses = [];
  client.on("message", (data, isBinary) => {
    if (!isBinary) statuses.push(Buffer.from(data).toString("utf8"));
  });
  worker1.send(Buffer.from("FIRST-MARKER\r\n"), { binary: true });
  await sleep(75);

  statuses.length = 0;
  const worker2 = await openWebSocket(`${server.base.replace("http", "ws")}/api/rudder/sail/${id}/worker`, workerToken);
  await new Promise((resolve) => worker1.once("close", resolve));
  await sleep(75);
  assert.ok(statuses.some((value) => value.includes("worker-connected")), server.logs());
  assert.equal(statuses.some((value) => value.includes("worker-disconnected")), false, server.logs());

  worker2.send(Buffer.from("SECOND-MARKER\r\n"), { binary: true });
  await sleep(75);
  worker2.close();
  client.close();
  await sleep(100);

  const output = await fetch(`${server.base}/api/rudder/sail/${id}/output`, {
    headers: { authorization: `Bearer ${cliToken}` },
  });
  assert.equal(output.status, 200, server.logs());
  const body = await output.json();
  assert.equal(body.connected, false);
  assert.match(body.output, /FIRST-MARKER/);
  assert.match(body.output, /SECOND-MARKER/);
});

test("heartbeats cannot resurrect terminal instances or fake workspace activity", async (t) => {
  const fly = http.createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
  });
  await new Promise((resolve) => fly.listen(0, "127.0.0.1", resolve));
  t.after(() => fly.close());
  const server = await startControlPlane(t, `http://127.0.0.1:${fly.address().port}`);
  const db = new Database(server.dbPath);
  const oldActivity = "2025-01-01T00:00:00.000Z";
  const now = new Date().toISOString();
  db.prepare(`
    insert into rudder_sails (id, account_id, status, runtime, worker_token_hash, created_at, updated_at)
    values ('terminal-sail', 'acct', 'completed', 'fly', ?, ?, ?)
  `).run(tokenHash("rdrw_terminal"), now, now);
  db.prepare(`
    insert into rudder_workspaces
      (id, account_id, workspace_key, status, worker_token_hash, last_activity_at, created_at, updated_at)
    values ('stopped-workspace', 'acct', 'workspace-key', 'stopped', ?, ?, ?, ?)
  `).run(tokenHash("rdrw_workspace"), oldActivity, now, now);
  db.close();

  const sailHeartbeat = await fetch(`${server.base}/api/rudder/sail/terminal-sail/heartbeat`, {
    method: "POST",
    headers: { authorization: "Bearer rdrw_terminal", "content-type": "application/json" },
    body: JSON.stringify({ state: "running" }),
  });
  assert.equal(sailHeartbeat.status, 200);
  assert.equal((await sailHeartbeat.json()).status, "completed");

  const workspaceHeartbeat = await fetch(`${server.base}/api/rudder/workspace/stopped-workspace/heartbeat`, {
    method: "POST",
    headers: { authorization: "Bearer rdrw_workspace", "content-type": "application/json" },
    body: JSON.stringify({ state: "running" }),
  });
  assert.equal(workspaceHeartbeat.status, 200);
  assert.equal((await workspaceHeartbeat.json()).status, "stopped");

  const check = new Database(server.dbPath, { readonly: true });
  assert.equal(check.prepare("select status from rudder_sails where id = ?").get("terminal-sail").status, "completed");
  const workspace = check.prepare(
    "select status, last_activity_at, last_heartbeat_at from rudder_workspaces where id = ?",
  ).get("stopped-workspace");
  check.close();
  assert.equal(workspace.status, "stopped");
  assert.equal(workspace.last_activity_at, oldActivity);
  assert.notEqual(workspace.last_heartbeat_at, null);
});

test("Fly refresh is parallel, workspace actions are distinct, delete works, and bad bodies fail safely", async (t) => {
  const flyCalls = [];
  const fly = http.createServer((req, res) => {
    flyCalls.push(`${req.method} ${req.url}`);
    const finish = () => {
      const state = req.url.endsWith("/suspend")
        ? "suspended"
        : req.url.endsWith("/stop")
          ? "stopped"
          : "started";
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ id: req.url.split("/").at(-1), state }));
    };
    if (req.method === "GET" && /\/machines\/sail-machine-/.test(req.url)) {
      setTimeout(finish, 250);
    } else {
      finish();
    }
  });
  await new Promise((resolve) => fly.listen(0, "127.0.0.1", resolve));
  t.after(() => fly.close());
  const server = await startControlPlane(t, `http://127.0.0.1:${fly.address().port}`);
  const cliToken = "rdr_lifecycle_cli";
  const accountId = "account-lifecycle";
  const now = new Date().toISOString();
  const db = new Database(server.dbPath);
  seedToken(db, cliToken, accountId);
  for (let index = 0; index < 8; index += 1) {
    db.prepare(`
      insert into rudder_sails
        (id, account_id, status, runtime, machine_id, worker_token_hash, last_activity_at, created_at, updated_at)
      values (?, ?, 'running', 'fly', ?, ?, ?, ?, ?)
    `).run(`parallel-${index}`, accountId, `sail-machine-${index}`, tokenHash(`rdrw_${index}`), now, now, now);
  }
  db.prepare(`
    insert into rudder_workspaces
      (id, account_id, workspace_key, status, machine_id, volume_id, worker_token_hash, last_activity_at, created_at, updated_at)
    values ('workspace-actions', ?, 'actions-key', 'running', 'workspace-machine', 'workspace-volume', ?, ?, ?, ?)
  `).run(accountId, tokenHash("rdrw_actions"), now, now, now);
  db.prepare(`
    insert into rudder_sails (id, account_id, status, runtime, worker_token_hash, created_at, updated_at)
    values ('delete-me', ?, 'completed', 'byo-vm', ?, ?, ?)
  `).run(accountId, tokenHash("rdrw_delete"), now, now);
  db.close();
  const headers = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };

  const started = Date.now();
  const list = await fetch(`${server.base}/api/rudder/sail`, { headers });
  const elapsed = Date.now() - started;
  assert.equal(list.status, 200, server.logs());
  assert.ok(elapsed < 1_200, `parallel refresh took ${elapsed}ms`);
  assert.equal(flyCalls.filter((call) => /GET .*\/machines\/sail-machine-/.test(call)).length, 8);

  for (const [action, expectedPath, expectedStatus] of [
    ["pause", "/suspend", "paused"],
    ["resume", "/start", "running"],
    ["stop", "/stop", "stopped"],
  ]) {
    const response = await fetch(`${server.base}/api/rudder/workspace/workspace-actions/${action}`, {
      method: "POST",
      headers,
      body: "{}",
    });
    assert.equal(response.status, 200, `${action}: ${server.logs()}`);
    assert.equal((await response.json()).status, expectedStatus);
    assert.ok(flyCalls.at(-1).includes(expectedPath), `${action} used ${flyCalls.at(-1)}`);
  }

  const beforeWorkspaceDelete = flyCalls.length;
  const workspaceDeleted = await fetch(`${server.base}/api/rudder/workspace/workspace-actions/delete`, {
    method: "POST",
    headers,
    body: "{}",
  });
  assert.equal(workspaceDeleted.status, 200, server.logs());
  const deleteCalls = flyCalls.slice(beforeWorkspaceDelete);
  const machineDelete = deleteCalls.findIndex((call) => call.startsWith("DELETE ") && call.includes("/machines/workspace-machine"));
  const volumeDelete = deleteCalls.findIndex((call) => call.startsWith("DELETE ") && call.includes("/volumes/workspace-volume"));
  assert.ok(machineDelete >= 0, JSON.stringify(deleteCalls));
  assert.ok(volumeDelete > machineDelete, JSON.stringify(deleteCalls));

  const deleted = await fetch(`${server.base}/api/rudder/sail/delete-me/delete`, {
    method: "POST",
    headers,
    body: "{}",
  });
  assert.equal(deleted.status, 200);
  assert.equal((await deleted.json()).deleted, true);

  const invalid = await fetch(`${server.base}/api/rudder/sail/parallel-0/input`, {
    method: "POST",
    headers,
    body: "{not-json",
  });
  assert.equal(invalid.status, 400);
  const oversized = await fetch(`${server.base}/api/cli/login/github-token`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: "x".repeat(2 * 1024 * 1024) }),
  });
  assert.equal(oversized.status, 413, server.logs());
  assert.equal((await fetch(`${server.base}/health`)).status, 200);
});
