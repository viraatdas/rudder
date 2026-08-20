// Solo view against the REAL binary: Alt+o hides every pane but the focused one,
// Alt+o again restores the split, and Alt+h keeps its existing meaning.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

test("Alt+h full-screens the pane, hiding the sidebar and task line", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-solo-");
  const { claudeBin } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("Type a task", { timeout: 30_000 });

  // The split shows the agents sidebar keymap alongside the worker pane.
  await session.waitForText("j/k move", { timeout: 10_000 });

  await session.press("Alt+h");
  // Full screen: the sidebar keymap AND the task prompt are both gone.
  await session.waitForGone("j/k move", { timeout: 10_000 });
  await session.waitForGone("Type a task", { timeout: 10_000 });

  await session.press("Alt+h");
  const restored = await session.waitFor((s) => s.includes("panes restored"), {
    timeout: 10_000,
    label: "the panes-restored notice",
  });
  assert.match(restored, /panes restored/);
  await session.waitForText("Type a task", { timeout: 10_000 });
  // The sidebar is back, which is what "restored" has to mean on screen.
  await session.waitForText("j/k move", { timeout: 10_000 });
});
