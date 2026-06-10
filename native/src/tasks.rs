#![allow(unused_imports)]
//! Prompt construction, task-summary generation, and rudder-plan parsing.
use super::*;

pub(crate) fn execution_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    const CONTEXT: &str = "Rudder-specific context injected by Rudder:\n- Version control: this repo uses jj (Jujutsu), colocated with git. You are working inside your own isolated jj workspace and jj is authoritative here. Inspect your work with `jj status` and `jj diff` (NOT `git status`/`git diff`). Just edit files: do NOT manage version control yourself (no `jj commit`, `jj new`, `jj squash`, `jj describe`, `git commit`, `git add`, `git branch`, `git checkout`, or `git merge`). Rudder snapshots your working copy and integrates it for you, and raw git commit/branch commands would desync jj. `jj log` is safe if you want to see history.\n- Read RUDDER.md first if it exists. Rudder generated it to show the CURRENT PLAN (the task DAG and each node's status), the active agents, and their worktrees. DECISIONS.md holds cross-cutting decisions other agents have recorded. If RUDDER_SHARED.md exists beside RUDDER.md, read it too: it is gitignored local context the user shared for all agents, often API tokens, credentials, private URLs, or env details that must survive model compaction.\n- The plan can CHANGE while you work: the user may refine the architecture, or a sibling may record a decision. Before each significant step, re-read RUDDER.md, DECISIONS.md, and RUDDER_SHARED.md when present. RUDDER.md carries a `freshness:` stamp; if it is newer than when you last read it, something changed, so re-read all shared context. If the plan or architecture has shifted in a way that affects your task, ADAPT your implementation to the new direction instead of continuing on the old one, and append a short note to DECISIONS.md describing the adjustment. Never edit RUDDER.md; it is orchestrator-owned.\n- REQUIRED FINAL STEP — when you finish, your LAST action MUST be to run `rudder done`. This is the ONLY way the orchestrator learns what you did and what work remains; if you skip it, your results are invisible and the plan cannot advance. Pipe a JSON object: `echo '{\"summary\":\"...\",\"interfaces\":\"files/types/functions you created or assumed\",\"followups\":[{\"title\":\"...\",\"why\":\"...\",\"scope\":\"in|out\"}]}' | rudder done --node <id>` where <id> is your `Worker node:` value shown above (omit `--node` if there is none). Use `scope:\"out\"` for follow-ups outside your lane. If you have no structured note, `rudder done --node <id> \"one-line summary\"` is fine. Run it exactly once, after your work is complete. It only records a report (it does not touch jj).";
    // Keep the objective block as the very first lines, but do NOT launch with a
    // leading `/goal` slash command or a literal `Goal:` header. Claude Code can route
    // goal-looking launch text through its goal condition machinery, so a long worker
    // brief trips "Goal condition is limited to 4000 characters" even when the actual
    // objective is short. User-typed `/goal` forwarding is still supported elsewhere.
    if starts_goal_prompt(task.trim_start()) {
        let (goal_block, rest) = split_leading_goal_block(&task);
        return format!("{goal_block}\n\n{CONTEXT}\n\n{rest}");
    }
    format!("{CONTEXT}\n\n{task}")
}

/// Split a goal-formatted prompt into its leading objective + `Done when: ...` block
/// and the remaining body. Legacy inputs may lead with `/goal` or `Goal:`; normalize
/// them to `Objective:` so process-launch prompts are never parsed as backend goals.
fn split_leading_goal_block(task: &str) -> (String, String) {
    let task = task.trim_start();
    let mut lines = task.lines();
    let mut block: Vec<String> = Vec::new();
    if let Some(goal_line) = lines.next() {
        block.push(cap_goal_command_line(goal_line));
    }
    // The optional Done-when line immediately follows the /goal line.
    let mut rest_lines: Vec<&str> = Vec::new();
    let mut consumed_done = false;
    for line in lines {
        if !consumed_done {
            consumed_done = true;
            if line
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("done when:")
            {
                block.push(cap_goal_command_line(line));
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

fn starts_goal_prompt(value: &str) -> bool {
    value == "/goal"
        || value
            .strip_prefix("/goal")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        || value.to_ascii_lowercase().starts_with("objective:")
        || value.to_ascii_lowercase().starts_with("goal:")
}

/// Cap a hoisted objective / `Done when: ...` line's ARGUMENT to
/// MAX_GOAL_LINE_CHARS while keeping the label prefix intact.
fn cap_goal_command_line(line: &str) -> String {
    if line == "/goal" {
        return "Objective: complete the task".to_string();
    }
    if let Some(rest) = line.strip_prefix("/goal") {
        if rest.starts_with(char::is_whitespace) {
            return format!(
                "Objective: {}",
                cap_goal_line(rest.trim_start().to_string())
            );
        }
    }
    if let Some(arg) = line.strip_prefix("Goal: ") {
        return format!("Objective: {}", cap_goal_line(arg.to_string()));
    }
    for prefix in ["Objective: ", "Done when: "] {
        if let Some(arg) = line.strip_prefix(prefix) {
            return format!("{prefix}{}", cap_goal_line(arg.to_string()));
        }
    }
    line.to_string()
}

pub(crate) fn plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "Plan this task before implementation. Inspect the repository and relevant read-only context first. Ask follow-up questions if the plan cannot be made decision-complete from inspection alone.\n\n{task}"
    )
}

/// Prompt for a ONE-OFF agent: a single conversational agent the user talks to in the
/// MAIN checkout for a question or a small self-contained change — NOT a multi-step DAG.
/// No /goal, no `rudder done`, no isolated-workspace framing: edits land in the working
/// tree directly and the agent should escalate to the planner if the work turns out big.
pub(crate) fn oneoff_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "You are a Rudder one-off agent. The user wants to ASK A QUESTION or make a SMALL, self-contained change — not a multi-step project. You are running directly in the user's working checkout (NOT an isolated workspace), so any edits you make land in their working tree. Follow the repo's own rules: read CLAUDE.md and AGENTS.md if they exist and honor them for any change you make. Read RUDDER.md if it exists, and read RUDDER_SHARED.md if it exists; RUDDER_SHARED.md is gitignored local context the user shared for agents, often API tokens, credentials, private URLs, or env details. Be conversational and focused: answer the question, or make the change directly and explain what you did. Do NOT run git/jj commit, branch, or merge commands — just edit files; the user manages version control. If the request turns out to be large or genuinely multi-part, say so and suggest the user let Rudder plan it as a multi-agent task instead of doing it all here.\n\n{task}"
    )
}

pub(crate) fn rudder_plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    [
        "You are Rudder's planning coordinator. You decompose one user request into a DAG of implementation tasks that a separate set of worker agents will implement in isolated git worktrees. You inspect the repository in READ-ONLY mode. You do NOT implement anything yourself. PROJECT RULES: read CLAUDE.md and AGENTS.md if they exist and honor them; when a rule affects implementation (build/test commands, style, architecture constraints), copy the relevant constraint into each affected task's `prompt` so workers inherit it. Read RUDDER_SHARED.md if it exists; it is gitignored local context the user shared for all agents, often API tokens, credentials, private URLs, or env details that must be included by reference in worker tasks.".to_string(),
        format!("User request:\n{task}"),
        "Process:\n1. Inspect the relevant files read-only to understand the work.\n2. You run TURN BY TURN, like plan mode: the user can reply and you resume with their answer, so treat this as a conversation. On your FIRST turn for a request, ALWAYS ASK 1 to 4 concise, specific clarifying questions about the things that shape the build (scope, approach, key product/technical decisions, constraints, what existing work to reuse vs rebuild) and STOP — do NOT emit a task DAG on this first turn. This is mandatory even for trivial or fully specified requests; if nothing material seems missing, ask the user to confirm that Rudder should use your judgment. Output the questions wrapped EXACTLY in these markers, ONE question per line, so they render as a clean numbered prompt:\nRUDDER_QUESTIONS_START\nWhich time range: last 4 weeks, 6 months, or all time?\nReuse the existing module contract, or rebuild from scratch?\nRUDDER_QUESTIONS_END\nThen stop. Only emit the DAG on a later turn that carries the user's answer. If that answer still leaves something materially ambiguous, ask 1 to 2 more concise questions and stop again; otherwise emit the COMPLETE task DAG. Do not ask pure trivia.\n3. Once the request is clear, decompose it into a DAG of tasks. Each task gets a short stable `id` (for example n0, n1, n2) and a `deps` array of typed edges to the task ids it depends on.\n4. Edge types: `hard` means the child's success condition CANNOT be met until the parent has merged. A task that CONSUMES another task's produced code is HARD on it: tests that import or exercise code another task writes, code that imports a module/function/type another task creates, or wiring that calls an API another task defines. The child can technically start, but it cannot SUCCEED (imports resolve, tests pass) until that code exists, so it must wait for the merge. `soft` means context-only: the parent's diff is delivered as background once it lands, but the child can succeed on its own in parallel (parallel features, doc updates, sibling modules that do not import each other). Use the MINIMAL set of hard edges: default to soft for independent work, but do NOT under-classify, if the child executes or imports the parent's code it is hard. Every hard edge needs a one-line `why`.\n5. Split into independent or softly-coupled tasks wherever the work can proceed in parallel across separate worktrees. Do NOT collapse everything into one task, and do NOT make every task independent when real ordering exists: model the ordering with hard/soft edges instead. Be THOROUGH: produce a genuinely complete decomposition (up to ~10 tasks for a substantial request), name the concrete files each task creates or edits in its `title` or `goal`, and split separable work (distinct modules, tests, docs, config) into its own task. Never pad with busywork, but do not under-decompose either.\n6. Each task must be self-contained, name the concrete files it creates or edits, and carry its own verification. Each worker runs in its OWN isolated workspace at a different filesystem path than this repository. In every `prompt`, `goal`, and `success`, refer to files by REPOSITORY-RELATIVE paths only (for example `mathutils.py`, `src/db/schema.ts`). NEVER embed an absolute filesystem path, this repository's location, or phrases like \"in the repository at <path>\" or \"cd into <path>\" — the worker is already in its workspace, and an absolute path sends the worker to the wrong directory and the task fails.\n7. Every task MUST carry both a `goal` and a `success`. `goal` is one line naming the single objective the worker should accomplish. `success` is the verifiable DONE-WHEN condition: the commands, artifacts, or criteria that mean the task is complete. Never omit either or leave them empty.".to_string(),
        "CONFLICT-SAFE DECOMPOSITION (critical — every parallel worker merges back into ONE tree, so structure the DAG to minimize merge collisions; this is as important as correctness):\n- FOUNDATION FIRST: put shared groundwork (project scaffold, shared schema / data-access layer, shared config, shared types) in early node(s) that every feature node HARD-depends on. Features must not begin until the foundation has merged, so they all branch from the SAME settled base instead of each re-creating it and colliding.\n- DISJOINT PATHS: scope each parallel feature to its OWN files/directories (its own route folder, its own module). Two sibling (soft-coupled) nodes must NOT edit the same source files. If two tasks would both edit one file, either merge them into a single node or make one HARD-depend on the other so they run sequentially, not concurrently.\n- SHARED MANIFESTS ARE OWNED, NOT SHARED: single-file manifests that every feature would otherwise touch (package.json, lock files, .gitignore, tsconfig, global config) should be ESTABLISHED COMPLETELY by the foundation node — including all dependencies the features will need. Do not spread edits to these across many parallel features; that produces a merge conflict on essentially every integration.\n- SUPERSEDE CLEANLY: if the build replaces existing draft/scaffold files, the foundation node must DELETE the superseded files authoritatively (name them in its prompt) so later feature merges never resurrect or re-delete them.\n- BROAD PASSES RUN LAST, ALONE: any whole-codebase pass (final polish, integration, cleanup, formatting, docs sweep) must be a SINGLE terminal node that HARD-depends on every feature node it touches, so it runs by itself after they have all merged — never as a sibling editing the same files concurrently with the features.".to_string(),
        "YOUR ROLE: You are a read-only DECOMPOSER, not an implementer. Your tools are inspection-only, so you cannot and must not edit, write, or run code. Your only deliverable is the task DAG below. Do NOT implement the work and do NOT ask to proceed: a separate set of worker agents implements each task in its own isolated workspace, and Rudder shows the user this plan for approval before launching them. When the DAG is ready, print exactly the block below as a normal assistant message and then stop.".to_string(),
        r#"Print exactly this block and no other JSON block:
RUDDER_PLAN_TASKS_START
{"tasks":[{"id":"n0","title":"short task title","prompt":"full implementation prompt for one worker agent","goal":"one-line objective for the worker","success":"verifiable done-when condition","deps":[]},{"id":"n1","title":"...","prompt":"...","goal":"...","success":"...","deps":[{"on":"n0","type":"hard","why":"edits the module n0 creates"}]}]}
RUDDER_PLAN_TASKS_END

After the block, add a short human summary of why this DAG is safe."#.to_string(),
    ].join("\n\n")
}

/// Where the INTERACTIVE orchestrator writes its task DAG (a RUDDER_PLAN_TASKS block).
/// The generated Rudder projection is wrapped in markers, and the orchestrator owns
/// content outside those markers. Rudder merges its generated block so this file can
/// be both the live context surface and the orchestrator's DAG/control channel.
pub(crate) fn orchestrator_plan_path(repo_root: &std::path::Path) -> std::path::PathBuf {
    repo_root.join("RUDDER.md")
}

/// System prompt for the INTERACTIVE orchestrator (a normal Claude Code PTY the user
/// converses with, behind RUDDER_INTERACTIVE_ORCHESTRATOR). Unlike the headless
/// decomposer it does not print a one-shot block to stdout; it writes the DAG to
/// RUDDER.md, which Rudder reads to populate + render the DAG.
pub(crate) fn orchestrator_system_prompt() -> String {
    [
        "You are Rudder's INTERACTIVE orchestration agent, running as a normal Claude Code session the user talks to directly.",
        "Your job: converse with the user, inspect the repository READ-ONLY, and maintain the task DAG that a SEPARATE set of worker agents will implement in isolated worktrees. You do NOT implement product code yourself.",
        "PROJECT RULES: the user's repo rules apply to YOU and to the plans you write. Before planning, read CLAUDE.md and AGENTS.md (and any rules they point to) if they exist, and honor your loaded Claude Code memory and settings. When a rule affects implementation (build/test commands, style, architecture constraints, files to avoid), copy the relevant constraint INTO each affected task's `prompt` so the worker inherits it — workers only reliably see what their prompt carries.",
        "If RUDDER_SHARED.md exists beside RUDDER.md, read it before planning. It is Rudder's gitignored local shared-context file for API tokens, credentials, private URLs, env details, and other user-provided context that every worker may need. If the user shares any credential, token, private URL, account id, env var, or similar local setup detail in this conversation, append it exactly to RUDDER_SHARED.md so it survives model compaction and becomes visible to workers. Do not put secret values in RUDDER.md or DECISIONS.md.",
        "Ask concise clarifying questions in the conversation whenever the request is ambiguous (scope, approach, key decisions, what to reuse vs rebuild). This is a back-and-forth: the user replies and you continue.",
        "When the task DAG is ready, WRITE it to `RUDDER.md` OUTSIDE the `<!-- RUDDER_GENERATED_START -->` / `<!-- RUDDER_GENERATED_END -->` generated block, wrapped EXACTLY in these markers (one JSON object on its own lines), then tell the user it is ready for approval:\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"short title\",\"prompt\":\"full worker prompt\",\"goal\":\"one-line objective\",\"success\":\"verifiable done-when\",\"deps\":[]},{\"id\":\"n1\",\"title\":\"...\",\"prompt\":\"...\",\"goal\":\"...\",\"success\":\"...\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"edits the module n0 creates\"}]}]}\nRUDDER_PLAN_TASKS_END",
        "Edge types: `hard` = the child cannot SUCCEED until the parent has merged (it imports/executes the parent's code); `soft` = context-only, can run in parallel. Use the minimal hard-edge set; every hard edge needs a one-line `why`. Each task carries a `goal` and a verifiable `success`, and refers to files by REPOSITORY-RELATIVE paths only.",
        "CONFLICT-SAFE DECOMPOSITION (every parallel worker merges back into ONE tree, so structure the DAG to minimize merge collisions): FOUNDATION FIRST — shared groundwork (scaffold, shared schema/data-access layer, shared config/types) goes in early node(s) that every feature HARD-depends on, so features branch from the same settled base. DISJOINT PATHS — scope each parallel feature to its OWN files/dirs; two soft-coupled siblings must not edit the same source file (merge them, or make one hard-depend on the other). SHARED MANIFESTS ARE OWNED — package.json, lock files, .gitignore, tsconfig and global config are established completely by the foundation node (including deps features will need), not edited by many parallel features. SUPERSEDE CLEANLY — if replacing draft/scaffold files, the foundation node deletes the superseded files authoritatively so later merges do not resurrect them. BROAD PASSES RUN LAST, ALONE — any whole-codebase pass (final polish, integration, cleanup, formatting) is a single terminal node that hard-depends on every feature it touches, never a concurrent sibling.",
        "Rudder renders the DAG in the pane ABOVE this terminal and reloads it LIVE from RUDDER.md. The plan is a LIVING document: whenever the user asks to ADD, CHANGE, REMOVE, reorder, re-scope, or re-split tasks at ANY point before they approve, UPDATE the DAG by re-writing the FULL RUDDER_PLAN_TASKS block in RUDDER.md to match - keep stable `id`s for unchanged nodes, add new ones with fresh ids, drop removed ones, and fix up `deps` so the edges stay correct. Always keep the block in sync with what you and the user have agreed; never describe a plan change only in chat without also writing it to the file.",
        "APPROVAL / LAUNCH: After - and ONLY after - the user has EXPLICITLY confirmed in the conversation that the plan is good to run (e.g. \"yes\", \"go\", \"approve\", \"launch it\"), signal Rudder to launch the workers by WRITING (Edit/Write) the line `RUDDER_APPROVE_PLAN` on its own line into RUDDER.md, keeping the RUDDER_PLAN_TASKS block in the file. Writing it into RUDDER.md (a structured Write) is the reliable channel; you MAY also print the same line in the chat as a fallback. Never write/print it preemptively, while you are still asking questions, or while you are revising the plan. When you merely need to REFER to the marker in prose, write it as RUDDER_APPROVE_PLAN_TEMPLATE so Rudder does not treat the mention as a launch trigger. Once you write RUDDER_APPROVE_PLAN your job is DONE: Rudder generates the DAG (graph.json), launches the SEPARATE worker fleet, and stops this planning session. Do not implement anything or keep working after that.",
        "You have Rudder project skills available under `.claude/skills/rudder-*`. Use them for dashboard actions when the user asks from this orchestrator session. Those skills tell you which exact `RUDDER_*` control marker to write to RUDDER.md; Rudder consumes the marker and runs the dashboard action.",
        "Only ever Edit/Write RUDDER.md, RUDDER_SHARED.md, and the generated `.claude/skills/rudder-*` SKILL.md files. Treat all other files as read-only.",
    ]
    .join("\n\n")
}

/// First-turn prompt for the interactive orchestrator (paired with the system prompt).
pub(crate) fn rudder_orchestrator_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    let base = "You are Rudder's orchestrator for this checkout. Read CLAUDE.md and AGENTS.md first if they exist (the repo's rules bind you and the plans you write). Read RUDDER.md if it exists, and read RUDDER_SHARED.md if it exists. RUDDER_SHARED.md is gitignored local context for all agents; if the user shares tokens, credentials, private URLs, account ids, or env details in this conversation, append them there so workers can read them later. If no concrete task is given yet, say you are ready and wait. For a concrete request, ask clarifying questions when needed; once clear, write the DAG to RUDDER.md outside Rudder's generated block and ask the user to approve. Only AFTER they explicitly confirm, add a `RUDDER_APPROVE_PLAN` line to RUDDER.md (keeping the plan block) to launch.";
    if task.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\nUser request:\n{task}")
    }
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
        "You are Rudder's injection coordinator. A multi-agent plan is ALREADY in flight; a separate set of worker agents is implementing its nodes in isolated git worktrees. The user is now ADDING one more task to that running plan. You inspect the repository in READ-ONLY mode. You do NOT implement anything yourself, and you do NOT re-plan or replace the existing work.\n\nExisting plan nodes (the frontier, in-flight, not yet merged):\n{existing}\n\nExisting node ids: [{existing_ids}]\n\nNew task the user is ADDING:\n{task}\n\nProcess:\n1. Inspect the relevant files read-only to understand how the new task relates to the in-flight nodes.\n2. Emit EXACTLY ONE node in the RUDDER_PLAN_TASKS block. Its `id` MUST be unique from every existing node id above. Its `deps` reference the existing frontier ids.\n3. Edge rules: add a `hard` dep only if the new task genuinely CANNOT start until that node merges (it edits that node's not-yet-merged code, or needs an interface that node defines) and justify it with a one-line `why`. Otherwise use `soft` deps for nodes the new task should be aware of, or no deps at all if the new task is fully independent. Prefer soft; a hard edge without a `why` is treated as soft.\n4. The node must be self-contained, name concrete files or modules to inspect when known, and carry both a `goal` (one-line objective the worker should accomplish) and a verifiable `success` (the DONE-WHEN condition). Never omit either. The worker runs in its OWN isolated workspace at a different filesystem path than this repository: refer to files by REPOSITORY-RELATIVE paths only, and NEVER embed an absolute path, the repository's location, or phrases like \"in the repository at <path>\" — an absolute path sends the worker to the wrong directory and the task fails.\n\nPLAN MODE: You are running in plan mode. Do NOT call ExitPlanMode. Do NOT implement, edit, or write any files. When the node is ready, print exactly the block below as a NORMAL assistant message and then stop. Rudder reads the block directly from your output; it does not need you to exit plan mode.\n\nPrint exactly this block and no other JSON block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"id\":\"new\",\"title\":\"short task title\",\"prompt\":\"full implementation prompt for one worker agent\",\"goal\":\"one-line objective for the worker\",\"success\":\"verifiable done-when condition\",\"deps\":[{{\"on\":\"<existing frontier id>\",\"type\":\"soft\",\"why\":\"context the new task should be aware of\"}}]}}]}}\nRUDDER_PLAN_TASKS_END\n\nAfter the block, add a short human summary of how the added node relates to the in-flight plan."
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
    /// One-line OBJECTIVE for the launch Objective line. Optional in the parse for
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

/// A queued unit of plannable work that has not launched yet. The scheduler
/// drains these into live `AgentRun`s as their hard deps merge and parallelism
/// slots free. Created one-per-task when a planner agent completes; rendered in
/// the Todo section until launched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        let mut task = task.clone();
        preflight_plan_task_for_launch(&mut task);
        let deps: Vec<String> = task.hard_deps().map(ToString::to_string).collect();
        let soft_deps: Vec<String> = task
            .deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Soft)
            .map(|edge| edge.on.clone())
            .collect();
        Self {
            id: task.id,
            title: task.title,
            prompt: task.prompt,
            goal: task.goal,
            success: task.success,
            deps,
            soft_deps,
            backend: task.backend,
            model: task.model,
            effort: task.effort,
        }
    }

    /// View this queued node as a plan task so the orchestrator pane can render a
    /// reconciled (post-launch) addition in the SAME DAG as the initial nodes. The
    /// initial nodes live in the orchestrator's frozen plan block; reconciled nodes
    /// live only here, so without this they would be invisible in the DAG.
    pub(crate) fn to_task(&self) -> RudderPlanTask {
        let mut deps: Vec<PlanEdge> = self
            .deps
            .iter()
            .map(|on| PlanEdge {
                on: on.clone(),
                edge: EdgeType::Hard,
                why: None,
            })
            .collect();
        deps.extend(self.soft_deps.iter().map(|on| PlanEdge {
            on: on.clone(),
            edge: EdgeType::Soft,
            why: None,
        }));
        RudderPlanTask {
            id: self.id.clone(),
            title: self.title.clone(),
            prompt: self.prompt.clone(),
            goal: self.goal.clone(),
            success: self.success.clone(),
            deps,
            backend: self.backend.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
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
pub(crate) fn planned_node_worker_prompt(
    planner_task: &str,
    node: &PlannedNode,
    depends_on: &str,
) -> String {
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
    rudder_plan_worker_prompt(planner_task, &task, depends_on, Backend::Claude)
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

    // Compare against the count of DISTINCT ids (indegree is keyed by id), not tasks.len():
    // duplicate ids dedupe in the maps, so tasks.len() would over-count and falsely report a
    // cycle, rejecting the whole plan. The TS daemon parser accepts duplicate ids, so this
    // keeps the two parsers in parity (a real cycle still leaves a node unvisited).
    if visited != indegree.len() {
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
        // OFFICIAL capture: a real plan-mode `ExitPlanMode` plan is the authoritative
        // text — it carries the RUDDER_PLAN_TASKS block even when the plan was written
        // to the plan file rather than streamed as assistant text. Falls back to the
        // reconstructed assistant text (headless decomposer / older flow).
        if let Some(plan) = stream.exit_plan() {
            return plan.to_string();
        }
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

/// Upper bound on tasks accepted from ONE plan block. This is NOT a feature limit
/// (real plans are far smaller); it is a runaway/cost backstop: every task becomes
/// a real worker agent (a Claude/Codex process), so an unbounded or hallucinated
/// block could spawn hundreds of processes. 100 is far above any legitimate single
/// plan, and a plan that exceeds it is surfaced (never silently truncated). Add
/// more work incrementally by typing into the task pane (reconcile) rather than one
/// giant block.
pub(crate) const MAX_PLAN_TASKS: usize = 100;

/// Extract the planner's clarifying questions from a `RUDDER_QUESTIONS_START..END` block
/// (one question per line, leading numbering/bullets stripped). Empty when the planner did
/// not ask. Used to render a clean numbered question prompt while the planner is paused.
pub(crate) fn extract_rudder_questions(output: &str) -> Vec<String> {
    const START: &str = "RUDDER_QUESTIONS_START";
    const END: &str = "RUDDER_QUESTIONS_END";
    let Some(start) = output.rfind(START) else {
        return Vec::new();
    };
    let after = &output[start + START.len()..];
    let body = match after.find(END) {
        Some(end) => &after[..end],
        None => after,
    };
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ' ' | '\t')
            })
            .trim()
            .to_string()
        })
        .filter(|line| !line.is_empty())
        .take(8)
        .collect()
}

pub(crate) fn forced_planner_questions() -> Vec<String> {
    vec![
        "Before I build the DAG, what scope, constraints, approach preferences, or existing work should shape this plan? Reply with details, or say \"use your judgment\" to continue.".to_string(),
    ]
}

pub(crate) fn planner_questions_or_forced(output: &str) -> Vec<String> {
    let questions = extract_rudder_questions(output);
    if questions.is_empty() {
        forced_planner_questions()
    } else {
        questions
    }
}

pub(crate) fn extract_rudder_plan_tasks(output: &str) -> Result<Vec<RudderPlanTask>> {
    extract_rudder_plan_tasks_with_frontier(output, &[])
}

/// The number of tasks the planner actually emitted in its block, BEFORE the
/// `MAX_PLAN_TASKS` backstop. Lets the caller tell the user when a plan was
/// truncated instead of silently dropping tasks. None if there is no valid block.
pub(crate) fn rudder_plan_block_task_count(output: &str) -> Option<usize> {
    const START: &str = "RUDDER_PLAN_TASKS_START";
    const END: &str = "RUDDER_PLAN_TASKS_END";
    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let start = clean.rfind(START)?;
    let after_start = &clean[start + START.len()..];
    let end = after_start.find(END)?;
    let mut json = after_start[..end].trim();
    json = json.strip_prefix("```json").unwrap_or(json).trim();
    json = json.strip_prefix("```").unwrap_or(json).trim();
    json = json.strip_suffix("```").unwrap_or(json).trim();
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
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
    let capped: Vec<&serde_json::Value> = tasks.iter().take(MAX_PLAN_TASKS).collect();

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

        let mut plan_task = RudderPlanTask {
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
        };
        preflight_plan_task_for_launch(&mut plan_task);
        out.push(plan_task);
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
    // Show the FULL human summary (the orchestrator pane scrolls it). A generous
    // upper bound only guards against a pathological reply, not normal prose; the
    // old 1500-char cap chopped real summaries off mid-sentence.
    Some(after.chars().take(20_000).collect())
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
/// Resume framing when the planner PAUSED to ask a clarifying question and the user is
/// ANSWERING (no plan exists yet). Distinct from build_refine_followup so the model is not
/// told to revise "the plan you produced" and is allowed to ask once more if still unsure.
pub(crate) fn build_clarification_answer_followup(answer: &str) -> String {
    format!(
        "Here is the user's answer to the clarifying question(s) you asked. Use it to finish planning. If something is STILL materially ambiguous you may ask 1 to 2 more concise questions and stop; otherwise emit the COMPLETE task DAG now as one block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[...]}}\nRUDDER_PLAN_TASKS_END\nAfter the block, add a short note of any assumptions you made.\n\nUser's answer:\n{answer}"
    )
}

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

/// Build the REBASE prompt: a structural mid-flight change while work is already in
/// flight. Unlike refine (pre-launch), this re-decomposes against THREE live zones:
/// MERGED work has already landed and is the baseline (build forward — do NOT re-plan
/// it; reference its ids as satisfied deps); RUNNING work is in flight (keep its id to
/// preserve the agent, change its goal to re-task it, or omit it to stop it); TODO work
/// is unlaunched and may be replaced freely. Reuse stable ids so the diff can match
/// nodes across the rewrite. The planner re-emits ONE full RUDDER_PLAN_TASKS block.
pub(crate) fn build_rebase_request(
    merged: &str,
    running: &str,
    todo: &str,
    direction: &str,
) -> String {
    format!(
        "The user is steering the project in a new direction MID-FLIGHT. Re-plan the work as a single complete task DAG that honours what has already happened. There are three zones; treat each differently and REUSE existing ids so the change can be applied surgically.\n\nMERGED (already landed — the baseline; do NOT re-plan these, build FORWARD on them; you may reference their ids as already-satisfied dependencies):\n{merged}\n\nRUNNING (in flight right now — keep an id to leave that agent working, restate its goal to re-task it, or omit it to stop it):\n{running}\n\nTODO (queued, not started — replace freely):\n{todo}\n\nNew direction from the user:\n{direction}\n\nRe-emit the FULL updated plan as one block exactly as before:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[...]}}\nRUDDER_PLAN_TASKS_END\nApply the new direction directly — do not ask questions. After the block, add a short note on what changed and why.",
    )
}

/// A running plan node as seen by the rebase differ: its stable id, its title (for the
/// title-overlap fallback when the planner renames ids), and `context` — the text the
/// agent is currently working from (its launch prompt / last goal). `context` is used
/// only to decide whether the new plan's goal for this node is materially different
/// (re-goal) or effectively the same (keep): if the new goal text is not already
/// present in the agent's context, its objective changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningNode {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) context: String,
}

/// The result of diffing a revised plan against the live RUNNING + TODO zones (MERGED
/// is excluded — it is the immutable baseline). Drives the autonomous apply in
/// `evaluate_completed_rebase`: running nodes are kept/re-goaled/stopped, the TODO queue
/// is rebuilt from `todo`, and `added` is the subset of `todo` that is brand new.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlanDiff {
    /// Running node ids the new plan keeps unchanged (matched, same objective).
    pub(crate) kept: Vec<String>,
    /// Running nodes whose objective changed: `(node_id, new_goal)` → re-goal.
    pub(crate) regoaled: Vec<(String, String)>,
    /// Running ids the new plan dropped + old TODO ids it no longer contains → stop/drop.
    pub(crate) dropped: Vec<String>,
    /// Brand-new planned nodes (in `todo` but with an id not previously queued).
    pub(crate) added: Vec<PlannedNode>,
    /// The full rebuilt TODO queue, in plan order, ready to REPLACE `planned_nodes`:
    /// every new task that is not a running or merged node.
    pub(crate) todo: Vec<PlannedNode>,
}

/// Normalize a title/goal for matching: lowercase, collapse runs of non-alphanumerics
/// to single spaces, trim. So "Add Rate-Limiting!" and "add rate limiting" compare equal.
pub(crate) fn norm_title(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_space = true; // trims leading separators
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The new plan's goal for a still-running node is a re-goal (objective changed) when
/// it is non-empty AND not already contained in what the agent is working from. A
/// planner that re-states the same objective produces a goal already present in the
/// agent's launch context, so that stays a KEEP (no disruptive PTY respawn).
pub(crate) fn goal_changed(context: &str, new_goal: &str) -> bool {
    let goal = norm_title(new_goal);
    if goal.is_empty() {
        return false;
    }
    !norm_title(context).contains(&goal)
}

/// Pure structural diff of a revised plan against the live zones. MERGED ids are the
/// baseline (their tasks are skipped — already landed). Each new task is matched to a
/// RUNNING node by id, then by normalized title; a match is kept or re-goaled. Unmatched
/// new tasks become TODO nodes (flagged `added` when their id was not already queued).
/// Running nodes no match touched → dropped; old TODO ids absent from the new plan are
/// also reported dropped (the queue is rebuilt regardless, but the diff stays visible).
pub(crate) fn diff_plan(
    running: &[RunningNode],
    todo: &[PlannedNode],
    merged_ids: &[String],
    new_tasks: &[RudderPlanTask],
) -> PlanDiff {
    use std::collections::HashSet;
    let merged: HashSet<&str> = merged_ids.iter().map(String::as_str).collect();
    let old_todo_ids: HashSet<&str> = todo.iter().map(|n| n.id.as_str()).collect();

    let mut diff = PlanDiff::default();
    let mut matched_running: HashSet<String> = HashSet::new();

    for task in new_tasks {
        // Build-forward: a task that re-describes already-merged work is ignored.
        if merged.contains(task.id.as_str()) {
            continue;
        }
        // Match to a RUNNING node: id first, then normalized title (planner renamed id).
        let run_match = running.iter().find(|node| node.id == task.id).or_else(|| {
            let nt = norm_title(&task.title);
            running.iter().find(|node| norm_title(&node.title) == nt)
        });
        if let Some(node) = run_match {
            matched_running.insert(node.id.clone());
            let new_goal = task.goal.clone().unwrap_or_default();
            if goal_changed(&node.context, &new_goal) {
                diff.regoaled.push((node.id.clone(), new_goal));
            } else {
                diff.kept.push(node.id.clone());
            }
            continue;
        }
        // Otherwise it is a TODO node (carried over or brand new).
        let planned = PlannedNode::from_task(task);
        if !old_todo_ids.contains(planned.id.as_str()) {
            diff.added.push(planned.clone());
        }
        diff.todo.push(planned);
    }

    // Running nodes the new plan abandoned → stop.
    for node in running {
        if !matched_running.contains(&node.id) {
            diff.dropped.push(node.id.clone());
        }
    }
    // Old TODO ids absent from the new plan → dropped (visible in the diff).
    let new_ids: HashSet<&str> = new_tasks.iter().map(|t| t.id.as_str()).collect();
    for node in todo {
        if !new_ids.contains(node.id.as_str()) {
            diff.dropped.push(node.id.clone());
        }
    }
    diff
}

/// Replacement verbs / phrases that signal a typed message is a STRUCTURAL pivot away
/// from the current plan (re-plan the whole DAG) rather than an ADDITIVE request (fold
/// one node in). Matched case-insensitively as substrings of the message.
const STRUCTURAL_MARKERS: &[&str] = &[
    "instead",
    "scrap",
    "rewrite",
    "re-write",
    "pivot",
    "start over",
    "start from scratch",
    "throw out",
    "throw away",
    "ditch",
    "abandon",
    "rather than",
    "no longer",
    "forget the",
    "forget about",
    "drop everything",
    "different approach",
    "rethink",
    "redo the",
    "redo everything",
    "overhaul",
    "completely change",
    "change the whole",
    "change everything",
    "replace the plan",
    "replan",
    "re-plan",
];

/// Stopwords excluded from the title-overlap heuristic so a shared common word does not
/// make an additive request look structural.
const TITLE_STOPWORDS: &[&str] = &[
    "about", "after", "again", "their", "there", "these", "those", "which", "while", "would",
    "could", "should", "with", "from", "into", "that", "this", "then", "them", "they", "have",
    "your", "make", "code", "task", "node", "using", "based", "where",
];

/// Decide whether a typed message during CONDUCTING is a STRUCTURAL pivot (true → rebase
/// the whole plan) or an ADDITIVE request (false → reconcile one node). Two cheap signals:
/// (1) any replacement verb/phrase, or (2) the message references a MAJORITY of existing
/// node titles (reshaping the plan wholesale, not adding to it). Pure + unit-tested; a
/// misfire is cheaply reversible (a rebase that yields no changes pops the plan back).
/// True if `needle` appears in `haystack` bounded by non-alphanumeric chars (or string
/// edges) on both sides, so a marker like "instead" matches the word but not a longer
/// word that merely contains it. Multi-word markers ("rather than") match as a phrase.
fn contains_word_phrase(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let before_ok = at == 0
            || !haystack[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let after = at + needle.len();
        let after_ok = after >= haystack.len()
            || !haystack[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

pub(crate) fn is_structural_direction(input: &str, titles: &[String]) -> bool {
    let lower = input.to_lowercase();
    // Whole-word/phrase match so a marker inside a longer word (or an incidental
    // substring) does not force a full plan rebase.
    if STRUCTURAL_MARKERS
        .iter()
        .any(|marker| contains_word_phrase(&lower, marker))
    {
        return true;
    }
    if titles.len() < 2 {
        return false;
    }
    let words: std::collections::HashSet<String> = norm_title(&lower)
        .split(' ')
        .filter(|w| w.len() >= 4)
        .map(ToString::to_string)
        .collect();
    let referenced = titles
        .iter()
        .filter(|title| title_referenced(&words, title))
        .count();
    referenced * 2 > titles.len()
}

/// A title is "referenced" by the message when one of its distinctive tokens (length ≥ 5,
/// not a stopword) appears as a whole word in the message.
fn title_referenced(message_words: &std::collections::HashSet<String>, title: &str) -> bool {
    norm_title(title)
        .split(' ')
        .filter(|w| w.len() >= 5 && !TITLE_STOPWORDS.contains(w))
        .any(|w| message_words.contains(w))
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
        // A BARE STRING dep ("n0") is a SOFT edge on that id (parity with the TS daemon's
        // parseRawDeps; the Rust parser previously dropped these silently).
        if let Some(bare) = dep.as_str().map(str::trim).filter(|v| !v.is_empty()) {
            if bare != self_id && known_ids.contains(bare) {
                out.push(PlanEdge {
                    on: bare.to_string(),
                    edge: EdgeType::Soft,
                    why: None,
                });
            }
            continue;
        }
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

/// Collapse a value to a single launch objective line, trimmed for prompt budget.
fn one_line(value: &str) -> String {
    let joined = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    truncate_chars(trimmed, 200)
}

/// The OBJECTIVE for a task's launch Objective line: the planner-supplied `goal`, else a
/// default derived from the title (or the first line of the prompt).
/// Keep launch goal lines under the backend's slash-command limit too, because legacy
/// prompts can still be recovered from `/goal` and users can forward `/goal` manually.
/// The full detail still rides along in the prompt body that follows.
pub(crate) const MAX_GOAL_LINE_CHARS: usize = 3000;

pub(crate) fn cap_goal_line(text: String) -> String {
    if text.chars().count() <= MAX_GOAL_LINE_CHARS {
        return text;
    }
    let truncated: String = text.chars().take(MAX_GOAL_LINE_CHARS - 1).collect();
    format!("{}…", truncated.trim_end())
}

/// The argument to forward for a slash command typed in the task bar. For `/goal` the
/// backend rejects an argument longer than 4000 chars ("Goal condition is limited to
/// 4000 characters"), so collapse the objective onto a SINGLE line (the slash command
/// only reads up to the first newline anyway) and cap it under MAX_GOAL_LINE_CHARS — a
/// long or pasted goal then never bounces; the full detail can ride in a normal
/// follow-up message. Every other slash command passes its trimmed argument through.
pub(crate) fn slash_command_arg(command: &str, rest: &str) -> String {
    let trimmed = rest.trim();
    if command == "/goal" {
        cap_goal_line(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        trimmed.to_string()
    }
}

fn goal_objective(task: &RudderPlanTask) -> String {
    if let Some(goal) = task
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        return cap_goal_line(one_line(goal));
    }
    let title = task.title.trim();
    if !title.is_empty() {
        return cap_goal_line(one_line(title));
    }
    let first_line = task
        .prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("complete the task");
    cap_goal_line(one_line(first_line))
}

/// The verifiable DONE-WHEN condition for a task: the planner-supplied
/// `success`, else the canonical default stopping condition.
fn goal_success(task: &RudderPlanTask) -> String {
    task.success
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(one_line)
        .map(cap_goal_line)
        .unwrap_or_else(|| DEFAULT_GOAL_SUCCESS.to_string())
}

/// Deterministic DAG preflight before approval/launch: every node gets a one-line,
/// capped objective and done-when condition. This is deliberately local, not model
/// driven, because character limits are mechanical and the transform is idempotent.
pub(crate) fn preflight_plan_task_for_launch(task: &mut RudderPlanTask) {
    task.goal = Some(goal_objective(task));
    task.success = Some(goal_success(task));
}

/// Build a worker launch prompt in the canonical objective format. Every spawned agent
/// leads with `Objective: <objective>` then a `Done when:` line so the worker has a
/// clear objective and stopping condition. Do not use `/goal` or `Goal:` here:
/// backends may parse the entire initial prompt as a goal condition.
pub(crate) fn rudder_plan_worker_prompt(
    planner_task: &str,
    task: &RudderPlanTask,
    depends_on: &str,
    _backend: Backend,
) -> String {
    // `depends_on` (when non-empty) is a pre-formatted "Depends on:" block built by the
    // caller from the live DAG + each parent's `rudder done` interfaces; it sits between
    // the worker task line and the node prompt so the worker BUILDS ON its prerequisites.
    let body = format!(
        "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nWorker node: {}\n\nOriginal request:\n{planner_task}\n\nWorker task: {}\n\n{depends_on}{}",
        task.id, task.title, task.prompt
    );
    format!(
        "Objective: {}\nDone when: {}\n\n{}",
        goal_objective(task),
        goal_success(task),
        body
    )
}

/// Wrap a manual single-task spawn (no planner) in the canonical objective format.
/// The objective is the task statement's first line; the success condition is
/// the canonical default stopping condition. Idempotent: a prompt that already has
/// a goal block is returned with any legacy `/goal` or `Goal:` prefix normalized to
/// `Objective:`.
pub(crate) fn manual_goal_prompt(task: &str) -> String {
    let trimmed = task.trim_start();
    if starts_goal_prompt(trimmed) {
        let (goal_block, rest) = split_leading_goal_block(trimmed);
        return if rest.is_empty() {
            goal_block
        } else {
            format!("{goal_block}\n\n{rest}")
        };
    }
    let objective = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(one_line)
        .filter(|line| !line.is_empty())
        .map(cap_goal_line)
        .unwrap_or_else(|| "complete the task".to_string());
    format!("Objective: {objective}\nDone when: {DEFAULT_GOAL_SUCCESS}\n\n{task}")
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

pub(crate) fn spawn_task_summary_worker(
    tx: mpsc::Sender<TaskSummaryResult>,
    run_id: String,
    task: String,
) {
    thread::spawn(move || {
        let title = generate_task_summary_title(&task);
        let _ = tx.send(TaskSummaryResult { run_id, title });
    });
}

/// Build the prompt for the completion-note BACKSTOP: a finished worker filed no `rudder
/// done` report, so a cheap one-shot summarizer reconstructs the note from the task + the
/// worker's diff. Asks for the same CompletionNote shape `rudder done` records.
pub(crate) fn build_completion_summary_prompt(task: &str, diff: &str) -> String {
    let task = normalize_task_text(task);
    let diff = if diff.trim().is_empty() {
        "(no file changes detected)"
    } else {
        diff
    };
    format!(
        "A coding agent finished the task below but did not file a structured report. From its TASK and the DIFF of what it changed, reconstruct a completion note. Return EXACTLY one JSON object and NO markdown:\n\
{{\"summary\":\"1-2 sentence plain summary of what was done\",\"interfaces\":\"key files/types/functions it added or changed\",\"followups\":[{{\"title\":\"short imperative task\",\"why\":\"why it follows\",\"scope\":\"in\"}}]}}\n\
Only include follow-ups clearly implied by the work (e.g. tests for new code, wiring a new-but-unused function, a TODO it left). If none are obvious, use an empty followups array. Use scope \"in\" for same-area work and \"out\" otherwise. Do not invent unrelated work.\n\n\
TASK:\n{task}\n\nDIFF:\n{diff}"
    )
}

/// Parse the backstop summarizer's stdout into a CompletionNote JSON value (the same
/// shape `parse_worker_done_block` yields), tolerant of surrounding prose/markdown.
pub(crate) fn completion_note_from_summary_output(output: &str) -> Option<serde_json::Value> {
    let trimmed = output.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&trimmed[start..=end]).ok()?;
    value.is_object().then_some(value)
}

/// Spawn the completion-note BACKSTOP off-thread: a one-shot haiku summarizer over the
/// worker's diff, sending the reconstructed note back over `tx`. Mirrors
/// `spawn_task_summary_worker`. ALWAYS sends a result (note `None` on any failure) so the
/// caller clears its pending flag and never re-spawns for the same run.
pub(crate) fn spawn_completion_summary_worker(
    tx: mpsc::Sender<CompletionSummaryResult>,
    run_id: String,
    node_id: String,
    task: String,
    diff: String,
) {
    thread::spawn(move || {
        let note = generate_completion_summary(&task, &diff);
        let _ = tx.send(CompletionSummaryResult {
            run_id,
            node_id,
            note,
        });
    });
}

/// Off-thread Haiku classification of a fresh task into one-off vs plan. Fire-and-forget
/// (mirrors `spawn_completion_summary_worker`): the result is sent back over `tx` and the
/// main loop routes it in `poll_dispatch_worker`. Defaults to Plan on any failure.
/// Only the real binary spawns this (begin_dispatch gates it out of tests, which inject
/// results directly), so it reads as dead code under `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn spawn_dispatch_worker(tx: mpsc::Sender<DispatchResult>, task: String) {
    thread::spawn(move || {
        let intent = classify_dispatch_intent(&task);
        let _ = tx.send(DispatchResult { task, intent });
    });
}

#[cfg(test)]
fn dispatch_classifier_timeout() -> Duration {
    Duration::from_millis(150)
}

#[cfg(not(test))]
fn dispatch_classifier_timeout() -> Duration {
    Duration::from_secs(3)
}

/// The classification prompt for the dispatcher. Asks Haiku for ONE word.
#[cfg_attr(test, allow(dead_code))]
fn build_dispatch_prompt(task: &str) -> String {
    format!(
        "You are Rudder's task-bar router. Classify the user's FRESH request into exactly one lowercase word: oneoff or plan. Output the word only.\n\nRudder has two paths:\n- oneoff: start one conversational agent in the user's MAIN checkout. Use this for questions, explanations, code reading, API/docs research, investigation, summarizing findings, or a tiny self-contained edit like a typo/log line.\n- plan: start the orchestrator, which decomposes work into a DAG of isolated workers. Use this for implementing, building, adding, creating, refactoring, migrating, integrating, wiring up, setting up, shipping, tests, docs pages, UI, multi-file, multi-step, or product work.\n\nDecision rules:\n1. If the user only wants to understand, inspect, research, or get advice, choose oneoff.\n2. If the user asks Rudder to change code or produce a deliverable, choose plan unless it is clearly a tiny isolated edit.\n3. If the request combines research with implementation, choose plan (for example: \"look into the docs and implement OAuth\").\n4. Polite question phrasing does not change implementation into advice: \"can you add X\", \"could you build X\", and \"please implement X\" are plan.\n5. \"how to add/build/implement X\" asks for an explanation, so choose oneoff. \"add/build/implement X\" asks for work, so choose plan.\n6. When unsure, choose plan.\n\nRequest:\n{task}"
    )
}

/// Run the Haiku classifier. Any failure (no `claude`, non-zero exit, unparseable) falls
/// back to Plan — the established path — so dispatch never blocks or misroutes to an
/// uncancellable one-off. A request that already leads with `/goal` is a worker prompt,
/// not a question: route it to the planner without spending a classification.
#[cfg_attr(test, allow(dead_code))]
fn classify_dispatch_intent(task: &str) -> DispatchIntent {
    let task = task.trim();
    if is_goal_slash_command(task) {
        return DispatchIntent::Plan;
    }
    if let Some(intent) = classify_dispatch_intent_locally(task) {
        return intent;
    }
    let prompt = build_dispatch_prompt(task);
    let output = run_dispatch_classifier(&prompt, dispatch_classifier_timeout());
    match output {
        Some(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
            // Strict one-token parse: anything else (incl. prose/JSON/"plan") -> Plan.
            if text == "oneoff" || text == "one-off" {
                DispatchIntent::OneOff
            } else {
                DispatchIntent::Plan
            }
        }
        _ => DispatchIntent::Plan,
    }
}

/// Fast local routing for obvious cases. This keeps common task-bar asks deterministic
/// and avoids routing simple research/questions to the planner when the Claude classifier
/// is unavailable (auth, billing, network, timeout).
pub(crate) fn classify_dispatch_intent_locally(task: &str) -> Option<DispatchIntent> {
    let task = task.trim();
    if task.is_empty() {
        return None;
    }
    let lower = task.to_ascii_lowercase();
    if is_goal_slash_command(&lower) {
        return Some(DispatchIntent::Plan);
    }

    if has_chained_plan_marker(&lower) {
        return Some(DispatchIntent::Plan);
    }
    if has_how_to_oneoff_marker(&lower) {
        return Some(DispatchIntent::OneOff);
    }
    if has_direct_plan_marker(&lower) {
        return Some(DispatchIntent::Plan);
    }
    let oneoff = has_oneoff_marker(&lower);
    if oneoff && has_plan_marker(&lower) {
        return None;
    }
    if oneoff {
        return Some(DispatchIntent::OneOff);
    }
    None
}

fn is_goal_slash_command(value: &str) -> bool {
    value == "/goal"
        || value
            .strip_prefix("/goal")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn has_direct_plan_marker(lower: &str) -> bool {
    let lower = lower
        .strip_prefix("please ")
        .or_else(|| lower.strip_prefix("pls "))
        .unwrap_or(lower);
    let starts = [
        "build ",
        "implement ",
        "create ",
        "add ",
        "refactor ",
        "rewrite ",
        "migrate ",
        "integrate ",
        "wire up ",
        "set up ",
        "setup ",
        "ship ",
    ];
    starts.iter().any(|prefix| lower.starts_with(prefix))
        || lower.contains("make a website")
        || lower.contains("make a web site")
        || lower.contains("make an app")
        || lower.contains("make a site")
        || lower.contains("web app")
        || lower.contains("landing page")
        || lower.contains("website for")
        || lower.contains("end-to-end")
        || lower.contains("multi-step")
}

fn has_plan_marker(lower: &str) -> bool {
    let patterns = [
        "build ",
        "implement ",
        "create ",
        "add ",
        "refactor ",
        "rewrite ",
        "migrate ",
        "integrate ",
        "wire up ",
        "set up ",
        "setup ",
        "ship ",
        "make a website",
        "make a web site",
        "make an app",
        "make a site",
        "web app",
        "landing page",
        "website for",
        "end-to-end",
        "multi-step",
    ];
    patterns.iter().any(|pattern| lower.contains(pattern))
}

fn has_chained_plan_marker(lower: &str) -> bool {
    let patterns = [
        " and build ",
        " and implement ",
        " and create ",
        " and add ",
        " and refactor ",
        " and rewrite ",
        " and migrate ",
        " and integrate ",
        " and wire up ",
        " and set up ",
        " and setup ",
        " and ship ",
        " then build ",
        " then implement ",
        " then create ",
        " then add ",
        " then refactor ",
        " then rewrite ",
        " then migrate ",
        " then integrate ",
        " then wire up ",
        " then set up ",
        " then setup ",
        " then ship ",
        " and then build ",
        " and then implement ",
        " and then create ",
        " and then add ",
        " and then refactor ",
        " and then rewrite ",
        " and then migrate ",
        " and then integrate ",
        " and then wire up ",
        " and then set up ",
        " and then setup ",
        " and then ship ",
        " to build ",
        " to implement ",
        " to create ",
    ];
    patterns.iter().any(|pattern| lower.contains(pattern))
}

fn has_how_to_oneoff_marker(lower: &str) -> bool {
    let starts = [
        "how to ",
        "how do i ",
        "how should i ",
        "explain how to ",
        "explain how i ",
        "tell me how to ",
        "what is the best way to ",
    ];
    starts.iter().any(|prefix| lower.starts_with(prefix))
}

fn has_oneoff_marker(lower: &str) -> bool {
    let starts = [
        "what ",
        "why ",
        "how ",
        "where ",
        "when ",
        "who ",
        "explain ",
        "summarize ",
        "describe ",
        "tell me ",
        "compare ",
        "list ",
        "look into ",
        "look up ",
        "research ",
        "read ",
        "review ",
        "inspect ",
        "investigate ",
        "check ",
        "find out ",
        "search ",
        "is ",
        "are ",
    ];
    if starts.iter().any(|prefix| lower.starts_with(prefix)) {
        return true;
    }
    let contains = ["what does", "how does"];
    contains.iter().any(|pattern| lower.contains(pattern))
}

#[cfg_attr(test, allow(dead_code))]
fn run_dispatch_classifier(prompt: &str, timeout: Duration) -> Option<std::process::Output> {
    let mut child = Command::new(claude_program())
        .args(["-p", &prompt, "--model", TASK_SUMMARY_MODEL])
        .env("CLAUDE_CODE_NO_FLICKER", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn generate_completion_summary(task: &str, diff: &str) -> Option<serde_json::Value> {
    let prompt = build_completion_summary_prompt(task, diff);
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
    completion_note_from_summary_output(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn generate_task_summary_title(task: &str) -> Option<String> {
    let task = normalize_task_text(&redact_secret_values(task));
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
    let original = normalize_task_text(&redact_secret_values(task));
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
    let normalized = redact_secret_values(value)
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

pub(crate) fn redact_secret_values(value: &str) -> String {
    value
        .split_whitespace()
        .map(redact_secret_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_secret_token(raw: &str) -> String {
    let trimmed = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    if let Some((key, value)) = trimmed.split_once('=') {
        if looks_like_secret_key(key)
            && (looks_like_secret_value(value) || looks_like_plausible_secret_value(value))
        {
            return raw.replace(value, "[redacted]");
        }
    }
    if looks_like_secret_value(trimmed) {
        return raw.replace(trimmed, "[redacted]");
    }
    raw.to_string()
}

fn looks_like_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("secret")
        || lower.contains("password")
}

fn looks_like_secret_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let has_secret_prefix = lower.contains("api_")
        || lower.starts_with("sk-")
        || lower.starts_with("xox")
        || lower.starts_with("ghp_")
        || lower.starts_with("gho_")
        || lower.starts_with("github_pat_");
    let long_mixed = value.len() >= 24
        && value.chars().any(|ch| ch.is_ascii_alphabetic())
        && value
            .chars()
            .any(|ch| ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.'));
    has_secret_prefix || long_mixed
}

fn looks_like_plausible_secret_value(value: &str) -> bool {
    value.len() >= 10
        && value.chars().any(|ch| ch.is_ascii_alphanumeric())
        && value
            .chars()
            .any(|ch| ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.' | '/'))
}
