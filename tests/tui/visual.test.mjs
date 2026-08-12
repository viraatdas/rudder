// Visual coverage for rudder: status color-coding. screen() reads the words
// ("working", "merged") but not their COLOR — yet color is how the dashboard
// signals state at a glance. A regression that renders every status the same
// color, or drops the color, is invisible to text assertions. These assert on
// the actual rendered fg (truecolor hex) via the framework's styleAt.
import assert from "node:assert/strict";
import test from "node:test";

import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

// Rudder's theme (native/src/theme.rs): running amber, merged green.
const RUNNING = "#b45309";
const MERGED = "#15803d";

test("a running worker's status renders in the running color", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-vis-run-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("a colored running task");
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });

  const style = await session.styleAt("working");
  assert.ok(style, "the running status label is on screen");
  assert.equal(
    String(style.fg).toLowerCase(),
    RUNNING,
    "the running status is rendered in the running (amber) color",
  );
});

test("running and merged statuses render in DIFFERENT colors", { timeout: 120_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-vis-merge-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("a task that merges");
  await session.press("Enter");
  await session.waitForText("press m to read", { timeout: 30_000 });
  await session.press("Ctrl+W");
  await session.press("M");
  await session.waitForText("[y] merge", { timeout: 20_000 });
  await session.press("y");
  await session.waitForText("merged locally", { timeout: 30_000 });

  const merged = await session.styleAt("merged locally");
  assert.ok(merged, "the merged status is on screen");
  assert.equal(
    String(merged.fg).toLowerCase(),
    MERGED,
    "a merged worker is rendered in the merged (green) color — distinct from running amber",
  );
  assert.notEqual(String(merged.fg).toLowerCase(), RUNNING, "state colors are not identical");
});
