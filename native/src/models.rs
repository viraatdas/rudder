#![allow(unused_imports)]
//! Model/effort tables, the model picker, and suggestion ranking.
use super::*;

pub(crate) fn default_model_for(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "sonnet",
        Backend::Codex => "gpt-5.5",
        // opencode is a front-end for many providers and already has a configured
        // default; an empty model means "use it" rather than Rudder guessing a
        // provider the user may not even be authenticated for.
        Backend::Opencode => "",
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
        // No fast tier to select: opencode's speed is whatever model is configured.
        Backend::Opencode => "",
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
        "opencode" | "oc" => Some(Backend::Opencode),
        _ => None,
    }
}

/// Whether a bare token names a REAL model (alias or explicit id) across the
/// known tables. Unlike `backend_for_model`, which guesses a default backend
/// for anything, this refuses task words like "fix" or "refactor" so `/gam`
/// never eats the first word of someone's ask as a model id.
pub(crate) fn known_model_backend(model: &str) -> Option<Backend> {
    let query = model.trim();
    if query.is_empty() {
        return None;
    }
    for (backend, candidate, _) in fallback_model_rows() {
        if candidate.eq_ignore_ascii_case(query) {
            return Some(backend);
        }
    }
    maybe_refresh_models_dev_cache();
    for (backend, candidate, _) in cached_models_dev_rows() {
        if candidate.eq_ignore_ascii_case(query) {
            return Some(backend);
        }
    }
    for (backend, candidate, _) in cached_codex_local_rows() {
        if candidate.eq_ignore_ascii_case(query) {
            return Some(backend);
        }
    }
    // Provider-qualified opencode ids ("anthropic/claude-sonnet-4-5"): a slash
    // whose head is NOT one of our provider keywords.
    if query.contains('/')
        && provider_backend(query.split('/').next().unwrap_or_default()).is_none()
    {
        return Some(Backend::Opencode);
    }
    None
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
        (Backend::Opencode, Some(EffortLevel::Max)) => "not used",
    }
}

pub(crate) fn model_supports_reasoning(backend: Backend, model: &str) -> bool {
    if is_reasoning_alias(backend, model) {
        return true;
    }
    let heuristic = match backend {
        Backend::Claude => {
            model.contains("opus")
                || model.contains("sonnet")
                || model.contains("fable")
                || (model.starts_with("claude-")
                    && !model.contains("haiku")
                    && !model.contains("3-"))
        }
        Backend::Codex => {
            is_gpt_text_model(model) || model.contains("codex") || is_o_series_model(model)
        }
        // opencode exposes no reasoning-effort flag, whatever the underlying model
        // supports, so Rudder offers no effort choice rather than a dead one.
        Backend::Opencode => false,
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
        Backend::Codex => {
            is_gpt_text_model(model) || model.contains("codex") || is_o_series_model(model)
        }
        Backend::Opencode => false,
    }
}

pub(crate) fn cached_model_reasoning(backend: Backend, model_id: &str) -> Option<bool> {
    let cache_path = models_dev_cache_path()?;
    let raw = fs::read_to_string(cache_path).ok()?;
    let data = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let provider = match backend {
        Backend::Claude => "anthropic",
        Backend::Codex => "openai",
        // opencode ids already carry their provider ("anthropic/claude-sonnet-4-5");
        // there is no single models.dev provider to look them up under.
        Backend::Opencode => return None,
    };
    data.get(provider)?
        .get("models")?
        .get(model_id)?
        .get("reasoning")?
        .as_bool()
}

pub(crate) fn suggestions_for(app: &App) -> Vec<Suggestion> {
    // Esc dismissed the palette for exactly this input; stay hidden until the
    // user edits the draft (which makes it a different string again).
    if app.picker_dismissed_input.as_deref() == Some(app.task_input.as_str()) {
        return Vec::new();
    }
    let input = app.task_input.trim_start();
    if !input.starts_with('/') {
        return Vec::new();
    }

    if let Some(rest) = resume_command_rest(input) {
        return handoff_suggestions(app, rest);
    }

    if input.starts_with("/model") {
        let mut suggestions =
            model_provider_or_model_suggestions(input.strip_prefix("/model").unwrap_or_default());
        mark_current_model_suggestions(&mut suggestions, app.backend, &app.model, app.effort);
        return suggestions;
    }

    if input.starts_with("/gam") {
        // Bare "/gam" opens the reviewer picker, because choosing who argues
        // with the generator is the interesting decision here. What it must NOT
        // do is take the Enter key by surprise: the first row is "just type the
        // task", so the default selection keeps the line and naming a provider
        // is a deliberate arrow-down.
        //
        // Past that, /gam is a task command. The picker stays open only while
        // the first word is still a partial PROVIDER name, matched by strict
        // prefix, so an ordinary ask never turns back into a model chooser.
        let rest = input.strip_prefix("/gam").unwrap_or_default().trim_start();
        maybe_refresh_models_dev_cache();
        if rest.is_empty() {
            return gam_reviewer_suggestions(app.backend);
        }
        {
            let trailing_space = rest.ends_with(char::is_whitespace);
            let parts = rest.split_whitespace().collect::<Vec<_>>();
            return match parts.first().and_then(|provider| provider_backend(provider)) {
                // A named provider: offer its models, still filtering while the
                // model token is being typed. A trailing space after the model
                // means the rest of the line is task text, so the palette goes
                // away.
                Some(backend) => match parts.as_slice() {
                    [_provider] => gam_model_suggestions(backend, ""),
                    [_provider, model] if !trailing_space => {
                        gam_model_suggestions(backend, model)
                    }
                    _ => Vec::new(),
                },
                // Not a provider. Only a lone, still-being-typed first word can
                // be a provider the user has not finished spelling; a second
                // word proves the line is task text ("/gam clean up auth").
                None => match parts.as_slice() {
                    [candidate] if !trailing_space => gam_provider_prefix_suggestions(candidate),
                    _ => Vec::new(),
                },
            };
        }
    }
    rank_suggestions(command_suggestions(), input.trim_start_matches('/'))
}

/// `/resume` (and its `/handoff` alias) with whatever follows it, or None when this
/// input is some other command.
pub(crate) fn resume_command_rest(input: &str) -> Option<&str> {
    let input = input.trim_start();
    for command in RESUME_COMMANDS {
        if let Some(rest) = input.strip_prefix(command) {
            return Some(rest);
        }
    }
    None
}

/// `/resume` is the name; `/handoff` stays a working alias because it is what the
/// CLI half is called (`rudder handoff`) and what early users typed.
pub(crate) const RESUME_COMMANDS: [&str; 2] = ["/resume", "/handoff"];

/// The `/resume` drill-down: this repo's recent CLI conversations, newest first,
/// filtered by whatever the user typed after the command.
///
/// Reads only the cached list (`App::handoff_candidates`); the cache itself is
/// refilled on keystrokes, never here — this runs on every frame.
pub(crate) fn handoff_suggestions(app: &App, rest: &str) -> Vec<Suggestion> {
    let query = rest.trim();
    // Once a concrete session id is present the choice is made: hide the palette so
    // Enter submits the command (and any instruction typed after the id) as written.
    if crate::handoff::valid_session_id(query.split_whitespace().next().unwrap_or_default()) {
        return Vec::new();
    }
    if app.handoff_candidates.is_empty() {
        return vec![Suggestion {
            label: "/resume <session-id>".to_string(),
            detail: "no recent claude, codex, or opencode conversations for this repo".to_string(),
            action: SuggestionAction::Insert("/resume ".to_string()),
        }];
    }
    let now = std::time::SystemTime::now();
    let normalized = normalize_search_text(query);
    let mut ranked: Vec<(i32, Suggestion)> = Vec::new();
    for candidate in &app.handoff_candidates {
        // Score the TITLE only. Every row's detail shares the same handful of words
        // ("claude", "continue this chat"…), and the fuzzy matcher happily finds a
        // subsequence of any short query in them — scoring details would make every
        // conversation match every filter.
        let score = if normalized.is_empty() {
            0
        } else {
            match text_match_score(
                &normalize_search_text(&candidate.title),
                &normalized,
                10_000,
            ) {
                Some(score) => score,
                None => continue,
            }
        };
        ranked.push((
            score,
            Suggestion {
                label: truncate_chars(&candidate.title, 64),
                // Backend + MODEL + WHERE IT RAN + age + the id itself: enough to
                // tell two similar conversations apart, to see what the fork will
                // run on, and to know whether this chat was about the main
                // checkout or a subfolder before it is picked up somewhere else.
                detail: format!(
                    "{} · {}{}{} · {}",
                    candidate.backend.as_str(),
                    candidate
                        .model
                        .as_deref()
                        .map(|model| format!("{} · ", conversation_model_label(model)))
                        .unwrap_or_default(),
                    crate::handoff::origin_label(candidate.cwd.as_deref(), &app.cwd)
                        .map(|origin| format!("{origin} · "))
                        .unwrap_or_default(),
                    crate::handoff::relative_age(candidate.modified, now),
                    short_session_label(&candidate.session_id),
                ),
                // Insert rather than run: it leaves room to type the next step after
                // the id (`/resume <id> now write the tests`) before Enter.
                action: SuggestionAction::Insert(format!("/resume {} ", candidate.session_id)),
            },
        ));
    }
    // Ties keep the incoming order, which is newest-conversation-first.
    ranked.sort_by(|left, right| right.0.cmp(&left.0));
    ranked
        .into_iter()
        .map(|(_, suggestion)| suggestion)
        .collect()
}

/// The model a conversation ran on, shortened for a picker row: drop the vendor
/// prefix and the release stamp, keep the version ("claude-opus-4-5-20251101" ->
/// "opus-4-5"). Deliberately NOT `usage::short_model_label`, which truncates to the
/// major version ("opus-4") — fine for a cost table, lossy for choosing a chat.
pub(crate) fn conversation_model_label(model: &str) -> String {
    let model = model.trim();
    let trimmed = model.strip_prefix("claude-").unwrap_or(model);
    let undated = match trimmed.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|ch| ch.is_ascii_digit()) => head,
        _ => trimmed,
    };
    truncate_chars(undated, 22)
}

/// Session ids are unreadable in full; show the leading chunk that identifies one.
pub(crate) fn short_session_label(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

pub(crate) fn mark_current_model_suggestions(
    suggestions: &mut [Suggestion],
    current_backend: Backend,
    current_model: &str,
    current_effort: Option<EffortLevel>,
) {
    for suggestion in suggestions {
        if is_current_model_suggestion(suggestion, current_backend, current_model, current_effort)
            && !suggestion.detail.ends_with("· current")
        {
            suggestion.detail.push_str(" · current");
        }
    }
}

pub(crate) fn is_current_model_suggestion(
    suggestion: &Suggestion,
    current_backend: Backend,
    current_model: &str,
    current_effort: Option<EffortLevel>,
) -> bool {
    match &suggestion.action {
        SuggestionAction::ChooseModelProvider(backend) => *backend == current_backend,
        SuggestionAction::ChooseModel { backend, model } => {
            *backend == current_backend && model == current_model
        }
        SuggestionAction::SetModel {
            backend,
            model,
            effort,
        } => *backend == current_backend && model == current_model && *effort == current_effort,
        _ => false,
    }
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
            label: "/ask <text>".to_string(),
            detail: "one-off agent in the main checkout, no DAG".to_string(),
            action: SuggestionAction::Insert("/ask ".to_string()),
        },
        Suggestion {
            label: "/run <task>".to_string(),
            detail: "one isolated mergeable worker, no DAG".to_string(),
            action: SuggestionAction::Insert("/run ".to_string()),
        },
        Suggestion {
            label: "/run main <task>".to_string(),
            detail: "another agent in the main checkout (several may run at once)"
                .to_string(),
            action: SuggestionAction::Insert("/run main ".to_string()),
        },
        Suggestion {
            label: "/plan <text>".to_string(),
            detail: "orchestrator planner for DAG work (plain input runs one main agent instead)".to_string(),
            action: SuggestionAction::Insert("/plan ".to_string()),
        },
        Suggestion {
            label: "/gam <task>".to_string(),
            detail: "generator + adversarial reviewer pair: reviewer questions every round, only the generator edits".to_string(),
            action: SuggestionAction::Insert("/gam ".to_string()),
        },
        Suggestion {
            label: "/gam <model> <task>".to_string(),
            detail: "pick the adversarial reviewer's model inline (e.g. /gam codex gpt-5.5 fix auth)".to_string(),
            action: SuggestionAction::Insert("/gam ".to_string()),
        },
        Suggestion {
            label: "/resume".to_string(),
            detail:
                "pick a recent claude/codex/opencode chat and continue it as a worker, context intact"
                    .to_string(),
            action: SuggestionAction::Insert("/resume ".to_string()),
        },
        Suggestion {
            label: "/restore <claude|codex> <session-id>".to_string(),
            detail: "reopen an existing CLI conversation in a new pane, permissions bypassed".to_string(),
            action: SuggestionAction::Insert("/restore ".to_string()),
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
            detail: "review all completed workspaces before merge".to_string(),
            action: SuggestionAction::RunCommand("/review-all".to_string()),
        },
        Suggestion {
            label: "/merge-all".to_string(),
            detail: "merge all completed workspaces".to_string(),
            action: SuggestionAction::RunCommand("/merge-all".to_string()),
        },
        Suggestion {
            label: "/nudge <message>".to_string(),
            detail: "send one message to every in-flight worker (dead ones are resumed)"
                .to_string(),
            action: SuggestionAction::Insert("/nudge ".to_string()),
        },
        Suggestion {
            label: "/verify".to_string(),
            detail: "rerun the final all-integrated repository checks".to_string(),
            action: SuggestionAction::RunCommand("/verify".to_string()),
        },
        Suggestion {
            label: "/color terminal|paper".to_string(),
            detail: "pick dashboard color mode; terminal uses your terminal background"
                .to_string(),
            action: SuggestionAction::Insert("/color ".to_string()),
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
            label: "/feedback <what broke>".to_string(),
            detail: "send a report with version, model and recent notices — never your prompts or code"
                .to_string(),
            action: SuggestionAction::Insert("/feedback ".to_string()),
        },
        Suggestion {
            label: "/telemetry".to_string(),
            detail: "show or change anonymous usage events (status|on|off)".to_string(),
            action: SuggestionAction::Insert("/telemetry ".to_string()),
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
        (Backend::Opencode, "opencode models (provider/model)"),
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

/// The three provider rows of the `/gam` picker. Every label says what the
/// choice is FOR: this picks the model that argues with the generator, it does
/// not touch the dashboard's own backend or model.
fn gam_provider_rows() -> impl Iterator<Item = Suggestion> {
    [
        (Backend::Claude, "Claude Code"),
        (Backend::Codex, "Codex"),
        (Backend::Opencode, "opencode"),
    ]
    .into_iter()
    .map(|(backend, name)| Suggestion {
        label: backend.as_str().to_string(),
        detail: format!("review with {name} · pick its model next"),
        action: SuggestionAction::ChooseGamProvider(backend),
    })
}

/// What bare `/gam` shows: who is going to argue with the generator.
///
/// The FIRST row is the do-nothing path, so the default selection under the
/// cursor keeps whatever the user is typing. Enter used to land on a provider
/// here, which read as `/gam` silently changing models.
pub(crate) fn gam_reviewer_suggestions(default_backend: Backend) -> Vec<Suggestion> {
    let auto = cross_gam_backend(default_backend);
    let mut rows = vec![Suggestion {
        label: "/gam <task>".to_string(),
        detail: format!(
            "just type the task · {} reviews it by default",
            auto.as_str()
        ),
        action: SuggestionAction::Insert("/gam ".to_string()),
    }];
    rows.extend(gam_provider_rows());
    rows
}

/// Provider rows filtered by what has been typed so far.
///
/// STRICT prefix match, deliberately not the fuzzy ranker every other palette
/// uses. Fuzzy matching surfaced "codex" for "code" and a provider row for any
/// task word that happened to share letters, which turned the opening word of
/// an ordinary ask into a model picker. Here a row appears only when the user
/// is genuinely part-way through spelling that provider's name.
pub(crate) fn gam_provider_prefix_suggestions(query: &str) -> Vec<Suggestion> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    gam_provider_rows()
        .filter(|suggestion| suggestion.label.starts_with(&query))
        .collect()
}

/// The same model rows /model offers, rewritten to insert into a `/gam` line.
/// Effort is intentionally not offered here: it is derived from the chosen
/// model, and the rest of the line belongs to the task text.
pub(crate) fn gam_model_suggestions(backend_filter: Backend, query: &str) -> Vec<Suggestion> {
    model_suggestions_for(backend_filter, query)
        .into_iter()
        .map(|suggestion| match suggestion.action {
            SuggestionAction::ChooseModel { backend, model } => Suggestion {
                detail: format!("as the adversarial reviewer · {}", suggestion.detail),
                action: SuggestionAction::ChooseGamModel { backend, model },
                ..suggestion
            },
            _ => suggestion,
        })
        .collect()
}

pub(crate) fn model_suggestions_for(backend_filter: Backend, query: &str) -> Vec<Suggestion> {
    let mut seen = HashSet::new();
    let mut suggestions = Vec::new();

    // Friendly ALIASES first ("fable", "opus[1m]", …): they always track the
    // newest release of each family, which is how the claude CLI's own /model
    // picker leads. Explicit ids follow, newest-first from models.dev, for
    // pinning a specific release; the static id rows are the no-cache safety net.
    let (alias_rows, id_rows): (Vec<_>, Vec<_>) = fallback_model_rows()
        .into_iter()
        .partition(|(_, model, _)| !model.contains('-'));
    for (backend, model, detail) in alias_rows {
        if backend == backend_filter {
            push_model_suggestion(&mut suggestions, &mut seen, backend, model, detail);
        }
    }
    // Codex's own cache is the authoritative account-specific list and often
    // receives new model families before models.dev. Keep its native ordering.
    if backend_filter == Backend::Codex {
        for (backend, model, detail) in cached_codex_local_rows() {
            push_model_suggestion(&mut suggestions, &mut seen, backend, &model, &detail);
        }
    }
    if backend_filter == Backend::Opencode {
        maybe_refresh_opencode_models_cache();
        for (backend, model, detail) in cached_opencode_rows() {
            push_model_suggestion(&mut suggestions, &mut seen, backend, &model, &detail);
        }
    }
    for (backend, model, detail) in cached_models_dev_rows() {
        if backend == backend_filter {
            push_model_suggestion(&mut suggestions, &mut seen, backend, &model, &detail);
        }
    }
    for (backend, model, detail) in id_rows {
        if backend == backend_filter {
            push_model_suggestion(&mut suggestions, &mut seen, backend, model, detail);
        }
    }

    rank_suggestions(suggestions, query)
}

pub(crate) fn cached_codex_local_rows() -> Vec<(Backend, String, String)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(home.join(".codex").join("models_cache.json")) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    collect_codex_local_rows(&data)
}

pub(crate) fn collect_codex_local_rows(data: &serde_json::Value) -> Vec<(Backend, String, String)> {
    data.get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let slug = model.get("slug")?.as_str()?.trim();
            if slug.is_empty()
                || slug == "codex-auto-review"
                || model.get("visibility").and_then(serde_json::Value::as_str) == Some("hide")
                || is_excluded_openai_text_model(slug)
            {
                return None;
            }
            let display = model
                .get("display_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim();
            let description = model
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim();
            let detail = match (display.is_empty(), description.is_empty()) {
                (false, false) => format!("{display} · {description}"),
                (false, true) => display.to_string(),
                (true, false) => description.to_string(),
                (true, true) => "local Codex cache".to_string(),
            };
            Some((Backend::Codex, slug.to_string(), detail))
        })
        .collect()
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
        (Backend::Claude, "opus", "1M context · complex tasks"),
        (Backend::Claude, "fable", "hard, long-running tasks"),
        (Backend::Claude, "sonnet", "efficient routine tasks"),
        (Backend::Claude, "haiku", "fastest quick answers"),
        (Backend::Claude, "claude-sonnet-4-6", "explicit id"),
        (Backend::Codex, "gpt-5.6", "newest · needs API-key auth"),
        (Backend::Codex, "gpt-5.5", "latest · works on ChatGPT auth"),
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
            // opencode's own `opencode models` output is the authoritative list for
            // that backend (it knows which providers are authenticated).
            Backend::Opencode => false,
        })
        // A dated snapshot ("claude-opus-4-5-20251101") duplicates its un-dated
        // (latest) twin in the picker; list the twin only.
        .filter(|(id, _)| dated_duplicate_base(id).is_none_or(|base| !models.contains_key(&base)))
        .map(|(id, model)| {
            let name = model
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim();
            let release = model
                .get("release_date")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim();
            let context = model
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(serde_json::Value::as_u64)
                .map(format_context_window);
            let detail = [
                (!name.is_empty()).then(|| name.to_string()),
                context,
                (!release.is_empty()).then(|| release.to_string()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let detail = if detail.is_empty() {
                "models.dev".to_string()
            } else {
                detail
            };
            (id.clone(), detail, release.to_string())
        })
        .collect::<Vec<_>>();

    // NEWEST FIRST — the order the claude/codex CLIs themselves present models —
    // then a variant nudge among same-day siblings (codex-tuned up; nano/mini/
    // pro/chat down), then the family score, then id DESC so an un-dated tie
    // still leans newest instead of listing 4-1 above 4-8.
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| variant_score(backend, &b.0).cmp(&variant_score(backend, &a.0)))
            .then_with(|| score_model(backend, &b.0).cmp(&score_model(backend, &a.0)))
            .then_with(|| b.0.cmp(&a.0))
    });
    for (id, detail, _) in entries.into_iter().take(24) {
        rows.push((backend, id, detail));
    }
}

pub(crate) fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M context", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        format!("{}K context", tokens / 1_000)
    } else {
        format!("{tokens} context")
    }
}

/// For a dated snapshot id like "claude-opus-4-5-20251101", the un-dated base id
/// ("claude-opus-4-5"). None when the id carries no trailing -YYYYMMDD stamp.
pub(crate) fn dated_duplicate_base(id: &str) -> Option<String> {
    let (base, stamp) = id.rsplit_once('-')?;
    if stamp.len() == 8 && stamp.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(base.to_string());
    }
    None
}

/// Nudge among SAME-RELEASE-DAY siblings: coding-tuned ids float above their
/// general twin for the codex picker; nano/mini/pro/chat variants sink below
/// the base model (they are niche picks, not what /model reaches for).
pub(crate) fn variant_score(backend: Backend, id: &str) -> i32 {
    if backend == Backend::Claude {
        return 0;
    }
    let lower = id.to_ascii_lowercase();
    let mut score = 0;
    if lower.contains("codex") {
        score += 10;
    }
    if ["nano", "mini", "-pro", "chat"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        score -= 5;
    }
    score
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
        && !is_excluded_openai_text_model(id)
        && !id.contains("deep-research")
        && !id.contains("chat-latest")
        && (id.contains("codex") || is_gpt_text_model(id) || is_o_series_model(id))
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
            score += gpt_version_score(id);
            if is_o_series_model(id) {
                score += 25;
            }
            score
        }
        // opencode's own list is already ordered the way opencode presents it;
        // Rudder does not second-guess a catalog that spans every provider.
        Backend::Opencode => 0,
    }
}

/// Where `opencode models` output is cached for the picker. Running the binary on
/// the render path is not an option, so the list is refreshed in the background.
pub(crate) fn opencode_models_cache_path() -> Option<PathBuf> {
    std::env::var_os("RUDDER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rudder")))
        .map(|home| home.join("opencode-models.txt"))
}

/// One detached `opencode models` refresh per session when the cache is missing or
/// a day old. Mirrors `maybe_refresh_models_dev_cache`: the picker re-reads the
/// file every render, so the list fills in live once the refresh lands.
fn maybe_refresh_opencode_models_cache() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REFRESH_STARTED: AtomicBool = AtomicBool::new(false);
    if cfg!(test) {
        return;
    }
    let Some(path) = opencode_models_cache_path() else {
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
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Write to a temp and rename so a half-written list is never read.
    let script = format!(
        "{program} models > '{path}'.tmp 2>/dev/null && mv -f '{path}'.tmp '{path}'",
        program = opencode_program(),
        path = path.display()
    );
    if let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// The models `opencode models` reported: `provider/model` per line. That command
/// knows which providers the user is actually authenticated for, which no static
/// table could.
pub(crate) fn cached_opencode_rows() -> Vec<(Backend, String, String)> {
    let Some(path) = opencode_models_cache_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_opencode_model_list(&raw)
}

pub(crate) fn parse_opencode_model_list(raw: &str) -> Vec<(Backend, String, String)> {
    raw.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && line.contains('/')
                && !line.contains(char::is_whitespace)
                && line
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "/-_.:[]".contains(ch))
        })
        .map(|line| {
            let provider = line.split('/').next().unwrap_or_default().to_string();
            (Backend::Opencode, line.to_string(), provider)
        })
        .collect()
}

pub(crate) fn is_excluded_openai_text_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    [
        "embedding",
        "image",
        "audio",
        "tts",
        "whisper",
        "transcribe",
        "translation",
        "moderation",
        "rerank",
        "realtime",
        "dall-e",
        "search",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn is_gpt_text_model(id: &str) -> bool {
    id.strip_prefix("gpt-")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|ch| ch.is_ascii_digit())
}

pub(crate) fn is_o_series_model(id: &str) -> bool {
    id.strip_prefix('o')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|ch| ch.is_ascii_digit())
}

pub(crate) fn gpt_version_score(id: &str) -> i32 {
    let Some(rest) = id.strip_prefix("gpt-") else {
        return 0;
    };
    let mut major = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            major.push(ch);
            chars.next();
        } else {
            break;
        }
    }
    if major.is_empty() {
        return 0;
    }
    let mut minor = String::new();
    if matches!(chars.peek(), Some('.') | Some('-')) {
        chars.next();
        while let Some(ch) = chars.peek().copied() {
            if ch.is_ascii_digit() {
                minor.push(ch);
                chars.next();
            } else {
                break;
            }
        }
    }
    let major = major.parse::<i32>().unwrap_or(0);
    let minor = minor.parse::<i32>().unwrap_or(0);
    20 + major * 20 + minor
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
    // A provider-qualified id ("anthropic/claude-sonnet-4-5") is opencode's shape;
    // neither claude nor codex names a model with a slash.
    if model.contains('/') {
        return Backend::Opencode;
    }
    if model.starts_with("gpt-") || model.contains("codex") || is_o_series_model(model) {
        Backend::Codex
    } else {
        Backend::Claude
    }
}
