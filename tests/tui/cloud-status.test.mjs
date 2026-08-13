// The in-VM cloud status lines. A dashboard running inside a cloud worker
// used to render "cloud offline · cloud workspace · none" about the very
// cloud it was running in: it decided "connected" by hunting for the
// laptop's RUDDER_CLOUD_TOKEN and asked for workspace status through an
// unauthenticated CLI (field report: Project_Ramsey). These tests pin the
// fixed behavior end to end — the worker session trusts its own env
// identity — and assert the VISUAL truth too: connected renders in the
// cloud teal, not the faint gray that means "offline".
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

const CLOUD_TEAL = "#0b747c"; // theme.rs CLOUD_COLOR — connected
const FAINT_GRAY = "#a6adb8"; // theme.rs FAINT — how "cloud offline" renders

test("inside a cloud worker the dashboard shows its own workspace, connected, in teal", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-cloudws-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, {
    repo,
    claudeBin: sleeper,
    env: {
      // The identity a Fly worker machine boots with. The dashboard must
      // trust these instead of hunting for laptop auth.
      RUDDER_WORKSPACE_ID: "ws1",
      RUDDER_WORKER_TOKEN: "rdrw_e2e_test_token",
    },
  });
  await session.waitForText("cloud connected", { timeout: 20_000 });
  const screen = await session.screen();
  assert.ok(
    screen.includes("cloud workspace · ws1"),
    `workspace line names the workspace:\n${screen}`,
  );
  assert.ok(
    !screen.includes("cloud offline"),
    "a worker session can never be 'offline' from its own cloud",
  );
  assert.ok(
    !screen.includes("cloud workspace · none"),
    "the workspace is the machine's own identity, never 'none'",
  );

  // The visual truth: "cloud connected" renders in the cloud teal. If the
  // words were right but painted in the faint offline gray, the user would
  // still read it as broken.
  const style = await session.styleAt("cloud connected");
  assert.ok(style, "the connected line is visible on screen");
  assert.equal(style.fg, CLOUD_TEAL, `connected renders in cloud teal, got ${style.fg}`);
  assert.notEqual(style.fg, FAINT_GRAY, "connected must not use the offline gray");

  // And nothing structurally broken anywhere on the frame.
  await session.assertIntact();
});

test("a plain local dashboard shows no cloud-worker status lines at all", { timeout: 60_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-cloudlocal-");
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });
  const screen = await session.screen();
  // Showing "cloud connected/offline" in a local session reflects saved auth,
  // not anything attached — it is worker-session-only by design.
  assert.ok(!screen.includes("cloud connected"), "no cloud status in a local session");
  assert.ok(!screen.includes("cloud offline"), "no offline banner in a local session");
  await session.assertIntact();
});
