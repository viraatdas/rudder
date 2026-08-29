#![allow(unused_imports)]
//! Rudder Cloud command plumbing and Codex session-log scanning.
use super::*;

pub(crate) fn is_cloud_worker_session() -> bool {
    is_cloud_workspace_session()
        || env::var("RUDDER_SAIL_ID")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
}

pub(crate) fn is_cloud_workspace_session() -> bool {
    env::var("RUDDER_WORKSPACE_ID")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
}

pub(crate) fn read_cloud_summary() -> CloudSummary {
    // Inside a cloud worker VM there is no user auth file or client token.
    // RUDDER_WORKSPACE_ID / RUDDER_SAIL_ID is the process's authoritative
    // execution identity; the supervisor deliberately withholds its worker
    // bearer from the dashboard child. Requiring that secret here rendered
    // "cloud offline" about the very cloud it was running in.
    if is_cloud_worker_session() {
        return CloudSummary {
            connected: true,
            runtime: env::var("RUDDER_CLOUD_RUNTIME")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
    }
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
    // Name the workspace, not just its state: "cloud workspace · running" told
    // the user nothing about WHICH workspace they were in (unclear enough to
    // be reported). The id is the identity users see in `rudder cloud list`.
    let name = workspace
        .id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!(" · {id}"))
        .unwrap_or_default();
    if workspace.client_count > 0 {
        format!(
            "cloud workspace{name} · {status} · {} attached",
            workspace.client_count
        )
    } else if workspace.active_agents_known && workspace.active_agents > 0 {
        // The worker-reported count is the busy FACT: agents are mid-task in
        // the cloud machine even with nobody attached, and the idle sweep
        // leaves them alone because of it. Show the count so "it's still
        // working" is answerable at a glance after a disconnect.
        format!(
            "cloud workspace{name} · {status} · {} running",
            workspace.active_agents
        )
    } else if workspace.active_agents_known {
        format!("cloud workspace{name} · {status} · idle")
    } else if let Some(idle) = workspace.idle_minutes {
        format!("cloud workspace{name} · {status} · idle {idle}m")
    } else {
        format!("cloud workspace{name} · {status}")
    }
}

pub(crate) fn query_cloud_workspace_status(cwd: &Path) -> Option<CloudWorkspaceStatus> {
    // Inside the worker VM, the workspace is not something to look up — it is
    // the machine's own identity, delivered by env. The CLI query below would
    // run unauthenticated there and fail, which rendered "cloud workspace ·
    // none" inside every cloud workspace.
    if let Ok(id) = env::var("RUDDER_WORKSPACE_ID") {
        if !id.trim().is_empty() {
            return Some(CloudWorkspaceStatus {
                id: Some(id),
                status: Some("running".to_string()),
                ..CloudWorkspaceStatus::default()
            });
        }
    }
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
    // The CLI reports `activeAgentCount` (worker-reported, may be null on old
    // control planes) and `activeAgents` (bool, the legacy heuristic). Prefer
    // the fact; fall back to the bool so an old CLI still degrades gracefully.
    let (active_agents, active_agents_known) = match value
        .get("activeAgentCount")
        .and_then(serde_json::Value::as_u64)
    {
        Some(count) => (count as u32, true),
        None => match value.get("activeAgentCount") {
            Some(serde_json::Value::Null) => (
                value
                    .get("activeAgents")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false) as u32,
                false,
            ),
            _ => (0, false),
        },
    };
    let idle_minutes = value
        .get("idleMinutes")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
    Some(CloudWorkspaceStatus {
        id,
        status,
        active_agents,
        active_agents_known,
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
            | "onload" | "workspace" | "byoc" | "vm" | "byo-vm",
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
        Some("workspace") => match args.get(2).map(String::as_str) {
            Some("attach") | None => "cloud migrate agents".to_string(),
            Some(action) => format!("cloud workspace {action}"),
        },
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

/// Where the per-repo cloud-mode flag lives. Plain task input routes to
/// Rudder Cloud while this is on, so it must survive a dashboard restart.
fn cloud_mode_path(cwd: &Path) -> PathBuf {
    cwd.join(".rudder").join("cloud-mode.json")
}

/// Read the per-repo cloud-mode flag. Absent or corrupt = off; a corrupted
/// one-line flag must never keep a user stuck routing tasks to the cloud.
pub(crate) fn read_cloud_mode(cwd: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(cloud_mode_path(cwd)) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.get("enabled").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// Persist the flag atomically (temp + rename), matching every other
/// `.rudder` ledger write. Best effort: the in-memory flag stays authoritative
/// for this session even if the disk write fails.
pub(crate) fn write_cloud_mode(cwd: &Path, enabled: bool) {
    let dir = cwd.join(".rudder");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = cloud_mode_path(cwd);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let Ok(json) = serde_json::to_string(&serde_json::json!({ "enabled": enabled })) else {
        return;
    };
    if fs::write(&tmp, json).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

pub(crate) fn latest_codex_session_id_for_cwd(cwd: &Path) -> Option<String> {
    let root = user_home_dir()?.join(".codex").join("sessions");
    latest_codex_session_id_in_dir(&root, cwd)
}

/// The Codex session a SPECIFIC RUN started, matched by cwd AND start time.
///
/// Matching on cwd alone is wrong for anything running in the main checkout: a
/// busy repo accumulates dozens of sessions in the same directory (90 in two days
/// on the machine this was written for), so "the newest one" pins somebody else's
/// conversation — worse than finding nothing, because resuming or branching it
/// opens the wrong chat.
///
/// A rollout records when its session began, and exactly one session begins in the
/// seconds after Rudder spawns the process. That makes the mapping unambiguous
/// even when it has to be reconstructed later — which is what rescues a run whose
/// id was never recorded because the machine died mid-session.
pub(crate) fn codex_session_id_for_run(cwd: &Path, started_at_ms: u64) -> Option<String> {
    let root = user_home_dir()?.join(".codex").join("sessions");
    codex_session_id_for_run_in(&root, cwd, started_at_ms)
}

/// How long after the spawn a session may appear and still be considered this
/// run's. Codex writes `session_meta` within a second; the slack absorbs a slow
/// start, and the small negative bound absorbs clock jitter.
const CODEX_SESSION_MATCH_BEFORE_MS: i64 = 30_000;
const CODEX_SESSION_MATCH_AFTER_MS: i64 = 180_000;

pub(crate) fn codex_session_id_for_run_in(
    root: &Path,
    cwd: &Path,
    started_at_ms: u64,
) -> Option<String> {
    let target = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let needles = [
        format!("\"cwd\":\"{}\"", target.display()),
        format!("\"cwd\":\"{}\"", cwd.display()),
    ];
    let mut best: Option<(i64, String)> = None;
    // Codex partitions rollouts by DAY, so only the day of the spawn and its
    // neighbours can hold the match — the difference between reading ~100 files
    // and every rollout the user has ever created.
    for dir in codex_day_dirs(root, started_at_ms) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(head) = read_file_head(&path, 8 * 1024) else {
                continue;
            };
            // Cheap reject before parsing: `cwd` sits in the first few hundred
            // bytes, while the rest of that line is the entire system prompt.
            if !needles.iter().any(|needle| head.contains(needle.as_str())) {
                continue;
            }
            let Some(line) = head.lines().next() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
                continue;
            }
            let Some(payload) = value.get("payload") else {
                continue;
            };
            let Some(id) = payload.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(started) = payload
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_iso8601_millis)
            else {
                continue;
            };
            let delta = started - started_at_ms as i64;
            if delta < -CODEX_SESSION_MATCH_BEFORE_MS || delta > CODEX_SESSION_MATCH_AFTER_MS {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(current, _)| delta.abs() < current.abs())
            {
                best = Some((delta, id.to_string()));
            }
        }
    }
    best.map(|(_, id)| id)
}

/// `<root>/YYYY/MM/DD` for the spawn day and its neighbours. Codex names the
/// directory in LOCAL time while the record inside is UTC, so the neighbours cover
/// the offset without needing a timezone database.
fn codex_day_dirs(root: &Path, started_at_ms: u64) -> Vec<PathBuf> {
    let day = (started_at_ms / 86_400_000) as i64;
    (-1..=1)
        .filter_map(|offset| {
            let (year, month, date) = civil_from_days(day + offset);
            let dir = root
                .join(format!("{year:04}"))
                .join(format!("{month:02}"))
                .join(format!("{date:02}"));
            dir.is_dir().then_some(dir)
        })
        .collect()
}

fn read_file_head(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0_u8; max_bytes];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Epoch millis from `2026-07-29T04:49:30.689Z`. The timestamp is always UTC, so
/// this needs no timezone data — only the civil-date arithmetic below.
pub(crate) fn parse_iso8601_millis(value: &str) -> Option<i64> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<i64>().ok();
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let millis = value
        .get(20..23)
        .filter(|_| bytes.get(19) == Some(&b'.'))
        .and_then(|fraction| fraction.parse::<i64>().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) + millis)
}

/// Howard Hinnant's civil-date algorithms: days since 1970-01-01 and back.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub(crate) fn latest_codex_session_id_in_dir(root: &Path, cwd: &Path) -> Option<String> {
    let target = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(SystemTime, String)> = None;
    visit_codex_session_dir(root, &target, &mut best, 0);
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
        // Symlinked rollouts are SKIPPED, deliberately. Cloud-offload tools
        // replace old sessions with symlinks into a file-provider mount
        // (observed: ~/Library/CloudStorage/StowAgent-Stow), and opening one
        // makes macOS materialize it on demand — which BLOCKS INDEFINITELY at
        // 0% CPU when the provider hangs. This walk runs on the UI thread from
        // a `b` press; an archived chat missing from a picker beats a wedged
        // dashboard. file_type() here is lstat semantics: it does not follow.
        if kind.is_symlink() {
            continue;
        }
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

/// Cheap "could this rollout be ours?" test against a bounded head.
///
/// The caller hands us a CANONICAL path while the rollout recorded whatever the
/// shell was in, so both spellings of macOS's symlinked roots are accepted
/// (`/private/var/...` vs `/var/...`). A false positive only costs the JSON parse
/// below, which then compares canonicalized paths properly; a false NEGATIVE would
/// silently lose the session, so this errs wide.
fn head_mentions_cwd(head: &str, target_cwd: &Path) -> bool {
    let canonical = target_cwd.display().to_string();
    let mut spellings = vec![canonical.clone()];
    match canonical.strip_prefix("/private") {
        Some(rest) if rest.starts_with('/') => spellings.push(rest.to_string()),
        _ => spellings.push(format!("/private{canonical}")),
    }
    spellings.iter().any(|spelling| {
        head.contains(&format!("\"cwd\":\"{spelling}\""))
            || head.contains(&format!("\"cwd\": \"{spelling}\""))
    })
}

pub(crate) fn codex_session_id_if_cwd_matches(path: &Path, target_cwd: &Path) -> Option<String> {
    // Read a bounded HEAD, not the first line: that line carries Codex's entire
    // system prompt (~19KB here), and this runs once per rollout across a tree
    // that grows into the thousands. `cwd` sits in its first few hundred bytes, so
    // a short read plus a substring reject skips the JSON parse for almost every
    // file — the difference between a scan that stalls the dashboard for seconds
    // and one that does not.
    let head = read_file_head(path, 8 * 1024)?;
    if !head_mentions_cwd(&head, target_cwd) {
        return None;
    }
    let line = head.lines().next()?;
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
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
