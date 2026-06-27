import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { writeAgentContext } from "../dist/run-manager.js";
import { createRunRecord, saveRunRecord } from "../dist/state.js";

test("writeAgentContext separates active, ready, completed, and merge-ready runs", async (t) => {
  const repo = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-run-context-"));
  t.after(async () => {
    await fsp.rm(repo, { recursive: true, force: true });
  });

  async function makeRun(id, status, { backend = "codex", worktree = true } = {}) {
    const workspace = worktree ? path.join(repo, `${id}-workspace`) : repo;
    await fsp.mkdir(workspace, { recursive: true });
    const run = await createRunRecord({
      id,
      repoRoot: repo,
      task: `${id} task`,
      backend,
      targetBranch: "base",
      baseCommit: "base",
      vcs: "jj",
      useWorktree: worktree,
      worktreePath: workspace,
      worktreeWorkspaceName: worktree ? `rudder-${id}` : undefined,
      worktreeJjChangeId: worktree ? `${id}-change` : undefined,
    });
    run.status = status;
    run.taskSummaryLlm = true;
    if (status === "steering") {
      run.autoSteer = { count: 0, max: 2, waitingSince: "2026-06-26T00:00:00.000Z" };
    }
    await saveRunRecord(run);
    return run;
  }

  await makeRun("created-run", "created", { backend: "claude" });
  await makeRun("running-run", "running");
  await makeRun("waiting-run", "steering");
  await makeRun("ready-worktree", "completed");
  await makeRun("ready-current", "completed", { worktree: false });
  await makeRun("merged-run", "merged", { backend: "claude" });

  await writeAgentContext(repo);

  const text = await fsp.readFile(path.join(repo, "RUDDER.md"), "utf8");
  assert.match(
    text,
    /- totals: total=6 running=1 waiting=1 done=2 merged=1 failed=0 stopped=0 pending-starts=1/,
  );
  assert.match(
    text,
    /- active-now: running=1 waiting=1 review-ready=2 merge-ready=1 pending-starts=1/,
  );
  assert.match(text, /- backends: claude=2 codex=4 acpx=0/);

  const active = text.split("## Active local Rudder agents")[1].split("## Ready local Rudder agents")[0];
  assert.match(active, /created-run/);
  assert.match(active, /running-run/);
  assert.match(active, /waiting-run/);
  assert.doesNotMatch(active, /ready-worktree/);
  assert.doesNotMatch(active, /merged-run/);

  const ready = text.split("## Ready local Rudder agents")[1].split("## Completed local Rudder agents")[0];
  assert.match(ready, /ready-worktree/);
  assert.match(ready, /ready-current/);
  assert.doesNotMatch(ready, /merged-run/);

  const completed = text.split("## Completed local Rudder agents")[1];
  assert.match(completed, /merged-run/);

  const workspaceCopy = await fsp.readFile(path.join(repo, "ready-worktree-workspace", "RUDDER.md"), "utf8");
  assert.equal(workspaceCopy, text);
});
