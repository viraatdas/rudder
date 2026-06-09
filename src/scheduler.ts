import type { RudderBus } from "./bus.js";
import {
  createEmptyChange,
  createNodeWorkspace,
  currentJjChangeId,
  currentOpId,
  exportToGit,
  jjDiff,
  listConflicts,
  mergeNode,
} from "./jj.js";
import {
  dependents,
  frontier as graphFrontier,
  hardParents,
  judgeParents,
  newEdgeId,
  newNodeId,
  projectNodeStatus,
  readGraph,
  readyNodes,
  updateGraph,
} from "./graph.js";
import { reconcile } from "./planner.js";
import { DEFAULT_SUCCESS, deriveGoal, formatGoalPrompt, normalizeGoalLine } from "./goal.js";
import { createRunRecord, loadConfig, loadRunRecord, runDir } from "./state.js";
import { spawnWorker } from "./run-manager.js";
import { ensureDecisionsFile, renderLiveRudderMd } from "./surfaces.js";
import type {
  GraphEdge,
  InferredDep,
  JsonValue,
  PlanNode,
  ResolverContext,
  RudderGraph,
  RunRecord,
  TaskNode,
} from "./types.js";
import { currentBranch } from "./git.js";
import path from "node:path";
import { newRunId, nowIso, writeJson } from "./util.js";

// ===========================================================================
// PURE decision functions. No IO, no spawning: these are the unit-tested core
// of the scheduler so tests never launch a worker.
// ===========================================================================

/**
 * The set of nodes to launch this tick: ready nodes (planned + all hard parents
 * merged) sorted oldest-first, sliced to the remaining parallel capacity.
 */
export function selectLaunchable(graph: RudderGraph, runningCount: number, maxParallel: number): TaskNode[] {
  const capacity = Math.max(0, maxParallel - runningCount);
  if (capacity === 0) {
    return [];
  }
  return readyNodes(graph)
    .slice()
    .sort((a, b) => a.createdAt.localeCompare(b.createdAt))
    .slice(0, capacity);
}

/**
 * When a node fails or is cancelled, the hard dependents (direct + transitive)
 * that can no longer ever start must become blocked. Soft dependents are
 * unaffected (they only ever received the parent's diff as context).
 */
export function computeBlocked(graph: RudderGraph, failedOrCancelledNodeId: string): string[] {
  const blocked = new Set<string>();
  const queue = [failedOrCancelledNodeId];
  while (queue.length) {
    const current = queue.shift() as string;
    for (const childId of dependents(graph, current)) {
      const child = graph.nodes[childId];
      if (!child) {
        continue;
      }
      // Only hard dependents are blocked; the edge from `current` must be hard.
      const isHardChild = hardParents(graph, childId).includes(current);
      if (!isHardChild) {
        continue;
      }
      if (child.status === "merged" || blocked.has(childId)) {
        continue;
      }
      blocked.add(childId);
      queue.push(childId);
    }
  }
  return [...blocked];
}

/**
 * Build the resolver-agent prompt. Pure: given the conflicted merge change, the
 * two merged titles, and the conflicted files, it instructs the worker to open
 * each file, resolve preserving BOTH intents, and remove all conflict markers.
 */
export function buildResolverPrompt(input: {
  mergeChangeId: string;
  intoTitle: string;
  nodeTitle: string;
  conflictedFiles: string[];
}): string {
  const files = input.conflictedFiles.length ? input.conflictedFiles.join(", ") : "(none reported)";
  return formatGoalPrompt({
    goal: `resolve the merge conflict between ${input.intoTitle} and ${input.nodeTitle}, preserving both intents`,
    success: "no conflict markers remain and `jj resolve --list` is empty",
    body: [
      `You are resolving a merge conflict at jj change ${input.mergeChangeId}, a merge of ${input.intoTitle} and ${input.nodeTitle}.`,
      `These files have conflict markers: ${files}.`,
      "Open each, resolve the conflict preserving BOTH intents, and remove all conflict markers.",
      "Do not touch unrelated files.",
      "When done, the merge must have no remaining conflicts.",
    ].join(" "),
  });
}

/**
 * PURE decision: should a completed resolver run finalize its original node?
 * Yes when the run is a resolver (has resolverFor) and no conflicts remain in
 * its workspace. The IO (listConflicts) is supplied by the caller so this stays
 * unit-testable without jj.
 */
export function resolverShouldFinalize(input: {
  resolverFor: string | undefined;
  remainingConflicts: string[];
}): boolean {
  return Boolean(input.resolverFor) && input.remainingConflicts.length === 0;
}

/**
 * Soft edges whose parent has merged but whose diff has not yet been delivered
 * to the child. The scheduler pipes each parent's diff into the child context.
 */
export function undeliveredSoftEdges(graph: RudderGraph): GraphEdge[] {
  return Object.values(graph.edges).filter(
    (edge) => edge.type === "soft" && !edge.delivered && graph.nodes[edge.from]?.status === "merged",
  );
}

/**
 * Judge edges (fan-out-and-judge) whose variant has REACHED review (or merged)
 * but whose diff has not yet been delivered to the judge node. Like
 * undeliveredSoftEdges but gated on review rather than merge, since the judge
 * compares the variants' finished work before any of them lands.
 */
export function undeliveredJudgeEdges(graph: RudderGraph): GraphEdge[] {
  return Object.values(graph.edges).filter((edge) => {
    if (edge.type !== "judge" || edge.delivered) {
      return false;
    }
    const status = graph.nodes[edge.from]?.status;
    return status === "review" || status === "merged";
  });
}

/**
 * Whether a node is a fan-out variant: the `from` of at least one judge edge.
 * Such a node is never merged on its own; the judge node it feeds produces the
 * winning implementation and merges that. Used to skip variants in auto-merge.
 */
export function isJudgedVariant(graph: RudderGraph, id: string): boolean {
  return Object.values(graph.edges).some((edge) => edge.type === "judge" && edge.from === id);
}

/**
 * Sum the node-attributed token usage across the whole graph (running +
 * terminal + merged). This is the number the budget cap compares against.
 */
export function sumNodeTokens(graph: RudderGraph): number {
  let total = 0;
  for (const node of Object.values(graph.nodes)) {
    if (node.tokens) {
      total += node.tokens.input + node.tokens.output;
    }
  }
  return total;
}

/**
 * Pure budget gate: true when a positive maxTokens cap is set and the spent
 * total has reached it. The scheduler HOLDS new launches when this is true.
 */
export function isOverBudget(tokensSpent: number, maxTokens: number | undefined): boolean {
  return typeof maxTokens === "number" && maxTokens > 0 && tokensSpent >= maxTokens;
}

/**
 * Whether `next` token usage is a larger total than `prev` (or prev is unset).
 * Backends report cumulative usage, so the largest value seen is the truth; we
 * only ever raise node.tokens, never lower it.
 */
export function tokensNewer(
  prev: { input: number; output: number } | undefined,
  next: { input: number; output: number },
): boolean {
  if (!prev) {
    return next.input + next.output > 0;
  }
  return next.input + next.output > prev.input + prev.output;
}

// ===========================================================================
// SIDE-EFFECTFUL orchestration. These mutate graph.json (always via updateGraph,
// the daemon-only writer), spawn detached workers, and call jj.
// ===========================================================================

/**
 * Launch a ready node: create its jj workspace off the scaffolded change (or the
 * integration trunk), create the run record, flip status to "running" INSIDE the
 * updateGraph transaction (the double-launch guard), then spawn the detached
 * worker and publish schedule.launched.
 */
export async function launchNode(
  repoRoot: string,
  graph: RudderGraph,
  node: TaskNode,
  bus: RudderBus,
): Promise<void> {
  // Determine the base change: the node's scaffolded empty change if present,
  // else the integration trunk, else `@`.
  const baseChange = node.jjChangeId || graph.integrationChangeId || (await currentJjChangeId(repoRoot)) || "@";

  const workspace = await createNodeWorkspace({
    repoRoot,
    nodeId: node.id,
    atChangeId: baseChange,
    task: node.title,
  });
  // Seed the shared-knowledge surface into the workspace if absent so the agent
  // edits it in its jj workspace (jj merges concurrent edits on fan-in).
  await ensureDecisionsFile(workspace.path).catch(() => undefined);
  const jjChangeId = (await currentJjChangeId(workspace.path)) || node.jjChangeId;

  const run = await createRunRecord({
    repoRoot,
    task: node.prompt,
    backend: node.backend,
    model: node.model,
    effort: node.effort,
    targetBranch: baseChange,
    baseCommit: baseChange,
    vcs: "jj",
    useWorktree: true,
    worktreeWorkspaceName: workspace.workspaceName,
    worktreeJjChangeId: jjChangeId,
    worktreePath: workspace.path,
  });

  // Flip status -> running in the same transaction so a concurrent tick cannot
  // re-select this node and double-launch it.
  await updateGraph(repoRoot, (g) => {
    const current = g.nodes[node.id];
    if (!current) {
      return g;
    }
    current.runId = run.id;
    current.status = "running";
    current.worktree = { path: workspace.path, workspaceName: workspace.workspaceName };
    if (jjChangeId) {
      current.jjChangeId = jjChangeId;
    }
    current.updatedAt = nowIso();
    return g;
  });

  spawnWorker(repoRoot, run.id);

  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "schedule.launched",
    message: `Launched ${node.id} (${node.title})`,
    data: { workspace: workspace.path },
  });
  bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "node.running" });
}

/**
 * Merge a node's change into the integration trunk. Captures the op id first.
 * Clean -> node "merged" + advance graph.integrationChangeId + exportToGit +
 * merge.merged. Conflict -> node "blocked" + record operationId/conflictedFiles
 * + merge.conflict. The resolver-agent spawn is Phase 5; for now hold it blocked.
 */
export async function mergeNodeIntoIntegration(repoRoot: string, node: TaskNode, bus: RudderBus): Promise<void> {
  const nodeChangeId = node.jjChangeId || (node.worktree?.path ? await currentJjChangeId(node.worktree.path) : "");
  if (!nodeChangeId) {
    bus.publish({
      ts: nowIso(),
      runId: node.runId ?? node.id,
      nodeId: node.id,
      type: "merge.conflict",
      message: `Could not determine jj change id for ${node.id}.`,
    });
    return;
  }

  const graph = await readGraph(repoRoot);
  const intoChange = graph.integrationChangeId || (await currentJjChangeId(repoRoot)) || "@";
  const opIdBefore = await currentOpId(repoRoot);

  bus.publish({
    ts: nowIso(),
    runId: node.runId ?? node.id,
    nodeId: node.id,
    type: "merge.attempt",
    message: `Merging ${node.id} into integration`,
    ...(opIdBefore ? { data: { operationId: opIdBefore } } : {}),
  });

  const result = await mergeNode({
    repoRoot,
    nodeChangeId,
    intoChangeId: intoChange,
    message: `rudder: ${node.title.slice(0, 72)}`,
  });

  if (result.conflictedFiles.length === 0 && result.mergeChangeId) {
    // If this is a judge node (fan-out-and-judge), its variant parents lose:
    // the judge produced the winning implementation and merged it. The variants
    // are never merged; they end in "review" flagged supersededBy the judge so
    // the board can render them as superseded.
    const supersededVariants = judgeParents(graph, node.id);
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "merged";
        current.reviewState = "approved";
        current.updatedAt = nowIso();
      }
      for (const variantId of supersededVariants) {
        const variant = g.nodes[variantId];
        if (variant && variant.status !== "merged") {
          variant.supersededBy = node.id;
          variant.updatedAt = nowIso();
        }
      }
      g.integrationChangeId = result.mergeChangeId;
      return g;
    });
    await exportToGit(repoRoot);
    bus.publish({
      ts: nowIso(),
      runId: node.runId ?? node.id,
      nodeId: node.id,
      type: "merge.merged",
      message: `Merged ${node.id}`,
      data: { mergeChangeId: result.mergeChangeId, operationId: result.opId },
    });
    bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.merged" });
    return;
  }

  // Conflict-as-data: the merge change exists and records the conflict. Hold the
  // node blocked and spawn a resolver-agent node whose working copy IS the
  // conflicted merge change, unless we have already auto-retried once (cap at 1).
  bus.publish({
    ts: nowIso(),
    runId: node.runId ?? node.id,
    nodeId: node.id,
    type: "merge.conflict",
    message: `Merge conflict for ${node.id}`,
    data: { mergeChangeId: result.mergeChangeId, operationId: result.opId, conflictedFiles: result.conflictedFiles },
  });
  bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.blocked" });

  if (!result.mergeChangeId) {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    return;
  }

  await spawnResolver(repoRoot, {
    node,
    mergeChangeId: result.mergeChangeId,
    intoChangeId: intoChange,
    intoTitle: intoTitleFor(graph),
    conflictedFiles: result.conflictedFiles,
    bus,
  });
}

function intoTitleFor(graph: RudderGraph): string {
  // The integration trunk has no single owning node; name it for the surface.
  return graph.integrationChangeId ? "integration trunk" : "the base change";
}

/**
 * Spawn a resolver-agent node at the conflicted merge change. The resolver's
 * workspace is created at the merge change so `jj edit` materializes the
 * conflict markers; its run record carries resolverFor = the blocked node id.
 * The original node keeps status "blocked" with resolverRunId set. The resolver
 * auto-retry is capped: a node that already has a resolverRunId is not respun.
 */
async function spawnResolver(
  repoRoot: string,
  input: {
    node: TaskNode;
    mergeChangeId: string;
    intoChangeId: string;
    intoTitle: string;
    conflictedFiles: string[];
    bus: RudderBus;
  },
): Promise<void> {
  const { node, bus } = input;

  // Cap auto-retry at 1: if this node already spawned a resolver, do not loop.
  if (node.resolverRunId) {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    return;
  }

  const resolverRunId = newRunId(`resolve ${node.title}`);

  let workspace: { workspaceName: string; path: string };
  try {
    workspace = await createNodeWorkspace({
      repoRoot,
      runId: resolverRunId,
      atChangeId: input.mergeChangeId,
      task: `resolve ${node.title}`,
    });
  } catch {
    // jj could not materialize the merge change; hold blocked without a resolver.
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    return;
  }

  const prompt = buildResolverPrompt({
    mergeChangeId: input.mergeChangeId,
    intoTitle: input.intoTitle,
    nodeTitle: node.title,
    conflictedFiles: input.conflictedFiles,
  });

  const run = await createRunRecord({
    id: resolverRunId,
    repoRoot,
    task: prompt,
    backend: node.backend,
    model: node.model,
    effort: node.effort,
    targetBranch: input.mergeChangeId,
    baseCommit: input.mergeChangeId,
    vcs: "jj",
    resolverFor: node.id,
    useWorktree: true,
    worktreeWorkspaceName: workspace.workspaceName,
    worktreeJjChangeId: input.mergeChangeId,
    worktreePath: workspace.path,
  });

  // Persist the resolver context for the worker (and any UI). Best-effort.
  const context: ResolverContext = {
    mergeChangeId: input.mergeChangeId,
    parentChangeIds: [input.intoChangeId, node.jjChangeId ?? ""].filter(Boolean),
    conflictedFiles: input.conflictedFiles,
    nodeTitle: node.title,
    intoTitle: input.intoTitle,
    workspacePath: workspace.path,
  };
  await writeJson(
    path.join(runDir(repoRoot, resolverRunId), "resolver.json"),
    context as unknown as JsonValue,
  ).catch(() => undefined);

  // Spawn the resolver worker FIRST. With detached + ignored stdio a failed spawn
  // returns undefined; if we back-pointed the node at resolverRunId before knowing
  // that, the node would stay blocked forever (the auto-retry cap skips any node that
  // already has a resolverRunId). So only persist the back-pointer once the worker is
  // actually live; on a failed spawn, leave the node blocked WITHOUT a resolverRunId so
  // a later merge attempt can retry, and surface the failure.
  const pid = spawnWorker(repoRoot, run.id);
  if (pid === undefined) {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.merge = {
          ...(current.merge ?? { status: "conflict" }),
          status: "conflict",
          mergeChangeId: input.mergeChangeId,
          conflictedFiles: input.conflictedFiles,
        };
        current.updatedAt = nowIso();
      }
      return g;
    });
    throw new Error(`failed to spawn conflict resolver ${run.id} for node ${node.id}`);
  }

  // Resolver is live: hold the original node blocked and back-point at its resolver.
  await updateGraph(repoRoot, (g) => {
    const current = g.nodes[node.id];
    if (current) {
      current.status = "blocked";
      current.resolverRunId = resolverRunId;
      current.merge = {
        ...(current.merge ?? { status: "conflict" }),
        status: "conflict",
        mergeChangeId: input.mergeChangeId,
        conflictedFiles: input.conflictedFiles,
      };
      current.updatedAt = nowIso();
    }
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "resolver.spawned",
    message: `Spawned resolver ${run.id} for ${node.id}`,
    data: { resolverRunId: run.id, mergeChangeId: input.mergeChangeId, conflictedFiles: input.conflictedFiles },
  });
}

/**
 * Deliver a parent's diff to its child as additional context, then mark the
 * edge delivered. Used for both soft edges (parent merged) and judge edges (the
 * variant reached review). Minimal acceptable form: record the parent diff on
 * the child node (the worker re-reads it). Never blocks. Publishes
 * schedule.softDelivered.
 */
export async function deliverSoftDiff(repoRoot: string, edge: GraphEdge, bus: RudderBus): Promise<void> {
  const graph = await readGraph(repoRoot);
  const parent = graph.nodes[edge.from];
  const child = graph.nodes[edge.to];
  if (!parent || !child) {
    return;
  }
  const parentWorkspace = parent.worktree?.path ?? repoRoot;
  const diff = await jjDiff(parentWorkspace).catch(() => "");
  const isJudge = edge.type === "judge";

  await updateGraph(repoRoot, (g) => {
    const target = g.edges[edge.id];
    if (target) {
      target.delivered = true;
    }
    const childNode = g.nodes[edge.to];
    if (childNode && diff.trim()) {
      // Stash the delivered context on the child's prompt so the worker reads it
      // when the run is created/continued. Append, do not replace, and never
      // block on it. Judge nodes receive each variant's diff to compare.
      const header = isJudge
        ? `\n\n--- Variant ${parent.id} (${parent.title}) diff to evaluate ---\n`
        : `\n\n--- Context from merged sibling ${parent.id} (${parent.title}) ---\n`;
      if (!childNode.prompt.includes(header)) {
        // Real repo diffs commonly exceed 8k chars; a too-small slice silently starves a
        // judge of comparison context (each variant is truncated independently). 16k keeps
        // it bounded while carrying typical diffs whole.
        childNode.prompt = `${childNode.prompt}${header}${diff.slice(0, 16000)}`;
      }
      childNode.updatedAt = nowIso();
    }
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: child.runId ?? child.id,
    nodeId: child.id,
    type: "schedule.softDelivered",
    message: isJudge
      ? `Delivered variant ${parent.id} diff to judge ${child.id}`
      : `Delivered ${parent.id} diff to ${child.id}`,
    data: { from: edge.from, to: edge.to, kind: edge.type },
  });
}

// Per-repo serialization for every scheduler operation that reads then advances
// the integration trunk. The daemon fires scheduleTick (1s interval) AND
// onRunTransition (per-run fs.watch) AND the board's manual merge endpoints, all
// in ONE process as un-awaited `void` calls. Without this lock, two nodes that
// reach review at nearly the same instant get merged concurrently: both read the
// SAME integrationChangeId, produce two SIBLING merges off it, and the second
// updateGraph clobbers integrationChangeId — so one node is marked "merged" while
// its work is orphaned off the trunk (silent data loss). Chaining every op per
// repo onto a single promise makes merges read the integration head the previous
// op just advanced. In-process is sufficient: the daemon is the sole scheduler.
const schedulerChains = new Map<string, Promise<unknown>>();

export function withSchedulerLock<T>(repoRoot: string, fn: () => Promise<T>): Promise<T> {
  const prev = schedulerChains.get(repoRoot) ?? Promise.resolve();
  // Run fn after the previous op settles (success OR failure), so one rejected op
  // never wedges the chain. The caller still sees fn's own result/rejection.
  const result = prev.then(fn, fn);
  schedulerChains.set(
    repoRoot,
    result.catch(() => undefined),
  );
  return result;
}

/**
 * One scheduler tick. Recompute each scheduled node's status from its run.json,
 * read orchestrator config, hold if at capacity or over budget, else launch the
 * selectable ready nodes and deliver undelivered soft diffs. A node that just
 * reached completed (review) auto-merges when reviewGate==="auto".
 *
 * Public entry point: serialized against all other scheduler ops for this repo.
 * Internal callers that already hold the lock use `scheduleTickCore` directly.
 */
export function scheduleTick(repoRoot: string, bus: RudderBus): Promise<void> {
  return withSchedulerLock(repoRoot, () => scheduleTickCore(repoRoot, bus));
}

async function scheduleTickCore(repoRoot: string, bus: RudderBus): Promise<void> {
  const config = await loadConfig();
  const orchestrator = config.orchestrator ?? { maxParallel: 1000, reviewGate: "manual" as const };
  const maxParallel = orchestrator.maxParallel ?? 1000;
  const budgetTokens = orchestrator.budget?.maxTokens;

  // Recompute statuses from run.json and surface review/auto-merge transitions.
  const graph = await readGraph(repoRoot);
  let runningCount = 0;
  let tokensSpent = 0;
  const justReached: Array<{ node: TaskNode; status: TaskNode["status"] }> = [];
  // Token usage the backend reported on run.json that is newer than what the
  // node currently carries; flushed into the graph so the budget sum is real.
  const tokenUpdates = new Map<string, { input: number; output: number }>();

  for (const node of Object.values(graph.nodes)) {
    let status = node.status;
    let nodeTokens = node.tokens;
    // A node held blocked with a resolver in flight is owned by that resolver,
    // not by its original (already-completed) run. Do not re-project it back to
    // review from the original run.json while the resolver works.
    const resolverInFlight = node.status === "blocked" && Boolean(node.resolverRunId);
    if (node.runId && !resolverInFlight) {
      const run = await loadRunRecord(repoRoot, node.runId).catch(() => null);
      status = projectNodeStatus(node, run ?? undefined);
      // Budget accounting: copy the backend-reported run.tokens onto the node so
      // scheduleTick's budget sum runs on real numbers (the worker captures them
      // from claude/codex stream output). Only update when newer/larger.
      if (run?.tokens && tokensNewer(node.tokens, run.tokens)) {
        nodeTokens = { input: run.tokens.input, output: run.tokens.output };
        tokenUpdates.set(node.id, nodeTokens);
      }
    }
    // Budget is a soft cap on node-attributed tokens summed across every node
    // that has reported usage (running + terminal + merged alike).
    if (nodeTokens) {
      tokensSpent += nodeTokens.input + nodeTokens.output;
    }
    if (status !== node.status) {
      justReached.push({ node, status });
    }
    if (status === "running") {
      runningCount += 1;
    }
  }

  // Persist any projected status changes (and capture review transitions) plus
  // any freshened token counts.
  if (justReached.length || tokenUpdates.size) {
    await updateGraph(repoRoot, (g) => {
      for (const { node, status } of justReached) {
        const current = g.nodes[node.id];
        if (current && current.status !== status) {
          current.status = status;
          if (status === "review") {
            current.reviewState = current.reviewState ?? "pending";
          }
          current.updatedAt = nowIso();
        }
      }
      for (const [nodeId, tokens] of tokenUpdates) {
        const current = g.nodes[nodeId];
        if (current) {
          current.tokens = tokens;
          current.updatedAt = nowIso();
        }
      }
      return g;
    });
    for (const { node, status } of justReached) {
      if (status === "review") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.review" });
      } else if (status === "failed") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.failed" });
      } else if (status === "merged") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.merged" });
      }
    }
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }

  // Auto-merge review nodes when the gate is auto. Skip fan-out variants (nodes
  // that feed a judge node via a judge edge): only the judge node merges; the
  // variants end in review, superseded by the judge's choice.
  if (orchestrator.reviewGate === "auto") {
    const fresh = await readGraph(repoRoot);
    for (const node of Object.values(fresh.nodes)) {
      if (node.status === "review" && !node.supersededBy && !isJudgedVariant(fresh, node.id)) {
        await mergeNodeIntoIntegration(repoRoot, node, bus);
      }
    }
  }

  const overBudget = isOverBudget(tokensSpent, budgetTokens);
  if (runningCount >= maxParallel || overBudget) {
    bus.publish({
      ts: nowIso(),
      runId: "scheduler",
      type: "schedule.tick",
      message: overBudget
        ? `holding: budget exceeded (${tokensSpent}/${budgetTokens} tokens)`
        : "holding: at parallel capacity",
      data: { runningCount, maxParallel, tokensSpent, ...(budgetTokens ? { maxTokens: budgetTokens } : {}), overBudget },
    });
    return;
  }

  // Deliver pending judge diffs BEFORE launching so a judge node that is about
  // to become ready has every variant diff already stitched into its prompt.
  const beforeLaunch = await readGraph(repoRoot);
  for (const edge of undeliveredJudgeEdges(beforeLaunch)) {
    await deliverSoftDiff(repoRoot, edge, bus);
  }

  const latest = await readGraph(repoRoot);
  const launchable = selectLaunchable(latest, runningCount, maxParallel);
  for (const node of launchable) {
    await launchNode(repoRoot, latest, node, bus);
  }

  const afterLaunch = launchable.length ? await readGraph(repoRoot) : latest;
  for (const edge of undeliveredSoftEdges(afterLaunch)) {
    await deliverSoftDiff(repoRoot, edge, bus);
  }

  bus.publish({
    ts: nowIso(),
    runId: "scheduler",
    type: "schedule.tick",
    message: `tick: launched ${launchable.length}`,
    data: { runningCount, maxParallel, launched: launchable.length },
  });

  if (launchable.length) {
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }
}

/**
 * Called by the daemon when a run.json changes. Projects the owning node's
 * status: completed -> review (auto-merge if configured); failed/cancelled ->
 * node.failed + propagate blocked to hard dependents. Then ticks the scheduler.
 */
export function onRunTransition(repoRoot: string, runId: string, bus: RudderBus): Promise<void> {
  return withSchedulerLock(repoRoot, () => onRunTransitionCore(repoRoot, runId, bus));
}

async function onRunTransitionCore(repoRoot: string, runId: string, bus: RudderBus): Promise<void> {
  const run = await loadRunRecord(repoRoot, runId).catch(() => null);

  // A resolver run finishing is handled specially: it owns no graph node of its
  // own; instead it points (resolverFor) at the originally-blocked node.
  if (run?.resolverFor && (run.status === "completed" || run.status === "merged")) {
    await onResolverTransition(repoRoot, run.resolverFor, run, bus);
    return;
  }

  const graph = await readGraph(repoRoot);
  const node = Object.values(graph.nodes).find((candidate) => candidate.runId === runId);
  if (!node) {
    // Not a graph-managed run (e.g. an ad-hoc TUI run); just tick.
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  // If this node is held blocked with a resolver in flight, the resolver (not
  // the original completed run) drives it. Ignore the original run's transition.
  if (node.status === "blocked" && node.resolverRunId) {
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  const projected = projectNodeStatus(node, run ?? undefined);

  if (projected === "review" && node.status !== "review") {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "review";
        current.reviewState = current.reviewState ?? "pending";
        current.updatedAt = nowIso();
      }
      return g;
    });
    bus.publish({ ts: nowIso(), runId, nodeId: node.id, type: "node.review" });
    await renderLiveRudderMd(repoRoot).catch(() => undefined);

    const config = await loadConfig();
    // A fan-out variant never auto-merges: it only feeds the judge node, which
    // is what eventually merges. Let scheduleTick deliver the variant diff and
    // launch the judge once all variants have reached review.
    if (config.orchestrator?.reviewGate === "auto" && !isJudgedVariant(graph, node.id)) {
      const fresh = await readGraph(repoRoot);
      const refreshed = fresh.nodes[node.id];
      if (refreshed) {
        await mergeNodeIntoIntegration(repoRoot, refreshed, bus);
      }
    }
  } else if (projected === "failed" && node.status !== "failed") {
    const blocked = computeBlocked(graph, node.id);
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "failed";
        current.updatedAt = nowIso();
      }
      for (const id of blocked) {
        const target = g.nodes[id];
        if (target && target.status !== "merged") {
          target.status = "blocked";
          target.updatedAt = nowIso();
        }
      }
      return g;
    });
    bus.publish({ ts: nowIso(), runId, nodeId: node.id, type: "node.failed" });
    for (const id of blocked) {
      bus.publish({ ts: nowIso(), runId, nodeId: id, type: "node.blocked" });
    }
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }

  await scheduleTickCore(repoRoot, bus);
}

/**
 * A resolver run finished. Re-check its workspace for conflicts. If none remain,
 * finalize the original node: mark it merged, advance integrationChangeId to the
 * (now-resolved) merge change, exportToGit, publish merge.merged + node.merged,
 * and tick (to unblock dependents). If conflicts remain, hold the node blocked
 * and re-publish merge.conflict. Auto-retry is capped at 1 (the original node
 * already has resolverRunId set, so mergeNodeIntoIntegration would not respin).
 */
async function onResolverTransition(
  repoRoot: string,
  originalNodeId: string,
  resolverRun: RunRecord,
  bus: RudderBus,
): Promise<void> {
  const workspacePath = resolverRun.worktree?.path || repoRoot;
  const remaining = await listConflicts(workspacePath).catch(() => [] as string[]);
  const mergeChangeId =
    resolverRun.worktree?.jjChangeId || (await currentJjChangeId(workspacePath).catch(() => "")) || "";

  const graph = await readGraph(repoRoot);
  const node = graph.nodes[originalNodeId];

  if (!resolverShouldFinalize({ resolverFor: resolverRun.resolverFor, remainingConflicts: remaining })) {
    // Conflicts remain: keep the node blocked. We do not respin (cap at 1).
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[originalNodeId];
      if (current && current.status !== "merged") {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    bus.publish({
      ts: nowIso(),
      runId: resolverRun.id,
      nodeId: originalNodeId,
      type: "merge.conflict",
      message: `Resolver ${resolverRun.id} left ${remaining.length} conflict(s) for ${originalNodeId}`,
      data: { conflictedFiles: remaining, mergeChangeId },
    });
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  // Clean: finalize the original node onto the resolved merge change.
  await updateGraph(repoRoot, (g) => {
    const current = g.nodes[originalNodeId];
    if (current) {
      current.status = "merged";
      current.reviewState = "approved";
      if (current.merge) {
        current.merge = { ...current.merge, status: "merged", conflictedFiles: [] };
      }
      current.updatedAt = nowIso();
    }
    if (mergeChangeId) {
      g.integrationChangeId = mergeChangeId;
    }
    return g;
  });
  await exportToGit(repoRoot).catch(() => undefined);

  bus.publish({
    ts: nowIso(),
    runId: resolverRun.id,
    nodeId: originalNodeId,
    type: "resolver.resolved",
    message: `Resolver ${resolverRun.id} resolved ${originalNodeId}`,
    data: { mergeChangeId },
  });
  bus.publish({
    ts: nowIso(),
    runId: node?.runId ?? originalNodeId,
    nodeId: originalNodeId,
    type: "merge.merged",
    message: `Merged ${originalNodeId} (conflict resolved)`,
    data: { mergeChangeId },
  });
  bus.publish({ ts: nowIso(), runId: node?.runId ?? originalNodeId, nodeId: originalNodeId, type: "node.merged" });

  await renderLiveRudderMd(repoRoot).catch(() => undefined);
  await scheduleTickCore(repoRoot, bus);
}

/**
 * The single injection chokepoint. A typed task (terminal pane or board
 * composer) becomes a NEW node reconciled against the frontier, never blindly
 * appended. Adds the node (status "planned", source "injection") + inferred
 * edges, scaffolds its empty jj change, publishes plan.reconciled, then ticks.
 */
export function reconcileInjection(
  repoRoot: string,
  input: { prompt: string; title?: string; backend?: TaskNode["backend"]; model?: string; effort?: TaskNode["effort"] },
  bus: RudderBus,
): Promise<{ nodeId: string }> {
  return withSchedulerLock(repoRoot, () => reconcileInjectionCore(repoRoot, input, bus));
}

async function reconcileInjectionCore(
  repoRoot: string,
  input: { prompt: string; title?: string; backend?: TaskNode["backend"]; model?: string; effort?: TaskNode["effort"] },
  bus: RudderBus,
): Promise<{ nodeId: string }> {
  const graph = await readGraph(repoRoot);
  const frontierNodes: PlanNode[] = graphFrontier(graph).map((node) => ({
    id: node.id,
    title: node.title,
    prompt: node.prompt,
    deps: [],
  }));

  const branch = await currentBranch(repoRoot).catch(() => "main");
  const result = await reconcile(input.prompt, frontierNodes, { root: repoRoot, branch }).catch(() => ({
    node: {
      id: "new",
      title: input.prompt.slice(0, 72),
      prompt: input.prompt,
      goal: deriveGoal(input.prompt),
      success: DEFAULT_SUCCESS,
      deps: [],
    } as PlanNode,
    inferredDeps: [] as InferredDep[],
  }));

  const title = input.title?.trim() || result.node.title || input.prompt.slice(0, 72);
  const nodeId = newNodeId(title);

  // Scaffold an empty jj change parented on the inferred deps' changes (or the
  // integration trunk). Soft-edge fallback never blocks.
  const parentChangeIds = result.inferredDeps
    .map((dep) => graph.nodes[dep.node]?.jjChangeId)
    .filter((value): value is string => Boolean(value));
  const trunk = graph.integrationChangeId || (await currentJjChangeId(repoRoot)) || "@";
  const parents = parentChangeIds.length ? parentChangeIds : [trunk];

  let changeId = "";
  try {
    changeId = await createEmptyChange({
      repoRoot,
      parents,
      description: `rudder-node:${nodeId} ${title}`,
    });
  } catch {
    // Degrade gracefully: a node with no scaffolded change still launches off
    // the integration trunk in launchNode.
    changeId = "";
  }

  const createdAt = nowIso();
  await updateGraph(repoRoot, (g) => {
    const incomingEdgeIds: string[] = [];
    for (const dep of result.inferredDeps) {
      if (!g.nodes[dep.node]) {
        continue;
      }
      const edgeId = newEdgeId(dep.node, nodeId);
      incomingEdgeIds.push(edgeId);
      g.edges[edgeId] = {
        id: edgeId,
        from: dep.node,
        to: nodeId,
        type: dep.type,
      };
    }
    const goal = normalizeGoalLine(result.node.goal ?? deriveGoal(title || input.prompt), title || input.prompt);
    const success = normalizeGoalLine(result.node.success ?? DEFAULT_SUCCESS, DEFAULT_SUCCESS);
    g.nodes[nodeId] = {
      id: nodeId,
      title,
      // Reconciled (injected) nodes lead with the objective-format header too.
      prompt: formatGoalPrompt({ goal, success, body: result.node.prompt || input.prompt }),
      goal,
      success,
      backend: input.backend ?? "claude",
      ...(input.model ? { model: input.model } : {}),
      ...(input.effort ? { effort: input.effort } : {}),
      status: "planned",
      ...(changeId ? { jjChangeId: changeId } : {}),
      deps: incomingEdgeIds,
      source: "injection",
      createdAt,
      updatedAt: createdAt,
    };
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: nodeId,
    nodeId,
    type: "plan.reconciled",
    message: `Reconciled injection ${nodeId} against ${frontierNodes.length} frontier node(s)`,
    data: { inferredDeps: result.inferredDeps },
  });
  bus.publish({ ts: nowIso(), runId: nodeId, nodeId, type: "node.created" });

  await renderLiveRudderMd(repoRoot).catch(() => undefined);
  await scheduleTickCore(repoRoot, bus);
  return { nodeId };
}
