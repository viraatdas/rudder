// Screen-level end-to-end tests: the REAL rudder-native binary in a real PTY,
// real keystroke bytes, assertions on what is actually visible. Each test maps
// to a bug that shipped — see the plan in the repo history. The agent backends
// are fake scripts (RUDDER_CLAUDE_BIN); everything else is the production path:
// main(), raw mode, the alt screen, input threads, jj workspaces, persistence.
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import {
  assertPrerequisites,
  fakeBackends,
  launchRudder,
  plantTranscript,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

/** Poll for a file the way waits poll the screen: no fixed sleeps. */
async function waitForFile(file, timeout) {
  const deadline = Date.now() + timeout;
  for (;;) {
    try {
      await fsp.access(file);
      return;
    } catch {
      if (Date.now() >= deadline) {
        throw new Error(`timed out waiting for ${file} to exist`);
      }
      await new Promise((resolve) => setTimeout(resolve, 150));
    }
  }
}

async function scratch(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const backends = await fakeBackends(repo);
  return { repo, ...backends };
}

test("two /plan orchestrators survive a process restart", { timeout: 120_000 }, async (t) => {
  // THE restart test. The shipped regression: concurrent plans collapsed to one
  // after a dashboard restart, while 502 unit tests stayed green because none
  // of them ever reloaded state from disk.
  const { repo, sleeper } = await scratch(t, "rudder-tui-plans-");
  let session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("/plan alpha objective");
  await session.press("Enter");
  await session.waitForText("alpha objective", { timeout: 30_000 });

  // /plan moves focus into the orchestrator's chat pane; a second /plan typed
  // there would be a MESSAGE to the first orchestrator. Ctrl+W is the leader,
  // 3 focuses the task input — the same keys a user would press.
  await session.press("Ctrl+W");
  await session.press("3");
  await session.type("/plan beta objective");
  await session.press("Enter");
  await session.waitForText("beta objective", { timeout: 30_000 });

  await session.kill();
  session = await session.respawn();
  t.after(() => session.close());

  // Both plans must come back. Before the fix, reload kept exactly one.
  await session.waitForText("alpha objective", { timeout: 30_000 });
  await session.waitForText("beta objective", { timeout: 30_000 });
});

test("a typed task becomes an isolated worker on screen", { timeout: 60_000 }, async (t) => {
  const { repo, sleeper } = await scratch(t, "rudder-tui-worker-");
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("add a parser test");
  await session.press("Enter");

  await session.waitForText("isolated worker running", { timeout: 30_000 });
  await session.waitForText("working", { timeout: 30_000 });
  const screen = await session.screen();
  assert.ok(!screen.includes("no agents yet"), `agents list still empty:\n${screen}`);
});

test("/main runs in the checkout and says so", { timeout: 60_000 }, async (t) => {
  const { repo, sleeper } = await scratch(t, "rudder-tui-main-");
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("/main tidy the readme");
  await session.press("Enter");

  await session.waitForText("tidy the readme", { timeout: 30_000 });
  // The row must be visibly a MAIN row, not an isolated worker: no workspace,
  // nothing to merge.
  await session.waitFor(
    (screen) => screen.includes("main") && !screen.includes("isolated worker running"),
    { timeout: 30_000, label: "a main-checkout row without a worker notice" },
  );
});

test("merge gate: first m shows the diff, second m merges", { timeout: 120_000 }, async (t) => {
  const { repo, completer } = await scratch(t, "rudder-tui-gate-");
  const session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("write the done marker");
  await session.press("Enter");
  // The fake agent writes DONE.txt into its workspace and exits 0. Wait for
  // the row's own done-state affordance — a bare "done" would match the
  // "Done when:" objective text while the agent is still running.
  await session.waitForText("press m to read the d", { timeout: 60_000 });

  // Ctrl+W is the leader; m is its merge binding. First press on unreviewed
  // work must open the diff and demand a second press, not merge.
  await session.press("Ctrl+W");
  await session.press("m");
  await session.waitForText("read the diff", { timeout: 20_000 });

  await session.press("Ctrl+W");
  await session.press("m");
  // Second m does not merge yet: it opens the merge confirmation.
  await session.waitForText("[y] merge", { timeout: 20_000 });
  await session.press("y");
  // The row's own state stamp — a bare "merged" would match the ever-present
  // "cc clear merged" legend and pass without any merge happening.
  await session.waitForText("● merged", { timeout: 30_000 });
  // The screen can say what it likes; the merged file must be in the checkout.
  await waitForFile(path.join(repo, "DONE.txt"), 20_000);
});

test("/resume shows where each conversation ran, and --here stays in the checkout", { timeout: 90_000 }, async (t) => {
  const { repo, sleeper } = await scratch(t, "rudder-tui-resume-");
  // A fixture HOME: two planted conversations, one from the repo root and one
  // from a subfolder. The picker must say which is which — shipped in 2.14.2
  // with unit tests only, never driven through the real screen until now.
  const home = path.join(repo, ".tui-fixture-home");
  await fsp.mkdir(path.join(repo, "site"), { recursive: true });
  await plantTranscript(home, repo, "11111111-1111-1111-1111-111111111111", "rewrite the auth middleware");
  await plantTranscript(home, path.join(repo, "site"), "22222222-2222-2222-2222-222222222222", "fix the sidebar spacing");

  const session = await launchRudder(t, { repo, claudeBin: sleeper, home });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("/resume ");
  await session.waitForText("rewrite the auth middleware", { timeout: 20_000 });
  await session.waitForText("fix the sidebar spacing", { timeout: 20_000 });
  // Origin labels, in the palette row's detail format ("claude · … · main · …").
  // Bare "main" would match the task-pane hint text and prove nothing.
  await session.waitForText("· main ·", { timeout: 20_000 });
  await session.waitForText("· site ·", { timeout: 20_000 });

  // --here adopts the conversation into the MAIN checkout, not a workspace.
  await session.type("11111111-1111-1111-1111-111111111111 --here keep going");
  await session.press("Enter");
  await session.waitForText("in the main checkout", { timeout: 30_000 });
});

test("dd deletes a running /main row instead of dead-ending", { timeout: 60_000 }, async (t) => {
  // The reported symptom: "sometimes dd doesn't work, for main". Root cause
  // was a three-layer dead end — dd on a live main row said "stop it with x",
  // the x handler excluded main rows, and stop_agent_at refused them again
  // underneath. The row was undeletable while running, with advice that could
  // not be followed.
  const { repo, sleeper } = await scratch(t, "rudder-tui-dd-main-");
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("/main long running chore");
  await session.press("Enter");
  await session.waitForText("long running chore", { timeout: 30_000 });

  // Focus the agents pane, then the two-press delete on the LIVE row.
  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("d");
  await session.waitForText("press d again to stop it and delete", { timeout: 20_000 });
  await session.press("d");

  // A POSITIVE post-state, not waitForGone: an absence assertion can match a
  // mid-repaint frame where the row text is transiently blank (observed — it
  // green-lit a build where deletion was provably refused). Deleting the only
  // row brings the empty-list placeholder back, which only exists afterwards.
  await session.waitForText("no agents yet", { timeout: 20_000 });
});

test("recorded work survives renaming the repo directory", { timeout: 120_000 }, async (t) => {
  // The incident, verbatim: the user ran `mv aws-v2 libra` and every recorded
  // row died with "agent process exited (exit 1)" because run.json stored
  // absolute paths under the old name. After the heal, a rename is invisible:
  // the respawned dashboard rebases recorded paths onto the root it actually
  // finds them in, and the work still merges.
  const { repo, completer } = await scratch(t, "rudder-tui-rename-");
  let session = await launchRudder(t, { repo, claudeBin: completer });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.type("write the done marker");
  await session.press("Enter");
  await session.waitForText("press m to read the d", { timeout: 60_000 });

  await session.kill();
  await session.close();

  // The rename. Everything inside (worktrees, .rudder state, jj) moves along.
  const renamed = `${repo}-renamed`;
  await fsp.rename(repo, renamed);
  t.after(() => removeScratch(renamed));

  session = await launchRudder(t, { repo: renamed, claudeBin: path.join(renamed, "fake-bin", "claude-completer") });
  t.after(() => session.close());
  await session.waitForText("write the done marker", { timeout: 30_000 });

  // The row must not be the incident's failure shape.
  const screen = await session.screen();
  assert.ok(!screen.includes("agent process exited"), `row died after rename:\n${screen}`);

  // And the recorded workspace still merges from its new home.
  await session.press("Ctrl+W");
  await session.press("m");
  await session.waitForText("read the diff", { timeout: 20_000 });
  await session.press("Ctrl+W");
  await session.press("m");
  await session.waitForText("[y] merge", { timeout: 20_000 });
  await session.press("y");
  await session.waitForText("● merged", { timeout: 30_000 });
  await waitForFile(path.join(renamed, "DONE.txt"), 20_000);
});

test("resize reflows without a panic and stays interactive", { timeout: 60_000 }, async (t) => {
  const { repo, sleeper } = await scratch(t, "rudder-tui-resize-");
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });

  await session.resize(60, 20);
  // Still alive and still drawing at the new size.
  await session.waitForText("task", { timeout: 20_000 });

  await session.resize(120, 40);
  await session.type("still alive after two reflows");
  await session.waitForText("still alive after two reflows", { timeout: 20_000 });
});
