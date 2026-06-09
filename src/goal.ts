// ---------------------------------------------------------------------------
// The launch-goal convention. Every agent Rudder spawns is prompted with a clear
// OBJECTIVE plus an explicit verifiable SUCCESS / stopping condition. Do not
// launch a process with a leading `/goal` slash command or a literal `Goal:`
// header: Claude Code can parse goal-looking launch text as a goal condition and
// reject a long worker brief. Runtime user-typed `/goal` forwarding is handled elsewhere.
// Launch prompts lead with:
//
//   Objective: <objective>
//   Done when: <success condition>
//
//   <full task details / context>
//
// This module is the single canonical formatter (formatGoalPrompt) plus the
// derived-default helpers used wherever a worker prompt is built.
// ---------------------------------------------------------------------------

// Used when the planner/caller did not supply an explicit success condition.
export const DEFAULT_SUCCESS = "the task is implemented and its own verification passes";

// Keep the launch goal lines under the backend's slash-command limit too, because
// legacy prompts can still be recovered from `/goal` and because users can forward
// `/goal` manually. Mirrors `MAX_GOAL_LINE_CHARS` / `cap_goal_line` in native/src/tasks.rs.
export const MAX_GOAL_LINE_CHARS = 3000;

export function capGoalLine(text: string): string {
  const chars = [...text];
  if (chars.length <= MAX_GOAL_LINE_CHARS) {
    return text;
  }
  return `${chars.slice(0, MAX_GOAL_LINE_CHARS - 1).join("").replace(/\s+$/, "")}…`;
}

export function normalizeGoalLine(value: string, fallback: string): string {
  return capGoalLine(oneLine(value) || oneLine(fallback) || "complete the task");
}

/**
 * Build a launch prompt in objective format. Leads with `Objective: <objective>`, then a
 * `Done when:` success line, then the full task body. Objective and success are
 * collapsed to a single line each and capped under the backend's 4000-char goal
 * limit. The full detail stays in the body.
 */
export function formatGoalPrompt(input: { goal: string; success: string; body: string }): string {
  const goal = normalizeGoalLine(input.goal, input.body || "complete the task");
  const success = normalizeGoalLine(input.success, DEFAULT_SUCCESS);
  const body = input.body.trim();
  const header = `Objective: ${goal}\nDone when: ${success}`;
  return body ? `${header}\n\n${body}` : header;
}

/**
 * Derive a one-line objective from a task statement when none is given: the
 * first non-empty line, trimmed for prompt budget. If the task is already in
 * goal format (a prior wrap), recover the real objective from the goal line.
 */
export function deriveGoal(task: string): string {
  const lines = task.split(/\r?\n/).map((line) => line.trim());
  const goalLine = lines.find((line) => /^\/goal(?:\s+|$)/.test(line));
  if (goalLine) {
    return oneLine(goalLine.replace(/^\/goal(?:\s+|$)/, "")).slice(0, 200) || "complete the task";
  }
  const plainGoalLine = lines.find((line) => /^(?:goal|objective):/i.test(line));
  if (plainGoalLine) {
    return oneLine(plainGoalLine.replace(/^(?:goal|objective):/i, "")).slice(0, 200) || "complete the task";
  }
  const first = lines.find((line) => line.length > 0);
  return oneLine(first ?? task).slice(0, 200) || "complete the task";
}

/**
 * Recover the success / done-when condition from a task that is already in
 * goal format (leads with `Objective: ...`, legacy `Goal: ...`, or legacy
 * `/goal ...`, then `Done when: ...`).
 * Returns undefined when no `Done when:` line is present.
 */
export function extractSuccess(task: string): string | undefined {
  const line = task
    .split(/\r?\n/)
    .map((entry) => entry.trim())
    .find((entry) => /^done when:/i.test(entry));
  if (!line) {
    return undefined;
  }
  const value = oneLine(line.replace(/^done when:/i, ""));
  return value || undefined;
}

/**
 * Derive a success condition. Prefer caller-supplied criteria (e.g. the
 * acceptance criteria); otherwise fall back to the default stopping condition.
 */
export function deriveSuccess(criteria?: string[]): string {
  const joined = (criteria ?? [])
    .map((item) => item.trim())
    .filter(Boolean)
    .join("; ");
  return joined ? oneLine(joined) : DEFAULT_SUCCESS;
}

function oneLine(value: string): string {
  return value.split(/\s+/).filter(Boolean).join(" ").trim();
}
