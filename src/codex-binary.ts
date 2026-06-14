import { createHash } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { gunzipSync } from "node:zlib";
import { commandExists, ensureDir, rudderHome } from "./util.js";

export const RUDDER_CODEX_REPOSITORY = "viraatdas/codex";
export const RUDDER_CODEX_RELEASE = "rudder-codex-v0.1.1-upstream-db9cb04";
export const RUDDER_CODEX_ASSET_SHA256 = "ea08a91e85b35c0c4782a96535011dfcaeaff7259113e65ecf5260bc24368517";
// Rudder workers run Codex as a child process, so do not inherit desktop-app
// notification hooks that expect the official signed app launch chain.
export const CODEX_RUDDER_CONFIG_ARGS = [
  "-c",
  "notify=[]",
  "-c",
  "features.plugins=false",
  "-c",
  "features.computer_use=false",
  "-c",
  'model_reasoning_summary="detailed"',
  "-c",
  "model_supports_reasoning_summaries=true",
];

export async function codexEnvVars(): Promise<Record<string, string>> {
  try {
    const codex = await ensureRudderCodexBinary();
    return {
      RUDDER_CODEX_BIN: codex,
      RUDDER_CODEX_VERSION: RUDDER_CODEX_RELEASE,
      CODEX_RUDDER_SCROLLBACK_SAFE: "1",
    };
  } catch {
    // Codex is OPTIONAL. On a platform with no managed Rudder Codex binary (e.g. a
    // linux/x64 cloud worker), enriching the env must not crash non-codex flows —
    // the dashboard preflight, and claude/acpx agents. Returning no codex env lets
    // them run; an EXPLICIT codex agent launch still hard-fails with a clear error
    // because backends/run-manager call ensureRudderCodexBinary() directly first.
    return {};
  }
}

export async function codexLaunchEnv(base: NodeJS.ProcessEnv = process.env): Promise<NodeJS.ProcessEnv> {
  return {
    ...base,
    ...await codexEnvVars(),
  };
}

export async function ensureRudderCodexBinary(): Promise<string> {
  const override = process.env.RUDDER_CODEX_BIN?.trim();
  if (override) {
    const resolved = expandCommandPath(override);
    if (await isRunnable(resolved)) {
      return resolved;
    }
    throw new Error(`RUDDER_CODEX_BIN is set but is not executable: ${override}`);
  }

  const assets = platformAssetNames();
  const dest = managedBinaryPath();
  if (await verifyCachedManagedBinary(dest)) {
    return dest;
  }

  await downloadManagedBinary(assets, dest);
  if (!await verifyCachedManagedBinary(dest)) {
    throw new Error(`Managed Rudder Codex install failed verification: ${dest}`);
  }
  return dest;
}

export function managedBinaryPath(): string {
  return path.join(rudderHome(), "bin", "codex", RUDDER_CODEX_RELEASE, "rudder-codex");
}

function managedChecksumPath(): string {
  return `${managedBinaryPath()}.sha256`;
}

function platformAssetNames(): string[] {
  if (process.platform === "darwin" && process.arch === "arm64") {
    return ["rudder-codex-darwin-arm64.gz"];
  }
  throw new Error(
    `Rudder's pinned Codex fork does not have a managed binary for ${process.platform}/${process.arch} yet. Set RUDDER_CODEX_BIN to an executable override.`,
  );
}

async function downloadManagedBinary(assets: string[], dest: string): Promise<void> {
  const repo = process.env.RUDDER_CODEX_REPO?.trim() || RUDDER_CODEX_REPOSITORY;
  const downloaded = Buffer.concat(await Promise.all(assets.map((asset) => downloadReleaseAsset(repo, asset))));
  if (RUDDER_CODEX_ASSET_SHA256) {
    const actual = createHash("sha256").update(downloaded).digest("hex");
    if (actual !== RUDDER_CODEX_ASSET_SHA256) {
      throw new Error(`Downloaded Rudder Codex checksum mismatch: expected ${RUDDER_CODEX_ASSET_SHA256}, got ${actual}`);
    }
  }
  const bytes = assets[0]?.includes(".gz") ? gunzipSync(downloaded) : downloaded;
  const binarySha = createHash("sha256").update(bytes).digest("hex");

  await ensureDir(path.dirname(dest));
  const temp = path.join(path.dirname(dest), `.rudder-codex.${process.pid}.${Date.now()}`);
  await fsp.writeFile(temp, bytes, { mode: 0o755 });
  await fsp.chmod(temp, 0o755);
  await fsp.rename(temp, dest);
  await fsp.writeFile(managedChecksumPath(), `${binarySha}\n`);
}

async function downloadReleaseAsset(repo: string, asset: string): Promise<Buffer> {
  const url = `https://github.com/${repo}/releases/download/${RUDDER_CODEX_RELEASE}/${asset}`;
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`Failed to download Rudder Codex ${RUDDER_CODEX_RELEASE} from ${url}: HTTP ${response.status}`);
  }
  return Buffer.from(await response.arrayBuffer());
}

async function verifyCachedManagedBinary(file: string): Promise<boolean> {
  if (!await isRunnable(file)) {
    return false;
  }

  const checksumFile = managedChecksumPath();
  try {
    const expected = (await fsp.readFile(checksumFile, "utf8")).trim();
    const actual = await sha256File(file);
    if (expected && actual === expected) {
      return true;
    }
  } catch {
    // Redownload below if the executable exists but its checksum marker is missing.
  }

  await fsp.rm(file, { force: true });
  await fsp.rm(checksumFile, { force: true });
  return false;
}

async function sha256File(file: string): Promise<string> {
  const hash = createHash("sha256");
  const stream = fs.createReadStream(file);
  for await (const chunk of stream) {
    hash.update(chunk);
  }
  return hash.digest("hex");
}

async function isRunnable(command: string): Promise<boolean> {
  if (!command.includes(path.sep)) {
    return commandExists(command);
  }
  try {
    await fsp.access(command, fs.constants.X_OK);
    return (await fsp.stat(command)).isFile();
  } catch {
    return false;
  }
}

function expandCommandPath(command: string): string {
  return command.startsWith("~/") ? path.join(os.homedir(), command.slice(2)) : command;
}
