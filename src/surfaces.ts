import fsp from "node:fs/promises";
import path from "node:path";

import { projectNodeStatus, readGraph, readyNodes } from "./graph.js";
import { agentContextPath, loadRunRecord } from "./state.js";
import type { RudderGraph, TaskNode } from "./types.js";
import { ensureDir, nowIso, pathExists, runCommand, shortenHome } from "./util.js";

// ---------------------------------------------------------------------------
// Surface A (Phase 4): the live RUDDER.md. The daemon re-renders this on every
// graph/status change. It is a read-only projection of graph.json: agents never
// write it. A monotonic `freshness:` epoch-ms stamp lets an agent detect that a
// sibling moved while it was working. It stays git-excluded (like the launch
// snapshot), so it never lands in a commit. Surface B (DECISIONS.md) is Phase 5.
// ---------------------------------------------------------------------------

const STATUS_BADGE: Record<TaskNode["status"], string> = {
  planned: "○ planned",
  ready: "◍ ready",
  running: "● running",
  review: "◆ review",
  blocked: "▲ blocked",
  merged: "✓ merged",
  failed: "✗ failed",
};

// The DECISIONS.md header. DECISIONS.md is TRACKED (not gitignored), unlike
// RUDDER.md: it is agent-authored shared knowledge that jj merges as first-class
// conflicts on fan-in.
export const DECISIONS_HEADER =
  "# Decisions  (shared, agent-authored. Append one bullet per cross-cutting decision: what, why, owning node.)";

// The propagation contract. These three lines live in the worker system-prompt
// (brain.renderContract working rules) and are mirrored into the RUDDER.md
// preamble so an agent reading only RUDDER.md still gets them. Kept here so the
// two surfaces never drift. No em dashes.
export const PROPAGATION_RULES: string[] = [
  "Before each significant step, re-read RUDDER.md (live orchestrator status) and DECISIONS.md (shared decisions from sibling agents); they change while you work.",
  "RUDDER.md carries a freshness stamp. If it is newer than when you last read it, a sibling changed state - re-read both before continuing.",
  "Record any cross-cutting decision other agents must honor by appending a bullet to DECISIONS.md (decision, rationale, owning node id). Never edit RUDDER.md; it is orchestrator-owned.",
];

function decisionsPath(repoRoot: string): string {
  return path.join(repoRoot, "DECISIONS.md");
}

/**
 * Surface B: ensure a TRACKED DECISIONS.md exists at the repo root (and is NOT
 * gitignored, unlike RUDDER.md). Agents append cross-cutting decisions to it in
 * their jj workspace; jj merges concurrent edits as first-class conflicts on
 * fan-in. Idempotent: only writes the header when the file is absent. Returns
 * the absolute path.
 */
export async function ensureDecisionsFile(repoRoot: string): Promise<string> {
  const target = decisionsPath(repoRoot);
  if (await pathExists(target)) {
    return target;
  }
  await ensureDir(path.dirname(target));
  await fsp.writeFile(target, `${DECISIONS_HEADER}\n\n`, "utf8").catch(() => undefined);
  return target;
}

/**
 * Append an insight as a DECISIONS.md bullet. Used by `rudder remember` (the
 * bd-remember equivalent) and any code path that records a decision. The bullet
 * carries an "(owner: X, <iso>)" suffix so the board parser can attribute it.
 */
export async function appendDecision(
  repoRoot: string,
  insight: string,
  owner = "cli",
): Promise<void> {
  await ensureDecisionsFile(repoRoot);
  const text = insight.replace(/\s+/g, " ").trim();
  if (!text) {
    return;
  }
  const bullet = `- ${text}  (owner: ${owner}, ${nowIso()})\n`;
  await fsp.appendFile(decisionsPath(repoRoot), bullet, "utf8").catch(() => undefined);
}

// The freshness stamp folds in DECISIONS.md's mtime so a sibling appending a
// decision bumps RUDDER.md's freshness (and the board's memory.updated SSE),
// not just a node status change. Falls back to Date.now() when stat fails.
async function computeFreshness(repoRoot: string): Promise<number> {
  let freshness = Date.now();
  try {
    const stat = await fsp.stat(decisionsPath(repoRoot));
    freshness = Math.max(freshness, Math.round(stat.mtimeMs));
  } catch {
    // DECISIONS.md may not exist yet; the wall clock is enough.
  }
  return freshness;
}

/**
 * Render the live RUDDER.md at repoRoot from the current graph.json. For nodes
 * that have a runId, the status is recomputed from the worker-owned run.json so
 * the surface reflects the live RunStatus, not the last graph snapshot. Never
 * throws on a missing run record; degrades to the graph status.
 */
export async function renderLiveRudderMd(repoRoot: string): Promise<void> {
  // Ensure the agent-authored knowledge surface exists so siblings have a place
  // to append decisions, and so its mtime feeds the freshness stamp below.
  await ensureDecisionsFile(repoRoot).catch(() => undefined);

  const graph = await readGraph(repoRoot);
  const nodes = Object.values(graph.nodes).sort((a, b) => a.createdAt.localeCompare(b.createdAt));

  const statusByNode = new Map<string, TaskNode["status"]>();
  for (const node of nodes) {
    if (node.runId) {
      const run = await loadRunRecord(repoRoot, node.runId).catch(() => null);
      statusByNode.set(node.id, projectNodeStatus(node, run ?? undefined));
    } else {
      statusByNode.set(node.id, node.status);
    }
  }

  const now = nowIso();
  const freshness = await computeFreshness(repoRoot);
  const lines: string[] = [
    "# RUDDER — Orchestrated Run Status   (read-only; re-read at the top of every significant step)",
    "",
    "This file is generated by Rudder. It is not user-authored repo documentation and is git-excluded.",
    "",
    `Updated: ${now}   ·   freshness: ${freshness}`,
    "",
    "## Coordination rules",
    ...PROPAGATION_RULES.map((rule) => `- ${rule}`),
    "",
    "## Nodes",
  ];

  if (nodes.length === 0) {
    lines.push("- No nodes in the graph yet.");
  } else {
    lines.push("| node | status | deps | workspace | task |");
    lines.push("| --- | --- | --- | --- | --- |");
    for (const node of nodes) {
      const status = statusByNode.get(node.id) ?? node.status;
      const deps = depLabels(graph, node.id);
      const workspace = node.worktree?.path ? shortenHome(node.worktree.path) : "—";
      const task = oneLine(node.title || node.prompt, 60);
      lines.push(`| ${node.id} | ${STATUS_BADGE[status]} | ${deps} | ${workspace} | ${task} |`);
    }
  }

  lines.push("", "## Ready work");
  const ready = readyNodes(graph);
  if (ready.length === 0) {
    lines.push("- None (no node has all hard deps merged).");
  } else {
    for (const node of ready) {
      lines.push(`- ${node.id} (deps met) — ${oneLine(node.title || node.prompt, 72)}`);
    }
  }

  lines.push(
    "",
    "## Shared decisions",
    "Shared, cross-cutting decisions live in DECISIONS.md (agent-authored, jj-tracked). Never edit RUDDER.md.",
    "",
  );

  await writeLiveRudderMd(repoRoot, graph, `${lines.join("\n")}\n`);
}

function depLabels(graph: RudderGraph, id: string): string {
  const labels: string[] = [];
  for (const edge of Object.values(graph.edges)) {
    if (edge.to === id) {
      labels.push(edge.type === "hard" ? edge.from : `${edge.from}~`);
    }
  }
  return labels.length ? labels.join(", ") : "—";
}

function oneLine(value: string, max: number): string {
  const flat = value.replace(/\s+/g, " ").replace(/\|/g, "/").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

// Write RUDDER.md to the repo root and every node workspace, keeping each
// git-excluded so the projection never lands in a commit. Mirrors the launch
// snapshot's exclusion handling (run-manager.writeRudderContextFiles).
async function writeLiveRudderMd(repoRoot: string, graph: RudderGraph, content: string): Promise<void> {
  await ensureLine(path.join(repoRoot, ".gitignore"), "RUDDER.md");
  const workspaces = new Set<string>([repoRoot]);
  for (const node of Object.values(graph.nodes)) {
    if (node.worktree?.path) {
      workspaces.add(node.worktree.path);
    }
  }
  for (const workspace of workspaces) {
    await ensureRudderExcluded(workspace);
    await ensureDir(path.dirname(agentContextPath(workspace)));
    await fsp.writeFile(agentContextPath(workspace), content, "utf8").catch(() => undefined);
  }
}

async function ensureRudderExcluded(workspace: string): Promise<void> {
  const result = await runCommand("git", ["rev-parse", "--git-path", "info/exclude"], {
    cwd: workspace,
    allowFailure: true,
  }).catch(() => null);
  const excludePath = result?.stdout.trim();
  if (!excludePath) {
    return;
  }
  await ensureLine(path.resolve(workspace, excludePath), "RUDDER.md");
}

async function ensureLine(filePath: string, line: string): Promise<void> {
  const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
  const lines = existing.split(/\r?\n/).map((item) => item.trim());
  if (lines.includes(line)) {
    return;
  }
  const prefix = existing && !existing.endsWith("\n") ? "\n" : "";
  await ensureDir(path.dirname(filePath));
  await fsp.appendFile(filePath, `${prefix}${line}\n`, "utf8").catch(() => undefined);
}
