#![allow(unused_imports)]
//! Git, worktree, run-record persistence, and filesystem helpers.
use super::*;
use std::cell::RefCell;

pub(crate) fn current_branch_at(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

pub(crate) fn native_runs_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".rudder").join("runs")
}

pub(crate) fn native_run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
    native_runs_dir(repo_root).join(run_id)
}

pub(crate) fn graph_json_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".rudder").join("graph.json")
}

/// A parsed, query-friendly view of `.rudder/graph.json`. The daemon (TS side)
/// owns writing this file; the TUI only reads it. The index lets us answer "what
/// are this run's hard/soft parent node ids" without re-walking JSON per agent.
#[derive(Clone, Debug, Default)]
pub(crate) struct GraphIndex {
    /// node id -> (hard parent node ids, soft parent node ids).
    parents_by_node: HashMap<String, (Vec<String>, Vec<String>)>,
    /// runId -> node id (so a run with a known runId resolves to its node).
    node_by_run: HashMap<String, String>,
}

impl GraphIndex {
    /// Build the index from a parsed graph.json value. Resolves each node's
    /// incoming edge ids (`deps`) through `edges` into parent node ids, split by
    /// edge `type`. Unknown edge ids are skipped.
    pub(crate) fn from_value(value: &serde_json::Value) -> Self {
        let mut index = GraphIndex::default();
        let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_object) else {
            return index;
        };
        let edges = value.get("edges").and_then(serde_json::Value::as_object);

        // edge id -> from node id (the parent), with type.
        let resolve_edge = |edge_id: &str| -> Option<(String, bool)> {
            let edge = edges?.get(edge_id)?;
            let from = edge.get("from").and_then(serde_json::Value::as_str)?;
            if from.trim().is_empty() {
                return None;
            }
            let is_hard = edge
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(|kind| kind.trim().eq_ignore_ascii_case("hard"))
                .unwrap_or(false);
            Some((from.to_string(), is_hard))
        };

        for (node_id, node) in nodes {
            let mut hard = Vec::new();
            let mut soft = Vec::new();
            if let Some(deps) = node.get("deps").and_then(serde_json::Value::as_array) {
                for dep in deps {
                    let Some(edge_id) = dep.as_str() else { continue };
                    if let Some((parent, is_hard)) = resolve_edge(edge_id) {
                        if is_hard {
                            hard.push(parent);
                        } else {
                            soft.push(parent);
                        }
                    }
                }
            }
            index
                .parents_by_node
                .insert(node_id.clone(), (hard, soft));

            if let Some(run_id) = node.get("runId").and_then(serde_json::Value::as_str) {
                if !run_id.trim().is_empty() {
                    index.node_by_run.insert(run_id.to_string(), node_id.clone());
                }
            }
        }

        index
    }

    /// Hard and soft parent node ids for a run, looked up first by its runId then
    /// by treating the run id itself as a node id. Returns empty vectors when the
    /// run has no node in the graph (flat behavior).
    pub(crate) fn deps_for_run(&self, run_id: &str) -> (Vec<String>, Vec<String>) {
        let node_id = self
            .node_by_run
            .get(run_id)
            .map(String::as_str)
            .unwrap_or(run_id);
        self.parents_by_node
            .get(node_id)
            .cloned()
            .unwrap_or_default()
    }
}

thread_local! {
    /// Per-repo cache of the parsed graph index, refreshed at most once per short
    /// window so a single `load_persisted_agents` pass reads graph.json once.
    static GRAPH_INDEX_CACHE: RefCell<Option<(PathBuf, Instant, GraphIndex)>> =
        const { RefCell::new(None) };
}

const GRAPH_INDEX_TTL: Duration = Duration::from_millis(500);

/// Load `.rudder/graph.json` for a repo as a [`GraphIndex`], reading from disk at
/// most once per [`GRAPH_INDEX_TTL`] window per repo. Absent or invalid graph.json
/// yields an empty index (flat behavior).
pub(crate) fn cached_graph_index(repo_root: &Path) -> GraphIndex {
    GRAPH_INDEX_CACHE.with(|cell| {
        if let Some((cached_root, loaded_at, index)) = cell.borrow().as_ref() {
            if cached_root == repo_root && loaded_at.elapsed() < GRAPH_INDEX_TTL {
                return index.clone();
            }
        }
        let index = load_graph_index(repo_root);
        *cell.borrow_mut() = Some((repo_root.to_path_buf(), Instant::now(), index.clone()));
        index
    })
}

fn load_graph_index(repo_root: &Path) -> GraphIndex {
    let Ok(raw) = fs::read_to_string(graph_json_path(repo_root)) else {
        return GraphIndex::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return GraphIndex::default();
    };
    GraphIndex::from_value(&value)
}

pub(crate) fn read_migration_manifest(repo_root: &Path) -> Vec<MigratedAgent> {
    let manifest_path = repo_root.join(".rudder").join("migration.json");
    let Ok(raw) = fs::read_to_string(&manifest_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let agents = value
        .get("agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for agent in agents {
        let run_id = agent
            .get("runId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = agent
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let worktree_path = agent
            .get("worktreePath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root.to_path_buf());
        let fresh_prompt = agent
            .get("freshPrompt")
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .map(ToOwned::to_owned);
        if run_id.is_empty() {
            continue;
        }
        out.push(MigratedAgent {
            run_id,
            session_id,
            worktree_path,
            fresh_prompt,
        });
    }
    out
}

pub(crate) fn create_main_agent(
    repo_root: &Path,
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    prompt: &str,
) -> AgentRun {
    let branch = current_branch_at(repo_root).unwrap_or_else(|| "HEAD".to_string());
    let task_summary = if prompt.trim().is_empty() {
        branch
    } else {
        summarize_task(prompt)
    };
    let now = now_stamp();
    AgentRun {
        id: new_run_id("main branch"),
        created_at: now.clone(),
        mode: AgentMode::Main,
        task: if prompt.trim().is_empty() {
            "main branch".to_string()
        } else {
            prompt.trim().to_string()
        },
        task_summary,
        current_prompt: prompt.trim().to_string(),
        turns: Vec::new(),
        last_user_input_at: now,
        backend,
        model: model.to_string(),
        effort,
        status: AgentStatus::Stopped,
        cwd: repo_root.to_path_buf(),
        worktree_branch: None,
        worktree_path: None,
        workspace_name: None,
        jj_change_id: None,
        session_id: None,
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
        review_source_ids: Vec::new(),
        deps: Vec::new(),
        soft_deps: Vec::new(),
        node_id: None,
        reconcile_planner: false,
        plan_stream: None,
        last_worker_input_at: None,
    }
}

pub(crate) fn load_persisted_agents(repo_root: &Path) -> Vec<AgentRun> {
    let Ok(entries) = fs::read_dir(native_runs_dir(repo_root)) else {
        return Vec::new();
    };
    let mut agents = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| fs::read_to_string(entry.path().join("run.json")).ok())
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .filter_map(|record| agent_from_run_record(repo_root, record))
        .collect::<Vec<_>>();
    agents.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    agents
}

pub(crate) fn agent_from_run_record(repo_root: &Path, record: serde_json::Value) -> Option<AgentRun> {
    let id = record.get("id")?.as_str()?.to_string();
    let task = record.get("task")?.as_str()?.to_string();
    let raw_task_summary = record
        .get("taskSummary")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let default_task_summary = summarize_task(&task);
    let task_summary = match (
        raw_task_summary.as_deref(),
        rudder_plan_worker_title_from_prompt(&task),
    ) {
        (Some(summary), Some(title))
            if summary == default_task_summary || summary.contains("rudder-plan coordinator") =>
        {
            truncate_chars(&title, 56)
        }
        (Some(summary), _) => summary.to_string(),
        (None, Some(title)) => truncate_chars(&title, 56),
        (None, None) => default_task_summary,
    };
    let backend = record
        .get("backend")
        .and_then(|value| value.as_str())
        .and_then(Backend::parse)
        .unwrap_or(Backend::Claude);
    let model = record
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_model_for(backend).to_string());
    let effort = record
        .get("effort")
        .and_then(|value| value.as_str())
        .and_then(EffortLevel::parse);
    let status = agent_status_from_record(record.get("status").and_then(|value| value.as_str()));
    let created_at = record
        .get("createdAt")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(now_stamp);
    let turns = turns_from_run_record(&record, &created_at, &task);
    let current_prompt = record
        .get("currentPrompt")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| turns.last().map(|turn| turn.prompt.clone()))
        .unwrap_or_else(|| task.clone());
    let last_user_input_at = record
        .get("lastUserInputAt")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| {
            turns
                .iter()
                .rev()
                .find(|turn| turn.source == "user")
                .map(|turn| turn.ts.clone())
        })
        .unwrap_or_else(|| created_at.clone());
    let mode = record
        .get("mode")
        .and_then(|value| value.as_str())
        .and_then(AgentMode::parse)
        .unwrap_or(AgentMode::Execute);
    let worktree = record.get("worktree");
    let worktree_enabled = worktree
        .and_then(|value| value.get("enabled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let cwd = worktree
        .and_then(|value| value.get("path"))
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.to_path_buf());
    let worktree_branch = worktree
        .and_then(|value| value.get("branch"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let worktree_path = worktree_enabled.then_some(cwd.clone());
    let workspace_name = worktree
        .and_then(|value| value.get("workspaceName"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let jj_change_id = worktree
        .and_then(|value| value.get("jjChangeId"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let session_id = record
        .get("session")
        .and_then(|value| value.get("nativeSessionId"))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let review_source_ids = record
        .get("reviewSourceIds")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Typed dependencies from the daemon-owned graph.json (read-only here). When
    // absent or the node is not found, both vectors stay empty (flat behavior).
    let (mut deps, soft_deps) = cached_graph_index(repo_root).deps_for_run(&id);

    // Plan node identity persisted by the scheduler-launch path. When present it
    // takes precedence so a restored plan-launched run keeps its gating + the id
    // that unblocks dependents on merge.
    let node_id = record
        .get("nodeId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    if deps.is_empty() {
        if let Some(plan_deps) = record.get("planDeps").and_then(|value| value.as_array()) {
            deps = plan_deps
                .iter()
                .filter_map(|value| value.as_str())
                .map(ToOwned::to_owned)
                .collect();
        }
    }

    Some(AgentRun {
        id,
        created_at,
        mode,
        task,
        task_summary,
        current_prompt,
        turns,
        last_user_input_at,
        backend,
        model,
        effort,
        status,
        cwd,
        worktree_branch,
        worktree_path,
        workspace_name,
        jj_change_id,
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
        review_source_ids,
        deps,
        soft_deps,
        node_id,
        // Reconcile-planner is ephemeral runtime routing state, never persisted; a
        // restored run is never mid-reconcile.
        reconcile_planner: false,
        plan_stream: None,
        last_worker_input_at: None,
    })
}

pub(crate) fn turns_from_run_record(
    record: &serde_json::Value,
    created_at: &str,
    task: &str,
) -> Vec<AgentTurn> {
    let mut turns = record
        .get("turns")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(turn_from_json)
                .collect::<Vec<AgentTurn>>()
        })
        .unwrap_or_default();

    if turns.is_empty() {
        turns.push(AgentTurn {
            ts: created_at.to_string(),
            prompt: task.to_string(),
            source: "user".to_string(),
        });
    }

    turns
}

pub(crate) fn turn_from_json(value: &serde_json::Value) -> Option<AgentTurn> {
    let prompt = value.get("prompt")?.as_str()?.to_string();
    let ts = value
        .get("ts")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(now_stamp);
    let source = value
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("user")
        .to_string();

    Some(AgentTurn { ts, prompt, source })
}

pub(crate) fn agent_status_from_record(status: Option<&str>) -> AgentStatus {
    match status {
        Some("completed") => AgentStatus::Done,
        Some("merged") => AgentStatus::Merged,
        Some("failed") => AgentStatus::Failed,
        Some("running") | Some("steering") | Some("verifying") | Some("created") => {
            AgentStatus::Running
        }
        Some("cancelled") | Some("merge-conflict") => AgentStatus::Stopped,
        _ => AgentStatus::Stopped,
    }
}

pub(crate) fn run_record_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "running",
        AgentStatus::Done => "completed",
        AgentStatus::Merged => "merged",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "cancelled",
    }
}

pub(crate) fn save_native_run_record(repo_root: &Path, run: &AgentRun) -> Result<()> {
    let run_dir = native_run_dir(repo_root, &run.id);
    fs::create_dir_all(&run_dir)?;
    let record_path = run_dir.join("run.json");
    let target_branch = current_branch_at(repo_root).unwrap_or_else(|| "HEAD".to_string());
    let base_commit = git_output(repo_root, ["rev-parse", "HEAD"])
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    let now = now_stamp();
    let turns = run
        .turns
        .iter()
        .map(|turn| {
            serde_json::json!({
                "ts": turn.ts,
                "prompt": turn.prompt,
                "source": turn.source,
            })
        })
        .collect::<Vec<_>>();
    // A run isolated in a jj workspace carries a workspace name. Those records
    // declare `vcs:"jj"` so the TS merge/sync path routes through jj. Legacy
    // git-worktree runs (a branch but no jj workspace, e.g. the review-all
    // aggregate) stay `vcs:"git"` for back-compat.
    let is_jj_run = run.workspace_name.is_some() || run.jj_change_id.is_some();
    let mut worktree = serde_json::json!({
        "enabled": run.worktree_path.is_some(),
        "path": run.cwd,
    });
    if let Some(map) = worktree.as_object_mut() {
        if is_jj_run {
            if let Some(name) = run.workspace_name.as_ref() {
                map.insert("workspaceName".to_string(), serde_json::json!(name));
            }
            if let Some(change_id) = run.jj_change_id.as_ref() {
                map.insert("jjChangeId".to_string(), serde_json::json!(change_id));
            }
        } else if let Some(branch) = run.worktree_branch.as_ref() {
            // Only legacy git runs keep a branch field; jj runs omit it.
            map.insert("branch".to_string(), serde_json::json!(branch));
        }
    }
    let record = serde_json::json!({
        "id": run.id,
        "status": run_record_status(run.status),
        "vcs": if is_jj_run { "jj" } else { "git" },
        "mode": run.mode.as_str(),
        "task": run.task,
        "taskSummary": run.task_summary,
        "backend": run.backend.as_str(),
        "model": run.model,
        "effort": run.effort.map(|effort| effort.as_str()),
        "createdAt": run.created_at,
        "updatedAt": now,
        "repoRoot": repo_root,
        "targetBranch": target_branch,
        "baseCommit": base_commit,
        "worktree": worktree,
        "currentPrompt": run.current_prompt,
        "turns": turns,
        "lastUserInputAt": run.last_user_input_at,
        "autoSteer": { "count": if run.autosteered { 1 } else { 0 }, "max": 2 },
        "reviewSourceIds": run.review_source_ids,
        // Plan node identity for scheduler-launched runs. `nodeId` enters the
        // merged set when this run merges; `planDeps` are the hard-dep node ids
        // this run was launched against (so a restored run keeps its gating).
        "nodeId": run.node_id,
        "planDeps": run.deps,
        "session": run.session_id.as_ref().map(|sid| serde_json::json!({ "nativeSessionId": sid })),
    });
    let temp = record_path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    fs::write(
        &temp,
        format!("{}\n", serde_json::to_string_pretty(&record)?),
    )?;
    fs::rename(temp, record_path)?;
    Ok(())
}

/// Read the `merge` (or `sync`) state object back from a run's run.json. The TS
/// `rudder merge`/`rudder sync` commands record their outcome there and exit 0
/// even on conflict, so the native side reads the recorded status rather than
/// the process exit code to tell merged/synced from conflict.
pub(crate) fn read_run_record_field(
    repo_root: &Path,
    run_id: &str,
    field: &str,
) -> Option<serde_json::Value> {
    let path = native_run_dir(repo_root, run_id).join("run.json");
    let raw = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    value.get(field).cloned()
}

/// Outcome of shelling out to a `rudder merge`/`rudder sync` jj run.
pub(crate) enum JjCliOutcome {
    Ok,
    Conflict { files: Vec<String> },
    Failed { error: String },
}

/// Run `rudder <command> <run_id>` synchronously (the TS command routes through
/// the jj merge/sync helpers for vcs:jj runs and captures op-log for undo), then
/// read back run.json to classify the outcome. Mirrors the cloudio.rs
/// locate_rudder_cli + Command::output pattern.
pub(crate) fn run_rudder_jj_command(
    repo_root: &Path,
    command: &str,
    run_id: &str,
    state_field: &str,
) -> JjCliOutcome {
    let Some(rudder) = locate_rudder_cli() else {
        return JjCliOutcome::Failed {
            error: "rudder CLI not found on PATH".to_string(),
        };
    };
    // A merge from the TUI is an explicit user action; allow a dirty target. jj
    // preserves the main workspace's working copy as a merge parent (no data loss,
    // undoable via `rudder undo`), so the git-style dirty gate would only get in the
    // way. (Rudder's own files no longer count as dirty either; see jj.ts.)
    let extra: &[&str] = if command == "merge" {
        &["--allow-dirty"]
    } else {
        &[]
    };
    let output = match Command::new(&rudder)
        .arg(command)
        .arg(run_id)
        .args(extra)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return JjCliOutcome::Failed {
                error: format!("failed to run rudder {command}: {error}"),
            };
        }
    };

    let state = read_run_record_field(repo_root, run_id, state_field);
    let status = state
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str);
    match status {
        Some("merged") | Some("synced") => JjCliOutcome::Ok,
        Some("conflict") => {
            let files = state
                .as_ref()
                .and_then(|value| value.get("conflictedFiles"))
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            JjCliOutcome::Conflict { files }
        }
        _ => {
            let recorded = state
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let error = recorded
                .filter(|value| !value.is_empty())
                .or(Some(stderr).filter(|value| !value.is_empty()))
                .or(Some(stdout).filter(|value| !value.is_empty()))
                .unwrap_or_else(|| format!("rudder {command} failed"));
            JjCliOutcome::Failed { error }
        }
    }
}

pub(crate) fn remove_native_run_record(repo_root: &Path, run_id: &str) -> Result<()> {
    let dir = native_run_dir(repo_root, run_id);
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct WorktreeInfo {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) path_is_worktree: bool,
    /// jj workspace name for the run (e.g. `rudder-<id>-<hash>`). Present for new
    /// runs isolated in a jj workspace; `None` for the dedicated main agent.
    pub(crate) workspace_name: Option<String>,
    /// The jj change id of the workspace's working-copy commit at launch.
    pub(crate) jj_change_id: Option<String>,
}

impl WorktreeInfo {
    pub(crate) fn current(path: PathBuf) -> Self {
        Self {
            id: new_run_id("task"),
            path,
            branch: None,
            path_is_worktree: false,
            workspace_name: None,
            jj_change_id: None,
        }
    }
}

/// Isolate a worker in a jj workspace instead of a git worktree. Shells out to
/// `rudder __launch-node` (the TS shim that owns the jj logic via src/jj.ts) so
/// the worker runs against a per-node jj workspace whose change id is captured
/// for merge/sync. The returned WorktreeInfo carries the workspace path, name,
/// and jj change id; `branch` stays empty for jj runs. New runs never silently
/// fall back to git: if the shim is unavailable or fails, this surfaces a clear
/// error to the caller.
pub(crate) fn prepare_jj_workspace(cwd: &Path, task: &str) -> Result<WorktreeInfo> {
    let repo = dashboard_root(cwd);
    if !is_git_repo(&repo) {
        return Ok(WorktreeInfo::current(cwd.to_path_buf()));
    }

    let id = new_run_id(task);
    let rudder = locate_rudder_cli()
        .context("rudder CLI not found on PATH; cannot create a jj workspace for the run")?;
    let output = Command::new(&rudder)
        .arg("__launch-node")
        .arg("--repo")
        .arg(&repo)
        .arg("--task")
        .arg(task)
        .arg("--node")
        .arg(&id)
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run rudder __launch-node")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if message.is_empty() {
            "rudder __launch-node failed".to_string()
        } else {
            message
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .context("rudder __launch-node printed no JSON line")?;
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).context("could not parse rudder __launch-node output")?;
    let path = value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("rudder __launch-node output is missing a workspace path")?
        .to_string();
    let workspace_name = value
        .get("workspaceName")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let jj_change_id = value
        .get("jjChangeId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    Ok(WorktreeInfo {
        id,
        path: PathBuf::from(path),
        branch: None,
        path_is_worktree: true,
        workspace_name,
        jj_change_id,
    })
}

pub(crate) fn prepare_worktree(cwd: &Path, task: &str) -> Result<WorktreeInfo> {
    let repo = dashboard_root(cwd);
    if !is_git_repo(&repo) {
        return Ok(WorktreeInfo::current(cwd.to_path_buf()));
    }

    let id = new_run_id(task);
    let base_commit = git_output(&repo, ["rev-parse", "main"])
        .or_else(|_| git_output(&repo, ["rev-parse", "HEAD"]))?;
    let task_slug = slugify(task, "task");
    let branch = format!("rudder/{}-{}", task_slug, worktree_unique_suffix(&id));
    let path = worktree_path(&repo, &id, task);
    let parent = path
        .parent()
        .context("worktree target has no parent directory")?;
    fs::create_dir_all(parent)?;

    let _ = Command::new("git")
        .args(["branch", &branch, base_commit.trim()])
        .current_dir(&repo)
        .output();
    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(&path)
        .arg(&branch)
        .current_dir(&repo)
        .output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if message.is_empty() {
            "git worktree add failed".to_string()
        } else {
            message
        });
    }

    Ok(WorktreeInfo {
        id,
        path,
        branch: Some(branch),
        path_is_worktree: true,
        workspace_name: None,
        jj_change_id: None,
    })
}

pub(crate) fn repo_root(cwd: &Path) -> PathBuf {
    git_output(cwd, ["rev-parse", "--show-toplevel"])
        .map(|root| PathBuf::from(root.trim()))
        .unwrap_or_else(|_| cwd.to_path_buf())
}

pub(crate) fn dashboard_root(cwd: &Path) -> PathBuf {
    let repo = repo_root(cwd);
    main_worktree_root(&repo).unwrap_or(repo)
}

pub(crate) fn main_worktree_root(repo: &Path) -> Option<PathBuf> {
    let output = git_output_args(repo, &["worktree", "list", "--porcelain"]).ok()?;
    main_worktree_from_porcelain(&output)
}

pub(crate) fn main_worktree_from_porcelain(output: &str) -> Option<PathBuf> {
    let mut current_path: Option<PathBuf> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
            continue;
        }
        if line.trim() == "branch refs/heads/main" {
            return current_path.clone();
        }
    }
    None
}

pub(crate) fn is_git_repo(cwd: &Path) -> bool {
    git_output(cwd, ["rev-parse", "--is-inside-work-tree"])
        .map(|value| value.trim() == "true")
        .unwrap_or(false)
}

pub(crate) fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if message.is_empty() {
            "git command failed".to_string()
        } else {
            message
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn git_output_args(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if message.is_empty() {
            "git command failed".to_string()
        } else {
            message
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn git_status_command(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::bail!(if message.is_empty() {
        "git command failed".to_string()
    } else {
        message
    });
}

pub(crate) fn rebase_worktree_onto_base(repo_root: &Path, worktree_path: &Path, base_branch: &str) -> Result<()> {
    let base_ref = resolve_rebase_base_ref(repo_root, base_branch)?;
    git_status_command(worktree_path, &["rebase", &base_ref])
        .with_context(|| format!("rebase onto {base_ref} failed"))
}

pub(crate) fn resolve_rebase_base_ref(repo_root: &Path, base_branch: &str) -> Result<String> {
    let branch = base_branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return current_commit_at(repo_root);
    }

    if ref_exists(repo_root, branch) {
        return Ok(branch.to_string());
    }

    let mut fetched = false;
    if git_status_command(repo_root, &["remote", "get-url", "origin"]).is_ok() {
        fetched = git_status_command(repo_root, &["fetch", "origin", branch]).is_ok();
    }

    let remote_ref = format!("refs/remotes/origin/{branch}");
    if ref_exists(repo_root, &remote_ref) {
        return Ok(remote_ref);
    }
    if fetched && ref_exists(repo_root, "FETCH_HEAD") {
        return Ok("FETCH_HEAD".to_string());
    }
    current_commit_at(repo_root)
}

pub(crate) fn current_commit_at(cwd: &Path) -> Result<String> {
    git_output(cwd, ["rev-parse", "HEAD"]).map(|value| value.trim().to_string())
}

pub(crate) fn ref_exists(repo_root: &Path, reference: &str) -> bool {
    let verify = format!("{reference}^{{commit}}");
    git_output_args(repo_root, &["rev-parse", "--verify", &verify]).is_ok()
}

pub(crate) fn commit_pending_changes_for_run(run: &AgentRun) -> Result<()> {
    if !has_git_changes(&run.cwd) {
        return Ok(());
    }

    git_status_command(&run.cwd, &["add", "-A"])?;
    let headline = if run.task_summary.trim().is_empty() {
        short_task(&run.task)
    } else {
        run.task_summary.trim().to_string()
    };
    let message = if run.task.trim() == headline.trim() {
        headline
    } else {
        format!("{headline}\n\n{}", run.task.trim())
    };
    let _ = git_status_command(&run.cwd, &["commit", "-m", &message]);
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn premerge_review_all_sources(cwd: &Path, sources: &[ReviewAllSource]) -> ReviewAllPremerge {
    let mut premerge = ReviewAllPremerge::default();
    for (index, source) in sources.iter().enumerate() {
        match git_status_command(cwd, &["merge", "--no-ff", &source.branch]) {
            Ok(()) => premerge.merged_branches.push(source.branch.clone()),
            Err(error) => {
                premerge.stopped_branch = Some(source.branch.clone());
                premerge.stopped_error = Some(error.to_string());
                premerge.remaining_branches = sources[index..]
                    .iter()
                    .map(|item| item.branch.clone())
                    .collect();
                break;
            }
        }
    }
    premerge
}

pub(crate) fn conflicted_files(cwd: &Path) -> Vec<String> {
    git_output(cwd, ["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn diff_short_summary(run: &AgentRun) -> Option<String> {
    diff_short_summary_at(&run.cwd)
}

pub(crate) fn diff_short_summary_at(cwd: &Path) -> Option<String> {
    let status = git_output(cwd, ["status", "--short"]).ok()?;
    if status.trim().is_empty() {
        return None;
    }
    let stat = git_output_args(cwd, &["diff", "--shortstat", "HEAD"]).ok();
    if let Some(stat) = stat
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(stat);
    }
    let files = status.lines().count();
    Some(format!(
        "{files} file{} changed",
        if files == 1 { "" } else { "s" }
    ))
}

pub(crate) fn play_completion_sound() {
    let Some(sound_path) = completion_sound_path() else {
        return;
    };

    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("afplay")
            .arg(sound_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = Command::new("ffplay")
            .args(["-nodisp", "-autoexit", "-loglevel", "quiet"])
            .arg(sound_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

pub(crate) fn completion_sound_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RUDDER_COMPLETION_SOUND").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("assets/sounds/ping.mp3"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(native_dir) = exe.parent() {
            candidates.push(native_dir.join("../../assets/sounds/ping.mp3"));
            candidates.push(native_dir.join("../../../assets/sounds/ping.mp3"));
        }
    }

    candidates.into_iter().find(|path| path.is_file())
}

pub(crate) fn has_git_changes(cwd: &Path) -> bool {
    git_output(cwd, ["status", "--short"])
        .map(|status| !status.trim().is_empty())
        .unwrap_or(false)
}

pub(crate) fn count_uncommitted_changes(cwd: &Path) -> usize {
    git_output(cwd, ["status", "--short"])
        .map(|status| {
            status
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
}

pub(crate) fn worktree_path(repo_root: &Path, run_id: &str, task: &str) -> PathBuf {
    let parent = repo_root.parent().unwrap_or(repo_root);
    let repo_name = format!(
        "{}-{}",
        slugify(
            repo_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repo"),
            "repo"
        ),
        short_hash(&repo_root.display().to_string())
    );
    parent
        .join(".rudder-worktrees")
        .join(repo_name)
        .join(worktree_dir_name(run_id, task))
}

pub(crate) fn worktree_dir_name(run_id: &str, task: &str) -> String {
    let task_slug = slugify(task, "task");
    format!("{}-{}", task_slug, worktree_unique_suffix(run_id))
}

pub(crate) fn write_rudder_context(
    repo_root: &Path,
    agents: &[AgentRun],
    pending: Option<&WorktreeInfo>,
) -> Result<()> {
    ensure_gitignore_contains(repo_root, "RUDDER.md")?;
    let mut body = String::from("# Rudder-Specific Context\n\nThis file is generated by Rudder. It is not user-authored repo documentation. Use it to coordinate with other Rudder agents in this checkout.\n\n## Active local Rudder agents\n");
    if agents.is_empty() && pending.is_none() {
        body.push_str("- none\n");
    }
    for agent in agents {
        let current_prompt = if agent.current_prompt != agent.task {
            format!(" current=\"{}\"", preview_text(&agent.current_prompt, 140))
        } else {
            String::new()
        };
        body.push_str(&format!(
            "- {}: {} [{} {}] cwd={}{}\n",
            agent.id,
            agent.task,
            agent.backend.as_str(),
            agent.model,
            agent.cwd.display(),
            current_prompt
        ));
    }
    if let Some(worktree) = pending {
        body.push_str(&format!(
            "- starting: cwd={} branch={}\n",
            worktree.path.display(),
            worktree.branch.as_deref().unwrap_or("current")
        ));
    }
    body.push_str(
        "\nRead this file before making changes so you know what other Rudder agents are doing.\n",
    );
    body.push_str(
        "\n## Rudder review integration\n\nRudder opens `hunk diff --watch` in the review pane when available. If a live Hunk review is open, run `hunk skill path`, load that skill, then use `hunk session review --repo . --json` to inspect the review and `hunk session comment add/apply --repo .` to leave inline notes for the user.\n",
    );
    fs::write(repo_root.join("RUDDER.md"), body.as_bytes())?;
    if let Some(worktree) = pending {
        if worktree.path_is_worktree {
            fs::write(worktree.path.join("RUDDER.md"), body.as_bytes())?;
        }
    }
    Ok(())
}

pub(crate) fn ensure_hunk_config(cwd: &Path) -> Result<()> {
    ensure_git_info_exclude_contains(cwd, ".hunk/")?;
    let dir = cwd.join(".hunk");
    fs::create_dir_all(&dir)?;
    let config = dir.join("config.toml");
    let theme = hunk_light_theme();
    let contents = [
        format!("theme = \"{theme}\""),
        "mode = \"auto\"".to_string(),
        "vcs = \"jj\"".to_string(),
        "exclude_untracked = false".to_string(),
        "line_numbers = true".to_string(),
        "wrap_lines = false".to_string(),
        "agent_notes = true".to_string(),
        String::new(),
    ]
    .join("\n");
    fs::write(config, contents)?;
    Ok(())
}

pub(crate) fn hunk_light_theme() -> String {
    match env::var("RUDDER_HUNK_THEME") {
        Ok(value) if value == "light" => "paper".to_string(),
        Ok(value) if matches!(value.as_str(), "paper" | "graphite" | "midnight" | "ember") => value,
        _ => "paper".to_string(),
    }
}

pub(crate) fn ensure_git_info_exclude_contains(cwd: &Path, line: &str) -> Result<()> {
    let path = git_output(cwd, ["rev-parse", "--git-path", "info/exclude"])?;
    let trimmed = path.trim();
    let path = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        cwd.join(trimmed)
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|existing| existing.trim() == line) {
        return Ok(());
    }
    let mut next = existing;
    if !next.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    next.push_str(line);
    next.push('\n');
    fs::write(path, next)?;
    Ok(())
}

pub(crate) fn ensure_gitignore_contains(repo_root: &Path, line: &str) -> Result<()> {
    let path = repo_root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|existing_line| existing_line.trim() == line)
    {
        return Ok(());
    }
    let prefix = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(path, format!("{existing}{prefix}{line}\n"))?;
    Ok(())
}

pub(crate) fn new_run_id(task: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos}-{}-{}", slugify(task, "task"), std::process::id())
}

pub(crate) fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

pub(crate) fn slugify(input: &str, fallback: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

pub(crate) fn short_hash(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:010x}", hasher.finish())[..10].to_string()
}

pub(crate) fn worktree_unique_suffix(run_id: &str) -> String {
    short_hash(run_id).chars().take(8).collect()
}
