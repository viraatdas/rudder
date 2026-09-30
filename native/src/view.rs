//! Second dashboard views: `rudder` in another tab while a dashboard is
//! already running for the checkout.
//!
//! There is still exactly one `App` and one writer (AGENTS.md §7): the agents,
//! plans and PTYs live here, and the other tab is a thin client (attach.rs).
//! What a view owns is only what a person looking at the dashboard owns: the
//! selection, the focused pane, the task-bar draft, open popups and the
//! hit-test maps from its last frame. `ViewState` holds those fields for each
//! remote view, and `App::with_view` swaps them INTO `App` around handling that
//! view's input or rendering its frame, then swaps them back. Everything else,
//! every key handler and the whole of render.rs, is the dashboard's own code
//! run unchanged, so a second view can do anything the first can: start
//! agents, `/plan`, merge, type into a worker.
//!
//! Rendering goes through a ratatui `Terminal` whose backend writes into a
//! buffer instead of stdout, so a view gets ratatui's own cell diff and only
//! changed cells cross the socket.
//!
//! A pane has one size. The view that last took input owns it (tmux's
//! `window-size latest`); while another view owns it, this one leaves the
//! owner's selected pane at the owner's size (`App::pane_size_hold`).

use super::*;
use ratatui::{TerminalOptions, Viewport};
use std::sync::{Arc, Mutex};

/// Swaps the listed `App` fields with a `ViewState`'s. Listing a field here is
/// the whole of making it per-view.
macro_rules! view_state {
    ($($field:ident : $ty:ty = $init:expr),* $(,)?) => {
        pub(crate) struct ViewState {
            $($field: $ty,)*
        }

        impl ViewState {
            fn fresh() -> Self {
                Self { $($field: $init,)* }
            }

            fn swap_with(&mut self, app: &mut App) {
                $(std::mem::swap(&mut self.$field, &mut app.$field);)*
            }
        }
    };
}

view_state! {
    focus: FocusPane = FocusPane::Task,
    nav_mode: bool = false,
    leader_pending: bool = false,
    worker_view: WorkerView = WorkerView::Terminal,
    gam_transcript_visible: bool = true,
    nest_view: bool = false,
    task_input: String = String::new(),
    task_cursor: usize = 0,
    pasted_chunks: Vec<PastedChunk> = Vec::new(),
    task_history_index: Option<usize> = None,
    task_history_draft: String = String::new(),
    selected_agent: usize = 0,
    agents_scroll: usize = 0,
    notice: Option<String> = None,
    cloud_prompt: Option<CloudLaunchPrompt> = None,
    delete_pending: Option<String> = None,
    delete_armed_frame: u64 = 0,
    merge_confirm: Option<MergeConfirmation> = None,
    conflict_prompt: Option<MergeConflictPrompt> = None,
    picker_index: usize = 0,
    picker_dismissed_input: Option<String> = None,
    worker_selection: Option<WorkerSelection> = None,
    task_selection: Option<WorkerSelection> = None,
    orch_selection: Option<WorkerSelection> = None,
    orch_visible_rows: Vec<String> = Vec::new(),
    agent_row_map: Vec<Option<usize>> = Vec::new(),
    drawer_row_map: Vec<Option<Bucket>> = Vec::new(),
    drawer_cursor: Option<Bucket> = None,
    drawer: Option<DrawerState> = None,
    orch_dag_scroll: usize = 0,
    orch_dag_max_scroll: usize = 0,
    orch_follow_bottom: bool = true,
    agents_area: Option<Rect> = None,
    worker_area: Option<Rect> = None,
    diff_area: Option<Rect> = None,
    diff_panel: DiffPanel = DiffPanel::new(),
    solo_pane: bool = false,
    solo_restore_focus: Option<FocusPane> = None,
    task_area: Option<Rect> = None,
    rename_input: Option<String> = None,
    rename_cursor: usize = 0,
    rename_prefilled: bool = false,
    quit_confirm_pending: bool = false,
}

/// A `Write` the view's ratatui backend draws into; the bytes are taken after
/// each frame and sent to the client.
#[derive(Clone, Default)]
pub(crate) struct ViewOutput(Arc<Mutex<Vec<u8>>>);

impl ViewOutput {
    fn take(&self) -> Vec<u8> {
        self.0
            .lock()
            .map(|mut buf| std::mem::take(&mut *buf))
            .unwrap_or_default()
    }
}

impl Write for ViewOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Ok(mut buf) = self.0.lock() {
            buf.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type ViewTerminal = Terminal<CrosstermBackend<ViewOutput>>;

pub(crate) struct DashboardView {
    state: ViewState,
    terminal: ViewTerminal,
    output: ViewOutput,
    /// This view's selection by run id: `selected_agent` is an index, and
    /// rows come and go between this view's turns.
    selected_run: Option<String>,
    /// `agents.len()` when this view last ran, so its click maps are dropped
    /// once rows were inserted or removed (the dashboard clears its own on
    /// every insert/remove; a stashed view never saw those).
    agents_len: usize,
    /// Something this view did needs a new frame.
    dirty: bool,
    rows: u16,
    cols: u16,
}

impl DashboardView {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let output = ViewOutput::default();
        Self {
            state: ViewState::fresh(),
            terminal: view_terminal(&output, rows, cols),
            output,
            selected_run: None,
            agents_len: 0,
            dirty: true,
            rows,
            cols,
        }
    }

    /// New size, or the client fell behind: start over from a blank screen.
    fn reset(&mut self, rows: u16, cols: u16) {
        self.output.take();
        self.terminal = view_terminal(&self.output, rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.dirty = true;
    }
}

fn view_terminal(output: &ViewOutput, rows: u16, cols: u16) -> ViewTerminal {
    let mut output_writer = output.clone();
    // A fresh ratatui terminal diffs against a blank buffer, so the client's
    // screen has to be blank too.
    let _ = output_writer.write_all(b"\x1b[0m\x1b[2J");
    Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, cols.max(1), rows.max(1))),
        },
    )
    .expect("a fixed viewport never queries the terminal")
}

/// Which view is acting: the dashboard's own terminal, or an attach client.
pub(crate) const LOCAL_VIEW: u64 = 0;

impl App {
    /// Run `f` with the remote view `client_id`'s state swapped into `self`.
    /// `None` when that client is gone or is not a dashboard view.
    pub(crate) fn with_view<R>(
        &mut self,
        client_id: u64,
        f: impl FnOnce(&mut App, &mut ViewTerminal) -> R,
    ) -> Option<R> {
        let pos = self
            .attach_clients
            .iter()
            .position(|client| client.id == client_id)?;
        let mut view = self.attach_clients[pos].dashboard.take()?;
        let local_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
        let local_len = self.agents.len();

        view.state.swap_with(self);
        self.selected_agent = self.index_for_view_selection(view.selected_run.as_deref());
        if view.agents_len != self.agents.len() {
            self.agent_row_map.clear();
            self.drawer_row_map.clear();
        }
        self.acting_view = client_id;
        let result = f(self, &mut view.terminal);
        self.acting_view = LOCAL_VIEW;
        view.selected_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
        view.agents_len = self.agents.len();
        view.state.swap_with(self);

        self.selected_agent = self.index_for_view_selection(local_run.as_deref());
        if local_len != self.agents.len() {
            self.agent_row_map.clear();
            self.drawer_row_map.clear();
        }
        if let Some(client) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
        {
            client.dashboard = Some(view);
        }
        Some(result)
    }

    /// The index a view's remembered run is at now; the same index clamped
    /// when that run is gone.
    fn index_for_view_selection(&self, run_id: Option<&str>) -> usize {
        run_id
            .and_then(|id| self.agents.iter().position(|run| run.id == id))
            .unwrap_or_else(|| self.selected_agent.min(self.agents.len().saturating_sub(1)))
    }

    /// Is this a second view acting right now (its keys, its frame)?
    pub(crate) fn acting_in_remote_view(&self) -> bool {
        self.acting_view != LOCAL_VIEW
    }

    /// Render code asks before resizing a pane: an attached single-pane
    /// terminal owns its pane, and another view owns the pane it has selected.
    pub(crate) fn pane_size_held(&self, run_id: &str) -> bool {
        self.run_is_attached(run_id) || self.pane_size_hold.iter().any(|id| id == run_id)
    }

    /// Before a view renders: hold the size owner's pane unless this view is
    /// the owner.
    pub(crate) fn prepare_pane_sizes_for(&mut self, view: u64, local_selected: Option<String>) {
        self.pane_size_hold.clear();
        if view == self.pane_size_owner {
            return;
        }
        let owner_run = if self.pane_size_owner == LOCAL_VIEW {
            local_selected
        } else {
            self.attach_clients
                .iter()
                .find(|client| client.id == self.pane_size_owner)
                .and_then(|client| client.dashboard.as_ref())
                .and_then(|view| view.selected_run.clone())
        };
        self.pane_size_hold.extend(owner_run);
    }

    /// Input arrived from `view`: it now owns the pane sizes. The owner
    /// changing means the panes resize on the next frames.
    pub(crate) fn note_view_input(&mut self, view: u64) {
        if self.pane_size_owner != view {
            self.pane_size_owner = view;
            self.dirty = true;
        }
    }

    pub(crate) fn open_dashboard_view(&mut self, client_id: u64, rows: u16, cols: u16) {
        if let Some(client) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
        {
            let mut view = DashboardView::new(rows, cols);
            // Start on the same row the dashboard is looking at.
            view.selected_run = self.agents.get(self.selected_agent).map(|run| run.id.clone());
            view.state.notice = Some(
                "second view of this dashboard · same agents · Ctrl+C closes this tab only".to_string(),
            );
            client.dashboard = Some(Box::new(view));
        }
    }

    pub(crate) fn resize_dashboard_view(&mut self, client_id: u64, rows: u16, cols: u16) {
        if let Some(view) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
            .and_then(|client| client.dashboard.as_mut())
        {
            view.reset(rows, cols);
        }
    }

    /// Apply one terminal event from a remote view. Returns true when that
    /// view asked to close (its quit keys close the tab, never the dashboard).
    pub(crate) fn handle_view_event(&mut self, client_id: u64, event: Event) -> bool {
        if matches!(event, Event::Key(_) | Event::Paste(_))
            || matches!(&event, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)))
        {
            self.note_view_input(client_id);
        }
        let closed = self
            .with_view(client_id, |app, _| match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Paste(text) => {
                    app.handle_paste(text);
                    false
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                    false
                }
                _ => false,
            })
            .unwrap_or(true);
        if let Some(view) = self
            .attach_clients
            .iter_mut()
            .find(|client| client.id == client_id)
            .and_then(|client| client.dashboard.as_mut())
        {
            view.dirty = true;
        }
        // Shared state may have moved (a new agent, a merge): the dashboard's
        // own terminal redraws too.
        self.dirty = true;
        closed
    }

    /// Draw every remote view that needs it and send the bytes. `changed`:
    /// shared state changed this tick, so every view redraws.
    pub(crate) fn render_dashboard_views(&mut self, changed: bool) {
        let ids: Vec<u64> = self
            .attach_clients
            .iter()
            .filter(|client| {
                client
                    .dashboard
                    .as_ref()
                    .is_some_and(|view| changed || view.dirty)
            })
            .map(|client| client.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let dirty_before = self.dirty;
        let local_selected = self.agents.get(self.selected_agent).map(|run| run.id.clone());
        for id in ids {
            self.prepare_pane_sizes_for(id, local_selected.clone());
            let drew = self
                .with_view(id, |app, terminal| {
                    terminal.draw(|frame| render(frame, app)).is_ok()
                })
                .unwrap_or(false);
            let Some(client) = self.attach_clients.iter_mut().find(|client| client.id == id) else {
                continue;
            };
            let Some(view) = client.dashboard.as_mut() else {
                continue;
            };
            view.dirty = false;
            let bytes = view.output.take();
            if !drew || bytes.is_empty() {
                continue;
            }
            client.send(&ServerMsg::Screen {
                data: String::from_utf8_lossy(&bytes).into_owned(),
            });
            if client.take_needs_full() {
                // Dropped: repaint the whole screen on the next pass.
                if let Some(view) = client.dashboard.as_mut() {
                    let (rows, cols) = (view.rows, view.cols);
                    view.reset(rows, cols);
                }
            }
        }
        self.pane_size_hold.clear();
        // Rendering a view is not a change the dashboard's own terminal has
        // to show.
        self.dirty = dirty_before;
    }
}
