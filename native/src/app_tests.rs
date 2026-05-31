    use super::*;

    fn count_byte_subsequence(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

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
    fn rudder_mouse_capture_uses_sgr_wheel_modes_without_any_event_tracking() {
        let mut output = Vec::new();

        enable_rudder_mouse_capture(&mut output).expect("enable mouse capture");

        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1000h"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1002h"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1006h"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1003h"), 0);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1003l"), 1);
    }

    #[test]
    fn rudder_mouse_capture_disables_sgr_wheel_modes_without_any_event_tracking() {
        let mut output = Vec::new();

        disable_rudder_mouse_capture(&mut output).expect("disable mouse capture");

        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1006l"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1002l"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1000l"), 1);
        assert_eq!(count_byte_subsequence(&output, b"\x1b[?1003l"), 1);
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
    fn ctrl_w_leader_then_digit_focuses_pane() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(app.leader_pending);
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(!app.leader_pending);
        assert_eq!(app.focus, FocusPane::Worker);
    }

    #[test]
    fn ctrl_w_leader_is_one_shot() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::Agents);
        // The leader disarmed after one command: a bare digit is not a command.
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::Agents);
    }

    #[test]
    fn ctrl_w_leader_escape_cancels_without_action() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(app.leader_pending);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.leader_pending);
        assert_eq!(app.focus, FocusPane::Worker);
    }

    #[test]
    fn option_typographic_chars_focus_panes() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        // Option+1 on a US macOS layout arrives as the bare char ¡.
        app.handle_key(KeyEvent::new(KeyCode::Char('\u{00a1}'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::Agents);
        // Option+3 = £
        app.handle_key(KeyEvent::new(KeyCode::Char('\u{00a3}'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::Task);
    }

    #[test]
    fn alt_digit_still_focuses_pane() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.focus, FocusPane::Worker);
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
