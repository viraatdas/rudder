import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { spawn } from "node:child_process";
import fsp from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Box, Text, render, useInput, useWindowSize } from "ink";
import { permissionAttentionFromOutput } from "./agent-attention.js";
import { discoverEffortOptions, fallbackEffortOptions } from "./effort.js";
import { currentBranch, findRepoRoot } from "./git.js";
import { discoverModelOptions, fallbackModelOptions } from "./models.js";
import { startNativePlan, startNativeRun, deleteRun, mergeRun, reconcileNativeTerminals, stopRun, syncRun } from "./run-manager.js";
import { listRuns, loadConfig, outputPath, rememberBackendSelection } from "./state.js";
import { loadTmuxDashboardState, updateTmuxDashboardState, } from "./tmux-state.js";
import { detachClient, resizePane, selectPane } from "./tmux.js";
import { taskDisplayLabel } from "./task-summary.js";
import { shortenHome } from "./util.js";
const COMPLETION_SOUND = fileURLToPath(new URL("../assets/sounds/ping.mp3", import.meta.url));
const ATTENTION_TAIL_BYTES = 64 * 1024;
const TASK_HISTORY_LIMIT = 100;
const SLASH_COMMANDS = [
    { label: "/backend claude", detail: "use Claude Code for new tasks", value: "/backend claude" },
    { label: "/backend codex", detail: "use Codex for new tasks", value: "/backend codex" },
    { label: "/plan", detail: "toggle Rudder read-only plan mode", value: "/plan" },
    { label: "/plan <task>", detail: "plan one task without toggling", value: "/plan ", complete: "/plan " },
    { label: "/run <task>", detail: "start implementation even when plan mode is on", value: "/run ", complete: "/run " },
    { label: "/sync", detail: "rebase the selected worktree without merging", value: "/sync" },
    { label: "/sync <run>", detail: "rebase a worktree without merging", value: "/sync ", complete: "/sync " },
    { label: "/model", detail: "pick from available models", value: "/model" },
    { label: "/model <id>", detail: "set model for new tasks", value: "/model ", complete: "/model " },
    { label: "/clear", detail: "clear the task input", value: "/clear" },
    { label: "/help", detail: "show available task commands", value: "/help" },
    { label: "/detach", detail: "detach the tmux session", value: "/detach" },
];
export async function runTmuxAgentPane(defaults) {
    const instance = render(_jsx(AgentPane, { defaults: defaults }), {
        exitOnCtrlC: false,
        maxFps: 20,
    });
    await instance.waitUntilExit();
}
export async function runTmuxTaskPane(defaults) {
    const instance = render(_jsx(TaskPane, { defaults: defaults }), {
        exitOnCtrlC: false,
        maxFps: 30,
    });
    await instance.waitUntilExit();
}
export async function runTmuxWorkerIdle(defaults) {
    const instance = render(_jsx(WorkerIdle, { defaults: defaults }), {
        exitOnCtrlC: false,
        maxFps: 10,
    });
    await instance.waitUntilExit();
}
function AgentPane({ defaults }) {
    const size = useWindowSize();
    const [repoRoot, setRepoRoot] = useState(() => findRepoRoot());
    const [branch, setBranch] = useState("HEAD");
    const [config, setConfig] = useState(null);
    const [runs, setRuns] = useState([]);
    const [selectedRunId, setSelectedRunId] = useState();
    const [notice, setNotice] = useState("");
    const [deleteIntent, setDeleteIntent] = useState(null);
    const alertRef = useRef(null);
    const refresh = useCallback(async () => {
        const root = findRepoRoot();
        await reconcileNativeTerminals(root).catch(() => undefined);
        const [nextBranch, nextConfig, nextRuns, state] = await Promise.all([
            currentBranch(root),
            loadConfig(),
            loadAgentPaneRuns(root),
            loadTmuxDashboardState(root, defaults.tmuxSessionName),
        ]);
        setRepoRoot(root);
        setBranch(nextBranch);
        setConfig(nextConfig);
        setRuns(nextRuns);
        setSelectedRunId((current) => state?.selectedRunId ?? current ?? nextRuns[0]?.id);
    }, [defaults.tmuxSessionName]);
    useEffect(() => {
        void refresh();
        const timer = setInterval(() => void refresh(), 1000);
        return () => clearInterval(timer);
    }, [refresh]);
    useEffect(() => {
        notifyRunAlerts(runs, alertRef);
    }, [runs]);
    const selectedIndex = Math.max(0, runs.findIndex((run) => run.id === selectedRunId));
    const selectedRun = runs[selectedIndex];
    const selectRun = useCallback(async (run) => {
        if (!run) {
            return;
        }
        setSelectedRunId(run.id);
        await updateTmuxDashboardState(repoRoot, defaults.tmuxSessionName, { selectedRunId: run.id });
    }, [defaults.tmuxSessionName, repoRoot]);
    useInput((chunk, key) => {
        if (key.ctrl && chunk === "c") {
            void detachClient(defaults.tmuxSessionName);
            return;
        }
        if (deleteIntent) {
            if (key.escape) {
                setDeleteIntent(null);
                setNotice("");
                return;
            }
            if (chunk === "d") {
                void deleteRun(deleteIntent.runId, { force: true, silent: true })
                    .then(() => {
                    setDeleteIntent(null);
                    setNotice(`deleted ${shortId(deleteIntent.runId)}`);
                    return refresh();
                })
                    .catch((error) => setNotice(error instanceof Error ? error.message : String(error)));
                return;
            }
            return;
        }
        if (key.upArrow || chunk === "k") {
            void selectRun(runs[Math.max(0, selectedIndex - 1)]);
            return;
        }
        if (key.downArrow || chunk === "j") {
            void selectRun(runs[Math.min(runs.length - 1, selectedIndex + 1)]);
            return;
        }
        if (key.return || chunk === "f") {
            void focusSelectedWorker(repoRoot, defaults.tmuxSessionName, selectedRun);
            return;
        }
        if (chunk === "m" && selectedRun) {
            void mergeRun(selectedRun.id, false, { silent: true })
                .then((merged) => {
                if (merged.merge?.status === "conflict") {
                    setNotice(`merge conflict ${shortId(selectedRun.id)}`);
                }
                else if (merged.merge?.status === "merged") {
                    setNotice(`merged ${shortId(selectedRun.id)}`);
                }
                else {
                    setNotice(`merge failed ${shortId(selectedRun.id)}: ${merged.merge?.error ?? "unknown error"}`);
                }
                return refresh();
            })
                .catch((error) => setNotice(error instanceof Error ? error.message : String(error)));
            return;
        }
        if (chunk === "u" && selectedRun) {
            void syncRun(selectedRun.id, { silent: true })
                .then((synced) => {
                setNotice(syncNotice(synced));
                return refresh();
            })
                .catch((error) => setNotice(error instanceof Error ? error.message : String(error)));
            return;
        }
        if (chunk === "s" && selectedRun) {
            void stopRun(selectedRun.id, { silent: true }).then(refresh);
            return;
        }
        if (chunk === "d" && selectedRun) {
            setDeleteIntent({ runId: selectedRun.id });
            setNotice("delete? press d to delete run + worktree, Esc cancel");
            return;
        }
        if (chunk === "q") {
            void detachClient(defaults.tmuxSessionName);
        }
    });
    const width = Math.max(24, size.columns);
    const maxRuns = Math.max(1, Math.floor((size.rows - 5) / 3));
    const visibleRuns = runs.slice(0, maxRuns);
    return (_jsxs(Box, { flexDirection: "column", children: [_jsx(Text, { bold: true, children: "rudder" }), _jsx(Text, { color: "gray", children: summarize(`${shortenHome(repoRoot)} ${branch}`, width) }), _jsxs(Text, { children: ["agents ", _jsxs(Text, { color: "gray", children: [runs.length, " runs"] })] }), visibleRuns.length === 0 ? _jsx(Text, { color: "gray", children: "No agents yet." }) : visibleRuns.map((run) => (_jsxs(Box, { flexDirection: "column", children: [_jsxs(Text, { color: run.id === selectedRun?.id ? "cyan" : taskColor(run), children: [run.id === selectedRun?.id ? "> " : "  ", summarize(taskDisplayLabel(run, 80), width - 3)] }), _jsxs(Text, { children: [_jsxs(Text, { color: runStatusColor(run), children: ["  ", statusMark(run)] }), _jsxs(Text, { color: "gray", children: ["  ", run.backend, " "] }), _jsx(Text, { color: "magenta", children: modelLabel(run, config) })] })] }, run.id))), notice ? _jsx(Text, { color: deleteIntent ? "red" : "yellow", children: summarize(notice, width) }) : null, _jsx(Text, { color: "gray", children: "j/k select  Enter focus  u sync  m merge  dd delete" })] }));
}
function TaskPane({ defaults }) {
    const [repoRoot, setRepoRoot] = useState(() => findRepoRoot());
    const [config, setConfig] = useState(null);
    const [backend, setBackend] = useState(toNativeBackend(defaults.backend ?? "claude"));
    const [model, setModel] = useState(defaults.model);
    const [effort, setEffort] = useState();
    const [input, setInput] = useState("");
    const [planMode, setPlanMode] = useState(false);
    const inputRef = useRef("");
    const taskHistoryRef = useRef([]);
    const taskHistoryIndexRef = useRef(null);
    const taskHistoryDraftRef = useRef("");
    const [notice, setNotice] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [commandIndex, setCommandIndex] = useState(0);
    const [modelPickerOpen, setModelPickerOpen] = useState(false);
    const [modelPickerStep, setModelPickerStep] = useState("model");
    const [modelIndex, setModelIndex] = useState(0);
    const [effortIndex, setEffortIndex] = useState(0);
    const [pendingModel, setPendingModel] = useState(null);
    const [claudeModels, setClaudeModels] = useState([]);
    const [codexModels, setCodexModels] = useState([]);
    const [claudeEfforts, setClaudeEfforts] = useState([]);
    const [codexEfforts, setCodexEfforts] = useState([]);
    const [taskPaneId, setTaskPaneId] = useState();
    const refresh = useCallback(async () => {
        const root = findRepoRoot();
        const [nextConfig, state] = await Promise.all([
            loadConfig(),
            loadTmuxDashboardState(root, defaults.tmuxSessionName),
        ]);
        setRepoRoot(root);
        setConfig(nextConfig);
        const nextBackend = state?.backend ?? toNativeBackend(defaults.backend ?? nextConfig.lastUsedBackend ?? nextConfig.defaultBackend);
        setBackend(nextBackend);
        setModel(state?.model ?? defaults.model);
        setEffort(state?.effort ?? effortForBackend(nextBackend, nextConfig));
        setTaskPaneId(state?.taskPaneId);
    }, [defaults.backend, defaults.model, defaults.tmuxSessionName]);
    useEffect(() => {
        void refresh();
        const timer = setInterval(() => void refresh(), 1500);
        return () => clearInterval(timer);
    }, [refresh]);
    const commandOptions = useMemo(() => filterSlashCommands(input), [input]);
    const commandMenuOpen = !modelPickerOpen && input.startsWith("/") && !isExactRunnableCommand(input) && commandOptions.length > 0;
    const claudeDefault = config?.backends.claude?.model;
    const codexDefault = config?.backends.codex?.model;
    const modelOptions = useMemo(() => {
        const primary = backend === "claude"
            ? withBackend(claudeModels.length ? claudeModels : fallbackModelOptions("claude", claudeDefault), "claude")
            : withBackend(codexModels.length ? codexModels : fallbackModelOptions("codex", codexDefault), "codex");
        const secondary = backend === "claude"
            ? withBackend(codexModels.length ? codexModels : fallbackModelOptions("codex", codexDefault), "codex")
            : withBackend(claudeModels.length ? claudeModels : fallbackModelOptions("claude", claudeDefault), "claude");
        return [...primary, ...secondary];
    }, [backend, claudeDefault, claudeModels, codexDefault, codexModels]);
    const effortOptionsFor = useCallback((nextBackend) => (nextBackend === "claude"
        ? (claudeEfforts.length ? claudeEfforts : fallbackEffortOptions("claude"))
        : (codexEfforts.length ? codexEfforts : fallbackEffortOptions("codex"))), [claudeEfforts, codexEfforts]);
    const setTaskInput = useCallback((next) => {
        const value = typeof next === "function" ? next(inputRef.current) : next;
        inputRef.current = value;
        setInput(value);
    }, []);
    const resetTaskHistoryNavigation = useCallback(() => {
        taskHistoryIndexRef.current = null;
        taskHistoryDraftRef.current = "";
    }, []);
    const editTaskInput = useCallback((next) => {
        resetTaskHistoryNavigation();
        setTaskInput(next);
    }, [resetTaskHistoryNavigation, setTaskInput]);
    const rememberTaskHistory = useCallback((value) => {
        const trimmed = value.trim();
        if (!trimmed) {
            return;
        }
        const history = taskHistoryRef.current;
        history.push(trimmed);
        if (history.length > TASK_HISTORY_LIMIT) {
            history.splice(0, history.length - TASK_HISTORY_LIMIT);
        }
        resetTaskHistoryNavigation();
    }, [resetTaskHistoryNavigation]);
    const showTaskHistory = useCallback((direction) => {
        const history = taskHistoryRef.current;
        if (!history.length) {
            return false;
        }
        if (direction === "previous") {
            const current = taskHistoryIndexRef.current;
            if (current === null) {
                taskHistoryDraftRef.current = inputRef.current;
                taskHistoryIndexRef.current = history.length - 1;
            }
            else {
                taskHistoryIndexRef.current = Math.max(0, Math.min(current, history.length - 1) - 1);
            }
            setTaskInput(history[taskHistoryIndexRef.current] ?? "");
            return true;
        }
        const current = taskHistoryIndexRef.current;
        if (current === null) {
            return false;
        }
        if (current + 1 < history.length) {
            taskHistoryIndexRef.current = current + 1;
            setTaskInput(history[taskHistoryIndexRef.current] ?? "");
            return true;
        }
        taskHistoryIndexRef.current = null;
        const draft = taskHistoryDraftRef.current;
        taskHistoryDraftRef.current = "";
        setTaskInput(draft);
        return true;
    }, [setTaskInput]);
    useEffect(() => {
        setCommandIndex(0);
    }, [input]);
    useEffect(() => {
        let cancelled = false;
        void Promise.all([
            discoverModelOptions("claude", claudeDefault).catch(() => fallbackModelOptions("claude", claudeDefault)),
            discoverModelOptions("codex", codexDefault).catch(() => fallbackModelOptions("codex", codexDefault)),
        ]).then(([nextClaudeModels, nextCodexModels]) => {
            if (!cancelled) {
                setClaudeModels(nextClaudeModels);
                setCodexModels(nextCodexModels);
                setModelIndex(0);
            }
        });
        return () => {
            cancelled = true;
        };
    }, [claudeDefault, codexDefault]);
    useEffect(() => {
        let cancelled = false;
        void Promise.all([
            discoverEffortOptions("claude").catch(() => fallbackEffortOptions("claude")),
            discoverEffortOptions("codex").catch(() => fallbackEffortOptions("codex")),
        ]).then(([nextClaudeEfforts, nextCodexEfforts]) => {
            if (!cancelled) {
                setClaudeEfforts(nextClaudeEfforts);
                setCodexEfforts(nextCodexEfforts);
            }
        });
        return () => {
            cancelled = true;
        };
    }, []);
    useEffect(() => {
        if (!taskPaneId) {
            return;
        }
        void resizePane(taskPaneId, modelPickerOpen ? 10 : 3);
    }, [modelPickerOpen, taskPaneId]);
    const submit = useCallback(async (override) => {
        const task = (override ?? inputRef.current).trim();
        if (!task || submitting) {
            return;
        }
        const resolvedCommand = resolveSlashCommand(task);
        if (resolvedCommand && resolvedCommand.value !== task && !resolvedCommand.complete) {
            await submit(resolvedCommand.value);
            return;
        }
        rememberTaskHistory(task);
        if (task === "/model") {
            setTaskInput("");
            setNotice("");
            setModelPickerOpen(true);
            setModelPickerStep("model");
            setModelIndex(0);
            setPendingModel(null);
            return;
        }
        if (task === "/plan") {
            setPlanMode((current) => {
                const next = !current;
                setNotice(next ? "Plan mode on: Enter starts a read-only planner" : "Plan mode off");
                return next;
            });
            setTaskInput("");
            setModelPickerOpen(false);
            return;
        }
        if (task.startsWith("/plan ")) {
            const planTask = task.slice("/plan ".length).trim();
            if (!planTask) {
                setNotice("Usage: /plan <task>");
                return;
            }
            await startPlanner(planTask);
            return;
        }
        if (task.startsWith("/run ")) {
            const runTask = task.slice("/run ".length).trim();
            if (!runTask) {
                setNotice("Usage: /run <task>");
                return;
            }
            await startWorker(runTask);
            return;
        }
        if (task === "/sync" || task.startsWith("/sync ")) {
            await syncSelectedRun(task.slice("/sync".length).trim());
            return;
        }
        if (task.startsWith("/model ")) {
            const nextModel = task.slice("/model ".length).trim() || undefined;
            setModel(nextModel);
            setEffort(undefined);
            await updateBackendDefaults(repoRoot, defaults.tmuxSessionName, backend, nextModel, undefined, {
                updateModel: true,
                updateEffort: true,
            });
            setTaskInput("");
            setNotice("");
            return;
        }
        if (task === "/backend claude" || task === "/backend codex") {
            const nextBackend = task.endsWith("codex") ? "codex" : "claude";
            setBackend(nextBackend);
            setModel(undefined);
            const nextEffort = effortForBackend(nextBackend, config);
            setEffort(nextEffort);
            await updateBackendDefaults(repoRoot, defaults.tmuxSessionName, nextBackend, undefined, nextEffort, {
                updateModel: false,
                updateEffort: false,
            });
            setTaskInput("");
            setNotice("");
            setModelPickerOpen(false);
            return;
        }
        if (task === "/clear") {
            setTaskInput("");
            setNotice("");
            return;
        }
        if (task === "/help") {
            setTaskInput("");
            setNotice("/plan toggles read-only planning, /run bypasses it, /sync rebases a worktree, /backend claude|codex, /model");
            return;
        }
        if (task === "/detach") {
            await detachClient(defaults.tmuxSessionName);
            return;
        }
        if (task.startsWith("/")) {
            setNotice("Unknown command. Type / to see commands.");
            return;
        }
        if (planMode) {
            await startPlanner(task);
            return;
        }
        await startWorker(task);
    }, [backend, config, defaults.tmuxSessionName, effort, effortOptionsFor, model, modelIndex, modelOptions, modelPickerStep, pendingModel, planMode, rememberTaskHistory, repoRoot, setTaskInput, submitting]);
    async function startWorker(task) {
        const state = await loadTmuxDashboardState(repoRoot, defaults.tmuxSessionName);
        if (!state) {
            setNotice("Rudder tmux state is missing. Reopen rudder.");
            return;
        }
        setSubmitting(true);
        try {
            const run = await startNativeRun({
                task,
                backend,
                model,
                effort,
                tmuxSessionName: defaults.tmuxSessionName,
                workerPaneId: state.workerPaneId,
                focus: true,
                silent: true,
            });
            await updateTmuxDashboardState(repoRoot, defaults.tmuxSessionName, { selectedRunId: run.id, backend, model, effort });
            setTaskInput("");
            setNotice("");
        }
        catch (error) {
            setNotice(error instanceof Error ? error.message : String(error));
        }
        finally {
            setSubmitting(false);
        }
    }
    async function startPlanner(task) {
        const state = await loadTmuxDashboardState(repoRoot, defaults.tmuxSessionName);
        if (!state) {
            setNotice("Rudder tmux state is missing. Reopen rudder.");
            return;
        }
        setSubmitting(true);
        try {
            const run = await startNativePlan({
                task,
                backend,
                model,
                effort,
                tmuxSessionName: defaults.tmuxSessionName,
                workerPaneId: state.workerPaneId,
                focus: true,
                silent: true,
            });
            await updateTmuxDashboardState(repoRoot, defaults.tmuxSessionName, { selectedRunId: run.id, backend, model, effort });
            setTaskInput("");
            setNotice("Read-only planner started");
        }
        catch (error) {
            setNotice(error instanceof Error ? error.message : String(error));
        }
        finally {
            setSubmitting(false);
        }
    }
    async function syncSelectedRun(runId) {
        const state = await loadTmuxDashboardState(repoRoot, defaults.tmuxSessionName);
        const targetRunId = runId || state?.selectedRunId;
        if (!targetRunId) {
            setNotice("Usage: /sync <run>");
            return;
        }
        setSubmitting(true);
        try {
            const synced = await syncRun(targetRunId, { silent: true });
            setTaskInput("");
            setNotice(syncNotice(synced));
        }
        catch (error) {
            setNotice(error instanceof Error ? error.message : String(error));
        }
        finally {
            setSubmitting(false);
        }
    }
    useInput((chunk, key) => {
        if (key.ctrl && chunk === "c") {
            void detachClient(defaults.tmuxSessionName);
            return;
        }
        if (modelPickerOpen) {
            if (key.escape) {
                if (modelPickerStep === "effort") {
                    setModelPickerStep("model");
                    setPendingModel(null);
                }
                else {
                    setModelPickerOpen(false);
                }
                setNotice("");
                return;
            }
            const activeBackend = toNativeBackend(pendingModel?.backend ?? backend);
            const effortOptions = effortOptionsFor(activeBackend);
            if (key.upArrow || chunk === "k") {
                if (modelPickerStep === "effort") {
                    setEffortIndex((current) => Math.max(0, current - 1));
                }
                else {
                    setModelIndex((current) => Math.max(0, current - 1));
                }
                return;
            }
            if (key.downArrow || chunk === "j") {
                if (modelPickerStep === "effort") {
                    setEffortIndex((current) => Math.min(effortOptions.length - 1, current + 1));
                }
                else {
                    setModelIndex((current) => Math.min(modelOptions.length - 1, current + 1));
                }
                return;
            }
            if (key.return) {
                if (modelPickerStep === "model") {
                    const option = modelOptions[modelIndex] ?? { label: "Default", value: undefined, backend };
                    const nextBackend = toNativeBackend(option.backend ?? backend);
                    const currentEffort = effortForBackend(nextBackend, config);
                    const nextEffortOptions = effortOptionsFor(nextBackend);
                    setPendingModel(option);
                    setEffortIndex(Math.max(0, nextEffortOptions.findIndex((candidate) => candidate.value === currentEffort)));
                    setModelPickerStep("effort");
                    return;
                }
                const option = pendingModel ?? modelOptions[modelIndex] ?? { label: "Default", value: undefined, backend };
                const nextBackend = toNativeBackend(option.backend ?? backend);
                const nextModel = option.value;
                const nextEffort = effortOptions[effortIndex]?.value;
                setBackend(nextBackend);
                setModel(nextModel);
                setEffort(nextEffort);
                void updateBackendDefaults(repoRoot, defaults.tmuxSessionName, nextBackend, nextModel, nextEffort, {
                    updateModel: true,
                    updateEffort: true,
                });
                setModelPickerOpen(false);
                setModelPickerStep("model");
                setPendingModel(null);
                setTaskInput("");
                setNotice("");
                return;
            }
            return;
        }
        const returnIndex = chunk.search(/[\r\n]/);
        if (returnIndex >= 0 && !key.ctrl && !key.meta) {
            const beforeReturn = stripControlInput(chunk.slice(0, returnIndex));
            if (beforeReturn) {
                editTaskInput((current) => current + beforeReturn);
            }
            void submit();
            return;
        }
        if (isLineClear(chunk, key)) {
            editTaskInput("");
            setNotice("");
            return;
        }
        if (isWordDelete(chunk, key)) {
            editTaskInput((current) => deletePreviousWord(current));
            return;
        }
        if (taskHistoryIndexRef.current !== null && key.upArrow) {
            showTaskHistory("previous");
            return;
        }
        if (taskHistoryIndexRef.current !== null && key.downArrow) {
            showTaskHistory("next");
            return;
        }
        if (commandMenuOpen) {
            if (key.upArrow || chunk === "k") {
                setCommandIndex((current) => Math.max(0, current - 1));
                return;
            }
            if (key.downArrow || chunk === "j") {
                setCommandIndex((current) => Math.min(commandOptions.length - 1, current + 1));
                return;
            }
            if (key.escape) {
                editTaskInput("");
                setNotice("");
                return;
            }
        }
        if (key.return) {
            if (commandMenuOpen) {
                const command = commandOptions[commandIndex];
                if (command?.complete) {
                    editTaskInput(command.complete);
                    setNotice("");
                    return;
                }
                if (command) {
                    void submit(command.value);
                    return;
                }
            }
            void submit();
            return;
        }
        if (key.upArrow) {
            showTaskHistory("previous");
            return;
        }
        if (key.downArrow) {
            showTaskHistory("next");
            return;
        }
        if (key.backspace || key.delete || chunk === "\u007f" || chunk === "\b") {
            editTaskInput((current) => current.slice(0, -1));
            return;
        }
        if (chunk && !key.ctrl && !key.meta) {
            editTaskInput((current) => current + chunk);
        }
    });
    const configured = model || (backend === "claude" ? config?.backends.claude?.model : config?.backends.codex?.model) || "default";
    const configuredEffort = effort || effortForBackend(backend, config) || "auto";
    const entryMode = planMode ? "plan" : "run";
    return (_jsxs(Box, { flexDirection: "column", children: [_jsxs(Text, { children: [_jsx(Text, { color: "cyan", bold: true, children: "TASK" }), " ", input, _jsx(Text, { color: "cyan", children: "_" }), submitting ? _jsx(Text, { color: "gray", children: "  starting..." }) : null, !submitting && notice ? _jsxs(Text, { color: "yellow", children: ["  ", notice] }) : null] }), commandMenuOpen ? (_jsx(CommandMenu, { commands: commandOptions, selected: commandIndex })) : modelPickerOpen ? (modelPickerStep === "effort"
                ? _jsx(EffortMenu, { option: pendingModel ?? modelOptions[modelIndex], selected: effortIndex, backend: toNativeBackend(pendingModel?.backend ?? backend), options: effortOptionsFor(toNativeBackend(pendingModel?.backend ?? backend)) })
                : _jsx(ModelMenu, { options: modelOptions, selected: modelIndex, backend: backend })) : (_jsxs(Text, { color: "gray", children: ["Enter ", entryMode, "  Up/Down history  Tab focus pane  /plan  /run  ", backend, " ", configured, " ", configuredEffort] }))] }));
}
function CommandMenu({ commands, selected }) {
    const command = commands[Math.max(0, Math.min(selected, commands.length - 1))];
    return _jsx(Text, { color: "cyan", children: command ? `> ${command.label}  ${command.detail}` : "No command" });
}
function ModelMenu({ options, selected, backend }) {
    const start = Math.max(0, Math.min(selected - 2, Math.max(0, options.length - 7)));
    const visible = options.slice(start, start + 7);
    return (_jsxs(Box, { flexDirection: "column", children: [_jsx(Text, { color: "gray", children: "Pick a model. Claude and Codex are both listed." }), visible.map((option, index) => {
                const absoluteIndex = start + index;
                const optionBackend = toNativeBackend(option.backend ?? backend);
                return (_jsxs(Text, { color: absoluteIndex === selected ? "cyan" : "gray", children: [absoluteIndex === selected ? "> " : "  ", _jsx(Text, { color: optionBackend === "claude" ? "cyan" : "green", children: optionBackend }), "  ", option.label, option.detail ? `  ${option.detail}` : ""] }, `${optionBackend}-${option.value ?? "default"}-${absoluteIndex}`));
            })] }));
}
function EffortMenu({ option, selected, backend, options }) {
    const modelName = option?.label ?? "Default";
    return (_jsxs(Box, { flexDirection: "column", children: [_jsxs(Text, { color: "gray", children: ["Pick effort for ", backend, " ", modelName, ". Esc goes back."] }), options.map((candidate, index) => (_jsxs(Text, { color: index === selected ? "cyan" : "gray", children: [index === selected ? "> " : "  ", _jsx(Text, { color: candidate.value === "xhigh" || candidate.value === "max" ? "yellow" : undefined, children: candidate.label }), candidate.detail ? `  ${candidate.detail}` : ""] }, candidate.value ?? "auto")))] }));
}
function WorkerIdle(_props) {
    return _jsx(Box, {});
}
function toNativeBackend(backend) {
    return backend === "codex" ? "codex" : "claude";
}
async function loadAgentPaneRuns(repoRoot) {
    const runs = await listRuns(repoRoot);
    return await Promise.all(runs.map(async (run) => {
        const output = await readTailIfExists(outputPath(repoRoot, run.id));
        return {
            ...run,
            attention: permissionAttentionFromOutput(output),
        };
    }));
}
async function readTailIfExists(file) {
    const handle = await fsp.open(file, "r").catch(() => null);
    if (!handle) {
        return "";
    }
    try {
        const stat = await handle.stat();
        const length = Math.min(stat.size, ATTENTION_TAIL_BYTES);
        const buffer = Buffer.alloc(length);
        await handle.read(buffer, 0, length, Math.max(0, stat.size - length));
        return buffer.toString("utf8");
    }
    catch {
        return "";
    }
    finally {
        await handle.close().catch(() => undefined);
    }
}
async function focusSelectedWorker(repoRoot, tmuxSessionName, run) {
    if (run?.terminal?.paneId) {
        await selectPane(run.terminal.paneId).catch(() => undefined);
        return;
    }
    const state = await loadTmuxDashboardState(repoRoot, tmuxSessionName);
    if (state?.workerPaneId) {
        await selectPane(state.workerPaneId).catch(() => undefined);
    }
}
function notifyRunAlerts(runs, ref) {
    const terminal = new Set(runs.filter((run) => isTerminalRun(run)).map((run) => run.id));
    const permission = new Set(runs.filter((run) => runNeedsPermission(run)).map((run) => run.id));
    if (!ref.current) {
        ref.current = { terminal, permission };
        return;
    }
    for (const run of runs) {
        if (isTerminalRun(run) && !ref.current.terminal.has(run.id)) {
            playCompletionSound();
            ref.current.terminal.add(run.id);
        }
        if (runNeedsPermission(run) && !ref.current.permission.has(run.id)) {
            playCompletionSound();
            ref.current.permission.add(run.id);
        }
    }
    ref.current.terminal = terminal;
    ref.current.permission = permission;
}
function isTerminalRun(run) {
    return ["completed", "failed", "cancelled", "merged", "merge-conflict"].includes(run.status);
}
function isActiveRun(run) {
    return ["created", "running", "steering", "verifying"].includes(run.status);
}
function runNeedsPermission(run) {
    return isActiveRun(run) && run.attention.needsPermission;
}
function playCompletionSound() {
    try {
        const player = process.platform === "darwin" ? "afplay" : "ffplay";
        const args = process.platform === "darwin"
            ? [COMPLETION_SOUND]
            : ["-nodisp", "-autoexit", "-loglevel", "quiet", COMPLETION_SOUND];
        const child = spawn(player, args, { detached: true, stdio: "ignore" });
        child.on("error", () => process.stdout.write("\u0007"));
        child.unref();
    }
    catch {
        process.stdout.write("\u0007");
    }
}
function statusMark(run) {
    if (runNeedsPermission(run))
        return "needs permission";
    if (run.mode === "plan" && isActiveRun(run))
        return "planning";
    if (run.status === "merged")
        return "merged";
    if (run.status === "completed")
        return "done";
    if (run.status === "failed" || run.status === "merge-conflict")
        return "failed";
    if (run.status === "cancelled")
        return "stopped";
    if (run.status === "running" || run.status === "steering" || run.status === "verifying")
        return "running";
    return "queued";
}
function runStatusColor(run) {
    return runNeedsPermission(run) ? "yellow" : statusColor(run);
}
function statusColor(run) {
    if (run.status === "merged" || run.status === "completed")
        return "green";
    if (run.status === "failed" || run.status === "merge-conflict")
        return "red";
    if (run.status === "cancelled")
        return "yellow";
    if (run.status === "running" || run.status === "steering" || run.status === "verifying")
        return "yellow";
    return "gray";
}
function taskColor(run) {
    return undefined;
}
function modelLabel(run, config) {
    const model = run.model
        ?? (run.backend === "claude"
            ? config?.backends.claude?.model
            : config?.backends.codex?.model)
        ?? "default";
    const effort = run.effort ?? effortForBackend(toNativeBackend(run.backend), config);
    const mode = run.mode === "plan" ? "plan " : "";
    return summarize(`${mode}${model} ${effort ?? "auto"}`, 18);
}
function modelForBackend(backend, config) {
    return backend === "claude" ? config?.backends.claude?.model : config?.backends.codex?.model;
}
function effortForBackend(backend, config) {
    if (backend === "claude") {
        return config?.backends.claude?.effort;
    }
    return config?.backends.codex?.reasoningEffort ?? config?.backends.codex?.effort;
}
async function updateBackendDefaults(repoRoot, tmuxSessionName, backend, model, effort, options) {
    await updateTmuxDashboardState(repoRoot, tmuxSessionName, { backend, model, effort });
    await rememberBackendSelection({
        backend,
        model,
        effort,
        updateModel: options.updateModel,
        updateEffort: options.updateEffort,
    });
}
function withBackend(options, backend) {
    return options.map((option) => ({ ...option, backend }));
}
function filterSlashCommands(input) {
    if (!input.startsWith("/")) {
        return [];
    }
    const query = input.toLowerCase();
    const matches = SLASH_COMMANDS.filter((command) => command.label.toLowerCase().startsWith(query));
    return matches.length ? matches : SLASH_COMMANDS.filter((command) => command.label.toLowerCase().includes(query.slice(1)));
}
function isExactRunnableCommand(input) {
    const trimmed = input.trim();
    return SLASH_COMMANDS.some((command) => !command.complete && command.value === trimmed);
}
function resolveSlashCommand(input) {
    if (!input.startsWith("/") || input.startsWith("/model ") || input.startsWith("/plan ") || input.startsWith("/run ") || input.startsWith("/sync ")) {
        return undefined;
    }
    if (isExactRunnableCommand(input)) {
        return undefined;
    }
    return filterSlashCommands(input)[0];
}
function isLineClear(chunk, key) {
    return (key.ctrl && chunk === "u") || chunk === "\u0015" || chunk === "\u001b\u0015";
}
function isWordDelete(chunk, key) {
    return Boolean((key.ctrl && chunk === "w") ||
        chunk === "\u0017" ||
        chunk === "\u001b\u007f" ||
        chunk === "\u001b\b" ||
        (key.meta && (key.backspace || key.delete || chunk === "\u007f" || chunk === "\b")));
}
function deletePreviousWord(value) {
    return value.trimEnd().replace(/\s*\S+$/, "");
}
function stripControlInput(value) {
    return value.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "");
}
function shortId(id) {
    return id.slice(0, 14);
}
function syncNotice(run) {
    if (run.sync?.status === "synced") {
        return `synced ${shortId(run.id)}`;
    }
    if (run.sync?.status === "conflict") {
        const count = run.sync.conflictedFiles?.length || "unknown";
        return `sync conflict ${shortId(run.id)} (${count} file${count === 1 ? "" : "s"}); resolve in worktree and run git rebase --continue`;
    }
    return `sync failed ${shortId(run.id)}`;
}
function summarize(value, width) {
    const clean = value.replace(/\s+/g, " ").trim();
    if (clean.length <= width) {
        return clean;
    }
    return `${clean.slice(0, Math.max(0, width - 1))}…`;
}
//# sourceMappingURL=tmux-dashboard.js.map