#![allow(unused_imports)]
//! Prompt construction, task-summary generation, and rudder-plan parsing.
use super::*;


pub(crate) fn execution_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "Rudder-specific context injected by Rudder:\n- Read RUDDER.md first if it exists. Rudder generated that file to show active Rudder agents and worktrees in this repo.\n- If a Hunk review is open for this worktree, run `hunk skill path`, load that skill, and use `hunk session review --repo . --json` plus `hunk session comment ...` commands to inspect and annotate the live review.\n\n{task}"
    )
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
        "You are Rudder's planning coordinator. Inspect the repository in read-only mode and decide whether this user request should be split across multiple independent implementation agents.\n\nUser request:\n{task}\n\nProcess:\n1. Identify missing requirements. If the work is ambiguous enough that implementation would likely go wrong, ask concise follow-up questions and do not emit tasks yet.\n2. Otherwise create the smallest set of independent implementation tasks that can run in separate git worktrees with minimal conflicts.\n3. Each task must be self-contained, include concrete files or modules to inspect when known, and include its own verification instructions.\n4. Do not include a task that depends on another task's unmerged changes. If work is sequential, make one task.\n5. Prefer 1-4 tasks. Use more only when the split is clearly independent.\n6. For each worker task that is bigger than one normal turn and has a clear validation loop, include a `goal` value suitable for Codex `/goal`. The goal must name one durable objective, important constraints, validation commands or artifacts, and a verifiable stopping condition. Omit `goal` or set it to an empty string for small tasks, vague tasks, or loose backlogs.\n\nWhen the task list is ready, print exactly this block and no other JSON block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"title\":\"short task title\",\"prompt\":\"full implementation prompt for one worker agent\",\"goal\":\"optional durable objective for /goal, without the leading slash command\"}}]}}\nRUDDER_PLAN_TASKS_END\n\nAfter the block, add a short human summary of why this split is safe."
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RudderPlanTask {
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) goal: Option<String>,
}

pub(crate) fn rudder_plan_output_for_run(run: &AgentRun) -> String {
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

    let mut out = Vec::new();
    for task in tasks.iter().take(6) {
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
        out.push(RudderPlanTask {
            title: if title.is_empty() {
                "worker task".to_string()
            } else {
                title.to_string()
            },
            prompt: prompt.to_string(),
            goal: task
                .get("goal")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
                .map(ToString::to_string),
        });
    }

    Ok(out)
}

pub(crate) fn rudder_plan_worker_prompt(
    planner_task: &str,
    task: &RudderPlanTask,
    backend: Backend,
) -> String {
    let mut prompt = format!(
        "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nOriginal request:\n{planner_task}\n\nWorker task: {}\n\n{}",
        task.title, task.prompt
    );
    if let Some(goal) = task.goal.as_deref() {
        match backend {
            Backend::Codex => {
                prompt.push_str(
                    "\n\nDurable Codex goal:\nIf goals are available, start by setting this goal before implementation:\n",
                );
                prompt.push_str("/goal ");
                prompt.push_str(goal);
            }
            Backend::Claude => {
                prompt.push_str(
                    "\n\nDurable objective:\nUse this as the stopping condition for the worker task:\n",
                );
                prompt.push_str(goal);
            }
        }
    }
    prompt
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
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
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
