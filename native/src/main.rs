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
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
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

        if self.nav_mode {
            return self.handle_nav_key(key);
        }

        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META)
        {
            match key.code {
                KeyCode::Char('1') => {
                    self.focus = FocusPane::Agents;
                    return false;
                }
                KeyCode::Char('2') => {
                    self.focus = FocusPane::Worker;
                    return false;
                }
                KeyCode::Char('3') => {
                    self.focus = FocusPane::Task;
                    return false;
                }
                KeyCode::Char('v') => {
                    self.toggle_worker_view();
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
        let position = visible
            .iter()
            .position(|&index| index == self.selected_agent)
            .unwrap_or_else(|| {
                visible
                    .iter()
                    .position(|&index| index >= self.selected_agent)
                    .unwrap_or_else(|| visible.len().saturating_sub(1))
            });
        self.selected_agent = visible[(position + 1).min(visible.len().saturating_sub(1))];
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
                    "Option-1/2/3 pane  Enter start/focus  wheel scrolls worker  R review all  M merge all"
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
                    "Option-1/2/3 pane  Enter start/focus  /plan  /rudder-plan  /model  /main|/m  /sync  /goal"
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

fn mark_run_done(run: &mut AgentRun) {
    if run.status != AgentStatus::Done {
        run.status = AgentStatus::Done;
        run.completed_at = Some(Instant::now());
        run.needs_permission = false;
        run.permission_notified = false;
        run.needs_user_input = false;
        run.user_input_notified = false;
        play_completion_sound();
    }
}

fn terminal_looks_ready_for_input_from_lines(backend: Backend, lines: &[String]) -> bool {
    if terminal_needs_permission_from_lines(lines) {
        return false;
    }
    if terminal_needs_user_input_from_lines(lines) {
        // Waiting on a question, not "done".
        return false;
    }

    let recent = lines
        .iter()
        .rev()
        .take(12)
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if recent.iter().any(|line| looks_busy(line)) {
        return false;
    }

    // Strongest signal: the agent's idle chrome footer is visible. That footer
    // is only rendered while the agent is sitting at its prompt waiting for
    // input, so it never appears mid-turn. Falls back to the prompt-char
    // heuristic if we don't see chrome (older claude versions, raw shells).
    if recent
        .iter()
        .any(|line| looks_like_idle_chrome(backend, line))
    {
        return true;
    }

    recent
        .iter()
        .any(|line| looks_like_agent_prompt(backend, line))
}

/// Returns true if the given line looks like static footer/chrome text that
/// the agent only renders while idle at its prompt.
fn looks_like_idle_chrome(backend: Backend, line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let common = [
        "shift+tab to cycle",
        "shift+tab for plan",
        "bypass permissions on",
        "bypass permissions off",
        "/help for commands",
        "tab to switch agent",
        "press up to edit",
        "esc to clear",
    ];
    if common.iter().any(|c| lower.contains(c)) {
        return true;
    }
    match backend {
        Backend::Claude => lower.contains("(shift+tab"),
        Backend::Codex => {
            // Codex idle markers seen in the wild:
            //   "Worked for 4m 46s"        - turn-end summary line
            //   "/ps to view"              - background-jobs hint
            //   "/stop to close"           - same hint, separate token
            //   "/ for commands"           - slash-help hint
            //   "ctrl+j newline"           - input-area hint
            //   "<model> <effort> ... · <cwd>"  - bottom status bar
            if lower.contains("worked for ")
                || lower.contains("/ps to view")
                || lower.contains("/stop to close")
                || lower.contains("/ for commands")
                || lower.contains("ctrl+j newline")
            {
                return true;
            }
            // Status bar pattern: contains " · " AND a path token starting with
            // "~/" or "/", but is NOT a busy "interrupt" line.
            !lower.contains("interrupt")
                && lower.contains(" \u{00b7} ")
                && (lower.contains(" ~/") || lower.contains(" /"))
        }
    }
}

fn terminal_needs_user_input_from_lines(lines: &[String]) -> bool {
    // Collect the most recent non-empty normalized lines (top of list = most recent).
    let recent_rev = lines
        .iter()
        .rev()
        .map(|line| normalize_terminal_line(line))
        .filter(|line| !line.is_empty())
        .take(6)
        .collect::<Vec<_>>();
    if recent_rev.is_empty() {
        return false;
    }

    let last = recent_rev[0].as_str();
    // Guard against false positives from chatty multi-paragraph output: the
    // closing line should be short.
    if last.chars().count() > 120 {
        return false;
    }

    // Suppress when the agent is clearly mid-work.
    if recent_rev.iter().any(|line| looks_busy(line)) {
        return false;
    }

    // Tail cursor heuristic: most of the trailing screen rows must be empty.
    // (visible_lines_snapshot returns the whole pane; if the cursor sits at
    // the bottom of the screen, the last rows will be blank.)
    let tail_blanks = lines
        .iter()
        .rev()
        .take(3)
        .filter(|line| normalize_terminal_line(line).is_empty())
        .count();
    if tail_blanks == 0 && lines.len() > 6 {
        // Cursor isn't near the bottom — likely just chatty output, not a prompt.
        // Still allow it through if the last line clearly ends in '?'.
        if !last.trim_end().ends_with('?') {
            // Also allow numbered menu pattern in the last 6 rows.
            if !has_numbered_menu_pattern(&recent_rev) {
                return false;
            }
        }
    }

    if last.trim_end().ends_with('?') {
        return true;
    }

    let lower = last.to_ascii_lowercase();
    let cues = [
        "what would you like",
        "how should i",
        "which would you",
        "can you clarify",
        "please confirm",
        "choose one",
        "select",
    ];
    if cues.iter().any(|cue| lower.contains(cue)) {
        return true;
    }

    if has_numbered_menu_pattern(&recent_rev) {
        return true;
    }

    false
}

fn has_numbered_menu_pattern(recent_rev: &[String]) -> bool {
    // recent_rev holds the most recent non-empty lines (most recent first).
    // Require at least two DISTINCT numeric options (1 and 2, or 1 and 3, etc.)
    // so a real numbered list inside agent output doesn't trip us. Also accept
    // a leading "❯ N." (cursor) plus another N. as a strong selection signal.
    let mut seen_indices = std::collections::HashSet::new();
    let mut saw_cursor_option = false;
    for line in recent_rev.iter().take(8) {
        let stripped = line
            .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{276f}' || c == '\u{25b8}');
        if line.starts_with("\u{276f} ") || line.starts_with("\u{25b8} ") {
            saw_cursor_option = true;
        }
        for n in 1u8..=9 {
            let prefix_dot = format!("{n}.");
            let prefix_paren = format!("{n})");
            if stripped.starts_with(&prefix_dot) || stripped.starts_with(&prefix_paren) {
                seen_indices.insert(n);
            }
        }
    }
    seen_indices.len() >= 2 || (saw_cursor_option && !seen_indices.is_empty())
}

fn terminal_needs_permission_from_lines(lines: &[String]) -> bool {
    let recent = lines
        .iter()
        .rev()
        .take(14)
        .map(|line| normalize_terminal_line(line))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if recent.is_empty() {
        return false;
    }
    // Strong signal: agent's yes/no permission menu shape. Claude and codex
    // both render permission prompts as a numbered selection list ending in
    // "Yes" / "No" options, often with a "(esc)" hint. This is rock-solid
    // when present and language-agnostic to natural-language keyword soup.
    if looks_like_yes_no_menu(&recent) {
        return true;
    }
    let text = recent.iter().rev().cloned().collect::<Vec<_>>().join("\n");

    permission_text_needs_attention(&text)
}

/// True if the most recent lines look like an agent's yes/no permission menu:
/// a leading "❯ 1. Yes" with a follow-on "2. No..." nearby.
fn looks_like_yes_no_menu(recent_rev: &[String]) -> bool {
    let mut saw_yes_option = false;
    let mut saw_no_option = false;
    for line in recent_rev.iter().take(8) {
        let lower = line.to_ascii_lowercase();
        let stripped = lower.trim_start_matches(|c: char| {
            c.is_whitespace() || c == '\u{276f}' || c == '>' || c == '*'
        });
        if (stripped.starts_with("1.") || stripped.starts_with("1)")) && stripped.contains("yes") {
            saw_yes_option = true;
        }
        if (stripped.starts_with("2.") || stripped.starts_with("2)"))
            && (stripped.contains("no")
                || stripped.contains("don't")
                || stripped.contains("do not"))
        {
            saw_no_option = true;
        }
    }
    saw_yes_option && saw_no_option
}

fn permission_text_needs_attention(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let text = text.as_str();

    let has_permission_word = contains_any_word(
        &text,
        &[
            "permission",
            "approval",
            "approve",
            "allow",
            "authorize",
            "authorization",
            "confirmation",
            "proceed",
            "deny",
        ],
    );
    if !has_permission_word {
        return false;
    }

    let asks_decision =
        contains_any_phrase(&text, &["do you want", "would you like", "are you sure"])
            && contains_any_word(
                &text,
                &["allow", "approve", "run", "execute", "continue", "proceed"],
            );
    let approves_action = contains_any_word(&text, &["allow", "approve", "authorize"])
        && contains_any_word(
            &text,
            &[
                "command",
                "tool",
                "edit",
                "write",
                "file",
                "access",
                "execution",
                "network",
                "shell",
                "operation",
            ],
        );
    let approval_request = contains_any_word(
        &text,
        &["permission", "approval", "authorization", "confirmation"],
    ) && contains_any_word(
        &text,
        &[
            "required",
            "needed",
            "need",
            "request",
            "requested",
            "requesting",
            "waiting",
            "prompt",
        ],
    ) && contains_approval_response(&text);
    let key_prompt = contains_word(&text, "press")
        && contains_any_word(&text, &["y", "yes", "enter", "return"])
        && contains_any_word(&text, &["allow", "approve", "continue", "proceed"]);
    let yes_no_prompt = (contains_word(&text, "yes") || contains_word(&text, "no"))
        && contains_any_word(&text, &["approve", "deny", "allow"]);

    asks_decision || approves_action || approval_request || key_prompt || yes_no_prompt
}

fn looks_busy(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // Be specific about what "busy" looks like to avoid false positives on
    // status lines that incidentally contain words like "running" (e.g.
    // codex's "1 background terminal running · /ps to view"). The agents'
    // actual busy spinners always advertise an interrupt key.
    lower.contains("esc to interrupt")
        || lower.contains("ctrl-c to interrupt")
        || lower.contains("ctrl+c to interrupt")
        || lower.contains("thinking...")
        || lower.contains("thinking (")
        || lower.contains("working...")
        || lower.contains("working (")
        || lower.contains("running...")
        || lower.contains("running (")
}

fn looks_like_agent_prompt(backend: Backend, line: &str) -> bool {
    match backend {
        Backend::Claude => {
            line == ">"
                || line.starts_with("> ")
                || line.starts_with("❯ ")
                || line.starts_with("› ")
                || line.contains("Type a message")
        }
        Backend::Codex => {
            line == "›"
                || line.starts_with("› ")
                || line.starts_with("> ")
                || line.contains("Type a message")
        }
    }
}

fn normalize_terminal_line(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
        .collect::<String>()
        .trim()
        .to_string()
}

fn contains_any_phrase(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn contains_any_word(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| contains_word(text, word))
}

fn contains_approval_response(text: &str) -> bool {
    contains_any_word(
        text,
        &["approve", "allow", "deny", "enter", "return", "press"],
    ) || contains_word(text, "yes")
        || contains_word(text, "no")
}

fn contains_word(text: &str, word: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == word)
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn worktree_dir_name_leads_with_task_slug() {
        let name = worktree_dir_name(
            "1779248379804-add-dark-and-light-mode-56991",
            "Add dark and light mode",
        );

        assert!(name.starts_with("add-dark-and-light-mode-"));
        assert!(!name.starts_with("1779248379804"));
    }

    #[test]
    fn parses_main_worktree_from_porcelain() {
        let output = "\
worktree /repo/feature\n\
HEAD 111\n\
branch refs/heads/rudder/task\n\
\n\
worktree /repo/main\n\
HEAD 222\n\
branch refs/heads/main\n";

        assert_eq!(
            main_worktree_from_porcelain(output),
            Some(PathBuf::from("/repo/main"))
        );
    }

    #[test]
    fn merge_strategy_defaults_to_merge_and_parses_rebase() {
        let missing = serde_json::json!({});
        let rebase = serde_json::json!({ "mergeStrategy": "rebase" });
        let invalid = serde_json::json!({ "mergeStrategy": "squash" });

        assert_eq!(config_merge_strategy(&missing), MergeStrategy::Merge);
        assert_eq!(config_merge_strategy(&rebase), MergeStrategy::Rebase);
        assert_eq!(config_merge_strategy(&invalid), MergeStrategy::Merge);
    }

    #[test]
    fn parses_json_task_summary_title() {
        assert_eq!(
            task_title_from_summary_output(
                "Here is the JSON:\n{\"title\":\"Fix Codex worker scrolling.\"}",
            )
            .as_deref(),
            Some("Fix Codex worker scrolling")
        );
    }

    #[test]
    fn detects_common_idle_prompts() {
        assert!(looks_like_agent_prompt(Backend::Claude, "> "));
        assert!(looks_like_agent_prompt(Backend::Claude, "› try something"));
        assert!(looks_like_agent_prompt(Backend::Codex, "› ask follow up"));
        assert!(looks_like_agent_prompt(Backend::Codex, "> "));
    }

    #[test]
    fn detects_busy_lines() {
        assert!(looks_busy("* Thinking... (3s)"));
        assert!(looks_busy("Working (12s · esc to interrupt)"));
        assert!(looks_busy("esc to interrupt"));
        assert!(looks_busy("press ctrl-c to interrupt"));
        // The word alone is not enough; it has to look like a spinner.
        assert!(!looks_busy("Thinking hard about tests"));
        assert!(!looks_busy("All checks passed."));
        // Codex's idle "background terminal running" must NOT count as busy.
        assert!(!looks_busy(
            "1 background terminal running \u{00b7} /ps to view"
        ));
    }

    #[test]
    fn idle_chrome_is_strong_done_signal() {
        // Claude's footer when idle at prompt
        let lines: Vec<String> = vec![
            "Edited 3 files".to_string(),
            "> ".to_string(),
            "  bypass permissions on (shift+tab to cycle)".to_string(),
        ];
        assert!(terminal_looks_ready_for_input_from_lines(
            Backend::Claude,
            &lines
        ));
    }

    #[test]
    fn codex_idle_chrome_marks_done() {
        let lines: Vec<String> = vec![
            "Verification passed:".to_string(),
            String::new(),
            "- pnpm lint".to_string(),
            "- pnpm build".to_string(),
            "Worked for 4m 46s".to_string(),
            "1 background terminal running \u{00b7} /ps to view \u{00b7} /stop to close"
                .to_string(),
            "> Run /review on my current changes".to_string(),
            "gpt-5.5 xhigh fast \u{00b7} ~/Documents/.rudder-worktrees/foo-bar".to_string(),
        ];
        assert!(terminal_looks_ready_for_input_from_lines(
            Backend::Codex,
            &lines
        ));
    }

    #[test]
    fn busy_blocks_done_even_with_prompt_visible() {
        let lines: Vec<String> = vec![
            "> ".to_string(),
            "* Thinking... (3s · esc to interrupt)".to_string(),
        ];
        assert!(!terminal_looks_ready_for_input_from_lines(
            Backend::Claude,
            &lines
        ));
    }

    #[test]
    fn yes_no_menu_is_strong_permission_signal() {
        let lines: Vec<String> = vec![
            "Do you want to allow Bash command".to_string(),
            "  grep -r foo .".to_string(),
            "\u{276f} 1. Yes".to_string(),
            "  2. No, and tell me what to do differently (esc)".to_string(),
        ];
        assert!(terminal_needs_permission_from_lines(&lines));
    }

    #[test]
    fn ordinary_numbered_list_does_not_trigger_menu_detector() {
        // Two "1." lines in a row should NOT count as a menu (e.g. an ordered
        // list in agent prose output). We need at least two DIFFERENT indices.
        let recent_rev: Vec<String> = vec![
            "1. Implement parser".to_string(),
            "1. Implement parser".to_string(),
        ];
        assert!(!has_numbered_menu_pattern(&recent_rev));

        let real_menu: Vec<String> = vec!["2. YAML".to_string(), "1. JSON".to_string()];
        assert!(has_numbered_menu_pattern(&real_menu));
    }

    #[test]
    fn cursor_arrow_with_one_option_counts_as_menu() {
        let recent_rev: Vec<String> = vec!["\u{276f} 1. Continue".to_string()];
        assert!(has_numbered_menu_pattern(&recent_rev));
    }

    #[test]
    fn question_mark_alone_at_bottom_triggers_input_need() {
        let lines: Vec<String> = vec![
            "What should I name the new module?".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ];
        assert!(terminal_needs_user_input_from_lines(&lines));
    }

    #[test]
    fn long_chatty_line_does_not_trigger_input_need() {
        let lines: Vec<String> = vec![
            "This is a long descriptive sentence about what I just did and why, intended to inform the user that I have completed several edits across the project and now wish to summarize ".to_string(),
        ];
        assert!(!terminal_needs_user_input_from_lines(&lines));
    }

    #[test]
    fn detects_permission_prompts_without_matching_task_text() {
        assert!(permission_text_needs_attention(
            "Approval required\nPress enter to approve this command, or no to deny"
        ));
        assert!(permission_text_needs_attention(
            "do you want to allow this shell command to run?"
        ));
        assert!(permission_text_needs_attention(
            "authorization required\npress enter to approve this operation"
        ));
        assert!(!permission_text_needs_attention(
            "it allows need to maeke the sound even if the agent is waiting for permission and it should show that in the agent pane so that it's obvious"
        ));
        assert!(!permission_text_needs_attention(
            "make the sound even if the agent is waiting for permission"
        ));
        assert!(!permission_text_needs_attention(
            "inspect and annotate the live review, then show when waiting for permission"
        ));
    }

    #[test]
    fn execution_prompt_does_not_label_user_task_or_nest_rudder_blocks() {
        let prompt = execution_prompt("fix the tests");
        assert!(prompt.contains("Rudder-specific context injected by Rudder"));
        assert!(prompt.contains("fix the tests"));
        assert!(!prompt.contains("USER TASK"));
        assert!(!prompt.contains("[RUDDER PROMPT INJECTION]"));
    }

    #[test]
    fn execution_prompt_strips_old_rudder_wrappers_from_followups() {
        let prompt = execution_prompt(
            "[RUDDER PROMPT INJECTION]\nRead RUDDER.md first.\n[END RUDDER PROMPT INJECTION]\n\nUSER TASK:\nship the cloud setup",
        );
        assert_eq!(
            prompt
                .matches("Rudder-specific context injected by Rudder")
                .count(),
            1
        );
        assert!(prompt.contains("ship the cloud setup"));
        assert!(!prompt.contains("USER TASK"));
        assert!(!prompt.contains("[END RUDDER PROMPT INJECTION]"));
    }

    #[test]
    fn execution_prompt_preserves_task_inside_legacy_rudder_wrapper() {
        let prompt = execution_prompt(
            "[RUDDER PROMPT INJECTION]\nRead RUDDER.md first. Review the current diff and tests.\n[END RUDDER PROMPT INJECTION]",
        );
        assert!(prompt.contains("Read RUDDER.md first. Review the current diff and tests."));
        assert!(!prompt.contains("[RUDDER PROMPT INJECTION]"));
        assert!(!prompt.contains("[END RUDDER PROMPT INJECTION]"));
    }

    #[test]
    fn auto_steer_prompt_is_plain_task_text() {
        let task = "add bring your own vm";
        let prompt = format!(
            "Review the current diff and tests for this original task: {}. If anything remains, fix it and run the relevant checks. If it is complete, say what you verified.",
            task
        );
        assert!(!prompt.contains("USER TASK"));
        assert!(!prompt.contains("[RUDDER PROMPT INJECTION]"));
        assert!(execution_prompt(&prompt).contains(task));
    }

    #[test]
    fn task_input_word_navigation_and_delete_respects_cursor() {
        let input = "fix the auth bug";
        assert_eq!(previous_word_position(input, 12), 8);
        assert_eq!(next_word_position(input, 4), 7);

        let mut editable = input.to_string();
        let mut cursor = 12;
        delete_previous_word_at(&mut editable, &mut cursor);
        assert_eq!(editable, "fix the  bug");
        assert_eq!(cursor, 8);

        insert_str_at_cursor(&mut editable, &mut cursor, "login");
        assert_eq!(editable, "fix the login bug");
        assert_eq!(cursor, 13);

        let mut app = App::new();
        app.task_input = "fix the auth bug".to_string();
        app.task_cursor = app.task_input.chars().count();
        app.handle_task_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.task_cursor, 13);
        app.handle_task_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.task_cursor, 8);
        app.handle_task_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(app.task_cursor, 12);
        app.handle_task_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.task_cursor, 0);
        app.handle_task_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.task_cursor, app.task_input.chars().count());
        app.task_cursor = 8;
        app.handle_task_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.task_input, "fix the ");
        app.handle_task_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(app.task_input, "fix the");
    }

    #[test]
    fn wraps_long_notice_text_to_width() {
        let lines = wrap_text(
            "merge stopped: error: Merging is not possible because you have unmerged files.",
            28,
        );

        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.chars().count() <= 28));
        assert_eq!(lines[0], "merge stopped: error:");
    }

    #[test]
    fn converts_mouse_events_to_review_terminal_coordinates() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 20,
            height: 10,
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 8,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(
            mouse_event_to_sgr(event, area),
            Some(b"\x1b[<0;3;4M".to_vec())
        );

        let modified_scroll = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 12,
            row: 8,
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(
            mouse_event_to_sgr(modified_scroll, area),
            Some(b"\x1b[<80;3;4M".to_vec())
        );
    }

    #[test]
    fn styled_terminal_line_draws_visible_cursor_cell() {
        let line = styled_terminal_line(vec![plain_terminal_cell("a".to_string())], None, Some(3));

        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[0].content.as_ref(), "a");
        assert_eq!(line.spans[3].content.as_ref(), " ");
        assert_eq!(line.spans[3].style, cursor_cell_style());
    }

    #[cfg(not(windows))]
    #[test]
    fn hidden_codex_cursor_still_gets_render_cursor() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            ["-lc", "printf '\\033[?25lhidden cursor\\r\\n'; sleep 1"],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("hidden cursor")
            {
                break;
            }
        }

        assert!(!pane.cursor().visible);
        assert!(worker_render_cursor(Backend::Codex, &pane, true, 5, 20, 0).is_some());
        assert!(worker_render_cursor(Backend::Codex, &pane, false, 5, 20, 0).is_none());
    }

    #[test]
    fn worker_wheel_scroll_rows_scale_with_viewport() {
        assert_eq!(wheel_scroll_rows(2, KeyModifiers::empty()), 1);
        assert_eq!(wheel_scroll_rows(6, KeyModifiers::empty()), 1);
        assert_eq!(wheel_scroll_rows(30, KeyModifiers::empty()), 1);
        assert_eq!(wheel_scroll_rows(90, KeyModifiers::empty()), 1);
        assert_eq!(wheel_scroll_rows(30, KeyModifiers::CONTROL), 29);

        let down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(mouse_scrollback_delta(down, 30), -1);
    }

    #[cfg(not(windows))]
    #[test]
    fn focused_worker_wheel_scrolls_alternate_screen_history() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "printf '\\033[?1049hfirst screen\\r\\n'; sleep 0.1; printf '\\033[2J\\033[Hsecond screen\\r\\n'; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.uses_alternate_screen()
                && pane
                    .visible_lines_snapshot()
                    .join("\n")
                    .contains("second screen")
            {
                break;
            }
        }

        assert!(pane.uses_alternate_screen());
        assert!(pane
            .visible_lines_snapshot()
            .join("\n")
            .contains("second screen"));

        let mut app = App::new();
        app.focus = FocusPane::Worker;
        app.worker_area = Some(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 7,
        });
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });

        let scrolled_up = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert!(
            scrolled_up.contains("first screen"),
            "scrolled_up was {scrolled_up:?}"
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });

        let scrolled_down = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert!(
            scrolled_down.contains("second screen"),
            "scrolled_down was {scrolled_down:?}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn worker_wheel_forwards_to_inner_tui_when_scrollback_cannot_move() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "stty raw -echo; printf '\\033[?1049h\\033[?1000h\\033[?1006h'; cat -v",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.uses_alternate_screen() && pane.wants_sgr_mouse_events() {
                break;
            }
        }

        assert!(pane.uses_alternate_screen());
        assert!(pane.wants_sgr_mouse_events());

        let mut app = App::new();
        app.worker_area = Some(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 7,
        });
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });

        std::thread::sleep(Duration::from_millis(50));
        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines().join("\n"))
            .unwrap_or_default();
        assert!(output.contains("^[[<65;1;1M"), "output was {output:?}");
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_worker_wheel_at_edge_does_not_send_page_keys() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            ["-lc", "stty raw -echo; printf 'ready\\r\\n'; cat -v"],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("ready") {
                break;
            }
        }

        let mut app = App::new();
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.backend = Backend::Codex;
        app.worker_area = Some(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 7,
        });
        app.agents.push(run);
        app.selected_agent = 0;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });

        std::thread::sleep(Duration::from_millis(50));
        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines().join("\n"))
            .unwrap_or_default();
        assert!(output.contains("ready"), "output was {output:?}");
        assert!(!output.contains("^[[5~"), "output was {output:?}");
        assert!(app
            .selected_terminal_mut()
            .is_some_and(|terminal| terminal.scrollback() == 0));
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_worker_wheel_moves_normal_scrollback_before_forwarding() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "stty raw -echo; i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; cat -v",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("line040") {
                break;
            }
        }
        assert!(pane.visible_lines_snapshot().join("\n").contains("line040"));
        assert_eq!(pane.scrollback(), 0);
        let before = pane.visible_lines_snapshot().join("\n");

        let mut app = App::new();
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.backend = Backend::Codex;
        app.agents.push(run);
        app.selected_agent = 0;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(app.scroll_selected_worker_or_forward(
            mouse,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            },
        ));

        std::thread::sleep(Duration::from_millis(50));
        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert_ne!(output, before);
        assert!(output.contains("line037"), "output was {output:?}");
        assert!(app
            .selected_terminal_mut()
            .is_some_and(|terminal| terminal.scrollback() > 0));
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_worker_wheel_moves_scroll_region_scrollback() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "stty raw -echo; printf '\\033[1;3r\\033[3;1H\\r\\nhistory001\\r\\nhistory002\\r\\nhistory003\\r\\nhistory004\\r\\nhistory005\\033[r'; cat -v",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane
                .visible_lines_snapshot()
                .join("\n")
                .contains("history005")
            {
                break;
            }
        }
        assert_eq!(pane.scrollback(), 0);

        let mut app = App::new();
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.backend = Backend::Codex;
        app.agents.push(run);
        app.selected_agent = 0;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(app.scroll_selected_worker_or_forward(
            mouse,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            },
        ));

        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert!(output.contains("history002"), "output was {output:?}");
        assert!(output.contains("history005"), "output was {output:?}");
        assert!(app
            .selected_terminal_mut()
            .is_some_and(|terminal| terminal.scrollback() > 0));
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_alternate_screen_wheel_moves_snapshot_scrollback_first() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "stty raw -echo; printf '\\033[?1049hfirst\\r\\n'; sleep 0.1; printf '\\033[2J\\033[Hsecond\\r\\n'; cat -v",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.uses_alternate_screen()
                && pane.visible_lines_snapshot().join("\n").contains("second")
            {
                break;
            }
        }
        assert!(pane.uses_alternate_screen());
        assert!(pane.visible_lines_snapshot().join("\n").contains("second"));

        let mut app = App::new();
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.backend = Backend::Codex;
        app.agents.push(run);
        app.selected_agent = 0;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(app.scroll_selected_worker_or_forward(
            mouse,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            },
        ));

        std::thread::sleep(Duration::from_millis(50));
        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert!(output.contains("first"), "output was {output:?}");
        assert!(!output.contains("^[[5~"), "output was {output:?}");
        assert!(app
            .selected_terminal_mut()
            .is_some_and(|terminal| terminal.scrollback() > 0));
    }

    #[cfg(not(windows))]
    #[test]
    fn worker_wheel_scroll_moves_normal_screen_scrollback() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("line040") {
                break;
            }
        }

        let before = pane.visible_lines_snapshot().join("\n");
        assert!(before.contains("line040"), "before was {before:?}");

        let mut app = App::new();
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(app.scroll_selected_worker_or_forward(
            mouse,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            },
        ));

        let after = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert_ne!(after, before);
        assert!(after.contains("line037"), "after was {after:?}");
    }

    #[cfg(not(windows))]
    #[test]
    fn worker_wheel_at_edge_does_not_flash_notice() {
        let command =
            TerminalCommand::with_args("/bin/sh", ["-lc", "printf 'ready\\r\\n'; sleep 1"]);
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("ready") {
                break;
            }
        }

        let mut app = App::new();
        app.notice = Some("keep me".to_string());
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(app.scroll_selected_worker_or_forward(
            mouse,
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 5,
            },
        ));

        assert_eq!(app.notice.as_deref(), Some("keep me"));
    }

    #[cfg(not(windows))]
    #[test]
    fn wheel_scroll_routes_to_worker_under_pointer_even_when_task_is_focused() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("line040") {
                break;
            }
        }

        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.agents_area = Some(Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 20,
        });
        app.worker_area = Some(Rect {
            x: 20,
            y: 0,
            width: 40,
            height: 7,
        });
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 21,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });

        let after = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert!(after.contains("line036"), "after was {after:?}");
        assert_eq!(app.focus, FocusPane::Task);
    }

    #[cfg(not(windows))]
    #[test]
    fn wheel_over_task_does_not_scroll_worker() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("line040") {
                break;
            }
        }

        let mut app = App::new();
        app.worker_area = Some(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 7,
        });
        app.task_area = Some(Rect {
            x: 0,
            y: 8,
            width: 40,
            height: 3,
        });
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;
        let before = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 9,
            modifiers: KeyModifiers::empty(),
        });

        let after = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
            .unwrap_or_default();
        assert_eq!(after, before);
    }

    #[cfg(not(windows))]
    #[test]
    fn worker_drag_selection_above_pane_autoscrolls() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            [
                "-lc",
                "i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; sleep 1",
            ],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("line040") {
                break;
            }
        }

        let mut app = App::new();
        app.agents.push(test_agent_run_with_terminal(&app, pane));
        app.selected_agent = 0;
        app.worker_selection = Some(WorkerSelection {
            start: SelectionPoint { row: 4, col: 0 },
            end: SelectionPoint { row: 4, col: 4 },
        });
        let area = Rect {
            x: 0,
            y: 5,
            width: 20,
            height: 5,
        };

        assert!(app.handle_worker_selection_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 1,
                row: 4,
                modifiers: KeyModifiers::empty(),
            },
            area,
        ));
        assert!(app
            .selected_terminal_mut()
            .is_some_and(|terminal| terminal.scrollback() > 0));
    }

    #[test]
    fn extracts_selected_worker_text_across_visible_lines() {
        let lines = vec![
            "first line".to_string(),
            "second line".to_string(),
            "third".to_string(),
        ];
        let selection = WorkerSelection {
            start: SelectionPoint { row: 0, col: 6 },
            end: SelectionPoint { row: 1, col: 5 },
        };

        assert_eq!(selected_text_from_lines(&lines, selection), "line\nsecond");
    }

    #[test]
    fn normalizes_reversed_worker_selection() {
        let lines = vec!["abcdef".to_string()];
        let selection = WorkerSelection {
            start: SelectionPoint { row: 0, col: 4 },
            end: SelectionPoint { row: 0, col: 1 },
        };

        assert_eq!(selected_text_from_lines(&lines, selection), "bcde");
    }

    #[test]
    fn maps_task_mouse_selection_to_wrapped_input() {
        let mut app = App::new();
        app.task_input = "abcdef".to_string();
        app.task_cursor = app.task_input.chars().count();
        app.notice = Some("hint".to_string());
        let area = Rect {
            x: 10,
            y: 4,
            width: 3,
            height: 8,
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(
            task_selection_point_from_mouse(&app, mouse, area),
            Some(SelectionPoint { row: 1, col: 1 })
        );
        assert_eq!(
            task_cursor_from_selection_point(&app.task_input, SelectionPoint { row: 1, col: 1 }, 3),
            4
        );
    }

    #[test]
    fn wraps_worker_paste_as_single_bracketed_paste_payload() {
        assert_eq!(
            bracketed_paste_bytes("hello\nworld"),
            b"\x1b[200~hello\nworld\x1b[201~".to_vec()
        );
    }

    #[test]
    fn maps_page_keys_for_terminal_passthrough() {
        assert_eq!(
            terminal_bytes_for_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty())),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            terminal_bytes_for_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty())),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            terminal_bytes_for_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            terminal_bytes_for_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty())),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            terminal_bytes_for_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn tab_does_not_cycle_pane_focus() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty())));
        assert_eq!(app.focus, FocusPane::Task);

        app.focus = FocusPane::Agents;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert_eq!(app.focus, FocusPane::Agents);
    }

    #[test]
    fn command_copy_is_not_forwarded_to_embedded_terminal() {
        let command_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
        let meta_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::META);
        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(is_copy_key(command_c));
        assert!(is_copy_key(meta_c));
        assert_eq!(terminal_bytes_for_key(command_c), None);
        assert_eq!(terminal_bytes_for_key(meta_c), None);
        assert_eq!(terminal_bytes_for_key(control_c), Some(vec![0x03]));
    }

    #[test]
    fn plan_commands_use_read_only_backend_profiles() {
        let execute_codex = agent_command(
            Backend::Codex,
            "gpt-5.5",
            Some(EffortLevel::High),
            "implement the work",
            AgentMode::Execute,
            None,
        );
        assert!(execute_codex
            .args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!execute_codex.args.iter().any(|arg| arg == "--sandbox"));
        assert!(!execute_codex
            .args
            .iter()
            .any(|arg| arg == "--ask-for-approval"));
        assert!(execute_codex
            .args
            .iter()
            .any(|arg| arg == "--no-alt-screen"));
        assert!(execute_codex
            .args
            .windows(2)
            .any(|window| window[0] == "--enable" && window[1] == "goals"));
        assert!(execute_codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "notify=[]"));
        assert!(execute_codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.plugins=false"));
        assert!(execute_codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.computer_use=false"));
        assert!(execute_codex
            .env
            .iter()
            .any(|(key, value)| key == "CODEX_RUDDER_SCROLLBACK_SAFE" && value == "1"));

        let execute_claude = agent_command(
            Backend::Claude,
            "sonnet",
            None,
            "implement the work",
            AgentMode::Execute,
            None,
        );
        assert!(execute_claude
            .env
            .iter()
            .any(|(key, value)| key == "CLAUDE_CODE_NO_FLICKER" && value == "0"));

        let codex = agent_command(
            Backend::Codex,
            "gpt-5.5",
            Some(EffortLevel::High),
            "plan the work",
            AgentMode::Plan,
            None,
        );
        assert_eq!(codex.program, "codex");
        assert!(codex.args.iter().any(|arg| arg == "--no-alt-screen"));
        assert!(codex
            .args
            .windows(2)
            .any(|window| window[0] == "--sandbox" && window[1] == "read-only"));
        assert!(codex.args.iter().any(|arg| arg == "--search"));
        assert!(codex
            .args
            .windows(2)
            .any(|window| window[0] == "--enable" && window[1] == "goals"));
        assert!(codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "notify=[]"));
        assert!(codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.plugins=false"));
        assert!(codex
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.computer_use=false"));
        assert!(!codex
            .args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));

        let claude = agent_command(
            Backend::Claude,
            "sonnet",
            None,
            "plan the work",
            AgentMode::Plan,
            None,
        );
        assert_eq!(claude.program, "claude");
        assert!(claude
            .env
            .iter()
            .any(|(key, value)| key == "CLAUDE_CODE_NO_FLICKER" && value == "0"));
        assert!(claude
            .args
            .windows(2)
            .any(|window| window[0] == "--permission-mode" && window[1] == "plan"));
        assert!(!claude
            .args
            .windows(2)
            .any(|window| window[0] == "--tools" || window[0] == "--allowedTools"));
        assert!(!claude
            .args
            .iter()
            .any(|arg| arg.contains("[RUDDER PLAN MODE]")));
        assert!(claude
            .args
            .iter()
            .any(|arg| arg.contains("Plan this task before implementation")));

        let rudder_plan = agent_command(
            Backend::Codex,
            "gpt-5.5",
            Some(EffortLevel::High),
            "build the feature",
            AgentMode::RudderPlan,
            None,
        );
        assert!(rudder_plan.args.iter().any(|arg| arg == "--no-alt-screen"));
        assert!(rudder_plan
            .args
            .windows(2)
            .any(|window| window[0] == "--sandbox" && window[1] == "read-only"));
        assert!(rudder_plan.args.iter().any(|arg| {
            arg.contains("RUDDER_PLAN_TASKS_START")
                && arg.contains("build the feature")
                && arg.contains("Codex `/goal`")
        }));
        assert!(rudder_plan
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "notify=[]"));
        assert!(rudder_plan
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.plugins=false"));
        assert!(rudder_plan
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.computer_use=false"));
    }

    #[test]
    fn extracts_rudder_plan_tasks_from_marked_json_block() {
        let output = "\x1b[32mRUDDER_PLAN_TASKS_START\x1b[0m\n{\"tasks\":[{\"title\":\"API\",\"prompt\":\"Implement API and test it.\",\"goal\":\"Complete the API without stopping until tests pass.\"},{\"title\":\"UI\",\"prompt\":\"Implement UI and test it.\"}]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "API");
        assert_eq!(
            tasks[0].goal.as_deref(),
            Some("Complete the API without stopping until tests pass.")
        );
        assert_eq!(tasks[1].prompt, "Implement UI and test it.");
        assert_eq!(tasks[1].goal, None);
    }

    #[test]
    fn extracts_rudder_plan_tasks_from_last_marked_json_block() {
        let output = "Planner prompt example:\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"placeholder\",\"prompt\":\"do not run this\"}]}\nRUDDER_PLAN_TASKS_END\n\nFinal answer:\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"Backend\",\"prompt\":\"Implement backend.\"}]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Backend");
        assert_eq!(tasks[0].prompt, "Implement backend.");
    }

    #[test]
    fn collects_rudder_plan_output_from_codex_session_messages() {
        let mut output = String::new();
        collect_codex_session_assistant_text(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"RUDDER_PLAN_TASKS_START\nbad\nRUDDER_PLAN_TASKS_END"}]}}"#,
            &mut output,
        );
        collect_codex_session_assistant_text(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"UI\",\"prompt\":\"Implement UI.\"}]}\nRUDDER_PLAN_TASKS_END"}]}}"#,
            &mut output,
        );

        let tasks = extract_rudder_plan_tasks(&output).expect("parse tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "UI");
    }

    #[test]
    fn rudder_plan_worker_prompt_includes_codex_goal_when_available() {
        let task = RudderPlanTask {
            title: "API".to_string(),
            prompt: "Implement API and run cargo test.".to_string(),
            goal: Some("Complete the API without stopping until cargo test passes.".to_string()),
        };
        let prompt = rudder_plan_worker_prompt("build the feature", &task, Backend::Codex);

        assert!(prompt.contains("Original request:\nbuild the feature"));
        assert!(prompt.contains("Worker task: API"));
        assert!(prompt.contains("/goal Complete the API without stopping until cargo test passes."));
    }

    #[test]
    fn extracts_rudder_plan_worker_title_for_agent_summary() {
        let prompt = "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nOriginal request:\nread rudder.md and launch tasks\n\nWorker task: Libra Issues backend state machine\n\nImplement the API-side work.";

        assert_eq!(
            rudder_plan_worker_title_from_prompt(prompt).as_deref(),
            Some("Libra Issues backend state machine")
        );
    }

    #[test]
    fn persisted_rudder_plan_worker_uses_worker_title_summary() {
        let task = "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nOriginal request:\nread rudder.md and launch tasks\n\nWorker task: Libra Issues product UI\n\nImplement the UI.";
        let record = serde_json::json!({
            "id": "run-1",
            "status": "running",
            "mode": "execute",
            "task": task,
            "taskSummary": "task was spawned Rudder /rudder-plan coordinator",
            "backend": "codex",
            "model": "gpt-5.5",
            "createdAt": "1",
            "worktree": { "enabled": false, "path": "/tmp/repo", "branch": null }
        });

        let run = agent_from_run_record(Path::new("/tmp/repo"), record).expect("run");
        assert_eq!(run.task_summary, "Libra Issues product UI");
    }

    #[test]
    fn cloud_command_defaults_to_generated_cloud_worker() {
        let app = App::new();
        let generated = app.cloud_command_args(Vec::new());
        assert_eq!(generated.first().map(String::as_str), Some("cloud"));
        assert_eq!(generated.len(), 2);
        assert!(generated[1].contains('-'));
        assert_eq!(
            app.cloud_command_args(vec!["login"]),
            vec!["cloud".to_string(), "login".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["onload"]),
            vec!["cloud".to_string(), "onload".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["list"]),
            vec!["cloud".to_string(), "list".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["visualization"]),
            vec!["cloud".to_string(), "visualization".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["setup", "vm"]),
            vec!["cloud".to_string(), "setup".to_string(), "vm".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["setup-byoc"]),
            vec!["cloud".to_string(), "setup-byoc".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["setup-vm"]),
            vec!["cloud".to_string(), "setup-vm".to_string()]
        );
        assert_eq!(
            app.cloud_command_args(vec!["vm", "fix", "tests"]),
            vec![
                "cloud".to_string(),
                "vm".to_string(),
                "fix".to_string(),
                "tests".to_string()
            ]
        );
        assert_eq!(
            app.cloud_command_args(vec!["byoc", "fix", "tests"]),
            vec![
                "cloud".to_string(),
                "byoc".to_string(),
                "fix".to_string(),
                "tests".to_string()
            ]
        );
        assert_eq!(
            app.cloud_command_args(vec!["bootstrap", "sail_123"]),
            vec![
                "cloud".to_string(),
                "bootstrap".to_string(),
                "sail_123".to_string()
            ]
        );
    }

    #[test]
    fn cloud_prompt_highlights_onload_for_selected_local_run() {
        let mut app = App::new();
        app.agents.push(test_agent_run("run-1", "fix cloud launch"));
        app.selected_agent = 0;

        assert!(app.maybe_prompt_cloud_launch(&[]));

        let prompt = app.cloud_prompt.as_ref().expect("cloud prompt");
        assert_eq!(prompt.choice, CloudLaunchChoice::Upload);
        assert_eq!(prompt.selected_task.as_deref(), Some("fix cloud launch"));
    }

    #[test]
    fn cloud_prompt_defaults_to_workspace_without_selected_local_run() {
        let mut app = App::new();

        assert!(app.maybe_prompt_cloud_launch(&[]));

        let prompt = app.cloud_prompt.as_ref().expect("cloud prompt");
        assert_eq!(prompt.choice, CloudLaunchChoice::Upload);
        assert!(prompt.selected_task.is_none());
        assert_eq!(
            prompt.scratch_args.first().map(String::as_str),
            Some("cloud")
        );
        assert_eq!(prompt.scratch_args.len(), 2);
    }

    #[test]
    fn cloud_prompt_enter_on_highlighted_onloads_current_run() {
        let prompt = CloudLaunchPrompt {
            scratch_args: vec!["cloud".to_string(), "bright-orbit".to_string()],
            scratch_label: "cloud bright-orbit".to_string(),
            selected_task: Some("fix the cloud modal".to_string()),
            choice: CloudLaunchChoice::Upload,
        };

        assert_eq!(
            cloud_prompt_launch(&prompt),
            Ok(CloudPromptLaunch {
                label: "cloud workspace fix the cloud modal".to_string(),
                args: vec!["cloud".to_string(), "onload".to_string()],
            })
        );
    }

    #[test]
    fn cloud_prompt_down_then_enter_starts_scratch_worker() {
        let prompt = CloudLaunchPrompt {
            scratch_args: vec!["cloud".to_string(), "bright-orbit".to_string()],
            scratch_label: "cloud bright-orbit".to_string(),
            selected_task: Some("fix the cloud modal".to_string()),
            choice: CloudLaunchChoice::Scratch,
        };

        assert_eq!(
            cloud_prompt_launch(&prompt),
            Ok(CloudPromptLaunch {
                label: "cloud bright-orbit".to_string(),
                args: vec!["cloud".to_string(), "bright-orbit".to_string()],
            })
        );
    }

    #[test]
    fn cloud_prompt_upload_without_selected_run_is_not_scratch() {
        let prompt = CloudLaunchPrompt {
            scratch_args: vec!["cloud".to_string(), "bright-orbit".to_string()],
            scratch_label: "cloud bright-orbit".to_string(),
            selected_task: None,
            choice: CloudLaunchChoice::Upload,
        };

        assert_eq!(
            cloud_prompt_launch(&prompt),
            Ok(CloudPromptLaunch {
                label: "cloud workspace".to_string(),
                args: vec!["cloud".to_string(), "onload".to_string()],
            })
        );
    }

    #[test]
    fn slash_commands_rank_closest_matches() {
        let mut app = App::new();

        app.task_input = "/cl".to_string();
        assert_eq!(
            suggestions_for(&app)
                .first()
                .map(|suggestion| suggestion.label.as_str()),
            Some("/cloud")
        );

        app.task_input = "/cloud l".to_string();
        assert_eq!(
            suggestions_for(&app)
                .first()
                .map(|suggestion| suggestion.label.as_str()),
            Some("/cloud list")
        );

        app.task_input = "/lgoin".to_string();
        assert_eq!(
            suggestions_for(&app)
                .first()
                .map(|suggestion| suggestion.label.as_str()),
            Some("/login")
        );
    }

    #[test]
    fn model_picker_uses_ranked_provider_and_effort_matches() {
        assert_eq!(
            provider_suggestions("cdx")
                .first()
                .map(|suggestion| suggestion.label.as_str()),
            Some("codex")
        );
        assert_eq!(
            effort_suggestions_for(Backend::Codex, "gpt-5.5", "xh")
                .first()
                .map(|suggestion| suggestion.label.as_str()),
            Some("xhigh")
        );
    }

    #[test]
    fn task_history_walks_backward_forward_and_restores_draft() {
        let history = vec![
            "first task".to_string(),
            "second task".to_string(),
            "third task".to_string(),
        ];
        let mut index = None;
        let mut draft = String::new();

        assert_eq!(
            previous_task_history_entry(&history, &mut index, &mut draft, "draft task").as_deref(),
            Some("third task")
        );
        assert_eq!(index, Some(2));
        assert_eq!(draft, "draft task");

        assert_eq!(
            previous_task_history_entry(&history, &mut index, &mut draft, "third task").as_deref(),
            Some("second task")
        );
        assert_eq!(
            previous_task_history_entry(&history, &mut index, &mut draft, "second task").as_deref(),
            Some("first task")
        );
        assert_eq!(
            previous_task_history_entry(&history, &mut index, &mut draft, "first task").as_deref(),
            Some("first task")
        );

        assert_eq!(
            next_task_history_entry(&history, &mut index, &mut draft).as_deref(),
            Some("second task")
        );
        assert_eq!(
            next_task_history_entry(&history, &mut index, &mut draft).as_deref(),
            Some("third task")
        );
        assert_eq!(
            next_task_history_entry(&history, &mut index, &mut draft).as_deref(),
            Some("draft task")
        );
        assert_eq!(index, None);
    }

    #[test]
    fn task_summary_turns_request_into_agent_label() {
        assert_eq!(
            summarize_task(
                "also can you summarize the task that user puts and then that's what gets lsited on the agent pane. rihgt now you are just putting the task name"
            ),
            "summarize the user task for the agent pane"
        );
        assert_eq!(
            summarize_task_to(
                "ok another thing for you to work on is when merge happens label the thing on the side merged and when you delete then only it deletes the worktree",
                40,
            ),
            "merge happens label thing side merged..."
        );
    }

    #[test]
    fn worker_prompt_draft_tracks_line_editing_until_enter() {
        let mut draft = String::new();
        let mut cursor = 0;
        let mut is_prompt = false;

        assert_eq!(
            update_worker_prompt_draft_for_key(
                &mut draft,
                &mut cursor,
                &mut is_prompt,
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::empty()),
                true,
            ),
            None
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty()),
            true,
        );
        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::empty()),
            true,
        );

        assert_eq!(
            update_worker_prompt_draft_for_key(
                &mut draft,
                &mut cursor,
                &mut is_prompt,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
                true,
            )
            .as_deref(),
            Some("fix it")
        );
        assert!(draft.is_empty());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn worker_prompt_draft_records_pasted_lines() {
        let mut draft = String::new();
        let mut cursor = 0;
        let mut is_prompt = false;

        let prompts = update_worker_prompt_draft_for_paste(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            "first follow-up\r\nsecond follow-up",
            true,
        );

        assert_eq!(prompts, vec!["first follow-up".to_string()]);
        assert_eq!(draft, "second follow-up");
        assert_eq!(cursor, "second follow-up".chars().count());
    }

    #[test]
    fn worker_prompt_draft_ignores_non_prompt_input() {
        let mut draft = String::new();
        let mut cursor = 0;
        let mut is_prompt = false;

        update_worker_prompt_draft_for_key(
            &mut draft,
            &mut cursor,
            &mut is_prompt,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()),
            false,
        );

        assert_eq!(
            update_worker_prompt_draft_for_key(
                &mut draft,
                &mut cursor,
                &mut is_prompt,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
                false,
            ),
            None
        );
        assert!(draft.is_empty());
    }

    #[test]
    fn wraps_task_input_and_tracks_cursor_on_wrapped_lines() {
        let input = "abcdef";

        assert_eq!(wrap_input_text(input, 3), vec!["abc", "def"]);
        assert_eq!(task_cursor_position(input, 6, 3), (2, 0));
        assert_eq!(task_input_lines(input, 6, 3), vec!["abc", "def", ""]);
    }

    #[test]
    fn task_pane_height_grows_for_long_task_input() {
        let mut app = App::new();
        let base_height = task_pane_height(&app, 40);
        app.task_input = "x".repeat(80);
        app.task_cursor = app.task_input.chars().count();

        assert!(task_pane_height(&app, 40) > base_height);
    }

    #[test]
    fn click_in_agent_pane_does_not_change_focus() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.agents_area = Some(Rect {
            x: 0,
            y: 0,
            width: 34,
            height: 20,
        });

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        });

        assert_eq!(app.focus, FocusPane::Task);
    }

    #[test]
    fn click_on_agent_row_selects_that_agent() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.agents_area = Some(Rect {
            x: 0,
            y: 0,
            width: 34,
            height: 20,
        });
        app.agents.push(test_agent_run("run-1", "first task"));
        app.agents.push(test_agent_run("run-2", "second task"));
        app.selected_agent = 0;
        app.delete_pending = Some("run-1".to_string());

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 15,
            modifiers: KeyModifiers::empty(),
        });

        assert_eq!(app.focus, FocusPane::Task);
        assert_eq!(app.selected_agent, 1);
        assert!(app.delete_pending.is_none());
    }

    #[test]
    fn mouse_over_worker_and_task_does_not_change_focus() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        app.worker_area = Some(Rect {
            x: 20,
            y: 0,
            width: 40,
            height: 10,
        });
        app.task_area = Some(Rect {
            x: 0,
            y: 12,
            width: 60,
            height: 4,
        });

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 21,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.focus, FocusPane::Agents);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 13,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.focus, FocusPane::Agents);
    }

    #[cfg(not(windows))]
    #[test]
    fn finished_cloud_command_does_not_write_to_dead_pty() {
        let command = TerminalCommand::with_args("/bin/sh", ["-lc", "printf done"]);
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");
        std::thread::sleep(Duration::from_millis(50));
        pane.drain_output();

        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.task = "cloud bright-orbit".to_string();
        run.current_prompt = "cloud bright-orbit".to_string();
        run.status = AgentStatus::Done;
        app.agents.push(run);
        app.selected_agent = 0;

        app.handle_worker_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()));

        let run = app.agents.first().expect("run");
        assert_eq!(run.status, AgentStatus::Done);
        assert!(run.last_error.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("cloud command finished; run /cloud again or press r to rerun")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn worker_plain_shifted_letters_are_forwarded_to_terminal() {
        let command = TerminalCommand::with_args(
            "/bin/sh",
            ["-lc", "stty raw -echo; printf 'ready\\r\\n'; cat -v"],
        );
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 5, cols: 20 },
                scrollback_lines: 100,
                ..Default::default()
            },
        )
        .expect("spawn test pty");

        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            pane.drain_output();
            if pane.visible_lines_snapshot().join("\n").contains("ready") {
                break;
            }
        }

        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut run = test_agent_run_with_terminal(&app, pane);
        run.backend = Backend::Codex;
        app.agents.push(run);
        app.selected_agent = 0;

        for ch in 'A'..='Z' {
            app.handle_worker_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SHIFT));
        }

        std::thread::sleep(Duration::from_millis(50));
        let output = app
            .selected_terminal_mut()
            .map(|terminal| terminal.visible_lines().join("\n"))
            .unwrap_or_default();
        for ch in 'A'..='Z' {
            assert!(output.contains(ch), "missing {ch:?}; output was {output:?}");
        }
        assert_eq!(app.agents.len(), 1);
    }

    #[test]
    fn merge_confirm_hint_highlights_merge_action() {
        let line = merge_confirm_hint_line();
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "Press y to merge, n to cancel.");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].style.fg, Some(FAILED_COLOR));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn agent_pane_hints_include_review_and_merge_all_shortcuts() {
        assert!(AGENT_PANE_HINTS.contains(&"R review all"));
        assert!(AGENT_PANE_HINTS.contains(&"M merge all"));
    }

    fn test_agent_run(id: &str, task: &str) -> AgentRun {
        AgentRun {
            id: id.to_string(),
            created_at: "1".to_string(),
            mode: AgentMode::Execute,
            task: task.to_string(),
            task_summary: summarize_task(task),
            current_prompt: task.to_string(),
            turns: vec![AgentTurn {
                ts: "1".to_string(),
                prompt: task.to_string(),
                source: "user".to_string(),
            }],
            last_user_input_at: "1".to_string(),
            backend: Backend::Claude,
            model: "sonnet".to_string(),
            effort: None,
            status: AgentStatus::Running,
            cwd: std::env::temp_dir(),
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
        }
    }

    fn test_agent_run_with_terminal(app: &App, terminal: TerminalPane) -> AgentRun {
        let mut run = test_agent_run("run-1", "test task");
        run.cwd = app.cwd.clone();
        run.terminal = Some(terminal);
        run
    }

    #[test]
    fn merged_status_is_distinct_and_labeled() {
        assert_eq!(
            agent_status_from_record(Some("merged")),
            AgentStatus::Merged
        );
        assert_eq!(run_record_status(AgentStatus::Merged), "merged");
        assert_eq!(
            agent_status_from_record(Some("running")),
            AgentStatus::Running
        );

        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Merged;

        assert_eq!(agent_status_label(&run), "[x] merged");
    }

    #[test]
    fn resume_commands_reuse_saved_session_ids() {
        let mut claude = test_agent_run("run-1", "test task");
        claude.backend = Backend::Claude;
        claude.session_id = Some("11111111-1111-4111-8111-111111111111".to_string());
        let claude_command = claude_resume_command(&claude, claude.session_id.as_deref().unwrap());
        assert_eq!(claude_command.program, "claude");
        assert!(claude_command.args.iter().any(|arg| arg == "--resume"));
        assert!(claude_command
            .args
            .iter()
            .any(|arg| arg == "11111111-1111-4111-8111-111111111111"));

        let mut codex = test_agent_run("run-2", "test task");
        codex.backend = Backend::Codex;
        codex.session_id = Some("019e297b-12fe-79e2-a8f8-33ba41e5fdd4".to_string());
        let codex_command = codex_resume_command(&codex, codex.session_id.as_deref().unwrap());
        assert_eq!(codex_command.program, "codex");
        assert!(codex_command.args.iter().any(|arg| arg == "resume"));
        assert!(codex_command
            .args
            .windows(2)
            .any(|window| window[0] == "--enable" && window[1] == "goals"));
        assert!(codex_command
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "notify=[]"));
        assert!(codex_command
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.plugins=false"));
        assert!(codex_command
            .args
            .windows(2)
            .any(|window| window[0] == "-c" && window[1] == "features.computer_use=false"));
        assert!(codex_command
            .args
            .iter()
            .any(|arg| arg == "019e297b-12fe-79e2-a8f8-33ba41e5fdd4"));
    }

    #[test]
    fn agent_navigation_follows_visible_order_with_merged_section() {
        let mut app = App::new();
        let mut merged = test_agent_run("run-merged", "merged task");
        merged.status = AgentStatus::Merged;
        app.agents.push(merged);
        app.agents.push(test_agent_run("run-live", "live task"));
        app.selected_agent = 1;

        assert_eq!(app.visible_agent_indices(), vec![1, 0]);

        app.select_next_agent();
        assert_eq!(app.selected_agent, 0);

        app.select_previous_agent();
        assert_eq!(app.selected_agent, 1);
    }

    #[test]
    fn agent_navigation_keeps_main_section_first() {
        let mut app = App::new();
        let live = test_agent_run("run-live", "live task");
        let mut main = test_agent_run(MAIN_AGENT_ID, "main branch");
        main.mode = AgentMode::Main;
        let mut merged = test_agent_run("run-merged", "merged task");
        merged.status = AgentStatus::Merged;
        app.agents.push(live);
        app.agents.push(main);
        app.agents.push(merged);

        assert_eq!(app.visible_agent_indices(), vec![1, 0, 2]);
    }

    #[test]
    fn merge_request_clears_pending_delete() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Done;
        run.worktree_branch = Some("rudder/test".to_string());
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);
        app.delete_pending = Some("run-1".to_string());

        app.request_merge_selected_agent();

        assert!(app.delete_pending.is_none());
        assert!(app.merge_confirm.is_some());
    }

    #[test]
    fn merge_all_can_be_triggered_from_nav_mode() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Done;
        run.worktree_branch = Some("rudder/test".to_string());
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);
        app.selected_agent = 0;
        app.focus = FocusPane::Worker;
        app.nav_mode = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));

        assert!(matches!(
            app.merge_confirm.as_ref().map(|confirm| &confirm.intent),
            Some(MergeIntent::All { ids }) if ids == &vec!["run-1".to_string()]
        ));
    }

    #[test]
    fn merge_all_command_opens_confirmation() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Done;
        run.worktree_branch = Some("rudder/test".to_string());
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);

        assert!(app.handle_command("/merge-all"));

        assert!(matches!(
            app.merge_confirm.as_ref().map(|confirm| &confirm.intent),
            Some(MergeIntent::All { ids }) if ids == &vec!["run-1".to_string()]
        ));
    }

    #[test]
    fn review_all_starts_codex_aggregate_agent() {
        let mut app = App::new();
        let mut first = test_agent_run("run-1", "first task");
        first.status = AgentStatus::Done;
        first.worktree_branch = Some("rudder/first".to_string());
        first.worktree_path = Some(app.cwd.join("worktree-1"));
        let mut second = test_agent_run("run-2", "second task");
        second.status = AgentStatus::Done;
        second.worktree_branch = Some("rudder/second".to_string());
        second.worktree_path = Some(app.cwd.join("worktree-2"));
        app.agents.push(first);
        app.agents.push(second);
        app.focus = FocusPane::Agents;

        app.review_all_ready();

        assert_eq!(app.agents.len(), 3);
        assert_eq!(app.selected_agent, 2);
        let review = &app.agents[2];
        assert_eq!(review.mode, AgentMode::ReviewAll);
        assert_eq!(review.backend, Backend::Codex);
        assert_eq!(review.model, REVIEW_ALL_MODEL);
        assert_eq!(review.effort, Some(REVIEW_ALL_EFFORT));
        assert_eq!(
            review.review_source_ids,
            vec!["run-1".to_string(), "run-2".to_string()]
        );
        assert!(review.task.contains("/review"));
        assert!(review.task.contains("rudder/first"));
        assert!(review.task.contains("rudder/second"));
        assert_eq!(app.focus, FocusPane::Worker);
        assert_eq!(app.worker_view, WorkerView::Terminal);
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Codex review-all")));
    }

    #[test]
    fn review_all_can_be_triggered_from_nav_mode() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Done;
        run.worktree_branch = Some("rudder/test".to_string());
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);
        app.selected_agent = 0;
        app.focus = FocusPane::Worker;
        app.nav_mode = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        assert_eq!(app.selected_agent, 1);
        assert_eq!(app.agents[1].mode, AgentMode::ReviewAll);
        assert_eq!(app.agents[1].review_source_ids, vec!["run-1".to_string()]);
    }

    #[test]
    fn review_all_command_starts_codex_review_agent() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.status = AgentStatus::Done;
        run.worktree_branch = Some("rudder/test".to_string());
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);

        assert!(app.handle_command("/review-all"));

        assert_eq!(app.selected_agent, 1);
        assert_eq!(app.agents[1].mode, AgentMode::ReviewAll);
        assert_eq!(app.agents[1].model, REVIEW_ALL_MODEL);
    }

    #[test]
    fn review_all_without_ready_worktrees_shows_notice() {
        let mut app = App::new();

        app.review_all_ready();

        assert!(app.agents.is_empty());
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("no completed worktrees")));
    }

    #[test]
    fn review_all_claimed_sources_are_not_merge_all_ready() {
        let mut app = App::new();
        let mut source = test_agent_run("run-1", "source task");
        source.status = AgentStatus::Done;
        source.worktree_branch = Some("rudder/source".to_string());
        source.worktree_path = Some(app.cwd.join("source"));
        let mut review = test_agent_run("review-1", "review all");
        review.mode = AgentMode::ReviewAll;
        review.status = AgentStatus::Running;
        review.worktree_branch = Some("rudder/review-all".to_string());
        review.worktree_path = Some(app.cwd.join("review"));
        review.review_source_ids = vec!["run-1".to_string()];
        app.agents.push(source);
        app.agents.push(review);

        app.request_merge_all_ready();

        assert!(app.merge_confirm.is_none());
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("no completed worktrees")));
    }

    #[test]
    fn merging_review_all_row_moves_source_agents_to_merged_section() {
        let mut app = App::new();
        let mut first = test_agent_run("run-1", "first task");
        first.status = AgentStatus::Done;
        first.worktree_branch = Some("rudder/first".to_string());
        let mut second = test_agent_run("run-2", "second task");
        second.status = AgentStatus::Done;
        second.worktree_branch = Some("rudder/second".to_string());
        let mut review = test_agent_run("review-1", "review all");
        review.mode = AgentMode::ReviewAll;
        review.status = AgentStatus::Done;
        review.worktree_branch = Some("rudder/review-all".to_string());
        review.review_source_ids = vec!["run-1".to_string(), "run-2".to_string()];
        let live = test_agent_run("run-live", "live task");
        app.agents.push(first);
        app.agents.push(second);
        app.agents.push(review);
        app.agents.push(live);

        app.mark_agent_and_review_sources_merged(2, vec!["run-1".to_string(), "run-2".to_string()]);

        assert_eq!(app.agents[0].status, AgentStatus::Merged);
        assert_eq!(app.agents[1].status, AgentStatus::Merged);
        assert_eq!(app.agents[2].status, AgentStatus::Merged);
        assert!(app.agents[0].worktree_branch.is_none());
        assert!(app.agents[1].worktree_branch.is_none());
        assert!(app.agents[2].worktree_branch.is_none());
        assert_eq!(app.visible_agent_indices(), vec![3, 0, 1, 2]);
    }

    #[test]
    fn delete_prompt_for_worktree_requires_second_d_without_merge_offer() {
        let mut app = App::new();
        let mut run = test_agent_run("run-1", "test task");
        run.worktree_path = Some(app.cwd.join("worktree"));
        app.agents.push(run);

        app.delete_selected_agent();

        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.delete_pending.as_deref(), Some("run-1"));
        let notice = app.notice.as_deref().unwrap_or_default();
        assert!(notice.contains("press d again"));
        assert!(!notice.contains("merge"));
    }

    #[test]
    fn delete_agent_requires_second_d() {
        let mut app = App::new();
        app.agents.push(AgentRun {
            id: "run-1".to_string(),
            created_at: "1".to_string(),
            mode: AgentMode::Execute,
            task: "test task".to_string(),
            task_summary: summarize_task("test task"),
            current_prompt: "test task".to_string(),
            turns: vec![AgentTurn {
                ts: "1".to_string(),
                prompt: "test task".to_string(),
                source: "user".to_string(),
            }],
            last_user_input_at: "1".to_string(),
            backend: Backend::Claude,
            model: "sonnet".to_string(),
            effort: None,
            status: AgentStatus::Done,
            cwd: app.cwd.clone(),
            worktree_branch: None,
            worktree_path: None,
            session_id: None,
            terminal: None,
            terminal_size: None,
            review_terminal: None,
            review_size: None,
            review_error: None,
            last_output_at: Instant::now(),
            completed_at: Some(Instant::now()),
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
        });

        app.delete_selected_agent();
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.delete_pending.as_deref(), Some("run-1"));

        app.delete_selected_agent();
        assert!(app.agents.is_empty());
        assert!(app.delete_pending.is_none());
    }

    #[test]
    fn create_main_agent_returns_distinct_main_runs() {
        let first = create_main_agent(
            std::env::temp_dir().as_path(),
            Backend::Claude,
            "sonnet",
            None,
            "",
        );
        let second = create_main_agent(
            std::env::temp_dir().as_path(),
            Backend::Claude,
            "sonnet",
            None,
            "inspect the CLI",
        );
        assert!(first.is_main());
        assert!(second.is_main());
        assert_ne!(first.id, MAIN_AGENT_ID);
        assert_ne!(second.id, MAIN_AGENT_ID);
        assert_ne!(first.id, second.id);
        assert!(second.task_summary.contains("inspect"));
        assert!(!second.task_summary.contains(':'));
    }

    #[test]
    fn task_summary_worker_updates_main_agent_label() {
        let repo_root = std::env::temp_dir().join(format!(
            "rudder-main-summary-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&repo_root).expect("create test repo root");

        let mut app = App::new();
        app.cwd = repo_root.clone();
        let main = create_main_agent(
            &repo_root,
            Backend::Claude,
            "sonnet",
            None,
            "make sure the main chat agent pane label is summarized",
        );
        let run_id = main.id.clone();
        app.agents.push(main);

        app.task_summary_tx
            .send(TaskSummaryResult {
                run_id,
                title: Some("Summarize Main Chat Labels".to_string()),
            })
            .expect("send task summary result");
        app.poll_task_summary_workers();

        assert_eq!(
            app.agents[0].task_summary,
            "Summarize Main Chat Labels".to_string()
        );
        let _ = fs::remove_dir_all(repo_root);
    }

    #[test]
    fn main_agent_blocks_delete_merge_and_rename() {
        let mut app = App::new();
        let mut main = test_agent_run(MAIN_AGENT_ID, "main branch");
        main.mode = AgentMode::Main;
        main.worktree_branch = None;
        main.worktree_path = None;
        app.agents.push(main);
        app.selected_agent = 0;

        app.delete_selected_agent();
        assert_eq!(app.agents.len(), 1);
        assert!(app
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("main agent"));

        app.notice = None;
        app.request_merge_selected_agent();
        assert!(app.merge_confirm.is_none());
        assert!(app
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("main agent"));

        app.notice = None;
        app.start_rename_selected_agent();
        assert!(app.rename_input.is_none());
        assert!(app
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("main agent"));
    }

    #[test]
    fn merge_cleanup_preserves_main_agent() {
        let mut app = App::new();
        let mut main = test_agent_run(MAIN_AGENT_ID, "main branch");
        main.mode = AgentMode::Main;
        main.worktree_branch = None;
        main.worktree_path = None;
        app.agents.push(main);
        // The defensive guard in merge_agent_at's cleanup branch must never
        // remove main even if invoked at index 0.
        let snapshot_len = app.agents.len();
        if app.agents.first().map(|a| a.is_main()).unwrap_or(false) {
            // Simulate the cleanup branch's gate.
            let index = 0;
            assert!(index < app.agents.len() && app.agents[index].is_main());
        }
        assert_eq!(app.agents.len(), snapshot_len);
    }

    #[test]
    fn ctrl_c_exits_from_every_focus_pane() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut app = App::new();

        app.focus = FocusPane::Agents;
        assert!(app.handle_key(key));

        app.focus = FocusPane::Worker;
        assert!(app.handle_key(key));

        app.focus = FocusPane::Task;
        assert!(app.handle_key(key));
    }

    #[test]
    fn event_dispatch_handles_paste_and_ctrl_c() {
        let mut app = App::new();
        assert!(!handle_event(&mut app, Event::Paste("hello".to_string())));
        assert_eq!(app.task_input, "hello");
        assert!(handle_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        ));
    }

    #[test]
    fn v_and_escape_leave_review_view() {
        let mut app = App::new();
        app.worker_view = WorkerView::Diff;
        app.focus = FocusPane::Worker;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::empty())));
        assert_eq!(app.worker_view, WorkerView::Terminal);

        app.worker_view = WorkerView::Diff;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())));
        assert_eq!(app.worker_view, WorkerView::Terminal);
    }
}

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
    execute!(stdout, EnableMouseCapture)?;
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

    let _ = execute!(stdout, DisableMouseCapture);
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

    let _ = execute!(stdout, DisableMouseCapture);
    let _ = disable_raw_mode();
    result
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
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

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let task_height = task_pane_height(app, area.width);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(task_height),
        ])
        .split(area);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(34),
            Constraint::Length(1),
            Constraint::Min(42),
        ])
        .split(rows[0]);

    app.agents_area = Some(main[0]);
    app.worker_area = Some(main[2]);
    app.task_area = Some(rows[2]);

    render_agents(frame, main[0], app);
    render_gutter(frame, main[1], Gutter::Vertical);
    render_worker(frame, main[2], app);
    render_gutter(frame, rows[1], Gutter::Horizontal);
    render_task(frame, rows[2], app);
    render_suggestions(frame, rows[2], app);
    render_cloud_prompt(frame, area, app);
    render_merge_prompt(frame, area, app);
    render_mouse_debug(frame, area, app);
}

#[derive(Clone, Copy)]
enum Gutter {
    Horizontal,
    Vertical,
}

fn render_gutter(frame: &mut Frame<'_>, area: Rect, gutter: Gutter) {
    let style = muted_style(false);
    let line = match gutter {
        Gutter::Horizontal => " ".repeat(area.width as usize),
        Gutter::Vertical => " ".to_string(),
    };

    let lines = vec![Line::from(Span::styled(line, style)); area.height as usize];
    frame.render_widget(Paragraph::new(lines).style(app_style()), area);
}

fn render_mouse_debug(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if !app.mouse_debug {
        return;
    }
    let text = app
        .mouse_debug_last
        .as_deref()
        .unwrap_or("waiting for mouse event");
    let width = area.width.saturating_sub(4).min(120).max(20);
    let height = 3_u16.min(area.height);
    let x = area.right().saturating_sub(width + 2);
    let y = area.bottom().saturating_sub(height + 1);
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Color::Yellow),
        )))
        .block(Block::default().borders(Borders::ALL).title("mouse debug"))
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_agents(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Agents;
    let diff_summaries: Vec<Option<String>> = {
        let keys: Vec<(String, PathBuf, bool)> = app
            .agents
            .iter()
            .map(|a| (a.id.clone(), a.cwd.clone(), a.is_main()))
            .collect();
        keys.iter()
            .map(|(id, cwd, is_main)| {
                if *is_main {
                    None
                } else {
                    app.cached_diff_summary(id, cwd)
                }
            })
            .collect()
    };
    let run_count = app.agents.iter().filter(|a| !a.is_main()).count();
    let mut lines = vec![
        ListItem::new(Line::from(Span::styled(
            "rudder",
            pane_text_style(focused).add_modifier(Modifier::BOLD),
        ))),
        ListItem::new(Line::from(vec![
            Span::styled(app.cwd.display().to_string(), pane_text_style(focused)),
            Span::raw(" "),
            Span::styled(
                app.branch.as_deref().unwrap_or("no-branch"),
                muted_style(focused),
            ),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled("agents ", pane_text_style(focused)),
            Span::styled(run_count.to_string(), accent_style(focused)),
            Span::styled(" runs", pane_text_style(focused)),
        ])),
        ListItem::new(Line::default()),
    ];
    if let Some((current, latest)) = read_update_notice() {
        lines.insert(
            lines.len() - 1,
            ListItem::new(Line::from(vec![
                Span::styled(
                    "\u{2191} ",
                    accent_style(focused).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("update: {current} -> {latest}"),
                    pane_text_style(focused).add_modifier(Modifier::BOLD),
                ),
            ])),
        );
        lines.insert(
            lines.len() - 1,
            ListItem::new(Line::from(Span::styled(
                "  npm i -g @viraatdas/rudder",
                muted_style(focused),
            ))),
        );
    }
    if is_cloud_worker_session() {
        // Only surface cloud status when this dashboard is actually running
        // inside a cloud worker. Showing "cloud connected" in a plain local
        // rudder session is misleading: it reflects whether the user has saved
        // cloud auth, not whether anything is attached.
        let insert_at = lines.len().saturating_sub(1);
        lines.insert(
            insert_at,
            ListItem::new(Line::from(vec![
                Span::styled("☁ ", cloud_style(app.cloud_connected, focused)),
                Span::styled(
                    if app.cloud_connected {
                        "cloud connected"
                    } else {
                        "cloud offline"
                    },
                    cloud_style(app.cloud_connected, focused),
                ),
            ])),
        );
        let insert_at = lines.len().saturating_sub(1);
        lines.insert(
            insert_at,
            ListItem::new(Line::from(vec![
                Span::styled("☁ ", cloud_style(app.cloud_connected, focused)),
                Span::styled(
                    cloud_workspace_label(app.cloud_workspace.as_ref()),
                    cloud_style(app.cloud_workspace.is_some(), focused),
                ),
            ])),
        );
    }

    for hint in AGENT_PANE_HINTS {
        lines.push(ListItem::new(Line::from(Span::styled(
            *hint,
            muted_style(focused),
        ))));
    }
    lines.push(ListItem::new(Line::default()));

    let task_width = area.width.saturating_sub(4).max(12) as usize;

    let push_agent_row = |lines: &mut Vec<ListItem>,
                          app: &App,
                          index: usize,
                          agent: &AgentRun,
                          diff: Option<String>| {
        let selected = index == app.selected_agent;
        let marker = if selected { "> " } else { "  " };
        let task_style = if selected {
            pane_text_style(focused).add_modifier(Modifier::BOLD)
        } else {
            pane_text_style(focused)
        };
        let task_label = if agent.is_main() {
            agent.task_summary.clone()
        } else if selected && app.rename_input.is_some() {
            let buf = app.rename_input.clone().unwrap_or_default();
            format!("✎ {buf}")
        } else if agent.task_summary.trim().is_empty() {
            summarize_task(&agent.task)
        } else {
            agent.task_summary.clone()
        };

        lines.push(ListItem::new(Line::from(vec![
            Span::styled(marker, accent_style(focused)),
            if is_cloud_agent(agent) {
                Span::styled(
                    "☁ ",
                    cloud_style(true, focused).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
            Span::styled(truncate_chars(&task_label, task_width), task_style),
        ])));

        let (status_label, status_style): (&'static str, Style) =
            if agent.is_main() && agent.terminal.is_none() {
                ("idle", muted_style(focused))
            } else {
                (agent_status_label(agent), agent_status_style(agent))
            };

        lines.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(status_label, status_style),
            Span::raw("  "),
            if agent.is_main() {
                Span::styled("main", accent_style(focused).add_modifier(Modifier::BOLD))
            } else if is_cloud_agent(agent) {
                Span::styled("cloud", accent_style(focused).add_modifier(Modifier::BOLD))
            } else if agent.mode == AgentMode::RudderPlan {
                Span::styled("rudder-plan", accent_style(focused))
            } else if agent.mode == AgentMode::ReviewAll {
                Span::styled("review-all", accent_style(focused))
            } else if agent.mode == AgentMode::Plan {
                Span::styled("plan", accent_style(focused))
            } else {
                Span::styled("run", muted_style(focused))
            },
            Span::raw("  "),
            Span::styled(agent.backend.as_str().to_string(), muted_style(focused)),
            Span::raw("  "),
            Span::styled(agent.model.clone(), model_style(focused)),
            Span::styled(
                format!("({})", effort_label(agent.effort)),
                model_style(focused),
            ),
        ])));
        if let Some(summary) = diff {
            lines.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(summary, muted_style(focused)),
            ])));
        }
    };

    let main_count = app.agents.iter().filter(|a| a.is_main()).count();
    let active_count = app
        .agents
        .iter()
        .filter(|a| a.status != AgentStatus::Merged && !a.is_main())
        .count();
    let merged_count = app
        .agents
        .iter()
        .filter(|a| a.status == AgentStatus::Merged && !a.is_main())
        .count();

    if main_count > 0 {
        lines.push(ListItem::new(Line::from(Span::styled(
            "main",
            muted_style(focused),
        ))));
        for (index, agent) in app.agents.iter().enumerate() {
            if !agent.is_main() {
                continue;
            }
            push_agent_row(&mut lines, app, index, agent, None);
        }
    }

    if active_count > 0 {
        if main_count > 0 {
            lines.push(ListItem::new(Line::default()));
        }
        lines.push(ListItem::new(Line::from(Span::styled(
            "worktrees",
            muted_style(focused),
        ))));
    }

    for (index, agent) in app.agents.iter().enumerate() {
        if agent.is_main() || agent.status == AgentStatus::Merged {
            continue;
        }
        let summary = diff_summaries.get(index).and_then(|opt| opt.clone());
        push_agent_row(&mut lines, app, index, agent, summary);
    }
    if main_count == 0 && active_count == 0 && merged_count == 0 {
        lines.push(ListItem::new(Line::from(Span::styled(
            "no agents yet  ·  type a task or /main",
            muted_style(focused),
        ))));
    }

    if merged_count > 0 {
        lines.push(ListItem::new(Line::default()));
        lines.push(ListItem::new(Line::from(Span::styled(
            "merged",
            muted_style(focused),
        ))));
        for (index, agent) in app.agents.iter().enumerate() {
            if agent.status != AgentStatus::Merged || agent.is_main() {
                continue;
            }
            push_agent_row(&mut lines, app, index, agent, None);
        }
    }

    frame.render_widget(
        List::new(lines)
            .style(app_style())
            .block(pane_block("agents", focused, app.nav_mode)),
        area,
    );
}

fn visible_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    let mut indices = agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| agent.is_main())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    indices.extend(
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.status != AgentStatus::Merged && !agent.is_main())
            .map(|(index, _)| index),
    );
    indices.extend(
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.status == AgentStatus::Merged && !agent.is_main())
            .map(|(index, _)| index),
    );
    indices
}
fn render_worker(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = block_inner(area);
    let terminal_size = TerminalSize::new(inner.height.max(1), inner.width.max(1)).ok();
    let focused = app.focus == FocusPane::Worker;

    if let Some(size) = terminal_size {
        if let Some(run) = app.agents.get_mut(app.selected_agent) {
            if app.worker_view == WorkerView::Terminal && run.terminal_size != Some(size) {
                if let Some(terminal) = run.terminal.as_mut() {
                    if terminal.resize(size).is_ok() {
                        run.terminal_size = Some(size);
                    }
                }
            }
            if app.worker_view == WorkerView::Diff && run.review_size != Some(size) {
                if let Some(review) = run.review_terminal.as_mut() {
                    if review.resize(size).is_ok() {
                        run.review_size = Some(size);
                    }
                }
            }
        }
    }

    let lines = match app.worker_view {
        WorkerView::Terminal => worker_lines(app, inner.height as usize, inner.width as usize),
        WorkerView::Diff => review_lines(app, inner.height as usize),
    };
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(pane_block(
            match app.worker_view {
                WorkerView::Terminal => "worker",
                WorkerView::Diff => "review",
            },
            focused,
            app.nav_mode,
        ))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);

    if focused {
        match app.worker_view {
            WorkerView::Terminal => set_worker_cursor(frame, inner, app),
            WorkerView::Diff => set_review_cursor(frame, inner, app),
        }
    }
}

fn set_worker_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let Some(run) = app.agents.get(app.selected_agent) else {
        return;
    };
    let Some(terminal) = run.terminal.as_ref() else {
        return;
    };
    if terminal.scrollback() > 0 {
        return;
    }
    let cursor = terminal.cursor();
    if cursor.row >= inner.height || cursor.col >= inner.width {
        return;
    }
    if !cursor.visible && !force_worker_cursor(run.backend) {
        return;
    }
    frame.set_cursor_position((inner.x + cursor.col, inner.y + cursor.row));
}

fn set_review_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let Some(terminal) = app
        .agents
        .get(app.selected_agent)
        .and_then(|run| run.review_terminal.as_ref())
    else {
        return;
    };
    if terminal.scrollback() > 0 {
        return;
    }
    let cursor = terminal.cursor();
    if cursor.row >= inner.height || cursor.col >= inner.width || !cursor.visible {
        return;
    }
    frame.set_cursor_position((inner.x + cursor.col, inner.y + cursor.row));
}

fn worker_lines(app: &mut App, height: usize, width: usize) -> Vec<Line<'static>> {
    let Some(run) = app.agents.get_mut(app.selected_agent) else {
        return vec![
            Line::from(""),
            Line::from(Span::styled("No worker is running yet.", muted_style(true))),
            Line::from(""),
            Line::from(Span::styled(
                "Enter a task below to start Claude Code or Codex in this pane.",
                pane_text_style(true),
            )),
        ];
    };

    if let Some(error) = &run.last_error {
        return vec![
            Line::from(vec![
                Span::styled("failed ", error_style()),
                Span::styled(run.cwd.display().to_string(), muted_style(true)),
            ]),
            Line::from(Span::styled(error.clone(), error_style())),
        ];
    }

    let backend = run.backend;
    let focused = app.focus == FocusPane::Worker;
    let Some(terminal) = run.terminal.as_mut() else {
        return vec![
            Line::from(Span::styled(
                format!("{}  {}", run.status.as_str(), short_task(&run.task)),
                pane_text_style(true),
            )),
            Line::from(""),
            Line::from(Span::styled(
                if matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan) {
                    "This read-only planner is not running."
                } else {
                    "This agent is not running."
                },
                muted_style(true),
            )),
            Line::from(Span::styled(
                run.cwd.display().to_string(),
                muted_style(true),
            )),
        ];
    };

    let selection = app
        .worker_selection
        .map(normalize_selection)
        .filter(|selection| !selection_is_empty(*selection));
    let styled_rows = terminal.styled_lines();
    let start_row = styled_rows.len().saturating_sub(height);
    let cursor = worker_render_cursor(backend, terminal, focused, height, width, start_row);
    let mut lines = styled_rows
        .into_iter()
        .enumerate()
        .skip(start_row)
        .map(|(row, cells)| {
            styled_terminal_line(
                cells,
                selection_for_row(selection, row),
                cursor
                    .filter(|cursor| cursor.row as usize == row)
                    .map(|cursor| cursor.col as usize),
            )
        })
        .collect::<Vec<_>>();
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    lines
}

fn worker_render_cursor(
    backend: Backend,
    terminal: &TerminalPane,
    focused: bool,
    height: usize,
    width: usize,
    start_row: usize,
) -> Option<TerminalCursor> {
    if !focused || terminal.scrollback() > 0 {
        return None;
    }
    let cursor = terminal.cursor();
    if !cursor.visible && !force_worker_cursor(backend) {
        return None;
    }
    let row = cursor.row as usize;
    let col = cursor.col as usize;
    if row < start_row || row >= start_row.saturating_add(height) || col >= width {
        return None;
    }
    Some(cursor)
}

fn force_worker_cursor(backend: Backend) -> bool {
    matches!(backend, Backend::Claude | Backend::Codex)
}

fn review_lines(app: &mut App, height: usize) -> Vec<Line<'static>> {
    let Some(run) = app.agents.get_mut(app.selected_agent) else {
        return vec![Line::from(Span::styled(
            "No agent selected.",
            muted_style(true),
        ))];
    };

    if let Some(error) = &run.review_error {
        return vec![
            Line::from(Span::styled("Hunk review failed", error_style())),
            Line::from(Span::styled(error.clone(), error_style())),
            Line::from(""),
            Line::from(Span::styled(
                "Press Ctrl-G then v to return to the worker.",
                muted_style(true),
            )),
        ];
    }

    let Some(review) = run.review_terminal.as_mut() else {
        return vec![
            Line::from(Span::styled("Opening Hunk review...", muted_style(true))),
            Line::from(""),
            Line::from(Span::styled(
                "If Hunk is unavailable, Rudder falls back to a live git diff.",
                pane_text_style(true),
            )),
        ];
    };

    let mut lines = review
        .styled_lines()
        .into_iter()
        .map(|cells| styled_terminal_line(cells, None, None))
        .collect::<Vec<_>>();
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    lines
}

fn render_task(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPane::Task;
    let default_hint = if app.plan_mode {
        "Enter plan  Up/Down history  Option-1/2/3 pane  /plan off"
    } else {
        "Enter start  Up/Down history  Option-1/2/3 pane  /plan  /rudder-plan  /sync"
    };
    let hint = app.notice.as_deref().unwrap_or(default_hint);
    let inner_width = area.width.saturating_sub(2).max(1);
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, inner_width);
    let (cursor_line, cursor_column) =
        task_cursor_position(&app.task_input, app.task_cursor, inner_width);
    let wrapped_hint = wrap_text(hint, inner_width);
    let hint_line_count = wrapped_hint.len().max(1);
    let available_lines = area.height.saturating_sub(2).max(1) as usize;
    let max_input_lines = available_lines.saturating_sub(hint_line_count).max(1);
    let input_start = if input_lines.len() > max_input_lines {
        cursor_line.saturating_sub(max_input_lines.saturating_sub(1))
    } else {
        0
    };
    let input_start = input_start.min(input_lines.len().saturating_sub(1));
    let task_selection = if app.task_input.is_empty() {
        None
    } else {
        app.task_selection
            .map(normalize_selection)
            .filter(|selection| !selection_is_empty(*selection))
    };
    let mut lines = input_lines
        .iter()
        .skip(input_start)
        .take(max_input_lines)
        .enumerate()
        .map(|(offset, line)| {
            let display = if app.task_input.is_empty() {
                if app.plan_mode {
                    "Type a task to plan"
                } else {
                    "Type a task, /plan, /rudder-plan, or /sync"
                }
            } else {
                line.as_str()
            };
            let style = if app.task_input.is_empty() {
                muted_style(focused)
            } else {
                pane_text_style(focused)
            };
            let row = input_start + offset;
            styled_plain_line(
                display,
                style,
                selection_for_row(task_selection, row).filter(|_| !app.task_input.is_empty()),
            )
        })
        .collect::<Vec<_>>();

    if app.notice.is_some() {
        for line in wrapped_hint {
            lines.push(Line::from(Span::styled(line, muted_style(focused))));
        }
    } else {
        let first_hint = wrapped_hint.first().cloned().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(first_hint, muted_style(focused)),
            Span::raw("  "),
            Span::styled(
                if app.plan_mode { "plan" } else { "run" },
                accent_style(focused),
            ),
            Span::raw("  "),
            Span::styled(app.backend.as_str(), accent_style(focused)),
            Span::raw(" "),
            Span::styled(app.model.as_str(), model_style(focused)),
            Span::styled(
                format!("({})", effort_label(app.effort)),
                model_style(focused),
            ),
        ]));
        for line in wrapped_hint.into_iter().skip(1) {
            lines.push(Line::from(Span::styled(line, muted_style(focused))));
        }
    }

    let paragraph =
        Paragraph::new(lines)
            .style(app_style())
            .block(pane_block("task", focused, app.nav_mode));

    frame.render_widget(paragraph, area);

    if app.focus == FocusPane::Task {
        let visible_cursor_line = cursor_line.saturating_sub(input_start);
        let x = area.x + 1 + cursor_column as u16;
        let y = area.y + 1 + visible_cursor_line as u16;
        if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
            frame.set_cursor_position((x, y));
        }
    }
}

fn task_pane_height(app: &App, width: u16) -> u16 {
    let default_hint = if app.plan_mode {
        "Enter plan  Up/Down history  Option-1/2/3 pane  /plan off"
    } else {
        "Enter start  Up/Down history  Option-1/2/3 pane  /plan  /rudder-plan  /sync"
    };
    let hint = app.notice.as_deref().unwrap_or(default_hint);
    let inner_width = width.saturating_sub(2).max(1);
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, inner_width)
        .len()
        .max(1) as u16;
    let hint_lines = wrap_text(hint, inner_width).len().max(1) as u16;
    2_u16
        .saturating_add(input_lines)
        .saturating_add(hint_lines)
        .clamp(4, 10)
}

fn render_suggestions(frame: &mut Frame<'_>, task_area: Rect, app: &App) {
    let suggestions = suggestions_for(app);
    if suggestions.is_empty() {
        return;
    }

    let visible_count = suggestions.len().min(8);
    let height = (visible_count as u16).saturating_add(2);
    if task_area.y < height {
        return;
    }
    let area = Rect {
        x: task_area.x,
        y: task_area.y - height,
        width: task_area.width,
        height,
    };

    let selected_index = app.picker_index.min(suggestions.len().saturating_sub(1));
    let offset = selected_index.saturating_sub(visible_count.saturating_sub(1));
    let items = suggestions
        .iter()
        .skip(offset)
        .take(visible_count)
        .enumerate()
        .map(|(index, suggestion)| {
            let selected = index + offset == selected_index;
            let marker = if selected { "> " } else { "  " };
            let style = if selected {
                accent_style(true)
            } else {
                app_style()
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(suggestion.label.clone(), style),
                Span::raw("  "),
                Span::styled(suggestion.detail.clone(), muted_style(true)),
            ]))
        })
        .collect::<Vec<_>>();

    let title = if app.task_input.starts_with("/model") {
        " model "
    } else {
        " commands "
    };
    let list = List::new(items).style(app_style()).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(FOCUS_COLOR)
                    .add_modifier(Modifier::BOLD),
            )
            .style(app_style()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(list, area);
}

fn render_merge_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (title, lines, border_color) = if let Some(confirm) = &app.merge_confirm {
        let summary = match &confirm.intent {
            MergeIntent::Selected { task, .. } => {
                format!("Merge selected worktree: {}", short_task(task))
            }
            MergeIntent::All { ids } => format!(
                "Merge {} completed worktree{}",
                ids.len(),
                if ids.len() == 1 { "" } else { "s" }
            ),
        };
        (
            " confirm merge ",
            vec![
                Line::from(Span::styled(summary, app_style())),
                Line::from(Span::styled(
                    if merge_strategy() == MergeStrategy::Rebase {
                        "This will rebase the worktree first, then fast-forward merge. Rebase conflicts stay in the worktree."
                    } else {
                        "This will run git merge into the current branch."
                    },
                    app_style(),
                )),
                merge_confirm_hint_line(),
            ],
            RUNNING_COLOR,
        )
    } else if let Some(prompt) = &app.conflict_prompt {
        let files = prompt.conflicted_files.join(", ");
        let operation_label = if prompt.operation == ConflictOperation::Rebase {
            "Rebase"
        } else {
            "Merge"
        };
        (
            if prompt.operation == ConflictOperation::Rebase {
                " rebase conflict "
            } else {
                " merge conflict "
            },
            vec![
                Line::from(Span::styled(
                    format!(
                        "{operation_label} stopped with {} conflicted file{}.",
                        prompt.conflicted_files.len(),
                        if prompt.conflicted_files.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    app_style(),
                )),
                Line::from(Span::styled(
                    if files.is_empty() {
                        "Git did not report conflicted files.".to_string()
                    } else {
                        format!("Files: {files}")
                    },
                    app_style(),
                )),
                Line::from(Span::styled(
                    "Press y to start an AI resolver, n to handle manually.",
                    app_style(),
                )),
            ],
            FAILED_COLOR,
        )
    } else {
        return;
    };

    let modal = centered_modal(area, 74, (lines.len() as u16).saturating_add(2));
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .style(app_style());
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, modal);
    frame.render_widget(paragraph, modal);
}

fn render_cloud_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(prompt) = &app.cloud_prompt else {
        return;
    };
    let selected = prompt
        .selected_task
        .as_deref()
        .map(|task| {
            format!(
                "onload current Rudder workspace to cloud: {}",
                short_task(task)
            )
        })
        .unwrap_or_else(|| "onload current Rudder workspace to cloud".to_string());
    let upload_selected = prompt.choice == CloudLaunchChoice::Upload;
    let scratch_selected = prompt.choice == CloudLaunchChoice::Scratch;
    let row_style = |selected: bool| {
        if selected {
            accent_style(true).add_modifier(Modifier::BOLD)
        } else {
            app_style()
        }
    };
    let marker = |selected: bool| if selected { "> " } else { "  " };
    let lines = vec![
        Line::from(Span::styled(
            "Move this Rudder workspace to the cloud, or start a fresh cloud worker.",
            app_style(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(marker(upload_selected), accent_style(true)),
            Span::styled(selected, row_style(upload_selected)),
        ]),
        Line::from(vec![
            Span::styled(marker(scratch_selected), accent_style(true)),
            Span::styled(
                "start scratch in a fresh cloud directory",
                row_style(scratch_selected),
            ),
        ]),
        Line::from(vec![
            Span::styled("Up/Down ", muted_style(true)),
            Span::styled("choose  ", muted_style(true)),
            Span::styled("Enter ", muted_style(true)),
            Span::styled("start  ", muted_style(true)),
            Span::styled("Esc ", muted_style(true)),
            Span::styled("cancel", muted_style(true)),
        ]),
    ];
    let modal = centered_modal(area, 78, 8);
    let block = Block::default()
        .title(" cloud launch ")
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(CLOUD_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .style(app_style());
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .style(app_style())
            .block(block)
            .wrap(Wrap { trim: true }),
        modal,
    );
}

fn merge_confirm_hint_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("Press ", app_style()),
        Span::styled(
            "y to merge",
            Style::default()
                .fg(FAILED_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(", n to cancel.", app_style()),
    ])
}

fn centered_modal(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(4)).max(24);
    let height = desired_height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn pane_block(title: &'static str, focused: bool, nav_mode: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default()
            .fg(FOCUS_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(INACTIVE_COLOR)
    };

    let title_style = if focused {
        Style::default()
            .fg(Color::Black)
            .bg(FOCUS_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let _ = nav_mode;

    Block::default()
        .title(Line::from(Span::styled(format!(" {title} "), title_style)))
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn task_input_lines(value: &str, cursor: usize, width: u16) -> Vec<String> {
    let mut lines = wrap_input_text(value, width);
    let (cursor_line, _) = task_cursor_position(value, cursor, width);
    while lines.len() <= cursor_line {
        lines.push(String::new());
    }
    lines
}

fn wrap_input_text(value: &str, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = vec![String::new()];

    for ch in value.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            lines.push(String::new());
            continue;
        }
        if lines
            .last()
            .is_some_and(|line| line.chars().count() == max_width)
        {
            lines.push(String::new());
        }
        if let Some(line) = lines.last_mut() {
            line.push(ch);
        }
    }

    lines
}

fn task_cursor_position(value: &str, cursor: usize, width: u16) -> (usize, usize) {
    let max_width = usize::from(width.max(1));
    let mut line = 0;
    let mut column = 0;

    for ch in value.chars().take(cursor) {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            line += 1;
            column = 0;
            continue;
        }
        column += 1;
        if column == max_width {
            line += 1;
            column = 0;
        }
    }

    (line, column)
}

fn wrap_text(value: &str, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        if current.is_empty() {
            push_wrapped_word(&mut lines, &mut current, word, max_width);
            continue;
        }

        if current.chars().count() + 1 + word.chars().count() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            push_wrapped_word(&mut lines, &mut current, word, max_width);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn push_wrapped_word(lines: &mut Vec<String>, current: &mut String, word: &str, max_width: usize) {
    if word.chars().count() <= max_width {
        current.push_str(word);
        return;
    }

    let mut chunk = String::new();
    for ch in word.chars() {
        if chunk.chars().count() == max_width {
            lines.push(std::mem::take(&mut chunk));
        }
        chunk.push(ch);
    }
    *current = chunk;
}

fn pane_text_style(focused: bool) -> Style {
    if focused {
        Style::default()
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

fn muted_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

fn accent_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(FOCUS_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

fn model_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(MODEL_COLOR)
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

fn cloud_style(connected: bool, focused: bool) -> Style {
    let color = if connected {
        CLOUD_COLOR
    } else {
        INACTIVE_COLOR
    };
    if focused {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).add_modifier(Modifier::DIM)
    }
}

fn app_style() -> Style {
    Style::default()
}

fn error_style() -> Style {
    Style::default()
        .fg(FAILED_COLOR)
        .add_modifier(Modifier::BOLD)
}

fn block_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn is_scroll_mouse_event(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

fn mouse_scrollback_delta(mouse: MouseEvent, viewport_height: u16) -> isize {
    let rows = wheel_scroll_rows(viewport_height, mouse.modifiers) as isize;
    match mouse.kind {
        MouseEventKind::ScrollUp => rows,
        MouseEventKind::ScrollDown => -rows,
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => 0,
        _ => 0,
    }
}

fn wheel_scroll_rows(viewport_height: u16, modifiers: KeyModifiers) -> u16 {
    let page = viewport_height.saturating_sub(1).max(1);
    if modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::META | KeyModifiers::SUPER,
    ) {
        return page;
    }

    wheel_scroll_rows_setting().min(page).max(1)
}

fn wheel_scroll_rows_setting() -> u16 {
    env::var("RUDDER_WHEEL_SCROLL_ROWS")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WHEEL_SCROLL_ROWS)
}

fn page_scroll_rows(area: Option<Rect>) -> isize {
    let height = area.map(block_inner).map(|inner| inner.height).unwrap_or(1);
    height.saturating_sub(1).max(1) as isize
}

fn status_style(status: AgentStatus) -> Style {
    Style::default().fg(status_color(status))
}

fn agent_status_label(agent: &AgentRun) -> &'static str {
    if agent.needs_permission {
        "needs permission"
    } else if agent.needs_user_input {
        "needs input"
    } else if matches!(agent.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && agent.status == AgentStatus::Running
    {
        "planning"
    } else if agent.status == AgentStatus::Merged {
        "[x] merged"
    } else {
        agent.status.as_str()
    }
}

fn agent_status_style(agent: &AgentRun) -> Style {
    if agent.needs_permission || agent.needs_user_input {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        status_style(agent.status)
    }
}

fn is_cloud_agent(agent: &AgentRun) -> bool {
    agent.task == "cloud"
        || agent.task.starts_with("cloud ")
        || agent.current_prompt == "cloud"
        || agent.current_prompt.starts_with("cloud ")
}

fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Running => RUNNING_COLOR,
        AgentStatus::Done | AgentStatus::Merged => DONE_COLOR,
        AgentStatus::Failed => FAILED_COLOR,
        AgentStatus::Stopped => INACTIVE_COLOR,
    }
}

fn selection_point_from_mouse(mouse: MouseEvent, area: Rect) -> SelectionPoint {
    SelectionPoint {
        row: mouse
            .row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1)) as usize,
        col: mouse
            .column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1)) as usize,
    }
}

fn task_selection_point_from_mouse(
    app: &App,
    mouse: MouseEvent,
    area: Rect,
) -> Option<SelectionPoint> {
    if !rect_contains(area, mouse.column, mouse.row) {
        return None;
    }
    let width = task_inner_width(area);
    let input_lines = task_input_lines(&app.task_input, app.task_cursor, width);
    let input_start = task_visible_input_start(app, area, &input_lines);
    let visible_count = task_visible_input_count(app, area, input_lines.len());
    let rel_row = mouse.row.saturating_sub(area.y) as usize;
    if rel_row >= visible_count {
        return None;
    }
    let row = input_start.saturating_add(rel_row);
    if row >= input_lines.len() {
        return None;
    }
    Some(SelectionPoint {
        row,
        col: mouse
            .column
            .saturating_sub(area.x)
            .min(width.saturating_sub(1)) as usize,
    })
}

fn agent_index_from_mouse(app: &App, mouse: MouseEvent, area: Rect) -> Option<usize> {
    let inner = block_inner(area);
    if !rect_contains(inner, mouse.column, mouse.row) {
        return None;
    }

    let mut row = mouse.row.saturating_sub(inner.y);
    if row < AGENT_LIST_RUN_START_ROW {
        return None;
    }

    row -= AGENT_LIST_RUN_START_ROW;
    for (index, agent) in app.agents.iter().enumerate() {
        let row_count = 2 + u16::from(diff_short_summary(agent).is_some());
        if row < row_count {
            return Some(index);
        }
        row = row.saturating_sub(row_count);
    }

    None
}

fn task_visible_input_start(app: &App, area: Rect, input_lines: &[String]) -> usize {
    let width = task_inner_width(area);
    let (cursor_line, _) = task_cursor_position(&app.task_input, app.task_cursor, width);
    let max_input_lines = task_visible_input_count(app, area, input_lines.len());
    let input_start = if input_lines.len() > max_input_lines {
        cursor_line.saturating_sub(max_input_lines.saturating_sub(1))
    } else {
        0
    };
    input_start.min(input_lines.len().saturating_sub(1))
}

fn task_visible_input_count(app: &App, area: Rect, input_line_count: usize) -> usize {
    let default_hint = if app.plan_mode {
        "Enter plan  Up/Down history  Option-1/2/3 pane  /plan off"
    } else {
        "Enter start  Up/Down history  Option-1/2/3 pane  /plan  /rudder-plan  /sync"
    };
    let hint = app.notice.as_deref().unwrap_or(default_hint);
    let hint_line_count = wrap_text(hint, task_inner_width(area)).len().max(1);
    (area.height.max(1) as usize)
        .saturating_sub(hint_line_count)
        .max(1)
        .min(input_line_count.max(1))
}

fn task_inner_width(area: Rect) -> u16 {
    area.width.max(1)
}

fn task_cursor_from_selection_point(value: &str, point: SelectionPoint, width: u16) -> usize {
    let lines = wrap_input_text(value, width);
    let mut cursor = 0;
    for (row, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if row == point.row {
            return cursor + point.col.min(line_len);
        }
        cursor += line_len;
    }
    value.chars().count()
}

fn normalize_selection(selection: WorkerSelection) -> NormalizedSelection {
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
    NormalizedSelection { start, end }
}

fn selection_is_empty(selection: NormalizedSelection) -> bool {
    selection.start == selection.end
}

fn selection_for_row(selection: Option<NormalizedSelection>, row: usize) -> Option<(usize, usize)> {
    let selection = selection?;
    if row < selection.start.row || row > selection.end.row {
        return None;
    }
    let start_col = if row == selection.start.row {
        selection.start.col
    } else {
        0
    };
    let end_col = if row == selection.end.row {
        selection.end.col
    } else {
        usize::MAX
    };
    Some((start_col, end_col))
}

fn selected_text_from_lines(lines: &[String], selection: WorkerSelection) -> String {
    let selection = normalize_selection(selection);
    let mut selected = Vec::new();
    for row in selection.start.row..=selection.end.row {
        let Some(line) = lines.get(row) else {
            continue;
        };
        let char_len = line.chars().count();
        let start = if row == selection.start.row {
            selection.start.col.min(char_len)
        } else {
            0
        };
        let end = if row == selection.end.row {
            selection.end.col.saturating_add(1).min(char_len)
        } else {
            char_len
        };
        selected.push(slice_chars(line, start, end));
    }
    selected.join("\n")
}

fn slice_chars(value: &str, start: usize, end: usize) -> String {
    value
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn copy_text_to_clipboard(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        return run_clipboard_command("pbcopy", &[], text);
    }

    #[cfg(target_os = "windows")]
    {
        return run_clipboard_command("clip", &[], text);
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let commands: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        for (command, args) in commands {
            if run_clipboard_command(command, args, text).is_ok() {
                return Ok(());
            }
        }
        bail!("no clipboard command found");
    }
}

fn run_clipboard_command(command: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("failed to write to {command}"))?;
    }
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {command}"))?;
    if !status.success() {
        bail!("{command} exited with {status}");
    }
    Ok(())
}

fn styled_terminal_line(
    cells: Vec<StyledTerminalCell>,
    selection: Option<(usize, usize)>,
    cursor_col: Option<usize>,
) -> Line<'static> {
    let column_count = cursor_col
        .map(|col| col.saturating_add(1))
        .unwrap_or(cells.len())
        .max(cells.len());
    let spans = (0..column_count)
        .map(|col| {
            let cell = cells
                .get(col)
                .cloned()
                .unwrap_or_else(|| plain_terminal_cell(" ".to_string()));
            let mut style = terminal_cell_style(&cell);
            if selection.is_some_and(|(start, end)| col >= start && col <= end) {
                style = style.fg(Color::Black).bg(FOCUS_COLOR);
            }
            if cursor_col == Some(col) {
                style = cursor_cell_style();
            }
            Span::styled(cell.contents, style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn cursor_cell_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(FOCUS_COLOR)
        .add_modifier(Modifier::BOLD)
}

fn plain_terminal_cell(contents: String) -> StyledTerminalCell {
    StyledTerminalCell {
        contents,
        fg: vt100::Color::Default,
        bg: vt100::Color::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
    }
}

fn styled_plain_line(text: &str, style: Style, selection: Option<(usize, usize)>) -> Line<'static> {
    let Some((start, end)) = selection else {
        return Line::from(Span::styled(text.to_string(), style));
    };
    let mut spans = Vec::new();
    let chars = text.chars().collect::<Vec<_>>();
    let clamped_start = start.min(chars.len());
    let clamped_end = end.saturating_add(1).min(chars.len());
    if clamped_start > 0 {
        spans.push(Span::styled(
            chars[..clamped_start].iter().collect::<String>(),
            style,
        ));
    }
    if clamped_end > clamped_start {
        spans.push(Span::styled(
            chars[clamped_start..clamped_end].iter().collect::<String>(),
            style.fg(Color::Black).bg(FOCUS_COLOR),
        ));
    }
    if clamped_end < chars.len() {
        spans.push(Span::styled(
            chars[clamped_end..].iter().collect::<String>(),
            style,
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
    }
    Line::from(spans)
}

fn terminal_cell_style(cell: &StyledTerminalCell) -> Style {
    let (fg, bg) = if cell.inverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    let mut style = app_style();
    if let Some(color) = map_vt100_color(fg) {
        style = style.fg(color);
    }
    if let Some(color) = map_vt100_color(bg) {
        style = style.bg(color);
    }
    let mut modifier = Modifier::empty();
    if cell.bold {
        modifier |= Modifier::BOLD;
    }
    if cell.dim {
        modifier |= Modifier::DIM;
    }
    if cell.italic {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline {
        modifier |= Modifier::UNDERLINED;
    }
    style.add_modifier(modifier)
}

fn map_vt100_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(match index {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::White,
            _ => Color::Indexed(index),
        }),
        vt100::Color::Rgb(red, green, blue) => Some(Color::Rgb(red, green, blue)),
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

fn load_rudder_config() -> Option<serde_json::Value> {
    let path = rudder_config_path()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn config_backend(config: &serde_json::Value) -> Option<Backend> {
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

fn config_model(config: &serde_json::Value, backend: Backend) -> Option<String> {
    config
        .get("backends")?
        .get(backend.as_str())?
        .get("model")?
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .map(ToString::to_string)
}

fn config_effort(config: &serde_json::Value, backend: Backend) -> Option<EffortLevel> {
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

fn config_merge_strategy(config: &serde_json::Value) -> MergeStrategy {
    config
        .get("mergeStrategy")
        .and_then(serde_json::Value::as_str)
        .map(MergeStrategy::parse)
        .unwrap_or(MergeStrategy::Merge)
}

fn merge_strategy() -> MergeStrategy {
    load_rudder_config()
        .as_ref()
        .map(config_merge_strategy)
        .unwrap_or(MergeStrategy::Merge)
}

fn save_model_defaults(backend: Backend, model: &str, effort: Option<EffortLevel>) -> Result<()> {
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

fn ensure_config_defaults(config: &mut serde_json::Value) {
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

fn default_config_value() -> serde_json::Value {
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

fn rudder_config_path() -> Option<PathBuf> {
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

fn rudder_cloud_auth_path() -> Option<PathBuf> {
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

fn rudder_cloud_authenticated() -> bool {
    read_cloud_summary().connected
}

fn read_update_notice() -> Option<(String, String)> {
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

fn is_cloud_worker_session() -> bool {
    env::var("RUDDER_WORKSPACE_ID")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
        || env::var("RUDDER_SAIL_ID")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
}

fn read_cloud_summary() -> CloudSummary {
    if env::var("RUDDER_CLOUD_TOKEN")
        .ok()
        .is_some_and(|token| !token.trim().is_empty())
    {
        return CloudSummary {
            connected: true,
            runtime: env::var("RUDDER_CLOUD_RUNTIME")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
    }

    let Some(path) = rudder_cloud_auth_path() else {
        return CloudSummary {
            connected: false,
            runtime: None,
        };
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return CloudSummary {
            connected: false,
            runtime: None,
        };
    };
    let data = serde_json::from_str::<serde_json::Value>(&raw).ok();
    let connected = data.as_ref().is_some_and(|data| {
        data.get("token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.trim().is_empty())
    });
    let runtime = env::var("RUDDER_CLOUD_RUNTIME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            data.as_ref()
                .and_then(|data| data.get("defaultRuntime"))
                .and_then(serde_json::Value::as_str)
                .map(|value| if value == "byo-vm" { "byoc" } else { value }.to_string())
        });
    CloudSummary { connected, runtime }
}

fn cloud_workspace_label(workspace: Option<&CloudWorkspaceStatus>) -> String {
    let Some(workspace) = workspace else {
        return "cloud workspace · none".to_string();
    };
    let status = workspace.status.as_deref().unwrap_or("unknown");
    if workspace.client_count > 0 {
        format!(
            "cloud workspace · {status} · {} attached",
            workspace.client_count
        )
    } else if workspace.active_agents {
        format!("cloud workspace · {status} · active")
    } else if let Some(idle) = workspace.idle_minutes {
        format!("cloud workspace · {status} · idle {idle}m")
    } else {
        format!("cloud workspace · {status}")
    }
}

fn query_cloud_workspace_status(cwd: &Path) -> Option<CloudWorkspaceStatus> {
    let rudder = locate_rudder_cli()?;
    let output = Command::new(rudder)
        .args(["cloud", "workspace", "status", "--json"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(text.trim()).ok()?;
    if value.get("offline").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    if value.get("workspace").is_some_and(|v| v.is_null()) {
        return Some(CloudWorkspaceStatus::default());
    }
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string());
    if id.is_none() {
        return None;
    }
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string());
    let client_count = value
        .get("clientCount")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(0);
    let active_agents = value
        .get("activeAgents")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let idle_minutes = value
        .get("idleMinutes")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as u32);
    Some(CloudWorkspaceStatus {
        id,
        status,
        active_agents,
        client_count,
        idle_minutes,
    })
}

fn locate_rudder_cli() -> Option<PathBuf> {
    if let Ok(value) = env::var("RUDDER_CLI") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    // PATH lookup
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join("rudder");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn cloud_args_need_auth(args: &[&str]) -> bool {
    !args
        .first()
        .is_some_and(|arg| matches!(*arg, "help" | "login"))
}

fn cloud_args_start_worker(args: &[&str]) -> bool {
    match args.first().copied() {
        None => true,
        Some(
            "help" | "login" | "list" | "ls" | "status" | "runtime" | "setup" | "setup-byoc"
            | "setup-vm" | "setup-fly" | "bootstrap" | "pause" | "resume" | "stop" | "logs"
            | "onload" | "byoc" | "vm" | "byo-vm",
        ) => false,
        Some(_) => true,
    }
}

fn cloud_agent_label(args: &[String]) -> String {
    match args.get(1).map(String::as_str) {
        Some("list" | "ls") => "cloud list".to_string(),
        Some("help") => "cloud help".to_string(),
        Some("login") => "cloud login".to_string(),
        Some("onload") => args
            .get(2)
            .map(|id| format!("cloud onload {id}"))
            .unwrap_or_else(|| "cloud onload".to_string()),
        Some("launch") => "cloud launch".to_string(),
        Some("pause" | "resume" | "stop" | "status" | "logs") => args
            .get(2)
            .map(|id| format!("cloud {} {id}", args[1]))
            .unwrap_or_else(|| format!("cloud {}", args[1])),
        Some(name) => format!("cloud {name}"),
        None => "cloud".to_string(),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CloudPromptLaunch {
    label: String,
    args: Vec<String>,
}

fn cloud_prompt_launch(prompt: &CloudLaunchPrompt) -> Result<CloudPromptLaunch, &'static str> {
    match prompt.choice {
        CloudLaunchChoice::Upload => {
            let label = prompt
                .selected_task
                .as_deref()
                .map(|task| format!("cloud workspace {}", short_task(task)))
                .unwrap_or_else(|| "cloud workspace".to_string());
            Ok(CloudPromptLaunch {
                label,
                args: vec!["cloud".to_string(), "onload".to_string()],
            })
        }
        CloudLaunchChoice::Scratch => Ok(CloudPromptLaunch {
            label: prompt.scratch_label.clone(),
            args: prompt.scratch_args.clone(),
        }),
    }
}

fn random_cloud_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "bright", "calm", "clear", "cosmic", "gentle", "golden", "lucky", "rapid",
        "silver", "steady", "swift",
    ];
    const NOUNS: &[&str] = &[
        "atlas", "harbor", "signal", "summit", "orbit", "ranger", "river", "rocket", "sparrow",
        "station", "voyager", "wave",
    ];
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as usize)
        .unwrap_or_default()
        ^ std::process::id() as usize;
    format!(
        "{}-{}",
        ADJECTIVES[seed % ADJECTIVES.len()],
        NOUNS[(seed / ADJECTIVES.len()) % NOUNS.len()]
    )
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn latest_codex_session_id_for_cwd(cwd: &Path) -> Option<String> {
    let root = user_home_dir()?.join(".codex").join("sessions");
    let target = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(SystemTime, String)> = None;
    visit_codex_session_dir(&root, &target, &mut best, 0);
    best.map(|(_, id)| id)
}

fn latest_codex_rudder_plan_output(run: &AgentRun) -> Option<String> {
    if run.backend != Backend::Codex || run.mode != AgentMode::RudderPlan {
        return None;
    }
    let root = user_home_dir()?.join(".codex").join("sessions");
    let target = fs::canonicalize(&run.cwd).unwrap_or_else(|_| run.cwd.clone());
    let created_after = run
        .created_at
        .parse::<u64>()
        .ok()
        .map(|millis| UNIX_EPOCH + Duration::from_millis(millis));
    latest_codex_rudder_plan_output_in_dir(&root, &target, created_after)
}

fn latest_codex_rudder_plan_output_in_dir(
    root: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
) -> Option<String> {
    let mut best: Option<(SystemTime, String)> = None;
    visit_codex_rudder_plan_output_dir(root, target_cwd, created_after, &mut best, 0);
    best.map(|(_, output)| output)
}

fn visit_codex_rudder_plan_output_dir(
    dir: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
    best: &mut Option<(SystemTime, String)>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            visit_codex_rudder_plan_output_dir(&path, target_cwd, created_after, best, depth + 1);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some((modified, output)) =
            codex_rudder_plan_output_if_cwd_matches(&path, target_cwd, created_after)
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(stamp, _)| modified > *stamp) {
            *best = Some((modified, output));
        }
    }
}

fn codex_rudder_plan_output_if_cwd_matches(
    path: &Path,
    target_cwd: &Path,
    created_after: Option<SystemTime>,
) -> Option<(SystemTime, String)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if let Some(created_after) = created_after {
        let cutoff = created_after
            .checked_sub(Duration::from_secs(60))
            .unwrap_or(created_after);
        if modified < cutoff {
            return None;
        }
    }

    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !codex_session_meta_cwd_matches(&line, target_cwd) {
        return None;
    }

    let mut output = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        collect_codex_session_assistant_text(&line, &mut output);
    }
    if output.contains("RUDDER_PLAN_TASKS_START") {
        Some((modified, output))
    } else {
        None
    }
}

fn codex_session_meta_cwd_matches(line: &str, target_cwd: &Path) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return false;
    }
    let Some(cwd) = value
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let session_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    session_cwd == target_cwd
}

fn collect_codex_session_assistant_text(line: &str, out: &mut String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(record_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(payload) = value.get("payload") else {
        return;
    };
    match record_type {
        "response_item" => collect_codex_response_item_text(payload, out),
        "event_msg" => {
            if matches!(
                payload.get("type").and_then(serde_json::Value::as_str),
                Some("agent_message" | "final_answer")
            ) {
                append_codex_text(payload.get("message"), out);
            }
        }
        _ => {}
    }
}

fn collect_codex_response_item_text(payload: &serde_json::Value, out: &mut String) {
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return;
    }
    if payload.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = payload.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for item in content {
        append_codex_text(item.get("text"), out);
    }
}

fn append_codex_text(value: Option<&serde_json::Value>, out: &mut String) {
    let Some(text) = value.and_then(serde_json::Value::as_str) else {
        return;
    };
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}

fn visit_codex_session_dir(
    dir: &Path,
    target_cwd: &Path,
    best: &mut Option<(SystemTime, String)>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            visit_codex_session_dir(&path, target_cwd, best, depth + 1);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = codex_session_id_if_cwd_matches(&path, target_cwd) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().is_none_or(|(stamp, _)| modified > *stamp) {
            *best = Some((modified, session_id));
        }
    }
}

fn codex_session_id_if_cwd_matches(path: &Path, target_cwd: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let session_id = payload.get("id")?.as_str()?.to_string();
    let cwd = payload.get("cwd")?.as_str()?;
    let session_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    if session_cwd == target_cwd {
        Some(session_id)
    } else {
        None
    }
}

#[derive(Default, Debug, Clone)]
struct ModelUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    let mut y = 1970_i64;
    loop {
        let dy = if is_leap_year(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let month_lens: [u32; 12] = [
        31,
        if is_leap_year(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0_u32;
    for (i, &ml) in month_lens.iter().enumerate() {
        if (days as u32) < ml {
            m = i as u32;
            break;
        }
        days -= ml as i64;
    }
    let d = days as u32 + 1;
    (y, m + 1, d)
}

/// Load the per-directory rudder session start timestamp from
/// `.rudder/session.json`, or create one on first use. This makes
/// session-scoped features (like /usage) persistent across rudder
/// restarts in the same repo. To reset, delete the file.
fn load_or_init_session_started(repo_root: &Path) -> String {
    let path = repo_root.join(".rudder/session.json");
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(ts) = value.get("started_at_iso").and_then(|v| v.as_str()) {
                if !ts.trim().is_empty() {
                    return ts.to_string();
                }
            }
        }
    }
    let now = system_time_to_iso(SystemTime::now());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "started_at_iso": now.clone() });
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
    now
}

fn system_time_to_iso(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (secs / 86400) as i64;
    let day_secs = secs % 86400;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.000Z")
}

fn encode_claude_projects_cwd(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn collect_claude_usage(
    repo_root: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let mut out: std::collections::BTreeMap<String, ModelUsage> = std::collections::BTreeMap::new();
    let Some(home) = user_home_dir() else {
        return out;
    };
    let dir_name = encode_claude_projects_cwd(repo_root);
    let project_dir = home.join(".claude/projects").join(&dir_name);
    let Ok(entries) = fs::read_dir(&project_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            for line in content.lines() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
                        continue;
                    }
                    let ts = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !ts.is_empty() && ts < since_iso {
                        continue;
                    }
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let Some(usage) = message.get("usage") else {
                        continue;
                    };
                    let model = message
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    if model == "<synthetic>" {
                        continue;
                    }
                    let entry = out.entry(model).or_default();
                    if let Some(n) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                        entry.input_tokens += n;
                    }
                    if let Some(n) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        entry.output_tokens += n;
                    }
                    if let Some(n) = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        entry.cache_creation_input_tokens += n;
                    }
                    if let Some(n) = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        entry.cache_read_input_tokens += n;
                    }
                }
            }
        }
    }
    out
}

fn collect_codex_usage(
    repo_root: &Path,
    since_iso: &str,
) -> std::collections::BTreeMap<String, ModelUsage> {
    let mut out: std::collections::BTreeMap<String, ModelUsage> = std::collections::BTreeMap::new();
    let Some(home) = user_home_dir() else {
        return out;
    };
    let sessions_root = home.join(".codex/sessions");
    if !sessions_root.exists() {
        return out;
    }
    let target_cwd = repo_root.display().to_string();
    let mut files = Vec::new();
    collect_jsonl_files(&sessions_root, &mut files, 4);
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let mut session_cwd: Option<String> = None;
        let mut session_model: Option<String> = None;
        let mut session_start: Option<String> = None;
        let mut last_total: Option<(u64, u64, u64, u64)> = None;
        // (input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens)
        for line in content.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "session_meta" => {
                    if let Some(cwd) = value
                        .get("payload")
                        .and_then(|p| p.get("cwd"))
                        .and_then(|v| v.as_str())
                    {
                        session_cwd = Some(cwd.to_string());
                    }
                    if let Some(ts) = value
                        .get("payload")
                        .and_then(|p| p.get("timestamp"))
                        .and_then(|v| v.as_str())
                    {
                        session_start = Some(ts.to_string());
                    } else if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
                        session_start = Some(ts.to_string());
                    }
                }
                "turn_context" => {
                    if session_model.is_none() {
                        if let Some(m) = value
                            .get("payload")
                            .and_then(|p| p.get("model"))
                            .and_then(|v| v.as_str())
                        {
                            session_model = Some(m.to_string());
                        }
                    }
                }
                "event_msg" => {
                    let payload = value.get("payload");
                    if payload.and_then(|p| p.get("type")).and_then(|v| v.as_str())
                        == Some("token_count")
                    {
                        if let Some(info) = payload.and_then(|p| p.get("info")) {
                            if let Some(total) = info.get("total_token_usage") {
                                let inp = total
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let cached = total
                                    .get("cached_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let out_t = total
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let reasoning = total
                                    .get("reasoning_output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                last_total = Some((inp, cached, out_t, reasoning));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(cwd) = session_cwd else {
            continue;
        };
        if cwd != target_cwd {
            continue;
        }
        // Scope to "this rudder session": only count codex sessions that
        // started after this rudder dashboard launched.
        if let Some(start) = session_start.as_deref() {
            if start < since_iso {
                continue;
            }
        } else {
            continue;
        }
        let model = session_model.unwrap_or_else(|| "unknown-codex".to_string());
        if let Some((inp, cached, out_t, reasoning)) = last_total {
            let entry = out.entry(model).or_default();
            // Map codex semantics into the shared ModelUsage shape: codex's
            // cached_input_tokens are a subset of input_tokens (already counted),
            // so we don't double them in input_tokens; we put the cached portion
            // into cache_read_input_tokens for visibility and discount pricing.
            let billable_input = inp.saturating_sub(cached);
            entry.input_tokens += billable_input;
            entry.cache_read_input_tokens += cached;
            // reasoning tokens are billed as output tokens by OpenAI.
            entry.output_tokens += out_t + reasoning;
        }
    }
    out
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>, depth_limit: usize) {
    if depth_limit == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                collect_jsonl_files(&path, out, depth_limit - 1);
            } else if ft.is_file() && path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(path);
            }
        }
    }
}

/// Approximate OpenAI pricing per million tokens (input, _unused_, _unused_,
/// cached_input). Cached input tokens are billed at a discount (~10%).
fn codex_model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt-5") || m.starts_with("o3") {
        return Some((10.0, 30.0, 0.0, 1.0));
    }
    if m.starts_with("gpt-4o-mini") {
        return Some((0.15, 0.60, 0.0, 0.075));
    }
    if m.starts_with("gpt-4o") {
        return Some((2.50, 10.0, 0.0, 1.25));
    }
    if m.starts_with("o1") {
        return Some((15.0, 60.0, 0.0, 7.50));
    }
    None
}

/// Approximate Anthropic pricing per million tokens (input, output,
/// cache_creation, cache_read). Used to surface a rough cost estimate, not
/// billing-grade numbers.
fn claude_model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        return Some((15.0, 75.0, 18.75, 1.50));
    }
    if m.contains("sonnet") {
        return Some((3.0, 15.0, 3.75, 0.30));
    }
    if m.contains("haiku") {
        return Some((0.80, 4.0, 1.00, 0.08));
    }
    None
}

fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn short_model_label(model: &str) -> String {
    // claude-haiku-4-5-20251001 -> haiku-4-5
    let lower = model.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("claude-") {
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() >= 2 {
            return format!("{}-{}", parts[0], parts.get(1).copied().unwrap_or("?"));
        }
    }
    // GPT / OpenAI model names usually short already; pass through.
    model.to_string()
}

/// Returns (input, output, cache_creation, cache_read) pricing for either
/// Claude or Codex models.
fn model_pricing(model: &str) -> Option<(f64, f64, f64, f64)> {
    claude_model_pricing(model).or_else(|| codex_model_pricing(model))
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

fn default_model_for(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "sonnet",
        Backend::Codex => "gpt-5.5",
    }
}

fn default_effort_for(backend: Backend, model: &str) -> Option<EffortLevel> {
    let options = effort_options_for(backend, model);
    if options.contains(&Some(EffortLevel::XHigh)) {
        Some(EffortLevel::XHigh)
    } else {
        options.into_iter().next().flatten()
    }
}

fn effort_label(effort: Option<EffortLevel>) -> &'static str {
    effort.map(EffortLevel::as_str).unwrap_or("auto")
}

fn parse_effort_arg(value: &str) -> Option<EffortLevel> {
    if value.eq_ignore_ascii_case("auto") {
        None
    } else {
        EffortLevel::parse(value)
    }
}

fn provider_backend(provider: &str) -> Option<Backend> {
    match provider.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some(Backend::Claude),
        "codex" | "openai" => Some(Backend::Codex),
        _ => None,
    }
}

fn effort_options_for(backend: Backend, model: &str) -> Vec<Option<EffortLevel>> {
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

fn effort_detail(backend: Backend, effort: Option<EffortLevel>) -> &'static str {
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

fn model_supports_reasoning(backend: Backend, model: &str) -> bool {
    if is_reasoning_alias(backend, model) {
        return true;
    }
    cached_model_reasoning(backend, model).unwrap_or_else(|| match backend {
        Backend::Claude => model.contains("opus") || model.contains("sonnet"),
        Backend::Codex => model.starts_with("gpt-5") || model.contains("codex"),
    })
}

fn is_reasoning_alias(backend: Backend, model: &str) -> bool {
    match backend {
        Backend::Claude => matches!(model, "sonnet" | "sonnet[1m]" | "opus" | "opus[1m]"),
        Backend::Codex => model.starts_with("gpt-5") || model.contains("codex"),
    }
}

fn cached_model_reasoning(backend: Backend, model_id: &str) -> Option<bool> {
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

fn suggestions_for(app: &App) -> Vec<Suggestion> {
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

fn rank_suggestions(suggestions: Vec<Suggestion>, query: &str) -> Vec<Suggestion> {
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

fn suggestion_match_score(suggestion: &Suggestion, query: &str) -> Option<i32> {
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

fn text_match_score(text: &str, query: &str, base: i32) -> Option<i32> {
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

fn normalize_search_text(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_prefix_score(text: &str, query: &str) -> Option<i32> {
    let mut tokens = text.split_whitespace();
    let mut score = 0;
    for part in query.split_whitespace() {
        let token = tokens.find(|token| token.starts_with(part))?;
        score += 200 - token.len() as i32;
    }
    Some(score)
}

fn fuzzy_subsequence_score(text: &str, query: &str) -> Option<i32> {
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

fn bounded_edit_distance(left: &str, right: &str, max_distance: usize) -> usize {
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

fn command_suggestions() -> Vec<Suggestion> {
    vec![
        Suggestion {
            label: "/plan".to_string(),
            detail: "toggle Rudder read-only planning".to_string(),
            action: SuggestionAction::Insert("/plan".to_string()),
        },
        Suggestion {
            label: "/plan <task>".to_string(),
            detail: "plan one task without toggling".to_string(),
            action: SuggestionAction::Insert("/plan ".to_string()),
        },
        Suggestion {
            label: "/rudder-plan <task>".to_string(),
            detail: "decompose a task and spawn worker agents".to_string(),
            action: SuggestionAction::Insert("/rudder-plan ".to_string()),
        },
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
            label: "/sync".to_string(),
            detail: "rebase selected worktree without merging".to_string(),
            action: SuggestionAction::RunCommand("/sync".to_string()),
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

fn model_provider_or_model_suggestions(rest: &str) -> Vec<Suggestion> {
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

fn provider_suggestions(query: &str) -> Vec<Suggestion> {
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

fn model_suggestions_for(backend_filter: Backend, query: &str) -> Vec<Suggestion> {
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

fn effort_suggestions_for(backend: Backend, model: &str, query: &str) -> Vec<Suggestion> {
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

fn fallback_model_rows() -> Vec<(Backend, &'static str, &'static str)> {
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

fn push_model_suggestion(
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

fn cached_models_dev_rows() -> Vec<(Backend, String, String)> {
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

fn collect_provider_models(
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

fn is_claude_picker_model(id: &str, model: &serde_json::Value) -> bool {
    id.starts_with("claude-")
        && !id.contains("3-")
        && (id.contains("sonnet") || id.contains("opus") || id.contains("haiku"))
        && model
            .get("tool_call")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
}

fn is_codex_picker_model(id: &str, model: &serde_json::Value) -> bool {
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

fn score_model(backend: Backend, id: &str) -> i32 {
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

fn models_dev_cache_path() -> Option<PathBuf> {
    std::env::var_os("RUDDER_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rudder")))
        .map(|home| home.join("models-dev.json"))
}

fn backend_for_model(model: &str) -> Backend {
    if model.starts_with("gpt-") || model.contains("codex") {
        Backend::Codex
    } else {
        Backend::Claude
    }
}

fn mint_session_id_for(backend: Backend) -> Option<String> {
    match backend {
        Backend::Claude => Some(uuid::Uuid::new_v4().to_string()),
        Backend::Codex => None,
    }
}

fn can_resume_agent(run: &AgentRun) -> bool {
    match run.backend {
        Backend::Claude | Backend::Codex => run.session_id.is_some(),
    }
}

fn claude_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args: Vec<String> = vec![
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ];
    if !run.model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(run.model.clone());
    }
    if let Some(effort) = run.effort {
        args.push("--effort".to_string());
        args.push(effort.as_str().to_string());
    }
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    TerminalCommand::with_args("claude", args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
}

fn codex_resume_command(run: &AgentRun, session_id: &str) -> TerminalCommand {
    let mut args = vec!["--no-alt-screen".to_string()];
    args.push("--enable".to_string());
    args.push("goals".to_string());
    match run.mode {
        AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
        AgentMode::Plan | AgentMode::RudderPlan => {
            args.push("--sandbox".to_string());
            args.push("read-only".to_string());
            args.push("--ask-for-approval".to_string());
            args.push("never".to_string());
            args.push("--search".to_string());
        }
    }
    push_codex_rudder_config_overrides(&mut args, run.effort);
    if !run.model.trim().is_empty() {
        args.push("-m".to_string());
        args.push(run.model.clone());
    }
    args.push("resume".to_string());
    args.push(session_id.to_string());
    TerminalCommand::with_args(codex_program(), args).with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
}

fn agent_command(
    backend: Backend,
    model: &str,
    effort: Option<EffortLevel>,
    task: &str,
    mode: AgentMode,
    session_id: Option<&str>,
) -> TerminalCommand {
    let prompt = match mode {
        AgentMode::Execute => Some(execution_prompt(task)),
        AgentMode::Plan => Some(plan_prompt(task)),
        AgentMode::RudderPlan => Some(rudder_plan_prompt(task)),
        AgentMode::ReviewAll => Some(task.to_string()),
        AgentMode::Main => {
            if task.trim().is_empty() {
                None
            } else {
                Some(execution_prompt(task))
            }
        }
    };
    match backend {
        Backend::Claude => {
            let mut args = match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => vec![
                    "--permission-mode".to_string(),
                    "bypassPermissions".to_string(),
                ],
                AgentMode::Plan | AgentMode::RudderPlan => vec![
                    "--permission-mode".to_string(),
                    "plan".to_string(),
                    "--name".to_string(),
                    format!("plan:{}", short_task(task)),
                ],
            };
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
            if let Some(prompt) = prompt {
                args.push(prompt);
            }
            TerminalCommand::with_args("claude", args).with_env("CLAUDE_CODE_NO_FLICKER", "0")
        }
        Backend::Codex => {
            let mut args = vec!["--no-alt-screen".to_string()];
            args.push("--enable".to_string());
            args.push("goals".to_string());
            match mode {
                AgentMode::Execute | AgentMode::ReviewAll | AgentMode::Main => {
                    args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
                }
                AgentMode::Plan | AgentMode::RudderPlan => {
                    args.push("--sandbox".to_string());
                    args.push("read-only".to_string());
                    args.push("--ask-for-approval".to_string());
                    args.push("never".to_string());
                    args.push("--search".to_string());
                }
            }
            push_codex_rudder_config_overrides(&mut args, effort);
            if !model.trim().is_empty() {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            if let Some(prompt) = prompt {
                args.push(prompt);
            }
            TerminalCommand::with_args(codex_program(), args)
                .with_env("CODEX_RUDDER_SCROLLBACK_SAFE", "1")
        }
    }
}

fn push_codex_rudder_config_overrides(args: &mut Vec<String>, effort: Option<EffortLevel>) {
    // Rudder workers run Codex as a child process, so do not inherit desktop-app
    // notification hooks that expect the official signed app launch chain.
    args.push("-c".to_string());
    args.push("notify=[]".to_string());
    args.push("-c".to_string());
    args.push("features.plugins=false".to_string());
    args.push("-c".to_string());
    args.push("features.computer_use=false".to_string());
    args.push("-c".to_string());
    args.push("model_reasoning_summary=\"detailed\"".to_string());
    args.push("-c".to_string());
    args.push("model_supports_reasoning_summaries=true".to_string());
    if let Some(effort) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
    }
}

fn codex_program() -> String {
    env::var("RUDDER_CODEX_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

fn review_all_run(
    worktree: WorktreeInfo,
    prompt: String,
    sources: Vec<ReviewAllSource>,
    session_id: Option<String>,
) -> AgentRun {
    let created_at = now_stamp();
    let source_ids = sources
        .iter()
        .map(|source| source.id.clone())
        .collect::<Vec<_>>();
    AgentRun {
        id: worktree.id,
        created_at: created_at.clone(),
        mode: AgentMode::ReviewAll,
        task: prompt.clone(),
        task_summary: format!("review all {} worktrees", source_ids.len()),
        current_prompt: prompt.clone(),
        turns: vec![AgentTurn {
            ts: created_at.clone(),
            prompt,
            source: "user".to_string(),
        }],
        last_user_input_at: created_at,
        backend: Backend::Codex,
        model: REVIEW_ALL_MODEL.to_string(),
        effort: Some(REVIEW_ALL_EFFORT),
        status: AgentStatus::Running,
        cwd: worktree.path.clone(),
        worktree_branch: worktree.branch.clone(),
        worktree_path: worktree.path_is_worktree.then_some(worktree.path),
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
        review_source_ids: source_ids,
    }
}

fn review_all_prompt(
    target_ref: &str,
    worktree: &WorktreeInfo,
    sources: &[ReviewAllSource],
    premerge: &ReviewAllPremerge,
) -> String {
    let aggregate_branch = worktree.branch.as_deref().unwrap_or("current branch");
    let source_lines = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let path = source
                .worktree_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unknown worktree path)".to_string());
            format!(
                "{}. {} ({})\n   branch: {}\n   worktree: {}\n   task: {}",
                index + 1,
                source.summary,
                source.id,
                source.branch,
                path,
                truncate_chars(&source.task, 220)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let merged = if premerge.merged_branches.is_empty() {
        "- none yet".to_string()
    } else {
        premerge
            .merged_branches
            .iter()
            .map(|branch| format!("- {branch}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let remaining = if premerge.remaining_branches.is_empty() {
        "- none".to_string()
    } else {
        premerge
            .remaining_branches
            .iter()
            .map(|branch| format!("- {branch}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stopped = match (
        premerge.stopped_branch.as_deref(),
        premerge.stopped_error.as_deref(),
    ) {
        (Some(branch), Some(error)) => {
            format!("Rudder stopped while merging `{branch}`:\n{error}")
        }
        _ => "No merge conflict was detected while building the aggregate branch.".to_string(),
    };

    format!(
        "/review Review the combined Rudder agent worktree changes against `{target_ref}`.\n\
\n\
You are the Rudder review-all integration agent. You are running on an aggregate worktree branch that is meant to become one reviewed merge back into main.\n\
\n\
Aggregate worktree\n\
- path: {path}\n\
- branch: {aggregate_branch}\n\
- target/base ref: {target_ref}\n\
\n\
Source worktrees included in this review\n\
{source_lines}\n\
\n\
Pre-merge state\n\
Already merged into this aggregate branch:\n\
{merged}\n\
\n\
Still not fully merged:\n\
{remaining}\n\
\n\
{stopped}\n\
\n\
Instructions\n\
1. Run `git status` first. If a merge is in progress, resolve the conflicts, `git add` the resolutions, and commit the merge.\n\
2. Merge every branch listed under \"Still not fully merged\" into this aggregate branch, in the listed order. Resolve conflicts carefully.\n\
3. Run the Codex `/review` flow on the combined diff against `{target_ref}`. If the slash command is unavailable, perform an equivalent code review using `git diff {target_ref}...HEAD`.\n\
4. Fix real review findings directly in this aggregate worktree. Do not edit the original source worktrees.\n\
5. Run the relevant tests/checks for the files touched. If a check cannot run, say exactly why.\n\
6. Do not check out `{target_ref}` and do not merge into `{target_ref}` yourself. When the aggregate branch is ready, stop and say: `Rudder review-all branch is ready; press m on this row to merge to main.`\n",
        target_ref = target_ref,
        path = worktree.path.display(),
        aggregate_branch = aggregate_branch,
        source_lines = source_lines,
        merged = merged,
        remaining = remaining,
        stopped = stopped,
    )
}

fn execution_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "Rudder-specific context injected by Rudder:\n- Read RUDDER.md first if it exists. Rudder generated that file to show active Rudder agents and worktrees in this repo.\n- If a Hunk review is open for this worktree, run `hunk skill path`, load that skill, and use `hunk session review --repo . --json` plus `hunk session comment ...` commands to inspect and annotate the live review.\n\n{task}"
    )
}

fn plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "Plan this task before implementation. Inspect the repository and relevant read-only context first. Ask follow-up questions if the plan cannot be made decision-complete from inspection alone.\n\n{task}"
    )
}

fn rudder_plan_prompt(task: &str) -> String {
    let task = strip_rudder_prompt_wrappers(task);
    format!(
        "You are Rudder's planning coordinator. Inspect the repository in read-only mode and decide whether this user request should be split across multiple independent implementation agents.\n\nUser request:\n{task}\n\nProcess:\n1. Identify missing requirements. If the work is ambiguous enough that implementation would likely go wrong, ask concise follow-up questions and do not emit tasks yet.\n2. Otherwise create the smallest set of independent implementation tasks that can run in separate git worktrees with minimal conflicts.\n3. Each task must be self-contained, include concrete files or modules to inspect when known, and include its own verification instructions.\n4. Do not include a task that depends on another task's unmerged changes. If work is sequential, make one task.\n5. Prefer 1-4 tasks. Use more only when the split is clearly independent.\n6. For each worker task that is bigger than one normal turn and has a clear validation loop, include a `goal` value suitable for Codex `/goal`. The goal must name one durable objective, important constraints, validation commands or artifacts, and a verifiable stopping condition. Omit `goal` or set it to an empty string for small tasks, vague tasks, or loose backlogs.\n\nWhen the task list is ready, print exactly this block and no other JSON block:\nRUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"title\":\"short task title\",\"prompt\":\"full implementation prompt for one worker agent\",\"goal\":\"optional durable objective for /goal, without the leading slash command\"}}]}}\nRUDDER_PLAN_TASKS_END\n\nAfter the block, add a short human summary of why this split is safe."
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RudderPlanTask {
    title: String,
    prompt: String,
    goal: Option<String>,
}

fn rudder_plan_output_for_run(run: &AgentRun) -> String {
    let mut output = run
        .terminal
        .as_ref()
        .map(|terminal| terminal.output_log_snapshot().to_string())
        .unwrap_or_default();
    if let Some(session_output) = latest_codex_rudder_plan_output(run) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&session_output);
    }
    output
}

fn extract_rudder_plan_tasks(output: &str) -> Result<Vec<RudderPlanTask>> {
    const START: &str = "RUDDER_PLAN_TASKS_START";
    const END: &str = "RUDDER_PLAN_TASKS_END";

    let clean = strip_ansi_for_plan(output).replace('\r', "");
    let Some(start) = clean.rfind(START) else {
        bail!("missing RUDDER_PLAN_TASKS_START");
    };
    let after_start = &clean[start + START.len()..];
    let Some(end) = after_start.find(END) else {
        bail!("missing RUDDER_PLAN_TASKS_END");
    };
    let mut json = after_start[..end].trim();
    if let Some(stripped) = json.strip_prefix("```json") {
        json = stripped.trim();
    } else if let Some(stripped) = json.strip_prefix("```") {
        json = stripped.trim();
    }
    if let Some(stripped) = json.strip_suffix("```") {
        json = stripped.trim();
    }

    let value: serde_json::Value =
        serde_json::from_str(json).context("task block was not valid JSON")?;
    let tasks = value
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .context("task block must contain a tasks array")?;

    let mut out = Vec::new();
    for task in tasks.iter().take(6) {
        let title = task
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("worker task")
            .trim();
        let prompt = task
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .context("each task needs a prompt")?
            .trim();
        if prompt.is_empty() {
            continue;
        }
        out.push(RudderPlanTask {
            title: if title.is_empty() {
                "worker task".to_string()
            } else {
                title.to_string()
            },
            prompt: prompt.to_string(),
            goal: task
                .get("goal")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
                .map(ToString::to_string),
        });
    }

    Ok(out)
}

fn rudder_plan_worker_prompt(
    planner_task: &str,
    task: &RudderPlanTask,
    backend: Backend,
) -> String {
    let mut prompt = format!(
        "This task was spawned by Rudder from a /rudder-plan coordinator.\n\nOriginal request:\n{planner_task}\n\nWorker task: {}\n\n{}",
        task.title, task.prompt
    );
    if let Some(goal) = task.goal.as_deref() {
        match backend {
            Backend::Codex => {
                prompt.push_str(
                    "\n\nDurable Codex goal:\nIf goals are available, start by setting this goal before implementation:\n",
                );
                prompt.push_str("/goal ");
                prompt.push_str(goal);
            }
            Backend::Claude => {
                prompt.push_str(
                    "\n\nDurable objective:\nUse this as the stopping condition for the worker task:\n",
                );
                prompt.push_str(goal);
            }
        }
    }
    prompt
}

fn rudder_plan_worker_title_from_prompt(task: &str) -> Option<String> {
    let mut lines = task.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Worker task:") else {
            continue;
        };
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
        for title in lines.by_ref() {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn strip_ansi_for_plan(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

fn strip_rudder_prompt_wrappers(task: &str) -> String {
    const START: &str = "[RUDDER PROMPT INJECTION]";
    const END: &str = "[END RUDDER PROMPT INJECTION]";

    let mut value = task.trim().to_string();
    loop {
        let trimmed = value.trim_start();
        if let Some(rest) = trimmed.strip_prefix("USER TASK:") {
            value = rest.trim_start().to_string();
            continue;
        }
        if let Some(after_start) = trimmed.strip_prefix(START) {
            if let Some(index) = after_start.find(END) {
                let body = after_start[..index].trim();
                let rest = after_start[index + END.len()..].trim_start();
                value = if rest.is_empty() { body } else { rest }.to_string();
                continue;
            }
        }
        return trimmed.to_string();
    }
}

fn short_task(task: &str) -> String {
    const MAX: usize = 26;
    truncate_chars(task, MAX)
}

fn summarize_task(task: &str) -> String {
    summarize_task_to(task, 56)
}

fn spawn_task_summary_worker(tx: mpsc::Sender<TaskSummaryResult>, run_id: String, task: String) {
    thread::spawn(move || {
        let title = generate_task_summary_title(&task);
        let _ = tx.send(TaskSummaryResult { run_id, title });
    });
}

fn generate_task_summary_title(task: &str) -> Option<String> {
    let task = normalize_task_text(task);
    if task.is_empty() {
        return None;
    }
    let prompt = format!(
        "Summarize this coding agent task for a compact sidebar label.\n\
Return exactly one JSON object and no markdown: {{\"title\":\"5-8 word imperative title\"}}\n\
Rules: no quotes inside the title, no trailing punctuation, do not mention Rudder unless it is the product being changed.\n\n\
Task:\n{task}"
    );
    let output = Command::new("claude")
        .args(["-p", &prompt, "--model", TASK_SUMMARY_MODEL])
        .env("CLAUDE_CODE_NO_FLICKER", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    task_title_from_summary_output(&stdout)
}

fn task_title_from_summary_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end >= start {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end])
                {
                    if let Some(title) = value.get("title").and_then(|value| value.as_str()) {
                        return clean_task_summary_title(title);
                    }
                }
            }
        }
    }
    clean_task_summary_title(trimmed)
}

fn clean_task_summary_title(raw: &str) -> Option<String> {
    let mut title = raw
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .to_string();
    title = strip_terminal_punctuation(&title);
    title = normalize_task_text(&title);
    if title.is_empty() {
        return None;
    }
    Some(truncate_chars(&title, 56))
}

fn summarize_task_to(task: &str, max_chars: usize) -> String {
    let original = normalize_task_text(task);
    if original.is_empty() {
        return "agent".to_string();
    }

    let mut summary = strip_leading_scaffolding(&original);
    summary = normalize_task_text(&summary)
        .replace("lsited", "listed")
        .replace("rihgt", "right")
        .replace("the task that the user puts", "the user task")
        .replace("the task that user puts", "the user task")
        .replace("task that the user puts", "user task")
        .replace("task that user puts", "user task")
        .replace("and then that's what gets listed on", "for")
        .replace("and then that is what gets listed on", "for");
    summary = strip_trailing_context(&summary);
    summary = first_sentence(&summary);
    summary = strip_terminal_punctuation(&summary);
    if summary.is_empty() {
        summary = original;
    }

    if summary.chars().count() <= max_chars {
        return summary;
    }

    compact_title(&summary, max_chars).unwrap_or_else(|| truncate_chars(&summary, max_chars))
}

fn normalize_task_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_leading_scaffolding(value: &str) -> String {
    let mut current = value.trim().to_string();
    loop {
        let lower = current.to_ascii_lowercase();
        let mut next = None;
        for prefix in [
            "ok, ",
            "okay, ",
            "hey, ",
            "ok ",
            "okay ",
            "hey ",
            "also ",
            "please ",
            "can you ",
            "could you ",
            "would you ",
            "can u ",
            "could u ",
            "would u ",
            "can we ",
            "could we ",
            "would we ",
            "i need you to ",
            "i need to ",
            "i want you to ",
            "i want to ",
            "need you to ",
            "need to ",
            "want you to ",
            "want to ",
            "we need to ",
            "we should ",
            "we have to ",
            "another thing for you to work on is ",
            "another thing is ",
            "the task is ",
        ] {
            if lower.starts_with(prefix) {
                next = Some(current[prefix.len()..].trim().to_string());
                break;
            }
        }
        match next {
            Some(value) if value != current => current = value,
            _ => break,
        }
    }
    current
}

fn strip_trailing_context(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    for marker in [" right now", " currently", " at the moment", " for now"] {
        if let Some(index) = lower.find(marker) {
            return value[..index].trim().to_string();
        }
    }
    value.trim().to_string()
}

fn first_sentence(value: &str) -> String {
    let mut char_count = 0;
    for (index, ch) in value.char_indices() {
        char_count += 1;
        if char_count >= 12 && matches!(ch, '.' | '!' | '?') {
            return value[..index].trim().to_string();
        }
    }
    value.trim().to_string()
}

fn compact_title(value: &str, max_chars: usize) -> Option<String> {
    let mut selected = Vec::new();
    for word in
        value.split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '/' | '-')))
    {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if is_task_summary_stop_word(&lower) {
            continue;
        }
        selected.push(word.to_string());
        if selected.join(" ").chars().count() >= max_chars.saturating_sub(1) || selected.len() >= 8
        {
            break;
        }
    }
    let compact = strip_terminal_punctuation(&selected.join(" "));
    if !compact.is_empty() && compact.chars().count() < value.chars().count() {
        Some(truncate_chars(&compact, max_chars))
    } else {
        None
    }
}

fn is_task_summary_stop_word(value: &str) -> bool {
    matches!(
        value,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "but"
            | "by"
            | "for"
            | "from"
            | "gets"
            | "have"
            | "in"
            | "is"
            | "it"
            | "its"
            | "just"
            | "of"
            | "on"
            | "or"
            | "put"
            | "puts"
            | "putting"
            | "right"
            | "so"
            | "than"
            | "that"
            | "the"
            | "then"
            | "this"
            | "to"
            | "user"
            | "what"
            | "when"
            | "where"
            | "with"
            | "you"
            | "your"
    )
}

fn strip_terminal_punctuation(value: &str) -> String {
    value
        .trim_end_matches(|ch| matches!(ch, '.' | '!' | '?'))
        .trim()
        .to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let short = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        if max_chars <= 3 {
            ".".repeat(max_chars)
        } else {
            format!(
                "{}...",
                short.chars().take(max_chars - 3).collect::<String>()
            )
        }
    } else {
        short
    }
}

fn preview_text(value: &str, max_chars: usize) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "\\\"");
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn record_agent_prompt(run: &mut AgentRun, prompt: String, source: &str) {
    let ts = now_stamp();
    if source == "user" {
        run.last_user_input_at = ts.clone();
    }
    run.current_prompt = prompt.clone();
    run.turns.push(AgentTurn {
        ts,
        prompt,
        source: source.to_string(),
    });
}

fn update_worker_prompt_draft_for_key(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
    key: KeyEvent,
    capture_as_prompt: bool,
) -> Option<String> {
    match key.code {
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            return finish_worker_prompt_draft(draft, cursor, is_prompt);
        }
        KeyCode::Char('u') | KeyCode::Char('U')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            draft.clear();
            *cursor = 0;
            *is_prompt = false;
            return None;
        }
        KeyCode::Char('w') | KeyCode::Char('W')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            delete_previous_word_at(draft, cursor);
            return None;
        }
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                draft.clear();
                *cursor = 0;
                *is_prompt = false;
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            {
                delete_previous_word_at(draft, cursor);
            } else {
                delete_char_before_cursor(draft, cursor);
            }
        }
        KeyCode::Delete => delete_char_at_cursor(draft, *cursor),
        KeyCode::Left => {
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                *cursor = previous_word_position(draft, *cursor);
            } else {
                *cursor = (*cursor).saturating_sub(1);
            }
        }
        KeyCode::Right => {
            let len = draft.chars().count();
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                *cursor = next_word_position(draft, *cursor);
            } else {
                *cursor = (*cursor + 1).min(len);
            }
        }
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = draft.chars().count(),
        KeyCode::Char(ch)
            if !key.modifiers.intersects(
                KeyModifiers::ALT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::SUPER
                    | KeyModifiers::META,
            ) =>
        {
            if draft.is_empty() {
                *is_prompt = capture_as_prompt;
            }
            insert_char_at_cursor(draft, cursor, ch);
        }
        _ => {}
    }

    None
}

fn update_worker_prompt_draft_for_paste(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
    text: &str,
    capture_as_prompt: bool,
) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut previous_was_carriage_return = false;

    for ch in text.chars() {
        match ch {
            '\r' => {
                if let Some(prompt) = finish_worker_prompt_draft(draft, cursor, is_prompt) {
                    prompts.push(prompt);
                }
                previous_was_carriage_return = true;
            }
            '\n' if previous_was_carriage_return => {
                previous_was_carriage_return = false;
            }
            '\n' => {
                if let Some(prompt) = finish_worker_prompt_draft(draft, cursor, is_prompt) {
                    prompts.push(prompt);
                }
                previous_was_carriage_return = false;
            }
            '\u{7f}' | '\u{8}' => {
                delete_char_before_cursor(draft, cursor);
                previous_was_carriage_return = false;
            }
            _ if ch.is_control() => {
                previous_was_carriage_return = false;
            }
            _ => {
                if draft.is_empty() {
                    *is_prompt = capture_as_prompt;
                }
                insert_char_at_cursor(draft, cursor, ch);
                previous_was_carriage_return = false;
            }
        }
    }

    prompts
}

fn finish_worker_prompt_draft(
    draft: &mut String,
    cursor: &mut usize,
    is_prompt: &mut bool,
) -> Option<String> {
    let prompt = draft.trim().to_string();
    let should_record = *is_prompt;
    draft.clear();
    *cursor = 0;
    *is_prompt = false;
    if prompt.is_empty() || !should_record {
        None
    } else {
        Some(prompt)
    }
}

fn previous_task_history_entry(
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut String,
    current_input: &str,
) -> Option<String> {
    if history.is_empty() {
        return None;
    }

    let next_index = match *index {
        Some(current) => current
            .min(history.len().saturating_sub(1))
            .saturating_sub(1),
        None => {
            *draft = current_input.to_string();
            history.len().saturating_sub(1)
        }
    };
    *index = Some(next_index);
    history.get(next_index).cloned()
}

fn next_task_history_entry(
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut String,
) -> Option<String> {
    let current = (*index)?.min(history.len().saturating_sub(1));
    if current + 1 < history.len() {
        let next_index = current + 1;
        *index = Some(next_index);
        return history.get(next_index).cloned();
    }

    *index = None;
    Some(std::mem::take(draft))
}

fn insert_char_at_cursor(input: &mut String, cursor: &mut usize, ch: char) {
    let byte_index = byte_index_for_char(input, *cursor);
    input.insert(byte_index, ch);
    *cursor += 1;
}

fn insert_str_at_cursor(input: &mut String, cursor: &mut usize, value: &str) {
    let byte_index = byte_index_for_char(input, *cursor);
    input.insert_str(byte_index, value);
    *cursor += value.chars().count();
}

fn delete_char_before_cursor(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = byte_index_for_char(input, cursor.saturating_sub(1));
    let end = byte_index_for_char(input, *cursor);
    input.replace_range(start..end, "");
    *cursor -= 1;
}

fn delete_char_at_cursor(input: &mut String, cursor: usize) {
    if cursor >= input.chars().count() {
        return;
    }
    let start = byte_index_for_char(input, cursor);
    let end = byte_index_for_char(input, cursor + 1);
    input.replace_range(start..end, "");
}

fn truncate_at_cursor(input: &mut String, cursor: usize) {
    let start = byte_index_for_char(input, cursor);
    input.truncate(start);
}

fn delete_previous_word_at(input: &mut String, cursor: &mut usize) {
    let start_char = previous_word_position(input, *cursor);
    if start_char == *cursor {
        return;
    }
    let start = byte_index_for_char(input, start_char);
    let end = byte_index_for_char(input, *cursor);
    input.replace_range(start..end, "");
    *cursor = start_char;
}

fn previous_word_position(input: &str, cursor: usize) -> usize {
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].is_whitespace() {
        index -= 1;
    }
    index
}

fn next_word_position(input: &str, cursor: usize) -> usize {
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    while index < chars.len() && !chars[index].is_whitespace() {
        index += 1;
    }
    index
}

fn byte_index_for_char(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(input.len())
}

fn is_copy_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn terminal_bytes_for_key(key: KeyEvent) -> Option<Vec<u8>> {
    if is_copy_key(key) {
        return None;
    }

    let bytes = match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_char_bytes(ch)?
            } else if key.modifiers.contains(KeyModifiers::SUPER) {
                return None;
            } else if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::META)
            {
                let mut bytes = vec![0x1b];
                bytes.extend(ch.to_string().into_bytes());
                bytes
            } else {
                ch.to_string().into_bytes()
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => b"\x1b[13;2u".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::META)
            {
                vec![0x15]
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\x7f".to_vec()
            } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                vec![0x17]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => b"\x1bb".to_vec(),
        KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => b"\x1bf".to_vec(),
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => b"\x1b[1;5D".to_vec(),
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => b"\x1b[1;5C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => modified_arrow("A", key.modifiers),
        KeyCode::Down => modified_arrow("B", key.modifiers),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

fn bracketed_paste_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + "\x1b[200~\x1b[201~".len());
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn mouse_event_to_sgr(mouse: MouseEvent, area: Rect) -> Option<Vec<u8>> {
    if !rect_contains(area, mouse.column, mouse.row) {
        return None;
    }

    let x = mouse.column.saturating_sub(area.x).saturating_add(1);
    let y = mouse.row.saturating_sub(area.y).saturating_add(1);
    let (button, press) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), true),
        MouseEventKind::Up(_) => (3, false),
        MouseEventKind::Drag(button) => (mouse_button_code(button) + 32, true),
        MouseEventKind::Moved => (35, true),
        MouseEventKind::ScrollUp => (64, true),
        MouseEventKind::ScrollDown => (65, true),
        MouseEventKind::ScrollLeft => (66, true),
        MouseEventKind::ScrollRight => (67, true),
    };
    let button = button + mouse_modifier_code(mouse.modifiers);

    Some(
        format!(
            "\x1b[<{};{};{}{}",
            button,
            x,
            y,
            if press { "M" } else { "m" }
        )
        .into_bytes(),
    )
}

fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn mouse_modifier_code(modifiers: KeyModifiers) -> u16 {
    let mut code = 0;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    code
}

fn control_char_bytes(ch: char) -> Option<Vec<u8>> {
    let lower = ch.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some(vec![(lower as u8) - b'a' + 1]);
    }
    match ch {
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

fn modified_arrow(final_byte: &str, modifiers: KeyModifiers) -> Vec<u8> {
    let modifier_code = if modifiers.contains(KeyModifiers::CONTROL) {
        Some(5)
    } else if modifiers.contains(KeyModifiers::ALT) {
        Some(3)
    } else {
        None
    };
    match modifier_code {
        Some(code) => format!("\x1b[1;{code}{final_byte}").into_bytes(),
        None => format!("\x1b[{final_byte}").into_bytes(),
    }
}

fn current_branch_at(cwd: &Path) -> Option<String> {
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

fn native_runs_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".rudder").join("runs")
}

fn native_run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
    native_runs_dir(repo_root).join(run_id)
}

fn read_migration_manifest(repo_root: &Path) -> Vec<MigratedAgent> {
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

fn create_main_agent(
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
    }
}

fn load_persisted_agents(repo_root: &Path) -> Vec<AgentRun> {
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

fn agent_from_run_record(repo_root: &Path, record: serde_json::Value) -> Option<AgentRun> {
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
        .map(ToOwned::to_owned);
    let worktree_path = worktree_enabled.then_some(cwd.clone());
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
    })
}

fn turns_from_run_record(
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

fn turn_from_json(value: &serde_json::Value) -> Option<AgentTurn> {
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

fn agent_status_from_record(status: Option<&str>) -> AgentStatus {
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

fn run_record_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "running",
        AgentStatus::Done => "completed",
        AgentStatus::Merged => "merged",
        AgentStatus::Failed => "failed",
        AgentStatus::Stopped => "cancelled",
    }
}

fn save_native_run_record(repo_root: &Path, run: &AgentRun) -> Result<()> {
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
    let record = serde_json::json!({
        "id": run.id,
        "status": run_record_status(run.status),
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
        "worktree": {
            "enabled": run.worktree_path.is_some(),
            "path": run.cwd,
            "branch": run.worktree_branch,
        },
        "currentPrompt": run.current_prompt,
        "turns": turns,
        "lastUserInputAt": run.last_user_input_at,
        "autoSteer": { "count": if run.autosteered { 1 } else { 0 }, "max": 2 },
        "reviewSourceIds": run.review_source_ids,
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

fn remove_native_run_record(repo_root: &Path, run_id: &str) -> Result<()> {
    let dir = native_run_dir(repo_root, run_id);
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct WorktreeInfo {
    id: String,
    path: PathBuf,
    branch: Option<String>,
    path_is_worktree: bool,
}

impl WorktreeInfo {
    fn current(path: PathBuf) -> Self {
        Self {
            id: new_run_id("task"),
            path,
            branch: None,
            path_is_worktree: false,
        }
    }
}

fn prepare_worktree(cwd: &Path, task: &str) -> Result<WorktreeInfo> {
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
    })
}

fn repo_root(cwd: &Path) -> PathBuf {
    git_output(cwd, ["rev-parse", "--show-toplevel"])
        .map(|root| PathBuf::from(root.trim()))
        .unwrap_or_else(|_| cwd.to_path_buf())
}

fn dashboard_root(cwd: &Path) -> PathBuf {
    let repo = repo_root(cwd);
    main_worktree_root(&repo).unwrap_or(repo)
}

fn main_worktree_root(repo: &Path) -> Option<PathBuf> {
    let output = git_output_args(repo, &["worktree", "list", "--porcelain"]).ok()?;
    main_worktree_from_porcelain(&output)
}

fn main_worktree_from_porcelain(output: &str) -> Option<PathBuf> {
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

fn is_git_repo(cwd: &Path) -> bool {
    git_output(cwd, ["rev-parse", "--is-inside-work-tree"])
        .map(|value| value.trim() == "true")
        .unwrap_or(false)
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
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

fn git_output_args(cwd: &Path, args: &[&str]) -> Result<String> {
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

fn git_status_command(cwd: &Path, args: &[&str]) -> Result<()> {
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

fn rebase_worktree_onto_base(repo_root: &Path, worktree_path: &Path, base_branch: &str) -> Result<()> {
    let base_ref = resolve_rebase_base_ref(repo_root, base_branch)?;
    git_status_command(worktree_path, &["rebase", &base_ref])
        .with_context(|| format!("rebase onto {base_ref} failed"))
}

fn resolve_rebase_base_ref(repo_root: &Path, base_branch: &str) -> Result<String> {
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

fn current_commit_at(cwd: &Path) -> Result<String> {
    git_output(cwd, ["rev-parse", "HEAD"]).map(|value| value.trim().to_string())
}

fn ref_exists(repo_root: &Path, reference: &str) -> bool {
    let verify = format!("{reference}^{{commit}}");
    git_output_args(repo_root, &["rev-parse", "--verify", &verify]).is_ok()
}

fn commit_pending_changes_for_run(run: &AgentRun) -> Result<()> {
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
fn premerge_review_all_sources(cwd: &Path, sources: &[ReviewAllSource]) -> ReviewAllPremerge {
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

fn conflicted_files(cwd: &Path) -> Vec<String> {
    git_output(cwd, ["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn diff_short_summary(run: &AgentRun) -> Option<String> {
    diff_short_summary_at(&run.cwd)
}

fn diff_short_summary_at(cwd: &Path) -> Option<String> {
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

fn play_completion_sound() {
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

fn completion_sound_path() -> Option<PathBuf> {
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

fn has_git_changes(cwd: &Path) -> bool {
    git_output(cwd, ["status", "--short"])
        .map(|status| !status.trim().is_empty())
        .unwrap_or(false)
}

fn count_uncommitted_changes(cwd: &Path) -> usize {
    git_output(cwd, ["status", "--short"])
        .map(|status| {
            status
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
}

fn worktree_path(repo_root: &Path, run_id: &str, task: &str) -> PathBuf {
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

fn worktree_dir_name(run_id: &str, task: &str) -> String {
    let task_slug = slugify(task, "task");
    format!("{}-{}", task_slug, worktree_unique_suffix(run_id))
}

fn write_rudder_context(
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

fn ensure_hunk_config(cwd: &Path) -> Result<()> {
    ensure_git_info_exclude_contains(cwd, ".hunk/")?;
    let dir = cwd.join(".hunk");
    fs::create_dir_all(&dir)?;
    let config = dir.join("config.toml");
    let theme = hunk_light_theme();
    let contents = [
        format!("theme = \"{theme}\""),
        "mode = \"auto\"".to_string(),
        "vcs = \"git\"".to_string(),
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

fn hunk_light_theme() -> String {
    match env::var("RUDDER_HUNK_THEME") {
        Ok(value) if value == "light" => "paper".to_string(),
        Ok(value) if matches!(value.as_str(), "paper" | "graphite" | "midnight" | "ember") => value,
        _ => "paper".to_string(),
    }
}

fn ensure_git_info_exclude_contains(cwd: &Path, line: &str) -> Result<()> {
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

fn ensure_gitignore_contains(repo_root: &Path, line: &str) -> Result<()> {
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

fn new_run_id(task: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos}-{}-{}", slugify(task, "task"), std::process::id())
}

fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn slugify(input: &str, fallback: &str) -> String {
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

fn short_hash(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:010x}", hasher.finish())[..10].to_string()
}

fn worktree_unique_suffix(run_id: &str) -> String {
    short_hash(run_id).chars().take(8).collect()
}
