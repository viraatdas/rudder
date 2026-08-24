import { spawn } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const CACHE_TTL_MS = 6 * 60 * 60 * 1000; // 6 hours
const AUTO_UPDATE_TIMEOUT_MS = 1000;

const moduleDir = path.dirname(fileURLToPath(import.meta.url));

function cachePath(): string {
  const base = process.env.XDG_CACHE_HOME?.trim() || path.join(os.homedir(), ".cache");
  return path.join(base, "rudder", "update-check.json");
}

export function compareSemver(a: string, b: string): number {
  const split = (v: string) =>
    v
      .split("-")[0]!
      .split(".")
      .map((part) => Number.parseInt(part, 10) || 0);
  const aa = split(a);
  const bb = split(b);
  for (let i = 0; i < 3; i++) {
    const x = aa[i] ?? 0;
    const y = bb[i] ?? 0;
    if (x !== y) return x < y ? -1 : 1;
  }
  return 0;
}

async function readPackageInfo(): Promise<{ root: string; version: string } | null> {
  // Try package.json relative to compiled dist/, then to source src/.
  const candidates = [
    path.join(moduleDir, "..", "package.json"),
    path.join(moduleDir, "..", "..", "package.json"),
  ];
  for (const candidate of candidates) {
    try {
      const raw = await fsp.readFile(candidate, "utf8");
      const parsed = JSON.parse(raw) as { version?: unknown; name?: unknown };
      if (parsed?.name === "@viraatdas/rudder" && typeof parsed.version === "string") {
        return { root: path.dirname(candidate), version: parsed.version };
      }
    } catch {
      // ignore
    }
  }
  return null;
}

async function readPackageVersion(): Promise<string | null> {
  return (await readPackageInfo())?.version ?? null;
}

async function readCache(): Promise<{ latest: string; checkedAt: number } | null> {
  try {
    const raw = await fsp.readFile(cachePath(), "utf8");
    const parsed = JSON.parse(raw) as { latest?: unknown; checkedAt?: unknown };
    if (typeof parsed.latest === "string" && typeof parsed.checkedAt === "number") {
      return { latest: parsed.latest, checkedAt: parsed.checkedAt };
    }
  } catch {
    // ignore
  }
  return null;
}

async function writeCache(latest: string): Promise<void> {
  const file = cachePath();
  try {
    await fsp.mkdir(path.dirname(file), { recursive: true });
    await fsp.writeFile(file, JSON.stringify({ latest, checkedAt: Date.now() }));
  } catch {
    // ignore
  }
}

async function fetchLatest(timeoutMs = 1500): Promise<string | null> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch("https://registry.npmjs.org/@viraatdas/rudder/latest", {
      signal: controller.signal,
      headers: { accept: "application/json" },
    });
    if (!res.ok) return null;
    const data = (await res.json()) as { version?: unknown };
    return typeof data?.version === "string" ? data.version : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Returns the latest published version if our local version is older, otherwise null.
 * Cached for 6h on disk so startup doesn't hit the network every launch. Never throws.
 * Disabled when RUDDER_DISABLE_UPDATE_CHECK is set.
 */
export async function getUpdateAvailable(): Promise<{ current: string; latest: string } | null> {
  if (process.env.RUDDER_DISABLE_UPDATE_CHECK) return null;
  const current = await readPackageVersion();
  if (!current) return null;

  const cached = await readCache();
  let latest: string | null = null;
  if (cached && Date.now() - cached.checkedAt < CACHE_TTL_MS) {
    latest = cached.latest;
  } else {
    latest = await fetchLatest();
    if (latest) {
      await writeCache(latest);
    } else if (cached) {
      latest = cached.latest;
    }
  }
  if (!latest) return null;
  return compareSemver(current, latest) < 0 ? { current, latest } : null;
}

export function shouldAutoUpdateFromPackageRoot(packageRoot: string): boolean {
  const root = path.resolve(packageRoot);
  if (process.env.RUDDER_DISABLE_UPDATE_CHECK || process.env.RUDDER_DISABLE_AUTO_UPDATE || process.env.RUDDER_SKIP_AUTO_UPDATE) {
    return false;
  }
  if (process.env.CI) {
    return false;
  }
  // Do not mutate a source checkout while developing Rudder locally.
  if (fs.existsSync(path.join(root, ".git")) || fs.existsSync(path.join(root, "src", "main.ts"))) {
    return false;
  }
  // `npx @viraatdas/rudder@...` is already an ephemeral install; turning that into
  // a global install is surprising and can fail under temp-directory permissions.
  const normalized = root.split(path.sep).join("/");
  if (normalized.includes("/_npx/") || normalized.includes("/.npm/_npx/")) {
    return false;
  }
  return true;
}

export async function autoUpdateAndRerunIfNeeded(argv: string[]): Promise<boolean> {
  const info = await readPackageInfo();
  if (!info || !shouldAutoUpdateFromPackageRoot(info.root)) {
    return false;
  }
  const latest = await fetchLatest(AUTO_UPDATE_TIMEOUT_MS);
  if (!latest || compareSemver(info.version, latest) >= 0) {
    if (latest) {
      await writeCache(latest);
    }
    return false;
  }

  console.error(`rudder: updating ${info.version} -> ${latest}...`);
  const installed = await installLatest(latest);
  if (!installed.ok) {
    // A version can be readable in the registry's packument seconds before every
    // replica can resolve it for install. The updater sees the new number, npm
    // says ETARGET, and the user gets a five-line npm error block for what is a
    // publish that has not finished propagating. Name it and move on; nothing is
    // cached on failure, so the next launch retries.
    if (isUnpublishedYet(installed.output)) {
      console.error(
        `rudder: ${latest} is not resolvable from the registry yet; staying on ${info.version}`,
      );
    } else {
      console.error(`rudder: auto-update failed; continuing with ${info.version}`);
      const detail = installed.output.trim();
      if (detail) {
        console.error(detail);
      }
    }
    return false;
  }
  await writeCache(latest);
  console.error(`rudder: updated to ${latest}; restarting command...`);
  process.exitCode = await rerunCurrentCommand(argv);
  return true;
}

type InstallResult = { ok: boolean; output: string };

async function installLatest(version: string): Promise<InstallResult> {
  const result = await spawnCaptured("npm", ["install", "-g", `@viraatdas/rudder@${version}`], {
    RUDDER_SKIP_AUTO_UPDATE: "1",
  });
  return { ok: result.code === 0, output: result.output };
}

/** npm's shape for "that version exists in the index but I cannot resolve it". */
export function isUnpublishedYet(npmOutput: string): boolean {
  return /\bETARGET\b/.test(npmOutput) || /\bnotarget\b/.test(npmOutput);
}

async function rerunCurrentCommand(argv: string[]): Promise<number> {
  const entry = process.argv[1];
  if (entry) {
    const code = await spawnExitCode(process.execPath, [entry, ...argv], {
      RUDDER_SKIP_AUTO_UPDATE: "1",
    });
    if (code !== 127) {
      return code;
    }
  }
  return await spawnExitCode("rudder", argv, { RUDDER_SKIP_AUTO_UPDATE: "1" });
}

/**
 * Inherited stdio. The re-run path hands the terminal to the real dashboard,
 * so this MUST NOT be swapped for the capturing variant: a captured TUI has no
 * terminal to draw on.
 */
async function spawnExitCode(
  command: string,
  args: string[],
  envPatch: NodeJS.ProcessEnv,
): Promise<number> {
  return await new Promise((resolve) => {
    const child = spawn(command, args, {
      stdio: "inherit",
      env: { ...process.env, ...envPatch },
    });
    child.on("error", () => resolve(127));
    child.on("close", (code) => resolve(code ?? 1));
  });
}

/**
 * Run a command with its output CAPTURED rather than inherited, so the caller
 * decides what the user sees. The updater runs before the TUI paints, and an
 * inherited npm error block is indistinguishable from rudder itself failing.
 */
async function spawnCaptured(
  command: string,
  args: string[],
  envPatch: NodeJS.ProcessEnv,
): Promise<{ code: number; output: string }> {
  return await new Promise((resolve) => {
    let output = "";
    const child = spawn(command, args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, ...envPatch },
    });
    child.stdout?.on("data", (chunk) => {
      output += String(chunk);
    });
    child.stderr?.on("data", (chunk) => {
      output += String(chunk);
    });
    child.on("error", () => resolve({ code: 127, output }));
    child.on("close", (code) => resolve({ code: code ?? 1, output }));
  });
}
