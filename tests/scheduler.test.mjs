import assert from "node:assert/strict";
import test from "node:test";

import {
  computeBlocked,
  isJudgedVariant,
  isOverBudget,
  selectLaunchable,
  sumNodeTokens,
  tokensNewer,
  undeliveredJudgeEdges,
  undeliveredSoftEdges,
} from "../dist/scheduler.js";
import { readyNodes } from "../dist/graph.js";

// ---------------------------------------------------------------------------
// In-memory RudderGraph fixtures. nodes/edges keyed by id, matching disk shape.
// Only the PURE scheduler decision functions are exercised here: no workers are
// spawned, no jj is called, no daemon is started.
// ---------------------------------------------------------------------------

function node(id, status, createdAt, extra = {}) {
  return {
    id,
    title: id,
    prompt: `prompt for ${id}`,
    backend: "claude",
    status,
    deps: [],
    source: "planner",
    createdAt,
    updatedAt: createdAt,
    ...extra,
  };
}

function edge(from, to, type, extra = {}) {
  return { id: `${from}->${to}`, from, to, type, ...extra };
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
  const at = {
    A: "2026-01-01T00:00:00.000Z",
    B: "2026-01-01T00:00:01.000Z",
    C: "2026-01-01T00:00:02.000Z",
    D: "2026-01-01T00:00:03.000Z",
  };
  const nodes = ["A", "B", "C", "D"].map((id) => node(id, statuses[id] ?? "planned", at[id]));
  const edges = [
    edge("A", "B", "hard"),
    edge("A", "C", "hard"),
    edge("B", "D", "hard"),
    edge("C", "D", "hard"),
  ];
  return makeGraph(nodes, edges);
}

// ---------------------------------------------------------------------------
// selectLaunchable
// ---------------------------------------------------------------------------

test("selectLaunchable returns only ready nodes, oldest-first, within capacity", () => {
  const g = diamond({ A: "merged" });
  // B and C are ready (A merged); D is not (B,C unmerged). maxParallel 3, none
  // running -> both B and C selectable, oldest-first (B created before C).
  const picked = selectLaunchable(g, 0, 3);
  assert.deepEqual(picked.map((n) => n.id), ["B", "C"]);
});

test("selectLaunchable respects maxParallel minus runningCount", () => {
  const g = diamond({ A: "merged" });
  // One slot left (maxParallel 2, one running) -> only the oldest ready node.
  const picked = selectLaunchable(g, 1, 2);
  assert.deepEqual(picked.map((n) => n.id), ["B"]);
});

test("selectLaunchable returns nothing at capacity", () => {
  const g = diamond({ A: "merged" });
  assert.deepEqual(selectLaunchable(g, 3, 3), []);
  assert.deepEqual(selectLaunchable(g, 5, 3), []);
});

test("selectLaunchable never returns a node whose hard parents are unmerged", () => {
  // Only A merged: D must never be selectable while B/C are unmerged.
  const g = diamond({ A: "merged", B: "running", C: "running" });
  const picked = selectLaunchable(g, 0, 5);
  assert.equal(picked.find((n) => n.id === "D"), undefined);
});

test("diamond: D is launchable only after BOTH B and C merge", () => {
  // Stage 1: A merged, B/C running. D not ready.
  let g = diamond({ A: "merged", B: "running", C: "running" });
  assert.equal(readyNodes(g).find((n) => n.id === "D"), undefined);
  assert.deepEqual(selectLaunchable(g, 2, 3), []);

  // Stage 2: B merged, C still running. D still not ready.
  g = diamond({ A: "merged", B: "merged", C: "running" });
  assert.equal(selectLaunchable(g, 1, 3).find((n) => n.id === "D"), undefined);

  // Stage 3: B and C merged. D becomes the only launchable node.
  g = diamond({ A: "merged", B: "merged", C: "merged" });
  assert.deepEqual(selectLaunchable(g, 0, 3).map((n) => n.id), ["D"]);
});

// ---------------------------------------------------------------------------
// computeBlocked
// ---------------------------------------------------------------------------

test("computeBlocked returns hard dependents of a failed node", () => {
  // A -> B (hard), A -> C (soft). A fails: B blocked, C unaffected.
  const g = makeGraph(
    [
      node("A", "failed", "2026-01-01T00:00:00.000Z"),
      node("B", "planned", "2026-01-01T00:00:01.000Z"),
      node("C", "planned", "2026-01-01T00:00:02.000Z"),
    ],
    [edge("A", "B", "hard"), edge("A", "C", "soft")],
  );
  assert.deepEqual(computeBlocked(g, "A"), ["B"]);
});

test("computeBlocked walks transitive hard dependents", () => {
  // A -> B -> D (all hard). A fails: B and D both blocked.
  const g = diamond({ A: "failed" });
  const blocked = computeBlocked(g, "A").sort();
  assert.deepEqual(blocked, ["B", "C", "D"]);
});

test("computeBlocked stops at merged nodes", () => {
  // A -> B (hard) but B already merged: B (and anything below it) not re-blocked.
  const g = makeGraph(
    [
      node("A", "failed", "2026-01-01T00:00:00.000Z"),
      node("B", "merged", "2026-01-01T00:00:01.000Z"),
    ],
    [edge("A", "B", "hard")],
  );
  assert.deepEqual(computeBlocked(g, "A"), []);
});

// ---------------------------------------------------------------------------
// undeliveredSoftEdges
// ---------------------------------------------------------------------------

test("undeliveredSoftEdges returns soft edges whose parent merged and not delivered", () => {
  const g = makeGraph(
    [
      node("A", "merged", "2026-01-01T00:00:00.000Z"),
      node("B", "running", "2026-01-01T00:00:01.000Z"),
    ],
    [edge("A", "B", "soft")],
  );
  const edges = undeliveredSoftEdges(g);
  assert.equal(edges.length, 1);
  assert.equal(edges[0].from, "A");
  assert.equal(edges[0].to, "B");
});

test("undeliveredSoftEdges excludes hard edges, unmerged parents, and delivered edges", () => {
  const g = makeGraph(
    [
      node("A", "merged", "2026-01-01T00:00:00.000Z"),
      node("B", "merged", "2026-01-01T00:00:01.000Z"),
      node("C", "running", "2026-01-01T00:00:02.000Z"),
      node("D", "running", "2026-01-01T00:00:03.000Z"),
    ],
    [
      edge("A", "B", "hard"), // hard -> excluded
      edge("A", "C", "soft", { delivered: true }), // already delivered -> excluded
      edge("D", "C", "soft"), // parent D not merged -> excluded
      edge("B", "C", "soft"), // parent merged, undelivered -> included
    ],
  );
  const edges = undeliveredSoftEdges(g);
  assert.deepEqual(
    edges.map((e) => `${e.from}->${e.to}`),
    ["B->C"],
  );
});

// ---------------------------------------------------------------------------
// Fan-out-and-judge: judge diff delivery is gated on review (not merge), and
// only soft edges are delivered through the soft path.
// ---------------------------------------------------------------------------

test("undeliveredJudgeEdges fires once a variant reaches review, not before", () => {
  // V1 review, V2 still running. Only the V1->J judge edge is deliverable.
  const g = makeGraph(
    [
      node("V1", "review", "2026-01-01T00:00:00.000Z"),
      node("V2", "running", "2026-01-01T00:00:01.000Z"),
      node("J", "planned", "2026-01-01T00:00:02.000Z"),
    ],
    [edge("V1", "J", "judge"), edge("V2", "J", "judge")],
  );
  assert.deepEqual(
    undeliveredJudgeEdges(g).map((e) => `${e.from}->${e.to}`),
    ["V1->J"],
  );
});

test("undeliveredJudgeEdges includes merged variants and excludes delivered ones", () => {
  const g = makeGraph(
    [
      node("V1", "merged", "2026-01-01T00:00:00.000Z"),
      node("V2", "review", "2026-01-01T00:00:01.000Z"),
      node("V3", "planned", "2026-01-01T00:00:02.000Z"),
      node("J", "planned", "2026-01-01T00:00:03.000Z"),
    ],
    [
      edge("V1", "J", "judge", { delivered: true }), // already delivered -> excluded
      edge("V2", "J", "judge"), // review, undelivered -> included
      edge("V3", "J", "judge"), // still planned -> excluded
    ],
  );
  assert.deepEqual(
    undeliveredJudgeEdges(g).map((e) => `${e.from}->${e.to}`),
    ["V2->J"],
  );
});

test("undeliveredSoftEdges ignores judge edges entirely", () => {
  const g = makeGraph(
    [
      node("V1", "review", "2026-01-01T00:00:00.000Z"),
      node("J", "planned", "2026-01-01T00:00:01.000Z"),
    ],
    [edge("V1", "J", "judge")],
  );
  assert.deepEqual(undeliveredSoftEdges(g), []);
});

test("isJudgedVariant flags only the variant feeding a judge edge", () => {
  const g = makeGraph(
    [
      node("V1", "review", "2026-01-01T00:00:00.000Z"),
      node("J", "planned", "2026-01-01T00:00:01.000Z"),
    ],
    [edge("V1", "J", "judge")],
  );
  assert.equal(isJudgedVariant(g, "V1"), true);
  assert.equal(isJudgedVariant(g, "J"), false);
});

// ---------------------------------------------------------------------------
// Budget token accounting: sum across the graph and hold launches when the cap
// is reached.
// ---------------------------------------------------------------------------

test("sumNodeTokens sums input+output across every node that reported usage", () => {
  const g = makeGraph(
    [
      node("A", "merged", "2026-01-01T00:00:00.000Z", { tokens: { input: 100, output: 50 } }),
      node("B", "running", "2026-01-01T00:00:01.000Z", { tokens: { input: 200, output: 25 } }),
      node("C", "planned", "2026-01-01T00:00:02.000Z" /* no tokens */),
    ],
    [],
  );
  assert.equal(sumNodeTokens(g), 100 + 50 + 200 + 25);
});

test("isOverBudget holds only when a positive cap is reached", () => {
  assert.equal(isOverBudget(999, undefined), false); // no cap -> never over
  assert.equal(isOverBudget(999, 0), false); // zero cap is disabled
  assert.equal(isOverBudget(499, 500), false); // under cap
  assert.equal(isOverBudget(500, 500), true); // at cap -> hold
  assert.equal(isOverBudget(501, 500), true); // over cap -> hold
});

test("budget sum + hold: a graph whose tokens exceed maxTokens yields zero launchable", () => {
  // A merged (spent), B ready and would launch, but the cap is already reached.
  const g = makeGraph(
    [
      node("A", "merged", "2026-01-01T00:00:00.000Z", { tokens: { input: 600, output: 400 } }),
      node("B", "planned", "2026-01-01T00:00:01.000Z"),
    ],
    [edge("A", "B", "soft")], // soft -> B is ready (soft never gates)
  );
  const spent = sumNodeTokens(g); // 1000
  const maxTokens = 800;
  assert.equal(isOverBudget(spent, maxTokens), true);
  // The scheduler holds when over budget: simulate the gate the tick applies.
  const launchable = isOverBudget(spent, maxTokens) ? [] : selectLaunchable(g, 0, 3);
  assert.deepEqual(launchable, []);
  // Sanity: without the cap, B WOULD be launchable.
  assert.deepEqual(selectLaunchable(g, 0, 3).map((n) => n.id), ["B"]);
});

test("tokensNewer only raises totals, never lowers", () => {
  assert.equal(tokensNewer(undefined, { input: 0, output: 0 }), false);
  assert.equal(tokensNewer(undefined, { input: 1, output: 0 }), true);
  assert.equal(tokensNewer({ input: 10, output: 10 }, { input: 25, output: 0 }), true);
  assert.equal(tokensNewer({ input: 10, output: 10 }, { input: 5, output: 5 }), false);
});
