import fsp from "node:fs/promises";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Box, Text, render, useApp, useInput, useWindowSize } from "ink";
import { permissionAttentionFromOutput, type AgentAttention } from "./agent-attention.js";
import { currentBranch, findRepoRoot } from "./git.js";
import { discoverModelOptions, fallbackModelOptions, type ModelOption } from "./models.js";
import {
  eventsPath,
  listRuns,
  loadConfig,
  outputPath,
  rememberBackendSelection,
} from "./state.js";
import { continueRun, deleteRun, mergeRun, startRun, stopRun, syncRun } from "./run-manager.js";
import { taskDisplayLabel } from "./task-summary.js";
import type { BackendId, RunRecord, RudderConfig, RudderEvent, RunStatus } from "./types.js";
import { pathExists, shortenHome } from "./util.js";

type TuiDefaults = {
  backend?: BackendId;
  model?: string;
  worktree?: boolean;
  detach?: boolean;
};

type UiRun = RunRecord & {
  output: string;
  events: RudderEvent[];
  work: WorkItem[];
  attention: AgentAttention;
};

type WorkItem = {
  label: string;
  detail?: string;
  tone: "muted" | "info" | "success" | "warning" | "danger";
};

type FocusPane = "agents" | "worker" | "task";

type DeletePrompt = {
  runId: string;
};

type MergePrompt =
  | { kind: "selected"; runId: string; label: string; allowDirty: boolean }
  | { kind: "all"; runIds: string[]; allowDirty: boolean };

type MergeConflictPrompt = {
  runId: string;
  files: string[];
};

type AlertState = {
  terminal: Set<string>;
  permission: Set<string>;
};

const INTERACTIVE_BACKENDS: BackendId[] = ["claude", "codex"];
const COMPLETION_SOUND = fileURLToPath(new URL("../assets/sounds/ping.mp3", import.meta.url));

type CommandOption = {
  name: string;
  detail: string;
  insert: string;
};

const COMMANDS: CommandOption[] = [
  { name: "backend", detail: "switch backend: claude or codex", insert: "/backend " },
  { name: "model", detail: "open model picker or set a custom model id", insert: "/model" },
  { name: "agent", detail: "send your next input to the selected agent", insert: "/agent" },
  { name: "interrupt", detail: "interrupt and redirect the selected running agent", insert: "/interrupt" },
  { name: "new", detail: "return to new-agent mode", insert: "/new" },
  { name: "worktree", detail: "toggle worktree policy", insert: "/worktree " },
  { name: "stop", detail: "stop the selected run", insert: "/stop" },
  { name: "delete", detail: "delete selected run and its worktree", insert: "/delete" },
  { name: "copy", detail: "copy selected worker transcript", insert: "/copy" },
  { name: "sync", detail: "rebase the selected worktree without merging", insert: "/sync" },
  { name: "merge", detail: "merge the selected completed worktree", insert: "/merge" },
  { name: "merge-all", detail: "merge all completed worktrees", insert: "/merge-all" },
  { name: "clear", detail: "collapse all run cards", insert: "/clear" },
  { name: "help", detail: "show key and slash command help", insert: "/help" },
  { name: "exit", detail: "quit Rudder", insert: "/exit" },
];

export async function runInteractiveTui(defaults?: TuiDefaults): Promise<void> {
  const instance = render(<RudderTui defaults={defaults ?? {}} />, {
    alternateScreen: true,
    exitOnCtrlC: false,
    maxFps: 60,
  });
  await instance.waitUntilExit();
}

function RudderTui({ defaults }: { defaults: TuiDefaults }): React.ReactElement {
  const app = useApp();
  const size = useWindowSize();
  const [repoRoot, setRepoRoot] = useState(() => findRepoRoot());
  const [branch, setBranch] = useState("HEAD");
  const [config, setConfig] = useState<RudderConfig | null>(null);
  const [backend, setBackend] = useState<BackendId>(toInteractiveBackend(defaults.backend ?? "claude"));
  const [model, setModel] = useState<string | undefined>(defaults.model);
  const [worktreeMode, setWorktreeMode] = useState<"auto" | "always">(defaults.worktree === false ? "auto" : "always");
  const [runs, setRuns] = useState<UiRun[]>([]);
  const [selectedRunId, setSelectedRunId] = useState<string | undefined>();
  const [targetRunId, setTargetRunId] = useState<string | undefined>();
  const [focusPane, setFocusPane] = useState<FocusPane>("task");
  const [expandedRunIds, setExpandedRunIds] = useState<Set<string>>(new Set());
  const [transcriptExpanded, setTranscriptExpanded] = useState(false);
  const [input, setInput] = useState("");
  const [notice, setNotice] = useState("Ready");
  const [helpOpen, setHelpOpen] = useState(false);
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [modelMenuIndex, setModelMenuIndex] = useState(0);
  const [commandMenuIndex, setCommandMenuIndex] = useState(0);
  const [discoveredModels, setDiscoveredModels] = useState<ModelOption[]>([]);
  const [deletePrompt, setDeletePrompt] = useState<DeletePrompt | null>(null);
  const [mergePrompt, setMergePrompt] = useState<MergePrompt | null>(null);
  const [conflictPrompt, setConflictPrompt] = useState<MergeConflictPrompt | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [preferencesLoaded, setPreferencesLoaded] = useState(false);
  const notifiedAlerts = useRef<AlertState | null>(null);

  const refresh = useCallback(async () => {
    const root = findRepoRoot();
    const [nextConfig, nextBranch, nextRuns] = await Promise.all([
      loadConfig(),
      currentBranch(root),
      loadUiRuns(root),
    ]);
    setRepoRoot(root);
    setConfig(nextConfig);
    setBranch(nextBranch);
    notifyRunAlerts(nextRuns, notifiedAlerts);
    setRuns(nextRuns);
    if (!preferencesLoaded) {
      setBackend(toInteractiveBackend(defaults.backend ?? nextConfig.lastUsedBackend ?? nextConfig.defaultBackend));
      setModel(defaults.model);
      setPreferencesLoaded(true);
    }
    setSelectedRunId((current) => current ?? nextRuns[0]?.id);
  }, [defaults.backend, defaults.model, preferencesLoaded]);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => {
      void refresh();
    }, input ? 1500 : 900);
    return () => clearInterval(timer);
  }, [input, refresh]);

  useEffect(() => {
    let cancelled = false;
    const configuredDefault = modelForBackend(backend, config);
    void discoverModelOptions(backend, configuredDefault)
      .then((options) => {
        if (!cancelled) {
          setDiscoveredModels(options);
          setModelMenuIndex(0);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setDiscoveredModels(fallbackModelOptions(backend, configuredDefault));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [backend, config]);

  const selectedIndex = Math.max(0, runs.findIndex((run) => run.id === selectedRunId));
  const selectedRun = runs[selectedIndex];
  const targetRun = focusPane === "worker" && selectedRun ? selectedRun : targetRunId ? runs.find((run) => run.id === targetRunId) : undefined;
  const activeCount = runs.filter((run) => isActive(run.status)).length;
  const selectedExpanded = Boolean(selectedRun && expandedRunIds.has(selectedRun.id));
  const modelOptions = useMemo(
    () => discoveredModels.length ? discoveredModels : fallbackModelOptions(backend, modelForBackend(backend, config)),
    [backend, config, discoveredModels],
  );
  const commandOptions = useMemo(() => filterCommands(input), [input]);
  const commandMenuOpen = input.startsWith("/") && !modelMenuOpen && commandOptions.length > 0;

  const submitTask = useCallback(async (task: string) => {
    const trimmed = task.trim();
    if (!trimmed || submitting) {
      return;
    }
    setSubmitting(true);
    if (targetRun) {
      const interrupt = isActive(targetRun.status);
      setNotice(`${interrupt ? "Interrupting" : "Sending to"} ${shortId(targetRun.id)}...`);
      try {
        const run = await continueRun({
          runId: targetRun.id,
          prompt: trimmed,
          interrupt,
          silent: true,
        });
        setInput("");
        setSelectedRunId(run.id);
        setExpandedRunIds((current) => new Set(current).add(run.id));
        setNotice(`${interrupt ? "Interrupted" : "Sent to"} ${shortId(run.id)}`);
        await refresh();
      } catch (error) {
        setNotice(error instanceof Error ? error.message : String(error));
      } finally {
        setSubmitting(false);
      }
      return;
    }
    setNotice(`Starting ${backend}...`);
    try {
      const run = await startRun({
        task: trimmed,
        backend,
        model,
        detach: true,
        worktree: worktreeMode === "always",
        silent: true,
        view: "shell",
      });
      setInput("");
      setSelectedRunId(run.id);
      setExpandedRunIds((current) => new Set(current).add(run.id));
      setNotice(`Started ${run.id}`);
      await refresh();
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setSubmitting(false);
    }
  }, [backend, model, refresh, submitting, targetRun, worktreeMode]);

  const requestDeleteSelectedRun = useCallback((runOverride?: UiRun) => {
    const run = runOverride ?? selectedRun;
    if (!run) {
      setNotice("No agent selected");
      return;
    }
    setDeletePrompt({ runId: run.id });
    setNotice(`Delete ${shortId(run.id)}? press d to confirm, Esc to cancel`);
  }, [selectedRun]);

  const confirmDelete = useCallback(async () => {
    if (!deletePrompt) {
      return;
    }
    const runId = deletePrompt.runId;
    try {
      await deleteRun(runId, { force: true, silent: true });
      setDeletePrompt(null);
      setSelectedRunId(undefined);
      setTargetRunId(undefined);
      setNotice(`Deleted ${shortId(runId)}`);
      await refresh();
    } catch (error) {
      setDeletePrompt(null);
      setNotice(error instanceof Error ? error.message : String(error));
    }
  }, [deletePrompt, refresh]);

  const requestMergeRun = useCallback((runOverride?: UiRun, allowDirty = false) => {
    const run = runOverride ?? selectedRun;
    if (!run) {
      setNotice("No agent selected");
      return;
    }
    if (!canMerge(run)) {
      setNotice(`${shortId(run.id)} is not ready to merge`);
      return;
    }
    setDeletePrompt(null);
    setConflictPrompt(null);
    setMergePrompt({
      kind: "selected",
      runId: run.id,
      label: truncate(taskDisplayLabel(run, 48), 48),
      allowDirty,
    });
    setNotice(`Merge ${shortId(run.id)}? press y to confirm or n to cancel`);
  }, [selectedRun]);

  const requestMergeAll = useCallback((allowDirty = false) => {
    const ready = runs.filter(canMerge);
    if (ready.length === 0) {
      setNotice("No completed worktree runs ready to merge");
      return;
    }
    setDeletePrompt(null);
    setConflictPrompt(null);
    setMergePrompt({ kind: "all", runIds: ready.map((run) => run.id), allowDirty });
    setNotice(`Merge ${ready.length} run${ready.length === 1 ? "" : "s"}? press y to confirm or n to cancel`);
  }, [runs]);

  const confirmMerge = useCallback(async () => {
    if (!mergePrompt) {
      return;
    }
    const prompt = mergePrompt;
    setMergePrompt(null);
    try {
      if (prompt.kind === "selected") {
        const merged = await mergeRun(prompt.runId, prompt.allowDirty, { silent: true });
        if (merged.merge?.status === "conflict") {
          const files = merged.merge.conflictedFiles ?? [];
          setConflictPrompt({ runId: prompt.runId, files });
          setNotice(`Merge conflict in ${files.length || "unknown"} file${files.length === 1 ? "" : "s"}; press y for AI help or n for manual`);
        } else if (merged.merge?.status === "merged") {
          setNotice(`Merged ${shortId(prompt.runId)}`);
        } else {
          setNotice(`Merge failed for ${shortId(prompt.runId)}: ${merged.merge?.error ?? "unknown error"}`);
        }
        await refresh();
        return;
      }

      let mergedCount = 0;
      for (const runId of prompt.runIds) {
        const merged = await mergeRun(runId, prompt.allowDirty, { silent: true });
        if (merged.merge?.status === "conflict") {
          const files = merged.merge.conflictedFiles ?? [];
          setConflictPrompt({ runId, files });
          setNotice(`Merge all stopped after ${mergedCount}: conflict in ${shortId(runId)}; press y for AI help or n for manual`);
          await refresh();
          return;
        }
        if (merged.merge?.status !== "merged") {
          setNotice(`Merge all stopped after ${mergedCount}: failed in ${shortId(runId)}: ${merged.merge?.error ?? "unknown error"}`);
          await refresh();
          return;
        }
        mergedCount += 1;
      }
      setNotice(`Merged ${mergedCount} run${mergedCount === 1 ? "" : "s"}`);
      await refresh();
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
      await refresh();
    }
  }, [mergePrompt, refresh]);

  const startConflictResolver = useCallback(async () => {
    if (!conflictPrompt) {
      return;
    }
    const files = conflictPrompt.files.length ? conflictPrompt.files.join("\n") : "(git did not report conflicted files)";
    const task = [
      "Read RUDDER.md first. A git merge stopped with conflicts in this checkout.",
      `Conflicted files:\n${files}`,
      "Resolve the merge conflicts, keep the intended changes from both sides where appropriate, run relevant checks if possible, and report what changed. Do not abort the merge unless resolving is impossible.",
    ].join("\n\n");
    try {
      const run = await startRun({
        task,
        backend,
        model,
        detach: true,
        worktree: false,
        silent: true,
        view: "shell",
      });
      setConflictPrompt(null);
      setSelectedRunId(run.id);
      setExpandedRunIds((current) => new Set(current).add(run.id));
      setNotice(`Started AI merge-conflict resolver ${shortId(run.id)}`);
      await refresh();
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    }
  }, [backend, conflictPrompt, model, refresh]);

  const copySelectedTranscript = useCallback(async (runOverride?: UiRun) => {
    const run = runOverride ?? selectedRun;
    if (!run) {
      setNotice("No agent selected");
      return;
    }
    try {
      await copyToClipboard(run.output);
      setNotice(`Copied transcript ${shortId(run.id)}`);
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    }
  }, [selectedRun]);

  const syncSelectedRun = useCallback(async (runOverride?: UiRun) => {
    const run = runOverride ?? selectedRun;
    if (!run) {
      setNotice("No agent selected");
      return;
    }
    try {
      const synced = await syncRun(run.id, { silent: true });
      setNotice(syncNotice(synced));
      await refresh();
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
      await refresh();
    }
  }, [refresh, selectedRun]);

  const handleCommand = useCallback(async (line: string) => {
    const [command = "", ...args] = line.slice(1).trim().split(/\s+/).filter(Boolean);
    switch (command) {
      case "":
      case "help":
      case "?":
        setHelpOpen((value) => !value);
        setInput("");
        return;
      case "q":
      case "quit":
      case "exit":
        app.exit();
        return;
      case "backend":
        if (isInteractiveBackend(args[0])) {
          const nextBackend = args[0];
          chooseBackend(nextBackend, setBackend, setModel, setNotice);
          setConfig(await rememberBackendSelection({ backend: nextBackend }));
        } else {
          setNotice("Usage: /backend claude|codex");
        }
        setInput("");
        return;
      case "agent":
      case "continue":
      case "interrupt": {
        const run = resolveUiRun(runs, args[0] ?? selectedRun?.id);
        if (run) {
          setTargetRunId(run.id);
          setSelectedRunId(run.id);
          setFocusPane("worker");
          setNotice(`${isActive(run.status) ? "Esc/Enter will interrupt" : "Typing to"} ${shortId(run.id)}`);
        } else {
          setNotice("No agent selected");
        }
        setInput("");
        return;
      }
      case "new":
        setTargetRunId(undefined);
        setFocusPane("task");
        setNotice("Typing starts a new agent");
        setInput("");
        return;
      case "model":
        if (args.length === 0) {
          setModelMenuOpen(true);
          setModelMenuIndex(0);
          setNotice(`Pick a ${backend} model`);
        } else {
          const nextModel = args.join(" ");
          setModel(nextModel);
          setConfig(await rememberBackendSelection({
            backend,
            model: nextModel,
            updateModel: true,
          }));
          setNotice(`Model ${nextModel}`);
        }
        setInput("");
        return;
      case "worktree":
        setWorktreeMode(args[0] === "always" || args[0] === "on" ? "always" : "auto");
        setNotice(`Worktrees ${args[0] === "always" || args[0] === "on" ? "always" : "auto"}`);
        setInput("");
        return;
      case "stop":
        await runAction(args[0] ?? selectedRun?.id, async (id) => stopRun(id, { silent: true }), "Stopped", setNotice, refresh);
        setInput("");
        return;
      case "delete":
        await requestDeleteSelectedRun(resolveUiRun(runs, args[0] ?? selectedRun?.id));
        setInput("");
        return;
      case "copy":
        await copySelectedTranscript(resolveUiRun(runs, args[0] ?? selectedRun?.id));
        setInput("");
        return;
      case "sync":
        await syncSelectedRun(resolveUiRun(runs, args[0] ?? selectedRun?.id));
        setInput("");
        return;
      case "merge":
        requestMergeRun(resolveUiRun(runs, args[0] ?? selectedRun?.id), args.includes("--allow-dirty"));
        setInput("");
        return;
      case "merge-all":
        requestMergeAll(args.includes("--allow-dirty"));
        setInput("");
        return;
      case "clear":
        setExpandedRunIds(new Set());
        setNotice("Collapsed all runs");
        setInput("");
        return;
      default:
        setNotice(`Unknown command: /${command}`);
        setInput("");
    }
  }, [app, backend, copySelectedTranscript, refresh, requestDeleteSelectedRun, requestMergeAll, requestMergeRun, runs, selectedRun?.id, syncSelectedRun]);

  const selectModelOption = useCallback((index: number) => {
    const option = modelOptions[index];
    if (!option) {
      return;
    }
    setModel(option.value);
    setModelMenuOpen(false);
    setModelMenuIndex(index);
    setNotice(option.value ? `Model ${option.value}` : "Using backend default model");
    void rememberBackendSelection({
      backend,
      model: option.value,
      updateModel: true,
    }).then(setConfig).catch((error) => {
      setNotice(error instanceof Error ? error.message : String(error));
    });
  }, [backend, modelOptions]);

  const selectCommandOption = useCallback((index: number) => {
    const option = commandOptions[index];
    if (!option) {
      return;
    }
    setInput(option.insert.endsWith(" ") ? option.insert : option.insert);
    setCommandMenuIndex(index);
    if (!option.insert.endsWith(" ") && option.insert !== "/model") {
      setNotice(`Press Enter to run ${option.insert}`);
    }
    if (option.insert === "/model") {
      setInput("");
      setModelMenuOpen(true);
      setModelMenuIndex(0);
      setNotice(`Pick a ${backend} model`);
    }
  }, [backend, commandOptions]);

  useInput((value, key) => {
    if (key.ctrl && value === "c") {
      app.exit();
      return;
    }
    if ((key.meta && value === "1") || value === "\u001b1") {
      setFocusPane("agents");
      setModelMenuOpen(false);
      setInput("");
      setNotice("Agents focus: j/k or arrows select runs");
      return;
    }
    if ((key.meta && value === "2") || value === "\u001b2") {
      setFocusPane("worker");
      setModelMenuOpen(false);
      setInput("");
      if (selectedRun) {
        setNotice(`${isActive(selectedRun.status) ? "Worker focus: type redirect, Enter interrupts" : "Worker focus: type follow-up"} ${shortId(selectedRun.id)}`);
      } else {
        setNotice("No agent selected");
      }
      return;
    }
    if ((key.meta && value === "3") || value === "\u001b3") {
      setFocusPane("task");
      setModelMenuOpen(false);
      setInput("");
      setTargetRunId(undefined);
      setNotice("Task focus: type a new task");
      return;
    }
    if (key.tab || value === "\t") {
      return;
    }
    if (mergePrompt) {
      if (key.escape || value === "n" || value === "N") {
        setMergePrompt(null);
        setNotice("Merge cancelled");
        return;
      }
      if (value === "y" || value === "Y") {
        void confirmMerge();
        return;
      }
      return;
    }
    if (conflictPrompt) {
      if (key.escape || value === "n" || value === "N") {
        setConflictPrompt(null);
        setNotice("Resolve the merge conflicts manually, then commit");
        return;
      }
      if (value === "y" || value === "Y") {
        void startConflictResolver();
        return;
      }
      return;
    }
    if (deletePrompt) {
      if (key.escape) {
        setDeletePrompt(null);
        setNotice("Delete cancelled");
        return;
      }
      if (value === "d") {
        void confirmDelete();
        return;
      }
      return;
    }
    if (modelMenuOpen) {
      if (key.escape) {
        setModelMenuOpen(false);
        setNotice("Model unchanged");
        return;
      }
      if (key.upArrow || value === "k") {
        setModelMenuIndex((current) => Math.max(0, current - 1));
        return;
      }
      if (key.downArrow || value === "j") {
        setModelMenuIndex((current) => Math.min(modelOptions.length - 1, current + 1));
        return;
      }
      if (key.return) {
        selectModelOption(modelMenuIndex);
        return;
      }
      const numeric = Number(value);
      if (Number.isInteger(numeric) && numeric >= 1 && numeric <= modelOptions.length) {
        selectModelOption(numeric - 1);
        return;
      }
      return;
    }
    if (commandMenuOpen && input.startsWith("/")) {
      if (key.upArrow || value === "\u001b[A") {
        setCommandMenuIndex((current) => Math.max(0, current - 1));
        return;
      }
      if (key.downArrow || value === "\u001b[B") {
        setCommandMenuIndex((current) => Math.min(commandOptions.length - 1, current + 1));
        return;
      }
    }
    if (key.escape) {
      if (input) {
        setInput("");
      } else if (helpOpen) {
        setHelpOpen(false);
      } else if (transcriptExpanded) {
        setTranscriptExpanded(false);
      } else if (focusPane === "worker") {
        setTargetRunId(undefined);
        setFocusPane("task");
        setNotice("Task focus: type a new task");
      } else if (selectedRun) {
        setFocusPane("worker");
        setNotice(`${isActive(selectedRun.status) ? "Type redirect, Enter interrupts" : "Typing to"} ${shortId(selectedRun.id)}`);
      }
      return;
    }
    if (key.upArrow || (focusPane === "agents" && input.length === 0 && value === "k")) {
      selectRelative(runs, selectedRunId, -1, setSelectedRunId);
      if (focusPane === "task") {
        setFocusPane("agents");
      }
      return;
    }
    if (key.downArrow || (focusPane === "agents" && input.length === 0 && value === "j")) {
      selectRelative(runs, selectedRunId, 1, setSelectedRunId);
      if (focusPane === "task") {
        setFocusPane("agents");
      }
      return;
    }
    if (key.pageUp) {
      selectRelative(runs, selectedRunId, -5, setSelectedRunId);
      return;
    }
    if (key.pageDown) {
      selectRelative(runs, selectedRunId, 5, setSelectedRunId);
      return;
    }
    if (key.return) {
      if (commandMenuOpen && shouldSelectCommand(input, commandOptions)) {
        selectCommandOption(commandMenuIndex);
      } else if (input.trim().startsWith("/")) {
        void handleCommand(input);
      } else {
        void submitTask(input);
      }
      return;
    }
    if ((key.meta && (key.backspace || key.delete)) || value === "\u0015" || (key.ctrl && value === "u")) {
      setInput("");
      return;
    }
    if (value === "\u001b\u007f" || value === "\u001b\b") {
      setInput((current) => deletePreviousWord(current));
      return;
    }
    if (key.ctrl && value === "w") {
      setInput((current) => deletePreviousWord(current));
      return;
    }
    if (key.backspace || key.delete || value === "\u007f" || value === "\b") {
      setInput((current) => current.slice(0, -1));
      return;
    }
    if (focusPane !== "agents" && !input.startsWith("/")) {
      const text = normalizeInputText(value);
      if (text) {
        setInput((current) => current + text);
        setCommandMenuIndex(0);
      }
      return;
    }
    if (input.length === 0 && value === "q") {
      app.exit();
      return;
    }
    if (input.length === 0 && value === "?") {
      setHelpOpen((current) => !current);
      return;
    }
    if (input.length === 0 && value === "r") {
      void refresh();
      setNotice("Refreshed");
      return;
    }
    if (input.length === 0 && value === "o") {
      setModelMenuOpen(true);
      setModelMenuIndex(0);
      setNotice(`Pick a ${backend} model`);
      return;
    }
    if (input.length === 0 && value === "c" && selectedRun) {
      setFocusPane("worker");
      setNotice(`${isActive(selectedRun.status) ? "Type redirect, Enter interrupts" : "Typing to"} ${shortId(selectedRun.id)}`);
      return;
    }
    if (input.length === 0 && value === "n") {
      setTargetRunId(undefined);
      setFocusPane("task");
      setNotice("Typing starts a new agent");
      return;
    }
    if (input.length === 0 && value === "w") {
      setWorktreeMode((current) => current === "auto" ? "always" : "auto");
      return;
    }
    if (input.length === 0 && value === "x" && selectedRun) {
      setExpandedRunIds((current) => toggleSet(current, selectedRun.id));
      return;
    }
    if (input.length === 0 && value === "l" && selectedRun) {
      setTranscriptExpanded((current) => !current);
      return;
    }
    if (input.length === 0 && value === "s" && selectedRun) {
      void runAction(selectedRun.id, async (id) => stopRun(id, { silent: true }), "Stopped", setNotice, refresh);
      return;
    }
    if (input.length === 0 && value === "u" && selectedRun) {
      void syncSelectedRun();
      return;
    }
    if (input.length === 0 && value === "m" && selectedRun) {
      requestMergeRun(selectedRun);
      return;
    }
    if (input.length === 0 && value === "M") {
      requestMergeAll();
      return;
    }
    if (input.length === 0 && value === "d") {
      void requestDeleteSelectedRun();
      return;
    }
    if (input.length === 0 && value === "y") {
      void copySelectedTranscript();
      return;
    }
    if (focusPane === "agents" && value !== "/") {
      return;
    }
    if (focusPane === "worker" && !selectedRun && value !== "/") {
      setNotice("No agent selected");
      return;
    }
    const text = normalizeInputText(value);
    if (text) {
      setInput((current) => current + text);
      setCommandMenuIndex(0);
    }
  });

  const width = Math.max(80, size.columns);
  const height = Math.max(24, size.rows);
  const railWidth = Math.min(42, Math.max(30, Math.floor(width * 0.34)));
  const detailWidth = Math.max(30, width - railWidth - 1);
  const detailHeight = Math.max(8, height - 8);

  return (
    <Box flexDirection="column" width={width} height={height}>
      <Header width={width} repoRoot={repoRoot} branch={branch} backend={backend} model={model ?? modelForBackend(backend, config)} activeCount={activeCount} worktreeMode={worktreeMode} />
      <Box flexGrow={1} minHeight={0}>
        <RunRail runs={runs} selectedRunId={selectedRun?.id} targetRunId={targetRun?.id} width={railWidth} expandedRunIds={expandedRunIds} focused={focusPane === "agents"} />
        <Box flexDirection="column" flexGrow={1} marginLeft={1}>
          <DetailPane
            run={selectedRun}
            width={detailWidth}
            height={detailHeight}
            expanded={selectedExpanded}
            transcriptExpanded={transcriptExpanded}
            focused={focusPane === "worker"}
            input={focusPane === "worker" ? input : ""}
            submitting={submitting}
          />
        </Box>
      </Box>
      {helpOpen ? <Help /> : null}
      {modelMenuOpen ? <ModelMenu backend={backend} options={modelOptions} selectedIndex={modelMenuIndex} currentModel={model} width={width} /> : null}
      {commandMenuOpen ? <CommandMenu options={commandOptions} selectedIndex={commandMenuIndex} width={width} /> : null}
      {mergePrompt ? <MergePromptBox prompt={mergePrompt} width={width} /> : null}
      {conflictPrompt ? <MergeConflictPromptBox prompt={conflictPrompt} width={width} /> : null}
      {deletePrompt ? <DeletePromptBox prompt={deletePrompt} width={width} /> : null}
      {focusPane === "worker"
        ? <StatusDock notice={notice} />
        : <PromptDock input={input} backend={backend} model={model ?? modelForBackend(backend, config)} notice={notice} submitting={submitting} targetRun={targetRun} focused={focusPane === "task"} />}
      <Footer focusPane={focusPane} />
    </Box>
  );
}

function Header(props: {
  width: number;
  repoRoot: string;
  branch: string;
  backend: BackendId;
  model?: string;
  activeCount: number;
  worktreeMode: "auto" | "always";
}): React.ReactElement {
  const contentWidth = Math.max(10, props.width - 4);
  const label = `rudder  ${shortenHome(props.repoRoot)} ${props.branch}  |  ${props.backend}${props.model ? ` ${props.model}` : ""}  |  worktree:${props.worktreeMode}  active:${props.activeCount}`;
  return (
    <Box borderStyle="single" borderColor="cyan" paddingX={1}>
      <Text bold color="cyan">{fitLine(label, contentWidth)}</Text>
    </Box>
  );
}

function RunRail(props: {
  runs: UiRun[];
  selectedRunId?: string;
  targetRunId?: string;
  width: number;
  expandedRunIds: Set<string>;
  focused: boolean;
}): React.ReactElement {
  const visible = props.runs.slice(0, 12);
  return (
    <Box flexDirection="column" width={props.width} borderStyle={props.focused ? "double" : "single"} borderColor={props.focused ? "cyan" : "gray"} paddingX={1}>
      <Box justifyContent="space-between">
        <Box>
          {props.focused ? <FocusPill label="focus" /> : null}
        <Text bold color={props.focused ? "cyan" : undefined}> agents</Text>
        </Box>
        <Text color="gray">{props.runs.length} runs</Text>
      </Box>
      {visible.length === 0 ? (
        <Box marginTop={1}><Text color="gray">No runs yet. Type a task below.</Text></Box>
      ) : null}
      {visible.map((run) => (
        <RunCard
          key={run.id}
          run={run}
          selected={run.id === props.selectedRunId}
          targeted={run.id === props.targetRunId}
          expanded={props.expandedRunIds.has(run.id)}
          width={props.width - 4}
        />
      ))}
    </Box>
  );
}

function RunCard(props: { run: UiRun; selected: boolean; targeted: boolean; expanded: boolean; width: number }): React.ReactElement {
  const tone = runStatusColor(props.run);
  const label = props.selected ? (props.targeted ? ">>" : "> ") : "  ";
  const task = truncate(taskDisplayLabel(props.run, 80), Math.max(12, props.width - 14));
  const progress = completionPercent(props.run);
  const summary = truncate(agentRailSummary(props.run), Math.max(12, props.width - 7));
  const meta = `${progressBar(progress)} ${progress}%`;
  return (
    <Box flexDirection="column" marginTop={1}>
      <Text wrap="truncate" color={props.selected ? "white" : "gray"} bold={props.selected}>
        {label} {meta} {statusGlyph(props.run.status)} {props.run.backend} {task}
      </Text>
      <Text wrap="truncate" color={tone}>  {statusWord(props.run)} {props.targeted ? "editing " : ""}{summary}</Text>
    </Box>
  );
}

function DetailPane(props: {
  run?: UiRun;
  width: number;
  height: number;
  expanded: boolean;
  transcriptExpanded: boolean;
  focused: boolean;
  input: string;
  submitting: boolean;
}): React.ReactElement {
  if (!props.run) {
    return (
      <Box width={props.width} height={props.height} borderStyle={props.focused ? "double" : "single"} borderColor={props.focused ? "cyan" : "gray"} paddingX={1} flexDirection="column">
        <Text color="gray">No agent selected.</Text>
      </Box>
    );
  }
  const composerHeight = props.focused ? 3 : 0;
  const outputHeight = Math.max(5, props.height - 6 - composerHeight);
  const contentWidth = Math.max(10, props.width - 4);
  return (
    <Box width={props.width} height={props.height} borderStyle={props.focused ? "double" : "single"} borderColor={props.focused ? "cyan" : runStatusColor(props.run)} paddingX={1} flexDirection="column">
      <Box justifyContent="space-between">
        <Box>
          {props.focused ? <FocusPill label="focus" /> : null}
          <Text bold color={props.focused ? "cyan" : undefined}> worker</Text>
        </Box>
        <Text color={runStatusColor(props.run)}>{workerStateLabel(props.run)}</Text>
      </Box>
      <Text wrap="truncate" color="gray">{fitLine(props.run.task, contentWidth)}</Text>
      <Box flexDirection="column" marginTop={1} minHeight={0}>
        <Box height={outputHeight} overflow="hidden" flexDirection="column">
          {tailLines(props.run.output, outputHeight).map((line, index) => (
            <Text key={index} wrap="truncate">{line || " "}</Text>
          ))}
        </Box>
      </Box>
      {props.focused ? <WorkerComposer run={props.run} input={props.input} submitting={props.submitting} width={contentWidth} /> : null}
    </Box>
  );
}

function WorkerComposer(props: { run: UiRun; input: string; submitting: boolean; width: number }): React.ReactElement {
  const active = isActive(props.run.status);
  const label = active ? "interrupt" : "agent";
  const helper = active ? "Enter interrupts and redirects this run" : "Enter continues this completed session";
  const value = props.input || (active ? "type a redirect..." : "type a follow-up...");
  return (
    <Box flexDirection="column" marginTop={1} borderStyle="single" borderColor="cyan" paddingX={1}>
      <Box justifyContent="space-between">
        <Text>
          <FocusPill label={label} />
          <Text color={props.submitting ? "yellow" : "cyan"}> {props.submitting ? "sending" : shortId(props.run.id)}</Text>
          <Text>  {truncate(value, Math.max(8, props.width - 28))}</Text>
          <Text color="cyan">_</Text>
        </Text>
        <Text color="gray">{active ? "running" : "resumable"}</Text>
      </Box>
      <Text color="gray">{fitLine(`${helper}. Option-1/2/3 changes pane, Esc returns to task.`, props.width)}</Text>
    </Box>
  );
}

function Help(): React.ReactElement {
  return (
    <Box borderStyle="single" borderColor="yellow" paddingX={1} flexDirection="column">
      <Text bold color="yellow">keys</Text>
      <Text><Text color="cyan">Option-1/2/3</Text> focus agents/worker/task   <Text color="cyan">Enter</Text> submit focused input   <Text color="cyan">j/k</Text> select run</Text>
      <Text><Text color="cyan">worker focus</Text> type to selected agent; running agents are interrupted on Enter   <Text color="cyan">n</Text> new task   <Text color="cyan">x</Text> expand   <Text color="cyan">l</Text> transcript</Text>
      <Text><Text color="cyan">o</Text> model picker   <Text color="cyan">/</Text> command search   <Text color="cyan">dd</Text> delete   <Text color="cyan">y</Text> copy transcript   <Text color="cyan">s</Text> stop   <Text color="cyan">u</Text> sync   <Text color="cyan">m/M</Text> merge</Text>
      <Text color="gray">Slash: /backend claude|codex, /model, /model &lt;name&gt;, /agent, /interrupt, /new, /worktree, /stop, /delete, /copy, /sync, /merge, /merge-all, /exit</Text>
    </Box>
  );
}

function FocusPill(props: { label: string }): React.ReactElement {
  return (
    <Text backgroundColor="cyan" color="black" bold> {props.label.toUpperCase()} </Text>
  );
}

function ModelMenu(props: {
  backend: BackendId;
  options: ModelOption[];
  selectedIndex: number;
  currentModel?: string;
  width: number;
}): React.ReactElement {
  const contentWidth = Math.max(24, props.width - 4);
  const start = visibleWindowStart(props.selectedIndex, props.options.length, 8);
  const visible = props.options.slice(start, start + 8);
  const hiddenBefore = start > 0;
  const hiddenAfter = start + visible.length < props.options.length;
  return (
    <Box width={props.width} borderStyle="single" borderColor="magenta" paddingX={1} flexDirection="column">
      <Text bold color="magenta">{fitLine(`model: ${props.backend}`, contentWidth)}</Text>
      {hiddenBefore ? <Text color="gray">{fitLine("  ...", contentWidth)}</Text> : null}
      {visible.map((option, localIndex) => {
        const index = start + localIndex;
        const selected = index === props.selectedIndex;
        const active = option.value === props.currentModel || (!option.value && !props.currentModel);
        const marker = active ? "* " : "";
        const line = `${selected ? "> " : "  "}${index + 1}. ${marker}${option.label}${option.detail ? `  ${option.detail}` : ""}`;
        return (
          <Text key={`${option.label}-${index}`} color={selected ? "white" : "gray"} bold={selected} wrap="truncate">
            {fitLine(line, contentWidth)}
          </Text>
        );
      })}
      {hiddenAfter ? <Text color="gray">{fitLine("  ...", contentWidth)}</Text> : null}
      <Text color="gray">{fitLine("Enter selects, Esc cancels, j/k or arrows move. Type /model <id> for custom.", contentWidth)}</Text>
    </Box>
  );
}

function CommandMenu(props: { options: CommandOption[]; selectedIndex: number; width: number }): React.ReactElement {
  const contentWidth = Math.max(24, props.width - 4);
  const start = visibleWindowStart(props.selectedIndex, props.options.length, 8);
  const visible = props.options.slice(start, start + 8);
  return (
    <Box width={props.width} borderStyle="single" borderColor="blue" paddingX={1} flexDirection="column">
      <Text bold color="blue">{fitLine("commands", contentWidth)}</Text>
      {visible.map((option, localIndex) => {
        const index = start + localIndex;
        const selected = index === props.selectedIndex;
        const line = `${selected ? "> " : "  "}/${option.name.padEnd(12, " ")} ${option.detail}`;
        return (
          <Text key={option.name} color={selected ? "white" : "gray"} bold={selected} wrap="truncate">
            {fitLine(line, contentWidth)}
          </Text>
        );
      })}
      <Text color="gray">{fitLine("Enter completes/runs selected command, arrows move, Option-1/2/3 changes pane focus.", contentWidth)}</Text>
    </Box>
  );
}

function DeletePromptBox(props: { prompt: DeletePrompt; width: number }): React.ReactElement {
  const contentWidth = Math.max(24, props.width - 4);
  const action = "d delete run + worktree, Esc cancel";
  return (
    <Box width={props.width} borderStyle="double" borderColor="yellow" paddingX={1}>
      <Text color="yellow" bold>{fitLine(`delete ${shortId(props.prompt.runId)}?  ${action}`, contentWidth)}</Text>
    </Box>
  );
}

function MergePromptBox(props: { prompt: MergePrompt; width: number }): React.ReactElement {
  const contentWidth = Math.max(24, props.width - 4);
  const subject = props.prompt.kind === "selected"
    ? `merge ${shortId(props.prompt.runId)}  ${props.prompt.label}`
    : `merge ${props.prompt.runIds.length} completed run${props.prompt.runIds.length === 1 ? "" : "s"}`;
  const prefix = "press ";
  const action = "y to merge";
  const suffix = ", n to cancel";
  const hint = fitLine(`${prefix}${action}${suffix}`, contentWidth);
  return (
    <Box width={props.width} borderStyle="double" borderColor="yellow" paddingX={1} flexDirection="column">
      <Text color="yellow" bold>{fitLine(subject, contentWidth)}</Text>
      <Text>
        <Text color="gray">{hint.slice(0, prefix.length)}</Text>
        <Text color="red" bold>{hint.slice(prefix.length, prefix.length + action.length)}</Text>
        <Text color="gray">{hint.slice(prefix.length + action.length)}</Text>
      </Text>
    </Box>
  );
}

function MergeConflictPromptBox(props: { prompt: MergeConflictPrompt; width: number }): React.ReactElement {
  const contentWidth = Math.max(24, props.width - 4);
  const files = props.prompt.files.length ? props.prompt.files.join(", ") : "unknown files";
  return (
    <Box width={props.width} borderStyle="double" borderColor="red" paddingX={1} flexDirection="column">
      <Text color="red" bold>{fitLine(`merge conflict ${shortId(props.prompt.runId)}`, contentWidth)}</Text>
      <Text color="gray">{fitLine(files, contentWidth)}</Text>
      <Text color="gray">{fitLine("press y for AI help, n to handle manually", contentWidth)}</Text>
    </Box>
  );
}

function PromptDock(props: {
  input: string;
  backend: BackendId;
  model?: string;
  notice: string;
  submitting: boolean;
  targetRun?: UiRun;
  focused: boolean;
}): React.ReactElement {
  const label = props.targetRun ? `${isActive(props.targetRun.status) ? "interrupt" : "agent"} ${shortId(props.targetRun.id)}` : "task";
  const showTextLabel = !props.focused || Boolean(props.targetRun);
  return (
    <Box borderStyle={props.focused ? "double" : "single"} borderColor={props.focused ? "cyan" : props.targetRun ? "magenta" : "gray"} paddingX={1} justifyContent="space-between">
      <Text>
        {props.focused ? <FocusPill label="task" /> : null}
        {showTextLabel ? (
          <Text color={props.submitting ? "yellow" : props.targetRun ? "magenta" : "cyan"}>{props.focused ? " " : ""}{props.submitting ? "starting" : label}</Text>
        ) : null}
        <Text>  {props.input}</Text>
        <Text color="cyan">_</Text>
      </Text>
      <Text color="gray">{props.notice}  {props.backend}{props.model ? ` ${props.model}` : ""}</Text>
    </Box>
  );
}

function StatusDock(props: { notice: string }): React.ReactElement {
  return (
    <Box borderStyle="single" borderColor="gray" paddingX={1} justifyContent="space-between">
      <Text color="gray">worker input is active inside the selected agent pane</Text>
      <Text color="gray">{props.notice}</Text>
    </Box>
  );
}

function Footer(props: { focusPane: FocusPane }): React.ReactElement {
  return (
    <Box>
      <Text color="gray">focus:{props.focusPane}  Opt-1/2/3 focus  / commands  o model  n new  c worker  u sync  dd delete  y copy  m/M merge  ? help</Text>
    </Box>
  );
}

async function loadUiRuns(repoRoot: string): Promise<UiRun[]> {
  const runs = await listRuns(repoRoot);
  return await Promise.all(runs.map(async (run) => {
    const [output, events] = await Promise.all([
      readTextIfExists(outputPath(repoRoot, run.id)),
      readEvents(repoRoot, run.id),
    ]);
    return {
      ...run,
      output,
      events,
      work: buildWork(events, run),
      attention: permissionAttentionFromOutput(output),
    };
  }));
}

async function readTextIfExists(file: string): Promise<string> {
  if (!(await pathExists(file))) {
    return "";
  }
  return await fsp.readFile(file, "utf8").catch(() => "");
}

async function readEvents(repoRoot: string, runId: string): Promise<RudderEvent[]> {
  const file = eventsPath(repoRoot, runId);
  const raw = await readTextIfExists(file);
  return raw
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line) as RudderEvent;
      } catch {
        return null;
      }
    })
    .filter((event): event is RudderEvent => Boolean(event));
}

function buildWork(events: RudderEvent[], run: RunRecord): WorkItem[] {
  const items: WorkItem[] = [];
  for (const event of events) {
    if (event.type === "run.created") {
      continue;
    }
    if (event.type === "run.continued") {
      items.push({ label: "user follow-up", detail: event.message, tone: "info" });
      continue;
    }
    if (event.type === "steerer.waiting") {
      items.push({ label: "auto-steering wait", detail: "10s grace period", tone: "warning" });
      continue;
    }
    if (event.type === "steerer.prompt") {
      items.push({ label: "auto-steering", detail: "review prompt sent", tone: "info" });
      continue;
    }
    if (event.type === "planner.spec") {
      continue;
    }
    if (event.type === "run.started") {
      continue;
    }
    if (event.type === "backend.output") {
      const tool = toolSummary(event.data);
      if (tool) {
        items.push(tool);
      }
      continue;
    }
    if (event.type === "backend.error") {
      items.push({ label: "backend error", detail: event.message, tone: "danger" });
      continue;
    }
    if (event.type === "verifier.result") {
      const missing = missingCount(event.data);
      items.push({ label: "verifier", detail: missing ? `${missing} missing item${missing === 1 ? "" : "s"}` : "accepted", tone: missing ? "warning" : "success" });
      continue;
    }
    if (event.type === "run.completed") {
      items.push({ label: "completed", detail: event.message, tone: "success" });
      continue;
    }
    if (event.type === "run.failed") {
      items.push({ label: "failed", detail: event.message, tone: "danger" });
      continue;
    }
    if (event.type === "run.cancelled") {
      items.push({ label: "cancelled", detail: event.message, tone: "warning" });
      continue;
    }
    if (event.type === "merge.result") {
      const detail = event.message;
      const tone = detail?.includes("conflict") || detail?.includes("failed") ? "warning" : "success";
      items.push({ label: "merge", detail, tone });
    }
    if (event.type === "sync.result") {
      const detail = event.message;
      const tone = detail?.includes("conflict") || detail?.includes("failed") ? "warning" : "success";
      items.push({ label: "sync", detail, tone });
    }
  }
  return compactWork(items);
}

function toolSummary(data: unknown): WorkItem | null {
  if (!isRecord(data) || data.type !== "stream_event" || !isRecord(data.event)) {
    return null;
  }
  const event = data.event;
  if (event.type === "content_block_start" && isRecord(event.content_block)) {
    const block = event.content_block;
    if (block.type === "tool_use") {
      return { label: "tool", detail: typeof block.name === "string" ? block.name : "tool_use", tone: "muted" };
    }
  }
  return null;
}

function compactWork(items: WorkItem[]): WorkItem[] {
  const compacted: WorkItem[] = [];
  for (const item of items) {
    const last = compacted.at(-1);
    if (last && last.label === item.label && last.detail === item.detail && last.tone === item.tone) {
      continue;
    }
    compacted.push(item);
  }
  return compacted;
}

function formatWorkLine(item: WorkItem, width: number): string {
  const raw = `${workGlyph(item.tone)} ${item.label}${item.detail ? `  ${item.detail}` : ""}`;
  return fitLine(raw, width);
}

function fitLine(value: string, width: number): string {
  return truncate(value, width).padEnd(width, " ");
}

async function runAction(
  runId: string | undefined,
  action: (runId: string) => Promise<unknown>,
  success: string,
  setNotice: (notice: string) => void,
  refresh: () => Promise<void>,
): Promise<void> {
  if (!runId) {
    setNotice("No run selected");
    return;
  }
  try {
    await action(runId);
    setNotice(`${success} ${shortId(runId)}`);
    await refresh();
  } catch (error) {
    setNotice(error instanceof Error ? error.message : String(error));
  }
}

async function mergeReadyRuns(
  runs: UiRun[],
  allowDirty: boolean,
  setNotice: (notice: string) => void,
  refresh: () => Promise<void>,
): Promise<void> {
  const ready = runs.filter(canMerge);
  if (ready.length === 0) {
    setNotice("No completed worktree runs ready to merge");
    return;
  }
  setNotice(`Merging ${ready.length} run${ready.length === 1 ? "" : "s"}...`);
  let merged = 0;
  for (const run of ready) {
    const result = await mergeRun(run.id, allowDirty, { silent: true });
    if (result.merge?.status === "conflict") {
      const files = result.merge.conflictedFiles ?? [];
      setNotice(`Merge all stopped after ${merged}: conflict in ${shortId(run.id)} (${files.length || "unknown"} file${files.length === 1 ? "" : "s"})`);
      await refresh();
      return;
    }
    if (result.merge?.status !== "merged") {
      setNotice(`Merge all stopped after ${merged}: failed in ${shortId(run.id)}: ${result.merge?.error ?? "unknown error"}`);
      await refresh();
      return;
    }
    merged += 1;
  }
  setNotice(`Merged ${merged} run${merged === 1 ? "" : "s"}`);
  await refresh();
}

function selectRelative(
  runs: UiRun[],
  selectedRunId: string | undefined,
  delta: number,
  setSelectedRunId: (runId: string | undefined) => void,
): void {
  if (runs.length === 0) {
    return;
  }
  const index = Math.max(0, runs.findIndex((run) => run.id === selectedRunId));
  const next = Math.min(runs.length - 1, Math.max(0, index + delta));
  setSelectedRunId(runs[next]?.id);
}

function chooseBackend(
  backend: BackendId,
  setBackend: (backend: BackendId) => void,
  setModel: (model: string | undefined) => void,
  setNotice: (notice: string) => void,
): void {
  setBackend(backend);
  setModel(undefined);
  setNotice(`Backend ${backend}`);
}

function visibleWindowStart(selectedIndex: number, total: number, windowSize: number): number {
  if (total <= windowSize) {
    return 0;
  }
  const half = Math.floor(windowSize / 2);
  return Math.max(0, Math.min(total - windowSize, selectedIndex - half));
}

function toggleSet(current: Set<string>, value: string): Set<string> {
  const next = new Set(current);
  if (next.has(value)) {
    next.delete(value);
  } else {
    next.add(value);
  }
  return next;
}

function modelForBackend(backend: BackendId, config: RudderConfig | null): string | undefined {
  if (!config) {
    return undefined;
  }
  if (backend === "claude") {
    return config.backends.claude?.model;
  }
  if (backend === "codex") {
    return config.backends.codex?.model;
  }
  return config.backends.acpx?.model;
}

function notifyRunAlerts(runs: UiRun[], ref: React.MutableRefObject<AlertState | null>): void {
  const terminal = new Set(runs.filter((run) => isTerminal(run.status)).map((run) => run.id));
  const permission = new Set(runs.filter((run) => runNeedsPermission(run)).map((run) => run.id));
  if (!ref.current) {
    ref.current = { terminal, permission };
    return;
  }
  for (const run of runs) {
    if (isTerminal(run.status) && !ref.current.terminal.has(run.id)) {
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

function playCompletionSound(): void {
  try {
    const player = process.platform === "darwin" ? "afplay" : "ffplay";
    const args = process.platform === "darwin"
      ? [COMPLETION_SOUND]
      : ["-nodisp", "-autoexit", "-loglevel", "quiet", COMPLETION_SOUND];
    const child = spawn(player, args, {
      detached: true,
      stdio: "ignore",
    });
    child.on("error", () => process.stdout.write("\u0007"));
    child.unref();
  } catch {
    process.stdout.write("\u0007");
  }
}

function resolveUiRun(runs: UiRun[], runId: string | undefined): UiRun | undefined {
  if (!runId) {
    return undefined;
  }
  return runs.find((run) => run.id === runId || run.id.startsWith(runId));
}

function filterCommands(input: string): CommandOption[] {
  if (!input.startsWith("/")) {
    return [];
  }
  const query = input.slice(1).trim().toLowerCase();
  if (!query) {
    return COMMANDS;
  }
  return COMMANDS.filter((command) => command.name.includes(query) || command.detail.toLowerCase().includes(query));
}

function shouldSelectCommand(input: string, options: CommandOption[]): boolean {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/") || trimmed.includes(" ")) {
    return false;
  }
  const query = trimmed.slice(1);
  return query.length === 0 || !options.some((option) => option.name === query);
}

function isInteractiveBackend(value: string | undefined): value is BackendId {
  return value === "claude" || value === "codex";
}

function toInteractiveBackend(value: BackendId | undefined): BackendId {
  return value === "codex" ? "codex" : "claude";
}

function isActive(status: RunStatus): boolean {
  return status === "created" || status === "running" || status === "steering" || status === "verifying";
}

function isTerminal(status: RunStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled" || status === "merged" || status === "merge-conflict";
}

function statusGlyph(status: RunStatus): string {
  if (status === "completed" || status === "merged") {
    return "ok";
  }
  if (status === "failed" || status === "merge-conflict") {
    return "!!";
  }
  if (status === "cancelled") {
    return "--";
  }
  return "..";
}

function statusColor(status: RunStatus): string {
  if (status === "completed" || status === "merged") {
    return "green";
  }
  if (status === "failed" || status === "merge-conflict") {
    return "red";
  }
  if (status === "cancelled") {
    return "yellow";
  }
  if (status === "verifying" || status === "steering") {
    return "magenta";
  }
  return "cyan";
}

function runStatusColor(run: UiRun): string {
  return runNeedsPermission(run) ? "yellow" : statusColor(run.status);
}

function runNeedsPermission(run: UiRun): boolean {
  return isActive(run.status) && run.attention.needsPermission;
}

function toneColor(tone: WorkItem["tone"]): string {
  if (tone === "success") {
    return "green";
  }
  if (tone === "warning") {
    return "yellow";
  }
  if (tone === "danger") {
    return "red";
  }
  if (tone === "info") {
    return "cyan";
  }
  return "gray";
}

function workGlyph(tone: WorkItem["tone"]): string {
  if (tone === "success") {
    return "ok";
  }
  if (tone === "warning") {
    return "??";
  }
  if (tone === "danger") {
    return "!!";
  }
  if (tone === "info") {
    return "->";
  }
  return "--";
}

function completionPercent(run: UiRun): number {
  if (runNeedsPermission(run)) {
    return 90;
  }
  if (run.status === "merged") {
    return 100;
  }
  if (run.status === "completed") {
    return 95;
  }
  if (run.status === "merge-conflict") {
    return 85;
  }
  if (run.status === "failed" || run.status === "cancelled") {
    return 50;
  }
  if (run.status === "steering") {
    return 88;
  }
  if (run.status === "verifying") {
    return 82;
  }
  const labels = new Set(run.work.map((item) => item.label));
  let value = 8;
  if (labels.has("worktree prepared") || labels.has("checkout claimed")) {
    value = 18;
  }
  if (labels.has("planner contract")) {
    value = 28;
  }
  if (labels.has("worker started")) {
    value = 45;
  }
  if (run.output.trim()) {
    value = Math.max(value, 60);
  }
  return value;
}

function progressBar(percent: number): string {
  const width = 6;
  const filled = Math.max(0, Math.min(width, Math.round((percent / 100) * width)));
  return `[${"#".repeat(filled)}${"-".repeat(width - filled)}]`;
}

function canMerge(run: UiRun): boolean {
  return (run.status === "completed" || (run.status === "merge-conflict" && run.merge?.conflictKind === "rebase")) && run.worktree.enabled;
}

function runSummary(run: UiRun): string {
  if (runNeedsPermission(run)) {
    return run.attention.summary ? `needs permission: ${run.attention.summary}` : "needs permission";
  }
  const latestWork = run.work.at(-1);
  if (run.status === "steering") {
    return "auto-steering after completion";
  }
  if (latestWork) {
    return `${latestWork.label}${latestWork.detail ? `: ${latestWork.detail}` : ""}`;
  }
  const latestLine = tailLines(run.output, 1)[0];
  if (latestLine && latestLine !== "No transcript yet.") {
    return latestLine;
  }
  return run.status;
}

function workerStateLabel(run: UiRun): string {
  if (runNeedsPermission(run)) {
    return "needs permission";
  }
  if (run.status === "completed") {
    return canMerge(run) ? "done  u sync  m merge" : "done";
  }
  if (run.status === "merged") {
    return "merged";
  }
  if (run.status === "failed" || run.status === "merge-conflict") {
    return "failed";
  }
  if (run.status === "cancelled") {
    return "stopped";
  }
  if (run.status === "verifying" || run.status === "steering") {
    return "checking";
  }
  return "running";
}

function syncNotice(run: RunRecord): string {
  if (run.sync?.status === "synced") {
    return `Synced ${shortId(run.id)}`;
  }
  if (run.sync?.status === "conflict") {
    const count = run.sync.conflictedFiles?.length || "unknown";
    return `Sync conflict in ${count} file${count === 1 ? "" : "s"}; resolve in the worktree and run git rebase --continue`;
  }
  return `Sync failed: ${run.sync?.error ?? "unknown error"}`;
}

function agentRailSummary(run: UiRun): string {
  if (runNeedsPermission(run)) {
    return run.attention.summary ?? "waiting for permission";
  }
  const output = summarizeOutput(run.output);
  if (output) {
    return output;
  }
  const latestWork = run.work.at(-1);
  if (latestWork) {
    return `${latestWork.label}${latestWork.detail ? `: ${latestWork.detail}` : ""}`;
  }
  return run.currentPrompt && run.currentPrompt !== run.task ? run.currentPrompt : run.task;
}

function summarizeOutput(output: string): string {
  const normalized = output.replace(/\s+/g, " ").trim();
  if (!normalized) {
    return "";
  }
  const sentence = normalized.match(/^.{24,180}?[.!?](?:\s|$)/)?.[0]?.trim();
  return sentence || normalized.slice(0, 180);
}

function statusWord(run: UiRun): string {
  if (runNeedsPermission(run)) {
    return "permission:";
  }
  const status = run.status;
  if (status === "merged") {
    return "merged:";
  }
  if (status === "completed") {
    return "done:";
  }
  if (status === "failed" || status === "merge-conflict") {
    return "failed:";
  }
  if (status === "cancelled") {
    return "stopped:";
  }
  if (status === "steering" || status === "verifying") {
    return "checking:";
  }
  return "running:";
}

function deletePreviousWord(value: string): string {
  return value.replace(/\s+$/, "").replace(/\S+$/, "");
}

function normalizeInputText(value: string): string {
  return value
    .replace(/\x1b\[[0-9;]*[A-Za-z]/g, "")
    .replace(/\r?\n/g, " ")
    .replace(/\t/g, " ")
    .replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, "");
}

async function copyToClipboard(text: string): Promise<void> {
  if (!text.trim()) {
    throw new Error("No transcript to copy");
  }
  const command = process.platform === "darwin" ? "pbcopy" : "xclip";
  const args = process.platform === "darwin" ? [] : ["-selection", "clipboard"];
  await new Promise<void>((resolve, reject) => {
    const child = spawn(command, args, { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`Clipboard command exited with ${code}`));
      }
    });
    child.stdin.end(text);
  });
}

function missingCount(data: unknown): number {
  if (!isRecord(data) || !Array.isArray(data.missing)) {
    return 0;
  }
  return data.missing.length;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function tailLines(text: string, maxLines: number): string[] {
  const normalized = text.replace(/\r/g, "");
  const lines = normalized.includes("\n") ? normalized.split("\n") : chunkLine(normalized, 96);
  const visible = lines.filter((line, index) => line.length > 0 || index < lines.length - 1).slice(-Math.max(1, maxLines));
  return visible.length ? visible : ["No transcript yet."];
}

function chunkLine(text: string, width: number): string[] {
  if (!text) {
    return [];
  }
  const chunks: string[] = [];
  for (let i = 0; i < text.length; i += width) {
    chunks.push(text.slice(i, i + width));
  }
  return chunks;
}

function shortId(runId: string): string {
  return runId.slice(0, 14);
}

function truncate(value: string, width: number): string {
  if (value.length <= width) {
    return value;
  }
  if (width <= 1) {
    return value.slice(0, width);
  }
  return `${value.slice(0, Math.max(0, width - 3))}...`;
}
