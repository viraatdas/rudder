// Production readiness: responsiveness and visual integrity, not just content.
// A dashboard that renders the right text but stutters, or draws a torn frame,
// is not shippable. Measured budgets (local keystroke latency was ~4ms, boot
// ~700ms) with generous CI headroom — these catch a regression into sluggish
// or corrupt, not micro-jitter.
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

async function dashboard(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const { sleeper } = await fakeBackends(repo);
  const session = await launchRudder(t, { repo, claudeBin: sleeper });
  await session.waitForText("Type a task", { timeout: 20_000 });
  return session;
}

test("keystrokes reflect on screen within the responsiveness budget", { timeout: 60_000 }, async (t) => {
  const session = await dashboard(t, "rudder-tui-lat-key-");
  // Latency is measured as a MEDIAN of samples, not a single reading: one
  // outlier on a loaded box must not fail the test, but a genuine freeze
  // (every sample slow) still does. Each sample types one more character and
  // times how long the growing string takes to appear. Local median ~4ms.
  const chars = "abcdefghij".split("");
  const samples = [];
  let typed = "";
  for (const ch of chars) {
    typed += ch;
    const target = typed;
    samples.push(
      await session.timeToScreen(
        () => session.type(ch),
        (screen) => screen.includes(target),
        { timeout: 5_000, label: "keystroke echo" },
      ),
    );
  }
  samples.sort((a, b) => a - b);
  const median = samples[Math.floor(samples.length / 2)];
  // Median well under a third of a second: a strong responsiveness guarantee
  // with wide CI headroom, immune to a lone spike.
  assert.ok(median < 300, `median keystroke latency ${median}ms over ${samples.length} samples (budget 300ms)`);
  // No sample may be pathologically slow — that would be a real freeze, not jitter.
  assert.ok(samples[samples.length - 1] < 3000, `worst keystroke ${samples[samples.length - 1]}ms (freeze guard 3000ms)`);
});

test("a typed task produces a worker row within budget", { timeout: 60_000 }, async (t) => {
  const session = await dashboard(t, "rudder-tui-lat-task-");
  const ms = await session.timeToScreen(
    async () => {
      await session.type("do the work");
      await session.press("Enter");
    },
    (screen) => screen.includes("working"),
    { timeout: 30_000, label: "worker row" },
  );
  assert.ok(ms < 10_000, `worker row appeared in ${ms}ms`);
});

test("the dashboard renders intact through boot, input, and resize", { timeout: 60_000 }, async (t) => {
  const session = await dashboard(t, "rudder-tui-intact-");
  // Nothing broken at boot.
  await session.assertIntact();

  // Nothing broken after starting a worker (panes repaint).
  await session.type("build a thing");
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });
  await session.assertIntact();

  // Nothing broken after a reflow — a resize is where torn borders show up.
  await session.resize(80, 24);
  await session.waitForText("working", { timeout: 10_000 });
  assert.deepEqual(
    await session.visualIssues(),
    [],
    "no visual corruption after resize",
  );

  // And back at the original size.
  await session.resize(120, 40);
  await session.assertIntact();
});
