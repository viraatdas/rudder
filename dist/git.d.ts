import type { MergeStrategy, RunRecord } from "./types.js";
export type RebaseResult = {
    success: boolean;
    baseRef: string;
    conflictedFiles: string[];
    error?: string;
};
export declare function findRepoRoot(cwd?: string): string;
export declare function isGitRepo(cwd: string): boolean;
export declare function currentBranch(repoRoot: string): Promise<string>;
export declare function currentCommit(repoRoot: string): Promise<string>;
export declare function worktreeBaseCommit(repoRoot: string): Promise<string>;
export declare function gitStatus(repoRoot: string): Promise<string[]>;
export declare function gitDiff(repoRoot: string): Promise<string>;
export declare function hasChanges(repoRoot: string): Promise<boolean>;
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
export declare function removeWorktree(repoRoot: string, targetPath: string, force?: boolean): Promise<void>;
export declare function mergeRunIntoCurrentBranch(run: RunRecord, allowDirty?: boolean, strategy?: MergeStrategy): Promise<RunRecord>;
export declare function syncRunWorktree(run: RunRecord, baseBranch: string): Promise<RunRecord>;
export declare function rebaseWorktreeOntoBase(params: {
    repoRoot: string;
    worktreePath: string;
    baseBranch: string;
}): Promise<RebaseResult>;
export declare function resolveRebaseBaseRef(repoRoot: string, baseBranch: string): Promise<string>;
export declare function conflictedFiles(repoRoot: string): Promise<string[]>;
export declare function worktreeList(repoRoot: string): Promise<string>;
