#![allow(unused_imports)]
//! Reading and writing ~/.rudder config, model defaults, and update notices.
use super::*;

pub(crate) fn load_rudder_config() -> Option<serde_json::Value> {
    let path = rudder_config_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Load the config for a mutating write without ever silently discarding a
/// user's settings.
///
/// `load_rudder_config` collapses both "no file" and "file present but
/// unparseable" into `None`. If a saver treated that `None` as "start from
/// defaults", a single transiently-corrupt file (an external editor mid-save,
/// a partial write from an older build) would be overwritten on the next
/// `/model` or another dashboard setting, wiping `backends`, `mergeStrategy`,
/// `orchestrator.maxParallel`, etc. Here we distinguish the two cases: a
/// present-but-unparseable file is copied aside to `config.json.corrupt` so the
/// user can recover it, and only then do we fall back to defaults.
fn load_config_for_write(path: &Path) -> serde_json::Value {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => value,
            Err(_) => {
                if !raw.trim().is_empty() {
                    let backup = path.with_extension("json.corrupt");
                    let _ = fs::write(&backup, &raw);
                }
                default_config_value()
            }
        },
        // Absent (or unreadable) file: a fresh defaults document is correct.
        Err(_) => default_config_value(),
    }
}

pub(crate) fn config_backend(config: &serde_json::Value) -> Option<Backend> {
    config
        .get("lastUsedBackend")
        .and_then(serde_json::Value::as_str)
        .and_then(provider_backend)
        .or_else(|| {
            config
                .get("defaultBackend")
                .and_then(serde_json::Value::as_str)
                .and_then(provider_backend)
        })
}

pub(crate) fn config_model(config: &serde_json::Value, backend: Backend) -> Option<String> {
    config
        .get("backends")?
        .get(backend.as_str())?
        .get("model")?
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .map(ToString::to_string)
}

pub(crate) fn config_effort(config: &serde_json::Value, backend: Backend) -> Option<EffortLevel> {
    let backend_config = config.get("backends")?.get(backend.as_str())?;
    let keys: &[&str] = match backend {
        Backend::Claude => &["effort", "reasoningEffort"],
        Backend::Codex => &["reasoningEffort", "effort"],
        // opencode has no effort flag; read the keys anyway so a config written by
        // hand is not silently ignored if opencode ever gains one.
        Backend::Opencode => &["effort", "reasoningEffort"],
    };
    keys.iter().find_map(|key| {
        backend_config
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .and_then(EffortLevel::parse)
    })
}

/// Whether Claude Code's native fast mode is on for new agents. Stored at the
/// config root as `fastMode` (the same key Claude Code's own settings use), so it
/// survives a restart and so we can inject it into a worker's `--settings`. Fast
/// mode is Opus with an accelerated, higher-cost API config — identical model and
/// reasoning, just lower latency — NOT an effort downgrade. Claude-only.
pub(crate) fn config_fast_mode(config: &serde_json::Value) -> Option<bool> {
    config.get("fastMode").and_then(serde_json::Value::as_bool)
}

pub(crate) fn fast_mode_enabled() -> bool {
    load_rudder_config()
        .as_ref()
        .and_then(config_fast_mode)
        .unwrap_or(false)
}

/// Whether Rudder should play a local completion sound when a worker enters review.
/// Default is OFF: audio is useful for some long-running sessions, but surprising
/// as an unconditional dashboard behavior.
pub(crate) fn config_completion_sound(config: &serde_json::Value) -> Option<bool> {
    config
        .get("completionSound")
        .and_then(serde_json::Value::as_bool)
}

pub(crate) fn completion_sound_enabled() -> bool {
    load_rudder_config()
        .as_ref()
        .and_then(config_completion_sound)
        .unwrap_or(false)
}

/// Dashboard color mode. `terminal` is the default so the native TUI leaves the
/// terminal foreground/background alone and blends with the surrounding tab bar.
/// `paper` preserves the previous hard-white canvas.
pub(crate) fn config_color_mode(config: &serde_json::Value) -> Option<ColorMode> {
    config
        .get("colorMode")
        .and_then(serde_json::Value::as_str)
        .and_then(ColorMode::parse)
}

pub(crate) fn initial_color_mode() -> ColorMode {
    std::env::var("RUDDER_COLOR_MODE")
        .ok()
        .and_then(|value| ColorMode::parse(&value))
        .or_else(|| load_rudder_config().as_ref().and_then(config_color_mode))
        .unwrap_or(ColorMode::Terminal)
}

/// Persist the `/sound` toggle so the next session keeps the same audio behavior.
pub(crate) fn save_completion_sound(enabled: bool) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "completionSound".to_string(),
        serde_json::Value::Bool(enabled),
    );
    write_config_atomically(&path, &config)
}

/// Persist the dashboard color mode so the next session keeps matching the
/// user's terminal background unless they explicitly pick the old paper canvas.
pub(crate) fn save_color_mode(mode: ColorMode) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "colorMode".to_string(),
        serde_json::Value::String(mode.as_str().to_string()),
    );
    write_config_atomically(&path, &config)
}

/// Persist the `/fast` toggle so new sessions and freshly-launched workers see it.
pub(crate) fn save_fast_mode(enabled: bool) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);
    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert("fastMode".to_string(), serde_json::Value::Bool(enabled));
    write_config_atomically(&path, &config)
}

/// Atomic temp-file + rename write shared by every config-mutating helper, so a
/// crash mid-write can never truncate ~/.rudder/config.json.
fn write_config_atomically(path: &Path, config: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(config)?),
    )?;
    fs::rename(&temp, path)?;
    set_private_file_mode(path);
    Ok(())
}

/// Read `orchestrator.maxParallel` from the config, clamped to a sane range.
/// Falls back to `DEFAULT_MAX_PARALLEL` when unset or out of range.
pub(crate) fn config_max_parallel(config: &serde_json::Value) -> usize {
    config
        .get("orchestrator")
        .and_then(|orchestrator| orchestrator.get("maxParallel"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| (1..=100_000).contains(value))
        .unwrap_or(DEFAULT_MAX_PARALLEL)
}

/// The plan-orchestration parallelism cap for this session.
pub(crate) fn max_parallel() -> usize {
    load_rudder_config()
        .as_ref()
        .map(config_max_parallel)
        .unwrap_or(DEFAULT_MAX_PARALLEL)
}

pub(crate) fn save_model_defaults(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_config_for_write(&path);
    if !config.is_object() {
        config = default_config_value();
    }
    ensure_config_defaults(&mut config);

    let root = config
        .as_object_mut()
        .context("Rudder config root is not an object")?;
    root.insert(
        "lastUsedBackend".to_string(),
        serde_json::Value::String(backend.as_str().to_string()),
    );
    let backends = root
        .entry("backends".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !backends.is_object() {
        *backends = serde_json::json!({});
    }
    let backends = backends
        .as_object_mut()
        .context("Rudder backends config is not an object")?;
    let backend_config = backends
        .entry(backend.as_str().to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !backend_config.is_object() {
        *backend_config = serde_json::json!({});
    }
    let backend_config = backend_config
        .as_object_mut()
        .context("Rudder backend config is not an object")?;
    if model.trim().is_empty() {
        backend_config.remove("model");
    } else {
        backend_config.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    match backend {
        Backend::Claude => {
            if let Some(effort) = effort {
                backend_config.insert(
                    "effort".to_string(),
                    serde_json::Value::String(effort.as_str().to_string()),
                );
            } else {
                backend_config.remove("effort");
            }
        }
        Backend::Codex => {
            if let Some(effort) = effort {
                backend_config.insert(
                    "reasoningEffort".to_string(),
                    serde_json::Value::String(effort.as_str().to_string()),
                );
            } else {
                backend_config.remove("reasoningEffort");
            }
        }
        Backend::Opencode => {
            backend_config.remove("effort");
            backend_config.remove("reasoningEffort");
        }
    }

    write_config_atomically(&path, &config)
}

pub(crate) fn ensure_config_defaults(config: &mut serde_json::Value) {
    let Some(root) = config.as_object_mut() else {
        return;
    };
    root.remove("autoMerge");
    root.entry("version".to_string())
        .or_insert(serde_json::json!(1));
    root.entry("defaultBackend".to_string())
        .or_insert(serde_json::json!("claude"));
    root.entry("mergeStrategy".to_string())
        .or_insert(serde_json::json!("merge"));
    root.entry("colorMode".to_string())
        .or_insert(serde_json::json!("terminal"));
    root.entry("runPolicy".to_string()).or_insert_with(|| {
        serde_json::json!({
            "sameCheckout": "single-active",
            "concurrentPromptMode": "worktree"
        })
    });
    root.entry("acpx".to_string())
        .or_insert_with(|| serde_json::json!({ "install": "latest" }));
    root.entry("backends".to_string())
        .or_insert_with(|| serde_json::json!({}));
}

pub(crate) fn default_config_value() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "defaultBackend": "claude",
        "mergeStrategy": "merge",
        "colorMode": "terminal",
        "runPolicy": {
            "sameCheckout": "single-active",
            "concurrentPromptMode": "worktree"
        },
        "acpx": { "install": "latest" },
        "backends": {
            "claude": {
                "profileId": "anthropic:claude-code",
                "model": "sonnet"
            },
            "codex": {
                "profileId": "openai-codex:default",
                "model": "gpt-5.5"
            },
            "acpx": {
                "model": "gpt-5.5"
            }
        }
    })
}

pub(crate) fn rudder_config_path() -> Option<PathBuf> {
    let home = if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            value
        } else {
            user_home_dir()?.join(".rudder")
        }
    } else {
        user_home_dir()?.join(".rudder")
    };
    Some(home.join("config.json"))
}

pub(crate) fn rudder_cloud_auth_path() -> Option<PathBuf> {
    let home = if let Some(value) = std::env::var_os("RUDDER_HOME") {
        let value = PathBuf::from(value);
        if !value.as_os_str().is_empty() {
            value
        } else {
            user_home_dir()?.join(".rudder")
        }
    } else {
        user_home_dir()?.join(".rudder")
    };
    Some(home.join("cloud.json"))
}

pub(crate) fn rudder_cloud_authenticated() -> bool {
    read_cloud_summary().connected
}

pub(crate) fn read_update_notice() -> Option<(String, String)> {
    let latest = env::var("RUDDER_UPDATE_AVAILABLE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let current = env::var("RUDDER_UPDATE_CURRENT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "current".to_string());
    Some((current, latest))
}
