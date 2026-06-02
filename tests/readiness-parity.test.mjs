import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import test from "node:test";

import { readyNodes } from "../dist/graph.js";

// The shared readiness contract. The SAME fixture is consumed by the Rust parity
// test (native/src/app_tests.rs::readiness_parity_fixture). If the readiness rule
// changes, edit the fixture once and both suites fail until both implementations
// agree. Keeps the TUI (Rust) and daemon (TS) schedulers from drifting.
const fixturePath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "fixtures",
  "readiness-cases.json",
);
const fixture = JSON.parse(readFileSync(fixturePath, "utf8"));

/** Build a RudderGraph from a fixture case (nodes: {id: status}, edges: [{from,to,type}]). */
function buildGraph(testCase) {
  const edges = {};
  const incoming = {};
  for (const e of testCase.edges) {
    const id = `e_${e.from}__${e.type}__${e.to}`;
    edges[id] = { id, from: e.from, to: e.to, type: e.type };
    (incoming[e.to] ||= []).push(id);
  }
  const nodes = {};
  for (const [id, status] of Object.entries(testCase.nodes)) {
    nodes[id] = {
      id,
      title: id,
      prompt: id,
      backend: "claude",
      status,
      deps: incoming[id] || [],
      source: "planner",
      createdAt: "2026-01-01T00:00:00.000Z",
      updatedAt: "2026-01-01T00:00:00.000Z",
    };
  }
  return { version: 1, repoRoot: "/tmp", nodes, edges, updatedAt: "2026-01-01T00:00:00.000Z" };
}

for (const testCase of fixture.cases) {
  test(`readiness parity (TS): ${testCase.name}`, () => {
    const ready = readyNodes(buildGraph(testCase))
      .map((n) => n.id)
      .sort();
    assert.deepEqual(ready, [...testCase.expectedReady].sort());
  });
}
