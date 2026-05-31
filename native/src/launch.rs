#![allow(unused_imports)]
//! Building agent launch/resume commands and review-all runs.
use super::*;

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
    TerminalCommand::with_args("claude", args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
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
                AgentMode::Plan | AgentMode::RudderPlan => vec![
                    "--permission-mode".to_string(),
                    "plan".to_string(),
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
            if let Some(sid) = session_id {
                args.push("--session-id".to_string());
                args.push(sid.to_string());
            }
            if let Some(prompt) = prompt {
                args.push(prompt);
            }
            TerminalCommand::with_args("claude", args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
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
            if let Some(prompt) = prompt {
                args.push(prompt);
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
