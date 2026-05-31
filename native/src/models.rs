#![allow(unused_imports)]
//! Model/effort tables, the model picker, and suggestion ranking.
use super::*;

pub(crate) fn default_model_for(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "sonnet",
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
    cached_model_reasoning(backend, model).unwrap_or_else(|| match backend {
        Backend::Claude => model.contains("opus") || model.contains("sonnet"),
        Backend::Codex => model.starts_with("gpt-5") || model.contains("codex"),
    })
}

pub(crate) fn is_reasoning_alias(backend: Backend, model: &str) -> bool {
    match backend {
        Backend::Claude => matches!(model, "sonnet" | "sonnet[1m]" | "opus" | "opus[1m]"),
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

pub(crate) fn model_provider_or_model_suggestions(rest: &str) -> Vec<Suggestion> {
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

pub(crate) fn effort_suggestions_for(backend: Backend, model: &str, query: &str) -> Vec<Suggestion> {
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
    id.starts_with("claude-")
        && !id.contains("3-")
        && (id.contains("sonnet") || id.contains("opus") || id.contains("haiku"))
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
            let mut score = 0;
            if id.contains("sonnet") {
                score += 40;
            }
            if id.contains("opus") {
                score += 35;
            }
            if id.contains("haiku") {
                score += 20;
            }
            score
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
    std::env::var_os("RUDDER_HOME")
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

