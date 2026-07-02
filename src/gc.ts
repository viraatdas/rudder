import fsp from "node:fs/promises";
import path from "node:path";
import { RUDDER_CODEX_RELEASE } from "./codex-binary.js";
import { findRepoRoot } from "./git.js";
import { LOG_KEEP_ROTATED, LOG_MAX_BYTES, logsDir } from "./logger.js";
import { loadRunRecord, runsDir } from "./state.js";
import type { RunRecord } from "./types.js";
import { pathExists, rudderHome } from "./util.js";

type GcCandidate = {
  category: string;
  path: string;
  bytes: number;
  reason: string;
};

const DAY_MS = 24 * 60 * 60 * 1000;
const PERF_TTL_MS = 3 * DAY_MS;
const SIGNAL_TTL_MS = 7 * DAY_MS;
const RUN_TTL_MS = 30 * DAY_MS;

export async function runGc(options: { dryRun?: boolean } = {}): Promise<void> {
  const dryRun = Boolean(options.dryRun);
  const home = rudderHome();
  const repoRoot = safeRepoRoot();
  const candidates = [
    ...await perfLogCandidates(home),
    ...await signalCandidates(home),
    ...await daemonLogCandidates(home),
    ...await oldBinaryCandidates(home),
    ...await finishedRunCandidates(repoRoot),
    ...await nestedWorktreeRunCandidates(repoRoot),
  ];

  let removed = 0;
  for (const candidate of candidates) {
    removed += candidate.bytes;
    if (!dryRun) {
      await fsp.rm(candidate.path, { recursive: true, force: true }).catch(() => undefined);
    }
  }

  const grouped = groupCandidates(candidates);
  console.log(dryRun ? "rudder gc dry run" : "rudder gc");
  if (candidates.length === 0) {
    console.log("  nothing to clean");
    return;
  }
  for (const [category, bytes] of grouped) {
    const count = candidates.filter((candidate) => candidate.category === category).length;
    console.log(`  ${category}: ${formatBytes(bytes)} in ${count} item(s)`);
  }
  console.log(`${dryRun ? "would recover" : "recovered"} ${formatBytes(removed)}`);
}

async function perfLogCandidates(home: string): Promise<GcCandidate[]> {
  const entries = await fileEntries(home, (name) => isPerfLog(name));
  const sorted = entries.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return sorted
    .map((entry, index) => {
      const tooOld = Date.now() - entry.mtimeMs > PERF_TTL_MS;
      const tooMany = index >= 2;
      const tooLarge = entry.bytes > LOG_MAX_BYTES;
      if (!tooOld && !tooMany && !tooLarge) return null;
      return {
        category: "perf logs",
        path: entry.path,
        bytes: entry.bytes,
        reason: tooOld ? "ttl" : tooMany ? "excess" : "oversize",
      };
    })
    .filter((candidate): candidate is GcCandidate => Boolean(candidate));
}

async function signalCandidates(home: string): Promise<GcCandidate[]> {
  const dir = path.join(home, "signals");
  const entries = await fileEntries(dir, () => true);
  return entries
    .filter((entry) => Date.now() - entry.mtimeMs > SIGNAL_TTL_MS)
    .map((entry) => ({
      category: "signals",
      path: entry.path,
      bytes: entry.bytes,
      reason: "ttl",
    }));
}

async function daemonLogCandidates(home: string): Promise<GcCandidate[]> {
  const dir = logsDir();
  if (!dir.startsWith(home)) {
    return [];
  }
  const entries = await fileEntries(dir, (name) => name.endsWith(".log") || name.endsWith(".ndjson") || /\.log\.\d+$/.test(name) || /\.ndjson\.\d+$/.test(name));
  const byBase = new Map<string, typeof entries>();
  for (const entry of entries) {
    const base = entry.path.replace(/\.\d+$/, "");
    byBase.set(base, [...(byBase.get(base) ?? []), entry]);
  }
  const candidates: GcCandidate[] = [];
  for (const [, group] of byBase) {
    const sorted = group.sort((a, b) => b.mtimeMs - a.mtimeMs);
    sorted.forEach((entry, index) => {
      if (entry.bytes <= LOG_MAX_BYTES && index <= LOG_KEEP_ROTATED) {
        return;
      }
      candidates.push({
        category: "daemon logs",
        path: entry.path,
        bytes: entry.bytes,
        reason: entry.bytes > LOG_MAX_BYTES ? "oversize" : "excess",
      });
    });
  }
  return candidates;
}

async function oldBinaryCandidates(home: string): Promise<GcCandidate[]> {
  const root = path.join(home, "bin", "codex");
  const entries = await fsp.readdir(root, { withFileTypes: true }).catch(() => []);
  const candidates: GcCandidate[] = [];
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.name === RUDDER_CODEX_RELEASE) {
      continue;
    }
    const fullPath = path.join(root, entry.name);
    candidates.push({
      category: "old binaries",
      path: fullPath,
      bytes: await pathSize(fullPath),
      reason: "superseded",
    });
  }
  return candidates;
}

async function finishedRunCandidates(repoRoot: string | null): Promise<GcCandidate[]> {
  if (!repoRoot) {
    return [];
  }
  const dir = runsDir(repoRoot);
  const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
  const candidates: GcCandidate[] = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const run = await loadRunRecord(repoRoot, entry.name);
    if (!run || !canCollectRunRecord(run)) continue;
    const stamp = Date.parse(run.updatedAt || run.createdAt);
    if (Number.isFinite(stamp) && Date.now() - stamp < RUN_TTL_MS) continue;
    if (run.worktree.enabled && run.worktree.path && await pathExists(run.worktree.path)) {
      continue;
    }
    const fullPath = path.join(dir, entry.name);
    candidates.push({
      category: "finished runs",
      path: fullPath,
      bytes: await pathSize(fullPath),
      reason: "ttl",
    });
  }
  return candidates;
}

async function nestedWorktreeRunCandidates(repoRoot: string | null): Promise<GcCandidate[]> {
  if (!repoRoot) {
    return [];
  }
  const root = path.join(repoRoot, ".rudder-worktrees");
  const runsDirs = await findNestedRunsDirs(root);
  const candidates: GcCandidate[] = [];
  for (const dir of runsDirs) {
    const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const fullPath = path.join(dir, entry.name);
      const stat = await fsp.stat(fullPath).catch(() => null);
      if (!stat || Date.now() - stat.mtimeMs < RUN_TTL_MS) continue;
      candidates.push({
        category: "worktree runs",
        path: fullPath,
        bytes: await pathSize(fullPath),
        reason: "ttl",
      });
    }
  }
  return candidates;
}

function canCollectRunRecord(run: RunRecord): boolean {
  return run.status === "merged" || run.status === "completed" || run.status === "failed" || run.status === "cancelled";
}

async function findNestedRunsDirs(root: string): Promise<string[]> {
  const out: string[] = [];
  async function walk(dir: string, depth: number): Promise<void> {
    if (depth > 6) return;
    const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const fullPath = path.join(dir, entry.name);
      if (entry.name === "runs" && path.basename(path.dirname(fullPath)) === ".rudder") {
        out.push(fullPath);
        continue;
      }
      await walk(fullPath, depth + 1);
    }
  }
  await walk(root, 0);
  return out;
}

async function fileEntries(dir: string, predicate: (name: string) => boolean): Promise<Array<{ path: string; bytes: number; mtimeMs: number }>> {
  const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
  const out: Array<{ path: string; bytes: number; mtimeMs: number }> = [];
  for (const entry of entries) {
    if (!entry.isFile() || !predicate(entry.name)) continue;
    const fullPath = path.join(dir, entry.name);
    const stat = await fsp.stat(fullPath).catch(() => null);
    if (!stat) continue;
    out.push({ path: fullPath, bytes: stat.size, mtimeMs: stat.mtimeMs });
  }
  return out;
}

async function pathSize(filePath: string): Promise<number> {
  const stat = await fsp.stat(filePath).catch(() => null);
  if (!stat) return 0;
  if (stat.isFile()) return stat.size;
  if (!stat.isDirectory()) return 0;
  const entries = await fsp.readdir(filePath, { withFileTypes: true }).catch(() => []);
  let total = 0;
  for (const entry of entries) {
    total += await pathSize(path.join(filePath, entry.name));
  }
  return total;
}

function isPerfLog(name: string): boolean {
  return name === "native-perf.ndjson"
    || name.startsWith("native-perf.ndjson.")
    || (name.startsWith("native-perf-") && name.includes(".ndjson"));
}

function groupCandidates(candidates: GcCandidate[]): Array<[string, number]> {
  const grouped = new Map<string, number>();
  for (const candidate of candidates) {
    grouped.set(candidate.category, (grouped.get(candidate.category) ?? 0) + candidate.bytes);
  }
  return [...grouped.entries()];
}

function safeRepoRoot(): string | null {
  try {
    return findRepoRoot();
  } catch {
    return null;
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kib = bytes / 1024;
  if (kib < 1024) return `${kib.toFixed(1)} KiB`;
  const mib = kib / 1024;
  if (mib < 1024) return `${mib.toFixed(1)} MiB`;
  return `${(mib / 1024).toFixed(1)} GiB`;
}
