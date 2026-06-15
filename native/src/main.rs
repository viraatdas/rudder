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
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

mod signals;
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
/// Poll ticks per visible spinner frame. The frame counter advances every tick (~33ms),
/// which spins too fast to read; showing each glyph for this many ticks slows it to a
/// calmer ~100ms/frame.
pub(crate) const SPINNER_TICKS_PER_FRAME: usize = 3;
const AGENT_PANE_HINTS: &[&str] = &[
    "j/k move",
    "Enter focus",
    "r rename",
    "v diff",
    "g nest",
    "R review all",
    "m merge",
    "M merge all",
    "o web ui",
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
    /// A single conversational agent the user talks to in the MAIN checkout for a
    /// question or a small self-contained change — NOT a DAG node. Spawned by
    /// default task input (or /ask). Structurally like Main (main checkout, no jj
    /// worktree, bypass-permissions tools), distinct in intent + its own list section.
    OneOff,
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
            Self::OneOff => "one-off",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "execute" | "run" | "task" => Some(Self::Execute),
            "plan" | "planning" => Some(Self::Plan),
            "rudder-plan" | "rudder_plan" | "orchestrate" => Some(Self::RudderPlan),
            "review-all" | "review_all" | "reviewall" => Some(Self::ReviewAll),
            "main" => Some(Self::Main),
            "one-off" | "oneoff" | "ask" => Some(Self::OneOff),
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
    /// Set while a STRUCTURAL plan-rebase is in flight: the orchestrator session has
    /// been resumed with a `build_rebase_request` (a mid-flight pivot), and we are
    /// waiting for its revised DAG. Like `refining` it lets plan detection run even
    /// though a plan is already active (post-approval, nodes launched), but it routes
    /// capture to `evaluate_completed_rebase` (build-forward diff/apply) instead of
    /// `evaluate_completed_plan`. Suppresses `maybe_auto_merge` while set so the zones
    /// stay stable until the diff is applied. Cleared the moment the rebase lands or fails.
    rebasing: bool,
    /// Set alongside `rebasing` when the rebase was triggered on the LIVE INTERACTIVE
    /// conductor (a Claude orchestrator the user is conversing with), e.g. via the
    /// conductor's own `RUDDER_REPLAN` marker. The rebase relaunches that row as a
    /// headless re-decompose, which tears down the interactive PTY; this flag tells the
    /// rebase evaluator to re-spawn the interactive conductor (resuming the same session)
    /// once the diff lands or fails, so the user never loses the conversation.
    rebase_restore_interactive: bool,
    /// When true, a plan node that finishes cleanly (review, no conflict) is merged
    /// automatically so its children unblock and the DAG drains hands-free. Toggled
    /// by `/automerge`. Default OFF: review-before-merge stays the norm.
    auto_merge: bool,
    /// Whether Claude Code's native fast mode is on for NEW agents (toggled by `/fast`).
    /// Display/UX mirror of the persisted `fastMode` config flag; the authoritative
    /// value injected into a worker's `--settings` is read from config at launch.
    /// Claude-only — Codex has no equivalent (its `/fast` just uses low reasoning effort).
    fast_mode: bool,
    /// Agent ids auto-merge has already hit a conflict on; skipped on later passes so
    /// it does not retry (and spam) every tick. The user resolves + merges manually.
    auto_merge_skip: Vec<String>,
    /// Localhost URL of the live web board (http://127.0.0.1:PORT), passed in by the
    /// Node parent via RUDDER_BOARD_URL when the in-process board daemon is running.
    /// None when launched without a board. Surfaced in the agents pane + opened by
    /// `/web` and the `o` key so users can live-monitor and steer from the browser.
    board_url: Option<String>,
    /// While true, a plan has been parsed into `planned_nodes` but is awaiting the
    /// user's APPROVAL gate: nothing launches. Enter approves (clears this and runs
    /// the scheduler); d removes the selected node, or discards the whole plan when
    /// the orchestrator is selected. Set on streaming plan detection; cleared once
    /// the user approves (or discards the plan).
    awaiting_approval: bool,
    /// Whether the orchestrator runs as an INTERACTIVE Claude Code PTY (the default; see
    /// `interactive_orchestrator()`) vs the headless decomposer. Snapshotted from the env
    /// ONCE at construction so render/poll/key paths read a per-App field instead of the
    /// process-global env (which races across parallel tests). Tests set it directly.
    interactive_orchestrator: bool,
    /// True ONLY while a headless planner has finished a turn WITHOUT emitting a DAG (it
    /// asked a clarifying question / needs more detail) and is waiting for the user's
    /// reply. Distinguishes a paused planner from a COMPLETED-and-shipped plan's leftover
    /// Done orchestrator, so a brand-new task typed after a plan finished is NOT mistakenly
    /// routed into refining the old session. Set in evaluate_completed_plan's no-DAG
    /// branches; cleared when a DAG is captured, on approval, and on a fresh plan.
    planner_paused_for_input: bool,
    /// The planner's clarifying questions (parsed from its RUDDER_QUESTIONS block) while it
    /// is paused for input, rendered as a clean numbered prompt in the orchestrator pane.
    /// Set when the planner pauses; cleared on DAG-capture / approval / fresh plan.
    pending_questions: Vec<String>,
    /// Code-level mandatory first question gate. A fresh initial planner may not
    /// capture a `RUDDER_PLAN_TASKS` block until the user has answered one question
    /// round. This makes "asks first" deterministic instead of relying only on the
    /// model prompt. Refine/rebase/reconcile paths are already later turns and do
    /// not use this gate.
    planner_question_round_done: bool,
    /// Tick counter used to run the scheduler on a coarse cadence rather than on
    /// every PTY-byte tick.
    scheduler_tick: u64,
    /// Animation frame for the orchestrator spinner. Advances every poll tick so
    /// the "decomposing the task..." spinner feels alive while the planner runs.
    spinner_frame: usize,
    selected_agent: usize,
    /// Top line offset of the agents-pane list, persisted across frames so the pane
    /// scrolls to follow the selection when there are more rows than fit on screen.
    /// Recomputed each frame in `render_agents` from the selected row + pane height.
    agents_scroll: usize,
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
    /// Agents-pane row -> agent index, harvested from the LAST rendered frame
    /// (render_agents). Mouse clicks resolve through this so hit-testing always
    /// matches what is actually drawn (headers, hints, sections, wrapped rows).
    agent_row_map: Vec<Option<usize>>,
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
    /// Throttle for the web-board steer inbox poll (.rudder/steer/*.json): checked
    /// ~once/sec from poll_agents so a browser "steer" reaches the right agent's PTY.
    last_steer_poll: Instant,
    /// Throttle for the periodic activity-feed heartbeat: emits a one-line "what's
    /// happening" status into .rudder/activity.jsonl every ~45s while work is live.
    last_heartbeat_emit: Instant,
    cloud_workspace: Option<CloudWorkspaceStatus>,
    last_workspace_check: Option<Instant>,
    workspace_status_rx: Option<mpsc::Receiver<Option<CloudWorkspaceStatus>>>,
    workspace_idle_notified: bool,
    task_summary_tx: mpsc::Sender<TaskSummaryResult>,
    task_summary_rx: mpsc::Receiver<TaskSummaryResult>,
    /// BACKSTOP channel: a finished worker that filed no report has a one-shot summarizer
    /// run over its diff; the reconstructed note arrives here. `pending` holds the run ids
    /// with a summarizer in flight so the poll loop never re-spawns one.
    completion_summary_tx: mpsc::Sender<CompletionSummaryResult>,
    completion_summary_rx: mpsc::Receiver<CompletionSummaryResult>,
    completion_summary_pending: HashSet<String>,
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
    /// Extra caution line shown in the confirm modal (e.g. "N uncommitted files
    /// will be auto-committed"). The modal is the single surface for the prompt;
    /// nothing about it goes through the notice line.
    detail: Option<String>,
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
    /// True only for a Claude orchestrator launched as a live interactive PTY. Headless
    /// planners also use `AgentMode::RudderPlan`, so this must not be inferred from
    /// `autosteered` once a plan is captured.
    interactive_orchestrator: bool,
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

/// Result of the completion-note BACKSTOP: a one-shot summarizer reconstructed a report
/// (or not) for a worker that finished without filing one. `note` is the same JSON shape
/// `parse_worker_done_block` yields; `None` when the summarizer failed or had nothing.
#[derive(Debug)]
struct CompletionSummaryResult {
    run_id: String,
    node_id: String,
    note: Option<serde_json::Value>,
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

    /// A one-off conversational agent (its own list section; not a DAG worker).
    pub(crate) fn is_oneoff(&self) -> bool {
        self.mode == AgentMode::OneOff
    }

    /// A "pinned planner" renders in the orchestrator section at the top of the list
    /// (above main + every status bucket), not in a status bucket. The headless
    /// RudderPlan orchestrator is the only such row.
    pub(crate) fn is_pinned_planner(&self) -> bool {
        self.is_orchestrator()
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

/// A unique temp dir for an App built in a test, so test state (DECISIONS.md, .rudder/,
/// RUDDER.md, graph.json) never lands in the real working repo and tests never share a
/// file. The atomic counter makes each `App::new()` in the same test process distinct.
#[cfg(test)]
fn test_default_cwd() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rudder-test-cwd-{}-{n}", std::process::id()))
}

impl App {
    fn new() -> Self {
        // In tests, default to a UNIQUE temp dir, never the real repo: App methods write
        // real files (DECISIONS.md, .rudder/, RUDDER.md, graph.json) keyed on cwd, so a test
        // that forgets to override app.cwd would otherwise pollute the working repo (this is
        // exactly how the repo's DECISIONS.md accumulated dozens of stray depth-cap entries).
        // The per-instance counter keeps tests from sharing a temp file and cross-contaminating.
        #[cfg(test)]
        let cwd = test_default_cwd();
        #[cfg(not(test))]
        let cwd = std::env::current_dir()
            .map(|path| dashboard_root(&path))
            .unwrap_or_else(|_| PathBuf::from("."));
        let selection = initial_selection();
        let agents = if cfg!(test) {
            Vec::new()
        } else {
            load_persisted_agents(&cwd)
        };
        // Restore the ingested-run ledger so a worker handled before the last exit is not
        // re-ingested (or re-summarized) when its Done record reloads. Empty in tests.
        let followups_ingested = if cfg!(test) {
            HashSet::new()
        } else {
            load_ingested_runs(&cwd)
        };
        // Restore the conflict-skip list alongside the auto_merge toggle: with the
        // toggle persisted but the skip list not, a restart's first tick would
        // re-merge a known-conflicted run and re-spawn an uninvited AI resolver.
        let auto_merge_skip = if cfg!(test) {
            Vec::new()
        } else {
            load_auto_merge_skip(&cwd)
        };
        // Restore the auto-expansion depth map so MAX_FOLLOWUP_DEPTH survives a restart.
        let followup_gen = if cfg!(test) {
            HashMap::new()
        } else {
            load_followup_gen(&cwd)
        };
        // Restore the approval-gate queue (queued nodes + gate state) so a mid-plan restart
        // resumes the plan instead of silently losing it. awaiting_approval is restored
        // TOGETHER with the queue, so the scheduler never launches an un-approved plan.
        let restored_queue = if cfg!(test) {
            PlanQueueSnapshot::default()
        } else {
            load_plan_queue(&cwd).unwrap_or_default()
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
        let (completion_summary_tx, completion_summary_rx) = mpsc::channel();
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
            planned_nodes: restored_queue.planned_nodes,
            planned_origin: restored_queue.planned_origin,
            plan_request: restored_queue.plan_request,
            plan_summary: restored_queue.plan_summary,
            refining: false,
            rebasing: false,
            rebase_restore_interactive: false,
            // Restore the persisted /automerge default (tests always start OFF so
            // merge-gating assertions stay deterministic). Losing the toggle on
            // restart silently stalled mid-flight plans at every merge gate.
            auto_merge: if cfg!(test) {
                false
            } else {
                initial_auto_merge()
            },
            fast_mode: if cfg!(test) {
                false
            } else {
                config::fast_mode_enabled()
            },
            auto_merge_skip,
            board_url: if cfg!(test) {
                None
            } else {
                // Always show a web-UI address in the TUI. Prefer the URL the Node
                // parent passes (the real bound port); fall back to the default
                // localhost board port so an address is visible even if the env was
                // not set (the board normally listens on 4774 = DEFAULT_BOARD_PORT).
                Some(
                    env::var("RUDDER_BOARD_URL")
                        .ok()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| "http://127.0.0.1:4774".to_string()),
                )
            },
            awaiting_approval: restored_queue.awaiting_approval,
            interactive_orchestrator: interactive_orchestrator(),
            planner_paused_for_input: false,
            pending_questions: Vec::new(),
            planner_question_round_done: false,
            scheduler_tick: 0,
            spinner_frame: 0,
            selected_agent: 0,
            agents_scroll: 0,
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
            agent_row_map: Vec::new(),
            activity_log: Vec::new(),
            followups_ingested,
            followup_gen,
            surfaced_overlaps: HashSet::new(),
            last_drift_scan: None,
            orch_dag_scroll: 0,
            agents_area: None,
            worker_area: None,
            task_area: None,
            cloud_connected: cloud.connected,
            cloud_runtime: cloud.runtime,
            last_cloud_check: Instant::now(),
            last_steer_poll: Instant::now(),
            last_heartbeat_emit: Instant::now(),
            cloud_workspace: None,
            last_workspace_check: None,
            workspace_status_rx: None,
            workspace_idle_notified: false,
            task_summary_tx,
            task_summary_rx,
            completion_summary_tx,
            completion_summary_rx,
            completion_summary_pending: HashSet::new(),
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
        SPINNER_FRAMES[(self.spinner_frame / SPINNER_TICKS_PER_FRAME) % SPINNER_FRAMES.len()]
    }

    /// True while an orchestrator (a RudderPlan agent) is actively decomposing:
    /// its planner process is Running before approval, or a refine/rebase is in
    /// flight. A post-approval interactive orchestrator is a live conductor, not
    /// a busy planner, so it is intentionally excluded from the spinner cadence.
    ///
    /// This deliberately keys off the planner being ALIVE, NOT off whether a plan
    /// block has parsed yet. Tying it to `extract_rudder_plan_tasks(...).is_err()`
    /// silently diverged from `orchestrator_phase` (which is what actually decides
    /// to draw the spinner): a present-but-empty task block parses to `Ok(empty)`,
    /// so the pane shows Planning while `.is_err()` was false — no redraw fired and
    /// the spinner froze, only advancing a frame per input event. Running-or-refining
    /// can never diverge that way.
    pub(crate) fn has_planning_orchestrator(&self) -> bool {
        let plan_lifecycle_started = self.awaiting_approval
            || !self.planned_nodes.is_empty()
            || self.agents.iter().any(|run| run.node_id.is_some());
        self.refining
            || self.rebasing
            || self.agents.iter().any(|run| {
                run.mode == AgentMode::RudderPlan
                    && run.status == AgentStatus::Running
                    && (!self.is_interactive_orchestrator_run(run)
                        || self.awaiting_approval
                        || !plan_lifecycle_started)
            })
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

    fn selected_uses_headless_orchestrator_chat(&self) -> bool {
        self.agents
            .get(self.selected_agent)
            .is_some_and(|run| run.is_orchestrator() && !self.is_interactive_orchestrator_run(run))
    }

    pub(crate) fn is_interactive_orchestrator_run(&self, run: &AgentRun) -> bool {
        run.is_orchestrator()
            && !run.reconcile_planner
            && run.backend == Backend::Claude
            && run.interactive_orchestrator
    }

    fn has_running_interactive_orchestrator(&self) -> bool {
        self.agents.iter().any(|run| {
            self.is_interactive_orchestrator_run(run) && run.status == AgentStatus::Running
        })
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
        if self.planner_paused_for_input {
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
        // Choosing a model explicitly exits fast mode (matches "/model switches back").
        // `/fast` re-arms it AFTER calling this. Persist so a launched worker's
        // `--settings` no longer carries `fastMode`.
        if self.fast_mode {
            self.fast_mode = false;
            let _ = config::save_fast_mode(false);
        }
        save_model_defaults(self.backend, &self.model, self.effort)
            .err()
            .map(|error| format!("config warning: {error}"))
    }

    /// The single quit gate: quitting with agents still running needs a second
    /// press (Ctrl+C, q, or y) to confirm. Every quit key — Ctrl+C and the
    /// pane-local `q`s — routes through here so none can skip the guard.
    fn confirm_or_quit(&mut self) -> bool {
        let running = self
            .agents
            .iter()
            .filter(|a| !a.is_main() && a.terminal.is_some() && a.status == AgentStatus::Running)
            .count();
        if running == 0 || self.quit_confirm_pending {
            return true;
        }
        self.quit_confirm_pending = true;
        self.notice = Some(format!(
            "{running} agent{} still running. press q / Ctrl+C again (or y) to quit; any other key cancels. Claude agents auto-resume on next rudder.",
            if running == 1 { "" } else { "s" }
        ));
        false
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.note_user_activity();
        if self.rename_input.is_some() {
            self.handle_rename_key(key);
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.confirm_or_quit();
        }
        // Any other key dismisses the pending quit confirmation. A repeated quit
        // key (q, like the y the notice offers) confirms; Ctrl+C re-enters
        // confirm_or_quit above and confirms there.
        if self.quit_confirm_pending {
            if matches!(
                key.code,
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('q')
            ) {
                return true;
            }
            self.quit_confirm_pending = false;
            self.notice = Some("quit cancelled".to_string());
            // fall through and let the key be handled normally
        }

        // Esc always dismisses the transient notice line (without being consumed:
        // its pane-specific meaning, like exiting the diff view, still applies).
        if key.code == KeyCode::Esc && self.notice.is_some() {
            self.notice = None;
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

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('w')
            && (self.focus != FocusPane::Task || self.task_input.is_empty())
        {
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
            KeyCode::Char('q') => return self.confirm_or_quit(),
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
            KeyCode::Char('q') => return self.confirm_or_quit(),
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
            KeyCode::Char('q') => return self.confirm_or_quit(),
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
            KeyCode::Char('o') => self.open_web_ui(),
            _ => {}
        }
        false
    }

    fn toggle_nest_view(&mut self) {
        self.nest_view = !self.nest_view;
        self.dirty = true;
    }

    /// Rotate pane focus between Agents and Worker. Used by the Ctrl+W leader
    /// Tab/BackTab cycle.
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

        // Orchestrator pane = a CHAT with the planner. For HEADLESS planners
        // typing composes a follow-up: Enter-with-text refines, Enter-on-empty approves;
        // we intercept before the PTY-forward path because writing to a -p process is
        // useless. For the INTERACTIVE Claude orchestrator the pane IS a live Claude PTY,
        // so keys forward straight to it and approval/refine happen via RUDDER.md markers.
        if self.selected_uses_headless_orchestrator_chat() {
            return self.handle_orchestrator_chat_key(key);
        }

        self.worker_selection = None;
        if self.selected_terminal_mut().is_none() {
            match key.code {
                KeyCode::Char('q') => return self.confirm_or_quit(),
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
                KeyCode::Char('q') => return self.confirm_or_quit(),
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
                    if self.rebasing {
                        self.notice = Some(
                            "still rebasing — applying the new direction to the live plan"
                                .to_string(),
                        );
                    } else if self.refining {
                        self.notice =
                            Some("still refining — the updated plan is on its way".to_string());
                    } else if self.awaiting_approval {
                        self.approve_planned_queue();
                    } else {
                        self.notice = Some(
                            "type a message to refine the plan, or wait for it to finish"
                                .to_string(),
                        );
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
                    FocusPane::Agents => "copied selection".to_string(),
                    FocusPane::Task => "copied task selection".to_string(),
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
        // Selection order MUST match the rendered order, which differs by view: the nest
        // view walks the dependency tree globally (crossing status buckets), while the
        // default view nests within each status section. Using the wrong one makes j/k
        // land on a different row than the highlighted one.
        if self.nest_view {
            nest_agent_order(&self.agents)
        } else {
            visible_agent_indices(&self.agents)
        }
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
            .filter(|run| !run.is_oneoff())
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
        let mut command = agent_command(
            Backend::Codex,
            REVIEW_ALL_MODEL,
            Some(REVIEW_ALL_EFFORT),
            &prompt,
            AgentMode::ReviewAll,
            session_id.as_deref(),
        );
        signals::augment_worker_command(
            &mut command,
            Backend::Codex,
            AgentMode::ReviewAll,
            &worktree.id,
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
                self.notice = Some("type /model, /main|/m, /goal, /usage, or /cloud".to_string());
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

        if let Some(agents_area) = self
            .agents_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        {
            self.worker_selection = None;
            self.task_selection = None;
            self.handle_agents_mouse(mouse, agents_area);
            return;
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
            self.orch_dag_scroll.saturating_add(delta.unsigned_abs())
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
        self.capture_shared_context_from_input(input);
        if self.handle_command(&input) {
            return;
        }
        self.notice = None;

        // REFINE / CONVERSE: while a plan is parsed but not yet approved, a typed
        // message is feedback to the orchestrator, not a new node. Interactive
        // orchestrators are live Claude sessions, so send the text into that PTY and
        // let it update RUDDER.md; headless planners still use the old refine path.
        if self.awaiting_approval {
            if self.send_to_interactive_orchestrator(input) {
                return;
            }
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

        // Once a DAG has launched, keep using the live orchestrator as the
        // high-level conversation surface even after every worker has merged. That
        // lets the user ask for a retro, status, or a follow-up without losing the
        // conductor context.
        if self.agents.iter().any(|run| run.node_id.is_some())
            && self.send_to_interactive_orchestrator(input)
        {
            return;
        }

        // CONDUCTING: a plan is already active and approved. Prefer the live
        // high-level orchestrator when it exists so the user can talk to the
        // conductor and let it decide whether to inspect, explain, add work, re-plan,
        // merge, stop, or re-goal workers through RUDDER_* markers. Headless plans
        // fall back to the local classifier.
        if self.plan_is_active() {
            if self.send_to_interactive_orchestrator(input) {
                return;
            }
            if self.classify_new_direction(&input) {
                self.start_plan_rebase(&input);
            } else {
                self.reconcile_injection(&input);
            }
            return;
        }

        // The planner ran but produced NO DAG (it asked a clarifying question, or needs
        // more detail). It is left resumable; RESUME the same session with this message so
        // the planning conversation continues, instead of starting a fresh planner that
        // loses all prior context.
        if self.planner_awaiting_input() {
            self.refine_plan(&input);
            return;
        }

        // Default first task (no active plan): hand it to the orchestrator, which
        // plans and implements it (spawning + merging workers as needed). No local
        // classifier decides one-off vs DAG — the orchestrator is the default, and
        // the one-off conversational agent is the explicit escape hatch via `/ask`.
        self.start_rudder_plan_task(input);
    }

    fn send_to_interactive_orchestrator(&mut self, input: &str) -> bool {
        let Some(index) = self.agents.iter().position(|run| {
            self.is_interactive_orchestrator_run(run)
                && run.status == AgentStatus::Running
                && run.terminal.is_some()
        }) else {
            return false;
        };
        let write_result = {
            let Some(terminal) = self.agents[index].terminal.as_mut() else {
                return false;
            };
            terminal.write_input(format!("{input}\r").as_bytes())
        };
        if let Err(error) = write_result {
            self.notice = Some(format!("could not send to orchestrator: {error}"));
            return false;
        }
        let now = now_stamp();
        if let Some(run) = self.agents.get_mut(index) {
            run.current_prompt = input.to_string();
            run.turns.push(AgentTurn {
                ts: now.clone(),
                prompt: input.to_string(),
                source: "user".to_string(),
            });
            run.last_user_input_at = now;
            run.last_worker_input_at = Some(Instant::now());
            run.last_output_at = Instant::now();
            run.ready_since = None;
            run.needs_permission = false;
            run.permission_notified = false;
            run.needs_user_input = false;
            run.user_input_notified = false;
            let _ = save_native_run_record(&self.cwd, run);
        }
        self.selected_agent = index;
        self.delete_pending = None;
        self.notice = Some("sent to orchestrator".to_string());
        self.dirty = true;
        true
    }

    fn capture_shared_context_from_input(&mut self, input: &str) {
        if capture_shared_context_from_user_input(&self.cwd, input).unwrap_or(false) {
            let _ = sync_shared_context_surfaces(&self.cwd, &self.agents, None);
        }
    }

    /// True when a planner ran and EXITED without producing a DAG (it asked a clarifying
    /// question or needs more detail) and is waiting for the user's reply: a pinned
    /// orchestrator that is no longer Running, still has its resumable session, with no
    /// queued nodes, no approval gate, and no refine/rebase in flight. A typed message in
    /// this state RESUMES the planner (refine_plan) rather than starting a fresh one.
    fn planner_awaiting_input(&self) -> bool {
        // The flag is the discriminator: it is set ONLY when a planner finished a turn
        // without a DAG (asked a question), and cleared on DAG-capture / approval / fresh
        // plan. Without it, a COMPLETED plan's leftover Done orchestrator (empty queue, no
        // gate, all workers merged) would also look "paused" and hijack a new task.
        if !self.planner_paused_for_input {
            return false;
        }
        if self.awaiting_approval
            || self.refining
            || self.rebasing
            || !self.planned_nodes.is_empty()
        {
            return false;
        }
        let has_paused_planner = self.agents.iter().any(|run| {
            run.is_orchestrator()
                && run.status != AgentStatus::Running
                && run
                    .session_id
                    .as_deref()
                    .is_some_and(|sid| !sid.trim().is_empty())
        });
        // No in-flight plan workers (that would be a conducting plan, routed elsewhere).
        let no_live_plan_workers = !self
            .agents
            .iter()
            .any(|run| run.node_id.is_some() && run.status != AgentStatus::Merged);
        has_paused_planner && no_live_plan_workers
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

    /// Every live plan node title: queued (TODO) nodes plus in-flight (not-yet-merged)
    /// plan agents. Feeds the structural-vs-additive classifier's title-overlap heuristic.
    fn plan_node_titles(&self) -> Vec<String> {
        let mut titles: Vec<String> = self
            .planned_nodes
            .iter()
            .map(|node| node.title.clone())
            .collect();
        for run in &self.agents {
            if run.node_id.is_some() && run.status != AgentStatus::Merged {
                titles.push(run.task_summary.clone());
            }
        }
        titles
    }

    /// Classify a typed message while CONDUCTING: STRUCTURAL pivot (true → rebase the
    /// whole plan) vs ADDITIVE request (false → reconcile one node). Thin wrapper over
    /// the pure `is_structural_direction` (replacement verbs OR majority title-overlap)
    /// that sources the live node titles. Autonomous + logged; never a confirm gate.
    fn classify_new_direction(&self, input: &str) -> bool {
        is_structural_direction(input, &self.plan_node_titles())
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
            // Skip Merged (already a satisfied/dangling ref) AND Failed/Stopped: a dead
            // node id must never be advertised as a hard-dep target, or a reconcile/rebase
            // node hard-linked to it would deadlock on arrival (is_ready never satisfies a
            // hard dep whose node is not Merged).
            if matches!(
                run.status,
                AgentStatus::Merged | AgentStatus::Failed | AgentStatus::Stopped
            ) {
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
                TerminalCommand::with_args(claude_program(), args)
                    .with_env("CLAUDE_CODE_NO_FLICKER", "0")
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
            interactive_orchestrator: false,
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
        // Lead the launch prompt with the canonical Objective + Done-when block so
        // every spawned agent has a clear objective and verifiable stopping condition.
        // Idempotent: an already objective-formatted worker prompt is normalized and kept.
        let goal_prompt = manual_goal_prompt(input);
        let mut command = agent_command(
            backend,
            &model,
            effort,
            &goal_prompt,
            AgentMode::Execute,
            session_id.as_deref(),
        );
        // ROBUST completion channel: tell this worker's `rudder done` EXACTLY where to
        // drop its machine-readable note. The orchestrator reads this same file when the
        // worker finishes, so the report survives no matter how Claude or Codex render
        // tool output in their interactive TUI (which would otherwise box/truncate/wrap
        // the echoed block in the PTY). The path lives inside the worker's own gitignored
        // .rudder dir: writable under either backend's (here unsandboxed) Bash tool, and
        // never snapshotted into the merge. The env propagates to the `rudder done`
        // subprocess the agent spawns. The PTY-scrape stays as a fallback.
        if let Some(id) = node_id.as_deref() {
            let done_file = worker_done_file(&worktree.path, id);
            command = command.with_env("RUDDER_DONE_FILE", done_file.to_string_lossy().to_string());
        }
        // Official completion signal: wire the backend's own Stop hook (Claude) /
        // notify (Codex) so it deterministically reports turn-end, instead of
        // relying on the PTY-scrape heuristics. Keyed by the run id the poll loop
        // reads. See signals.rs.
        signals::augment_worker_command(&mut command, backend, AgentMode::Execute, &worktree.id);
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
            interactive_orchestrator: false,
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
            interactive_orchestrator: false,
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
        // A fresh plan supersedes any paused planner.
        self.planner_paused_for_input = false;
        self.pending_questions.clear();
        self.planner_question_round_done = false;
        // AT-MOST-ONE ORCHESTRATOR: a brand-new plan supersedes any prior pinned
        // planner. Retire stale orchestrator rows (and their on-disk records) before
        // pushing the new one so completed plans never stack as phantom orchestrators
        // in the agent pane (`is_orchestrator()` is true for any RudderPlan row).
        // refine/rebase reuse the existing orchestrator in place and never reach here;
        // this path only runs for a genuinely fresh plan (`!plan_is_active()`).
        let stale_orchestrators: Vec<String> = self
            .agents
            .iter()
            .filter(|run| run.is_orchestrator())
            .map(|run| run.id.clone())
            .collect();
        for id in stale_orchestrators {
            self.retire_planner_row(&id);
        }
        // Remember the original request so the refine loop can re-plan against it
        // (each refinement layers the user's feedback on top of this, not on top of
        // the previous composite prompt).
        self.plan_request = input.to_string();
        let backend = self.backend;
        let interactive_planner = self.interactive_orchestrator && backend == Backend::Claude;
        let model = self.model.clone();
        // Decomposing a task into a node DAG does not need max reasoning. Cap the
        // planner's effort so plan mode stays responsive even when the dashboard
        // default is a heavy model at high effort. The model itself is unchanged.
        let base_effort = self.effort;
        let effort = match base_effort {
            Some(EffortLevel::High) | Some(EffortLevel::XHigh) | Some(EffortLevel::Max) => {
                Some(EffortLevel::Medium)
            }
            other => other,
        };
        let session_id = mint_session_id_for(backend);
        let command = agent_command_with_orchestrator_mode(
            backend,
            &model,
            effort,
            input,
            AgentMode::RudderPlan,
            session_id.as_deref(),
            interactive_planner,
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
            interactive_orchestrator: interactive_planner,
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

        // INTERACTIVE orchestrator: it never exits and presents its DAG via RUDDER.md, so
        // it is NOT autosteered (the headless completed-plan capture must not fire).
        // Clear stale one-shot markers before spawn so a RUDDER_* action left in
        // RUDDER.md while no orchestrator was running cannot fire under this new,
        // unrelated planner. Generate project-level Claude skills before spawn so
        // dashboard actions are also available inside this Claude Code session.
        if interactive_planner {
            run.autosteered = false;
            if let Err(error) = clear_orchestrator_plan_markers(&self.cwd) {
                self.notice = Some(format!("orchestrator plan cleanup warning: {error}"));
            }
            if let Err(error) = ensure_orchestrator_skills(&self.cwd) {
                self.notice = Some(format!("orchestrator skills warning: {error}"));
            }
        }

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
        // A refine/rebase is already in flight: the revised plan is on its way. Relaunching
        // the orchestrator now would kill the in-flight planner and race its capture, so
        // hold (mirrors the empty-Enter and approve_planned_queue guards).
        if self.refining || self.rebasing {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
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
        // Pick the framing: if the planner PAUSED to ask a clarifying question, the user's
        // message is an ANSWER (continue planning), not feedback on a shown plan. Using the
        // refine framing here ("the plan you produced") would confuse the model since no
        // plan exists yet, and its "do not ask questions" would block further clarification.
        let answering_clarification = self.planner_paused_for_input;
        let followup = if answering_clarification {
            build_clarification_answer_followup(feedback)
        } else {
            build_refine_followup(feedback)
        };
        let command = match &session {
            Some(sid) => rudder_plan_refine_command(backend, &model, effort, &followup, sid),
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
            if answering_clarification {
                self.planner_question_round_done = true;
            }
            self.notice = Some("refining the plan with your feedback…".to_string());
        } else {
            // The planner could not be relaunched: drop back to the existing plan so
            // the user is not stuck (they can still approve it or try again).
            self.refining = false;
            self.notice =
                Some("could not relaunch the planner; the current plan still stands".to_string());
        }
    }

    /// STRUCTURAL pivot (D): re-decompose the WHOLE plan against the live zones instead
    /// of folding in one node. Reuses the refine machinery (resume the orchestrator
    /// session) but with a `build_rebase_request` that states which work has MERGED
    /// (baseline — build forward), which is RUNNING (keep/re-goal/stop), and which is
    /// TODO (replace freely). Sets `rebasing` so `maybe_detect_plan_ready` routes the
    /// revised DAG to `evaluate_completed_rebase` (a build-forward diff/apply), and
    /// `maybe_auto_merge` is suppressed until the diff lands. Autonomous + logged.
    fn start_plan_rebase(&mut self, input: &str) {
        // A refine/rebase is already in flight: relaunching the orchestrator now would kill
        // the in-flight planner and race its capture (same guard refine_plan has).
        if self.refining || self.rebasing {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
        let Some(index) = self.agents.iter().position(|run| run.is_orchestrator()) else {
            // No orchestrator session to resume (e.g. a daemon-launched plan): a
            // structural change with no planner to re-decompose against falls back to
            // a fresh plan from the new direction.
            self.start_rudder_plan_task(input);
            return;
        };
        let (backend, model, effort, session, was_interactive) = {
            let run = &self.agents[index];
            (
                run.backend,
                run.model.clone(),
                run.effort,
                run.session_id.clone().filter(|s| !s.trim().is_empty()),
                run.interactive_orchestrator,
            )
        };
        // Relaunching the orchestrator below replaces the live interactive PTY with a
        // headless re-decompose. If this WAS the interactive conductor (e.g. it emitted
        // RUDDER_REPLAN itself), remember to re-spawn it once the rebase resolves so the
        // user keeps the conversation. Claude-only: Codex orchestrators are headless.
        self.rebase_restore_interactive = was_interactive && backend == Backend::Claude;
        let request = build_rebase_request(
            &self.rebase_zone_merged(),
            &self.rebase_zone_running(),
            &self.current_plan_outline(),
            input,
        );
        // Resume the orchestrator's session when we have one (it remembers the prior
        // plan + reasoning); otherwise fall back to a fresh decompose carrying the
        // full zone context in the prompt.
        let resume = session.is_some();
        let command = match &session {
            Some(sid) => rudder_plan_refine_command(backend, &model, effort, &request, sid),
            None => agent_command(
                backend,
                &model,
                effort,
                &request,
                AgentMode::RudderPlan,
                mint_session_id_for(backend).as_deref(),
            ),
        };
        // Mark the rebase in flight BEFORE relaunch so the poll loop routes the revised
        // block to evaluate_completed_rebase and holds auto-merge steady.
        self.rebasing = true;
        if let Some(run) = self.agents.get_mut(index) {
            if resume {
                if let Some(stream) = run.plan_stream.as_mut() {
                    stream.begin_user_turn(input);
                    stream.rebind_stream();
                }
            } else {
                run.plan_stream = Some(PlanStreamState::new());
            }
        }
        if self.relaunch_orchestrator_with(index, command, input) {
            self.push_activity(format!("rebasing the plan: {}", short_task(input)));
        } else {
            // Could not relaunch: drop back to the current plan so the fleet keeps
            // running. Nothing was changed; the rebase is simply abandoned (the old
            // interactive session was never replaced), so there is nothing to restore.
            self.rebasing = false;
            self.rebase_restore_interactive = false;
            self.notice = Some(
                "could not relaunch the planner to rebase; the current plan still stands"
                    .to_string(),
            );
        }
    }

    /// The MERGED zone for a rebase request: already-landed plan nodes (the immutable
    /// build-forward baseline). One `- id [title]` line each.
    fn rebase_zone_merged(&self) -> String {
        let lines: Vec<String> = self
            .agents
            .iter()
            .filter(|run| run.status == AgentStatus::Merged)
            .filter_map(|run| {
                run.node_id
                    .as_ref()
                    .map(|id| format!("- {id} [{}]", run.task_summary))
            })
            .collect();
        if lines.is_empty() {
            "(nothing merged yet)".to_string()
        } else {
            lines.join("\n")
        }
    }

    /// The RUNNING zone for a rebase request: plan agents in flight (carry a node id,
    /// not main, not merged/failed/stopped). One `- id [title]` line each.
    fn rebase_zone_running(&self) -> String {
        let lines: Vec<String> = self
            .agents
            .iter()
            .filter(|run| {
                run.node_id.is_some()
                    && !run.is_main()
                    && matches!(run.status, AgentStatus::Running | AgentStatus::Done)
            })
            .filter_map(|run| {
                run.node_id
                    .as_ref()
                    .map(|id| format!("- {id} [{}]", run.task_summary))
            })
            .collect();
        if lines.is_empty() {
            "(nothing running yet)".to_string()
        } else {
            lines.join("\n")
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
                run.interactive_orchestrator = false;
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

    /// On startup, reconcile in-flight runs persisted as Running whose work is actually
    /// FINISHED, so a rudder restart UNFREEZES a stuck board instead of resurrecting the
    /// dead processes in their stuck state. The motivating case: a merge-conflict resolver
    /// that resolved cleanly but, under an older binary, never got its completion signal
    /// wired — so it was persisted `Running` forever and its node never merged, blocking
    /// every child. Here, any resolver with no live PTY whose jj workspace has no unresolved
    /// conflicts is marked Done; the next poll tick's finalize_merge_resolvers then merges it
    /// and unblocks its children. Runs BEFORE restore_running_agents so these finished
    /// resolvers are not pointlessly resumed as live claude sessions.
    fn reconcile_orphaned_runs(&mut self) {
        let mut reconciled = 0usize;
        for run in self.agents.iter_mut() {
            if run.terminal.is_some() || run.status != AgentStatus::Running {
                continue;
            }
            // A merge resolver whose checkout is conflict-free already did its job.
            if run.merge_resolver && jj_unresolved_conflicts(&run.cwd).is_empty() {
                mark_run_done(run);
                let _ = save_native_run_record(&self.cwd, run);
                reconciled += 1;
            }
        }
        if reconciled > 0 {
            self.notice = Some(format!(
                "reconciled {reconciled} finished merge(s) from last session"
            ));
            // finalize_merge_resolvers (poll loop) merges these and unblocks children; nudge
            // the scheduler + board so the unfreeze is visible immediately on restart.
            self.run_scheduler();
            self.mirror_graph();
            self.dirty = true;
        }
    }

    fn restore_running_agents(&mut self) {
        let snapshot: Vec<(usize, MigratedAgent)> = self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(idx, run)| {
                // Never resume a transient reconcile planner as a background orchestrator
                // (load_persisted_agents already drops these; this is defense in depth).
                if run.terminal.is_some()
                    || run.status != AgentStatus::Running
                    || run.reconcile_planner
                {
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
        let mut command = if !entry.session_id.is_empty() && run.backend == Backend::Claude {
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
        // RE-WIRE the completion hooks on resume. The per-run hook config file from
        // the original launch persists, so worker_has_config() is true and the poll
        // loop WAITS for the official signal — a resumed process launched without
        // --settings / notify could never write it and would sit "running" forever.
        // (Same trap the conflict-resolver respawn fixed in e027f32.)
        signals::augment_worker_command(&mut command, run.backend, run.mode, &run.id);
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
                // The resume spawn failed: do NOT leave the agent stuck in Running with no
                // terminal forever (it would consume a parallelism slot and deadlock hard
                // dependents silently). Mark it Failed so the slot frees and the
                // "blocked by a FAILED dependency" notice can fire for its dependents.
                run.status = AgentStatus::Failed;
                run.completed_at = Some(Instant::now());
                run.last_error = Some(error.to_string());
                let _ = save_native_run_record(&self.cwd, run);
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
                // Re-spawning a stopped main starts a FRESH session: the old session id
                // was already consumed, and `claude --session-id` is a NEW-session flag
                // (not --resume), so reusing it fails to launch. Always mint a new id,
                // matching restart_selected_agent.
                mint_session_id_for(run.backend),
            )
        };
        let mut command = agent_command(
            backend,
            &model,
            effort,
            &bootstrap,
            AgentMode::Main,
            session_id.as_deref(),
        );
        signals::augment_worker_command(
            &mut command,
            backend,
            AgentMode::Main,
            &self.agents[main_index].id.clone(),
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

    /// Spawn a ONE-OFF agent: a single conversational agent in the MAIN checkout (no jj
    /// worktree, no DAG node) that the user talks to for a question or a small change. It
    /// can edit the working tree directly. Selected + focused so the user can converse
    /// immediately (keys forward to its PTY via the normal worker path).
    fn start_oneoff_task(&mut self, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        let backend = self.backend;
        let model = self.model.clone();
        let effort = self.effort;
        let session_id = mint_session_id_for(backend);
        let mut run = create_oneoff_agent(&self.cwd, backend, &model, effort, input);
        let run_id = run.id.clone();
        let mut command = agent_command(
            backend,
            &model,
            effort,
            input,
            AgentMode::OneOff,
            session_id.as_deref(),
        );
        signals::augment_worker_command(&mut command, backend, AgentMode::OneOff, &run_id);
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.session_id = session_id;
                run.last_output_at = Instant::now();
                self.notice = Some(
                    "one-off agent — talk to it; it edits the main checkout directly".to_string(),
                );
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("one-off launch failed: {error}"));
            }
        }
        let _ = save_native_run_record(&self.cwd, &run);
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.focus = FocusPane::Worker;
        self.worker_view = WorkerView::Terminal;
        self.dirty = true;
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
        let orchestrator_interactive = run.mode == AgentMode::RudderPlan
            && run.backend == Backend::Claude
            && run.interactive_orchestrator;
        let mut command = agent_command_with_orchestrator_mode(
            run.backend,
            &run.model,
            run.effort,
            &prompt,
            run.mode,
            session_id.as_deref(),
            orchestrator_interactive,
        );
        signals::augment_worker_command(&mut command, run.backend, run.mode, &run.id);
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
                run.autosteered = matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan)
                    && !orchestrator_interactive;
                run.interactive_orchestrator = orchestrator_interactive;
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
                // Explicit alias for the default: hand the task to the orchestrator.
                // Plain task input does the same thing now.
                let rest = command_rest(input, "/plan").trim();
                if rest.is_empty() {
                    self.notice = Some(
                        "usage: /plan <task> — same as plain input: the orchestrator plans + implements it (use /ask for a one-off)"
                            .to_string(),
                    );
                } else {
                    self.start_rudder_plan_task(rest);
                }
                true
            }
            Some("/ask") => {
                // The escape hatch from the orchestrator default: a one-off
                // conversational agent in the main checkout, no DAG.
                let rest = command_rest(input, "/ask").trim();
                if rest.is_empty() {
                    self.notice = Some(
                        "usage: /ask <question or small change> — one-off agent in the main checkout, no DAG (plain input goes to the orchestrator)"
                            .to_string(),
                    );
                } else {
                    self.start_oneoff_task(rest);
                }
                true
            }
            Some("/share") => {
                // Durable, gitignored local context for all Rudder agents. Use this for
                // API tokens, private URLs, account ids, env details, and anything else
                // the fleet must keep after model compaction.
                let rest = command_rest(input, "/share").trim();
                if rest.is_empty() {
                    self.notice = Some(
                        "usage: /share <tokens, env vars, private URLs, or other local context>"
                            .to_string(),
                    );
                } else {
                    match append_shared_context(&self.cwd, "task bar /share", rest) {
                        Ok(_) => {
                            let _ = sync_shared_context_surfaces(&self.cwd, &self.agents, None);
                            self.notice = Some(
                                "shared context saved to RUDDER_SHARED.md for all agents"
                                    .to_string(),
                            );
                        }
                        Err(error) => {
                            self.notice = Some(format!("share failed: {error}"));
                        }
                    }
                }
                true
            }
            Some("/help") => {
                self.notice = Some(
                    "panes: Option-1/2/3 or ^W · keys: j/k select · Enter focus · v diff · m merge · M merge all · R review all · g nest · o web ui · x stop · dd delete · P model — commands: /model /fast /automerge /merge-all /review-all /main /ask /plan /share /usage /goal /cloud /web — DAG node ids (n0, n1…) match the agent rows and the worker pane title"
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
                let raw = command_rest(input, "/goal");
                self.forward_slash_command_to_focused_agent("/goal", raw);
                true
            }
            Some("/merge-all") => {
                self.request_merge_all_ready();
                true
            }
            Some("/automerge") => {
                self.auto_merge = !self.auto_merge;
                // Persist the toggle so the next session starts in the same mode; a
                // forgotten OFF default used to stall restarted plans at merge gates.
                let _ = save_auto_merge(self.auto_merge);
                if self.auto_merge {
                    // Do NOT blanket-clear auto_merge_skip here: a node parked there by the
                    // conflict-resolver (resolver finished with conflicts still left) would
                    // be un-skipped and immediately re-merged -> re-conflict -> re-spawn a
                    // resolver every tick, the exact stacking loop the skip prevents. Skip
                    // entries are already cleared per-id on a successful merge / re-goal.
                    self.notice = Some(
                        "auto-merge ON (saved as default): clean finished nodes merge themselves and unblock children; conflicts still pause for you".to_string(),
                    );
                    self.maybe_auto_merge();
                } else {
                    self.notice = Some(
                        "auto-merge OFF (saved as default): merge finished nodes yourself with m / M".to_string(),
                    );
                }
                true
            }
            Some("/sync") => {
                // Retired: jj keeps node workspaces current automatically, so manual
                // worktree sync no longer fits the orchestrator paradigm.
                self.notice = Some(
                    "sync is retired; jj keeps node workspaces current automatically".to_string(),
                );
                true
            }
            Some("/review-all") => {
                self.review_all_ready();
                true
            }
            Some("/fast") => {
                // Match what each tool ITSELF calls fast — do not invent a preset.
                //
                // Claude Code: fast mode is NATIVE — the `fastMode` settings flag, which
                // runs Opus on an accelerated (higher-cost) API config. Same model, same
                // reasoning effort, lower latency — it is NOT an effort downgrade. So we
                // select Opus (the only family fast mode supports) WITHOUT touching effort
                // and persist the flag; it is injected into each new worker's `--settings`
                // (see signals::claude_settings_json). `/fast` toggles it on/off.
                //
                // Codex: has no native fast mode. Its only speed lever is reasoning effort,
                // so the faithful equivalent is the flagship at LOW effort.
                let backend = self.backend;
                match backend {
                    Backend::Claude => {
                        if self.fast_mode {
                            self.fast_mode = false;
                            let _ = config::save_fast_mode(false);
                            self.notice = Some(format!(
                                "fast mode OFF · {} {} keeps its effort · running agents unaffected",
                                self.backend.as_str(),
                                self.model
                            ));
                        } else {
                            // Fast mode only runs on Opus; select it but leave effort as-is.
                            let model = fast_model_for(backend).to_string();
                            let warning = self.set_model_defaults(backend, model, self.effort);
                            self.fast_mode = true;
                            let _ = config::save_fast_mode(true);
                            self.notice = warning.or_else(|| {
                                Some(format!(
                                    "fast mode ON · {} {} ({}) for NEW agents · native Opus fast mode, full reasoning · /fast again to turn off",
                                    self.backend.as_str(),
                                    self.model,
                                    effort_label(self.effort)
                                ))
                            });
                        }
                    }
                    Backend::Codex => {
                        let model = fast_model_for(backend).to_string();
                        let warning = self.set_model_defaults(backend, model, Some(EffortLevel::Low));
                        self.notice = warning.or_else(|| {
                            Some(format!(
                                "fast mode: {} {} (low effort) for NEW agents · codex has no native fast mode, so this lowers reasoning effort · /model switches back",
                                self.backend.as_str(),
                                self.model
                            ))
                        });
                    }
                }
                true
            }
            Some("/web") | Some("/ui") | Some("/board") => {
                self.open_web_ui();
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
        // Inserting at 0 shifts every index under the rendered rows; drop the click map
        // so a same-tick click resolves to nothing instead of a neighbor.
        self.agent_row_map.clear();
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
        // Cap the argument for /goal so a long/pasted objective never trips the backend's
        // "Goal condition is limited to 4000 characters" rejection; other commands pass through.
        let arg = slash_command_arg(command, rest);
        let payload = if arg.is_empty() {
            format!("{command}\r")
        } else {
            format!("{command} {arg}\r")
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
            interactive_orchestrator: false,
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

        // REBASE: a structural pivot resumed the orchestrator WHILE a plan is active
        // (post-approval, nodes launched), so its revised block must be captured from
        // streaming output regardless of the initial-plan guard below (which would bail
        // on the non-empty queue). Detected before that guard, like reconcile.
        if self.rebasing {
            let index = self.agents.iter().position(|run| {
                run.mode == AgentMode::RudderPlan
                    && !run.reconcile_planner
                    && run.autosteered
                    && extract_rudder_plan_tasks(&rudder_plan_output_for_run(run)).is_ok()
            });
            if let Some(index) = index {
                self.evaluate_completed_rebase(index);
            }
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
        // Mandatory first question gate: do not capture a first-turn DAG from
        // streaming output. Let the planner process finish, then
        // `evaluate_completed_plan` pauses for the required user answer. This
        // prevents a model that ignored the prompt from slipping a plan through
        // the early-capture path.
        if !self.refining && !self.planner_question_round_done {
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
    /// INTERACTIVE orchestrator capture: read the DAG the orchestrator wrote to
    /// its plan file and present it at the approval gate, reusing the SAME
    /// planned_nodes/awaiting_approval machinery as the headless flow. The per-run
    /// `interactive_orchestrator` flag is authoritative for persisted rows; the App
    /// env setting only chooses the mode for fresh launches.
    fn maybe_capture_orchestrator_plan(&mut self) {
        if self.refining || self.rebasing || self.awaiting_approval {
            return;
        }
        // Only capture a FRESH plan: nothing pending and no workers launched yet.
        if !self.planned_nodes.is_empty() || self.agents.iter().any(|run| run.node_id.is_some()) {
            return;
        }
        if !self.has_running_interactive_orchestrator() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(orchestrator_plan_path(&self.cwd)) else {
            return;
        };
        let tasks = match extract_rudder_plan_tasks(&text) {
            Ok(tasks) if !tasks.is_empty() => tasks,
            _ => return,
        };
        let nodes: Vec<PlannedNode> = tasks.iter().map(PlannedNode::from_task).collect();
        let count = nodes.len();
        self.planned_nodes = nodes;
        if !self.plan_request.trim().is_empty() {
            self.planned_origin = self.plan_request.clone();
        }
        self.plan_summary = extract_rudder_plan_summary(&text);
        self.planner_paused_for_input = false;
        self.awaiting_approval = true;
        self.persist_plan_queue();
        self.notice = Some(format!(
            "orchestrator proposed a {count}-node plan — review the DAG above · Enter approve"
        ));
        self.dirty = true;
    }

    /// While the user keeps iterating the plan WITH the orchestrator before launch
    /// (awaiting approval, nothing launched yet), the orchestrator may ADD / CHANGE /
    /// REMOVE tasks and re-write its plan file. `maybe_capture_orchestrator_plan` only
    /// captures the FIRST DAG (it bails once `awaiting_approval` is set), so this re-reads
    /// the file and refreshes `planned_nodes` whenever the parsed DAG actually changed —
    /// keeping the gate open so the user can keep refining by talking to the orchestrator.
    /// Skips when the approval marker is present (scan_orchestrator_markers owns that), and
    /// no-ops once any worker has launched (post-approval DAG changes go through conductor
    /// control markers like RUDDER_ADD_TASK and RUDDER_REPLAN).
    fn maybe_recapture_orchestrator_plan(&mut self) {
        if !self.awaiting_approval || self.refining || self.rebasing {
            return;
        }
        if self.agents.iter().any(|run| run.node_id.is_some()) {
            return;
        }
        if !self.has_running_interactive_orchestrator() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(orchestrator_plan_path(&self.cwd)) else {
            return;
        };
        // The user approved in the same write: defer to scan_orchestrator_markers so the
        // refreshed DAG is launched, not re-captured then immediately approved twice.
        if output_has_approve_marker(&text) {
            return;
        }
        let tasks = match extract_rudder_plan_tasks(&text) {
            Ok(tasks) if !tasks.is_empty() => tasks,
            _ => return,
        };
        let nodes: Vec<PlannedNode> = tasks.iter().map(PlannedNode::from_task).collect();
        if nodes == self.planned_nodes {
            return; // unchanged since the last capture — nothing to refresh
        }
        let count = nodes.len();
        self.planned_nodes = nodes;
        self.plan_summary = extract_rudder_plan_summary(&text);
        self.persist_plan_queue();
        self.notice = Some(format!(
            "orchestrator updated the plan — now {count} node(s) · review the DAG · Enter approve"
        ));
        self.dirty = true;
    }

    /// INTERACTIVE orchestrator (opt-in) self-launch: the orchestrator, after the user
    /// approves the plan IN CHAT, the orchestrator signals approval and Rudder launches —
    /// the user need not press Enter in the task bar.
    ///
    /// PRIMARY (hardened) channel: the orchestrator writes a `RUDDER_APPROVE_PLAN` line INTO
    /// its plan FILE (a structured Write, exact content — no terminal-rendering / ANSI /
    /// markdown / partial-read fragility). FALLBACK: the marker printed in the orchestrator
    /// PTY (the lossy channel, kept only as a backstop). `approve_planned_queue` is idempotent
    /// (early-returns when !awaiting_approval and flips it false on success), so the signal
    /// re-seen on every poll approves EXACTLY once. Gated by the per-run interactive flag.
    fn scan_orchestrator_markers(&mut self) {
        if !self.awaiting_approval || self.refining || self.rebasing {
            return;
        }
        if !self.has_running_interactive_orchestrator() {
            return;
        }
        // PRIMARY: the orchestrator wrote the approval into its plan file.
        let file_approved = std::fs::read_to_string(orchestrator_plan_path(&self.cwd))
            .map(|text| output_has_approve_marker(&text))
            .unwrap_or(false);
        // FALLBACK: the marker was printed into the orchestrator PTY.
        let pty_approved = !file_approved
            && self
                .agents
                .iter()
                .filter(|run| run.mode == AgentMode::RudderPlan && !run.reconcile_planner)
                .filter_map(|run| run.terminal.as_ref())
                .any(|terminal| output_has_approve_marker(terminal.output_log_snapshot()));
        if file_approved || pty_approved {
            self.approve_planned_queue();
        }
    }

    fn scan_orchestrator_skill_markers(&mut self) {
        if self.refining || self.rebasing {
            return;
        }
        if !self.has_running_interactive_orchestrator() {
            return;
        }
        let path = orchestrator_plan_path(&self.cwd);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut actions: Vec<String> = Vec::new();
        let mut kept: Vec<&str> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if is_orchestrator_skill_marker(trimmed) {
                actions.push(trimmed.to_string());
            } else {
                kept.push(line);
            }
        }
        if actions.is_empty() {
            return;
        }
        let mut rewritten = kept.join("\n");
        if text.ends_with('\n') {
            rewritten.push('\n');
        }
        let _ = std::fs::write(&path, rewritten);
        for action in actions {
            self.handle_orchestrator_skill_marker(&action);
        }
        self.dirty = true;
    }

    fn handle_orchestrator_skill_marker(&mut self, marker: &str) {
        if let Some(rest) = marker.strip_prefix("RUDDER_MODEL") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_MODEL requires <provider> <model> [effort]".to_string());
            } else {
                self.handle_command(&format!("/model {rest}"));
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_MAIN") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.handle_command("/main");
            } else {
                self.handle_command(&format!("/main {rest}"));
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_GOAL") {
            self.handle_command(&format!("/goal {}", rest.trim()));
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_CLOUD") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.handle_command("/cloud");
            } else {
                self.handle_command(&format!("/cloud {rest}"));
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_PLAN") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_PLAN requires a task".to_string());
            } else {
                self.handle_command(&format!("/plan {rest}"));
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_ADD_TASK") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_ADD_TASK requires a task".to_string());
            } else if self.plan_is_active() {
                self.reconcile_injection(rest);
            } else {
                self.start_rudder_plan_task(rest);
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_REPLAN") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_REPLAN requires a direction".to_string());
            } else if self.plan_is_active() {
                self.start_plan_rebase(rest);
            } else {
                self.start_rudder_plan_task(rest);
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_ASK") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_ASK requires a question or small change".to_string());
            } else {
                self.handle_command(&format!("/ask {rest}"));
            }
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_AUTOMERGE") {
            self.apply_automerge_marker(rest.trim());
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_MERGE ") {
            self.merge_agent_for_marker(rest.trim());
            return;
        }
        if marker == "RUDDER_MERGE" {
            self.notice = Some("RUDDER_MERGE requires a node id or run id".to_string());
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_STOP ") {
            self.stop_agent_for_marker(rest.trim());
            return;
        }
        if marker == "RUDDER_STOP" {
            self.notice = Some("RUDDER_STOP requires a node id or run id".to_string());
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_REGOAL ") {
            self.regoal_agent_for_marker(rest.trim());
            return;
        }
        if marker == "RUDDER_REGOAL" {
            self.notice = Some("RUDDER_REGOAL requires <node-or-run-id> <goal>".to_string());
            return;
        }
        if let Some(rest) = marker.strip_prefix("RUDDER_INJECT ") {
            self.inject_agent_for_marker(rest.trim());
            return;
        }
        if marker == "RUDDER_INJECT" {
            self.notice = Some("RUDDER_INJECT requires <node-or-run-id> <message>".to_string());
            return;
        }
        match marker {
            "RUDDER_USAGE" => self.show_usage_summary(),
            "RUDDER_HELP" => {
                self.notice = Some(
                    "skills: model, main, goal, add/replan, merge/stop/regoal/inject, usage, cloud, review-all, automerge"
                        .to_string(),
                );
            }
            "RUDDER_LOGIN" => {
                self.handle_command("/login");
            }
            "RUDDER_REVIEW_ALL" => {
                self.review_all_ready();
            }
            "RUDDER_MERGE_ALL" => {
                self.request_merge_all_ready();
            }
            _ => {}
        }
    }

    fn agent_index_for_token(&self, token: &str) -> Option<usize> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        self.agents
            .iter()
            .position(|run| run.id == token || run.node_id.as_deref() == Some(token))
    }

    fn merge_agent_for_marker(&mut self, token: &str) {
        let Some(index) = self.agent_index_for_token(token) else {
            self.notice = Some(format!("RUDDER_MERGE target not found: {token}"));
            return;
        };
        let run = &self.agents[index];
        if run.is_main() || run.is_oneoff() || run.is_orchestrator() {
            self.notice = Some("RUDDER_MERGE target is not a mergeable worker".to_string());
            return;
        }
        if run.status == AgentStatus::Merged {
            self.notice = Some(format!("{token} is already merged"));
            return;
        }
        if run.worktree_path.is_none() && run.worktree_branch.is_none() {
            self.notice = Some(format!("{token} has no workspace to merge"));
            return;
        }
        let task = run.task.clone();
        let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
        let source_branch = run.worktree_branch.clone();
        let worktree_path = run.worktree_path.clone();
        let agent_id = Some(run.id.clone());
        match self.merge_agent_at(index) {
            Ok(()) => {
                self.push_activity(format!("merged {label}"));
                self.run_scheduler();
            }
            Err(error) => {
                self.handle_merge_error(task, error, None, source_branch, worktree_path, agent_id);
            }
        }
    }

    fn stop_agent_for_marker(&mut self, token: &str) {
        let Some(index) = self.agent_index_for_token(token) else {
            self.notice = Some(format!("RUDDER_STOP target not found: {token}"));
            return;
        };
        if self
            .agents
            .get(index)
            .is_some_and(|run| run.is_orchestrator())
        {
            self.notice = Some("RUDDER_STOP cannot stop the orchestrator".to_string());
            return;
        }
        if !self.stop_agent_at(index) {
            self.notice = Some(format!("could not stop {token}"));
        }
    }

    fn regoal_agent_for_marker(&mut self, rest: &str) {
        let Some((token, goal)) = split_marker_target_payload(rest) else {
            self.notice = Some("RUDDER_REGOAL requires <node-or-run-id> <goal>".to_string());
            return;
        };
        let Some(index) = self.agent_index_for_token(token) else {
            self.notice = Some(format!("RUDDER_REGOAL target not found: {token}"));
            return;
        };
        if !self.regoal_agent_at(index, goal) {
            self.notice = Some(format!("could not re-goal {token}"));
        }
    }

    fn inject_agent_for_marker(&mut self, rest: &str) {
        let Some((token, message)) = split_marker_target_payload(rest) else {
            self.notice = Some("RUDDER_INJECT requires <node-or-run-id> <message>".to_string());
            return;
        };
        let Some(index) = self.agent_index_for_token(token) else {
            self.notice = Some(format!("RUDDER_INJECT target not found: {token}"));
            return;
        };
        if self
            .agents
            .get(index)
            .is_some_and(|run| run.is_orchestrator())
        {
            self.notice = Some("RUDDER_INJECT cannot target the orchestrator".to_string());
            return;
        }
        if self.live_inject_at(index, message) {
            self.push_activity(format!("injected note into {token}"));
        } else {
            self.notice = Some(format!("{token} has no live terminal for injection"));
        }
    }

    fn apply_automerge_marker(&mut self, value: &str) {
        let next = match value {
            "on" | "true" | "1" => true,
            "off" | "false" | "0" => false,
            "toggle" | "" => !self.auto_merge,
            _ => {
                self.notice = Some("RUDDER_AUTOMERGE expects on, off, or toggle".to_string());
                return;
            }
        };
        self.auto_merge = next;
        let _ = save_auto_merge(self.auto_merge);
        if self.auto_merge {
            self.notice = Some(
                "auto-merge ON: clean finished nodes merge themselves and unblock children"
                    .to_string(),
            );
            self.maybe_auto_merge();
        } else {
            self.notice = Some("auto-merge OFF: merge finished nodes manually".to_string());
        }
    }

    fn evaluate_completed_plan(&mut self, index: usize) {
        // A REBASE in flight must NEVER reach the initial-plan REPLACE path (it would wipe
        // the running plan's todo queue and re-gate the fleet); evaluate_completed_rebase
        // owns that case. Defensive guard in case a caller routes a rebase planner here.
        if self.rebasing {
            return;
        }
        let Some(run) = self.agents.get_mut(index) else {
            return;
        };
        // A transient reconcile planner must never reach the initial-plan REPLACE path
        // (it would wipe `planned_nodes`); it is routed to evaluate_completed_reconcile.
        // Callers already filter it out; this guard matches evaluate_completed_rebase and
        // is defensive against future refactors.
        if run.mode != AgentMode::RudderPlan || run.reconcile_planner || !run.autosteered {
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
        let backend = run.backend;
        let session_id = run.session_id.clone();
        let mut output = rudder_plan_output_for_run(run);
        // FALLBACK: if the live PTY-stream reconstruction has no parseable plan block (a
        // large RUDDER_PLAN_TASKS block + PTY ring-buffer truncation can drop it — the
        // exact "planner paused with a plan it actually produced" failure), recover the
        // block from the backend's OWN authoritative session record, which reliably carries
        // the full final message: Claude's `~/.claude/projects/.../<sid>.jsonl` transcript,
        // or Codex's `~/.codex/sessions/...` rollout. Only swap when it yields a real plan.
        let pty_has_plan = extract_rudder_plan_tasks(&output)
            .map(|tasks| !tasks.is_empty())
            .unwrap_or(false);
        if !pty_has_plan {
            let fallback_text = match backend {
                Backend::Claude => session_id
                    .as_deref()
                    .and_then(|sid| claude_transcript_final_text(&self.cwd, sid)),
                Backend::Codex => latest_codex_rudder_plan_output(run),
            };
            if let Some(text) = fallback_text {
                if extract_rudder_plan_tasks(&text)
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
                {
                    output = text;
                }
            }
        }

        let tasks = match extract_rudder_plan_tasks(&output) {
            Ok(tasks) => tasks,
            Err(error) => {
                run.autosteered = false;
                let _ = save_native_run_record(&self.cwd, run);
                self.refining = false;
                // No DAG yet: the planner likely asked a clarifying question or needs more
                // detail. Mark it PAUSED-for-input so the next typed message RESUMES this
                // planning conversation (NOT a leftover Done orchestrator from a shipped
                // plan, which must start fresh), and capture its questions for the prompt.
                self.planner_paused_for_input = true;
                self.pending_questions = planner_questions_or_forced(&output);
                self.notice = Some(format!(
                    "planner is waiting ({error}) — type your answer or more detail to continue planning"
                ));
                return;
            }
        };
        if !self.refining && !self.planner_question_round_done {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            self.planner_paused_for_input = true;
            self.pending_questions = planner_questions_or_forced(&output);
            self.notice = Some(
                "planner needs one question round before the DAG — type your answer to continue planning"
                    .to_string(),
            );
            self.dirty = true;
            return;
        }
        // Capture the planner's prose after the block (assumptions / open questions)
        // so the orchestrator pane can show what it assumed and invite refinement.
        let summary = extract_rudder_plan_summary(&output);
        if tasks.is_empty() {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            self.refining = false;
            self.planner_paused_for_input = true;
            self.pending_questions = planner_questions_or_forced(&output);
            // Same as the Err branch: keep the planner resumable so a typed answer
            // continues this conversation rather than starting a fresh planner.
            self.notice = Some(
                "planner is waiting (it asked a question or needs detail) — type your answer to continue planning"
                    .to_string(),
            );
            return;
        }
        // A DAG WAS captured: this planner is no longer paused for input.
        self.planner_paused_for_input = false;
        self.pending_questions.clear();

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
        // Persist the captured queue + gate so a restart resumes at this gate.
        self.persist_plan_queue();
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

    /// Retire a planner row: drop it from BOTH `self.agents` and disk. Used for the two
    /// rows that must not linger as phantom orchestrators (`is_orchestrator()` is true for
    /// ANY RudderPlan row): a TRANSIENT reconcile planner once it has been evaluated, and a
    /// STALE pinned orchestrator superseded by a brand-new plan. Deleting the on-disk
    /// `run.json` is the crucial half: without it the orphan reloads via
    /// `load_persisted_agents`. Idempotent: a no-op if the id is already gone. If the user
    /// was watching the row, return them to a remaining orchestrator.
    fn retire_planner_row(&mut self, run_id: &str) {
        let Some(index) = self.agents.iter().position(|run| run.id == run_id) else {
            return;
        };
        let was_watching = self.selected_agent == index;
        self.agents.remove(index);
        // Indices shifted under the rendered rows; drop the click map so a same-tick
        // click resolves to nothing instead of a neighbor (next render rebuilds it).
        self.agent_row_map.clear();
        // Delete the persisted record too, or it reloads as an orphan orchestrator.
        let _ = remove_native_run_record(&self.cwd, run_id);
        if was_watching {
            if let Some(orch_index) = self.agents.iter().position(|run| run.is_orchestrator()) {
                self.selected_agent = orch_index;
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
            }
        }
        if self.selected_agent >= self.agents.len() {
            self.selected_agent = self.agents.len().saturating_sub(1);
        }
    }

    /// APPEND a reconcile planner's node(s) into the active plan (parallel to
    /// `evaluate_completed_plan`, but it pushes instead of replacing). Each parsed
    /// task becomes a PlannedNode whose id is UNIQUIFIED against the existing
    /// planned-node ids and agent node ids (so dep ids resolve), then PUSHED onto
    /// `planned_nodes`. FALLBACK: a node the model returned with no deps gets a SOFT
    /// edge to every current frontier id (mirroring the daemon's soft-edge frontier
    /// fallback in src/scheduler.ts) so the added work is aware of the in-flight work
    /// but never deadlocks. The reconcile
    /// planner agent is then removed. If the session is already approved/running
    /// (not awaiting approval), the scheduler runs so the new node launches when its
    /// deps are met; if the initial plan is still awaiting approval, the node is
    /// left queued so it becomes part of the plan the user approves.
    fn evaluate_completed_reconcile(&mut self, index: usize) {
        // Read + validate the planner run inside a scoped mutable borrow, then drop
        // it before touching other `self` state (frontier, uniquify, append). On a
        // parse failure the captured-once flag is cleared and we bail.
        let (planner_task, output, run_id) = {
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
            (
                run.task.clone(),
                rudder_plan_output_for_run(run),
                run.id.clone(),
            )
        };

        // The frontier the new node(s) reconcile against: existing queued nodes plus
        // not-yet-merged plan-launched agents. The reconcile planner carries no node
        // id, so it never appears in the frontier. Feeds BOTH the dep parser (so
        // cross-block deps on these ids survive) and the no-deps soft fallback.
        let frontier: Vec<String> = self.plan_frontier().into_iter().map(|(id, _)| id).collect();

        let tasks = match extract_rudder_plan_tasks_with_frontier(&output, &frontier) {
            Ok(tasks) => tasks,
            Err(error) => {
                // Retire the transient planner even on failure, or it lingers as a
                // second orchestrator (in-session AND, via its run.json, after restart).
                self.retire_planner_row(&run_id);
                self.notice = Some(format!(
                    "added task did not produce a runnable node: {error}"
                ));
                return;
            }
        };
        if tasks.is_empty() {
            self.retire_planner_row(&run_id);
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

        // The reconcile planner has done its job; retire it (drop from self.agents AND
        // delete its run.json) so it never lingers as a second pinned orchestrator nor
        // reloads as one next launch. The initial planner is KEPT as the orchestrator.
        self.retire_planner_row(&run_id);

        // If the session is already approved/running, schedule so the new node
        // launches as soon as its deps are met. If the initial plan is still
        // awaiting approval, leave the node QUEUED: it joins the plan the user
        // approves at the gate.
        if self.awaiting_approval {
            // Pre-approval: the node only joins the queue the user is reviewing, so this
            // is a gated change (notice only), not an autonomous one.
            self.notice = Some(format!(
                "added {appended} node(s) to the plan awaiting approval"
            ));
        } else {
            // Post-approval: this is an AUTONOMOUS mutation, so log it to the activity
            // feed (visible) like every other autonomous action, then schedule.
            self.push_activity(format!("reconciled plan: added {appended} node(s)"));
            self.run_scheduler();
        }
        // MIRROR the appended node(s) into graph.json. When awaiting approval the
        // node was only queued (run_scheduler did not run), so mirror here too so
        // the board reflects the reconcile. Coalesced + non-fatal.
        self.mirror_graph();
        self.dirty = true;
    }

    /// Apply a STRUCTURAL plan-rebase once the resumed orchestrator emits its revised
    /// DAG (routed here by `maybe_detect_plan_ready` while `rebasing`). Builds the three
    /// live zones (MERGED baseline / RUNNING in-flight / TODO queued), diffs the new plan
    /// against them with the pure `diff_plan`, then applies build-forward, AUTONOMOUSLY:
    /// merged nodes are untouched; running nodes are kept, re-goaled (objective changed),
    /// or stopped (dropped); the TODO queue is rebuilt; the scheduler launches newly-ready
    /// work. No confirm — the diff is logged to the activity feed (visible) and every
    /// jj-touching step rides the op-log (undoable). If the planner produced no runnable
    /// block, the current plan stands unchanged.
    fn evaluate_completed_rebase(&mut self, index: usize) {
        // Drain + ingest the planner's FINAL buffered output before snapshotting (the
        // process may have blocked at a plan-mode approval prompt without exiting, so the
        // authoritative block can still be in the PTY). Mirrors evaluate_completed_plan.
        {
            let Some(run) = self.agents.get_mut(index) else {
                return;
            };
            if run.mode != AgentMode::RudderPlan || run.reconcile_planner || !run.autosteered {
                return;
            }
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
        }
        let output = rudder_plan_output_for_run(&self.agents[index]);

        // Parse against the frontier so cross-block deps onto in-flight ids survive.
        let frontier: Vec<String> = self.plan_frontier().into_iter().map(|(id, _)| id).collect();
        let new_tasks = match extract_rudder_plan_tasks_with_frontier(&output, &frontier) {
            Ok(tasks) if !tasks.is_empty() => tasks,
            other => {
                // No runnable block: KEEP the current plan, clear the rebase, surface why.
                if let Some(run) = self.agents.get_mut(index) {
                    run.autosteered = false;
                    let _ = save_native_run_record(&self.cwd, run);
                }
                self.rebasing = false;
                self.refining = false;
                self.notice = Some(match other {
                    Err(error) => format!(
                        "rebase produced no runnable plan ({error}); the current plan still stands"
                    ),
                    _ => "rebase produced no runnable plan; the current plan still stands"
                        .to_string(),
                });
                // Even on a no-op rebase the headless re-decompose already replaced the
                // live conductor's PTY, so restore it rather than stranding the user.
                self.restore_interactive_conductor_after_rebase(index);
                self.dirty = true;
                return;
            }
        };
        let summary = extract_rudder_plan_summary(&output);

        // Snapshot the three zones from the live fleet.
        let merged_ids: Vec<String> = self
            .agents
            .iter()
            .filter(|run| run.status == AgentStatus::Merged)
            .filter_map(|run| run.node_id.clone())
            .collect();
        let running: Vec<RunningNode> = self
            .agents
            .iter()
            .filter(|run| {
                run.node_id.is_some()
                    && !run.is_main()
                    && matches!(run.status, AgentStatus::Running | AgentStatus::Done)
            })
            .map(|run| RunningNode {
                id: run.node_id.clone().unwrap_or_default(),
                title: run.task_summary.clone(),
                context: run.current_prompt.clone(),
            })
            .collect();
        let todo = self.planned_nodes.clone();

        let diff = diff_plan(&running, &todo, &merged_ids, &new_tasks);
        let (added, regoaled, dropped, kept) = (
            diff.added.len(),
            diff.regoaled.len(),
            diff.dropped.len(),
            diff.kept.len(),
        );

        // APPLY (build-forward). 1) Stop running agents the new plan abandoned — keep
        // their jj workspace (undoable). Re-find by id each time since indices shift.
        for id in &diff.dropped {
            if let Some(idx) = self.agents.iter().position(|run| {
                run.node_id.as_deref() == Some(id.as_str())
                    && matches!(run.status, AgentStatus::Running | AgentStatus::Done)
            }) {
                self.stop_agent_at(idx);
            }
        }
        // 2) Re-goal running agents whose objective changed (resume session, files kept).
        for (id, goal) in &diff.regoaled {
            if let Some(idx) = self.agents.iter().position(|run| {
                run.node_id.as_deref() == Some(id.as_str())
                    && matches!(run.status, AgentStatus::Running | AgentStatus::Done)
            }) {
                self.regoal_agent_at(idx, goal);
            }
        }
        // 3) Replace the TODO queue with the rebuilt one (planner ids; the permissive
        // dangling-dep rule keeps the DAG from deadlocking on any stale reference).
        self.planned_nodes = diff.todo;
        self.plan_summary = summary;
        self.persist_plan_queue();

        // The rebase has landed: clear the rebase + refine flags + the planner's
        // capture-once flag (refining may have been set by an interleaved refine; leaving
        // it true would wedge the approval gate forever).
        self.rebasing = false;
        self.refining = false;
        if let Some(run) = self.agents.get_mut(index) {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
        }

        // 4) Launch newly-ready work, log the diff (visible even if mirror fails), then
        // mirror. save -> log -> mirror matches regoal_agent_at / stop_agent_at; mirror is
        // best-effort/non-fatal so logging first guarantees the user sees the action.
        self.run_scheduler();
        self.push_activity(format!(
            "rebased plan: +{added} added · {regoaled} re-goaled · {dropped} stopped/dropped · {kept} kept"
        ));
        self.mirror_graph();
        // The headless re-decompose replaced the live conductor's PTY; bring the
        // interactive conductor back so the user's conversation continues.
        self.restore_interactive_conductor_after_rebase(index);
        self.dirty = true;
    }

    /// After a structural rebase resolves, re-spawn the LIVE INTERACTIVE conductor that
    /// `start_plan_rebase` tore down to run the headless re-decompose. No-op unless
    /// `rebase_restore_interactive` was armed (the rebase was on an interactive Claude
    /// conductor) and the orchestrator row carries a resumable session. We resume that
    /// session so the conductor keeps the full conversation plus the rebase turn, and
    /// re-mark the row interactive + not-autosteered (matching the initial spawn) so the
    /// completion router never treats this live session's later exit as a fresh plan.
    fn restore_interactive_conductor_after_rebase(&mut self, index: usize) {
        if !self.rebase_restore_interactive {
            return;
        }
        self.rebase_restore_interactive = false;
        let cwd = self.cwd.clone();
        let Some(run) = self.agents.get_mut(index) else {
            return;
        };
        let Some(session) = run.session_id.clone().filter(|s| !s.trim().is_empty()) else {
            // No resumable session: leave the (headless) row as-is rather than
            // minting a brand-new conductor that has lost all prior context.
            return;
        };
        let command = rudder_orchestrator_resume_command(
            &run.model,
            run.effort,
            &session,
            "The plan has been rebased and is now live; the workers are implementing it. \
             Continue conducting: answer questions and steer the fleet via RUDDER_* markers.",
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
                run.completed_at = None;
                run.last_output_at = Instant::now();
                // Interactive conductor again, and NOT autosteered (its DAG is captured
                // from the plan file / markers, never from a headless completed-plan exit).
                run.interactive_orchestrator = true;
                run.autosteered = false;
                run.needs_permission = false;
                run.permission_notified = false;
                run.needs_user_input = false;
                run.user_input_notified = false;
                run.last_error = None;
                let _ = save_native_run_record(&cwd, run);
                self.push_activity("conductor live again: resumed after rebase".to_string());
            }
            Err(error) => {
                // Could not respawn: surface it but keep the (now headless) row so the
                // plan still runs; the user can keep steering via the dashboard keys.
                self.notice = Some(format!("could not resume the live conductor: {error}"));
            }
        }
    }

    /// Append a one-line entry to the conductor activity log (bounded) and surface it
    /// as the current notice. This is how every AUTONOMOUS action stays visible
    /// without a confirm gate.
    fn push_activity(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.notice = Some(msg.clone());
        // Mirror every conductor/steer line into the append-only narration stream the
        // web board tails, so "what's happening" shows up live in the browser too.
        self.append_activity_jsonl(&msg, "action");
        self.activity_log.push(msg);
        const MAX_ACTIVITY: usize = 200;
        if self.activity_log.len() > MAX_ACTIVITY {
            let overflow = self.activity_log.len() - MAX_ACTIVITY;
            self.activity_log.drain(0..overflow);
        }
        self.dirty = true;
    }

    /// Record a cross-cutting conductor DECISION: surface it in the in-pane activity log
    /// AND append it durably to DECISIONS.md, so the fleet (workers re-read that file) sees
    /// the conductor's plan/steer reasoning, not just the ephemeral activity line.
    fn record_decision(&mut self, title: &str, what: &str, why: Option<&str>) {
        // Only surface it in the activity feed when DECISIONS.md actually took a new entry,
        // so a deduped repeat (same decision re-hit every tick) does not spam either surface.
        if append_conductor_decision(&self.cwd, title, what, why) {
            self.push_activity(what.to_string());
        }
    }

    /// Append a single event line to `.rudder/activity.jsonl` — the append-only
    /// narration stream the web board tails to show "what's happening" live. Best
    /// effort and bounded: when the file grows past ~512KB it is rewritten to its
    /// tail so a long session never grows it without limit. `kind` is "action" for
    /// real conductor/steer events and "heartbeat" for the periodic liveness ping.
    fn append_activity_jsonl(&self, text: &str, kind: &str) {
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("activity.jsonl");
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 512 * 1024 {
                if let Ok(existing) = std::fs::read_to_string(&path) {
                    let mut tail: Vec<&str> = existing.lines().rev().take(400).collect();
                    tail.reverse();
                    let trimmed = tail.join("\n");
                    let tmp = path.with_extension(format!("jsonl.{}.tmp", std::process::id()));
                    if std::fs::write(&tmp, format!("{trimmed}\n")).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            }
        }
        let line = serde_json::json!({ "ts": now_stamp(), "text": text, "kind": kind });
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write as _;
            let _ = writeln!(file, "{line}");
        }
    }

    /// Periodic activity-feed heartbeat: ~every 45s while at least one worker is
    /// live, append a one-line status summary to the narration stream so the web
    /// board shows the fleet is alive even when the conductor is mid-step and
    /// silent. Ephemeral (jsonl only) — never touches DECISIONS.md or self.notice.
    fn maybe_emit_heartbeat(&mut self) {
        if self.last_heartbeat_emit.elapsed() < Duration::from_secs(45) {
            return;
        }
        self.last_heartbeat_emit = Instant::now();
        let total = self
            .agents
            .iter()
            .filter(|run| !run.is_orchestrator())
            .count();
        if total == 0 {
            return;
        }
        let live = self
            .agents
            .iter()
            .filter(|run| !run.is_orchestrator() && run.terminal.is_some())
            .count();
        let summary = if live > 0 {
            format!("{live} of {total} worker(s) active")
        } else {
            format!("{total} worker(s) idle, awaiting review or merge")
        };
        self.append_activity_jsonl(&summary, "heartbeat");
    }

    /// Poll the web board's steer inbox (`.rudder/steer/*.json`) and deliver each
    /// instruction into the matching agent's live PTY, then consume the file. The
    /// board writes one JSON file per steer request: `{"taskId": "<node-or-run-id
    /// or 'conductor'>", "instruction": "<text>"}`. This is the browser -> running
    /// agent control path; it mirrors `inject_agent_for_marker` but the source is a
    /// file rather than a RUDDER_INJECT PTY marker. Files are consumed before
    /// delivery so a failed inject never re-fires on the next poll.
    fn poll_steer_inbox(&mut self) {
        let dir = self.cwd.join(".rudder").join("steer");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut requests: Vec<(std::path::PathBuf, String, String)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
            let Some(value) = parsed else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let task_id = value
                .get("taskId")
                .or_else(|| value.get("id"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let instruction = value
                .get("instruction")
                .or_else(|| value.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            requests.push((path, task_id, instruction));
        }
        // Stable order so multiple queued steers apply in filename (timestamp) order.
        requests.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, task_id, instruction) in requests {
            // Consume first: a delivery failure must not re-inject in a loop.
            let _ = std::fs::remove_file(&path);
            if instruction.is_empty() {
                continue;
            }
            self.deliver_steer(&task_id, &instruction);
        }
    }

    /// Route one steer instruction from the web board to its target agent (or the
    /// live conductor) and record it in the activity feed. A blank/`conductor`/
    /// `orchestrator` target steers the interactive orchestrator pane.
    fn deliver_steer(&mut self, task_id: &str, instruction: &str) {
        // The web board is now token-gated, but treat steer text as untrusted in depth:
        // strip control characters so it can't inject extra Enter keys or escape
        // sequences into the agent PTY (live_inject_at appends one \r to submit it).
        let instruction = sanitize_steer_instruction(instruction);
        if instruction.is_empty() {
            return;
        }
        let conductor_target = task_id.is_empty()
            || task_id.eq_ignore_ascii_case("conductor")
            || task_id.eq_ignore_ascii_case("orchestrator");
        let index = if conductor_target {
            self.agents.iter().position(|run| run.is_orchestrator())
        } else {
            self.agent_index_for_token(task_id)
        };
        let Some(index) = index else {
            self.push_activity(format!("steer: target not found ({task_id})"));
            return;
        };
        let label = if conductor_target {
            "conductor".to_string()
        } else {
            task_id.to_string()
        };
        if self.live_inject_at(index, &instruction) {
            self.push_activity(format!("steered {label}: {instruction}"));
        } else {
            self.push_activity(format!("steer: {label} has no live terminal"));
        }
    }

    /// Open the live web board in the user's default browser (the `/web` command and
    /// the `o` key). No-op with a notice when no board URL was provided.
    fn open_web_ui(&mut self) {
        match self.board_url.clone() {
            Some(url) => match open_url_in_browser(&url) {
                // Keep the URL hidden on success (the hotkey is the way in); only
                // reveal it if auto-open fails so the user can open it manually.
                Ok(()) => self.notice = Some("opening this project's web view in your browser".to_string()),
                Err(err) => self.notice = Some(format!("could not open browser ({err}); open {url} manually")),
            },
            None => {
                self.notice = Some("web UI is not running for this session".to_string());
            }
        }
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
        let norm = |s: &str| {
            s.to_ascii_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
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
                // Done is the normal trigger. Failed/Stopped are included so a worker that
                // filed a report before dying still has it READ (ingest only spawns the
                // diff-backstop for cleanly-Done workers — an incomplete diff would invite
                // irrelevant follow-ups).
                run.node_id.is_some()
                    && matches!(run.mode, AgentMode::Execute)
                    && matches!(
                        run.status,
                        AgentStatus::Done | AgentStatus::Failed | AgentStatus::Stopped
                    )
                    && !run.merge_resolver
                    && !self.followups_ingested.contains(&run.id)
                    && !self.completion_summary_pending.contains(&run.id)
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

    /// Recover the finishing worker's completion note over the file channel (preferred)
    /// or the PTY-scrape fallback, and append its in-scope follow-ups (deduped, depth- and
    /// cap-guarded). If NEITHER channel produced a note, fall back to the BACKSTOP: spawn a
    /// one-shot summarizer over the worker's diff so a silent agent still advances the plan
    /// (the run is held `pending`, not marked ingested, until that result lands). Returns
    /// true only when a node was added synchronously here.
    fn ingest_worker_followups(&mut self, index: usize) -> bool {
        let (run_id, node_id, sidecar_note, output, cwd, task, is_complete) = {
            let Some(run) = self.agents.get_mut(index) else {
                return false;
            };
            // Only a cleanly-Done worker is eligible for the diff-backstop; a Failed/Stopped
            // one may still have a filed note we should read, but its diff is half-finished.
            let is_complete = run.status == AgentStatus::Done;
            let node_id = run.node_id.clone().unwrap_or_default();
            // PRIMARY, backend-agnostic channel: `rudder done` writes the structured note
            // to <workspace>/.rudder/done/<node>.json (see worker_done_file). This does
            // NOT pass through the agent's terminal, so it survives however Claude or
            // Codex render the echoed block in their interactive TUI (boxes, truncation,
            // wrapping). Read + parse it directly from the worker's workspace on disk.
            let sidecar_note = (!node_id.is_empty())
                .then(|| worker_done_file(&run.cwd, &node_id))
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
            // FALLBACK channel: scrape the echoed RUDDER_DONE block from the PTY. Flush
            // the tail first (a worker that printed-and-exited while UNFOCUSED may have
            // been marked Done by the cheap try_wait path without a final drain). We mark
            // the run ingested below (once, no retry), so this is the last chance.
            if sidecar_note.is_none() {
                if let Some(terminal) = run.terminal.as_mut() {
                    for _ in 0..16 {
                        if terminal.drain_output().is_empty() {
                            break;
                        }
                    }
                }
            }
            let output = run
                .terminal
                .as_ref()
                .map(|t| t.output_log_snapshot().to_string())
                .unwrap_or_default();
            (
                run.id.clone(),
                node_id,
                sidecar_note,
                output,
                run.cwd.clone(),
                run.task_summary.clone(),
                is_complete,
            )
        };

        if let Some(note) = sidecar_note.or_else(|| parse_worker_done_block(&output)) {
            // A note came through a direct channel. If the agent ADDRESSED follow-ups at
            // all (a `followups` array, even empty), trust it: apply what is there and we
            // are done (an empty list is a deliberate "nothing further"). If the note never
            // addressed follow-ups (freeform prose, or a summary with no `followups` key),
            // the agent's next-steps are unstructured, so fall through to the diff-backstop
            // and feed it the agent's own words. This is the freeform-prose recovery.
            let addressed_followups = note.get("followups").is_some();
            let grew = self.apply_worker_followups(&node_id, &note);
            if grew || addressed_followups {
                self.mark_run_ingested(run_id);
                return grew;
            }
            let note_summary = note
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let task = if note_summary.is_empty() {
                task
            } else {
                format!("{task}\n\nThe agent's own note: {note_summary}")
            };
            return self.spawn_completion_backstop(
                run_id,
                node_id,
                cwd,
                task,
                is_complete,
                "agent reported no structured next steps; mining its diff for follow-ups",
            );
        }

        // No report filed at all: reconstruct one from the diff (cleanly-Done only).
        self.spawn_completion_backstop(
            run_id,
            node_id,
            cwd,
            task,
            is_complete,
            "agent finished without a report; summarizing its diff to extract follow-ups",
        )
    }

    /// Spawn the one-shot diff summarizer for a worker that produced no usable structured
    /// follow-ups. Refuses (and just marks the run handled) when the worker is not cleanly
    /// Done (a half-finished diff invites noise) or carries no node id (nothing to attach
    /// to). Holds the run `pending` while the summarizer runs so it is not re-spawned.
    fn spawn_completion_backstop(
        &mut self,
        run_id: String,
        node_id: String,
        cwd: PathBuf,
        task: String,
        is_complete: bool,
        reason: &str,
    ) -> bool {
        if !is_complete || node_id.is_empty() {
            self.mark_run_ingested(run_id);
            return false;
        }
        let diff = jj_diff_text(&cwd, 16000);
        self.completion_summary_pending.insert(run_id.clone());
        spawn_completion_summary_worker(
            self.completion_summary_tx.clone(),
            run_id,
            node_id,
            task,
            diff,
        );
        self.push_activity(reason);
        false
    }

    /// Mark a run's follow-ups as ingested (so it is never re-scanned) and persist the
    /// ledger so a restart does not re-ingest or re-summarize an already-handled worker.
    fn mark_run_ingested(&mut self, run_id: String) {
        self.followups_ingested.insert(run_id);
        self.persist_ingested_runs();
    }

    /// Drain BACKSTOP summarizer results: apply each reconstructed note (growing the DAG)
    /// and mark the run ingested so it is never re-processed. Mirrors
    /// `poll_task_summary_workers`; called from the poll loop.
    fn poll_completion_summary_workers(&mut self) {
        let mut grew = false;
        while let Ok(result) = self.completion_summary_rx.try_recv() {
            // The backstop summarizer thread is fire-and-forget: it cannot be cancelled, so
            // a result can land AFTER the run was re-goaled (regoal clears it from pending +
            // the ledger to re-ingest the NEW work) or deleted. Only honor the result if the
            // run is STILL pending — otherwise marking it ingested would permanently skip the
            // re-goaled run's new completion (or resurrect a deleted run into the ledger).
            let still_wanted = self.completion_summary_pending.remove(&result.run_id);
            if !still_wanted {
                continue;
            }
            self.mark_run_ingested(result.run_id);
            if let Some(note) = result.note {
                if self.apply_worker_followups(&result.node_id, &note) {
                    grew = true;
                }
            }
        }
        if grew && !self.awaiting_approval {
            self.run_scheduler();
            self.mirror_graph();
        }
    }

    /// Persist the ingested-run ledger to `.rudder/ingestion-ledger.json` (atomic) so a
    /// restart does not re-ingest a worker (re-reading its sidecar) or, worse, re-spawn a
    /// duplicate diff-backstop for a silent one. Best-effort; bounded by plan size.
    fn persist_ingested_runs(&self) {
        let ids: Vec<&String> = self.followups_ingested.iter().collect();
        let Ok(json) = serde_json::to_string(&ids) else {
            return;
        };
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("ingestion-ledger.json");
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Persist the auto-merge skip list to `.rudder/auto-merge-skip.json` (atomic). The
    /// /automerge toggle itself is persisted (initial_auto_merge), so a skip list that
    /// dies with the process lets a restart's first maybe_auto_merge tick re-merge a
    /// known-conflicted run and re-spawn an AI resolver uninvited. Best-effort; bounded
    /// by plan size.
    fn persist_auto_merge_skip(&self) {
        let Ok(json) = serde_json::to_string(&self.auto_merge_skip) else {
            return;
        };
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("auto-merge-skip.json");
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Persist the auto-expansion DEPTH map to `.rudder/followup-gen.json` (atomic) so the
    /// MAX_FOLLOWUP_DEPTH cap survives a restart. Without this, reopening Rudder mid-plan
    /// resets every node's depth to 0 and an auto-expansion chain could grow further than
    /// the cap intends (still bounded by MAX_PLAN_TASKS, but the depth guard would be lost).
    fn persist_followup_gen(&self) {
        let Ok(json) = serde_json::to_string(&self.followup_gen) else {
            return;
        };
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("followup-gen.json");
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Persist the approval-gate queue + gate state to `.rudder/plan-queue.json` (atomic) so
    /// a mid-plan restart resumes the queued DAG instead of silently dropping it. Called on
    /// every queue mutation (plan captured, reconcile, rebase, approve, scheduler drain).
    fn persist_plan_queue(&self) {
        let snapshot = PlanQueueSnapshot {
            planned_nodes: self.planned_nodes.clone(),
            planned_origin: self.planned_origin.clone(),
            plan_request: self.plan_request.clone(),
            plan_summary: self.plan_summary.clone(),
            awaiting_approval: self.awaiting_approval,
        };
        let Ok(json) = serde_json::to_string(&snapshot) else {
            return;
        };
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("plan-queue.json");
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
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
            let titles: Vec<String> = followups
                .iter()
                .filter_map(|f| {
                    f.get("title")
                        .or_else(|| f.get("prompt"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            self.record_decision(
                &format!("Deferred follow-ups from {node_id} (depth cap)"),
                &format!(
                    "Auto-expansion stopped at depth cap {MAX_FOLLOWUP_DEPTH}; these follow-ups were NOT launched and need manual scheduling if wanted: {}",
                    if titles.is_empty() { "(unnamed)".to_string() } else { titles.join("; ") }
                ),
                Some("the depth cap prevents unbounded auto-growth of the DAG"),
            );
            return false;
        }
        // Every node id known to the plan (queued + launched). Used to (a) skip a batch
        // whose finishing node is gone, and (b) validate explicit follow-up deps below.
        let known = self.known_plan_node_ids();
        // If the finishing node's agent was deleted while a backstop was in flight, there
        // is nothing to attach to; skip rather than soft-link to a ghost id.
        if !node_id.is_empty() && !known.iter().any(|id| id == node_id) {
            self.push_activity(format!(
                "follow-ups from {node_id} skipped (its node is gone)"
            ));
            return false;
        }
        let frontier: Vec<String> = self.plan_frontier().into_iter().map(|(id, _)| id).collect();
        let mut added = 0usize;
        for f in &followups {
            // Out-of-lane work is recorded by the worker in DECISIONS.md, not auto-injected.
            // Match case-insensitively so "Out"/"OUT" are honored (anything else, incl. a
            // missing scope, is treated as in-lane and injected — never silently dropped).
            if f.get("scope")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("out"))
            {
                continue;
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
                self.record_decision(
                    &format!("Plan at {MAX_PLAN_TASKS}-node cap"),
                    &format!(
                        "Remaining follow-ups from {node_id} (incl. '{title}') were NOT launched: the plan hit the {MAX_PLAN_TASKS}-node cap. Schedule manually if still needed."
                    ),
                    Some("the plan-size cap prevents runaway auto-growth"),
                );
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
            // Explicit hard deps must reference real, LIVE plan nodes. A dep absent from the
            // plan is treated as already-satisfied by is_ready, so a typo'd id (e.g. "n_0"
            // for "n0") would make the node launch IMMEDIATELY, losing the gate. A dep that
            // resolves to a FAILED/STOPPED node is the opposite trap: is_ready only satisfies
            // a hard dep that MERGED, so it would deadlock forever. Treat BOTH (unknown and
            // dead) as bad: fall back to the soft-frontier edge and surface it.
            let dead: Vec<String> = self
                .agents
                .iter()
                .filter(|r| matches!(r.status, AgentStatus::Failed | AgentStatus::Stopped))
                .filter_map(|r| r.node_id.clone())
                .collect();
            let unknown: Vec<&str> = explicit
                .iter()
                .filter(|d| !known.iter().any(|k| k == *d) || dead.iter().any(|x| x == *d))
                .map(String::as_str)
                .collect();
            if explicit.is_empty() || !unknown.is_empty() {
                if !unknown.is_empty() {
                    self.push_activity(format!(
                        "follow-up '{title}' had unknown or failed dep(s) {unknown:?}; soft-linked instead of hard-gated"
                    ));
                }
                // Default / fallback: a SOFT edge to the finishing node + the frontier, so
                // the new work is aware of in-flight work but never gates/deadlocks.
                let mut soft = vec![node_id.to_string()];
                soft.extend(frontier.iter().cloned());
                soft.retain(|id| !id.is_empty());
                soft.sort();
                soft.dedup();
                node.soft_deps = soft;
            } else {
                // The agent said this follow-up consumes those nodes (all resolve): hard.
                node.deps = explicit;
            }
            self.followup_gen.insert(node.id.clone(), gen + 1);
            self.persist_followup_gen();
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
            // Degenerate approval (e.g. a rebase diffed every task away): clear the
            // gate. Keep the interactive orchestrator alive so the user can keep
            // talking to the conductor even when there is nothing to launch.
            self.awaiting_approval = false;
            self.persist_plan_queue();
            self.keep_orchestrator_after_approval();
            return;
        }
        self.awaiting_approval = false;
        self.planner_paused_for_input = false;
        self.pending_questions.clear();
        self.persist_plan_queue();
        // Record the plan approval as a cross-cutting decision so the fleet has the
        // authoritative goal + shape in DECISIONS.md from the start.
        let goal = if self.plan_request.trim().is_empty() {
            self.planned_origin.clone()
        } else {
            self.plan_request.clone()
        };
        let titles: Vec<String> = self.planned_nodes.iter().map(|n| n.title.clone()).collect();
        self.record_decision(
            "Plan approved",
            &format!(
                "Approved a {}-task plan for: {}. Tasks: {}.",
                titles.len(),
                goal.trim(),
                titles.join("; ")
            ),
            Some("user-approved plan; workers implement these nodes in isolated workspaces, honoring the dependency edges"),
        );
        // GENERATE graph.json from the approved DAG — the user's "it generates the
        // graph json" step. build_mirror_payload projects every planned node, so this
        // persists the full approved plan at the approval moment. run_scheduler
        // re-mirrors once workers launch so the board then tracks them as running.
        self.mirror_graph();
        // Drain immediately so a ready node moves todo->in progress without waiting
        // a full scheduler interval (covers the trivial 1-node case visibly).
        self.run_scheduler();
        // Keep the interactive orchestrator alive as the high-level conductor. It
        // remains read-only for repository files, but can explain progress and write
        // RUDDER_* control markers that the dashboard consumes.
        self.keep_orchestrator_after_approval();
        self.dirty = true;
    }

    /// After approval, the interactive orchestrator stops being the plan author and
    /// becomes the live conductor. It stays in the worker pane, can answer high-level
    /// questions, and can control the worker fleet through consumed RUDDER_* markers.
    /// Repository implementation remains delegated to the separate worker agents.
    fn keep_orchestrator_after_approval(&mut self) {
        let cwd = self.cwd.clone();
        let mut kept = false;
        for run in self.agents.iter_mut() {
            if run.mode != AgentMode::RudderPlan
                || run.reconcile_planner
                || run.backend != Backend::Claude
                || !run.interactive_orchestrator
                || run.status != AgentStatus::Running
            {
                continue;
            }
            run.needs_permission = false;
            run.permission_notified = false;
            run.needs_user_input = false;
            run.user_input_notified = false;
            // CRITICAL: clear the autosteer flag now that the plan is approved.
            // The completion router in `poll_agents` (and `evaluate_completed_plan`
            // itself) only treats a Done RudderPlan run as a freshly-captured plan
            // while `autosteered` is true. The post-approval conductor is a live
            // session that CAN exit later (model ends its turn, `/exit`, crash);
            // if it were still autosteered, that exit would route through
            // `evaluate_completed_plan`, re-set `awaiting_approval`, and re-gate the
            // already-running fleet with a bogus "plan ready" prompt. Clearing it
            // makes a post-approval exit a no-op for plan capture.
            run.autosteered = false;
            let _ = save_native_run_record(&cwd, run);
            kept = true;
        }
        if kept {
            self.push_activity(
                "orchestrator live: plan approved, workers are implementing the DAG".to_string(),
            );
            self.dirty = true;
        }
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

    /// The WHOLE plan DAG as render tasks, surviving the scheduler draining
    /// `planned_nodes` as it launches workers. Already-launched nodes are
    /// reconstructed from their `node_id` agents (the same projection
    /// `build_mirror_payload` uses); not-yet-launched nodes come from
    /// `planned_nodes`. Without this the orchestrator DAG pane would collapse to the
    /// "still planning" placeholder the moment the queue drains on approval. Live
    /// per-node status badges are supplied separately by `orchestrator_task_status`,
    /// so launched nodes carry no goal/success here.
    pub(crate) fn orchestrator_dag_tasks(&self) -> Vec<RudderPlanTask> {
        let mut tasks: Vec<RudderPlanTask> = Vec::new();
        // Launched nodes first (agents are appended in launch order, which tracks the
        // plan order), reconstructed from their carrying agents.
        for run in &self.agents {
            let Some(id) = run.node_id.clone() else {
                continue;
            };
            let title = if run.task_summary.trim().is_empty() {
                run.task.clone()
            } else {
                run.task_summary.clone()
            };
            let mut deps: Vec<PlanEdge> = run
                .deps
                .iter()
                .map(|on| PlanEdge {
                    on: on.clone(),
                    edge: EdgeType::Hard,
                    why: None,
                })
                .collect();
            deps.extend(run.soft_deps.iter().map(|on| PlanEdge {
                on: on.clone(),
                edge: EdgeType::Soft,
                why: None,
            }));
            let task = RudderPlanTask {
                id: id.clone(),
                title,
                prompt: run.current_prompt.clone(),
                goal: None,
                success: None,
                deps,
                backend: Some(run.backend.as_str().to_string()),
                model: Some(run.model.clone()),
                effort: run.effort.map(|effort| effort.as_str().to_string()),
            };
            // A node id can map to MORE THAN ONE agent (a failed launch + a re-goaled
            // relaunch). Collapse to a single row, keeping the LATEST agent's data
            // (agents are appended in order) so the DAG never double-renders a node.
            if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == id) {
                *existing = task;
            } else {
                tasks.push(task);
            }
        }
        // Queued (not-yet-launched) nodes still in the scheduler's hands, skipping any
        // id already represented by a launched agent.
        for node in &self.planned_nodes {
            if tasks.iter().any(|task| task.id == node.id) {
                continue;
            }
            tasks.push(node.to_task());
        }
        tasks
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
        // Hold auto-merge steady while a structural rebase is in flight: merging would
        // shift a RUNNING node into the MERGED zone mid-diff and the build-forward apply
        // would compute its zones against a moving target. Resume once the rebase lands.
        if self.rebasing {
            return;
        }
        // Serialize integration through the shared jj workspace: while a merge-conflict
        // resolver is mid-flight it is resolving a conflict IN self.cwd, and starting another
        // merge there would stack a new merge change on top of the in-progress resolution —
        // shifting @ under the resolver and making finalize_merge_resolvers mis-read whose
        // conflicts remain. Workers run in parallel, but integration is one-at-a-time. Resume
        // when the resolver lands (finalize_merge_resolvers clears its resolver state).
        if self
            .agents
            .iter()
            .any(|run| run.merge_resolver && run.status == AgentStatus::Running)
        {
            return;
        }
        let mut merged_labels: Vec<String> = Vec::new();
        let mut conflicted = false;
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
                Ok(()) => merged_labels.push(short_task(&label)),
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
                    conflicted = true;
                    let resolver_running =
                        self.agents.iter().any(|r| r.id == id && r.merge_resolver);
                    if resolver_running {
                        self.notice = Some(format!(
                            "conflict in {} — AI resolver integrating it",
                            short_task(&label)
                        ));
                    } else {
                        // Resolver did not start; do not retry every tick.
                        self.auto_merge_skip.push(id);
                        self.persist_auto_merge_skip();
                    }
                    break; // one conflict at a time
                }
            }
        }
        if !merged_labels.is_empty() {
            // Auto-merges used to be silent: nodes jumped from review to done with no
            // explanation. Announce them (the conflict notice above wins when both fire,
            // since that one still needs the user's attention).
            if !conflicted {
                self.notice = Some(match merged_labels.as_slice() {
                    [only] => format!("auto-merged {only} · dependents unblocked"),
                    many => format!("auto-merged {} nodes · dependents unblocked", many.len()),
                });
            }
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
                self.notice = Some(format!(
                    "resolved conflict and merged {}",
                    short_task(&label)
                ));
            } else {
                // Conflicts still remain: drop back to manual. CRUCIALLY, skip it from
                // auto-merge — otherwise (resolver flag now cleared, status still Done) the
                // next maybe_auto_merge tick re-merges, re-conflicts, and re-spawns a
                // resolver every tick, stacking jj merge changes forever. Cleared again on a
                // successful manual merge / re-goal (see merge_agent_at, regoal_agent_at).
                if !self.auto_merge_skip.contains(&id) {
                    self.auto_merge_skip.push(id.clone());
                    self.persist_auto_merge_skip();
                }
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

    /// Build the "Depends on:" context block injected into a launching worker's prompt.
    /// Lists each hard then soft parent by id + title, with its `rudder done` interface
    /// summary when the parent has already merged (its code is in this worker's workspace
    /// base, since the scheduler gates a node on its hard parents merging first). Empty
    /// when the node has no deps. Orients the worker to BUILD ON its prerequisites rather
    /// than rediscover or reimplement them.
    fn dependency_context(&self, node: &PlannedNode) -> String {
        let deps: Vec<(&str, bool)> = node
            .deps
            .iter()
            .map(|id| (id.as_str(), true))
            .chain(node.soft_deps.iter().map(|id| (id.as_str(), false)))
            .collect();
        if deps.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        for (id, is_hard) in deps {
            let parent = self
                .agents
                .iter()
                .find(|run| run.node_id.as_deref() == Some(id));
            let title = parent
                .map(|run| {
                    if run.task_summary.trim().is_empty() {
                        run.task.clone()
                    } else {
                        run.task_summary.clone()
                    }
                })
                .or_else(|| {
                    self.planned_nodes
                        .iter()
                        .find(|n| n.id == id)
                        .map(|n| n.title.clone())
                })
                .unwrap_or_else(|| id.to_string());
            let merged = parent.is_some_and(|run| run.status == AgentStatus::Merged);
            // The parent's own `rudder done` note (interfaces it created, else its summary),
            // read from its workspace. Present only once the parent has reported.
            let interface = parent.and_then(|run| {
                let raw = std::fs::read_to_string(worker_done_file(&run.cwd, id)).ok()?;
                let note: serde_json::Value = serde_json::from_str(&raw).ok()?;
                ["interfaces", "summary"].iter().find_map(|key| {
                    note.get(*key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(|value| preview_text(value, 240))
                })
            });
            let kind = if is_hard { "hard" } else { "soft" };
            let state = if merged {
                "already merged into your workspace — build on it"
            } else if is_hard {
                "merges before your task is expected to succeed"
            } else {
                "may land while you work; treat as context"
            };
            let mut line = format!("- {id} ({kind}) {title} — {state}");
            if let Some(interface) = interface {
                line.push_str(&format!("\n    exposes: {interface}"));
            }
            lines.push(line);
        }
        format!(
            "Depends on (build on these — do NOT reimplement them):\n{}\n\n",
            lines.join("\n")
        )
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
            let depends_on = self.dependency_context(&node);
            let prompt = planned_node_worker_prompt(&planner_task, &node, &depends_on);
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
        } else {
            // DEADLOCK SURFACE: a queued node whose HARD dep FAILED can never become ready
            // (is_ready only satisfies a hard dep that MERGED), so it would sit in Todo
            // forever with no explanation. When nothing launched this pass, detect those
            // and tell the user how to unblock. notice (not activity_log) so it does not
            // flood; idempotent if it persists across ticks.
            // Only a genuine deadlock, not a cap wait: if the parallelism cap is saturated,
            // nothing launched simply because every slot is busy — not because a dependency
            // failed. Suppress the notice then to avoid a spurious "blocked by FAILED dep".
            let cap_saturated = self.running_plan_agents() >= max_parallel();
            let failed_ids: Vec<String> = if cap_saturated {
                Vec::new()
            } else {
                self.agents
                    .iter()
                    .filter(|run| run.status == AgentStatus::Failed)
                    .filter_map(|run| run.node_id.clone())
                    .collect()
            };
            if !failed_ids.is_empty() {
                let blocked: Vec<&str> = self
                    .planned_nodes
                    .iter()
                    .filter(|node| {
                        node.deps
                            .iter()
                            .any(|dep| failed_ids.iter().any(|f| f == dep))
                    })
                    .map(|node| node.title.as_str())
                    .collect();
                if !blocked.is_empty() {
                    self.notice = Some(format!(
                        "{} task(s) blocked by a FAILED dependency ({}) — retry or re-goal the failed node to unblock, or delete it",
                        blocked.len(),
                        failed_ids.join(", ")
                    ));
                    self.dirty = true;
                }
            }
        }

        // MIRROR the plan into graph.json so the board reflects this DAG. Covers
        // both the just-launched nodes (now Running agents) and the queue that
        // remains in Todo. Coalesced + non-fatal inside mirror_graph.
        self.mirror_graph();
        // The queue shrank as nodes launched; persist so a restart does not re-launch
        // an already-launched node or lose the remaining queue.
        self.persist_plan_queue();
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
        let Some(selected) = self.agents.get(self.selected_agent) else {
            return;
        };
        if self.delete_pending.as_deref() != Some(&selected.id) {
            self.delete_pending = Some(selected.id.clone());
            let label = if selected.task_summary.trim().is_empty() {
                short_task(&selected.task)
            } else {
                truncate_chars(selected.task_summary.trim(), 40)
            };
            // Name WHAT is being deleted and spell out both outcomes; the bare
            // "press d again" read as a generic hint and was easy to act on with
            // the wrong row selected.
            self.notice = Some(if selected.worktree_path.is_some() {
                format!("delete {label} and remove its worktree? press d again to confirm · any other key cancels")
            } else {
                format!("delete {label}? press d again to confirm · any other key cancels")
            });
            return;
        }

        let run = self.agents.remove(self.selected_agent);
        // Indices shifted under the rendered rows; drop the click map so a same-tick
        // click resolves to nothing instead of a neighbor (next render rebuilds it).
        self.agent_row_map.clear();
        let _ = remove_native_run_record(&self.cwd, &run.id);
        // Drop the run's hook/signal files too, or ~/.rudder/signals grows forever.
        signals::cleanup_run_signals(&run.id);
        // Prune the run from the ingest ledger so it does not accumulate dead ids (and so
        // a hypothetically-reused run id is not pre-marked ingested).
        let was_ingested = self.followups_ingested.remove(&run.id);
        let was_pending = self.completion_summary_pending.remove(&run.id);
        if was_ingested || was_pending {
            self.persist_ingested_runs();
        }
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
        if run.is_oneoff() {
            self.notice = Some("one-off agent: merge disabled".to_string());
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
            detail: (pending > 0).then(|| {
                format!(
                    "{pending} uncommitted file{plural} will be auto-committed as \"{summary}\".",
                    plural = if pending == 1 { "" } else { "s" },
                    summary = truncate_chars(&summary, 48),
                )
            }),
        });
        self.conflict_prompt = None;
        // The modal carries the question and the keys; a parallel notice would
        // just duplicate (and outlive) it.
        self.notice = None;
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
                    && !run.is_oneoff()
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

        let _ = count;
        self.delete_pending = None;
        self.merge_confirm = Some(MergeConfirmation {
            intent: MergeIntent::All { ids },
            detail: (pending_total > 0).then(|| {
                format!(
                    "{pending_total} uncommitted file{p1} across {pending_runs} worktree{p2} will be auto-committed.",
                    p1 = if pending_total == 1 { "" } else { "s" },
                    p2 = if pending_runs == 1 { "" } else { "s" },
                )
            }),
        });
        self.conflict_prompt = None;
        // The modal carries the question and the keys; a parallel notice would
        // just duplicate (and outlive) it.
        self.notice = None;
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
                        self.notice = Some("merged · dependent nodes unblocked".to_string());
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
                let total = ids.len();
                let mut merged = 0;
                for (position, id) in ids.iter().enumerate() {
                    let Some(index) = self.agents.iter().position(|run| run.id == *id) else {
                        continue;
                    };
                    let task = self.agents[index].task.clone();
                    let source_branch = self.agents[index].worktree_branch.clone();
                    let worktree_path = self.agents[index].worktree_path.clone();
                    let agent_id = Some(self.agents[index].id.clone());
                    if let Err(error) = self.merge_agent_at(index) {
                        // Integration is serialized through the shared workspace, so a
                        // conflict stops the batch (continuing would stack merges on a
                        // conflicted @). Tell the user the FULL outcome — what landed,
                        // what is paused, and what is still waiting — not just the error.
                        let remaining = total.saturating_sub(position + 1);
                        self.handle_merge_error(
                            task,
                            error,
                            Some(merged),
                            source_branch,
                            worktree_path,
                            agent_id,
                        );
                        if let Some(notice) = self.notice.take() {
                            self.notice = Some(format!(
                                "merged {merged}/{total} · {notice}{}",
                                if remaining > 0 {
                                    format!(" · {remaining} more wait in review")
                                } else {
                                    String::new()
                                }
                            ));
                        }
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
                // The WORK is finished; only its integration conflicted. Keep the run
                // Done (review bucket) so it still reads as merge-able — flipping it to
                // Stopped buried it in "closed" looking cancelled. Park it in the
                // auto-merge skip list so auto-merge does not re-conflict every tick;
                // a successful merge or re-goal clears the skip.
                run.last_error = Some(error.to_string());
                let _ = save_native_run_record(&self.cwd, run);
                let id = self.agents[index].id.clone();
                if !self.auto_merge_skip.contains(&id) {
                    self.auto_merge_skip.push(id);
                    self.persist_auto_merge_skip();
                }
            }
        }
        let operation_label = if operation == ConflictOperation::Rebase {
            "rebase"
        } else {
            "merge"
        };
        self.notice = Some(format!(
            "{operation_label} conflict in {} ({count} file{}): press y to let AI resolve & complete the merge, or n to do it manually",
            short_task(&self.conflict_prompt.as_ref().map(|p| p.task.clone()).unwrap_or_default()),
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
        // A re-goaled worker reuses its run id but will do NEW work and file a NEW report.
        // Clear its ingest ledger + any in-flight backstop so that second completion is
        // ingested afresh instead of being skipped as "already handled".
        if let Some((id, node_id, run_cwd)) = self
            .agents
            .get(index)
            .map(|run| (run.id.clone(), run.node_id.clone(), run.cwd.clone()))
        {
            let was_ingested = self.followups_ingested.remove(&id);
            let was_pending = self.completion_summary_pending.remove(&id);
            // Persist if EITHER set changed. If only `pending` was set (a backstop is in
            // flight), failing to persist the clear lets a late backstop result re-add the
            // stale id (mark_run_ingested), which would permanently skip the re-goaled
            // worker's NEW completion, including across a restart.
            if was_ingested || was_pending {
                self.persist_ingested_runs();
            }
            // Delete the STALE completion sidecar so the re-goaled run cannot re-ingest its
            // previous turn's note before it has written a new one.
            if let Some(node_id) = node_id.as_deref() {
                let _ = std::fs::remove_file(worker_done_file(&run_cwd, node_id));
            }
            // A re-goaled run is no longer a conflict to skip; let it auto-merge again.
            let before = self.auto_merge_skip.len();
            self.auto_merge_skip.retain(|x| x != &id);
            if self.auto_merge_skip.len() != before {
                self.persist_auto_merge_skip();
            }
        }
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
            let run_id = run.id.clone();
            let run_mode = run.mode;
            let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
            let session = run.session_id.clone().filter(|s| !s.trim().is_empty());
            let (mut command, deliver_after, new_session) = if let Some(sid) = session.as_deref() {
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
                let command = agent_command(
                    backend,
                    &run.model,
                    run.effort,
                    &prompt,
                    AgentMode::Execute,
                    sid.as_deref(),
                );
                (command, None, sid)
            };
            // RE-WIRE completion hooks for the re-goaled session: its hook config file
            // already exists, so the poll loop waits for the official signal — a
            // relaunch without the hooks wired would never flip back to review.
            signals::augment_worker_command(&mut command, backend, run_mode, &run_id);
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
        let result = match self
            .agents
            .get_mut(index)
            .and_then(|run| run.terminal.as_mut())
        {
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
        let (label, run_id) = {
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
            (
                run.node_id.clone().unwrap_or_else(|| run.id.clone()),
                run.id.clone(),
            )
        };
        // A stopped worker will not report again; prune it from the ingest ledger so it
        // does not linger (and a re-goal later re-ingests it afresh).
        let was_ingested = self.followups_ingested.remove(&run_id);
        let was_pending = self.completion_summary_pending.remove(&run_id);
        if was_ingested || was_pending {
            self.persist_ingested_runs();
        }
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
        let (operation, resolver_cwd, source_branch, worktree_path) =
            conflict_context.unwrap_or((ConflictOperation::Merge, self.cwd.clone(), None, None));
        let session_id = mint_session_id_for(backend);
        let resolver_run_id = self.agents[index].id.clone();
        let mut command = agent_command(
            backend,
            &model,
            effort,
            &prompt,
            AgentMode::Execute,
            session_id.as_deref(),
        );
        // Wire the backend's own completion signal (Claude Stop hook / Codex notify)
        // for the resolver, exactly like every other worker spawn. WITHOUT this the
        // resolver reuses the node's run_id whose `{run_id}-claude.json` still exists
        // on disk, so worker_has_config() is true and the poll loop WAITS for a Stop
        // hook that was never wired -> a cleanly-resolved resolver sits Running forever
        // and finalize_merge_resolvers (needs status==Done) never merges it. See signals.rs.
        signals::augment_worker_command(
            &mut command,
            backend,
            AgentMode::Execute,
            &resolver_run_id,
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
            "(no specific conflicted files were reported; run the status command to find them)"
                .to_string()
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
        // Already merged: a confirm prompt acted on a stale snapshot and an intervening
        // auto-merge already merged this run. Re-running `rudder merge` would create a
        // spurious second merge. No-op instead.
        if run.status == AgentStatus::Merged {
            return Ok(());
        }
        let is_jj = Self::run_is_jj(run);
        let review_source_ids = run.review_source_ids.clone();
        let run_id = run.id.clone();

        if is_jj {
            // jj runs merge through the TS `rudder merge <id>` command, which
            // routes to mergeJjRunIntoCurrentWorkspace and captures op-log for
            // `rudder undo`. The command records its outcome in run.json and
            // exits 0 even on conflict, so classify by the recorded state.
            let run_id = run.id.clone();
            match run_rudder_jj_command(&self.cwd, "merge", &run_id, "merge") {
                JjCliOutcome::Ok => {}
                JjCliOutcome::Conflict { files } => {
                    // Before falling back to a multi-minute LLM resolver, auto-resolve the
                    // MECHANICAL conflicts directly in jj (package.json/.gitignore union,
                    // regenerable lock files). These collide on nearly every parallel
                    // feature merge but have an unambiguous answer. If that clears every
                    // conflict the merge @ is now clean and we fall through to the success
                    // finalize below — no resolver spawned, the node merges instantly.
                    let remaining = auto_resolve_mechanical_conflicts(&self.cwd, &files);
                    if !remaining.is_empty() {
                        self.pending_jj_conflict = Some(remaining);
                        anyhow::bail!("jj merge created conflicts");
                    }
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
            // INGEST the worker's completion follow-ups BEFORE flipping it to Merged: a
            // Merged run is excluded from the follow-up candidate set, so a MANUAL merge
            // (m/M) racing ahead of the cadence-bound maybe_ingest_worker_followups would
            // otherwise drop its follow-ups permanently. Idempotent via the guards (the
            // auto-merge path already ingested on an earlier tick → this is a no-op there).
            let should_ingest = self.agents.get(index).is_some_and(|r| {
                r.node_id.is_some()
                    && matches!(r.mode, AgentMode::Execute)
                    && !r.merge_resolver
                    && !self.followups_ingested.contains(&r.id)
                    && !self.completion_summary_pending.contains(&r.id)
            });
            if should_ingest {
                self.ingest_worker_followups(index);
            }
            self.mark_agent_and_review_sources_merged(index, review_source_ids);
        }
        // A successful merge clears any prior conflict-skip for this run, so it can
        // auto-merge again if it ever returns to review (and does not stay permanently
        // un-auto-merged for the session).
        let before = self.auto_merge_skip.len();
        self.auto_merge_skip.retain(|x| x != &run_id);
        if self.auto_merge_skip.len() != before {
            self.persist_auto_merge_skip();
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
        self.poll_completion_summary_workers();

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

        // Browser control + narration heartbeat, throttled to ~1s so the render
        // loop (11-33ms tick) is never stalled by the readdir/fs work.
        if self.last_steer_poll.elapsed() >= Duration::from_secs(1) {
            self.last_steer_poll = Instant::now();
            self.poll_steer_inbox();
            self.maybe_emit_heartbeat();
        }

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
                // needs_permission / needs_user_input are surfaced visually (amber
                // status label) but DO NOT ring: only entering review pings (see
                // mark_run_done). Previously these rang on every detector flicker,
                // which fired a ping whenever you selected a waiting agent.
                if needs_permission {
                    run.permission_notified = true;
                }
                let needs_user_input = !needs_permission
                    && visible_lines
                        .as_ref()
                        .is_some_and(|lines| terminal_needs_user_input_from_lines(lines));
                run.needs_user_input = needs_user_input;
                if needs_user_input {
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
                            changed = true;
                        };
                    }
                    Ok(None) => {
                        // The process is still alive (interactive agents do not exit
                        // when a turn ends).
                        //
                        // AUTHORITATIVE: the backend's own completion signal (Claude `Stop`
                        // hook / Codex `notify`, wired in signals.rs). When present it is
                        // deterministic — no PTY-scrape guessing — so it wins. A "done"
                        // signal is one-shot (cleared on consume) so a later turn of the
                        // same live agent re-fires cleanly; an "input" signal marks the
                        // waiting state. The scrape below is the FALLBACK for older CLI
                        // versions or when hooks are unavailable.
                        let signal = (!run.is_orchestrator())
                            .then(|| signals::read_signal(&run.id))
                            .flatten();
                        match signal {
                            Some(signals::SignalState::Done) => {
                                signals::clear_signal(&run.id);
                                run.ready_since = None;
                                mark_run_done(run);
                                changed = true;
                            }
                            Some(signals::SignalState::Input) => {
                                run.ready_since = None;
                                if !run.needs_user_input {
                                    run.needs_user_input = true;
                                    run.user_input_notified = true;
                                    changed = true;
                                }
                            }
                            None if signals::worker_has_config(&run.id, run.backend) => {
                                // Official signals ARE wired for this worker but none has
                                // arrived yet: the turn has not ended. WAIT for the Stop
                                // hook / notify — do NOT let the chrome-scrape flip it to
                                // review during a mid-turn idle lull (the premature-review
                                // bug). Completion comes only from the signal or process exit.
                                run.ready_since = None;
                            }
                            None => {
                                // FALLBACK (no hooks wired — older CLI version): declare
                                // completion once the screen has looked ready-for-input
                                // continuously for READY_GRACE. Tracking the ready window
                                // (not output-silence) makes this robust against a TUI that
                                // repaints while idle. Headless planners (RudderPlan) are
                                // excluded: their stdout is raw JSONL, not idle-chrome, and
                                // complete via process exit only.
                                let ready = !run.is_orchestrator()
                                    && visible_lines.as_ref().is_some_and(|lines| {
                                        terminal_looks_ready_for_input_from_lines(
                                            run.backend,
                                            lines,
                                        )
                                    });
                                if ready {
                                    let since = *run.ready_since.get_or_insert_with(Instant::now);
                                    if since.elapsed() >= READY_GRACE {
                                        run.ready_since = None;
                                        mark_run_done(run);
                                        changed = true;
                                    }
                                } else if visible_lines.is_some() {
                                    // Screen is readable and NOT idle (busy or a
                                    // prompt/permission): reset the window.
                                    run.ready_since = None;
                                }
                            }
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
            } else if self.rebasing {
                // A REBASE planner exits headless right after printing the revised DAG. It
                // is not a reconcile planner, so without this branch it would fall to the
                // initial-plan REPLACE path (evaluate_completed_plan), wiping the running
                // plan's todo queue and re-gating the whole fleet. Route it to the
                // build-forward rebase evaluator (which preserves merged/running zones).
                self.evaluate_completed_rebase(index);
            } else {
                self.evaluate_completed_plan(index);
            }
        }

        // STREAMING DETECTION: a plan-mode planner may print the RUDDER_PLAN_TASKS
        // block and then BLOCK at an ExitPlanMode approval prompt without ever
        // exiting, so the Done-keyed path above never fires. Capture the plan into
        // the approval gate as soon as a parseable block appears in the live output.
        self.maybe_detect_plan_ready();
        // INTERACTIVE orchestrator (opt-in): capture the DAG it wrote to its plan file, then
        // self-launch if it printed RUDDER_APPROVE_PLAN after the user approved in chat.
        self.maybe_capture_orchestrator_plan();
        // While the user keeps talking to the orchestrator before launch, it may add/change/
        // remove tasks and re-write the plan file; reflect those edits in the DAG live.
        self.maybe_recapture_orchestrator_plan();
        self.scan_orchestrator_markers();
        self.scan_orchestrator_skill_markers();

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
        // Grow the DAG from finished workers' `rudder done` reports (auto-expand):
        // a finishing agent's recommended follow-ups become new planned nodes,
        // surfaced in the activity log. Autonomous, no confirm.
        //
        // This MUST run before auto-merge: a candidate is ingested only while its status
        // is still Done, and maybe_auto_merge flips Done -> Merged. If merge ran first, an
        // auto-merged node would never be ingested and the plan would stop growing under
        // /automerge. Ingesting first reads the sidecar/PTY while the worker (and its
        // workspace) are intact, then merge unblocks its children on the same tick.
        if self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            self.maybe_ingest_worker_followups();
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

fn command_rest<'a>(input: &'a str, command: &str) -> &'a str {
    input.trim_start().strip_prefix(command).unwrap_or_default()
}

/// Neutralize an untrusted steer instruction before it is injected into a live agent
/// PTY. The text arrives from the web board (`.rudder/steer/*.json`) and is written to
/// the terminal followed by a single Enter, so any embedded control character is a
/// vector: CR/LF would submit extra command lines and ESC could smuggle terminal escape
/// sequences. Replace every control char with a space and collapse runs of whitespace so
/// the instruction stays exactly one line of printable text.
fn sanitize_steer_instruction(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Open a URL in the user's default browser. Spawned detached with null stdio so
/// it never blocks the render loop; success only means the opener launched.
fn open_url_in_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn split_marker_target_payload(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    let mut parts = value.splitn(2, char::is_whitespace);
    let target = parts.next()?.trim();
    let payload = parts.next()?.trim();
    if target.is_empty() || payload.is_empty() {
        None
    } else {
        Some((target, payload))
    }
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
    // Unfreeze finished-but-stuck in-flight work (e.g. merge resolvers that completed under
    // an older binary) BEFORE trying to resume anything, so a restart makes the board progress
    // instead of resurrecting dead processes in their stuck state.
    app.reconcile_orphaned_runs();
    app.restore_running_agents();
    // Reconcile graph.json with the restored in-memory state on startup so the board does
    // not show stale "planned" nodes from a previous session (and reflects the restored
    // plan queue / reloaded agents). last_mirror_signature is None here, so this runs once.
    app.mirror_graph();

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
/// Path where a finished worker drops its machine-readable completion note: the ROBUST,
/// rendering-independent half of the `rudder done` report. Lives inside the worker's own
/// gitignored `.rudder` dir (so it is writable under either backend's Bash tool and is
/// never merged), keyed by node id so workers that share a checkout never collide. The
/// launcher sets RUDDER_DONE_FILE to this path; the orchestrator reads it on completion.
fn worker_done_file(workspace: &Path, node_id: &str) -> PathBuf {
    workspace
        .join(".rudder")
        .join("done")
        .join(format!("{node_id}.json"))
}

/// Load the persisted ingested-run ledger (run ids whose follow-ups were already handled)
/// written by `persist_ingested_runs`. Missing/corrupt file => empty set.
fn load_ingested_runs(cwd: &Path) -> HashSet<String> {
    std::fs::read_to_string(cwd.join(".rudder").join("ingestion-ledger.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default()
}

/// Load the persisted auto-merge skip list (run ids parked after a merge conflict)
/// written by `persist_auto_merge_skip`. Missing/corrupt file => empty list.
fn load_auto_merge_skip(cwd: &Path) -> Vec<String> {
    std::fs::read_to_string(cwd.join(".rudder").join("auto-merge-skip.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

fn load_followup_gen(cwd: &Path) -> HashMap<String, u8> {
    std::fs::read_to_string(cwd.join(".rudder").join("followup-gen.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<HashMap<String, u8>>(&raw).ok())
        .unwrap_or_default()
}

/// Encode a working directory the way Claude Code names its session-transcript folder
/// under `~/.claude/projects/` (every non-alphanumeric char -> `-`).
fn encode_claude_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Path to a Claude session transcript (JSONL) for `session_id` run in `cwd`. Rudder mints
/// the session id, so it always knows where Claude streams the JSONL. Falls back to a
/// scan-by-id when the directory encoding does not match.
fn claude_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let projects = home.join(".claude").join("projects");
    let direct = projects
        .join(encode_claude_project_dir(cwd))
        .join(format!("{session_id}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(&projects).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The model's FINAL response text from its on-disk Claude session transcript: the
/// authoritative `result` text, else the last assistant text block. This is a robust
/// fallback for the planner's RUDDER_PLAN_TASKS block when the live PTY-stream
/// reconstruction missed it (a large block + PTY ring-buffer truncation) — the on-disk
/// transcript reliably carries the full final message.
fn claude_transcript_final_text(cwd: &Path, session_id: &str) -> Option<String> {
    let path = claude_transcript_path(cwd, session_id)?;
    let raw = std::fs::read_to_string(path).ok()?;
    parse_transcript_final_text(&raw)
}

/// Pure parser for `claude_transcript_final_text` (testable without a file): the last
/// `result` success text, else the last assistant text block.
fn parse_transcript_final_text(raw: &str) -> Option<String> {
    let mut last = String::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) == Some("result")
            && value.get("subtype").and_then(|t| t.as_str()) == Some("success")
        {
            if let Some(result) = value.get("result").and_then(|r| r.as_str()) {
                last = result.to_string();
            }
        }
        let message = value.get("message");
        if message.and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(blocks) = message
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            last = text.to_string();
                        }
                    }
                }
            }
        }
    }
    (!last.trim().is_empty()).then_some(last)
}

/// The approval-gate queue persisted to `.rudder/plan-queue.json` so a mid-plan restart
/// does not silently lose the queued DAG (the not-yet-launched nodes + the gate state).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PlanQueueSnapshot {
    #[serde(default)]
    planned_nodes: Vec<PlannedNode>,
    #[serde(default)]
    planned_origin: String,
    #[serde(default)]
    plan_request: String,
    #[serde(default)]
    plan_summary: Option<String>,
    #[serde(default)]
    awaiting_approval: bool,
}

fn load_plan_queue(cwd: &Path) -> Option<PlanQueueSnapshot> {
    let raw = std::fs::read_to_string(cwd.join(".rudder").join("plan-queue.json")).ok()?;
    serde_json::from_str::<PlanQueueSnapshot>(&raw).ok()
}

fn parse_worker_done_block(output: &str) -> Option<serde_json::Value> {
    const START: &str = "RUDDER_DONE_START";
    const END: &str = "RUDDER_DONE_END";
    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let start = clean.rfind(START)?;
    let after = &clean[start + START.len()..];
    let end = after.find(END)?;
    serde_json::from_str(after[..end].trim()).ok()
}

/// True if any line of `output` is EXACTLY `RUDDER_APPROVE_PLAN` once ANSI escapes and
/// markdown wrappers (backticks, bold asterisks, surrounding whitespace) are stripped. The
/// interactive orchestrator prints this on its own line to approve + launch the plan. The
/// exact full-line match means a mention inside prose, the plan block (`RUDDER_PLAN_TASKS_*`
/// is a different string), or a `RUDDER_APPROVE_PLAN_TEMPLATE` reference never triggers it.
fn output_has_approve_marker(output: &str) -> bool {
    let clean = strip_ansi_for_plan(output).replace('\r', "");
    clean.lines().any(line_is_approve_marker)
}

fn line_is_approve_marker(line: &str) -> bool {
    line.trim().trim_matches('`').trim_matches('*').trim() == "RUDDER_APPROVE_PLAN"
}

fn clear_orchestrator_plan_markers(repo_root: &Path) -> Result<()> {
    const START: &str = "RUDDER_PLAN_TASKS_START";
    const END: &str = "RUDDER_PLAN_TASKS_END";
    let path = orchestrator_plan_path(repo_root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };

    let mut changed = false;
    let mut in_plan_block = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if in_plan_block {
            changed = true;
            if trimmed == END {
                in_plan_block = false;
            }
            continue;
        }
        if trimmed == START {
            changed = true;
            in_plan_block = true;
            continue;
        }
        if line_is_approve_marker(line) || is_orchestrator_skill_marker(trimmed) {
            changed = true;
            continue;
        }
        kept.push(line);
    }

    if !changed {
        return Ok(());
    }
    let mut rewritten = kept.join("\n");
    if text.ends_with('\n') {
        rewritten.push('\n');
    }
    std::fs::write(path, rewritten.as_bytes())?;
    Ok(())
}

fn is_orchestrator_skill_marker(line: &str) -> bool {
    [
        "RUDDER_MODEL",
        "RUDDER_MAIN",
        "RUDDER_GOAL",
        "RUDDER_USAGE",
        "RUDDER_HELP",
        "RUDDER_LOGIN",
        "RUDDER_CLOUD",
        "RUDDER_REVIEW_ALL",
        "RUDDER_MERGE_ALL",
        "RUDDER_MERGE",
        "RUDDER_STOP",
        "RUDDER_REGOAL",
        "RUDDER_INJECT",
        "RUDDER_AUTOMERGE",
        "RUDDER_ADD_TASK",
        "RUDDER_REPLAN",
        "RUDDER_PLAN",
        "RUDDER_ASK",
    ]
    .iter()
    .any(|marker| line == *marker || line.starts_with(&format!("{marker} ")))
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
