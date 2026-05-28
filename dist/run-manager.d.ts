import type { BackendId, EffortLevel, RunRecord } from "./types.js";
export declare function startRun(params: {
    task: string;
    backend?: BackendId;
    model?: string;
    effort?: EffortLevel;
    detach?: boolean;
    worktree?: boolean;
    queue?: boolean;
    json?: boolean;
    exitOnComplete?: boolean;
    watchSignal?: AbortSignal;
    quiet?: boolean;
    silent?: boolean;
    view?: "default" | "shell";
}): Promise<RunRecord>;
export declare function startNativeRun(params: {
    task: string;
    tmuxSessionName: string;
    backend?: Exclude<BackendId, "acpx">;
    model?: string;
    effort?: EffortLevel;
    workerPaneId?: string;
    focus?: boolean;
    silent?: boolean;
}): Promise<RunRecord>;
export declare function startNativePlan(params: {
    task: string;
    tmuxSessionName: string;
    backend?: Exclude<BackendId, "acpx">;
    model?: string;
    effort?: EffortLevel;
    workerPaneId?: string;
    focus?: boolean;
    silent?: boolean;
}): Promise<RunRecord>;
export declare function continueRun(params: {
    runId: string;
    prompt: string;
    interrupt?: boolean;
    silent?: boolean;
}): Promise<RunRecord>;
export declare function workerRun(repoRoot: string, runId: string): Promise<void>;
export declare function writeAgentContext(repoRoot: string): Promise<void>;
export declare function statusRuns(options?: {
    json?: boolean;
}): Promise<void>;
export declare function listProjectRuns(options?: {
    json?: boolean;
}): Promise<void>;
export declare function watchRun(params: {
    repoRoot?: string;
    runId?: string;
    follow?: boolean;
    exitOnComplete?: boolean;
    signal?: AbortSignal;
    view?: "default" | "shell";
}): Promise<void>;
export declare function printLogs(runId?: string, follow?: boolean): Promise<void>;
export declare function stopRun(runId: string, options?: {
    silent?: boolean;
}): Promise<void>;
export declare function mergeRun(runId: string, allowDirty?: boolean, options?: {
    silent?: boolean;
}): Promise<RunRecord>;
export declare function syncRun(runId?: string, options?: {
    silent?: boolean;
}): Promise<RunRecord>;
export declare function deleteRun(runId: string, options?: {
    mergeFirst?: boolean;
    force?: boolean;
    silent?: boolean;
}): Promise<void>;
export declare function cleanupRuns(force?: boolean): Promise<void>;
export declare function reconcileNativeTerminals(repoRoot: string): Promise<void>;
