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

  ws.close();
});
