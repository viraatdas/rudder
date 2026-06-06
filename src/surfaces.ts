import fsp from "node:fs/promises";
import path from "node:path";

import { projectNodeStatus, readGraph, readyNodes } from "./graph.js";
import { mergeGeneratedRudderMd } from "./rudder-md.js";
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
  "# Decisions\n\nShared, agent-authored log of cross-cutting decisions the fleet must honor. The conductor records plan/rebase/steer decisions here; workers record interface contracts + adjustments. Each entry is a `##` heading with **What / Why / By** so it scans. Re-read before each significant step; jj merges concurrent edits as first-class conflicts on fan-in.";

/** A short scannable title from free text: the first ~9 words, single line, capped. */
function decisionTitle(text: string, max = 9): string {
  const flat = text.replace(/\s+/g, " ").trim();
  if (!flat) {
    return "decision";
  }
  const words = flat.split(" ").slice(0, max).join(" ");
  return words.length > 80 ? `${words.slice(0, 79)}…` : words;
}

/** Render ONE canonical DECISIONS.md entry: a `## title` heading, labeled body lines,
 *  and a `**By:** owner · when` footer. Shared shape for every writer (remember, the
 *  conductor, completion notes) so the log stays uniform and scannable. */
export function renderDecisionEntry(opts: {
  title: string;
  body: string[];
  owner: string;
  when?: string;
}): string {
  const lines = [`## ${opts.title.replace(/\s+/g, " ").trim() || "decision"}`];
  for (const line of opts.body) {
    if (line.trim()) {
      lines.push(line);
    }
  }
  lines.push(`- **By:** ${opts.owner.trim() || "rudder"} · ${opts.when || nowIso()}`);
  return `${lines.join("\n")}\n\n`;
}

/** Append a rendered entry to DECISIONS.md, prepending a newline when the file does not
 *  already end in one so a new `##` heading never fuses onto a previous non-terminated
 *  line (mirrors the Rust gitio.rs append_conductor_decision guard). */
async function appendDecisionsEntry(repoRoot: string, entry: string): Promise<void> {
  const target = await ensureDecisionsFile(repoRoot);
  let prefix = "";
  try {
    const existing = await fsp.readFile(target, "utf8");
    if (existing.length > 0 && !existing.endsWith("\n")) {
      prefix = "\n";
    }
  } catch {
    // Absent (just created by ensureDecisionsFile); no prefix needed.
  }
  await fsp.appendFile(target, `${prefix}${entry}`, "utf8").catch(() => undefined);
}

// The propagation contract. These three lines live in the worker system-prompt
// (brain.renderContract working rules) and are mirrored into the RUDDER.md
// preamble so an agent reading only RUDDER.md still gets them. Kept here so the
// two surfaces never drift. No em dashes.
export const PROPAGATION_RULES: string[] = [
  "Before each significant step, re-read RUDDER.md (live orchestrator status + the current plan/DAG) and DECISIONS.md (shared decisions from sibling agents); they change while you work.",
  "RUDDER.md carries a freshness stamp. If it is newer than when you last read it, the plan or a sibling changed state - re-read both before continuing.",
  "If the plan or architecture has shifted in a way that affects your task (the user refined it, or a sibling recorded a conflicting decision), ADAPT your in-progress work to the new direction instead of continuing on the old plan, so the work does not stray; note the adjustment in DECISIONS.md.",
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
  const entry = renderDecisionEntry({
    title: decisionTitle(text),
    body: [`- **What:** ${text}`],
    owner,
  });
  await appendDecisionsEntry(repoRoot, entry);
}

// Markers wrapping the JSON a worker's `rudder done` echoes to stdout, so the TUI
// brain can parse the completion note out of the worker's PTY scrollback (the same
// way it parses RUDDER_PLAN_TASKS). Kept here so both sides agree on the shape.
export const RUDDER_DONE_START = "RUDDER_DONE_START";
export const RUDDER_DONE_END = "RUDDER_DONE_END";

/** One piece of follow-up work a finishing agent recommends. `scope: "out"` marks
 *  work outside the agent's lane (recorded but not auto-injected by the brain). */
export type FollowupProposal = {
  title: string;
  prompt?: string;
  deps?: string[];
  why?: string;
  scope?: "in" | "out";
};

/** A worker's structured end-of-task report: what it did, the interfaces it
 *  created/assumed, and follow-up work it recommends. Appended to DECISIONS.md and
 *  echoed (RUDDER_DONE markers) so siblings AND the orchestrator pick it up. */
export type CompletionNote = {
  node?: string;
  summary?: string;
  interfaces?: string;
  followups?: FollowupProposal[];
};

/** Parse a `rudder done` argument into a CompletionNote. Accepts a bare JSON object, a
 *  fenced ```json block (models love to wrap output in fences), and falls back to a
 *  freeform `{ summary: raw }` for prose, arrays, or primitives — anything that is not a
 *  JSON OBJECT. Keeping arrays/primitives out of the note matters: the orchestrator reads
 *  `note.followups`, and a stray array cast would yield nothing AND lose the text. */
export function parseCompletionNoteArg(raw: string): CompletionNote {
  const tryObject = (text: string): CompletionNote | undefined => {
    try {
      const parsed: unknown = JSON.parse(text);
      // Only a plain object is a structured note; arrays/numbers/strings are not.
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return parsed as CompletionNote;
      }
    } catch {
      // not JSON
    }
    return undefined;
  };
  // Strip a leading/trailing markdown code fence, if present, and parse the inside.
  const fenced = raw.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/);
  return tryObject(raw) ?? (fenced ? tryObject(fenced[1]) : undefined) ?? { summary: raw };
}

/** Drop the structured completion note as a machine-readable JSON file the orchestrator
 *  reads straight off disk. This is the AUTHORITATIVE, terminal-independent channel: it
 *  never passes through the agent's interactive TUI, so a real Claude/Codex worker's
 *  report survives however that UI boxes/truncates/wraps the echoed block in the PTY.
 *  The launcher sets RUDDER_DONE_FILE to <workspace>/.rudder/done/<node>.json and the
 *  orchestrator reads that exact path on completion. Atomic (temp + rename) so the reader
 *  never sees a partial file. Best-effort: returns false on any error (the DECISIONS.md
 *  and stdout channels still carry the note). */
export async function writeCompletionNoteFile(
  doneFile: string,
  note: CompletionNote,
): Promise<boolean> {
  try {
    await fsp.mkdir(path.dirname(doneFile), { recursive: true });
    const tmp = `${doneFile}.${process.pid}.tmp`;
    await fsp.writeFile(tmp, JSON.stringify(note), "utf8");
    await fsp.rename(tmp, doneFile);
    return true;
  } catch {
    return false;
  }
}

/** Append a worker's completion note to DECISIONS.md as human-legible bullets (so
 *  siblings re-reading DECISIONS.md and the board both see it). The orchestrator
 *  also reads the same note from the RUDDER_DONE block `rudder done` echoes to
 *  stdout. No jj management - just an append, exactly like appendDecision. */
export async function appendCompletionNote(
  repoRoot: string,
  note: CompletionNote,
  owner = "worker",
): Promise<void> {
  await ensureDecisionsFile(repoRoot);
  const id = (note.node || owner).trim() || "worker";
  const summary = (note.summary || "").replace(/\s+/g, " ").trim();
  const body: string[] = [`- **Did:** ${summary || "(no summary)"}`];
  const interfaces = (note.interfaces || "").replace(/\s+/g, " ").trim();
  if (interfaces) {
    body.push(`- **Interfaces:** ${interfaces}`);
  }
  const followups = (note.followups || []).filter((f) => f && (f.title || f.prompt));
  if (followups.length) {
    body.push("- **Follow-ups:**");
    for (const f of followups) {
      const title = (f.title || f.prompt || "").replace(/\s+/g, " ").trim();
      const deps = f.deps && f.deps.length ? ` [deps: ${f.deps.join(", ")}]` : "";
      const scope = f.scope === "out" ? " [out of lane]" : "";
      const why = f.why ? ` — ${f.why.replace(/\s+/g, " ").trim()}` : "";
      body.push(`  - ${title}${deps}${scope}${why}`);
    }
  }
  const entry = renderDecisionEntry({
    title: `${id}: ${decisionTitle(summary || "done")}`,
    body,
    owner: id,
  });
  await appendDecisionsEntry(repoRoot, entry);
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
  // Worktrees live inside the project; ignore them so worker checkouts are not untracked.
  await ensureLine(path.join(repoRoot, ".gitignore"), ".rudder-worktrees/");
  const workspaces = new Set<string>([repoRoot]);
  for (const node of Object.values(graph.nodes)) {
    if (node.worktree?.path) {
      workspaces.add(node.worktree.path);
    }
  }
  for (const workspace of workspaces) {
    await ensureRudderExcluded(workspace);
    await ensureDir(path.dirname(agentContextPath(workspace)));
    const filePath = agentContextPath(workspace);
    const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
    await fsp
      .writeFile(filePath, mergeGeneratedRudderMd(existing, content), "utf8")
      .catch(() => undefined);
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
