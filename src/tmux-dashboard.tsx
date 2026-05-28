import { spawn } from "node:child_process";
import fsp from "node:fs/promises";
import { fileURLToPath } from "node:url";
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Box, Text, render, useInput, useWindowSize } from "ink";
import { permissionAttentionFromOutput, type AgentAttention } from "./agent-attention.js";
import { discoverEffortOptions, fallbackEffortOptions, type EffortOption } from "./effort.js";
import { currentBranch, findRepoRoot } from "./git.js";
import { discoverModelOptions, fallbackModelOptions, type ModelOption } from "./models.js";
import { startNativePlan, startNativeRun, deleteRun, mergeRun, reconcileNativeTerminals, stopRun, syncRun } from "./run-manager.js";
import { listRuns, loadConfig, outputPath, rememberBackendSelection } from "./state.js";
import {
  loadTmuxDashboardState,
  updateTmuxDashboardState,
  type NativeBackendId,
} from "./tmux-state.js";
import { detachClient, resizePane, selectPane } from "./tmux.js";
import type { BackendId, EffortLevel, RunRecord, RudderConfig } from "./types.js";
import { taskDisplayLabel } from "./task-summary.js";
import { shortenHome } from "./util.js";

const COMPLETION_SOUND = fileURLToPath(new URL("../assets/sounds/ping.mp3", import.meta.url));
const ATTENTION_TAIL_BYTES = 64 * 1024;
const TASK_HISTORY_LIMIT = 100;

type PaneDefaults = {
  tmuxSessionName: string;
  backend?: BackendId;
  model?: string;
};

type AgentPaneRun = RunRecord & {
  attention: AgentAttention;
};

type AlertState = {
  terminal: Set<string>;
  permission: Set<string>;
};

type SlashCommand = {
  label: string;
  detail: string;
  value: string;
  complete?: string;
};

const SLASH_COMMANDS: SlashCommand[] = [
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

export async function runTmuxAgentPane(defaults: PaneDefaults): Promise<void> {
  const instance = render(<AgentPane defaults={defaults} />, {
    exitOnCtrlC: false,
    maxFps: 20,
  });
  await instance.waitUntilExit();
}

export async function runTmuxTaskPane(defaults: PaneDefaults): Promise<void> {
  const instance = render(<TaskPane defaults={defaults} />, {
    exitOnCtrlC: false,
    maxFps: 30,
  });
  await instance.waitUntilExit();
}

export async function runTmuxWorkerIdle(defaults: PaneDefaults): Promise<void> {
  const instance = render(<WorkerIdle defaults={defaults} />, {
    exitOnCtrlC: false,
    maxFps: 10,
  });
  await instance.waitUntilExit();
}

function AgentPane({ defaults }: { defaults: PaneDefaults }): React.ReactElement {
  const size = useWindowSize();
  const [repoRoot, setRepoRoot] = useState(() => findRepoRoot());
  const [branch, setBranch] = useState("HEAD");
  const [config, setConfig] = useState<RudderConfig | null>(null);
  const [runs, setRuns] = useState<AgentPaneRun[]>([]);
  const [selectedRunId, setSelectedRunId] = useState<string | undefined>();
  const [notice, setNotice] = useState("");
  const [deleteIntent, setDeleteIntent] = useState<{ runId: string } | null>(null);
  const alertRef = useRef<AlertState | null>(null);

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

  const selectRun = useCallback(async (run: RunRecord | undefined) => {
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
          } else if (merged.merge?.status === "merged") {
            setNotice(`merged ${shortId(selectedRun.id)}`);
          } else {
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

  return (
    <Box flexDirection="column">
      <Text bold>rudder</Text>
      <Text color="gray">{summarize(`${shortenHome(repoRoot)} ${branch}`, width)}</Text>
      <Text>agents <Text color="gray">{runs.length} runs</Text></Text>
      {visibleRuns.length === 0 ? <Text color="gray">No agents yet.</Text> : visibleRuns.map((run) => (
        <Box key={run.id} flexDirection="column">
          <Text color={run.id === selectedRun?.id ? "cyan" : taskColor(run)}>
            {run.id === selectedRun?.id ? "> " : "  "}{summarize(taskDisplayLabel(run, 80), width - 3)}
          </Text>
          <Text>
            <Text color={runStatusColor(run)}>  {statusMark(run)}</Text>
            <Text color="gray">  {run.backend} </Text>
            <Text color="magenta">{modelLabel(run, config)}</Text>
          </Text>
        </Box>
      ))}
      {notice ? <Text color={deleteIntent ? "red" : "yellow"}>{summarize(notice, width)}</Text> : null}
      <Text color="gray">j/k select  Enter focus  u sync  m merge  dd delete</Text>
    </Box>
  );
}

function TaskPane({ defaults }: { defaults: PaneDefaults }): React.ReactElement {
  const [repoRoot, setRepoRoot] = useState(() => findRepoRoot());
  const [config, setConfig] = useState<RudderConfig | null>(null);
  const [backend, setBackend] = useState<NativeBackendId>(toNativeBackend(defaults.backend ?? "claude"));
  const [model, setModel] = useState<string | undefined>(defaults.model);
  const [effort, setEffort] = useState<EffortLevel | undefined>();
  const [input, setInput] = useState("");
  const [planMode, setPlanMode] = useState(false);
  const inputRef = useRef("");
  const taskHistoryRef = useRef<string[]>([]);
  const taskHistoryIndexRef = useRef<number | null>(null);
  const taskHistoryDraftRef = useRef("");
  const [notice, setNotice] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [commandIndex, setCommandIndex] = useState(0);
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  const [modelPickerStep, setModelPickerStep] = useState<"model" | "effort">("model");
  const [modelIndex, setModelIndex] = useState(0);
  const [effortIndex, setEffortIndex] = useState(0);
  const [pendingModel, setPendingModel] = useState<ModelOption | null>(null);
  const [claudeModels, setClaudeModels] = useState<ModelOption[]>([]);
  const [codexModels, setCodexModels] = useState<ModelOption[]>([]);
  const [claudeEfforts, setClaudeEfforts] = useState<EffortOption[]>([]);
  const [codexEfforts, setCodexEfforts] = useState<EffortOption[]>([]);
  const [taskPaneId, setTaskPaneId] = useState<string | undefined>();

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
  const modelOptions = useMemo(
    () => {
      const primary = backend === "claude"
        ? withBackend(claudeModels.length ? claudeModels : fallbackModelOptions("claude", claudeDefault), "claude")
        : withBackend(codexModels.length ? codexModels : fallbackModelOptions("codex", codexDefault), "codex");
      const secondary = backend === "claude"
        ? withBackend(codexModels.length ? codexModels : fallbackModelOptions("codex", codexDefault), "codex")
        : withBackend(claudeModels.length ? claudeModels : fallbackModelOptions("claude", claudeDefault), "claude");
      return [...primary, ...secondary];
    },
    [backend, claudeDefault, claudeModels, codexDefault, codexModels],
  );
  const effortOptionsFor = useCallback((nextBackend: NativeBackendId) => (
    nextBackend === "claude"
      ? (claudeEfforts.length ? claudeEfforts : fallbackEffortOptions("claude"))
      : (codexEfforts.length ? codexEfforts : fallbackEffortOptions("codex"))
  ), [claudeEfforts, codexEfforts]);

  const setTaskInput = useCallback((next: string | ((current: string) => string)) => {
    const value = typeof next === "function" ? next(inputRef.current) : next;
    inputRef.current = value;
    setInput(value);
  }, []);

  const resetTaskHistoryNavigation = useCallback(() => {
    taskHistoryIndexRef.current = null;
    taskHistoryDraftRef.current = "";
  }, []);

  const editTaskInput = useCallback((next: string | ((current: string) => string)) => {
    resetTaskHistoryNavigation();
    setTaskInput(next);
  }, [resetTaskHistoryNavigation, setTaskInput]);

  const rememberTaskHistory = useCallback((value: string) => {
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

  const showTaskHistory = useCallback((direction: "previous" | "next") => {
    const history = taskHistoryRef.current;
    if (!history.length) {
      return false;
    }

    if (direction === "previous") {
      const current = taskHistoryIndexRef.current;
      if (current === null) {
        taskHistoryDraftRef.current = inputRef.current;
        taskHistoryIndexRef.current = history.length - 1;
      } else {
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

  const submit = useCallback(async (override?: string) => {
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

  async function startWorker(task: string): Promise<void> {
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
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setSubmitting(false);
    }
  }

  async function startPlanner(task: string): Promise<void> {
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
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setSubmitting(false);
    }
  }

  async function syncSelectedRun(runId: string): Promise<void> {
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
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    } finally {
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
        } else {
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
        } else {
          setModelIndex((current) => Math.max(0, current - 1));
        }
        return;
      }
      if (key.downArrow || chunk === "j") {
        if (modelPickerStep === "effort") {
          setEffortIndex((current) => Math.min(effortOptions.length - 1, current + 1));
        } else {
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

  return (
    <Box flexDirection="column">
      <Text>
        <Text color="cyan" bold>TASK</Text> {input}<Text color="cyan">_</Text>
        {submitting ? <Text color="gray">  starting...</Text> : null}
        {!submitting && notice ? <Text color="yellow">  {notice}</Text> : null}
      </Text>
      {commandMenuOpen ? (
        <CommandMenu commands={commandOptions} selected={commandIndex} />
      ) : modelPickerOpen ? (
        modelPickerStep === "effort"
          ? <EffortMenu option={pendingModel ?? modelOptions[modelIndex]} selected={effortIndex} backend={toNativeBackend(pendingModel?.backend ?? backend)} options={effortOptionsFor(toNativeBackend(pendingModel?.backend ?? backend))} />
          : <ModelMenu options={modelOptions} selected={modelIndex} backend={backend} />
      ) : (
        <Text color="gray">Enter {entryMode}  Up/Down history  Tab focus pane  /plan  /run  {backend} {configured} {configuredEffort}</Text>
      )}
    </Box>
  );
}

function CommandMenu({ commands, selected }: { commands: SlashCommand[]; selected: number }): React.ReactElement {
  const command = commands[Math.max(0, Math.min(selected, commands.length - 1))];
  return <Text color="cyan">{command ? `> ${command.label}  ${command.detail}` : "No command"}</Text>;
}

function ModelMenu({ options, selected, backend }: { options: ModelOption[]; selected: number; backend: NativeBackendId }): React.ReactElement {
  const start = Math.max(0, Math.min(selected - 2, Math.max(0, options.length - 7)));
  const visible = options.slice(start, start + 7);
  return (
    <Box flexDirection="column">
      <Text color="gray">Pick a model. Claude and Codex are both listed.</Text>
      {visible.map((option, index) => {
        const absoluteIndex = start + index;
        const optionBackend = toNativeBackend(option.backend ?? backend);
        return (
          <Text key={`${optionBackend}-${option.value ?? "default"}-${absoluteIndex}`} color={absoluteIndex === selected ? "cyan" : "gray"}>
            {absoluteIndex === selected ? "> " : "  "}
            <Text color={optionBackend === "claude" ? "cyan" : "green"}>{optionBackend}</Text>
            {"  "}{option.label}{option.detail ? `  ${option.detail}` : ""}
          </Text>
        );
      })}
    </Box>
  );
}

function EffortMenu({ option, selected, backend, options }: { option?: ModelOption; selected: number; backend: NativeBackendId; options: EffortOption[] }): React.ReactElement {
  const modelName = option?.label ?? "Default";
  return (
    <Box flexDirection="column">
      <Text color="gray">Pick effort for {backend} {modelName}. Esc goes back.</Text>
      {options.map((candidate, index) => (
        <Text key={candidate.value ?? "auto"} color={index === selected ? "cyan" : "gray"}>
          {index === selected ? "> " : "  "}
          <Text color={candidate.value === "xhigh" || candidate.value === "max" ? "yellow" : undefined}>{candidate.label}</Text>
          {candidate.detail ? `  ${candidate.detail}` : ""}
        </Text>
      ))}
    </Box>
  );
}

function WorkerIdle(_props: { defaults: PaneDefaults }): React.ReactElement {
  return <Box />;
}

function toNativeBackend(backend: BackendId): NativeBackendId {
  return backend === "codex" ? "codex" : "claude";
}

async function loadAgentPaneRuns(repoRoot: string): Promise<AgentPaneRun[]> {
  const runs = await listRuns(repoRoot);
  return await Promise.all(runs.map(async (run) => {
    const output = await readTailIfExists(outputPath(repoRoot, run.id));
    return {
      ...run,
      attention: permissionAttentionFromOutput(output),
    };
  }));
}

async function readTailIfExists(file: string): Promise<string> {
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
  } catch {
    return "";
  } finally {
    await handle.close().catch(() => undefined);
  }
}

async function focusSelectedWorker(repoRoot: string, tmuxSessionName: string, run: RunRecord | undefined): Promise<void> {
  if (run?.terminal?.paneId) {
    await selectPane(run.terminal.paneId).catch(() => undefined);
    return;
  }
  const state = await loadTmuxDashboardState(repoRoot, tmuxSessionName);
  if (state?.workerPaneId) {
    await selectPane(state.workerPaneId).catch(() => undefined);
  }
}

function notifyRunAlerts(runs: AgentPaneRun[], ref: React.MutableRefObject<AlertState | null>): void {
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

function isTerminalRun(run: RunRecord): boolean {
  return ["completed", "failed", "cancelled", "merged", "merge-conflict"].includes(run.status);
}

function isActiveRun(run: RunRecord): boolean {
  return ["created", "running", "steering", "verifying"].includes(run.status);
}

function runNeedsPermission(run: AgentPaneRun): boolean {
  return isActiveRun(run) && run.attention.needsPermission;
}

function playCompletionSound(): void {
  try {
    const player = process.platform === "darwin" ? "afplay" : "ffplay";
    const args = process.platform === "darwin"
      ? [COMPLETION_SOUND]
      : ["-nodisp", "-autoexit", "-loglevel", "quiet", COMPLETION_SOUND];
    const child = spawn(player, args, { detached: true, stdio: "ignore" });
    child.on("error", () => process.stdout.write("\u0007"));
    child.unref();
  } catch {
    process.stdout.write("\u0007");
  }
}

function statusMark(run: AgentPaneRun): string {
  if (runNeedsPermission(run)) return "needs permission";
  if (run.mode === "plan" && isActiveRun(run)) return "planning";
  if (run.status === "merged") return "merged";
  if (run.status === "completed") return "done";
  if (run.status === "failed" || run.status === "merge-conflict") return "failed";
  if (run.status === "cancelled") return "stopped";
  if (run.status === "running" || run.status === "steering" || run.status === "verifying") return "running";
  return "queued";
}

function runStatusColor(run: AgentPaneRun): string {
  return runNeedsPermission(run) ? "yellow" : statusColor(run);
}

function statusColor(run: RunRecord): string {
  if (run.status === "merged" || run.status === "completed") return "green";
  if (run.status === "failed" || run.status === "merge-conflict") return "red";
  if (run.status === "cancelled") return "yellow";
  if (run.status === "running" || run.status === "steering" || run.status === "verifying") return "yellow";
  return "gray";
}

function taskColor(run: RunRecord): string | undefined {
  return undefined;
}

function modelLabel(run: RunRecord, config: RudderConfig | null): string {
  const model = run.model
    ?? (run.backend === "claude"
      ? config?.backends.claude?.model
      : config?.backends.codex?.model)
    ?? "default";
  const effort = run.effort ?? effortForBackend(toNativeBackend(run.backend), config);
  const mode = run.mode === "plan" ? "plan " : "";
  return summarize(`${mode}${model} ${effort ?? "auto"}`, 18);
}

function modelForBackend(backend: NativeBackendId, config: RudderConfig | null): string | undefined {
  return backend === "claude" ? config?.backends.claude?.model : config?.backends.codex?.model;
}

function effortForBackend(backend: NativeBackendId, config: RudderConfig | null): EffortLevel | undefined {
  if (backend === "claude") {
    return config?.backends.claude?.effort;
  }
  return config?.backends.codex?.reasoningEffort ?? config?.backends.codex?.effort;
}

async function updateBackendDefaults(
  repoRoot: string,
  tmuxSessionName: string,
  backend: NativeBackendId,
  model: string | undefined,
  effort: EffortLevel | undefined,
  options: {
    updateModel: boolean;
    updateEffort: boolean;
  },
): Promise<void> {
  await updateTmuxDashboardState(repoRoot, tmuxSessionName, { backend, model, effort });
  await rememberBackendSelection({
    backend,
    model,
    effort,
    updateModel: options.updateModel,
    updateEffort: options.updateEffort,
  });
}

function withBackend(options: ModelOption[], backend: NativeBackendId): ModelOption[] {
  return options.map((option) => ({ ...option, backend }));
}

function filterSlashCommands(input: string): SlashCommand[] {
  if (!input.startsWith("/")) {
    return [];
  }
  const query = input.toLowerCase();
  const matches = SLASH_COMMANDS.filter((command) => command.label.toLowerCase().startsWith(query));
  return matches.length ? matches : SLASH_COMMANDS.filter((command) => command.label.toLowerCase().includes(query.slice(1)));
}

function isExactRunnableCommand(input: string): boolean {
  const trimmed = input.trim();
  return SLASH_COMMANDS.some((command) => !command.complete && command.value === trimmed);
}

function resolveSlashCommand(input: string): SlashCommand | undefined {
  if (!input.startsWith("/") || input.startsWith("/model ") || input.startsWith("/plan ") || input.startsWith("/run ") || input.startsWith("/sync ")) {
    return undefined;
  }
  if (isExactRunnableCommand(input)) {
    return undefined;
  }
  return filterSlashCommands(input)[0];
}

function isLineClear(chunk: string, key: { ctrl?: boolean; meta?: boolean; backspace?: boolean; delete?: boolean }): boolean {
  return (key.ctrl && chunk === "u") || chunk === "\u0015" || chunk === "\u001b\u0015";
}

function isWordDelete(chunk: string, key: { ctrl?: boolean; meta?: boolean; backspace?: boolean; delete?: boolean }): boolean {
  return Boolean(
    (key.ctrl && chunk === "w") ||
    chunk === "\u0017" ||
    chunk === "\u001b\u007f" ||
    chunk === "\u001b\b" ||
    (key.meta && (key.backspace || key.delete || chunk === "\u007f" || chunk === "\b"))
  );
}

function deletePreviousWord(value: string): string {
  return value.trimEnd().replace(/\s*\S+$/, "");
}

function stripControlInput(value: string): string {
  return value.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "");
}

function shortId(id: string): string {
  return id.slice(0, 14);
}

function syncNotice(run: RunRecord): string {
  if (run.sync?.status === "synced") {
    return `synced ${shortId(run.id)}`;
  }
  if (run.sync?.status === "conflict") {
    const count = run.sync.conflictedFiles?.length || "unknown";
    return `sync conflict ${shortId(run.id)} (${count} file${count === 1 ? "" : "s"}); resolve in worktree and run git rebase --continue`;
  }
  return `sync failed ${shortId(run.id)}`;
}

function summarize(value: string, width: number): string {
  const clean = value.replace(/\s+/g, " ").trim();
  if (clean.length <= width) {
    return clean;
  }
  return `${clean.slice(0, Math.max(0, width - 1))}…`;
}
