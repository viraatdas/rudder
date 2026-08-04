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
        // Codex and opencode mint their own session ids and offer no way to
        // pre-declare one; Rudder discovers theirs after the fact.
        Backend::Codex | Backend::Opencode => None,
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
        body: "Before approval, edit RUDDER.md outside the generated block. Keep exactly one full RUDDER_PLAN_TASKS_START / RUDDER_PLAN_TASKS_END block containing the complete task DAG. When the user explicitly approves the plan, write RUDDER_APPROVE_PLAN on its own line in RUDDER.md while keeping the plan block. After approval, do not rewrite the plan block to control running work; use RUDDER_ADD_TASK or RUDDER_REPLAN markers instead.",
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
        description: "Use when the user asks for Rudder Cloud login, list, migration of current agents, onload, byoc, or other cloud actions.",
        body: "Read RUDDER.md first. When the user asks to take, move, migrate, or run the CURRENT/ACTIVE AGENTS on Rudder Cloud, write RUDDER_CLOUD_MIGRATE on its own line. That action freezes every live isolated worker, snapshots its workspace/session/project environment, and resumes the fleet in one cloud workspace; do not use onload for this because onload transfers only one selected run. For login, write RUDDER_LOGIN. For other cloud actions, write RUDDER_CLOUD <args>, for example RUDDER_CLOUD list. Rudder consumes the marker and starts the corresponding Rudder Cloud command pane.",
    },
    OrchestratorSkill {
        slug: "rudder-monitor-work",
        name: "rudder-monitor-work",
        description: "Use when the user asks to monitor current Claude, Codex, or Rudder jobs and then take follow-up actions when they finish.",
        body: "Read RUDDER.md's generated Global job snapshot plus Active, Ready, and Completed local Rudder agent sections first. Active means live or waiting, Ready means completed work awaiting review or merge, and Completed is terminal history. If relevant jobs are still running or waiting, report a concise status grounded in those rows and keep monitoring by re-reading RUDDER.md; do not invent completion. Planned DAG nodes integrate automatically after completion; use RUDDER_REVIEW_ALL for a whole-repo review, RUDDER_MERGE_ALL for manually started ready work, and RUDDER_ADD_TASK or RUDDER_RUN for fixes found by review. For repo-specific final steps such as push, publish, release, or deploy, start a main-checkout worker with RUDDER_MAIN <prompt> that names the exact final action. Do not run final release commands directly from the orchestrator.",
    },
    OrchestratorSkill {
        slug: "rudder-review-merge",
        name: "rudder-review-merge",
        description: "Use when the user asks to review all work or merge ready manually started work.",
        body: "Write exactly one control line to RUDDER.md: RUDDER_REVIEW_ALL to start a review-all agent, RUDDER_MERGE_ALL to ask the dashboard to merge all ready work, or RUDDER_MERGE <node-or-run-id> to merge one worker. Planned DAG nodes integrate automatically.",
    },
    OrchestratorSkill {
        slug: "rudder-plan-ask",
        name: "rudder-plan-ask",
        description: "Use when the user explicitly asks to force a DAG plan, add work to the running DAG, re-plan the running DAG, force one isolated worker, or run something in the shared main checkout.",
        body: "Write RUDDER_ADD_TASK <task> to append one new task to the running DAG, RUDDER_REPLAN <direction> to structurally revise the running DAG, RUDDER_PLAN <task> to force a fresh DAG planner, RUDDER_RUN <task> to start exactly one isolated mergeable worker with no DAG (this is also what plain user input does), or RUDDER_MAIN <prompt> to start an agent in the shared main checkout, which edits the user's tree directly and has nothing to merge. Rudder consumes the marker and runs the matching action.",
    },
    OrchestratorSkill {
        slug: "rudder-worker-control",
        name: "rudder-worker-control",
        description: "Use when the user asks to control, pause, resume, re-goal, or change the model of a worker.",
        body: "Read RUDDER.md first and resolve phrases such as 'current task' or 'all running tasks' to the live node ids. Use node ids such as n0/n1 when available; otherwise use run ids. Write one or more control lines to RUDDER.md: RUDDER_STOP <node-or-run-id> pauses a worker while preserving its workspace; RUDDER_RESUME <node-or-run-id> <claude|codex> <model> [low|medium|high|xhigh|max|auto] [new direction] resumes it in the same workspace on that model; RUDDER_REGOAL <node-or-run-id> <new goal> resumes it without changing model; RUDDER_INJECT <node-or-run-id> <message> sends a note into its live terminal; RUDDER_MERGE <node-or-run-id> merges one completed worker. For 'pause and resume on model X', write STOP then RESUME for every matching live worker. Never target the orchestrator with these markers.",
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
        Backend::Claude | Backend::Codex | Backend::Opencode => run.session_id.is_some(),
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

/// BRANCH a Claude chat: resume the source session but `--fork-session` so the
/// continuation gets a NEW session id and the original conversation is left
/// exactly where it was. The forked pane opens interactive with the full prior
/// context; the user types the new direction as its next turn — or `prompt`
/// supplies it (a handoff carries the next step with it).
pub(crate) fn claude_fork_command(
    model: &str,
    effort: Option<EffortLevel>,
    session_id: &str,
    prompt: Option<&str>,
) -> TerminalCommand {
    let mut args: Vec<String> = vec![
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
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
    args.push("--fork-session".to_string());
    // Claude takes the first turn as a positional prompt and still opens interactive.
    if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
        args.push(prompt.to_string());
    }
    TerminalCommand::with_args(claude_program(), args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
}

/// BRANCH a Codex chat: `codex fork <session-id>` copies the conversation into a
/// brand-new session (the original stays untouched) and opens it interactive.
/// Same worker flags as `codex_resume_command`'s Execute arm — a branched run is
/// always a worker, never a planner.
pub(crate) fn codex_fork_command(
    model: &str,
    effort: Option<EffortLevel>,
    session_id: &str,
    prompt: Option<&str>,
) -> TerminalCommand {
    let mut args = vec![
        "--no-alt-screen".to_string(),
        "--enable".to_string(),
        "goals".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
    ];
    push_codex_resume_cwd_current(&mut args);
    push_codex_worker_config_overrides(&mut args, effort);
    if !model.trim().is_empty() {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    args.push("fork".to_string());
    args.push(session_id.to_string());
    // `codex fork [SESSION_ID] [PROMPT]` — the optional prompt starts the session.
    if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
        args.push(prompt.to_string());
    }
    TerminalCommand::with_args(codex_program(), args).with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
}

pub(crate) fn codex_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args = Vec::new();
    push_codex_resume_cwd_current(&mut args);
    match run.mode {
        AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff => {
            args.push("--no-alt-screen".to_string());
            args.push("--enable".to_string());
            args.push("goals".to_string());
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
            push_codex_worker_config_overrides(&mut args, run.effort);
        }
        AgentMode::RudderPlan if run.interactive_orchestrator => {
            // A live conductor must be able to update RUDDER.md/RUDDER_SHARED.md.
            // The prompt contract forbids product-file edits.
            push_codex_interactive_orchestrator_args(&mut args, run.effort);
        }
        AgentMode::Plan | AgentMode::RudderPlan => {
            args.push("--no-alt-screen".to_string());
            args.push("--enable".to_string());
            args.push("goals".to_string());
            args.push("--sandbox".to_string());
            args.push("read-only".to_string());
            args.push("--ask-for-approval".to_string());
            args.push("never".to_string());
            args.push("--search".to_string());
            push_codex_planner_config_overrides(&mut args, run.effort);
        }
    }
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
            push_codex_planner_config_overrides(&mut args, effort);
            if !model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            args.push(session_id.to_string());
            args.push(feedback_prompt.to_string());
            TerminalCommand::with_args(codex_program(), args)
                .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
        }
        // opencode resumes the planner session in its TUI and takes the feedback as
        // the next turn; there is no headless streaming mode to parse, so the
        // revised plan block is read out of the pane exactly like the first turn.
        Backend::Opencode => {
            let mut args = vec!["--session".to_string(), session_id.to_string()];
            push_opencode_model(&mut args, model);
            if !feedback_prompt.trim().is_empty() {
                args.push("--prompt".to_string());
                args.push(feedback_prompt.to_string());
            }
            TerminalCommand::with_args(opencode_program(), args)
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
        AgentMode::RudderPlan if orchestrator_interactive && backend == Backend::Codex => {
            Some(codex_orchestrator_prompt(task))
        }
        AgentMode::RudderPlan => Some(rudder_plan_prompt(task)),
        AgentMode::ReviewAll => Some(task.to_string()),
        AgentMode::Main => {
            if task.trim().is_empty() {
                None
            } else {
                Some(main_task_prompt(task))
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
            // Interactive orchestrators use the normal Codex TUI and write their plan
            // into RUDDER.md, just like the Claude conductor. Headless helper planners
            // still use `codex exec --json` below so Rudder can parse their output.
            if mode == AgentMode::RudderPlan && orchestrator_interactive {
                let mut args = Vec::new();
                push_codex_interactive_orchestrator_args(&mut args, effort);
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
            if mode == AgentMode::RudderPlan {
                let mut args = vec![
                    "exec".to_string(),
                    // Emit events as JSONL so plan_stream.rs can render a live
                    // transcript and capture the thread id (for `exec resume`).
                    "--json".to_string(),
                    "--sandbox".to_string(),
                    "read-only".to_string(),
                ];
                push_codex_planner_config_overrides(&mut args, effort);
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
            match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff => {
                    push_codex_worker_config_overrides(&mut args, effort);
                }
                AgentMode::Plan | AgentMode::RudderPlan => {
                    push_codex_planner_config_overrides(&mut args, effort);
                }
            }
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
        Backend::Opencode => opencode_command(model, prompt.as_deref(), mode),
    }
}

/// Every opencode launch has the same shape: the interactive TUI, optionally
/// auto-approving, with the first turn passed as `--prompt`.
///
/// opencode has no headless streaming mode Rudder can parse (no `claude -p
/// --output-format stream-json`, no `codex exec --json`), so planner modes run the
/// same TUI and Rudder reads their RUDDER_PLAN_TASKS block out of the pane text —
/// the same path the interactive orchestrator already uses. Planners are launched
/// WITHOUT `--auto` so an unexpected edit stops for approval rather than landing.
pub(crate) fn opencode_command(
    model: &str,
    prompt: Option<&str>,
    mode: AgentMode,
) -> TerminalCommand {
    let mut args: Vec<String> = Vec::new();
    if matches!(
        mode,
        AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main | AgentMode::OneOff
    ) {
        args.push("--auto".to_string());
    }
    push_opencode_model(&mut args, model);
    if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--prompt".to_string());
        args.push(prompt.to_string());
    }
    TerminalCommand::with_args(opencode_program(), args)
}

/// opencode names models `provider/model`. An empty model means "whatever opencode
/// is configured to use", which is a legitimate choice, so it is simply omitted.
fn push_opencode_model(args: &mut Vec<String>, model: &str) {
    let model = model.trim();
    if !model.is_empty() {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
}

/// Continue an opencode conversation in place (same session, no fork).
pub(crate) fn opencode_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args = vec![
        "--auto".to_string(),
        "--session".to_string(),
        session_id.to_string(),
    ];
    push_opencode_model(&mut args, &run.model);
    TerminalCommand::with_args(opencode_program(), args)
}

/// BRANCH an opencode chat: `--session <id> --fork` copies the conversation into a
/// new session and leaves the original untouched, like the other two backends.
pub(crate) fn opencode_fork_command(
    model: &str,
    session_id: &str,
    prompt: Option<&str>,
) -> TerminalCommand {
    let mut args = vec![
        "--auto".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--fork".to_string(),
    ];
    push_opencode_model(&mut args, model);
    if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--prompt".to_string());
        args.push(prompt.to_string());
    }
    TerminalCommand::with_args(opencode_program(), args)
}

/// Resume an INTERACTIVE orchestrator session as the LIVE CONDUCTOR.
///
/// A structural rebase (`RUDDER_REPLAN`) runs a headless re-decompose over the
/// conductor's session, which tears down the interactive PTY the user was
/// talking to. Once the rebase lands we re-spawn the conductor here so the
/// conversation continues seamlessly. This mirrors the interactive-orchestrator
/// arm of `agent_command_with_orchestrator_mode`, but `--resume`s the existing
/// session (carrying the whole conversation plus the rebase turn) instead of
/// minting a fresh `--session-id`, and submits a short continuation prompt.
pub(crate) fn rudder_orchestrator_resume_command(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    session_id: &str,
    continuation: &str,
) -> TerminalCommand {
    match backend {
        Backend::Claude => {
            let mut args = vec![
                // No `-p`: stay in the interactive Claude Code TUI, resuming the session.
                "--resume".to_string(),
                session_id.to_string(),
                "--permission-mode".to_string(),
                "default".to_string(),
                "--allowedTools".to_string(),
                "Read,Grep,Glob,LS,WebSearch,WebFetch,Bash,Edit,Write".to_string(),
                "--append-system-prompt".to_string(),
                orchestrator_system_prompt(),
                "--name".to_string(),
                "rudder-orchestrator".to_string(),
            ];
            if !model.trim().is_empty() {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(effort) = effort {
                args.push("--effort".to_string());
                args.push(effort.as_str().to_string());
            }
            if !continuation.trim().is_empty() {
                args.push(continuation.to_string());
            }
            TerminalCommand::with_args(claude_program(), args)
                .with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
            let mut args = Vec::new();
            push_codex_interactive_orchestrator_args(&mut args, effort);
            if !model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            args.push("resume".to_string());
            args.push(session_id.to_string());
            if !continuation.trim().is_empty() {
                args.push(continuation.to_string());
            }
            TerminalCommand::with_args(codex_program(), args)
                .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
        }
        Backend::Opencode => {
            let mut args = vec!["--session".to_string(), session_id.to_string()];
            push_opencode_model(&mut args, model);
            if !continuation.trim().is_empty() {
                args.push("--prompt".to_string());
                args.push(continuation.to_string());
            }
            TerminalCommand::with_args(opencode_program(), args)
        }
    }
}

/// Answer Codex's "Choose working directory to fork this session" prompt up front.
///
/// A forked/resumed Codex session remembers the directory it ran in, which for a
/// Rudder handoff is the MAIN checkout — while Rudder deliberately spawns it in a
/// fresh jj workspace. Left unanswered, Codex stops and asks, and picking the
/// session directory would send the agent's edits into the main checkout, outside
/// the workspace Rudder merges. Rudder always sets the cwd it means, so: current.
pub(crate) fn push_codex_resume_cwd_current(args: &mut Vec<String>) {
    args.push("-c".to_string());
    args.push("tui.resume_cwd=\"current\"".to_string());
}

pub(crate) fn push_codex_interactive_orchestrator_args(
    args: &mut Vec<String>,
    effort: Option<EffortLevel>,
) {
    args.push("--no-alt-screen".to_string());
    args.push("--enable".to_string());
    args.push("goals".to_string());
    args.push("--sandbox".to_string());
    args.push("workspace-write".to_string());
    args.push("--ask-for-approval".to_string());
    args.push("never".to_string());
    args.push("--search".to_string());
    push_codex_planner_config_overrides(args, effort);
}

fn push_codex_common_config_overrides(args: &mut Vec<String>, effort: Option<EffortLevel>) {
    // Rudder workers run Codex as a child process, so do not inherit desktop-app
    // notification hooks that expect the official signed app launch chain.
    args.push("-c".to_string());
    args.push("notify=[]".to_string());
    args.push("-c".to_string());
    args.push("model_reasoning_summary=\"detailed\"".to_string());
    args.push("-c".to_string());
    args.push("model_supports_reasoning_summaries=true".to_string());
    if let Some(effort) = effort {
        // Codex has no "max" tier; TS normalizes max→xhigh (normalizeEffortForBackend)
        // but this Rust path used to emit the raw value, which Codex rejects —
        // `/model codex gpt-5.5 max` launched a worker that died on startup.
        let effort = if effort == EffortLevel::Max {
            EffortLevel::XHigh
        } else {
            effort
        };
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
    }
}

/// Implementation, review, and shipping agents match a direct Codex session:
/// user-installed plugins and Computer Use are available inside the already
/// unrestricted worker. Explicit true values prevent a stale global setting from
/// silently removing capabilities Rudder promises to workers.
pub(crate) fn push_codex_worker_config_overrides(
    args: &mut Vec<String>,
    effort: Option<EffortLevel>,
) {
    push_codex_common_config_overrides(args, effort);
    args.push("-c".to_string());
    args.push("features.plugins=true".to_string());
    args.push("-c".to_string());
    args.push("features.computer_use=true".to_string());
}

/// Planners and conductors remain externally side-effect-free. Filesystem
/// sandboxing alone cannot constrain a plugin or Computer Use action.
pub(crate) fn push_codex_planner_config_overrides(
    args: &mut Vec<String>,
    effort: Option<EffortLevel>,
) {
    push_codex_common_config_overrides(args, effort);
    args.push("-c".to_string());
    args.push("features.plugins=false".to_string());
    args.push("-c".to_string());
    args.push("features.computer_use=false".to_string());
}

pub(crate) fn opencode_program() -> String {
    env::var("RUDDER_OPENCODE_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "opencode".to_string())
}

pub(crate) fn codex_program() -> String {
    env::var("RUDDER_CODEX_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

/// The orchestrator runs as a normal INTERACTIVE backend PTY the user converses
/// with (real plan-mode feel + visible thinking), writing its DAG to the orchestrator plan
/// file and self-launching on the `RUDDER_APPROVE_PLAN` marker. This is now the DEFAULT
/// ("the main plan mode"); set `RUDDER_INTERACTIVE_ORCHESTRATOR=0` to opt back into the
/// headless decomposer.
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
        integration: IntegrationEvidence::default(),
        delivery: DeliveryEvidence::default(),
        session_id,
        terminal: None,
        terminal_size: None,
        review_terminal: None,
        review_size: None,
        review_error: None,
        last_output_at: Instant::now(),
        completed_at: None,
        autosteered: false,
        interactive_orchestrator: false,
        needs_permission: false,
        needs_user_input: false,
        last_error: None,
        worker_input_draft: String::new(),
        worker_input_cursor: 0,
        worker_input_is_prompt: false,
        last_drain_at: None,
        review_source_ids: source_ids,
        deps: Vec::new(),
        soft_deps: Vec::new(),
        node_id: None,
        plan_id: None,
        reviewed_at: None,
        reconcile_planner: false,
        plan_stream: None,
        plan_output_cache: None,
        last_worker_input_at: None,
        merge_resolver: false,
        merge_conflict: false,
        merge_conflict_operation: ConflictOperation::Merge,
        merge_conflict_files: Vec::new(),
        had_merge_conflict: false,
        done_summary: None,
        tokens_in: 0,
        tokens_out: 0,
    }
}

/// Prompt for the per-node review agent that runs in a finished node's OWN jj
/// workspace before Rudder auto-merges it. It reviews the node's working-copy
/// diff with the installed `thermonuclear` skill, fixes findings in place, then
/// stops — Rudder integrates the (now reviewed + fixed) change.
pub(crate) fn node_review_prompt(node_label: &str) -> String {
    format!(
        "Review and fix this node's work before it merges.\n\
\n\
You are the Rudder per-node review agent for: {node_label}\n\
You are running in the node's own jj workspace; its uncommitted working-copy change IS the work to review.\n\
\n\
Instructions\n\
1. Run `jj diff` to see exactly what this node changed.\n\
2. If the `thermonuclear` code-review skill/plugin is available, invoke it over that diff (use the Skill tool, or type `/thermonuclear` if your CLI exposes it as a command). If it is NOT installed, do an equivalent thorough review yourself — a missing skill is NOT an error, just review manually.\n\
3. Fix real findings directly in this working copy. Keep the change scoped to this node's task; do not expand it.\n\
4. Run the relevant tests/checks for the files you touched. If a check cannot run, say exactly why.\n\
5. Do NOT run jj/git commit/new/merge/branch commands — Rudder snapshots and integrates this change. When the review is clean, stop and say the node is reviewed.\n",
        node_label = node_label
    )
}

pub(crate) fn review_all_prompt(
    target_ref: &str,
    worktree: &WorktreeInfo,
    sources: &[ReviewAllSource],
    premerge: &ReviewAllPremerge,
) -> String {
    let aggregate_change = worktree
        .jj_change_id
        .as_deref()
        .unwrap_or("current jj change");
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
                "{}. {} ({})\n   jj revision: {}\n   worktree: {}\n   task: {}",
                index + 1,
                source.summary,
                source.id,
                source.revision,
                path,
                truncate_chars(&source.task, 220)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let merged = if premerge.merged_revisions.is_empty() {
        "- none yet".to_string()
    } else {
        premerge
            .merged_revisions
            .iter()
            .map(|revision| format!("- {revision}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let remaining = if premerge.remaining_revisions.is_empty() {
        "- none".to_string()
    } else {
        premerge
            .remaining_revisions
            .iter()
            .map(|revision| format!("- {revision}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stopped = match (
        premerge.stopped_revision.as_deref(),
        premerge.stopped_error.as_deref(),
    ) {
        (Some(revision), Some(error)) => {
            format!("Rudder stopped while combining `{revision}`:\n{error}")
        }
        _ => "No conflict was detected while building the aggregate jj change.".to_string(),
    };

    format!(
        "Review the combined Rudder agent worktree changes against `{target_ref}`.\n\
\n\
You are the Rudder review-all integration agent. You are running in an aggregate jj workspace that will become one reviewed integration.\n\
\n\
Aggregate worktree\n\
- path: {path}\n\
- jj change: {aggregate_change}\n\
- target/base ref: {target_ref}\n\
\n\
Source worktrees included in this review\n\
{source_lines}\n\
\n\
Pre-merge state\n\
Already combined into this aggregate change:\n\
{merged}\n\
\n\
Still not fully merged:\n\
{remaining}\n\
\n\
{stopped}\n\
\n\
Instructions\n\
1. Run `jj status` and `jj resolve --list` first. Resolve conflicts by editing files; do not use `git add` or create commits.\n\
2. If a revision remains under \"Still not fully merged\", report it; Rudder stopped before stacking another change on a conflict.\n\
3. If the `thermonuclear` code-review skill/plugin is available, invoke it over `jj diff --from {target_ref}` (use the Skill tool, or type `/thermonuclear` if your CLI exposes it as a command). If it is NOT installed, do an equivalent thorough review yourself — a missing skill is NOT an error, just review manually.\n\
4. Fix real review findings directly in this aggregate worktree. Do not edit the original source worktrees.\n\
5. Run the relevant tests/checks for the files touched. If a check cannot run, say exactly why.\n\
6. Do not run jj commit/new/squash or Git branch/merge commands. When the aggregate change is ready, stop and say: `Rudder review-all change is ready; press m on this row to integrate it.`\n",
        target_ref = target_ref,
        path = worktree.path.display(),
        aggregate_change = aggregate_change,
        source_lines = source_lines,
        merged = merged,
        remaining = remaining,
        stopped = stopped,
    )
}
