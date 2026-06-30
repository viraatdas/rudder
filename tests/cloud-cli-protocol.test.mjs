import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { WebSocketServer } from "ws";

const execFileAsync = promisify(execFile);
const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cliPath = path.join(repoRoot, "dist", "index.js");

async function listen(handler) {
  const server = http.createServer(handler);
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  return {
    server,
    url: `http://127.0.0.1:${address.port}`,
    close: async () => {
      server.closeAllConnections?.();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

async function jsonBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(Buffer.from(chunk));
  const text = Buffer.concat(chunks).toString("utf8");
  return text ? JSON.parse(text) : {};
}

function sendJson(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
  res.end(body);
}

function initRepo(root) {
  fs.mkdirSync(root, { recursive: true });
  execFileSync("git", ["init", "-q"], { cwd: root });
  execFileSync("git", ["config", "user.email", "cloud-test@example.com"], { cwd: root });
  execFileSync("git", ["config", "user.name", "Cloud Test"], { cwd: root });
}

function commitAll(root) {
  execFileSync("git", ["add", "-A"], { cwd: root });
  execFileSync("git", ["commit", "-qm", "fixture"], { cwd: root });
}

function cliEnv(home, cloudUrl, extra = {}) {
  return {
    ...process.env,
    HOME: home,
    RUDDER_HOME: path.join(home, ".rudder"),
    RUDDER_CLOUD_URL: cloudUrl,
    RUDDER_CLOUD_TOKEN: "rdr_cli_protocol_test",
    RUDDER_CLOUD_NO_ATTACH: "1",
    RUDDER_DISABLE_AUTO_UPDATE: "1",
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    ...extra,
  };
}

async function runCli(args, { cwd, env, timeout = 15_000 } = {}) {
  return await execFileAsync(process.execPath, [cliPath, ...args], {
    cwd: cwd ?? repoRoot,
    env,
    timeout,
    maxBuffer: 16 * 1024 * 1024,
  });
}

test("workspace fingerprint changes when an already-dirty file's bytes change", async (t) => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-fingerprint-"));
  const repo = path.join(temp, "repo");
  const home = path.join(temp, "home");
  fs.mkdirSync(home, { recursive: true });
  initRepo(repo);
  fs.writeFileSync(path.join(repo, "tracked.txt"), "committed\n");
  commitAll(repo);

  const requests = [];
  const fixture = await listen(async (req, res) => {
    if (req.url === "/health") return sendJson(res, 200, { ok: true });
    if (req.url === "/api/rudder/workspace/attach") {
      requests.push(await jsonBody(req));
      return sendJson(res, 200, { id: "workspace-fixture", status: "running", isNew: false });
    }
    sendJson(res, 404, { error: "not found" });
  });
  t.after(async () => {
    await fixture.close();
    fs.rmSync(temp, { recursive: true, force: true });
  });

  const env = cliEnv(home, fixture.url);
  fs.writeFileSync(path.join(repo, "tracked.txt"), "dirty-one\n");
  await runCli(["cloud", "workspace", "attach", "--json"], { cwd: repo, env });
  fs.writeFileSync(path.join(repo, "tracked.txt"), "dirty-two\n");
  await runCli(["cloud", "workspace", "attach", "--json"], { cwd: repo, env });

  assert.equal(requests.length, 2);
  assert.notEqual(requests[0].snapshotFingerprint, requests[1].snapshotFingerprint);
  assert.equal(requests[0].snapshot, undefined, "warm attach does not eagerly build a snapshot");
});

test("launch snapshot excludes .env files, oversized credential dumps, and unsafe HOME symlinks", async (t) => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-snapshot-"));
  const home = path.join(temp, "home");
  // Most real checkouts live under ~/projects or ~/worktrees. HOME bulk
  // exclusions must not be applied to repository files merely because of
  // their absolute parent directory.
  const repo = path.join(home, "projects", "repo");
  const outside = path.join(temp, "outside-token");
  fs.mkdirSync(path.join(home, "dotfiles"), { recursive: true });
  initRepo(repo);
  fs.writeFileSync(path.join(repo, "safe.txt"), "safe snapshot content\n");
  fs.writeFileSync(path.join(repo, ".env.staging"), "DATABASE_PASSWORD=do-not-upload\n");
  fs.writeFileSync(path.join(repo, ".envrc"), "export PRIVATE_TOKEN=do-not-upload\n");
  fs.mkdirSync(path.join(repo, "node_modules"), { recursive: true });
  fs.writeFileSync(path.join(repo, "node_modules", "do-not-upload.js"), "large generated dependency\n");
  commitAll(repo);
  fs.writeFileSync(
    path.join(home, ".profile"),
    `${"x".repeat(1024 * 1024 + 32)}\nAWS_SECRET_ACCESS_KEY=do-not-upload\n`,
  );
  fs.writeFileSync(outside, "outside secret\n");
  fs.symlinkSync(outside, path.join(home, ".npmrc"));
  fs.writeFileSync(path.join(home, "dotfiles", "gitconfig"), "[user]\n  name = Snapshot User\n");
  fs.symlinkSync(path.join(home, "dotfiles", "gitconfig"), path.join(home, ".gitconfig"));

  let launchBody;
  const fixture = await listen(async (req, res) => {
    if (req.url === "/api/rudder/sail/launch") {
      launchBody = await jsonBody(req);
      return sendJson(res, 200, { id: "snapshot-sail", status: "running", runtime: "fly" });
    }
    sendJson(res, 404, { error: "not found" });
  });
  t.after(async () => {
    await fixture.close();
    fs.rmSync(temp, { recursive: true, force: true });
  });

  const env = cliEnv(home, fixture.url);
  await runCli(["cloud", "launch", "snapshot", "audit", "--json"], { cwd: repo, env });
  assert.ok(launchBody?.snapshot?.base64);

  const archive = path.join(temp, "snapshot.tgz");
  const unpacked = path.join(temp, "unpacked");
  fs.writeFileSync(archive, Buffer.from(launchBody.snapshot.base64, "base64"));
  fs.mkdirSync(unpacked);
  await execFileAsync("tar", ["-xzf", archive, "-C", unpacked]);
  assert.equal(fs.readFileSync(path.join(unpacked, "repo", "safe.txt"), "utf8"), "safe snapshot content\n");
  assert.equal(fs.existsSync(path.join(unpacked, "repo", ".env.staging")), false);
  assert.equal(fs.existsSync(path.join(unpacked, "repo", ".envrc")), false);
  assert.equal(fs.existsSync(path.join(unpacked, "repo", "node_modules")), false);
  assert.equal(fs.existsSync(path.join(unpacked, "home", ".profile")), false);
  assert.equal(fs.existsSync(path.join(unpacked, "home", ".npmrc")), false);
  const stagedGitconfig = path.join(unpacked, "home", ".gitconfig");
  assert.equal(
    fs.existsSync(stagedGitconfig),
    true,
    `safe HOME link missing; manifest=${JSON.stringify(launchBody.snapshot.manifest)}; archive contained:\n${(await execFileAsync("tar", ["-tzf", archive])).stdout}`,
  );
  assert.equal(fs.lstatSync(stagedGitconfig).isSymbolicLink(), false, "safe HOME links are dereferenced");
  assert.match(fs.readFileSync(stagedGitconfig, "utf8"), /Snapshot User/);
});

test("cloud failures are bounded and login cannot silently succeed", async (t) => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-timeout-"));
  const home = path.join(temp, "home");
  fs.mkdirSync(home, { recursive: true });
  const fixture = await listen(async (req, res) => {
    if (req.url === "/api/rudder/sail") {
      res.writeHead(200, { "content-type": "application/json" });
      res.write('{"sails":');
      return;
    }
    sendJson(res, 404, { error: "not found" });
  });
  t.after(async () => {
    await fixture.close();
    fs.rmSync(temp, { recursive: true, force: true });
  });

  const timeoutEnv = cliEnv(home, fixture.url, { RUDDER_CLOUD_REQUEST_TIMEOUT_MS: "150" });
  await assert.rejects(
    runCli(["cloud", "list", "--json"], { env: timeoutEnv, timeout: 3_000 }),
    (error) => /timed out/.test(String(error.stderr)),
  );

  const poisonBin = path.join(temp, "bin");
  fs.mkdirSync(poisonBin);
  fs.writeFileSync(path.join(poisonBin, "tar"), "#!/bin/sh\necho SNAPSHOT-WORK-RAN >&2\nexit 99\n", { mode: 0o755 });
  const unauthenticatedEnv = cliEnv(home, fixture.url, {
    RUDDER_CLOUD_TOKEN: "",
    PATH: `${poisonBin}${path.delimiter}${process.env.PATH ?? ""}`,
  });
  await assert.rejects(
    runCli(["cloud", "launch", "should-fail-before-snapshot", "--json"], { env: unauthenticatedEnv }),
    (error) => /Not logged in to Rudder Cloud/.test(String(error.stderr))
      && !/SNAPSHOT-WORK-RAN/.test(String(error.stderr)),
  );

  const loginEnv = cliEnv(home, fixture.url, {
    RUDDER_CLOUD_TOKEN: "",
    RUDDER_SKIP_GH_CLI: "1",
    RUDDER_SKIP_GITHUB_DEVICE_LOGIN: "1",
  });
  await assert.rejects(
    runCli(["login", "--json"], { env: loginEnv }),
    (error) => /Could not log in to Rudder Cloud/.test(String(error.stderr)),
  );
  assert.equal(fs.existsSync(path.join(home, ".rudder", "cloud.json")), false);
});

test("talk handles a disconnected relay and delete falls back from sail to workspace", async (t) => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-actions-"));
  const home = path.join(temp, "home");
  fs.mkdirSync(home, { recursive: true });
  const seen = [];
  const fixture = await listen(async (req, res) => {
    seen.push(`${req.method} ${req.url}`);
    if (req.url === "/api/rudder/sail/gone/input") {
      await jsonBody(req);
      return sendJson(res, 409, { delivered: false, error: "instance is not connected" });
    }
    if (req.url === "/api/rudder/sail/loggy/output") {
      return sendJson(res, 200, { id: "loggy", connected: true, output: "worker log line\n" });
    }
    if (req.url === "/api/rudder/sail/workspace-1/delete") {
      await jsonBody(req);
      return sendJson(res, 404, { error: "sail not found" });
    }
    if (req.url === "/api/rudder/workspace/workspace-1/delete") {
      await jsonBody(req);
      return sendJson(res, 200, { ok: true, id: "workspace-1", deleted: true });
    }
    sendJson(res, 404, { error: "not found" });
  });
  t.after(async () => {
    await fixture.close();
    fs.rmSync(temp, { recursive: true, force: true });
  });
  const env = cliEnv(home, fixture.url);

  const talked = await runCli(["cloud", "talk", "gone", "hello", "--json"], { env });
  assert.deepEqual(JSON.parse(talked.stdout), { delivered: false, error: "instance is not connected" });
  const logs = await runCli(["cloud", "logs", "loggy", "--json"], { env });
  assert.match(JSON.parse(logs.stdout).output, /worker log line/);
  const deleted = await runCli(["cloud", "delete", "workspace-1", "--json"], { env });
  assert.equal(JSON.parse(deleted.stdout).deleted, true);
  assert.deepEqual(seen.slice(-2), [
    "POST /api/rudder/sail/workspace-1/delete",
    "POST /api/rudder/workspace/workspace-1/delete",
  ]);
});

test("attach streams binary output and terminates on the remote exit frame", async (t) => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-attach-"));
  const home = path.join(temp, "home");
  fs.mkdirSync(home, { recursive: true });
  const fixture = await listen((_req, res) => sendJson(res, 404, { error: "not found" }));
  const wss = new WebSocketServer({ noServer: true, perMessageDeflate: false });
  const stalledSockets = [];
  fixture.server.on("upgrade", (req, socket, head) => {
    assert.equal(req.headers.authorization, "Bearer rdr_cli_protocol_test");
    if (req.url?.includes("/stalled/")) {
      stalledSockets.push(socket);
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => wss.emit("connection", ws, req));
  });
  wss.on("connection", (ws) => {
    ws.send(Buffer.from("REMOTE-FRAME\n"), { binary: true });
    setTimeout(() => ws.send(JSON.stringify({ type: "exit", code: 0 })), 20);
  });
  t.after(async () => {
    for (const socket of stalledSockets) socket.destroy();
    wss.close();
    await fixture.close();
    fs.rmSync(temp, { recursive: true, force: true });
  });

  const result = await runCli(["cloud", "attach", "sail-1"], {
    env: cliEnv(home, fixture.url, { RUDDER_CLOUD_NO_ATTACH: "" }),
  });
  assert.match(result.stdout, /REMOTE-FRAME/);

  await assert.rejects(
    runCli(["cloud", "attach", "stalled"], {
      env: cliEnv(home, fixture.url, {
        RUDDER_CLOUD_NO_ATTACH: "",
        RUDDER_CLOUD_ATTACH_TIMEOUT_MS: "150",
      }),
      timeout: 3_000,
    }),
    (error) => /timed out/i.test(String(error.stderr)),
  );
});
