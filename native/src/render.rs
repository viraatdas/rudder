#![allow(unused_imports)]
//! ratatui rendering: panes, prompts, layout, styles, and scroll math.
use super::*;
use crate::plan_stream::{PlanEntry, PlanEntryKind};

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    // In terminal-background mode this leaves the terminal foreground/background
    // untouched; in paper mode it paints the previous white canvas.
    frame.render_widget(Block::default().style(app_style()), area);

    let task_height = task_pane_height(app, area.width);

    // SOLO: the focused pane takes the whole screen and the others are not drawn.
    // The task line stays regardless — it is where you type, and a mode that hid it
    // would strand you in a view you cannot act from.
    if app.solo_pane {
        // FULL SCREEN: the focused pane and nothing else -- no sidebar, no task
        // line, no gutters. Soloing from the task pane moves focus to the worker
        // (toggle_solo_pane does it) so keystrokes still land somewhere you can see;
        // ⌥h restores both the split and the focus you had.
        let solo = match app.focus {
            FocusPane::Agents => FocusPane::Agents,
            _ => FocusPane::Worker,
        };
        // Areas the hit-testing and scroll code read: a hidden pane gets NONE, so a
        // stale rect cannot take a click meant for the pane that is actually drawn.
        app.agents_area = (solo == FocusPane::Agents).then_some(area);
        app.worker_area = (solo == FocusPane::Worker).then_some(area);
        app.task_area = None;
        if solo == FocusPane::Agents {
            render_agents(frame, area, app);
        } else {
            render_worker(frame, area, app);
        }
        render_cloud_prompt(frame, area, app);
        render_merge_prompt(frame, area, app);
        render_mouse_debug(frame, area, app);
        return;
    }

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
    render_perf_hud(frame, area, app);
}

#[derive(Clone, Copy)]
pub(crate) enum Gutter {
    Horizontal,
    Vertical,
}

pub(crate) fn render_gutter(frame: &mut Frame<'_>, area: Rect, gutter: Gutter) {
    let style = muted_style(false);
    let line = match gutter {
        Gutter::Horizontal => " ".repeat(area.width as usize),
        Gutter::Vertical => " ".to_string(),
    };

    let lines = vec![Line::from(Span::styled(line, style)); area.height as usize];
    frame.render_widget(Paragraph::new(lines).style(app_style()), area);
}

pub(crate) fn render_mouse_debug(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
            Style::default().fg(ACCENT_2),
        )))
        .style(app_style())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("mouse debug")
                .style(app_style()),
        )
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
}

pub(crate) fn render_perf_hud(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let agent_count = app.agents.len();
    let Some(text) = app.perf_stats.hud_line(agent_count) else {
        return;
    };
    let width = area.width.saturating_sub(4).min(120).max(20);
    let height = 3_u16.min(area.height);
    let x = area.right().saturating_sub(width + 2);
    let y = 1_u16.min(area.bottom().saturating_sub(height));
    let rect = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(ACCENT_2),
        )))
        .style(app_style())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("perf")
                .style(app_style()),
        )
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
}

// Row spans recorded while the agents pane renders: (first_row, end_row, agent index).
// The TUI renders on one thread, so a thread-local recorder lets the deeply nested
// row-push walks tag their rows without threading a collector through every helper.
// render_agents clears it, the push helpers append, and render_agents harvests it
// into `app.agent_row_map`, which is what mouse clicks resolve against. This replaced
// a hardcoded "agents start at row 12" offset that silently broke whenever the pane
// header grew and never accounted for section headers or blank separator rows.
thread_local! {
    static AGENT_ROW_SPANS: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_agent_rows(start: usize, end: usize, agent_index: usize) {
    AGENT_ROW_SPANS.with(|spans| spans.borrow_mut().push((start, end, agent_index)));
}

// Same idea for the collapsed drawer headers: they are sidebar rows that resolve to a
// bucket rather than to an agent, so a click on "done 27" opens that drawer.
thread_local! {
    static DRAWER_ROW_SPANS: std::cell::RefCell<Vec<(usize, Bucket)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_drawer_row(row: usize, bucket: Bucket) {
    DRAWER_ROW_SPANS.with(|spans| spans.borrow_mut().push((row, bucket)));
}

/// Render one agent into the list: a task-label line, a status badge line, and an
/// optional diff line. `prefix` is prepended to the task-label line and
/// `cont_prefix` to the status/diff continuation lines. Both are empty in flat
/// mode (byte-for-byte unchanged output); nest mode passes jj-log connector glyphs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_agent_row<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    diff: Option<String>,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    cont_prefix: &[Span<'a>],
) {
    push_agent_row_with_trailing(
        lines,
        app,
        index,
        agent,
        diff,
        focused,
        task_width,
        prefix,
        cont_prefix,
        &[],
    );
}

/// Like `push_agent_row` but appends `trailing` spans to the task-label line (used
/// for the faint "^dep" cross-section dependency hint).
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_agent_row_with_trailing<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    diff: Option<String>,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    cont_prefix: &[Span<'a>],
    trailing: &[Span<'a>],
) {
    let row_start = lines.len();
    let selected = index == app.selected_agent;
    // The selected row must read clearly EVEN WHEN THE PANE IS UNFOCUSED (you navigate the
    // tree from either pane), so its marker + title use the accent color + BOLD
    // unconditionally instead of fading to FAINT the way accent_style(false) did — that
    // fade is why the arrow was sometimes invisible. A filled glyph reads better than ">".
    // A selected row must pop unmistakably from EITHER pane (you navigate the tree
    // from both). The marker is a big, bold, slow-blinking arrow in the accent color,
    // and the title gets a solid accent "pill" (white ink on teal) so the highlighted
    // agent reads at a glance instead of a faint one-shade shift.
    let selected_marker_style = Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK);
    let selected_label_style = Style::default()
        .fg(PAPER)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let marker = if selected { "▶ " } else { "  " };
    let marker_style = if selected {
        selected_marker_style
    } else {
        accent_style(focused)
    };
    let task_style = if selected {
        selected_label_style
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

    let mut first = prefix.to_vec();
    first.extend([
        Span::styled(marker, marker_style),
        if is_cloud_agent(agent) {
            Span::styled("☁ ", cloud_style(true, focused))
        } else {
            Span::raw("")
        },
        Span::styled(truncate_chars(&task_label, task_width), task_style),
    ]);
    first.extend(trailing.iter().cloned());
    lines.push(ListItem::new(Line::from(first)));

    let (status_label, status_style): (String, Style) =
        if agent.is_main() && agent.terminal.is_none() {
            ("idle".to_string(), muted_style(focused))
        } else {
            (agent_status_text(agent), agent_status_style(agent))
        };
    let badge_style = Style::default().fg(status_style.fg.unwrap_or(MUTED));

    let mode_span = if agent.is_main() {
        Span::styled("main", accent_style(focused))
    } else if is_cloud_agent(agent) {
        Span::styled("cloud", accent_style(focused))
    } else if agent.mode == AgentMode::RudderPlan {
        Span::styled("rudder-plan", accent_style(focused))
    } else if agent.mode == AgentMode::ReviewAll {
        Span::styled("review-all", accent_style(focused))
    } else if agent.mode == AgentMode::Plan {
        Span::styled("plan", accent_style(focused))
    } else if let Some(node_id) = agent.node_id.as_deref() {
        // Plan workers carry their DAG node id so the row is matchable against the
        // orchestrator pane's "n2 <title>" line at a glance.
        Span::styled(format!("run {node_id}"), muted_style(focused))
    } else {
        Span::styled("run", muted_style(focused))
    };

    // The status head (badge + status + mode) always fits; the model tail
    // (backend + model + effort) is what overflows the ~30-col pane and gets the
    // model name clipped mid-word ("opus[1m]" -> "opus[1"). Keep it inline when it
    // fits, otherwise wrap it onto its own indented line so the model shows in full.
    let mut status_head = cont_prefix.to_vec();
    status_head.extend([
        Span::raw("  "),
        Span::styled(BADGE, badge_style),
        Span::raw(" "),
        Span::styled(status_label, status_style),
        Span::raw("  "),
        mode_span,
    ]);
    let model_tail = vec![
        Span::styled(agent.backend.as_str().to_string(), muted_style(focused)),
        Span::raw("  "),
        Span::styled(agent.model.clone(), model_style(focused)),
        Span::styled(
            format!("({})", effort_label(agent.effort)),
            model_style(focused),
        ),
    ];
    let head_width: usize = status_head.iter().map(Span::width).sum();
    let tail_width: usize = model_tail.iter().map(Span::width).sum();
    if head_width + 2 + tail_width <= task_width {
        let mut status_line = status_head;
        status_line.push(Span::raw("  "));
        status_line.extend(model_tail);
        lines.push(ListItem::new(Line::from(status_line)));
    } else {
        lines.push(ListItem::new(Line::from(status_head)));
        let mut model_line = cont_prefix.to_vec();
        model_line.push(Span::raw("    "));
        model_line.extend(model_tail);
        lines.push(ListItem::new(Line::from(model_line)));
    }
    if let Some(detail) = integration_detail(agent) {
        let mut integration_line = cont_prefix.to_vec();
        integration_line.extend([Span::raw("  "), Span::styled(detail, muted_style(focused))]);
        lines.push(ListItem::new(Line::from(integration_line)));
    }
    if let Some(detail) = delivery_detail(agent) {
        let mut delivery_line = cont_prefix.to_vec();
        delivery_line.extend([Span::raw("  "), Span::styled(detail, muted_style(focused))]);
        lines.push(ListItem::new(Line::from(delivery_line)));
    }
    if let Some(summary) = diff {
        let mut diff_line = cont_prefix.to_vec();
        diff_line.extend([
            Span::raw("  "),
            Span::styled(compact_diff_stat(&summary), muted_style(focused)),
        ]);
        lines.push(ListItem::new(Line::from(diff_line)));
    }
    record_agent_rows(row_start, lines.len(), index);
}

/// Compress `git diff --shortstat` output ("34 files changed, 3162 insertions(+),
/// 120 deletions(-)") into a pane-friendly "34f +3162 -120" so every count stays
/// visible in the narrow agents pane instead of clipping mid-word. Falls back to
/// the original string if it does not parse.
fn compact_diff_stat(summary: &str) -> String {
    let (mut files, mut ins, mut del) = (None, None, None);
    for part in summary.split(',') {
        let part = part.trim();
        let Some(num) = part.split_whitespace().next() else {
            continue;
        };
        if part.contains("file") {
            files = Some(num);
        } else if part.contains("insertion") {
            ins = Some(num);
        } else if part.contains("deletion") {
            del = Some(num);
        }
    }
    let mut out = String::new();
    if let Some(files) = files {
        out.push_str(&format!("{files}f"));
    }
    if let Some(ins) = ins {
        out.push_str(&format!(" +{ins}"));
    }
    if let Some(del) = del {
        out.push_str(&format!(" -{del}"));
    }
    let out = out.trim();
    if out.is_empty() {
        summary.to_string()
    } else {
        out.to_string()
    }
}

/// Status section for the agents pane. Order here is the rendered/navigated order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Bucket {
    OneOff,
    Todo,
    InProgress,
    Review,
    Done,
    Closed,
}

impl Bucket {
    /// Sections in display order; main agents + the orchestrator render above all of these.
    /// One-off agents lead (they are quick, transient, and what you most recently asked for).
    const ORDER: [Bucket; 6] = [
        Bucket::OneOff,
        Bucket::Todo,
        Bucket::InProgress,
        Bucket::Review,
        Bucket::Done,
        Bucket::Closed,
    ];

    /// Short header label (the left pane is only 34 cols wide).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Bucket::OneOff => "one-off",
            Bucket::Todo => "todo",
            Bucket::InProgress => "in progress",
            Bucket::Review => "review",
            Bucket::Done => "done",
            Bucket::Closed => "closed",
        }
    }

    /// Finished work collapses to a single sidebar row instead of one row per run.
    /// A long session ends with dozens of merged and failed rows; listing them all
    /// buries the two or three that are still moving.
    ///
    /// `done` and `closed` always collapse. `review` is the awkward one: it is
    /// finished work that still needs a merge decision, so a handful of rows belongs
    /// inline where the user can act on it — but a session that ends with 25 of them
    /// (which is what a long fleet run actually produces) is not scannable at four
    /// lines a row, and the section you must act on is the one whose shape you cannot
    /// see. Past the threshold it collapses like the rest.
    pub(crate) fn always_drawer(self) -> bool {
        matches!(self, Bucket::Done | Bucket::Closed)
    }
}

/// Review stays inline up to this many rows, then collapses. Sized so a normal plan
/// (a few nodes awaiting merge) reads exactly as before.
pub(crate) const REVIEW_INLINE_LIMIT: usize = 5;

/// Is this bucket currently drawn as a collapsed drawer? Depends on the fleet, because
/// `review` collapses only once it is too long to read.
pub(crate) fn bucket_is_drawer(agents: &[AgentRun], bucket: Bucket) -> bool {
    if bucket.always_drawer() {
        return true;
    }
    bucket == Bucket::Review && bucket_members(agents, bucket).len() > REVIEW_INLINE_LIMIT
}

/// The buckets currently collapsed, in sidebar (Bucket::ORDER) order, so navigation
/// and rendering agree on which header rows exist and where.
pub(crate) fn drawer_buckets(agents: &[AgentRun]) -> Vec<Bucket> {
    Bucket::ORDER
        .into_iter()
        .filter(|&bucket| bucket_is_drawer(agents, bucket))
        .collect()
}

/// Indices of the runs a drawer holds, in sidebar order.
pub(crate) fn drawer_members(agents: &[AgentRun], bucket: Bucket) -> Vec<usize> {
    bucket_members(agents, bucket)
}

/// Is this run tucked inside a collapsed drawer (and so not its own sidebar row)?
pub(crate) fn in_drawer(agents: &[AgentRun], agent: &AgentRun) -> bool {
    !agent.is_main() && !agent.is_pinned_planner() && bucket_is_drawer(agents, status_bucket(agent))
}

/// Map an agent to its status section.
///
/// - todo:        planned (not-yet-launched) DAG nodes (rendered from the
///                `planned_nodes` queue, not from agents).
/// - in progress: Running (incl. steering/verifying via AgentStatus::Running).
/// - review:      completed-but-not-merged (Done), plus needs-permission/-input.
/// - done:        Merged.
/// - closed:      Failed / Stopped (cancelled).
///
/// Main agents are rendered in their own leading section and are not bucketed.
/// Agents never land in Todo: that section now holds planned nodes only.
pub(crate) fn status_bucket(agent: &AgentRun) -> Bucket {
    // Bucket strictly by run status. A RUNNING agent stays "in progress" even when it is
    // waiting on a permission prompt or a question: it is still an active session, not
    // finished work. The "needs permission"/"needs input" state is surfaced on the row
    // itself (agent_status_label, in amber) and via a notification, so it is discoverable
    // without being mislabeled "review". "review" means DONE: the agent finished a turn
    // and is awaiting your review/merge. (Previously needs_permission/needs_user_input
    // forced a running agent into Review, which read as "finished" and was confusing.)
    // One-off conversational agents get their own leading section regardless of run status.
    if agent.is_oneoff() {
        return Bucket::OneOff;
    }
    match agent.status {
        AgentStatus::Running => Bucket::InProgress,
        AgentStatus::Done => Bucket::Review,
        AgentStatus::Merged => Bucket::Done,
        AgentStatus::Failed
        | AgentStatus::Stopped
        | AgentStatus::Paused
        | AgentStatus::Orphaned
        | AgentStatus::Migrated => Bucket::Closed,
    }
}

/// Build the per-section nest: parent->children adjacency restricted to edges where
/// BOTH endpoints fall in `members` (the indices in one section). Also returns,
/// for each member, whether it has a parent that lives OUTSIDE this section (so we
/// render it as a section root with a dep hint rather than a connector).
/// Map every agent's identifiers to its index, for resolving dependency edges in the
/// agents-pane nest. Both the run id AND the plan node id are inserted, because a
/// launched plan worker records its deps as plan NODE ids (`node.deps`) while
/// manual/test runs reference the run id. `node_id` is inserted after the run id so it
/// wins on the (rare) collision; deps are node ids in production.
fn agent_id_index(agents: &[AgentRun]) -> std::collections::HashMap<&str, usize> {
    let mut map: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(agents.len() * 2);
    for (index, agent) in agents.iter().enumerate() {
        map.insert(agent.id.as_str(), index);
        if let Some(node_id) = agent.node_id.as_deref() {
            map.insert(node_id, index);
        }
    }
    map
}

fn section_nest(
    agents: &[AgentRun],
    members: &[usize],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
) {
    use std::collections::{HashMap, HashSet};
    let member_set: HashSet<usize> = members.iter().copied().collect();
    // Index by BOTH the run id and the plan node id. A launched plan worker carries its
    // deps as plan NODE ids (`node.deps`), while manual/test setups reference the run id;
    // mapping both lets a child resolve its parent either way. `node_id` is inserted last
    // so it wins if it ever collides with a different run's id (deps are node ids in
    // production). Without the node_id key, launched workers never nest (run id != node id).
    let id_to_index: HashMap<&str, usize> = agent_id_index(agents);

    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_in_section_parent: HashSet<usize> = HashSet::new();
    let mut has_out_of_section_parent: HashSet<usize> = HashSet::new();

    for &child_index in members {
        let agent = &agents[child_index];
        // Hard parents bind before soft so the most-binding edge wins.
        for parent_id in &agent.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                if member_set.contains(&parent_index) {
                    children
                        .entry(parent_index)
                        .or_default()
                        .push((child_index, EdgeType::Hard));
                    has_in_section_parent.insert(child_index);
                } else {
                    has_out_of_section_parent.insert(child_index);
                }
            }
        }
        for parent_id in &agent.soft_deps {
            if agent.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                if member_set.contains(&parent_index) {
                    children
                        .entry(parent_index)
                        .or_default()
                        .push((child_index, EdgeType::Soft));
                    has_in_section_parent.insert(child_index);
                } else {
                    has_out_of_section_parent.insert(child_index);
                }
            }
        }
    }

    (children, has_in_section_parent, has_out_of_section_parent)
}

/// Depth-first walk of one section's nest, appending visited indices in render
/// order (parents before children). Cycle-guarded by `visited`.
#[allow(clippy::too_many_arguments)]
fn section_walk(
    out: &mut Vec<usize>,
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    visited: &mut std::collections::HashSet<usize>,
) {
    if !visited.insert(index) {
        return;
    }
    out.push(index);
    if let Some(kids) = children.get(&index) {
        for (child_index, _) in kids {
            section_walk(out, children, *child_index, visited);
        }
    }
}

/// The members of one section in nested render order: section roots (no in-section
/// parent) in display order, each followed by its in-section subtree.
fn section_order(agents: &[AgentRun], members: &[usize]) -> Vec<usize> {
    use std::collections::HashSet;
    let (children, has_in_section_parent, _out) = section_nest(agents, members);
    let mut visited: HashSet<usize> = HashSet::new();
    let mut out: Vec<usize> = Vec::new();
    for &index in members {
        if !has_in_section_parent.contains(&index) {
            section_walk(&mut out, &children, index, &mut visited);
        }
    }
    // Any member left unvisited (e.g. inside an in-section cycle) renders at root.
    for &index in members {
        section_walk(&mut out, &children, index, &mut visited);
    }
    out
}

/// Indices that belong to `bucket`, in display (insertion) order, excluding main
/// and pinned orchestrators (which render above all sections).
fn bucket_members(agents: &[AgentRun], bucket: Bucket) -> Vec<usize> {
    agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| {
            !agent.is_main() && !agent.is_pinned_planner() && status_bucket(agent) == bucket
        })
        .map(|(index, _)| index)
        .collect()
}

/// Canonical agents-pane order used by BOTH the renderer and navigation so the
/// selection marker always lands on the row j/k move to: main agents first, then
/// each status section in `Bucket::ORDER`, nested within the section.
pub(crate) fn sectioned_agent_order(agents: &[AgentRun]) -> Vec<usize> {
    // Pinned planners (the orchestrator and the plan-mode front-end) render and
    // navigate first, above main and every status section, so j/k land on them
    // where they are drawn.
    let mut order: Vec<usize> = agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| agent.is_pinned_planner())
        .map(|(index, _)| index)
        .collect();
    order.extend(
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.is_main() && !agent.is_pinned_planner())
            .map(|(index, _)| index),
    );
    for bucket in Bucket::ORDER {
        let members = bucket_members(agents, bucket);
        order.extend(section_order(agents, &members));
    }
    order
}

/// Faint "^dep" badge appended to a section root whose parent lives in another
/// section (so it has no connector to draw here). Short to fit the 34-col pane.
fn dep_hint_span(focused: bool) -> Span<'static> {
    Span::styled(
        "  ^dep",
        Style::default()
            .fg(if focused { FAINT } else { FAINT })
            .add_modifier(Modifier::DIM),
    )
}

/// Recursive section nest walk that pushes rendered rows (mirrors `nest_walk` but
/// scoped to one section's adjacency, and tags out-of-section roots with a dep hint).
#[allow(clippy::too_many_arguments)]
fn section_walk_rows<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    out_of_section: &std::collections::HashSet<usize>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut std::collections::HashSet<usize>,
) {
    if !visited.insert(index) {
        return;
    }
    let agent = &app.agents[index];
    let is_root = lanes.is_empty();
    let mut prefix = nest_prefix(lanes, is_last, edge);
    // A section root whose parent is in another section gets a faint dep hint after
    // its label rather than a connector glyph (the parent isn't drawn here).
    let trailing = if is_root && out_of_section.contains(&index) {
        vec![dep_hint_span(focused)]
    } else {
        Vec::new()
    };

    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children
        .iter()
        .any(|(child, _)| !visited.contains(child));
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    let summary = if agent.is_main() {
        None
    } else {
        diff_summaries.get(index).and_then(|opt| opt.clone())
    };
    push_agent_row_with_trailing(
        lines,
        app,
        index,
        agent,
        summary,
        focused,
        task_width,
        &prefix,
        &cont_prefix,
        &trailing,
    );
    prefix.clear();

    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited.contains(child))
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        lanes.push(!child_is_last);
        section_walk_rows(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            children,
            out_of_section,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Render one status section: a header line with a count, then its members nested
/// by dependency. Returns true if anything was emitted (the section was non-empty).
#[allow(clippy::too_many_arguments)]
fn render_status_section<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    bucket: Bucket,
    leading_blank: bool,
) -> bool {
    use std::collections::HashSet;
    let members = bucket_members(&app.agents, bucket);
    if members.is_empty() {
        return false;
    }
    if leading_blank {
        lines.push(ListItem::new(Line::default()));
    }
    if bucket_is_drawer(&app.agents, bucket) {
        // One row for the whole section. The chevron is the affordance: `›` closed,
        // `⌄` open, and the row carries the selection marker when the sidebar cursor
        // is on it, exactly like an agent row.
        let selected = app.drawer_cursor == Some(bucket);
        let open = app.drawer_open() == Some(bucket);
        let marker = if selected { "▶ " } else { "  " };
        let chevron = if open { "⌄" } else { "›" };
        record_drawer_row(lines.len(), bucket);
        lines.push(ListItem::new(Line::from(vec![
            Span::styled(
                marker,
                if selected {
                    accent_style(focused)
                } else {
                    muted_style(focused)
                },
            ),
            Span::styled(bucket.label(), header_style(focused)),
            Span::styled(format!(" {}", members.len()), muted_style(focused)),
            Span::styled(format!("  {chevron}"), muted_style(focused)),
        ])));
        return true;
    }
    lines.push(ListItem::new(Line::from(vec![
        Span::styled(bucket.label(), header_style(focused)),
        Span::styled(format!(" {}", members.len()), muted_style(focused)),
    ])));

    let (children, has_in_section_parent, out_of_section) = section_nest(&app.agents, &members);
    let mut visited: HashSet<usize> = HashSet::new();
    let mut lanes: Vec<bool> = Vec::new();
    // Roots: members with no in-section parent, in display order.
    for &index in &members {
        if !has_in_section_parent.contains(&index) {
            section_walk_rows(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                &out_of_section,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    // Any member trapped in an in-section cycle renders at root so nothing hides.
    for &index in &members {
        if !visited.contains(&index) {
            section_walk_rows(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                &out_of_section,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    true
}

/// Push one planned (not-yet-launched) node row into the Todo section: a single
/// line carrying the Todo-colored badge and the node title. The "todo"/"planned"
/// status text is omitted on purpose — the section is already titled "todo", so
/// repeating it per row is noise. `prefix` carries the nest glyphs (empty for a
/// root); `_cont_prefix` is unused now that there is no second line to bridge.
fn push_planned_row<'a>(
    lines: &mut Vec<ListItem<'a>>,
    node: &PlannedNode,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    _cont_prefix: &[Span<'a>],
    blocked_reason: Option<&str>,
) {
    let label = if node.title.trim().is_empty() {
        summarize_task(&node.prompt)
    } else {
        node.title.clone()
    };
    let mut line = prefix.to_vec();
    line.extend([
        Span::raw("  "),
        Span::styled(BADGE, Style::default().fg(ST_PLANNED)),
        Span::raw(" "),
        Span::styled(
            truncate_chars(&label, task_width.saturating_sub(2)),
            pane_text_style(focused),
        ),
    ]);
    if let Some(dep) = blocked_reason {
        line.push(Span::styled(
            format!("  blocked: {dep} failed"),
            Style::default().fg(ST_FAILED),
        ));
    }
    lines.push(ListItem::new(Line::from(line)));
}

/// Walk the planned-node forest depth-first, nesting a node under a parent that is
/// also still planned. Roots are nodes whose every dep id is not itself a pending
/// planned node (so its parent has already launched or never existed).
#[allow(clippy::too_many_arguments)]
fn planned_walk<'a>(
    lines: &mut Vec<ListItem<'a>>,
    nodes: &[&PlannedNode],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    focused: bool,
    task_width: usize,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
    blocked_reasons: &std::collections::HashMap<String, String>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;

    let is_root = lanes.is_empty();
    let prefix = nest_prefix(lanes, is_last, edge);
    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children.iter().any(|(child, _)| !visited[*child]);
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    push_planned_row(
        lines,
        nodes[index],
        focused,
        task_width,
        &prefix,
        &cont_prefix,
        blocked_reasons.get(&nodes[index].id).map(String::as_str),
    );

    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        lanes.push(!child_is_last);
        planned_walk(
            lines,
            nodes,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            focused,
            task_width,
            lanes,
            visited,
            blocked_reasons,
        );
        lanes.pop();
    }
}

/// Render the Todo section from the planned-node queue. A node nests under its
/// parent when that parent is also still a pending planned node; once the parent
/// launches it leaves the queue and the child becomes a section root. Returns
/// true when anything was emitted.
fn render_planned_section<'a>(
    lines: &mut Vec<ListItem<'a>>,
    nodes: &[&PlannedNode],
    focused: bool,
    task_width: usize,
    leading_blank: bool,
    awaiting_approval: bool,
    blocked_reasons: &std::collections::HashMap<String, String>,
) -> bool {
    use std::collections::HashMap;
    if nodes.is_empty() {
        return false;
    }
    if leading_blank {
        lines.push(ListItem::new(Line::default()));
    }
    lines.push(ListItem::new(Line::from(vec![
        Span::styled(Bucket::Todo.label(), header_style(focused)),
        Span::styled(format!(" {}", nodes.len()), muted_style(focused)),
    ])));
    // APPROVAL GATE hint: while the plan awaits approval nothing has launched yet;
    // the user refines in the task pane or approves with an empty Enter.
    if awaiting_approval {
        lines.push(ListItem::new(Line::from(Span::styled(
            "  type below to refine  ·  empty Enter approves",
            Style::default().fg(ACCENT),
        ))));
    }

    let id_to_index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();

    // Parent (still-planned node) -> children among the planned set. Hard edges
    // bind before soft. A dep whose parent already launched is not in the map, so
    // such a node renders at root.
    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_planned_parent = vec![false; nodes.len()];
    for (child_index, node) in nodes.iter().enumerate() {
        for parent_id in &node.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Hard));
                has_planned_parent[child_index] = true;
            }
        }
        for parent_id in &node.soft_deps {
            if node.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Soft));
                has_planned_parent[child_index] = true;
            }
        }
    }

    let mut visited = vec![false; nodes.len()];
    let mut lanes: Vec<bool> = Vec::new();
    for index in 0..nodes.len() {
        if !has_planned_parent[index] {
            planned_walk(
                lines,
                nodes,
                &children,
                index,
                None,
                true,
                focused,
                task_width,
                &mut lanes,
                &mut visited,
                blocked_reasons,
            );
        }
    }
    // Anything trapped in a planned-node cycle renders at root so nothing hides.
    for index in 0..nodes.len() {
        if !visited[index] {
            planned_walk(
                lines,
                nodes,
                &children,
                index,
                None,
                true,
                focused,
                task_width,
                &mut lanes,
                &mut visited,
                blocked_reasons,
            );
        }
    }
    true
}

// --- Orchestrator view -----------------------------------------------------
//
// A RudderPlan agent is the pinned orchestrator: it owns the active plan. While it
// is still decomposing the task (Running and no parseable RUDDER_PLAN_TASKS block
// yet) the orchestrator is in the PLANNING phase and shows an animated spinner.
// Once a plan block parses it flips to PLAN-READY and renders a DAG tree of the
// parsed tasks, each row badged with that task's LIVE status (todo / in-progress /
// done / failed) derived from the planned-node queue and launched agents.

/// Diamond marker used to identify the orchestrator row and header.
const ORCH_MARK: &str = "\u{25C6}"; // ◆

/// Live status of a single plan task, derived from the planned-node queue (todo)
/// and any agent launched from that node id (running/done/failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrchTaskStatus {
    Todo,
    Running,
    /// The worker finished (AgentStatus::Done) but has NOT merged yet: it is
    /// awaiting review, exactly like the agents-pane Review bucket. Reserve Done
    /// for Merged so the orchestrator DAG matches the agents-pane buckets.
    Review,
    Done,
    Failed,
}

impl OrchTaskStatus {
    fn label(self) -> &'static str {
        match self {
            OrchTaskStatus::Todo => "todo",
            OrchTaskStatus::Running => "in-progress",
            OrchTaskStatus::Review => "review",
            OrchTaskStatus::Done => "done",
            OrchTaskStatus::Failed => "failed",
        }
    }

    fn color(self) -> Color {
        match self {
            OrchTaskStatus::Todo => ST_PLANNED,
            OrchTaskStatus::Running => ST_RUNNING,
            OrchTaskStatus::Review => ST_REVIEW,
            OrchTaskStatus::Done => ST_MERGED,
            OrchTaskStatus::Failed => ST_FAILED,
        }
    }
}

/// Derive a plan task's live status: a launched agent tagged with this node id
/// wins (Merged -> done, Done-not-merged -> review, Failed/Stopped -> failed,
/// otherwise in-progress); else if it is still a pending planned node it is todo.
/// Mirrors the agents-pane status buckets: Review is reserved for finished-but-
/// unmerged, Done for Merged.
pub(crate) fn orchestrator_task_status(app: &App, node_id: &str) -> OrchTaskStatus {
    // A node id can map to more than one run (e.g. a failed launch followed by a relaunch).
    // Pick the MOST SIGNIFICANT rather than the first match, so a live/merged relaunch is
    // not masked by a stale Failed/Stopped run; ties resolve to the most recent (later)
    // run since agents are appended in order.
    let rank = |s: &OrchTaskStatus| match s {
        OrchTaskStatus::Done => 4, // merged
        OrchTaskStatus::Running => 3,
        OrchTaskStatus::Review => 2,
        OrchTaskStatus::Failed => 1,
        OrchTaskStatus::Todo => 0,
    };
    let mut best: Option<OrchTaskStatus> = None;
    for run in app
        .agents
        .iter()
        .filter(|run| run.node_id.as_deref() == Some(node_id))
    {
        let status = match run.status {
            AgentStatus::Merged => OrchTaskStatus::Done,
            AgentStatus::Done => OrchTaskStatus::Review,
            AgentStatus::Failed | AgentStatus::Stopped | AgentStatus::Orphaned => {
                OrchTaskStatus::Failed
            }
            _ => OrchTaskStatus::Running,
        };
        if best.as_ref().is_none_or(|b| rank(&status) >= rank(b)) {
            best = Some(status);
        }
    }
    best.unwrap_or(OrchTaskStatus::Todo)
}

/// The orchestrator's current phase. `Planning` shows the spinner; `PlanReady`
/// carries the parsed tasks for the DAG tree.
pub(crate) enum OrchestratorPhase {
    Planning,
    PlanReady(Vec<RudderPlanTask>),
}

/// Compute the orchestrator phase from the agent's live output. A parseable
/// RUDDER_PLAN_TASKS block means the plan is ready (even while the planner process
/// is still winding down); otherwise it is still decomposing the task.
pub(crate) fn orchestrator_phase(agent: &AgentRun) -> OrchestratorPhase {
    rudder_plan_tasks_for_run(agent)
        .map(OrchestratorPhase::PlanReady)
        .unwrap_or(OrchestratorPhase::Planning)
}

fn orchestrator_phase_for_app(app: &App, agent: &AgentRun) -> OrchestratorPhase {
    if agent.is_orchestrator() && app.plan().planner_paused_for_input {
        OrchestratorPhase::Planning
    } else {
        orchestrator_phase(agent)
    }
}

/// Short phase label for the pinned orchestrator row: "planning" while
/// decomposing, then "plan N · running X" once tasks are parsed and launching.
fn orchestrator_phase_label(app: &App, agent: &AgentRun) -> String {
    match orchestrator_phase_for_app(app, agent) {
        OrchestratorPhase::Planning => "planning".to_string(),
        OrchestratorPhase::PlanReady(tasks) => {
            let running = tasks
                .iter()
                .filter(|t| orchestrator_task_status(app, &t.id) == OrchTaskStatus::Running)
                .count();
            format!("plan {} · running {}", tasks.len(), running)
        }
    }
}

/// Push the pinned orchestrator row: a diamond + selection marker, the task label,
/// then a phase line (spinner + "planning" while decomposing, or the plan summary).
fn push_orchestrator_row<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    index: usize,
    agent: &AgentRun,
    focused: bool,
    task_width: usize,
) {
    let row_start = lines.len();
    let selected = index == app.selected_agent;
    let marker = if selected { "▶ " } else { "  " };
    let label = if selected && app.rename_input.is_some() {
        format!("✎ {}", app.rename_input.clone().unwrap_or_default())
    } else if agent.task_summary.trim().is_empty() {
        summarize_task(&agent.task)
    } else {
        agent.task_summary.clone()
    };
    // Match the agent rows: a slow-blinking accent arrow + a solid accent pill on the
    // selected title so the highlighted orchestrator reads at a glance.
    let marker_style = if selected {
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)
    } else {
        accent_style(focused)
    };
    let label_style = if selected {
        Style::default()
            .fg(PAPER)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    lines.push(ListItem::new(Line::from(vec![
        Span::styled(marker, marker_style),
        Span::styled(format!("{ORCH_MARK} "), label_style),
        Span::styled(
            truncate_chars(&label, task_width.saturating_sub(2)),
            label_style,
        ),
    ])));

    // While the planner is still researching/decomposing, present it as the model's
    // PLAN MODE (Claude Code / Codex) so it reads as genuine plan mode; once the DAG is
    // parsed it becomes the orchestrator with its live status. Spin during planning.
    let planning = matches!(
        orchestrator_phase_for_app(app, agent),
        OrchestratorPhase::Planning
    ) && agent.status == AgentStatus::Running;
    // The plan is decomposed and waiting for the user to approve it before any
    // node launches. Surface that on the left panel in the attention color so
    // "it's asking for approval" is obvious without opening the plan.
    let awaiting = app.plan().awaiting_approval && agent.is_orchestrator() && !planning;
    let badge = if planning { app.spinner_glyph() } else { BADGE };
    let (role, phase) = if planning {
        let backend = match agent.backend {
            Backend::Claude => "Claude Code",
            Backend::Codex => "Codex",
            Backend::Opencode => "opencode",
        };
        ("plan mode", format!("{backend} · researching the plan"))
    } else if awaiting {
        (
            "awaiting approval",
            "press Enter on this row to launch the plan".to_string(),
        )
    } else {
        ("orchestrator", orchestrator_phase_label(app, agent))
    };
    let row_accent = if awaiting { RUNNING_COLOR } else { ACCENT };
    let role_style = if awaiting {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    let phase_style = if awaiting {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
    } else {
        muted_style(focused)
    };
    lines.push(ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(badge, Style::default().fg(row_accent)),
        Span::raw(" "),
        Span::styled(role, role_style),
        Span::raw("  "),
        Span::styled(phase, phase_style),
    ])));
    record_agent_rows(row_start, lines.len(), index);
}

pub(crate) fn render_agents(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Agents;
    // Fresh row-span recording for this frame; harvested into app.agent_row_map below.
    AGENT_ROW_SPANS.with(|spans| spans.borrow_mut().clear());
    DRAWER_ROW_SPANS.with(|spans| spans.borrow_mut().clear());
    app.normalize_drawer_state();
    let diff_summaries: Vec<Option<String>> = {
        // Pinned planners (orchestrator + plan-mode front-end) run in the MAIN repo
        // and are rendered via push_orchestrator_row (no diff line), so a diff summary
        // there is both meaningless and never shown: skip it like main agents.
        // Only rows that are actually DRAWN. Each summary costs two git subprocesses
        // on a miss, and a row collapsed inside the done/closed/review drawer never
        // shows one — so asking for every agent made startup O(all rows ever) in
        // subprocess spawns. Measured at 200 seeded rows: ~2 git processes per row
        // before the first frame.
        let drawn: std::collections::HashSet<usize> =
            sidebar_agent_indices(&app.agents).into_iter().collect();
        let keys: Vec<(String, PathBuf, bool, bool)> = app
            .agents
            .iter()
            .enumerate()
            .map(|(index, a)| {
                (
                    a.id.clone(),
                    a.cwd.clone(),
                    a.is_main() || a.is_pinned_planner() || !drawn.contains(&index),
                    // Only a row whose workspace can still change needs re-asking.
                    matches!(a.status, AgentStatus::Running | AgentStatus::Paused),
                )
            })
            .collect();
        keys.iter()
            .map(|(id, cwd, skip, live)| {
                if *skip {
                    None
                } else {
                    app.cached_diff_summary(id, cwd, *live)
                }
            })
            .collect()
    };
    let run_count = app.agents.iter().filter(|a| !a.is_main()).count();
    let mut lines = vec![
        ListItem::new(Line::from(Span::styled("rudder", pane_text_style(focused)))),
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
            Span::styled("  ·  ", muted_style(focused)),
            Span::styled("plan integration automatic", accent_style(focused)),
        ])),
        ListItem::new(Line::default()),
    ];
    if let Some((current, latest)) = read_update_notice() {
        lines.insert(
            lines.len() - 1,
            ListItem::new(Line::from(vec![
                Span::styled("\u{2191} ", accent_style(focused)),
                Span::styled(
                    format!("update: {current} -> {latest}"),
                    pane_text_style(focused),
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
    // The board URL is intentionally NOT shown in the UI; the "o web ui" hint below
    // (and /web) opens this project's board in the browser. Keeping the URL off-screen
    // avoids clipping it in this narrow pane and keeps the dashboard uncluttered.
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

    // Mirrors `sectioned_agent_order`: a pinned planner draws in the orchestrator
    // block above, so it must not also count as (or render in) the main section.
    let main_indices: Vec<usize> = app
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_main() && !a.is_pinned_planner())
        .map(|(index, _)| index)
        .collect();
    let main_count = main_indices.len();
    let run_total = app.agents.iter().filter(|a| !a.is_main()).count();
    let orchestrator_count = app.agents.iter().filter(|a| a.is_pinned_planner()).count();

    // Pinned planners render at the very top of BOTH views: one distinct accent +
    // diamond row per orchestrator / plan-mode front-end showing its phase, above
    // main and every status section. There is normally one at a time.
    if orchestrator_count > 0 {
        lines.push(ListItem::new(Line::from(Span::styled(
            "orchestrator",
            header_style(focused),
        ))));
        for (index, agent) in app.agents.iter().enumerate() {
            if agent.is_pinned_planner() {
                push_orchestrator_row(&mut lines, app, index, agent, focused, task_width);
            }
        }
        lines.push(ListItem::new(Line::default()));
    }

    if app.nest_view {
        // Alternate full-DAG view (toggled with `g`): one topological tree across
        // every agent, ignoring status sections.
        render_agents_nest_into(&mut lines, app, focused, task_width, &diff_summaries);
        if main_count == 0 && run_total == 0 {
            lines.push(ListItem::new(Line::from(Span::styled(
                "no agents yet  ·  type a task or /main",
                muted_style(focused),
            ))));
        }
    } else {
        // Default view: main agents first, then status sections (todo / in progress
        // / review / done / closed), each header carrying a count and omitted when
        // empty. Dependents nest under their parents within a section.
        if main_count > 0 {
            // Running several `/main` agents at once is supported, but they all edit
            // the SAME working copy, so the header states it: with only the per-row
            // "main" tag the shared-checkout risk had to be inferred by counting rows.
            let noun = if main_count == 1 { "agent" } else { "agents" };
            lines.push(ListItem::new(Line::from(vec![
                Span::styled("main", header_style(focused)),
                Span::styled(
                    format!(" {main_count} {noun} · shared checkout"),
                    muted_style(focused),
                ),
            ])));
            for &index in &main_indices {
                push_agent_row(
                    &mut lines,
                    app,
                    index,
                    &app.agents[index],
                    None,
                    focused,
                    task_width,
                    &[],
                    &[],
                );
            }
        }

        let mut emitted_any_section = false;
        for bucket in Bucket::ORDER {
            // Leave a blank line before any non-first block (after main or a prior
            // section) for visual separation.
            let leading_blank = main_count > 0 || emitted_any_section;
            // The Todo section is the planned-node queue (no agent ever buckets
            // there); every other section renders agents by status.
            let emitted = if bucket == Bucket::Todo {
                // A queued node whose HARD dep FAILED can never launch until the
                // user retries/deletes the failed node; say so ON the row (the
                // one-shot notice was easy to miss and the node then sat in todo
                // with no explanation forever).
                let failed_ids: std::collections::HashSet<&str> = app
                    .agents
                    .iter()
                    .filter(|run| run.status == AgentStatus::Failed)
                    .filter_map(|run| run.node_id.as_deref())
                    .collect();
                let blocked_reasons: std::collections::HashMap<String, String> = app
                    .plan()
                    .planned_nodes
                    .iter()
                    .filter_map(|node| {
                        node.deps
                            .iter()
                            .find(|dep| failed_ids.contains(dep.as_str()))
                            .map(|dep| (node.id.clone(), dep.clone()))
                    })
                    .collect();
                // EVERY plan's queue, not just the selected one's: the agents pane is
                // the whole fleet, so work waiting in another plan must not vanish from
                // it just because a different orchestrator is focused. Node ids are kept
                // unique across plans, so the flat list stays unambiguous.
                let queued: Vec<&PlannedNode> = app
                    .plans
                    .iter()
                    .flat_map(|plan| plan.planned_nodes.iter())
                    .collect();
                let awaiting = app
                    .plans
                    .iter()
                    .any(|plan| plan.awaiting_approval && !plan.planned_nodes.is_empty());
                render_planned_section(
                    &mut lines,
                    &queued,
                    focused,
                    task_width,
                    leading_blank,
                    awaiting,
                    &blocked_reasons,
                )
            } else {
                render_status_section(
                    &mut lines,
                    app,
                    focused,
                    task_width,
                    &diff_summaries,
                    bucket,
                    leading_blank,
                )
            };
            if emitted {
                emitted_any_section = true;
            }
        }

        if main_count == 0
            && run_total == 0
            && app.plans.iter().all(|plan| plan.planned_nodes.is_empty())
        {
            lines.push(ListItem::new(Line::from(Span::styled(
                "no agents yet  ·  type a task or /main",
                muted_style(focused),
            ))));
        }
    }

    // Harvest the recorded row spans into the row -> agent map mouse clicks resolve
    // against. Built from what was ACTUALLY rendered this frame, so it stays correct
    // however the header, hints, sections, or per-row line counts evolve.
    let mut row_map: Vec<Option<usize>> = vec![None; lines.len()];
    AGENT_ROW_SPANS.with(|spans| {
        for &(start, end, agent) in spans.borrow().iter() {
            for row in row_map.iter_mut().take(end).skip(start) {
                *row = Some(agent);
            }
        }
    });
    app.agent_row_map = row_map;
    let mut drawer_map: Vec<Option<Bucket>> = vec![None; lines.len()];
    DRAWER_ROW_SPANS.with(|spans| {
        for &(row, bucket) in spans.borrow().iter() {
            if let Some(slot) = drawer_map.get_mut(row) {
                *slot = Some(bucket);
            }
        }
    });
    app.drawer_row_map = drawer_map;

    // Scroll the pane to follow the selection. The list was rendered stateless before,
    // so it always drew from the top: with more agents than fit, the selected row (and
    // the bottom of the tree) was clipped off-screen. Compute the minimal top offset
    // that keeps the selected agent's whole row group visible, persisted across frames.
    let view_h = area.height.saturating_sub(2) as usize; // minus the block's top+bottom border
    let total = lines.len();
    let mut offset = app.agents_scroll;
    if view_h > 0 {
        let mut first: Option<usize> = None;
        let mut last: Option<usize> = None;
        // The cursor is EITHER on an agent's row group or on a collapsed drawer
        // header. Following only the agent case left the drawer headers unreachable
        // whenever they sat below the fold: j walked onto them and the pane never
        // scrolled, so the marker vanished off-screen.
        if let Some(bucket) = app.drawer_cursor {
            for (i, m) in app.drawer_row_map.iter().enumerate() {
                if *m == Some(bucket) {
                    first = Some(i);
                    last = Some(i);
                }
            }
        } else {
            for (i, m) in app.agent_row_map.iter().enumerate() {
                if *m == Some(app.selected_agent) {
                    if first.is_none() {
                        first = Some(i);
                    }
                    last = Some(i);
                }
            }
        }
        if let (Some(first), Some(last)) = (first, last) {
            if last + 1 <= view_h {
                // The whole selection fits on the first page: show from the very top so
                // the pane header (cwd, run count, hints) stays visible.
                offset = 0;
            } else if first < offset {
                // Selection is above the viewport: scroll up to reveal its top.
                offset = first;
            } else if last >= offset + view_h {
                // Selection is below the viewport: scroll down to reveal its bottom.
                offset = last + 1 - view_h;
            }
            // If the group alone is taller than the viewport, prefer showing its top.
            if last + 1 - first > view_h {
                offset = first;
            }
        }
        // Never scroll past the end (which would leave dead space at the bottom).
        offset = offset.min(total.saturating_sub(view_h));
    } else {
        offset = 0;
    }
    app.agents_scroll = offset;

    let mut list_state = ratatui::widgets::ListState::default().with_offset(offset);
    frame.render_stateful_widget(
        List::new(lines).style(app_style()).block(pane_block(
            if app.nest_view {
                "agents · nest"
            } else {
                "agents"
            },
            focused,
            app.nav_mode,
        )),
        area,
        &mut list_state,
    );
}

// --- Nest / DAG view -------------------------------------------------------
//
// ratatui has no tree widget, so the dependency tree is drawn with manual jj-log
// glyphs: a colored status BADGE node marker plus connector glyphs for ancestry.
// HARD-edge connectors render solid in MUTED; SOFT-edge connectors render with a
// dashed glyph in FAINT + DIM. The walk is a depth-first topological pass from
// roots (nodes with no present parent), guarding cycles with a visited set so an
// orphaned cycle still renders (at depth 0) rather than looping forever.

const NEST_LANE_BAR: &str = "\u{2502} "; // "│ " ancestor lane continues
const NEST_LANE_BLANK: &str = "  "; // ancestor lane ended
const NEST_TEE_HARD: &str = "\u{251c}\u{2500}"; // "├─" child with a following sibling
const NEST_CORNER_HARD: &str = "\u{2570}\u{2500}"; // "╰─" last child
const NEST_TEE_SOFT: &str = "\u{251c}\u{2504}"; // "├┄" soft, dashed feel
const NEST_CORNER_SOFT: &str = "\u{2570}\u{2504}"; // "╰┄" soft, dashed feel

/// Build the parent->children adjacency for the displayed agents, plus the edge
/// type for each (child, parent) relation. Children are kept in display (index)
/// order so the tree renders deterministically.
pub(crate) fn nest_children_by_index(
    agents: &[AgentRun],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    Vec<bool>,
) {
    use std::collections::HashMap;
    let id_to_index: HashMap<&str, usize> = agent_id_index(agents);

    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_parent = vec![false; agents.len()];

    for (child_index, agent) in agents.iter().enumerate() {
        // Hard parents first so the most-binding edge wins if a parent appears in
        // both lists; soft parents only contribute a soft edge when not already hard.
        for parent_id in &agent.deps {
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Hard));
                has_parent[child_index] = true;
            }
        }
        for parent_id in &agent.soft_deps {
            if agent.deps.iter().any(|hard| hard == parent_id) {
                continue;
            }
            if let Some(&parent_index) = id_to_index.get(parent_id.as_str()) {
                if parent_index == child_index {
                    continue;
                }
                children
                    .entry(parent_index)
                    .or_default()
                    .push((child_index, EdgeType::Soft));
                has_parent[child_index] = true;
            }
        }
    }

    (children, has_parent)
}

/// Connector glyph + style for the first (task-label) line of a node, given the
/// ancestor lane state, whether this node is the last among its siblings, and the
/// edge type from its parent. `lanes[i]` is true when ancestor level `i` still has
/// a following sibling (draw a vertical bar) and false when its subtree ended.
fn nest_prefix(lanes: &[bool], is_last: bool, edge: Option<EdgeType>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for continues in lanes {
        spans.push(Span::styled(
            if *continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    }
    if let Some(edge) = edge {
        let (glyph, style) = match edge {
            EdgeType::Hard => (
                if is_last {
                    NEST_CORNER_HARD
                } else {
                    NEST_TEE_HARD
                },
                Style::default().fg(MUTED),
            ),
            EdgeType::Soft => (
                if is_last {
                    NEST_CORNER_SOFT
                } else {
                    NEST_TEE_SOFT
                },
                Style::default().fg(FAINT).add_modifier(Modifier::DIM),
            ),
        };
        spans.push(Span::styled(glyph, style));
    }
    spans
}

/// Continuation-line prefix (for the status/diff rows under a node): the ancestor
/// lanes plus this node's own lane, so child rows align beneath the task label.
fn nest_cont_prefix(lanes: &[bool], self_continues: bool, is_root: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for continues in lanes {
        spans.push(Span::styled(
            if *continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    }
    // The node's own subtree lane: a bar while it (or its later siblings) continue.
    if !is_root {
        spans.push(Span::styled(
            if self_continues {
                NEST_LANE_BAR
            } else {
                NEST_LANE_BLANK
            },
            Style::default().fg(MUTED),
        ));
    } else if self_continues {
        spans.push(Span::styled(NEST_LANE_BAR, Style::default().fg(MUTED)));
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn nest_walk<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;

    let agent = &app.agents[index];
    let is_root = lanes.is_empty();
    let prefix = nest_prefix(lanes, is_last, edge);
    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let has_unvisited_children = own_children.iter().any(|(child, _)| !visited[*child]);
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    let summary = if agent.is_main() {
        None
    } else {
        diff_summaries.get(index).and_then(|opt| opt.clone())
    };
    push_agent_row(
        lines,
        app,
        index,
        agent,
        summary,
        focused,
        task_width,
        &prefix,
        &cont_prefix,
    );

    // Recurse into children that have not yet been placed in the tree.
    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        // In a diamond DAG an earlier sibling's subtree may have already drawn this child
        // (it has two parents); it would be skipped on entry, so skip it here too and do
        // not let it affect the is_last decision for the children that ARE drawn.
        if visited[*child_index] {
            continue;
        }
        // "Last" = no LATER pending child is still undrawn — computed dynamically so the
        // connector is a corner (not a tee) for the child that is actually drawn last,
        // even when later pending entries were already placed under an earlier sibling.
        let child_is_last = pending[position + 1..]
            .iter()
            .all(|(later, _)| visited[*later]);
        // Push THIS node's continuation (does it have a following sibling) as the ancestor
        // lane for its descendants — not the child's own flag, which double-counted and
        // misaligned the connectors.
        lanes.push(!is_last);
        nest_walk(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Test-facing wrapper that materializes the nest view as its own vec.
#[cfg(test)]
pub(crate) fn render_agents_nest(
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
) -> Vec<ListItem<'static>> {
    let mut lines: Vec<ListItem<'static>> = Vec::new();
    render_agents_nest_into(&mut lines, app, focused, task_width, diff_summaries);
    lines
}

/// Nest-view body pushed onto the CALLER's row vec, so the row positions the agent
/// row-span recorder captures are absolute agents-pane rows (mouse hit-testing
/// resolves against them).
fn render_agents_nest_into<'a>(
    lines: &mut Vec<ListItem<'a>>,
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
) {
    let (children, has_parent) = nest_children_by_index(&app.agents);
    let mut visited = vec![false; app.agents.len()];

    // Pinned planners are drawn as a dedicated row above this tree, so mark them
    // visited to keep the nest walk (and its orphan sweep) from re-drawing them.
    for index in 0..app.agents.len() {
        if app.agents[index].is_pinned_planner() {
            visited[index] = true;
        }
    }

    // Roots: any node with no present parent, in display order. Main agents have no
    // deps and so are always roots, rendered first.
    let mut roots: Vec<usize> = (0..app.agents.len())
        .filter(|index| app.agents[*index].is_main() && !app.agents[*index].is_pinned_planner())
        .collect();
    roots.extend((0..app.agents.len()).filter(|index| {
        !app.agents[*index].is_main()
            && !app.agents[*index].is_pinned_planner()
            && !has_parent[*index]
    }));

    let mut lanes: Vec<bool> = Vec::new();
    for &root in &roots {
        nest_walk(
            lines,
            app,
            focused,
            task_width,
            diff_summaries,
            &children,
            root,
            None,
            true,
            &mut lanes,
            &mut visited,
        );
    }

    // Any node not reached from a root (e.g. part of a dependency cycle) is an
    // orphan; render it at depth 0 so a cycle never hides work or loops.
    for index in 0..app.agents.len() {
        if !visited[index] {
            nest_walk(
                lines,
                app,
                focused,
                task_width,
                diff_summaries,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
}

pub(crate) fn visible_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    // Navigation follows the rendered order: main agents first, then each status
    // section nested by dependency. Sharing `sectioned_agent_order` guarantees the
    // selection marker lands exactly where j/k move.
    //
    // Drawer members are drawn inside their drawer rather than as sidebar rows, so the
    // sectioned view's navigation skips them (see `App::visible_agent_indices`). This
    // free function stays the full rendered order: nest view (`g`) still draws every
    // run as its own row, and filtering here would desync that view's j/k.
    sectioned_agent_order(agents)
}

/// The sectioned view's navigable rows: everything except the runs that a collapsed
/// drawer holds.
pub(crate) fn sidebar_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    visible_agent_indices(agents)
        .into_iter()
        .filter(|&index| {
            agents
                .get(index)
                .is_none_or(|agent| !in_drawer(agents, agent))
        })
        .collect()
}

/// The navigation order for the NEST view (`g`), mirroring `render_agents_nest` exactly
/// so j/k land on the row that is actually drawn. The sectioned order cannot be reused
/// here because nest view walks the dependency tree globally (crossing status buckets)
/// rather than within each section. Order: pinned orchestrators (drawn above the tree),
/// then mains, then dependency roots, then the tree (DFS via the children map), then any
/// cycle-trapped orphans.
pub(crate) fn nest_agent_order(agents: &[AgentRun]) -> Vec<usize> {
    let (children, has_parent) = nest_children_by_index(agents);
    let mut order: Vec<usize> = Vec::new();
    let mut visited = vec![false; agents.len()];

    for index in 0..agents.len() {
        if agents[index].is_pinned_planner() {
            order.push(index);
            visited[index] = true;
        }
    }
    let mut roots: Vec<usize> = (0..agents.len())
        .filter(|&i| agents[i].is_main() && !agents[i].is_pinned_planner())
        .collect();
    roots.extend(
        (0..agents.len())
            .filter(|&i| !agents[i].is_main() && !agents[i].is_pinned_planner() && !has_parent[i]),
    );
    for &root in &roots {
        nest_order_walk(&mut order, &children, root, &mut visited);
    }
    for index in 0..agents.len() {
        if !visited[index] {
            nest_order_walk(&mut order, &children, index, &mut visited);
        }
    }
    order
}

fn nest_order_walk(
    order: &mut Vec<usize>,
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    order.push(index);
    if let Some(kids) = children.get(&index) {
        for (child, _) in kids {
            if !visited[*child] {
                nest_order_walk(order, children, *child, visited);
            }
        }
    }
}

/// Push one task row of the orchestrator DAG tree: nest glyphs, a live-status
/// BADGE, the task title, and the status label. Mirrors `push_planned_row` styling
/// but the badge color reflects the task's LIVE status.
fn push_orchestrator_task_row<'a>(
    lines: &mut Vec<Line<'a>>,
    task: &RudderPlanTask,
    status: OrchTaskStatus,
    prefix: &[Span<'a>],
) {
    let label = if task.title.trim().is_empty() {
        summarize_task(&task.prompt)
    } else {
        task.title.clone()
    };
    let mut spans = prefix.to_vec();
    spans.extend([
        Span::styled(BADGE, Style::default().fg(status.color())),
        Span::raw(" "),
        // The node id is the cross-reference key: the same id shows on the worker's
        // agents-pane row and in the worker pane title, so "n2 here" is findable there.
        Span::styled(format!("{} ", task.id), muted_style(true)),
        Span::styled(label, pane_text_style(true)),
        Span::raw("  "),
        Span::styled(status.label(), Style::default().fg(status.color())),
    ]);
    lines.push(Line::from(spans));
}

/// Depth-first walk of the parsed plan task forest for the orchestrator DAG tree.
/// Nests a task under a parent that is also in the plan (hard edges solid MUTED,
/// soft edges dashed/dimmed), reusing the same nest glyphs as the agents pane.
#[allow(clippy::too_many_arguments)]
/// Build the orchestrator DAG tree structure. Nesting follows HARD edges only:
/// soft edges are advisory "context" that never gate launch, so soft-linked tasks
/// run in parallel and must NOT be drawn as a sequential chain. A node nests under its
/// DEEPEST hard prerequisite (longest hard chain, ties by index) so an integrator
/// renders below its real inputs; a node with no hard parent is a top-level (parallel)
/// root. Returns
/// the children adjacency and which nodes have a hard parent. Pure, so the nesting
/// is unit-testable.
/// Longest hard-dependency chain depth for task `index` over the FULL hard-edge graph
/// (memoized, cycle-safe). A node with no hard prereqs is depth 0. Used so a JOIN nests
/// under its DEEPEST prerequisite rather than under whichever has the largest id.
fn hard_chain_depth(
    index: usize,
    tasks: &[RudderPlanTask],
    id_to_index: &std::collections::HashMap<&str, usize>,
    memo: &mut [Option<usize>],
    in_progress: &mut [bool],
) -> usize {
    if let Some(depth) = memo[index] {
        return depth;
    }
    if in_progress[index] {
        return 0; // cycle: break the recursion
    }
    in_progress[index] = true;
    let depth = tasks[index]
        .deps
        .iter()
        .filter(|edge| edge.edge == EdgeType::Hard)
        .filter_map(|edge| id_to_index.get(edge.on.as_str()).copied())
        .filter(|&parent| parent != index)
        .map(|parent| 1 + hard_chain_depth(parent, tasks, id_to_index, memo, in_progress))
        .max()
        .unwrap_or(0);
    in_progress[index] = false;
    memo[index] = Some(depth);
    depth
}

pub(crate) fn orchestrator_hard_tree(
    tasks: &[RudderPlanTask],
) -> (
    std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    Vec<bool>,
) {
    use std::collections::HashMap;
    let id_to_index: HashMap<&str, usize> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();
    let mut memo = vec![None; tasks.len()];
    let mut in_progress = vec![false; tasks.len()];
    let depths: Vec<usize> = (0..tasks.len())
        .map(|i| hard_chain_depth(i, tasks, &id_to_index, &mut memo, &mut in_progress))
        .collect();
    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_parent = vec![false; tasks.len()];
    for (child_index, task) in tasks.iter().enumerate() {
        // Of all resolvable hard prerequisites, nest under the DEEPEST (longest hard
        // chain), ties broken by index. So an integrator that depends on several units
        // renders below its deepest input instead of under a shallow parallel root.
        let hard_parent = task
            .deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Hard)
            .filter_map(|edge| id_to_index.get(edge.on.as_str()).copied())
            .filter(|&parent_index| parent_index != child_index)
            .max_by_key(|&parent_index| (depths[parent_index], parent_index));
        if let Some(parent_index) = hard_parent {
            children
                .entry(parent_index)
                .or_default()
                .push((child_index, EdgeType::Hard));
            has_parent[child_index] = true;
        }
    }
    (children, has_parent)
}

/// Build the orchestrator DAG tree rows (no status/summary lines). Roots (no resolvable
/// hard parent) first, depth-first; any cycle-trapped node is then rendered at depth 0 so
/// work is never hidden. Extracted so the rendered structure is unit-testable.
pub(crate) fn orchestrator_dag_tree_lines(
    app: &App,
    tasks: &[RudderPlanTask],
) -> Vec<Line<'static>> {
    let mut dag: Vec<Line<'static>> = Vec::new();
    let (children, has_parent) = orchestrator_hard_tree(tasks);
    let mut visited = vec![false; tasks.len()];
    let mut lanes: Vec<bool> = Vec::new();
    for index in 0..tasks.len() {
        if !has_parent[index] {
            orchestrator_tree_walk(
                &mut dag,
                app,
                tasks,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    for index in 0..tasks.len() {
        if !visited[index] {
            orchestrator_tree_walk(
                &mut dag,
                app,
                tasks,
                &children,
                index,
                None,
                true,
                &mut lanes,
                &mut visited,
            );
        }
    }
    dag
}

fn orchestrator_tree_walk<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    tasks: &[RudderPlanTask],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    let prefix = nest_prefix(lanes, is_last, edge);
    let status = orchestrator_task_status(app, &tasks[index].id);
    push_orchestrator_task_row(lines, &tasks[index], status, &prefix);

    let own_children = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);
    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        // Push THIS node's continuation (does it have a following sibling), which becomes
        // the ancestor lane drawn to the LEFT of every descendant. (Pushing the child's
        // own flag added a spurious extra column and misaligned the connectors.)
        lanes.push(!is_last);
        orchestrator_tree_walk(
            lines,
            app,
            tasks,
            children,
            *child_index,
            Some(*child_edge),
            child_is_last,
            lanes,
            visited,
        );
        lanes.pop();
    }
}

/// Custom command-center view for a selected orchestrator (RudderPlan) agent,
/// rendered in place of the raw planner PTY. PLANNING shows an animated spinner;
/// PLAN-READY renders a DAG tree of the parsed tasks with live status badges.
pub(crate) fn render_orchestrator(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    use std::collections::HashMap;
    let focused = app.focus == FocusPane::Worker;
    let Some(agent) = app.agents.get(app.selected_agent) else {
        return;
    };

    let phase = orchestrator_phase_for_app(app, agent);
    let phase_label = match app.plan().final_gate_status {
        FinalGateStatus::Running => "all integrated · verifying".to_string(),
        FinalGateStatus::Passed => "all integrated · checks passed".to_string(),
        FinalGateStatus::Failed => "all integrated · checks failed".to_string(),
        FinalGateStatus::Idle => match &phase {
            OrchestratorPhase::Planning => "plan mode".to_string(),
            OrchestratorPhase::PlanReady(tasks) => format!("plan · {} tasks", tasks.len()),
        },
    };
    // The model + version doing the planning, shown on the header so it is always clear
    // which agent/version is in plan mode (e.g. "Claude Code · opus[1m]").
    let backend_label = match agent.backend {
        Backend::Claude => "Claude Code",
        Backend::Codex => "Codex",
        Backend::Opencode => "opencode",
    };
    let model_label = if agent.model.trim().is_empty() {
        backend_label.to_string()
    } else {
        format!("{backend_label} · {}", agent.model.trim())
    };

    // Draw the pane border first; content is laid out inside it. Splitting the
    // inner area into fixed (header + DAG) and scrollable (plan prose) regions lets
    // the DAG tree stay PINNED on top while only the prose plan scrolls.
    let block = pane_block("orchestrator", focused, app.nav_mode);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Header: ◆ ORCHESTRATOR · phase · <backend · model>.
    let header_spans = vec![
        Span::styled(
            format!("{ORCH_MARK} ORCHESTRATOR"),
            Style::default().fg(ACCENT),
        ),
        Span::styled("  ·  ", muted_style(focused)),
        Span::styled(phase_label, muted_style(focused)),
        Span::styled("  ·  ", muted_style(focused)),
        Span::styled(model_label, Style::default().fg(ACCENT)),
    ];
    let header: Vec<Line<'static>> = vec![Line::from(header_spans)];

    // Chat input (pinned at the bottom when focused): the follow-up being composed,
    // with a visible block cursor at worker_input_cursor so Left/Right/Home/End and
    // typing show where the insertion point is. Rendered inline (a reversed cell) so
    // it tracks the text through wrapping without any coordinate math.
    let draft = agent.worker_input_draft.clone();
    let cursor_col = agent.worker_input_cursor;
    let input: Vec<Line<'static>> = if focused {
        let mut spans = vec![Span::styled("› ", Style::default().fg(ACCENT))];
        let cursor_style = pane_text_style(focused).add_modifier(Modifier::REVERSED);
        let chars: Vec<char> = draft.chars().collect();
        if chars.is_empty() {
            // Cursor block, then the dimmed hint.
            spans.push(Span::styled(" ", cursor_style));
            spans.push(Span::styled(
                "chat to refine the plan, or press Enter to approve & launch",
                muted_style(focused),
            ));
        } else {
            let cursor = cursor_col.min(chars.len());
            let before: String = chars[..cursor].iter().collect();
            if !before.is_empty() {
                spans.push(Span::styled(before, pane_text_style(focused)));
            }
            if cursor < chars.len() {
                spans.push(Span::styled(chars[cursor].to_string(), cursor_style));
                let after: String = chars[cursor + 1..].iter().collect();
                if !after.is_empty() {
                    spans.push(Span::styled(after, pane_text_style(focused)));
                }
            } else {
                // Cursor at end of the draft: a trailing block.
                spans.push(Span::styled(" ", cursor_style));
            }
        }
        vec![Line::default(), Line::from(spans)]
    } else {
        Vec::new()
    };

    match phase {
        OrchestratorPhase::Planning => {
            // Smooth live stream: the model's reasoning (dim), the files it inspects,
            // and the plan as it writes it. No pinned region; the whole transcript
            // scrolls and sticks to the bottom so new output stays in view.
            let mut body: Vec<Line<'static>> = Vec::new();
            // If the planner process has EXITED without a parseable DAG (it asked a
            // clarifying question or needs more detail), show a terminal PAUSED state with
            // a static badge instead of an animated spinner that would imply it is still
            // working forever. A typed message resumes the session (planner_awaiting_input).
            let paused = agent.status != AgentStatus::Running;
            if !paused {
                let spinner_label = if app.plan().rebasing {
                    "rebasing the plan…"
                } else if app.plan().refining {
                    "refining the plan…"
                } else {
                    "decomposing the task…"
                };
                body.push(Line::from(vec![
                    Span::styled(app.spinner_glyph(), Style::default().fg(ACCENT)),
                    Span::raw(" "),
                    Span::styled(spinner_label, pane_text_style(true)),
                ]));
                body.push(Line::default());
            }
            // The planner's reasoning + the files it inspected, then (when paused) the
            // prose that leads into the questions. The RUDDER_QUESTIONS block is stripped
            // here because it is re-rendered as a clean numbered prompt just below, so the
            // reading order is: inspection → "what shapes the build" → the questions.
            let empty: &[PlanEntry] = &[];
            let transcript = agent
                .plan_stream
                .as_ref()
                .map(|stream| stream.transcript())
                .unwrap_or(empty);
            if transcript.is_empty() {
                body.push(Line::from(Span::styled(
                    "starting the planner…",
                    muted_style(focused),
                )));
            } else {
                // Only strip the questions block when we actually re-render it as a
                // separate prompt below; otherwise (no parsed questions) leave it in the
                // transcript so the "its question is above" pointer is truthful.
                let strip_questions = paused && !app.plan().pending_questions.is_empty();
                push_transcript_lines(&mut body, transcript, focused, strip_questions);
            }
            if paused {
                // Render the question/answer step as a bordered "plan mode" card with the
                // free-text answer field pulled INSIDE the box (native-plan-mode feel),
                // anchored at the BOTTOM of the body so it stays in view (the body sticks to
                // the bottom) instead of scrolling off above a long transcript. The answer is
                // `worker_input_draft`; Enter submits it (refine), Esc clears it.
                body.push(Line::default());
                if app.plan().pending_questions.is_empty() {
                    body.push(Line::from(Span::styled(
                        "❓ The planner needs your input",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )));
                    body.push(Line::from(Span::styled(
                        "Its question is at the end of the transcript above.",
                        pane_text_style(focused),
                    )));
                } else {
                    push_question_card(
                        &mut body,
                        &app.plan().pending_questions,
                        &agent.worker_input_draft,
                        agent.worker_input_cursor,
                        inner.width as usize,
                        focused,
                    );
                }
            }
            // While paused, the answer is typed into the card's own field, so suppress the
            // separate chat-input region to avoid a duplicated cursor below the box.
            let input_region: &[Line<'static>] = if paused { &[] } else { &input };
            let follow_bottom = app.orch_follow_bottom;
            let max_scroll = render_orchestrator_layout(
                frame,
                inner,
                &header,
                &[],
                &body,
                input_region,
                &mut app.orch_dag_scroll,
                follow_bottom,
            );
            app.orch_dag_max_scroll = max_scroll;
        }
        OrchestratorPhase::PlanReady(mut tasks) => {
            // Fold in nodes the user RECONCILED into the plan after launch. They live
            // only in `planned_nodes` (never in the orchestrator's frozen plan block),
            // so without this they are invisible in the orchestrator DAG. The initial
            // nodes share ids with `tasks`, so only post-launch additions get appended
            // here, and the user sees a typed task show up in the DAG.
            for node in &app.plan().planned_nodes {
                if !tasks.iter().any(|task| task.id == node.id) {
                    tasks.push(node.to_task());
                }
            }
            // PINNED top region: the DAG tree + a live status line. The tree nests
            // by HARD edges only (see orchestrator_hard_tree) so parallel soft-linked
            // tasks render as top-level roots instead of a misleading chain.
            let mut dag: Vec<Line<'static>> = orchestrator_dag_tree_lines(app, &tasks);
            let (mut running, mut review, mut done, mut todo, mut failed) =
                (0usize, 0usize, 0usize, 0usize, 0usize);
            for task in &tasks {
                match orchestrator_task_status(app, &task.id) {
                    OrchTaskStatus::Running => running += 1,
                    OrchTaskStatus::Review => review += 1,
                    OrchTaskStatus::Done => done += 1,
                    OrchTaskStatus::Failed => failed += 1,
                    OrchTaskStatus::Todo => todo += 1,
                }
            }
            // Surface failed tasks too (they were silently dropped from the tally, hiding
            // that a node failed and is blocking its dependents). Only shown when > 0.
            let failed_suffix = if failed > 0 {
                format!(" · failed {failed}")
            } else {
                String::new()
            };
            dag.push(Line::from(Span::styled(
                format!("running {running} · review {review} · done {done} · todo {todo}{failed_suffix}"),
                muted_style(focused),
            )));
            if app.plan().awaiting_approval {
                dag.push(Line::from(Span::styled(
                    "type below to refine  ·  Enter to approve & launch",
                    Style::default().fg(ACCENT),
                )));
            }

            // SCROLLABLE body: the human summary, THEN a per-node breakdown. Showing
            // each task's goal, done-when, and dependencies surfaces the plan's full
            // depth and fills the pane instead of leaving it blank below a short
            // summary.
            let mut prose: Vec<Line<'static>> = Vec::new();
            prose.push(Line::from(Span::styled("Plan", header_style(focused))));
            // Read the cached LIVE summary rather than only the frozen app.plan().plan_summary
            // captured at exit-detection: final bytes can arrive a tick or two after the
            // process is marked done. The cache refreshes on semantic stream changes, so
            // the prose self-heals without reparsing the whole plan on every redraw.
            let summary = rudder_plan_summary_for_run(agent)
                .or_else(|| app.plan().plan_summary.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(summary) = summary {
                for raw in summary.lines() {
                    prose.push(Line::from(Span::styled(
                        raw.trim_end().to_string(),
                        pane_text_style(focused),
                    )));
                }
            }

            // Per-node breakdown: title (+ dependency note), goal, and done-when for
            // every task, so the plan reads in full without launching anything.
            prose.push(Line::default());
            prose.push(Line::from(Span::styled("Tasks", header_style(focused))));
            for task in &tasks {
                let hard: Vec<&str> = task
                    .deps
                    .iter()
                    .filter(|edge| edge.edge == EdgeType::Hard)
                    .map(|edge| edge.on.as_str())
                    .collect();
                let soft: Vec<&str> = task
                    .deps
                    .iter()
                    .filter(|edge| edge.edge == EdgeType::Soft)
                    .map(|edge| edge.on.as_str())
                    .collect();
                let note = if hard.is_empty() && soft.is_empty() {
                    "  ·  parallel".to_string()
                } else {
                    let mut parts = String::new();
                    if !hard.is_empty() {
                        parts.push_str(&format!("  ·  after {}", hard.join(", ")));
                    }
                    if !soft.is_empty() {
                        parts.push_str(&format!("  ·  with {}", soft.join(", ")));
                    }
                    parts
                };
                prose.push(Line::default());
                prose.push(Line::from(vec![
                    Span::styled(format!("{}  ", task.id), accent_style(focused)),
                    Span::styled(task.title.clone(), pane_text_style(focused)),
                    Span::styled(note, muted_style(focused)),
                ]));
                if let Some(goal) = task
                    .goal
                    .as_deref()
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                {
                    prose.push(Line::from(Span::styled(
                        format!("    goal: {goal}"),
                        muted_style(focused),
                    )));
                }
                if let Some(success) = task
                    .success
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    prose.push(Line::from(Span::styled(
                        format!("    done when: {success}"),
                        muted_style(focused),
                    )));
                }
            }

            // Activity: the conductor's autonomous actions (auto-expand, steering),
            // so the user always sees what the brain did without a confirm gate.
            if !app.activity_log.is_empty() {
                prose.push(Line::default());
                prose.push(Line::from(Span::styled("Activity", header_style(focused))));
                let start = app.activity_log.len().saturating_sub(8);
                for entry in &app.activity_log[start..] {
                    prose.push(Line::from(Span::styled(
                        format!("· {entry}"),
                        muted_style(focused),
                    )));
                }
            }
            let max_scroll = render_orchestrator_layout(
                frame,
                inner,
                &header,
                &dag,
                &prose,
                &input,
                &mut app.orch_dag_scroll,
                false,
            );
            app.orch_dag_max_scroll = max_scroll;
        }
    }

    capture_orchestrator_visible_rows(frame, inner, app);
}

fn capture_orchestrator_visible_rows(frame: &mut Frame<'_>, inner: Rect, app: &mut App) {
    // Read the pane back from the rendered buffer as plain-text rows (post wrap +
    // scroll) so a mouse drag can select and copy exactly what is on screen — the
    // pane composes Lines, not the PTY, so this is the only faithful source for the
    // selection machinery. Then paint the active selection by reversing its cells.
    let selection = app.orch_selection.map(normalize_selection);
    let buf = frame.buffer_mut();
    let mut rows: Vec<String> = Vec::with_capacity(inner.height as usize);
    for (row_idx, y) in (inner.y..inner.bottom()).enumerate() {
        let sel_cols = selection_for_row(selection, row_idx);
        let mut text = String::new();
        for (col_idx, x) in (inner.x..inner.right()).enumerate() {
            let symbol = buf
                .cell((x, y))
                .map(|cell| cell.symbol().to_string())
                .unwrap_or_else(|| " ".to_string());
            text.push_str(&symbol);
            if sel_cols.is_some_and(|(start, end)| col_idx >= start && col_idx <= end) {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(Style::default().fg(INK).bg(SURFACE_SEL));
                }
            }
        }
        rows.push(text);
    }
    app.orch_visible_rows = rows;
}

/// Lay out the orchestrator pane inside `inner` as: header (fixed) · top/DAG
/// (PINNED, capped) · body (SCROLLABLE) · input (pinned bottom). Only the body
/// scrolls (via `scroll`); the DAG stays on top. `follow_bottom` keeps the body
/// pinned to its last line until the user scrolls away.
#[allow(clippy::too_many_arguments)]
fn render_orchestrator_layout(
    frame: &mut Frame<'_>,
    inner: Rect,
    header: &[Line<'static>],
    top: &[Line<'static>],
    body: &[Line<'static>],
    input: &[Line<'static>],
    scroll: &mut usize,
    follow_bottom: bool,
) -> usize {
    let width = inner.width.max(1) as usize;
    let rows = |lines: &[Line<'static>]| -> u16 {
        lines
            .iter()
            .map(|line| wrapped_row_count(line.width().max(1), width))
            .sum::<usize>()
            .min(u16::MAX as usize) as u16
    };
    let header_h = rows(header);
    let input_h = rows(input);
    let avail = inner
        .height
        .saturating_sub(header_h)
        .saturating_sub(input_h);
    // Pin the DAG but never let it eat more than half the body area, so the prose
    // plan always has room; it shows the top of the tree if it is very tall.
    let top_h = if top.is_empty() {
        0
    } else {
        rows(top).min((avail / 2).max(3)).min(avail)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(top_h),
            Constraint::Min(1),
            Constraint::Length(input_h),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(header.to_vec())
            .style(app_style())
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    if top_h > 0 {
        frame.render_widget(
            Paragraph::new(top.to_vec())
                .style(app_style())
                .wrap(Wrap { trim: false }),
            chunks[1],
        );
    }
    let body_area = chunks[2];
    let body_visible = body_area.height.max(1) as usize;
    let body_rows: usize = body
        .iter()
        .map(|line| wrapped_row_count(line.width().max(1), width))
        .sum();
    let max_scroll = body_rows.saturating_sub(body_visible);
    let effective = orchestrator_scroll_offset(scroll, max_scroll, follow_bottom);
    frame.render_widget(
        Paragraph::new(body.to_vec())
            .style(app_style())
            .scroll((effective as u16, 0))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    if input_h > 0 {
        frame.render_widget(
            Paragraph::new(input.to_vec())
                .style(app_style())
                .wrap(Wrap { trim: false }),
            chunks[3],
        );
    }
    max_scroll
}

/// A blank interior row of the plan-mode card: `│` + spaces + `│`, exactly `box_w` wide.
fn card_blank(box_w: usize, border: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│", border),
        Span::raw(" ".repeat(box_w.saturating_sub(2))),
        Span::styled("│", border),
    ])
}

/// A top (`╭─ … ─╮`) or bottom (`╰─ … ─╯`) border row with an embedded label,
/// exactly `box_w` wide. The label is truncated with `…` if it would not fit.
fn card_border(
    top: bool,
    label: &str,
    box_w: usize,
    border: Style,
    label_style: Style,
) -> Line<'static> {
    let (lead, corner) = if top {
        ("╭─ ", "╮")
    } else {
        ("╰─ ", "╯")
    };
    // Reserve: lead(3) + label + trailing space(1) + dashes(>=1) + corner(1).
    let max_label = box_w.saturating_sub(6);
    let label_fit: String = if label.chars().count() > max_label {
        format!(
            "{}…",
            label
                .chars()
                .take(max_label.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        label.to_string()
    };
    let used = 3 + label_fit.chars().count() + 1; // lead + label + trailing space
    let dashes = box_w.saturating_sub(used + 1); // +1 for the corner
    Line::from(vec![
        Span::styled(lead, border),
        Span::styled(label_fit, label_style),
        Span::styled(" ", border),
        Span::styled("─".repeat(dashes), border),
        Span::styled(corner, border),
    ])
}

/// Render the planner's clarifying questions as a bordered "plan mode" card with the
/// free-text answer field pulled INSIDE the box, for a native-plan-mode feel. `draft`
/// + `cursor` are the live `worker_input_draft`/`worker_input_cursor`: Enter submits
/// the answer (refine), Esc clears it. All width math is char-count based, which
/// matches how ratatui renders this (ASCII-ish) content, so the borders stay aligned.
pub(crate) fn push_question_card(
    body: &mut Vec<Line<'static>>,
    questions: &[String],
    draft: &str,
    cursor: usize,
    width: usize,
    focused: bool,
) {
    let border = Style::default().fg(ACCENT);
    let accent = Style::default().fg(ACCENT);
    let text = pane_text_style(focused);
    let box_w = width.max(1);

    // Narrow-pane fallback: a plain numbered list (a box would wrap and break).
    if box_w < 30 {
        body.push(Line::from(Span::styled(
            "❓ The planner needs your input",
            accent.add_modifier(Modifier::BOLD),
        )));
        for (i, q) in questions.iter().enumerate() {
            body.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), accent),
                Span::styled(q.clone(), text),
            ]));
        }
        body.push(Line::from(Span::styled(
            "↳ type your answer below and press Enter",
            muted_style(focused),
        )));
        return;
    }

    let content_w = box_w - 4; // "│ " (2) + content + " │" (2)

    body.push(card_border(
        true,
        "◆ Plan mode · a few quick questions",
        box_w,
        border,
        accent.add_modifier(Modifier::BOLD),
    ));
    body.push(card_blank(box_w, border));

    // Numbered, wrapped questions. The number label is a fixed 3-col gutter so wrapped
    // continuation lines hang-indent under the text.
    let label_w = 3usize;
    let q_avail = content_w.saturating_sub(label_w);
    for (i, q) in questions.iter().enumerate() {
        for (li, seg) in wrap_text(q, q_avail as u16).iter().enumerate() {
            let label = if li == 0 {
                format!("{:<width$}", i + 1, width = label_w)
            } else {
                " ".repeat(label_w)
            };
            let pad = content_w.saturating_sub(label_w + seg.chars().count());
            body.push(Line::from(vec![
                Span::styled("│ ", border),
                Span::styled(label, accent),
                Span::styled(seg.clone(), text),
                Span::raw(" ".repeat(pad)),
                Span::styled(" │", border),
            ]));
        }
    }

    body.push(card_blank(box_w, border));

    // The answer field, inside the box: "› " + the live draft with a block cursor.
    let inner_avail = content_w.saturating_sub(2); // after the "› " prompt
    let cursor_style = text.add_modifier(Modifier::REVERSED);
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled("│ ", border), Span::styled("› ", accent)];
    let mut used = 2usize; // "› "
    let chars: Vec<char> = draft.chars().collect();
    if chars.is_empty() {
        if focused {
            spans.push(Span::styled(" ", cursor_style));
            used += 1;
            let ph: String = "type your answer, e.g. 1: reuse, 2: mock"
                .chars()
                .take(inner_avail.saturating_sub(1))
                .collect();
            used += ph.chars().count();
            spans.push(Span::styled(ph, muted_style(focused)));
        } else {
            let ph: String = "type to answer".chars().take(inner_avail).collect();
            used += ph.chars().count();
            spans.push(Span::styled(ph, muted_style(focused)));
        }
    } else if !focused {
        let shown: String = chars.iter().take(inner_avail).collect();
        used += shown.chars().count();
        spans.push(Span::styled(shown, text));
    } else if chars.len() + 1 <= inner_avail {
        // Whole draft fits: render before / cursor cell / after so mid-line editing shows.
        let cur = cursor.min(chars.len());
        let before: String = chars[..cur].iter().collect();
        let after: String = if cur < chars.len() {
            chars[cur + 1..].iter().collect()
        } else {
            String::new()
        };
        used += before.chars().count();
        spans.push(Span::styled(before, text));
        let cell = chars
            .get(cur)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".to_string());
        used += 1;
        spans.push(Span::styled(cell, cursor_style));
        used += after.chars().count();
        spans.push(Span::styled(after, text));
    } else {
        // Overflow: tail-anchor with a leading ellipsis and an end cursor.
        let tail: String = chars
            .iter()
            .skip(chars.len().saturating_sub(inner_avail.saturating_sub(2)))
            .collect();
        used += 1;
        spans.push(Span::styled("…".to_string(), muted_style(focused)));
        used += tail.chars().count();
        spans.push(Span::styled(tail, text));
        used += 1;
        spans.push(Span::styled(" ", cursor_style));
    }
    let pad = content_w.saturating_sub(used);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(" │", border));
    body.push(Line::from(spans));

    body.push(card_border(
        false,
        "↵ continue  ·  esc clear  ·  or just describe what you want",
        box_w,
        border,
        muted_style(focused),
    ));
}

/// Render a planner transcript (reasoning/tool/text/user turns) into `out`, with
/// dim italic thinking, muted tool/system lines, and accented user turns.
pub(crate) fn push_transcript_lines(
    out: &mut Vec<Line<'static>>,
    transcript: &[PlanEntry],
    focused: bool,
    strip_questions: bool,
) {
    // When the planner has paused for input, the RUDDER_QUESTIONS block is rendered
    // separately as a clean numbered prompt, so showing it again here (raw markers
    // and all) is noisy duplication. Skip the block — including its START/END marker
    // lines — wherever it appears in the streamed text. `skipping` persists across
    // entries because streamed deltas can split the block over several Text entries.
    let mut skipping = false;
    for entry in transcript {
        let (prefix, style) = match entry.kind {
            PlanEntryKind::Thinking => ("· ", muted_style(focused).add_modifier(Modifier::ITALIC)),
            PlanEntryKind::Tool => ("  ", muted_style(focused)),
            PlanEntryKind::System => ("", muted_style(focused)),
            PlanEntryKind::UserTurn => ("you: ", Style::default().fg(ACCENT)),
            PlanEntryKind::Text => ("", pane_text_style(focused)),
        };
        let mut first = true;
        for sub in entry.text.split('\n') {
            if strip_questions {
                let marker = sub.trim();
                if marker == "RUDDER_QUESTIONS_START" {
                    skipping = true;
                    first = false;
                    continue;
                }
                if marker == "RUDDER_QUESTIONS_END" {
                    skipping = false;
                    first = false;
                    continue;
                }
                if skipping {
                    first = false;
                    continue;
                }
            }
            let content = if first {
                format!("{prefix}{}", sub.trim_end())
            } else {
                sub.trim_end().to_string()
            };
            first = false;
            if content.is_empty() {
                continue;
            }
            out.push(Line::from(Span::styled(content, style)));
        }
    }
}

/// Render the INTERACTIVE orchestrator (opt-in): a live DAG pane on top (from the plan
/// the orchestrator wrote to its plan file → `planned_nodes`) and the raw interactive
/// backend PTY below, which the user converses with directly.
pub(crate) fn interactive_orchestrator_areas(area: Rect) -> (Rect, Rect) {
    let dag_h = (area.height / 3).clamp(6, area.height.saturating_sub(6).max(6));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(dag_h), Constraint::Min(1)])
        .split(area);
    (chunks[0], chunks[1])
}

pub(crate) fn render_interactive_orchestrator(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Worker;
    // DAG pane on top (capped to a third, leaving room for the conversation), PTY below.
    let (dag_area, term_area) = interactive_orchestrator_areas(area);

    // ---- DAG pane (top) ----
    // Render the WHOLE plan (queued + already-launched nodes), so the tree stays
    // intact after approval drains the queue into running workers. orchestrator_dag_tasks
    // reconstructs launched nodes from their agents; status badges come from live runs.
    let mut dag_lines: Vec<Line<'static>> = Vec::new();
    let tasks = app.orchestrator_dag_tasks();
    if tasks.is_empty() {
        dag_lines.push(Line::from(Span::styled(
            "Planning… talk to the orchestrator below.",
            muted_style(focused),
        )));
        dag_lines.push(Line::from(Span::styled(
            "It writes the task DAG to RUDDER.md; the DAG shows here.",
            muted_style(focused),
        )));
    } else {
        dag_lines = orchestrator_dag_tree_lines(app, &tasks);
        if app.plan().awaiting_approval {
            dag_lines.push(Line::from(Span::styled(
                "awaiting approval — press empty Enter in the task pane to launch",
                Style::default().fg(ACCENT),
            )));
        }
    }
    let dag_inner = block_inner(dag_area);
    let dag_rows: usize = dag_lines
        .iter()
        .map(|line| wrapped_row_count(line.width().max(1), dag_inner.width.max(1) as usize))
        .sum();
    let max_scroll = orchestrator_dag_max_scroll(dag_rows, dag_inner.height.max(1) as usize);
    let scroll = orchestrator_scroll_offset(&mut app.orch_dag_scroll, max_scroll, false);
    app.orch_dag_max_scroll = max_scroll;
    frame.render_widget(
        Paragraph::new(dag_lines)
            .style(app_style())
            .block(pane_block("dag", focused, app.nav_mode))
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        dag_area,
    );
    capture_orchestrator_visible_rows(frame, dag_inner, app);

    // ---- interactive PTY (bottom) ----
    let term_inner = block_inner(term_area);
    // Once the plan is approved, the orchestrator is Stopped (its PTY was killed): it
    // has handed off to the worker fleet and must never implement itself. Show a clear
    // hand-off banner instead of the dead terminal so the pane explains what happened
    // and points at the work above.
    let orchestrator_stopped = app
        .agents
        .get(app.selected_agent)
        .is_some_and(|run| run.is_orchestrator() && run.status == AgentStatus::Stopped);
    if orchestrator_stopped {
        let working = app
            .agents
            .iter()
            .filter(|run| run.node_id.is_some() && run.status == AgentStatus::Running)
            .count();
        let banner = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Plan approved. Orchestrator stopped.",
                pane_text_style(focused),
            )),
            Line::from(Span::styled(
                format!("{working} worker(s) implementing the DAG above. Press ↑/↓ to watch them."),
                muted_style(focused),
            )),
        ];
        frame.render_widget(
            Paragraph::new(banner)
                .style(app_style())
                .block(pane_block("orchestrator · stopped", focused, app.nav_mode))
                .wrap(Wrap { trim: false }),
            term_area,
        );
        return;
    }
    if let Ok(size) = TerminalSize::new(term_inner.height.max(1), term_inner.width.max(1)) {
        if let Some(run) = app.agents.get_mut(app.selected_agent) {
            if run.terminal_size != Some(size) {
                if let Some(terminal) = run.terminal.as_mut() {
                    if terminal.resize(size).is_ok() {
                        run.terminal_size = Some(size);
                    }
                }
            }
        }
    }
    let lines = worker_lines(app, term_inner.height as usize, term_inner.width as usize);
    frame.render_widget(
        Paragraph::new(lines)
            .style(app_style())
            .block(pane_block("orchestrator", focused, app.nav_mode))
            .wrap(Wrap { trim: false }),
        term_area,
    );
    if focused {
        set_worker_cursor(frame, term_inner, app);
    }
}

/// Split for the finished-worker card view: card (objective + what-it-did) on
/// top, conversation below. Same shape as the interactive orchestrator's split so
/// the mouse handlers can derive the sub-areas from the pane rect alone.
pub(crate) fn done_card_areas(area: Rect) -> (Rect, Rect) {
    let card_h = (area.height / 3).clamp(6, area.height.saturating_sub(6).max(6));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(card_h), Constraint::Min(1)])
        .split(area);
    (chunks[0], chunks[1])
}

/// The finished-worker card content: status + title, the objective, and the
/// worker's own completion summary ("what it did"). Pure so tests can assert it.
pub(crate) fn done_worker_card_lines(run: &AgentRun, focused: bool) -> Vec<Line<'static>> {
    let title = if run.task_summary.trim().is_empty() {
        summarize_task(&run.task)
    } else {
        run.task_summary.clone()
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{} · {}", run.status.as_str(), title),
            pane_text_style(focused),
        )),
        Line::from(""),
        Line::from(Span::styled("Objective", muted_style(focused))),
    ];
    for text_line in run.task.lines().take(12) {
        lines.push(Line::from(Span::styled(
            text_line.to_string(),
            pane_text_style(focused),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "What it did",
        muted_style(focused),
    )));
    match run.done_summary.as_deref() {
        Some(summary) => {
            for text_line in summary.lines() {
                lines.push(Line::from(Span::styled(
                    text_line.to_string(),
                    pane_text_style(focused),
                )));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "(no completion summary was recorded)",
            muted_style(focused),
        ))),
    }
    lines
}

/// Two-panel view for a FINISHED worker, mirroring the interactive orchestrator:
/// the card on top (objective + what it did), the conversation below. The session
/// stays conversable — typing below resumes it and the card refreshes when the
/// resumed turn completes.
/// One line per finished run in a drawer: node label (or `·` when it has none), the
/// task summary, and its diff size when the run recorded one. Pure so tests can
/// assert the list without a terminal.
pub(crate) fn drawer_list_lines(
    agents: &[AgentRun],
    members: &[usize],
    selected: usize,
    focused: bool,
    width: usize,
) -> Vec<Line<'static>> {
    members
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| {
            let run = agents.get(index)?;
            let is_selected = position == selected;
            let label = run.node_id.clone().unwrap_or_else(|| "·".to_string());
            let title = if run.task_summary.trim().is_empty() {
                summarize_task(&run.task)
            } else {
                run.task_summary.clone()
            };
            // The label column is fixed so the titles line up into a readable column.
            let label_width = 4usize;
            // marker(2) + badge(2) + label + gap(2) + status label.
            let status_text = agent_status_text(run);
            let text_width = width
                .saturating_sub(label_width + 6 + status_text.chars().count())
                .max(8);
            Some(Line::from(vec![
                Span::styled(
                    if is_selected { "▶ " } else { "  " },
                    if is_selected {
                        accent_style(focused)
                    } else {
                        muted_style(focused)
                    },
                ),
                // The status badge keeps state legible at a glance inside the drawer,
                // the way it is on a sidebar row: merged green, failed red. Collapsing
                // the section must not cost the color coding.
                Span::styled(BADGE, status_style(run.status)),
                Span::raw(" "),
                Span::styled(
                    format!("{label:<label_width$}"),
                    if is_selected {
                        accent_style(focused)
                    } else {
                        muted_style(focused)
                    },
                ),
                Span::styled(truncate_chars(&title, text_width), pane_text_style(focused)),
                Span::raw("  "),
                Span::styled(status_text, status_style(run.status)),
            ]))
        })
        .collect()
}

/// The drawer view: the finished runs on top, the highlighted run's result card
/// below. Mirrors `render_done_worker_card`'s split so the two read the same, but the
/// top half is the LIST rather than one run's card.
pub(crate) fn render_drawer(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let focused = app.focus == FocusPane::Worker || app.focus == FocusPane::Agents;
    let Some(bucket) = app.drawer_open() else {
        return;
    };
    let members = drawer_members(&app.agents, bucket);
    let selected = app.drawer_selection().min(members.len().saturating_sub(1));
    let (list_area, detail_area) = done_card_areas(area);

    let list_inner = block_inner(list_area);
    let mut lines = drawer_list_lines(
        &app.agents,
        &members,
        selected,
        focused,
        list_inner.width as usize,
    );
    // Keep the highlighted row on screen when the drawer holds more runs than fit.
    let view_h = list_inner.height as usize;
    if view_h > 0 && lines.len() > view_h {
        let start = selected.saturating_sub(view_h.saturating_sub(1));
        lines = lines.into_iter().skip(start).take(view_h).collect();
    }
    frame.render_widget(
        Paragraph::new(lines).style(app_style()).block(pane_block(
            &format!("{} · {}", bucket.label(), members.len()),
            focused,
            app.nav_mode,
        )),
        list_area,
    );

    let (title, detail) = match members
        .get(selected)
        .and_then(|&index| app.agents.get(index))
    {
        Some(run) => {
            let label = run.node_id.clone().unwrap_or_else(|| "run".to_string());
            let summary = if run.task_summary.trim().is_empty() {
                summarize_task(&run.task)
            } else {
                run.task_summary.clone()
            };
            (
                format!("{label} · {summary}"),
                done_worker_card_lines(run, focused),
            )
        }
        None => (
            format!("{} · nothing here", bucket.label()),
            vec![Line::from(Span::styled(
                "This drawer is empty.",
                muted_style(focused),
            ))],
        ),
    };
    frame.render_widget(
        Paragraph::new(detail)
            .style(app_style())
            .block(pane_block(&title, focused, app.nav_mode))
            .wrap(Wrap { trim: false }),
        detail_area,
    );
}

pub(crate) fn render_worker(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = block_inner(area);
    let terminal_size = TerminalSize::new(inner.height.max(1), inner.width.max(1)).ok();
    let focused = app.focus == FocusPane::Worker;

    // An open drawer owns this pane: the finished runs it holds, then the highlighted
    // one's result. It replaces the terminal view because a merged or failed run has
    // nothing live to show, and the point of collapsing them was to read the results.
    if app.drawer_open().is_some() {
        render_drawer(frame, area, app);
        return;
    }

    // The selected orchestrator gets the custom DAG command-center view ONLY once
    // its plan is ready. While it is still planning, show its raw PTY so the live
    // plan-mode session (clarifying questions, selection menus, approval) renders
    // cleanly through the terminal emulator and fills the pane. A custom extracted
    // -lines view mangled spacing, leaked OSC escape sequences, and broke menu
    // navigation. The diff/review view always uses the normal terminal path.
    let selected_is_orchestrator = app
        .agents
        .get(app.selected_agent)
        .is_some_and(|run| run.is_orchestrator());
    if app.worker_view == WorkerView::PlanReview {
        app.ensure_plan_review_state();
    }
    // The orchestrator ALWAYS gets the custom command-center view: a live streaming
    // transcript while planning (its raw PTY is now JSON events, not human text), and
    // the DAG tree once a plan is ready. Both phases are rendered by render_orchestrator.
    if app.worker_view == WorkerView::Terminal && selected_is_orchestrator {
        let selected_is_interactive_orchestrator = app
            .agents
            .get(app.selected_agent)
            .is_some_and(|run| app.is_interactive_orchestrator_run(run));
        if selected_is_interactive_orchestrator {
            // Opt-in: the orchestrator is a normal interactive backend PTY the user talks
            // to, with the DAG it wrote to its plan file rendered in a pane ABOVE it.
            render_interactive_orchestrator(frame, area, app);
        } else {
            render_orchestrator(frame, area, app);
        }
        return;
    }

    // A finished worker renders as its plain conversation. It used to get a
    // two-panel card (objective + what-it-did on top, conversation below), but the
    // card restated what the transcript already said and took half the pane to do
    // it. The same summary is still one keypress away in the done drawer.

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
        WorkerView::PlanReview => {
            plan_review_lines(app, inner.height as usize, inner.width as usize)
        }
    };
    // Identify WHICH agent this pane is showing: its id (the node id for plan
    // workers, matching the n0/n1 ids in the orchestrator DAG) plus a short task
    // label. Without this the pane just said "worker" and the user had to infer
    // the mapping from the agents-pane highlight.
    let identity = app.agents.get(app.selected_agent).map(|run| {
        let id = worker_title_id(app, run);
        let label = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        format!("{id} · {}", truncate_chars(&label, 36))
    });
    let title = match app.worker_view {
        WorkerView::Terminal => {
            let role = if selected_is_orchestrator {
                "orchestrator"
            } else {
                "worker"
            };
            match &identity {
                Some(identity) if !selected_is_orchestrator => format!("{role} · {identity}"),
                _ => role.to_string(),
            }
        }
        WorkerView::Diff => match &identity {
            Some(identity) => format!("review · {identity} · Esc/v back · m merge"),
            None => "review · Esc/v back · m merge".to_string(),
        },
        WorkerView::PlanReview => format!(
            "plan review · {} · Ctrl+S save · Ctrl+Enter approve · Esc hide",
            app.plan().plan_review.field.label()
        ),
    };
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(pane_block(&title, focused, app.nav_mode))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);

    if focused {
        match app.worker_view {
            WorkerView::Terminal => set_worker_cursor(frame, inner, app),
            WorkerView::Diff => set_review_cursor(frame, inner, app),
            WorkerView::PlanReview => set_plan_review_cursor(frame, inner, app),
        }
    }
}

fn worker_title_id(app: &App, run: &AgentRun) -> String {
    if let Some(node_id) = run.node_id.as_ref().filter(|id| !id.trim().is_empty()) {
        return node_id.clone();
    }
    if run.is_oneoff() {
        return format!("q{}", oneoff_display_index(app, &run.id).unwrap_or(1));
    }
    if run.is_main() {
        return "main".to_string();
    }
    run.id.clone()
}

fn oneoff_display_index(app: &App, run_id: &str) -> Option<usize> {
    app.agents
        .iter()
        .filter(|agent| agent.is_oneoff())
        .position(|agent| agent.id == run_id)
        .map(|index| index + 1)
}

pub(crate) fn plan_review_lines(app: &mut App, height: usize, width: usize) -> Vec<Line<'static>> {
    app.plan_mut().plan_review.cursor_row = None;
    app.plan_mut().plan_review.cursor_col = 0;
    let focused = app.focus == FocusPane::Worker;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let content_width = width.max(1);
    if app.plan().plan_review.nodes.is_empty() {
        return vec![Line::from(Span::styled(
            "No approval-gated plan is available.",
            muted_style(focused),
        ))];
    }

    lines.push(Line::from(vec![
        Span::styled("Plan ready", accent_style(focused)),
        Span::styled("  edit fields inline before launch", muted_style(focused)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Tab", accent_style(focused)),
        Span::styled(" field  ", muted_style(focused)),
        Span::styled("\u{2191}/\u{2193}", accent_style(focused)),
        Span::styled(" task  ", muted_style(focused)),
        Span::styled("Ctrl+S", accent_style(focused)),
        Span::styled(" save  ", muted_style(focused)),
        Span::styled("Ctrl+Enter", accent_style(focused)),
        Span::styled(" approve  ", muted_style(focused)),
        Span::styled("just type", accent_style(focused)),
        Span::styled(" to refine with the planner", muted_style(focused)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Tasks", muted_style(focused))));
    for (index, node) in app.plan().plan_review.nodes.iter().enumerate() {
        let selected = index == app.plan().plan_review.selected;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default().fg(PAPER).bg(FOCUS_COLOR)
        } else {
            pane_text_style(focused)
        };
        let dep_count = plan_review_dep_count(node);
        let mut spans = vec![
            Span::styled(marker, accent_style(focused)),
            Span::styled(format!("{} ", node.id), accent_style(focused)),
            Span::styled(
                truncate_chars(&node.title, content_width.saturating_sub(12)),
                style,
            ),
        ];
        if dep_count > 0 {
            spans.push(Span::styled(
                format!("  deps {dep_count}"),
                muted_style(focused),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    let selected = app.plan().plan_review.selected;
    if let Some(node) = app.plan().plan_review.nodes.get(selected).cloned() {
        lines.push(Line::from(vec![
            Span::styled(format!("Editing {} ", node.id), accent_style(focused)),
            Span::styled(
                format!("({}/{})", selected + 1, app.plan().plan_review.nodes.len()),
                muted_style(focused),
            ),
        ]));
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Title,
            "title",
            &node.title,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Goal,
            "goal",
            &node.goal,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Success,
            "done",
            &node.success,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::HardDeps,
            "hard deps",
            &node.hard_deps,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::SoftDeps,
            "soft deps",
            &node.soft_deps,
            content_width,
            focused,
        );
        push_plan_review_field(
            app,
            &mut lines,
            PlanReviewField::Prompt,
            "prompt",
            &node.prompt,
            content_width,
            focused,
        );
    }

    if !app.plan().plan_review.errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Errors", error_style())));
        for error in &app.plan().plan_review.errors {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                error_style(),
            )));
        }
    }
    if !app.plan().plan_review.warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Warnings",
            Style::default().fg(ACCENT_2),
        )));
        for warning in &app.plan().plan_review.warnings {
            lines.push(Line::from(Span::styled(
                format!("  {warning}"),
                Style::default().fg(ACCENT_2),
            )));
        }
    }

    let max_scroll = lines.len().saturating_sub(height.max(1));
    app.plan_mut().plan_review.scroll = app.plan().plan_review.scroll.min(max_scroll);
    lines
        .into_iter()
        .skip(app.plan().plan_review.scroll)
        .take(height.max(1))
        .collect()
}

fn plan_review_dep_count(node: &PlanReviewDraftNode) -> usize {
    App::parse_plan_review_deps(&node.hard_deps).len()
        + App::parse_plan_review_deps(&node.soft_deps).len()
}

fn push_plan_review_field(
    app: &mut App,
    lines: &mut Vec<Line<'static>>,
    field: PlanReviewField,
    label: &str,
    value: &str,
    width: usize,
    focused: bool,
) {
    let active = app.plan().plan_review.field == field;
    let label_text = format!("  {label}: ");
    let label_width = label_text.chars().count();
    let value_width = width.saturating_sub(label_width).max(1) as u16;
    let wrapped = wrap_input_text(value, value_width);
    if active {
        let (cursor_line, cursor_col) =
            task_cursor_position(value, app.plan().plan_review.cursor, value_width);
        app.plan_mut().plan_review.cursor_row = Some(lines.len() + cursor_line);
        app.plan_mut().plan_review.cursor_col = label_width + cursor_col;
    }
    let label_style = if active {
        accent_style(focused)
    } else {
        muted_style(focused)
    };
    let value_style = if active {
        Style::default().fg(INK).bg(SURFACE_SEL)
    } else {
        pane_text_style(focused)
    };
    for (index, line) in wrapped.iter().enumerate() {
        let prefix = if index == 0 {
            label_text.clone()
        } else {
            " ".repeat(label_width)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, label_style),
            Span::styled(line.clone(), value_style),
        ]));
    }
}

pub(crate) fn set_plan_review_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    let Some(row) = app.plan().plan_review.cursor_row else {
        return;
    };
    if row < app.plan().plan_review.scroll {
        return;
    }
    let visible_row = row - app.plan().plan_review.scroll;
    if visible_row >= inner.height as usize {
        return;
    }
    let col = app
        .plan()
        .plan_review
        .cursor_col
        .min(inner.width.saturating_sub(1) as usize);
    frame.set_cursor_position((inner.x + col as u16, inner.y + visible_row as u16));
}

pub(crate) fn set_worker_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
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

pub(crate) fn set_review_cursor(frame: &mut Frame<'_>, inner: Rect, app: &App) {
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

pub(crate) fn worker_lines(app: &mut App, height: usize, width: usize) -> Vec<Line<'static>> {
    let perf_start = Instant::now();
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
    let run_id = run.id.clone();
    let Some(terminal) = run.terminal.as_mut() else {
        let title = if run.task_summary.trim().is_empty() {
            summarize_task(&run.task)
        } else {
            run.task_summary.clone()
        };
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{}  {}", run.status.as_str(), title),
                pane_text_style(true),
            )),
            Line::from(""),
        ];
        // The FULL objective/prompt, one Line per source line. The surrounding
        // Paragraph wraps long lines to the pane width, so a completed agent's
        // objective is fully readable here (it used to be short_task's 26 chars,
        // which made finished work unreviewable after its terminal was gone).
        for text_line in run.task.lines() {
            lines.push(Line::from(Span::styled(
                text_line.to_string(),
                pane_text_style(true),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if matches!(run.mode, AgentMode::Plan | AgentMode::RudderPlan) {
                "This read-only planner is not running."
            } else {
                "This agent is not running."
            },
            muted_style(true),
        )));
        lines.push(Line::from(Span::styled(
            run.cwd.display().to_string(),
            muted_style(true),
        )));
        return lines;
    };

    let selection = app
        .worker_selection
        .map(normalize_selection)
        .filter(|selection| !selection_is_empty(*selection));
    let (start_row, styled_rows) = terminal.styled_line_window_snapshot(height);
    let cursor = worker_render_cursor(backend, terminal, focused, height, width, start_row);
    let row_count = styled_rows.len();
    let lines = styled_rows
        .into_iter()
        .enumerate()
        .map(|(offset, cells)| {
            let row = start_row + offset;
            styled_terminal_line(
                cells,
                selection_for_row(selection, row),
                cursor
                    .filter(|cursor| cursor.row as usize == row)
                    .map(|cursor| cursor.col as usize),
            )
        })
        .collect::<Vec<_>>();
    let duration = perf_start.elapsed();
    app.record_perf_duration("worker_lines", duration);
    app.log_perf_duration_over(
        "worker_lines",
        duration,
        SLOW_LINE_RENDER_THRESHOLD,
        serde_json::json!({
            "run_id": run_id,
            "rows": row_count,
            "height": height,
            "width": width,
            "start_row": start_row,
        }),
    );
    lines
}

pub(crate) fn worker_render_cursor(
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

pub(crate) fn force_worker_cursor(backend: Backend) -> bool {
    matches!(backend, Backend::Claude | Backend::Codex)
}

pub(crate) fn review_lines(app: &mut App, height: usize) -> Vec<Line<'static>> {
    let perf_start = Instant::now();
    let Some(run) = app.agents.get_mut(app.selected_agent) else {
        return vec![Line::from(Span::styled(
            "No agent selected.",
            muted_style(true),
        ))];
    };

    if let Some(error) = &run.review_error {
        return vec![
            Line::from(Span::styled("diff failed", error_style())),
            Line::from(Span::styled(error.clone(), error_style())),
            Line::from(""),
            Line::from(Span::styled(
                "Press Ctrl-G then v to return to the worker.",
                muted_style(true),
            )),
        ];
    }

    let run_id = run.id.clone();
    let Some(review) = run.review_terminal.as_mut() else {
        return vec![
            Line::from(Span::styled("Opening jj diff...", muted_style(true))),
            Line::from(""),
            Line::from(Span::styled(
                "Live `jj diff` of this agent's workspace.",
                pane_text_style(true),
            )),
        ];
    };

    let (_start_row, styled_rows) = review.styled_line_window_snapshot(height);
    let row_count = styled_rows.len();
    let lines = styled_rows
        .into_iter()
        .map(|cells| styled_terminal_line(cells, None, None))
        .collect::<Vec<_>>();
    let duration = perf_start.elapsed();
    app.record_perf_duration("review_lines", duration);
    app.log_perf_duration_over(
        "review_lines",
        duration,
        SLOW_LINE_RENDER_THRESHOLD,
        serde_json::json!({
            "run_id": run_id,
            "rows": row_count,
            "height": height,
        }),
    );
    lines
}

/// The default task-pane hint shown when there is no transient notice. Centralised
/// so render_task, task_pane_height, and selection's height/hit-test calc all wrap
/// the SAME string: if these drift, the pane height and mouse selection bounds
/// disagree and clicks on valid input rows get rejected.
pub(crate) fn task_default_hint(app: &App) -> &'static str {
    if app.plan().planner_paused_for_input {
        "↳ the planner asked a question — answer here or in the orchestrator pane"
    } else if app.plan().awaiting_approval {
        "type to talk to the orchestrator, or press Enter to approve  ·  Option-1/2/3 or ^W pane"
    } else if app.plan_is_active() {
        "type to talk to the live orchestrator; it can add, re-plan, merge, stop, or re-goal workers  ·  ^W pane"
    } else if app.agents.iter().any(|run| run.node_id.is_some())
        && app.has_running_interactive_orchestrator()
    {
        "type to talk to the live orchestrator for status, retro, or follow-up control  ·  ^W pane"
    } else {
        "type for one isolated mergeable worker; /plan for a DAG; /main for this checkout  ·  Option-1/2/3 or ^W pane"
    }
}

/// Style for the transient notice line, by severity. Errors and pending
/// confirmations must not read like routine status: the notice is the only
/// surface many failures have, and an all-muted line buried them. Keyed off the
/// message text so the ~60 notice call sites need no extra state to maintain.
pub(crate) fn notice_style(text: &str, focused: bool) -> Style {
    const ERROR_MARKERS: &[&str] = &[
        "failed",
        "error",
        "conflict",
        "stopped",
        "timed out",
        "not found",
        "not logged in",
        "no longer exists",
        "cannot",
        "disabled",
    ];
    const CONFIRM_MARKERS: &[&str] = &["press d again", "press q / ctrl+c again", "still running"];
    let lower = text.to_ascii_lowercase();
    if CONFIRM_MARKERS.iter().any(|marker| lower.contains(marker)) {
        Style::default().fg(RUNNING_COLOR)
    } else if ERROR_MARKERS.iter().any(|marker| lower.contains(marker)) {
        Style::default().fg(FAILED_COLOR)
    } else {
        muted_style(focused)
    }
}

/// `task · claude opus(high)` — the backend, model and effort a new agent gets.
/// `/fast` is called out because it changes behaviour without changing the model.
pub(crate) fn task_pane_title(app: &App) -> String {
    let model = app.model.trim();
    if model.is_empty() {
        // opencode's legitimate "use your configured default".
        return format!("task · {}", app.backend.as_str());
    }
    let fast = if app.fast_mode { " · fast" } else { "" };
    format!(
        "task · {} {model}({}){fast}",
        app.backend.as_str(),
        effort_label(app.effort)
    )
}

pub(crate) fn render_task(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPane::Task;
    let default_hint = task_default_hint(app);
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
                if app.plan().awaiting_approval {
                    "Type to refine the plan, or press Enter to approve"
                } else {
                    "Type a task to plan and run"
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
        let style = notice_style(hint, focused);
        for line in wrapped_hint {
            lines.push(Line::from(Span::styled(line, style)));
        }
    } else {
        let first_hint = wrapped_hint.first().cloned().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(first_hint, muted_style(focused)),
            Span::raw("  "),
            Span::styled(
                if app.plan().awaiting_approval {
                    "refine"
                } else {
                    "run"
                },
                accent_style(focused),
            ),
            // The model used to be repeated here; it lives in the pane title now,
            // where a notice cannot hide it.
        ]));
        for line in wrapped_hint.into_iter().skip(1) {
            lines.push(Line::from(Span::styled(line, muted_style(focused))));
        }
    }

    // The model belongs in the TITLE, not only on the hint line: the hint is
    // replaced by every notice, so the one place that answers "what will this run
    // on?" kept disappearing exactly when the user was acting.
    let paragraph = Paragraph::new(lines).style(app_style()).block(pane_block(
        &task_pane_title(app),
        focused,
        app.nav_mode,
    ));

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

pub(crate) fn task_pane_height(app: &App, width: u16) -> u16 {
    let default_hint = task_default_hint(app);
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

pub(crate) fn render_suggestions(frame: &mut Frame<'_>, task_area: Rect, app: &App) {
    let suggestions = suggestions_for(app);
    if suggestions.is_empty() {
        return;
    }

    let visible_count = suggestions
        .len()
        .min(task_area.y.saturating_sub(2).min(10) as usize);
    if visible_count == 0 {
        return;
    }
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
    let max_label_width = area.width.saturating_sub(8).min(32) as usize;
    let label_width = suggestions
        .iter()
        .skip(offset)
        .take(visible_count)
        .map(|suggestion| suggestion.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(max_label_width);
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
            let label = truncate_chars(&suggestion.label, label_width);
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{label:<label_width$}"), style),
                Span::raw("  "),
                Span::styled(suggestion.detail.clone(), muted_style(true)),
            ]))
        })
        .collect::<Vec<_>>();

    let title = if app.task_input.starts_with("/model") {
        model_picker_title(
            &app.task_input,
            selected_index.saturating_add(1),
            suggestions.len(),
        )
    } else {
        format!(
            " commands · {}/{} ",
            selected_index.saturating_add(1),
            suggestions.len()
        )
    };
    let title = truncate_chars(&title, area.width.saturating_sub(4) as usize);
    let list = List::new(items).style(app_style()).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FOCUS_COLOR))
            .style(app_style()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(list, area);
}

pub(crate) fn model_picker_title(input: &str, selected: usize, total: usize) -> String {
    let rest = input
        .trim_start()
        .strip_prefix("/model")
        .unwrap_or_default();
    let trailing_space = rest.ends_with(char::is_whitespace);
    let parts = rest.split_whitespace().collect::<Vec<_>>();
    let step = match parts.as_slice() {
        [] => "provider".to_string(),
        [provider] if !trailing_space => format!("provider · {provider}"),
        [provider] => format!("{provider} · model"),
        [provider, model] if !trailing_space => format!("{provider} · model · {model}"),
        [provider, model] => format!("{provider} · {model} · effort"),
        [provider, model, ..] => format!("{provider} · {model} · effort"),
    };
    format!(" model · {step} · {selected}/{total} ")
}

pub(crate) fn render_merge_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (title, lines, border_color): (Span<'static>, Vec<Line<'static>>, Color) = if let Some(
        confirm,
    ) =
        &app.merge_confirm
    {
        let publishing = matches!(confirm.intent, MergeIntent::Publish { .. });
        let headline = match &confirm.intent {
            MergeIntent::Selected { task, .. } => format!("Merge:  {}", short_task(task)),
            MergeIntent::Publish { task, .. } => format!("Publish:  {}", short_task(task)),
            MergeIntent::All { ids } => format!(
                "Merge {} completed workspace{}",
                ids.len(),
                if ids.len() == 1 { "" } else { "s" }
            ),
        };
        // The ACTION comes first. This modal used to explain itself for three
        // wrapped rows and mention the keys only on the last one, which is also the
        // row that got clipped — so the question "which key confirms?" had no
        // answer on screen.
        let mut lines = vec![
            Line::from(Span::styled(headline, app_style())),
            Line::from(""),
            key_choice_line(&[
                (
                    "y",
                    if publishing {
                        "push and open PR"
                    } else {
                        "merge"
                    },
                ),
                ("Esc", "cancel"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                if publishing {
                    // The detail line below names the remote and branch. This one
                    // says what the answer commits the repo to, because the prompt
                    // is asked exactly once per repo.
                    "The PR opens as a draft. Rudder will not ask again for this repo; from here on m publishes instead of merging locally."
                } else {
                    "Dependent nodes unblock once it lands. A clean merge is mechanical (no AI); on conflict you can launch an AI resolver or resolve it yourself."
                },
                muted_style(true),
            )),
        ];
        if let Some(detail) = &confirm.detail {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                detail.clone(),
                Style::default().fg(RUNNING_COLOR),
            )));
        }

        (
            Span::styled(
                if publishing {
                    " First publish · y pushes and opens a draft PR · Esc cancels "
                } else {
                    " Confirm merge · y merges · Esc cancels "
                },
                Style::default().fg(ACCENT),
            ),
            lines,
            ACCENT,
        )
    } else if let Some(prompt) = &app.conflict_prompt {
        let operation_label = if prompt.operation == ConflictOperation::Rebase {
            "Rebase"
        } else {
            "Merge"
        };
        let count = prompt.conflicted_files.len();
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    "{operation_label} stopped on {count} conflicted file{}.",
                    if count == 1 { "" } else { "s" }
                ),
                Style::default().fg(FAILED_COLOR),
            )),
            Line::from(""),
        ];
        lines.push(key_choice_line(&[
            ("y", "AI resolver"),
            ("n", "resolve it yourself"),
            ("Esc", "cancel"),
        ]));
        lines.push(Line::from(""));
        if prompt.conflicted_files.is_empty() {
            lines.push(Line::from(Span::styled(
                "No specific files were reported.",
                muted_style(true),
            )));
        } else {
            // Cap the list so a wide conflict cannot push the key hints out of
            // the fixed-height modal.
            const MAX_CONFLICT_ROWS: usize = 8;
            for file in prompt.conflicted_files.iter().take(MAX_CONFLICT_ROWS) {
                lines.push(Line::from(vec![
                    Span::styled("   • ", muted_style(true)),
                    Span::styled(file.clone(), app_style()),
                ]));
            }
            if count > MAX_CONFLICT_ROWS {
                lines.push(Line::from(Span::styled(
                    format!("   … and {} more", count - MAX_CONFLICT_ROWS),
                    muted_style(true),
                )));
            }
        }

        (
            Span::styled(
                if prompt.operation == ConflictOperation::Rebase {
                    " Rebase conflict "
                } else {
                    " Merge conflict "
                },
                Style::default().fg(FAILED_COLOR),
            ),
            lines,
            FAILED_COLOR,
        )
    } else {
        return;
    };

    // Height must count WRAPPED rows, not Line structs. The body holds a long
    // explanatory sentence and, when the checkout is dirty, a warning — each one
    // Line that renders as three or four. Sizing by `lines.len()` clipped the
    // bottom of the modal, and the bottom is exactly where the key hint lives, so
    // the dialogue asked for confirmation without ever saying which key confirms.
    let modal_width = 76;
    let inner_width = modal_width
        .min(area.width.saturating_sub(4))
        .max(24)
        .saturating_sub(2);
    let modal = centered_modal(
        area,
        modal_width,
        wrapped_line_count(&lines, inner_width).saturating_add(2),
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(app_style());
    let paragraph = Paragraph::new(lines)
        .style(app_style())
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, modal);
    frame.render_widget(paragraph, modal);
}

pub(crate) fn render_cloud_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
            accent_style(true)
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
        Line::from(""),
        // Keys accent-colored like every other modal hint (emphasis by color,
        // not weight), so the available actions read at a glance.
        Line::from(vec![
            Span::styled("Up/Down", accent_style(true)),
            Span::styled(" choose  ·  ", muted_style(true)),
            Span::styled("Enter", accent_style(true)),
            Span::styled(" start  ·  ", muted_style(true)),
            Span::styled("Esc", accent_style(true)),
            Span::styled(" cancel", muted_style(true)),
        ]),
    ];
    // Same wrapping rule as the merge modal: size by rendered rows, or the last
    // line (which carries the keys) falls off the bottom.
    let modal_width = 78;
    let inner_width = modal_width
        .min(area.width.saturating_sub(4))
        .max(24)
        .saturating_sub(2);
    let modal = centered_modal(
        area,
        modal_width,
        wrapped_line_count(&lines, inner_width).saturating_add(2),
    );
    let block = Block::default()
        .title(" cloud launch ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLOUD_COLOR))
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

/// The one row a confirm dialogue must never bury: `[y] merge   [Esc] cancel`,
/// keys bracketed and in the accent colour so they read as buttons rather than
/// prose. Rendered directly under the headline, before any explanation.
pub(crate) fn key_choice_line(choices: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (index, (key, label)) in choices.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("     "));
        }
        spans.push(Span::styled("[", muted_style(true)));
        spans.push(Span::styled((*key).to_string(), accent_style(true)));
        spans.push(Span::styled("] ", muted_style(true)));
        spans.push(Span::styled((*label).to_string(), app_style()));
    }
    Line::from(spans)
}

/// How many terminal rows these lines occupy once wrapped to `width` — what a
/// modal must be sized by, since `Paragraph::wrap` reflows every Line. Sizing by
/// `lines.len()` clipped the bottom of the box, and the bottom is where a
/// dialogue's keys used to live.
pub(crate) fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> u16 {
    lines
        .iter()
        .map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .join("");
            if text.trim().is_empty() {
                1
            } else {
                wrap_text(&text, width).len().max(1) as u16
            }
        })
        .sum()
}

pub(crate) fn centered_modal(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(4)).max(24);
    let height = desired_height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

pub(crate) fn pane_block(title: &str, focused: bool, nav_mode: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(FOCUS_COLOR)
    } else {
        Style::default().fg(INACTIVE_COLOR)
    };

    // Focused: white text on a teal title fill. Unfocused: a quiet label with
    // no forced background in terminal-background mode.
    let title_style = if focused {
        Style::default().fg(PAPER).bg(FOCUS_COLOR)
    } else if terminal_background_mode() {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(MUTED).bg(PAPER)
    };
    let _ = nav_mode;

    // Carry the configured base style so pane interiors are consistent with the
    // selected color mode.
    Block::default()
        .title(Line::from(Span::styled(format!(" {title} "), title_style)))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(app_style())
}

pub(crate) fn task_input_lines(value: &str, cursor: usize, width: u16) -> Vec<String> {
    let mut lines = wrap_input_text(value, width);
    let (cursor_line, _) = task_cursor_position(value, cursor, width);
    while lines.len() <= cursor_line {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn wrap_input_text(value: &str, width: u16) -> Vec<String> {
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

pub(crate) fn task_cursor_position(value: &str, cursor: usize, width: u16) -> (usize, usize) {
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

pub(crate) fn wrap_text(value: &str, width: u16) -> Vec<String> {
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

pub(crate) fn push_wrapped_word(
    lines: &mut Vec<String>,
    current: &mut String,
    word: &str,
    max_width: usize,
) {
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

pub(crate) fn pane_text_style(focused: bool) -> Style {
    if focused {
        app_style()
    } else {
        Style::default().fg(MUTED)
    }
}

pub(crate) fn muted_style(focused: bool) -> Style {
    Style::default().fg(if focused { MUTED } else { FAINT })
}

pub(crate) fn accent_style(focused: bool) -> Style {
    Style::default().fg(if focused { FOCUS_COLOR } else { FAINT })
}

pub(crate) fn model_style(focused: bool) -> Style {
    Style::default().fg(if focused { MODEL_COLOR } else { FAINT })
}

pub(crate) fn cloud_style(connected: bool, focused: bool) -> Style {
    let color = if connected { CLOUD_COLOR } else { FAINT };
    let _ = focused;
    Style::default().fg(color)
}

/// Base style for the whole UI. Terminal mode intentionally sets neither fg nor
/// bg so the user's terminal theme shows through; paper mode restores the old
/// ink-on-white canvas.
pub(crate) fn app_style() -> Style {
    if terminal_background_mode() {
        Style::default()
    } else {
        Style::default().fg(INK).bg(PAPER)
    }
}

pub(crate) fn error_style() -> Style {
    Style::default().fg(FAILED_COLOR)
}

pub(crate) fn block_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

pub(crate) fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

pub(crate) fn is_scroll_mouse_event(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

/// On-screen rows a single logical line occupies once wrapped to `width` columns.
/// A blank or sub-column line still takes one row.
pub(crate) fn wrapped_row_count(line_width: usize, width: usize) -> usize {
    line_width.div_ceil(width.max(1)).max(1)
}

/// Largest valid scroll offset (lines from the top) for a list of `content_rows`
/// drawn in a `visible_height`-row viewport. Zero when everything fits.
pub(crate) fn orchestrator_dag_max_scroll(content_rows: usize, visible_height: usize) -> usize {
    content_rows.saturating_sub(visible_height)
}

pub(crate) fn orchestrator_scroll_offset(
    scroll: &mut usize,
    max_scroll: usize,
    follow_bottom: bool,
) -> usize {
    if follow_bottom {
        *scroll = max_scroll;
    } else {
        *scroll = (*scroll).min(max_scroll);
    }
    *scroll
}

pub(crate) fn mouse_scrollback_delta(mouse: MouseEvent, viewport_height: u16) -> isize {
    let rows = wheel_scroll_rows(viewport_height, mouse.modifiers) as isize;
    match mouse.kind {
        MouseEventKind::ScrollUp => rows,
        MouseEventKind::ScrollDown => -rows,
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => 0,
        _ => 0,
    }
}

pub(crate) fn wheel_scroll_rows(viewport_height: u16, modifiers: KeyModifiers) -> u16 {
    let page = viewport_height.saturating_sub(1).max(1);
    if modifiers.intersects(
        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::META | KeyModifiers::SUPER,
    ) {
        return page;
    }

    wheel_scroll_rows_setting().min(page).max(1)
}

pub(crate) fn wheel_scroll_rows_setting() -> u16 {
    env::var("RUDDER_WHEEL_SCROLL_ROWS")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_WHEEL_SCROLL_ROWS)
}

pub(crate) fn page_scroll_rows(area: Option<Rect>) -> isize {
    let height = area.map(block_inner).map(|inner| inner.height).unwrap_or(1);
    height.saturating_sub(1).max(1) as isize
}

/// Scrollback rows for an Alt-modified key in the worker pane, or None when
/// the key is not an alt-scroll binding. Vim grammar, held behind Alt so the
/// bare letters still reach the agent when the pane is focused:
///   Alt+k / Alt+Up   → one line up        Alt+j / Alt+Down → one line down
///   Alt+u            → half a page up     Alt+d            → half a page down
/// Positive rows scroll toward history (up), matching scrollback_by.
pub(crate) fn alt_scroll_rows(key: KeyEvent, area: Option<Rect>) -> Option<isize> {
    if !key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META)
    {
        return None;
    }
    let half = (page_scroll_rows(area) / 2).max(1);
    match key.code {
        KeyCode::Char('k') | KeyCode::Up => Some(1),
        KeyCode::Char('j') | KeyCode::Down => Some(-1),
        KeyCode::Char('u') => Some(half),
        KeyCode::Char('d') => Some(-half),
        _ => None,
    }
}

pub(crate) fn status_style(status: AgentStatus) -> Style {
    Style::default().fg(status_color(status))
}

/// The status a row actually shows. A published row's PR outranks every local
/// label: once the work is a pull request, "done · press m to read the diff" is a
/// stale instruction and `PR #123 · draft` is where the work now lives.
pub(crate) fn agent_status_text(agent: &AgentRun) -> String {
    if let Some(label) = agent.publish.label() {
        return label;
    }
    let label = agent_status_label(agent).to_string();
    // A row that says "working" says exactly the same thing whether the agent
    // is mid-thought or has been parked for a day and a half. In ~/code/battery
    // an orchestrator sat at "running" for 32 HOURS with a live-but-idle
    // process and nothing on screen distinguished it from real work. Once a
    // running agent has been silent well past any normal pause, say how long.
    // (The clock restarts when a dashboard reloads a row, so this reports
    // silence within the current session, never a fabricated longer one.)
    if agent.status == AgentStatus::Running && !agent.needs_permission && !agent.needs_user_input {
        let quiet = agent.last_output_at.elapsed();
        if quiet >= QUIET_AGENT_THRESHOLD {
            return format!("{label} · quiet {}", humanize_quiet(quiet));
        }
    }
    label
}

/// How long a running agent may go without output before the row says so.
/// Long enough that a thinking model or a slow build never trips it.
const QUIET_AGENT_THRESHOLD: Duration = Duration::from_secs(15 * 60);

fn humanize_quiet(quiet: Duration) -> String {
    let minutes = quiet.as_secs() / 60;
    if minutes < 90 {
        format!("{minutes}m")
    } else {
        let hours = minutes / 60;
        if hours < 48 {
            format!("{hours}h")
        } else {
            format!("{}d", hours / 24)
        }
    }
}

pub(crate) fn agent_status_label(agent: &AgentRun) -> &'static str {
    // Evidence first. A row that claims "merged" while jj says the change is no
    // longer in trunk is the most misleading thing the pane can show — the work
    // looks landed and is not. Same for a row still pointing at a workspace that
    // has been swept: every key you press against it will fail.
    if agent.status == AgentStatus::Merged && agent.integration.in_trunk == Some(false) {
        return "merged · NOT in main";
    }
    if agent.needs_permission {
        "needs permission"
    } else if agent.needs_user_input {
        "needs input"
    } else if matches!(agent.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && agent.status == AgentStatus::Running
    {
        "plan mode"
    } else if agent.status == AgentStatus::Merged
        && agent.delivery.required
        && !agent.delivery.is_verified()
    {
        // A "deploy/publish" node auto-merges within a tick, and the plain
        // "merged locally" label silently swallowed its unmet delivery proof.
        "merged · delivery proof needed"
    } else if agent.status == AgentStatus::Merged && agent.integration.pushed {
        "pushed"
    } else if agent.status == AgentStatus::Merged {
        "merged locally"
    } else if agent.integration.phase == IntegrationPhase::Integrating {
        "integrating"
    } else if agent.has_merge_conflict() && agent.status == AgentStatus::Running {
        "resolving conflict"
    } else if agent.has_merge_conflict() && agent.status == AgentStatus::Done {
        "merge conflict · press m"
    } else if agent.status == AgentStatus::Done && agent.delivery.required {
        // A deploy/publish task's delivery state (deployed / delivery blocked /
        // delivery proof needed) outranks the generic merge hint — these labels
        // were unreachable for workspace runs because the awaits-merge branch
        // below always won.
        agent.lifecycle_label()
    } else if agent.status == AgentStatus::Done && agent_awaits_merge(agent) {
        // Two different futures, one bucket: planned DAG nodes integrate on
        // their own; everything else waits for the user. The old label
        // ("verifying · ready to integrate") claimed active work that wasn't
        // happening and never said WHO merges next.
        if agent.node_id.is_some() {
            "done · auto-merging"
        } else if agent.reviewed_at.is_some() {
            // The gate is only visible if the two sides look different. Without this
            // both a read and an unread row said "press m to merge", and the extra
            // keystroke on unread work read as the dashboard misbehaving.
            "reviewed · press m to merge"
        } else {
            "done · press m to read the diff"
        }
    } else {
        agent.lifecycle_label()
    }
}

fn integration_detail(agent: &AgentRun) -> Option<String> {
    if agent.integration.phase == IntegrationPhase::Pending
        && agent.integration.bookmark.is_none()
        && agent.integration.merge_change_id.is_none()
    {
        return None;
    }
    let bookmark = agent.integration.bookmark.as_deref().unwrap_or("HEAD");
    let revision = agent
        .integration
        .git_commit
        .as_deref()
        .or(agent.integration.merge_change_id.as_deref())
        .map(short_hash)
        .unwrap_or_else(|| "pending".to_string());
    let remote = if agent.integration.pushed {
        "remote contains commit"
    } else {
        "not pushed"
    };
    Some(format!("{bookmark} @ {revision} · {remote}"))
}

fn delivery_detail(agent: &AgentRun) -> Option<String> {
    if !agent.delivery.required {
        return None;
    }
    let target = agent
        .delivery
        .target
        .as_deref()
        .unwrap_or("target not recorded");
    let revision = agent
        .delivery
        .revision
        .as_deref()
        .map(short_hash)
        .unwrap_or_else(|| "revision not recorded".to_string());
    Some(format!(
        "delivery {} · {} · {} · {} check{}",
        agent.delivery.status.as_str(),
        target,
        revision,
        agent.delivery.checks.len(),
        if agent.delivery.checks.len() == 1 {
            ""
        } else {
            "s"
        }
    ))
}

/// A finished run that still has a workspace to integrate: it sits in the review
/// bucket until merged (m / M for manual runs; automatic for DAG nodes), and hard-dependent nodes stay gated
/// on it. Main, one-off, and planner runs never merge.
pub(crate) fn agent_awaits_merge(agent: &AgentRun) -> bool {
    agent.status == AgentStatus::Done
        && !agent.is_main()
        && !agent.is_oneoff()
        && !matches!(agent.mode, AgentMode::Plan | AgentMode::RudderPlan)
        && agent.has_merge_source()
}

pub(crate) fn agent_status_style(agent: &AgentRun) -> Style {
    if agent.needs_permission || agent.needs_user_input {
        Style::default().fg(RUNNING_COLOR)
    } else if agent.has_merge_conflict() {
        Style::default().fg(FAILED_COLOR)
    } else if agent.status == AgentStatus::Done
        && agent.delivery.required
        && matches!(
            agent.delivery.status,
            DeliveryStatus::Blocked | DeliveryStatus::Failed
        )
    {
        Style::default().fg(FAILED_COLOR)
    } else if agent.status == AgentStatus::Done
        && agent.delivery.required
        && !agent.delivery.is_verified()
    {
        Style::default().fg(RUNNING_COLOR)
    } else {
        status_style(agent.status)
    }
}

pub(crate) fn is_cloud_agent(agent: &AgentRun) -> bool {
    agent.task == "cloud"
        || agent.task.starts_with("cloud ")
        || agent.current_prompt == "cloud"
        || agent.current_prompt.starts_with("cloud ")
}

// status_color now lives in theme.rs (re-exported via the crate root).
