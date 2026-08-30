// Multi-line task drafts, driven through a real PTY with the real bytes.
//
// The unit tests assert on synthesized KeyEvents; this file asserts on what a
// terminal actually SENDS. That distinction is the whole bug: Shift+Enter only
// reaches the app as a distinct key when the terminal speaks the kitty
// protocol, and Option+Enter arrives as ESC + CR — which used to fall through
// to the plain-Enter arm and LAUNCH the half-written task.
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

// What each chord puts on the wire. The `u`-suffixed ones are the kitty
// keyboard protocol encodings rudder asks for at startup; ESC+CR is what
// Terminal.app and friends send for Option+Enter with no protocol at all.
const CHORDS = {
  "Option+Enter (ESC + CR)": "\x1b\r",
  "Shift+Enter (kitty)": "\x1b[13;2u",
  "Ctrl+Enter (kitty)": "\x1b[13;5u",
  "Ctrl+J (literal LF)": "\x0a",
};

/**
 * Just the rows inside the bottom task pane. Searching the WHOLE screen is a
 * trap: a submitted draft reappears verbatim as an agent row in the sidebar,
 * so "both halves are visible on separate rows" is true even when the chord
 * launched the task instead of breaking the line.
 */
function taskPaneRows(screen) {
  const rows = screen.split("\n");
  const start = rows.findIndex((line) => line.includes("┌ task ·"));
  if (start < 0) return [];
  const end = rows.findIndex((line, index) => index > start && line.includes("└"));
  return rows.slice(start + 1, end < 0 ? rows.length : end);
}

async function taskPane(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });
  return session;
}

for (const [name, bytes] of Object.entries(CHORDS)) {
  test(`${name} adds a line instead of running the task`, { timeout: 60_000 }, async (t) => {
    const session = await taskPane(t, "rudder-tui-nl-");

    await session.type("first half");
    await session.write(bytes);
    await session.type("second half");

    // Assert the launch FIRST. A submitted draft leaves "first half" on screen
    // as an agent row, so every line-position check below would pass while the
    // task was already running — the exact failure this file exists to catch.
    await session.waitForText("second half", { timeout: 10_000 });
    const screen = await session.screen();
    assert.ok(
      !screen.includes("working"),
      `${name} must NOT have launched an agent:\n${screen}`,
    );

    // Both halves in the TASK PANE, on separate rows.
    const rows = taskPaneRows(screen);
    const firstRow = rows.findIndex((line) => line.includes("first half"));
    const secondRow = rows.findIndex((line) => line.includes("second half"));
    assert.ok(firstRow >= 0 && secondRow >= 0, `both halves in the draft:\n${screen}`);
    assert.ok(
      secondRow > firstRow,
      `${name} must break the line, not continue it:\n${screen}`,
    );
    assert.ok(
      screen.includes("adds a line"),
      `the pane says which key sends a multi-line draft:\n${screen}`,
    );

    // And plain Enter still runs the whole two-line draft.
    await session.press("Enter");
    await session.waitForText("working", { timeout: 30_000 });
  });
}

test("a trailing backslash before Enter is the newline of last resort", { timeout: 60_000 }, async (t) => {
  const session = await taskPane(t, "rudder-tui-nlesc-");

  await session.type("first half\\");
  await session.press("Enter");
  await session.type("second half");
  await session.waitForText("second half", { timeout: 10_000 });

  const screen = await session.screen();
  assert.ok(!screen.includes("working"), `nothing launched:\n${screen}`);
  const rows = taskPaneRows(screen);
  const firstRow = rows.findIndex((line) => line.includes("first half"));
  const secondRow = rows.findIndex((line) => line.includes("second half"));
  assert.ok(firstRow >= 0 && secondRow > firstRow, `the backslash broke the line:\n${screen}`);
  assert.ok(
    !rows[firstRow].includes("\\"),
    `the escape is consumed, never sent to the agent:\n${screen}`,
  );
});

test("arrows walk a multi-line draft before they reach task history", { timeout: 60_000 }, async (t) => {
  const session = await taskPane(t, "rudder-tui-nlnav-");

  // One task in history, so a stray Up has something to clobber the draft with.
  await session.type("an older task");
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });
  await session.press("Ctrl+W");
  await session.press("3");

  await session.type("line one");
  await session.write(CHORDS["Option+Enter (ESC + CR)"]);
  await session.type("line two");

  // Up belongs to the draft here. Before this fix it swapped in "an older task".
  await session.press("Up");
  const afterUp = await session.screen();
  assert.ok(
    afterUp.includes("line one") && afterUp.includes("line two"),
    `Up moved the cursor, it did not replace the draft:\n${afterUp}`,
  );

  // A second Up has no line above it, so history takes over as it always did.
  await session.press("Up");
  await session.waitForText("an older task", { timeout: 10_000 });
});
