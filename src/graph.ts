import path from "node:path";
import type {
  BackendId,
  DepType,
  EdgeType,
  EffortLevel,
  GraphEdge,
  JsonValue,
  NodeStatus,
  RudderGraph,
  RunRecord,
  RunStatus,
  TaskNode,
} from "./types.js";
import { ensureProjectRuntimeIgnored, projectStateDir } from "./state.js";
import { nowIso, pathExists, readJson, shortHash, updateJson } from "./util.js";

// ---------------------------------------------------------------------------
// graph.json IO. The graph is the daemon-owned DAG topology. nodes/edges are
// objects keyed by id so two branches that each add nodes merge as a clean key
// union (no line conflicts). Keys are sorted on write for minimal diffs.
// ---------------------------------------------------------------------------

export function graphPath(repoRoot: string): string {
  return path.join(projectStateDir(repoRoot), "graph.json");
}

function emptyGraph(repoRoot: string): RudderGraph {
  return {
    version: 1,
    repoRoot,
    nodes: {},
    edges: {},
    updatedAt: nowIso(),
  };
}

/**
 * Read the graph for a repo. Returns an empty graph (not null) when graph.json
 * is absent or malformed, so callers never special-case the first-write path.
 */
export async function readGraph(repoRoot: string): Promise<RudderGraph> {
  const graph = await readJson<RudderGraph>(graphPath(repoRoot));
  if (graph && graph.version === 1 && graph.nodes && graph.edges) {
    return {
      version: 1,
      repoRoot: graph.repoRoot || repoRoot,
      ...(graph.integrationChangeId ? { integrationChangeId: graph.integrationChangeId } : {}),
      nodes: graph.nodes,
      edges: graph.edges,
      updatedAt: graph.updatedAt || nowIso(),
    };
  }
  return emptyGraph(repoRoot);
}

/**
 * Content-hashed node id: stable mergeable key from the title, the current
 * time, and a nonce so two nodes with the same title never collide.
 */
export function newNodeId(title: string): string {
  return shortHash(`${title}:${nowIso()}:${Math.random()}`);
}

export function newEdgeId(from: string, to: string): string {
  return shortHash(`${from}->${to}:${nowIso()}:${Math.random()}`);
}

/**
 * Atomically read-modify-write the graph under the per-path lock (daemon-only
 * writer). The transform mutates/returns the graph; keys are sorted and the
 * updatedAt stamp refreshed before write so diffs stay minimal.
 */
export async function updateGraph(
  repoRoot: string,
  fn: (graph: RudderGraph) => RudderGraph | void,
): Promise<RudderGraph> {
  // graph.json is mutable scheduler state, never source code. Install the
  // exclusion before the first write even when callers bypass `rudder plan`.
  await ensureProjectRuntimeIgnored(repoRoot);
  let result: RudderGraph = emptyGraph(repoRoot);
  const file = graphPath(repoRoot);
  const existed = await pathExists(file);
  await updateJson<RudderGraph>(file, (current) => {
    // Once a graph exists, a transient/contended read must never be interpreted
    // as an empty DAG. Throwing leaves the atomic file untouched and lets the
    // scheduler retry; silently using emptyGraph here can erase the whole fleet.
    if (existed && !(current && current.version === 1 && current.nodes && current.edges)) {
      throw new Error(`Refusing to replace an unreadable existing graph: ${file}`);
    }
    const base: RudderGraph =
      current && current.version === 1 && current.nodes && current.edges
        ? current
        : emptyGraph(repoRoot);
    const next = fn(base) ?? base;
    next.version = 1;
    next.repoRoot = next.repoRoot || repoRoot;
    next.updatedAt = nowIso();
    const sorted = sortGraph(next);
    result = sorted;
    return sorted as unknown as JsonValue;
  });
  return result;
}

// ---------------------------------------------------------------------------
// Plan mirror (write-only projection of the TUI's in-memory plan). The native
// TUI owns NO graph.json schema: it pipes a simple payload describing its plan
// to `rudder __graph-mirror`, which calls mirrorPlanIntoGraph below. graph.json
// is a write-only mirror the board reads; it is NEVER read back for scheduling.
// ---------------------------------------------------------------------------

/** A dependency edge in the mirror payload: this node depends `on` another. */
export type MirrorDep = { on: string; type: DepType };

/** One node in the mirror payload. Only `id`, `title`, `status`, and `deps` are
 * load-bearing; the rest enrich the board projection when the TUI knows them. */
export type MirrorNode = {
  id: string;
  title: string;
  prompt?: string;
  status: string;
  runId?: string;
  jjChangeId?: string;
  backend?: string;
  model?: string;
  effort?: string;
  worktreePath?: string;
  deps?: MirrorDep[];
};

export type MirrorPayload = { nodes?: MirrorNode[] };

/** Map a TUI status string onto the DAG-level NodeStatus vocabulary. Unknown
 * values fall back to "planned" so a malformed payload never crashes the mirror. */
export function mirrorNodeStatus(status: string | undefined): NodeStatus {
  switch (status) {
    case "ready":
      return "ready";
    case "running":
      return "running";
    case "review":
      return "review";
    case "merged":
      return "merged";
    case "failed":
      return "failed";
    case "blocked":
      return "blocked";
    default:
      return "planned";
  }
}

function mirrorBackend(backend: string | undefined): BackendId {
  return backend === "codex" || backend === "acpx" ? backend : "claude";
}

function mirrorEffort(effort: string | undefined): EffortLevel | undefined {
  switch (effort) {
    case "low":
    case "medium":
    case "high":
    case "xhigh":
    case "max":
      return effort;
    default:
      return undefined;
  }
}

/** Stable, mergeable edge id keyed only by endpoints + type (no timestamp/nonce)
 * so re-mirroring an unchanged plan reuses the same key and the prune is a no-op. */
function mirrorEdgeId(from: string, to: string, type: EdgeType): string {
  return `e_${from}__${type}__${to}`;
}

/**
 * Project a TUI plan payload into the graph, IN PLACE. Upserts every node in the
 * payload (mapping TUI status -> NodeStatus), rebuilds each node's incoming edges
 * from its deps, and PRUNES any graph node/edge no longer present in the payload
 * so the mirror exactly reflects the current TUI plan (not an append-only log).
 * Pure (no IO): callers wrap it in updateGraph. The prompt is taken from the
 * payload when present (so a re-plan that reuses a node id cannot leave a stale
 * prompt from the previous plan); source/createdAt are preserved on upsert;
 * everything else is overwritten from the payload.
 */
export function mirrorPlanIntoGraph(graph: RudderGraph, payload: MirrorPayload): RudderGraph {
  const inNodes = Array.isArray(payload.nodes) ? payload.nodes : [];
  const createdAt = nowIso();
  const keepNodeIds = new Set<string>();
  const keepEdgeIds = new Set<string>();

  for (const incoming of inNodes) {
    if (!incoming || typeof incoming.id !== "string" || !incoming.id) {
      continue;
    }
    keepNodeIds.add(incoming.id);
    const title = (incoming.title || incoming.id).slice(0, 200);
    const status = mirrorNodeStatus(incoming.status);

    const incomingEdgeIds: string[] = [];
    const deps = Array.isArray(incoming.deps) ? incoming.deps : [];
    for (const dep of deps) {
      if (!dep || typeof dep.on !== "string" || !dep.on) {
        continue;
      }
      const type: EdgeType = dep.type === "soft" ? "soft" : "hard";
      const edgeId = mirrorEdgeId(dep.on, incoming.id, type);
      keepEdgeIds.add(edgeId);
      graph.edges[edgeId] = { id: edgeId, from: dep.on, to: incoming.id, type };
      incomingEdgeIds.push(edgeId);
    }

    const existing = graph.nodes[incoming.id];
    const effort = mirrorEffort(incoming.effort);
    // Prompt comes from the payload when it carries one, so a re-plan that reuses
    // this id overwrites the previous plan's prompt instead of keeping it stale.
    const prompt =
      typeof incoming.prompt === "string" && incoming.prompt.trim()
        ? incoming.prompt
        : existing?.prompt ?? title;
    graph.nodes[incoming.id] = {
      id: incoming.id,
      title,
      prompt,
      backend: mirrorBackend(incoming.backend),
      ...(incoming.model ? { model: incoming.model } : {}),
      ...(effort ? { effort } : {}),
      status,
      ...(incoming.runId ? { runId: incoming.runId } : {}),
      ...(incoming.worktreePath ? { worktree: { path: incoming.worktreePath } } : {}),
      ...(incoming.jjChangeId ? { jjChangeId: incoming.jjChangeId } : {}),
      deps: incomingEdgeIds,
      source: existing?.source ?? "planner",
      createdAt: existing?.createdAt ?? createdAt,
      updatedAt: createdAt,
    };
  }

  // PRUNE: drop graph nodes/edges absent from the payload so the mirror is an
  // exact reflection of the live TUI plan.
  for (const id of Object.keys(graph.nodes)) {
    if (!keepNodeIds.has(id)) {
      delete graph.nodes[id];
    }
  }
  for (const id of Object.keys(graph.edges)) {
    if (!keepEdgeIds.has(id)) {
      delete graph.edges[id];
    }
  }
  return graph;
}

function sortGraph(graph: RudderGraph): RudderGraph {
  const nodes: Record<string, TaskNode> = {};
  for (const key of Object.keys(graph.nodes).sort()) {
    const node = graph.nodes[key];
    if (node) {
      nodes[key] = node;
    }
  }
  const edges: Record<string, GraphEdge> = {};
  for (const key of Object.keys(graph.edges).sort()) {
    const edge = graph.edges[key];
    if (edge) {
      edges[key] = edge;
    }
  }
  return {
    version: 1,
    repoRoot: graph.repoRoot,
    ...(graph.integrationChangeId ? { integrationChangeId: graph.integrationChangeId } : {}),
    nodes,
    edges,
    updatedAt: graph.updatedAt,
  };
}

// ---------------------------------------------------------------------------
// Pure queries. None of these mutate or read disk.
// ---------------------------------------------------------------------------

export function hardParents(graph: RudderGraph, id: string): string[] {
  const parents: string[] = [];
  for (const edge of Object.values(graph.edges)) {
    if (edge.to === id && edge.type === "hard") {
      parents.push(edge.from);
    }
  }
  return parents;
}

export function softParents(graph: RudderGraph, id: string): string[] {
  const parents: string[] = [];
  for (const edge of Object.values(graph.edges)) {
    if (edge.to === id && edge.type === "soft") {
      parents.push(edge.from);
    }
  }
  return parents;
}

/**
 * Judge parents of a node: the variants a judge node compares. A judge edge
 * gates readiness on the variant REACHING review (finishing its work), not on
 * merge. Fan-out-and-judge uses this so the judge launches once every variant
 * has produced a diff to compare.
 */
export function judgeParents(graph: RudderGraph, id: string): string[] {
  const parents: string[] = [];
  for (const edge of Object.values(graph.edges)) {
    if (edge.to === id && edge.type === "judge") {
      parents.push(edge.from);
    }
  }
  return parents;
}

/**
 * Direct dependents (children) of a node: nodes with an edge `from` this id.
 */
export function dependents(graph: RudderGraph, id: string): string[] {
  const out: string[] = [];
  for (const edge of Object.values(graph.edges)) {
    if (edge.from === id) {
      out.push(edge.to);
    }
  }
  return out;
}

/**
 * A node is ready when it is still `planned` AND every hard-dep parent has
 * merged AND every judge parent has reached review or merged. Soft parents
 * never block readiness. A judge node (which has judge parents) launches only
 * once all of its variants have finished their work (review or merged), so it
 * has every variant's diff to compare.
 */
export function isReady(graph: RudderGraph, node: TaskNode): boolean {
  if (node.status !== "planned") {
    return false;
  }
  // A hard dep id that is NOT part of the plan (pruned or never existed) is treated
  // as satisfied, matching the Rust scheduler's permissive rule: never deadlock on a
  // dangling reference. Otherwise the parent must be merged.
  const hardMet = hardParents(graph, node.id).every((parentId) => {
    const parent = graph.nodes[parentId];
    return parent === undefined || parent.status === "merged";
  });
  if (!hardMet) {
    return false;
  }
  return judgeParents(graph, node.id).every((parentId) => {
    const parent = graph.nodes[parentId];
    return parent === undefined || parent.status === "review" || parent.status === "merged";
  });
}

/**
 * Every node eligible to launch: planned with all hard parents merged.
 */
export function readyNodes(graph: RudderGraph): TaskNode[] {
  return Object.values(graph.nodes).filter((node) => isReady(graph, node));
}

/**
 * The frontier: non-merged leaves. A node is on the frontier when it is not
 * merged AND has no non-merged child (every dependent is already merged, or it
 * has no dependents). Injection reconciliation attaches new nodes here.
 */
export function frontier(graph: RudderGraph): TaskNode[] {
  return Object.values(graph.nodes).filter((node) => {
    if (node.status === "merged") {
      return false;
    }
    const children = dependents(graph, node.id);
    return children.every((childId) => graph.nodes[childId]?.status === "merged");
  });
}

/**
 * Project a worker-owned RunStatus into the DAG-level NodeStatus. With no run
 * yet, the node keeps its deps-derived status (planned/ready/blocked); the
 * scheduler decides those. Mirrors plan section 2's projection table.
 */
export function projectNodeStatus(node: TaskNode, run?: RunRecord): NodeStatus {
  // Daemon-owned terminal graph statuses are STICKY against re-projection. The
  // worker only ever writes "completed" to run.json (it never writes "merged" —
  // merge state lives in graph.json, owned by the daemon). So once the daemon
  // has merged a node, or blocked it on a merge conflict / handed it to a
  // resolver, the run.json stays "completed" forever. Without this guard the
  // next tick re-projects merged -> review, integration fires again on an
  // already-merged change, and the node thrashes review<->merged, never settling
  // as done. `blocked` is likewise daemon-owned (failed-parent propagation or a
  // held merge conflict) and must not be re-projected back to review.
  if (node.status === "merged" || node.status === "blocked") {
    return node.status;
  }
  if (!run) {
    return node.status === "ready" ? "ready" : "planned";
  }
  return nodeStatusFromRunStatus(run.status, node.status);
}

function nodeStatusFromRunStatus(runStatus: RunStatus, fallback: NodeStatus): NodeStatus {
  switch (runStatus) {
    case "created":
    case "running":
    case "steering":
    case "verifying":
    case "migrated":
      return "running";
    case "completed":
      return "review";
    case "failed":
    case "cancelled":
    case "orphaned":
      return "failed";
    case "merged":
      return "merged";
    case "merge-conflict":
      return "failed";
    default:
      return fallback;
  }
}
