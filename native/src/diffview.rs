#![allow(unused_imports)]
//! The diff panel (⌥d): a structured, scrollable view of the selected agent's
//! change set, drawn beside the worker pane.
//!
//! Unlike the `v` review view (a live `jj diff` running in a PTY), this parses
//! the unified diff itself so it can draw per-file stats, line numbers and
//! colours, jump between files, and put the run's token usage and cost in the
//! header — the shape of opencode's diff viewer, on rudder's canvas.
//!
//! The diff is produced on a background thread (jj or git can take a few
//! hundred milliseconds on a big workspace) and re-requested every couple of
//! seconds while the panel is open and the worker has produced output since.
use super::*;
use std::sync::mpsc::{channel, Receiver, Sender};

/// Where the diff came from. A jj workspace diffs its working copy against its
/// parent change; a plain git checkout diffs the working tree against HEAD and
/// lists untracked files as additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffSource {
    Jj,
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl FileKind {
    pub(crate) fn badge(self) -> &'static str {
        match self {
            FileKind::Modified => "M",
            FileKind::Added => "A",
            FileKind::Deleted => "D",
            FileKind::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` and similar annotations.
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffLine {
    pub(crate) kind: LineKind,
    pub(crate) old_no: Option<u32>,
    pub(crate) new_no: Option<u32>,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffHunk {
    /// The `@@ -a,b +c,d @@ context` line, verbatim.
    pub(crate) header: String,
    pub(crate) lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFile {
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
    pub(crate) kind: FileKind,
    pub(crate) binary: bool,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    pub(crate) hunks: Vec<DiffHunk>,
}

/// Parse `git diff` / `jj diff --git` output. Tolerant: anything it does not
/// recognise is skipped rather than failing the whole panel.
pub(crate) fn parse_unified_diff(text: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut old_no = 0_u32;
    let mut new_no = 0_u32;
    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let (old, new) = split_git_paths(rest);
            files.push(DiffFile {
                path: new.clone(),
                old_path: (old != new).then_some(old),
                kind: FileKind::Modified,
                binary: false,
                additions: 0,
                deletions: 0,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if raw.starts_with("new file mode") {
            file.kind = FileKind::Added;
            continue;
        }
        if raw.starts_with("deleted file mode") {
            file.kind = FileKind::Deleted;
            continue;
        }
        if raw.starts_with("rename from ") || raw.starts_with("rename to ") {
            file.kind = FileKind::Renamed;
            if let Some(to) = raw.strip_prefix("rename to ") {
                file.path = to.to_string();
            }
            if let Some(from) = raw.strip_prefix("rename from ") {
                file.old_path = Some(from.to_string());
            }
            continue;
        }
        if raw.starts_with("Binary files ") || raw.starts_with("GIT binary patch") {
            file.binary = true;
            continue;
        }
        if raw.starts_with("--- ") || raw.starts_with("+++ ") {
            // `+++ /dev/null` is a deletion even without a mode line (jj).
            if raw == "+++ /dev/null" {
                file.kind = FileKind::Deleted;
            } else if raw == "--- /dev/null" {
                file.kind = FileKind::Added;
            }
            continue;
        }
        if raw.starts_with("index ")
            || raw.starts_with("similarity index")
            || raw.starts_with("old mode")
            || raw.starts_with("new mode")
        {
            continue;
        }
        if let Some(header) = raw.strip_prefix("@@") {
            let (o, n) = parse_hunk_ranges(header);
            old_no = o;
            new_no = n;
            file.hunks.push(DiffHunk {
                header: raw.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        if let Some(text) = raw.strip_prefix('+') {
            hunk.lines.push(DiffLine {
                kind: LineKind::Add,
                old_no: None,
                new_no: Some(new_no),
                text: text.to_string(),
            });
            file.additions += 1;
            new_no += 1;
        } else if let Some(text) = raw.strip_prefix('-') {
            hunk.lines.push(DiffLine {
                kind: LineKind::Del,
                old_no: Some(old_no),
                new_no: None,
                text: text.to_string(),
            });
            file.deletions += 1;
            old_no += 1;
        } else if let Some(text) = raw.strip_prefix('\\') {
            hunk.lines.push(DiffLine {
                kind: LineKind::Meta,
                old_no: None,
                new_no: None,
                text: text.trim().to_string(),
            });
        } else {
            let text = raw.strip_prefix(' ').unwrap_or(raw);
            hunk.lines.push(DiffLine {
                kind: LineKind::Context,
                old_no: Some(old_no),
                new_no: Some(new_no),
                text: text.to_string(),
            });
            old_no += 1;
            new_no += 1;
        }
    }
    files
}

/// `a/path b/path` → (path, path). Paths with spaces are rare in diff --git
/// headers but the `a/`+`b/` prefixes make the split unambiguous when the two
/// halves are the same length; otherwise fall back to the last space.
fn split_git_paths(rest: &str) -> (String, String) {
    let trimmed = rest.trim();
    if let Some(idx) = trimmed.find(" b/") {
        let old = trimmed[..idx].trim_start_matches("a/").to_string();
        let new = trimmed[idx + 3..].to_string();
        return (old, new);
    }
    match trimmed.rsplit_once(' ') {
        Some((old, new)) => (
            old.trim_start_matches("a/").to_string(),
            new.trim_start_matches("b/").to_string(),
        ),
        None => (trimmed.to_string(), trimmed.to_string()),
    }
}

fn parse_hunk_ranges(header: &str) -> (u32, u32) {
    let mut old = 1;
    let mut new = 1;
    for token in header.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            old = rest
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
        } else if let Some(rest) = token.strip_prefix('+') {
            new = rest
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            break;
        }
    }
    (old, new)
}

/// Longest diff the panel will parse; beyond this the tail is dropped and the
/// header says so. Keeps a runaway generated file from freezing the UI.
pub(crate) const DIFF_PANEL_MAX_CHARS: usize = 600_000;

fn find_vcs_root(cwd: &Path) -> Option<DiffSource> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".jj").is_dir() {
            return Some(DiffSource::Jj);
        }
        if d.join(".git").exists() {
            return Some(DiffSource::Git);
        }
        dir = d.parent();
    }
    None
}

/// Produce the unified diff for a workspace. Synchronous; the panel calls it
/// from a background thread.
pub(crate) fn load_diff_text(cwd: &Path) -> Result<(String, DiffSource, bool), String> {
    if !cwd.exists() {
        return Err("workspace is gone (merged work is cleaned up); nothing left to diff".into());
    }
    let source = find_vcs_root(cwd).ok_or_else(|| "not inside a jj or git checkout".to_string())?;
    let mut text = match source {
        DiffSource::Jj => jj_diff_text(cwd, DIFF_PANEL_MAX_CHARS + 1),
        DiffSource::Git => git_diff_text(cwd)?,
    };
    let truncated = text.chars().count() > DIFF_PANEL_MAX_CHARS;
    if truncated {
        text = text.chars().take(DIFF_PANEL_MAX_CHARS).collect();
    }
    Ok((text, source, truncated))
}

fn git_diff_text(cwd: &Path) -> Result<String, String> {
    let run = |args: &[&str]| -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("git: {e}"))
    };
    let tracked = run(&["diff", "HEAD", "--no-color", "--no-ext-diff", "--patch"])?;
    let mut text = if tracked.status.success() {
        String::from_utf8_lossy(&tracked.stdout).into_owned()
    } else {
        // A repo with no commits yet has no HEAD: diff against the empty tree.
        let staged = run(&["diff", "--no-color", "--no-ext-diff", "--patch"])?;
        String::from_utf8_lossy(&staged.stdout).into_owned()
    };
    // Untracked files are part of the change set a reviewer needs to see.
    let untracked = run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
    let names = String::from_utf8_lossy(&untracked.stdout).into_owned();
    for name in names.split('\0').filter(|n| !n.is_empty()).take(200) {
        let path = cwd.join(name);
        let too_big = std::fs::metadata(&path)
            .map(|m| m.len() > 512 * 1024)
            .unwrap_or(true);
        if too_big {
            text.push_str(&format!("diff --git a/{name} b/{name}\nnew file mode 100644\nBinary files /dev/null and b/{name} differ\n"));
            continue;
        }
        // Exit code 1 means "differences found", which is the point.
        let out = run(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-index",
            "--",
            "/dev/null",
            name,
        ])?;
        let patch = String::from_utf8_lossy(&out.stdout);
        if patch.trim().is_empty() {
            continue;
        }
        // `--no-index` writes `a//dev/null`; normalise to the shape the parser reads.
        for line in patch.lines() {
            if line.starts_with("diff --git ") {
                text.push_str(&format!(
                    "diff --git a/{name} b/{name}\nnew file mode 100644\n"
                ));
            } else if line.starts_with("--- ") {
                text.push_str("--- /dev/null\n");
            } else if line.starts_with("+++ ") {
                text.push_str(&format!("+++ b/{name}\n"));
            } else {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    Ok(text)
}

struct DiffJob {
    run_id: String,
    result: Result<(String, DiffSource, bool), String>,
}

/// Refresh cadence while the panel is open.
pub(crate) const DIFF_PANEL_REFRESH: Duration = Duration::from_millis(2500);
/// Token totals are read from the backend's session log; cheaper than a diff
/// but still a file scan, so less often.
pub(crate) const DIFF_PANEL_TOKEN_REFRESH: Duration = Duration::from_secs(10);

pub(crate) struct DiffPanel {
    pub(crate) open: bool,
    pub(crate) run_id: Option<String>,
    pub(crate) files: Vec<DiffFile>,
    pub(crate) source: Option<DiffSource>,
    pub(crate) error: Option<String>,
    pub(crate) truncated: bool,
    /// Ever received a result for `run_id`; until then the panel says "computing".
    pub(crate) loaded: bool,
    pub(crate) scroll: usize,
    /// Bumps on every content change so the rendered-line cache invalidates.
    pub(crate) revision: u64,
    pub(crate) refreshed_at: Option<Instant>,
    pub(crate) tokens_refreshed_at: Option<Instant>,
    /// Instant of the worker output the current diff already reflects.
    pub(crate) output_seen_at: Option<Instant>,
    inflight: bool,
    tx: Sender<DiffJob>,
    rx: Receiver<DiffJob>,
    /// (revision, width, focused) → rendered lines and the line index of each file.
    cache: Option<(u64, u16, bool, Vec<Line<'static>>, Vec<usize>)>,
}

impl DiffPanel {
    pub(crate) fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            open: false,
            run_id: None,
            files: Vec::new(),
            source: None,
            error: None,
            truncated: false,
            loaded: false,
            scroll: 0,
            revision: 0,
            refreshed_at: None,
            tokens_refreshed_at: None,
            output_seen_at: None,
            inflight: false,
            tx,
            rx,
            cache: None,
        }
    }

    /// Point the panel at a run; a different run resets scroll and content.
    pub(crate) fn track(&mut self, run_id: &str) {
        if self.run_id.as_deref() == Some(run_id) {
            return;
        }
        self.run_id = Some(run_id.to_string());
        self.files.clear();
        self.source = None;
        self.error = None;
        self.truncated = false;
        self.loaded = false;
        self.scroll = 0;
        self.refreshed_at = None;
        self.output_seen_at = None;
        self.revision += 1;
        self.cache = None;
    }

    /// Whether a fresh diff should be computed now.
    pub(crate) fn due(&self, last_output_at: Instant) -> bool {
        if self.inflight {
            return false;
        }
        let Some(at) = self.refreshed_at else {
            return true;
        };
        if at.elapsed() < DIFF_PANEL_REFRESH {
            return false;
        }
        // Nothing new from the worker since the last diff: the diff is current.
        self.output_seen_at.is_none_or(|seen| last_output_at > seen)
    }

    /// Compute the diff off-thread; `drain` picks the result up next tick.
    pub(crate) fn request(&mut self, run_id: &str, cwd: PathBuf, last_output_at: Instant) {
        if self.inflight {
            return;
        }
        self.inflight = true;
        self.output_seen_at = Some(last_output_at);
        let tx = self.tx.clone();
        let run_id = run_id.to_string();
        std::thread::spawn(move || {
            let result = load_diff_text(&cwd);
            let _ = tx.send(DiffJob { run_id, result });
        });
    }

    /// Synchronous variant for tests and for a first paint when cheap.
    pub(crate) fn apply(
        &mut self,
        run_id: &str,
        result: Result<(String, DiffSource, bool), String>,
    ) {
        if self.run_id.as_deref() != Some(run_id) {
            return;
        }
        self.loaded = true;
        self.refreshed_at = Some(Instant::now());
        match result {
            Ok((text, source, truncated)) => {
                let files = parse_unified_diff(&text);
                let changed = files != self.files
                    || self.source != Some(source)
                    || self.truncated != truncated
                    || self.error.is_some();
                self.files = files;
                self.source = Some(source);
                self.truncated = truncated;
                self.error = None;
                if changed {
                    self.revision += 1;
                    self.cache = None;
                }
            }
            Err(error) => {
                if self.error.as_deref() != Some(error.as_str()) {
                    self.error = Some(error);
                    self.revision += 1;
                    self.cache = None;
                }
            }
        }
    }

    /// Pull finished jobs in. True when something on screen changed.
    pub(crate) fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(job) = self.rx.try_recv() {
            self.inflight = false;
            let before = self.revision;
            self.apply(&job.run_id, job.result);
            changed |= self.revision != before || !self.loaded;
        }
        changed
    }

    pub(crate) fn additions(&self) -> usize {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub(crate) fn deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }

    /// Rendered lines for one width, cached per revision.
    pub(crate) fn lines(
        &mut self,
        run: &AgentRun,
        width: u16,
        focused: bool,
    ) -> (&[Line<'static>], &[usize]) {
        let stale = match &self.cache {
            Some((rev, w, f, _, _)) => *rev != self.revision || *w != width || *f != focused,
            None => true,
        };
        if stale {
            let (lines, starts) = diff_panel_lines(self, run, width, focused);
            self.cache = Some((self.revision, width, focused, lines, starts));
        }
        let (_, _, _, lines, starts) = self.cache.as_ref().expect("cache filled");
        (lines.as_slice(), starts.as_slice())
    }

    pub(crate) fn max_scroll(&self, viewport: usize) -> usize {
        self.cache
            .as_ref()
            .map(|(_, _, _, lines, _)| lines.len().saturating_sub(viewport))
            .unwrap_or(0)
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, viewport: usize) -> bool {
        let max = self.max_scroll(viewport);
        let next = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        let changed = next != self.scroll;
        self.scroll = next;
        changed
    }

    pub(crate) fn scroll_to(&mut self, line: usize, viewport: usize) -> bool {
        let next = line.min(self.max_scroll(viewport));
        let changed = next != self.scroll;
        self.scroll = next;
        changed
    }

    /// Jump to the next (`forward`) or previous file header.
    pub(crate) fn jump_file(&mut self, forward: bool, viewport: usize) -> bool {
        let starts: Vec<usize> = self
            .cache
            .as_ref()
            .map(|(_, _, _, _, s)| s.clone())
            .unwrap_or_default();
        let target = if forward {
            starts.iter().copied().find(|&s| s > self.scroll)
        } else {
            starts.iter().copied().rev().find(|&s| s < self.scroll)
        };
        match target {
            Some(line) => self.scroll_to(line, viewport),
            None => false,
        }
    }
}

fn truncate_cells(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            if width > 0 {
                out.push('…');
            }
            return out;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

/// Estimated API-rate cost for a run's cumulative tokens, when its model is priced.
pub(crate) fn run_cost_estimate(run: &AgentRun) -> Option<f64> {
    let (input, output, _, _) = model_pricing(&run.model)?;
    Some((run.tokens_in as f64 * input + run.tokens_out as f64 * output) / 1_000_000.0)
}

/// Build the panel's lines: a header with stats, tokens and cost; the file
/// summary; then every file's hunks with a two-column line-number gutter.
/// Returns the lines and the index of each file's header line (for n/p).
pub(crate) fn diff_panel_lines(
    panel: &DiffPanel,
    run: &AgentRun,
    width: u16,
    focused: bool,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let width = width.max(10) as usize;
    let text = pane_text_style(focused);
    let muted = muted_style(focused);
    let add = Style::default().fg(DONE_COLOR);
    let del = Style::default().fg(FAILED_COLOR);
    let bold = text.add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();

    // ── header ───────────────────────────────────────────────────────────
    let summary = if run.task_summary.trim().is_empty() {
        run.task.lines().next().unwrap_or("").to_string()
    } else {
        run.task_summary.clone()
    };
    lines.push(Line::from(Span::styled(
        truncate_cells(&summary, width),
        bold,
    )));
    let mut meta: Vec<Span<'static>> = vec![
        Span::styled(
            format!("{}", run.backend.as_str()),
            Style::default().fg(MODEL_COLOR),
        ),
        Span::styled(
            format!(" {}", short_model_label(&run.model)),
            Style::default().fg(MODEL_COLOR),
        ),
        Span::styled(
            format!("  ·  {}", run.status.as_str()),
            Style::default().fg(status_color(run.status)),
        ),
    ];
    if let Some(source) = panel.source {
        meta.push(Span::styled(
            format!(
                "  ·  {}",
                match source {
                    DiffSource::Jj => "jj",
                    DiffSource::Git => "git vs HEAD",
                }
            ),
            muted,
        ));
    }
    lines.push(Line::from(meta));
    let mut stats: Vec<Span<'static>> = vec![
        Span::styled(format!("+{}", panel.additions()), add),
        Span::styled(format!(" −{}", panel.deletions()), del),
        Span::styled(
            format!(
                "  ·  {} file{}",
                panel.files.len(),
                if panel.files.len() == 1 { "" } else { "s" }
            ),
            text,
        ),
    ];
    if run.tokens_in + run.tokens_out > 0 {
        stats.push(Span::styled(
            format!(
                "  ·  tokens in {} · out {}",
                format_token_count(run.tokens_in),
                format_token_count(run.tokens_out)
            ),
            muted,
        ));
        match run_cost_estimate(run) {
            Some(cost) => stats.push(Span::styled(
                format!("  ·  ≈ ${cost:.2} at API rates"),
                text,
            )),
            None => stats.push(Span::styled("  ·  cost n/a".to_string(), muted)),
        }
    } else {
        stats.push(Span::styled(
            "  ·  tokens: none recorded yet".to_string(),
            muted,
        ));
    }
    lines.push(Line::from(stats));
    if panel.truncated {
        lines.push(Line::from(Span::styled(
            "diff truncated: too large to show in full".to_string(),
            Style::default().fg(RUNNING_COLOR),
        )));
    }
    lines.push(Line::from(""));

    if let Some(error) = &panel.error {
        lines.push(Line::from(Span::styled(
            "diff unavailable".to_string(),
            error_style(),
        )));
        lines.push(Line::from(Span::styled(
            truncate_cells(error, width),
            error_style(),
        )));
        return (lines, starts);
    }
    if !panel.loaded {
        lines.push(Line::from(Span::styled(
            "computing diff…".to_string(),
            muted,
        )));
        return (lines, starts);
    }
    if panel.files.is_empty() {
        lines.push(Line::from(Span::styled(
            "no changes yet".to_string(),
            muted,
        )));
        lines.push(Line::from(Span::styled(
            "the panel refreshes as the agent edits files".to_string(),
            muted,
        )));
        return (lines, starts);
    }

    // ── file summary ─────────────────────────────────────────────────────
    let stat_width = 14;
    for file in &panel.files {
        let badge_style = match file.kind {
            FileKind::Added => add,
            FileKind::Deleted => del,
            FileKind::Renamed => Style::default().fg(ACCENT),
            FileKind::Modified => Style::default().fg(RUNNING_COLOR),
        };
        let path = match &file.old_path {
            Some(old) if file.kind == FileKind::Renamed => format!("{old} → {}", file.path),
            _ => file.path.clone(),
        };
        // badge (3) + path + space (1) + stats (stat_width) must fit the width.
        let path_width = width.saturating_sub(stat_width + 4);
        let stats = if file.binary {
            "binary".to_string()
        } else {
            format!("+{} −{}", file.additions, file.deletions)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", file.kind.badge()), badge_style),
            Span::styled(
                format!("{:<w$}", truncate_cells(&path, path_width), w = path_width),
                text,
            ),
            Span::styled(format!(" {stats:>stat_width$}"), muted),
        ]));
    }
    lines.push(Line::from(""));

    // ── hunks ────────────────────────────────────────────────────────────
    let max_no = panel
        .files
        .iter()
        .flat_map(|f| f.hunks.iter())
        .flat_map(|h| h.lines.iter())
        .map(|l| l.old_no.unwrap_or(0).max(l.new_no.unwrap_or(0)))
        .max()
        .unwrap_or(0);
    let digits = max_no.to_string().len().max(3);
    let gutter = width >= 48;
    let body_width = if gutter {
        width.saturating_sub(digits * 2 + 4)
    } else {
        width.saturating_sub(2)
    };

    for file in &panel.files {
        starts.push(lines.len());
        let title = match &file.old_path {
            Some(old) if file.kind == FileKind::Renamed => format!("{old} → {}", file.path),
            _ => file.path.clone(),
        };
        let label = format!("{} {} ", file.kind.badge(), title);
        let rule = "─".repeat(width.saturating_sub(label.chars().count() + 1));
        lines.push(Line::from(vec![
            Span::styled(truncate_cells(&label, width), bold.fg(ACCENT)),
            Span::styled(rule, Style::default().fg(FAINT)),
        ]));
        if file.binary {
            lines.push(Line::from(Span::styled("  binary file".to_string(), muted)));
            lines.push(Line::from(""));
            continue;
        }
        if file.hunks.is_empty() {
            lines.push(Line::from(Span::styled(
                match file.kind {
                    FileKind::Renamed => "  renamed, contents unchanged".to_string(),
                    _ => "  mode or metadata change".to_string(),
                },
                muted,
            )));
            lines.push(Line::from(""));
            continue;
        }
        for hunk in &file.hunks {
            lines.push(Line::from(Span::styled(
                truncate_cells(&hunk.header, width),
                Style::default().fg(ACCENT_DEEP),
            )));
            for line in &hunk.lines {
                let (sigil, style) = match line.kind {
                    LineKind::Add => ("+", add),
                    LineKind::Del => ("−", del),
                    LineKind::Context => (" ", text),
                    LineKind::Meta => ("\\", muted),
                };
                let body = truncate_cells(&expand_tabs(&line.text), body_width);
                let mut spans: Vec<Span<'static>> = Vec::new();
                if gutter {
                    let old = line.old_no.map(|n| n.to_string()).unwrap_or_default();
                    let new = line.new_no.map(|n| n.to_string()).unwrap_or_default();
                    spans.push(Span::styled(
                        format!("{old:>digits$} {new:>digits$} "),
                        Style::default().fg(FAINT),
                    ));
                    spans.push(Span::styled("│".to_string(), Style::default().fg(FAINT)));
                }
                spans.push(Span::styled(format!("{sigil}{body}"), style));
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::from(""));
    }
    (lines, starts)
}
