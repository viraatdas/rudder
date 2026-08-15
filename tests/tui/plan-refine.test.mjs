// Refining a plan after it lands. Field report: "the refine the plan once the
// plan is made is not working."
//
// Two stacked bugs made the natural gesture — read the proposed plan, type
// "make it three nodes instead", press Enter — do the wrong thing silently:
//   1. Capturing the plan moved focus INTO the review editor, so the sentence
//      was typed into node 0's TITLE field instead of being sent to the
//      planner. The plan quietly mutated; no refine ever happened.
//   2. In that editor bare j/k moved between tasks rather than inserting, so
//      the k in "make" jumped rows mid-sentence — text landed on a different
//      node than the one on screen.
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import test from "node:test";

import {
  assertPrerequisites,
  interactiveOrchestratorBackend,
  launchRudder,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

test("typing after a plan lands refines it with the planner", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-refine-");
  t.after(() => removeScratch(repo));
  const { claudeBin, stdinLog } = await interactiveOrchestratorBackend(repo, 2);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("/plan build the thing");
  await session.press("Enter");
  // The approval gate: the orchestrator's DAG, presented for review.
  await session.waitForText("Plan ready", { timeout: 30_000 });

  // The user's natural next move — type the change, press Enter. NO pane
  // switching: whatever rudder focused when the plan landed is what a real
  // user types into.
  await session.type("make it three nodes instead");
  await session.press("Enter");

  // It must reach the ORCHESTRATOR, not a text field. Poll the fake's stdin
  // log the way waits poll the screen.
  const deadline = Date.now() + 20_000;
  for (;;) {
    const log = await fsp.readFile(stdinLog, "utf8").catch(() => "");
    if (log.includes("make it three nodes instead")) break;
    if (Date.now() >= deadline) {
      throw new Error(
        `the refinement never reached the orchestrator; its stdin was: ${JSON.stringify(log)}\n--- screen ---\n${await session.screen()}`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }

  // And the plan itself must be untouched: the sentence must not have been
  // typed into a node's title (bug 1's signature).
  const screen = await session.screen();
  assert.ok(
    !screen.includes("node 0make") && !screen.includes("make it three nodes instead…"),
    `the refinement leaked into a plan field:\n${screen}`,
  );
  assert.ok(screen.includes("node 0"), `the proposed node survived intact:\n${screen}`);
});

test("the plan review editor can type letters that are also nav keys", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-review-edit-");
  t.after(() => removeScratch(repo));
  const { claudeBin } = await interactiveOrchestratorBackend(repo, 2);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("/plan build the thing");
  await session.press("Enter");
  await session.waitForText("Plan ready", { timeout: 30_000 });

  // Open the editor explicitly (typing now refines instead), then type a word
  // containing BOTH j and k. Before the fix these jumped tasks, so the word
  // could never be entered and the edit landed on another node.
  await session.press("Ctrl+W");
  await session.press("2");
  await session.type("jk");
  await session.waitFor((screen) => screen.includes("jk"), {
    timeout: 10_000,
    label: "j and k typed into the focused field",
  });
});


test("a task in a plain directory says isolation is off instead of failing quietly", { timeout: 60_000 }, async (t) => {
  // Field report (job-nikita): a whole session of agents ran in a folder that
  // was never a git repo. Workspace creation "succeeds" there by falling back
  // to the checkout itself, so nothing complained — the agents just had no
  // isolation and nothing that could ever merge, and the rows only looked
  // "failed". The dashboard must SAY so, and name the fix.
  const fsp = await import("node:fs/promises");
  const os = await import("node:os");
  const path = await import("node:path");
  const dir = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-tui-norepo-")));
  t.after(() => removeScratch(dir));
  await fsp.writeFile(path.join(dir, "README.md"), "not a repo\n");
  const { claudeBin } = await interactiveOrchestratorBackend(dir, 1);

  const session = await launchRudder(t, { repo: dir, claudeBin });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("do some work here");
  await session.press("Enter");

  await session.waitForText("no git repo here", { timeout: 30_000 });
  await session.waitForText("git init", { timeout: 10_000 });
});
