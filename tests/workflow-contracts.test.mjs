import test from "node:test";
import assert from "node:assert/strict";

import {
  CODEX_RUDDER_PLANNER_CONFIG_ARGS,
  CODEX_RUDDER_WORKER_CONFIG_ARGS,
} from "../dist/codex-binary.js";
import { columnForRun, hasVerifiedDelivery } from "../dist/board/daemon.js";
import { parseCompletionNoteArg } from "../dist/surfaces.js";

function run(overrides = {}) {
  return {
    id: "run-1",
    status: "completed",
    task: "do the work",
    backend: "codex",
    createdAt: "2026-07-17T00:00:00Z",
    updatedAt: "2026-07-17T00:01:00Z",
    repoRoot: "/tmp/repo",
    targetBranch: "main",
    baseCommit: "abc",
    worktree: { enabled: false, path: "/tmp/repo" },
    ...overrides,
  };
}

test("Codex workers get plugin and Computer Use parity while planners stay restricted", () => {
  assert.ok(CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.plugins=true"));
  assert.ok(CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.computer_use=true"));
  assert.ok(!CODEX_RUDDER_WORKER_CONFIG_ARGS.includes("features.plugins=false"));
  assert.ok(CODEX_RUDDER_PLANNER_CONFIG_ARGS.includes("features.plugins=false"));
  assert.ok(CODEX_RUDDER_PLANNER_CONFIG_ARGS.includes("features.computer_use=false"));
});

test("direct completion is done, but requested delivery stays review until proof is complete", () => {
  assert.equal(columnForRun(run({ mode: "main" })), "done");
  assert.equal(
    columnForRun(
      run({
        mode: "main",
        delivery: { required: true, status: "pending", checks: [] },
      }),
    ),
    "review",
  );

  const delivered = run({
    mode: "main",
    delivery: {
      required: true,
      kind: "web",
      status: "verified",
      target: "https://example.test",
      revision: "abc123",
      verifiedAt: "2026-07-17T00:02:00Z",
      checks: ["GET / returned 200"],
    },
  });
  assert.equal(hasVerifiedDelivery(delivered), true);
  assert.equal(columnForRun(delivered), "done");

  assert.equal(
    columnForRun(run({ worktree: { enabled: true, path: "/tmp/worker" } })),
    "review",
    "an isolated worker still needs integration",
  );
});

test("rudder done preserves structured delivery evidence", () => {
  const note = parseCompletionNoteArg(
    JSON.stringify({
      summary: "deployed production",
      delivery: {
        kind: "web",
        status: "verified",
        target: "https://example.test",
        verifiedAt: "2026-07-17T00:02:00Z",
        checks: ["homepage passed"],
      },
    }),
  );
  assert.equal(note.delivery?.kind, "web");
  assert.deepEqual(note.delivery?.checks, ["homepage passed"]);
});
