// Process lifecycle — the highest-consequence, screen-invisible class:
// deleting or stopping an ongoing agent session must reap the WHOLE process
// group (the agent AND its subprocesses), leaving no orphans. Rudder does
// this via a negative-pgid signal in TerminalPane::terminate_child; these
// tests hold that contract by tracking REAL pids, because a leaked claude /
// MCP / node process is invisible to every assertion about the screen.
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  assertPrerequisites,
  launchRudder,
  leakDetectorBackend,
  pidAlive,
  readPid,
  removeScratch,
  scratchRepo,
  waitPidGone,
} from "./helpers.mjs";

assertPrerequisites();

async function scratchWithLeakDetector(t, prefix) {
  const repo = await scratchRepo(prefix);
  t.after(() => removeScratch(repo));
  const pidDir = await fsp.mkdtemp(path.join(os.tmpdir(), `${prefix}pids-`));
  t.after(() => fsp.rm(pidDir, { recursive: true, force: true }));
  const claudeBin = await leakDetectorBackend(repo, pidDir);
  const session = await launchRudder(t, { repo, claudeBin });
  await session.waitForText("Type a task", { timeout: 20_000 });
  return { session, pidDir };
}

async function startOngoingSession(session, pidDir, task) {
  await session.type(task);
  await session.press("Enter");
  await session.waitForText("working", { timeout: 30_000 });
  const leader = await readPid(pidDir, "leader.pid");
  const descendant = await readPid(pidDir, "descendant.pid");
  if (!pidAlive(leader) || !pidAlive(descendant)) {
    throw new Error("fixture never got both processes alive");
  }
  return { leader, descendant };
}

test("dd on an ongoing session reaps the whole process group (no orphans)", { timeout: 90_000 }, async (t) => {
  const { session, pidDir } = await scratchWithLeakDetector(t, "rudder-tui-ddproc-");
  const { leader, descendant } = await startOngoingSession(
    session,
    pidDir,
    "an ongoing session with a subprocess",
  );

  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("d");
  await session.waitForText("press d again", { timeout: 20_000 });
  await session.press("d");
  await session.waitForText("no agents yet", { timeout: 20_000 });

  // The row is gone from the screen; the processes must be gone from the OS.
  // The descendant is the orphan canary: a leader-only kill (pgid vs -pgid)
  // leaves it running with nothing pointing at it.
  await waitPidGone(leader, "session leader");
  await waitPidGone(descendant, "session subprocess");
});

test("x (stop) on an ongoing session also reaps its subprocesses", { timeout: 90_000 }, async (t) => {
  const { session, pidDir } = await scratchWithLeakDetector(t, "rudder-tui-xproc-");
  const { leader, descendant } = await startOngoingSession(
    session,
    pidDir,
    "another ongoing session with a subprocess",
  );

  await session.press("Ctrl+W");
  await session.press("1");
  await session.press("x");
  // Stop keeps the row (undoable) but must end the processes.
  await waitPidGone(leader, "stopped session leader");
  await waitPidGone(descendant, "stopped session subprocess");
});
