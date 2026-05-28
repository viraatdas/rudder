import type { MergeStrategy, RunRecord, VcsMode } from "./types.js";
export type RebaseResult = {
    success: boolean;
    baseRef: string;
    conflictedFiles: string[];
    error?: string;
};
export declare function findRepoRoot(cwd?: string): string;
export declare function isGitRepo(cwd: string): boolean;
export declare function isJjRepo(cwd: string): boolean;
export declare function findJjRoot(cwd: string): string;
export declare function detectVcsMode(repoRoot: string, configured?: VcsMode): VcsMode;
export declare function currentBranch(repoRoot: string): Promise<string>;
export declare function currentCommit(repoRoot: string): Promise<string>;
export declare function currentJjChangeId(workspacePath: string): Promise<string>;
export declare function worktreeBaseCommit(repoRoot: string): Promise<string>;
export declare function gitStatus(repoRoot: string): Promise<string[]>;
export declare function gitDiff(repoRoot: string): Promise<string>;
export declare function hasChanges(repoRoot: string): Promise<boolean>;
export declare function jjStatus(workspacePath: string): Promise<string[]>;
export declare function parseJjStatus(stdout: string): string[];
export declare function jjDiff(workspacePath: string): Promise<string>;
export declare function workspaceStatus(run: RunRecord): Promise<string[]>;
export declare function workspaceDiff(run: RunRecord): Promise<string>;
export declare function runHasChanges(run: RunRecord): Promise<boolean>;
export declare function processAlive(pid: number | undefined): boolean;
export declare function activeRunsForCheckout(repoRoot: string, checkoutPath: string): Promise<RunRecord[]>;
export declare function createRunWorktree(params: {
    repoRoot: string;
    runId: string;
    task: string;
    baseCommit: string;
}): Promise<{
    branch: string;
    path: string;
}>;
export declare function createRunJjWorkspace(params: {
    repoRoot: string;
    runId: string;
    task: string;
}): Promise<{
    workspaceName: string;
    path: string;
}>;
export declare function removeWorktree(repoRoot: string, targetPath: string, force?: boolean): Promise<void>;
export declare function removeJjWorkspace(params: {
    repoRoot: string;
    workspaceName: string;
    workspacePath: string;
}): Promise<void>;
export declare function removeRunWorkspace(run: RunRecord, force?: boolean): Promise<void>;
export declare function mergeRunIntoCurrentBranch(run: RunRecord, allowDirty?: boolean, strategy?: MergeStrategy): Promise<RunRecord>;
export declare function syncRunWorktree(run: RunRecord, baseBranch: string): Promise<RunRecord>;
export declare function rebaseWorktreeOntoBase(params: {
    repoRoot: string;
    worktreePath: string;
    baseBranch: string;
}): Promise<RebaseResult>;
export declare function resolveRebaseBaseRef(repoRoot: string, baseBranch: string): Promise<string>;
export declare function mergeJjRunIntoCurrentWorkspace(run: RunRecord, allowDirty?: boolean): Promise<RunRecord>;
export declare function conflictedFiles(repoRoot: string): Promise<string[]>;
export declare function jjConflictedFiles(workspacePath: string): Promise<string[]>;
export declare function parseJjConflictedFiles(stdout: string): string[];
export declare function worktreeList(repoRoot: string): Promise<string>;
