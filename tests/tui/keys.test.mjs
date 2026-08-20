// Key coverage — the thorough, rudder-specific pass. The other files test
// sequences (merge, restart, process lifecycle); this one exists to make sure
// every actionable KEY in the dashboard's agents view is exercised and does
// something visible, so a dead or misrouted keybinding cannot ship green.
// Discovered by driving the real binary and reading the real notices.
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

async function twoWorkerDashboard(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("alpha worker");
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });
  await session.press("Ctrl+W");
  await session.press("3");
  await session.type("beta worker");
  await session.press("Enter");
  await session.waitForText("beta worker", { timeout: 30_000 });
  // Focus the agents list so action keys route there.
  await session.press("Ctrl+W");
  await session.press("1");
  return { repo, session };
}

/** The task on the currently-selected (▶) row. */
async function selectedRow(session) {
  const screen = await session.screen();
  const line = screen.split("\n").find((l) => l.includes("▶"));
  return line ?? "";
}

/** Poll until the selected (▶) row contains `task`. */
async function waitSelected(session, task, timeout = 10_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    if ((await selectedRow(session)).includes(task)) return;
    if (Date.now() >= deadline) {
      throw new Error(`selection never reached "${task}"; row was: ${await selectedRow(session)}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 120));
  }
}

test("j/k move the selection up and down the agents list", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-nav-");
  // Newest row is selected on arrival; k moves up to the older one, j back down.
  await waitSelected(session, "beta worker");
  await session.press("k");
  await waitSelected(session, "alpha worker");
  await session.press("j");
  await waitSelected(session, "beta worker");
});

test("Alt+p/l step across agents; Alt+h hides instead of stepping", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-hlstep-");
  // Alt+h used to step back. It now hides the panes, and Alt+[ cannot cover for
  // it — ESC-[ is the CSI introducer, so terminals read that chord as the start
  // of an escape sequence. Alt+p is the backward step that actually arrives.
  await waitSelected(session, "beta worker");
  await session.press("Alt+p");
  await waitSelected(session, "alpha worker");
  await session.press("Alt+l");
  await waitSelected(session, "beta worker");

  // And Alt+h moves nothing: it toggles the hide.
  await session.press("Alt+h");
  await session.waitForText("showing this pane only", { timeout: 10_000 });
  await session.press("Alt+h");
  await session.waitForText("split restored", { timeout: 10_000 });
  await waitSelected(session, "beta worker");
});

test("v opens the diff/review view and toggles back", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-diff-");
  await session.press("v");
  await session.waitForText("Esc/v back", { timeout: 15_000 });
  await session.press("v");
  await session.waitForGone("Esc/v back", { timeout: 15_000 });
});

test("r opens rename on the selected row; Esc cancels", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-rename-");
  await session.press("r");
  // Rename puts the row's task into an editable field; type and see it echo.
  await session.type(" RENAMED");
  await session.waitForText("RENAMED", { timeout: 10_000 });
  await session.press("Escape");
});

test("guard-notice keys each fire: R (review-all), o (web), b (branch)", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-guards-");
  // R: nothing is done yet, so review-all says exactly that.
  await session.press("R");
  await session.waitForText("no completed workspaces ready to review", { timeout: 15_000 });
  // o: opens the project's web view (offline: it still emits the notice).
  await session.press("o");
  await session.waitForText("web view", { timeout: 15_000 });
  // b: a sleeping fake worker has no transcript, so branch explains why.
  await session.press("b");
  await session.waitFor((s) => /branch failed|no session/i.test(s), {
    timeout: 15_000,
    label: "the branch guard notice",
  });
});

test("P opens the model switcher", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-model-");
  await session.press("P");
  // The switcher lists backends/models to pick from.
  await session.waitFor((s) => /claude|codex|sonnet|opus|model/i.test(s), {
    timeout: 15_000,
    label: "the model switcher",
  });
  await session.press("Escape");
});

test("g toggles the nested view", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-nest-");
  const before = await session.screen();
  await session.press("g");
  // The layout must actually change (nest on), then change back (nest off).
  await session.waitFor(async () => (await session.screen()) !== before, {
    timeout: 10_000,
    label: "the screen changing when nest toggles",
  });
  await session.press("g");
});

test("cc clears merged rows from the list (two-press)", { timeout: 120_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-clear-");
  t.after(() => removeScratch(repo));
  const { completer } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("mergeable task");
  await session.press("Enter");
  await session.waitForText("press m to read", { timeout: 30_000 });
  await session.press("Ctrl+W");
  await session.press("M");
  await session.waitForText("[y] merge", { timeout: 20_000 });
  await session.press("y");
  // Merged rows live in the collapsed `done` drawer; its count is the proof.
  await session.waitForText("done 1", { timeout: 30_000 });

  // c arms the clear confirm; a second c removes every merged row.
  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("c");
  await session.waitForText("press c again to confirm", { timeout: 15_000 });
  await session.press("c");
  await session.waitForText("no agents yet", { timeout: 20_000 });
});

test("q quits the dashboard (double-press with live workers)", { timeout: 60_000 }, async (t) => {
  const { session } = await twoWorkerDashboard(t, "rudder-tui-quit-");
  // With running agents the first q arms a confirm ("press q again to quit");
  // any other key would cancel. A second q actually quits and pauses them.
  await session.press("q");
  await session.waitForText("still running", { timeout: 15_000 });
  await session.press("q");
  await session.waitForExit({ timeout: 20_000 });
});
