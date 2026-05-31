#![allow(unused_imports)]
//! ratatui rendering: panes, prompts, layout, styles, and scroll math.
use super::*;

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
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
            Style::default().fg(Color::Yellow),
        )))
        .block(Block::default().borders(Borders::ALL).title("mouse debug"))
        .style(app_style())
        .wrap(Wrap { trim: false }),
        rect,
    );
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

pub(crate) fn visible_agent_indices(agents: &[AgentRun]) -> Vec<usize> {
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
pub(crate) fn render_worker(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
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

pub(crate) fn render_task(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPane::Task;
    let default_hint = if app.plan_mode {
        "Enter plan  Up/Down history  Option-1/2/3 or ^W pane  /plan off"
    } else {
        "Enter start  Up/Down history  Option-1/2/3 or ^W pane  /plan  /rudder-plan  /sync"
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

pub(crate) fn task_pane_height(app: &App, width: u16) -> u16 {
    let default_hint = if app.plan_mode {
        "Enter plan  Up/Down history  Option-1/2/3 or ^W pane  /plan off"
    } else {
        "Enter start  Up/Down history  Option-1/2/3 or ^W pane  /plan  /rudder-plan  /sync"
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
                    .add_modifier(Modifier::BOLD),
            )
            .style(app_style()),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(list, area);
}

pub(crate) fn render_merge_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
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

pub(crate) fn merge_confirm_hint_line() -> Line<'static> {
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
        Style::default()
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

pub(crate) fn muted_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

pub(crate) fn accent_style(focused: bool) -> Style {
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

pub(crate) fn model_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(MODEL_COLOR)
    } else {
        Style::default()
            .fg(INACTIVE_COLOR)
            .add_modifier(Modifier::DIM)
    }
}

pub(crate) fn cloud_style(connected: bool, focused: bool) -> Style {
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

pub(crate) fn app_style() -> Style {
    Style::default()
}

pub(crate) fn error_style() -> Style {
    Style::default()
        .fg(FAILED_COLOR)
        .add_modifier(Modifier::BOLD)
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

pub(crate) fn agent_status_style(agent: &AgentRun) -> Style {
    if agent.needs_permission || agent.needs_user_input {
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD)
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

pub(crate) fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Running => RUNNING_COLOR,
        AgentStatus::Done | AgentStatus::Merged => DONE_COLOR,
        AgentStatus::Failed => FAILED_COLOR,
        AgentStatus::Stopped => INACTIVE_COLOR,
    }
}

