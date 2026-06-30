// Process-level Rudder Cloud contract test.
//
// This starts the compiled control plane, seeds two isolated accounts, connects
// a worker over the real WebSocket bridge, and drives the public commands
// through dist/index.js. It catches drift between src/cloud.ts and
// cloud/src/server.ts that endpoint-only tests cannot see.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

const execFileAsync = promisify(execFile);
const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cloudDir = path.join(repoRoot, "cloud");
const cliPath = path.join(repoRoot, "dist", "index.js");
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
      const address = server.address();
      assert.ok(address && typeof address === "object");
      server.close(() => resolve(address.port));
    });
  });
}

async function waitFor(fn, { timeout = 8_000, interval = 50 } = {}) {
  const startedAt = Date.now();
  for (;;) {
    try {
      const value = await fn();
      if (value) return value;
    } catch {
      // The server and socket are intentionally racing us during startup.
    }
    if (Date.now() - startedAt > timeout) {
      throw new Error("waitFor timed out");
    }
    await sleep(interval);
  }
}

function insertAccount(db, { accountId, cliToken, email, sailId, workerToken }) {
  const now = new Date().toISOString();
  db.prepare(
    "insert into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(cliToken), accountId, email, now, now);
  db.prepare(
    `insert into rudder_sails
       (id, account_id, status, runtime, repo_name, task, worker_token_hash, created_at, updated_at)
     values (?,?,?,?,?,?,?,?,?)`,
  ).run(sailId, accountId, "running", "byo-vm", "fixture-repo", "fixture task", tokenHash(workerToken), now, now);
}

test("shipped CLI drives an isolated cloud sail through the real control plane", async (t) => {
  const port = await freePort();
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-cli-e2e-"));
  const dataDir = path.join(tempDir, "data");
  const homeDir = path.join(tempDir, "home");
  const dbPath = path.join(dataDir, "state.sqlite");
  const baseUrl = `http://127.0.0.1:${port}`;
  fs.mkdirSync(homeDir, { recursive: true });

  const server = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    env: {
      ...process.env,
      PORT: String(port),
      BETTER_AUTH_URL: baseUrl,
      BETTER_AUTH_SECRET: "test-secret-test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      SLACK_BOT_TOKEN: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let serverLog = "";
  server.stdout.on("data", (chunk) => { serverLog += chunk.toString(); });
  server.stderr.on("data", (chunk) => { serverLog += chunk.toString(); });

  const sockets = [];
  t.after(() => {
    for (const socket of sockets) socket.terminate();
    server.kill("SIGKILL");
    fs.rmSync(tempDir, { recursive: true, force: true });
  });

  await waitFor(async () => (await fetch(`${baseUrl}/health`)).ok);
  await waitFor(() => fs.existsSync(dbPath));

  const own = {
    accountId: "account-a",
    cliToken: "rdr_cli_account_a",
    email: "a@example.com",
    sailId: "cloud-owned-sail",
    workerToken: "rdrw_worker_account_a",
  };
  const other = {
    accountId: "account-b",
    cliToken: "rdr_cli_account_b",
    email: "b@example.com",
    sailId: "cloud-other-sail",
    workerToken: "rdrw_worker_account_b",
  };
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  insertAccount(db, own);
  insertAccount(db, other);
  db.close();

  const received = [];
  const worker = new WebSocket(`${baseUrl.replace(/^http/, "ws")}/api/rudder/sail/${own.sailId}/worker`, {
    headers: { authorization: `Bearer ${own.workerToken}` },
  });
  sockets.push(worker);
  worker.binaryType = "nodebuffer";
  worker.on("message", (data, isBinary) => {
    if (isBinary && Buffer.isBuffer(data)) received.push(data.toString("utf8"));
  });
  await new Promise((resolve, reject) => {
    worker.once("open", resolve);
    worker.once("error", reject);
  });
  worker.send(Buffer.from("worker-ready-marker\r\n"), { binary: true });

  const cliEnv = {
    ...process.env,
    HOME: homeDir,
    RUDDER_HOME: path.join(homeDir, ".rudder"),
    RUDDER_CLOUD_URL: baseUrl,
    RUDDER_CLOUD_TOKEN: own.cliToken,
    RUDDER_CLOUD_NO_ATTACH: "1",
    RUDDER_DISABLE_AUTO_UPDATE: "1",
    RUDDER_DISABLE_UPDATE_CHECK: "1",
  };
  const runCli = async (...args) => {
    try {
      const result = await execFileAsync(process.execPath, [cliPath, ...args, "--json"], {
        cwd: repoRoot,
        env: cliEnv,
        timeout: 10_000,
      });
      return JSON.parse(result.stdout.trim());
    } catch (error) {
      const stderr = error && typeof error === "object" && "stderr" in error ? error.stderr : "";
      throw new Error(`CLI failed: ${stderr || error}\ncontrol-plane log:\n${serverLog}`);
    }
  };

  const listed = await runCli("cloud", "list");
  assert.equal(listed.sails.length, 1, "only the caller's tenant is listed");
  assert.equal(listed.sails[0].id, own.sailId);
  assert.ok(!JSON.stringify(listed).includes(other.sailId), "another tenant never leaks into output");

  const crossTenant = await fetch(`${baseUrl}/api/rudder/sail/${other.sailId}/output`, {
    headers: { authorization: `Bearer ${own.cliToken}` },
  });
  assert.equal(crossTenant.status, 404, "known ids from another tenant remain inaccessible");

  const talked = await runCli("cloud", "talk", own.sailId, "continue", "the", "audit");
  assert.equal(talked.delivered, true);
  await waitFor(() => received.some((chunk) => chunk.includes("continue the audit\r")));

  const output = await runCli("cloud", "output", own.sailId);
  assert.equal(output.connected, true);
  assert.match(output.output, /worker-ready-marker/);

  // A reconnect replaces the old worker. The old socket's delayed close event
  // must not tell attached clients that the newly connected worker is gone.
  const statusStates = [];
  const attached = new WebSocket(
    `${baseUrl.replace(/^http/, "ws")}/api/rudder/sail/${own.sailId}/attach`,
    { headers: { authorization: `Bearer ${own.cliToken}` } },
  );
  sockets.push(attached);
  attached.on("message", (data, isBinary) => {
    if (isBinary) return;
    try {
      const value = JSON.parse(data.toString());
      if (value.type === "status") statusStates.push(value.state);
    } catch {
      // PTY text frames are not status messages.
    }
  });
  await new Promise((resolve, reject) => {
    attached.once("open", resolve);
    attached.once("error", reject);
  });
  await waitFor(() => statusStates.includes("worker-connected"));

  const replacement = new WebSocket(
    `${baseUrl.replace(/^http/, "ws")}/api/rudder/sail/${own.sailId}/worker`,
    { headers: { authorization: `Bearer ${own.workerToken}` } },
  );
  sockets.push(replacement);
  await new Promise((resolve, reject) => {
    replacement.once("open", resolve);
    replacement.once("error", reject);
  });
  await sleep(150);
  assert.equal(statusStates.at(-1), "worker-connected", "stale close does not mask the replacement worker");

  const stopped = await runCli("cloud", "stop", own.sailId);
  assert.equal(stopped.id, own.sailId);
  assert.equal(stopped.machineState, "stopped");
});
