// Worker status classification, driven end to end through the real binary.
//
// The backends report their turn state through their OWN hooks (Claude
// `--settings`, Codex `notify`, opencode plugin), which write a tiny JSON signal
// file that rudder's poll loop reads. These fakes do exactly what a real backend
// does — locate their signal file from the launch wiring and write states into it
// — so the whole path is exercised: hook -> file -> poll loop -> badge.
import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import {
  assertPrerequisites,
  launchRudder,
  removeScratch,
  scratchRepo,
} from "./helpers.mjs";

assertPrerequisites();

/**
 * A fake claude that finds its signal file the way the real one does — rudder
 * passes `--settings <dir>/<run-id>-claude.json`, and the signal file is
 * `<dir>/<run-id>.json` — then writes the states named in `script`.
 * Each script step is "<seconds> <state>"; a state of "-" deletes the file.
 */
async function signallingBackend(dir, name, script) {
  const bin = path.join(dir, "fake-bin");
  await fsp.mkdir(bin, { recursive: true });
  const file = path.join(bin, name);
  const steps = script
    .map(([delay, state]) =>
      state === "-"
        ? `sleep ${delay}\nrm -f "$sig"\n`
        : `sleep ${delay}\nprintf '{"state":"${state}"}' > "$sig"\n`,
    )
    .join("");
  await fsp.writeFile(
    file,
    `#!/bin/sh
prev=""
settings=""
for a in "$@"; do
  [ "$prev" = "--settings" ] && settings="$a"
  prev="$a"
done
sig=$(printf '%s' "$settings" | sed 's/-claude\\.json$/.json/')
printf 'starting the task\\n'
${steps}printf 'still here\\n'
sleep 300
`,
  );
  await fsp.chmod(file, 0o755);
  return file;
}

async function runningWorker(t, prefix, backend) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const session = await launchRudder(t, { repo, claudeBin: backend });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("do the thing");
  await session.press("Enter");
  return session;
}

test("answering a waiting agent actually clears its badge", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-waitclear-");
  t.after(() => removeScratch(repo));
  // Pauses to ask, then goes quiet — exactly the case that used to latch on
  // forever, because the signal file was re-read every tick and never consumed.
  const backend = await signallingBackend(repo, "claude-asker", [[2, "input"]]);
  const session = await launchRudder(t, { repo, claudeBin: backend });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("do the thing");
  await session.press("Enter");

  await session.waitForText("needs input", { timeout: 30_000 });

  // The wait must SURVIVE across polls with the signal file already consumed:
  // the prompt scrolls out of the pane, so the latch is what remembers.
  await new Promise((r) => setTimeout(r, 2500));
  let screen = await session.screen();
  assert.ok(
    screen.includes("needs input"),
    `the wait outlives the one-shot signal:\n${screen}`,
  );

  // Answer it, in the worker pane, the way a human would.
  await session.press("Ctrl+W");
  await session.press("2");
  await session.type("use the second approach");
  await session.press("Enter");

  await session.waitFor((s) => !s.includes("needs input"), {
    label: "the badge clears when you answer",
    timeout: 15_000,
  });

  // And it STAYS cleared. This is the whole bug: it used to come back on the
  // very next poll tick, because nothing consumed the signal file.
  await new Promise((r) => setTimeout(r, 3000));
  screen = await session.screen();
  assert.ok(
    !screen.includes("needs input"),
    `answering ends the wait for good:\n${screen}`,
  );
});

test("a permission block reads as needing permission, not as a question", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-perm-");
  t.after(() => removeScratch(repo));
  // Claude's `permission_request` notification matcher — previously unwired, so
  // the commonest reason an agent stops dead had no native signal at all.
  const backend = await signallingBackend(repo, "claude-blocked", [[2, "permission"]]);
  const session = await launchRudder(t, { repo, claudeBin: backend });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("do the thing");
  await session.press("Enter");

  await session.waitForText("needs permission", { timeout: 30_000 });
  const screen = await session.screen();
  assert.ok(
    !screen.includes("needs input"),
    `a decision and a question are different states:\n${screen}`,
  );
});

test("the backend saying it resumed lifts the wait with no keystroke in rudder", { timeout: 90_000 }, async (t) => {
  const repo = await scratchRepo("rudder-tui-resume-");
  t.after(() => removeScratch(repo));
  // The human answers inside the AGENT'S own UI, so rudder sees no keystroke it
  // can attribute. Claude's UserPromptSubmit / opencode's permission.replied say
  // so directly — the counterpart the signal protocol never had.
  const backend = await signallingBackend(repo, "claude-resumer", [
    [2, "input"],
    [3, "working"],
  ]);
  const session = await launchRudder(t, { repo, claudeBin: backend });
  await session.waitForText("Type a task", { timeout: 20_000 });
  await session.type("do the thing");
  await session.press("Enter");

  await session.waitForText("needs input", { timeout: 30_000 });
  await session.waitFor((s) => !s.includes("needs input"), {
    label: "the resume signal lifts the wait",
    timeout: 20_000,
  });
});
