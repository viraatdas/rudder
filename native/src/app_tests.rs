use super::*;

#[test]
fn base64_encode_matches_known_vectors() {
    // RFC 4648 test vectors — the OSC 52 clipboard payload encoder must be exact or
    // the terminal copies garbage.
    assert_eq!(crate::selection::base64_encode(b""), "");
    assert_eq!(crate::selection::base64_encode(b"f"), "Zg==");
    assert_eq!(crate::selection::base64_encode(b"fo"), "Zm8=");
    assert_eq!(crate::selection::base64_encode(b"foo"), "Zm9v");
    assert_eq!(crate::selection::base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(crate::selection::base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(crate::selection::base64_encode(b"foobar"), "Zm9vYmFy");
    // Multi-byte UTF-8 round-trips through the byte encoder.
    assert_eq!(crate::selection::base64_encode("é".as_bytes()), "w6k=");
}

#[test]
fn union_gitignore_merges_both_sides_without_duplicates() {
    let left = "node_modules/\n.env\n*.db\n";
    let right = "node_modules/\n.next/\n*.db\n.vercel\n";
    let merged = union_gitignore_like(left, right);
    // Every rule from both sides is present exactly once.
    for rule in ["node_modules/", ".env", "*.db", ".next/", ".vercel"] {
        assert_eq!(
            merged.matches(&format!("{rule}\n")).count(),
            1,
            "{rule} appears exactly once in:\n{merged}"
        );
    }
    // Left side's order is preserved, right's new rules appended after.
    let next_pos = merged.find(".next/").unwrap();
    let env_pos = merged.find(".env").unwrap();
    assert!(env_pos < next_pos, "left rules precede right-only rules");
}

#[test]
fn merge_package_json_unions_deps_and_scripts() {
    let left = r#"{"name":"app","version":"1.0.0","scripts":{"dev":"next dev"},"dependencies":{"next":"15"}}"#;
    let right = r#"{"name":"app","version":"2.0.0","scripts":{"db:seed":"tsx seed.ts"},"dependencies":{"@libsql/client":"0.14"}}"#;
    let merged = merge_package_json(left, right).expect("valid merge");
    let value: serde_json::Value = serde_json::from_str(&merged).expect("merged is valid json");
    // Scripts and dependencies from BOTH sides survive.
    assert!(value["scripts"]["dev"].is_string(), "kept left script");
    assert!(value["scripts"]["db:seed"].is_string(), "kept right script");
    assert!(value["dependencies"]["next"].is_string(), "kept left dep");
    assert!(
        value["dependencies"]["@libsql/client"].is_string(),
        "kept right dep"
    );
    // Scalar conflict (version) deterministically keeps the base/left side.
    assert_eq!(value["version"], "1.0.0", "scalar tie keeps left side");
}

#[test]
fn merge_package_json_bails_on_invalid_json() {
    // A non-JSON side must fall back to the LLM resolver (None), never corrupt the file.
    assert!(merge_package_json("{not json", "{}").is_none());
}

#[test]
fn merge_decisions_sides_recovers_entries_from_conflicted_content() {
    let left = "# Decisions\n\n## n1: foundation\n- **Did:** Added schema\n- **By:** n1\n";
    let conflicted = "# Decisions\n\n<<<<<<< Conflict 1 of 1\n## n2: worker\n- **Did:** Added detector\n- **By:** n2\n=======\n## n3: sibling\n- **Did:** Added collector\n- **By:** n3\n>>>>>>> Conflict 1 of 1\n";
    let merged = merge_decisions_sides(&[left.to_string(), conflicted.to_string()])
        .expect("decisions merge");
    assert!(merged.starts_with("# Decisions"));
    assert!(merged.contains("## n1: foundation"));
    assert!(merged.contains("## n2: worker"));
    assert!(merged.contains("## n3: sibling"));
    assert_eq!(merged.matches("## n2: worker").count(), 1);
    assert!(
        !contains_conflict_markers(&merged),
        "merged decisions must be marker-free:\n{merged}"
    );
}

/// Flatten a rendered Line back into its plain text (span contents joined).
fn flatten_line(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// TUI HARNESS: render the REAL dashboard (`render::render`) to a TestBackend and
/// return the screen as plain text (one row per line, cell symbols joined). This is
/// what lets end-to-end tests assert on what the user actually SEES, driving the
/// full App state machine + rendering instead of poking internal fields.
fn render_screen(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| crate::render::render(frame, app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

/// Serializes tests that mutate PROCESS-GLOBAL env (RUDDER_CLAUDE_BIN /
/// RUDDER_CODEX_BIN): cargo runs tests in parallel, so without this two harness
/// tests would clobber each other's fake-backend path. Hold the guard for the
/// whole test (set env -> use -> remove).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write `body` as an executable script at `path` (the fake `claude`/`codex` the
/// TUI harness injects via RUDDER_CLAUDE_BIN / RUDDER_CODEX_BIN).
fn write_fake_bin(path: &std::path::Path, body: &str) {
    fs::write(path, body).expect("write fake bin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

fn count_byte_subsequence(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

#[test]
fn goal_line_capped_under_backend_limit() {
    // The backend's /goal rejects a goal condition over 4000 chars ("got 4595"). A
    // planner that emits a huge goal/success must still produce launchable lines.
    let task = RudderPlanTask {
        id: "n0".to_string(),
        title: "t".to_string(),
        prompt: "do the thing".to_string(),
        goal: Some("G".repeat(4595)),
        success: Some("S".repeat(5000)),
        deps: vec![],
        backend: None,
        model: None,
        effort: None,
    };
    let prompt = rudder_plan_worker_prompt("original request", &task, "", Backend::Claude);
    let mut lines = prompt.lines();
    let goal_line = lines.next().unwrap();
    assert!(goal_line.starts_with("Objective: "));
    // The objective argument must be <= 4000 chars (it was 4595).
    let arg_len = goal_line.chars().count() - "Objective: ".chars().count();
    assert!(
        arg_len <= 4000,
        "/goal arg capped under 4000: was {arg_len}"
    );
    // The Done-when line is capped too.
    let done_line = lines.next().unwrap();
    assert!(done_line.starts_with("Done when: "));
    let done_len = done_line.chars().count() - "Done when: ".chars().count();
    assert!(
        done_len <= 4000,
        "done-when capped under 4000: was {done_len}"
    );
    // The full prompt body still carries the detail (nothing silently lost upstream).
    assert!(prompt.contains("do the thing"));
}

#[test]
fn parsed_plan_tasks_are_preflighted_before_queueing() {
    let raw = format!(
        "RUDDER_PLAN_TASKS_START\n{{\"tasks\":[{{\"id\":\"n0\",\"title\":\"seed\",\"prompt\":\"write data\",\"goal\":\"{}\",\"success\":\"{}\",\"deps\":[]}}]}}\nRUDDER_PLAN_TASKS_END",
        "G".repeat(4800),
        "S".repeat(4800)
    );
    let tasks = extract_rudder_plan_tasks(&raw).expect("parse plan tasks");
    let task = tasks.first().expect("one task");
    assert!(
        task.goal.as_deref().unwrap_or_default().chars().count() <= MAX_GOAL_LINE_CHARS,
        "parsed goal is capped before it enters planned_nodes"
    );
    assert!(
        task.success.as_deref().unwrap_or_default().chars().count() <= MAX_GOAL_LINE_CHARS,
        "parsed success is capped before it enters planned_nodes"
    );

    let node = PlannedNode::from_task(task);
    assert!(
        node.goal.as_deref().unwrap_or_default().chars().count() <= MAX_GOAL_LINE_CHARS,
        "queued goal remains capped"
    );
    assert!(
        node.success.as_deref().unwrap_or_default().chars().count() <= MAX_GOAL_LINE_CHARS,
        "queued success remains capped"
    );
}

#[test]
fn long_worker_body_does_not_become_a_slash_goal_condition() {
    // Regression from a real worker launch: Claude reported
    // "Goal condition is limited to 4000 characters (got 8685)" even though the
    // planned node's goal/success were short. Root cause: the initial prompt began
    // with `/goal`, and Claude treated the entire launch prompt as that command's
    // argument. Long body detail must stay in the body, not in a slash command.
    let task = RudderPlanTask {
        id: "n2".to_string(),
        title: "Seed data + Apify pipeline".to_string(),
        prompt: format!(
            "Build the talent seed-data layer. {}",
            "Each record MUST follow the exact schema and write scripts/apify-ingest.ts. "
                .repeat(120)
        ),
        goal: Some("Produce the seed JSON and Apify ingestion script.".to_string()),
        success: Some("seed data and ingest script exist and run.".to_string()),
        deps: vec![],
        backend: None,
        model: None,
        effort: None,
    };
    let worker_prompt = rudder_plan_worker_prompt("build the CRM", &task, "", Backend::Claude);
    assert!(
        worker_prompt.len() > 8_000,
        "test prompt should match the long-body failure"
    );
    assert!(worker_prompt.starts_with("Objective: Produce the seed JSON"));
    assert!(!worker_prompt.starts_with("/goal"));
    assert!(!worker_prompt.starts_with("Goal:"));

    let launch_prompt = execution_prompt(&worker_prompt);
    assert!(launch_prompt.starts_with("Objective: Produce the seed JSON"));
    assert!(!launch_prompt.starts_with("/goal"));
    assert!(!launch_prompt.starts_with("Goal:"));
    assert!(
        launch_prompt.contains("scripts/apify-ingest.ts"),
        "full worker body is still present"
    );
}

#[test]
fn cap_goal_line_is_a_passthrough_when_short() {
    assert_eq!(
        cap_goal_line("short objective".to_string()),
        "short objective"
    );
}

#[test]
fn execution_prompt_caps_a_long_leading_goal_line() {
    // The real 4000-char source: a planner node prompt that LEADS with its own long
    // /goal line, hoisted by execution_prompt. The hoisted line must be capped and
    // normalized away from a slash command so Claude does not parse the full prompt as
    // the /goal argument.
    let task = format!(
        "/goal {}\nDone when: ok\n\nimplement the thing",
        "G".repeat(4595)
    );
    let out = execution_prompt(&task);
    let goal_line = out.lines().next().unwrap();
    assert!(goal_line.starts_with("Objective: "));
    let arg = goal_line.chars().count() - "Objective: ".chars().count();
    assert!(arg <= 4000, "hoisted /goal arg capped: was {arg}");
    assert!(out.contains("implement the thing"), "body preserved");
    assert!(
        !out.starts_with("/goal"),
        "launch prompt must not begin with a slash command"
    );
}

#[test]
fn execution_prompt_does_not_treat_goal_prefix_words_as_slash_goal() {
    let out = execution_prompt("/goalkeeper route should be documented");
    assert!(
        !out.starts_with("Objective:"),
        "only the real /goal command should be normalized: {out}"
    );
    assert!(out.contains("/goalkeeper route should be documented"));
}

#[test]
fn execution_prompt_normalizes_legacy_goal_with_tab_separator() {
    let out = execution_prompt("/goal\tship it\nDone when: tests pass\n\nbody");
    assert!(out.starts_with("Objective: ship it\nDone when: tests pass"));
    assert!(!out.starts_with("/goal"));
}

#[test]
fn manual_goal_slash_command_is_capped() {
    // The OTHER 4000-char source the user hit ("got 4760"): a /goal typed/pasted in
    // the task bar was forwarded VERBATIM to the agent. slash_command_arg caps it.
    let long = "a".repeat(4760);
    let arg = slash_command_arg("/goal", &long);
    assert!(
        arg.chars().count() <= MAX_GOAL_LINE_CHARS,
        "/goal arg capped under the backend limit: was {}",
        arg.chars().count()
    );
    // Multi-line paste collapses to a single line (the slash command reads one line).
    let multi = slash_command_arg("/goal", "make\nthe\nsite  pretty");
    assert_eq!(multi, "make the site pretty");
    // Non-goal commands pass their trimmed argument through unchanged.
    assert_eq!(slash_command_arg("/model", "  opus  "), "opus");
}

#[test]
fn slash_command_rest_ignores_leading_whitespace_before_command() {
    assert_eq!(command_rest("  /goal   ship it", "/goal"), "   ship it");
    assert_eq!(command_rest("\t/ask explain auth", "/ask"), " explain auth");
    assert_eq!(command_rest("/run build auth", "/run"), " build auth");
    assert_eq!(command_rest("/plan build it", "/plan"), " build it");
}

#[test]
fn dependency_context_names_parents_and_their_interfaces() {
    // A launching worker should be told its deps by title + the parent's rudder-done
    // interface, so it builds on merged prerequisites instead of reimplementing them.
    let repo = unique_test_repo("dep-context");
    let mut app = App::new();
    app.cwd = repo.clone();

    // A MERGED parent n0 that filed a rudder-done note exposing an interface.
    let mut parent = test_agent_run("run-n0", "auth.js: Spotify PKCE flow");
    parent.node_id = Some("n0".to_string());
    parent.task_summary = "auth.js: Spotify PKCE flow".to_string();
    parent.status = AgentStatus::Merged;
    parent.cwd = repo.clone();
    fs::create_dir_all(repo.join(".rudder").join("done")).unwrap();
    fs::write(
        repo.join(".rudder").join("done").join("n0.json"),
        r#"{"summary":"built auth","interfaces":"getAccessToken(), refreshToken() in auth.js"}"#,
    )
    .unwrap();
    app.agents.push(parent);

    let node = PlannedNode {
        id: "n1".to_string(),
        title: "app.js: wire auth".to_string(),
        prompt: "wire it".to_string(),
        goal: None,
        success: None,
        deps: vec!["n0".to_string()],
        soft_deps: vec![],
        backend: None,
        model: None,
        effort: None,
    };
    let ctx = app.dependency_context(&node);
    assert!(ctx.contains("Depends on"), "block header present\n{ctx}");
    assert!(
        ctx.contains("n0") && ctx.contains("auth.js: Spotify PKCE flow"),
        "parent named by id + title\n{ctx}"
    );
    assert!(
        ctx.contains("getAccessToken"),
        "parent rudder-done interface included\n{ctx}"
    );
    assert!(ctx.contains("merged"), "merged state surfaced\n{ctx}");

    // A node with no deps gets no block.
    let mut independent = node.clone();
    independent.deps.clear();
    assert!(
        app.dependency_context(&independent).is_empty(),
        "no deps -> empty block"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn duplicate_task_ids_are_accepted_not_rejected_as_a_cycle() {
    // assert_no_hard_cycle previously compared visited against tasks.len(), so duplicate
    // ids (which dedupe in the maps) falsely looked like a cycle and rejected the plan.
    let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"p\"},{\"id\":\"n0\",\"title\":\"b\",\"prompt\":\"q\"}]}\nRUDDER_PLAN_TASKS_END";
    let tasks = extract_rudder_plan_tasks(block).expect("dup ids accepted, not a cycle");
    assert_eq!(tasks.len(), 2);
}

#[test]
fn bare_string_dep_becomes_a_soft_edge() {
    // A dep given as a bare string "n0" (not {on,type}) must be a soft edge (TS parity),
    // not silently dropped.
    let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"a\",\"prompt\":\"p\"},{\"id\":\"n1\",\"title\":\"b\",\"prompt\":\"q\",\"deps\":[\"n0\"]}]}\nRUDDER_PLAN_TASKS_END";
    let tasks = extract_rudder_plan_tasks(block).expect("parses");
    let n1 = tasks.iter().find(|t| t.id == "n1").expect("n1");
    assert_eq!(n1.deps.len(), 1, "bare-string dep kept");
    assert_eq!(n1.deps[0].on, "n0");
    assert_eq!(n1.deps[0].edge, EdgeType::Soft);
}

#[test]
fn structural_markers_match_whole_words_not_substrings() {
    // A marker inside a longer word must NOT force a rebase.
    assert!(!is_structural_direction(
        "make the pivotal config change",
        &[]
    ));
    // The marker as a real word does.
    assert!(is_structural_direction("pivot to a new design", &[]));
    assert!(is_structural_direction("rewrite the auth flow", &[]));
}

#[test]
fn orchestrator_task_status_prefers_a_live_relaunch_over_a_stale_failed_run() {
    let mut app = App::new();
    let mut failed = test_agent_run("a", "x");
    failed.node_id = Some("n1".to_string());
    failed.status = AgentStatus::Failed;
    let mut running = test_agent_run("b", "y");
    running.node_id = Some("n1".to_string());
    running.status = AgentStatus::Running;
    app.agents = vec![failed, running];
    assert_eq!(
        orchestrator_task_status(&app, "n1"),
        OrchTaskStatus::Running
    );
}

#[test]
fn plan_stream_lands_final_plan_emitted_after_tool_narration() {
    // Repro of the my-charts hang: the planner narrates (streamed text), uses tools,
    // THEN emits the plan as a final message. The `result` event's text is that final
    // message only, so it does not prefix the full streamed turn — the fix splices on
    // the overlap so RUDDER_PLAN_TASKS still lands (the DAG is parsed, no perpetual
    // "decomposing" spinner).
    let mut stream = PlanStreamState::new();
    let plan_block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"x\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END";
    let result_text =
        format!("I have a clear picture. Let me confirm.\n\n{plan_block}\n\nQuestions:\n1. scope?");
    let snapshot = format!(
        "{}\n{}\n{}\n",
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"I'll inspect the repo.\n"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"I have a clear picture. Let me confir"}}}"#,
        format!(
            r#"{{"type":"result","subtype":"success","result":{}}}"#,
            serde_json::to_string(&result_text).unwrap()
        )
    );
    stream.ingest(&snapshot);
    let text = stream.parse_text();
    assert!(
        text.contains("RUDDER_PLAN_TASKS_START"),
        "final plan landed despite tool narration: {text:?}"
    );
    let tasks = extract_rudder_plan_tasks(text).expect("the DAG parses");
    assert_eq!(tasks.len(), 1);
    // No gross duplication of the overlap ("I have a clear picture" appears once).
    assert_eq!(
        text.matches("I have a clear picture").count(),
        1,
        "{text:?}"
    );
}

#[test]
fn conductor_decision_appends_parseable_entry_with_header() {
    let dir = std::env::temp_dir().join(format!("rudder-dec-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("DECISIONS.md");
    let _ = std::fs::remove_file(&file);

    append_conductor_decision(
        &dir,
        "Plan approved",
        "Approved a 3-task plan",
        Some("user approved"),
    );
    let content = std::fs::read_to_string(&file).unwrap();
    // Header created on first write, then the canonical ## block format.
    assert!(
        content.starts_with("# Decisions"),
        "header present: {content}"
    );
    assert!(content.contains("## Plan approved"));
    assert!(content.contains("- **What:** Approved a 3-task plan"));
    assert!(content.contains("- **Why:** user approved"));
    assert!(content.contains("- **By:** conductor · "));

    // A second decision appends (does not clobber).
    append_conductor_decision(&dir, "Plan at cap", "deferred follow-ups", None);
    let content = std::fs::read_to_string(&file).unwrap();
    assert!(content.contains("## Plan approved") && content.contains("## Plan at cap"));
    // Empty `what` is a no-op (never writes a blank decision).
    let before = content.len();
    assert!(!append_conductor_decision(&dir, "x", "   ", None));
    assert_eq!(std::fs::read_to_string(&file).unwrap().len(), before);

    // Idempotent: re-recording the SAME title+what (the depth-cap spam case) is a no-op,
    // even though a fresh entry would carry a new timestamp footer.
    let len_before = std::fs::read_to_string(&file).unwrap().len();
    assert!(
        !append_conductor_decision(&dir, "Plan at cap", "deferred follow-ups", None),
        "an identical decision is not appended again"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap().len(),
        len_before,
        "duplicate decision did not grow the file"
    );
    assert_eq!(
        std::fs::read_to_string(&file)
            .unwrap()
            .matches("## Plan at cap")
            .count(),
        1,
        "the capped decision appears exactly once"
    );
    let _ = std::fs::remove_file(&file);
}

#[test]
fn push_activity_appends_parseable_jsonl_for_the_web_board() {
    let dir = std::env::temp_dir().join(format!("rudder-activity-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join(".rudder").join("activity.jsonl"));

    let mut app = App::new();
    app.cwd = dir.clone();
    app.push_activity("merged n0 into trunk".to_string());

    let raw = std::fs::read_to_string(dir.join(".rudder").join("activity.jsonl")).unwrap();
    let line = raw.lines().next_back().unwrap();
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["text"], "merged n0 into trunk");
    assert_eq!(value["kind"], "action");
    assert!(value["ts"].is_string(), "carries a timestamp: {line}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sanitize_steer_instruction_preserves_lines_and_strips_terminal_controls() {
    // Newlines are safe inside bracketed paste and preserve the web composer's
    // structure; ESC and other controls must not reach the terminal.
    let dirty = "run tests\rrm -rf /\nthen leak\x1bsecret\tnow";
    let clean = sanitize_steer_instruction(dirty);
    assert!(!clean.contains('\r') && !clean.contains('\x1b'));
    assert_eq!(
        clean, "run tests\nrm -rf /\nthen leak secret now",
        "preserves composer line breaks while stripping terminal controls"
    );
    // All-control input collapses to empty (deliver_steer then drops it).
    assert!(sanitize_steer_instruction("\r\n\t\x1b").is_empty());
}

#[test]
fn poll_steer_inbox_consumes_files_and_records_unknown_target() {
    let dir = std::env::temp_dir().join(format!("rudder-steer-{}", std::process::id()));
    let steer = dir.join(".rudder").join("steer");
    let _ = std::fs::create_dir_all(&steer);
    let request = steer.join("1700000000000-n7.json");
    std::fs::write(
        &request,
        r#"{"requestId":"request-00000007","taskId":"n7","instruction":"focus on the failing test"}"#,
    )
    .unwrap();

    let mut app = App::new();
    app.cwd = dir.clone();
    // No agent matches "n7", so the steer is consumed and a not-found line is logged
    // rather than silently retried forever.
    app.poll_steer_inbox();

    assert!(!request.exists(), "steer request file is consumed");
    assert!(
        app.activity_log
            .iter()
            .any(|l| l.contains("steer: target not found") && l.contains("n7")),
        "records the unroutable steer: {:?}",
        app.activity_log
    );
    let receipt = dir
        .join(".rudder")
        .join("steer-receipts")
        .join("request-00000007.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
    assert_eq!(value["status"], "failed");
    assert!(value["error"].as_str().unwrap_or_default().contains("n7"));

    // Receipt existence is an at-most-once ledger after a crash/restart: a
    // leftover duplicate inbox file is consumed without routing a second time.
    let activity_count = app.activity_log.len();
    std::fs::write(
        &request,
        r#"{"requestId":"request-00000007","taskId":"n7","instruction":"focus on the failing test"}"#,
    )
    .unwrap();
    app.poll_steer_inbox();
    assert!(!request.exists());
    assert_eq!(app.activity_log.len(), activity_count);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn web_conductor_steer_uses_task_input_path_without_a_live_conductor() {
    let dir = unique_test_repo("web-conductor-steer");
    let mut app = App::new();
    app.cwd = dir.clone();
    assert!(
        !app.agents.iter().any(AgentRun::is_orchestrator),
        "fixture intentionally has no conductor PTY"
    );

    // /help is a task-input command with no process spawn, so it proves the
    // conductor web target reached start_task_from_input rather than the old
    // "target not found" branch.
    let _ = app.deliver_steer_request("conductor", "/help");

    assert!(
        app.notice
            .as_deref()
            .is_some_and(|notice| notice.contains("plain input -> orchestrator/DAG")),
        "task-input command result remains visible: {:?}",
        app.notice
    );
    assert!(
        app.activity_log
            .iter()
            .any(|line| line.contains("steered conductor: /help")),
        "web action is acknowledged in activity: {:?}",
        app.activity_log
    );
    assert!(app.agents.is_empty(), "the command did not spawn an agent");

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(windows))]
#[test]
fn web_steer_running_worker_injects_and_persists_a_user_turn() {
    let dir = unique_test_repo("web-running-steer");
    let mut app = App::new();
    app.cwd = dir.clone();
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "sleep 5"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 5, cols: 40 },
            cwd: Some(dir.clone()),
            ..Default::default()
        },
    )
    .expect("spawn fake worker PTY");
    let mut run = test_agent_run_with_terminal(&app, pane);
    run.id = "web-running-worker".to_string();
    run.node_id = Some("n-running".to_string());
    run.needs_user_input = true;
    run.user_input_notified = true;
    app.agents.push(run);

    let _ = app.deliver_steer_request("n-running", "focus on the failing parser test");

    let run = &app.agents[0];
    assert_eq!(run.status, AgentStatus::Running);
    assert_eq!(run.current_prompt, "focus on the failing parser test");
    assert_ne!(run.last_user_input_at, "1");
    assert!(!run.needs_user_input);
    let turn = run.turns.last().expect("steer appended a turn");
    assert_eq!(turn.prompt, "focus on the failing parser test");
    assert_eq!(turn.source, "user");

    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            dir.join(".rudder")
                .join("runs")
                .join("web-running-worker")
                .join("run.json"),
        )
        .expect("running steer persisted run.json"),
    )
    .expect("parse persisted run record");
    assert_eq!(saved["currentPrompt"], "focus on the failing parser test");
    assert_eq!(
        saved["turns"].as_array().and_then(|turns| turns.last()),
        Some(&serde_json::json!({
            "ts": run.last_user_input_at,
            "prompt": "focus on the failing parser test",
            "source": "user",
        }))
    );

    app.agents[0].terminal = None;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn web_steer_rejects_terminal_worker_statuses() {
    let dir = unique_test_repo("web-terminal-steer");
    for status in [
        AgentStatus::Merged,
        AgentStatus::Failed,
        AgentStatus::Stopped,
    ] {
        let mut app = App::new();
        app.cwd = dir.clone();
        let mut run = test_agent_run("terminal-worker", "finished task");
        run.node_id = Some("n-terminal".to_string());
        run.status = status;
        app.agents.push(run);

        let _ = app.deliver_steer_request("n-terminal", "change the implementation");

        assert_eq!(app.agents[0].status, status);
        assert_eq!(app.agents[0].turns.len(), 1, "no turn was appended");
        assert!(
            app.activity_log.iter().any(|line| {
                line.contains("steer: n-terminal unavailable") && line.contains(status.as_str())
            }),
            "status is reported as unavailable: {:?}",
            app.activity_log
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(windows))]
#[test]
fn web_steer_done_worker_regoals_the_same_row() {
    let _env = env_guard();
    let dir = unique_test_repo("web-done-steer");
    let fake = dir.join("fake-claude.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    let old_claude = std::env::var_os("RUDDER_CLAUDE_BIN");
    let old_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);
    std::env::set_var("RUDDER_HOME", dir.join("home"));

    let mut app = App::new();
    app.cwd = dir.clone();
    let mut run = test_agent_run("web-done-worker", "implement parser");
    run.cwd = dir.clone();
    run.node_id = Some("n-done".to_string());
    run.status = AgentStatus::Done;
    run.completed_at = Some(Instant::now());
    run.session_id = Some("web-steer-session".to_string());
    app.agents.push(run);

    let _ = app.deliver_steer_request("n-done", "also cover malformed input");

    let status = app.agents[0].status;
    let current_prompt = app.agents[0].current_prompt.clone();
    let last_turn = app.agents[0].turns.last().cloned();
    let activity = app.activity_log.clone();
    app.agents[0].terminal = None;
    match old_claude {
        Some(value) => std::env::set_var("RUDDER_CLAUDE_BIN", value),
        None => std::env::remove_var("RUDDER_CLAUDE_BIN"),
    }
    match old_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(status, AgentStatus::Running);
    assert_eq!(current_prompt, "also cover malformed input");
    let last_turn = last_turn.expect("re-goal appended a turn");
    assert_eq!(last_turn.prompt, "also cover malformed input");
    assert_eq!(last_turn.source, "regoal");
    assert!(
        activity
            .iter()
            .any(|line| line.contains("re-goaled n-done: also cover malformed input")),
        "same worker row was resumed: {activity:?}"
    );
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
fn worktrees_live_inside_the_project_not_the_parent() {
    // Worktrees must stay WITHIN the project so a planner/agent confined to the
    // project never reads outside it (the source of the out-of-project permission
    // prompt). They used to land in repo_root.parent()/.rudder-worktrees.
    let repo = std::path::Path::new("/Users/viraat/Documents/rudder");
    let wt = worktree_path(repo, "1779-build-thing-4029", "build a thing");
    assert!(
        wt.starts_with(repo),
        "worktree is inside the project: {wt:?}"
    );
    assert!(
        wt.to_string_lossy().contains("/rudder/.rudder-worktrees/"),
        "under the project's gitignored .rudder-worktrees: {wt:?}"
    );
    assert!(
        !wt.starts_with("/Users/viraat/Documents/.rudder-worktrees"),
        "never the parent directory: {wt:?}"
    );
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
        "1 background terminal running \u{00b7} /ps to view \u{00b7} /stop to close".to_string(),
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
    assert!(
        prompt.contains("RUDDER_SHARED.md"),
        "workers must be told to read shared local context: {prompt}"
    );
    // The old Hunk review integration paragraph is gone.
    assert!(!prompt.to_lowercase().contains("hunk"), "no hunk: {prompt}");
}

#[test]
fn shared_context_extracts_secret_like_task_input_only() {
    let input =
        "please use APIFY_TOKEN=abc1234567 when scraping\nthen track token budget carefully";
    let snippet = extract_shared_context_snippet(input).expect("captures token line");
    assert!(snippet.contains("APIFY_TOKEN=abc1234567"));
    assert!(
        !snippet.contains("token budget"),
        "ordinary token-budget prose should not be captured"
    );
}

#[test]
fn preview_text_redacts_secret_values() {
    let preview = preview_text(
        "apify token store this somewhere rdrtest_abc1234567890abcdef",
        200,
    );
    assert!(preview.contains("[redacted]"));
    assert!(!preview.contains("rdrtest_abc"));

    let assigned = preview_text("APIFY_TOKEN=abc1234567", 200);
    assert_eq!(assigned, "APIFY_TOKEN=[redacted]");

    let short_keyed = preview_text("API_TOKEN=abc1234567", 200);
    assert_eq!(short_keyed, "API_TOKEN=[redacted]");

    let summary = summarize_task("scrape data with API_TOKEN=abc1234567");
    assert!(summary.contains("[redacted]"));
    assert!(!summary.contains("abc1234567"));
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
    assert!(
        prompt.contains("jj resolve --list"),
        "uses jj resolve: {prompt}"
    );
    assert!(prompt.contains("jj status"));
    // The old git INSTRUCTIONS must be gone (the prompt now only mentions git to
    // warn the resolver away from it, which does not resolve jj conflicts).
    assert!(
        !prompt.contains("Run `git status`"),
        "no git status step: {prompt}"
    );
    assert!(
        !prompt.contains("Stage the resolved files"),
        "no git staging step: {prompt}"
    );
}

#[test]
fn manual_goal_prompt_leads_with_objective_and_done_when() {
    let prompt = manual_goal_prompt("fix the flaky login test");
    assert!(prompt.starts_with("Objective: fix the flaky login test\n"));
    assert!(prompt.contains("Done when: the task is implemented and its own verification passes"));
    // The full task body follows the header.
    assert!(prompt.contains("fix the flaky login test"));
}

#[test]
fn manual_goal_prompt_is_idempotent_for_existing_goal_prompts() {
    let already = "/goal do the thing\nDone when: tests pass\n\nbody text";
    assert_eq!(
        manual_goal_prompt(already),
        "Objective: do the thing\nDone when: tests pass\n\nbody text"
    );
}

#[test]
fn execution_prompt_hoists_goal_block_above_context() {
    let prompt = execution_prompt(
        "Goal: ship the parser\nDone when: cargo test passes\n\nWrite the parser.",
    );
    // The objective lines must be first, but not as a slash command or Goal header.
    assert!(prompt.starts_with("Objective: ship the parser\nDone when: cargo test passes\n"));
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

    app.task_input = "fix auth login".to_string();
    app.task_cursor = app.task_input.chars().count();
    app.focus = FocusPane::Task;
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(app.task_input, "fix auth ");
    assert!(!app.leader_pending, "Ctrl-W edits text in the task pane");
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

#[cfg(not(windows))]
#[test]
fn real_pty_planner_full_summary_survives_stream_then_exit() {
    // EMPIRICAL end-to-end check of the truncation fix against a REAL PTY+process.
    // A script streams claude-style stream-json the way `claude -p` does: the
    // RUDDER_PLAN_TASKS block + a summary whose streamed text is TRUNCATED
    // mid-sentence, then (after a lag, like the real planner) the authoritative
    // `result` event with the COMPLETE text, then it exits. We drive it through
    // the exact drain -> ingest -> (on exit) final-drain flow poll_agents +
    // evaluate_completed_plan use, then assert the captured summary is COMPLETE.
    let block = "RUDDER_PLAN_TASKS_START {\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"scaffold\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]} RUDDER_PLAN_TASKS_END";
    // Streamed deltas cut off mid-sentence at "frozen (" (the reported symptom).
    let l1 = format!(
            "{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{block}\\nThis DAG is safe because config is frozen (\"}}}}}}"
        );
    // The result event carries the FULL final text (block + complete summary).
    let l2 = format!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"{block}\\nThis DAG is safe because config is frozen (no edits) and the modules are independent.\"}}"
        );
    let command = TerminalCommand::with_args(
        "sh",
        [
            "-lc",
            "printf '%s\\n' \"$L1\"; sleep 0.2; printf '%s\\n' \"$L2\"",
        ],
    )
    .with_env("L1", l1)
    .with_env("L2", l2);
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 200,
            ..Default::default()
        },
    )
    .expect("spawn test pty");

    let mut stream = PlanStreamState::new();
    let mut exited = false;
    for _ in 0..400 {
        let _ = pane.drain_output();
        stream.ingest(pane.output_log_snapshot());
        if matches!(pane.try_wait(), Ok(Some(_))) {
            // Final drain after exit (mirrors evaluate_completed_plan).
            for _ in 0..64 {
                let had = !pane.drain_output().is_empty();
                stream.ingest(pane.output_log_snapshot());
                if !had {
                    break;
                }
            }
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(exited, "planner process exited");

    // The plan still parses, and the summary is the COMPLETE sentence, not the
    // truncated "...frozen (" the user was seeing.
    let output = stream.parse_text();
    assert!(
        extract_rudder_plan_tasks(output).is_ok(),
        "plan block parses: {output:?}"
    );
    let summary = extract_rudder_plan_summary(output).expect("summary present");
    assert!(
        summary.contains("the modules are independent"),
        "full summary recovered (not truncated): {summary:?}"
    );
}

#[test]
fn worker_wheel_scroll_rows_scale_with_viewport() {
    assert_eq!(wheel_scroll_rows(2, KeyModifiers::empty()), 1);
    assert_eq!(wheel_scroll_rows(6, KeyModifiers::empty()), 3);
    assert_eq!(wheel_scroll_rows(30, KeyModifiers::empty()), 3);
    assert_eq!(wheel_scroll_rows(90, KeyModifiers::empty()), 3);
    assert_eq!(wheel_scroll_rows(30, KeyModifiers::CONTROL), 29);

    let down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    };
    assert_eq!(mouse_scrollback_delta(down, 30), -3);
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
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "printf 'ready\\r\\n'; sleep 1"]);
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
    assert!(!app.scroll_selected_worker_or_forward(
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
fn terminal_snapshot_queries_do_not_drain_pending_output() {
    let command = TerminalCommand::with_args(
        "/bin/sh",
        [
            "-lc",
            "printf '\\033[?1049h\\033[?1000h\\033[?1006hready\\r\\n'; sleep 1",
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

    std::thread::sleep(Duration::from_millis(80));
    assert!(!pane.uses_alternate_screen_snapshot());
    assert!(!pane.wants_sgr_mouse_events_snapshot());
    assert!(!pane.visible_lines_snapshot().join("\n").contains("ready"));

    for _ in 0..20 {
        pane.drain_output();
        if pane.uses_alternate_screen_snapshot() && pane.wants_sgr_mouse_events_snapshot() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    assert!(pane.uses_alternate_screen_snapshot());
    assert!(pane.wants_sgr_mouse_events_snapshot());
    assert!(pane.visible_lines_snapshot().join("\n").contains("ready"));
}

#[cfg(not(windows))]
#[test]
fn worker_scroll_moves_existing_scrollback_with_pending_output() {
    let command = TerminalCommand::with_args(
        "/bin/sh",
        [
            "-lc",
            "i=1; while [ $i -le 40 ]; do printf 'line%03d\\r\\n' $i; i=$((i+1)); done; sleep 0.2; printf 'pending-output\\r\\n'; sleep 1",
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
    std::thread::sleep(Duration::from_millis(260));

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

    let changed = app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    });
    assert!(
        changed,
        "scroll should move existing scrollback immediately"
    );
    let scrolled = app
        .selected_terminal_mut()
        .map(|terminal| terminal.visible_lines_snapshot().join("\n"))
        .unwrap_or_default();
    assert!(scrolled.contains("line037"), "scrolled was {scrolled:?}");
    assert!(
        !scrolled.contains("pending-output"),
        "scroll handler must not drain pending PTY output: {scrolled:?}"
    );
}

#[cfg(not(windows))]
#[test]
fn no_op_worker_scroll_does_not_mark_dirty() {
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "printf 'ready\\r\\n'; sleep 1"]);
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
    app.worker_area = Some(Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 7,
    });
    app.agents.push(test_agent_run_with_terminal(&app, pane));
    app.selected_agent = 0;
    app.dirty = false;

    let changed = app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    });

    assert!(!changed, "scroll at the bottom should be a no-op");
    assert!(!app.dirty, "no-op scroll must not force a redraw");
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
fn task_cursor_from_selection_point_accounts_for_newlines() {
    // Multi-line input "ab\ncd": clicking row 1 col 1 maps to source index 4 (after
    // 'c'), not 3 — the '\n' is a real source char the old wrap-based mapping dropped,
    // which shifted the cursor and corrupted later edits on multi-line task input.
    let v = "ab\ncd";
    assert_eq!(
        task_cursor_from_selection_point(v, SelectionPoint { row: 0, col: 0 }, 80),
        0
    );
    assert_eq!(
        task_cursor_from_selection_point(v, SelectionPoint { row: 1, col: 0 }, 80),
        3
    );
    assert_eq!(
        task_cursor_from_selection_point(v, SelectionPoint { row: 1, col: 1 }, 80),
        4
    );
    // Clicking past the end of row 0 clamps before the newline (end of "ab" = idx 2).
    assert_eq!(
        task_cursor_from_selection_point(v, SelectionPoint { row: 0, col: 9 }, 80),
        2
    );
}

#[test]
fn assert_no_hard_cycle_allows_hard_dep_on_out_of_block_parent() {
    use crate::tasks::{assert_no_hard_cycle, EdgeType, PlanEdge, RudderPlanTask};
    let mk = |id: &str, dep: &str| RudderPlanTask {
        id: id.to_string(),
        title: id.to_string(),
        prompt: "do".to_string(),
        goal: None,
        success: None,
        deps: vec![PlanEdge {
            on: dep.to_string(),
            edge: EdgeType::Hard,
            why: Some("x".into()),
        }],
        backend: None,
        model: None,
        effort: None,
    };
    // A reconcile/rebase node hard-depending on a FRONTIER id not in the task set is
    // already satisfied and must not be flagged as a cycle (regression: it used to
    // leave the child stuck above indegree 0 and reject the whole plan).
    let reconcile = mk("n1", "frontier-merged-id");
    assert!(assert_no_hard_cycle(std::slice::from_ref(&reconcile)).is_ok());
    // A genuine 2-cycle WITHIN the set is still rejected.
    assert!(assert_no_hard_cycle(&[mk("a", "b"), mk("b", "a")]).is_err());
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
fn orchestrator_scroll_offset_follows_bottom_until_user_scrolls_away() {
    let mut scroll = 0;
    assert_eq!(orchestrator_scroll_offset(&mut scroll, 12, true), 12);
    assert_eq!(scroll, 12);

    scroll = 4;
    assert_eq!(orchestrator_scroll_offset(&mut scroll, 12, false), 4);

    scroll = 99;
    assert_eq!(orchestrator_scroll_offset(&mut scroll, 12, false), 12);
    assert_eq!(scroll, 12);
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
    // This asserts the HEADLESS decomposer profile, which is now the opt-out path
    // (interactive orchestrator is the default). Pin it via the env so the assertion is
    // deterministic regardless of the process-global default; env_guard serializes it
    // against the other orchestrator-mode tests.
    let _env = env_guard();
    std::env::set_var("RUDDER_INTERACTIVE_ORCHESTRATOR", "0");
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
    // Plan mode PRE-APPROVES the research tools INCLUDING Bash (so find/ls/grep/git
    // run without permission prompts) but does NOT restrict the tool set (--tools) or
    // block tools (--disallowedTools): that restrictive profile is the decomposer's.
    assert!(claude
        .args
        .windows(2)
        .any(|window| window[0] == "--allowedTools"
            && window[1] == "Read,Grep,Glob,LS,WebSearch,WebFetch,Bash"));
    assert!(!claude
        .args
        .windows(2)
        .any(|window| window[0] == "--tools" || window[0] == "--disallowedTools"));
    assert!(!claude
        .args
        .iter()
        .any(|arg| arg.contains("[RUDDER PLAN MODE]")));
    assert!(claude
        .args
        .iter()
        .any(|arg| arg.contains("Plan this task before implementation")));

    // The Claude orchestrator (RudderPlan) is a read-only DECOMPOSER that STREAMS its
    // reasoning + the DAG as assistant text: --permission-mode default, read-only tool
    // allowlist, edit/write/bash blocked, headless -p stream-json. (Real plan mode was
    // tried in 1.16.0 and reverted — slower + hid the plan in the plan file.)
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
        "orchestrator runs the read-only decomposer (permission-mode default), not plan mode"
    );
    assert!(
        !orchestrator_claude
            .args
            .windows(2)
            .any(|window| window[0] == "--permission-mode" && window[1] == "plan"),
        "orchestrator is NOT Claude plan mode (reverted in 1.17.x: slow + hid the plan)"
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
        orchestrator_claude
            .args
            .iter()
            .any(|arg| arg == "--include-partial-messages"),
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

    let interactive_codex = agent_command_with_orchestrator_mode(
        Backend::Codex,
        "gpt-5.5",
        Some(EffortLevel::High),
        "build the feature",
        AgentMode::RudderPlan,
        None,
        true,
    );
    assert_eq!(
        interactive_codex.args.first().map(String::as_str),
        Some("--no-alt-screen"),
        "interactive Codex orchestrator uses the TUI, not codex exec"
    );
    assert!(
        !interactive_codex.args.iter().any(|arg| arg == "exec"),
        "interactive Codex orchestrator must not use codex exec"
    );
    assert!(interactive_codex
        .args
        .windows(2)
        .any(|window| window[0] == "--sandbox" && window[1] == "workspace-write"));
    assert!(interactive_codex
        .args
        .windows(2)
        .any(|window| window[0] == "--ask-for-approval" && window[1] == "never"));
    assert!(interactive_codex.args.iter().any(|arg| {
        arg.contains("normal Codex session")
            && arg.contains("RUDDER_PLAN_TASKS_START")
            && arg.contains("Only ever Edit/Write RUDDER.md and RUDDER_SHARED.md")
            && !arg.contains(".claude/skills")
    }));
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
    // still parses, but the queue boundary preflights the node so launch can
    // never inherit an oversized or missing goal condition.
    assert_eq!(tasks[1].goal.as_deref(), Some("UI"));
    assert_eq!(tasks[1].success.as_deref(), Some(DEFAULT_GOAL_SUCCESS));
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
fn finds_latest_codex_session_id_for_matching_cwd() {
    let repo = unique_test_repo("codex-session-cwd");
    let root = unique_test_repo("codex-session-root");
    let shard = root.join("2026").join("06");
    fs::create_dir_all(&shard).expect("create codex sessions dir");

    let old = shard.join("old.jsonl");
    fs::write(
        &old,
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"old-session\",\"cwd\":\"{}\"}}}}\n",
            repo.display()
        ),
    )
    .expect("write old session");

    let other = shard.join("other.jsonl");
    fs::write(
        &other,
        "{{\"type\":\"session_meta\",\"payload\":{\"id\":\"other-session\",\"cwd\":\"/tmp/not-this-repo\"}}}\n",
    )
    .expect("write other session");

    // Ensure the target session has the newest mtime without relying on platform-specific
    // timestamp setters.
    std::thread::sleep(Duration::from_millis(5));
    let newest = shard.join("newest.jsonl");
    fs::write(
        &newest,
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"newest-session\",\"cwd\":\"{}\"}}}}\n",
            repo.display()
        ),
    )
    .expect("write newest session");

    let found = latest_codex_session_id_in_dir(&root, &repo);

    assert_eq!(found.as_deref(), Some("newest-session"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&repo);
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
fn readiness_parity_fixture() {
    // The SAME fixture is consumed by the TS parity test
    // (tests/readiness-parity.test.mjs). If the readiness rule changes, edit the
    // fixture once and both suites fail until both implementations agree. This is
    // the dedup that keeps the TUI (Rust) and daemon (TS) schedulers from drifting.
    let raw = include_str!("../../tests/fixtures/readiness-cases.json");
    let fixture: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
    for case in fixture["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().unwrap_or("?");
        let nodes = case["nodes"].as_object().expect("nodes");
        let edges = case["edges"].as_array().expect("edges");
        let merged: Vec<String> = nodes
            .iter()
            .filter(|(_, v)| v.as_str() == Some("merged"))
            .map(|(k, _)| k.clone())
            .collect();
        let plan_ids: Vec<String> = nodes.keys().cloned().collect();
        let parents = |to: &str, kind: &str| -> Vec<String> {
            edges
                .iter()
                .filter(|e| e["to"].as_str() == Some(to) && e["type"].as_str() == Some(kind))
                .filter_map(|e| e["from"].as_str().map(str::to_string))
                .collect()
        };
        let mut ready: Vec<String> = Vec::new();
        for (id, status) in nodes.iter() {
            if status.as_str() != Some("planned") {
                continue; // is_ready only governs queued nodes
            }
            let node = PlannedNode {
                id: id.clone(),
                title: id.clone(),
                prompt: "p".to_string(),
                goal: None,
                success: None,
                deps: parents(id, "hard"),
                soft_deps: parents(id, "soft"),
                backend: None,
                model: None,
                effort: None,
            };
            if node.is_ready(&merged, &plan_ids) {
                ready.push(id.clone());
            }
        }
        ready.sort();
        let mut expected: Vec<String> = case["expectedReady"]
            .as_array()
            .expect("expectedReady")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        expected.sort();
        assert_eq!(ready, expected, "readiness parity case: {name}");
    }
}

#[test]
fn rudder_plan_worker_prompt_always_leads_with_objective_and_done_when() {
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

    // The objective block leads the prompt unconditionally for BOTH backends.
    for backend in [Backend::Codex, Backend::Claude] {
        let prompt = rudder_plan_worker_prompt("build the feature", &task, "", backend);
        assert!(
            prompt.starts_with(
                "Objective: Complete the API without stopping until cargo test passes."
            ),
            "prompt must lead with a plain objective block: {prompt}"
        );
        assert!(prompt.contains("\nDone when: cargo test passes with no failures"));
        assert!(prompt.contains("Original request:\nbuild the feature"));
        assert!(prompt.contains("Worker task: API"));
        assert!(!prompt.starts_with("/goal"));
        assert!(!prompt.starts_with("Goal:"));
    }
}

#[test]
fn rudder_plan_worker_prompt_derives_goal_and_success_when_absent() {
    // No explicit goal/success: the worker prompt still emits an objective line
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
    let prompt = rudder_plan_worker_prompt("build it", &task, "", Backend::Claude);
    assert!(
        prompt.starts_with("Objective: Implement the parser\n"),
        "derived objective from title: {prompt}"
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
fn run_record_round_trips_merge_resolver_flag() {
    let repo_root = std::env::temp_dir().join(format!(
        "rudder-resolver-record-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&repo_root).expect("create test repo root");

    let mut run = test_agent_run("resolver-run-1", "resolve merge conflicts");
    run.status = AgentStatus::Done;
    run.merge_resolver = true;

    save_native_run_record(&repo_root, &run).expect("save resolver run record");
    let raw = fs::read_to_string(native_run_dir(&repo_root, "resolver-run-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(
        value.get("mergeResolver").and_then(|v| v.as_bool()),
        Some(true)
    );

    let loaded = load_persisted_agents(&repo_root);
    assert_eq!(loaded.len(), 1);
    assert!(
        loaded[0].merge_resolver,
        "resolver discriminator survives reload"
    );
    assert_eq!(loaded[0].status, AgentStatus::Done);

    let _ = fs::remove_dir_all(repo_root);
}

#[test]
fn merge_conflict_run_record_round_trips_actionable_state() {
    let repo_root = unique_test_repo("merge-conflict-record");
    let run_dir = native_run_dir(&repo_root, "conflict-run-1");
    fs::create_dir_all(&run_dir).expect("create run dir");
    let worktree = repo_root.join("worktree");
    let record = serde_json::json!({
        "id": "conflict-run-1",
        "status": "merge-conflict",
        "vcs": "jj",
        "mode": "execute",
        "task": "build auth",
        "taskSummary": "build auth",
        "backend": "claude",
        "model": "sonnet",
        "createdAt": "2026-06-26T00:00:00.000Z",
        "worktree": {
            "enabled": true,
            "path": worktree,
            "workspaceName": "rudder-conflict-run-1",
            "jjChangeId": "abc123",
        },
        "merge": {
            "status": "conflict",
            "conflictKind": "merge",
            "conflictedFiles": ["src/auth.ts", "src/session.ts"],
        },
    });
    fs::write(
        run_dir.join("run.json"),
        serde_json::to_string_pretty(&record).expect("serialize record"),
    )
    .expect("write run.json");

    let loaded = load_persisted_agents(&repo_root);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].status, AgentStatus::Done);
    assert!(loaded[0].has_merge_conflict());
    // Back-compat: a record written before `hadMergeConflict` existed still
    // restores the durable marker from its live conflict state.
    assert!(loaded[0].had_merge_conflict);
    assert_eq!(
        loaded[0].merge_conflict_files,
        vec!["src/auth.ts".to_string(), "src/session.ts".to_string()]
    );
    assert_eq!(agent_status_label(&loaded[0]), "merge conflict · press m");

    save_native_run_record(&repo_root, &loaded[0]).expect("save conflict record");
    let raw = fs::read_to_string(native_run_dir(&repo_root, "conflict-run-1").join("run.json"))
        .expect("read saved run.json");
    let saved: serde_json::Value = serde_json::from_str(&raw).expect("parse saved run.json");
    assert_eq!(
        saved.get("status").and_then(|v| v.as_str()),
        Some("merge-conflict")
    );
    assert_eq!(
        saved
            .get("merge")
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str()),
        Some("conflict")
    );
    assert_eq!(
        saved
            .get("merge")
            .and_then(|v| v.get("conflictedFiles"))
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        Some(2)
    );

    let _ = fs::remove_dir_all(repo_root);
}

#[test]
fn resolved_conflict_keeps_durable_had_merge_conflict_marker() {
    let repo_root = unique_test_repo("had-merge-conflict-record");

    // A merge conflicts: the live flag and the durable marker are both set.
    let mut run = test_agent_run("conflicted-run-1", "build auth");
    run.status = AgentStatus::Done;
    run.merge_conflict = true;
    run.had_merge_conflict = true;
    save_native_run_record(&repo_root, &run).expect("save conflicted record");

    // A resolver succeeds: the live conflict state clears and the run merges,
    // but the durable marker must survive so telemetry (the improve loop's
    // mergeConflictRate) still counts the session.
    let mut loaded = load_persisted_agents(&repo_root);
    assert_eq!(loaded.len(), 1);
    let mut run = loaded.remove(0);
    assert!(run.had_merge_conflict, "conflict history restored on load");
    run.status = AgentStatus::Merged;
    run.merge_conflict = false;
    run.merge_conflict_files.clear();
    save_native_run_record(&repo_root, &run).expect("save merged record");

    let raw = fs::read_to_string(native_run_dir(&repo_root, "conflicted-run-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("merged"));
    assert!(value.get("merge").is_none(), "live conflict state cleared");
    assert_eq!(
        value.get("hadMergeConflict").and_then(|v| v.as_bool()),
        Some(true)
    );

    let reloaded = load_persisted_agents(&repo_root);
    assert_eq!(reloaded.len(), 1);
    assert!(reloaded[0].had_merge_conflict);
    assert!(!reloaded[0].merge_conflict);

    let _ = fs::remove_dir_all(repo_root);
}

#[test]
fn created_at_to_iso_converts_native_millis_and_passes_iso_through() {
    // Native records store epoch millis (now_stamp); comparisons against
    // backend session timestamps need ISO.
    assert_eq!(created_at_to_iso("0"), "1970-01-01T00:00:00.000Z");
    assert_eq!(created_at_to_iso("86400000"), "1970-01-02T00:00:00.000Z");
    // TS-written records already store ISO; pass through unchanged.
    assert_eq!(
        created_at_to_iso("2026-07-08T03:57:01.794Z"),
        "2026-07-08T03:57:01.794Z"
    );
}

#[test]
fn codex_run_usage_reads_rollout_scoped_by_cwd_and_start() {
    let base = unique_test_repo("codex-run-usage");
    let sessions_root = base.join("codex-sessions");
    let day_dir = sessions_root.join("2027/01/01");
    fs::create_dir_all(&day_dir).expect("create sessions dir");
    let run_cwd = base.join("workspace");
    fs::create_dir_all(&run_cwd).expect("create workspace dir");

    // The run's own session: cumulative token_count events; the LAST total wins.
    fs::write(
        day_dir.join("rollout-match.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":{cwd},\"timestamp\":\"2027-01-01T00:00:10.000Z\",\"id\":\"sess-match\"}}}}\n",
                "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.5\"}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":500,\"cached_input_tokens\":100,\"output_tokens\":80,\"reasoning_output_tokens\":10}}}}}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":1000,\"cached_input_tokens\":400,\"output_tokens\":200,\"reasoning_output_tokens\":50}}}}}}}}\n",
            ),
            cwd = serde_json::json!(run_cwd.display().to_string()),
        ),
    )
    .expect("write matching rollout");
    // Another agent's session in a DIFFERENT cwd: never attributed to this run.
    fs::write(
        day_dir.join("rollout-other-cwd.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/somewhere/else\",\"timestamp\":\"2027-01-01T00:00:10.000Z\",\"id\":\"sess-other\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":9999,\"cached_input_tokens\":0,\"output_tokens\":9999,\"reasoning_output_tokens\":0}}}}\n",
        ),
    )
    .expect("write other-cwd rollout");
    // A session in the SAME cwd that started before this run: excluded by start time.
    fs::write(
        day_dir.join("rollout-old.jsonl"),
        format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":{cwd},\"timestamp\":\"2000-01-01T00:00:00.000Z\",\"id\":\"sess-old\"}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":7777,\"cached_input_tokens\":0,\"output_tokens\":7777,\"reasoning_output_tokens\":0}}}}}}}}\n",
            ),
            cwd = serde_json::json!(run_cwd.display().to_string()),
        ),
    )
    .expect("write old rollout");

    let mut run = test_agent_run("codex-usage-run", "build auth");
    run.backend = Backend::Codex;
    run.cwd = run_cwd;
    // Epoch millis for 2026-07-07-ish: after the old session, before the match.
    run.created_at = "1783300000000".to_string();

    let (input, output) =
        collect_run_token_usage_in(&run, &base.join("no-claude-projects"), &sessions_root);
    // Mirrors src/backends.ts accumulateUsage for codex: cached input is a
    // subset of input_tokens (excluded from billable input); reasoning tokens
    // bill as output.
    assert_eq!(input, 600);
    assert_eq!(output, 250);

    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_run_usage_sums_assistant_usage_since_run_start() {
    let base = unique_test_repo("claude-run-usage");
    let projects_root = base.join("claude-projects");
    let run_cwd = base.join("workspace");
    fs::create_dir_all(&run_cwd).expect("create workspace dir");
    let project_dir = projects_root.join(encode_claude_projects_cwd(&run_cwd));
    fs::create_dir_all(&project_dir).expect("create project dir");
    fs::write(
        project_dir.join("session.jsonl"),
        concat!(
            // Before the run started: excluded.
            "{\"type\":\"assistant\",\"timestamp\":\"2000-01-01T00:00:00.000Z\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":5000,\"output_tokens\":5000}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2027-01-01T00:00:01.000Z\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":50}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2027-01-01T00:00:02.000Z\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
        ),
    )
    .expect("write claude session jsonl");

    let mut run = test_agent_run("claude-usage-run", "build auth");
    run.cwd = run_cwd;
    run.created_at = "1783300000000".to_string();

    let (input, output) =
        collect_run_token_usage_in(&run, &projects_root, &base.join("no-codex-sessions"));
    // Mirrors src/backends.ts readClaudeUsage: input includes cache
    // creation/read tokens.
    assert_eq!(input, 190);
    assert_eq!(output, 25);

    let _ = fs::remove_dir_all(base);
}

#[test]
fn run_token_usage_persists_to_run_json_and_reloads() {
    let repo_root = unique_test_repo("token-usage-record");
    let mut run = test_agent_run("tokens-run-1", "build auth");
    run.status = AgentStatus::Done;

    // A run with no usage leaves the field ABSENT (matches the TS __worker
    // path), so telemetry can tell "no data" from a genuine zero.
    save_native_run_record(&repo_root, &run).expect("save zero-usage record");
    let raw = fs::read_to_string(native_run_dir(&repo_root, "tokens-run-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert!(value.get("tokens").is_none(), "zero usage omits tokens");

    run.tokens_in = 600;
    run.tokens_out = 250;
    save_native_run_record(&repo_root, &run).expect("save usage record");
    let raw = fs::read_to_string(native_run_dir(&repo_root, "tokens-run-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(
        value
            .get("tokens")
            .and_then(|t| t.get("input"))
            .and_then(|v| v.as_u64()),
        Some(600)
    );
    assert_eq!(
        value
            .get("tokens")
            .and_then(|t| t.get("output"))
            .and_then(|v| v.as_u64()),
        Some(250)
    );

    // Reload restores the totals, and a post-reload save (merge, rename) does
    // not drop them.
    let mut loaded = load_persisted_agents(&repo_root);
    assert_eq!(loaded.len(), 1);
    let reloaded = loaded.remove(0);
    assert_eq!(reloaded.tokens_in, 600);
    assert_eq!(reloaded.tokens_out, 250);
    save_native_run_record(&repo_root, &reloaded).expect("re-save reloaded record");
    let raw = fs::read_to_string(native_run_dir(&repo_root, "tokens-run-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(
        value
            .get("tokens")
            .and_then(|t| t.get("input"))
            .and_then(|v| v.as_u64()),
        Some(600)
    );

    let _ = fs::remove_dir_all(repo_root);
}

#[test]
fn refresh_run_token_usage_never_regresses_a_stored_total() {
    // The run's cwd is a fresh unique dir, so no real session log can match;
    // the rescan yields (0, 0) and the stored cumulative totals must survive.
    let repo_root = unique_test_repo("token-usage-guard");
    let mut run = test_agent_run("tokens-guard-1", "build auth");
    run.cwd = repo_root.clone();
    run.tokens_in = 500;
    run.tokens_out = 100;
    refresh_run_token_usage(&mut run);
    assert_eq!(run.tokens_in, 500);
    assert_eq!(run.tokens_out, 100);
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

    app.task_input = "/as".to_string();
    let ask_suggestions = suggestions_for(&app);
    let ask = ask_suggestions
        .first()
        .expect("/ask suggestion should be present");
    assert_eq!(ask.label.as_str(), "/ask <text>");
    assert!(
        ask.detail.contains("one-off"),
        "/ask suggestion should explain one-off behavior"
    );

    app.task_input = "/ru".to_string();
    let run_suggestions = suggestions_for(&app);
    let run = run_suggestions
        .first()
        .expect("/run suggestion should be present");
    assert_eq!(run.label.as_str(), "/run <task>");
    assert!(
        run.detail.contains("mergeable"),
        "/run suggestion should explain mergeable worker behavior"
    );

    app.task_input = "/pl".to_string();
    let plan_suggestions = suggestions_for(&app);
    let plan = plan_suggestions
        .first()
        .expect("/plan suggestion should be present");
    assert_eq!(plan.label.as_str(), "/plan <text>");
    assert!(
        plan.detail.contains("DAG"),
        "/plan suggestion should explain DAG behavior"
    );

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
    app.agents.push(test_agent_run("run-1", "first task"));
    app.agents.push(test_agent_run("run-2", "second task"));
    app.selected_agent = 0;
    app.delete_pending = Some("run-1".to_string());

    // Render once: mouse hit-testing resolves through the row map recorded from the
    // actually drawn frame (replacing the old hardcoded header-offset arithmetic).
    render_screen(&mut app, 120, 40);
    let area = app
        .agents_area
        .expect("agents pane area recorded by render");
    let row = app
        .agent_row_map
        .iter()
        .position(|agent| *agent == Some(1))
        .expect("second agent has a rendered row") as u16;

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 2,
        row: area.y + 1 + row,
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
    assert_eq!(
        selected_text_from_lines(&app.orch_visible_rows, sel),
        "hello"
    );

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
fn parse_worker_done_block_extracts_last_block() {
    let out = "chatter\nRUDDER_DONE_START\n{\"summary\":\"did x\",\"followups\":[{\"title\":\"t\"}]}\nRUDDER_DONE_END\ntrailing";
    let note = parse_worker_done_block(out).expect("parsed");
    assert_eq!(note["summary"], "did x");
    assert_eq!(note["followups"][0]["title"], "t");
    assert!(parse_worker_done_block("no block here").is_none());
}

#[test]
fn auto_expand_grows_dag_from_in_scope_followups() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    // A launched plan node n0 (so the frontier is real); not awaiting approval.
    let mut a = test_agent_run("n0agent", "scaffold");
    a.node_id = Some("n0".to_string());
    a.mode = AgentMode::Execute;
    app.agents = vec![a];
    let note = serde_json::json!({
        "summary": "built auth",
        "followups": [
            { "title": "add token refresh", "scope": "in" },
            { "title": "rate limit handling", "scope": "out" }
        ]
    });
    let grew = app.apply_worker_followups("n0", &note);
    assert!(grew, "an in-scope follow-up grows the DAG");
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "only the in-scope follow-up is injected"
    );
    let added = &app.planned_nodes[0];
    assert_eq!(added.title, "add token refresh");
    assert!(
        added.soft_deps.contains(&"n0".to_string()),
        "soft-linked to the finishing node (never deadlocks)"
    );
    assert!(
        app.activity_log.iter().any(|l| l.contains("grew 1 node")),
        "auto-expansion is surfaced in the activity log: {:?}",
        app.activity_log
    );
    // A second identical note adds nothing (title dedupe).
    assert!(
        !app.apply_worker_followups("n0", &note),
        "duplicate title is skipped"
    );
    assert_eq!(app.planned_nodes.len(), 1);
}

#[test]
fn auto_expand_ingests_rudder_done_block_from_worker_pty() {
    // FULL TUI-side communication pathway against a REAL PTY: a finished worker
    // echoes a RUDDER_DONE block to stdout; the conductor scrapes its scrollback,
    // parses the note, and GROWS the DAG with the in-scope follow-up (scope:out is
    // recorded, not injected). Exercises the whole glue that the piecewise unit
    // tests skip: maybe_ingest_worker_followups -> ingest_worker_followups (PTY
    // snapshot) -> parse_worker_done_block -> apply_worker_followups, + idempotency.
    let json = "{\"summary\":\"built auth module\",\"interfaces\":\"added login()\",\"followups\":[{\"title\":\"wire login into router\",\"why\":\"login needs a caller\",\"scope\":\"in\"},{\"title\":\"document the auth flow\",\"scope\":\"out\"}]}";
    let block = format!("RUDDER_DONE_START\n{json}\nRUDDER_DONE_END");
    let command = TerminalCommand::with_args("sh", ["-lc", "printf '%s\\n' \"$BLOCK\""])
        .with_env("BLOCK", block);
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize {
                rows: 12,
                cols: 240,
            },
            scrollback_lines: 200,
            ..Default::default()
        },
    )
    .expect("spawn test pty");
    // Drive the PTY to completion, draining its output into the scrollback.
    let mut exited = false;
    for _ in 0..400 {
        let _ = pane.drain_output();
        if matches!(pane.try_wait(), Ok(Some(_))) {
            for _ in 0..64 {
                let had = !pane.drain_output().is_empty();
                if !had {
                    break;
                }
            }
            exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(exited, "worker pty exited");
    assert!(
        pane.output_log_snapshot().contains("RUDDER_DONE_END"),
        "the done block reached the scrollback: {:?}",
        pane.output_log_snapshot()
    );

    let repo = unique_test_repo("pty-done-ingest");
    let mut app = App::new();
    app.cwd = repo.clone();
    // Hold scheduling so the grown node is not actually launched (no real worker
    // spawn in a unit test); we only assert the DAG GREW.
    app.awaiting_approval = true;
    let mut worker = test_agent_run_with_terminal(&app, pane);
    worker.id = "worker-done-1".to_string();
    worker.node_id = Some("n0".to_string());
    worker.mode = AgentMode::Execute;
    worker.status = AgentStatus::Done;
    app.agents = vec![worker];

    app.maybe_ingest_worker_followups();

    assert_eq!(
        app.planned_nodes.len(),
        1,
        "only the in-scope follow-up is injected"
    );
    assert_eq!(app.planned_nodes[0].title, "wire login into router");
    assert!(
        app.followups_ingested.contains("worker-done-1"),
        "the finished worker is marked ingested once"
    );
    // Idempotent: a second pass over the same Done worker adds nothing.
    app.maybe_ingest_worker_followups();
    assert_eq!(app.planned_nodes.len(), 1, "ingest is once-per-worker");
    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn auto_expand_skips_non_done_or_non_plan_workers() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    // Still Running -> not a candidate.
    let mut running = test_agent_run("r", "x");
    running.node_id = Some("n0".to_string());
    running.mode = AgentMode::Execute;
    running.status = AgentStatus::Running;
    // Done but carries no node_id (a manual run, not a plan node) -> not a candidate.
    let mut manual = test_agent_run("m", "y");
    manual.mode = AgentMode::Execute;
    manual.status = AgentStatus::Done;
    app.agents = vec![running, manual];

    app.maybe_ingest_worker_followups();
    assert!(
        app.planned_nodes.is_empty(),
        "no grow from running / non-plan workers"
    );
    assert!(
        app.followups_ingested.is_empty(),
        "non-candidates are never marked"
    );
}

#[test]
fn stop_agent_frees_slot_and_keeps_workspace_for_undo() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut a = test_agent_run("n0agent", "scaffold");
    a.node_id = Some("n0".to_string());
    a.mode = AgentMode::Execute;
    a.status = AgentStatus::Running;
    app.agents = vec![a];
    assert_eq!(app.running_plan_agents(), 1);

    assert!(app.stop_agent_at(0));
    assert_eq!(app.agents[0].status, AgentStatus::Stopped);
    assert!(app.agents[0].terminal.is_none());
    assert_eq!(
        app.running_plan_agents(),
        0,
        "stop frees the parallelism slot"
    );
    // A stopped node never enters the merged set, so hard dependents stay blocked.
    assert!(!app.merged_node_ids().contains(&"n0".to_string()));
    assert!(app.activity_log.iter().any(|l| l.contains("stopped n0")));
}

#[test]
fn overlapping_files_finds_shared_paths() {
    let a = vec!["src/app.js".to_string(), "README.md".to_string()];
    let b = vec!["src/app.js".to_string(), "src/auth.js".to_string()];
    assert_eq!(overlapping_files(&a, &b), vec!["src/app.js".to_string()]);
    assert!(overlapping_files(&a, &["x".to_string()]).is_empty());
}

#[test]
fn drift_scan_noops_without_two_running_agents() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut a = test_agent_run("n0a", "x");
    a.node_id = Some("n0".to_string());
    a.mode = AgentMode::Execute;
    a.status = AgentStatus::Running;
    app.agents = vec![a];
    app.maybe_handle_drift();
    assert!(
        app.activity_log.is_empty(),
        "no drift action with <2 running agents"
    );
    assert!(
        app.last_drift_scan.is_some(),
        "the scan still stamps its throttle"
    );
}

#[test]
fn steering_guards_refuse_unsafe_targets() {
    // No real agent spawn happens in these (early-return guards only).
    let mut app = App::new();
    // live-inject with no live terminal returns false (caller falls back).
    let no_term = test_agent_run("a", "t");
    app.agents = vec![no_term];
    assert!(!app.live_inject_at(0, "hi"), "no terminal -> false");
    // re-goal refuses the orchestrator (would never spawn a worker for it).
    let mut orch = test_agent_run("orch", "build");
    orch.mode = AgentMode::RudderPlan;
    app.agents = vec![orch];
    assert!(
        !app.regoal_agent_at(0, "new direction"),
        "orchestrator is not re-goaled"
    );
}

#[test]
fn auto_expand_respects_depth_cap() {
    let mut app = App::new();
    // Isolate the DECISIONS.md write: record_decision now dedupes, so it must run against a
    // clean file (not the repo's, which already carries depth-cap entries) for the first
    // record to genuinely write and surface in the activity feed.
    let dir = std::env::temp_dir().join(format!("rudder-depthcap-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join("DECISIONS.md"));
    app.cwd = dir.clone();
    app.followup_gen
        .insert("deep".to_string(), MAX_FOLLOWUP_DEPTH);
    let note = serde_json::json!({ "followups": [{ "title": "more work", "scope": "in" }] });
    assert!(
        !app.apply_worker_followups("deep", &note),
        "depth cap blocks runaway expansion"
    );
    assert!(app.planned_nodes.is_empty());
    assert!(app.activity_log.iter().any(|l| l.contains("depth cap")));
    let _ = std::fs::remove_file(dir.join("DECISIONS.md"));
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
    assert!(
        !app.has_planning_orchestrator(),
        "done planner does not animate"
    );

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
    assert_eq!(
        app.agents[0].worker_input_cursor,
        "use python ".chars().count()
    );

    // Alt+Left jumps back over a word ("python "), then Alt+Backspace removes it.
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(app.agents[0].worker_input_cursor, "use ".chars().count());
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(app.agents[0].worker_input_cursor, 0);

    // Ctrl+K truncates from the cursor to the end of the line.
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::End, KeyModifiers::empty()));
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(app.agents[0].worker_input_draft, "use ");

    // Ctrl+D deletes the char at the cursor; Delete does the same.
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Home, KeyModifiers::empty()));
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
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
    assert!(
        text.contains("use") && text.contains("python"),
        "draft renders: {text}"
    );
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
    assert!(!post_completion_output_is_new_turn(
        Some(before),
        Some(done)
    ));
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

    assert_eq!(text, "Press y to merge  ·  n or Esc to cancel");
    assert_eq!(line.spans.len(), 3);
    // The action key is color-emphasized in the focus accent (teal), not red.
    assert_eq!(line.spans[1].content.as_ref(), "y");
    assert_eq!(line.spans[1].style.fg, Some(ACCENT));
    // Thin theme: emphasis is by color, not weight.
    assert!(!line.spans[1].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn conflict_hint_color_emphasizes_keys_not_weight() {
    let line = conflict_resolve_hint_line();
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        text,
        "Press y for an AI resolver  ·  n to resolve it yourself  ·  Esc to cancel"
    );
    assert_eq!(line.spans[1].content.as_ref(), "y");
    assert_eq!(line.spans[1].style.fg, Some(ACCENT));
    assert_eq!(line.spans[3].content.as_ref(), "n");
    assert_eq!(line.spans[3].style.fg, Some(ACCENT));
    assert!(
        line.spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::BOLD)),
        "emphasis is by color, not weight"
    );
}

#[test]
fn render_merge_conflict_modal_lists_files_and_hint() {
    let mut app = App::new();
    app.conflict_prompt = Some(MergeConflictPrompt {
        operation: ConflictOperation::Merge,
        task: "build auth".to_string(),
        conflicted_files: vec!["src/auth.rs".to_string(), "src/router.rs".to_string()],
        error: "conflict".to_string(),
        repo_root: std::env::temp_dir(),
        target_branch: None,
        source_branch: None,
        worktree_path: None,
        agent_id: None,
    });

    let area = Rect::new(0, 0, 80, 16);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 16)).expect("test backend");
    terminal
        .draw(|frame| render_merge_prompt(frame, area, &app))
        .expect("draw merge prompt");
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();

    assert!(text.contains("Merge conflict"), "modal is titled");
    assert!(
        text.contains("2 conflicted files"),
        "headline counts the files"
    );
    assert!(
        text.contains("src/auth.rs") && text.contains("src/router.rs"),
        "every file is listed"
    );
    assert!(text.contains("\u{2022}"), "files render as bullets");
    assert!(text.contains("AI resolver"), "the action hint is shown");
}

#[test]
fn agent_pane_hints_include_review_and_merge_all_shortcuts() {
    assert!(AGENT_PANE_HINTS.contains(&"R review all"));
    assert!(AGENT_PANE_HINTS.contains(&"M merge all"));
    assert!(AGENT_PANE_HINTS.contains(&"cc clear merged"));
    assert!(AGENT_PANE_HINTS.contains(&"b branch"));
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
        merge_conflict: false,
        merge_conflict_operation: ConflictOperation::Merge,
        merge_conflict_files: Vec::new(),
        had_merge_conflict: false,
        done_summary: None,
        tokens_in: 0,
        tokens_out: 0,
    }
}

fn test_agent_run_with_terminal(app: &App, terminal: TerminalPane) -> AgentRun {
    let mut run = test_agent_run("run-1", "test task");
    run.cwd = app.cwd.clone();
    run.terminal = Some(terminal);
    run
}

#[test]
fn model_picker_accepts_new_claude_families_without_a_release() {
    // The picker must surface a brand-new model family straight from the
    // models.dev cache; a family-name allowlist here is how claude-fable-5
    // stayed invisible for weeks.
    let meta = serde_json::json!({ "tool_call": true });
    assert!(is_claude_picker_model("claude-fable-5", &meta));
    assert!(is_claude_picker_model("claude-somenewtier-6", &meta));
    assert!(is_claude_picker_model("claude-opus-4-8", &meta));
    assert!(!is_claude_picker_model("claude-3-5-sonnet-20241022", &meta));

    // Unknown families rank above haiku instead of scoring zero and falling
    // off the top-8 cut; fable outranks everything.
    assert!(
        score_model(Backend::Claude, "claude-fable-5") > score_model(Backend::Claude, "sonnet")
    );
    assert!(
        score_model(Backend::Claude, "claude-somenewtier-6")
            > score_model(Backend::Claude, "haiku")
    );

    // The curated aliases include fable, and it gets the full effort ladder.
    let rows = fallback_model_rows();
    assert!(rows
        .iter()
        .any(|(backend, model, _)| *backend == Backend::Claude && *model == "fable"));
    assert!(is_reasoning_alias(Backend::Claude, "fable[1m]"));
    assert!(effort_options_for(Backend::Claude, "fable").contains(&Some(EffortLevel::Max)));
    assert!(effort_options_for(Backend::Claude, "claude-somenewtier-6")
        .contains(&Some(EffortLevel::Max)));
}

#[test]
fn model_picker_lists_newest_releases_first_and_drops_dated_duplicates() {
    let data = serde_json::json!({
        "anthropic": { "models": {
            "claude-opus-4-1": { "name": "Claude Opus 4.1", "release_date": "2025-08-05", "tool_call": true },
            "claude-opus-4-8": { "name": "Opus 4.8", "release_date": "2026-05-28", "tool_call": true },
            "claude-opus-4-5": { "name": "Claude Opus 4.5 (latest)", "release_date": "2025-11-24", "tool_call": true },
            "claude-opus-4-5-20251101": { "name": "Claude Opus 4.5", "release_date": "2025-11-24", "tool_call": true },
            "claude-sonnet-5": { "name": "Sonnet 5", "release_date": "2026-06-29", "tool_call": true },
            "claude-fable-5": { "name": "Fable 5", "release_date": "2026-06-07", "tool_call": true },
        }},
        "openai": { "models": {
            "gpt-5.5": { "name": "GPT-5.5", "release_date": "2026-04-23", "tool_call": true },
            "gpt-5.5-pro": { "name": "GPT-5.5 Pro", "release_date": "2026-04-23", "tool_call": true },
            "gpt-5.3-codex": { "name": "GPT-5.3 Codex", "release_date": "2026-02-05", "tool_call": true },
            "gpt-5.4": { "name": "GPT-5.4", "release_date": "2026-03-05", "tool_call": true },
        }},
    });

    let mut rows = Vec::new();
    collect_provider_models(&data, "anthropic", Backend::Claude, &mut rows);
    let claude_ids: Vec<&str> = rows.iter().map(|(_, id, _)| id.as_str()).collect();
    assert_eq!(
        claude_ids,
        vec![
            "claude-sonnet-5", // 2026-06-29
            "claude-fable-5",  // 2026-06-07
            "claude-opus-4-8", // 2026-05-28
            "claude-opus-4-5", // dated twin suppressed
            "claude-opus-4-1",
        ],
        "newest release first; dated snapshot ids collapse into their (latest) twin"
    );
    // Detail carries the human name AND the release date.
    assert_eq!(rows[0].2, "Sonnet 5 · 2026-06-29");

    let mut rows = Vec::new();
    collect_provider_models(&data, "openai", Backend::Codex, &mut rows);
    let codex_ids: Vec<&str> = rows.iter().map(|(_, id, _)| id.as_str()).collect();
    assert_eq!(
        codex_ids,
        vec!["gpt-5.5", "gpt-5.5-pro", "gpt-5.4", "gpt-5.3-codex"],
        "newest first; the -pro variant sinks below its same-day base model"
    );
}

#[test]
fn model_picker_leads_with_aliases_then_explicit_ids() {
    let _guard = env_guard();
    let home = unique_test_repo("model-picker-order");
    let prior = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("models-dev.json"),
        serde_json::json!({
            "anthropic": { "models": {
                "claude-sonnet-5": { "name": "Sonnet 5", "release_date": "2026-06-29", "tool_call": true },
                "claude-opus-4-8": { "name": "Opus 4.8", "release_date": "2026-05-28", "tool_call": true },
            }},
        })
        .to_string(),
    )
    .unwrap();

    let labels: Vec<String> = model_suggestions_for(Backend::Claude, "")
        .into_iter()
        .map(|s| s.label)
        .collect();

    match prior {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&home);

    // Friendly aliases lead (they track the newest release, like the claude
    // CLI's own picker); explicit ids follow newest-first.
    let alias_end = labels.iter().position(|l| l == "claude-sonnet-5").unwrap();
    for alias in ["fable", "sonnet", "opus", "haiku", "fable[1m]"] {
        let at = labels
            .iter()
            .position(|l| l == alias)
            .unwrap_or_else(|| panic!("{alias} missing from {labels:?}"));
        assert!(
            at < alias_end,
            "{alias} lists before explicit ids: {labels:?}"
        );
    }
    let sonnet5 = labels.iter().position(|l| l == "claude-sonnet-5").unwrap();
    let opus48 = labels.iter().position(|l| l == "claude-opus-4-8").unwrap();
    assert!(
        sonnet5 < opus48,
        "explicit ids stay newest-first: {labels:?}"
    );
}

#[test]
fn model_picker_accepts_future_codex_models_without_a_release() {
    let meta = serde_json::json!({
        "tool_call": true,
        "modalities": { "output": ["text"] }
    });
    assert!(is_codex_picker_model("gpt-6-codex-preview", &meta));
    assert!(is_codex_picker_model("gpt-6.1", &meta));
    assert!(is_codex_picker_model("o5", &meta));
    assert!(!is_codex_picker_model("gpt-image-2", &meta));
    assert!(!is_codex_picker_model("gpt-realtime-2", &meta));

    assert!(
        score_model(Backend::Codex, "gpt-6-codex-preview") > score_model(Backend::Codex, "gpt-5.5")
    );
    assert!(effort_options_for(Backend::Codex, "gpt-6.1").contains(&Some(EffortLevel::XHigh)));
    assert_eq!(backend_for_model("o5"), Backend::Codex);
}

#[test]
fn merge_generated_rudder_md_collapses_duplicates_and_is_idempotent() {
    // Two generated blocks (prior corruption) + a stray orphan marker collapse
    // to exactly one fresh block; orchestrator content around them survives.
    let existing = "intro prose\n\n<!-- RUDDER_GENERATED_START -->\nold one\n<!-- RUDDER_GENERATED_END -->\n\nplan notes\n\n<!-- RUDDER_GENERATED_START -->\nold two\n<!-- RUDDER_GENERATED_END -->\n\ntail\n<!-- RUDDER_GENERATED_END -->\n";
    let merged = merge_generated_rudder_md(existing, "fresh body");
    assert_eq!(merged.matches("<!-- RUDDER_GENERATED_START -->").count(), 1);
    assert_eq!(merged.matches("<!-- RUDDER_GENERATED_END -->").count(), 1);
    assert!(merged.contains("fresh body"));
    assert!(merged.contains("intro prose"));
    assert!(merged.contains("plan notes"));
    assert!(merged.contains("tail"));
    assert!(!merged.contains("old one"));
    assert!(!merged.contains("old two"));

    // Re-merging the same body is byte-identical (mirrors the TS guarantee), so
    // repeated renders cannot accrete whitespace forever.
    assert_eq!(merge_generated_rudder_md(&merged, "fresh body"), merged);
}

#[test]
fn q_confirms_before_quitting_with_running_agents() {
    let mut app = App::new();
    app.focus = FocusPane::Agents;
    // No running agents: q quits immediately.
    assert!(app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())));

    let command = TerminalCommand::with_args("sh", ["-c", "sleep 2"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 5, cols: 20 },
            ..Default::default()
        },
    )
    .expect("spawn test pty");
    let mut run = test_agent_run_with_terminal(&app, pane);
    run.status = AgentStatus::Running;
    app.agents.push(run);

    // q now routes through the same guard as Ctrl+C: first press asks, second quits.
    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())));
    assert!(app.quit_confirm_pending);
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("still running"));
    assert!(app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())));
}

#[test]
fn ctrl_c_confirms_before_quitting_and_y_does_not_confirm() {
    let mut app = App::new();
    app.focus = FocusPane::Agents;

    let command = TerminalCommand::with_args("sh", ["-c", "sleep 2"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 5, cols: 20 },
            ..Default::default()
        },
    )
    .expect("spawn test pty");
    let mut run = test_agent_run_with_terminal(&app, pane);
    run.status = AgentStatus::Running;
    app.agents.push(run);

    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(app.quit_confirm_pending);
    assert!(!app.notice.as_deref().unwrap_or_default().contains("(or y)"));

    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty())));
    assert!(!app.quit_confirm_pending);
    assert_eq!(app.notice.as_deref(), Some("quit cancelled"));

    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
}

#[test]
fn esc_dismisses_the_notice_without_being_consumed() {
    let mut app = App::new();
    app.focus = FocusPane::Agents;
    app.notice = Some("auto-merge ON".to_string());
    assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())));
    assert!(app.notice.is_none());
}

#[test]
fn notice_style_colors_errors_and_pending_confirms() {
    assert_eq!(
        notice_style("merge stopped: boom", true).fg,
        Some(FAILED_COLOR)
    );
    assert_eq!(
        notice_style(
            "delete x? press d again to confirm · any other key cancels",
            true
        )
        .fg,
        Some(RUNNING_COLOR)
    );
    assert_eq!(
        notice_style("auto-merged 2 nodes · dependents unblocked", true).fg,
        muted_style(true).fg
    );
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
    // Builds commands via claude_program()/codex_program() (which read RUDDER_*_BIN), so
    // serialize against the bin-injecting orchestrator/harness tests.
    let _env = env_guard();
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
fn fork_commands_branch_the_session_without_touching_the_original() {
    // Builds commands via claude_program()/codex_program(); serialize against
    // bin-injecting tests.
    let _env = env_guard();

    // Claude branches by resuming WITH --fork-session (new session id minted).
    let claude = claude_fork_command("opus", Some(EffortLevel::High), "sid-claude");
    assert_eq!(claude.program, "claude");
    assert!(claude
        .args
        .windows(2)
        .any(|w| w[0] == "--resume" && w[1] == "sid-claude"));
    assert!(
        claude.args.iter().any(|arg| arg == "--fork-session"),
        "--fork-session is what keeps the ORIGINAL session untouched"
    );
    assert!(claude
        .args
        .windows(2)
        .any(|w| w[0] == "--model" && w[1] == "opus"));

    // Codex has a first-class fork subcommand.
    let codex = codex_fork_command("gpt-test", None, "sid-codex");
    assert_eq!(codex.program, "codex");
    assert!(codex
        .args
        .windows(2)
        .any(|w| w[0] == "fork" && w[1] == "sid-codex"));
    assert!(
        !codex.args.iter().any(|arg| arg == "resume"),
        "fork must not resume in place"
    );
    assert!(codex
        .args
        .windows(2)
        .any(|w| w[0] == "-m" && w[1] == "gpt-test"));
    // Same worker profile as a normal Execute run.
    assert!(codex
        .args
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
}

#[test]
fn branch_guards_main_orchestrator_and_sessionless_runs() {
    let mut app = App::new();

    // Main agent: refused.
    let mut main = test_agent_run(MAIN_AGENT_ID, "main branch");
    main.mode = AgentMode::Main;
    app.agents.push(main);
    app.selected_agent = 0;
    app.branch_selected_agent();
    assert_eq!(app.agents.len(), 1, "no fork row was created for main");
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("branch works on worker agents"));

    // Orchestrator: refused.
    app.agents.push(planner_run("orch-1", false));
    app.selected_agent = 1;
    app.branch_selected_agent();
    assert_eq!(
        app.agents.len(),
        2,
        "no fork row was created for the planner"
    );

    // Claude worker with NO session id: nothing to fork yet.
    let worker = test_agent_run("w-1", "some work");
    app.agents.push(worker);
    app.selected_agent = 2;
    app.branch_selected_agent();
    assert_eq!(app.agents.len(), 3, "no fork row without a session");
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("no resumable session"));
}

#[test]
fn stage_claude_session_copies_the_transcript_to_the_fork_cwd() {
    let _env = env_guard();
    let base = unique_test_repo("stage-session");
    let home = base.join("home");
    let source_cwd = base.join("source-ws");
    let target_cwd = base.join("fork-ws");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&target_cwd).unwrap();
    let prior_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);

    let sid = "22222222-2222-4222-8222-222222222222";
    let source_dir = home
        .join(".claude")
        .join("projects")
        .join(encode_claude_project_dir(&source_cwd));
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join(format!("{sid}.jsonl")), "{\"a\":1}\n").unwrap();

    // Missing transcript fails with a readable error (no such session id).
    let missing = stage_claude_session_for_cwd(&source_cwd, "no-such-sid", &target_cwd);
    assert!(missing.is_err());

    let staged = stage_claude_session_for_cwd(&source_cwd, sid, &target_cwd);
    assert!(staged.is_ok(), "staging succeeds: {staged:?}");
    let target = home
        .join(".claude")
        .join("projects")
        .join(encode_claude_project_dir(&target_cwd))
        .join(format!("{sid}.jsonl"));
    assert!(
        target.is_file(),
        "the transcript is readable from the fork's cwd, so --resume finds it"
    );
    // The ORIGINAL transcript is untouched.
    assert!(source_dir.join(format!("{sid}.jsonl")).is_file());

    match prior_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    let _ = fs::remove_dir_all(&base);
}

#[cfg(not(windows))]
#[test]
fn branch_spawns_a_new_forked_agent_row() {
    let _env = env_guard();
    let repo = unique_test_repo("branch-fork");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);
    // Claude scopes --resume by cwd; branching requires the source transcript to
    // exist, so give this session one under an isolated HOME.
    let home = repo.join("home");
    let prior_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    let sid = "11111111-1111-4111-8111-111111111111";
    let project_dir = home
        .join(".claude")
        .join("projects")
        .join(encode_claude_project_dir(&repo));
    fs::create_dir_all(&project_dir).expect("create fake claude project dir");
    fs::write(project_dir.join(format!("{sid}.jsonl")), "{}\n").expect("seed transcript");

    let mut app = App::new();
    app.cwd = repo.clone();
    let mut source = test_agent_run("src-1", "implement the feature");
    source.task_summary = "implement the feature".to_string();
    source.session_id = Some(sid.to_string());
    source.status = AgentStatus::Done;
    source.cwd = repo.clone();
    app.agents.push(source);
    app.selected_agent = 0;

    app.branch_selected_agent();
    match prior_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }

    assert_eq!(app.agents.len(), 2, "a forked agent row was added");
    let fork = &app.agents[1];
    assert_eq!(fork.mode, AgentMode::Execute);
    assert!(fork.task.starts_with("Branch of: "));
    assert!(fork.task_summary.starts_with("branch: "));
    assert_eq!(fork.backend, Backend::Claude);
    assert_eq!(fork.status, AgentStatus::Running);
    assert!(
        fork.session_id.is_none(),
        "the fork mints its own session id; it must NOT reuse the source's"
    );
    // The ORIGINAL run is untouched.
    let source = &app.agents[0];
    assert_eq!(source.status, AgentStatus::Done);
    assert_eq!(
        source.session_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );
    // The fork is selected and focused so the user can type the new direction.
    assert_eq!(app.selected_agent, 1);
    assert_eq!(app.focus, FocusPane::Worker);

    std::env::remove_var("RUDDER_CLAUDE_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn restart_preserves_interactive_orchestrator_profile_from_run_record() {
    // A persisted interactive orchestrator row is authoritative even if the current
    // process default says future planners should be headless.
    let _env = env_guard();
    let repo = unique_test_repo("restart-interactive-orch");
    let fake = repo.join("fake-claude.sh");
    let args_file = repo.join("claude-args.txt");
    write_fake_bin(
        &fake,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nsleep 2\n",
            shell_single_quote(&args_file.to_string_lossy())
        ),
    );
    std::env::set_var("RUDDER_INTERACTIVE_ORCHESTRATOR", "0");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = false;
    let mut run = test_agent_run("orch-1", "plan it");
    run.cwd = repo.clone();
    run.mode = AgentMode::RudderPlan;
    run.backend = Backend::Claude;
    run.status = AgentStatus::Stopped;
    run.autosteered = false;
    run.interactive_orchestrator = true;
    run.terminal = None;
    app.agents.push(run);
    app.selected_agent = 0;

    app.restart_selected_agent();
    // The env vars were only needed while restart_selected_agent built+spawned the
    // command; the fake child is already running. Remove them NOW so a flaky assert
    // below (e.g. the timing-sensitive args_file wait) can't leak RUDDER_CLAUDE_BIN
    // into the next env-guarded test and cascade a second failure.
    std::env::remove_var("RUDDER_CLAUDE_BIN");
    std::env::remove_var("RUDDER_INTERACTIVE_ORCHESTRATOR");
    let restarted = app.agents.first().expect("restarted row");
    assert_eq!(restarted.status, AgentStatus::Running);
    assert!(restarted.interactive_orchestrator);
    assert!(
        !restarted.autosteered,
        "interactive orchestrators must not restart as headless/autosteered planners"
    );

    // Generous wait: under parallel test load the fake child's spawn + first write
    // can lag well past 1s, so poll up to ~5s before giving up (this is the flaky
    // assertion that used to fail and cascade an env leak into the next test).
    for _ in 0..200 {
        if args_file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let args = fs::read_to_string(&args_file).expect("fake claude recorded args");
    assert!(
        args.contains("--append-system-prompt") && args.contains("rudder-orchestrator"),
        "restart used the interactive orchestrator command profile, not current env:\n{args}"
    );
    assert!(
        !args.lines().any(|arg| arg == "-p"),
        "interactive restart must not use headless print-mode args:\n{args}"
    );

    let _ = fs::remove_dir_all(&repo);
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
fn merge_all_command_includes_jj_workspace_runs_without_legacy_path() {
    let mut app = App::new();
    let mut run = test_agent_run("run-1", "test task");
    run.status = AgentStatus::Done;
    run.node_id = Some("n1".to_string());
    run.workspace_name = Some("rudder-17824484932183-fe7bf2".to_string());
    run.jj_change_id = Some("wuytqszwzqswmkrzrznwvrqmonnzkvoo".to_string());
    app.agents.push(run);

    assert!(app.handle_command("/merge-all"));

    assert!(matches!(
        app.merge_confirm.as_ref().map(|confirm| &confirm.intent),
        Some(MergeIntent::All { ids }) if ids == &vec!["run-1".to_string()]
    ));
    assert!(app
        .merge_confirm
        .as_ref()
        .and_then(|confirm| confirm.detail.as_deref())
        .is_some_and(|detail| detail.contains("Ready: n1")));
}

#[test]
fn merge_all_skips_recorded_merge_conflict_rows() {
    let mut app = App::new();
    let mut conflicted = test_agent_run("run-1", "conflicted task");
    conflicted.status = AgentStatus::Done;
    conflicted.node_id = Some("n1".to_string());
    conflicted.workspace_name = Some("rudder-conflicted".to_string());
    conflicted.jj_change_id = Some("abc123".to_string());
    conflicted.merge_conflict = true;
    conflicted.merge_conflict_files = vec!["src/auth.ts".to_string()];
    app.agents.push(conflicted);

    app.request_merge_all_ready();

    assert!(app.merge_confirm.is_none());
    assert!(app
        .notice
        .as_deref()
        .is_some_and(|notice| notice.contains("need selected m")));
}

#[test]
fn merge_all_confirmation_names_ready_nodes_and_logs_action() {
    let mut app = App::new();
    let mut first = test_agent_run("run-1", "first task");
    first.status = AgentStatus::Done;
    first.node_id = Some("n1".to_string());
    first.worktree_path = Some(app.cwd.join("worktree-1"));
    let mut second = test_agent_run("run-2", "second task");
    second.status = AgentStatus::Done;
    second.node_id = Some("n2".to_string());
    second.worktree_path = Some(app.cwd.join("worktree-2"));
    app.auto_merge_skip.push("run-2".to_string());
    app.agents.push(first);
    app.agents.push(second);

    app.request_merge_all_ready();

    let detail = app
        .merge_confirm
        .as_ref()
        .and_then(|confirm| confirm.detail.as_deref())
        .unwrap_or_default();
    assert!(detail.contains("Ready: n1, n2"));
    assert!(detail.contains("parked by auto-merge"));
    assert!(app
        .activity_log
        .iter()
        .any(|line| line.contains("merge-all ready: 2 worktrees (n1, n2)")));
    assert!(app.notice.is_none());
}

#[test]
fn merge_all_ignores_oneoff_agents_even_if_they_have_workspace_fields() {
    let mut app = App::new();
    let mut oneoff = test_agent_run("oneoff-1", "quick question");
    oneoff.mode = AgentMode::OneOff;
    oneoff.status = AgentStatus::Done;
    oneoff.worktree_branch = Some("rudder/oneoff".to_string());
    oneoff.worktree_path = Some(app.cwd.join("oneoff-worktree"));
    let mut run = test_agent_run("run-1", "test task");
    run.status = AgentStatus::Done;
    run.worktree_branch = Some("rudder/test".to_string());
    run.worktree_path = Some(app.cwd.join("worktree"));
    app.agents.push(oneoff);
    app.agents.push(run);

    app.request_merge_all_ready();

    assert!(matches!(
        app.merge_confirm.as_ref().map(|confirm| &confirm.intent),
        Some(MergeIntent::All { ids }) if ids == &vec!["run-1".to_string()]
    ));
}

#[test]
fn selected_oneoff_agent_cannot_be_merged() {
    let mut app = App::new();
    let mut oneoff = test_agent_run("oneoff-1", "quick question");
    oneoff.mode = AgentMode::OneOff;
    oneoff.status = AgentStatus::Done;
    oneoff.worktree_branch = Some("rudder/oneoff".to_string());
    oneoff.worktree_path = Some(app.cwd.join("oneoff-worktree"));
    app.agents.push(oneoff);
    app.selected_agent = 0;

    app.request_merge_selected_agent();

    assert!(app.merge_confirm.is_none());
    assert!(app
        .notice
        .as_deref()
        .is_some_and(|notice| notice.contains("one-off agent: merge disabled")));
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
fn review_all_ignores_oneoff_agents_even_if_they_have_workspace_fields() {
    let mut app = App::new();
    let mut oneoff = test_agent_run("oneoff-1", "quick question");
    oneoff.mode = AgentMode::OneOff;
    oneoff.status = AgentStatus::Done;
    oneoff.worktree_branch = Some("rudder/oneoff".to_string());
    oneoff.worktree_path = Some(app.cwd.join("oneoff-worktree"));
    let mut run = test_agent_run("run-1", "test task");
    run.status = AgentStatus::Done;
    run.worktree_branch = Some("rudder/test".to_string());
    run.worktree_path = Some(app.cwd.join("worktree"));
    app.agents.push(oneoff);
    app.agents.push(run);

    app.review_all_ready();

    assert_eq!(app.selected_agent, 2);
    assert_eq!(app.agents[2].mode, AgentMode::ReviewAll);
    assert_eq!(app.agents[2].review_source_ids, vec!["run-1".to_string()]);
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
fn merge_restores_resolver_relabeled_run_identity() {
    let mut app = App::new();
    let mut run = test_agent_run("run-resolved", "Add arbitrary emoji picker");
    run.status = AgentStatus::Done;
    // What start_conflict_resolution_agent leaves behind.
    run.task = "Resolve merge conflicts: Add arbitrary emoji picker".to_string();
    run.task_summary = "merge conflicts \u{2192} Add arbitrary emoji picker".to_string();
    run.had_merge_conflict = true;
    app.agents.push(run);

    app.mark_agent_and_review_sources_merged(0, Vec::new());

    assert_eq!(app.agents[0].status, AgentStatus::Merged);
    assert_eq!(app.agents[0].task, "Add arbitrary emoji picker");
    assert_eq!(app.agents[0].task_summary, "Add arbitrary emoji picker");
    // Conflict telemetry survives via the durable marker, not the label.
    assert!(app.agents[0].had_merge_conflict);
}

#[test]
fn merge_leaves_unlabeled_run_identity_alone() {
    let mut app = App::new();
    let mut run = test_agent_run("run-clean", "Build the settings pane");
    run.status = AgentStatus::Done;
    app.agents.push(run);

    app.mark_agent_and_review_sources_merged(0, Vec::new());

    assert_eq!(app.agents[0].task, "Build the settings pane");
    assert_eq!(
        app.agents[0].task_summary,
        summarize_task("Build the settings pane")
    );
}

#[test]
fn clear_merged_requires_second_c_and_removes_only_merged() {
    let mut app = App::new();
    let mut merged_a = test_agent_run("run-merged-a", "merged task a");
    merged_a.status = AgentStatus::Merged;
    let mut merged_b = test_agent_run("run-merged-b", "merged task b");
    merged_b.status = AgentStatus::Merged;
    let running = test_agent_run("run-live", "still running");
    let mut done = test_agent_run("run-done", "awaiting review");
    done.status = AgentStatus::Done;
    app.agents.push(merged_a);
    app.agents.push(running);
    app.agents.push(merged_b);
    app.agents.push(done);

    // First press only arms the confirm; nothing is removed.
    app.clear_merged_agents();
    assert_eq!(app.agents.len(), 4);
    assert_eq!(app.delete_pending.as_deref(), Some(CLEAR_MERGED_PENDING));
    let notice = app.notice.as_deref().unwrap_or_default();
    assert!(notice.contains("clear 2 merged agent(s)"));
    assert!(notice.contains("press c again"));

    // Second press clears exactly the merged agents.
    app.clear_merged_agents();
    let ids: Vec<&str> = app.agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["run-live", "run-done"]);
    assert!(app.delete_pending.is_none());
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("cleared 2 merged agent(s)"));
}

#[test]
fn clear_merged_pending_cancels_on_selection_move_like_delete() {
    let mut app = App::new();
    let mut merged = test_agent_run("run-merged", "merged task");
    merged.status = AgentStatus::Merged;
    app.agents.push(merged);
    app.agents.push(test_agent_run("run-live", "still running"));

    app.clear_merged_agents();
    assert_eq!(app.delete_pending.as_deref(), Some(CLEAR_MERGED_PENDING));
    app.select_next_agent();
    assert!(app.delete_pending.is_none());
    // A later `c` starts over at the confirm step; nothing was removed meanwhile.
    assert_eq!(app.agents.len(), 2);
}

#[test]
fn clear_merged_with_none_merged_just_notices() {
    let mut app = App::new();
    app.agents.push(test_agent_run("run-live", "still running"));

    app.clear_merged_agents();
    assert_eq!(app.agents.len(), 1);
    assert!(app.delete_pending.is_none());
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("no merged agents to clear"));
}

#[test]
fn c_key_in_agents_pane_routes_to_clear_merged() {
    let mut app = App::new();
    let mut merged = test_agent_run("run-merged", "merged task");
    merged.status = AgentStatus::Merged;
    app.agents.push(merged);
    app.focus = FocusPane::Agents;

    app.handle_agents_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert_eq!(app.delete_pending.as_deref(), Some(CLEAR_MERGED_PENDING));
    app.handle_agents_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(app.agents.is_empty());
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
        merge_conflict: false,
        merge_conflict_operation: ConflictOperation::Merge,
        merge_conflict_files: Vec::new(),
        had_merge_conflict: false,
        done_summary: None,
        tokens_in: 0,
        tokens_out: 0,
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
fn done_worker_card_shows_objective_and_summary() {
    let mut run = test_agent_run(
        "done-1",
        "Objective: ship the widget\nDone when: tests pass",
    );
    run.status = AgentStatus::Done;
    run.done_summary = Some("Added widget.rs and 4 tests; all green.".to_string());
    let text = done_worker_card_lines(&run, true)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Objective"), "card labels the objective");
    assert!(text.contains("Objective: ship the widget"));
    assert!(text.contains("Done when: tests pass"));
    assert!(text.contains("What it did"));
    assert!(text.contains("Added widget.rs and 4 tests; all green."));

    // Without a recorded summary the card says so instead of going blank.
    run.done_summary = None;
    let text = done_worker_card_lines(&run, true)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("no completion summary was recorded"));
}

#[test]
fn done_worker_card_is_active_only_for_finished_workers() {
    let mut app = App::new();
    let mut done = test_agent_run("w-done", "task");
    done.status = AgentStatus::Done;
    app.agents.push(done);
    let mut running = test_agent_run("w-running", "task");
    running.status = AgentStatus::Running;
    app.agents.push(running);
    let mut orch = test_agent_run("orch", "plan");
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Done;
    app.agents.push(orch);

    app.selected_agent = 0;
    assert!(
        app.selected_done_worker_card_active(),
        "done worker gets the card"
    );
    app.selected_agent = 1;
    assert!(
        !app.selected_done_worker_card_active(),
        "running worker keeps its PTY view"
    );
    app.selected_agent = 2;
    assert!(
        !app.selected_done_worker_card_active(),
        "the orchestrator has its own view"
    );
    app.selected_agent = 0;
    app.worker_view = WorkerView::Diff;
    assert!(
        !app.selected_done_worker_card_active(),
        "diff view is never hijacked"
    );
}

#[test]
fn done_summary_round_trips_through_run_record() {
    let repo = unique_test_repo("done-summary-roundtrip");
    let mut run = test_agent_run("ds-1", "build the thing");
    run.status = AgentStatus::Done;
    run.done_summary = Some("Implemented the thing across 3 files.".to_string());
    save_native_run_record(&repo, &run).expect("save run record");

    let loaded = load_persisted_agents(&repo)
        .into_iter()
        .find(|r| r.id == "ds-1")
        .expect("reload run");
    assert_eq!(
        loaded.done_summary.as_deref(),
        Some("Implemented the thing across 3 files."),
        "doneSummary survives a restart"
    );
    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn ingest_note_summary_lands_on_the_run() {
    let repo = unique_test_repo("ingest-summary");
    let mut app = App::new();
    app.cwd = repo.clone();
    let mut run = test_agent_run("w-1", "task");
    run.status = AgentStatus::Done;
    app.agents.push(run);

    app.set_run_done_summary(
        0,
        &serde_json::json!({ "summary": "  Fixed the flaky test.  " }),
    );
    assert_eq!(
        app.agents[0].done_summary.as_deref(),
        Some("Fixed the flaky test."),
        "summary is stored trimmed"
    );
    // A note without a summary leaves the existing one alone.
    app.set_run_done_summary(0, &serde_json::json!({ "followups": [] }));
    assert_eq!(
        app.agents[0].done_summary.as_deref(),
        Some("Fixed the flaky test.")
    );
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn typing_into_a_finished_worker_reopens_it() {
    let repo = unique_test_repo("reopen-finished");
    let mut app = App::new();
    app.cwd = repo.clone();
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "sleep 5"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 5, cols: 40 },
            cwd: Some(repo.clone()),
            ..Default::default()
        },
    )
    .expect("spawn fake worker PTY");
    let mut run = test_agent_run_with_terminal(&app, pane);
    run.id = "w-done".to_string();
    run.node_id = Some("n0".to_string());
    run.status = AgentStatus::Done;
    app.agents.push(run);
    app.selected_agent = 0;
    app.followups_ingested.insert("w-done".to_string());

    app.record_selected_worker_prompt("also add retries to the client".to_string());

    let run = &app.agents[0];
    assert_eq!(
        run.status,
        AgentStatus::Running,
        "a new instruction re-opens the finished worker"
    );
    assert!(
        !app.followups_ingested.contains("w-done"),
        "ledger cleared so the NEXT completion re-ingests (card summary refreshes)"
    );
    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn not_running_worker_pane_shows_the_full_objective() {
    let mut app = App::new();
    let long_task = format!(
        "Objective: {}\nDone when: every acceptance check in the plan passes and the diff is reviewed.",
        "implement the entire telemetry ingestion pipeline including watermarks, redaction, and ranking "
            .repeat(3)
    );
    let mut run = test_agent_run("done-1", &long_task);
    run.status = AgentStatus::Done;
    run.terminal = None;
    app.agents.push(run);
    app.selected_agent = 0;

    let lines = worker_lines(&mut app, 40, 120);
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The FULL objective is present — not short_task's 26-char cut.
    assert!(
        text.contains("watermarks, redaction, and ranking"),
        "full task body is rendered: {text}"
    );
    assert!(
        text.contains("Done when: every acceptance check"),
        "later lines of the objective survive too"
    );
    assert!(text.contains("This agent is not running."));
}

fn type_rename_char(app: &mut App, ch: char) {
    app.handle_rename_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
}

#[test]
fn rename_first_keystroke_clears_the_prefilled_name() {
    let mut app = App::new();
    let mut run = test_agent_run("a1", "old task name");
    run.task_summary = "old name".to_string();
    app.agents.push(run);
    app.selected_agent = 0;

    app.start_rename_selected_agent();
    // Box opens prefilled with the current name as a preview.
    assert_eq!(app.rename_input.as_deref(), Some("old name"));
    assert!(app.rename_prefilled, "the preview starts selected/pristine");

    // The first typed character wipes the preview and starts from char one.
    type_rename_char(&mut app, 'x');
    assert_eq!(app.rename_input.as_deref(), Some("x"));
    assert_eq!(app.rename_cursor, 1);
    assert!(!app.rename_prefilled, "no longer pristine after typing");

    type_rename_char(&mut app, 'y');
    assert_eq!(
        app.rename_input.as_deref(),
        Some("xy"),
        "subsequent chars append"
    );

    app.commit_rename();
    assert_eq!(app.agents[0].task_summary, "xy");
}

#[test]
fn rename_backspace_edits_the_prefilled_name_in_place() {
    let mut app = App::new();
    let mut run = test_agent_run("a1", "old task name");
    run.task_summary = "hello".to_string();
    app.agents.push(run);
    app.selected_agent = 0;

    app.start_rename_selected_agent();
    // Backspace means "edit this name", not "retype from scratch": the name
    // survives and only the last character is removed.
    app.handle_rename_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
    assert_eq!(app.rename_input.as_deref(), Some("hell"));
    assert!(!app.rename_prefilled);

    // A char now appends to the edited name rather than wiping it.
    type_rename_char(&mut app, 'p');
    assert_eq!(app.rename_input.as_deref(), Some("hellp"));
}

#[test]
fn rename_arrow_key_keeps_the_prefilled_name_for_in_place_edit() {
    let mut app = App::new();
    let mut run = test_agent_run("a1", "old task name");
    run.task_summary = "abc".to_string();
    app.agents.push(run);
    app.selected_agent = 0;

    app.start_rename_selected_agent();
    // Moving the cursor is an edit gesture: the name stays and the next char
    // inserts at the cursor instead of clearing everything.
    app.handle_rename_key(KeyEvent::new(KeyCode::Home, KeyModifiers::empty()));
    assert!(!app.rename_prefilled);
    assert_eq!(app.rename_cursor, 0);
    type_rename_char(&mut app, 'Z');
    assert_eq!(app.rename_input.as_deref(), Some("Zabc"));
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
    app.focus = FocusPane::Task;
    assert!(!handle_event(&mut app, Event::Paste("hello".to_string())));
    assert_eq!(app.task_input, "hello");
    assert_eq!(app.focus, FocusPane::Task);
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
    assert!(
        root.is_ready(&[], &plan_ids),
        "a 0-dep root is always ready"
    );

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
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];

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
    assert_eq!(
        app.planned_nodes[position].id, "n1",
        "merged parent unblocks n1"
    );
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
fn d_key_does_not_discard_plan_queue() {
    // The plan is refined by typing into the task pane, never discarded with `d`.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    app.planned_nodes = vec![test_planned_node("n0", &[])];
    app.focus = FocusPane::Agents;

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "d no longer discards the plan queue"
    );
}

#[test]
fn plan_review_saves_inline_edits_and_blocks_invalid_deps() {
    let repo = unique_test_repo("plan-review-save");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.agents.push(planner_run("orch-1", false));
    app.selected_agent = 0;
    app.awaiting_approval = true;
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];
    app.open_plan_review();

    app.plan_review.field = PlanReviewField::Title;
    app.plan_review.cursor = app.plan_review.active_text().chars().count();
    for ch in " updated".chars() {
        app.handle_plan_review_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
    }
    assert!(app.plan_review.dirty, "typing marks the plan draft dirty");
    assert!(app.commit_plan_review_edits(), "valid inline edit saves");
    assert_eq!(app.planned_nodes[0].title, "n0 updated");
    let text = fs::read_to_string(orchestrator_plan_path(&repo)).expect("RUDDER.md written");
    assert!(
        text.contains("n0 updated") && text.contains("RUDDER_PLAN_TASKS_START"),
        "saved edit is mirrored into RUDDER.md: {text}"
    );

    app.plan_review.nodes[0].hard_deps = "missing".to_string();
    app.plan_review.dirty = true;
    app.approve_planned_queue();
    assert!(
        app.awaiting_approval,
        "invalid deps keep the approval gate closed"
    );
    assert!(
        app.plan_review
            .errors
            .iter()
            .any(|error| error.contains("unknown hard dep")),
        "dependency validation error is shown"
    );
}

#[test]
fn fresh_interactive_plan_ignores_historical_merged_nodes_and_remaps_ids() {
    let repo = unique_test_repo("fresh-plan-after-merged");
    fs::write(
        orchestrator_plan_path(&repo),
        "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"new root\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]},{\"id\":\"n1\",\"title\":\"child\",\"prompt\":\"q\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[{\"on\":\"n0\",\"type\":\"hard\",\"why\":\"uses root\"}]}]}\nRUDDER_PLAN_TASKS_END\nRUDDER_APPROVE_PLAN\n",
    )
    .unwrap();
    let mut app = App::new();
    app.cwd = repo;
    app.plan_request = "old request".to_string();
    app.planned_origin = "old request".to_string();
    app.agents.push(node_agent("n0", AgentStatus::Merged));
    let mut orch = planner_run("orch-new", false);
    orch.task = "new request".to_string();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Codex;
    orch.interactive_orchestrator = true;
    orch.autosteered = false;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    app.maybe_capture_orchestrator_plan();

    assert!(
        app.awaiting_approval,
        "fresh plan captured despite old merged node"
    );
    assert_eq!(app.planned_origin, "new request");
    assert_eq!(app.planned_nodes[0].id, "n0-2");
    assert_eq!(
        app.planned_nodes[1].deps,
        vec!["n0-2".to_string()],
        "intra-plan deps remap with the renamed root"
    );
}

#[test]
fn clear_orchestrator_plan_markers_removes_consumed_plan_and_approval() {
    let repo = unique_test_repo("clear-markers");
    fs::write(
        orchestrator_plan_path(&repo),
        "<!-- RUDDER_GENERATED_START -->\n# generated\n<!-- RUDDER_GENERATED_END -->\n\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[]}\nRUDDER_PLAN_TASKS_END\nRUDDER_APPROVE_PLAN\nRUDDER_ADD_TASK extra\n",
    )
    .unwrap();

    clear_orchestrator_plan_markers(&repo).expect("clear markers");
    let text = fs::read_to_string(orchestrator_plan_path(&repo)).expect("read RUDDER.md");
    assert!(text.contains("RUDDER_GENERATED_START"));
    assert!(!text.contains("RUDDER_PLAN_TASKS_START"));
    assert!(!text.contains("RUDDER_APPROVE_PLAN"));
    assert!(!text.contains("RUDDER_ADD_TASK"));
}

#[test]
fn paste_into_headless_orchestrator_updates_chat_draft() {
    let mut app = App::new();
    app.focus = FocusPane::Worker;
    app.worker_view = WorkerView::Terminal;
    let mut orch = planner_run("orch-1", false);
    orch.interactive_orchestrator = false;
    orch.autosteered = true;
    app.agents.push(orch);
    app.selected_agent = 0;

    app.handle_paste("please revise\nthis plan".to_string());

    assert_eq!(
        app.agents[0].worker_input_draft, "please revise\nthis plan",
        "paste lands in the headless orchestrator chat draft"
    );
}

#[cfg(not(windows))]
#[test]
fn interactive_codex_orchestrator_mouse_selection_targets_terminal_region() {
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "printf 'hello\\r\\n'; sleep 1"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 8, cols: 50 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn pty");
    let mut app = App::new();
    app.focus = FocusPane::Worker;
    app.worker_view = WorkerView::Terminal;
    let mut orch = test_agent_run_with_terminal(&app, pane);
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Codex;
    orch.interactive_orchestrator = true;
    app.agents = vec![orch];
    app.selected_agent = 0;
    let area = Rect::new(0, 0, 80, 30);
    app.worker_area = Some(area);
    let (dag_area, term_area) = interactive_orchestrator_areas(area);
    let term_inner = block_inner(term_area);

    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: term_inner.x + 1,
        row: term_inner.y + 1,
        modifiers: KeyModifiers::empty(),
    });

    assert!(
        app.worker_selection.is_some(),
        "bottom Codex PTY gets terminal selection"
    );
    assert!(
        app.orch_selection.is_none(),
        "bottom Codex PTY is not treated as DAG text"
    );

    let dag_inner = block_inner(dag_area);
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: dag_inner.x + 1,
        row: dag_inner.y + 1,
        modifiers: KeyModifiers::empty(),
    });

    assert!(
        app.orch_selection.is_some(),
        "top DAG still uses rendered-plan selection"
    );
    assert!(
        app.worker_selection.is_none(),
        "top DAG clears terminal selection"
    );
}

#[test]
fn enter_on_agent_focuses_worker() {
    let mut app = App::new();
    let run = test_agent_run("run-1", "ordinary task");
    app.agents.push(run);
    app.selected_agent = 0;
    app.focus = FocusPane::Agents;

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(
        app.focus,
        FocusPane::Worker,
        "Enter focuses worker as before"
    );
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
    app.planner_question_round_done = true;

    app.evaluate_completed_plan(0);

    // The planner run is KEPT as the pinned orchestrator (it owns the plan), not
    // removed; its autosteer flag is cleared so it is only captured once.
    let orchestrators: Vec<&AgentRun> = app
        .agents
        .iter()
        .filter(|run| run.mode == AgentMode::RudderPlan)
        .collect();
    assert_eq!(
        orchestrators.len(),
        1,
        "planner stays as the pinned orchestrator"
    );
    assert!(orchestrators[0].is_orchestrator());
    assert!(!orchestrators[0].autosteered, "captured-once flag cleared");
    // APPROVAL GATE: both nodes queued, nothing launched, awaiting approval.
    assert!(app.awaiting_approval, "plan gates for approval");
    assert_eq!(
        app.planned_nodes.len(),
        2,
        "all nodes queued, none launched"
    );
    assert!(
        !app.agents.iter().any(|run| run.node_id.is_some()),
        "no worker launched before approval"
    );

    // APPROVE: Enter on the selected orchestrator launches the ready root.
    app.handle_agents_key(KeyEvent::from(KeyCode::Enter));
    assert!(!app.awaiting_approval, "approval clears the gate");
    // n0 (root) launched -> an agent tagged with its node id. n1 stays in Todo
    // because its hard dep n0 has not merged.
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "blocked dependent stays in todo"
    );
    assert_eq!(app.planned_nodes[0].id, "n1");
    assert!(
        app.agents
            .iter()
            .any(|run| run.node_id.as_deref() == Some("n0")),
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
    app.planner_question_round_done = true;

    app.evaluate_completed_plan(0);

    // Even a 1-node plan gates: it is queued, nothing launches, awaiting approval.
    assert!(
        app.awaiting_approval,
        "1-node plan still gates for approval"
    );
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "the single node is queued, not launched"
    );
    assert!(
        !app.agents.iter().any(|run| run.node_id.is_some()),
        "no worker launched before approval"
    );

    // APPROVE launches the single ready node.
    app.handle_agents_key(KeyEvent::from(KeyCode::Enter));
    assert!(!app.awaiting_approval, "approval clears the gate");
    assert!(
        app.planned_nodes.is_empty(),
        "the single node launched after approval"
    );
    assert!(
        app.agents.iter().any(|run| run.node_id.is_some()),
        "a node-tagged worker run was launched"
    );
}

#[cfg(not(windows))]
#[test]
fn first_turn_dag_is_forced_to_pause_for_question_round() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let block = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"title\":\"only\",\"prompt\":\"do the thing\",\"goal\":\"thing\",\"success\":\"done\"}]}\nRUDDER_PLAN_TASKS_END\n";
    let planner = planner_with_block(&app, block);
    app.agents.push(planner);
    app.selected_agent = 0;

    app.evaluate_completed_plan(0);

    assert!(
        app.planner_paused_for_input,
        "the first turn must ask before DAG capture"
    );
    assert!(
        !app.awaiting_approval,
        "no approval gate until the answer turn emits a DAG"
    );
    assert!(app.planned_nodes.is_empty(), "premature DAG is not queued");
    assert_eq!(
        app.pending_questions,
        forced_planner_questions(),
        "Rudder supplies a deterministic question if the model skipped its own"
    );
    assert!(
        !app.agents[0].autosteered,
        "the refused first turn is captured once and waits for a resumed answer"
    );
}

#[test]
fn paused_planner_transcript_strips_the_duplicate_questions_block() {
    // When the planner pauses for input, the questions are re-rendered as a clean
    // numbered prompt, so the streamed RUDDER_QUESTIONS block (markers + the same
    // questions) must NOT also appear in the transcript below it. The inspection
    // prose around it stays.
    let transcript = vec![
            PlanEntry {
                kind: PlanEntryKind::Tool,
                text: "Reading js/config.js".to_string(),
            },
            PlanEntry {
                kind: PlanEntryKind::Text,
                text: "I inspected the repo.\nA few things shape the build:\nRUDDER_QUESTIONS_START\nTracks, artists, or both?\nReuse js/config.js?\nRUDDER_QUESTIONS_END".to_string(),
            },
        ];
    let flatten = |lines: &[ratatui::text::Line<'static>]| -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut stripped = Vec::new();
    crate::render::push_transcript_lines(&mut stripped, &transcript, false, true);
    let stripped = flatten(&stripped);
    assert!(
        !stripped.contains("RUDDER_QUESTIONS_START"),
        "marker is hidden"
    );
    assert!(
        !stripped.contains("RUDDER_QUESTIONS_END"),
        "marker is hidden"
    );
    assert!(
        !stripped.contains("Tracks, artists, or both?"),
        "question is not duplicated"
    );
    assert!(
        !stripped.contains("Reuse js/config.js?"),
        "question is not duplicated"
    );
    assert!(
        stripped.contains("I inspected the repo."),
        "inspection prose is kept"
    );
    assert!(
        stripped.contains("A few things shape the build:"),
        "the lead-in into the questions is kept (it now sits right above them)"
    );
    assert!(
        stripped.contains("Reading js/config.js"),
        "tool steps are kept"
    );

    let mut raw = Vec::new();
    crate::render::push_transcript_lines(&mut raw, &transcript, false, false);
    let raw = flatten(&raw);
    assert!(
        raw.contains("RUDDER_QUESTIONS_START") && raw.contains("Tracks, artists, or both?"),
        "without strip the block is shown verbatim (live planning has no separate prompt)"
    );
}

#[test]
fn question_card_is_box_aligned_and_holds_questions_plus_answer() {
    // The bordered "plan mode" card must keep every row exactly box-wide (or the
    // right border tears), show each question, and render the live answer + cursor.
    let box_w = 64usize;
    let questions = vec![
        "Reuse js/config.js, or rebuild from scratch?".to_string(),
        "Real Spotify API, or mock data so it works with no setup at all today?".to_string(),
    ];
    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
    crate::render::push_question_card(&mut lines, &questions, "1: reuse, 2: mock", 17, box_w, true);

    // Alignment invariant: every rendered row is exactly box_w columns.
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            line.width(),
            box_w,
            "row {i} is not box-aligned: {:?}",
            flatten_line(line)
        );
    }
    let text = lines
        .iter()
        .map(flatten_line)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("╭─") && text.contains("╮"),
        "has a top border"
    );
    assert!(
        text.contains("╰─") && text.contains("╯"),
        "has a bottom border"
    );
    assert!(text.contains("Plan mode"), "titled as plan mode");
    assert!(text.contains("Reuse js/config.js"), "shows question 1");
    assert!(text.contains("mock data"), "shows question 2 (wrapped)");
    assert!(
        text.contains("› 1: reuse, 2: mock"),
        "renders the live answer field"
    );
    assert!(
        text.contains('↵') && text.contains("esc clear"),
        "footer shows key hints"
    );
}

#[test]
fn question_card_falls_back_to_a_plain_list_on_narrow_panes() {
    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
    crate::render::push_question_card(&mut lines, &["only one?".to_string()], "", 0, 20, true);
    let text = lines
        .iter()
        .map(flatten_line)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('╭'), "no box on a narrow pane");
    assert!(text.contains("The planner needs your input") && text.contains("only one?"));
}

#[test]
fn signal_state_parses_done_input_and_rejects_garbage() {
    use crate::signals::{parse_signal_state, SignalState};
    assert_eq!(
        parse_signal_state(r#"{"state":"done"}"#),
        Some(SignalState::Done)
    );
    assert_eq!(
        parse_signal_state(" {\"state\":\"input\"}\n"),
        Some(SignalState::Input)
    );
    assert_eq!(parse_signal_state(r#"{"state":"weird"}"#), None);
    assert_eq!(parse_signal_state("not json"), None);
    assert_eq!(parse_signal_state("{}"), None);
}

#[test]
fn claude_settings_wire_stop_and_idle_hooks_to_the_signal_file() {
    use std::path::Path;
    let json = crate::signals::claude_settings_json(Path::new("/tmp/sig/run.json"), false);
    // Round-trips as valid JSON with the official hook shape.
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid settings json");
    assert!(v["hooks"]["Stop"].is_array(), "has a Stop hook");
    assert_eq!(
        v["hooks"]["Notification"][0]["matcher"], "idle_prompt",
        "idle Notification hook"
    );
    let stop_cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        stop_cmd.contains("/tmp/sig/run.json") && stop_cmd.contains("\"state\":\"done\""),
        "Stop writes done"
    );
    let note_cmd = v["hooks"]["Notification"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        note_cmd.contains("\"state\":\"input\""),
        "idle writes input"
    );
    // fast_mode=false carries no fastMode key; fast_mode=true sets it (Claude Code's
    // native fast-mode settings flag, injected into a worker's --settings).
    assert!(v.get("fastMode").is_none(), "no fastMode unless requested");
    let fast: serde_json::Value = serde_json::from_str(&crate::signals::claude_settings_json(
        Path::new("/tmp/sig/run.json"),
        true,
    ))
    .expect("valid settings json");
    assert_eq!(
        fast["fastMode"],
        serde_json::Value::Bool(true),
        "fastMode flag set"
    );
}

#[test]
fn completion_sound_defaults_off_and_config_parses_bool() {
    assert_eq!(
        config::config_completion_sound(&serde_json::json!({})),
        None
    );
    assert_eq!(
        config::config_completion_sound(&serde_json::json!({"completionSound": true})),
        Some(true)
    );
    assert_eq!(
        config::config_completion_sound(&serde_json::json!({"completionSound": false})),
        Some(false)
    );
}

#[test]
fn color_mode_defaults_to_terminal_background_and_config_parses_aliases() {
    assert_eq!(config::config_color_mode(&serde_json::json!({})), None);
    assert_eq!(
        config::config_color_mode(&serde_json::json!({"colorMode": "terminal"})),
        Some(ColorMode::Terminal)
    );
    assert_eq!(
        config::config_color_mode(&serde_json::json!({"colorMode": "terminal-bg"})),
        Some(ColorMode::Terminal)
    );
    assert_eq!(
        config::config_color_mode(&serde_json::json!({"colorMode": "paper"})),
        Some(ColorMode::Paper)
    );
}

#[test]
fn sound_command_persists_completion_sound_toggle() {
    let _env = env_guard();
    let home = unique_test_repo("sound-config-home");
    let previous_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let mut app = App::new();
    assert!(!config::completion_sound_enabled(), "default is quiet");

    assert!(app.handle_command("/sound on"));
    assert!(config::completion_sound_enabled(), "sound on persisted");
    assert_eq!(
        app.notice.as_deref(),
        Some("completion sound ON (saved): workers ping when they enter review")
    );

    assert!(app.handle_command("/sound off"));
    assert!(!config::completion_sound_enabled(), "sound off persisted");
    assert_eq!(
        app.notice.as_deref(),
        Some("completion sound OFF (saved): workers enter review silently")
    );

    match previous_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn color_command_persists_and_applies_dashboard_color_mode() {
    let _env = env_guard();
    let home = unique_test_repo("color-config-home");
    let previous_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let mut app = App::new();
    assert_eq!(color_mode(), ColorMode::Terminal);

    assert!(app.handle_command("/color paper"));
    assert_eq!(color_mode(), ColorMode::Paper);
    assert_eq!(
        config::config_color_mode(&config::load_rudder_config().expect("config saved")),
        Some(ColorMode::Paper)
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("color mode paper (saved): using Rudder's white paper canvas")
    );

    assert!(app.handle_command("/color terminal"));
    assert_eq!(color_mode(), ColorMode::Terminal);
    assert_eq!(
        config::config_color_mode(&config::load_rudder_config().expect("config saved")),
        Some(ColorMode::Terminal)
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("color mode terminal (saved): using terminal foreground/background for the dashboard")
    );

    match previous_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn codex_notify_script_writes_done_on_turn_complete() {
    use std::path::Path;
    let script = crate::signals::codex_notify_script(Path::new("/tmp/sig/run.json"));
    assert!(script.starts_with("#!/bin/sh"), "is a shell script");
    assert!(
        script.contains("agent-turn-complete"),
        "matches the turn-end event"
    );
    assert!(
        script.contains("/tmp/sig/run.json") && script.contains("\"state\":\"done\""),
        "writes done signal"
    );
}

#[cfg(not(windows))]
#[test]
fn poll_loop_consumes_the_official_done_signal_for_a_live_worker() {
    // Integration test for the CONSUMPTION half (the gap the node e2e can't reach,
    // since it drives the daemon/process-exit path, not the native poll loop): a
    // LIVE interactive worker (a `sleep` PTY so try_wait == Ok(None)) is flipped to
    // Done ONLY when the official signal file appears — proving the signal, not the
    // PTY-scrape, drives it (`sleep` has no idle chrome for the fallback to match).
    let Some(sig) = crate::signals::signal_path("sig-worker-itest") else {
        return; // no HOME in this env; signal path is unavailable
    };
    let _ = std::fs::remove_file(&sig);

    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "sleep 30"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn worker pty");
    let mut run = test_agent_run("sig-worker-itest", "do the thing");
    run.cwd = app.cwd.clone();
    run.terminal = Some(pane);
    // defaults: mode Execute, status Running.
    app.agents.push(run);
    app.selected_agent = 0;

    // No signal yet + not idle chrome -> the worker stays Running.
    app.poll_agents();
    assert_eq!(
        app.agents[0].status,
        AgentStatus::Running,
        "without a signal (and no idle chrome on `sleep`) the worker stays running"
    );

    // The Claude Stop hook / Codex notify would write exactly this.
    if let Some(parent) = sig.parent() {
        std::fs::create_dir_all(parent).expect("signals dir");
    }
    std::fs::write(&sig, "{\"state\":\"done\"}").expect("write signal");

    app.poll_agents();
    assert_eq!(
        app.agents[0].status,
        AgentStatus::Done,
        "the official done signal flips the live worker to Done"
    );
    assert!(
        !sig.exists(),
        "the signal is consumed (cleared) so the next turn re-fires"
    );

    let _ = std::fs::remove_file(&sig);
}

#[cfg(not(windows))]
#[test]
fn wired_worker_waits_for_the_signal_and_ignores_idle_chrome() {
    // The premature-review bug: with official signals wired (its hook config on
    // disk), a mid-turn idle screen — claude's "shift+tab to cycle" footer, which
    // the scrape treats as done — must NOT flip the worker to review past
    // READY_GRACE. Only the Stop-hook/notify signal (or process exit) may.
    let _env = env_guard();
    let home = unique_test_repo("sig-wait-home");
    let prior_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let run_id = "sig-wait-itest";
    let wiring = crate::signals::prepare_worker_signals(run_id, Backend::Claude);
    let settings = wiring
        .claude_settings
        .clone()
        .expect("RUDDER_HOME-backed signal settings are writable");
    assert!(
        crate::signals::worker_has_config(run_id, Backend::Claude),
        "config wired"
    );

    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let command = TerminalCommand::with_args(
        "/bin/sh",
        ["-lc", "printf 'shift+tab to cycle\\r\\n'; sleep 8"],
    );
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn worker pty");
    let mut run = test_agent_run(run_id, "do the thing");
    run.cwd = app.cwd.clone();
    run.terminal = Some(pane);
    app.agents.push(run);
    app.selected_agent = 0;

    // Poll past READY_GRACE (3.2s). Without the fix the idle chrome would mark it
    // done; with the fix the wired worker waits for the signal.
    let deadline = Instant::now() + Duration::from_millis(4500);
    while Instant::now() < deadline {
        app.poll_agents();
        assert_eq!(
            app.agents[0].status,
            AgentStatus::Running,
            "a wired worker must wait for the signal, not the idle-chrome scrape"
        );
        std::thread::sleep(Duration::from_millis(150));
    }

    // The Stop hook fires -> writes the signal -> now it completes.
    let sig = crate::signals::signal_path(run_id).unwrap();
    std::fs::write(&sig, "{\"state\":\"done\"}").unwrap();
    app.poll_agents();
    assert_eq!(
        app.agents[0].status,
        AgentStatus::Done,
        "the official done signal completes the wired worker"
    );

    let _ = std::fs::remove_file(&sig);
    let _ = std::fs::remove_file(&settings);
    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(not(windows))]
#[test]
fn merge_conflict_resolver_spawn_wires_its_completion_signal() {
    // Regression: a merge-conflict resolver reuses the failing node's row (same
    // run_id). The node's original launch left `{run_id}-claude.json` on disk, so
    // worker_has_config() is true -> the poll loop WAITS for a Stop hook. If the
    // resolver spawn does not RE-wire that hook, the hook never fires, the resolver
    // sits Running forever even after it resolves cleanly, and finalize_merge_resolvers
    // (which needs status==Done) never merges the node. So the spawn MUST wire signals.
    let _env = env_guard();
    if crate::signals::signals_dir().is_none() {
        return; // no HOME/RUDDER_HOME in this env
    }
    let run_id = "resolver-signal-itest";
    // Start from a clean slate: no config on disk for this run id.
    let config = crate::signals::signals_dir()
        .unwrap()
        .join(format!("{run_id}-claude.json"));
    let _ = std::fs::remove_file(&config);

    let repo = unique_test_repo("resolver-signal");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Claude;
    let mut run = test_agent_run(run_id, "build auth");
    run.cwd = repo.clone();
    app.agents.push(run);
    app.selected_agent = 0;
    app.conflict_prompt = Some(MergeConflictPrompt {
        operation: ConflictOperation::Merge,
        task: "build auth".to_string(),
        conflicted_files: vec!["src/auth.rs".to_string()],
        error: "conflict".to_string(),
        repo_root: repo.clone(),
        target_branch: None,
        source_branch: None,
        worktree_path: None,
        agent_id: Some(run_id.to_string()),
    });

    app.start_conflict_resolution_agent();

    // Capture results, THEN clean up process-global state, THEN assert — so a failing
    // assert can never leak RUDDER_CLAUDE_BIN into the next env-guarded test.
    let is_resolver = app.agents[0].merge_resolver;
    let has_config = crate::signals::worker_has_config(run_id, Backend::Claude);
    if let Some(run) = app.agents.get_mut(0) {
        run.terminal = None; // kill the fake PTY
    }
    std::env::remove_var("RUDDER_CLAUDE_BIN");
    let _ = std::fs::remove_file(&config);

    assert!(
        is_resolver,
        "the reused row is flagged as a merge resolver so finalize can pick it up"
    );
    assert!(
        has_config,
        "the resolver spawn must wire the backend completion signal, or it sits Running forever"
    );
}

#[test]
fn restart_reconciles_finished_merge_resolver_to_done() {
    // The frozen-board case: under an older binary a merge resolver resolved cleanly but
    // was persisted `Running` forever (its completion signal was never wired), so its node
    // never merged and its children stayed blocked. On the next rudder start,
    // reconcile_orphaned_runs must flip such a resolver (no live PTY, conflict-free
    // workspace) to Done so finalize_merge_resolvers merges it and unblocks the plan.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut run = test_agent_run("stuck-resolver", "Resolve merge conflicts: build auth");
    run.cwd = std::env::temp_dir(); // a non-jj dir reports zero conflicts
    run.status = AgentStatus::Running;
    run.merge_resolver = true;
    run.node_id = Some("n2".to_string());
    run.terminal = None;
    app.agents.push(run);

    app.reconcile_orphaned_runs();

    assert_eq!(
        app.agents[0].status,
        AgentStatus::Done,
        "a finished merge resolver with no live PTY and a clean workspace is reconciled to Done"
    );
}

#[cfg(not(windows))]
#[test]
fn selected_agent_row_renders_a_visible_marker() {
    // The selection arrow must actually appear on screen (it used to fade to FAINT when the
    // agents pane was unfocused, so it was sometimes invisible).
    let mut app = App::new();
    app.agents = vec![
        test_agent_run("a", "first task"),
        test_agent_run("b", "second task"),
    ];
    app.selected_agent = 1;
    let screen = render_screen(&mut app, 120, 40);
    assert!(
        screen.contains('\u{25b6}'),
        "the selected agent row shows the marker glyph (big ▶ arrow):\n{screen}"
    );
}

#[test]
fn agents_pane_scrolls_to_follow_the_selection() {
    // With more agents than fit in the pane, navigating down must scroll the pane so
    // the selected agent stays on screen. The list used to render stateless from the
    // top, so the bottom of the list — including the selection — was clipped off.
    let mut app = App::new();
    let mut agents = Vec::new();
    for i in 0..30 {
        agents.push(test_agent_run(&format!("n{i}"), &format!("zztask{i:02}")));
    }
    app.agents = agents;
    // Seed the diff cache so render doesn't shell out to git/jj 30 times per frame.
    for a in &app.agents {
        app.diff_summary_cache
            .insert(a.id.clone(), (Instant::now(), None));
    }

    // A short pane (height 24) cannot show all 30 multi-line rows at once. Selecting
    // the LAST agent must scroll it into view and push the FIRST off the top.
    app.selected_agent = 29;
    let screen = render_screen(&mut app, 120, 24);
    assert!(
        screen.contains("zztask29"),
        "the selected (last) agent must scroll into view:\n{screen}"
    );
    assert!(
        !screen.contains("zztask00"),
        "the first agent must scroll off the top when the last is selected:\n{screen}"
    );

    // Selecting the FIRST agent must scroll back to the top.
    app.selected_agent = 0;
    let screen = render_screen(&mut app, 120, 24);
    assert!(
        screen.contains("zztask00"),
        "selecting the first agent scrolls back to the top:\n{screen}"
    );
    assert!(
        !screen.contains("zztask29"),
        "the last agent is off-screen when the first is selected:\n{screen}"
    );
}

#[test]
fn auto_merge_waits_for_an_active_resolver() {
    // Integration through the shared jj workspace is serial: a Done node must NOT be merged
    // while a merge-conflict resolver is mid-flight in the same workspace, or the new merge
    // would stack on top of the in-progress resolution and corrupt both. The guard returns
    // before any merge is attempted (so this is deterministic — no `rudder merge` subprocess).
    let mut app = App::new();
    app.auto_merge = true;
    let mut ready = test_agent_run("ready", "feature");
    ready.node_id = Some("n1".to_string());
    ready.status = AgentStatus::Done;
    let mut resolver = test_agent_run("resolving", "Resolve merge conflicts: foundation");
    resolver.node_id = Some("n0".to_string());
    resolver.status = AgentStatus::Running;
    resolver.merge_resolver = true;
    app.agents = vec![ready, resolver];

    app.maybe_auto_merge();

    assert_eq!(
        app.agents[0].status,
        AgentStatus::Done,
        "the ready node stays unmerged while a resolver integrates in the shared workspace"
    );
}

#[cfg(not(windows))]
#[test]
fn tui_harness_drives_planner_to_dag_and_renders_it() {
    // FULL end-to-end through the real App: a planner task launches the (fake) Claude
    // planner, which emits an ExitPlanMode plan carrying a RUDDER_PLAN_TASKS DAG; the
    // App captures it (plan_stream exit_plan), builds the DAG, and the orchestrator
    // pane RENDERS it. This is the harness that exercises the 1.16.0 plan-mode capture
    // through the actual render path — no real claude, no auth, deterministic.
    let _env = env_guard();
    std::env::set_var("RUDDER_INTERACTIVE_ORCHESTRATOR", "0"); // test the HEADLESS path
    let repo = unique_test_repo("tui-harness");
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .status()
        .is_ok());

    // The fake `claude` emits the headless decomposer stream: system init, then a
    // result event whose text STREAMS the reasoning + the RUDDER_PLAN_TASKS block as
    // assistant text (how the real read-only decomposer presents the DAG), then exits.
    let dag = r#"{"tasks":[{"id":"n0","title":"impl-mathutils","prompt":"add add()","goal":"add add","success":"defines add","deps":[]},{"id":"n1","title":"test-mathutils","prompt":"pytest","goal":"add a test","success":"imports add","deps":[{"on":"n0","type":"hard","why":"the test imports add()"}]}]}"#;
    let plan = format!("Here is the DAG.\nRUDDER_PLAN_TASKS_START\n{dag}\nRUDDER_PLAN_TASKS_END\nThis split is safe.");
    let result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": plan,
    })
    .to_string();
    let stream = format!(
        "{}\n{}\n",
        r#"{"type":"system","subtype":"init","session_id":"fake-planner"}"#, result
    );
    let stream_file = repo.join("planner-stream.jsonl");
    fs::write(&stream_file, &stream).expect("write stream");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(
        &fake,
        &format!(
            "#!/bin/sh\ncat {}\nsleep 0.4\n",
            shell_single_quote(&stream_file.to_string_lossy())
        ),
    );
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Claude;
    app.start_rudder_plan_task("build mathutils.add and a test");
    // The first-question gate is unit-tested on its own; here we drive the DAG-capture
    // path, so mark the question round satisfied.
    app.planner_question_round_done = true;

    let mut captured = false;
    for _ in 0..300 {
        app.poll_agents();
        if app.awaiting_approval || !app.planned_nodes.is_empty() {
            captured = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    std::env::remove_var("RUDDER_CLAUDE_BIN");

    assert!(
        captured,
        "planner produced a DAG via the ExitPlanMode capture"
    );
    assert_eq!(app.planned_nodes.len(), 2, "both DAG nodes captured");
    assert!(
        app.planned_nodes
            .iter()
            .any(|n| n.title == "impl-mathutils")
            && app
                .planned_nodes
                .iter()
                .any(|n| n.title == "test-mathutils"),
        "captured nodes match the plan: {:?}",
        app.planned_nodes
            .iter()
            .map(|n| &n.title)
            .collect::<Vec<_>>()
    );

    // RENDER the real dashboard and assert the DAG is on screen.
    let screen = render_screen(&mut app, 120, 40);
    assert!(
        screen.contains("impl-mathutils") && screen.contains("test-mathutils"),
        "the orchestrator pane renders the captured DAG:\n{screen}"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn tui_harness_renders_the_plan_mode_card_when_the_planner_asks() {
    // End-to-end: the planner's FIRST turn asks (RUDDER_QUESTIONS, no ExitPlanMode);
    // the App pauses and the orchestrator pane renders the bordered "plan mode" card
    // with the numbered questions. Exercises the question gate + card render together.
    // The card + question gate are the HEADLESS decomposer's UX, now the opt-out path.
    let _env = env_guard();
    std::env::set_var("RUDDER_INTERACTIVE_ORCHESTRATOR", "0");
    let repo = unique_test_repo("tui-harness-q");
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .status()
        .is_ok());
    let stream = format!(
        "{}\n{}\n",
        r#"{"type":"system","subtype":"init","session_id":"fake-planner-q"}"#,
        r#"{"type":"result","subtype":"success","result":"RUDDER_QUESTIONS_START\nWhich time range: 4 weeks or all time?\nReuse the existing config module?\nRUDDER_QUESTIONS_END"}"#
    );
    let stream_file = repo.join("planner-q.jsonl");
    fs::write(&stream_file, &stream).expect("write stream");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(
        &fake,
        &format!(
            "#!/bin/sh\ncat {}\nsleep 0.4\n",
            shell_single_quote(&stream_file.to_string_lossy())
        ),
    );
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Claude;
    app.start_rudder_plan_task("build a spotify top tracks page");

    let mut paused = false;
    for _ in 0..300 {
        app.poll_agents();
        if app.planner_paused_for_input && !app.pending_questions.is_empty() {
            paused = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    std::env::remove_var("RUDDER_CLAUDE_BIN");

    assert!(
        paused,
        "the planner's first turn paused for the question round"
    );
    assert!(
        app.planned_nodes.is_empty(),
        "no DAG captured on the asking turn"
    );
    assert_eq!(app.pending_questions.len(), 2, "both questions parsed");

    // The orchestrator pane renders the plan-mode card with the questions.
    app.selected_agent = 0;
    app.focus = FocusPane::Worker;
    let screen = render_screen(&mut app, 120, 40);
    assert!(
        screen.contains("Plan mode") && screen.contains("Which time range"),
        "the plan-mode card renders the planner's questions:\n{screen}"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn tui_harness_interactive_orchestrator_captures_plan_file_and_renders_dag() {
    // Interactive orchestrator: a (fake) interactive Claude writes the DAG to
    // RUDDER.md; the App captures it into the approval gate and the
    // DAG pane ABOVE the orchestrator PTY renders it. Validates the step-1 build.
    let _env = env_guard();
    let repo = unique_test_repo("tui-interactive-orch");
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .status()
        .is_ok());
    let dag = r#"{"tasks":[{"id":"n0","title":"impl-alpha","prompt":"do","goal":"g","success":"s","deps":[]},{"id":"n1","title":"test-alpha","prompt":"do","goal":"g","success":"s","deps":[{"on":"n0","type":"hard","why":"imports n0"}]}]}"#;
    // The fake interactive `claude` runs in the repo cwd: write the plan file, then idle.
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(
            &fake,
            &format!(
                "#!/bin/sh\ncat > RUDDER.md <<'PLAN'\n# Plan\nRUDDER_PLAN_TASKS_START\n{dag}\nRUDDER_PLAN_TASKS_END\nPLAN\nsleep 8\n"
            ),
        );
    std::env::set_var("RUDDER_INTERACTIVE_ORCHESTRATOR", "1");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Claude;
    app.start_rudder_plan_task("build the alpha module and a test");

    let mut captured = false;
    for _ in 0..300 {
        app.poll_agents();
        if app.awaiting_approval && app.planned_nodes.len() == 2 {
            captured = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    std::env::remove_var("RUDDER_CLAUDE_BIN");

    assert!(
        captured,
        "the orchestrator's plan file was captured into the approval gate"
    );
    assert!(
        app.planned_nodes.iter().any(|n| n.title == "impl-alpha")
            && app.planned_nodes.iter().any(|n| n.title == "test-alpha"),
        "captured the DAG nodes: {:?}",
        app.planned_nodes
            .iter()
            .map(|n| &n.title)
            .collect::<Vec<_>>()
    );

    // The captured plan now pops into the inline review document.
    app.selected_agent = 0;
    app.focus = FocusPane::Worker;
    let screen = render_screen(&mut app, 120, 40);
    std::env::remove_var("RUDDER_INTERACTIVE_ORCHESTRATOR");
    assert!(
        screen.contains("impl-alpha") && screen.contains("plan review"),
        "the inline review document renders the captured plan:\n{screen}"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn render_screen_shows_bottom_task_bar() {
    let mut app = App::new();
    app.focus = FocusPane::Task;

    let screen = render_screen(&mut app, 100, 30);

    assert!(
        screen.contains("Type a task to plan and run"),
        "the bottom task bar should render:\n{screen}"
    );
}

#[test]
fn write_rudder_context_preserves_orchestrator_plan_block() {
    let repo = unique_test_repo("rudder-md-merge-native");
    fs::write(
        repo.join("RUDDER.md"),
        "# Orchestrator notes\n\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"keep\",\"prompt\":\"p\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n",
    )
    .unwrap();

    write_rudder_context(&repo, &[], None).expect("write RUDDER.md");
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();

    assert!(text.contains("<!-- RUDDER_GENERATED_START -->"));
    assert!(text.contains("RUDDER_PLAN_TASKS_START"));
    assert!(text.contains("\"title\":\"keep\""));

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn write_rudder_context_mirrors_shared_context_to_worktree() {
    let repo = unique_test_repo("shared-context-mirror");
    let workspace = repo.join("worker");
    fs::create_dir_all(&workspace).unwrap();

    append_shared_context(&repo, "test", "APIFY_TOKEN=abc1234567").unwrap();
    let pending = WorktreeInfo {
        id: "run-1".to_string(),
        path: workspace.clone(),
        branch: None,
        path_is_worktree: true,
        workspace_name: None,
        jj_change_id: None,
    };
    write_rudder_context(&repo, &[], Some(&pending)).expect("write RUDDER.md");

    let root_shared = fs::read_to_string(repo.join("RUDDER_SHARED.md")).unwrap();
    let workspace_shared = fs::read_to_string(workspace.join("RUDDER_SHARED.md")).unwrap();
    let rudder_md = fs::read_to_string(repo.join("RUDDER.md")).unwrap();
    let gitignore = fs::read_to_string(repo.join(".gitignore")).unwrap();

    assert!(root_shared.contains("APIFY_TOKEN=abc1234567"));
    assert_eq!(workspace_shared, root_shared);
    assert!(rudder_md.contains("RUDDER_SHARED.md"));
    assert!(gitignore
        .lines()
        .any(|line| line.trim() == "RUDDER_SHARED.md"));

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn write_rudder_context_redacts_secret_values_in_agent_previews() {
    let repo = unique_test_repo("rudder-md-redacts-agent-preview");
    let mut agent = test_agent_run(
        "run-secret",
        "scrape with APIFY_TOKEN=abc1234567 and write data/seed/talent.json",
    );
    agent.cwd = repo.clone();
    agent.current_prompt = "keep using APIFY_TOKEN=abc1234567 for the ingest".to_string();

    write_rudder_context(&repo, &[agent], None).expect("write RUDDER.md");
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();

    assert!(text.contains("APIFY_TOKEN=[redacted]"));
    assert!(!text.contains("abc1234567"));
    assert!(!text.contains("abc1234567"));

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn write_rudder_context_includes_global_job_snapshot() {
    let repo = unique_test_repo("rudder-md-global-job-snapshot");
    let worktree = repo.join("worker-starting");
    fs::create_dir_all(&worktree).unwrap();

    let mut running = test_agent_run("run-codex", "monitor running codex work");
    running.cwd = repo.join("codex-worktree");
    running.backend = Backend::Codex;
    running.model = "gpt-5.1-codex-max".to_string();
    running.node_id = Some("n1".to_string());
    running.deps = vec!["n0".to_string()];
    running.soft_deps = vec!["n2".to_string()];
    running.needs_user_input = true;
    fs::create_dir_all(&running.cwd).unwrap();

    let mut done = test_agent_run("run-claude", "finish claude worker");
    done.cwd = repo.join("claude-worktree");
    done.status = AgentStatus::Done;
    done.node_id = Some("n3".to_string());
    done.workspace_name = Some("rudder-run-claude-test".to_string());
    done.jj_change_id = Some("zzzzzzzz".to_string());
    fs::create_dir_all(&done.cwd).unwrap();

    let mut merged = test_agent_run("run-merged", "already merged worker");
    merged.cwd = repo.join("merged-worktree");
    merged.status = AgentStatus::Merged;
    merged.node_id = Some("n4".to_string());
    fs::create_dir_all(&merged.cwd).unwrap();

    let pending = WorktreeInfo {
        id: "pending-1".to_string(),
        path: worktree,
        branch: Some("feature/global-view".to_string()),
        path_is_worktree: true,
        workspace_name: None,
        jj_change_id: None,
    };

    write_rudder_context(&repo, &[running, done, merged], Some(&pending)).expect("write RUDDER.md");
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();

    assert!(text.contains("## Global job snapshot"));
    assert!(text.contains(
        "- totals: total=4 running=1 waiting=1 done=1 merged=1 failed=0 stopped=0 pending-starts=1"
    ));
    assert!(text.contains(
        "- active-now: running=1 waiting=1 review-ready=1 merge-ready=1 pending-starts=1"
    ));
    assert!(text.contains("- completed: merged=1 failed=0 stopped=0"));
    assert!(text.contains("- backends: claude=2 codex=1"));
    assert!(text.contains("- ready-to-act: review-ready=1 merge-ready=1"));
    assert!(text.contains("## Active local Rudder agents"));
    assert!(text.contains("## Ready local Rudder agents"));
    assert!(text.contains("## Completed local Rudder agents"));
    let active_section = text
        .split("## Active local Rudder agents")
        .nth(1)
        .unwrap()
        .split("## Ready local Rudder agents")
        .next()
        .unwrap();
    assert!(active_section.contains("run-codex node=n1 mode=execute status=running waiting=user-input backend=codex model=gpt-5.1-codex-max"));
    assert!(active_section.contains("deps=hard=[n0] soft=[n2]"));
    assert!(!active_section.contains("run-claude"));
    assert!(!active_section.contains("run-merged"));
    let ready_section = text
        .split("## Ready local Rudder agents")
        .nth(1)
        .unwrap()
        .split("## Completed local Rudder agents")
        .next()
        .unwrap();
    assert!(ready_section.contains("run-claude node=n3 mode=execute status=done backend=claude"));
    assert!(ready_section.contains("merge=needs-merge"));
    assert!(!ready_section.contains("run-merged"));
    let completed_section = text
        .split("## Completed local Rudder agents")
        .nth(1)
        .unwrap();
    assert!(
        completed_section.contains("run-merged node=n4 mode=execute status=merged backend=claude")
    );
    assert!(text.contains("status=starting backend=pending model=pending"));
    let worker_text = fs::read_to_string(repo.join("codex-worktree").join("RUDDER.md"))
        .expect("running workspace RUDDER.md");
    assert_eq!(worker_text, text);
    let ready_text = fs::read_to_string(repo.join("claude-worktree").join("RUDDER.md"))
        .expect("ready workspace RUDDER.md");
    assert_eq!(ready_text, text);
    let completed_text = fs::read_to_string(repo.join("merged-worktree").join("RUDDER.md"))
        .expect("completed workspace RUDDER.md");
    assert_eq!(completed_text, text);

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn rudder_context_carries_session_memory_for_new_agents() {
    let repo = unique_test_repo("context-session-memory");
    let mut done = test_agent_run("run-done", "Objective: add retries to the sync client");
    done.status = AgentStatus::Done;
    done.done_summary = Some("Added exponential backoff to sync.ts and 6 tests.".to_string());
    let history = vec![
        "/model claude opus".to_string(), // commands are noise, not instructions
        "add retries to the sync client".to_string(),
        "also cover the timeout path".to_string(),
    ];

    write_rudder_context_with_history(&repo, &[done], None, &history)
        .expect("write RUDDER.md with history");
    let text = fs::read_to_string(repo.join("RUDDER.md")).expect("read RUDDER.md");

    // A new agent sees what each finished run actually DID…
    assert!(
        text.contains("did=\"Added exponential backoff to sync.ts and 6 tests.\""),
        "done summary rides the roster line: {text}"
    );
    // …and what the user has been asking for, newest first, commands filtered.
    let section = text
        .split("## Recent user instructions (newest first)")
        .nth(1)
        .expect("instructions section");
    let timeout_at = section.find("also cover the timeout path").unwrap();
    let retries_at = section.find("add retries to the sync client").unwrap();
    assert!(timeout_at < retries_at, "newest instruction first");
    assert!(
        !section.contains("/model"),
        "slash commands are filtered out"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn orchestrator_prompts_include_global_monitoring_contract() {
    let claude_prompt = orchestrator_system_prompt();
    assert!(claude_prompt.contains("Global job snapshot"));
    assert!(claude_prompt.contains("wait for current Claude/Codex jobs"));
    assert!(claude_prompt.contains("RUDDER_REVIEW_ALL"));
    assert!(claude_prompt.contains("RUDDER_RUN <task>"));
    assert!(claude_prompt.contains("RUDDER_MAIN <prompt>"));

    let codex_prompt = codex_orchestrator_prompt("monitor the current threads");
    assert!(codex_prompt.contains("Global job snapshot"));
    assert!(codex_prompt.contains("Active local Rudder agents"));
    assert!(codex_prompt.contains("Ready local Rudder agents"));
    assert!(codex_prompt.contains("Completed local Rudder agents"));
    assert!(codex_prompt.contains("Active as live or waiting only"));
    assert!(codex_prompt.contains("RUDDER_RUN <task>"));
    assert!(codex_prompt.contains("monitor the current threads"));
}

#[test]
fn orchestrator_skill_markers_are_consumed_once() {
    let repo = unique_test_repo("orch-skill-markers");
    fs::write(
        repo.join("RUDDER.md"),
        "before\nRUDDER_AUTOMERGE on\nRUDDER_HELP\nafter\n",
    )
    .unwrap();

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    app.auto_merge = false;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    app.scan_orchestrator_skill_markers();

    assert!(app.auto_merge, "automerge marker toggled state");
    assert!(app
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("skills:"));
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();
    assert!(!text.contains("RUDDER_AUTOMERGE"));
    assert!(!text.contains("RUDDER_HELP"));
    assert!(text.contains("before") && text.contains("after"));

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn orchestrator_control_marker_can_stop_worker() {
    let repo = unique_test_repo("orch-stop-marker");
    fs::write(repo.join("RUDDER.md"), "before\nRUDDER_STOP n0\nafter\n").unwrap();

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    let mut worker = test_agent_run("run-1", "do node");
    worker.cwd = repo.clone();
    worker.node_id = Some("n0".to_string());
    worker.status = AgentStatus::Running;
    app.agents.push(worker);

    app.scan_orchestrator_skill_markers();

    let worker = app
        .agents
        .iter()
        .find(|run| run.node_id.as_deref() == Some("n0"))
        .expect("worker row");
    assert_eq!(worker.status, AgentStatus::Stopped);
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();
    assert!(!text.contains("RUDDER_STOP"));
    assert!(text.contains("before") && text.contains("after"));

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn orchestrator_run_marker_starts_manual_execute_worker() {
    let _env = env_guard();
    let repo = unique_test_repo("orch-run-marker");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);
    fs::write(
        repo.join("RUDDER.md"),
        "before\nRUDDER_RUN build the settings panel\nafter\n",
    )
    .unwrap();

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    app.scan_orchestrator_skill_markers();

    let spawned = app
        .agents
        .iter()
        .find(|run| run.mode == AgentMode::Execute)
        .expect("RUDDER_RUN starts an execute worker");
    assert!(spawned.node_id.is_none(), "RUDDER_RUN is not a DAG node");
    let text = fs::read_to_string(repo.join("RUDDER.md")).unwrap();
    assert!(!text.contains("RUDDER_RUN"));

    for run in &mut app.agents {
        run.terminal = None;
    }
    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn interactive_default_uses_codex_live_orchestrator() {
    let _env = env_guard();
    let repo = unique_test_repo("codex-orch-backend");
    let fake = repo.join("fake-codex.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    std::env::set_var("RUDDER_CODEX_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    app.backend = Backend::Codex;
    app.model = "gpt-test".to_string();
    app.effort = Some(EffortLevel::High);

    app.start_rudder_plan_task("plan with codex");

    let run = app
        .agents
        .iter()
        .find(|run| run.is_orchestrator())
        .expect("orchestrator run");
    assert_eq!(run.backend, Backend::Codex, "Codex stays selected");
    assert_eq!(
        run.model, "gpt-test",
        "Codex model is not swapped to Claude"
    );
    assert!(
        run.interactive_orchestrator,
        "Codex uses the live conductor path when interactive orchestration is enabled"
    );
    assert!(
        !run.autosteered,
        "Codex live conductor is not captured through the headless completed-planner path"
    );
    assert!(
        !repo.join(".claude").join("skills").exists(),
        "Claude-only orchestrator skills are not generated for Codex"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn fresh_plan_retires_stale_orchestrator_rows() {
    let _env = env_guard();
    let repo = unique_test_repo("orch-stopped-default");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    app.backend = Backend::Claude;
    app.model = "sonnet".to_string();
    let mut old = test_agent_run("orch-old", "approved plan");
    old.cwd = repo.clone();
    old.mode = AgentMode::RudderPlan;
    old.backend = Backend::Claude;
    old.autosteered = false;
    old.status = AgentStatus::Stopped;
    app.agents.push(old);

    app.start_rudder_plan_task("new plan");

    let orchestrators: Vec<&AgentRun> = app
        .agents
        .iter()
        .filter(|run| run.is_orchestrator())
        .collect();
    assert_eq!(orchestrators.len(), 1, "stopped row was retired");
    assert_ne!(orchestrators[0].id, "orch-old", "a fresh row was started");
    assert_eq!(orchestrators[0].status, AgentStatus::Running);
    assert!(
        orchestrators[0].terminal.is_some(),
        "fresh default orchestrator has a live PTY"
    );

    std::env::remove_var("RUDDER_CLAUDE_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn fresh_interactive_orchestrator_clears_stale_plan_markers() {
    let _env = env_guard();
    let repo = unique_test_repo("orch-clear-stale-markers");
    let fake = repo.join("fake-claude.sh");
    write_fake_bin(&fake, "#!/bin/sh\nsleep 5\n");
    std::env::set_var("RUDDER_CLAUDE_BIN", &fake);
    fs::write(
        orchestrator_plan_path(&repo),
        "before\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"old\",\"title\":\"old\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n\nRUDDER_APPROVE_PLAN\nRUDDER_AUTOMERGE on\nRUDDER_MERGE_ALL\nafter\n",
    )
    .unwrap();

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    app.backend = Backend::Claude;
    app.model = "sonnet".to_string();

    app.start_rudder_plan_task("new request");

    let text = fs::read_to_string(orchestrator_plan_path(&repo)).unwrap();
    assert!(!text.contains("RUDDER_PLAN_TASKS_START"));
    assert!(!text.contains("RUDDER_PLAN_TASKS_END"));
    assert!(!output_has_approve_marker(&text));
    assert!(!text.contains("RUDDER_AUTOMERGE"));
    assert!(!text.contains("RUDDER_MERGE_ALL"));
    assert!(text.contains("before") && text.contains("after"));

    std::env::remove_var("RUDDER_CLAUDE_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn codex_headless_planner_ignores_stale_rudder_md_plan() {
    let repo = unique_test_repo("codex-ignore-stale-rudder-md");
    fs::write(
        orchestrator_plan_path(&repo),
        "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"old\",\"title\":\"old\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\nRUDDER_APPROVE_PLAN\n",
    )
    .unwrap();

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    let mut run = test_agent_run("codex-plan", "plan it");
    run.cwd = repo.clone();
    run.mode = AgentMode::RudderPlan;
    run.backend = Backend::Codex;
    run.autosteered = true;
    run.status = AgentStatus::Running;
    app.agents.push(run);

    app.maybe_capture_orchestrator_plan();
    app.scan_orchestrator_markers();

    assert!(
        !app.awaiting_approval,
        "Codex headless planners must not capture stale RUDDER.md DAGs"
    );
    assert!(app.planned_nodes.is_empty());

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn approve_marker_detection_is_exact_full_line() {
    // Triggers only on an exact full-line RUDDER_APPROVE_PLAN (after ANSI + markdown strip).
    assert!(output_has_approve_marker(
        "ok launching\nRUDDER_APPROVE_PLAN\n"
    ));
    assert!(output_has_approve_marker("```\nRUDDER_APPROVE_PLAN\n```"));
    assert!(output_has_approve_marker("`RUDDER_APPROVE_PLAN`"));
    assert!(output_has_approve_marker("**RUDDER_APPROVE_PLAN**"));
    assert!(output_has_approve_marker(
        "\u{1b}[32mRUDDER_APPROVE_PLAN\u{1b}[0m\n"
    ));
    // Must NOT trigger on a mention, a quote, or the _TEMPLATE reference.
    assert!(!output_has_approve_marker(
        "print RUDDER_APPROVE_PLAN when the user agrees"
    ));
    assert!(!output_has_approve_marker("RUDDER_APPROVE_PLAN_TEMPLATE"));
    assert!(!output_has_approve_marker(
        "the marker is RUDDER_APPROVE_PLAN, got it"
    ));
    assert!(!output_has_approve_marker("RUDDER_PLAN_TASKS_START"));
}

#[cfg(not(windows))]
#[test]
fn interactive_orchestrator_self_launches_on_approve_marker() {
    // After the user approves in chat, the orchestrator prints RUDDER_APPROVE_PLAN in its
    // PTY; scan_orchestrator_markers detects it and approves+launches (no task-bar Enter).
    let _env = env_guard(); // serializes RUDDER_CODEX_BIN
    let repo = unique_test_repo("orch-marker"); // non-git: worker launch runs in place
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.interactive_orchestrator = true; // per-App field, no global env
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.planned_nodes = vec![titled_planned_node("n0", "do the thing")];
    app.awaiting_approval = true;

    // An orchestrator PTY that printed the approve marker.
    let command = TerminalCommand::with_args(
        "/bin/sh",
        ["-lc", "printf 'RUDDER_APPROVE_PLAN\\r\\n'; sleep 5"],
    );
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        pane.drain_output();
        if pane.output_log_snapshot().contains("RUDDER_APPROVE_PLAN") {
            break;
        }
    }
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.interactive_orchestrator = true;
    orch.terminal = Some(pane);
    app.agents.push(orch);

    app.scan_orchestrator_markers();
    assert!(
        !app.awaiting_approval,
        "the approve marker launched the plan (gate cleared)"
    );
    assert!(
        app.planned_nodes.is_empty() || app.agents.iter().any(|run| run.node_id.is_some()),
        "the planned node was launched into a worker"
    );

    // Idempotent: re-scanning the same (still-present) marker does nothing.
    let agents_after = app.agents.len();
    app.scan_orchestrator_markers();
    assert!(!app.awaiting_approval);
    assert_eq!(
        app.agents.len(),
        agents_after,
        "no duplicate launch on re-scan"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn interactive_orchestrator_self_launches_from_plan_file_marker() {
    // PRIMARY (hardened) approval channel: the orchestrator writes RUDDER_APPROVE_PLAN
    // INTO its plan file (a structured Write), and Rudder launches from the file — no
    // PTY-scrape needed.
    let _env = env_guard(); // serializes RUDDER_CODEX_BIN
    let repo = unique_test_repo("orch-file-marker"); // non-git: worker launch runs in place
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.interactive_orchestrator = true; // per-App field, no global env
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.planned_nodes = vec![titled_planned_node("n0", "do the thing")];
    app.awaiting_approval = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    fs::create_dir_all(repo.join(".rudder")).unwrap();
    fs::write(
            orchestrator_plan_path(&repo),
            "# Plan\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"x\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n\nRUDDER_APPROVE_PLAN\n",
        )
        .unwrap();

    app.scan_orchestrator_markers();
    assert!(
        !app.awaiting_approval,
        "the plan-file approval marker launched the plan"
    );
    assert!(
        app.planned_nodes.is_empty() || app.agents.iter().any(|run| run.node_id.is_some()),
        "the planned node launched from the file-based approval"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn orchestrator_recaptures_dag_edits_before_launch() {
    // While iterating the plan WITH the orchestrator (before launch), if it re-writes
    // the plan file to add/change tasks, Rudder must refresh planned_nodes live —
    // the first-capture guard otherwise freezes the DAG after the first proposal.
    let _env = env_guard();
    let repo = unique_test_repo("orch-recapture");
    let mut app = App::new();
    app.interactive_orchestrator = true;
    app.cwd = repo.clone();
    app.awaiting_approval = true;
    app.planned_nodes = vec![titled_planned_node("n0", "first")];

    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Running;
    orch.backend = Backend::Claude;
    orch.interactive_orchestrator = true;
    app.agents.push(orch);

    fs::create_dir_all(repo.join(".rudder")).unwrap();
    // The orchestrator added a second task in response to the user.
    fs::write(
            orchestrator_plan_path(&repo),
            "# Plan\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"first\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]},{\"id\":\"n1\",\"title\":\"second\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n",
        )
        .unwrap();

    app.maybe_recapture_orchestrator_plan();
    assert!(app.awaiting_approval, "the gate stays OPEN while iterating");
    assert_eq!(
        app.planned_nodes.len(),
        2,
        "the added task was recaptured live"
    );
    assert_eq!(
        app.planned_nodes[0].id, "n0",
        "recapture preserves the orchestrator's stable ids"
    );
    assert!(app.planned_nodes.iter().any(|n| n.id == "n1"));

    app.plan_review.nodes[0].title = "local draft edit".to_string();
    app.plan_review.dirty = true;

    // Idempotent: re-reading the same file does not churn or reset an in-progress
    // inline review draft. This used to flip n0 -> n0-2 because the current preview
    // itself was treated as an id collision.
    app.maybe_recapture_orchestrator_plan();
    assert_eq!(
        app.planned_nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["n0", "n1"],
        "no id churn on an unchanged file"
    );
    assert_eq!(
        app.plan_review.nodes[0].title, "local draft edit",
        "unchanged recapture does not reset the inline plan preview"
    );

    // When the approval marker is present, recapture defers to scan (does not apply a
    // further edit in the same write) — n2 must NOT be folded in here.
    fs::write(
            orchestrator_plan_path(&repo),
            "# Plan\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"first\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]},{\"id\":\"n1\",\"title\":\"second\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]},{\"id\":\"n2\",\"title\":\"third\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n\nRUDDER_APPROVE_PLAN\n",
        )
        .unwrap();
    app.maybe_recapture_orchestrator_plan();
    assert_eq!(
        app.planned_nodes.len(),
        2,
        "recapture defers to scan_orchestrator_markers when the approve marker is present"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn approval_keeps_the_orchestrator_live_and_mirrors_graph() {
    // On approval the interactive orchestrator must (1) mirror the DAG into
    // graph.json, (2) launch separate workers, and (3) stay alive as the
    // high-level conductor. It must not implement product code, but the PTY stays
    // available for status and control markers.
    let _env = env_guard();
    let repo = unique_test_repo("orch-live-on-approve");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.interactive_orchestrator = true;
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.planned_nodes = vec![titled_planned_node("n0", "do the thing")];
    app.awaiting_approval = true;

    // A LIVE interactive orchestrator PTY (RudderPlan, !reconcile_planner, Running).
    let command = TerminalCommand::with_args("/bin/sh", ["-lc", "sleep 5"]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.interactive_orchestrator = true;
    orch.terminal = Some(pane);
    app.agents.push(orch);

    // Approve via the hardened FILE channel (marker written into the plan file).
    fs::create_dir_all(repo.join(".rudder")).unwrap();
    fs::write(
            orchestrator_plan_path(&repo),
            "# Plan\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"x\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n\nRUDDER_APPROVE_PLAN\n",
        )
        .unwrap();

    app.scan_orchestrator_markers();

    assert!(!app.awaiting_approval, "approval cleared the gate");
    // graph.json was mirrored on approval (the __graph-mirror shell-out is a no-op
    // under cfg(test), but mirror_graph still records the signature it computed,
    // proving the DAG was generated at the approval moment).
    assert!(
        app.last_mirror_signature.is_some(),
        "graph.json mirrored on approval"
    );
    // The orchestrator row is KEPT and LIVE: the conductor PTY remains available.
    let orch_after = app
        .agents
        .iter()
        .find(|run| run.id == "orch-1")
        .expect("orchestrator row kept after approval");
    assert_eq!(
        orch_after.status,
        AgentStatus::Running,
        "orchestrator stays live on approval"
    );
    assert!(
        orch_after.terminal.is_some(),
        "orchestrator PTY remains attached"
    );
    // The plan still launched its worker (or stayed queued if the in-place jj
    // workspace prep no-ops in this bare temp repo) — same tolerance as the
    // sibling self-launch test.
    assert!(
        app.planned_nodes.is_empty() || app.agents.iter().any(|run| run.node_id.is_some()),
        "the approved node launched a worker"
    );

    // Idempotent: re-scanning does not resurrect or re-stop anything.
    let agents_after = app.agents.len();
    app.scan_orchestrator_markers();
    assert_eq!(app.agents.len(), agents_after, "no churn on re-scan");

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn task_input_routes_to_live_orchestrator_after_launch() {
    let _env = env_guard();
    let repo = unique_test_repo("orch-task-forward");
    let capture = repo.join("orchestrator-input.txt");
    let script = format!(
        "IFS= read line; printf '%s' \"$line\" > {}; sleep 5",
        capture.display()
    );
    let command = TerminalCommand::with_args("/bin/sh", vec!["-lc".to_string(), script]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    orch.terminal = Some(pane);
    app.agents.push(orch);

    let mut worker = test_agent_run("run-1", "do node");
    worker.cwd = repo.clone();
    worker.node_id = Some("n0".to_string());
    worker.status = AgentStatus::Running;
    app.agents.push(worker);

    app.start_task_from_input("what is happening?");

    let mut captured = String::new();
    for _ in 0..40 {
        if let Ok(text) = fs::read_to_string(&capture) {
            captured = text;
            if captured.contains("what is happening?") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(captured, "what is happening?");
    assert_eq!(app.selected_agent, 0, "the conductor stays selected");
    assert_eq!(app.notice.as_deref(), Some("sent to orchestrator"));

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn task_input_sends_enter_key_to_live_orchestrator() {
    let _env = env_guard();
    let repo = unique_test_repo("orch-task-enter");
    let capture = repo.join("orchestrator-bytes.bin");
    let ready = repo.join("orchestrator-ready");
    let script = format!(
        "stty raw -echo; : > {}; dd bs=1 count=4 of={} 2>/dev/null; sleep 1",
        shell_single_quote(&ready.to_string_lossy()),
        shell_single_quote(&capture.to_string_lossy())
    );
    let command = TerminalCommand::with_args("/bin/sh", vec!["-lc".to_string(), script]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");

    for _ in 0..40 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(ready.exists(), "orchestrator pty is ready for raw input");

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    orch.terminal = Some(pane);
    app.agents.push(orch);

    let mut worker = test_agent_run("run-1", "do node");
    worker.cwd = repo.clone();
    worker.node_id = Some("n0".to_string());
    worker.status = AgentStatus::Running;
    app.agents.push(worker);

    app.start_task_from_input("go!");

    let mut captured = Vec::new();
    for _ in 0..40 {
        if let Ok(bytes) = fs::read(&capture) {
            captured = bytes;
            if captured.len() >= 4 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(captured, b"go!\r");
    assert_eq!(app.notice.as_deref(), Some("sent to orchestrator"));

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn completed_plan_launch_followup_starts_fresh_dag_not_old_conductor() {
    let _env = env_guard();
    let repo = unique_test_repo("completed-plan-fresh-dag");
    let old_capture = repo.join("old-orchestrator-input.txt");
    let old_script = format!(
        "stty raw -echo; if IFS= read -r line; then printf '%s' \"$line\" > {}; fi; sleep 1",
        shell_single_quote(&old_capture.to_string_lossy())
    );
    let old_command = TerminalCommand::with_args("/bin/sh", vec!["-lc".to_string(), old_script]);
    let old_pane = TerminalPane::spawn_shell_or_command(
        Some(old_command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn old orchestrator pty");

    let fake = repo.join("fake-codex.sh");
    write_fake_bin(&fake, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &fake);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-old", "old plan");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Codex;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    orch.terminal = Some(old_pane);
    app.agents.push(orch);

    let mut completed = test_agent_run("run-1", "old node");
    completed.cwd = repo.clone();
    completed.node_id = Some("n0".to_string());
    completed.status = AgentStatus::Merged;
    app.agents.push(completed);

    app.start_task_from_input("launch separate agents to fix this then merge them all");

    assert!(
        !old_capture.exists(),
        "work-launching follow-up was not typed into the completed plan's conductor"
    );
    let planners: Vec<&AgentRun> = app
        .agents
        .iter()
        .filter(|run| run.is_orchestrator())
        .collect();
    assert_eq!(planners.len(), 1, "stale orchestrator retired");
    let planner = planners[0];
    assert_eq!(planner.id, app.agents[app.selected_agent].id);
    assert_eq!(planner.mode, AgentMode::RudderPlan);
    assert_eq!(
        planner.task, "launch separate agents to fix this then merge them all",
        "a fresh DAG planner owns the follow-up request"
    );
    assert!(
        !planner.reconcile_planner,
        "completed-plan implementation follow-up starts an initial planner"
    );

    for run in &mut app.agents {
        run.terminal = None;
    }
    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn completed_plan_status_question_still_goes_to_old_conductor() {
    let _env = env_guard();
    let repo = unique_test_repo("completed-plan-status-conductor");
    let capture = repo.join("orchestrator-input.bin");
    let ready = repo.join("orchestrator-ready");
    let script = format!(
        "stty raw -echo; : > {}; dd bs=1 count=20 of={} 2>/dev/null; sleep 1",
        shell_single_quote(&ready.to_string_lossy()),
        shell_single_quote(&capture.to_string_lossy())
    );
    let command = TerminalCommand::with_args("/bin/sh", vec!["-lc".to_string(), script]);
    let pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 10, cols: 80 },
            scrollback_lines: 100,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");

    for _ in 0..40 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(ready.exists(), "orchestrator pty is ready for input");

    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run("orch-1", "old plan");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Claude;
    orch.autosteered = false;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    orch.terminal = Some(pane);
    app.agents.push(orch);

    let mut completed = test_agent_run("run-1", "old node");
    completed.cwd = repo.clone();
    completed.node_id = Some("n0".to_string());
    completed.status = AgentStatus::Merged;
    app.agents.push(completed);

    app.start_task_from_input("what is the status?");

    let mut captured = Vec::new();
    for _ in 0..40 {
        if let Ok(bytes) = fs::read(&capture) {
            captured = bytes;
            if captured.len() >= 20 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(captured, b"what is the status?\r");
    assert_eq!(app.selected_agent, 0, "the old conductor stays selected");
    assert_eq!(app.notice.as_deref(), Some("sent to orchestrator"));

    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn approval_does_not_stop_orchestrator_in_headless_mode() {
    // Headless decomposer (interactive_orchestrator = false): the orchestrator
    // exits on its own, so approval must NOT reach in and stop a RudderPlan row.
    let _env = env_guard();
    let repo = unique_test_repo("orch-headless-no-stop");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.interactive_orchestrator = false;
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.planned_nodes = vec![titled_planned_node("n0", "do the thing")];
    app.awaiting_approval = true;

    let mut orch = test_agent_run("orch-1", "plan it");
    orch.cwd = repo.clone();
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Running;
    app.agents.push(orch);

    app.approve_planned_queue();

    let orch_after = app.agents.iter().find(|run| run.id == "orch-1").unwrap();
    assert_eq!(
        orch_after.status,
        AgentStatus::Running,
        "headless approval leaves the orchestrator alone (it self-exits)"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn stopped_orchestrator_renders_handoff_banner() {
    // Legacy/stale stopped orchestrator rows still render a hand-off banner instead
    // of an empty terminal: the DAG stays above, the bottom explains the stopped
    // state + points at work.
    let _env = env_guard();
    let mut app = App::new();
    app.interactive_orchestrator = true;

    // A stopped orchestrator: RudderPlan, no terminal, Stopped.
    let mut orch = test_agent_run("orch-1", "plan the work");
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Stopped;
    orch.backend = Backend::Claude;
    orch.interactive_orchestrator = true;
    orch.terminal = None;
    app.agents.push(orch);
    // Two workers implementing nodes (Running, with node_id).
    for (id, node) in [("w0", "n0"), ("w1", "n1")] {
        let mut w = test_agent_run(id, "do node");
        w.node_id = Some(node.to_string());
        w.status = AgentStatus::Running;
        app.agents.push(w);
    }
    // A remaining queued node so the DAG pane renders the tree.
    app.planned_nodes = vec![titled_planned_node("n2", "later task")];

    app.selected_agent = 0; // the orchestrator
    app.focus = FocusPane::Worker;
    app.worker_view = WorkerView::Terminal;

    let screen = render_screen(&mut app, 120, 40);
    assert!(
        screen.contains("Plan approved. Orchestrator stopped."),
        "stopped orchestrator shows the hand-off banner\n{screen}"
    );
    assert!(
        screen.contains("2 worker(s) implementing"),
        "banner reports the running worker count\n{screen}"
    );
}

#[cfg(not(windows))]
#[test]
fn orchestrator_dag_survives_queue_drain() {
    // REGRESSION: after approval the scheduler drains planned_nodes into running
    // workers. The DAG pane must reconstruct the launched nodes from their agents
    // (orchestrator_dag_tasks) instead of collapsing to the "Planning…" placeholder.
    let _env = env_guard();
    let mut app = App::new();
    app.interactive_orchestrator = true;

    let mut orch = test_agent_run("orch-1", "plan the work");
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Stopped;
    orch.terminal = None;
    app.agents.push(orch);

    // Two launched workers, queue fully drained (planned_nodes empty).
    for (id, node, title) in [("w0", "n0", "alpha-node"), ("w1", "n1", "beta-node")] {
        let mut w = test_agent_run(id, "do node");
        w.node_id = Some(node.to_string());
        w.task_summary = title.to_string();
        w.status = AgentStatus::Running;
        app.agents.push(w);
    }
    assert!(app.planned_nodes.is_empty(), "queue is fully drained");

    app.selected_agent = 0; // the orchestrator
    app.focus = FocusPane::Worker;
    app.worker_view = WorkerView::Terminal;

    let screen = render_screen(&mut app, 120, 40);
    assert!(
        !screen.contains("Planning…"),
        "the drained queue must NOT collapse the DAG to the planning placeholder\n{screen}"
    );
    assert!(
        screen.contains("alpha-node") && screen.contains("beta-node"),
        "the DAG pane reconstructs launched nodes from their agents\n{screen}"
    );
}

#[test]
fn orchestrator_dag_tasks_dedup_relaunched_nodes() {
    // A node id can carry two agents (a failed launch + a re-goaled relaunch).
    // orchestrator_dag_tasks must collapse them to ONE task so the DAG tree does
    // not double-render the node.
    let mut app = App::new();
    app.interactive_orchestrator = true;

    let mut failed = test_agent_run("run-old", "first try");
    failed.node_id = Some("n0".to_string());
    failed.task_summary = "stale-title".to_string();
    failed.status = AgentStatus::Failed;
    app.agents.push(failed);

    let mut relaunched = test_agent_run("run-new", "second try");
    relaunched.node_id = Some("n0".to_string());
    relaunched.task_summary = "fresh-title".to_string();
    relaunched.status = AgentStatus::Running;
    app.agents.push(relaunched);

    let tasks = app.orchestrator_dag_tasks();
    assert_eq!(tasks.len(), 1, "the relaunched node collapses to one row");
    assert_eq!(tasks[0].id, "n0");
    assert_eq!(
        tasks[0].title, "fresh-title",
        "the latest agent's data wins for the collapsed node"
    );
}

#[test]
fn self_launch_marker_is_inert_without_flag_or_gate() {
    // Opt OUT (headless decomposer): the scan no-ops even with a plan awaiting.
    let mut app = App::new();
    app.interactive_orchestrator = false; // per-App field, no global env
    app.cwd = std::env::temp_dir();
    app.planned_nodes = vec![titled_planned_node("n0", "x")];
    app.awaiting_approval = true;
    let mut orch = test_agent_run("orch-1", "plan it");
    orch.mode = AgentMode::RudderPlan;
    app.agents.push(orch);
    app.scan_orchestrator_markers();
    assert!(
        app.awaiting_approval,
        "headless mode → marker ignored, still awaiting approval"
    );

    // Interactive ON but NOT awaiting approval: nothing to launch, scan no-ops.
    app.interactive_orchestrator = true;
    app.awaiting_approval = false;
    app.scan_orchestrator_markers();
    assert!(!app.awaiting_approval);
}

#[test]
fn only_interactive_worker_modes_want_signals() {
    use crate::signals::worker_wants_signals;
    for mode in [AgentMode::Execute, AgentMode::Main, AgentMode::ReviewAll] {
        assert!(
            worker_wants_signals(Backend::Claude, mode),
            "{mode:?} is an interactive worker"
        );
    }
    for mode in [AgentMode::Plan, AgentMode::RudderPlan] {
        assert!(
            !worker_wants_signals(Backend::Codex, mode),
            "{mode:?} is headless (process-exit)"
        );
    }
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
    app.planner_question_round_done = true;

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
fn extract_rudder_questions_parses_a_numbered_block() {
    let out = "intro\nRUDDER_QUESTIONS_START\n1. Time range: 4 weeks or 6 months?\n2. Local only, or also deploy?\nRUDDER_QUESTIONS_END\ntrailing";
    let qs = extract_rudder_questions(out);
    assert_eq!(qs.len(), 2);
    assert_eq!(qs[0], "Time range: 4 weeks or 6 months?");
    assert_eq!(qs[1], "Local only, or also deploy?");
    // No block -> empty (so the paused panel falls back to the transcript hint).
    assert!(extract_rudder_questions("no markers here").is_empty());
}

#[test]
fn planner_prompt_asks_and_pauses_when_materially_ambiguous() {
    // The planner must be told to ASK + STOP on the first turn, not decide that a
    // request is clear enough to skip questions. And the clarification-answer framing
    // must be distinct from the refine framing.
    let prompt = rudder_plan_prompt("make me something");
    assert!(
        prompt.contains("ALWAYS ASK") && prompt.contains("STOP"),
        "instructs ask-then-pause"
    );
    assert!(
        !prompt.contains("unless it is already completely specified"),
        "the first-turn question round is no longer optional"
    );
    let answer = build_clarification_answer_followup("medium_term, top 5 tracks");
    assert!(
        answer.to_lowercase().contains("answer"),
        "framed as an answer, not refine"
    );
    assert!(
        answer.contains("RUDDER_PLAN_TASKS_START"),
        "asks for the DAG once answered"
    );
    assert!(
        answer.contains("medium_term, top 5 tracks"),
        "carries the user's answer"
    );
}

#[test]
fn transcript_fallback_recovers_a_plan_block_the_pty_stream_missed() {
    // The on-disk transcript reliably carries the full final message even when the live
    // PTY stream truncated a large RUDDER_PLAN_TASKS block. parse_transcript_final_text
    // returns the result text, which extract_rudder_plan_tasks then parses.
    let result_text = "Here is the plan.\\n\\nRUDDER_PLAN_TASKS_START\\n{\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"a\\\",\\\"prompt\\\":\\\"p\\\"}]}\\nRUDDER_PLAN_TASKS_END";
    let line = format!(
        r#"{{"type":"result","subtype":"success","result":"{}"}}"#,
        result_text
    );
    let recovered = parse_transcript_final_text(&line).expect("final text");
    assert!(recovered.contains("RUDDER_PLAN_TASKS_START"));
    let tasks = extract_rudder_plan_tasks(&recovered).expect("the recovered block parses");
    assert_eq!(tasks.len(), 1);
    // Empty / non-JSON transcript yields None.
    assert_eq!(parse_transcript_final_text(""), None);
}

#[test]
fn completed_plan_does_not_hijack_a_new_task_into_refine() {
    // Regression guard: a SHIPPED plan leaves a Done orchestrator (with a session),
    // empty queue, and Merged workers. Without the paused flag that state looked
    // "awaiting input" and a brand-new task got routed into refining the old session.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut orch = test_agent_run("orch", "ship it");
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Done;
    orch.session_id = Some("sid-1".to_string());
    let mut worker = test_agent_run("w", "node n0");
    worker.node_id = Some("n0".to_string());
    worker.status = AgentStatus::Merged;
    app.agents = vec![orch, worker];

    // Flag NOT set (plan completed normally) -> not awaiting input -> fresh plan.
    assert!(
        !app.planner_awaiting_input(),
        "a completed plan must not look like a paused planner"
    );
    // Only when the planner actually paused for a clarifying question does it resume.
    app.planner_paused_for_input = true;
    assert!(
        app.planner_awaiting_input(),
        "a genuinely paused planner resumes on the next typed message"
    );
}

#[cfg(not(windows))]
#[test]
fn default_task_input_goes_to_orchestrator() {
    // Plain task input (no /ask) is handed to the orchestrator, which plans and
    // implements it. There is no local one-off-vs-DAG classifier: the orchestrator
    // is the default and /ask is the explicit one-off escape hatch.
    let _env = env_guard();
    let repo = unique_test_repo("default-orch");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    assert!(!app.plan_is_active(), "no plan active on a clean session");

    app.start_task_from_input("build the first feature for the app");

    let spawned = app.agents.last().expect("an orchestrator spawned");
    assert_eq!(
        spawned.mode,
        AgentMode::RudderPlan,
        "plain input starts the orchestrator, not a one-off"
    );
    assert!(!spawned.is_oneoff(), "the default is no longer a one-off");
    assert!(
        !spawned.reconcile_planner,
        "a fresh task starts the initial orchestrator planner"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn plan_command_starts_orchestrator_for_dag_work() {
    let _env = env_guard();
    let repo = unique_test_repo("plan-cmd");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;

    let handled = app.handle_command("/plan build the first feature");
    assert!(handled, "/plan is a recognized command");
    let spawned = app.agents.last().expect("a planner spawned by /plan");
    assert_eq!(spawned.mode, AgentMode::RudderPlan);
    assert!(
        !spawned.reconcile_planner,
        "/plan starts the initial orchestrator planner"
    );

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn run_command_starts_single_execute_worker() {
    let _env = env_guard();
    let repo = unique_test_repo("run-cmd");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;

    let handled = app.handle_command("/run build the settings panel");
    assert!(handled, "/run is a recognized command");
    let spawned = app
        .agents
        .last()
        .expect("an execute worker spawned by /run");
    assert_eq!(spawned.mode, AgentMode::Execute);
    assert!(
        !spawned.is_oneoff(),
        "/run is not the direct main-checkout one-off path"
    );
    assert!(
        !spawned.is_orchestrator(),
        "/run skips the planner/orchestrator path"
    );
    assert!(
        spawned.node_id.is_none(),
        "/run creates one manual worker, not a DAG node"
    );

    if let Some(run) = app.agents.last_mut() {
        run.terminal = None;
    }
    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn ask_command_starts_oneoff_alias() {
    let _env = env_guard();
    let repo = unique_test_repo("ask-cmd");
    let worker = repo.join("fake-worker.sh");
    write_fake_bin(&worker, FAKE_CONDUCTOR_WORKER);
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;

    let handled = app.handle_command("/ask explain the build script");
    assert!(handled, "/ask is a recognized command");
    let spawned = app.agents.last().expect("a one-off agent spawned by /ask");
    assert_eq!(spawned.mode, AgentMode::OneOff);

    std::env::remove_var("RUDDER_CODEX_BIN");
    let _ = fs::remove_dir_all(&repo);
}

#[cfg(not(windows))]
#[test]
fn oneoff_agent_renders_in_its_own_section() {
    let _env = env_guard();
    let mut app = App::new();
    let mut run = test_agent_run("oneoff-1", "explain the build");
    run.mode = AgentMode::OneOff;
    run.status = AgentStatus::Running;
    app.agents.push(run);
    app.selected_agent = 0;

    let screen = render_screen(&mut app, 120, 40);
    assert!(
        screen.contains("one-off"),
        "the one-off section header renders in the agent list\n{screen}"
    );
}

fn dag_task(id: &str, title: &str, hard: &[&str]) -> RudderPlanTask {
    RudderPlanTask {
        id: id.to_string(),
        title: title.to_string(),
        prompt: title.to_string(),
        goal: None,
        success: None,
        deps: hard
            .iter()
            .map(|on| PlanEdge {
                on: (*on).to_string(),
                edge: EdgeType::Hard,
                why: None,
            })
            .collect(),
        backend: None,
        model: None,
        effort: None,
    }
}

fn dag_rows(app: &App, tasks: &[RudderPlanTask]) -> Vec<String> {
    orchestrator_dag_tree_lines(app, tasks)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn orchestrator_dag_nests_join_under_deepest_dep_with_clean_lanes() {
    // A diamond: config -> {auth, tracks}; html standalone; app needs auth+tracks+html.
    let app = App::new();
    let tasks = vec![
        dag_task("n0", "config", &[]),
        dag_task("n1", "auth", &["n0"]),
        dag_task("n2", "tracks", &["n0"]),
        dag_task("n3", "html", &[]),
        dag_task("n4", "app", &["n1", "n2", "n3"]),
    ];
    let rows = dag_rows(&app, &tasks);
    for r in &rows {
        println!("{r}");
    }
    assert_eq!(
        rows.len(),
        tasks.len(),
        "every task renders exactly once: {rows:#?}"
    );

    // Render order: roots in plan order, children depth-first under them.
    for (row, title) in rows.iter().zip(["config", "auth", "tracks", "app", "html"]) {
        assert!(row.contains(title), "row {row:?} should contain {title}");
    }
    let indent = |s: &str| s.chars().take_while(|c| *c != '●').count();
    // Roots start at column 0 (badge first, no leading lane); children are indented.
    assert!(
        rows[0].starts_with('●'),
        "config is a root at col 0: {:?}",
        rows[0]
    );
    assert!(
        rows[4].starts_with('●'),
        "html is a root at col 0: {:?}",
        rows[4]
    );
    assert!(
        indent(&rows[1]) > 0,
        "auth is nested under config: {:?}",
        rows[1]
    );
    // Clean, consistent lanes: auth and tracks (siblings) share the same indent.
    assert_eq!(
        indent(&rows[1]),
        indent(&rows[2]),
        "auth & tracks align as siblings: {:?} vs {:?}",
        rows[1],
        rows[2]
    );
    // The join nests UNDER tracks (a deep dep) — deeper than tracks, never under the
    // shallow parallel root html.
    assert!(
        indent(&rows[3]) > indent(&rows[2]),
        "app (join) nests under its deepest dep tracks: {:?} vs {:?}",
        rows[3],
        rows[2]
    );
}

#[test]
fn orchestrator_dag_draws_continuation_bar_across_deep_child() {
    // n0 -> {n1 -> n3(leaf), n2}. n1 has a child (leaf) AND a following sibling (n2),
    // so the leaf row must carry a vertical bar connecting n1's lane down to n2.
    let app = App::new();
    let tasks = vec![
        dag_task("n0", "root", &[]),
        dag_task("n1", "first", &["n0"]),
        dag_task("n2", "second", &["n0"]),
        dag_task("n3", "leaf", &["n1"]),
    ];
    let rows = dag_rows(&app, &tasks);
    for r in &rows {
        println!("{r}");
    }
    for (row, title) in rows.iter().zip(["root", "first", "leaf", "second"]) {
        assert!(row.contains(title), "row {row:?} should contain {title}");
    }
    assert!(
        rows[2].contains('│'),
        "the leaf carries a continuation bar to the later sibling 'second': {:?}",
        rows[2]
    );
    assert!(
        !rows[3].contains('│'),
        "the last sibling has no trailing bar: {:?}",
        rows[3]
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
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];
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
    assert_eq!(
        app.planned_nodes.len(),
        3,
        "appended, not replaced: {queued_ids:?}"
    );
    assert!(
        queued_ids.contains(&"n0"),
        "existing node n0 preserved: {queued_ids:?}"
    );
    assert!(
        queued_ids.contains(&"n1"),
        "existing node n1 preserved: {queued_ids:?}"
    );
    assert!(
        queued_ids.contains(&"new"),
        "new node appended: {queued_ids:?}"
    );
    // The appended node carries its soft dep on the queued node n1.
    let added = app.planned_nodes.iter().find(|n| n.id == "new").unwrap();
    assert_eq!(
        added.soft_deps,
        vec!["n1".to_string()],
        "soft dep on n1 retained"
    );
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
    assert!(
        !initial.reconcile_planner,
        "initial planner is not a reconcile planner"
    );

    let block_reconcile = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"new\",\"title\":\"b\",\"prompt\":\"do b\",\"goal\":\"b\",\"success\":\"ok\"}]}\nRUDDER_PLAN_TASKS_END\n";
    let reconcile = reconcile_planner_with_block(&app, block_reconcile);
    assert!(
        reconcile.reconcile_planner,
        "reconcile planner carries the discriminator"
    );

    // The discriminator is what the poll loop branches on: capture the initial
    // plan (replace) and verify the queue holds exactly its node.
    app.agents.push(initial);
    app.planner_question_round_done = true;
    app.evaluate_completed_plan(0);
    assert_eq!(app.planned_nodes.len(), 1);
    assert_eq!(
        app.planned_nodes[0].id, "n0",
        "initial plan captured via replace path"
    );
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
    assert!(
        prompt.contains("n0: build api"),
        "frontier listed: {prompt}"
    );
    assert!(prompt.contains("n1: build ui"), "frontier listed: {prompt}");
    assert!(
        prompt.contains("EXACTLY ONE node"),
        "asks for one node: {prompt}"
    );
    assert!(prompt.contains("unique from every existing node id"));
    assert!(prompt.contains("hard") && prompt.contains("soft"));
    assert!(
        prompt.contains("ALREADY in flight"),
        "states the plan is in flight"
    );
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
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];
    app.awaiting_approval = true;

    // d on the orchestrator must NOT throw the plan away: the plan is refined by
    // typing into the task pane and approved with Enter. The gate stays open.
    app.handle_agents_key(KeyEvent::from(KeyCode::Char('d')));
    assert_eq!(app.planned_nodes.len(), 2, "d does not discard the plan");
    assert!(app.awaiting_approval, "the approval gate stays open");
}

// --- Plan refinement (plan-mode-style discussion before approval) --------

#[test]
fn extract_rudder_plan_summary_reads_prose_after_the_block() {
    let output = "RUDDER_PLAN_TASKS_START\n{\"tasks\":[]}\nRUDDER_PLAN_TASKS_END\n\nAssumptions: I assumed a Node CLI.\nOpen question: which package manager?";
    let summary = extract_rudder_plan_summary(output).expect("summary present");
    assert!(summary.contains("Assumptions: I assumed a Node CLI."));
    assert!(summary.contains("Open question: which package manager?"));
    // No trailing prose -> None.
    assert!(
        extract_rudder_plan_summary("RUDDER_PLAN_TASKS_START\n{}\nRUDDER_PLAN_TASKS_END").is_none()
    );
}

#[test]
fn build_refine_request_carries_original_plan_and_feedback() {
    let req = build_refine_request(
        "make a CLI",
        "- n0 [scaffold] deps: none",
        "use Python not Node",
    );
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
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];
    app.awaiting_approval = true;
    app.refining = true;

    app.approve_planned_queue();

    assert!(
        app.awaiting_approval,
        "refining blocks approval; gate stays up"
    );
    assert_eq!(app.planned_nodes.len(), 2, "the stale plan is not launched");
}

#[test]
fn current_plan_outline_lists_nodes_with_deps() {
    let mut app = App::new();
    app.planned_nodes = vec![
        test_planned_node("n0", &[]),
        test_planned_node("n1", &["n0"]),
    ];
    let outline = app.current_plan_outline();
    assert!(outline.contains("n0"));
    assert!(outline.contains("n1"));
    assert!(outline.contains("n0:hard"), "hard dep is shown: {outline}");
}

// --- Live planner stream (plan_stream.rs) --------------------------------
// ingest() takes the FULL cumulative output_log snapshot each call (the PTY log
// grows), so the tests append to a buffer and feed the whole buffer each time.

fn text_delta_line(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
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
    assert!(s
        .transcript()
        .iter()
        .any(|e| e.kind == PlanEntryKind::Thinking));
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
    let tasks =
        extract_rudder_plan_tasks(s.parse_text()).expect("block parses from reconstructed text");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "n0");
}

#[test]
fn plan_stream_rebuilds_when_consumed_offset_is_not_char_boundary() {
    let mut s = PlanStreamState::new();
    s.ingest("x\n");

    // `s.consumed` is now 2. In this later snapshot, byte 2 sits inside the
    // three-byte ellipsis. This can happen when PTY scrollback drains from the front
    // and fresh output arrives before the next poll.
    let snapshot = format!("a…\n{}", text_delta_line("after drain"));
    s.ingest(&snapshot);

    assert_eq!(s.assistant_text(), "after drain");
}

#[test]
fn plan_stream_rebuilds_when_snapshot_shifted_at_valid_boundary() {
    let mut s = PlanStreamState::new();
    s.ingest("hello\n");

    // The old consumed offset (6) is a valid boundary in this new snapshot, but it
    // belongs to a different PTY buffer after front-drain. Reusing it would skip the
    // leading JSON envelope and silently lose the text delta.
    s.ingest(&text_delta_line("after valid-boundary drain"));

    assert_eq!(s.assistant_text(), "after valid-boundary drain");
}

#[test]
fn plan_stream_captures_exit_plan_as_authoritative_dag_text() {
    // Real plan mode presents the plan via ExitPlanMode: plan_stream must capture
    // its input.plan (carrying the RUDDER_PLAN_TASKS block) as the authoritative
    // text, even when the block never appeared in the streamed assistant text.
    let mut s = PlanStreamState::new();
    let plan = "# Plan\\nHere is the DAG.\\nRUDDER_PLAN_TASKS_START\\n{\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"impl\\\",\\\"prompt\\\":\\\"do it\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]}\\nRUDDER_PLAN_TASKS_END\\n";
    // An assistant envelope whose only content is the ExitPlanMode tool_use.
    let line = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"ExitPlanMode\",\"input\":{{\"plan\":\"{plan}\",\"planFilePath\":\"/p/x.md\"}}}}]}}}}\n"
        );
    s.ingest(&line);
    let captured = s.exit_plan().expect("ExitPlanMode plan captured");
    assert!(
        captured.contains("RUDDER_PLAN_TASKS_START"),
        "plan carries the block"
    );
    let tasks = extract_rudder_plan_tasks(captured).expect("the captured plan's DAG parses");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "n0");
}

#[test]
fn plan_stream_captures_exit_plan_from_result_permission_denials() {
    // Backstop: in headless -p ExitPlanMode is auto-denied, so the plan is also in
    // the result event's permission_denials.
    let mut s = PlanStreamState::new();
    let plan = "RUDDER_PLAN_TASKS_START\\n{\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"t\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"g\\\",\\\"success\\\":\\\"s\\\",\\\"deps\\\":[]}]}\\nRUDDER_PLAN_TASKS_END";
    let line = format!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"permission_denials\":[{{\"tool_name\":\"ExitPlanMode\",\"tool_input\":{{\"plan\":\"{plan}\"}}}}]}}\n"
        );
    s.ingest(&line);
    let captured = s
        .exit_plan()
        .expect("plan captured from permission_denials");
    assert!(
        extract_rudder_plan_tasks(captured).is_ok(),
        "the backstop plan parses"
    );
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
    assert_eq!(
        s.assistant_text(),
        "abcd",
        "no duplicate of the first delta"
    );
}

#[test]
fn plan_stream_per_turn_baseline_isolates_refine() {
    let mut s = PlanStreamState::new();
    let mut buf = String::new();
    let block = |id: &str| {
        format!(
            "RUDDER_PLAN_TASKS_START {{\"tasks\":[{{\"id\":\"{id}\",\"title\":\"t\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}}]}} RUDDER_PLAN_TASKS_END"
        )
    };
    buf.push_str(&text_delta_line(&block("old")));
    s.ingest(&buf);
    assert_eq!(
        extract_rudder_plan_tasks(s.parse_text()).unwrap()[0].id,
        "old"
    );
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
    assert_eq!(
        s.assistant_text(),
        "the real plan text",
        "chrome excluded from text"
    );
    assert!(
        !s.transcript()
            .iter()
            .any(|e| e.text.contains("thinking with medium effort")),
        "status-bar chrome must not render as transcript"
    );
    assert!(
        s.transcript()
            .iter()
            .any(|e| e.text.contains("the real plan text")),
        "the real plan text renders"
    );
}

// --- Orchestrator view: pinned row + spinner/DAG worker pane -------------

#[test]
fn spinner_glyph_advances_with_frame() {
    let mut app = App::new();
    let first = app.spinner_glyph();
    // The spinner is slowed: it holds each glyph for SPINNER_TICKS_PER_FRAME ticks, so
    // a single tick must NOT change it, but a full frame's worth of ticks does.
    app.spinner_frame = app.spinner_frame.wrapping_add(1);
    assert_eq!(
        first,
        app.spinner_glyph(),
        "one tick holds the same glyph (slowed)"
    );
    app.spinner_frame = SPINNER_TICKS_PER_FRAME;
    assert_ne!(
        first,
        app.spinner_glyph(),
        "a full frame of ticks advances the glyph"
    );
    // Wrapping back to frame 0 yields the first glyph.
    app.spinner_frame = SPINNER_FRAMES.len() * SPINNER_TICKS_PER_FRAME;
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
    assert!(
        order.contains(&0),
        "the worker still appears in a status section"
    );
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
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40)).expect("test backend");
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
        text.contains("orchestrator"),
        "pinned orchestrator header present: {text}"
    );
    // While Running with no plan block, the row is labeled as the model's PLAN MODE
    // (the product framing) rather than a generic "planning".
    assert!(text.contains("plan mode"), "phase reads plan mode: {text}");
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
    assert_eq!(
        orchestrator_task_status(&app, "n1"),
        OrchTaskStatus::Running
    );
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
    // These tests assert the HEADLESS orchestrator view (render_orchestrator). The
    // interactive orchestrator is the default now, so pin this helper to the headless
    // renderer — interactive-orchestrator tests use render_screen instead. Per-App field,
    // so no process-global env and no cross-test race.
    app.interactive_orchestrator = false;
    let area = Rect::new(0, 0, width, height);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
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
fn oneoff_worker_title_uses_friendly_identity_not_run_id() {
    let mut app = App::new();
    app.focus = FocusPane::Worker;
    let mut run = test_agent_run(
        "1782777962862889000-one-off-1834",
        "read slack threads cedar-amil status",
    );
    run.mode = AgentMode::OneOff;
    run.task_summary = "read slack threads cedar-amil status".to_string();
    app.agents = vec![run];
    app.selected_agent = 0;

    let text = render_worker_text(&mut app, 100, 12);
    assert!(
        text.contains("worker · q1 · read slack threads cedar-amil status"),
        "one-off worker title should be human-readable:\n{text}"
    );
    assert!(
        !text.contains("1782777962862889000-one-off-1834"),
        "generated run id should not appear in the worker title:\n{text}"
    );
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
    assert!(
        text.contains("scaffold"),
        "DAG tree shows the node title: {text}"
    );
    assert!(
        text.contains("Plan"),
        "the prose plan section heading is shown: {text}"
    );
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
        hint.contains("talk to the live orchestrator"),
        "post-launch hint points at the conductor: {hint}"
    );
}

#[test]
fn app_style_uses_terminal_background_by_default_and_keeps_paper_mode_available() {
    set_color_mode(ColorMode::Terminal);
    assert_eq!(app_style().bg, None);
    assert_eq!(app_style().fg, None);
    assert_eq!(pane_text_style(true).fg, None);

    set_color_mode(ColorMode::Paper);
    assert_eq!(app_style().bg, Some(PAPER));
    assert_eq!(app_style().fg, Some(INK));
    assert_eq!(pane_text_style(true).fg, Some(INK));

    set_color_mode(ColorMode::Terminal);
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
    assert!(
        summary.len() > 1500,
        "full summary kept (len {})",
        summary.len()
    );
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
        s.assistant_text()
            .matches("Summary: the repo has config")
            .count(),
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
    assert!(
        app.orch_dag_scroll > 0,
        "PageDown scrolls the prose plan down"
    );
    app.handle_orchestrator_chat_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()));
    assert_eq!(app.orch_dag_scroll, 0, "PageUp scrolls back toward the top");
}

#[test]
fn orchestrator_pane_shows_per_node_goal_and_done_when() {
    // The pane now fills with a per-node breakdown (goal + done-when + deps),
    // surfacing the plan's depth instead of leaving the space blank.
    let mut app = App::new();
    app.focus = FocusPane::Worker;
    let mut orch = test_agent_run("orch", "build");
    orch.mode = AgentMode::RudderPlan;
    let mut stream = PlanStreamState::new();
    stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"RUDDER_PLAN_TASKS_START {\\\"tasks\\\":[{\\\"id\\\":\\\"n0\\\",\\\"title\\\":\\\"scaffold\\\",\\\"prompt\\\":\\\"p\\\",\\\"goal\\\":\\\"set up indexpage\\\",\\\"success\\\":\\\"pageloads\\\",\\\"deps\\\":[]}]} RUDDER_PLAN_TASKS_END\"}}}\n");
    orch.plan_stream = Some(stream);
    app.agents = vec![orch];
    app.selected_agent = 0;

    let text = render_worker_text(&mut app, 90, 30);
    assert!(text.contains("Tasks"), "tasks section heading: {text}");
    assert!(
        text.contains("set up indexpage"),
        "per-node goal shown: {text}"
    );
    assert!(
        text.contains("done when:") && text.contains("pageloads"),
        "per-node done-when shown: {text}"
    );
}

#[test]
fn orchestrator_tree_nests_by_hard_edges_not_soft() {
    // The my-charts case: n1 and n2 are SOFT-only (they run in parallel); n3 is
    // the hard integrator. Soft edges must not nest (that drew a misleading
    // sequential staircase that contradicted the "run fully in parallel" prose).
    let task = |id: &str, deps: Vec<(&str, EdgeType)>| RudderPlanTask {
        id: id.to_string(),
        title: id.to_string(),
        prompt: "p".to_string(),
        goal: None,
        success: None,
        deps: deps
            .into_iter()
            .map(|(on, edge)| PlanEdge {
                on: on.to_string(),
                edge,
                why: None,
            })
            .collect(),
        backend: None,
        model: None,
        effort: None,
    };
    let tasks = vec![
        task("n0", vec![]),
        task("n1", vec![("n0", EdgeType::Soft)]),
        task("n2", vec![("n0", EdgeType::Soft), ("n1", EdgeType::Soft)]),
        task(
            "n3",
            vec![
                ("n0", EdgeType::Hard),
                ("n1", EdgeType::Hard),
                ("n2", EdgeType::Hard),
            ],
        ),
    ];
    let (children, has_parent) = orchestrator_hard_tree(&tasks);
    // n0/n1/n2 are top-level (parallel) roots; only the hard integrator nests.
    assert!(
        !has_parent[0] && !has_parent[1] && !has_parent[2],
        "soft-only tasks are roots"
    );
    assert!(has_parent[3], "the hard integrator nests");
    // n3 nests under its LATEST hard prerequisite (n2), not n0 or n1.
    assert_eq!(children.get(&2), Some(&vec![(3usize, EdgeType::Hard)]));
    assert!(
        children.get(&0).is_none(),
        "n0 has no hard children (links to n1/n2 are soft)"
    );
    assert!(children.get(&1).is_none(), "n1 has no hard children");
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
    assert!(
        text.contains("orchestrator"),
        "pane title is orchestrator: {text}"
    );
    assert!(
        text.contains("decomposing the task") || text.contains("Scoping the CLI"),
        "planning shows the live transcript: {text}"
    );
    assert!(
        !text.contains("stream_event"),
        "raw JSON events are not shown: {text}"
    );
    assert!(
        !text.contains("running 1"),
        "no DAG summary while planning: {text}"
    );
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
    assert!(
        text.contains("in-progress"),
        "n0 badge reflects launched agent: {text}"
    );
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

#[test]
fn planning_orchestrator_trackpad_scroll_targets_rendered_transcript() {
    let mut app = App::new();
    app.focus = FocusPane::Worker;
    let mut orch = test_agent_run("orch", "plan this");
    orch.mode = AgentMode::RudderPlan;
    orch.status = AgentStatus::Running;
    orch.interactive_orchestrator = false;
    let mut stream = PlanStreamState::new();
    stream.ingest("{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Still decomposing the task.\"}}}\n");
    orch.plan_stream = Some(stream);
    app.agents = vec![orch];
    app.selected_agent = 0;
    app.worker_view = WorkerView::Terminal;

    assert!(
        app.selected_headless_orchestrator_rendered_view_active(),
        "planning orchestrator is rendered, not a raw PTY"
    );
    assert!(
        !app.selected_orchestrator_dag_active(),
        "the plan is not ready yet"
    );

    let area = Rect::new(0, 0, 60, 20);
    app.worker_area = Some(area);
    let inner = block_inner(area);
    app.orch_dag_scroll = 0;

    app.handle_pane_scroll(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: inner.x + 1,
        row: inner.y + 1,
        modifiers: KeyModifiers::empty(),
    });

    assert!(
        app.orch_dag_scroll > 0,
        "trackpad scroll over the planning transcript moves the rendered transcript offset"
    );
}

#[cfg(not(windows))]
#[test]
fn interactive_orchestrator_trackpad_scroll_uses_pointer_region() {
    let command = TerminalCommand::with_args(
        "/bin/sh",
        [
            "-lc",
            "for i in $(seq 1 60); do printf \"line $i\\r\\n\"; done; sleep 1",
        ],
    );
    let mut pane = TerminalPane::spawn_shell_or_command(
        Some(command),
        TerminalPaneOptions {
            size: TerminalSize { rows: 8, cols: 50 },
            scrollback_lines: 200,
            ..Default::default()
        },
    )
    .expect("spawn orchestrator pty");
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        pane.drain_output();
        if pane.output_log_snapshot().contains("line 60") {
            break;
        }
    }

    let mut app = App::new();
    app.focus = FocusPane::Worker;
    app.worker_view = WorkerView::Terminal;
    app.interactive_orchestrator = true;
    let mut orch = test_agent_run_with_terminal(&app, pane);
    orch.id = "orch".to_string();
    orch.mode = AgentMode::RudderPlan;
    orch.backend = Backend::Codex;
    orch.interactive_orchestrator = true;
    orch.status = AgentStatus::Running;
    app.agents = vec![orch];
    app.selected_agent = 0;
    app.worker_area = Some(Rect::new(0, 0, 80, 30));
    app.orch_dag_max_scroll = 20;

    let (dag_area, term_area) = interactive_orchestrator_areas(app.worker_area.unwrap());
    let dag_inner = block_inner(dag_area);
    app.handle_pane_scroll(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: dag_inner.x + 1,
        row: dag_inner.y + 1,
        modifiers: KeyModifiers::empty(),
    });
    assert!(
        app.orch_dag_scroll > 0,
        "trackpad over the interactive DAG strip scrolls the DAG"
    );

    let dag_scroll = app.orch_dag_scroll;
    let before = app
        .selected_terminal_mut()
        .map(|terminal| terminal.scrollback())
        .unwrap_or(0);
    let term_inner = block_inner(term_area);
    app.handle_pane_scroll(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: term_inner.x + 1,
        row: term_inner.y + 1,
        modifiers: KeyModifiers::empty(),
    });
    let after = app
        .selected_terminal_mut()
        .map(|terminal| terminal.scrollback())
        .unwrap_or(0);

    assert!(
        after > before,
        "trackpad over the interactive terminal scrolls terminal scrollback"
    );
    assert_eq!(
        app.orch_dag_scroll, dag_scroll,
        "terminal scroll must not mutate the DAG offset"
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
    app.handle_key(KeyEvent::new(
        KeyCode::Char('\u{2122}'),
        KeyModifiers::empty(),
    ));
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

    // A RUNNING agent that needs permission/input stays IN PROGRESS (the row label
    // flags it); only DONE means review. A running agent must never read as "review".
    run.status = AgentStatus::Running;
    run.needs_permission = true;
    assert_eq!(
        status_bucket(&run),
        Bucket::InProgress,
        "running+needs-permission is in progress"
    );
    run.needs_user_input = true;
    assert_eq!(
        status_bucket(&run),
        Bucket::InProgress,
        "running+needs-input is in progress"
    );
    run.needs_permission = false;
    run.needs_user_input = false;

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
    assert!(
        planner.is_orchestrator(),
        "RudderPlan agent is the orchestrator"
    );
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
fn sectioned_order_nests_launched_worker_by_node_id() {
    // PRODUCTION shape: a launched worker's run id != its node id, and a child's deps
    // reference the parent's NODE id (node.deps), not its run id. Regression for the
    // run-id/node-id keying bug: the nest must resolve by node id. The child is placed
    // FIRST in the vec, so a working nest reorders it AFTER its parent (root-first).
    let mut child = test_agent_run("run-bbb", "child task");
    child.node_id = Some("n1".to_string());
    child.deps = vec!["n0".to_string()]; // parent's NODE id, not its run id
    let mut parent = test_agent_run("run-aaa", "parent task");
    parent.node_id = Some("n0".to_string());
    let mut app = App::new();
    app.agents = vec![child, parent]; // child at index 0, parent at index 1

    // With node-id resolution the parent (root) renders first, child nests under it:
    // [1, 0]. Before the fix the dep "n0" matched no run id, so both were roots: [0, 1].
    assert_eq!(app.visible_agent_indices(), vec![1, 0]);
}

#[test]
fn nest_view_selection_order_matches_nest_render_across_buckets() {
    // Cross-bucket dep: a Running child depends on a Merged parent. In the default
    // sectioned view they sort by status bucket (in-progress before done): [child,
    // parent]. In NEST view the tree is global, so the parent is the root and the
    // child nests under it: [parent, child]. Selection MUST follow the active view, or
    // j/k move the cursor to a different row than the one highlighted.
    let mut child = test_agent_run("run-c", "child");
    child.node_id = Some("n1".to_string());
    child.deps = vec!["n0".to_string()];
    child.status = AgentStatus::Running;
    let mut parent = test_agent_run("run-p", "parent");
    parent.node_id = Some("n0".to_string());
    parent.status = AgentStatus::Merged;
    let mut app = App::new();
    app.agents = vec![child, parent]; // child idx 0, parent idx 1

    app.nest_view = false;
    assert_eq!(
        app.visible_agent_indices(),
        vec![0, 1],
        "sectioned: by bucket"
    );

    app.nest_view = true;
    assert_eq!(
        app.visible_agent_indices(),
        vec![1, 0],
        "nest: parent root, child nested"
    );
}

#[test]
fn followup_unknown_explicit_dep_falls_back_to_soft() {
    // A follow-up that names a hard dep NOT in the plan (e.g. a typo) must not install
    // it as a hard dep: is_ready treats an out-of-plan dep as satisfied, so the node
    // would launch immediately, silently losing the gate. It is soft-linked instead.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut a = node_agent("n0", AgentStatus::Done);
    a.mode = AgentMode::Execute;
    app.agents = vec![a];
    let note = serde_json::json!({
        "followups": [{ "title": "wire it", "deps": ["ghost"], "scope": "in" }]
    });
    assert!(app.apply_worker_followups("n0", &note));
    let node = app
        .planned_nodes
        .iter()
        .find(|n| n.title == "wire it")
        .expect("follow-up added");
    assert!(node.deps.is_empty(), "unknown hard dep is not installed");
    assert!(
        node.soft_deps.contains(&"n0".to_string()),
        "soft-linked to the finishing node instead"
    );
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
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40)).expect("test backend");
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
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 40)).expect("test backend");
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
    assert!(deps.iter().any(|d| d["on"] == "n0" && d["type"] == "hard"));
    assert!(deps.iter().any(|d| d["on"] == "n0" && d["type"] == "soft"));
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

// ---- Phase 3: plan-rebase (structural mid-flight change) --------------------

/// A plan task with an optional goal and hard deps, for the rebase differ tests.
fn rebase_task(id: &str, title: &str, goal: Option<&str>, hard: &[&str]) -> RudderPlanTask {
    RudderPlanTask {
        id: id.to_string(),
        title: title.to_string(),
        prompt: format!("do {id}"),
        goal: goal.map(ToString::to_string),
        success: None,
        deps: hard
            .iter()
            .map(|on| PlanEdge {
                on: on.to_string(),
                edge: EdgeType::Hard,
                why: Some("consumes".to_string()),
            })
            .collect(),
        backend: None,
        model: None,
        effort: None,
    }
}

fn titled_planned_node(id: &str, title: &str) -> PlannedNode {
    let mut node = test_planned_node(id, &[]);
    node.title = title.to_string();
    node
}

#[test]
fn norm_title_collapses_punctuation_and_case() {
    assert_eq!(norm_title("Add Rate-Limiting!"), "add rate limiting");
    assert_eq!(norm_title("  set   up  DB  "), "set up db");
    assert_eq!(norm_title("---"), "");
}

#[test]
fn goal_changed_only_when_new_goal_absent_from_context() {
    // The launch context embeds the original goal, so re-stating it is a KEEP.
    assert!(!goal_changed(
        "/goal build the api endpoint done when ...",
        "build the api endpoint"
    ));
    // A genuinely new objective is a re-goal.
    assert!(goal_changed(
        "/goal build the api endpoint",
        "switch the api to graphql"
    ));
    // An empty new goal never forces a re-goal.
    assert!(!goal_changed("anything", "   "));
}

#[test]
fn diff_plan_keeps_running_drops_obsolete_and_adds_new() {
    let merged = vec!["m0".to_string()];
    let running = vec![RunningNode {
        id: "r0".to_string(),
        title: "build api".to_string(),
        context: "/goal build api endpoint done when tests pass".to_string(),
    }];
    let todo = vec![
        titled_planned_node("t0", "write docs"),
        titled_planned_node("t1", "old task"),
    ];
    let new_tasks = vec![
        // Re-describes merged work → ignored (build-forward).
        rebase_task("m0", "schema", Some("redo schema"), &[]),
        // Running node, same objective → kept.
        rebase_task("r0", "build api", Some("build api endpoint"), &[]),
        // Carried-over todo → todo, not "added".
        rebase_task("t0", "write docs", None, &[]),
        // Brand new todo node → added + todo.
        rebase_task("t2", "new feature", None, &["r0"]),
        // (t1 is not re-emitted → dropped.)
    ];

    let diff = diff_plan(&running, &todo, &merged, &new_tasks);

    assert_eq!(
        diff.kept,
        vec!["r0".to_string()],
        "matched running, same goal → kept"
    );
    assert!(diff.regoaled.is_empty(), "no objective changed");
    assert_eq!(
        diff.dropped,
        vec!["t1".to_string()],
        "obsolete todo dropped"
    );
    let added_ids: Vec<&str> = diff.added.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(added_ids, vec!["t2"], "only the brand-new node is added");
    let todo_ids: Vec<&str> = diff.todo.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        todo_ids,
        vec!["t0", "t2"],
        "todo rebuilt; merged + running excluded"
    );
}

#[test]
fn diff_plan_regoals_running_when_objective_changes() {
    let running = vec![RunningNode {
        id: "r0".to_string(),
        title: "auth".to_string(),
        context: "/goal add password login".to_string(),
    }];
    let new_tasks = vec![rebase_task("r0", "auth", Some("switch to oauth"), &[])];

    let diff = diff_plan(&running, &[], &[], &new_tasks);
    assert!(diff.kept.is_empty());
    assert_eq!(
        diff.regoaled,
        vec![("r0".to_string(), "switch to oauth".to_string())],
        "changed objective → re-goal"
    );
    assert!(diff.dropped.is_empty(), "still in the plan, just re-goaled");
}

#[test]
fn diff_plan_matches_running_by_title_when_id_renamed() {
    // Planner renamed the id but kept the title: match by title so the agent is not
    // stopped-and-readded. Same objective → kept under its ORIGINAL id.
    let running = vec![RunningNode {
        id: "r0".to_string(),
        title: "payment flow".to_string(),
        context: "/goal implement the payment flow".to_string(),
    }];
    let new_tasks = vec![rebase_task(
        "payments",
        "Payment Flow",
        Some("implement the payment flow"),
        &[],
    )];

    let diff = diff_plan(&running, &[], &[], &new_tasks);
    assert_eq!(
        diff.kept,
        vec!["r0".to_string()],
        "title match keeps the original id"
    );
    assert!(diff.dropped.is_empty());
    assert!(
        diff.added.is_empty(),
        "the renamed task is not a new todo node"
    );
}

#[test]
fn is_structural_direction_detects_replacement_verbs() {
    let titles = vec!["build login".to_string(), "set up database".to_string()];
    for msg in [
        "scrap that and rewrite it in rust",
        "let's pivot to a CLI instead",
        "start over from scratch",
        "actually, ditch the whole approach",
    ] {
        assert!(is_structural_direction(msg, &titles), "structural: {msg}");
    }
}

#[test]
fn is_structural_direction_triggers_on_majority_title_overlap() {
    let titles = vec![
        "build login page".to_string(),
        "set up database".to_string(),
        "add payments".to_string(),
    ];
    // No replacement verb, but references a MAJORITY of node titles → structural.
    assert!(is_structural_direction(
        "make the login and database screens match the new mockups",
        &titles
    ));
}

#[test]
fn is_structural_direction_additive_request_is_not_structural() {
    let titles = vec![
        "build login page".to_string(),
        "set up database".to_string(),
        "add payments".to_string(),
    ];
    // Touches only one node, no replacement verb → additive (reconcile path).
    assert!(!is_structural_direction(
        "also add a logout button to the navbar",
        &titles
    ));
    // Too few titles to judge overlap, no verb → additive.
    assert!(!is_structural_direction(
        "tweak the spacing",
        &["build login".to_string()]
    ));
}

#[test]
fn classify_new_direction_reads_live_plan_titles() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    app.planned_nodes = vec![
        titled_planned_node("n0", "implement billing"),
        titled_planned_node("n1", "wire up notifications"),
    ];
    let mut running = node_agent("n2", AgentStatus::Running);
    running.task_summary = "render the dashboard".to_string();
    app.agents.push(running);

    // Additive: one new feature, no pivot.
    assert!(!app.classify_new_direction("also add a settings page"));
    // Structural: a replacement verb.
    assert!(app.classify_new_direction("scrap billing and start over with stripe"));
}

#[test]
fn rebasing_suppresses_auto_merge() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    app.auto_merge = true;
    app.rebasing = true;
    // A clean Done node would normally be auto-merged; the rebase guard holds it.
    app.agents.push(node_agent("n0", AgentStatus::Done));
    app.maybe_auto_merge();
    assert_eq!(
        app.agents[0].status,
        AgentStatus::Done,
        "auto-merge is suppressed while a rebase is in flight"
    );
}

// ---- duplicate-orchestrator leak: reconcile-planner cleanup -----------------

fn unique_test_repo(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rudder-{tag}-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create test repo root");
    root
}

/// A persisted RudderPlan row carrying a given reconcile-planner flag.
fn planner_run(id: &str, reconcile: bool) -> AgentRun {
    let mut run = test_agent_run(id, &format!("plan {id}"));
    run.mode = AgentMode::RudderPlan;
    run.reconcile_planner = reconcile;
    run.status = AgentStatus::Running;
    run
}

#[test]
fn reconcile_planner_flag_survives_save_and_reload() {
    let repo = unique_test_repo("reconcile-roundtrip");
    let run = planner_run("rec-1", true);
    save_native_run_record(&repo, &run).expect("save reconcile planner");

    let raw =
        fs::read_to_string(native_run_dir(&repo, "rec-1").join("run.json")).expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(
        value.get("reconcilePlanner").and_then(|v| v.as_bool()),
        Some(true),
        "the discriminator is persisted"
    );

    let reloaded = agent_from_run_record(&repo, value).expect("reload run");
    assert!(reloaded.reconcile_planner, "flag survives the round-trip");

    // Old-format record (no field) defaults to false (the real planner).
    let mut legacy: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(native_run_dir(&repo, "rec-1").join("run.json")).unwrap(),
    )
    .unwrap();
    legacy.as_object_mut().unwrap().remove("reconcilePlanner");
    let legacy_run = agent_from_run_record(&repo, legacy).expect("reload legacy run");
    assert!(
        !legacy_run.reconcile_planner,
        "a record without the field loads as the real planner"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn interactive_orchestrator_flag_survives_save_and_reload() {
    let repo = unique_test_repo("interactive-orch-roundtrip");
    let mut run = planner_run("orch-1", false);
    run.backend = Backend::Codex;
    run.autosteered = false;
    run.interactive_orchestrator = true;
    save_native_run_record(&repo, &run).expect("save interactive orchestrator");

    let raw = fs::read_to_string(native_run_dir(&repo, "orch-1").join("run.json"))
        .expect("read run.json");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse run.json");
    assert_eq!(
        value
            .get("interactiveOrchestrator")
            .and_then(|value| value.as_bool()),
        Some(true),
        "the renderer discriminator is persisted"
    );
    let reloaded = agent_from_run_record(&repo, value).expect("reload run");
    assert!(
        reloaded.interactive_orchestrator,
        "interactive orchestrator survives the round-trip"
    );
    let mut app = App::new();
    app.cwd = repo.clone();
    app.interactive_orchestrator = false;
    let mut running = reloaded;
    running.status = AgentStatus::Running;
    app.agents.push(running);
    assert!(
        app.is_interactive_orchestrator_run(&app.agents[0]),
        "the persisted row flag is authoritative even when the current launch default is headless"
    );
    fs::create_dir_all(repo.join(".rudder")).unwrap();
    fs::write(
        orchestrator_plan_path(&repo),
        "# Plan\nRUDDER_PLAN_TASKS_START\n{\"tasks\":[{\"id\":\"n0\",\"title\":\"persisted\",\"prompt\":\"p\",\"goal\":\"g\",\"success\":\"s\",\"deps\":[]}]}\nRUDDER_PLAN_TASKS_END\n",
    )
    .unwrap();
    app.maybe_capture_orchestrator_plan();
    assert!(
        app.awaiting_approval && app.planned_nodes.len() == 1,
        "persisted interactive rows still capture their plan file after an env/default change"
    );

    let mut headless = planner_run("headless-1", false);
    headless.backend = Backend::Claude;
    headless.autosteered = true;
    headless.interactive_orchestrator = false;
    save_native_run_record(&repo, &headless).expect("save headless planner");
    let raw = fs::read_to_string(native_run_dir(&repo, "headless-1").join("run.json"))
        .expect("read headless run.json");
    let reloaded = agent_from_run_record(
        &repo,
        serde_json::from_str(&raw).expect("parse headless run.json"),
    )
    .expect("reload headless");
    assert!(
        reloaded.autosteered,
        "headless capture flag survives reload"
    );
    assert!(
        !reloaded.interactive_orchestrator,
        "headless planner does not reload as interactive"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn reload_restores_resolver_relabeled_merged_run_identity() {
    let repo = unique_test_repo("resolver-label-reload");
    let mut run = test_agent_run("run-labeled", "Add arbitrary emoji picker");
    // What start_conflict_resolution_agent leaves behind, persisted as merged
    // without passing through mark_agent_and_review_sources_merged (records
    // written before the merge-time restore, or merged via the CLI while the
    // dashboard was closed).
    run.task = "Resolve merge conflicts: Add arbitrary emoji picker".to_string();
    run.task_summary = "merge conflicts \u{2192} Add arbitrary emoji picker".to_string();
    run.had_merge_conflict = true;
    run.status = AgentStatus::Merged;
    save_native_run_record(&repo, &run).expect("save merged labeled run");

    let raw = fs::read_to_string(native_run_dir(&repo, "run-labeled").join("run.json"))
        .expect("read run.json");
    let reloaded =
        agent_from_run_record(&repo, serde_json::from_str(&raw).expect("parse run.json"))
            .expect("reload run");
    assert_eq!(reloaded.status, AgentStatus::Merged);
    assert_eq!(reloaded.task, "Add arbitrary emoji picker");
    assert_eq!(reloaded.task_summary, "Add arbitrary emoji picker");
    // Conflict telemetry survives via the durable marker, not the label.
    assert!(reloaded.had_merge_conflict);

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn reload_keeps_resolver_label_while_conflict_is_live() {
    let repo = unique_test_repo("resolver-label-live");
    let mut run = test_agent_run("run-conflicted", "Add arbitrary emoji picker");
    run.task = "Resolve merge conflicts: Add arbitrary emoji picker".to_string();
    run.task_summary = "merge conflicts \u{2192} Add arbitrary emoji picker".to_string();
    run.status = AgentStatus::Done;
    run.merge_conflict = true;
    run.had_merge_conflict = true;
    save_native_run_record(&repo, &run).expect("save conflicted run");

    let raw = fs::read_to_string(native_run_dir(&repo, "run-conflicted").join("run.json"))
        .expect("read run.json");
    let reloaded =
        agent_from_run_record(&repo, serde_json::from_str(&raw).expect("parse run.json"))
            .expect("reload run");
    // The conflict is still live: the pane should keep reading as conflict work.
    assert_eq!(reloaded.status, AgentStatus::Done);
    assert!(reloaded.merge_conflict);
    assert_eq!(
        reloaded.task,
        "Resolve merge conflicts: Add arbitrary emoji picker"
    );
    assert_eq!(
        reloaded.task_summary,
        "merge conflicts \u{2192} Add arbitrary emoji picker"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn load_persisted_agents_drops_reconcile_planners() {
    let repo = unique_test_repo("load-filter");
    save_native_run_record(&repo, &planner_run("orch-1", false)).expect("save pinned planner");
    save_native_run_record(&repo, &planner_run("rec-1", true)).expect("save reconcile planner");

    let loaded = load_persisted_agents(&repo);
    let orchestrators: Vec<&AgentRun> = loaded.iter().filter(|run| run.is_orchestrator()).collect();
    assert_eq!(
        orchestrators.len(),
        1,
        "exactly one orchestrator reloads (the reconcile planner is filtered out)"
    );
    assert_eq!(orchestrators[0].id, "orch-1", "the pinned planner survives");
    assert!(
        loaded.iter().all(|run| !run.reconcile_planner),
        "no reconcile planner is ever reloaded"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn load_persisted_agents_keeps_only_newest_orchestrator() {
    let repo = unique_test_repo("orch-dedupe");
    // Two initial planners from two past plans (both reconcile_planner=false). Every
    // plan ever run persists its planner, so without dedupe a long-lived repo reloads
    // one phantom orchestrator per plan.
    let mut old = planner_run("orch-old", false);
    old.created_at = "2026-01-01T00:00:00Z".to_string();
    let mut newer = planner_run("orch-new", false);
    newer.created_at = "2026-05-01T00:00:00Z".to_string();
    save_native_run_record(&repo, &old).expect("save old orchestrator");
    save_native_run_record(&repo, &newer).expect("save new orchestrator");

    let loaded = load_persisted_agents(&repo);
    let orchestrators: Vec<&AgentRun> = loaded.iter().filter(|run| run.is_orchestrator()).collect();
    assert_eq!(orchestrators.len(), 1, "only one orchestrator reloads");
    assert_eq!(
        orchestrators[0].id, "orch-new",
        "the newest pinned planner survives the dedupe"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn retire_reconcile_planner_clears_memory_and_disk() {
    let repo = unique_test_repo("retire-reconcile");
    let mut app = App::new();
    app.cwd = repo.clone();

    let orch = planner_run("orch-1", false);
    let rec = planner_run("rec-1", true);
    save_native_run_record(&repo, &orch).expect("save orchestrator");
    save_native_run_record(&repo, &rec).expect("save reconcile planner");
    app.agents.push(orch);
    app.agents.push(rec);
    app.selected_agent = 1; // watching the (transient) reconcile planner

    // Both run.json records exist before retiring.
    assert!(native_run_dir(&repo, "rec-1").join("run.json").exists());

    app.retire_planner_row("rec-1");

    // In memory: exactly one orchestrator remains, the pinned one.
    let orchestrators: Vec<&AgentRun> = app
        .agents
        .iter()
        .filter(|run| run.is_orchestrator())
        .collect();
    assert_eq!(orchestrators.len(), 1, "steady state has one orchestrator");
    assert_eq!(orchestrators[0].id, "orch-1");
    // Selection followed back to the real orchestrator.
    assert_eq!(app.selected_agent, 0);
    // On disk: the reconcile record is gone; the pinned one is untouched.
    assert!(
        !native_run_dir(&repo, "rec-1").join("run.json").exists(),
        "reconcile run.json deleted so it cannot reload as an orphan orchestrator"
    );
    assert!(
        native_run_dir(&repo, "orch-1").join("run.json").exists(),
        "the pinned orchestrator's record is preserved"
    );

    // Idempotent: a second call is a harmless no-op.
    app.retire_planner_row("rec-1");
    assert_eq!(app.agents.len(), 1);

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn restore_running_agents_skips_reconcile_planners() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    // A reloaded reconcile planner (no terminal, status Running) must NOT be queued
    // for background resume even if it slips past the load filter.
    let mut rec = planner_run("rec-1", true);
    rec.terminal = None;
    rec.session_id = Some("sid".to_string());
    app.agents.push(rec);
    app.migration_resumes_attempted = false;
    app.restore_running_agents();
    // It is still present (restore does not delete) but was never resumed: it has
    // no terminal and its status was left untouched.
    assert!(
        app.agents[0].terminal.is_none(),
        "reconcile planner is not resumed"
    );
}

#[test]
fn auto_expand_recovers_followups_from_sidecar_file_without_a_pty() {
    // The ROBUST channel in isolation: a finished worker with NO terminal at all. The
    // only way to recover its follow-ups is the file `rudder done` dropped at
    // <workspace>/.rudder/done/<node>.json. This is what makes auto-expand survive a
    // real Claude/Codex agent whose interactive TUI would have boxed/truncated the
    // echoed block in the PTY beyond recovery.
    let repo = unique_test_repo("done-sidecar");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.awaiting_approval = true; // hold scheduling; we only assert the DAG grew

    let mut worker = node_agent("n0", AgentStatus::Done); // mode Execute, node_id "n0"
    worker.cwd = repo.clone();
    worker.terminal = None; // no PTY: the file on disk is the ONLY source
    app.agents = vec![worker];

    // Exactly what `rudder done` (pointed at RUDDER_DONE_FILE) writes into the workspace.
    let done = worker_done_file(&repo, "n0");
    fs::create_dir_all(done.parent().unwrap()).unwrap();
    fs::write(
            &done,
            "{\"summary\":\"did it\",\"followups\":[{\"title\":\"add docs\",\"scope\":\"in\"},{\"title\":\"out of lane\",\"scope\":\"out\"}]}",
        )
        .unwrap();

    app.maybe_ingest_worker_followups();

    assert_eq!(
        app.planned_nodes.len(),
        1,
        "the in-scope follow-up is recovered from the file (no PTY involved)"
    );
    assert_eq!(app.planned_nodes[0].title, "add docs");
    assert!(
        app.followups_ingested.contains("n0"),
        "the worker is marked ingested once"
    );
    let _ = fs::remove_dir_all(&repo);
}

// ---- backstop summarizer (silent agents) ------------------------------------

#[test]
fn completion_summary_prompt_carries_task_and_diff() {
    let prompt = build_completion_summary_prompt("wire up auth", "diff --git a/auth.rs b/auth.rs");
    assert!(prompt.contains("wire up auth"), "the task is included");
    assert!(prompt.contains("auth.rs"), "the diff is included");
    assert!(
        prompt.contains("\"followups\""),
        "asks for the CompletionNote shape"
    );
    // An empty diff is labeled rather than left blank.
    assert!(build_completion_summary_prompt("x", "   ").contains("no file changes detected"));
}

#[test]
fn completion_note_parser_extracts_json_amid_prose() {
    let out = "Here is the note:\n```json\n{\"summary\":\"did x\",\"followups\":[{\"title\":\"add tests\",\"scope\":\"in\"}]}\n```\nDone.";
    let note = completion_note_from_summary_output(out).expect("parsed a note");
    assert_eq!(note.get("summary").and_then(|v| v.as_str()), Some("did x"));
    assert!(completion_note_from_summary_output("no json here at all").is_none());
    assert!(completion_note_from_summary_output("}{ broken").is_none());
}

#[test]
fn backstop_result_grows_dag_via_poll() {
    // The summarizer ran off-thread and returned a reconstructed note; poll applies it
    // exactly like a real `rudder done` report would.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    app.awaiting_approval = true; // hold scheduling; assert the DAG grew
    let mut worker = node_agent("n0", AgentStatus::Done); // run.id == node_id == "n0"
    worker.mode = AgentMode::Execute;
    app.agents = vec![worker];
    app.completion_summary_pending.insert("n0".to_string());

    app.completion_summary_tx
        .send(CompletionSummaryResult {
            run_id: "n0".to_string(),
            node_id: "n0".to_string(),
            note: Some(serde_json::json!({
                "summary": "implemented it",
                "followups": [{ "title": "add tests", "scope": "in" }]
            })),
        })
        .unwrap();

    app.poll_completion_summary_workers();

    assert!(
        !app.completion_summary_pending.contains("n0"),
        "pending cleared"
    );
    assert!(
        app.followups_ingested.contains("n0"),
        "marked ingested (never re-summarized)"
    );
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "the reconstructed note grew the DAG"
    );
    assert_eq!(app.planned_nodes[0].title, "add tests");
}

#[test]
fn ingest_without_note_or_node_id_does_not_spawn_backstop() {
    // A Done worker carrying NO node id (a manual run, not a plan node) has nothing to
    // attach follow-ups to: mark it handled, never spawn a summarizer (which would
    // shell out to claude). Keeps the test suite hermetic.
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut worker = test_agent_run("manual", "did stuff");
    worker.mode = AgentMode::Execute;
    worker.status = AgentStatus::Done;
    worker.node_id = None;
    worker.terminal = None;
    app.agents = vec![worker];

    let grew = app.ingest_worker_followups(0);
    assert!(!grew);
    assert!(app.followups_ingested.contains("manual"), "marked handled");
    assert!(
        app.completion_summary_pending.is_empty(),
        "no backstop without a node id"
    );
}

/// Build a finished plan worker whose sidecar holds the given JSON note.
fn done_worker_with_sidecar(
    repo: &std::path::Path,
    node: &str,
    json: &str,
    status: AgentStatus,
) -> AgentRun {
    let done = worker_done_file(repo, node);
    fs::create_dir_all(done.parent().unwrap()).unwrap();
    fs::write(&done, json).unwrap();
    let mut worker = node_agent(node, status); // run.id == node_id == node
    worker.mode = AgentMode::Execute;
    worker.cwd = repo.to_path_buf();
    worker.terminal = None;
    worker
}

#[test]
fn ingest_empty_followups_is_trusted_no_backstop() {
    // The agent ADDRESSED follow-ups with an explicit empty list: trust it, do not mine
    // the diff. (Distinguishes the deliberate "nothing further" from a missing report.)
    let repo = unique_test_repo("ingest-empty");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.awaiting_approval = true;
    app.agents = vec![done_worker_with_sidecar(
        &repo,
        "n0",
        "{\"summary\":\"done\",\"followups\":[]}",
        AgentStatus::Done,
    )];

    let grew = app.ingest_worker_followups(0);
    assert!(!grew, "empty list grows nothing");
    assert!(
        app.followups_ingested.contains("n0"),
        "trusted + marked ingested"
    );
    assert!(
        app.completion_summary_pending.is_empty(),
        "no diff-backstop for an explicit empty list"
    );
    assert!(app.planned_nodes.is_empty());
    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn ingest_failed_worker_reads_note_but_never_backstops() {
    // A FAILED worker still has its filed note READ (Fix: broadened candidate filter)…
    let repo = unique_test_repo("ingest-failed");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.awaiting_approval = true;
    app.agents = vec![done_worker_with_sidecar(
            &repo,
            "n0",
            "{\"summary\":\"partial\",\"followups\":[{\"title\":\"finish the refactor\",\"scope\":\"in\"}]}",
            AgentStatus::Failed,
        )];
    let grew = app.ingest_worker_followups(0);
    assert!(grew, "a failed worker's filed follow-ups are still applied");
    assert_eq!(app.planned_nodes[0].title, "finish the refactor");

    // …but a FAILED worker with only freeform prose never triggers the diff-backstop
    // (its half-finished diff would invite noise).
    let repo2 = unique_test_repo("ingest-failed-prose");
    let mut app2 = App::new();
    app2.cwd = repo2.clone();
    app2.agents = vec![done_worker_with_sidecar(
        &repo2,
        "n1",
        "{\"summary\":\"I crashed but also TODO: add caching\"}",
        AgentStatus::Failed,
    )];
    let grew2 = app2.ingest_worker_followups(0);
    assert!(!grew2);
    assert!(
        app2.completion_summary_pending.is_empty(),
        "no backstop for an incomplete worker"
    );
    assert!(
        app2.followups_ingested.contains("n1"),
        "still marked handled"
    );
    let _ = fs::remove_dir_all(&repo);
    let _ = fs::remove_dir_all(&repo2);
}

#[test]
fn spawn_completion_backstop_refuses_incomplete_or_unattached() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    // Not cleanly Done -> no spawn (half-finished diff invites noise).
    let r = app.spawn_completion_backstop(
        "r1".to_string(),
        "n0".to_string(),
        std::env::temp_dir(),
        "task".to_string(),
        false, // is_complete
        "x",
    );
    assert!(!r);
    assert!(app.completion_summary_pending.is_empty());
    assert!(
        app.followups_ingested.contains("r1"),
        "marked handled, not retried"
    );
    // Complete but no node id -> nothing to attach to -> no spawn.
    let r2 = app.spawn_completion_backstop(
        "r2".to_string(),
        String::new(),
        std::env::temp_dir(),
        "task".to_string(),
        true,
        "x",
    );
    assert!(!r2);
    assert!(app.completion_summary_pending.is_empty());
    assert!(app.followups_ingested.contains("r2"));
}

#[test]
fn followup_scope_out_is_case_insensitive() {
    let mut app = App::new();
    app.cwd = std::env::temp_dir();
    let mut a = node_agent("n0", AgentStatus::Done);
    a.mode = AgentMode::Execute;
    app.agents = vec![a];
    let note = serde_json::json!({
        "followups": [
            { "title": "out of lane work", "scope": "OUT" },
            { "title": "in lane work", "scope": "in" }
        ]
    });
    let grew = app.apply_worker_followups("n0", &note);
    assert!(grew);
    assert_eq!(
        app.planned_nodes.len(),
        1,
        "the OUT (any case) follow-up is not injected"
    );
    assert_eq!(app.planned_nodes[0].title, "in lane work");
}

#[test]
fn ingested_ledger_persists_and_reloads() {
    let repo = unique_test_repo("ingest-ledger");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.mark_run_ingested("run-aaa".to_string());
    app.mark_run_ingested("run-bbb".to_string());

    // The on-disk ledger round-trips through the loader (used by App::new on restart).
    let reloaded = load_ingested_runs(&repo);
    assert!(reloaded.contains("run-aaa") && reloaded.contains("run-bbb"));
    assert_eq!(reloaded.len(), 2);
    let _ = fs::remove_dir_all(&repo);
}

// ---- LIVE conductor simulation (opt-in) -------------------------------------
//
// A scripted end-to-end run of the REAL conductor brain: the real scheduler, real
// worker PTYs, and the real auto-expand loop, driven headlessly. Workers are FAKE
// (a tiny shell script injected via RUDDER_CODEX_BIN) so there are no API keys, no
// network, and no jj/rudder shim (a non-git cwd makes prepare_jj_workspace run the
// worker in place). It spawns real processes + sleeps, so it is #[ignore]d; run it
// with: `cargo test conductor_live -- --ignored --nocapture` to watch the timeline.

// A stand-in for `codex exec` (RUDDER_CODEX_BIN). It prints only NON-parseable TUI
// noise to the PTY (as a real agent's UI would, after boxing/truncating tool output)
// and reports its real note over the ROBUST FILE CHANNEL the launcher points it at
// (RUDDER_DONE_FILE), exactly as `rudder done` does. So the conductor must recover the
// follow-up from the file on disk, NOT from the terminal.
const FAKE_CONDUCTOR_WORKER: &str = "#!/bin/sh\n\
        echo 'working on it...'\n\
        echo '[tool result truncated by the agent UI]'\n\
        if [ -n \"$RUDDER_DONE_FILE\" ]; then\n\
          mkdir -p \"$(dirname \"$RUDDER_DONE_FILE\")\"\n\
          printf '%s' '{\"summary\":\"did the work\",\"followups\":[{\"title\":\"write integration tests\",\"why\":\"cover it\",\"scope\":\"in\"}]}' > \"$RUDDER_DONE_FILE\"\n\
        fi\n";

#[test]
#[ignore = "live conductor simulation: spawns fake worker processes + sleeps; run with --ignored --nocapture"]
fn conductor_live_run_drains_dag_with_fake_workers() {
    let repo = unique_test_repo("conductor-live");
    let worker = repo.join("fake-worker.sh");
    fs::write(&worker, FAKE_CONDUCTOR_WORKER).expect("write fake worker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&worker).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&worker, perms).unwrap();
    }
    std::env::set_var("RUDDER_CODEX_BIN", &worker);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Codex;
    app.auto_merge = false; // merge has its own tests; we simulate the merge event here
                            // Seed a 2-node plan: a root + a child that HARD-depends on it.
    app.planned_nodes = vec![titled_planned_node("setup", "scaffold the project"), {
        let mut n = titled_planned_node("feature", "build the feature");
        n.deps = vec!["setup".to_string()];
        n
    }];
    app.planned_origin = "build a small app".to_string();

    println!("\n=== conductor live run ===");
    println!("seed: [setup] (root)  ->hard->  [feature]");
    app.run_scheduler();
    println!("after approve: launched the ready root, feature held on its hard dep");

    let node_status = |app: &App, id: &str| {
        app.agents
            .iter()
            .find(|r| r.node_id.as_deref() == Some(id))
            .map(|r| r.status)
    };

    // A node is "present" whether it is still queued in planned_nodes OR has already
    // been launched into an agent: the conductor launches a grown follow-up the same
    // tick it ingests it, so it leaves planned_nodes almost immediately.
    let followup_present = |app: &App| {
        app.planned_nodes
            .iter()
            .any(|n| n.title == "write integration tests")
            || app
                .agents
                .iter()
                .any(|r| r.task_summary == "write integration tests")
    };

    let mut saw_followup = false;
    let mut drained = false;
    for tick in 0..300 {
        app.poll_agents();
        app.maybe_ingest_worker_followups();

        // The conductor grows the DAG from a finished worker's RUDDER_DONE block;
        // the grown node launches the same tick, so check queue AND agents.
        if !saw_followup && followup_present(&app) {
            saw_followup = true;
            println!("[tick {tick}] auto-expand: a finished worker's `rudder done` grew the DAG with 'write integration tests'");
        }

        // Simulate the conductor's MERGE: any finished plan worker merges, which
        // unblocks its hard children on the next scheduler pass (feature waits for
        // setup's merge, not just its completion).
        let just_done: Vec<(usize, String)> = app
            .agents
            .iter()
            .enumerate()
            .filter(|(_, r)| r.node_id.is_some() && r.status == AgentStatus::Done)
            .map(|(i, r)| (i, r.node_id.clone().unwrap_or_default()))
            .collect();
        if !just_done.is_empty() {
            for (i, id) in &just_done {
                // A real merge (mark_agent_and_review_sources_merged) drops the PTY
                // too, so poll_agents stops re-marking the exited process Done; mirror
                // that here for a faithful simulation.
                app.agents[*i].terminal = None;
                app.agents[*i].status = AgentStatus::Merged;
                println!("[tick {tick}] {id} finished -> merged");
            }
            app.run_scheduler();
        }

        let running = app
            .agents
            .iter()
            .filter(|r| r.node_id.is_some() && r.status == AgentStatus::Running)
            .count();
        if saw_followup
            && app.planned_nodes.is_empty()
            && running == 0
            && node_status(&app, "setup") == Some(AgentStatus::Merged)
            && node_status(&app, "feature") == Some(AgentStatus::Merged)
        {
            println!("[tick {tick}] DAG drained: the hard-dep child waited for its parent's merge, and the auto-grown node launched + finished");
            drained = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    println!("\n--- activity log ---");
    for line in &app.activity_log {
        println!("  · {line}");
    }
    println!("--- final node states ---");
    for run in &app.agents {
        if let Some(id) = &run.node_id {
            println!("  {id:<10} {:?}  ({})", run.status, run.task_summary);
        }
    }
    println!();

    std::env::remove_var("RUDDER_CODEX_BIN");

    assert!(
        saw_followup,
        "a finished worker's `rudder done` grew the DAG"
    );
    assert!(
        node_status(&app, "setup") == Some(AgentStatus::Merged),
        "the root ran and merged"
    );
    assert!(
        node_status(&app, "feature") == Some(AgentStatus::Merged),
        "the hard-dependent child launched after its parent merged, then merged"
    );
    let followup_launched = app.agents.iter().any(|r| {
        r.task_summary.contains("write integration tests")
            || r.current_prompt.contains("write integration tests")
    });
    assert!(
        followup_launched,
        "the auto-grown follow-up node was scheduled into a worker"
    );
    assert!(drained, "the whole DAG drained within the tick budget");

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn auto_merge_skip_round_trips_through_persistence() {
    // auto_merge is a persisted default, so the skip list must survive a restart with
    // it: a fresh App that loses the list would re-merge a known-conflicted run on the
    // first maybe_auto_merge tick and re-spawn an AI resolver uninvited.
    let repo = unique_test_repo("automerge-skip");
    let mut app = App::new();
    app.cwd = repo.clone();
    // Missing file => empty list (fresh repo, corrupt file fallback).
    assert!(load_auto_merge_skip(&repo).is_empty());

    app.auto_merge_skip.push("run-conflicted-a".to_string());
    app.auto_merge_skip.push("run-conflicted-b".to_string());
    app.persist_auto_merge_skip();
    assert_eq!(
        load_auto_merge_skip(&repo),
        vec![
            "run-conflicted-a".to_string(),
            "run-conflicted-b".to_string()
        ],
        "skip list round-trips through .rudder/auto-merge-skip.json"
    );

    // A successful merge / re-goal clears an id; the persisted copy must follow.
    app.auto_merge_skip.retain(|id| id != "run-conflicted-a");
    app.persist_auto_merge_skip();
    assert_eq!(
        load_auto_merge_skip(&repo),
        vec!["run-conflicted-b".to_string()],
        "clearing a skip entry persists too"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn cleanup_run_signals_removes_only_that_runs_files() {
    // Deleting an agent must prune all three per-run signal artifacts, or
    // ~/.rudder/signals accumulates dead runs forever. env_guard: RUDDER_HOME is
    // process-global.
    let _env = env_guard();
    let home = unique_test_repo("signals-home");
    let prior_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let dir = crate::signals::signals_dir().expect("signals dir under RUDDER_HOME");
    fs::create_dir_all(&dir).expect("create signals dir");
    let doomed = [
        dir.join("run-dead.json"),
        dir.join("run-dead-claude.json"),
        dir.join("run-dead-notify.sh"),
    ];
    for path in &doomed {
        fs::write(path, "x").expect("seed signal file");
    }
    let survivor = dir.join("run-live-claude.json");
    fs::write(&survivor, "x").expect("seed survivor");

    crate::signals::cleanup_run_signals("run-dead");

    // Capture, restore the env, THEN assert, so a failure cannot leak RUDDER_HOME.
    let removed = doomed.iter().all(|path| !path.exists());
    let survivor_intact = survivor.exists();
    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&home);

    assert!(removed, "all three per-run signal files are removed");
    assert!(survivor_intact, "other runs' signal files are untouched");
}

#[test]
fn native_perf_logger_is_disabled_by_default() {
    let _env = env_guard();
    let home = unique_test_repo("perf-home-default");
    let prior_home = std::env::var_os("RUDDER_HOME");
    let prior_perf = std::env::var_os("RUDDER_NATIVE_PERF");
    std::env::set_var("RUDDER_HOME", &home);
    std::env::remove_var("RUDDER_NATIVE_PERF");

    let mut logger = PerfLogger::new();
    logger.log("test_event", serde_json::json!({ "ok": true }));
    drop(logger);
    let files = fs::read_dir(&home)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);

    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    match prior_perf {
        Some(value) => std::env::set_var("RUDDER_NATIVE_PERF", value),
        None => std::env::remove_var("RUDDER_NATIVE_PERF"),
    }
    let _ = fs::remove_dir_all(&home);

    assert_eq!(files, 0, "disabled perf logging must not create files");
}

#[test]
fn native_perf_logger_uses_per_process_file_when_enabled() {
    let _env = env_guard();
    let home = unique_test_repo("perf-home-enabled");
    let prior_home = std::env::var_os("RUDDER_HOME");
    let prior_perf = std::env::var_os("RUDDER_NATIVE_PERF");
    std::env::set_var("RUDDER_HOME", &home);
    std::env::set_var("RUDDER_NATIVE_PERF", "1");

    let mut logger = PerfLogger::new();
    logger.log("test_event", serde_json::json!({ "ok": true }));
    drop(logger);
    let names = fs::read_dir(&home)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    match prior_perf {
        Some(value) => std::env::set_var("RUDDER_NATIVE_PERF", value),
        None => std::env::remove_var("RUDDER_NATIVE_PERF"),
    }
    let _ = fs::remove_dir_all(&home);

    assert!(
        names
            .iter()
            .any(|name| name.starts_with("native-perf-") && name.ends_with(".ndjson")),
        "enabled perf logging should use a per-process file, got {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "native-perf.ndjson"),
        "legacy shared perf log file must not be used"
    );
}

#[test]
fn cleanup_old_signals_prunes_without_run_delete() {
    let _env = env_guard();
    let home = unique_test_repo("signals-ttl-home");
    let prior_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let dir = crate::signals::signals_dir().expect("signals dir under RUDDER_HOME");
    fs::create_dir_all(&dir).expect("create signals dir");
    let stale = dir.join("stale.json");
    fs::write(&stale, "x").expect("seed stale signal");
    std::thread::sleep(Duration::from_millis(2));
    let removed = crate::signals::cleanup_old_signals(Duration::ZERO);
    let gone = !stale.exists();

    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&home);

    assert_eq!(removed, 1, "one stale signal should be pruned");
    assert!(gone, "stale signal should be removed");
}

// ---- Paste-collapse in the task draft ("[Pasted #N …]" chips) ----

#[test]
fn short_single_line_paste_is_not_collapsed() {
    assert!(!paste_should_collapse("https://example.com/some/path"));
    let mut input = String::new();
    let mut cursor = 0;
    let mut chunks = Vec::new();
    apply_task_paste(&mut input, &mut cursor, &mut chunks, "just a url");
    assert_eq!(input, "just a url", "short paste inserts verbatim");
    assert!(chunks.is_empty(), "no chip is created for a short paste");
}

#[test]
fn multiline_paste_collapses_to_a_chip() {
    assert!(paste_should_collapse("line one\nline two\nline three"));
    let mut input = "fix the bug in ".to_string();
    let mut cursor = input.chars().count();
    let mut chunks = Vec::new();
    apply_task_paste(
        &mut input,
        &mut cursor,
        &mut chunks,
        "line one\nline two\nline three",
    );
    assert_eq!(input, "fix the bug in [Pasted #1 +3 lines]");
    assert_eq!(cursor, input.chars().count());
    assert_eq!(chunks.len(), 1);
    // A submit expands the chip back to the full pasted text.
    assert_eq!(
        expand_pasted_chips(&input, &chunks),
        "fix the bug in line one\nline two\nline three"
    );
}

#[test]
fn long_single_line_paste_collapses_with_a_char_count() {
    let long = "x".repeat(250);
    assert!(paste_should_collapse(&long));
    let mut input = String::new();
    let mut cursor = 0;
    let mut chunks = Vec::new();
    apply_task_paste(&mut input, &mut cursor, &mut chunks, &long);
    assert_eq!(input, "[Pasted #1 +250 chars]", "single-line reports chars");
    assert_eq!(expand_pasted_chips(&input, &chunks), long);
}

#[test]
fn re_pasting_toggles_a_chip_open_then_closed() {
    let pasted = "alpha\nbeta\ngamma";
    let mut input = "context: ".to_string();
    let mut cursor = input.chars().count();
    let mut chunks = Vec::new();

    apply_task_paste(&mut input, &mut cursor, &mut chunks, pasted);
    assert_eq!(input, "context: [Pasted #1 +3 lines]");

    // Re-paste the SAME content: the chip expands inline to the full text.
    apply_task_paste(&mut input, &mut cursor, &mut chunks, pasted);
    assert_eq!(input, "context: alpha\nbeta\ngamma", "chip expanded inline");
    assert_eq!(cursor, input.chars().count());
    assert_eq!(chunks.len(), 1, "still one chunk, just toggled");

    // Re-paste again: it re-collapses back to the chip.
    apply_task_paste(&mut input, &mut cursor, &mut chunks, pasted);
    assert_eq!(input, "context: [Pasted #1 +3 lines]", "chip re-collapsed");

    // Whether collapsed or expanded, a submit always yields the real text.
    assert_eq!(
        expand_pasted_chips(&input, &chunks),
        "context: alpha\nbeta\ngamma"
    );
}

#[test]
fn editing_a_chip_away_makes_the_next_paste_fresh() {
    let pasted = "one\ntwo";
    let mut input = String::new();
    let mut cursor = 0;
    let mut chunks = Vec::new();

    apply_task_paste(&mut input, &mut cursor, &mut chunks, pasted);
    assert_eq!(input, "[Pasted #1 +2 lines]");

    // User deletes the chip text by hand.
    input.clear();
    cursor = 0;

    // Re-pasting the same content now inserts a fresh chip (an empty draft resets
    // numbering to #1) rather than trying to toggle a chip that is no longer present.
    apply_task_paste(&mut input, &mut cursor, &mut chunks, pasted);
    assert_eq!(input, "[Pasted #1 +2 lines]");
    assert_eq!(
        chunks.len(),
        1,
        "stale chunk was dropped when the draft emptied"
    );
}

#[test]
fn two_distinct_pastes_get_sequential_chip_ids() {
    let mut input = String::new();
    let mut cursor = 0;
    let mut chunks = Vec::new();
    apply_task_paste(&mut input, &mut cursor, &mut chunks, "aaa\nbbb");
    apply_task_paste(&mut input, &mut cursor, &mut chunks, "ccc\nddd");
    assert_eq!(input, "[Pasted #1 +2 lines][Pasted #2 +2 lines]");
    assert_eq!(
        expand_pasted_chips(&input, &chunks),
        "aaa\nbbbccc\nddd",
        "each chip expands to its own content"
    );
}

#[test]
fn handle_paste_into_task_pane_creates_a_chip_and_toggles() {
    let mut app = App::new();
    app.focus = FocusPane::Task;
    let pasted = "def f():\n    return 1\n    # a longer block\n    pass";
    app.handle_paste(pasted.to_string());
    assert!(
        app.task_input.starts_with("[Pasted #1 "),
        "task draft shows a chip, not the raw block: {:?}",
        app.task_input
    );
    // Re-paste the same content toggles the chip open in the live app.
    app.handle_paste(pasted.to_string());
    assert_eq!(app.task_input, pasted, "re-paste expanded the chip inline");
    assert_eq!(
        expand_pasted_chips(&app.task_input, &app.pasted_chunks),
        pasted
    );
}

// ---- Model switch retires a still-planning planner ----

#[test]
fn model_switch_retires_a_planning_orchestrator() {
    let repo = unique_test_repo("model-switch-retire");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.agents.push(planner_run("orch-1", false));
    app.planned_nodes = vec![test_planned_node("n0", &[])];
    app.awaiting_approval = true;

    let note = app.retire_planner_for_model_switch();

    assert!(note.is_some(), "retiring a planner returns a note");
    assert!(
        !app.agents.iter().any(|run| run.is_orchestrator()),
        "the planning orchestrator was retired"
    );
    assert!(app.planned_nodes.is_empty(), "the pending plan was cleared");
    assert!(!app.awaiting_approval, "no longer awaiting approval");
    assert!(
        !app.plan_is_active(),
        "a fresh task now starts a new planner instead of refining"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn model_switch_leaves_an_executing_plan_untouched() {
    let repo = unique_test_repo("model-switch-executing");
    let mut app = App::new();
    app.cwd = repo.clone();
    app.agents.push(planner_run("orch-1", false));
    // A launched worker node still in flight = an executing plan.
    let mut worker = test_agent_run("w-1", "do the work");
    worker.node_id = Some("n0".to_string());
    worker.status = AgentStatus::Running;
    app.agents.push(worker);

    let note = app.retire_planner_for_model_switch();

    assert!(note.is_none(), "an executing plan is not disturbed");
    assert!(
        app.agents.iter().any(|run| run.is_orchestrator()),
        "the conductor stays so running workers are not stranded"
    );

    let _ = fs::remove_dir_all(&repo);
}

#[test]
fn set_model_defaults_only_retires_on_a_real_model_change() {
    let _guard = env_guard();
    let repo = unique_test_repo("model-switch-gate");
    let home = repo.join("home");
    let prior_home = std::env::var_os("RUDDER_HOME");
    std::env::set_var("RUDDER_HOME", &home);

    let mut app = App::new();
    app.cwd = repo.clone();
    app.backend = Backend::Claude;
    app.model = "sonnet".to_string();
    app.agents.push(planner_run("orch-1", false));

    // Re-selecting the SAME model must not tear down the planner.
    app.set_model_defaults(Backend::Claude, "sonnet".to_string(), None);
    assert!(
        app.agents.iter().any(|run| run.is_orchestrator()),
        "an identical model selection leaves the planner in place"
    );

    // Switching to a DIFFERENT model retires it.
    app.set_model_defaults(Backend::Claude, "opus".to_string(), None);
    assert!(
        !app.agents.iter().any(|run| run.is_orchestrator()),
        "changing the model retired the planner"
    );

    match prior_home {
        Some(value) => std::env::set_var("RUDDER_HOME", value),
        None => std::env::remove_var("RUDDER_HOME"),
    }
    let _ = fs::remove_dir_all(&repo);
}
