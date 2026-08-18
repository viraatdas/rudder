# rudder

<p>
  <a href="https://rudder.viraat.dev">
    <img src="https://rudder.viraat.dev/favicon.svg" width="36" height="36" alt="Rudder logo" />
  </a>
</p>

[![TUI e2e](https://github.com/viraatdas/rudder/actions/workflows/tui.yml/badge.svg)](https://github.com/viraatdas/rudder/actions/workflows/tui.yml)
[![npm version](https://img.shields.io/npm/v/@viraatdas/rudder.svg)](https://www.npmjs.com/package/@viraatdas/rudder)
[![Node >=20](https://img.shields.io/badge/node-%3E%3D20-43853d.svg)](https://nodejs.org/)
[![Website](https://img.shields.io/badge/site-rudder.viraat.dev-111111.svg)](https://rudder.viraat.dev)

Rudder runs coding agents the way they should be used: several at once,
isolated, reviewable, and easy to merge. It opens a native three-pane dashboard,
gives isolated tasks their own jj workspace, and runs real Claude Code or Codex
processes in the worker pane. Plain requests instead get one end-to-end owner in
the main checkout, so implementation, release, and deployment do not fall between
separate agents unless you explicitly ask for a DAG.

> **AI agents and contributors:** this README is for using Rudder. If you are
> an AI agent (or a human) working on the Rudder codebase itself, read
> [`AGENTS.md`](./AGENTS.md) first; it is the engineering reference and the
> source of truth for architecture, conventions, and release rules.

## Install

```bash
npm install -g @viraatdas/rudder@latest
rudder
```

After the global install, every user-facing `rudder` launch checks npm and
auto-updates itself before running when a newer version is available. Set
`RUDDER_DISABLE_AUTO_UPDATE=1` to opt out. Run without a global install using
`npx @viraatdas/rudder@latest`.

## Requirements

- Node.js 20 or newer
- Git
- Claude Code and/or Codex installed and logged in
- macOS, Linux, or another Unix-like terminal

Check your setup at any time:

```bash
rudder doctor
```

## Quick start

```bash
rudder
```

With no arguments, `rudder` opens the dashboard. Type in the bottom input and
press `Enter` to give the request to a single end-to-end main-checkout agent.
That agent owns implementation, relevant tests, and any push/deploy/install work
the request explicitly authorizes. Use `/plan <task>` when you want a reviewed
multi-agent DAG, `/run <task>` for exactly one isolated mergeable worker with no
DAG, or `/ask <text>` for a separate one-off main-checkout conversation.

If a task needs shared local context like API tokens, private URLs, account ids,
or environment values, save it with `/share <text>` in the task input. Rudder
also captures obvious token-looking lines like `APIFY_TOKEN=...` from task-bar
messages. The text is written to `RUDDER_SHARED.md`, which is gitignored and
mirrored into worker workspaces; all agents are explicitly told to read it when
present.

You can also start a task directly from the shell:

```bash
rudder "fix the failing tests"
rudder claude "fix the auth redirect bug"
rudder codex --model gpt-5.5 "refactor the parser"
```

## Onboarding and auth

```bash
rudder onboard
```

Onboarding uses the auth you already have, so you usually do not need API keys:

- Claude Code auth from the macOS Keychain or `~/.claude/.credentials.json`
- Codex auth from `~/.codex/auth.json`
- `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` if you prefer keys

If auth is missing you can skip it and set up a backend later. Config is written
to `~/.rudder/config.json`.

## The dashboard

```text
┌───────────────┬────────────────────────────────────────────┐
│ agents        │ worker                                       │
│ task list     │ live Claude Code or Codex terminal           │
│ status/model  │ DAG-over-orchestrator, scrollback, review    │
├───────────────┴────────────────────────────────────────────┤
│ task input: main owner by default, /plan DAG, /run isolated   │
└──────────────────────────────────────────────────────────────┘
```

- **agents** (left): one row per task with its backend, model, effort, and status.
- **worker** (right): the real Claude Code or Codex terminal. When it is focused,
  your keystrokes go straight to the agent, so its prompts, slash commands,
  selection, and `Tab` all work normally.
- **orchestrator** (right, when selected): a Claude Code PTY with the live DAG
  rendered above it. For planned work, use it to inspect/refine the plan while
  the bottom task input remains the primary place to type new requests.
- **task** (bottom): the entry point for fresh requests, slash commands, plan
  refinements, and approval with empty `Enter`.

Mouse wheel and trackpad scroll the pane under the pointer. Over the worker or
review pane they scroll Rudder's captured scrollback.

### Web board

Press `o` in the agents pane (or type `/web`) to open the live project board in
your browser. The board is a second control surface, not just a monitor: open a
task to send an update while it runs, or request changes from Review to resume
the same agent in the same workspace. The task's update thread records your
directions alongside the latest worker activity. `+ Task` follows the same
end-to-end main-owner path as typing in the dashboard task pane; use `/plan` in
the native dashboard for an explicit DAG.

The board also works on its own with `rudder board`: in that mode updates
redirect or resume the detached worker directly and move revised work back
through Review before it can merge.

## Keyboard shortcuts

**Direct (work from any pane):**

| Key | Action |
| --- | --- |
| `Option-1` / `Option-2` / `Option-3` | Focus the agents, worker, or task pane |
| `Option-[` / `Option-]` | Step to the previous / next agent, staying in the pane you are in |
| `Option-v` | Toggle the selected agent's review view |
| `Cmd-C` | Copy the active Rudder selection |
| `Ctrl-C` | Quit (asks to confirm if agents are still running) |

`Option-1/2/3` and `Option-[`/`Option-]` work out of the box on macOS terminals,
whether or not "Use Option as Meta" is enabled. `Cmd-[` / `Cmd-]` also step agents
in terminals that forward Cmd to the app (Ghostty and other terminals that speak
the kitty keyboard protocol, and only if the terminal has not bound those keys
itself). `Option-[`/`Option-]` are the ones that always work.

Stepping agents is the shortcut worth learning: the worker pane hands every
keystroke to the agent's own TUI, so `j`/`k` only select from the agents list —
these move between agents without leaving the conversation you are reading.

**Leader: press `Ctrl-W`, then one key.** A reliable way to run a dashboard
command, even while typing inside the worker pane:

| Then press | Action |
| --- | --- |
| `1` / `2` / `3` | Focus agents / worker / task |
| `v` | Toggle review |
| `m` | Merge the selected completed workspace |
| `M` | Merge all completed workspaces |
| `R` | Review all completed workspaces (Codex review-all agent) |
| `r` | Rename the selected agent |
| `b` | Branch the selected agent's chat into a new worker |
| `u` | Undo the selected row's merge (restores the jj operation it ran as) |
| `d` | Delete the selected agent and its workspace |
| `j` / `k` | Move the agent selection |
| `q` | Quit |
| `Esc` | Cancel the leader |

`Ctrl-G` toggles the same command set as a sticky "nav mode" (`Esc` exits) if you
prefer a held mode over the one-shot leader.

**In the worker pane:** keystrokes go to the agent. `Tab` / `Shift+Tab` are
forwarded to it, `Shift+Enter` inserts a newline, and `PageUp` / `PageDown`
scroll the pane.

**In the agents pane:** `j` / `k` or arrows move the selection, `Enter` focuses
the worker, `m` / `M` / `R` / `r` / `d` act on the selection, `x` stops a running
agent (keeping its workspace), `c` clears all merged agents from the list (press
twice to confirm), `g` toggles the nested DAG view, and `o` opens the web board.

**Finished work collapses.** A long session ends with dozens of merged and failed
rows, which bury the two or three still moving, so `done` and `closed` each render
as a single row with a count and a chevron (`done 27 ›`). `j` / `k` walk onto that
row and `Enter` opens the drawer: the finished runs list in the worker pane with
the highlighted one's result below it, and `Esc` backs out. `review` stays inline
up to five rows — merge-ready work should be one keystroke away — and collapses the
same way past that, because a 25-row review section is not scannable either.

## Commands and Orchestrator Skills

Type these in the bottom task input. When an interactive Claude orchestrator is
running, Rudder also exposes matching project skills; the orchestrator can write
`RUDDER_*` control markers in `RUDDER.md` and Rudder consumes them.

| Command | Action |
| --- | --- |
| `/model` | Pick provider, then model, then effort |
| `/fast` | Fast mode for new agents: flagship model at low effort (Claude opus / Codex gpt-5.5); `/model` switches back |
| `/plan <text>` | Start the orchestrator / DAG planner |
| `/run <task>` | Start one isolated mergeable worker, with no DAG |
| `/ask <text>` | Start a one-off conversational agent in the main checkout |
| `/share <text>` | Save gitignored shared context for all agents in `RUDDER_SHARED.md` |
| `/main` or `/m` | Start a new main-branch agent |
| `/review-all` | Combine completed workspaces and start a Codex review-all agent |
| `/merge-all` | Merge all completed workspaces |
| `/verify` | Re-run the final repository checks after every DAG node is integrated |
| `/color terminal\|paper` | Choose the native dashboard color mode; `terminal` uses your terminal foreground/background, `paper` restores the white canvas |
| `/login` | Browser login for Rudder Cloud |
| `/cloud` | Onload the current workspace or start a fresh cloud worker |
| `/cloud list` | List cloud workers |
| `/help` | Show the keybinding + command cheat sheet |

Finding a DAG task's worker: every plan node id (`n0`, `n1`, …) appears on the
orchestrator DAG row, on the matching worker's row in the agents pane (`run n1`),
and in the worker pane title, so the three panes cross-reference by id. A finished
Planned DAG workers integrate automatically after completion; manually started
workers remain ready for `m`/`M`. Only locally merged nodes unblock dependents.
Merged rows show the target branch, jj/Git revision, and whether the remote contains it.

## Models

Use `/model [provider] [model] [effort]` in the task input. Claude offers the
same current aliases as Claude Code (`opus`, `fable`, `sonnet`, and `haiku`);
the picker labels context windows such as Opus's 1M context without inventing a
different model name. Codex offers `gpt-5.5`, `gpt-5.4-codex`, and other
discovered models. `auto` effort means Rudder passes no override.

`opencode` is available as a third backend: `/model opencode <provider/model>`
(its ids are provider-qualified, e.g. `anthropic/claude-sonnet-4-5`), and the
picker lists whatever `opencode models` reports for your authenticated providers.
opencode has no reasoning-effort flag, so Rudder offers no effort choice for it
rather than a dead control, and `/fast` says so instead of pretending. Turn-end is
reported by a small plugin Rudder generates per run (loaded through
`OPENCODE_CONFIG`, never written into your workspace), so opencode rows reach
Review deterministically like the other two. Token/cost accounting for opencode
rows is not wired up yet and reports zero.

Your last provider, model, and effort are saved in `~/.rudder/config.json` and
reused next time. Rudder refreshes model metadata from
`https://models.dev/api.json` and falls back to local caches when offline.
The native Codex picker also reads `~/.codex/models_cache.json`, so account-specific
models such as the GPT-5.6 Sol/Terra/Luna family appear as soon as Codex exposes them.
Implementation, review, resume, and shipping Codex sessions keep Codex plugins and
Computer Use enabled, matching a direct Codex session. Read-only planner sessions
disable both capabilities so planning cannot cause external side effects.

## One-Off and Planning

Type a fresh request in the bottom task input. When no plan is active, Rudder
starts or continues one main-checkout agent as the end-to-end owner. It can inspect,
edit, test, commit, release, and deploy according to the request and the repository's
instructions. Rudder does not silently split a plain request across a planner and
workers, which keeps final delivery ownership unambiguous.

Use `/ask <text>` for a one-off conversational agent in the main checkout, with
no DAG and no merge step. Use `/run <task>` for exactly one isolated worker that
lands in Review and merges back with `m` or `/merge-all`. Use `/run main <task>`
to add another agent in the main checkout itself — several may run there at once
(they edit the same tree, so they can overwrite each other; rudder says so in the
activity log rather than stopping you). Use `/plan <text>` when
you want the orchestrator / DAG path: the read-only planner decomposes the task,
then separate isolated workers implement its nodes.

For planned work, Rudder runs a dedicated Claude Code orchestrator PTY with a DAG
pane above it. The flow stays inside Rudder: the orchestrator researches
read-only, writes the DAG to `RUDDER.md`, and separate workers do the
implementation.

1. **It asks when it needs to.** The planner inspects the repo first. If something
   material is missing (scope, approach, a key decision), it asks before the first
   DAG, shown as a numbered prompt; a clear request goes straight to a DAG with the
   planner's assumptions listed under it:

   ```
   ❓ The planner needs your input
     1. Which time range: last 4 weeks, 6 months, or all time?
     2. Reuse the existing module contract, or rebuild from scratch?

   ↳ answer in the task input (e.g. "1: ..., 2: ...")
   ```

   Type your answer in the task input; Rudder resumes the same planning
   conversation with your answer. The focused orchestrator pane can also accept
   a follow-up when it is showing the planner chat.
2. **It lays out a DAG.** When it is ready it shows the task DAG (a tree of worker tasks
   with their dependencies) above the orchestrator terminal. Type feedback in the
   task input to refine it, or press empty `Enter` to approve/launch.
3. **The fleet runs.** The scheduler drains the DAG (todo → in-progress → review → done) as
   dependencies merge, each task in its own isolated jj workspace.

After launch, typing a new task folds it into the running DAG as a new node. When a worker
finishes, Rudder reads back what it did and what work it found remaining (reconstructing it
from the diff if the agent did not say), so the plan keeps growing on its own. Type a
sweeping change ("rewrite this in Rust instead") and the plan re-plans around the work
already done. The queued plan and workspaces survive a restart. Closing the dashboard
marks owned live PTYs `paused` and removes their dead process ids; select a paused row
and press `Enter` to resume it (reusing its backend session when available).

The orchestrator also handles worker operations in natural language. For example, tell it
"pause the current tasks and resume them on Codex gpt-5.6 at max effort." It resolves the
live node ids from `RUDDER.md`, pauses each matching worker without deleting its jj
workspace, and resumes it on the requested provider/model. A same-provider switch keeps
the agent conversation; a provider switch keeps the workspace and starts a fresh session
with a handoff to inspect and continue the existing diff.

## Continuing a conversation you already started

The chat you are already having in a terminal — a plain `claude`, `codex`, or
`opencode` session — holds the context you would otherwise retype: the files you
looked at, the decisions you made, the plan you agreed on. Rudder can pick that
conversation up and keep it going as an agent.

Two directions, same result:

- **From the dashboard:** type `/resume`. The palette lists this repository's
  recent conversations — Claude, Codex, and opencode in one list, newest first —
  showing what each was about, which model it was running, how long ago, and its
  session id. Pick one, optionally type the next step after it, and press Enter. It
  opens as an isolated worker that merges with `m` like any other.
- **From inside the chat:** run `rudder handoff "<next step>"`. That queues the
  conversation you are in; the dashboard picks it up within a second (start it with
  `rudder` if it is not open). `--here` continues in the main checkout instead of an
  isolated workspace, `--list` shows the conversations Rudder can see, and
  `--codex` / `--opencode` pick a different source CLI.

Handoffs always FORK the conversation. The chat in your terminal is untouched and
keeps working; the Rudder agent continues from a copy. The fork is told it is now
running in a different workspace, so it re-reads files instead of trusting its
memory of the checkout it came from.

`/handoff` still works as an alias for `/resume` (it is what the CLI half is
called). `b` on a worker row does the same thing for a conversation already inside
Rudder, and `/restore <claude|codex> <id>` continues a session IN PLACE (same
session, main checkout) instead of forking it into a worker.

## Worktrees and merging

**Who merges what:** planned `/plan` DAG nodes integrate into your checkout
**automatically** as soon as they finish — no per-node confirmation — so
dependent nodes unblock and the chain keeps flowing (their row shows
`done · auto-merging`, then `merged locally`; the final gate runs the repo's
checks after the last node lands). Manually started `/run` workers never
auto-merge: they wait in Review (`done · press m to merge`) until you merge
them yourself. There is no `/automerge` command or config flag — this split is
the behavior.

Planned and `/run` worker tasks run in their own jj workspaces under a sibling
`.rudder-workspaces` area, so parallel agents never edit the same checkout. Plain
main-owner and `/ask` agents intentionally run in the main checkout. Run records
live under `.rudder/runs/`. If you quit Rudder, live workers become `paused`, keep
their workspace/session metadata, and stay listed when you reopen the repo.

- Press `m` to merge the selected completed agent back into its branch.
- Press `M` (or `/merge-all`) to merge all completed agents.
- Both ask for a `y`/`n` confirmation first, and the prompt warns when your own
  uncommitted changes in the main checkout would ride along in the merge.

Clean merges become merge commits. If git reports conflicts, Rudder leaves the
conflicted state in place and can open an agent in the main checkout to help
resolve it.

Choose the merge behavior in `~/.rudder/config.json`:

```json
{ "mergeStrategy": "rebase" }
```

- `"merge"` (default): `git merge --no-ff`.
- `"rebase"`: rebase the workspace onto the latest base, then `git merge --ff-only`.

Command-line equivalents:

```bash
rudder merge <runId>
rudder sync <runId>
rudder cleanup
```

`rudder stop` records a durable cancellation request and terminates the native PTY process
group. Cancellation is terminal: a resulting signal exit cannot be relabeled as a failure.

## Shipping and delivery evidence

Rudder treats these as different facts: an agent finished, changes merged locally,
a revision was pushed, and a product was deployed. A task that explicitly asks to
deploy, publish, release, install, or launch remains `delivery proof needed` until
its completion report records the target, delivery kind, verification time, and at
least one real smoke check. Verified web URLs, installed macOS app paths, provider
deployment ids, revisions, and versions are persisted in `.rudder/runs/<id>/run.json`
and shown in the native dashboard, web board, and generated `RUDDER.md`.

Pushing a commit is never presented as proof that a host deployed it. Likewise,
building a macOS app is not the same as replacing and launching the installed app.
If delivery fails or needs credentials/user input, the row says `delivery blocked`
instead of incorrectly moving to deployed.

## Review

Press `v` on an agent to toggle a review of its workspace, showing the run's diff.
Press `v` or `Esc` to return to the worker.

Press `R` to review all completed workspaces as one bundle: Rudder builds an
aggregate branch and starts a Codex review-all agent over the combined diff. When
that row is done, press `m` on it to merge the reviewed bundle into your checkout.

## Rudder Cloud

Rudder Cloud is an optional hosted worker mode. The local dashboard stays your
control surface; you decide whether a task runs locally or is handed to the cloud.

```bash
rudder login                 # connect this machine to Rudder Cloud
rudder cloud                 # onload the current workspace or start a worker
rudder cloud list            # list cloud workers
rudder cloud logs <id>       # worker status
rudder cloud onload [runId]  # upload the current workspace (or one run)
rudder cloud workspace attach # migrate all live isolated agents into one cloud workspace
rudder sail <name>           # short alias for starting a cloud worker
```

Inside the dashboard, `/login` starts browser auth and `/cloud` opens a
confirmation pane: the default option onloads the current workspace (repo
snapshot plus selected auth/config) to a Fly worker; press Down to start a fresh
scratch worker instead. Completed cloud work returns through the same review and
merge path as local work.

Cloud comes in two shapes: a **sail** is an ephemeral, task-scoped worker (it goes away
when the task is done; idle sails pause and can resume), while a **workspace** is a
persistent, volume-backed dev environment you can come back to. Both restore the same
workspace snapshot.

You can also tell the live orchestrator: "take the current agents and run them on
Rudder Cloud." Rudder freezes the local worker PTYs first, then migrates every live
isolated workspace. Claude sessions resume from their transcript when available;
Codex and missing-session workers restart from a context-rich handoff over the existing
diff. The explicit fleet-migration snapshot includes all inherited environment variables
(except OS/Rudder control variables),
agent auth/config, shell and cloud CLI configuration, and project `.env*` files (including
nested package dotenv files) in each restored worker workspace.

Cloud workers use Fly Machines by default. To use your own server over SSH (Docker over
SSH; bring-your-own-compute can be stopped but not paused/resumed):

```bash
rudder cloud byoc <ssh-host>   # an entry from ~/.ssh/config, key auth + Docker
rudder cloud vm "task"         # run on that host
rudder cloud runtime [fly|byoc]
```

The CLI points at the hosted control plane `https://rudder-cloud-control.fly.dev`.
Set `RUDDER_CLOUD_URL` to use your own deployment. Rudder Cloud login is separate
from Claude Code and Codex login; provider auth stays in the official CLIs.

## Run management

```bash
rudder status
rudder runs
rudder watch <runId>
rudder logs <runId> --follow
rudder stop <runId>
rudder delete <runId>
rudder merge <runId>
rudder sync <runId>
rudder cleanup
```

## Troubleshooting

- Stale behavior after an upgrade: restart any already-running Rudder dashboards
  so no old `rudder-native` process lingers. New `rudder` launches auto-update
  unless `RUDDER_DISABLE_AUTO_UPDATE=1` is set.
- Sustained CPU/redraw load: version 2.12.9 and newer cache Codex plan-output scans,
  avoid reparsing unchanged planner JSON, throttle background PTY drains, and redraw
  only dirty frames. Upgrade and restart any dashboard launched by 2.12.8 or older.
- Trackpad scrolling: confirm your terminal sends scroll events with
  `rudder mouse-test parsed`. Set `RUDDER_WHEEL_SCROLL_ROWS=<n>` to change the
  scroll step, or `RUDDER_MOUSE_DEBUG=1` to inspect routing.

## Continual improvement loop

Rudder improves itself from its own telemetry. Every run already leaves a
record on disk (`.rudder/runs/`, event logs, verifier results, steer history,
token usage). The improvement loop, `rudder improve`, is a scheduled local
batch job (launchd on macOS, not a resident daemon) that:

1. **Collects** recent session telemetry across your registered projects,
   redacting secrets at the source.
2. **Mines** it into ranked friction findings (failed runs, user redirects,
   merge conflicts, verifier misses) using the advisor pattern: a Sonnet
   executor consults a Fable 5 advisor mid-generation, so most tokens bill at
   the executor rate.
3. **Proposes** fixes with headless agents in isolated git worktrees of the
   rudder repo, each briefed with a rich context pack (finding, evidence,
   surface map, prior failed attempts, repo conventions).
4. **Judges** each candidate with the repo's full test gates plus a
   three-lens adversarial LLM panel that fails closed.
5. **Ships** survivors automatically: rebase onto `origin/main`,
   `npm version patch`, push main + tag, and the normal tag-driven CI
   publishes the new version to npm. Later cycles verify the targeted metric
   actually improved and flag regressions for revert.

```bash
rudder improve run --dry-run     # see what it would do, propose nothing
rudder improve schedule install  # nightly cycle at 03:30 via launchd
rudder improve status            # shipped versions, metrics trend, ledger
```

Everything stays on your machine, spend is hard-capped per cycle
(`improve.budgetUsd`, default $5), autonomy is configurable
(`improve.autonomy`: `observe` | `propose` | `ship`), and `RUDDER_IMPROVE=0`
disables it. The full harness spec is in
[`docs/continual-improvement.md`](./docs/continual-improvement.md);
implementation details live in [`AGENTS.md`](./AGENTS.md) section 15.

## What Rudder derives rather than remembers

A status that can be checked is checked, on a slow tick, against whatever actually
knows: jj for "is this work in main", the process handle for "is this agent
alive", the filesystem for "does this workspace exist", each backend's own session
store for "which conversation is this". Anything that changed is written to
`.rudder/activity.jsonl` with the ids it touched — change id, bookmark, commit,
jj operation — so it can be lined up against `jj op log` afterwards.

This is why a merged row can say **`merged · NOT in main`**: the merge landed, and
then something rewrote history under it. Rudder used to keep claiming success.

## Telemetry and feedback

Rudder sends a small set of **anonymous usage events** so the parts people get
stuck on are visible: how many installs ever start an agent, how many ever merge,
which backend and model get picked, which errors fire most. It is on by default,
disclosed the first time you run anything, and off in one command:

```bash
rudder telemetry status
rudder telemetry off          # or RUDDER_TELEMETRY=0 in your shell
```

What is sent: the event name, rudder version, platform, backend/model, agent and
merge counts, slash-command *names*, and a hashed project id (a digest of the
repo path — it counts projects without naming them). What is never sent: your
prompts, code, diffs, file contents, paths, repo names, branch names, or session
ids. Events are anonymous (no person profile), keyed to a random id minted per
install. Implementation and the rules it enforces: `src/analytics.ts`.

**`/feedback <what broke>`** in the task pane (or `rudder feedback "..."`) sends a
report with the message plus what was on screen in structure: version, platform,
backend/model, how many agents were running, the last few notices and the last
error — with paths stripped. It lands in three places, in order of durability: a
local JSON copy under `.rudder/feedback/` (always, so nothing is lost offline), a
usage event, and a GitHub issue when `gh` is installed and authenticated. Add
`--no-issue` to keep it out of the public tracker.

## Building from source

```bash
git clone https://github.com/viraatdas/rudder.git
cd rudder
./setup.sh
```

`setup.sh` checks prerequisites (Node >=20, git, npm, Rust/`cargo`), installs
dependencies, builds (`tsc` + `cargo build --release`), typechecks, and smoke
tests the CLI. It is safe to re-run after pulling. For architecture and
implementation details, see [`AGENTS.md`](./AGENTS.md).

## License

MIT
