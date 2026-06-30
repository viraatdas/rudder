import { spawn } from "node:child_process";
import { randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import fs from "node:fs";
import fsp from "node:fs/promises";
import http from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Per-daemon secret. The SPA shell embeds it (only loopback callers ever receive
// the shell — see the Host check below) and the browser echoes it back as the
// `x-rudder-token` header on every mutating request. A custom header forces a CORS
// preflight cross-origin (which this server never approves) and the value is
// unguessable, so a drive-by web page cannot forge a steer/inject/merge/cancel.
const BOARD_TOKEN = randomBytes(24).toString("hex");

// Cap request bodies so an unauthenticated POST can't buffer unbounded memory.
const MAX_BODY_BYTES = 1_000_000;

/** This process's board token (exported for tests; it is regenerated each start). */
export function getBoardToken(): string {
  return BOARD_TOKEN;
}

/** The board binds 127.0.0.1 only, so a legitimate Host header is always loopback.
 *  Rejecting anything else defeats DNS-rebinding: an attacker page whose hostname
 *  resolves to 127.0.0.1 still sends its own (non-loopback) Host. */
export function isLoopbackHost(req: IncomingMessage): boolean {
  const host = (req.headers.host ?? "").trim().toLowerCase();
  const name = host.replace(/:\d+$/, "");
  return name === "127.0.0.1" || name === "localhost" || name === "[::1]" || name === "::1";
}

function isLoopbackOrigin(origin: string): boolean {
  try {
    const h = new URL(origin).hostname.toLowerCase();
    return h === "127.0.0.1" || h === "localhost" || h === "::1";
  } catch {
    return false;
  }
}

export function hasValidToken(req: IncomingMessage): boolean {
  const header = req.headers["x-rudder-token"];
  const token = Array.isArray(header) ? header[0] : header;
  if (typeof token !== "string" || token.length !== BOARD_TOKEN.length) {
    return false;
  }
  try {
    return timingSafeEqual(Buffer.from(token), Buffer.from(BOARD_TOKEN));
  } catch {
    return false;
  }
}

/** Guard every state-mutating request. Returns true (after writing a 403) when the
 *  request must be denied: a non-loopback Origin, or a missing/invalid token. */
function denyMutation(req: IncomingMessage, res: ServerResponse): boolean {
  const origin = req.headers.origin;
  if (typeof origin === "string" && origin.length > 0 && !isLoopbackOrigin(origin)) {
    sendJson(res, 403, { error: "forbidden origin" });
    return true;
  }
  if (!hasValidToken(req)) {
    sendJson(res, 403, { error: "missing or invalid board token" });
    return true;
  }
  return false;
}

import { findProjectBySlug, loadProjects, loadRunRecord, outputPath, projectStateDir, runsDir } from "../state.js";
import { mergeJjRunIntoCurrentWorkspace } from "../jj.js";
import { hardParents, readGraph, softParents, updateGraph } from "../graph.js";
import { continueRun, stopRun } from "../run-manager.js";
import type { RudderBus } from "../bus.js";
import { mergeNodeIntoIntegration, reconcileInjection, withSchedulerLock } from "../scheduler.js";
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

export type BoardControlMode = "projector" | "scheduler";
type SteerReceiptPayload = {
  requestId: string;
  status: "queued" | "accepted" | "delivered" | "failed";
  mode: "queued" | "redirected" | "resumed";
  taskId?: string;
  error?: string;
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
  /** projector = native TUI owns PTYs; scheduler = this daemon owns workers. */
  controlMode?: BoardControlMode;
}): Promise<BoardDaemonHandle> {
  const controlMode = opts.controlMode ?? "projector";
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
    handleRequest(req, res, opts.bus, controlMode, opts.repoRoot).catch((error) => {
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
    slug = projects.find((entry) => sameRepoPath(entry.repoRoot, opts.repoRoot))?.slug ?? "";
  } catch {
    slug = "";
  }

  if (opts.open) {
    openBrowser(slug ? `${url}/rudder/${slug}` : `${url}/rudder`);
  }

  return { port, url, close };
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
  bus: RudderBus | undefined,
  controlMode: BoardControlMode,
  ownerRepoRoot: string,
): Promise<void> {
  const url = new URL(req.url || "/", "http://127.0.0.1");
  const pathname = decodeURIComponent(url.pathname);
  const method = (req.method || "GET").toUpperCase();

  // Anti-DNS-rebinding: the board is loopback-only, so reject any request whose Host
  // is not loopback before it can read state or serve the token-bearing shell.
  if (!isLoopbackHost(req)) {
    sendJson(res, 403, { error: "forbidden host" });
    return;
  }

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
    await handleProjectApi(req, res, method, slug, rest, url, bus, controlMode, ownerRepoRoot);
    return;
  }

  // SPA shell for the index and per-project routes.
  if (method === "GET" && (pathname === "/" || pathname === "/rudder")) {
    sendHtml(res, renderShell("", controlMode, false));
    return;
  }
  const slugMatch = pathname.match(/^\/rudder\/([^/]+)\/?$/);
  if (method === "GET" && slugMatch) {
    const slug = slugMatch[1] ?? "";
    const project = await findProjectBySlug(slug);
    const canMutate = Boolean(
      project && sameRepoPath(project.repoRoot, ownerRepoRoot),
    );
    sendHtml(res, renderShell(slug, controlMode, canMutate));
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
  controlMode: BoardControlMode = "projector",
  ownerRepoRoot = "",
): Promise<void> {
  const project = await findProjectBySlug(slug);
  if (!project) {
    sendJson(res, 404, { error: `unknown project: ${slug}` });
    return;
  }

  // Every mutating route is a POST. Require the secret token (+ loopback origin)
  // before any of them run, so a cross-site page cannot steer/inject/merge/cancel.
  if (method === "POST" && denyMutation(req, res)) {
    return;
  }
  // One daemon owns exactly one repository's scheduler or native-projector
  // channel. Other registered projects remain readable in the overview, but
  // mutations here would route work to the wrong owner.
  if (
    method === "POST" &&
    ownerRepoRoot &&
    !sameRepoPath(project.repoRoot, ownerRepoRoot)
  ) {
    sendJson(res, 409, {
      error: "this board daemon is read-only for that project; open its own Rudder board to make changes",
    });
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

  const steerReceiptMatch = rest.match(/^\/steer-receipts\/([A-Za-z0-9-]+)$/);
  if (steerReceiptMatch && method === "GET") {
    const requestId = steerReceiptMatch[1] ?? "";
    const receipt = await readSteerReceipt(project.repoRoot, requestId);
    if (receipt) {
      sendJson(res, receipt.status === "queued" ? 202 : 200, receipt);
    } else {
      sendJson(res, 202, { requestId, status: "queued", mode: "queued" });
    }
    return;
  }

  if (rest === "/tasks" && method === "POST") {
    const body = await readJsonBody(req);
    const prompt = typeof body.prompt === "string" ? body.prompt.trim() : "";
    if (!prompt) {
      sendJson(res, 400, { error: "missing prompt" });
      return;
    }
    // In projector mode graph.json is a one-way mirror written by the native
    // dashboard. Queue the request through its durable control inbox so the TUI
    // handles it exactly like text entered in the task pane. Writing graph.json
    // here would create a card the TUI never schedules.
    if (controlMode === "projector") {
      const queued = await writeSteerRequest(
        project.repoRoot,
        "conductor",
        prompt,
        "task",
        requestIdFromBody(body),
      );
      sendJson(res, queued.status === "queued" ? 202 : 200, { ...queued, nodeIds: [] });
      return;
    }
    if (!bus) {
      sendJson(res, 503, { error: "scheduler not available" });
      return;
    }
    const requestId = requestIdFromBody(body);
    const claimed = await claimSteerRequest(project.repoRoot, requestId, "conductor", prompt);
    if (!claimed) {
      const existing = await readSteerReceipt(project.repoRoot, requestId);
      if (existing?.taskId) {
        sendJson(res, 200, {
          requestId,
          status: existing.status,
          nodeId: existing.taskId,
          nodeIds: [existing.taskId],
        });
      } else {
        sendJson(res, 202, { requestId, status: "queued", nodeIds: [] });
      }
      return;
    }
    // The injection chokepoint: a typed task becomes a NEW node reconciled
    // against the frontier (never blindly appended), then the scheduler takes
    // over. Routes through reconcileInjection rather than plain startRun.
    const title = typeof body.title === "string" ? body.title.trim() : undefined;
    const result = await inRepo(project.repoRoot, () =>
      reconcileInjection(project.repoRoot, { prompt, ...(title ? { title } : {}) }, bus),
    );
    await writeSteerReceipt(project.repoRoot, {
      requestId,
      status: "accepted",
      mode: "queued",
      taskId: result.nodeId,
    });
    sendJson(res, 200, { requestId, nodeId: result.nodeId, nodeIds: [result.nodeId] });
    return;
  }

  const logMatch = rest.match(/^\/tasks\/([^/]+)\/log$/);
  if (logMatch && method === "GET") {
    const id = logMatch[1] ?? "";
    const tail = Number.parseInt(url.searchParams.get("tail") ?? "200", 10);
    const target = await resolveSteerTarget(project.repoRoot, id);
    const text = await readLogTail(
      project.repoRoot,
      target?.run.id ?? id,
      Number.isFinite(tail) ? tail : 200,
    );
    res.writeHead(200, { "content-type": "text/plain; charset=utf-8" });
    res.end(text);
    return;
  }

  // Approve a node in review: mark reviewState "approved" and merge it into the
  // integration trunk via the scheduler (the daemon-owned jj merge path).
  const approveMatch = rest.match(/^\/tasks\/([^/]+)\/approve$/);
  if (approveMatch && method === "POST") {
    const id = approveMatch[1] ?? "";
    if (controlMode === "projector") {
      const queued = await writeSteerRequest(project.repoRoot, id, "merge", "merge");
      sendJson(res, queued.status === "queued" ? 202 : 200, queued);
      return;
    }
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
      // Serialize against the auto-scheduler's ticks/transitions so a manual
      // approve never merges against a stale integration head.
      await withSchedulerLock(project.repoRoot, () => mergeNodeIntoIntegration(project.repoRoot, target!, bus));
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
    if (controlMode === "projector") {
      const queued = await writeSteerRequest(project.repoRoot, id, "merge", "merge");
      sendJson(res, queued.status === "queued" ? 202 : 200, {
        ...queued,
        conflictedFiles: [],
      });
      return;
    }
    // A graph node id routes through the daemon's integration merge (same path
    // as approve). A bare run id keeps the legacy run-merge for unmanaged runs.
    if (bus) {
      const graph = await readGraph(project.repoRoot);
      const node = graph.nodes[id] ?? Object.values(graph.nodes).find((candidate) => candidate.runId === id);
      if (node) {
        await inRepo(project.repoRoot, () =>
          withSchedulerLock(project.repoRoot, () => mergeNodeIntoIntegration(project.repoRoot, node, bus)),
        );
        const fresh = await readGraph(project.repoRoot);
        const refreshed = fresh.nodes[node.id];
        // A graph node that did not merge (blocked) signals a conflict to the UI.
        const conflicted = refreshed?.status === "blocked" || refreshed?.merge?.status === "conflict";
        sendJson(res, 200, {
          status: conflicted ? "merge-conflict" : refreshed?.status ?? "merged",
          conflictedFiles: refreshed?.merge?.conflictedFiles ?? [],
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
    // mergeJjRunIntoCurrentWorkspace records merge.status "conflict"; normalize to
    // the single "merge-conflict" signal the UI reacts to so server + UI agree.
    const conflicted = merged.merge?.status === "conflict" || merged.status === "merge-conflict";
    sendJson(res, 200, {
      status: conflicted ? "merge-conflict" : merged.merge?.status ?? merged.status,
      conflictedFiles: merged.merge?.conflictedFiles ?? [],
    });
    return;
  }

  const cancelMatch = rest.match(/^\/tasks\/([^/]+)\/cancel$/);
  if (cancelMatch && method === "POST") {
    const id = cancelMatch[1] ?? "";
    if (controlMode === "projector") {
      const queued = await writeSteerRequest(project.repoRoot, id, "cancel", "cancel");
      sendJson(res, queued.status === "queued" ? 202 : 200, queued);
      return;
    }
    const graph = await readGraph(project.repoRoot);
    const node = graph.nodes[id] ?? Object.values(graph.nodes).find((candidate) => candidate.runId === id);
    await inRepo(project.repoRoot, () => stopRun(node?.runId ?? id, { silent: true }));
    sendJson(res, 200, { ok: true });
    return;
  }

  // Steer a running agent (or the conductor) from the browser. We drop a JSON
  // instruction into the .rudder/steer/ inbox; the native TUI polls it each tick
  // and injects the text straight into the matching agent's PTY (see
  // poll_steer_inbox in native/src/main.rs). File-based so it works whether or
  // not the daemon owns the scheduler (projector-only TUI sessions included).
  const steerMatch = rest.match(/^\/tasks\/([^/]+)\/steer$/);
  if (steerMatch && method === "POST") {
    const id = steerMatch[1] ?? "";
    const body = await readJsonBody(req);
    const instruction = typeof body.instruction === "string"
      ? body.instruction.trim()
      : typeof body.text === "string"
        ? body.text.trim()
        : "";
    if (!instruction) {
      sendJson(res, 400, { error: "missing instruction" });
      return;
    }
    if (instruction.length > 8_000) {
      sendJson(res, 413, { error: "instruction is too long (maximum 8000 characters)" });
      return;
    }
    if (controlMode === "scheduler") {
      if (!bus) {
        sendJson(res, 503, { error: "scheduler not available" });
        return;
      }
      const requestId = requestIdFromBody(body);
      const claimed = await claimSteerRequest(project.repoRoot, requestId, id, instruction);
      if (!claimed) {
        const existing = await readSteerReceipt(project.repoRoot, requestId);
        sendJson(res, existing?.status === "queued" ? 202 : 200, existing ?? {
          requestId,
          status: "queued",
          mode: "queued",
          taskId: id,
        });
        return;
      }
      const result = await inRepo(project.repoRoot, () =>
        withSchedulerLock(
          project.repoRoot,
          () => steerHeadlessRun(project.repoRoot, id, instruction, bus, requestId),
        ),
      );
      if (!result.ok) {
        await writeSteerReceipt(project.repoRoot, {
          requestId,
          status: "failed",
          mode: "queued",
          taskId: id,
          error: result.error,
        });
        sendJson(res, result.status, { error: result.error });
        return;
      }
      await writeSteerReceipt(project.repoRoot, result.receipt);
      sendJson(res, 200, result.receipt);
      return;
    }

    const target = await resolveSteerTarget(project.repoRoot, id);
    if (!target) {
      sendJson(res, 404, { error: `unknown task: ${id}` });
      return;
    }
    if (!isSteerable(target.run.status, target.node?.status)) {
      sendJson(res, 409, { error: `task is ${target.node?.status ?? target.run.status} and cannot be steered` });
      return;
    }
    const queued = await writeSteerRequest(
      project.repoRoot,
      target.node?.id ?? id,
      instruction,
      "steer",
      requestIdFromBody(body),
    );
    sendJson(res, queued.status === "queued" ? 202 : 200, queued);
    return;
  }
  // Conductor-level steer: same inbox, target "conductor".
  if (rest === "/steer" && method === "POST") {
    const body = await readJsonBody(req);
    const instruction = typeof body.instruction === "string"
      ? body.instruction.trim()
      : typeof body.text === "string"
        ? body.text.trim()
        : "";
    if (!instruction) {
      sendJson(res, 400, { error: "missing instruction" });
      return;
    }
    if (instruction.length > 8_000) {
      sendJson(res, 413, { error: "instruction is too long (maximum 8000 characters)" });
      return;
    }
    if (controlMode === "scheduler") {
      sendJson(res, 409, {
        error: "the standalone board has no live conductor; create a task or steer a running worker instead",
      });
      return;
    }
    const queued = await writeSteerRequest(
      project.repoRoot,
      "conductor",
      instruction,
      "steer",
      requestIdFromBody(body),
    );
    sendJson(res, queued.status === "queued" ? 202 : 200, queued);
    return;
  }

  sendJson(res, 404, { error: "not found" });
}

// Write one steer request into .rudder/steer/. Filename is timestamp-prefixed so
// the native poller applies queued steers in order; the id is sanitized so a node
// id never escapes the inbox dir.
async function writeSteerRequest(
  repoRoot: string,
  taskId: string,
  instruction: string,
  kind: "steer" | "task" | "merge" | "cancel",
  requestedRequestId?: string,
): Promise<{ ok: true } & SteerReceiptPayload> {
  const dir = path.join(projectStateDir(repoRoot), "steer");
  await fsp.mkdir(dir, { recursive: true });
  await pruneSteerReceipts(repoRoot).catch(() => undefined);
  const ts = Date.now();
  const requestId = requestedRequestId ?? randomUUID();
  const claimed = await claimSteerRequest(repoRoot, requestId, taskId, instruction);
  if (!claimed) {
    const existing = await readSteerReceipt(repoRoot, requestId);
    return { ok: true, ...(existing ?? { requestId, status: "queued", mode: "queued", taskId }) };
  }
  const safeId = (taskId || "conductor").replace(/[^A-Za-z0-9_.-]/g, "-").slice(0, 64) || "conductor";
  // hrtime + UUID prevents same-millisecond steers from overwriting each other;
  // the zero-padded wall-clock prefix retains delivery order for the TUI poller.
  const sequence = process.hrtime.bigint().toString().padStart(20, "0");
  const file = path.join(dir, `${String(ts).padStart(13, "0")}-${sequence}-${safeId}-${requestId}.json`);
  const tmp = `${file}.${process.pid}.tmp`;
  await fsp.writeFile(tmp, JSON.stringify({ requestId, kind, taskId, instruction, ts: new Date(ts).toISOString() }));
  await fsp.rename(tmp, file);
  return { ok: true, requestId, status: "queued", mode: "queued", taskId };
}

function requestIdFromBody(body: Record<string, unknown>): string {
  const value = typeof body.requestId === "string" ? body.requestId.trim() : "";
  return /^[A-Za-z0-9-]{8,128}$/.test(value) ? value : randomUUID();
}

async function claimSteerRequest(
  repoRoot: string,
  requestId: string,
  taskId: string,
  instruction: string,
): Promise<boolean> {
  const dir = path.join(projectStateDir(repoRoot), "steer-claims");
  await fsp.mkdir(dir, { recursive: true });
  try {
    await fsp.writeFile(
      path.join(dir, `${requestId}.json`),
      JSON.stringify({ requestId, taskId, instruction, claimedAt: nowIso() }),
      { flag: "wx" },
    );
    return true;
  } catch (error) {
    if (error && typeof error === "object" && "code" in error && error.code === "EEXIST") {
      return false;
    }
    throw error;
  }
}

async function writeSteerReceipt(repoRoot: string, receipt: SteerReceiptPayload): Promise<void> {
  const dir = path.join(projectStateDir(repoRoot), "steer-receipts");
  await fsp.mkdir(dir, { recursive: true });
  const file = path.join(dir, `${receipt.requestId}.json`);
  const temp = `${file}.${process.pid}.tmp`;
  await fsp.writeFile(temp, JSON.stringify(receipt));
  await fsp.rename(temp, file);
}

async function pruneSteerReceipts(repoRoot: string): Promise<void> {
  const dir = path.join(projectStateDir(repoRoot), "steer-receipts");
  const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
  const files = await Promise.all(entries
    .filter((entry) => entry.isFile() && entry.name.endsWith(".json"))
    .map(async (entry) => ({
      path: path.join(dir, entry.name),
      mtimeMs: await fsp.stat(path.join(dir, entry.name)).then((stat) => stat.mtimeMs).catch(() => 0),
    })));
  files.sort((left, right) => left.mtimeMs - right.mtimeMs);
  const now = Date.now();
  for (let index = 0; index < files.length; index += 1) {
    const file = files[index]!;
    const expired = now - file.mtimeMs > 24 * 60 * 60 * 1_000;
    const overCapAndObserved = files.length - index > 1_000 && now - file.mtimeMs > 5 * 60 * 1_000;
    if (expired || overCapAndObserved) {
      await fsp.unlink(file.path).catch(() => undefined);
      await fsp.unlink(
        path.join(projectStateDir(repoRoot), "steer-claims", path.basename(file.path)),
      ).catch(() => undefined);
    }
  }
}

async function readSteerReceipt(
  repoRoot: string,
  requestId: string,
): Promise<SteerReceiptPayload | null> {
  if (!/^[A-Za-z0-9-]{8,128}$/.test(requestId)) return null;
  const file = path.join(projectStateDir(repoRoot), "steer-receipts", `${requestId}.json`);
  const value = await fsp.readFile(file, "utf8")
    .then((raw) => JSON.parse(raw) as Record<string, unknown>)
    .catch(() => null);
  if (!value) return null;
  if (value.status === "processing") {
    return { requestId, status: "queued", mode: "queued" };
  }
  if (
    value.status !== "queued" &&
    value.status !== "accepted" &&
    value.status !== "delivered" &&
    value.status !== "failed"
  ) return null;
  return {
    requestId,
    status: value.status,
    mode: value.mode === "redirected" || value.mode === "resumed" ? value.mode : "queued",
    ...(typeof value.taskId === "string" ? { taskId: value.taskId } : {}),
    ...(typeof value.error === "string" ? { error: value.error } : {}),
  };
}

async function resolveSteerTarget(
  repoRoot: string,
  id: string,
): Promise<{ run: RunRecord; node?: TaskNode } | null> {
  const graph = await readGraph(repoRoot);
  const node = graph.nodes[id] ?? Object.values(graph.nodes).find((candidate) => candidate.runId === id);
  const runId = node?.runId ?? id;
  if (!runId) return null;
  const run = await loadRunRecord(repoRoot, runId);
  return run ? { run, ...(node ? { node } : {}) } : null;
}

function isSteerable(
  runStatus: RunStatus,
  nodeStatus?: NodeStatus,
  runMode?: RunRecord["mode"],
): boolean {
  // Native TUI records may contain runtime-only mode strings outside the TS
  // union. A standalone headless daemon must never relaunch those as execute
  // workers. Undefined remains accepted for legacy headless records.
  if (runMode && runMode !== "execute") return false;
  if (nodeStatus === "merged" || nodeStatus === "blocked" || nodeStatus === "failed") return false;
  return runStatus === "created" ||
    runStatus === "running" ||
    runStatus === "steering" ||
    runStatus === "verifying" ||
    runStatus === "completed";
}

async function steerHeadlessRun(
  repoRoot: string,
  id: string,
  instruction: string,
  bus: RudderBus,
  requestId: string,
): Promise<
  | { ok: true; receipt: { ok: true; requestId: string; status: "accepted"; mode: "redirected" | "resumed"; taskId: string } }
  | { ok: false; status: 404 | 409; error: string }
> {
  const target = await resolveSteerTarget(repoRoot, id);
  if (!target) {
    return { ok: false, status: 404, error: `unknown task: ${id}` };
  }
  if (!isSteerable(target.run.status, target.node?.status, target.run.mode)) {
    return {
      ok: false,
      status: 409,
      error: `task is ${target.node?.status ?? target.run.status} and cannot be steered`,
    };
  }

  const active = target.run.status === "created" ||
    target.run.status === "running" ||
    target.run.status === "steering" ||
    target.run.status === "verifying";
  const mode = active ? "redirected" as const : "resumed" as const;

  // A live run without an attempt id may belong to a pre-upgrade worker whose
  // unconditional writes cannot participate in the new ownership CAS. Review
  // feedback is safe once that worker is finished; live redirect is not.
  if (active && !target.run.process?.attemptId) {
    return {
      ok: false,
      status: 409,
      error: "this live worker predates safe web redirects; let it reach Review, then request changes",
    };
  }

  try {
    await continueRun({
      runId: target.run.id,
      prompt: instruction,
      interrupt: active,
      silent: true,
    });
  } catch (error) {
    return {
      ok: false,
      status: 409,
      error: error instanceof Error ? error.message : String(error),
    };
  }

  if (target.node) {
    await updateGraph(repoRoot, (graph) => {
      const node = graph.nodes[target.node!.id];
      if (node) {
        node.status = "running";
        // Revised work must pass through Review again; never retain an approval
        // from the previous turn.
        delete node.reviewState;
        node.updatedAt = nowIso();
      }
      return graph;
    });
  }

  bus.publish({
    ts: nowIso(),
    runId: target.run.id,
    ...(target.node ? { nodeId: target.node.id } : {}),
    type: "node.running",
    message: mode === "redirected"
      ? `Redirected ${target.node?.id ?? target.run.id} from the web board`
      : `Reopened ${target.node?.id ?? target.run.id} with review feedback`,
    data: { instruction },
  });

  return {
    ok: true,
    receipt: {
      ok: true,
      requestId,
      // The attempt is installed, but the backend has not acknowledged model
      // delivery yet. "accepted" is terminal UI feedback without overstating
      // the handoff as model delivery.
      status: "accepted",
      mode,
      taskId: target.node?.id ?? target.run.id,
    },
  };
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
const lastActivity = new Map<string, string>();

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

  const activityKey = JSON.stringify(snapshot.activity ?? []);
  if (lastActivity.get(project.slug) !== activityKey) {
    lastActivity.set(project.slug, activityKey);
    frames.push({ event: "activity.updated", data: { activity: snapshot.activity ?? [] } });
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

function userUpdates(run: RunRecord): BoardNode["updates"] {
  const turns = run.turns ?? [];
  const start = turns[0]?.prompt === run.task ? 1 : 0;
  return turns
    .slice(start)
    .filter((turn) => turn.source === "user" || turn.source === "regoal")
    .map((turn) => ({ instruction: turn.prompt, ts: turn.ts, source: turn.source }));
}

function projectRunToNode(run: RunRecord, lastLine: string | null, graphNode?: TaskNode): BoardNode {
  return {
    // Keep the graph id stable across planned -> launched so an open issue drawer
    // does not disappear exactly when the worker starts. Pure runs use run.id.
    id: graphNode?.id ?? run.id,
    runId: run.id,
    title: graphNode?.title || run.taskSummary || run.task,
    status: graphNode?.status ?? run.status,
    column: graphNode ? columnForNodeStatus(graphNode.status) : columnForStatus(run.status),
    blocked: graphNode?.status === "blocked",
    backend: run.backend,
    model: run.model,
    effort: run.effort,
    lastLine,
    tokens: run.tokens ?? graphNode?.tokens ?? null,
    deps: { hard: [], soft: [] },
    createdAt: run.createdAt,
    updatedAt: run.updatedAt,
    worktree: run.worktree
      ? { path: run.worktree.path, workspaceName: run.worktree.workspaceName }
      : null,
    merge: run.merge ?? null,
    updates: userUpdates(run),
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
    ...(node.runId ? { runId: node.runId } : {}),
    title: node.title,
    // RunStatus and NodeStatus share running/merged/failed; the SPA reads
    // `column` for layout, so the broad status string is sufficient here.
    status: node.status,
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
    updates: [],
  };
}

// Build {from-node-id -> {hard,soft}} maps so each BoardNode carries the ids of
// its incoming parents, and the flat BoardEdge list for the Nest/DAG view.
function projectGraphEdges(graph: RudderGraph): { edges: BoardEdge[]; depsByNode: Map<string, { hard: string[]; soft: string[] }> } {
  const edges: BoardEdge[] = [];
  const depsByNode = new Map<string, { hard: string[]; soft: string[] }>();
  for (const id of Object.keys(graph.nodes)) {
    depsByNode.set(id, {
      hard: hardParents(graph, id),
      soft: softParents(graph, id),
    });
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
  for (const run of runs) {
    const lastLine = await lastNonEmptyLine(project.repoRoot, run.id);
    const graphNode =
      Object.values(graph.nodes).find((candidate) => candidate.runId === run.id) ??
      graph.nodes[run.id];
    const boardNode = projectRunToNode(run, lastLine, graphNode);
    // If a graph node points at this run (by runId, or by node id), the GRAPH node
    // is authoritative for scheduled nodes: the daemon writes merged/blocked/review
    // to graph.json, not run.json. Override the projected column/status/blocked so
    // merged nodes leave Review and blocked nodes surface. Pure run-derived nodes
    // (no graph entry) keep their run-projected status untouched.
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
  const activity = await loadActivity(project.repoRoot);
  return {
    slug: project.slug,
    name: project.name,
    generatedAt: new Date().toISOString(),
    nodes,
    edges,
    gates: [],
    memory,
    activity,
  };
}

async function buildProjectSummary(project: ProjectEntry): Promise<ProjectSummary> {
  const runs = await listProjectRuns(project.repoRoot);
  const counts = { todo: 0, running: 0, review: 0, done: 0, blocked: 0, failed: 0 };
  let lastActivityAt = "";
  for (const run of runs) {
    // Count each run exactly once so the per-status tallies sum to the node total.
    // failed/cancelled get their own bucket and do NOT also fall into `done`
    // (columnForStatus maps both to "done", which would double-count failed).
    if (run.status === "failed" || run.status === "cancelled") {
      counts.failed += 1;
    } else {
      counts[columnForStatus(run.status)] += 1;
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
 * Memory view: parse DECISIONS.md (repo root) into entries. The current format is one
 * `## title` block per decision, with `- **What:**` / `- **Did:**` / `- **Decision:**`
 * body lines and a `- **By:** owner · <iso>` footer; each block becomes one MemoryEntry
 * (text from the What/Did/Decision line or the title, owner+ts from By). Legacy top-level
 * `-`/`*` bullets with a trailing "(owner: X, <iso>)" suffix are still parsed for back
 * compat. Absent file yields [].
 */
export function parseDecisions(raw: string): MemoryEntry[] {
  const entries: MemoryEntry[] = [];
  const lines = raw.split(/\r?\n/);
  let i = 0;
  while (i < lines.length) {
    const heading = lines[i].match(/^##\s+(.*)$/);
    if (heading) {
      const title = heading[1].trim();
      let text: string | undefined;
      let owner: string | undefined;
      let ts: string | undefined;
      i++;
      // Gather the block until the next `## ` heading (or EOF).
      while (i < lines.length && !/^##\s+/.test(lines[i])) {
        const field = lines[i].trim().match(/^[-*]\s+\*\*([^:*]+):\*\*\s*(.*)$/);
        if (field) {
          const label = field[1].trim().toLowerCase();
          const value = field[2].trim();
          if (label === "by") {
            const by = value.match(/^(.*?)\s*·\s*(.*)$/);
            owner = (by ? by[1] : value).trim() || undefined;
            ts = by?.[2]?.trim() || undefined;
            // Normalize an epoch-ms stamp (the conductor's now_stamp) to ISO so the board
            // shows one consistent format alongside the worker/CLI ISO stamps.
            if (ts && /^\d{13}$/.test(ts)) {
              const iso = new Date(Number(ts)).toISOString();
              if (!Number.isNaN(Date.parse(iso))) {
                ts = iso;
              }
            }
          } else if (
            text === undefined &&
            (label === "what" || label === "did" || label === "decision")
          ) {
            text = value;
          }
        }
        i++;
      }
      entries.push({
        text: text || title,
        ...(owner ? { owner } : {}),
        ...(ts ? { ts } : {}),
      });
      continue;
    }
    // Legacy: a top-level "-"/"*" bullet (optionally with an "(owner: X, <iso>)" suffix).
    const bullet = lines[i].match(/^\s*[-*]\s+(.*)$/);
    const body = bullet?.[1]?.trim();
    if (body) {
      entries.push(parseDecisionBullet(body));
    }
    i++;
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

// Activity feed: the live narration stream the native TUI appends to
// .rudder/activity.jsonl (conductor actions, steer confirmations, periodic
// heartbeats). We tail the last N lines so a long session stays cheap, parse
// each JSON line, and normalize the ms timestamp to ISO for the UI.
const ACTIVITY_TAIL = 120;

export function parseActivityJsonl(raw: string): BoardSnapshot["activity"] {
  const out: NonNullable<BoardSnapshot["activity"]> = [];
  const lines = raw.split(/\r?\n/).filter((line) => line.trim().length > 0);
  for (const line of lines.slice(-ACTIVITY_TAIL)) {
    try {
      const parsed = JSON.parse(line) as { ts?: unknown; text?: unknown; kind?: unknown };
      const text = typeof parsed.text === "string" ? parsed.text : "";
      if (!text) {
        continue;
      }
      let ts: string | undefined;
      if (typeof parsed.ts === "string" && parsed.ts) {
        ts = /^\d{10,}$/.test(parsed.ts) ? new Date(Number(parsed.ts)).toISOString() : parsed.ts;
      } else if (typeof parsed.ts === "number") {
        ts = new Date(parsed.ts).toISOString();
      }
      const kind = parsed.kind === "heartbeat" ? "heartbeat" : "action";
      out.push({ text, kind, ...(ts ? { ts } : {}) });
    } catch {
      // skip a malformed line; the stream is best-effort
    }
  }
  return out;
}

async function loadActivity(repoRoot: string): Promise<BoardSnapshot["activity"]> {
  let raw: string;
  try {
    raw = await fsp.readFile(path.join(projectStateDir(repoRoot), "activity.jsonl"), "utf8");
  } catch {
    return [];
  }
  return parseActivityJsonl(raw);
}

// ---------------------------------------------------------------------------
// HTML shell. The SPA owns all CSS; this carries no inline styles.
// ---------------------------------------------------------------------------

function renderShell(slug: string, controlMode: BoardControlMode, canMutate: boolean): string {
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
    <script>window.__RUDDER_SLUG__ = ${slugJson}; window.__RUDDER_TOKEN__ = ${JSON.stringify(BOARD_TOKEN)}; window.__RUDDER_CONTROL_MODE__ = ${JSON.stringify(controlMode)}; window.__RUDDER_CAN_MUTATE__ = ${JSON.stringify(canMutate)}</script>
    <script type="module" src="/board.js"></script>
  </body>
</html>
`;
}

function sameRepoPath(left: string, right: string): boolean {
  const canonical = (value: string): string => {
    const resolved = path.resolve(value);
    try {
      return fs.realpathSync(resolved);
    } catch {
      return resolved;
    }
  };
  return canonical(left) === canonical(right);
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
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > MAX_BODY_BYTES) {
      // Oversized body: stop reading and treat as empty (DoS guard).
      req.destroy();
      return {};
    }
    chunks.push(buf);
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
