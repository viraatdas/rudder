use std::{
    collections::{HashMap, HashSet},
    env, fs,
    hash::{Hash, Hasher},
    io::{self, BufRead, Read, Stdout, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use rudder_native::pty_terminal::{
    StyledTerminalCell, TerminalCommand, TerminalCursor, TerminalPane, TerminalPaneOptions,
    TerminalSize,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

mod usage;
use crate::usage::*;
mod config;
use crate::config::*;
mod gitio;
use crate::gitio::*;
mod keys;
use crate::keys::*;
mod textedit;
use crate::textedit::*;
mod tasks;
use crate::tasks::*;
mod launch;
use crate::launch::*;
mod models;
use crate::models::*;
mod cloudio;
use crate::cloudio::*;
mod selection;
use crate::selection::*;
mod render;
use crate::render::*;
mod detect;
use crate::detect::*;
mod theme;
use crate::theme::*;
mod plan_stream;
use crate::plan_stream::*;

const TICK_RATE: Duration = Duration::from_millis(33);
/// Faster event-loop tick used only while a planner is actively streaming, so the
/// orchestrator's live transcript appears in finer increments (snappier streaming)
/// instead of being batched into 33ms bursts.
const STREAM_TICK_RATE: Duration = Duration::from_millis(11);
const MAX_EVENTS_PER_FRAME: usize = 64;
/// After this brief output lull, re-check the screen for the idle prompt even if the
/// agent is still emitting cursor-blink/animation repaints (which would otherwise
/// keep it looking busy forever).
const READY_EVAL_LULL: Duration = Duration::from_millis(900);
/// Once an agent has looked ready-for-input (idle chrome, not busy) continuously for
/// this long, declare the turn complete. Independent of output-silence, so an idle
/// TUI that keeps repainting is still detected as done.
///
/// This window must outlast the longest spinner GAP a still-working interactive agent
/// can show. Execution agents run as interactive TUIs (not `claude -p`), and their
/// footer chrome ("bypass permissions on (shift+tab to cycle)", the "> " prompt) is
/// drawn persistently — even mid-turn — so the ONLY thing distinguishing busy from
/// idle is the spinner's "esc to interrupt" line. Between steps (model done streaming
/// a chunk, about to call a tool, waiting on a slow tool) that spinner can briefly
/// vanish; a short grace would read that lull as "done" and, because post-completion
/// agent output does not reopen a finished agent, leave it stuck in review while still
/// working. Keep this comfortably longer than those gaps; the busy-spinner reopen
/// below is the backstop if a false completion still slips through.
const READY_GRACE: Duration = Duration::from_millis(3200);
// Dashboard colors now live in `theme.rs` (FOCUS_COLOR, INACTIVE_COLOR, ...),
// re-exported above so call sites are unchanged.
const DEFAULT_WHEEL_SCROLL_ROWS: u16 = 1;
const TASK_HISTORY_LIMIT: usize = 100;
const MOUSE_DEBUG_ENV: &str = "RUDDER_MOUSE_DEBUG";
const RUDDER_MOUSE_ENABLE_SEQUENCES: &[u8] = b"\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const RUDDER_MOUSE_DISABLE_SEQUENCES: &[u8] = b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const AGENT_LIST_RUN_START_ROW: u16 = 12;
const REVIEW_ALL_MODEL: &str = "gpt-5.5";
const REVIEW_ALL_EFFORT: EffortLevel = EffortLevel::XHigh;
const TASK_SUMMARY_MODEL: &str = "claude-haiku-4-5-20251001";
/// Default cap on how many plan-launched agents may run at once. Overridable via
/// `orchestrator.maxParallel` in ~/.rudder/config.json. This is what makes nodes
/// visibly wait in Todo and move to In Progress as slots free.
const DEFAULT_MAX_PARALLEL: usize = 1000;
/// Run the scheduler every N poll ticks (a coarse cadence; the tick rate is 33ms).
const SCHEDULER_TICK_INTERVAL: u64 = 8;
/// Max plan nodes to launch per scheduler pass. Each launch synchronously sets up
/// a jj workspace (a brief UI-thread block), so launches are spread across passes
/// to keep the TUI responsive when a merge unblocks a whole wave of children.
const MAX_LAUNCH_PER_TICK: usize = 2;
/// Max auto-expansion depth: a node created from a finishing agent's follow-ups
/// inherits parent generation + 1; beyond this we record the follow-ups but stop
/// growing the DAG, so a chain of agents proposing follow-ups can't run away.
const MAX_FOLLOWUP_DEPTH: u8 = 3;
/// Braille spinner frames for the orchestrator "planning" phase (and, as a nice
/// touch, running agents' status badge). Advances one frame per poll tick so it
/// animates while the planner decomposes the task.
pub(crate) const SPINNER_FRAMES: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];
const AGENT_PANE_HINTS: &[&str] = &[
    "j/k move",
    "Enter focus",
    "r rename",
    "v diff",
    "g nest",
    "R review all",
    "m merge",
    "M merge all",
    "x stop",
    "dd delete",
    "P model",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusPane {
    Agents,
    Worker,
    Task,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerView {
    Terminal,
    Diff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Claude,
    Codex,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" | "anthropic" => Some(Self::Claude),
            "codex" | "openai" => Some(Self::Codex),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentStatus {
    Running,
    Done,
    Merged,
    Failed,
    Stopped,
}

impl AgentStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Merged => "merged",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MergeStrategy {
    Merge,
    Rebase,
}

impl MergeStrategy {
    fn parse(value: &str) -> Self {
        match value {
            "rebase" => Self::Rebase,
            _ => Self::Merge,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentMode {
    Execute,
    Plan,
    RudderPlan,
    ReviewAll,
    Main,
}

const MAIN_AGENT_ID: &str = "__main__";

const MAIN_BOOTSTRAP_PROMPT: &str =
    "Read RUDDER.md if it exists, then briefly tell me what this project does and where its entry points live. After that, wait for instructions.";

impl AgentMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Plan => "plan",
            Self::RudderPlan => "rudder-plan",
            Self::ReviewAll => "review-all",
            Self::Main => "main",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "execute" | "run" | "task" => Some(Self::Execute),
            "plan" | "planning" => Some(Self::Plan),
            "rudder-plan" | "rudder_plan" | "orchestrate" => Some(Self::RudderPlan),
            "review-all" | "review_all" | "reviewall" => Some(Self::ReviewAll),
            "main" => Some(Self::Main),
            _ => None,
        }
    }
}

struct App {
    focus: FocusPane,
    nav_mode: bool,
    /// One-shot leader armed by Ctrl+W: the next keypress runs a dashboard
    /// command (focus a pane, review, merge, ...) instead of reaching the pane.
    leader_pending: bool,
    worker_view: WorkerView,
    /// When true, the agents pane renders a topological dependency tree (nest view)
    /// instead of the flat main/worktrees/merged sections. Toggled with `g`.
    nest_view: bool,
    cwd: PathBuf,
    branch: Option<String>,
    task_input: String,
    task_cursor: usize,
    task_history: Vec<String>,
    task_history_index: Option<usize>,
    task_history_draft: String,
    plan_mode: bool,
    agents: Vec<AgentRun>,
    /// Queue of planned (not-yet-launched) DAG nodes. The scheduler drains these
    /// into live agents as their hard deps merge and parallelism slots free.
    /// Rendered in the Todo section. A new completed plan replaces the queue.
    planned_nodes: Vec<PlannedNode>,
    /// The original user request that produced `planned_nodes`, used to build each
    /// worker launch prompt so the worker sees the coordinating request.
    planned_origin: String,
    /// The canonical first request the user typed for this plan, preserved verbatim
    /// across refine rounds so each refinement re-plans against the ORIGINAL ask
    /// (plus the running DAG + feedback) rather than the previous composite prompt.
    plan_request: String,
    /// The orchestrator's human-readable summary/assumptions/open-questions prose
    /// printed AFTER the RUDDER_PLAN_TASKS block. Shown under the DAG so the user
    /// knows what the planner assumed and what to discuss/refine.
    plan_summary: Option<String>,
    /// True while the orchestrator is RE-PLANNING in response to refinement feedback
    /// (between relaunch and the revised DAG being captured). It lets plan detection
    /// run even though `awaiting_approval` is still true (so the refined plan is
    /// captured), blocks premature approval of the stale plan, and is cleared the
    /// moment the revised plan lands (or the re-plan fails).
    refining: bool,
    /// When true, a plan node that finishes cleanly (review, no conflict) is merged
    /// automatically so its children unblock and the DAG drains hands-free. Toggled
    /// by `/automerge`. Default OFF: review-before-merge stays the norm.
    auto_merge: bool,
    /// Agent ids auto-merge has already hit a conflict on; skipped on later passes so
    /// it does not retry (and spam) every tick. The user resolves + merges manually.
    auto_merge_skip: Vec<String>,
    /// While true, a plan has been parsed into `planned_nodes` but is awaiting the
    /// user's APPROVAL gate: nothing launches. Enter approves (clears this and runs
    /// the scheduler); d removes the selected node, or discards the whole plan when
    /// the orchestrator is selected. Set on streaming plan detection; cleared once
    /// the user approves (or discards the plan).
    awaiting_approval: bool,
    /// Tick counter used to run the scheduler on a coarse cadence rather than on
    /// every PTY-byte tick.
    scheduler_tick: u64,
    /// Animation frame for the orchestrator spinner. Advances every poll tick so
    /// the "decomposing the task..." spinner feels alive while the planner runs.
    spinner_frame: usize,
    selected_agent: usize,
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
    notice: Option<String>,
    cloud_prompt: Option<CloudLaunchPrompt>,
    delete_pending: Option<String>,
    merge_confirm: Option<MergeConfirmation>,
    conflict_prompt: Option<MergeConflictPrompt>,
    /// Conflicted files reported by a jj `rudder merge`/`rudder sync` shell-out.
    /// jj records conflicts in the change rather than leaving git unmerged paths,
    /// so handle_merge_error reads them from here instead of `git diff -U`.
    pending_jj_conflict: Option<Vec<String>>,
    picker_index: usize,
    worker_selection: Option<WorkerSelection>,
    task_selection: Option<WorkerSelection>,
    /// Mouse text selection over the orchestrator pane. That pane composes Lines
    /// (transcript + DAG), not the planner PTY, so it cannot reuse the worker PTY
    /// selection. Coordinates are relative to the pane's inner area and index into
    /// `orch_visible_rows`, which `render_orchestrator` captures from the rendered
    /// buffer each frame (so the selection is over exactly what is on screen).
    orch_selection: Option<WorkerSelection>,
    orch_visible_rows: Vec<String>,
    /// Conductor activity log: an append-only, bounded record of every autonomous
    /// action the orchestrator takes (auto-expanded a node, steered an agent, etc.),
    /// shown in the orchestrator pane. The "visible" half of the no-confirm bargain.
    activity_log: Vec<String>,
    /// Run ids whose completion follow-ups have already been ingested, so a Done
    /// worker grows the DAG exactly once even as it is re-polled.
    followups_ingested: HashSet<String>,
    /// Auto-expansion depth per node id (0 = human/plan/reconcile origin; a node
    /// auto-created from a finishing node inherits parent+1). Caps runaway chains.
    followup_gen: HashMap<String, u8>,
    /// Node-id pairs the drift scan has already acted on, so a given collision is
    /// nudged once (not every scan). Key is the pair sorted lexically.
    surfaced_overlaps: HashSet<(String, String)>,
    /// Throttle for the (shell-out) cross-agent drift scan; runs at most ~every 5s.
    last_drift_scan: Option<Instant>,
    /// Scroll offset (lines from the top) for the orchestrator DAG command-center
    /// view. The DAG is a static line list (not a PTY), so it needs its own offset
    /// to read a long plan. Reset to 0 whenever the selected agent changes; clamped
    /// against the rendered line count in `render_orchestrator`.
    orch_dag_scroll: usize,
    agents_area: Option<Rect>,
    worker_area: Option<Rect>,
    task_area: Option<Rect>,
    cloud_connected: bool,
    cloud_runtime: Option<String>,
    last_cloud_check: Instant,
    cloud_workspace: Option<CloudWorkspaceStatus>,
    last_workspace_check: Option<Instant>,
    workspace_status_rx: Option<mpsc::Receiver<Option<CloudWorkspaceStatus>>>,
    workspace_idle_notified: bool,
    task_summary_tx: mpsc::Sender<TaskSummaryResult>,
    task_summary_rx: mpsc::Receiver<TaskSummaryResult>,
    last_user_activity: Instant,
    mouse_debug: bool,
    mouse_debug_last: Option<String>,
    pending_migration_resumes: Vec<MigratedAgent>,
    migration_resumes_attempted: bool,
    rename_input: Option<String>,
    rename_cursor: usize,
    diff_summary_cache: HashMap<String, (Instant, Option<String>)>,
    dirty: bool,
    last_tab_emoji: Option<char>,
    /// True when the user pressed Ctrl+C once but there are running agents we
    /// want them to confirm pausing before we actually quit. Cleared by any
    /// other key.
    quit_confirm_pending: bool,
    /// ISO-8601 timestamp captured at dashboard startup. Used to scope
    /// `/usage` to this rudder session rather than the user's full lifetime
    /// claude/codex history for the repo.
    session_started_iso: String,
    /// Coalesce guard for graph.json mirroring: a hash of the last mirrored
    /// plan/agent signature. `mirror_graph` is a no-op when the signature has
    /// not changed, so a burst of poll ticks coalesces into one shell-out. None
    /// until the first mirror.
    last_mirror_signature: Option<u64>,
}

#[derive(Clone, Debug)]
struct MigratedAgent {
    run_id: String,
    session_id: String,
    worktree_path: PathBuf,
    fresh_prompt: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SelectionPoint {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerSelection {
    start: SelectionPoint,
    end: SelectionPoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NormalizedSelection {
    start: SelectionPoint,
    end: SelectionPoint,
}

struct MergeConfirmation {
    intent: MergeIntent,
}

struct CloudLaunchPrompt {
    scratch_args: Vec<String>,
    scratch_label: String,
    selected_task: Option<String>,
    choice: CloudLaunchChoice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloudLaunchChoice {
    Upload,
    Scratch,
}

struct CloudSummary {
    connected: bool,
    runtime: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CloudWorkspaceStatus {
    id: Option<String>,
    status: Option<String>,
    active_agents: bool,
    client_count: u32,
    idle_minutes: Option<u32>,
}

enum MergeIntent {
    Selected { id: String, task: String },
    All { ids: Vec<String> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewAllSource {
    id: String,
    branch: String,
    task: String,
    summary: String,
    worktree_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReviewAllPremerge {
    merged_branches: Vec<String>,
    stopped_branch: Option<String>,
    stopped_error: Option<String>,
    remaining_branches: Vec<String>,
}

struct MergeConflictPrompt {
    operation: ConflictOperation,
    task: String,
    conflicted_files: Vec<String>,
    error: String,
    repo_root: PathBuf,
    target_branch: Option<String>,
    source_branch: Option<String>,
    worktree_path: Option<PathBuf>,
    /// The id of the agent whose merge stopped. We reuse its row for the AI
    /// conflict resolver so we never grow a fresh dashboard pane mid-merge.
    agent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictOperation {
    Merge,
    Rebase,
}

struct AgentRun {
    id: String,
    created_at: String,
    mode: AgentMode,
    task: String,
    task_summary: String,
    current_prompt: String,
    turns: Vec<AgentTurn>,
    last_user_input_at: String,
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
    status: AgentStatus,
    cwd: PathBuf,
    worktree_branch: Option<String>,
    worktree_path: Option<PathBuf>,
    /// jj workspace name for runs isolated in a jj workspace. `None` for the
    /// main agent and legacy git-worktree runs.
    workspace_name: Option<String>,
    /// jj change id captured when the run's workspace was created. Used to route
    /// merge/sync through jj for new (vcs:jj) runs.
    jj_change_id: Option<String>,
    session_id: Option<String>,
    terminal: Option<TerminalPane>,
    terminal_size: Option<TerminalSize>,
    review_terminal: Option<TerminalPane>,
    review_size: Option<TerminalSize>,
    review_error: Option<String>,
    last_output_at: Instant,
    completed_at: Option<Instant>,
    autosteered: bool,
    needs_permission: bool,
    permission_notified: bool,
    needs_user_input: bool,
    user_input_notified: bool,
    last_error: Option<String>,
    worker_input_draft: String,
    worker_input_cursor: usize,
    worker_input_is_prompt: bool,
    last_drain_at: Option<Instant>,
    review_source_ids: Vec<String>,
    /// Hard-dependency parent node ids (from `.rudder/graph.json`). Empty in flat
    /// mode (no graph.json or node not found).
    deps: Vec<String>,
    /// Soft-dependency parent node ids. Never block; rendered dimmed in nest view.
    soft_deps: Vec<String>,
    /// Plan node id when this run was launched from a planned node by the
    /// scheduler. `None` for manually started agents and planner runs. When the
    /// run reaches Merged, this id enters the merged set and unblocks dependents.
    node_id: Option<String>,
    /// True when this is a RECONCILE planner: a RudderPlan agent spawned to fold
    /// one ADDED task into an already-active plan. The poll loop routes its
    /// completion to the APPEND path (`evaluate_completed_reconcile`) instead of
    /// the REPLACE path (`evaluate_completed_plan`), so the existing plan's
    /// not-yet-launched nodes are preserved. Ordinary planners have this `false`.
    reconcile_planner: bool,
    /// Live planner stream parser for a RudderPlan run: turns the backend's JSON
    /// event stream into a conversation transcript + the reconstructed assistant
    /// text the RUDDER_PLAN_TASKS parser reads, and captures the session id for
    /// refine-via-resume. `None` for non-planner agents and until first ingest.
    plan_stream: Option<PlanStreamState>,
    /// When the user last sent input (a keystroke/prompt) to this agent's PTY. Used
    /// to decide whether post-completion output is a genuine NEW turn (user typed)
    /// vs incidental repaint (e.g. a resize when the pane is focused). Without this,
    /// highlighting a finished agent flips it from done back to in-progress.
    last_worker_input_at: Option<Instant>,
    /// When the agent FIRST looked ready-for-input (idle chrome present, not busy) in
    /// the current turn. Completion is declared once it stays ready for a short grace
    /// window, independent of output-silence — so a TUI that repaints while idle
    /// (cursor blink/animation) is still detected as done. Reset when it looks busy
    /// again or starts a new turn.
    ready_since: Option<Instant>,
    /// True when this row is currently an AI merge-conflict resolver (its jj merge
    /// recorded a conflict and an agent is resolving it in place). When it finishes
    /// with no conflicts left, the merge is finalized (the node flips to Merged so
    /// its children unblock); if conflicts remain it drops to manual.
    merge_resolver: bool,
}

#[derive(Debug)]
struct TaskSummaryResult {
    run_id: String,
    title: Option<String>,
}

impl AgentRun {
    fn is_main(&self) -> bool {
        self.mode == AgentMode::Main || self.id == MAIN_AGENT_ID
    }

    /// A RudderPlan agent is the pinned orchestrator that owns the active plan. It
    /// renders as a distinct row at the very top of the list (not in a status
    /// bucket) and its worker pane shows the orchestrator DAG view.
    pub(crate) fn is_orchestrator(&self) -> bool {
        self.mode == AgentMode::RudderPlan
    }
}

#[derive(Clone, Debug)]
struct AgentTurn {
    ts: String,
    prompt: String,
    source: String,
}

#[derive(Clone)]
struct Suggestion {
    label: String,
    detail: String,
    action: SuggestionAction,
}

#[derive(Clone)]
enum SuggestionAction {
    Insert(String),
    RunCommand(String),
    ChooseModelProvider(Backend),
    ChooseModel {
        backend: Backend,
        model: String,
    },
    SetModel {
        backend: Backend,
        model: String,
        effort: Option<EffortLevel>,
    },
    ShowHelp,
}

struct InitialSelection {
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
}

#[derive(Default)]
struct CliSelection {
    backend: Option<Backend>,
    model: Option<String>,
}

impl App {
    fn new() -> Self {
        let cwd = std::env::current_dir()
            .map(|path| dashboard_root(&path))
            .unwrap_or_else(|_| PathBuf::from("."));
        let selection = initial_selection();
        let agents = if cfg!(test) {
            Vec::new()
        } else {
            load_persisted_agents(&cwd)
        };
        // Main is no longer auto-pinned. If the user wants one, they type
        // /main from the task pane. Main records render in their own section
        // instead of being mixed into ordinary worktree agents.
        let (task_input, task_cursor) = (String::new(), 0);
        let pending_migration_resumes = if cfg!(test) {
            Vec::new()
        } else {
            read_migration_manifest(&cwd)
        };
        let cloud = read_cloud_summary();
        let session_started_iso = load_or_init_session_started(&cwd);
        let (task_summary_tx, task_summary_rx) = mpsc::channel();
        let branch = current_branch_at(&cwd);
        Self {
            focus: FocusPane::Task,
            nav_mode: false,
            leader_pending: false,
            worker_view: WorkerView::Terminal,
            nest_view: false,
            cwd,
            branch,
            task_input,
            task_cursor,
            task_history: Vec::new(),
            task_history_index: None,
            task_history_draft: String::new(),
            plan_mode: false,
            agents,
            planned_nodes: Vec::new(),
            planned_origin: String::new(),
            plan_request: String::new(),
            plan_summary: None,
            refining: false,
            auto_merge: false,
            auto_merge_skip: Vec::new(),
            awaiting_approval: false,
            scheduler_tick: 0,
            spinner_frame: 0,
            selected_agent: 0,
            backend: selection.backend,
            model: selection.model,
            effort: selection.effort,
            notice: None,
            cloud_prompt: None,
            delete_pending: None,
            merge_confirm: None,
            conflict_prompt: None,
            pending_jj_conflict: None,
            picker_index: 0,
            worker_selection: None,
            task_selection: None,
            orch_selection: None,
            orch_visible_rows: Vec::new(),
            activity_log: Vec::new(),
            followups_ingested: HashSet::new(),
            followup_gen: HashMap::new(),
            surfaced_overlaps: HashSet::new(),
            last_drift_scan: None,
            orch_dag_scroll: 0,
            agents_area: None,
            worker_area: None,
            task_area: None,
            cloud_connected: cloud.connected,
            cloud_runtime: cloud.runtime,
            last_cloud_check: Instant::now(),
            cloud_workspace: None,
            last_workspace_check: None,
            workspace_status_rx: None,
            workspace_idle_notified: false,
            task_summary_tx,
            task_summary_rx,
            last_user_activity: Instant::now(),
            mouse_debug: env::var(MOUSE_DEBUG_ENV).is_ok_and(|value| value != "0"),
            mouse_debug_last: None,
            pending_migration_resumes,
            migration_resumes_attempted: false,
            rename_input: None,
            rename_cursor: 0,
            diff_summary_cache: HashMap::new(),
            dirty: true,
            last_tab_emoji: None,
            session_started_iso,
            quit_confirm_pending: false,
            last_mirror_signature: None,
        }
    }

    fn tab_status_emoji(&self) -> char {
        if self
            .agents
            .iter()
            .any(|a| a.needs_permission || a.needs_user_input)
        {
            return '\u{1f7e1}'; // yellow circle - your attention needed
        }
        if self.agents.iter().any(|a| a.status == AgentStatus::Failed) {
            return '\u{1f534}'; // red circle - failure
        }
        if self.agents.iter().any(|a| a.status == AgentStatus::Running) {
            return '\u{1f7e2}'; // green circle - actively running
        }
        '\u{26aa}' // white circle - idle / no work
    }

    /// Update the host terminal tab title to reflect current state. Cheap;
    /// only emits an OSC when the leading status emoji actually changed.
    fn refresh_tab_title(&mut self) {
        let emoji = self.tab_status_emoji();
        if self.last_tab_emoji == Some(emoji) {
            return;
        }
        let repo = self
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.cwd.display().to_string());
        let prefix = if is_cloud_worker_session() {
            "Rudder cloud"
        } else {
            "Rudder"
        };
        let title = format!("{emoji} {prefix}: {repo}");
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b]0;{title}\x07");
        let _ = stdout.flush();
        self.last_tab_emoji = Some(emoji);
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn take_dirty(&mut self) -> bool {
        let was = self.dirty;
        self.dirty = false;
        was
    }

    /// Current spinner glyph for the active animation frame.
    pub(crate) fn spinner_glyph(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    /// True while an orchestrator (a RudderPlan agent) is actively working: its
    /// planner process is Running, or a refine is in flight. While this holds the
    /// pane shows the "decomposing…/refining…" spinner, so the render loop must
    /// redraw on every tick to animate it.
    ///
    /// This deliberately keys off the planner being ALIVE, NOT off whether a plan
    /// block has parsed yet. Tying it to `extract_rudder_plan_tasks(...).is_err()`
    /// silently diverged from `orchestrator_phase` (which is what actually decides
    /// to draw the spinner): a present-but-empty task block parses to `Ok(empty)`,
    /// so the pane shows Planning while `.is_err()` was false — no redraw fired and
    /// the spinner froze, only advancing a frame per input event. Running-or-refining
    /// can never diverge that way.
    pub(crate) fn has_planning_orchestrator(&self) -> bool {
        self.refining
            || self
                .agents
                .iter()
                .any(|run| run.mode == AgentMode::RudderPlan && run.status == AgentStatus::Running)
    }

    fn cached_diff_summary(&mut self, id: &str, cwd: &Path) -> Option<String> {
        const TTL: Duration = Duration::from_millis(1500);
        let now = Instant::now();
        if let Some((stamp, value)) = self.diff_summary_cache.get(id) {
            if now.duration_since(*stamp) < TTL {
                return value.clone();
            }
        }
        let value = diff_short_summary_at(cwd);
        self.diff_summary_cache
            .insert(id.to_string(), (now, value.clone()));
        value
    }

    fn selected_is_main(&self) -> bool {
        self.agents
            .get(self.selected_agent)
            .map(|run| run.is_main())
            .unwrap_or(false)
    }

    /// True when the currently-selected agent is the pinned orchestrator
    /// (a RudderPlan run). Used by the approval gate: Enter approves and d discards
    /// the pending plan when the orchestrator row is selected.
    fn selected_is_orchestrator(&self) -> bool {
        self.agents
            .get(self.selected_agent)
            .map(|run| run.is_orchestrator())
            .unwrap_or(false)
    }

    /// True when the worker pane is currently showing the custom orchestrator DAG
    /// command-center view (selected orchestrator, Terminal view, plan parsed)
    /// rather than the raw PTY. In that view scroll/selection target the rendered
    /// DAG lines, not the underlying planner terminal. Mirrors the dispatch in
    /// `render_worker`.
    fn selected_orchestrator_dag_active(&self) -> bool {
        if self.worker_view != WorkerView::Terminal {
            return false;
        }
        self.agents.get(self.selected_agent).is_some_and(|run| {
            run.is_orchestrator()
                && matches!(orchestrator_phase(run), OrchestratorPhase::PlanReady(_))
        })
    }

    fn selected_main_index(&self) -> Option<usize> {
        if self.selected_is_main() {
            Some(self.selected_agent)
        } else {
            self.agents.iter().position(|run| run.is_main())
        }
    }

    fn set_model_defaults(
        &mut self,
        backend: Backend,
        model: String,
        effort: Option<EffortLevel>,
    ) -> Option<String> {
        self.backend = backend;
        self.model = model;
        self.effort = effort;
        save_model_defaults(self.backend, &self.model, self.effort)
            .err()
            .map(|error| format!("config warning: {error}"))
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.note_user_activity();
        if self.rename_input.is_some() {
            self.handle_rename_key(key);
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Guard against accidental quit when agents are still working.
            // First Ctrl+C asks for confirmation; second Ctrl+C (or y) quits.
            let running = self
                .agents
                .iter()
                .filter(|a| {
                    !a.is_main() && a.terminal.is_some() && a.status == AgentStatus::Running
                })
                .count();
            if running == 0 || self.quit_confirm_pending {
                return true;
            }
            self.quit_confirm_pending = true;
            self.notice = Some(format!(
                "{running} agent{} still running. Ctrl+C again (or y) to quit; any other key to keep going. Claude agents auto-resume on next rudder.",
                if running == 1 { "" } else { "s" }
            ));
            return false;
        }
        // Any other key dismisses the pending quit confirmation.
        if self.quit_confirm_pending {
            if key.code == KeyCode::Char('y') || key.code == KeyCode::Char('Y') {
                return true;
            }
            self.quit_confirm_pending = false;
            self.notice = Some("quit cancelled".to_string());
            // fall through and let the key be handled normally
        }

        // One-shot leader: Ctrl+W arms it (below) and the next keypress is
        // routed here as a dashboard command instead of going to the pane.
        if self.leader_pending {
            return self.handle_leader_key(key);
        }

        if self.handle_cloud_prompt_key(key) {
            return false;
        }

        if is_copy_key(key) {
            self.copy_focused_selection();
            return false;
        }

        if self.handle_merge_prompt_key(key) {
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            self.nav_mode = !self.nav_mode;
            self.notice = Some(if self.nav_mode {
                "nav mode: 1 agents  2 worker  3 task  v review  R review-all  M merge-all  Esc exits"
                    .to_string()
            } else {
                "worker input restored".to_string()
            });
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
            self.leader_pending = true;
            self.nav_mode = false;
            self.notice = Some(
                "Ctrl+W: Tab cycle  1/2/3 panes  v review  m merge  R review-all  M merge-all  Esc cancels"
                    .to_string(),
            );
            return false;
        }

        if self.nav_mode {
            return self.handle_nav_key(key);
        }

        // Option/Alt + 1/2/3 (and v) jumps directly between panes. Many macOS
        // terminals (Terminal.app and default iTerm2) do not report an Alt
        // modifier for Option+key and instead emit the typographic character it
        // produces on a US layout, so accept those bare characters as well:
        // Option+1=¡, Option+2=™, Option+3=£, Option+v=√.
        let alt_like = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META);
        match key.code {
            KeyCode::Char('1') if alt_like => {
                self.focus = FocusPane::Agents;
                return false;
            }
            KeyCode::Char('2') if alt_like => {
                self.focus = FocusPane::Worker;
                return false;
            }
            KeyCode::Char('3') if alt_like => {
                self.focus = FocusPane::Task;
                return false;
            }
            KeyCode::Char('v') if alt_like => {
                self.toggle_worker_view();
                return false;
            }
            KeyCode::Char('\u{00a1}') => {
                self.focus = FocusPane::Agents;
                return false;
            }
            KeyCode::Char('\u{2122}') => {
                self.focus = FocusPane::Worker;
                return false;
            }
            KeyCode::Char('\u{00a3}') => {
                self.focus = FocusPane::Task;
                return false;
            }
            KeyCode::Char('\u{221a}') => {
                self.toggle_worker_view();
                return false;
            }
            _ => {}
        }

        match self.focus {
            FocusPane::Agents => self.handle_agents_key(key),
            FocusPane::Worker => self.handle_worker_key(key),
            FocusPane::Task => self.handle_task_key(key),
        }
    }

    fn handle_nav_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.nav_mode = false;
                self.delete_pending = None;
                self.notice = Some("worker input restored".to_string());
            }
            KeyCode::Char('1') => {
                self.delete_pending = None;
                self.focus = FocusPane::Agents;
            }
            KeyCode::Char('2') => {
                self.delete_pending = None;
                self.focus = FocusPane::Worker;
            }
            KeyCode::Char('3') => {
                self.delete_pending = None;
                self.focus = FocusPane::Task;
            }
            KeyCode::Char('v') => self.toggle_worker_view(),
            KeyCode::Char('r') => self.start_rename_selected_agent(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_agent(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_agent(),
            KeyCode::Char('m') => self.request_merge_selected_agent(),
            KeyCode::Char('R') => self.review_all_ready(),
            KeyCode::Char('M') => self.request_merge_all_ready(),
            KeyCode::Char('d') => self.delete_selected_agent(),
            KeyCode::Char('q') => return true,
            _ => {}
        }
        false
    }

    /// Runs a single dashboard command after the Ctrl+W leader is armed, then
    /// disarms. Mirrors nav mode but is one-shot rather than sticky.
    fn handle_leader_key(&mut self, key: KeyEvent) -> bool {
        self.leader_pending = false;
        match key.code {
            KeyCode::Esc => {
                self.delete_pending = None;
                self.notice = Some("leader cancelled".to_string());
            }
            // Tab cycles focus forward and RE-ARMS the leader so repeated Tab keeps
            // cycling (BackTab cycles backward, also re-arming). This only happens
            // under the Ctrl+W leader; a bare Tab still forwards to the worker.
            KeyCode::Tab => {
                self.delete_pending = None;
                self.cycle_focus(true);
                self.leader_pending = true;
            }
            KeyCode::BackTab => {
                self.delete_pending = None;
                self.cycle_focus(false);
                self.leader_pending = true;
            }
            KeyCode::Char('1') | KeyCode::Char('\u{00a1}') => {
                self.delete_pending = None;
                self.focus = FocusPane::Agents;
            }
            KeyCode::Char('2') | KeyCode::Char('\u{2122}') => {
                self.delete_pending = None;
                self.focus = FocusPane::Worker;
            }
            KeyCode::Char('3') | KeyCode::Char('\u{00a3}') => {
                self.delete_pending = None;
                self.focus = FocusPane::Task;
            }
            KeyCode::Char('v') | KeyCode::Char('\u{221a}') => self.toggle_worker_view(),
            KeyCode::Char('r') => self.start_rename_selected_agent(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_agent(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_agent(),
            KeyCode::Char('m') => self.request_merge_selected_agent(),
            KeyCode::Char('R') => self.review_all_ready(),
            KeyCode::Char('M') => self.request_merge_all_ready(),
            KeyCode::Char('d') => self.delete_selected_agent(),
            KeyCode::Char('q') => return true,
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Double Ctrl+W keeps the leader armed rather than cancelling.
                self.leader_pending = true;
            }
            _ => {
                self.notice = Some("leader: unknown key (Esc to cancel)".to_string());
            }
        }
        false
    }

    fn handle_agents_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_agent(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_agent(),
            KeyCode::Enter => {
                // APPROVAL GATE: while a plan awaits approval and the pinned
                // orchestrator is selected, Enter approves the plan and launches the
                // ready nodes rather than focusing the worker pane.
                if self.awaiting_approval && self.selected_is_orchestrator() {
                    self.delete_pending = None;
                    self.approve_planned_queue();
                } else if !self.agents.is_empty() {
                    self.delete_pending = None;
                    if self.selected_is_main() {
                        self.focus_or_spawn_main();
                    } else {
                        self.focus = FocusPane::Worker;
                    }
                }
            }
            KeyCode::Char('v') => self.toggle_worker_view(),
            KeyCode::Char('g') => self.toggle_nest_view(),
            KeyCode::Char('r') => self.start_rename_selected_agent(),
            KeyCode::Char('R') => self.review_all_ready(),
            KeyCode::Char('M') => self.request_merge_all_ready(),
            KeyCode::Char('m') => self.request_merge_selected_agent(),
            KeyCode::Char('d') => {
                // The pinned orchestrator is never deleted/discarded with `d`; the
                // pending plan is refined by typing into the task pane, not thrown
                // away. For any other agent, `d` deletes it as usual.
                if !self.selected_is_orchestrator() {
                    self.delete_selected_agent();
                }
            }
            KeyCode::Char('x') => {
                // Stop the selected worker: drops its PTY, marks it Stopped, frees a
                // parallelism slot, and keeps its jj workspace (undoable). Not the
                // main agent or the orchestrator.
                if !self.selected_is_main() && !self.selected_is_orchestrator() {
                    let idx = self.selected_agent;
                    self.stop_agent_at(idx);
                }
            }
            KeyCode::Char('P') => self.open_main_model_switcher(),
            _ => {}
        }
        false
    }

    fn toggle_nest_view(&mut self) {
        self.nest_view = !self.nest_view;
        self.dirty = true;
    }

    /// Rotate pane focus: Agents -> Worker -> Task -> Agents (forward) or the
    /// reverse. Used by the Ctrl+W leader Tab/BackTab cycle.
    fn cycle_focus(&mut self, forward: bool) {
        self.focus = if forward {
            match self.focus {
                FocusPane::Agents => FocusPane::Worker,
                FocusPane::Worker => FocusPane::Task,
                FocusPane::Task => FocusPane::Agents,
            }
        } else {
            match self.focus {
                FocusPane::Agents => FocusPane::Task,
                FocusPane::Worker => FocusPane::Agents,
                FocusPane::Task => FocusPane::Worker,
            }
        };
    }

    fn handle_worker_key(&mut self, key: KeyEvent) -> bool {
        if self.worker_view == WorkerView::Diff {
            match key.code {
                KeyCode::Esc | KeyCode::Char('v') => {
                    self.worker_view = WorkerView::Terminal;
                    self.notice = None;
                    return false;
                }
                KeyCode::Char('m') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.request_merge_selected_agent();
                    return false;
                }
                _ => {}
            }

            if self.selected_review_terminal_mut().is_none() {
                return false;
            }

            if let Some(bytes) = terminal_bytes_for_key(key) {
                if let Some(review) = self.selected_review_terminal_mut() {
                    if let Err(error) = review.write_input(&bytes) {
                        self.set_selected_review_error(error.to_string());
                    }
                }
            }
            return false;
        }

        // Orchestrator pane = a CHAT with the planner. Typing composes a follow-up;
        // Enter with text refines the plan (resumes the planner conversation), Enter
        // on an empty line approves & launches. We intercept before the PTY-forward
        // path because the planner runs non-interactively (writing to it is useless).
        if self.selected_is_orchestrator() {
            return self.handle_orchestrator_chat_key(key);
        }

        self.worker_selection = None;
        if self.selected_terminal_mut().is_none() {
            match key.code {
                KeyCode::Char('q') => return true,
                _ => {}
            }
        }

        match key.code {
            KeyCode::PageUp => {
                self.handle_worker_page_key(key, page_scroll_rows(self.worker_area));
                return false;
            }
            KeyCode::PageDown => {
                self.handle_worker_page_key(key, -page_scroll_rows(self.worker_area));
                return false;
            }
            _ => {}
        }

        if self.selected_worker_is_finished_cloud_command() {
            match key.code {
                KeyCode::Char('r') => self.restart_selected_agent(),
                KeyCode::Char('q') => return true,
                _ => {
                    self.notice = Some(
                        "cloud command finished; run /cloud again or press r to rerun".to_string(),
                    );
                }
            }
            return false;
        }

        let Some(bytes) = terminal_bytes_for_key(key) else {
            return false;
        };

        let capture_as_prompt = self.selected_worker_accepts_prompt_input();
        let result = match self.selected_terminal_mut() {
            Some(terminal) => {
                terminal.reset_scrollback();
                terminal.write_input(&bytes)
            }
            None => return false,
        };
        if let Err(error) = result {
            self.set_selected_error(error.to_string());
            return false;
        }
        // Mark genuine user input so post-completion output is recognized as a new
        // turn (and a finished agent re-enters in-progress) — repaints do not.
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            run.last_worker_input_at = Some(Instant::now());
        }
        self.clear_selected_attention_flags();
        if let Some(prompt) = self.capture_selected_worker_key(key, capture_as_prompt) {
            self.record_selected_worker_prompt(prompt);
        }
        false
    }

    /// Key handling for the orchestrator chat (worker pane on a RudderPlan run). The
    /// draft lives on `worker_input_draft`. Enter with text refines (a follow-up turn
    /// to the planner); Enter on an empty line approves & launches; Esc clears the
    /// draft; PageUp/Down scroll the transcript; other printable keys type.
    fn handle_orchestrator_chat_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                let draft = self
                    .agents
                    .get(self.selected_agent)
                    .map(|run| run.worker_input_draft.trim().to_string())
                    .unwrap_or_default();
                if draft.is_empty() {
                    if self.refining {
                        self.notice =
                            Some("still refining — the updated plan is on its way".to_string());
                    } else if self.awaiting_approval {
                        self.approve_planned_queue();
                    } else {
                        self.notice =
                            Some("type a message to refine the plan, or wait for it to finish".to_string());
                    }
                } else {
                    if let Some(run) = self.agents.get_mut(self.selected_agent) {
                        run.worker_input_draft.clear();
                        run.worker_input_cursor = 0;
                    }
                    self.remember_task_history(&draft);
                    self.refine_plan(&draft);
                }
            }
            // Editing in the chat draft, at full parity with the task pane so a
            // follow-up can be edited mid-line: Option/Alt+Backspace deletes the
            // previous word, Alt/Meta+arrows + Alt+b/f navigate by word, plus the
            // emacs basics (Ctrl+A/E/K/U/W/D/H).
            KeyCode::Backspace => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    if key.modifiers.intersects(
                        KeyModifiers::ALT
                            | KeyModifiers::CONTROL
                            | KeyModifiers::SUPER
                            | KeyModifiers::META,
                    ) {
                        delete_previous_word_at(
                            &mut run.worker_input_draft,
                            &mut run.worker_input_cursor,
                        );
                    } else {
                        delete_char_before_cursor(
                            &mut run.worker_input_draft,
                            &mut run.worker_input_cursor,
                        );
                    }
                }
            }
            KeyCode::Delete => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    delete_char_at_cursor(&mut run.worker_input_draft, run.worker_input_cursor);
                }
            }
            KeyCode::Esc => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_draft.clear();
                    run.worker_input_cursor = 0;
                }
            }
            KeyCode::Left => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::META)
                    {
                        run.worker_input_cursor = previous_word_position(
                            &run.worker_input_draft,
                            run.worker_input_cursor,
                        );
                    } else {
                        run.worker_input_cursor = run.worker_input_cursor.saturating_sub(1);
                    }
                }
            }
            KeyCode::Right => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::META)
                    {
                        run.worker_input_cursor =
                            next_word_position(&run.worker_input_draft, run.worker_input_cursor);
                    } else {
                        let len = run.worker_input_draft.chars().count();
                        run.worker_input_cursor = (run.worker_input_cursor + 1).min(len);
                    }
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B')
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META) =>
            {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor =
                        previous_word_position(&run.worker_input_draft, run.worker_input_cursor);
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META) =>
            {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor =
                        next_word_position(&run.worker_input_draft, run.worker_input_cursor);
                }
            }
            KeyCode::Home => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor = run.worker_input_draft.chars().count();
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor = 0;
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_cursor = run.worker_input_draft.chars().count();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    run.worker_input_draft.clear();
                    run.worker_input_cursor = 0;
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    delete_previous_word_at(
                        &mut run.worker_input_draft,
                        &mut run.worker_input_cursor,
                    );
                }
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    truncate_at_cursor(&mut run.worker_input_draft, run.worker_input_cursor);
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    delete_char_at_cursor(&mut run.worker_input_draft, run.worker_input_cursor);
                }
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    delete_char_before_cursor(
                        &mut run.worker_input_draft,
                        &mut run.worker_input_cursor,
                    );
                }
            }
            // Scroll the orchestrator's PROSE plan with the keyboard. Previously these
            // went to the PTY (which is JSON now), so a long plan could not be read
            // without a mouse. orch_dag_scroll drives the scrollable body; render
            // clamps it to the content height.
            KeyCode::PageUp => {
                let page = page_scroll_rows(self.worker_area).max(1) as usize;
                self.orch_dag_scroll = self.orch_dag_scroll.saturating_sub(page);
            }
            KeyCode::PageDown => {
                let page = page_scroll_rows(self.worker_area).max(1) as usize;
                self.orch_dag_scroll = self.orch_dag_scroll.saturating_add(page);
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(run) = self.agents.get_mut(self.selected_agent) {
                    insert_char_at_cursor(
                        &mut run.worker_input_draft,
                        &mut run.worker_input_cursor,
                        ch,
                    );
                }
            }
            _ => {}
        }
        self.dirty = true;
        false
    }

    fn clear_selected_attention_flags(&mut self) {
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            if run.needs_permission || run.needs_user_input {
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                self.dirty = true;
            }
        }
    }

    fn copy_focused_selection(&mut self) {
        let text = match self.focus {
            FocusPane::Worker if self.worker_view == WorkerView::Terminal => {
                // The DAG view shows rendered lines, not the planner PTY; copying the
                // PTY selection here would yield text the user cannot see.
                if self.selected_orchestrator_dag_active() {
                    return;
                }
                let Some(selection) = self.worker_selection else {
                    return;
                };
                self.selected_worker_selection_text(selection)
            }
            FocusPane::Task => {
                let Some(selection) = self.task_selection else {
                    return;
                };
                let width = self
                    .task_area
                    .map(block_inner)
                    .map(task_inner_width)
                    .unwrap_or(80);
                let lines = task_input_lines(&self.task_input, self.task_cursor, width);
                selected_text_from_lines(&lines, selection)
            }
            _ => return,
        };

        if text.trim().is_empty() {
            return;
        }

        match copy_text_to_clipboard(&text) {
            Ok(()) => {
                self.notice = Some(match self.focus {
                    FocusPane::Worker => "copied worker selection".to_string(),
                    FocusPane::Task => "copied task selection".to_string(),
                    FocusPane::Agents => "copied selection".to_string(),
                });
            }
            Err(error) => self.notice = Some(format!("copy failed: {error}")),
        }
    }

    fn select_previous_agent(&mut self) {
        self.delete_pending = None;
        self.orch_dag_scroll = 0;
        self.orch_selection = None;
        let visible = self.visible_agent_indices();
        if visible.is_empty() {
            self.selected_agent = 0;
            return;
        }
        let position = visible
            .iter()
            .position(|&index| index == self.selected_agent)
            .unwrap_or_else(|| {
                visible
                    .iter()
                    .position(|&index| index >= self.selected_agent)
                    .unwrap_or_else(|| visible.len().saturating_sub(1))
            });
        self.selected_agent = visible[position.saturating_sub(1)];
    }

    fn select_next_agent(&mut self) {
        self.delete_pending = None;
        self.orch_dag_scroll = 0;
        self.orch_selection = None;
        let visible = self.visible_agent_indices();
        if visible.is_empty() {
            self.selected_agent = 0;
            return;
        }
        let exact = visible
            .iter()
            .position(|&index| index == self.selected_agent);
        let step = match exact {
            // Still visible: advance to the following agent.
            Some(position) => position + 1,
            // Hidden/removed: the first visible index at or after the old
            // selection already IS the natural next agent, so do not skip it.
            None => visible
                .iter()
                .position(|&index| index >= self.selected_agent)
                .unwrap_or_else(|| visible.len().saturating_sub(1)),
        };
        self.selected_agent = visible[step.min(visible.len().saturating_sub(1))];
    }

    fn visible_agent_indices(&self) -> Vec<usize> {
        visible_agent_indices(&self.agents)
    }

    fn toggle_worker_view(&mut self) {
        self.worker_selection = None;
        self.worker_view = match self.worker_view {
            WorkerView::Terminal => {
                self.ensure_review_diff();
                WorkerView::Diff
            }
            WorkerView::Diff => {
                self.notice = None;
                WorkerView::Terminal
            }
        };
        self.focus = FocusPane::Worker;
    }

    fn review_all_ready(&mut self) {
        let sources = self.review_all_sources();

        if sources.is_empty() {
            self.notice = Some("no completed worktrees ready to review".to_string());
            return;
        }

        #[cfg(test)]
        {
            self.start_review_all_test_agent(sources);
            return;
        }

        #[cfg(not(test))]
        if let Err(error) = self.start_review_all_agent(sources) {
            self.notice = Some(format!("review all failed: {error}"));
        }
    }

    fn review_all_sources(&self) -> Vec<ReviewAllSource> {
        let claimed = self.review_all_claimed_source_ids();
        self.agents
            .iter()
            .filter(|run| run.status == AgentStatus::Done)
            .filter(|run| !claimed.contains(&run.id))
            .filter_map(|run| {
                let branch = run.worktree_branch.clone()?;
                Some(ReviewAllSource {
                    id: run.id.clone(),
                    branch,
                    task: run.task.clone(),
                    summary: if run.task_summary.trim().is_empty() {
                        short_task(&run.task)
                    } else {
                        run.task_summary.trim().to_string()
                    },
                    worktree_path: run.worktree_path.clone(),
                })
            })
            .collect()
    }

    fn review_all_claimed_source_ids(&self) -> HashSet<String> {
        self.agents
            .iter()
            .filter(|run| run.mode == AgentMode::ReviewAll && run.status != AgentStatus::Merged)
            .flat_map(|run| run.review_source_ids.iter().cloned())
            .collect()
    }

    #[cfg(test)]
    fn start_review_all_test_agent(&mut self, sources: Vec<ReviewAllSource>) {
        let worktree = WorktreeInfo {
            id: new_run_id("review all"),
            path: self.cwd.join(".rudder-review-all-test"),
            branch: Some("rudder/test-review-all".to_string()),
            path_is_worktree: true,
            workspace_name: None,
            jj_change_id: None,
        };
        let premerge = ReviewAllPremerge {
            merged_branches: sources.iter().map(|source| source.branch.clone()).collect(),
            ..ReviewAllPremerge::default()
        };
        let prompt = review_all_prompt(
            current_branch_at(&self.cwd).as_deref().unwrap_or("HEAD"),
            &worktree,
            &sources,
            &premerge,
        );
        let run = review_all_run(worktree, prompt, sources, None);
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.worker_selection = None;
        self.worker_view = WorkerView::Terminal;
        self.focus = FocusPane::Worker;
        self.notice = Some("started Codex review-all merge agent".to_string());
    }

    #[cfg(not(test))]
    fn start_review_all_agent(&mut self, sources: Vec<ReviewAllSource>) -> Result<()> {
        for source in &sources {
            if let Some(run) = self.agents.iter().find(|run| run.id == source.id) {
                commit_pending_changes_for_run(run)?;
            }
        }

        let target_ref = current_branch_at(&self.cwd)
            .or_else(|| {
                git_output(&self.cwd, ["rev-parse", "HEAD"])
                    .ok()
                    .map(|value| value.trim().to_string())
            })
            .unwrap_or_else(|| "HEAD".to_string());
        let worktree = prepare_worktree(&self.cwd, "review all completed worktrees")?;
        let premerge = premerge_review_all_sources(&worktree.path, &sources);
        let prompt = review_all_prompt(&target_ref, &worktree, &sources, &premerge);
        let session_id = mint_session_id_for(Backend::Codex);
        let command = agent_command(
            Backend::Codex,
            REVIEW_ALL_MODEL,
            Some(REVIEW_ALL_EFFORT),
            &prompt,
            AgentMode::ReviewAll,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(worktree.path.clone()),
            ..TerminalPaneOptions::default()
        };
        let mut run = review_all_run(worktree, prompt, sources, session_id);
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("failed to start Codex review-all: {error}"));
            }
        }
        let started = run.status == AgentStatus::Running;

        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.worker_selection = None;
        self.worker_view = WorkerView::Terminal;
        self.focus = FocusPane::Worker;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
        if started {
            let count = self
                .agents
                .get(self.selected_agent)
                .map(|run| run.review_source_ids.len())
                .unwrap_or(0);
            self.notice = Some(format!(
                "started Codex {REVIEW_ALL_MODEL} review-all for {count} worktree{}; press m on that row when done",
                if count == 1 { "" } else { "s" }
            ));
        }
        Ok(())
    }

    fn handle_task_key(&mut self, key: KeyEvent) -> bool {
        self.task_selection = None;
        if self.task_history_index.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.show_previous_task_history();
                    return false;
                }
                KeyCode::Down => {
                    self.show_next_task_history();
                    return false;
                }
                _ => {}
            }
        }

        if self.handle_picker_key(key) {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                self.reset_task_history_navigation();
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Shift+Enter inserts a literal newline in the task draft.
                self.reset_task_history_navigation();
                insert_str_at_cursor(&mut self.task_input, &mut self.task_cursor, "\n");
                self.clamp_picker_index();
            }
            KeyCode::Enter => self.start_task(),
            KeyCode::Up => self.show_previous_task_history(),
            KeyCode::Down => self.show_next_task_history(),
            KeyCode::Backspace => {
                self.reset_task_history_navigation();
                if key.modifiers.intersects(
                    KeyModifiers::ALT
                        | KeyModifiers::CONTROL
                        | KeyModifiers::SUPER
                        | KeyModifiers::META,
                ) {
                    delete_previous_word_at(&mut self.task_input, &mut self.task_cursor);
                } else {
                    delete_char_before_cursor(&mut self.task_input, &mut self.task_cursor);
                }
                self.clamp_picker_index();
            }
            KeyCode::Delete => {
                self.reset_task_history_navigation();
                delete_char_at_cursor(&mut self.task_input, self.task_cursor);
                self.clamp_picker_index();
            }
            KeyCode::Left => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META)
                {
                    self.task_cursor = previous_word_position(&self.task_input, self.task_cursor);
                } else {
                    self.task_cursor = self.task_cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                let len = self.task_input.chars().count();
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META)
                {
                    self.task_cursor = next_word_position(&self.task_input, self.task_cursor);
                } else {
                    self.task_cursor = (self.task_cursor + 1).min(len);
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B')
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META) =>
            {
                self.task_cursor = previous_word_position(&self.task_input, self.task_cursor);
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META) =>
            {
                self.task_cursor = next_word_position(&self.task_input, self.task_cursor);
            }
            KeyCode::Home => {
                self.task_cursor = 0;
            }
            KeyCode::End => {
                self.task_cursor = self.task_input.chars().count();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reset_task_history_navigation();
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reset_task_history_navigation();
                delete_previous_word_at(&mut self.task_input, &mut self.task_cursor);
                self.clamp_picker_index();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.task_cursor = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.task_cursor = self.task_input.chars().count();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reset_task_history_navigation();
                truncate_at_cursor(&mut self.task_input, self.task_cursor);
                self.clamp_picker_index();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reset_task_history_navigation();
                delete_char_at_cursor(&mut self.task_input, self.task_cursor);
                self.clamp_picker_index();
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reset_task_history_navigation();
                delete_char_before_cursor(&mut self.task_input, &mut self.task_cursor);
                self.clamp_picker_index();
            }
            KeyCode::Char('/') if self.task_input.is_empty() => {
                self.reset_task_history_navigation();
                self.task_input.push('/');
                self.task_cursor = 1;
                self.picker_index = 0;
                self.notice = Some(
                    "type /model, /main|/m, /goal, /usage, or /cloud".to_string(),
                );
            }
            KeyCode::Char(ch) => {
                self.reset_task_history_navigation();
                insert_char_at_cursor(&mut self.task_input, &mut self.task_cursor, ch);
                self.clamp_picker_index();
            }
            _ => {}
        }
        false
    }

    fn show_previous_task_history(&mut self) {
        if let Some(value) = previous_task_history_entry(
            &self.task_history,
            &mut self.task_history_index,
            &mut self.task_history_draft,
            &self.task_input,
        ) {
            self.replace_task_input(value);
        }
    }

    fn show_next_task_history(&mut self) {
        if let Some(value) = next_task_history_entry(
            &self.task_history,
            &mut self.task_history_index,
            &mut self.task_history_draft,
        ) {
            self.replace_task_input(value);
        }
    }

    fn replace_task_input(&mut self, value: String) {
        self.task_input = value;
        self.task_cursor = self.task_input.chars().count();
        self.task_selection = None;
        self.picker_index = 0;
        self.clamp_picker_index();
    }

    fn reset_task_history_navigation(&mut self) {
        self.task_history_index = None;
        self.task_history_draft.clear();
    }

    fn remember_task_history(&mut self, input: &str) {
        if input.trim().is_empty() {
            return;
        }
        self.task_history.push(input.to_string());
        if self.task_history.len() > TASK_HISTORY_LIMIT {
            let overflow = self.task_history.len() - TASK_HISTORY_LIMIT;
            self.task_history.drain(0..overflow);
        }
        self.reset_task_history_navigation();
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        let suggestions = suggestions_for(self);
        if suggestions.is_empty() {
            return false;
        }

        match key.code {
            KeyCode::Up => {
                self.picker_index = self.picker_index.saturating_sub(1);
                true
            }
            KeyCode::Down => {
                self.picker_index =
                    (self.picker_index + 1).min(suggestions.len().saturating_sub(1));
                true
            }
            KeyCode::Enter => {
                let selected = suggestions
                    .get(self.picker_index.min(suggestions.len().saturating_sub(1)))
                    .cloned();
                drop(suggestions);
                if let Some(selected) = selected {
                    self.apply_suggestion(selected);
                }
                true
            }
            _ => false,
        }
    }

    fn apply_suggestion(&mut self, suggestion: Suggestion) {
        self.reset_task_history_navigation();
        match suggestion.action {
            SuggestionAction::Insert(value) => {
                self.replace_task_input(value);
            }
            SuggestionAction::RunCommand(value) => {
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
                self.start_task_from_input(&value);
            }
            SuggestionAction::ChooseModelProvider(backend) => {
                self.replace_task_input(format!("/model {} ", backend.as_str()));
                self.notice = Some(format!("pick a {} model", backend.as_str()));
            }
            SuggestionAction::ChooseModel { backend, model } => {
                self.replace_task_input(format!("/model {} {} ", backend.as_str(), model));
                self.notice = Some(format!("pick effort for {model}"));
            }
            SuggestionAction::SetModel {
                backend,
                model,
                effort,
            } => {
                let warning = self.set_model_defaults(backend, model.clone(), effort);
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
                let mut should_spawn_main = false;
                if let Some(main_index) = self.selected_main_index() {
                    let cwd = self.cwd.clone();
                    let run = &mut self.agents[main_index];
                    run.backend = backend;
                    run.model = model;
                    run.effort = effort;
                    let _ = save_native_run_record(&cwd, run);
                    if run.terminal.is_none() && self.selected_agent == main_index {
                        should_spawn_main = true;
                    }
                }
                if should_spawn_main {
                    self.focus_or_spawn_main();
                }
                self.notice = warning.or_else(|| {
                    Some(format!(
                        "{} {}({})",
                        self.backend.as_str(),
                        self.model,
                        effort_label(self.effort)
                    ))
                });
            }
            SuggestionAction::ShowHelp => {
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
                self.notice = Some(
                    "Option-1/2/3 or ^W pane  Enter start/focus  wheel scrolls worker  R review all  M merge all"
                        .to_string(),
                );
            }
        }
    }

    fn clamp_picker_index(&mut self) {
        let len = suggestions_for(self).len();
        if len == 0 {
            self.picker_index = 0;
        } else {
            self.picker_index = self.picker_index.min(len - 1);
        }
    }

    fn handle_paste(&mut self, text: String) {
        match self.focus {
            FocusPane::Worker => {
                self.worker_selection = None;
                if self.worker_view == WorkerView::Diff {
                    if let Some(terminal) = self.selected_review_terminal_mut() {
                        if let Err(error) = terminal.write_input(text.as_bytes()) {
                            self.set_selected_review_error(error.to_string());
                        }
                    }
                } else if self.selected_worker_is_finished_cloud_command() {
                    self.notice = Some(
                        "cloud command finished; run /cloud again or press r to rerun".to_string(),
                    );
                } else {
                    let capture_as_prompt = self.selected_worker_accepts_prompt_input();
                    let result = match self.selected_terminal_mut() {
                        Some(terminal) => {
                            terminal.reset_scrollback();
                            terminal.write_input(&bracketed_paste_bytes(&text))
                        }
                        None => return,
                    };
                    if let Err(error) = result {
                        self.set_selected_error(error.to_string());
                        return;
                    }
                    self.clear_selected_attention_flags();
                    let prompts = self.capture_selected_worker_paste(&text, capture_as_prompt);
                    for prompt in prompts {
                        self.record_selected_worker_prompt(prompt);
                    }
                }
            }
            FocusPane::Task => {
                self.reset_task_history_navigation();
                insert_str_at_cursor(&mut self.task_input, &mut self.task_cursor, &text);
                self.clamp_picker_index();
            }
            FocusPane::Agents => {}
        }
    }

    fn selected_worker_accepts_prompt_input(&self) -> bool {
        let Some(run) = self.agents.get(self.selected_agent) else {
            return false;
        };
        if run.needs_permission {
            return false;
        }
        if matches!(
            run.status,
            AgentStatus::Done | AgentStatus::Merged | AgentStatus::Stopped
        ) {
            return true;
        }
        run.terminal.as_ref().is_some_and(|terminal| {
            terminal_looks_ready_for_input_from_lines(
                run.backend,
                &terminal.visible_lines_snapshot(),
            )
        })
    }

    fn selected_worker_is_finished_cloud_command(&self) -> bool {
        self.agents.get(self.selected_agent).is_some_and(|run| {
            run.terminal.is_some()
                && is_cloud_agent(run)
                && !matches!(run.status, AgentStatus::Running)
        })
    }

    fn capture_selected_worker_key(
        &mut self,
        key: KeyEvent,
        capture_as_prompt: bool,
    ) -> Option<String> {
        let run = self.agents.get_mut(self.selected_agent)?;
        update_worker_prompt_draft_for_key(
            &mut run.worker_input_draft,
            &mut run.worker_input_cursor,
            &mut run.worker_input_is_prompt,
            key,
            capture_as_prompt,
        )
    }

    fn capture_selected_worker_paste(
        &mut self,
        text: &str,
        capture_as_prompt: bool,
    ) -> Vec<String> {
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            return Vec::new();
        };
        update_worker_prompt_draft_for_paste(
            &mut run.worker_input_draft,
            &mut run.worker_input_cursor,
            &mut run.worker_input_is_prompt,
            text,
            capture_as_prompt,
        )
    }

    fn record_selected_worker_prompt(&mut self, prompt: String) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.remember_task_history(&prompt);
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            record_agent_prompt(run, prompt, "user");
            let _ = save_native_run_record(&self.cwd, run);
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if is_scroll_mouse_event(mouse.kind) {
            self.handle_pane_scroll(mouse);
            return;
        }

        // Continue an in-progress drag even if the pointer leaves the pane, so a fast
        // drag does not drop the selection. Orchestrator and worker selections are
        // tracked separately (the orchestrator pane is composed Lines, not a PTY).
        if self.orch_selection.is_some()
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            if let Some(worker_area) = self.worker_area {
                self.task_selection = None;
                if self.handle_orchestrator_selection_mouse(mouse, block_inner(worker_area)) {
                    return;
                }
            }
        }

        if self.worker_selection.is_some()
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            if let Some(worker_area) = self.worker_area {
                self.task_selection = None;
                if self.handle_worker_selection_mouse(mouse, block_inner(worker_area)) {
                    return;
                }
            }
        }

        if let Some(task_area) = self
            .task_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        {
            self.worker_selection = None;
            if self.handle_task_selection_mouse(mouse, block_inner(task_area)) {
                return;
            }
            return;
        }

        if let Some(agents_area) = self
            .agents_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        {
            self.worker_selection = None;
            self.task_selection = None;
            self.handle_agents_mouse(mouse, agents_area);
            return;
        }

        let Some(worker_area) = self
            .worker_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        else {
            return;
        };

        self.task_selection = None;
        let inner = block_inner(worker_area);

        if self.worker_view == WorkerView::Diff {
            if self.write_mouse_to_selected_review(mouse, inner) {
                return;
            }
            return;
        }

        // The orchestrator pane renders composed Lines, not the planner PTY: select
        // over the captured visible rows instead of forwarding to / selecting the PTY.
        if self.selected_is_orchestrator() {
            self.worker_selection = None;
            self.handle_orchestrator_selection_mouse(mouse, inner);
            return;
        }

        if self.handle_worker_selection_mouse(mouse, inner) {
            return;
        }
        if self.write_mouse_to_selected_worker(mouse, inner) {
            return;
        }
    }

    fn handle_agents_mouse(&mut self, mouse: MouseEvent, area: Rect) {
        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=agents",
            mouse.kind, mouse.column, mouse.row
        ));

        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }

        self.delete_pending = None;
        if let Some(index) = agent_index_from_mouse(self, mouse, area) {
            if index != self.selected_agent {
                self.orch_dag_scroll = 0;
                self.orch_selection = None;
            }
            self.selected_agent = index;
        }
    }

    fn handle_pane_scroll(&mut self, mouse: MouseEvent) {
        // The focus-shortcut routes scrolls to the worker pane only when the
        // pointer is also over the worker pane; otherwise the regular
        // pointer-based routing below kicks in so scrolling over a different
        // pane doesn't get silently eaten by an unrelated worker (e.g. a codex
        // agent on normal screen with no scrollback).
        if self.focus == FocusPane::Worker {
            if let Some(area) = self
                .worker_area
                .filter(|area| rect_contains(*area, mouse.column, mouse.row))
            {
                let inner = block_inner(area);
                self.set_mouse_debug(format!(
                    "mouse {:?} @{},{} focus=worker view={:?}",
                    mouse.kind, mouse.column, mouse.row, self.worker_view
                ));
                if self.selected_orchestrator_dag_active() {
                    self.scroll_orchestrator_dag(mouse, inner);
                } else if self.worker_view == WorkerView::Diff {
                    let _ = self.scroll_selected_review_or_forward(mouse, inner);
                } else {
                    let _ = self.scroll_selected_worker_or_forward(mouse, inner);
                }
                return;
            }
        }

        if let Some(area) = self
            .worker_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        {
            let inner = block_inner(area);
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=worker view={:?}",
                mouse.kind, mouse.column, mouse.row, self.worker_view
            ));
            if self.selected_orchestrator_dag_active() {
                self.scroll_orchestrator_dag(mouse, inner);
            } else if self.worker_view == WorkerView::Diff {
                let _ = self.scroll_selected_review_or_forward(mouse, inner);
            } else {
                let _ = self.scroll_selected_worker_or_forward(mouse, inner);
            }
            return;
        }

        if self
            .agents_area
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=agents",
                mouse.kind, mouse.column, mouse.row
            ));
            if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                self.select_previous_agent();
            } else if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                self.select_next_agent();
            }
            return;
        }

        if self
            .task_area
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=task route=ignored",
                mouse.kind, mouse.column, mouse.row
            ));
            return;
        }

        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=none route=ignored",
            mouse.kind, mouse.column, mouse.row
        ));
    }

    fn set_mouse_debug(&mut self, message: String) {
        if self.mouse_debug {
            self.mouse_debug_last = Some(message);
        }
    }

    fn handle_worker_selection_mouse(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        // The orchestrator DAG command-center view renders its own line list, not
        // the planner PTY. Dragging there would select PTY content the user cannot
        // see (a mismatch), so selection is cleanly disabled while the DAG shows.
        if self.selected_orchestrator_dag_active() {
            self.worker_selection = None;
            return false;
        }
        if self.selected_terminal_mut().is_none() {
            self.worker_selection = None;
            return false;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.worker_selection = Some(WorkerSelection {
                    start: selection_point_from_mouse(mouse, area),
                    end: selection_point_from_mouse(mouse, area),
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.autoscroll_worker_selection(mouse, area);
                if let Some(selection) = self.worker_selection.as_mut() {
                    selection.end = selection_point_from_mouse(mouse, area);
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(mut selection) = self.worker_selection else {
                    return false;
                };
                selection.end = selection_point_from_mouse(mouse, area);
                self.worker_selection = Some(selection);
                if selection_is_empty(normalize_selection(selection)) {
                    self.worker_selection = None;
                    return true;
                }
                let text = self.selected_worker_selection_text(selection);
                if text.trim().is_empty() {
                    self.notice = Some("selection empty".to_string());
                    return true;
                }
                match copy_text_to_clipboard(&text) {
                    Ok(()) => self.notice = Some("copied worker selection".to_string()),
                    Err(error) => self.notice = Some(format!("copy failed: {error}")),
                }
                true
            }
            _ => false,
        }
    }

    /// Mouse selection over the orchestrator pane (transcript / DAG / prose). Works
    /// against `orch_visible_rows`, the plain text `render_orchestrator` captures
    /// from the rendered buffer, so a drag highlights and copies exactly what is on
    /// screen regardless of wrapping or scroll. Mirrors `handle_worker_selection_mouse`.
    fn handle_orchestrator_selection_mouse(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let point = selection_point_from_mouse(mouse, area);
                self.orch_selection = Some(WorkerSelection {
                    start: point,
                    end: point,
                });
                self.dirty = true;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = self.orch_selection.as_mut() {
                    selection.end = selection_point_from_mouse(mouse, area);
                    self.dirty = true;
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(mut selection) = self.orch_selection else {
                    return false;
                };
                selection.end = selection_point_from_mouse(mouse, area);
                self.orch_selection = Some(selection);
                self.dirty = true;
                if selection_is_empty(normalize_selection(selection)) {
                    // A plain click (no drag) clears any prior highlight.
                    self.orch_selection = None;
                    return true;
                }
                let text = selected_text_from_lines(&self.orch_visible_rows, selection);
                if text.trim().is_empty() {
                    self.notice = Some("selection empty".to_string());
                    return true;
                }
                match copy_text_to_clipboard(&text) {
                    Ok(()) => self.notice = Some("copied orchestrator selection".to_string()),
                    Err(error) => self.notice = Some(format!("copy failed: {error}")),
                }
                true
            }
            _ => false,
        }
    }

    fn autoscroll_worker_selection(&mut self, mouse: MouseEvent, area: Rect) {
        let rows = if mouse.row < area.y {
            1
        } else if mouse.row >= area.bottom() {
            -1
        } else {
            0
        };
        if rows == 0 {
            return;
        }
        if let Some(terminal) = self.selected_terminal_mut() {
            terminal.scrollback_by(rows);
        }
    }

    fn handle_task_selection_mouse(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let Some(point) = task_selection_point_from_mouse(self, mouse, area) else {
            return false;
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.task_selection = Some(WorkerSelection {
                    start: point,
                    end: point,
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = self.task_selection.as_mut() {
                    selection.end = point;
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(mut selection) = self.task_selection else {
                    self.task_cursor = task_cursor_from_selection_point(
                        &self.task_input,
                        point,
                        task_inner_width(area),
                    );
                    return true;
                };
                selection.end = point;
                self.task_selection = Some(selection);
                let normalized = normalize_selection(selection);
                if selection_is_empty(normalized) {
                    self.task_selection = None;
                    self.task_cursor = task_cursor_from_selection_point(
                        &self.task_input,
                        point,
                        task_inner_width(area),
                    );
                    return true;
                }
                let input_lines =
                    task_input_lines(&self.task_input, self.task_cursor, task_inner_width(area));
                let text = selected_text_from_lines(&input_lines, selection);
                if text.trim().is_empty() {
                    self.notice = Some("selection empty".to_string());
                    return true;
                }
                match copy_text_to_clipboard(&text) {
                    Ok(()) => self.notice = Some("copied task selection".to_string()),
                    Err(error) => self.notice = Some(format!("copy failed: {error}")),
                }
                true
            }
            _ => false,
        }
    }

    fn selected_worker_selection_text(&self, selection: WorkerSelection) -> String {
        let Some(run) = self.agents.get(self.selected_agent) else {
            return String::new();
        };
        let Some(terminal) = run.terminal.as_ref() else {
            return String::new();
        };
        selected_text_from_lines(&terminal.visible_lines_snapshot(), selection)
    }

    fn write_mouse_to_selected_worker(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let Some(bytes) = mouse_event_to_sgr(mouse, area) else {
            return false;
        };
        let result = match self.selected_terminal_mut() {
            Some(terminal) => {
                if !terminal.wants_sgr_mouse_events() {
                    return false;
                }
                terminal.reset_scrollback();
                terminal.write_input(&bytes)
            }
            None => return false,
        };
        if let Err(error) = result {
            self.set_selected_error(error.to_string());
        }
        true
    }

    fn scroll_selected_worker_or_forward(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let rows = mouse_scrollback_delta(mouse, area.height);
        let mouse_bytes = mouse_event_to_sgr(mouse, area);
        let Some(terminal) = self.selected_terminal_mut() else {
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=worker route=no-terminal",
                mouse.kind, mouse.column, mouse.row
            ));
            return false;
        };
        let before = terminal.scrollback();
        let alternate = terminal.uses_alternate_screen();
        let mut forwarded = false;
        let mut write_error = None;
        terminal.scrollback_by(rows);
        let after = terminal.scrollback();
        let moved = after != before;
        let wants_mouse = if moved || rows == 0 {
            false
        } else {
            terminal.wants_sgr_mouse_events()
        };
        if !moved && rows != 0 && wants_mouse {
            if let Some(bytes) = mouse_bytes {
                if let Err(error) = terminal.write_input(&bytes) {
                    write_error = Some(error.to_string());
                } else {
                    forwarded = true;
                }
            }
        }
        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=worker rows={} before={} after={} moved={} alt={} wants_mouse={} forwarded={}",
            mouse.kind,
            mouse.column,
            mouse.row,
            rows,
            before,
            after,
            moved,
            alternate,
            wants_mouse,
            forwarded
        ));
        if let Some(error) = write_error {
            self.set_selected_error(error);
            return true;
        }
        if moved || forwarded {
            return true;
        }
        true
    }

    /// Scroll the orchestrator DAG command-center view. The DAG is a static list of
    /// rendered lines (not a PTY), so we move an app-level line offset. ScrollUp
    /// reveals earlier lines (smaller offset); ScrollDown reveals later lines. The
    /// upper bound is clamped against the rendered content in `render_orchestrator`.
    fn scroll_orchestrator_dag(&mut self, mouse: MouseEvent, area: Rect) {
        let delta = mouse_scrollback_delta(mouse, area.height);
        if delta == 0 {
            return;
        }
        // Scrolling shifts which rows are on screen; a stale selection would then
        // highlight the wrong text, so drop it.
        self.orch_selection = None;
        let before = self.orch_dag_scroll;
        // Positive delta is ScrollUp (toward the top), which lowers the offset.
        self.orch_dag_scroll = if delta > 0 {
            self.orch_dag_scroll.saturating_sub(delta as usize)
        } else {
            self.orch_dag_scroll
                .saturating_add(delta.unsigned_abs())
        };
        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=orchestrator-dag delta={} before={} after={}",
            mouse.kind, mouse.column, mouse.row, delta, before, self.orch_dag_scroll
        ));
    }

    fn write_mouse_to_selected_review(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let Some(bytes) = mouse_event_to_sgr(mouse, area) else {
            return false;
        };
        let result = match self.selected_review_terminal_mut() {
            Some(review) => {
                if !review.wants_sgr_mouse_events() {
                    return false;
                }
                review.reset_scrollback();
                review.write_input(&bytes)
            }
            None => return false,
        };
        if let Err(error) = result {
            self.set_selected_review_error(error.to_string());
        }
        true
    }

    fn scroll_selected_review_or_forward(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let rows = mouse_scrollback_delta(mouse, area.height);
        let mouse_bytes = mouse_event_to_sgr(mouse, area);
        let Some(review) = self.selected_review_terminal_mut() else {
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=review route=no-terminal",
                mouse.kind, mouse.column, mouse.row
            ));
            return false;
        };
        let before = review.scrollback();
        let alternate = review.uses_alternate_screen();
        review.scrollback_by(rows);
        let after = review.scrollback();
        let moved = after != before;
        let wants_mouse = if moved || rows == 0 {
            false
        } else {
            review.wants_sgr_mouse_events()
        };
        let mut forwarded = false;
        let mut write_error = None;
        if !moved && rows != 0 && wants_mouse {
            if let Some(bytes) = mouse_bytes {
                if let Err(error) = review.write_input(&bytes) {
                    write_error = Some(error.to_string());
                } else {
                    forwarded = true;
                }
            }
        }
        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=review rows={} before={} after={} moved={} alt={} wants_mouse={} forwarded={}",
            mouse.kind,
            mouse.column,
            mouse.row,
            rows,
            before,
            after,
            moved,
            alternate,
            wants_mouse,
            forwarded
        ));
        if let Some(error) = write_error {
            self.set_selected_review_error(error);
            return true;
        }
        if moved || forwarded {
            return true;
        }
        true
    }

    fn handle_worker_page_key(&mut self, key: KeyEvent, rows: isize) {
        let Some(bytes) = terminal_bytes_for_key(key) else {
            return;
        };
        let result = match self.selected_terminal_mut() {
            Some(terminal) => {
                if terminal.uses_alternate_screen() {
                    terminal.write_input(&bytes)
                } else {
                    terminal.scrollback_by(rows);
                    Ok(())
                }
            }
            None => return,
        };
        if let Err(error) = result {
            self.set_selected_error(error.to_string());
        }
    }

    fn start_task(&mut self) {
        let input = self.task_input.trim().to_string();
        if input.is_empty() {
            // Empty Enter while a plan awaits approval = approve & launch. This makes
            // the task pane the single plan-mode surface: type to refine, Enter to go.
            if self.awaiting_approval {
                self.approve_planned_queue();
            }
            return;
        }
        self.remember_task_history(&input);
        self.task_input.clear();
        self.task_cursor = 0;
        self.worker_selection = None;
        self.start_task_from_input(&input);
    }

    fn start_task_from_input(&mut self, input: &str) {
        if self.handle_command(&input) {
            return;
        }
        self.notice = None;

        // REFINE (plan-mode-style discussion): while a plan is parsed but not yet
        // approved, a typed message is feedback to the orchestrator, not a new node.
        // Re-run the planner with the current DAG + the feedback so it REVISES the
        // whole plan and the DAG tree updates in place. The user iterates until they
        // approve with Enter.
        if self.awaiting_approval {
            self.refine_plan(&input);
            return;
        }

        if self.plan_mode {
            // Explicit /plan mode: a read-only planner that never spawns workers.
            // The /plan toggle is unchanged by reconcile: it always runs a fresh
            // read-only planner regardless of any active plan.
            self.start_plan_task(&input);
            return;
        }

        // RECONCILE: when a plan is already active, a second task must be folded
        // INTO the existing DAG, not handed to a fresh planner that would replace
        // it. Route to the append pipeline and return so the in-flight nodes are
        // preserved. The no-plan-active first task falls through to the planner.
        if self.plan_is_active() {
            self.reconcile_injection(&input);
            return;
        }

        // Default first task (no active plan): run the adaptive planner. It
        // inspects the repo read-only and emits a task DAG; the poll loop then
        // gates the plan for the user to approve (Enter) or discard.
        self.start_rudder_plan_task(&input);
    }

    /// True when a plan is currently active: nodes are queued/awaiting approval,
    /// an orchestrator is still planning, OR at least one plan-launched agent (one
    /// carrying a `node_id`) has not yet merged. While this holds, a newly typed
    /// task is RECONCILED into the existing plan instead of starting a fresh one.
    pub(crate) fn plan_is_active(&self) -> bool {
        if !self.planned_nodes.is_empty() || self.has_planning_orchestrator() {
            return true;
        }
        self.agents
            .iter()
            .any(|run| run.node_id.is_some() && run.status != AgentStatus::Merged)
    }

    /// The current plan FRONTIER: the `(id, title)` of every node the added task
    /// can depend on. That is the still-queued planned nodes PLUS every
    /// plan-launched agent (carrying a `node_id`) that has not yet merged.
    fn plan_frontier(&self) -> Vec<(String, String)> {
        let mut frontier: Vec<(String, String)> = self
            .planned_nodes
            .iter()
            .map(|node| (node.id.clone(), node.title.clone()))
            .collect();
        for run in &self.agents {
            if run.status == AgentStatus::Merged {
                continue;
            }
            let Some(node_id) = run.node_id.as_ref() else {
                continue;
            };
            if frontier.iter().any(|(id, _)| id == node_id) {
                continue;
            }
            frontier.push((node_id.clone(), run.task_summary.clone()));
        }
        frontier
    }

    /// Build the RECONCILE planner command: same plan-mode + read-only backend
    /// flags as the orchestrator, but carrying the reconcile prompt directly (which
    /// is already plan-mode-safe) instead of the fresh-plan prompt. Mirrors the
    /// `AgentMode::RudderPlan` branch of `agent_command` without re-wrapping the
    /// prompt as a brand-new plan.
    fn reconcile_planner_command(
        &self,
        backend: Backend,
        model: &str,
        effort: Option<EffortLevel>,
        prompt: &str,
        session_id: Option<&str>,
    ) -> TerminalCommand {
        match backend {
            Backend::Claude => {
                let mut args = vec![
                    "--permission-mode".to_string(),
                    "plan".to_string(),
                    "--name".to_string(),
                    format!("reconcile:{}", short_task(prompt)),
                ];
                if !model.trim().is_empty() {
                    args.push("--model".to_string());
                    args.push(model.to_string());
                }
                if let Some(effort) = effort {
                    args.push("--effort".to_string());
                    args.push(effort.as_str().to_string());
                }
                if let Some(sid) = session_id {
                    args.push("--session-id".to_string());
                    args.push(sid.to_string());
                }
                args.push(prompt.to_string());
                TerminalCommand::with_args("claude", args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
            }
            Backend::Codex => {
                let mut args = vec!["--no-alt-screen".to_string()];
                args.push("--enable".to_string());
                args.push("goals".to_string());
                args.push("--sandbox".to_string());
                args.push("read-only".to_string());
                args.push("--ask-for-approval".to_string());
                args.push("never".to_string());
                args.push("--search".to_string());
                push_codex_rudder_config_overrides(&mut args, effort);
                if !model.trim().is_empty() {
                    args.push("-m".to_string());
                    args.push(model.to_string());
                }
                args.push(prompt.to_string());
                TerminalCommand::with_args(codex_program(), args)
                    .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
            }
        }
    }

    /// Fold an ADDED task into the active plan. Spawns a RudderPlan agent flagged as
    /// a RECONCILE planner whose prompt names the current frontier and asks for
    /// exactly one new node with inferred deps. The poll loop routes that planner's
    /// completion to `evaluate_completed_reconcile`, which APPENDS the node to
    /// `planned_nodes` (never replaces) and schedules it if the session is running.
    fn reconcile_injection(&mut self, input: &str) {
        let frontier = self.plan_frontier();
        let model = self.model.clone();
        let backend = self.backend;
        // Reconciling one task against a known frontier does not need max reasoning;
        // cap the planner's effort just like the initial planner does.
        let effort = match self.effort {
            Some(EffortLevel::High) | Some(EffortLevel::XHigh) | Some(EffortLevel::Max) => {
                Some(EffortLevel::Medium)
            }
            other => other,
        };
        let session_id = mint_session_id_for(backend);
        let prompt = rudder_reconcile_prompt(input, &frontier);
        let command =
            self.reconcile_planner_command(backend, &model, effort, &prompt, session_id.as_deref());
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        let created_at = now_stamp();
        let mut run = AgentRun {
            id: new_run_id(input),
            created_at: created_at.clone(),
            mode: AgentMode::RudderPlan,
            task: input.to_string(),
            task_summary: format!("add {}", summarize_task(input)),
            current_prompt: input.to_string(),
            turns: vec![AgentTurn {
                ts: created_at.clone(),
                prompt: input.to_string(),
                source: "user".to_string(),
            }],
            last_user_input_at: created_at,
            backend,
            model,
            effort,
            status: AgentStatus::Running,
            cwd: self.cwd.clone(),
            worktree_branch: None,
            worktree_path: None,
            workspace_name: None,
            jj_change_id: None,
            session_id,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
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
            // Discriminator: route completion to the APPEND path, not REPLACE.
            reconcile_planner: true,
            plan_stream: None,
            last_worker_input_at: None,
            ready_since: None,
            merge_resolver: false,
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                self.notice = Some("reconciling added task into the plan".to_string());
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!(
                    "failed to start {} reconcile planner: {error}",
                    backend.as_str()
                ));
            }
        }

        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = FocusPane::Worker;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
    }

    /// Launch a live execute agent. When `node` is `Some`, the run is tagged with
    /// its plan node id and hard deps (so the scheduler can track its merge and
    /// unblock dependents), and the node's backend/model/effort overrides apply.
    /// When `None`, this is an ordinary manual launch using the dashboard defaults.
    fn start_execute_task_node(
        &mut self,
        input: &str,
        explicit_summary: Option<&str>,
        node: Option<PlannedNode>,
    ) {
        let planner_title = explicit_summary
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| rudder_plan_worker_title_from_prompt(input));
        let should_generate_summary = planner_title.is_none();
        let worktree_label = planner_title.as_deref().unwrap_or(input);
        let task_summary = planner_title
            .as_deref()
            .map(|title| truncate_chars(title, 56))
            .unwrap_or_else(|| summarize_task(input));
        let worktree = match prepare_jj_workspace(&self.cwd, worktree_label) {
            Ok(worktree) => worktree,
            Err(error) => {
                // New runs are jj-isolated; never silently fall back to git.
                // Surface the failure and run in the current checkout so the user
                // sees the error rather than a half-broken merge later.
                self.notice = Some(format!("jj workspace failed: {error}"));
                WorktreeInfo::current(self.cwd.clone())
            }
        };
        if let Err(error) = write_rudder_context(&self.cwd, &self.agents, Some(&worktree)) {
            self.notice = Some(format!("context warning: {error}"));
        }

        // Plan node overrides win over the dashboard defaults when supplied.
        let backend = node
            .as_ref()
            .and_then(|n| n.backend.as_deref())
            .and_then(provider_backend)
            .unwrap_or(self.backend);
        let model = node
            .as_ref()
            .and_then(|n| n.model.clone())
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| self.model.clone());
        let effort = node
            .as_ref()
            .and_then(|n| n.effort.as_deref())
            .and_then(EffortLevel::parse)
            .or(self.effort);
        let node_id = node.as_ref().map(|n| n.id.clone());
        let node_deps = node.as_ref().map(|n| n.deps.clone()).unwrap_or_default();
        let session_id = mint_session_id_for(backend);
        // Lead the launch prompt with the canonical /goal + Done-when block so
        // every spawned agent has a clear objective and verifiable stopping
        // condition. Idempotent: a /rudder-plan worker prompt that already leads
        // with /goal is left unchanged.
        let goal_prompt = manual_goal_prompt(input);
        let command = agent_command(
            backend,
            &model,
            effort,
            &goal_prompt,
            AgentMode::Execute,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(worktree.path.clone()),
            ..TerminalPaneOptions::default()
        };

        let created_at = now_stamp();
        let mut run = AgentRun {
            id: worktree.id.clone(),
            created_at: created_at.clone(),
            mode: AgentMode::Execute,
            task: goal_prompt.clone(),
            task_summary,
            current_prompt: goal_prompt.clone(),
            turns: vec![AgentTurn {
                ts: created_at.clone(),
                prompt: goal_prompt.clone(),
                source: "user".to_string(),
            }],
            last_user_input_at: created_at,
            backend,
            model,
            effort,
            status: AgentStatus::Running,
            cwd: worktree.path.clone(),
            worktree_branch: worktree.branch.clone(),
            worktree_path: worktree.path_is_worktree.then_some(worktree.path.clone()),
            workspace_name: worktree.workspace_name.clone(),
            jj_change_id: worktree.jj_change_id.clone(),
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
            review_source_ids: Vec::new(),
            deps: node_deps,
            soft_deps: Vec::new(),
            node_id,
            reconcile_planner: false,
            plan_stream: None,
            last_worker_input_at: None,
            ready_since: None,
            merge_resolver: false,
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("failed to start {}: {error}", backend.as_str()));
            }
        }

        let run_id = run.id.clone();
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = FocusPane::Worker;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
        if should_generate_summary {
            spawn_task_summary_worker(self.task_summary_tx.clone(), run_id, input.to_string());
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
    }

    fn start_plan_task(&mut self, input: &str) {
        let model = self.model.clone();
        let backend = self.backend;
        let effort = self.effort;
        let session_id = mint_session_id_for(backend);
        let command = agent_command(
            backend,
            &model,
            effort,
            input,
            AgentMode::Plan,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        let created_at = now_stamp();
        let mut run = AgentRun {
            id: new_run_id(input),
            created_at: created_at.clone(),
            mode: AgentMode::Plan,
            task: input.to_string(),
            task_summary: summarize_task(input),
            current_prompt: input.to_string(),
            turns: vec![AgentTurn {
                ts: created_at.clone(),
                prompt: input.to_string(),
                source: "user".to_string(),
            }],
            last_user_input_at: created_at,
            backend,
            model,
            effort,
            status: AgentStatus::Running,
            cwd: self.cwd.clone(),
            worktree_branch: None,
            worktree_path: None,
            workspace_name: None,
            jj_change_id: None,
            session_id,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
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
            ready_since: None,
            merge_resolver: false,
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                self.notice = Some("read-only planner started".to_string());
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!(
                    "failed to start {} planner: {error}",
                    backend.as_str()
                ));
            }
        }

        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = FocusPane::Worker;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
    }

    fn start_rudder_plan_task(&mut self, input: &str) {
        // Remember the original request so the refine loop can re-plan against it
        // (each refinement layers the user's feedback on top of this, not on top of
        // the previous composite prompt).
        self.plan_request = input.to_string();
        let model = self.model.clone();
        let backend = self.backend;
        // Decomposing a task into a node DAG does not need max reasoning. Cap the
        // planner's effort so plan mode stays responsive even when the dashboard
        // default is a heavy model at high effort. The model itself is unchanged.
        let effort = match self.effort {
            Some(EffortLevel::High) | Some(EffortLevel::XHigh) | Some(EffortLevel::Max) => {
                Some(EffortLevel::Medium)
            }
            other => other,
        };
        let session_id = mint_session_id_for(backend);
        let command = agent_command(
            backend,
            &model,
            effort,
            input,
            AgentMode::RudderPlan,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        let created_at = now_stamp();
        let mut run = AgentRun {
            id: new_run_id(input),
            created_at: created_at.clone(),
            mode: AgentMode::RudderPlan,
            task: input.to_string(),
            task_summary: format!("plan {}", summarize_task(input)),
            current_prompt: input.to_string(),
            turns: vec![AgentTurn {
                ts: created_at.clone(),
                prompt: input.to_string(),
                source: "user".to_string(),
            }],
            last_user_input_at: created_at,
            backend,
            model,
            effort,
            status: AgentStatus::Running,
            cwd: self.cwd.clone(),
            worktree_branch: None,
            worktree_path: None,
            workspace_name: None,
            jj_change_id: None,
            session_id,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
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
            ready_since: None,
            merge_resolver: false,
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                self.notice = Some("planner started".to_string());
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!(
                    "failed to start {} planner: {error}",
                    backend.as_str()
                ));
            }
        }

        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = FocusPane::Worker;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
    }

    /// REFINE the pending plan with the user's feedback (plan-mode-style
    /// discussion). Re-runs the pinned orchestrator with the original request, the
    /// current DAG, and the feedback so it emits a REVISED full DAG that REPLACES
    /// the pending plan. Stays at the approval gate; nothing launches until Enter.
    fn refine_plan(&mut self, feedback: &str) {
        let Some(index) = self.agents.iter().position(|run| run.is_orchestrator()) else {
            // No orchestrator to refine (shouldn't happen while awaiting approval):
            // fall back to a fresh plan from the feedback.
            self.start_rudder_plan_task(feedback);
            return;
        };
        let (backend, model, effort, session) = {
            let run = &self.agents[index];
            (
                run.backend,
                run.model.clone(),
                run.effort,
                run.session_id.clone().filter(|s| !s.trim().is_empty()),
            )
        };
        // FOLLOW-UP when we have a session to resume: the model already remembers the
        // prior plan + reasoning, so send only the slim feedback and RESUME the
        // conversation (claude --resume / codex exec resume). Otherwise fall back to a
        // fresh decompose with the full context crammed into the prompt.
        let resume = session.is_some();
        let command = match &session {
            Some(sid) => rudder_plan_refine_command(
                backend,
                &model,
                effort,
                &build_refine_followup(feedback),
                sid,
            ),
            None => {
                let original = if self.plan_request.trim().is_empty() {
                    self.planned_origin.clone()
                } else {
                    self.plan_request.clone()
                };
                let outline = self.current_plan_outline();
                let composite = build_refine_request(&original, &outline, feedback);
                agent_command(
                    backend,
                    &model,
                    effort,
                    &composite,
                    AgentMode::RudderPlan,
                    mint_session_id_for(backend).as_deref(),
                )
            }
        };
        // Mark the refine in flight: keeps awaiting_approval = true (the scheduler
        // never launches the stale plan and Enter cannot approve it mid-refine) while
        // maybe_detect_plan_ready still captures the revised DAG. evaluate_completed_plan
        // clears `refining` once the new plan lands.
        self.refining = true;
        if let Some(run) = self.agents.get_mut(index) {
            if resume {
                // Keep the conversation transcript; append "you: <feedback>" and move
                // the parse baseline so the prior block is not re-captured. Re-point
                // ingest at the new PTY without wiping the pane.
                if let Some(stream) = run.plan_stream.as_mut() {
                    stream.begin_user_turn(feedback);
                    stream.rebind_stream();
                }
            } else {
                // Fresh re-plan (no session): start a clean transcript.
                run.plan_stream = Some(PlanStreamState::new());
            }
        }
        if self.relaunch_orchestrator_with(index, command, feedback) {
            self.notice = Some("refining the plan with your feedback…".to_string());
        } else {
            // The planner could not be relaunched: drop back to the existing plan so
            // the user is not stuck (they can still approve it or try again).
            self.refining = false;
            self.notice = Some("could not relaunch the planner; the current plan still stands".to_string());
        }
    }

    /// A compact, human-readable outline of the currently-queued plan, fed back to
    /// the orchestrator so it can revise rather than start from scratch.
    fn current_plan_outline(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for node in &self.planned_nodes {
            let mut deps: Vec<String> = node.deps.iter().map(|d| format!("{d}:hard")).collect();
            deps.extend(node.soft_deps.iter().map(|d| format!("{d}:soft")));
            let deps_str = if deps.is_empty() {
                "none".to_string()
            } else {
                deps.join(", ")
            };
            let goal = node.goal.clone().unwrap_or_default();
            lines.push(format!(
                "- {} [{}]{} deps: {}",
                node.id,
                node.title,
                if goal.is_empty() {
                    String::new()
                } else {
                    format!(" goal: {goal}")
                },
                deps_str
            ));
        }
        if lines.is_empty() {
            "(no tasks yet)".to_string()
        } else {
            lines.join("\n")
        }
    }

    /// Re-launch an existing orchestrator run (by index) with a NEW composite task
    /// and re-arm autosteer so its revised plan is captured. Mirrors the spawn in
    /// start_rudder_plan_task but reuses the pinned orchestrator row in place.
    fn relaunch_orchestrator_with(
        &mut self,
        index: usize,
        command: TerminalCommand,
        feedback: &str,
    ) -> bool {
        let cwd = self.cwd.clone();
        let Some(run) = self.agents.get_mut(index) else {
            return false;
        };
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(run.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        let launched = match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                // Record the follow-up as a turn; KEEP run.task (the original label)
                // and run.session_id (so the next refine can resume the same session;
                // a fresh-fallback's new id is captured from the stream on ingest).
                run.current_prompt = feedback.to_string();
                let now = now_stamp();
                run.turns.push(AgentTurn {
                    ts: now.clone(),
                    prompt: feedback.to_string(),
                    source: "user".to_string(),
                });
                run.last_user_input_at = now;
                run.status = AgentStatus::Running;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.autosteered = true;
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                run.last_error = None;
                let _ = save_native_run_record(&cwd, run);
                true
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                false
            }
        };
        self.selected_agent = index;
        self.focus = FocusPane::Worker;
        self.worker_view = WorkerView::Terminal;
        launched
    }

    fn restore_running_agents(&mut self) {
        let snapshot: Vec<(usize, MigratedAgent)> = self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(idx, run)| {
                if run.terminal.is_some() || run.status != AgentStatus::Running {
                    return None;
                }
                Some((
                    idx,
                    MigratedAgent {
                        run_id: run.id.clone(),
                        session_id: run.session_id.clone().unwrap_or_default(),
                        worktree_path: run.worktree_path.clone().unwrap_or(run.cwd.clone()),
                        fresh_prompt: None,
                    },
                ))
            })
            .collect();
        let total = snapshot.len();
        if total == 0 {
            return;
        }
        let mut resumed = 0;
        for (idx, entry) in snapshot {
            if self.spawn_claude_resume_for(idx, &entry) {
                resumed += 1;
            }
        }
        if resumed > 0 {
            self.notice = Some(format!("resumed {resumed} agent(s) from last session"));
        }
        let needs_manual = self
            .agents
            .iter()
            .filter(|run| {
                run.terminal.is_none()
                    && run.status == AgentStatus::Running
                    && !can_resume_agent(run)
            })
            .count();
        if needs_manual > 0 {
            let prefix = self
                .notice
                .take()
                .map(|n| format!("{n}; "))
                .unwrap_or_default();
            self.notice = Some(format!(
                "{prefix}{needs_manual} agent(s) could not be resumed"
            ));
        }
    }

    fn resume_migrated_agents(&mut self) {
        if self.migration_resumes_attempted {
            return;
        }
        self.migration_resumes_attempted = true;
        if self.pending_migration_resumes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_migration_resumes);
        let mut resumed = 0usize;
        for entry in pending {
            if let Some(idx) = self.agents.iter().position(|run| run.id == entry.run_id) {
                if self.spawn_claude_resume_for(idx, &entry) {
                    resumed += 1;
                }
            }
        }
        if resumed > 0 {
            self.notice = Some(format!(
                "resumed {resumed} migrated agent(s) via claude --resume"
            ));
        }
    }

    fn spawn_claude_resume_for(&mut self, index: usize, entry: &MigratedAgent) -> bool {
        let Some(run) = self.agents.get_mut(index) else {
            return false;
        };
        if run.terminal.is_some() && run.status == AgentStatus::Running {
            return false;
        }
        let cwd = if entry.worktree_path.as_os_str().is_empty() {
            run.cwd.clone()
        } else {
            entry.worktree_path.clone()
        };
        let command = if !entry.session_id.is_empty() && run.backend == Backend::Claude {
            claude_resume_command(run, &entry.session_id)
        } else if !entry.session_id.is_empty() && run.backend == Backend::Codex {
            codex_resume_command(run, &entry.session_id)
        } else {
            let session_id = mint_session_id_for(run.backend);
            // If the local CLI built a context-rich handoff prompt for this
            // migrated agent (because we couldn't resume the real session),
            // use that as the agent's input so it has continuity instead of
            // restarting from the bare task.
            let prompt_for_agent = entry
                .fresh_prompt
                .clone()
                .unwrap_or_else(|| run.task.clone());
            let cmd = agent_command(
                run.backend,
                &run.model,
                run.effort,
                &prompt_for_agent,
                run.mode,
                session_id.as_deref(),
            );
            run.session_id = session_id;
            cmd
        };
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.last_error = None;
                run.cwd = cwd;
                let _ = save_native_run_record(&self.cwd, run);
                true
            }
            Err(error) => {
                run.last_error = Some(error.to_string());
                false
            }
        }
    }

    fn start_rename_selected_agent(&mut self) {
        if self.selected_is_main() {
            self.notice = Some("main agent: rename disabled".to_string());
            return;
        }
        let Some(run) = self.agents.get(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        let current = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        self.rename_cursor = current.chars().count();
        self.rename_input = Some(current);
    }

    fn cancel_rename(&mut self) {
        self.rename_input = None;
        self.rename_cursor = 0;
    }

    fn commit_rename(&mut self) {
        let Some(new_name) = self.rename_input.take() else {
            return;
        };
        self.rename_cursor = 0;
        let trimmed = new_name.trim();
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            return;
        };
        if trimmed.is_empty() {
            return;
        }
        run.task_summary = trimmed.to_string();
        let _ = save_native_run_record(&self.cwd, run);
        self.notice = Some(format!("renamed to {trimmed}"));
    }

    fn handle_rename_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut input) = self.rename_input.take() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => {
                self.cancel_rename();
                return true;
            }
            KeyCode::Enter => {
                self.rename_input = Some(input);
                self.commit_rename();
                return true;
            }
            KeyCode::Backspace => {
                if self.rename_cursor > 0 {
                    let chars: Vec<char> = input.chars().collect();
                    let new_cursor = self.rename_cursor - 1;
                    let mut next = String::new();
                    for (i, c) in chars.into_iter().enumerate() {
                        if i != new_cursor {
                            next.push(c);
                        }
                    }
                    input = next;
                    self.rename_cursor = new_cursor;
                }
            }
            KeyCode::Left => {
                if self.rename_cursor > 0 {
                    self.rename_cursor -= 1;
                }
            }
            KeyCode::Right => {
                let len = input.chars().count();
                if self.rename_cursor < len {
                    self.rename_cursor += 1;
                }
            }
            KeyCode::Home => self.rename_cursor = 0,
            KeyCode::End => self.rename_cursor = input.chars().count(),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let chars: Vec<char> = input.chars().collect();
                let mut next = String::new();
                for (i, c) in chars.iter().enumerate() {
                    if i == self.rename_cursor {
                        next.push(ch);
                    }
                    next.push(*c);
                }
                if self.rename_cursor >= chars.len() {
                    next.push(ch);
                }
                input = next;
                self.rename_cursor += 1;
            }
            _ => {}
        }
        self.rename_input = Some(input);
        true
    }

    fn focus_or_spawn_main(&mut self) {
        self.focus_or_spawn_main_with_prompt("");
    }

    fn focus_or_spawn_main_with_prompt(&mut self, override_prompt: &str) {
        let main_index = match self.selected_main_index() {
            Some(idx) => idx,
            None => {
                self.notice = Some("no main agent".to_string());
                return;
            }
        };
        self.selected_agent = main_index;
        self.delete_pending = None;

        let already_running = self
            .agents
            .get(main_index)
            .and_then(|run| run.terminal.as_ref())
            .is_some();
        if already_running {
            // Already spawned. If the user gave a new prompt, forward it
            // straight into the live PTY so they don't have to re-focus and
            // type it themselves.
            if !override_prompt.is_empty() {
                if let Some(run) = self.agents.get_mut(main_index) {
                    if let Some(terminal) = run.terminal.as_mut() {
                        let _ = terminal.write_input(format!("{override_prompt}\r").as_bytes());
                        let now = now_stamp();
                        run.turns.push(AgentTurn {
                            ts: now.clone(),
                            prompt: override_prompt.to_string(),
                            source: "user".to_string(),
                        });
                        run.last_user_input_at = now;
                    }
                }
            }
            self.focus = FocusPane::Worker;
            self.worker_view = WorkerView::Terminal;
            return;
        }

        let (backend, model, effort, terminal_size, bootstrap, session_id) = {
            let run = &self.agents[main_index];
            let bootstrap = if !override_prompt.is_empty() {
                override_prompt.to_string()
            } else if run.turns.is_empty() {
                MAIN_BOOTSTRAP_PROMPT.to_string()
            } else {
                String::new()
            };
            (
                run.backend,
                run.model.clone(),
                run.effort,
                run.terminal_size.unwrap_or_default(),
                bootstrap,
                run.session_id
                    .clone()
                    .or_else(|| mint_session_id_for(run.backend)),
            )
        };
        let command = agent_command(
            backend,
            &model,
            effort,
            &bootstrap,
            AgentMode::Main,
            session_id.as_deref(),
        );
        let cwd = self.cwd.clone();
        let options = TerminalPaneOptions {
            size: terminal_size,
            cwd: Some(cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                let run = &mut self.agents[main_index];
                run.cwd = cwd;
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.session_id = session_id;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.needs_permission = false;
                run.permission_notified = false;
                run.last_error = None;
                if !bootstrap.is_empty() {
                    let now = now_stamp();
                    run.current_prompt = bootstrap.clone();
                    run.turns.push(AgentTurn {
                        ts: now.clone(),
                        prompt: bootstrap.clone(),
                        source: "bootstrap".to_string(),
                    });
                    run.last_user_input_at = now;
                }
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
                let _ = save_native_run_record(&self.cwd, run);
            }
            Err(error) => {
                let run = &mut self.agents[main_index];
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("main launch failed: {error}"));
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
    }

    fn open_main_model_switcher(&mut self) {
        if !self.selected_is_main() {
            return;
        }
        self.replace_task_input("/model ".to_string());
        self.focus = FocusPane::Task;
        self.notice = Some("pick a model for main".to_string());
    }

    fn restart_selected_agent(&mut self) {
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        if run.terminal.is_some() && run.status == AgentStatus::Running {
            self.notice = Some("selected agent is already running".to_string());
            return;
        }

        let prompt = run.task.clone();
        let session_id = mint_session_id_for(run.backend);
        let command = agent_command(
            run.backend,
            &run.model,
            run.effort,
            &prompt,
            run.mode,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(run.cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.session_id = session_id;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.autosteered = matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan);
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                run.last_error = None;
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
                self.notice = Some(format!("restarted {}", short_task(&run.task)));
                let _ = save_native_run_record(&self.cwd, run);
                let _ = write_rudder_context(&self.cwd, &self.agents, None);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("restart failed: {error}"));
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
    }

    fn handle_command(&mut self, input: &str) -> bool {
        let mut parts = input.split_whitespace();
        match parts.next() {
            Some("/model") => {
                let args = parts.collect::<Vec<_>>();
                match args.as_slice() {
                    [] => {
                        self.notice =
                            Some("usage: /model claude|codex <model> [effort]".to_string());
                    }
                    [provider] if provider_backend(provider).is_some() => {
                        self.notice = Some(format!("usage: /model {provider} <model> [effort]"));
                    }
                    [provider, model] if provider_backend(provider).is_some() => {
                        let backend = provider_backend(provider).unwrap();
                        let warning = self.set_model_defaults(
                            backend,
                            (*model).to_string(),
                            default_effort_for(backend, model),
                        );
                        self.notice = warning.or_else(|| {
                            Some(format!(
                                "{} {}({})",
                                self.backend.as_str(),
                                self.model,
                                effort_label(self.effort)
                            ))
                        });
                    }
                    [provider, model, effort, ..] if provider_backend(provider).is_some() => {
                        let backend = provider_backend(provider).unwrap();
                        let parsed_effort = parse_effort_arg(effort);
                        let warning =
                            self.set_model_defaults(backend, (*model).to_string(), parsed_effort);
                        self.notice = warning.or_else(|| {
                            Some(format!(
                                "{} {}({})",
                                self.backend.as_str(),
                                self.model,
                                effort_label(self.effort)
                            ))
                        });
                    }
                    _ => {
                        let model = args.join(" ");
                        let backend = backend_for_model(&model);
                        let effort = default_effort_for(backend, &model);
                        let warning = self.set_model_defaults(backend, model, effort);
                        self.notice = warning.or_else(|| {
                            Some(format!(
                                "{} {}({})",
                                self.backend.as_str(),
                                self.model,
                                effort_label(self.effort)
                            ))
                        });
                    }
                }
                true
            }
            Some("/plan") => {
                // Planning is the default paradigm now: just type a task and the
                // orchestrator plans it, then you refine before approving. The old
                // standalone /plan read-only mode is retired.
                self.notice = Some(
                    "planning is the default — just type your task; the orchestrator plans it and you refine before approving"
                        .to_string(),
                );
                true
            }
            Some("/help") => {
                self.notice = Some(
                    "Option-1/2/3 or ^W pane  Enter start/focus  type to refine the plan  /model  /main|/m  /goal  /automerge"
                        .to_string(),
                );
                true
            }
            Some("/login") => {
                self.start_rudder_cli_command("cloud login", vec!["login".to_string()]);
                true
            }
            Some("/cloud") => {
                let raw_args = parts.collect::<Vec<_>>();
                if cloud_args_need_auth(&raw_args) && !rudder_cloud_authenticated() {
                    self.notice =
                        Some("not logged in to Rudder Cloud; run /login first".to_string());
                    return true;
                }
                if raw_args.is_empty() {
                    self.notice = Some(
                        "Exit this dashboard and run `rudder cloud` to open the cloud workspace."
                            .to_string(),
                    );
                    return true;
                }
                if self.maybe_prompt_cloud_launch(&raw_args) {
                    return true;
                }
                let args = self.cloud_command_args(raw_args.clone());
                let label = cloud_agent_label(&args);
                self.start_rudder_cli_command(&label, args);
                true
            }
            Some("/main") | Some("/m") => {
                // Everything after "/main " or "/m " is the user's prompt. If empty,
                // we use the default RUDDER.md bootstrap. The model must be
                // set ahead of time via /model.
                let trimmed = input.trim_start();
                let prompt = trimmed
                    .strip_prefix("/main")
                    .or_else(|| trimmed.strip_prefix("/m"))
                    .map(|rest| rest.trim().to_string())
                    .unwrap_or_default();
                self.handle_main_command(&prompt);
                true
            }
            Some("/usage") => {
                self.show_usage_summary();
                true
            }
            Some("/goal") => {
                let raw = input.trim_start_matches("/goal");
                self.forward_slash_command_to_focused_agent("/goal", raw);
                true
            }
            Some("/merge-all") => {
                self.request_merge_all_ready();
                true
            }
            Some("/automerge") => {
                self.auto_merge = !self.auto_merge;
                if self.auto_merge {
                    self.auto_merge_skip.clear();
                    self.notice = Some(
                        "auto-merge ON: clean finished nodes merge themselves and unblock children (conflicts still pause for you)".to_string(),
                    );
                    self.maybe_auto_merge();
                } else {
                    self.notice =
                        Some("auto-merge OFF: merge finished nodes yourself with m / M".to_string());
                }
                true
            }
            Some("/sync") => {
                // Retired: jj keeps node workspaces current automatically, so manual
                // worktree sync no longer fits the orchestrator paradigm.
                self.notice =
                    Some("sync is retired; jj keeps node workspaces current automatically".to_string());
                true
            }
            Some("/review-all") => {
                self.review_all_ready();
                true
            }
            _ => false,
        }
    }

    /// Start a new main agent.
    ///   /main or /m                  spawn with the default RUDDER.md bootstrap
    ///   /main <anything> or /m ...   spawn with <anything> as the first prompt
    /// Model is whatever the user has set via /model (or their CLI default);
    /// to change it, run /model first.
    fn handle_main_command(&mut self, prompt: &str) {
        let cwd = self.cwd.clone();
        let trimmed_prompt = prompt.trim();
        let run = create_main_agent(&cwd, self.backend, &self.model, self.effort, trimmed_prompt);
        let run_id = run.id.clone();
        self.agents.insert(0, run);
        self.selected_agent = 0;
        if let Some(run) = self.agents.first() {
            let _ = save_native_run_record(&cwd, run);
        }
        self.task_input.clear();
        self.task_cursor = 0;
        self.focus_or_spawn_main_with_prompt(trimmed_prompt);
        if !trimmed_prompt.is_empty() {
            spawn_task_summary_worker(
                self.task_summary_tx.clone(),
                run_id,
                trimmed_prompt.to_string(),
            );
        }
    }

    /// Show a one-line summary of token usage and estimated cost for the
    /// current repo, merged across Claude's session jsonls and Codex's
    /// session rollouts.
    fn show_usage_summary(&mut self) {
        let since = self.session_started_iso.clone();
        let claude = collect_claude_usage(&self.cwd, &since);
        let codex = collect_codex_usage(&self.cwd, &since);
        if claude.is_empty() && codex.is_empty() {
            self.notice = Some(
                "no usage data this rudder session yet (type a prompt to claude/codex first)"
                    .to_string(),
            );
            return;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut total_cost = 0.0_f64;
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut total_cache_creation = 0u64;
        let mut total_cache_read = 0u64;
        let render = |label: &str,
                      usage: &std::collections::BTreeMap<String, ModelUsage>,
                      parts: &mut Vec<String>,
                      total_cost: &mut f64,
                      total_in: &mut u64,
                      total_out: &mut u64,
                      total_cache_creation: &mut u64,
                      total_cache_read: &mut u64| {
            for (model, u) in usage {
                let cost = model_pricing(model)
                    .map(|(pi, po, pc, pr)| {
                        (u.input_tokens as f64) / 1e6 * pi
                            + (u.output_tokens as f64) / 1e6 * po
                            + (u.cache_creation_input_tokens as f64) / 1e6 * pc
                            + (u.cache_read_input_tokens as f64) / 1e6 * pr
                    })
                    .unwrap_or(0.0);
                *total_cost += cost;
                *total_in += u.input_tokens;
                *total_out += u.output_tokens;
                *total_cache_creation += u.cache_creation_input_tokens;
                *total_cache_read += u.cache_read_input_tokens;
                parts.push(format!(
                    "{label}/{}: {} in / {} out ~${:.2}",
                    short_model_label(model),
                    format_token_count(u.input_tokens),
                    format_token_count(u.output_tokens),
                    cost,
                ));
            }
        };
        render(
            "claude",
            &claude,
            &mut parts,
            &mut total_cost,
            &mut total_in,
            &mut total_out,
            &mut total_cache_creation,
            &mut total_cache_read,
        );
        render(
            "codex",
            &codex,
            &mut parts,
            &mut total_cost,
            &mut total_in,
            &mut total_out,
            &mut total_cache_creation,
            &mut total_cache_read,
        );
        parts.push(format!(
            "total: {} in / {} out · {} cache-create / {} cache-read · ~${:.2} (estimate)",
            format_token_count(total_in),
            format_token_count(total_out),
            format_token_count(total_cache_creation),
            format_token_count(total_cache_read),
            total_cost,
        ));
        self.notice = Some(parts.join("  ·  "));
    }

    /// Forward a slash command (e.g. "/goal foo") straight into the focused
    /// worker pane's PTY. Used for slash commands that the underlying agent
    /// (claude or codex) handles itself.
    fn forward_slash_command_to_focused_agent(&mut self, command: &str, rest: &str) {
        if self.agents.is_empty() {
            self.notice = Some(format!("no agent to receive {command}"));
            return;
        }
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            self.notice = Some(format!("no agent selected for {command}"));
            return;
        };
        let Some(terminal) = run.terminal.as_mut() else {
            self.notice = Some(format!(
                "selected agent is not running; cannot send {command}"
            ));
            return;
        };
        let trimmed_rest = rest.trim();
        let payload = if trimmed_rest.is_empty() {
            format!("{command}\r")
        } else {
            format!("{command} {trimmed_rest}\r")
        };
        if let Err(error) = terminal.write_input(payload.as_bytes()) {
            self.notice = Some(format!("{command}: {error}"));
            return;
        }
        self.task_input.clear();
        self.task_cursor = 0;
        self.focus = FocusPane::Worker;
        self.worker_view = WorkerView::Terminal;
    }

    fn maybe_prompt_cloud_launch(&mut self, raw_args: &[&str]) -> bool {
        if !cloud_args_start_worker(raw_args) {
            return false;
        }
        let scratch_args = self.cloud_command_args_with_fly(raw_args);
        let scratch_label = cloud_agent_label(&scratch_args);
        let selected_task = self
            .agents
            .get(self.selected_agent)
            .filter(|run| !is_cloud_agent(run))
            .map(|run| run.task_summary.clone());
        self.cloud_prompt = Some(CloudLaunchPrompt {
            scratch_args,
            scratch_label,
            selected_task,
            choice: CloudLaunchChoice::Upload,
        });
        self.notice = Some(
            "Cloud launch: Enter onloads this Rudder workspace; Down starts scratch in cloud"
                .to_string(),
        );
        true
    }

    fn handle_cloud_prompt_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut prompt) = self.cloud_prompt.take() else {
            return false;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.notice = Some("cloud launch cancelled".to_string());
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                prompt.choice = CloudLaunchChoice::Upload;
                self.cloud_prompt = Some(prompt);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                prompt.choice = CloudLaunchChoice::Scratch;
                self.cloud_prompt = Some(prompt);
                true
            }
            KeyCode::Char('n') => {
                prompt.choice = CloudLaunchChoice::Scratch;
                self.start_cloud_prompt_choice(prompt);
                true
            }
            KeyCode::Char('o') => {
                prompt.choice = CloudLaunchChoice::Upload;
                self.start_cloud_prompt_choice(prompt);
                true
            }
            KeyCode::Enter => {
                self.start_cloud_prompt_choice(prompt);
                true
            }
            _ => {
                self.cloud_prompt = Some(prompt);
                false
            }
        }
    }

    fn start_cloud_prompt_choice(&mut self, prompt: CloudLaunchPrompt) {
        match cloud_prompt_launch(&prompt) {
            Ok(launch) => {
                self.start_rudder_cli_command_with_env(
                    &launch.label,
                    launch.args,
                    &[("RUDDER_CLOUD_RUNTIME", "fly")],
                );
            }
            Err(message) => {
                self.notice = Some(message.to_string());
            }
        }
    }

    fn cloud_command_args(&self, args: Vec<&str>) -> Vec<String> {
        if args.is_empty() {
            return vec!["cloud".to_string(), random_cloud_name()];
        }
        if args[0] == "onload" && args.len() == 1 {
            if let Some(run) = self.agents.get(self.selected_agent) {
                return vec!["cloud".to_string(), "onload".to_string(), run.id.clone()];
            }
        }
        let known = [
            "help",
            "login",
            "list",
            "ls",
            "onload",
            "sail",
            "launch",
            "pause",
            "resume",
            "status",
            "stop",
            "logs",
            "vm",
            "byoc",
            "byo-vm",
            "bootstrap",
            "runtime",
            "setup",
            "byoc",
            "setup-byoc",
            "setup-vm",
            "setup-fly",
        ];
        let mut command = vec!["cloud".to_string()];
        if known.contains(&args[0]) {
            command.extend(args.into_iter().map(ToString::to_string));
        } else {
            command.extend(args.into_iter().map(ToString::to_string));
        }
        command
    }

    fn cloud_command_args_with_fly(&self, args: &[&str]) -> Vec<String> {
        self.cloud_command_args(args.to_vec())
    }

    fn start_rudder_cli_command(&mut self, label: &str, args: Vec<String>) {
        self.start_rudder_cli_command_with_env(label, args, &[]);
    }

    fn start_rudder_cli_command_with_env(
        &mut self,
        label: &str,
        args: Vec<String>,
        env_overrides: &[(&str, &str)],
    ) {
        let id = new_run_id(label);
        let mut command = TerminalCommand::with_args("rudder", args);
        for (key, value) in env_overrides {
            command = command.with_env(*key, *value);
        }
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        let created_at = now_stamp();
        let task = label.to_string();
        let mut run = AgentRun {
            id,
            created_at: created_at.clone(),
            mode: AgentMode::Execute,
            task: task.clone(),
            task_summary: summarize_task(&task),
            current_prompt: task.clone(),
            turns: vec![AgentTurn {
                ts: created_at.clone(),
                prompt: task.clone(),
                source: "user".to_string(),
            }],
            last_user_input_at: created_at,
            backend: self.backend,
            model: self.model.clone(),
            effort: self.effort,
            status: AgentStatus::Running,
            cwd: self.cwd.clone(),
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
            autosteered: true,
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
            ready_since: None,
            merge_resolver: false,
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                self.notice = Some(format!("opened {label}"));
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("{label} failed: {error}"));
            }
        }
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = FocusPane::Worker;
    }

    fn refresh_cloud_workspace_status(&mut self) {
        if env::var("RUDDER_OFFLINE")
            .ok()
            .is_some_and(|value| value == "1")
        {
            self.workspace_status_rx = None;
            return;
        }
        if !self.cloud_connected {
            self.cloud_workspace = None;
            self.workspace_status_rx = None;
            return;
        }
        if let Some(rx) = self.workspace_status_rx.take() {
            match rx.try_recv() {
                Ok(snapshot) => {
                    if self.cloud_workspace != snapshot {
                        self.cloud_workspace = snapshot;
                        self.dirty = true;
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.workspace_status_rx = Some(rx);
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    if self.cloud_workspace.take().is_some() {
                        self.dirty = true;
                    }
                }
            }
        }
        let due = match self.last_workspace_check {
            None => true,
            Some(at) => at.elapsed() >= Duration::from_secs(30),
        };
        if !due {
            return;
        }
        self.last_workspace_check = Some(Instant::now());
        let cwd = self.cwd.clone();
        let (tx, rx) = mpsc::channel();
        self.workspace_status_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(query_cloud_workspace_status(&cwd));
        });
    }

    fn maybe_notify_workspace_idle(&mut self) {
        let Some(workspace) = self.cloud_workspace.as_ref() else {
            self.workspace_idle_notified = false;
            return;
        };
        let running = workspace
            .status
            .as_deref()
            .is_some_and(|value| value == "running");
        if !running {
            self.workspace_idle_notified = false;
            return;
        }
        if workspace.client_count > 0 {
            self.workspace_idle_notified = false;
            return;
        }
        if self.last_user_activity.elapsed() < Duration::from_secs(30 * 60) {
            return;
        }
        if self.workspace_idle_notified {
            return;
        }
        self.workspace_idle_notified = true;
        self.notice = Some(
            "Cloud workspace idle. Run `rudder cloud workspace stop <id>` to shut it down."
                .to_string(),
        );
    }

    fn note_user_activity(&mut self) {
        self.last_user_activity = Instant::now();
        self.workspace_idle_notified = false;
    }

    /// Streaming plan detection: as soon as a parseable RUDDER_PLAN_TASKS block
    /// appears in any orchestrator's accumulated output, capture the plan into the
    /// approval gate WITHOUT waiting for the process to exit or the idle heuristic.
    /// This makes the orchestrator robust to plan mode blocking at an approval
    /// prompt (where the process never exits). Fires at most once per plan: the
    /// `autosteered` flag on the run gates re-entry, and we only act while no plan
    /// is already pending approval / queued.
    fn maybe_detect_plan_ready(&mut self) {
        // RECONCILE planners are detected FIRST and independently of the
        // initial-plan guard below: a reconcile planner runs WHILE a plan is
        // already active (planned_nodes non-empty, possibly awaiting approval), so
        // it must be captured from streaming output regardless of that state.
        // Plan mode may block the planner at an approval prompt without exiting, so
        // we route as soon as a parseable block appears in the live output.
        let reconcile_index = self.agents.iter().position(|run| {
            run.mode == AgentMode::RudderPlan
                && run.reconcile_planner
                && run.autosteered
                && extract_rudder_plan_tasks(&rudder_plan_output_for_run(run)).is_ok()
        });
        if let Some(index) = reconcile_index {
            self.evaluate_completed_reconcile(index);
            return;
        }

        // INITIAL plan: a fresh plan is already captured (awaiting approval, or
        // some nodes queued): do not re-capture from a still-streaming orchestrator.
        // EXCEPTION: while `refining`, the orchestrator has been relaunched to revise
        // the plan, so we MUST capture its new block even though a (stale) plan is
        // still pending; evaluate_completed_plan replaces the queue and clears the
        // flag.
        if !self.refining && (self.awaiting_approval || !self.planned_nodes.is_empty()) {
            return;
        }
        let index = self.agents.iter().position(|run| {
            run.mode == AgentMode::RudderPlan
                && !run.reconcile_planner
                && run.autosteered
                && extract_rudder_plan_tasks(&rudder_plan_output_for_run(run)).is_ok()
        });
        if let Some(index) = index {
            self.evaluate_completed_plan(index);
        }
    }

    /// Capture a completed (or streaming-detected) orchestrator plan into the
    /// APPROVAL gate. Each task becomes a PLANNED node queued in `planned_nodes`,
    /// visible in the Todo section, and `awaiting_approval` is set so NOTHING
    /// launches yet: the user reviews the DAG, removes a node (d), discards the
    /// plan (d on the orchestrator), or approves (Enter) to launch. The planner run
    /// is KEPT as the pinned orchestrator that owns the plan (its worker pane
    /// renders the DAG view); we clear `autosteered` so it is captured once.
    fn evaluate_completed_plan(&mut self, index: usize) {
        let Some(run) = self.agents.get_mut(index) else {
            return;
        };
        if run.mode != AgentMode::RudderPlan || !run.autosteered {
            return;
        }
        // The planner process just exited; its FINAL stdout (the rest of the human
        // summary and the authoritative `result` event) can still be buffered in the
        // PTY and not yet ingested, which truncated the captured summary mid-sentence.
        // Pull everything still buffered and ingest it before snapshotting. Cheap and
        // non-blocking; the orchestrator pane also re-extracts the summary live, so
        // this is immediacy on top of that safeguard.
        if let Some(terminal) = run.terminal.as_mut() {
            for _ in 0..64 {
                let had = !terminal.drain_output().is_empty();
                let snapshot = terminal.output_log_snapshot().to_string();
                if let Some(stream) = run.plan_stream.as_mut() {
                    stream.ingest(&snapshot);
                }
                if !had {
                    break;
                }
            }
        }
        let planner_task = run.task.clone();
        let output = rudder_plan_output_for_run(run);

        let tasks = match extract_rudder_plan_tasks(&output) {
            Ok(tasks) => tasks,
            Err(error) => {
                run.autosteered = false;
                let _ = save_native_run_record(&self.cwd, run);
                self.refining = false;
                self.notice = Some(format!(
                    "planner finished without a runnable plan ({error}). Refine the task with more detail and re-plan."
                ));
                return;
            }
        };
        // Capture the planner's prose after the block (assumptions / open questions)
        // so the orchestrator pane can show what it assumed and invite refinement.
        let summary = extract_rudder_plan_summary(&output);
        if tasks.is_empty() {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            self.refining = false;
            self.notice = Some(
                "planner finished without a runnable plan (it may have asked for clarification). Refine the task with more detail and re-plan."
                    .to_string(),
            );
            return;
        }

        // Queue every task as a PLANNED node. A new plan replaces any pending queue
        // (the planner is the single source of truth for the active plan).
        let nodes: Vec<PlannedNode> = tasks.iter().map(PlannedNode::from_task).collect();
        let count = nodes.len();
        self.planned_nodes = nodes;
        // Keep planned_origin anchored to the ORIGINAL request so refine rounds (whose
        // run.task is a composite "revise this" prompt) do not overwrite it; worker
        // launch prompts and further refinements stay tied to what the user asked for.
        self.planned_origin = if self.plan_request.trim().is_empty() {
            planner_task
        } else {
            self.plan_request.clone()
        };
        self.plan_summary = summary;
        // The revised plan has landed: the refine round is complete.
        self.refining = false;

        // Clear the planner's autosteer flag so it is captured once, but KEEP the
        // run: it stays pinned at the top of the list as the orchestrator that owns
        // the plan. Its worker pane renders the DAG tree of the parsed tasks.
        run.autosteered = false;
        let _ = save_native_run_record(&self.cwd, run);

        // APPROVAL GATE: do NOT launch. Hold the plan until the user approves so
        // they can review, discuss/refine (type in the task pane), or approve it.
        self.awaiting_approval = true;
        // Surface (never silently drop) when the planner emitted more tasks than the
        // runaway backstop allows.
        let emitted = rudder_plan_block_task_count(&output).unwrap_or(count);
        self.notice = Some(if emitted > MAX_PLAN_TASKS {
            format!(
                "plan ready: {count} of {emitted} node(s) (capped at {MAX_PLAN_TASKS}; add more by typing). Type to refine · Enter approve"
            )
        } else {
            format!("plan ready: {count} node(s). Type to refine · Enter approve")
        });
        self.dirty = true;
    }

    /// APPEND a reconcile planner's node(s) into the active plan (parallel to
    /// `evaluate_completed_plan`, but it pushes instead of replacing). Each parsed
    /// task becomes a PlannedNode whose id is UNIQUIFIED against the existing
    /// planned-node ids and agent node ids (so dep ids resolve), then PUSHED onto
    /// `planned_nodes`. FALLBACK: a node the model returned with no deps gets a SOFT
    /// edge to every current frontier id (mirrors src/scheduler.ts:901-911) so the
    /// added work is aware of the in-flight work but never deadlocks. The reconcile
    /// planner agent is then removed. If the session is already approved/running
    /// (not awaiting approval), the scheduler runs so the new node launches when its
    /// deps are met; if the initial plan is still awaiting approval, the node is
    /// left queued so it becomes part of the plan the user approves.
    fn evaluate_completed_reconcile(&mut self, index: usize) {
        // Read + validate the planner run inside a scoped mutable borrow, then drop
        // it before touching other `self` state (frontier, uniquify, append). On a
        // parse failure the captured-once flag is cleared and we bail.
        let (planner_task, output) = {
            let Some(run) = self.agents.get_mut(index) else {
                return;
            };
            if run.mode != AgentMode::RudderPlan || !run.reconcile_planner || !run.autosteered {
                return;
            }
            // Clear the captured-once flag now so a re-poll of the same Done planner
            // is a no-op even if it lingers a moment before removal.
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            (run.task.clone(), rudder_plan_output_for_run(run))
        };

        // The frontier the new node(s) reconcile against: existing queued nodes plus
        // not-yet-merged plan-launched agents. The reconcile planner carries no node
        // id, so it never appears in the frontier. Feeds BOTH the dep parser (so
        // cross-block deps on these ids survive) and the no-deps soft fallback.
        let frontier: Vec<String> = self
            .plan_frontier()
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let tasks = match extract_rudder_plan_tasks_with_frontier(&output, &frontier) {
            Ok(tasks) => tasks,
            Err(error) => {
                self.notice =
                    Some(format!("added task did not produce a runnable node: {error}"));
                return;
            }
        };
        if tasks.is_empty() {
            self.notice = Some("added task produced no runnable node".to_string());
            return;
        }

        let mut appended = 0usize;
        for task in &tasks {
            let mut node = PlannedNode::from_task(task);
            // UNIQUIFY against every id already known to the plan (queued nodes +
            // launched agent node ids) so the appended node's id never collides and
            // its dep ids continue to resolve against the real frontier.
            node.id = self.uniquify_node_id(&node.id);

            // FALLBACK: the model returned no dependencies. Attach a SOFT edge to
            // every current frontier id so the added work is aware of in-flight
            // work, but it can never deadlock (soft edges never gate launch).
            if node.deps.is_empty() && node.soft_deps.is_empty() {
                node.soft_deps = frontier.clone();
            }

            self.planned_nodes.push(node);
            appended += 1;
        }

        // Keep the planned-origin populated so worker prompts have an origin even
        // when reconcile happens before any initial plan was captured.
        if self.planned_origin.trim().is_empty() {
            self.planned_origin = planner_task;
        }

        // The reconcile planner has done its job; remove it so it does not linger as
        // a second pinned orchestrator. (The initial planner is KEPT as the
        // orchestrator; a reconcile planner is transient.)
        // If the user was watching the (transient) reconcile planner, return them to
        // the orchestrator afterwards so they SEE the new node land in the DAG. If
        // they navigated elsewhere, leave their selection alone.
        let was_watching_reconcile = self.selected_agent == index;
        self.agents.remove(index);
        if was_watching_reconcile {
            if let Some(orch_index) = self.agents.iter().position(|run| run.is_orchestrator()) {
                self.selected_agent = orch_index;
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
            }
        }
        if self.selected_agent >= self.agents.len() {
            self.selected_agent = self.agents.len().saturating_sub(1);
        }

        // If the session is already approved/running, schedule so the new node
        // launches as soon as its deps are met. If the initial plan is still
        // awaiting approval, leave the node QUEUED: it joins the plan the user
        // approves at the gate.
        if self.awaiting_approval {
            self.notice = Some(format!(
                "added {appended} node(s) to the plan awaiting approval"
            ));
        } else {
            self.notice = Some(format!("added {appended} node(s) to the plan"));
            self.run_scheduler();
        }
        // MIRROR the appended node(s) into graph.json. When awaiting approval the
        // node was only queued (run_scheduler did not run), so mirror here too so
        // the board reflects the reconcile. Coalesced + non-fatal.
        self.mirror_graph();
        self.dirty = true;
    }

    /// Append a one-line entry to the conductor activity log (bounded) and surface it
    /// as the current notice. This is how every AUTONOMOUS action stays visible
    /// without a confirm gate.
    fn push_activity(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.notice = Some(msg.clone());
        self.activity_log.push(msg);
        const MAX_ACTIVITY: usize = 200;
        if self.activity_log.len() > MAX_ACTIVITY {
            let overflow = self.activity_log.len() - MAX_ACTIVITY;
            self.activity_log.drain(0..overflow);
        }
        self.dirty = true;
    }

    /// Live plan size = queued nodes + launched-but-not-merged plan agents. The
    /// auto-expansion backstop guards this against MAX_PLAN_TASKS.
    fn plan_node_count(&self) -> usize {
        self.planned_nodes.len()
            + self
                .agents
                .iter()
                .filter(|run| run.node_id.is_some() && run.status != AgentStatus::Merged)
                .count()
    }

    /// True if a node with a title that normalizes to `title` already exists (queued
    /// or launched), so an auto-expanded follow-up does not duplicate existing work.
    fn followup_title_exists(&self, title: &str) -> bool {
        let norm = |s: &str| s.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        let want = norm(title);
        if want.is_empty() {
            return true;
        }
        self.planned_nodes.iter().any(|n| norm(&n.title) == want)
            || self.agents.iter().any(|r| {
                r.node_id.is_some() && (norm(&r.task_summary) == want || norm(&r.task) == want)
            })
    }

    /// Scan finished plan workers and GROW the DAG from each one's `rudder done`
    /// report exactly once. Autonomous (no confirm); surfaced via the activity log.
    fn maybe_ingest_worker_followups(&mut self) {
        let candidates: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.node_id.is_some()
                    && matches!(run.mode, AgentMode::Execute)
                    && run.status == AgentStatus::Done
                    && !run.merge_resolver
                    && !self.followups_ingested.contains(&run.id)
            })
            .map(|(index, _)| index)
            .collect();
        let mut grew = false;
        for index in candidates {
            if self.ingest_worker_followups(index) {
                grew = true;
            }
        }
        if grew && !self.awaiting_approval {
            self.run_scheduler();
            self.mirror_graph();
        }
    }

    /// Parse the finishing worker's RUDDER_DONE block and append its in-scope
    /// follow-ups as new planned nodes (deduped, depth- and cap-guarded). Returns
    /// true if any node was added. Idempotent: the run id is marked ingested up front.
    fn ingest_worker_followups(&mut self, index: usize) -> bool {
        let (run_id, node_id, output) = {
            let Some(run) = self.agents.get(index) else {
                return false;
            };
            (
                run.id.clone(),
                run.node_id.clone().unwrap_or_default(),
                run.terminal
                    .as_ref()
                    .map(|t| t.output_log_snapshot().to_string())
                    .unwrap_or_default(),
            )
        };
        self.followups_ingested.insert(run_id);

        let Some(note) = parse_worker_done_block(&output) else {
            return false;
        };
        self.apply_worker_followups(&node_id, &note)
    }

    /// Grow the DAG from a parsed completion note: dedupe, depth- and cap-guard,
    /// infer deps, and push the in-scope follow-ups as planned nodes. Pure of any
    /// PTY/terminal access (split out of ingest_worker_followups so it is testable).
    /// Returns true if any node was added.
    fn apply_worker_followups(&mut self, node_id: &str, note: &serde_json::Value) -> bool {
        let followups = note
            .get("followups")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if followups.is_empty() {
            return false;
        }
        // Depth guard: a node that is itself deep in an auto-expansion chain does not
        // get to spawn more; record the intent instead of growing without bound.
        let gen = self.followup_gen.get(node_id).copied().unwrap_or(0);
        if gen >= MAX_FOLLOWUP_DEPTH {
            self.push_activity(format!(
                "follow-ups from {node_id} not expanded (depth cap {MAX_FOLLOWUP_DEPTH}); recorded to DECISIONS.md"
            ));
            return false;
        }
        let frontier: Vec<String> = self.plan_frontier().into_iter().map(|(id, _)| id).collect();
        let mut added = 0usize;
        for f in &followups {
            if f.get("scope").and_then(serde_json::Value::as_str) == Some("out") {
                continue; // out-of-lane: recorded in DECISIONS.md, not auto-injected
            }
            let title = f
                .get("title")
                .or_else(|| f.get("prompt"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(title) = title else { continue };
            if self.followup_title_exists(title) {
                continue;
            }
            if self.plan_node_count() >= MAX_PLAN_TASKS {
                self.push_activity(format!(
                    "plan at {MAX_PLAN_TASKS}-node cap; remaining follow-ups from {node_id} recorded, not launched"
                ));
                break;
            }
            let prompt = f
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(title)
                .to_string();
            let explicit: Vec<String> = f
                .get("deps")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut node = PlannedNode {
                id: self.uniquify_node_id("followup"),
                title: title.to_string(),
                prompt,
                goal: Some(title.to_string()),
                success: None,
                deps: Vec::new(),
                soft_deps: Vec::new(),
                backend: None,
                model: None,
                effort: None,
            };
            if explicit.is_empty() {
                // Default: a SOFT edge to the finishing node + the frontier, so the
                // new work is aware of in-flight work but never gates/deadlocks.
                let mut soft = vec![node_id.to_string()];
                soft.extend(frontier.iter().cloned());
                soft.retain(|id| !id.is_empty());
                soft.sort();
                soft.dedup();
                node.soft_deps = soft;
            } else {
                // The agent said this follow-up consumes those nodes: treat as hard.
                node.deps = explicit;
            }
            self.followup_gen.insert(node.id.clone(), gen + 1);
            self.planned_nodes.push(node);
            added += 1;
        }
        if added > 0 {
            self.push_activity(format!("grew {added} node(s) from {node_id}'s completion"));
        }
        added > 0
    }

    /// Make `id` unique against every node id already known to the active plan:
    /// queued planned-node ids plus the node ids of launched agents. Appends a
    /// numeric suffix (`-2`, `-3`, ...) until no collision remains so an appended
    /// node never shadows an existing node and its dep references still resolve.
    fn uniquify_node_id(&self, id: &str) -> String {
        let taken = |candidate: &str| -> bool {
            self.planned_nodes.iter().any(|node| node.id == candidate)
                || self
                    .agents
                    .iter()
                    .any(|run| run.node_id.as_deref() == Some(candidate))
        };
        if !taken(id) {
            return id.to_string();
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{id}-{suffix}");
            if !taken(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// APPROVE the pending plan: clear the approval gate and drain the queue into
    /// live workers. Ready nodes (hard deps satisfied, slot free) launch on this
    /// immediate scheduler pass; the rest stay in Todo until their deps merge.
    fn approve_planned_queue(&mut self) {
        if !self.awaiting_approval {
            return;
        }
        // A refine is in flight: the revised DAG is still being produced. Do NOT
        // approve/launch the stale plan; tell the user to wait for the update.
        if self.refining {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
        if self.planned_nodes.is_empty() {
            self.awaiting_approval = false;
            return;
        }
        self.awaiting_approval = false;
        self.notice = Some("plan approved".to_string());
        // Drain immediately so a ready node moves todo->in progress without waiting
        // a full scheduler interval (covers the trivial 1-node case visibly).
        self.run_scheduler();
        self.dirty = true;
    }

    /// Remove a single planned node from the queue (FIX the plan before approval).
    /// Returns true when a node was removed. Dropping a parent leaves its children
    /// as roots (their dep id is simply absent from the plan, treated as satisfied).
    fn remove_planned_node(&mut self, node_id: &str) -> bool {
        let Some(position) = self
            .planned_nodes
            .iter()
            .position(|node| node.id == node_id)
        else {
            return false;
        };
        let node = self.planned_nodes.remove(position);
        self.notice = Some(format!("removed planned node {}", node.title));
        if self.planned_nodes.is_empty() {
            self.awaiting_approval = false;
            self.planned_origin.clear();
        }
        self.dirty = true;
        true
    }

    /// Node ids of agents that have reached Merged. These satisfy hard deps and
    /// unblock dependents on the next scheduler pass.
    fn merged_node_ids(&self) -> Vec<String> {
        self.agents
            .iter()
            .filter(|run| run.status == AgentStatus::Merged)
            .filter_map(|run| run.node_id.clone())
            .collect()
    }

    /// Count of currently-running agents that were launched from a planned node.
    /// This is the figure the parallelism cap (MAX_PARALLEL) limits.
    fn running_plan_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|run| run.node_id.is_some() && run.status == AgentStatus::Running)
            .count()
    }

    /// Build the JSON payload that MIRRORS the current plan into graph.json. The
    /// TUI owns no graph.json schema: it just describes its plan and the TS shim
    /// (`rudder __graph-mirror`) projects it. The payload contains:
    ///   - every queued `planned_node` (status "planned", with its hard + soft deps)
    ///   - every plan-launched agent (those carrying a `node_id`), status mapped
    ///     from AgentStatus (Running->running, Done->review, Merged->merged,
    ///     Failed/Stopped->failed), carrying its runId, jjChangeId, worktree path,
    ///     and its deps/soft_deps.
    /// Pure (no IO) so the builder can be unit-tested without a shell-out.
    fn build_mirror_payload(&self) -> serde_json::Value {
        let mut nodes: Vec<serde_json::Value> = Vec::new();

        // Queued planned nodes: not yet launched, status "planned".
        for node in &self.planned_nodes {
            let mut deps: Vec<serde_json::Value> = node
                .deps
                .iter()
                .map(|on| serde_json::json!({ "on": on, "type": "hard" }))
                .collect();
            deps.extend(
                node.soft_deps
                    .iter()
                    .map(|on| serde_json::json!({ "on": on, "type": "soft" })),
            );
            let mut value = serde_json::json!({
                "id": node.id,
                "title": node.title,
                "prompt": node.prompt,
                "status": "planned",
                "deps": deps,
            });
            if let Some(backend) = &node.backend {
                value["backend"] = serde_json::json!(backend);
            }
            if let Some(model) = &node.model {
                value["model"] = serde_json::json!(model);
            }
            if let Some(effort) = &node.effort {
                value["effort"] = serde_json::json!(effort);
            }
            nodes.push(value);
        }

        // Plan-launched agents: those carrying a node_id, projected to a node
        // keyed by that node_id. AgentStatus maps onto the board's NodeStatus.
        for run in &self.agents {
            let Some(node_id) = run.node_id.as_ref() else {
                continue;
            };
            let status = match run.status {
                AgentStatus::Running => "running",
                AgentStatus::Done => "review",
                AgentStatus::Merged => "merged",
                AgentStatus::Failed | AgentStatus::Stopped => "failed",
            };
            let mut deps: Vec<serde_json::Value> = run
                .deps
                .iter()
                .map(|on| serde_json::json!({ "on": on, "type": "hard" }))
                .collect();
            deps.extend(
                run.soft_deps
                    .iter()
                    .map(|on| serde_json::json!({ "on": on, "type": "soft" })),
            );
            let title = if run.task_summary.trim().is_empty() {
                run.task.clone()
            } else {
                run.task_summary.clone()
            };
            // Carry the node's actual prompt so graph.json never shows a stale prompt
            // from an earlier plan that reused this node id. Prefer the launched
            // prompt; fall back to the title.
            let prompt = if run.current_prompt.trim().is_empty() {
                title.clone()
            } else {
                run.current_prompt.clone()
            };
            let mut value = serde_json::json!({
                "id": node_id,
                "title": title,
                "prompt": prompt,
                "status": status,
                "runId": run.id,
                "backend": run.backend.as_str(),
                "deps": deps,
            });
            value["model"] = serde_json::json!(run.model);
            if let Some(effort) = run.effort {
                value["effort"] = serde_json::json!(effort.as_str());
            }
            if let Some(change) = &run.jj_change_id {
                value["jjChangeId"] = serde_json::json!(change);
            }
            if let Some(path) = &run.worktree_path {
                value["worktreePath"] = serde_json::json!(path.to_string_lossy());
            }
            nodes.push(value);
        }

        serde_json::json!({ "nodes": nodes })
    }

    /// MIRROR the current plan into graph.json so the web board shows this TUI
    /// session's DAG. Coalesced: a stable signature of the payload is hashed and
    /// the shell-out is skipped when nothing changed since the last mirror (so a
    /// burst of poll ticks is one write at most). Best-effort + NON-FATAL: the TS
    /// shim is shelled synchronously and any failure (missing CLI, non-zero exit,
    /// spawn error) is swallowed - mirroring must never block or break the TUI.
    fn mirror_graph(&mut self) {
        let payload = self.build_mirror_payload();
        // Coalesce: hash the serialized payload; skip when unchanged. The payload
        // intentionally excludes volatile fields (no timestamps), so the signature
        // only changes when the plan/agent set or a status actually changes.
        let serialized = payload.to_string();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serialized.hash(&mut hasher);
        let signature = hasher.finish();
        if self.last_mirror_signature == Some(signature) {
            return;
        }
        self.last_mirror_signature = Some(signature);

        // Nothing to mirror and we have never mirrored a non-empty plan: skip the
        // shell-out entirely (avoids spawning a CLI just to write an empty graph).
        // We still recorded the signature above so we do not re-check every tick.

        #[cfg(test)]
        {
            // Tests exercise build_mirror_payload + the coalesce guard directly;
            // never shell out from a test run.
            let _ = serialized;
        }

        #[cfg(not(test))]
        {
            let Some(rudder) = locate_rudder_cli() else {
                return;
            };
            let repo = self.cwd.clone();
            let child = Command::new(&rudder)
                .arg("__graph-mirror")
                .arg("--repo")
                .arg(&repo)
                .current_dir(&repo)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                return;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(serialized.as_bytes());
                // Drop stdin to signal EOF so the shim's stdin read completes.
            }
            // Reap so we do not leave a zombie; ignore the outcome (best-effort).
            let _ = child.wait();
        }
    }

    /// Scheduler step: drain ready planned nodes into live agents while a slot is
    /// free. A node is ready when all its hard deps are merged (or reference ids
    /// absent from the plan, treated as satisfied so the DAG never deadlocks).
    /// Soft deps never gate. Runs on a coarse tick and after a plan is queued.
    /// When auto-merge is on, merge every plan node that has finished cleanly (in
    /// review, not main, not already skipped for a conflict) so its hard children
    /// unblock. A node that conflicts is recorded in `auto_merge_skip` and left for
    /// the user (press m to resolve + merge); auto-merge stops at the first conflict
    /// this pass to avoid cascading on a broken base. After merging, drain the
    /// scheduler so newly-ready children launch.
    fn maybe_auto_merge(&mut self) {
        if !self.auto_merge {
            return;
        }
        let mut merged_any = false;
        loop {
            let next = self.agents.iter().position(|run| {
                run.node_id.is_some()
                    && run.status == AgentStatus::Done
                    && !run.is_main()
                    && !run.merge_resolver
                    && !self.auto_merge_skip.contains(&run.id)
            });
            let Some(index) = next else { break };
            let id = self.agents[index].id.clone();
            let label = self.agents[index].task_summary.clone();
            let task = self.agents[index].task.clone();
            match self.merge_agent_at(index) {
                Ok(()) => merged_any = true,
                Err(error) => {
                    // Conflict: auto-spawn the AI resolver to integrate both sides in
                    // the integration workspace. finalize_merge_resolvers flips the
                    // node to Merged (unblocking children) once it finishes clean.
                    // Clone (don't consume) the recorded conflict so it survives a
                    // resolver spawn failure for any later manual recovery.
                    let files = self.pending_jj_conflict.clone().unwrap_or_default();
                    self.conflict_prompt = Some(MergeConflictPrompt {
                        operation: ConflictOperation::Merge,
                        task,
                        conflicted_files: files,
                        error: error.to_string(),
                        repo_root: self.cwd.clone(),
                        target_branch: None,
                        source_branch: None,
                        worktree_path: None,
                        agent_id: Some(id.clone()),
                    });
                    self.start_conflict_resolution_agent();
                    let resolver_running = self
                        .agents
                        .iter()
                        .any(|r| r.id == id && r.merge_resolver);
                    if resolver_running {
                        self.notice = Some(format!(
                            "conflict in {} — AI resolver integrating it",
                            short_task(&label)
                        ));
                    } else {
                        // Resolver did not start; do not retry every tick.
                        self.auto_merge_skip.push(id);
                    }
                    break; // one conflict at a time
                }
            }
        }
        if merged_any {
            self.run_scheduler();
            self.mirror_graph();
            self.dirty = true;
        }
    }

    /// Finalize any AI merge-conflict resolver that has finished its turn. If no jj
    /// conflicts remain in its workspace the integration succeeded: flip the node to
    /// Merged (its hard children unblock) and drain the scheduler. If conflicts
    /// remain, drop it back to manual (clear the resolver flag + notify) so the user
    /// can finish. Runs for both auto-spawned and manually-started (y) resolvers.
    fn finalize_merge_resolvers(&mut self) {
        // Iterate by agent id, not index: marking one merged can shift the agents
        // vector, so a cached index could point at the wrong row (or out of bounds).
        let done_ids: Vec<String> = self
            .agents
            .iter()
            .filter(|run| run.merge_resolver && run.status == AgentStatus::Done)
            .map(|run| run.id.clone())
            .collect();
        if done_ids.is_empty() {
            return;
        }
        let mut finalized_any = false;
        for id in done_ids {
            let Some(index) = self.agents.iter().position(|run| run.id == id) else {
                continue;
            };
            let cwd = self.agents[index].cwd.clone();
            let label = self.agents[index].task_summary.clone();
            // `jj resolve --list` reads jj's COMMITTED state, and the resolver agent's
            // turn only ends after its tool calls return, so an empty list here means
            // the conflict is genuinely resolved in the workspace (not mid-flight).
            let conflicts = jj_unresolved_conflicts(&cwd);
            if let Some(run) = self.agents.get_mut(index) {
                run.merge_resolver = false;
            }
            if conflicts.is_empty() {
                let sources = self.agents[index].review_source_ids.clone();
                if !self.agents[index].is_main() {
                    self.mark_agent_and_review_sources_merged(index, sources);
                }
                finalized_any = true;
                self.notice = Some(format!("AI resolved & merged {}", short_task(&label)));
            } else {
                self.notice = Some(format!(
                    "resolver left {} conflict(s) in {}; resolve and press m",
                    conflicts.len(),
                    short_task(&label)
                ));
            }
        }
        if finalized_any {
            self.run_scheduler();
            self.mirror_graph();
            self.dirty = true;
        }
    }

    fn run_scheduler(&mut self) {
        if self.planned_nodes.is_empty() {
            return;
        }
        let cap = max_parallel();
        let planner_task = self.planned_origin.clone();
        let mut launched = 0usize;

        // Launch at most MAX_LAUNCH_PER_TICK nodes per pass. Each launch shells a
        // synchronous jj-workspace setup that briefly blocks the UI thread, so
        // draining a whole wave of newly-ready nodes at once (e.g. when a parent
        // merge unblocks several children) would freeze the TUI. The periodic
        // scheduler tick and merge transitions keep calling run_scheduler, so the
        // remaining nodes launch progressively and the UI stays responsive. The
        // launch decision is recomputed each iteration so a node launched this pass
        // (now Running) updates the cap accounting before the next pick.
        while launched < MAX_LAUNCH_PER_TICK {
            let Some(position) = self.next_node_to_launch(cap) else {
                break;
            };
            let node = self.planned_nodes.remove(position);
            let title = node.title.clone();
            let prompt = planned_node_worker_prompt(&planner_task, &node);
            self.start_execute_task_node(&prompt, Some(&title), Some(node));
            launched += 1;
        }

        if launched > 0 {
            let remaining = self.planned_nodes.len();
            self.notice = Some(if remaining > 0 {
                format!("launched {launched} node(s), {remaining} waiting in todo")
            } else {
                format!("launched {launched} node(s)")
            });
            self.dirty = true;
        }

        // MIRROR the plan into graph.json so the board reflects this DAG. Covers
        // both the just-launched nodes (now Running agents) and the queue that
        // remains in Todo. Coalesced + non-fatal inside mirror_graph.
        self.mirror_graph();
    }

    /// Position in `planned_nodes` of the next node to launch, or `None` when the
    /// cap is reached or no queued node is ready. Pure decision (no side effects)
    /// so the scheduler's dep-gating + cap can be tested without spawning PTYs.
    fn next_node_to_launch(&self, cap: usize) -> Option<usize> {
        if self.running_plan_agents() >= cap {
            return None;
        }
        let merged = self.merged_node_ids();
        let plan_ids = self.known_plan_node_ids();
        self.planned_nodes
            .iter()
            .position(|node| node.is_ready(&merged, &plan_ids))
    }

    /// Every node id that belongs to the active plan: ids still queued in
    /// `planned_nodes` PLUS ids of agents already launched from a node. A dep id
    /// outside this set was never part of the plan and is treated as satisfied so
    /// the DAG cannot deadlock; a dep still inside it must merge before unblocking.
    fn known_plan_node_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .planned_nodes
            .iter()
            .map(|node| node.id.clone())
            .collect();
        ids.extend(self.agents.iter().filter_map(|run| run.node_id.clone()));
        ids
    }

    /// Discard the entire pending plan queue. No longer bound to a key (the plan is
    /// refined by typing into the task pane, not discarded), but kept for now.
    #[allow(dead_code)]
    fn discard_planned_queue(&mut self) {
        if self.planned_nodes.is_empty() {
            return;
        }
        let count = self.planned_nodes.len();
        self.planned_nodes.clear();
        self.planned_origin.clear();
        self.awaiting_approval = false;
        self.notice = Some(format!("discarded {count} planned node(s)"));
        self.dirty = true;
    }

    fn selected_terminal_mut(&mut self) -> Option<&mut TerminalPane> {
        self.agents
            .get_mut(self.selected_agent)
            .and_then(|run| run.terminal.as_mut())
    }

    fn selected_review_terminal_mut(&mut self) -> Option<&mut TerminalPane> {
        self.agents
            .get_mut(self.selected_agent)
            .and_then(|run| run.review_terminal.as_mut())
    }

    fn set_selected_error(&mut self, message: String) {
        // A write that fails because the agent process ALREADY exited cleanly is
        // not a run failure — the process simply finished (common for the
        // non-interactive `claude -p` orchestrator, which prints its plan and
        // exits; a stray scroll/keystroke into the dead pane would otherwise turn
        // the whole pane red with "agent process exited (exit 0)"). Treat that as
        // completion so a finished planner still gets its plan evaluated, and show
        // a gentle notice instead of a hard error.
        let mut clean_exit = false;
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            let exited_clean = run
                .terminal
                .as_mut()
                .and_then(|terminal| terminal.try_wait().ok().flatten())
                .is_some_and(|status| status.success())
                || message.contains("(exit 0)");
            if exited_clean {
                mark_run_done(run);
                clean_exit = true;
            } else {
                run.status = AgentStatus::Failed;
                run.last_error = Some(message);
            }
        }
        if clean_exit {
            self.notice = Some("agent already finished".to_string());
        }
    }

    fn set_selected_review_error(&mut self, message: String) {
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            run.review_error = Some(message);
        }
    }

    fn ensure_review_diff(&mut self) {
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            return;
        };
        if run.review_terminal.is_some() {
            return;
        }

        #[cfg(test)]
        {
            run.review_error = None;
            self.notice = Some("opening review".to_string());
            return;
        }

        #[cfg(not(test))]
        {
            // The review pane is jj's own diff, watched live: `jj status` (the
            // working-copy summary) + `jj diff` (jj's default diff program), redrawn
            // every 2s so edits the agent makes show up. The agent works in a jj
            // workspace, so this is the faithful view of its changes. Falls back to
            // `git diff` only if jj is somehow unavailable.
            //
            // CRITICAL: pipe jj through `cat` (and use git `--no-pager`). The pane is
            // a real PTY, so jj/git see a tty on stdout and would launch a PAGER on a
            // long diff, blocking the watch loop forever. Piping to cat makes stdout a
            // pipe, so jj's auto-pager stays off and the loop keeps refreshing.
            let command = TerminalCommand::with_args(
                "sh",
                [
                    "-lc",
                    "if command -v jj >/dev/null 2>&1; then while :; do printf '\\033[2J\\033[H'; jj --color=always status 2>&1 | cat; printf '\\n'; jj --color=always diff 2>&1 | cat; sleep 2; done; else while :; do printf '\\033[2J\\033[H'; git --no-pager status --short 2>&1; printf '\\n'; git --no-pager diff --color=always HEAD 2>&1; sleep 2; done; fi",
                ],
            );
            let options = TerminalPaneOptions {
                size: run.terminal_size.unwrap_or_default(),
                cwd: Some(run.cwd.clone()),
                ..TerminalPaneOptions::default()
            };

            match TerminalPane::spawn_shell_or_command(Some(command), options) {
                Ok(mut terminal) => {
                    let _ = terminal.drain_output();
                    run.review_terminal = Some(terminal);
                    run.review_error = None;
                    self.notice = Some("opening review".to_string());
                }
                Err(error) => {
                    run.review_error = Some(error.to_string());
                    self.notice = Some(format!("failed to open diff: {error}"));
                }
            }
        }
    }

    fn delete_selected_agent(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        if self.selected_is_main() {
            self.notice = Some("main agent: delete disabled".to_string());
            return;
        }
        let selected = &self.agents[self.selected_agent];
        if self.delete_pending.as_deref() != Some(&selected.id) {
            self.delete_pending = Some(selected.id.clone());
            self.notice = Some(if selected.worktree_path.is_some() {
                "press d again to delete agent and remove its worktree".to_string()
            } else {
                "press d again to delete agent".to_string()
            });
            return;
        }

        let run = self.agents.remove(self.selected_agent);
        let _ = remove_native_run_record(&self.cwd, &run.id);
        let worktree_error = run.worktree_path.as_ref().and_then(|path| {
            let output = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(path)
                .current_dir(&self.cwd)
                .output()
                .ok()?;
            if output.status.success() {
                None
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Some(if stderr.is_empty() {
                    "failed to remove worktree".to_string()
                } else {
                    format!("failed to remove worktree: {stderr}")
                })
            }
        });
        let last = self.agents.len().saturating_sub(1);
        self.selected_agent = self.selected_agent.min(last);
        if !self.agents.is_empty() {
            let visible = self.visible_agent_indices();
            if let Some(index) = visible
                .iter()
                .copied()
                .find(|&index| index >= self.selected_agent)
                .or_else(|| visible.last().copied())
            {
                self.selected_agent = index;
            }
        }
        self.delete_pending = None;
        self.notice = Some(worktree_error.unwrap_or_else(|| {
            if run.worktree_path.is_some() {
                "deleted agent and removed worktree".to_string()
            } else {
                "deleted agent from dashboard".to_string()
            }
        }));
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
    }

    /// Remove the agent at `index` from the dashboard and its on-disk run record,
    /// then re-anchor `selected_agent` onto the nearest visible row. Used to abandon
    /// a gated plan's planner run (which has no worktree to clean up).
    // Retained as a general utility (the orchestrator is no longer removed on plan
    // completion, which was its only caller). Kept for future deletion paths.
    #[allow(dead_code)]
    fn remove_agent_at(&mut self, index: usize) {
        if index >= self.agents.len() {
            return;
        }
        let run = self.agents.remove(index);
        let _ = remove_native_run_record(&self.cwd, &run.id);
        let last = self.agents.len().saturating_sub(1);
        self.selected_agent = self.selected_agent.min(last);
        if !self.agents.is_empty() {
            let visible = self.visible_agent_indices();
            if let Some(target) = visible
                .iter()
                .copied()
                .find(|&visible_index| visible_index >= self.selected_agent)
                .or_else(|| visible.last().copied())
            {
                self.selected_agent = target;
            }
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
    }

    // Retired from the UI (no keybinding, /sync is a no-op); kept for now.
    #[allow(dead_code)]
    fn request_sync_selected_agent(&mut self) {
        if self.selected_is_main() {
            self.notice = Some("main agent: sync disabled".to_string());
            return;
        }
        let Some(run) = self.agents.get(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        if run.status == AgentStatus::Running {
            self.notice = Some("selected agent is still running".to_string());
            return;
        }
        if run.status == AgentStatus::Merged {
            self.notice = Some("selected agent is already merged".to_string());
            return;
        }
        // A run is isolated (and thus syncable) when it has a workspace path,
        // whether that is a jj workspace (new runs) or a git worktree (legacy).
        if run.worktree_path.is_none() && run.worktree_branch.is_none() {
            self.notice = Some("selected agent has no workspace to sync".to_string());
            return;
        }
        let task = run.task.clone();
        let source_branch = run.worktree_branch.clone();
        let worktree_path = run.worktree_path.clone();
        let agent_id = Some(run.id.clone());
        match self.sync_agent_at(self.selected_agent) {
            Ok(()) => {
                self.notice = Some("synced selected worktree".to_string());
            }
            Err(error) => self.handle_merge_error(
                task,
                error,
                None,
                source_branch,
                worktree_path,
                agent_id,
            ),
        }
    }

    fn request_merge_selected_agent(&mut self) {
        if self.selected_is_main() {
            self.notice = Some("main agent: merge disabled".to_string());
            return;
        }
        let Some(run) = self.agents.get(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        if run.status == AgentStatus::Merged {
            self.notice = Some("selected agent is already merged".to_string());
            return;
        }
        // A run is mergeable when it has a workspace path (jj workspace for new
        // runs, git worktree for legacy runs) or a legacy git branch.
        if run.worktree_path.is_none() && run.worktree_branch.is_none() {
            self.notice = Some("selected agent has no workspace to merge".to_string());
            return;
        }
        let pending = run
            .worktree_path
            .as_ref()
            .map(|p| count_uncommitted_changes(p))
            .unwrap_or(0);
        let summary = if run.task_summary.trim().is_empty() {
            short_task(&run.task)
        } else {
            run.task_summary.trim().to_string()
        };
        self.delete_pending = None;
        self.merge_confirm = Some(MergeConfirmation {
            intent: MergeIntent::Selected {
                id: run.id.clone(),
                task: run.task.clone(),
            },
        });
        self.conflict_prompt = None;
        let pending_suffix = if pending > 0 {
            format!(
                " ({pending} uncommitted file{plural} will be auto-committed as \"{summary}\")",
                plural = if pending == 1 { "" } else { "s" },
                summary = truncate_chars(&summary, 48),
            )
        } else {
            String::new()
        };
        self.notice = Some(format!(
            "merge {summary}? press y to confirm or n to cancel{pending_suffix}",
            summary = truncate_chars(&summary, 48),
        ));
    }

    fn request_merge_all_ready(&mut self) {
        let claimed = self.review_all_claimed_source_ids();
        let ready_runs: Vec<&AgentRun> = self
            .agents
            .iter()
            .filter(|run| {
                run.status == AgentStatus::Done
                    && (run.worktree_path.is_some() || run.worktree_branch.is_some())
                    && !run.is_main()
                    && !claimed.contains(&run.id)
            })
            .collect();

        if ready_runs.is_empty() {
            self.notice = Some("no completed workspaces ready to merge".to_string());
            return;
        }

        let mut pending_total = 0usize;
        let mut pending_runs = 0usize;
        for run in &ready_runs {
            if let Some(p) = run.worktree_path.as_ref() {
                let c = count_uncommitted_changes(p);
                if c > 0 {
                    pending_total += c;
                    pending_runs += 1;
                }
            }
        }
        let ids: Vec<String> = ready_runs.iter().map(|r| r.id.clone()).collect();
        let count = ids.len();

        self.delete_pending = None;
        self.merge_confirm = Some(MergeConfirmation {
            intent: MergeIntent::All { ids },
        });
        self.conflict_prompt = None;
        let pending_suffix = if pending_total > 0 {
            format!(
                " ({pending_total} uncommitted file{p1} across {pending_runs} worktree{p2} will be auto-committed)",
                p1 = if pending_total == 1 { "" } else { "s" },
                p2 = if pending_runs == 1 { "" } else { "s" },
            )
        } else {
            String::new()
        };
        self.notice = Some(format!(
            "merge {count} completed worktree{plural}? press y to confirm or n to cancel{pending_suffix}",
            plural = if count == 1 { "" } else { "s" }
        ));
    }

    fn handle_merge_prompt_key(&mut self, key: KeyEvent) -> bool {
        if self.merge_confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_pending_merge(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.merge_confirm = None;
                    self.notice = Some("merge cancelled".to_string());
                }
                _ => {}
            }
            return true;
        }

        if self.conflict_prompt.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.start_conflict_resolution_agent(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    let rebase = self
                        .conflict_prompt
                        .as_ref()
                        .is_some_and(|prompt| prompt.operation == ConflictOperation::Rebase);
                    self.conflict_prompt = None;
                    self.notice = Some(if rebase {
                        "resolve the rebase conflicts in the worktree, then run git rebase --continue"
                            .to_string()
                    } else {
                        "resolve the merge conflicts manually, then commit".to_string()
                    });
                }
                _ => {}
            }
            return true;
        }

        false
    }

    fn confirm_pending_merge(&mut self) {
        let Some(confirm) = self.merge_confirm.take() else {
            return;
        };

        match confirm.intent {
            MergeIntent::Selected { id, task } => {
                let Some(index) = self.agents.iter().position(|run| run.id == id) else {
                    self.notice = Some("selected agent no longer exists".to_string());
                    return;
                };
                let source_branch = self.agents[index].worktree_branch.clone();
                let worktree_path = self.agents[index].worktree_path.clone();
                let agent_id = Some(self.agents[index].id.clone());
                match self.merge_agent_at(index) {
                    Ok(()) => {
                        self.delete_pending = None;
                        self.notice = Some("merged selected worktree".to_string());
                    }
                    Err(error) => {
                        self.handle_merge_error(
                            task,
                            error,
                            None,
                            source_branch,
                            worktree_path,
                            agent_id,
                        );
                    }
                }
            }
            MergeIntent::All { ids } => {
                let mut merged = 0;
                for id in ids {
                    let Some(index) = self.agents.iter().position(|run| run.id == id) else {
                        continue;
                    };
                    let task = self.agents[index].task.clone();
                    let source_branch = self.agents[index].worktree_branch.clone();
                    let worktree_path = self.agents[index].worktree_path.clone();
                    let agent_id = Some(self.agents[index].id.clone());
                    if let Err(error) = self.merge_agent_at(index) {
                        self.handle_merge_error(
                            task,
                            error,
                            Some(merged),
                            source_branch,
                            worktree_path,
                            agent_id,
                        );
                        return;
                    }
                    merged += 1;
                }
                self.delete_pending = None;
                self.notice = Some(format!(
                    "merged {merged} worktree{}",
                    if merged == 1 { "" } else { "s" }
                ));
            }
        }
        // A merge can satisfy a child's hard dep on the just-merged parent. Drain the
        // scheduler now so those children leave todo immediately instead of waiting
        // for the next periodic tick. No-op when nothing newly became ready.
        self.run_scheduler();
    }

    fn handle_merge_error(
        &mut self,
        task: String,
        error: anyhow::Error,
        merged_before_error: Option<usize>,
        source_branch: Option<String>,
        worktree_path: Option<PathBuf>,
        agent_id: Option<String>,
    ) {
        let mut operation = ConflictOperation::Merge;
        let mut conflict_root = self.cwd.clone();
        // jj runs report their conflicted files through pending_jj_conflict (jj
        // records conflicts in the change, not as git unmerged paths). Prefer
        // those; fall back to git's unmerged paths for legacy git-worktree runs.
        let jj_conflicts = self.pending_jj_conflict.take();
        let mut conflicts = jj_conflicts.unwrap_or_else(|| conflicted_files(&self.cwd));
        if conflicts.is_empty() {
            if let Some(path) = worktree_path.as_ref() {
                let worktree_conflicts = conflicted_files(path);
                if !worktree_conflicts.is_empty() {
                    operation = ConflictOperation::Rebase;
                    conflict_root = path.clone();
                    conflicts = worktree_conflicts;
                }
            }
        }
        if conflicts.is_empty() {
            let prefix = merged_before_error
                .map(|count| format!("merge all stopped after {count}: "))
                .unwrap_or_else(|| "merge stopped: ".to_string());
            self.notice = Some(format!("{prefix}{error}"));
            if let Some(index) = agent_id
                .as_deref()
                .and_then(|id| self.agents.iter().position(|run| run.id == id))
            {
                if let Some(run) = self.agents.get_mut(index) {
                    run.status = AgentStatus::Failed;
                    run.last_error = Some(error.to_string());
                    let _ = save_native_run_record(&self.cwd, run);
                }
            }
            return;
        }

        let count = conflicts.len();
        let target_branch = current_branch_at(&self.cwd);
        self.conflict_prompt = Some(MergeConflictPrompt {
            operation,
            task,
            conflicted_files: conflicts,
            error: error.to_string(),
            repo_root: conflict_root,
            target_branch,
            source_branch: source_branch.clone(),
            worktree_path: worktree_path.clone(),
            agent_id: agent_id.clone(),
        });
        if let Some(index) = agent_id
            .as_deref()
            .and_then(|id| self.agents.iter().position(|run| run.id == id))
        {
            if let Some(run) = self.agents.get_mut(index) {
                run.status = AgentStatus::Stopped;
                run.last_error = Some(error.to_string());
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        let operation_label = if operation == ConflictOperation::Rebase {
            "rebase"
        } else {
            "merge"
        };
        self.notice = Some(format!(
            "{operation_label} conflict in {count} file{}: press y to let AI resolve & complete the merge, or n to do it manually",
            if count == 1 { "" } else { "s" }
        ));
    }

    /// STEERING: re-goal a running/finished worker by RESUMING its session (so it
    /// keeps its memory and its jj-workspace edits) and delivering a new objective as
    /// the next turn. Falls back to a fresh, context-carrying spawn when there is no
    /// resumable session. The conflict resolver (below) is the same re-task-in-place
    /// pattern; this differs by resuming instead of minting fresh. Returns true on
    /// success. Never re-goals the main agent or the orchestrator.
    // Wired by the autonomous drift-fix (2c) and conductor chat routing (2d); the
    // stop primitive below already has a keybinding.
    #[allow(dead_code)]
    fn regoal_agent_at(&mut self, index: usize, new_goal: &str) -> bool {
        let cwd_default = self.cwd.clone();
        let (command, deliver_after, new_session, size, cwd, node_label) = {
            let Some(run) = self.agents.get(index) else {
                return false;
            };
            if run.is_main() || run.is_orchestrator() {
                return false;
            }
            let backend = run.backend;
            let size = run.terminal_size.unwrap_or_default();
            let cwd = run.cwd.clone();
            let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
            let session = run.session_id.clone().filter(|s| !s.trim().is_empty());
            let (command, deliver_after, new_session) = if let Some(sid) = session.as_deref() {
                // Resume the SAME session; the new goal arrives as the next turn.
                let command = match backend {
                    Backend::Claude => claude_resume_command(run, sid),
                    Backend::Codex => codex_resume_command(run, sid),
                };
                (command, Some(new_goal.to_string()), session.clone())
            } else {
                // No session to resume: fresh spawn carrying a handoff. Files persist
                // in the jj workspace; only conversation memory is lost.
                let sid = mint_session_id_for(backend);
                let prompt = format!(
                    "Continue your task: {}. New direction: {new_goal}. Your prior edits are in this workspace; run `jj diff` to see them.",
                    run.task
                );
                let command =
                    agent_command(backend, &run.model, run.effort, &prompt, AgentMode::Execute, sid.as_deref());
                (command, None, sid)
            };
            (command, deliver_after, new_session, size, cwd, label)
        };

        let options = TerminalPaneOptions {
            size,
            cwd: Some(cwd),
            ..TerminalPaneOptions::default()
        };
        if let Some(run) = self.agents.get_mut(index) {
            run.terminal = None;
            run.review_terminal = None;
        }
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                if let Some(text) = &deliver_after {
                    let _ = terminal.write_input(format!("{text}\r").as_bytes());
                }
                let now = now_stamp();
                if let Some(run) = self.agents.get_mut(index) {
                    run.terminal = Some(terminal);
                    run.status = AgentStatus::Running;
                    run.session_id = new_session;
                    run.completed_at = None;
                    run.last_output_at = Instant::now();
                    run.ready_since = None;
                    run.needs_permission = false;
                    run.permission_notified = false;
                    run.needs_user_input = false;
                    run.user_input_notified = false;
                    run.last_error = None;
                    run.current_prompt = new_goal.to_string();
                    run.turns.push(AgentTurn {
                        ts: now.clone(),
                        prompt: new_goal.to_string(),
                        source: "regoal".to_string(),
                    });
                    run.last_user_input_at = now;
                    run.last_worker_input_at = Some(Instant::now());
                    let _ = save_native_run_record(&cwd_default, run);
                }
                self.push_activity(format!("re-goaled {node_label}: {}", short_task(new_goal)));
                self.mirror_graph();
                true
            }
            Err(error) => {
                if let Some(run) = self.agents.get_mut(index) {
                    run.status = AgentStatus::Failed;
                    run.last_error = Some(error.to_string());
                }
                self.notice = Some(format!("re-goal failed: {error}"));
                false
            }
        }
    }

    /// STEERING: write a line of context straight into a RUNNING agent's PTY (it
    /// picks it up on its next turn). Returns false if there is no live terminal
    /// (the caller can fall back to re-goal-via-resume).
    fn live_inject_at(&mut self, index: usize, text: &str) -> bool {
        let result = match self.agents.get_mut(index).and_then(|run| run.terminal.as_mut()) {
            Some(terminal) => {
                terminal.reset_scrollback();
                terminal.write_input(format!("{text}\r").as_bytes())
            }
            None => return false,
        };
        if result.is_ok() {
            if let Some(run) = self.agents.get_mut(index) {
                run.last_worker_input_at = Some(Instant::now());
            }
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// STEERING: stop a running/queued worker. Drops its PTY and marks it Stopped,
    /// freeing a parallelism slot (running_plan_agents ignores Stopped) while KEEPING
    /// its jj workspace on disk (undoable / re-goalable). Its node id never enters the
    /// merged set, so hard dependents stay correctly blocked.
    fn stop_agent_at(&mut self, index: usize) -> bool {
        let cwd = self.cwd.clone();
        let label = {
            let Some(run) = self.agents.get_mut(index) else {
                return false;
            };
            if run.is_main() {
                return false;
            }
            run.terminal = None;
            run.review_terminal = None;
            run.status = AgentStatus::Stopped;
            run.completed_at = Some(Instant::now());
            run.merge_resolver = false;
            run.needs_permission = false;
            run.permission_notified = false;
            run.needs_user_input = false;
            run.user_input_notified = false;
            let _ = save_native_run_record(&cwd, run);
            run.node_id.clone().unwrap_or_else(|| run.id.clone())
        };
        self.push_activity(format!("stopped {label}"));
        self.mirror_graph();
        true
    }

    /// AUTONOMOUS DRIFT (2c): predict cross-agent collisions and act WITHOUT a confirm
    /// (product decision: autonomous, see AGENTS.md §14.5). Agents are isolated, so the
    /// concrete signal is two RUNNING workers modifying the SAME file in their separate
    /// jj workspaces (a future merge conflict). The least-disruptive autonomous fix is
    /// to live-INJECT a coordination note into the later-launched agent so it adapts
    /// in-flight (no restart, no stop) and the merge stays clean; the merge-time AI
    /// resolver remains the backstop. Each colliding pair is nudged once. Throttled.
    fn maybe_handle_drift(&mut self) {
        if let Some(last) = self.last_drift_scan {
            if last.elapsed() < Duration::from_secs(5) {
                return;
            }
        }
        self.last_drift_scan = Some(Instant::now());

        let running: Vec<(usize, String, PathBuf)> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.node_id.is_some()
                    && matches!(r.mode, AgentMode::Execute)
                    && r.status == AgentStatus::Running
                    && !r.merge_resolver
            })
            .map(|(i, r)| (i, r.node_id.clone().unwrap_or_default(), r.cwd.clone()))
            .collect();
        if running.len() < 2 {
            return;
        }
        let touched: Vec<(usize, String, Vec<String>)> = running
            .into_iter()
            .map(|(i, id, cwd)| (i, id, jj_touched_files(&cwd)))
            .collect();

        // Phase 1: find newly-colliding pairs (mutates only surfaced_overlaps).
        let mut nudges: Vec<(usize, String, String, Vec<String>)> = Vec::new();
        for a in 0..touched.len() {
            for b in (a + 1)..touched.len() {
                let overlap = overlapping_files(&touched[a].2, &touched[b].2);
                if overlap.is_empty() {
                    continue;
                }
                let (ia, ida) = (touched[a].0, touched[a].1.clone());
                let (ib, idb) = (touched[b].0, touched[b].1.clone());
                let key = if ida <= idb {
                    (ida.clone(), idb.clone())
                } else {
                    (idb.clone(), ida.clone())
                };
                if !self.surfaced_overlaps.insert(key) {
                    continue; // already nudged this pair
                }
                // Nudge the LATER-launched agent (higher index ~ later, less invested).
                let later_index = if ia >= ib { ia } else { ib };
                nudges.push((later_index, ida, idb, overlap));
            }
        }
        // Phase 2: act (mutates agents + activity log).
        for (later_index, a_id, b_id, files) in nudges {
            let note = format!(
                "Heads up from Rudder: a sibling agent is also editing {} (predicted merge overlap). Coordinate via DECISIONS.md and integrate ON TOP of those files rather than restructuring them; whoever merges first sets the base.",
                files.join(", ")
            );
            let injected = self.live_inject_at(later_index, &note);
            self.push_activity(format!(
                "drift: {a_id} & {b_id} overlap on {} - {}",
                files.join(","),
                if injected {
                    "nudged the later agent to coordinate"
                } else {
                    "noted (no live pane); merge resolver will reconcile"
                }
            ));
        }
    }

    fn start_conflict_resolution_agent(&mut self) {
        let Some(prompt) = self.conflict_resolution_prompt() else {
            return;
        };
        let agent_id = self
            .conflict_prompt
            .as_ref()
            .and_then(|p| p.agent_id.clone());
        let original_task = self
            .conflict_prompt
            .as_ref()
            .map(|p| p.task.clone())
            .unwrap_or_default();
        let conflict_context = self.conflict_prompt.as_ref().map(|p| {
            (
                p.operation,
                p.repo_root.clone(),
                p.source_branch.clone(),
                p.worktree_path.clone(),
            )
        });
        self.conflict_prompt = None;

        // Reuse the failing agent's row instead of spawning a new pane. The
        // PTY is rooted in the checkout that contains the conflicted operation.
        let target_index = agent_id
            .as_deref()
            .and_then(|id| self.agents.iter().position(|a| a.id == id));
        let Some(index) = target_index else {
            self.notice = Some(
                "could not find the agent to resolve conflicts in (its row was removed)"
                    .to_string(),
            );
            return;
        };

        let backend = self.agents[index].backend;
        let model = self.agents[index].model.clone();
        let effort = self.agents[index].effort;
        let terminal_size = self.agents[index].terminal_size.unwrap_or_default();
        let (operation, resolver_cwd, source_branch, worktree_path) = conflict_context
            .unwrap_or((ConflictOperation::Merge, self.cwd.clone(), None, None));
        let session_id = mint_session_id_for(backend);
        let command = agent_command(
            backend,
            &model,
            effort,
            &prompt,
            AgentMode::Execute,
            session_id.as_deref(),
        );
        let options = TerminalPaneOptions {
            size: terminal_size,
            cwd: Some(resolver_cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        // Drop the old PTY before spawning the resolver in the checkout that
        // contains the conflicted git operation.
        if let Some(run) = self.agents.get_mut(index) {
            run.terminal = None;
            run.review_terminal = None;
        }

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                let now = now_stamp();
                if let Some(run) = self.agents.get_mut(index) {
                    run.cwd = resolver_cwd.clone();
                    run.terminal = Some(terminal);
                    run.status = AgentStatus::Running;
                    if operation == ConflictOperation::Rebase {
                        run.worktree_path = worktree_path.or_else(|| Some(resolver_cwd.clone()));
                        run.worktree_branch = source_branch;
                    } else {
                        run.worktree_path = None;
                        run.worktree_branch = None;
                    }
                    run.session_id = session_id;
                    run.completed_at = None;
                    run.last_output_at = Instant::now();
                    run.needs_permission = false;
                    run.permission_notified = false;
                    run.needs_user_input = false;
                    run.user_input_notified = false;
                    run.last_error = None;
                    // Track jj merge resolvers so the poll loop finalizes the merge
                    // (flip the node to Merged, unblock children) once they finish
                    // with no conflicts left. Git rebase resolvers keep manual flow.
                    run.merge_resolver = operation == ConflictOperation::Merge;
                    run.ready_since = None;
                    run.task = if original_task.is_empty() {
                        if operation == ConflictOperation::Rebase {
                            "Resolve rebase conflicts".to_string()
                        } else {
                            "Resolve merge conflicts".to_string()
                        }
                    } else {
                        format!(
                            "Resolve {} conflicts: {original_task}",
                            if operation == ConflictOperation::Rebase {
                                "rebase"
                            } else {
                                "merge"
                            }
                        )
                    };
                    run.task_summary = format!(
                        "{} conflicts \u{2192} {}",
                        if operation == ConflictOperation::Rebase {
                            "rebase"
                        } else {
                            "merge"
                        },
                        summarize_task(&original_task)
                    );
                    run.turns.push(AgentTurn {
                        ts: now.clone(),
                        prompt: prompt.clone(),
                        source: "user".to_string(),
                    });
                    run.last_user_input_at = now;
                }
                self.selected_agent = index;
                self.focus = FocusPane::Worker;
                self.notice = Some("AI conflict resolver running in this pane".to_string());
                if let Some(run) = self.agents.get(index) {
                    let _ = save_native_run_record(&self.cwd, run);
                }
                let _ = write_rudder_context(&self.cwd, &self.agents, None);
            }
            Err(error) => {
                if let Some(run) = self.agents.get_mut(index) {
                    run.status = AgentStatus::Failed;
                    run.last_error = Some(error.to_string());
                }
                self.notice = Some(format!("failed to start AI resolver: {error}"));
            }
        }
    }

    fn conflict_resolution_prompt(&self) -> Option<String> {
        let prompt = self.conflict_prompt.as_ref()?;
        let files = if prompt.conflicted_files.is_empty() {
            "(no specific conflicted files were reported; run the status command to find them)".to_string()
        } else {
            prompt
                .conflicted_files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let repo = prompt.repo_root.display().to_string();
        let target = prompt
            .target_branch
            .clone()
            .unwrap_or_else(|| "HEAD".to_string());
        let source = prompt
            .source_branch
            .clone()
            .unwrap_or_else(|| "(unknown branch)".to_string());
        if prompt.operation == ConflictOperation::Rebase {
            return Some(format!(
                "A git rebase stopped with conflicts in the agent worktree.\n\
\n\
Where you are working\n\
- You are running inside the conflicted worktree at: {repo}\n\
- The worktree branch being rebased is: {source}\n\
- The base branch is: {target}\n\
\n\
What was being attempted\n\
- Original task on {source}: {task}\n\
\n\
What conflicted\n\
{files}\n\
\n\
Git reported\n\
{err}\n\
\n\
What to do\n\
1. Run `git status` from {repo} to see the rebase state.\n\
2. Resolve the conflict markers in the files above.\n\
3. After every file is resolved, run relevant tests or checks if practical.\n\
4. Stage the resolved files with `git add`.\n\
5. Run `git rebase --continue` and report the result. Do not abort the rebase unless it is truly unresolvable.\n",
                repo = repo,
                target = target,
                source = source,
                task = prompt.task,
                files = files,
                err = prompt.error,
            ));
        }
        Some(format!(
            "A jj (Jujutsu) merge created conflicts and you are now the conflict resolver.\n\
\n\
This repo uses jj, NOT git. Use jj commands. Do NOT use git (`git add`/`git commit`/`git merge`); those do not resolve jj conflicts and will desync the repo.\n\
\n\
Where you are working\n\
- You are running inside the main repo at: {repo}\n\
- jj already created the merge commit; it is the current working-copy change (`@`) here. Both the original code and the agent's new work are its parents, and jj has written conflict markers into the working copy for the files that could not auto-merge.\n\
\n\
What was being attempted\n\
- Original task being merged in: {task}\n\
\n\
What conflicted\n\
{files}\n\
\n\
jj reported\n\
{err}\n\
\n\
How to think about the sides\n\
- Conflict markers wrap the competing versions. One side is the existing code already integrated; the other is the agent's new work for the task above.\n\
- Preserve the intent of the task while not regressing existing behavior.\n\
\n\
What to do\n\
1. Run `jj status` from {repo} to see the merge state, and `jj resolve --list` to list the files jj still considers conflicted.\n\
2. Open each conflicted file and resolve the markers by hand (or run `jj resolve` to use the configured merge tool). Editing the working copy IS the resolution in jj. There is no staging step and no `git add`.\n\
3. After every file is resolved, confirm `jj resolve --list` reports nothing left, then run any relevant tests or checks the repo provides.\n\
4. Tell me what you changed and why. Do NOT run `jj commit`, `jj squash`, `jj new`, or `jj abandon`: the merge change already exists and Rudder finalizes it once `jj resolve --list` is empty.\n\
5. Do NOT undo the merge (`jj undo`/`jj op undo`) unless the conflicts are truly unresolvable, and if so explain why.\n",
            repo = repo,
            task = prompt.task,
            files = files,
            err = prompt.error,
        ))
    }

    /// A run is jj-isolated when it carries a jj workspace name or change id. New
    /// runs are always jj; only pre-existing git-worktree runs (a git branch and
    /// no jj workspace, e.g. the review-all aggregate) take the legacy git path.
    fn run_is_jj(run: &AgentRun) -> bool {
        run.workspace_name.is_some() || run.jj_change_id.is_some()
    }

    fn merge_agent_at(&mut self, index: usize) -> Result<()> {
        self.pending_jj_conflict = None;
        let Some(run) = self.agents.get(index) else {
            anyhow::bail!("no selected agent");
        };
        let is_jj = Self::run_is_jj(run);
        let review_source_ids = run.review_source_ids.clone();

        if is_jj {
            // jj runs merge through the TS `rudder merge <id>` command, which
            // routes to mergeJjRunIntoCurrentWorkspace and captures op-log for
            // `rudder undo`. The command records its outcome in run.json and
            // exits 0 even on conflict, so classify by the recorded state.
            let run_id = run.id.clone();
            match run_rudder_jj_command(&self.cwd, "merge", &run_id, "merge") {
                JjCliOutcome::Ok => {}
                JjCliOutcome::Conflict { files } => {
                    self.pending_jj_conflict = Some(files);
                    anyhow::bail!("jj merge created conflicts");
                }
                JjCliOutcome::Failed { error } => anyhow::bail!(error),
            }
        } else {
            let Some(branch) = run.worktree_branch.clone() else {
                anyhow::bail!("selected agent is not in a worktree");
            };
            commit_pending_changes_for_run(run)?;
            match merge_strategy() {
                MergeStrategy::Merge => {
                    git_status_command(&self.cwd, &["merge", "--no-ff", &branch])?;
                }
                MergeStrategy::Rebase => {
                    let base_branch =
                        current_branch_at(&self.cwd).unwrap_or_else(|| "HEAD".to_string());
                    rebase_worktree_onto_base(&self.cwd, &run.cwd, &base_branch)?;
                    git_status_command(&self.cwd, &["merge", "--ff-only", &branch])?;
                }
            }
        }
        // Successful merge: keep the agent's row in the dashboard but flip it
        // to Merged so it appears in a dedicated section. Keep the worktree
        // path on the record and defer cleanup to delete, which keeps merge
        // confirmation responsive. Never touch the dedicated main agent.
        if index < self.agents.len() && !self.agents[index].is_main() {
            self.mark_agent_and_review_sources_merged(index, review_source_ids);
        }
        Ok(())
    }

    fn sync_agent_at(&mut self, index: usize) -> Result<()> {
        self.pending_jj_conflict = None;
        let Some(run) = self.agents.get(index) else {
            anyhow::bail!("no selected agent");
        };
        if Self::run_is_jj(run) {
            // jj sync rebases the run's change onto the base via `rudder sync`.
            let run_id = run.id.clone();
            match run_rudder_jj_command(&self.cwd, "sync", &run_id, "sync") {
                JjCliOutcome::Ok => {}
                JjCliOutcome::Conflict { files } => {
                    self.pending_jj_conflict = Some(files);
                    anyhow::bail!("jj sync created conflicts");
                }
                JjCliOutcome::Failed { error } => anyhow::bail!(error),
            }
            if let Some(run) = self.agents.get(index) {
                save_native_run_record(&self.cwd, run)?;
            }
            return Ok(());
        }
        if run.worktree_branch.is_none() {
            anyhow::bail!("selected agent is not in a worktree");
        }
        commit_pending_changes_for_run(run)?;
        let base_branch = current_branch_at(&self.cwd).unwrap_or_else(|| "HEAD".to_string());
        rebase_worktree_onto_base(&self.cwd, &run.cwd, &base_branch)?;
        if let Some(run) = self.agents.get(index) {
            save_native_run_record(&self.cwd, run)?;
        }
        Ok(())
    }

    fn mark_agent_and_review_sources_merged(
        &mut self,
        index: usize,
        review_source_ids: Vec<String>,
    ) {
        let mut merge_indices = Vec::new();
        if index < self.agents.len() && !self.agents[index].is_main() {
            merge_indices.push(index);
        }
        for source_id in review_source_ids {
            if let Some(source_index) = self
                .agents
                .iter()
                .position(|run| run.id == source_id && !run.is_main())
            {
                if !merge_indices.contains(&source_index) {
                    merge_indices.push(source_index);
                }
            }
        }

        for merge_index in merge_indices {
            if let Some(run) = self.agents.get_mut(merge_index) {
                run.terminal = None;
                run.review_terminal = None;
                run.status = AgentStatus::Merged;
                run.worktree_branch = None;
                run.completed_at = Some(Instant::now());
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
        // A merge is a meaningful node transition (-> merged); mirror it so the
        // board reflects it without waiting for the next poll pass. Coalesced.
        self.mirror_graph();
    }

    fn poll_agents(&mut self) {
        self.poll_task_summary_workers();

        if self.last_cloud_check.elapsed() >= Duration::from_secs(2) {
            let cloud = read_cloud_summary();
            if self.cloud_connected != cloud.connected || self.cloud_runtime != cloud.runtime {
                self.dirty = true;
            }
            self.cloud_connected = cloud.connected;
            self.cloud_runtime = cloud.runtime;
            self.last_cloud_check = Instant::now();
        }

        self.refresh_cloud_workspace_status();
        self.maybe_notify_workspace_idle();

        // Only fully drain the focused agent every tick. For unfocused agents,
        // throttle drains to every 500ms so vt100 parsing + styled-cache
        // invalidation cost scales with focus rather than with agent count.
        const UNFOCUSED_DRAIN_INTERVAL: Duration = Duration::from_millis(500);
        let focused_index = self.selected_agent;
        let now = Instant::now();
        let repo_root = self.cwd.clone();
        let mut any_dirty = false;
        let mut completed_rudder_plans = Vec::new();
        for (index, run) in self.agents.iter_mut().enumerate() {
            let mut changed = false;
            let Some(terminal) = run.terminal.as_mut() else {
                continue;
            };
            let is_focused = index == focused_index;
            // An actively-streaming planner is always drained every tick (even if not
            // the selected row), so its live transcript never falls into the 500ms
            // unfocused throttle and streams smoothly.
            let is_streaming_planner =
                run.mode == AgentMode::RudderPlan && run.status == AgentStatus::Running;
            let due_to_drain = is_focused
                || is_streaming_planner
                || run
                    .last_drain_at
                    .is_none_or(|stamp| now.duration_since(stamp) >= UNFOCUSED_DRAIN_INTERVAL);
            if !due_to_drain {
                // Skip the heavy drain+parse on unfocused panes; still keep
                // liveness signal cheap via try_wait below.
                if let Ok(Some(status)) = terminal.try_wait() {
                    if status.success() {
                        mark_run_done(run);
                        if run.mode == AgentMode::RudderPlan && run.autosteered {
                            completed_rudder_plans.push(index);
                        }
                    } else {
                        run.status = AgentStatus::Failed;
                        run.completed_at = Some(Instant::now());
                        run.needs_permission = false;
                        run.permission_notified = false;
                        run.needs_user_input = false;
                        run.user_input_notified = false;
                        play_completion_sound();
                    }
                    let _ = save_native_run_record(&repo_root, run);
                    any_dirty = true;
                }
                continue;
            }
            run.last_drain_at = Some(now);
            let had_output = !terminal.drain_output().is_empty();
            if had_output {
                any_dirty = true;
            }
            // Feed the orchestrator's JSON event stream into its live transcript +
            // reconstructed plan text, and capture the backend session id so a refine
            // can resume the same conversation. Incremental: a no-op when no new bytes.
            if run.mode == AgentMode::RudderPlan {
                let snapshot = terminal.output_log_snapshot().to_string();
                let stream = run.plan_stream.get_or_insert_with(PlanStreamState::new);
                if stream.ingest(&snapshot) {
                    changed = true;
                }
                let captured = stream.session_id().map(str::to_string);
                if run.session_id.is_none() {
                    if let Some(sid) = captured {
                        run.session_id = Some(sid);
                    }
                }
            }
            if had_output {
                run.last_output_at = Instant::now();
                if run.status == AgentStatus::Done {
                    // Only treat post-completion output as a NEW turn if the user
                    // actually sent input since the agent finished. Otherwise this is
                    // just a repaint (e.g. the resize that fires when you HIGHLIGHT a
                    // finished agent), and flipping done->in-progress is the flicker
                    // the pane suffered from. Require user input after completion.
                    let user_started_new_turn = post_completion_output_is_new_turn(
                        run.last_worker_input_at,
                        run.completed_at,
                    );
                    // Backstop against a false completion: a reappearing busy spinner
                    // ("esc to interrupt") is unambiguous proof the agent is actively
                    // working, so reopen it even without user input — we mis-detected
                    // done during a transient inter-step lull. A genuine idle repaint
                    // (the highlight resize) shows no spinner, so this never revives
                    // the flicker the user-input guard was added to kill.
                    let resumed_work = !user_started_new_turn
                        && recent_lines_look_busy(&terminal.visible_lines_snapshot());
                    if user_started_new_turn || resumed_work {
                        run.status = AgentStatus::Running;
                        run.completed_at = None;
                        run.ready_since = None;
                        changed = true;
                    }
                }
            }
            if run.status == AgentStatus::Running {
                // Evaluate the screen when there is activity, when we are already
                // tracking readiness, or after a brief output lull. The lull (not a
                // full 4s silence) is key: an idle agent whose TUI keeps repainting
                // (cursor blink/animation) still gets re-checked, so completion no
                // longer depends on output going fully silent.
                let lull = run.last_output_at.elapsed() >= READY_EVAL_LULL;
                let visible_lines = if had_output
                    || run.needs_permission
                    || run.needs_user_input
                    || run.ready_since.is_some()
                    || lull
                {
                    Some(terminal.visible_lines_snapshot())
                } else {
                    None
                };
                let needs_permission = visible_lines
                    .as_ref()
                    .is_some_and(|lines| terminal_needs_permission_from_lines(lines));
                run.needs_permission = needs_permission;
                if needs_permission {
                    if !run.permission_notified {
                        play_completion_sound();
                    }
                    run.permission_notified = true;
                }
                // Intentionally do NOT reset *_notified when the flag flips
                // back to false. The detector heuristics can flicker while the
                // agent is streaming, and resetting here causes a fresh ping
                // on every flicker. The notified flags stay sticky until the
                // user actually types something (clear_selected_attention_flags
                // handles that), at which point a real new prompt will ring
                // again.
                let needs_user_input = !needs_permission
                    && visible_lines
                        .as_ref()
                        .is_some_and(|lines| terminal_needs_user_input_from_lines(lines));
                run.needs_user_input = needs_user_input;
                if needs_user_input && !run.user_input_notified {
                    play_completion_sound();
                    run.user_input_notified = true;
                }
                match terminal.try_wait() {
                    Ok(Some(status)) => {
                        // Process exit is the most reliable signal: done if it
                        // succeeded, failed otherwise.
                        run.ready_since = None;
                        if status.success() {
                            mark_run_done(run);
                            changed = true;
                        } else {
                            run.status = AgentStatus::Failed;
                            run.completed_at = Some(Instant::now());
                            run.needs_permission = false;
                            run.permission_notified = false;
                            run.needs_user_input = false;
                            run.user_input_notified = false;
                            play_completion_sound();
                            changed = true;
                        };
                    }
                    Ok(None) => {
                        // The process is still alive (interactive agents do not exit
                        // when a turn ends). Declare completion once it has looked
                        // ready-for-input continuously for READY_GRACE. Tracking the
                        // ready window (not output-silence) is what makes this robust
                        // against a TUI that repaints while idle.
                        let ready = visible_lines.as_ref().is_some_and(|lines| {
                            terminal_looks_ready_for_input_from_lines(run.backend, lines)
                        });
                        if ready {
                            let since = *run.ready_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= READY_GRACE {
                                run.ready_since = None;
                                mark_run_done(run);
                                changed = true;
                            }
                        } else if visible_lines.is_some() {
                            // We could read the screen and it is NOT idle (busy or a
                            // prompt/permission): reset the window.
                            run.ready_since = None;
                        }
                    }
                    Err(error) => {
                        run.ready_since = None;
                        run.status = AgentStatus::Failed;
                        run.completed_at = Some(Instant::now());
                        run.last_error = Some(error.to_string());
                        run.needs_permission = false;
                        run.permission_notified = false;
                        run.needs_user_input = false;
                        run.user_input_notified = false;
                        play_completion_sound();
                        changed = true;
                    }
                }
            } else {
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
            }
            if changed {
                if run.mode == AgentMode::RudderPlan
                    && run.status == AgentStatus::Done
                    && run.autosteered
                {
                    completed_rudder_plans.push(index);
                }
                any_dirty = true;
                let _ = save_native_run_record(&repo_root, run);
            }
            if run.mode == AgentMode::RudderPlan
                && run.status == AgentStatus::Done
                && run.autosteered
                && !completed_rudder_plans.contains(&index)
            {
                completed_rudder_plans.push(index);
            }
        }

        // The INITIAL planner run is KEPT as the pinned orchestrator (no removal),
        // so its index stays stable. A RECONCILE planner is transient and removed by
        // the APPEND path. Process highest-index first so a reconcile removal never
        // shifts an index we still need. Route on the `reconcile_planner`
        // discriminator: the append path preserves the existing plan's not-yet-
        // launched nodes, while the replace path captures a fresh plan. `autosteered`
        // is cleared inside each path so a re-poll of the same Done planner is a
        // no-op. This handles the case where the planner process actually exits
        // after printing the block.
        completed_rudder_plans.sort_unstable();
        completed_rudder_plans.dedup();
        for index in completed_rudder_plans.into_iter().rev() {
            let is_reconcile = self
                .agents
                .get(index)
                .is_some_and(|run| run.reconcile_planner);
            if is_reconcile {
                self.evaluate_completed_reconcile(index);
            } else {
                self.evaluate_completed_plan(index);
            }
        }

        // STREAMING DETECTION: a plan-mode planner may print the RUDDER_PLAN_TASKS
        // block and then BLOCK at an ExitPlanMode approval prompt without ever
        // exiting, so the Done-keyed path above never fires. Capture the plan into
        // the approval gate as soon as a parseable block appears in the live output.
        self.maybe_detect_plan_ready();

        // Drain the planned-node queue on a coarse cadence: as plan-launched
        // agents reach Merged their node ids satisfy dependents' hard deps, so a
        // periodic pass moves newly-ready nodes todo->in progress as slots free.
        // Suppressed while a plan awaits approval: nothing launches until the user
        // approves the DAG at the gate.
        self.scheduler_tick = self.scheduler_tick.wrapping_add(1);
        if !self.awaiting_approval
            && !self.planned_nodes.is_empty()
            && self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0
        {
            self.run_scheduler();
        }
        // When auto-merge is on, merge clean finished nodes on the same cadence so
        // their children unblock and the chain flows without manual m/M. Runs even
        // when the queue is empty (to merge the final nodes), but not at the gate.
        if self.auto_merge
            && !self.awaiting_approval
            && self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0
        {
            self.maybe_auto_merge();
        }
        // Finalize any AI merge-conflict resolver that just finished (auto-spawned or
        // started manually with y): a clean workspace flips the node to Merged and
        // unblocks its children; leftover conflicts drop back to manual.
        if self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            self.finalize_merge_resolvers();
        }
        // Grow the DAG from finished workers' `rudder done` reports (auto-expand):
        // a finishing agent's recommended follow-ups become new planned nodes,
        // surfaced in the activity log. Autonomous, no confirm.
        if self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            self.maybe_ingest_worker_followups();
        }
        // Predict cross-agent collisions and nudge the later agent to coordinate
        // (autonomous; self-throttled to ~5s). No confirm.
        if self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            self.maybe_handle_drift();
        }

        // Advance the spinner each tick and force a redraw while an orchestrator is
        // still planning, so "decomposing the task..." animates even when the
        // planner is not emitting bytes this tick.
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        if self.has_planning_orchestrator() {
            self.dirty = true;
        }

        // MIRROR plan-launched agents' status transitions (running->review->merged
        // / failed) into graph.json so the board tracks them. Only when something
        // changed this pass AND a plan node/agent exists; the coalesce guard inside
        // mirror_graph then makes this a real shell-out ONLY when the DAG signature
        // actually changed (it is not per-tick: terminal-byte churn does not move
        // the signature, which excludes volatile output). Non-fatal.
        if any_dirty
            && (!self.planned_nodes.is_empty()
                || self.agents.iter().any(|run| run.node_id.is_some()))
        {
            self.mirror_graph();
        }

        if any_dirty {
            self.dirty = true;
        }
    }

    fn poll_task_summary_workers(&mut self) {
        let mut changed = false;
        let repo_root = self.cwd.clone();
        while let Ok(result) = self.task_summary_rx.try_recv() {
            let Some(title) = result.title else {
                continue;
            };
            let Some(run) = self.agents.iter_mut().find(|run| run.id == result.run_id) else {
                continue;
            };
            if !matches!(run.mode, AgentMode::Execute | AgentMode::Main) {
                continue;
            }
            if run.task_summary == title {
                continue;
            }
            run.task_summary = title;
            let _ = save_native_run_record(&repo_root, run);
            changed = true;
        }
        if changed {
            let _ = write_rudder_context(&self.cwd, &self.agents, None);
            self.dirty = true;
        }
    }

    fn shutdown(&mut self) {
        for run in &mut self.agents {
            if run.terminal.is_some() && run.status == AgentStatus::Running {
                if run.backend == Backend::Codex && run.session_id.is_none() {
                    run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
                }
                run.terminal = None;
                run.status = AgentStatus::Running;
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                run.completed_at = None;
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        let _ = write_rudder_context(&self.cwd, &self.agents, None);
    }
}

#[cfg(test)]
mod app_tests;

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--smoke") {
        println!("rudder-native smoke ok");
        return Ok(());
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "mouse-test") {
        return run_mouse_test(args.get(1).map(String::as_str).unwrap_or("parsed"));
    }

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
    enable_rudder_mouse_capture(&mut stdout)?;
    set_terminal_title(&mut stdout, &startup_title())?;
    stdout.flush()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn startup_title() -> String {
    let cwd = std::env::current_dir()
        .map(|p| repo_root(&p))
        .unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.display().to_string());
    let prefix = if is_cloud_worker_session() {
        "Rudder cloud"
    } else {
        "Rudder"
    };
    // Start in the idle state; refresh_tab_title overwrites once we have agents.
    format!("\u{26aa} {prefix}: {name}")
}

fn set_terminal_title(stdout: &mut impl Write, title: &str) -> io::Result<()> {
    // OSC 0: set both icon and window/tab title. Ghostty + iTerm + Terminal.app
    // + Alacritty + Kitty all honor this; for the user that means each rudder
    // tab labels itself instead of all reading "ghostty".
    write!(stdout, "\x1b]0;{title}\x07")
}

fn enable_rudder_mouse_capture(stdout: &mut impl Write) -> Result<()> {
    // Avoid crossterm's EnableMouseCapture here: in crossterm 0.29 it also
    // emits ?1003h any-event tracking, which reports every pointer movement
    // and can flood terminals like Kitty while scrolling. Enable only xterm
    // button, button-motion, and SGR modes; first disable ?1003 in case a
    // previous Rudder process left it on.
    stdout.write_all(RUDDER_MOUSE_ENABLE_SEQUENCES)?;
    stdout.flush()?;
    Ok(())
}

fn disable_rudder_mouse_capture(stdout: &mut impl Write) -> Result<()> {
    stdout.write_all(RUDDER_MOUSE_DISABLE_SEQUENCES)?;
    stdout.flush()?;
    Ok(())
}

fn run_mouse_test(mode: &str) -> Result<()> {
    match mode {
        "raw" => run_mouse_test_raw(),
        "parsed" | "" => run_mouse_test_parsed(),
        other => {
            eprintln!("unknown mouse-test mode: {other}");
            eprintln!("usage: rudder mouse-test [raw|parsed]");
            Ok(())
        }
    }
}

fn run_mouse_test_raw() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enable_rudder_mouse_capture(&mut stdout)?;
    writeln!(
        stdout,
        "rudder mouse-test raw\r\nScroll/click inside this terminal. Press q to quit.\r\n"
    )?;
    stdout.flush()?;

    let result = (|| -> Result<()> {
        let mut stdin = io::stdin();
        let mut buf = [0_u8; 64];
        loop {
            let n = stdin.read(&mut buf)?;
            if n == 0 {
                break;
            }
            for byte in &buf[..n] {
                if *byte == b'q' || *byte == 3 {
                    return Ok(());
                }
            }
            let printable = buf[..n]
                .iter()
                .map(|byte| match *byte {
                    0x1b => "ESC".to_string(),
                    b'\r' => "CR".to_string(),
                    b'\n' => "LF".to_string(),
                    0x20..=0x7e => format!("'{}'", *byte as char),
                    _ => format!("0x{byte:02x}"),
                })
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(stdout, "{printable}\r")?;
            stdout.flush()?;
        }
        Ok(())
    })();

    let _ = disable_rudder_mouse_capture(&mut stdout);
    let _ = disable_raw_mode();
    result
}

fn run_mouse_test_parsed() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enable_rudder_mouse_capture(&mut stdout)?;
    writeln!(
        stdout,
        "rudder mouse-test parsed\r\nScroll/click inside this terminal. Press q to quit.\r\n"
    )?;
    stdout.flush()?;

    let result = (|| -> Result<()> {
        loop {
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    writeln!(stdout, "key {:?} modifiers={:?}\r", key.code, key.modifiers)?;
                    stdout.flush()?;
                    if key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        break;
                    }
                }
                Event::Mouse(mouse) => {
                    writeln!(
                        stdout,
                        "mouse {:?} col={} row={} modifiers={:?}\r",
                        mouse.kind, mouse.column, mouse.row, mouse.modifiers
                    )?;
                    stdout.flush()?;
                }
                Event::Resize(cols, rows) => {
                    writeln!(stdout, "resize cols={cols} rows={rows}\r")?;
                    stdout.flush()?;
                }
                other => {
                    writeln!(stdout, "event {other:?}\r")?;
                    stdout.flush()?;
                }
            }
        }
        Ok(())
    })();

    let _ = disable_rudder_mouse_capture(&mut stdout);
    let _ = disable_raw_mode();
    result
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    disable_rudder_mouse_capture(terminal.backend_mut())?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    // Clear the tab title we set on startup so the user's shell prompt can
    // rewrite it on exit. Empty title is the conventional way to release it.
    let _ = set_terminal_title(&mut io::stdout(), "");
    terminal.show_cursor()?;
    Ok(())
}

fn run(terminal: &mut Tui) -> Result<()> {
    let mut app = App::new();
    app.resume_migrated_agents();
    app.restore_running_agents();

    loop {
        // poll_agents flips app.dirty when any state mutates (PTY bytes,
        // status change, cloud info, etc).
        app.poll_agents();
        app.refresh_tab_title();
        if app.take_dirty() {
            terminal.draw(|frame| render(frame, &mut app))?;
        }

        // While a planner is actively streaming, poll faster so its live transcript
        // lands ~3x sooner (text appears in finer increments instead of 33ms bursts).
        // Idle/normal use stays at the calmer 33ms tick to keep CPU low.
        let poll_timeout = if app.has_planning_orchestrator() {
            STREAM_TICK_RATE
        } else {
            TICK_RATE
        };
        if event::poll(poll_timeout)? {
            if handle_event(&mut app, event::read()?) {
                app.shutdown();
                break;
            }

            for _ in 1..MAX_EVENTS_PER_FRAME {
                if !event::poll(Duration::ZERO)? {
                    break;
                }
                if handle_event(&mut app, event::read()?) {
                    app.shutdown();
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

/// The files two agents' touched-file sets share (the predicted merge collision).
/// Pure, so the drift pairing is unit-testable without jj.
fn overlapping_files(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|f| b.contains(f)).cloned().collect()
}

/// Parse the LAST RUDDER_DONE block a finished worker echoed (via `rudder done`)
/// out of its PTY scrollback into the completion-note JSON. None if absent/invalid.
fn parse_worker_done_block(output: &str) -> Option<serde_json::Value> {
    const START: &str = "RUDDER_DONE_START";
    const END: &str = "RUDDER_DONE_END";
    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let start = clean.rfind(START)?;
    let after = &clean[start + START.len()..];
    let end = after.find(END)?;
    serde_json::from_str(after[..end].trim()).ok()
}

fn handle_event(app: &mut App, event: Event) -> bool {
    // Any inbound terminal event is a user-visible signal: mark dirty so the
    // next tick re-renders. Resize must redraw too.
    app.mark_dirty();
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
        Event::Key(_) => false,
        Event::Paste(text) => {
            app.handle_paste(text);
            false
        }
        Event::Mouse(mouse) => {
            app.handle_mouse(mouse);
            false
        }
        _ => false,
    }
}

fn initial_selection() -> InitialSelection {
    let cli = cli_selection();
    let config = load_rudder_config();
    let backend = cli
        .backend
        .or_else(|| config.as_ref().and_then(config_backend))
        .unwrap_or(Backend::Claude);
    let cli_model = cli.model.filter(|model| !model.trim().is_empty());
    let should_remember = cli.backend.is_some() || cli_model.is_some();
    let model = cli_model
        .clone()
        .or_else(|| {
            config
                .as_ref()
                .and_then(|config| config_model(config, backend))
        })
        .unwrap_or_else(|| default_model_for(backend).to_string());
    let effort = if cli_model.is_some() {
        default_effort_for(backend, &model)
    } else {
        config
            .as_ref()
            .and_then(|config| config_effort(config, backend))
            .or_else(|| default_effort_for(backend, &model))
    };

    if should_remember {
        let _ = save_model_defaults(backend, &model, effort);
    }

    InitialSelection {
        backend,
        model,
        effort,
    }
}

fn cli_selection() -> CliSelection {
    let mut selection = CliSelection::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => break,
            "--backend" | "-b" => {
                if let Some(value) = args.next() {
                    selection.backend = provider_backend(&value);
                }
            }
            "--model" | "-m" => {
                selection.model = args.next();
            }
            _ if arg.starts_with("--backend=") => {
                selection.backend = provider_backend(&arg["--backend=".len()..]);
            }
            _ if arg.starts_with("--model=") => {
                selection.model = Some(arg["--model=".len()..].to_string());
            }
            _ => {}
        }
    }
    selection
}

#[derive(Default, Debug, Clone)]
struct ModelUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}


#[cfg(unix)]
fn set_private_file_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_file_mode(_path: &Path) {}

