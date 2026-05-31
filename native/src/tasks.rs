#![allow(unused_imports)]
//! Prompt construction, task-summary generation, and rudder-plan parsing.
use super::*;


pub(crate) fn execution_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    const CONTEXT: &str = "Rudder-specific context injected by Rudder:\n- Read RUDDER.md first if it exists. Rudder generated that file to show active Rudder agents and worktrees in this repo.\n- If a Hunk review is open for this worktree, run `hunk skill path`, load that skill, and use `hunk session review --repo . --json` plus `hunk session comment ...` commands to inspect and annotate the live review.";
    // Keep the /goal block as the very first line so the backend (Claude or
    // Codex) picks it up as a slash command: hoist a leading `/goal ...` (and its
    // `Done when:` line) above the injected Rudder context.
    if task.trim_start().starts_with("/goal") {
        let (goal_block, rest) = split_leading_goal_block(&task);
        return format!("{goal_block}\n\n{CONTEXT}\n\n{rest}");
    }
    format!("{CONTEXT}\n\n{task}")
}

/// Split a /goal-formatted prompt into its leading `/goal ...` + `Done when: ...`
/// block and the remaining body. Assumes the input leads with `/goal`.
fn split_leading_goal_block(task: &str) -> (String, String) {
    let task = task.trim_start();
    let mut lines = task.lines();
    let mut block = Vec::new();
    if let Some(goal_line) = lines.next() {
        block.push(goal_line);
    }
    // The optional Done-when line immediately follows the /goal line.
    let mut rest_lines: Vec<&str> = Vec::new();
    let mut consumed_done = false;
    for line in lines {
        if !consumed_done {
            consumed_done = true;
            if line.trim_start().to_ascii_lowercase().starts_with("done when:") {
                block.push(line);
                continue;
            }
        }
        rest_lines.push(line);
    }
    let body = rest_lines
        .iter()
        .skip_while(|line| line.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    (block.join("\n"), body)
}

pub(crate) fn plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "Plan this task before implementation. Inspect the repository and relevant read-only context first. Ask follow-up questions if the plan cannot be made decision-complete from inspection alone.\n\n{task}"
    )
}

pub(crate) fn rudder_plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "You are Rudder's planning coordinator. You decompose one user request into a DAG of implementation tasks that a separate set of worker agents will implement in isolated git worktrees. You inspect the repository in READ-ONLY mode. You do NOT implement anything yourself.\n\nUser request:\n{task}\n\nProcess:\n1. Inspect the relevant files read-only to understand the work.\n2. You run NON-INTERACTIVELY: nobody can answer questions, so you must NEVER ask clarifying questions and you must NEVER stop without emitting a plan. If the request is ambiguous, pick the single most reasonable interpretation, make explicit assumptions, and ALWAYS produce a complete task DAG anyway. State your assumptions in the human summary after the block (and, where they shape the work, inside the affected task prompts). The user reviews the DAG at an approval gate and can fix or discard it before any worker runs, so a best-effort plan with clear, stated assumptions is far more useful than no plan.\n3. Once the request is clear, decompose it into a DAG of tasks. Each task gets a short stable `id` (for example n0, n1, n2) and a `deps` array of typed edges to the task ids it depends on.\n4. Edge types: `hard` means the child's success condition CANNOT be met until the parent has merged. A task that CONSUMES another task's produced code is HARD on it: tests that import or exercise code another task writes, code that imports a module/function/type another task creates, or wiring that calls an API another task defines. The child can technically start, but it cannot SUCCEED (imports resolve, tests pass) until that code exists, so it must wait for the merge. `soft` means context-only: the parent's diff is delivered as background once it lands, but the child can succeed on its own in parallel (parallel features, doc updates, sibling modules that do not import each other). Use the MINIMAL set of hard edges: default to soft for independent work, but do NOT under-classify, if the child executes or imports the parent's code it is hard. Every hard edge needs a one-line `why`.\n5. Split into independent or softly-coupled tasks wherever the work can proceed in parallel across separate worktrees. Do NOT collapse everything into one task, and do NOT make every task independent when real ordering exists: model the ordering with hard/soft edges instead.\n6. Each task must be self-contained, name concrete files or modules to inspect when known, and carry its own verification. Each worker runs in its OWN isolated workspace at a different filesystem path than this repository. In every `prompt`, `goal`, and `success`, refer to files by REPOSITORY-RELATIVE paths only (for example `mathutils.py`, `src/db/schema.ts`). NEVER embed an absolute filesystem path, this repository's location, or phrases like \"in the repository at <path>\" or \"cd into <path>\" — the worker is already in its workspace, and an absolute path sends it to the wrong directory and the task fails.\n7. Every task MUST carry both a `goal` and a `success`. `goal` is one line naming the single objective the worker should accomplish (suitable for the `/goal` slash command, without the leading slash). `success` is the verifiable DONE-WHEN condition: the commands, artifacts, or criteria that mean the task is complete. Never omit either or leave them empty.\n\nYOUR ROLE: You are a read-only DECOMPOSER, not an implementer. Your tools are inspection-only, so you cannot and must not edit, write, or run code. Your only deliverable is the task DAG below. Do NOT implement the work and do NOT ask to proceed: a separate set of worker agents implements each task in its own isolated workspace, and Rudder shows the user this plan for approval before launching them. When the DAG is ready, print exactly the block below as a normal assistant message and then stop.\n\nPrint exactly this block and no other JSON block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"id\":\"n0\",\"title\":\"short task title\",\"prompt\":\"full implementation prompt for one worker agent\",\"goal\":\"one-line objective for /goal, without the leading slash command\",\"success\":\"verifiable done-when condition\",\"deps\":[]}},{{\"id\":\"n1\",\"title\":\"...\",\"prompt\":\"...\",\"goal\":\"...\",\"success\":\"...\",\"deps\":[{{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"edits the module n0 creates\"}}]}}]}}\nRUDDER_PLAN_TASKS_END\n\nAfter the block, add a short human summary of why this DAG is safe."
    )
}

/// Build the RECONCILE planner prompt: a multi-agent plan is already in flight and
/// the user is ADDING one task. The planner must emit exactly ONE node whose
/// `deps` reference the existing frontier ids (never replace the plan). Mirrors the
/// TS `reconcileSystemPrompt` shape in src/planner.ts. `frontier` is the list of
/// `(id, title)` pairs for the current plan nodes the new task can depend on.
pub(crate) fn rudder_reconcile_prompt(task: &str, frontier: &[(String, String)]) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    let existing = if frontier.is_empty() {
        "(none: the plan currently has no in-flight nodes)".to_string()
    } else {
        frontier
            .iter()
            .map(|(id, title)| format!("- {id}: {title}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let existing_ids = frontier
        .iter()
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are Rudder's injection coordinator. A multi-agent plan is ALREADY in flight; a separate set of worker agents is implementing its nodes in isolated git worktrees. The user is now ADDING one more task to that running plan. You inspect the repository in READ-ONLY mode. You do NOT implement anything yourself, and you do NOT re-plan or replace the existing work.\n\nExisting plan nodes (the frontier, in-flight, not yet merged):\n{existing}\n\nExisting node ids: [{existing_ids}]\n\nNew task the user is ADDING:\n{task}\n\nProcess:\n1. Inspect the relevant files read-only to understand how the new task relates to the in-flight nodes.\n2. Emit EXACTLY ONE node in the RUDDER_PLAN_TASKS block. Its `id` MUST be unique from every existing node id above. Its `deps` reference the existing frontier ids.\n3. Edge rules: add a `hard` dep only if the new task genuinely CANNOT start until that node merges (it edits that node's not-yet-merged code, or needs an interface that node defines) and justify it with a one-line `why`. Otherwise use `soft` deps for nodes the new task should be aware of, or no deps at all if the new task is fully independent. Prefer soft; a hard edge without a `why` is treated as soft.\n4. The node must be self-contained, name concrete files or modules to inspect when known, and carry both a `goal` (one-line objective for the `/goal` slash command, without the leading slash) and a verifiable `success` (the DONE-WHEN condition). Never omit either. The worker runs in its OWN isolated workspace at a different filesystem path than this repository: refer to files by REPOSITORY-RELATIVE paths only, and NEVER embed an absolute path, the repository's location, or phrases like \"in the repository at <path>\" — an absolute path sends the worker to the wrong directory and the task fails.\n\nPLAN MODE: You are running in plan mode. Do NOT call ExitPlanMode. Do NOT implement, edit, or write any files. When the node is ready, print exactly the block below as a NORMAL assistant message and then stop. Rudder reads the block directly from your output; it does not need you to exit plan mode.\n\nPrint exactly this block and no other JSON block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"id\":\"new\",\"title\":\"short task title\",\"prompt\":\"full implementation prompt for one worker agent\",\"goal\":\"one-line objective for /goal, without the leading slash command\",\"success\":\"verifiable done-when condition\",\"deps\":[{{\"on\":\"<existing frontier id>\",\"type\":\"soft\",\"why\":\"context the new task should be aware of\"}}]}}]}}\nRUDDER_PLAN_TASKS_END\n\nAfter the block, add a short human summary of how the added node relates to the in-flight plan."
    )
}

/// Dependency edge type. `hard` blocks the child from starting until the parent
/// has merged; `soft` never blocks (the parent's diff is delivered as context once
/// it lands). `soft` is the safe default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeType {
    Hard,
    Soft,
}

impl EdgeType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

/// A typed dependency declared by a plan task: this task depends `on` another
/// task id, with the given `edge` type and an optional justification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanEdge {
    pub(crate) on: String,
    pub(crate) edge: EdgeType,
    pub(crate) why: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RudderPlanTask {
    /// Stable node id. Synthesized as `n{i}` when the plan omits it so older flat
    /// plans (no id, no deps) keep working as 0-dep roots.
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) prompt: String,
    /// One-line OBJECTIVE for the `/goal` launch line. Optional in the parse for
    /// backward compatibility; the worker prompt derives a default when absent.
    pub(crate) goal: Option<String>,
    /// Verifiable SUCCESS / DONE-WHEN condition for the launch prompt. Optional
    /// in the parse for backward compatibility; defaulted in the worker prompt.
    pub(crate) success: Option<String>,
    /// Typed dependencies. Empty for a 0-dep root.
    pub(crate) deps: Vec<PlanEdge>,
    pub(crate) backend: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

impl RudderPlanTask {
    /// Ids this task hard-depends on (cannot start until these are merged).
    pub(crate) fn hard_deps(&self) -> impl Iterator<Item = &str> {
        self.deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Hard)
            .map(|edge| edge.on.as_str())
    }
}

/// Ready-work detection (beads-style): a task is ready when every one of its
/// hard-dep ids appears in `merged_ids`. Soft deps never block readiness.
pub(crate) fn ready_nodes<'a>(
    tasks: &'a [RudderPlanTask],
    merged_ids: &[String],
) -> Vec<&'a RudderPlanTask> {
    tasks
        .iter()
        .filter(|task| {
            task.hard_deps()
                .all(|dep| merged_ids.iter().any(|id| id == dep))
        })
        .collect()
}

/// A queued unit of plannable work that has not launched yet. The scheduler
/// drains these into live `AgentRun`s as their hard deps merge and parallelism
/// slots free. Created one-per-task when a planner agent completes; rendered in
/// the Todo section until launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedNode {
    /// Stable node id (the plan task id). Becomes the launched `AgentRun.node_id`.
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) goal: Option<String>,
    /// Verifiable DONE-WHEN condition carried through to the worker launch prompt.
    pub(crate) success: Option<String>,
    /// Hard-dependency parent node ids: launch is gated until ALL are merged.
    pub(crate) deps: Vec<String>,
    /// Soft-dependency parent node ids: never gate launch (kept for nesting).
    pub(crate) soft_deps: Vec<String>,
    pub(crate) backend: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

impl PlannedNode {
    /// Build a queued node from a parsed plan task: hard-dep ids gate the launch;
    /// soft-dep ids are retained only for nesting.
    pub(crate) fn from_task(task: &RudderPlanTask) -> Self {
        let deps: Vec<String> = task.hard_deps().map(ToString::to_string).collect();
        let soft_deps: Vec<String> = task
            .deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Soft)
            .map(|edge| edge.on.clone())
            .collect();
        Self {
            id: task.id.clone(),
            title: task.title.clone(),
            prompt: task.prompt.clone(),
            goal: task.goal.clone(),
            success: task.success.clone(),
            deps,
            soft_deps,
            backend: task.backend.clone(),
            model: task.model.clone(),
            effort: task.effort.clone(),
        }
    }

    /// A planned node is launchable when every hard dep is satisfied: either the
    /// dep id is in `merged_ids`, or it is not present in `plan_ids` (the set of
    /// node ids known to this plan: still-queued nodes plus already-launched ones).
    /// A dep outside `plan_ids` was never part of the plan and is treated as
    /// satisfied so the DAG never deadlocks on a dangling reference.
    pub(crate) fn is_ready(&self, merged_ids: &[String], plan_ids: &[String]) -> bool {
        self.deps.iter().all(|dep| {
            merged_ids.iter().any(|id| id == dep) || !plan_ids.iter().any(|id| id == dep)
        })
    }
}

/// Build the worker launch prompt for a planned node. Mirrors
/// `rudder_plan_worker_prompt` but sources fields off the queued node rather than
/// a freshly parsed `RudderPlanTask`.
pub(crate) fn planned_node_worker_prompt(planner_task: &str, node: &PlannedNode) -> String {
    let task = RudderPlanTask {
        id: node.id.clone(),
        title: node.title.clone(),
        prompt: node.prompt.clone(),
        goal: node.goal.clone(),
        success: node.success.clone(),
        deps: Vec::new(),
        backend: node.backend.clone(),
        model: node.model.clone(),
        effort: node.effort.clone(),
    };
    // Backend is unused by the prompt builder; pass a placeholder.
    rudder_plan_worker_prompt(planner_task, &task, Backend::Claude)
}

/// Kahn topological sort over the hard edges only. Returns `Err` when a cycle is
/// present so the TUI can surface it instead of deadlocking. Unknown ids are
/// assumed already dropped by the caller.
fn assert_no_hard_cycle(tasks: &[RudderPlanTask]) -> Result<()> {
    use std::collections::{HashMap, VecDeque};

    let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut indegree: HashMap<&str, usize> = ids.iter().map(|id| (*id, 0usize)).collect();
    // child -> list of parents it hard-depends on; edge parent -> child.
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for parent in task.hard_deps() {
            children.entry(parent).or_default().push(task.id.as_str());
            *indegree.entry(task.id.as_str()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<&str> = indegree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0usize;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(kids) = children.get(node) {
            for kid in kids {
                let entry = indegree.entry(kid).or_insert(0);
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    queue.push_back(kid);
                }
            }
        }
    }

    if visited != tasks.len() {
        bail!("rudder-plan tasks form a hard-dependency cycle");
    }
    Ok(())
}

pub(crate) fn rudder_plan_output_for_run(run: &AgentRun) -> String {
    // The orchestrator streams JSON events now, so its raw PTY output_log is NDJSON,
    // not plan text. `plan_stream` reconstructs the assistant text (and, for a refine,
    // exposes only the CURRENT turn so a stale prior block is not re-captured). Prefer
    // it whenever present; the raw/codex-session path is a pre-ingest fallback only.
    if let Some(stream) = run.plan_stream.as_ref() {
        if stream.has_text() {
            return stream.parse_text().to_string();
        }
    }
    let mut output = run
        .terminal
        .as_ref()
        .map(|terminal| terminal.output_log_snapshot().to_string())
        .unwrap_or_default();
    if let Some(session_output) = latest_codex_rudder_plan_output(run) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&session_output);
    }
    output
}

pub(crate) fn extract_rudder_plan_tasks(output: &str) -> Result<Vec<RudderPlanTask>> {
    extract_rudder_plan_tasks_with_frontier(output, &[])
}

/// Like `extract_rudder_plan_tasks`, but `frontier_ids` are ALSO treated as known
/// ids so a RECONCILE node's `deps` referencing existing (out-of-block) frontier
/// nodes are kept instead of dropped. The initial-plan path passes an empty slice,
/// preserving its block-local resolution exactly.
pub(crate) fn extract_rudder_plan_tasks_with_frontier(
    output: &str,
    frontier_ids: &[String],
) -> Result<Vec<RudderPlanTask>> {
    const START: &str = "RUDDER_PLAN_TASKS_START";
    const END: &str = "RUDDER_PLAN_TASKS_END";

    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let Some(start) = clean.rfind(START) else {
        bail!("missing RUDDER_PLAN_TASKS_START");
    };
    let after_start = &clean[start + START.len()..];
    let Some(end) = after_start.find(END) else {
        bail!("missing RUDDER_PLAN_TASKS_END");
    };
    let mut json = after_start[..end].trim();
    if let Some(stripped) = json.strip_prefix("```json") {
        json = stripped.trim();
    } else if let Some(stripped) = json.strip_prefix("```") {
        json = stripped.trim();
    }
    if let Some(stripped) = json.strip_suffix("```") {
        json = stripped.trim();
    }

    let value: serde_json::Value =
        serde_json::from_str(json).context("task block was not valid JSON")?;
    let tasks = value
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .context("task block must contain a tasks array")?;

    // Cap first, then synthesize ids by position so `deps` referencing dropped
    // (beyond-cap) tasks are treated as unknown and dropped below.
    let capped: Vec<&serde_json::Value> = tasks.iter().take(6).collect();

    // Known ids across the capped block. An explicit id wins; otherwise the
    // positional fallback `n{i}` is used (matching the synthesized id below). The
    // reconcile frontier ids are merged in so cross-block deps on existing nodes
    // survive `parse_plan_deps`.
    let mut known_ids: std::collections::HashSet<String> = capped
        .iter()
        .enumerate()
        .map(|(index, task)| plan_task_id(task, index))
        .collect();
    known_ids.extend(frontier_ids.iter().cloned());

    let mut out = Vec::new();
    for (index, task) in capped.iter().enumerate() {
        let title = task
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("worker task")
            .trim();
        let prompt = task
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .context("each task needs a prompt")?
            .trim();
        if prompt.is_empty() {
            continue;
        }

        let id = plan_task_id(task, index);
        let deps = parse_plan_deps(task, &id, &known_ids);

        out.push(RudderPlanTask {
            id,
            title: if title.is_empty() {
                "worker task".to_string()
            } else {
                title.to_string()
            },
            prompt: prompt.to_string(),
            goal: plan_optional_str(task, "goal"),
            success: plan_optional_str(task, "success"),
            deps,
            backend: plan_optional_str(task, "backend"),
            model: plan_optional_str(task, "model"),
            effort: plan_optional_str(task, "effort"),
        });
    }

    assert_no_hard_cycle(&out)?;

    Ok(out)
}

/// The orchestrator's human-readable prose printed AFTER the RUDDER_PLAN_TASKS_END
/// marker: its assumptions, why-this-is-safe, and any open questions. Shown under
/// the DAG so the user knows what the planner assumed and what to discuss. Returns
/// None when there is no trailing prose. ANSI-stripped, trimmed, and length-capped.
pub(crate) fn extract_rudder_plan_summary(output: &str) -> Option<String> {
    const END: &str = "RUDDER_PLAN_TASKS_END";
    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let idx = clean.rfind(END)?;
    let after = clean[idx + END.len()..].trim();
    // Drop a stray closing fence if the whole reply was fenced.
    let after = after.trim_start_matches("```").trim();
    if after.is_empty() {
        return None;
    }
    Some(after.chars().take(1500).collect())
}

/// Build the composite request handed to the orchestrator when the user REFINES a
/// pending plan: the original ask, the current DAG outline, and the user's
/// feedback, framed so the planner revises the whole DAG (reusing stable ids)
/// rather than starting over. This string is the orchestrator's task; the
/// decomposer system prompt (`rudder_plan_prompt`) still wraps it.
/// The SLIM follow-up prompt sent on a session-RESUMED refine. The resumed session
/// already holds the prior plan + reasoning + the files it inspected, so we send
/// only the feedback plus the one rule that is not re-applied on resume: re-emit the
/// full RUDDER_PLAN_TASKS block. Used when there is a session to `--resume`.
pub(crate) fn build_refine_followup(feedback: &str) -> String {
    format!(
        "The user reviewed the plan you just produced and wants changes. Apply this feedback directly — do not ask questions and do not re-explain the whole plan. Keep the parts that still make sense (reuse their ids), add, remove, or edit tasks and dependencies as needed, then RE-EMIT THE FULL UPDATED PLAN as one block exactly as before:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[...]}}\nRUDDER_PLAN_TASKS_END\nAfter the block, add a short note on what changed.\n\nUser feedback:\n{feedback}"
    )
}

pub(crate) fn build_refine_request(original: &str, current_plan: &str, feedback: &str) -> String {
    format!(
        "You previously produced the task DAG below for the original request. The user has reviewed it and wants changes. Produce the REVISED, COMPLETE task DAG with the changes applied: keep the parts that still make sense (reuse their ids), add, remove, or edit tasks and dependencies as needed, and re-emit the full plan. Apply the user's feedback directly and do not ask questions.\n\nOriginal request:\n{original}\n\nCurrent plan:\n{current_plan}\n\nUser feedback / requested changes:\n{feedback}"
    )
}

/// Read a non-empty trimmed string field, or `None`.
fn plan_optional_str(task: &serde_json::Value, key: &str) -> Option<String> {
    task.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// The node id for a task: explicit `id` if non-empty, else synthesized `n{index}`.
fn plan_task_id(task: &serde_json::Value, index: usize) -> String {
    plan_optional_str(task, "id").unwrap_or_else(|| format!("n{index}"))
}

/// Parse `deps` into typed edges. Drops edges whose `on` is not a known id (and
/// self-edges). Downgrades a `hard` edge with empty/missing `why` to `soft` so
/// every hard dependency is justified.
fn parse_plan_deps(
    task: &serde_json::Value,
    self_id: &str,
    known_ids: &std::collections::HashSet<String>,
) -> Vec<PlanEdge> {
    let Some(deps) = task.get("deps").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dep in deps {
        // Models phrase the parent id as either "on" or "from"; accept both.
        let on = dep
            .get("on")
            .or_else(|| dep.get("from"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(on) = on else { continue };
        // Drop edges to unknown ids and self-edges.
        if on == self_id || !known_ids.contains(on) {
            continue;
        }
        let why = dep
            .get("why")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string);
        let declared_hard = dep
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.trim().eq_ignore_ascii_case("hard"))
            .unwrap_or(false);
        // hard requires a justification; downgrade to soft otherwise. soft is default.
        let edge = if declared_hard && why.is_some() {
            EdgeType::Hard
        } else {
            EdgeType::Soft
        };
        out.push(PlanEdge {
            on: on.to_string(),
            edge,
            why,
        });
    }
    out
}

/// The canonical default DONE-WHEN condition when the planner did not supply a
/// `success`. Mirrors the TS `DEFAULT_SUCCESS` constant in src/goal.ts.
pub(crate) const DEFAULT_GOAL_SUCCESS: &str =
    "the task is implemented and its own verification passes";

/// Collapse a value to a single line so the leading `/goal` slash command stays
/// intact, trimmed for prompt budget.
fn one_line(value: &str) -> String {
    let joined = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    truncate_chars(trimmed, 200)
}

/// The OBJECTIVE for a task's `/goal` line: the planner-supplied `goal`, else a
/// default derived from the title (or the first line of the prompt).
fn goal_objective(task: &RudderPlanTask) -> String {
    if let Some(goal) = task.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        return one_line(goal);
    }
    let title = task.title.trim();
    if !title.is_empty() {
        return one_line(title);
    }
    let first_line = task
        .prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("complete the task");
    one_line(first_line)
}

/// The verifiable DONE-WHEN condition for a task: the planner-supplied
/// `success`, else the canonical default stopping condition.
fn goal_success(task: &RudderPlanTask) -> String {
    task.success
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(one_line)
        .unwrap_or_else(|| DEFAULT_GOAL_SUCCESS.to_string())
}

/// Build a worker launch prompt in the canonical /goal format. EVERY spawned
/// agent (Claude or Codex) leads with `/goal <objective>` then a `Done when:`
/// line so the backend picks up the slash command and the worker always knows
/// what done means. The full task body follows. Both backends support /goal, so
/// the block is emitted unconditionally (not gated on the goals feature flag).
pub(crate) fn rudder_plan_worker_prompt(
    planner_task: &str,
    task: &RudderPlanTask,
    _backend: Backend,
) -> String {
    let body = format!(
        "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nOriginal request:\n{planner_task}\n\nWorker task: {}\n\n{}",
        task.title, task.prompt
    );
    format!(
        "/goal {}\nDone when: {}\n\n{}",
        goal_objective(task),
        goal_success(task),
        body
    )
}

/// Wrap a manual single-task spawn (no planner) in the canonical /goal format.
/// The objective is the task statement's first line; the success condition is
/// the canonical default stopping condition. Idempotent: a prompt that already
/// leads with `/goal` (e.g. a /rudder-plan worker prompt) is returned unchanged.
pub(crate) fn manual_goal_prompt(task: &str) -> String {
    let trimmed = task.trim_start();
    if trimmed.starts_with("/goal") {
        return task.to_string();
    }
    let objective = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(one_line)
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "complete the task".to_string());
    format!("/goal {objective}\nDone when: {DEFAULT_GOAL_SUCCESS}\n\n{task}")
}

pub(crate) fn rudder_plan_worker_title_from_prompt(task: &str) -> Option<String> {
    let mut lines = task.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Worker task:") else {
            continue;
        };
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
        for title in lines.by_ref() {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

pub(crate) fn strip_ansi_for_plan(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ ... <final byte @-~> (colors, cursor moves).
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... terminated by BEL (\x07) or ST (ESC \). Claude emits
            // OSC 777 desktop notifications ("Claude needs your permission"); without
            // this their payload leaks into the output and can pollute the parse.
            Some(']') => {
                chars.next();
                while let Some(&next) = chars.peek() {
                    if next == '\x07' {
                        chars.next();
                        break;
                    }
                    if next == '\x1b' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
            }
            // Other ESC sequences (e.g. charset selection ESC ( B): drop ESC + the
            // intermediate/final byte so a stray escape never breaks block parsing.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

pub(crate) fn strip_rudder_prompt_wrappers(task: &str) -> String {
    const START: &str = "[RUDDER PROMPT INJECTION]";
    const END: &str = "[END RUDDER PROMPT INJECTION]";

    let mut value = task.trim().to_string();
    loop {
        let trimmed = value.trim_start();
        if let Some(rest) = trimmed.strip_prefix("USER TASK:") {
            value = rest.trim_start().to_string();
            continue;
        }
        if let Some(after_start) = trimmed.strip_prefix(START) {
            if let Some(index) = after_start.find(END) {
                let body = after_start[..index].trim();
                let rest = after_start[index + END.len()..].trim_start();
                value = if rest.is_empty() { body } else { rest }.to_string();
                continue;
            }
        }
        return trimmed.to_string();
    }
}

pub(crate) fn short_task(task: &str) -> String {
    const MAX: usize = 26;
    truncate_chars(task, MAX)
}

pub(crate) fn summarize_task(task: &str) -> String {
    summarize_task_to(task, 56)
}

pub(crate) fn spawn_task_summary_worker(tx: mpsc::Sender<TaskSummaryResult>, run_id: String, task: String) {
    thread::spawn(move || {
        let title = generate_task_summary_title(&task);
        let _ = tx.send(TaskSummaryResult { run_id, title });
    });
}

pub(crate) fn generate_task_summary_title(task: &str) -> Option<String> {
    let task = normalize_task_text(task);
    if task.is_empty() {
        return None;
    }
    let prompt = format!(
        "Summarize this coding agent task for a compact sidebar label.\n\
Return exactly one JSON object and no markdown: {{\"title\":\"5-8 word imperative title\"}}\n\
Rules: no quotes inside the title, no trailing punctuation, do not mention Rudder unless it is the product being changed.\n\n\
Task:\n{task}"
    );
    let output = Command::new("claude")
        .args(["-p", &prompt, "--model", TASK_SUMMARY_MODEL])
        .env("CLAUDE_CODE_NO_FLICKER", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    task_title_from_summary_output(&stdout)
}

pub(crate) fn task_title_from_summary_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end >= start {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end])
                {
                    if let Some(title) = value.get("title").and_then(|value| value.as_str()) {
                        return clean_task_summary_title(title);
                    }
                }
            }
        }
    }
    clean_task_summary_title(trimmed)
}

pub(crate) fn clean_task_summary_title(raw: &str) -> Option<String> {
    let mut title = raw
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .to_string();
    title = strip_terminal_punctuation(&title);
    title = normalize_task_text(&title);
    if title.is_empty() {
        return None;
    }
    Some(truncate_chars(&title, 56))
}

pub(crate) fn summarize_task_to(task: &str, max_chars: usize) -> String {
    let original = normalize_task_text(task);
    if original.is_empty() {
        return "agent".to_string();
    }

    let mut summary = strip_leading_scaffolding(&original);
    summary = normalize_task_text(&summary)
        .replace("lsited", "listed")
        .replace("rihgt", "right")
        .replace("the task that the user puts", "the user task")
        .replace("the task that user puts", "the user task")
        .replace("task that the user puts", "user task")
        .replace("task that user puts", "user task")
        .replace("and then that's what gets listed on", "for")
        .replace("and then that is what gets listed on", "for");
    summary = strip_trailing_context(&summary);
    summary = first_sentence(&summary);
    summary = strip_terminal_punctuation(&summary);
    if summary.is_empty() {
        summary = original;
    }

    if summary.chars().count() <= max_chars {
        return summary;
    }

    compact_title(&summary, max_chars).unwrap_or_else(|| truncate_chars(&summary, max_chars))
}

pub(crate) fn normalize_task_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn strip_leading_scaffolding(value: &str) -> String {
    let mut current = value.trim().to_string();
    loop {
        let lower = current.to_ascii_lowercase();
        let mut next = None;
        for prefix in [
            "ok, ",
            "okay, ",
            "hey, ",
            "ok ",
            "okay ",
            "hey ",
            "also ",
            "please ",
            "can you ",
            "could you ",
            "would you ",
            "can u ",
            "could u ",
            "would u ",
            "can we ",
            "could we ",
            "would we ",
            "i need you to ",
            "i need to ",
            "i want you to ",
            "i want to ",
            "need you to ",
            "need to ",
            "want you to ",
            "want to ",
            "we need to ",
            "we should ",
            "we have to ",
            "another thing for you to work on is ",
            "another thing is ",
            "the task is ",
        ] {
            if lower.starts_with(prefix) {
                next = Some(current[prefix.len()..].trim().to_string());
                break;
            }
        }
        match next {
            Some(value) if value != current => current = value,
            _ => break,
        }
    }
    current
}

pub(crate) fn strip_trailing_context(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    for marker in [" right now", " currently", " at the moment", " for now"] {
        if let Some(index) = lower.find(marker) {
            return value[..index].trim().to_string();
        }
    }
    value.trim().to_string()
}

pub(crate) fn first_sentence(value: &str) -> String {
    let mut char_count = 0;
    for (index, ch) in value.char_indices() {
        char_count += 1;
        if char_count >= 12 && matches!(ch, '.' | '!' | '?') {
            return value[..index].trim().to_string();
        }
    }
    value.trim().to_string()
}

pub(crate) fn compact_title(value: &str, max_chars: usize) -> Option<String> {
    let mut selected = Vec::new();
    for word in
        value.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '/' | '-')))
    {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if is_task_summary_stop_word(&lower) {
            continue;
        }
        selected.push(word.to_string());
        if selected.join(" ").chars().count() >= max_chars.saturating_sub(1) || selected.len() >= 8
        {
            break;
        }
    }
    let compact = strip_terminal_punctuation(&selected.join(" "));
    if !compact.is_empty() && compact.chars().count() < value.chars().count() {
        Some(truncate_chars(&compact, max_chars))
    } else {
        None
    }
}

pub(crate) fn is_task_summary_stop_word(value: &str) -> bool {
    matches!(
        value,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "but"
            | "by"
            | "for"
            | "from"
            | "gets"
            | "have"
            | "in"
            | "is"
            | "it"
            | "its"
            | "just"
            | "of"
            | "on"
            | "or"
            | "put"
            | "puts"
            | "putting"
            | "right"
            | "so"
            | "than"
            | "that"
            | "the"
            | "then"
            | "this"
            | "to"
            | "user"
            | "what"
            | "when"
            | "where"
            | "with"
            | "you"
            | "your"
    )
}

pub(crate) fn strip_terminal_punctuation(value: &str) -> String {
    value
        .trim_end_matches(|ch| matches!(ch, '.' | '!' | '?'))
        .trim()
        .to_string()
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let short = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        if max_chars <= 3 {
            ".".repeat(max_chars)
        } else {
            format!(
                "{}...",
                short.chars().take(max_chars - 3).collect::<String>()
            )
        }
    } else {
        short
    }
}

pub(crate) fn preview_text(value: &str, max_chars: usize) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "\\\"");
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}
