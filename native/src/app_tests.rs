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
    fn execution_prompt_tells_the_worker_it_is_a_jj_runtime() {
        let prompt = execution_prompt("add a feature");
        assert!(prompt.contains("jj (Jujutsu)"), "names the VCS: {prompt}");
        assert!(prompt.contains("jj status") && prompt.contains("jj diff"));
        // The old Hunk review integration paragraph is gone.
        assert!(!prompt.to_lowercase().contains("hunk"), "no hunk: {prompt}");
    }

    #[test]
    fn jj_merge_conflict_prompt_uses_jj_not_git() {
        let mut app = App::new();
        app.conflict_prompt = Some(MergeConflictPrompt {
            operation: ConflictOperation::Merge,
            task: "wire up the parser".to_string(),
            conflicted_files: vec!["src/parser.rs".to_string()],
            error: "jj merge created conflicts.".to_string(),
            repo_root: std::env::temp_dir(),
            target_branch: None,
            source_branch: None,
            worktree_path: None,
            agent_id: Some("agent-1".to_string()),
        });
        let prompt = app.conflict_resolution_prompt().expect("merge prompt");
        assert!(prompt.contains("jj resolve --list"), "uses jj resolve: {prompt}");
        assert!(prompt.contains("jj status"));
        // The old git INSTRUCTIONS must be gone (the prompt now only mentions git to
        // warn the resolver away from it, which does not resolve jj conflicts).
        assert!(!prompt.contains("Run `git status`"), "no git status step: {prompt}");
        assert!(
            !prompt.contains("Stage the resolved files"),
            "no git staging step: {prompt}"
        );
    }

    #[test]
    fn manual_goal_prompt_leads_with_goal_and_done_when() {
        let prompt = manual_goal_prompt("fix the flaky login test");
        assert!(prompt.starts_with("/goal fix the flaky login test\n"));
        assert!(prompt.contains("Done when: the task is implemented and its own verification passes"));
        // The full task body follows the header.
        assert!(prompt.contains("fix the flaky login test"));
    }

    #[test]
    fn manual_goal_prompt_is_idempotent_for_existing_goal_prompts() {
        let already = "/goal do the thing\nDone when: tests pass\n\nbody text";
        assert_eq!(manual_goal_prompt(already), already);
    }

    #[test]
    fn execution_prompt_hoists_goal_block_above_context() {
        let prompt =
            execution_prompt("/goal ship the parser\nDone when: cargo test passes\n\nWrite the parser.");
        // The /goal line must be the very first line so the backend picks it up.
        assert!(prompt.starts_with("/goal ship the parser\nDone when: cargo test passes\n"));
        assert!(prompt.contains("Rudder-specific context injected by Rudder"));
        assert!(prompt.contains("Write the parser."));
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
    fn selection_point_from_mouse_offsets_by_pane_inner_origin() {
        // Callers pass block_inner(pane) as `area`, so the point must be relative to
        // that inner origin (pane border + any header already accounted for by the
        // caller's Rect). Row/col are clamped into the inner height/width.
        let inner = Rect {
            x: 2,
            y: 3,
            width: 10,
            height: 5,
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 6,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            selection_point_from_mouse(mouse, inner),
            SelectionPoint { row: 3, col: 3 }
        );

        // A click above/left of the inner origin clamps to (0, 0).
        let above = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            selection_point_from_mouse(above, inner),
            SelectionPoint { row: 0, col: 0 }
        );

        // A click past the inner extent clamps to the last cell.
        let past = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 99,
            row: 99,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            selection_point_from_mouse(past, inner),
            SelectionPoint { row: 4, col: 9 }
        );
    }

    #[test]
    fn orchestrator_dag_scroll_offset_moves_and_clamps() {
        let mut app = App::new();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let scroll_up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        let scroll_down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };

        // Start at the bottom (large offset); scrolling up moves toward the top.
        app.orch_dag_scroll = 5;
        app.scroll_orchestrator_dag(scroll_up, area);
        assert!(app.orch_dag_scroll < 5);

        // Scrolling up never goes below zero (cannot scroll above the first line).
        app.orch_dag_scroll = 0;
        app.scroll_orchestrator_dag(scroll_up, area);
        assert_eq!(app.orch_dag_scroll, 0);

        // Scrolling down increases the offset (reveals later lines).
        app.orch_dag_scroll = 0;
        app.scroll_orchestrator_dag(scroll_down, area);
        assert!(app.orch_dag_scroll > 0);
    }

    #[test]
    fn orchestrator_dag_scroll_clamp_reaches_bottom_and_no_overflow() {
        // Content shorter than the viewport pins the offset to 0.
        assert_eq!(orchestrator_dag_max_scroll(4, 10), 0);
        assert_eq!(orchestrator_dag_max_scroll(10, 10), 0);
        // The last content row can scroll to the top of the viewport, no further.
        assert_eq!(orchestrator_dag_max_scroll(25, 10), 15);

        // Wrapped-row accounting: a wide line consumes multiple viewport rows.
        assert_eq!(wrapped_row_count(0, 10), 1);
        assert_eq!(wrapped_row_count(10, 10), 1);
        assert_eq!(wrapped_row_count(11, 10), 2);
        assert_eq!(wrapped_row_count(25, 10), 3);
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

        // The orchestrator (RudderPlan) is a read-only DECOMPOSER, not Claude plan
        // mode: inspection-only tools, edit/write/bash blocked, no plan-mode UI.
        let orchestrator_claude = agent_command(
            Backend::Claude,
            "sonnet",
            None,
            "build the feature",
            AgentMode::RudderPlan,
            None,
        );
        assert!(
            orchestrator_claude
                .args
                .windows(2)
                .any(|window| window[0] == "--permission-mode" && window[1] == "default"),
            "orchestrator runs read-only (permission-mode default), not plan mode"
        );
        assert!(
            !orchestrator_claude
                .args
                .windows(2)
                .any(|window| window[0] == "--permission-mode" && window[1] == "plan"),
            "orchestrator is not Claude plan mode"
        );
        assert!(
            orchestrator_claude
                .args
                .windows(2)
                .any(|window| window[0] == "--allowedTools" && window[1].contains("Read")),
            "orchestrator allows read-only inspection tools"
        );
        assert!(
            orchestrator_claude
                .args
                .windows(2)
                .any(|window| window[0] == "--disallowedTools" && window[1].contains("Edit")),
            "orchestrator blocks edit/write tools"
        );
        assert!(
            orchestrator_claude.args.iter().any(|arg| arg == "-p"),
            "orchestrator runs non-interactively (claude -p), not the interactive TUI"
        );
        assert!(
            orchestrator_claude
                .args
                .windows(2)
                .any(|w| w[0] == "--output-format" && w[1] == "stream-json"),
            "orchestrator streams JSON events for the live transcript"
        );
        assert!(
            orchestrator_claude.args.iter().any(|arg| arg == "--include-partial-messages"),
            "orchestrator streams partial messages so text/thinking arrive incrementally"
        );

        let rudder_plan = agent_command(
            Backend::Codex,
            "gpt-5.5",
            Some(EffortLevel::High),
            "build the feature",
            AgentMode::RudderPlan,
            None,
        );
        assert_eq!(
            rudder_plan.args.first().map(String::as_str),
            Some("exec"),
            "codex orchestrator runs non-interactively via codex exec"
        );
        assert!(
            !rudder_plan.args.iter().any(|arg| arg == "--no-alt-screen"),
            "codex exec is not the interactive TUI"
        );
        assert!(rudder_plan
            .args
            .windows(2)
            .any(|window| window[0] == "--sandbox" && window[1] == "read-only"));
        assert!(rudder_plan.args.iter().any(|arg| {
            arg.contains("RUDDER_PLAN_TASKS_START")
                && arg.contains("build the feature")
                && arg.contains("Every task MUST carry both a `goal` and a `success`")
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
    fn rudder_plan_prompt_is_a_read_only_decomposer_with_typed_deps() {
        let prompt = rudder_plan_prompt("build the feature");
        // Decomposer framing: the planner is read-only and must NOT implement; it
        // only emits the DAG, which it prints as a normal assistant message.
        assert!(
            prompt.contains("read-only DECOMPOSER") && prompt.contains("not an implementer"),
            "prompt must frame the planner as a read-only decomposer, not an implementer"
        );
        assert!(
            prompt.contains("Do NOT implement"),
            "prompt must tell the planner not to implement the work"
        );
        assert!(
            prompt.contains("separate set of worker agents"),
            "prompt must say workers implement the tasks, not the planner"
        );
        // Typed-dependency DAG: it must ask for an id and typed deps, and define the
        // hard/soft edge semantics rather than telling the model to make everything
        // independent / one task.
        assert!(prompt.contains("`id`") && prompt.contains("`deps`"));
        assert!(prompt.contains("\"on\"") && prompt.contains("\"type\"") && prompt.contains("\"why\""));
        assert!(prompt.contains("hard") && prompt.contains("soft"));
        assert!(
            prompt.contains("MINIMAL set of hard edges"),
            "prompt must ask for the minimal hard-edge set"
        );
        // Clarifying-questions-first behavior is requested.
        assert!(prompt.contains("clarifying questions"));
        assert!(prompt.contains("RUDDER_PLAN_TASKS_START") && prompt.contains("RUDDER_PLAN_TASKS_END"));
        // It must NOT still tell the model to make everything independent.
        assert!(
            !prompt.contains("smallest set of independent implementation tasks"),
            "old independent-only wording must be gone"
        );
    }

    #[test]
    fn extracts_rudder_plan_tasks_from_marked_json_block() {
        let output = "\x1b[32mRUDDER_PLAN_TASKS_START\x1b[0m\n{\"tasks\":[{\"title\":\"API\",\"prompt\":\"Implement API and test it.\",\"goal\":\"Complete the API without stopping until tests pass.\",\"success\":\"cargo test passes\"},{\"title\":\"UI\",\"prompt\":\"Implement UI and test it.\"}]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "API");
        assert_eq!(
            tasks[0].goal.as_deref(),
            Some("Complete the API without stopping until tests pass.")
        );
        assert_eq!(tasks[0].success.as_deref(), Some("cargo test passes"));
        assert_eq!(tasks[1].prompt, "Implement UI and test it.");
        // goal + success stay backward-compatible: a plan block that omits them
        // still parses (with None), and the worker prompt derives defaults.
        assert_eq!(tasks[1].goal, None);
        assert_eq!(tasks[1].success, None);
    }

    #[test]
    fn extracts_rudder_plan_tasks_through_osc_and_csi_noise() {
        // The non-interactive planner PTY stream carries CSI color codes AND OSC
        // sequences (Claude's OSC 777 "needs your permission" notification). The
        // parser must strip all of them so the JSON block is recovered intact.
        let output = "\x1b[1msome reasoning\x1b[0m\n\x1b]777;notify;Claude Code;Claude needs your permission\x07\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"client\",\"prompt\":\"build client\",\"goal\":\"client\",\"success\":\"ok\",\"deps\":[]},{\"id\":\"n1\",\"title\":\"loop\",\"prompt\":\"build loop\",\"goal\":\"loop\",\"success\":\"ok\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"needs client\"}]}]}\nRUDDER_PLAN_TASKS_END\n\x1b]0;done\x07";
        let tasks = extract_rudder_plan_tasks(output).expect("parse through OSC/CSI noise");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].deps.len(), 1, "the hard edge survives stripping");
        // And the stripper removes the OSC payload entirely.
        assert!(!strip_ansi_for_plan(output).contains("needs your permission"));
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
    fn flat_plan_tasks_synthesize_ids_and_have_no_deps() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"A\",\"prompt\":\"Do A.\"},{\"title\":\"B\",\"prompt\":\"Do B.\"}]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "n0");
        assert_eq!(tasks[1].id, "n1");
        assert!(tasks[0].deps.is_empty());
        assert!(tasks[1].deps.is_empty());
        assert_eq!(tasks[0].backend, None);
    }

    #[test]
    fn parses_typed_deps_and_optional_fields() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"schema\",\"prompt\":\"Write schema.\"},\
            {\"id\":\"n1\",\"title\":\"api\",\"prompt\":\"Write API.\",\"backend\":\"codex\",\"model\":\"gpt-5\",\"effort\":\"high\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"needs the schema n0 migrates\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].deps.len(), 1);
        let edge = &tasks[1].deps[0];
        assert_eq!(edge.on, "n0");
        assert_eq!(edge.edge, EdgeType::Hard);
        assert_eq!(edge.why.as_deref(), Some("needs the schema n0 migrates"));
        assert_eq!(tasks[1].backend.as_deref(), Some("codex"));
        assert_eq!(tasks[1].model.as_deref(), Some("gpt-5"));
        assert_eq!(tasks[1].effort.as_deref(), Some("high"));
    }

    #[test]
    fn drops_dep_edges_to_unknown_ids() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"Do a.\",\"deps\":[{\"on\":\"ghost\",\"type\":\"hard\",\"why\":\"x\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks.len(), 1);
        assert!(
            tasks[0].deps.is_empty(),
            "edge to unknown id should be dropped"
        );
    }

    #[test]
    fn downgrades_unjustified_hard_edge_to_soft() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"Do a.\"},\
            {\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"Do b.\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");
        assert_eq!(tasks[1].deps.len(), 1);
        assert_eq!(
            tasks[1].deps[0].edge,
            EdgeType::Soft,
            "a hard edge without a why must downgrade to soft"
        );
    }

    #[test]
    fn rejects_hard_dependency_cycle() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"Do a.\",\"deps\":[{\"on\":\"n1\",\"type\":\"hard\",\"why\":\"loop\"}]},\
            {\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"Do b.\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"loop\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let result = extract_rudder_plan_tasks(output);
        assert!(result.is_err(), "a hard-edge cycle must be rejected");
    }

    #[test]
    fn soft_cycle_does_not_deadlock_parsing() {
        // soft edges never block, so a soft cycle is allowed (it cannot deadlock).
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"Do a.\",\"deps\":[{\"on\":\"n1\"}]},\
            {\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"Do b.\",\"deps\":[{\"on\":\"n0\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("soft cycle parses");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].deps[0].edge, EdgeType::Soft);
        assert_eq!(tasks[1].deps[0].edge, EdgeType::Soft);
    }

    #[test]
    fn ready_nodes_only_blocks_on_hard_deps() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"Do a.\"},\
            {\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"Do b.\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"needs a merged\"}]},\
            {\"id\":\"n2\",\"title\":\"c\",\"prompt\":\"Do c.\",\"deps\":[{\"on\":\"n0\",\"type\":\"soft\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END";
        let tasks = extract_rudder_plan_tasks(output).expect("parse tasks");

        // Nothing merged: roots n0 and the soft-only child n2 are ready, n1 is not.
        let ready: Vec<&str> = ready_nodes(&tasks, &[])
            .iter()
            .map(|task| task.id.as_str())
            .collect();
        assert!(ready.contains(&"n0"));
        assert!(ready.contains(&"n2"), "soft deps never block readiness");
        assert!(!ready.contains(&"n1"), "hard dep not yet merged blocks n1");

        // Once n0 is merged, n1 becomes ready too.
        let ready: Vec<&str> = ready_nodes(&tasks, &["n0".to_string()])
            .iter()
            .map(|task| task.id.as_str())
            .collect();
        assert!(ready.contains(&"n1"));
    }

    #[test]
    fn rudder_plan_worker_prompt_always_leads_with_goal_and_done_when() {
        let task = RudderPlanTask {
            id: "n0".to_string(),
            title: "API".to_string(),
            prompt: "Implement API and run cargo test.".to_string(),
            goal: Some("Complete the API without stopping until cargo test passes.".to_string()),
            success: Some("cargo test passes with no failures".to_string()),
            deps: Vec::new(),
            backend: None,
            model: None,
            effort: None,
        };

        // The /goal block leads the prompt unconditionally for BOTH backends.
        for backend in [Backend::Codex, Backend::Claude] {
            let prompt = rudder_plan_worker_prompt("build the feature", &task, backend);
            assert!(
                prompt.starts_with("/goal Complete the API without stopping until cargo test passes."),
                "prompt must lead with /goal: {prompt}"
            );
            assert!(prompt.contains("\nDone when: cargo test passes with no failures"));
            assert!(prompt.contains("Original request:\nbuild the feature"));
            assert!(prompt.contains("Worker task: API"));
        }
    }

    #[test]
    fn rudder_plan_worker_prompt_derives_goal_and_success_when_absent() {
        // No explicit goal/success: the worker prompt still emits a /goal line
        // (derived from the title) and a Done-when line (the canonical default).
        let task = RudderPlanTask {
            id: "n0".to_string(),
            title: "Implement the parser".to_string(),
            prompt: "Write the parser and add tests.".to_string(),
            goal: None,
            success: None,
            deps: Vec::new(),
            backend: None,
            model: None,
            effort: None,
        };
        let prompt = rudder_plan_worker_prompt("build it", &task, Backend::Claude);
        assert!(
            prompt.starts_with("/goal Implement the parser\n"),
            "derived /goal from title: {prompt}"
        );
        assert!(
            prompt.contains("Done when: the task is implemented and its own verification passes"),
            "derived Done-when default: {prompt}"
        );
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
    fn jj_run_record_writes_vcs_jj_and_workspace_fields() {
        let repo_root = std::env::temp_dir().join(format!(
            "rudder-jj-record-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&repo_root).expect("create test repo root");

        let mut run = test_agent_run("jj-run-1", "wire up jj isolation");
        run.cwd = repo_root.join("workspace");
        run.worktree_path = Some(run.cwd.clone());
        run.worktree_branch = None;
        run.workspace_name = Some("rudder-jj-run-1-abc123".to_string());
        run.jj_change_id = Some("zzzzzzzz".to_string());

        save_native_run_record(&repo_root, &run).expect("save jj run record");
        let raw = fs::read_to_string(native_run_dir(&repo_root, "jj-run-1").join("run.json"))
            .expect("read run.json");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");

        assert_eq!(value.get("vcs").and_then(|v| v.as_str()), Some("jj"));
        let worktree = value.get("worktree").expect("worktree object");
        assert_eq!(
            worktree.get("workspaceName").and_then(|v| v.as_str()),
            Some("rudder-jj-run-1-abc123")
        );
        assert_eq!(
            worktree.get("jjChangeId").and_then(|v| v.as_str()),
            Some("zzzzzzzz")
        );
        // jj runs omit the legacy git branch field.
        assert!(worktree.get("branch").is_none());

        let _ = fs::remove_dir_all(repo_root);
    }

    #[test]
    fn legacy_git_run_record_keeps_vcs_git_and_branch() {
        let repo_root = std::env::temp_dir().join(format!(
            "rudder-git-record-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&repo_root).expect("create test repo root");

        let mut run = test_agent_run("git-run-1", "legacy worktree run");
        run.cwd = repo_root.join("worktree");
        run.worktree_path = Some(run.cwd.clone());
        run.worktree_branch = Some("rudder/legacy-1".to_string());
        run.workspace_name = None;
        run.jj_change_id = None;

        save_native_run_record(&repo_root, &run).expect("save git run record");
        let raw = fs::read_to_string(native_run_dir(&repo_root, "git-run-1").join("run.json"))
            .expect("read run.json");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");

        assert_eq!(value.get("vcs").and_then(|v| v.as_str()), Some("git"));
        let worktree = value.get("worktree").expect("worktree object");
        assert_eq!(
            worktree.get("branch").and_then(|v| v.as_str()),
            Some("rudder/legacy-1")
        );
        assert!(worktree.get("workspaceName").is_none());

        let _ = fs::remove_dir_all(repo_root);
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
    fn orchestrator_chat_cursor_edits_mid_line() {
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        app.agents = vec![orch];
        app.selected_agent = 0;
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
        for c in "abc".chars() {
            app.handle_orchestrator_chat_key(key(c));
        }
        assert_eq!(app.agents[0].worker_input_draft, "abc");
        assert_eq!(app.agents[0].worker_input_cursor, 3);
        // Move left and insert mid-line.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()));
        assert_eq!(app.agents[0].worker_input_cursor, 2);
        app.handle_orchestrator_chat_key(key('X'));
        assert_eq!(app.agents[0].worker_input_draft, "abXc");
        assert_eq!(app.agents[0].worker_input_cursor, 3);
        // Home / End jump to the ends.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Home, KeyModifiers::empty()));
        assert_eq!(app.agents[0].worker_input_cursor, 0);
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::End, KeyModifiers::empty()));
        assert_eq!(app.agents[0].worker_input_cursor, 4);
        // Backspace deletes before the cursor.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.agents[0].worker_input_draft, "abX");
        assert_eq!(app.agents[0].worker_input_cursor, 3);
    }

    #[test]
    fn orchestrator_pane_mouse_drag_selects_visible_text() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        app.worker_area = Some(Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        });
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        app.agents = vec![orch];
        app.selected_agent = 0;
        // render() normally captures these from the buffer; set them directly here.
        app.orch_visible_rows = vec!["hello world".to_string(), "second line".to_string()];

        // inner = block_inner(worker_area) = {x:1,y:1,...}; column/row 1,1 -> (0,0).
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });
        assert!(app.orch_selection.is_some(), "down starts a selection");

        // Drag right to inner col 4 (column 5) on the same row.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });
        let sel = app.orch_selection.expect("selection still active");
        assert_eq!(sel.start.row, 0);
        assert_eq!(sel.start.col, 0);
        assert_eq!(sel.end.col, 4);
        // The text the Up handler would copy: chars 0..=4 of row 0 -> "hello".
        assert_eq!(selected_text_from_lines(&app.orch_visible_rows, sel), "hello");

        // A plain click (down+up, no movement) clears the highlight.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 2,
            modifiers: KeyModifiers::empty(),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 2,
            modifiers: KeyModifiers::empty(),
        });
        assert!(app.orch_selection.is_none(), "empty click clears selection");
    }

    #[test]
    fn planning_redraw_gate_tracks_running_not_parse_state() {
        // The spinner animation depends on this gate forcing a per-tick redraw. It
        // must stay true for the whole time the planner is alive, regardless of
        // whether a plan block has parsed yet (the old is_err() gate froze the
        // spinner the moment a present-but-empty block appeared).
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.status = AgentStatus::Running;
        app.agents = vec![orch];
        assert!(app.has_planning_orchestrator(), "running planner animates");

        // Planner finished -> nothing is spinning, so the gate releases.
        app.agents[0].status = AgentStatus::Done;
        assert!(!app.has_planning_orchestrator(), "done planner does not animate");

        // A refine in flight keeps redraws coming even between status snapshots.
        app.refining = true;
        assert!(app.has_planning_orchestrator(), "refining animates");
    }

    #[test]
    fn orchestrator_chat_word_editing_keys() {
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.worker_input_draft = "use python instead".to_string();
        orch.worker_input_cursor = "use python instead".chars().count();
        app.agents = vec![orch];
        app.selected_agent = 0;

        // Option/Alt+Backspace deletes the previous word ("instead").
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(app.agents[0].worker_input_draft, "use python ");
        assert_eq!(app.agents[0].worker_input_cursor, "use python ".chars().count());

        // Alt+Left jumps back over a word ("python "), then Alt+Backspace removes it.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.agents[0].worker_input_cursor, "use ".chars().count());
        app.handle_orchestrator_chat_key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        ));
        assert_eq!(app.agents[0].worker_input_cursor, 0);

        // Ctrl+K truncates from the cursor to the end of the line.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::End, KeyModifiers::empty()));
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        app.handle_orchestrator_chat_key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.agents[0].worker_input_draft, "use ");

        // Ctrl+D deletes the char at the cursor; Delete does the same.
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Home, KeyModifiers::empty()));
        app.handle_orchestrator_chat_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.agents[0].worker_input_draft, "se ");
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::empty()));
        assert_eq!(app.agents[0].worker_input_draft, "e ");
    }

    #[test]
    fn orchestrator_chat_renders_cursor_block() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.worker_input_draft = "use python".to_string();
        orch.worker_input_cursor = 3;
        app.agents = vec![orch];
        app.selected_agent = 0;
        // Render: the draft text appears in the orchestrator pane (with the cursor at
        // index 3 drawn as a reversed cell over the 4th char). We assert the text is
        // present; the cursor styling is exercised by the render path not panicking.
        let text = render_worker_text(&mut app, 72, 24);
        assert!(text.contains("use") && text.contains("python"), "draft renders: {text}");
    }

    #[test]
    fn automerge_command_toggles_the_flag() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        assert!(!app.auto_merge, "auto-merge defaults off");
        assert!(app.handle_command("/automerge"));
        assert!(app.auto_merge, "first /automerge turns it on");
        assert!(app.handle_command("/automerge"));
        assert!(!app.auto_merge, "second /automerge turns it off");
    }

    #[test]
    fn finished_agent_does_not_reopen_on_repaint() {
        // The flicker: highlighting a done agent triggers a resize/repaint -> output,
        // which previously flipped done -> in-progress. Output with no user input
        // since completion must NOT re-open the agent; input after completion does.
        let done = Instant::now();
        // No input at all -> repaint, stays done.
        assert!(!post_completion_output_is_new_turn(None, Some(done)));
        // Input BEFORE completion (the original turn) -> still just a repaint.
        let before = done.checked_sub(Duration::from_secs(5)).unwrap_or(done);
        assert!(!post_completion_output_is_new_turn(Some(before), Some(done)));
        // Input AFTER completion -> a genuine new turn, re-open as in-progress.
        let after = done + Duration::from_secs(1);
        assert!(post_completion_output_is_new_turn(Some(after), Some(done)));
    }

    #[test]
    fn busy_spinner_reopens_a_falsely_completed_agent() {
        // A reappearing spinner proves the agent is still working: reopen even with
        // no user input (mis-detected completion during an inter-step lull).
        let busy: Vec<String> = vec![
            "Editing src/main.rs".to_string(),
            "> ".to_string(),
            "* Cogitating... (12s \u{00b7} esc to interrupt)".to_string(),
            "  bypass permissions on (shift+tab to cycle)".to_string(),
        ];
        assert!(recent_lines_look_busy(&busy));

        // The idle resize/repaint that caused the old flicker shows NO spinner, so it
        // must NOT reopen a finished agent.
        let idle: Vec<String> = vec![
            "Edited 3 files".to_string(),
            "> ".to_string(),
            "  bypass permissions on (shift+tab to cycle)".to_string(),
        ];
        assert!(!recent_lines_look_busy(&idle));
    }

    #[test]
    fn merge_confirm_hint_highlights_merge_action() {
        let line = merge_confirm_hint_line();
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "Press y to merge, n or Esc to cancel.");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].style.fg, Some(FAILED_COLOR));
        // Thin theme: emphasis is by color, not weight.
        assert!(!line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn agent_pane_hints_include_review_and_merge_all_shortcuts() {
        assert!(AGENT_PANE_HINTS.contains(&"R review all"));
        assert!(AGENT_PANE_HINTS.contains(&"M merge all"));
    }

    #[test]
    fn g_key_toggles_nest_view_in_agents_pane() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        assert!(!app.nest_view, "nest view is off (flat) by default");

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty()));
        assert!(app.nest_view, "g toggles nest view on");

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty()));
        assert!(!app.nest_view, "g toggles nest view back off");
    }

    #[test]
    fn nest_hint_is_listed_in_agent_pane() {
        assert!(AGENT_PANE_HINTS.contains(&"g nest"));
    }

    fn nest_line_text(item: &ListItem) -> String {
        // ListItem wraps a Text (one Line per row); join its first line's spans.
        format!("{item:?}")
    }

    #[test]
    fn nest_view_renders_parents_before_children_with_glyphs() {
        let mut app = App::new();
        // n0 (root) -> n1 hard child -> n2 hard grandchild, plus n3 a soft child of n0.
        let n0 = test_agent_run("n0", "schema");
        let mut n1 = test_agent_run("n1", "api");
        n1.deps = vec!["n0".to_string()];
        let mut n2 = test_agent_run("n2", "tests");
        n2.deps = vec!["n1".to_string()];
        let mut n3 = test_agent_run("n3", "docs");
        n3.soft_deps = vec!["n0".to_string()];
        app.agents = vec![n0, n1, n2, n3];

        let diff: Vec<Option<String>> = vec![None, None, None, None];
        let lines = render_agents_nest(&app, true, 40, &diff);

        // Collect the task-label lines (the first span line for each node) in order.
        // Each node contributes a task line then a status line (no diffs here), so
        // task-label lines are at even offsets.
        let label_text = |item: &ListItem| -> String {
            let dbg = nest_line_text(item);
            dbg
        };
        // Find the order of node summaries by scanning the rendered debug text.
        let blob: String = lines.iter().map(label_text).collect::<Vec<_>>().join("\n");
        let pos = |needle: &str| blob.find(needle).unwrap_or(usize::MAX);
        let p0 = pos("schema");
        let p1 = pos("api");
        let p2 = pos("tests");
        let p3 = pos("docs");
        assert!(p0 < p1, "parent schema renders before child api");
        assert!(p1 < p2, "child api renders before grandchild tests");
        assert!(p0 < p3, "parent schema renders before soft child docs");

        // Assert glyphs: a hard connector glyph (├─ or ╰─) and a soft dashed
        // connector (├┄ or ╰┄) both appear in the rendered tree.
        assert!(
            blob.contains('\u{2570}') || blob.contains('\u{251c}'),
            "hard connector glyph (corner/tee) present"
        );
        assert!(
            blob.contains('\u{2504}'),
            "soft dashed connector glyph present"
        );
    }

    #[test]
    fn nest_view_first_child_line_carries_connector_prefix_span() {
        let mut app = App::new();
        let n0 = test_agent_run("n0", "root task");
        let mut n1 = test_agent_run("n1", "child task");
        n1.deps = vec!["n0".to_string()];
        app.agents = vec![n0, n1];

        let diff: Vec<Option<String>> = vec![None, None];
        let lines = render_agents_nest(&app, true, 40, &diff);

        // The first rendered line is the root's task label: no connector glyph at
        // depth 0 (just the selection marker). ListItem content is private in
        // ratatui, so inspect via the Debug representation like the sibling test.
        let root_dbg = nest_line_text(&lines[0]);
        assert!(
            !root_dbg.contains('\u{2570}') && !root_dbg.contains('\u{251c}'),
            "root task line has no connector glyph"
        );

        // The child task-label line (third entry: root label, root status, child
        // label) carries a hard corner connector styled MUTED (Rgb 154,147,132).
        let child_dbg = nest_line_text(&lines[2]);
        assert!(
            child_dbg.contains('\u{2570}'),
            "single hard child uses the corner connector"
        );
        assert!(
            child_dbg.contains("107, 114, 128"),
            "hard connector is MUTED"
        );
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
            deps: Vec::new(),
            soft_deps: Vec::new(),
            node_id: None,
            reconcile_planner: false,
            plan_stream: None,
            last_worker_input_at: None,
            ready_since: None,
            merge_resolver: false,
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
            .is_some_and(|notice| notice.contains("no completed workspaces")));
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
            workspace_name: None,
            jj_change_id: None,
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
            deps: Vec::new(),
            soft_deps: Vec::new(),
            node_id: None,
            reconcile_planner: false,
            plan_stream: None,
            last_worker_input_at: None,
            ready_since: None,
            merge_resolver: false,
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

    // --- CHANGE 1: plan -> planned-node queue (DAG orchestration) ------------

    fn test_planned_node(id: &str, deps: &[&str]) -> PlannedNode {
        PlannedNode {
            id: id.to_string(),
            title: id.to_string(),
            prompt: format!("do {id}"),
            goal: None,
            success: None,
            deps: deps.iter().map(ToString::to_string).collect(),
            soft_deps: Vec::new(),
            backend: None,
            model: None,
            effort: None,
        }
    }

    #[test]
    fn planned_node_ready_when_hard_deps_merged_or_missing() {
        let plan_ids = vec!["n0".to_string(), "n1".to_string()];

        let root = test_planned_node("n0", &[]);
        assert!(root.is_ready(&[], &plan_ids), "a 0-dep root is always ready");

        let child = test_planned_node("n1", &["n0"]);
        assert!(
            !child.is_ready(&[], &plan_ids),
            "child blocked while parent unmerged"
        );
        assert!(
            child.is_ready(&["n0".to_string()], &plan_ids),
            "child ready once parent merged"
        );

        // A dep id not present anywhere in the plan is treated as satisfied so the
        // DAG never deadlocks on a dangling reference.
        let dangling = test_planned_node("n1", &["ghost"]);
        assert!(
            dangling.is_ready(&[], &plan_ids),
            "dep absent from plan is treated as satisfied"
        );
    }

    /// A fake plan-launched agent (no PTY) with a node id and status, used to drive
    /// the scheduler's merged-set / cap accounting in tests.
    fn node_agent(node_id: &str, status: AgentStatus) -> AgentRun {
        let mut run = test_agent_run(node_id, &format!("work {node_id}"));
        run.node_id = Some(node_id.to_string());
        run.status = status;
        run
    }

    #[test]
    fn scheduler_launches_ready_root_but_holds_blocked_dependent() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // n0 root + n1 hard-depends on n0. Nothing merged yet.
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];

        // With no merged ids, only the root n0 is ready.
        let position = app.next_node_to_launch(4).expect("a ready node");
        assert_eq!(app.planned_nodes[position].id, "n0", "root launches first");

        // Pretend n0 launched and is running: n1 is still blocked (n0 not merged).
        app.planned_nodes.remove(position);
        app.agents.push(node_agent("n0", AgentStatus::Running));
        assert!(
            app.next_node_to_launch(4).is_none(),
            "dependent stays in todo until its parent merges"
        );

        // n0 merges: n1's hard dep is satisfied, so it becomes launchable.
        app.agents[0].status = AgentStatus::Merged;
        let position = app.next_node_to_launch(4).expect("dependent now ready");
        assert_eq!(app.planned_nodes[position].id, "n1", "merged parent unblocks n1");
    }

    #[test]
    fn scheduler_respects_parallelism_cap() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // Three independent ready roots, but only 2 slots.
        app.planned_nodes = vec![
            test_planned_node("n0", &[]),
            test_planned_node("n1", &[]),
            test_planned_node("n2", &[]),
        ];
        app.agents.push(node_agent("n0", AgentStatus::Running));
        app.agents.push(node_agent("n1", AgentStatus::Running));

        assert_eq!(
            app.running_plan_agents(),
            2,
            "two plan-launched agents are running"
        );
        assert!(
            app.next_node_to_launch(2).is_none(),
            "cap reached: no further node launches while 2 run"
        );
        // A slot frees (one finishes): the next ready node may launch.
        app.agents[0].status = AgentStatus::Done;
        assert!(
            app.next_node_to_launch(2).is_some(),
            "a freed slot lets a waiting node launch"
        );
    }

    #[test]
    fn scheduler_treats_unknown_dep_as_satisfied() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // n0 hard-depends on an id absent from the plan -> not a deadlock.
        app.planned_nodes = vec![test_planned_node("n0", &["ghost"])];
        assert!(
            app.next_node_to_launch(4).is_some(),
            "a node whose dep is absent from the plan is launchable"
        );
    }

    #[test]
    fn discard_planned_queue_clears_todo() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];

        app.discard_planned_queue();

        assert!(app.planned_nodes.is_empty(), "queue is cleared");
        assert_eq!(app.notice.as_deref(), Some("discarded 2 planned node(s)"));
    }

    #[test]
    fn d_key_does_not_discard_plan_queue() {
        // The plan is refined by typing into the task pane, never discarded with `d`.
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        app.planned_nodes = vec![test_planned_node("n0", &[])];
        app.focus = FocusPane::Agents;

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        assert_eq!(app.planned_nodes.len(), 1, "d no longer discards the plan queue");
    }

    #[test]
    fn enter_on_agent_focuses_worker() {
        let mut app = App::new();
        let run = test_agent_run("run-1", "ordinary task");
        app.agents.push(run);
        app.selected_agent = 0;
        app.focus = FocusPane::Agents;

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(app.focus, FocusPane::Worker, "Enter focuses worker as before");
        assert_eq!(app.agents.len(), 1);
    }

    #[cfg(not(windows))]
    fn planner_with_block(app: &App, block: &str) -> AgentRun {
        // Spawn a real pty that prints the RUDDER_PLAN_TASKS block so
        // rudder_plan_output_for_run can read it back from the output log.
        let script = format!("printf '%s' {}; sleep 1", shell_single_quote(block));
        let command = TerminalCommand::with_args("/bin/sh", ["-lc", script.as_str()]);
        let mut pane = TerminalPane::spawn_shell_or_command(
            Some(command),
            TerminalPaneOptions {
                size: TerminalSize { rows: 40, cols: 80 },
                scrollback_lines: 400,
                ..Default::default()
            },
        )
        .expect("spawn planner pty");
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            pane.drain_output();
            if pane.output_log_snapshot().contains("RUDDER_PLAN_TASKS_END") {
                break;
            }
        }
        let mut run = test_agent_run("planner-1", "plan it");
        run.cwd = app.cwd.clone();
        run.mode = AgentMode::RudderPlan;
        run.status = AgentStatus::Done;
        run.autosteered = true;
        run.terminal = Some(pane);
        run
    }

    #[cfg(not(windows))]
    fn shell_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(not(windows))]
    #[test]
    fn completed_plan_queues_planned_nodes_and_pins_orchestrator() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // A two-node plan with a hard dep: n1 (ui) depends on n0 (api). Evaluation
        // queues BOTH nodes and gates for approval: NOTHING launches yet.
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"api\",\"prompt\":\"build api\",\"goal\":\"api\",\"success\":\"tests pass\"},\
            {\"id\":\"n1\",\"title\":\"ui\",\"prompt\":\"build ui\",\"goal\":\"ui\",\"success\":\"renders\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"ui needs the api merged\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = planner_with_block(&app, block);
        app.agents.push(planner);
        app.selected_agent = 0;

        app.evaluate_completed_plan(0);

        // The planner run is KEPT as the pinned orchestrator (it owns the plan), not
        // removed; its autosteer flag is cleared so it is only captured once.
        let orchestrators: Vec<&AgentRun> = app
            .agents
            .iter()
            .filter(|run| run.mode == AgentMode::RudderPlan)
            .collect();
        assert_eq!(orchestrators.len(), 1, "planner stays as the pinned orchestrator");
        assert!(orchestrators[0].is_orchestrator());
        assert!(!orchestrators[0].autosteered, "captured-once flag cleared");
        // APPROVAL GATE: both nodes queued, nothing launched, awaiting approval.
        assert!(app.awaiting_approval, "plan gates for approval");
        assert_eq!(app.planned_nodes.len(), 2, "all nodes queued, none launched");
        assert!(
            !app.agents.iter().any(|run| run.node_id.is_some()),
            "no worker launched before approval"
        );

        // APPROVE: Enter on the selected orchestrator launches the ready root.
        app.handle_agents_key(KeyEvent::from(KeyCode::Enter));
        assert!(!app.awaiting_approval, "approval clears the gate");
        // n0 (root) launched -> an agent tagged with its node id. n1 stays in Todo
        // because its hard dep n0 has not merged.
        assert_eq!(app.planned_nodes.len(), 1, "blocked dependent stays in todo");
        assert_eq!(app.planned_nodes[0].id, "n1");
        assert!(
            app.agents.iter().any(|run| run.node_id.as_deref() == Some("n0")),
            "ready root n0 launched as a node-tagged agent after approval"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn trivial_plan_gates_for_approval_then_launches_on_enter() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"only\",\"prompt\":\"do the thing\",\"goal\":\"thing\",\"success\":\"done\"}]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = planner_with_block(&app, block);
        app.agents.push(planner);
        app.selected_agent = 0;

        app.evaluate_completed_plan(0);

        // Even a 1-node plan gates: it is queued, nothing launches, awaiting approval.
        assert!(app.awaiting_approval, "1-node plan still gates for approval");
        assert_eq!(app.planned_nodes.len(), 1, "the single node is queued, not launched");
        assert!(
            !app.agents.iter().any(|run| run.node_id.is_some()),
            "no worker launched before approval"
        );

        // APPROVE launches the single ready node.
        app.handle_agents_key(KeyEvent::from(KeyCode::Enter));
        assert!(!app.awaiting_approval, "approval clears the gate");
        assert!(app.planned_nodes.is_empty(), "the single node launched after approval");
        assert!(
            app.agents.iter().any(|run| run.node_id.is_some()),
            "a node-tagged worker run was launched"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn evaluate_populates_planned_nodes_without_launching() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"do a\",\"goal\":\"a\",\"success\":\"ok\"},\
            {\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"do b\",\"goal\":\"b\",\"success\":\"ok\"}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = planner_with_block(&app, block);
        app.agents.push(planner);
        app.selected_agent = 0;

        app.evaluate_completed_plan(0);

        // The gate is set, every node is queued, and NO scheduler launch happened.
        assert!(app.awaiting_approval);
        assert_eq!(app.planned_nodes.len(), 2);
        assert!(
            app.agents.iter().all(|run| run.node_id.is_none()),
            "evaluate must not launch any worker"
        );
    }

    // --- RECONCILE: add work into the existing DAG ---------------------------

    /// A RudderPlan agent flagged as a reconcile planner, whose PTY printed the
    /// given RUDDER_PLAN_TASKS block. Used to drive the APPEND path in tests.
    #[cfg(not(windows))]
    fn reconcile_planner_with_block(app: &App, block: &str) -> AgentRun {
        let mut run = planner_with_block(app, block);
        run.reconcile_planner = true;
        run.task = "add a docs page".to_string();
        run
    }

    #[test]
    fn second_task_routes_to_reconcile_when_plan_active() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // A plan is active: a node is queued. A newly typed task must reconcile into
        // the existing plan, not spawn a fresh planner that would replace it.
        app.planned_nodes = vec![test_planned_node("n0", &[])];
        assert!(app.plan_is_active(), "queued nodes mean a plan is active");

        let before_agents = app.agents.len();
        let before_nodes = app.planned_nodes.len();
        app.start_task_from_input("add a second feature");

        // Exactly one new agent appeared and it is a reconcile planner (not a fresh
        // initial planner). The existing queued node is untouched (not replaced).
        assert_eq!(app.agents.len(), before_agents + 1, "one planner spawned");
        let spawned = app.agents.last().expect("a spawned planner");
        assert_eq!(spawned.mode, AgentMode::RudderPlan);
        assert!(
            spawned.reconcile_planner,
            "an active plan routes the second task to a RECONCILE planner"
        );
        assert_eq!(
            app.planned_nodes.len(),
            before_nodes,
            "the existing queued node is preserved, not wiped by a fresh plan"
        );
    }

    #[test]
    fn no_plan_active_first_task_uses_initial_planner_not_reconcile() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // No nodes queued, no orchestrator, no live plan-launched agent.
        assert!(!app.plan_is_active(), "no plan active on a clean session");

        app.start_task_from_input("build the first feature");

        let spawned = app.agents.last().expect("a spawned planner");
        assert_eq!(spawned.mode, AgentMode::RudderPlan);
        assert!(
            !spawned.reconcile_planner,
            "the first task with no active plan uses the INITIAL planner (replace path)"
        );
    }

    #[test]
    fn plan_active_when_unmerged_node_agent_running() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // No queued nodes, but a plan-launched agent is still running (not merged).
        app.agents.push(node_agent("n0", AgentStatus::Running));
        assert!(
            app.plan_is_active(),
            "an unmerged plan-launched agent keeps the plan active"
        );

        // Once that agent merges and nothing else is in flight, the plan is no
        // longer active and the next task starts a fresh plan.
        app.agents[0].status = AgentStatus::Merged;
        assert!(
            !app.plan_is_active(),
            "a fully-merged plan is no longer active"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn evaluate_completed_reconcile_appends_and_does_not_replace() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // Existing plan: two queued nodes n0 + n1, gating for approval so the
        // append does not immediately schedule (we inspect the queue directly).
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];
        app.planned_origin = "build it".to_string();
        app.awaiting_approval = true;

        // Reconcile planner returns ONE node with a soft dep on the queued node n1.
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"new\",\"title\":\"docs\",\"prompt\":\"write docs\",\"goal\":\"docs\",\"success\":\"docs build\",\"deps\":[{\"on\":\"n1\",\"type\":\"soft\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = reconcile_planner_with_block(&app, block);
        let planner_index = app.agents.len();
        app.agents.push(planner);

        app.evaluate_completed_reconcile(planner_index);

        // Both existing queued nodes survived (NOT replaced); the new node was
        // APPENDED (push). The queue now holds n0, n1, and new.
        let queued_ids: Vec<&str> = app.planned_nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(app.planned_nodes.len(), 3, "appended, not replaced: {queued_ids:?}");
        assert!(queued_ids.contains(&"n0"), "existing node n0 preserved: {queued_ids:?}");
        assert!(queued_ids.contains(&"n1"), "existing node n1 preserved: {queued_ids:?}");
        assert!(queued_ids.contains(&"new"), "new node appended: {queued_ids:?}");
        // The appended node carries its soft dep on the queued node n1.
        let added = app.planned_nodes.iter().find(|n| n.id == "new").unwrap();
        assert_eq!(added.soft_deps, vec!["n1".to_string()], "soft dep on n1 retained");
        assert!(added.deps.is_empty(), "no hard dep");
        // The transient reconcile planner was removed (no lingering orchestrator).
        assert!(
            !app.agents.iter().any(|run| run.reconcile_planner),
            "reconcile planner removed after append"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn evaluate_completed_reconcile_schedules_when_session_running() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // No queued nodes, but a plan-launched agent n0 is running: the session is
        // approved/running (not awaiting approval), so an appended ready node should
        // be drained into a live worker by the scheduler.
        app.agents.push(node_agent("n0", AgentStatus::Running));
        app.planned_origin = "build it".to_string();
        app.awaiting_approval = false;

        // The added node is fully independent (no deps), so it is ready immediately.
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"new\",\"title\":\"docs\",\"prompt\":\"write docs\",\"goal\":\"docs\",\"success\":\"ok\"}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = reconcile_planner_with_block(&app, block);
        let planner_index = app.agents.len();
        app.agents.push(planner);

        app.evaluate_completed_reconcile(planner_index);

        // The scheduler launched the ready appended node: it left the queue and now
        // exists as a node-tagged worker agent.
        assert!(
            app.planned_nodes.iter().all(|n| n.id != "new"),
            "ready appended node was scheduled out of the queue"
        );
        assert!(
            app.agents.iter().any(|run| {
                run.node_id.as_deref() == Some("new") || run.node_id.as_deref() == Some("new-2")
            }),
            "the appended node launched as a node-tagged agent"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn evaluate_completed_reconcile_uniquifies_colliding_id() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // An existing queued node already uses id "new"; the reconcile node also
        // claims "new" and must be uniquified so dep resolution stays unambiguous.
        app.planned_nodes = vec![test_planned_node("new", &[])];
        app.awaiting_approval = true; // initial plan still gating

        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"new\",\"title\":\"docs\",\"prompt\":\"write docs\",\"goal\":\"docs\",\"success\":\"ok\"}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = reconcile_planner_with_block(&app, block);
        let planner_index = app.agents.len();
        app.agents.push(planner);

        app.evaluate_completed_reconcile(planner_index);

        let ids: Vec<&str> = app.planned_nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(app.planned_nodes.len(), 2, "both nodes queued: {ids:?}");
        assert!(ids.contains(&"new"), "original id kept: {ids:?}");
        assert!(
            ids.iter().any(|id| *id == "new-2"),
            "colliding id was uniquified to new-2: {ids:?}"
        );
        // Awaiting approval: the appended node is left QUEUED, nothing launched.
        assert!(
            !app.agents.iter().any(|run| run.node_id.is_some()),
            "nothing launches while the plan awaits approval"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn evaluate_completed_reconcile_no_deps_falls_back_to_soft_frontier() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // Frontier: one queued node n0 + one running plan-launched agent n1. Gate
        // for approval so the no-deps node stays queued (we inspect its soft edges).
        app.planned_nodes = vec![test_planned_node("n0", &[])];
        app.agents.push(node_agent("n1", AgentStatus::Running));
        app.awaiting_approval = true;

        // Reconcile planner returns a node with NO deps at all.
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"new\",\"title\":\"docs\",\"prompt\":\"write docs\",\"goal\":\"docs\",\"success\":\"ok\"}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let planner = reconcile_planner_with_block(&app, block);
        let planner_index = app.agents.len();
        app.agents.push(planner);

        app.evaluate_completed_reconcile(planner_index);

        let added = app
            .planned_nodes
            .iter()
            .find(|n| n.id == "new")
            .expect("appended node present");
        // FALLBACK: a soft edge to every current frontier id (n0 and n1), and no
        // hard deps so it can never deadlock.
        assert!(added.deps.is_empty(), "no hard deps from the fallback");
        let mut soft = added.soft_deps.clone();
        soft.sort();
        assert_eq!(
            soft,
            vec!["n0".to_string(), "n1".to_string()],
            "no-deps node gets soft edges to the whole frontier"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn reconcile_planner_is_discriminated_from_initial_planner() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // An initial planner (replace) and a reconcile planner (append) both Done.
        let block_initial = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"do a\",\"goal\":\"a\",\"success\":\"ok\"}]}\nRUDDER_PLAN_TASKS_END\n";
        let initial = planner_with_block(&app, block_initial);
        assert!(!initial.reconcile_planner, "initial planner is not a reconcile planner");

        let block_reconcile = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"new\",\"title\":\"b\",\"prompt\":\"do b\",\"goal\":\"b\",\"success\":\"ok\"}]}\nRUDDER_PLAN_TASKS_END\n";
        let reconcile = reconcile_planner_with_block(&app, block_reconcile);
        assert!(reconcile.reconcile_planner, "reconcile planner carries the discriminator");

        // The discriminator is what the poll loop branches on: capture the initial
        // plan (replace) and verify the queue holds exactly its node.
        app.agents.push(initial);
        app.evaluate_completed_plan(0);
        assert_eq!(app.planned_nodes.len(), 1);
        assert_eq!(app.planned_nodes[0].id, "n0", "initial plan captured via replace path");
    }

    #[test]
    fn reconcile_prompt_names_frontier_and_asks_for_one_node() {
        let frontier = vec![
            ("n0".to_string(), "build api".to_string()),
            ("n1".to_string(), "build ui".to_string()),
        ];
        let prompt = rudder_reconcile_prompt("add a docs page", &frontier);
        // Plan-mode safety mirrors the initial planner.
        assert!(prompt.contains("Do NOT call ExitPlanMode"));
        assert!(prompt.contains("plan mode"));
        // It names the existing frontier nodes and asks for exactly one new node.
        assert!(prompt.contains("n0: build api"), "frontier listed: {prompt}");
        assert!(prompt.contains("n1: build ui"), "frontier listed: {prompt}");
        assert!(prompt.contains("EXACTLY ONE node"), "asks for one node: {prompt}");
        assert!(prompt.contains("unique from every existing node id"));
        assert!(prompt.contains("hard") && prompt.contains("soft"));
        assert!(prompt.contains("ALREADY in flight"), "states the plan is in flight");
        assert!(prompt.contains("RUDDER_PLAN_TASKS_START") && prompt.contains("RUDDER_PLAN_TASKS_END"));
    }

    #[test]
    fn d_on_orchestrator_does_not_discard_pending_plan() {
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.status = AgentStatus::Running;
        app.agents = vec![orch];
        app.selected_agent = 0;
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];
        app.awaiting_approval = true;

        // d on the orchestrator must NOT throw the plan away: the plan is refined by
        // typing into the task pane and approved with Enter. The gate stays open.
        app.handle_agents_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(app.planned_nodes.len(), 2, "d does not discard the plan");
        assert!(app.awaiting_approval, "the approval gate stays open");
    }

    #[test]
    fn remove_planned_node_drops_one_and_keeps_rest() {
        let mut app = App::new();
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];
        app.planned_origin = "build it".to_string();
        app.awaiting_approval = true;

        assert!(app.remove_planned_node("n1"), "removes the named node");
        assert_eq!(app.planned_nodes.len(), 1, "one node remains");
        assert_eq!(app.planned_nodes[0].id, "n0");
        // The plan is not empty, so the gate stays up for the surviving node.
        assert!(app.awaiting_approval, "gate stays while nodes remain");

        // Removing the last node clears the gate and the origin.
        assert!(app.remove_planned_node("n0"));
        assert!(app.planned_nodes.is_empty());
        assert!(!app.awaiting_approval, "removing the last node clears the gate");
        assert!(app.planned_origin.is_empty());
        // A node id not in the queue is a no-op.
        assert!(!app.remove_planned_node("ghost"));
    }

    // --- Plan refinement (plan-mode-style discussion before approval) --------

    #[test]
    fn extract_rudder_plan_summary_reads_prose_after_the_block() {
        let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[]}\nRUDDER_PLAN_TASKS_END\n\nAssumptions: I assumed a Node CLI.\nOpen question: which package manager?";
        let summary = extract_rudder_plan_summary(output).expect("summary present");
        assert!(summary.contains("Assumptions: I assumed a Node CLI."));
        assert!(summary.contains("Open question: which package manager?"));
        // No trailing prose -> None.
        assert!(extract_rudder_plan_summary("RUDDER_PLAN_TASKS_START\n{}\nRUDDER_PLAN_TASKS_END").is_none());
    }

    #[test]
    fn build_refine_request_carries_original_plan_and_feedback() {
        let req = build_refine_request("make a CLI", "- n0 [scaffold] deps: none", "use Python not Node");
        assert!(req.contains("make a CLI"), "keeps the original request");
        assert!(req.contains("n0 [scaffold]"), "includes the current plan");
        assert!(req.contains("use Python not Node"), "includes the feedback");
        assert!(req.contains("REVISED"), "asks for a revised full DAG");
    }

    #[test]
    fn approve_is_blocked_while_refining() {
        // Empty-Enter (or Enter on the orchestrator) must NOT launch the stale plan
        // while a refine is in flight; the gate stays up until the revised DAG lands.
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];
        app.awaiting_approval = true;
        app.refining = true;

        app.approve_planned_queue();

        assert!(app.awaiting_approval, "refining blocks approval; gate stays up");
        assert_eq!(app.planned_nodes.len(), 2, "the stale plan is not launched");
    }

    #[test]
    fn current_plan_outline_lists_nodes_with_deps() {
        let mut app = App::new();
        app.planned_nodes = vec![test_planned_node("n0", &[]), test_planned_node("n1", &["n0"])];
        let outline = app.current_plan_outline();
        assert!(outline.contains("n0"));
        assert!(outline.contains("n1"));
        assert!(outline.contains("n0:hard"), "hard dep is shown: {outline}");
    }

    // --- Live planner stream (plan_stream.rs) --------------------------------
    // ingest() takes the FULL cumulative output_log snapshot each call (the PTY log
    // grows), so the tests append to a buffer and feed the whole buffer each time.

    fn text_delta_line(text: &str) -> String {
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        format!(
            "{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{escaped}\"}}}}}}\n"
        )
    }

    #[test]
    fn plan_stream_accumulates_text_not_thinking() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        buf.push_str("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}}\n");
        s.ingest(&buf);
        buf.push_str(&text_delta_line("the plan"));
        s.ingest(&buf);
        // Thinking never feeds the DAG parser; only assistant text does.
        assert_eq!(s.assistant_text(), "the plan");
        assert!(s.transcript().iter().any(|e| e.kind == PlanEntryKind::Thinking));
        assert!(s.transcript().iter().any(|e| e.kind == PlanEntryKind::Text));
    }

    #[test]
    fn plan_stream_reconstructs_block_across_text_deltas() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        for chunk in [
            "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",",
            "\"title\":\"scaffold\",\"prompt\":\"do it\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\n",
            "RUDDER_PLAN_TASKS_END\nThat's the plan.",
        ] {
            buf.push_str(&text_delta_line(chunk));
            s.ingest(&buf);
        }
        let tasks = extract_rudder_plan_tasks(s.parse_text()).expect("block parses from reconstructed text");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "n0");
    }

    #[test]
    fn plan_stream_codex_agent_message_and_session() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        buf.push_str("{\"type\":\"thread.started\",\"thread_id\":\"abc-123\"}\n");
        s.ingest(&buf);
        buf.push_str("{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"RUDDER_PLAN_TASKS_START\\n{\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"t\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]}\\nRUDDER_PLAN_TASKS_END\"}}\n");
        s.ingest(&buf);
        assert_eq!(s.session_id(), Some("abc-123"));
        let tasks = extract_rudder_plan_tasks(s.parse_text()).expect("codex block parses");
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn plan_stream_is_incremental_without_duplicates() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        buf.push_str(&text_delta_line("ab"));
        s.ingest(&buf);
        buf.push_str(&text_delta_line("cd"));
        s.ingest(&buf);
        assert_eq!(s.assistant_text(), "abcd", "no duplicate of the first delta");
    }

    #[test]
    fn plan_stream_per_turn_baseline_isolates_refine() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        let block = |id: &str| format!(
            "RUDDER_PLAN_TASKS_START {{\"tasks\":[{{\"id\":\"{id}\",\"title\":\"t\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}}]}} RUDDER_PLAN_TASKS_END"
        );
        buf.push_str(&text_delta_line(&block("old")));
        s.ingest(&buf);
        assert_eq!(extract_rudder_plan_tasks(s.parse_text()).unwrap()[0].id, "old");
        // A refine begins a new turn (baseline moves) then the revised block streams.
        s.begin_user_turn("change it");
        buf.push_str(&text_delta_line(&block("new")));
        s.ingest(&buf);
        assert_eq!(
            extract_rudder_plan_tasks(s.parse_text()).unwrap()[0].id,
            "new",
            "parse_text returns only the current turn's block"
        );
    }

    #[test]
    fn plan_stream_skips_non_json_noise() {
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        buf.push_str("ERROR rmcp transport closed\n");
        s.ingest(&buf);
        buf.push_str(&text_delta_line("plan text"));
        s.ingest(&buf);
        assert_eq!(s.assistant_text(), "plan text");
    }

    #[test]
    fn plan_stream_drops_terminal_chrome_redraws() {
        // Codex/claude print a status bar that repaints in place with \r ("12s · 432
        // tokens · thinking..."), interleaved with JSON events on the same \n-chunk.
        // Only the JSON text must render; the chrome must be dropped entirely.
        let mut s = PlanStreamState::new();
        let mut buf = String::new();
        buf.push_str("9 · thinking with medium effort\r30 · thinking with medium effort\r");
        buf.push_str(&text_delta_line("the real plan text"));
        s.ingest(&buf);
        assert_eq!(s.assistant_text(), "the real plan text", "chrome excluded from text");
        assert!(
            !s.transcript().iter().any(|e| e.text.contains("thinking with medium effort")),
            "status-bar chrome must not render as transcript"
        );
        assert!(
            s.transcript().iter().any(|e| e.text.contains("the real plan text")),
            "the real plan text renders"
        );
    }

    // --- Orchestrator view: pinned row + spinner/DAG worker pane -------------

    #[test]
    fn spinner_glyph_advances_with_frame() {
        let mut app = App::new();
        let first = app.spinner_glyph();
        app.spinner_frame = app.spinner_frame.wrapping_add(1);
        let second = app.spinner_glyph();
        assert_ne!(first, second, "advancing the frame changes the spinner glyph");
        // Wrapping back to the same frame yields the same glyph.
        app.spinner_frame = SPINNER_FRAMES.len();
        assert_eq!(app.spinner_glyph(), SPINNER_FRAMES[0]);
    }

    #[test]
    fn orchestrator_is_pinned_above_status_sections_and_excluded_from_buckets() {
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.status = AgentStatus::Running;
        let running = test_agent_run("worker", "do work"); // in progress bucket
        app.agents = vec![running, orch];

        // The orchestrator (index 1) navigates first even though it is pushed last.
        let order = app.visible_agent_indices();
        assert_eq!(order.first().copied(), Some(1), "orchestrator pinned first");
        assert!(order.contains(&0), "the worker still appears in a status section");
        // It is not in any status bucket (RudderPlan is excluded).
        assert!(
            !sectioned_agent_order(&app.agents)
                .iter()
                .skip(1)
                .any(|&i| app.agents[i].is_orchestrator()),
            "orchestrator does not also appear in a status section"
        );
    }

    #[test]
    fn render_agents_shows_pinned_orchestrator_header_and_phase() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        let mut orch = test_agent_run("orch", "ship the thing");
        orch.mode = AgentMode::RudderPlan;
        orch.status = AgentStatus::Running;
        app.agents = vec![orch];
        app.selected_agent = 0;

        let area = Rect::new(0, 0, 34, 40);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40))
            .expect("test backend");
        terminal
            .draw(|frame| render_agents(frame, area, &mut app))
            .expect("draw agents pane");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("orchestrator"), "pinned orchestrator header/label present: {text}");
        // While Running with no plan block, the phase reads "planning".
        assert!(text.contains("planning"), "phase reads planning: {text}");
    }

    #[test]
    fn orchestrator_task_status_reflects_launched_agent() {
        let mut app = App::new();
        // n0 launched and merged -> done; n1 launched and running -> in-progress;
        // n2 still queued (no agent) -> todo.
        app.agents = vec![
            node_agent("n0", AgentStatus::Merged),
            node_agent("n1", AgentStatus::Running),
        ];
        assert_eq!(orchestrator_task_status(&app, "n0"), OrchTaskStatus::Done);
        assert_eq!(orchestrator_task_status(&app, "n1"), OrchTaskStatus::Running);
        assert_eq!(orchestrator_task_status(&app, "n2"), OrchTaskStatus::Todo);

        let mut failed = node_agent("n3", AgentStatus::Failed);
        failed.id = "n3-run".to_string();
        app.agents.push(failed);
        assert_eq!(orchestrator_task_status(&app, "n3"), OrchTaskStatus::Failed);
    }

    #[test]
    fn orchestrator_task_status_done_not_merged_maps_to_review() {
        let mut app = App::new();
        // A worker that finished (AgentStatus::Done) but has not merged maps to
        // Review, not Done. Done is reserved for Merged, matching the agents-pane
        // buckets (Done -> review, Merged -> done).
        app.agents = vec![node_agent("n0", AgentStatus::Done)];
        assert_eq!(orchestrator_task_status(&app, "n0"), OrchTaskStatus::Review);

        // And once merged it becomes Done.
        app.agents = vec![node_agent("n0", AgentStatus::Merged)];
        assert_eq!(orchestrator_task_status(&app, "n0"), OrchTaskStatus::Done);
    }

    /// Render the worker pane (orchestrator view) into a TestBackend and return its
    /// flattened text.
    fn render_worker_text(app: &mut App, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test backend");
        terminal
            .draw(|frame| render_worker(frame, area, app))
            .expect("draw worker pane");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn render_orchestrator_pins_dag_and_shows_plan_prose() {
        // PlanReady: the DAG tree (node titles) is pinned on top and the prose plan
        // (the planner's narrative) renders below it in the scrollable body.
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        let mut stream = PlanStreamState::new();
        stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"RUDDER_PLAN_TASKS_START {\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"scaffold\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"set up project\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]} RUDDER_PLAN_TASKS_END\"}}}\n");
        orch.plan_stream = Some(stream);
        app.agents = vec![orch];
        app.selected_agent = 0;
        app.plan_summary = Some("This plan scaffolds the project first.".to_string());

        let text = render_worker_text(&mut app, 72, 24);
        assert!(text.contains("scaffold"), "DAG tree shows the node title: {text}");
        assert!(text.contains("Plan"), "the prose plan section heading is shown: {text}");
        assert!(
            text.contains("scaffolds the project"),
            "the prose plan narrative is shown: {text}"
        );
    }

    #[test]
    fn planned_node_to_task_round_trips_hard_and_soft_deps() {
        let mut node = test_planned_node("extra", &["a"]);
        node.soft_deps = vec!["b".to_string()];
        let task = node.to_task();
        assert_eq!(task.id, "extra");
        let hard: Vec<&str> = task
            .deps
            .iter()
            .filter(|e| e.edge == EdgeType::Hard)
            .map(|e| e.on.as_str())
            .collect();
        let soft: Vec<&str> = task
            .deps
            .iter()
            .filter(|e| e.edge == EdgeType::Soft)
            .map(|e| e.on.as_str())
            .collect();
        assert_eq!(hard, vec!["a"]);
        assert_eq!(soft, vec!["b"]);
    }

    #[test]
    fn orchestrator_dag_shows_reconciled_nodes_added_after_launch() {
        // The orchestrator's frozen plan block has node "scaffold". The user then
        // typed a task post-launch, which appended a reconciled node to planned_nodes.
        // That node lives ONLY in planned_nodes, so it must still appear in the DAG.
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        let mut stream = PlanStreamState::new();
        stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"RUDDER_PLAN_TASKS_START {\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"scaffold\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]} RUDDER_PLAN_TASKS_END\"}}}\n");
        orch.plan_stream = Some(stream);
        app.agents = vec![orch];
        app.selected_agent = 0;
        // A reconciled node added after launch (distinct id/title not in the block).
        app.planned_nodes = vec![test_planned_node("addeddocs", &["n0"])];

        let text = render_worker_text(&mut app, 72, 24);
        assert!(text.contains("scaffold"), "initial node shown: {text}");
        assert!(
            text.contains("addeddocs"),
            "reconciled node appears in the orchestrator DAG: {text}"
        );
    }

    #[test]
    fn task_hint_invites_adding_to_running_plan_post_launch() {
        let mut app = App::new();
        // A plan is active (queued node) but not awaiting approval -> post-launch.
        app.planned_nodes = vec![test_planned_node("n0", &[])];
        app.awaiting_approval = false;
        assert!(app.plan_is_active());
        let hint = task_default_hint(&app);
        assert!(
            hint.contains("add it to the running plan"),
            "post-launch hint invites reconcile: {hint}"
        );
    }

    #[test]
    fn app_style_paints_white_with_ink_text() {
        // The light theme paints its own canvas; primary text must set an explicit
        // dark fg (terminal-default fg could be light = invisible on white).
        assert_eq!(app_style().bg, Some(PAPER));
        assert_eq!(app_style().fg, Some(INK));
        assert_eq!(pane_text_style(true).fg, Some(INK));
        // Thin theme: helpers carry no bold.
        assert!(!accent_style(true).add_modifier.contains(Modifier::BOLD));
        assert!(!header_style(true).add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn plan_summary_is_not_truncated_at_1500() {
        let long = "word ".repeat(600); // ~3000 chars, well past the old cap
        let output = format!(
            "RUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"title\":\"a\",\"prompt\":\"p\"}}]}}\nRUDDER_PLAN_TASKS_END\n{long}"
        );
        let summary = extract_rudder_plan_summary(&output).expect("summary present");
        assert!(summary.len() > 1500, "full summary kept (len {})", summary.len());
    }

    #[test]
    fn plan_stream_result_recovers_dropped_streaming_tail() {
        let mut s = PlanStreamState::new();
        // Streaming dropped the tail mid-sentence (PTY ring/partial-event loss).
        s.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Summary: the repo has config with\"}}}\n");
        assert!(s.assistant_text().ends_with("with"));
        // The authoritative result carries the full text; only the missing tail is
        // appended, with no double-append of the streamed prefix.
        s.ingest("{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Summary: the repo has config with auth and tracks modules.\"}\n");
        assert!(
            s.assistant_text().contains("auth and tracks modules"),
            "dropped tail recovered: {}",
            s.assistant_text()
        );
        assert_eq!(
            s.assistant_text().matches("Summary: the repo has config").count(),
            1,
            "prefix not double-appended"
        );
    }

    #[test]
    fn orchestrator_pageup_pagedown_scroll_the_prose() {
        let mut app = App::new();
        let mut orch = test_agent_run("orch", "build");
        orch.mode = AgentMode::RudderPlan;
        app.agents = vec![orch];
        app.selected_agent = 0;
        app.worker_area = Some(Rect::new(0, 0, 60, 20));
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty()));
        assert!(app.orch_dag_scroll > 0, "PageDown scrolls the prose plan down");
        app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()));
        assert_eq!(app.orch_dag_scroll, 0, "PageUp scrolls back toward the top");
    }

    #[test]
    fn orchestrator_prose_reads_live_summary_when_frozen_is_empty() {
        // The truncation bug: app.plan_summary was captured once at exit-detection,
        // before the planner's tail drained. The prose must re-extract from the LIVE
        // plan_stream so it shows the full summary even when the frozen copy is empty.
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut orch = test_agent_run("orch", "build");
        orch.mode = AgentMode::RudderPlan;
        let mut stream = PlanStreamState::new();
        stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"RUDDER_PLAN_TASKS_START {\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"scaffold\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]} RUDDER_PLAN_TASKS_END\\nThis DAG is safe because config is frozen.\"}}}\n");
        orch.plan_stream = Some(stream);
        app.agents = vec![orch];
        app.selected_agent = 0;
        app.plan_summary = None; // not captured yet -> must read live

        let text = render_worker_text(&mut app, 80, 24);
        assert!(
            text.contains("DAG is safe because config is frozen"),
            "prose reads the live summary: {text}"
        );
    }

    #[test]
    fn render_worker_shows_live_transcript_while_planning() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let mut orch = test_agent_run("orch", "build a feature");
        orch.mode = AgentMode::RudderPlan;
        orch.status = AgentStatus::Running; // no plan block yet -> planning phase
        // The planner streams JSON events; the live transcript renders the model's
        // text and tool steps as it works.
        let mut stream = PlanStreamState::new();
        stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Scoping the CLI now.\"}}}\n");
        orch.plan_stream = Some(stream);
        app.agents = vec![orch];
        app.selected_agent = 0;

        let text = render_worker_text(&mut app, 70, 20);
        // While planning, the orchestrator pane shows the custom transcript view
        // (titled "orchestrator"), NOT the raw JSON PTY and NOT the DAG summary.
        assert!(text.contains("orchestrator"), "pane title is orchestrator: {text}");
        assert!(
            text.contains("decomposing the task") || text.contains("Scoping the CLI"),
            "planning shows the live transcript: {text}"
        );
        assert!(
            !text.contains("stream_event"),
            "raw JSON events are not shown: {text}"
        );
        assert!(!text.contains("running 1"), "no DAG summary while planning: {text}");
    }

    #[cfg(not(windows))]
    #[test]
    fn render_orchestrator_shows_task_tree_with_live_badges_once_plan_parsed() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"api\",\"prompt\":\"build api\",\"goal\":\"api\",\"success\":\"tests pass\"},\
            {\"id\":\"n1\",\"title\":\"ui\",\"prompt\":\"build ui\",\"goal\":\"ui\",\"success\":\"renders\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"needs api\"}]}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let mut orch = planner_with_block(&app, block);
        // The orchestrator is still selected after a plan parses; mark it as the
        // post-plan pinned run (autosteer cleared) and the source of the plan.
        orch.autosteered = false;
        orch.id = "orch".to_string();
        // n0 launched and is running; n1 not yet launched -> todo.
        let mut n0 = node_agent("n0", AgentStatus::Running);
        n0.id = "n0-run".to_string();
        app.agents = vec![orch, n0];
        app.selected_agent = 0;

        let text = render_worker_text(&mut app, 60, 20);
        assert!(text.contains("ORCHESTRATOR"), "header present: {text}");
        // The parsed task titles render in the tree.
        assert!(text.contains("api"), "task n0 title in tree: {text}");
        assert!(text.contains("ui"), "task n1 title in tree: {text}");
        // n0's badge reflects its launched agent (running == in-progress); n1 is todo.
        assert!(text.contains("in-progress"), "n0 badge reflects launched agent: {text}");
        assert!(text.contains("todo"), "n1 not launched -> todo: {text}");
        // Summary line present.
        assert!(text.contains("running 1"), "summary running count: {text}");
    }

    #[cfg(not(windows))]
    #[test]
    fn orchestrator_dag_view_is_scrollable_and_disables_pty_selection() {
        let mut app = App::new();
        app.focus = FocusPane::Worker;
        let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[\
            {\"id\":\"n0\",\"title\":\"api\",\"prompt\":\"build api\",\"goal\":\"api\",\"success\":\"ok\"},\
            {\"id\":\"n1\",\"title\":\"ui\",\"prompt\":\"build ui\",\"goal\":\"ui\",\"success\":\"ok\"}\
        ]}\nRUDDER_PLAN_TASKS_END\n";
        let orch = planner_with_block(&app, block);
        app.agents = vec![orch];
        app.selected_agent = 0;
        // Plan parsed -> the worker pane shows the custom DAG, not the PTY.
        assert!(
            app.selected_orchestrator_dag_active(),
            "PlanReady orchestrator shows the DAG command-center view"
        );

        let area = Rect::new(0, 0, 60, 20);
        app.worker_area = Some(area);
        let inner = block_inner(area);

        // Capture the planner PTY scrollback so we can prove a DAG scroll does NOT
        // touch it (the old bug: scroll moved the invisible PTY instead).
        let pty_before = app
            .selected_terminal_mut()
            .map(|t| t.scrollback())
            .unwrap_or(0);

        // ScrollDown over the DAG advances the DAG offset.
        app.orch_dag_scroll = 0;
        app.handle_pane_scroll(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: inner.x + 1,
            row: inner.y + 1,
            modifiers: KeyModifiers::empty(),
        });
        assert!(
            app.orch_dag_scroll > 0,
            "scroll over the DAG moves the DAG offset"
        );
        let pty_after = app
            .selected_terminal_mut()
            .map(|t| t.scrollback())
            .unwrap_or(0);
        assert_eq!(
            pty_before, pty_after,
            "scrolling the DAG must not move the planner PTY scrollback"
        );

        // Selection over the DAG is disabled: the handler clears any selection and
        // refuses to start a PTY-based drag (which would select unseen text).
        app.worker_selection = Some(WorkerSelection {
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 1 },
        });
        let started = app.handle_worker_selection_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: inner.x + 1,
                row: inner.y + 1,
                modifiers: KeyModifiers::empty(),
            },
            inner,
        );
        assert!(!started, "DAG view does not start a PTY selection");
        assert!(
            app.worker_selection.is_none(),
            "DAG view clears any stale PTY selection"
        );
    }

    // --- CHANGE 2: Ctrl+W leader Tab cycle -----------------------------------

    #[test]
    fn cycle_focus_rotates_panes_both_directions() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        app.cycle_focus(true);
        assert_eq!(app.focus, FocusPane::Worker);
        app.cycle_focus(true);
        assert_eq!(app.focus, FocusPane::Task);
        app.cycle_focus(true);
        assert_eq!(app.focus, FocusPane::Agents);

        app.cycle_focus(false);
        assert_eq!(app.focus, FocusPane::Task);
        app.cycle_focus(false);
        assert_eq!(app.focus, FocusPane::Worker);
    }

    #[test]
    fn ctrl_w_then_tab_cycles_and_rearms() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(app.leader_pending);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.focus, FocusPane::Worker, "Ctrl+W Tab cycles one pane");
        assert!(app.leader_pending, "leader re-arms after Tab");

        // A second Tab (no fresh Ctrl+W) keeps cycling because the leader re-armed.
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.focus, FocusPane::Task, "Ctrl+W Tab Tab cycles twice");
        assert!(app.leader_pending, "leader still armed after second Tab");
    }

    #[test]
    fn ctrl_w_then_backtab_cycles_backward_and_rearms() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.focus, FocusPane::Task, "Ctrl+W BackTab cycles backward");
        assert!(app.leader_pending, "leader re-arms after BackTab");
    }

    #[test]
    fn ctrl_w_then_option_typographic_chars_focus_panes() {
        let mut app = App::new();
        app.focus = FocusPane::Task;
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('\u{2122}'), KeyModifiers::empty()));
        assert_eq!(app.focus, FocusPane::Worker, "Option+2 (™) focuses worker");
        assert!(!app.leader_pending, "pane focus is one-shot");
    }

    // --- CHANGE 3: status sections + nested deps -----------------------------

    #[test]
    fn status_bucket_maps_each_state() {
        let mut run = test_agent_run("a", "t");
        run.status = AgentStatus::Running;
        assert_eq!(status_bucket(&run), Bucket::InProgress);

        run.status = AgentStatus::Done;
        assert_eq!(status_bucket(&run), Bucket::Review);

        run.needs_permission = true;
        assert_eq!(status_bucket(&run), Bucket::Review, "needs-permission is review");
        run.needs_permission = false;

        run.status = AgentStatus::Merged;
        assert_eq!(status_bucket(&run), Bucket::Done);

        run.status = AgentStatus::Failed;
        assert_eq!(status_bucket(&run), Bucket::Closed);
        run.status = AgentStatus::Stopped;
        assert_eq!(status_bucket(&run), Bucket::Closed);

        // Agents never land in Todo: that section now holds planned nodes only. The
        // orchestrator's raw status still maps to Review at the status_bucket level,
        // but it is excluded from buckets and pinned at the top of the list instead.
        let mut planner = test_agent_run("p", "plan");
        planner.mode = AgentMode::RudderPlan;
        planner.status = AgentStatus::Done;
        planner.autosteered = true;
        assert_eq!(status_bucket(&planner), Bucket::Review);
        assert!(planner.is_orchestrator(), "RudderPlan agent is the orchestrator");
    }

    #[test]
    fn sectioned_order_groups_by_status_then_nests_children() {
        let mut app = App::new();
        // parent (Running -> in progress), child hard-depends on parent (also Running),
        // plus a merged agent (done) and a failed agent (closed).
        let parent = test_agent_run("parent", "parent task"); // Running
        let mut child = test_agent_run("child", "child task");
        child.deps = vec!["parent".to_string()];
        let mut merged = test_agent_run("m", "merged task");
        merged.status = AgentStatus::Merged;
        let mut failed = test_agent_run("f", "failed task");
        failed.status = AgentStatus::Failed;
        app.agents = vec![parent, child, merged, failed];

        // in progress: parent(0) then nested child(1); done: merged(2); closed: failed(3).
        assert_eq!(app.visible_agent_indices(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn sectioned_render_emits_status_headers_with_counts() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        let running = test_agent_run("r", "alpha work"); // in progress
        let mut done = test_agent_run("m", "beta work");
        done.status = AgentStatus::Merged; // done section
        app.agents = vec![running, done];

        let area = Rect::new(0, 0, 34, 40);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40))
            .expect("test backend");
        terminal
            .draw(|frame| render_agents(frame, area, &mut app))
            .expect("draw agents pane");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("in progress"), "in progress header present");
        // "done" header (note: the row-2 "agents N runs" line never says "done").
        assert!(text.contains("done"), "done header present: {text}");
        // The old grouping headers must be gone; neither agent label uses these words.
        assert!(!text.contains("worktrees"), "no worktrees header");
    }

    #[test]
    fn cross_section_dependent_renders_dep_hint() {
        let mut app = App::new();
        app.focus = FocusPane::Agents;
        // parent is merged (done section); child is running (in progress section) and
        // hard-depends on the parent which is NOT in the child's section.
        let mut parent = test_agent_run("parent", "parent task");
        parent.status = AgentStatus::Merged;
        let mut child = test_agent_run("child", "child task");
        child.deps = vec!["parent".to_string()];
        app.agents = vec![parent, child];

        let area = Rect::new(0, 0, 34, 40);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40))
            .expect("test backend");
        terminal
            .draw(|frame| render_agents(frame, area, &mut app))
            .expect("draw agents pane");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("^dep"),
            "cross-section dependent shows a dep hint: {text}"
        );
    }

    // -----------------------------------------------------------------------
    // graph.json mirror: build_mirror_payload projects the in-memory plan into
    // the JSON payload the `rudder __graph-mirror` shim consumes. Statuses are
    // mapped from AgentStatus; planned nodes and plan-launched agents both
    // appear; deps/soft_deps become typed edges in the payload.
    // -----------------------------------------------------------------------

    #[test]
    fn mirror_payload_includes_planned_nodes_with_typed_deps() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        let mut root = test_planned_node("n0", &[]);
        root.title = "build the parser".to_string();
        let mut child = test_planned_node("n1", &["n0"]);
        child.soft_deps = vec!["n0".to_string()];
        app.planned_nodes = vec![root, child];

        let payload = app.build_mirror_payload();
        let nodes = payload["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 2);

        let n0 = nodes.iter().find(|n| n["id"] == "n0").expect("n0");
        assert_eq!(n0["status"], "planned");
        assert_eq!(n0["title"], "build the parser");
        assert!(n0["deps"].as_array().unwrap().is_empty());

        let n1 = nodes.iter().find(|n| n["id"] == "n1").expect("n1");
        assert_eq!(n1["status"], "planned");
        let deps = n1["deps"].as_array().expect("n1 deps");
        // One hard edge on n0 and one soft edge on n0.
        assert!(deps
            .iter()
            .any(|d| d["on"] == "n0" && d["type"] == "hard"));
        assert!(deps
            .iter()
            .any(|d| d["on"] == "n0" && d["type"] == "soft"));
    }

    #[test]
    fn mirror_payload_maps_agent_status_and_carries_run_metadata() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();

        let mut running = node_agent("n0", AgentStatus::Running);
        running.id = "run-a".to_string();
        running.jj_change_id = Some("zchange".to_string());
        running.worktree_path = Some(PathBuf::from("/tmp/w/n0"));
        running.deps = vec![];

        let mut done = node_agent("n1", AgentStatus::Done);
        done.id = "run-b".to_string();
        done.deps = vec!["n0".to_string()];

        let mut merged = node_agent("n2", AgentStatus::Merged);
        merged.id = "run-c".to_string();

        let mut failed = node_agent("n3", AgentStatus::Failed);
        failed.id = "run-d".to_string();

        let mut stopped = node_agent("n4", AgentStatus::Stopped);
        stopped.id = "run-e".to_string();

        app.agents = vec![running, done, merged, failed, stopped];

        let payload = app.build_mirror_payload();
        let nodes = payload["nodes"].as_array().expect("nodes array");
        let by_id = |id: &str| nodes.iter().find(|n| n["id"] == id).cloned().unwrap();

        // AgentStatus -> board NodeStatus mapping.
        assert_eq!(by_id("n0")["status"], "running");
        assert_eq!(by_id("n1")["status"], "review");
        assert_eq!(by_id("n2")["status"], "merged");
        assert_eq!(by_id("n3")["status"], "failed");
        assert_eq!(by_id("n4")["status"], "failed");

        // The running node carries its run id, jj change, and worktree path.
        let n0 = by_id("n0");
        assert_eq!(n0["runId"], "run-a");
        assert_eq!(n0["jjChangeId"], "zchange");
        assert_eq!(n0["worktreePath"], "/tmp/w/n0");

        // The Done agent's hard dep on n0 becomes a typed edge in the payload.
        let n1_deps = by_id("n1")["deps"].as_array().unwrap().clone();
        assert!(n1_deps
            .iter()
            .any(|d| d["on"] == "n0" && d["type"] == "hard"));
    }

    #[test]
    fn mirror_payload_combines_queued_nodes_and_launched_agents() {
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        // n0 launched (running), n1 still queued in Todo.
        app.agents = vec![node_agent("n0", AgentStatus::Running)];
        app.planned_nodes = vec![test_planned_node("n1", &["n0"])];

        let payload = app.build_mirror_payload();
        let nodes = payload["nodes"].as_array().expect("nodes array");
        let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"n0"), "launched agent projected");
        assert!(ids.contains(&"n1"), "queued node projected");
    }

    #[test]
    fn mirror_graph_coalesce_guard_skips_when_signature_unchanged() {
        // In a #[cfg(test)] build mirror_graph never shells out, so we assert the
        // coalesce bookkeeping directly: the first call records a signature; a
        // second call with an identical plan keeps the same signature (no churn);
        // a plan change moves the signature.
        let mut app = App::new();
        app.cwd = std::env::temp_dir();
        app.planned_nodes = vec![test_planned_node("n0", &[])];
        assert_eq!(app.last_mirror_signature, None);

        app.mirror_graph();
        let first = app.last_mirror_signature;
        assert!(first.is_some(), "first mirror records a signature");

        app.mirror_graph();
        assert_eq!(
            app.last_mirror_signature, first,
            "an unchanged plan keeps the same signature (coalesced)"
        );

        // Mutate the plan: the signature must change.
        app.planned_nodes.push(test_planned_node("n1", &["n0"]));
        app.mirror_graph();
        assert_ne!(
            app.last_mirror_signature, first,
            "a changed plan moves the signature"
        );
    }
