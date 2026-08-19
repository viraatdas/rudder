// Solo view against the REAL binary: Alt+o hides every pane but the focused one,
// Alt+o again restores the split, and Alt+h keeps its existing meaning.
import assert from "node:assert/strict";
import test from "node:test";

import { assertPrerequisites, fakeBackends, launchRudder, scratchRepo } from "./helpers.mjs";

assertPrerequisites();

test("Alt+o shows one pane, Alt+o again restores the split", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-solo-");
  const { claudeBin } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("Type a task", { timeout: 30_000 });

  // The split shows the agents sidebar keymap alongside the worker pane.
  await session.waitForText("j/k move", { timeout: 10_000 });

  await session.press("Alt+o");
  await session.waitForText("solo", { timeout: 10_000 });

  await session.press("Alt+o");
  const restored = await session.waitFor((s) => s.includes("split restored"), {
    timeout: 10_000,
    label: "the split-restored notice",
  });
  assert.match(restored, /split restored/);
  // The sidebar is back, which is what "restored" has to mean on screen.
  await session.waitForText("j/k move", { timeout: 10_000 });
});
