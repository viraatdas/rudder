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
    CellContents, PtyOutputWaker, StyledTerminalCell, TerminalCommand, TerminalCursor,
    TerminalPane, TerminalPaneOptions, TerminalSize,
};

/// A single message the main event loop selects on. Either a terminal input
/// event read off stdin by a dedicated reader thread, or a nudge from a PTY
/// reader thread signalling that fresh child output is ready to drain.
enum LoopSignal {
    Term(Event),
    PtyOutput,
}

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
mod perf;
use crate::perf::*;
mod lifecycle;
use crate::lifecycle::*;
mod publish;
use crate::publish::*;
mod handoff;
use crate::handoff::*;
mod plan_stream;
mod telemetry;

mod signals;
use crate::plan_stream::*;

const TICK_RATE: Duration = Duration::from_millis(33);
/// How long injected text sits in a live agent's input box before the deferred
/// CR submits it. Long enough that Claude's Ink paste detection has closed its
/// burst window; short enough to be imperceptible.
const ENTER_SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// A deferred Enter for text injected into a live agent TUI (see
/// `App::pending_enters`).
struct PendingEnter {
    run_id: String,
    due: Instant,
}
/// Animation-only redraw cadence. Real PTY output wakes the loop immediately; this
/// interval only advances the idle spinner.
const SPINNER_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const MAX_EVENTS_PER_FRAME: usize = 64;
/// Trackpad wheel bursts can deliver many tiny scroll events. The handler itself
/// is cheap; the expensive part is flushing a full terminal draw. Briefly defer
/// scroll-triggered paints so queued wheel deltas collapse into fewer flushes.
const SCROLL_DRAW_DEFER: Duration = Duration::from_millis(8);
/// After this brief output lull, re-check the screen for permission/user-input prompts.
const READY_EVAL_LULL: Duration = Duration::from_millis(900);
// Dashboard colors now live in `theme.rs` (FOCUS_COLOR, INACTIVE_COLOR, ...),
// re-exported above so call sites are unchanged.
const DEFAULT_WHEEL_SCROLL_ROWS: u16 = 3;
const TASK_HISTORY_LIMIT: usize = 100;
/// How many recent conversations the `/handoff` palette offers. Each one costs a
/// transcript head read, and a list longer than the popup is not a list.
const HANDOFF_PICKER_LIMIT: usize = 8;
const MOUSE_DEBUG_ENV: &str = "RUDDER_MOUSE_DEBUG";
const RUDDER_MOUSE_ENABLE_SEQUENCES: &[u8] = b"\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const RUDDER_MOUSE_DISABLE_SEQUENCES: &[u8] = b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const SLOW_FRAME_THRESHOLD: Duration = Duration::from_millis(66);
const SLOW_POLL_THRESHOLD: Duration = Duration::from_millis(33);
const SLOW_DRAW_THRESHOLD: Duration = Duration::from_millis(33);
const SLOW_PTY_DRAIN_THRESHOLD: Duration = Duration::from_millis(10);
const SLOW_SCROLL_THRESHOLD: Duration = Duration::from_millis(5);
const SLOW_LINE_RENDER_THRESHOLD: Duration = Duration::from_millis(5);
// The Codex-side code-review profile (Claude plans review on Opus/Medium — see
// `review_agent_profile`). Review runs the installed `thermonuclear` skill.
// gpt-5.5 is Codex's current default/latest; an aspirational "gpt-5.6" is NOT a
// real model — Codex rejects it ("not supported with a ChatGPT account"). Bump
// this when a newer Codex model actually ships and appears in models.rs.
const REVIEW_ALL_MODEL: &str = "gpt-5.5";
const REVIEW_ALL_EFFORT: EffortLevel = EffortLevel::High;
const TASK_SUMMARY_MODEL: &str = "claude-haiku-4-5-20251001";
/// Default cap on how many plan-launched agents may run at once. Overridable via
/// `orchestrator.maxParallel` in ~/.rudder/config.json. This is what makes nodes
/// visibly wait in Todo and move to In Progress as slots free.
const DEFAULT_MAX_PARALLEL: usize = 1000;
/// Polls that shell out to another process start at their base interval and, when
/// they keep returning the same answer, double up to the cap. Anything that could
/// plausibly change the answer resets them to base, so responsiveness is preserved
/// where it matters and an idle dashboard stops spawning processes for nothing.
const REMOTE_STATE_BASE_INTERVAL: Duration = Duration::from_secs(5);
const REMOTE_STATE_MAX_INTERVAL: Duration = Duration::from_secs(300);
const WORKSPACE_CHECK_BASE_INTERVAL: Duration = Duration::from_secs(30);
/// Held lower than the remote-state cap: a cloud workspace can start or stop
/// without any local signal to key off, so this one has to keep looking.
const WORKSPACE_CHECK_MAX_INTERVAL: Duration = Duration::from_secs(120);

/// Doubles a poll interval, capped. Used by the process-spawning background polls.
fn backed_off_interval(current: Duration, cap: Duration) -> Duration {
    current.saturating_mul(2).clamp(Duration::from_secs(1), cap)
}

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
/// touch, running agents' status badge).
pub(crate) const SPINNER_FRAMES: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];
const AGENT_PANE_HINTS: &[&str] = &[
    "j/k move",
    "⌥j/k scroll",
    "⌥h/l agents",
    "Enter focus",
    "r rename",
    "v diff",
    "g nest",
    "R review all",
    "m merge",
    "M merge all",
    "o web ui",
    "x stop",
    "b branch",
    "dd delete",
    "cc clear merged",
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
    PlanReview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanReviewField {
    Title,
    Goal,
    Success,
    Prompt,
    HardDeps,
    SoftDeps,
}

impl PlanReviewField {
    const ALL: [Self; 6] = [
        Self::Title,
        Self::Goal,
        Self::Success,
        Self::Prompt,
        Self::HardDeps,
        Self::SoftDeps,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Goal => "goal",
            Self::Success => "done when",
            Self::Prompt => "prompt",
            Self::HardDeps => "hard deps",
            Self::SoftDeps => "soft deps",
        }
    }

    fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

impl Default for PlanReviewField {
    fn default() -> Self {
        Self::Title
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PlanReviewDraftNode {
    id: String,
    title: String,
    goal: String,
    success: String,
    prompt: String,
    hard_deps: String,
    soft_deps: String,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl PlanReviewDraftNode {
    fn from_planned(node: &PlannedNode) -> Self {
        Self {
            id: node.id.clone(),
            title: node.title.clone(),
            goal: node.goal.clone().unwrap_or_default(),
            success: node.success.clone().unwrap_or_default(),
            prompt: node.prompt.clone(),
            hard_deps: node.deps.join(", "),
            soft_deps: node.soft_deps.join(", "),
            backend: node.backend.clone(),
            model: node.model.clone(),
            effort: node.effort.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PlanReviewState {
    nodes: Vec<PlanReviewDraftNode>,
    selected: usize,
    field: PlanReviewField,
    cursor: usize,
    scroll: usize,
    cursor_row: Option<usize>,
    cursor_col: usize,
    dirty: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
    signature: String,
}

impl PlanReviewState {
    fn from_planned_nodes(nodes: &[PlannedNode]) -> Self {
        let drafts: Vec<PlanReviewDraftNode> = nodes
            .iter()
            .map(PlanReviewDraftNode::from_planned)
            .collect();
        let mut state = Self {
            nodes: drafts,
            signature: plan_review_signature(nodes),
            ..Self::default()
        };
        state.clamp_selection();
        state.cursor = state.active_text().chars().count();
        state
    }

    fn clamp_selection(&mut self) {
        if self.nodes.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.nodes.len() {
            self.selected = self.nodes.len().saturating_sub(1);
        }
    }

    fn active_node(&self) -> Option<&PlanReviewDraftNode> {
        self.nodes.get(self.selected)
    }

    fn active_text(&self) -> &str {
        let Some(node) = self.active_node() else {
            return "";
        };
        match self.field {
            PlanReviewField::Title => &node.title,
            PlanReviewField::Goal => &node.goal,
            PlanReviewField::Success => &node.success,
            PlanReviewField::Prompt => &node.prompt,
            PlanReviewField::HardDeps => &node.hard_deps,
            PlanReviewField::SoftDeps => &node.soft_deps,
        }
    }

    fn set_field(&mut self, field: PlanReviewField) {
        self.field = field;
        self.cursor = self.active_text().chars().count();
    }
}

fn plan_review_signature(nodes: &[PlannedNode]) -> String {
    serde_json::to_string(nodes).unwrap_or_default()
}

fn optional_nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Claude,
    Codex,
    /// opencode (https://opencode.ai) — a third CLI agent Rudder can drive. It
    /// speaks the same shapes Rudder needs: an interactive TUI, `--session <id>`
    /// resume, `--fork`, and `--prompt` for the first turn. What it does NOT have
    /// is a turn-end hook (see signals.rs) or a reasoning-effort flag, so those
    /// surfaces degrade rather than pretend.
    Opencode,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" | "anthropic" => Some(Self::Claude),
            "codex" | "openai" => Some(Self::Codex),
            "opencode" | "oc" => Some(Self::Opencode),
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
    Paused,
    Orphaned,
    Migrated,
}

impl AgentStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Merged => "merged",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Paused => "paused",
            Self::Orphaned => "orphaned",
            Self::Migrated => "migrated",
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
    /// A single conversational agent the user talks to in the MAIN checkout — NOT a
    /// DAG node. `/ask` used to spawn these; it is gone, and the survivors are
    /// `/restore` and `/resume --here`, which adopt an existing chat that was already
    /// about the user's real files. Structurally like Main (main checkout, no jj
    /// worktree, bypass-permissions tools), distinct in intent + its own list section.
    OneOff,
}

const MAIN_AGENT_ID: &str = "__main__";
/// Sentinel occupying `delete_pending` while a `c` clear-merged confirm is
/// armed. Run ids are numeric strings, so this can never collide with one.
const CLEAR_MERGED_PENDING: &str = "__clear-merged__";

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

/// Everything ONE `/plan` owns. Several orchestrators run at once, each driving its
/// own DAG, so none of this may be global: two plans reuse the same node ids (`n0`,
/// `n1`, …) and each has its own approval gate, refine/rebase turn, and final gate.
/// Anything a second plan could clobber lives here; genuinely dashboard-wide state
/// (the selected backend, `node_review_enabled`, the activity log) stays on `App`.
#[derive(Default)]
struct PlanState {
    /// Stable identity of this plan, minted when its `/plan` starts. `AgentRun.plan_id`
    /// and `PlannedNode.plan_id` point back at it, which is what lets a node resolve to
    /// its own plan when node ids collide across plans. The orchestrator ROW is found by
    /// `plan_id`, not stored here, so a relaunched orchestrator (refine, rebase) does not
    /// strand the plan.
    id: String,
    /// Queue of planned (not-yet-launched) DAG nodes. The scheduler drains these
    /// into live agents as their hard deps merge and parallelism slots free.
    /// Rendered in the Todo section. A new completed plan replaces the queue.
    planned_nodes: Vec<PlannedNode>,
    /// Durable facts for this plan. Queue membership is not sufficient: a
    /// crash can happen after a worker run is persisted but before the shrunken
    /// queue is saved, and presentation cleanup may delete merged run rows.
    plan_launched_node_ids: HashSet<String>,
    plan_merged_node_ids: HashSet<String>,
    /// Node ids that passed the optional per-node review gate (eligible to merge);
    /// `node_reviewers` maps a live reviewer run id -> the node id it reviews.
    reviewed_nodes: HashSet<String>,
    node_reviewers: HashMap<String, String>,
    /// A plan file is accepted only after an explicit new planning turn arms
    /// capture. An empty queue/all-merged fleet is never evidence of a new plan.
    plan_capture_armed: bool,
    /// Editable projection of `planned_nodes` while an approval-gated plan is being
    /// reviewed in the worker pane. Draft edits are committed back to
    /// `planned_nodes` only after validation, so an invalid dependency edit cannot
    /// launch or persist accidentally.
    plan_review: PlanReviewState,
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
    final_gate_status: FinalGateStatus,
    final_gate_summary: Option<String>,
    /// True while this plan's orchestrator is RE-PLANNING in response to refinement
    /// feedback (between relaunch and the revised DAG being captured). It lets plan
    /// detection run even though `awaiting_approval` is still true (so the refined plan
    /// is captured), blocks premature approval of the stale plan, and is cleared the
    /// moment the revised plan lands (or the re-plan fails).
    refining: bool,
    /// Set while a STRUCTURAL plan-rebase is in flight: the orchestrator session has
    /// been resumed with a `build_rebase_request` (a mid-flight pivot), and we are
    /// waiting for its revised DAG. Like `refining` it lets plan detection run even
    /// though a plan is already active (post-approval, nodes launched), but it routes
    /// capture to `evaluate_completed_rebase` (build-forward diff/apply) instead of
    /// `evaluate_completed_plan`. Suppresses integration while set so the zones
    /// stay stable until the diff is applied. Cleared the moment the rebase lands or fails.
    rebasing: bool,
    /// Set alongside `rebasing` when the rebase was triggered on the LIVE INTERACTIVE
    /// conductor (the backend PTY the user is conversing with), e.g. via the
    /// conductor's own `RUDDER_REPLAN` marker. The rebase relaunches that row as a
    /// headless re-decompose, which tears down the interactive PTY; this flag tells the
    /// rebase evaluator to re-spawn the interactive conductor (resuming the same session)
    /// once the diff lands or fails, so the user never loses the conversation.
    rebase_restore_interactive: bool,
    /// While true, a plan has been parsed into `planned_nodes` but is awaiting the
    /// user's APPROVAL gate: nothing launches. Enter approves (clears this and runs
    /// the scheduler); d removes the selected node, or discards the whole plan when
    /// the orchestrator is selected. Set on streaming plan detection; cleared once
    /// the user approves (or discards the plan).
    awaiting_approval: bool,
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
    /// Waker installed on every PTY pane so child output wakes the main event loop
    /// immediately (see `run`). `None` until `run` builds it; panes spawned while it
    /// is `None` (e.g. during construction) fall back to the tick cadence.
    pty_output_waker: Option<PtyOutputWaker>,
    /// Deferred Enter presses for text injected into a live agent TUI. Claude's
    /// Ink input treats a text+CR burst arriving in one read as a single paste
    /// and swallows the CR (the prompt fills but never submits), so injection
    /// sites write the text as a bracketed paste and queue the CR here; it is
    /// flushed as a separate write once ENTER_SUBMIT_DELAY has passed.
    pending_enters: Vec<PendingEnter>,
    /// Injected tasks waiting to be folded into the active plan, one at a time.
    /// Without this, rapid injections each spawned their own RudderPlan planner
    /// and the agent pane filled with 5+ concurrent "planning" rows.
    pending_reconcile_inputs: Vec<String>,
    cwd: PathBuf,
    branch: Option<String>,
    task_input: String,
    task_cursor: usize,
    /// Full text of pastes collapsed into "[Pasted #N …]" chips in `task_input`.
    /// Lets a re-paste toggle a chip open/closed and lets a submit expand every
    /// chip back to its real content. Cleared whenever the draft is cleared.
    pasted_chunks: Vec<PastedChunk>,
    task_history: Vec<String>,
    task_history_index: Option<usize>,
    task_history_draft: String,
    agents: Vec<AgentRun>,
    /// Every live plan, in the order its `/plan` was started. Several orchestrators
    /// may run at once and each owns exactly one entry, so plan state is per-plan
    /// rather than global. NEVER empty: `App::new` seeds one plan (holding whatever
    /// `plan-queue.json` restored) so the accessors below can always resolve one.
    plans: Vec<PlanState>,
    /// Per-node auto-review gate. When enabled, a finished plan node is reviewed
    /// (thermonuclear skill, auto-fix in its own workspace) before it auto-merges.
    /// OFF by default — this automatic review is deliberately disabled pending a
    /// better design; opt in with `RUDDER_NODE_REVIEW=1`. It is a dashboard-wide
    /// setting, not a per-plan one, so it stays on `App`.
    node_review_enabled: bool,
    final_gate_tx: mpsc::Sender<FinalGateResult>,
    final_gate_rx: mpsc::Receiver<FinalGateResult>,
    /// Whether Claude Code's native fast mode is on for NEW agents (toggled by `/fast`).
    /// Display/UX mirror of the persisted `fastMode` config flag; the authoritative
    /// value injected into a worker's `--settings` is read from config at launch.
    /// Claude-only — Codex has no equivalent (its `/fast` just uses low reasoning effort).
    fast_mode: bool,
    /// Localhost URL of the live web board (http://127.0.0.1:PORT), passed in by the
    /// Node parent via RUDDER_BOARD_URL when the in-process board daemon is running.
    /// None when launched without a board. Surfaced in the agents pane + opened by
    /// `/web` and the `o` key so users can live-monitor and steer from the browser.
    board_url: Option<String>,
    /// Whether the orchestrator runs as an INTERACTIVE backend PTY (the default; see
    /// `interactive_orchestrator()`) vs the headless decomposer. Snapshotted from the env
    /// ONCE at construction so render/poll/key paths read a per-App field instead of the
    /// process-global env (which races across parallel tests). Tests set it directly.
    interactive_orchestrator: bool,
    /// Tick counter used to run the scheduler on a coarse cadence rather than on
    /// every PTY-byte tick.
    scheduler_tick: u64,
    /// Animation frame for the orchestrator spinner. Advances at most every 100ms.
    spinner_frame: usize,
    last_spinner_advance: Instant,
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
    /// When Esc dismisses the command palette, remember the exact input it was
    /// dismissed for: while `task_input` still equals this, `suggestions_for`
    /// stays empty. Any edit changes the input and re-enables the palette. This
    /// lets Esc close the popup WITHOUT destroying a typed draft.
    picker_dismissed_input: Option<String>,
    /// Recent CLI conversations offered by the `/handoff` picker. Populated from
    /// disk while the user types `/handoff` (never on the render path — the picker
    /// is consulted every frame, and a readdir + transcript read per frame is
    /// exactly the kind of work that made the dashboard burn CPU before).
    handoff_candidates: Vec<ConversationCandidate>,
    handoff_candidates_at: Option<Instant>,
    /// Codex and opencode conversations, fetched OFF-THREAD: Codex means walking
    /// `~/.codex/sessions` and opencode means a ~0.5s `opencode session list`
    /// subprocess. Doing either on the input thread would stall the dashboard for
    /// a keystroke. Kept separately so a plain refresh does not drop them.
    handoff_extra_candidates: Vec<ConversationCandidate>,
    handoff_extra_rx: Option<mpsc::Receiver<Vec<ConversationCandidate>>>,
    /// One `dashboard_opened` event per session, not per App (tests build many).
    dashboard_open_emitted: bool,
    /// Last heartbeat sentence written, so an unchanged one is not repeated.
    last_heartbeat_summary: Option<String>,
    /// Rows whose workspace is gone from disk, refreshed by the reconcile pass.
    /// A derived fact lives here rather than on the row: it is a question for the
    /// filesystem, asked again every pass, never persisted.
    rows_missing_workspace: HashSet<String>,
    /// Last full reconcile against the authorities (jj, the filesystem, the
    /// backends' session stores). Cheap, but not every-tick cheap.
    last_reconcile: Option<Instant>,
    /// Backoff for `ensure_session_ids_recorded`: a row whose backend never wrote
    /// a session (it died at launch) must not make the dashboard rescan forever.
    session_id_attempts: HashMap<String, u32>,
    session_id_last_try: HashMap<String, Instant>,
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
    /// Largest scroll offset from the last rendered orchestrator DAG/transcript
    /// viewport. Mouse wheel handling uses this to resume live bottom-following
    /// once the user scrolls back to the end.
    orch_dag_max_scroll: usize,
    /// Live planning transcripts should follow the bottom until the user scrolls
    /// up. Once they scroll back to the bottom, following resumes.
    orch_follow_bottom: bool,
    agents_area: Option<Rect>,
    worker_area: Option<Rect>,
    task_area: Option<Rect>,
    cloud_connected: bool,
    cloud_runtime: Option<String>,
    last_cloud_check: Instant,
    /// Refreshes whether each locally integrated Git commit is contained by its
    /// origin tracking branch. This is local-only and never performs a fetch.
    last_remote_state_check: Instant,
    /// Current gap between those refreshes. Each pass spawns one `git merge-base`
    /// per merged-but-unpushed agent, and `origin/<bookmark>` can only move on a
    /// fetch or push - so a fixed 5s cadence spawned thousands of processes a day
    /// to re-learn an unchanged answer. The interval backs off while the answer
    /// holds still and snaps back the moment it changes.
    remote_state_interval: Duration,
    /// How many merged-but-unpushed agents the last refresh watched. A newly
    /// merged agent has to be noticed promptly, so a rise here resets the backoff
    /// rather than leaving the new row's push badge stale for minutes.
    remote_state_watched: usize,
    /// Whether this repo publishes reviewed work as pull requests instead of
    /// merging it locally. `Unknown` until the first probe lands — deliberately
    /// distinct from `Inactive`, because treating "not asked yet" as "cannot
    /// publish" would make the first `m` after launch take the wrong road.
    publish: PublishState,
    publish_probe_rx: Option<mpsc::Receiver<PublishState>>,
    /// Detection spawns up to four processes, two of them network round-trips, to
    /// re-learn an answer (gh installed? logged in? which remote?) that changes
    /// roughly never. Backed off per AGENTS.md 12.8.
    publish_probe_interval: Duration,
    last_publish_probe: Option<Instant>,
    /// PR state (draft/open/merged) is GitHub's answer, re-derived rather than
    /// stored (12.7). One `gh pr list` covers every published row.
    publish_pr_state_rx: Option<mpsc::Receiver<HashMap<u64, String>>>,
    publish_pr_state_interval: Duration,
    last_publish_pr_state: Option<Instant>,
    /// Cadence gate for the merged-workspace GC sweep (gc_merged_workspaces).
    last_worktree_gc: Instant,
    /// Throttle for the web-board steer inbox poll (.rudder/steer/*.json): checked
    /// ~once/sec from poll_agents so a browser "steer" reaches the right agent's PTY.
    last_steer_poll: Instant,
    /// Throttle for the periodic activity-feed heartbeat: emits a one-line "what's
    /// happening" status into .rudder/activity.jsonl every ~45s while work is live.
    last_heartbeat_emit: Instant,
    cloud_workspace: Option<CloudWorkspaceStatus>,
    last_workspace_check: Option<Instant>,
    /// Current gap between cloud workspace polls. Each poll spawns the Node CLI
    /// (`rudder cloud workspace status`), which costs far more than the answer is
    /// usually worth, so this backs off while the status is unchanged.
    workspace_check_interval: Duration,
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
    /// True while the rename box still holds the untouched current name as a
    /// preview: the first typed character wipes it so the user retypes from
    /// scratch (Finder-style "prefill is selected"). Any edit/nav key instead
    /// keeps the name and clears this, so editing-in-place still works.
    rename_prefilled: bool,
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
    /// Opt-in native TUI perf diagnostics (`RUDDER_NATIVE_PERF=1`) plus in-memory
    /// latency samples for the optional HUD (`RUDDER_PERF_HUD=1`).
    perf: PerfLogger,
    perf_stats: PerfStats,
    scroll_draw_defer_until: Option<Instant>,
    scroll_events_since_draw: usize,
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

#[derive(Debug)]
enum MergeIntent {
    Selected { id: String, task: String },
    All { ids: Vec<String> },
    /// The FIRST publish in a repo. Publishing is otherwise unprompted, so this
    /// intent exists only to carry that one-time consent: accepting it records the
    /// acceptance for the remote and then publishes.
    Publish { id: String, task: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewAllSource {
    id: String,
    revision: String,
    task: String,
    summary: String,
    worktree_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReviewAllPremerge {
    merged_revisions: Vec<String>,
    stopped_revision: Option<String>,
    stopped_error: Option<String>,
    remaining_revisions: Vec<String>,
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
    integration: IntegrationEvidence,
    /// What happened when this row was published as a pull request: the branch,
    /// the PR number and its url. Empty on a repo where publishing is inactive
    /// (there `m` merges locally and no PR ever exists). The PR's STATE is not in
    /// here as stored data — see `PublishEvidence`.
    publish: PublishEvidence,
    /// First-class outcome evidence for an explicitly requested push/deploy,
    /// orthogonal to process status and local jj integration.
    delivery: DeliveryEvidence,
    session_id: Option<String>,
    terminal: Option<TerminalPane>,
    terminal_size: Option<TerminalSize>,
    review_terminal: Option<TerminalPane>,
    review_size: Option<TerminalSize>,
    review_error: Option<String>,
    last_output_at: Instant,
    completed_at: Option<Instant>,
    autosteered: bool,
    /// A pane running a plain process (a rudder CLI command such as `undo`)
    /// rather than an agent conversation. Its completion IS process exit, so
    /// the worker-lifecycle-hooks sweep must not police it.
    plain_process: bool,
    /// True only for an orchestrator launched as a live interactive PTY. Headless
    /// planners also use `AgentMode::RudderPlan`, so this must not be inferred from
    /// `autosteered` once a plan is captured.
    interactive_orchestrator: bool,
    needs_permission: bool,
    needs_user_input: bool,
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
    /// The plan this run belongs to (`PlanState::id`): set on the orchestrator row that
    /// drives the plan AND on every worker the plan launches. Node ids (`n0`, `n1`, …)
    /// repeat across concurrent plans, so `node_id` alone cannot identify a node — the
    /// pair does. `None` for manually started agents that belong to no plan.
    plan_id: Option<String>,
    /// When the user last looked at this row's diff, and the gate between done and
    /// merged: work does not reach the user's branch until someone has actually seen
    /// what it changed. Set by opening the diff, because "you have seen this" is a
    /// claim the dashboard can honestly make; "you approved this" is not.
    ///
    /// Deliberately an attribute rather than an `AgentStatus` variant. Review is
    /// orthogonal to the lifecycle — a row is Done whether or not it has been read —
    /// and adding a status would mean auditing every `== AgentStatus::Done` in the
    /// codebase to also admit the new one, which is how a state machine grows holes.
    /// The agents list still RENDERS it as a state, because that is what makes the
    /// gate visible.
    ///
    /// Distinct from a plan's `reviewed_nodes`/`node_reviewers`, which track AI
    /// reviewer AGENTS over DAG nodes and are off by default. This is the human, and
    /// it applies to any row with a workspace, plan node or not.
    reviewed_at: Option<String>,
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
    /// Parsed DAG/summary derived from the latest semantic planner-stream update.
    /// `Some` with no tasks is a cached negative result, preventing the heartbeat and
    /// render paths from reparsing identical incomplete output dozens of times a second.
    plan_output_cache: Option<RudderPlanOutputCache>,
    /// When the user last sent input (a keystroke/prompt) to this agent's PTY. Used
    /// to decide whether post-completion output is a genuine NEW turn (user typed)
    /// vs incidental repaint (e.g. a resize when the pane is focused). Without this,
    /// highlighting a finished agent flips it from done back to in-progress.
    last_worker_input_at: Option<Instant>,
    /// True when this row is currently an AI merge-conflict resolver (its jj merge
    /// recorded a conflict and an agent is resolving it in place). When it finishes
    /// with no conflicts left, the merge is finalized (the node flips to Merged so
    /// its children unblock); if conflicts remain it drops to manual.
    merge_resolver: bool,
    /// True when the run's work completed but its integration is parked on a merge
    /// conflict. This is distinct from `merge_resolver`: a parked conflict is waiting
    /// for the user to press `m` (or approve a resolver), while a resolver is actively
    /// running in this same row.
    merge_conflict: bool,
    merge_conflict_operation: ConflictOperation,
    merge_conflict_files: Vec<String>,
    /// Durable "an integration conflict happened at some point this run" marker.
    /// Unlike `merge_conflict` (LIVE state, cleared once a resolver succeeds or the
    /// run is re-goaled), this is never cleared and persists to run.json as
    /// `hadMergeConflict`, so telemetry (the improve loop's mergeConflictRate) still
    /// counts conflicts that were auto-resolved before it read the record.
    had_merge_conflict: bool,
    /// The worker's own account of WHAT IT DID, taken from its completion note
    /// (`rudder done` sidecar / PTY block / diff-backstop summary) each time it
    /// finishes. Shown in the finished-worker card above the conversation, and
    /// refreshed on every subsequent completion, so a finished agent reads as
    /// "objective + what it did" instead of a dead terminal. Persists to run.json
    /// as `doneSummary`.
    done_summary: Option<String>,
    /// Best-effort cumulative token usage read from the backend's OWN session log
    /// (claude project jsonl / codex rollout) when a turn completes. Interactive
    /// PTYs expose no usage stream, so this is the only cost signal a native run
    /// has; it persists to run.json as `tokens` for telemetry (the improve loop's
    /// cost accounting reads run.tokens, which the TS __worker path also writes).
    tokens_in: u64,
    tokens_out: u64,
}

#[derive(Debug)]
struct TaskSummaryResult {
    run_id: String,
    title: Option<String>,
}

/// Result of the completion-note BACKSTOP: a one-shot summarizer reconstructed a report
/// (or not) for a worker that finished without filing one. `note` is the completion-note
/// JSON shape; `None` when the summarizer failed or had nothing.
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

    /// Restore a resolver-relabeled run's original identity once its conflict
    /// is gone. start_conflict_resolution_agent overwrites task/task_summary
    /// with the resolver labels ("Resolve merge conflicts: …" / "merge
    /// conflicts → …") so the pane reads as conflict work while it happens,
    /// but nothing ever put the original back: every run that ever conflicted
    /// kept the resolver label forever and the merged list lost what each run
    /// actually did (observed on ALL merged runs of a real project). The
    /// original task is recovered from the label itself, so this also works
    /// for records reloaded after a restart. Telemetry keeps its conflict
    /// evidence through the durable had_merge_conflict marker, not the label.
    fn restore_pre_conflict_identity(&mut self) {
        let original = self
            .task
            .strip_prefix("Resolve merge conflicts: ")
            .or_else(|| self.task.strip_prefix("Resolve rebase conflicts: "))
            .map(str::to_string);
        if let Some(original) = original {
            if !original.trim().is_empty() {
                self.task_summary = summarize_task(&original);
                self.task = original;
            }
        }
    }

    /// Finished worker runs are mergeable only with a durable jj workspace/change.
    fn has_merge_source(&self) -> bool {
        self.worktree_path.is_some() || self.workspace_name.is_some() || self.jj_change_id.is_some()
    }

    fn has_merge_conflict(&self) -> bool {
        self.merge_conflict
    }

    /// A "pinned planner" renders in the orchestrator section at the top of the list
    /// (above main + every status bucket), not in a status bucket. The headless
    /// RudderPlan orchestrator is the only such row.
    pub(crate) fn is_pinned_planner(&self) -> bool {
        self.is_orchestrator()
    }
}

fn merge_label_for_run(run: &AgentRun) -> String {
    if let Some(node_id) = run.node_id.as_ref().filter(|id| !id.trim().is_empty()) {
        return node_id.clone();
    }
    if !run.task_summary.trim().is_empty() {
        return truncate_chars(run.task_summary.trim(), 24);
    }
    let task = short_task(&run.task);
    if !task.trim().is_empty() {
        return truncate_chars(&task, 24);
    }
    truncate_chars(&run.id, 18)
}

fn merge_all_labels(runs: &[&AgentRun]) -> Vec<String> {
    runs.iter().map(|run| merge_label_for_run(run)).collect()
}

fn summarize_labels(labels: &[String], limit: usize) -> String {
    let shown = labels
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = labels.len().saturating_sub(limit);
    if remaining == 0 {
        shown
    } else if shown.is_empty() {
        format!("+{remaining} more")
    } else {
        format!("{shown}, +{remaining} more")
    }
}

#[derive(Clone, Debug)]
struct AgentTurn {
    ts: String,
    prompt: String,
    source: String,
}

enum SteerDelivery {
    Delivered,
    Failed(String),
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

/// One request to fork an existing CLI conversation into a fresh worker row.
struct ForkedConversation {
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
    session_id: String,
    /// Directory the SOURCE conversation ran in. Claude resolves `--resume` against
    /// the current directory's transcript folder, so the fork needs to know where
    /// the transcript came from in order to stage it.
    source_cwd: PathBuf,
    /// jj change the new workspace starts from; None starts from the dashboard's.
    base_change: Option<String>,
    /// Workspace label, also the row title.
    label: String,
    /// The `task` text recorded on the row.
    task: String,
    /// First turn handed to the fork. None opens it waiting for the user to type.
    seed: Option<String>,
}

enum ForkOutcome {
    Started,
    /// The row exists (marked Failed) but the backend process never started.
    SpawnFailed(String),
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
    /// Index of the plan the UI is currently acting on. DERIVED from the selection
    /// (12.7: ask the authority, do not cache): the plan owned by the selected row —
    /// its orchestrator, or the worker whose node belongs to it — falling back to the
    /// most recently started plan. That fallback is what keeps a single-plan session
    /// behaving exactly as before, when there was only one global plan.
    fn active_plan_index(&self) -> usize {
        if let Some(id) = self
            .agents
            .get(self.selected_agent)
            .and_then(|run| run.plan_id.as_deref())
        {
            if let Some(index) = self.plans.iter().position(|plan| plan.id == id) {
                return index;
            }
        }
        self.plans.len().saturating_sub(1)
    }

    /// The plan the UI is currently acting on. `plans` is never empty (see the field),
    /// so this cannot panic.
    fn plan(&self) -> &PlanState {
        &self.plans[self.active_plan_index()]
    }

    fn plan_mut(&mut self) -> &mut PlanState {
        let index = self.active_plan_index();
        &mut self.plans[index]
    }

    /// Which plan owns `node_id`. Node ids repeat across concurrent plans, so this asks
    /// the two authorities that actually know — the launched run's `plan_id`, then the
    /// queue a node with that id still sits in — before falling back to the active plan
    /// (the single-plan answer, and the only one when the node is brand new).
    fn plan_id_for_node(&self, node_id: &str) -> String {
        if let Some(id) = self
            .agents
            .iter()
            .find(|run| run.node_id.as_deref() == Some(node_id))
            .and_then(|run| run.plan_id.clone())
        {
            return id;
        }
        if let Some(plan) = self
            .plans
            .iter()
            .find(|plan| plan.planned_nodes.iter().any(|node| node.id == node_id))
        {
            return plan.id.clone();
        }
        self.plan().id.clone()
    }

    /// The orchestrator row driving `plan_id`. Several orchestrators are live at once,
    /// so every path that used to grab "the" orchestrator must name the plan it means.
    /// A row restored from disk (or one that predates plan ids) carries no `plan_id`;
    /// it is adopted only when there is a single plan, where it cannot be ambiguous.
    fn orchestrator_index_for_plan(&self, plan_id: &str) -> Option<usize> {
        if let Some(index) = self
            .agents
            .iter()
            .position(|run| run.is_orchestrator() && run.plan_id.as_deref() == Some(plan_id))
        {
            return Some(index);
        }
        (self.plans.len() == 1)
            .then(|| {
                self.agents
                    .iter()
                    .position(|run| run.is_orchestrator() && run.plan_id.is_none())
            })
            .flatten()
    }

    /// The orchestrator of the plan the UI is acting on.
    fn active_orchestrator_index(&self) -> Option<usize> {
        let plan_id = self.plan().id.clone();
        self.orchestrator_index_for_plan(&plan_id)
    }

    /// A plan with nothing left to lose: no queue, nothing at the approval gate, no
    /// planner turn in flight, and no worker of its own still short of merged. A fresh
    /// `/plan` reuses such a slot instead of stacking a new one, which is what keeps
    /// repeated single-plan use looking exactly as it did before plans could coexist.
    fn spent_plan_index(&self) -> Option<usize> {
        (0..self.plans.len()).find(|&index| {
            let plan = &self.plans[index];
            if !plan.planned_nodes.is_empty()
                || plan.awaiting_approval
                || plan.refining
                || plan.rebasing
                || plan.planner_paused_for_input
            {
                return false;
            }
            if self
                .orchestrator_index_for_plan(&plan.id)
                .is_some_and(|i| self.agents[i].status == AgentStatus::Running)
            {
                return false;
            }
            !self
                .plan_agents(index)
                .any(|run| run.node_id.is_some() && run.status != AgentStatus::Merged)
        })
    }

    /// Adopt freshly parsed nodes into `plan_index`: stamp each with its owning plan and
    /// RENAME any id another live plan already uses, rewriting this plan's own deps to
    /// match. `AgentRun.node_id` is the join key everywhere (the agents pane, the merge
    /// ledgers, graph.json), so two plans both calling a node `n0` would silently cross
    /// their wires. Renaming only fires on an actual collision, so a lone plan keeps the
    /// planner's `n0`, `n1`, … verbatim and reads exactly as it always has.
    fn adopt_plan_nodes(&mut self, plan_index: usize, mut nodes: Vec<PlannedNode>) {
        let plan_id = self.plans[plan_index].id.clone();
        let taken: HashSet<String> = self
            .plans
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != plan_index)
            .flat_map(|(_, plan)| {
                plan.planned_nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .chain(plan.plan_launched_node_ids.iter().cloned())
            })
            .chain(
                self.agents
                    .iter()
                    .filter(|run| run.plan_id.as_deref() != Some(plan_id.as_str()))
                    .filter_map(|run| run.node_id.clone()),
            )
            .collect();
        let mut renames: HashMap<String, String> = HashMap::new();
        let mut used: HashSet<String> = nodes.iter().map(|node| node.id.clone()).collect();
        for node in &mut nodes {
            if !taken.contains(&node.id) {
                continue;
            }
            let mut suffix = 2usize;
            let fresh = loop {
                let candidate = format!("{}-{suffix}", node.id);
                if !taken.contains(&candidate) && !used.contains(&candidate) {
                    break candidate;
                }
                suffix += 1;
            };
            used.insert(fresh.clone());
            renames.insert(node.id.clone(), fresh.clone());
            node.id = fresh;
        }
        for node in &mut nodes {
            node.plan_id = plan_id.clone();
            for dep in node.deps.iter_mut().chain(node.soft_deps.iter_mut()) {
                if let Some(fresh) = renames.get(dep) {
                    *dep = fresh.clone();
                }
            }
        }
        self.plans[plan_index].planned_nodes = nodes;
    }

    /// Append one node to `plan_index` under the same adoption rules.
    fn push_plan_node(&mut self, plan_index: usize, node: PlannedNode) {
        let mut nodes = std::mem::take(&mut self.plans[plan_index].planned_nodes);
        nodes.push(node);
        self.adopt_plan_nodes(plan_index, nodes);
    }

    /// Plan that owns the `RUDDER.md` control channel. Prefers a RUNNING interactive
    /// orchestrator, then the most recent interactive orchestrator row whatever its
    /// state (a marker must not strand merely because the PTY finished first), and
    /// finally the plan the UI is acting on.
    fn marker_channel_plan_index(&self) -> usize {
        if let Some((_, plan_index)) = self.running_interactive_orchestrator_plan() {
            return plan_index;
        }
        if let Some(run_index) = self
            .agents
            .iter()
            .rposition(|run| self.is_interactive_orchestrator_run(run))
        {
            return self.plan_index_for_run(run_index);
        }
        self.active_plan_index()
    }

    /// Does `run` belong to the plan at `plan_index`? A run stamped with a plan id
    /// answers for itself. A run with NO plan id predates concurrent plans (restored
    /// from disk, or started before any `/plan`); with a single plan there is only one
    /// possible owner, so it is attributed there — which is what keeps a one-plan
    /// session behaving exactly as it did before plans could coexist. With several
    /// plans live an unstamped run is claimed by none of them.
    fn run_belongs_to_plan(&self, run: &AgentRun, plan_index: usize) -> bool {
        match run.plan_id.as_deref() {
            Some(id) => id == self.plans[plan_index].id,
            None => self.plans.len() == 1,
        }
    }

    /// The workers belonging to `plan_index`. Node ids are per-plan, so any question
    /// about "this plan's workers" has to filter by owner or it answers for the fleet.
    fn plan_agents(&self, plan_index: usize) -> impl Iterator<Item = &AgentRun> {
        self.agents
            .iter()
            .filter(move |run| self.run_belongs_to_plan(run, plan_index))
    }

    /// The workers belonging to the plan the UI is acting on.
    fn active_plan_agents(&self) -> impl Iterator<Item = &AgentRun> {
        self.plan_agents(self.active_plan_index())
    }

    /// The running interactive orchestrator and the plan it drives, as `(run, plan)`
    /// indices. `RUDDER.md` is a single per-repo file, so the interactive orchestrator's
    /// FILE channel (DAG capture, `RUDDER_*` markers) belongs to whichever interactive
    /// conductor is live rather than to the selected pane. Headless orchestrators stream
    /// their DAG through their own PTY and are plan-scoped without this restriction, so
    /// any number of them run side by side.
    fn running_interactive_orchestrator_plan(&self) -> Option<(usize, usize)> {
        let run_index = self.agents.iter().position(|run| {
            self.is_interactive_orchestrator_run(run) && run.status == AgentStatus::Running
        })?;
        Some((run_index, self.plan_index_for_run(run_index)))
    }

    /// The plan the agent row at `run_index` belongs to. Plan capture, approval and
    /// rebase are driven by a specific orchestrator ROW, so they resolve their plan from
    /// that row rather than from what the user happens to have selected.
    fn plan_index_for_run(&self, run_index: usize) -> usize {
        self.agents
            .get(run_index)
            .and_then(|run| run.plan_id.as_deref())
            .and_then(|id| self.plans.iter().position(|plan| plan.id == id))
            .unwrap_or_else(|| self.active_plan_index())
    }

    /// Index of the plan owning `node_id`, for the paths that must write to that plan's
    /// ledgers (launch/merge facts, queue drain) rather than the selected one.
    fn plan_index_for_node(&self, node_id: &str) -> usize {
        let id = self.plan_id_for_node(node_id);
        self.plans
            .iter()
            .position(|plan| plan.id == id)
            .unwrap_or_else(|| self.active_plan_index())
    }

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
        let dashboard_color_mode = if cfg!(test) {
            ColorMode::Terminal
        } else {
            config::initial_color_mode()
        };
        set_color_mode(dashboard_color_mode);
        // The fleet and the live plans come back TOGETHER (see `restore_persisted_state`):
        // which planner rows survive depends on which plans returned, and which nodes a
        // plan still owes depends on which runs returned. It lives outside this
        // constructor so a test can drive the real restore against a temp repo — every
        // `App::new()` in the suite would otherwise read the developer's own `.rudder/`,
        // so this arm must stay short-circuited, and that is precisely the blind spot
        // that let two concurrent plans collapse into one across a restart with a green
        // suite behind it.
        let (agents, restored_plans) = if cfg!(test) {
            (Vec::new(), PlanQueueFile::default().into_plans())
        } else {
            restore_persisted_state(&cwd)
        };
        // Restore the ingested-run ledger so a worker handled before the last exit is not
        // re-ingested (or re-summarized) when its Done record reloads. Empty in tests.
        let followups_ingested = if cfg!(test) {
            HashSet::new()
        } else {
            load_ingested_runs(&cwd)
        };
        // Restore the auto-expansion depth map so MAX_FOLLOWUP_DEPTH survives a restart.
        let followup_gen = if cfg!(test) {
            HashMap::new()
        } else {
            load_followup_gen(&cwd)
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
        let (final_gate_tx, final_gate_rx) = mpsc::channel();
        let branch = current_branch_at(&cwd);
        let _ = signals::cleanup_old_signals(Duration::from_secs(7 * 24 * 60 * 60));
        let perf = PerfLogger::new();
        let perf_stats = PerfStats::new(perf.enabled());
        Self {
            focus: FocusPane::Task,
            nav_mode: false,
            leader_pending: false,
            worker_view: WorkerView::Terminal,
            nest_view: false,
            pty_output_waker: None,
            pending_enters: Vec::new(),
            pending_reconcile_inputs: Vec::new(),
            cwd,
            branch,
            task_input,
            task_cursor,
            pasted_chunks: Vec::new(),
            task_history: Vec::new(),
            task_history_index: None,
            task_history_draft: String::new(),
            agents,
            plans: restored_plans,
            // OFF by default: the automatic per-node review is disabled pending a
            // better design. Opt in explicitly with RUDDER_NODE_REVIEW=1.
            node_review_enabled: std::env::var("RUDDER_NODE_REVIEW")
                .map(|value| {
                    let value = value.trim();
                    value == "1" || value.eq_ignore_ascii_case("true")
                })
                .unwrap_or(false),
            final_gate_tx,
            final_gate_rx,
            fast_mode: if cfg!(test) {
                false
            } else {
                config::fast_mode_enabled()
            },
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
            interactive_orchestrator: interactive_orchestrator(),
            scheduler_tick: 0,
            spinner_frame: 0,
            last_spinner_advance: Instant::now(),
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
            picker_dismissed_input: None,
            handoff_candidates: Vec::new(),
            handoff_candidates_at: None,
            handoff_extra_candidates: Vec::new(),
            handoff_extra_rx: None,
            dashboard_open_emitted: false,
            last_heartbeat_summary: None,
            rows_missing_workspace: HashSet::new(),
            last_reconcile: None,
            session_id_attempts: HashMap::new(),
            session_id_last_try: HashMap::new(),
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
            orch_dag_max_scroll: 0,
            orch_follow_bottom: true,
            agents_area: None,
            worker_area: None,
            task_area: None,
            cloud_connected: cloud.connected,
            cloud_runtime: cloud.runtime,
            last_cloud_check: Instant::now(),
            last_remote_state_check: Instant::now(),
            remote_state_interval: REMOTE_STATE_BASE_INTERVAL,
            remote_state_watched: 0,
            publish: PublishState::Unknown,
            publish_probe_rx: None,
            publish_probe_interval: PUBLISH_PROBE_BASE_INTERVAL,
            last_publish_probe: None,
            publish_pr_state_rx: None,
            publish_pr_state_interval: PUBLISH_PR_STATE_BASE_INTERVAL,
            last_publish_pr_state: None,
            last_worktree_gc: Instant::now(),
            last_steer_poll: Instant::now(),
            last_heartbeat_emit: Instant::now(),
            cloud_workspace: None,
            last_workspace_check: None,
            workspace_check_interval: WORKSPACE_CHECK_BASE_INTERVAL,
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
            rename_prefilled: false,
            diff_summary_cache: HashMap::new(),
            dirty: true,
            last_tab_emoji: None,
            session_started_iso,
            quit_confirm_pending: false,
            last_mirror_signature: None,
            perf,
            perf_stats,
            scroll_draw_defer_until: None,
            scroll_events_since_draw: 0,
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

    /// Install the main-loop waker on a freshly-spawned pane so its child output
    /// wakes the event loop immediately. No-op until `run` has built the waker.
    /// Takes the waker field by reference (not `&self`) so call sites that already
    /// hold a mutable borrow of `self.agents` can still pass it as a disjoint
    /// field borrow.
    fn attach_output_waker(waker: &Option<PtyOutputWaker>, pane: &TerminalPane) {
        if let Some(waker) = waker {
            pane.set_output_waker(waker.clone());
        }
    }

    fn take_dirty(&mut self) -> bool {
        let was = self.dirty;
        self.dirty = false;
        was
    }

    fn note_scroll_dirty(&mut self) {
        self.scroll_draw_defer_until = Some(Instant::now() + SCROLL_DRAW_DEFER);
        self.scroll_events_since_draw = self.scroll_events_since_draw.saturating_add(1);
    }

    fn should_defer_scroll_draw(&self) -> bool {
        self.scroll_draw_defer_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn scroll_draw_poll_timeout(&self, normal: Duration) -> Duration {
        let Some(until) = self.scroll_draw_defer_until else {
            return normal;
        };
        let now = Instant::now();
        if now >= until {
            return normal;
        }
        normal.min(until.saturating_duration_since(now))
    }

    fn consume_scroll_draw_stats(&mut self) -> usize {
        self.scroll_draw_defer_until = None;
        std::mem::take(&mut self.scroll_events_since_draw)
    }

    fn record_perf_duration(&mut self, metric: &'static str, duration: Duration) {
        self.perf_stats.record_duration(metric, duration);
    }

    fn log_perf_duration_over(
        &mut self,
        event: &str,
        duration: Duration,
        threshold: Duration,
        fields: serde_json::Value,
    ) {
        self.perf
            .log_duration_over(event, duration, threshold, fields);
    }

    fn write_rudder_context_timed(&mut self, pending: Option<&WorktreeInfo>) -> Result<()> {
        let started = Instant::now();
        // Every plan reports its own verdict: each verifies only the nodes it launched.
        let final_gates: Vec<(FinalGateStatus, Option<&str>)> = self
            .plans
            .iter()
            .filter(|plan| plan.final_gate_status != FinalGateStatus::Idle)
            .map(|plan| (plan.final_gate_status, plan.final_gate_summary.as_deref()))
            .collect();
        let result = write_rudder_context_with_history(
            &self.cwd,
            &self.agents,
            pending,
            &self.task_history,
            &final_gates,
        );
        let duration = started.elapsed();
        self.record_perf_duration("write_rudder_context", duration);
        self.log_perf_duration_over(
            "write_rudder_context",
            duration,
            SLOW_POLL_THRESHOLD,
            serde_json::json!({
                "agents": self.agents.len(),
                "pending": pending.is_some(),
                "ok": result.is_ok(),
            }),
        );
        result
    }

    /// Current spinner glyph for the active animation frame.
    pub(crate) fn spinner_glyph(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    fn advance_spinner_if_due(&mut self, now: Instant) -> bool {
        if !self.has_planning_orchestrator()
            || now.duration_since(self.last_spinner_advance) < SPINNER_FRAME_INTERVAL
        {
            return false;
        }
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        self.last_spinner_advance = now;
        self.dirty = true;
        true
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
        let plan_lifecycle_started = self.plan().awaiting_approval
            || !self.plan().planned_nodes.is_empty()
            || self.active_plan_agents().any(|run| run.node_id.is_some());
        self.plan().refining
            || self.plan().rebasing
            || self.active_plan_agents().any(|run| {
                run.mode == AgentMode::RudderPlan
                    && run.status == AgentStatus::Running
                    && (!self.is_interactive_orchestrator_run(run)
                        || self.plan().awaiting_approval
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

    fn selected_headless_orchestrator_rendered_view_active(&self) -> bool {
        self.worker_view == WorkerView::Terminal && self.selected_uses_headless_orchestrator_chat()
    }

    fn selected_interactive_orchestrator_active(&self) -> bool {
        self.worker_view == WorkerView::Terminal
            && self
                .agents
                .get(self.selected_agent)
                .is_some_and(|run| self.is_interactive_orchestrator_run(run))
    }

    pub(crate) fn is_interactive_orchestrator_run(&self, run: &AgentRun) -> bool {
        run.is_orchestrator() && !run.reconcile_planner && run.interactive_orchestrator
    }

    fn has_running_interactive_orchestrator(&self) -> bool {
        self.agents.iter().any(|run| {
            self.is_interactive_orchestrator_run(run) && run.status == AgentStatus::Running
        })
    }

    fn running_interactive_orchestrator_task(&self) -> Option<String> {
        self.agents
            .iter()
            .find(|run| {
                self.is_interactive_orchestrator_run(run) && run.status == AgentStatus::Running
            })
            .map(|run| run.task.clone())
            .filter(|task| !task.trim().is_empty())
    }

    /// True when the worker pane is currently showing the custom orchestrator DAG
    /// command-center view (selected orchestrator, Terminal view, plan parsed)
    /// rather than the raw PTY. In that view scroll/selection target the rendered
    /// DAG lines, not the underlying planner terminal. Mirrors the dispatch in
    /// `render_worker`.
    fn selected_orchestrator_dag_active(&self) -> bool {
        if !self.selected_headless_orchestrator_rendered_view_active() {
            return false;
        }
        if self.plan().planner_paused_for_input {
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
        let model_changed = self.backend != backend || self.model != model;
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
        // Reflect the switch on the LEFT pane for EVERY switch path. The main
        // agent row renders its own run.model; only the `/model` command used to
        // update it, so `/fast` and the `P` picker left the row showing the old
        // model. Update all main rows here (the one chokepoint every path calls).
        let model_now = self.model.clone();
        let cwd_now = self.cwd.clone();
        // Only relabel main rows with NO live PTY: a running process keeps its
        // launch-time model, and rewriting its row advertised a model the
        // process was not using. The next (re)spawn picks up the new default.
        for run in self
            .agents
            .iter_mut()
            .filter(|run| run.is_main() && run.terminal.is_none())
        {
            run.backend = backend;
            run.model = model_now.clone();
            run.effort = effort;
            let _ = save_native_run_record(&cwd_now, run);
        }
        let warning = save_model_defaults(self.backend, &self.model, self.effort)
            .err()
            .map(|error| format!("config warning: {error}"));
        // Switching to a DIFFERENT model retires a still-planning planner so the next
        // task re-plans and launches its agents on the new model (see
        // `retire_planner_for_model_switch`). Logged to the activity feed rather than
        // the notice line, which every caller overwrites with the model summary.
        if model_changed {
            if let Some(note) = self.retire_planner_for_model_switch() {
                self.push_activity(note);
            }
        }
        warning
    }

    /// A model switch (via `/model`, the `P` picker, or `/fast`) retires a planner
    /// that is still DECOMPOSING — or a plan still AWAITING APPROVAL — and clears the
    /// pending plan, so the next task re-plans and launches its agents on the newly
    /// chosen model. An already-EXECUTING plan (worker agents in flight) is left
    /// untouched: its running agents keep the model they launched with, matching how
    /// `/fast` leaves running agents alone. Returns a short note when it retired
    /// something, else `None`.
    fn retire_planner_for_model_switch(&mut self) -> Option<String> {
        // Executing plan = any plan-launched worker not yet merged. Do not disturb it;
        // retiring the conductor mid-flight would strand the running workers. Asked per
        // plan, so a second plan that is already executing does not veto clearing a
        // first plan that is still only a proposal.
        let mut retired = 0usize;
        for index in 0..self.plans.len() {
            let plan_id = self.plans[index].id.clone();
            let executing = self
                .plan_agents(index)
                .any(|run| run.node_id.is_some() && run.status != AgentStatus::Merged);
            if executing {
                continue;
            }
            let planner = self
                .orchestrator_index_for_plan(&plan_id)
                .map(|i| self.agents[i].id.clone());
            if planner.is_none()
                && self.plans[index].planned_nodes.is_empty()
                && !self.plans[index].awaiting_approval
            {
                continue;
            }
            if let Some(id) = planner {
                self.retire_planner_row(&id);
            }
            // Drop the pending (unapproved) plan so the next typed task starts a FRESH
            // planner (plan_is_active() is now false) instead of refining the old model's
            // proposal. Persist so a restart does not reload the discarded plan.
            self.plans[index] = PlanState {
                id: plan_id,
                ..PlanState::default()
            };
            retired += 1;
        }
        if retired == 0 {
            return None;
        }
        self.persist_plan_queue();
        Some(format!(
            "model → {} {}: cleared the planner; next task plans on the new model",
            self.backend.as_str(),
            self.model
        ))
    }

    /// The single quit gate: quitting with agents still running needs a second
    /// press of the same quit intent (Ctrl+C or q). Every quit key — Ctrl+C and
    /// the pane-local `q`s — routes through here so none can skip the guard.
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
            "{running} agent{} still running. press q / Ctrl+C again to quit and pause them; any other key cancels.",
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
        // Any other key dismisses the pending quit confirmation. A repeated q
        // confirms; Ctrl+C re-enters confirm_or_quit above and confirms there.
        if self.quit_confirm_pending {
            if matches!(key.code, KeyCode::Char('q')) {
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
                "nav mode: 1 agents  2 worker  3 task  v review  b branch chat  R review-all  M merge-all  Esc exits"
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
                "Ctrl+W: Tab cycle  1/2/3 panes  v review  m merge  b branch chat  R review-all  M merge-all  Esc cancels"
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

        // Option/Cmd + [ and ] step through agents WITHOUT leaving the pane you are
        // in. The worker pane forwards every keystroke to the agent's own TUI, so
        // moving between agents from there needs a chord the dashboard claims first;
        // j/k only work in the agents list. Cmd is accepted for the terminals that
        // report SUPER (the kitty protocol Rudder enables), Option for the rest.
        let stepper_like = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Char('[') if stepper_like => {
                self.select_previous_agent();
                return false;
            }
            KeyCode::Char(']') if stepper_like => {
                self.select_next_agent();
                return false;
            }
            // Vim-grammar aliases for the stepper: with Alt+j/k scrolling the
            // pane vertically, Alt+h/l are the matching horizontal moves —
            // left/right across agents. A PTY wraps at the pane width, so
            // there is no horizontal scrollback for these to mean instead.
            // Not while composing a task, same rule as every Option+letter.
            KeyCode::Char('h') if stepper_like && self.focus != FocusPane::Task => {
                self.select_previous_agent();
                return false;
            }
            KeyCode::Char('l') if stepper_like && self.focus != FocusPane::Task => {
                self.select_next_agent();
                return false;
            }
            // Terminals that swallow the modifier send what Option+[ / Option+]
            // TYPE on a US layout instead. Not honored while composing a task,
            // where a curly quote is far more likely to be text than a shortcut.
            KeyCode::Char('\u{201c}') if self.focus != FocusPane::Task => {
                self.select_previous_agent();
                return false;
            }
            KeyCode::Char('\u{2018}') if self.focus != FocusPane::Task => {
                self.select_next_agent();
                return false;
            }
            _ => {}
        }

        // Alt+j/k/u/d scroll the selected worker's scrollback from anywhere
        // except the task composer (Option+letter types a composed character
        // there, which is far more likely to be text than a scroll). Focused
        // pane included: bare letters forward to the agent, the Alt chord is
        // the dashboard's — same claim rule as the Alt+[/] stepper above.
        if self.focus != FocusPane::Task {
            if let Some(rows) = alt_scroll_rows(key, self.worker_area) {
                self.scroll_worker_scrollback(rows);
                return false;
            }
            // Terminals without option-as-meta type the composed character
            // instead: Option+j → ∆, Option+k → ˚, Option+d → ∂, Option+h → ˙,
            // Option+l → ¬ (Option+u is a dead key, no composed fallback
            // exists). Same fallback rule the Alt+[/] stepper uses above.
            match key.code {
                KeyCode::Char('\u{02d9}') => {
                    self.select_previous_agent();
                    return false;
                }
                KeyCode::Char('\u{00ac}') => {
                    self.select_next_agent();
                    return false;
                }
                KeyCode::Char('\u{2206}') => {
                    self.scroll_worker_scrollback(-1);
                    return false;
                }
                KeyCode::Char('\u{02da}') => {
                    self.scroll_worker_scrollback(1);
                    return false;
                }
                KeyCode::Char('\u{2202}') => {
                    self.scroll_worker_scrollback(-(page_scroll_rows(self.worker_area) / 2).max(1));
                    return false;
                }
                _ => {}
            }
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
            // Branching is only bound to a bare `b` in the agents list, which the
            // worker pane cannot use — every keystroke there goes to the agent's own
            // TUI, so `b` was simply typed into it. Reachable here and under ^W.
            KeyCode::Char('b') => self.branch_selected_agent(),
            KeyCode::Char('u') => self.undo_selected_merge(),
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
            KeyCode::Char('b') => self.branch_selected_agent(),
            KeyCode::Char('u') => self.undo_selected_merge(),
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
                if self.plan().awaiting_approval && self.selected_is_orchestrator() {
                    self.delete_pending = None;
                    self.approve_planned_queue();
                } else if !self.agents.is_empty() {
                    self.delete_pending = None;
                    if self.selected_is_main() {
                        self.focus_or_spawn_main();
                    } else if self.agents[self.selected_agent].status == AgentStatus::Paused {
                        let index = self.selected_agent;
                        if self.agents[index].is_orchestrator() {
                            self.restart_selected_agent();
                        } else {
                            let continuation =
                                if self.agents[index].current_prompt.trim().is_empty() {
                                    self.agents[index].task.clone()
                                } else {
                                    self.agents[index].current_prompt.clone()
                                };
                            self.regoal_agent_at(index, &continuation);
                        }
                        self.focus = FocusPane::Worker;
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
                // parallelism slot, and keeps its jj workspace (undoable). Main rows
                // are stoppable too — they are just a conversation in the checkout,
                // and excluding them created a dead end where the dd flow's own
                // advice ("stop it with x") did nothing. Only the orchestrator is
                // off-limits: its lifecycle belongs to the plan.
                if !self.selected_is_orchestrator() {
                    let idx = self.selected_agent;
                    self.stop_agent_at(idx);
                }
            }
            KeyCode::Char('c') => self.clear_merged_agents(),
            KeyCode::Char('b') => self.branch_selected_agent(),
            KeyCode::Char('u') => self.undo_selected_merge(),
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
        if self.worker_view == WorkerView::PlanReview {
            return self.handle_plan_review_key(key);
        }

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
        // useless. For the INTERACTIVE orchestrator the pane IS a live backend PTY,
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
            None => {
                // A FINISHED worker whose PTY is gone (e.g. after a restart) stays
                // conversable: keys type into the run's draft, and Enter resumes the
                // same session with the new instruction via the regoal path.
                if self.selected_done_worker_card_active() {
                    if let Some(prompt) = self.capture_selected_worker_key(key, true) {
                        let index = self.selected_agent;
                        self.regoal_agent_at(index, &prompt);
                    }
                    self.dirty = true;
                }
                return false;
            }
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
                    if self.plan().rebasing {
                        self.notice = Some(
                            "still rebasing — applying the new direction to the live plan"
                                .to_string(),
                        );
                    } else if self.plan().refining {
                        self.notice =
                            Some("still refining — the updated plan is on its way".to_string());
                    } else if self.plan().awaiting_approval {
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
                    // Talking to an orchestrator pane edits THAT orchestrator's plan,
                    // and is now the only way to do it: the task pane always starts a
                    // standalone worker instead. Before approval the message refines
                    // the draft plan; once it is running the message conducts, so a
                    // structural pivot rebases the DAG and anything else is injected
                    // as work on top of it.
                    if self.plan_is_active() {
                        if self.classify_new_direction(&draft) {
                            self.start_plan_rebase(&draft);
                        } else {
                            self.reconcile_injection(&draft);
                        }
                    } else {
                        self.refine_plan(&draft);
                    }
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
                self.orch_follow_bottom = false;
                self.orch_dag_scroll = self.orch_dag_scroll.saturating_sub(page);
            }
            KeyCode::PageDown => {
                let page = page_scroll_rows(self.worker_area).max(1) as usize;
                self.orch_dag_scroll = self.orch_dag_scroll.saturating_add(page);
                if self.orch_dag_max_scroll == 0 {
                    self.orch_follow_bottom = true;
                } else if self.orch_dag_scroll >= self.orch_dag_max_scroll {
                    self.orch_dag_scroll = self.orch_dag_max_scroll;
                    self.orch_follow_bottom = true;
                }
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
                run.needs_user_input = false;
                self.dirty = true;
            }
        }
    }

    fn copy_focused_selection(&mut self) {
        let text = match self.focus {
            FocusPane::Worker if self.worker_view == WorkerView::Terminal => {
                if self.selected_is_orchestrator() {
                    if let Some(selection) = self.orch_selection {
                        selected_text_from_lines(&self.orch_visible_rows, selection)
                    } else if let Some(selection) = self.worker_selection {
                        self.selected_worker_selection_text(selection)
                    } else {
                        return;
                    }
                } else {
                    let Some(selection) = self.worker_selection else {
                        return;
                    };
                    self.selected_worker_selection_text(selection)
                }
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
        self.orch_dag_max_scroll = 0;
        self.orch_follow_bottom = true;
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
        self.orch_dag_max_scroll = 0;
        self.orch_follow_bottom = true;
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
        if self.plan().awaiting_approval && self.selected_is_orchestrator() {
            self.worker_view = match self.worker_view {
                WorkerView::PlanReview => WorkerView::Terminal,
                _ => {
                    self.ensure_plan_review_state();
                    WorkerView::PlanReview
                }
            };
            self.focus = FocusPane::Worker;
            return;
        }
        self.worker_view = match self.worker_view {
            WorkerView::Terminal => {
                self.ensure_review_diff();
                self.mark_selected_reviewed();
                WorkerView::Diff
            }
            WorkerView::Diff => {
                self.notice = None;
                WorkerView::Terminal
            }
            WorkerView::PlanReview => WorkerView::Terminal,
        };
        self.focus = FocusPane::Worker;
    }

    /// Record that this row's diff has been put in front of the user. This is the
    /// review gate, and it is deliberately weak: it claims only that the change was
    /// SHOWN, which is a thing the dashboard can actually observe. A stronger claim
    /// ("approved") would need a second keystroke that people learn to hammer, which
    /// buys a stricter-sounding gate and no more real review.
    ///
    /// Only rows with something to merge are marked. A main-checkout or one-off agent
    /// has no workspace and never reaches the gate, so marking it would put a
    /// "reviewed" label on a row that can never be merged.
    fn mark_selected_reviewed(&mut self) {
        let cwd = self.cwd.clone();
        let Some(run) = self.agents.get_mut(self.selected_agent) else {
            return;
        };
        if run.is_main() || run.is_oneoff() || run.is_orchestrator() {
            return;
        }
        if run.reviewed_at.is_some() {
            return;
        }
        run.reviewed_at = Some(now_stamp());
        let _ = save_native_run_record(&cwd, run);
        self.dirty = true;
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
                let revision = run.jj_change_id.clone()?;
                Some(ReviewAllSource {
                    id: run.id.clone(),
                    revision,
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
            branch: None,
            path_is_worktree: true,
            workspace_name: Some("rudder-test-review-all".to_string()),
            jj_change_id: Some("review-change".to_string()),
        };
        let premerge = ReviewAllPremerge {
            merged_revisions: sources
                .iter()
                .map(|source| source.revision.clone())
                .collect(),
            ..ReviewAllPremerge::default()
        };
        let prompt = review_all_prompt(
            current_branch_at(&self.cwd).as_deref().unwrap_or("HEAD"),
            &worktree,
            &sources,
            &premerge,
        );
        let (review_backend, review_model, review_effort) = self.review_agent_profile();
        let mut run = review_all_run(worktree, prompt, sources, None);
        run.backend = review_backend;
        run.model = review_model.to_string();
        run.effort = Some(review_effort);
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.worker_selection = None;
        self.worker_view = WorkerView::Terminal;
        self.focus = FocusPane::Worker;
        self.notice = Some("started review-all merge agent".to_string());
    }

    /// Deterministic model for the code-review agent, chosen by the plan's
    /// backend: Claude plans review on Opus (medium effort), Codex plans review
    /// on gpt-5.5 (high effort) by default. Override per backend with
    /// `RUDDER_REVIEW_MODEL_CLAUDE` / `RUDDER_REVIEW_MODEL_CODEX` — e.g. set the
    /// Codex one to `gpt-5.6` on workers whose Codex is authed with an API key
    /// that can reach it (gpt-5.6 400s on ChatGPT-account Codex auth). The review
    /// itself runs the installed `thermonuclear` skill (see `review_all_prompt`).
    fn review_agent_profile(&self) -> (Backend, String, EffortLevel) {
        let env_override = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        match self.backend {
            Backend::Claude => (
                Backend::Claude,
                env_override("RUDDER_REVIEW_MODEL_CLAUDE").unwrap_or_else(|| "opus".to_string()),
                EffortLevel::Medium,
            ),
            Backend::Codex => (
                Backend::Codex,
                env_override("RUDDER_REVIEW_MODEL_CODEX")
                    .unwrap_or_else(|| REVIEW_ALL_MODEL.to_string()),
                REVIEW_ALL_EFFORT,
            ),
            // Reviews run on whatever model opencode is configured with; there is no
            // Rudder-curated review profile to pin (and no effort dial to set).
            Backend::Opencode => (
                Backend::Opencode,
                env_override("RUDDER_REVIEW_MODEL_OPENCODE").unwrap_or_default(),
                EffortLevel::Medium,
            ),
        }
    }

    #[cfg(not(test))]
    fn start_review_all_agent(&mut self, sources: Vec<ReviewAllSource>) -> Result<()> {
        let target_ref = current_branch_at(&self.cwd)
            .or_else(|| {
                git_output(&self.cwd, ["rev-parse", "HEAD"])
                    .ok()
                    .map(|value| value.trim().to_string())
            })
            .unwrap_or_else(|| "HEAD".to_string());
        let mut worktree =
            prepare_jj_workspace_at(&self.cwd, "review all completed worktrees", None)?;
        let premerge = premerge_review_all_sources(&worktree.path, &sources);
        worktree.jj_change_id = jj_workspace_change_id(&worktree.path);
        let prompt = review_all_prompt(&target_ref, &worktree, &sources, &premerge);
        let (review_backend, review_model, review_effort) = self.review_agent_profile();
        let session_id = mint_session_id_for(review_backend);
        let mut command = agent_command(
            review_backend,
            &review_model,
            Some(review_effort),
            &prompt,
            AgentMode::ReviewAll,
            session_id.as_deref(),
        );
        signals::augment_worker_command(
            &mut command,
            review_backend,
            AgentMode::ReviewAll,
            &worktree.id,
        );
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(worktree.path.clone()),
            ..TerminalPaneOptions::default()
        };
        let mut run = review_all_run(worktree, prompt, sources, session_id);
        // Keep the row's shown model honest with what actually spawned.
        run.backend = review_backend;
        run.model = review_model;
        run.effort = Some(review_effort);
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
        let _ = self.write_rudder_context_timed(None);
        if started {
            let count = self
                .agents
                .get(self.selected_agent)
                .map(|run| run.review_source_ids.len())
                .unwrap_or(0);
            let (_, review_model, _) = self.review_agent_profile();
            self.notice = Some(format!(
                "started {review_model} thermonuclear review-all for {count} worktree{}; press m on that row when done",
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
            self.maybe_refresh_handoff_candidates();
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                // First Esc with the palette open just dismisses the palette;
                // clearing the whole draft here lost long typed tasks whose
                // author only wanted the popup gone. A second Esc (or Esc with
                // no palette) clears the draft as before.
                if !suggestions_for(self).is_empty() {
                    self.picker_dismissed_input = Some(self.task_input.clone());
                    self.picker_index = 0;
                } else {
                    self.reset_task_history_navigation();
                    self.task_input.clear();
                    self.task_cursor = 0;
                    self.pasted_chunks.clear();
                    self.picker_index = 0;
                    self.picker_dismissed_input = None;
                }
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
                self.pasted_chunks.clear();
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
                    "just type for one isolated mergeable worker, /plan for a DAG, /main|/m for this checkout, /resume to continue a chat you already started, /model, /usage, or /cloud"
                        .to_string(),
                );
            }
            KeyCode::Char(ch) => {
                self.reset_task_history_navigation();
                insert_char_at_cursor(&mut self.task_input, &mut self.task_cursor, ch);
                self.clamp_picker_index();
            }
            _ => {}
        }
        // The /handoff palette lists conversations read off disk. Refresh it here,
        // on the (user-paced) keystroke, never in `suggestions_for` — that runs on
        // every frame.
        self.maybe_refresh_handoff_candidates();
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
        // Keep the remembered text for any paste chip the replacement still
        // shows: history nav restores drafts WITH their chips, and dropping the
        // chunks here sent the literal "[Pasted #1 +60 lines]" placeholder to
        // the agent instead of the pasted content. Chips absent from the new
        // text are genuinely gone.
        let input = &self.task_input;
        self.pasted_chunks
            .retain(|chunk| input.contains(&chunk.placeholder));
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
                // Capture Enter only while the user is still TYPING a command
                // name (no argument text yet) or inside the /model drill-down.
                // "/plan x" fuzzy-matched the "/plan" suggestion, and accepting
                // it REPLACED the input — silently deleting the argument and
                // requiring a second Enter. Once real arguments exist, Enter
                // submits the command.
                let trimmed = self.task_input.trim();
                let typing_command_name = !trimmed.contains(char::is_whitespace);
                // /model and /resume are drill-down pickers: their argument is
                // chosen from the list, not typed, so Enter keeps selecting. Both
                // hide the palette once a concrete argument is present, which is
                // what lets a second Enter submit.
                if !(typing_command_name
                    || trimmed.starts_with("/model")
                    || resume_command_rest(trimmed).is_some())
                {
                    return false;
                }
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
                let is_model_picker = value.starts_with("/model");
                self.replace_task_input(value);
                if is_model_picker {
                    self.select_current_model_picker_choice();
                }
            }
            SuggestionAction::RunCommand(value) => {
                self.task_input.clear();
                self.task_cursor = 0;
                self.picker_index = 0;
                self.start_task_from_input(&value);
            }
            SuggestionAction::ChooseModelProvider(backend) => {
                self.replace_task_input(format!("/model {} ", backend.as_str()));
                self.select_current_model_picker_choice();
                self.notice = Some(format!("pick a {} model", backend.as_str()));
            }
            SuggestionAction::ChooseModel { backend, model } => {
                self.replace_task_input(format!("/model {} {} ", backend.as_str(), model));
                self.select_current_model_picker_choice();
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
                    // Only a row with no live process takes the new selection.
                    // `set_model_defaults` already refuses to relabel a running
                    // row; overriding that here advertised a model the process was
                    // not using, and rewriting `backend` mid-flight made the poll
                    // loop hunt for the wrong hook file and kill the agent.
                    if run.terminal.is_none() {
                        run.backend = backend;
                        run.model = model;
                        run.effort = effort;
                        let _ = save_native_run_record(&cwd, run);
                        if self.selected_agent == main_index {
                            should_spawn_main = true;
                        }
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

    fn select_current_model_picker_choice(&mut self) {
        let suggestions = suggestions_for(self);
        self.picker_index = suggestions
            .iter()
            .position(|suggestion| {
                is_current_model_suggestion(suggestion, self.backend, &self.model, self.effort)
            })
            .unwrap_or(0);
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
                } else if self.worker_view == WorkerView::PlanReview {
                    self.with_plan_review_text_mut(|value, cursor| {
                        insert_str_at_cursor(value, cursor, &text);
                    });
                } else if self.selected_uses_headless_orchestrator_chat() {
                    if let Some(run) = self.agents.get_mut(self.selected_agent) {
                        insert_str_at_cursor(
                            &mut run.worker_input_draft,
                            &mut run.worker_input_cursor,
                            &text,
                        );
                        self.dirty = true;
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
                        None => {
                            // Finished worker without a PTY: paste lands in the
                            // resume draft (Enter sends it via the regoal path).
                            if self.selected_done_worker_card_active() {
                                let prompts = self.capture_selected_worker_paste(&text, true);
                                let joined = prompts.join("\n");
                                if !joined.trim().is_empty() {
                                    let index = self.selected_agent;
                                    self.regoal_agent_at(index, joined.trim());
                                }
                                self.dirty = true;
                            }
                            return;
                        }
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
                // Collapse a large paste into a "[Pasted #N …]" chip; re-pasting the
                // same content toggles it open/closed (expand_pasted_chips restores the
                // real text on submit).
                apply_task_paste(
                    &mut self.task_input,
                    &mut self.task_cursor,
                    &mut self.pasted_chunks,
                    &text,
                );
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
            AgentStatus::Done | AgentStatus::Merged | AgentStatus::Stopped | AgentStatus::Orphaned
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
        // A finished worker stays CONVERSABLE: a new instruction typed into its live
        // PTY is a new working turn, so flip it back to Running and clear its ingest
        // ledger — the NEXT completion then re-ingests (follow-ups + card summary
        // refresh) instead of being skipped as already handled.
        self.reopen_selected_finished_worker();
        let _ = self.write_rudder_context_timed(None);
    }

    /// If the selected agent is a FINISHED worker with a live terminal, re-open it:
    /// status back to Running, ledger cleared so its next completion is ingested
    /// afresh, and the stale done-sidecar removed (mirrors `regoal_agent_at`'s
    /// prologue, but for input typed directly into the still-alive session).
    fn reopen_selected_finished_worker(&mut self) {
        let Some(run) = self.agents.get(self.selected_agent) else {
            return;
        };
        if run.is_main()
            || run.is_orchestrator()
            || run.merge_resolver
            || run.terminal.is_none()
            || !matches!(
                run.status,
                AgentStatus::Done
                    | AgentStatus::Merged
                    | AgentStatus::Stopped
                    | AgentStatus::Failed
            )
        {
            return;
        }
        let id = run.id.clone();
        let node_id = run.node_id.clone();
        let run_cwd = run.cwd.clone();
        let was_ingested = self.followups_ingested.remove(&id);
        let was_pending = self.completion_summary_pending.remove(&id);
        if was_ingested || was_pending {
            self.persist_ingested_runs();
        }
        if let Some(node_id) = node_id.as_deref() {
            let _ = std::fs::remove_file(worker_done_file(&run_cwd, node_id));
        }
        let cwd = self.cwd.clone();
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            run.status = AgentStatus::Running;
            run.completed_at = None;
            let _ = save_native_run_record(&cwd, run);
        }
    }

    /// The finished-worker CARD view is active: the selected agent is a completed /
    /// merged / stopped / failed worker (not main, not the orchestrator), so the
    /// worker pane renders objective + what-it-did above the conversation, and the
    /// session stays conversable.
    pub(crate) fn selected_done_worker_card_active(&self) -> bool {
        if self.worker_view != WorkerView::Terminal {
            return false;
        }
        self.agents.get(self.selected_agent).is_some_and(|run| {
            !run.is_main()
                && !run.is_orchestrator()
                && !run.merge_resolver
                && matches!(run.mode, AgentMode::Execute | AgentMode::OneOff)
                && matches!(
                    run.status,
                    AgentStatus::Done
                        | AgentStatus::Merged
                        | AgentStatus::Stopped
                        | AgentStatus::Orphaned
                        | AgentStatus::Failed
                )
        })
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if is_scroll_mouse_event(mouse.kind) {
            return self.handle_pane_scroll(mouse);
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
                let orch_area = if self.selected_interactive_orchestrator_active() {
                    let (dag_area, _) = interactive_orchestrator_areas(worker_area);
                    block_inner(dag_area)
                } else {
                    block_inner(worker_area)
                };
                if self.handle_orchestrator_selection_mouse(mouse, orch_area) {
                    return true;
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
                let worker_inner = if self.selected_interactive_orchestrator_active() {
                    let (_, term_area) = interactive_orchestrator_areas(worker_area);
                    block_inner(term_area)
                } else if self.selected_done_worker_card_active() {
                    let (_, term_area) = done_card_areas(worker_area);
                    block_inner(term_area)
                } else {
                    block_inner(worker_area)
                };
                if self.handle_worker_selection_mouse(mouse, worker_inner) {
                    return true;
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
            return true;
        }

        if let Some(task_area) = self
            .task_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        {
            self.worker_selection = None;
            if self.handle_task_selection_mouse(mouse, block_inner(task_area)) {
                return true;
            }
            return false;
        }

        let Some(worker_area) = self
            .worker_area
            .filter(|area| rect_contains(*area, mouse.column, mouse.row))
        else {
            return false;
        };

        self.task_selection = None;
        let inner = block_inner(worker_area);

        if self.worker_view == WorkerView::PlanReview {
            self.worker_selection = None;
            self.orch_selection = None;
            return true;
        }

        if self.worker_view == WorkerView::Diff {
            if self.write_mouse_to_selected_review(mouse, inner) {
                return true;
            }
            return false;
        }

        // Headless orchestrator renders composed Lines, not a PTY: select over the
        // captured rows. Interactive orchestrator is split: DAG selection above,
        // normal terminal selection/input below.
        if self.selected_uses_headless_orchestrator_chat() {
            self.worker_selection = None;
            self.handle_orchestrator_selection_mouse(mouse, inner);
            return true;
        }

        if self.selected_interactive_orchestrator_active() {
            let (dag_area, term_area) = interactive_orchestrator_areas(worker_area);
            if rect_contains(dag_area, mouse.column, mouse.row) {
                self.worker_selection = None;
                self.handle_orchestrator_selection_mouse(mouse, block_inner(dag_area));
                return true;
            }
            if rect_contains(term_area, mouse.column, mouse.row) {
                self.orch_selection = None;
                let term_inner = block_inner(term_area);
                if self.handle_worker_selection_mouse(mouse, term_inner) {
                    return true;
                }
                if self.write_mouse_to_selected_worker(mouse, term_inner) {
                    return true;
                }
            }
            return false;
        }

        // Finished-worker card view: the card is static text (no selection mapping
        // to PTY rows), so mouse work targets only the conversation sub-pane.
        if self.selected_done_worker_card_active() {
            let (_, term_area) = done_card_areas(worker_area);
            if rect_contains(term_area, mouse.column, mouse.row) {
                let term_inner = block_inner(term_area);
                if self.handle_worker_selection_mouse(mouse, term_inner) {
                    return true;
                }
                if self.write_mouse_to_selected_worker(mouse, term_inner) {
                    return true;
                }
            }
            return false;
        }

        if self.handle_worker_selection_mouse(mouse, inner) {
            return true;
        }
        self.write_mouse_to_selected_worker(mouse, inner)
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
                self.orch_dag_max_scroll = 0;
                self.orch_follow_bottom = true;
                self.orch_selection = None;
            }
            self.selected_agent = index;
        }
    }

    fn handle_pane_scroll(&mut self, mouse: MouseEvent) -> bool {
        let started = Instant::now();
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
                let changed = if self.worker_view == WorkerView::PlanReview {
                    self.scroll_plan_review(mouse, inner)
                } else if self.selected_interactive_orchestrator_active() {
                    self.scroll_interactive_orchestrator(mouse, area)
                } else if self.selected_done_worker_card_active() {
                    let (_, term_area) = done_card_areas(area);
                    self.scroll_selected_worker_or_forward(mouse, block_inner(term_area))
                } else if self.selected_headless_orchestrator_rendered_view_active() {
                    self.scroll_orchestrator_dag(mouse, inner)
                } else if self.worker_view == WorkerView::Diff {
                    self.scroll_selected_review_or_forward(mouse, inner)
                } else {
                    self.scroll_selected_worker_or_forward(mouse, inner)
                };
                let duration = started.elapsed();
                self.record_perf_duration("scroll_event", duration);
                self.log_perf_duration_over(
                    "scroll_event",
                    duration,
                    SLOW_SCROLL_THRESHOLD,
                    serde_json::json!({
                        "pane": "worker-focus",
                        "view": format!("{:?}", self.worker_view),
                        "kind": format!("{:?}", mouse.kind),
                        "changed": changed,
                    }),
                );
                if changed {
                    self.note_scroll_dirty();
                }
                return changed;
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
            let changed = if self.worker_view == WorkerView::PlanReview {
                self.scroll_plan_review(mouse, inner)
            } else if self.selected_interactive_orchestrator_active() {
                self.scroll_interactive_orchestrator(mouse, area)
            } else if self.selected_done_worker_card_active() {
                let (_, term_area) = done_card_areas(area);
                self.scroll_selected_worker_or_forward(mouse, block_inner(term_area))
            } else if self.selected_headless_orchestrator_rendered_view_active() {
                self.scroll_orchestrator_dag(mouse, inner)
            } else if self.worker_view == WorkerView::Diff {
                self.scroll_selected_review_or_forward(mouse, inner)
            } else {
                self.scroll_selected_worker_or_forward(mouse, inner)
            };
            let duration = started.elapsed();
            self.record_perf_duration("scroll_event", duration);
            self.log_perf_duration_over(
                "scroll_event",
                duration,
                SLOW_SCROLL_THRESHOLD,
                serde_json::json!({
                    "pane": "worker",
                    "view": format!("{:?}", self.worker_view),
                    "kind": format!("{:?}", mouse.kind),
                    "changed": changed,
                }),
            );
            if changed {
                self.note_scroll_dirty();
            }
            return changed;
        }

        if self
            .agents_area
            .is_some_and(|area| rect_contains(area, mouse.column, mouse.row))
        {
            self.set_mouse_debug(format!(
                "mouse {:?} @{},{} pane=agents",
                mouse.kind, mouse.column, mouse.row
            ));
            let before = self.selected_agent;
            if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                self.select_previous_agent();
            } else if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                self.select_next_agent();
            }
            let changed = self.selected_agent != before;
            let duration = started.elapsed();
            self.record_perf_duration("scroll_event", duration);
            self.log_perf_duration_over(
                "scroll_event",
                duration,
                SLOW_SCROLL_THRESHOLD,
                serde_json::json!({
                    "pane": "agents",
                    "kind": format!("{:?}", mouse.kind),
                    "changed": changed,
                }),
            );
            if changed {
                self.note_scroll_dirty();
            }
            return changed;
        }

        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=none route=ignored",
            mouse.kind, mouse.column, mouse.row
        ));
        let duration = started.elapsed();
        self.record_perf_duration("scroll_event", duration);
        self.log_perf_duration_over(
            "scroll_event",
            duration,
            SLOW_SCROLL_THRESHOLD,
            serde_json::json!({
                "pane": "none",
                "kind": format!("{:?}", mouse.kind),
                "changed": false,
            }),
        );
        false
    }

    fn scroll_interactive_orchestrator(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let (dag_area, term_area) = interactive_orchestrator_areas(area);
        if rect_contains(dag_area, mouse.column, mouse.row) {
            return self.scroll_orchestrator_dag(mouse, block_inner(dag_area));
        }
        if rect_contains(term_area, mouse.column, mouse.row) {
            return self.scroll_selected_worker_or_forward(mouse, block_inner(term_area));
        }
        false
    }

    fn scroll_plan_review(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let rows = mouse_scrollback_delta(mouse, area.height);
        let before = self.plan().plan_review.scroll;
        if rows > 0 {
            self.plan_mut().plan_review.scroll =
                self.plan().plan_review.scroll.saturating_sub(rows as usize);
        } else if rows < 0 {
            self.plan_mut().plan_review.scroll = self
                .plan()
                .plan_review
                .scroll
                .saturating_add(rows.unsigned_abs() as usize);
        }
        self.plan().plan_review.scroll != before
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
        let alternate = terminal.uses_alternate_screen_snapshot();
        let mut forwarded = false;
        let mut write_error = None;
        terminal.scrollback_by(rows);
        let after = terminal.scrollback();
        let moved = after != before;
        let wants_mouse = if moved || rows == 0 {
            false
        } else {
            terminal.wants_sgr_mouse_events_snapshot()
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
        moved || forwarded
    }

    /// Scroll the orchestrator DAG command-center view. The DAG is a static list of
    /// rendered lines (not a PTY), so we move an app-level line offset. ScrollUp
    /// reveals earlier lines (smaller offset); ScrollDown reveals later lines. The
    /// upper bound is clamped against the rendered content in `render_orchestrator`.
    fn scroll_orchestrator_dag(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        let delta = mouse_scrollback_delta(mouse, area.height);
        if delta == 0 {
            return false;
        }
        // Scrolling shifts which rows are on screen; a stale selection would then
        // highlight the wrong text, so drop it.
        let had_selection = self.orch_selection.take().is_some();
        let before = self.orch_dag_scroll;
        let before_follow = self.orch_follow_bottom;
        // Positive delta is ScrollUp (toward the top), which lowers the offset.
        if delta > 0 {
            self.orch_follow_bottom = false;
            self.orch_dag_scroll = self.orch_dag_scroll.saturating_sub(delta as usize);
        } else {
            self.orch_dag_scroll = self.orch_dag_scroll.saturating_add(delta.unsigned_abs());
            if self.orch_dag_max_scroll == 0 {
                self.orch_follow_bottom = true;
            } else if self.orch_dag_scroll >= self.orch_dag_max_scroll {
                self.orch_dag_scroll = self.orch_dag_max_scroll;
                self.orch_follow_bottom = true;
            }
        }
        self.set_mouse_debug(format!(
            "mouse {:?} @{},{} pane=orchestrator-dag delta={} before={} after={} max={} follow={}",
            mouse.kind,
            mouse.column,
            mouse.row,
            delta,
            before,
            self.orch_dag_scroll,
            self.orch_dag_max_scroll,
            self.orch_follow_bottom
        ));
        self.orch_dag_scroll != before || self.orch_follow_bottom != before_follow || had_selection
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
        let alternate = review.uses_alternate_screen_snapshot();
        review.scrollback_by(rows);
        let after = review.scrollback();
        let moved = after != before;
        let wants_mouse = if moved || rows == 0 {
            false
        } else {
            review.wants_sgr_mouse_events_snapshot()
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
        moved || forwarded
    }

    /// Alt-modified scrollback for the worker pane. Unlike PageUp/Down this
    /// NEVER forwards bytes to the agent: Alt+j would arrive as ESC+j, which
    /// vim-like TUIs interpret as a cursor move — scrolling must not type.
    /// On an alternate-screen app the pane has no scrollback to move, so the
    /// keys are simply absorbed.
    fn scroll_worker_scrollback(&mut self, rows: isize) {
        if let Some(terminal) = self.selected_terminal_mut() {
            if !terminal.uses_alternate_screen_snapshot() {
                terminal.scrollback_by(rows);
            }
        }
    }

    fn handle_worker_page_key(&mut self, key: KeyEvent, rows: isize) {
        let Some(bytes) = terminal_bytes_for_key(key) else {
            return;
        };
        let result = match self.selected_terminal_mut() {
            Some(terminal) => {
                if terminal.uses_alternate_screen_snapshot() {
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
        // Expand any collapsed paste chips back to their real content before the draft
        // is interpreted, so the agent receives the full text whether the user left the
        // chips collapsed or toggled them open.
        let input = expand_pasted_chips(&self.task_input, &self.pasted_chunks)
            .trim()
            .to_string();
        if input.is_empty() {
            // Empty Enter while a plan awaits approval = approve & launch. This makes
            // the task pane the single plan-mode surface: type to refine, Enter to go.
            if self.plan().awaiting_approval {
                self.approve_planned_queue();
            }
            return;
        }
        self.remember_task_history(&input);
        self.task_input.clear();
        self.task_cursor = 0;
        self.pasted_chunks.clear();
        self.worker_selection = None;
        self.start_task_from_input(&input);
    }

    fn start_task_from_input(&mut self, input: &str) {
        self.capture_shared_context_from_input(input);
        if self.handle_command(&input) {
            return;
        }
        // A slash-command-shaped token that no command matched is almost always
        // a typo (/plna, /deploy). Falling through spawned a REAL agent whose
        // prompt was the literal "/plna fix the bug" — an expensive way to
        // learn about a typo. Paths ("/tmp/x") contain another slash and still
        // pass through as plain text.
        let trimmed = input.trim_start();
        if let Some(token) = trimmed
            .strip_prefix('/')
            .and_then(|rest| rest.split_whitespace().next())
        {
            if !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                self.notice = Some(format!(
                    "unknown command /{token} — /help lists commands · remove the leading / to send this as a task"
                ));
                self.dirty = true;
                return;
            }
        }
        self.notice = None;

        // REFINE / CONVERSE: while a plan is parsed but not yet approved, a typed
        // message is feedback to the orchestrator, not a new node. Interactive
        // orchestrators are live Claude sessions, so send the text into that PTY and
        // let it update RUDDER.md; headless planners still use the old refine path.
        if self.plan().awaiting_approval {
            if self.send_to_interactive_orchestrator(input) {
                return;
            }
            self.refine_plan(&input);
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

        // DEFAULT: one isolated worker in its own jj workspace, always.
        //
        // Plain input used to mean "one end-to-end agent in the real checkout", and
        // while a plan was running it was instead routed into the orchestrator as
        // conducting. Both are gone. A typed task now means the same thing no matter
        // what else is on screen: a standalone mergeable worker that touches nothing
        // but its own workspace. Editing a plan is done by talking to that
        // orchestrator's own pane, which is unambiguous once several plans can run at
        // once — a single global input could not say WHICH plan it meant.
        // `/plan` is the only route to an orchestrator; `/main` is the only route
        // into the shared checkout.
        self.start_single_run_task(input);
    }

    /// Queue a deferred Enter for `run_id`, flushed by `flush_pending_enters`
    /// once ENTER_SUBMIT_DELAY has passed. Injected text and its submitting CR
    /// must be separate writes with a gap, or Claude's paste detection absorbs
    /// the CR into the paste and the prompt sits unsubmitted.
    fn queue_enter_for(&mut self, run_id: &str) {
        self.pending_enters.push(PendingEnter {
            run_id: run_id.to_string(),
            due: Instant::now() + ENTER_SUBMIT_DELAY,
        });
    }

    fn flush_pending_enters(&mut self) {
        if self.pending_enters.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut remaining: Vec<PendingEnter> = Vec::new();
        for pending in std::mem::take(&mut self.pending_enters) {
            if pending.due > now {
                remaining.push(pending);
                continue;
            }
            if let Some(run) = self.agents.iter_mut().find(|run| run.id == pending.run_id) {
                if let Some(terminal) = run.terminal.as_mut() {
                    let _ = terminal.write_input(b"\r");
                    run.last_worker_input_at = Some(Instant::now());
                    self.dirty = true;
                }
            }
        }
        self.pending_enters = remaining;
    }

    fn send_to_interactive_orchestrator(&mut self, input: &str) -> bool {
        let Some(index) = self.agents.iter().position(|run| {
            self.is_interactive_orchestrator_run(run)
                && run.status == AgentStatus::Running
                && run.terminal.is_some()
        }) else {
            return false;
        };
        let run_id = self.agents[index].id.clone();
        let write_result = {
            let Some(terminal) = self.agents[index].terminal.as_mut() else {
                return false;
            };
            terminal.reset_scrollback();
            // Text only; the submitting CR is deferred (see queue_enter_for).
            terminal.write_input(&bracketed_paste_bytes(input))
        };
        if let Err(error) = write_result {
            self.notice = Some(format!("could not send to orchestrator: {error}"));
            return false;
        }
        self.queue_enter_for(&run_id);
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
            run.needs_permission = false;
            run.needs_user_input = false;
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
        if !self.plan().planner_paused_for_input {
            return false;
        }
        if self.plan().awaiting_approval
            || self.plan().refining
            || self.plan().rebasing
            || !self.plan().planned_nodes.is_empty()
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
        if !self.plan().planned_nodes.is_empty() || self.has_planning_orchestrator() {
            return true;
        }
        self.active_plan_agents()
            .any(|run| run.node_id.is_some() && run.status != AgentStatus::Merged)
    }

    /// Every live plan node title: queued (TODO) nodes plus in-flight (not-yet-merged)
    /// plan agents. Feeds the structural-vs-additive classifier's title-overlap heuristic.
    fn plan_node_titles(&self) -> Vec<String> {
        let mut titles: Vec<String> = self
            .plan()
            .planned_nodes
            .iter()
            .map(|node| node.title.clone())
            .collect();
        for run in self.active_plan_agents() {
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
            .plan()
            .planned_nodes
            .iter()
            .map(|node| (node.id.clone(), node.title.clone()))
            .collect();
        for run in self.active_plan_agents() {
            // Skip Merged (already a satisfied/dangling ref) AND Failed/Stopped: a dead
            // node id must never be advertised as a hard-dep target, or a reconcile/rebase
            // node hard-linked to it would deadlock on arrival (is_ready never satisfies a
            // hard dep whose node is not Merged).
            if matches!(
                run.status,
                AgentStatus::Merged
                    | AgentStatus::Failed
                    | AgentStatus::Stopped
                    | AgentStatus::Orphaned
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
                push_codex_planner_config_overrides(&mut args, effort);
                if !model.trim().is_empty() {
                    args.push("-m".to_string());
                    args.push(model.to_string());
                }
                args.push(prompt.to_string());
                TerminalCommand::with_args(codex_program(), args)
                    .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
            }
            // opencode has no read-only headless mode; the reconcile planner runs in
            // its TUI (no --auto, so an unexpected edit stops for approval) and its
            // plan block is read out of the pane like every other opencode planner.
            Backend::Opencode => opencode_command(model, Some(prompt), AgentMode::RudderPlan),
        }
    }

    /// Fold an ADDED task into the active plan. Spawns a RudderPlan agent flagged as
    /// a RECONCILE planner whose prompt names the current frontier and asks for
    /// exactly one new node with inferred deps. The poll loop routes that planner's
    /// completion to `evaluate_completed_reconcile`, which APPENDS the node to
    /// `planned_nodes` (never replaces) and schedules it if the session is running.
    /// True while an injection-coordinator (reconcile) planner is mid-flight.
    fn reconcile_planner_running(&self) -> bool {
        self.agents
            .iter()
            .any(|run| run.reconcile_planner && run.status == AgentStatus::Running)
    }

    /// Start the next queued injection once the current reconcile planner has
    /// finished, so injections fold into the plan one at a time instead of
    /// spawning a pane full of concurrent planners. Called from the poll loop.
    fn maybe_start_queued_reconcile(&mut self) {
        if self.pending_reconcile_inputs.is_empty() || self.reconcile_planner_running() {
            return;
        }
        if !self.plan_is_active() {
            // The plan ended (or was retired) while injections were queued; a
            // fresh plan path owns new input now, so drop the stale queue.
            self.pending_reconcile_inputs.clear();
            return;
        }
        let next = self.pending_reconcile_inputs.remove(0);
        self.reconcile_injection(&next);
    }

    fn reconcile_injection(&mut self, input: &str) {
        // Serialize injection-coordinators: only one reconcile planner runs at a
        // time. Queue anything that arrives while one is in flight and drain it
        // via maybe_start_queued_reconcile when the current one finishes.
        if self.reconcile_planner_running() {
            self.pending_reconcile_inputs.push(input.to_string());
            self.notice = Some(format!(
                "queued \"{}\" — folding it into the plan after the current one ({} waiting)",
                short_task(input),
                self.pending_reconcile_inputs.len()
            ));
            self.dirty = true;
            return;
        }
        let frontier = self.plan_frontier();
        // The injected node joins the plan whose pane the message was typed into.
        let plan_id = self.plan().id.clone();
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
            plan_id: Some(plan_id),
            reviewed_at: None,
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
            integration: IntegrationEvidence::default(),
            publish: PublishEvidence::default(),
            delivery: DeliveryEvidence::default(),
            session_id,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
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
            // Discriminator: route completion to the APPEND path, not REPLACE.
            reconcile_planner: true,
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
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
        if let Err(error) = self.write_rudder_context_timed(Some(&worktree)) {
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
        // A node carries its owning plan, so the worker row records which plan it
        // belongs to. Without it a merge could credit the wrong plan's ledger.
        let plan_id = node
            .as_ref()
            .map(|n| n.plan_id.clone())
            .filter(|id| !id.is_empty());
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
        // subprocess the agent spawns. A missing report is reconstructed from the diff.
        if let Some(id) = node_id.as_deref() {
            let done_file = worker_done_file(&worktree.path, id);
            command = command.with_env("RUDDER_DONE_FILE", done_file.to_string_lossy().to_string());
        }
        // Official completion signal: wire the backend's own Stop hook (Claude) /
        // notify (Codex) so it deterministically reports turn-end. Keyed by the run id the poll loop
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
            integration: IntegrationEvidence::default(),
            publish: PublishEvidence::default(),
            delivery: DeliveryEvidence::for_task(&goal_prompt),
            session_id,
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
            deps: node_deps,
            soft_deps: Vec::new(),
            node_id,
            plan_id,
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
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("failed to start {}: {error}", backend.as_str()));
            }
        }

        let run_id = run.id.clone();
        let spawned = run.terminal.is_some();
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        // A dead pane swallows every key except q; focusing it after a failed
        // spawn trapped the user. Stay in the task bar so they can retry.
        self.focus = if spawned {
            FocusPane::Worker
        } else {
            FocusPane::Task
        };
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
        if should_generate_summary {
            spawn_task_summary_worker(self.task_summary_tx.clone(), run_id, input.to_string());
        }
        let _ = self.write_rudder_context_timed(None);
    }

    fn start_rudder_plan_task(&mut self, input: &str) {
        // SEVERAL ORCHESTRATORS: a fresh `/plan` opens its OWN plan next to any plan
        // already running, so two goals can be decomposed and conducted side by side.
        // (This replaced an at-most-one rule that retired every existing orchestrator
        // row and wiped the plan state; that made a second `/plan` silently destroy the
        // first one's DAG.) A plan is only ever RECYCLED when it is spent — no queue, no
        // live workers, nothing at the gate — so a user who runs one `/plan` after
        // another still sees exactly one orchestrator row rather than a growing museum
        // of finished ones. refine/rebase reuse their own orchestrator in place and
        // never reach here.
        let plan_index = match self.spent_plan_index() {
            Some(index) => {
                self.plans[index] = PlanState {
                    id: self.plans[index].id.clone(),
                    ..PlanState::default()
                };
                index
            }
            None => {
                self.plans.push(PlanState {
                    id: new_plan_id(),
                    ..PlanState::default()
                });
                self.plans.len() - 1
            }
        };
        let plan_id = self.plans[plan_index].id.clone();
        // Retire only THIS plan's own leftover orchestrator row (a recycled slot can
        // still be showing the finished planner); other plans keep theirs.
        if let Some(stale) = self.orchestrator_index_for_plan(&plan_id) {
            let stale_id = self.agents[stale].id.clone();
            self.retire_planner_row(&stale_id);
        }
        let plan = &mut self.plans[plan_index];
        // Remember the original request so the refine loop can re-plan against it
        // (each refinement layers the user's feedback on top of this, not on top of
        // the previous composite prompt).
        plan.plan_request = input.to_string();
        plan.planned_origin = input.to_string();
        plan.plan_capture_armed = true;
        self.persist_plan_queue();
        let backend = self.backend;
        let interactive_planner = self.interactive_orchestrator;
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
            // This row DRIVES the plan opened above; that link is what lets each
            // orchestrator's pane keep talking to its own plan.
            plan_id: Some(plan_id),
            reviewed_at: None,
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
            integration: IntegrationEvidence::default(),
            publish: PublishEvidence::default(),
            delivery: DeliveryEvidence::default(),
            session_id,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
            plain_process: false,
            interactive_orchestrator: interactive_planner,
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
        };

        // INTERACTIVE orchestrator: it never exits and presents its DAG via RUDDER.md, so
        // it is NOT autosteered (the headless completed-plan capture must not fire).
        // Clear stale one-shot markers before spawn so a RUDDER_* action left in
        // RUDDER.md while no orchestrator was running cannot fire under this new,
        // unrelated planner. Generate project-level Claude skills before spawn when
        // the conductor is Claude so dashboard actions are also available as skills.
        if interactive_planner {
            run.autosteered = false;
            if let Err(error) = clear_orchestrator_plan_markers(&self.cwd) {
                self.notice = Some(format!("orchestrator plan cleanup warning: {error}"));
            }
            if backend == Backend::Claude {
                if let Err(error) = ensure_orchestrator_skills(&self.cwd) {
                    self.notice = Some(format!("orchestrator skills warning: {error}"));
                }
            }
        }

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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

        let spawned = run.terminal.is_some();
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.delete_pending = None;
        self.focus = if spawned {
            FocusPane::Worker
        } else {
            FocusPane::Task
        };
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
        if self.plan().refining || self.plan().rebasing {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
        // Refine the plan the user is looking at, via ITS own orchestrator: another
        // plan's planner must not be resumed with this plan's feedback.
        let Some(index) = self.active_orchestrator_index() else {
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
        let answering_clarification = self.plan().planner_paused_for_input;
        let followup = if answering_clarification {
            build_clarification_answer_followup(feedback)
        } else {
            build_refine_followup(feedback)
        };
        let command = match &session {
            Some(sid) => rudder_plan_refine_command(backend, &model, effort, &followup, sid),
            None => {
                let original = if self.plan().plan_request.trim().is_empty() {
                    self.plan().planned_origin.clone()
                } else {
                    self.plan().plan_request.clone()
                };
                let outline = self.current_plan_outline();
                let composite = build_refine_request(&original, &outline, feedback);
                agent_command_with_orchestrator_mode(
                    backend,
                    &model,
                    effort,
                    &composite,
                    AgentMode::RudderPlan,
                    mint_session_id_for(backend).as_deref(),
                    false,
                )
            }
        };
        // Mark the refine in flight: keeps awaiting_approval = true (the scheduler
        // never launches the stale plan and Enter cannot approve it mid-refine) while
        // maybe_detect_plan_ready still captures the revised DAG. evaluate_completed_plan
        // clears `refining` once the new plan lands.
        self.plan_mut().refining = true;
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
            // The prior turn's parsed DAG must not remain visible/detectable while the
            // revised turn is still streaming. `Some(default)` is a cached negative.
            run.plan_output_cache = Some(RudderPlanOutputCache::default());
        }
        if self.relaunch_orchestrator_with(index, command, feedback) {
            self.notice = Some("refining the plan with your feedback…".to_string());
        } else {
            // The planner could not be relaunched: drop back to the existing plan so
            // the user is not stuck (they can still approve it or try again).
            self.plan_mut().refining = false;
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
    /// plan integration is suppressed until the diff lands. Autonomous + logged.
    fn start_plan_rebase(&mut self, input: &str) {
        // A refine/rebase is already in flight: relaunching the orchestrator now would kill
        // the in-flight planner and race its capture (same guard refine_plan has).
        if self.plan().refining || self.plan().rebasing {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
        let Some(index) = self.active_orchestrator_index() else {
            // No orchestrator session to resume (e.g. a daemon-launched plan): a
            // structural change with no planner to re-decompose against falls back to
            // a fresh plan from the new direction.
            self.start_rudder_plan_task(input);
            return;
        };
        let (backend, model, effort, session, was_interactive) = {
            let run = &mut self.agents[index];
            if run.backend == Backend::Codex
                && run.interactive_orchestrator
                && run
                    .session_id
                    .as_deref()
                    .is_none_or(|sid| sid.trim().is_empty())
            {
                run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
            }
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
        // user keeps the conversation.
        self.plan_mut().rebase_restore_interactive = was_interactive;
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
            None => agent_command_with_orchestrator_mode(
                backend,
                &model,
                effort,
                &request,
                AgentMode::RudderPlan,
                mint_session_id_for(backend).as_deref(),
                false,
            ),
        };
        // Mark the rebase in flight BEFORE relaunch so the poll loop routes the revised
        // block to evaluate_completed_rebase and holds plan integration steady.
        self.plan_mut().rebasing = true;
        if let Some(run) = self.agents.get_mut(index) {
            if resume {
                if let Some(stream) = run.plan_stream.as_mut() {
                    stream.begin_user_turn(input);
                    stream.rebind_stream();
                }
            } else {
                run.plan_stream = Some(PlanStreamState::new());
            }
            run.plan_output_cache = Some(RudderPlanOutputCache::default());
        }
        if self.relaunch_orchestrator_with(index, command, input) {
            self.push_activity(format!("rebasing the plan: {}", short_task(input)));
        } else {
            // Could not relaunch: drop back to the current plan so the fleet keeps
            // running. Nothing was changed; the rebase is simply abandoned (the old
            // interactive session was never replaced), so there is nothing to restore.
            self.plan_mut().rebasing = false;
            self.plan_mut().rebase_restore_interactive = false;
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
        for node in &self.plan().planned_nodes {
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
                run.needs_user_input = false;
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

    /// On startup, classify persisted Running records without a native PTY. A clean
    /// merge resolver is complete; every other row is an orphan and remains visible
    /// for an explicit resume. Blindly relaunching all such rows can duplicate a task
    /// whose previous controller is still alive or whose last turn already completed.
    fn reconcile_orphaned_runs(&mut self) {
        let mut reconciled = 0usize;
        let mut orphaned = 0usize;
        for run in self.agents.iter_mut() {
            if run.terminal.is_some() || run.status != AgentStatus::Running {
                continue;
            }
            // A merge resolver whose checkout is conflict-free already did its job.
            if run.merge_resolver && jj_unresolved_conflicts(&run.cwd).is_empty() {
                mark_run_done(run);
                let _ = save_native_run_record(&self.cwd, run);
                reconciled += 1;
            } else {
                run.status = AgentStatus::Orphaned;
                run.last_error = Some(
                    "orphaned run: native PTY/controller is gone; resume explicitly".to_string(),
                );
                let _ = save_native_run_record(&self.cwd, run);
                orphaned += 1;
            }
        }
        if reconciled > 0 || orphaned > 0 {
            self.notice = Some(format!(
                "startup recovery: {reconciled} finished merge(s), {orphaned} orphaned run(s) need explicit resume"
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
            let orchestrator_interactive =
                run.mode == AgentMode::RudderPlan && run.interactive_orchestrator;
            let cmd = agent_command_with_orchestrator_mode(
                run.backend,
                &run.model,
                run.effort,
                &prompt_for_agent,
                run.mode,
                session_id.as_deref(),
                orchestrator_interactive,
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
        // Show the current name as a selected-style preview; the first typed
        // character clears it and the user retypes from the first character.
        self.rename_prefilled = true;
    }

    fn cancel_rename(&mut self) {
        self.rename_input = None;
        self.rename_cursor = 0;
        self.rename_prefilled = false;
    }

    fn commit_rename(&mut self) {
        let Some(new_name) = self.rename_input.take() else {
            return;
        };
        self.rename_cursor = 0;
        self.rename_prefilled = false;
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
                // Editing the prefilled name keeps it (only a typed char wipes it).
                self.rename_prefilled = false;
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
                self.rename_prefilled = false;
                if self.rename_cursor > 0 {
                    self.rename_cursor -= 1;
                }
            }
            KeyCode::Right => {
                self.rename_prefilled = false;
                let len = input.chars().count();
                if self.rename_cursor < len {
                    self.rename_cursor += 1;
                }
            }
            KeyCode::Home => {
                self.rename_prefilled = false;
                self.rename_cursor = 0;
            }
            KeyCode::End => {
                self.rename_prefilled = false;
                self.rename_cursor = input.chars().count();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                // First keystroke on the untouched preview clears it so the user
                // types the new name from the first character.
                if self.rename_prefilled {
                    input.clear();
                    self.rename_cursor = 0;
                    self.rename_prefilled = false;
                }
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
                let mut queued_run_id: Option<String> = None;
                let done_path = self
                    .agents
                    .get(main_index)
                    .map(|run| worker_done_file(&run.cwd, &run.id));
                if let Some(path) = done_path {
                    // A sidecar describes exactly one turn. Remove the previous
                    // report before submitting another prompt so old delivery
                    // proof cannot be mistaken for the new request's outcome.
                    let _ = std::fs::remove_file(path);
                }
                if let Some(run) = self.agents.get_mut(main_index) {
                    if let Some(terminal) = run.terminal.as_mut() {
                        let starting_new_turn = run.status == AgentStatus::Done;
                        // Text only; the submitting CR is deferred (see queue_enter_for).
                        let _ = terminal.write_input(&bracketed_paste_bytes(override_prompt));
                        let now = now_stamp();
                        run.turns.push(AgentTurn {
                            ts: now.clone(),
                            prompt: override_prompt.to_string(),
                            source: "user".to_string(),
                        });
                        run.last_user_input_at = now;
                        run.last_worker_input_at = Some(Instant::now());
                        run.current_prompt = override_prompt.to_string();
                        run.done_summary = None;
                        let requested_delivery = DeliveryEvidence::for_task(override_prompt);
                        if requested_delivery.required
                            || !run.delivery.required
                            || starting_new_turn
                        {
                            run.delivery = requested_delivery;
                        }
                        run.status = AgentStatus::Running;
                        run.completed_at = None;
                        run.needs_permission = false;
                        run.needs_user_input = false;
                        queued_run_id = Some(run.id.clone());
                    }
                }
                if let Some(run_id) = queued_run_id {
                    self.queue_enter_for(&run_id);
                }
            }
            self.focus = FocusPane::Worker;
            self.worker_view = WorkerView::Terminal;
            return;
        }

        let (backend, model, effort, terminal_size, bootstrap, session_id, resume_existing) = {
            let run = &self.agents[main_index];
            let bootstrap = if !override_prompt.is_empty() {
                override_prompt.to_string()
            } else if run.turns.is_empty() {
                MAIN_BOOTSTRAP_PROMPT.to_string()
            } else {
                String::new()
            };
            let resume_existing = override_prompt.is_empty()
                && run.status == AgentStatus::Paused
                && run
                    .session_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            (
                run.backend,
                run.model.clone(),
                run.effort,
                run.terminal_size.unwrap_or_default(),
                bootstrap,
                if resume_existing {
                    run.session_id.clone()
                } else {
                    // A stopped/completed main starts a fresh session. A Paused main
                    // is the one exception: it resumes the session captured at shutdown.
                    mint_session_id_for(run.backend)
                },
                resume_existing,
            )
        };
        let mut command = if resume_existing {
            let run = &self.agents[main_index];
            match backend {
                Backend::Claude => claude_resume_command(run, session_id.as_deref().unwrap()),
                Backend::Codex => codex_resume_command(run, session_id.as_deref().unwrap()),
                Backend::Opencode => opencode_resume_command(run, session_id.as_deref().unwrap()),
            }
        } else {
            agent_command(
                backend,
                &model,
                effort,
                &bootstrap,
                AgentMode::Main,
                session_id.as_deref(),
            )
        };
        signals::augment_worker_command(
            &mut command,
            backend,
            AgentMode::Main,
            &self.agents[main_index].id.clone(),
        );
        let done_file = worker_done_file(&self.cwd, &self.agents[main_index].id);
        if !bootstrap.is_empty() {
            let _ = std::fs::remove_file(&done_file);
        }
        command = command.with_env("RUDDER_DONE_FILE", done_file.to_string_lossy().to_string());
        let cwd = self.cwd.clone();
        let options = TerminalPaneOptions {
            size: terminal_size,
            cwd: Some(cwd.clone()),
            ..TerminalPaneOptions::default()
        };

        // Read the PRE-spawn status: the Ok arm flips it to Running before the
        // delivery guard below, and comparing the already-overwritten value
        // against Done was always false — stale delivery evidence survived a
        // fresh non-delivery turn. (The sibling sites read status first.)
        let was_done_before_spawn = self.agents[main_index].status == AgentStatus::Done;
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                let run = &mut self.agents[main_index];
                run.cwd = cwd;
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.session_id = session_id;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.needs_permission = false;
                run.last_error = None;
                if !bootstrap.is_empty() {
                    let starting_new_turn = was_done_before_spawn;
                    let now = now_stamp();
                    run.current_prompt = bootstrap.clone();
                    run.turns.push(AgentTurn {
                        ts: now.clone(),
                        prompt: bootstrap.clone(),
                        source: "bootstrap".to_string(),
                    });
                    run.last_user_input_at = now;
                    run.done_summary = None;
                    let requested_delivery = DeliveryEvidence::for_task(&bootstrap);
                    if requested_delivery.required || !run.delivery.required || starting_new_turn {
                        run.delivery = requested_delivery;
                    }
                }
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
                if resume_existing {
                    self.notice = Some("resumed paused main session".to_string());
                }
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
    /// `/restore claude|codex <session-id>`: reopen an existing CLI conversation
    /// in a new agent pane with all permissions bypassed (claude:
    /// `--permission-mode bypassPermissions`, codex:
    /// `--dangerously-bypass-approvals-and-sandbox`). Continues the SAME session
    /// in place (no fork), in the current checkout.
    fn start_restore_task(&mut self, backend: Backend, session_id: &str) {
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            self.notice = Some("usage: /restore claude|codex <session-id>".to_string());
            return;
        }
        // Claude scopes --resume lookup to the current directory's project folder
        // under ~/.claude/projects; find the transcript anywhere on disk and stage
        // it into this checkout when it was recorded elsewhere. (Codex sessions
        // are global, so no staging is needed.)
        if backend == Backend::Claude {
            if claude_transcript_path(&self.cwd, &session_id).is_none() {
                self.notice = Some(format!(
                    "no Claude session transcript found for {session_id} under ~/.claude/projects"
                ));
                return;
            }
            if let Err(error) = stage_claude_session_for_cwd(&self.cwd, &session_id, &self.cwd) {
                self.notice = Some(format!("restore failed: {error}"));
                return;
            }
        }
        let model = self.model.clone();
        let effort = self.effort;
        let short_id: String = session_id.chars().take(8).collect();
        let mut run = create_oneoff_agent(
            &self.cwd,
            backend,
            &model,
            effort,
            &format!("Restore {} session {session_id}", backend.as_str()),
        );
        run.session_id = Some(session_id.clone());
        let mut command = match backend {
            Backend::Claude => claude_resume_command(&run, &session_id),
            Backend::Codex => codex_resume_command(&run, &session_id),
            Backend::Opencode => opencode_resume_command(&run, &session_id),
        };
        // Re-wire completion hooks like every other (re)spawn path; without this
        // the run would sit "running" forever waiting for a signal the resumed
        // process was never configured to send.
        signals::augment_worker_command(&mut command, backend, run.mode, &run.id);
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.last_output_at = Instant::now();
                self.notice = Some(format!(
                    "restored {} session {short_id}… with permissions bypassed",
                    backend.as_str()
                ));
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("restore failed to start: {error}"));
            }
        }
        let spawned = run.terminal.is_some();
        let _ = save_native_run_record(&self.cwd, &run);
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.focus = if spawned {
            FocusPane::Worker
        } else {
            FocusPane::Task
        };
        self.worker_view = WorkerView::Terminal;
        self.dirty = true;
    }

    /// `/handoff <session-id> [next step]`, and every request queued by
    /// `rudder handoff`: ADOPT a conversation that was happening outside the
    /// dashboard (a plain `claude`/`codex` chat in another terminal) and continue it
    /// as a Rudder agent that already knows everything that was discussed.
    ///
    /// The conversation is FORKED, never resumed in place: the source chat is
    /// usually still open in the user's terminal, and two processes appending to one
    /// transcript corrupt it. The original is left exactly where it was.
    fn start_handoff_task(&mut self, request: HandoffRequest) {
        let session_id = request.session_id.trim().to_string();
        if !valid_session_id(&session_id) {
            self.notice = Some(
                "usage: /resume <session-id> [next step] — continue an existing claude/codex/opencode chat as a worker"
                    .to_string(),
            );
            return;
        }
        let short_id: String = session_id.chars().take(8).collect();
        // A conversation that does not exist must never create a workspace: check
        // BOTH backends here, because queued requests name their own backend.
        let exists = match request.backend {
            Backend::Claude => claude_transcript_path(&self.cwd, &session_id).is_some(),
            Backend::Codex => codex_session_exists(&session_id),
            Backend::Opencode => opencode_session_exists(&session_id),
        };
        if !exists {
            self.notice = Some(format!(
                "no {} conversation found for {short_id}… — /handoff lists the recent ones",
                request.backend.as_str()
            ));
            return;
        }
        let title = request
            .title
            .clone()
            .filter(|text| !text.trim().is_empty())
            // The palette already read every backend's title; reuse it so a Codex or
            // opencode row is named after the conversation instead of "session
            // 019f6cc3…". `conversation_title` only knows Claude transcripts.
            .or_else(|| {
                self.handoff_candidates
                    .iter()
                    .find(|candidate| candidate.session_id == session_id)
                    .map(|candidate| candidate.title.clone())
            })
            .or_else(|| conversation_title(&self.cwd, &session_id))
            .unwrap_or_else(|| format!("session {short_id}…"));
        let title = truncate_chars(title.trim(), 60);
        let instruction = request
            .instruction
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string);
        let task = match instruction.as_deref() {
            Some(next) => format!("Handed-off conversation ({title}). Next: {next}"),
            None => format!("Handed-off conversation ({title})"),
        };
        let backend = request.backend;
        let model = self.model.clone();
        let effort = self.effort;

        match request.target {
            HandoffTarget::Worker => {
                match self.spawn_forked_conversation(ForkedConversation {
                    backend,
                    model,
                    effort,
                    session_id,
                    source_cwd: self.cwd.clone(),
                    // No base change: the fork starts from the dashboard checkout's
                    // current jj change, which is the tree the conversation was about.
                    base_change: None,
                    label: format!("handoff: {title}"),
                    task,
                    seed: instruction,
                }) {
                    Ok(ForkOutcome::Started) => {
                        self.record_activity(format!("handed off conversation: {title}"));
                        self.notice = Some(format!(
                            "picked up “{title}” in an isolated worker; merge with m when it is done"
                        ));
                    }
                    Ok(ForkOutcome::SpawnFailed(error)) => {
                        self.notice = Some(format!("handoff failed to start: {error}"));
                    }
                    Err(error) => {
                        self.notice = Some(format!("handoff failed: {error}"));
                    }
                }
            }
            HandoffTarget::Here => self.start_handoff_in_main_checkout(
                backend,
                &session_id,
                &title,
                &task,
                instruction.as_deref(),
            ),
        }
    }

    /// `rudder handoff --here`: continue the conversation in the MAIN checkout.
    /// No workspace, no merge step — the fork edits the user's tree directly,
    /// which is what they want when the chat was already about these exact files.
    fn start_handoff_in_main_checkout(
        &mut self,
        backend: Backend,
        session_id: &str,
        title: &str,
        task: &str,
        instruction: Option<&str>,
    ) {
        // Claude scopes `--resume` lookup to the current directory's project folder;
        // a chat started in a subdirectory of this repo lives in a different one.
        if backend == Backend::Claude {
            if let Err(error) = stage_claude_session_for_cwd(&self.cwd, session_id, &self.cwd) {
                self.notice = Some(format!("handoff failed: {error}"));
                return;
            }
        }
        let model = self.model.clone();
        let effort = self.effort;
        let mut run = create_oneoff_agent(&self.cwd, backend, &model, effort, task);
        run.task_summary = truncate_chars(&format!("handoff: {title}"), 56);
        let mut command = match backend {
            Backend::Claude => claude_fork_command(&model, effort, session_id, instruction),
            Backend::Codex => codex_fork_command(&model, effort, session_id, instruction),
            Backend::Opencode => opencode_fork_command(&model, session_id, instruction),
        };
        signals::augment_worker_command(&mut command, backend, run.mode, &run.id);
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(self.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.last_output_at = Instant::now();
                self.record_activity(format!("handed off conversation: {title}"));
                self.notice = Some(format!(
                    "picked up “{title}” in the main checkout — it edits this tree directly"
                ));
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("handoff failed to start: {error}"));
            }
        }
        let spawned = run.terminal.is_some();
        let _ = save_native_run_record(&self.cwd, &run);
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.focus = if spawned {
            FocusPane::Worker
        } else {
            FocusPane::Task
        };
        self.worker_view = WorkerView::Terminal;
        self.dirty = true;
    }

    /// What was on screen when the user complained: enough to act on a one-line
    /// report, and nothing that could carry their code. Recent activity lines are
    /// included because they usually ARE the error text; the CLI redacts paths out
    /// of them before anything leaves the machine.
    fn feedback_context(&self, text: &str) -> serde_json::Value {
        let running = self
            .agents
            .iter()
            .filter(|run| run.status == AgentStatus::Running)
            .count();
        let mut notices: Vec<String> = self.activity_log.iter().rev().take(3).cloned().collect();
        notices.reverse();
        let last_error = self
            .agents
            .iter()
            .rev()
            .find_map(|run| run.last_error.clone());
        serde_json::json!({
            "text": text,
            "backend": self.backend.as_str(),
            "model": self.model,
            "effort": effort_label(self.effort),
            "agents": self.agents.len(),
            "agentsRunning": running,
            "notices": notices,
            "lastError": last_error,
            "focus": match self.focus {
                FocusPane::Agents => "agents",
                FocusPane::Worker => "worker",
                FocusPane::Task => "task",
            },
            "view": match self.worker_view {
                WorkerView::Terminal => "terminal",
                WorkerView::Diff => "diff",
                WorkerView::PlanReview => "plan-review",
            },
            "planActive": self.plan_is_active(),
        })
    }

    /// Keep the `/handoff` palette's conversation list fresh WITHOUT hitting the
    /// filesystem on the render path: refresh while the user is typing `/handoff`,
    /// at most once every few seconds.
    fn maybe_refresh_handoff_candidates(&mut self) {
        // Always cheap: pick up a background list that landed since the last key.
        self.drain_handoff_extra();
        if resume_command_rest(&self.task_input).is_none() {
            return;
        }
        if self
            .handoff_candidates_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        self.handoff_candidates_at = Some(Instant::now());
        self.rebuild_handoff_candidates();
        // Codex's session tree and opencode's `session list` subprocess are both too
        // slow for the input thread; fetch them together and merge when they land.
        if self.handoff_extra_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            let cwd = self.cwd.clone();
            self.handoff_extra_rx = Some(rx);
            thread::spawn(move || {
                let mut found = recent_codex_conversations(&cwd, HANDOFF_PICKER_LIMIT);
                found.extend(recent_opencode_conversations(&cwd, HANDOFF_PICKER_LIMIT));
                let _ = tx.send(found);
            });
        }
    }

    fn drain_handoff_extra(&mut self) {
        let Some(rx) = self.handoff_extra_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(found) => {
                self.handoff_extra_rx = None;
                self.handoff_extra_candidates = found;
                self.rebuild_handoff_candidates();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.handoff_extra_rx = None,
        }
    }

    /// Merge every backend's conversations into ONE list, newest first: the user is
    /// looking for "the chat I was just having", not for which CLI hosted it.
    fn rebuild_handoff_candidates(&mut self) {
        // An agent's own session is not a handoff candidate — it is already a pane,
        // and `b` branches it in place.
        let mine: HashSet<String> = self
            .agents
            .iter()
            .filter_map(|run| run.session_id.clone())
            .collect();
        let mut candidates = recent_claude_conversations(&self.cwd, HANDOFF_PICKER_LIMIT, &mine);
        candidates.extend(
            self.handoff_extra_candidates
                .iter()
                .filter(|candidate| !mine.contains(&candidate.session_id))
                .cloned(),
        );
        candidates.sort_by(|left, right| right.modified.cmp(&left.modified));
        candidates.dedup_by(|left, right| left.session_id == right.session_id);
        candidates.truncate(HANDOFF_PICKER_LIMIT);
        self.handoff_candidates = candidates;
        self.dirty = true;
    }

    /// Drain `.rudder/handoffs/`: requests dropped by `rudder handoff` running inside
    /// a live chat elsewhere. Consumed at most once — the file is removed before the
    /// agent is launched, so a crash mid-launch cannot spawn the same fork twice.
    fn poll_handoff_inbox(&mut self) {
        let dir = self.cwd.join(".rudder").join("handoffs");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        // Filename order is queue order (the CLI stamps them with the epoch millis).
        files.sort();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or_default();
        for path in files {
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| parse_handoff_request(&value, now_ms));
            // Malformed, stale, or unreadable: drop it rather than retrying forever.
            let _ = std::fs::remove_file(&path);
            let Some(request) = parsed else {
                continue;
            };
            self.start_handoff_task(request);
        }
    }

    fn open_main_model_switcher(&mut self) {
        if !self.selected_is_main() {
            return;
        }
        self.replace_task_input("/model ".to_string());
        self.select_current_model_picker_choice();
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
        let orchestrator_interactive =
            run.mode == AgentMode::RudderPlan && run.interactive_orchestrator;
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.session_id = session_id;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                run.autosteered = matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan)
                    && !orchestrator_interactive;
                run.interactive_orchestrator = orchestrator_interactive;
                run.needs_permission = false;
                run.needs_user_input = false;
                run.last_error = None;
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
                self.notice = Some(format!("restarted {}", short_task(&run.task)));
                let _ = save_native_run_record(&self.cwd, run);
                let _ = self.write_rudder_context_timed(None);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!("restart failed: {error}"));
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
    }

    /// Which slash commands people actually use (the NAME only — never the
    /// argument, which is the user's task text).
    fn emit_command_used(&mut self, input: &str) {
        let name = input
            .trim_start()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if name.starts_with('/') && name.len() > 1 {
            telemetry::emit_event("command_used", serde_json::json!({ "name": name }));
        }
    }

    fn handle_command(&mut self, input: &str) -> bool {
        self.emit_command_used(input);
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
                // Explicit multi-agent route: hand the task to the DAG orchestrator.
                let rest = command_rest(input, "/plan").trim();
                if rest.is_empty() {
                    self.notice = Some(
                        "usage: /plan <task>: plan and run an isolated multi-agent DAG (plain input uses one end-to-end main agent)"
                            .to_string(),
                    );
                } else {
                    self.start_rudder_plan_task(rest);
                }
                true
            }
            Some("/restore") => {
                // Reopen an existing claude/codex CLI conversation in a new agent
                // pane, continuing the SAME session with all permissions bypassed.
                let args = parts.collect::<Vec<_>>();
                match args.as_slice() {
                    [provider, session_id] => {
                        match Backend::parse(&provider.to_ascii_lowercase()) {
                            Some(backend) => self.start_restore_task(backend, session_id),
                            None => {
                                self.notice = Some(format!(
                                "unknown provider {provider}; usage: /restore claude|codex <session-id>"
                            ));
                            }
                        }
                    }
                    _ => {
                        self.notice = Some(
                            "usage: /restore claude|codex <session-id> — reopen that conversation in a new pane with permissions bypassed"
                                .to_string(),
                        );
                    }
                }
                true
            }
            Some("/resume") | Some("/handoff") => {
                // Adopt a chat that was happening OUTSIDE the dashboard. With no
                // session id the palette (which lists this repo's recent
                // conversations) is the interface; reaching here means the user
                // dismissed it, so say what the command wants.
                let rest = resume_command_rest(input)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let (target, session_id, instruction) =
                    crate::handoff::parse_resume_args(&rest);
                if session_id.is_empty() {
                    self.maybe_refresh_handoff_candidates();
                    let found = self.handoff_candidates.len();
                    self.notice = Some(format!(
                        "/resume <session-id> [next step] — continue an existing claude/codex/opencode chat in an isolated worker ({found} in the palette) · add --here to continue it in the main checkout instead · from inside that chat: rudder handoff \"<next step>\""
                    ));
                } else {
                    // The id may be TRUNCATED: every TUI cuts it to fit its pane,
                    // so what gets pasted here is often a prefix. Resolve it to the
                    // real one rather than handing a prefix to the backend, which
                    // fails at launch with "No saved session found with ID …".
                    let (backend, session_id) = match resolve_session(&self.cwd, &session_id) {
                        SessionLookup::Found {
                            backend,
                            session_id,
                        } => (backend, session_id),
                        SessionLookup::Ambiguous(ids) => {
                            let shown = ids
                                .iter()
                                .take(3)
                                .map(|id| truncate_chars(id, 14))
                                .collect::<Vec<_>>()
                                .join(", ");
                            self.notice = Some(format!(
                                "{} conversations start with {} ({shown}…) — paste more of the id",
                                ids.len(),
                                truncate_chars(&session_id, 14)
                            ));
                            return true;
                        }
                        SessionLookup::Missing => {
                            self.notice = Some(format!(
                                "no claude, codex, or opencode conversation found for {} — /resume lists this repo's recent chats",
                                truncate_chars(&session_id, 14)
                            ));
                            return true;
                        }
                    };
                    self.start_handoff_task(HandoffRequest {
                        session_id,
                        backend,
                        target,
                        instruction: (!instruction.is_empty()).then_some(instruction),
                        title: None,
                        created_at_ms: None,
                    });
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
            Some("/feedback") => {
                // A one-line gripe plus what was on screen. The context is
                // structured (backend, model, agent counts, recent notices, last
                // error) so a report is actionable without shipping any prompt,
                // diff, or file content — see src/feedback.ts for the redaction.
                let rest = command_rest(input, "/feedback").trim().to_string();
                if rest.is_empty() {
                    self.notice = Some(
                        "usage: /feedback <what went wrong or what you wanted> — sends the message plus version, model and recent notices; never your prompts or code"
                            .to_string(),
                    );
                } else {
                    match telemetry::submit_feedback(&self.cwd, self.feedback_context(&rest)) {
                        Ok(path) => {
                            self.record_activity(format!("feedback: {rest}"));
                            self.notice = Some(format!(
                                "thanks — feedback saved to {} and sent (paths redacted)",
                                path.file_name()
                                    .map(|name| name.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            ));
                        }
                        Err(error) => {
                            self.notice = Some(format!("feedback failed to save: {error}"));
                        }
                    }
                }
                true
            }
            Some("/telemetry") => {
                let rest = command_rest(input, "/telemetry").trim().to_string();
                let action = if rest.is_empty() {
                    "status".to_string()
                } else {
                    rest
                };
                self.start_rudder_cli_command(
                    &format!("telemetry {action}"),
                    vec!["telemetry".to_string(), action],
                );
                true
            }
            Some("/help") => {
                self.notice = Some(
                    "plain input -> one isolated mergeable worker in its own jj workspace · /plan <task> -> an orchestrator that plans and runs a DAG · /main|/m <task> -> another agent in this shared checkout · panes: Option-1/2/3 or ^W · keys: j/k select · Option-[ / Option-] step agents from any pane · Enter focus · v diff · m merge · u undo a merge · M merge all · R review all · g nest · o web ui · x stop · b branch chat · dd delete · cc clear merged · P model; commands: /model /fast /sound /color /main /plan /resume /restore /share /usage /goal /cloud /web /feedback"
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
            Some("/verify") => {
                self.plan_mut().final_gate_status = FinalGateStatus::Idle;
                self.plan_mut().final_gate_summary = None;
                self.maybe_start_final_gate();
                if self.plan().final_gate_status == FinalGateStatus::Idle {
                    self.notice = Some(
                        "verification waits until every planned node is integrated".to_string(),
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
                        let warning =
                            self.set_model_defaults(backend, model, Some(EffortLevel::Low));
                        self.notice = warning.or_else(|| {
                            Some(format!(
                                "fast mode: {} {} (low effort) for NEW agents · codex has no native fast mode, so this lowers reasoning effort · /model switches back",
                                self.backend.as_str(),
                                self.model
                            ))
                        });
                    }
                    Backend::Opencode => {
                        // No fast tier and no effort dial: say so instead of silently
                        // doing nothing (or pretending a flag was applied).
                        self.notice = Some(
                            "opencode has no fast mode or effort dial — pick a faster model with /model opencode <provider/model>"
                                .to_string(),
                        );
                    }
                }
                true
            }
            Some("/sound") => {
                let arg = parts.next().unwrap_or("toggle").to_ascii_lowercase();
                let enabled = match arg.as_str() {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    "toggle" => !config::completion_sound_enabled(),
                    _ => {
                        self.notice = Some(
                            "usage: /sound [on|off|toggle] — completion pings are off by default"
                                .to_string(),
                        );
                        return true;
                    }
                };
                match config::save_completion_sound(enabled) {
                    Ok(()) => {
                        self.notice = Some(if enabled {
                            "completion sound ON (saved): workers ping when they enter review"
                                .to_string()
                        } else {
                            "completion sound OFF (saved): workers enter review silently"
                                .to_string()
                        });
                    }
                    Err(error) => {
                        self.notice = Some(format!("sound setting failed: {error}"));
                    }
                }
                true
            }
            Some("/color") => {
                let arg = parts.next().unwrap_or("toggle");
                let mode = if arg.eq_ignore_ascii_case("toggle") {
                    color_mode().toggled()
                } else if let Some(mode) = ColorMode::parse(arg) {
                    mode
                } else {
                    self.notice = Some(
                        "usage: /color terminal|paper|toggle — terminal uses your terminal background"
                            .to_string(),
                    );
                    return true;
                };
                set_color_mode(mode);
                match config::save_color_mode(mode) {
                    Ok(()) => {
                        self.notice = Some(format!(
                            "color mode {} (saved): {}",
                            mode.as_str(),
                            match mode {
                                ColorMode::Terminal =>
                                    "using terminal foreground/background for the dashboard",
                                ColorMode::Paper => "using Rudder's white paper canvas",
                            }
                        ));
                    }
                    Err(error) => {
                        self.notice = Some(format!("color setting failed: {error}"));
                    }
                }
                self.dirty = true;
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
        // /main deliberately creates ANOTHER main agent (plain input reuses the
        // existing one) — but two live agents editing the same checkout
        // concurrently is a foot-gun worth naming when it happens.
        let live_main_exists = self
            .agents
            .iter()
            .any(|run| run.is_main() && run.status == AgentStatus::Running);
        if live_main_exists {
            self.push_activity(
                "note: another main agent is live in this same checkout — supported, but they can overwrite each other's edits"
                    .to_string(),
            );
        }
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
            "workspace",
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

    fn migrate_current_agents_to_cloud(&mut self) {
        let indices = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.status == AgentStatus::Running
                    && run.has_merge_source()
                    && !run.is_orchestrator()
                    && !run.is_main()
                    && !run.is_oneoff()
                    && !is_cloud_agent(run)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if indices.is_empty() {
            self.notice = Some("no live isolated agents to migrate to Rudder Cloud".to_string());
            return;
        }

        let mut labels = Vec::new();
        let mut run_ids = Vec::new();
        for index in indices {
            let run = &mut self.agents[index];
            if run.backend == Backend::Codex
                && run
                    .session_id
                    .as_deref()
                    .is_none_or(|session| session.trim().is_empty())
            {
                run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
            }
            run.terminal = None;
            run.review_terminal = None;
            run.needs_permission = false;
            run.needs_user_input = false;
            // Keep the record nonterminal. The cloud migration planner selects
            // running workspaces and the cloud dashboard resumes from this state.
            run.status = AgentStatus::Running;
            labels.push(run.node_id.clone().unwrap_or_else(|| run.id.clone()));
            run_ids.push(run.id.clone());
            let _ = save_native_run_record(&self.cwd, run);
        }

        let count = labels.len();
        self.push_activity(format!(
            "cloud migration: paused {count} agent(s) ({})",
            labels.join(", ")
        ));
        let cloud_label = format!("cloud migrate {count} agents");
        self.start_rudder_cli_command(
            &cloud_label,
            vec![
                "cloud".to_string(),
                "workspace".to_string(),
                "attach".to_string(),
            ],
        );
        if let Some(cloud_run) = self.agents.last_mut() {
            if cloud_run.task == cloud_label {
                cloud_run.review_source_ids = run_ids.clone();
            }
        }
        if self
            .agents
            .last()
            .is_some_and(|run| run.task == cloud_label && run.status == AgentStatus::Failed)
        {
            self.restore_running_agents();
            self.notice = Some("cloud migration failed to start; local agents resumed".to_string());
            return;
        }
        for run_id in &run_ids {
            if let Some(run) = self.agents.iter_mut().find(|run| &run.id == run_id) {
                run.status = AgentStatus::Migrated;
                run.last_error = None;
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        self.notice = Some(format!(
            "migrating {count} agent(s) to Rudder Cloud with workspaces, sessions, auth, and project environment"
        ));
        let _ = self.write_rudder_context_timed(None);
    }

    fn recover_failed_cloud_migrations(&mut self) {
        let failed = self
            .agents
            .iter_mut()
            .filter(|run| {
                run.status == AgentStatus::Failed
                    && run.task.starts_with("cloud migrate ")
                    && !run.review_source_ids.is_empty()
            })
            .flat_map(|run| std::mem::take(&mut run.review_source_ids))
            .collect::<Vec<_>>();
        if failed.is_empty() {
            return;
        }
        let mut resumed = 0usize;
        for run_id in failed {
            let Some(index) = self.agents.iter().position(|run| run.id == run_id) else {
                continue;
            };
            let entry = MigratedAgent {
                run_id: run_id.clone(),
                session_id: self.agents[index].session_id.clone().unwrap_or_default(),
                worktree_path: self.agents[index].cwd.clone(),
                fresh_prompt: Some(format!(
                    "Cloud migration failed. Continue the original task locally from the existing workspace: {}",
                    self.agents[index].task
                )),
            };
            if self.spawn_claude_resume_for(index, &entry) {
                resumed += 1;
            }
        }
        self.notice = Some(format!(
            "cloud migration failed; resumed {resumed} local agent(s)"
        ));
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
        // Resolve the CLI like every other spawn path (RUDDER_CLI, then a real
        // PATH search) instead of handing a bare "rudder" to the PTY builder;
        // this was the file's only unresolved spawn.
        let program = crate::cloudio::locate_rudder_cli()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "rudder".to_string());
        let mut command = TerminalCommand::with_args(program, args);
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
            integration: IntegrationEvidence::default(),
            publish: PublishEvidence::default(),
            delivery: DeliveryEvidence::default(),
            session_id: None,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: None,
            autosteered: true,
            plain_process: true,
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
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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

    /// Spawn exactly one normal execute worker without invoking the planner. In a git
    /// repo this uses the same jj workspace isolation as DAG workers, so the result
    /// appears in Review and is integrated with `m` or `/merge-all`.
    fn start_single_run_task(&mut self, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        let before = self.agents.len();
        self.notice = None;
        self.start_execute_task_node(input, None, None);
        let launch_notice = self.notice.clone();
        if self.agents.len() > before {
            let Some(run) = self.agents.last() else {
                return;
            };
            if run.status == AgentStatus::Running && run.has_merge_source() {
                self.notice = Some(
                    "isolated worker running; merge with m or /merge-all when done".to_string(),
                );
            } else if run.status == AgentStatus::Running
                && !launch_notice
                    .as_deref()
                    .is_some_and(|notice| notice.starts_with("jj workspace failed:"))
            {
                self.notice = Some(
                    "worker running in the current checkout; no merge step outside a git repo"
                        .to_string(),
                );
            }
        }
    }

    fn refresh_cloud_workspace_status(&mut self) {
        // Inside a worker VM the workspace identity is the machine's own env —
        // no network, no CLI spawn, nothing to poll. Resolve it before the
        // offline gate: gating it there left the label at "none" whenever the
        // network path was disabled, even though the answer was local.
        if is_cloud_worker_session() {
            let snapshot = query_cloud_workspace_status(&self.cwd);
            if snapshot.is_some() && self.cloud_workspace != snapshot {
                self.cloud_workspace = snapshot;
                self.dirty = true;
            }
            self.workspace_status_rx = None;
            return;
        }
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
            // Re-arm so reconnecting polls at once instead of inheriting the
            // backed-off cadence from the last connected session.
            self.workspace_check_interval = WORKSPACE_CHECK_BASE_INTERVAL;
            self.last_workspace_check = None;
            return;
        }
        if let Some(rx) = self.workspace_status_rx.take() {
            match rx.try_recv() {
                Ok(snapshot) => {
                    // Each poll costs a Node CLI spawn, so only keep paying that
                    // often while the answer is actually moving.
                    self.workspace_check_interval = if self.cloud_workspace == snapshot {
                        backed_off_interval(
                            self.workspace_check_interval,
                            WORKSPACE_CHECK_MAX_INTERVAL,
                        )
                    } else {
                        WORKSPACE_CHECK_BASE_INTERVAL
                    };
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
            Some(at) => at.elapsed() >= self.workspace_check_interval,
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
                && rudder_plan_tasks_for_run(run).is_some()
        });
        if let Some(index) = reconcile_index {
            self.evaluate_completed_reconcile(index);
            return;
        }

        // Each remaining orchestrator is judged against ITS OWN plan: one plan already
        // sitting at the approval gate must not suppress capture for another that is
        // still decomposing. At most one is handled per tick (as before), so a capture
        // that reshuffles rows cannot invalidate the indices behind it.
        let candidates: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.mode == AgentMode::RudderPlan
                    && !run.reconcile_planner
                    && run.autosteered
                    && rudder_plan_tasks_for_run(run).is_some()
            })
            .map(|(index, _)| index)
            .collect();
        for index in candidates {
            let plan = &self.plans[self.plan_index_for_run(index)];
            // REBASE: a structural pivot resumed the orchestrator WHILE a plan is active
            // (post-approval, nodes launched), so its revised block must be captured from
            // streaming output regardless of the initial-plan guard below (which would
            // bail on the non-empty queue). Checked first, like reconcile.
            if plan.rebasing {
                self.evaluate_completed_rebase(index);
                return;
            }
            // INITIAL plan: a fresh plan is already captured (awaiting approval, or
            // some nodes queued): do not re-capture from a still-streaming orchestrator.
            // EXCEPTION: while `refining`, the orchestrator has been relaunched to revise
            // the plan, so we MUST capture its new block even though a (stale) plan is
            // still pending; evaluate_completed_plan replaces the queue and clears the
            // flag.
            if !plan.refining && (plan.awaiting_approval || !plan.planned_nodes.is_empty()) {
                continue;
            }
            self.evaluate_completed_plan(index);
            return;
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
        // RUDDER.md is the interactive orchestrator's control channel and there is one
        // per repo, so plan-file capture is resolved from the interactive orchestrator
        // that is actually running and applied to ITS plan.
        let Some((_, plan_index)) = self.running_interactive_orchestrator_plan() else {
            return;
        };
        if !self.plans[plan_index].plan_capture_armed
            || self.plans[plan_index].refining
            || self.plans[plan_index].rebasing
            || self.plans[plan_index].awaiting_approval
        {
            return;
        }
        // Only capture a FRESH plan: nothing pending and no live/unmerged workers.
        // Historical merged nodes may still be in the agents list; they must not
        // block a brand-new orchestrator plan after the previous DAG completed.
        if !self.plans[plan_index].planned_nodes.is_empty()
            || self.agents.iter().any(|run| {
                run.node_id.is_some()
                    && self.run_belongs_to_plan(run, plan_index)
                    && run.status != AgentStatus::Merged
            })
        {
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
        let nodes = self.planned_nodes_from_fresh_tasks(&tasks);
        let count = nodes.len();
        self.adopt_plan_nodes(plan_index, nodes);
        self.plans[plan_index].plan_launched_node_ids.clear();
        self.plans[plan_index].plan_merged_node_ids.clear();
        // Node ids (n0, n1, …) are reused across plans, so review state must not
        // carry over — a new plan's n0 must not inherit an old n0's reviewed flag.
        self.plans[plan_index].reviewed_nodes.clear();
        self.plans[plan_index].node_reviewers.clear();
        self.plans[plan_index].final_gate_status = FinalGateStatus::Idle;
        self.plans[plan_index].final_gate_summary = None;
        self.plans[plan_index].plan_capture_armed = false;
        if let Some(origin) = self.running_interactive_orchestrator_task() {
            self.plans[plan_index].plan_request = origin.clone();
            self.plans[plan_index].planned_origin = origin;
        } else if !self.plans[plan_index].plan_request.trim().is_empty() {
            self.plans[plan_index].planned_origin = self.plans[plan_index].plan_request.clone();
        }
        self.plans[plan_index].plan_summary = extract_rudder_plan_summary(&text);
        self.plans[plan_index].planner_paused_for_input = false;
        self.plans[plan_index].awaiting_approval = true;
        self.persist_plan_queue();
        self.open_plan_review();
        self.notice = Some(format!(
            "orchestrator proposed a {count}-node plan — edit inline · Ctrl+Enter approve"
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
        // RUDDER.md is the interactive orchestrator's control channel and there is one
        // per repo, so plan-file capture is resolved from the interactive orchestrator
        // that is actually running and applied to ITS plan.
        let Some((_, plan_index)) = self.running_interactive_orchestrator_plan() else {
            return;
        };
        if !self.plans[plan_index].awaiting_approval
            || self.plans[plan_index].refining
            || self.plans[plan_index].rebasing
        {
            return;
        }
        if self.agents.iter().any(|run| {
            run.node_id.is_some()
                && self.run_belongs_to_plan(run, plan_index)
                && run.status != AgentStatus::Merged
        }) {
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
        let nodes = self.planned_nodes_from_fresh_tasks(&tasks);
        // Compare the DAG, not the bookkeeping: live nodes carry their owning plan's
        // stamp while freshly parsed ones do not, and that difference is not an edit.
        let unchanged = nodes.len() == self.plans[plan_index].planned_nodes.len()
            && nodes
                .iter()
                .zip(self.plans[plan_index].planned_nodes.iter())
                .all(|(parsed, live)| {
                    let mut parsed = parsed.clone();
                    parsed.plan_id = live.plan_id.clone();
                    parsed == *live
                });
        if unchanged {
            return; // unchanged since the last capture — nothing to refresh
        }
        let count = nodes.len();
        self.adopt_plan_nodes(plan_index, nodes);
        if let Some(origin) = self.running_interactive_orchestrator_task() {
            self.plans[plan_index].plan_request = origin.clone();
            self.plans[plan_index].planned_origin = origin;
        }
        self.plans[plan_index].plan_summary = extract_rudder_plan_summary(&text);
        self.persist_plan_queue();
        self.open_plan_review();
        self.notice = Some(format!(
            "orchestrator updated the plan — now {count} node(s) · edit inline · Ctrl+Enter approve"
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
        // Resolve which plan the marker approves WITHOUT requiring a live PTY: RUDDER.md
        // is the durable control channel, so a marker written just before the
        // orchestrator's turn ended must still land (it used to strand forever).
        let plan_index = self.marker_channel_plan_index();
        if !self.plans[plan_index].awaiting_approval
            || self.plans[plan_index].refining
            || self.plans[plan_index].rebasing
        {
            return;
        }
        // PRIMARY (hardened file channel): the plan is already persisted, so a
        // `RUDDER_APPROVE_PLAN` line in the plan file approves it even if the
        // orchestrator PTY has since exited, idled out of Running, or was launched
        // headless. The old code gated this whole scan on a *running interactive*
        // orchestrator, so a marker written just before the orchestrator's turn
        // ended was stranded forever and the plan never launched (silent stall).
        let file_approved = std::fs::read_to_string(orchestrator_plan_path(&self.cwd))
            .map(|text| output_has_approve_marker(&text))
            .unwrap_or(false);
        // FALLBACK (lossy PTY channel): only meaningful while an interactive
        // orchestrator is still running, since it reads that PTY's live output.
        let pty_approved = !file_approved
            && self.has_running_interactive_orchestrator()
            && self
                .agents
                .iter()
                .filter(|run| run.mode == AgentMode::RudderPlan && !run.reconcile_planner)
                .filter_map(|run| run.terminal.as_ref())
                .any(|terminal| output_has_approve_marker(terminal.output_log_snapshot()));
        if file_approved || pty_approved {
            self.approve_planned_queue_for(plan_index);
        }
    }

    fn scan_orchestrator_skill_markers(&mut self) {
        if self.plan().refining || self.plan().rebasing {
            return;
        }
        // RUDDER.md is the durable control channel. A marker written immediately
        // before the orchestrator's turn-complete signal must still execute after
        // that PTY becomes Done; gating on a live orchestrator stranded auto-ship
        // and other final actions at exactly that boundary.
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
        if marker == "RUDDER_CLOUD_MIGRATE" {
            self.migrate_current_agents_to_cloud();
            return;
        }
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
        if let Some(rest) = marker.strip_prefix("RUDDER_RUN") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_RUN requires a task".to_string());
            } else {
                // `/run` is gone; one isolated worker is what plain input means now.
                self.start_single_run_task(rest);
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
        // RUDDER_ASK is retired along with `/ask`. Older orchestrator sessions (and
        // any model working from a stale prompt) can still emit it, so route it to
        // the main checkout rather than dropping the request on the floor.
        if let Some(rest) = marker.strip_prefix("RUDDER_ASK") {
            let rest = rest.trim();
            if rest.is_empty() {
                self.notice = Some("RUDDER_ASK is retired; use RUDDER_MAIN <prompt>".to_string());
            } else {
                self.handle_command(&format!("/main {rest}"));
            }
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
        if let Some(rest) = marker.strip_prefix("RUDDER_RESUME ") {
            self.resume_agent_for_marker(rest.trim());
            return;
        }
        if marker == "RUDDER_RESUME" {
            self.notice = Some(
                "RUDDER_RESUME requires <node-or-run-id> <provider> <model> [effort] [direction]"
                    .to_string(),
            );
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
                    "skills: model, main, goal, monitor, add/replan, merge/stop/regoal/inject, usage, cloud, review-all"
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
        if !run.has_merge_source() {
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

    fn resume_agent_for_marker(&mut self, rest: &str) {
        let Some(spec) = parse_worker_resume_spec(rest) else {
            self.notice = Some(
                "RUDDER_RESUME requires <node-or-run-id> <provider> <model> [effort] [direction]"
                    .to_string(),
            );
            return;
        };
        let Some(index) = self.agent_index_for_token(&spec.target) else {
            self.notice = Some(format!("RUDDER_RESUME target not found: {}", spec.target));
            return;
        };
        let direction = if spec.direction.trim().is_empty() {
            "Continue the current task from the existing workspace. Inspect `jj diff` first, preserve useful prior work, and finish the original objective."
        } else {
            spec.direction.as_str()
        };
        if !self.retarget_agent_at(index, spec.backend, &spec.model, spec.effort, direction) {
            self.notice = Some(format!("could not resume {}", spec.target));
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

    fn evaluate_completed_plan(&mut self, index: usize) {
        // Capture belongs to the plan THIS orchestrator drives, not to whichever
        // plan the user is currently looking at.
        let plan_index = self.plan_index_for_run(index);
        // A REBASE in flight must NEVER reach the initial-plan REPLACE path (it would wipe
        // the running plan's todo queue and re-gate the fleet); evaluate_completed_rebase
        // owns that case. Defensive guard in case a caller routes a rebase planner here.
        if self.plans[plan_index].rebasing {
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
                // opencode keeps its transcript in a database Rudder does not read;
                // the pane text IS the plan source for this backend.
                Backend::Opencode => None,
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
                self.plans[plan_index].refining = false;
                // No DAG yet: the planner likely asked a clarifying question or needs more
                // detail. Mark it PAUSED-for-input so the next typed message RESUMES this
                // planning conversation (NOT a leftover Done orchestrator from a shipped
                // plan, which must start fresh), and capture its questions for the prompt.
                self.plans[plan_index].planner_paused_for_input = true;
                self.plans[plan_index].pending_questions = planner_questions_or_forced(&output);
                self.notice = Some(format!(
                    "planner is waiting ({error}) — type your answer or more detail to continue planning"
                ));
                return;
            }
        };
        // A parsed first-turn DAG is a RESULT, not a violation: the planner asks
        // clarifying questions only when something material is missing (prompt),
        // and the approval gate below is the human checkpoint either way. The
        // old mandatory question round discarded this DAG and forced a second
        // full planner round-trip for every request, even trivial ones.
        // Capture the planner's prose after the block (assumptions / open questions)
        // so the orchestrator pane can show what it assumed and invite refinement.
        let summary = extract_rudder_plan_summary(&output);
        if tasks.is_empty() {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            self.plans[plan_index].refining = false;
            self.plans[plan_index].planner_paused_for_input = true;
            self.plans[plan_index].pending_questions = planner_questions_or_forced(&output);
            // Same as the Err branch: keep the planner resumable so a typed answer
            // continues this conversation rather than starting a fresh planner.
            self.notice = Some(
                "planner is waiting (it asked a question or needs detail) — type your answer to continue planning"
                    .to_string(),
            );
            return;
        }
        // Clear the planner's autosteer flag so it is captured once, but KEEP the
        // run: it stays pinned at the top of the list as the orchestrator that owns
        // the plan. Its worker pane renders the DAG tree of the parsed tasks.
        run.plan_output_cache = Some(RudderPlanOutputCache {
            tasks: Some(tasks.clone()),
            summary: summary.clone(),
        });
        run.autosteered = false;
        let _ = save_native_run_record(&self.cwd, run);
        let _ = run;
        // A DAG WAS captured: this planner is no longer paused for input.
        self.plans[plan_index].planner_paused_for_input = false;
        self.plans[plan_index].pending_questions.clear();

        // Queue every task as a PLANNED node. A new plan replaces any pending queue
        // (the planner is the single source of truth for the active plan).
        let nodes = self.planned_nodes_from_fresh_tasks(&tasks);
        let count = nodes.len();
        self.adopt_plan_nodes(plan_index, nodes);
        self.plans[plan_index].plan_launched_node_ids.clear();
        self.plans[plan_index].plan_merged_node_ids.clear();
        // Node ids (n0, n1, …) are reused across plans, so review state must not
        // carry over — a new plan's n0 must not inherit an old n0's reviewed flag.
        self.plans[plan_index].reviewed_nodes.clear();
        self.plans[plan_index].node_reviewers.clear();
        self.plans[plan_index].final_gate_status = FinalGateStatus::Idle;
        self.plans[plan_index].final_gate_summary = None;
        self.plans[plan_index].plan_capture_armed = false;
        // Keep planned_origin anchored to the ORIGINAL request so refine rounds (whose
        // run.task is a composite "revise this" prompt) do not overwrite it; worker
        // launch prompts and further refinements stay tied to what the user asked for.
        self.plans[plan_index].planned_origin =
            if self.plans[plan_index].plan_request.trim().is_empty() {
                planner_task
            } else {
                self.plans[plan_index].plan_request.clone()
            };
        self.plans[plan_index].plan_summary = summary;
        // The revised plan has landed: the refine round is complete.
        self.plans[plan_index].refining = false;

        // APPROVAL GATE: do NOT launch. Hold the plan until the user approves so
        // they can review, discuss/refine (type in the task pane), or approve it.
        self.plans[plan_index].awaiting_approval = true;
        // Persist the captured queue + gate so a restart resumes at this gate.
        self.persist_plan_queue();
        self.open_plan_review();
        // Surface (never silently drop) when the planner emitted more tasks than the
        // runaway backstop allows.
        let emitted = rudder_plan_block_task_count(&output).unwrap_or(count);
        self.notice = Some(if emitted > MAX_PLAN_TASKS {
            format!(
                "plan ready: {count} of {emitted} node(s) (capped at {MAX_PLAN_TASKS}). Edit inline · Ctrl+Enter approve"
            )
        } else {
            format!("plan ready: {count} node(s). Edit inline · Ctrl+Enter approve")
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
        // Capture belongs to the plan THIS orchestrator drives, not to whichever
        // plan the user is currently looking at.
        let plan_index = self.plan_index_for_run(index);
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
                recover_completed_codex_rudder_plan_output(run),
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

            self.push_plan_node(plan_index, node);
            self.plans[plan_index].final_gate_status = FinalGateStatus::Idle;
            self.plans[plan_index].final_gate_summary = None;
            appended += 1;
        }

        // Keep the planned-origin populated so worker prompts have an origin even
        // when reconcile happens before any initial plan was captured.
        if self.plans[plan_index].planned_origin.trim().is_empty() {
            self.plans[plan_index].planned_origin = planner_task;
        }

        // The reconcile planner has done its job; retire it (drop from self.agents AND
        // delete its run.json) so it never lingers as a second pinned orchestrator nor
        // reloads as one next launch. The initial planner is KEPT as the orchestrator.
        self.retire_planner_row(&run_id);

        // If the session is already approved/running, schedule so the new node
        // launches as soon as its deps are met. If the initial plan is still
        // awaiting approval, leave the node QUEUED: it joins the plan the user
        // approves at the gate.
        if self.plans[plan_index].awaiting_approval {
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
        // Capture belongs to the plan THIS orchestrator drives, not to whichever
        // plan the user is currently looking at.
        let plan_index = self.plan_index_for_run(index);
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
        let output = recover_completed_codex_rudder_plan_output(&self.agents[index]);

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
                self.plans[plan_index].rebasing = false;
                self.plans[plan_index].refining = false;
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
        if let Some(run) = self.agents.get_mut(index) {
            run.plan_output_cache = Some(RudderPlanOutputCache {
                tasks: Some(new_tasks.clone()),
                summary: summary.clone(),
            });
        }

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
        let todo = self.plans[plan_index].planned_nodes.clone();

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
        self.adopt_plan_nodes(plan_index, diff.todo);
        self.plans[plan_index].plan_summary = summary;
        self.persist_plan_queue();

        // The rebase has landed: clear the rebase + refine flags + the planner's
        // capture-once flag (refining may have been set by an interleaved refine; leaving
        // it true would wedge the approval gate forever).
        self.plans[plan_index].rebasing = false;
        self.plans[plan_index].refining = false;
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
    /// `rebase_restore_interactive` was armed (the rebase was on an interactive
    /// conductor) and the orchestrator row carries a resumable session. We resume that
    /// session so the conductor keeps the full conversation plus the rebase turn, and
    /// re-mark the row interactive + not-autosteered (matching the initial spawn) so the
    /// completion router never treats this live session's later exit as a fresh plan.
    fn restore_interactive_conductor_after_rebase(&mut self, index: usize) {
        if !self.plan().rebase_restore_interactive {
            return;
        }
        self.plan_mut().rebase_restore_interactive = false;
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
            run.backend,
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.completed_at = None;
                run.last_output_at = Instant::now();
                // Interactive conductor again, and NOT autosteered (its DAG is captured
                // from the plan file / markers, never from a headless completed-plan exit).
                run.interactive_orchestrator = true;
                run.autosteered = false;
                run.needs_permission = false;
                run.needs_user_input = false;
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
        self.record_activity(msg);
    }

    /// Append a one-line entry to the conductor activity log without taking over
    /// the task-pane notice. Used when a modal is already carrying the active prompt.
    fn record_activity(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
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
        self.write_activity_line(&path, line);
    }

    /// Record an operation WITH the ids it touched. A line of prose cannot answer
    /// "what happened to this merge?" three hours later; a change id, a bookmark
    /// and an op id can — and they join to `jj op log`, which is the only other
    /// record of the same events.
    fn record_event(&self, kind: &str, text: &str, fields: serde_json::Value) {
        let dir = self.cwd.join(".rudder");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("activity.jsonl");
        let mut line = serde_json::json!({ "ts": now_stamp(), "text": text, "kind": kind });
        if let (Some(target), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
            for (key, value) in extra {
                if !value.is_null() {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        self.write_activity_line(&path, line);
    }

    fn write_activity_line(&self, path: &Path, line: serde_json::Value) {
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
        // Only on CHANGE. An unconditional heartbeat every 45s buried every real
        // event: one repo's log held 3,000 lines, all the same sentence, and no
        // record of the merges and sweeps that actually happened.
        if self.last_heartbeat_summary.as_deref() == Some(summary.as_str()) {
            return;
        }
        self.last_heartbeat_summary = Some(summary.clone());
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
        let mut requests: Vec<(std::path::PathBuf, String, String, String, String)> = Vec::new();
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
            let request_id = value
                .get("requestId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let kind = value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("steer")
                .trim()
                .to_string();
            requests.push((path, task_id, instruction, request_id, kind));
        }
        // Stable order so multiple queued steers apply in filename (timestamp) order.
        requests.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, task_id, instruction, request_id, kind) in requests {
            let valid_request_id = request_id.len() >= 8
                && request_id.len() <= 128
                && request_id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
            if valid_request_id
                && self
                    .cwd
                    .join(".rudder")
                    .join("steer-receipts")
                    .join(format!("{request_id}.json"))
                    .is_file()
            {
                // Receipt existence is the at-most-once ledger. A crash after
                // injection but before inbox cleanup must not inject it again.
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if instruction.is_empty() {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if valid_request_id && self.write_steer_processing(&request_id).is_err() {
                // Do not touch the PTY until the at-most-once tombstone is
                // durable. A later poll can safely retry the filesystem write.
                continue;
            }
            let result = match kind.as_str() {
                "cancel" => self.deliver_cancel_request(&task_id),
                "merge" => self.deliver_merge_request(&task_id),
                _ => self.deliver_steer_request(&task_id, &instruction),
            };
            let _ = self.write_steer_receipt(&request_id, &result);
            // The durable receipt is written after the injection attempt, then
            // the request is consumed. This gives the browser a correlated
            // delivered/failed outcome without risking repeated PTY input.
            let _ = std::fs::remove_file(&path);
        }
    }

    fn write_steer_processing(&self, request_id: &str) -> io::Result<()> {
        let dir = self.cwd.join(".rudder").join("steer-receipts");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{request_id}.json"));
        let temp = dir.join(format!("{request_id}.{}.tmp", std::process::id()));
        let value = serde_json::json!({
            "requestId": request_id,
            "status": "processing",
        });
        std::fs::write(&temp, serde_json::to_vec(&value)?)?;
        std::fs::rename(temp, file)
    }

    fn write_steer_receipt(&self, request_id: &str, result: &SteerDelivery) -> io::Result<()> {
        if request_id.len() < 8
            || request_id.len() > 128
            || !request_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Ok(());
        }
        let dir = self.cwd.join(".rudder").join("steer-receipts");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{request_id}.json"));
        let temp = dir.join(format!("{request_id}.{}.tmp", std::process::id()));
        let value = match result {
            SteerDelivery::Delivered => serde_json::json!({
                "requestId": request_id,
                "status": "delivered",
            }),
            SteerDelivery::Failed(error) => serde_json::json!({
                "requestId": request_id,
                "status": "failed",
                "error": error,
            }),
        };
        std::fs::write(&temp, serde_json::to_vec(&value)?)?;
        std::fs::rename(temp, file)
    }

    /// Route one steer instruction from the web board to its target agent (or the
    /// high-level task-input path) and record it in the activity feed. A blank/
    /// `conductor`/`orchestrator` target deliberately goes through
    /// `start_task_from_input`: that is the same decision path as the task pane, so
    /// it can converse with a live conductor, refine/rebase an active plan, or start
    /// fresh work even when no conductor PTY currently exists.
    fn deliver_steer_request(&mut self, task_id: &str, instruction: &str) -> SteerDelivery {
        // The web board is now token-gated, but treat steer text as untrusted in depth:
        // Strip terminal escape/control characters while preserving intentional
        // newlines. live_inject_at wraps multiline text in bracketed paste, so
        // those newlines remain one composed message rather than extra submits.
        let instruction = sanitize_steer_instruction(instruction);
        if instruction.is_empty() {
            return SteerDelivery::Failed(
                "instruction became empty after sanitization".to_string(),
            );
        }
        let conductor_target = task_id.is_empty()
            || task_id.eq_ignore_ascii_case("conductor")
            || task_id.eq_ignore_ascii_case("orchestrator");
        if conductor_target {
            self.start_task_from_input(&instruction);
            // Do not replace the notice produced by the task-input path (for
            // example a command result or a planner error); the activity stream is
            // enough to acknowledge the web action without hiding that feedback.
            self.record_activity(format!("steered conductor: {instruction}"));
            return SteerDelivery::Delivered;
        }

        let index = self.agent_index_for_token(task_id);
        let Some(index) = index else {
            self.push_activity(format!("steer: target not found ({task_id})"));
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        };
        let label = task_id.to_string();
        match self.agents[index].status {
            AgentStatus::Running => {
                if self.live_inject_at(index, &instruction) {
                    // A browser steer is a real user turn, not an ephemeral PTY
                    // keystroke. Persist the same conversation metadata as direct
                    // worker/orchestrator input so restart/context views retain it.
                    let now = now_stamp();
                    let cwd = self.cwd.clone();
                    if let Some(run) = self.agents.get_mut(index) {
                        run.current_prompt = instruction.clone();
                        run.turns.push(AgentTurn {
                            ts: now.clone(),
                            prompt: instruction.clone(),
                            source: "user".to_string(),
                        });
                        run.last_user_input_at = now;
                        run.needs_user_input = false;
                        let _ = save_native_run_record(&cwd, run);
                    }
                    self.push_activity(format!("steered {label}: {instruction}"));
                    SteerDelivery::Delivered
                } else {
                    self.push_activity(format!("steer: {label} has no live terminal"));
                    SteerDelivery::Failed(format!("{label} has no live terminal"))
                }
            }
            AgentStatus::Done => {
                // Completed workers keep their workspace and session. Re-goal the
                // same row so review feedback resumes in place instead of being
                // dropped into an idle PTY or spawning unrelated work.
                if !self.regoal_agent_at(index, &instruction) {
                    self.record_activity(format!("steer: {label} could not be resumed"));
                    SteerDelivery::Failed(format!("{label} could not be resumed"))
                } else {
                    SteerDelivery::Delivered
                }
            }
            status => {
                self.push_activity(format!(
                    "steer: {label} unavailable (status={})",
                    status.as_str()
                ));
                SteerDelivery::Failed(format!("{label} is unavailable ({})", status.as_str()))
            }
        }
    }

    fn deliver_cancel_request(&mut self, task_id: &str) -> SteerDelivery {
        let Some(index) = self.agent_index_for_token(task_id) else {
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        };
        if self.agents[index].is_orchestrator() {
            return SteerDelivery::Failed(
                "the conductor cannot be stopped from a task card".to_string(),
            );
        }
        if self.stop_agent_at(index) {
            SteerDelivery::Delivered
        } else {
            SteerDelivery::Failed(format!("{task_id} could not be stopped"))
        }
    }

    fn deliver_merge_request(&mut self, task_id: &str) -> SteerDelivery {
        if self.agent_index_for_token(task_id).is_none() {
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        }
        // The failure branch below reports self.notice as the reason; a notice
        // left over from an unrelated action would masquerade as this merge's
        // error on the board.
        self.notice = None;
        self.merge_agent_for_marker(task_id);
        match self.agent_index_for_token(task_id) {
            Some(index) if self.agents[index].status == AgentStatus::Merged => {
                SteerDelivery::Delivered
            }
            _ => SteerDelivery::Failed(
                self.notice
                    .clone()
                    .unwrap_or_else(|| format!("{task_id} could not be merged")),
            ),
        }
    }

    /// Open the live web board in the user's default browser (the `/web` command and
    /// the `o` key). No-op with a notice when no board URL was provided.
    fn open_web_ui(&mut self) {
        match self.board_url.clone() {
            Some(url) => match open_url_in_browser(&url) {
                // Keep the URL hidden on success (the hotkey is the way in); only
                // reveal it if auto-open fails so the user can open it manually.
                Ok(()) => {
                    self.notice =
                        Some("opening this project's web view in your browser".to_string())
                }
                Err(err) => {
                    self.notice = Some(format!(
                        "could not open browser ({err}); open {url} manually"
                    ))
                }
            },
            None => {
                self.notice = Some("web UI is not running for this session".to_string());
            }
        }
    }

    /// Live plan size = queued nodes + launched-but-not-merged plan agents. The
    /// auto-expansion backstop guards this against MAX_PLAN_TASKS.
    fn plan_node_count(&self) -> usize {
        self.plan().planned_nodes.len()
            + self
                .active_plan_agents()
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
        self.plan()
            .planned_nodes
            .iter()
            .any(|n| norm(&n.title) == want)
            || self.active_plan_agents().any(|r| {
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
        if grew && !self.plan().awaiting_approval {
            self.run_scheduler();
            self.mirror_graph();
        }
    }

    /// Main/one-off agents do not participate in the DAG follow-up ledger, but
    /// their `rudder done` report is still authoritative session and delivery
    /// evidence. Read it directly so a finished end-to-end owner has a durable
    /// outcome instead of an empty generic "review" row.
    fn ingest_direct_completion_notes(&mut self) {
        let notes = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.status == AgentStatus::Done && (run.is_main() || run.is_oneoff())
            })
            .filter_map(|(index, run)| {
                std::fs::read_to_string(worker_done_file(&run.cwd, &run.id))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .map(|note| (index, note))
            })
            .collect::<Vec<_>>();
        for (index, note) in notes {
            self.set_run_done_summary(index, &note);
        }
    }

    /// Recover the finishing worker's completion note from its sidecar and append its
    /// in-scope follow-ups (deduped, depth- and cap-guarded). If no note was filed, spawn a
    /// one-shot summarizer over the worker's diff so a silent agent still advances the plan
    /// (the run is held `pending`, not marked ingested, until that result lands). Returns
    /// true only when a node was added synchronously here.
    fn ingest_worker_followups(&mut self, index: usize) -> bool {
        let (run_id, node_id, sidecar_note, cwd, task, is_complete) = {
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
            (
                run.id.clone(),
                node_id,
                sidecar_note,
                run.cwd.clone(),
                run.task_summary.clone(),
                is_complete,
            )
        };

        if let Some(note) = sidecar_note {
            // A note came through a direct channel. If the agent ADDRESSED follow-ups at
            // all (a `followups` array, even empty), trust it: apply what is there and we
            // are done (an empty list is a deliberate "nothing further"). If the note never
            // addressed follow-ups (freeform prose, or a summary with no `followups` key),
            // the agent's next-steps are unstructured, so fall through to the diff-backstop
            // and feed it the agent's own words. This is the freeform-prose recovery.
            let addressed_followups = note.get("followups").is_some();
            // Keep the worker's own account of what it did on the run, so the
            // finished-worker card can show "objective + what it did" (and refresh
            // it on every later completion of the same, re-goaled session).
            self.set_run_done_summary(index, &note);
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

    /// Store the completion note's human summary and structured delivery proof.
    /// Delivery stays proof-gated: a requested ship cannot be cleared by filing
    /// `required:false`, and incomplete evidence remains visibly pending.
    fn set_run_done_summary(&mut self, index: usize, note: &serde_json::Value) {
        let summary = note
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let delivery = note
            .get("delivery")
            .and_then(|value| DeliveryEvidence::from_json(value, true));
        let cwd = self.cwd.clone();
        if let Some(run) = self.agents.get_mut(index) {
            let mut changed = false;
            if let Some(summary) = summary {
                if run.done_summary.as_deref() != Some(summary.as_str()) {
                    run.done_summary = Some(summary);
                    changed = true;
                }
            }
            if let Some(mut delivery) = delivery {
                delivery.required |= run.delivery.required;
                if delivery.required && delivery.status == DeliveryStatus::NotRequested {
                    delivery.status = DeliveryStatus::Pending;
                }
                if run.delivery != delivery {
                    run.delivery = delivery;
                    changed = true;
                }
            }
            if changed {
                let _ = save_native_run_record(&cwd, run);
            }
        }
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
            if let Some(note) = result.note {
                if let Some(index) = self.agents.iter().position(|run| run.id == result.run_id) {
                    self.set_run_done_summary(index, &note);
                }
                if self.apply_worker_followups(&result.node_id, &note) {
                    grew = true;
                }
            }
            self.mark_run_ingested(result.run_id);
        }
        if grew && !self.plan().awaiting_approval {
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
        let snapshot = PlanQueueFile {
            plans: self
                .plans
                .iter()
                .map(PlanQueueSnapshot::from_plan)
                .collect(),
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

    pub(crate) fn ensure_plan_review_state(&mut self) {
        if !self.plan().awaiting_approval {
            return;
        }
        let signature = plan_review_signature(&self.plan().planned_nodes);
        if self.plan().plan_review.nodes.is_empty()
            || (!self.plan().plan_review.dirty && self.plan().plan_review.signature != signature)
        {
            self.plan_mut().plan_review =
                PlanReviewState::from_planned_nodes(&self.plan().planned_nodes);
        }
    }

    fn reset_plan_review_from_nodes(&mut self) {
        self.plan_mut().plan_review =
            PlanReviewState::from_planned_nodes(&self.plan().planned_nodes);
    }

    fn open_plan_review(&mut self) {
        if self.plan().planned_nodes.is_empty() {
            return;
        }
        self.reset_plan_review_from_nodes();
        if let Some(index) = self.active_orchestrator_index() {
            self.selected_agent = index;
        }
        self.worker_view = WorkerView::PlanReview;
        self.focus = FocusPane::Worker;
        self.orch_selection = None;
        self.worker_selection = None;
        self.dirty = true;
    }

    fn parse_plan_review_deps(value: &str) -> Vec<String> {
        let mut out = Vec::new();
        for dep in value
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .map(str::trim)
            .filter(|dep| !dep.is_empty())
        {
            if !out.iter().any(|existing| existing == dep) {
                out.push(dep.to_string());
            }
        }
        out
    }

    fn plan_review_drafts_to_nodes(
        &self,
    ) -> std::result::Result<(Vec<PlannedNode>, Vec<String>), Vec<String>> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let ids: Vec<String> = self
            .plan()
            .plan_review
            .nodes
            .iter()
            .map(|node| node.id.trim().to_string())
            .collect();
        let id_set: HashSet<String> = ids.iter().cloned().collect();
        if ids.len() != id_set.len() {
            errors.push("node ids must be unique".to_string());
        }
        let mut tasks = Vec::new();
        for node in &self.plan().plan_review.nodes {
            let id = node.id.trim();
            if id.is_empty() {
                errors.push("node id cannot be empty".to_string());
                continue;
            }
            let title = node.title.trim();
            let prompt = node.prompt.trim();
            if title.is_empty() {
                errors.push(format!("{id}: title cannot be empty"));
            }
            if prompt.is_empty() {
                errors.push(format!("{id}: prompt cannot be empty"));
            }
            let hard_deps = Self::parse_plan_review_deps(&node.hard_deps);
            let soft_deps = Self::parse_plan_review_deps(&node.soft_deps);
            let mut deps = Vec::new();
            for dep in &hard_deps {
                if dep == id {
                    errors.push(format!("{id}: hard dep cannot reference itself"));
                } else if !id_set.contains(dep) {
                    errors.push(format!("{id}: unknown hard dep `{dep}`"));
                } else {
                    deps.push(PlanEdge {
                        on: dep.clone(),
                        edge: EdgeType::Hard,
                        // The parser demotes hard edges with no justification to
                        // soft (an LLM habit-check). These edges are USER-typed:
                        // a round-trip through the RUDDER.md block must not
                        // silently discard the user's explicit ordering.
                        why: Some("user-specified ordering".to_string()),
                    });
                }
            }
            for dep in &soft_deps {
                if dep == id {
                    errors.push(format!("{id}: soft dep cannot reference itself"));
                } else if !id_set.contains(dep) {
                    errors.push(format!("{id}: unknown soft dep `{dep}`"));
                } else {
                    deps.push(PlanEdge {
                        on: dep.clone(),
                        edge: EdgeType::Soft,
                        why: None,
                    });
                }
            }
            if node.goal.trim().is_empty() {
                warnings.push(format!("{id}: goal is empty; Rudder will derive one"));
            }
            if node.success.trim().is_empty() {
                warnings.push(format!("{id}: done-when is empty; Rudder will derive one"));
            }
            tasks.push(RudderPlanTask {
                id: id.to_string(),
                title: title.to_string(),
                prompt: prompt.to_string(),
                goal: optional_nonempty(&node.goal),
                success: optional_nonempty(&node.success),
                deps,
                backend: node.backend.clone(),
                model: node.model.clone(),
                effort: node.effort.clone(),
            });
        }
        if errors.is_empty() {
            if let Err(error) = assert_no_hard_cycle(&tasks) {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let nodes = tasks.iter().map(PlannedNode::from_task).collect();
        Ok((nodes, warnings))
    }

    /// Commit the draft edits of the plan the UI is acting on.
    fn commit_plan_review_edits(&mut self) -> bool {
        let plan_index = self.active_plan_index();
        self.commit_plan_review_edits_for(plan_index)
    }

    fn commit_plan_review_edits_for(&mut self, plan_index: usize) -> bool {
        let (nodes, warnings) = match self.plan_review_drafts_to_nodes() {
            Ok(result) => result,
            Err(errors) => {
                self.plans[plan_index].plan_review.errors = errors;
                self.plans[plan_index].plan_review.warnings.clear();
                self.plans[plan_index].plan_review.dirty = true;
                self.notice = Some("fix the plan review errors before approval".to_string());
                self.dirty = true;
                return false;
            }
        };
        self.adopt_plan_nodes(plan_index, nodes);
        self.persist_plan_queue();
        let tasks: Vec<RudderPlanTask> = self
            .plan()
            .planned_nodes
            .iter()
            .map(PlannedNode::to_task)
            .collect();
        let block = rudder_plan_tasks_block(&tasks);
        if let Err(error) = replace_rudder_plan_block(&self.cwd, &block) {
            self.plans[plan_index].plan_review.errors =
                vec![format!("could not update RUDDER.md: {error}")];
            self.plans[plan_index].plan_review.warnings.clear();
            self.plans[plan_index].plan_review.dirty = true;
            self.notice = Some("plan saved to queue but RUDDER.md update failed".to_string());
            self.dirty = true;
            return false;
        }
        self.plans[plan_index].plan_review =
            PlanReviewState::from_planned_nodes(&self.plans[plan_index].planned_nodes);
        self.plans[plan_index].plan_review.warnings = warnings;
        self.notice = Some("plan updated".to_string());
        self.dirty = true;
        true
    }

    fn with_plan_review_text_mut<F>(&mut self, edit: F)
    where
        F: FnOnce(&mut String, &mut usize),
    {
        let state = &mut self.plan_mut().plan_review;
        let selected = state.selected;
        let field = state.field;
        let cursor = &mut state.cursor;
        let Some(node) = state.nodes.get_mut(selected) else {
            return;
        };
        let text = match field {
            PlanReviewField::Title => &mut node.title,
            PlanReviewField::Goal => &mut node.goal,
            PlanReviewField::Success => &mut node.success,
            PlanReviewField::Prompt => &mut node.prompt,
            PlanReviewField::HardDeps => &mut node.hard_deps,
            PlanReviewField::SoftDeps => &mut node.soft_deps,
        };
        edit(text, cursor);
        *cursor = (*cursor).min(text.chars().count());
        state.dirty = true;
        state.errors.clear();
        state.warnings.clear();
        self.dirty = true;
    }

    fn select_plan_review_node(&mut self, delta: isize) {
        let len = self.plan().plan_review.nodes.len();
        if len == 0 {
            return;
        }
        let current = self.plan().plan_review.selected as isize;
        let next = (current + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        self.plan_mut().plan_review.selected = next;
        self.plan_mut().plan_review.cursor = self.plan().plan_review.active_text().chars().count();
        self.dirty = true;
    }

    fn handle_plan_review_key(&mut self, key: KeyEvent) -> bool {
        self.ensure_plan_review_state();
        match key.code {
            KeyCode::Esc => {
                self.worker_view = WorkerView::Terminal;
                self.notice = Some(
                    "plan review hidden; press v on the orchestrator to edit again".to_string(),
                );
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::META) =>
            {
                if self.commit_plan_review_edits() {
                    self.approve_planned_queue();
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.commit_plan_review_edits();
            }
            KeyCode::Tab => {
                let field = self.plan().plan_review.field.next();
                self.plan_mut().plan_review.set_field(field);
            }
            KeyCode::BackTab => {
                let field = self.plan().plan_review.field.previous();
                self.plan_mut().plan_review.set_field(field);
            }
            KeyCode::Up | KeyCode::Char('k')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META) =>
            {
                self.select_plan_review_node(-1);
            }
            KeyCode::Down | KeyCode::Char('j')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META) =>
            {
                self.select_plan_review_node(1);
            }
            KeyCode::PageUp => {
                let page = page_scroll_rows(self.worker_area).max(1) as usize;
                self.plan_mut().plan_review.scroll =
                    self.plan().plan_review.scroll.saturating_sub(page);
            }
            KeyCode::PageDown => {
                let page = page_scroll_rows(self.worker_area).max(1) as usize;
                self.plan_mut().plan_review.scroll =
                    self.plan().plan_review.scroll.saturating_add(page);
            }
            KeyCode::Enter if self.plan().plan_review.field == PlanReviewField::Prompt => {
                self.with_plan_review_text_mut(|text, cursor| {
                    insert_char_at_cursor(text, cursor, '\n');
                });
            }
            KeyCode::Enter => {
                let field = self.plan().plan_review.field.next();
                self.plan_mut().plan_review.set_field(field);
            }
            KeyCode::Backspace => {
                self.with_plan_review_text_mut(|text, cursor| {
                    if key.modifiers.intersects(
                        KeyModifiers::ALT
                            | KeyModifiers::CONTROL
                            | KeyModifiers::SUPER
                            | KeyModifiers::META,
                    ) {
                        delete_previous_word_at(text, cursor);
                    } else {
                        delete_char_before_cursor(text, cursor);
                    }
                });
            }
            KeyCode::Delete => {
                self.with_plan_review_text_mut(|text, cursor| {
                    delete_char_at_cursor(text, *cursor);
                });
            }
            KeyCode::Left => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META)
                {
                    let pos = previous_word_position(
                        self.plan().plan_review.active_text(),
                        self.plan().plan_review.cursor,
                    );
                    self.plan_mut().plan_review.cursor = pos;
                } else {
                    self.plan_mut().plan_review.cursor =
                        self.plan().plan_review.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::META)
                {
                    let pos = next_word_position(
                        self.plan().plan_review.active_text(),
                        self.plan().plan_review.cursor,
                    );
                    self.plan_mut().plan_review.cursor = pos;
                } else {
                    let len = self.plan().plan_review.active_text().chars().count();
                    self.plan_mut().plan_review.cursor =
                        (self.plan().plan_review.cursor + 1).min(len);
                }
            }
            KeyCode::Home | KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.plan_mut().plan_review.cursor = 0;
            }
            KeyCode::End | KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.plan_mut().plan_review.cursor =
                    self.plan().plan_review.active_text().chars().count();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.with_plan_review_text_mut(|text, cursor| {
                    text.clear();
                    *cursor = 0;
                });
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.with_plan_review_text_mut(|text, cursor| {
                    delete_previous_word_at(text, cursor);
                });
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.with_plan_review_text_mut(|text, cursor| {
                    truncate_at_cursor(text, *cursor);
                });
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.with_plan_review_text_mut(|text, cursor| {
                    delete_char_at_cursor(text, *cursor);
                });
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.with_plan_review_text_mut(|text, cursor| {
                    delete_char_before_cursor(text, cursor);
                });
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::META) =>
            {
                self.with_plan_review_text_mut(|text, cursor| {
                    insert_char_at_cursor(text, cursor, ch);
                });
            }
            _ => {}
        }
        self.dirty = true;
        false
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
        // A follow-up joins the plan that owns the node that reported it, never whichever
        // plan the user happens to be looking at.
        let plan_index = self.plan_index_for_node(node_id);
        let plan_id = self.plans[plan_index].id.clone();
        // Every node id known to that plan (queued + launched). Used to (a) skip a batch
        // whose finishing node is gone, and (b) validate explicit follow-up deps below.
        let known = self.known_plan_node_ids_in(plan_index);
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
                plan_id: plan_id.clone(),
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
                .filter(|r| {
                    matches!(
                        r.status,
                        AgentStatus::Failed | AgentStatus::Stopped | AgentStatus::Orphaned
                    )
                })
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
            self.push_plan_node(plan_index, node);
            // New work reopens THIS plan's gate; another plan's verdict is untouched.
            self.plans[plan_index].final_gate_status = FinalGateStatus::Idle;
            self.plans[plan_index].final_gate_summary = None;
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
        // Unique across the WHOLE fleet, not just this plan: `AgentRun.node_id` is the
        // join key the panes and ledgers use, so a second plan reusing an id would cross
        // their wires.
        let taken = |candidate: &str| -> bool {
            self.plans.iter().any(|plan| {
                plan.planned_nodes.iter().any(|node| node.id == candidate)
                    || plan.plan_launched_node_ids.contains(candidate)
            }) || self
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

    fn planned_nodes_from_fresh_tasks(&self, tasks: &[RudderPlanTask]) -> Vec<PlannedNode> {
        let mut taken: HashSet<String> = self
            .agents
            .iter()
            .filter_map(|run| run.node_id.clone())
            .collect();

        let mut id_map: HashMap<String, String> = HashMap::new();
        for task in tasks {
            let original = task.id.trim().to_string();
            let base = if original.is_empty() {
                "node".to_string()
            } else {
                original.clone()
            };
            let mut candidate = base.clone();
            let mut suffix = 2usize;
            while taken.contains(&candidate) {
                candidate = format!("{base}-{suffix}");
                suffix += 1;
            }
            taken.insert(candidate.clone());
            id_map.insert(original, candidate);
        }

        tasks
            .iter()
            .map(|task| {
                let mut task = task.clone();
                if let Some(id) = id_map.get(&task.id) {
                    task.id = id.clone();
                }
                for edge in &mut task.deps {
                    if let Some(id) = id_map.get(&edge.on) {
                        edge.on = id.clone();
                    }
                }
                PlannedNode::from_task(&task)
            })
            .collect()
    }

    /// APPROVE the pending plan: clear the approval gate and drain the queue into
    /// live workers. Ready nodes (hard deps satisfied, slot free) launch on this
    /// immediate scheduler pass; the rest stay in Todo until their deps merge.
    /// Approve the plan the UI is acting on (empty-Enter in its orchestrator pane).
    fn approve_planned_queue(&mut self) {
        let plan_index = self.active_plan_index();
        self.approve_planned_queue_for(plan_index);
    }

    fn approve_planned_queue_for(&mut self, plan_index: usize) {
        if !self.plans[plan_index].awaiting_approval {
            return;
        }
        // A refine is in flight: the revised DAG is still being produced. Do NOT
        // approve/launch the stale plan; tell the user to wait for the update.
        if self.plans[plan_index].refining {
            self.notice = Some("still refining — the updated plan is on its way".to_string());
            return;
        }
        if self.plans[plan_index].plan_review.dirty
            && !self.commit_plan_review_edits_for(plan_index)
        {
            return;
        }
        if self.plans[plan_index].planned_nodes.is_empty() {
            // Degenerate approval (e.g. a rebase diffed every task away): clear the
            // gate. Keep the interactive orchestrator alive so the user can keep
            // talking to the conductor even when there is nothing to launch.
            self.plans[plan_index].awaiting_approval = false;
            self.persist_plan_queue();
            self.plans[plan_index].plan_review = PlanReviewState::default();
            self.worker_view = WorkerView::Terminal;
            if let Err(error) = clear_orchestrator_plan_markers(&self.cwd) {
                self.notice = Some(format!("orchestrator marker cleanup warning: {error}"));
            }
            self.keep_orchestrator_after_approval();
            return;
        }
        self.plans[plan_index].awaiting_approval = false;
        self.plans[plan_index].planner_paused_for_input = false;
        self.plans[plan_index].pending_questions.clear();
        self.persist_plan_queue();
        self.plans[plan_index].plan_review = PlanReviewState::default();
        self.worker_view = WorkerView::Terminal;
        if let Err(error) = clear_orchestrator_plan_markers(&self.cwd) {
            self.notice = Some(format!("orchestrator marker cleanup warning: {error}"));
        }
        // Record the plan approval as a cross-cutting decision so the fleet has the
        // authoritative goal + shape in DECISIONS.md from the start.
        let goal = if self.plans[plan_index].plan_request.trim().is_empty() {
            self.plans[plan_index].planned_origin.clone()
        } else {
            self.plans[plan_index].plan_request.clone()
        };
        let titles: Vec<String> = self
            .plan()
            .planned_nodes
            .iter()
            .map(|n| n.title.clone())
            .collect();
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
        self.run_scheduler_for(plan_index);
        self.mirror_graph();
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
                || !run.interactive_orchestrator
                || run.status != AgentStatus::Running
            {
                continue;
            }
            run.needs_permission = false;
            run.needs_user_input = false;
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
    fn merged_node_ids_in(&self, plan_index: usize) -> Vec<String> {
        let mut ids = self.plans[plan_index].plan_merged_node_ids.clone();
        ids.extend(
            self.plan_agents(plan_index)
                .filter(|run| run.status == AgentStatus::Merged)
                .filter_map(|run| run.node_id.clone()),
        );
        ids.into_iter().collect()
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
        // plan order), reconstructed from their carrying agents. Only THIS plan's agents:
        // the pane renders one orchestrator's DAG, never a merge of every plan's.
        for run in self.active_plan_agents() {
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
        for node in &self.plan().planned_nodes {
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

        // Queued planned nodes across EVERY plan: not yet launched, status "planned".
        // The board sees the whole fleet; node ids are kept unique across plans at
        // adoption (`adopt_plan_nodes`), so the flat list cannot cross two plans' wires.
        for node in self.plans.iter().flat_map(|plan| plan.planned_nodes.iter()) {
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
                AgentStatus::Paused => "paused",
                AgentStatus::Orphaned => "orphaned",
                AgentStatus::Migrated => "cloud-owned",
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
            let mirror_started = Instant::now();
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
            let duration = mirror_started.elapsed();
            self.record_perf_duration("mirror_graph", duration);
            self.log_perf_duration_over(
                "mirror_graph",
                duration,
                SLOW_POLL_THRESHOLD,
                serde_json::json!({
                    "nodes": payload.get("nodes").and_then(|nodes| nodes.as_array()).map(|nodes| nodes.len()).unwrap_or(0),
                }),
            );
        }
    }

    /// Scheduler step: drain ready planned nodes into live agents while a slot is
    /// free. A node is ready when all its hard deps are merged (or reference ids
    /// absent from the plan, treated as satisfied so the DAG never deadlocks).
    /// Soft deps never gate. Runs on a coarse tick and after a plan is queued.
    /// Integrate every plan node that has finished cleanly (in
    /// review, not main, not already skipped for a conflict) so its hard children
    /// unblock. A recorded conflict itself prevents another attempt until it is
    /// resolved; integration stops at the first conflict to avoid stacking changes.
    /// After merging, drain the
    /// scheduler so newly-ready children launch.
    /// Reclaim disk from MERGED nodes' jj workspaces. Each agent runs in a full
    /// working-copy checkout under `.rudder-worktrees`; once a node is merged its
    /// code lives in main, so the checkout is pure redundancy. We keep it for a
    /// short grace window (default 1h, `RUDDER_WORKTREE_GC_GRACE_SECS`) so a
    /// just-merged node stays re-goalable, then this sweep forgets the workspace
    /// and removes the directory. Only MERGED, non-main jj runs whose checkout
    /// has been idle past the grace are touched; the merged row stays in the
    /// dashboard with its workspace path nulled. Capped per sweep so a large
    /// backlog (e.g. the piles already on disk) drains without a UI stall.
    fn gc_merged_workspaces(&mut self) {
        // See `merged_workspace_is_sweepable` for why a live pane is untouchable.
        const MAX_PER_SWEEP: usize = 3;
        let grace = Duration::from_secs(
            std::env::var("RUDDER_WORKTREE_GC_GRACE_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(3600),
        );
        let now = std::time::SystemTime::now();
        let repo = self.cwd.clone();
        let mut cleaned = 0usize;
        let mut swept: Vec<String> = Vec::new();
        for run in self.agents.iter_mut() {
            if cleaned >= MAX_PER_SWEEP {
                break;
            }
            let (Some(name), Some(path)) = (run.workspace_name.clone(), run.worktree_path.clone())
            else {
                continue;
            };
            if !merged_workspace_is_sweepable(run, now, grace) {
                continue;
            }
            if forget_jj_workspace(&repo, &name, &path).is_ok() {
                run.worktree_path = None;
                let _ = save_native_run_record(&repo, run);
                cleaned += 1;
                // Durable, not just a notice: "where did my workspace go?" has to
                // be answerable after the fact.
                swept.push(format!(
                    "swept merged workspace {} ({})",
                    path.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    run.id
                ));
            }
        }
        if cleaned > 0 {
            for entry in swept {
                self.push_activity(entry);
            }
            self.notice = Some(format!(
                "reclaimed {cleaned} merged workspace{} from disk",
                if cleaned == 1 { "" } else { "s" }
            ));
            self.dirty = true;
        }
    }

    /// Sweep ORPHANED jj workspaces — ones under `.rudder-worktrees` (or in jj's
    /// registry) that no live agent row owns, e.g. after rows were cleared or a
    /// plan was collapsed on reload. `gc_merged_workspaces` only reaches merged
    /// ROWS; without this, orphans pile up on disk (observed: 4.1 GB / 10 dirs).
    /// Safe by construction: it never touches a path owned by any current agent
    /// row, never touches `default` or a dir outside `.rudder-worktrees`, waits
    /// out the grace window, and skips while a resolver/rebase churns the op log.
    fn gc_orphan_workspaces(&mut self) {
        if self.plan().rebasing
            || self
                .agents
                .iter()
                .any(|run| run.merge_resolver && run.status == AgentStatus::Running)
        {
            return;
        }
        let grace = Duration::from_secs(
            std::env::var("RUDDER_WORKTREE_GC_GRACE_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(3600),
        );
        let now = std::time::SystemTime::now();
        let repo = self.cwd.clone();
        // Everything a live agent row owns is off-limits (its own checkout path,
        // or the main checkout for rows without a workspace).
        let live_paths: HashSet<PathBuf> = self
            .agents
            .iter()
            .map(|run| run.worktree_path.clone().unwrap_or_else(|| run.cwd.clone()))
            .collect();
        let live_names: HashSet<String> = self
            .agents
            .iter()
            .filter_map(|run| run.workspace_name.clone())
            .collect();

        // 1) Forget registry entries jj still lists that no live row owns.
        if let Ok(output) = Command::new("jj")
            .args(["workspace", "list"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::null())
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let name = line.split(':').next().unwrap_or("").trim();
                    if name.is_empty() || name == "default" || live_names.contains(name) {
                        continue;
                    }
                    let _ = Command::new("jj")
                        .args(["workspace", "forget", name])
                        .current_dir(&repo)
                        .stdin(std::process::Stdio::null())
                        .output();
                }
            }
        }

        // 2) Remove orphan directories under .rudder-worktrees/<group>/<node> that
        // no live row owns and that have sat idle past the grace window.
        let root = repo.join(".rudder-worktrees");
        let mut removed = 0usize;
        if let Ok(groups) = fs::read_dir(&root) {
            for group in groups.flatten() {
                let Ok(entries) = fs::read_dir(group.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() || live_paths.contains(&path) {
                        continue;
                    }
                    let idle = fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|mtime| now.duration_since(mtime).ok())
                        .map(|age| age >= grace)
                        .unwrap_or(false);
                    if idle && fs::remove_dir_all(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        if removed > 0 {
            self.notice = Some(format!(
                "reclaimed {removed} orphaned workspace{} from disk",
                if removed == 1 { "" } else { "s" }
            ));
            self.dirty = true;
        }
    }

    /// If per-node review is on and no reviewer is in flight, start one for a
    /// finished-but-unreviewed plan node. Serialized (one at a time) so a plan
    /// doesn't fan out N expensive review agents at once. The reviewer runs the
    /// thermonuclear skill in the node's OWN workspace and fixes findings in
    /// place; `integrate_ready_plan_nodes` won't merge the node until it's marked
    /// reviewed by `finalize_node_reviews`.
    fn maybe_start_node_review(&mut self) {
        if !self.node_review_enabled || !self.plan().node_reviewers.is_empty() {
            return;
        }
        let claimed = self.review_all_claimed_source_ids();
        let candidate = self.agents.iter().position(|run| {
            run.status == AgentStatus::Done
                && !run.is_main()
                && !run.merge_resolver
                && !run.has_merge_conflict()
                && run.workspace_name.is_some()
                && !claimed.contains(&run.id)
                && run
                    .node_id
                    .as_deref()
                    .is_some_and(|node_id| !self.plan().reviewed_nodes.contains(node_id))
        });
        if let Some(index) = candidate {
            self.spawn_node_reviewer(index);
        }
    }

    fn spawn_node_reviewer(&mut self, index: usize) {
        let (node_id, node_label, cwd) = {
            let Some(run) = self.agents.get(index) else {
                return;
            };
            let Some(node_id) = run.node_id.clone() else {
                return;
            };
            let label = if run.task_summary.trim().is_empty() {
                short_task(&run.task)
            } else {
                run.task_summary.trim().to_string()
            };
            (node_id, label, run.cwd.clone())
        };
        let (backend, model, effort) = self.review_agent_profile();
        let prompt = node_review_prompt(&node_label);
        let mut run = create_oneoff_agent(
            &self.cwd,
            backend,
            &model,
            Some(effort),
            &format!("thermonuclear review: {node_label}"),
        );
        // The reviewer runs in the NODE's workspace and edits its change in place;
        // node_id stays None so the scheduler never treats the reviewer as a plan
        // worker. It is transient — removed by finalize_node_reviews when done.
        run.cwd = cwd.clone();
        run.mode = AgentMode::ReviewAll;
        let reviewer_id = run.id.clone();
        let session_id = mint_session_id_for(backend);
        run.session_id = session_id.clone();
        let mut command = agent_command(
            backend,
            &model,
            Some(effort),
            &prompt,
            AgentMode::ReviewAll,
            session_id.as_deref(),
        );
        signals::augment_worker_command(&mut command, backend, AgentMode::ReviewAll, &reviewer_id);
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(cwd),
            ..TerminalPaneOptions::default()
        };
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
                run.status = AgentStatus::Running;
                run.last_output_at = Instant::now();
                self.plan_mut().node_reviewers.insert(reviewer_id, node_id);
                self.agents.push(run);
                self.notice = Some(format!(
                    "reviewing {node_label} (thermonuclear) before merge"
                ));
                self.dirty = true;
            }
            Err(error) => {
                // Fail open: never block the DAG on a reviewer that won't start.
                self.plan_mut().reviewed_nodes.insert(node_id);
                self.notice = Some(format!(
                    "review could not start ({error}); merging {node_label} unreviewed"
                ));
            }
        }
    }

    /// Mark nodes reviewed once their reviewer reaches a terminal state and drop
    /// the transient reviewer row. Fail open: a Failed/Stopped reviewer still
    /// marks the node reviewed, so a broken review never permanently blocks the
    /// node or its dependents.
    fn finalize_node_reviews(&mut self) {
        if self.plan().node_reviewers.is_empty() {
            return;
        }
        let tracked: Vec<(String, String)> = self
            .plan()
            .node_reviewers
            .iter()
            .map(|(reviewer_id, node_id)| (reviewer_id.clone(), node_id.clone()))
            .collect();
        let mut changed = false;
        for (reviewer_id, node_id) in tracked {
            let terminal = self.agents.iter().any(|run| {
                run.id == reviewer_id
                    && matches!(
                        run.status,
                        AgentStatus::Done | AgentStatus::Failed | AgentStatus::Stopped
                    )
            });
            if !terminal {
                continue;
            }
            self.plan_mut().node_reviewers.remove(&reviewer_id);
            self.plan_mut().reviewed_nodes.insert(node_id);
            if let Some(pos) = self.agents.iter().position(|run| run.id == reviewer_id) {
                let removed = self.agents.remove(pos);
                let _ = remove_native_run_record(&self.cwd, &removed.id);
                signals::cleanup_run_signals(&removed.id);
            }
            changed = true;
        }
        if changed {
            let last = self.agents.len().saturating_sub(1);
            self.selected_agent = self.selected_agent.min(last);
            self.agent_row_map.clear();
            self.dirty = true;
        }
    }

    fn integrate_ready_plan_nodes(&mut self) {
        // Hold integration steady for a plan whose structural rebase is in flight: merging
        // would shift a RUNNING node into the MERGED zone mid-diff and the build-forward
        // apply would compute its zones against a moving target. Asked per plan below,
        // so one plan's pivot does not freeze another plan's merges. Resume once that
        // rebase lands.

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
        // Per-node review gate: start a reviewer for a finished, unreviewed node.
        // Nodes only pass the merge predicate below once they're marked reviewed.
        self.maybe_start_node_review();
        let mut merged_labels: Vec<String> = Vec::new();
        let mut conflicted = false;
        loop {
            let next = self.agents.iter().position(|run| {
                run.node_id.is_some()
                    && run.status == AgentStatus::Done
                    && !run.is_main()
                    && !run.merge_resolver
                    && !run.has_merge_conflict()
                    && !(0..self.plans.len()).any(|index| {
                        self.plans[index].rebasing && self.run_belongs_to_plan(run, index)
                    })
                    && (!self.node_review_enabled
                        || run.node_id.as_deref().is_some_and(|node_id| {
                            self.plans[self.plan_index_for_node(node_id)]
                                .reviewed_nodes
                                .contains(node_id)
                        }))
            });
            let Some(index) = next else { break };
            let id = self.agents[index].id.clone();
            let label = self.agents[index].task_summary.clone();
            let task = self.agents[index].task.clone();
            match self.merge_agent_at(index) {
                Ok(()) => merged_labels.push(short_task(&label)),
                Err(error) => {
                    // Only a REAL conflict (jj reported conflicted files) gets the AI
                    // resolver. Every other failure — rudder CLI missing, 120s timeout,
                    // dirty integration target — used to be dressed up as a conflict
                    // with zero files, which spawned a resolver with nothing to resolve
                    // and stamped merge_conflict, excluding the node from this loop
                    // forever. Route those through the same terminal handling as a
                    // manual merge failure instead.
                    let files = self.pending_jj_conflict.clone().unwrap_or_default();
                    if files.is_empty() {
                        self.handle_merge_error(task, error, None, None, None, Some(id.clone()));
                        conflicted = true;
                        break; // stop integrating this tick; the notice explains why
                    }
                    // Conflict: auto-spawn the AI resolver to integrate both sides in
                    // the integration workspace. finalize_merge_resolvers flips the
                    // node to Merged (unblocking children) once it finishes clean.
                    // Clone (don't consume) the recorded conflict so it survives a
                    // resolver spawn failure for any later manual recovery.
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
                    [only] => format!("integrated {only} locally · dependents unblocked"),
                    many => format!(
                        "integrated {} nodes locally · dependents unblocked",
                        many.len()
                    ),
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
                match self.merge_agent_at(index) {
                    Ok(()) => {
                        finalized_any = true;
                        self.notice = Some(format!(
                            "resolved conflict and integrated {} locally",
                            short_task(&label)
                        ));
                    }
                    Err(error) => {
                        if let Some(run) = self.agents.get_mut(index) {
                            run.status = AgentStatus::Done;
                            run.merge_conflict = true;
                            run.had_merge_conflict = true;
                            run.last_error = Some(error.to_string());
                            let _ = save_native_run_record(&self.cwd, run);
                        }
                        self.notice = Some(format!(
                            "resolved files but integration failed for {}: {error}",
                            short_task(&label)
                        ));
                    }
                }
            } else {
                // Conflicts still remain. The durable merge_conflict state itself keeps
                // the integration scheduler from retrying until the user resolves it.
                if let Some(run) = self.agents.get_mut(index) {
                    run.status = AgentStatus::Done;
                    run.merge_conflict = true;
                    run.had_merge_conflict = true;
                    run.merge_conflict_operation = ConflictOperation::Merge;
                    run.merge_conflict_files = conflicts.clone();
                    run.last_error = Some(format!("resolver left {} conflict(s)", conflicts.len()));
                    let _ = save_native_run_record(&self.cwd, run);
                }
                self.notice = Some(format!(
                    "resolver left {} conflict(s) in {}; press m to retry or resolve manually",
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
                    self.plan()
                        .planned_nodes
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

    /// Dispatch every plan. Plans are independent — each has its own queue, ledgers and
    /// workspaces — so they are scheduled one after another and share only the global
    /// parallelism cap, which is a machine limit rather than a plan-level one.
    fn run_scheduler(&mut self) {
        for plan_index in 0..self.plans.len() {
            self.run_scheduler_for(plan_index);
        }
        // MIRROR the plans into graph.json so the board reflects these DAGs. Covers
        // both the just-launched nodes (now Running agents) and the queues that
        // remain in Todo. Coalesced + non-fatal inside mirror_graph.
        self.mirror_graph();
    }

    fn run_scheduler_for(&mut self, plan_index: usize) {
        if self.plans[plan_index].planned_nodes.is_empty() {
            return;
        }
        let cap = max_parallel();
        let planner_task = self.plans[plan_index].planned_origin.clone();
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
            let Some(position) = self.next_node_to_launch_in(plan_index, cap) else {
                break;
            };
            let node = self.plans[plan_index].planned_nodes.remove(position);
            // Persist the dequeue and at-most-once launch fact together BEFORE
            // spawning. A crash after the worker record is written can therefore
            // never resurrect this node from an older queue snapshot.
            self.plans[plan_index]
                .plan_launched_node_ids
                .insert(node.id.clone());
            self.persist_plan_queue();
            let title = node.title.clone();
            let depends_on = self.dependency_context(&node);
            let prompt = planned_node_worker_prompt(&planner_task, &node, &depends_on);
            self.start_execute_task_node(&prompt, Some(&title), Some(node));
            launched += 1;
        }

        if launched > 0 {
            let remaining = self.plans[plan_index].planned_nodes.len();
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
                    .plan()
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

        // The queue shrank as nodes launched; persist so a restart does not re-launch
        // an already-launched node or lose the remaining queue.
        self.persist_plan_queue();
    }

    /// Position in `planned_nodes` of the next node to launch, or `None` when the
    /// cap is reached or no queued node is ready. Pure decision (no side effects)
    /// so the scheduler's dep-gating + cap can be tested without spawning PTYs.
    /// Convenience wrapper over the plan the UI is acting on.
    #[cfg(test)]
    fn next_node_to_launch(&self, cap: usize) -> Option<usize> {
        self.next_node_to_launch_in(self.active_plan_index(), cap)
    }

    fn next_node_to_launch_in(&self, plan_index: usize, cap: usize) -> Option<usize> {
        if self.running_plan_agents() >= cap {
            return None;
        }
        let merged = self.merged_node_ids_in(plan_index);
        let plan_ids = self.known_plan_node_ids_in(plan_index);
        self.plans[plan_index]
            .planned_nodes
            .iter()
            .position(|node| {
                !self.plans[plan_index]
                    .plan_launched_node_ids
                    .contains(&node.id)
                    && !self
                        .agents
                        .iter()
                        .any(|run| run.node_id.as_deref() == Some(node.id.as_str()))
                    && node.is_ready(&merged, &plan_ids)
            })
    }

    /// Every node id that belongs to the active plan: ids still queued in
    /// `planned_nodes` PLUS ids of agents already launched from a node. A dep id
    /// outside this set was never part of the plan and is treated as satisfied so
    /// the DAG cannot deadlock; a dep still inside it must merge before unblocking.
    fn known_plan_node_ids_in(&self, plan_index: usize) -> Vec<String> {
        let mut ids: Vec<String> = self
            .plan()
            .planned_nodes
            .iter()
            .map(|node| node.id.clone())
            .collect();
        ids.extend(
            self.plan_agents(plan_index)
                .filter_map(|run| run.node_id.clone()),
        );
        ids.extend(
            self.plans[plan_index]
                .plan_launched_node_ids
                .iter()
                .cloned(),
        );
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
        if let Some(review) = run.review_terminal.as_mut() {
            if review.is_alive() {
                return;
            }
            // The watch loop died (workspace deleted, jj crash): keep the last
            // frame and a frozen pane was all the user got. Respawn instead.
            review.terminate_and_wait();
            run.review_terminal = None;
        }
        // A once-failed pane must not pin its error forever; each open retries.
        run.review_error = None;

        #[cfg(test)]
        {
            run.review_error = None;
            self.notice = Some("opening review".to_string());
            return;
        }

        #[cfg(not(test))]
        {
            // A cloud agent's cwd is the LOCAL repo root — its real workspace
            // lives on the remote worker and there is no diff fetch-back yet.
            // Watching the local checkout here would confidently show the
            // user's own edits as the worker's. Be honest instead.
            if render::is_cloud_agent(run) {
                run.review_error = Some(
                    "cloud worker: its diff lives on the remote workspace (local diff view \
                     would show YOUR checkout) · use `rudder cloud logs <id>` or merge-back"
                        .to_string(),
                );
                return;
            }
            // The workspace can be gone (merged + GC'd): spawning sh in a deleted
            // cwd yields a bare ENOENT that reads like a Rudder bug. Say what
            // actually happened instead.
            if !run.cwd.exists() {
                run.review_error = Some(
                    "workspace was reclaimed (merged work is cleaned up); nothing left to diff"
                        .to_string(),
                );
                return;
            }
            // The review pane is jj's own diff, watched live: `jj status` (the
            // working-copy summary) + `jj diff` (jj's default diff program). The
            // agent works in a jj workspace, so this is the faithful view of its
            // changes. Falls back to `git diff` only if jj is somehow unavailable.
            //
            // The loop only repaints when the capture CHANGED: an unconditional
            // clear+reprint every 2s shifted the scroll position by a whole diff
            // length per tick (scrollback is measured from the bottom), making any
            // >1-screen diff unreadable while an idle pane still churned CPU.
            // A stale working copy (siblings snapshotting concurrently) recovers
            // via `jj workspace update-stale` instead of erroring forever.
            //
            // CRITICAL: pipe jj through `cat` (and use git `--no-pager`). The pane is
            // a real PTY, so jj/git see a tty on stdout and would launch a PAGER on a
            // long diff, blocking the watch loop forever. Piping to cat makes stdout a
            // pipe, so jj's auto-pager stays off and the loop keeps refreshing.
            let command = TerminalCommand::with_args(
                "sh",
                [
                    "-lc",
                    "if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then snap() { jj --color=always status 2>&1 | cat; printf '\\n'; jj --color=always diff 2>&1 | cat; }; else snap() { git --no-pager status --short 2>&1; printf '\\n'; git --no-pager diff --color=always HEAD 2>&1; }; fi; prev='__rudder_unrendered__'; while :; do cur=$(snap); case \"$cur\" in *'working copy is stale'*) jj workspace update-stale >/dev/null 2>&1; cur=$(snap);; esac; if [ \"$cur\" != \"$prev\" ]; then printf '\\033[2J\\033[H'; printf '%s\\n' \"$cur\"; prev=$cur; fi; sleep 2; done",
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
                    Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
        let Some(selected) = self.agents.get(self.selected_agent) else {
            return;
        };
        // A LIVE main row used to dead-end here: the refusal said "stop it with
        // x", but the x handler excludes main rows, so a running main agent
        // could be neither stopped nor deleted — dd just re-printed advice that
        // could not be followed. Deleting is already a confirmed two-press flow,
        // so fold the stop into it: say the second press will stop the agent,
        // and on confirm actually do so before deleting.
        let live_main = self.selected_is_main()
            && selected.terminal.is_some()
            && selected.status == AgentStatus::Running;
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
            self.notice = Some(if live_main {
                format!("{label} is still running — press d again to stop it and delete the row · any other key cancels")
            } else if selected.worktree_path.is_some() {
                format!("delete {label} and remove its worktree? press d again to confirm · any other key cancels")
            } else {
                format!("delete {label}? press d again to confirm · any other key cancels")
            });
            return;
        }
        if live_main {
            let idx = self.selected_agent;
            // If the stop fails, do NOT delete: removing the row while its PTY
            // lives would leak the process with nothing left pointing at it.
            if !self.stop_agent_at(idx) {
                self.delete_pending = None;
                self.notice =
                    Some("could not stop the main agent — row not deleted".to_string());
                return;
            }
        }

        let run = self.agents.remove(self.selected_agent);
        // Indices shifted under the rendered rows; drop the click map so a same-tick
        // click resolves to nothing instead of a neighbor (next render rebuilds it).
        self.agent_row_map.clear();
        let _ = remove_native_run_record(&self.cwd, &run.id);
        // A deleted launched node must leave the plan's id set: its id can never
        // reach Merged, so keeping it in plan_launched_node_ids made every hard
        // dependent permanently un-ready (known_plan_node_ids still contained
        // the id) with no notice. Dropping it turns the dep into a dangling id,
        // which is_ready treats as satisfied — the user removed the node, so
        // its dependents proceed.
        if let Some(node_id) = run.node_id.as_deref() {
            let plan_index = self.plan_index_for_node(node_id);
            if !self.plans[plan_index]
                .plan_merged_node_ids
                .contains(node_id)
                && self.plans[plan_index]
                    .plan_launched_node_ids
                    .remove(node_id)
            {
                self.persist_plan_queue();
            }
        }
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
            if run.workspace_name.is_some() {
                // jj workspace: forget the registry entry AND remove the checkout.
                // `git worktree remove` (used here before) fails on a jj workspace
                // and leaked both the directory and jj's workspace list.
                forget_jj_workspace(&self.cwd, run.workspace_name.as_deref().unwrap_or(""), path)
                    .err()
                    .map(|error| format!("failed to remove jj workspace: {error}"))
            } else {
                // Legacy git-worktree run.
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
        let _ = self.write_rudder_context_timed(None);
    }

    /// `b` in the agents pane: BRANCH the selected agent's chat. The backend forks
    /// the conversation (Claude: `--resume <sid> --fork-session`; Codex: `codex
    /// fork <sid>`) into a brand-new session, so the original chat is left exactly
    /// where it was, and the fork opens as a NEW agent row in its own jj workspace
    /// seeded from the original's current change — the forked conversation's
    /// memory of "the edits so far" matches the files it sees. The user then types
    /// the new direction into the forked pane.
    fn branch_selected_agent(&mut self) {
        let Some(run) = self.agents.get(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        // The orchestrator owns the plan and cannot be forked into a worker, but a
        // MAIN or one-off row is just a conversation in the checkout — forking it
        // into an isolated, mergeable worker is one of the more useful things you
        // can do with it, and refusing was arbitrary.
        if run.is_orchestrator() {
            self.notice = Some("branch works on agents, not the orchestrator".to_string());
            return;
        }
        let backend_for_notice = run.backend;
        // Read the "has this pane said anything" state BEFORE the session lookup
        // below, not after. That lookup walks the backend's whole session tree and
        // can take minutes on a machine with thousands of rollouts, and the check
        // is an elapsed-time one — measuring it afterwards folds the scan's own
        // duration into the answer and reports "never started a conversation"
        // about a pane that was talking when the key was pressed.
        let never_spoke = self.agents.get(self.selected_agent).is_some_and(|run| {
            run.turns.is_empty() || run.last_output_at.elapsed() > Duration::from_secs(600)
        });
        // Codex and opencode runs only learn their session id later; for a live
        // branch, fall back to the newest session recorded for that workspace.
        let session_id = run
            .session_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| match run.backend {
                Backend::Codex => latest_codex_session_id_for_cwd(&run.cwd),
                // opencode records its sessions in a database and ships the query.
                Backend::Opencode => recent_opencode_conversations(&run.cwd, 1)
                    .into_iter()
                    .next()
                    .map(|candidate| candidate.session_id),
                Backend::Claude => None,
            });
        let Some(session_id) = session_id else {
            // Say WHY. Claude gets a session id at launch (Rudder mints it), but
            // Codex and opencode only record one once the conversation exists, and
            // Rudder finds it by the directory the session ran in.
            // Say which of the two states this is. "No session yet" almost always
            // means the pane never got past its first prompt (a codex fork asking
            // which directory to use, an auth prompt), and the row still reads as
            // running — so point at the pane rather than at the backend.
            self.notice = Some(match (backend_for_notice, never_spoke) {
                (Backend::Claude, _) => {
                    "nothing to branch: this agent has no session yet".to_string()
                }
                (other, true) => format!(
                    "nothing to branch: this {} pane never started a conversation — open it (Enter) and check whether it is waiting on a prompt",
                    other.as_str()
                ),
                (other, false) => format!(
                    "no {} session recorded for this workspace yet — it is written once the conversation starts; branch after its first reply",
                    other.as_str()
                ),
            });
            return;
        };

        let source_task = run.task.clone();
        let source_summary = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        let backend = run.backend;
        let model = run.model.clone();
        let effort = run.effort;
        let base_change = run.jj_change_id.clone();
        let source_cwd = run.cwd.clone();
        // Claude resolves --resume against the CURRENT directory's transcripts; make
        // sure the source session's transcript actually exists before creating a
        // workspace we'd abandon on failure. (Codex sessions are global.)
        if backend == Backend::Claude && claude_transcript_path(&source_cwd, &session_id).is_none()
        {
            self.notice = Some("branch failed: no session transcript found to branch".to_string());
            return;
        }

        let label = format!("branch: {source_summary}");
        match self.spawn_forked_conversation(ForkedConversation {
            backend,
            model,
            effort,
            session_id,
            source_cwd,
            // Seed the fork's workspace from the source's CURRENT jj change (jj snapshots
            // the source working copy in the process), so files match the forked memory.
            base_change,
            label,
            task: format!("Branch of: {source_task}"),
            seed: None,
        }) {
            Ok(ForkOutcome::Started) => {
                self.notice = Some(format!(
                    "branched {source_summary}; type the new direction in the forked pane"
                ));
            }
            Ok(ForkOutcome::SpawnFailed(error)) => {
                self.notice = Some(format!("branch failed to start: {error}"));
            }
            Err(error) => {
                self.notice = Some(format!("branch failed: {error}"));
            }
        }
        self.delete_pending = None;
    }

    /// Fork a CLI conversation into a NEW jj workspace and open it as a worker row.
    /// Shared by `b` (branch a live agent's chat) and `/handoff` (adopt a chat that
    /// was happening OUTSIDE the dashboard). Always a fork, never a plain resume:
    /// the source conversation may still be open in another process, and two
    /// writers on one transcript corrupt it.
    fn spawn_forked_conversation(
        &mut self,
        request: ForkedConversation,
    ) -> std::result::Result<ForkOutcome, String> {
        let ForkedConversation {
            backend,
            model,
            effort,
            session_id,
            source_cwd,
            base_change,
            label,
            task,
            seed,
        } = request;
        // Claude resolves --resume against the CURRENT directory's transcripts; make
        // sure the source session's transcript actually exists before creating a
        // workspace we'd abandon on failure. (Codex sessions are global.)
        if backend == Backend::Claude && claude_transcript_path(&source_cwd, &session_id).is_none()
        {
            return Err("no session transcript found to fork".to_string());
        }
        let worktree = prepare_jj_workspace_at(&self.cwd, &label, base_change.as_deref())
            .map_err(|error| error.to_string())?;
        if let Err(error) = self.write_rudder_context_timed(Some(&worktree)) {
            self.notice = Some(format!("context warning: {error}"));
        }
        // Stage the source transcript into the fork workspace's project folder so
        // `--resume <sid> --fork-session` can find the conversation from its new cwd.
        if backend == Backend::Claude {
            stage_claude_session_for_cwd(&source_cwd, &session_id, &worktree.path)?;
        }
        // A seeded fork wakes up in a DIFFERENT directory than the conversation it
        // remembers; say so before the instruction so it re-reads instead of trusting
        // its memory of the other checkout.
        let seed = seed
            .map(|text| worker_orientation(&worktree.path, &text))
            .filter(|text| !text.trim().is_empty());
        let mut command = match backend {
            Backend::Claude => claude_fork_command(&model, effort, &session_id, seed.as_deref()),
            Backend::Codex => codex_fork_command(&model, effort, &session_id, seed.as_deref()),
            Backend::Opencode => opencode_fork_command(&model, &session_id, seed.as_deref()),
        };
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
            task,
            task_summary: truncate_chars(&label, 56),
            current_prompt: seed.clone().unwrap_or_default(),
            turns: seed
                .clone()
                .map(|prompt| {
                    vec![AgentTurn {
                        ts: created_at.clone(),
                        prompt,
                        source: "user".to_string(),
                    }]
                })
                .unwrap_or_default(),
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
            integration: IntegrationEvidence::default(),
            publish: PublishEvidence::default(),
            delivery: DeliveryEvidence::default(),
            // The fork mints its own fresh session id (Claude --fork-session prints
            // it; Codex records it under the fork's cwd, discovered at completion).
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
        };

        let mut outcome = ForkOutcome::Started;
        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                run.terminal = Some(terminal);
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                outcome = ForkOutcome::SpawnFailed(error.to_string());
            }
        }

        let spawned = run.terminal.is_some();
        self.agents.push(run);
        self.selected_agent = self.agents.len().saturating_sub(1);
        self.focus = if spawned {
            FocusPane::Worker
        } else {
            FocusPane::Task
        };
        self.worker_view = WorkerView::Terminal;
        if let Some(run) = self.agents.get(self.selected_agent) {
            let _ = save_native_run_record(&self.cwd, run);
        }
        let _ = self.write_rudder_context_timed(None);
        self.dirty = true;
        Ok(outcome)
    }

    /// `c` in the agents pane: clear every MERGED agent from the list in one
    /// action, so finished work stops occupying the pane. Same press-twice
    /// confirm contract as `d`, reusing `delete_pending` (with a sentinel that
    /// can never collide with a run id) so any other key cancels it exactly
    /// like a pending delete. Merged work is already integrated, so removing
    /// the records and leftover workspaces loses nothing undoable.
    fn clear_merged_agents(&mut self) {
        let merged: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| {
                agent.status == AgentStatus::Merged && !agent.is_main() && !agent.is_orchestrator()
            })
            .map(|(index, _)| index)
            .collect();
        if merged.is_empty() {
            self.delete_pending = None;
            self.notice = Some("no merged agents to clear".to_string());
            return;
        }
        if self.delete_pending.as_deref() != Some(CLEAR_MERGED_PENDING) {
            self.delete_pending = Some(CLEAR_MERGED_PENDING.to_string());
            self.notice = Some(format!(
                "clear {} merged agent(s) from the list? press c again to confirm · any other key cancels",
                merged.len()
            ));
            return;
        }

        let mut persist_ledger = false;
        let mut cleared = 0usize;
        for index in merged.into_iter().rev() {
            let run = self.agents.remove(index);
            let _ = remove_native_run_record(&self.cwd, &run.id);
            signals::cleanup_run_signals(&run.id);
            persist_ledger |= self.followups_ingested.remove(&run.id);
            persist_ledger |= self.completion_summary_pending.remove(&run.id);
            if let Some(path) = run.worktree_path.as_ref() {
                // Merged workspaces are integrated leftovers; best-effort removal,
                // same command the single-delete path uses.
                let _ = Command::new("git")
                    .args(["worktree", "remove", "--force"])
                    .arg(path)
                    .current_dir(&self.cwd)
                    .output();
            }
            cleared += 1;
        }
        if persist_ledger {
            self.persist_ingested_runs();
        }
        // Indices shifted under the rendered rows; drop the click map so a
        // same-tick click resolves to nothing instead of a neighbor.
        self.agent_row_map.clear();
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
        self.notice = Some(format!("cleared {cleared} merged agent(s)"));
        let _ = self.write_rudder_context_timed(None);
    }

    fn start_stored_merge_conflict_resolution(&mut self, index: usize) {
        let Some(run) = self.agents.get(index) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        if !run.has_merge_conflict() {
            self.notice = Some("selected agent is not in a merge conflict".to_string());
            return;
        }

        let operation = run.merge_conflict_operation;
        let repo_root = if operation == ConflictOperation::Rebase {
            run.worktree_path.clone().unwrap_or_else(|| run.cwd.clone())
        } else {
            self.cwd.clone()
        };
        let mut files = run.merge_conflict_files.clone();
        if files.is_empty() {
            files = if operation == ConflictOperation::Rebase {
                conflicted_files(&repo_root)
            } else {
                jj_unresolved_conflicts(&repo_root)
            };
        }
        let task = run
            .task
            .strip_prefix("Resolve merge conflicts: ")
            .or_else(|| run.task.strip_prefix("Resolve rebase conflicts: "))
            .unwrap_or(&run.task)
            .to_string();
        self.conflict_prompt = Some(MergeConflictPrompt {
            operation,
            task,
            conflicted_files: files,
            error: run
                .last_error
                .clone()
                .unwrap_or_else(|| "merge conflict is still unresolved".to_string()),
            repo_root,
            target_branch: current_branch_at(&self.cwd),
            source_branch: run.worktree_branch.clone(),
            worktree_path: run.worktree_path.clone(),
            agent_id: Some(run.id.clone()),
        });
        self.merge_confirm = None;
        self.delete_pending = None;
        self.notice = None;
        self.start_conflict_resolution_agent();
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
        if run.status == AgentStatus::Running {
            self.notice =
                Some("agent is still running; stop it with x or wait before merging".to_string());
            return;
        }
        if run.has_merge_conflict() {
            self.start_stored_merge_conflict_resolution(self.selected_agent);
            return;
        }
        if !run.has_merge_source() {
            self.notice = Some("selected agent has no workspace to merge".to_string());
            return;
        }
        // Reaching for merge at all means the armed `dd` is not what you want, so it
        // is disarmed before the gate rather than after it. Otherwise pressing `m` on
        // unreviewed work left a delete primed behind the diff you were reading.
        self.delete_pending = None;
        // THE FORK IN THE ROAD. Where publishing can work, this row's route to main
        // is a pull request and NOT a local merge; where it cannot, it is the local
        // merge below. Exactly one of the two runs for a given row, ever — two roads
        // would mean the same work landing twice by two mechanisms with no single
        // place to look for "did this ship?".
        match self.merge_route_for(self.selected_agent) {
            // Rudder has never pushed to a remote, so the first publish in a repo
            // says exactly what it is about to do, to which remote and on which
            // branch. Once accepted, this repo never asks again.
            MergeRoute::ConfirmFirstPublish => {
                let detail = self.publish_first_time_detail(self.selected_agent);
                self.merge_confirm = Some(MergeConfirmation {
                    intent: MergeIntent::Publish {
                        id: run.id.clone(),
                        task: run.task.clone(),
                    },
                    detail,
                });
                self.conflict_prompt = None;
                self.notice = None;
                return;
            }
            MergeRoute::Publish => {
                let index = self.selected_agent;
                self.publish_agent_at(index);
                return;
            }
            MergeRoute::Checking => {
                // One road or the other, but not a guess. The probe resolves within
                // a second or two of launch, so this is a wait, not a wall.
                self.notice =
                    Some("checking whether this repo publishes · press m again".to_string());
                self.dirty = true;
                return;
            }
            MergeRoute::LocalMerge => {}
        }
        // THE GATE, and it belongs to the LOCAL road only.
        //
        // Nothing merges into the user's checkout that they have not been shown, and
        // `m` is the key that shows it: unread work opens its diff, a second `m`
        // merges. On the publishing road there is no gate, because the pull request
        // IS the review surface — a draft PR requests no review and the diff gets
        // read there. Gating both roads meant reading the same change twice for one
        // landing, and a review you are made to do twice is one you learn to skim.
        if run.reviewed_at.is_none() {
            self.worker_view = WorkerView::Terminal;
            self.toggle_worker_view();
            self.notice =
                Some("read the diff, then press m again to merge · Esc goes back".to_string());
            return;
        }
        // Merges run with --allow-dirty: uncommitted edits in the main checkout
        // become part of the merge parent. Surprising enough to warn about in
        // the modal instead of discovering it in the merge commit.
        let dirty_warning = diff_short_summary_at(&self.cwd)
            .map(|stat| format!("your uncommitted local changes merge too ({stat})"));
        self.merge_confirm = Some(MergeConfirmation {
            intent: MergeIntent::Selected {
                id: run.id.clone(),
                task: run.task.clone(),
            },
            detail: dirty_warning,
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
                    && run.has_merge_source()
                    && !run.is_main()
                    && !run.is_oneoff()
                    && !run.is_orchestrator()
                    && !run.has_merge_conflict()
                    && !claimed.contains(&run.id)
            })
            .collect();

        if ready_runs.is_empty() {
            let conflict_count = self
                .agents
                .iter()
                .filter(|run| {
                    run.status == AgentStatus::Done
                        && run.has_merge_conflict()
                        && !run.is_main()
                        && !run.is_oneoff()
                        && !run.is_orchestrator()
                        && !claimed.contains(&run.id)
                })
                .count();
            self.notice = Some(if conflict_count > 0 {
                format!(
                    "{conflict_count} merge-conflict row{} need selected m before merge-all can continue",
                    if conflict_count == 1 { "" } else { "s" }
                )
            } else {
                "no completed workspaces ready to merge".to_string()
            });
            return;
        }

        let ids: Vec<String> = ready_runs.iter().map(|r| r.id.clone()).collect();
        let count = ids.len();
        let labels = merge_all_labels(&ready_runs);
        let detail = format!("Ready: {}", summarize_labels(&labels, 6));

        let _ = count;
        self.delete_pending = None;
        self.merge_confirm = Some(MergeConfirmation {
            intent: MergeIntent::All { ids },
            detail: Some(detail),
        });
        self.conflict_prompt = None;
        self.record_activity(format!(
            "merge-all ready: {count} worktree{} ({})",
            if count == 1 { "" } else { "s" },
            summarize_labels(&labels, 8)
        ));
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
                _ => {
                    // The modal captures EVERY key; a silently-eaten keystroke
                    // reads as a frozen dashboard. Say why nothing happened.
                    self.notice =
                        Some("merge prompt open — y merges · n or Esc cancels".to_string());
                    self.dirty = true;
                }
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
                        "resolve the jj conflicts manually, then press m to finalize".to_string()
                    });
                }
                _ => {
                    self.notice = Some(
                        "conflict prompt open — y starts the AI resolver · n dismisses".to_string(),
                    );
                    self.dirty = true;
                }
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
            MergeIntent::Publish { id, .. } => {
                let Some(index) = self.agents.iter().position(|run| run.id == id) else {
                    self.notice = Some("selected agent no longer exists".to_string());
                    return;
                };
                self.delete_pending = None;
                // `publish_agent_at` records the acceptance, so this repo goes
                // straight to publishing from here on.
                self.publish_agent_at(index);
            }
            MergeIntent::All { ids } => {
                let total = ids.len();
                let mut merged = 0;
                let labels = ids
                    .iter()
                    .filter_map(|id| {
                        self.agents
                            .iter()
                            .find(|run| run.id == *id)
                            .map(merge_label_for_run)
                    })
                    .collect::<Vec<_>>();
                self.push_activity(format!(
                    "merge-all started: {total} worktree{} ({})",
                    if total == 1 { "" } else { "s" },
                    summarize_labels(&labels, 8)
                ));
                for (position, id) in ids.iter().enumerate() {
                    let Some(index) = self.agents.iter().position(|run| run.id == *id) else {
                        continue;
                    };
                    // The ids were snapshotted at request time; auto-integration
                    // may have landed some meanwhile. Counting those as "merged
                    // by merge-all" over-reported the batch.
                    if self.agents[index].status == AgentStatus::Merged {
                        continue;
                    }
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
                            let message = format!(
                                "merged {merged}/{total} · {notice}{}",
                                if remaining > 0 {
                                    format!(" · {remaining} more wait in review")
                                } else {
                                    String::new()
                                }
                            );
                            self.push_activity(format!("merge-all stopped: {message}"));
                            self.notice = Some(message);
                        }
                        return;
                    }
                    merged += 1;
                }
                self.delete_pending = None;
                self.push_activity(format!(
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

    /// Merge outcomes are the sharpest product signal Rudder has: work that
    /// reaches "done" and never merges is the failure mode users complain about.
    fn emit_merge_finished(&mut self, ok: bool, conflict: bool, planned: bool) {
        telemetry::emit_event(
            "merge_finished",
            serde_json::json!({
                "ok": ok,
                "conflict": conflict,
                "planned": planned,
                "backend": self.backend.as_str(),
            }),
        );
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
        // those; the git query remains only for rebase conflict reporting.
        let jj_conflicts = self.pending_jj_conflict.take();
        let mut conflicts = jj_conflicts.unwrap_or_else(|| conflicted_files(&self.cwd));
        self.emit_merge_finished(false, !conflicts.is_empty(), agent_id.is_none());
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
                    // A failed MERGE must not kill a live agent: the work is
                    // still in progress, only the (refused) integration failed.
                    if run.status != AgentStatus::Running {
                        run.status = AgentStatus::Failed;
                    }
                    run.last_error = Some(error.to_string());
                    let _ = save_native_run_record(&self.cwd, run);
                }
            }
            return;
        }

        let count = conflicts.len();
        let target_branch = current_branch_at(&self.cwd);
        let conflict_files = conflicts.clone();
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
                // Stopped buried it in "closed" looking cancelled. The explicit
                // conflict state prevents automatic integration from retrying it.
                run.status = AgentStatus::Done;
                run.last_error = Some(error.to_string());
                run.merge_resolver = false;
                run.merge_conflict = true;
                run.had_merge_conflict = true;
                run.merge_conflict_operation = operation;
                run.merge_conflict_files = conflict_files;
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        let operation_label = if operation == ConflictOperation::Rebase {
            "rebase"
        } else {
            "merge"
        };
        let message = format!(
            "{operation_label} conflict in {} ({count} file{}): press y to let AI resolve & complete the merge, or n to do it manually",
            short_task(&self.conflict_prompt.as_ref().map(|p| p.task.clone()).unwrap_or_default()),
            if count == 1 { "" } else { "s" }
        );
        self.notice = Some(message.clone());
        self.record_activity(message);
    }

    /// STEERING: re-goal a running/finished worker by RESUMING its session (so it
    /// keeps its memory and its jj-workspace edits) and delivering a new objective as
    /// the next turn. Falls back to a fresh, context-carrying spawn when there is no
    /// resumable session. The conflict resolver (below) is the same re-task-in-place
    /// pattern; this differs by resuming instead of minting fresh. Returns true on
    /// success. Never re-goals the main agent or the orchestrator.
    // Wired by the autonomous drift-fix (2c) and conductor chat routing (2d); the
    // stop primitive below already has a keybinding.
    fn regoal_agent_at(&mut self, index: usize, new_goal: &str) -> bool {
        self.restart_agent_at(index, new_goal, None)
    }

    fn retarget_agent_at(
        &mut self,
        index: usize,
        backend: Backend,
        model: &str,
        effort: Option<EffortLevel>,
        new_goal: &str,
    ) -> bool {
        self.restart_agent_at(index, new_goal, Some((backend, model.to_string(), effort)))
    }

    fn restart_agent_at(
        &mut self,
        index: usize,
        new_goal: &str,
        target: Option<(Backend, String, Option<EffortLevel>)>,
    ) -> bool {
        let cwd_default = self.cwd.clone();
        let retargeted = target.is_some();
        if let Some((backend, model, effort)) = target {
            let Some(run) = self.agents.get_mut(index) else {
                return false;
            };
            if run.is_main() || run.is_orchestrator() {
                return false;
            }
            let provider_changed = run.backend != backend;
            run.backend = backend;
            run.model = model;
            run.effort = effort;
            if provider_changed {
                // Backend session ids are provider-specific. Keep the workspace and
                // start a context-carrying session on the requested provider.
                run.session_id = None;
            } else if backend == Backend::Codex
                && run
                    .session_id
                    .as_deref()
                    .is_none_or(|session| session.trim().is_empty())
            {
                // Interactive Codex runs discover their id from the rollout log. A
                // STOP immediately followed by RESUME can happen before normal
                // completion captures it, so recover it here to preserve the chat.
                run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
            }
        }
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
            let report_id = node_id.as_deref().unwrap_or(&id);
            let _ = std::fs::remove_file(worker_done_file(&run_cwd, report_id));
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
                    Backend::Opencode => opencode_resume_command(run, sid),
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
            let report_id = run.node_id.as_deref().unwrap_or(&run.id);
            command = command.with_env(
                "RUDDER_DONE_FILE",
                worker_done_file(&run.cwd, report_id)
                    .to_string_lossy()
                    .to_string(),
            );
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
                if let Some(text) = &deliver_after {
                    let _ = terminal.write_input(format!("{text}\r").as_bytes());
                }
                let now = now_stamp();
                if let Some(run) = self.agents.get_mut(index) {
                    let starting_new_turn = run.status == AgentStatus::Done;
                    run.terminal = Some(terminal);
                    run.status = AgentStatus::Running;
                    run.session_id = new_session;
                    run.completed_at = None;
                    run.last_output_at = Instant::now();
                    run.needs_permission = false;
                    run.needs_user_input = false;
                    run.last_error = None;
                    run.merge_resolver = false;
                    run.merge_conflict = false;
                    run.merge_conflict_files.clear();
                    run.done_summary = None;
                    let requested_delivery = DeliveryEvidence::for_task(new_goal);
                    if requested_delivery.required || !run.delivery.required || starting_new_turn {
                        run.delivery = requested_delivery;
                    }
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
                if retargeted {
                    let backend_label = self.agents[index].backend.as_str().to_string();
                    let model_label = self.agents[index].model.clone();
                    self.push_activity(format!(
                        "resumed {node_label} on {} {}: {}",
                        backend_label,
                        model_label,
                        short_task(new_goal)
                    ));
                } else {
                    self.push_activity(format!("re-goaled {node_label}: {}", short_task(new_goal)));
                }
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
        let run_id = match self.agents.get(index) {
            Some(run) => run.id.clone(),
            None => return false,
        };
        let result = match self
            .agents
            .get_mut(index)
            .and_then(|run| run.terminal.as_mut())
        {
            Some(terminal) => {
                terminal.reset_scrollback();
                // Text only; the submitting CR is deferred (see queue_enter_for).
                terminal.write_input(&bracketed_paste_bytes(text))
            }
            None => return false,
        };
        if result.is_ok() {
            if let Some(run) = self.agents.get_mut(index) {
                run.last_worker_input_at = Some(Instant::now());
            }
            self.queue_enter_for(&run_id);
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
            // Main rows are stoppable: the body below only kills the PTY and
            // records Stopped — nothing here touches a worktree. The old
            // is_main() refusal here was the bottom of a three-layer dead end
            // (dd said "stop it with x", x excluded main, and this guard made
            // even a permitted call a silent no-op).
            let was_conflict_resolver = run.merge_resolver || run.merge_conflict;
            let unresolved = if was_conflict_resolver {
                jj_unresolved_conflicts(&run.cwd)
            } else {
                Vec::new()
            };
            if let Some(terminal) = run.terminal.as_mut() {
                terminal.terminate_and_wait();
            }
            run.terminal = None;
            run.review_terminal = None;
            // User cancellation is absorbing. Integration conflict remains a
            // separate durable fact, so a stopped resolver can still be resumed
            // without lying that the process completed successfully.
            run.status = AgentStatus::Stopped;
            run.completed_at = Some(Instant::now());
            run.merge_resolver = false;
            if was_conflict_resolver {
                run.merge_conflict = true;
                run.had_merge_conflict = true;
                if !unresolved.is_empty() {
                    run.merge_conflict_files = unresolved;
                }
            }
            run.needs_permission = false;
            run.needs_user_input = false;
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

    fn consume_stop_requests(&mut self) {
        let requested: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                run.status == AgentStatus::Running && stop_requested(&self.cwd, &run.id)
            })
            .map(|(index, _)| index)
            .collect();
        for index in requested {
            let run_id = self.agents[index].id.clone();
            self.stop_agent_at(index);
            clear_stop_request(&self.cwd, &run_id);
        }
        for run in &self.agents {
            if run.status != AgentStatus::Running && stop_requested(&self.cwd, &run.id) {
                clear_stop_request(&self.cwd, &run.id);
            }
        }
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
                p.conflicted_files.clone(),
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
        let (operation, resolver_cwd, source_branch, worktree_path, conflicted_files) =
            conflict_context.unwrap_or((
                ConflictOperation::Merge,
                self.cwd.clone(),
                None,
                None,
                Vec::new(),
            ));
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
                Self::attach_output_waker(&self.pty_output_waker, &terminal);
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
                    run.needs_user_input = false;
                    run.last_error = None;
                    // Track jj merge resolvers so the poll loop finalizes the merge
                    // (flip the node to Merged, unblock children) once they finish
                    // with no conflicts left. Git rebase resolvers keep manual flow.
                    run.merge_resolver = operation == ConflictOperation::Merge;
                    run.integration.phase = if operation == ConflictOperation::Merge {
                        IntegrationPhase::Resolving
                    } else {
                        IntegrationPhase::Pending
                    };
                    run.merge_conflict = true;
                    run.had_merge_conflict = true;
                    run.merge_conflict_operation = operation;
                    run.merge_conflict_files = conflicted_files;
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
                let _ = self.write_rudder_context_timed(None);
            }
            Err(error) => {
                if let Some(run) = self.agents.get_mut(index) {
                    run.status = AgentStatus::Done;
                    run.merge_resolver = false;
                    run.merge_conflict = true;
                    run.had_merge_conflict = true;
                    run.merge_conflict_operation = operation;
                    run.merge_conflict_files = conflicted_files;
                    run.last_error = Some(error.to_string());
                    let _ = save_native_run_record(&self.cwd, run);
                }
                self.notice = Some(format!(
                    "failed to start AI resolver: {error}; press m to retry"
                ));
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

    /// A run is mergeable when it carries a jj workspace name or change id.
    fn run_is_jj(run: &AgentRun) -> bool {
        run.workspace_name.is_some() || run.jj_change_id.is_some()
    }

    fn merge_agent_at(&mut self, index: usize) -> Result<()> {
        self.pending_jj_conflict = None;
        let Some(run) = self.agents.get(index) else {
            anyhow::bail!("no selected agent");
        };
        // Already merged: a confirm prompt acted on a stale snapshot and an intervening
        // plan integration already merged this run. Re-running `rudder merge` would create a
        // spurious second merge. No-op instead.
        if run.status == AgentStatus::Merged {
            return Ok(());
        }
        // A live agent's workspace is mid-edit; merging it would land a
        // half-finished diff. Every entry point (modal, markers, board steer)
        // funnels through here, so guard once at the seam.
        if run.status == AgentStatus::Running {
            anyhow::bail!("agent is still running; stop it or wait before merging");
        }
        let is_jj = Self::run_is_jj(run);
        let review_source_ids = run.review_source_ids.clone();
        let run_id = run.id.clone();

        if is_jj {
            if let Some(run) = self.agents.get_mut(index) {
                run.integration.phase = IntegrationPhase::Integrating;
                let _ = save_native_run_record(&self.cwd, run);
            }
            // jj runs merge through the TS `rudder merge <id>` command, which
            // routes to mergeJjRunIntoCurrentWorkspace and captures op-log for
            // `rudder undo`. The command records its outcome in run.json and
            // exits 0 even on conflict, so classify by the recorded state.
            match run_rudder_jj_command(&self.cwd, "merge", &run_id, "merge") {
                JjCliOutcome::Ok { integration } => {
                    self.agents[index].integration = integration;
                }
                JjCliOutcome::Conflict { files } => {
                    self.agents[index].integration.phase = IntegrationPhase::Pending;
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
                    // The mechanical edits only clean the existing jj merge change.
                    // Re-enter the TS merge transaction so it moves the bookmark,
                    // exports to Git, and records durable integration evidence.
                    match run_rudder_jj_command(&self.cwd, "merge", &run_id, "merge") {
                        JjCliOutcome::Ok { integration } => {
                            self.agents[index].integration = integration;
                        }
                        JjCliOutcome::Conflict { files } => {
                            self.agents[index].integration.phase = IntegrationPhase::Pending;
                            self.pending_jj_conflict = Some(files);
                            anyhow::bail!(
                                "jj merge still has conflicts after automatic resolution"
                            );
                        }
                        JjCliOutcome::Failed { error } => {
                            self.agents[index].integration.phase = IntegrationPhase::Pending;
                            anyhow::bail!(error)
                        }
                    }
                }
                JjCliOutcome::Failed { error } => {
                    self.agents[index].integration.phase = IntegrationPhase::Pending;
                    anyhow::bail!(error)
                }
            }
        } else {
            anyhow::bail!(
                "this run predates Rudder's jj workspace model; restart it before integrating"
            );
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
            // plan integration path already ingested on an earlier tick; this is a no-op there).
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

        let mut merged_node_ids = Vec::new();
        let planned_merge = merge_indices.iter().any(|index| {
            self.agents
                .get(*index)
                .is_some_and(|run| run.node_id.is_some())
        });
        self.emit_merge_finished(true, false, planned_merge);
        for index in &merge_indices {
            let Some(run) = self.agents.get(*index) else {
                continue;
            };
            self.record_event(
                "merge",
                &format!("merged {}", run.task_summary),
                serde_json::json!({
                    "run": run.id,
                    "change": run.integration.merge_change_id,
                    "bookmark": run.integration.bookmark,
                    "commit": run.integration.git_commit,
                    "op": run.integration.operation_id,
                    "pushed": run.integration.pushed,
                }),
            );
        }
        for merge_index in merge_indices {
            if let Some(run) = self.agents.get_mut(merge_index) {
                run.terminal = None;
                run.review_terminal = None;
                run.status = AgentStatus::Merged;
                run.integration.phase = if run.integration.pushed {
                    IntegrationPhase::Pushed
                } else {
                    IntegrationPhase::MergedLocal
                };
                run.worktree_branch = None;
                run.completed_at = Some(Instant::now());
                run.merge_resolver = false;
                run.merge_conflict = false;
                run.merge_conflict_files.clear();
                run.needs_permission = false;
                run.needs_user_input = false;
                run.restore_pre_conflict_identity();
                if let Some(node_id) = run.node_id.clone() {
                    // Credit the run's OWN plan: node ids are per-plan, so recording a
                    // merge against the selected plan would unblock the wrong DAG.
                    merged_node_ids.push((run.plan_id.clone(), node_id));
                }
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        if !merged_node_ids.is_empty() {
            for (plan_id, node_id) in merged_node_ids {
                let plan_index =
                    match plan_id.and_then(|id| self.plans.iter().position(|plan| plan.id == id)) {
                        Some(index) => index,
                        None => self.plan_index_for_node(&node_id),
                    };
                self.plans[plan_index].plan_merged_node_ids.insert(node_id);
            }
            self.persist_plan_queue();
        }
        let _ = self.write_rudder_context_timed(None);
        // A merge is a meaningful node transition (-> merged); mirror it so the
        // board reflects it without waiting for the next poll pass. Coalesced.
        self.mirror_graph();
    }

    /// `u` on a merged row: undo exactly that merge.
    ///
    /// jj records every operation, and Rudder already captured the id its merge
    /// ran as — it just never offered it. Undoing a merge was therefore a manual
    /// `jj op log` expedition, which is a lot to ask of someone who merged the
    /// wrong row with one keystroke.
    fn undo_selected_merge(&mut self) {
        let Some(run) = self.agents.get(self.selected_agent) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        if run.status != AgentStatus::Merged {
            self.notice = Some("u undoes a merge — this row is not merged".to_string());
            return;
        }
        let Some(operation) = run.integration.operation_id.clone() else {
            self.notice = Some(
                "no recorded jj operation for this merge — undo it with `jj op log` and `jj op restore`"
                    .to_string(),
            );
            return;
        };
        let summary = run.task_summary.clone();
        let run_id = run.id.clone();
        self.record_event(
            "merge-undo",
            &format!("undoing the merge of {summary}"),
            serde_json::json!({ "run": run_id, "op": operation }),
        );
        self.start_rudder_cli_command(
            &format!("undo merge of {summary}"),
            vec!["undo".to_string(), operation],
        );
    }

    /// Replace BELIEFS with what the authorities say.
    ///
    /// Every wrong thing Rudder has shown came from a stored answer drifting from
    /// something that already knew better: a row "running" with no process, a row
    /// "merged" whose change is not in trunk, a row owning a workspace that is
    /// gone, a session id Rudder never wrote down while the backend had it on
    /// disk. Statuses that can be derived are derived here, on a slow tick, and
    /// anything that changed is recorded with its ids so the change is explicable
    /// afterwards.
    fn reconcile_rows(&mut self) {
        const EVERY: Duration = Duration::from_secs(60);
        if self.last_reconcile.is_some_and(|at| at.elapsed() < EVERY) {
            return;
        }
        self.last_reconcile = Some(Instant::now());
        self.ensure_session_ids_recorded();
        self.verify_merged_rows();
        self.flag_missing_workspaces();
    }

    /// Ask jj whether each merged row's work is STILL in trunk. A merge that was
    /// undone, abandoned, or rebased away leaves the row claiming success forever
    /// otherwise — which is exactly what happened in a live repo tonight.
    fn verify_merged_rows(&mut self) {
        let cwd = self.cwd.clone();
        let mut drifted: Vec<(String, String)> = Vec::new();
        for run in self.agents.iter_mut() {
            if run.status != AgentStatus::Merged {
                continue;
            }
            let Some(change) = run
                .integration
                .merge_change_id
                .clone()
                .or_else(|| run.jj_change_id.clone())
            else {
                continue;
            };
            let Some(in_trunk) = jj_change_in_trunk(&cwd, &change) else {
                continue;
            };
            let was = run.integration.in_trunk;
            run.integration.in_trunk = Some(in_trunk);
            if !in_trunk && was != Some(false) {
                drifted.push((run.id.clone(), change));
            }
        }
        for (run_id, change) in drifted {
            self.record_event(
                "merge-drift",
                "merged work is no longer in trunk — history was rewritten under it",
                serde_json::json!({ "run": run_id, "change": change }),
            );
            self.dirty = true;
        }
    }

    /// A row that still points at a workspace which is gone cannot be diffed,
    /// merged, or branched. Say so on the row instead of failing at the keystroke.
    fn flag_missing_workspaces(&mut self) {
        let mut vanished: Vec<(String, String)> = Vec::new();
        let mut missing_now: HashSet<String> = HashSet::new();
        for run in self.agents.iter() {
            let missing = run
                .worktree_path
                .as_ref()
                .is_some_and(|path| !path.is_dir());
            if !missing {
                continue;
            }
            missing_now.insert(run.id.clone());
            if !self.rows_missing_workspace.contains(&run.id) {
                vanished.push((
                    run.id.clone(),
                    run.worktree_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                ));
            }
        }
        self.rows_missing_workspace = missing_now;
        for (run_id, path) in vanished {
            self.record_event(
                "workspace-missing",
                "a row's workspace is gone from disk",
                serde_json::json!({ "run": run_id, "workspace": path }),
            );
            self.dirty = true;
        }
    }

    /// Pin the backend session id for every row that does not have one yet.
    ///
    /// Claude ids are minted by Rudder at launch, so they are durable from the
    /// first instant. Codex and opencode mint their OWN, and Rudder used to learn
    /// them only when a turn ended or the dashboard shut down cleanly — so a
    /// machine that died mid-session lost the mapping permanently, and with it the
    /// ability to resume or branch that conversation. (This is not hypothetical: a
    /// kernel panic took one.)
    ///
    /// Recording it while the run is alive closes that window, and running the same
    /// lookup for rows that RELOAD without an id rescues the ones already lost —
    /// the rollout is still on disk, Rudder just never wrote down which one it was.
    fn ensure_session_ids_recorded(&mut self) {
        const RETRY_AFTER: Duration = Duration::from_secs(5);
        const MAX_ATTEMPTS: u32 = 24;
        let cwd = self.cwd.clone();
        let mut found: Vec<(usize, String)> = Vec::new();
        for (index, run) in self.agents.iter().enumerate() {
            if run.session_id.is_some() || run.backend != Backend::Codex {
                continue;
            }
            // A row that never started has nothing to map.
            if matches!(run.status, AgentStatus::Failed | AgentStatus::Migrated) {
                continue;
            }
            let attempts = self.session_id_attempts.get(&run.id).copied().unwrap_or(0);
            if attempts >= MAX_ATTEMPTS {
                continue;
            }
            if self
                .session_id_last_try
                .get(&run.id)
                .is_some_and(|at| at.elapsed() < RETRY_AFTER)
            {
                continue;
            }
            let Ok(started_at_ms) = run.created_at.parse::<u64>() else {
                continue;
            };
            self.session_id_attempts
                .insert(run.id.clone(), attempts + 1);
            self.session_id_last_try
                .insert(run.id.clone(), Instant::now());
            let search_cwd = if run.cwd.as_os_str().is_empty() {
                cwd.clone()
            } else {
                run.cwd.clone()
            };
            if let Some(session_id) = codex_session_id_for_run(&search_cwd, started_at_ms) {
                found.push((index, session_id));
            }
        }
        for (index, session_id) in found {
            let Some(run) = self.agents.get_mut(index) else {
                continue;
            };
            run.session_id = Some(session_id.clone());
            let id = run.id.clone();
            let backend = run.backend.as_str();
            let _ = save_native_run_record(&cwd, run);
            self.record_event(
                "session-pinned",
                "recorded the backend session for a run",
                serde_json::json!({ "run": id, "backend": backend, "session": session_id }),
            );
            self.dirty = true;
        }
    }

    /// One `dashboard_opened` per session, on the first poll rather than in
    /// `App::new` (which tests construct thousands of times).
    fn emit_dashboard_opened_once(&mut self) {
        if self.dashboard_open_emitted {
            return;
        }
        self.dashboard_open_emitted = true;
        telemetry::emit_event(
            "dashboard_opened",
            serde_json::json!({
                "backend": self.backend.as_str(),
                "model": self.model,
                "agents_restored": self.agents.len(),
                // `project` is added by the CLI (one hash implementation, in
                // src/analytics.ts); the detached child inherits this cwd.
            }),
        );
    }

    fn poll_agents(&mut self) {
        let poll_started = Instant::now();
        self.emit_dashboard_opened_once();
        self.flush_pending_enters();
        self.maybe_start_queued_reconcile();
        self.poll_task_summary_workers();
        self.poll_completion_summary_workers();
        self.poll_final_gate();
        // Cross-process CLI stop requests are authoritative and must be consumed
        // before try_wait can observe the resulting signal as a failed process.
        self.consume_stop_requests();

        if self.last_remote_state_check.elapsed() >= self.remote_state_interval {
            self.refresh_remote_integration_state();
            self.last_remote_state_check = Instant::now();
        }

        // Both of these own their own cadence and do their work on a background
        // thread; calling them every tick only drains a channel.
        self.maybe_refresh_publish_capability();
        self.maybe_refresh_publish_pr_state();

        if self.last_worktree_gc.elapsed() >= Duration::from_secs(60) {
            self.gc_merged_workspaces();
            self.gc_orphan_workspaces();
            self.last_worktree_gc = Instant::now();
        }

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
            self.poll_handoff_inbox();
            self.reconcile_rows();
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
        let mut context_dirty = false;
        let mut completed_rudder_plans = Vec::new();
        let mut drain_perf: Vec<(Duration, serde_json::Value)> = Vec::new();
        for (index, run) in self.agents.iter_mut().enumerate() {
            let mut changed = false;
            let is_orchestrator = run.is_orchestrator();
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
                if run.status == AgentStatus::Running {
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
                            run.needs_user_input = false;
                        }
                        let _ = save_native_run_record(&repo_root, run);
                        any_dirty = true;
                        context_dirty = true;
                    }
                }
                continue;
            }
            run.last_drain_at = Some(now);
            let drain_started = Instant::now();
            let drained = terminal.drain_output();
            let drained_bytes = drained.len();
            let had_output = drained_bytes > 0;
            let headless_planner = run.mode == AgentMode::RudderPlan
                && (run.reconcile_planner || !run.interactive_orchestrator);
            let drain_duration = drain_started.elapsed();
            drain_perf.push((
                drain_duration,
                serde_json::json!({
                    "run_id": run.id.clone(),
                    "index": index,
                    "focused": is_focused,
                    "streaming_planner": is_streaming_planner,
                    "bytes": drained_bytes,
                }),
            ));
            // A headless planner's PTY contains JSON events plus rapidly repainting
            // terminal chrome. The raw PTY is never rendered; only semantic changes in
            // PlanStreamState affect the custom orchestrator pane. Do not turn every
            // status-bar repaint into a full dashboard draw.
            if had_output && !headless_planner {
                any_dirty = true;
            }
            // Feed the orchestrator's JSON event stream into its live transcript +
            // reconstructed plan text, and capture the backend session id so a refine
            // can resume the same conversation. Incremental: a no-op when no new bytes.
            if run.mode == AgentMode::RudderPlan {
                let snapshot = terminal.output_log_snapshot().to_string();
                let stream = run.plan_stream.get_or_insert_with(PlanStreamState::new);
                let prior_plan_revision = stream.plan_revision();
                let stream_changed = stream.ingest(&snapshot);
                let plan_output_changed = stream.plan_revision() != prior_plan_revision;
                let captured = stream.session_id().map(str::to_string);
                if run.plan_output_cache.is_none() || plan_output_changed {
                    let plan_text = stream
                        .exit_plan()
                        .or_else(|| stream.has_text().then(|| stream.parse_text()))
                        .unwrap_or("");
                    run.plan_output_cache = Some(parse_rudder_plan_output_cache(plan_text));
                }
                if stream_changed {
                    changed = true;
                }
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
                        changed = true;
                    }
                }
            }
            if run.status == AgentStatus::Running {
                // Evaluate the screen after activity or a brief lull to surface
                // permission and user-input prompts. Completion never comes from
                // terminal chrome; only backend lifecycle signals or process exit.
                let lull = run.last_output_at.elapsed() >= READY_EVAL_LULL;
                let visible_lines =
                    if had_output || run.needs_permission || run.needs_user_input || lull {
                        Some(terminal.visible_lines_snapshot())
                    } else {
                        None
                    };
                let previous_needs_permission = run.needs_permission;
                let previous_needs_user_input = run.needs_user_input;
                let needs_permission = visible_lines
                    .as_ref()
                    .is_some_and(|lines| terminal_needs_permission_from_lines(lines));
                run.needs_permission = needs_permission;
                // needs_permission / needs_user_input are surfaced visually (amber
                // status label) but DO NOT ring: only entering review pings (see
                // mark_run_done). Previously these rang on every detector flicker,
                // which fired a ping whenever you selected a waiting agent.
                let needs_user_input = !needs_permission
                    && visible_lines
                        .as_ref()
                        .is_some_and(|lines| terminal_needs_user_input_from_lines(lines));
                run.needs_user_input = needs_user_input;
                if run.needs_permission != previous_needs_permission
                    || run.needs_user_input != previous_needs_user_input
                {
                    changed = true;
                }
                match terminal.try_wait() {
                    Ok(Some(status)) => {
                        // Process exit is the most reliable signal: done if it
                        // succeeded, failed otherwise.
                        if status.success() {
                            mark_run_done(run);
                            changed = true;
                        } else {
                            run.status = AgentStatus::Failed;
                            run.completed_at = Some(Instant::now());
                            run.needs_permission = false;
                            run.needs_user_input = false;
                            changed = true;
                        };
                    }
                    Ok(None) => {
                        // The process is still alive (interactive agents do not exit
                        // when a turn ends).
                        //
                        // AUTHORITATIVE: the backend's own completion signal (Claude `Stop`
                        // hook / Codex `notify`, wired in signals.rs). When present it is
                        // deterministic and does not guess from terminal chrome. A "done"
                        // signal is one-shot (cleared on consume) so a later turn of the
                        // same live agent re-fires cleanly; an "input" signal marks the
                        // waiting state.
                        let signal = (!is_orchestrator)
                            .then(|| signals::read_signal(&run.id))
                            .flatten();
                        match signal {
                            Some(signals::SignalState::Done) => {
                                signals::clear_signal(&run.id);
                                mark_run_done(run);
                                changed = true;
                            }
                            Some(signals::SignalState::Input) => {
                                if !run.needs_user_input {
                                    run.needs_user_input = true;
                                    changed = true;
                                }
                            }
                            None => {
                                // Plain-process panes (rudder CLI commands like
                                // `undo`) have no lifecycle hooks BY DESIGN:
                                // their completion is process exit. This sweep
                                // used to execute them within a tick of spawning
                                // — every `u` undo pane died at ~1ms, killed by
                                // our own hook police, and the death note blamed
                                // the process ("agent process exited").
                                if !is_orchestrator
                                    && !run.plain_process
                                    && !signals::worker_has_config(&run.id, run.backend)
                                {
                                    terminal.terminate_and_wait();
                                    run.status = AgentStatus::Failed;
                                    run.completed_at = Some(Instant::now());
                                    run.last_error = Some(
                                        "worker lifecycle hooks were not installed; refusing to guess completion from terminal output"
                                            .to_string(),
                                    );
                                    changed = true;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        run.status = AgentStatus::Failed;
                        run.completed_at = Some(Instant::now());
                        run.last_error = Some(error.to_string());
                        run.needs_permission = false;
                        run.needs_user_input = false;
                        changed = true;
                    }
                }
            } else {
                let had_waiting_state = run.needs_permission || run.needs_user_input;
                run.needs_permission = false;
                run.needs_user_input = false;
                if had_waiting_state {
                    changed = true;
                }
            }
            if changed {
                if run.mode == AgentMode::RudderPlan
                    && run.status == AgentStatus::Done
                    && run.autosteered
                {
                    completed_rudder_plans.push(index);
                }
                any_dirty = true;
                context_dirty = true;
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

        self.recover_failed_cloud_migrations();

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
            } else if self.plan().rebasing {
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
        self.ingest_direct_completion_notes();

        // Drain the planned-node queue on a coarse cadence: as plan-launched
        // agents reach Merged their node ids satisfy dependents' hard deps, so a
        // periodic pass moves newly-ready nodes todo->in progress as slots free.
        // Suppressed while a plan awaits approval: nothing launches until the user
        // approves the DAG at the gate.
        self.scheduler_tick = self.scheduler_tick.wrapping_add(1);
        if !self.plan().awaiting_approval
            && !self.plan().planned_nodes.is_empty()
            && self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0
        {
            self.run_scheduler();
        }
        // Grow the DAG from finished workers' `rudder done` reports (auto-expand):
        // a finishing agent's recommended follow-ups become new planned nodes,
        // surfaced in the activity log. Autonomous, no confirm.
        //
        // This MUST run before integration: a candidate is ingested only while its status
        // is still Done, and integration flips Done -> Merged. If merge ran first, an
        // integrated node would never be ingested. Ingesting first reads the sidecar/PTY while the worker (and its
        // workspace) are intact, then merge unblocks its children on the same tick.
        if self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            self.maybe_ingest_worker_followups();
        }
        // Integrate clean finished plan nodes on the same cadence so their children
        // unblock and the chain flows without manual m/M. Runs even
        // when the queue is empty (to merge the final nodes), but not at the gate.
        if !self.plan().awaiting_approval && self.scheduler_tick % SCHEDULER_TICK_INTERVAL == 0 {
            // Mark nodes whose reviewer just finished, so integrate can merge them.
            self.finalize_node_reviews();
            self.integrate_ready_plan_nodes();
            self.maybe_start_final_gate();
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

        // PTY output wakes/redraws immediately. When no output changes, animate at a
        // calm 10fps rather than forcing a full terminal repaint on every planner
        // heartbeat (which made Ghostty and WindowServer amplify Rudder's own CPU use).
        self.advance_spinner_if_due(Instant::now());

        // MIRROR plan-launched agents' status transitions (running->review->merged
        // / failed) into graph.json so the board tracks them. Only when something
        // changed this pass AND a plan node/agent exists; the coalesce guard inside
        // mirror_graph then makes this a real shell-out ONLY when the DAG signature
        // actually changed (it is not per-tick: terminal-byte churn does not move
        // the signature, which excludes volatile output). Non-fatal.
        if any_dirty
            && (!self.plan().planned_nodes.is_empty()
                || self.agents.iter().any(|run| run.node_id.is_some()))
        {
            self.mirror_graph();
        }

        if context_dirty {
            let _ = self.write_rudder_context_timed(None);
        }

        // Live diff pane: the loop above drains each agent's MAIN terminal but not
        // its review_terminal, so the `jj diff` watch loop's every-2s output never
        // reached the grid and `v` looked frozen. Drain the selected agent's review
        // pane here whenever the Diff view is open so it refreshes as the agent edits.
        // Every OTHER review terminal is torn down: nothing drains it (its PTY
        // buffer would grow without bound) and its sh+jj watch loop would keep
        // spinning for the run's lifetime. `v` respawns one instantly on re-open.
        let viewed_review = (self.worker_view == WorkerView::Diff).then_some(self.selected_agent);
        for (index, run) in self.agents.iter_mut().enumerate() {
            let Some(review) = run.review_terminal.as_mut() else {
                continue;
            };
            if Some(index) == viewed_review {
                if !review.drain_output().is_empty() {
                    any_dirty = true;
                }
            } else {
                review.terminate_and_wait();
                run.review_terminal = None;
            }
        }

        if any_dirty {
            self.dirty = true;
        }
        for (duration, fields) in drain_perf {
            self.record_perf_duration("pty_drain_parse", duration);
            self.log_perf_duration_over(
                "pty_drain_parse",
                duration,
                SLOW_PTY_DRAIN_THRESHOLD,
                fields,
            );
        }
        let poll_duration = poll_started.elapsed();
        self.record_perf_duration("poll_agents", poll_duration);
        self.log_perf_duration_over(
            "poll_agents",
            poll_duration,
            SLOW_POLL_THRESHOLD,
            serde_json::json!({
                "agents": self.agents.len(),
                "any_dirty": any_dirty,
                "context_dirty": context_dirty,
            }),
        );
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
            let _ = self.write_rudder_context_timed(None);
            self.dirty = true;
        }
    }

    fn shutdown(&mut self) {
        for run in &mut self.agents {
            if run.terminal.is_some() {
                if run.backend == Backend::Codex && run.session_id.is_none() {
                    run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
                }
                run.terminal = None;
                if run.status == AgentStatus::Running {
                    run.status = AgentStatus::Paused;
                    run.needs_permission = false;
                    run.needs_user_input = false;
                    run.completed_at = None;
                    run.last_error = Some(
                        "paused because the Rudder dashboard closed; resume explicitly".to_string(),
                    );
                }
                // Save every detached terminal, including a Done process whose
                // interactive CLI had not exited yet, so `process` is removed.
                let _ = save_native_run_record(&self.cwd, run);
            }
        }
        let _ = self.write_rudder_context_timed(None);
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

/// Neutralize an untrusted steer instruction before it reaches a live agent PTY.
/// Preserve line breaks from the multiline web composer, but remove terminal
/// escapes and other controls. Multiline delivery uses bracketed paste so the
/// embedded newlines do not become independent submits.
fn sanitize_steer_instruction(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = normalized
        .chars()
        .map(|c| {
            if c == '\n' {
                '\n'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    cleaned
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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

#[derive(Debug, PartialEq, Eq)]
struct WorkerResumeSpec {
    target: String,
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
    direction: String,
}

fn parse_worker_resume_spec(value: &str) -> Option<WorkerResumeSpec> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    let backend = provider_backend(parts[1])?;
    let model = parts[2].trim();
    if model.is_empty() {
        return None;
    }
    let mut direction_start = 3;
    let effort = match parts.get(3).copied() {
        Some(value)
            if value.eq_ignore_ascii_case("auto") || EffortLevel::parse(value).is_some() =>
        {
            direction_start = 4;
            parse_effort_arg(value)
        }
        _ => default_effort_for(backend, model),
    };
    Some(WorkerResumeSpec {
        target: parts[0].to_string(),
        backend,
        model: model.to_string(),
        effort,
        direction: parts[direction_start..].join(" "),
    })
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut app = App::new();

    // The main loop selects on ONE channel fed by two kinds of source:
    //   * a dedicated stdin reader thread that forwards terminal input events, and
    //   * every PTY reader thread, which nudges the loop when fresh child output is
    //     ready to drain.
    // This replaces the old `event::poll(timeout)` block so child output wakes the
    // loop immediately instead of waiting up to a full tick. `signal_tx` stays live
    // in this scope for the whole run, so `recv_timeout` never sees Disconnected
    // spuriously — the loop is driven by its explicit shutdown paths.
    let (signal_tx, signal_rx) = mpsc::channel::<LoopSignal>();

    // Coalescing waker. A read burst flips `pty_output_pending` false->true and,
    // ONLY on that transition, enqueues a single `PtyOutput` signal — bounding
    // channel traffic to one message per drain cycle. The main loop clears the flag
    // at the TOP of each iteration (before poll_agents drains), so a byte arriving
    // mid-drain re-arms the waker and schedules a fresh wake: no lost-wake race.
    let pty_output_pending = Arc::new(AtomicBool::new(false));
    {
        let waker_tx = signal_tx.clone();
        let pending = Arc::clone(&pty_output_pending);
        let waker: PtyOutputWaker = Arc::new(move || {
            if pending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // A send error just means the loop is gone; nothing to do.
                let _ = waker_tx.send(LoopSignal::PtyOutput);
            }
        });
        app.pty_output_waker = Some(waker);
    }

    // Dedicated blocking reader for terminal input. `event::read` blocks until an
    // event is available and forwards each one to the loop. At shutdown this thread
    // is parked inside `event::read`; we intentionally do NOT join it — process exit
    // reaps it. A send error (loop dropped its receiver) or a read error ends it.
    {
        let term_tx = signal_tx.clone();
        thread::Builder::new()
            .name("rudder-term-reader".to_string())
            .spawn(move || loop {
                match event::read() {
                    Ok(ev) => {
                        if term_tx.send(LoopSignal::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            })
            .ok();
    }

    app.resume_migrated_agents();
    // Unfreeze finished-but-stuck in-flight work (e.g. merge resolvers that completed under
    // an older binary) BEFORE trying to resume anything, so a restart makes the board progress
    // instead of resurrecting dead processes in their stuck state.
    app.reconcile_orphaned_runs();
    // Reconcile graph.json with the restored in-memory state on startup so the board does
    // not show stale "planned" nodes from a previous session (and reflects the restored
    // plan queue / reloaded agents). last_mirror_signature is None here, so this runs once.
    app.mirror_graph();

    loop {
        let frame_started = Instant::now();
        // Reset the PTY-output waker BEFORE poll_agents drains, so a byte arriving
        // mid-drain re-arms it and enqueues a fresh wake instead of being lost.
        pty_output_pending.store(false, Ordering::Release);
        // poll_agents flips app.dirty when any state mutates (PTY bytes,
        // status change, cloud info, etc).
        app.poll_agents();
        app.refresh_tab_title();
        if drain_ready_events(&mut app, MAX_EVENTS_PER_FRAME, &signal_rx)? {
            app.shutdown();
            break;
        }
        if app.dirty && app.should_defer_scroll_draw() {
            app.perf.log(
                "draw_deferred",
                serde_json::json!({
                    "reason": "scroll_burst",
                    "scroll_events": app.scroll_events_since_draw,
                }),
            );
        } else if app.take_dirty() {
            let draw_started = Instant::now();
            let mut render_build_us = 0_u64;
            terminal.draw(|frame| {
                let render_started = Instant::now();
                render(frame, &mut app);
                render_build_us = render_started.elapsed().as_micros() as u64;
            })?;
            let scroll_events = app.consume_scroll_draw_stats();
            let draw_duration = draw_started.elapsed();
            app.record_perf_duration("terminal_draw", draw_duration);
            app.log_perf_duration_over(
                "terminal_draw",
                draw_duration,
                SLOW_DRAW_THRESHOLD,
                serde_json::json!({
                    "render_build_us": render_build_us,
                    "scroll_events": scroll_events,
                }),
            );
        }

        // PTY readers wake this loop as soon as child output arrives, so the timeout is
        // only a backstop for timer-driven state and does not need a planner fast path.
        let poll_timeout = app.scroll_draw_poll_timeout(TICK_RATE);
        let active_us_before_block = frame_started.elapsed().as_micros() as u64;
        // Block until a terminal event, a PTY-output nudge, or the tick backstop.
        // TICK_RATE still bounds worst-case latency for state that
        // does not signal (cloud polling, timers); PtyOutput just wakes us sooner.
        match signal_rx.recv_timeout(poll_timeout) {
            Ok(LoopSignal::Term(ev)) => {
                if handle_event(&mut app, ev) {
                    app.shutdown();
                    break;
                }
                let shutdown = drain_ready_events(
                    &mut app,
                    MAX_EVENTS_PER_FRAME.saturating_sub(1),
                    &signal_rx,
                )?;
                if shutdown {
                    app.shutdown();
                    return Ok(());
                }
            }
            // Fresh child output: nothing to do here — the loop wraps back to
            // poll_agents, which drains it. The wake itself is the point.
            Ok(LoopSignal::PtyOutput) => {}
            // Backstop tick elapsed with no signal.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The term reader and every waker sender are gone AND our own sender
            // was dropped — cannot happen while `signal_tx` lives, but shut down
            // cleanly rather than spin if it ever does.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                app.shutdown();
                break;
            }
        }
        let frame_duration = frame_started.elapsed();
        app.record_perf_duration("frame_total", frame_duration);
        app.log_perf_duration_over(
            "frame_total",
            frame_duration,
            SLOW_FRAME_THRESHOLD,
            serde_json::json!({
                "active_us_before_block": active_us_before_block,
            }),
        );
        app.perf_stats.emit_due(&mut app.perf);
    }

    Ok(())
}

fn drain_ready_events(
    app: &mut App,
    max_events: usize,
    signal_rx: &mpsc::Receiver<LoopSignal>,
) -> Result<bool> {
    let mut drained_events = 0_usize;
    while drained_events < max_events {
        match signal_rx.try_recv() {
            Ok(LoopSignal::Term(ev)) => {
                if handle_event(app, ev) {
                    app.perf.log(
                        "event_drain",
                        serde_json::json!({
                            "count": drained_events + 1,
                            "phase": "ready",
                        }),
                    );
                    return Ok(true);
                }
                drained_events += 1;
            }
            // A PTY-output nudge here is a no-op — the main loop wraps back to
            // poll_agents to drain it. Skip WITHOUT charging the input budget, so
            // MAX_EVENTS_PER_FRAME still bounds only real input events.
            Ok(LoopSignal::PtyOutput) => continue,
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    if drained_events > 1 {
        app.perf.log(
            "event_drain",
            serde_json::json!({
                "count": drained_events,
                "phase": "ready",
            }),
        );
    }
    Ok(false)
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

fn load_followup_gen(cwd: &Path) -> HashMap<String, u8> {
    std::fs::read_to_string(cwd.join(".rudder").join("followup-gen.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<HashMap<String, u8>>(&raw).ok())
        .unwrap_or_default()
}

/// Whether a merged row's workspace may be deleted.
///
/// "Merged" does NOT mean the agent is gone: claude and codex stay RESIDENT between
/// turns, so a row can be merged while its pane is still live and rooted in that
/// directory. Deleting it then leaves the agent running in a phantom cwd — observed
/// in the wild as a codex worker whose working directory had become the user's HOME,
/// with approvals bypassed, so anything it wrote landed outside the repo and nothing
/// it did could ever merge. A live pane keeps its workspace; clearing the row (`cc`,
/// `dd`) drops the terminal and the next sweep collects it.
fn merged_workspace_is_sweepable(
    run: &AgentRun,
    now: std::time::SystemTime,
    grace: Duration,
) -> bool {
    if run.status != AgentStatus::Merged || run.is_main() {
        return false;
    }
    if run.terminal.is_some() {
        return false;
    }
    let Some(path) = run.worktree_path.as_ref() else {
        return false;
    };
    // Grace: only sweep once the checkout has been idle past the window. The dir
    // mtime survives restarts, unlike an in-memory timestamp.
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .map(|age| age >= grace)
        .unwrap_or(false)
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

/// Make a Claude session resumable from `target_cwd`. Claude Code scopes `--resume`
/// lookup to the CURRENT directory's project folder under ~/.claude/projects, so a
/// fork spawned in a fresh workspace cannot see the source session ("No conversation
/// found with session ID"). Copying the transcript into the target's project folder
/// is enough: `--resume <sid> --fork-session` reads it there and mints a NEW session
/// for the fork, leaving the original transcript untouched.
fn stage_claude_session_for_cwd(
    source_cwd: &Path,
    session_id: &str,
    target_cwd: &Path,
) -> std::result::Result<(), String> {
    let source = claude_transcript_path(source_cwd, session_id)
        .ok_or_else(|| "no session transcript found to branch".to_string())?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let target_dir = home
        .join(".claude")
        .join("projects")
        .join(encode_claude_project_dir(target_cwd));
    let target = target_dir.join(format!("{session_id}.jsonl"));
    if target == source {
        return Ok(());
    }
    std::fs::create_dir_all(&target_dir)
        .map_err(|error| format!("could not create session dir: {error}"))?;
    std::fs::copy(&source, &target)
        .map_err(|error| format!("could not stage session transcript: {error}"))?;
    Ok(())
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
    /// Identity of the plan this queue belongs to. Empty in a file written before plans
    /// had ids; restore mints a fresh one so the queue is never dropped on upgrade.
    #[serde(default)]
    id: String,
    #[serde(default)]
    planned_nodes: Vec<PlannedNode>,
    #[serde(default)]
    launched_node_ids: HashSet<String>,
    #[serde(default)]
    merged_node_ids: HashSet<String>,
    #[serde(default)]
    capture_armed: bool,
    #[serde(default)]
    planned_origin: String,
    #[serde(default)]
    plan_request: String,
    #[serde(default)]
    plan_summary: Option<String>,
    #[serde(default)]
    final_gate_status: FinalGateStatus,
    #[serde(default)]
    final_gate_summary: Option<String>,
    #[serde(default)]
    awaiting_approval: bool,
}

impl PlanQueueSnapshot {
    fn from_plan(plan: &PlanState) -> Self {
        Self {
            id: plan.id.clone(),
            planned_nodes: plan.planned_nodes.clone(),
            launched_node_ids: plan.plan_launched_node_ids.clone(),
            merged_node_ids: plan.plan_merged_node_ids.clone(),
            capture_armed: plan.plan_capture_armed,
            planned_origin: plan.planned_origin.clone(),
            plan_request: plan.plan_request.clone(),
            plan_summary: plan.plan_summary.clone(),
            final_gate_status: plan.final_gate_status,
            final_gate_summary: plan.final_gate_summary.clone(),
            awaiting_approval: plan.awaiting_approval,
        }
    }

    fn into_plan(self) -> PlanState {
        // The approval-gate draft is a projection of the queue, so it is rebuilt rather
        // than persisted: a restored plan at the gate must be editable immediately.
        let plan_review = if self.awaiting_approval {
            PlanReviewState::from_planned_nodes(&self.planned_nodes)
        } else {
            PlanReviewState::default()
        };
        PlanState {
            id: if self.id.is_empty() {
                new_plan_id()
            } else {
                self.id
            },
            planned_nodes: self.planned_nodes,
            plan_launched_node_ids: self.launched_node_ids,
            plan_merged_node_ids: self.merged_node_ids,
            plan_capture_armed: self.capture_armed,
            plan_review,
            planned_origin: self.planned_origin,
            plan_request: self.plan_request,
            plan_summary: self.plan_summary,
            final_gate_status: self.final_gate_status,
            final_gate_summary: self.final_gate_summary,
            awaiting_approval: self.awaiting_approval,
            ..PlanState::default()
        }
    }
}

/// `.rudder/plan-queue.json`: EVERY live plan, because several `/plan`s run at once.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PlanQueueFile {
    #[serde(default)]
    plans: Vec<PlanQueueSnapshot>,
}

impl PlanQueueFile {
    /// A file written before plans could coexist is a bare single-plan snapshot at the
    /// top level, so parsing falls back to that shape rather than dropping the queue.
    fn parse(raw: &str) -> Option<Self> {
        if let Ok(file) = serde_json::from_str::<PlanQueueFile>(raw) {
            if !file.plans.is_empty() {
                return Some(file);
            }
        }
        let legacy = serde_json::from_str::<PlanQueueSnapshot>(raw).ok()?;
        Some(Self {
            plans: vec![legacy],
        })
    }

    /// Rebuild the live plan list. `App::plans` is never empty, so a file with no plans
    /// still yields one (empty) plan for the dashboard to start from.
    fn into_plans(self) -> Vec<PlanState> {
        let plans: Vec<PlanState> = self
            .plans
            .into_iter()
            .map(PlanQueueSnapshot::into_plan)
            .collect();
        if plans.is_empty() {
            vec![PlanState {
                id: new_plan_id(),
                ..PlanState::default()
            }]
        } else {
            plans
        }
    }
}

/// Mint a plan id. Plans outlive the orchestrator ROW that started them (refine and
/// rebase relaunch that row), so the id is independent of any run id.
fn new_plan_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "plan-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Fold the persisted runs back into a restored queue so the crash window between a
/// worker record being written and the shrunken queue being saved cannot relaunch a
/// node. Only runs belonging to THIS plan count: a run stamped with another plan's id
/// says nothing about this one. A run with no `plan_id` predates concurrent plans and
/// is attributed to the only plan a file of that vintage can have (`sole_plan`).
fn reconcile_plan_queue_with_agents(
    snapshot: &mut PlanQueueSnapshot,
    agents: &[AgentRun],
    sole_plan: bool,
) {
    for run in agents {
        let Some(node_id) = run.node_id.as_ref() else {
            continue;
        };
        let owned = match run.plan_id.as_deref() {
            Some(plan_id) => plan_id == snapshot.id,
            None => sole_plan,
        };
        if !owned {
            continue;
        }
        snapshot.launched_node_ids.insert(node_id.clone());
        if run.status == AgentStatus::Merged {
            snapshot.merged_node_ids.insert(node_id.clone());
        }
    }
    snapshot
        .planned_nodes
        .retain(|node| !snapshot.launched_node_ids.contains(&node.id));
}

fn load_plan_queue(cwd: &Path) -> Option<PlanQueueFile> {
    let raw = std::fs::read_to_string(cwd.join(".rudder").join("plan-queue.json")).ok()?;
    PlanQueueFile::parse(&raw)
}

/// The whole startup restore: persisted run records plus `.rudder/plan-queue.json` in,
/// the reconciled fleet and the live plans out. `App::new()` calls this and nothing else
/// to rebuild what the last process was doing.
///
/// It is one function rather than two because the two answers are entangled: a plan's
/// remaining nodes are decided by which RUNS came back (a node whose worker is already on
/// disk must not relaunch), and which PLANNER ROWS survive is decided by which plans came
/// back (every plan that ever ran left a planner row behind). Restoring them separately is
/// what made the concurrent-plan collapse possible.
fn restore_persisted_state(cwd: &Path) -> (Vec<AgentRun>, Vec<PlanState>) {
    let mut agents = load_persisted_agents(cwd);
    // Restore the approval-gate queue (queued nodes + gate state) so a mid-plan restart
    // resumes the plan instead of silently losing it. awaiting_approval is restored
    // TOGETHER with the queue, so the scheduler never launches an un-approved plan.
    let mut restored_queue = load_plan_queue(cwd).unwrap_or_default();
    let sole_plan = restored_queue.plans.len() <= 1;
    for snapshot in &mut restored_queue.plans {
        reconcile_plan_queue_with_agents(snapshot, &agents, sole_plan);
        // A final gate that was mid-flight when the process died has no process behind it
        // any more, so it re-arms as Idle rather than claiming a run that is not happening.
        if snapshot.final_gate_status == FinalGateStatus::Running {
            snapshot.final_gate_status = FinalGateStatus::Idle;
            snapshot.final_gate_summary = None;
        }
    }
    let restored_plans = restored_queue.into_plans();
    // Runs persisted before plans had ids carry no `plan_id`. With one restored plan
    // there is exactly one answer, so adopt them into it — otherwise the restored
    // fleet would look ownerless and the plan would read as spent. With SEVERAL plans
    // there is no answer, and guessing would hand one plan another's workers.
    if restored_plans.len() == 1 {
        let plan_id = restored_plans[0].id.clone();
        for run in &mut agents {
            if run.plan_id.is_none() && (run.node_id.is_some() || run.is_orchestrator()) {
                run.plan_id = Some(plan_id.clone());
            }
        }
    }
    // Drop orchestrator rows whose plan did not come back. `load_persisted_agents`
    // now keeps the newest planner PER PLAN, which is what stops concurrent plans
    // collapsing into one across a restart — but every plan that ever ran left a
    // planner row on disk, so without this the pane fills with orchestrators for
    // plans that no longer exist and cannot be steered. This is the only place that
    // knows which plans were actually restored. Rows with no plan id predate
    // concurrent plans and are left alone; the loader already collapsed those.
    if !restored_plans.is_empty() {
        let live: HashSet<&str> = restored_plans.iter().map(|plan| plan.id.as_str()).collect();
        agents.retain(|run| {
            !run.is_orchestrator()
                || run
                    .plan_id
                    .as_deref()
                    .is_none_or(|plan_id| live.contains(plan_id))
        });
    }
    (agents, restored_plans)
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
    let _lock = acquire_rudder_md_lock(repo_root);
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
        "RUDDER_CLOUD_MIGRATE",
        "RUDDER_REVIEW_ALL",
        "RUDDER_MERGE_ALL",
        "RUDDER_MERGE",
        "RUDDER_STOP",
        "RUDDER_RESUME",
        "RUDDER_REGOAL",
        "RUDDER_INJECT",
        "RUDDER_ADD_TASK",
        "RUDDER_REPLAN",
        "RUDDER_PLAN",
        "RUDDER_RUN",
        "RUDDER_ASK",
    ]
    .iter()
    .any(|marker| line == *marker || line.starts_with(&format!("{marker} ")))
}

fn handle_event(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            app.mark_dirty();
            app.handle_key(key)
        }
        Event::Key(_) => false,
        Event::Paste(text) => {
            app.mark_dirty();
            app.handle_paste(text);
            false
        }
        Event::Mouse(mouse) => {
            if app.handle_mouse(mouse) {
                app.mark_dirty();
            }
            false
        }
        Event::Resize(_, _) => {
            app.mark_dirty();
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
