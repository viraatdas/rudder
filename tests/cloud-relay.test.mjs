// End-to-end relay test: spawn the real control plane, attach a fake worker
// over the worker WebSocket, then drive the new HTTP message endpoints:
//   POST /api/rudder/sail/:id/input   -> bytes reach the worker PTY
//   GET  /api/rudder/sail/:id/output  -> buffered worker output comes back
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cloudDir = path.join(repoRoot, "cloud");

// The cloud/ subproject has its own build and dependencies (see AGENTS.md
// section 11). In a checkout where it was never built (a fresh worktree, for
// example), skip instead of erroring so `npm test` stays green without a
// network install.
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = cloudReady
  ? nodeTest
  : (name, fn) =>
      nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const require = createRequire(path.join(cloudDir, "package.json"));
const Database = cloudReady ? require("better-sqlite3") : null;

const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitFor(fn, { timeout = 8000, interval = 100 } = {}) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(interval);
  }
}

test("input is injected into the worker and output is read back", async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-test-"));
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
      SLACK_BOT_TOKEN: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let serverLog = "";
  child.stdout.on("data", (d) => { serverLog += d.toString(); });
  child.stderr.on("data", (d) => { serverLog += d.toString(); });

  t.after(async () => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });

  // Wait for the server to be healthy and to have created the schema.
  await waitFor(async () => {
    const res = await fetch(`${base}/health`).catch(() => null);
    return res && res.ok;
  });
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  // Seed an account token + a running sail with a known worker token.
  const cliToken = "rdr_test_cli_token";
  const workerToken = "rdrw_test_worker_token";
  const sailId = "cloud-relay-test";
  const accountId = "acct-test";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(cliToken), accountId, "test@example.com", now, now);
  db.prepare(
    "insert into rudder_sails (id, account_id, status, runtime, worker_token_hash, created_at, updated_at) values (?,?,?,?,?,?,?)",
  ).run(sailId, accountId, "running", "fly", tokenHash(workerToken), now, now);
  db.close();

  // Connect a fake worker. It records inbound binary (keystrokes) and, once
  // open, emits a line of output exactly like the real supervisor would.
  const received = [];
  const ws = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/sail/${sailId}/worker`, {
    headers: { authorization: `Bearer ${workerToken}` },
  });
  ws.binaryType = "nodebuffer";
  const opened = new Promise((resolve, reject) => {
    ws.on("open", resolve);
    ws.on("error", reject);
  });
  ws.on("message", (data, isBinary) => {
    if (isBinary && Buffer.isBuffer(data)) received.push(data.toString("utf8"));
  });
  await opened;
  ws.send(JSON.stringify({ type: "hello", cols: 120, rows: 32 }));
  // Worker emits output -> control plane buffers it for /output.
  ws.send(Buffer.from("AGENT-OUTPUT-MARKER-123\r\n", "utf8"), { binary: true });
  await sleep(200);

  const authHeaders = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };

  // POST /input -> delivered, and the worker actually receives the bytes.
  const inputRes = await fetch(`${base}/api/rudder/sail/${sailId}/input`, {
    method: "POST",
    headers: authHeaders,
    body: JSON.stringify({ text: "hello-from-test" }),
  });
  assert.equal(inputRes.status, 200, `input status (log: ${serverLog})`);
  const inputBody = await inputRes.json();
  assert.equal(inputBody.delivered, true);
  await waitFor(() => received.some((m) => m.includes("hello-from-test")));
  assert.ok(received.join("").includes("hello-from-test\r"), "worker received the message + submit");

  // GET /output -> the buffered worker output is returned.
  const outRes = await fetch(`${base}/api/rudder/sail/${sailId}/output`, { headers: authHeaders });
  assert.equal(outRes.status, 200);
  const outBody = await outRes.json();
  assert.equal(outBody.connected, true);
  assert.ok(outBody.output.includes("AGENT-OUTPUT-MARKER-123"), "output buffer returned");

  // Auth is enforced: no token -> 401-ish (not 200).
  const noAuth = await fetch(`${base}/api/rudder/sail/${sailId}/output`);
  assert.notEqual(noAuth.status, 200);

  // A workspace heartbeat proves the machine is alive, but must not count as
  // user activity or revive a workspace after the user has stopped it.
  const workspaceId = "workspace-relay-test";
  const workspaceToken = "rdrw_test_workspace_token";
  const oldActivity = "2024-01-01T00:00:00.000Z";
  const workspaceDb = new Database(dbPath);
  workspaceDb.prepare(`
    insert into rudder_workspaces (
      id, account_id, workspace_key, repo_name, status, machine_id, machine_state,
      worker_token_hash, last_activity_at, created_at, updated_at
    ) values (?,?,?,?,?,?,?,?,?,?,?)
  `).run(
    workspaceId, accountId, "workspace-key", "repo", "running", "machine-current", "started",
    tokenHash(workspaceToken), oldActivity, now, now,
  );
  workspaceDb.close();

  const heartbeatHeaders = {
    authorization: `Bearer ${workspaceToken}`,
    "content-type": "application/json",
  };
  const heartbeat = await fetch(`${base}/api/rudder/workspace/${workspaceId}/heartbeat`, {
    method: "POST",
    headers: heartbeatHeaders,
    body: JSON.stringify({ state: "running", machineId: "machine-current" }),
  });
  assert.equal(heartbeat.status, 200);
  const heartbeatDb = new Database(dbPath);
  let workspaceRow = heartbeatDb.prepare(
    "select status, last_activity_at, last_heartbeat_at from rudder_workspaces where id = ?",
  ).get(workspaceId);
  assert.equal(workspaceRow.last_activity_at, oldActivity, "heartbeat preserves the idle clock");
  assert.ok(workspaceRow.last_heartbeat_at, "heartbeat still updates liveness");
  heartbeatDb.prepare("update rudder_workspaces set status = 'stopped' where id = ?").run(workspaceId);
  heartbeatDb.close();

  const lateHeartbeat = await fetch(`${base}/api/rudder/workspace/${workspaceId}/heartbeat`, {
    method: "POST",
    headers: heartbeatHeaders,
    body: JSON.stringify({ state: "failed", machineId: "machine-current" }),
  });
  assert.equal(lateHeartbeat.status, 200);
  const stoppedDb = new Database(dbPath);
  workspaceRow = stoppedDb.prepare("select status from rudder_workspaces where id = ?").get(workspaceId);
  assert.equal(workspaceRow.status, "stopped", "late worker exit cannot overwrite a manual stop");
  stoppedDb.prepare("update rudder_workspaces set status = 'running' where id = ?").run(workspaceId);
  stoppedDb.close();

  // Attach before the worker is online. Resize and keystrokes must survive the
  // reconnect and arrive once, in order, when the worker connects.
  const clientWs = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/workspace/${workspaceId}/attach`, {
    headers: { authorization: `Bearer ${cliToken}` },
  });
  clientWs.binaryType = "nodebuffer";
  await new Promise((resolve, reject) => {
    clientWs.once("open", resolve);
    clientWs.once("error", reject);
  });
  clientWs.send(JSON.stringify({ type: "resize", cols: 100, rows: 40 }));
  clientWs.send(Buffer.from("buffered-keystroke", "utf8"), { binary: true });

  const workspaceReceived = [];
  const workerWs = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/workspace/${workspaceId}/worker`, {
    headers: {
      authorization: `Bearer ${workspaceToken}`,
      "x-rudder-machine-id": "machine-current",
    },
  });
  workerWs.binaryType = "nodebuffer";
  workerWs.on("message", (data, isBinary) => {
    workspaceReceived.push({ text: Buffer.from(data).toString("utf8"), isBinary });
  });
  await new Promise((resolve, reject) => {
    workerWs.once("open", resolve);
    workerWs.once("error", reject);
  });
  await waitFor(() => workspaceReceived.some((item) => item.text.includes("buffered-keystroke")));
  assert.ok(workspaceReceived.some((item) => !item.isBinary && item.text.includes('"resize"')));
  assert.ok(workspaceReceived.some((item) => item.isBinary && item.text === "buffered-keystroke"));

  // Historical PTY output is sent as one bounded replay frame instead of
  // thousands of per-write WebSocket frames.
  // clientWs is the live controller and receives every forwarded worker frame,
  // so use its byte count to know when the relay has ingested all 320 chunks —
  // a fixed sleep races under parallel-suite load and live frames then leak
  // into the replay count below.
  let liveBytes = 0;
  const countLiveBytes = (data, isBinary) => {
    if (isBinary) liveBytes += data.length;
  };
  clientWs.on("message", countLiveBytes);
  for (let index = 0; index < 320; index += 1) {
    workerWs.send(Buffer.alloc(1024, index % 255), { binary: true });
  }
  await waitFor(() => liveBytes >= 320 * 1024, { timeout: 15000 });
  clientWs.off("message", countLiveBytes);
  const replayFrames = [];
  const replayClient = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/workspace/${workspaceId}/attach`, {
    headers: { authorization: `Bearer ${cliToken}` },
  });
  replayClient.binaryType = "nodebuffer";
  replayClient.on("message", (data, isBinary) => {
    if (isBinary) replayFrames.push(Buffer.from(data));
  });
  await new Promise((resolve, reject) => {
    replayClient.once("open", resolve);
    replayClient.once("error", reject);
  });
  await waitFor(() => replayFrames.length > 0);
  await sleep(100);
  assert.equal(replayFrames.length, 1, "replay is coalesced into one frame");
  assert.ok(replayFrames[0].length <= 256 * 1024, "replay stays within its byte budget");

  ws.close();
  clientWs.close();
  replayClient.close();
  workerWs.close();
});

test("secrets vault round trip and latency-probe relay", async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-test-"));
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
      SLACK_BOT_TOKEN: "",
      RUDDER_SECRETS_KEY: Buffer.alloc(32, 7).toString("base64"),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let serverLog = "";
  child.stdout.on("data", (d) => { serverLog += d.toString(); });
  child.stderr.on("data", (d) => { serverLog += d.toString(); });
  t.after(async () => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });

  await waitFor(async () => {
    const res = await fetch(`${base}/health`).catch(() => null);
    return res && res.ok;
  });
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  const cliToken = "rdr_test_cli_token2";
  const workerToken = "rdrw_test_worker_token2";
  const workspaceId = "workspace-secrets-test";
  const accountId = "acct-secrets";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(cliToken), accountId, "test@example.com", now, now);
  db.prepare(`
    insert into rudder_workspaces (
      id, account_id, workspace_key, repo_name, status, machine_id, machine_state,
      worker_token_hash, last_activity_at, created_at, updated_at
    ) values (?,?,?,?,?,?,?,?,?,?,?)
  `).run(
    workspaceId, accountId, "workspace-secrets-key", "repo", "running", "machine-1", "started",
    tokenHash(workerToken), now, now, now,
  );
  db.close();

  const authHeaders = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };

  // health advertises the vault
  const health = await (await fetch(`${base}/health`)).json();
  assert.equal(health.secrets, true, "vault configured");

  // PUT env + file secrets, then list (metadata only, never values).
  const putEnv = await fetch(`${base}/api/rudder/secrets/item`, {
    method: "PUT",
    headers: authHeaders,
    body: JSON.stringify({
      name: "ANTHROPIC_API_KEY",
      kind: "env",
      valueBase64: Buffer.from("sk-ant-test-value").toString("base64"),
    }),
  });
  assert.equal(putEnv.status, 200, `put env (log: ${serverLog})`);
  const putFile = await fetch(`${base}/api/rudder/secrets/item`, {
    method: "PUT",
    headers: authHeaders,
    body: JSON.stringify({
      name: "~/.claude/.credentials.json",
      kind: "file",
      filePath: "~/.claude/.credentials.json",
      valueBase64: Buffer.from('{"token":"abc"}').toString("base64"),
    }),
  });
  assert.equal(putFile.status, 200);
  // Blocked path parts are refused server-side.
  const putBlocked = await fetch(`${base}/api/rudder/secrets/item`, {
    method: "PUT",
    headers: authHeaders,
    body: JSON.stringify({
      name: "~/.ssh/id_rsa",
      kind: "file",
      valueBase64: Buffer.from("nope").toString("base64"),
    }),
  });
  assert.equal(putBlocked.status, 400, "refuses .ssh paths");

  const listRes = await fetch(`${base}/api/rudder/secrets`, { headers: authHeaders });
  assert.equal(listRes.status, 200);
  const listBody = await listRes.json();
  assert.equal(listBody.secrets.length, 2);
  assert.ok(!JSON.stringify(listBody).includes("sk-ant-test-value"), "list never leaks values");

  // Worker fetch decrypts with the worker bearer.
  const workerRes = await fetch(`${base}/api/rudder/workspace/${workspaceId}/secrets`, {
    headers: { authorization: `Bearer ${workerToken}` },
  });
  assert.equal(workerRes.status, 200);
  const workerBody = await workerRes.json();
  assert.equal(workerBody.env.ANTHROPIC_API_KEY, "sk-ant-test-value");
  assert.equal(workerBody.files.length, 1);
  assert.equal(
    Buffer.from(workerBody.files[0].contentBase64, "base64").toString("utf8"),
    '{"token":"abc"}',
  );
  // Account CLI tokens must NOT work on the worker endpoint and vice versa.
  const wrongBearer = await fetch(`${base}/api/rudder/workspace/${workspaceId}/secrets`, {
    headers: { authorization: `Bearer ${cliToken}` },
  });
  assert.notEqual(wrongBearer.status, 200);

  // DELETE removes and bumps the version.
  const del = await fetch(`${base}/api/rudder/secrets/item?name=ANTHROPIC_API_KEY`, {
    method: "DELETE",
    headers: authHeaders,
  });
  assert.equal(del.status, 200);
  const afterDelete = await (await fetch(`${base}/api/rudder/secrets`, { headers: authHeaders })).json();
  assert.equal(afterDelete.secrets.length, 1);

  // Latency probe: a client "probe" text frame relays to the worker, whose
  // "probe-reply" relays back — mirroring supervisor.mjs handleControl.
  const workerWs = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/workspace/${workspaceId}/worker`, {
    headers: { authorization: `Bearer ${workerToken}`, "x-rudder-machine-id": "machine-1" },
  });
  workerWs.binaryType = "nodebuffer";
  workerWs.on("message", (data, isBinary) => {
    if (isBinary) return;
    let payload = null;
    try { payload = JSON.parse(Buffer.from(data).toString("utf8")); } catch { return; }
    if (payload?.type === "probe") {
      workerWs.send(JSON.stringify({ type: "probe-reply", id: payload.id ?? null }));
    }
  });
  await new Promise((resolve, reject) => {
    workerWs.once("open", resolve);
    workerWs.once("error", reject);
  });

  const clientWs = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/workspace/${workspaceId}/attach`, {
    headers: { authorization: `Bearer ${cliToken}` },
  });
  clientWs.binaryType = "nodebuffer";
  const replies = [];
  clientWs.on("message", (data, isBinary) => {
    if (isBinary) return;
    try { replies.push(JSON.parse(Buffer.from(data).toString("utf8"))); } catch { /* ignore */ }
  });
  await new Promise((resolve, reject) => {
    clientWs.once("open", resolve);
    clientWs.once("error", reject);
  });
  clientWs.send(JSON.stringify({ type: "probe", id: 42 }));
  await waitFor(() => replies.some((reply) => reply.type === "probe-reply" && reply.id === 42));

  workerWs.close();
  clientWs.close();
});
