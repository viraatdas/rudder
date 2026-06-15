import type {
  BackendId,
  DepType,
  EdgeType,
  EffortLevel,
  InferredDep,
  PlanDag,
  PlanEdge,
  PlanNode,
  ReconcileResult,
} from "./types.js";
import { callTextModel } from "./task-summary.js";
import { loadInstructionFiles } from "./brain.js";
import { createEmptyChange, currentJjChangeId } from "./jj.js";
import { newEdgeId, newNodeId, updateGraph } from "./graph.js";
import { nowIso } from "./util.js";
import { DEFAULT_SUCCESS, deriveGoal, formatGoalPrompt, normalizeGoalLine } from "./goal.js";

export { loadInstructionFiles } from "./brain.js";

// A sonnet-class model: planning/decomposition is too hard for haiku, which is
// the task-summary model. Keep this a single constant so it is easy to bump.
export const PLANNER_MODEL = "claude-sonnet-4-6";

const PLAN_START = "RUDDER_PLAN_TASKS_START";
const PLAN_END = "RUDDER_PLAN_TASKS_END";

// The dependency rules quoted into the system prompts. The same wording is the
// contract the native tasks.rs parser enforces, so the model and both parsers
// agree on what hard vs soft means.
// The launch-goal convention quoted into the system prompts. Every spawnable task
// MUST carry a one-line objective (goal) and a verifiable stopping condition
// (success). Rudder launches workers with a leading `Objective:` line plus a `Done when:`
// line, so the worker always knows what done means without invoking a slash command.
const GOAL_RULES = [
  "Goal rules (REQUIRED for every task):",
  "- `goal`: one line naming the single objective the worker should accomplish.",
  "- `success`: the verifiable DONE-WHEN condition (the commands, artifacts, or criteria that mean it is complete).",
  "- Always include both. Never omit them and never leave them empty.",
].join("\n");

// Each worker runs in its OWN isolated jj workspace at a different path than the
// repository root you inspect. If a task prompt embeds an absolute path (e.g.
// "in the repository at /Users/.../repo, edit foo.py"), the worker will write to
// THAT path instead of its own workspace, the workspace stays empty, and the run
// fails verification with no changes. So prompts MUST be path-agnostic.
const WORKER_PATH_RULES = [
  "Worker workspace rules (REQUIRED):",
  "- Each task is run by a separate worker in its OWN isolated workspace checkout, at a different filesystem path than the repository root you are inspecting.",
  "- In every `prompt`, `goal`, and `success`, refer to files by REPOSITORY-RELATIVE paths only (e.g. `mathutils.py`, `src/db/schema.ts`). The worker is already cd'd into its workspace; relative paths resolve correctly there.",
  "- NEVER write an absolute filesystem path, the repository's location, or phrases like \"in the repository at <path>\" / \"cd into <path>\". An absolute path sends the worker to the wrong directory and the task fails.",
].join("\n");

const DEP_RULES = [
  "Dependency rules:",
  "- A task with no id and no deps is a 0-dep root.",
  "- hard = ordering: the child's success condition CANNOT be met until the parent has merged. Every hard edge MUST carry a non-empty `why` justifying the ordering; an unjustified hard edge is downgraded to soft.",
  "- A task that CONSUMES another task's produced code is HARD on it. Concretely: tests that import or exercise code another task writes; code that imports a module/function/type another task creates; integration or wiring that calls an API another task defines. The child can technically start, but it cannot SUCCEED (imports resolve, tests pass) until that code exists — so it must wait for the merge. Reading the parent's diff as soft context is NOT enough when the child must execute the parent's code.",
  "- soft = context-sharing (the default): the child runs in parallel and is fed the parent's diff once the parent merges. Use it when the child can succeed on its own and the parent's work is merely informative (parallel features, doc updates, sibling modules that do not import each other).",
  "- Prefer the MINIMAL hard-edge set. Default to soft for independent work, but do NOT under-classify: if the child executes or imports the parent's code, it is hard.",
  "- No cycles.",
].join("\n");

// ---------------------------------------------------------------------------
// Parser: TS port of native extract_rudder_plan_tasks (tasks.rs), extended for
// typed deps. Synthesizes ids, drops dangling edges, downgrades unjustified
// hard edges, and rejects cycles.
// ---------------------------------------------------------------------------

type RawTask = {
  id?: unknown;
  title?: unknown;
  prompt?: unknown;
  goal?: unknown;
  success?: unknown;
  deps?: unknown;
  backend?: unknown;
  model?: unknown;
  effort?: unknown;
  fileScope?: unknown;
};

export function parsePlanBlock(output: string): PlanDag {
  const json = extractPlanJson(output);
  let value: unknown;
  try {
    value = JSON.parse(json);
  } catch {
    throw new Error("RUDDER_PLAN_TASKS block was not valid JSON.");
  }
  const tasks = (value as { tasks?: unknown })?.tasks;
  if (!Array.isArray(tasks)) {
    throw new Error("RUDDER_PLAN_TASKS block must contain a tasks array.");
  }

  // First pass: build nodes, synthesizing ids n0.. for tasks without one.
  const nodes: PlanNode[] = [];
  const rawDepsById = new Map<string, PlanEdge[]>();
  const seenIds = new Set<string>();
  tasks.slice(0, 100).forEach((entry, index) => {
    const raw = (entry ?? {}) as RawTask;
    const prompt = typeof raw.prompt === "string" ? raw.prompt.trim() : "";
    if (!prompt) {
      return;
    }
    let id = typeof raw.id === "string" && raw.id.trim() ? raw.id.trim() : `n${index}`;
    // Uniquify colliding ids (mirrors the native parser): two tasks sharing an id
    // would otherwise collapse into one graph node + jj change, silently dropping a
    // task. rawDepsById is keyed by id, so a collision must be resolved before use.
    if (seenIds.has(id)) {
      let suffix = 2;
      while (seenIds.has(`${id}-${suffix}`)) {
        suffix += 1;
      }
      id = `${id}-${suffix}`;
    }
    seenIds.add(id);
    const title = typeof raw.title === "string" && raw.title.trim() ? raw.title.trim() : "worker task";
    // goal + success are part of the launch-goal convention: REQUIRED in the system
    // prompt, but we stay backward-compatible by deriving sane defaults when the
    // model omits them (goal = title/first line of the prompt; success = the
    // canonical stopping condition).
    const goal = normalizeGoalLine(
      typeof raw.goal === "string" && raw.goal.trim() ? raw.goal : deriveGoal(title || prompt),
      title || prompt,
    );
    const success = normalizeGoalLine(
      typeof raw.success === "string" && raw.success.trim() ? raw.success : DEFAULT_SUCCESS,
      DEFAULT_SUCCESS,
    );
    const node: PlanNode = {
      id,
      title,
      prompt,
      goal,
      success,
      deps: [],
      ...(isBackend(raw.backend) ? { backend: raw.backend } : {}),
      ...(typeof raw.model === "string" && raw.model.trim() ? { model: raw.model.trim() } : {}),
      ...(isEffort(raw.effort) ? { effort: raw.effort } : {}),
      ...(parseFileScope(raw.fileScope) ? { fileScope: parseFileScope(raw.fileScope) } : {}),
    };
    nodes.push(node);
    rawDepsById.set(id, parseRawDeps(raw.deps));
  });

  const knownIds = new Set(nodes.map((node) => node.id));

  // Second pass: resolve deps. Drop edges to unknown ids; downgrade hard edges
  // with an empty `why` to soft (forces justification).
  const edges: PlanDag["edges"] = [];
  for (const node of nodes) {
    const resolved: PlanEdge[] = [];
    for (const dep of rawDepsById.get(node.id) ?? []) {
      if (!knownIds.has(dep.on) || dep.on === node.id) {
        continue; // dangling or self edge
      }
      const why = dep.why?.trim();
      const type: DepType = dep.type === "hard" && why ? "hard" : "soft";
      resolved.push({ on: dep.on, type, ...(why ? { why } : {}) });
      edges.push({ from: dep.on, to: node.id, type, ...(why ? { why } : {}) });
    }
    node.deps = resolved;
  }

  if (hasCycle(nodes, edges)) {
    throw new Error("RUDDER_PLAN_TASKS dependency graph has a cycle.");
  }

  return { nodes, edges };
}

function extractPlanJson(output: string): string {
  const clean = output.replace(/\r/g, "");
  const start = clean.lastIndexOf(PLAN_START);
  if (start < 0) {
    throw new Error(`missing ${PLAN_START}`);
  }
  const afterStart = clean.slice(start + PLAN_START.length);
  const end = afterStart.indexOf(PLAN_END);
  if (end < 0) {
    throw new Error(`missing ${PLAN_END}`);
  }
  let json = afterStart.slice(0, end).trim();
  if (json.startsWith("```json")) {
    json = json.slice("```json".length).trim();
  } else if (json.startsWith("```")) {
    json = json.slice("```".length).trim();
  }
  if (json.endsWith("```")) {
    json = json.slice(0, -3).trim();
  }
  return json;
}

function parseRawDeps(value: unknown): PlanEdge[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const deps: PlanEdge[] = [];
  for (const entry of value) {
    if (typeof entry === "string" && entry.trim()) {
      // Bare id string -> soft edge by default.
      deps.push({ on: entry.trim(), type: "soft" });
      continue;
    }
    if (entry && typeof entry === "object") {
      const dep = entry as { on?: unknown; from?: unknown; type?: unknown; why?: unknown };
      // Models phrase the parent id as either "on" or "from"; accept both.
      const on =
        typeof dep.on === "string" && dep.on.trim()
          ? dep.on.trim()
          : typeof dep.from === "string"
            ? dep.from.trim()
            : "";
      if (!on) {
        continue;
      }
      const type: DepType = dep.type === "hard" ? "hard" : "soft";
      const why = typeof dep.why === "string" ? dep.why.trim() : "";
      deps.push({ on, type, ...(why ? { why } : {}) });
    }
  }
  return deps;
}

function parseFileScope(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const scope = value.filter((entry): entry is string => typeof entry === "string" && entry.trim().length > 0);
  return scope.length ? scope : undefined;
}

function isBackend(value: unknown): value is BackendId {
  return value === "claude" || value === "codex" || value === "acpx";
}

function isEffort(value: unknown): value is EffortLevel {
  return value === "low" || value === "medium" || value === "high" || value === "xhigh" || value === "max";
}

/**
 * Kahn topo-sort over hard edges only. Soft edges share context in parallel and
 * never deadlock, so a soft cycle is harmless; only hard cycles are rejected.
 */
function hasCycle(nodes: PlanNode[], edges: PlanDag["edges"]): boolean {
  const ids = nodes.map((node) => node.id);
  const indegree = new Map<string, number>(ids.map((id) => [id, 0]));
  const adjacency = new Map<string, string[]>(ids.map((id) => [id, []]));
  for (const edge of edges) {
    if (edge.type !== "hard") {
      continue;
    }
    if (!indegree.has(edge.to) || !adjacency.has(edge.from)) {
      continue;
    }
    indegree.set(edge.to, (indegree.get(edge.to) ?? 0) + 1);
    adjacency.get(edge.from)?.push(edge.to);
  }
  const queue = ids.filter((id) => (indegree.get(id) ?? 0) === 0);
  let visited = 0;
  while (queue.length) {
    const id = queue.shift() as string;
    visited += 1;
    for (const next of adjacency.get(id) ?? []) {
      const degree = (indegree.get(next) ?? 0) - 1;
      indegree.set(next, degree);
      if (degree === 0) {
        queue.push(next);
      }
    }
  }
  return visited !== ids.length;
}

// ---------------------------------------------------------------------------
// Adaptive gating. Trivial tasks bypass the gate entirely (today's fast path);
// anything richer gates for human approval before scaffolding.
// ---------------------------------------------------------------------------

const MULTI_STEP_MARKER = /\b(\d\.|first|then|after that|finally)\b/i;

export function gateDecision(dag: PlanDag): { autoRun: boolean; reason: string } {
  if (dag.nodes.length !== 1) {
    return { autoRun: false, reason: `plan has ${dag.nodes.length} nodes` };
  }
  if (dag.edges.length > 0) {
    return { autoRun: false, reason: "plan has dependencies" };
  }
  const node = dag.nodes[0];
  if (!node) {
    return { autoRun: false, reason: "plan has no nodes" };
  }
  const estTokens = Math.ceil(node.prompt.length / 4);
  if (estTokens >= 1500) {
    return { autoRun: false, reason: `prompt is large (~${estTokens} tokens)` };
  }
  if ((node.fileScope?.length ?? 0) > 3) {
    return { autoRun: false, reason: `file scope spans ${node.fileScope?.length} paths` };
  }
  if (MULTI_STEP_MARKER.test(node.prompt)) {
    return { autoRun: false, reason: "prompt has multi-step markers" };
  }
  return { autoRun: true, reason: "single trivial task" };
}

// ---------------------------------------------------------------------------
// Planner LLM calls. planTask decomposes; reconcile attaches an injected node
// to the existing frontier. Both reuse the task-summary LLM call path.
// ---------------------------------------------------------------------------

export type PlanContext = { root: string; branch: string };

function planSystemPrompt(): string {
  return [
    "You are Rudder's planning coordinator. Decompose a coding request into the smallest set of",
    "independent implementation tasks that separate worker agents can run in isolated jj workspaces.",
    "",
    DEP_RULES,
    "",
    GOAL_RULES,
    "",
    WORKER_PATH_RULES,
    "",
    "Output rules:",
    "- You run NON-INTERACTIVELY: never ask clarifying questions and never refuse. If the request is ambiguous, pick the most reasonable interpretation, state assumptions, and ALWAYS emit a complete plan.",
    "- Print exactly one block and no other JSON block:",
    `${PLAN_START}`,
    '{"tasks":[{"id":"n0","title":"short title","prompt":"full implementation prompt for one worker","goal":"one-line objective","success":"verifiable done-when condition","deps":[{"on":"n0","type":"hard","why":"why ordering is required"}],"backend":"claude","model":"...","effort":"medium","fileScope":["src/..."]}]}',
    `${PLAN_END}`,
    "- Use 1-4 tasks; more only when the split is clearly independent.",
    "- Keep the hard-edge set minimal and justify every hard edge in its `why`.",
    "- After the block, add a short human summary of why this split is safe.",
  ].join("\n");
}

export async function planTask(task: string, ctx: PlanContext): Promise<PlanDag> {
  const instructions = await loadInstructionFiles(ctx.root).catch(() => []);
  const instructionText = instructions.length
    ? `\n\nRepository instructions:\n${instructions.map((file) => `### ${file.path}\n${file.content}`).join("\n\n")}`
    : "";
  const user = [
    `Repository root (for your read-only inspection ONLY — do NOT put this path in any task prompt; workers run elsewhere): ${ctx.root}`,
    `Branch: ${ctx.branch}`,
    "",
    `User request:\n${task}`,
    instructionText,
  ].join("\n");

  const output = await callTextModel({
    model: PLANNER_MODEL,
    system: planSystemPrompt(),
    user,
    maxTokens: 4096,
  });
  return parsePlanBlock(output);
}

function reconcileSystemPrompt(): string {
  return [
    "You are Rudder's injection coordinator. A new task is being added to a project that already has",
    "in-flight work (the frontier). Decide how the new task depends on each frontier node.",
    "",
    DEP_RULES,
    "",
    GOAL_RULES,
    "",
    WORKER_PATH_RULES,
    "",
    "Output rules:",
    "- Print exactly one block and no other JSON block. The block holds a single task whose `deps`",
    "  reference the frontier node ids you were given:",
    `${PLAN_START}`,
    '{"tasks":[{"id":"new","title":"short title","prompt":"full implementation prompt","goal":"one-line objective","success":"verifiable done-when condition","deps":[{"on":"<frontier id>","type":"soft","why":"..."}]}]}',
    `${PLAN_END}`,
    "- Default every edge to soft. Only make an edge hard when the new task literally cannot start",
    "  until that frontier node merges, and justify it in `why`.",
  ].join("\n");
}

/**
 * Infer the new node's deps against the current frontier. On any LLM failure
 * (timeout / no model / unparsable), fall back to a SOFT edge to every frontier
 * node: this over-delivers context and never deadlocks. Never a default hard
 * edge.
 */
export async function reconcile(
  task: string,
  frontierNodes: PlanNode[],
  ctx: PlanContext,
): Promise<ReconcileResult> {
  const fallback = (): ReconcileResult => {
    const node: PlanNode = {
      id: "new",
      title: task.slice(0, 72),
      prompt: task,
      goal: deriveGoal(task),
      success: DEFAULT_SUCCESS,
      deps: frontierNodes.map((frontierNode) => ({ on: frontierNode.id, type: "soft" as DepType })),
    };
    return {
      node,
      inferredDeps: frontierNodes.map((frontierNode) => ({ node: frontierNode.id, type: "soft" as DepType })),
    };
  };

  if (frontierNodes.length === 0) {
    return {
      node: { id: "new", title: task.slice(0, 72), prompt: task, goal: deriveGoal(task), success: DEFAULT_SUCCESS, deps: [] },
      inferredDeps: [],
    };
  }

  const user = [
    `Repository root: ${ctx.root}`,
    `Branch: ${ctx.branch}`,
    "",
    "Frontier nodes (in-flight, not yet merged):",
    ...frontierNodes.map((node) => `- ${node.id}: ${node.title}`),
    "",
    `New task to reconcile:\n${task}`,
  ].join("\n");

  let output: string;
  try {
    output = await callTextModel({
      model: PLANNER_MODEL,
      system: reconcileSystemPrompt(),
      user,
      maxTokens: 1024,
      timeoutMs: 30000,
    });
  } catch {
    return fallback();
  }

  let dag: PlanDag;
  try {
    dag = parsePlanBlock(output);
  } catch {
    return fallback();
  }
  const node = dag.nodes[0];
  if (!node) {
    return fallback();
  }
  const frontierIds = new Set(frontierNodes.map((frontierNode) => frontierNode.id));
  // Injection reconciliation only ever produces hard/soft edges; the planner
  // parser never emits judge edges, so coerce any stray non-hard to soft.
  const inferredDeps: InferredDep[] = node.deps
    .filter((dep) => frontierIds.has(dep.on))
    .map((dep) => ({ node: dep.on, type: dep.type === "hard" ? "hard" : "soft" }));
  return { node, inferredDeps };
}

// ---------------------------------------------------------------------------
// Fan-out-and-judge: N independent variant agents attempt the same task, then a
// single judge agent compares their diffs and produces the winning
// implementation. Modeled as a DAG: N variant roots + 1 judge node with a
// "judge" edge from each variant. The judge becomes ready (isReady) when every
// variant has reached review; each variant's diff is delivered to the judge on
// review (treated like a soft edge, but gated on review not merge).
// ---------------------------------------------------------------------------

// Short angle hints cycled across variants so the attempts genuinely differ
// instead of being N identical runs of the same prompt.
const FANOUT_ANGLES = ["simplicity", "robustness", "minimal-diff", "speed", "clarity"];

export type FanoutOptions = {
  backend?: BackendId;
  model?: string;
  effort?: EffortLevel;
};

export function buildFanoutDag(task: string, n: number, opts: FanoutOptions = {}): PlanDag {
  const count = Math.max(2, Math.min(6, Math.floor(Number.isFinite(n) ? n : 3)));
  const trimmed = task.trim();
  const shortTitle = trimmed.slice(0, 56) || "task";

  const nodes: PlanNode[] = [];
  const variantIds: string[] = [];
  for (let index = 0; index < count; index += 1) {
    const angle = FANOUT_ANGLES[index % FANOUT_ANGLES.length] ?? "simplicity";
    const id = `v${index}`;
    variantIds.push(id);
    // Each variant: objective = the task with its angle; success = its approach
    // is implemented and its own verification passes. The objective-format header is
    // applied at scaffold time (scaffoldPlan), so the body stays raw here.
    nodes.push({
      id,
      title: `variant ${index + 1}/${count}: ${shortTitle}`,
      goal: `${trimmed} (favoring ${angle})`,
      success: "your approach is implemented and its own verification passes",
      prompt: [
        trimmed,
        "",
        `Approach variant ${index + 1} of ${count}: favor ${angle}. Implement the task end to end in your workspace; another agent will compare your result against the other variants.`,
      ].join("\n"),
      deps: [],
      ...(opts.backend ? { backend: opts.backend } : {}),
      ...(opts.model ? { model: opts.model } : {}),
      ...(opts.effort ? { effort: opts.effort } : {}),
    });
  }

  // Judge: objective = pick/synthesize the best; success = the final
  // implementation is complete and verified. The objective-format header is applied
  // at scaffold time (scaffoldPlan).
  const judgePrompt = [
    `${count} independent agents each attempted this task: ${trimmed}`,
    "Their diffs will be provided as context below.",
    "Compare them, choose the best approach (you may combine the strongest parts of several),",
    "and produce the final, complete implementation in your workspace.",
    "Note which variant you based it on.",
  ].join(" ");

  const judge: PlanNode = {
    id: "judge",
    title: `judge: ${shortTitle}`,
    goal: `pick or synthesize the best implementation for: ${trimmed}`,
    success: "the final implementation is complete and verified",
    prompt: judgePrompt,
    deps: variantIds.map((variantId) => ({ on: variantId, type: "judge" as EdgeType })),
    ...(opts.backend ? { backend: opts.backend } : {}),
    ...(opts.model ? { model: opts.model } : {}),
    ...(opts.effort ? { effort: opts.effort } : {}),
  };
  nodes.push(judge);

  const edges = variantIds.map((variantId) => ({
    from: variantId,
    to: "judge",
    type: "judge" as EdgeType,
  }));

  return { nodes, edges };
}

// ---------------------------------------------------------------------------
// Scaffold: turn a PlanDag into real graph.json nodes + edges, each backed by
// an empty jj change parented on its hard+soft deps' changes (plan section 3a).
// Dependents exist as real merge changes before parents do work.
// ---------------------------------------------------------------------------

export async function scaffoldPlan(repoRoot: string, dag: PlanDag): Promise<void> {
  const order = topoOrderHard(dag);
  const trunk = (await currentJjChangeId(repoRoot)) || "@";

  // Map plan id -> created jj change id, filled as we walk in topo order so a
  // child can parent on its already-created parents.
  const changeByPlanId = new Map<string, string>();
  // Map plan id -> the final graph node id (content-hashed, mergeable).
  const nodeIdByPlanId = new Map<string, string>();

  for (const node of order) {
    // hard+soft deps contribute jj-change parentage (the change stacks on them).
    // judge deps do NOT: a judge node produces its own implementation off the
    // trunk and the variants it compares are never merged.
    const parentChangeIds = node.deps
      .filter((dep) => dep.type !== "judge")
      .map((dep) => changeByPlanId.get(dep.on))
      .filter((value): value is string => Boolean(value));
    const parents = parentChangeIds.length ? parentChangeIds : [trunk];
    const nodeId = newNodeId(node.title);
    const changeId = await createEmptyChange({
      repoRoot,
      parents,
      description: `rudder-node:${nodeId} ${node.title}`,
    });
    changeByPlanId.set(node.id, changeId);
    nodeIdByPlanId.set(node.id, nodeId);
  }

  const createdAt = nowIso();
  await updateGraph(repoRoot, (graph) => {
    for (const node of dag.nodes) {
      const nodeId = nodeIdByPlanId.get(node.id);
      const changeId = changeByPlanId.get(node.id);
      if (!nodeId) {
        continue;
      }
      const incomingEdgeIds: string[] = [];
      for (const dep of node.deps) {
        const fromId = nodeIdByPlanId.get(dep.on);
        if (!fromId) {
          continue;
        }
        const edgeId = newEdgeId(fromId, nodeId);
        incomingEdgeIds.push(edgeId);
        graph.edges[edgeId] = {
          id: edgeId,
          from: fromId,
          to: nodeId,
          type: dep.type,
          ...(dep.why ? { why: dep.why } : {}),
        };
      }
      const goal = normalizeGoalLine(node.goal ?? deriveGoal(node.title || node.prompt), node.title || node.prompt);
      const success = normalizeGoalLine(node.success ?? DEFAULT_SUCCESS, DEFAULT_SUCCESS);
      graph.nodes[nodeId] = {
        id: nodeId,
        title: node.title,
        // Persist the launch prompt already in objective format so every planner-produced
        // worker leads with its objective + verifiable done-when line.
        prompt: formatGoalPrompt({ goal, success, body: node.prompt }),
        goal,
        success,
        backend: node.backend ?? "claude",
        ...(node.model ? { model: node.model } : {}),
        ...(node.effort ? { effort: node.effort } : {}),
        status: "planned",
        ...(changeId ? { jjChangeId: changeId } : {}),
        deps: incomingEdgeIds,
        source: "planner",
        createdAt,
        updatedAt: createdAt,
      };
    }
    return graph;
  });
}

/**
 * Topological order over HARD edges (the ones that gate scaffolding parentage).
 * parsePlanBlock already rejected hard cycles, so Kahn's algorithm completes;
 * any node left over (only possible via a soft-only cycle) is appended.
 */
function topoOrderHard(dag: PlanDag): PlanNode[] {
  const byId = new Map(dag.nodes.map((node) => [node.id, node]));
  const indegree = new Map<string, number>(dag.nodes.map((node) => [node.id, 0]));
  const adjacency = new Map<string, string[]>(dag.nodes.map((node) => [node.id, []]));
  for (const edge of dag.edges) {
    if (edge.type !== "hard" || !indegree.has(edge.to) || !adjacency.has(edge.from)) {
      continue;
    }
    indegree.set(edge.to, (indegree.get(edge.to) ?? 0) + 1);
    adjacency.get(edge.from)?.push(edge.to);
  }
  const queue = dag.nodes.filter((node) => (indegree.get(node.id) ?? 0) === 0).map((node) => node.id);
  const ordered: PlanNode[] = [];
  const seen = new Set<string>();
  while (queue.length) {
    const id = queue.shift() as string;
    if (seen.has(id)) {
      continue;
    }
    seen.add(id);
    const node = byId.get(id);
    if (node) {
      ordered.push(node);
    }
    for (const next of adjacency.get(id) ?? []) {
      const degree = (indegree.get(next) ?? 0) - 1;
      indegree.set(next, degree);
      if (degree === 0) {
        queue.push(next);
      }
    }
  }
  for (const node of dag.nodes) {
    if (!seen.has(node.id)) {
      ordered.push(node);
    }
  }
  return ordered;
}
