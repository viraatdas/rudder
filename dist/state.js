import path from "node:path";
import fsp from "node:fs/promises";
import { ensureDir, newRunId, nowIso, readJson, rudderHome, shortHash, slugPrefix, slugify, writeJson, } from "./util.js";
import { llmSummarizeTask, summarizeTask } from "./task-summary.js";
export function globalConfigPath() {
    return path.join(rudderHome(), "config.json");
}
export function authStorePath() {
    return path.join(rudderHome(), "auth-profiles.json");
}
export function cloudAuthPath() {
    return path.join(rudderHome(), "cloud.json");
}
export function projectStateDir(repoRoot) {
    return path.join(repoRoot, ".rudder");
}
export function runsDir(repoRoot) {
    return path.join(projectStateDir(repoRoot), "runs");
}
export function runDir(repoRoot, runId) {
    return path.join(runsDir(repoRoot), runId);
}
export function runRecordPath(repoRoot, runId) {
    return path.join(runDir(repoRoot, runId), "run.json");
}
export function eventsPath(repoRoot, runId) {
    return path.join(runDir(repoRoot, runId), "events.ndjson");
}
export function outputPath(repoRoot, runId) {
    return path.join(runDir(repoRoot, runId), "output.txt");
}
export function agentContextPath(repoRoot) {
    return path.join(repoRoot, "RUDDER.md");
}
export function specPath(repoRoot, runId) {
    return path.join(runDir(repoRoot, runId), "spec.json");
}
export function verifierPath(repoRoot, runId) {
    return path.join(runDir(repoRoot, runId), "verifier.json");
}
export function worktreePath(repoRoot, runId, task) {
    const parent = path.dirname(repoRoot);
    const repoName = `${slugify(path.basename(repoRoot), "repo")}-${shortHash(repoRoot)}`;
    return path.join(parent, ".rudder-worktrees", repoName, worktreeDirName(runId, task));
}
function worktreeDirName(runId, task) {
    const slug = slugPrefix(task ?? runId, "task");
    const suffix = shortHash(runId).slice(0, 8);
    return `${slug}-${suffix}`;
}
export async function loadConfig() {
    const existing = await readJson(globalConfigPath());
    if (existing?.version === 1) {
        return normalizeConfig(existing);
    }
    return defaultConfig();
}
export function defaultConfig() {
    return {
        version: 1,
        defaultBackend: "claude",
        mergeStrategy: "merge",
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
    };
}
function normalizeConfig(existing) {
    const defaults = defaultConfig();
    return {
        ...defaults,
        ...existing,
        mergeStrategy: parseMergeStrategy(existing.mergeStrategy),
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
    };
}
function parseMergeStrategy(value) {
    return value === "rebase" ? "rebase" : "merge";
}
export async function saveConfig(config) {
    await writeJson(globalConfigPath(), config, { mode: 0o600 });
}
export async function rememberBackendSelection(params) {
    const config = await loadConfig();
    applyBackendSelection(config, params);
    await saveConfig(config);
    return config;
}
function applyBackendSelection(config, params) {
    config.lastUsedBackend = params.backend;
    config.backends = config.backends ?? {};
    if (!params.updateModel && !params.updateEffort) {
        return;
    }
    const next = { ...(config.backends[params.backend] ?? {}) };
    if (params.updateModel) {
        if (params.model) {
            next.model = params.model;
        }
        else {
            delete next.model;
        }
    }
    if (params.updateEffort) {
        if (params.backend === "claude") {
            if (params.effort) {
                next.effort = params.effort;
            }
            else {
                delete next.effort;
            }
        }
        else if (params.effort) {
            next.reasoningEffort = params.effort;
        }
        else {
            delete next.reasoningEffort;
        }
    }
    config.backends[params.backend] = next;
}
export async function loadAuthStore() {
    const existing = await readJson(authStorePath());
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
function normalizeProfiles(profiles) {
    const normalized = {};
    for (const [profileId, raw] of Object.entries(profiles)) {
        if (!raw || typeof raw !== "object") {
            continue;
        }
        const entry = { ...raw };
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
        if (entry.type === "oauth" &&
            typeof entry.provider === "string" &&
            typeof entry.access === "string" &&
            typeof entry.refresh === "string" &&
            typeof entry.expires === "number") {
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
export async function saveAuthStore(store) {
    await writeJson(authStorePath(), store, { mode: 0o600 });
}
export async function createRunRecord(params) {
    const id = params.id ?? newRunId(params.task);
    const createdAt = nowIso();
    const record = {
        id,
        status: "created",
        vcs: params.vcs,
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
export async function saveRunRecord(record) {
    record.taskSummary = record.taskSummary || summarizeTask(record.task);
    record.updatedAt = nowIso();
    await writeJson(runRecordPath(record.repoRoot, record.id), record);
}
const inflightLlmSummaries = new Set();
export async function loadRunRecord(repoRoot, runId) {
    const record = await readJson(runRecordPath(repoRoot, runId));
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
export async function backfillLlmTaskSummaries(repoRoot, maxInFlight = 8) {
    try {
        const dir = runsDir(repoRoot);
        const entries = await fsp.readdir(dir, { withFileTypes: true });
        const ids = entries.filter((e) => e.isDirectory()).map((e) => e.name);
        const slice = ids.slice(0, maxInFlight);
        for (const id of slice) {
            const record = await readJson(runRecordPath(repoRoot, id));
            if (record) {
                maybeBackgroundLlmSummarize(record);
            }
        }
    }
    catch {
        // ignore — best-effort
    }
}
function maybeBackgroundLlmSummarize(record) {
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
            const fresh = await readJson(runRecordPath(record.repoRoot, record.id));
            if (!fresh) {
                return;
            }
            if (fresh.taskSummaryLlm) {
                return;
            }
            fresh.taskSummary = title;
            fresh.taskSummaryLlm = true;
            await saveRunRecord(fresh);
        }
        catch {
            // swallow — background best-effort
        }
        finally {
            inflightLlmSummaries.delete(key);
        }
    })();
}
export async function appendEvent(repoRoot, event) {
    await ensureDir(runDir(repoRoot, event.runId));
    await fsp.appendFile(eventsPath(repoRoot, event.runId), `${JSON.stringify(event)}\n`, "utf8");
    const text = event.type === "backend.output" || event.type === "backend.error"
        ? event.message ?? (typeof event.data === "string" ? event.data : undefined)
        : undefined;
    if (text) {
        await fsp.appendFile(outputPath(repoRoot, event.runId), text, "utf8");
    }
}
export async function listRuns(repoRoot) {
    await ensureDir(runsDir(repoRoot));
    const entries = await fsp.readdir(runsDir(repoRoot), { withFileTypes: true });
    const runs = await Promise.all(entries
        .filter((entry) => entry.isDirectory())
        .map(async (entry) => await loadRunRecord(repoRoot, entry.name)));
    return runs
        .filter((run) => Boolean(run))
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
}
export async function resolveRun(repoRoot, runId) {
    if (runId) {
        return await loadRunRecord(repoRoot, runId);
    }
    const runs = await listRuns(repoRoot);
    return runs[0] ?? null;
}
//# sourceMappingURL=state.js.map