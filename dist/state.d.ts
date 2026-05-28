import type { AuthProfileStore, BackendId, EffortLevel, RudderConfig, RunRecord, RudderEvent, VcsMode } from "./types.js";
export declare function globalConfigPath(): string;
export declare function authStorePath(): string;
export declare function cloudAuthPath(): string;
export declare function projectStateDir(repoRoot: string): string;
export declare function runsDir(repoRoot: string): string;
export declare function runDir(repoRoot: string, runId: string): string;
export declare function runRecordPath(repoRoot: string, runId: string): string;
export declare function eventsPath(repoRoot: string, runId: string): string;
export declare function outputPath(repoRoot: string, runId: string): string;
export declare function agentContextPath(repoRoot: string): string;
export declare function specPath(repoRoot: string, runId: string): string;
export declare function verifierPath(repoRoot: string, runId: string): string;
export declare function worktreePath(repoRoot: string, runId: string, task?: string): string;
export declare function loadConfig(): Promise<RudderConfig>;
export declare function defaultConfig(): RudderConfig;
export declare function saveConfig(config: RudderConfig): Promise<void>;
export declare function rememberBackendSelection(params: {
    backend: BackendId;
    model?: string;
    effort?: EffortLevel;
    updateModel?: boolean;
    updateEffort?: boolean;
}): Promise<RudderConfig>;
export declare function loadAuthStore(): Promise<AuthProfileStore>;
export declare function saveAuthStore(store: AuthProfileStore): Promise<void>;
export declare function createRunRecord(params: {
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
    useWorktree: boolean;
    worktreeBranch?: string;
    worktreeWorkspaceName?: string;
    worktreePath?: string;
}): Promise<RunRecord>;
export declare function saveRunRecord(record: RunRecord): Promise<void>;
export declare function loadRunRecord(repoRoot: string, runId: string): Promise<RunRecord | null>;
/**
 * Scan every run record in the repo and fire a background LLM summarization
 * for any whose task summary has never been upgraded. Used by the CLI before
 * spawning the native dashboard so the next launch picks up nicer titles even
 * though the native dashboard reads run.json directly and skips the TS load
 * path. Caps the number of in-flight summaries so a big repo doesn't blast
 * Anthropic. Never throws.
 */
export declare function backfillLlmTaskSummaries(repoRoot: string, maxInFlight?: number): Promise<void>;
export declare function appendEvent(repoRoot: string, event: RudderEvent): Promise<void>;
export declare function listRuns(repoRoot: string): Promise<RunRecord[]>;
export declare function resolveRun(repoRoot: string, runId?: string): Promise<RunRecord | null>;
