import { createHash, createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import fs from "node:fs/promises";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import os from "node:os";
import path from "node:path";
import { Readable } from "node:stream";
import { betterAuth } from "better-auth";
import Database from "better-sqlite3";
import { toNodeHandler } from "better-auth/node";
import { DeleteObjectCommand, GetObjectCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import type { Duplex } from "node:stream";
import { WebSocket, WebSocketServer, type RawData } from "ws";
import {
  formatOutputForSlack,
  escapeSlackText,
  parseSlackCommand,
  postSlackMessage,
  slackConfigFromEnv,
  SLACK_HELP_TEXT,
  verifySlackSignature,
} from "./slack.js";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

type JsonRecord = Record<string, Json>;

type DeviceLogin = {
  deviceCode: string;
  token?: string;
  accountId?: string;
  email?: string;
  expiresAt: number;
};

type GithubBrowserLogin = {
  githubDeviceCode: string;
  userCode: string;
  verificationUri: string;
  verificationUriComplete?: string;
  expiresAt: number;
  intervalMs: number;
  nextPollAt: number;
};

type SailStatus = "queued" | "running" | "paused" | "completed" | "failed";
type SailRuntime = "fly" | "byo-vm";

type Sail = {
  id: string;
  status: SailStatus;
  runtime: SailRuntime;
  repoName?: string;
  task?: string;
  branch?: string;
  machineId?: string;
  machineState?: string;
  snapshotKey?: string;
  lastActivityAt?: string;
  lastHeartbeatAt?: string;
  slackThreadTs?: string;
  createdAt: string;
  updatedAt: string;
  bootstrapCommand?: string;
};

type WorkspaceStatus = "queued" | "running" | "paused" | "stopped" | "failed";

type Workspace = {
  id: string;
  accountId: string;
  workspaceKey: string;
  repoName?: string;
  status: WorkspaceStatus;
  machineId?: string;
  machineState?: string;
  snapshotKey?: string;
  snapshotFingerprint?: string;
  region?: string;
  volumeId?: string;
  lastActivityAt?: string;
  lastHeartbeatAt?: string;
  createdAt: string;
  updatedAt: string;
};

type FlyMachine = {
  id?: string;
  name?: string;
  state?: string;
  instance_id?: string;
  config?: JsonRecord;
};

const port = positiveIntegerEnv("PORT", 3000, 1, 65_535);
const baseURL = requiredEnv("BETTER_AUTH_URL", `http://localhost:${port}`).replace(/\/+$/, "");
const authBaseURL = `${baseURL}/api/auth`;
const dataDir = process.env.RUDDER_CLOUD_DATA_DIR || path.join(os.homedir(), ".rudder-cloud");
const dbPath = process.env.RUDDER_CLOUD_DB || path.join(dataDir, "rudder-cloud.sqlite");
const awsRegion = process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || "us-east-1";
const snapshotBucket = process.env.RUDDER_S3_BUCKET || "";
const flyApiToken = process.env.FLY_API_TOKEN || "";
const flyApiBase = (process.env.FLY_API_HOSTNAME || "https://api.machines.dev").replace(/\/$/, "");
// Fly injects FLY_APP_NAME with the control plane's own app name. Falling back
// to it can accidentally provision user workers inside the control-plane app.
const flyAppName = (process.env.RUDDER_FLY_APP_NAME || "").trim();
const flyRegion = (process.env.RUDDER_FLY_REGION || process.env.FLY_REGION || "iad").trim();
const flyWorkerImage = process.env.RUDDER_WORKER_IMAGE || "ghcr.io/viraatdas/rudder-worker:latest";
const flyWorkerMemoryMb = positiveIntegerEnv("RUDDER_WORKER_MEMORY_MB", 1024, 256, 65_536);
const flyWorkerCpus = positiveIntegerEnv("RUDDER_WORKER_CPUS", 1, 1, 64);
const flyWorkerCpuKind = process.env.RUDDER_WORKER_CPU_KIND || "shared";
const flyWorkspaceVolumeGb = positiveIntegerEnv("RUDDER_WORKSPACE_VOLUME_GB", 3, 1, 500);
const idlePauseMs = nonNegativeIntegerEnv("RUDDER_IDLE_PAUSE_MS", 120 * 60 * 1000, 30 * 24 * 60 * 60 * 1000);
const stateKey = process.env.RUDDER_CLOUD_STATE_KEY || "control-plane/rudder-cloud.sqlite";
const persistStateToS3 = process.env.RUDDER_CLOUD_PERSIST_STATE !== "0";
const MAX_STATE_DB_BYTES = 512 * 1024 * 1024;
const githubDeviceClientId = process.env.RUDDER_GITHUB_DEVICE_CLIENT_ID || "178c6fc778ccc68e1d6a";
const publicLoginUrl = (process.env.RUDDER_PUBLIC_LOGIN_URL || "").trim();
const adminEmails = new Set((process.env.RUDDER_ADMIN_EMAILS || "viraat.laldas@gmail.com,viraat@exla.ai")
  .split(",")
  .map((email) => email.trim().toLowerCase())
  .filter(Boolean));
// Slack user IDs authorized to issue control commands (list/output/stop/talk) in the
// shared channel. The channel is multi-tenant, so without this any channel member
// could drive any account's cloud instances — fail closed: when unset, control
// commands are refused and only `help` works.
const slackAllowedUsers = new Set((process.env.RUDDER_SLACK_ALLOWED_USERS || "")
  .split(",")
  .map((id) => id.trim())
  .filter(Boolean));
// Explicitly opt accounts into the shared Slack surface. Without this tenant
// boundary, launching from any Rudder account would disclose its repo/task in
// one global channel and let allowlisted Slack operators control it.
const slackAccountIds = new Set((process.env.RUDDER_SLACK_ACCOUNT_IDS || "")
  .split(",")
  .map((id) => id.trim())
  .filter(Boolean));
const deviceLogins = new Map<string, DeviceLogin>();
const githubBrowserLogins = new Map<string, GithubBrowserLogin>();
const s3 = new S3Client({ region: awsRegion });
const slack = slackConfigFromEnv();
// Slack message events are delivered at-least-once; dedupe by event_id so a
// retry doesn't double-inject into an instance.
const seenSlackEvents = new Set<string>();
const MAX_DEVICE_LOGINS = 10_000;
const EXTERNAL_REQUEST_TIMEOUT_MS = positiveIntegerEnv("RUDDER_EXTERNAL_REQUEST_TIMEOUT_MS", 15_000, 1_000, 120_000);
const maxActiveSailsPerAccount = positiveIntegerEnv("RUDDER_MAX_ACTIVE_SAILS_PER_ACCOUNT", 20, 1, 1_000);
const maxWorkspacesPerAccount = positiveIntegerEnv("RUDDER_MAX_WORKSPACES_PER_ACCOUNT", 10, 1, 1_000);

await fs.mkdir(dataDir, { recursive: true });
await restoreDatabaseFromS3();

const database = new Database(dbPath);
database.pragma("journal_mode = WAL");
database.pragma("busy_timeout = 5000");
database.pragma("foreign_keys = ON");
await fs.chmod(dbPath, 0o600).catch(() => undefined);
database.exec(`
  create table if not exists user (
    id text primary key not null,
    name text not null,
    email text not null unique,
    emailVerified integer not null,
    image text,
    createdAt date not null,
    updatedAt date not null
  );
  create table if not exists session (
    id text primary key not null,
    expiresAt date not null,
    token text not null unique,
    createdAt date not null,
    updatedAt date not null,
    ipAddress text,
    userAgent text,
    userId text not null references user(id) on delete cascade
  );
  create index if not exists session_userId_idx on session(userId);
  create table if not exists account (
    id text primary key not null,
    accountId text not null,
    providerId text not null,
    userId text not null references user(id) on delete cascade,
    accessToken text,
    refreshToken text,
    idToken text,
    accessTokenExpiresAt date,
    refreshTokenExpiresAt date,
    scope text,
    password text,
    createdAt date not null,
    updatedAt date not null
  );
  create index if not exists account_userId_idx on account(userId);
  create table if not exists verification (
    id text primary key not null,
    identifier text not null,
    value text not null,
    expiresAt date not null,
    createdAt date not null,
    updatedAt date not null
  );
  create index if not exists verification_identifier_idx on verification(identifier);
  create table if not exists rudder_tokens (
    token_hash text primary key,
    account_id text not null,
    email text,
    created_at text not null,
    last_used_at text
  );
  create table if not exists rudder_sails (
    id text primary key,
    account_id text not null,
    status text not null,
    runtime text,
    repo_name text,
    task text,
    branch text,
    machine_id text,
    machine_state text,
    snapshot_key text,
    manifest_json text,
    worker_token_hash text,
    last_activity_at text,
    last_heartbeat_at text,
    created_at text not null,
    updated_at text not null
  );
  create table if not exists rudder_settings (
    key text primary key,
    value text not null,
    updated_at text not null
  );
  create table if not exists rudder_workspaces (
    id text primary key,
    account_id text not null,
    workspace_key text not null,
    repo_name text,
    status text not null,
    machine_id text,
    machine_state text,
    snapshot_key text,
    worker_token_hash text,
    last_activity_at text,
    last_heartbeat_at text,
    created_at text not null,
    updated_at text not null,
    volume_id text,
    unique(account_id, workspace_key)
  );
`);
ensureColumn("rudder_sails", "worker_token_hash", "text");
ensureColumn("rudder_sails", "last_heartbeat_at", "text");
ensureColumn("rudder_sails", "runtime", "text");
ensureColumn("rudder_sails", "slack_thread_ts", "text");
ensureColumn("rudder_sails", "last_activity_at", "text");
ensureColumn("rudder_workspaces", "region", "text");
ensureColumn("rudder_workspaces", "snapshot_fingerprint", "text");
ensureColumn("rudder_workspaces", "volume_id", "text");

const insertToken = database.prepare(`
  insert into rudder_tokens (token_hash, account_id, email, created_at, last_used_at)
  values (@tokenHash, @accountId, @email, @createdAt, @lastUsedAt)
`);
const findToken = database.prepare("select * from rudder_tokens where token_hash = ?");
const touchToken = database.prepare(`
  update rudder_tokens
  set last_used_at = ?
  where token_hash = ? and (last_used_at is null or last_used_at < ?)
`);
const insertSail = database.prepare(`
  insert into rudder_sails (
    id, account_id, status, runtime, repo_name, task, branch, machine_id, machine_state,
    snapshot_key, manifest_json, worker_token_hash, last_activity_at, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @status, @runtime, @repoName, @task, @branch, @machineId, @machineState,
    @snapshotKey, @manifestJson, @workerTokenHash, @lastActivityAt, @lastHeartbeatAt, @createdAt, @updatedAt
  )
`);
const updateSail = database.prepare(`
  update rudder_sails
  set status = @status,
      machine_id = @machineId,
      machine_state = @machineState,
      updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const findSail = database.prepare("select * from rudder_sails where id = ? and account_id = ?");
const findSailById = database.prepare("select * from rudder_sails where id = ?");
const listSailsForAccount = database.prepare(
  "select * from rudder_sails where account_id = ? order by updated_at desc limit 100",
);
const countActiveSailsForAccount = database.prepare(
  "select count(*) as count from rudder_sails where account_id = ? and status in ('queued','running','paused')",
);
const updateHeartbeat = database.prepare(`
  update rudder_sails
  set status = @status,
      machine_id = coalesce(@machineId, machine_id),
      machine_state = @machineState,
      last_heartbeat_at = @lastHeartbeatAt,
      updated_at = @updatedAt
  where id = @id
`);
const updateSailActivity = database.prepare(`
  update rudder_sails
  set last_activity_at = @lastActivityAt,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkerToken = database.prepare(`
  update rudder_sails
  set worker_token_hash = @workerTokenHash,
      updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const updateSailSlackThread = database.prepare(
  "update rudder_sails set slack_thread_ts = @threadTs, updated_at = @updatedAt where id = @id",
);
const findSailBySlackThread = database.prepare("select * from rudder_sails where slack_thread_ts = ?");
const listRunningSails = database.prepare(
  "select * from rudder_sails where status in ('queued','running','paused') order by updated_at desc limit 100",
);
const insertWorkspace = database.prepare(`
  insert into rudder_workspaces (
    id, account_id, workspace_key, repo_name, status, machine_id, machine_state,
    snapshot_key, snapshot_fingerprint, region, volume_id, worker_token_hash,
    last_activity_at, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @workspaceKey, @repoName, @status, @machineId, @machineState,
    @snapshotKey, @snapshotFingerprint, @region, @volumeId, @workerTokenHash,
    @lastActivityAt, @lastHeartbeatAt, @createdAt, @updatedAt
  )
`);
const updateWorkspaceVolume = database.prepare(`
  update rudder_workspaces
  set volume_id = @volumeId,
      region = @region,
      updated_at = @updatedAt
  where id = @id
`);
const findWorkspaceByKey = database.prepare(
  "select * from rudder_workspaces where account_id = ? and workspace_key = ?",
);
const findWorkspaceById = database.prepare("select * from rudder_workspaces where id = ?");
const findWorkspaceForAccount = database.prepare(
  "select * from rudder_workspaces where id = ? and account_id = ?",
);
const listWorkspacesForAccount = database.prepare(
  "select * from rudder_workspaces where account_id = ? order by updated_at desc limit 100",
);
const countWorkspacesForAccount = database.prepare(
  "select count(*) as count from rudder_workspaces where account_id = ?",
);
const listAllRunningWorkspaces = database.prepare(
  "select * from rudder_workspaces where status in ('running','paused','queued')",
);
const updateWorkspaceMachine = database.prepare(`
  update rudder_workspaces
  set status = @status,
      machine_id = @machineId,
      machine_state = @machineState,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceSnapshot = database.prepare(`
  update rudder_workspaces
  set snapshot_key = @snapshotKey,
      snapshot_fingerprint = @snapshotFingerprint,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceWorkerToken = database.prepare(`
  update rudder_workspaces
  set worker_token_hash = @workerTokenHash,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceActivity = database.prepare(`
  update rudder_workspaces
  set last_activity_at = @lastActivityAt,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceHeartbeat = database.prepare(`
  update rudder_workspaces
  set status = @status,
      machine_id = coalesce(@machineId, machine_id),
      machine_state = @machineState,
      last_heartbeat_at = @lastHeartbeatAt,
      updated_at = @updatedAt
  where id = @id
`);
const deleteSailRow = database.prepare("delete from rudder_sails where id = ? and account_id = ?");
const deleteWorkspaceRow = database.prepare("delete from rudder_workspaces where id = ? and account_id = ?");
const getSetting = database.prepare("select value from rudder_settings where key = ?");
const upsertSetting = database.prepare(`
  insert into rudder_settings (key, value, updated_at)
  values (@key, @value, @updatedAt)
  on conflict(key) do update set value = excluded.value, updated_at = excluded.updated_at
`);

let authProviderFingerprint = providerFingerprint();
let auth: ReturnType<typeof createBetterAuth> = createBetterAuth();
let authHandler = toNodeHandler(auth.handler);

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url || "/", baseURL);
    if (url.pathname === "/api/auth" || url.pathname.startsWith("/api/auth/")) {
      refreshAuthHandler();
      // Better Auth owns its SQL statements, so this is the one route family
      // whose mutations cannot call markDatabaseDirty directly.
      res.once("finish", markDatabaseDirty);
      authHandler(req, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/health") {
      sendJson(res, 200, {
        ok: true,
        s3: Boolean(snapshotBucket),
        fly: Boolean(flyApiToken && flyAppName && flyWorkerImage),
        byoVm: Boolean(snapshotBucket && flyWorkerImage),
        state: Boolean(snapshotBucket && persistStateToS3),
        auth: configuredProviders(),
        slack: {
          enabled: slack.enabled,
          scoped: slackAccountIds.size > 0,
        },
      });
      return;
    }
    if (req.method === "GET" && url.pathname === "/setup/github") {
      // Provider-credential setup is admin-only: an unauthenticated visitor must not
      // be able to install (or, via the callback, overwrite) the OAuth client creds.
      await requireAdminRequest(req);
      renderGithubAppSetup(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/setup/github/callback") {
      await requireAdminRequest(req);
      await handleGithubAppSetupCallback(url, res);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/rudder/setup/github") {
      await handleOAuthCredentialSetup(req, res, "github");
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/rudder/setup/google") {
      await handleOAuthCredentialSetup(req, res, "google");
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/cli/login") {
      checkGlobalRateLimit("cli-login", 300);
      await handleCliLoginStart(res);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/cli/login/github-token") {
      checkGlobalRateLimit("github-token", 60);
      await handleCliGithubToken(req, res);
      return;
    }
    if ((req.method === "GET" || req.method === "POST") && url.pathname === "/api/cli/login/poll") {
      await handleCliLoginPoll(req, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/login") {
      renderLoginPage(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/oauth/google/start") {
      await handleCliOAuthStart(url, res, "google");
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/oauth/github/start") {
      await handleCliOAuthStart(url, res, "github");
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/github/start") {
      await handleCliGithubStart(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/github/wait") {
      await handleCliGithubWait(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/approve") {
      await handleCliApprove(req, res, url);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/admin/workspace/gc") {
      await handleAdminWorkspaceGc(req, res, url);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/slack/events") {
      await handleSlackEvents(req, res);
      return;
    }
    if (url.pathname === "/api/rudder/sail" || url.pathname.startsWith("/api/rudder/sail/")) {
      await handleSailApi(req, res, url);
      return;
    }
    if (url.pathname === "/api/rudder/workspace" || url.pathname.startsWith("/api/rudder/workspace/")) {
      await handleWorkspaceApi(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/") || req.method !== "GET") {
      sendJson(res, 404, { error: "not found" });
      return;
    }
    renderHome(res);
  } catch (error) {
    const status = error && typeof error === "object" && "status" in error && typeof error.status === "number"
      ? error.status
      : 500;
    if (status >= 500) {
      console.error(`request error ${req.method} ${req.url} -> ${status}`, error);
    }
    if (res.headersSent) {
      if (!res.writableEnded) {
        res.end();
      }
      return;
    }
    if (status === 413) {
      res.setHeader("connection", "close");
    }
    sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
  }
});

const SAIL_ATTACH_PATH_RE = /^\/api\/rudder\/sail\/([^/]+)\/(worker|attach)$/;
const WORKSPACE_ATTACH_PATH_RE = /^\/api\/rudder\/workspace\/([^/]+)\/(worker|attach)$/;
const REPLAY_BUFFER_BYTES = 256 * 1024;
const MAX_REPLAY_BUFFER_CHUNKS = 512;
const MAX_WS_PAYLOAD_BYTES = 2 * 1024 * 1024;
const MAX_WS_BACKPRESSURE_BYTES = 4 * 1024 * 1024;
const MAX_CHANNEL_CLIENTS = 32;
const CHANNEL_RETENTION_MS = positiveIntegerEnv("RUDDER_CHANNEL_RETENTION_MS", 60 * 60 * 1000, 60_000, 24 * 60 * 60 * 1000);
const MAX_RETAINED_CHANNELS = 500;
const ACTIVITY_WRITE_INTERVAL_MS = 30_000;

type Channel = {
  worker?: WebSocket;
  clients: Set<WebSocket>;
  buffer: Buffer[];
  bufferBytes: number;
  lastTouchedAt: number;
  cleanupTimer?: NodeJS.Timeout;
};

type ChannelKind = "sail" | "workspace";

const sailChannels = new Map<string, Channel>();
const workspaceChannels = new Map<string, Channel>();

const wss = new WebSocketServer({
  noServer: true,
  maxPayload: MAX_WS_PAYLOAD_BYTES,
  // Compress large terminal redraws (a single Claude frame can be many kB of
  // ANSI). Skip small frames so per-keystroke echoes don't pay the deflate
  // tax.
  perMessageDeflate: {
    threshold: 1024,
    zlibDeflateOptions: { level: 1 },
  },
});
const socketAlive = new WeakMap<WebSocket, boolean>();
const socketHealthTimer = setInterval(() => {
  for (const ws of wss.clients) {
    if (socketAlive.get(ws) === false) {
      ws.terminate();
      continue;
    }
    socketAlive.set(ws, false);
    try {
      ws.ping();
    } catch {
      ws.terminate();
    }
  }
}, 30_000);
socketHealthTimer.unref?.();

server.on("upgrade", (req: IncomingMessage, socket: Duplex, head: Buffer) => {
  try {
    const url = new URL(req.url || "/", baseURL);
    const sailMatch = SAIL_ATTACH_PATH_RE.exec(url.pathname);
    if (sailMatch) {
      handleSailUpgrade(req, socket, head, sailMatch[1], sailMatch[2] as "worker" | "attach");
      return;
    }
    const workspaceMatch = WORKSPACE_ATTACH_PATH_RE.exec(url.pathname);
    if (workspaceMatch) {
      handleWorkspaceUpgrade(req, socket, head, workspaceMatch[1], workspaceMatch[2] as "worker" | "attach");
      return;
    }
    destroyUpgrade(socket, 404);
  } catch {
    destroyUpgrade(socket, 500);
  }
});

function handleSailUpgrade(
  req: IncomingMessage,
  socket: Duplex,
  head: Buffer,
  sailId: string,
  role: "worker" | "attach",
): void {
  const sailRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!sailRow) {
    destroyUpgrade(socket, 404);
    return;
  }
  if (role === "worker") {
    try {
      requireWorkerBearer(req, sailRow);
    } catch {
      destroyUpgrade(socket, 401);
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => attachWorker("sail", sailId, ws));
    return;
  }
  let auth;
  try {
    auth = requireBearer(req);
  } catch {
    destroyUpgrade(socket, 401);
    return;
  }
  if (String(sailRow.account_id) !== auth.accountId) {
    destroyUpgrade(socket, 404);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachClient("sail", sailId, ws));
}

function handleWorkspaceUpgrade(
  req: IncomingMessage,
  socket: Duplex,
  head: Buffer,
  workspaceId: string,
  role: "worker" | "attach",
): void {
  const workspaceRow = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!workspaceRow) {
    destroyUpgrade(socket, 404);
    return;
  }
  if (role === "worker") {
    try {
      requireWorkerBearer(req, workspaceRow);
    } catch {
      destroyUpgrade(socket, 401);
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => attachWorker("workspace", workspaceId, ws));
    return;
  }
  let auth;
  try {
    auth = requireBearer(req);
  } catch {
    destroyUpgrade(socket, 401);
    return;
  }
  if (String(workspaceRow.account_id) !== auth.accountId) {
    destroyUpgrade(socket, 404);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachClient("workspace", workspaceId, ws));
}

function destroyUpgrade(socket: Duplex, status: number): void {
  const reason = status === 401 ? "Unauthorized"
    : status === 403 ? "Forbidden"
    : status === 404 ? "Not Found"
    : "Internal Server Error";
  try {
    socket.write(`HTTP/1.1 ${status} ${reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
  } catch {
    // ignore
  }
  socket.destroy();
}

function channelMap(kind: ChannelKind): Map<string, Channel> {
  return kind === "workspace" ? workspaceChannels : sailChannels;
}

function getChannel(kind: ChannelKind, id: string): Channel {
  const map = channelMap(kind);
  let channel = map.get(id);
  if (!channel) {
    channel = { clients: new Set(), buffer: [], bufferBytes: 0, lastTouchedAt: Date.now() };
    map.set(id, channel);
    trimRetainedChannels(map);
  } else if (channel.cleanupTimer) {
    clearTimeout(channel.cleanupTimer);
    channel.cleanupTimer = undefined;
  }
  channel.lastTouchedAt = Date.now();
  return channel;
}

function disposeChannelIfEmpty(kind: ChannelKind, id: string): void {
  const map = channelMap(kind);
  const channel = map.get(id);
  if (!channel) {
    return;
  }
  if (!channel.worker && channel.clients.size === 0) {
    if (channel.bufferBytes === 0) {
      map.delete(id);
      lastActivityWrites.delete(`${kind}:${id}`);
      return;
    }
    channel.cleanupTimer ??= setTimeout(() => {
      const current = map.get(id);
      if (current === channel && !current.worker && current.clients.size === 0) {
        map.delete(id);
        lastActivityWrites.delete(`${kind}:${id}`);
      }
    }, CHANNEL_RETENTION_MS);
    channel.cleanupTimer.unref?.();
  }
}

function trimRetainedChannels(map: Map<string, Channel>): void {
  if (map.size <= MAX_RETAINED_CHANNELS) {
    return;
  }
  const candidates = [...map.entries()]
    .filter(([, channel]) => !channel.worker && channel.clients.size === 0)
    .sort((a, b) => a[1].lastTouchedAt - b[1].lastTouchedAt);
  while (map.size > MAX_RETAINED_CHANNELS && candidates.length > 0) {
    const [id, channel] = candidates.shift()!;
    if (channel.cleanupTimer) {
      clearTimeout(channel.cleanupTimer);
    }
    map.delete(id);
    for (const kind of ["sail", "workspace"] as const) {
      if (channelMap(kind) === map) {
        lastActivityWrites.delete(`${kind}:${id}`);
        break;
      }
    }
  }
}

function trackSocket(ws: WebSocket): void {
  socketAlive.set(ws, true);
  ws.on("pong", () => socketAlive.set(ws, true));
}

function attachWorker(kind: ChannelKind, id: string, ws: WebSocket): void {
  const channel = getChannel(kind, id);
  trackSocket(ws);
  if (channel.worker && channel.worker.readyState === WebSocket.OPEN) {
    try {
      channel.worker.close(4000, "replaced");
    } catch {
      // ignore
    }
  }
  channel.worker = ws;
  channel.lastTouchedAt = Date.now();
  broadcastStatus(channel, "worker-connected");

  ws.on("message", (data, isBinary) => {
    const bytes = rawDataToBuffer(data);
    if (isBinary) {
      pushBuffer(channel, bytes);
      touchInstanceActivity(kind, id);
      for (const client of channel.clients) {
        sendSocketFrame(client, bytes, true);
      }
      return;
    }
    forwardTextToClients(channel, bytes.toString("utf8"));
  });

  ws.on("close", () => {
    if (channel.worker === ws) {
      channel.worker = undefined;
      channel.lastTouchedAt = Date.now();
      broadcastStatus(channel, "worker-disconnected");
      disposeChannelIfEmpty(kind, id);
    }
  });
  ws.on("error", () => undefined);
}

function attachClient(kind: ChannelKind, id: string, ws: WebSocket): void {
  const channel = getChannel(kind, id);
  trackSocket(ws);
  if (channel.clients.size >= MAX_CHANNEL_CLIENTS) {
    ws.close(1013, "too many attached clients");
    disposeChannelIfEmpty(kind, id);
    return;
  }
  channel.clients.add(ws);
  channel.lastTouchedAt = Date.now();
  touchInstanceActivity(kind, id, true);

  sendSocketFrame(ws, JSON.stringify({
    type: "status",
    state: channel.worker?.readyState === WebSocket.OPEN ? "worker-connected" : "worker-waiting",
  }), false);
  for (const chunk of channel.buffer) {
    sendSocketFrame(ws, chunk, true);
  }

  ws.on("message", (data, isBinary) => {
    const worker = channel.worker;
    if (!worker || worker.readyState !== WebSocket.OPEN) {
      return;
    }
    const bytes = rawDataToBuffer(data);
    if (isBinary) {
      if (!sendSocketFrame(worker, bytes, true)) {
        return;
      }
      touchInstanceActivity(kind, id);
      return;
    }
    sendSocketFrame(worker, bytes.toString("utf8"), false);
  });

  ws.on("close", () => {
    channel.clients.delete(ws);
    disposeChannelIfEmpty(kind, id);
  });
  ws.on("error", () => undefined);
}

function pushBuffer(channel: Channel, chunk: Buffer): void {
  const retained = chunk.length > REPLAY_BUFFER_BYTES
    ? Buffer.from(chunk.subarray(chunk.length - REPLAY_BUFFER_BYTES))
    : Buffer.from(chunk);
  channel.buffer.push(retained);
  channel.bufferBytes += retained.length;
  channel.lastTouchedAt = Date.now();
  while (
    (channel.bufferBytes > REPLAY_BUFFER_BYTES || channel.buffer.length > MAX_REPLAY_BUFFER_CHUNKS)
    && channel.buffer.length > 0
  ) {
    const dropped = channel.buffer.shift();
    if (dropped) {
      channel.bufferBytes -= dropped.length;
    }
  }
}

function rawDataToBuffer(data: RawData): Buffer {
  if (Buffer.isBuffer(data)) {
    return data;
  }
  if (Array.isArray(data)) {
    return Buffer.concat(data);
  }
  return Buffer.from(data);
}

function sendSocketFrame(ws: WebSocket, data: Buffer | string, binary: boolean): boolean {
  if (ws.readyState !== WebSocket.OPEN) {
    return false;
  }
  if (ws.bufferedAmount > MAX_WS_BACKPRESSURE_BYTES) {
    ws.close(1013, "slow consumer");
    return false;
  }
  try {
    ws.send(data, { binary }, (error) => {
      if (error && ws.readyState === WebSocket.OPEN) {
        ws.terminate();
      }
    });
    return true;
  } catch {
    ws.terminate();
    return false;
  }
}

function forwardTextToClients(channel: Channel, text: string): void {
  for (const client of channel.clients) {
    sendSocketFrame(client, text, false);
  }
}

function broadcastStatus(channel: Channel, state: string): void {
  const payload = JSON.stringify({ type: "status", state });
  for (const client of channel.clients) {
    sendSocketFrame(client, payload, false);
  }
}

// Inject text into a live worker's PTY exactly as a `cloud attach` client would.
// The worker supervisor treats binary frames as raw keystrokes, so we send the
// UTF-8 bytes (plus a carriage return to submit) on the worker socket. Returns
// false when no worker is currently connected for that instance.
function sendInputToChannel(kind: ChannelKind, id: string, text: string, submit = true): boolean {
  const channel = channelMap(kind).get(id);
  const worker = channel?.worker;
  if (!channel || !worker || worker.readyState !== WebSocket.OPEN) {
    return false;
  }
  const payload = submit ? `${text}\r` : text;
  const delivered = sendSocketFrame(worker, Buffer.from(payload, "utf8"), true);
  if (delivered) {
    touchInstanceActivity(kind, id);
  }
  return delivered;
}

// Read the most recent raw output (with ANSI) buffered for an instance. Empty
// channels retain this bounded tail for a configurable window so completion
// heartbeats, Slack, and `cloud output` do not race the worker's socket close.
function readChannelOutput(kind: ChannelKind, id: string): string {
  const channel = channelMap(kind).get(id);
  if (!channel || channel.buffer.length === 0) {
    return "";
  }
  return Buffer.concat(channel.buffer).toString("utf8");
}

function instanceHasLiveWorker(kind: ChannelKind, id: string): boolean {
  const worker = channelMap(kind).get(id)?.worker;
  return Boolean(worker && worker.readyState === WebSocket.OPEN);
}

const lastActivityWrites = new Map<string, number>();

function touchInstanceActivity(kind: ChannelKind, id: string, force = false): void {
  const key = `${kind}:${id}`;
  const nowMs = Date.now();
  if (!force && nowMs - (lastActivityWrites.get(key) ?? 0) < ACTIVITY_WRITE_INTERVAL_MS) {
    return;
  }
  lastActivityWrites.set(key, nowMs);
  try {
    const now = new Date(nowMs).toISOString();
    if (kind === "workspace") {
      updateWorkspaceActivity.run({ id, lastActivityAt: now, updatedAt: now });
    } else {
      updateSailActivity.run({ id, lastActivityAt: now, updatedAt: now });
    }
    markDatabaseDirty();
  } catch {
    // ignore
  }
}

server.requestTimeout = 5 * 60 * 1000;
server.headersTimeout = 30 * 1000;
server.keepAliveTimeout = 65 * 1000;
server.listen(port, () => {
  console.log(`rudder cloud listening on ${baseURL}`);
});

let shuttingDown = false;
for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void shutdown(signal);
  });
}

async function shutdown(signal: "SIGINT" | "SIGTERM"): Promise<void> {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  clearInterval(socketHealthTimer);
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = undefined;
  }
  const closed = new Promise<void>((resolve) => server.close(() => resolve()));
  for (const ws of wss.clients) {
    try {
      ws.close(1001, "server shutdown");
    } catch {
      ws.terminate();
    }
  }
  const forceClose = setTimeout(() => {
    for (const ws of wss.clients) {
      ws.terminate();
    }
    server.closeAllConnections?.();
  }, 1_500);
  forceClose.unref?.();
  await Promise.race([closed, sleep(2_000)]);
  clearTimeout(forceClose);
  await Promise.race([persistDatabaseToS3(true), sleep(6_000)]);
  try {
    database.close();
  } catch {
    // ignore
  }
  process.exit(signal === "SIGINT" ? 130 : 143);
}

const workspaceIdleMs = nonNegativeIntegerEnv("RUDDER_WORKSPACE_IDLE_MS", 30 * 60 * 1000, 30 * 24 * 60 * 60 * 1000);
const workspaceSweepIntervalMs = nonNegativeIntegerEnv("RUDDER_WORKSPACE_SWEEP_MS", 60 * 1000, 24 * 60 * 60 * 1000);
if (workspaceIdleMs >= 60 * 1000 && workspaceSweepIntervalMs >= 10 * 1000) {
  const timer = setInterval(() => {
    void sweepIdleWorkspaces().catch(() => undefined);
  }, workspaceSweepIntervalMs);
  timer.unref?.();
}

async function sweepIdleWorkspaces(): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    return;
  }
  const rows = listAllRunningWorkspaces.all() as Record<string, unknown>[];
  const now = Date.now();
  for (const row of rows) {
    const candidate = rowToWorkspace(row);
    await withOperationLock(`workspace:${candidate.accountId}:${candidate.workspaceKey}`, async () => {
      const currentRow = findWorkspaceById.get(candidate.id) as Record<string, unknown> | undefined;
      if (!currentRow) return;
      const workspace = rowToWorkspace(currentRow);
      if (
        !workspace.machineId
        || !["running", "paused", "queued"].includes(workspace.status)
        || workspaceChannels.get(workspace.id)?.clients.size
      ) return;
      const last = workspace.lastActivityAt ?? workspace.createdAt;
      const lastMs = Date.parse(last);
      if (!Number.isFinite(lastMs) || now - lastMs < workspaceIdleMs) return;
      try {
        await flyRequest<FlyMachine>(
          `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/stop`,
          { method: "POST", body: { signal: "SIGTERM", timeout: "10s" } },
        );
        updateWorkspaceMachine.run({
          id: workspace.id,
          status: "stopped",
          machineId: workspace.machineId,
          machineState: "stopping",
          updatedAt: new Date().toISOString(),
        });
        markDatabaseDirty();
      } catch {
        // ignore; will retry next sweep
      }
    });
  }
}

let persistTimer: NodeJS.Timeout | undefined;
let persistInFlight = false;
let persistAgain = false;
let databaseDirty = false;
let lastPersistAt = 0;
const MIN_PERSIST_INTERVAL_MS = positiveIntegerEnv("RUDDER_STATE_PERSIST_INTERVAL_MS", 5_000, 750, 60_000);

async function restoreDatabaseFromS3(): Promise<void> {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  try {
    const response = await s3.send(new GetObjectCommand({
      Bucket: snapshotBucket,
      Key: stateKey,
    }));
    if (!response.Body) {
      return;
    }
    if (typeof response.ContentLength === "number" && response.ContentLength > MAX_STATE_DB_BYTES) {
      throw new Error("persisted database exceeds the restore size limit");
    }
    const buffer = await streamToBuffer(response.Body, MAX_STATE_DB_BYTES);
    if (buffer.length === 0) {
      return;
    }
    if (buffer.length < 100 || buffer.subarray(0, 16).toString("binary") !== "SQLite format 3\0") {
      throw new Error("persisted database is not a valid SQLite file");
    }
    const tempPath = `${dbPath}.${process.pid}.restore`;
    await fs.writeFile(tempPath, buffer, { mode: 0o600 });
    await Promise.all([
      fs.rm(`${dbPath}-wal`, { force: true }),
      fs.rm(`${dbPath}-shm`, { force: true }),
    ]);
    await fs.rename(tempPath, dbPath);
  } catch (error) {
    const name = error && typeof error === "object" && "name" in error ? String(error.name) : "";
    if (name !== "NoSuchKey" && name !== "NotFound") {
      console.warn(`rudder cloud state restore skipped: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
}

function schedulePersistDatabase(): void {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  if (persistTimer) {
    return;
  }
  persistTimer = setTimeout(() => {
    persistTimer = undefined;
    void persistDatabaseToS3();
  }, Math.max(750, lastPersistAt + MIN_PERSIST_INTERVAL_MS - Date.now()));
  persistTimer.unref?.();
}

function markDatabaseDirty(): void {
  databaseDirty = true;
  schedulePersistDatabase();
}

async function persistDatabaseToS3(force = false): Promise<void> {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  if (!force && !databaseDirty) {
    return;
  }
  if (persistInFlight) {
    persistAgain = true;
    if (force) {
      while (persistInFlight) {
        await sleep(25);
      }
      return await persistDatabaseToS3(true);
    }
    return;
  }
  persistInFlight = true;
  databaseDirty = false;
  try {
    database.pragma("wal_checkpoint(TRUNCATE)");
    const body = await fs.readFile(dbPath);
    await s3.send(new PutObjectCommand({
      Bucket: snapshotBucket,
      Key: stateKey,
      Body: body,
      ContentType: "application/vnd.sqlite3",
      ServerSideEncryption: "AES256",
    }));
    lastPersistAt = Date.now();
  } catch (error) {
    databaseDirty = true;
    persistAgain = true;
    console.warn(`rudder cloud state persist failed: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    persistInFlight = false;
    if (persistAgain) {
      persistAgain = false;
      schedulePersistDatabase();
    }
  }
}

async function streamToBuffer(body: unknown, maxBytes = Number.MAX_SAFE_INTEGER): Promise<Buffer> {
  if (Buffer.isBuffer(body)) {
    if (body.length > maxBytes) throw new Error("stream exceeds size limit");
    return body;
  }
  if (body instanceof Uint8Array) {
    if (body.byteLength > maxBytes) throw new Error("stream exceeds size limit");
    return Buffer.from(body);
  }
  if (body instanceof Readable || (body && typeof body === "object" && Symbol.asyncIterator in body)) {
    const chunks: Buffer[] = [];
    let total = 0;
    for await (const chunk of body as AsyncIterable<Buffer | Uint8Array | string>) {
      const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      total += bytes.length;
      if (total > maxBytes) throw new Error("stream exceeds size limit");
      chunks.push(bytes);
    }
    return Buffer.concat(chunks, total);
  }
  if (body && typeof body === "object" && "transformToByteArray" in body) {
    const bytes = await (body as { transformToByteArray(): Promise<Uint8Array> }).transformToByteArray();
    if (bytes.byteLength > maxBytes) throw new Error("stream exceeds size limit");
    return Buffer.from(bytes);
  }
  throw new Error("unsupported S3 body type");
}

async function handleCliLoginStart(res: ServerResponse): Promise<void> {
  pruneLoginState();
  if (deviceLogins.size >= MAX_DEVICE_LOGINS) {
    throw tooManyRequests("too many active login sessions; try again shortly");
  }
  const deviceCode = randomUUID();
  const expiresAt = Date.now() + 5 * 60 * 1000;
  deviceLogins.set(deviceCode, { deviceCode, expiresAt });
  const loginBase = publicLoginUrl || `${baseURL}/cli/login`;
  const separator = loginBase.includes("?") ? "&" : "?";
  sendJson(res, 200, {
    deviceCode,
    loginUrl: `${loginBase}${separator}device_code=${encodeURIComponent(deviceCode)}`,
    pollUrl: "/api/cli/login/poll",
    interval: 2,
    expiresIn: 300,
  });
}

async function handleCliLoginPoll(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const body = req.method === "POST" ? await readJsonBody(req) : {};
  const url = new URL(req.url || "/", baseURL);
  const deviceCode = stringField(body, "deviceCode") || url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    deviceLogins.delete(deviceCode);
    githubBrowserLogins.delete(deviceCode);
    sendJson(res, 404, { error: "login expired" });
    return;
  }
  if (!login.token) {
    sendJson(res, 200, { pending: true });
    return;
  }
  deviceLogins.delete(deviceCode);
  const responseBody: JsonRecord = {
    pending: false,
    token: login.token,
  };
  if (login.accountId) {
    responseBody.accountId = login.accountId;
  }
  if (login.email) {
    responseBody.email = login.email;
  }
  sendJson(res, 200, responseBody);
}

async function handleCliGithubToken(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const body = await readJsonBody(req);
  const githubToken = stringField(body, "token");
  if (!githubToken) {
    throw badRequest("token is required");
  }
  const user = await githubUser(githubToken);
  const issued = issueRudderToken(`github:${user.id}`, user.email);
  const responseBody: JsonRecord = {
    token: issued.token,
    accountId: issued.accountId,
    provider: "github",
  };
  if (issued.email) {
    responseBody.email = issued.email;
  }
  sendJson(res, 200, responseBody);
}

function issueRudderToken(accountId: string, email?: string): { token: string; accountId: string; email?: string } {
  const rudderToken = `rdr_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  insertToken.run({
    tokenHash: tokenHash(rudderToken),
    accountId,
    email: email ?? null,
    createdAt: now,
    lastUsedAt: now,
  });
  markDatabaseDirty();
  return { token: rudderToken, accountId, email };
}

function renderLoginPage(url: URL, res: ServerResponse): void {
  const deviceCode = url.searchParams.get("device_code") || "";
  const callbackURL = `/cli/approve?device_code=${encodeURIComponent(deviceCode)}`;
  const providers = configuredProviders();
  const buttons: string[] = [];
  if (providers.google) {
    buttons.push(providerButton(
      `/cli/oauth/google/start?device_code=${encodeURIComponent(deviceCode)}`,
      "Continue with Google",
      GOOGLE_ICON_SVG,
    ));
  }
  if (providers.github) {
    buttons.push(providerButton(
      `/cli/oauth/github/start?device_code=${encodeURIComponent(deviceCode)}`,
      "Continue with GitHub",
      GITHUB_ICON_SVG,
    ));
  }
  const cliBlock = deviceCode
    ? `<div class="device">
        Don't want to use a provider above? You can also
        <a href="/cli/github/start?device_code=${escapeHtml(deviceCode)}">sign in with a GitHub device code</a>.
      </div>`
    : `<div class="device">
        Run <code>rudder login</code> from the CLI to start a login session.
      </div>`;
  const noProviders = buttons.length === 0
    ? `<p class="empty">No OAuth providers are configured yet.${deviceCode ? " Use the GitHub device code option below." : ""}</p>`
    : "";
  const body = `
    <section class="hero">
      <h1>Sign in.</h1>
      <p class="lede">Connect this browser to Rudder Cloud so the CLI can launch and watch over cloud workers.</p>
      <div class="card">
        ${noProviders}
        ${buttons.join("\n")}
        ${cliBlock}
      </div>
    </section>
  `;
  sendHtml(res, renderShell({ title: "Sign in · Rudder Cloud", body }));
}

function providerButton(href: string, label: string, icon: string): string {
  return `<a class="provider" href="${escapeHtml(href)}"><span class="icon" aria-hidden="true">${icon}</span><span>${escapeHtml(label)}</span></a>`;
}

async function handleCliOAuthStart(url: URL, res: ServerResponse, provider: "google" | "github"): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }

  const providers = configuredProviders();
  if (!providers[provider]) {
    sendHtml(res, renderShell({
      title: "Provider unavailable · Rudder Cloud",
      body: `<section class="hero"><h1>Provider unavailable.</h1><p class="lede">${escapeHtml(provider)} login is not configured. Use the GitHub device code option instead.</p></section>`,
    }), 404);
    return;
  }

  const callbackURL = `${baseURL}/cli/approve?device_code=${encodeURIComponent(deviceCode)}`;
  refreshAuthHandler();
  const response = await auth.handler(new Request(`${authBaseURL}/sign-in/social`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      origin: baseURL,
    },
    body: JSON.stringify({
      provider,
      callbackURL,
      disableRedirect: true,
    }),
  }));
  const text = await response.text();
  let parsed: Json = text ? parseJson(text) : null;
  if (!response.ok) {
    const message = responseErrorMessage(parsed) ?? (text || `${response.status} ${response.statusText}`);
    sendHtml(res, renderShell({
      title: "Login failed · Rudder Cloud",
      body: `<section class="hero"><h1>Login failed.</h1><p class="lede">${escapeHtml(message)}</p></section>`,
    }), response.status);
    return;
  }
  const redirectURL = parsed && typeof parsed === "object" && !Array.isArray(parsed) && typeof parsed.url === "string"
    ? parsed.url
    : undefined;
  if (!redirectURL) {
    throw new Error("OAuth provider did not return an authorization URL");
  }
  const responseHeaders = response.headers as Headers & { getSetCookie?: () => string[] };
  for (const cookie of responseHeaders.getSetCookie?.() ?? splitSetCookieHeader(response.headers.get("set-cookie"))) {
    res.setHeader("Set-Cookie", appendHeader(res.getHeader("Set-Cookie"), cookie));
  }
  res.statusCode = 302;
  res.setHeader("Location", redirectURL);
  res.end();
}

const GOOGLE_ICON_SVG = `<svg viewBox="0 0 18 18" xmlns="http://www.w3.org/2000/svg"><path fill="#4285F4" d="M17.64 9.205c0-.639-.057-1.252-.164-1.841H9v3.481h4.844a4.14 4.14 0 0 1-1.796 2.716v2.258h2.908c1.702-1.567 2.684-3.875 2.684-6.614Z"/><path fill="#34A853" d="M9 18c2.43 0 4.467-.806 5.956-2.181l-2.908-2.258c-.806.54-1.836.86-3.048.86-2.344 0-4.328-1.584-5.036-3.711H.957v2.331A8.997 8.997 0 0 0 9 18Z"/><path fill="#FBBC05" d="M3.964 10.71A5.41 5.41 0 0 1 3.68 9c0-.593.102-1.17.284-1.71V4.959H.957A8.996 8.996 0 0 0 0 9c0 1.452.348 2.827.957 4.041l3.007-2.331Z"/><path fill="#EA4335" d="M9 3.58c1.321 0 2.508.454 3.44 1.345l2.582-2.581C13.463.891 11.426 0 9 0A8.997 8.997 0 0 0 .957 4.959L3.964 7.29C4.672 5.163 6.656 3.58 9 3.58Z"/></svg>`;
const GITHUB_ICON_SVG = `<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor"><path fill-rule="evenodd" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>`;

const BRAND_SVG = `<svg viewBox="0 0 64 64" xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><rect x="6" y="6" width="52" height="52" rx="12" fill="#fff"/><rect x="6" y="6" width="52" height="52" rx="12" fill="none" stroke="#111" stroke-width="3"/><path d="M18 20h12c8 0 13 4 13 11s-5 11-13 11h-6v9h-6V20Zm6 6v10h6c4 0 7-2 7-5s-3-5-7-5h-6Z" fill="#111"/><path d="M39 42l8 9" stroke="#111" stroke-width="6" stroke-linecap="round"/></svg>`;

function renderShell(options: { title: string; body: string; footer?: string }): string {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="theme-color" content="#ffffff">
    <title>${escapeHtml(options.title)}</title>
    <link rel="icon" href="https://rudder.viraat.dev/favicon.svg" type="image/svg+xml">
    <style>
      :root { color:#111; background:#fff; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; font-weight:300; }
      * { box-sizing: border-box; }
      body { margin:0; background:#fff; color:#111; min-height:100vh; }
      a { color:inherit; text-decoration-thickness:1px; text-underline-offset:4px; }
      .page { max-width: 1100px; margin: 0 auto; padding: 32px 24px 72px; }
      header { display:flex; align-items:center; justify-content:space-between; gap:24px; padding-bottom: 64px; }
      .brand { display:inline-flex; align-items:center; gap:12px; font-size:15px; text-decoration:none; color:#111; }
      .brand svg { width:28px; height:28px; }
      nav { display:flex; gap:18px; color:#555; font-size:14px; }
      nav a { color:#555; }
      h1 { margin:0; font-size: clamp(44px, 7vw, 88px); line-height:0.94; font-weight:300; letter-spacing:0; }
      .lede { margin: 28px 0 0; max-width: 620px; font-size: clamp(17px, 1.6vw, 21px); line-height:1.45; color:#333; font-weight:300; }
      .card { margin-top: 38px; max-width: 460px; border:1px solid #111; background:#fff; padding: 22px; box-shadow: 12px 12px 0 #111; }
      .provider { display:flex; align-items:center; gap:14px; width:100%; border:1px solid #111; background:#fff; color:#111; padding: 12px 14px; font:inherit; font-size:15px; cursor:pointer; text-decoration:none; transition: background .12s ease, color .12s ease; }
      .provider + .provider { margin-top: 10px; }
      .provider:hover, .provider:focus-visible { background:#111; color:#fff; outline:none; }
      .provider .icon { width:20px; height:20px; flex-shrink:0; display:inline-flex; align-items:center; justify-content:center; }
      .provider .icon svg { width:100%; height:100%; }
      .device { margin-top: 22px; padding-top: 18px; border-top: 1px dashed #111; color:#555; font-size: 14px; line-height:1.55; }
      .device code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background:#f2f2f2; padding: 1px 6px; }
      .device a { color:#111; }
      .empty { color:#555; font-size:15px; line-height:1.5; margin: 0 0 14px; }
      .code-display { font: 600 32px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; letter-spacing: 6px; margin: 20px 0 6px; }
      .muted { color:#666; font-size:14px; line-height:1.55; }
      .pill { display:inline-block; padding: 6px 10px; border:1px solid #111; font:600 12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; text-transform:uppercase; letter-spacing:1px; }
      .btn-primary { display:inline-block; margin-top: 18px; border: 1px solid #111; background:#111; color:#fff; padding: 10px 14px; font:inherit; font-size:14px; text-decoration:none; }
      .btn-primary:hover { background:#fff; color:#111; }
      code.kbd { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background:#f2f2f2; padding: 2px 6px; }
      footer { margin-top: 56px; padding-top: 22px; border-top: 1px solid #ddd; color:#666; font-size: 13px; line-height:1.55; max-width: 620px; }
      @media (max-width: 640px) {
        header { padding-bottom: 36px; }
        .card { box-shadow: 8px 8px 0 #111; padding: 18px; }
        .code-display { font-size: 26px; letter-spacing: 4px; }
      }
    </style>
  </head>
  <body>
    <div class="page">
      <header>
        <a class="brand" href="https://rudder.viraat.dev">${BRAND_SVG}<span>Rudder Cloud</span></a>
        <nav>
          <a href="https://rudder.viraat.dev">rudder.viraat.dev</a>
          <a href="https://github.com/viraatdas/rudder">GitHub</a>
        </nav>
      </header>
      <main>${options.body}</main>
      <footer>${options.footer ?? "You can close this tab once the CLI says you're signed in. Rudder Cloud uses Better Auth for OAuth — your provider tokens stay on this server."}</footer>
    </div>
  </body>
</html>`;
}

function renderGithubAppSetup(url: URL, res: ServerResponse): void {
  const org = url.searchParams.get("org")?.trim();
  const state = createSetupState();
  const action = org
    ? `https://github.com/organizations/${encodeURIComponent(org)}/settings/apps/new?state=${encodeURIComponent(state)}`
    : `https://github.com/settings/apps/new?state=${encodeURIComponent(state)}`;
  const manifest = {
    name: org ? `Rudder Cloud (${org})` : "Rudder Cloud",
    url: "https://rudder.viraat.dev",
    hook_attributes: {
      url: `${baseURL}/api/github/events`,
      active: false,
    },
    redirect_url: `${baseURL}/setup/github/callback`,
    callback_urls: [
      `${authBaseURL}/callback/github`,
    ],
    setup_url: `${baseURL}/setup/github`,
    description: "Rudder Cloud login and coding-agent orchestration.",
    public: false,
    request_oauth_on_install: false,
    default_permissions: {},
    default_events: [],
  };
  sendHtml(res, renderShell({
    title: "Set up GitHub OAuth · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>GitHub OAuth.</h1>
        <p class="lede">This creates a GitHub App from a manifest and stores its OAuth client ID and secret in Rudder Cloud's persisted state.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Callback URL</p>
          <p><code class="kbd">${escapeHtml(`${authBaseURL}/callback/github`)}</code></p>
          <form action="${escapeHtml(action)}" method="post" style="margin-top:18px">
            <input type="hidden" name="manifest" value="${escapeHtml(JSON.stringify(manifest))}">
            <button class="btn-primary" type="submit">Create GitHub App</button>
          </form>
        </div>
      </section>
    `,
  }));
}

async function handleGithubAppSetupCallback(url: URL, res: ServerResponse): Promise<void> {
  const code = url.searchParams.get("code") || "";
  const state = url.searchParams.get("state") || "";
  if (!code || !verifySetupState(state)) {
    sendHtml(res, renderShell({
      title: "Setup expired · Rudder Cloud",
      body: `<section class="hero"><h1>Setup expired.</h1><p class="lede">Open <code class="kbd">/setup/github</code> and try again.</p></section>`,
    }), 400);
    return;
  }
  const app = await githubManifestConversion(code);
  const clientId = typeof app.client_id === "string" ? app.client_id : undefined;
  const clientSecret = typeof app.client_secret === "string" ? app.client_secret : undefined;
  if (!clientId || !clientSecret) {
    throw new Error("GitHub manifest conversion did not return OAuth client credentials");
  }
  setSetting("github_client_id", clientId);
  setSetting("github_client_secret", clientSecret);
  refreshAuthHandler(true);
  await persistDatabaseToS3();
  sendHtml(res, renderShell({
    title: "GitHub OAuth ready · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>All set.</h1>
        <p class="lede">Rudder Cloud saved the GitHub App OAuth credentials. The sign-in page can now show the GitHub button.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Next</p>
          <p><a class="btn-primary" href="/cli/login">Go to sign in</a></p>
          <div class="device"><a href="/health">Check health</a></div>
        </div>
      </section>
    `,
  }));
}

async function handleOAuthCredentialSetup(
  req: IncomingMessage,
  res: ServerResponse,
  provider: "github" | "google",
): Promise<void> {
  const authContext = requireBearer(req);
  requireAdmin(authContext);
  const body = await readJsonBody(req);
  const clientId = stringField(body, "clientId") || stringField(body, "client_id");
  const clientSecret = stringField(body, "clientSecret") || stringField(body, "client_secret");
  if (!clientId || !clientSecret) {
    throw badRequest("clientId and clientSecret are required");
  }
  setSetting(`${provider}_client_id`, clientId.trim());
  setSetting(`${provider}_client_secret`, clientSecret.trim());
  refreshAuthHandler(true);
  await persistDatabaseToS3();
  sendJson(res, 200, {
    ok: true,
    provider,
    auth: configuredProviders(),
  });
}

async function handleCliGithubStart(url: URL, res: ServerResponse): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }
  const existing = githubBrowserLogins.get(deviceCode);
  const githubLogin = existing && existing.expiresAt > Date.now()
    ? existing
    : await startGithubBrowserLogin(deviceCode);
  renderGithubDevicePage(res, deviceCode, githubLogin);
}

async function handleCliGithubWait(url: URL, res: ServerResponse): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  const githubLogin = githubBrowserLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now() || !githubLogin || githubLogin.expiresAt < Date.now()) {
    githubBrowserLogins.delete(deviceCode);
    renderExpiredPage(res);
    return;
  }
  if (Date.now() < githubLogin.nextPollAt) {
    renderGithubDevicePage(res, deviceCode, githubLogin);
    return;
  }
  githubLogin.nextPollAt = Date.now() + githubLogin.intervalMs;
  const poll = await githubOAuthRequest<{
    access_token?: string;
    error?: string;
    error_description?: string;
    interval?: number;
  }>("https://github.com/login/oauth/access_token", {
    client_id: githubDeviceClientId,
    device_code: githubLogin.githubDeviceCode,
    grant_type: "urn:ietf:params:oauth:grant-type:device_code",
  });
  if (poll.access_token) {
    const user = await githubUser(poll.access_token);
    const issued = issueRudderToken(`github:${user.id}`, user.email);
    login.token = issued.token;
    login.accountId = issued.accountId;
    login.email = issued.email;
    githubBrowserLogins.delete(deviceCode);
    renderSuccessPage(res, login.email);
    return;
  }
  if (poll.error === "slow_down") {
    githubLogin.intervalMs = Math.max(githubLogin.intervalMs + 5000, (poll.interval ?? 5) * 1000);
    githubLogin.nextPollAt = Date.now() + githubLogin.intervalMs;
  } else if (poll.error && poll.error !== "authorization_pending") {
    githubBrowserLogins.delete(deviceCode);
    sendHtml(res, renderShell({
      title: "GitHub login failed · Rudder Cloud",
      body: `
        <section class="hero">
          <h1>GitHub login failed.</h1>
          <p class="lede">${escapeHtml(poll.error_description || poll.error)}</p>
          <div class="card">
            <p class="muted" style="margin-top:0">Run the CLI again to try once more.</p>
            <p><code class="kbd">rudder login</code></p>
          </div>
        </section>
      `,
    }), 400);
    return;
  }
  renderGithubDevicePage(res, deviceCode, githubLogin);
}

async function startGithubBrowserLogin(deviceCode: string): Promise<GithubBrowserLogin> {
  const start = await githubOAuthRequest<{
    device_code?: string;
    user_code?: string;
    verification_uri?: string;
    verification_uri_complete?: string;
    expires_in?: number;
    interval?: number;
    error?: string;
    error_description?: string;
  }>("https://github.com/login/device/code", {
    client_id: githubDeviceClientId,
    scope: "read:user user:email",
  });
  if (!start.device_code || !start.user_code || !start.verification_uri) {
    throw new Error(start.error_description || start.error || "GitHub device login failed");
  }
  const githubLogin: GithubBrowserLogin = {
    githubDeviceCode: start.device_code,
    userCode: start.user_code,
    verificationUri: start.verification_uri,
    verificationUriComplete: start.verification_uri_complete,
    expiresAt: Date.now() + (start.expires_in ?? 900) * 1000,
    intervalMs: Math.max(1000, (start.interval ?? 5) * 1000),
    nextPollAt: Date.now() + Math.max(1000, (start.interval ?? 5) * 1000),
  };
  githubBrowserLogins.set(deviceCode, githubLogin);
  return githubLogin;
}

function pruneLoginState(now = Date.now()): void {
  for (const [deviceCode, login] of deviceLogins) {
    if (login.expiresAt < now) {
      deviceLogins.delete(deviceCode);
      githubBrowserLogins.delete(deviceCode);
    }
  }
  for (const [deviceCode, login] of githubBrowserLogins) {
    if (login.expiresAt < now || !deviceLogins.has(deviceCode)) {
      githubBrowserLogins.delete(deviceCode);
    }
  }
}

const rateLimitWindows = new Map<string, { startedAt: number; count: number }>();

function checkGlobalRateLimit(key: string, limit: number, windowMs = 60_000): void {
  const now = Date.now();
  const current = rateLimitWindows.get(key);
  if (!current || now - current.startedAt >= windowMs) {
    rateLimitWindows.set(key, { startedAt: now, count: 1 });
    return;
  }
  current.count += 1;
  if (current.count > limit) {
    throw tooManyRequests("too many login attempts; try again shortly");
  }
}

async function githubOAuthRequest<T>(url: string, body: Record<string, string>): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(EXTERNAL_REQUEST_TIMEOUT_MS),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) || text.trim() || `${response.status} ${response.statusText}`);
  }
  return parsed as T;
}

async function githubManifestConversion(code: string): Promise<JsonRecord> {
  const response = await fetch(`https://api.github.com/app-manifests/${encodeURIComponent(code)}/conversions`, {
    method: "POST",
    headers: {
      Accept: "application/vnd.github+json",
      "User-Agent": "rudder-cloud",
      "X-GitHub-Api-Version": "2022-11-28",
    },
    signal: AbortSignal.timeout(EXTERNAL_REQUEST_TIMEOUT_MS),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) || text.trim() || `GitHub manifest conversion failed: ${response.status}`);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("GitHub manifest conversion returned an unexpected response");
  }
  return parsed;
}

function createSetupState(): string {
  const payload = Buffer.from(JSON.stringify({
    exp: Date.now() + 10 * 60 * 1000,
    nonce: randomBytes(16).toString("base64url"),
  })).toString("base64url");
  return `${payload}.${setupStateSignature(payload)}`;
}

function verifySetupState(state: string): boolean {
  const [payload, signature] = state.split(".");
  if (!payload || !signature) {
    return false;
  }
  const expected = setupStateSignature(payload);
  const signatureBuffer = Buffer.from(signature);
  const expectedBuffer = Buffer.from(expected);
  if (signatureBuffer.length !== expectedBuffer.length || !timingSafeEqual(signatureBuffer, expectedBuffer)) {
    return false;
  }
  try {
    const parsed = JSON.parse(Buffer.from(payload, "base64url").toString("utf8")) as { exp?: unknown };
    return typeof parsed.exp === "number" && parsed.exp > Date.now();
  } catch {
    return false;
  }
}

function setupStateSignature(payload: string): string {
  return createHmac("sha256", requiredEnv("BETTER_AUTH_SECRET")).update(payload).digest("base64url");
}

function renderGithubDevicePage(res: ServerResponse, deviceCode: string, githubLogin: GithubBrowserLogin): void {
  const href = githubLogin.verificationUriComplete || githubLogin.verificationUri;
  const refreshSec = Math.ceil(githubLogin.intervalMs / 1000);
  const html = renderShell({
    title: "Authorize GitHub · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Authorize on GitHub.</h1>
        <p class="lede">Open GitHub, paste this code, then come back. This tab will finish on its own.</p>
        <div class="card">
          <span class="pill">Device code</span>
          <div class="code-display">${escapeHtml(githubLogin.userCode)}</div>
          <p class="muted">Waiting for GitHub approval&hellip;</p>
          <a class="btn-primary" href="${escapeHtml(href)}" target="_blank" rel="noreferrer">Open GitHub</a>
          <div class="device">Stuck? Run <code>rudder login</code> again from the CLI.</div>
        </div>
      </section>
    `,
  }).replace(
    "</head>",
    `<meta http-equiv="refresh" content="${refreshSec};url=/cli/github/wait?device_code=${encodeURIComponent(deviceCode)}"></head>`,
  );
  sendHtml(res, html);
}

async function handleCliApprove(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }
  const session = await getBetterAuthSession(req);
  if (!session?.user) {
    renderLoginPage(url, res);
    return;
  }
  if (typeof session.user.id !== "string" || !session.user.id) {
    throw unauthorized();
  }
  // Only bake the email into the (admin-trusted) rdr_ token when it is VERIFIED, so
  // the token path shares the same invariant as requireAdminRequest's session path:
  // an unverified email can never become an admin token. (better-auth is social-only
  // today, but enforce it at the trust boundary rather than relying on that.)
  const verifiedEmail =
    typeof session.user.email === "string" &&
    (session.user as { emailVerified?: unknown }).emailVerified === true
      ? session.user.email
      : undefined;
  const issued = issueRudderToken(
    session.user.id,
    verifiedEmail,
  );
  login.token = issued.token;
  login.accountId = issued.accountId;
  login.email = verifiedEmail;
  renderSuccessPage(res, login.email);
}

async function handleAdminWorkspaceGc(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
): Promise<void> {
  await requireAdminRequest(req);
  ensureFlyConfigured();
  const dryRun = url.searchParams.get("dry") === "1" || url.searchParams.get("dryRun") === "1";
  const now = new Date().toISOString();
  const workspaceRows = database
    .prepare("select * from rudder_workspaces")
    .all() as Record<string, unknown>[];
  const sailRows = database
    .prepare("select * from rudder_sails")
    .all() as Record<string, unknown>[];

  const reclaimedWorkspaces: { id: string; machineId: string; previousStatus: string }[] = [];
  const reclaimedSails: { id: string; machineId: string; previousStatus: string }[] = [];
  const reclaimedVolumes: { id: string; volumeId: string }[] = [];
  const skipped: { kind: "workspace" | "sail"; id: string; reason: string }[] = [];
  const errors: { kind: "workspace" | "sail"; id: string; error: string }[] = [];

  const update = database.prepare(
    "update rudder_workspaces set status = ?, machine_id = NULL, machine_state = 'destroyed', updated_at = ? where id = ?",
  );
  const clearVolume = database.prepare(
    "update rudder_workspaces set volume_id = NULL, updated_at = ? where id = ?",
  );
  const updateSailRow = database.prepare(
    "update rudder_sails set status = ?, machine_id = NULL, machine_state = 'destroyed', updated_at = ? where id = ?",
  );

  for (const row of workspaceRows) {
    const id = String(row.id);
    const status = String(row.status);
    const machineId = optionalString(row.machine_id);
    if (!machineId) {
      skipped.push({ kind: "workspace", id, reason: "no machineId" });
      continue;
    }
    try {
      const exists = await flyMachineExists(machineId);
      if (exists) {
        skipped.push({ kind: "workspace", id, reason: "machine still present on Fly" });
        continue;
      }
      reclaimedWorkspaces.push({ id, machineId, previousStatus: status });
      const volumeId = optionalString(row.volume_id);
      if (volumeId) {
        reclaimedVolumes.push({ id, volumeId });
      }
      if (!dryRun) {
        if (volumeId) {
          await destroyWorkspaceVolume(volumeId);
          clearVolume.run(now, id);
        }
        update.run("stopped", now, id);
      }
    } catch (error) {
      errors.push({
        kind: "workspace",
        id,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  for (const row of sailRows) {
    const id = String(row.id);
    const status = String(row.status);
    const machineId = optionalString(row.machine_id);
    if (!machineId) {
      skipped.push({ kind: "sail", id, reason: "no machineId" });
      continue;
    }
    if (status === "completed" || status === "failed") {
      skipped.push({ kind: "sail", id, reason: `terminal status: ${status}` });
      continue;
    }
    try {
      const exists = await flyMachineExists(machineId);
      if (exists) {
        skipped.push({ kind: "sail", id, reason: "machine still present on Fly" });
        continue;
      }
      reclaimedSails.push({ id, machineId, previousStatus: status });
      if (!dryRun) {
        updateSailRow.run("completed", now, id);
      }
    } catch (error) {
      errors.push({
        kind: "sail",
        id,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  if (!dryRun && (reclaimedWorkspaces.length || reclaimedSails.length)) {
    markDatabaseDirty();
  }

  sendJson(res, 200, {
    ok: true,
    dryRun,
    reclaimedWorkspaces,
    reclaimedSails,
    reclaimedVolumes,
    skipped,
    errors,
  });
}

async function flyMachineExists(machineId: string): Promise<boolean> {
  const machine = await getFlyMachineIfPresent(machineId);
  return Boolean(machine && machine.state !== "destroyed" && machine.state !== "destroying");
}

async function requireAdminRequest(req: IncomingMessage): Promise<void> {
  const adminToken = process.env.RUDDER_ADMIN_TOKEN || "";
  const authHeader = req.headers.authorization || "";
  const bearer = authHeader.startsWith("Bearer ")
    ? authHeader.slice("Bearer ".length).trim()
    : "";
  if (adminToken && bearer && timingSafeEqualString(bearer, adminToken)) {
    return;
  }
  // Fallback: admin-email rudder bearer token.
  if (bearer.startsWith("rdr_")) {
    const ctx = requireBearer(req);
    requireAdmin(ctx);
    return;
  }
  // Fallback: better-auth session cookie from an admin email. The email must be
  // verified — an unverified provider email must never satisfy the admin check.
  const session = await getBetterAuthSession(req);
  const email = typeof session?.user?.email === "string" ? session.user.email.toLowerCase() : undefined;
  const emailVerified = (session?.user as { emailVerified?: unknown } | undefined)?.emailVerified === true;
  if (email && emailVerified && adminEmails.has(email)) {
    return;
  }
  throw unauthorized();
}

function timingSafeEqualString(a: string, b: string): boolean {
  const ba = Buffer.from(a);
  const bb = Buffer.from(b);
  if (ba.length !== bb.length) {
    return false;
  }
  return timingSafeEqual(ba, bb);
}

async function handleSailApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const heartbeatMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/heartbeat$/);
  if (req.method === "POST" && heartbeatMatch) {
    await handleWorkerHeartbeat(req, res, heartbeatMatch[1]);
    return;
  }

  const snapshotUrlMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/snapshot-url$/);
  if (req.method === "GET" && snapshotUrlMatch) {
    await handleSailSnapshotUrl(req, res, snapshotUrlMatch[1]);
    return;
  }

  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/sail") {
    await refreshAccountSails(authContext.accountId);
    sendJson(res, 200, { sails: listAccountSails(authContext.accountId) });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/launch") {
    const sail = await withSnapshotRequestBudget(req, async () => {
      const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
      return await withOperationLock(
        `account:${authContext.accountId}:sail-launch`,
        async () => createSail(authContext.accountId, body),
      );
    });
    sendJson(res, 200, sail);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/onload") {
    const sail = await withSnapshotRequestBudget(req, async () => {
      const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
      return await withOperationLock(
        `account:${authContext.accountId}:sail-launch`,
        async () => createSail(authContext.accountId, body, stringField(body, "runId")),
      );
    });
    sendJson(res, 200, sail);
    return;
  }
  const bootstrapMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/bootstrap$/);
  if (req.method === "POST" && bootstrapMatch) {
    const sail = getAccountSail(bootstrapMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    const next = await withOperationLock(
      `sail:${sail.id}`,
      async () => refreshByoVmBootstrap(sail, authContext.accountId),
    );
    sendJson(res, 200, next);
    return;
  }
  const inputMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/input$/);
  if (req.method === "POST" && inputMatch) {
    const sail = getAccountSail(inputMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    const body = await readJsonBody(req);
    const text = stringField(body, "text");
    if (typeof text !== "string" || text.length === 0) {
      sendJson(res, 400, { error: "text is required" });
      return;
    }
    if (Buffer.byteLength(text, "utf8") > 64 * 1024) {
      throw payloadTooLarge();
    }
    const submit = body && typeof body === "object" && !Array.isArray(body) && body.submit === false ? false : true;
    const delivered = sendInputToChannel("sail", sail.id, text, submit);
    sendJson(res, delivered ? 200 : 409, delivered
      ? { delivered: true }
      : { delivered: false, error: "instance is not connected" });
    return;
  }
  const outputMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/output$/);
  if (req.method === "GET" && outputMatch) {
    const sail = getAccountSail(outputMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    sendJson(res, 200, {
      id: sail.id,
      connected: instanceHasLiveWorker("sail", sail.id),
      output: readChannelOutput("sail", sail.id),
    });
    return;
  }
  const match = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/(pause|resume|onload|stop)$/);
  if (req.method === "POST" && match) {
    const sail = getAccountSail(match[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    const next = await withOperationLock(
      `sail:${sail.id}`,
      async () => mutateSail(sail, authContext.accountId, match[2]),
    );
    sendJson(res, 200, next);
    return;
  }
  const deleteMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/delete$/);
  if (req.method === "POST" && deleteMatch) {
    const sail = getAccountSail(deleteMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    await withOperationLock(`sail:${sail.id}`, async () => deleteSail(sail, authContext.accountId));
    sendJson(res, 200, { ok: true, id: sail.id, deleted: true });
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

async function handleWorkspaceApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const heartbeatMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/heartbeat$/);
  if (req.method === "POST" && heartbeatMatch) {
    await handleWorkspaceHeartbeat(req, res, heartbeatMatch[1]);
    return;
  }

  const snapshotUrlMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/snapshot-url$/);
  if (req.method === "GET" && snapshotUrlMatch) {
    await handleWorkspaceSnapshotUrl(req, res, snapshotUrlMatch[1]);
    return;
  }

  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/workspace") {
    await refreshAccountWorkspaces(authContext.accountId);
    sendJson(res, 200, {
      workspaces: listAccountWorkspaces(authContext.accountId).map(annotateWorkspaceClients),
    });
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/rudder/workspace/lookup") {
    const key = url.searchParams.get("key");
    if (!key) {
      sendJson(res, 400, { error: "key is required" });
      return;
    }
    const row = findWorkspaceByKey.get(authContext.accountId, key) as
      | Record<string, unknown>
      | undefined;
    if (!row) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    await refreshWorkspaceRow(rowToWorkspace(row));
    const refreshed = findWorkspaceByKey.get(authContext.accountId, key) as Record<string, unknown> | undefined;
    sendJson(res, 200, annotateWorkspaceClients(rowToWorkspace(refreshed ?? row)));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/workspace/attach") {
    const result = await withSnapshotRequestBudget(req, async () => {
      const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
      return await ensureWorkspaceForAttach(authContext.accountId, body);
    });
    sendJson(res, 200, result);
    return;
  }
  const stopMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/(stop|pause|resume)$/);
  if (req.method === "POST" && stopMatch) {
    const workspace = getAccountWorkspace(stopMatch[1], authContext.accountId);
    if (!workspace) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    const next = await withOperationLock(
      `workspace:${workspace.accountId}:${workspace.workspaceKey}`,
      async () => mutateWorkspace(workspace, stopMatch[2]),
    );
    sendJson(res, 200, next);
    return;
  }
  const deleteMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/delete$/);
  if (req.method === "POST" && deleteMatch) {
    const workspace = getAccountWorkspace(deleteMatch[1], authContext.accountId);
    if (!workspace) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    await withOperationLock(
      `workspace:${workspace.accountId}:${workspace.workspaceKey}`,
      async () => deleteWorkspace(workspace),
    );
    sendJson(res, 200, { ok: true, id: workspace.id, deleted: true });
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

async function createSail(accountId: string, body: Json, preferredId?: string): Promise<Sail> {
  const runtime = sailRuntimeFromBody(body);
  ensureCloudRuntimeConfigured(runtime);
  const active = countActiveSailsForAccount.get(accountId) as { count?: number } | undefined;
  if ((active?.count ?? 0) >= maxActiveSailsPerAccount) {
    throw tooManyRequests(`active sail limit reached (${maxActiveSailsPerAccount}); stop or delete an instance first`);
  }
  const now = new Date().toISOString();
  const id = preferredId ? validatePreferredSailId(preferredId) : uniqueSailId(stringField(body, "name"));
  if (findSailById.get(id)) {
    throw conflict(`sail already exists: ${id}`);
  }
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const task = stringField(body, "task");
  const repoName = stringField(body, "repoName");
  if (task && Buffer.byteLength(task, "utf8") > 64 * 1024) {
    throw payloadTooLarge();
  }
  if (repoName && Buffer.byteLength(repoName, "utf8") > 255) {
    throw badRequest("repoName is too long");
  }
  const snapshot = await storeSnapshot(accountId, body);
  const snapshotInput = objectField(body, "snapshot");
  const manifest = snapshotInput ? objectField(snapshotInput, "manifest") : undefined;
  const manifestRepo = manifest ? objectField(manifest, "repo") : undefined;
  const branch = manifestRepo ? stringField(manifestRepo, "branch") : undefined;
  try {
    insertSail.run({
      id,
      accountId,
      status: "queued",
      runtime,
      repoName: repoName ?? null,
      task: task ?? null,
      branch: branch ?? null,
      machineId: null,
      machineState: runtime === "byo-vm" ? "bootstrap-pending" : null,
      snapshotKey: snapshot.key,
      manifestJson: JSON.stringify(manifest ?? {}),
      workerTokenHash: tokenHash(workerToken),
      lastActivityAt: now,
      lastHeartbeatAt: null,
      createdAt: now,
      updatedAt: now,
    });
  } catch (error) {
    await deleteSnapshotBestEffort(snapshot.key);
    if (isSqliteConstraint(error)) {
      throw conflict(`sail already exists: ${id}`);
    }
    throw error;
  }
  markDatabaseDirty();

  if (runtime === "byo-vm") {
    const sail = getAccountSail(id, accountId) ?? {
      id,
      status: "queued",
      runtime,
      repoName,
      task,
      branch,
      machineState: "bootstrap-pending",
      snapshotKey: snapshot.key,
      createdAt: now,
      updatedAt: now,
    };
    const result = {
      ...sail,
      bootstrapCommand: await byoVmBootstrapCommand({
        sailId: id,
        accountId,
        snapshotKey: snapshot.key,
        workerToken,
        task,
        repoName,
      }),
    };
    void announceSailToSlack(id, accountId, task, repoName, runtime);
    return result;
  }

  let machine: FlyMachine;
  try {
    const snapshotUrl = await signedSnapshotUrl(snapshot.key);
    machine = await createFlyMachine({
      sailId: id,
      accountId,
      snapshotUrl,
      workerToken,
      task,
      repoName,
    });
    if (!machine.id) {
      throw new Error("Fly create did not return a machine id");
    }
    const status = flyStateToSailStatus(machine.state);
    updateSail.run({
      id,
      accountId,
      status,
      machineId: machine.id,
      machineState: machine.state ?? null,
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
  } catch (error) {
    updateSail.run({
      id,
      accountId,
      status: "failed",
      machineId: null,
      machineState: "provision-failed",
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    throw error;
  }
  const status = flyStateToSailStatus(machine.state);
  void announceSailToSlack(id, accountId, task, repoName, runtime);
  return getAccountSail(id, accountId) ?? {
    id,
    status,
    runtime,
    repoName,
    task,
    branch,
    machineId: machine.id,
    machineState: machine.state,
    snapshotKey: snapshot.key,
    createdAt: now,
    updatedAt: now,
  };
}

function validatePreferredSailId(value: string): string {
  const id = value.trim();
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(id)) {
    throw badRequest("runId must be 1-128 URL-safe characters");
  }
  return id;
}

function uniqueSailId(name?: string): string {
  const base = slugForSailId(name) || `${cloudWord()}-${cloudWord()}`;
  let id = base;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    if (!findSailById.get(id)) {
      return id;
    }
    id = `${base}-${randomBytes(2).toString("hex")}`;
  }
  return `cloud-${randomBytes(5).toString("hex")}`;
}

function slugForSailId(value?: string): string {
  const slug = (value || "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 42)
    .replace(/-+$/g, "");
  return slug ? `cloud-${slug}` : "";
}

function cloudWord(): string {
  const words = [
    "amber",
    "atlas",
    "bright",
    "harbor",
    "orbit",
    "rapid",
    "river",
    "signal",
    "silver",
    "summit",
    "swift",
    "wave",
  ];
  return words[randomBytes(1)[0] % words.length] || "cloud";
}

async function storeSnapshot(accountId: string, body: Json): Promise<{ key: string }> {
  ensureS3Configured();
  const snapshot = objectField(body, "snapshot");
  const base64 = snapshot ? stringField(snapshot, "base64") : undefined;
  const contentType = snapshot ? stringField(snapshot, "contentType") || "application/gzip" : "application/gzip";
  if (!base64) {
    throw badRequest("snapshot.base64 is required");
  }
  if (base64.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64)) {
    throw badRequest("snapshot.base64 is invalid");
  }
  const buffer = Buffer.from(base64, "base64");
  if (buffer.length < 2 || buffer[0] !== 0x1f || buffer[1] !== 0x8b) {
    throw badRequest("snapshot must be a gzip archive");
  }
  const key = `snapshots/${accountId}/${new Date().toISOString().slice(0, 10)}/${randomUUID()}.tgz`;
  await s3.send(new PutObjectCommand({
    Bucket: snapshotBucket,
    Key: key,
    Body: buffer,
    ContentType: contentType,
    ServerSideEncryption: "AES256",
  }));
  return { key };
}

async function signedSnapshotUrl(key: string): Promise<string> {
  ensureS3Configured();
  return await getSignedUrl(
    s3,
    new GetObjectCommand({
      Bucket: snapshotBucket,
      Key: key,
    }),
    { expiresIn: 60 * 60 },
  );
}

async function createFlyMachine(params: {
  sailId: string;
  accountId: string;
  snapshotUrl: string;
  workerToken: string;
  task?: string;
  repoName?: string;
}): Promise<FlyMachine> {
  ensureFlyConfigured();
  const machine = await flyRequest<FlyMachine>(`/v1/apps/${encodeURIComponent(flyAppName)}/machines`, {
    method: "POST",
    body: {
      name: flyMachineName("rudder", params.sailId),
      region: flyRegion,
      config: {
        image: flyWorkerImage,
        env: {
          RUDDER_SAIL_ID: params.sailId,
          RUDDER_ACCOUNT_ID: params.accountId,
          RUDDER_CLOUD_URL: baseURL,
          RUDDER_WORKER_TOKEN: params.workerToken,
          RUDDER_SNAPSHOT_URL: params.snapshotUrl,
          RUDDER_TASK: params.task || "",
          RUDDER_REPO_NAME: params.repoName || "",
        },
        guest: {
          cpu_kind: flyWorkerCpuKind,
          cpus: flyWorkerCpus,
          memory_mb: flyWorkerMemoryMb,
        },
        restart: {
          policy: "no",
        },
        // Sails are task-scoped. Once the supervisor exits, there is no state
        // worth keeping on the ephemeral root disk; Fly should reclaim it.
        auto_destroy: true,
      },
      skip_launch: false,
    },
  });
  return machine;
}

async function mutateSail(sail: Sail, accountId: string, action: string): Promise<Sail> {
  if (sail.runtime === "fly") {
    return await mutateFlySail(sail, accountId, action);
  }
  if (action === "stop") {
    updateSail.run({
      id: sail.id,
      accountId,
      status: "completed",
      machineId: sail.machineId ?? null,
      machineState: "stopped",
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    return getAccountSail(sail.id, accountId) ?? sail;
  }
  throw badRequest("BYO VM sails cannot be paused or resumed from Rudder Cloud. Stop the worker on your VM, or use stop to mark it stopped.");
}

async function mutateFlySail(sail: Sail, accountId: string, action: string): Promise<Sail> {
  if (!sail.machineId) {
    throw badRequest("sail does not have a Fly machine yet");
  }
  let machine: FlyMachine;
  if (action === "pause") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/suspend`,
      { method: "POST", body: {} },
    ) ?? {};
  } else if (action === "resume" || action === "onload") {
    machine = await startFlyMachine(sail.machineId, `sail ${sail.id}`);
  } else if (action === "stop") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGINT", timeout: "10s" } },
    ) ?? {};
  } else {
    throw badRequest(`unsupported sail action: ${action}`);
  }
  updateSail.run({
    id: sail.id,
    accountId,
    status: action === "pause"
      ? "paused"
      : action === "stop"
        ? "completed"
        : flyStateToSailStatus(machine.state),
    machineId: sail.machineId,
    machineState: machine.state ?? null,
    updatedAt: new Date().toISOString(),
  });
  markDatabaseDirty();
  return getAccountSail(sail.id, accountId) ?? sail;
}

async function deleteSail(sail: Sail, accountId: string): Promise<void> {
  if (sail.runtime === "fly" && sail.machineId) {
    await destroyFlyMachine(sail.machineId);
  }
  closeChannel("sail", sail.id, "instance deleted");
  deleteSailRow.run(sail.id, accountId);
  lastActivityWrites.delete(`sail:${sail.id}`);
  markDatabaseDirty();
  await deleteSnapshotBestEffort(sail.snapshotKey);
}

async function deleteWorkspace(workspace: Workspace): Promise<void> {
  if (workspace.machineId) {
    await destroyFlyMachine(workspace.machineId, true);
  }
  if (workspace.volumeId) {
    await destroyWorkspaceVolume(workspace.volumeId);
  }
  closeChannel("workspace", workspace.id, "workspace deleted");
  deleteWorkspaceRow.run(workspace.id, workspace.accountId);
  lastActivityWrites.delete(`workspace:${workspace.id}`);
  markDatabaseDirty();
  await deleteSnapshotBestEffort(workspace.snapshotKey);
}

async function destroyFlyMachine(machineId: string, waitUntilDestroyed = false): Promise<void> {
  try {
    await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}?force=true`,
      { method: "DELETE" },
    );
  } catch (error) {
    if (!isHttpStatus(error, 404)) {
      throw error;
    }
  }
  if (waitUntilDestroyed) {
    try {
      await flyRequest<JsonRecord>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}/wait?state=destroyed&timeout=10`,
        { method: "GET" },
      );
    } catch (error) {
      if (!isHttpStatus(error, 404)) {
        throw error;
      }
    }
  }
}

async function deleteSnapshotBestEffort(key: string | undefined): Promise<void> {
  if (!key || !snapshotBucket) {
    return;
  }
  try {
    await s3.send(new DeleteObjectCommand({ Bucket: snapshotBucket, Key: key }));
  } catch (error) {
    console.warn(`delete snapshot ${key} failed: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function closeChannel(kind: ChannelKind, id: string, reason: string): void {
  const map = channelMap(kind);
  const channel = map.get(id);
  if (!channel) {
    return;
  }
  if (channel.cleanupTimer) {
    clearTimeout(channel.cleanupTimer);
  }
  const sockets = channel.worker ? [channel.worker, ...channel.clients] : [...channel.clients];
  map.delete(id);
  for (const ws of sockets) {
    try {
      ws.close(1000, reason);
    } catch {
      ws.terminate();
    }
  }
}

function disconnectWorker(kind: ChannelKind, id: string, reason: string): void {
  const channel = channelMap(kind).get(id);
  const worker = channel?.worker;
  if (!worker) {
    return;
  }
  try {
    worker.close(4001, reason);
  } catch {
    worker.terminate();
  }
}

async function refreshByoVmBootstrap(sail: Sail, accountId: string): Promise<Sail> {
  if (sail.runtime !== "byo-vm") {
    throw badRequest("bootstrap is only available for BYO VM sails");
  }
  if (!sail.snapshotKey) {
    throw badRequest("sail does not have a snapshot");
  }
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  updateWorkerToken.run({
    id: sail.id,
    accountId,
    workerTokenHash: tokenHash(workerToken),
    updatedAt: now,
  });
  markDatabaseDirty();
  disconnectWorker("sail", sail.id, "credentials rotated");
  const next = getAccountSail(sail.id, accountId) ?? { ...sail, updatedAt: now };
  return {
    ...next,
    bootstrapCommand: await byoVmBootstrapCommand({
      sailId: sail.id,
      accountId,
      snapshotKey: sail.snapshotKey,
      workerToken,
      task: sail.task,
      repoName: sail.repoName,
    }),
  };
}

async function byoVmBootstrapCommand(params: {
  sailId: string;
  accountId: string;
  snapshotKey: string;
  workerToken: string;
  task?: string;
  repoName?: string;
}): Promise<string> {
  const snapshotUrl = await signedSnapshotUrl(params.snapshotKey);
  const env: Array<[string, string]> = [
    ["RUDDER_SAIL_ID", params.sailId],
    ["RUDDER_ACCOUNT_ID", params.accountId],
    ["RUDDER_CLOUD_URL", baseURL],
    ["RUDDER_WORKER_TOKEN", params.workerToken],
    ["RUDDER_SNAPSHOT_URL", snapshotUrl],
    ["RUDDER_TASK", params.task || ""],
    ["RUDDER_REPO_NAME", params.repoName || ""],
  ];
  const armWorkerImage = imageWithTag(flyWorkerImage, "arm64");
  const dockerLines = [
    // BYOC hosts commonly cache `latest`; always pull so a control-plane deploy
    // cannot keep launching an old, protocol-incompatible worker image.
    "docker run --rm --pull=always",
    ...env.map(([key, value]) => `  -e ${key}=${shellQuote(value)}`),
    "  \"$RUDDER_WORKER_IMAGE\"",
  ];
  const dockerCommand = dockerLines.map((line, index) => index < dockerLines.length - 1 ? `${line} \\` : line).join("\n");
  return [
    `RUDDER_WORKER_IMAGE=${shellQuote(flyWorkerImage)}`,
    "case \"$(uname -m)\" in",
    `  aarch64|arm64) RUDDER_WORKER_IMAGE=${shellQuote(armWorkerImage)} ;;`,
    "esac",
    dockerCommand,
  ].join("\n");
}

async function handleWorkerHeartbeat(req: IncomingMessage, res: ServerResponse, sailId: string): Promise<void> {
  const authRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!authRow) {
    sendJson(res, 404, { error: "sail not found" });
    return;
  }
  requireWorkerBearer(req, authRow);
  const body = await readJsonBody(req);
  const sailRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!sailRow) {
    sendJson(res, 404, { error: "sail not found" });
    return;
  }
  // A bootstrap rotation may happen while the request body is in flight.
  requireWorkerBearer(req, sailRow);
  const state = stringField(body, "state");
  const reportedStatus: SailStatus = state === "completed"
    ? "completed"
    : state === "failed"
      ? "failed"
      : "running";
  const now = new Date().toISOString();
  const previousStatus = String(sailRow.status) as SailStatus;
  // Late periodic heartbeats must not resurrect a completed/failed/paused sail.
  // Resume updates the row to running before the worker can reconnect, so preserving
  // paused here does not block a legitimate resume.
  const status = previousStatus === "completed" || previousStatus === "failed"
    ? previousStatus
    : reportedStatus === "running" && previousStatus === "paused"
      ? previousStatus
      : reportedStatus;
  updateHeartbeat.run({
    id: sailId,
    status,
    machineId: stringField(body, "machineId") ?? null,
    machineState: status === reportedStatus ? state || status : optionalString(sailRow.machine_state) ?? status,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
  markDatabaseDirty();
  // On the transition into a terminal state, post the final result + a tail of
  // output into the instance's Slack thread.
  if ((status === "completed" || status === "failed") && previousStatus !== status) {
    const threadTs = optionalString(sailRow.slack_thread_ts);
    const icon = status === "completed" ? "✅" : "❌";
    const tail = readChannelOutput("sail", sailId);
    if (threadTs && slackAccountIds.has(String(sailRow.account_id))) {
      void postToSlack(
        `${icon} *${sailId}* ${status}.${tail ? `\n${formatOutputForSlack(tail)}` : ""}`,
        threadTs,
      );
    }
  }
  sendJson(res, 200, { ok: true, status });
}

async function refreshAccountSails(accountId: string): Promise<void> {
  if (Date.now() - (lastSailRefreshAt.get(accountId) ?? 0) < 5_000) {
    return;
  }
  const existing = sailRefreshes.get(accountId);
  if (existing) {
    await existing;
    return;
  }
  const refresh = refreshAccountSailsCore(accountId).then(() => {
    lastSailRefreshAt.set(accountId, Date.now());
  }).finally(() => {
    if (sailRefreshes.get(accountId) === refresh) {
      sailRefreshes.delete(accountId);
    }
  });
  sailRefreshes.set(accountId, refresh);
  await refresh;
}

const sailRefreshes = new Map<string, Promise<void>>();
const lastSailRefreshAt = new Map<string, number>();

async function refreshAccountSailsCore(accountId: string): Promise<void> {
  const sails = listAccountSails(accountId);
  await mapWithConcurrency(sails, 8, async (sail) => {
    await withOperationLock(`sail:${sail.id}`, async () => {
    if (sail.runtime !== "fly" || !flyApiToken || !flyAppName) {
      return;
    }
    if (sail.status === "completed" || sail.status === "failed") {
      return;
    }
    if (!sail.machineId) {
      return;
    }
    const machine = await getFlyMachineIfPresent(sail.machineId).catch(() => undefined);
    if (machine === undefined) {
      return;
    }
    if (machine === null) {
      updateSail.run({
        id: sail.id,
        accountId,
        status: "completed",
        machineId: sail.machineId,
        machineState: "destroyed",
        updatedAt: new Date().toISOString(),
      });
      markDatabaseDirty();
      return;
    }
    const latestSail = getAccountSail(sail.id, accountId) ?? sail;
    if (shouldPauseStaleSail(latestSail) && !["suspended", "stopped", "stopping"].includes(machine.state ?? "")) {
      const suspended = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/suspend`,
        { method: "POST", body: {} },
      ).catch(() => null);
      if (!suspended) {
        return;
      }
      const now = new Date().toISOString();
      updateSail.run({
        id: sail.id,
        accountId,
        status: "paused",
        machineId: sail.machineId,
        machineState: "suspended",
        updatedAt: now,
      });
      markDatabaseDirty();
      return;
    }
    const refreshedStatus = flyStateToSailStatus(machine.state);
    updateSail.run({
      id: sail.id,
      accountId,
      status: refreshedStatus === "queued" && latestSail.status === "running" ? "running" : refreshedStatus,
      machineId: sail.machineId,
      machineState: machine.state ?? null,
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    });
  });
}

const operationTails = new Map<string, Promise<void>>();

async function withOperationLock<T>(key: string, operation: () => Promise<T>): Promise<T> {
  const previous = operationTails.get(key) ?? Promise.resolve();
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const tail = previous.catch(() => undefined).then(() => gate);
  operationTails.set(key, tail);
  await previous.catch(() => undefined);
  try {
    return await operation();
  } finally {
    release();
    if (operationTails.get(key) === tail) {
      operationTails.delete(key);
    }
  }
}

function shouldPauseStaleSail(sail: Sail): boolean {
  if (sail.status !== "running" || !idlePauseMs || idlePauseMs < 1000) {
    return false;
  }
  const lastActivity = sail.lastActivityAt ?? sail.lastHeartbeatAt ?? sail.createdAt;
  const lastSeen = Date.parse(lastActivity);
  return Number.isFinite(lastSeen) && Date.now() - lastSeen > idlePauseMs;
}

async function ensureWorkspaceForAttach(accountId: string, body: Json): Promise<JsonRecord> {
  ensureCloudRuntimeConfigured("fly");
  const workspaceKey = stringField(body, "workspaceKey")?.trim();
  if (!workspaceKey || workspaceKey.length < 4 || workspaceKey.length > 128) {
    throw badRequest("workspaceKey is required (4-128 chars)");
  }
  if (!/^[A-Za-z0-9._:@/+\-=]+$/.test(workspaceKey)) {
    throw badRequest("workspaceKey contains unsupported characters");
  }
  return await withOperationLock(`workspace:${accountId}:${workspaceKey}`, async () => {
    const repoName = stringField(body, "repoName");
    const existingRow = findWorkspaceByKey.get(accountId, workspaceKey) as Record<string, unknown> | undefined;
    if (existingRow) {
      const existing = rowToWorkspace(existingRow);
      return await reuseOrRestartWorkspace(existing, body, repoName);
    }
    return await withOperationLock(`account:${accountId}:workspace-create`, async () => {
      // Another workspace-key operation for this account may have consumed the
      // final quota slot while we waited for the account-wide creation lock.
      return await createWorkspace(accountId, workspaceKey, body, repoName);
    });
  });
}

async function createWorkspace(
  accountId: string,
  workspaceKey: string,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  const count = countWorkspacesForAccount.get(accountId) as { count?: number } | undefined;
  if ((count?.count ?? 0) >= maxWorkspacesPerAccount) {
    throw tooManyRequests(`workspace limit reached (${maxWorkspacesPerAccount}); delete a workspace first`);
  }
  const requestedRegion = stringField(body, "region");
  const region = sanitizeRegion(requestedRegion);
  if (requestedRegion && !region) {
    throw badRequest("region must be a three-letter Fly region code");
  }
  if (repoName && Buffer.byteLength(repoName, "utf8") > 255) {
    throw badRequest("repoName is too long");
  }
  const snapshot = await storeSnapshot(accountId, body);
  const snapshotFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId(repoName);
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  try {
    insertWorkspace.run({
      id,
      accountId,
      workspaceKey,
      repoName: repoName ?? null,
      status: "queued",
      machineId: null,
      machineState: null,
      snapshotKey: snapshot.key,
      snapshotFingerprint,
      region,
      volumeId: null,
      workerTokenHash: tokenHash(workerToken),
      lastActivityAt: now,
      lastHeartbeatAt: null,
      createdAt: now,
      updatedAt: now,
    });
  } catch (error) {
    await deleteSnapshotBestEffort(snapshot.key);
    if (isSqliteConstraint(error)) {
      throw conflict("workspace was created concurrently; retry attach");
    }
    throw error;
  }
  markDatabaseDirty();
  const regionToUse = region ?? flyRegion;
  let volumeId: string | undefined;
  let machine: FlyMachine | undefined;
  try {
    const volume = await createWorkspaceVolume(id, regionToUse);
    if (!volume.id) {
      throw new Error("Fly create did not return a volume id");
    }
    volumeId = volume.id;
    updateWorkspaceVolume.run({
      id,
      volumeId,
      region: regionToUse,
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    const snapshotUrl = await signedSnapshotUrl(snapshot.key);
    machine = await createFlyWorkspaceMachine({
      workspaceId: id,
      accountId,
      snapshotUrl,
      workerToken,
      repoName,
      region: regionToUse,
      volumeId,
    });
    if (!machine.id) {
      throw new Error("Fly create did not return a machine id");
    }
  } catch (error) {
    if (machine?.id) {
      await destroyFlyMachine(machine.id, true).catch(() => undefined);
    }
    if (volumeId) {
      await destroyWorkspaceVolume(volumeId).catch(() => undefined);
    }
    updateWorkspaceVolume.run({ id, volumeId: null, region: regionToUse, updatedAt: new Date().toISOString() });
    updateWorkspaceMachine.run({
      id,
      status: "failed",
      machineId: null,
      machineState: "provision-failed",
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    throw error;
  }
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({
    id,
    status: started.status,
    machineId: machine.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  markDatabaseDirty();
  const workspace = (findWorkspaceById.get(id) as Record<string, unknown> | undefined);
  const result = workspace ? rowToWorkspace(workspace) : {
    id,
    accountId,
    workspaceKey,
    repoName,
    status: started.status,
    machineId: machine.id,
    machineState: started.machineState,
    snapshotKey: snapshot.key,
    snapshotFingerprint: snapshotFingerprint ?? undefined,
    region: region ?? undefined,
    createdAt: now,
    updatedAt: now,
  } as Workspace;
  return { ...result, isNew: true } as unknown as JsonRecord;
}

async function reuseOrRestartWorkspace(
  workspace: Workspace,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  ensureFlyConfigured();
  let machine: FlyMachine | null = null;
  if (workspace.machineId) {
    machine = await getFlyMachineIfPresent(workspace.machineId);
  }
  if (instanceHasLiveWorker("workspace", workspace.id)) {
    const now = new Date().toISOString();
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: "running",
      machineId: workspace.machineId ?? machine?.id ?? null,
      machineState: "running",
      updatedAt: now,
    });
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
    markDatabaseDirty();
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : { ...workspace, status: "running" }), isNew: false } as unknown as JsonRecord;
  }
  // Path 1: machine already running -> just attach.
  if (machine && (machine.state === "started" || machine.state === "starting" || machine.state === "running")) {
    const now = new Date().toISOString();
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: flyStateToWorkspaceStatus(machine.state),
      machineId: machine.id ?? workspace.machineId ?? null,
      machineState: machine.state ?? null,
      updatedAt: now,
    });
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
    markDatabaseDirty();
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : workspace), isNew: false } as unknown as JsonRecord;
  }

  const incomingFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const snapshotInput = objectField(body, "snapshot");
  const fingerprintMatches = Boolean(
    incomingFingerprint
      && workspace.snapshotFingerprint
      && incomingFingerprint === workspace.snapshotFingerprint,
  );

  // Path 2: warm restart — machine exists but is stopped, and the user's
  // local state hasn't changed (fingerprint matches). With a persistent
  // Fly Volume mounted at /workspace, the supervisor's alreadyStaged()
  // marker survives stop+start so the supervisor skips re-staging entirely
  // (~1-2s total). Without a volume, the marker is on the ephemeral rootfs
  // and the supervisor re-downloads using a fresh URL via the snapshot-url
  // endpoint (~10-30s). Either way, no destroy+recreate is needed.
  if (
    machine
    && machine.id
    && (machine.state === "stopped" || machine.state === "suspended")
    && fingerprintMatches
    && workspace.snapshotKey
    && workspace.status !== "failed"
  ) {
    const restarted = await warmRestartWorkspaceMachine({
      machineId: machine.id,
      snapshotKey: workspace.snapshotKey,
    }).catch((error) => {
      console.warn(`warm restart failed for ${workspace.id}: ${error instanceof Error ? error.message : String(error)}`);
      return null;
    });
    if (restarted) {
      const now = new Date().toISOString();
      const started = workspaceStartResult(restarted.state);
      updateWorkspaceMachine.run({
        id: workspace.id,
        status: started.status,
        machineId: restarted.id ?? machine.id,
        machineState: started.machineState,
        updatedAt: now,
      });
      updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
      markDatabaseDirty();
      const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
      return { ...(next ? rowToWorkspace(next) : workspace), isNew: false } as unknown as JsonRecord;
    }
    // Warm restart failed (env update error, etc.) — fall through to
    // destroy+recreate so the user always gets a working machine.
  }

  // Path 3: must (re)create a machine. Either no machine, machine is gone,
  // or fingerprint mismatch (user's local state changed). Need a fresh
  // snapshot from the CLI. Also destroy + recreate the volume so the user
  // sees their new repo state instead of the cached one on the old disk.
  if (!snapshotInput) {
    throw badRequest("snapshot is required to (re)create this workspace");
  }
  const requestedRegion = stringField(body, "region");
  const sanitizedRegion = sanitizeRegion(requestedRegion);
  if (requestedRegion && !sanitizedRegion) {
    throw badRequest("region must be a three-letter Fly region code");
  }
  const region = sanitizedRegion ?? workspace.region ?? null;
  const regionToUse = region ?? flyRegion;
  // Upload and validate the replacement before touching the working machine.
  // A failed S3 request must leave the existing workspace intact.
  const snapshotKey = (await storeSnapshot(workspace.accountId, body)).key;
  // A Fly volume cannot be deleted while still attached. Destroy the machine
  // first, then its volume. Never swallow either failure and provision a second
  // set of resources alongside the first one.
  try {
    if (workspace.machineId && machine && machine.state !== "destroyed" && machine.state !== "destroying") {
      await destroyFlyMachine(workspace.machineId, true);
    }
    if (workspace.volumeId) {
      await destroyWorkspaceVolume(workspace.volumeId);
    }
  } catch (error) {
    await deleteSnapshotBestEffort(snapshotKey);
    throw error;
  }
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  // Teardown succeeded, so make the database immediately stop advertising the
  // old resources. Persist the replacement snapshot now; every later failure
  // leaves a coherent, retryable failed row rather than stale machine/volume ids.
  updateWorkspaceMachine.run({
    id: workspace.id,
    status: "queued",
    machineId: null,
    machineState: "destroyed",
    updatedAt: now,
  });
  updateWorkspaceVolume.run({ id: workspace.id, volumeId: null, region: regionToUse, updatedAt: now });
  updateWorkspaceSnapshot.run({ id: workspace.id, snapshotKey, snapshotFingerprint: incomingFingerprint, updatedAt: now });
  markDatabaseDirty();
  let newVolumeId: string | undefined;
  let fresh: FlyMachine | undefined;
  try {
    try {
      const freshVolume = await createWorkspaceVolume(workspace.id, regionToUse);
      if (!freshVolume.id) {
        throw new Error("Fly create did not return a volume id");
      }
      newVolumeId = freshVolume.id;
    } catch (error) {
      if (!isHttpStatus(error, 409) && !isHttpStatus(error, 422)) {
        throw error;
      }
      // Volume name may still be reserved if the prior delete is settling.
      const message = error instanceof Error ? error.message : String(error);
      console.warn(`recreate volume for ${workspace.id} failed (${message}); retrying with suffix`);
      const suffixed = await createWorkspaceVolumeNamed(
        workspaceVolumeName(workspace.id, String(Date.now()).slice(-6)),
        regionToUse,
      );
      if (!suffixed.id) {
        throw new Error("Fly create did not return a volume id");
      }
      newVolumeId = suffixed.id;
    }
    updateWorkspaceVolume.run({
      id: workspace.id,
      volumeId: newVolumeId,
      region: regionToUse,
      updatedAt: new Date().toISOString(),
    });
    updateWorkspaceWorkerToken.run({ id: workspace.id, workerTokenHash: tokenHash(workerToken), updatedAt: now });
    markDatabaseDirty();
    disconnectWorker("workspace", workspace.id, "credentials rotated");
    const snapshotUrl = await signedSnapshotUrl(snapshotKey);
    fresh = await createFlyWorkspaceMachine({
      workspaceId: workspace.id,
      accountId: workspace.accountId,
      snapshotUrl,
      workerToken,
      repoName: repoName ?? workspace.repoName,
      region: regionToUse,
      volumeId: newVolumeId,
    });
    if (!fresh.id) {
      throw new Error("Fly create did not return a machine id");
    }
  } catch (error) {
    if (fresh?.id) {
      await destroyFlyMachine(fresh.id, true).catch(() => undefined);
    }
    if (newVolumeId) {
      await destroyWorkspaceVolume(newVolumeId).catch(() => undefined);
    }
    updateWorkspaceVolume.run({ id: workspace.id, volumeId: null, region: regionToUse, updatedAt: new Date().toISOString() });
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: "failed",
      machineId: null,
      machineState: "provision-failed",
      updatedAt: new Date().toISOString(),
    });
    markDatabaseDirty();
    if (workspace.snapshotKey && workspace.snapshotKey !== snapshotKey) {
      void deleteSnapshotBestEffort(workspace.snapshotKey);
    }
    throw error;
  }
  if (!fresh) {
    throw new Error("Fly workspace provisioning returned no machine");
  }
  const started = workspaceStartResult(fresh.state);
  updateWorkspaceMachine.run({
    id: workspace.id,
    status: started.status,
    machineId: fresh.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  markDatabaseDirty();
  if (workspace.snapshotKey && workspace.snapshotKey !== snapshotKey) {
    void deleteSnapshotBestEffort(workspace.snapshotKey);
  }
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : workspace), isNew: true } as unknown as JsonRecord;
}

function sanitizeRegion(value: string | undefined): string | null {
  if (!value) return null;
  const lower = value.trim().toLowerCase();
  return /^[a-z]{3}$/.test(lower) ? lower : null;
}

async function warmRestartWorkspaceMachine(params: {
  machineId: string;
  snapshotKey: string;
}): Promise<FlyMachine | null> {
  ensureFlyConfigured();
  // Don't update env. Fly's machine update POST replaces the machine and
  // breaks the start. Just start it. Fly machine disks are ephemeral across
  // stop+start, so the supervisor's alreadyStaged() marker is missing on cold
  // boot and it will re-download the snapshot. The supervisor refreshes its
  // own pre-signed URL via GET /api/rudder/workspace/:id/snapshot-url, which
  // means the stale env URL is no longer a problem on warm restart.
  void params.snapshotKey;
  try {
    return await startFlyMachine(params.machineId, `workspace ${params.machineId}`);
  } catch (error) {
    console.warn(`warm restart ${params.machineId}: start failed: ${error instanceof Error ? error.message : String(error)}`);
    throw error;
  }
}

async function startFlyMachine(machineId: string, label: string): Promise<FlyMachine> {
  let lastError: unknown;
  const maxAttempts = 8;
  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    try {
      const started = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}/start`,
        { method: "POST", body: {} },
      );
      return started ?? { id: machineId, state: "starting" };
    } catch (error) {
      lastError = error;
      const machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
        { method: "GET" },
      ).catch(() => null);
      if (machine?.state === "started" || machine?.state === "starting") {
        return machine;
      }
      if (attempt < maxAttempts) {
        await sleep(Math.min(2_000, 250 * (2 ** (attempt - 1))));
      }
    }
  }
  const message = lastError instanceof Error ? lastError.message : String(lastError);
  throw new Error(`Fly start failed for ${label}: ${message}`);
}

function workspaceStartResult(state: string | undefined): { status: WorkspaceStatus; machineState: string } {
  if (state === "failed") {
    return { status: "failed", machineState: "failed" };
  }
  if (state === "started" || state === "starting" || state === "running") {
    return { status: "running", machineState: state };
  }
  // Fly's start endpoint can return the pre-start machine document (`created`)
  // even though the start request was accepted and the worker connects moments
  // later. Treat a successful start request as running/starting so a later
  // stale Fly response never overwrites a worker heartbeat back to queued.
  return { status: "running", machineState: "starting" };
}

async function createFlyWorkspaceMachine(params: {
  workspaceId: string;
  accountId: string;
  snapshotUrl: string;
  workerToken: string;
  repoName?: string;
  region?: string;
  volumeId?: string;
}): Promise<FlyMachine> {
  ensureFlyConfigured();
  const config: JsonRecord = {
    image: flyWorkerImage,
    env: {
      RUDDER_WORKSPACE_ID: params.workspaceId,
      RUDDER_ACCOUNT_ID: params.accountId,
      RUDDER_CLOUD_URL: baseURL,
      RUDDER_WORKER_TOKEN: params.workerToken,
      RUDDER_SNAPSHOT_URL: params.snapshotUrl,
      RUDDER_REPO_NAME: params.repoName || "",
    },
    guest: {
      cpu_kind: flyWorkerCpuKind,
      cpus: flyWorkerCpus,
      memory_mb: flyWorkerMemoryMb,
    },
    restart: { policy: "no" },
    auto_destroy: false,
  };
  if (params.volumeId) {
    config.mounts = [{ volume: params.volumeId, path: "/workspace" }];
  }
  const machine = await flyRequest<FlyMachine>(`/v1/apps/${encodeURIComponent(flyAppName)}/machines`, {
    method: "POST",
    body: {
      name: flyMachineName("rudder-ws", params.workspaceId),
      region: params.region || flyRegion,
      config,
      skip_launch: false,
    },
  });
  return machine;
}

type FlyVolume = {
  id?: string;
  name?: string;
  region?: string;
  state?: string;
};

function workspaceVolumeName(workspaceId: string, suffix?: string): string {
  // Fly volume names allow only alphanumeric + underscore, max 30 chars per
  // the API. Replace any other char (e.g. "-") with "_" and truncate.
  const base = `rdr_${workspaceId.replace(/[^a-zA-Z0-9]/g, "_")}`;
  if (!suffix) {
    return base.slice(0, 30);
  }
  const tail = `_${suffix}`;
  const maxBase = 30 - tail.length;
  return `${base.slice(0, maxBase)}${tail}`;
}

function flyMachineName(prefix: string, id: string): string {
  const suffix = randomBytes(3).toString("hex");
  const base = `${prefix}-${id}`
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .replace(/-+/g, "-");
  const maxBaseLength = 63 - suffix.length - 1;
  const safeBase = (base || prefix).slice(0, maxBaseLength).replace(/-+$/g, "");
  return `${safeBase || "rudder"}-${suffix}`;
}

async function createWorkspaceVolume(workspaceId: string, region: string): Promise<FlyVolume> {
  return await createWorkspaceVolumeNamed(workspaceVolumeName(workspaceId), region);
}

async function createWorkspaceVolumeNamed(name: string, region: string): Promise<FlyVolume> {
  ensureFlyConfigured();
  return await flyRequest<FlyVolume>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/volumes`,
    {
      method: "POST",
      body: {
        name,
        region,
        size_gb: flyWorkspaceVolumeGb,
      },
    },
  );
}

async function destroyWorkspaceVolume(volumeId: string): Promise<void> {
  ensureFlyConfigured();
  try {
    await flyRequest<FlyVolume>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/volumes/${encodeURIComponent(volumeId)}`,
      { method: "DELETE" },
    );
  } catch (error) {
    if (!isHttpStatus(error, 404)) {
      throw error;
    }
  }
}

async function mutateWorkspace(workspace: Workspace, action: string): Promise<Workspace> {
  if (!workspace.machineId) {
    throw badRequest("workspace does not have a Fly machine yet");
  }
  let machine: FlyMachine;
  if (action === "stop") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGTERM", timeout: "10s" } },
    ) ?? {};
  } else if (action === "pause") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/suspend`,
      { method: "POST", body: {} },
    ) ?? {};
  } else if (action === "resume") {
    machine = await startFlyMachine(workspace.machineId, `workspace ${workspace.id}`);
  } else {
    throw badRequest(`unsupported workspace action: ${action}`);
  }
  const started = workspaceStartResult(machine.state);
  const status: WorkspaceStatus = action === "stop"
    ? "stopped"
    : action === "pause"
      ? "paused"
      : started.status;
  const now = new Date().toISOString();
  updateWorkspaceMachine.run({
    id: workspace.id,
    status,
    machineId: workspace.machineId,
    machineState: action === "stop" || action === "pause" ? machine.state ?? null : started.machineState,
    updatedAt: now,
  });
  markDatabaseDirty();
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return next ? rowToWorkspace(next) : { ...workspace, status, machineState: machine.state };
}

async function handleWorkspaceHeartbeat(req: IncomingMessage, res: ServerResponse, workspaceId: string): Promise<void> {
  const authRow = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!authRow) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, authRow);
  const body = await readJsonBody(req);
  const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const state = stringField(body, "state");
  const reportedStatus: WorkspaceStatus = state === "failed"
    ? "failed"
    : state === "completed" || state === "stopped"
      ? "stopped"
      : "running";
  const previousStatus = String(row.status) as WorkspaceStatus;
  const status = previousStatus === "failed"
    ? previousStatus
    : reportedStatus === "running" && (previousStatus === "stopped" || previousStatus === "paused")
      ? previousStatus
      : reportedStatus;
  const now = new Date().toISOString();
  updateWorkspaceHeartbeat.run({
    id: workspaceId,
    status,
    machineId: stringField(body, "machineId") ?? null,
    machineState: status === reportedStatus ? state || status : optionalString(row.machine_state) ?? status,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
  markDatabaseDirty();
  sendJson(res, 200, { ok: true, status });
}

async function handleWorkspaceSnapshotUrl(req: IncomingMessage, res: ServerResponse, workspaceId: string): Promise<void> {
  const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const snapshotKey = optionalString(row.snapshot_key);
  if (!snapshotKey) {
    sendJson(res, 400, { error: "workspace has no snapshot" });
    return;
  }
  const signed = await signedSnapshotUrl(snapshotKey);
  sendJson(res, 200, { url: signed, expiresInSeconds: 3600 });
}

async function handleSailSnapshotUrl(req: IncomingMessage, res: ServerResponse, sailId: string): Promise<void> {
  const row = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "sail not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const snapshotKey = optionalString(row.snapshot_key);
  if (!snapshotKey) {
    sendJson(res, 400, { error: "sail has no snapshot" });
    return;
  }
  const signed = await signedSnapshotUrl(snapshotKey);
  sendJson(res, 200, { url: signed, expiresInSeconds: 3600 });
}

const workspaceRefreshes = new Map<string, Promise<void>>();
const lastWorkspaceRefreshAt = new Map<string, number>();

async function refreshAccountWorkspaces(accountId: string): Promise<void> {
  if (!flyApiToken || !flyAppName || Date.now() - (lastWorkspaceRefreshAt.get(accountId) ?? 0) < 5_000) {
    return;
  }
  const existing = workspaceRefreshes.get(accountId);
  if (existing) {
    await existing;
    return;
  }
  const refresh = mapWithConcurrency(listAccountWorkspaces(accountId), 8, refreshWorkspaceRow)
    .then(() => lastWorkspaceRefreshAt.set(accountId, Date.now()))
    .then(() => undefined)
    .finally(() => {
      if (workspaceRefreshes.get(accountId) === refresh) {
        workspaceRefreshes.delete(accountId);
      }
    });
  workspaceRefreshes.set(accountId, refresh);
  await refresh;
}

async function refreshWorkspaceRow(workspace: Workspace): Promise<void> {
  await withOperationLock(`workspace:${workspace.accountId}:${workspace.workspaceKey}`, async () => {
    await refreshWorkspaceRowCore(workspace);
  });
}

async function refreshWorkspaceRowCore(workspace: Workspace): Promise<void> {
  if (!flyApiToken || !flyAppName || !workspace.machineId || instanceHasLiveWorker("workspace", workspace.id)) {
    return;
  }
  const machine = await getFlyMachineIfPresent(workspace.machineId).catch(() => undefined);
  if (machine === undefined) {
    return;
  }
  const now = new Date().toISOString();
  if (machine === null || machine.state === "destroyed" || machine.state === "destroying") {
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: "stopped",
      machineId: null,
      machineState: "destroyed",
      updatedAt: now,
    });
    markDatabaseDirty();
    return;
  }
  const status = flyStateToWorkspaceStatus(machine.state);
  // Unknown transient Fly states should not regress a healthy heartbeat to queued.
  if (status === "queued" && workspace.status === "running") {
    return;
  }
  updateWorkspaceMachine.run({
    id: workspace.id,
    status,
    machineId: machine.id ?? workspace.machineId,
    machineState: machine.state ?? workspace.machineState ?? null,
    updatedAt: now,
  });
  markDatabaseDirty();
}

function listAccountWorkspaces(accountId: string): Workspace[] {
  return (listWorkspacesForAccount.all(accountId) as unknown[]).map(rowToWorkspace);
}

function annotateWorkspaceClients(workspace: Workspace): Workspace & { clientCount: number } {
  const channel = workspaceChannels.get(workspace.id);
  return { ...workspace, clientCount: channel ? channel.clients.size : 0 };
}

function getAccountWorkspace(id: string, accountId: string): Workspace | null {
  const row = findWorkspaceForAccount.get(id, accountId);
  return row ? rowToWorkspace(row) : null;
}

function rowToWorkspace(row: unknown): Workspace {
  const value = row as Record<string, unknown>;
  return {
    id: String(value.id),
    accountId: String(value.account_id),
    workspaceKey: String(value.workspace_key),
    repoName: optionalString(value.repo_name),
    status: String(value.status) as WorkspaceStatus,
    machineId: optionalString(value.machine_id),
    machineState: optionalString(value.machine_state),
    snapshotKey: optionalString(value.snapshot_key),
    snapshotFingerprint: optionalString(value.snapshot_fingerprint),
    region: optionalString(value.region),
    volumeId: optionalString(value.volume_id),
    lastActivityAt: optionalString(value.last_activity_at),
    lastHeartbeatAt: optionalString(value.last_heartbeat_at),
    createdAt: String(value.created_at),
    updatedAt: String(value.updated_at),
  };
}

function flyStateToWorkspaceStatus(state: string | undefined): WorkspaceStatus {
  switch (state) {
    case "started":
    case "starting":
      return "running";
    case "stopped":
    case "stopping":
      return "stopped";
    case "suspended":
    case "suspending":
      return "paused";
    case "destroyed":
      return "stopped";
    case "failed":
      return "failed";
    default:
      return "queued";
  }
}

function uniqueWorkspaceId(repoName: string | undefined): string {
  const base = slugForWorkspaceId(repoName) || `workspace`;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const candidate = attempt === 0
      ? `${base}-${randomBytes(2).toString("hex")}`
      : `${base}-${randomBytes(3).toString("hex")}`;
    if (!findWorkspaceById.get(candidate)) {
      return candidate;
    }
  }
  return `workspace-${randomBytes(5).toString("hex")}`;
}

function slugForWorkspaceId(value: string | undefined): string {
  const slug = (value || "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24)
    .replace(/-+$/g, "");
  return slug || "";
}

class ExternalHttpError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
    this.name = "ExternalHttpError";
  }
}

function isHttpStatus(error: unknown, status: number): boolean {
  return error instanceof ExternalHttpError && error.status === status;
}

async function getFlyMachineIfPresent(machineId: string): Promise<FlyMachine | null> {
  try {
    return await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
      { method: "GET" },
    );
  } catch (error) {
    if (isHttpStatus(error, 404)) {
      return null;
    }
    throw error;
  }
}

async function flyRequest<T>(pathname: string, init: { method: string; body?: JsonRecord }): Promise<T> {
  ensureFlyConfigured();
  const headers: Record<string, string> = {
    Authorization: `Bearer ${flyApiToken}`,
    Accept: "application/json",
  };
  let body: string | undefined;
  if (init.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(init.body);
  }
  const response = await fetch(`${flyApiBase}${pathname}`, {
    method: init.method,
    headers,
    body,
    signal: AbortSignal.timeout(EXTERNAL_REQUEST_TIMEOUT_MS),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new ExternalHttpError(
      response.status,
      responseErrorMessage(parsed) || text.trim() || `Fly API ${response.status}`,
    );
  }
  return parsed as T;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function mapWithConcurrency<T>(
  values: readonly T[],
  concurrency: number,
  mapper: (value: T) => Promise<void>,
): Promise<void> {
  let next = 0;
  const workers = Array.from({ length: Math.min(concurrency, values.length) }, async () => {
    for (;;) {
      const index = next;
      next += 1;
      if (index >= values.length) {
        return;
      }
      await mapper(values[index]);
    }
  });
  await Promise.all(workers);
}

async function githubUser(token: string): Promise<{ id: number | string; login: string; email?: string }> {
  const verifiedEmail = githubVerifiedPrimaryEmail(token);
  const response = await fetch("https://api.github.com/user", {
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "rudder-cloud",
      "X-GitHub-Api-Version": "2022-11-28",
    },
    signal: AbortSignal.timeout(EXTERNAL_REQUEST_TIMEOUT_MS),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw unauthorized();
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw unauthorized();
  }
  const id = typeof parsed.id === "number" || typeof parsed.id === "string" ? parsed.id : undefined;
  const login = typeof parsed.login === "string" ? parsed.login : undefined;
  if (!id || !login) {
    throw unauthorized();
  }
  // SECURITY: the public profile `email` is user-mutable and may be unverified, so it
  // must never drive authorization (it gates admin via adminEmails). Use the
  // GitHub-verified primary email instead; undefined when none is verified.
  const email = await verifiedEmail;
  return { id, login, email };
}

async function githubVerifiedPrimaryEmail(token: string): Promise<string | undefined> {
  try {
    const response = await fetch("https://api.github.com/user/emails", {
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        "User-Agent": "rudder-cloud",
        "X-GitHub-Api-Version": "2022-11-28",
      },
      signal: AbortSignal.timeout(EXTERNAL_REQUEST_TIMEOUT_MS),
    });
    if (!response.ok) {
      return undefined;
    }
    const list = (await response.json()) as unknown;
    if (!Array.isArray(list)) {
      return undefined;
    }
    const primary = list.find(
      (entry): entry is { email: string } =>
        Boolean(entry) &&
        typeof entry === "object" &&
        (entry as { primary?: unknown }).primary === true &&
        (entry as { verified?: unknown }).verified === true &&
        typeof (entry as { email?: unknown }).email === "string",
    );
    return primary?.email.toLowerCase();
  } catch {
    return undefined;
  }
}

// --------------------------------------------------------------------------
// Slack: the shared channel is the "main panel" for every cloud instance.
// --------------------------------------------------------------------------

async function postToSlack(text: string, threadTs?: string): Promise<string | undefined> {
  if (!slack.enabled) {
    return undefined;
  }
  const result = await postSlackMessage({
    botToken: slack.botToken,
    channel: slack.channel,
    text,
    threadTs,
  });
  if (!result.ok) {
    console.warn(`slack post failed: ${result.error}`);
    return undefined;
  }
  return result.ts;
}

// Open a thread for a freshly launched instance and remember its root ts so
// later replies route back to this instance.
async function announceSailToSlack(
  id: string,
  accountId: string,
  task: string | undefined,
  repoName: string | undefined,
  runtime: SailRuntime,
): Promise<void> {
  if (!slack.enabled || !slackAccountIds.has(accountId)) {
    return;
  }
  const safeRepo = repoName ? escapeSlackText(repoName).replace(/`/g, "\u02cb") : "";
  const safeTask = task ? escapeSlackText(task).replace(/\r?\n/g, "\n> ") : "";
  const lines = [
    `🚀 *${id}* launched on ${runtime === "fly" ? "Fly" : "your VM"}.`,
    safeRepo ? `repo: \`${safeRepo}\`` : "",
    safeTask ? `> ${safeTask}` : "",
    "_Reply in this thread to talk to the agent._",
  ].filter(Boolean);
  const ts = await postToSlack(lines.join("\n"));
  if (ts) {
    updateSailSlackThread.run({ id, threadTs: ts, updatedAt: new Date().toISOString() });
    markDatabaseDirty();
  }
}

// After injecting input, the agent needs a moment to react. Read the output
// tail a few seconds later and post it back into the instance's thread.
function scheduleOutputEcho(sailId: string, threadTs: string | undefined, delayMs = 4500): void {
  setTimeout(() => {
    const tail = readChannelOutput("sail", sailId);
    if (tail.trim()) {
      void postToSlack(formatOutputForSlack(tail), threadTs);
    }
  }, delayMs).unref();
}

// Default body cap for unauthenticated/early-parsed routes. Snapshots (base64
// tarballs) are larger, so authenticated upload routes pass a bigger explicit cap.
const DEFAULT_MAX_BODY_BYTES = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BODY_BYTES = 256 * 1024 * 1024;
const MAX_INFLIGHT_SNAPSHOT_BODY_BYTES = 384 * 1024 * 1024;
let inflightSnapshotBodyBytes = 0;

async function withSnapshotRequestBudget<T>(req: IncomingMessage, operation: () => Promise<T>): Promise<T> {
  const declared = Number(req.headers["content-length"]);
  const reservation = Number.isFinite(declared) && declared >= 0
    ? Math.min(declared, MAX_SNAPSHOT_BODY_BYTES)
    : MAX_SNAPSHOT_BODY_BYTES;
  if (inflightSnapshotBodyBytes + reservation > MAX_INFLIGHT_SNAPSHOT_BODY_BYTES) {
    req.once("error", () => undefined);
    req.resume();
    throw tooManyRequests("too many snapshot uploads in progress; retry shortly");
  }
  inflightSnapshotBodyBytes += reservation;
  try {
    return await operation();
  } finally {
    inflightSnapshotBodyBytes -= reservation;
  }
}

async function readRawBody(req: IncomingMessage, maxBytes = DEFAULT_MAX_BODY_BYTES): Promise<string> {
  return (await readBodyBuffer(req, maxBytes)).toString("utf8");
}

async function handleSlackEvents(req: IncomingMessage, res: ServerResponse): Promise<void> {
  // Slack events are tiny; cap hard so an unauthenticated POST can't exhaust memory.
  const raw = await readRawBody(req, 256 * 1024);
  // Fail closed: never process an inbound Slack event without verifying its
  // signature. An unset signing secret means Slack is misconfigured, not open.
  if (!slack.signingSecret) {
    sendJson(res, 401, { error: "slack not configured" });
    return;
  }
  const ok = verifySlackSignature({
    signingSecret: slack.signingSecret,
    timestamp: req.headers["x-slack-request-timestamp"] as string | undefined,
    signature: req.headers["x-slack-signature"] as string | undefined,
    rawBody: raw,
  });
  if (!ok) {
    sendJson(res, 401, { error: "bad signature" });
    return;
  }
  let body: Record<string, unknown>;
  try {
    body = JSON.parse(raw) as Record<string, unknown>;
  } catch {
    sendJson(res, 400, { error: "invalid json" });
    return;
  }

  if (body.type === "url_verification") {
    sendJson(res, 200, { challenge: typeof body.challenge === "string" ? body.challenge : "" });
    return;
  }

  // Slack expects a 200 within 3s; do the real work afterwards.
  sendJson(res, 200, { ok: true });

  if (body.type !== "event_callback" || !slack.enabled) {
    return;
  }
  const eventId = typeof body.event_id === "string" ? body.event_id : "";
  if (eventId) {
    if (seenSlackEvents.has(eventId)) {
      return;
    }
    seenSlackEvents.add(eventId);
    if (seenSlackEvents.size > 2000) {
      const oldest = seenSlackEvents.values().next().value;
      if (oldest) {
        seenSlackEvents.delete(oldest);
      }
    }
  }
  const event = body.event as Record<string, unknown> | undefined;
  if (!event) {
    return;
  }
  processSlackEvent(event).catch((error) => {
    console.error("slack event error", error);
  });
}

async function processSlackEvent(event: Record<string, unknown>): Promise<void> {
  const type = String(event.type || "");
  // Ignore anything the bot itself posted, and edits/joins/etc.
  if (event.bot_id || event.subtype) {
    return;
  }
  if (type !== "app_mention" && type !== "message") {
    return;
  }
  if (typeof event.channel !== "string" || event.channel !== slack.channel) {
    return;
  }
  const text = typeof event.text === "string" ? event.text : "";
  const ts = typeof event.ts === "string" ? event.ts : undefined;
  const threadTs = typeof event.thread_ts === "string" ? event.thread_ts : undefined;
  const inThread = Boolean(threadTs);

  // Plain channel chatter is not for us — only act on @mentions and on replies
  // inside a thread we own.
  const threadSail = threadTs
    ? (findSailBySlackThread.get(threadTs) as Record<string, unknown> | undefined)
    : undefined;
  const allowedThreadSail = threadSail && slackAccountIds.has(String(threadSail.account_id))
    ? threadSail
    : undefined;
  if (type === "message" && !allowedThreadSail) {
    return;
  }

  const command = parseSlackCommand(text, { inThread });
  const replyThread = threadTs ?? ts;

  // The channel is shared across accounts, so authorize the sender before any command
  // that reads output or controls an instance. Without an allowlist, fail closed.
  if (command.action !== "help") {
    const sender = typeof event.user === "string" ? event.user : "";
    if (!sender || !slackAllowedUsers.has(sender)) {
      await postToSlack(
        "You're not authorized to control cloud instances from Slack. Ask an admin to add your Slack user ID to RUDDER_SLACK_ALLOWED_USERS.",
        replyThread,
      );
      return;
    }
    if (slackAccountIds.size === 0) {
      await postToSlack(
        "Slack control is not scoped to a Rudder account. Set RUDDER_SLACK_ACCOUNT_IDS before enabling commands.",
        replyThread,
      );
      return;
    }
  }

  switch (command.action) {
    case "help":
      await postToSlack(SLACK_HELP_TEXT, replyThread);
      return;
    case "list": {
      const sails = (listRunningSails.all() as Record<string, unknown>[])
        .filter((row) => slackAccountIds.has(String(row.account_id)))
        .map(rowToSail);
      if (sails.length === 0) {
        await postToSlack("No running cloud instances. Start one with `rudder cloud \"<task>\"`.", replyThread);
        return;
      }
      const lines = sails.map((s) => {
        const live = instanceHasLiveWorker("sail", s.id) ? "🟢" : "⚪️";
        return `${live} *${s.id}* — ${s.status}${s.task ? ` · ${escapeSlackText(s.task)}` : ""}`;
      });
      await postToSlack(`*Cloud instances*\n${lines.join("\n")}`, replyThread);
      return;
    }
    case "output": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      if (!row || !slackAccountIds.has(String(row.account_id))) {
        await postToSlack(`No instance \`${escapeSlackText(command.id)}\`.`, replyThread);
        return;
      }
      const tail = readChannelOutput("sail", command.id);
      await postToSlack(
        tail
          ? `${instanceHasLiveWorker("sail", command.id) ? "" : `*${command.id}* is not connected; last output:\n`}${formatOutputForSlack(tail)}`
          : `*${command.id}* is not connected and has no retained output.`,
        replyThread,
      );
      return;
    }
    case "stop": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      if (!row || !slackAccountIds.has(String(row.account_id))) {
        await postToSlack(`No instance \`${command.id}\`.`, replyThread);
        return;
      }
      const accountId = String(row.account_id);
      const sail = getAccountSail(command.id, accountId);
      if (sail) {
        try {
          await withOperationLock(`sail:${sail.id}`, async () => mutateSail(sail, accountId, "stop"));
        } catch (error) {
          await postToSlack(
            `Could not stop *${command.id}*: ${escapeSlackText(error instanceof Error ? error.message : String(error))}`,
            replyThread,
          );
          return;
        }
      }
      await postToSlack(`🛑 Stopping *${command.id}*.`, replyThread);
      return;
    }
    case "talk":
    case "thread-reply": {
      const targetId = command.action === "talk" ? command.id : String(allowedThreadSail?.id || "");
      const message = command.message;
      if (!targetId) {
        await postToSlack("Which instance? Use `talk <id> <message>` or reply in an instance thread.", replyThread);
        return;
      }
      if (!message) {
        return;
      }
      // Route the echo into the instance's own thread when we know it.
      const sailRow = findSailById.get(targetId) as Record<string, unknown> | undefined;
      if (!sailRow || !slackAccountIds.has(String(sailRow.account_id))) {
        await postToSlack(`No instance \`${escapeSlackText(targetId)}\`.`, replyThread);
        return;
      }
      const ownThread = optionalString(sailRow?.slack_thread_ts) ?? replyThread;
      const delivered = sendInputToChannel("sail", targetId, message, true);
      if (!delivered) {
        await postToSlack(`*${targetId}* is not connected right now.`, replyThread);
        return;
      }
      scheduleOutputEcho(targetId, ownThread);
      return;
    }
  }
}

function listAccountSails(accountId: string): Sail[] {
  return listSailsForAccount.all(accountId).map(rowToSail);
}

function getAccountSail(id: string, accountId: string): Sail | null {
  const row = findSail.get(id, accountId);
  return row ? rowToSail(row) : null;
}

function rowToSail(row: unknown): Sail {
  const value = row as Record<string, unknown>;
  return {
    id: String(value.id),
    status: String(value.status) as SailStatus,
    runtime: sailRuntimeValue(optionalString(value.runtime)),
    repoName: optionalString(value.repo_name),
    task: optionalString(value.task),
    branch: optionalString(value.branch),
    machineId: optionalString(value.machine_id),
    machineState: optionalString(value.machine_state),
    snapshotKey: optionalString(value.snapshot_key),
    lastActivityAt: optionalString(value.last_activity_at),
    lastHeartbeatAt: optionalString(value.last_heartbeat_at),
    slackThreadTs: optionalString(value.slack_thread_ts),
    createdAt: String(value.created_at),
    updatedAt: String(value.updated_at),
  };
}

function sailRuntimeFromBody(body: Json): SailRuntime {
  const worker = objectField(body, "worker");
  const raw = stringField(body, "runtime") || stringField(worker, "type") || stringField(worker, "runtime");
  return sailRuntimeValue(raw);
}

function sailRuntimeValue(raw: string | undefined): SailRuntime {
  const value = (raw || "fly").trim().toLowerCase();
  if (value === "fly" || value === "fly-machine" || value === "fly-machines") {
    return "fly";
  }
  if (value === "byo" || value === "byoc" || value === "byo-vm" || value === "manual" || value === "self-hosted" || value === "vm") {
    return "byo-vm";
  }
  throw badRequest(`unsupported cloud runtime: ${raw}`);
}

function flyStateToSailStatus(state: string | undefined): SailStatus {
  switch (state) {
    case "started":
    case "starting":
      return "running";
    case "suspended":
    case "stopped":
    case "stopping":
      return "paused";
    case "destroyed":
      return "completed";
    case "failed":
      return "failed";
    default:
      return "queued";
  }
}

function requireBearer(req: IncomingMessage): { accountId: string; email?: string } {
  const authHeader = req.headers.authorization || "";
  const token = authHeader.startsWith("Bearer ") ? authHeader.slice("Bearer ".length).trim() : "";
  if (!token.startsWith("rdr_")) {
    throw unauthorized();
  }
  const hash = tokenHash(token);
  const row = findToken.get(hash) as Record<string, unknown> | undefined;
  if (!row) {
    throw unauthorized();
  }
  const now = new Date();
  const touched = touchToken.run(
    now.toISOString(),
    hash,
    new Date(now.getTime() - 60 * 60 * 1000).toISOString(),
  );
  if (touched.changes > 0) {
    markDatabaseDirty();
  }
  return {
    accountId: String(row.account_id),
    email: optionalString(row.email),
  };
}

function requireAdmin(authContext: { email?: string }): void {
  const email = authContext.email?.toLowerCase();
  if (!email || !adminEmails.has(email)) {
    throw unauthorized();
  }
}

function requireWorkerBearer(req: IncomingMessage, sailRow: Record<string, unknown>): void {
  const authHeader = req.headers.authorization || "";
  const token = authHeader.startsWith("Bearer ") ? authHeader.slice("Bearer ".length).trim() : "";
  const expected = optionalString(sailRow.worker_token_hash);
  if (!token.startsWith("rdrw_") || !expected || !timingSafeEqualString(tokenHash(token), expected)) {
    throw unauthorized();
  }
}

function createBetterAuth() {
  return betterAuth({
    baseURL: authBaseURL,
    secret: requiredEnv("BETTER_AUTH_SECRET"),
    database,
    socialProviders: socialProviders(),
  });
}

function refreshAuthHandler(force = false): void {
  const nextFingerprint = providerFingerprint();
  if (!force && nextFingerprint === authProviderFingerprint) {
    return;
  }
  authProviderFingerprint = nextFingerprint;
  auth = createBetterAuth();
  authHandler = toNodeHandler(auth.handler);
}

function providerFingerprint(): string {
  return JSON.stringify(configuredProviders());
}

function configuredProviders(): JsonRecord {
  return {
    google: Boolean(oauthValue("GOOGLE_CLIENT_ID", "google_client_id") && oauthValue("GOOGLE_CLIENT_SECRET", "google_client_secret")),
    github: Boolean(oauthValue("GITHUB_CLIENT_ID", "github_client_id") && oauthValue("GITHUB_CLIENT_SECRET", "github_client_secret")),
    githubDevice: Boolean(githubDeviceClientId),
  };
}

function socialProviders(): JsonRecord {
  const providers: JsonRecord = {};
  const googleClientId = oauthValue("GOOGLE_CLIENT_ID", "google_client_id");
  const googleClientSecret = oauthValue("GOOGLE_CLIENT_SECRET", "google_client_secret");
  const githubClientId = oauthValue("GITHUB_CLIENT_ID", "github_client_id");
  const githubClientSecret = oauthValue("GITHUB_CLIENT_SECRET", "github_client_secret");
  if (googleClientId && googleClientSecret) {
    providers.google = {
      clientId: googleClientId,
      clientSecret: googleClientSecret,
    };
  }
  if (githubClientId && githubClientSecret) {
    providers.github = {
      clientId: githubClientId,
      clientSecret: githubClientSecret,
    };
  }
  return providers;
}

function oauthValue(envName: string, settingKey: string): string | undefined {
  return process.env[envName] || settingValue(settingKey);
}

function settingValue(key: string): string | undefined {
  const row = getSetting.get(key) as Record<string, unknown> | undefined;
  return typeof row?.value === "string" && row.value.length > 0 ? row.value : undefined;
}

function setSetting(key: string, value: string): void {
  upsertSetting.run({ key, value, updatedAt: new Date().toISOString() });
  markDatabaseDirty();
}

function ensureColumn(table: string, column: string, definition: string): void {
  const rows = database.prepare(`pragma table_info(${table})`).all() as Array<{ name?: string }>;
  if (rows.some((row) => row.name === column)) {
    return;
  }
  database.prepare(`alter table ${table} add column ${column} ${definition}`).run();
}

function renderHome(res: ServerResponse): void {
  sendHtml(res, renderShell({
    title: "Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Rudder Cloud.</h1>
        <p class="lede">The control plane that lets Rudder hand off coding-agent runs to managed cloud workers. Sign in to connect a laptop.</p>
        <div class="card">
          <a class="provider" href="/cli/login"><span class="icon" aria-hidden="true">${BRAND_SVG}</span><span>Sign in to Rudder Cloud</span></a>
          <div class="device">Or run <code>rudder login</code> from the CLI to open this page with a device code attached.</div>
        </div>
      </section>
    `,
  }));
}

function renderSuccessPage(res: ServerResponse, email?: string): void {
  sendHtml(res, renderShell({
    title: "Signed in · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>You're in.</h1>
        <p class="lede">${email ? `Signed in as <strong>${escapeHtml(email)}</strong>. ` : ""}You can close this tab. The Rudder CLI will pick up the session in a moment.</p>
        <div class="card">
          <span class="pill">Logged in</span>
          <p class="muted" style="margin-top:14px">Try it next:</p>
          <p><code class="kbd">rudder cloud list</code> &middot; <code class="kbd">rudder sail "fix the failing tests"</code></p>
        </div>
      </section>
    `,
  }));
}

function renderExpiredPage(res: ServerResponse, status = 400): void {
  sendHtml(res, renderShell({
    title: "Login expired · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Session expired.</h1>
        <p class="lede">This login link has timed out.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Run the command again to start a fresh session:</p>
          <p><code class="kbd">rudder login</code></p>
        </div>
      </section>
    `,
  }), status);
}

async function readJsonBody(req: IncomingMessage, maxBytes = DEFAULT_MAX_BODY_BYTES): Promise<Json> {
  const body = await readBodyBuffer(req, maxBytes);
  if (body.length === 0) {
    return {};
  }
  try {
    return JSON.parse(body.toString("utf8")) as Json;
  } catch {
    throw badRequest("invalid JSON body");
  }
}

async function readBodyBuffer(req: IncomingMessage, maxBytes: number): Promise<Buffer> {
  const declaredLength = Number(req.headers["content-length"]);
  const declaredTooLarge = Number.isFinite(declaredLength) && declaredLength > maxBytes;
  return await new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    let total = 0;
    let exceeded = declaredTooLarge;
    let settled = false;
    const cleanup = () => {
      req.off("data", onData);
      req.off("end", onEnd);
      req.off("error", onError);
      req.off("aborted", onAborted);
    };
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const onData = (chunk: Buffer | string) => {
      if (exceeded) {
        return;
      }
      const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      total += buf.length;
      if (total > maxBytes) {
        // Drain the rest without retaining it. Waiting for end lets clients
        // reliably receive the 413 instead of seeing an ECONNRESET mid-upload.
        exceeded = true;
        chunks.length = 0;
        return;
      }
      chunks.push(buf);
    };
    const onEnd = () => {
      if (settled) return;
      settled = true;
      cleanup();
      if (exceeded) {
        reject(payloadTooLarge());
      } else {
        resolve(Buffer.concat(chunks, total));
      }
    };
    const onError = (error: Error) => fail(error);
    const onAborted = () => fail(badRequest("request aborted"));
    req.on("data", onData);
    req.once("end", onEnd);
    req.once("error", onError);
    req.once("aborted", onAborted);
  });
}

function objectField(value: Json | undefined, field: string): JsonRecord | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const next = value[field];
  return next && typeof next === "object" && !Array.isArray(next) ? next : undefined;
}

function stringField(value: Json | undefined, field: string): string | undefined {
  return value && typeof value === "object" && !Array.isArray(value) && typeof value[field] === "string"
    ? value[field]
    : undefined;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function sendJson(res: ServerResponse, status: number, body: Json): void {
  setResponseSecurityHeaders(res);
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(body));
}

function sendHtml(res: ServerResponse, body: string, status = 200): void {
  setResponseSecurityHeaders(res);
  res.writeHead(status, { "content-type": "text/html; charset=utf-8" });
  res.end(body);
}

function setResponseSecurityHeaders(res: ServerResponse): void {
  res.setHeader("cache-control", "no-store");
  res.setHeader("x-content-type-options", "nosniff");
  res.setHeader("referrer-policy", "no-referrer");
  res.setHeader("x-frame-options", "DENY");
  res.setHeader(
    "content-security-policy",
    "default-src 'none'; style-src 'unsafe-inline'; img-src https: data:; form-action https://github.com; base-uri 'none'; frame-ancestors 'none'",
  );
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function imageWithTag(image: string, tag: string): string {
  if (image.includes("@")) {
    return image;
  }
  const slashIndex = image.lastIndexOf("/");
  const tagIndex = image.lastIndexOf(":");
  if (tagIndex > slashIndex) {
    return `${image.slice(0, tagIndex + 1)}${tag}`;
  }
  return `${image}:${tag}`;
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

async function getBetterAuthSession(req: IncomingMessage): Promise<{ user?: { id?: string; email?: string } } | null> {
  try {
    const headers = new Headers();
    for (const [key, value] of Object.entries(req.headers)) {
      if (Array.isArray(value)) {
        headers.set(key, value.join(", "));
      } else if (value) {
        headers.set(key, value);
      }
    }
    return await (auth.api as unknown as {
      getSession(input: { headers: Headers }): Promise<{ user?: { id?: string; email?: string } } | null>;
    }).getSession({ headers });
  } catch {
    return null;
  }
}

function parseJson(text: string): Json {
  try {
    return JSON.parse(text) as Json;
  } catch {
    return text;
  }
}

function responseErrorMessage(value: Json | null): string | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  return typeof value.error === "string"
    ? value.error
    : typeof value.message === "string"
      ? value.message
      : undefined;
}

function isSqliteConstraint(error: unknown): boolean {
  return Boolean(
    error
      && typeof error === "object"
      && "code" in error
      && typeof error.code === "string"
      && error.code.startsWith("SQLITE_CONSTRAINT"),
  );
}

function appendHeader(existing: number | string | string[] | undefined, value: string): string[] {
  if (Array.isArray(existing)) {
    return [...existing, value];
  }
  if (typeof existing === "string") {
    return [existing, value];
  }
  if (typeof existing === "number") {
    return [String(existing), value];
  }
  return [value];
}

function splitSetCookieHeader(value: string | null): string[] {
  if (!value) {
    return [];
  }
  return value.split(/,(?=\s*[^;,]+=)/).map((cookie) => cookie.trim()).filter(Boolean);
}

function ensureCloudRuntimeConfigured(runtime: SailRuntime): void {
  ensureS3Configured();
  if (runtime === "fly") {
    ensureFlyConfigured();
  }
}

function ensureS3Configured(): void {
  if (!snapshotBucket) {
    throw new Error("RUDDER_S3_BUCKET is required for cloud snapshots");
  }
}

function ensureFlyConfigured(): void {
  if (!flyApiToken) {
    throw new Error("FLY_API_TOKEN is required to create Fly Machines");
  }
  if (!flyAppName) {
    throw new Error("RUDDER_FLY_APP_NAME is required to create Fly Machines");
  }
  if (!flyWorkerImage) {
    throw new Error("RUDDER_WORKER_IMAGE is required to create Fly Machines");
  }
}

function badRequest(message: string): Error {
  const error = new Error(message);
  (error as Error & { status?: number }).status = 400;
  return error;
}

function unauthorized(): Error {
  const error = new Error("unauthorized");
  (error as Error & { status?: number }).status = 401;
  return error;
}

function conflict(message: string): Error {
  const error = new Error(message);
  (error as Error & { status?: number }).status = 409;
  return error;
}

function payloadTooLarge(): Error {
  const error = new Error("payload too large");
  (error as Error & { status?: number }).status = 413;
  return error;
}

function tooManyRequests(message: string): Error {
  const error = new Error(message);
  (error as Error & { status?: number }).status = 429;
  return error;
}

function requiredEnv(name: string, fallback?: string): string {
  const value = process.env[name] || fallback;
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function positiveIntegerEnv(name: string, fallback: number, min: number, max: number): number {
  const raw = process.env[name]?.trim();
  if (!raw) {
    return fallback;
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new Error(`${name} must be an integer between ${min} and ${max}`);
  }
  return value;
}

function nonNegativeIntegerEnv(name: string, fallback: number, max: number): number {
  const raw = process.env[name]?.trim();
  if (!raw) {
    return fallback;
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 0 || value > max) {
    throw new Error(`${name} must be an integer between 0 and ${max}`);
  }
  return value;
}

function tokenHash(token: string): string {
  return createHash("sha256").update(token).digest("hex").slice(0, 32);
}
