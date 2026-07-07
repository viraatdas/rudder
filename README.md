# rudder

<p>
  <a href="https://rudder.viraat.dev">
    <img src="https://rudder.viraat.dev/favicon.svg" width="36" height="36" alt="Rudder logo" />
  </a>
</p>

[![npm version](https://img.shields.io/npm/v/@viraatdas/rudder.svg)](https://www.npmjs.com/package/@viraatdas/rudder)
[![npm downloads](https://img.shields.io/npm/dm/@viraatdas/rudder.svg)](https://www.npmjs.com/package/@viraatdas/rudder)
[![Node >=20](https://img.shields.io/badge/node-%3E%3D20-43853d.svg)](https://nodejs.org/)
[![Website](https://img.shields.io/badge/site-rudder.viraat.dev-111111.svg)](https://rudder.viraat.dev)

Rudder runs coding agents the way they should be used: several at once,
isolated, reviewable, and easy to merge. It opens a native three-pane dashboard,
gives every task its own git worktree, and runs real Claude Code or Codex
processes in the worker pane.

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
press `Enter` to hand the request to the orchestrator. It can answer, inspect the
repo, or plan a DAG and run isolated workers. Use `/run <task>` when you want
exactly one isolated mergeable worker with no DAG. Use `/ask <text>` when you
want a one-off agent in the main checkout with no merge step.

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
│ task input: orchestrator by default, /run single, /ask direct │
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
orchestrator path as typing in the dashboard task pane.

The board also works on its own with `rudder board`: in that mode updates
redirect or resume the detached worker directly and move revised work back
through Review before it can merge.

## Keyboard shortcuts

**Direct (work from any pane):**

| Key | Action |
| --- | --- |
| `Option-1` / `Option-2` / `Option-3` | Focus the agents, worker, or task pane |
| `Option-v` | Toggle the selected agent's review view |
| `Cmd-C` | Copy the active Rudder selection |
| `Ctrl-C` | Quit (asks to confirm if agents are still running) |

`Option-1/2/3` work out of the box on macOS terminals, whether or not "Use Option
as Meta" is enabled.

**Leader: press `Ctrl-W`, then one key.** A reliable way to run a dashboard
command, even while typing inside the worker pane:

| Then press | Action |
| --- | --- |
| `1` / `2` / `3` | Focus agents / worker / task |
| `v` | Toggle review |
| `m` | Merge the selected completed worktree |
| `M` | Merge all completed worktrees |
| `R` | Review all completed worktrees (Codex review-all agent) |
| `r` | Rename the selected agent |
| `d` | Delete the selected agent and its worktree |
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
agent (keeping its worktree), `g` toggles the nested DAG view, and `o` opens the
web board.

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
| `/review-all` | Combine completed worktrees and start a Codex review-all agent |
| `/merge-all` | Merge all completed worktrees |
| `/automerge` | Toggle automatic clean merges (persists across sessions; shown in the agents pane header) |
| `/color terminal\|paper` | Choose the native dashboard color mode; `terminal` uses your terminal foreground/background, `paper` restores the white canvas |
| `/login` | Browser login for Rudder Cloud |
| `/cloud` | Onload the current workspace or start a fresh cloud worker |
| `/cloud list` | List cloud workers |
| `/help` | Show the keybinding + command cheat sheet |

Finding a DAG task's worker: every plan node id (`n0`, `n1`, …) appears on the
orchestrator DAG row, on the matching worker's row in the agents pane (`run n1`),
and in the worker pane title, so the three panes cross-reference by id. A finished
worker shows `done · needs merge` until it merges (with `m`, `M`, or auto-merge);
only merged nodes unblock their dependents.

## Models

Use `/model [provider] [model] [effort]` in the task input. Claude offers
aliases like `sonnet`, `opus`, `haiku` (and the `[1m]` long-context variants);
Codex offers `gpt-5.5`, `gpt-5.4-codex`, and other discovered models. `auto`
effort means Rudder passes no override.

Your last provider, model, and effort are saved in `~/.rudder/config.json` and
reused next time. Rudder refreshes model metadata from
`https://models.dev/api.json` and falls back to local caches when offline.

## One-Off and Planning

Type a fresh request in the bottom task input. When no plan is active, Rudder
starts the orchestrator, which can inspect the repo, plan the work, and spawn
isolated workers when needed.

Use `/ask <text>` for a one-off conversational agent in the main checkout, with
no DAG and no merge step. Use `/run <task>` for exactly one isolated worker that
lands in Review and merges back with `m` or `/merge-all`. Use `/plan <text>` when
you want to be explicit about the orchestrator / DAG path; it is the same route
as plain input.

For planned work, Rudder runs a dedicated Claude Code orchestrator PTY with a DAG
pane above it. The flow stays inside Rudder: the orchestrator researches
read-only, writes the DAG to `RUDDER.md`, and separate workers do the
implementation.

1. **It asks first.** The planner inspects the repo and then asks at least one
   clarifying or confirmation question before the first DAG, shown as a numbered prompt:

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
   dependencies merge, each task in its own isolated worktree.

After launch, typing a new task folds it into the running DAG as a new node. When a worker
finishes, Rudder reads back what it did and what work it found remaining (reconstructing it
from the diff if the agent did not say), so the plan keeps growing on its own. Type a
sweeping change ("rewrite this in Rust instead") and the plan re-plans around the work
already done. The queued plan survives a restart, so quitting mid-plan resumes where you
left off.

## Worktrees and merging

Planned worker tasks run in their own git worktrees under `.rudder-worktrees/`
inside your project (gitignored), so parallel DAG agents never edit the same
checkout and every Rudder path stays within the project boundary. One-off agents
are the exception: they intentionally run in the main checkout. Run records live
under `.rudder/runs/`. If you quit Rudder, live workers stop but the agents stay
listed the next time you open Rudder in that repo.

- Press `m` to merge the selected completed agent back into its branch.
- Press `M` to merge all completed agents. Rudder confirms first.

Clean merges become merge commits. If git reports conflicts, Rudder leaves the
conflicted state in place and can open an agent in the main checkout to help
resolve it.

Choose the merge behavior in `~/.rudder/config.json`:

```json
{ "mergeStrategy": "rebase" }
```

- `"merge"` (default): `git merge --no-ff`.
- `"rebase"`: rebase the worktree onto the latest base, then `git merge --ff-only`.

Command-line equivalents:

```bash
rudder merge <runId>
rudder sync <runId>
rudder cleanup
```

## Review

Press `v` on an agent to toggle a review of its worktree, showing the run's diff.
Press `v` or `Esc` to return to the worker.

Press `R` to review all completed worktrees as one bundle: Rudder builds an
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
3. **Proposes** fixes with headless agents in isolated worktrees of the
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
