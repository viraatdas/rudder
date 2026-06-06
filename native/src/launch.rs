#![allow(unused_imports)]
//! Building agent launch/resume commands and review-all runs.
use super::*;

// The orchestrator (decomposer) runs read-only: only inspection tools are available and
// edit/write/shell tools are blocked, so it literally cannot implement — it only decomposes
// the task into a DAG that separate worker agents run. It streams its reasoning + the DAG as
// assistant text (fast, live in the orchestrator pane), instead of real plan mode which is
// slower (extra ToolSearch/ExitPlanMode round-trip + plan-file write) and hides the plan in
// the file rather than streaming it. Comma-joined for --tools/--allowedTools/--disallowedTools.
const CLAUDE_DECOMPOSER_TOOLS: &str = "Read,Grep,Glob,LS,WebSearch,WebFetch";
const CLAUDE_DECOMPOSER_DISALLOWED: &str = "Edit,Write,MultiEdit,NotebookEdit,Bash";
// The standalone /plan FRONT-END pre-approves the read tools PLUS Bash so it can investigate
// without prompts; --permission-mode plan still blocks edits.
const CLAUDE_PLAN_FRONTEND_TOOLS: &str = "Read,Grep,Glob,LS,WebSearch,WebFetch,Bash";
const ORCHESTRATOR_SKILLS_DIR: &str = ".claude/skills";

pub(crate) fn mint_session_id_for(backend: Backend) -> Option<String> {
    match backend {
        Backend::Claude => Some(uuid::Uuid::new_v4().to_string()),
        Backend::Codex => None,
    }
}

pub(crate) fn ensure_orchestrator_skills(repo_root: &Path) -> Result<()> {
    ensure_gitignore_contains(repo_root, ".claude/skills/rudder-*/")?;
    let base = repo_root.join(ORCHESTRATOR_SKILLS_DIR);
    fs::create_dir_all(&base)?;
    for skill in ORCHESTRATOR_SKILLS {
        let dir = base.join(skill.slug);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("SKILL.md"), skill.render().as_bytes())?;
    }
    Ok(())
}

struct OrchestratorSkill {
    slug: &'static str,
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

impl OrchestratorSkill {
    fn render(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            self.name, self.description, self.body
        )
    }
}

const ORCHESTRATOR_SKILLS: &[OrchestratorSkill] = &[
    OrchestratorSkill {
        slug: "rudder-edit-dag",
        name: "rudder-edit-dag",
        description: "Use when maintaining Rudder's task DAG in RUDDER.md or approving the plan.",
        body: "Edit RUDDER.md outside the generated block. Keep exactly one full RUDDER_PLAN_TASKS_START / RUDDER_PLAN_TASKS_END block containing the complete task DAG. When the user explicitly approves the plan, write RUDDER_APPROVE_PLAN on its own line in RUDDER.md while keeping the plan block.",
    },
    OrchestratorSkill {
        slug: "rudder-model",
        name: "rudder-model",
        description: "Use when the user asks to change the default Claude or Codex model or effort.",
        body: "Write one control line to RUDDER.md: RUDDER_MODEL <provider> <model> [effort]. Provider is claude or codex. Effort is optional and may be low, medium, high, xhigh, or max. Rudder consumes the marker and updates the dashboard defaults.",
    },
    OrchestratorSkill {
        slug: "rudder-main",
        name: "rudder-main",
        description: "Use when the user wants a main-checkout agent instead of a DAG worker.",
        body: "Write one control line to RUDDER.md: RUDDER_MAIN <optional prompt>. Leave the prompt empty to start the default main agent bootstrap. Rudder consumes the marker and starts a main-checkout agent.",
    },
    OrchestratorSkill {
        slug: "rudder-goal",
        name: "rudder-goal",
        description: "Use when the user asks to set or forward a /goal to the selected agent.",
        body: "Write one control line to RUDDER.md: RUDDER_GOAL <goal text>. Rudder consumes the marker and forwards /goal to the focused agent.",
    },
    OrchestratorSkill {
        slug: "rudder-usage",
        name: "rudder-usage",
        description: "Use when the user asks for Rudder session token usage or cost.",
        body: "Write RUDDER_USAGE on its own line in RUDDER.md. Rudder consumes the marker and shows the usage summary in the dashboard notice.",
    },
    OrchestratorSkill {
        slug: "rudder-cloud",
        name: "rudder-cloud",
        description: "Use when the user asks for Rudder Cloud login, list, onload, byoc, or other cloud actions.",
        body: "For login, write RUDDER_LOGIN on its own line in RUDDER.md. For other cloud actions, write RUDDER_CLOUD <args> on one line, for example RUDDER_CLOUD list or RUDDER_CLOUD onload. Rudder consumes the marker and starts the corresponding Rudder Cloud command pane.",
    },
    OrchestratorSkill {
        slug: "rudder-review-merge",
        name: "rudder-review-merge",
        description: "Use when the user asks to review all work, merge all ready work, or toggle auto-merge.",
        body: "Write exactly one control line to RUDDER.md: RUDDER_REVIEW_ALL to start a review-all agent, RUDDER_MERGE_ALL to merge all ready work, or RUDDER_AUTOMERGE on|off|toggle to control automatic clean merges.",
    },
    OrchestratorSkill {
        slug: "rudder-plan-ask",
        name: "rudder-plan-ask",
        description: "Use when the user explicitly asks to force a DAG plan or force a one-off ask.",
        body: "Write RUDDER_PLAN <task> to force a DAG planner for the task, or RUDDER_ASK <question or small change> to start a one-off agent in the main checkout. Rudder consumes the marker and runs the matching action.",
    },
    OrchestratorSkill {
        slug: "rudder-help",
        name: "rudder-help",
        description: "Use when the user asks what Rudder dashboard commands or orchestrator skills are available.",
        body: "Write RUDDER_HELP on its own line in RUDDER.md. Rudder consumes the marker and shows a short dashboard command hint.",
    },
];

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
        AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff => {
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
            // Resume the same decomposer session with the slim feedback. Same read-only
            // streaming profile as the first turn, so plan_stream keeps rendering live and
            // the revised RUDDER_PLAN_TASKS block parses the same way.
            let mut args = vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--include-partial-messages".to_string(),
                "--verbose".to_string(),
                "--permission-mode".to_string(),
                "default".to_string(),
                "--tools".to_string(),
                CLAUDE_DECOMPOSER_TOOLS.to_string(),
                "--allowedTools".to_string(),
                CLAUDE_DECOMPOSER_TOOLS.to_string(),
                "--disallowedTools".to_string(),
                CLAUDE_DECOMPOSER_DISALLOWED.to_string(),
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
            args.push(feedback_prompt.to_string());
            TerminalCommand::with_args(claude_program(), args)
                .with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
            // `codex exec resume [OPTIONS] <SESSION_ID> [PROMPT]`. No --sandbox here:
            // it inherits the read-only sandbox from the resumed session.
            let mut args = vec![
                "exec".to_string(),
                "resume".to_string(),
                "--json".to_string(),
            ];
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
    agent_command_with_orchestrator_mode(
        backend,
        model,
        effort,
        task,
        mode,
        session_id,
        interactive_orchestrator(),
    )
}

pub(crate) fn agent_command_with_orchestrator_mode(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    task: &str,
    mode: AgentMode,
    session_id: Option<&str>,
    orchestrator_interactive: bool,
) -> TerminalCommand {
    let prompt = match mode {
        AgentMode::Execute => Some(execution_prompt(task)),
        AgentMode::Plan => Some(plan_prompt(task)),
        AgentMode::RudderPlan if orchestrator_interactive && backend == Backend::Claude => {
            Some(rudder_orchestrator_prompt(task))
        }
        AgentMode::RudderPlan => Some(rudder_plan_prompt(task)),
        AgentMode::ReviewAll => Some(task.to_string()),
        AgentMode::Main => {
            if task.trim().is_empty() {
                None
            } else {
                Some(execution_prompt(task))
            }
        }
        AgentMode::OneOff => Some(oneoff_prompt(task)),
    };
    match backend {
        Backend::Claude => {
            let mut args = match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff => {
                    vec![
                        "--permission-mode".to_string(),
                        "bypassPermissions".to_string(),
                    ]
                }
                // The orchestrator is a read-only DECOMPOSER, not a Claude plan-mode
                // session. Plan mode was tried (1.16.0) and reverted: it is SLOWER (an
                // extra ToolSearch/ExitPlanMode round-trip + a plan-file write) and HIDES
                // the plan (written to the plan file, not streamed), so the pane showed
                // only tool activity with no live thinking. The decomposer instead runs
                // read-only by tool allowlist (inspection only; Edit/Write/Bash blocked),
                // permission-mode default so reads auto-run, and STREAMS its reasoning +
                // the RUDDER_PLAN_TASKS block as assistant text — fast and visible live.
                // INTERACTIVE orchestrator (opt-in): a normal Claude Code PTY the user
                // converses with, with the orchestrator system prompt. It writes its DAG to
                // the orchestrator plan file (which Rudder renders above), so no -p/stream.
                AgentMode::RudderPlan if orchestrator_interactive => vec![
                    "--permission-mode".to_string(),
                    "default".to_string(),
                    // Auto-approve read tools + Edit/Write (the prompt restricts writes to
                    // the orchestrator plan file) so planning never stalls on a prompt.
                    "--allowedTools".to_string(),
                    "Read,Grep,Glob,LS,WebSearch,WebFetch,Bash,Edit,Write".to_string(),
                    "--append-system-prompt".to_string(),
                    orchestrator_system_prompt(),
                    "--name".to_string(),
                    "rudder-orchestrator".to_string(),
                ],
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
                    "--permission-mode".to_string(),
                    "default".to_string(),
                    "--tools".to_string(),
                    CLAUDE_DECOMPOSER_TOOLS.to_string(),
                    "--allowedTools".to_string(),
                    CLAUDE_DECOMPOSER_TOOLS.to_string(),
                    "--disallowedTools".to_string(),
                    CLAUDE_DECOMPOSER_DISALLOWED.to_string(),
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
            if let Some(prompt) = prompt {
                args.push(prompt);
            }
            TerminalCommand::with_args(claude_program(), args)
                .with_env("CLAUDE_CODE_NO_FLICKER", "0")
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
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff => {
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

pub(crate) fn push_codex_rudder_config_overrides(
    args: &mut Vec<String>,
    effort: Option<EffortLevel>,
) {
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

/// The Claude orchestrator runs as a normal INTERACTIVE Claude Code PTY the user converses
/// with (real plan-mode feel + visible thinking), writing its DAG to the orchestrator plan
/// file and self-launching on the `RUDDER_APPROVE_PLAN` marker. This is now the DEFAULT
/// ("the main plan mode"); set `RUDDER_INTERACTIVE_ORCHESTRATOR=0` to opt back into the
/// headless `claude -p` decomposer. (Codex orchestrators stay headless regardless.)
pub(crate) fn interactive_orchestrator() -> bool {
    env::var("RUDDER_INTERACTIVE_ORCHESTRATOR")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
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
