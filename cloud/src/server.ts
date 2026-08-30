import { createHash, createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import fs from "node:fs/promises";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { Readable } from "node:stream";
import { betterAuth } from "better-auth";
import Database from "better-sqlite3";
import { toNodeHandler } from "better-auth/node";
import { GetObjectCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import type { Duplex } from "node:stream";
import { WebSocket, WebSocketServer } from "ws";
import { createSecretsVault, SecretsVaultError, type SecretItemInput, type SecretsVault } from "./secrets.js";
import {
  formatOutputForSlack,
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
  sourceKind?: "snapshot" | "git-clone";
  repoUrl?: string;
  gitRef?: string;
  lastActivityAt?: string;
  lastHeartbeatAt?: string;
  activeAgents?: number;
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

const port = Number(process.env.PORT || 3000);
const baseURL = requiredEnv("BETTER_AUTH_URL", `http://localhost:${port}`);
const authBaseURL = `${baseURL.replace(/\/$/, "")}/api/auth`;
const dataDir = process.env.RUDDER_CLOUD_DATA_DIR || path.join(os.homedir(), ".rudder-cloud");
const dbPath = process.env.RUDDER_CLOUD_DB || path.join(dataDir, "rudder-cloud.sqlite");
const awsRegion = process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || "us-east-1";
const snapshotBucket = process.env.RUDDER_S3_BUCKET || "";
const flyApiToken = process.env.FLY_API_TOKEN || "";
const flyApiBase = (process.env.FLY_API_HOSTNAME || "https://api.machines.dev").replace(/\/$/, "");
const flyAppName = (process.env.RUDDER_FLY_APP_NAME || process.env.FLY_APP_NAME || "").trim();
const flyRegion = (process.env.RUDDER_FLY_REGION || process.env.FLY_REGION || "iad").trim();
const flyWorkerImage = process.env.RUDDER_WORKER_IMAGE || "ghcr.io/viraatdas/rudder-worker:latest";
const flyWorkerMemoryMb = Number(process.env.RUDDER_WORKER_MEMORY_MB || 1024);
const flyWorkerCpus = Number(process.env.RUDDER_WORKER_CPUS || 1);
const flyWorkerCpuKind = process.env.RUDDER_WORKER_CPU_KIND || "shared";
const flyWorkspaceVolumeGb = Number(process.env.RUDDER_WORKSPACE_VOLUME_GB || 3);
const idlePauseMs = Number(process.env.RUDDER_IDLE_PAUSE_MS || 120 * 60 * 1000);
const stateKey = process.env.RUDDER_CLOUD_STATE_KEY || "control-plane/rudder-cloud.sqlite";
const persistStateToS3 = process.env.RUDDER_CLOUD_PERSIST_STATE !== "0";
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
const deviceLogins = new Map<string, DeviceLogin>();
const githubBrowserLogins = new Map<string, GithubBrowserLogin>();
const s3 = new S3Client({ region: awsRegion });
const slack = slackConfigFromEnv();
// Slack message events are delivered at-least-once; dedupe by event_id so a
// retry doesn't double-inject into an instance.
const seenSlackEvents = new Set<string>();

await fs.mkdir(dataDir, { recursive: true });
await restoreDatabaseFromS3();

const database = new Database(dbPath);
database.pragma("journal_mode = WAL");
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
ensureColumn("rudder_workspaces", "region", "text");
ensureColumn("rudder_workspaces", "snapshot_fingerprint", "text");
ensureColumn("rudder_workspaces", "volume_id", "text");
ensureColumn("rudder_workspaces", "source_kind", "text");
ensureColumn("rudder_workspaces", "repo_url", "text");
ensureColumn("rudder_workspaces", "git_ref", "text");
// Busy signal reported by the worker supervisor in each heartbeat: how many
// rudder runs are actively working in that machine. The idle sweep must not
// stop a workspace whose agents are mid-task just because nobody is attached
// (the laptop disconnected) — that is the "sessions survive the commute" case.
ensureColumn("rudder_workspaces", "active_agents", "integer");

// Secrets vault: initialized lazily so a missing RUDDER_SECRETS_KEY only
// breaks the vault endpoints, not the rest of the control plane.
const secretsKeyBase64 = (process.env.RUDDER_SECRETS_KEY || "").trim();
let secretsVaultInstance: SecretsVault | null = null;
function secretsVault(): SecretsVault {
  if (!secretsKeyBase64) {
    throw new SecretsVaultError(503, "secrets vault is not configured on this control plane (RUDDER_SECRETS_KEY missing)");
  }
  if (!secretsVaultInstance) {
    secretsVaultInstance = createSecretsVault(database, secretsKeyBase64);
  }
  return secretsVaultInstance;
}

const insertToken = database.prepare(`
  insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at)
  values (@tokenHash, @accountId, @email, @createdAt, @lastUsedAt)
`);
const findToken = database.prepare("select * from rudder_tokens where token_hash = ?");
const touchToken = database.prepare("update rudder_tokens set last_used_at = ? where token_hash = ?");
const insertSail = database.prepare(`
  insert into rudder_sails (
    id, account_id, status, runtime, repo_name, task, branch, machine_id, machine_state,
    snapshot_key, manifest_json, worker_token_hash, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @status, @runtime, @repoName, @task, @branch, @machineId, @machineState,
    @snapshotKey, @manifestJson, @workerTokenHash, @lastHeartbeatAt, @createdAt, @updatedAt
  )
`);
// Only ever used to clear a dead sail so its preferred id can be relaunched
// (see createSail); account-scoped so one account cannot clear another's row.
const deleteSailRow = database.prepare(`
  delete from rudder_sails where id = @id and account_id = @accountId
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
const updateHeartbeat = database.prepare(`
  update rudder_sails
  set status = @status,
      machine_id = coalesce(@machineId, machine_id),
      machine_state = @machineState,
      last_heartbeat_at = @lastHeartbeatAt,
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
// Newest stored snapshot for a repo, across sails AND workspaces — this is what a
// Slack `launch <repo> <task>` boots from (there is no terminal to upload a fresh
// snapshot, so the last thing pushed up for that repo is the base).
const latestSailSnapshotForRepo = database.prepare(`
  select snapshot_key, account_id, updated_at from rudder_sails
  where repo_name = ? and snapshot_key is not null
  order by updated_at desc limit 1
`);
const latestWorkspaceSnapshotForRepo = database.prepare(`
  select snapshot_key, account_id, updated_at from rudder_workspaces
  where repo_name = ? and snapshot_key is not null
  order by updated_at desc limit 1
`);
const listSailSnapshotRepos = database.prepare(`
  select repo_name, max(updated_at) as updated_at from rudder_sails
  where repo_name is not null and snapshot_key is not null group by repo_name
`);
const listWorkspaceSnapshotRepos = database.prepare(`
  select repo_name, max(updated_at) as updated_at from rudder_workspaces
  where repo_name is not null and snapshot_key is not null group by repo_name
`);
const insertWorkspace = database.prepare(`
  insert into rudder_workspaces (
    id, account_id, workspace_key, repo_name, status, machine_id, machine_state,
    snapshot_key, snapshot_fingerprint, region, volume_id, worker_token_hash,
    source_kind, repo_url, git_ref,
    last_activity_at, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @workspaceKey, @repoName, @status, @machineId, @machineState,
    @snapshotKey, @snapshotFingerprint, @region, @volumeId, @workerTokenHash,
    @sourceKind, @repoUrl, @gitRef,
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
      active_agents = @activeAgents,
      last_heartbeat_at = @lastHeartbeatAt,
      updated_at = @updatedAt
  where id = @id
    and status <> 'stopped'
    and (@machineId is null or machine_id is null or machine_id = @machineId)
`);
const getSetting = database.prepare("select value from rudder_settings where key = ?");
const upsertSetting = database.prepare(`
  insert into rudder_settings (key, value, updated_at)
  values (@key, @value, @updatedAt)
  on conflict(key) do update set value = excluded.value, updated_at = excluded.updated_at
`);

let authProviderFingerprint = providerFingerprint();
let auth: ReturnType<typeof createBetterAuth> = createBetterAuth();
let authHandler = toNodeHandler(auth.handler);

// noDelay: keystroke/echo frames relayed between attach clients and workers
// are a few bytes each; they must never sit in the kernel buffer waiting for
// piggyback ACKs. Node >=18 defaults accepted sockets to noDelay, but make it
// explicit so the relay's latency floor doesn't hinge on a runtime default.
const server = http.createServer({ noDelay: true }, async (req, res) => {
  res.once("finish", () => {
    schedulePersistDatabase();
  });
  try {
    const url = new URL(req.url || "/", baseURL);
    if (url.pathname.startsWith("/api/auth")) {
      refreshAuthHandler();
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
        secrets: Boolean(secretsKeyBase64),
        auth: configuredProviders(),
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
      await handleCliLoginStart(res);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/cli/login/github-token") {
      await handleCliGithubToken(req, res);
      return;
    }
    if (url.pathname === "/api/cli/login/poll") {
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
    if (url.pathname.startsWith("/api/rudder/secrets")) {
      await handleSecretsApi(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/rudder/sail")) {
      await handleSailApi(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/rudder/workspace")) {
      await handleWorkspaceApi(req, res, url);
      return;
    }
    renderHome(res);
  } catch (error) {
    const status = error && typeof error === "object" && "status" in error && typeof error.status === "number"
      ? error.status
      : 500;
    console.error(`request error ${req.method} ${req.url} -> ${status}`, error);
    sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
  }
});

const SAIL_ATTACH_PATH_RE = /^\/api\/rudder\/sail\/([^/]+)\/(worker|attach)$/;
const WORKSPACE_ATTACH_PATH_RE = /^\/api\/rudder\/workspace\/([^/]+)\/(worker|attach)$/;
const REPLAY_BUFFER_BYTES = 256 * 1024;
const PENDING_INPUT_BYTES = 64 * 1024;
const MAX_SOCKET_BUFFER_BYTES = 1024 * 1024;
const SOCKET_PING_MS = 30_000;

type Channel = {
  worker?: WebSocket;
  clients: Set<WebSocket>;
  controller?: WebSocket;
  buffer: Buffer[];
  bufferBytes: number;
  pendingInput: Buffer[];
  pendingInputBytes: number;
  latestResize?: string;
};

type ChannelKind = "sail" | "workspace";

const sailChannels = new Map<string, Channel>();
const workspaceChannels = new Map<string, Channel>();

const wss = new WebSocketServer({
  noServer: true,
  // Compress large terminal redraws (a single Claude frame can be many kB of
  // ANSI). Skip small frames so per-keystroke echoes don't pay the deflate
  // tax.
  perMessageDeflate: {
    threshold: 1024,
    zlibDeflateOptions: { level: 1 },
  },
});

server.on("upgrade", (req: IncomingMessage, socket: Duplex, head: Buffer) => {
  // Both the attach client and the worker dial into this relay, so both relay
  // hops are accepted sockets that pass through here. Disable Nagle on each so
  // single-keystroke frames forward immediately in both directions.
  if (socket instanceof net.Socket) {
    try { socket.setNoDelay(true); } catch { /* ignore */ }
  }
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
    destroyUpgrade(socket, 403);
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
    const machineId = typeof req.headers["x-rudder-machine-id"] === "string"
      ? req.headers["x-rudder-machine-id"]
      : undefined;
    const expectedMachineId = optionalString(workspaceRow.machine_id);
    if (machineId && expectedMachineId && machineId !== expectedMachineId) {
      destroyUpgrade(socket, 409);
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
    destroyUpgrade(socket, 403);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachClient("workspace", workspaceId, ws));
}

function destroyUpgrade(socket: Duplex, status: number): void {
  const reason = status === 401 ? "Unauthorized"
    : status === 403 ? "Forbidden"
    : status === 404 ? "Not Found"
    : status === 409 ? "Conflict"
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
    channel = {
      clients: new Set(),
      buffer: [],
      bufferBytes: 0,
      pendingInput: [],
      pendingInputBytes: 0,
    };
    map.set(id, channel);
  }
  return channel;
}

function disposeChannelIfEmpty(kind: ChannelKind, id: string): void {
  const map = channelMap(kind);
  const channel = map.get(id);
  if (!channel) {
    return;
  }
  if (!channel.worker && channel.clients.size === 0) {
    map.delete(id);
  }
}

function attachWorker(kind: ChannelKind, id: string, ws: WebSocket): void {
  const channel = getChannel(kind, id);
  if (channel.worker && channel.worker.readyState === WebSocket.OPEN) {
    try {
      channel.worker.close(4000, "replaced");
    } catch {
      // ignore
    }
  }
  channel.worker = ws;
  channel.buffer = [];
  channel.bufferBytes = 0;
  enableSocketLiveness(ws);
  flushPendingClientState(channel);
  broadcastStatus(channel, "worker-connected");
  // Deliver any Slack messages that were queued while this sail was asleep.
  if (kind === "sail") {
    flushPendingSlackInputs(id);
  }

  ws.on("message", (data, isBinary) => {
    if (channel.worker !== ws) {
      return;
    }
    if (isBinary && data instanceof Buffer) {
      pushBuffer(channel, data);
      for (const client of channel.clients) {
        sendSocket(client, data, true);
      }
      return;
    }
    if (data instanceof Buffer) {
      const text = data.toString("utf8");
      forwardTextToClients(channel, text);
    }
  });

  ws.on("close", () => {
    if (channel.worker !== ws) {
      return;
    }
    channel.worker = undefined;
    broadcastStatus(channel, "worker-disconnected");
    disposeChannelIfEmpty(kind, id);
  });
  ws.on("error", () => undefined);
}

function attachClient(kind: ChannelKind, id: string, ws: WebSocket): void {
  const channel = getChannel(kind, id);
  channel.clients.add(ws);
  channel.controller = ws;
  enableSocketLiveness(ws);
  if (kind === "workspace") {
    touchWorkspaceActivity(id);
  }

  sendSocket(ws, JSON.stringify({
    type: "status",
    state: isOpen(channel.worker) ? "worker-connected" : "worker-waiting",
    control: "active",
  }));
  if (channel.bufferBytes > 0) {
    sendSocket(ws, Buffer.concat(channel.buffer, channel.bufferBytes), true);
  }

  ws.on("message", (data, isBinary) => {
    if (channel.controller !== ws) {
      sendSocket(ws, JSON.stringify({ type: "status", state: "read-only" }));
      return;
    }
    const worker = channel.worker;
    if (isBinary && data instanceof Buffer) {
      if (isOpen(worker)) {
        sendSocket(worker, data, true);
      } else {
        pushPendingInput(channel, data);
        sendSocket(ws, JSON.stringify({ type: "status", state: "input-buffered" }));
      }
      if (kind === "workspace") {
        touchWorkspaceActivity(id);
      }
      return;
    }
    if (data instanceof Buffer) {
      const text = data.toString("utf8");
      if (isResizeMessage(text)) {
        channel.latestResize = text;
      }
      if (isOpen(worker)) {
        sendSocket(worker, text);
      }
    }
  });

  ws.on("close", () => {
    channel.clients.delete(ws);
    if (channel.controller === ws) {
      channel.controller = Array.from(channel.clients).reverse().find(isOpen);
      if (channel.controller) {
        sendSocket(channel.controller, JSON.stringify({
          type: "status",
          state: "controller-promoted",
          control: "active",
        }));
      }
    }
    disposeChannelIfEmpty(kind, id);
  });
  ws.on("error", () => undefined);
}

function pushBuffer(channel: Channel, chunk: Buffer): void {
  if (chunk.length >= REPLAY_BUFFER_BYTES) {
    channel.buffer = [chunk.subarray(chunk.length - REPLAY_BUFFER_BYTES)];
    channel.bufferBytes = REPLAY_BUFFER_BYTES;
    return;
  }
  channel.buffer.push(chunk);
  channel.bufferBytes += chunk.length;
  while (channel.bufferBytes > REPLAY_BUFFER_BYTES && channel.buffer.length > 0) {
    const dropped = channel.buffer.shift();
    if (dropped) {
      channel.bufferBytes -= dropped.length;
    }
  }
}

function pushPendingInput(channel: Channel, chunk: Buffer): void {
  const remaining = PENDING_INPUT_BYTES - channel.pendingInputBytes;
  if (remaining <= 0) {
    return;
  }
  const accepted = chunk.length <= remaining ? chunk : chunk.subarray(0, remaining);
  channel.pendingInput.push(Buffer.from(accepted));
  channel.pendingInputBytes += accepted.length;
}

function flushPendingClientState(channel: Channel): void {
  const worker = channel.worker;
  if (!isOpen(worker)) {
    return;
  }
  if (channel.latestResize) {
    sendSocket(worker, channel.latestResize);
  }
  for (const chunk of channel.pendingInput) {
    sendSocket(worker, chunk, true);
  }
  channel.pendingInput = [];
  channel.pendingInputBytes = 0;
}

function isResizeMessage(text: string): boolean {
  try {
    const value = JSON.parse(text) as { type?: unknown };
    return value?.type === "resize";
  } catch {
    return false;
  }
}

function isOpen(ws: WebSocket | undefined): ws is WebSocket {
  return Boolean(ws && ws.readyState === WebSocket.OPEN);
}

function sendSocket(ws: WebSocket | undefined, data: string | Buffer, binary = false): boolean {
  if (!isOpen(ws)) {
    return false;
  }
  if (ws.bufferedAmount > MAX_SOCKET_BUFFER_BYTES) {
    ws.terminate();
    return false;
  }
  if (binary) {
    ws.send(data, { binary: true });
  } else {
    ws.send(data);
  }
  return true;
}

function enableSocketLiveness(ws: WebSocket): void {
  let alive = true;
  ws.on("pong", () => { alive = true; });
  const timer = setInterval(() => {
    if (!alive || ws.readyState !== WebSocket.OPEN) {
      clearInterval(timer);
      if (ws.readyState !== WebSocket.CLOSED) {
        ws.terminate();
      }
      return;
    }
    alive = false;
    try { ws.ping(); } catch { ws.terminate(); }
  }, SOCKET_PING_MS);
  timer.unref?.();
  ws.once("close", () => clearInterval(timer));
}

function forwardTextToClients(channel: Channel, text: string): void {
  for (const client of channel.clients) {
    sendSocket(client, text);
  }
}

function broadcastStatus(channel: Channel, state: string): void {
  const payload = JSON.stringify({ type: "status", state });
  for (const client of channel.clients) {
    sendSocket(client, payload);
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
  worker.send(Buffer.from(payload, "utf8"), { binary: true });
  if (kind === "workspace") {
    touchWorkspaceActivity(id);
  }
  return true;
}

// Read the most recent raw output (with ANSI) buffered for an instance. The
// replay buffer only exists while the worker is connected, so this returns ""
// for paused/dead instances.
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

function touchWorkspaceActivity(workspaceId: string): void {
  const previous = workspaceActivityWrites.get(workspaceId) ?? 0;
  const nowMs = Date.now();
  if (nowMs - previous < 1_000) {
    return;
  }
  workspaceActivityWrites.set(workspaceId, nowMs);
  try {
    const now = new Date().toISOString();
    updateWorkspaceActivity.run({ id: workspaceId, lastActivityAt: now, updatedAt: now });
  } catch {
    // ignore
  }
}

const workspaceActivityWrites = new Map<string, number>();

server.listen(port, () => {
  console.log(`rudder cloud listening on ${baseURL}`);
});

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void persistDatabaseToS3().finally(() => {
      process.exit(signal === "SIGINT" ? 130 : 143);
    });
  });
}

const workspaceIdleMs = Number(process.env.RUDDER_WORKSPACE_IDLE_MS || 30 * 60 * 1000);
const workspaceSweepIntervalMs = Number(process.env.RUDDER_WORKSPACE_SWEEP_MS || 60 * 1000);
if (workspaceIdleMs >= 60 * 1000 && workspaceSweepIntervalMs >= 10 * 1000) {
  const timer = setInterval(() => {
    void sweepIdleWorkspaces().catch(() => undefined);
  }, workspaceSweepIntervalMs);
  timer.unref?.();
}

// How stale the busy signal may be and still count: the supervisor heartbeats
// every 30s, so a machine that is alive and busy refreshes it constantly. A
// frozen active_agents > 0 with NO recent heartbeat means the supervisor died
// (or the machine is wedged) — that must NOT keep a dead machine un-stopped
// forever, so the busy exemption requires a fresh heartbeat.
const busyHeartbeatFreshMs = 5 * 60 * 1000;

async function sweepIdleWorkspaces(): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    return;
  }
  const rows = listAllRunningWorkspaces.all() as Record<string, unknown>[];
  const now = Date.now();
  for (const row of rows) {
    const workspace = rowToWorkspace(row);
    if (!workspace.machineId) {
      continue;
    }
    if (Array.from(workspaceChannels.get(workspace.id)?.clients ?? []).some(isOpen)) {
      continue;
    }
    // BUSY EXEMPTION: agents are actively working in this machine (reported by
    // the worker, not guessed from user activity). Stopping it would kill live
    // cloud sessions the moment the user's laptop disconnected for longer than
    // the idle window — the exact opposite of what the workspace is for.
    if ((workspace.activeAgents ?? 0) > 0) {
      const heartbeatMs = Date.parse(workspace.lastHeartbeatAt ?? "");
      if (Number.isFinite(heartbeatMs) && now - heartbeatMs <= busyHeartbeatFreshMs) {
        continue;
      }
    }
    const last = workspace.lastActivityAt ?? workspace.lastHeartbeatAt ?? workspace.updatedAt;
    const lastMs = Date.parse(last);
    if (!Number.isFinite(lastMs) || now - lastMs < workspaceIdleMs) {
      continue;
    }
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
    } catch {
      // ignore; will retry next sweep
    }
  }
}

let persistTimer: NodeJS.Timeout | undefined;
let persistInFlight = false;
let persistAgain = false;

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
    const buffer = await streamToBuffer(response.Body);
    if (buffer.length === 0) {
      return;
    }
    await fs.writeFile(dbPath, buffer, { mode: 0o600 });
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
    clearTimeout(persistTimer);
  }
  persistTimer = setTimeout(() => {
    persistTimer = undefined;
    void persistDatabaseToS3();
  }, 750);
  persistTimer.unref?.();
}

async function persistDatabaseToS3(): Promise<void> {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  if (persistInFlight) {
    persistAgain = true;
    return;
  }
  persistInFlight = true;
  try {
    database.pragma("wal_checkpoint(FULL)");
    const body = await fs.readFile(dbPath);
    await s3.send(new PutObjectCommand({
      Bucket: snapshotBucket,
      Key: stateKey,
      Body: body,
      ContentType: "application/vnd.sqlite3",
      ServerSideEncryption: "AES256",
    }));
  } catch (error) {
    console.warn(`rudder cloud state persist failed: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    persistInFlight = false;
    if (persistAgain) {
      persistAgain = false;
      schedulePersistDatabase();
    }
  }
}

async function streamToBuffer(body: unknown): Promise<Buffer> {
  if (Buffer.isBuffer(body)) {
    return body;
  }
  if (body instanceof Uint8Array) {
    return Buffer.from(body);
  }
  if (body instanceof Readable || (body && typeof body === "object" && Symbol.asyncIterator in body)) {
    const chunks: Buffer[] = [];
    for await (const chunk of body as AsyncIterable<Buffer | Uint8Array | string>) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    }
    return Buffer.concat(chunks);
  }
  if (body && typeof body === "object" && "transformToByteArray" in body) {
    const bytes = await (body as { transformToByteArray(): Promise<Uint8Array> }).transformToByteArray();
    return Buffer.from(bytes);
  }
  throw new Error("unsupported S3 body type");
}

async function handleCliLoginStart(res: ServerResponse): Promise<void> {
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
  const issued = issueRudderToken(`github:${user.id}`, user.email ?? `${user.login}@users.noreply.github.com`);
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
    const issued = issueRudderToken(`github:${user.id}`, user.email ?? `${user.login}@users.noreply.github.com`);
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

async function githubOAuthRequest<T>(url: string, body: Record<string, string>): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `${response.status} ${response.statusText}`);
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
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `GitHub manifest conversion failed: ${response.status}`);
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
    String(session.user.id || `better-auth:${randomUUID()}`),
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
    if (status === "stopped" && !machineId) {
      skipped.push({ kind: "workspace", id, reason: "already stopped" });
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
        update.run("stopped", now, id);
        if (volumeId) {
          await destroyWorkspaceVolume(volumeId);
          clearVolume.run(now, id);
        }
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
    schedulePersistDatabase();
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
  const response = await fetch(
    `${flyApiBase}/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    {
      method: "GET",
      headers: {
        Authorization: `Bearer ${flyApiToken}`,
        Accept: "application/json",
      },
    },
  );
  if (response.status === 404) {
    return false;
  }
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Fly API ${response.status}: ${text.trim()}`);
  }
  // 200 means the machine exists, but Fly may still report state "destroyed"
  // for already-deleted machines. Treat destroyed/destroying as gone.
  try {
    const body = (await response.json()) as { state?: string } | null;
    const state = body?.state;
    if (state === "destroyed" || state === "destroying") {
      return false;
    }
  } catch {
    // fall through: assume exists
  }
  return true;
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

  const workerSecretsMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/secrets$/);
  if (req.method === "GET" && workerSecretsMatch) {
    await handleWorkerSecrets(req, res, findSailById.get(workerSecretsMatch[1]) as Record<string, unknown> | undefined);
    return;
  }

  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/sail") {
    await refreshAccountSails(authContext.accountId);
    sendJson(res, 200, { sails: listAccountSails(authContext.accountId) });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/launch") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const sail = await createSail(authContext.accountId, body);
    sendJson(res, 200, sail);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/onload") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const sail = await createSail(authContext.accountId, body, stringField(body, "runId"));
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
    const next = await refreshByoVmBootstrap(sail, authContext.accountId);
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
    const next = await mutateSail(sail, authContext.accountId, match[2]);
    sendJson(res, 200, next);
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

function parseSecretItem(body: Json): SecretItemInput {
  const record = (body && typeof body === "object" && !Array.isArray(body) ? body : {}) as JsonRecord;
  const name = typeof record.name === "string" ? record.name.trim() : "";
  const kind = record.kind === "file" ? "file" : record.kind === "env" ? "env" : undefined;
  const valueBase64 = typeof record.valueBase64 === "string" ? record.valueBase64 : "";
  if (!name || !kind || !valueBase64) {
    throw new SecretsVaultError(400, "name, kind (env|file) and valueBase64 are required");
  }
  return {
    name,
    kind,
    filePath: typeof record.filePath === "string" ? record.filePath : undefined,
    value: Buffer.from(valueBase64, "base64"),
    source: typeof record.source === "string" ? record.source : undefined,
  };
}

const MAX_SECRETS_BULK_BODY_BYTES = 64 * 1024 * 1024;

async function handleSecretsApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/secrets") {
    const { secrets, version } = secretsVault().list(authContext.accountId);
    sendJson(res, 200, { secrets: secrets as unknown as Json, secretsVersion: version });
    return;
  }
  if (req.method === "PUT" && url.pathname === "/api/rudder/secrets/item") {
    const body = await readJsonBody(req, 4 * 1024 * 1024);
    const saved = secretsVault().put(authContext.accountId, parseSecretItem(body));
    // Persist immediately: a secret acknowledged to the client must survive a
    // control-plane crash, not wait out the 750ms debounce window.
    await persistDatabaseToS3();
    sendJson(res, 200, { ok: true, secret: saved as unknown as Json });
    return;
  }
  if (req.method === "DELETE" && url.pathname === "/api/rudder/secrets/item") {
    const name = (url.searchParams.get("name") || "").trim();
    if (!name) {
      sendJson(res, 400, { error: "name is required" });
      return;
    }
    const removed = secretsVault().remove(authContext.accountId, name);
    await persistDatabaseToS3();
    sendJson(res, removed ? 200 : 404, removed ? { ok: true } : { error: "secret not found" });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/secrets/bulk") {
    const body = await readJsonBody(req, MAX_SECRETS_BULK_BODY_BYTES);
    const items = body && typeof body === "object" && !Array.isArray(body) && Array.isArray((body as JsonRecord).items)
      ? ((body as JsonRecord).items as Json[])
      : [];
    const results: Json[] = [];
    for (const item of items) {
      try {
        const saved = secretsVault().put(authContext.accountId, parseSecretItem(item));
        results.push({ name: saved.name, ok: true });
      } catch (error) {
        const parsedName = item && typeof item === "object" && !Array.isArray(item) && typeof (item as JsonRecord).name === "string"
          ? String((item as JsonRecord).name)
          : "<invalid>";
        results.push({
          name: parsedName,
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    }
    await persistDatabaseToS3();
    sendJson(res, 200, { results });
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

// Worker-authenticated secrets fetch: the supervisor calls this at every boot
// (rootfs is ephemeral) with its rdrw_ bearer, before spawning the agent.
// Secret values are deliberately NOT placed in Fly machine env/config, which
// is readable via the Fly API.
async function handleWorkerSecrets(
  req: IncomingMessage,
  res: ServerResponse,
  row: Record<string, unknown> | undefined,
): Promise<void> {
  if (!row) {
    sendJson(res, 404, { error: "not found" });
    return;
  }
  requireWorkerBearer(req, row);
  if (!secretsKeyBase64) {
    // Vault not configured: boot proceeds on legacy snapshot credentials.
    sendJson(res, 200, { version: 0, env: {}, files: [] });
    return;
  }
  sendJson(res, 200, secretsVault().exportForWorker(String(row.account_id)) as unknown as Json);
}

async function handleWorkspaceApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const workerSecretsMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/secrets$/);
  if (req.method === "GET" && workerSecretsMatch) {
    await handleWorkerSecrets(req, res, findWorkspaceById.get(workerSecretsMatch[1]) as Record<string, unknown> | undefined);
    return;
  }

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
    sendJson(res, 200, {
      workspaces: listAccountWorkspaces(authContext.accountId).map(annotateWorkspaceClients),
    });
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/rudder/workspace/lookup") {
    const repo = url.searchParams.get("repo");
    const key = repo && REPO_SLUG_RE.test(repo.trim().replace(/\.git$/, ""))
      ? workspaceKeyForRepo(repo.trim().replace(/\.git$/, ""))
      : url.searchParams.get("key");
    if (!key) {
      sendJson(res, 400, { error: "key or repo is required" });
      return;
    }
    const row = findWorkspaceByKey.get(authContext.accountId, key) as
      | Record<string, unknown>
      | undefined;
    if (!row) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    sendJson(res, 200, annotateWorkspaceClients(rowToWorkspace(row)));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/workspace/attach") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const result = await ensureWorkspaceForAttach(authContext.accountId, body);
    sendJson(res, 200, result);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/workspace/create") {
    const body = await readJsonBody(req, 1024 * 1024);
    const result = await ensureCloneWorkspace(authContext.accountId, body);
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
    const next = await mutateWorkspace(workspace, stopMatch[2]);
    sendJson(res, 200, next);
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

async function createSail(
  accountId: string,
  body: Json,
  preferredId?: string,
  existingSnapshotKey?: string,
): Promise<Sail> {
  const runtime = sailRuntimeFromBody(body);
  ensureCloudRuntimeConfigured(runtime);
  const now = new Date().toISOString();
  // A preferred id (onload keys sails by the local run id) must be retryable:
  // a handoff whose first attempt died mid-flight leaves a sail row behind,
  // and the retry used to 500 on the id's UNIQUE constraint. If that sail's
  // worker is LIVE, the conversation is already in the cloud — return it
  // instead of cloning it. Otherwise clear the corpse (row + any machine)
  // and launch fresh.
  if (preferredId) {
    const existing = getAccountSail(preferredId, accountId);
    if (existing) {
      if (instanceHasLiveWorker("sail", preferredId)) {
        return existing;
      }
      if (existing.machineId) {
        await destroyFlyMachine(existing.machineId).catch(() => undefined);
      }
      deleteSailRow.run({ id: preferredId, accountId });
    }
  }
  // Slack-launched sails reuse an already-stored snapshot (there is no terminal
  // to upload a fresh one from); everything else uploads as before.
  const snapshot = existingSnapshotKey
    ? { key: existingSnapshotKey }
    : await storeSnapshot(accountId, body);
  const id = preferredId || uniqueSailId(stringField(body, "name"));
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const task = stringField(body, "task");
  const repoName = stringField(body, "repoName");
  const snapshotInput = objectField(body, "snapshot");
  const manifest = snapshotInput ? objectField(snapshotInput, "manifest") : undefined;
  const manifestRepo = manifest ? objectField(manifest, "repo") : undefined;
  const branch = manifestRepo ? stringField(manifestRepo, "branch") : undefined;
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
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });

  // Announce the new instance into the shared Slack channel so it is talk-able
  // from there. Fire-and-forget: a Slack hiccup must never fail a launch.
  void announceSailToSlack(id, task, repoName, runtime);

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
    return {
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
  }

  const snapshotUrl = await signedSnapshotUrl(snapshot.key);
  const machine = await createFlyMachine({
    sailId: id,
    accountId,
    snapshotUrl,
    workerToken,
    task,
    repoName,
  });
  const status = flyStateToSailStatus(machine.state);
  updateSail.run({
    id,
    accountId,
    status,
    machineId: machine.id ?? null,
    machineState: machine.state ?? null,
    updatedAt: new Date().toISOString(),
  });
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
  const buffer = Buffer.from(base64, "base64");
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
        auto_destroy: false,
      },
    },
  });
  if (!machine.id) {
    return machine;
  }
  return await startFlyMachine(machine.id, `sail ${params.sailId}`);
}

async function mutateSail(sail: Sail, accountId: string, action: string): Promise<Sail> {
  if (sail.runtime === "fly") {
    return await mutateFlySail(sail, accountId, action);
  }
  if (action === "stop") {
    updateSail.run({
      id: sail.id,
      accountId,
      status: "stopped",
      machineId: sail.machineId ?? null,
      machineState: "stopped",
      updatedAt: new Date().toISOString(),
    });
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
    );
  } else if (action === "resume" || action === "onload") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/start`,
      { method: "POST", body: {} },
    );
  } else if (action === "stop") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGINT", timeout: "10s" } },
    );
  } else {
    throw badRequest(`unsupported sail action: ${action}`);
  }
  updateSail.run({
    id: sail.id,
    accountId,
    status: action === "pause" ? "paused" : flyStateToSailStatus(machine.state),
    machineId: sail.machineId,
    machineState: machine.state ?? null,
    updatedAt: new Date().toISOString(),
  });
  return getAccountSail(sail.id, accountId) ?? sail;
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
    "docker run --rm",
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
  const sailRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!sailRow) {
    sendJson(res, 404, { error: "sail not found" });
    return;
  }
  requireWorkerBearer(req, sailRow);
  const body = await readJsonBody(req);
  const state = stringField(body, "state");
  const status: SailStatus = state === "completed"
    ? "completed"
    : state === "failed"
      ? "failed"
      : "running";
  const now = new Date().toISOString();
  const previousStatus = String(sailRow.status);
  updateHeartbeat.run({
    id: sailId,
    status,
    machineId: stringField(body, "machineId") ?? null,
    machineState: state || status,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
  // On the transition into a terminal state, post the final result + a tail of
  // output into the instance's Slack thread.
  if ((status === "completed" || status === "failed") && previousStatus !== status) {
    const threadTs = optionalString(sailRow.slack_thread_ts);
    const icon = status === "completed" ? "✅" : "❌";
    const tail = readChannelOutput("sail", sailId);
    void postToSlack(
      `${icon} *${sailId}* ${status}.${tail ? `\n${formatOutputForSlack(tail)}` : ""}`,
      threadTs,
    );
  }
  sendJson(res, 200, { ok: true, status });
}

async function refreshAccountSails(accountId: string): Promise<void> {
  const sails = listAccountSails(accountId);
  for (const sail of sails) {
    if (sail.runtime !== "fly" || !flyApiToken || !flyAppName) {
      continue;
    }
    if (sail.status === "completed" || sail.status === "failed") {
      continue;
    }
    if (!sail.machineId) {
      continue;
    }
    if (shouldPauseStaleSail(sail)) {
      await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/suspend`,
        { method: "POST", body: {} },
      ).catch(() => null);
      const now = new Date().toISOString();
      updateSail.run({
        id: sail.id,
        accountId,
        status: "paused",
        machineId: sail.machineId,
        machineState: "suspended",
        updatedAt: now,
      });
      continue;
    }
    const machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}`,
      { method: "GET" },
    ).catch(() => null);
    if (!machine) {
      continue;
    }
    updateSail.run({
      id: sail.id,
      accountId,
      status: flyStateToSailStatus(machine.state),
      machineId: sail.machineId,
      machineState: machine.state ?? null,
      updatedAt: new Date().toISOString(),
    });
  }
}

function shouldPauseStaleSail(sail: Sail): boolean {
  if (sail.status !== "running" || !idlePauseMs || idlePauseMs < 1000) {
    return false;
  }
  const heartbeatOrCreated = sail.lastHeartbeatAt ?? sail.createdAt;
  const lastSeen = Date.parse(heartbeatOrCreated);
  return Number.isFinite(lastSeen) && Date.now() - lastSeen > idlePauseMs;
}

async function ensureWorkspaceForAttach(accountId: string, body: Json): Promise<JsonRecord> {
  ensureCloudRuntimeConfigured("fly");
  const workspaceKey = stringField(body, "workspaceKey");
  if (!workspaceKey || workspaceKey.length < 4 || workspaceKey.length > 128) {
    throw badRequest("workspaceKey is required (4-128 chars)");
  }
  const repoName = stringField(body, "repoName");
  const existingRow = findWorkspaceByKey.get(accountId, workspaceKey) as Record<string, unknown> | undefined;
  if (existingRow) {
    const existing = rowToWorkspace(existingRow);
    return await reuseOrRestartWorkspace(existing, body, repoName);
  }
  return await createWorkspace(accountId, workspaceKey, body, repoName);
}

async function createWorkspace(
  accountId: string,
  workspaceKey: string,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  const region = sanitizeRegion(stringField(body, "region"));
  const snapshot = await storeSnapshot(accountId, body);
  const snapshotFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId(repoName);
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
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
    sourceKind: "snapshot",
    repoUrl: null,
    gitRef: null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  const regionToUse = region ?? flyRegion;
  const volume = await createWorkspaceVolume(id, regionToUse);
  const volumeId = volume.id;
  updateWorkspaceVolume.run({
    id,
    volumeId: volumeId ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  const snapshotUrl = await signedSnapshotUrl(snapshot.key);
  const machine = await createFlyWorkspaceMachine({
    workspaceId: id,
    accountId,
    snapshotUrl,
    workerToken,
    repoName,
    region: regionToUse,
    volumeId,
  });
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({
    id,
    status: started.status,
    machineId: machine.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
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

const REPO_SLUG_RE = /^[\w.-]+\/[\w.-]+$/;

// Clone-based workspaces are keyed by repo identity instead of the local
// checkout path. The "gh:" prefix keeps the keyspace disjoint from the
// 32-hex path-hash keys snapshot workspaces use.
function workspaceKeyForRepo(slug: string): string {
  return `gh:${createHash("sha256").update(`github.com/${slug.toLowerCase()}`).digest("hex").slice(0, 29)}`;
}

async function ensureCloneWorkspace(accountId: string, body: Json): Promise<JsonRecord> {
  ensureCloudRuntimeConfigured("fly");
  const repo = (stringField(body, "repo") ?? "").trim().replace(/\.git$/, "");
  if (!REPO_SLUG_RE.test(repo)) {
    throw badRequest("repo must look like owner/name");
  }
  const branch = stringField(body, "branch")?.trim() || undefined;
  const workspaceKey = workspaceKeyForRepo(repo);
  const existingRow = findWorkspaceByKey.get(accountId, workspaceKey) as Record<string, unknown> | undefined;
  const repoName = repo.split("/")[1];
  if (existingRow) {
    return await reuseOrRestartWorkspace(rowToWorkspace(existingRow), body, repoName);
  }
  const repoUrl = `https://github.com/${repo}.git`;
  const region = sanitizeRegion(stringField(body, "region"));
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId(repoName);
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  insertWorkspace.run({
    id,
    accountId,
    workspaceKey,
    repoName,
    status: "queued",
    machineId: null,
    machineState: null,
    snapshotKey: null,
    snapshotFingerprint: null,
    region,
    volumeId: null,
    workerTokenHash: tokenHash(workerToken),
    sourceKind: "git-clone",
    repoUrl,
    gitRef: branch ?? null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  const regionToUse = region ?? flyRegion;
  const volume = await createWorkspaceVolume(id, regionToUse);
  updateWorkspaceVolume.run({
    id,
    volumeId: volume.id ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  const machine = await createFlyWorkspaceMachine({
    workspaceId: id,
    accountId,
    workerToken,
    repoName,
    region: regionToUse,
    volumeId: volume.id,
    gitRemote: repoUrl,
    gitRef: branch,
  });
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({
    id,
    status: started.status,
    machineId: machine.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  const next = findWorkspaceById.get(id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : { id, accountId, workspaceKey }), isNew: true } as unknown as JsonRecord;
}

async function reuseOrRestartWorkspace(
  workspace: Workspace,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  ensureFlyConfigured();
  let machine: FlyMachine | null = null;
  if (workspace.machineId) {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}`,
      { method: "GET" },
    ).catch(() => null);
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
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : { ...workspace, status: "running" }), isNew: false } as unknown as JsonRecord;
  }
  // Path 1: machine already running -> just attach.
  if (machine && (machine.state === "started" || machine.state === "starting")) {
    const now = new Date().toISOString();
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: flyStateToWorkspaceStatus(machine.state),
      machineId: machine.id ?? workspace.machineId ?? null,
      machineState: machine.state ?? null,
      updatedAt: now,
    });
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : workspace), isNew: false } as unknown as JsonRecord;
  }

  const isClone = workspace.sourceKind === "git-clone";
  const incomingFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const snapshotInput = objectField(body, "snapshot");
  // Clone workspaces have no snapshot to compare: the repo's source of truth
  // is origin, so a stopped machine is always warm-restartable.
  const fingerprintMatches = isClone || Boolean(
    incomingFingerprint
      && workspace.snapshotFingerprint
      && incomingFingerprint === workspace.snapshotFingerprint,
  );
  // A mismatch triggers the expensive destroy+recreate path, so name the
  // reason: which side is missing, or that both exist and differ. Prefixes
  // only — fingerprints hash credentials-adjacent inputs.
  if (!isClone && !fingerprintMatches) {
    console.info(
      `workspace ${workspace.id}: fingerprint mismatch (stored ${workspace.snapshotFingerprint?.slice(0, 8) ?? "none"}, incoming ${incomingFingerprint?.slice(0, 8) ?? "none"}) — recreate path`,
    );
  }
  // The recreate decision has several SILENT inputs beyond the fingerprint
  // (machine lookup returned null, machine in an unexpected state, workspace
  // marked failed, no snapshot key). When the expensive path is about to be
  // taken anyway, name the full condition set once — chasing a spurious
  // rebuild without this took hours of log archaeology.
  const explainRecreate = () =>
    `machine=${machine ? `${machine.id}:${machine.state}` : "null"} recordedMachineId=${workspace.machineId ?? "none"} fingerprintMatches=${fingerprintMatches} snapshotKey=${workspace.snapshotKey ? "yes" : "no"} status=${workspace.status}`;

  // Path 2: warm restart — machine exists but is stopped, and the user's
  // local state hasn't changed (fingerprint matches). With a persistent
  // Fly Volume mounted at /workspace, the supervisor's alreadyStaged()
  // marker survives stop+start so the supervisor skips re-staging entirely
  // (~1-2s total). Without a volume, the marker is on the ephemeral rootfs
  // and the supervisor re-downloads using a fresh URL via the snapshot-url
  // endpoint (~10-30s). Either way, no destroy+recreate is needed.
  // A machine caught mid-"stopping" (the supervisor just exited) is seconds
  // from being warm-restartable; without this wait it missed both reuse paths
  // and fell through to a full destroy+recreate. Poll briefly for it to
  // settle — if it doesn't, the recreate fallthrough still applies.
  if (machine?.id && machine.state === "stopping") {
    const machineId = machine.id;
    for (let attempt = 0; attempt < 15 && machine?.state === "stopping"; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
      machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
        { method: "GET" },
      ).catch(() => machine);
    }
  }

  if (
    machine
    && machine.id
    && (machine.state === "stopped" || machine.state === "suspended")
    && fingerprintMatches
    && (workspace.snapshotKey || isClone)
    && workspace.status !== "failed"
  ) {
    const restarted = await warmRestartWorkspaceMachine({
      machineId: machine.id,
      snapshotKey: workspace.snapshotKey ?? "",
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
  //
  // REFUSE BEFORE DESTROYING ANYTHING. Recreating a snapshot workspace needs
  // a snapshot in hand; the CLI's first attach is a cheap probe without one.
  // This check used to live after the machine and volume destroys, so the
  // probe demolished a healthy stopped machine, THEN 400'd "snapshot is
  // required" — turning every warm reuse into a 40s full rebuild (and, had
  // the CLI died between probe and re-upload, stranding the workspace with
  // no machine at all). Validate first; destroy only when rebuild can follow.
  if (!isClone && !snapshotInput) {
    console.info(`workspace ${workspace.id}: probe reached recreate path (${explainRecreate()})`);
    throw badRequest("snapshot is required to (re)create this workspace");
  }
  console.info(`workspace ${workspace.id}: recreating (${explainRecreate()})`);
  // `machine` is null both when the machine is genuinely gone AND when the GET
  // above simply failed, because that lookup swallows its error. Keying the
  // delete off it meant one flaky Fly read left the machine alive, still holding
  // the volume, and the recreate died on "volume is currently bound to machine".
  // A DELETE against a machine that no longer exists is a 404 this tolerates.
  if (workspace.machineId && machine?.state !== "destroyed" && machine?.state !== "destroying") {
    await destroyFlyMachine(workspace.machineId);
  }
  // Clone workspaces recreate by re-cloning from origin — no snapshot upload.
  // Keep a surviving volume: the staged clone on it short-circuits re-staging.
  if (isClone) {
    return await recreateCloneWorkspaceMachine(workspace, repoName);
  }
  // A Fly volume cannot be deleted while its machine is attached. Delete the
  // machine first, then wait for detachment; if cleanup still fails, keep the
  // old volume id in SQLite and abort instead of creating an untracked volume.
  if (workspace.volumeId) {
    await destroyWorkspaceVolume(workspace.volumeId);
  }
  if (!snapshotInput) {
    throw badRequest("snapshot is required to (re)create this workspace");
  }
  const snapshotKey = (await storeSnapshot(workspace.accountId, body)).key;
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  const region = sanitizeRegion(stringField(body, "region")) ?? workspace.region ?? null;
  const regionToUse = region ?? flyRegion;
  let newVolumeId: string | undefined;
  try {
    const freshVolume = await createWorkspaceVolume(workspace.id, regionToUse);
    newVolumeId = freshVolume.id;
  } catch (error) {
    // Volume name may still be reserved if the prior delete is still
    // settling on Fly's side. Retry once with a timestamp suffix.
    const message = error instanceof Error ? error.message : String(error);
    console.warn(`recreate volume for ${workspace.id} failed (${message}); retrying with suffix`);
    const suffixed = await createWorkspaceVolumeNamed(
      workspaceVolumeName(workspace.id, String(Date.now()).slice(-6)),
      regionToUse,
    );
    newVolumeId = suffixed.id;
  }
  updateWorkspaceVolume.run({
    id: workspace.id,
    volumeId: newVolumeId ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceSnapshot.run({ id: workspace.id, snapshotKey, snapshotFingerprint: incomingFingerprint, updatedAt: now });
  updateWorkspaceWorkerToken.run({ id: workspace.id, workerTokenHash: tokenHash(workerToken), updatedAt: now });
  const snapshotUrl = await signedSnapshotUrl(snapshotKey);
  const fresh = await createFlyWorkspaceMachine({
    workspaceId: workspace.id,
    accountId: workspace.accountId,
    snapshotUrl,
    workerToken,
    repoName: repoName ?? workspace.repoName,
    region: regionToUse,
    volumeId: newVolumeId,
  });
  const started = workspaceStartResult(fresh.state);
  updateWorkspaceMachine.run({
    id: workspace.id,
    status: started.status,
    machineId: fresh.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : workspace), isNew: true } as unknown as JsonRecord;
}

async function recreateCloneWorkspaceMachine(
  workspace: Workspace,
  repoName: string | undefined,
): Promise<JsonRecord> {
  const now = new Date().toISOString();
  const regionToUse = workspace.region ?? flyRegion;
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  updateWorkspaceWorkerToken.run({ id: workspace.id, workerTokenHash: tokenHash(workerToken), updatedAt: now });
  const createMachine = (volumeId: string | undefined) => createFlyWorkspaceMachine({
    workspaceId: workspace.id,
    accountId: workspace.accountId,
    workerToken,
    repoName: repoName ?? workspace.repoName,
    region: regionToUse,
    volumeId,
    gitRemote: workspace.repoUrl,
    gitRef: workspace.gitRef,
  });
  let fresh: FlyMachine;
  try {
    fresh = await createMachine(workspace.volumeId);
  } catch (error) {
    // The recorded volume may itself be gone (destroyed out-of-band). Create a
    // replacement and retry once; the worker re-clones onto the fresh disk.
    const message = error instanceof Error ? error.message : String(error);
    console.warn(`clone machine create for ${workspace.id} failed (${message}); retrying with a fresh volume`);
    const volume = await createWorkspaceVolumeNamed(
      workspaceVolumeName(workspace.id, String(Date.now()).slice(-6)),
      regionToUse,
    );
    updateWorkspaceVolume.run({
      id: workspace.id,
      volumeId: volume.id ?? null,
      region: regionToUse,
      updatedAt: new Date().toISOString(),
    });
    fresh = await createMachine(volume.id);
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
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : workspace), isNew: true } as unknown as JsonRecord;
}

const FLY_REGIONS = new Set([
  "ams","arn","atl","bog","bom","bos","cdg","den","dfw","ewr","eze","fra","gdl",
  "gig","gru","hkg","iad","jnb","lax","lhr","mad","maa","mia","nrt","ord","otp",
  "phx","qro","scl","sea","sin","sjc","syd","waw","yul","yyz",
]);

function sanitizeRegion(value: string | undefined): string | null {
  if (!value) return null;
  const lower = value.trim().toLowerCase();
  return FLY_REGIONS.has(lower) ? lower : null;
}

async function warmRestartWorkspaceMachine(params: {
  machineId: string;
  snapshotKey: string;
}): Promise<FlyMachine | null> {
  ensureFlyConfigured();
  // Refresh the image while preserving the complete existing machine config,
  // including env and mounts. Persistent workspaces otherwise stay pinned to
  // the worker image digest they were originally created with forever.
  void params.snapshotKey;
  try {
    await refreshWorkspaceMachineImage(params.machineId);
    return await startFlyMachine(params.machineId, `workspace ${params.machineId}`);
  } catch (error) {
    console.warn(`warm restart ${params.machineId}: start failed: ${error instanceof Error ? error.message : String(error)}`);
    throw error;
  }
}

async function refreshWorkspaceMachineImage(machineId: string): Promise<void> {
  const machine = await flyRequest<FlyMachine>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    { method: "GET" },
  );
  if (!machine.config) {
    return;
  }
  // Re-submit even when the configured tag text is unchanged. Fly resolves a
  // mutable tag such as `latest` when the machine config is updated, not when
  // an already-created machine is merely started.
  await flyRequest<FlyMachine>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    {
      method: "POST",
      body: { config: { ...machine.config, image: flyWorkerImage } },
    },
  );
}

async function startFlyMachine(machineId: string, label: string): Promise<FlyMachine> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= 12; attempt += 1) {
    try {
      const started = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}/start`,
        { method: "POST", body: {} },
      );
      // Fly's start endpoint responds with {previous_state, migrated, ...} —
      // NO id and NO state. Callers persist `.id` into the workspace row; when
      // this returned the raw start response, every create/recreate stored
      // machineId=NULL, so the next attach couldn't find the machine and did
      // a full ~40s destroy+rebuild instead of an instant reuse. The row only
      // healed when the supervisor's heartbeat reported the id minutes later.
      // Always return an object that carries the id and a usable state.
      return {
        ...started,
        id: started?.id ?? machineId,
        state: started?.state ?? "starting",
      };
    } catch (error) {
      lastError = error;
      const machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
        { method: "GET" },
      ).catch(() => null);
      if (machine?.state === "started" || machine?.state === "starting") {
        return machine;
      }
      if (attempt < 12) {
        await sleep(Math.min(5000, attempt * 2000));
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
  snapshotUrl?: string;
  workerToken: string;
  repoName?: string;
  region?: string;
  volumeId?: string;
  gitRemote?: string;
  gitRef?: string;
}): Promise<FlyMachine> {
  ensureFlyConfigured();
  const config: JsonRecord = {
    image: flyWorkerImage,
    env: {
      RUDDER_WORKSPACE_ID: params.workspaceId,
      RUDDER_ACCOUNT_ID: params.accountId,
      RUDDER_CLOUD_URL: baseURL,
      RUDDER_WORKER_TOKEN: params.workerToken,
      // Clone-mode machines get the (public) remote/ref here; the git token
      // itself arrives via the vault at boot, never via Fly machine config.
      ...(params.snapshotUrl ? { RUDDER_SNAPSHOT_URL: params.snapshotUrl } : {}),
      ...(params.gitRemote ? { RUDDER_GIT_REMOTE: params.gitRemote } : {}),
      ...(params.gitRef ? { RUDDER_GIT_REF: params.gitRef } : {}),
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
    },
  });
  if (!machine.id) {
    return machine;
  }
  return await startFlyMachine(machine.id, `workspace ${params.workspaceId}`);
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

/** Force-destroy a Fly machine. A machine that is already gone counts as done. */
async function destroyFlyMachine(machineId: string): Promise<void> {
  try {
    await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}?force=true`,
      { method: "DELETE" },
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!/not found|404/i.test(message)) {
      throw error;
    }
  }
}

/**
 * The machine Fly says is holding a volume, read out of its own refusal:
 * "failed_precondition: volume is currently bound to machine: 80e966df155e08".
 */
function machineHoldingVolume(message: string): string | null {
  return /bound to machine:?\s*([0-9a-z]+)/i.exec(message)?.[1] ?? null;
}

async function destroyWorkspaceVolume(volumeId: string): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    throw new Error("Fly is not configured; cannot destroy workspace volume");
  }
  let lastError: unknown;
  // Detaching after a machine destroy is not instant, so this retries rather
  // than failing on the first refusal.
  const freed = new Set<string>();
  for (let attempt = 1; attempt <= 12; attempt += 1) {
    try {
      await flyRequest<FlyVolume>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/volumes/${encodeURIComponent(volumeId)}`,
        { method: "DELETE" },
      );
      return;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (/not found|404/i.test(message)) {
        return;
      }
      lastError = error;
      // A volume still bound to a machine will refuse forever, so retrying the
      // same DELETE just burns the budget and reports a precondition the caller
      // cannot act on. Fly names the machine in the refusal: destroy THAT one.
      //
      // It is not always the machine this workspace has on record. The recreate
      // path only deletes `workspace.machineId`, and it skips even that when the
      // machine GET happened to fail, so an orphan from an earlier half-finished
      // recreate can sit on the volume with nothing pointing at it.
      const holder = machineHoldingVolume(message);
      if (holder && !freed.has(holder)) {
        freed.add(holder);
        console.warn(
          `volume ${volumeId} is held by machine ${holder}; destroying it to release the volume`,
        );
        try {
          await destroyFlyMachine(holder);
        } catch (destroyError) {
          const detail =
            destroyError instanceof Error ? destroyError.message : String(destroyError);
          throw new Error(
            `destroy volume ${volumeId} failed: it is bound to machine ${holder}, which could not be destroyed: ${detail}`,
          );
        }
      }
      if (attempt < 12) {
        await sleep(1_000);
      }
    }
  }
  throw new Error(`destroy volume ${volumeId} failed: ${lastError instanceof Error ? lastError.message : String(lastError)}`);
}

async function mutateWorkspace(workspace: Workspace, action: string): Promise<Workspace> {
  if (!workspace.machineId) {
    throw badRequest("workspace does not have a Fly machine yet");
  }
  let machine: FlyMachine;
  if (action === "stop" || action === "pause") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGTERM", timeout: "10s" } },
    );
  } else if (action === "resume") {
    await refreshWorkspaceMachineImage(workspace.machineId);
    machine = await startFlyMachine(workspace.machineId, `workspace ${workspace.id}`);
  } else {
    throw badRequest(`unsupported workspace action: ${action}`);
  }
  const started = workspaceStartResult(machine.state);
  const status = action === "stop" || action === "pause"
    ? "stopped" as WorkspaceStatus
    : started.status;
  const now = new Date().toISOString();
  updateWorkspaceMachine.run({
    id: workspace.id,
    status,
    machineId: workspace.machineId,
    machineState: action === "stop" || action === "pause" ? machine.state ?? null : started.machineState,
    updatedAt: now,
  });
  if (action === "resume") {
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  }
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return next ? rowToWorkspace(next) : { ...workspace, status, machineState: machine.state };
}

async function handleWorkspaceHeartbeat(req: IncomingMessage, res: ServerResponse, workspaceId: string): Promise<void> {
  const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const body = await readJsonBody(req);
  const state = stringField(body, "state");
  const machineId = stringField(body, "machineId");
  const expectedMachineId = optionalString(row.machine_id);
  if (machineId && expectedMachineId && machineId !== expectedMachineId) {
    sendJson(res, 409, { error: "stale workspace machine" });
    return;
  }
  const currentStatus = String(row.status) as WorkspaceStatus;
  if (currentStatus === "stopped") {
    sendJson(res, 200, { ok: true, status: currentStatus, ignored: true });
    return;
  }
  const status: WorkspaceStatus = state === "failed" ? "failed" : "running";
  const activeAgents = numberField(body, "activeAgents");
  const now = new Date().toISOString();
  updateWorkspaceHeartbeat.run({
    id: workspaceId,
    status,
    machineId: machineId ?? null,
    machineState: state || status,
    activeAgents: activeAgents ?? 0,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
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

function listAccountWorkspaces(accountId: string): Workspace[] {
  return (listWorkspacesForAccount.all(accountId) as unknown[]).map(rowToWorkspace);
}

function annotateWorkspaceClients(workspace: Workspace): Workspace & { clientCount: number } {
  const channel = workspaceChannels.get(workspace.id);
  const clientCount = channel
    ? Array.from(channel.clients).filter(isOpen).length
    : 0;
  return { ...workspace, clientCount };
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
    sourceKind: (optionalString(value.source_kind) as "snapshot" | "git-clone" | undefined) ?? "snapshot",
    repoUrl: optionalString(value.repo_url),
    gitRef: optionalString(value.git_ref),
    lastActivityAt: optionalString(value.last_activity_at),
    lastHeartbeatAt: optionalString(value.last_heartbeat_at),
    activeAgents: Number.isFinite(Number(value.active_agents)) && value.active_agents !== null
      ? Math.max(0, Math.floor(Number(value.active_agents)))
      : 0,
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
    case "suspended":
      return "stopped";
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
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `Fly API ${response.status}`);
  }
  return parsed as T;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function githubUser(token: string): Promise<{ id: number | string; login: string; email?: string }> {
  const response = await fetch("https://api.github.com/user", {
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "rudder-cloud",
      "X-GitHub-Api-Version": "2022-11-28",
    },
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
  const email = await githubVerifiedPrimaryEmail(token);
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
  task: string | undefined,
  repoName: string | undefined,
  runtime: SailRuntime,
): Promise<void> {
  if (!slack.enabled) {
    return;
  }
  const lines = [
    `🚀 *${id}* launched on ${runtime === "fly" ? "Fly" : "your VM"}.`,
    repoName ? `repo: \`${repoName}\`` : "",
    task ? `> ${task}` : "",
    "_Reply in this thread to talk to the agent._",
  ].filter(Boolean);
  const ts = await postToSlack(lines.join("\n"));
  if (ts) {
    updateSailSlackThread.run({ id, threadTs: ts, updatedAt: new Date().toISOString() });
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

async function readRawBody(req: IncomingMessage, maxBytes = DEFAULT_MAX_BODY_BYTES): Promise<string> {
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      req.destroy();
      throw payloadTooLarge();
    }
    chunks.push(buf);
  }
  return Buffer.concat(chunks).toString("utf8");
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
      seenSlackEvents.clear();
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

// Newest snapshot stored for a repo (sails and workspaces both count) — the base a
// Slack `launch` boots from. Returns the owning account so the new sail lands in
// the same account that uploaded the snapshot.
function latestSnapshotForRepo(repoName: string): { key: string; accountId: string } | null {
  const rows = [
    latestSailSnapshotForRepo.get(repoName),
    latestWorkspaceSnapshotForRepo.get(repoName),
  ].filter((row): row is Record<string, unknown> => Boolean(row));
  rows.sort((a, b) => String(b.updated_at || "").localeCompare(String(a.updated_at || "")));
  const newest = rows[0];
  if (!newest) {
    return null;
  }
  return { key: String(newest.snapshot_key), accountId: String(newest.account_id) };
}

function listSnapshotRepoSummaries(): { repo: string; updatedAt: string }[] {
  const merged = new Map<string, string>();
  const rows = [
    ...(listSailSnapshotRepos.all() as Record<string, unknown>[]),
    ...(listWorkspaceSnapshotRepos.all() as Record<string, unknown>[]),
  ];
  for (const row of rows) {
    const repo = String(row.repo_name || "");
    const at = String(row.updated_at || "");
    if (!repo) {
      continue;
    }
    const prev = merged.get(repo);
    if (!prev || at > prev) {
      merged.set(repo, at);
    }
  }
  return [...merged.entries()]
    .map(([repo, updatedAt]) => ({ repo, updatedAt }))
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
}

// Messages sent from Slack to a sleeping instance: the machine is woken and the
// message is delivered once its worker WS reconnects. Bounded per sail so a sail
// that never comes back cannot hoard memory.
const pendingSlackInputs = new Map<string, { message: string; threadTs?: string }[]>();

function queuePendingSlackInput(sailId: string, message: string, threadTs?: string): void {
  const queue = pendingSlackInputs.get(sailId) ?? [];
  queue.push({ message, threadTs });
  while (queue.length > 20) {
    queue.shift();
  }
  pendingSlackInputs.set(sailId, queue);
}

function flushPendingSlackInputs(sailId: string): void {
  const queue = pendingSlackInputs.get(sailId);
  if (!queue || queue.length === 0) {
    return;
  }
  pendingSlackInputs.delete(sailId);
  // Give the resumed agent PTY a beat to be ready before typing into it.
  setTimeout(() => {
    for (const item of queue) {
      const delivered = sendInputToChannel("sail", sailId, item.message, true);
      if (delivered) {
        scheduleOutputEcho(sailId, item.threadTs);
      } else if (item.threadTs) {
        void postToSlack(
          `*${sailId}* reconnected but did not accept the queued message; send it again.`,
          item.threadTs,
        );
      }
    }
  }, 3000);
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
  const text = typeof event.text === "string" ? event.text : "";
  const ts = typeof event.ts === "string" ? event.ts : undefined;
  const threadTs = typeof event.thread_ts === "string" ? event.thread_ts : undefined;
  const inThread = Boolean(threadTs);

  // Plain channel chatter is not for us — only act on @mentions and on replies
  // inside a thread we own.
  const threadSail = threadTs
    ? (findSailBySlackThread.get(threadTs) as Record<string, unknown> | undefined)
    : undefined;
  if (type === "message" && !threadSail) {
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
  }

  switch (command.action) {
    case "help":
      await postToSlack(SLACK_HELP_TEXT, replyThread);
      return;
    case "list": {
      const sails = listRunningSails.all().map(rowToSail);
      if (sails.length === 0) {
        await postToSlack("No running cloud instances. Start one with `rudder cloud \"<task>\"`.", replyThread);
        return;
      }
      const lines = sails.map((s) => {
        const live = instanceHasLiveWorker("sail", s.id) ? "🟢" : "⚪️";
        return `${live} *${s.id}* — ${s.status}${s.task ? ` · ${s.task}` : ""}`;
      });
      await postToSlack(`*Cloud instances*\n${lines.join("\n")}`, replyThread);
      return;
    }
    case "output": {
      const tail = readChannelOutput("sail", command.id);
      await postToSlack(
        instanceHasLiveWorker("sail", command.id)
          ? formatOutputForSlack(tail)
          : `*${command.id}* is not connected.`,
        replyThread,
      );
      return;
    }
    case "stop": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      if (!row) {
        await postToSlack(`No instance \`${command.id}\`.`, replyThread);
        return;
      }
      const accountId = String(row.account_id);
      const sail = getAccountSail(command.id, accountId);
      if (sail) {
        await mutateSail(sail, accountId, "stop").catch(() => undefined);
      }
      await postToSlack(`🛑 Stopping *${command.id}*.`, replyThread);
      return;
    }
    case "pause":
    case "resume": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      const accountId = row ? String(row.account_id) : "";
      const sail = accountId ? getAccountSail(command.id, accountId) : null;
      if (!sail) {
        await postToSlack(`No instance \`${command.id}\`.`, replyThread);
        return;
      }
      try {
        await mutateSail(sail, accountId, command.action);
        await postToSlack(
          command.action === "pause" ? `⏸️ Paused *${command.id}*.` : `▶️ Waking *${command.id}*.`,
          replyThread,
        );
      } catch (error) {
        await postToSlack(
          `${command.action} failed: ${error instanceof Error ? error.message : String(error)}`,
          replyThread,
        );
      }
      return;
    }
    case "repos": {
      const repos = listSnapshotRepoSummaries();
      if (repos.length === 0) {
        await postToSlack(
          "No repo snapshots yet. Run `rudder cloud` from a repo once to upload one.",
          replyThread,
        );
        return;
      }
      const lines = repos
        .slice(0, 20)
        .map((r) => `• *${r.repo}* — snapshot from ${r.updatedAt.slice(0, 10) || "unknown"}`);
      await postToSlack(
        `*Launchable repos*\n${lines.join("\n")}\nStart one with \`launch <repo> <task>\`.`,
        replyThread,
      );
      return;
    }
    case "launch": {
      const found = latestSnapshotForRepo(command.repo);
      if (!found) {
        await postToSlack(
          `No snapshot for *${command.repo}*. Run \`rudder cloud\` from that repo once, or say \`repos\` to see what's launchable.`,
          replyThread,
        );
        return;
      }
      try {
        const sail = await createSail(
          found.accountId,
          { runtime: "fly", task: command.task, repoName: command.repo },
          undefined,
          found.key,
        );
        // createSail already announces the new instance as its own thread root;
        // this reply just closes the loop on the command itself.
        await postToSlack(
          `🚀 Launching *${sail.id}* on *${command.repo}* — ${command.task}`,
          replyThread,
        );
      } catch (error) {
        await postToSlack(
          `Launch failed: ${error instanceof Error ? error.message : String(error)}`,
          replyThread,
        );
      }
      return;
    }
    case "talk":
    case "thread-reply": {
      const targetId = command.action === "talk" ? command.id : String(threadSail?.id || "");
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
      const ownThread = optionalString(sailRow?.slack_thread_ts) ?? replyThread;
      const delivered = sendInputToChannel("sail", targetId, message, true);
      if (!delivered) {
        // The instance is asleep (idle-suspended) or its worker dropped. For a
        // live-able Fly sail, wake the machine and deliver the message once its
        // worker reconnects — this is what makes 24/7 Slack control feel always-on.
        const accountId = sailRow ? String(sailRow.account_id) : "";
        const sail = accountId ? getAccountSail(targetId, accountId) : null;
        const wakeable = sail?.runtime === "fly"
          && Boolean(sail.machineId)
          && sail.status !== "completed"
          && sail.status !== "failed";
        if (sail && wakeable) {
          queuePendingSlackInput(targetId, message, ownThread);
          try {
            await mutateSail(sail, accountId, "resume");
            await postToSlack(
              `⏰ *${targetId}* was asleep — waking it; your message will be delivered when it reconnects.`,
              replyThread,
            );
          } catch (error) {
            pendingSlackInputs.delete(targetId);
            await postToSlack(
              `*${targetId}* is not connected and could not be woken: ${error instanceof Error ? error.message : String(error)}`,
              replyThread,
            );
          }
          return;
        }
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
  touchToken.run(new Date().toISOString(), hash);
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
  if (!token.startsWith("rdrw_") || !expected || tokenHash(token) !== expected) {
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
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      req.destroy();
      throw payloadTooLarge();
    }
    chunks.push(buf);
  }
  if (chunks.length === 0) {
    return {};
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
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
function numberField(value: Json | undefined, field: string): number | undefined {
  const raw = value && typeof value === "object" && !Array.isArray(value) ? value[field] : undefined;
  const num = typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN;
  return Number.isFinite(num) && num >= 0 ? Math.floor(num) : undefined;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function sendJson(res: ServerResponse, status: number, body: Json): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function sendHtml(res: ServerResponse, body: string, status = 200): void {
  res.writeHead(status, { "content-type": "text/html; charset=utf-8" });
  res.end(body);
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
    throw new Error("FLY_APP_NAME is required to create Fly Machines");
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

function payloadTooLarge(): Error {
  const error = new Error("payload too large");
  (error as Error & { status?: number }).status = 413;
  return error;
}

function requiredEnv(name: string, fallback?: string): string {
  const value = process.env[name] || fallback;
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function tokenHash(token: string): string {
  return createHash("sha256").update(token).digest("hex").slice(0, 32);
}
