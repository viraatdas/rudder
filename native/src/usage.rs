#![allow(unused_imports)]
//! Token-usage accounting, pricing tables, and date helpers.
use super::*;

pub(crate) fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub(crate) fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    let mut y = 1970_i64;
    loop {
        let dy = if is_leap_year(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let month_lens: [u32; 12] = [
        31,
        if is_leap_year(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0_u32;
    for (i, &ml) in month_lens.iter().enumerate() {
        if (days as u32) < ml {
            m = i as u32;
            break;
        }
        days -= ml as i64;
    }
    let d = days as u32 + 1;
    (y, m + 1, d)
}

/// Load the per-directory rudder session start timestamp from
/// `.rudder/session.json`, or create one on first use. This makes
/// session-scoped features (like /usage) persistent across rudder
/// restarts in the same repo. To reset, delete the file.
pub(crate) fn load_or_init_session_started(repo_root: &Path) -> String {
    let path = repo_root.join(".rudder/session.json");
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(ts) = value.get("started_at_iso").and_then(|v| v.as_str()) {
                if !ts.trim().is_empty() {
                    return ts.to_string();
                }
            }
        }
    }
    let now = system_time_to_iso(SystemTime::now());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "started_at_iso": now.clone() });
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
    now
}

pub(crate) fn system_time_to_iso(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (secs / 86400) as i64;
    let day_secs = secs % 86400;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.000Z")
}

pub(crate) fn encode_claude_projects_cwd(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// True when `path` was last modified before `since_iso` (with a small slack for
/// the whole-second truncation of `system_time_to_iso`): its content cannot hold
/// events at/after `since_iso`, so usage scans skip reading it entirely.
pub(crate) fn file_modified_before(path: &Path, since_iso: &str) -> bool {
    if since_iso.is_empty() {
        return false;
    }
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| system_time_to_iso(modified + Duration::from_secs(2)).as_str() < since_iso)
        .unwrap_or(false)
}

pub(crate) fn collect_claude_usage(
    repo_root: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let Some(home) = user_home_dir() else {
        return std::collections::BTreeMap::new();
    };
    collect_claude_usage_in(&home.join(".claude/projects"), repo_root, since_iso)
}

pub(crate) fn collect_claude_usage_in(
    projects_root: &Path,
    cwd: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let mut out: std::collections::BTreeMap<String, ModelUsage> = std::collections::BTreeMap::new();
    let dir_name = encode_claude_projects_cwd(cwd);
    let project_dir = projects_root.join(&dir_name);
    let Ok(entries) = fs::read_dir(&project_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if file_modified_before(&path, since_iso) {
            continue;
        }
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
                        continue;
                    }
                    let ts = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !ts.is_empty() && ts < since_iso {
                        continue;
                    }
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let Some(usage) = message.get("usage") else {
                        continue;
                    };
                    let model = message
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    if model == "<synthetic>" {
                        continue;
                    }
                    let entry = out.entry(model).or_default();
                    if let Some(n) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        entry.input_tokens += n;
                    }
                    if let Some(n) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        entry.output_tokens += n;
                    }
                    if let Some(n) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        entry.cache_creation_input_tokens += n;
                    }
                    if let Some(n) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        entry.cache_read_input_tokens += n;
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn collect_codex_usage(
    repo_root: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let Some(home) = user_home_dir() else {
        return std::collections::BTreeMap::new();
    };
    collect_codex_usage_in(&home.join(".codex/sessions"), repo_root, since_iso)
}

pub(crate) fn collect_codex_usage_in(
    sessions_root: &Path,
    cwd: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let mut out: std::collections::BTreeMap<String, ModelUsage> = std::collections::BTreeMap::new();
    if !sessions_root.exists() {
        return out;
    }
    let target_cwd = cwd.display().to_string();
    let mut files = Vec::new();
    collect_jsonl_files(sessions_root, &mut files, 4);
    for file in files {
        if file_modified_before(&file, since_iso) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let mut session_cwd: Option<String> = None;
        let mut session_model: Option<String> = None;
        let mut session_start: Option<String> = None;
        let mut last_total: Option<(u64, u64, u64, u64)> = None;
        // (input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens)
        for line in content.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "session_meta" => {
                    if let Some(cwd) = value
                        .get("payload")
                        .and_then(|p| p.get("cwd"))
                        .and_then(|v| v.as_str())
                    {
                        session_cwd = Some(cwd.to_string());
                    }
                    if let Some(ts) = value
                        .get("payload")
                        .and_then(|p| p.get("timestamp"))
                        .and_then(|v| v.as_str())
                    {
                        session_start = Some(ts.to_string());
                    } else if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
                        session_start = Some(ts.to_string());
                    }
                }
                "turn_context" => {
                    if session_model.is_none() {
                        if let Some(m) = value
                            .get("payload")
                            .and_then(|p| p.get("model"))
                            .and_then(|v| v.as_str())
                        {
                            session_model = Some(m.to_string());
                        }
                    }
                }
                "event_msg" => {
                    let payload = value.get("payload");
                    if payload.and_then(|p| p.get("type")).and_then(|v| v.as_str())
                        == Some("token_count")
                    {
                        if let Some(info) = payload.and_then(|p| p.get("info")) {
                            if let Some(total) = info.get("total_token_usage") {
                                let inp = total
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let cached = total
                                    .get("cached_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let out_t = total
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let reasoning = total
                                    .get("reasoning_output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                last_total = Some((inp, cached, out_t, reasoning));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(cwd) = session_cwd else {
            continue;
        };
        if cwd != target_cwd {
            continue;
        }
        // Scope to "this rudder session": only count codex sessions that
        // started after this rudder dashboard launched.
        if let Some(start) = session_start.as_deref() {
            if start < since_iso {
                continue;
            }
        } else {
            continue;
        }
        let model = session_model.unwrap_or_else(|| "unknown-codex".to_string());
        if let Some((inp, cached, out_t, reasoning)) = last_total {
            let entry = out.entry(model).or_default();
            // Map codex semantics into the shared ModelUsage shape: codex's
            // cached_input_tokens are a subset of input_tokens (already counted),
            // so we don't double them in input_tokens; we put the cached portion
            // into cache_read_input_tokens for visibility and discount pricing.
            let billable_input = inp.saturating_sub(cached);
            entry.input_tokens += billable_input;
            entry.cache_read_input_tokens += cached;
            // reasoning tokens are billed as output tokens by OpenAI.
            entry.output_tokens += out_t + reasoning;
        }
    }
    out
}

/// Normalize a run record's createdAt for comparison against backend session
/// timestamps. Native records store epoch millis (`now_stamp`); records written
/// by the TS orchestrator store ISO already and pass through unchanged.
pub(crate) fn created_at_to_iso(created_at: &str) -> String {
    let trimmed = created_at.trim();
    if let Ok(millis) = trimmed.parse::<u64>() {
        return system_time_to_iso(UNIX_EPOCH + Duration::from_millis(millis));
    }
    trimmed.to_string()
}

/// Best-effort token totals for ONE run, read from the backend's own session
/// log scoped by the run's cwd (unique per jj workspace) and creation time.
/// Interactive PTYs expose no usage stream, so the session log is the only
/// cost source a native run has. Mirrors the run.tokens semantics of the TS
/// worker (src/backends.ts accumulateUsage): claude input includes cache
/// creation/read tokens; codex input is billable input (the cached subset
/// excluded) and reasoning tokens bill as output. Returns (input, output);
/// (0, 0) when no session log matches.
pub(crate) fn collect_run_token_usage(run: &AgentRun) -> (u64, u64) {
    let Some(home) = user_home_dir() else {
        return (0, 0);
    };
    collect_run_token_usage_in(
        run,
        &home.join(".claude/projects"),
        &home.join(".codex/sessions"),
    )
}

pub(crate) fn collect_run_token_usage_in(
    run: &AgentRun,
    claude_projects_root: &Path,
    codex_sessions_root: &Path,
) -> (u64, u64) {
    let since = created_at_to_iso(&run.created_at);
    let mut input = 0u64;
    let mut output = 0u64;
    match run.backend {
        Backend::Claude => {
            for usage in collect_claude_usage_in(claude_projects_root, &run.cwd, &since).values() {
                input += usage.input_tokens
                    + usage.cache_creation_input_tokens
                    + usage.cache_read_input_tokens;
                output += usage.output_tokens;
            }
        }
        // opencode keeps its usage in its own store (`opencode stats`), which Rudder
        // does not read yet: an opencode row reports zero rather than a wrong number.
        Backend::Opencode => {}
        Backend::Codex => {
            for usage in collect_codex_usage_in(codex_sessions_root, &run.cwd, &since).values() {
                input += usage.input_tokens;
                output += usage.output_tokens;
            }
        }
    }
    (input, output)
}

/// One-shot refresh of a run's token accounting at turn completion. Keeps the
/// larger total (the backend reports cumulative numbers, so a rescan of the same
/// session can only grow them) so a later pass never regresses a stored figure.
pub(crate) fn refresh_run_token_usage(run: &mut AgentRun) {
    let (input, output) = collect_run_token_usage(run);
    if input + output > run.tokens_in + run.tokens_out {
        run.tokens_in = input;
        run.tokens_out = output;
    }
}

pub(crate) fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>, depth_limit: usize) {
    if depth_limit == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                collect_jsonl_files(&path, out, depth_limit - 1);
            } else if ft.is_file() && path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(path);
            }
        }
    }
}

/// Approximate OpenAI pricing per million tokens as `(input, output,
/// cache_creation, cache_read)` — the same 4-tuple contract as `model_pricing`
/// and `claude_model_pricing`. The cost formula multiplies slot 1 by output
/// tokens, so do NOT treat slots 1/2 as unused. Cached input is ~10% of input.
pub(crate) fn codex_model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt-5") || m.starts_with("o3") {
        return Some((10.0, 30.0, 0.0, 1.0));
    }
    if m.starts_with("gpt-4o-mini") {
        return Some((0.15, 0.60, 0.0, 0.075));
    }
    if m.starts_with("gpt-4o") {
        return Some((2.50, 10.0, 0.0, 1.25));
    }
    if m.starts_with("o1") {
        return Some((15.0, 60.0, 0.0, 7.50));
    }
    None
}

/// Approximate Anthropic pricing per million tokens (input, output,
/// cache_creation, cache_read). Used to surface a rough cost estimate, not
/// billing-grade numbers.
pub(crate) fn claude_model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    // Fable is the current flagship family; price it at the flagship tier so it
    // does not silently bill at $0.00 in /usage.
    if m.contains("fable") {
        return Some((15.0, 75.0, 18.75, 1.50));
    }
    if m.contains("opus") {
        return Some((15.0, 75.0, 18.75, 1.50));
    }
    if m.contains("sonnet") {
        return Some((3.0, 15.0, 3.75, 0.30));
    }
    if m.contains("haiku") {
        return Some((0.80, 4.0, 1.00, 0.08));
    }
    // Any other Claude family (e.g. a newly launched model id surfaced from the
    // dynamic model list before its pricing is known): fall back to a
    // mid-tier (sonnet) estimate rather than reporting it as free.
    if m.contains("claude") {
        return Some((3.0, 15.0, 3.75, 0.30));
    }
    None
}

pub(crate) fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub(crate) fn short_model_label(model: &str) -> String {
    // claude-haiku-4-5-20251001 -> haiku-4-5
    let lower = model.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("claude-") {
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() >= 2 {
            return format!("{}-{}", parts[0], parts.get(1).copied().unwrap_or("?"));
        }
    }
    // GPT / OpenAI model names usually short already; pass through.
    model.to_string()
}

/// Returns (input, output, cache_creation, cache_read) pricing for either
/// Claude or Codex models.
pub(crate) fn model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    claude_model_pricing(model).or_else(|| codex_model_pricing(model))
}
