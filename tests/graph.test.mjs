import assert from "node:assert/strict";
import test from "node:test";

import {
  dependents,
  frontier,
  hardParents,
  isReady,
  judgeParents,
  mirrorNodeStatus,
  mirrorPlanIntoGraph,
  projectNodeStatus,
  readyNodes,
  softParents,
} from "../dist/graph.js";
import { buildFanoutDag, gateDecision, parsePlanBlock } from "../dist/planner.js";

// ---------------------------------------------------------------------------
// Graph fixtures. nodes/edges are keyed by id, matching RudderGraph on disk.
// ---------------------------------------------------------------------------

function node(id, status, extra = {}) {
  return {
    id,
    title: id,
    prompt: `prompt for ${id}`,
    backend: "claude",
    status,
    deps: [],
    source: "planner",
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z",
    ...extra,
  };
}

function edge(from, to, type) {
  return { id: `${from}->${to}`, from, to, type };
}

function makeGraph(nodes, edges) {
  return {
    version: 1,
    repoRoot: "/tmp/repo",
    nodes: Object.fromEntries(nodes.map((n) => [n.id, n])),
    edges: Object.fromEntries(edges.map((e) => [e.id, e])),
    updatedAt: "2026-01-01T00:00:00.000Z",
  };
}

// Diamond: A -> B, A -> C, B -> D, C -> D (all hard edges).
function diamond(statuses) {
  const nodes = ["A", "B", "C", "D"].map((id) => node(id, statuses[id] ?? "planned"));
  const edges = [edge("A", "B", "hard"), edge("A", "C", "hard"), edge("B", "D", "hard"), edge("C", "D", "hard")];
  return makeGraph(nodes, edges);
}

test("hardParents/softParents/dependents read the edge set", () => {
  const g = makeGraph(
    [node("A", "merged"), node("B", "planned"), node("C", "planned")],
    [edge("A", "B", "hard"), edge("A", "C", "soft")],
  );
  assert.deepEqual(hardParents(g, "B"), ["A"]);
  assert.deepEqual(softParents(g, "C"), ["A"]);
  assert.deepEqual(hardParents(g, "C"), []);
  assert.deepEqual(dependents(g, "A").sort(), ["B", "C"]);
});

test("isReady requires planned status and all hard parents merged", () => {
  // A merged -> B is ready; C still planned with an unmerged hard parent.
  const g = diamond({ A: "merged" });
  assert.equal(isReady(g, g.nodes.B), true);
  assert.equal(isReady(g, g.nodes.C), true);
  // D has two unmerged hard parents (B, C) -> not ready.
  assert.equal(isReady(g, g.nodes.D), false);
  // A is merged, not planned -> not ready.
  assert.equal(isReady(g, g.nodes.A), false);
});

test("readyNodes on a diamond: D is ready only when B AND C have merged", () => {
  // Stage 1: only A merged. B and C are ready, D is not.
  let g = diamond({ A: "merged" });
  assert.deepEqual(
    readyNodes(g).map((n) => n.id).sort(),
    ["B", "C"],
  );

  // Stage 2: B merged, C still running. D still blocked on C.
  g = diamond({ A: "merged", B: "merged", C: "running" });
  assert.deepEqual(
    readyNodes(g).map((n) => n.id),
    [],
  );

  // Stage 3: both B and C merged. D becomes ready.
  g = diamond({ A: "merged", B: "merged", C: "merged" });
  assert.deepEqual(
    readyNodes(g).map((n) => n.id),
    ["D"],
  );
});

test("soft parents never gate readiness", () => {
  const g = makeGraph(
    [node("A", "running"), node("B", "planned")],
    [edge("A", "B", "soft")],
  );
  assert.equal(isReady(g, g.nodes.B), true);
});

// ---------------------------------------------------------------------------
// Judge edges (fan-out-and-judge): a judge node is ready once every variant
// parent has REACHED review (or merged), not before, and not gated on merge.
// ---------------------------------------------------------------------------

// V1, V2 variants -> J judge (two judge edges).
function fanout(statuses) {
  const nodes = ["V1", "V2", "J"].map((id) => node(id, statuses[id] ?? "planned"));
  const edges = [edge("V1", "J", "judge"), edge("V2", "J", "judge")];
  return makeGraph(nodes, edges);
}

test("judgeParents reads the judge edge set", () => {
  const g = fanout({});
  assert.deepEqual(judgeParents(g, "J").sort(), ["V1", "V2"]);
  assert.deepEqual(judgeParents(g, "V1"), []);
});

test("judge node is NOT ready while a variant is still running", () => {
  // Both variants running: the judge has nothing to compare yet.
  const g = fanout({ V1: "running", V2: "running" });
  assert.equal(isReady(g, g.nodes.J), false);
  // One variant reached review, the other still running -> still not ready.
  const g2 = fanout({ V1: "review", V2: "running" });
  assert.equal(isReady(g2, g2.nodes.J), false);
});

test("judge node IS ready when every variant reaches review (not merged)", () => {
  const g = fanout({ V1: "review", V2: "review" });
  assert.equal(isReady(g, g.nodes.J), true);
  assert.deepEqual(
    readyNodes(g).map((n) => n.id),
    ["J"],
  );
});

test("judge node IS ready when variants are review or merged", () => {
  const g = fanout({ V1: "merged", V2: "review" });
  assert.equal(isReady(g, g.nodes.J), true);
});

test("judge readiness also requires any hard parents to be merged", () => {
  // H -(hard)-> J, V -(judge)-> J. J ready only when H merged AND V in review.
  const g = makeGraph(
    [node("H", "review"), node("V", "review"), node("J", "planned")],
    [edge("H", "J", "hard"), edge("V", "J", "judge")],
  );
  // H is only in review, not merged -> J not ready.
  assert.equal(isReady(g, g.nodes.J), false);
  const g2 = makeGraph(
    [node("H", "merged"), node("V", "review"), node("J", "planned")],
    [edge("H", "J", "hard"), edge("V", "J", "judge")],
  );
  assert.equal(isReady(g2, g2.nodes.J), true);
});

test("frontier returns non-merged leaves", () => {
  // A merged, B running (child of A), C planned leaf. Frontier = B, C.
  const g = makeGraph(
    [node("A", "merged"), node("B", "running"), node("C", "planned")],
    [edge("A", "B", "hard")],
  );
  assert.deepEqual(
    frontier(g).map((n) => n.id).sort(),
    ["B", "C"],
  );

  // When B merges too, A is still not a frontier node (its only child merged
  // makes A a non-merged-leaf candidate only if A itself is unmerged). A is
  // merged, B merged -> only C remains on the frontier.
  const g2 = makeGraph(
    [node("A", "merged"), node("B", "merged"), node("C", "planned")],
    [edge("A", "B", "hard")],
  );
  assert.deepEqual(
    frontier(g2).map((n) => n.id),
    ["C"],
  );
});

// ---------------------------------------------------------------------------
// parsePlanBlock: id synthesis, unknown-id drop, unjustified-hard downgrade,
// cycle rejection.
// ---------------------------------------------------------------------------

function block(tasks) {
  return `prose before\nRUDDER_PLAN_TASKS_START\n${JSON.stringify({ tasks })}\nRUDDER_PLAN_TASKS_END\nprose after`;
}

test("parsePlanBlock synthesizes n0.. ids when absent", () => {
  const dag = parsePlanBlock(
    block([
      { title: "first", prompt: "do the first thing" },
      { title: "second", prompt: "do the second thing" },
    ]),
  );
  assert.deepEqual(
    dag.nodes.map((n) => n.id),
    ["n0", "n1"],
  );
  assert.equal(dag.edges.length, 0);
});

test("parsePlanBlock parses goal and success and derives defaults when absent", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "build the api", prompt: "do it", goal: "ship the api", success: "cargo test passes" },
      { id: "b", title: "build the ui", prompt: "do it" },
    ]),
  );
  const a = dag.nodes.find((n) => n.id === "a");
  const b = dag.nodes.find((n) => n.id === "b");
  // Explicit goal + success are parsed verbatim.
  assert.equal(a.goal, "ship the api");
  assert.equal(a.success, "cargo test passes");
  // Backward-compatible: a block omitting goal/success still parses, deriving
  // goal from the title and the canonical default success.
  assert.equal(b.goal, "build the ui");
  assert.equal(b.success, "the task is implemented and its own verification passes");
});

test("parsePlanBlock preflights long goal and success before storing nodes", () => {
  const dag = parsePlanBlock(
    block([
      {
        id: "a",
        title: "seed data",
        prompt: "write the seed data",
        goal: "G".repeat(4800),
        success: "S".repeat(4800),
      },
    ]),
  );
  const node = dag.nodes.find((n) => n.id === "a");
  assert.ok(node.goal.length <= 3000, "stored goal is capped before launch");
  assert.ok(node.success.length <= 3000, "stored success is capped before launch");
});

test("parsePlanBlock drops edges to unknown ids", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first" },
      {
        id: "b",
        title: "b",
        prompt: "second",
        deps: [
          { on: "a", type: "soft" },
          { on: "ghost", type: "soft" },
        ],
      },
    ]),
  );
  // Only the a->b edge survives; the dangling ghost edge is dropped.
  assert.equal(dag.edges.length, 1);
  assert.deepEqual(dag.edges[0], { from: "a", to: "b", type: "soft" });
  assert.deepEqual(dag.nodes.find((n) => n.id === "b").deps, [{ on: "a", type: "soft" }]);
});

test("parsePlanBlock downgrades an unjustified hard edge to soft", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first" },
      {
        id: "b",
        title: "b",
        prompt: "second",
        // hard but no `why` -> downgraded to soft.
        deps: [{ on: "a", type: "hard" }],
      },
    ]),
  );
  assert.equal(dag.edges[0].type, "soft");
  assert.equal(dag.edges[0].why, undefined);
});

test("parsePlanBlock keeps a justified hard edge", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first" },
      {
        id: "b",
        title: "b",
        prompt: "second",
        deps: [{ on: "a", type: "hard", why: "b needs the schema a migrates" }],
      },
    ]),
  );
  assert.equal(dag.edges[0].type, "hard");
  assert.equal(dag.edges[0].why, "b needs the schema a migrates");
});

test("parsePlanBlock rejects a hard cycle", () => {
  assert.throws(
    () =>
      parsePlanBlock(
        block([
          { id: "a", title: "a", prompt: "first", deps: [{ on: "b", type: "hard", why: "x" }] },
          { id: "b", title: "b", prompt: "second", deps: [{ on: "a", type: "hard", why: "y" }] },
        ]),
      ),
    /cycle/i,
  );
});

test("parsePlanBlock tolerates a soft cycle (no deadlock risk)", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first", deps: [{ on: "b", type: "soft" }] },
      { id: "b", title: "b", prompt: "second", deps: [{ on: "a", type: "soft" }] },
    ]),
  );
  assert.equal(dag.edges.length, 2);
});

test("parsePlanBlock skips tasks with an empty prompt and caps at the runaway backstop (100)", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "" },
      { id: "b", title: "b", prompt: "real" },
    ]),
  );
  assert.deepEqual(
    dag.nodes.map((n) => n.id),
    ["b"],
  );
  // A real plan never hits it; a pathological 120-task block is capped at 100.
  const big = parsePlanBlock(
    block(Array.from({ length: 120 }, (_, i) => ({ id: `t${i}`, title: `t${i}`, prompt: "real" }))),
  );
  assert.equal(big.nodes.length, 100, "runaway backstop caps at 100 tasks");
});

test("parsePlanBlock throws on a missing block", () => {
  assert.throws(() => parsePlanBlock("no block here"), /RUDDER_PLAN_TASKS_START/);
});

// ---------------------------------------------------------------------------
// gateDecision: trivial single tasks auto-run; everything else gates.
// ---------------------------------------------------------------------------

test("gateDecision auto-runs a single trivial 0-dep task", () => {
  const dag = parsePlanBlock(block([{ id: "a", title: "a", prompt: "fix the typo in the readme" }]));
  assert.equal(gateDecision(dag).autoRun, true);
});

test("gateDecision gates a multi-node plan", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first" },
      { id: "b", title: "b", prompt: "second" },
    ]),
  );
  assert.equal(gateDecision(dag).autoRun, false);
});

test("gateDecision gates a task with dependencies", () => {
  const dag = parsePlanBlock(
    block([
      { id: "a", title: "a", prompt: "first" },
      { id: "b", title: "b", prompt: "second", deps: [{ on: "a", type: "hard", why: "needs a" }] },
    ]),
  );
  assert.equal(gateDecision(dag).autoRun, false);
});

test("gateDecision gates a task with multi-step markers", () => {
  const dag = parsePlanBlock(
    block([{ id: "a", title: "a", prompt: "first do this, then do that, finally clean up" }]),
  );
  assert.equal(gateDecision(dag).autoRun, false);
});

test("gateDecision gates a task with a broad file scope", () => {
  const dag = parsePlanBlock(
    block([{ id: "a", title: "a", prompt: "touch many files", fileScope: ["a", "b", "c", "d"] }]),
  );
  assert.equal(gateDecision(dag).autoRun, false);
});

// ---------------------------------------------------------------------------
// buildFanoutDag: N variants + 1 judge with N judge edges and the right prompts.
// ---------------------------------------------------------------------------

test("buildFanoutDag produces N variants + 1 judge with N judge edges", () => {
  const dag = buildFanoutDag("implement the cache layer", 3);
  const variants = dag.nodes.filter((n) => n.id !== "judge");
  const judge = dag.nodes.find((n) => n.id === "judge");
  assert.equal(variants.length, 3);
  assert.ok(judge, "judge node exists");
  // Exactly N judge edges, all variant -> judge.
  assert.equal(dag.edges.length, 3);
  assert.ok(dag.edges.every((e) => e.type === "judge" && e.to === "judge"));
  assert.deepEqual(
    dag.edges.map((e) => e.from).sort(),
    ["v0", "v1", "v2"],
  );
  // Judge node deps are the N judge edges.
  assert.equal(judge.deps.length, 3);
  assert.ok(judge.deps.every((d) => d.type === "judge"));
});

test("buildFanoutDag varies each variant by a distinct angle hint", () => {
  const dag = buildFanoutDag("do the thing", 3);
  const variants = dag.nodes.filter((n) => n.id !== "judge");
  // Each variant prompt contains the original task plus its own angle line.
  assert.ok(variants.every((v) => v.prompt.includes("do the thing")));
  assert.ok(variants[0].prompt.includes("favor simplicity"));
  assert.ok(variants[1].prompt.includes("favor robustness"));
  assert.ok(variants[2].prompt.includes("favor minimal-diff"));
});

test("buildFanoutDag judge prompt references the task and asks to compare/produce", () => {
  const dag = buildFanoutDag("build a parser", 4);
  const judge = dag.nodes.find((n) => n.id === "judge");
  assert.ok(judge.prompt.includes("build a parser"));
  assert.ok(/compare/i.test(judge.prompt));
  assert.ok(/best/i.test(judge.prompt));
  assert.ok(/which variant/i.test(judge.prompt));
});

test("buildFanoutDag sets goal + success on every variant and the judge", () => {
  const dag = buildFanoutDag("build a parser", 3);
  const variants = dag.nodes.filter((n) => n.id !== "judge");
  const judge = dag.nodes.find((n) => n.id === "judge");
  // Each variant: objective = task with its angle; success = its own verification.
  assert.ok(variants.every((v) => v.goal && v.goal.includes("build a parser")));
  assert.ok(variants.every((v) => v.success === "your approach is implemented and its own verification passes"));
  // Judge: objective = pick/synthesize the best; success = final impl verified.
  assert.ok(judge.goal.includes("build a parser"));
  assert.equal(judge.success, "the final implementation is complete and verified");
});

test("buildFanoutDag clamps N to [2,6] and applies the backend option", () => {
  const tooFew = buildFanoutDag("x", 1, { backend: "codex" });
  assert.equal(tooFew.nodes.filter((n) => n.id !== "judge").length, 2);
  assert.ok(tooFew.nodes.every((n) => n.backend === "codex"));
  const tooMany = buildFanoutDag("x", 99);
  assert.equal(tooMany.nodes.filter((n) => n.id !== "judge").length, 6);
});

// ---------------------------------------------------------------------------
// mirrorPlanIntoGraph: the write-only projection of the TUI's in-memory plan.
// Maps TUI statuses, rebuilds edges from deps, and PRUNES nodes/edges absent
// from the payload so graph.json exactly reflects the live plan.
// ---------------------------------------------------------------------------

function emptyGraph() {
  return { version: 1, repoRoot: "/tmp/repo", nodes: {}, edges: {}, updatedAt: "t" };
}

test("mirrorNodeStatus maps the TUI vocabulary and defaults unknown to planned", () => {
  assert.equal(mirrorNodeStatus("running"), "running");
  assert.equal(mirrorNodeStatus("review"), "review");
  assert.equal(mirrorNodeStatus("merged"), "merged");
  assert.equal(mirrorNodeStatus("failed"), "failed");
  assert.equal(mirrorNodeStatus("blocked"), "blocked");
  assert.equal(mirrorNodeStatus("ready"), "ready");
  assert.equal(mirrorNodeStatus("planned"), "planned");
  assert.equal(mirrorNodeStatus("done"), "planned");
  assert.equal(mirrorNodeStatus(undefined), "planned");
});

test("mirrorPlanIntoGraph upserts nodes with mapped status and metadata", () => {
  const g = mirrorPlanIntoGraph(emptyGraph(), {
    nodes: [
      { id: "n0", title: "build api", status: "merged", backend: "codex", model: "o3", effort: "high" },
      { id: "n1", title: "build ui", status: "running", runId: "run-1", jjChangeId: "z123", worktreePath: "/w/n1" },
    ],
  });
  assert.deepEqual(Object.keys(g.nodes).sort(), ["n0", "n1"]);
  assert.equal(g.nodes.n0.status, "merged");
  assert.equal(g.nodes.n0.backend, "codex");
  assert.equal(g.nodes.n0.model, "o3");
  assert.equal(g.nodes.n0.effort, "high");
  assert.equal(g.nodes.n1.status, "running");
  assert.equal(g.nodes.n1.runId, "run-1");
  assert.equal(g.nodes.n1.jjChangeId, "z123");
  assert.deepEqual(g.nodes.n1.worktree, { path: "/w/n1" });
});

test("mirrorPlanIntoGraph rebuilds edges from deps and stores edge ids on the node", () => {
  const g = mirrorPlanIntoGraph(emptyGraph(), {
    nodes: [
      { id: "n0", title: "a", status: "merged" },
      { id: "n1", title: "b", status: "running", deps: [{ on: "n0", type: "hard" }] },
      { id: "n2", title: "c", status: "planned", deps: [{ on: "n0", type: "soft" }] },
    ],
  });
  const edges = Object.values(g.edges);
  assert.equal(edges.length, 2);
  const hard = edges.find((e) => e.type === "hard");
  const soft = edges.find((e) => e.type === "soft");
  assert.deepEqual({ from: hard.from, to: hard.to }, { from: "n0", to: "n1" });
  assert.deepEqual({ from: soft.from, to: soft.to }, { from: "n0", to: "n2" });
  // The node's deps array holds its incoming edge ids (matching the keyed edges).
  assert.deepEqual(g.nodes.n1.deps, [hard.id]);
  assert.deepEqual(g.nodes.n2.deps, [soft.id]);
});

test("mirrorPlanIntoGraph PRUNES nodes and edges absent from a later payload", () => {
  let g = mirrorPlanIntoGraph(emptyGraph(), {
    nodes: [
      { id: "n0", title: "a", status: "merged" },
      { id: "n1", title: "b", status: "running", deps: [{ on: "n0", type: "hard" }] },
    ],
  });
  assert.equal(Object.keys(g.nodes).length, 2);
  assert.equal(Object.keys(g.edges).length, 1);

  // A later mirror that drops n0 (and thus its edge) must prune both.
  g = mirrorPlanIntoGraph(g, {
    nodes: [{ id: "n1", title: "b", status: "review" }],
  });
  assert.deepEqual(Object.keys(g.nodes), ["n1"]);
  assert.equal(Object.keys(g.edges).length, 0, "stale n0->n1 edge pruned");
  assert.equal(g.nodes.n1.status, "review");
  assert.deepEqual(g.nodes.n1.deps, []);
});

test("mirrorPlanIntoGraph is idempotent for edge ids (stable keys, no churn)", () => {
  const payload = {
    nodes: [
      { id: "n0", title: "a", status: "merged" },
      { id: "n1", title: "b", status: "running", deps: [{ on: "n0", type: "hard" }] },
    ],
  };
  const first = mirrorPlanIntoGraph(emptyGraph(), payload);
  const firstEdgeIds = Object.keys(first.edges).sort();
  // Re-mirror the same plan onto the same graph: same edge keys (no duplicates).
  const second = mirrorPlanIntoGraph(first, payload);
  assert.deepEqual(Object.keys(second.edges).sort(), firstEdgeIds);
});

test("mirrorPlanIntoGraph preserves prompt/source/createdAt across re-mirror", () => {
  let g = emptyGraph();
  g.nodes.n0 = {
    id: "n0",
    title: "a",
    prompt: "the original detailed prompt",
    backend: "claude",
    status: "running",
    deps: [],
    source: "injection",
    createdAt: "2020-01-01T00:00:00.000Z",
    updatedAt: "2020-01-01T00:00:00.000Z",
  };
  g = mirrorPlanIntoGraph(g, { nodes: [{ id: "n0", title: "a", status: "review" }] });
  assert.equal(g.nodes.n0.prompt, "the original detailed prompt");
  assert.equal(g.nodes.n0.source, "injection");
  assert.equal(g.nodes.n0.createdAt, "2020-01-01T00:00:00.000Z");
  assert.equal(g.nodes.n0.status, "review");
});

test("mirrorPlanIntoGraph OVERWRITES a stale prompt when the new payload carries one", () => {
  // Reproduces the my-charts symptom: a node id reused by a later, different plan
  // kept the previous plan's prompt. The payload now carries the prompt, so it wins.
  let g = emptyGraph();
  g.nodes.n1 = {
    id: "n1",
    title: "Chart ranking",
    prompt: "Chart ranking + cross-period dedup logic",
    backend: "claude",
    status: "planned",
    deps: [],
    source: "planner",
    createdAt: "2020-01-01T00:00:00.000Z",
    updatedAt: "2020-01-01T00:00:00.000Z",
  };
  g = mirrorPlanIntoGraph(g, {
    nodes: [
      {
        id: "n1",
        title: "Implement js/auth.js (browser-only PKCE)",
        prompt: "Implement js/auth.js with PKCE",
        status: "planned",
      },
    ],
  });
  assert.equal(g.nodes.n1.title, "Implement js/auth.js (browser-only PKCE)");
  assert.equal(
    g.nodes.n1.prompt,
    "Implement js/auth.js with PKCE",
    "stale prompt overwritten by the new plan",
  );
  // createdAt/source still preserved on upsert (cosmetic identity).
  assert.equal(g.nodes.n1.createdAt, "2020-01-01T00:00:00.000Z");
  assert.equal(g.nodes.n1.source, "planner");
});

test("mirrorPlanIntoGraph on an empty payload clears the whole graph", () => {
  let g = mirrorPlanIntoGraph(emptyGraph(), { nodes: [{ id: "n0", title: "a", status: "running" }] });
  assert.equal(Object.keys(g.nodes).length, 1);
  g = mirrorPlanIntoGraph(g, {});
  assert.deepEqual(Object.keys(g.nodes), []);
  assert.deepEqual(Object.keys(g.edges), []);
});

// ---------------------------------------------------------------------------
// projectNodeStatus: daemon-owned terminal statuses are sticky against the
// worker's run.json (which only ever reaches "completed", never "merged").
// Regression: without stickiness a merged node re-projects to "review" every
// tick, auto-merge re-fires on an already-merged change, and the node thrashes
// review<->merged and never settles as done.
// ---------------------------------------------------------------------------

test("projectNodeStatus keeps a merged node merged even when run.json says completed", () => {
  const n = node("n0", "merged");
  assert.equal(projectNodeStatus(n, { status: "completed" }), "merged");
  assert.equal(projectNodeStatus(n, { status: "merged" }), "merged");
  // No run at all (post-restart) must still hold merged.
  assert.equal(projectNodeStatus(n, undefined), "merged");
});

test("projectNodeStatus keeps a blocked node blocked (failed-parent / held conflict)", () => {
  const n = node("n0", "blocked");
  assert.equal(projectNodeStatus(n, { status: "completed" }), "blocked");
  assert.equal(projectNodeStatus(n, undefined), "blocked");
});

test("projectNodeStatus still projects a completed run to review for a running node", () => {
  assert.equal(projectNodeStatus(node("n0", "running"), { status: "completed" }), "review");
  assert.equal(projectNodeStatus(node("n0", "review"), { status: "completed" }), "review");
  assert.equal(projectNodeStatus(node("n0", "running"), { status: "running" }), "running");
  assert.equal(projectNodeStatus(node("n0", "running"), { status: "failed" }), "failed");
  assert.equal(projectNodeStatus(node("n0", "ready"), undefined), "ready");
  assert.equal(projectNodeStatus(node("n0", "planned"), undefined), "planned");
});
