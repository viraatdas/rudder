#![allow(unused_imports)]
//! Building agent launch/resume commands and review-all runs.
use super::*;

// The Claude planner now runs in REAL `--permission-mode plan` (see agent_command /
// rudder_plan_refine_command): plan mode is read-only by construction (no repo edits) and
// gives us the official ExitPlanMode signal, so the old explicit decomposer tool allow/deny
// lists are gone. Reads are auto-approved via --allowedTools below.
// The plan-mode tool set pre-approves read tools PLUS Bash, so the
// model can investigate (find/ls/grep/git) without permission prompts. Per the Claude CLI
// reference, --allowedTools lists "tools that execute without prompting"; a bare tool name
// auto-approves all its invocations. --permission-mode plan still blocks edits, so the
// front-end stays research-only and never implements (Rudder, not the model, approves and
// then STOPS it). Bash stays OUT of the decomposer's set (that one is strictly read-only).
const CLAUDE_PLAN_FRONTEND_TOOLS: &str = "Read,Grep,Glob,LS,WebSearch,WebFetch,Bash";

// Appended to the RudderPlan prompt for Claude (which now runs in real `--permission-mode
// plan`). The plan is presented to the host via the ExitPlanMode tool, so the model must
// put the machine-readable DAG inside that plan. Captured by plan_stream's exit_plan.
const PLAN_MODE_EXIT_INSTRUCTION: &str = "\n\nYou are running in plan mode. When (and ONLY when) the DAG is ready to present, write your plan so that it CONTAINS the RUDDER_PLAN_TASKS_START..RUDDER_PLAN_TASKS_END block verbatim, then call the ExitPlanMode tool to present that plan. Do NOT call ExitPlanMode on a turn where you are still asking clarifying questions — emit the RUDDER_QUESTIONS block and stop instead.";

pub(crate) fn mint_session_id_for(backend: Backend) -> Option<String> {
    match backend {
        Backend::Claude => Some(uuid::Uuid::new_v4().to_string()),
        Backend::Codex => None,
    }
}

pub(crate) fn can_resume_agent(run: &AgentRun) -> bool {
    match run.backend {
        Backend::Claude | Backend::Codex => run.session_id.is_some(),
    }
}

pub(crate) fn claude_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args: Vec<String> = vec![
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ];
    if !run.model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(run.model.clone());
    }
    if let Some(effort) = run.effort {
        args.push("--effort".to_string());
        args.push(effort.as_str().to_string());
    }
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    TerminalCommand::with_args(claude_program(), args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
}

pub(crate) fn codex_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args = vec!["--no-alt-screen".to_string()];
    args.push("--enable".to_string());
    args.push("goals".to_string());
    match run.mode {
        AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
        AgentMode::Plan | AgentMode::RudderPlan => {
            args.push("--sandbox".to_string());
            args.push("read-only".to_string());
            args.push("--ask-for-approval".to_string());
            args.push("never".to_string());
            args.push("--search".to_string());
        }
    }
    push_codex_rudder_config_overrides(&mut args, run.effort);
    if !run.model.trim().is_empty() {
        args.push("-m".to_string());
        args.push(run.model.clone());
    }
    args.push("resume".to_string());
    args.push(session_id.to_string());
    TerminalCommand::with_args(codex_program(), args).with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
}

/// Build the REFINE follow-up command for the orchestrator: RESUME the planner's
/// existing session so it remembers the prior plan + reasoning, and send only the
/// slim feedback prompt (the system prompt is already in the session). Claude:
/// `--resume <sid>`; Codex: `exec resume <sid>`. Same JSON streaming + read-only
/// tool allowlist as the first turn, so plan_stream.rs keeps rendering the live
/// transcript and the revised RUDDER_PLAN_TASKS block parses the same way.
pub(crate) fn rudder_plan_refine_command(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    feedback_prompt: &str,
    session_id: &str,
) -> TerminalCommand {
    match backend {
        Backend::Claude => {
            // Resume the same plan-mode session so the refine re-presents a revised plan
            // via ExitPlanMode (captured the same way). Mirrors the first-turn args.
            let mut args = vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--include-partial-messages".to_string(),
                "--verbose".to_string(),
                "--permission-mode".to_string(),
                "plan".to_string(),
                "--allowedTools".to_string(),
                CLAUDE_PLAN_FRONTEND_TOOLS.to_string(),
            ];
            if !model.trim().is_empty() {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(effort) = effort {
                args.push("--effort".to_string());
                args.push(effort.as_str().to_string());
            }
            args.push("--resume".to_string());
            args.push(session_id.to_string());
            args.push(format!("{feedback_prompt}{PLAN_MODE_EXIT_INSTRUCTION}"));
            TerminalCommand::with_args(claude_program(), args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
            // `codex exec resume [OPTIONS] <SESSION_ID> [PROMPT]`. No --sandbox here:
            // it inherits the read-only sandbox from the resumed session.
            let mut args = vec!["exec".to_string(), "resume".to_string(), "--json".to_string()];
            push_codex_rudder_config_overrides(&mut args, effort);
            if !model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            args.push(session_id.to_string());
            args.push(feedback_prompt.to_string());
            TerminalCommand::with_args(codex_program(), args)
                .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
        }
    }
}

pub(crate) fn agent_command(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    task: &str,
    mode: AgentMode,
    session_id: Option<&str>,
) -> TerminalCommand {
    let prompt = match mode {
        AgentMode::Execute => Some(execution_prompt(task)),
        AgentMode::Plan => Some(plan_prompt(task)),
        AgentMode::RudderPlan => Some(rudder_plan_prompt(task)),
        AgentMode::ReviewAll => Some(task.to_string()),
        AgentMode::Main => {
            if task.trim().is_empty() {
                None
            } else {
                Some(execution_prompt(task))
            }
        }
    };
    match backend {
        Backend::Claude => {
            let mut args = match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => vec![
                    "--permission-mode".to_string(),
                    "bypassPermissions".to_string(),
                ],
                // The orchestrator is a DECOMPOSER, not a Claude plan-mode session.
                // Claude plan mode frames as "approve my plan so I implement it",
                // which reads like a single agent. Instead it runs read-only by
                // tool allowlist (only inspection tools; Edit/Write/Bash blocked),
                // permission-mode default so the allowed read tools auto-run with no
                // prompts and no plan-mode approve-to-implement UI. It cannot
                // implement; it only inspects and emits the task DAG, and Rudder
                // spawns the separate worker agents.
                AgentMode::RudderPlan => vec![
                    // Print mode: run non-interactively so Rudder parses the plan and
                    // the process exits. NOT the interactive Claude Code TUI.
                    "-p".to_string(),
                    // Stream the event log as JSONL so the orchestrator pane can show
                    // the model thinking + inspecting files + writing the plan live
                    // (plain -p hides thinking, leaving a silent gap). plan_stream.rs
                    // parses these events into a transcript and reconstructs the text.
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--include-partial-messages".to_string(),
                    "--verbose".to_string(),
                    // REAL plan mode: read-only by construction (plan mode blocks repo
                    // edits), and it gives us the official `ExitPlanMode` signal — when the
                    // plan is ready the model calls ExitPlanMode and plan_stream captures
                    // its `input.plan` (the authoritative RUDDER_PLAN_TASKS source). In
                    // headless -p ExitPlanMode is auto-denied, so the planner can NEVER
                    // implement; Rudder owns the DAG and launches the separate workers.
                    // No --tools/--disallowedTools: those would block the plan-file Write
                    // and exclude ExitPlanMode; --allowedTools just auto-approves reads.
                    "--permission-mode".to_string(),
                    "plan".to_string(),
                    "--allowedTools".to_string(),
                    CLAUDE_PLAN_FRONTEND_TOOLS.to_string(),
                ],
                // The standalone `/plan` command: a read-only Claude plan-mode session,
                // with the research tools pre-approved so it never stops on a prompt.
                AgentMode::Plan => vec![
                    "--permission-mode".to_string(),
                    "plan".to_string(),
                    "--allowedTools".to_string(),
                    CLAUDE_PLAN_FRONTEND_TOOLS.to_string(),
                    "--name".to_string(),
                    format!("plan:{}", short_task(task)),
                ],
            };
            if !model.trim().is_empty() {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(effort) = effort {
                args.push("--effort".to_string());
                args.push(effort.as_str().to_string());
            }
            // Pass the session id for every mode including RudderPlan: the planner
            // now persists a resumable session so a refinement can `--resume` it as
            // a follow-up turn (the model remembers the prior plan) instead of
            // re-planning from scratch. See rudder_plan_refine_command.
            if let Some(sid) = session_id {
                args.push("--session-id".to_string());
                args.push(sid.to_string());
            }
            if let Some(mut prompt) = prompt {
                // In plan mode the planner presents its plan via the ExitPlanMode tool,
                // so tell it to embed the RUDDER_PLAN_TASKS block in that plan and call
                // ExitPlanMode when (and only when) the DAG is ready. The questions-first
                // gate still applies: on the first turn it asks (RUDDER_QUESTIONS) and
                // STOPS without ExitPlanMode (the rudder_plan_prompt already says so).
                if mode == AgentMode::RudderPlan {
                    prompt.push_str(PLAN_MODE_EXIT_INSTRUCTION);
                }
                args.push(prompt);
            }
            TerminalCommand::with_args(claude_program(), args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
            // The orchestrator runs non-interactively via `codex exec` (read-only):
            // it prints the decomposition + DAG and exits, so Rudder parses it.
            // Other modes use the interactive codex TUI.
            if mode == AgentMode::RudderPlan {
                let mut args = vec![
                    "exec".to_string(),
                    // Emit events as JSONL so plan_stream.rs can render a live
                    // transcript and capture the thread id (for `exec resume`).
                    "--json".to_string(),
                    "--sandbox".to_string(),
                    "read-only".to_string(),
                ];
                push_codex_rudder_config_overrides(&mut args, effort);
                if !model.trim().is_empty() {
                    args.push("-m".to_string());
                    args.push(model.to_string());
                }
                if let Some(prompt) = prompt {
                    args.push(prompt);
                }
                return TerminalCommand::with_args(codex_program(), args)
                    .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1");
            }
            let mut args = vec!["--no-alt-screen".to_string()];
            args.push("--enable".to_string());
            args.push("goals".to_string());
            match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => {
                    args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                }
                AgentMode::Plan | AgentMode::RudderPlan => {
                    args.push("--sandbox".to_string());
                    args.push("read-only".to_string());
                    args.push("--ask-for-approval".to_string());
                    args.push("never".to_string());
                    args.push("--search".to_string());
                }
            }
            push_codex_rudder_config_overrides(&mut args, effort);
            if !model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            // Every mode gets its prompt as an arg.
            {
                if let Some(prompt) = prompt {
                    args.push(prompt);
                }
            }
            TerminalCommand::with_args(codex_program(), args)
                .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
        }
    }
}

pub(crate) fn push_codex_rudder_config_overrides(args: &mut Vec<String>, effort: Option<EffortLevel>) {
    // Rudder workers run Codex as a child process, so do not inherit desktop-app
    // notification hooks that expect the official signed app launch chain.
    args.push("-c".to_string());
    args.push("notify=[]".to_string());
    args.push("-c".to_string());
    args.push("features.plugins=false".to_string());
    args.push("-c".to_string());
    args.push("features.computer_use=false".to_string());
    args.push("-c".to_string());
    args.push("model_reasoning_summary=\"detailed\"".to_string());
    args.push("-c".to_string());
    args.push("model_supports_reasoning_summaries=true".to_string());
    if let Some(effort) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
    }
}

pub(crate) fn codex_program() -> String {
    env::var("RUDDER_CODEX_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

/// The Claude executable. Overridable via RUDDER_CLAUDE_BIN so the end-to-end TUI
/// harness can inject a fake `claude` (deterministic stream-json, no auth/network),
/// mirroring RUDDER_CODEX_BIN.
pub(crate) fn claude_program() -> String {
    env::var("RUDDER_CLAUDE_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "claude".to_string())
}

pub(crate) fn review_all_run(
    worktree: WorktreeInfo,
    prompt: String,
    sources: Vec<ReviewAllSource>,
    session_id: Option<String>,
) -> AgentRun {
    let created_at = now_stamp();
    let source_ids = sources
        .iter()
        .map(|source| source.id.clone())
        .collect::<Vec<_>>();
    AgentRun {
        id: worktree.id,
        created_at: created_at.clone(),
        mode: AgentMode::ReviewAll,
        task: prompt.clone(),
        task_summary: format!("review all {} worktrees", source_ids.len()),
        current_prompt: prompt.clone(),
        turns: vec![AgentTurn {
            ts: created_at.clone(),
            prompt,
            source: "user".to_string(),
        }],
        last_user_input_at: created_at,
        backend: Backend::Codex,
        model: REVIEW_ALL_MODEL.to_string(),
        effort: Some(REVIEW_ALL_EFFORT),
        status: AgentStatus::Running,
        cwd: worktree.path.clone(),
        worktree_branch: worktree.branch.clone(),
        worktree_path: worktree.path_is_worktree.then_some(worktree.path),
        workspace_name: worktree.workspace_name.clone(),
        jj_change_id: worktree.jj_change_id.clone(),
        session_id,
        terminal: None,
        terminal_size: None,
        review_terminal: None,
        review_size: None,
        review_error: None,
        last_output_at: Instant::now(),
        completed_at: None,
        autosteered: false,
        needs_permission: false,
        permission_notified: false,
        needs_user_input: false,
        user_input_notified: false,
        last_error: None,
        worker_input_draft: String::new(),
        worker_input_cursor: 0,
        worker_input_is_prompt: false,
        last_drain_at: None,
        review_source_ids: source_ids,
        deps: Vec::new(),
        soft_deps: Vec::new(),
        node_id: None,
        reconcile_planner: false,
        plan_stream: None,
        last_worker_input_at: None,
        ready_since: None,
        merge_resolver: false,
    }
}

pub(crate) fn review_all_prompt(
    target_ref: &str,
    worktree: &WorktreeInfo,
    sources: &[ReviewAllSource],
    premerge: &ReviewAllPremerge,
) -> String {
    let aggregate_branch = worktree.branch.as_deref().unwrap_or("current branch");
    let source_lines = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let path = source
                .worktree_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unknown worktree path)".to_string());
            format!(
                "{}. {} ({})\n   branch: {}\n   worktree: {}\n   task: {}",
                index + 1,
                source.summary,
                source.id,
                source.branch,
                path,
                truncate_chars(&source.task, 220)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let merged = if premerge.merged_branches.is_empty() {
        "- none yet".to_string()
    } else {
        premerge
            .merged_branches
            .iter()
            .map(|branch| format!("- {branch}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let remaining = if premerge.remaining_branches.is_empty() {
        "- none".to_string()
    } else {
        premerge
            .remaining_branches
            .iter()
            .map(|branch| format!("- {branch}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stopped = match (
        premerge.stopped_branch.as_deref(),
        premerge.stopped_error.as_deref(),
    ) {
        (Some(branch), Some(error)) => {
            format!("Rudder stopped while merging `{branch}`:\n{error}")
        }
        _ => "No merge conflict was detected while building the aggregate branch.".to_string(),
    };

    format!(
        "/review Review the combined Rudder agent worktree changes against `{target_ref}`.\n\
\n\
You are the Rudder review-all integration agent. You are running on an aggregate worktree branch that is meant to become one reviewed merge back into main.\n\
\n\
Aggregate worktree\n\
- path: {path}\n\
- branch: {aggregate_branch}\n\
- target/base ref: {target_ref}\n\
\n\
Source worktrees included in this review\n\
{source_lines}\n\
\n\
Pre-merge state\n\
Already merged into this aggregate branch:\n\
{merged}\n\
\n\
Still not fully merged:\n\
{remaining}\n\
\n\
{stopped}\n\
\n\
Instructions\n\
1. Run `git status` first. If a merge is in progress, resolve the conflicts, `git add` the resolutions, and commit the merge.\n\
2. Merge every branch listed under \"Still not fully merged\" into this aggregate branch, in the listed order. Resolve conflicts carefully.\n\
3. Run the Codex `/review` flow on the combined diff against `{target_ref}`. If the slash command is unavailable, perform an equivalent code review using `git diff {target_ref}...HEAD`.\n\
4. Fix real review findings directly in this aggregate worktree. Do not edit the original source worktrees.\n\
5. Run the relevant tests/checks for the files touched. If a check cannot run, say exactly why.\n\
6. Do not check out `{target_ref}` and do not merge into `{target_ref}` yourself. When the aggregate branch is ready, stop and say: `Rudder review-all branch is ready; press m on this row to merge to main.`\n",
        target_ref = target_ref,
        path = worktree.path.display(),
        aggregate_branch = aggregate_branch,
        source_lines = source_lines,
        merged = merged,
        remaining = remaining,
        stopped = stopped,
    )
}
