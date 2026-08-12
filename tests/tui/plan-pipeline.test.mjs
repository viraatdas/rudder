// The /plan pipeline — the deepest, most complex flow, and until now entirely
// untested end-to-end: a plan is proposed as a task DAG, approved, its nodes
// run as isolated workers, and their work merges into the checkout. This is a
// journey because every step depends on the last, and the whole storyline is
// worth keeping in the report when any of it breaks.
import fsp from "node:fs/promises";
import test from "node:test";

import { journey } from "tui-integration-tests";
import {
  assertPrerequisites,
  launchRudder,
  planPipelineBackend,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

test("plan → approve → workers → merge lands every node's work", { timeout: 180_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-plan-");
  t.after(() => removeScratch(repo));
  const { claudeBin, nodeCount } = await planPipelineBackend(repo, 2);
  // The headless decomposer scrapes the DAG from the planner's output; the
  // default interactive orchestrator reads it from RUDDER.md instead (a
  // separate, follow-up coverage target).
  const session = await launchRudder(t, {
    repo,
    claudeBin,
    env: { RUDDER_INTERACTIVE_ORCHESTRATOR: "0" },
  });
  const story = journey(session, "plan pipeline: propose, approve, run, merge");

  await story.step("dashboard boots", async () => {
    await session.waitForText("Type a task", { timeout: 20_000 });
  });

  await story.step("/plan proposes a two-node DAG", async () => {
    await session.type("/plan build the two node thing");
    await session.press("Enter");
    await session.waitForText("Plan ready", { timeout: 40_000 });
    await session.waitForText("node 0", { timeout: 10_000 });
    await session.waitForText("node 1", { timeout: 10_000 });
  });

  await story.step("approving the plan launches both nodes as workers", async () => {
    // Leave the inline plan-review editor, focus the agents list, select the
    // pinned orchestrator row (◆), and Enter to approve — the launch trigger.
    await session.press("Escape");
    await session.press("Ctrl+W");
    await session.press("1");
    await session.press("k");
    await session.press("k");
    await session.waitFor((s) => s.includes("▶") && s.includes("◆"), {
      timeout: 10_000,
      label: "the orchestrator row selected",
    });
    await session.press("Enter");
    await session.waitForText("working", { timeout: 40_000 });
  });

  await story.step("both node workers finish", async () => {
    // Each node writes NODE-<workspace>.txt in its isolated workspace.
    const deadline = Date.now() + 60_000;
    for (;;) {
      const files = [];
      const roots = await fsp.readdir(repo, { withFileTypes: true }).catch(() => []);
      const wt = roots.find((e) => e.name === ".rudder-worktrees");
      if (wt) {
        const walk = async (dir) => {
          for (const e of await fsp.readdir(dir, { withFileTypes: true }).catch(() => [])) {
            const p = `${dir}/${e.name}`;
            if (e.isDirectory()) await walk(p);
            else if (e.name.startsWith("NODE-")) files.push(p);
          }
        };
        await walk(`${repo}/.rudder-worktrees`);
      }
      if (files.length >= nodeCount) break;
      if (Date.now() >= deadline) {
        throw new Error(`only ${files.length}/${nodeCount} node files were produced`);
      }
      await new Promise((r) => setTimeout(r, 400));
    }
  });

  await story.step("nodes auto-merge and every file lands in the checkout", async () => {
    // A plan integrates its nodes automatically as they finish (no manual
    // merge). Wait for the merged state, then let the disk be the arbiter:
    // every node's file must be in the main checkout.
    await session.waitForText("merged", { timeout: 40_000 });
    const deadline = Date.now() + 40_000;
    for (;;) {
      const landed = (await fsp.readdir(repo)).filter((n) => n.startsWith("NODE-"));
      if (landed.length >= nodeCount) return;
      if (Date.now() >= deadline) {
        throw new Error(`only ${landed.length}/${nodeCount} node files reached the checkout`);
      }
      await new Promise((r) => setTimeout(r, 400));
    }
  });

  await story.end();
});
