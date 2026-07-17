import type { RunRecord } from "./types.js";
import { CODEX_RUDDER_PLANNER_CONFIG_ARGS, CODEX_RUDDER_WORKER_CONFIG_ARGS } from "./codex-binary.js";
import { normalizeEffortForBackend } from "./effort.js";
import { PLAN_MODE_CONTRACT } from "./plan-mode.js";
import { taskDisplayLabel } from "./task-summary.js";
import { shellQuote, stripRudderPromptWrappers } from "./util.js";

export function nativeAgentCommand(params: {
  run: RunRecord;
  prompt: string;
  contract: string;
  mode?: "execute" | "plan";
  codexCommand?: string;
}): string {
  const mode = params.mode ?? params.run.mode ?? "execute";
  const prompt = stripRudderPromptWrappers(params.prompt);
  const args = mode === "plan"
    ? planArgs(params.run, prompt, params.codexCommand)
    : params.run.backend === "codex"
      ? codexArgs(params.run, prompt, params.contract, params.codexCommand)
      : claudeArgs(params.run, prompt, params.contract);
  return args.map(shellQuote).join(" ");
}

function claudeArgs(run: RunRecord, prompt: string, contract: string): string[] {
  const model = run.model || "sonnet";
  const effort = normalizeEffortForBackend("claude", run.effort);
  const sessionId = run.session?.nativeSessionId;
  return compact([
    "env",
    "CLAUDE_CODE_NO_FLICKER=0",
    "claude",
    "--model",
    model,
    effort ? "--effort" : undefined,
    effort,
    "--permission-mode",
    "bypassPermissions",
    "--dangerously-skip-permissions",
    "--append-system-prompt",
    contract,
    "--name",
    paneTitle(run),
    sessionId ? "--session-id" : undefined,
    sessionId,
    prompt,
  ]);
}

function codexArgs(run: RunRecord, prompt: string, contract: string, codexCommand = "codex"): string[] {
  const model = run.model || "gpt-5.5";
  const effort = normalizeEffortForBackend("codex", run.effort);
  return [
    "env",
    "CODEX_RUDDER_SCROLLBACK_SAFE=1",
    codexCommand,
    "--no-alt-screen",
    "--model",
    model,
    "--dangerously-bypass-approvals-and-sandbox",
    "--enable",
    "goals",
    ...CODEX_RUDDER_WORKER_CONFIG_ARGS,
    effort ? "-c" : undefined,
    effort ? `model_reasoning_effort="${effort}"` : undefined,
    "--cd",
    run.worktree.path,
    [contract, "", prompt].join("\n"),
  ].filter((value): value is string => Boolean(value));
}

function planArgs(run: RunRecord, prompt: string, codexCommand?: string): string[] {
  return run.backend === "codex"
    ? codexPlanArgs(run, prompt, codexCommand)
    : claudePlanArgs(run, prompt);
}

function claudePlanArgs(run: RunRecord, prompt: string): string[] {
  const model = run.model || "sonnet";
  const effort = normalizeEffortForBackend("claude", run.effort);
  return compact([
    "env",
    "CLAUDE_CODE_NO_FLICKER=0",
    "claude",
    "--model",
    model,
    effort ? "--effort" : undefined,
    effort,
    "--permission-mode",
    "default",
    "--tools",
    CLAUDE_PLAN_TOOLS.join(","),
    "--allowedTools",
    CLAUDE_PLAN_TOOLS.join(","),
    "--disallowedTools",
    CLAUDE_PLAN_DISALLOWED_TOOLS.join(","),
    "--append-system-prompt",
    PLAN_MODE_CONTRACT,
    "--name",
    `plan:${paneTitle(run)}`,
    prompt,
  ]);
}

function codexPlanArgs(run: RunRecord, prompt: string, codexCommand = "codex"): string[] {
  const model = run.model || "gpt-5.5";
  const effort = normalizeEffortForBackend("codex", run.effort);
  return compact([
    "env",
    "CODEX_RUDDER_SCROLLBACK_SAFE=1",
    codexCommand,
    "--no-alt-screen",
    "--model",
    model,
    "--sandbox",
    "read-only",
    "--ask-for-approval",
    "never",
    "--enable",
    "goals",
    "--search",
    ...CODEX_RUDDER_PLANNER_CONFIG_ARGS,
    effort ? "-c" : undefined,
    effort ? `model_reasoning_effort="${effort}"` : undefined,
    "--cd",
    run.worktree.path,
    `${PLAN_MODE_CONTRACT}\n\n${prompt}`,
  ]);
}

function paneTitle(run: RunRecord): string {
  const words = taskDisplayLabel(run, 34);
  return `${run.backend}:${words || run.id.slice(0, 12)}`;
}

function compact(values: Array<string | undefined>): string[] {
  return values.filter((value): value is string => Boolean(value));
}

const CLAUDE_PLAN_TOOLS = [
  "Read",
  "Grep",
  "Glob",
  "LS",
  "WebSearch",
  "WebFetch",
];

const CLAUDE_PLAN_DISALLOWED_TOOLS = [
  "Edit",
  "Write",
  "MultiEdit",
  "NotebookEdit",
  "Bash",
  "Bash(rm *)",
  "Bash(mv *)",
  "Bash(cp *)",
  "Bash(mkdir *)",
  "Bash(touch *)",
  "Bash(chmod *)",
  "Bash(chown *)",
  "Bash(git add*)",
  "Bash(git commit*)",
  "Bash(git checkout*)",
  "Bash(git switch*)",
  "Bash(git reset*)",
  "Bash(git clean*)",
  "Bash(git merge*)",
  "Bash(git rebase*)",
  "Bash(git push*)",
  "Bash(fly deploy*)",
  "Bash(fly secrets set*)",
  "Bash(fly secrets unset*)",
  "Bash(fly scale*)",
  "Bash(fly apps destroy*)",
];
