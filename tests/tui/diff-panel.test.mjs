// ⌥d diff panel, end to end on the REAL binary: a worker writes a file into
// its jj workspace, the panel opens beside the worker with that file parsed
// from `jj diff`, scrolls, solos under ⌥h, and closes on Esc.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, removeScratch, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

const ALT_D = "\x1bd";
const ALT_H = "\x1bh";

async function finishedWorker(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer, cols: 160, rows: 40 });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("write the done marker");
  await session.press("Enter");
  // The completer exits after writing DONE.txt; the row reaches review.
  await session.waitForText("press m", { timeout: 30_000 });
  return { repo, session };
}

test("⌥d opens a diff panel with the worker's change set and Esc closes it", { timeout: 90_000 }, async (t) => {
  const { session } = await finishedWorker(t, "rudder-tui-diffpanel-");
  await session.press("Ctrl+W");
  await session.press("2");

  await session.write(ALT_D);
  await session.waitForText("┌ diff", { timeout: 10_000 });
  // The panel parses the real jj diff of the workspace: the completer's file,
  // its one added line, and the running totals in the title.
  await session.waitForText("DONE.txt", { timeout: 15_000 });
  const screen = await session.screen();
  assert.ok(screen.includes("+1 −0"), `stats for the one-line file:\n${screen}`);
  assert.ok(screen.includes("done marker"), `the added line itself:\n${screen}`);
  assert.ok(screen.includes("A DONE.txt"), `an added file is badged A:\n${screen}`);
  assert.ok(screen.includes("┌ worker"), `the worker pane stays beside it:\n${screen}`);

  // ⌥h solos the focused panel: the diff fills the screen, nothing else drawn.
  await session.write(ALT_H);
  await session.waitFor(async () => {
    const s = await session.screen();
    return s.includes("┌ diff") && !s.includes("┌ agents") && !s.includes("┌ worker");
  }, { timeout: 10_000 });
  await session.write(ALT_H);
  await session.waitForText("┌ agents", { timeout: 10_000 });
  assert.ok((await session.screen()).includes("┌ diff"), "the split comes back with the panel");

  // Esc closes the panel and the worker pane widens again.
  await session.press("Escape");
  await session.waitFor(async () => !(await session.screen()).includes("┌ diff"), { timeout: 10_000 });
  assert.ok((await session.screen()).includes("┌ worker"));
});
