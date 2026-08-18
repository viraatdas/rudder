// The finished-work drawer as a SEQUENCE against the real binary. A long session
// ends with dozens of merged and failed rows; listing each one buries the two that
// are still moving. `done` and `closed` collapse to a single sidebar row that opens
// into a list + result view in the worker pane.
//
// Unit tests cover the state machine; this covers what a user actually does — merge
// a worker, watch its row leave the sidebar, open the drawer, and read the result —
// through real keystrokes on the rendered screen.
import test from "node:test";

import { journey } from "tui-integration-tests";
import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

test("a merged row collapses into the done drawer and opens to its result", { timeout: 180_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-drawer-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  const story = journey(session, "finished work collapses into a drawer");

  await story.step("a worker finishes into review", async () => {
    await session.waitForText("Type a task", { timeout: 20_000 });
    await session.type("write the done marker");
    await session.press("Enter");
    await session.waitForText("press m to read", { timeout: 30_000 });
    // Still a live row while it awaits a merge decision: review never collapses.
    await session.waitForText("review 1", { timeout: 20_000 });
  });

  await story.step("merging moves it out of the sidebar into the done drawer", async () => {
    await session.press("Ctrl+W");
    await session.press("M");
    await session.waitForText("[y] merge", { timeout: 20_000 });
    await session.press("y");
    await session.waitForText("merged 1 workspace", { timeout: 30_000 });
    // One collapsed row with a count and a closed chevron — not a per-run row.
    await session.waitForText("done 1", { timeout: 20_000 });
    const screen = await session.screen();
    if (!screen.includes("›")) {
      throw new Error(`expected the collapsed chevron on the done header:\n${screen}`);
    }
  });

  await story.step("Enter on the drawer header opens it to the run's result", async () => {
    await session.press("Ctrl+W");
    await session.press("1");
    // j walks past the live rows onto the done header, which Enter opens.
    for (let i = 0; i < 6; i += 1) {
      const screen = await session.screen();
      if (/▶ *done/.test(screen)) break;
      await session.press("j");
    }
    const beforeOpen = await session.screen();
    if (!/▶ *done/.test(beforeOpen)) {
      throw new Error(`j never landed the cursor on the done header:\n${beforeOpen}`);
    }
    await session.press("Enter");
    // The worker pane is now the drawer: titled by bucket, listing its members.
    await session.waitForText("done · 1", { timeout: 20_000 });
    await session.waitForText("What it did", { timeout: 20_000 });
    // The status coding survives the collapse: the row inside the drawer still
    // says what state the run ended in.
    await session.waitForText("merged locally", { timeout: 20_000 });
  });

  await story.step("Esc backs out and the sidebar is intact", async () => {
    await session.press("Escape");
    // A lone ESC is held briefly by the input reader while it waits to see whether
    // an escape SEQUENCE follows, so wait for the drawer to go rather than snapshot.
    await session.waitForGone("done \u00b7 1", { timeout: 20_000 });
    const screen = await session.screen();
    if (!screen.includes("done 1")) {
      throw new Error(`the collapsed header survives closing the drawer:\n${screen}`);
    }
    // "done · 1" is the drawer pane's own title; the per-run done card (which also
    // says "What it did") is what legitimately shows after backing out.
    if (screen.includes("done \u00b7 1")) {
      throw new Error(`the drawer view should be gone after Esc:\n${screen}`);
    }
    if (!screen.includes("\u203a")) {
      throw new Error(`the chevron should read closed after Esc:\n${screen}`);
    }
  });

  await story.end();
});
