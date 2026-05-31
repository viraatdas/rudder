import path from "node:path";
import type { GraphEdge, JsonValue, NodeStatus, RudderGraph, RunRecord, RunStatus, TaskNode } from "./types.js";
import { projectStateDir } from "./state.js";
import { nowIso, readJson, shortHash, updateJson } from "./util.js";

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
  let result: RudderGraph = emptyGraph(repoRoot);
  await updateJson<RudderGraph>(graphPath(repoRoot), (current) => {
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
  const hardMet = hardParents(graph, node.id).every(
    (parentId) => graph.nodes[parentId]?.status === "merged",
  );
  if (!hardMet) {
    return false;
  }
  return judgeParents(graph, node.id).every((parentId) => {
    const status = graph.nodes[parentId]?.status;
    return status === "review" || status === "merged";
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
  if (!run) {
    return node.status === "ready" || node.status === "blocked" ? node.status : "planned";
  }
  return nodeStatusFromRunStatus(run.status, node.status);
}

function nodeStatusFromRunStatus(runStatus: RunStatus, fallback: NodeStatus): NodeStatus {
  switch (runStatus) {
    case "created":
    case "running":
    case "steering":
    case "verifying":
      return "running";
    case "completed":
      return "review";
    case "failed":
    case "cancelled":
      return "failed";
    case "merged":
      return "merged";
    case "merge-conflict":
      return "failed";
    default:
      return fallback;
  }
}
