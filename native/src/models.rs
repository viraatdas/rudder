#![allow(unused_imports)]
//! Model/effort tables, the model picker, and suggestion ranking.
use super::*;

pub(crate) fn default_model_for(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "sonnet",
        Backend::Codex => "gpt-5.5",
    }
}

/// The model `/fast` selects per backend.
///
/// Claude: fast mode is Opus on an accelerated API config (NOT a different or
/// smaller model, and NOT an effort change), so `/fast` selects `opus` and enables
/// the native `fastMode` settings flag — see the `/fast` handler and
/// `signals::claude_settings_json`. Effort is left untouched.
///
/// Codex: has no native fast mode, so the closest equivalent is its flagship run
/// at low reasoning effort (the effort is applied by the `/fast` handler, not here).
pub(crate) fn fast_model_for(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "opus",
        Backend::Codex => "gpt-5.5",
    }
}

pub(crate) fn default_effort_for(backend: Backend, model: &str) -> Option<EffortLevel> {
    let options = effort_options_for(backend, model);
    if options.contains(&Some(EffortLevel::XHigh)) {
        Some(EffortLevel::XHigh)
    } else {
        options.into_iter().next().flatten()
    }
}

pub(crate) fn effort_label(effort: Option<EffortLevel>) -> &'static str {
    effort.map(EffortLevel::as_str).unwrap_or("auto")
}

pub(crate) fn parse_effort_arg(value: &str) -> Option<EffortLevel> {
    if value.eq_ignore_ascii_case("auto") {
        None
    } else {
        EffortLevel::parse(value)
    }
}

pub(crate) fn provider_backend(provider: &str) -> Option<Backend> {
    match provider.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some(Backend::Claude),
        "codex" | "openai" => Some(Backend::Codex),
        _ => None,
    }
}

pub(crate) fn effort_options_for(backend: Backend, model: &str) -> Vec<Option<EffortLevel>> {
    if !model_supports_reasoning(backend, model) {
        return vec![None];
    }

    let mut options = vec![
        None,
        Some(EffortLevel::Low),
        Some(EffortLevel::Medium),
        Some(EffortLevel::High),
        Some(EffortLevel::XHigh),
    ];
    if backend == Backend::Claude {
        options.push(Some(EffortLevel::Max));
    }
    options
}

pub(crate) fn effort_detail(backend: Backend, effort: Option<EffortLevel>) -> &'static str {
    match (backend, effort) {
        (_, None) => "let the agent decide",
        (_, Some(EffortLevel::Low)) => "fastest",
        (_, Some(EffortLevel::Medium)) => "balanced",
        (_, Some(EffortLevel::High)) => "deeper reasoning",
        (_, Some(EffortLevel::XHigh)) => "extended reasoning",
        (Backend::Claude, Some(EffortLevel::Max)) => "maximum reasoning",
        (Backend::Codex, Some(EffortLevel::Max)) => "not used",
    }
}

pub(crate) fn model_supports_reasoning(backend: Backend, model: &str) -> bool {
    if is_reasoning_alias(backend, model) {
        return true;
    }
    let heuristic = match backend {
        Backend::Claude => {
            model.contains("opus") || model.contains("sonnet") || model.contains("fable")
        }
        Backend::Codex => model.starts_with("gpt-5") || model.contains("codex"),
    };
    // OR the heuristic with the cache rather than letting the cache override it: a
    // stale or wrong models.dev entry (`reasoning: false`) must never REMOVE the
    // High/XHigh/Max effort options from a family we know reasons. The cache can
    // still ADD reasoning for ids the heuristic doesn't recognize.
    heuristic || cached_model_reasoning(backend, model).unwrap_or(false)
}

pub(crate) fn is_reasoning_alias(backend: Backend, model: &str) -> bool {
    match backend {
        Backend::Claude => matches!(
            model,
            "sonnet" | "sonnet[1m]" | "opus" | "opus[1m]" | "fable" | "fable[1m]"
        ),
        Backend::Codex => model.starts_with("gpt-5") || model.contains("codex"),
    }
}

pub(crate) fn cached_model_reasoning(backend: Backend, model_id: &str) -> Option<bool> {
    let cache_path = models_dev_cache_path()?;
    let raw = fs::read_to_string(cache_path).ok()?;
    let data = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let provider = match backend {
        Backend::Claude => "anthropic",
        Backend::Codex => "openai",
    };
    data.get(provider)?
        .get("models")?
        .get(model_id)?
        .get("reasoning")?
        .as_bool()
}

pub(crate) fn suggestions_for(app: &App) -> Vec<Suggestion> {
    let input = app.task_input.trim_start();
    if !input.starts_with('/') {
        return Vec::new();
    }

    if input.starts_with("/model") {
        return model_provider_or_model_suggestions(
            input.strip_prefix("/model").unwrap_or_default(),
        );
    }

    rank_suggestions(command_suggestions(), input.trim_start_matches('/'))
}

pub(crate) fn rank_suggestions(suggestions: Vec<Suggestion>, query: &str) -> Vec<Suggestion> {
    let query = normalize_search_text(query);
    if query.is_empty() {
        return suggestions;
    }

    let mut ranked = suggestions
        .into_iter()
        .enumerate()
        .filter_map(|(index, suggestion)| {
            suggestion_match_score(&suggestion, &query).map(|score| (index, score, suggestion))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
        .into_iter()
        .map(|(_, _, suggestion)| suggestion)
        .collect()
}

pub(crate) fn suggestion_match_score(suggestion: &Suggestion, query: &str) -> Option<i32> {
    let label = normalize_search_text(&suggestion.label);
    let detail = normalize_search_text(&suggestion.detail);
    [
        text_match_score(&label, query, 10_000),
        text_match_score(&detail, query, 3_000),
    ]
    .into_iter()
    .flatten()
    .max()
}

pub(crate) fn text_match_score(text: &str, query: &str, base: i32) -> Option<i32> {
    if text.is_empty() || query.is_empty() {
        return None;
    }
    if text == query {
        return Some(base + 6_000);
    }
    if text.starts_with(query) {
        return Some(base + 5_000 - text.len() as i32);
    }
    if let Some(position) = text.find(query) {
        return Some(base + 4_000 - (position as i32 * 10) - text.len() as i32);
    }
    if let Some(score) = token_prefix_score(text, query) {
        return Some(base + 3_500 + score);
    }
    if let Some(score) = fuzzy_subsequence_score(text, query) {
        return Some(base + 2_500 + score);
    }

    let distance = bounded_edit_distance(text, query, 3);
    let threshold = ((query.chars().count() + 2) / 3).clamp(1, 3);
    if distance <= threshold {
        return Some(base + 1_500 - (distance as i32 * 200) - text.len() as i32);
    }
    None
}

pub(crate) fn normalize_search_text(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn token_prefix_score(text: &str, query: &str) -> Option<i32> {
    let mut tokens = text.split_whitespace();
    let mut score = 0;
    for part in query.split_whitespace() {
        let token = tokens.find(|token| token.starts_with(part))?;
        score += 200 - token.len() as i32;
    }
    Some(score)
}

pub(crate) fn fuzzy_subsequence_score(text: &str, query: &str) -> Option<i32> {
    let query_chars = query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<Vec<_>>();
    if query_chars.is_empty() {
        return None;
    }

    let mut positions = Vec::with_capacity(query_chars.len());
    let mut text_iter = text.char_indices();
    for query_char in query_chars {
        let (position, _) = text_iter.find(|(_, text_char)| *text_char == query_char)?;
        positions.push(position as i32);
    }

    let first = *positions.first().unwrap_or(&0);
    let last = *positions.last().unwrap_or(&first);
    let compactness = 800 - (last - first).max(0) * 20;
    let early = 300 - first * 15;
    Some(compactness + early)
}

pub(crate) fn bounded_edit_distance(left: &str, right: &str, max_distance: usize) -> usize {
    let left = left.chars().take(40).collect::<Vec<_>>();
    let right = right.chars().take(40).collect::<Vec<_>>();
    if left.len().abs_diff(right.len()) > max_distance {
        return max_distance + 1;
    }

    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, right_char) in right.iter().enumerate() {
            let substitution_cost = usize::from(left_char != right_char);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + substitution_cost);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > max_distance {
            return max_distance + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

pub(crate) fn command_suggestions() -> Vec<Suggestion> {
    vec![
        Suggestion {
            label: "/model".to_string(),
            detail: "pick Claude or Codex model".to_string(),
            action: SuggestionAction::Insert("/model ".to_string()),
        },
        Suggestion {
            label: "/fast".to_string(),
            detail: "fast mode for new agents: flagship model at low effort (claude opus / codex gpt-5.5)"
                .to_string(),
            action: SuggestionAction::RunCommand("/fast".to_string()),
        },
        Suggestion {
            label: "/main".to_string(),
            detail: "spawn a main-branch agent (uses current model)".to_string(),
            action: SuggestionAction::RunCommand("/main".to_string()),
        },
        Suggestion {
            label: "/m".to_string(),
            detail: "alias for /main".to_string(),
            action: SuggestionAction::RunCommand("/m".to_string()),
        },
        Suggestion {
            label: "/main <prompt>".to_string(),
            detail: "main-branch agent with a custom first prompt".to_string(),
            action: SuggestionAction::Insert("/main ".to_string()),
        },
        Suggestion {
            label: "/m <prompt>".to_string(),
            detail: "alias for /main <prompt>".to_string(),
            action: SuggestionAction::Insert("/m ".to_string()),
        },
        Suggestion {
            label: "/goal <text>".to_string(),
            detail: "forward /goal to the focused agent (claude/codex)".to_string(),
            action: SuggestionAction::Insert("/goal ".to_string()),
        },
        Suggestion {
            label: "/review-all".to_string(),
            detail: "review all completed worktrees before merge".to_string(),
            action: SuggestionAction::RunCommand("/review-all".to_string()),
        },
        Suggestion {
            label: "/merge-all".to_string(),
            detail: "merge all completed worktrees".to_string(),
            action: SuggestionAction::RunCommand("/merge-all".to_string()),
        },
        Suggestion {
            label: "/automerge".to_string(),
            detail: "toggle auto-merge: clean finished nodes merge + unblock children".to_string(),
            action: SuggestionAction::RunCommand("/automerge".to_string()),
        },
        Suggestion {
            label: "/usage".to_string(),
            detail: "show tokens and estimated cost per model".to_string(),
            action: SuggestionAction::RunCommand("/usage".to_string()),
        },
        Suggestion {
            label: "/login".to_string(),
            detail: "authenticate Rudder Cloud in the browser".to_string(),
            action: SuggestionAction::RunCommand("/login".to_string()),
        },
        Suggestion {
            label: "/cloud".to_string(),
            detail: "onload this Rudder workspace or start scratch in Fly".to_string(),
            action: SuggestionAction::RunCommand("/cloud".to_string()),
        },
        Suggestion {
            label: "/cloud list".to_string(),
            detail: "list cloud workers".to_string(),
            action: SuggestionAction::RunCommand("/cloud list".to_string()),
        },
        Suggestion {
            label: "/cloud byoc".to_string(),
            detail: "bring your own computer for cloud workers".to_string(),
            action: SuggestionAction::RunCommand("/cloud byoc".to_string()),
        },
        Suggestion {
            label: "/help".to_string(),
            detail: "show shortcuts".to_string(),
            action: SuggestionAction::ShowHelp,
        },
    ]
}

/// Keep the model list dynamic for LONG-LIVED sessions: the models.dev cache
/// refreshes at dashboard launch, but a dashboard left open for days would
/// otherwise never see a model released mid-session. Opening the /model picker
/// on a stale cache (>24h) fires one detached `rudder __refresh-models`;
/// `cached_models_dev_rows` re-reads the file on every render, so the picker
/// updates live the moment the refresh lands. At most one spawn per session.
fn maybe_refresh_models_dev_cache() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REFRESH_STARTED: AtomicBool = AtomicBool::new(false);
    if cfg!(test) {
        return;
    }
    let Some(path) = models_dev_cache_path() else {
        return;
    };
    let stale = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|age| age > Duration::from_secs(24 * 60 * 60))
        .unwrap_or(true);
    if !stale || REFRESH_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(rudder) = locate_rudder_cli() else {
        return;
    };
    if let Ok(mut child) = Command::new(rudder)
        .arg("__refresh-models")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap off-thread so the child never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub(crate) fn model_provider_or_model_suggestions(rest: &str) -> Vec<Suggestion> {
    maybe_refresh_models_dev_cache();
    let rest = rest.trim_start();
    if rest.is_empty() {
        return provider_suggestions("");
    }

    let trailing_space = rest.ends_with(char::is_whitespace);
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    let Some(backend) = parts
        .first()
        .and_then(|provider| provider_backend(provider))
    else {
        return provider_suggestions(parts.first().copied().unwrap_or_default());
    };

    match parts.as_slice() {
        [provider] if !trailing_space => provider_suggestions(provider),
        [_provider] => model_suggestions_for(backend, ""),
        [_provider, model] if trailing_space => effort_suggestions_for(backend, model, ""),
        [_provider, model] => model_suggestions_for(backend, model),
        [_provider, model, effort_query, ..] => {
            effort_suggestions_for(backend, model, effort_query)
        }
        _ => provider_suggestions(""),
    }
}

pub(crate) fn provider_suggestions(query: &str) -> Vec<Suggestion> {
    let suggestions = [
        (Backend::Claude, "Claude Code models"),
        (Backend::Codex, "Codex models"),
    ]
    .into_iter()
    .map(|(backend, detail)| Suggestion {
        label: backend.as_str().to_string(),
        detail: detail.to_string(),
        action: SuggestionAction::ChooseModelProvider(backend),
    })
    .collect();
    rank_suggestions(suggestions, query)
}

pub(crate) fn model_suggestions_for(backend_filter: Backend, query: &str) -> Vec<Suggestion> {
    let mut seen = HashSet::new();
    let mut suggestions = Vec::new();

    for (backend, model, detail) in fallback_model_rows() {
        if backend == backend_filter {
            push_model_suggestion(&mut suggestions, &mut seen, backend, model, detail);
        }
    }
    for (backend, model, detail) in cached_models_dev_rows() {
        if backend == backend_filter {
            push_model_suggestion(&mut suggestions, &mut seen, backend, &model, &detail);
        }
    }

    rank_suggestions(suggestions, query)
}

pub(crate) fn effort_suggestions_for(
    backend: Backend,
    model: &str,
    query: &str,
) -> Vec<Suggestion> {
    let suggestions = effort_options_for(backend, model)
        .into_iter()
        .map(|effort| {
            let label = effort_label(effort).to_string();
            Suggestion {
                detail: effort_detail(backend, effort).to_string(),
                label,
                action: SuggestionAction::SetModel {
                    backend,
                    model: model.to_string(),
                    effort,
                },
            }
        })
        .collect();
    rank_suggestions(suggestions, query)
}

pub(crate) fn fallback_model_rows() -> Vec<(Backend, &'static str, &'static str)> {
    vec![
        (Backend::Claude, "fable", "most capable model"),
        (Backend::Claude, "fable[1m]", "most capable · large context"),
        (Backend::Claude, "sonnet", "default strong model"),
        (Backend::Claude, "sonnet[1m]", "large context"),
        (Backend::Claude, "opus", "strongest reasoning"),
        (Backend::Claude, "opus[1m]", "large context"),
        (Backend::Claude, "haiku", "fast model"),
        (Backend::Claude, "claude-sonnet-4-6", "explicit id"),
        (Backend::Codex, "gpt-5.5", "latest"),
        (Backend::Codex, "gpt-5.4-codex", "coding"),
        (Backend::Codex, "gpt-5.4", "general"),
        (Backend::Codex, "gpt-5.3-codex", "coding"),
        (Backend::Codex, "gpt-5.3-codex-spark", "fast"),
    ]
}

pub(crate) fn push_model_suggestion(
    suggestions: &mut Vec<Suggestion>,
    seen: &mut HashSet<String>,
    backend: Backend,
    model: &str,
    detail: &str,
) {
    let key = format!("{}:{model}", backend.as_str());
    if !seen.insert(key) {
        return;
    }
    suggestions.push(Suggestion {
        label: model.to_string(),
        detail: detail.to_string(),
        action: SuggestionAction::ChooseModel {
            backend,
            model: model.to_string(),
        },
    });
}

pub(crate) fn cached_models_dev_rows() -> Vec<(Backend, String, String)> {
    let Some(cache_path) = models_dev_cache_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(cache_path) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    collect_provider_models(&data, "anthropic", Backend::Claude, &mut rows);
    collect_provider_models(&data, "openai", Backend::Codex, &mut rows);
    rows
}

pub(crate) fn collect_provider_models(
    data: &serde_json::Value,
    provider: &str,
    backend: Backend,
    rows: &mut Vec<(Backend, String, String)>,
) {
    let Some(models) = data
        .get(provider)
        .and_then(|provider| provider.get("models"))
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };

    let mut entries = models
        .iter()
        .filter(|(id, model)| match backend {
            Backend::Claude => is_claude_picker_model(id, model),
            Backend::Codex => is_codex_picker_model(id, model),
        })
        .map(|(id, model)| {
            let detail = model
                .get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    model
                        .get("release_date")
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("models.dev")
                .to_string();
            (id.clone(), detail)
        })
        .collect::<Vec<_>>();

    entries.sort_by(|a, b| score_model(backend, &b.0).cmp(&score_model(backend, &a.0)));
    for (id, detail) in entries.into_iter().take(8) {
        rows.push((backend, id, detail));
    }
}

pub(crate) fn is_claude_picker_model(id: &str, model: &serde_json::Value) -> bool {
    // No family-name allowlist: a brand-new model family (fable, and whatever
    // comes after it) must show up in the picker straight from the models.dev
    // cache, without a Rudder release. Only legacy 3.x-generation ids are
    // excluded.
    id.starts_with("claude-")
        && !id.contains("3-")
        && model
            .get("tool_call")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
}

pub(crate) fn is_codex_picker_model(id: &str, model: &serde_json::Value) -> bool {
    let text_output = model
        .get("modalities")
        .and_then(|modalities| modalities.get("output"))
        .and_then(serde_json::Value::as_array)
        .is_none_or(|output| output.iter().any(|value| value.as_str() == Some("text")));
    text_output
        && !id.contains("deep-research")
        && !id.contains("chat-latest")
        && !id.contains("pro")
        && (id.contains("codex") || id.starts_with("gpt-5"))
}

pub(crate) fn score_model(backend: Backend, id: &str) -> i32 {
    match backend {
        Backend::Claude => {
            if id.contains("fable") {
                50
            } else if id.contains("sonnet") {
                40
            } else if id.contains("opus") {
                35
            } else if id.contains("haiku") {
                20
            } else {
                // Unknown family = likely a NEW tier; rank above haiku rather
                // than scoring zero and falling off the top-8 cut.
                30
            }
        }
        Backend::Codex => {
            let mut score = 0;
            if id.contains("codex") {
                score += 40;
            }
            if id.starts_with("gpt-5.5") {
                score += 35;
            }
            if id.starts_with("gpt-5.4") {
                score += 30;
            }
            score
        }
    }
}

pub(crate) fn models_dev_cache_path() -> Option<PathBuf> {
    // Treat an EMPTY RUDDER_HOME as unset (matching `config.rs`), or the cache would
    // resolve to a cwd-relative `models-dev.json` and split from the config it pairs with.
    std::env::var_os("RUDDER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rudder")))
        .map(|home| home.join("models-dev.json"))
}

pub(crate) fn backend_for_model(model: &str) -> Backend {
    if model.starts_with("gpt-") || model.contains("codex") {
        Backend::Codex
    } else {
        Backend::Claude
    }
}
