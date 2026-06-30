import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { WebSocket } from "ws";
import { currentBranch, currentCommit, findRepoRoot } from "./git.js";
import {
  type MigrationCandidate,
  type MigrationManifestEntry,
  type MigrationPlan,
  type MigrationSnapshotManifest,
  applyDefaultDecisions,
  buildFreshHandoffPrompt,
  cloudWorktreeAbsolutePath,
  findMigrationCandidates,
  migrationSummary,
  summaryAsJson,
} from "./migration.js";
import { cloudAuthPath } from "./state.js";
import type { CloudAuthState, CloudSail, JsonValue } from "./types.js";
import {
  ensureDir,
  commandEnv,
  commandExists,
  expandHome,
  isTty,
  newRunId,
  nowIso,
  pathExists,
  promptConfirm,
  promptText,
  promptSelect,
  promptSecret,
  readJson,
  runCommand,
  shortenHome,
  shellQuote,
  writeJson,
} from "./util.js";

type CloudCommandOptions = {
  json?: boolean;
  homePaths?: string[];
  sshHost?: string;
  noAttach?: boolean;
  quietBanner?: boolean;
};

type LoginStartResponse = {
  loginUrl?: string;
  verificationUri?: string;
  pollUrl?: string;
  deviceCode?: string;
  interval?: number;
  expiresIn?: number;
};

type LoginPollResponse = {
  pending?: boolean;
  token?: string;
  accessToken?: string;
  accountId?: string;
  email?: string;
  expiresAt?: string;
  expiresIn?: number;
};

type SnapshotManifest = {
  version: 1;
  createdAt: string;
  repo: {
    root: string;
    branch: string;
    commit: string;
  };
  homePaths: string[];
  rudderState?: {
    runs: number;
    files: string[];
  };
  migratedAgents?: number;
  capturedEnvVars?: number;
};

type SnapshotOptions = {
  includeRudderState?: boolean;
  migration?: {
    repoName: string;
    plan: MigrationPlan;
  };
};

type CreatedSnapshot = {
  tempDir: string;
  archivePath: string;
  manifest: SnapshotManifest;
};

type CloudClient = {
  baseUrl: string;
  request<T>(pathOrUrl: string, init: { method: string; body?: JsonValue; timeoutMs?: number }): Promise<T>;
};

type CloudRuntime = "fly" | "byo-vm";

class CloudRequestError extends Error {
  readonly status: number;
  readonly payload: JsonValue | null;

  constructor(status: number, message: string, payload: JsonValue | null) {
    super(message);
    this.name = "CloudRequestError";
    this.status = status;
    this.payload = payload;
  }
}

const DEFAULT_LOGIN_INTERVAL_MS = 2000;
const DEFAULT_LOGIN_TIMEOUT_MS = 5 * 60 * 1000;
const DEFAULT_CLOUD_REQUEST_TIMEOUT_MS = 30_000;
const DEFAULT_CLOUD_UPLOAD_TIMEOUT_MS = 5 * 60_000;
const DEFAULT_ATTACH_CONNECT_TIMEOUT_MS = 20_000;
const DEFAULT_ATTACH_READY_TIMEOUT_MS = 5 * 60_000;
const DEFAULT_REGION_DETECT_TIMEOUT_MS = 1_500;
// The control plane accepts a 256 MiB JSON request. Base64 costs 4/3 plus
// manifest/JSON overhead, so fail locally with a useful message before reading
// an archive that cannot possibly be accepted.
const MAX_SNAPSHOT_ARCHIVE_BYTES = 190 * 1024 * 1024;
const DEFAULT_CLOUD_URL = "https://rudder-cloud-control.fly.dev";
const GITHUB_CLI_CLIENT_ID = "178c6fc778ccc68e1d6a";
const MAX_HOME_SECRET_SCAN_BYTES = 1024 * 1024;
const DEFAULT_HOME_PATHS = [
  // Agent CLIs
  "~/.claude/.credentials.json",
  "~/.claude/settings.json",
  "~/.claude/CLAUDE.md",
  "~/.claude.json",
  "~/.codex/auth.json",
  "~/.codex/config.toml",
  "~/.codex/AGENTS.md",
  "~/.codex/hooks.json",
  "~/.codex/rules",
  // Git + GitHub
  "~/.gitconfig",
  "~/.config/gh",
  // Shell rc so PATH/aliases/env exports come along
  "~/.zshrc",
  "~/.zprofile",
  "~/.bashrc",
  "~/.bash_profile",
  "~/.profile",
  // Package managers
  "~/.npmrc",
  "~/.yarnrc",
  "~/.yarnrc.yml",
  "~/.cargo/config",
  "~/.cargo/config.toml",
  // Cloud provider CLIs
  "~/.vercel",
  "~/.config/vercel",
  "~/.aws/config",
  "~/.aws/credentials",
  "~/.config/gcloud/configurations",
  "~/.config/gcloud/active_config",
  "~/.config/gcloud/credentials.db",
  "~/.config/gcloud/access_tokens.db",
  "~/.config/gcloud/application_default_credentials.json",
  "~/.kube/config",
  // Netrc for tools that auth via netrc
  "~/.netrc",
];
// Paths or path components that should never be uploaded under any
// circumstance — even if a user's DEFAULT_HOME_PATHS entry references them.
// Specifically leaving out .aws/.kube/.docker so the corresponding configs
// can ship; .ssh/.gnupg/keychains stay blocked because they contain
// private key material that isn't recoverable if leaked.
const SECRET_PATH_PARTS = new Set([
  ".ssh",
  ".gnupg",
  "keychains",
]);
const BULKY_HOME_PATH_PARTS = new Set([
  ".cache",
  "archived_sessions",
  "backups",
  "cache",
  "file-history",
  "log",
  "paste-cache",
  "plugins",
  "projects",
  "session",
  "sessions",
  "shell_snapshots",
  "skills",
  "telemetry",
  "todos",
  "worktrees",
]);
const ALWAYS_BULKY_PATH_PARTS = new Set(["node_modules"]);
const SECRET_BASENAMES = new Set([
  ".env",
  "id_rsa",
  "id_ed25519",
  "known_hosts",
]);
const BULKY_HOME_BASENAME_PATTERNS = [
  /^history\./,
  /^logs?_/,
  /^state_\d+\.sqlite/,
  /\.sqlite(?:-(?:wal|shm))?$/,
  /\.log$/,
  /\.jsonl$/,
];

export async function runCloudCommand(command: string, args: string[], options: CloudCommandOptions = {}): Promise<void> {
  const subcommand = args[0] ?? "";
  const rest = args.slice(1);

  if (command === "cloud" && subcommand === "help") {
    printCloudHelp();
    return;
  }

  // `rudder cloud` with no further args means "open the cloud workspace for
  // this repo". Explicit subcommands (sail, launch, etc.) keep their old
  // behavior. `rudder sail` and `rudder cloud sail` still launch a sail.
  if (command === "cloud" && subcommand === "") {
    await workspaceCommand([], options);
    return;
  }

  switch (subcommand) {
    case "login":
      await login(options);
      return;
    case "launch":
      await launch(rest, options, "task");
      return;
    case "sail":
      await launch(rest, options);
      return;
    case "byoc":
      await setupByoc(rest, options);
      return;
    case "vm":
    case "byo-vm":
      await launch(rest, options, "task", "byo-vm");
      return;
    case "list":
    case "ls":
      await listSails(options);
      return;
    case "status":
      await status(options);
      return;
    case "logs":
      await logs(rest, options);
      return;
    case "attach":
      await attach(rest, options);
      return;
    case "talk":
    case "say":
    case "msg":
      await talk(rest, options);
      return;
    case "output":
    case "tail":
      await tailOutput(rest, options);
      return;
    case "quickstart":
      await quickstart(options);
      return;
    case "slack":
      await slackSetup(rest, options);
      return;
    case "workspace":
      await workspaceCommand(rest, options);
      return;
    case "onload":
      await onload(rest, options);
      return;
    case "bootstrap":
      await bootstrap(rest, options);
      return;
    case "pause":
      await mutateSail("pause", rest, options);
      return;
    case "resume":
      await mutateSail("resume", rest, options);
      return;
    case "stop":
      await mutateSail("stop", rest, options);
      return;
    case "delete":
    case "rm":
      await mutateSail("delete", rest, options);
      return;
    case "setup-github":
      await setupOAuthProvider("github", rest, options);
      return;
    case "setup-google":
      await setupOAuthProvider("google", rest, options);
      return;
    case "setup-byoc":
      await setupByoc(rest, options);
      return;
    case "setup-vm":
      await setupByoc(rest, options);
      return;
    case "setup-fly":
      await configureDefaultRuntime("fly", options);
      return;
    case "setup":
      if (rest[0] === "byoc" || rest[0] === "vm" || rest[0] === "byo-vm") {
        await setupByoc(rest.slice(1), options);
        return;
      }
      if (rest[0] === "fly") {
        await configureDefaultRuntime("fly", options);
        return;
      }
      throw new Error("Usage: rudder cloud setup byoc | rudder cloud setup fly");
    case "runtime":
      await runtime(rest, options);
      return;
    default:
      // A bare `rudder cloud "<text>"` / `rudder sail "<text>"` is the documented
      // way to launch a worker ON that task (the instance name is derived from it).
      // Use task mode so the worker actually RUNS the task instead of opening an
      // idle dashboard. Explicit `rudder cloud sail <name>` keeps name-only mode.
      await launch(command === "sail" ? args : [subcommand, ...rest], options, "task");
      return;
  }
}

async function login(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: false });
  const failures: string[] = [];
  const browserLogin = await tryBrowserLogin(client, options).catch((error) => {
    failures.push(`browser login: ${error instanceof Error ? error.message : String(error)}`);
    if (!options.json) {
      console.warn(`Browser login unavailable: ${error instanceof Error ? error.message : String(error)}`);
      console.warn("Trying local GitHub auth fallback...");
    }
    return null;
  });
  if (browserLogin?.token || browserLogin?.accessToken) {
    const token = browserLogin.token ?? browserLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, browserLogin, token, options, "browser");
      return;
    }
  }

  const githubLogin = await tryGithubCliLogin(client).catch((error) => {
    failures.push(`GitHub CLI login: ${error instanceof Error ? error.message : String(error)}`);
    return null;
  });
  if (githubLogin?.token || githubLogin?.accessToken) {
    const token = githubLogin.token ?? githubLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, githubLogin, token, options, "GitHub CLI");
      return;
    }
  }

  const githubDeviceLogin = await tryGithubDeviceLogin(client, options).catch((error) => {
    failures.push(`GitHub device login: ${error instanceof Error ? error.message : String(error)}`);
    return null;
  });
  if (githubDeviceLogin?.token || githubDeviceLogin?.accessToken) {
    const token = githubDeviceLogin.token ?? githubDeviceLogin.accessToken;
    if (token) {
      await saveCloudLogin(client, githubDeviceLogin, token, options, "GitHub device");
      return;
    }
  }

  const detail = failures.length > 0 ? ` (${failures.join("; ")})` : "";
  throw new Error(`Could not log in to Rudder Cloud${detail}.`);
}

async function tryBrowserLogin(client: CloudClient, options: CloudCommandOptions): Promise<LoginPollResponse | null> {
  const response = await client.request<LoginStartResponse>("/api/cli/login", {
    method: "POST",
    body: {
      deviceName: os.hostname(),
      client: "rudder",
    },
  });
  const deviceCode = response.deviceCode;
  const loginUrl = response.loginUrl ?? response.verificationUri ?? withQuery(client.baseUrl, "/cli/login", deviceCode ? { device_code: deviceCode } : {});
  const pollPath = response.pollUrl ?? "/api/cli/login/poll";
  const intervalMs = Math.max(1000, (response.interval ?? DEFAULT_LOGIN_INTERVAL_MS / 1000) * 1000);
  const timeoutMs = Math.max(intervalMs, (response.expiresIn ?? DEFAULT_LOGIN_TIMEOUT_MS / 1000) * 1000);

  loginProgress(options, `Opening ${loginUrl}`);
  if (!options.json) {
    openBrowser(loginUrl);
  }
  loginProgress(options, "Waiting for browser login to complete...");

  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    await sleep(intervalMs);
    const poll = await pollLogin(client, pollPath, deviceCode);
    const token = poll.token ?? poll.accessToken;
    if (token) {
      return poll;
    }
    if (poll.pending === false) {
      throw new Error("Cloud login was not approved.");
    }
  }
  throw new Error("Timed out waiting for cloud login.");
}

async function tryGithubCliLogin(client: CloudClient): Promise<LoginPollResponse | null> {
  if (process.env.RUDDER_SKIP_GH_CLI === "1") {
    return null;
  }
  const gh = await runCommand("gh", ["auth", "token"], { allowFailure: true });
  const token = gh.stdout.trim();
  if (gh.code !== 0 || !token) {
    return null;
  }
  return await client.request<LoginPollResponse>("/api/cli/login/github-token", {
    method: "POST",
    body: { token },
  });
}

async function tryGithubDeviceLogin(client: CloudClient, options: CloudCommandOptions): Promise<LoginPollResponse | null> {
  if (process.env.RUDDER_SKIP_GITHUB_DEVICE_LOGIN === "1") {
    return null;
  }
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
    client_id: GITHUB_CLI_CLIENT_ID,
    scope: "read:user user:email",
  });
  if (!start.device_code || !start.user_code || !start.verification_uri) {
    return null;
  }
  if (!options.json) {
    const url = start.verification_uri_complete ?? start.verification_uri;
    console.log(`Opening ${url}`);
    console.log(`GitHub code: ${start.user_code}`);
    openBrowser(url);
  }

  const intervalMs = Math.max(1000, (start.interval ?? 5) * 1000);
  const timeoutMs = Math.max(intervalMs, (start.expires_in ?? 900) * 1000);
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    await sleep(intervalMs);
    const poll = await githubOAuthRequest<{
      access_token?: string;
      token_type?: string;
      scope?: string;
      error?: string;
      error_description?: string;
      interval?: number;
    }>("https://github.com/login/oauth/access_token", {
      client_id: GITHUB_CLI_CLIENT_ID,
      device_code: start.device_code,
      grant_type: "urn:ietf:params:oauth:grant-type:device_code",
    });
    if (poll.access_token) {
      return await client.request<LoginPollResponse>("/api/cli/login/github-token", {
        method: "POST",
        body: { token: poll.access_token },
      });
    }
    if (poll.error === "authorization_pending") {
      continue;
    }
    if (poll.error === "slow_down") {
      await sleep(Math.max(intervalMs, (poll.interval ?? 5) * 1000));
      continue;
    }
    return null;
  }
  return null;
}

async function githubOAuthRequest<T>(url: string, body: Record<string, string>): Promise<T> {
  const { response, text } = await fetchTextWithTimeout(
    url,
    {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    },
    cloudRequestTimeoutMs(),
    "GitHub OAuth request",
    1024 * 1024,
  );
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) || text.trim() || `${response.status} ${response.statusText}`);
  }
  return parsed as T;
}

function loginProgress(options: CloudCommandOptions, message: string): void {
  if (options.json) {
    process.stderr.write(`${message}\n`);
    return;
  }
  console.log(message);
}

async function saveCloudLogin(
  client: CloudClient,
  login: LoginPollResponse,
  token: string,
  options: CloudCommandOptions,
  source: string,
): Promise<void> {
  const previous = await loadCloudAuth();
  const previousRuntime = previous?.cloudUrl === client.baseUrl ? parseCloudRuntime(previous.defaultRuntime) : undefined;
  const previousByocHost = previous?.cloudUrl === client.baseUrl ? previous.byocSshHost : undefined;
  await saveCloudAuth({
    version: 1,
    token,
    cloudUrl: client.baseUrl,
    defaultRuntime: previousRuntime,
    byocSshHost: previousByocHost,
    accountId: login.accountId,
    email: login.email,
    expiresAt: login.expiresAt ?? (login.expiresIn ? new Date(Date.now() + login.expiresIn * 1000).toISOString() : undefined),
    updatedAt: nowIso(),
  });
  if (options.json) {
    const result: Record<string, JsonValue> = { ok: true, cloudUrl: client.baseUrl, source };
    if (login.email) {
      result.email = login.email;
    }
    if (login.accountId) {
      result.accountId = login.accountId;
    }
    printJson(result);
  } else {
    console.log(`Logged in to ${client.baseUrl}${login.email ? ` as ${login.email}` : ""} via ${source}.`);
  }
}

async function launch(
  args: string[],
  options: CloudCommandOptions,
  mode: "name" | "task" = "name",
  explicitRuntime?: CloudRuntime,
): Promise<void> {
  const raw = args.join(" ").trim();
  const repoRoot = findRepoRoot();
  // Validate auth/runtime before doing the expensive filesystem walk, copy,
  // and compression work. An unauthenticated launch should fail immediately.
  const client = await cloudClient({ requireToken: true });
  const runtime = await selectedCloudRuntime(explicitRuntime);
  const snapshot = await createSnapshot(repoRoot, options.homePaths ?? []);
  try {
    const task = mode === "task" || runtime === "byo-vm" ? raw : "";
    const name = task ? cloudNameFromTask(task) : raw || randomCloudName();
    const body: Record<string, JsonValue> = {
      repoName: path.basename(repoRoot),
      name,
      snapshot: await snapshotPayload(snapshot),
    };
    if (runtime !== "fly") {
      body.runtime = runtime;
    }
    if (task) {
      body.task = task;
    }
    const result = await client.request<JsonValue>("/api/rudder/sail/launch", {
      method: "POST",
      body,
      timeoutMs: cloudUploadTimeoutMs(),
    });
    await printResult(result, options);
    await maybeAutoAttach(result, options);
  } finally {
    await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
  }
}

async function onload(args: string[], options: CloudCommandOptions): Promise<void> {
  const runId = args[0];
  if (runId && (runId === "." || runId === ".." || runId.includes("/") || runId.includes("\\") || runId.includes("\0"))) {
    throw new Error("Invalid run id. Usage: rudder cloud onload [runId]");
  }
  const repoRoot = findRepoRoot();
  const runRecord = runId
    ? await readJson<JsonValue>(path.join(repoRoot, ".rudder", "runs", runId, "run.json"))
    : null;
  if (runId && !runRecord) {
    throw new Error(`Local Rudder run not found: ${runId}`);
  }
  const worktreePath = runRecord && typeof runRecord === "object" && !Array.isArray(runRecord)
    ? (runRecord as Record<string, JsonValue>).worktree
    : undefined;
  const sourceRoot = worktreePath && typeof worktreePath === "object" && !Array.isArray(worktreePath)
    ? ((worktreePath as Record<string, JsonValue>).path as string | undefined)
    : undefined;
  const snapshotRoot = sourceRoot && await pathExists(sourceRoot) ? sourceRoot : repoRoot;
  const client = await cloudClient({ requireToken: true });
  const runtime = await selectedCloudRuntime();
  const snapshot = await createSnapshot(snapshotRoot, options.homePaths ?? [], { includeRudderState: !runId });
  try {
    const name = runId ? undefined : `workspace-${path.basename(repoRoot)}`;
    const body: Record<string, JsonValue> = {
      repoName: path.basename(repoRoot),
      run: runRecord ?? null,
      workspace: !runId,
      snapshot: await snapshotPayload(snapshot),
    };
    if (runId) {
      body.runId = runId;
    } else {
      body.name = name ?? `workspace-${path.basename(repoRoot)}`;
    }
    if (runtime !== "fly") {
      body.runtime = runtime;
    }
    const result = await client.request<JsonValue>("/api/rudder/sail/onload", {
      method: "POST",
      body,
      timeoutMs: cloudUploadTimeoutMs(),
    });
    await printResult(result, options);
    await maybeAutoAttach(result, options);
  } finally {
    await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
  }
}

async function logs(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Usage: rudder cloud logs <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<Record<string, JsonValue>>(
    `/api/rudder/sail/${encodeURIComponent(sailId)}/output`,
    { method: "GET" },
  );
  if (options.json) {
    printJson(result);
    return;
  }
  const connected = result.connected === true ? "connected" : "disconnected";
  console.log(`${sailId}  ${connected}`);
  const output = typeof result.output === "string" ? stripAnsiForCli(result.output).trimEnd() : "";
  console.log(output || "(no output yet)");
}

async function listSails(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>("/api/rudder/sail", { method: "GET" });
  await printResult(result, options);
}

async function status(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  const runtime = await selectedCloudRuntime();
  const sails = await client.request<JsonValue>("/api/rudder/sail", { method: "GET" });
  const sailRows = Array.isArray(sails)
    ? sails
    : sails && typeof sails === "object" && !Array.isArray(sails) && Array.isArray((sails as Record<string, JsonValue>).sails)
      ? (sails as Record<string, JsonValue>).sails as JsonValue[]
      : [];
  const sailCount = sailRows.length;
  const result: Record<string, JsonValue> = {
    ok: true,
    cloudUrl: client.baseUrl,
    runtime,
    sails: sailCount,
  };
  if (state?.cloudUrl === client.baseUrl) {
    if (state.email) {
      result.email = state.email;
    }
    if (state.accountId) {
      result.accountId = state.accountId;
    }
    if (state.byocSshHost) {
      result.byocSshHost = state.byocSshHost;
    }
  }
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(`Logged in to ${client.baseUrl}${state?.email ? ` as ${state.email}` : ""}.`);
  console.log(`Runtime: ${runtime}`);
  if (state?.byocSshHost) {
    console.log(`BYOC SSH host: ${state.byocSshHost}`);
  }
  console.log(`Cloud workers: ${sailCount}`);
}

async function bootstrap(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Missing sail id. Usage: rudder cloud bootstrap <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/sail/${encodeURIComponent(sailId)}/bootstrap`, {
    method: "POST",
    body: {},
  });
  await printResult(result, options);
}

async function mutateSail(
  action: "onload" | "pause" | "resume" | "stop" | "delete",
  args: string[],
  options: CloudCommandOptions,
): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error(`Missing sail id. Usage: rudder sail ${action} <id>`);
  }
  const client = await cloudClient({ requireToken: true });
  let result: JsonValue;
  try {
    result = await client.request<JsonValue>(`/api/rudder/sail/${encodeURIComponent(sailId)}/${action}`, {
      method: "POST",
      body: args.length > 1 ? { args: args.slice(1) } : {},
    });
  } catch (error) {
    if (action !== "onload" && isSailNotFound(error)) {
      await workspaceMutate(action, args, options);
      return;
    }
    throw error;
  }
  await printResult(result, options);
}

function isSailNotFound(error: unknown): boolean {
  if (error instanceof CloudRequestError && error.status === 404) {
    return true;
  }
  const message = error instanceof Error ? error.message : String(error);
  return /sail not found/i.test(message);
}

async function talk(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  const message = args.slice(1).join(" ").trim();
  if (!sailId || !message) {
    throw new Error('Usage: rudder cloud talk <id> "<message>"');
  }
  const client = await cloudClient({ requireToken: true });
  let result: JsonValue;
  try {
    result = await client.request<JsonValue>(
      `/api/rudder/sail/${encodeURIComponent(sailId)}/input`,
      { method: "POST", body: { text: message } },
    );
  } catch (error) {
    // A disconnected worker is a normal relay state, not a malformed request.
    // The server returns 409 with a useful `{delivered:false}` payload; preserve
    // the command's friendly output instead of turning that into a stack-level
    // request failure.
    if (error instanceof CloudRequestError && error.status === 409 && error.payload) {
      result = error.payload;
    } else {
      throw error;
    }
  }
  if (options.json) {
    printJson(result);
    return;
  }
  const delivered = result && typeof result === "object" && !Array.isArray(result)
    ? (result as Record<string, JsonValue>).delivered === true
    : false;
  if (!delivered) {
    console.log(`Could not reach ${sailId}: it is not connected right now.`);
    return;
  }
  console.log(`→ ${sailId}: ${message}`);
  // Give the agent a moment, then show what it said back.
  await sleep(3500);
  await tailOutput([sailId], options);
}

async function tailOutput(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Usage: rudder cloud output <id>");
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<Record<string, JsonValue>>(
    `/api/rudder/sail/${encodeURIComponent(sailId)}/output`,
    { method: "GET" },
  );
  if (options.json) {
    printJson(result);
    return;
  }
  const output = typeof result.output === "string" ? result.output : "";
  const cleaned = stripAnsiForCli(output).trimEnd();
  const lines = cleaned ? cleaned.split("\n").slice(-30).join("\n") : "(no output yet)";
  console.log(lines);
}

function stripAnsiForCli(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[@-Z\\-_=>()#][0-9;]*/g, "")
    .replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
}

async function quickstart(options: CloudCommandOptions): Promise<void> {
  const url = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL) || DEFAULT_CLOUD_URL;
  if (options.json) {
    printJson({ cloudUrl: url, steps: ["install", "login", "launch", "talk", "slack"] });
    return;
  }
  const lines = [
    "Rudder Cloud — copy/paste quickstart",
    "",
    "1. Install Rudder:",
    "   npm install -g @viraatdas/rudder@latest",
    "",
    "2. Log in to the cloud control plane:",
    "   rudder login",
    "",
    "3. Launch a fresh cloud instance on any task (run inside a repo):",
    '   rudder cloud "fix the failing tests"',
    "",
    "4. See your instances and talk to any of them:",
    "   rudder cloud list",
    '   rudder cloud talk <id> "what are you working on?"',
    "   rudder cloud output <id>",
    "   rudder cloud attach <id>      # full interactive terminal",
    "",
    "5. Talk to every instance from Slack (one thread per instance):",
    "   rudder cloud slack            # prints the Slack setup",
    "",
    `Control plane: ${url}`,
  ];
  console.log(lines.join("\n"));
}

async function slackSetup(args: string[], options: CloudCommandOptions): Promise<void> {
  const url = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL) || DEFAULT_CLOUD_URL;
  const eventsUrl = `${url.replace(/\/$/, "")}/api/slack/events`;
  const channel = process.env.RUDDER_SLACK_CHANNEL || "C0B78TDLM5G";
  const manifest = {
    display_information: { name: "Rudder Cloud" },
    features: {
      bot_user: { display_name: "rudder", always_online: true },
    },
    oauth_config: {
      scopes: { bot: ["app_mentions:read", "channels:history", "groups:history", "chat:write"] },
    },
    settings: {
      event_subscriptions: {
        request_url: eventsUrl,
        bot_events: ["app_mention", "message.channels", "message.groups"],
      },
      org_deploy_enabled: false,
      socket_mode_enabled: false,
    },
  };
  if (args[0] === "manifest") {
    console.log(JSON.stringify(manifest, null, 2));
    return;
  }
  if (options.json) {
    printJson({ eventsUrl, channel, manifest });
    return;
  }
  const lines = [
    "Rudder Cloud → Slack setup",
    "",
    `Channel: ${channel}`,
    `Events request URL: ${eventsUrl}`,
    "",
    "1. Create a Slack app from the manifest (api.slack.com/apps → Create New App → From a manifest):",
    "   rudder cloud slack manifest    # prints the JSON manifest to paste",
    "",
    "2. Install the app to your workspace and invite it to the channel:",
    `   /invite @rudder   (in ${channel})`,
    "",
    "3. Set these on the control plane (Fly secrets), then redeploy:",
    "   SLACK_BOT_TOKEN=xoxb-...        # Bot User OAuth Token",
    "   SLACK_SIGNING_SECRET=...        # Basic Information → Signing Secret",
    `   RUDDER_SLACK_CHANNEL=${channel}`,
    "",
    "   flyctl secrets set SLACK_BOT_TOKEN=xoxb-... SLACK_SIGNING_SECRET=... \\",
    `     RUDDER_SLACK_CHANNEL=${channel} -a rudder-cloud-control`,
    "",
    "That's it. Every `rudder cloud \"<task>\"` now opens a thread in the channel;",
    "reply in a thread to talk to that instance, or use `list` / `talk <id> <msg>`.",
  ];
  console.log(lines.join("\n"));
}

async function setupOAuthProvider(
  provider: "github" | "google",
  args: string[],
  options: CloudCommandOptions,
): Promise<void> {
  const envPrefix = provider === "github" ? "RUDDER_GITHUB" : "RUDDER_GOOGLE";
  const clientId = args[0]?.trim() || process.env[`${envPrefix}_CLIENT_ID`]?.trim();
  const clientSecret =
    process.env[`${envPrefix}_CLIENT_SECRET`]?.trim() ||
    args[1]?.trim() ||
    await promptSecret(`${provider === "github" ? "GitHub App" : "Google OAuth"} client secret`);
  if (!clientId || !clientSecret) {
    throw new Error([
      `Missing ${provider === "github" ? "GitHub" : "Google"} OAuth credentials.`,
      `Usage: rudder cloud setup-${provider} <client-id>`,
      `Or set ${envPrefix}_CLIENT_ID and ${envPrefix}_CLIENT_SECRET.`,
    ].join("\n"));
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/setup/${provider}`, {
    method: "POST",
    body: {
      clientId,
      clientSecret,
    },
  });
  await printResult(result, options);
}

async function setupByoc(args: string[], options: CloudCommandOptions): Promise<void> {
  const sshConfigPath = path.join(os.homedir(), ".ssh", "config");
  const configuredHosts = await listSshConfigHosts(sshConfigPath);
  const host = (options.sshHost ?? args.join(" ").trim()) || await chooseByocHost(configuredHosts);
  if (!host) {
    throw new Error([
      "Missing BYOC SSH host.",
      "Add your workstation/server to ~/.ssh/config, then run:",
      "",
      "  rudder cloud byoc <ssh-host>",
      "",
      "Example ~/.ssh/config:",
      "  Host rudder-workstation",
      "    HostName 203.0.113.10",
      "    User ubuntu",
      "    IdentityFile ~/.ssh/id_ed25519",
      "",
      configuredHosts.length
        ? `Detected SSH hosts: ${configuredHosts.slice(0, 12).join(", ")}`
        : `No usable hosts found in ${shortenHome(sshConfigPath)}.`,
    ].join("\n"));
  }

  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  if (!state || state.cloudUrl !== client.baseUrl) {
    throw new Error("Not logged in to this Rudder Cloud control plane. Run `rudder login` first.");
  }
  const [configMentionsHost, diagnostics] = await Promise.all([
    configuredHosts.includes(host) ? true : sshConfigMentions(sshConfigPath, host),
    checkByocHost(host),
  ]);
  await saveCloudAuth({
    ...state,
    defaultRuntime: state.defaultRuntime === "byo-vm" ? "fly" : state.defaultRuntime,
    byocSshHost: host,
    updatedAt: nowIso(),
  });

  if (options.json) {
    const result: Record<string, JsonValue> = {
      ok: true,
      cloudUrl: client.baseUrl,
      byocSshHost: host,
    };
    const defaultRuntime = state.defaultRuntime === "byo-vm" ? "fly" : state.defaultRuntime;
    if (defaultRuntime) {
      result.defaultRuntime = defaultRuntime;
    }
    printJson(result);
    return;
  }

  console.log(`Rudder BYOC host set to ${host}.`);
  console.log("Plain `rudder cloud` and dashboard `/cloud` continue to use Fly by default.");
  console.log("Use `rudder cloud vm <task>` when you want to run a task on this BYOC host.");

  if (!configMentionsHost) {
    console.log(`\nNote: ${shortenHome(sshConfigPath)} does not appear to define Host ${host}.`);
    console.log("Rudder can still use it if SSH resolves it, but a ~/.ssh/config entry is recommended:");
    console.log(`  Host ${host}`);
    console.log("    HostName <server-ip-or-dns>");
    console.log("    User <user>");
    console.log("    IdentityFile ~/.ssh/<private-key>");
  }
  if (diagnostics.ok) {
    console.log(`SSH check passed for ${host}. Docker is available on the BYOC host.`);
  } else {
    console.log(`\nSSH check did not fully pass for ${host}: ${diagnostics.message}`);
    console.log("Fix SSH/Docker before launching, or run the printed Docker command manually on that host.");
  }
}

async function chooseByocHost(hosts: string[]): Promise<string> {
  if (hosts.length === 0) {
    return await promptText("SSH host from ~/.ssh/config");
  }
  return await promptSelect(
    "Choose a BYOC SSH host from ~/.ssh/config",
    hosts.slice(0, 24).map((host) => ({ value: host, label: host })),
    hosts[0],
  );
}

async function configureDefaultRuntime(runtime: CloudRuntime, options: CloudCommandOptions, byocSshHost?: string): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const state = await loadCloudAuth();
  if (!state || state.cloudUrl !== client.baseUrl) {
    throw new Error("Not logged in to this Rudder Cloud control plane. Run `rudder login` first.");
  }
  await saveCloudAuth({
    ...state,
    defaultRuntime: runtime,
    byocSshHost: runtime === "byo-vm" ? byocSshHost ?? state.byocSshHost : undefined,
    updatedAt: nowIso(),
  });
  const result: Record<string, JsonValue> = {
    ok: true,
    cloudUrl: client.baseUrl,
    defaultRuntime: runtime,
  };
  const savedByocHost = byocSshHost ?? state.byocSshHost;
  if (runtime === "byo-vm" && savedByocHost) {
    result.byocSshHost = savedByocHost;
  }
  const envRuntime = envCloudRuntime();
  if (envRuntime) {
    result.envOverride = envRuntime;
  }
  if (options.json) {
    printJson(result);
    return;
  }
  console.log(`Rudder Cloud runtime set to ${runtime}.`);
  if (runtime === "byo-vm") {
    const host = byocSshHost ?? state.byocSshHost;
    console.log("Future `rudder cloud <task>` and `/sail <task>` launches will prepare a BYOC worker instead of creating a Fly Machine.");
    console.log(host
      ? `Rudder will try to start the worker over SSH on ${host}.`
      : "Run `rudder cloud byoc <ssh-host>` to let Rudder start workers over SSH.");
  } else {
    console.log("Future `rudder cloud <task>` and `/sail <task>` launches will create Fly Machines.");
  }
  if (envRuntime) {
    console.log(`RUDDER_CLOUD_RUNTIME=${envRuntime} is set and will override this saved default.`);
  }
}

async function runtime(args: string[], options: CloudCommandOptions): Promise<void> {
  const next = args[0] ? parseCloudRuntime(args[0]) : undefined;
  if (args[0] && !next) {
    throw new Error("Runtime must be `fly`, `byoc`, or `byo-vm`.");
  }
  if (next) {
    await configureDefaultRuntime(next, options);
    return;
  }
  const client = await cloudClient({ requireToken: true });
  const current = await selectedCloudRuntime();
  const state = await loadCloudAuth();
  const savedRuntime = parseCloudRuntime(state?.defaultRuntime);
  const result: Record<string, JsonValue> = {
    cloudUrl: client.baseUrl,
    runtime: current,
  };
  const envRuntime = envCloudRuntime();
  if (state?.cloudUrl === client.baseUrl && savedRuntime) {
    result.savedDefaultRuntime = savedRuntime;
  }
  if (state?.cloudUrl === client.baseUrl && state.byocSshHost) {
    result.byocSshHost = state.byocSshHost;
  }
  if (envRuntime) {
    result.envOverride = envRuntime;
  }
  if (options.json) {
    printJson(result);
  } else {
    console.log(`Rudder Cloud runtime: ${current}`);
    if (envRuntime) {
      console.log(`Set by RUDDER_CLOUD_RUNTIME=${envRuntime}.`);
    } else if (state?.cloudUrl === client.baseUrl && savedRuntime) {
      console.log("Set in local Rudder Cloud config.");
    } else {
      console.log("Using default Fly Machines runtime.");
    }
    if (state?.cloudUrl === client.baseUrl && state.byocSshHost) {
      console.log(`BYOC SSH host: ${state.byocSshHost}`);
    }
  }
}

async function sshConfigMentions(configPath: string, host: string): Promise<boolean> {
  const text = await fsp.readFile(configPath, "utf8").catch(() => "");
  if (!text.trim()) {
    return false;
  }
  const target = host.toLowerCase();
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = /^Host\s+(.+)$/i.exec(trimmed);
    if (!match) {
      continue;
    }
    const patterns = match[1].split(/\s+/).map((part) => part.toLowerCase());
    if (patterns.includes(target)) {
      return true;
    }
  }
  return false;
}

async function listSshConfigHosts(configPath: string): Promise<string[]> {
  const text = await fsp.readFile(configPath, "utf8").catch(() => "");
  const hosts: string[] = [];
  const seen = new Set<string>();
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = /^Host\s+(.+)$/i.exec(trimmed);
    if (!match) {
      continue;
    }
    for (const host of match[1].split(/\s+/)) {
      if (!host || host.includes("*") || host.includes("?") || host.startsWith("!")) {
        continue;
      }
      if (seen.has(host)) {
        continue;
      }
      seen.add(host);
      hosts.push(host);
    }
  }
  return hosts;
}

async function checkByocHost(host: string): Promise<{ ok: boolean; message: string }> {
  if (!commandExists("ssh")) {
    return { ok: false, message: "ssh is not installed or not on PATH" };
  }
  const result = await runCommandWithDeadline("ssh", [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=8",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=2",
    host,
    "command -v docker >/dev/null && docker info >/dev/null 2>&1",
  ], 20_000, { allowFailure: true });
  if (result.code === 0) {
    return { ok: true, message: "ok" };
  }
  const detail = (result.stderr || result.stdout || `ssh exited ${result.code}`).trim();
  return { ok: false, message: detail };
}

async function startByocWorkerOverSsh(host: string, bootstrapCommand: string): Promise<void> {
  if (!commandExists("ssh")) {
    throw new Error("ssh is not installed or not on PATH");
  }
  const remoteCommand = [
    "mkdir -p ~/.rudder/byoc",
    `nohup sh -lc ${shellQuote(nonInteractiveDockerCommand(bootstrapCommand))} > ~/.rudder/byoc/worker.log 2>&1 < /dev/null &`,
  ].join(" && ");
  await runCommandWithDeadline("ssh", [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=2",
    host,
    remoteCommand,
  ], 30_000);
}

async function runCommandWithDeadline(
  command: string,
  args: string[],
  timeoutMs: number,
  options: { allowFailure?: boolean } = {},
): Promise<{ stdout: string; stderr: string; code: number }> {
  return await new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      env: commandEnv(),
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let forceKillTimer: NodeJS.Timeout | undefined;
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => { stdout += chunk; });
    child.stderr.on("data", (chunk: string) => { stderr += chunk; });
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      forceKillTimer = setTimeout(() => child.kill("SIGKILL"), 750);
      forceKillTimer.unref?.();
    }, timeoutMs);
    timer.unref?.();
    child.once("error", (error) => {
      clearTimeout(timer);
      if (forceKillTimer) clearTimeout(forceKillTimer);
      reject(error);
    });
    child.once("close", (code) => {
      clearTimeout(timer);
      if (forceKillTimer) clearTimeout(forceKillTimer);
      const exitCode = timedOut ? 124 : code ?? 1;
      if (timedOut) {
        stderr = `${stderr.trim()}${stderr.trim() ? "\n" : ""}${command} timed out after ${Math.ceil(timeoutMs / 1000)}s`;
      }
      if (exitCode !== 0 && !options.allowFailure) {
        // Do not echo argv: the BYOC remote command contains the short-lived
        // worker credential and snapshot URL.
        reject(new Error(`${command} failed: ${stderr.trim() || stdout.trim()}`));
        return;
      }
      resolve({ stdout, stderr, code: exitCode });
    });
  });
}

function nonInteractiveDockerCommand(command: string): string {
  return command
    .replace(/\bdocker run --rm -it\b/g, "docker run --rm")
    .replace(/\bdocker run --rm -i -t\b/g, "docker run --rm")
    .replace(/\bdocker run --rm -t -i\b/g, "docker run --rm");
}

async function cloudClient(options: { requireToken: boolean }): Promise<CloudClient> {
  const baseUrl = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL);
  const state = await loadCloudAuth();
  const envToken = process.env.RUDDER_CLOUD_TOKEN?.trim();
  const token = envToken || (state?.cloudUrl === baseUrl ? state.token.trim() : undefined);
  if (options.requireToken && !token) {
    throw new Error("Not logged in to Rudder Cloud. Run `rudder login` first.");
  }
  return {
    baseUrl,
    async request<T>(pathOrUrl: string, init: { method: string; body?: JsonValue; timeoutMs?: number }) {
      const url = pathOrUrl.startsWith("http://") || pathOrUrl.startsWith("https://")
        ? new URL(pathOrUrl)
        : new URL(pathOrUrl, `${baseUrl}/`);
      if (url.origin !== new URL(baseUrl).origin) {
        throw new Error(`Refusing a cross-origin Rudder Cloud API URL: ${url.origin}`);
      }
      const headers: Record<string, string> = {
        Accept: "application/json",
      };
      let body: string | undefined;
      if (init.body !== undefined) {
        headers["Content-Type"] = "application/json";
        body = JSON.stringify(init.body);
      }
      if (token) {
        headers.Authorization = `Bearer ${token}`;
      }
      let response: Response;
      let text: string;
      try {
        ({ response, text } = await fetchTextWithTimeout(
          url,
          {
            method: init.method,
            headers,
            body,
          },
          init.timeoutMs ?? cloudRequestTimeoutMs(),
          "Rudder Cloud request",
          8 * 1024 * 1024,
        ));
      } catch (error) {
        throw new Error(`Rudder Cloud request failed: ${error instanceof Error ? error.message : String(error)}`);
      }
      const parsed = text ? parseJson(text) : null;
      if (!response.ok) {
        const message = responseErrorMessage(parsed) || text.trim() || `${response.status} ${response.statusText}`;
        throw new CloudRequestError(
          response.status,
          `Rudder Cloud request failed (${response.status}): ${message.slice(0, 2_000)}`,
          parsed,
        );
      }
      if (text && !response.headers.get("content-type")?.toLowerCase().includes("json")) {
        throw new Error(`Rudder Cloud returned a non-JSON response (${response.status}).`);
      }
      if (text && typeof parsed === "string" && parsed === text) {
        throw new Error(`Rudder Cloud returned invalid JSON (${response.status}).`);
      }
      return parsed as T;
    },
  };
}

function normalizeCloudUrl(raw: string | undefined): string {
  const value = raw?.trim() || DEFAULT_CLOUD_URL;
  if (!value) {
    throw new Error("RUDDER_CLOUD_URL is not configured. Set it to your Rudder Cloud control plane URL.");
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error("RUDDER_CLOUD_URL must be a valid http(s) URL.");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("RUDDER_CLOUD_URL must be a valid http(s) URL.");
  }
  if (url.username || url.password) {
    throw new Error("RUDDER_CLOUD_URL must not contain embedded credentials.");
  }
  // The control plane hands back a shell command (bootstrapCommand) that the client
  // can auto-run over SSH, so a plain-HTTP control plane is a MITM-to-RCE risk.
  // Require https except for loopback dev or an explicit opt-in.
  const isLoopback = url.hostname === "127.0.0.1"
    || url.hostname === "localhost"
    || url.hostname === "[::1]";
  if (url.protocol === "http:" && !isLoopback && process.env.RUDDER_CLOUD_ALLOW_HTTP !== "1") {
    throw new Error(
      "Refusing a plain-HTTP RUDDER_CLOUD_URL (it can be MITM'd into running arbitrary commands). " +
        "Use https://, or set RUDDER_CLOUD_ALLOW_HTTP=1 to override for a trusted local network.",
    );
  }
  url.hash = "";
  url.search = "";
  return url.toString().replace(/\/$/, "");
}

function timeoutFromEnv(name: string, fallback: number, maximum: number): number {
  const raw = process.env[name]?.trim();
  if (!raw) {
    return fallback;
  }
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 100 || parsed > maximum) {
    throw new Error(`${name} must be a number of milliseconds between 100 and ${maximum}.`);
  }
  return Math.floor(parsed);
}

function cloudRequestTimeoutMs(): number {
  return timeoutFromEnv("RUDDER_CLOUD_REQUEST_TIMEOUT_MS", DEFAULT_CLOUD_REQUEST_TIMEOUT_MS, 10 * 60_000);
}

function cloudUploadTimeoutMs(): number {
  return timeoutFromEnv("RUDDER_CLOUD_UPLOAD_TIMEOUT_MS", DEFAULT_CLOUD_UPLOAD_TIMEOUT_MS, 30 * 60_000);
}

function attachConnectTimeoutMs(): number {
  return timeoutFromEnv("RUDDER_CLOUD_ATTACH_TIMEOUT_MS", DEFAULT_ATTACH_CONNECT_TIMEOUT_MS, 10 * 60_000);
}

function attachReadyTimeoutMs(): number {
  return timeoutFromEnv("RUDDER_CLOUD_READY_TIMEOUT_MS", DEFAULT_ATTACH_READY_TIMEOUT_MS, 30 * 60_000);
}

async function fetchWithTimeout(
  url: string | URL,
  init: RequestInit,
  timeoutMs: number,
  label: string,
): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  timer.unref?.();
  try {
    return await fetch(url, { ...init, signal: controller.signal });
  } catch (error) {
    if (controller.signal.aborted) {
      throw new Error(`${label} timed out after ${Math.ceil(timeoutMs / 1000)}s`);
    }
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

async function fetchTextWithTimeout(
  url: string | URL,
  init: RequestInit,
  timeoutMs: number,
  label: string,
  maxBytes: number,
): Promise<{ response: Response; text: string }> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  timer.unref?.();
  try {
    const response = await fetch(url, { ...init, signal: controller.signal });
    const declared = Number(response.headers.get("content-length"));
    if (Number.isFinite(declared) && declared > maxBytes) {
      controller.abort();
      throw new Error(`${label} response exceeded ${Math.floor(maxBytes / 1024 / 1024)} MiB`);
    }
    if (!response.body) {
      return { response, text: "" };
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let total = 0;
    let text = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        text += decoder.decode();
        return { response, text };
      }
      total += value.byteLength;
      if (total > maxBytes) {
        controller.abort();
        throw new Error(`${label} response exceeded ${Math.floor(maxBytes / 1024 / 1024)} MiB`);
      }
      text += decoder.decode(value, { stream: true });
    }
  } catch (error) {
    if (controller.signal.aborted && !(error instanceof Error && /response exceeded/.test(error.message))) {
      throw new Error(`${label} timed out after ${Math.ceil(timeoutMs / 1000)}s`);
    }
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

async function selectedCloudRuntime(explicit?: CloudRuntime): Promise<CloudRuntime> {
  if (explicit) {
    return explicit;
  }
  const envRuntime = envCloudRuntime();
  if (envRuntime) {
    return envRuntime;
  }
  const baseUrl = normalizeCloudUrl(process.env.RUDDER_CLOUD_URL);
  const state = await loadCloudAuth();
  const savedRuntime = parseCloudRuntime(state?.defaultRuntime);
  return state?.cloudUrl === baseUrl && savedRuntime ? savedRuntime : "fly";
}

function parseCloudRuntime(raw: string | undefined): CloudRuntime | undefined {
  const value = raw?.trim().toLowerCase();
  if (!value) {
    return undefined;
  }
  if (value === "fly" || value === "fly-machine" || value === "fly-machines") {
    return "fly";
  }
  if (value === "byo" || value === "byoc" || value === "byo-vm" || value === "manual" || value === "self-hosted" || value === "vm") {
    return "byo-vm";
  }
  return undefined;
}

function envCloudRuntime(): CloudRuntime | undefined {
  const runtime = parseCloudRuntime(process.env.RUDDER_CLOUD_RUNTIME);
  if (process.env.RUDDER_CLOUD_RUNTIME?.trim() && !runtime) {
    throw new Error("RUDDER_CLOUD_RUNTIME must be `fly`, `byoc`, or `byo-vm`.");
  }
  return runtime;
}

async function pollLogin(
  client: CloudClient,
  pollPath: string,
  deviceCode: string | undefined,
): Promise<LoginPollResponse> {
  if (pollPath.startsWith("http://") || pollPath.startsWith("https://") || !deviceCode) {
    return await client.request<LoginPollResponse>(pollPath, { method: "GET" });
  }
  return await client.request<LoginPollResponse>(pollPath, {
    method: "POST",
    body: { deviceCode },
  });
}

async function loadCloudAuth(): Promise<CloudAuthState | null> {
  const state = await readJson<CloudAuthState>(cloudAuthPath());
  return state?.version === 1 && typeof state.token === "string" ? state : null;
}

async function saveCloudAuth(state: CloudAuthState): Promise<void> {
  await writeJson(cloudAuthPath(), state, { mode: 0o600 });
}

async function createSnapshot(
  repoRoot: string,
  requestedHomePaths: string[],
  options: SnapshotOptions = {},
): Promise<CreatedSnapshot> {
  const tempDir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-cloud-"));
  try {
    const stageDir = path.join(tempDir, "snapshot");
    const repoStage = path.join(stageDir, "repo");
    const homeStage = path.join(stageDir, "home");
    await ensureDir(repoStage);
    await copyRepoFiles(repoRoot, repoStage);
    const rudderState = options.includeRudderState ? await copyRudderState(repoRoot, repoStage) : undefined;

    const homePaths = normalizeHomePaths(requestedHomePaths);
    const homeCopyResults = await mapLimit(homePaths, 8, async (homePath) => ({
      homePath,
      copied: await copyHomePath(homePath, homeStage),
    }));
    const includedHomePaths = homeCopyResults
      .filter(({ copied }) => copied)
      .map(({ homePath }) => shortenHome(homePath));

    // On macOS, Claude Code stores its OAuth token in the Keychain rather than
    // ~/.claude/.credentials.json, so the home-paths copy above doesn't pick it
    // up. Extract it from the Keychain and stage it as a credentials file so
    // the cloud worker boots already logged in.
    if (await stageClaudeKeychainCredentials(homeStage)) {
      includedHomePaths.push("~/.claude/.credentials.json (keychain)");
    }

    const capturedEnv = captureCloudEnv();
    let capturedEnvCount = 0;
    if (Object.keys(capturedEnv).length > 0) {
      await ensureDir(path.join(stageDir, "env"));
      await writeJson(path.join(stageDir, "env", "cloud-env.json"), capturedEnv as unknown as JsonValue);
      capturedEnvCount = Object.keys(capturedEnv).length;
    }

    let migratedAgentsCount = 0;
    if (options.migration && options.migration.plan.migrated.length > 0) {
      const entries = await stageMigratedAgents(
        stageDir,
        repoRoot,
        options.migration.repoName,
        options.migration.plan.migrated,
      );
      const migrationManifest: MigrationSnapshotManifest = {
        version: 1,
        createdAt: nowIso(),
        agents: entries,
      };
      await writeJson(path.join(stageDir, "migration.json"), migrationManifest as unknown as JsonValue);
      migratedAgentsCount = entries.length;
    }

    const [branch, commit] = await Promise.all([
      currentBranch(repoRoot),
      currentCommit(repoRoot),
    ]);
    const manifest: SnapshotManifest = {
      version: 1,
      createdAt: nowIso(),
      repo: {
        root: path.basename(repoRoot),
        branch,
        commit,
      },
      homePaths: includedHomePaths,
      ...(rudderState ? { rudderState } : {}),
      ...(migratedAgentsCount > 0 ? { migratedAgents: migratedAgentsCount } : {}),
      ...(capturedEnvCount > 0 ? { capturedEnvVars: capturedEnvCount } : {}),
    };
    await writeJson(path.join(stageDir, "manifest.json"), manifest);

    const archivePath = path.join(tempDir, `${newRunId("cloud-snapshot")}.tgz`);
    await runCommand("tar", ["-czf", archivePath, "-C", stageDir, "."], { cwd: stageDir });
    return { tempDir, archivePath, manifest };
  } catch (error) {
    await fsp.rm(tempDir, { recursive: true, force: true }).catch(() => undefined);
    throw error;
  }
}

async function snapshotPayload(snapshot: CreatedSnapshot): Promise<JsonValue> {
  const stat = await fsp.stat(snapshot.archivePath);
  if (stat.size > MAX_SNAPSHOT_ARCHIVE_BYTES) {
    throw new Error(
      `Cloud snapshot is too large (${Math.ceil(stat.size / 1024 / 1024)} MiB; maximum ${Math.floor(MAX_SNAPSHOT_ARCHIVE_BYTES / 1024 / 1024)} MiB). `
        + "Remove large untracked files or add them to .gitignore before retrying.",
    );
  }
  return {
    name: path.basename(snapshot.archivePath),
    contentType: "application/gzip",
    base64: await fsp.readFile(snapshot.archivePath, "base64"),
    manifest: snapshot.manifest as unknown as JsonValue,
  };
}

const CLOUD_ENV_DEFAULT_NAMES = new Set([
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_AUTH_TOKEN",
  "OPENAI_API_KEY",
  "GOOGLE_API_KEY",
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
  "AWS_REGION",
  "AWS_DEFAULT_REGION",
  "AWS_PROFILE",
  "VERCEL_TOKEN",
  "VERCEL_ORG_ID",
  "VERCEL_PROJECT_ID",
  "NETLIFY_AUTH_TOKEN",
  "GITHUB_TOKEN",
  "GH_TOKEN",
  "GITLAB_TOKEN",
  "NPM_TOKEN",
  "CARGO_REGISTRY_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_TOKEN",
  "DATABASE_URL",
  "REDIS_URL",
  "POSTGRES_URL",
  "STRIPE_API_KEY",
  "STRIPE_SECRET_KEY",
  "SLACK_BOT_TOKEN",
  "DISCORD_TOKEN",
]);
const CLOUD_ENV_SUFFIX_PATTERNS = [/_API_KEY$/, /_AUTH_TOKEN$/, /_ACCESS_TOKEN$/, /_TOKEN$/, /_SECRET$/, /_SECRET_KEY$/];
const CLOUD_ENV_BLOCKLIST = new Set([
  // Things we explicitly do not want shipping
  "PATH",
  "HOME",
  "USER",
  "PWD",
  "SHELL",
  "TERM",
  "TMPDIR",
  "LOGNAME",
  "SHLVL",
  "OLDPWD",
  "DISPLAY",
  "EDITOR",
  "PAGER",
  "LANG",
  "LC_ALL",
  // Rudder-internal vars that are set by the worker itself
  "RUDDER_WORKSPACE_ID",
  "RUDDER_SAIL_ID",
  "RUDDER_WORKER_TOKEN",
  "RUDDER_CLOUD_URL",
  "RUDDER_SNAPSHOT_URL",
  "RUDDER_CLOUD_TOKEN",
  "RUDDER_TASK",
  "RUDDER_REPO_NAME",
  "RUDDER_ACCOUNT_ID",
  "RUDDER_HANDOFF_PATH",
]);

function captureCloudEnv(): Record<string, string> {
  const extra = (process.env.RUDDER_CLOUD_ENV_VARS || "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  const blockExtra = (process.env.RUDDER_CLOUD_ENV_BLOCKLIST || "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  const blocked = new Set([...CLOUD_ENV_BLOCKLIST, ...blockExtra]);
  const out: Record<string, string> = {};
  for (const [name, value] of Object.entries(process.env)) {
    if (!name || typeof value !== "string" || !value) {
      continue;
    }
    if (blocked.has(name)) {
      continue;
    }
    const matches = CLOUD_ENV_DEFAULT_NAMES.has(name)
      || CLOUD_ENV_SUFFIX_PATTERNS.some((pattern) => pattern.test(name))
      || extra.includes(name);
    if (!matches) {
      continue;
    }
    out[name] = value;
  }
  return out;
}

async function stageMigratedAgents(
  stageDir: string,
  repoRoot: string,
  repoName: string,
  migrated: MigrationCandidate[],
): Promise<MigrationManifestEntry[]> {
  const worktreesStage = path.join(stageDir, "migrated-worktrees");
  const sessionsStage = path.join(stageDir, "migrated-sessions");
  const entries: MigrationManifestEntry[] = [];
  for (const candidate of migrated) {
    if (!(await pathExists(candidate.worktreePath))) {
      continue;
    }
    const hasSession = Boolean(
      candidate.sessionId
        && candidate.sessionJsonlPath
        && (await pathExists(candidate.sessionJsonlPath)),
    );
    const worktreeDest = path.join(worktreesStage, candidate.runId);
    await ensureDir(worktreeDest);
    await copyWorktreeFiles(candidate.worktreePath, worktreeDest, repoRoot);
    let sessionJsonlSnapshotPath: string | undefined;
    if (hasSession) {
      const jsonlDest = path.join(sessionsStage, `${candidate.runId}.jsonl`);
      await ensureDir(path.dirname(jsonlDest));
      await fsp.cp(candidate.sessionJsonlPath!, jsonlDest, { force: true });
      sessionJsonlSnapshotPath = path.posix.join("migrated-sessions", `${candidate.runId}.jsonl`);
    }
    const cloudWorktreeAbs = cloudWorktreeAbsolutePath(repoName, candidate.runId, candidate.task);
    // For fresh restarts, build a prompt-engineered handoff from the local
    // run record so the new agent gets context instead of just the bare task.
    let freshPrompt: string | undefined;
    if (!hasSession) {
      const runJsonPath = path.join(repoRoot, ".rudder", "runs", candidate.runId, "run.json");
      const record = await readJson<{ turns?: Array<{ prompt: string; source: string }> }>(runJsonPath);
      const turns = Array.isArray(record?.turns) ? record!.turns : [];
      freshPrompt = buildFreshHandoffPrompt(candidate, turns);
    }
    entries.push({
      runId: candidate.runId,
      task: candidate.task,
      taskSummary: candidate.taskSummary,
      backend: candidate.backend,
      sessionId: candidate.sessionId ?? "",
      localWorktreePath: candidate.worktreePath,
      cloudWorktreeRelativePath: cloudWorktreeAbs,
      sessionJsonlSnapshotPath: sessionJsonlSnapshotPath ?? "",
      worktreeBranch: candidate.worktreeBranch,
      freshPrompt,
    });
  }
  return entries;
}

async function copyWorktreeFiles(worktreePath: string, target: string, _repoRoot: string): Promise<void> {
  const result = await runCommand("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
    cwd: worktreePath,
    allowFailure: true,
  });
  const files = result.code === 0
    ? result.stdout.split("\0").filter(Boolean)
    : await listFiles(worktreePath);
  await mapLimit(files, 24, async (relative) => {
    if (!relative || relative.startsWith(".git/") || relative === ".git" || relative.startsWith(".rudder/")) {
      return;
    }
    const source = path.join(worktreePath, relative);
    const dest = path.join(target, relative);
    if (!isInside(worktreePath, source) || !isInside(target, dest)) {
      return;
    }
    const stat = await fsp.lstat(source).catch(() => null);
    if (!stat || stat.isDirectory() || !(await shouldIncludeSnapshotPath(source, "repo"))) {
      return;
    }
    await ensureDir(path.dirname(dest));
    await fsp.cp(source, dest, { dereference: false, force: true });
  });
}

async function copyRudderState(repoRoot: string, repoStage: string): Promise<{ runs: number; files: string[] }> {
  const copied: string[] = [];
  const rudderMd = path.join(repoRoot, "RUDDER.md");
  if (await pathExists(rudderMd)) {
    const target = path.join(repoStage, "RUDDER.md");
    await fsp.cp(rudderMd, target, { force: true });
    copied.push("RUDDER.md");
  }

  const runsDir = path.join(repoRoot, ".rudder", "runs");
  const entries = await fsp.readdir(runsDir, { withFileTypes: true }).catch(() => []);
  let runs = 0;
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name.includes("/") || entry.name.includes("\\")) {
      continue;
    }
    const runJson = path.join(runsDir, entry.name, "run.json");
    if (!(await pathExists(runJson))) {
      continue;
    }
    const relative = path.join(".rudder", "runs", entry.name, "run.json");
    const target = path.join(repoStage, relative);
    if (!isInside(repoRoot, runJson) || !isInside(repoStage, target)) {
      continue;
    }
    await ensureDir(path.dirname(target));
    await fsp.cp(runJson, target, { force: true });
    copied.push(relative);
    runs += 1;
  }
  return { runs, files: copied };
}

async function copyRepoFiles(repoRoot: string, repoStage: string): Promise<void> {
  const result = await runCommand("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const files = result.code === 0
    ? result.stdout.split("\0").filter(Boolean)
    : await listFiles(repoRoot);
  await mapLimit(files, 24, async (relative) => {
    if (!relative || relative.startsWith(".git/") || relative.startsWith(".rudder/")) {
      return;
    }
    const source = path.join(repoRoot, relative);
    const target = path.join(repoStage, relative);
    if (!isInside(repoRoot, source) || !isInside(repoStage, target)) {
      return;
    }
    const stat = await fsp.lstat(source).catch(() => null);
    if (!stat || stat.isDirectory() || !(await shouldIncludeSnapshotPath(source, "repo"))) {
      return;
    }
    await ensureDir(path.dirname(target));
    await fsp.cp(source, target, { dereference: false, force: true });
  });
}

async function mapLimit<T, R>(items: readonly T[], concurrency: number, fn: (item: T) => Promise<R>): Promise<R[]> {
  if (items.length === 0) {
    return [];
  }
  const results = new Array<R>(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.min(Math.max(1, concurrency), items.length) }, async () => {
    for (;;) {
      const index = next;
      next += 1;
      if (index >= items.length) {
        return;
      }
      results[index] = await fn(items[index]!);
    }
  });
  const settled = await Promise.allSettled(workers);
  const failed = settled.find((entry): entry is PromiseRejectedResult => entry.status === "rejected");
  if (failed) {
    throw failed.reason;
  }
  return results;
}

async function listFiles(dir: string): Promise<string[]> {
  const files: string[] = [];
  async function walk(current: string): Promise<void> {
    const entries = await fsp.readdir(current, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (entry.name === ".git" || entry.name === ".rudder" || entry.name === "node_modules") {
        continue;
      }
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
      } else {
        files.push(path.relative(dir, full));
      }
    }
  }
  await walk(dir);
  return files;
}

function normalizeHomePaths(requested: string[]): string[] {
  const raw = [
    ...DEFAULT_HOME_PATHS,
    ...requested,
    ...(process.env.RUDDER_CLOUD_HOME_PATHS?.split(",") ?? []),
  ];
  const home = os.homedir();
  const seen = new Set<string>();
  const paths: string[] = [];
  for (const item of raw) {
    const trimmed = item.trim();
    if (!trimmed) {
      continue;
    }
    const resolved = path.resolve(expandHome(trimmed));
    if (!isInside(home, resolved) || seen.has(resolved)) {
      continue;
    }
    seen.add(resolved);
    paths.push(resolved);
  }
  // If the user explicitly selected a parent directory, do not walk/copy its
  // default children a second time. This also prevents concurrent copies from
  // racing on the same destination files.
  return paths.filter((candidate) => !paths.some((other) => other !== candidate && isInside(other, candidate)));
}

async function stageClaudeKeychainCredentials(homeStage: string): Promise<boolean> {
  if (process.platform !== "darwin") {
    return false;
  }
  if (!commandExists("security")) {
    return false;
  }
  const result = await runCommand(
    "security",
    ["find-generic-password", "-s", "Claude Code-credentials", "-w"],
    { allowFailure: true },
  );
  if (result.code !== 0) {
    return false;
  }
  const payload = result.stdout.trim();
  if (!payload || !payload.startsWith("{")) {
    return false;
  }
  try {
    JSON.parse(payload);
  } catch {
    return false;
  }
  const targetDir = path.join(homeStage, ".claude");
  await ensureDir(targetDir);
  await fsp.writeFile(path.join(targetDir, ".credentials.json"), payload + "\n", { mode: 0o600 });
  return true;
}

async function copyHomePath(source: string, homeStage: string): Promise<boolean> {
  const exists = await pathExists(source);
  const included = exists ? await shouldIncludeSnapshotPath(source, "home") : false;
  if (!exists || !included) {
    return false;
  }
  const relative = path.relative(os.homedir(), source);
  const target = path.join(homeStage, relative);
  if (!isInside(homeStage, target)) {
    return false;
  }
  await fsp.cp(source, target, {
    // Absolute dotfile symlinks from the local machine are normally broken in
    // the worker. Dereference only after shouldIncludeSnapshotPath verifies
    // that every HOME symlink remains inside HOME and outside blocked paths.
    dereference: true,
    recursive: true,
    force: true,
    filter: async (candidate) => await shouldIncludeSnapshotPath(candidate, "home"),
  });
  return true;
}

async function shouldIncludeSnapshotPath(candidate: string, scope: "repo" | "home"): Promise<boolean> {
  const normalized = path.resolve(candidate);
  const parts = normalized.split(path.sep).map((part) => part.toLowerCase());
  const basename = path.basename(normalized).toLowerCase();
  if (basename.startsWith("._")) {
    return false;
  }
  if (
    parts.some((part) => SECRET_PATH_PARTS.has(part))
    || parts.some((part) => ALWAYS_BULKY_PATH_PARTS.has(part))
    || SECRET_BASENAMES.has(basename)
    || basename.startsWith(".env")
  ) {
    return false;
  }
  if (scope === "home") {
    if (parts.some((part) => BULKY_HOME_PATH_PARTS.has(part))) {
      return false;
    }
    if (BULKY_HOME_BASENAME_PATTERNS.some((pattern) => pattern.test(basename))) {
      return false;
    }
  }
  const stat = await fsp.lstat(normalized).catch(() => null);
  if (!stat) {
    return false;
  }
  if (stat.isSymbolicLink()) {
    if (scope === "repo") {
      // Repository symlinks are part of source semantics and are archived as
      // links (copyRepoFiles uses dereference:false).
      return true;
    }
    const real = await fsp.realpath(normalized).catch(() => "");
    if (!real || !isInside(await canonicalHomePath(), real)) {
      return false;
    }
    return await shouldIncludeSnapshotPath(real, "home");
  }
  if (!stat.isFile()) {
    return true;
  }
  if (scope === "repo") {
    return true;
  }
  // Keep the existing cheap scan for ordinary files. Large HOME files used to
  // bypass it entirely, allowing a large credentials dump to be uploaded just
  // by padding it past 1 MiB; stream those files with bounded memory instead.
  if (stat.size <= MAX_HOME_SECRET_SCAN_BYTES) {
    const text = await fsp.readFile(normalized, "utf8").catch(() => null);
    return text !== null && !cloudSecretMarkerPattern().test(text);
  }
  return !(await fileContainsCloudSecretMarker(normalized).catch(() => true));
}

let canonicalHomePathPromise: Promise<string> | undefined;
function canonicalHomePath(): Promise<string> {
  canonicalHomePathPromise ??= fsp.realpath(os.homedir()).catch(() => path.resolve(os.homedir()));
  return canonicalHomePathPromise;
}

function cloudSecretMarkerPattern(): RegExp {
  return /aws_access_key_id|aws_secret_access_key|aws_session_token/i;
}

async function fileContainsCloudSecretMarker(filePath: string): Promise<boolean> {
  const handle = await fsp.open(filePath, "r").catch(() => null);
  if (!handle) {
    return true;
  }
  const buffer = Buffer.allocUnsafe(64 * 1024);
  let carry = "";
  try {
    for (;;) {
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) {
        return false;
      }
      const text = carry + buffer.toString("utf8", 0, bytesRead);
      if (cloudSecretMarkerPattern().test(text)) {
        return true;
      }
      carry = text.slice(-64);
    }
  } finally {
    await handle.close();
  }
}

function isInside(parent: string, child: string): boolean {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

function openBrowser(url: string): void {
  const platform = process.platform;
  const command = platform === "darwin" ? "open" : platform === "win32" ? "cmd" : "xdg-open";
  const args = platform === "win32" ? ["/c", "start", "", url] : [url];
  const child = spawn(command, args, {
    detached: true,
    stdio: "ignore",
  });
  child.on("error", () => undefined);
  child.unref();
}

function withQuery(baseUrl: string, pathname: string, query: Record<string, string>): string {
  const url = new URL(pathname, `${baseUrl}/`);
  for (const [key, value] of Object.entries(query)) {
    url.searchParams.set(key, value);
  }
  return url.toString();
}

function parseJson(text: string): JsonValue {
  try {
    return JSON.parse(text) as JsonValue;
  } catch {
    return text;
  }
}

function responseErrorMessage(value: JsonValue | null): string | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const record = value as Record<string, JsonValue>;
  return typeof record.error === "string"
    ? record.error
    : typeof record.message === "string"
      ? record.message
      : undefined;
}

async function printResult(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (options.json) {
    printJson(result);
    return;
  }
  if (Array.isArray(result)) {
    printSailList(result);
    return;
  }
  if (result && typeof result === "object" && !Array.isArray(result)) {
    const record = result as Record<string, JsonValue>;
    if (typeof record.bootstrapCommand === "string") {
      const id = typeof record.id === "string" ? record.id : "BYOC sail";
      const status = typeof record.status === "string" ? record.status : undefined;
      const state = await loadCloudAuth();
      const host = options.sshHost ?? state?.byocSshHost;
      console.log(`${id}${status ? ` (${status})` : ""} is ready for BYOC.`);
      // bootstrapCommand is a shell command supplied by the control plane. Auto-running
      // it over SSH is RCE-on-your-host if the control plane is compromised or MITM'd,
      // so autostart is OPT-IN (RUDDER_BYOC_AUTOSTART=1) and the command is always
      // printed first so the user can review exactly what would run.
      console.log("BYOC bootstrap command (review before running):");
      console.log(record.bootstrapCommand);
      if (host && process.env.RUDDER_BYOC_AUTOSTART === "1") {
        try {
          console.log(`\nRUDDER_BYOC_AUTOSTART=1 set — running the above over SSH on ${host}...`);
          await startByocWorkerOverSsh(host, record.bootstrapCommand);
          console.log(`Started BYOC worker over SSH on ${host}.`);
          console.log(`Remote log: ssh ${host} 'tail -f ~/.rudder/byoc/worker.log'`);
        } catch (error) {
          console.log(`Could not start BYOC worker over SSH on ${host}: ${error instanceof Error ? error.message : String(error)}`);
          console.log("Run the command above manually on your workstation/server.");
        }
      } else {
        console.log("\nRun the command above on your workstation/server.");
        if (host) {
          console.log("To auto-run it over SSH next time, set RUDDER_BYOC_AUTOSTART=1.");
        } else {
          console.log("Tip: run `rudder cloud byoc <ssh-host>` to target an SSH host.");
        }
      }
      if (typeof record.updatedAt === "string") {
        console.log(`\nIf the command expires, run: rudder cloud bootstrap ${id}`);
      }
      return;
    }
    const sails = record.sails ?? record.items;
    if (Array.isArray(sails)) {
      printSailList(sails);
      return;
    }
    if (typeof record.id === "string" && (typeof record.status === "string" || typeof record.runtime === "string")) {
      const parts = [
        record.id,
        typeof record.status === "string" ? record.status : undefined,
        typeof record.runtime === "string" ? record.runtime : undefined,
        typeof record.repoName === "string" ? record.repoName : undefined,
      ].filter(Boolean);
      console.log(parts.join("  "));
      if (record.workspace === true || (record.run === null && typeof record.task !== "string")) {
        console.log("Rudder workspace uploaded. Use /cloud list to track it.");
      } else {
        console.log("Cloud worker created. Use /cloud list to track it.");
      }
      return;
    }
  }
  console.log(JSON.stringify(result, null, 2));
}

function printSailList(items: JsonValue[]): void {
  if (items.length === 0) {
    console.log("No cloud sails.");
    return;
  }
  for (const item of items) {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      console.log(String(item));
      continue;
    }
    const sail = item as CloudSail;
    console.log([
      sail.id,
      sail.status,
      sail.runtime,
      typeof sail.task === "string" && sail.task ? sail.task : undefined,
      typeof sail.repoName === "string" && sail.repoName ? sail.repoName : undefined,
      sail.branch,
      sail.url,
      sail.updatedAt ?? sail.createdAt,
    ].filter(Boolean).join("  "));
  }
}

function printJson(value: JsonValue): void {
  console.log(JSON.stringify(value, null, 2));
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function workspaceCommand(args: string[], options: CloudCommandOptions): Promise<void> {
  const sub = args[0] ?? "";
  const rest = args.slice(1);
  if (sub === "" || sub === "attach") {
    await workspaceAttach(rest, options);
    return;
  }
  if (sub === "share") {
    await workspaceShare(options);
    return;
  }
  if (sub === "status") {
    await workspaceStatus(options);
    return;
  }
  if (sub === "stop" || sub === "pause" || sub === "resume" || sub === "delete" || sub === "rm") {
    const action = sub === "rm" ? "delete" : sub;
    await workspaceMutate(action, rest, options);
    return;
  }
  if (sub === "list" || sub === "ls") {
    await workspaceList(options);
    return;
  }
  throw new Error("Usage: rudder cloud workspace [attach [id]|share|status|pause|resume|stop|delete|list]");
}

function computeWorkspaceKey(repoRoot: string): string {
  const normalized = path.resolve(repoRoot);
  return createHash("sha256").update(normalized).digest("hex").slice(0, 32);
}

let cachedFlyRegion: string | undefined;
async function detectFlyRegion(baseUrl: string): Promise<string | undefined> {
  if (process.env.RUDDER_CLOUD_REGION) {
    return process.env.RUDDER_CLOUD_REGION.trim().toLowerCase();
  }
  if (cachedFlyRegion) {
    return cachedFlyRegion;
  }
  try {
    const response = await fetchWithTimeout(
      `${baseUrl.replace(/\/$/, "")}/health`,
      { method: "GET" },
      DEFAULT_REGION_DETECT_TIMEOUT_MS,
      "Cloud region detection",
    );
    const requestId = response.headers.get("fly-request-id");
    await response.body?.cancel().catch(() => undefined);
    if (requestId) {
      // Fly request-id format: <ulid>-<region>
      const dash = requestId.lastIndexOf("-");
      const region = dash > 0 ? requestId.slice(dash + 1).trim().toLowerCase() : "";
      if (region && region.length <= 6 && /^[a-z]+$/.test(region)) {
        cachedFlyRegion = region;
        return region;
      }
    }
  } catch {
    // ignore — server will fall back to its default region
  }
  return undefined;
}

async function computeSnapshotFingerprint(repoRoot: string, requestedHomePaths: string[]): Promise<string> {
  const hash = createHash("sha256");
  // HEAD covers every unchanged tracked file. Porcelain identifies the much
  // smaller dirty/untracked set, whose actual bytes are hashed below. Hashing
  // only the porcelain text caused stale warm restarts whenever an already-M
  // file was edited again because its status line did not change.
  const headCommit = await currentCommit(repoRoot).catch(() => "");
  hash.update(`repo:head:${headCommit}\n`);
  const status = await runCommand("git", ["status", "--porcelain=v1", "-z", "--untracked-files=all"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  if (status.code === 0) {
    hash.update(`repo:status:${status.stdout}\n`);
    for (const relative of dirtyPathsFromPorcelain(status.stdout).sort()) {
      const candidate = path.join(repoRoot, relative);
      if (isInside(repoRoot, candidate)) {
        await updateHashForFile(hash, candidate, `repo:file:${relative}`);
      }
    }
  }
  // HOME selections are part of the snapshot too. Metadata is enough to
  // invalidate the cache for ordinary editor/CLI rewrites without rereading
  // large credential databases on every warm attach. The snapshot filter is
  // applied here as well, so excluded secrets/bulk do not create churn.
  const homeFingerprints = await mapLimit(normalizeHomePaths(requestedHomePaths), 8, fingerprintHomePath);
  for (const entry of homeFingerprints.filter(Boolean).sort()) {
    hash.update(`home:${entry}\n`);
  }
  // macOS Keychain claude credentials: hash content so re-logging in
  // invalidates the cache but a steady-state user keeps it.
  if (process.platform === "darwin") {
    const creds = await runCommand("security", ["find-generic-password", "-s", "Claude Code-credentials", "-w"], {
      allowFailure: true,
    });
    if (creds.code === 0) {
      hash.update(`keychain:claude:${createHash("sha256").update(creds.stdout).digest("hex")}\n`);
    }
  }
  // Captured env vars (excluding ones we know rotate like AWS session tokens).
  const env = captureCloudEnv();
  const unstable = new Set(["AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN"]);
  for (const key of Object.keys(env).sort()) {
    if (unstable.has(key)) continue;
    hash.update(`env:${key}:${createHash("sha256").update(env[key] ?? "").digest("hex")}\n`);
  }
  return hash.digest("hex").slice(0, 32);
}

function dirtyPathsFromPorcelain(value: string): string[] {
  const records = value.split("\0");
  const paths: string[] = [];
  for (let i = 0; i < records.length; i += 1) {
    const record = records[i];
    if (!record || record.length < 4) {
      continue;
    }
    const status = record.slice(0, 2);
    const relative = record.slice(3);
    if (relative) {
      paths.push(relative);
    }
    // In -z mode rename/copy records are "XY new\0old\0". The snapshot
    // contains the new path; consume the old path so it is not parsed as a
    // status record on the next iteration.
    if (/[RC]/.test(status)) {
      i += 1;
    }
  }
  return [...new Set(paths)];
}

async function updateHashForFile(
  hash: ReturnType<typeof createHash>,
  filePath: string,
  label: string,
): Promise<void> {
  const stat = await fsp.lstat(filePath).catch(() => null);
  if (!stat) {
    hash.update(`${label}:missing\n`);
    return;
  }
  hash.update(`${label}:mode=${stat.mode}:size=${stat.size}\n`);
  if (stat.isSymbolicLink()) {
    hash.update(`link:${await fsp.readlink(filePath).catch(() => "")}\n`);
    return;
  }
  if (!stat.isFile()) {
    return;
  }
  const handle = await fsp.open(filePath, "r");
  const buffer = Buffer.allocUnsafe(128 * 1024);
  try {
    for (;;) {
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) {
        break;
      }
      hash.update(buffer.subarray(0, bytesRead));
    }
  } finally {
    await handle.close();
  }
  hash.update("\n");
}

async function fingerprintHomePath(source: string): Promise<string> {
  if (!(await pathExists(source)) || !(await shouldIncludeSnapshotPath(source, "home"))) {
    return "";
  }
  const hash = createHash("sha256");
  const root = path.resolve(source);
  const seenDirectories = new Set<string>();
  async function walk(candidate: string, relative: string): Promise<void> {
    if (!(await shouldIncludeSnapshotPath(candidate, "home"))) {
      return;
    }
    let stat = await fsp.lstat(candidate).catch(() => null);
    if (!stat) {
      return;
    }
    let actual = candidate;
    if (stat.isSymbolicLink()) {
      actual = await fsp.realpath(candidate).catch(() => "");
      if (!actual || !isInside(await canonicalHomePath(), actual)) {
        return;
      }
      stat = await fsp.stat(actual).catch(() => null);
      if (!stat) {
        return;
      }
    }
    hash.update(`${relative}\0${stat.mode}\0${stat.size}\0${stat.mtimeMs}\n`);
    if (!stat.isDirectory()) {
      return;
    }
    const realDirectory = await fsp.realpath(actual).catch(() => actual);
    if (seenDirectories.has(realDirectory)) {
      return;
    }
    seenDirectories.add(realDirectory);
    const entries = await fsp.readdir(actual, { withFileTypes: true }).catch(() => []);
    entries.sort((a, b) => a.name.localeCompare(b.name));
    for (const entry of entries) {
      await walk(path.join(actual, entry.name), path.posix.join(relative, entry.name));
    }
  }
  await walk(root, shortenHome(root));
  return `${shortenHome(root)}:${hash.digest("hex")}`;
}

async function workspaceAttach(args: string[], options: CloudCommandOptions): Promise<void> {
  const explicitId = args[0];
  if (explicitId) {
    await workspaceAttachById(explicitId, options);
    return;
  }
  const repoRoot = findRepoRoot();
  const workspaceKey = computeWorkspaceKey(repoRoot);
  const repoName = path.basename(repoRoot);
  const client = await cloudClient({ requireToken: true });

  if (!options.json) {
    process.stderr.write(`Resolving cloud workspace for ${repoName}...\n`);
  }

  // Kick off the non-interactive work in parallel. planAgentMigration can
  // call promptConfirm for a TTY prompt, so we serialize it AFTER the
  // parallel work resolves to avoid garbled stdout during the prompt.
  const [region, fingerprint] = await Promise.all([
    detectFlyRegion(client.baseUrl).catch(() => undefined),
    computeSnapshotFingerprint(repoRoot, options.homePaths ?? []),
  ]);
  const migrationPlan = await planAgentMigration(repoRoot, options);
  const mustUploadSnapshot = Boolean(migrationPlan && migrationPlan.migrated.length > 0);
  const baseBody: Record<string, JsonValue> = {
    workspaceKey,
    repoName,
    snapshotFingerprint: fingerprint,
    ...(region ? { region } : {}),
  };
  let result: JsonValue | null = null;
  if (!mustUploadSnapshot) {
    try {
      result = await client.request<JsonValue>("/api/rudder/workspace/attach", {
        method: "POST",
        body: baseBody,
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!/snapshot/i.test(message)) {
        throw error;
      }
      result = null;
    }
  }
  if (!result) {
    if (!options.json) {
      process.stderr.write(`Uploading workspace snapshot...\n`);
    }
    const snapshot = await createSnapshot(repoRoot, options.homePaths ?? [], {
      includeRudderState: true,
      migration: migrationPlan ? { repoName, plan: migrationPlan } : undefined,
    });
    try {
      const body: Record<string, JsonValue> = {
        ...baseBody,
        snapshot: await snapshotPayload(snapshot),
      };
      if (migrationPlan && migrationPlan.migrated.length > 0) {
        body.migratedAgents = summaryAsJson(migrationPlan);
      }
      result = await client.request<JsonValue>("/api/rudder/workspace/attach", {
        method: "POST",
        body,
        timeoutMs: cloudUploadTimeoutMs(),
      });
    } finally {
      await fsp.rm(snapshot.tempDir, { recursive: true, force: true });
    }
  }
  if (migrationPlan && !options.json) {
    process.stderr.write(`${migrationSummary(migrationPlan)}\n`);
  }
  if (!result) {
    throw new Error("Workspace attach returned no result");
  }
  await attachToWorkspaceResult(result, options);
}

async function planAgentMigration(
  repoRoot: string,
  options: CloudCommandOptions,
): Promise<MigrationPlan | null> {
  const candidates = await findMigrationCandidates(repoRoot);
  if (candidates.length === 0) {
    return null;
  }
  const plan = applyDefaultDecisions(candidates);
  if (options.json) {
    return plan;
  }
  if (!isTty()) {
    return plan;
  }
  console.log(migrationSummary(plan));
  if (plan.migrated.length === 0) {
    return plan;
  }
  const confirmed = await promptConfirm("Move resumable agents to cloud?", true);
  if (confirmed) {
    return plan;
  }
  return {
    candidates,
    migrated: [],
    stayedLocal: candidates.map((c) => ({ ...c, decision: "stay" as const })),
  };
}

async function attachToWorkspaceResult(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    throw new Error("Unexpected workspace response from cloud");
  }
  const record = result as Record<string, JsonValue>;
  const workspaceId = typeof record.id === "string" ? record.id : undefined;
  if (!workspaceId) {
    throw new Error("Workspace response is missing id");
  }
  if (options.json) {
    printJson(record);
    return;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    process.stderr.write(`Workspace ${workspaceId} is ready. Run \`rudder cloud workspace attach\` from a TTY to take over.\n`);
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  await runAttach({ kind: "workspace", id: workspaceId, label: `workspace ${workspaceId}` }, { ...options, quietBanner: false });
}

async function workspaceAttachById(workspaceId: string, options: CloudCommandOptions): Promise<void> {
  if (options.json) {
    throw new Error("Cloud attach is an interactive stream and cannot be used with --json.");
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    process.stderr.write(`Workspace ${workspaceId}: attach requires a TTY.\n`);
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  await runAttach(
    { kind: "workspace", id: workspaceId, label: `workspace ${workspaceId}` },
    { ...options, quietBanner: false },
  );
}

async function lookupWorkspaceForRepo(
  options: CloudCommandOptions,
): Promise<Record<string, JsonValue> | null> {
  void options;
  const repoRoot = findRepoRoot();
  const workspaceKey = computeWorkspaceKey(repoRoot);
  const client = await cloudClient({ requireToken: true });
  try {
    const result = await client.request<JsonValue>(
      `/api/rudder/workspace/lookup?key=${encodeURIComponent(workspaceKey)}`,
      { method: "GET" },
    );
    if (result && typeof result === "object" && !Array.isArray(result)) {
      return result as Record<string, JsonValue>;
    }
    return null;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (/not found/i.test(message) || /404/.test(message)) {
      return null;
    }
    throw error;
  }
}

async function workspaceShare(options: CloudCommandOptions): Promise<void> {
  const workspace = await lookupWorkspaceForRepo(options);
  if (!workspace) {
    if (options.json) {
      printJson({ workspace: null });
      return;
    }
    console.log("No cloud workspace exists for this repo yet. Run `rudder cloud workspace attach` to create one.");
    return;
  }
  const id = typeof workspace.id === "string" ? workspace.id : "";
  if (!id) {
    throw new Error("Workspace lookup returned no id");
  }
  if (options.json) {
    printJson({
      id,
      attachCommand: `rudder cloud workspace attach ${id}`,
      status: workspace.status ?? null,
    });
    return;
  }
  console.log("Share this workspace with a teammate by sending them:");
  console.log("");
  console.log(`  rudder cloud workspace attach ${id}`);
  console.log("");
  console.log("They must already be logged in to Rudder Cloud with their own account (run `rudder cloud login` if not).");
}

async function workspaceStatus(options: CloudCommandOptions): Promise<void> {
  if (process.env.RUDDER_OFFLINE === "1") {
    if (options.json) {
      printJson({ offline: true, workspace: null });
    } else {
      console.log("RUDDER_OFFLINE is set; skipping cloud workspace status check.");
    }
    return;
  }
  let workspace: Record<string, JsonValue> | null;
  try {
    workspace = await lookupWorkspaceForRepo(options);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (options.json) {
      printJson({ offline: true, workspace: null, error: message });
    } else {
      console.warn(`Could not reach Rudder Cloud: ${message}`);
    }
    return;
  }
  if (!workspace) {
    if (options.json) {
      printJson({ workspace: null });
    } else {
      console.log("No cloud workspace for this repo.");
    }
    return;
  }
  const id = typeof workspace.id === "string" ? workspace.id : "";
  const status = typeof workspace.status === "string" ? workspace.status : "unknown";
  const clientCount = typeof workspace.clientCount === "number" ? workspace.clientCount : 0;
  const lastActivityAt = typeof workspace.lastActivityAt === "string" ? workspace.lastActivityAt : undefined;
  const idleMinutes = computeIdleMinutes(lastActivityAt);
  const activeAgents = clientCount > 0 || (idleMinutes !== null && idleMinutes < 5);
  if (options.json) {
    printJson({
      id,
      status,
      clientCount,
      lastActivityAt: lastActivityAt ?? null,
      idleMinutes,
      activeAgents,
      repoName: typeof workspace.repoName === "string" ? workspace.repoName : null,
    });
    return;
  }
  const idlePart = idleMinutes !== null ? `  idle ${idleMinutes}m` : "";
  console.log(`workspace ${id}  ${status}  clients=${clientCount}${idlePart}`);
  if (activeAgents) {
    console.log("Active agents likely running.");
  } else {
    console.log("No recent activity.");
  }
}

function computeIdleMinutes(lastActivityAt: string | undefined): number | null {
  if (!lastActivityAt) {
    return null;
  }
  const ms = Date.parse(lastActivityAt);
  if (!Number.isFinite(ms)) {
    return null;
  }
  const diff = Date.now() - ms;
  if (diff < 0) {
    return 0;
  }
  return Math.floor(diff / 60_000);
}

async function workspaceMutate(
  action: "pause" | "resume" | "stop" | "delete",
  args: string[],
  options: CloudCommandOptions,
): Promise<void> {
  const id = args[0];
  if (!id) {
    throw new Error(`Usage: rudder cloud workspace ${action} <id>`);
  }
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>(`/api/rudder/workspace/${encodeURIComponent(id)}/${action}`, {
    method: "POST",
    body: {} as JsonValue,
  });
  await printResult(result, options);
}

async function workspaceList(options: CloudCommandOptions): Promise<void> {
  const client = await cloudClient({ requireToken: true });
  const result = await client.request<JsonValue>("/api/rudder/workspace", { method: "GET" });
  await printResult(result, options);
}

async function attach(args: string[], options: CloudCommandOptions): Promise<void> {
  const sailId = args[0];
  if (!sailId) {
    throw new Error("Usage: rudder cloud attach <id>");
  }
  if (options.json) {
    throw new Error("Cloud attach is an interactive stream and cannot be used with --json.");
  }
  await runAttach({ kind: "sail", id: sailId, label: sailId }, options);
}

type AttachTarget = {
  kind: "sail" | "workspace";
  id: string;
  label: string;
};

type AttachResult = "exited" | "failed";

async function runAttach(target: AttachTarget, options: CloudCommandOptions): Promise<AttachResult> {
  const client = await cloudClient({ requireToken: true });
  const baseUrl = client.baseUrl;
  const state = await loadCloudAuth();
  const envToken = process.env.RUDDER_CLOUD_TOKEN?.trim();
  const token = envToken || (state?.cloudUrl === baseUrl ? state.token.trim() : undefined);
  if (!token) {
    throw new Error("Not logged in to Rudder Cloud. Run `rudder login` first.");
  }
  const wsUrl = baseUrl.replace(/^http/, "ws").replace(/\/$/, "")
    + `/api/rudder/${target.kind}/${encodeURIComponent(target.id)}/attach`;

  const stdin = process.stdin;
  const stdout = process.stdout;
  const isInteractive = Boolean(stdin.isTTY && stdout.isTTY);
  const connectTimeoutMs = attachConnectTimeoutMs();
  const readyTimeoutMs = attachReadyTimeoutMs();

  return await new Promise<AttachResult>((resolve, reject) => {
    const socket = new WebSocket(wsUrl, {
      headers: { authorization: `Bearer ${token}` },
      handshakeTimeout: connectTimeoutMs,
      maxPayload: 4 * 1024 * 1024,
    });
    socket.binaryType = "nodebuffer";
    let opened = false;
    let cleaned = false;
    let firstFrameRendered = false;
    let result: AttachResult = "exited";
    let readyTimer: NodeJS.Timeout | undefined;
    let resizeTimer: NodeJS.Timeout | undefined;
    const connectTimer = setTimeout(() => {
      if (opened) {
        return;
      }
      cleanup();
      try { socket.terminate(); } catch { /* ignore */ }
      reject(new Error(`Cloud attach timed out after ${Math.ceil(connectTimeoutMs / 1000)}s`));
    }, connectTimeoutMs);
    connectTimer.unref?.();

    const splashAllowed = isInteractive && !options.json && !options.quietBanner;
    const splash = splashAllowed ? new AttachSplash(stdout, target.label) : null;

    const sendResize = () => {
      if (socket.readyState !== WebSocket.OPEN) {
        return;
      }
      const cols = stdout.columns ?? 120;
      const rows = stdout.rows ?? 32;
      socket.send(JSON.stringify({ type: "resize", cols, rows }));
    };

    const scheduleResize = () => {
      if (resizeTimer) {
        clearTimeout(resizeTimer);
      }
      resizeTimer = setTimeout(sendResize, 150);
      resizeTimer.unref?.();
    };

    const markReady = () => {
      firstFrameRendered = true;
      if (readyTimer) {
        clearTimeout(readyTimer);
        readyTimer = undefined;
      }
    };

    let lastCtrlC = 0;
    const onStdin = (chunk: Buffer | string) => {
      if (socket.readyState !== WebSocket.OPEN) {
        return;
      }
      const buffer = typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk;
      const isCtrlC = buffer.length === 1 && buffer[0] === 0x03;
      // While the loading splash is up (no remote frame has rendered yet),
      // Ctrl+C should cancel the local attach instead of being forwarded to
      // a remote dashboard the user can't see.
      if (isCtrlC && !firstFrameRendered) {
        try { socket.close(1000, "client-cancel"); } catch { /* ignore */ }
        cleanup();
        if (!opened) {
          reject(new Error("Cloud attach cancelled"));
        } else {
          process.stderr.write("\nCancelled.\n");
          resolve("exited");
        }
        return;
      }
      // After handoff: forward Ctrl+C to the remote so Claude/codex can be
      // cancelled, but if the user mashes Ctrl+C twice within 2 seconds we
      // take it as "the remote is unresponsive; get me out".
      if (isCtrlC && firstFrameRendered) {
        const now = Date.now();
        if (now - lastCtrlC < 2000) {
          process.stderr.write("\nForce-exiting local attach (press Ctrl+C again to re-enter).\n");
          try { socket.close(1000, "client-force-quit"); } catch { /* ignore */ }
          cleanup();
          resolve("exited");
          return;
        }
        lastCtrlC = now;
      }
      socket.send(buffer, { binary: true });
    };

    const onResize = () => {
      sendResize();
      splash?.redraw();
    };

    const cleanup = () => {
      if (cleaned) {
        return;
      }
      cleaned = true;
      clearTimeout(connectTimer);
      if (readyTimer) {
        clearTimeout(readyTimer);
        readyTimer = undefined;
      }
      if (resizeTimer) {
        clearTimeout(resizeTimer);
        resizeTimer = undefined;
      }
      stdin.off("data", onStdin);
      stdout.off("resize", onResize);
      process.off("SIGINT", onSigint);
      splash?.dispose();
      if (isInteractive && stdin.isTTY) {
        // Disable local mouse capture before leaving raw mode so the user's
        // shell prompt does not get spammed with mouse SGR bytes.
        try {
          stdout.write("\x1b[?1006l\x1b[?1002l\x1b[?1000l");
        } catch {
          // ignore
        }
        // Release the tab title so the user's shell prompt can rewrite it.
        try {
          stdout.write("\x1b]0;\x07");
        } catch {
          // ignore
        }
        try {
          stdin.setRawMode(false);
        } catch {
          // ignore
        }
      }
      if (splashAllowed && opened) {
        // We held the local terminal in the alt-screen buffer for the entire
        // attach session (see AttachSplash.handoff). Restore the main buffer
        // and cursor now so the user is dropped back to their shell prompt
        // instead of a blank alt-screen with the dashboard frozen on it.
        try {
          stdout.write("\x1b[?1049l\x1b[?25h");
        } catch {
          // ignore
        }
      }
      stdin.pause();
    };

    const onSigint = () => {
      // Belt-and-suspenders: if raw mode failed for any reason, Node still
      // sees SIGINT. Close cleanly so the user never gets a frozen terminal.
      process.stderr.write("\nReceived SIGINT, closing cloud attach.\n");
      try { socket.close(1000, "sigint"); } catch { /* ignore */ }
      cleanup();
      if (!opened) {
        reject(new Error("Cloud attach cancelled"));
      } else {
        resolve("exited");
      }
    };
    process.once("SIGINT", onSigint);

    socket.on("open", () => {
      opened = true;
      clearTimeout(connectTimer);
      readyTimer = setTimeout(() => {
        cleanup();
        try { socket.terminate(); } catch { /* ignore */ }
        reject(new Error(`Cloud worker did not become ready within ${Math.ceil(readyTimeoutMs / 1000)}s`));
      }, readyTimeoutMs);
      readyTimer.unref?.();
      // Disable Nagle on the underlying TCP socket so single keystrokes don't
      // sit in the send buffer waiting for piggyback ACKs (~40ms savings per
      // keypress on the WAN).
      try {
        const underlying = (socket as unknown as { _socket?: { setNoDelay?: (v: boolean) => void } })._socket;
        underlying?.setNoDelay?.(true);
      } catch {
        // ignore
      }
      // Label this terminal tab so the user can find the right rudder cloud
      // session at a glance instead of squinting at a row of "ghostty" tabs.
      try {
        stdout.write(`\x1b]0;Rudder cloud: ${target.label}\x07`);
      } catch {
        // ignore
      }
      if (splash) {
        splash.start();
        splash.setStatus(`Booting cloud workspace · ${target.label}`);
      } else if (!options.json && !options.quietBanner) {
        const tail = isInteractive ? " (Ctrl+C sends to remote; close this pane to detach)" : "";
        process.stderr.write(`Attached to ${target.label}${tail}\n`);
      }
      sendResize();
      if (isInteractive) {
        try {
          stdin.setRawMode(true);
        } catch {
          // ignore
        }
        // Enable mouse tracking on the LOCAL terminal so scroll/click events
        // get forwarded into stdin. The remote rudder TUI also emits these
        // enable sequences, but they arrive as PTY output bytes and the local
        // terminal does not interpret output bytes as mode toggles. We have
        // to enable mouse capture locally too.
        try {
          stdout.write("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        } catch {
          // ignore
        }
      }
      stdin.resume();
      stdin.on("data", onStdin);
      stdout.on("resize", onResize);
    });

    socket.on("message", (data, isBinary) => {
      if (isBinary && Buffer.isBuffer(data)) {
        if (!firstFrameRendered) {
          markReady();
          splash?.handoff();
          // Re-send resize right after handoff and once more on a small
          // delay; some terminals don't surface accurate columns/rows
          // until after alt-screen exits, so the initial resize at WS
          // open can race.
          sendResize();
          scheduleResize();
        }
        stdout.write(data);
        return;
      }
      const text = Buffer.isBuffer(data)
        ? data.toString("utf8")
        : Array.isArray(data)
          ? Buffer.concat(data).toString("utf8")
          : Buffer.from(data as ArrayBuffer).toString("utf8");
      handleControlText(text);
    });

    socket.on("close", (code, reason) => {
      cleanup();
      if (!opened) {
        const reasonText = reason && reason.length ? reason.toString("utf8") : "";
        reject(new Error(`Cloud attach failed (code ${code}${reasonText ? `: ${reasonText}` : ""})`));
        return;
      }
      if (code !== 1000 && code !== 1001 && result === "exited") {
        const reasonText = reason && reason.length ? reason.toString("utf8") : "";
        reject(new Error(`Cloud attach disconnected (code ${code}${reasonText ? `: ${reasonText}` : ""})`));
        return;
      }
      resolve(result);
    });

    socket.on("error", (err) => {
      cleanup();
      try { socket.terminate(); } catch { /* ignore */ }
      reject(new Error(`Cloud attach failed: ${err.message}`));
    });

    function handleControlText(text: string): void {
      let payload: unknown;
      try {
        payload = JSON.parse(text);
      } catch {
        stdout.write(text);
        return;
      }
      if (!payload || typeof payload !== "object") {
        return;
      }
      const message = payload as { type?: string; state?: string; code?: number };
      if (message.type === "exit") {
        const exitCode = Number.isInteger(message.code) && message.code! >= 0 && message.code! <= 255
          ? message.code!
          : 1;
        result = exitCode === 0 ? "exited" : "failed";
        if (typeof process.exitCode !== "number") {
          process.exitCode = exitCode;
        }
        try { socket.close(1000, "remote-exit"); } catch { /* ignore */ }
        return;
      }
      if (message.type === "status") {
        // If the worker dies before we've ever seen a remote frame, the
        // session is effectively dead. Don't sit on a splash spinner pretending
        // it'll come back; bail.
        if (message.state === "worker-disconnected" && !firstFrameRendered) {
          splash?.dispose();
          try { socket.close(1000, "worker-gone"); } catch { /* ignore */ }
          cleanup();
          if (!opened) {
            reject(new Error("Cloud worker exited before the dashboard started"));
          } else {
            process.stderr.write("\nCloud worker exited before the dashboard started.\n");
            result = "failed";
            resolve(result);
          }
          return;
        }
        if (splash && !firstFrameRendered) {
          if (message.state === "worker-waiting") {
            splash.setStatus(`Waiting for cloud worker · ${target.label}`);
          } else if (message.state === "worker-connected") {
            // Hand off the splash as soon as the server reports the worker
            // is connected. Waiting for the first BINARY PTY frame can add
            // 3-6s on warm restart because the worker may not flush until
            // after its first render. The binary-frame path below still
            // calls handoff() as a safety net if we never see this status.
            markReady();
            splash.handoff();
            sendResize();
            scheduleResize();
          }
        } else if (!options.json && !options.quietBanner) {
          if (message.state === "worker-disconnected") {
            process.stderr.write("\nCloud worker disconnected; waiting for reconnect...\n");
          } else if (message.state === "worker-connected") {
            process.stderr.write("Cloud worker connected.\n");
          }
        }
      }
    }
  });
}

class AttachSplash {
  private stdout: NodeJS.WriteStream;
  private label: string;
  private frame = 0;
  private status: string;
  private timer: NodeJS.Timeout | undefined;
  private active = false;
  private static FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

  constructor(stdout: NodeJS.WriteStream, label: string) {
    this.stdout = stdout;
    this.label = label;
    this.status = `Connecting to ${label}`;
  }

  start(): void {
    if (this.active) {
      return;
    }
    this.active = true;
    this.stdout.write("\x1b[?1049h\x1b[?25l");
    this.redraw();
    this.timer = setInterval(() => {
      this.frame = (this.frame + 1) % AttachSplash.FRAMES.length;
      this.redraw();
    }, 100);
    this.timer.unref?.();
  }

  setStatus(status: string): void {
    this.status = status;
    if (this.active) {
      this.redraw();
    }
  }

  redraw(): void {
    if (!this.active) {
      return;
    }
    const cols = this.stdout.columns ?? 80;
    const rows = this.stdout.rows ?? 24;
    const spinner = AttachSplash.FRAMES[this.frame] ?? "·";
    const lines = [
      `${spinner}  ${this.status}`,
      `   Ctrl+C to disconnect`,
    ];
    const top = Math.max(1, Math.floor(rows / 2) - 1);
    this.stdout.write("\x1b[2J");
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i] ?? "";
      const visible = stripAnsi(line);
      const left = Math.max(1, Math.floor((cols - visible.length) / 2) + 1);
      this.stdout.write(`\x1b[${top + i};${left}H${line}`);
    }
  }

  handoff(): void {
    if (!this.active) {
      return;
    }
    this.stop();
    // Stay in the alt-screen buffer for the rest of the attach session. The
    // remote dashboard will render into this same buffer, and runAttach's
    // cleanup() will exit the alt screen on shutdown. Wipe the splash content
    // first so the remote's partial frames don't render on top of leftover
    // spinner text (and so any tiny window before the remote's first frame
    // is a clean buffer, not main-buffer scrollback).
    this.stdout.write("\x1b[2J\x1b[H\x1b[?25h");
  }

  dispose(): void {
    if (!this.active) {
      return;
    }
    this.stop();
    // dispose() is the abort path (worker died, user cancelled before any
    // remote frame). Leave the alt screen so the user is dropped back to their
    // shell prompt instead of staring at a frozen spinner buffer.
    this.stdout.write("\x1b[?1049l\x1b[?25h");
  }

  private stop(): void {
    this.active = false;
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  }
}

function stripAnsi(value: string): string {
  return value.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");
}

async function maybeAutoAttach(result: JsonValue, options: CloudCommandOptions): Promise<void> {
  if (options.json || options.noAttach) {
    return;
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    return;
  }
  if (process.env.RUDDER_CLOUD_NO_ATTACH === "1") {
    return;
  }
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    return;
  }
  const record = result as Record<string, JsonValue>;
  if (typeof record.bootstrapCommand === "string") {
    return;
  }
  const sailId = extractSailId(record);
  if (!sailId) {
    return;
  }
  try {
    await runAttach({ kind: "sail", id: sailId, label: sailId }, { ...options, quietBanner: false });
  } catch (error) {
    console.warn(`Could not attach to ${sailId}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function extractSailId(record: Record<string, JsonValue>): string | undefined {
  if (typeof record.id === "string" && record.id) {
    return record.id;
  }
  return undefined;
}

function printCloudHelp(): void {
  console.log(`rudder cloud

Usage:
  rudder cloud login
  rudder cloud help
  rudder cloud [name or task]
  rudder cloud launch [--home-path <path>] ["task"]
  rudder cloud byoc [ssh-host]
  rudder cloud vm ["task"]
  rudder cloud list
  rudder cloud onload [runId]
      no runId uploads the current Rudder workspace state
  rudder cloud logs <id>
      show current worker status and buffered terminal output
  rudder cloud attach <id>
      stream the live cloud worker terminal into this pane
  rudder cloud talk <id> "<message>"
      send a message to a running instance and show its reply
  rudder cloud output <id>
      show an instance's latest output
  rudder cloud quickstart
      print the copy/paste setup for the whole flow
  rudder cloud slack [manifest]
      print Slack setup (one thread per instance in the shared channel)
  rudder cloud delete <id>
      permanently delete a worker (or workspace with the same id)
  rudder cloud workspace [attach [id]|share|status [--json]|pause <id>|resume <id>|stop <id>|delete <id>|list]
      shared cloud workspace for this repo
  rudder cloud bootstrap <id>
  rudder cloud runtime [fly|byoc]
  rudder cloud setup-byoc <ssh-host>   compatibility alias
  rudder cloud setup-fly
  rudder sail [name or task]
  rudder sail list
  rudder sail pause <id>
  rudder sail resume <id>
  rudder sail delete <id>
  rudder cloud setup-github <client-id>
  rudder cloud setup-google <client-id>

Environment:
  RUDDER_CLOUD_URL              Cloud control plane URL (defaults to ${DEFAULT_CLOUD_URL})
  RUDDER_CLOUD_RUNTIME          fly, byoc, or byo-vm (overrides saved local default)
  RUDDER_CLOUD_HOME_PATHS       Extra comma-separated HOME paths to include in snapshots
  RUDDER_CLOUD_REQUEST_TIMEOUT_MS  API request deadline (default ${DEFAULT_CLOUD_REQUEST_TIMEOUT_MS})
  RUDDER_CLOUD_UPLOAD_TIMEOUT_MS   Snapshot upload deadline (default ${DEFAULT_CLOUD_UPLOAD_TIMEOUT_MS})
  RUDDER_GITHUB_CLIENT_ID       GitHub App OAuth client ID for setup-github
  RUDDER_GITHUB_CLIENT_SECRET   GitHub App OAuth client secret for setup-github
  RUDDER_GOOGLE_CLIENT_ID       Google OAuth client ID for setup-google
  RUDDER_GOOGLE_CLIENT_SECRET   Google OAuth client secret for setup-google
`);
}

const CLOUD_ADJECTIVES = [
  "amber",
  "bright",
  "calm",
  "clear",
  "cosmic",
  "gentle",
  "golden",
  "lucky",
  "rapid",
  "silver",
  "steady",
  "swift",
];

const CLOUD_NOUNS = [
  "atlas",
  "harbor",
  "signal",
  "summit",
  "orbit",
  "ranger",
  "river",
  "rocket",
  "sparrow",
  "station",
  "voyager",
  "wave",
];

function randomCloudName(): string {
  const seed = Date.now() + process.pid + Math.floor(Math.random() * 1_000_000);
  return [
    CLOUD_ADJECTIVES[Math.abs(seed) % CLOUD_ADJECTIVES.length],
    CLOUD_NOUNS[Math.abs(Math.floor(seed / CLOUD_ADJECTIVES.length)) % CLOUD_NOUNS.length],
  ].join("-");
}

function cloudNameFromTask(task: string): string {
  const slug = task
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 36)
    .replace(/-+$/g, "");
  return slug || randomCloudName();
}
