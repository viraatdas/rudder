#![allow(unused_imports)]
//! Reading and writing ~/.rudder config, model defaults, and update notices.
use super::*;

pub(crate) fn load_rudder_config() -> Option<serde_json::Value> {
    let path = rudder_config_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
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
    };
    keys.iter().find_map(|key| {
        backend_config
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .and_then(EffortLevel::parse)
    })
}

pub(crate) fn config_merge_strategy(config: &serde_json::Value) -> MergeStrategy {
    config
        .get("mergeStrategy")
        .and_then(serde_json::Value::as_str)
        .map(MergeStrategy::parse)
        .unwrap_or(MergeStrategy::Merge)
}

pub(crate) fn merge_strategy() -> MergeStrategy {
    load_rudder_config()
        .as_ref()
        .map(config_merge_strategy)
        .unwrap_or(MergeStrategy::Merge)
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

pub(crate) fn save_model_defaults(backend: Backend, model: &str, effort: Option<EffortLevel>) -> Result<()> {
    let path = rudder_config_path().context("could not determine Rudder config path")?;
    let mut config = load_rudder_config().unwrap_or_else(default_config_value);
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
    }

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
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    fs::rename(&temp, &path)?;
    set_private_file_mode(&path);
    Ok(())
}

pub(crate) fn ensure_config_defaults(config: &mut serde_json::Value) {
    let Some(root) = config.as_object_mut() else {
        return;
    };
    root.entry("version".to_string())
        .or_insert(serde_json::json!(1));
    root.entry("defaultBackend".to_string())
        .or_insert(serde_json::json!("claude"));
    root.entry("mergeStrategy".to_string())
        .or_insert(serde_json::json!("merge"));
    root.entry("runPolicy".to_string()).or_insert_with(|| {
        serde_json::json!({
            "sameCheckout": "single-active",
            "concurrentPromptMode": "worktree",
            "mergeMode": "manual-on-conflict"
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
        "runPolicy": {
            "sameCheckout": "single-active",
            "concurrentPromptMode": "worktree",
            "mergeMode": "manual-on-conflict"
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

