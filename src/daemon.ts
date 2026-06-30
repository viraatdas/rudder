import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";

import { processAlive } from "./git.js";
import { loadConfig, projectStateDir, registerProject, runsDir } from "./state.js";
import { DEFAULT_BOARD_PORT } from "./types.js";
import { ensureDir, readJson, writeJson } from "./util.js";
import { shortenHome } from "./util.js";
import { startBoardDaemon } from "./board/daemon.js";
import type { BoardControlMode, BoardDaemonHandle } from "./board/daemon.js";
import { RudderBus } from "./bus.js";
import { onRunTransition, scheduleTick } from "./scheduler.js";
import { renderLiveRudderMd } from "./surfaces.js";

const SCHEDULE_TICK_MS = 1000;

export type DaemonPidFile = {
  pid: number;
  port: number;
  startedAt: string;
  controlMode?: BoardControlMode;
};

export function daemonPidPath(repoRoot: string): string {
  return path.join(projectStateDir(repoRoot), "daemon.pid");
}

async function readPidFile(repoRoot: string): Promise<DaemonPidFile | null> {
  const data = await readJson<DaemonPidFile>(daemonPidPath(repoRoot));
  if (data && typeof data.pid === "number" && typeof data.port === "number") {
    return data;
  }
  return null;
}

/**
 * Phase 2 lifecycle: a single-instance guard. If a live daemon already holds
 * the pid file, returns its port without starting a second server. Otherwise
 * starts the board daemon in-process and writes the pid file.
 */
export async function ensureBoardRunning(
  repoRoot: string,
  opts?: { port?: number; open?: boolean; scheduler?: boolean },
): Promise<{
  port: number;
  url: string;
  slug: string;
  /** Deep link to THIS repo's board (`${url}/rudder/${slug}`), or the index when the
   * slug is unknown. The TUI opens this so `o` lands on the current project. */
  projectUrl: string;
  started: boolean;
  handle?: BoardDaemonHandle;
}> {
  // registerProject is idempotent and returns this repo's stable slug, which we use
  // to deep-link the browser straight to this project's board.
  const project = await registerProject(repoRoot).catch(() => null);
  const slug = project?.slug ?? "";
  const projectPath = slug ? `/rudder/${slug}` : "/rudder";
  const runScheduler = opts?.scheduler ?? true;
  const requestedMode: BoardControlMode = runScheduler ? "scheduler" : "projector";

  const existing = await readPidFile(repoRoot);
  if (existing && existing.pid !== process.pid && processAlive(existing.pid)) {
    if (!existing.controlMode) {
      throw new Error(
        `An older board process already owns this repository on port ${existing.port}; stop it once so Rudder can record its control mode safely.`,
      );
    }
    if (existing.controlMode !== requestedMode) {
      throw new Error(
        `A ${existing.controlMode} board already owns this repository on port ${existing.port}; stop it before starting ${requestedMode} mode.`,
      );
    }
    const url = `http://127.0.0.1:${existing.port}`;
    return { port: existing.port, url, slug, projectUrl: `${url}${projectPath}`, started: false };
  }

  const config = await loadConfig();
  const port = opts?.port ?? config.board?.port ?? DEFAULT_BOARD_PORT;

  // One bus shared between the scheduler (producer) and the board SSE
  // broadcaster (consumer). The daemon is the single authoritative scheduler.
  const bus = new RudderBus();
  const handle = await startBoardDaemon({
    port,
    repoRoot,
    open: opts?.open,
    bus,
    controlMode: requestedMode,
  });

  // Projector-only mode: when scheduler === false the board still serves
  // HTTP + SSE + fs.watch (so it reflects whatever is written to graph.json),
  // but the LAUNCHING scheduler tick is NOT started. The TUI starts the daemon
  // this way so the TUI remains the sole scheduler and there is no double-launch
  // (the TUI mirrors its plan into graph.json; the daemon only projects it). The
  // standalone `rudder board`/`serve` paths keep the full scheduler (default).
  const scheduler = runScheduler ? startScheduler(repoRoot, bus) : { stop: (): void => {} };

  await ensureDir(projectStateDir(repoRoot));
  await writeJson(daemonPidPath(repoRoot), {
    pid: process.pid,
    port: handle.port,
    startedAt: new Date().toISOString(),
    controlMode: requestedMode,
  });

  const cleanup = async (): Promise<void> => {
    scheduler.stop();
    await handle.close().catch(() => undefined);
    await fsp.unlink(daemonPidPath(repoRoot)).catch(() => undefined);
  };
  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.once(signal, () => {
      void cleanup().finally(() => process.exit(signal === "SIGINT" ? 130 : 143));
    });
  }
  process.once("exit", () => {
    // Best-effort synchronous-ish unlink on normal exit.
    void fsp.unlink(daemonPidPath(repoRoot)).catch(() => undefined);
  });

  console.log(`rudder board listening on ${handle.url}`);
  console.log(`  repo: ${shortenHome(repoRoot)}`);

  return {
    port: handle.port,
    url: handle.url,
    slug,
    projectUrl: `${handle.url}${projectPath}`,
    started: true,
    handle,
  };
}

/**
 * Run the dependency-aware scheduler inside the daemon. The daemon is the single
 * authoritative scheduler: a coarse 1s tick covers steady-state launches, and a
 * fs.watch on .rudder/runs edge-triggers onRunTransition the moment a run.json
 * changes (completion/failure/merge). Each run id is debounced so a burst of
 * writes coalesces into one transition. Returns a stop() to tear everything down.
 */
function startScheduler(repoRoot: string, bus: RudderBus): { stop: () => void } {
  let stopped = false;
  let tickInFlight = false;

  // Coalesce interval ticks. A tick can spend several seconds creating jj
  // workspaces; blindly enqueueing another locked tick every second starves the
  // completion transitions that need the same lock to move nodes through Review.
  const tick = (): void => {
    if (stopped || tickInFlight) return;
    tickInFlight = true;
    void scheduleTick(repoRoot, bus)
      .catch((error) => {
        console.warn(`rudder scheduler tick failed: ${error instanceof Error ? error.message : String(error)}`);
      })
      .finally(() => {
        tickInFlight = false;
      });
  };

  // Render the live RUDDER.md once on startup and tick immediately so a daemon
  // restart catches up missed transitions from disk.
  void renderLiveRudderMd(repoRoot).catch(() => undefined);
  tick();

  const interval = setInterval(() => {
    tick();
  }, SCHEDULE_TICK_MS);
  interval.unref?.();

  // Watch .rudder/runs for run.json changes; map the path back to a run id and
  // edge-trigger a transition. Per-run debounce so a write burst is one call.
  const runs = runsDir(repoRoot);
  try {
    fs.mkdirSync(runs, { recursive: true });
  } catch {
    // ignore
  }
  const runTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const trigger = (runId: string): void => {
    const existing = runTimers.get(runId);
    if (existing) {
      clearTimeout(existing);
    }
    const timer = setTimeout(() => {
      runTimers.delete(runId);
      if (!stopped) {
        void onRunTransition(repoRoot, runId, bus).catch((error) => {
          console.warn(
            `rudder scheduler transition failed for ${runId}: ${error instanceof Error ? error.message : String(error)}`,
          );
        });
      }
    }, 100);
    timer.unref?.();
    runTimers.set(runId, timer);
  };

  let watcher: fs.FSWatcher | undefined;
  try {
    watcher = fs.watch(runs, { recursive: true }, (_event, filename) => {
      if (!filename) {
        return;
      }
      const normalized = String(filename).replace(/\\/g, "/");
      // Match runs/<id>/run.json or <id>/run.json.
      const match = normalized.match(/(?:^|\/)([^/]+)\/run\.json$/);
      if (match?.[1]) {
        trigger(match[1]);
      }
    });
    watcher.on("error", () => undefined);
  } catch {
    watcher = undefined;
  }

  return {
    stop: (): void => {
      stopped = true;
      clearInterval(interval);
      for (const [, timer] of runTimers) {
        clearTimeout(timer);
      }
      runTimers.clear();
      watcher?.close();
    },
  };
}
