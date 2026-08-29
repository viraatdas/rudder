#!/usr/bin/env node
// Rudder cloud worker supervisor.
// 1. Stages the snapshot into /workspace.
// 2. Spawns `rudder` (or `rudder codex --worktree --json "$task"`) under a PTY.
// 3. Bridges PTY stdin/stdout to a control-plane WebSocket so a remote
//    `rudder cloud attach <id>` session can drive the worker live.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { StringDecoder } from "node:string_decoder";
import { WebSocket } from "ws";
import pty from "node-pty";

import { countActiveAgents } from "./active-runs.mjs";

const cloudUrl = (process.env.RUDDER_CLOUD_URL || "").trim();
const sailId = (process.env.RUDDER_SAIL_ID || "").trim();
const workspaceId = (process.env.RUDDER_WORKSPACE_ID || "").trim();
const workerToken = (process.env.RUDDER_WORKER_TOKEN || "").trim();
const snapshotUrl = (process.env.RUDDER_SNAPSHOT_URL || "").trim();
const gitRemote = (process.env.RUDDER_GIT_REMOTE || "").trim();
const gitRef = (process.env.RUDDER_GIT_REF || "").trim();
const repoName = (process.env.RUDDER_REPO_NAME || "repo").trim() || "repo";
const task = process.env.RUDDER_TASK || "";
const flyMachineId = (process.env.FLY_MACHINE_ID || "").trim();

const isWorkspaceMode = Boolean(workspaceId);
const sessionId = isWorkspaceMode ? workspaceId : sailId;
const sessionKind = isWorkspaceMode ? "workspace" : "sail";

if (!sessionId) {
  console.error("RUDDER_WORKSPACE_ID or RUDDER_SAIL_ID is required");
  process.exit(2);
}

// Vault secrets come FIRST: $HOME lives on the ephemeral rootfs, so credential
// files must be re-written on every boot (cold or warm restart alike), and git
// credentials must exist before a clone-mode workspace can reach its remote.
// Values are fetched over the worker's authenticated channel instead of Fly
// machine env, which is readable via the Fly API.
const vaultSecrets = await fetchVaultSecrets();
if (vaultSecrets) {
  writeVaultFiles(vaultSecrets.files);
  configureGitCredentials(vaultSecrets);
}

if (alreadyStaged()) {
  console.log(`Rudder worker re-using staged workspace`);
  restoreSelectedHome();
  chdirToStagedWorkdir();
  if (gitRemote) {
    // Volume-backed clone persists across restarts; refresh remote refs so the
    // agent starts from a current view of origin.
    shSoft("git fetch --all -q");
  }
} else if (gitRemote) {
  // Cloud-native workspace: clone straight from the git remote — full history,
  // real origin, no snapshot upload involved.
  stageGitClone(gitRemote, gitRef);
  markStaged();
} else {
  // Fly Machines disks are ephemeral across stop+start, so the staged marker
  // is missing on every cold boot. Try fetching a freshly signed snapshot URL
  // from the control plane first, because the URL injected via env is signed
  // at machine-create time and expires after one hour.
  const freshUrl = await freshSnapshotUrl();
  const effectiveSnapshotUrl = freshUrl || snapshotUrl;
  if (!effectiveSnapshotUrl) {
    console.error("RUDDER_SNAPSHOT_URL is required for first start (and control-plane refresh failed)");
    process.exit(2);
  }
  if (freshUrl) {
    console.log("Using freshly signed snapshot URL from control plane.");
  } else {
    console.log("Falling back to snapshot URL from env (control-plane refresh unavailable).");
  }
  stageSnapshot(effectiveSnapshotUrl);
  markStaged();
}

const cwd = process.cwd();
console.log(`Rudder worker ready in ${cwd}`);

const capturedEnv = loadCapturedEnv();
if (Object.keys(capturedEnv).length > 0) {
  console.log(`Loaded ${Object.keys(capturedEnv).length} captured env var(s) from local snapshot.`);
}

const command = "rudder";
// Cloud task sails run Claude Code by default: claude-code is installed in the
// worker image and the snapshot carries its auth. Codex is NOT usable here —
// Rudder's pinned Codex fork has no managed linux/x64 binary, so `rudder codex`
// exits 1 on boot. Override with RUDDER_CLOUD_BACKEND only if a working codex
// binary is provided via RUDDER_CODEX_BIN.
const cloudBackend = (process.env.RUDDER_CLOUD_BACKEND || "claude").trim() || "claude";
const args = isWorkspaceMode
  ? []
  : task ? [cloudBackend, "--worktree", task] : [];

// Build the agent's environment from a COPY with the control-plane secrets removed.
// The supervisor already captured these into module-level consts at startup, so the
// spawned coding agent never needs them — and must not see them: a steered or
// otherwise compromised agent could read the worker bearer token / presigned snapshot
// URL straight out of its own environment and exfiltrate them. (We mutate childEnv,
// not process.env, so the supervisor's own WebSocket auth is unaffected.)
const childEnv = {
  ...process.env,
  ...capturedEnv,
  // Vault env overrides snapshot-captured env: the vault is rotatable and
  // fresher than whatever was frozen into the snapshot at attach time.
  ...(vaultSecrets?.env ?? {}),
  TERM: "xterm-256color",
  COLORTERM: "truecolor",
  RUDDER_HEADLESS: "0",
  RUDDER_CLOUD_WORKER: "1",
  RUDDER_DISABLE_AUTO_UPDATE: "1",
  RUDDER_DISABLE_UPDATE_CHECK: "1",
  RUDDER_DISABLE_ONBOARD_INSTALL: "1",
  // Rudder's pinned Codex fork has no linux/x64 binary, so any codex-binary
  // resolution (e.g. when the snapshot carries codex auth) would otherwise crash
  // even a claude run. Point it at the stock @openai/codex installed in the image
  // so resolution always succeeds; the default backend is still claude.
  RUDDER_CODEX_BIN: process.env.RUDDER_CODEX_BIN || "codex",
};
for (const secret of ["RUDDER_WORKER_TOKEN", "RUDDER_SNAPSHOT_URL", "RUDDER_CLOUD_TOKEN"]) {
  delete childEnv[secret];
}

const term = pty.spawn(command, args, {
  name: "xterm-256color",
  cols: 120,
  rows: 32,
  cwd,
  env: childEnv,
});

let ws = null;
let wsReadyPromise = connect();
let lastReportedState = "running";
let exited = false;
let heartbeatTimer = setInterval(reportHeartbeat, 30000);
const inputDecoder = new StringDecoder("utf8");
const pendingOutput = [];
let pendingOutputBytes = 0;
const MAX_PENDING_OUTPUT_BYTES = 256 * 1024;
reportHeartbeat();

term.onData((data) => {
  process.stdout.write(data);
  const chunk = Buffer.from(data, "utf8");
  const socket = ws;
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(chunk, { binary: true });
  } else {
    bufferPendingOutput(chunk);
  }
});

term.onExit(({ exitCode, signal }) => {
  exited = true;
  clearInterval(heartbeatTimer);
  const state = exitCode === 0 ? "completed" : "failed";
  reportDone(state, exitCode ?? (signal ? 128 + signal : 1));
  const socket = ws;
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify({ type: "exit", code: exitCode, signal: signal ?? null }));
    socket.close(1000, "worker-exit");
  }
  setTimeout(() => process.exit(exitCode ?? 1), 250).unref();
});

process.on("SIGTERM", () => {
  if (!exited) {
    term.kill("SIGTERM");
  }
});
process.on("SIGINT", () => {
  if (!exited) {
    term.kill("SIGINT");
  }
});

function connect() {
  if (!cloudUrl || !sessionId || !workerToken) {
    return Promise.resolve(null);
  }
  const wsUrl = cloudUrl.replace(/^http/, "ws").replace(/\/$/, "")
    + `/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/worker`;
  return new Promise((resolve) => {
    let connected = false;
    const socket = new WebSocket(wsUrl, {
      headers: {
        authorization: `Bearer ${workerToken}`,
        ...(flyMachineId ? { "x-rudder-machine-id": flyMachineId } : {}),
      },
    });
    socket.binaryType = "nodebuffer";
    socket.on("open", () => {
      connected = true;
      ws = socket;
      try { socket._socket?.setNoDelay?.(true); } catch { /* ignore */ }
      socket.send(JSON.stringify({
        type: "hello",
        cols: term.cols,
        rows: term.rows,
        sessionKind,
        sessionId,
      }));
      flushPendingOutput(socket);
      resolve(socket);
    });
    socket.on("message", (data, isBinary) => {
      if (exited) {
        return;
      }
      if (isBinary && Buffer.isBuffer(data)) {
        term.write(inputDecoder.write(data));
        return;
      }
      const text = Buffer.isBuffer(data) ? data.toString("utf8") : String(data);
      handleControl(text);
    });
    const reschedule = () => {
      if (exited) {
        return;
      }
      ws = null;
      setTimeout(() => { wsReadyPromise = connect(); }, 2000);
    };
    socket.on("close", reschedule);
    socket.on("error", () => {
      if (!connected) {
        try { socket.terminate(); } catch { /* ignore */ }
        resolve(null);
      }
    });
  });
}

function bufferPendingOutput(chunk) {
  if (chunk.length >= MAX_PENDING_OUTPUT_BYTES) {
    pendingOutput.length = 0;
    pendingOutput.push(chunk.subarray(chunk.length - MAX_PENDING_OUTPUT_BYTES));
    pendingOutputBytes = MAX_PENDING_OUTPUT_BYTES;
    return;
  }
  pendingOutput.push(chunk);
  pendingOutputBytes += chunk.length;
  while (pendingOutputBytes > MAX_PENDING_OUTPUT_BYTES && pendingOutput.length > 1) {
    pendingOutputBytes -= pendingOutput.shift().length;
  }
}

function flushPendingOutput(socket) {
  if (pendingOutputBytes === 0 || socket.readyState !== WebSocket.OPEN) {
    return;
  }
  socket.send(Buffer.concat(pendingOutput, pendingOutputBytes), { binary: true });
  pendingOutput.length = 0;
  pendingOutputBytes = 0;
}

function handleControl(text) {
  let payload = null;
  try {
    payload = JSON.parse(text);
  } catch {
    return;
  }
  if (!payload || typeof payload !== "object") {
    return;
  }
  if (payload.type === "resize" && Number.isFinite(payload.cols) && Number.isFinite(payload.rows)) {
    const cols = Math.max(20, Math.min(500, Math.floor(payload.cols)));
    const rows = Math.max(5, Math.min(200, Math.floor(payload.rows)));
    try {
      term.resize(cols, rows);
    } catch {
      // ignore
    }
    return;
  }
  if (payload.type === "signal" && typeof payload.name === "string") {
    try {
      term.kill(payload.name);
    } catch {
      // ignore
    }
    return;
  }
  if (payload.type === "probe") {
    // Latency probe: reply immediately so `rudder cloud attach --latency-probe`
    // can measure the pure transport round trip, excluding the TUI render path.
    const socket = ws;
    if (socket && socket.readyState === WebSocket.OPEN) {
      try {
        socket.send(JSON.stringify({ type: "probe-reply", id: payload.id ?? null }));
      } catch {
        // ignore
      }
    }
  }
}

function loadCapturedEnv() {
  const candidates = [
    "/workspace/unpacked/env/cloud-env.json",
    path.join(process.cwd(), ".rudder", "cloud-env.json"),
  ];
  for (const candidate of candidates) {
    if (!fs.existsSync(candidate)) {
      continue;
    }
    try {
      const raw = fs.readFileSync(candidate, "utf8");
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const out = {};
        for (const [k, v] of Object.entries(parsed)) {
          if (typeof k === "string" && typeof v === "string") {
            out[k] = v;
          }
        }
        return out;
      }
    } catch (error) {
      console.error(`Failed to load captured env ${candidate}: ${error.message}`);
    }
  }
  return {};
}

function stagedMarkerPath() {
  return path.join("/workspace", ".rudder-staged.json");
}

function alreadyStaged() {
  try {
    const raw = fs.readFileSync(stagedMarkerPath(), "utf8");
    const parsed = JSON.parse(raw);
    return typeof parsed?.workdir === "string" && fs.existsSync(parsed.workdir);
  } catch {
    return false;
  }
}

function chdirToStagedWorkdir() {
  const raw = fs.readFileSync(stagedMarkerPath(), "utf8");
  const parsed = JSON.parse(raw);
  process.chdir(parsed.workdir);
}

function markStaged() {
  try {
    const payload = { workdir: process.cwd(), stagedAt: new Date().toISOString() };
    fs.writeFileSync(stagedMarkerPath(), JSON.stringify(payload));
  } catch {
    // ignore
  }
}

async function freshSnapshotUrl() {
  const id = (process.env.RUDDER_WORKSPACE_ID || "").trim();
  const token = (process.env.RUDDER_WORKER_TOKEN || "").trim();
  const base = (process.env.RUDDER_CLOUD_URL || "").trim();
  if (!id || !token || !base) return null;
  try {
    const url = `${base.replace(/\/$/, "")}/api/rudder/workspace/${encodeURIComponent(id)}/snapshot-url`;
    const res = await fetch(url, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (!res.ok) {
      console.warn(`snapshot-url refresh: HTTP ${res.status}`);
      return null;
    }
    const data = await res.json();
    return typeof data?.url === "string" ? data.url : null;
  } catch (error) {
    console.warn(`snapshot-url refresh failed: ${error?.message || error}`);
    return null;
  }
}

async function fetchVaultSecrets() {
  if (!cloudUrl || !sessionId || !workerToken) {
    return null;
  }
  const url = `${cloudUrl.replace(/\/$/, "")}/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/secrets`;
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      const res = await fetch(url, { headers: { authorization: `Bearer ${workerToken}` } });
      if (res.ok) {
        const data = await res.json();
        const env = data && typeof data.env === "object" && data.env && !Array.isArray(data.env) ? data.env : {};
        const files = Array.isArray(data?.files) ? data.files : [];
        if (Object.keys(env).length > 0 || files.length > 0) {
          console.log(`Loaded ${Object.keys(env).length} env and ${files.length} file secret(s) from the cloud vault.`);
        }
        return { version: Number(data?.version) || 0, env, files };
      }
      console.warn(`vault secrets fetch: HTTP ${res.status}`);
      if (res.status === 401 || res.status === 403 || res.status === 404) {
        break;
      }
    } catch (error) {
      console.warn(`vault secrets fetch failed: ${error?.message || error}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 1000 * (attempt + 1)));
  }
  console.warn("Continuing WITHOUT vault secrets; legacy snapshot credentials (if any) still apply.");
  return null;
}

function writeVaultFiles(files) {
  const home = os.homedir() || process.env.HOME || "/root";
  for (const file of files || []) {
    if (typeof file?.path !== "string" || typeof file?.contentBase64 !== "string") {
      continue;
    }
    if (!file.path.startsWith("~/")) {
      continue;
    }
    const segments = file.path.slice(2).split("/");
    if (segments.some((part) => part === "" || part === "." || part === "..")) {
      continue;
    }
    const target = path.join(home, ...segments);
    try {
      fs.mkdirSync(path.dirname(target), { recursive: true, mode: 0o700 });
      fs.writeFileSync(target, Buffer.from(file.contentBase64, "base64"), { mode: file.mode || 0o600 });
    } catch (error) {
      console.warn(`vault file ${file.path}: ${error?.message || error}`);
    }
  }
}

function configureGitCredentials(vault) {
  const home = os.homedir() || process.env.HOME || "/root";
  const ghHosts = path.join(home, ".config", "gh", "hosts.yml");
  if (fs.existsSync(ghHosts)) {
    // Wire git's credential helper through gh; also gives the agent `gh pr create`.
    shSoft("command -v gh >/dev/null 2>&1 && gh auth setup-git 2>/dev/null");
    return;
  }
  const token = vault?.env?.GITHUB_TOKEN || vault?.env?.GH_TOKEN || "";
  if (!token) {
    if (gitRemote) {
      console.warn("No GitHub credentials in the vault; private clones/pushes will fail. Run `rudder cloud secrets sync` locally.");
    }
    return;
  }
  try {
    // $HOME is ephemeral rootfs, so this token never outlives the boot.
    fs.writeFileSync(path.join(home, ".git-credentials"), `https://x-access-token:${token}@github.com\n`, { mode: 0o600 });
    shSoft("git config --global credential.helper store");
  } catch (error) {
    console.warn(`git credential setup failed: ${error?.message || error}`);
  }
}

function stageGitClone(remote, ref) {
  fs.mkdirSync("/workspace", { recursive: true });
  process.chdir("/workspace");
  const workdir = path.join("/workspace", repoName);
  console.log(`Cloning ${remote}${ref ? ` (${ref})` : ""}...`);
  try {
    sh(`git clone ${ref ? `--branch ${shQuote(ref)} ` : ""}${shQuote(remote)} ${shQuote(workdir)}`);
  } catch (error) {
    console.error(`git clone failed: ${error?.message || error}`);
    reportDone("failed", 2);
    process.exit(2);
  }
  process.chdir(workdir);
  // Only set identity when the vault didn't already deliver a .gitconfig.
  shSoft('git config user.email >/dev/null 2>&1 || git config user.email "rudder-cloud@local"');
  shSoft('git config user.name >/dev/null 2>&1 || git config user.name "Rudder Cloud"');
}

function stageSnapshot(downloadUrl) {
  fs.mkdirSync("/workspace", { recursive: true });
  process.chdir("/workspace");
  console.log("Downloading Rudder snapshot...");
  sh(`curl -fsSL ${shQuote(downloadUrl)} -o snapshot.tgz`);
  fs.mkdirSync("unpacked", { recursive: true });
  sh("tar -xzf snapshot.tgz -C unpacked");
  restoreSelectedHome();
  let workdir;
  if (fs.existsSync("unpacked/repo")) {
    workdir = path.join("/workspace", repoName);
    fs.mkdirSync(workdir, { recursive: true });
    sh(`cp -R unpacked/repo/. ${shQuote(workdir + "/")}`);
  } else {
    workdir = "/workspace/unpacked";
  }
  process.chdir(workdir);
  if (!fs.existsSync(".git")) {
    console.log("Initializing cloud git baseline...");
    sh("git init -q");
    sh('git config user.email "rudder-cloud@local"');
    sh('git config user.name "Rudder Cloud"');
    sh("git add -A");
    sh('git commit -qm "rudder cloud baseline" || true');
  }
  stageMigratedAgents(workdir);
}

function restoreSelectedHome() {
  const source = "/workspace/unpacked/home";
  if (!fs.existsSync(source)) {
    return;
  }
  console.log("Restoring selected HOME config...");
  const home = os.homedir() || process.env.HOME || "/root";
  fs.mkdirSync(home, { recursive: true });
  for (const entry of fs.readdirSync(source)) {
    const destination = path.join(home, entry);
    fs.cpSync(path.join(source, entry), destination, {
      recursive: true,
      force: true,
      preserveTimestamps: true,
    });
    removeAppleDoubleFiles(destination);
  }
}

function removeAppleDoubleFiles(directory) {
  if (!fs.existsSync(directory) || !fs.statSync(directory).isDirectory()) {
    return;
  }
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.name.startsWith("._")) {
      fs.rmSync(entryPath, { recursive: true, force: true });
    } else if (entry.isDirectory()) {
      removeAppleDoubleFiles(entryPath);
    }
  }
}

function stageMigratedAgents(workdir) {
  const migrationPath = "/workspace/unpacked/migration.json";
  if (!fs.existsSync(migrationPath)) {
    return;
  }
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(migrationPath, "utf8"));
  } catch (error) {
    console.error(`Migration manifest unreadable: ${error.message}`);
    return;
  }
  const agents = Array.isArray(manifest?.agents) ? manifest.agents : [];
  if (agents.length === 0) {
    return;
  }
  console.log(`Restoring ${agents.length} migrated agent(s)...`);
  const home = os.homedir() || process.env.HOME || "/root";
  const placed = [];
  for (const agent of agents) {
    if (!agent || typeof agent !== "object") {
      continue;
    }
    if (typeof agent.runId !== "string") {
      continue;
    }
    const cloudWorktree = typeof agent.cloudWorktreeRelativePath === "string"
      ? agent.cloudWorktreeRelativePath
      : null;
    if (!cloudWorktree) {
      continue;
    }
    const stagedWorktree = path.join("/workspace/unpacked/migrated-worktrees", agent.runId);
    if (fs.existsSync(stagedWorktree)) {
      fs.mkdirSync(cloudWorktree, { recursive: true });
      sh(`cp -R ${shQuote(stagedWorktree + "/.")} ${shQuote(cloudWorktree + "/")}`);
      if (!fs.existsSync(path.join(cloudWorktree, ".git"))) {
        shSoft(`cd ${shQuote(cloudWorktree)} && git init -q && git config user.email rudder-cloud@local && git config user.name "Rudder Cloud" && git add -A && git commit -qm "rudder cloud baseline (migrated agent ${agent.runId})"`);
      }
    } else {
      fs.mkdirSync(cloudWorktree, { recursive: true });
    }

    const sessionId = typeof agent.sessionId === "string" && agent.sessionId.trim().length > 0
      ? agent.sessionId.trim()
      : "";
    const sessionPathHint = typeof agent.sessionJsonlSnapshotPath === "string"
      ? agent.sessionJsonlSnapshotPath.trim()
      : "";
    let sessionPlaced = "";
    if (sessionId && sessionPathHint) {
      const stagedJsonl = path.join("/workspace/unpacked", sessionPathHint);
      if (fs.existsSync(stagedJsonl)) {
        const encoded = encodeClaudeProjectsCwd(cloudWorktree);
        const claudeProjectDir = path.join(home, ".claude", "projects", encoded);
        fs.mkdirSync(claudeProjectDir, { recursive: true });
        const dest = path.join(claudeProjectDir, `${sessionId}.jsonl`);
        sh(`cp ${shQuote(stagedJsonl)} ${shQuote(dest)}`);
        sessionPlaced = sessionId;
      } else {
        console.log(`Migrated agent ${agent.runId}: jsonl missing in snapshot, falling back to fresh restart`);
      }
    }
    placed.push({
      runId: agent.runId,
      sessionId: sessionPlaced,
      worktreePath: cloudWorktree,
      worktreeBranch: agent.worktreeBranch || null,
      task: agent.task || "",
      taskSummary: agent.taskSummary || "",
      backend: agent.backend || "claude",
      freshPrompt: typeof agent.freshPrompt === "string" ? agent.freshPrompt : "",
    });
  }

  if (placed.length === 0) {
    return;
  }

  for (const entry of placed) {
    const runJsonPath = path.join(workdir, ".rudder", "runs", entry.runId, "run.json");
    if (!fs.existsSync(runJsonPath)) {
      continue;
    }
    let record;
    try {
      record = JSON.parse(fs.readFileSync(runJsonPath, "utf8"));
    } catch {
      continue;
    }
    record.repoRoot = workdir;
    record.worktree = {
      ...(record.worktree || {}),
      enabled: true,
      path: entry.worktreePath,
      branch: entry.worktreeBranch || record.worktree?.branch,
    };
    if (entry.sessionId) {
      record.session = {
        ...(record.session || {}),
        nativeSessionId: entry.sessionId,
      };
    }
    record.status = record.status === "completed" || record.status === "merged" ? record.status : "running";
    record.migration = {
      origin: "local",
      pendingResume: Boolean(entry.sessionId),
      pendingFresh: !entry.sessionId,
      sessionId: entry.sessionId || null,
      backend: entry.backend,
      freshPrompt: entry.freshPrompt || null,
      migratedAt: new Date().toISOString(),
    };
    fs.writeFileSync(runJsonPath, JSON.stringify(record, null, 2));
  }

  const summary = {
    version: 1,
    createdAt: new Date().toISOString(),
    agents: placed,
  };
  const summaryDir = path.join(workdir, ".rudder");
  fs.mkdirSync(summaryDir, { recursive: true });
  fs.writeFileSync(path.join(summaryDir, "migration.json"), JSON.stringify(summary, null, 2));
  console.log(`Migrated ${placed.length} agent(s); dashboard will resume them on startup.`);
}

function encodeClaudeProjectsCwd(absolutePath) {
  return String(absolutePath).replace(/[^A-Za-z0-9-]/g, "-");
}

function heartbeatUrl() {
  return `${cloudUrl.replace(/\/$/, "")}/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/heartbeat`;
}

// Count the rudder runs that are ACTIVELY WORKING in this worker (the remote
// dashboard/worker writes <repo>/.rudder/runs/<id>/run.json for each agent).
// The control plane uses this as the busy signal for idle sweeps: a workspace
// whose agents are mid-task must never be stopped just because the user's
// laptop disconnected — heartbeats alone only prove the machine is alive, not
// that it is doing something. 0 when nothing rudder-shaped is running (or the
// state is unreadable — a broken read must read as idle, not busy).
function reportHeartbeat() {
  if (!cloudUrl || !sessionId || !workerToken) {
    return;
  }
  fetch(heartbeatUrl(), {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${workerToken}`,
    },
    body: JSON.stringify({
      state: lastReportedState,
      machineId: flyMachineId || undefined,
      activeAgents: countActiveAgents(),
    }),
  }).catch(() => undefined);
}

function reportDone(state, code) {
  lastReportedState = state;
  if (!cloudUrl || !sessionId || !workerToken) {
    return;
  }
  // best-effort fire-and-forget; we exit shortly after
  fetch(heartbeatUrl(), {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${workerToken}`,
    },
    body: JSON.stringify({ state, exitCode: code, machineId: flyMachineId || undefined }),
  }).catch(() => undefined);
}

function shSoft(cmd) {
  spawnSync("sh", ["-c", cmd], { stdio: "inherit" });
}

function sh(cmd) {
  const result = spawnSync("sh", ["-c", cmd], { stdio: "inherit" });
  if (result.status !== 0) {
    throw new Error(`command failed (${result.status}): ${cmd}`);
  }
}

function shQuote(value) {
  return `'${String(value).replace(/'/g, "'\\''")}'`;
}
