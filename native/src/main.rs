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

const TICK_RATE: Duration = Duration::from_millis(33);
const MAX_EVENTS_PER_FRAME: usize = 64;
const INTERACTIVE_COMPLETION_IDLE: Duration = Duration::from_secs(4);
const FOCUS_COLOR: Color = Color::Rgb(57, 255, 20);
const INACTIVE_COLOR: Color = Color::DarkGray;
const MODEL_COLOR: Color = Color::Magenta;
const RUNNING_COLOR: Color = Color::Yellow;
const DONE_COLOR: Color = Color::Gray;
const FAILED_COLOR: Color = Color::Red;
const CLOUD_COLOR: Color = Color::Cyan;
const DEFAULT_WHEEL_SCROLL_ROWS: u16 = 1;
const TASK_HISTORY_LIMIT: usize = 100;
const MOUSE_DEBUG_ENV: &str = "RUDDER_MOUSE_DEBUG";
const RUDDER_MOUSE_ENABLE_SEQUENCES: &[u8] = b"\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const RUDDER_MOUSE_DISABLE_SEQUENCES: &[u8] = b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const AGENT_LIST_RUN_START_ROW: u16 = 12;
const REVIEW_ALL_MODEL: &str = "gpt-5.5";
const REVIEW_ALL_EFFORT: EffortLevel = EffortLevel::XHigh;
const TASK_SUMMARY_MODEL: &str = "claude-haiku-4-5-20251001";
const AGENT_PANE_HINTS: &[&str] = &[
    "j/k move",
    "Enter focus",
    "r rename",
    "v review",
    "u sync",
    "R review all",
    "m merge",
    "M merge all",
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
    cwd: PathBuf,
    branch: Option<String>,
    task_input: String,
    task_cursor: usize,
    task_history: Vec<String>,
    task_history_index: Option<usize>,
    task_history_draft: String,
    plan_mode: bool,
    agents: Vec<AgentRun>,
    selected_agent: usize,
    backend: Backend,
    model: String,
    effort: Option<EffortLevel>,
    notice: Option<String>,
    cloud_prompt: Option<CloudLaunchPrompt>,
    delete_pending: Option<String>,
    merge_confirm: Option<MergeConfirmation>,
    conflict_prompt: Option<MergeConflictPrompt>,
    picker_index: usize,
    worker_selection: Option<WorkerSelection>,
    task_selection: Option<WorkerSelection>,
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
            cwd,
            branch,
            task_input,
            task_cursor,
            task_history: Vec::new(),
            task_history_index: None,
            task_history_draft: String::new(),
            plan_mode: false,
            agents,
            selected_agent: 0,
            backend: selection.backend,
            model: selection.model,
            effort: selection.effort,
            notice: None,
            cloud_prompt: None,
            delete_pending: None,
            merge_confirm: None,
            conflict_prompt: None,
            picker_index: 0,
            worker_selection: None,
            task_selection: None,
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
                "Ctrl+W: 1 agents  2 worker  3 task  v review  m merge  R review-all  M merge-all  Esc cancels"
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
            KeyCode::Char('u') => self.request_sync_selected_agent(),
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
            KeyCode::Char('u') => self.request_sync_selected_agent(),
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
                if !self.agents.is_empty() {
                    self.delete_pending = None;
                    if self.selected_is_main() {
                        self.focus_or_spawn_main();
                    } else {
                        self.focus = FocusPane::Worker;
                    }
                }
            }
            KeyCode::Char('v') => self.toggle_worker_view(),
            KeyCode::Char('r') => self.start_rename_selected_agent(),
            KeyCode::Char('u') => self.request_sync_selected_agent(),
            KeyCode::Char('R') => self.review_all_ready(),
            KeyCode::Char('M') => self.request_merge_all_ready(),
            KeyCode::Char('m') => self.request_merge_selected_agent(),
            KeyCode::Char('d') => self.delete_selected_agent(),
            KeyCode::Char('P') => self.open_main_model_switcher(),
            _ => {}
        }
        false
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
        self.clear_selected_attention_flags();
        if let Some(prompt) = self.capture_selected_worker_key(key, capture_as_prompt) {
            self.record_selected_worker_prompt(prompt);
        }
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
                self.ensure_hunk_review();
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
                    "type /plan, /rudder-plan, /model, /main|/m, /sync, /goal, /usage, or /cloud"
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
                if self.worker_view == WorkerView::Diff {
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
            if self.worker_view == WorkerView::Diff {
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

        if self.plan_mode {
            self.start_plan_task(&input);
        } else {
            self.start_execute_task(&input);
        }
    }

    fn start_execute_task(&mut self, input: &str) {
        self.start_execute_task_with_summary(input, None);
    }

    fn start_execute_task_with_summary(&mut self, input: &str, explicit_summary: Option<&str>) {
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
        let worktree = match prepare_worktree(&self.cwd, worktree_label) {
            Ok(worktree) => worktree,
            Err(error) => {
                self.notice = Some(format!("worktree failed: {error}"));
                WorktreeInfo::current(self.cwd.clone())
            }
        };
        if let Err(error) = write_rudder_context(&self.cwd, &self.agents, Some(&worktree)) {
            self.notice = Some(format!("context warning: {error}"));
        }
        if let Err(error) = ensure_hunk_config(&worktree.path) {
            self.notice = Some(format!("hunk config warning: {error}"));
        }

        let model = self.model.clone();
        let backend = self.backend;
        let effort = self.effort;
        let session_id = mint_session_id_for(backend);
        let command = agent_command(
            backend,
            &model,
            effort,
            input,
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
            task: input.to_string(),
            task_summary,
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
            cwd: worktree.path.clone(),
            worktree_branch: worktree.branch.clone(),
            worktree_path: worktree.path_is_worktree.then_some(worktree.path.clone()),
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
        let model = self.model.clone();
        let backend = self.backend;
        let effort = self.effort;
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
        };

        match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(mut terminal) => {
                let _ = terminal.drain_output();
                run.terminal = Some(terminal);
                self.notice = Some("rudder planner started".to_string());
            }
            Err(error) => {
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                self.notice = Some(format!(
                    "failed to start {} rudder planner: {error}",
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
                let task = parts.collect::<Vec<_>>().join(" ");
                if task.trim().is_empty() {
                    self.plan_mode = !self.plan_mode;
                    self.notice = Some(if self.plan_mode {
                        "plan mode on: Enter starts a read-only planner".to_string()
                    } else {
                        "plan mode off".to_string()
                    });
                } else {
                    self.start_plan_task(task.trim());
                }
                true
            }
            Some("/rudder-plan") => {
                let task = parts.collect::<Vec<_>>().join(" ");
                if task.trim().is_empty() {
                    self.notice = Some("usage: /rudder-plan <task>".to_string());
                } else {
                    self.start_rudder_plan_task(task.trim());
                }
                true
            }
            Some("/help") => {
                self.notice = Some(
                    "Option-1/2/3 or ^W pane  Enter start/focus  /plan  /rudder-plan  /model  /main|/m  /sync  /goal"
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
            Some("/sync") => {
                self.request_sync_selected_agent();
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

    fn spawn_agents_from_rudder_plan(&mut self, index: usize) {
        let Some(run) = self.agents.get_mut(index) else {
            return;
        };
        if run.mode != AgentMode::RudderPlan || !run.autosteered {
            return;
        }
        let planner_task = run.task.clone();
        let output = rudder_plan_output_for_run(run);

        let tasks = match extract_rudder_plan_tasks(&output) {
            Ok(tasks) => tasks,
            Err(error) => {
                self.notice = Some(format!(
                    "rudder-plan did not produce runnable tasks: {error}"
                ));
                return;
            }
        };
        if tasks.is_empty() {
            run.autosteered = false;
            let _ = save_native_run_record(&self.cwd, run);
            self.notice = Some("rudder-plan produced no runnable tasks".to_string());
            return;
        }

        run.autosteered = false;
        let _ = save_native_run_record(&self.cwd, run);
        let count = tasks.len();
        let worker_backend = self.backend;
        for task in tasks {
            let prompt = rudder_plan_worker_prompt(&planner_task, &task, worker_backend);
            self.start_execute_task_with_summary(&prompt, Some(&task.title));
        }
        self.notice = Some(format!("rudder-plan spawned {count} agent(s)"));
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
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            run.status = AgentStatus::Failed;
            run.last_error = Some(message);
        }
    }

    fn set_selected_review_error(&mut self, message: String) {
        if let Some(run) = self.agents.get_mut(self.selected_agent) {
            run.review_error = Some(message);
        }
    }

    fn ensure_hunk_review(&mut self) {
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
            let command = TerminalCommand::with_args(
                "sh",
                [
                    "-lc",
                    "theme=\"${RUDDER_HUNK_THEME:-paper}\"; if [ \"$theme\" = light ]; then theme=paper; fi; if command -v hunk >/dev/null 2>&1; then exec hunk diff --watch --theme \"$theme\"; fi; if command -v hunkdiff >/dev/null 2>&1; then exec hunkdiff diff --watch --theme \"$theme\"; fi; while :; do printf '\\033[2J\\033[H'; git status --short; printf '\\n'; git diff --stat HEAD; printf '\\n'; git diff --color=always HEAD; sleep 2; done",
                ],
            );
            if let Err(error) = ensure_hunk_config(&run.cwd) {
                run.review_error = Some(error.to_string());
                self.notice = Some(format!("hunk config warning: {error}"));
            }
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
                    self.notice = Some(format!("failed to open Hunk: {error}"));
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
        if run.worktree_branch.is_none() || run.worktree_path.is_none() {
            self.notice = Some("selected agent has no worktree to sync".to_string());
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
        if run.worktree_branch.is_none() {
            self.notice = Some("selected agent has no worktree to merge".to_string());
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
                    && run.worktree_branch.is_some()
                    && !claimed.contains(&run.id)
            })
            .collect();

        if ready_runs.is_empty() {
            self.notice = Some("no completed worktrees ready to merge".to_string());
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
        let mut conflicts = conflicted_files(&self.cwd);
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
            "{operation_label} conflict in {count} file{}: press y for AI help or n to resolve manually",
            if count == 1 { "" } else { "s" }
        ));
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
            "(git did not report conflicted files)".to_string()
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
        let worktree = prompt
            .worktree_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown worktree)".to_string());
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
            "A git merge stopped with conflicts and you are now the conflict resolver.\n\
\n\
Where you are working\n\
- You are running inside the main repo at: {repo}\n\
- The merge is happening on the target branch (currently checked out here): {target}\n\
- The branch being merged in is the agent's worktree branch: {source}\n\
- That branch was developed in this worktree (separate checkout): {worktree}\n\
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
How to think about the sides\n\
- The 'ours' side in every conflict marker is {target} (what is already on the main branch).\n\
- The 'theirs' side is {source} (the agent's new work coming from {worktree}).\n\
- Preserve the intent of the task on {source} while not regressing existing behavior on {target}.\n\
\n\
What to do\n\
1. Run `git status` from {repo} to see the merge state.\n\
2. Open each conflicted file at {repo} and resolve the markers. Edit files in {repo}, not in {worktree}.\n\
3. After every file is resolved, run any relevant tests or checks the repo provides.\n\
4. Stage the resolved files with `git add` and tell me what you changed and why.\n\
5. Do NOT run `git commit` (the merge is in progress; the user will commit when they are ready).\n\
6. Do NOT run `git merge --abort` unless the conflicts are truly unresolvable, and if so explain why.\n",
            repo = repo,
            target = target,
            source = source,
            worktree = worktree,
            task = prompt.task,
            files = files,
            err = prompt.error,
        ))
    }

    fn merge_agent_at(&mut self, index: usize) -> Result<()> {
        let Some(run) = self.agents.get(index) else {
            anyhow::bail!("no selected agent");
        };
        let Some(branch) = run.worktree_branch.clone() else {
            anyhow::bail!("selected agent is not in a worktree");
        };
        let review_source_ids = run.review_source_ids.clone();

        commit_pending_changes_for_run(run)?;

        match merge_strategy() {
            MergeStrategy::Merge => {
                git_status_command(&self.cwd, &["merge", "--no-ff", &branch])?;
            }
            MergeStrategy::Rebase => {
                let base_branch = current_branch_at(&self.cwd).unwrap_or_else(|| "HEAD".to_string());
                rebase_worktree_onto_base(&self.cwd, &run.cwd, &base_branch)?;
                git_status_command(&self.cwd, &["merge", "--ff-only", &branch])?;
            }
        }
        // Successful merge: keep the agent's row in the dashboard but flip it
        // to Merged so it appears in a dedicated section. Keep the worktree
        // path on the record and defer `git worktree remove` to delete, which
        // keeps merge confirmation responsive and preserves cleanup control.
        // Never touch the dedicated main agent.
        if index < self.agents.len() && !self.agents[index].is_main() {
            self.mark_agent_and_review_sources_merged(index, review_source_ids);
        }
        Ok(())
    }

    fn sync_agent_at(&mut self, index: usize) -> Result<()> {
        let Some(run) = self.agents.get(index) else {
            anyhow::bail!("no selected agent");
        };
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
            let due_to_drain = is_focused
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
            if had_output {
                run.last_output_at = Instant::now();
                if run.status == AgentStatus::Done {
                    run.status = AgentStatus::Running;
                    run.completed_at = None;
                    changed = true;
                }
            }
            if run.status == AgentStatus::Running {
                let idle_enough = run.last_output_at.elapsed() >= INTERACTIVE_COMPLETION_IDLE;
                let visible_lines =
                    if had_output || run.needs_permission || run.needs_user_input || idle_enough {
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
                        if idle_enough
                            && visible_lines.as_ref().is_some_and(|lines| {
                                terminal_looks_ready_for_input_from_lines(run.backend, lines)
                            })
                        {
                            mark_run_done(run);
                            changed = true;
                        }
                    }
                    Err(error) => {
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

        for index in completed_rudder_plans {
            self.spawn_agents_from_rudder_plan(index);
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

        if event::poll(TICK_RATE)? {
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

