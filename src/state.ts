import path from "node:path";
import fsp from "node:fs/promises";
import type {
  AuthProfileStore,
  BackendConfig,
  BackendId,
  EffortLevel,
  JsonValue,
  MergeStrategy,
  ProjectEntry,
  ProjectsRegistry,
  RudderConfig,
  RunRecord,
  RudderEvent,
  UndoEntry,
  VcsMode,
} from "./types.js";
import { DEFAULT_BOARD_PORT } from "./types.js";
import {
  ensureDir,
  newRunId,
  nowIso,
  readJson,
  rudderHome,
  runCommand,
  shortHash,
  slugPrefix,
  slugify,
  updateJson,
  writeJson,
} from "./util.js";
import { llmSummarizeTask, summarizeTask } from "./task-summary.js";

const OUTPUT_TXT_MAX_BYTES = 2 * 1024 * 1024;

export function globalConfigPath(): string {
  return path.join(rudderHome(), "config.json");
}

export function projectsRegistryPath(): string {
  return path.join(rudderHome(), "projects.json");
}

export function authStorePath(): string {
  return path.join(rudderHome(), "auth-profiles.json");
}

export function cloudAuthPath(): string {
  return path.join(rudderHome(), "cloud.json");
}

export function projectStateDir(repoRoot: string): string {
  return path.join(repoRoot, ".rudder");
}

const PROJECT_RUNTIME_IGNORES = [
  ".rudder/",
  ".rudder-worktrees/",
  "RUDDER.md",
  "RUDDER_SHARED.md",
];
const projectRuntimeIgnoreInstalls = new Map<string, Promise<void>>();

/**
 * Keep Rudder's live control plane out of every jj snapshot. This must run
 * before creating scaffold changes or workspaces: adding the ignores after a
 * workspace command is too late because jj snapshots the current checkout as
 * part of that command.
 *
 * We write both .gitignore (the existing, visible project contract) and Git's
 * local exclude file. The latter applies immediately across colocated jj
 * workspaces, including a scaffold change created before an older Rudder
 * version had installed the project-level rules.
 */
export function ensureProjectRuntimeIgnored(repoRoot: string): Promise<void> {
  const key = path.resolve(repoRoot);
  const existing = projectRuntimeIgnoreInstalls.get(key);
  if (existing) return existing;
  const install = installProjectRuntimeIgnores(key).catch((error) => {
    projectRuntimeIgnoreInstalls.delete(key);
    throw error;
  });
  projectRuntimeIgnoreInstalls.set(key, install);
  return install;
}

async function installProjectRuntimeIgnores(repoRoot: string): Promise<void> {
  const targets = [path.join(repoRoot, ".gitignore")];
  const exclude = await runCommand("git", ["rev-parse", "--git-path", "info/exclude"], {
    cwd: repoRoot,
    allowFailure: true,
  }).catch(() => null);
  const excludePath = exclude?.stdout.trim();
  if (excludePath) {
    targets.push(path.isAbsolute(excludePath) ? excludePath : path.resolve(repoRoot, excludePath));
  }
  for (const target of new Set(targets)) {
    for (const line of PROJECT_RUNTIME_IGNORES) {
      await ensureIgnoreLine(target, line);
    }
  }
}

async function ensureIgnoreLine(filePath: string, line: string): Promise<void> {
  const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
  if (existing.split(/\r?\n/).some((item) => item.trim() === line)) {
    return;
  }
  const prefix = existing && !existing.endsWith("\n") ? "\n" : "";
  await ensureDir(path.dirname(filePath));
  await fsp.appendFile(filePath, `${prefix}${line}\n`, "utf8");
}

export function runsDir(repoRoot: string): string {
  return path.join(projectStateDir(repoRoot), "runs");
}

export function undoStackPath(repoRoot: string): string {
  return path.join(projectStateDir(repoRoot), "undo-stack.json");
}

export function runDir(repoRoot: string, runId: string): string {
  return path.join(runsDir(repoRoot), runId);
}

export function runRecordPath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "run.json");
}

export function eventsPath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "events.ndjson");
}

export function outputPath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "output.txt");
}

export function agentContextPath(repoRoot: string): string {
  return path.join(repoRoot, "RUDDER.md");
}

export function specPath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "spec.json");
}

export function verifierPath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "verifier.json");
}

export function worktreePath(repoRoot: string, runId: string, task?: string): string {
  // Worktrees live INSIDE the project (gitignored .rudder-worktrees/), not in the parent
  // directory. This keeps every Rudder path within the project boundary, so a planner or
  // agent confined to the project never reads outside it — which is what triggered
  // Claude's "allow reading outside the project?" permission prompt.
  const repoName = `${slugify(path.basename(repoRoot), "repo")}-${shortHash(repoRoot)}`;
  return path.join(repoRoot, ".rudder-worktrees", repoName, worktreeDirName(runId, task));
}

function worktreeDirName(runId: string, task?: string): string {
  const slug = slugPrefix(task ?? runId, "task");
  const suffix = shortHash(runId).slice(0, 8);
  return `${slug}-${suffix}`;
}

export async function loadConfig(): Promise<RudderConfig> {
  const existing = await readJson<RudderConfig>(globalConfigPath());
  if (existing?.version === 1) {
    return normalizeConfig(existing);
  }
  return defaultConfig();
}

export function defaultConfig(): RudderConfig {
  return {
    version: 1,
    defaultBackend: "claude",
    mergeStrategy: "merge",
    colorMode: "terminal",
    runPolicy: {
      sameCheckout: "single-active",
      concurrentPromptMode: "worktree",
      mergeMode: "manual-on-conflict",
    },
    acpx: { install: "latest" },
    backends: {
      claude: { profileId: "anthropic:claude-code", model: "sonnet" },
      codex: {
        profileId: "openai-codex:default",
        model: "gpt-5.5",
      },
      acpx: { model: "gpt-5.5" },
    },
    board: { port: DEFAULT_BOARD_PORT },
    orchestrator: { maxParallel: 1000, reviewGate: "manual" },
  };
}

function normalizeConfig(existing: RudderConfig): RudderConfig {
  const defaults = defaultConfig();
  return {
    ...defaults,
    ...existing,
    mergeStrategy: parseMergeStrategy(existing.mergeStrategy),
    colorMode: existing.colorMode === "paper" ? "paper" : "terminal",
    runPolicy: {
      ...defaults.runPolicy,
      ...(existing.runPolicy ?? {}),
    },
    acpx: {
      ...defaults.acpx,
      ...(existing.acpx ?? {}),
    },
    backends: {
      ...defaults.backends,
      ...(existing.backends ?? {}),
    },
    board: {
      ...defaults.board,
      ...(existing.board ?? {}),
    },
    orchestrator: {
      maxParallel: existing.orchestrator?.maxParallel ?? defaults.orchestrator?.maxParallel ?? 1000,
      reviewGate: existing.orchestrator?.reviewGate ?? defaults.orchestrator?.reviewGate ?? "manual",
      ...(existing.orchestrator?.budget ? { budget: existing.orchestrator.budget } : {}),
    },
  };
}

function parseMergeStrategy(value: unknown): MergeStrategy {
  return value === "rebase" ? "rebase" : "merge";
}

export async function saveConfig(config: RudderConfig): Promise<void> {
  await writeJson(globalConfigPath(), config, { mode: 0o600 });
}

export async function rememberBackendSelection(params: {
  backend: BackendId;
  model?: string;
  effort?: EffortLevel;
  updateModel?: boolean;
  updateEffort?: boolean;
}): Promise<RudderConfig> {
  const config = await loadConfig();
  applyBackendSelection(config, params);
  await saveConfig(config);
  return config;
}

function applyBackendSelection(
  config: RudderConfig,
  params: {
    backend: BackendId;
    model?: string;
    effort?: EffortLevel;
    updateModel?: boolean;
    updateEffort?: boolean;
  },
): void {
  config.lastUsedBackend = params.backend;
  config.backends = config.backends ?? {};
  if (!params.updateModel && !params.updateEffort) {
    return;
  }

  const next: BackendConfig = { ...(config.backends[params.backend] ?? {}) };
  if (params.updateModel) {
    if (params.model) {
      next.model = params.model;
    } else {
      delete next.model;
    }
  }
  if (params.updateEffort) {
    if (params.backend === "claude") {
      if (params.effort) {
        next.effort = params.effort;
      } else {
        delete next.effort;
      }
    } else if (params.effort) {
      next.reasoningEffort = params.effort;
    } else {
      delete next.reasoningEffort;
    }
  }
  config.backends[params.backend] = next;
}

export async function loadAuthStore(): Promise<AuthProfileStore> {
  const existing = await readJson<AuthProfileStore>(authStorePath());
  if (existing?.version === 1 && existing.profiles && typeof existing.profiles === "object") {
    return {
      version: 1,
      profiles: normalizeProfiles(existing.profiles),
      order: existing.order,
      lastGood: existing.lastGood,
      usageStats: existing.usageStats,
    };
  }
  return { version: 1, profiles: {} };
}

function normalizeProfiles(
  profiles: Record<string, unknown>,
): AuthProfileStore["profiles"] {
  const normalized: AuthProfileStore["profiles"] = {};
  for (const [profileId, raw] of Object.entries(profiles)) {
    if (!raw || typeof raw !== "object") {
      continue;
    }
    const entry = { ...(raw as Record<string, unknown>) };
    if (!entry.type && typeof entry.mode === "string") {
      entry.type = entry.mode;
    }
    if (entry.type === "api_key" && typeof entry.provider === "string") {
      normalized[profileId] = {
        type: "api_key",
        provider: entry.provider,
        ...(typeof entry.key === "string" ? { key: entry.key } : {}),
        ...(typeof entry.apiKey === "string" ? { key: entry.apiKey } : {}),
      };
      continue;
    }
    if (entry.type === "token" && typeof entry.provider === "string") {
      normalized[profileId] = {
        type: "token",
        provider: entry.provider,
        ...(typeof entry.token === "string" ? { token: entry.token } : {}),
        ...(typeof entry.expires === "number" ? { expires: entry.expires } : {}),
      };
      continue;
    }
    if (
      entry.type === "oauth" &&
      typeof entry.provider === "string" &&
      typeof entry.access === "string" &&
      typeof entry.refresh === "string" &&
      typeof entry.expires === "number"
    ) {
      normalized[profileId] = {
        type: "oauth",
        provider: entry.provider,
        access: entry.access,
        refresh: entry.refresh,
        expires: entry.expires,
        ...(typeof entry.email === "string" ? { email: entry.email } : {}),
        ...(typeof entry.accountId === "string" ? { accountId: entry.accountId } : {}),
      };
    }
  }
  return normalized;
}

export async function saveAuthStore(store: AuthProfileStore): Promise<void> {
  await writeJson(authStorePath(), store, { mode: 0o600 });
}

export async function createRunRecord(params: {
  id?: string;
  repoRoot: string;
  task: string;
  backend: RunRecord["backend"];
  model?: string;
  effort?: RunRecord["effort"];
  mode?: RunRecord["mode"];
  targetBranch: string;
  baseCommit: string;
  vcs?: VcsMode;
  resolverFor?: string;
  useWorktree: boolean;
  worktreeBranch?: string;
  worktreeWorkspaceName?: string;
  worktreeJjChangeId?: string;
  worktreePath?: string;
}): Promise<RunRecord> {
  const id = params.id ?? newRunId(params.task);
  const createdAt = nowIso();
  const record: RunRecord = {
    id,
    status: "created",
    vcs: params.vcs,
    ...(params.resolverFor ? { resolverFor: params.resolverFor } : {}),
    mode: params.mode ?? "execute",
    task: params.task,
    taskSummary: summarizeTask(params.task),
    backend: params.backend,
    model: params.model,
    effort: params.effort,
    createdAt,
    updatedAt: createdAt,
    repoRoot: params.repoRoot,
    targetBranch: params.targetBranch,
    baseCommit: params.baseCommit,
    worktree: {
      enabled: params.useWorktree,
      path: params.worktreePath ?? params.repoRoot,
      branch: params.worktreeBranch,
      workspaceName: params.worktreeWorkspaceName,
      ...(params.worktreeJjChangeId ? { jjChangeId: params.worktreeJjChangeId } : {}),
    },
    currentPrompt: params.task,
    turns: [{ ts: createdAt, prompt: params.task, source: "user" }],
    lastUserInputAt: createdAt,
    autoSteer: { count: 0, max: 2 },
  };
  await ensureDir(runDir(params.repoRoot, id));
  await saveRunRecord(record);
  return record;
}

export async function saveRunRecord(
  record: RunRecord,
  options?: { expectedAttemptId?: string; expectedStartedAt?: string },
): Promise<boolean> {
  record.taskSummary = record.taskSummary || summarizeTask(record.task);
  record.updatedAt = nowIso();
  let saved = false;
  await updateJson<RunRecord>(runRecordPath(record.repoRoot, record.id), (prev) => {
    // A redirected headless run starts a fresh attempt. Backend/controller
    // callbacks from the superseded process may still arrive briefly after the
    // signal; keep those stale snapshots from overwriting the new turn.
    if (
      options?.expectedAttemptId &&
      prev &&
      prev.process?.attemptId !== options.expectedAttemptId
    ) {
      return prev;
    }
    // Runs created by an older Rudder have no attempt id. Their worker still
    // carries the original startedAt, so use it as a migration-safe ownership
    // token and reject it once a redirect installs a new attempt.
    if (
      !options?.expectedAttemptId &&
      options?.expectedStartedAt &&
      prev &&
      (Boolean(prev.process?.attemptId) || prev.process?.startedAt !== options.expectedStartedAt)
    ) {
      return prev;
    }
    // A long-lived in-memory `record` can be stale: the background LLM
    // summarizer reads + rewrites run.json out of band. Preserve the title it
    // persisted instead of clobbering it with the naive summary.
    if (prev?.taskSummaryLlm && !record.taskSummaryLlm) {
      record.taskSummary = prev.taskSummary;
      record.taskSummaryLlm = true;
    }
    saved = true;
    return record;
  });
  return saved;
}

const inflightLlmSummaries = new Set<string>();

export async function loadRunRecord(repoRoot: string, runId: string): Promise<RunRecord | null> {
  const record = await readJson<RunRecord>(runRecordPath(repoRoot, runId));
  if (record && !record.taskSummary) {
    record.taskSummary = summarizeTask(record.task);
  }
  if (record) {
    maybeBackgroundLlmSummarize(record);
  }
  return record;
}

/**
 * Scan every run record in the repo and fire a background LLM summarization
 * for any whose task summary has never been upgraded. Used by the CLI before
 * spawning the native dashboard so the next launch picks up nicer titles even
 * though the native dashboard reads run.json directly and skips the TS load
 * path. Caps the number of in-flight summaries so a big repo doesn't blast
 * Anthropic. Never throws.
 */
export async function backfillLlmTaskSummaries(repoRoot: string, maxInFlight = 8): Promise<void> {
  try {
    const dir = runsDir(repoRoot);
    const entries = await fsp.readdir(dir, { withFileTypes: true });
    const ids = entries.filter((e) => e.isDirectory()).map((e) => e.name);
    const slice = ids.slice(0, maxInFlight);
    for (const id of slice) {
      const record = await readJson<RunRecord>(runRecordPath(repoRoot, id));
      if (record) {
        maybeBackgroundLlmSummarize(record);
      }
    }
  } catch {
    // ignore — best-effort
  }
}

function maybeBackgroundLlmSummarize(record: RunRecord): void {
  if (record.taskSummaryLlm) {
    return;
  }
  const task = (record.task ?? "").trim();
  if (!task) {
    return;
  }
  const naive = summarizeTask(record.task);
  const current = (record.taskSummary ?? "").trim();
  if (current && current !== naive) {
    // user (or some other path) already set a non-naive title; skip
    return;
  }
  const key = `${record.repoRoot}::${record.id}`;
  if (inflightLlmSummaries.has(key)) {
    return;
  }
  inflightLlmSummaries.add(key);
  (async () => {
    try {
      const title = await llmSummarizeTask(record.task);
      if (!title) {
        return;
      }
      const file = runRecordPath(record.repoRoot, record.id);
      await updateJson<RunRecord>(file, (fresh) => {
        if (!fresh) return undefined;
        if (fresh.taskSummaryLlm) return fresh as unknown as JsonValue;
        const currentNaive = summarizeTask(fresh.task);
        const currentTitle = (fresh.taskSummary ?? "").trim();
        if (currentTitle && currentTitle !== currentNaive) {
          return fresh as unknown as JsonValue;
        }
        // Merge only the summary fields into the record read under the
        // cross-process lock. Never rewrite status/turn/process ownership from
        // the stale snapshot that initiated the LLM request.
        fresh.taskSummary = title;
        fresh.taskSummaryLlm = true;
        fresh.updatedAt = nowIso();
        return fresh as unknown as JsonValue;
      });
    } catch {
      // swallow — background best-effort
    } finally {
      inflightLlmSummaries.delete(key);
    }
  })();
}

export async function appendEvent(repoRoot: string, event: RudderEvent): Promise<void> {
  await ensureDir(runDir(repoRoot, event.runId));
  await fsp.appendFile(eventsPath(repoRoot, event.runId), `${JSON.stringify(event)}\n`, "utf8");
  const text =
    event.type === "backend.output" || event.type === "backend.error"
      ? event.message ?? (typeof event.data === "string" ? event.data : undefined)
      : undefined;
  if (text) {
    const file = outputPath(repoRoot, event.runId);
    await fsp.appendFile(file, text, "utf8");
    await trimFileTail(file, OUTPUT_TXT_MAX_BYTES);
  }
}

async function trimFileTail(file: string, maxBytes: number): Promise<void> {
  const stat = await fsp.stat(file).catch(() => null);
  if (!stat || stat.size <= maxBytes) {
    return;
  }
  const keep = Math.max(1, maxBytes);
  const handle = await fsp.open(file, "r").catch(() => null);
  if (!handle) {
    return;
  }
  try {
    const buffer = Buffer.alloc(keep);
    await handle.read(buffer, 0, keep, Math.max(0, stat.size - keep));
    const temp = `${file}.${process.pid}.${Date.now()}.tmp`;
    await fsp.writeFile(temp, buffer);
    await fsp.rename(temp, file);
  } finally {
    await handle.close().catch(() => undefined);
  }
}

export async function listRuns(repoRoot: string): Promise<RunRecord[]> {
  await ensureDir(runsDir(repoRoot));
  const entries = await fsp.readdir(runsDir(repoRoot), { withFileTypes: true });
  const runs = await Promise.all(
    entries
      .filter((entry) => entry.isDirectory())
      .map(async (entry) => await loadRunRecord(repoRoot, entry.name)),
  );
  return runs
    .filter((run): run is RunRecord => Boolean(run))
    .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
}

export async function resolveRun(repoRoot: string, runId?: string): Promise<RunRecord | null> {
  if (runId) {
    return await loadRunRecord(repoRoot, runId);
  }
  const runs = await listRuns(repoRoot);
  return runs[0] ?? null;
}

export async function loadUndoStack(repoRoot: string): Promise<UndoEntry[]> {
  const stack = await readJson<UndoEntry[]>(undoStackPath(repoRoot));
  return Array.isArray(stack) ? stack : [];
}

export async function pushUndoEntry(repoRoot: string, entry: UndoEntry): Promise<void> {
  await updateJson<UndoEntry[]>(undoStackPath(repoRoot), (current) => {
    const stack = Array.isArray(current) ? current : [];
    return [...stack, entry] as unknown as JsonValue;
  });
}

export async function popUndoEntry(repoRoot: string): Promise<UndoEntry | null> {
  let popped: UndoEntry | null = null;
  await updateJson<UndoEntry[]>(undoStackPath(repoRoot), (current) => {
    const stack = Array.isArray(current) ? [...current] : [];
    popped = stack.pop() ?? null;
    return stack as unknown as JsonValue;
  });
  return popped;
}

// ---------------------------------------------------------------------------
// Multi-project registry (~/.rudder/projects.json). The localhost board serves
// every registered repo; runs auto-register their repo so projects appear.
// ---------------------------------------------------------------------------

export async function loadProjects(): Promise<ProjectEntry[]> {
  const registry = await readJson<ProjectsRegistry>(projectsRegistryPath());
  if (registry?.version === 1 && Array.isArray(registry.projects)) {
    return registry.projects.filter(
      (entry): entry is ProjectEntry =>
        Boolean(entry) && typeof entry.slug === "string" && typeof entry.repoRoot === "string",
    );
  }
  return [];
}

/**
 * Compute a stable slug for a repo: slugify(basename) with a -<shortHash> suffix
 * appended only when another already-registered repo claimed the bare slug.
 */
function computeProjectSlug(repoRoot: string, existing: ProjectEntry[]): string {
  const resolved = path.resolve(repoRoot);
  const base = slugify(path.basename(resolved), "project");
  const collision = existing.some(
    (entry) => entry.slug === base && path.resolve(entry.repoRoot) !== resolved,
  );
  return collision ? `${base}-${shortHash(resolved)}` : base;
}

/**
 * Register the given repo in the global registry. Idempotent: an existing entry
 * for the same repoRoot is returned unchanged (its slug is stable). Never throws
 * on a malformed registry (it is rebuilt from the known-good entries).
 */
export async function registerProject(repoRoot: string): Promise<ProjectEntry> {
  const resolved = path.resolve(repoRoot);
  let result: ProjectEntry | null = null;
  await updateJson<ProjectsRegistry>(projectsRegistryPath(), (current) => {
    const projects =
      current?.version === 1 && Array.isArray(current.projects)
        ? current.projects.filter(
            (entry): entry is ProjectEntry =>
              Boolean(entry) && typeof entry.slug === "string" && typeof entry.repoRoot === "string",
          )
        : [];
    const found = projects.find((entry) => path.resolve(entry.repoRoot) === resolved);
    if (found) {
      result = found;
      return { version: 1, projects } as unknown as JsonValue;
    }
    const entry: ProjectEntry = {
      slug: computeProjectSlug(resolved, projects),
      repoRoot: resolved,
      name: path.basename(resolved),
      addedAt: nowIso(),
    };
    result = entry;
    return { version: 1, projects: [...projects, entry] } as unknown as JsonValue;
  });
  if (!result) {
    // Should be unreachable; the transform always assigns result.
    result = {
      slug: computeProjectSlug(resolved, []),
      repoRoot: resolved,
      name: path.basename(resolved),
      addedAt: nowIso(),
    };
  }
  return result;
}

export async function findProjectBySlug(slug: string): Promise<ProjectEntry | null> {
  const projects = await loadProjects();
  return projects.find((entry) => entry.slug === slug) ?? null;
}
