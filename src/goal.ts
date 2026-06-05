// ---------------------------------------------------------------------------
// The /goal-format convention. EVERY agent Rudder spawns is prompted with a
// clear OBJECTIVE plus an explicit verifiable SUCCESS / stopping condition. Both
// Claude and Codex understand a leading `/goal` slash command, so the launch
// prompt always leads with:
//
//   /goal <objective>
//   Done when: <success condition>
//
//   <full task details / context>
//
// This module is the single canonical formatter (formatGoalPrompt) plus the
// derived-default helpers used wherever a worker prompt is built.
// ---------------------------------------------------------------------------

// Used when the planner/caller did not supply an explicit success condition.
export const DEFAULT_SUCCESS = "the task is implemented and its own verification passes";

// The backend's `/goal` slash command rejects a goal condition longer than 4000
// characters ("Goal condition is limited to 4000 characters"). The objective and
// the done-when each sit on their own `/goal` / `Done when:` line, so cap each
// safely under that — the full detail still rides along in the prompt body that
// follows. Mirrors `MAX_GOAL_LINE_CHARS` / `cap_goal_line` in native/src/tasks.rs.
export const MAX_GOAL_LINE_CHARS = 3900;

export function capGoalLine(text: string): string {
  const chars = [...text];
  if (chars.length <= MAX_GOAL_LINE_CHARS) {
    return text;
  }
  return `${chars.slice(0, MAX_GOAL_LINE_CHARS - 1).join("").replace(/\s+$/, "")}…`;
}

/**
 * Build a launch prompt in /goal format. Leads with `/goal <objective>` (the
 * backends pick this up as a slash command), then a `Done when:` success line,
 * then the full task body. Objective and success are collapsed to a single line
 * each AND capped under the backend's 4000-char `/goal` limit so the leading
 * slash command is never rejected; the full detail stays in the body.
 */
export function formatGoalPrompt(input: { goal: string; success: string; body: string }): string {
  const goal = capGoalLine(oneLine(input.goal) || oneLine(input.body) || "complete the task");
  const success = capGoalLine(oneLine(input.success) || DEFAULT_SUCCESS);
  const body = input.body.trim();
  const header = `/goal ${goal}\nDone when: ${success}`;
  return body ? `${header}\n\n${body}` : header;
}

/**
 * Derive a one-line objective from a task statement when none is given: the
 * first non-empty line, trimmed for prompt budget. If the task is already in
 * /goal format (a prior wrap), recover the real objective from the `/goal` line
 * so re-deriving never produces a nested "/goal /goal ..." string.
 */
export function deriveGoal(task: string): string {
  const lines = task.split(/\r?\n/).map((line) => line.trim());
  const goalLine = lines.find((line) => line.startsWith("/goal"));
  if (goalLine) {
    return oneLine(goalLine.slice("/goal".length)).slice(0, 200) || "complete the task";
  }
  const first = lines.find((line) => line.length > 0);
  return oneLine(first ?? task).slice(0, 200) || "complete the task";
}

/**
 * Recover the success / done-when condition from a task that is already in
 * /goal format (leads with `/goal ...` then `Done when: ...`). Returns undefined
 * when no `Done when:` line is present.
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
