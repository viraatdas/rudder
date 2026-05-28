import type { JsonValue } from "./types.js";
export declare function nowIso(): string;
export declare function isTty(): boolean;
export declare function rudderHome(): string;
export declare function expandHome(value: string): string;
export declare function shortenHome(value: string): string;
export declare function ensureDir(dir: string): Promise<void>;
export declare function pathExists(filePath: string): Promise<boolean>;
export declare function pathExistsSync(filePath: string): boolean;
export declare function readJson<T>(filePath: string): Promise<T | null>;
export declare function writeJson(filePath: string, value: JsonValue, options?: {
    mode?: number;
}): Promise<void>;
export declare function commandExists(command: string): boolean;
export declare class MissingToolError extends Error {
    readonly tool: string;
    readonly hint: string;
    constructor(tool: string, message?: string);
}
export declare function formatMissingToolMessage(tool: string, hintOverride?: string): string;
export declare function isMissingToolSpawnError(error: unknown): boolean;
export declare function runCommand(command: string, args: string[], options?: {
    cwd?: string;
    allowFailure?: boolean;
    env?: NodeJS.ProcessEnv;
}): Promise<{
    stdout: string;
    stderr: string;
    code: number;
}>;
export declare function runCommandSync(command: string, args: string[], options?: {
    cwd?: string;
    allowFailure?: boolean;
}): {
    stdout: string;
    stderr: string;
    code: number;
};
export declare function promptText(message: string, defaultValue?: string): Promise<string>;
export declare function promptSecret(message: string): Promise<string>;
export declare function promptConfirm(message: string, defaultValue?: boolean): Promise<boolean>;
export declare function promptSelect<T extends string>(message: string, options: Array<{
    value: T;
    label: string;
    hint?: string;
}>, defaultValue: T): Promise<T>;
export declare function slugify(inputValue: string, fallback?: string): string;
export declare function slugPrefix(inputValue: string, fallback?: string, maxChars?: number): string;
export declare function newRunId(task: string): string;
export declare function shortHash(value: string): string;
export declare function shellQuote(value: string): string;
export declare function commandEnv(extra?: NodeJS.ProcessEnv): NodeJS.ProcessEnv;
export declare function parseJsonLine(line: string): JsonValue | null;
export declare function lineSplitBuffer(previous: string, chunk: string): {
    lines: string[];
    rest: string;
};
