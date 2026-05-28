import readline from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import { runDoctor } from "./auth.js";
import { currentBranch, findRepoRoot } from "./git.js";
import { cleanupRuns, listProjectRuns, mergeRun, printLogs, startRun, statusRuns, stopRun, watchRun, } from "./run-manager.js";
import { loadConfig, rememberBackendSelection } from "./state.js";
import { shortenHome } from "./util.js";
export async function runInteractiveShell(defaults) {
    const config = await loadConfig();
    const backend = defaults?.backend ?? config.lastUsedBackend ?? config.defaultBackend;
    const state = {
        backend,
        model: defaults?.model ?? modelForBackend(backend, config),
        worktree: defaults?.worktree ? "always" : "auto",
        detach: Boolean(defaults?.detach),
    };
    const repoRoot = findRepoRoot();
    const branch = await currentBranch(repoRoot);
    console.log("rudder");
    console.log(`${shortenHome(repoRoot)}${branch ? `  ${branch}` : ""}  ${formatBackend(state)}  worktree:${state.worktree}`);
    console.log("Type a task. /help for commands. Ctrl-C detaches while watching.");
    const rl = readline.createInterface({ input, output });
    try {
        while (true) {
            const line = await rl.question(`${promptLabel(state)} `).catch((error) => {
                if (isReadlineClosed(error)) {
                    return "/exit";
                }
                throw error;
            });
            const trimmed = line.trim();
            if (!trimmed) {
                continue;
            }
            if (trimmed.startsWith("/")) {
                const shouldExit = await handleSlashCommand(trimmed, state);
                if (shouldExit) {
                    return;
                }
                continue;
            }
            await runTaskFromShell(trimmed, state);
        }
    }
    finally {
        rl.close();
    }
}
async function handleSlashCommand(line, state) {
    const [command = "", ...args] = line.slice(1).trim().split(/\s+/).filter(Boolean);
    switch (command) {
        case "?":
        case "help":
            printShellHelp();
            return false;
        case "exit":
        case "quit":
        case "q":
            return true;
        case "backend":
            if (setBackend(state, args[0])) {
                await rememberBackendSelection({ backend: state.backend });
            }
            return false;
        case "model":
            state.model = args.join(" ") || undefined;
            await rememberBackendSelection({
                backend: state.backend,
                model: state.model,
                updateModel: true,
            });
            console.log(`model: ${state.model || "(backend default)"}`);
            return false;
        case "worktree":
            setWorktree(state, args[0]);
            return false;
        case "detach":
            state.detach = parseOnOff(args[0], !state.detach);
            console.log(`watch: ${state.detach ? "off" : "on"}`);
            return false;
        case "status":
            await statusRuns();
            return false;
        case "runs":
            await listProjectRuns();
            return false;
        case "watch":
            await watchFromShell(args[0]);
            return false;
        case "logs":
            await printLogs(args[0], args.includes("--follow") || args.includes("-f"));
            return false;
        case "stop":
            if (!args[0]) {
                console.log("usage: /stop <run>");
                return false;
            }
            await stopRun(args[0]);
            return false;
        case "merge":
            if (!args[0]) {
                console.log("usage: /merge <run>");
                return false;
            }
            await mergeRun(args[0], args.includes("--allow-dirty"));
            return false;
        case "cleanup":
            await cleanupRuns(args.includes("--force"));
            return false;
        case "doctor":
            await runDoctor();
            return false;
        case "clear":
            process.stdout.write("\x1Bc");
            return false;
        default:
            console.log(`Unknown command: /${command}`);
            console.log("Type /help for commands.");
            return false;
    }
}
async function runTaskFromShell(task, state) {
    const controller = new AbortController();
    let detachedBySignal = false;
    const onSigint = () => {
        detachedBySignal = true;
        controller.abort();
        console.log("\n[rudder] detached. Run continues in the background.");
    };
    process.once("SIGINT", onSigint);
    try {
        await startRun({
            task,
            backend: state.backend,
            model: state.model,
            detach: state.detach,
            worktree: state.worktree === "always",
            exitOnComplete: false,
            watchSignal: controller.signal,
            quiet: true,
            view: "shell",
        });
    }
    finally {
        process.off("SIGINT", onSigint);
    }
    if (state.detach || detachedBySignal) {
        console.log("[rudder] use /status, /watch, or /logs to follow up.");
    }
}
async function watchFromShell(runId) {
    const controller = new AbortController();
    const onSigint = () => {
        controller.abort();
        console.log("\n[rudder] detached from watch.");
    };
    process.once("SIGINT", onSigint);
    try {
        await watchRun({ runId, follow: true, exitOnComplete: false, signal: controller.signal, view: "shell" });
    }
    finally {
        process.off("SIGINT", onSigint);
    }
}
function setBackend(state, value) {
    if (value !== "claude" && value !== "codex" && value !== "acpx") {
        console.log("usage: /backend claude|codex|acpx");
        return false;
    }
    state.backend = value;
    state.model = undefined;
    console.log(`backend: ${formatBackend(state)}`);
    return true;
}
function setWorktree(state, value) {
    if (value === "auto" || value === undefined) {
        state.worktree = "auto";
    }
    else if (value === "always" || value === "on") {
        state.worktree = "always";
    }
    else {
        console.log("usage: /worktree auto|always");
        return;
    }
    console.log(`worktree: ${state.worktree}`);
}
function parseOnOff(value, fallback) {
    if (!value) {
        return fallback;
    }
    if (["on", "yes", "true", "1"].includes(value.toLowerCase())) {
        return true;
    }
    if (["off", "no", "false", "0"].includes(value.toLowerCase())) {
        return false;
    }
    return fallback;
}
function printShellHelp() {
    console.log(`Commands:
  /backend claude|codex|acpx   Choose worker backend
  /model <model>               Set model for new tasks
  /worktree auto|always        Isolate runs in worktrees/workspaces
  /detach on|off               Start tasks without watching output
  /status                      Show active runs
  /runs                        List recent runs
  /watch [run]                 Attach to a run
  /logs [run] [--follow]       Print saved output
  /stop <run>                  Cancel a run
  /merge <run>                 Merge a worktree run
  /cleanup [--force]           Remove merged worktrees
  /doctor                      Check local tools and auth
  /exit                        Leave the shell

Type any non-command line to start a task.`);
}
function promptLabel(state) {
    void state;
    return ">";
}
function formatBackend(state) {
    return `${state.backend}${state.model ? ` (${state.model})` : ""}`;
}
function modelForBackend(backend, config) {
    if (backend === "claude") {
        return config.backends.claude?.model;
    }
    if (backend === "codex") {
        return config.backends.codex?.model;
    }
    return config.backends.acpx?.model;
}
function isReadlineClosed(error) {
    return error instanceof Error && error.message.includes("readline was closed");
}
//# sourceMappingURL=repl.js.map