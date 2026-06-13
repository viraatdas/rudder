#![allow(unused_imports)]
//! Rudder Cloud command plumbing and Codex session-log scanning.
use super::*;

pub(crate) fn is_cloud_worker_session() -> bool {
    env::var("RUDDER_WORKSPACE_ID")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
        || env::var("RUDDER_SAIL_ID")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
}

pub(crate) fn read_cloud_summary() -> CloudSummary {
    if env::var("RUDDER_CLOUD_TOKEN")
        .ok()
        .is_some_and(|token| !token.trim().is_empty())
    {
        return CloudSummary {
            connected: true,
            runtime: env::var("RUDDER_CLOUD_RUNTIME")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
    }

    let Some(path) = rudder_cloud_auth_path() else {
        return CloudSummary {
            connected: false,
            runtime: None,
        };
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return CloudSummary {
            connected: false,
            runtime: None,
        };
    };
    let data = serde_json::from_str::<serde_json::Value>(&raw).ok();
    let connected = data.as_ref().is_some_and(|data| {
        data.get("token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.trim().is_empty())
    });
    let runtime = env::var("RUDDER_CLOUD_RUNTIME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            data.as_ref()
                .and_then(|data| data.get("defaultRuntime"))
                .and_then(serde_json::Value::as_str)
                .map(|value| if value == "byo-vm" { "byoc" } else { value }.to_string())
        });
    CloudSummary { connected, runtime }
}

pub(crate) fn cloud_workspace_label(workspace: Option<&CloudWorkspaceStatus>) -> String {
    let Some(workspace) = workspace else {
        return "cloud workspace · none".to_string();
    };
    let status = workspace.status.as_deref().unwrap_or("unknown");
    if workspace.client_count > 0 {
        format!(
            "cloud workspace · {status} · {} attached",
            workspace.client_count
        )
    } else if workspace.active_agents {
        format!("cloud workspace · {status} · active")
    } else if let Some(idle) = workspace.idle_minutes {
        format!("cloud workspace · {status} · idle {idle}m")
    } else {
        format!("cloud workspace · {status}")
    }
}

pub(crate) fn query_cloud_workspace_status(cwd: &Path) -> Option<CloudWorkspaceStatus> {
    let rudder = locate_rudder_cli()?;
    let mut child = Command::new(rudder)
        .args(["cloud", "workspace", "status", "--json"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Bound the wait. A slow or unreachable cloud relay would otherwise let the CLI
    // hang forever, wedging the status-feed caller with no deadline. Poll for exit
    // and kill the child if it overruns, treating an overrun as "status unavailable".
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(text.trim()).ok()?;
    if value.get("offline").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    if value.get("workspace").is_some_and(|v| v.is_null()) {
        return Some(CloudWorkspaceStatus::default());
    }
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string());
    if id.is_none() {
        return None;
    }
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string());
    let client_count = value
        .get("clientCount")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(0);
    let active_agents = value
        .get("activeAgents")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let idle_minutes = value
        .get("idleMinutes")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
    Some(CloudWorkspaceStatus {
        id,
        status,
        active_agents,
        client_count,
        idle_minutes,
    })
}

pub(crate) fn locate_rudder_cli() -> Option<PathBuf> {
    if let Ok(value) = env::var("RUDDER_CLI") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    // PATH lookup
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join("rudder");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn cloud_args_need_auth(args: &[&str]) -> bool {
    !args
        .first()
        .is_some_and(|arg| matches!(*arg, "help" | "login"))
}

pub(crate) fn cloud_args_start_worker(args: &[&str]) -> bool {
    match args.first().copied() {
        None => true,
        Some(
            "help" | "login" | "list" | "ls" | "status" | "runtime" | "setup" | "setup-byoc"
            | "setup-vm" | "setup-fly" | "bootstrap" | "pause" | "resume" | "stop" | "logs"
            | "onload" | "byoc" | "vm" | "byo-vm",
        ) => false,
        Some(_) => true,
    }
}

pub(crate) fn cloud_agent_label(args: &[String]) -> String {
    match args.get(1).map(String::as_str) {
        Some("list" | "ls") => "cloud list".to_string(),
        Some("help") => "cloud help".to_string(),
        Some("login") => "cloud login".to_string(),
        Some("onload") => args
            .get(2)
            .map(|id| format!("cloud onload {id}"))
            .unwrap_or_else(|| "cloud onload".to_string()),
        Some("launch") => "cloud launch".to_string(),
        Some("pause" | "resume" | "stop" | "status" | "logs") => args
            .get(2)
            .map(|id| format!("cloud {} {id}", args[1]))
            .unwrap_or_else(|| format!("cloud {}", args[1])),
        Some(name) => format!("cloud {name}"),
        None => "cloud".to_string(),
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CloudPromptLaunch {
    pub(crate) label: String,
    pub(crate) args: Vec<String>,
}

pub(crate) fn cloud_prompt_launch(
    prompt: &CloudLaunchPrompt,
) -> Result<CloudPromptLaunch, &'static str> {
    match prompt.choice {
        CloudLaunchChoice::Upload => {
            let label = prompt
                .selected_task
                .as_deref()
                .map(|task| format!("cloud workspace {}", short_task(task)))
                .unwrap_or_else(|| "cloud workspace".to_string());
            Ok(CloudPromptLaunch {
                label,
                args: vec!["cloud".to_string(), "onload".to_string()],
            })
        }
        CloudLaunchChoice::Scratch => Ok(CloudPromptLaunch {
            label: prompt.scratch_label.clone(),
            args: prompt.scratch_args.clone(),
        }),
    }
}

pub(crate) fn random_cloud_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "bright", "calm", "clear", "cosmic", "gentle", "golden", "lucky", "rapid",
        "silver", "steady", "swift",
    ];
    const NOUNS: &[&str] = &[
        "atlas", "harbor", "signal", "summit", "orbit", "ranger", "river", "rocket", "sparrow",
        "station", "voyager", "wave",
    ];
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as usize)
        .unwrap_or_default()
        ^ std::process::id() as usize;
    format!(
        "{}-{}",
        ADJECTIVES[seed % ADJECTIVES.len()],
        NOUNS[(seed / ADJECTIVES.len()) % NOUNS.len()]
    )
}

pub(crate) fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

pub(crate) fn latest_codex_session_id_for_cwd(cwd: &Path) -> Option<String> {
    let root = user_home_dir()?.join(".codex").join("sessions");
    let target = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(SystemTime, String)> = None;
    visit_codex_session_dir(&root, &target, &mut best, 0);
    best.map(|(_, id)| id)
}

pub(crate) fn latest_codex_rudder_plan_output(run: &AgentRun) -> Option<String> {
    if run.backend != Backend::Codex || run.mode != AgentMode::RudderPlan {
        return None;
    }
    let root = user_home_dir()?.join(".codex").join("sessions");
    let target = fs::canonicalize(&run.cwd).unwrap_or_else(|_| run.cwd.clone());
    let created_after = run
        .created_at
        .parse::<u64>()
        .ok()
        .map(|millis| UNIX_EPOCH + Duration::from_millis(millis));
    latest_codex_rudder_plan_output_in_dir(&root, &target, created_after)
}

pub(crate) fn latest_codex_rudder_plan_output_in_dir(
    root: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
) -> Option<String> {
    let mut best: Option<(SystemTime, String)> = None;
    visit_codex_rudder_plan_output_dir(root, target_cwd, created_after, &mut best, 0);
    best.map(|(_, output)| output)
}

pub(crate) fn visit_codex_rudder_plan_output_dir(
    dir: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
    best: &mut Option<(SystemTime, String)>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            visit_codex_rudder_plan_output_dir(&path, target_cwd, created_after, best, depth + 1);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some((modified, output)) =
            codex_rudder_plan_output_if_cwd_matches(&path, target_cwd, created_after)
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(stamp, _)| modified > *stamp) {
            *best = Some((modified, output));
        }
    }
}

pub(crate) fn codex_rudder_plan_output_if_cwd_matches(
    path: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
) -> Option<(SystemTime, String)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if let Some(created_after) = created_after {
        let cutoff = created_after
            .checked_sub(Duration::from_secs(60))
            .unwrap_or(created_after);
        if modified < cutoff {
            return None;
        }
    }

    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !codex_session_meta_cwd_matches(&line, target_cwd) {
        return None;
    }

    let mut output = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        collect_codex_session_assistant_text(&line, &mut output);
    }
    if output.contains("RUDDER_PLAN_TASKS_START") {
        Some((modified, output))
    } else {
        None
    }
}

pub(crate) fn codex_session_meta_cwd_matches(line: &str, target_cwd: &Path) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return false;
    }
    let Some(cwd) = value
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let session_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    session_cwd == target_cwd
}

pub(crate) fn collect_codex_session_assistant_text(line: &str, out: &mut String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(record_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(payload) = value.get("payload") else {
        return;
    };
    match record_type {
        "response_item" => collect_codex_response_item_text(payload, out),
        "event_msg" => {
            if matches!(
                payload.get("type").and_then(serde_json::Value::as_str),
                Some("agent_message" | "final_answer")
            ) {
                append_codex_text(payload.get("message"), out);
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_codex_response_item_text(payload: &serde_json::Value, out: &mut String) {
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return;
    }
    if payload.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = payload.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for item in content {
        append_codex_text(item.get("text"), out);
    }
}

pub(crate) fn append_codex_text(value: Option<&serde_json::Value>, out: &mut String) {
    let Some(text) = value.and_then(serde_json::Value::as_str) else {
        return;
    };
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}

pub(crate) fn visit_codex_session_dir(
    dir: &Path,
    target_cwd: &Path,
    best: &mut Option<(SystemTime, String)>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            visit_codex_session_dir(&path, target_cwd, best, depth + 1);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = codex_session_id_if_cwd_matches(&path, target_cwd) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().is_none_or(|(stamp, _)| modified > *stamp) {
            *best = Some((modified, session_id));
        }
    }
}

pub(crate) fn codex_session_id_if_cwd_matches(path: &Path, target_cwd: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let session_id = payload.get("id")?.as_str()?.to_string();
    let cwd = payload.get("cwd")?.as_str()?;
    let session_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    if session_cwd == target_cwd {
        Some(session_id)
    } else {
        None
    }
}
