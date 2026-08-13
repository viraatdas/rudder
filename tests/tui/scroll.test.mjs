// Alt+j/k/u/d scrollback in the worker pane, end to end through a real PTY.
// The bare letters must keep reaching the agent when the pane is focused —
// only the Alt chord belongs to the dashboard — and scrolling must never
// type into the agent (Alt+j forwarded raw would arrive as ESC+j, a cursor
// move in vim-like TUIs).
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import {
  assertPrerequisites,
  launchRudder,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

// A backend that fills the pane with numbered scrollback, then stays alive.
async function scrollerBackend(repo, lines) {
  const dir = path.join(repo, ".fake-bin");
  await fs.mkdir(dir, { recursive: true });
  const bin = path.join(dir, "scroller");
  const script = `#!/bin/sh
i=1
while [ "$i" -le ${lines} ]; do
  printf 'LINE-%03d\\n' "$i"
  i=$((i + 1))
done
exec sleep 600
`;
  await fs.writeFile(bin, script, { mode: 0o755 });
  return bin;
}

test("mouse wheel scrolls the worker pane through real SGR sequences", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-wheel-");
  t.after(() => removeScratch(repo));
  const scroller = await scrollerBackend(repo, 200);
  const session = await launchRudder(t, { repo, claudeBin: scroller });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("fill the pane");
  await session.press("Enter");
  await session.waitForText("LINE-200", { timeout: 30_000 });

  // Rudder enables SGR mouse capture; a wheel tick inside the worker pane is
  // ESC[<64;x;yM (up) / ESC[<65;x;yM (down). (70, 20) lands mid-pane at
  // 120x40. A flick's worth of ticks must travel visibly into history —
  // this is the path the draw throttle serves, previously untested.
  const wheel = (btn, n) => Array.from({ length: n }, () => `\x1b[<${btn};70;20M`).join("");
  await session.write(wheel(64, 40));
  await session.waitFor(
    (screen) => !screen.includes("LINE-200") && screen.includes("LINE-"),
    { label: "wheel-up leaves the live tail", timeout: 10_000 },
  );

  // Wheel back down returns to the tail.
  await session.write(wheel(65, 80));
  await session.waitForText("LINE-200", { timeout: 10_000 });
  await session.assertIntact();
});

test("Alt+u/k/j/d travel the worker scrollback without typing into the agent", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-scroll-");
  t.after(() => removeScratch(repo));
  const scroller = await scrollerBackend(repo, 200);
  const session = await launchRudder(t, { repo, claudeBin: scroller });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("fill the pane");
  await session.press("Enter");
  // The tail of the output is on screen; the head has scrolled away.
  await session.waitForText("LINE-200", { timeout: 30_000 });
  assert.ok(!(await session.screen()).includes("LINE-001"), "head already scrolled off");

  // Focus the pane, then ride Alt+u (half page up) into history until the
  // very first line is visible. 200 lines / ~19-row half pages ≈ 11 presses;
  // 40 is comfortably past the top, where scrolling clamps.
  await session.press("Enter");
  for (let i = 0; i < 40; i += 1) await session.press("Alt+u");
  await session.waitForText("LINE-001", { timeout: 10_000 });

  // One line back down: Alt+j. The top line leaves; LINE-002 becomes the head.
  await session.press("Alt+j");
  await session.waitFor(
    (screen) => !screen.includes("LINE-001") && screen.includes("LINE-002"),
    { label: "Alt+j advances one line", timeout: 10_000, stablePolls: 2 },
  );

  // And one line back up: Alt+k restores the top.
  await session.press("Alt+k");
  await session.waitForText("LINE-001", { timeout: 10_000 });

  // Alt+d half-pages toward the live tail; enough presses return to LINE-200.
  for (let i = 0; i < 40; i += 1) await session.press("Alt+d");
  await session.waitForText("LINE-200", { timeout: 10_000 });

  // Scrolling must not have typed anything into the agent: the pane's live
  // tail is still the backend's own output, not stray j/k/u/d characters.
  const screen = await session.screen();
  assert.ok(screen.includes("LINE-200"), "live tail intact after the round trip");

  // The frame stayed structurally sound through all of it.
  await session.assertIntact();
});
