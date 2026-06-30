#!/usr/bin/env node
// Rudder Cloud worker supervisor. It stages a snapshot, runs Rudder under a
// PTY, and bridges that PTY to the control plane. The supervisor stays root in
// the official image while the PTY child runs as uid/gid 1500. This keeps the
// worker bearer token and signed snapshot URL outside the coding agent's uid.

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import pty from "node-pty";
import { WebSocket } from "ws";
import {
  BoundedByteQueue,
  buildChildEnv,
  filterCapturedEnv,
  isPathInside,
  parseControlMessage,
  reconnectDelay,
  safePathSegment,
  sanitizeRepoName,
} from "./supervisor-lib.mjs";

const env = process.env;
const cloudUrl = (env.RUDDER_CLOUD_URL || "").trim();
const sailId = (env.RUDDER_SAIL_ID || "").trim();
const workspaceId = (env.RUDDER_WORKSPACE_ID || "").trim();
const workerToken = (env.RUDDER_WORKER_TOKEN || "").trim();
const snapshotUrl = (env.RUDDER_SNAPSHOT_URL || "").trim();
const repoName = sanitizeRepoName(env.RUDDER_REPO_NAME);
const task = env.RUDDER_TASK || "";
const flyMachineId = (env.FLY_MACHINE_ID || "").trim();
const workspaceRoot = path.resolve(env.RUDDER_WORKSPACE_ROOT || "/workspace");
const agentUid = boundedInteger(env.RUDDER_AGENT_UID, 1500, 1, 2 ** 31 - 1);
const agentGid = boundedInteger(env.RUDDER_AGENT_GID, 1500, 1, 2 ** 31 - 1);
const agentHome = path.resolve(env.RUDDER_AGENT_HOME || "/home/rudder");
const supervisorDir = path.join(workspaceRoot, ".rudder-supervisor");
const maxPendingOutputBytes = boundedInteger(
  env.RUDDER_WORKER_OUTPUT_BUFFER_BYTES,
  1024 * 1024,
  64 * 1024,
  8 * 1024 * 1024,
);

const isWorkspaceMode = Boolean(workspaceId);
const sessionId = isWorkspaceMode ? workspaceId : sailId;
const sessionKind = isWorkspaceMode ? "workspace" : "sail";
const canDropPrivileges = typeof process.getuid === "function" && process.getuid() === 0;

let term = null;
let ws = null;
let connectSocket = null;
let reconnectTimer = null;
let keepaliveTimer = null;
let reconnectAttempt = 0;
let heartbeatTimer = null;
let forceExitTimer = null;
let sendInFlight = false;
let intentionalSignal = null;
let terminalExited = false;
let finalizing = false;
let stdoutBackpressured = false;
let lastReportedState = "starting";
let loggedDroppedBytes = 0;
const outbound = new BoundedByteQueue(maxPendingOutputBytes);
const unprivilegedEnv = buildChildEnv(env, {}, {
  HOME: agentHome,
  USER: "rudder",
  LOGNAME: "rudder",
  SHELL: "/bin/bash",
});

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.once(signal, () => beginShutdown(signal));
}

try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  console.error(`Rudder worker startup failed: ${message}`);
  if (!intentionalSignal) {
    await reportState("failed", { exitCode: 1, phase: "startup" }, 2);
  }
  process.exit(intentionalSignal ? signalExitCode(intentionalSignal) : 1);
}

async function main() {
  if (!sessionId) {
    throw new Error("RUDDER_WORKSPACE_ID or RUDDER_SAIL_ID is required");
  }
  ensureWorkspaceRoot();
  void reportState("starting");

  if (alreadyStaged()) {
    console.log("Rudder worker re-using staged workspace");
    chdirToStagedWorkdir();
  } else {
    const freshUrl = await freshSnapshotUrl();
    const effectiveSnapshotUrl = freshUrl || snapshotUrl;
    if (!effectiveSnapshotUrl) {
      throw new Error("RUDDER_SNAPSHOT_URL is required for first start (and control-plane refresh failed)");
    }
    console.log(freshUrl
      ? "Using a freshly signed snapshot URL from the control plane."
      : "Using the snapshot URL supplied at machine creation.");
    stageSnapshot(effectiveSnapshotUrl);
    markStaged();
  }

  const cwd = process.cwd();
  console.log(`Rudder worker ready in ${cwd}`);
  const capturedEnv = loadCapturedEnv();
  if (Object.keys(capturedEnv).length > 0) {
    console.log(`Loaded ${Object.keys(capturedEnv).length} captured env var(s) from local snapshot.`);
  }

  const cloudBackend = (env.RUDDER_CLOUD_BACKEND || "claude").trim() || "claude";
  if (!new Set(["claude", "codex", "acpx"]).has(cloudBackend)) {
    throw new Error(`Unsupported RUDDER_CLOUD_BACKEND: ${cloudBackend}`);
  }
  // `--` is essential: a task beginning with --model/--cwd/etc. is user text,
  // not a Rudder supervisor flag.
  const args = isWorkspaceMode
    ? []
    : task ? [cloudBackend, "--worktree", "--", task] : [];
  const childEnv = buildChildEnv(env, capturedEnv, {
    HOME: agentHome,
    USER: "rudder",
    LOGNAME: "rudder",
    SHELL: "/bin/bash",
    TERM: "xterm-256color",
    COLORTERM: "truecolor",
    RUDDER_HEADLESS: "0",
    RUDDER_CLOUD_WORKER: "1",
    RUDDER_DISABLE_AUTO_UPDATE: "1",
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_DISABLE_ONBOARD_INSTALL: "1",
    RUDDER_CODEX_BIN: env.RUDDER_CODEX_BIN || "codex",
  });

  const ptyOptions = {
    name: "xterm-256color",
    cols: 120,
    rows: 32,
    cwd,
    env: childEnv,
    ...(canDropPrivileges ? { uid: agentUid, gid: agentGid } : {}),
  };
  if (!canDropPrivileges) {
    console.warn("Worker supervisor is not root; PTY credential isolation is unavailable.");
  }
  const rudderCommand = (env.RUDDER_WORKER_COMMAND || "rudder").trim() || "rudder";
  term = env.RUDDER_WORKER_TEST_PIPE === "1"
    ? spawnPipeTerminal(rudderCommand, args, ptyOptions)
    : pty.spawn(rudderCommand, args, ptyOptions);

  term.onData((data) => {
    if (!stdoutBackpressured && !process.stdout.write(data)) {
      stdoutBackpressured = true;
      process.stdout.once("drain", () => { stdoutBackpressured = false; });
    }
    outbound.push(Buffer.from(data, "utf8"));
    flushOutbound();
  });
  term.onExit((event) => {
    void finalizeExit(event);
  });

  connect();
  lastReportedState = "running";
  void reportState("running");
  heartbeatTimer = setInterval(() => void reportState(lastReportedState), 30_000);
  heartbeatTimer.unref?.();
}

function connect() {
  if (
    !cloudUrl
    || !workerToken
    || (terminalExited && (!finalizing || outbound.length === 0))
    || ws?.readyState === WebSocket.OPEN
    || connectSocket?.readyState === WebSocket.CONNECTING
  ) {
    return;
  }
  let wsUrl;
  try {
    const url = new URL(cloudUrl);
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new Error("RUDDER_CLOUD_URL must use http or https");
    }
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.pathname = `/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/worker`;
    url.search = "";
    url.hash = "";
    wsUrl = url.toString();
  } catch (error) {
    console.error(`Worker relay URL is invalid: ${error instanceof Error ? error.message : String(error)}`);
    return;
  }

  const socket = new WebSocket(wsUrl, {
    headers: { authorization: `Bearer ${workerToken}` },
    handshakeTimeout: 10_000,
    maxPayload: 1024 * 1024,
  });
  connectSocket = socket;
  socket.binaryType = "nodebuffer";
  let alive = true;

  socket.on("open", () => {
    if (terminalExited && outbound.length === 0) {
      socket.close(1000, "worker-exit");
      return;
    }
    connectSocket = null;
    ws = socket;
    reconnectAttempt = 0;
    alive = true;
    socket.send(JSON.stringify({
      type: "hello",
      cols: term?.cols || 120,
      rows: term?.rows || 32,
      sessionKind,
      sessionId,
    }));
    keepaliveTimer = setInterval(() => {
      if (socket.readyState !== WebSocket.OPEN) return;
      if (!alive) {
        socket.terminate();
        return;
      }
      alive = false;
      try { socket.ping(); } catch { socket.terminate(); }
    }, 20_000);
    keepaliveTimer.unref?.();
    flushOutbound();
  });
  socket.on("pong", () => { alive = true; });
  socket.on("message", (data, isBinary) => {
    if (terminalExited || !term) return;
    if (isBinary) {
      const input = Buffer.isBuffer(data) ? data : Buffer.from(data);
      term.write(input.toString("utf8"));
      return;
    }
    handleControl(Buffer.isBuffer(data) ? data.toString("utf8") : String(data));
  });
  socket.on("unexpected-response", (_request, response) => {
    console.warn(`Worker relay rejected the connection with HTTP ${response.statusCode || "unknown"}.`);
  });
  socket.on("error", (error) => {
    if (!terminalExited && reconnectAttempt === 0) {
      console.warn(`Worker relay connection failed: ${error.message}`);
    }
  });
  socket.on("close", () => {
    if (keepaliveTimer) clearInterval(keepaliveTimer);
    keepaliveTimer = null;
    if (connectSocket === socket) connectSocket = null;
    if (ws === socket) ws = null;
    if (!terminalExited) scheduleReconnect();
  });
}

function scheduleReconnect() {
  if (reconnectTimer || terminalExited) return;
  const delayMs = reconnectDelay(reconnectAttempt++);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, delayMs);
}

function flushOutbound() {
  const socket = ws;
  if (!socket || socket.readyState !== WebSocket.OPEN || sendInFlight) return;
  const chunk = outbound.shift();
  if (!chunk) return;
  sendInFlight = true;
  socket.send(chunk, { binary: true }, (error) => {
    sendInFlight = false;
    if (error) {
      outbound.unshift(chunk);
      try { socket.terminate(); } catch { /* ignore */ }
      return;
    }
    if (outbound.droppedBytes > loggedDroppedBytes) {
      console.warn(`Worker relay dropped ${outbound.droppedBytes - loggedDroppedBytes} byte(s) of old output under backpressure.`);
      loggedDroppedBytes = outbound.droppedBytes;
    }
    flushOutbound();
  });
}

function handleControl(text) {
  const control = parseControlMessage(text);
  if (!control || !term) return;
  try {
    if (control.type === "resize") {
      term.resize(control.cols, control.rows);
    } else if (control.type === "signal") {
      term.kill(control.name);
    }
  } catch {
    // The PTY may have exited between parsing and dispatch.
  }
}

function beginShutdown(signal) {
  if (intentionalSignal) return;
  intentionalSignal = signal;
  if (!term || terminalExited) {
    process.exit(signalExitCode(signal));
  }
  try { term.kill(signal); } catch { /* finalize below */ }
  forceExitTimer = setTimeout(() => {
    try { term?.kill("SIGKILL"); } catch { /* ignore */ }
    process.exit(signalExitCode(signal));
  }, 8_000);
}

async function finalizeExit({ exitCode, signal }) {
  if (finalizing) return;
  finalizing = true;
  terminalExited = true;
  if (heartbeatTimer) clearInterval(heartbeatTimer);
  if (reconnectTimer) clearTimeout(reconnectTimer);
  if (forceExitTimer) clearTimeout(forceExitTimer);
  const code = intentionalSignal
    ? signalExitCode(intentionalSignal)
    : exitCode ?? (signal ? 128 + signal : 1);
  const state = code === 0 ? "completed" : "failed";
  lastReportedState = state;

  // Intentional Fly/Docker stops are lifecycle operations, not task failures.
  // The control plane records the requested paused/stopped state itself.
  await waitForOutboundDrain(2_000);
  if (!intentionalSignal) {
    await reportState(state, { exitCode: code }, 3);
  }

  const socket = ws;
  if (socket?.readyState === WebSocket.OPEN) {
    try {
      socket.send(JSON.stringify({
        type: "exit",
        code,
        signal: intentionalSignal || signal || null,
      }));
      socket.close(1000, "worker-exit");
    } catch {
      // ignore
    }
  }
  process.exit(code);
}

async function waitForOutboundDrain(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while ((outbound.length > 0 || sendInFlight) && Date.now() < deadline) {
    if (!ws && !connectSocket && cloudUrl && workerToken) connect();
    flushOutbound();
    await delay(20);
  }
}

function ensureWorkspaceRoot() {
  fs.mkdirSync(workspaceRoot, { recursive: true });
  const workspaceStat = fs.lstatSync(workspaceRoot);
  if (!workspaceStat.isDirectory() || workspaceStat.isSymbolicLink()) {
    throw new Error("RUDDER_WORKSPACE_ROOT must be a real directory");
  }
  if (canDropPrivileges) {
    // Root-owned sticky workspace root: the agent can create worktrees, but
    // cannot replace the root-owned supervisor metadata directory.
    fs.chownSync(workspaceRoot, 0, 0);
    fs.chmodSync(workspaceRoot, 0o1777);
  }
  const existing = fs.lstatSync(supervisorDir, { throwIfNoEntry: false });
  if (existing && (!existing.isDirectory() || existing.isSymbolicLink())) {
    fs.rmSync(supervisorDir, { recursive: true, force: true });
  }
  fs.mkdirSync(supervisorDir, { recursive: true, mode: 0o700 });
  fs.chmodSync(supervisorDir, 0o700);
  if (canDropPrivileges) fs.chownSync(supervisorDir, 0, 0);
  fs.mkdirSync(agentHome, { recursive: true });
  const homeStat = fs.lstatSync(agentHome);
  if (!homeStat.isDirectory() || homeStat.isSymbolicLink()) {
    throw new Error("RUDDER_AGENT_HOME must be a real directory");
  }
  if (canDropPrivileges) fs.chownSync(agentHome, agentUid, agentGid);
}

function markerPath() {
  return path.join(supervisorDir, "staged.json");
}

function alreadyStaged() {
  try {
    const stat = fs.lstatSync(markerPath());
    if (!stat.isFile() || stat.isSymbolicLink() || (canDropPrivileges && stat.uid !== 0)) return false;
    const parsed = JSON.parse(fs.readFileSync(markerPath(), "utf8"));
    if (
      parsed?.sessionKind !== sessionKind
      || parsed?.sessionId !== sessionId
      || parsed?.repoName !== repoName
    ) return false;
    if (typeof parsed?.workdir !== "string") return false;
    const resolved = path.resolve(parsed.workdir);
    const workdirStat = fs.lstatSync(resolved);
    return isPathInside(workspaceRoot, resolved) && workdirStat.isDirectory() && !workdirStat.isSymbolicLink();
  } catch {
    // Upgrade path from the former agent-writable marker. Only accept the one
    // exact repository directory this machine is expected to use, then replace
    // it with root-owned supervisor metadata.
    try {
      const legacyPath = path.join(workspaceRoot, ".rudder-staged.json");
      const legacyStat = fs.lstatSync(legacyPath);
      if (!legacyStat.isFile() || legacyStat.isSymbolicLink()) return false;
      const parsed = JSON.parse(fs.readFileSync(legacyPath, "utf8"));
      const resolved = path.resolve(parsed?.workdir || "");
      const expected = path.join(workspaceRoot, repoName);
      const workdirStat = fs.lstatSync(resolved);
      if (resolved !== expected || !workdirStat.isDirectory() || workdirStat.isSymbolicLink()) return false;
      markStaged(resolved);
      return true;
    } catch {
      return false;
    }
  }
}

function chdirToStagedWorkdir() {
  const parsed = JSON.parse(fs.readFileSync(markerPath(), "utf8"));
  process.chdir(parsed.workdir);
}

function markStaged(workdir = process.cwd()) {
  const payload = {
    version: 2,
    sessionKind,
    sessionId,
    repoName,
    workdir,
    stagedAt: new Date().toISOString(),
  };
  writeFileAtomic(markerPath(), `${JSON.stringify(payload)}\n`, 0o600, false);
}

async function freshSnapshotUrl() {
  if (!workerToken || !cloudUrl) return null;
  try {
    const base = new URL(cloudUrl);
    base.pathname = `/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/snapshot-url`;
    base.search = "";
    base.hash = "";
    const response = await fetch(base, {
      headers: { authorization: `Bearer ${workerToken}` },
      signal: AbortSignal.timeout(8_000),
    });
    if (!response.ok) {
      console.warn(`snapshot-url refresh: HTTP ${response.status}`);
      return null;
    }
    const data = await response.json();
    return typeof data?.url === "string" && /^https?:\/\//.test(data.url) ? data.url : null;
  } catch (error) {
    console.warn(`snapshot-url refresh failed: ${error instanceof Error ? error.message : String(error)}`);
    return null;
  }
}

function stageSnapshot(downloadUrl) {
  const nonce = `${process.pid}-${Date.now()}`;
  const unpackRoot = path.join(workspaceRoot, `.rudder-stage-${nonce}`);
  const archivePath = path.join(unpackRoot, "snapshot.tgz");
  prepareAgentDir(unpackRoot);
  try {
    console.log("Downloading Rudder snapshot...");
    runCommand("curl", [
      "--fail",
      "--location",
      "--silent",
      "--show-error",
      "--retry", "4",
      "--retry-delay", "1",
      "--retry-all-errors",
      "--connect-timeout", "10",
      "--max-time", String(boundedInteger(env.RUDDER_SNAPSHOT_DOWNLOAD_TIMEOUT, 600, 30, 3600)),
      "--max-filesize", String(boundedInteger(env.RUDDER_SNAPSHOT_MAX_BYTES, 512 * 1024 * 1024, 1024, 2 ** 31 - 1)),
      "--output", archivePath,
      downloadUrl,
    ], { label: "snapshot download" });
    if (canDropPrivileges) fs.chownSync(archivePath, agentUid, agentGid);
    runCommand("tar", ["-xzf", archivePath, "-C", unpackRoot, "--no-same-owner"], {
      label: "snapshot extraction",
      asAgent: true,
    });
    fs.rmSync(archivePath, { force: true });

    const repoSource = path.join(unpackRoot, "repo");
    if (!safeExtractedEntry(unpackRoot, repoSource, "directory")) {
      throw new Error("snapshot is malformed: missing a safe repo/ directory");
    }
    restoreHome(unpackRoot);
    persistCapturedEnv(unpackRoot);
    const workdir = path.join(workspaceRoot, repoName);
    if (!isPathInside(workspaceRoot, workdir) || workdir === supervisorDir) {
      throw new Error("snapshot repository path is invalid");
    }
    fs.rmSync(workdir, { recursive: true, force: true });
    fs.renameSync(repoSource, workdir);
    process.chdir(workdir);
    initializeGitBaseline(workdir);
    stageMigratedAgents(workdir, unpackRoot);
    fs.rmSync(unpackRoot, { recursive: true, force: true });
  } catch (error) {
    fs.rmSync(unpackRoot, { recursive: true, force: true });
    throw error;
  }
}

function restoreHome(unpackRoot) {
  const source = path.join(unpackRoot, "home");
  if (!safeExtractedEntry(unpackRoot, source, "directory")) return;
  console.log("Restoring selected HOME config...");
  runCommand("cp", ["-a", `${source}/.`, `${agentHome}/`], {
    label: "HOME restore",
    asAgent: true,
  });
  runCommand("find", [agentHome, "-name", "._*", "-delete"], {
    label: "HOME metadata cleanup",
    asAgent: true,
    allowFailure: true,
  });
}

function persistCapturedEnv(unpackRoot) {
  const source = path.join(unpackRoot, "env", "cloud-env.json");
  if (!safeExtractedEntry(unpackRoot, source, "file")) return;
  // Parse and filter before persisting so a crafted snapshot cannot preserve
  // supervisor/runtime variables for a later warm start.
  try {
    const filtered = filterCapturedEnv(JSON.parse(fs.readFileSync(source, "utf8")));
    writeFileAtomic(
      path.join(supervisorDir, "cloud-env.json"),
      `${JSON.stringify(filtered)}\n`,
      0o600,
      false,
    );
  } catch (error) {
    console.warn(`Ignoring invalid captured environment: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function loadCapturedEnv() {
  const candidates = [
    { root: supervisorDir, path: path.join(supervisorDir, "cloud-env.json") },
    { root: path.join(workspaceRoot, "unpacked"), path: path.join(workspaceRoot, "unpacked", "env", "cloud-env.json") },
    { root: process.cwd(), path: path.join(process.cwd(), ".rudder", "cloud-env.json") },
  ];
  for (const candidate of candidates) {
    try {
      if (!safeExtractedEntry(candidate.root, candidate.path, "file")) continue;
      return filterCapturedEnv(JSON.parse(fs.readFileSync(candidate.path, "utf8")));
    } catch (error) {
      console.error(`Failed to load captured env ${candidate.path}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  return {};
}

function initializeGitBaseline(workdir) {
  console.log("Initializing cloud git baseline...");
  // Snapshot creation deliberately excludes Git internals. Discard any crafted
  // .git entry rather than trusting hooks/config supplied by an archive.
  fs.rmSync(path.join(workdir, ".git"), { recursive: true, force: true });
  runCommand("git", ["init", "-q"], { cwd: workdir, label: "git init", asAgent: true });
  runCommand("git", ["config", "user.email", "rudder-cloud@local"], { cwd: workdir, label: "git config", asAgent: true });
  runCommand("git", ["config", "user.name", "Rudder Cloud"], { cwd: workdir, label: "git config", asAgent: true });
  runCommand("git", ["add", "-A"], { cwd: workdir, label: "git add", asAgent: true });
  runCommand("git", ["commit", "-qm", "rudder cloud baseline"], {
    cwd: workdir,
    label: "git commit",
    asAgent: true,
    allowFailure: true,
  });
}

function stageMigratedAgents(workdir, unpackRoot) {
  const migrationPath = path.join(unpackRoot, "migration.json");
  if (!safeExtractedEntry(unpackRoot, migrationPath, "file")) return;
  let manifest;
  try {
    manifest = JSON.parse(fs.readFileSync(migrationPath, "utf8"));
  } catch (error) {
    console.error(`Migration manifest unreadable: ${error instanceof Error ? error.message : String(error)}`);
    return;
  }
  const agents = Array.isArray(manifest?.agents) ? manifest.agents : [];
  if (agents.length === 0) return;
  console.log(`Restoring ${agents.length} migrated agent(s)...`);
  const placed = [];
  const worktreeRoot = path.join(workspaceRoot, ".rudder-worktrees");
  prepareAgentDir(worktreeRoot);

  for (const agent of agents.slice(0, 100)) {
    const runId = safePathSegment(agent?.runId);
    if (!runId || typeof agent?.cloudWorktreeRelativePath !== "string") continue;
    const cloudWorktree = remapCloudPath(agent.cloudWorktreeRelativePath);
    if (!cloudWorktree || !isPathInside(worktreeRoot, cloudWorktree)) {
      console.warn(`Migrated agent ${runId}: rejected invalid worktree path`);
      continue;
    }
    const stagedWorktree = path.join(unpackRoot, "migrated-worktrees", runId);
    fs.rmSync(cloudWorktree, { recursive: true, force: true });
    prepareAgentDir(cloudWorktree);
    if (safeExtractedEntry(unpackRoot, stagedWorktree, "directory")) {
      runCommand("cp", ["-a", `${stagedWorktree}/.`, `${cloudWorktree}/`], {
        label: `migrated worktree ${runId}`,
        asAgent: true,
      });
      initializeGitBaseline(cloudWorktree);
    }

    const sessionIdValue = safePathSegment(agent.sessionId, 200) || "";
    const sessionHint = typeof agent.sessionJsonlSnapshotPath === "string"
      ? agent.sessionJsonlSnapshotPath.trim()
      : "";
    let sessionPlaced = "";
    if (sessionIdValue && sessionHint) {
      const stagedJsonl = path.resolve(unpackRoot, sessionHint);
      if (safeExtractedEntry(unpackRoot, stagedJsonl, "file")) {
        const projectDir = path.join(agentHome, ".claude", "projects", encodeClaudeProjectsCwd(cloudWorktree));
        prepareAgentDir(projectDir);
        const destination = path.join(projectDir, `${sessionIdValue}.jsonl`);
        fs.copyFileSync(stagedJsonl, destination);
        fs.chmodSync(destination, 0o600);
        if (canDropPrivileges) fs.chownSync(destination, agentUid, agentGid);
        sessionPlaced = sessionIdValue;
      } else {
        console.log(`Migrated agent ${runId}: session jsonl missing, falling back to a fresh restart`);
      }
    }
    placed.push({
      runId,
      sessionId: sessionPlaced,
      worktreePath: cloudWorktree,
      worktreeBranch: typeof agent.worktreeBranch === "string" ? agent.worktreeBranch : null,
      task: typeof agent.task === "string" ? agent.task : "",
      taskSummary: typeof agent.taskSummary === "string" ? agent.taskSummary : "",
      backend: typeof agent.backend === "string" ? agent.backend : "claude",
      freshPrompt: typeof agent.freshPrompt === "string" ? agent.freshPrompt : "",
    });
  }

  for (const entry of placed) {
    const runJsonPath = path.join(workdir, ".rudder", "runs", entry.runId, "run.json");
    if (!isPathInside(workdir, runJsonPath) || !fs.existsSync(runJsonPath)) continue;
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
      record.session = { ...(record.session || {}), nativeSessionId: entry.sessionId };
    }
    if (record.status !== "completed" && record.status !== "merged") record.status = "running";
    record.migration = {
      origin: "local",
      pendingResume: Boolean(entry.sessionId),
      pendingFresh: !entry.sessionId,
      sessionId: entry.sessionId || null,
      backend: entry.backend,
      freshPrompt: entry.freshPrompt || null,
      migratedAt: new Date().toISOString(),
    };
    writeFileAtomic(runJsonPath, `${JSON.stringify(record, null, 2)}\n`, 0o600, true);
  }

  if (placed.length > 0) {
    const summaryPath = path.join(workdir, ".rudder", "migration.json");
    prepareAgentDir(path.dirname(summaryPath));
    writeFileAtomic(summaryPath, `${JSON.stringify({
      version: 1,
      createdAt: new Date().toISOString(),
      agents: placed,
    }, null, 2)}\n`, 0o600, true);
    console.log(`Migrated ${placed.length} agent(s); dashboard will resume them on startup.`);
  }
}

function remapCloudPath(value) {
  const normalized = String(value).replaceAll("\\", "/");
  if (normalized === "/workspace") return workspaceRoot;
  if (normalized.startsWith("/workspace/")) {
    return path.resolve(workspaceRoot, normalized.slice("/workspace/".length));
  }
  if (!path.isAbsolute(normalized)) return path.resolve(workspaceRoot, normalized);
  return path.resolve(normalized);
}

function encodeClaudeProjectsCwd(absolutePath) {
  return String(absolutePath).replace(/[^A-Za-z0-9-]/g, "-");
}

async function reportState(state, extra = {}, attempts = 1) {
  lastReportedState = state;
  if (!cloudUrl || !workerToken || !sessionId) return false;
  let endpoint;
  try {
    endpoint = new URL(cloudUrl);
    endpoint.pathname = `/api/rudder/${sessionKind}/${encodeURIComponent(sessionId)}/heartbeat`;
    endpoint.search = "";
    endpoint.hash = "";
  } catch {
    return false;
  }
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const response = await fetch(endpoint, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${workerToken}`,
        },
        body: JSON.stringify({ state, machineId: flyMachineId || undefined, ...extra }),
        signal: AbortSignal.timeout(5_000),
      });
      if (response.ok) return true;
      if (response.status >= 400 && response.status < 500) return false;
    } catch {
      // retry below
    }
    if (attempt + 1 < attempts) await delay(200 * (attempt + 1));
  }
  return false;
}

function prepareAgentDir(directory) {
  const resolved = path.resolve(directory);
  const base = isPathInside(agentHome, resolved)
    ? agentHome
    : isPathInside(workspaceRoot, resolved)
      ? workspaceRoot
      : null;
  if (!base) throw new Error(`refusing to create agent directory outside managed roots: ${resolved}`);
  const baseStat = fs.lstatSync(base);
  if (!baseStat.isDirectory() || baseStat.isSymbolicLink()) {
    throw new Error(`managed root is not a real directory: ${base}`);
  }
  let current = base;
  const relative = path.relative(base, resolved);
  for (const segment of relative.split(path.sep).filter(Boolean)) {
    current = path.join(current, segment);
    const stat = fs.lstatSync(current, { throwIfNoEntry: false });
    if (!stat) {
      fs.mkdirSync(current);
    } else if (!stat.isDirectory() || stat.isSymbolicLink()) {
      throw new Error(`refusing to follow a managed-directory symlink: ${current}`);
    }
    if (canDropPrivileges) fs.chownSync(current, agentUid, agentGid);
  }
  if (canDropPrivileges && resolved !== workspaceRoot) fs.chownSync(resolved, agentUid, agentGid);
}

function safeExtractedEntry(root, candidate, expected) {
  try {
    if (!isPathInside(root, candidate)) return false;
    const stat = fs.lstatSync(candidate);
    if (stat.isSymbolicLink()) return false;
    if (expected === "file" && !stat.isFile()) return false;
    if (expected === "directory" && !stat.isDirectory()) return false;
    return isPathInside(fs.realpathSync(root), fs.realpathSync(candidate));
  } catch {
    return false;
  }
}

function writeFileAtomic(destination, contents, mode, agentOwned) {
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const temporary = `${destination}.${process.pid}.${Date.now()}.tmp`;
  try {
    fs.writeFileSync(temporary, contents, { mode });
    fs.chmodSync(temporary, mode);
    if (agentOwned && canDropPrivileges) fs.chownSync(temporary, agentUid, agentGid);
    fs.renameSync(temporary, destination);
  } finally {
    fs.rmSync(temporary, { force: true });
  }
}

function runCommand(program, args, options = {}) {
  const result = spawnSync(program, args, {
    cwd: options.cwd,
    stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
    encoding: options.capture ? "utf8" : undefined,
    env: options.asAgent ? unprivilegedEnv : env,
    ...(options.asAgent && canDropPrivileges ? { uid: agentUid, gid: agentGid } : {}),
  });
  if (result.error && !options.allowFailure) {
    throw new Error(`${options.label || program} failed: ${result.error.message}`);
  }
  if (result.status !== 0 && !options.allowFailure) {
    throw new Error(`${options.label || program} failed with exit code ${result.status ?? "unknown"}`);
  }
  return result;
}

// Deterministic test adapter for hosts where native node-pty cannot allocate a
// terminal (for example a sandboxed macOS CI runner). Production never sets
// RUDDER_WORKER_TEST_PIPE and always exercises node-pty.
function spawnPipeTerminal(program, args, options) {
  const child = spawn(program, args, {
    cwd: options.cwd,
    env: options.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const dataListeners = new Set();
  const exitListeners = new Set();
  const earlyData = [];
  let exitEvent = null;
  const emitData = (chunk) => {
    const text = chunk.toString("utf8");
    if (dataListeners.size === 0) earlyData.push(text);
    for (const listener of dataListeners) listener(text);
  };
  child.stdout.on("data", emitData);
  child.stderr.on("data", emitData);
  child.on("exit", (code, signal) => {
    const signalNumber = signal === "SIGINT" ? 2 : signal === "SIGTERM" ? 15 : undefined;
    exitEvent = { exitCode: code, signal: signalNumber };
    for (const listener of exitListeners) listener(exitEvent);
  });
  return {
    cols: options.cols,
    rows: options.rows,
    onData(listener) {
      dataListeners.add(listener);
      for (const text of earlyData.splice(0)) queueMicrotask(() => listener(text));
      return { dispose: () => dataListeners.delete(listener) };
    },
    onExit(listener) {
      exitListeners.add(listener);
      if (exitEvent) queueMicrotask(() => listener(exitEvent));
      return { dispose: () => exitListeners.delete(listener) };
    },
    write(value) { child.stdin.write(value); },
    resize() {},
    kill(signal) { child.kill(signal); },
  };
}

function signalExitCode(signal) {
  return signal === "SIGINT" ? 130 : 143;
}

function boundedInteger(value, fallback, min, max) {
  const parsed = Number.parseInt(String(value ?? ""), 10);
  return Number.isFinite(parsed) ? Math.max(min, Math.min(max, parsed)) : fallback;
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
