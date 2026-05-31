import { spawn } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import http from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { findProjectBySlug, loadProjects, loadRunRecord, outputPath, projectStateDir, runsDir } from "../state.js";
import { mergeJjRunIntoCurrentWorkspace } from "../jj.js";
import { hardParents, readGraph, softParents, updateGraph } from "../graph.js";
import { stopRun } from "../run-manager.js";
import type { RudderBus } from "../bus.js";
import { mergeNodeIntoIntegration, reconcileInjection } from "../scheduler.js";
import { nowIso } from "../util.js";
import type {
  BoardColumn,
  BoardEdge,
  BoardNode,
  BoardSnapshot,
  MemoryEntry,
  NodeStatus,
  ProjectEntry,
  ProjectSummary,
  RudderGraph,
  RunRecord,
  RunStatus,
  TaskNode,
} from "../types.js";

export type BoardDaemonHandle = {
  port: number;
  url: string;
  close: () => Promise<void>;
};

// dist/board/daemon.js sits next to dist/board/board.{js,css}, so the prebuilt
// SPA bundle resolves relative to this module's own URL.
const BOARD_JS_PATH = fileURLToPath(new URL("./board.js", import.meta.url));
const BOARD_CSS_PATH = fileURLToPath(new URL("./board.css", import.meta.url));

type SseClient = {
  res: ServerResponse;
};

// One Set of SSE clients per slug, plus one fs.watch per watched runs dir.
const sseClients = new Map<string, Set<SseClient>>();
const watchers = new Map<string, fs.FSWatcher>();
// A second watch per slug on the tracked DECISIONS.md at the repo root: it lives
// outside .rudder, so a sibling decision (or `rudder remember`) must trigger a
// re-broadcast (memory.updated SSE) the same way a run/graph change does.
const decisionsWatchers = new Map<string, fs.FSWatcher>();
const watchTimers = new Map<string, ReturnType<typeof setTimeout>>();

export async function startBoardDaemon(opts: {
  port: number;
  repoRoot: string;
  open?: boolean;
  bus?: RudderBus;
}): Promise<BoardDaemonHandle> {
  // Subscribe the SSE broadcaster to the shared bus: node.*/schedule.*/merge.*
  // events re-broadcast a fresh snapshot to every connected client (simplest
  // correct projection; the SPA always rebuilds from the snapshot on connect).
  let unsubscribe: (() => void) | undefined;
  if (opts.bus) {
    unsubscribe = opts.bus.subscribe(() => {
      for (const slug of sseClients.keys()) {
        void rebroadcastForSlug(slug);
      }
    });
  }

  const server = http.createServer((req, res) => {
    handleRequest(req, res, opts.bus).catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      if (!res.headersSent) {
        sendJson(res, 500, { error: message });
      } else {
        try {
          res.end();
        } catch {
          // ignore
        }
      }
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(opts.port, "127.0.0.1", () => {
      server.removeListener("error", reject);
      resolve();
    });
  });

  const address = server.address();
  const port = typeof address === "object" && address ? address.port : opts.port;
  const url = `http://127.0.0.1:${port}`;

  const close = async (): Promise<void> => {
    unsubscribe?.();
    for (const [, timer] of watchTimers) {
      clearTimeout(timer);
    }
    watchTimers.clear();
    for (const [, watcher] of watchers) {
      watcher.close();
    }
    watchers.clear();
    for (const [, watcher] of decisionsWatchers) {
      watcher.close();
    }
    decisionsWatchers.clear();
    for (const [, clients] of sseClients) {
      for (const client of clients) {
        try {
          client.res.end();
        } catch {
          // ignore
        }
      }
    }
    sseClients.clear();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  };

  // Determine this repo's slug for the open-browser convenience landing.
  let slug = "";
  try {
    const projects = await loadProjects();
    const resolved = path.resolve(opts.repoRoot);
    slug = projects.find((entry) => path.resolve(entry.repoRoot) === resolved)?.slug ?? "";
  } catch {
    slug = "";
  }

  if (opts.open) {
    openBrowser(slug ? `${url}/rudder/${slug}` : `${url}/rudder`);
  }

  return { port, url, close };
}

async function handleRequest(req: IncomingMessage, res: ServerResponse, bus?: RudderBus): Promise<void> {
  const url = new URL(req.url || "/", "http://127.0.0.1");
  const pathname = decodeURIComponent(url.pathname);
  const method = (req.method || "GET").toUpperCase();

  // Static SPA bundle.
  if (method === "GET" && pathname === "/board.js") {
    await sendStatic(res, BOARD_JS_PATH, "text/javascript; charset=utf-8");
    return;
  }
  if (method === "GET" && pathname === "/board.css") {
    await sendStatic(res, BOARD_CSS_PATH, "text/css; charset=utf-8");
    return;
  }

  // API routes: /api/projects[/:slug/...]
  if (pathname === "/api/projects" && method === "GET") {
    await handleProjectsList(res);
    return;
  }

  const apiMatch = pathname.match(/^\/api\/projects\/([^/]+)(\/.*)?$/);
  if (apiMatch) {
    const slug = apiMatch[1] ?? "";
    const rest = apiMatch[2] ?? "";
    await handleProjectApi(req, res, method, slug, rest, url, bus);
    return;
  }

  // SPA shell for the index and per-project routes.
  if (method === "GET" && (pathname === "/" || pathname === "/rudder")) {
    sendHtml(res, renderShell(""));
    return;
  }
  const slugMatch = pathname.match(/^\/rudder\/([^/]+)\/?$/);
  if (method === "GET" && slugMatch) {
    sendHtml(res, renderShell(slugMatch[1] ?? ""));
    return;
  }

  sendJson(res, 404, { error: "not found" });
}

async function handleProjectApi(
  req: IncomingMessage,
  res: ServerResponse,
  method: string,
  slug: string,
  rest: string,
  url: URL,
  bus?: RudderBus,
): Promise<void> {
  const project = await findProjectBySlug(slug);
  if (!project) {
    sendJson(res, 404, { error: `unknown project: ${slug}` });
    return;
  }

  if (rest === "/state" && method === "GET") {
    const snapshot = await buildSnapshot(project);
    sendJson(res, 200, snapshot);
    return;
  }

  if (rest === "/events" && method === "GET") {
    await handleSse(req, res, project);
    return;
  }

  if (rest === "/tasks" && method === "POST") {
    const body = await readJsonBody(req);
    const prompt = typeof body.prompt === "string" ? body.prompt.trim() : "";
    if (!prompt) {
      sendJson(res, 400, { error: "missing prompt" });
      return;
    }
    if (!bus) {
      sendJson(res, 503, { error: "scheduler not available" });
      return;
    }
    // The injection chokepoint: a typed task becomes a NEW node reconciled
    // against the frontier (never blindly appended), then the scheduler takes
    // over. Routes through reconcileInjection rather than plain startRun.
    const title = typeof body.title === "string" ? body.title.trim() : undefined;
    const result = await inRepo(project.repoRoot, () =>
      reconcileInjection(project.repoRoot, { prompt, ...(title ? { title } : {}) }, bus),
    );
    sendJson(res, 200, { nodeId: result.nodeId, nodeIds: [result.nodeId] });
    return;
  }

  const logMatch = rest.match(/^\/tasks\/([^/]+)\/log$/);
  if (logMatch && method === "GET") {
    const id = logMatch[1] ?? "";
    const tail = Number.parseInt(url.searchParams.get("tail") ?? "200", 10);
    const text = await readLogTail(project.repoRoot, id, Number.isFinite(tail) ? tail : 200);
    res.writeHead(200, { "content-type": "text/plain; charset=utf-8" });
    res.end(text);
    return;
  }

  // Approve a node in review: mark reviewState "approved" and merge it into the
  // integration trunk via the scheduler (the daemon-owned jj merge path).
  const approveMatch = rest.match(/^\/tasks\/([^/]+)\/approve$/);
  if (approveMatch && method === "POST") {
    const id = approveMatch[1] ?? "";
    if (!bus) {
      sendJson(res, 503, { error: "scheduler not available" });
      return;
    }
    const result = await inRepo(project.repoRoot, async () => {
      let target: TaskNode | undefined;
      await updateGraph(project.repoRoot, (graph) => {
        const node = graph.nodes[id] ?? Object.values(graph.nodes).find((candidate) => candidate.runId === id);
        if (node) {
          node.reviewState = "approved";
          node.updatedAt = nowIso();
          target = node;
        }
        return graph;
      });
      if (!target) {
        return { ok: false as const };
      }
      await mergeNodeIntoIntegration(project.repoRoot, target, bus);
      return { ok: true as const, nodeId: target.id };
    });
    if (!result.ok) {
      sendJson(res, 404, { error: `unknown node: ${id}` });
      return;
    }
    sendJson(res, 200, { ok: true, nodeId: result.nodeId });
    return;
  }

  const mergeMatch = rest.match(/^\/tasks\/([^/]+)\/merge$/);
  if (mergeMatch && method === "POST") {
    const id = mergeMatch[1] ?? "";
    // A graph node id routes through the daemon's integration merge (same path
    // as approve). A bare run id keeps the legacy run-merge for unmanaged runs.
    if (bus) {
      const graph = await readGraph(project.repoRoot);
      const node = graph.nodes[id] ?? Object.values(graph.nodes).find((candidate) => candidate.runId === id);
      if (node) {
        await inRepo(project.repoRoot, () => mergeNodeIntoIntegration(project.repoRoot, node, bus));
        const fresh = await readGraph(project.repoRoot);
        const refreshed = fresh.nodes[node.id];
        sendJson(res, 200, {
          status: refreshed?.status ?? "merged",
          conflictedFiles: [],
        });
        return;
      }
    }
    const run = await loadRunRecord(project.repoRoot, id);
    if (!run) {
      sendJson(res, 404, { error: `unknown run: ${id}` });
      return;
    }
    const merged = await inRepo(project.repoRoot, () => mergeJjRunIntoCurrentWorkspace(run));
    sendJson(res, 200, {
      status: merged.merge?.status ?? merged.status,
      conflictedFiles: merged.merge?.conflictedFiles ?? [],
    });
    return;
  }

  const cancelMatch = rest.match(/^\/tasks\/([^/]+)\/cancel$/);
  if (cancelMatch && method === "POST") {
    const id = cancelMatch[1] ?? "";
    await inRepo(project.repoRoot, () => stopRun(id, { silent: true }));
    sendJson(res, 200, { ok: true });
    return;
  }

  sendJson(res, 404, { error: "not found" });
}

async function handleProjectsList(res: ServerResponse): Promise<void> {
  const projects = await loadProjects();
  const summaries = await Promise.all(projects.map((entry) => buildProjectSummary(entry)));
  sendJson(res, 200, { projects: summaries });
}

// ---------------------------------------------------------------------------
// SSE: emit a full snapshot on connect, then deltas driven by fs.watch.
// ---------------------------------------------------------------------------

async function handleSse(
  req: IncomingMessage,
  res: ServerResponse,
  project: ProjectEntry,
): Promise<void> {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
  });
  res.write(": connected\n\n");

  const client: SseClient = { res };
  let set = sseClients.get(project.slug);
  if (!set) {
    set = new Set();
    sseClients.set(project.slug, set);
  }
  set.add(client);
  ensureWatcher(project);

  // Full snapshot on connect.
  const snapshot = await buildSnapshot(project);
  writeSseFrame(res, "snapshot", snapshot);

  const ping = setInterval(() => {
    try {
      res.write(": ping\n\n");
    } catch {
      // ignore
    }
  }, 15000);
  ping.unref?.();

  const cleanup = (): void => {
    clearInterval(ping);
    const clients = sseClients.get(project.slug);
    if (clients) {
      clients.delete(client);
      if (clients.size === 0) {
        sseClients.delete(project.slug);
        const watcher = watchers.get(project.slug);
        if (watcher) {
          watcher.close();
          watchers.delete(project.slug);
        }
        const decisionsWatcher = decisionsWatchers.get(project.slug);
        if (decisionsWatcher) {
          decisionsWatcher.close();
          decisionsWatchers.delete(project.slug);
        }
        const timer = watchTimers.get(project.slug);
        if (timer) {
          clearTimeout(timer);
          watchTimers.delete(project.slug);
        }
      }
    }
  };
  req.on("close", cleanup);
  res.on("close", cleanup);
}

function ensureWatcher(project: ProjectEntry): void {
  if (watchers.has(project.slug)) {
    return;
  }
  // Watch the whole .rudder dir recursively: this covers both runs/<id>/*.json
  // (worker-owned execution state) and graph.json (daemon-owned DAG topology),
  // so plan/edge changes broadcast a fresh snapshot just like run changes.
  const dir = projectStateDir(project.repoRoot);
  const runsPath = runsDir(project.repoRoot);
  try {
    fs.mkdirSync(runsPath, { recursive: true });
  } catch {
    // ignore
  }
  let watcher: fs.FSWatcher;
  try {
    watcher = fs.watch(dir, { recursive: true }, () => scheduleBroadcast(project));
  } catch {
    try {
      // Non-recursive fallback: watch the runs dir (most active) since some
      // platforms reject recursive watches.
      watcher = fs.watch(runsPath, () => scheduleBroadcast(project));
    } catch {
      return;
    }
  }
  watcher.on("error", () => undefined);
  watchers.set(project.slug, watcher);
  ensureDecisionsWatcher(project);
}

// Watch the repo-root DECISIONS.md (created on first render if absent) so an
// agent or `rudder remember` appending a decision fires a re-broadcast and the
// memory.updated SSE, exactly like a run/graph change inside .rudder.
function ensureDecisionsWatcher(project: ProjectEntry): void {
  if (decisionsWatchers.has(project.slug)) {
    return;
  }
  const decisionsPath = path.join(project.repoRoot, "DECISIONS.md");
  try {
    // Watching a possibly-absent file throws; watch the repo root and filter for
    // DECISIONS.md so the surface is covered even before it is first created.
    const watcher = fs.watch(project.repoRoot, (_event, filename) => {
      if (!filename || filename === "DECISIONS.md") {
        scheduleBroadcast(project);
      }
    });
    watcher.on("error", () => undefined);
    decisionsWatchers.set(project.slug, watcher);
  } catch {
    // Fall back to a direct file watch if the dir watch is rejected.
    try {
      const watcher = fs.watch(decisionsPath, () => scheduleBroadcast(project));
      watcher.on("error", () => undefined);
      decisionsWatchers.set(project.slug, watcher);
    } catch {
      // ignore: the .rudder watch + bus still cover most updates.
    }
  }
}

function scheduleBroadcast(project: ProjectEntry): void {
  const existing = watchTimers.get(project.slug);
  if (existing) {
    clearTimeout(existing);
  }
  const timer = setTimeout(() => {
    watchTimers.delete(project.slug);
    void broadcastSnapshot(project);
  }, 50);
  timer.unref?.();
  watchTimers.set(project.slug, timer);
}

// Phase 2 keeps deltas simple: recompute the snapshot and diff node ids so the
// SPA receives node.added/node.updated/node.removed frames, mirroring the
// cloud broadcast pattern (one payload, fan out to every client for the slug).
const lastNodes = new Map<string, Map<string, BoardNode>>();
const lastEdges = new Map<string, Map<string, BoardEdge>>();
const lastMemory = new Map<string, string>();

function edgeKey(edge: BoardEdge): string {
  return `${edge.from}->${edge.to}:${edge.kind}`;
}

// Bus-driven re-broadcast: resolve the slug to a project and emit deltas. Used
// when a scheduler/merge event fires so connected clients refresh without
// waiting for the fs.watch debounce.
async function rebroadcastForSlug(slug: string): Promise<void> {
  const clients = sseClients.get(slug);
  if (!clients || clients.size === 0) {
    return;
  }
  const project = await findProjectBySlug(slug);
  if (project) {
    await broadcastSnapshot(project);
  }
}

async function broadcastSnapshot(project: ProjectEntry): Promise<void> {
  const clients = sseClients.get(project.slug);
  if (!clients || clients.size === 0) {
    return;
  }
  const snapshot = await buildSnapshot(project);
  const previous = lastNodes.get(project.slug) ?? new Map<string, BoardNode>();
  const current = new Map<string, BoardNode>();
  for (const node of snapshot.nodes) {
    current.set(node.id, node);
  }

  const frames: Array<{ event: string; data: unknown }> = [];
  for (const [id, node] of current) {
    const prior = previous.get(id);
    if (!prior) {
      frames.push({ event: "node.added", data: node });
    } else if (JSON.stringify(prior) !== JSON.stringify(node)) {
      frames.push({ event: "node.updated", data: node });
    }
  }
  for (const [id] of previous) {
    if (!current.has(id)) {
      frames.push({ event: "node.removed", data: { id } });
    }
  }
  lastNodes.set(project.slug, current);

  // Edge deltas from graph.json: edge.added / edge.removed for the Nest view.
  const previousEdges = lastEdges.get(project.slug) ?? new Map<string, BoardEdge>();
  const currentEdges = new Map<string, BoardEdge>();
  for (const edge of snapshot.edges) {
    currentEdges.set(edgeKey(edge), edge);
  }
  for (const [key, edge] of currentEdges) {
    if (!previousEdges.has(key)) {
      frames.push({ event: "edge.added", data: edge });
    }
  }
  for (const [key, edge] of previousEdges) {
    if (!currentEdges.has(key)) {
      frames.push({ event: "edge.removed", data: { from: edge.from, to: edge.to } });
    }
  }
  lastEdges.set(project.slug, currentEdges);

  const memoryKey = JSON.stringify(snapshot.memory);
  if (lastMemory.get(project.slug) !== memoryKey) {
    lastMemory.set(project.slug, memoryKey);
    frames.push({ event: "memory.updated", data: { memory: snapshot.memory } });
  }

  if (frames.length === 0) {
    return;
  }
  for (const client of clients) {
    for (const frame of frames) {
      writeSseFrame(client.res, frame.event, frame.data);
    }
  }
}

function writeSseFrame(res: ServerResponse, event: string, data: unknown): void {
  try {
    res.write(`event: ${event}\n`);
    res.write(`data: ${JSON.stringify(data)}\n\n`);
  } catch {
    // ignore broken pipe
  }
}

// ---------------------------------------------------------------------------
// Projection: RunRecord -> BoardNode / BoardSnapshot / ProjectSummary.
// ---------------------------------------------------------------------------

export function columnForStatus(status: RunStatus): BoardNode["column"] {
  switch (status) {
    case "created":
      return "todo";
    case "running":
    case "steering":
    case "verifying":
      return "running";
    case "completed":
    case "merge-conflict":
      return "review";
    case "merged":
    case "failed":
    case "cancelled":
      return "done";
    default:
      return "todo";
  }
}

function projectRunToNode(run: RunRecord, lastLine: string | null): BoardNode {
  return {
    id: run.id,
    title: run.taskSummary || run.task,
    status: run.status,
    column: columnForStatus(run.status),
    blocked: false,
    backend: run.backend,
    model: run.model,
    effort: run.effort,
    lastLine,
    tokens: null,
    deps: { hard: [], soft: [] },
    createdAt: run.createdAt,
    updatedAt: run.updatedAt,
    worktree: run.worktree
      ? { path: run.worktree.path, workspaceName: run.worktree.workspaceName }
      : null,
    merge: run.merge ?? null,
  };
}

function columnForNodeStatus(status: NodeStatus): BoardColumn {
  switch (status) {
    case "planned":
    case "ready":
      return "todo";
    case "running":
      return "running";
    case "review":
      return "review";
    case "merged":
    case "failed":
      return "done";
    case "blocked":
      return "todo";
    default:
      return "todo";
  }
}

// Project a planner TaskNode (one that has not been scheduled into a run yet)
// into a BoardNode. Run-derived nodes stay authoritative for scheduled nodes;
// these fill in the not-yet-scheduled DAG. Deps come from incoming edges.
function projectTaskNodeToBoardNode(
  node: TaskNode,
  deps: { hard: string[]; soft: string[] },
): BoardNode {
  return {
    id: node.id,
    title: node.title,
    // RunStatus and NodeStatus share running/merged/failed; the SPA reads
    // `column` for layout, so the broad status string is sufficient here.
    status: node.status as unknown as RunStatus,
    column: columnForNodeStatus(node.status),
    blocked: node.status === "blocked",
    backend: node.backend,
    model: node.model,
    effort: node.effort,
    lastLine: node.lastLine ?? null,
    tokens: node.tokens ?? null,
    deps,
    createdAt: node.createdAt,
    updatedAt: node.updatedAt,
    worktree: node.worktree ? { path: node.worktree.path, workspaceName: node.worktree.workspaceName } : null,
    merge: null,
  };
}

// Build {from-node-id -> {hard,soft}} maps so each BoardNode carries the ids of
// its incoming parents, and the flat BoardEdge list for the Nest/DAG view.
function projectGraphEdges(graph: RudderGraph): { edges: BoardEdge[]; depsByNode: Map<string, { hard: string[]; soft: string[] }> } {
  const edges: BoardEdge[] = [];
  const depsByNode = new Map<string, { hard: string[]; soft: string[] }>();
  for (const id of Object.keys(graph.nodes)) {
    depsByNode.set(id, { hard: hardParents(graph, id), soft: softParents(graph, id) });
  }
  for (const edge of Object.values(graph.edges)) {
    edges.push({ from: edge.from, to: edge.to, kind: edge.type });
  }
  return { edges, depsByNode };
}

async function buildSnapshot(project: ProjectEntry): Promise<BoardSnapshot> {
  const runs = await listProjectRuns(project.repoRoot);
  const graph = await readGraph(project.repoRoot);
  const { edges, depsByNode } = projectGraphEdges(graph);

  const nodes: BoardNode[] = [];
  // Run-derived nodes are authoritative. Track which graph nodes they cover so
  // we do not double-list a node that has already been scheduled into a run.
  const coveredNodeIds = new Set<string>();
  for (const node of Object.values(graph.nodes)) {
    if (node.runId) {
      coveredNodeIds.add(node.runId);
    }
  }

  for (const run of runs) {
    const lastLine = await lastNonEmptyLine(project.repoRoot, run.id);
    const boardNode = projectRunToNode(run, lastLine);
    // If a graph node points at this run, carry its DAG deps onto the run node.
    const graphNode = Object.values(graph.nodes).find((candidate) => candidate.runId === run.id);
    if (graphNode) {
      boardNode.deps = depsByNode.get(graphNode.id) ?? boardNode.deps;
    }
    nodes.push(boardNode);
  }

  // Planner nodes that have not been scheduled yet (no runId) fill in the DAG.
  for (const node of Object.values(graph.nodes)) {
    if (node.runId) {
      continue;
    }
    nodes.push(projectTaskNodeToBoardNode(node, depsByNode.get(node.id) ?? { hard: [], soft: [] }));
  }

  const memory = await loadMemory(project.repoRoot);
  return {
    slug: project.slug,
    name: project.name,
    generatedAt: new Date().toISOString(),
    nodes,
    edges,
    gates: [],
    memory,
  };
}

async function buildProjectSummary(project: ProjectEntry): Promise<ProjectSummary> {
  const runs = await listProjectRuns(project.repoRoot);
  const counts = { todo: 0, running: 0, review: 0, done: 0, blocked: 0, failed: 0 };
  let lastActivityAt = "";
  for (const run of runs) {
    counts[columnForStatus(run.status)] += 1;
    if (run.status === "failed") {
      counts.failed += 1;
    }
    const stamp = run.updatedAt || run.createdAt;
    if (stamp && stamp > lastActivityAt) {
      lastActivityAt = stamp;
    }
  }
  return {
    slug: project.slug,
    name: project.name,
    repoRoot: project.repoRoot,
    counts,
    lastActivityAt: lastActivityAt || new Date(0).toISOString(),
  };
}

async function listProjectRuns(repoRoot: string): Promise<RunRecord[]> {
  const dir = runsDir(repoRoot);
  let entries: fs.Dirent[];
  try {
    entries = await fsp.readdir(dir, { withFileTypes: true });
  } catch {
    return [];
  }
  const runs = await Promise.all(
    entries
      .filter((entry) => entry.isDirectory())
      .map((entry) => loadRunRecord(repoRoot, entry.name)),
  );
  return runs
    .filter((run): run is RunRecord => Boolean(run))
    .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
}

async function lastNonEmptyLine(repoRoot: string, runId: string): Promise<string | null> {
  const text = await readOutput(repoRoot, runId);
  if (!text) {
    return null;
  }
  const lines = text.split(/\r?\n/);
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const trimmed = lines[i]?.trim();
    if (trimmed) {
      return trimmed.length > 500 ? trimmed.slice(0, 500) : trimmed;
    }
  }
  return null;
}

async function readOutput(repoRoot: string, runId: string): Promise<string> {
  try {
    return await fsp.readFile(outputPath(repoRoot, runId), "utf8");
  } catch {
    return "";
  }
}

async function readLogTail(repoRoot: string, runId: string, tail: number): Promise<string> {
  const text = await readOutput(repoRoot, runId);
  if (!text) {
    return "";
  }
  const lines = text.split(/\r?\n/);
  // A trailing newline yields an empty final element; drop it so tail=N counts
  // real lines rather than spending a slot on the blank line.
  if (lines.length > 0 && lines[lines.length - 1] === "") {
    lines.pop();
  }
  const count = Math.max(1, tail);
  return lines.slice(-count).join("\n");
}

/**
 * Memory view: parse DECISIONS.md (repo root) into bullet entries. Each
 * top-level "-" / "*" bullet becomes one MemoryEntry; absent file yields [].
 * A trailing "(owner: X, <iso>)" suffix (written by `rudder remember` and the
 * agent contract) is extracted into owner/ts; plain bullets just get text.
 */
export function parseDecisions(raw: string): MemoryEntry[] {
  const entries: MemoryEntry[] = [];
  for (const line of raw.split(/\r?\n/)) {
    const match = line.match(/^\s*[-*]\s+(.*)$/);
    const body = match?.[1]?.trim();
    if (!body) {
      continue;
    }
    entries.push(parseDecisionBullet(body));
  }
  return entries;
}

// Parse one bullet body. The optional "(owner: X, <iso>)" suffix is stripped off
// the text and split into owner + ts. A bullet may carry just "(owner: X)" or a
// bare "(<iso>)"; anything unrecognized stays part of the text.
function parseDecisionBullet(body: string): MemoryEntry {
  const suffix = body.match(/\s*\(owner:\s*([^,)]+?)(?:,\s*([^)]+?))?\)\s*$/i);
  if (suffix) {
    const text = body.slice(0, suffix.index).trim();
    const owner = suffix[1]?.trim();
    const ts = suffix[2]?.trim();
    return {
      text: text || body,
      ...(owner ? { owner } : {}),
      ...(ts ? { ts } : {}),
    };
  }
  return { text: body };
}

async function loadMemory(repoRoot: string): Promise<BoardSnapshot["memory"]> {
  let raw: string;
  try {
    raw = await fsp.readFile(path.join(repoRoot, "DECISIONS.md"), "utf8");
  } catch {
    return [];
  }
  return parseDecisions(raw);
}

// ---------------------------------------------------------------------------
// HTML shell. The SPA owns all CSS; this carries no inline styles.
// ---------------------------------------------------------------------------

function renderShell(slug: string): string {
  const slugJson = JSON.stringify(slug ?? "");
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>rudder</title>
    <link rel="stylesheet" href="/board.css" />
  </head>
  <body>
    <div id="app"></div>
    <script>window.__RUDDER_SLUG__ = ${slugJson}</script>
    <script type="module" src="/board.js"></script>
  </body>
</html>
`;
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/**
 * Run a callback with the process cwd pinned to repoRoot, so run-manager and jj
 * functions that resolve the repo from cwd act on the right project. The board
 * serializes mutating routes implicitly via this chdir swap.
 */
let repoChdirLock: Promise<unknown> = Promise.resolve();

function inRepo<T>(repoRoot: string, fn: () => Promise<T>): Promise<T> {
  const run = repoChdirLock.then(async () => {
    const previous = process.cwd();
    process.chdir(repoRoot);
    try {
      return await fn();
    } finally {
      try {
        process.chdir(previous);
      } catch {
        // ignore
      }
    }
  });
  repoChdirLock = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

async function readJsonBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  if (chunks.length === 0) {
    return {};
  }
  try {
    const parsed = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

async function sendStatic(res: ServerResponse, filePath: string, contentType: string): Promise<void> {
  try {
    const data = await fsp.readFile(filePath);
    res.writeHead(200, { "content-type": contentType });
    res.end(data);
  } catch {
    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    res.end("bundle not built");
  }
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(body));
}

function sendHtml(res: ServerResponse, body: string, status = 200): void {
  res.writeHead(status, { "content-type": "text/html; charset=utf-8" });
  res.end(body);
}

function openBrowser(url: string): void {
  const platform = process.platform;
  const command = platform === "darwin" ? "open" : platform === "win32" ? "cmd" : "xdg-open";
  const args = platform === "win32" ? ["/c", "start", "", url] : [url];
  const child = spawn(command, args, { detached: true, stdio: "ignore" });
  child.on("error", () => undefined);
  child.unref();
}
