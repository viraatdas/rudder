import { spawn, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import readline from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import type { JsonValue } from "./types.js";

const DEFAULT_PATH = [
  "/opt/homebrew/bin",
  "/opt/homebrew/sbin",
  "/usr/local/bin",
  "/usr/bin",
  "/bin",
  "/usr/sbin",
  "/sbin",
].join(":");

export function nowIso(): string {
  return new Date().toISOString();
}

export function isTty(): boolean {
  return Boolean(process.stdin.isTTY && process.stdout.isTTY);
}

export function rudderHome(): string {
  return path.resolve(process.env.RUDDER_HOME?.trim() || path.join(os.homedir(), ".rudder"));
}

export function expandHome(value: string): string {
  if (value === "~") {
    return os.homedir();
  }
  if (value.startsWith("~/")) {
    return path.join(os.homedir(), value.slice(2));
  }
  return value;
}

export function shortenHome(value: string): string {
  const home = os.homedir();
  return value === home || value.startsWith(`${home}${path.sep}`)
    ? `~${value.slice(home.length)}`
    : value;
}

export async function ensureDir(dir: string): Promise<void> {
  await fsp.mkdir(dir, { recursive: true });
}

export async function pathExists(filePath: string): Promise<boolean> {
  try {
    await fsp.access(filePath);
    return true;
  } catch {
    return false;
  }
}

export function pathExistsSync(filePath: string): boolean {
  try {
    fs.accessSync(filePath);
    return true;
  } catch {
    return false;
  }
}

export async function readJson<T>(filePath: string): Promise<T | null> {
  try {
    const raw = await fsp.readFile(filePath, "utf8");
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

export async function writeJson(
  filePath: string,
  value: JsonValue,
  options?: { mode?: number },
): Promise<void> {
  await ensureDir(path.dirname(filePath));
  const temp = `${filePath}.${process.pid}.${Date.now()}.tmp`;
  await fsp.writeFile(temp, `${JSON.stringify(value, null, 2)}\n`, {
    encoding: "utf8",
    mode: options?.mode ?? 0o644,
  });
  await fsp.rename(temp, filePath);
  if (options?.mode !== undefined) {
    await fsp.chmod(filePath, options.mode);
  }
}

export function commandExists(command: string): boolean {
  const result = spawnSync("sh", ["-c", `command -v ${shellQuote(command)}`], {
    encoding: "utf8",
    env: commandEnv(),
  });
  return result.status === 0 && result.stdout.trim().length > 0;
}

const TOOL_INSTALL_HINTS: Record<string, string> = {
  claude: "Install with `npm install -g @anthropic-ai/claude-code` (see https://github.com/anthropics/claude-code).",
  codex: "Install with `npm install -g @openai/codex` (see https://github.com/openai/codex).",
  acpx: "Install with `npm install -g acpx@latest` or run `rudder onboard`.",
  jj: "Install Jujutsu and ensure `jj` is on PATH.",
};

export class MissingToolError extends Error {
  readonly tool: string;
  readonly hint: string;

  constructor(tool: string, message?: string) {
    const hint = TOOL_INSTALL_HINTS[tool] ?? "Please install it and ensure it is on your PATH.";
    super(message ?? formatMissingToolMessage(tool, hint));
    this.name = "MissingToolError";
    this.tool = tool;
    this.hint = hint;
  }
}

export function formatMissingToolMessage(tool: string, hintOverride?: string): string {
  const hint = hintOverride ?? TOOL_INSTALL_HINTS[tool] ?? "Please install it and ensure it is on your PATH.";
  return `${tool} is not installed or not found on PATH. ${hint}`;
}

export function isMissingToolSpawnError(error: unknown): boolean {
  if (!error || typeof error !== "object") {
    return false;
  }
  const code = (error as { code?: unknown }).code;
  return code === "ENOENT";
}

export async function runCommand(
  command: string,
  args: string[],
  options?: { cwd?: string; allowFailure?: boolean; env?: NodeJS.ProcessEnv },
): Promise<{ stdout: string; stderr: string; code: number }> {
  return await new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: options?.cwd,
      env: commandEnv(options?.env),
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => {
      const exitCode = code ?? 1;
      if (exitCode !== 0 && !options?.allowFailure) {
        reject(new Error(`${command} ${args.join(" ")} failed: ${stderr.trim() || stdout.trim()}`));
        return;
      }
      resolve({ stdout, stderr, code: exitCode });
    });
  });
}

export function runCommandSync(
  command: string,
  args: string[],
  options?: { cwd?: string; allowFailure?: boolean },
): { stdout: string; stderr: string; code: number } {
  const result = spawnSync(command, args, {
    cwd: options?.cwd,
    env: commandEnv(),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const code = result.status ?? 1;
  if (code !== 0 && !options?.allowFailure) {
    throw new Error(`${command} ${args.join(" ")} failed: ${result.stderr || result.stdout}`);
  }
  return {
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
    code,
  };
}

export async function promptText(message: string, defaultValue?: string): Promise<string> {
  const rl = readline.createInterface({ input, output });
  try {
    const suffix = defaultValue ? ` [${defaultValue}]` : "";
    const answer = await rl.question(`${message}${suffix}: `);
    const trimmed = answer.trim();
    return trimmed || defaultValue || "";
  } finally {
    rl.close();
  }
}

export async function promptSecret(message: string): Promise<string> {
  if (!isTty()) {
    return "";
  }
  const rl = readline.createInterface({ input, output });
  output.write(`${message}: `);
  const shouldDisableEcho = process.platform !== "win32";
  try {
    if (shouldDisableEcho) {
      spawnSync("stty", ["-echo"], { stdio: ["inherit", "ignore", "ignore"] });
    }
    const answer = await rl.question("");
    return answer.trim();
  } finally {
    if (shouldDisableEcho) {
      spawnSync("stty", ["echo"], { stdio: ["inherit", "ignore", "ignore"] });
    }
    output.write("\n");
    rl.close();
  }
}

export async function promptConfirm(message: string, defaultValue = true): Promise<boolean> {
  const defaultHint = defaultValue ? "Y/n" : "y/N";
  const raw = await promptText(`${message} (${defaultHint})`);
  if (!raw) {
    return defaultValue;
  }
  return ["y", "yes", "true", "1"].includes(raw.toLowerCase());
}

export async function promptSelect<T extends string>(
  message: string,
  options: Array<{ value: T; label: string; hint?: string }>,
  defaultValue: T,
): Promise<T> {
  console.log(message);
  options.forEach((option, index) => {
    const marker = option.value === defaultValue ? " [default]" : "";
    const hint = option.hint ? ` - ${option.hint}` : "";
    console.log(`  ${index + 1}. ${option.label}${marker}${hint}`);
  });
  const raw = await promptText("Choose", String(options.findIndex((o) => o.value === defaultValue) + 1));
  const byIndex = options[Number(raw) - 1];
  if (byIndex) {
    return byIndex.value;
  }
  const byValue = options.find((option) => option.value === raw);
  return byValue?.value ?? defaultValue;
}

export function slugify(inputValue: string, fallback = "task"): string {
  const slug = inputValue
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  return slug || fallback;
}

export function slugPrefix(inputValue: string, fallback = "task", maxChars = 40): string {
  const slug = slugify(inputValue, fallback).slice(0, maxChars).replace(/-+$/g, "");
  return slug || fallback;
}

export function newRunId(task: string): string {
  const stamp = new Date().toISOString().replace(/[-:.TZ]/g, "").slice(0, 14);
  return `${stamp}-${slugify(task)}-${randomUUID().slice(0, 8)}`;
}

export function shortHash(value: string): string {
  return createHash("sha1").update(value).digest("hex").slice(0, 10);
}

export function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

export function commandEnv(extra?: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const env = { ...process.env, ...extra };
  const existingPath = env.PATH?.trim();
  env.PATH = existingPath ? mergePath(existingPath, DEFAULT_PATH) : DEFAULT_PATH;
  return env;
}

function mergePath(primary: string, fallback: string): string {
  const seen = new Set<string>();
  const parts: string[] = [];
  for (const item of `${primary}:${fallback}`.split(":")) {
    if (!item || seen.has(item)) {
      continue;
    }
    seen.add(item);
    parts.push(item);
  }
  return parts.join(":");
}

export function parseJsonLine(line: string): JsonValue | null {
  try {
    return JSON.parse(line) as JsonValue;
  } catch {
    return null;
  }
}

export function lineSplitBuffer(
  previous: string,
  chunk: string,
): { lines: string[]; rest: string } {
  const joined = previous + chunk;
  const parts = joined.split(/\r?\n/);
  const rest = parts.pop() ?? "";
  return { lines: parts, rest };
}
