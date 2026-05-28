import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import fsp from "node:fs/promises";
import path from "node:path";
import { createSpec, renderContract, verifyRun } from "./brain.js";
import { ensureRudderCodexBinary } from "./codex-binary.js";
import { getBackend } from "./backends.js";
import { nativeAgentCommand } from "./native-agents.js";
import { buildPlanPrompt, PLAN_MODE_CONTRACT } from "./plan-mode.js";
import { createRunRecord, agentContextPath, eventsPath, listRuns, loadConfig, loadRunRecord, outputPath, rememberBackendSelection, resolveRun, runDir, saveRunRecord, } from "./state.js";
import { appendEvent, } from "./state.js";
import { activeRunsForCheckout, createRunJjWorkspace, createRunWorktree, currentBranch, currentCommit, currentJjChangeId, detectVcsMode, findRepoRoot, mergeRunIntoCurrentBranch, processAlive, removeRunWorkspace, runHasChanges, syncRunWorktree, worktreeBaseCommit, } from "./git.js";
import { commandExists, ensureDir, isTty, MissingToolError, newRunId, nowIso, pathExists, runCommand, shortenHome, } from "./util.js";
import { createAgentPane, killPane, normalizeTmuxDashboardLayout, paneExitStatus, respawnPane, selectPane } from "./tmux.js";
import { taskDisplayLabel } from "./task-summary.js";
const AUTO_STEER_DELAY_MS = 10_000;
function missingBackendError(backend, healthMessage) {
    return new MissingToolError(backend, healthMessage);
}
export async function startRun(params) {
    const repoRoot = findRepoRoot();
    const config = await loadConfig();
    const backend = params.backend ?? config.lastUsedBackend ?? config.defaultBackend;
    if (!commandExists(backend)) {
        throw new MissingToolError(backend);
    }
    const model = params.model ??
        (backend === "claude"
            ? config.backends.claude?.model
            : backend === "codex"
                ? config.backends.codex?.model
                : config.backends.acpx?.model);
    const effort = params.effort ?? effortForBackend(backend, config);
    const vcs = detectVcsMode(repoRoot, config.vcs);
    const active = await activeRunsForCheckout(repoRoot, repoRoot);
    if (params.queue && active.length > 0) {
        throw new Error("Queue mode is not implemented yet; omit --queue to create a worktree run.");
    }
    const useWorktree = Boolean(params.worktree || active.length > 0);
    const baseCommit = await baseRevision(repoRoot, vcs, useWorktree);
    const targetBranch = await targetRevision(repoRoot, vcs);
    const id = newRunId(params.task);
    const worktreeInfo = useWorktree
        ? await createRunWorkspace({ vcs, repoRoot, runId: id, task: params.task, baseCommit })
        : { path: repoRoot, branch: undefined, workspaceName: undefined };
    const run = await createRunRecord({
        id,
        repoRoot,
        task: params.task,
        backend,
        model,
        effort,
        targetBranch,
        baseCommit,
        vcs,
        useWorktree,
        worktreeBranch: worktreeInfo.branch,
        worktreeWorkspaceName: worktreeInfo.workspaceName,
        worktreePath: worktreeInfo.path,
    });
    await emit(run, {
        ts: nowIso(),
        runId: run.id,
        type: "run.created",
        message: useWorktree
            ? `Created ${workspaceKind(vcs)} ${shortenHome(worktreeInfo.path)}`
            : "Created run in current checkout",
    });
    await writeAgentContext(repoRoot);
    await rememberBackendSelection({
        backend,
        model: params.model,
        effort: params.effort,
        updateModel: params.model !== undefined,
        updateEffort: params.effort !== undefined,
    });
    const worker = spawn(process.execPath, [process.argv[1] ?? "", "__worker", "--repo", repoRoot, "--run", run.id], {
        cwd: repoRoot,
        detached: true,
        stdio: "ignore",
    });
    worker.unref();
    run.process = {
        pid: worker.pid,
        startedAt: nowIso(),
    };
    run.status = "running";
    await saveRunRecord(run);
    if (params.json) {
        console.log(JSON.stringify(run, null, 2));
        return run;
    }
    if (!params.quiet && !params.silent) {
        console.log(`Started ${run.id}`);
        console.log(`  backend: ${backend}${model ? ` (${model})` : ""}`);
        console.log(`  mode:    ${useWorktree ? `${workspaceKind(vcs)} ${shortenHome(worktreeInfo.path)}` : "current checkout"}`);
    }
    if (params.detach || !isTty()) {
        if (params.silent) {
            return run;
        }
        if (params.quiet) {
            console.log(`Started ${run.id} in background. Use /watch ${run.id} or rudder watch ${run.id}.`);
        }
        else {
            console.log(`  watch:   rudder watch ${run.id}`);
        }
        return run;
    }
    await watchRun({
        repoRoot,
        runId: run.id,
        follow: true,
        exitOnComplete: params.exitOnComplete,
        signal: params.watchSignal,
        view: params.view,
    });
    return run;
}
export async function startNativeRun(params) {
    const repoRoot = findRepoRoot();
    const config = await loadConfig();
    const backend = params.backend ?? toNativeBackend(config.lastUsedBackend ?? config.defaultBackend);
    if (!commandExists(backend)) {
        throw new MissingToolError(backend);
    }
    const model = params.model ??
        (backend === "claude"
            ? config.backends.claude?.model
            : config.backends.codex?.model);
    const effort = params.effort ?? effortForBackend(backend, config);
    const vcs = detectVcsMode(repoRoot, config.vcs);
    const baseCommit = await baseRevision(repoRoot, vcs, true);
    const targetBranch = await targetRevision(repoRoot, vcs);
    const id = newRunId(params.task);
    const worktreeInfo = await createRunWorkspace({ vcs, repoRoot, runId: id, task: params.task, baseCommit });
    const run = await createRunRecord({
        id,
        repoRoot,
        task: params.task,
        backend,
        model,
        effort,
        targetBranch,
        baseCommit,
        vcs,
        useWorktree: true,
        worktreeBranch: worktreeInfo.branch,
        worktreeWorkspaceName: worktreeInfo.workspaceName,
        worktreePath: worktreeInfo.path,
    });
    run.session = {
        ...(run.session ?? {}),
        nativeSessionId: backend === "claude" ? randomUUID() : run.session?.nativeSessionId,
        sessionName: params.tmuxSessionName,
    };
    run.status = "running";
    await saveRunRecord(run);
    await emit(run, {
        ts: nowIso(),
        runId: run.id,
        type: "run.created",
        message: `Created ${workspaceKind(vcs)} ${shortenHome(worktreeInfo.path)}`,
    });
    await writeAgentContext(repoRoot);
    await rememberBackendSelection({
        backend,
        model: params.model,
        effort: params.effort,
        updateModel: params.model !== undefined,
        updateEffort: params.effort !== undefined,
    });
    try {
        const backendAdapter = getBackend(backend);
        const health = await backendAdapter.verify();
        if (!health.ok) {
            throw missingBackendError(backend, health.message);
        }
        const spec = await createSpec(run);
        await emit(run, {
            ts: nowIso(),
            runId: run.id,
            type: "planner.spec",
            message: "Planner contract created",
            data: spec,
        });
        const contract = renderContract(spec);
        const codexCommand = backend === "codex" ? await ensureRudderCodexBinary() : undefined;
        const command = nativeAgentCommand({
            run,
            prompt: params.task,
            contract,
            codexCommand,
        });
        await ensureDir(path.dirname(outputPath(repoRoot, run.id)));
        const title = `${backend}:${shortRunTask(run)}`;
        const paneId = params.workerPaneId;
        if (paneId) {
            await normalizeTmuxDashboardLayout(repoRoot, params.tmuxSessionName);
            await respawnPane({
                paneId,
                cwd: worktreeInfo.path,
                title,
                command,
                logPath: outputPath(repoRoot, run.id),
            });
        }
        const launchedPaneId = paneId ?? await createAgentPane({
            sessionName: params.tmuxSessionName,
            cwd: worktreeInfo.path,
            title,
            command,
            logPath: outputPath(repoRoot, run.id),
        });
        run.terminal = {
            kind: "tmux",
            sessionName: params.tmuxSessionName,
            paneId: launchedPaneId,
            paneTitle: title,
            logPath: outputPath(repoRoot, run.id),
            launchedAt: nowIso(),
        };
        await saveRunRecord(run);
        await emit(run, {
            ts: nowIso(),
            runId: run.id,
            type: "run.started",
            message: `${backend} pane started`,
            data: { paneId: launchedPaneId, worktree: worktreeInfo.path },
        });
        await writeAgentContext(repoRoot);
        await normalizeTmuxDashboardLayout(repoRoot, params.tmuxSessionName);
        if (params.focus !== false && launchedPaneId) {
            await selectPane(launchedPaneId);
        }
        if (!params.silent) {
            console.log(`Started ${run.id}`);
        }
        return run;
    }
    catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        run.status = "failed";
        await saveRunRecord(run);
        await emit(run, { ts: nowIso(), runId: run.id, type: "run.failed", message });
        await writeAgentContext(repoRoot);
        throw error;
    }
}
export async function startNativePlan(params) {
    const repoRoot = findRepoRoot();
    const config = await loadConfig();
    const backend = params.backend ?? toNativeBackend(config.lastUsedBackend ?? config.defaultBackend);
    if (!commandExists(backend)) {
        throw new MissingToolError(backend);
    }
    const model = params.model ??
        (backend === "claude"
            ? config.backends.claude?.model
            : config.backends.codex?.model);
    const effort = params.effort ?? effortForBackend(backend, config);
    const vcs = detectVcsMode(repoRoot, config.vcs);
    const baseCommit = await baseRevision(repoRoot, vcs, false);
    const targetBranch = await targetRevision(repoRoot, vcs);
    const id = newRunId(params.task);
    const run = await createRunRecord({
        id,
        repoRoot,
        task: params.task,
        backend,
        model,
        effort,
        mode: "plan",
        targetBranch,
        baseCommit,
        vcs,
        useWorktree: false,
        worktreePath: repoRoot,
    });
    run.session = {
        ...(run.session ?? {}),
        nativeSessionId: backend === "claude" ? randomUUID() : run.session?.nativeSessionId,
        sessionName: params.tmuxSessionName,
    };
    run.status = "running";
    await saveRunRecord(run);
    await emit(run, {
        ts: nowIso(),
        runId: run.id,
        type: "run.created",
        message: "Started read-only plan in current checkout",
    });
    await rememberBackendSelection({
        backend,
        model: params.model,
        effort: params.effort,
        updateModel: params.model !== undefined,
        updateEffort: params.effort !== undefined,
    });
    try {
        const backendAdapter = getBackend(backend);
        const health = await backendAdapter.verify();
        if (!health.ok) {
            throw missingBackendError(backend, health.message);
        }
        const codexCommand = backend === "codex" ? await ensureRudderCodexBinary() : undefined;
        const command = nativeAgentCommand({
            run,
            prompt: buildPlanPrompt(params.task),
            contract: PLAN_MODE_CONTRACT,
            mode: "plan",
            codexCommand,
        });
        await ensureDir(path.dirname(outputPath(repoRoot, run.id)));
        const title = `${backend}:plan:${shortRunTask(run)}`;
        const paneId = params.workerPaneId;
        if (paneId) {
            await normalizeTmuxDashboardLayout(repoRoot, params.tmuxSessionName);
            await respawnPane({
                paneId,
                cwd: repoRoot,
                title,
                command,
                logPath: outputPath(repoRoot, run.id),
            });
        }
        const launchedPaneId = paneId ?? await createAgentPane({
            sessionName: params.tmuxSessionName,
            cwd: repoRoot,
            title,
            command,
            logPath: outputPath(repoRoot, run.id),
        });
        run.terminal = {
            kind: "tmux",
            sessionName: params.tmuxSessionName,
            paneId: launchedPaneId,
            paneTitle: title,
            logPath: outputPath(repoRoot, run.id),
            launchedAt: nowIso(),
        };
        await saveRunRecord(run);
        await emit(run, {
            ts: nowIso(),
            runId: run.id,
            type: "run.started",
            message: `${backend} read-only planner pane started`,
            data: { paneId: launchedPaneId, worktree: repoRoot },
        });
        await normalizeTmuxDashboardLayout(repoRoot, params.tmuxSessionName);
        if (params.focus !== false && launchedPaneId) {
            await selectPane(launchedPaneId);
        }
        if (!params.silent) {
            console.log(`Started plan ${run.id}`);
        }
        return run;
    }
    catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        run.status = "failed";
        await saveRunRecord(run);
        await emit(run, { ts: nowIso(), runId: run.id, type: "run.failed", message });
        throw error;
    }
}
export async function continueRun(params) {
    const repoRoot = findRepoRoot();
    const run = await loadRunRecord(repoRoot, params.runId);
    if (!run) {
        throw new Error(`Run not found: ${params.runId}`);
    }
    if (run.mode === "plan") {
        throw new Error("Plan runs are interactive read-only sessions. Continue the planner in its worker pane.");
    }
    if (isActiveStatus(run.status) && run.status !== "steering" && !params.interrupt) {
        throw new Error("That agent is still running. Wait for it to finish before sending another message.");
    }
    if (params.interrupt && run.process?.pid && processAlive(run.process.pid)) {
        process.kill(run.process.pid, "SIGTERM");
        await delay(350);
    }
    const prompt = params.prompt.trim();
    if (!prompt) {
        throw new Error("Missing prompt.");
    }
    const ts = nowIso();
    run.status = "running";
    run.currentPrompt = prompt;
    run.lastUserInputAt = ts;
    run.turns = [...(run.turns ?? []), { ts, prompt, source: "user" }];
    run.autoSteer = { count: 0, max: run.autoSteer?.max ?? 2 };
    await saveRunRecord(run);
    await emit(run, {
        ts,
        runId: run.id,
        type: "run.continued",
        message: params.interrupt ? "User interrupted and redirected the agent" : "User continued the agent",
        data: { prompt },
    });
    await writeAgentContext(repoRoot);
    const worker = spawn(process.execPath, [process.argv[1] ?? "", "__worker", "--repo", repoRoot, "--run", run.id], {
        cwd: repoRoot,
        detached: true,
        stdio: "ignore",
    });
    worker.unref();
    run.process = {
        pid: worker.pid,
        startedAt: nowIso(),
    };
    await saveRunRecord(run);
    if (!params.silent) {
        console.log(`Continued ${run.id}`);
    }
    return run;
}
export async function workerRun(repoRoot, runId) {
    const run = await loadRunRecord(repoRoot, runId);
    if (!run) {
        throw new Error(`Run not found: ${runId}`);
    }
    try {
        let current = run;
        while (true) {
            const pass = await runBackendPass(current);
            current = pass.run;
            if (pass.exitCode !== 0) {
                if (await newerWorkerOwnsRun(repoRoot, current)) {
                    await writeAgentContext(repoRoot);
                    return;
                }
                current.status = "failed";
                await saveRunRecord(current);
                await emit(current, {
                    ts: nowIso(),
                    runId,
                    type: "run.failed",
                    message: `Backend exited with ${pass.exitCode}`,
                });
                await writeAgentContext(repoRoot);
                return;
            }
            current.status = "verifying";
            await saveRunRecord(current);
            const verification = await verifyRun(current);
            current.verification = verification;
            await saveRunRecord(current);
            await emit(current, {
                ts: nowIso(),
                runId,
                type: "verifier.result",
                message: verification.notes,
                data: verification,
            });
            if (await shouldAutoSteer(current, verification)) {
                const waitingSince = nowIso();
                current.status = "steering";
                current.autoSteer = {
                    count: current.autoSteer?.count ?? 0,
                    max: current.autoSteer?.max ?? 2,
                    waitingSince,
                };
                await saveRunRecord(current);
                await emit(current, {
                    ts: waitingSince,
                    runId,
                    type: "steerer.waiting",
                    message: "Waiting 10 seconds before automatic steering",
                });
                await writeAgentContext(repoRoot);
                await delay(AUTO_STEER_DELAY_MS);
                const latest = await loadRunRecord(repoRoot, runId);
                if (!latest) {
                    return;
                }
                if (latest.lastUserInputAt && latest.lastUserInputAt > waitingSince) {
                    if (latest.status !== "running") {
                        latest.status = "completed";
                        await saveRunRecord(latest);
                        await emit(latest, {
                            ts: nowIso(),
                            runId,
                            type: "run.completed",
                            message: "Run completed; user follow-up took over steering",
                        });
                    }
                    await writeAgentContext(repoRoot);
                    return;
                }
                const steeringPrompt = buildSteeringPrompt(verification);
                latest.currentPrompt = steeringPrompt;
                latest.turns = [...(latest.turns ?? []), { ts: nowIso(), prompt: steeringPrompt, source: "steerer" }];
                latest.autoSteer = {
                    count: (latest.autoSteer?.count ?? 0) + 1,
                    max: latest.autoSteer?.max ?? 2,
                };
                latest.status = "running";
                await saveRunRecord(latest);
                await emit(latest, {
                    ts: nowIso(),
                    runId,
                    type: "steerer.prompt",
                    message: "Automatic steering prompt sent",
                    data: { prompt: steeringPrompt },
                });
                current = latest;
                continue;
            }
            current.status = verification.shouldContinue ? "failed" : "completed";
            await saveRunRecord(current);
            await emit(current, {
                ts: nowIso(),
                runId,
                type: current.status === "completed" ? "run.completed" : "run.failed",
                message: current.status === "completed"
                    ? "Run completed"
                    : `Run failed verification: ${verification.missing.join("; ")}`,
            });
            await writeAgentContext(repoRoot);
            return;
        }
    }
    catch (error) {
        if (await newerWorkerOwnsRun(repoRoot, run)) {
            await writeAgentContext(repoRoot);
            return;
        }
        const message = error instanceof Error ? error.message : String(error);
        run.status = "failed";
        run.process = {
            ...(run.process ?? {}),
            endedAt: nowIso(),
            exitCode: 1,
            signal: null,
        };
        await saveRunRecord(run);
        await emit(run, {
            ts: nowIso(),
            runId,
            type: "run.failed",
            message,
        });
        await writeAgentContext(repoRoot);
    }
}
async function newerWorkerOwnsRun(repoRoot, staleRun) {
    const latest = await loadRunRecord(repoRoot, staleRun.id);
    return Boolean(latest &&
        isActiveStatus(latest.status) &&
        latest.process?.startedAt &&
        staleRun.process?.startedAt &&
        latest.process.startedAt !== staleRun.process.startedAt);
}
async function runBackendPass(run) {
    if (run.mode === "plan") {
        throw new Error("Plan runs do not use the background worker.");
    }
    const spec = await createSpec(run);
    await emit(run, {
        ts: nowIso(),
        runId: run.id,
        type: "planner.spec",
        message: "Planner contract created",
        data: spec,
    });
    const backend = getBackend(run.backend);
    const health = await backend.verify();
    if (!health.ok) {
        throw missingBackendError(run.backend, health.message);
    }
    const exitCode = await backend.run({
        run,
        prompt: run.currentPrompt ?? run.task,
        contract: renderContract(spec),
    }, async (event) => {
        await emit(run, event);
    });
    run.process = {
        ...(run.process ?? {}),
        endedAt: nowIso(),
        exitCode,
        signal: null,
    };
    await saveRunRecord(run);
    return { run, exitCode };
}
async function shouldAutoSteer(run, verification) {
    if (run.mode === "plan") {
        return false;
    }
    if (run.status === "cancelled" || run.status === "merged" || run.status === "merge-conflict") {
        return false;
    }
    const max = run.autoSteer?.max ?? 2;
    const count = run.autoSteer?.count ?? 0;
    if (count >= max) {
        return false;
    }
    if (verification.shouldContinue || verification.missing.length > 0) {
        return true;
    }
    if (count > 0) {
        return false;
    }
    return await runHasChanges(run);
}
function buildSteeringPrompt(verification) {
    const missing = verification.missing.length
        ? `\nKnown gaps:\n${verification.missing.map((item) => `- ${item}`).join("\n")}`
        : "";
    return [
        "Rudder automatic steering check:",
        "Pause and review the work you just completed.",
        "What remaining tasks are there, if any?",
        "Does the implementation look correct and scoped?",
        "Were the relevant tests/checks run? If not, run the smallest useful checks now or explain why not.",
        "If anything is missing, fix it. If everything is done, say that clearly and do not make unnecessary changes.",
        missing,
    ]
        .filter(Boolean)
        .join("\n");
}
export async function writeAgentContext(repoRoot) {
    const runs = await listRuns(repoRoot);
    const active = runs.filter((run) => isActiveStatus(run.status));
    const lines = [
        "# Rudder-Specific Context",
        "",
        "This file is generated by Rudder. It is not user-authored repo documentation. Use it to coordinate with other Rudder agents in this checkout.",
        "",
        `Updated: ${nowIso()}`,
        "",
        "## Active Agents",
        ...(active.length
            ? active.map((run) => formatAgentContextRun(run))
            : ["- None"]),
        "",
        "## Recent Agents",
        ...runs.slice(0, 12).map((run) => formatAgentContextRun(run)),
        "",
    ];
    await writeRudderContextFiles(repoRoot, active, `${lines.join("\n")}\n`);
}
function formatAgentContextRun(run) {
    const location = run.worktree.enabled
        ? `${workspaceKind(run.vcs ?? "git")}=${shortenHome(run.worktree.path)}`
        : "current checkout";
    const prompt = run.currentPrompt && run.currentPrompt !== run.task ? ` current="${run.currentPrompt.slice(0, 140)}"` : "";
    return `- ${run.id}: ${run.status}, ${run.backend}, ${location}, task="${run.task.slice(0, 140)}"${prompt}`;
}
async function writeRudderContextFiles(repoRoot, activeRuns, content) {
    await ensureLine(path.join(repoRoot, ".gitignore"), "RUDDER.md");
    const workspaces = new Set([
        repoRoot,
        ...activeRuns.map((run) => run.worktree.path),
    ]);
    for (const workspace of workspaces) {
        await ensureRudderExcluded(workspace);
        await ensureDir(path.dirname(agentContextPath(workspace)));
        await fsp.writeFile(agentContextPath(workspace), content, "utf8");
    }
}
async function ensureRudderExcluded(workspace) {
    const result = await runCommand("git", ["rev-parse", "--git-path", "info/exclude"], {
        cwd: workspace,
        allowFailure: true,
    });
    const excludePath = result.stdout.trim();
    if (!excludePath) {
        return;
    }
    await ensureLine(path.resolve(workspace, excludePath), "RUDDER.md");
}
async function ensureLine(filePath, line) {
    const existing = await fsp.readFile(filePath, "utf8").catch(() => "");
    const lines = existing.split(/\r?\n/).map((item) => item.trim());
    if (lines.includes(line)) {
        return;
    }
    const prefix = existing && !existing.endsWith("\n") ? "\n" : "";
    await ensureDir(path.dirname(filePath));
    await fsp.appendFile(filePath, `${prefix}${line}\n`, "utf8");
}
function delay(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
function toNativeBackend(backend) {
    return backend === "codex" ? "codex" : "claude";
}
async function createRunWorkspace(params) {
    if (params.vcs === "jj") {
        return await createRunJjWorkspace(params);
    }
    return await createRunWorktree(params);
}
async function baseRevision(repoRoot, vcs, useWorktree) {
    if (vcs === "jj") {
        return await currentJjChangeId(repoRoot) || "";
    }
    return useWorktree ? await worktreeBaseCommit(repoRoot) : await currentCommit(repoRoot);
}
async function targetRevision(repoRoot, vcs) {
    if (vcs === "jj") {
        return await currentJjChangeId(repoRoot) || "@";
    }
    return await currentBranch(repoRoot);
}
function workspaceKind(vcs) {
    return vcs === "jj" ? "jj workspace" : "worktree";
}
function effortForBackend(backend, config) {
    if (backend === "claude") {
        return config.backends.claude?.effort;
    }
    if (backend === "codex") {
        return config.backends.codex?.reasoningEffort ?? config.backends.codex?.effort;
    }
    return config.backends.acpx?.reasoningEffort ?? config.backends.acpx?.effort;
}
function shortRunTask(run) {
    return taskDisplayLabel(run, 34) || "agent";
}
export async function statusRuns(options) {
    const repoRoot = findRepoRoot();
    const runs = await listRuns(repoRoot);
    const active = runs.filter((run) => isActiveStatus(run.status));
    if (options?.json) {
        console.log(JSON.stringify({ repoRoot, active, runs }, null, 2));
        return;
    }
    console.log(`Repo: ${repoRoot}`);
    if (active.length === 0) {
        console.log("No active runs.");
        return;
    }
    for (const run of active) {
        const alive = processAlive(run.process?.pid) ? "alive" : "stale";
        console.log(`${run.id}  ${run.status}  ${alive}  ${run.backend}  ${run.task}`);
    }
}
export async function listProjectRuns(options) {
    const repoRoot = findRepoRoot();
    const runs = await listRuns(repoRoot);
    if (options?.json) {
        console.log(JSON.stringify(runs, null, 2));
        return;
    }
    for (const run of runs) {
        const wt = run.worktree.enabled ? ` ${workspaceKind(run.vcs ?? "git")}=${shortenHome(run.worktree.path)}` : "";
        console.log(`${run.id}  ${run.status}  ${run.backend}${wt}  ${run.task}`);
    }
}
export async function watchRun(params) {
    const repoRoot = params.repoRoot ?? findRepoRoot();
    const run = await resolveRun(repoRoot, params.runId);
    if (!run) {
        throw new Error("No runs found.");
    }
    const file = eventsPath(repoRoot, run.id);
    await waitForFile(file);
    let offset = 0;
    const initial = await fsp.readFile(file, "utf8").catch(() => "");
    offset = Buffer.byteLength(initial);
    const view = params.view ?? "default";
    const renderer = createEventRenderer(view);
    renderer.print(initial);
    if (!params.follow) {
        return;
    }
    const alreadyDone = await loadRunRecord(repoRoot, run.id);
    if (alreadyDone && !isActiveStatus(alreadyDone.status)) {
        if (params.exitOnComplete !== false) {
            process.exitCode = terminalExitCode(alreadyDone.status);
        }
        return;
    }
    if (view === "default") {
        console.log(`Watching ${run.id}. Ctrl-C detaches; use 'rudder stop ${run.id}' to cancel.`);
    }
    await followFile(file, offset, async (chunk) => {
        renderer.print(chunk);
        const latest = await loadRunRecord(repoRoot, run.id);
        if (latest && !isActiveStatus(latest.status)) {
            if (params.exitOnComplete === false) {
                renderer.finish();
                return false;
            }
            renderer.finish();
            process.exitCode = terminalExitCode(latest.status);
            process.exit();
        }
        return true;
    }, params.signal);
}
export async function printLogs(runId, follow = false) {
    const repoRoot = findRepoRoot();
    const run = await resolveRun(repoRoot, runId);
    if (!run) {
        throw new Error("No runs found.");
    }
    const file = outputPath(repoRoot, run.id);
    if (!(await pathExists(file))) {
        return;
    }
    const output = await fsp.readFile(file, "utf8");
    process.stdout.write(output);
    if (follow) {
        await followFile(file, Buffer.byteLength(output), async (chunk) => {
            process.stdout.write(chunk);
        });
    }
}
export async function stopRun(runId, options) {
    const repoRoot = findRepoRoot();
    const run = await loadRunRecord(repoRoot, runId);
    if (!run) {
        throw new Error(`Run not found: ${runId}`);
    }
    if (run.process?.pid && processAlive(run.process.pid)) {
        process.kill(run.process.pid, "SIGTERM");
    }
    if (run.terminal?.kind === "tmux") {
        await killPane(run.terminal.paneId).catch(() => undefined);
    }
    run.status = "cancelled";
    run.process = {
        ...(run.process ?? {}),
        endedAt: nowIso(),
        signal: "SIGTERM",
    };
    await saveRunRecord(run);
    await emit(run, { ts: nowIso(), runId, type: "run.cancelled", message: "Run cancelled" });
    await writeAgentContext(repoRoot);
    if (!options?.silent) {
        console.log(`Stopped ${runId}`);
    }
}
export async function mergeRun(runId, allowDirty = false, options) {
    const repoRoot = findRepoRoot();
    const run = await loadRunRecord(repoRoot, runId);
    if (!run) {
        throw new Error(`Run not found: ${runId}`);
    }
    const config = await loadConfig();
    const merged = await mergeRunIntoCurrentBranch(run, allowDirty, config.mergeStrategy);
    await emit(merged, {
        ts: nowIso(),
        runId,
        type: "merge.result",
        message: mergeResultMessage(merged),
        data: (merged.merge ?? {}),
    });
    if (merged.merge?.status === "merged") {
        await writeAgentContext(repoRoot);
        if (!options?.silent) {
            console.log(`Merged ${runId}`);
        }
        return merged;
    }
    await writeAgentContext(repoRoot);
    if (!options?.silent) {
        if (merged.merge?.status === "conflict") {
            const kind = merged.merge.conflictKind === "rebase" ? "Rebase conflict" : "Merge conflict";
            console.log(`${kind} for ${runId}`);
            for (const file of merged.merge?.conflictedFiles ?? []) {
                console.log(`  ${file}`);
            }
            if (merged.merge.conflictKind === "rebase") {
                console.log(`Resolve in ${shortenHome(merged.worktree.path)}, run git rebase --continue, then retry rudder merge ${runId}.`);
            }
        }
        else {
            console.log(`Merge failed for ${runId}: ${merged.merge?.error ?? "unknown error"}`);
        }
    }
    return merged;
}
export async function syncRun(runId, options) {
    const repoRoot = findRepoRoot();
    const run = await resolveRun(repoRoot, runId);
    if (!run) {
        throw new Error("No runs found.");
    }
    if (!run.worktree.enabled || !run.worktree.branch) {
        throw new Error(`Run ${run.id} has no worktree branch to sync.`);
    }
    if (isActiveStatus(run.status)) {
        throw new Error(`Run ${run.id} is still active; wait for it to finish before syncing.`);
    }
    if (run.status === "merged") {
        throw new Error(`Run ${run.id} is already merged.`);
    }
    const current = await currentBranch(repoRoot);
    const baseBranch = current === "HEAD" ? run.targetBranch : current;
    const synced = await syncRunWorktree(run, baseBranch);
    await emit(synced, {
        ts: nowIso(),
        runId: synced.id,
        type: "sync.result",
        message: syncResultMessage(synced),
        data: (synced.sync ?? {}),
    });
    await writeAgentContext(repoRoot);
    if (!options?.silent) {
        if (synced.sync?.status === "synced") {
            console.log(`Synced ${synced.id} with ${synced.sync.baseBranch ?? synced.targetBranch}`);
        }
        else if (synced.sync?.status === "conflict") {
            console.log(`Rebase conflict for ${synced.id}`);
            for (const file of synced.sync.conflictedFiles ?? []) {
                console.log(`  ${file}`);
            }
            console.log(`Resolve in ${shortenHome(synced.worktree.path)}, run git rebase --continue, then retry rudder sync ${synced.id}.`);
        }
        else {
            console.log(`Sync failed for ${synced.id}: ${synced.sync?.error ?? "unknown error"}`);
        }
    }
    return synced;
}
export async function deleteRun(runId, options) {
    const repoRoot = findRepoRoot();
    const run = await loadRunRecord(repoRoot, runId);
    if (!run) {
        throw new Error(`Run not found: ${runId}`);
    }
    let mergeError;
    if (options?.mergeFirst) {
        try {
            await mergeRun(runId, false, { silent: true });
        }
        catch (error) {
            mergeError = error;
        }
    }
    const latest = await loadRunRecord(repoRoot, runId) ?? run;
    if (options?.mergeFirst && latest.merge?.status === "conflict") {
        throw new Error(`Merge conflict for ${runId}; resolve it before deleting the run.`);
    }
    if (mergeError && !options?.force) {
        const message = mergeError instanceof Error ? mergeError.message : String(mergeError);
        throw new Error(`Merge failed for ${runId}; run was not deleted. ${message}`);
    }
    if (latest.process?.pid && processAlive(latest.process.pid)) {
        process.kill(latest.process.pid, "SIGTERM");
    }
    if (latest.terminal?.kind === "tmux") {
        await killPane(latest.terminal.paneId).catch(() => undefined);
    }
    if (latest.worktree.enabled) {
        await removeRunWorkspace(latest, options?.force ?? true).catch(() => undefined);
    }
    await fsp.rm(runDir(repoRoot, runId), { recursive: true, force: true });
    await writeAgentContext(repoRoot);
    if (!options?.silent) {
        console.log(`Deleted ${runId}`);
    }
}
function mergeResultMessage(run) {
    if (run.merge?.status === "merged") {
        return run.merge.strategy === "rebase"
            ? "Rebased and fast-forward merged successfully"
            : "Merged successfully";
    }
    if (run.merge?.status === "conflict") {
        const files = (run.merge.conflictedFiles ?? []).join(", ") || "unknown files";
        if (run.merge.conflictKind === "rebase") {
            return `Rebase conflict before merge: ${files}. Resolve in the worktree, run git rebase --continue, then retry merge.`;
        }
        return `Merge conflict: ${files}`;
    }
    if (run.merge?.status === "failed") {
        return `Merge failed: ${run.merge.error ?? "unknown error"}`;
    }
    return "Merge did not complete";
}
function syncResultMessage(run) {
    if (run.sync?.status === "synced") {
        return `Synced with ${run.sync.baseBranch ?? run.targetBranch}`;
    }
    if (run.sync?.status === "conflict") {
        const files = (run.sync.conflictedFiles ?? []).join(", ") || "unknown files";
        return `Rebase conflict while syncing: ${files}. Resolve in the worktree, run git rebase --continue, then retry sync.`;
    }
    if (run.sync?.status === "failed") {
        return `Sync failed: ${run.sync.error ?? "unknown error"}`;
    }
    return "Sync did not complete";
}
export async function cleanupRuns(force = false) {
    const repoRoot = findRepoRoot();
    const runs = await listRuns(repoRoot);
    for (const run of runs) {
        if (!canCleanupRun(run, force)) {
            continue;
        }
        try {
            await removeRunWorkspace(run, force);
            console.log(`Removed ${shortenHome(run.worktree.path)}`);
        }
        catch {
            // Best-effort cleanup matches the existing git worktree behavior.
        }
    }
    await writeAgentContext(repoRoot);
}
function canCleanupRun(run, force) {
    if (!run.worktree.enabled) {
        return false;
    }
    if (force) {
        return true;
    }
    if ((run.vcs ?? "git") === "jj") {
        return run.status === "merged" || run.status === "completed";
    }
    return run.status === "merged";
}
export async function reconcileNativeTerminals(repoRoot) {
    const runs = await listRuns(repoRoot);
    let touched = false;
    let contextTouched = false;
    for (const run of runs) {
        if (run.terminal?.kind !== "tmux") {
            continue;
        }
        if (run.status === "running") {
            const exitCode = await paneExitStatus(run.terminal.paneId).catch(() => null);
            if (exitCode === null) {
                continue;
            }
            run.process = {
                ...(run.process ?? {}),
                endedAt: nowIso(),
                exitCode,
                signal: null,
            };
            if (exitCode !== 0) {
                run.status = "failed";
                await saveRunRecord(run);
                await emit(run, { ts: nowIso(), runId: run.id, type: "run.failed", message: `Backend exited with ${exitCode}` });
                touched = true;
                contextTouched = contextTouched || run.mode !== "plan";
                continue;
            }
            if (run.mode === "plan") {
                run.status = "completed";
                await saveRunRecord(run);
                await emit(run, {
                    ts: nowIso(),
                    runId: run.id,
                    type: "run.completed",
                    message: "Plan completed",
                });
                touched = true;
                continue;
            }
            run.status = "verifying";
            await saveRunRecord(run);
            const verification = await verifyRun(run);
            run.verification = verification;
            await saveRunRecord(run);
            await emit(run, {
                ts: nowIso(),
                runId: run.id,
                type: "verifier.result",
                message: verification.notes,
                data: verification,
            });
            if (await shouldAutoSteer(run, verification)) {
                const waitingSince = nowIso();
                run.status = "steering";
                run.autoSteer = {
                    count: run.autoSteer?.count ?? 0,
                    max: run.autoSteer?.max ?? 2,
                    waitingSince,
                };
                await saveRunRecord(run);
                await emit(run, { ts: waitingSince, runId: run.id, type: "steerer.waiting", message: "Waiting 10 seconds before automatic steering" });
            }
            else {
                run.status = verification.shouldContinue ? "failed" : "completed";
                await saveRunRecord(run);
                await emit(run, {
                    ts: nowIso(),
                    runId: run.id,
                    type: run.status === "completed" ? "run.completed" : "run.failed",
                    message: run.status === "completed" ? "Run completed" : `Run failed verification: ${verification.missing.join("; ")}`,
                });
            }
            touched = true;
            contextTouched = true;
            continue;
        }
        if (run.mode !== "plan" && run.status === "steering" && run.autoSteer?.waitingSince && Date.now() - Date.parse(run.autoSteer.waitingSince) >= AUTO_STEER_DELAY_MS) {
            const steeringPrompt = buildSteeringPrompt(run.verification ?? { satisfied: [], missing: [], notes: "", shouldContinue: false });
            run.currentPrompt = steeringPrompt;
            run.turns = [...(run.turns ?? []), { ts: nowIso(), prompt: steeringPrompt, source: "steerer" }];
            run.autoSteer = {
                count: (run.autoSteer?.count ?? 0) + 1,
                max: run.autoSteer?.max ?? 2,
            };
            run.status = "running";
            await saveRunRecord(run);
            await emit(run, { ts: nowIso(), runId: run.id, type: "steerer.prompt", message: "Automatic steering prompt sent", data: { prompt: steeringPrompt } });
            const spec = await createSpec(run);
            const codexCommand = run.backend === "codex" ? await ensureRudderCodexBinary() : undefined;
            const command = nativeAgentCommand({ run, prompt: steeringPrompt, contract: renderContract(spec), codexCommand });
            await respawnPane({
                paneId: run.terminal.paneId,
                cwd: run.worktree.path,
                title: `${run.backend}:${shortRunTask(run)}`,
                command,
                logPath: outputPath(repoRoot, run.id),
            });
            await emit(run, { ts: nowIso(), runId: run.id, type: "run.started", message: `${run.backend} pane restarted`, data: { paneId: run.terminal.paneId } });
            touched = true;
            contextTouched = true;
        }
    }
    if (touched && contextTouched) {
        await writeAgentContext(repoRoot);
    }
}
async function emit(run, event) {
    await ensureDir(runDir(run.repoRoot, run.id));
    await appendEvent(run.repoRoot, event);
}
async function waitForFile(file) {
    for (let i = 0; i < 50; i += 1) {
        if (await pathExists(file)) {
            return;
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
    }
}
function createEventRenderer(view) {
    let sawStreamingText = false;
    let partialOpen = false;
    return {
        print(raw) {
            for (const line of raw.split(/\r?\n/)) {
                if (!line.trim()) {
                    continue;
                }
                try {
                    const event = JSON.parse(line);
                    if (view === "shell") {
                        const rendered = renderShellEvent(event, {
                            sawStreamingText,
                            partialOpen,
                        });
                        sawStreamingText = rendered.sawStreamingText;
                        partialOpen = rendered.partialOpen;
                        if (rendered.text) {
                            if (rendered.inline) {
                                process.stdout.write(rendered.text);
                            }
                            else {
                                if (partialOpen) {
                                    process.stdout.write("\n");
                                    partialOpen = false;
                                }
                                console.log(rendered.text);
                            }
                        }
                        continue;
                    }
                    printDefaultEvent(event);
                }
                catch {
                    console.log(line);
                }
            }
        },
        finish() {
            if (partialOpen) {
                process.stdout.write("\n");
                partialOpen = false;
            }
        },
    };
}
function printDefaultEvent(event) {
    if (event.type === "backend.output" || event.type === "backend.error") {
        if (event.message) {
            console.log(event.message);
        }
        else if (event.data) {
            const text = formatBackendData(event.data);
            if (text) {
                console.log(text);
            }
        }
    }
    else if (event.message) {
        console.log(`[rudder] ${event.message}`);
    }
}
function renderShellEvent(event, state) {
    let sawStreamingText = state.sawStreamingText;
    let partialOpen = state.partialOpen;
    if (event.type === "run.created") {
        const message = event.message?.startsWith("Created worktree ")
            ? event.message.replace("Created worktree ", "worktree ")
            : undefined;
        return { text: message, sawStreamingText, partialOpen };
    }
    if (event.type === "planner.spec") {
        return { sawStreamingText, partialOpen };
    }
    if (event.type === "run.started") {
        const command = objectField(event.data, "command");
        return {
            text: command ? `running ${command}` : undefined,
            sawStreamingText,
            partialOpen,
        };
    }
    if (event.type === "backend.output" || event.type === "backend.error") {
        const rendered = renderBackendForShell(event.data ?? event.message, event.type === "backend.error", sawStreamingText);
        if (rendered.sawStreamingText) {
            sawStreamingText = true;
        }
        if (rendered.inline) {
            partialOpen = true;
        }
        else if (rendered.text) {
            partialOpen = false;
        }
        return { ...rendered, sawStreamingText, partialOpen };
    }
    if (event.type === "backend.exit") {
        return { sawStreamingText, partialOpen };
    }
    if (event.type === "verifier.result") {
        const missing = Array.isArray(event.data?.missing)
            ? event.data.missing.filter((item) => typeof item === "string")
            : [];
        return {
            text: missing.length ? `verification needs follow-up: ${missing.join("; ")}` : undefined,
            sawStreamingText,
            partialOpen,
        };
    }
    if (event.type === "run.completed") {
        return { text: "done", sawStreamingText, partialOpen };
    }
    if (event.type === "run.failed") {
        return { text: event.message ? `failed: ${event.message}` : "failed", sawStreamingText, partialOpen };
    }
    if (event.type === "run.cancelled") {
        return { text: "cancelled", sawStreamingText, partialOpen };
    }
    if (event.type === "merge.result") {
        return { text: event.message, sawStreamingText, partialOpen };
    }
    return { sawStreamingText, partialOpen };
}
function renderBackendForShell(data, stderr, sawStreamingText) {
    if (typeof data === "string") {
        return { text: stderr ? `error: ${data}` : data };
    }
    if (!data || typeof data !== "object") {
        return {};
    }
    const record = data;
    if (record.type === "stream_event" && isRecord(record.event)) {
        const event = record.event;
        if (event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string") {
            return { text: event.delta.text, inline: true, sawStreamingText: true };
        }
        return {};
    }
    if (record.type === "assistant") {
        if (sawStreamingText) {
            return {};
        }
        const text = textFromAssistantMessage(record.message);
        return text ? { text } : {};
    }
    if (record.type === "result") {
        if (record.subtype === "success") {
            const text = typeof record.result === "string" && !sawStreamingText ? record.result.trim() : "";
            return text ? { text } : {};
        }
        const errors = Array.isArray(record.errors) ? record.errors.filter((item) => typeof item === "string") : [];
        return { text: `error: ${errors.join(", ") || String(record.subtype ?? "unknown")}` };
    }
    if (record.type === "system") {
        return {};
    }
    return {};
}
function textFromAssistantMessage(message) {
    if (!isRecord(message)) {
        return "";
    }
    const content = message.content;
    if (typeof content === "string") {
        return content;
    }
    if (!Array.isArray(content)) {
        return "";
    }
    return content
        .map((block) => {
        if (isRecord(block) && block.type === "text" && typeof block.text === "string") {
            return block.text;
        }
        return "";
    })
        .filter(Boolean)
        .join("\n");
}
function objectField(data, key) {
    return isRecord(data) && typeof data[key] === "string" ? data[key] : undefined;
}
function isRecord(value) {
    return Boolean(value && typeof value === "object" && !Array.isArray(value));
}
function formatBackendData(data) {
    if (typeof data === "string") {
        return data;
    }
    if (data && typeof data === "object") {
        const record = data;
        if (record.type === "system" || record.type === "rate_limit_event" || record.type === "tool_use_summary") {
            return "";
        }
        if (record.type === "stream_event" && isRecord(record.event)) {
            const event = record.event;
            if (event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string") {
                return event.delta.text;
            }
            return "";
        }
        if (record.type === "assistant") {
            return textFromAssistantMessage(record.message);
        }
        if (record.type === "result") {
            if (record.subtype === "success" && typeof record.result === "string") {
                return record.result;
            }
            if (Array.isArray(record.errors)) {
                return record.errors.filter((item) => typeof item === "string").join(", ");
            }
        }
        for (const key of ["message", "text", "content", "delta"]) {
            if (typeof record[key] === "string") {
                return record[key];
            }
        }
        if (typeof record.type === "string") {
            return `[${record.type}] ${JSON.stringify(record)}`;
        }
    }
    return JSON.stringify(data);
}
async function followFile(file, startOffset, onChunk, signal) {
    let offset = startOffset;
    while (!signal?.aborted) {
        const stat = await fsp.stat(file).catch(() => null);
        if (stat && stat.size > offset) {
            const handle = await fsp.open(file, "r");
            try {
                const length = stat.size - offset;
                const buffer = Buffer.alloc(length);
                await handle.read(buffer, 0, length, offset);
                offset = stat.size;
                const keepGoing = await onChunk(buffer.toString("utf8"));
                if (keepGoing === false) {
                    return;
                }
            }
            finally {
                await handle.close();
            }
        }
        await new Promise((resolve) => setTimeout(resolve, 350));
    }
}
function isActiveStatus(status) {
    return status === "created" || status === "running" || status === "steering" || status === "verifying";
}
function terminalExitCode(status) {
    return status === "completed" || status === "merged" ? 0 : 1;
}
//# sourceMappingURL=run-manager.js.map