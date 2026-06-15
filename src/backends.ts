import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import type { BackendAdapter, BackendId, RunRequest, RudderEvent, AuthProfileStore, EffortLevel } from "./types.js";
import { CODEX_RUDDER_CONFIG_ARGS, codexLaunchEnv, ensureRudderCodexBinary } from "./codex-binary.js";
import { loadAuthStore, loadConfig, saveRunRecord } from "./state.js";
import { normalizeEffortForBackend } from "./effort.js";
import {
  commandExists,
  formatMissingToolMessage,
  isMissingToolSpawnError,
  isRecord,
  lineSplitBuffer,
  MissingToolError,
  nowIso,
  parseJsonLine,
  runCommand,
  stripRudderPromptWrappers,
  textFromAssistantMessage,
} from "./util.js";

export function getBackend(id: BackendId): BackendAdapter {
  // TEST-ONLY deterministic hook: when RUDDER_FAKE_BACKEND=1, every backend is a
  // scripted fake that applies file edits encoded in the node prompt and exits 0,
  // with no real model call or auth. This lets the end-to-end orchestrator test
  // drive the REAL pipeline (schedule -> isolated jj workspace -> worker -> verify
  // -> jj merge -> unblock dependents) repeatably. See `fakeBackend`.
  if (process.env.RUDDER_FAKE_BACKEND === "1") {
    return fakeBackend(id);
  }
  if (id === "claude") {
    return claudeBackend();
  }
  if (id === "codex") {
    return codexBackend();
  }
  return acpxBackend();
}

/**
 * TEST-ONLY backend. It reads `[[FAKE_FILE:relpath]]…[[/FAKE_FILE]]` blocks from
 * the node prompt (`run.task`, which is stable across auto-steer passes) and
 * writes each into the run's isolated workspace, then emits a `backend.output`
 * line that mentions verification so the local verifier is satisfied. Parsing
 * `run.task` (not the per-turn prompt) keeps the second auto-steer pass writing
 * the same files idempotently.
 */
function fakeBackend(id: BackendId): BackendAdapter {
  return {
    id,
    async verify() {
      return { ok: true, message: "fake backend (RUDDER_FAKE_BACKEND=1)" };
    },
    async run(request, emit) {
      const fsp = await import("node:fs/promises");
      const path = await import("node:path");
      // Mirror spawnAndStream's bookkeeping so downstream readers (and the e2e
      // test's ordering assertions) see the same run.process timing shape.
      request.run.process = {
        ...(request.run.process ?? {}),
        startedAt: nowIso(),
      };
      request.run.status = "running";
      await saveRunRecord(request.run);
      const root = request.run.worktree?.path ?? request.run.repoRoot;
      const source = request.run.task ?? request.prompt ?? "";
      const blocks = [...source.matchAll(/\[\[FAKE_FILE:(.+?)\]\]\n([\s\S]*?)\n\[\[\/FAKE_FILE\]\]/g)];
      const written: string[] = [];
      for (const match of blocks) {
        const rel = (match[1] ?? "").trim();
        const content = match[2] ?? "";
        if (!rel) {
          continue;
        }
        const dest = path.resolve(root, rel);
        // Stay inside the workspace: a directive must never escape via `..`.
        if (!dest.startsWith(path.resolve(root))) {
          continue;
        }
        await fsp.mkdir(path.dirname(dest), { recursive: true });
        await fsp.writeFile(dest, content.endsWith("\n") ? content : `${content}\n`, "utf8");
        written.push(rel);
      }
      await emit({
        ts: nowIso(),
        runId: request.run.id,
        type: "backend.output",
        message: written.length
          ? `fake backend wrote ${written.join(", ")} and ran the verification checks (tests pass)\n`
          : "fake backend made no file changes; verification check ran\n",
      });
      request.run.process = {
        ...(request.run.process ?? {}),
        endedAt: nowIso(),
        exitCode: 0,
        signal: null,
      };
      await saveRunRecord(request.run);
      return 0;
    },
  };
}

function claudeBackend(): BackendAdapter {
  return {
    id: "claude",
    async verify() {
      return commandExists("claude")
        ? { ok: true, message: "claude found" }
        : { ok: false, message: formatMissingToolMessage("claude") };
    },
    async run(request, emit) {
      const prompt = stripRudderPromptWrappers(request.prompt);
      const existingSessionId = request.run.session?.nativeSessionId;
      const isFollowUp = (request.run.turns?.length ?? 0) > 1;
      const sessionId = existingSessionId ?? randomUUID();
      request.run.session = {
        ...(request.run.session ?? {}),
        nativeSessionId: sessionId,
      };
      await saveRunRecord(request.run);
      const env = await backendEnv("anthropic");
      const effort = normalizeEffortForBackend("claude", request.run.effort);
      const args = compact([
        "-p",
        prompt,
        "--model",
        request.run.model || "sonnet",
        effort ? "--effort" : undefined,
        effort,
        "--permission-mode",
        "bypassPermissions",
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--append-system-prompt",
        request.contract,
        ...(isFollowUp && existingSessionId
          ? ["--resume", existingSessionId, "--fork-session"]
          : ["--session-id", sessionId]),
      ]);
      return await spawnAndStream({
        command: "claude",
        args,
        cwd: request.run.worktree.path,
        env,
        request,
        emit,
      });
    },
  };
}

function codexBackend(): BackendAdapter {
  return {
    id: "codex",
    async verify() {
      try {
        const binary = await ensureRudderCodexBinary();
        return { ok: true, message: `rudder-codex found at ${binary}` };
      } catch (error) {
        return { ok: false, message: error instanceof Error ? error.message : String(error) };
      }
    },
    async run(request, emit) {
      const prompt = stripRudderPromptWrappers(request.prompt);
      const command = await ensureRudderCodexBinary();
      const env = await codexLaunchEnv(await backendEnv("openai"));
      const effort = normalizeEffortForBackend("codex", request.run.effort);
      const args = compact([
        "exec",
        "--json",
        "--color",
        "never",
        "--model",
        request.run.model || "gpt-5.5",
        "--dangerously-bypass-approvals-and-sandbox",
        "--enable",
        "goals",
        ...CODEX_RUDDER_CONFIG_ARGS,
        effort ? "-c" : undefined,
        effort ? `model_reasoning_effort="${effort}"` : undefined,
        `${request.contract}\n\n${prompt}`,
      ]);
      return await spawnAndStream({
        command,
        args,
        cwd: request.run.worktree.path,
        env,
        request,
        emit,
      });
    },
  };
}

function compact(values: Array<string | undefined>): string[] {
  return values.filter((value): value is string => Boolean(value));
}

function acpxBackend(): BackendAdapter {
  return {
    id: "acpx",
    async verify() {
      return commandExists("acpx")
        ? { ok: true, message: "acpx found" }
        : { ok: false, message: formatMissingToolMessage("acpx") };
    },
    async run(request, emit) {
      const prompt = stripRudderPromptWrappers(request.prompt);
      const sessionName = request.run.session?.sessionName ?? request.run.id;
      const env = process.env;
      const model = acpxCodexModel(request.run.model, request.run.effort);
      request.run.session = {
        ...(request.run.session ?? {}),
        sessionName,
      };
      await saveRunRecord(request.run);
      await runCommand("acpx", ["codex", "sessions", "ensure", "--name", sessionName], {
        cwd: request.run.worktree.path,
        env,
      });
      const args = [
        "--approve-all",
        "--format",
        "json",
        ...(model ? ["--model", model] : []),
        "--cwd",
        request.run.worktree.path,
        "codex",
        "-s",
        sessionName,
        `${request.contract}\n\n${prompt}`,
      ];
      return await spawnAndStream({
        command: "acpx",
        args,
        cwd: request.run.worktree.path,
        env,
        request,
        emit,
      });
    },
  };
}

function acpxCodexModel(model: string | undefined, effort: EffortLevel | undefined): string | undefined {
  const selectedModel = model || "gpt-5.5";
  if (selectedModel.includes("/")) {
    return selectedModel;
  }
  const selectedEffort = normalizeEffortForBackend("codex", effort) || "xhigh";
  return `${selectedModel}/${selectedEffort}`;
}

export async function backendEnv(provider: "anthropic" | "openai"): Promise<NodeJS.ProcessEnv> {
  const store = await loadAuthStore();
  const config = await loadConfig().catch(() => undefined);
  const env = { ...process.env };
  if (provider === "anthropic") {
    // Honor the configured/synced profile FIRST, then the common fallbacks. The
    // synced Claude Code profile is "anthropic:claude-code" — omitting it meant the
    // primary credential was never injected.
    const configuredId = config?.backends?.claude?.profileId;
    const profile = firstProfile(store, [
      ...(configuredId ? [configuredId] : []),
      "anthropic:claude-code",
      "anthropic:env",
      "anthropic:default",
    ]);
    if (profile?.type === "api_key" && profile.key) {
      env.ANTHROPIC_API_KEY = profile.key;
    }
    if (profile?.type === "token" && profile.token) {
      // The Claude CLI reads CLAUDE_CODE_OAUTH_TOKEN for a setup-token; the previously
      // set ANTHROPIC_OAUTH_TOKEN is not a recognized var, so the token was ignored.
      env.CLAUDE_CODE_OAUTH_TOKEN = profile.token;
    }
  }
  if (provider === "openai") {
    const configuredId = config?.backends?.codex?.profileId;
    const profile = firstProfile(store, [
      ...(configuredId ? [configuredId] : []),
      "openai-codex:default",
      "openai:env",
      "openai:default",
    ]);
    if (profile?.type === "api_key" && profile.key) {
      env.OPENAI_API_KEY = profile.key;
    }
  }
  return env;
}

function firstProfile(store: AuthProfileStore, ids: string[]) {
  for (const id of ids) {
    const profile = store.profiles[id];
    if (profile) {
      return profile;
    }
  }
  return undefined;
}

async function spawnAndStream(params: {
  command: string;
  args: string[];
  cwd: string;
  env: NodeJS.ProcessEnv;
  request: RunRequest;
  emit: (event: RudderEvent) => Promise<void>;
}): Promise<number> {
  await params.emit({
    ts: nowIso(),
    runId: params.request.run.id,
    type: "run.started",
    message: `${params.command} started`,
    data: { command: params.command, args: params.args },
  });
  const child = spawn(params.command, params.args, {
    cwd: params.cwd,
    env: params.env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  params.request.run.process = {
    ...(params.request.run.process ?? {}),
    pid: child.pid,
    startedAt: nowIso(),
  };
  params.request.run.status = "running";
  await saveRunRecord(params.request.run);

  let outRest = "";
  let errRest = "";
  const streamState = { sawStreamingText: false };
  // Best-effort token accounting: accumulate the largest usage the backend
  // reports on its own stream (claude usage / codex token_count). Backends
  // report cumulative totals, so we keep the max seen and write it onto the
  // run record so the scheduler's budget cap operates on real numbers.
  const usageState = { input: 0, output: 0 };
  let emitQueue = Promise.resolve();
  const enqueueBackendLine = (line: string, stderr: boolean) => {
    emitQueue = emitQueue
      .then(async () => {
        accumulateUsage(usageState, line);
        await emitBackendLine(params.request.run, line, params.emit, stderr, streamState);
      })
      .catch(async (error: unknown) => {
        await params.emit({
          ts: nowIso(),
          runId: params.request.run.id,
          type: "backend.error",
          message: error instanceof Error ? error.message : String(error),
        });
      });
  };
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk: string) => {
    const split = lineSplitBuffer(outRest, chunk);
    outRest = split.rest;
    for (const line of split.lines) {
      enqueueBackendLine(line, false);
    }
  });
  child.stderr.on("data", (chunk: string) => {
    const split = lineSplitBuffer(errRest, chunk);
    errRest = split.rest;
    for (const line of split.lines) {
      enqueueBackendLine(line, true);
    }
  });

  return await new Promise((resolve, reject) => {
    child.on("error", (error) => {
      if (isMissingToolSpawnError(error)) {
        reject(new MissingToolError(params.command));
        return;
      }
      void params.emit({
        ts: nowIso(),
        runId: params.request.run.id,
        type: "backend.error",
        message: error.message,
      });
      resolve(1);
    });
    child.on("close", (code, signal) => {
      if (outRest) {
        enqueueBackendLine(outRest, false);
      }
      if (errRest) {
        enqueueBackendLine(errRest, true);
      }
      void (async () => {
        await emitQueue;
        // Persist the best-effort token usage captured from the stream so the
        // scheduler can sum it against the budget cap. Only write when we saw a
        // real number; backends that emit no usage leave run.tokens untouched.
        if (usageState.input + usageState.output > 0) {
          const previous = params.request.run.tokens;
          if (!previous || usageState.input + usageState.output >= previous.input + previous.output) {
            params.request.run.tokens = { input: usageState.input, output: usageState.output };
            await saveRunRecord(params.request.run);
          }
        }
        await params.emit({
          ts: nowIso(),
          runId: params.request.run.id,
          type: "backend.exit",
          message: `${params.command} exited with ${code ?? signal ?? "unknown"}`,
          data: { code: code ?? null, signal: signal ?? null, tokens: { ...usageState } },
        });
        resolve(code ?? (signal ? 130 : 1));
      })();
    });
  });
}

async function emitBackendLine(
  run: RunRequest["run"],
  line: string,
  emit: (event: RudderEvent) => Promise<void>,
  stderr: boolean,
  streamState: { sawStreamingText: boolean },
): Promise<void> {
  const trimmed = line.trimEnd();
  if (!trimmed) {
    return;
  }
  if (/^\[acpx\].*agent needs reconnect$/.test(trimmed)) {
    return;
  }
  const parsed = parseJsonLine(trimmed);
  const message = parsed ? textFromBackendData(parsed, streamState.sawStreamingText) : trimmed;
  if (isStreamingTextEvent(parsed)) {
    streamState.sawStreamingText = true;
  }
  const sessionId = sessionIdFromBackendData(parsed);
  if (sessionId && run.session?.nativeSessionId !== sessionId) {
    run.session = {
      ...(run.session ?? {}),
      nativeSessionId: sessionId,
    };
    await saveRunRecord(run);
  }
  await emit({
    ts: nowIso(),
    runId: run.id,
    type: stderr ? "backend.error" : "backend.output",
    message: message || undefined,
    data: parsed ?? trimmed,
  });
}

function sessionIdFromBackendData(data: unknown): string | undefined {
  return isRecord(data) && typeof data.session_id === "string" ? data.session_id : undefined;
}

/**
 * Best-effort token accounting. Parses a single backend stream line and raises
 * the running usage totals to the largest cumulative numbers seen. Recognizes:
 *  - claude (--output-format stream-json): `result` events carry a cumulative
 *    `usage`; `assistant` events carry per-message `message.usage`. We take the
 *    cumulative `result` usage when present and otherwise accumulate assistant
 *    deltas, keeping whichever total is larger.
 *  - codex (exec --json): `token_count` event with
 *    `payload.info.total_token_usage` (cumulative). We take the max.
 * Anything else is a no-op. Mirrors native/src/usage.rs field names.
 */
function accumulateUsage(state: { input: number; output: number }, line: string): void {
  const parsed = parseJsonLine(line.trimEnd());
  if (!isRecord(parsed)) {
    return;
  }

  // Claude: final `result` event with a cumulative usage block.
  if (parsed.type === "result") {
    const usage = readClaudeUsage(parsed.usage);
    if (usage) {
      raiseUsage(state, usage.input, usage.output);
    }
    return;
  }
  // Claude: per-message assistant usage. Sum across messages; compare to state.
  if (parsed.type === "assistant" && isRecord(parsed.message)) {
    const usage = readClaudeUsage(parsed.message.usage);
    if (usage) {
      // assistant usage is per-turn; add it to the running totals.
      state.input += usage.input;
      state.output += usage.output;
    }
    return;
  }
  // Codex: token_count event carrying a cumulative total_token_usage.
  const payload = isRecord(parsed.payload) ? parsed.payload : undefined;
  if (payload && payload.type === "token_count" && isRecord(payload.info)) {
    const total = isRecord(payload.info.total_token_usage) ? payload.info.total_token_usage : undefined;
    if (total) {
      const inp = numberField(total.input_tokens);
      const cached = numberField(total.cached_input_tokens);
      const out = numberField(total.output_tokens);
      const reasoning = numberField(total.reasoning_output_tokens);
      // Mirror usage.rs: codex cached_input_tokens are a subset already in
      // input_tokens; reasoning tokens are billed as output.
      raiseUsage(state, Math.max(0, inp - cached), out + reasoning);
    }
  }
}

function readClaudeUsage(value: unknown): { input: number; output: number } | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  const input =
    numberField(value.input_tokens) +
    numberField(value.cache_creation_input_tokens) +
    numberField(value.cache_read_input_tokens);
  const output = numberField(value.output_tokens);
  if (input + output === 0) {
    return undefined;
  }
  return { input, output };
}

function raiseUsage(state: { input: number; output: number }, input: number, output: number): void {
  if (input + output > state.input + state.output) {
    state.input = input;
    state.output = output;
  }
}

function numberField(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function textFromBackendData(data: unknown, sawStreamingText: boolean): string {
  if (!data || typeof data !== "object") {
    return "";
  }
  const record = data as Record<string, unknown>;
  if (record.type === "stream_event" && isRecord(record.event)) {
    const event = record.event;
    if (event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string") {
      return event.delta.text;
    }
    return "";
  }
  if (record.method === "session/update" && isRecord(record.params)) {
    const update = record.params.update;
    if (isRecord(update) && update.sessionUpdate === "agent_message_chunk" && isRecord(update.content)) {
      return typeof update.content.text === "string" ? update.content.text : "";
    }
    return "";
  }
  if (isRecord(record.error) && typeof record.error.message === "string") {
    return record.error.message;
  }
  if (record.type === "assistant") {
    if (sawStreamingText) {
      return "";
    }
    return textFromAssistantMessage(record.message);
  }
  if (record.type === "result") {
    if (record.subtype === "success" && typeof record.result === "string") {
      return sawStreamingText ? "" : record.result;
    }
    if (Array.isArray(record.errors)) {
      return record.errors.filter((item) => typeof item === "string").join(", ");
    }
    return "";
  }
  if (typeof record.message === "string") {
    return record.message;
  }
  if (typeof record.text === "string") {
    return record.text;
  }
  return "";
}

function isStreamingTextEvent(data: unknown): boolean {
  if (!isRecord(data) || data.type !== "stream_event" || !isRecord(data.event)) {
    return false;
  }
  const event = data.event;
  return event.type === "content_block_delta" && isRecord(event.delta) && typeof event.delta.text === "string";
}

