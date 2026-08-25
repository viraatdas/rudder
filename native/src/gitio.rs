#![allow(unused_imports)]
//! Git, workspace, run-record persistence, and filesystem helpers.
use super::*;
use std::cell::RefCell;

// ---------------------------------------------------------------------------
// Git state cache — invalidated by WATCHED FILE MTIMES, not by a timer.
//
// A TTL cache re-runs its subprocess every N seconds whether or not anything
// changed; git state in a repo nobody is committing to is unchanged for hours,
// so that work is almost entirely wasted. Instead, stamp the files git itself
// writes when the answer would change and recompute only when a stamp moves:
//
//   .git/HEAD   — branch switches, detached HEAD
//   .git/config — remote URL changes
//   .git/refs   — new commits move a loose ref inside this directory
//
// A stat is microseconds against a subprocess's ~10-25ms, so the steady state
// costs effectively nothing. (This mirrors Claude Code's GitFileWatcher; its
// Node `watchFile` is itself an interval stat-poller, so stamping is the same
// mechanism without pulling in a filesystem-notification dependency.)
// ---------------------------------------------------------------------------

type GitStamps = [Option<std::time::SystemTime>; 3];

fn git_state_stamps(repo_root: &Path) -> GitStamps {
    let git_dir = repo_root.join(".git");
    let stamp = |path: PathBuf| {
        std::fs::metadata(path)
            .ok()
            .and_then(|meta| meta.modified().ok())
    };
    [
        stamp(git_dir.join("HEAD")),
        stamp(git_dir.join("config")),
        stamp(git_dir.join("refs")),
    ]
}

#[allow(clippy::type_complexity)]
fn git_state_cache() -> &'static std::sync::Mutex<HashMap<(PathBuf, &'static str), (GitStamps, Option<String>)>>
{
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(PathBuf, &'static str), (GitStamps, Option<String>)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Cached git value for `repo_root`, recomputed only when a watched file moved.
fn cached_git_state(
    repo_root: &Path,
    key: &'static str,
    compute: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let stamps = git_state_stamps(repo_root);
    let cache_key = (repo_root.to_path_buf(), key);
    if let Ok(cache) = git_state_cache().lock() {
        if let Some((seen, value)) = cache.get(&cache_key) {
            if *seen == stamps {
                return value.clone();
            }
        }
    }
    let value = compute();
    if let Ok(mut cache) = git_state_cache().lock() {
        cache.insert(cache_key, (stamps, value.clone()));
    }
    value
}

/// Drop every cached git answer. For tests, and for any path that knowingly
/// rewrites git state behind the stamps.
pub(crate) fn clear_git_state_cache() {
    if let Ok(mut cache) = git_state_cache().lock() {
        cache.clear();
    }
}

pub(crate) fn current_branch_at(cwd: &Path) -> Option<String> {
    cached_git_state(cwd, "branch", || current_branch_uncached(cwd))
}

fn current_branch_uncached(cwd: &Path) -> Option<String> {
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

pub(crate) const RUDDER_SHARED_CONTEXT_FILE: &str = "RUDDER_SHARED.md";

pub(crate) fn shared_context_path(repo_root: &Path) -> PathBuf {
    repo_root.join(RUDDER_SHARED_CONTEXT_FILE)
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
                    let Some(edge_id) = dep.as_str() else {
                        continue;
                    };
                    if let Some((parent, is_hard)) = resolve_edge(edge_id) {
                        if is_hard {
                            hard.push(parent);
                        } else {
                            soft.push(parent);
                        }
                    }
                }
            }
            index.parents_by_node.insert(node_id.clone(), (hard, soft));

            if let Some(run_id) = node.get("runId").and_then(serde_json::Value::as_str) {
                if !run_id.trim().is_empty() {
                    index
                        .node_by_run
                        .insert(run_id.to_string(), node_id.clone());
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
        let workspace_path = agent
            .get("workspacePath")
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
            workspace_path,
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
        workspace_branch: None,
        workspace_path: None,
        workspace_name: None,
        jj_change_id: None,
        integration: IntegrationEvidence::default(),
        publish: PublishEvidence::default(),
        delivery: DeliveryEvidence::for_task(prompt),
        session_id: None,
        terminal: None,
        terminal_size: None,
        review_terminal: None,
        review_size: None,
        review_error: None,
        last_output_at: Instant::now(),
        completed_at: None,
        autosteered: false,
        plain_process: false,
        interactive_orchestrator: false,
        needs_permission: false,
        needs_user_input: false,
        last_error: None,
        worker_input_draft: String::new(),
        worker_input_cursor: 0,
        worker_input_is_prompt: false,
        last_drain_at: None,
        review_source_ids: Vec::new(),
        deps: Vec::new(),
        soft_deps: Vec::new(),
        node_id: None,
        plan_id: None,
        reviewed_at: None,
        reconcile_planner: false,
        plan_stream: None,
        plan_output_cache: None,
        last_worker_input_at: None,
        merge_resolver: false,
        merge_conflict: false,
        merge_conflict_operation: ConflictOperation::Merge,
        merge_conflict_files: Vec::new(),
        had_merge_conflict: false,
        done_summary: None,
        tokens_in: 0,
        tokens_out: 0,
        gam: None,
    }
}

/// Build a ONE-OFF agent: a single conversational agent that runs in the MAIN checkout
/// (no jj workspace, no DAG node) for a question or a small self-contained change. Mirrors
/// `create_main_agent` but with `AgentMode::OneOff` and the task seeded from the request.
/// `status` starts Stopped/`terminal` None; `start_oneoff_task` spawns the PTY.
pub(crate) fn create_oneoff_agent(
    repo_root: &Path,
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    prompt: &str,
) -> AgentRun {
    let now = now_stamp();
    let mut run = create_main_agent(repo_root, backend, model, effort, prompt);
    run.id = new_run_id("one-off");
    run.created_at = now.clone();
    run.mode = AgentMode::OneOff;
    run.task = prompt.trim().to_string();
    run.task_summary = summarize_task(prompt);
    run.current_prompt = prompt.trim().to_string();
    run
}

pub(crate) fn load_persisted_agents(repo_root: &Path) -> Vec<AgentRun> {
    let Ok(entries) = fs::read_dir(native_runs_dir(repo_root)) else {
        return Vec::new();
    };
    let mut agents: Vec<AgentRun> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path().join("run.json");
        // A missing/corrupt/partial run.json used to make the agent silently vanish on
        // reload (filter_map swallowed it). Log the path + reason so a lost agent is
        // diagnosable; still skip it (we cannot reconstruct a malformed record).
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue, // not a run dir (no run.json); normal, stay quiet
        };
        let mut record = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("rudder: skipping unreadable {}: {error}", path.display());
                continue;
            }
        };
        heal_moved_repo_paths(&mut record, repo_root);
        match agent_from_run_record(repo_root, record) {
            Some(agent) => agents.push(agent),
            None => eprintln!(
                "rudder: skipping {} (run record missing required fields)",
                path.display()
            ),
        }
    }
    // Drop any TRANSIENT reconcile planner left on disk (e.g. by a crash mid-reconcile,
    // or an older build that never deleted them). They are ephemeral helpers, not the
    // pinned orchestrator: reloading one as a RudderPlan row would surface a phantom
    // second orchestrator in the agent pane. The real planner has reconcile_planner=false.
    agents.retain(|run| !run.reconcile_planner);
    agents.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    // ONE ORCHESTRATOR PER PLAN on reload: keep the NEWEST pinned planner for each
    // plan id (rows are sorted desc, so the first one seen per plan wins) and drop
    // older rows of the SAME plan. Every plan ever run persists its planner row and
    // `is_orchestrator()` is true for all of them, so without this a long-lived repo
    // reloads a phantom orchestrator per past refine/rebase.
    //
    // This used to keep exactly one orchestrator overall, which was right when only
    // one plan could exist. With concurrent plans it silently collapsed them: two
    // live plans survived in memory but came back as one after any restart, so the
    // second plan's row — and the way into its pane — was gone. Rows predating plan
    // ids carry None and still collapse to the newest, which is the old behavior and
    // the only safe reading of an unowned planner.
    //
    // Orchestrators whose plan no longer exists are dropped by the caller, which is
    // the only place that knows which plans were restored. In-memory only; nothing
    // is written to disk during a read.
    let mut seen_plans: HashSet<Option<String>> = HashSet::new();
    agents.retain(|run| {
        if run.is_orchestrator() {
            return seen_plans.insert(run.plan_id.clone());
        }
        true
    });
    agents
}

pub(crate) fn stop_request_path(repo_root: &Path, run_id: &str) -> PathBuf {
    native_run_dir(repo_root, run_id).join("stop-request.json")
}

pub(crate) fn stop_requested(repo_root: &Path, run_id: &str) -> bool {
    stop_request_path(repo_root, run_id).is_file()
}

pub(crate) fn clear_stop_request(repo_root: &Path, run_id: &str) {
    let _ = fs::remove_file(stop_request_path(repo_root, run_id));
}

/// Recorded absolute paths survive a repo RENAME or MOVE; the repo does not.
/// A run record's workspace and cwd live INSIDE the repo and moved with it, so
/// when the record's `repoRoot` disagrees with the root the record was read
/// from, every stored path under the old root has an exact counterpart under
/// the new one. Rewriting the prefix makes the record true again — without
/// this, one `mv` of the repo directory made every relaunch spawn into a
/// nonexistent cwd and die with "agent process exited (exit 1)". (jj survives
/// the same rename because its workspace pointers are relative; this is the
/// derive-don't-cache principle applied to our own records.)
pub(crate) fn heal_moved_repo_paths(record: &mut serde_json::Value, actual_root: &Path) {
    let current = actual_root.to_string_lossy().to_string();
    // Rule 1: prefix-remap against the STORED root, when it disagrees.
    let stored_root = record
        .get("repoRoot")
        .and_then(serde_json::Value::as_str)
        .filter(|root| !root.is_empty() && *root != current)
        .map(ToString::to_string);
    // Rule 2 exists because rule 1 is not enough: a later save can rewrite
    // repoRoot alone (observed in the field: repoRoot said the new name while
    // workspace.path still said the old one), which makes the stored root
    // USELESS as a key. Workspaces always live at
    // <root>/.rudder-workspaces/..., so whatever precedes that marker in a
    // stored path is some spelling of the root — rebase it onto the real one.
    remap_strings(record, stored_root.as_deref(), &current);
    // Rule 3: a record inside <root>/.rudder/runs belongs to <root> by
    // construction; the field is derivable, so derive it.
    if let Some(root_field) = record.get_mut("repoRoot") {
        if root_field.is_string() {
            *root_field = serde_json::Value::String(current);
        }
    }
}

/// Directory inside the repo holding one jj workspace per agent.
pub(crate) const AGENT_WORKSPACES_DIR: &str = ".rudder-workspaces";
/// The pre-rename name for the same directory. Still recognized wherever a path
/// is CLASSIFIED, because a jj workspace stays registered under the absolute
/// path it was created at. Only NEW workspaces use AGENT_WORKSPACES_DIR.
pub(crate) const LEGACY_AGENT_WORKSPACES_DIR: &str = ".rudder-worktrees";

/// True when `path` sits inside either spelling of the agent-workspaces dir.
pub(crate) fn is_under_agent_workspaces(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str() == AGENT_WORKSPACES_DIR
            || component.as_os_str() == LEGACY_AGENT_WORKSPACES_DIR
    })
}

const WORKSPACES_MARKERS: [&str; 2] = ["/.rudder-workspaces/", "/.rudder-worktrees/"];

fn remap_strings(value: &mut serde_json::Value, old_root: Option<&str>, new_root: &str) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(old_root) = old_root {
                if text == old_root {
                    *text = new_root.to_string();
                    return;
                }
                if let Some(rest) = text.strip_prefix(&format!("{old_root}/")) {
                    *text = format!("{new_root}/{rest}");
                    return;
                }
            }
            for marker in WORKSPACES_MARKERS {
                if let Some(index) = text.find(marker) {
                    if !text.starts_with(new_root) {
                        *text = format!("{new_root}{}", &text[index..]);
                    }
                    break;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                remap_strings(item, old_root, new_root);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                remap_strings(item, old_root, new_root);
            }
        }
        _ => {}
    }
}

pub(crate) fn agent_from_run_record(
    repo_root: &Path,
    record: serde_json::Value,
) -> Option<AgentRun> {
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
    // Records written before the worktree->workspace rename carry the isolation
    // info under `worktree`. Read either; every write below emits `workspace`.
    let workspace = record.get("workspace").or_else(|| record.get("worktree"));
    let workspace_enabled = workspace
        .and_then(|value| value.get("enabled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let cwd = workspace
        .and_then(|value| value.get("path"))
        .and_then(|value| value.as_str())
        // An EMPTY string is not a usable cwd. Without this filter a record
        // whose workspace.path is "" (corruption, a half-written record, an
        // interrupted rename) loads with an empty cwd, and every spawn against
        // that row fails silently — a row that is simply "not working" with no
        // error. Fall through to the repo root, the right home for a
        // main-checkout agent anyway.
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.to_path_buf());
    let workspace_branch = workspace
        .and_then(|value| value.get("branch"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let workspace_path = workspace_enabled.then_some(cwd.clone());
    let workspace_name = workspace
        .and_then(|value| value.get("workspaceName"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let jj_change_id = workspace
        .and_then(|value| value.get("jjChangeId"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let integration = integration_evidence_from_record(&record);
    let publish = publish_evidence_from_record(&record);
    let delivery = record
        .get("delivery")
        .and_then(|value| DeliveryEvidence::from_json(value, true))
        .unwrap_or_else(|| DeliveryEvidence::for_task(&task));
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
    // The owning plan, when the record was written by a version that knew about it.
    // Absent on older records; `App::new` adopts those into the single restored plan.
    let plan_id = record
        .get("planId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    // The review gate has to survive a restart: a crash between reviewing and
    // merging must not silently re-arm the gate, or the user reviews twice and
    // learns to distrust it.
    let reviewed_at = record
        .get("reviewedAt")
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

    let autosteered = record
        .get("autoSteer")
        .and_then(|value| value.get("count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        > 0;
    let reconcile_planner = record
        .get("reconcilePlanner")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let merge_resolver = record
        .get("mergeResolver")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| record.get("resolverFor").is_some());
    let merge_state = record.get("merge");
    let sync_state = record.get("sync");
    let merge_status = merge_state
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str);
    let sync_status = sync_state
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str);
    let record_status = record.get("status").and_then(serde_json::Value::as_str);
    let merge_conflict = record_status == Some("merge-conflict")
        || merge_status == Some("conflict")
        || sync_status == Some("conflict");
    // Durable conflict history: the explicit marker, or (back-compat with records
    // written before it existed) a still-live conflict state.
    let had_merge_conflict = record
        .get("hadMergeConflict")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || merge_conflict;
    let conflict_kind = merge_state
        .and_then(|value| value.get("conflictKind"))
        .or_else(|| sync_state.and_then(|value| value.get("conflictKind")))
        .and_then(serde_json::Value::as_str);
    let merge_conflict_operation = if conflict_kind == Some("rebase") {
        ConflictOperation::Rebase
    } else {
        ConflictOperation::Merge
    };
    let conflict_files_from = |state: Option<&serde_json::Value>| {
        state
            .and_then(|value| value.get("conflictedFiles"))
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut merge_conflict_files = conflict_files_from(merge_state);
    if merge_conflict_files.is_empty() {
        merge_conflict_files = conflict_files_from(sync_state);
    }
    // Token usage persisted at completion (or by the TS __worker path). Read it
    // back so a post-reload save (merge, rename) does not drop the cost signal.
    let tokens_in = record
        .get("tokens")
        .and_then(|value| value.get("input"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let tokens_out = record
        .get("tokens")
        .and_then(|value| value.get("output"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let interactive_orchestrator = record
        .get("interactiveOrchestrator")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| {
            // Back-compat for records written before `interactiveOrchestrator` existed:
            // interactive orchestrators were non-autosteered and stayed running
            // (or stopped after approval). Headless planners generally complete/exit.
            mode == AgentMode::RudderPlan
                && !reconcile_planner
                && !autosteered
                && matches!(status, AgentStatus::Running | AgentStatus::Stopped)
        });

    let mut run = AgentRun {
        plain_process: false,
        id,
        created_at,
        mode,
        plan_id,
        reviewed_at,
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
        workspace_branch,
        workspace_path,
        workspace_name,
        jj_change_id,
        integration,
        publish,
        delivery,
        session_id,
        terminal: None,
        terminal_size: None,
        review_terminal: None,
        review_size: None,
        review_error: None,
        last_output_at: Instant::now(),
        completed_at: None,
        autosteered,
        interactive_orchestrator,
        needs_permission: false,
        needs_user_input: false,
        last_error: None,
        worker_input_draft: String::new(),
        worker_input_cursor: 0,
        worker_input_is_prompt: false,
        last_drain_at: None,
        review_source_ids,
        deps,
        soft_deps,
        node_id,
        // A reconcile planner is a TRANSIENT row that should never reload as a pinned
        // orchestrator. Read the persisted discriminator (absent in old records →
        // false, the real planner); load_persisted_agents filters true ones out.
        reconcile_planner,
        plan_stream: None,
        plan_output_cache: None,
        last_worker_input_at: None,
        merge_resolver,
        merge_conflict,
        merge_conflict_operation,
        merge_conflict_files,
        had_merge_conflict,
        done_summary: record
            .get("doneSummary")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        tokens_in,
        tokens_out,
        gam: gam_state_from_record(&record),
    };
    // A record persisted with the resolver labels ("Resolve merge conflicts: …")
    // and merged out of band (records written before the merge-time restore
    // existed, or a labeled run merged via the CLI while the dashboard was
    // closed) would otherwise reload with the clobbered title forever: the
    // restore in mark_agent_and_review_sources_merged only runs at the merge
    // transition. In-memory only (no disk mutation during a read); conflict
    // telemetry rides the durable had_merge_conflict marker, not the label.
    if run.status == AgentStatus::Merged {
        run.restore_pre_conflict_identity();
    }
    Some(run)
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
        // A conflicted merge means the WORK finished and only integration is
        // pending: reload it as Done (review bucket, mergeable with m) rather
        // than Stopped, which buried it in "closed" looking cancelled.
        Some("merge-conflict") => AgentStatus::Done,
        Some("cancelled") => AgentStatus::Stopped,
        Some("paused") => AgentStatus::Paused,
        Some("orphaned") => AgentStatus::Orphaned,
        Some("migrated") => AgentStatus::Migrated,
        _ => AgentStatus::Stopped,
    }
}

/// Read a persisted `/gam` pair link back off a run record. Old records (and
/// every non-gam run) have no `gam` object and load as `None`.
pub(crate) fn gam_state_from_record(record: &serde_json::Value) -> Option<crate::GamState> {
    use crate::{
        GamMessage, GamMessageKind, GamOutcome, GamPhase, GamRole, GamState, GAM_RUNAWAY_ROUNDS,
    };
    let gam = record.get("gam")?;
    let role = gam
        .get("role")
        .and_then(serde_json::Value::as_str)
        .and_then(GamRole::parse)?;
    let peer_run_id = gam
        .get("peerId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)?;
    let task = gam
        .get("task")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let phase = match gam.get("phase").and_then(serde_json::Value::as_str) {
        Some("verdict") => GamPhase::AwaitingVerdict,
        Some("accepted") => GamPhase::Settled(GamOutcome::Accepted),
        // An escalation reason is not persisted (it lives in the activity log);
        // a reloaded escalated pair still reads as ended-without-agreement.
        Some("escalated") => GamPhase::Settled(GamOutcome::Escalated(String::new())),
        Some("halted") => GamPhase::Settled(GamOutcome::Halted(String::new())),
        // A mid-turn check cannot survive a restart: the generator turn it was
        // watching is gone with its PTY. It reloads as an ordinary working turn,
        // which is the state the pair would have been in either way.
        _ => GamPhase::GeneratorWorking,
    };
    let transcript = gam
        .get("transcript")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let text = entry
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if text.trim().is_empty() {
                        return None;
                    }
                    Some(GamMessage {
                        round: entry
                            .get("round")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as u32,
                        speaker: entry
                            .get("speaker")
                            .and_then(serde_json::Value::as_str)
                            .and_then(GamRole::parse)?,
                        kind: entry
                            .get("kind")
                            .and_then(serde_json::Value::as_str)
                            .and_then(GamMessageKind::parse)?,
                        text,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(GamState {
        role,
        peer_run_id,
        round: gam.get("round").and_then(serde_json::Value::as_u64).unwrap_or(0) as u32,
        runaway_rounds: gam
            .get("runawayRounds")
            // Records written before the round cap became a runaway guard carry
            // "maxRounds": 4. Reading that as the new guard would stop a
            // reloaded pair after four rounds, which is the behaviour being
            // removed, so old values are ignored rather than honoured.
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as u32)
            .filter(|value| *value > 0)
            .unwrap_or(GAM_RUNAWAY_ROUNDS),
        // Progress is compared against the round BEFORE it, which a restart has
        // no memory of; starting empty just means the first round after a
        // reload cannot be judged a stall.
        last_progress: None,
        phase,
        task,
        last_objection: gam
            .get("lastObjection")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        last_reply: gam
            .get("lastReply")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // Transient timestamps: meaningless across a restart.
        phase_since: None,
        transcript,
        steers: gam
            .get("steers")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        // The mid-turn watch is armed by a live turn, not by a record: there is
        // no PTY to watch until something starts one again.
        next_interim_at: None,
        last_interim_progress: None,
        awaiting_since: None,
    })
}

/// Mirror of `gam_state_from_record` for writes. Absent from the JSON entirely
/// when the row is not part of a `/gam` pair, so old readers see nothing new.
pub(crate) fn gam_state_to_json(gam: &crate::GamState) -> serde_json::Value {
    serde_json::json!({
        "role": gam.role.as_str(),
        "peerId": gam.peer_run_id,
        "round": gam.round,
        "runawayRounds": gam.runaway_rounds,
        "phase": gam.phase.as_str(),
        "task": gam.task,
        "lastObjection": gam.last_objection,
        "lastReply": gam.last_reply,
        "steers": gam.steers,
        // The dialogue is the only part of the exchange that outlives the PTYs,
        // so it is the one thing here worth writing per round.
        "transcript": gam
            .transcript
            .iter()
            .map(|message| {
                serde_json::json!({
                    "round": message.round,
                    "speaker": message.speaker.as_str(),
                    "kind": message.kind.as_str(),
                    "text": message.text,
                })
            })
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn integration_evidence_from_record(record: &serde_json::Value) -> IntegrationEvidence {    let merge = record.get("merge");
    let text = |field: &str| {
        merge
            .and_then(|value| value.get(field))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    };
    IntegrationEvidence {
        operation_id: text("operationId"),
        // Derived, never loaded: whether the work is still in trunk is a question
        // for jj on every tick, not a flag that can go stale on disk.
        in_trunk: None,
        phase: match record
            .get("lifecyclePhase")
            .and_then(serde_json::Value::as_str)
        {
            Some("integrating") => IntegrationPhase::Integrating,
            Some("resolving") => IntegrationPhase::Resolving,
            Some("pushed") => IntegrationPhase::Pushed,
            Some("merged-local") | Some("merged-locally") => IntegrationPhase::MergedLocal,
            _ if merge
                .and_then(|value| value.get("pushed"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false) =>
            {
                IntegrationPhase::Pushed
            }
            _ if merge
                .and_then(|value| value.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("merged") =>
            {
                IntegrationPhase::MergedLocal
            }
            _ => IntegrationPhase::Pending,
        },
        bookmark: text("targetBranch"),
        merge_change_id: text("mergeChangeId"),
        git_commit: text("localCommit"),
        pushed: merge
            .and_then(|value| value.get("pushed"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

pub(crate) fn run_record_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "running",
        AgentStatus::Done => "completed",
        AgentStatus::Merged => "merged",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "cancelled",
        AgentStatus::Paused => "paused",
        AgentStatus::Orphaned => "orphaned",
        AgentStatus::Migrated => "migrated",
    }
}

pub(crate) fn save_native_run_record(repo_root: &Path, run: &AgentRun) -> Result<()> {
    let run_dir = native_run_dir(repo_root, &run.id);
    fs::create_dir_all(&run_dir)?;
    let record_path = run_dir.join("run.json");
    let existing_record = fs::read_to_string(&record_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let target_branch = existing_record
        .get("targetBranch")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| current_branch_at(repo_root).unwrap_or_else(|| "HEAD".to_string()));
    let base_commit = existing_record
        .get("baseCommit")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            git_output(repo_root, ["rev-parse", "HEAD"])
                .map(|value| value.trim().to_string())
                .unwrap_or_default()
        });
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
    // declare `vcs:"jj"` so the TS merge/sync path routes through jj. Old
    // git-worktree records retain `vcs:"git"` for readable history only.
    // A workspace's IDENTITY is written once and never unwritten. An in-memory row
    // that has lost its name or change id (reloaded from a partial record, rebuilt
    // by a path that does not carry them) must not erase what disk already knows:
    // without those fields the row cannot be merged, and the orphan sweep stops
    // recognising its directory as owned — which is how a finished-but-unmerged
    // workspace disappeared from a live repo.
    let recorded_workspace_name = existing_record
        .get("workspace")
        .or_else(|| existing_record.get("worktree"))
        .and_then(|value| value.get("workspaceName"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let recorded_change_id = existing_record
        .get("workspace")
        .or_else(|| existing_record.get("worktree"))
        .and_then(|value| value.get("jjChangeId"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let workspace_name = run.workspace_name.clone().or(recorded_workspace_name);
    let jj_change_id = run.jj_change_id.clone().or(recorded_change_id);
    let is_jj_run = workspace_name.is_some() || jj_change_id.is_some();
    let mut workspace = serde_json::json!({
        "enabled": run.workspace_path.is_some(),
        "path": run.cwd,
    });
    if let Some(map) = workspace.as_object_mut() {
        if is_jj_run {
            if let Some(name) = workspace_name.as_ref() {
                map.insert("workspaceName".to_string(), serde_json::json!(name));
            }
            if let Some(change_id) = jj_change_id.as_ref() {
                map.insert("jjChangeId".to_string(), serde_json::json!(change_id));
            }
        } else if let Some(branch) = run.workspace_branch.as_ref() {
            // Preserve the branch field only when rewriting an old record.
            map.insert("branch".to_string(), serde_json::json!(branch));
        }
    }
    let record_status = if run.status == AgentStatus::Done && run.merge_conflict {
        "merge-conflict"
    } else {
        run_record_status(run.status)
    };
    let mut record = serde_json::json!({
        "id": run.id,
        "status": record_status,
        "lifecyclePhase": run.lifecycle_label(),
        "vcs": if is_jj_run { "jj" } else { "git" },
        "mode": run.mode.as_str(),
        // Whether this RudderPlan row is a TRANSIENT reconcile planner (vs the pinned
        // orchestrator). Persisted so an orphan left by a crash mid-reconcile is
        // identifiable on reload and filtered out instead of resurfacing as a second
        // orchestrator. Absent in old records → defaults to false (the real planner).
        "reconcilePlanner": run.reconcile_planner,
        "interactiveOrchestrator": run.interactive_orchestrator,
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
        "workspace": workspace,
        "currentPrompt": run.current_prompt,
        "turns": turns,
        "lastUserInputAt": run.last_user_input_at,
        "autoSteer": { "count": if run.autosteered { 1 } else { 0 }, "max": 2 },
        "reviewSourceIds": run.review_source_ids,
        "mergeResolver": run.merge_resolver,
        // Durable conflict history for telemetry: unlike the `merge` object below
        // (live state, dropped once the conflict resolves), this survives a
        // successful resolution so mergeConflictRate counts resolved conflicts.
        "hadMergeConflict": run.had_merge_conflict || run.merge_conflict,
        // The worker's own completion note summary, shown in the finished-worker
        // card (objective + what it did) and refreshed on every completion.
        "doneSummary": run.done_summary,
        // Plan node identity for scheduler-launched runs. `nodeId` enters the
        // merged set when this run merges; `planDeps` are the hard-dep node ids
        // this run was launched against (so a restored run keeps its gating).
        "nodeId": run.node_id,
        // The plan this run belongs to. Node ids repeat across concurrent plans, so
        // without this a restart could not tell whose node `nodeId` names.
        "planId": run.plan_id,
        "reviewedAt": run.reviewed_at,
        "planDeps": run.deps,
        "delivery": run.delivery.to_json(),
        "session": run.session_id.as_ref().map(|sid| serde_json::json!({ "nativeSessionId": sid })),
    });
    if let Some(gam) = run.gam.as_ref() {
        record["gam"] = gam_state_to_json(gam);
    }
    // Native and TS commands share run.json. Preserve fields owned by the TS
    // merge/undo/verifier layers instead of replacing the document with the
    // native projection on every UI transition.
    if let (Some(record_map), Some(existing_map)) =
        (record.as_object_mut(), existing_record.as_object())
    {
        for (key, value) in existing_map {
            record_map
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(pid) = run
        .terminal
        .as_ref()
        .and_then(TerminalPane::child_process_id)
    {
        record["process"] = serde_json::json!({
            "pid": pid,
            "controllerPid": pid,
            "backendPid": pid,
            "owner": "native-pty",
        });
    } else if let Some(map) = record.as_object_mut() {
        // A native-owned process exists only while this dashboard owns a live PTY.
        // Never preserve the previous process object after shutdown/reconciliation:
        // that was the source of dead PIDs being presented as running work.
        map.remove("process");
    }
    if run.merge_conflict {
        let conflict_kind = if run.merge_conflict_operation == ConflictOperation::Rebase {
            "rebase"
        } else {
            "merge"
        };
        if let Some(map) = record.as_object_mut() {
            map.insert(
                "merge".to_string(),
                serde_json::json!({
                    "status": "conflict",
                    "conflictKind": conflict_kind,
                    "conflictedFiles": run.merge_conflict_files,
                }),
            );
        }
    } else if run.status == AgentStatus::Merged {
        let mut merge = existing_record
            .get("merge")
            .cloned()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        merge["status"] = serde_json::json!("merged");
        if let Some(map) = merge.as_object_mut() {
            map.remove("conflictKind");
            map.remove("conflictedFiles");
            if let Some(bookmark) = run.integration.bookmark.as_ref() {
                map.insert("targetBranch".to_string(), serde_json::json!(bookmark));
            }
            if let Some(change) = run.integration.merge_change_id.as_ref() {
                map.insert("mergeChangeId".to_string(), serde_json::json!(change));
            }
            if let Some(commit) = run.integration.git_commit.as_ref() {
                map.insert("localCommit".to_string(), serde_json::json!(commit));
            }
            map.insert(
                "pushed".to_string(),
                serde_json::json!(run.integration.pushed),
            );
            if run.integration.pushed && !map.contains_key("pushedAt") {
                map.insert("pushedAt".to_string(), serde_json::json!(now));
            }
        }
        record["merge"] = merge;
    } else if let Some(map) = record.as_object_mut() {
        // A live conflict is the only non-terminal integration state native
        // owns. Do not retain an old conflict object after re-goal/retry.
        if map
            .get("merge")
            .and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str)
            == Some("conflict")
        {
            map.remove("merge");
        }
    }
    if run.terminal.is_none() && run.status != AgentStatus::Running {
        if let Some(map) = record.as_object_mut() {
            map.remove("process");
        }
    }
    // What Rudder DID when it published this row: the branch it pushed and the PR
    // it opened. Only the identity is written. The PR's state (draft/open/merged)
    // is GitHub's to answer and is re-derived on a poll, so persisting it here
    // would be exactly the stale-cached-answer failure §12.7 exists to prevent.
    if run.publish.is_published() || run.publish.branch.is_some() {
        let mut published = serde_json::json!({});
        if let Some(map) = published.as_object_mut() {
            if let Some(branch) = run.publish.branch.as_ref() {
                map.insert("branch".to_string(), serde_json::json!(branch));
            }
            if let Some(number) = run.publish.number {
                map.insert("number".to_string(), serde_json::json!(number));
            }
            if let Some(url) = run.publish.url.as_ref() {
                map.insert("url".to_string(), serde_json::json!(url));
            }
        }
        if let Some(map) = record.as_object_mut() {
            map.insert("publish".to_string(), published);
        }
    }
    // Best-effort token usage captured from the backend's session log at
    // completion. Like the TS __worker path, a run with no usage leaves the
    // `tokens` field absent rather than persisting zeros.
    if run.tokens_in + run.tokens_out > 0 {
        if let Some(map) = record.as_object_mut() {
            map.insert(
                "tokens".to_string(),
                serde_json::json!({ "input": run.tokens_in, "output": run.tokens_out }),
            );
        }
    }
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

/// Hard cap on a `rudder merge`/`rudder sync` shell-out: it runs on the UI thread, so a
/// hung jj/node child must be killed rather than freeze the TUI forever.
const RUDDER_JJ_CLI_TIMEOUT: Duration = Duration::from_secs(120);

/// Outcome of shelling out to a `rudder merge`/`rudder sync` jj run.
pub(crate) enum JjCliOutcome {
    Ok { integration: IntegrationEvidence },
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
    // This runs on the UI thread: never `Command::output()` (unbounded wait). A hung
    // jj/node child would freeze the whole TUI, so spawn + poll with a deadline and
    // kill the child if it blows through it.
    let mut child = match Command::new(&rudder)
        .arg(command)
        .arg(run_id)
        .args(extra)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return JjCliOutcome::Failed {
                error: format!("failed to run rudder {command}: {error}"),
            };
        }
    };
    // Drain both pipes from threads so a chatty child can never fill a pipe buffer and
    // deadlock against the wait loop (and so the readers see EOF after a kill).
    let read_pipe = |pipe: Option<Box<dyn Read + Send>>| {
        pipe.map(|mut pipe| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                buf
            })
        })
    };
    let stdout_reader = read_pipe(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr_reader = read_pipe(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + RUDDER_JJ_CLI_TIMEOUT;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap; closes the pipes so the readers finish
                    break true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return JjCliOutcome::Failed {
                    error: format!("failed to run rudder {command}: {error}"),
                };
            }
        }
    };
    let join = |reader: Option<thread::JoinHandle<Vec<u8>>>| {
        reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    let (cli_stdout, cli_stderr) = (join(stdout_reader), join(stderr_reader));
    if timed_out {
        return JjCliOutcome::Failed {
            error: format!(
                "rudder {command} timed out after {}s; the jj workspace may need attention",
                RUDDER_JJ_CLI_TIMEOUT.as_secs()
            ),
        };
    }

    let state = read_run_record_field(repo_root, run_id, state_field);
    let status = state
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str);
    match status {
        Some("merged") | Some("synced") => {
            let record = fs::read_to_string(native_run_dir(repo_root, run_id).join("run.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            JjCliOutcome::Ok {
                integration: integration_evidence_from_record(&record),
            }
        }
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
            let stderr = String::from_utf8_lossy(&cli_stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&cli_stdout).trim().to_string();
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
pub(crate) struct WorkspaceInfo {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) path_is_workspace: bool,
    /// jj workspace name for the run (e.g. `rudder-<id>-<hash>`). Present for new
    /// runs isolated in a jj workspace; `None` for the dedicated main agent.
    pub(crate) workspace_name: Option<String>,
    /// The jj change id of the workspace's working-copy commit at launch.
    pub(crate) jj_change_id: Option<String>,
}

impl WorkspaceInfo {
    pub(crate) fn current(path: PathBuf) -> Self {
        Self {
            id: new_run_id("task"),
            path,
            branch: None,
            path_is_workspace: false,
            workspace_name: None,
            jj_change_id: None,
        }
    }
}

/// Isolate a worker in a jj workspace instead of a git worktree. Shells out to
/// `rudder __launch-node` (the TS shim that owns the jj logic via src/jj.ts) so
/// the worker runs against a per-node jj workspace whose change id is captured
/// for merge/sync. The returned WorkspaceInfo carries the workspace path, name,
/// and jj change id; `branch` stays empty for jj runs. New runs never silently
/// fall back to git: if the shim is unavailable or fails, this surfaces a clear
/// error to the caller.
pub(crate) fn prepare_jj_workspace(cwd: &Path, task: &str) -> Result<WorkspaceInfo> {
    prepare_jj_workspace_at(cwd, task, None)
}

/// Like `prepare_jj_workspace`, but when `base_change` is given the new workspace's
/// working copy is parented on that jj change (via `__launch-node --base`), so the
/// run starts from another run's current edits instead of the repo's default base.
/// Used by branch/fork so the forked chat's file state matches its conversation.
/// Is this change still an ancestor of trunk (or of the working copy)? The
/// question "did this merge land, and is it STILL landed?" belongs to jj, which
/// answers it exactly and cheaply. Rudder used to store the answer as a flag at
/// merge time and never look again — so a row went on claiming "merged" after
/// history was rewritten under it. None means jj could not be asked.
pub(crate) fn jj_change_in_trunk(repo_root: &Path, change_id: &str) -> Option<bool> {
    let change_id = change_id.trim();
    if change_id.is_empty() || !is_git_repo(repo_root) {
        return None;
    }
    let output = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "--ignore-working-copy",
            "-r",
            &format!("{change_id} & ::(trunk() | @)"),
            "-T",
            "change_id.short()",
        ])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Which of `change_ids` are still reachable from trunk, answered in ONE jj call.
///
/// The per-row version spawned a `jj log` for every merged row on every reconcile
/// tick. jj's own work here is trivial; the cost is process startup, so it scaled
/// with the number of merged rows and ran on the UI thread — measured at ~206ms
/// for 16 rows versus ~10ms for the same question asked once. A repo with a long
/// history of merged agents turned that into a visible periodic stall.
///
/// Ids absent from the answer are reported false. An id jj cannot resolve at all
/// is simply absent, which is the same conclusion the per-row version reached.
pub(crate) fn jj_changes_in_trunk(
    repo_root: &Path,
    change_ids: &[String],
) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    let ids: Vec<&str> = change_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() || !is_git_repo(repo_root) {
        return out;
    }
    // Chunked so a repo with hundreds of merged rows cannot build a revset long
    // enough to hit an argument-length limit.
    const CHUNK: usize = 128;
    for chunk in ids.chunks(CHUNK) {
        let union = chunk.join("|");
        let Ok(output) = Command::new("jj")
            .args([
                "log",
                "--no-graph",
                "--ignore-working-copy",
                "-r",
                &format!("({union}) & ::(trunk() | @)"),
                "-T",
                "change_id.short() ++ \"\\n\"",
            ])
            .current_dir(repo_root)
            .stdin(Stdio::null())
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            // A single unresolvable id fails the whole revset, so fall back to
            // asking per id rather than silently reporting the batch as gone.
            for id in chunk {
                if let Some(answer) = jj_change_in_trunk(repo_root, id) {
                    out.insert((*id).to_string(), answer);
                }
            }
            continue;
        }
        let found: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        for id in chunk {
            // jj prints a SHORT change id; the stored one may be the full id, so
            // match on whichever is the prefix of the other.
            let present = found
                .iter()
                .any(|value| value.starts_with(*id) || id.starts_with(value.as_str()));
            out.insert((*id).to_string(), present);
        }
    }
    out
}

pub(crate) fn prepare_jj_workspace_at(
    cwd: &Path,
    task: &str,
    base_change: Option<&str>,
) -> Result<WorkspaceInfo> {
    let repo = dashboard_root(cwd);
    if !is_git_repo(&repo) {
        return Ok(WorkspaceInfo::current(cwd.to_path_buf()));
    }

    let id = new_run_id(task);
    let rudder = locate_rudder_cli()
        .context("rudder CLI not found on PATH; cannot create a jj workspace for the run")?;
    let mut launch = Command::new(&rudder);
    launch
        .arg("__launch-node")
        .arg("--repo")
        .arg(&repo)
        .arg("--task")
        .arg(task)
        .arg("--node")
        .arg(&id);
    if let Some(base) = base_change.map(str::trim).filter(|base| !base.is_empty()) {
        launch.arg("--base").arg(base);
    }
    let output = launch
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

    Ok(WorkspaceInfo {
        id,
        path: PathBuf::from(path),
        branch: None,
        path_is_workspace: true,
        workspace_name,
        jj_change_id,
    })
}

pub(crate) fn repo_root(cwd: &Path) -> PathBuf {
    git_output(cwd, ["rev-parse", "--show-toplevel"])
        .map(|root| PathBuf::from(root.trim()))
        .unwrap_or_else(|_| cwd.to_path_buf())
}

pub(crate) fn dashboard_root(cwd: &Path) -> PathBuf {
    let repo = repo_root(cwd);
    main_workspace_root(&repo).unwrap_or(repo)
}

pub(crate) fn main_workspace_root(repo: &Path) -> Option<PathBuf> {
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
    // Whether a path is inside a work tree changes only when a repo is created or
    // removed under it, yet this was a subprocess on EVERY call — 68 of them in a
    // single traced session. Same stamp-keyed cache as the other git answers.
    cached_git_state(cwd, "is-work-tree", || {
        Some(
            git_output(cwd, ["rev-parse", "--is-inside-work-tree"])
                .map(|value| value.trim() == "true")
                .unwrap_or(false)
                .to_string(),
        )
    })
    .as_deref()
        == Some("true")
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

pub(crate) fn premerge_review_all_sources(
    cwd: &Path,
    sources: &[ReviewAllSource],
) -> ReviewAllPremerge {
    let mut premerge = ReviewAllPremerge::default();
    for (index, source) in sources.iter().enumerate() {
        let output = Command::new("jj")
            .args([
                "new",
                "@",
                source.revision.as_str(),
                "-m",
                "rudder: review-all aggregate",
            ])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => {
                premerge.merged_revisions.push(source.revision.clone());
                let conflicts = jj_unresolved_conflicts(cwd);
                if !conflicts.is_empty() {
                    premerge.stopped_revision = Some(source.revision.clone());
                    premerge.stopped_error = Some(format!(
                        "aggregate change has conflicts in {}",
                        conflicts.join(", ")
                    ));
                    premerge.remaining_revisions = sources[index + 1..]
                        .iter()
                        .map(|item| item.revision.clone())
                        .collect();
                    break;
                }
            }
            Ok(output) => {
                premerge.stopped_revision = Some(source.revision.clone());
                premerge.stopped_error =
                    Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
                premerge.remaining_revisions = sources[index..]
                    .iter()
                    .map(|item| item.revision.clone())
                    .collect();
                break;
            }
            Err(error) => {
                premerge.stopped_revision = Some(source.revision.clone());
                premerge.stopped_error = Some(error.to_string());
                premerge.remaining_revisions = sources[index..]
                    .iter()
                    .map(|item| item.revision.clone())
                    .collect();
                break;
            }
        }
    }
    premerge
}

pub(crate) fn jj_workspace_change_id(cwd: &Path) -> Option<String> {
    let output = Command::new("jj")
        .args(["log", "--no-graph", "-r", "@", "-T", "change_id"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
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

/// Files with unresolved jj conflicts in `cwd` (via `jj resolve --list`). Empty when
/// the working copy is conflict-free. `jj resolve --list` exits non-zero and prints
/// to stderr when there are no conflicts, so a failed/empty stdout means "none".
/// The repo-relative paths a jj workspace has modified in its working-copy change
/// (`jj diff --name-only`), excluding Rudder's own coordination files. Cheap; used
/// to predict cross-agent collisions before they reach a merge. Empty on any error
/// or non-jj cwd.
pub(crate) fn jj_touched_files(cwd: &Path) -> Vec<String> {
    let Some(stdout) = run_jj_with_stale_recovery(cwd, &["diff", "--name-only", "--no-pager"])
    else {
        return Vec::new();
    };
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            // Rudder writes RUDDER.md / DECISIONS.md as coordination surfaces; they
            // are not real cross-agent collisions.
            !line.ends_with("RUDDER.md") && !line.ends_with("DECISIONS.md")
        })
        .map(ToOwned::to_owned)
        .collect()
}

/// The worker's working-copy diff as git-format text, capped to `max_chars` (the caller
/// bounds the summarizer prompt). Used by the completion-note backstop to reconstruct a
/// report for an agent that finished without filing one. Empty on any failure.
pub(crate) fn jj_diff_text(cwd: &Path, max_chars: usize) -> String {
    let Some(text) = run_jj_with_stale_recovery(cwd, &["diff", "--no-pager", "--git"]) else {
        return String::new();
    };
    if text.chars().count() > max_chars {
        let head: String = text.chars().take(max_chars).collect();
        format!("{head}\n...(diff truncated)...")
    } else {
        text
    }
}

/// Run a read-only jj command in `cwd`, recovering ONCE from a stale working
/// copy. Sibling workspaces snapshotting concurrently routinely leave a worker
/// workspace stale; without recovery every jj read here returns empty and the
/// UI silently shows "no changes" for a workspace full of edits. Mirrors the
/// TS-side recoverStaleWorkspace (src/jj.ts). None on any other failure.
fn run_jj_with_stale_recovery(cwd: &Path, args: &[&str]) -> Option<String> {
    let run = || {
        Command::new("jj")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
    };
    let output = run().ok()?;
    if output.status.success() {
        return Some(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.contains("stale") {
        return None;
    }
    let _ = Command::new("jj")
        .args(["workspace", "update-stale"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output();
    let retry = run().ok()?;
    if !retry.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&retry.stdout).into_owned())
}

/// Best-effort: set a jj workspace's working-copy change description to reflect
/// the agent's status, so `jj log` reads like a live coordination board
/// (`rudder[n3]: fix memory leak [done]`) instead of "(no description set)".
/// No-op for non-jj runs (the main agent, legacy git worktrees) and on any error
/// — a description update must never disturb the run.
pub(crate) fn describe_workspace_status(run: &AgentRun, status: &str) {
    if run.workspace_name.is_none() {
        return;
    }
    let label = run
        .node_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!("rudder[{id}]"))
        .unwrap_or_else(|| "rudder".to_string());
    let task = run.task_summary.trim();
    let task = if task.is_empty() {
        run.task.trim()
    } else {
        task
    };
    let summary: String = task.chars().take(72).collect();
    let message = if summary.trim().is_empty() {
        format!("{label} [{status}]")
    } else {
        format!("{label}: {summary} [{status}]")
    };
    let _ = Command::new("jj")
        .args(["describe", "-m", &message])
        .current_dir(&run.cwd)
        .stdin(Stdio::null())
        .output();
}

/// Tear down a closed/merged jj workspace: forget it from jj's registry AND
/// remove its on-disk directory (a full working-copy checkout). The interactive
/// `dd` delete used to run `git worktree remove` here, which fails on a jj
/// workspace and leaked both the directory and the registry entry.
///
/// SAFETY: only ever removes a directory that sits under `.rudder-workspaces`.
/// A record with a bad/empty path can never delete the main checkout or an
/// unrelated tree — the function refuses and returns an error instead.
pub(crate) fn forget_jj_workspace(
    repo_root: &Path,
    workspace_name: &str,
    path: &Path,
) -> Result<()> {
    if !is_under_agent_workspaces(path) {
        anyhow::bail!(
            "refusing to remove a workspace outside .rudder-workspaces: {}",
            path.display()
        );
    }
    // Forget from jj first (best-effort: a workspace jj already dropped is fine).
    if !workspace_name.trim().is_empty() {
        let _ = Command::new("jj")
            .args(["workspace", "forget", workspace_name])
            .current_dir(repo_root)
            .stdin(Stdio::null())
            .output();
    }
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

pub(crate) fn jj_unresolved_conflicts(cwd: &Path) -> Vec<String> {
    let Ok(output) = Command::new("jj")
        .args(["resolve", "--list"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // `jj resolve --list` lines look like "path/to/file    2-sided conflict";
        // take the leading path token.
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect()
}

/// Change ids of the working-copy commit's parents. A 2-sided merge conflict has
/// exactly two parents (the two sides being integrated); octopus merges have more.
pub(crate) fn jj_parent_revs(cwd: &Path) -> Vec<String> {
    let Ok(output) = Command::new("jj")
        .args([
            "log",
            "--no-graph",
            "--no-pager",
            "-r",
            "@-",
            "-T",
            "change_id.short() ++ \"\\n\"",
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Contents of `path` at revision `rev`, or None if the file does not exist there
/// (e.g. one side of a modify/delete conflict).
pub(crate) fn jj_file_show(cwd: &Path, rev: &str, path: &str) -> Option<String> {
    let output = Command::new("jj")
        .args(["file", "show", "--no-pager", "-r", rev, path])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Resolve the MECHANICAL conflicts in `conflicted` directly, before any LLM resolver
/// is spawned, and return the conflicts that still remain. The mechanical cases are the
/// ones that collide on nearly every parallel feature merge yet have an unambiguous
/// answer: `.gitignore` (union of both sides' rules) and `package.json` (deep-union of
/// dependencies/scripts). Regenerable lock files take the larger side. Everything else
/// is left untouched for the LLM resolver, so the worst case is exactly today's behavior.
///
/// Resolutions are written into the working copy; jj records them on the next snapshot
/// (the trailing `jj_unresolved_conflicts` call), which also yields the authoritative
/// remaining set.
pub(crate) fn auto_resolve_mechanical_conflicts(cwd: &Path, conflicted: &[String]) -> Vec<String> {
    let parents = jj_parent_revs(cwd);
    // Need at least two sides to merge. Handles N-SIDED conflicts (a stacked octopus merge
    // can record 3+ sides), folding the mechanical merge across every parent.
    if parents.len() < 2 || conflicted.is_empty() {
        return conflicted.to_vec();
    }
    let mut resolved_any = false;
    for path in conflicted {
        let Some(merged) = mechanical_merge_for(cwd, path, &parents) else {
            continue;
        };
        // NEVER snapshot a "resolution" that still carries conflict markers (e.g. a
        // side that was itself conflicted). Doing so would commit literal
        // <<<<<<</=======/>>>>>>> lines into history and a reader before reinstall
        // would see a corrupt file. Leave it to the LLM resolver instead.
        if contains_conflict_markers(&merged) {
            continue;
        }
        if std::fs::write(cwd.join(path), merged).is_ok() {
            resolved_any = true;
        }
    }
    if !resolved_any {
        return conflicted.to_vec();
    }
    // Snapshot + re-list: jj is the source of truth for what is still conflicted.
    jj_unresolved_conflicts(cwd)
}

/// The mechanical resolution for one conflicted path across ALL parent sides, or None to
/// leave it to the LLM resolver. Keyed on file name so it works at any depth in the tree.
fn mechanical_merge_for(cwd: &Path, path: &str, parents: &[String]) -> Option<String> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Each side's content (a side that lacks the file reads as empty — fine for union/longest).
    let sides: Vec<String> = parents
        .iter()
        .map(|rev| jj_file_show(cwd, rev, path).unwrap_or_default())
        .collect();
    match name {
        // Rudder's own coordination files are NEVER a real product conflict and must never
        // block integration: RUDDER.md is regenerated by the orchestrator immediately after
        // the merge, DECISIONS.md is a union of agent-authored entries, and RUDDER_SHARED.md
        // is an append-style local context file. This is the dominant cause of N-sided
        // conflict pileups, since every worker workspace carries its own copy.
        "DECISIONS.md" => merge_decisions_sides(&sides),
        "RUDDER.md" | "RUDDER_SHARED.md" => sides
            .into_iter()
            .filter(|side| !contains_conflict_markers(side))
            .max_by_key(String::len),
        // .gitignore (and nested *.gitignore): append-only rule lists; union all sides.
        ".gitignore" => Some(
            sides
                .iter()
                .fold(String::new(), |acc, side| union_gitignore_like(&acc, side)),
        ),
        // package.json: deep-union deps/devDeps/scripts across every side; scalar ties keep
        // the earlier side. Bail (None -> LLM resolver) if any non-empty side is invalid JSON.
        "package.json" => {
            let mut acc = String::from("{}");
            let mut saw = false;
            for side in &sides {
                if side.trim().is_empty() {
                    continue;
                }
                acc = merge_package_json(&acc, side)?;
                saw = true;
            }
            saw.then_some(acc)
        }
        // Regenerable lock files: take the most complete side; install/build rebuilds it.
        "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" => sides
            .into_iter()
            .filter(|side| !contains_conflict_markers(side))
            .max_by_key(String::len),
        _ => None,
    }
}

/// True if `content` still carries conflict markers — either git's
/// (`<<<<<<<` / `=======` / `>>>>>>>`) or jj's materialized form
/// (`<<<<<<<` / `%%%%%%%` / `+++++++` / `>>>>>>>`). A mechanical "resolution"
/// containing any of these must be rejected so it is never written back into the
/// working copy and snapshotted as history.
pub(crate) fn contains_conflict_markers(content: &str) -> bool {
    content.lines().any(is_conflict_marker_line)
}

pub(crate) fn merge_decisions_sides(sides: &[String]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for side in sides {
        for entry in extract_decision_entries(side) {
            let normalized = entry.trim();
            if normalized.is_empty() || contains_conflict_markers(normalized) {
                continue;
            }
            if seen.insert(normalized.to_string()) {
                entries.push(normalized.to_string());
            }
        }
    }
    if entries.is_empty() {
        return sides
            .iter()
            .filter(|side| !contains_conflict_markers(side))
            .max_by_key(|side| side.len())
            .cloned();
    }
    let mut out = String::from("# Decisions\n\nShared, agent-authored log of cross-cutting decisions the fleet must honor. The conductor records plan/steer decisions here; workers record interface contracts + adjustments. Re-read before each significant step.\n\n");
    out.push_str(&entries.join("\n\n"));
    out.push('\n');
    Some(out)
}

fn extract_decision_entries(content: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in content.lines() {
        if is_conflict_marker_line(line) {
            continue;
        }
        if line.starts_with("## ") {
            if !current.is_empty() {
                entries.push(current.join("\n"));
            }
            current = vec![line.to_string()];
            continue;
        }
        if !current.is_empty() {
            current.push(line.to_string());
        }
    }
    if !current.is_empty() {
        entries.push(current.join("\n"));
    }
    entries
}

fn is_conflict_marker_line(line: &str) -> bool {
    line.starts_with("<<<<<<<")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
        || line.starts_with("%%%%%%%")
        || line.starts_with("+++++++")
}

/// Union two `.gitignore`-style files: keep the left side's lines in order, then append
/// any of the right side's lines not already present. Dedupes by trimmed content and
/// collapses runs of blank lines so the result stays tidy.
pub(crate) fn union_gitignore_like(left: &str, right: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for line in left.lines().chain(right.lines()) {
        let key = line.trim();
        if key.is_empty() {
            if out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                continue; // collapse leading / consecutive blanks
            }
            out.push(String::new());
        } else if seen.insert(key.to_string()) {
            out.push(line.to_string());
        }
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Deep-union two `package.json` documents: objects merge key-by-key (so dependencies,
/// devDependencies, and scripts accumulate from both sides), arrays union preserving
/// order, and scalar conflicts keep the left (base) side. Returns None if either side is
/// not valid JSON, in which case the LLM resolver handles it.
pub(crate) fn merge_package_json(left: &str, right: &str) -> Option<String> {
    let l: serde_json::Value = serde_json::from_str(left).ok()?;
    let r: serde_json::Value = serde_json::from_str(right).ok()?;
    let merged = deep_merge_json(&l, &r);
    let mut s = serde_json::to_string_pretty(&merged).ok()?;
    s.push('\n');
    Some(s)
}

fn deep_merge_json(left: &serde_json::Value, right: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            let mut out = l.clone();
            for (k, rv) in r {
                let merged = match out.get(k) {
                    Some(lv) => deep_merge_json(lv, rv),
                    None => rv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(l), Value::Array(r)) => {
            let mut out = l.clone();
            for item in r {
                if !out.contains(item) {
                    out.push(item.clone());
                }
            }
            Value::Array(out)
        }
        // Scalar (or type mismatch): keep the base/left side for determinism.
        _ => left.clone(),
    }
}

pub(crate) fn diff_short_summary_at(cwd: &Path) -> Option<String> {
    // Worker cwds are jj workspaces under .rudder-workspaces with no .git of
    // their own; git silently walks UP to the enclosing repo and reports the
    // MAIN checkout's diff on every worker row. Ask jj about jj workspaces.
    if cwd.join(".jj").is_dir() && !cwd.join(".git").exists() {
        return jj_diff_short_summary(cwd);
    }
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

/// The roll-up line of `jj diff --stat` for the workspace's working-copy
/// change ("3 files changed, 10 insertions(+), 2 deletions(-)"). None when the
/// change is empty or jj fails.
fn jj_diff_short_summary(cwd: &Path) -> Option<String> {
    let stdout = run_jj_with_stale_recovery(cwd, &["diff", "--stat", "--no-pager"])?;
    let summary = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();
    if summary.starts_with("0 files changed") {
        return None;
    }
    Some(summary)
}

pub(crate) fn play_completion_sound() {
    if !config::completion_sound_enabled() {
        return;
    }
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

/// What Rudder did with the control markers the orchestrator most recently wrote,
/// newest first. Rudder strips marker lines from this file as it consumes them, so
/// without this section the conductor cannot distinguish "acted" from "rejected":
/// a marker aimed at a stale node id vanishes exactly like one that worked, and the
/// conductor goes on to report a success that never happened.
pub(crate) fn append_control_results(body: &mut String, control_results: &[(String, String)]) {
    if control_results.is_empty() {
        return;
    }
    body.push_str("\n## Your last control markers, and what they did (newest first)\n\nRudder removes each marker line from this file as it runs it, so a missing line only means it was CONSUMED, not that it succeeded. These are the actual outcomes. Check them before telling the user an action landed, and re-issue anything that reports a failure or a zero count.\n");
    for (marker, outcome) in control_results.iter().rev().take(10) {
        body.push_str(&format!("- `{marker}` -> {outcome}\n"));
    }
}

/// `recent_instructions` is the tail of the user's task history (newest last):
/// every agent workspace mirrors this file, so a freshly spawned `/run` worker
/// opens with a one-page digest of the session — what the user has been asking
/// for and (via each agent's `did=` line) what actually got done.
pub(crate) fn write_rudder_context_with_history(
    repo_root: &Path,
    agents: &[AgentRun],
    pending: Option<&WorkspaceInfo>,
    recent_instructions: &[String],
    final_gates: &[(FinalGateStatus, Option<&str>)],
    control_results: &[(String, String)],
) -> Result<()> {
    ensure_gitignore_contains(repo_root, "RUDDER.md")?;
    ensure_gitignore_contains(repo_root, RUDDER_SHARED_CONTEXT_FILE)?;
    // Workspaces now live INSIDE the project at <repo>/.rudder-workspaces; ignore them in the
    // USER's repo (only Rudder's own repo previously listed it), or each worker's checkout
    // would show up as untracked files in the parent and could be committed/cleaned.
    ensure_gitignore_contains(repo_root, &format!("{AGENT_WORKSPACES_DIR}/"))?;
    ensure_gitignore_contains(repo_root, &format!("{LEGACY_AGENT_WORKSPACES_DIR}/"))?;
    let mut body = String::from("# Rudder-Specific Context\n\nThis file is generated by Rudder. It is not user-authored repo documentation. Use it to coordinate with other Rudder agents in this checkout.\n\n");
    append_global_job_snapshot(&mut body, agents, pending);
    append_ship_status(&mut body, repo_root, agents);
    // One line per PLAN that has reached its gate: several plans run at once and each
    // verifies only its own merged nodes, so a single verdict could not speak for them.
    if !final_gates.is_empty() {
        body.push_str("\n## Final plan verification\n");
        for (status, summary) in final_gates {
            body.push_str(&format!(
                "- status={}{}\n",
                match status {
                    FinalGateStatus::Idle => "waiting",
                    FinalGateStatus::Running => "running",
                    FinalGateStatus::Passed => "passed",
                    FinalGateStatus::Failed => "failed",
                },
                summary
                    .map(|value| format!(" summary=\"{}\"", preview_text(value, 240)))
                    .unwrap_or_default()
            ));
        }
    }
    // Legend for the `where=` token on every roster row below. Without it a worker
    // reads sibling rows and assumes one shared tree — then looks for a sibling's
    // edits that are not in its workspace, or tries to hand work over by writing a
    // file into a directory nothing else can see. Stated once here rather than per
    // row: RUDDER.md is re-read every turn, so the rows stay one line each.
    body.push_str("\n## Where each agent works\n\nEvery row below carries `where=`. `where=workspace` means that agent has its OWN jj workspace — a private copy of the repo at `.rudder-workspaces/<name>` (its `cwd=`), isolated from the user's checkout and from every other agent; its work reaches the user only when Rudder integrates it. `where=main-checkout` means that agent edits the user's REAL files in place: several agents can share that tree, and they have nothing to merge.\n\nSo: a sibling's uncommitted edits are NOT in your tree and you cannot read them, waiting on a file a sibling is \"about to write\" will hang forever, and writing into another agent's workspace to hand work over does not work. Coordinate through this file, not the filesystem.\n");
    body.push_str("\n## Active local Rudder agents\n");
    let live_agents = agents
        .iter()
        .filter(|agent| agent_is_live_for_context(agent))
        .collect::<Vec<_>>();
    if live_agents.is_empty() && pending.is_none() {
        body.push_str("- none\n");
    }
    for agent in live_agents {
        append_agent_context_line(&mut body, agent);
    }
    if let Some(workspace) = pending {
        body.push_str(&format!(
            "- starting node=- mode=execute status=starting backend=pending model=pending cwd={} where={} branch={}\n",
            workspace.path.display(),
            if workspace.path_is_workspace {
                "workspace"
            } else {
                "main-checkout"
            },
            workspace.branch.as_deref().unwrap_or("current")
        ));
    }
    let ready_agents = agents
        .iter()
        .filter(|agent| agent_awaits_action_for_context(agent))
        .collect::<Vec<_>>();
    if !ready_agents.is_empty() {
        body.push_str("\n## Ready local Rudder agents\n");
        for agent in ready_agents {
            append_agent_context_line(&mut body, agent);
        }
    }
    let completed_agents = agents
        .iter()
        .filter(|agent| agent_is_completed_for_context(agent))
        .collect::<Vec<_>>();
    if !completed_agents.is_empty() {
        body.push_str("\n## Completed local Rudder agents\n");
        for agent in completed_agents {
            append_agent_context_line(&mut body, agent);
        }
    }
    // SESSION MEMORY: the user's recent instructions, newest first. Combined with
    // the `did=` summaries above, a brand-new agent opens knowing what this
    // session has been about without inheriting any other agent's full chat.
    let instructions = recent_instructions
        .iter()
        .rev()
        .filter(|instruction| !instruction.trim_start().starts_with('/'))
        .take(10)
        .collect::<Vec<_>>();
    if !instructions.is_empty() {
        body.push_str("\n## Recent user instructions (newest first)\n");
        for instruction in instructions {
            body.push_str(&format!("- {}\n", preview_text(instruction, 200)));
        }
    }
    append_control_results(&mut body, control_results);
    body.push_str(
        "\nRead this file before making changes so you know what other Rudder agents are doing. If RUDDER_SHARED.md exists beside it, read that too; it is Rudder's gitignored file for user-shared credentials, API tokens, private URLs, and other local context that must survive model compaction.\n",
    );
    body.push_str(
        "\n## Version control\n\nThis repo uses jj (Jujutsu), colocated with git. Workspace agents each have their own jj workspace, main-checkout agents share the user's; jj is authoritative. Inspect changes with `jj status` / `jj diff`. Run `jj log` to see every agent's workspace and its description — Rudder sets each workspace's change description to what that agent is doing and its status (e.g. `rudder[n3]: fix memory leak [done]`), so read them to see where sibling work is and avoid stepping on the same files.\n\nIMPLEMENTATION nodes: do NOT run commit/branch/merge yourself — Rudder snapshots and integrates your working copy, and pushing partial node work is wrong.\n\nSHIPPING (push / publish / deploy) is NOT automatic and is a separate step: Rudder integrates work into LOCAL git only — it NEVER pushes to a remote or deploys. If your task is to push, publish, or deploy, that IS your job: push the integrated result with `jj git push` (or `git push`), then run the project's deploy command (or rely on the host auto-deploying from the pushed remote, e.g. Netlify/Vercel). `merged` means local git only — nothing is on the remote or live until you push and deploy, so verify the actual remote/host before reporting something as deployed.\n",
    );
    {
        // One lock per write batch at the repo root (the TS side locks the same
        // way), covering the workspace copies too. Released on drop.
        let _lock = acquire_rudder_md_lock(repo_root);
        for workspace in rudder_context_workspaces(repo_root, agents, pending) {
            write_merged_rudder_md(&workspace.join("RUDDER.md"), &body)?;
        }
    }
    sync_shared_context_surfaces(repo_root, agents, pending)?;
    Ok(())
}

fn append_agent_context_line(body: &mut String, agent: &AgentRun) {
    let current_prompt = if agent.current_prompt != agent.task {
        format!(" current=\"{}\"", preview_text(&agent.current_prompt, 140))
    } else {
        String::new()
    };
    // The worker's own account of what it did (its completion note). This is what
    // turns the roster into session memory: a new agent sees not just WHO ran,
    // but what each finished run actually produced.
    let did = agent
        .done_summary
        .as_deref()
        .map(|summary| format!(" did=\"{}\"", preview_text(summary, 200)))
        .unwrap_or_default();
    let node = agent.node_id.as_deref().unwrap_or("-");
    let waiting = agent_waiting_label(agent);
    let deps = agent_deps_label(agent);
    let merge_state = match agent.status {
        AgentStatus::Done if agent_is_mergeable_worker(agent) => " integration=ready".to_string(),
        AgentStatus::Merged => format!(
            " integration={} jj={} branch={} git={} remote={}",
            agent.integration.phase.as_str(),
            agent
                .integration
                .merge_change_id
                .as_deref()
                .unwrap_or("unknown"),
            agent.integration.bookmark.as_deref().unwrap_or("unknown"),
            agent.integration.git_commit.as_deref().unwrap_or("unknown"),
            if agent.integration.pushed {
                "pushed"
            } else {
                "not-pushed"
            }
        ),
        _ => String::new(),
    };
    let delivery_state = if agent.delivery.required {
        format!(
            " delivery={} target=\"{}\" revision={} verified-at={} checks={}",
            agent.delivery.status.as_str(),
            preview_text(
                agent.delivery.target.as_deref().unwrap_or("not-recorded"),
                120
            ),
            agent.delivery.revision.as_deref().unwrap_or("not-recorded"),
            agent
                .delivery
                .verified_at
                .as_deref()
                .unwrap_or("not-recorded"),
            agent.delivery.checks.len(),
        )
    } else {
        String::new()
    };
    body.push_str(&format!(
        "- {} node={} mode={} status={}{} backend={} model={} cwd={} where={}{}{}{} task=\"{}\"{}{}\n",
        agent.id,
        node,
        agent.mode.as_str(),
        agent.lifecycle_label(),
        waiting,
        agent.backend.as_str(),
        agent.model,
        agent.cwd.display(),
        agent_location_label(agent),
        deps,
        merge_state,
        delivery_state,
        preview_text(&agent.task, 140),
        current_prompt,
        did
    ));
}

/// Which tree a row's files actually live in. Derived from the cwd rather than
/// from mode/workspace_name because the path is ground truth: workspaces are
/// always created under `<repo>/.rudder-workspaces`, so this cannot drift out of
/// sync with the spawn bookkeeping the way a mode lookup table would.
fn agent_location_label(agent: &AgentRun) -> &'static str {
    if is_under_agent_workspaces(&agent.cwd) {
        "workspace"
    } else {
        "main-checkout"
    }
}

fn agent_is_live_for_context(agent: &AgentRun) -> bool {
    matches!(agent.status, AgentStatus::Running | AgentStatus::Migrated)
        || agent.needs_permission
        || agent.needs_user_input
}

fn agent_is_completed_for_context(agent: &AgentRun) -> bool {
    matches!(
        agent.status,
        AgentStatus::Merged
            | AgentStatus::Failed
            | AgentStatus::Stopped
            | AgentStatus::Paused
            | AgentStatus::Orphaned
    ) || (agent.status == AgentStatus::Done && !agent_awaits_action_for_context(agent))
}

fn agent_awaits_action_for_context(agent: &AgentRun) -> bool {
    agent.status == AgentStatus::Done
        && (agent_is_mergeable_worker(agent)
            || (agent.delivery.required && !agent.delivery.is_verified()))
}

fn rudder_context_workspaces(
    repo_root: &Path,
    agents: &[AgentRun],
    pending: Option<&WorkspaceInfo>,
) -> Vec<PathBuf> {
    let mut workspaces = agents
        .iter()
        .map(|agent| agent.cwd.clone())
        .collect::<Vec<_>>();
    if let Some(workspace) = pending.filter(|workspace| workspace.path_is_workspace) {
        workspaces.push(workspace.path.clone());
    }
    workspaces.push(repo_root.to_path_buf());
    workspaces.sort();
    workspaces.dedup();
    workspaces
        .into_iter()
        .filter(|workspace| workspace.exists())
        .collect()
}

/// Answer "is it deployed?" directly. `merged` in the snapshot above means
/// merged into LOCAL git only — it is NOT pushed and NOT deployed until the
/// commits leave the machine. This section computes whether local work has
/// actually been pushed, so the orchestrator (and the user reading RUDDER.md)
/// can tell shipped from merged instead of conflating them.
fn append_ship_status(body: &mut String, repo_root: &Path, agents: &[AgentRun]) {
    let branch = current_branch_at(repo_root).unwrap_or_else(|| "HEAD".to_string());
    // Prefer the branch's own upstream; fall back to a matching origin ref.
    let ref_exists = |candidate: &str| {
        git_output_args(repo_root, &["rev-parse", "--verify", "--quiet", candidate])
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    };
    let compare_ref = git_output(
        repo_root,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty() && !value.contains("HEAD"))
    .or_else(|| {
        [
            format!("origin/{branch}"),
            "origin/main".to_string(),
            "origin/master".to_string(),
        ]
        .into_iter()
        .find(|candidate| ref_exists(candidate))
    });

    body.push_str("\n## Ship status (merged ≠ pushed ≠ deployed)\n");
    match compare_ref {
        None => {
            body.push_str(
                "- pushed: NO REMOTE; this repo has never been pushed. Every commit lives only on this machine.\n",
            );
        }
        Some(remote) => {
            let ahead = git_output_args(
                repo_root,
                &["rev-list", "--count", &format!("{remote}..HEAD")],
            )
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
            let behind = git_output_args(
                repo_root,
                &["rev-list", "--count", &format!("HEAD..{remote}")],
            )
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
            if ahead == 0 && behind == 0 {
                body.push_str(&format!(
                    "- pushed: YES; {branch} is up to date with {remote}.\n"
                ));
            } else if ahead == 0 {
                body.push_str(&format!(
                    "- pushed: REMOTE AHEAD; {branch} is {behind} commit(s) behind {remote}; local HEAD is contained by the remote.\n"
                ));
            } else if behind > 0 {
                body.push_str(&format!(
                    "- pushed: DIVERGED; {branch} has {ahead} unpushed commit(s) and is {behind} commit(s) behind {remote}.\n"
                ));
            } else {
                body.push_str(&format!(
                    "- pushed: NO; {ahead} local commit(s) on {branch} are not on {remote}.\n"
                ));
            }
        }
    }
    if let Some(delivery) = agents
        .iter()
        .filter(|agent| agent.delivery.required)
        .max_by(|left, right| left.last_user_input_at.cmp(&right.last_user_input_at))
        .map(|agent| &agent.delivery)
    {
        if delivery.is_verified() {
            body.push_str(&format!(
                "- deployed: VERIFIED; kind={} target=\"{}\" revision={} verified-at={} checks={}.\n",
                delivery.kind.as_deref().unwrap_or("custom"),
                preview_text(delivery.target.as_deref().unwrap_or("unknown"), 160),
                delivery.revision.as_deref().unwrap_or("not-recorded"),
                delivery.verified_at.as_deref().unwrap_or("unknown"),
                delivery.checks.len(),
            ));
        } else {
            body.push_str(&format!(
                "- deployed: NO VERIFIED PROOF; requested delivery is {} (target={}, verified-at={}, checks={}).\n",
                delivery.status.as_str(),
                delivery.target.as_deref().unwrap_or("not-recorded"),
                delivery.verified_at.as_deref().unwrap_or("not-recorded"),
                delivery.checks.len(),
            ));
        }
    } else {
        body.push_str("- deployed: NOT CLAIMED; no delivery request/evidence is recorded.\n");
    }
    body.push_str(
        "- Reminder: `merged` in the snapshot above = merged into LOCAL git only, NOT pushed or deployed.\n",
    );
}

fn append_global_job_snapshot(
    body: &mut String,
    agents: &[AgentRun],
    pending: Option<&WorkspaceInfo>,
) {
    let pending_count = usize::from(pending.is_some());
    let total = agents.len() + pending_count;
    let running = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Running)
        .count();
    let waiting = agents
        .iter()
        .filter(|agent| agent.needs_permission || agent.needs_user_input)
        .count();
    let done = agents
        .iter()
        .filter(|agent| agent_awaits_action_for_context(agent))
        .count();
    let merged = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Merged)
        .count();
    let failed = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Failed)
        .count();
    let stopped = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Stopped)
        .count();
    let orphaned = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Orphaned)
        .count();
    let migrated = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Migrated)
        .count();
    let merge_ready = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Done && agent_is_mergeable_worker(agent))
        .count();
    let claude = agents
        .iter()
        .filter(|agent| agent.backend == Backend::Claude)
        .count();
    let codex = agents
        .iter()
        .filter(|agent| agent.backend == Backend::Codex)
        .count();

    body.push_str("## Global job snapshot\n");
    body.push_str(&format!(
        "- totals: total={} running={} cloud-owned={} waiting={} done={} merged={} failed={} stopped={} orphaned={} pending-starts={}\n",
        total, running, migrated, waiting, done, merged, failed, stopped, orphaned, pending_count
    ));
    body.push_str(&format!(
        "- active-now: running={} cloud-owned={} waiting={} review-ready={} merge-ready={} pending-starts={}\n",
        running, migrated, waiting, done, merge_ready, pending_count
    ));
    body.push_str(&format!(
        "- completed: merged={} failed={} stopped={} orphaned={}\n",
        merged, failed, stopped, orphaned
    ));
    body.push_str(&format!("- backends: claude={} codex={}\n", claude, codex));
    body.push_str(&format!(
        "- ready-to-act: review-ready={} merge-ready={}\n",
        done, merge_ready
    ));
}

fn agent_waiting_label(agent: &AgentRun) -> &'static str {
    if agent.needs_permission {
        " waiting=permission"
    } else if agent.needs_user_input {
        " waiting=user-input"
    } else {
        ""
    }
}

fn agent_deps_label(agent: &AgentRun) -> String {
    let mut parts = Vec::new();
    if !agent.deps.is_empty() {
        parts.push(format!("hard=[{}]", agent.deps.join(",")));
    }
    if !agent.soft_deps.is_empty() {
        parts.push(format!("soft=[{}]", agent.soft_deps.join(",")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" deps={}", parts.join(" "))
    }
}

fn agent_is_mergeable_worker(agent: &AgentRun) -> bool {
    !agent.is_main() && !agent.is_oneoff() && !agent.is_orchestrator() && agent.has_merge_source()
}

pub(crate) fn sync_shared_context_surfaces(
    repo_root: &Path,
    agents: &[AgentRun],
    pending: Option<&WorkspaceInfo>,
) -> Result<()> {
    ensure_gitignore_contains(repo_root, RUDDER_SHARED_CONTEXT_FILE)?;
    let source = shared_context_path(repo_root);
    if !source.exists() {
        return Ok(());
    }
    let content = fs::read(&source)?;
    restrict_private_file(&source);

    let mut targets: Vec<PathBuf> = agents
        .iter()
        .map(|agent| agent.cwd.clone())
        .collect::<Vec<_>>();
    if let Some(workspace) = pending.filter(|workspace| workspace.path_is_workspace) {
        targets.push(workspace.path.clone());
    }
    targets.push(repo_root.to_path_buf());
    targets.sort();
    targets.dedup();

    for workspace in targets {
        if !workspace.exists() {
            continue;
        }
        exclude_shared_context_in_workspace(&workspace);
        let target = workspace.join(RUDDER_SHARED_CONTEXT_FILE);
        if target == source {
            continue;
        }
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&target, &content)?;
        restrict_private_file(&target);
    }
    Ok(())
}

pub(crate) fn append_shared_context(repo_root: &Path, source: &str, text: &str) -> Result<bool> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(false);
    }
    ensure_gitignore_contains(repo_root, RUDDER_SHARED_CONTEXT_FILE)?;
    let path = shared_context_path(repo_root);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(text) {
        return Ok(false);
    }
    let header = "# Rudder Shared Context\n\nLocal, gitignored context the user shared with Rudder. This may include API tokens, private URLs, environment values, account ids, or other details that must be available to every agent even after model compaction. Agents should read this file when present, use the values as needed, and avoid printing secret values back unless necessary.\n\n";
    let mut next = String::new();
    if existing.trim().is_empty() {
        next.push_str(header);
    } else {
        next.push_str(&existing);
        if !next.ends_with('\n') {
            next.push('\n');
        }
    }
    let source = source.trim();
    let source = if source.is_empty() {
        "task bar"
    } else {
        source
    };
    next.push_str(&format!("## {} · {source}\n", now_stamp()));
    for line in text.lines() {
        next.push_str("    ");
        next.push_str(line);
        next.push('\n');
    }
    next.push('\n');
    fs::write(&path, next)?;
    restrict_private_file(&path);
    Ok(true)
}

pub(crate) fn capture_shared_context_from_user_input(
    repo_root: &Path,
    input: &str,
) -> Result<bool> {
    let Some(snippet) = extract_shared_context_snippet(input) else {
        return Ok(false);
    };
    append_shared_context(repo_root, "task bar", &snippet)
}

pub(crate) fn extract_shared_context_snippet(input: &str) -> Option<String> {
    let lines = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && looks_like_shared_secret_line(line))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn looks_like_shared_secret_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let has_keyword = [
        "api key",
        "api token",
        "access token",
        "auth token",
        "bearer token",
        "bearer ",
        "client secret",
        "client_secret",
        "secret key",
        "authorization: bearer",
        "password",
        "_token",
        "token=",
        "token:",
        "api_key",
        "apikey",
        "secret=",
        "secret:",
        "_secret",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !has_keyword {
        return false;
    }
    let assignmentish = lower.contains('=')
        || lower.contains(": ")
        || lower.contains(" is ")
        || lower.starts_with("use ")
        || lower.contains(" use ");
    assignmentish && has_plausible_secret_value(line)
}

fn has_plausible_secret_value(line: &str) -> bool {
    line.split(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '`' | ',' | ';'))
        .map(|part| {
            part.trim_matches(|ch: char| {
                matches!(ch, ':' | '=' | '[' | ']' | '(' | ')' | '{' | '}')
            })
        })
        .any(|part| {
            part.len() >= 10
                && part.chars().any(|ch| ch.is_ascii_alphanumeric())
                && part
                    .chars()
                    .any(|ch| ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.' | '/'))
        })
}

fn exclude_shared_context_in_workspace(workspace: &Path) {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(workspace)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let exclude = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if exclude.is_empty() {
        return;
    }
    let path = workspace.join(exclude);
    let _ = ensure_line_in_file(&path, RUDDER_SHARED_CONTEXT_FILE);
}

fn ensure_line_in_file(path: &Path, line: &str) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing
        .lines()
        .any(|existing_line| existing_line.trim() == line)
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let prefix = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(path, format!("{existing}{prefix}{line}\n"))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_private_file(_path: &Path) {}

const RUDDER_GENERATED_START: &str = "<!-- RUDDER_GENERATED_START -->";
const RUDDER_GENERATED_END: &str = "<!-- RUDDER_GENERATED_END -->";
const RUDDER_PLAN_START: &str = "RUDDER_PLAN_TASKS_START";
const RUDDER_PLAN_END: &str = "RUDDER_PLAN_TASKS_END";

// Cross-process advisory lock around RUDDER.md read-modify-write, mirroring the
// TS protocol in src/rudder-md.ts (withRudderMdLock): an atomic mkdir of
// <repo>/.rudder/rudder-md.lock, 50ms retry up to 2s, locks older than 10s are
// treated as a crashed holder and taken over, and acquisition failure falls
// through to an unlocked write so a writer can never deadlock. The Rust TUI,
// TS CLI invocations, the daemon, and the orchestrator agent all write the
// file; this serializes the merge so interleavings cannot drop a freshly
// written DAG or control marker.
const RUDDER_MD_LOCK_RETRY: Duration = Duration::from_millis(50);
const RUDDER_MD_LOCK_WAIT: Duration = Duration::from_secs(2);
const RUDDER_MD_LOCK_STALE: Duration = Duration::from_secs(10);

pub(crate) struct RudderMdLock {
    dir: Option<PathBuf>,
}

impl Drop for RudderMdLock {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

pub(crate) fn acquire_rudder_md_lock(repo_root: &Path) -> RudderMdLock {
    let lock = repo_root.join(".rudder").join("rudder-md.lock");
    if let Some(parent) = lock.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let deadline = Instant::now() + RUDDER_MD_LOCK_WAIT;
    loop {
        if fs::create_dir(&lock).is_ok() {
            return RudderMdLock { dir: Some(lock) };
        }
        let stale = fs::metadata(&lock)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > RUDDER_MD_LOCK_STALE);
        if stale {
            let _ = fs::remove_dir_all(&lock);
            continue;
        }
        if Instant::now() >= deadline {
            return RudderMdLock { dir: None };
        }
        std::thread::sleep(RUDDER_MD_LOCK_RETRY);
    }
}

fn write_merged_rudder_md(path: &Path, generated: &str) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let merged = merge_generated_rudder_md(&existing, generated);
    // Atomic temp+rename so a concurrent reader (a worker re-reading shared
    // context mid-task) never sees a half-written file.
    let temp = path.with_extension(format!("md.{}.tmp", std::process::id()));
    fs::write(&temp, merged.as_bytes())?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// Remove every well-formed generated block, then any orphaned single markers a
/// stray literal (e.g. in orchestrator prose) or torn write left behind, so the
/// rebuilt file always carries exactly one marker pair. Mirrors
/// stripGeneratedMarkers in src/rudder-md.ts.
fn strip_generated_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(RUDDER_GENERATED_START) {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + RUDDER_GENERATED_START.len()..];
        if let Some(end) = after_start.find(RUDDER_GENERATED_END) {
            rest = &after_start[end + RUDDER_GENERATED_END.len()..];
        } else {
            rest = after_start;
        }
    }
    out.push_str(rest);
    out.replace(RUDDER_GENERATED_END, "")
}

pub(crate) fn merge_generated_rudder_md(existing: &str, generated: &str) -> String {
    let wrapped = format!(
        "{RUDDER_GENERATED_START}\n{}\n{RUDDER_GENERATED_END}\n",
        generated.trim_end()
    );
    let start_idx = existing.find(RUDDER_GENERATED_START);
    let end_idx = existing.find(RUDDER_GENERATED_END);
    if start_idx.is_some() || end_idx.is_some() {
        // The fresh block goes where the first marker sat so it keeps its
        // position relative to orchestrator content; everything before that
        // point cannot contain markers and is preserved verbatim. Duplicate
        // blocks and orphaned markers (prior corruption / stray literals) are
        // collapsed instead of swallowing the orchestrator content between them.
        let insert_at = match (start_idx, end_idx) {
            (Some(start), Some(end)) => start.min(end),
            (Some(start), None) => start,
            (None, Some(end)) => end,
            (None, None) => unreachable!(),
        };
        let prefix = existing[..insert_at].trim_end();
        let suffix_owned = strip_generated_markers(&existing[insert_at..]);
        // suffix.trim() (not trim_start) keeps repeated renders byte-identical:
        // trailing newlines would otherwise accumulate one per merge.
        let suffix = suffix_owned.trim();
        let mut parts: Vec<&str> = Vec::new();
        if !prefix.is_empty() {
            parts.push(prefix);
        }
        parts.push(wrapped.trim_end());
        if !suffix.is_empty() {
            parts.push(suffix);
        }
        return format!("{}\n", parts.join("\n\n"));
    }

    match latest_rudder_plan_block(existing) {
        Some(plan) => format!("{wrapped}\n## Orchestrator-authored plan\n\n{plan}\n"),
        None => wrapped,
    }
}

fn latest_rudder_plan_block(text: &str) -> Option<String> {
    let mut current: Option<Vec<String>> = None;
    let mut latest: Option<String> = None;
    for line in text.replace('\r', "").lines() {
        let trimmed = line.trim();
        if trimmed == RUDDER_PLAN_START {
            current = Some(vec![RUDDER_PLAN_START.to_string()]);
        } else if trimmed == RUDDER_PLAN_END {
            if let Some(mut block) = current.take() {
                block.push(RUDDER_PLAN_END.to_string());
                latest = Some(block.join("\n"));
            }
        } else if let Some(block) = current.as_mut() {
            block.push(line.to_string());
        }
    }
    latest
}

fn strip_rudder_plan_blocks_and_approval(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_plan = false;
    for line in text.replace('\r', "").lines() {
        let trimmed = line.trim();
        if trimmed == RUDDER_PLAN_START {
            in_plan = true;
            continue;
        }
        if trimmed == RUDDER_PLAN_END {
            in_plan = false;
            continue;
        }
        if in_plan || trimmed == "RUDDER_APPROVE_PLAN" {
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

pub(crate) fn replace_rudder_plan_block(repo_root: &Path, plan_block: &str) -> Result<()> {
    let path = repo_root.join("RUDDER.md");
    let _lock = acquire_rudder_md_lock(repo_root);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut preserved = strip_rudder_plan_blocks_and_approval(&existing)
        .trim_end()
        .to_string();
    if !preserved.is_empty() {
        preserved.push_str("\n\n");
    }
    if !preserved.contains("## Orchestrator-authored plan") {
        preserved.push_str("## Orchestrator-authored plan\n\n");
    }
    preserved.push_str(plan_block.trim());
    preserved.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("md.{}.tmp", std::process::id()));
    fs::write(&temp, preserved.as_bytes())?;
    fs::rename(&temp, path)?;
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

/// Append a cross-cutting conductor decision to the repo's DECISIONS.md (the shared,
/// agent-authored log workers re-read before each step). Mirrors the TS
/// `renderDecisionEntry` format: a `## title` heading, **What** / optional **Why**, and a
/// **By: conductor** footer. Best-effort; creates the file with a header when absent.
/// Keeps the conductor's plan/steer decisions DURABLE and visible to the fleet, not only
/// in the in-pane activity log.
/// Returns true if a new entry was written, false if it was skipped (empty `what`, or a
/// byte-identical title+what decision already present so this is a no-op duplicate).
pub(crate) fn append_conductor_decision(
    repo_root: &Path,
    title: &str,
    what: &str,
    why: Option<&str>,
) -> bool {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let what = collapse(what);
    if what.is_empty() {
        return false;
    }
    let title = {
        let t = collapse(title);
        if t.is_empty() {
            "decision".to_string()
        } else {
            t
        }
    };
    // The decision's identity is its title + what (the only varying part of a repeat is the
    // timestamp footer). Skip when that exact block already exists, so a conductor that
    // re-hits the same state every tick — e.g. a depth-capped auto-expansion that keeps
    // deferring the same follow-ups — does not spam DECISIONS.md with identical entries.
    let signature = format!("## {title}\n- **What:** {what}\n");
    let path = repo_root.join("DECISIONS.md");
    const HEADER: &str = "# Decisions\n\nShared, agent-authored log of cross-cutting decisions the fleet must honor. The conductor records plan/steer decisions here; workers record interface contracts + adjustments. Re-read before each significant step.\n\n";
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&signature) {
        return false;
    }
    let mut entry = signature;
    if let Some(why) = why.map(|w| collapse(w)).filter(|w| !w.is_empty()) {
        entry.push_str(&format!("- **Why:** {why}\n"));
    }
    entry.push_str(&format!("- **By:** conductor · {}\n\n", now_stamp()));

    let base = if existing.is_empty() {
        HEADER.to_string()
    } else {
        existing
    };
    let prefix = if base.ends_with('\n') { "" } else { "\n" };
    fs::write(&path, format!("{base}{prefix}{entry}")).is_ok()
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
