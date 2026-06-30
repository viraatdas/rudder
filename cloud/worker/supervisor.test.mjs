import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { after, before, test } from "node:test";
import { WebSocketServer } from "ws";
import {
  BoundedByteQueue,
  buildChildEnv,
  filterCapturedEnv,
  isPathInside,
  parseControlMessage,
  reconnectDelay,
  sanitizeRepoName,
} from "./supervisor-lib.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const supervisorPath = path.join(here, "supervisor.mjs");
const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-worker-test-"));
const workspaceRoot = path.join(tempRoot, "workspace");
const agentHome = path.join(tempRoot, "home");
const fakeBin = path.join(tempRoot, "bin");
const snapshotStage = path.join(tempRoot, "snapshot-stage");
const snapshotPath = path.join(tempRoot, "snapshot.tgz");

let server;
let serverBaseUrl;
let websocketOutput = "";
let delayUpgrades = true;
let heartbeatBodies = [];
let finalHeartbeatResponseAt = 0;
let outputAtFinalHeartbeat = "";

before(async () => {
  fs.mkdirSync(path.join(snapshotStage, "repo"), { recursive: true });
  fs.mkdirSync(path.join(snapshotStage, "env"), { recursive: true });
  fs.writeFileSync(path.join(snapshotStage, "repo", "README.md"), "cloud snapshot\n");
  fs.writeFileSync(path.join(snapshotStage, "env", "cloud-env.json"), JSON.stringify({
    SAFE_CAPTURED_TOKEN: "captured-ok",
    NODE_OPTIONS: "--definitely-invalid",
    RUDDER_WORKER_TOKEN: "snapshot-override",
  }));
  const tar = spawnSyncChecked("tar", ["-czf", snapshotPath, "-C", snapshotStage, "."]);
  assert.equal(tar.code, 0, tar.stderr);

  fs.mkdirSync(fakeBin, { recursive: true });
  const fakeRudder = path.join(fakeBin, "rudder");
  fs.writeFileSync(fakeRudder, `#!/bin/sh
printf 'ARGS:'
for arg in "$@"; do printf '[%s]' "$arg"; done
printf '\\nCAPTURED=%s WORKER_TOKEN=%s NODE_OPTIONS=%s\\n' "\${SAFE_CAPTURED_TOKEN:-}" "\${RUDDER_WORKER_TOKEN:-}" "\${NODE_OPTIONS:-}"
if [ "\${FAKE_SLEEP:-0}" = 1 ]; then
  trap 'exit 0' INT TERM
  while :; do sleep 1; done
fi
exit "\${FAKE_EXIT_CODE:-0}"
`);
  fs.chmodSync(fakeRudder, 0o755);

  const wss = new WebSocketServer({ noServer: true });
  wss.on("connection", (socket) => {
    socket.on("message", (data, isBinary) => {
      if (isBinary) websocketOutput += Buffer.from(data).toString("utf8");
    });
  });
  server = http.createServer((request, response) => {
    if (request.method === "GET" && /^\/api\/rudder\/(?:workspace|sail)\/ws-test\/snapshot-url$/.test(request.url)) {
      sendJson(response, 200, { url: `${serverBaseUrl}/snapshot.tgz`, expiresInSeconds: 3600 });
      return;
    }
    if (request.method === "GET" && request.url === "/snapshot.tgz") {
      response.writeHead(200, { "content-type": "application/gzip" });
      fs.createReadStream(snapshotPath).pipe(response);
      return;
    }
    if (request.method === "POST" && /^\/api\/rudder\/(?:workspace|sail)\/ws-test\/heartbeat$/.test(request.url)) {
      let body = "";
      request.setEncoding("utf8");
      request.on("data", (chunk) => { body += chunk; });
      request.on("end", () => {
        const parsed = JSON.parse(body);
        heartbeatBodies.push(parsed);
        const finish = () => {
          if (parsed.state === "failed" || parsed.state === "completed") {
            finalHeartbeatResponseAt = Date.now();
          }
          sendJson(response, 200, { ok: true });
        };
        if (parsed.state === "failed" || parsed.state === "completed") {
          outputAtFinalHeartbeat = websocketOutput;
          setTimeout(finish, 350);
        } else {
          finish();
        }
      });
      return;
    }
    response.writeHead(404).end();
  });
  server.on("upgrade", (request, socket, head) => {
    const accept = () => wss.handleUpgrade(request, socket, head, (websocket) => {
      wss.emit("connection", websocket, request);
    });
    if (delayUpgrades) setTimeout(accept, 250);
    else accept();
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  serverBaseUrl = `http://127.0.0.1:${address.port}`;
});

after(async () => {
  await new Promise((resolve) => server.close(resolve));
  fs.rmSync(tempRoot, { recursive: true, force: true });
});

test("worker utility boundaries sanitize paths, env, controls, and buffers", () => {
  assert.equal(sanitizeRepoName("../../unsafe repo"), "unsafe-repo");
  assert.equal(sanitizeRepoName("..."), "repo");
  assert.equal(isPathInside("/workspace", "/workspace/repo"), true);
  assert.equal(isPathInside("/workspace", "/workspace-escape"), false);

  assert.deepEqual(filterCapturedEnv({
    OK_TOKEN: "yes",
    NODE_OPTIONS: "bad",
    LD_PRELOAD: "bad",
    "NOT=AN=ENV": "bad",
    NUL_VALUE: "bad\0value",
  }), { OK_TOKEN: "yes" });
  assert.deepEqual(buildChildEnv(
    { PATH: "/bin", RUDDER_WORKER_TOKEN: "secret", FLY_API_TOKEN: "fly" },
    { PATH: "/evil", SAFE_TOKEN: "ok" },
  ), { PATH: "/bin", SAFE_TOKEN: "ok" });

  assert.deepEqual(parseControlMessage('{"type":"resize","cols":999,"rows":1}'), {
    type: "resize", cols: 500, rows: 5,
  });
  assert.deepEqual(parseControlMessage('{"type":"signal","name":"SIGTERM"}'), {
    type: "signal", name: "SIGTERM",
  });
  assert.equal(parseControlMessage('{"type":"signal","name":"SIGKILL"}'), null);
  assert.equal(reconnectDelay(0, () => 0.5), 1000);
  assert.equal(reconnectDelay(20, () => 0.5), 30000);

  const queue = new BoundedByteQueue(5);
  queue.push(Buffer.from("abc"));
  queue.push(Buffer.from("def"));
  assert.equal(queue.bytes, 3);
  assert.equal(queue.droppedBytes, 3);
  assert.equal(queue.shift().toString(), "def");
});

test("worker stages, relays startup output, preserves task argv, and awaits final heartbeat", async () => {
  const startedAt = Date.now();
  const result = await runSupervisor({ FAKE_EXIT_CODE: "7" });
  const closedAt = Date.now();
  assert.equal(result.code, 7, result.stderr || result.stdout);
  assert.match(result.stdout, /Rudder worker ready/);
  assert.match(result.stdout, /CAPTURED=captured-ok WORKER_TOKEN= NODE_OPTIONS=/);
  assert.match(websocketOutput, /ARGS:\[claude\]\[--worktree\]\[--\]\[--model should remain task text\]/);
  assert.ok(heartbeatBodies.some((body) => body.state === "running"));
  assert.ok(heartbeatBodies.some((body) => body.state === "failed" && body.exitCode === 7));
  assert.match(outputAtFinalHeartbeat, /CAPTURED=captured-ok/);
  assert.ok(finalHeartbeatResponseAt >= startedAt);
  assert.ok(closedAt >= finalHeartbeatResponseAt, "worker exited before final heartbeat response");
  assert.equal(fs.readFileSync(path.join(workspaceRoot, "unsafe-repo", "README.md"), "utf8"), "cloud snapshot\n");
  assert.equal(fs.existsSync(path.join(workspaceRoot, "snapshot.tgz")), false);
  assert.equal(fs.existsSync(path.join(workspaceRoot, "unpacked")), false);
});

test("intentional supervisor shutdown is not reported as task failure", async () => {
  heartbeatBodies = [];
  delayUpgrades = false;
  const child = startSupervisor({ FAKE_SLEEP: "1" });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  await waitUntil(() => heartbeatBodies.some((body) => body.state === "running"), 5_000);
  child.kill("SIGTERM");
  const { code } = await waitForClose(child, 10_000);
  assert.equal(code, 143, stderr || stdout);
  assert.equal(heartbeatBodies.some((body) => body.state === "failed"), false);
});

function startSupervisor(extraEnv = {}) {
  return spawn(process.execPath, [supervisorPath], {
    cwd: here,
    env: {
      ...process.env,
      PATH: `${fakeBin}${path.delimiter}${process.env.PATH}`,
      HOME: agentHome,
      RUDDER_AGENT_HOME: agentHome,
      RUDDER_WORKSPACE_ROOT: workspaceRoot,
      RUDDER_SAIL_ID: "ws-test",
      RUDDER_CLOUD_URL: serverBaseUrl,
      RUDDER_WORKER_TOKEN: "rdrw_test-token",
      RUDDER_SNAPSHOT_URL: "http://127.0.0.1:1/expired",
      RUDDER_REPO_NAME: "../../unsafe repo",
      RUDDER_TASK: "--model should remain task text",
      RUDDER_WORKER_COMMAND: path.join(fakeBin, "rudder"),
      RUDDER_WORKER_TEST_PIPE: "1",
      ...extraEnv,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
}

async function runSupervisor(extraEnv) {
  const child = startSupervisor(extraEnv);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const result = await waitForClose(child, 15_000);
  return { ...result, stdout, stderr };
}

function waitForClose(child, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`child did not exit within ${timeoutMs}ms`));
    }, timeoutMs);
    child.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    child.once("close", (code, signal) => {
      clearTimeout(timeout);
      resolve({ code, signal });
    });
  });
}

async function waitUntil(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("condition timed out");
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

function sendJson(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

function spawnSyncChecked(program, args) {
  const result = spawnSync(program, args, { encoding: "utf8" });
  return {
    code: result.status,
    stdout: result.stdout || "",
    stderr: result.stderr || result.error?.message || "",
  };
}
