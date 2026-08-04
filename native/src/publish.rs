#![allow(unused_imports)]
//! Publishing reviewed work to GitHub as draft pull requests.
//!
//! Rudder's road to main used to be exactly one: `m` merged a reviewed workspace
//! into the user's local checkout, and nothing ever left the machine. Publishing
//! is the second road, and the rule is that a repo has ONE of them, never both:
//! where publishing can work (a GitHub remote, `gh` present and logged in), `m`
//! opens a pull request instead of merging locally. Where it cannot, `m` merges
//! locally exactly as before. Two roads for the same row would mean work landing
//! in main twice, by two different mechanisms, with no single place to look for
//! "did this ship?".
//!
//! Three things this module is careful about:
//!
//! - **Every shell-out is bounded.** `gh` talks to the network; a hung request on
//!   the render loop is a frozen dashboard, so `bounded_output` spawns, polls
//!   `try_wait` against a deadline, and kills the child on overrun (the
//!   `cloudio::query_cloud_workspace_status` pattern).
//! - **Detection is cached and backed off.** Whether this repo can publish is a
//!   question about `gh`, auth and the remote — all of which change roughly never.
//!   The probe runs on a background thread and its cadence doubles while the
//!   answer holds still (AGENTS.md 12.8).
//! - **What GitHub knows, GitHub is asked.** The PR NUMBER is a durable fact about
//!   what Rudder did, so it is recorded. Whether that PR is still a draft is
//!   GitHub's answer and is re-derived, never loaded off disk (AGENTS.md 12.7).
use super::*;

/// How long a single `gh`/`jj`/`git` child may run before it is killed. `gh`
/// commands are network round-trips; `jj git push` is one too. Twenty seconds is
/// long enough for a slow link and short enough that a wedged child is noticed as
/// a failure rather than as a hang.
pub(crate) const PUBLISH_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// `gh stack sync`/`submit` push and rebase every branch in the stack, so they get
/// a longer leash than a single-PR call.
pub(crate) const PUBLISH_STACK_TIMEOUT: Duration = Duration::from_secs(90);

pub(crate) const PUBLISH_PROBE_BASE_INTERVAL: Duration = Duration::from_secs(20);
pub(crate) const PUBLISH_PROBE_MAX_INTERVAL: Duration = Duration::from_secs(600);
pub(crate) const PUBLISH_PR_STATE_BASE_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) const PUBLISH_PR_STATE_MAX_INTERVAL: Duration = Duration::from_secs(600);

/// The extension B2 stacks are built on. Absent means single PRs still work; only
/// stacking is unavailable.
pub(crate) const GH_STACK_EXTENSION: &str = "github/gh-stack";

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

/// Why this repo cannot publish. Each variant carries what the user would have to
/// do about it: "publish failed" with no reason is how a logged-out `gh` gets read
/// as a broken feature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PublishBlocker {
    /// No `origin`, or an `origin` that is not GitHub.
    NotGitHub,
    /// `gh` is not on PATH.
    GhMissing,
    /// `gh` is installed but `gh auth status` says no.
    GhUnauthenticated,
    /// `gh` and auth are fine but the repo itself could not be read (no access,
    /// renamed, offline). Carries GitHub's own first line of complaint.
    RepoUnreadable(String),
}

impl PublishBlocker {
    /// The notice text. Says the problem AND the fix, in that order.
    pub(crate) fn explain(&self) -> String {
        match self {
            Self::NotGitHub => {
                "no GitHub remote: publishing is off, m merges locally".to_string()
            }
            Self::GhMissing => {
                "gh is not installed: publishing is off, m merges locally (install gh to open PRs)"
                    .to_string()
            }
            Self::GhUnauthenticated => {
                "gh is not authenticated: run `gh auth login`, then m opens PRs instead of merging"
                    .to_string()
            }
            Self::RepoUnreadable(detail) => {
                format!("gh cannot read this repo ({detail}): publishing is off, m merges locally")
            }
        }
    }
}

/// Everything publishing needs to know about the remote, learned once and cached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishCapability {
    /// Git remote name. Always `origin` today; carried explicitly so the
    /// confirmation can NAME what it is about to push to rather than imply it.
    pub(crate) remote: String,
    pub(crate) remote_url: String,
    pub(crate) name_with_owner: String,
    /// The PR base. Also the one branch publishing may never push to.
    pub(crate) default_branch: String,
    /// gh-stack cannot stack across forks, so a fork publishes independent PRs.
    pub(crate) is_fork: bool,
    /// Whether the `gh stack` extension is installed. Its absence costs stacking,
    /// not publishing.
    pub(crate) has_gh_stack: bool,
}

impl PublishCapability {
    /// Can a plan be published as a STACK, or only as independent PRs? Returns the
    /// reason when it cannot, so the notice can say why instead of silently
    /// producing a different shape than the user expected.
    pub(crate) fn stack_blocker(&self) -> Option<String> {
        if self.is_fork {
            return Some(format!(
                "{} is a fork and gh stack cannot stack across forks",
                self.name_with_owner
            ));
        }
        if !self.has_gh_stack {
            return Some(format!(
                "the {GH_STACK_EXTENSION} extension is not installed (gh extension install {GH_STACK_EXTENSION})"
            ));
        }
        None
    }
}

/// Publishing's three states. `Unknown` is the startup state and is NOT the same
/// as `Inactive`: before the first probe lands we do not yet know which road this
/// repo is on, and treating "not yet asked" as "cannot publish" would make the
/// first `m` after launch merge locally on a repo that publishes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PublishState {
    Unknown,
    Active(PublishCapability),
    Inactive(PublishBlocker),
}

impl Default for PublishState {
    fn default() -> Self {
        Self::Unknown
    }
}

// ---------------------------------------------------------------------------
// Evidence recorded on a run
// ---------------------------------------------------------------------------

/// What Rudder did when it published a row, plus what GitHub currently says about
/// it. The split matters: `branch`/`number`/`url` are facts about an action that
/// happened and persist to run.json; `state` is GitHub's answer and is re-derived
/// on a backed-off poll, never loaded from the record (AGENTS.md 12.7). A row that
/// claimed "draft" forever after someone marked the PR ready would be the same
/// class of lie as a row claiming "merged" after history moved under it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PublishEvidence {
    pub(crate) branch: Option<String>,
    pub(crate) number: Option<u64>,
    pub(crate) url: Option<String>,
    /// `draft` / `open` / `merged` / `closed`. `None` = not asked yet.
    pub(crate) state: Option<String>,
}

impl PublishEvidence {
    pub(crate) fn is_published(&self) -> bool {
        self.number.is_some()
    }

    /// The agents-pane label: `PR #123 · draft`, or just `PR #123` before the first
    /// state refresh lands. Never invents a state it has not been told.
    pub(crate) fn label(&self) -> Option<String> {
        let number = self.number?;
        Some(match self.state.as_deref() {
            Some(state) if !state.trim().is_empty() => format!("PR #{number} · {state}"),
            _ => format!("PR #{number}"),
        })
    }
}

/// Read publish evidence back off a run record. `state` is deliberately not read:
/// see `PublishEvidence`.
pub(crate) fn publish_evidence_from_record(record: &serde_json::Value) -> PublishEvidence {
    let publish = record.get("publish");
    let text = |field: &str| {
        publish
            .and_then(|value| value.get(field))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    };
    PublishEvidence {
        branch: text("branch"),
        number: publish
            .and_then(|value| value.get("number"))
            .and_then(serde_json::Value::as_u64),
        url: text("url"),
        state: None,
    }
}

// ---------------------------------------------------------------------------
// Per-repo acceptance
// ---------------------------------------------------------------------------

/// Rudder has never pushed to a remote — that is a documented invariant in
/// RUDDER.md, AGENTS.md and on the website. The first publish in a repo is
/// therefore the moment that invariant changes for that user, and it must not
/// happen silently. The acceptance is recorded against the REMOTE URL, not just
/// the repo: repointing `origin` at a different GitHub repo is a different
/// promise, and asking again is the honest thing to do.
pub(crate) fn publish_acceptance_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".rudder").join("publish.json")
}

pub(crate) fn publish_accepted_for(repo_root: &Path, remote_url: &str) -> bool {
    let Ok(raw) = fs::read_to_string(publish_acceptance_path(repo_root)) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value
        .get("acceptedRemoteUrl")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|accepted| accepted == remote_url)
}

pub(crate) fn record_publish_acceptance(repo_root: &Path, remote_url: &str) -> Result<()> {
    let path = publish_acceptance_path(repo_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "acceptedRemoteUrl": remote_url,
        "acceptedAt": now_stamp(),
    });
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&body)?))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Command construction (pure — the argv is built here, run elsewhere)
// ---------------------------------------------------------------------------

/// Branch names Rudder creates. Namespaced so a publish branch is never mistaken
/// for a hand-made one, and never the default branch (see `publish_branch_name`).
pub(crate) const PUBLISH_BRANCH_PREFIX: &str = "rudder";

/// The branch a row publishes to. Slug from what the row is ABOUT plus a short
/// hash of its run id, because two rows can carry the same summary and a collision
/// would silently repoint the earlier one's branch at the later one's work.
///
/// The default branch can never be produced: decision 5 is that publishing always
/// pushes a fresh branch, so a name that would collide with trunk is suffixed
/// rather than used.
pub(crate) fn publish_branch_name(run_id: &str, label: &str, default_branch: &str) -> String {
    let slug = slugify(label, "work");
    let name = format!("{PUBLISH_BRANCH_PREFIX}/{slug}-{}", short_run_key(run_id));
    if name == default_branch {
        format!("{name}-pr")
    } else {
        name
    }
}

/// Short, stable disambiguator for a run id. Not a hash of the content — the id
/// itself is already unique, this only shortens it for a branch name.
fn short_run_key(run_id: &str) -> String {
    let cleaned: String = run_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if cleaned.len() <= 8 {
        if cleaned.is_empty() {
            short_hash(run_id)[..8].to_string()
        } else {
            cleaned
        }
    } else {
        cleaned[cleaned.len() - 8..].to_string()
    }
}

/// PR title from what the row was asked to do. GitHub truncates nothing, but a
/// title that runs to a full task prompt is unreadable in a PR list.
pub(crate) fn publish_pr_title(task_summary: &str, task: &str) -> String {
    let source = if task_summary.trim().is_empty() {
        task.trim()
    } else {
        task_summary.trim()
    };
    let single_line = source.split('\n').next().unwrap_or_default().trim();
    if single_line.is_empty() {
        return "Rudder change".to_string();
    }
    let truncated: String = single_line.chars().take(72).collect();
    truncated
}

/// PR body: the worker's own account of what it did when it left one, and always a
/// trailer naming the run so a PR can be traced back to the row that opened it.
pub(crate) fn publish_pr_body(run_id: &str, task: &str, done_summary: Option<&str>) -> String {
    let mut body = String::new();
    if let Some(summary) = done_summary.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str(summary);
        body.push_str("\n\n");
    } else if !task.trim().is_empty() {
        body.push_str(task.trim());
        body.push_str("\n\n");
    }
    body.push_str(&format!("---\nOpened by Rudder from run `{run_id}`.\n"));
    body
}

/// `jj bookmark set <name> -r <change>` — point (or create) the bookmark at the
/// run's own change. `set` rather than `create` so republishing an amended row
/// moves the bookmark instead of failing on "already exists".
pub(crate) fn jj_bookmark_set_argv(bookmark: &str, change_id: &str) -> Vec<String> {
    vec![
        "bookmark".to_string(),
        "set".to_string(),
        bookmark.to_string(),
        "-r".to_string(),
        change_id.to_string(),
    ]
}

/// `jj git export` — materialize jj bookmarks as real git refs. gh/gh-stack are
/// git tools and cannot see a bookmark that has not been exported.
pub(crate) fn jj_git_export_argv() -> Vec<String> {
    vec!["git".to_string(), "export".to_string()]
}

/// `jj git push --remote <remote> --bookmark <name>`. No `--allow-new`: it was
/// removed in jj 0.40 and a plain `--bookmark` push creates the remote bookmark
/// (see AGENTS.md 12).
pub(crate) fn jj_git_push_argv(remote: &str, bookmark: &str) -> Vec<String> {
    vec![
        "git".to_string(),
        "push".to_string(),
        "--remote".to_string(),
        remote.to_string(),
        "--bookmark".to_string(),
        bookmark.to_string(),
    ]
}

/// `gh pr create --draft ...`. Draft is not a flag we let callers choose: decision
/// 4 is that Rudder's PRs open as drafts, so the only way to get a ready PR is for
/// a human to press the button on GitHub.
pub(crate) fn gh_pr_create_argv(base: &str, head: &str, title: &str, body: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "create".to_string(),
        "--draft".to_string(),
        "--base".to_string(),
        base.to_string(),
        "--head".to_string(),
        head.to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ]
}

/// `gh stack init --base <trunk> <b1> <b2> ...` — bottom-to-top. Existing branches
/// are adopted, which is what makes this work over branches jj already exported.
pub(crate) fn gh_stack_init_argv(base: &str, branches: &[String]) -> Vec<String> {
    let mut argv = vec![
        "stack".to_string(),
        "init".to_string(),
        "--base".to_string(),
        base.to_string(),
    ];
    argv.extend(branches.iter().cloned());
    argv
}

/// `gh stack sync` — the ONLY way Rudder pushes a stack. `gh stack push` is
/// documented as non-atomic ("a branch may update even if another branch is
/// rejected"), which would leave a stack half-published with no way to tell which
/// half. `sync` pushes with `--force-with-lease --atomic` and, on a rebase
/// conflict, restores every branch to its original state.
pub(crate) fn gh_stack_sync_argv() -> Vec<String> {
    vec!["stack".to_string(), "sync".to_string()]
}

/// `gh stack submit --auto`. `--auto` skips the interactive editor (Rudder is
/// always a non-interactive terminal here, which would skip it anyway — saying so
/// explicitly means the behavior does not depend on how the child's tty is
/// detected). `--open` is omitted ON PURPOSE: without it new PRs are created as
/// drafts, which is how decision 4 is satisfied for stacks.
pub(crate) fn gh_stack_submit_argv() -> Vec<String> {
    vec!["stack".to_string(), "submit".to_string(), "--auto".to_string()]
}

/// `gh stack view --json` — machine-readable stack state. The human output is a
/// box-drawing table with status GLYPHS; scraping it would break on the next
/// release of the extension.
pub(crate) fn gh_stack_view_json_argv() -> Vec<String> {
    vec!["stack".to_string(), "view".to_string(), "--json".to_string()]
}

/// `gh pr edit <n> --title ... --body ...`. Needed because `gh stack submit` in a
/// non-interactive terminal has no title/body flags and uses AUTO-GENERATED
/// titles; the node's own goal is the title the reviewer should see.
pub(crate) fn gh_pr_edit_argv(number: u64, title: &str, body: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "edit".to_string(),
        number.to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ]
}

/// One `gh pr list` for every row, rather than one `gh pr view` per row: the
/// dashboard can hold dozens of published rows and the per-row shape would spawn
/// dozens of network calls per refresh.
pub(crate) fn gh_pr_list_argv() -> Vec<String> {
    vec![
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        "all".to_string(),
        "--limit".to_string(),
        "100".to_string(),
        "--json".to_string(),
        "number,state,isDraft,url".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// DAG decomposition
// ---------------------------------------------------------------------------

/// How one group of a plan's nodes reaches GitHub.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishUnitKind {
    /// A linear chain of hard edges: PR N's base is PR N-1's branch.
    Stack,
    /// One node with no hard edge tying it to a stackable neighbour. Its PR
    /// targets the default branch.
    Independent,
    /// Two or more hard parents. Cannot be stacked (a stack has one base), so its
    /// PR targets the default branch and waits for its parents' PRs to merge.
    Join,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishUnit {
    pub(crate) kind: PublishUnitKind,
    /// Node ids, bottom-to-top in dependency order for a `Stack`.
    pub(crate) ids: Vec<String>,
}

/// Decompose a plan's DAG into the units that can actually become pull requests.
///
/// A plan's nodes are not independent and they are not a single line either, so
/// neither shape can be assumed. The three rules:
///
/// - a JOIN (two or more hard parents inside the publish set) is its own unit. A
///   stack has exactly one base; a node with two parents has no single branch to
///   sit on, so it targets trunk and waits for both parents' PRs to merge.
/// - a LINEAR CHAIN of hard edges becomes one stack, bottom-to-top. "Linear" is
///   strict: the link only survives if the parent has exactly one hard child and
///   the child has exactly one hard parent. A parent with two children is a fork,
///   and stacking one arbitrary child on it would invent an order.
/// - anything else is an INDEPENDENT PR off the default branch. Nodes with no hard
///   edge between them are genuinely parallel, and linearizing them into a stack
///   would assert a dependency that does not exist — the reviewer would then be
///   told PR 2 needs PR 1 first, which is false.
///
/// Input is `(node id, hard parent ids)` in the order the units should be tried.
/// Parents outside the set are ignored: an edge to a node that is not being
/// published cannot be expressed as a stack base.
pub(crate) fn decompose_plan_for_publish(nodes: &[(String, Vec<String>)]) -> Vec<PublishUnit> {
    let present: HashSet<&str> = nodes.iter().map(|(id, _)| id.as_str()).collect();
    // Hard parents restricted to the publish set, deduped: a DAG edited by hand can
    // list the same parent twice, and that must not read as a join.
    let parents: HashMap<&str, Vec<&str>> = nodes
        .iter()
        .map(|(id, deps)| {
            let mut kept: Vec<&str> = Vec::new();
            for dep in deps {
                let dep = dep.as_str();
                if present.contains(dep) && dep != id.as_str() && !kept.contains(&dep) {
                    kept.push(dep);
                }
            }
            (id.as_str(), kept)
        })
        .collect();
    let is_join = |id: &str| parents.get(id).is_some_and(|list| list.len() >= 2);

    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, _) in nodes {
        for parent in parents.get(id.as_str()).into_iter().flatten() {
            children.entry(parent).or_default().push(id.as_str());
        }
    }

    // A link is only stackable when it is unambiguous in BOTH directions.
    let can_link = |parent: &str, child: &str| -> bool {
        if is_join(parent) || is_join(child) {
            return false;
        }
        let child_parents = parents.get(child).map(Vec::as_slice).unwrap_or_default();
        let parent_children = children.get(parent).map(Vec::as_slice).unwrap_or_default();
        child_parents == [parent] && parent_children == [child]
    };

    let mut claimed: HashSet<&str> = HashSet::new();
    let mut units = Vec::new();
    for (id, _) in nodes {
        let id = id.as_str();
        if claimed.contains(id) {
            continue;
        }
        if is_join(id) {
            claimed.insert(id);
            units.push(PublishUnit {
                kind: PublishUnitKind::Join,
                ids: vec![id.to_string()],
            });
            continue;
        }
        // Only start a chain at its bottom, so the walk below emits it once and in
        // dependency order.
        let has_linked_parent = parents
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .any(|parent| can_link(parent, id));
        if has_linked_parent {
            continue;
        }
        let mut chain = vec![id];
        claimed.insert(id);
        let mut cursor = id;
        while let Some(next) = children
            .get(cursor)
            .and_then(|kids| kids.iter().find(|kid| can_link(cursor, kid)))
        {
            chain.push(next);
            claimed.insert(next);
            cursor = next;
        }
        units.push(PublishUnit {
            kind: if chain.len() >= 2 {
                PublishUnitKind::Stack
            } else {
                PublishUnitKind::Independent
            },
            ids: chain.into_iter().map(ToOwned::to_owned).collect(),
        });
    }
    // A chain whose bottom was claimed as a join's child can still be unvisited;
    // sweep the leftovers so no node is silently dropped from the publish.
    for (id, _) in nodes {
        let id = id.as_str();
        if claimed.contains(id) {
            continue;
        }
        claimed.insert(id);
        let mut chain = vec![id];
        let mut cursor = id;
        while let Some(next) = children.get(cursor).and_then(|kids| {
            kids.iter()
                .find(|kid| can_link(cursor, kid) && !claimed.contains(**kid))
        }) {
            chain.push(next);
            claimed.insert(next);
            cursor = next;
        }
        units.push(PublishUnit {
            kind: if chain.len() >= 2 {
                PublishUnitKind::Stack
            } else {
                PublishUnitKind::Independent
            },
            ids: chain.into_iter().map(ToOwned::to_owned).collect(),
        });
    }
    units
}

/// When a stack is impossible (fork remote, missing extension), every unit becomes
/// an independent PR. The nodes still depend on each other in reality, so this is
/// a DEGRADED shape and the caller says so in the notice rather than presenting it
/// as what was asked for.
pub(crate) fn flatten_units_to_independent(units: &[PublishUnit]) -> Vec<PublishUnit> {
    units
        .iter()
        .flat_map(|unit| {
            unit.ids.iter().map(|id| PublishUnit {
                kind: if unit.kind == PublishUnitKind::Join {
                    PublishUnitKind::Join
                } else {
                    PublishUnitKind::Independent
                },
                ids: vec![id.clone()],
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

/// Pull the PR number out of whatever `gh pr create` printed. gh prints the PR URL
/// on its own line; anything else on stdout is noise we must not mistake for it.
pub(crate) fn parse_pr_url(text: &str) -> Option<(u64, String)> {
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !line.contains("/pull/") {
            continue;
        }
        let number = line.rsplit('/').next()?.parse::<u64>().ok()?;
        return Some((number, line.to_string()));
    }
    None
}

/// One branch's state as `gh stack view --json` reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StackBranchState {
    pub(crate) branch: String,
    pub(crate) number: Option<u64>,
    pub(crate) url: Option<String>,
    /// gh-stack's own "this branch needs a rebase" condition. Surfaced rather than
    /// auto-repaired: a rebase that Rudder decided to run on its own is exactly the
    /// kind of history rewrite the user should have asked for.
    pub(crate) needs_rebase: bool,
}

/// Tolerant reader for `gh stack view --json`. The extension is v0.1.0 and its
/// key names are not a stable contract, so this looks for the branch array
/// wherever it is and accepts the obvious spellings of each field. Anything it
/// cannot read comes back empty, and the caller falls back to reporting the
/// branches it pushed rather than inventing PR numbers.
pub(crate) fn parse_stack_view_json(text: &str) -> Vec<StackBranchState> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Vec::new();
    };
    let array = if value.is_array() {
        value.as_array().cloned().unwrap_or_default()
    } else {
        ["branches", "stack", "entries", "layers"]
            .iter()
            .find_map(|key| value.get(*key).and_then(serde_json::Value::as_array).cloned())
            .unwrap_or_default()
    };
    array
        .iter()
        .filter_map(|entry| {
            let branch = ["branch", "name", "branchName", "head"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(serde_json::Value::as_str))?
                .to_string();
            // The PR may be a bare number or a nested object.
            let pr = entry.get("pr").or_else(|| entry.get("pullRequest"));
            let number = ["number", "prNumber", "pr"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(serde_json::Value::as_u64))
                .or_else(|| pr.and_then(|pr| pr.get("number")).and_then(serde_json::Value::as_u64));
            let url = ["url", "prUrl"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(serde_json::Value::as_str))
                .or_else(|| pr.and_then(|pr| pr.get("url")).and_then(serde_json::Value::as_str))
                .map(ToOwned::to_owned);
            let needs_rebase = ["needsRebase", "needs_rebase", "requiresRebase"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(serde_json::Value::as_bool))
                .unwrap_or(false);
            Some(StackBranchState {
                branch,
                number,
                url,
                needs_rebase,
            })
        })
        .collect()
}

/// Map PR number -> the label the row should show, from `gh pr list --json`.
/// `isDraft` outranks `state`: a draft PR is OPEN as far as GitHub's state field
/// is concerned, and "open" on a row that nobody can merge yet is the wrong story.
pub(crate) fn parse_pr_states(text: &str) -> HashMap<u64, String> {
    let mut states = HashMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return states;
    };
    let Some(array) = value.as_array() else {
        return states;
    };
    for entry in array {
        let Some(number) = entry.get("number").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let draft = entry
            .get("isDraft")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let state = entry
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("OPEN")
            .to_ascii_lowercase();
        let label = if state == "open" && draft {
            "draft".to_string()
        } else {
            state
        };
        states.insert(number, label);
    }
    states
}

/// Classify a failed `gh stack sync`. Sync fails SAFE — it restores every branch —
/// so each of these is a normal outcome to report, not something to retry blindly.
pub(crate) fn classify_stack_sync_failure(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("conflict") {
        return "gh stack sync hit a rebase conflict and restored every branch; run `gh stack rebase` to resolve it, then press m again".to_string();
    }
    if lower.contains("diverge") {
        return "the local stack and the stack on GitHub have diverged; gh stack sync aborted without pushing (Rudder runs it non-interactively). Reconcile with `gh stack sync` in a terminal, then press m again".to_string();
    }
    let detail = first_error_line(stderr);
    if detail.is_empty() {
        "gh stack sync failed; nothing was pushed".to_string()
    } else {
        format!("gh stack sync failed ({detail}); nothing was pushed")
    }
}

/// First line of a child's stderr that actually says something. Used everywhere a
/// failure is reported, so the notice names the real problem instead of "failed".
pub(crate) fn first_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(160).collect())
        .unwrap_or_default()
}

/// Parse the owner/name out of a git remote URL. Both the https and ssh spellings,
/// because a repo cloned over ssh publishes exactly as well as one cloned over
/// https and refusing it would be an arbitrary limitation.
pub(crate) fn github_slug_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://github.com/") {
        rest
    } else {
        return None;
    };
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|value| !value.is_empty())?;
    let name = parts.next().filter(|value| !value.is_empty())?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

// ---------------------------------------------------------------------------
// Bounded execution
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct BoundedOutput {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) timed_out: bool,
    /// The child could not be spawned at all (binary missing).
    pub(crate) spawn_failed: bool,
}

/// Run a child with a deadline. This is the only way this module executes
/// anything: `gh` is a network client and the TUI redraws at 33ms, so an
/// unbounded `Command::output()` on the loop is a freeze, not a slow frame.
/// Spawn, poll `try_wait`, kill on overrun — the
/// `cloudio::query_cloud_workspace_status` pattern, with both pipes drained from
/// threads so a chatty child cannot fill a pipe buffer and deadlock the wait.
pub(crate) fn bounded_output(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> BoundedOutput {
    let mut child = match Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            return BoundedOutput {
                spawn_failed: true,
                ..BoundedOutput::default()
            };
        }
    };
    let read_pipe = |pipe: Option<Box<dyn Read + Send>>| {
        pipe.map(|mut pipe| {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                buf
            })
        })
    };
    let stdout_reader = read_pipe(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr_reader = read_pipe(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let (mut ok, mut timed_out) = (false, false);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                ok = status.success();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap; closes the pipes so the readers finish
                    timed_out = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let join = |reader: Option<thread::JoinHandle<Vec<u8>>>| {
        reader
            .and_then(|handle| handle.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    BoundedOutput {
        ok,
        stdout: join(stdout_reader),
        stderr: join(stderr_reader),
        timed_out,
        spawn_failed: false,
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Learn whether this repo can publish. Runs on a BACKGROUND thread: it makes up
/// to three network round-trips (`gh auth status`, `gh repo view`) and none of
/// them may sit on the render loop.
pub(crate) fn probe_publish_capability(cwd: &Path) -> PublishState {
    let remote = "origin";
    let url = bounded_output(
        "git",
        &[
            "remote".to_string(),
            "get-url".to_string(),
            remote.to_string(),
        ],
        cwd,
        Duration::from_secs(5),
    );
    if !url.ok {
        return PublishState::Inactive(PublishBlocker::NotGitHub);
    }
    let remote_url = url.stdout.trim().to_string();
    let Some(slug_hint) = github_slug_from_remote(&remote_url) else {
        return PublishState::Inactive(PublishBlocker::NotGitHub);
    };

    let version = bounded_output(
        "gh",
        &["--version".to_string()],
        cwd,
        Duration::from_secs(5),
    );
    if version.spawn_failed {
        return PublishState::Inactive(PublishBlocker::GhMissing);
    }

    let auth = bounded_output(
        "gh",
        &["auth".to_string(), "status".to_string()],
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    if !auth.ok {
        return PublishState::Inactive(PublishBlocker::GhUnauthenticated);
    }

    let view = bounded_output(
        "gh",
        &[
            "repo".to_string(),
            "view".to_string(),
            "--json".to_string(),
            "nameWithOwner,isFork,defaultBranchRef".to_string(),
        ],
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    if !view.ok {
        return PublishState::Inactive(PublishBlocker::RepoUnreadable(first_error_line(
            &view.stderr,
        )));
    }
    let parsed = serde_json::from_str::<serde_json::Value>(view.stdout.trim()).unwrap_or_default();
    let name_with_owner = parsed
        .get("nameWithOwner")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or(slug_hint);
    let is_fork = parsed
        .get("isFork")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let default_branch = parsed
        .get("defaultBranchRef")
        .and_then(|value| value.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("main")
        .to_string();

    // The extension list is local and cheap; its absence costs stacking only, so it
    // must not block the whole capability.
    let extensions = bounded_output(
        "gh",
        &["extension".to_string(), "list".to_string()],
        cwd,
        Duration::from_secs(10),
    );
    let has_gh_stack = extensions.ok && extensions.stdout.contains(GH_STACK_EXTENSION);

    PublishState::Active(PublishCapability {
        remote: remote.to_string(),
        remote_url,
        name_with_owner,
        default_branch,
        is_fork,
        has_gh_stack,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// One row's identity as far as publishing is concerned. Extracted from the
/// `AgentRun` before any command runs so the execution helpers stay independent of
/// `App` and can be exercised without a dashboard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishSubject {
    pub(crate) run_id: String,
    pub(crate) node_id: Option<String>,
    pub(crate) change_id: String,
    pub(crate) branch: String,
    pub(crate) title: String,
    pub(crate) body: String,
}

pub(crate) fn publish_subject_for(run: &AgentRun, default_branch: &str) -> Option<PublishSubject> {
    let change_id = run
        .jj_change_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let label = if run.task_summary.trim().is_empty() {
        run.task.as_str()
    } else {
        run.task_summary.as_str()
    };
    Some(PublishSubject {
        run_id: run.id.clone(),
        node_id: run.node_id.clone(),
        change_id,
        branch: publish_branch_name(&run.id, label, default_branch),
        title: publish_pr_title(&run.task_summary, &run.task),
        body: publish_pr_body(&run.id, &run.task, run.done_summary.as_deref()),
    })
}

/// Point a bookmark at the subject's change and export it to git. Publishing NEVER
/// touches the default branch (decision 5), and this is the seam where that is
/// enforced: a subject whose branch resolved to trunk is refused outright rather
/// than pushed.
pub(crate) fn stage_publish_branch(
    cwd: &Path,
    subject: &PublishSubject,
    default_branch: &str,
) -> std::result::Result<(), String> {
    if subject.branch == default_branch {
        return Err(format!(
            "refusing to publish onto the default branch ({default_branch})"
        ));
    }
    let set = bounded_output(
        "jj",
        &jj_bookmark_set_argv(&subject.branch, &subject.change_id),
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    if !set.ok {
        return Err(format!(
            "jj could not point bookmark {} at {} ({})",
            subject.branch,
            subject.change_id,
            first_error_line(&set.stderr)
        ));
    }
    let export = bounded_output("jj", &jj_git_export_argv(), cwd, PUBLISH_COMMAND_TIMEOUT);
    if !export.ok {
        return Err(format!(
            "jj git export failed ({}); the branch exists in jj but not in git, so gh cannot see it",
            first_error_line(&export.stderr)
        ));
    }
    Ok(())
}

/// Push one branch and open its draft PR. Returns the PR number and URL.
pub(crate) fn publish_one_pr(
    cwd: &Path,
    capability: &PublishCapability,
    subject: &PublishSubject,
    base: &str,
) -> std::result::Result<(u64, String), String> {
    stage_publish_branch(cwd, subject, &capability.default_branch)?;
    let push = bounded_output(
        "jj",
        &jj_git_push_argv(&capability.remote, &subject.branch),
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    if !push.ok {
        let detail = first_error_line(&push.stderr);
        return Err(if push.timed_out {
            format!(
                "push of {} to {} timed out; nothing was opened",
                subject.branch, capability.remote
            )
        } else if detail.is_empty() {
            format!(
                "{} could not be pushed to {}; nothing was opened",
                subject.branch, capability.remote
            )
        } else {
            format!("push of {} rejected: {detail}", subject.branch)
        });
    }
    let create = bounded_output(
        "gh",
        &gh_pr_create_argv(base, &subject.branch, &subject.title, &subject.body),
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    if !create.ok {
        return Err(format!(
            "{} was pushed but gh pr create failed: {}",
            subject.branch,
            first_error_line(&create.stderr)
        ));
    }
    // The push landed and gh exited 0; if we still cannot read a PR number, say so
    // rather than reporting a PR that we cannot name.
    parse_pr_url(&create.stdout).ok_or_else(|| {
        format!(
            "gh pr create for {} reported success but printed no PR url",
            subject.branch
        )
    })
}

/// Publish a linear chain as a gh-stack stack: stage every branch, adopt them into
/// a stack bottom-to-top, push atomically, open drafts, then give each PR the
/// node's real title (submit's non-interactive titles are auto-generated).
pub(crate) fn publish_stack(
    cwd: &Path,
    capability: &PublishCapability,
    subjects: &[PublishSubject],
) -> std::result::Result<Vec<StackBranchState>, String> {
    for subject in subjects {
        stage_publish_branch(cwd, subject, &capability.default_branch)?;
    }
    let branches: Vec<String> = subjects
        .iter()
        .map(|subject| subject.branch.clone())
        .collect();
    let init = bounded_output(
        "gh",
        &gh_stack_init_argv(&capability.default_branch, &branches),
        cwd,
        PUBLISH_STACK_TIMEOUT,
    );
    if !init.ok {
        return Err(format!(
            "gh stack init failed ({}); nothing was pushed",
            first_error_line(&init.stderr)
        ));
    }
    // sync, never push: push is documented as non-atomic and would leave a partly
    // updated stack behind on a rejection.
    let sync = bounded_output(
        "gh",
        &gh_stack_sync_argv(),
        cwd,
        PUBLISH_STACK_TIMEOUT,
    );
    if !sync.ok {
        return Err(classify_stack_sync_failure(&sync.stderr));
    }
    let submit = bounded_output(
        "gh",
        &gh_stack_submit_argv(),
        cwd,
        PUBLISH_STACK_TIMEOUT,
    );
    if !submit.ok {
        return Err(format!(
            "branches were pushed but gh stack submit failed: {}",
            first_error_line(&submit.stderr)
        ));
    }
    let view = bounded_output(
        "gh",
        &gh_stack_view_json_argv(),
        cwd,
        PUBLISH_COMMAND_TIMEOUT,
    );
    let mut states = if view.ok {
        parse_stack_view_json(&view.stdout)
    } else {
        Vec::new()
    };
    // Retitle each PR from its node's goal. gh stack submit had no title to work
    // with, so without this the stack reads as a column of auto-generated summaries.
    for state in &mut states {
        let Some(number) = state.number else { continue };
        let Some(subject) = subjects
            .iter()
            .find(|subject| subject.branch == state.branch)
        else {
            continue;
        };
        let edit = bounded_output(
            "gh",
            &gh_pr_edit_argv(number, &subject.title, &subject.body),
            cwd,
            PUBLISH_COMMAND_TIMEOUT,
        );
        // A failed retitle is cosmetic: the PR exists and is correct. Do not fail
        // the whole publish over it.
        let _ = edit;
    }
    Ok(states)
}

// ---------------------------------------------------------------------------
// Dashboard wiring
// ---------------------------------------------------------------------------

/// What `m` on a given row does. There are exactly three answers and they are
/// mutually exclusive; a row is never both merged locally and published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MergeRoute {
    /// Publishing is inactive for this repo: merge into the local checkout,
    /// exactly as Rudder has always done.
    LocalMerge,
    /// Publishing is active but this repo has never pushed. Ask, naming the
    /// remote and branch, before the first one.
    ConfirmFirstPublish,
    /// Publishing is active and already accepted for this remote: go.
    Publish,
    /// The first probe has not landed yet. Deliberately NOT `LocalMerge`: merging
    /// into the user's checkout because we had not finished asking whether this
    /// repo publishes would take the wrong road for a second or two after launch,
    /// and the row it took it for cannot be un-taken by a later answer. The probe
    /// is bounded on every call it makes, so this state always resolves.
    Checking,
}

impl App {
    /// The capability, if publishing is on for this repo.
    pub(crate) fn publish_capability(&self) -> Option<&PublishCapability> {
        match &self.publish {
            PublishState::Active(capability) => Some(capability),
            _ => None,
        }
    }

    /// Has this repo's user already agreed to Rudder pushing to this remote?
    pub(crate) fn publish_accepted(&self) -> bool {
        self.publish_capability()
            .is_some_and(|capability| publish_accepted_for(&self.cwd, &capability.remote_url))
    }

    /// Drain the probe result and re-arm the cadence. Detection spawns up to four
    /// processes, two of which hit the network, so it gets the AGENTS.md 12.8
    /// treatment: double the gap while the answer holds still, snap back the moment
    /// it changes.
    pub(crate) fn maybe_refresh_publish_capability(&mut self) {
        if let Some(rx) = self.publish_probe_rx.take() {
            match rx.try_recv() {
                Ok(state) => {
                    self.publish_probe_interval = if self.publish == state {
                        backed_off_interval(self.publish_probe_interval, PUBLISH_PROBE_MAX_INTERVAL)
                    } else {
                        PUBLISH_PROBE_BASE_INTERVAL
                    };
                    if self.publish != state {
                        self.publish = state;
                        self.dirty = true;
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.publish_probe_rx = Some(rx);
                    return;
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        let due = match self.last_publish_probe {
            None => true,
            Some(at) => at.elapsed() >= self.publish_probe_interval,
        };
        if !due {
            return;
        }
        self.last_publish_probe = Some(Instant::now());
        // Tests never shell out to real GitHub; they set `App::publish` directly.
        #[cfg(test)]
        {
            return;
        }
        #[cfg(not(test))]
        {
            let cwd = self.cwd.clone();
            let (tx, rx) = mpsc::channel();
            self.publish_probe_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(probe_publish_capability(&cwd));
            });
        }
    }

    /// Re-derive each published row's PR state. One `gh pr list` covers every row
    /// (a per-row `gh pr view` would spawn a network call per published agent), on
    /// a background thread and a backed-off cadence.
    pub(crate) fn maybe_refresh_publish_pr_state(&mut self) {
        if let Some(rx) = self.publish_pr_state_rx.take() {
            match rx.try_recv() {
                Ok(states) => {
                    let mut changed = false;
                    for run in self.agents.iter_mut() {
                        let Some(number) = run.publish.number else {
                            continue;
                        };
                        let next = states.get(&number).cloned();
                        if run.publish.state != next {
                            run.publish.state = next;
                            changed = true;
                        }
                    }
                    self.publish_pr_state_interval = if changed {
                        PUBLISH_PR_STATE_BASE_INTERVAL
                    } else {
                        backed_off_interval(
                            self.publish_pr_state_interval,
                            PUBLISH_PR_STATE_MAX_INTERVAL,
                        )
                    };
                    if changed {
                        self.dirty = true;
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.publish_pr_state_rx = Some(rx);
                    return;
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        if self.publish_capability().is_none() {
            return;
        }
        if !self.agents.iter().any(|run| run.publish.is_published()) {
            return;
        }
        let due = match self.last_publish_pr_state {
            None => true,
            Some(at) => at.elapsed() >= self.publish_pr_state_interval,
        };
        if !due {
            return;
        }
        self.last_publish_pr_state = Some(Instant::now());
        #[cfg(test)]
        {
            return;
        }
        #[cfg(not(test))]
        {
            let cwd = self.cwd.clone();
            let (tx, rx) = mpsc::channel();
            self.publish_pr_state_rx = Some(rx);
            thread::spawn(move || {
                let output = bounded_output("gh", &gh_pr_list_argv(), &cwd, PUBLISH_COMMAND_TIMEOUT);
                let states = if output.ok {
                    parse_pr_states(&output.stdout)
                } else {
                    HashMap::new()
                };
                let _ = tx.send(states);
            });
        }
    }

    /// Does pressing `m` on this row publish, or merge locally? Exactly one of the
    /// two, never both — the whole point of the publishing design.
    pub(crate) fn row_publishes_instead_of_merging(&self, index: usize) -> bool {
        if self.publish_capability().is_none() {
            return false;
        }
        self.agents
            .get(index)
            .is_some_and(|run| run.jj_change_id.is_some())
    }

    /// The single place the fork in the road is decided. Keeping it as one
    /// function (rather than a condition re-derived at each call site) is what
    /// makes "exactly one road to main, never both" checkable instead of a rule
    /// everyone has to remember.
    pub(crate) fn merge_route_for(&self, index: usize) -> MergeRoute {
        // A row with no jj change has nothing to publish either way, so it takes
        // the local road regardless of what the probe eventually says.
        let publishable = self
            .agents
            .get(index)
            .is_some_and(|run| run.jj_change_id.is_some());
        if publishable && self.publish == PublishState::Unknown {
            return MergeRoute::Checking;
        }
        if !self.row_publishes_instead_of_merging(index) {
            return MergeRoute::LocalMerge;
        }
        if self.publish_first_time_detail(index).is_some() {
            return MergeRoute::ConfirmFirstPublish;
        }
        MergeRoute::Publish
    }

    /// The confirmation shown before the FIRST push in a repo, naming the exact
    /// remote and branch. Returns `None` once the user has accepted for this remote.
    pub(crate) fn publish_first_time_detail(&self, index: usize) -> Option<String> {
        if self.publish_accepted() {
            return None;
        }
        let capability = self.publish_capability()?;
        let run = self.agents.get(index)?;
        let subject = publish_subject_for(run, &capability.default_branch)?;
        Some(format!(
            "Rudder has never pushed to a remote. This pushes branch {} to {} ({}) and opens a draft PR against {}.",
            subject.branch, capability.remote, capability.remote_url, capability.default_branch
        ))
    }

    /// Publish the selected row. A standalone row (no `node_id`) is one PR; a plan
    /// node carries its whole plan, because a node's PR is not reviewable without
    /// the stack it sits in.
    pub(crate) fn publish_agent_at(&mut self, index: usize) {
        let Some(capability) = self.publish_capability().cloned() else {
            // Name the ACTUAL problem. "publishing is not active" tells someone who
            // expected a PR nothing about whether to log in, install gh, or stop
            // expecting one.
            self.notice = Some(match &self.publish {
                PublishState::Inactive(blocker) => blocker.explain(),
                _ => "still checking whether this repo can publish; press m again in a moment"
                    .to_string(),
            });
            return;
        };
        // Accepting is what the modal was for; record it before the first push so a
        // failed publish does not re-ask on every retry.
        if let Err(error) = record_publish_acceptance(&self.cwd, &capability.remote_url) {
            self.notice = Some(format!("could not record publish consent: {error}"));
            return;
        }
        let plan_id = self
            .agents
            .get(index)
            .and_then(|run| run.node_id.as_ref().and(run.plan_id.clone()));
        match plan_id {
            Some(plan_id) => self.publish_plan(&plan_id, &capability),
            None => self.publish_standalone_at(index, &capability),
        }
    }

    /// B1: one self-contained row, one draft PR against the default branch.
    fn publish_standalone_at(&mut self, index: usize, capability: &PublishCapability) {
        let Some(run) = self.agents.get(index) else {
            self.notice = Some("no agent selected".to_string());
            return;
        };
        let Some(subject) = publish_subject_for(run, &capability.default_branch) else {
            self.notice =
                Some("this row has no jj change to publish; it predates the workspace model".to_string());
            return;
        };
        match publish_one_pr(
            &self.cwd,
            capability,
            &subject,
            &capability.default_branch,
        ) {
            Ok((number, url)) => {
                self.record_publish_result(index, &subject.branch, Some(number), Some(url.clone()));
                self.notice = Some(format!("PR #{number} opened as a draft · {url}"));
                self.record_activity(format!(
                    "published run {} as PR #{number} on branch {}",
                    subject.run_id, subject.branch
                ));
            }
            Err(error) => {
                // Never report a PR that was not opened. The branch may or may not
                // have been pushed; the message says which.
                self.notice = Some(format!("publish failed: {error}"));
            }
        }
    }

    /// B2: a plan's DAG, decomposed. Chains stack, joins and parallel work do not.
    fn publish_plan(&mut self, plan_id: &str, capability: &PublishCapability) {
        let nodes: Vec<(String, Vec<String>)> = self
            .agents
            .iter()
            .filter(|run| run.plan_id.as_deref() == Some(plan_id))
            .filter(|run| run.node_id.is_some() && run.jj_change_id.is_some())
            .filter_map(|run| run.node_id.clone().map(|id| (id, run.deps.clone())))
            .collect();
        if nodes.is_empty() {
            self.notice = Some("this plan has no workspaces to publish".to_string());
            return;
        }
        let units = decompose_plan_for_publish(&nodes);
        // A fork remote or a missing extension cannot stack. Degrade to independent
        // PRs and SAY SO — silently producing a different shape would leave the user
        // to discover from GitHub that nothing is stacked.
        let stack_blocker = capability.stack_blocker();
        let units = if stack_blocker.is_some() {
            flatten_units_to_independent(&units)
        } else {
            units
        };

        let mut opened = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for unit in &units {
            let subjects: Vec<(usize, PublishSubject)> = unit
                .ids
                .iter()
                .filter_map(|node_id| {
                    let index = self.agents.iter().position(|run| {
                        run.plan_id.as_deref() == Some(plan_id)
                            && run.node_id.as_deref() == Some(node_id.as_str())
                    })?;
                    let subject =
                        publish_subject_for(&self.agents[index], &capability.default_branch)?;
                    Some((index, subject))
                })
                .collect();
            if subjects.is_empty() {
                continue;
            }
            match unit.kind {
                PublishUnitKind::Stack => {
                    let flat: Vec<PublishSubject> =
                        subjects.iter().map(|(_, s)| s.clone()).collect();
                    match publish_stack(&self.cwd, capability, &flat) {
                        Ok(states) => {
                            for (index, subject) in &subjects {
                                let state = states
                                    .iter()
                                    .find(|state| state.branch == subject.branch);
                                let number = state.and_then(|state| state.number);
                                if number.is_some() {
                                    opened += 1;
                                }
                                let url = state.and_then(|state| state.url.clone());
                                self.record_publish_result(*index, &subject.branch, number, url);
                            }
                            if states.iter().any(|state| state.needs_rebase) {
                                failures.push(
                                    "a branch in the stack needs a rebase; run `gh stack rebase`"
                                        .to_string(),
                                );
                            }
                        }
                        Err(error) => failures.push(error),
                    }
                }
                PublishUnitKind::Independent | PublishUnitKind::Join => {
                    for (index, subject) in &subjects {
                        match publish_one_pr(
                            &self.cwd,
                            capability,
                            subject,
                            &capability.default_branch,
                        ) {
                            Ok((number, url)) => {
                                opened += 1;
                                self.record_publish_result(
                                    *index,
                                    &subject.branch,
                                    Some(number),
                                    Some(url),
                                );
                            }
                            Err(error) => failures.push(error),
                        }
                    }
                }
            }
        }

        let stacks = units
            .iter()
            .filter(|unit| unit.kind == PublishUnitKind::Stack)
            .count();
        let joins = units
            .iter()
            .filter(|unit| unit.kind == PublishUnitKind::Join)
            .count();
        let mut parts = vec![format!(
            "{opened} draft PR{}",
            if opened == 1 { "" } else { "s" }
        )];
        if stacks > 0 {
            parts.push(format!("{stacks} stack{}", if stacks == 1 { "" } else { "s" }));
        }
        if joins > 0 {
            parts.push(format!(
                "{joins} join node{} target {} and wait for their parents",
                if joins == 1 { "" } else { "s" },
                capability.default_branch
            ));
        }
        if let Some(blocker) = stack_blocker {
            parts.push(format!("not stacked: {blocker}"));
        }
        if !failures.is_empty() {
            parts.push(failures.join("; "));
        }
        self.notice = Some(parts.join(" · "));
        self.record_activity(format!(
            "published plan {plan_id}: {opened} PRs, {stacks} stacks, {joins} joins"
        ));
        self.dirty = true;
    }

    /// Persist what actually happened. Only the identity is stored; the PR's state
    /// is left for the refresh to derive.
    fn record_publish_result(
        &mut self,
        index: usize,
        branch: &str,
        number: Option<u64>,
        url: Option<String>,
    ) {
        let cwd = self.cwd.clone();
        let Some(run) = self.agents.get_mut(index) else {
            return;
        };
        run.publish.branch = Some(branch.to_string());
        if number.is_some() {
            run.publish.number = number;
        }
        if url.is_some() {
            run.publish.url = url;
        }
        // Publishing IS the push, so the integration evidence should say so rather
        // than waiting for `refresh_remote_integration_state` to notice a bookmark
        // it has no local merge commit for.
        run.integration.bookmark = Some(branch.to_string());
        if number.is_some() {
            run.integration.pushed = true;
            run.integration.phase = IntegrationPhase::Pushed;
        }
        let _ = save_native_run_record(&cwd, run);
        self.dirty = true;
    }
}
