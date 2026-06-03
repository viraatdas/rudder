#![allow(unused_imports)]
//! ratatui rendering: panes, prompts, layout, styles, and scroll math.
use super::*;
use crate::plan_stream::{PlanEntry, PlanEntryKind};

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    // Paint the white canvas so the whole UI is light regardless of the user's
    // terminal background, and bare text inherits the ink fg.
    frame.render_widget(Block::default().style(app_style()), area);

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
        .block(Block::default().borders(Borders::ALL).title("mouse debug").style(app_style()))
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
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
        lines, app, index, agent, diff, focused, task_width, prefix, cont_prefix, &[],
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
    let selected = index == app.selected_agent;
    let marker = if selected { "> " } else { "  " };
    let task_style = if selected {
        pane_text_style(focused)
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
        Span::styled(marker, accent_style(focused)),
        if is_cloud_agent(agent) {
            Span::styled(
                "☁ ",
                cloud_style(true, focused),
            )
        } else {
            Span::raw("")
        },
        Span::styled(truncate_chars(&task_label, task_width), task_style),
    ]);
    first.extend(trailing.iter().cloned());
    lines.push(ListItem::new(Line::from(first)));

    let (status_label, status_style): (&'static str, Style) =
        if agent.is_main() && agent.terminal.is_none() {
            ("idle", muted_style(focused))
        } else {
            (agent_status_label(agent), agent_status_style(agent))
        };
    let badge_style = Style::default().fg(status_style.fg.unwrap_or(MUTED));

    let mut status_line = cont_prefix.to_vec();
    status_line.extend([
        Span::raw("  "),
        Span::styled(BADGE, badge_style),
        Span::raw(" "),
        Span::styled(status_label, status_style),
        Span::raw("  "),
        if agent.is_main() {
            Span::styled("main", accent_style(focused))
        } else if is_cloud_agent(agent) {
            Span::styled("cloud", accent_style(focused))
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
    ]);
    lines.push(ListItem::new(Line::from(status_line)));
    if let Some(summary) = diff {
        let mut diff_line = cont_prefix.to_vec();
        diff_line.extend([
            Span::raw("  "),
            Span::styled(summary, muted_style(focused)),
        ]);
        lines.push(ListItem::new(Line::from(diff_line)));
    }
}

/// Status section for the agents pane. Order here is the rendered/navigated order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Bucket {
    Todo,
    InProgress,
    Review,
    Done,
    Closed,
}

impl Bucket {
    /// Sections in display order; main agents render above all of these.
    const ORDER: [Bucket; 5] = [
        Bucket::Todo,
        Bucket::InProgress,
        Bucket::Review,
        Bucket::Done,
        Bucket::Closed,
    ];

    /// Short header label (the left pane is only 34 cols wide).
    fn label(self) -> &'static str {
        match self {
            Bucket::Todo => "todo",
            Bucket::InProgress => "in progress",
            Bucket::Review => "review",
            Bucket::Done => "done",
            Bucket::Closed => "closed",
        }
    }
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
    match agent.status {
        AgentStatus::Running => Bucket::InProgress,
        AgentStatus::Done => Bucket::Review,
        AgentStatus::Merged => Bucket::Done,
        AgentStatus::Failed | AgentStatus::Stopped => Bucket::Closed,
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
            !agent.is_main() && !agent.is_orchestrator() && status_bucket(agent) == bucket
        })
        .map(|(index, _)| index)
        .collect()
}

/// Canonical agents-pane order used by BOTH the renderer and navigation so the
/// selection marker always lands on the row j/k move to: main agents first, then
/// each status section in `Bucket::ORDER`, nested within the section.
pub(crate) fn sectioned_agent_order(agents: &[AgentRun]) -> Vec<usize> {
    // Pinned orchestrators (RudderPlan) render and navigate first, above main and
    // every status section, so j/k land on them where they are drawn.
    let mut order: Vec<usize> = agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| agent.is_orchestrator())
        .map(|(index, _)| index)
        .collect();
    order.extend(
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.is_main() && !agent.is_orchestrator())
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
    let has_unvisited_children = own_children.iter().any(|(child, _)| !visited.contains(child));
    let cont_prefix = nest_cont_prefix(lanes, has_unvisited_children, is_root);

    let summary = if agent.is_main() {
        None
    } else {
        diff_summaries.get(index).and_then(|opt| opt.clone())
    };
    push_agent_row_with_trailing(
        lines, app, index, agent, summary, focused, task_width, &prefix, &cont_prefix, &trailing,
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

/// Push one planned (not-yet-launched) node row into the Todo section: a label
/// line with the node title and a status line with a Todo-colored badge reading
/// "todo". `prefix`/`cont_prefix` carry the nest glyphs (empty for a root).
fn push_planned_row<'a>(
    lines: &mut Vec<ListItem<'a>>,
    node: &PlannedNode,
    focused: bool,
    task_width: usize,
    prefix: &[Span<'a>],
    cont_prefix: &[Span<'a>],
) {
    let label = if node.title.trim().is_empty() {
        summarize_task(&node.prompt)
    } else {
        node.title.clone()
    };
    let mut first = prefix.to_vec();
    first.extend([
        Span::raw("  "),
        Span::styled(truncate_chars(&label, task_width), pane_text_style(focused)),
    ]);
    lines.push(ListItem::new(Line::from(first)));

    let mut status_line = cont_prefix.to_vec();
    status_line.extend([
        Span::raw("  "),
        Span::styled(BADGE, Style::default().fg(ST_PLANNED)),
        Span::raw(" "),
        Span::styled("todo", Style::default().fg(ST_PLANNED)),
        Span::raw("  "),
        Span::styled("planned", muted_style(focused)),
    ]);
    lines.push(ListItem::new(Line::from(status_line)));
}

/// Walk the planned-node forest depth-first, nesting a node under a parent that is
/// also still planned. Roots are nodes whose every dep id is not itself a pending
/// planned node (so its parent has already launched or never existed).
#[allow(clippy::too_many_arguments)]
fn planned_walk<'a>(
    lines: &mut Vec<ListItem<'a>>,
    nodes: &[PlannedNode],
    children: &std::collections::HashMap<usize, Vec<(usize, EdgeType)>>,
    index: usize,
    edge: Option<EdgeType>,
    is_last: bool,
    focused: bool,
    task_width: usize,
    lanes: &mut Vec<bool>,
    visited: &mut Vec<bool>,
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

    push_planned_row(lines, &nodes[index], focused, task_width, &prefix, &cont_prefix);

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
    nodes: &[PlannedNode],
    focused: bool,
    task_width: usize,
    leading_blank: bool,
    awaiting_approval: bool,
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
    // the user refines it (type into the task pane) or approves (Enter).
    if awaiting_approval {
        lines.push(ListItem::new(Line::from(Span::styled(
            "  type to refine  ·  Enter approve",
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
                lines, nodes, &children, index, None, true, focused, task_width, &mut lanes,
                &mut visited,
            );
        }
    }
    // Anything trapped in a planned-node cycle renders at root so nothing hides.
    for index in 0..nodes.len() {
        if !visited[index] {
            planned_walk(
                lines, nodes, &children, index, None, true, focused, task_width, &mut lanes,
                &mut visited,
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
    if let Some(run) = app
        .agents
        .iter()
        .find(|run| run.node_id.as_deref() == Some(node_id))
    {
        return match run.status {
            AgentStatus::Merged => OrchTaskStatus::Done,
            AgentStatus::Done => OrchTaskStatus::Review,
            AgentStatus::Failed | AgentStatus::Stopped => OrchTaskStatus::Failed,
            _ => OrchTaskStatus::Running,
        };
    }
    OrchTaskStatus::Todo
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
    match extract_rudder_plan_tasks(&rudder_plan_output_for_run(agent)) {
        Ok(tasks) if !tasks.is_empty() => OrchestratorPhase::PlanReady(tasks),
        _ => OrchestratorPhase::Planning,
    }
}

/// Short phase label for the pinned orchestrator row: "planning" while
/// decomposing, then "plan N · running X" once tasks are parsed and launching.
fn orchestrator_phase_label(app: &App, agent: &AgentRun) -> String {
    match orchestrator_phase(agent) {
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
    let selected = index == app.selected_agent;
    let marker = if selected { "> " } else { "  " };
    let label = if selected && app.rename_input.is_some() {
        format!("✎ {}", app.rename_input.clone().unwrap_or_default())
    } else if agent.task_summary.trim().is_empty() {
        summarize_task(&agent.task)
    } else {
        agent.task_summary.clone()
    };
    let label_style = if selected {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(ACCENT)
    };
    lines.push(ListItem::new(Line::from(vec![
        Span::styled(marker, accent_style(focused)),
        Span::styled(format!("{ORCH_MARK} "), label_style),
        Span::styled(truncate_chars(&label, task_width.saturating_sub(2)), label_style),
    ])));

    let planning = matches!(orchestrator_phase(agent), OrchestratorPhase::Planning)
        && agent.status == AgentStatus::Running;
    let badge = if planning {
        app.spinner_glyph()
    } else {
        BADGE
    };
    lines.push(ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(badge, Style::default().fg(ACCENT)),
        Span::raw(" "),
        Span::styled("orchestrator", Style::default().fg(ACCENT)),
        Span::raw("  "),
        Span::styled(orchestrator_phase_label(app, agent), muted_style(focused)),
    ])));
}

pub(crate) fn render_agents(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
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
            pane_text_style(focused),
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
                    accent_style(focused),
                ),
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

    let main_count = app.agents.iter().filter(|a| a.is_main()).count();
    let run_total = app.agents.iter().filter(|a| !a.is_main()).count();
    let orchestrator_count = app.agents.iter().filter(|a| a.is_orchestrator()).count();

    // Pinned orchestrators render at the very top of BOTH views: one distinct
    // accent + diamond row per RudderPlan agent showing its phase, above main and
    // every status section. There is normally one at a time.
    if orchestrator_count > 0 {
        lines.push(ListItem::new(Line::from(Span::styled(
            "orchestrator",
            header_style(focused),
        ))));
        for (index, agent) in app.agents.iter().enumerate() {
            if agent.is_orchestrator() {
                push_orchestrator_row(&mut lines, app, index, agent, focused, task_width);
            }
        }
        lines.push(ListItem::new(Line::default()));
    }

    if app.nest_view {
        // Alternate full-DAG view (toggled with `g`): one topological tree across
        // every agent, ignoring status sections.
        lines.extend(render_agents_nest(app, focused, task_width, &diff_summaries));
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
            lines.push(ListItem::new(Line::from(Span::styled(
                "main",
                header_style(focused),
            ))));
            for (index, agent) in app.agents.iter().enumerate() {
                if !agent.is_main() {
                    continue;
                }
                push_agent_row(&mut lines, app, index, agent, None, focused, task_width, &[], &[]);
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
                render_planned_section(
                    &mut lines,
                    &app.planned_nodes,
                    focused,
                    task_width,
                    leading_blank,
                    app.awaiting_approval,
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

        if main_count == 0 && run_total == 0 && app.planned_nodes.is_empty() {
            lines.push(ListItem::new(Line::from(Span::styled(
                "no agents yet  ·  type a task or /main",
                muted_style(focused),
            ))));
        }
    }

    frame.render_widget(
        List::new(lines)
            .style(app_style())
            .block(pane_block(
                if app.nest_view { "agents · nest" } else { "agents" },
                focused,
                app.nav_mode,
            )),
        area,
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
) -> (std::collections::HashMap<usize, Vec<(usize, EdgeType)>>, Vec<bool>) {
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
            if *continues { NEST_LANE_BAR } else { NEST_LANE_BLANK },
            Style::default().fg(MUTED),
        ));
    }
    if let Some(edge) = edge {
        let (glyph, style) = match edge {
            EdgeType::Hard => (
                if is_last { NEST_CORNER_HARD } else { NEST_TEE_HARD },
                Style::default().fg(MUTED),
            ),
            EdgeType::Soft => (
                if is_last { NEST_CORNER_SOFT } else { NEST_TEE_SOFT },
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
            if *continues { NEST_LANE_BAR } else { NEST_LANE_BLANK },
            Style::default().fg(MUTED),
        ));
    }
    // The node's own subtree lane: a bar while it (or its later siblings) continue.
    if !is_root {
        spans.push(Span::styled(
            if self_continues { NEST_LANE_BAR } else { NEST_LANE_BLANK },
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
        lines, app, index, agent, summary, focused, task_width, &prefix, &cont_prefix,
    );

    // Recurse into children that have not yet been placed in the tree.
    let pending: Vec<(usize, EdgeType)> = own_children
        .iter()
        .copied()
        .filter(|(child, _)| !visited[*child])
        .collect();
    for (position, (child_index, child_edge)) in pending.iter().enumerate() {
        let child_is_last = position + 1 == pending.len();
        lanes.push(!child_is_last);
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

pub(crate) fn render_agents_nest(
    app: &App,
    focused: bool,
    task_width: usize,
    diff_summaries: &[Option<String>],
) -> Vec<ListItem<'static>> {
    let mut lines: Vec<ListItem<'static>> = Vec::new();
    let (children, has_parent) = nest_children_by_index(&app.agents);
    let mut visited = vec![false; app.agents.len()];

    // Pinned orchestrators are drawn as a dedicated row above this tree, so mark
    // them visited to keep the nest walk (and its orphan sweep) from re-drawing them.
    for index in 0..app.agents.len() {
        if app.agents[index].is_orchestrator() {
            visited[index] = true;
        }
    }

    // Roots: any node with no present parent, in display order. Main agents have no
    // deps and so are always roots, rendered first.
    let mut roots: Vec<usize> = (0..app.agents.len())
        .filter(|index| app.agents[*index].is_main() && !app.agents[*index].is_orchestrator())
        .collect();
    roots.extend(
        (0..app.agents.len()).filter(|index| {
            !app.agents[*index].is_main()
                && !app.agents[*index].is_orchestrator()
                && !has_parent[*index]
        }),
    );

    let mut lanes: Vec<bool> = Vec::new();
    for &root in &roots {
        nest_walk(
            &mut lines,
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
                &mut lines,
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

    lines
}

pub(crate) fn visible_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
    // Navigation follows the rendered order: main agents first, then each status
    // section nested by dependency. Sharing `sectioned_agent_order` guarantees the
    // selection marker lands exactly where j/k move.
    sectioned_agent_order(agents)
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
        if agents[index].is_orchestrator() {
            order.push(index);
            visited[index] = true;
        }
    }
    let mut roots: Vec<usize> = (0..agents.len())
        .filter(|&i| agents[i].is_main() && !agents[i].is_orchestrator())
        .collect();
    roots.extend(
        (0..agents.len())
            .filter(|&i| !agents[i].is_main() && !agents[i].is_orchestrator() && !has_parent[i]),
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
/// run in parallel and must NOT be drawn as a sequential chain. Each node nests
/// under its LATEST hard prerequisite (max index) so an integrator renders below
/// its inputs; a node with no hard parent is a top-level (parallel) root. Returns
/// the children adjacency and which nodes have a hard parent. Pure, so the nesting
/// is unit-testable.
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
    let mut children: HashMap<usize, Vec<(usize, EdgeType)>> = HashMap::new();
    let mut has_parent = vec![false; tasks.len()];
    for (child_index, task) in tasks.iter().enumerate() {
        let hard_parent = task
            .deps
            .iter()
            .filter(|edge| edge.edge == EdgeType::Hard)
            .filter_map(|edge| id_to_index.get(edge.on.as_str()).copied())
            .filter(|&parent_index| parent_index != child_index)
            .max();
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
        lanes.push(!child_is_last);
        orchestrator_tree_walk(
            lines, app, tasks, children, *child_index, Some(*child_edge), child_is_last, lanes,
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

    let phase = orchestrator_phase(agent);
    let phase_label = match &phase {
        OrchestratorPhase::Planning => "planning".to_string(),
        OrchestratorPhase::PlanReady(tasks) => format!("plan · {} tasks", tasks.len()),
    };

    // Draw the pane border first; content is laid out inside it. Splitting the
    // inner area into fixed (header + DAG) and scrollable (plan prose) regions lets
    // the DAG tree stay PINNED on top while only the prose plan scrolls.
    let block = pane_block("orchestrator", focused, app.nav_mode);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Header: ◆ ORCHESTRATOR · phase [· auto-merge].
    let mut header_spans = vec![
        Span::styled(
            format!("{ORCH_MARK} ORCHESTRATOR"),
            Style::default().fg(ACCENT),
        ),
        Span::styled("  ·  ", muted_style(focused)),
        Span::styled(phase_label, muted_style(focused)),
    ];
    if app.auto_merge {
        header_spans.push(Span::styled("  ·  ", muted_style(focused)));
        header_spans.push(Span::styled(
            "auto-merge",
            Style::default().fg(ACCENT),
        ));
    }
    let header: Vec<Line<'static>> = vec![Line::from(header_spans)];

    // Chat input (pinned at the bottom when focused): the follow-up being composed,
    // with a visible block cursor at worker_input_cursor so Left/Right/Home/End and
    // typing show where the insertion point is. Rendered inline (a reversed cell) so
    // it tracks the text through wrapping without any coordinate math.
    let draft = agent.worker_input_draft.clone();
    let cursor_col = agent.worker_input_cursor;
    let input: Vec<Line<'static>> = if focused {
        let mut spans = vec![Span::styled(
            "› ",
            Style::default().fg(ACCENT),
        )];
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
            let spinner_label = if app.rebasing {
                "rebasing the plan…"
            } else if app.refining {
                "refining the plan…"
            } else {
                "decomposing the task…"
            };
            body.push(Line::from(vec![
                Span::styled(
                    app.spinner_glyph(),
                    Style::default().fg(ACCENT),
                ),
                Span::raw(" "),
                Span::styled(spinner_label, pane_text_style(true)),
            ]));
            body.push(Line::default());
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
                push_transcript_lines(&mut body, transcript, focused);
            }
            render_orchestrator_layout(
                frame,
                inner,
                &header,
                &[],
                &body,
                &input,
                &mut app.orch_dag_scroll,
                true,
            );
        }
        OrchestratorPhase::PlanReady(mut tasks) => {
            // Fold in nodes the user RECONCILED into the plan after launch. They live
            // only in `planned_nodes` (never in the orchestrator's frozen plan block),
            // so without this they are invisible in the orchestrator DAG. The initial
            // nodes share ids with `tasks`, so only post-launch additions get appended
            // here, and the user sees a typed task show up in the DAG.
            for node in &app.planned_nodes {
                if !tasks.iter().any(|task| task.id == node.id) {
                    tasks.push(node.to_task());
                }
            }
            // PINNED top region: the DAG tree + a live status line. The tree nests
            // by HARD edges only (see orchestrator_hard_tree) so parallel soft-linked
            // tasks render as top-level roots instead of a misleading chain.
            let mut dag: Vec<Line<'static>> = Vec::new();
            let (children, has_parent) = orchestrator_hard_tree(&tasks);
            let mut visited = vec![false; tasks.len()];
            let mut lanes: Vec<bool> = Vec::new();
            for index in 0..tasks.len() {
                if !has_parent[index] {
                    orchestrator_tree_walk(
                        &mut dag, app, &tasks, &children, index, None, true, &mut lanes,
                        &mut visited,
                    );
                }
            }
            // Any node trapped in a cycle (e.g. part of a dependency loop) has a parent
            // and so was skipped above; render it at depth 0 so a cycle never hides work.
            for index in 0..tasks.len() {
                if !visited[index] {
                    orchestrator_tree_walk(
                        &mut dag, app, &tasks, &children, index, None, true, &mut lanes,
                        &mut visited,
                    );
                }
            }
            let (mut running, mut review, mut done, mut todo) = (0usize, 0usize, 0usize, 0usize);
            for task in &tasks {
                match orchestrator_task_status(app, &task.id) {
                    OrchTaskStatus::Running => running += 1,
                    OrchTaskStatus::Review => review += 1,
                    OrchTaskStatus::Done => done += 1,
                    OrchTaskStatus::Failed => {}
                    OrchTaskStatus::Todo => todo += 1,
                }
            }
            dag.push(Line::from(Span::styled(
                format!("running {running} · review {review} · done {done} · todo {todo}"),
                muted_style(focused),
            )));
            if app.awaiting_approval {
                dag.push(Line::from(Span::styled(
                    "chat to refine  ·  Enter to approve & launch",
                    Style::default().fg(ACCENT),
                )));
            }

            // SCROLLABLE body: the human summary, THEN a per-node breakdown. Showing
            // each task's goal, done-when, and dependencies surfaces the plan's full
            // depth and fills the pane instead of leaving it blank below a short
            // summary.
            let mut prose: Vec<Line<'static>> = Vec::new();
            prose.push(Line::from(Span::styled("Plan", header_style(focused))));
            // Re-extract the summary from the LIVE planner text every frame, not the
            // frozen app.plan_summary captured at exit-detection: the planner's final
            // bytes (the rest of the summary) often arrive a tick or two AFTER it is
            // marked done, so a one-shot capture truncated it mid-sentence. Reading
            // live lets the prose self-heal as the post-exit drain ingests the tail.
            let summary = extract_rudder_plan_summary(&rudder_plan_output_for_run(agent))
                .or_else(|| app.plan_summary.clone())
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
                if let Some(goal) = task.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
                    prose.push(Line::from(Span::styled(
                        format!("    goal: {goal}"),
                        muted_style(focused),
                    )));
                }
                if let Some(success) =
                    task.success.as_deref().map(str::trim).filter(|s| !s.is_empty())
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
            render_orchestrator_layout(
                frame,
                inner,
                &header,
                &dag,
                &prose,
                &input,
                &mut app.orch_dag_scroll,
                false,
            );
        }
    }

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
/// scrolls (via `scroll`); the DAG stays on top. `stick_bottom` pins the body to
/// its last line (used for the live planning stream).
#[allow(clippy::too_many_arguments)]
fn render_orchestrator_layout(
    frame: &mut Frame<'_>,
    inner: Rect,
    header: &[Line<'static>],
    top: &[Line<'static>],
    body: &[Line<'static>],
    input: &[Line<'static>],
    scroll: &mut usize,
    stick_bottom: bool,
) {
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
    let avail = inner.height.saturating_sub(header_h).saturating_sub(input_h);
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
        Paragraph::new(header.to_vec()).style(app_style()).wrap(Wrap { trim: false }),
        chunks[0],
    );
    if top_h > 0 {
        frame.render_widget(
            Paragraph::new(top.to_vec()).style(app_style()).wrap(Wrap { trim: false }),
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
    *scroll = (*scroll).min(max_scroll);
    let effective = if stick_bottom { max_scroll } else { *scroll };
    frame.render_widget(
        Paragraph::new(body.to_vec())
            .style(app_style())
            .scroll((effective as u16, 0))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    if input_h > 0 {
        frame.render_widget(
            Paragraph::new(input.to_vec()).style(app_style()).wrap(Wrap { trim: false }),
            chunks[3],
        );
    }
}

/// Render a planner transcript (reasoning/tool/text/user turns) into `out`, with
/// dim italic thinking, muted tool/system lines, and accented user turns.
fn push_transcript_lines(out: &mut Vec<Line<'static>>, transcript: &[PlanEntry], focused: bool) {
    for entry in transcript {
        let (prefix, style) = match entry.kind {
            PlanEntryKind::Thinking => ("· ", muted_style(focused).add_modifier(Modifier::ITALIC)),
            PlanEntryKind::Tool => ("  ", muted_style(focused)),
            PlanEntryKind::System => ("", muted_style(focused)),
            PlanEntryKind::UserTurn => (
                "you: ",
                Style::default().fg(ACCENT),
            ),
            PlanEntryKind::Text => ("", pane_text_style(focused)),
        };
        let mut first = true;
        for sub in entry.text.split('\n') {
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

pub(crate) fn render_worker(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = block_inner(area);
    let terminal_size = TerminalSize::new(inner.height.max(1), inner.width.max(1)).ok();
    let focused = app.focus == FocusPane::Worker;

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
    // The orchestrator ALWAYS gets the custom command-center view: a live streaming
    // transcript while planning (its raw PTY is now JSON events, not human text), and
    // the DAG tree once a plan is ready. Both phases are rendered by render_orchestrator.
    if app.worker_view == WorkerView::Terminal && selected_is_orchestrator {
        render_orchestrator(frame, area, app);
        return;
    }

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
                WorkerView::Terminal => {
                    if selected_is_orchestrator {
                        "orchestrator"
                    } else {
                        "worker"
                    }
                }
                WorkerView::Diff => "review · Esc/v back · m merge",
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

/// The default task-pane hint shown when there is no transient notice. Centralised
/// so render_task, task_pane_height, and selection's height/hit-test calc all wrap
/// the SAME string: if these drift, the pane height and mouse selection bounds
/// disagree and clicks on valid input rows get rejected.
pub(crate) fn task_default_hint(app: &App) -> &'static str {
    if app.awaiting_approval {
        "type to refine the plan  ·  Enter (empty) to approve & launch  ·  Option-1/2/3 or ^W pane"
    } else if app.plan_is_active() {
        "type a task to add it to the running plan (shows up in the orchestrator DAG)  ·  ^W pane"
    } else {
        "Enter to plan + run  ·  Up/Down history  ·  Option-1/2/3 or ^W pane"
    }
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
                if app.awaiting_approval {
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
        for line in wrapped_hint {
            lines.push(Line::from(Span::styled(line, muted_style(focused))));
        }
    } else {
        let first_hint = wrapped_hint.first().cloned().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(first_hint, muted_style(focused)),
            Span::raw("  "),
            Span::styled(
                if app.awaiting_approval { "refine" } else { "run" },
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
                    ,
            )
            .style(app_style()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(list, area);
}

pub(crate) fn render_merge_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (title, lines, border_color): (Span<'static>, Vec<Line<'static>>, Color) =
        if let Some(confirm) = &app.merge_confirm {
            let headline = match &confirm.intent {
                MergeIntent::Selected { task, .. } => format!("Merge:  {}", short_task(task)),
                MergeIntent::All { ids } => format!(
                    "Merge {} completed worktree{}",
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                ),
            };
            (
                Span::styled(" Confirm merge ", Style::default().fg(ACCENT)),
                vec![
                    Line::from(Span::styled(headline, app_style())),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Dependent nodes unblock once it lands. A clean merge is mechanical (no AI); on conflict you can launch an AI resolver or resolve it yourself.",
                        muted_style(true),
                    )),
                    Line::from(""),
                    merge_confirm_hint_line(),
                ],
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
            if prompt.conflicted_files.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No specific files were reported.",
                    muted_style(true),
                )));
            } else {
                for file in &prompt.conflicted_files {
                    lines.push(Line::from(vec![
                        Span::styled("   • ", muted_style(true)),
                        Span::styled(file.clone(), app_style()),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(conflict_resolve_hint_line());
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

    let modal = centered_modal(area, 76, (lines.len() as u16).saturating_add(2));
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
                ,
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

pub(crate) fn merge_confirm_hint_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("Press ", muted_style(true)),
        Span::styled("y", accent_style(true)),
        Span::styled(" to merge  ·  n or Esc to cancel", muted_style(true)),
    ])
}

/// Hint for the conflict modal: the keys are color-emphasized (teal), the rest muted,
/// matching the thin theme (emphasis by color, not weight) and the "·"-separated style
/// used in the orchestrator pane.
pub(crate) fn conflict_resolve_hint_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("Press ", muted_style(true)),
        Span::styled("y", accent_style(true)),
        Span::styled(" for an AI resolver  ·  ", muted_style(true)),
        Span::styled("n", accent_style(true)),
        Span::styled(" to resolve it yourself  ·  Esc to cancel", muted_style(true)),
    ])
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

pub(crate) fn pane_block(title: &'static str, focused: bool, nav_mode: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(FOCUS_COLOR)
    } else {
        Style::default().fg(INACTIVE_COLOR)
    };

    // Focused: white text on a teal title fill. Unfocused: a quiet ink-on-paper
    // label. No bold anywhere (the theme is thin).
    let title_style = if focused {
        Style::default().fg(PAPER).bg(FOCUS_COLOR)
    } else {
        Style::default().fg(MUTED).bg(PAPER)
    };
    let _ = nav_mode;

    // Carry the white canvas style so the pane interior is painted paper, not the
    // terminal background.
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

pub(crate) fn push_wrapped_word(lines: &mut Vec<String>, current: &mut String, word: &str, max_width: usize) {
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
        // Explicit ink. The app paints its own white canvas, so primary text must
        // set a dark fg; the terminal-default fg could be light = invisible on white.
        Style::default().fg(INK)
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

/// Base style for the whole UI: ink on the white canvas. Every pane block and
/// paragraph carries this so the background is white regardless of the user's
/// terminal theme, and bare `Span::raw` text inherits the ink fg.
pub(crate) fn app_style() -> Style {
    Style::default().fg(INK).bg(PAPER)
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
#[allow(dead_code)]
pub(crate) fn orchestrator_dag_max_scroll(content_rows: usize, visible_height: usize) -> usize {
    content_rows.saturating_sub(visible_height)
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

pub(crate) fn status_style(status: AgentStatus) -> Style {
    Style::default().fg(status_color(status))
}

pub(crate) fn agent_status_label(agent: &AgentRun) -> &'static str {
    if agent.needs_permission {
        "needs permission"
    } else if agent.needs_user_input {
        "needs input"
    } else if matches!(
        agent.mode,
        AgentMode::Plan | AgentMode::PlanFront | AgentMode::RudderPlan
    ) && agent.status == AgentStatus::Running
    {
        "planning"
    } else if agent.status == AgentStatus::Merged {
        "[x] merged"
    } else {
        agent.status.as_str()
    }
}

pub(crate) fn agent_status_style(agent: &AgentRun) -> Style {
    if agent.needs_permission || agent.needs_user_input {
        Style::default()
            .fg(RUNNING_COLOR)
            
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

