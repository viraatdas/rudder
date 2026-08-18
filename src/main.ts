import { spawn } from "node:child_process";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { authStoreExists, runDoctor, runOnboard } from "./auth.js";
import { codexLaunchEnv } from "./codex-binary.js";
import { runCloudCommand } from "./cloud.js";
import { printContextAudit } from "./context-audit.js";
import { currentBranch, findRepoRoot } from "./git.js";
import { runGc } from "./gc.js";
import { capture, pendingTelemetryNotice, projectHash, setTelemetryEnabled, telemetryEnabled, TELEMETRY_NOTICE } from "./analytics.js";
import { submitFeedback } from "./feedback.js";
import type { FeedbackContext } from "./feedback.js";
import { dashboardRoot, runHandoff } from "./handoff.js";
import { ensureBoardRunning } from "./daemon.js";
import {
  createNodeWorkspace,
  currentJjChangeId,
  currentOpId,
  ensureColocated,
  ensureJj,
  undoLast,
  undoToOp,
} from "./jj.js";
import { graphPath, mirrorPlanIntoGraph, updateGraph } from "./graph.js";
import type { MirrorPayload } from "./graph.js";
import { buildFanoutDag, gateDecision, planTask, scaffoldPlan } from "./planner.js";
import { backfillLlmTaskSummaries, listRuns, popUndoEntry, projectStateDir, registerProject, runsDir } from "./state.js";
import { discoverModelOptions } from "./models.js";
import { resolveNativeBinaryPath } from "./native-binary.js";
import {
  cleanupRuns,
  deleteRun,
  listProjectRuns,
  mergeRun,
  printLogs,
  startRun,
  statusRuns,
  stopRun,
  syncRun,
  watchRun,
  workerRun,
} from "./run-manager.js";
import { appendCompletionNote, appendDecision, appendSharedContext, parseCompletionNoteArg, SHARED_CONTEXT_FILE, syncSharedContextToWorkspaces, writeCompletionNoteFile } from "./surfaces.js";
import type { CompletionNote } from "./surfaces.js";
import type { BackendId } from "./types.js";
import { commandExists, isTty, MissingToolError, newRunId, runCommand } from "./util.js";
import { autoUpdateAndRerunIfNeeded, getUpdateAvailable } from "./version-check.js";

type Parsed = {
  command?: string;
  args: string[];
  flags: {
    json?: boolean;
    quiet?: boolean;
    detach?: boolean;
    watch?: boolean;
    follow?: boolean;
    workspace?: boolean;
    queue?: boolean;
    allowDirty?: boolean;
    dryRun?: boolean;
    force?: boolean;
    nonInteractive?: boolean;
    model?: string;
    backend?: BackendId;
    cwd?: string;
    repo?: string;
    run?: string;
    attempt?: string;
    task?: string;
    node?: string;
    base?: string;
    help?: boolean;
    version?: boolean;
    homePaths?: string[];
    sshHost?: string;
    port?: number;
    open?: boolean;
    n?: number;
    /** `rudder feedback --context <file>`: the dashboard's on-screen context. */
    contextFile?: string;
    /** `--no-issue` opts out of filing a GitHub issue for a feedback report. */
    issue?: boolean;
    /** `rudder __event <name> --props '{json}'` (hidden telemetry emitter). */
    props?: string;
  };
};

export async function main(): Promise<void> {
  const parsed = parseArgs(process.argv.slice(2));
  if (shouldAutoUpdateForCommand(parsed.command) && await autoUpdateAndRerunIfNeeded(process.argv.slice(2))) {
    return;
  }
  // One-time disclosure, before the dashboard takes over the screen. Printing it
  // here (rather than burying it in docs) is the deal: telemetry is on, and the
  // user is told what it is and how to stop it the first time they run anything.
  if (!parsed.command?.startsWith("__")) {
    const notice = await pendingTelemetryNotice();
    if (notice) {
      console.log(notice);
    }
  }
  if (parsed.flags.version || parsed.command === "version") {
    const current = await packageVersion();
    console.log(current);
    const update = await getUpdateAvailable().catch(() => null);
    if (update && update.latest !== current) {
      console.log(
        `update available: ${update.latest} (current ${current})`,
      );
      console.log("  npm i -g @viraatdas/rudder");
    }
    return;
  }
  if (parsed.flags.cwd) {
    process.chdir(parsed.flags.cwd);
  }
  if (!parsed.command || parsed.flags.help) {
    if (!parsed.command && parsed.args.length > 0) {
      await maybeOnboard();
      await startRun({
        task: parsed.args.join(" "),
        backend: parsed.flags.backend,
        model: parsed.flags.model,
        detach: parsed.flags.detach,
        workspace: parsed.flags.workspace,
        queue: parsed.flags.queue,
        json: parsed.flags.json,
        view: "shell",
      });
      return;
    }
    if (!parsed.command && isTty() && !parsed.flags.help) {
      await maybeOnboard();
      await openDashboard(parsed);
      return;
    }
    printHelp();
    return;
  }

  switch (parsed.command) {
    case "tmux":
      throw new Error("The tmux dashboard has been removed. Run `rudder` or `rudder dashboard` to open the native dashboard.");
    case "dashboard":
      await maybeOnboard();
      await openDashboard(parsed);
      return;
    case "restart":
      await maybeOnboard();
      await resetLocalRudderSession(parsed);
      await openDashboard(parsed);
      return;
    case "mouse-test":
      await runNativeCommand(["mouse-test", ...parsed.args]);
      return;
    case "tui":
    case "shell":
    case "interactive":
    case "legacy-shell":
      throw new Error("Legacy interactive shells have been removed. Run `rudder` or `rudder dashboard` to open the native dashboard.");
    case "__worker": {
      const repo = parsed.flags.repo;
      const run = parsed.flags.run;
      if (!repo || !run) {
        throw new Error("__worker requires --repo and --run");
      }
      await workerRun(repo, run, parsed.flags.attempt);
      return;
    }
    case "__launch-node": {
      await runLaunchNode(parsed);
      return;
    }
    case "__graph-mirror": {
      await runGraphMirror(parsed);
      return;
    }
    case "__refresh-models": {
      // Spawned detached by the native TUI when the /model picker opens on a
      // stale models.dev cache, so a long-lived dashboard session picks up
      // newly released models without a restart. The picker re-reads the cache
      // file on every render, so results appear as soon as this lands.
      await refreshModelCache();
      return;
    }
    case "onboard":
      await runOnboard({
        nonInteractive: parsed.flags.nonInteractive,
        json: parsed.flags.json,
      });
      return;
    case "doctor":
      await runDoctor({ json: parsed.flags.json });
      return;
    case "login":
      await runCloudCommand("cloud", ["login", ...parsed.args], {
        json: parsed.flags.json,
        homePaths: parsed.flags.homePaths,
        sshHost: parsed.flags.sshHost,
      });
      return;
    case "cloud":
    case "sail":
      await runCloudCommand(parsed.command, parsed.args, {
        json: parsed.flags.json,
        homePaths: parsed.flags.homePaths,
        sshHost: parsed.flags.sshHost,
      });
      return;
    case "run": {
      await maybeOnboard();
      const task = parsed.args.join(" ").trim();
      if (!task) {
        throw new Error("Missing task. Usage: rudder run \"fix the tests\"");
      }
      await startRun({
        task,
        backend: parsed.flags.backend,
        model: parsed.flags.model,
        detach: parsed.flags.detach,
        workspace: parsed.flags.workspace,
        queue: parsed.flags.queue,
        json: parsed.flags.json,
        view: "shell",
      });
      return;
    }
    case "claude":
    case "codex": {
      if (!commandExists(parsed.command)) {
        throw new MissingToolError(parsed.command);
      }
      await maybeOnboard();
      const task = parsed.args.join(" ").trim();
      if (!task) {
        throw new Error(`Missing task. Usage: rudder ${parsed.command} "fix the tests"`);
      }
      await startRun({
        task,
        backend: parsed.command,
        model: parsed.flags.model,
        detach: parsed.flags.detach,
        workspace: parsed.flags.workspace,
        queue: parsed.flags.queue,
        json: parsed.flags.json,
        view: "shell",
      });
      return;
    }
    case "acpx": {
      if (!commandExists("acpx")) {
        throw new MissingToolError("acpx");
      }
      await maybeOnboard();
      const args = parsed.args[0] === "codex" ? parsed.args.slice(1) : parsed.args;
      const task = args.join(" ").trim();
      if (!task) {
        throw new Error('Missing task. Usage: rudder acpx codex "fix the tests"');
      }
      await startRun({
        task,
        backend: "acpx",
        model: parsed.flags.model,
        detach: parsed.flags.detach,
        workspace: parsed.flags.workspace,
        queue: parsed.flags.queue,
        json: parsed.flags.json,
        view: "shell",
      });
      return;
    }
    case "watch":
      await watchRun({ runId: parsed.args[0], follow: true });
      return;
    case "logs":
      await printLogs(parsed.args[0], Boolean(parsed.flags.follow));
      return;
    case "status":
      await statusRuns({ json: parsed.flags.json });
      return;
    case "runs":
      await listProjectRuns({ json: parsed.flags.json });
      return;
    case "stop": {
      const run = parsed.args[0];
      if (!run) {
        throw new Error("Missing run id.");
      }
      await stopRun(run);
      return;
    }
    case "delete": {
      const run = parsed.args[0];
      if (!run) {
        throw new Error("Missing run id.");
      }
      await deleteRun(run, { force: Boolean(parsed.flags.force) });
      return;
    }
    case "merge": {
      const run = parsed.args[0];
      if (!run) {
        throw new Error("Missing run id.");
      }
      await mergeRun(run, Boolean(parsed.flags.allowDirty));
      return;
    }
    case "sync":
      await syncRun(parsed.args[0]);
      return;
    case "cleanup":
      await cleanupRuns(Boolean(parsed.flags.force));
      return;
    case "gc":
      await runGc({ dryRun: Boolean(parsed.flags.dryRun) });
      return;
    case "improve": {
      const { runImprove } = await import("./improve/index.js");
      await runImprove(parsed);
      return;
    }
    case "board":
    case "serve": {
      await runBoard(parsed);
      return;
    }
    case "undo": {
      await runUndo(parsed.args[0]);
      return;
    }
    case "remember": {
      const insight = parsed.args.join(" ").trim();
      if (!insight) {
        throw new Error('Missing insight. Usage: rudder remember "the parser owns token budget"');
      }
      await runRemember(insight);
      return;
    }
    case "share": {
      const text = parsed.args.join(" ").trim();
      if (!text) {
        throw new Error('Missing shared context. Usage: rudder share "APIFY_TOKEN=..."');
      }
      await runShare(text);
      return;
    }
    case "done": {
      // Worker end-of-task report (summary + interfaces + recommended follow-ups).
      await runDone(parsed);
      return;
    }
    case "plan": {
      await maybeOnboard();
      const task = parsed.args.join(" ").trim();
      if (!task) {
        throw new Error('Missing task. Usage: rudder plan "split the parser work"');
      }
      await runPlan(task);
      return;
    }
    case "fanout": {
      await maybeOnboard();
      const task = parsed.args.join(" ").trim();
      if (!task) {
        throw new Error('Missing task. Usage: rudder fanout "implement the cache" [--n 3]');
      }
      await runFanout(task, parsed.flags.n, parsed.flags.backend);
      return;
    }
    case "context": {
      await runContextCommand(parsed);
      return;
    }
    case "telemetry": {
      await runTelemetryCommand(parsed.args[0]);
      return;
    }
    case "feedback": {
      await runFeedbackCommand(parsed);
      return;
    }
    case "__event": {
      // Hidden: the native dashboard shells out to this so one telemetry
      // implementation serves both halves of Rudder. Fire-and-forget by design —
      // it must never fail loudly or delay the caller.
      await runEventCommand(parsed);
      return;
    }
    case "handoff": {
      // Run from inside a live claude/codex chat: queues THIS conversation for
      // the dashboard, which forks it into an agent pane with all of its context.
      await runHandoff(parsed.args, parsed.flags.backend);
      return;
    }
    default: {
      // INTERNAL calls (`__event`, `__worker`, …) come from the native dashboard,
      // which finds `rudder` on PATH — and that can be a DIFFERENT version than
      // the binary calling it. Falling through here would treat the unknown
      // internal command as a task and spawn a real agent named
      // "__event dashboard_opened …". Version skew must be inert, not expensive.
      if (isInternalCommand(parsed.command)) {
        return;
      }
      await maybeOnboard();
      await startRun({
        task: [parsed.command, ...parsed.args].join(" "),
        backend: parsed.flags.backend,
        model: parsed.flags.model,
        detach: parsed.flags.detach,
        workspace: parsed.flags.workspace,
        queue: parsed.flags.queue,
        json: parsed.flags.json,
        view: "shell",
      });
    }
  }
}

/** Hidden cross-process entry points; never user tasks. */
export function isInternalCommand(command: string | undefined): boolean {
  return Boolean(command?.startsWith("__"));
}

function shouldAutoUpdateForCommand(command: string | undefined): boolean {
  if (command?.startsWith("__")) {
    return false;
  }
  // `rudder done` is invoked by worker agents inside a live Rudder session; updating
  // the global package in that callback can desync the running dashboard from its
  // worker surface.
  return command !== "done";
}

function parseArgs(argv: string[]): Parsed {
  const parsed: Parsed = { args: [], flags: {} };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i] ?? "";
    if (arg === "--") {
      parsed.args.push(...argv.slice(i + 1));
      break;
    }
    if (arg === "--help" || arg === "-h") {
      parsed.flags.help = true;
      continue;
    }
    if (arg === "--version" || arg === "-v") {
      parsed.flags.version = true;
      continue;
    }
    if (arg === "--json") {
      parsed.flags.json = true;
      continue;
    }
    if (arg === "--quiet" || arg === "-q") {
      parsed.flags.quiet = true;
      continue;
    }
    if (arg === "--detach" || arg === "-d") {
      parsed.flags.detach = true;
      continue;
    }
    if (arg === "--watch") {
      parsed.flags.watch = true;
      continue;
    }
    if (arg === "--follow" || arg === "-f") {
      parsed.flags.follow = true;
      continue;
    }
    // `--worktree` is the pre-rename spelling. Still accepted so muscle memory
    // and existing scripts keep working; help only documents `--workspace`.
    if (arg === "--workspace" || arg === "--worktree") {
      parsed.flags.workspace = true;
      continue;
    }
    if (arg === "--queue") {
      parsed.flags.queue = true;
      continue;
    }
    if (arg === "--allow-dirty") {
      parsed.flags.allowDirty = true;
      continue;
    }
    if (arg === "--force") {
      parsed.flags.force = true;
      continue;
    }
    if (arg === "--dry-run") {
      parsed.flags.dryRun = true;
      continue;
    }
    if (arg === "--non-interactive") {
      parsed.flags.nonInteractive = true;
      continue;
    }
    if (arg === "--no-tmux" || arg === "--no-native" || arg === "--headless") {
      throw new Error(`${arg} was removed with the legacy dashboards. Run \`rudder\` for the native dashboard.`);
    }
    if (arg === "--open") {
      parsed.flags.open = true;
      continue;
    }
    if (arg === "--no-open") {
      parsed.flags.open = false;
      continue;
    }
    if (arg === "--no-issue") {
      parsed.flags.issue = false;
      continue;
    }
    if (takesValue(arg, "--context")) {
      parsed.flags.contextFile = readValue(argv, ++i, arg);
      continue;
    }
    if (arg.startsWith("--context=")) {
      parsed.flags.contextFile = arg.slice("--context=".length);
      continue;
    }
    if (takesValue(arg, "--props")) {
      parsed.flags.props = readValue(argv, ++i, arg);
      continue;
    }
    if (arg.startsWith("--props=")) {
      parsed.flags.props = arg.slice("--props=".length);
      continue;
    }
    if (takesValue(arg, "--port", "-p")) {
      parsed.flags.port = Number.parseInt(readValue(argv, ++i, arg), 10);
      continue;
    }
    if (arg.startsWith("--port=")) {
      parsed.flags.port = Number.parseInt(arg.slice("--port=".length), 10);
      continue;
    }
    if (takesValue(arg, "--n")) {
      parsed.flags.n = Number.parseInt(readValue(argv, ++i, arg), 10);
      continue;
    }
    if (arg.startsWith("--n=")) {
      parsed.flags.n = Number.parseInt(arg.slice("--n=".length), 10);
      continue;
    }
    if (takesValue(arg, "--model", "-m")) {
      parsed.flags.model = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--backend", "-b")) {
      parsed.flags.backend = normalizeBackend(readValue(argv, ++i, arg));
      continue;
    }
    if (takesValue(arg, "--cwd", "-C")) {
      parsed.flags.cwd = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--repo")) {
      parsed.flags.repo = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--run")) {
      parsed.flags.run = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--attempt")) {
      parsed.flags.attempt = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--task")) {
      parsed.flags.task = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--node")) {
      parsed.flags.node = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--base")) {
      parsed.flags.base = readValue(argv, ++i, arg);
      continue;
    }
    if (takesValue(arg, "--home-path")) {
      parsed.flags.homePaths = [...(parsed.flags.homePaths ?? []), readValue(argv, ++i, arg)];
      continue;
    }
    if (takesValue(arg, "--ssh", "--ssh-host")) {
      parsed.flags.sshHost = readValue(argv, ++i, arg);
      continue;
    }
    if (arg.startsWith("--model=")) {
      parsed.flags.model = arg.slice("--model=".length);
      continue;
    }
    if (arg.startsWith("--backend=")) {
      parsed.flags.backend = normalizeBackend(arg.slice("--backend=".length));
      continue;
    }
    if (arg.startsWith("--cwd=")) {
      parsed.flags.cwd = arg.slice("--cwd=".length);
      continue;
    }
    if (arg.startsWith("--repo=")) {
      parsed.flags.repo = arg.slice("--repo=".length);
      continue;
    }
    if (arg.startsWith("--run=")) {
      parsed.flags.run = arg.slice("--run=".length);
      continue;
    }
    if (arg.startsWith("--attempt=")) {
      parsed.flags.attempt = arg.slice("--attempt=".length);
      continue;
    }
    if (arg.startsWith("--task=")) {
      parsed.flags.task = arg.slice("--task=".length);
      continue;
    }
    if (arg.startsWith("--node=")) {
      parsed.flags.node = arg.slice("--node=".length);
      continue;
    }
    if (arg.startsWith("--base=")) {
      parsed.flags.base = arg.slice("--base=".length);
      continue;
    }
    if (arg.startsWith("--home-path=")) {
      parsed.flags.homePaths = [...(parsed.flags.homePaths ?? []), arg.slice("--home-path=".length)];
      continue;
    }
    if (arg.startsWith("--ssh=")) {
      parsed.flags.sshHost = arg.slice("--ssh=".length);
      continue;
    }
    if (arg.startsWith("--ssh-host=")) {
      parsed.flags.sshHost = arg.slice("--ssh-host=".length);
      continue;
    }
    if (!parsed.command && !arg.startsWith("-")) {
      parsed.command = arg;
      continue;
    }
    parsed.args.push(arg);
  }
  return parsed;
}

function takesValue(arg: string, ...names: string[]): boolean {
  return names.includes(arg);
}

function readValue(argv: string[], index: number, flag: string): string {
  const value = argv[index];
  if (!value) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

function normalizeBackend(value: string): BackendId {
  if (value === "claude" || value === "codex" || value === "acpx" || value === "opencode") {
    return value;
  }
  throw new Error(`Unknown backend: ${value}`);
}

async function maybeOnboard(): Promise<void> {
  if (await authStoreExists()) {
    return;
  }
  if (!isTty()) {
    await runOnboard({ nonInteractive: true });
    return;
  }
  await runOnboard();
}

async function openDashboard(parsed: Parsed): Promise<void> {
  await refreshModelCache();
  if (await runNativeDashboard()) {
    return;
  }
  throw new Error("rudder-native binary is not available. Reinstall or rebuild Rudder, then run `rudder` again.");
}

async function refreshModelCache(): Promise<void> {
  await Promise.all([
    discoverModelOptions("claude").catch(() => []),
    discoverModelOptions("codex").catch(() => []),
  ]);
}

async function runNativeDashboard(): Promise<boolean> {
  const nativeBinary = resolveNativeBinaryPath();
  if (!nativeBinary) {
    return false;
  }
  const update = await getUpdateAvailable().catch(() => null);
  const env = await codexLaunchEnv(process.env);
  const previousNativeTui = process.env.RUDDER_NATIVE_TUI;
  process.env.RUDDER_NATIVE_TUI = "1";
  env.RUDDER_NATIVE_TUI = "1";
  if (update) {
    env.RUDDER_UPDATE_AVAILABLE = update.latest;
    env.RUDDER_UPDATE_CURRENT = update.current;
  }
  // Kick off background LLM task-summary backfill for run records in this repo.
  // Native dashboard reads run.json directly so this is how we get nicer titles
  // for new agents - the upgrades show up on the next launch. Fire-and-forget;
  // never blocks startup.
  try {
    const repoRoot = findRepoRoot();
    if (repoRoot) {
      void backfillLlmTaskSummaries(repoRoot);
    }
  } catch {
    // ignore
  }
  // Start the board daemon in-process so the web board is live and reflects this
  // TUI session (this also registers the project so /api/projects is populated).
  // PROJECTOR-ONLY: the TUI is the sole scheduler. It schedules from its own
  // in-memory planned_nodes and MIRRORS its plan + node statuses into graph.json.
  // The daemon here must NOT run the launching scheduler, or both would launch
  // the same graph.json node (double-launch). So we pass scheduler:false: the
  // board still serves HTTP + SSE + fs.watch and reflects the mirrored graph,
  // but never launches. Browser task/steer requests use the native control inbox
  // in this mode, so the TUI handles them through the same path as task-pane input
  // instead of writing graph state it cannot read back.
  // ensureBoardRunning returns promptly; the listeners keep running on the Node
  // event loop while the native TUI is in the foreground. Strictly non-fatal: if
  // it throws, log a notice and keep launching the TUI - the daemon must never
  // block or break the session.
  let board: Awaited<ReturnType<typeof ensureBoardRunning>> | undefined;
  try {
    const repoRoot = findRepoRoot();
    if (repoRoot) {
      board = await ensureBoardRunning(repoRoot, { open: false, scheduler: false });
      // Hand the native child a deep link to THIS project's board so the `o` key /
      // `/web` opens straight into the current project (not the all-projects index).
      // The URL itself stays hidden in the TUI; the hotkey is the way in, and the
      // board's "all projects" link is the way back out to other projects.
      env.RUDDER_BOARD_URL = board.projectUrl;
      env.RUDDER_BOARD_PORT = String(board.port);
    }
  } catch (err) {
    if (isTty()) {
      console.warn(`rudder board daemon did not start: ${(err as Error)?.message ?? err}`);
    }
  }
  try {
    const code = await new Promise<number | null>((resolve, reject) => {
      const child = spawn(nativeBinary, process.argv.slice(2), {
        stdio: "inherit",
        env,
      });
      child.on("error", reject);
      child.on("exit", (exitCode) => resolve(exitCode));
    });
    process.exitCode = code ?? 1;
    // The in-process board daemon keeps the event loop alive, so without closing it
    // the CLI would hang after the foreground TUI exits. Close it before returning.
    await board?.handle?.close().catch(() => undefined);
    restoreNativeTuiEnv(previousNativeTui);
    return true;
  } catch {
    await board?.handle?.close().catch(() => undefined);
    restoreNativeTuiEnv(previousNativeTui);
    return false;
  }
}

function restoreNativeTuiEnv(previous: string | undefined): void {
  if (previous === undefined) {
    delete process.env.RUDDER_NATIVE_TUI;
  } else {
    process.env.RUDDER_NATIVE_TUI = previous;
  }
}

async function runNativeCommand(args: string[]): Promise<void> {
  const nativeBinary = resolveNativeBinaryPath();
  if (!nativeBinary) {
    throw new Error("rudder-native binary is not available in this package");
  }
  const code = await new Promise<number | null>((resolve, reject) => {
    const child = spawn(nativeBinary, args, {
      stdio: "inherit",
      env: process.env,
    });
    child.on("error", reject);
    child.on("exit", (exitCode) => resolve(exitCode));
  });
  process.exitCode = code ?? 1;
}

async function resetLocalRudderSession(parsed: Parsed): Promise<void> {
  const repoRoot = findRepoRoot();
  const stateDir = projectStateDir(repoRoot);
  await Promise.all([
    fsp.rm(path.join(stateDir, "session.json"), { force: true }),
    fsp.rm(runsDir(repoRoot), { recursive: true, force: true }),
    fsp.rm(path.join(stateDir, "tmux"), { recursive: true, force: true }),
  ]);
  await fsp.mkdir(runsDir(repoRoot), { recursive: true });
}

async function packageVersion(): Promise<string> {
  const packageFile = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "package.json");
  const raw = await fsp.readFile(packageFile, "utf8").catch(() => "");
  if (!raw) {
    return "unknown";
  }
  try {
    const parsed = JSON.parse(raw) as { version?: string };
    return parsed.version ?? "unknown";
  } catch {
    return "unknown";
  }
}

async function runBoard(parsed: Parsed): Promise<void> {
  const repoRoot = findRepoRoot();
  await registerProject(repoRoot).catch(() => undefined);
  // `rudder board` defaults to opening the browser; `--no-open` suppresses it.
  // `rudder serve` defaults to not opening unless `--open` is passed.
  const open = parsed.flags.open ?? parsed.command === "board";
  const result = await ensureBoardRunning(repoRoot, { port: parsed.flags.port, open });
  if (!result.started) {
    console.log(`rudder board already running on ${result.url}`);
    return;
  }
  // Keep the process alive while the in-process daemon serves the board.
  await new Promise<void>(() => {});
}

/**
 * `rudder __launch-node`: the native TUI calls this synchronously to isolate a
 * worker in a jj workspace (replacing `git workspace add`). It colocates jj with
 * the git repo, creates a per-node workspace, reads back its change id, and
 * prints a single JSON line {path, workspaceName, jjChangeId} to stdout. Errors
 * go to stderr and exit non-zero so the native side surfaces a clear failure.
 */
async function runLaunchNode(parsed: Parsed): Promise<void> {
  const repoRoot = parsed.flags.repo;
  const task = parsed.flags.task;
  if (!repoRoot) {
    throw new Error("__launch-node requires --repo");
  }
  if (!task) {
    throw new Error("__launch-node requires --task");
  }
  const runId = parsed.flags.node ?? newRunId(task);
  ensureJj();
  await ensureColocated(repoRoot);
  const workspace = await createNodeWorkspace({
    repoRoot,
    runId,
    task,
    ...(parsed.flags.base ? { atChangeId: parsed.flags.base } : {}),
  });
  const jjChangeId = await currentJjChangeId(workspace.path);
  process.stdout.write(
    `${JSON.stringify({
      path: workspace.path,
      workspaceName: workspace.workspaceName,
      jjChangeId,
    })}\n`,
  );
}

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  return Buffer.concat(chunks).toString("utf8");
}

/**
 * `rudder __graph-mirror --repo <root>`: read a JSON plan payload from stdin and
 * MIRROR it into graph.json. The TUI is the sole scheduler; graph.json is a
 * write-only projection the board reads (it is NEVER read back for scheduling).
 * The schema lives in graph.ts: this shim just reads stdin and hands the payload
 * to mirrorPlanIntoGraph under the graph lock. Prints `ok`/exit 0; the native
 * caller treats it as best-effort and never blocks on it.
 */
async function runGraphMirror(parsed: Parsed): Promise<void> {
  const repoRoot = parsed.flags.repo ?? findRepoRoot();
  if (!repoRoot) {
    throw new Error("__graph-mirror requires --repo");
  }

  const raw = await readStdin();
  let payload: MirrorPayload = {};
  if (raw.trim()) {
    payload = JSON.parse(raw) as MirrorPayload;
  }

  await updateGraph(repoRoot, (graph) => mirrorPlanIntoGraph(graph, payload));

  process.stdout.write("ok\n");
}

async function runUndo(opId?: string): Promise<void> {
  const repoRoot = findRepoRoot();
  ensureJj();
  await ensureColocated(repoRoot);

  if (opId) {
    await undoToOp(repoRoot, opId);
    console.log(`Restored jj to operation ${opId}.`);
    console.log("Note: jj op restore is global - it rewinds all workspaces and refs to that point.");
    return;
  }

  const entry = await popUndoEntry(repoRoot);
  if (entry) {
    if (entry.runIds.length) {
      console.log(`This undo reverts runs: ${entry.runIds.join(", ")}`);
    }
    await undoToOp(repoRoot, entry.opId);
    console.log(`Restored jj to operation ${entry.opId} (${entry.label}).`);
    console.log("Note: jj op restore is global - it rewinds all workspaces and refs to that point.");
    return;
  }

  const before = await currentOpId(repoRoot);
  await undoLast(repoRoot);
  console.log(`Undid the last jj operation${before ? ` (was at ${before})` : ""}.`);
  console.log("Note: jj undo is global - it rewinds all workspaces and refs.");
}

async function runRemember(insight: string): Promise<void> {
  // The bd-remember equivalent: append a durable insight to DECISIONS.md, the
  // tracked agent-authored knowledge surface the board's Memory view renders.
  const repoRoot = findRepoRoot();
  await appendDecision(repoRoot, insight, "cli");
  console.log("Remembered. Appended to DECISIONS.md (shared, jj-tracked).");
}

async function runTelemetryCommand(action: string | undefined): Promise<void> {
  const verb = (action ?? "status").trim().toLowerCase();
  if (verb === "on" || verb === "enable") {
    await setTelemetryEnabled(true);
    console.log(`Telemetry ON. ${TELEMETRY_NOTICE}`);
    return;
  }
  if (verb === "off" || verb === "disable") {
    await setTelemetryEnabled(false);
    console.log("Telemetry OFF. Nothing is sent; `rudder feedback` still works and stays local.");
    return;
  }
  if (verb === "status") {
    const enabled = await telemetryEnabled();
    console.log(enabled ? `Telemetry ON. ${TELEMETRY_NOTICE}` : "Telemetry OFF.");
    return;
  }
  throw new Error("Usage: rudder telemetry [status|on|off]");
}

async function runFeedbackCommand(parsed: Parsed): Promise<void> {
  // `--context <file>` is how the dashboard hands over what was on screen; a
  // plain CLI report just carries the message.
  const contextFile = parsed.flags.contextFile;
  let context: FeedbackContext | undefined;
  if (contextFile) {
    const raw = await fsp.readFile(contextFile, "utf8").catch(() => "");
    try {
      context = JSON.parse(raw) as FeedbackContext;
    } catch {
      context = undefined;
    }
  }
  const text = parsed.args.join(" ").trim();
  const report = await submitFeedback({
    text: text || (context as { text?: string } | undefined)?.text || "",
    context,
    issue: parsed.flags.issue,
  });
  console.log(`Thanks. Saved locally to ${report.localPath.replace(os.homedir(), "~")}.`);
  if (report.issueUrl) {
    console.log(`Filed ${report.issueUrl}`);
  }
  for (const reason of report.skipped) {
    console.log(`  note: ${reason}`);
  }
}

async function runEventCommand(parsed: Parsed): Promise<void> {
  const event = parsed.args[0]?.trim();
  if (!event) {
    return;
  }
  let properties: Record<string, string | number | boolean> = {};
  const raw = parsed.flags.props;
  if (raw) {
    try {
      const parsedProps = JSON.parse(raw) as Record<string, unknown>;
      for (const [key, value] of Object.entries(parsedProps)) {
        if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
          properties[key] = value;
        }
      }
    } catch {
      properties = {};
    }
  }
  // Stamp the project id here so ONE implementation owns it: the dashboard
  // shells out with its repo as cwd, and CLI invocations resolve the same root.
  properties.project ??= projectHash(dashboardRoot());
  await capture(event, properties);
}

async function runShare(text: string): Promise<void> {
  const repoRoot = findRepoRoot();
  await appendSharedContext(repoRoot, text, "cli share");
  const runs = await listRuns(repoRoot).catch(() => []);
  await syncSharedContextToWorkspaces(
    repoRoot,
    runs.map((run) => run.workspace.path),
  ).catch(() => undefined);
  console.log(`Shared. Appended to ${SHARED_CONTEXT_FILE} (gitignored, mirrored to agents).`);
}

/** Read all of stdin when it is piped (never when interactive), with a short
 *  timeout so a worker that calls `rudder done` without piping never hangs. */
async function readPipedStdin(timeoutMs = 3000): Promise<string> {
  if (process.stdin.isTTY) {
    return "";
  }
  return await new Promise<string>((resolve) => {
    const chunks: Buffer[] = [];
    const done = (): void => {
      process.stdin.removeAllListeners("data");
      resolve(Buffer.concat(chunks).toString("utf8"));
    };
    const timer = setTimeout(done, timeoutMs);
    process.stdin.on("data", (c) => chunks.push(Buffer.from(c)));
    process.stdin.on("end", () => {
      clearTimeout(timer);
      done();
    });
    process.stdin.on("error", () => {
      clearTimeout(timer);
      resolve("");
    });
  });
}

/** `rudder done [--node <id>] ['<json>']`: a worker's end-of-task report. Accepts
 *  a CompletionNote JSON via stdin (piped) or as the joined args; a non-JSON arg
 *  is treated as a freeform summary. The note travels back to the orchestrator over
 *  two durable channels so a real Claude/Codex worker's report is
 *  not lost to terminal rendering:
 *    1. RUDDER_DONE_FILE (set by the launcher to <workspace>/.rudder/done/<node>.json):
 *       a machine-readable JSON drop the orchestrator reads straight off disk. This is
 *       the authoritative channel - it never passes through the agent's TUI.
 *    2. DECISIONS.md: a human-legible bullet siblings re-read.
 *  Never manages jj - just records, like `rudder remember`. */
async function runDone(parsed: Parsed): Promise<void> {
  const repoRoot = findRepoRoot();
  const node = parsed.flags.node;
  let raw = (await readPipedStdin()).trim();
  if (!raw && parsed.args.length) {
    raw = parsed.args.join(" ").trim();
  }
  let note: CompletionNote = parseCompletionNoteArg(raw);
  if (node && !note.node) {
    note.node = node;
  }
  // (1) Authoritative file drop. Prefer the launcher-provided absolute path; otherwise
  // fall back to the conventional path under the cwd (.rudder/done/<node>.json) so a
  // hand-run `rudder done --node X` still reports.
  const doneFile =
    process.env.RUDDER_DONE_FILE ||
    (node ? path.join(process.cwd(), ".rudder", "done", `${node}.json`) : "");
  if (doneFile) {
    await writeCompletionNoteFile(doneFile, note);
  }
  // (2) Human-legible record for sibling agents and review history.
  await appendCompletionNote(repoRoot, note, node || "worker");
}

async function runPlan(task: string): Promise<void> {
  const repoRoot = findRepoRoot();
  ensureJj();
  await ensureColocated(repoRoot);
  const branch = await currentBranch(repoRoot).catch(() => "");

  const dag = await planTask(task, { root: repoRoot, branch });
  const gate = gateDecision(dag);

  if (gate.autoRun) {
    console.log(`auto: ${gate.reason}`);
  } else {
    console.log(`gate: ${gate.reason}`);
    console.log(`Plan: ${dag.nodes.length} node(s), ${dag.edges.length} edge(s)`);
    for (const node of dag.nodes) {
      const deps = node.deps.length
        ? ` deps=[${node.deps.map((dep) => `${dep.on}:${dep.type}`).join(", ")}]`
        : "";
      console.log(`  - ${node.id} ${node.title}${deps}`);
    }
  }

  await scaffoldPlan(repoRoot, dag);
  console.log(`graph: ${graphPath(repoRoot)}`);
}

async function runFanout(task: string, n: number | undefined, backend?: BackendId): Promise<void> {
  const repoRoot = findRepoRoot();
  ensureJj();
  await ensureColocated(repoRoot);

  const requested = typeof n === "number" && Number.isFinite(n) ? n : 3;
  const dag = buildFanoutDag(task, requested, backend ? { backend } : {});
  const variants = dag.nodes.filter((node) => node.id !== "judge");

  console.log(`fanout: ${variants.length} variant(s) + 1 judge`);
  for (const node of dag.nodes) {
    const judgeOf = node.deps.filter((dep) => dep.type === "judge").map((dep) => dep.on);
    const suffix = judgeOf.length ? ` judges=[${judgeOf.join(", ")}]` : "";
    console.log(`  - ${node.id} ${node.title}${suffix}`);
  }

  await scaffoldPlan(repoRoot, dag);
  console.log(`graph: ${graphPath(repoRoot)}`);
  console.log("The judge launches once every variant reaches review; variants are not merged.");
}

async function runContextCommand(parsed: Parsed): Promise<void> {
  const sub = parsed.args[0] ?? "audit";
  const repoRoot = findRepoRoot();
  if (sub === "audit") {
    await printContextAudit({ json: parsed.flags.json, repoRoot });
    return;
  }
  throw new Error("Usage: rudder context [audit]");
}

function printHelp(): void {
  console.log(`rudder

Usage:
  rudder                         Open native dashboard with real agent panes
  rudder restart                 Clear local session and open dashboard
  rudder "task"
  rudder run [options] "task"
  rudder claude [options] "task"
  rudder codex [options] "task"
  rudder login
  rudder cloud [name or task]
  rudder cloud help
  rudder cloud byoc <ssh-host>
  rudder cloud runtime [fly|byoc]
  rudder cloud vm <task>
  rudder cloud list
  rudder cloud attach <id>
  rudder cloud talk <id> "message"
  rudder cloud output <id>
  rudder cloud onload [runId]
  rudder cloud logs <id>
  rudder cloud bootstrap <id>
  rudder cloud quickstart
  rudder sail [name or task]
Run management:
  rudder watch [run]              Attach to live output
  rudder logs [run] [--follow]    Print saved output
  rudder status [--json]          Show active runs for this repo
  rudder runs [--json]            List runs for this repo
  rudder stop <run>               Cancel a run
  rudder delete <run>             Delete a run and its workspace
  rudder merge <run>              Merge a run into the current change
  rudder sync [run]               Rebase a run's jj change onto its base without merging
  rudder cleanup [--force]        Remove merged run workspaces
  rudder gc [--dry-run]           Prune bounded diagnostics, old binaries, and stale run records
  rudder undo [opId]              Rewind jj to an op id, or the last undo-stack entry (global)

Improvement loop:
  rudder improve run [--dry-run] [--budget-usd N]   Mine telemetry, propose+judge fixes, ship a release
  rudder improve status                             Last cycle, shipped versions, ledger tail
  rudder improve report [date]                      Print a cycle report
  rudder improve schedule install|uninstall|status  Nightly launchd schedule (macOS)

Planner:
  rudder plan "task"             Decompose a task into a DAG, scaffold empty jj changes, write graph.json
  rudder fanout "task" [--n N]   Fan out N variant agents on one task, then a judge picks/merges the best (default N=3)
  rudder context audit           Audit agent context files for size, duplication, secrets, and injection text

Conversation handoff:
  rudder handoff "<next step>"    Hand THIS chat to Rudder: it forks the conversation into a worker that already knows everything
  rudder handoff --here "..."     Continue it in the main checkout instead of an isolated workspace
  rudder handoff --opencode "..." Hand off an opencode chat (default is the claude one; --codex for codex)
  rudder handoff --list           List recent conversations in this repo with their session ids

Telemetry and feedback:
  rudder feedback "<what broke>"  Send a report: message + version/model/recent notices (never prompts or code)
  rudder telemetry [status|on|off] Anonymous usage events; on by default, off in one command

Memory:
  rudder remember "<insight>"    Append a durable cross-cutting decision to DECISIONS.md (shared, jj-tracked)
  rudder share "<token/context>"  Append gitignored shared context to RUDDER_SHARED.md for all agents

Board:
  rudder board [--port N] [--open|--no-open]   Serve the localhost board (opens browser by default)
  rudder serve [--port N] [--open]             Ensure the board server is up

Setup:
  rudder onboard
  rudder doctor [--json]
  rudder mouse-test [raw|parsed]    Show whether your terminal sends wheel events

Cloud:
  rudder login                    Open browser login and store cloud token
  rudder cloud [name or task]     Start a cloud worker from this repo snapshot
  rudder cloud list               List cloud workers/runs
  rudder cloud help               Show cloud command help
  rudder cloud byoc <ssh>         Use your own SSH host for BYOC cloud launches
  rudder cloud runtime [runtime]  Show or set fly/byoc cloud runtime
  rudder cloud vm <task>          Prepare a BYOC worker command for a task
  rudder cloud onload [runId]     Move this Rudder workspace, or one run, to cloud
  rudder cloud logs <id>          Show cloud worker status while log streaming is pending
  rudder cloud attach <id>        Attach this terminal to a live cloud worker
  rudder cloud bootstrap <id>     Regenerate a BYOC worker command
  rudder cloud pause <id>         Pause an idle cloud worker
  rudder cloud resume <id>        Resume a cloud worker
  rudder sail [name or task]      Alias for starting a cloud worker

Options:
  -d, --detach                    Start in background
      --workspace                  Always isolate the run in its own jj workspace
      --queue                     Queue mode (reserved)
  -m, --model <model>             Backend model
  -b, --backend <backend>         claude, codex, or opencode
  -C, --cwd <dir>                 Run from another directory
  -p, --port <port>               Board server port (default 4774)
      --n <count>                 Number of fan-out variants for rudder fanout (default 3)
      --open / --no-open          Open (or do not open) the browser for the board
      --home-path <path>          Include extra HOME path in cloud snapshot
      --ssh <host>                BYOC SSH host from ~/.ssh/config
      --json                      Machine-readable output
  -v, --version                   Print version
      --allow-dirty               Allow merge into dirty target branch
      --dry-run                   Report gc cleanup without deleting

Rebase-first merge:
  Set {"mergeStrategy":"rebase"} in ~/.rudder/config.json to rebase before
  merging. If that rebase conflicts, Rudder leaves the workspace mid-rebase so
  you can resolve it, run git rebase --continue, and retry sync or merge.
`);
}
