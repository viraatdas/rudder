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

Rudder leans into how coding agents should be used: massively parallel,
isolated, reviewable, and easy to merge. It opens a native three-pane dashboard,
creates an isolated git worktree for each task, and runs real Claude Code or
Codex processes in the worker pane.

Rudder Cloud lives behind `/login`, `/cloud`, and `/sail`. It keeps the same
local dashboard and worktree flow, with an optional Fly Machines worker path for
tasks that should continue away from your laptop.

## Install

```bash
npm install -g @viraatdas/rudder@latest
rudder
```

Upgrade with the same command:

```bash
npm install -g @viraatdas/rudder@latest
```

Run without a global install:

```bash
npx @viraatdas/rudder@latest
```

## Requirements

- Node.js 20 or newer
- Git
- Optional: Jujutsu (`jj`) for jj workspace mode
- Claude Code and/or Codex installed and logged in
- macOS, Linux, or another Unix-like terminal environment

Check the local setup:

```bash
rudder doctor
```

## Onboarding

```bash
rudder onboard
```

Onboarding checks for `claude`, Codex auth, git, auth files, env vars, and acpx.
It uses the auth you already have whenever possible:

- Claude Code auth from macOS Keychain or `~/.claude/.credentials.json`
- Codex auth from `~/.codex/auth.json`
- `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`

If auth is missing, Rudder lets you skip it and set up the backend later. It
does not require API keys when you already use Claude Code or Codex login.

Config is written to `~/.rudder/config.json`. Mirrored auth metadata is written
to `~/.rudder/auth-profiles.json`.

## Rudder Cloud

Rudder Cloud is the hosted worker mode for Rudder. The local app remains the
control surface: you start in your repo, choose a task, and decide whether it
should run locally or be handed to a cloud worker.

```bash
rudder login
rudder cloud
rudder cloud list
rudder cloud byoc rudder-workstation
rudder cloud onload
rudder cloud onload <runId>
rudder cloud logs <id>
rudder cloud migration-lab
rudder sail staging-worker
```

By default the CLI points at the hosted Rudder Cloud control plane:
`https://rudder-cloud-control.fly.dev`. Set `RUDDER_CLOUD_URL` to
override it for local development or another deployment.

Inside the dashboard, `/login` starts browser auth and `/cloud list` lists
cloud workers after you are logged in. `/cloud` opens a confirmation pane before
anything launches. The highlighted default option onloads the current Rudder
workspace to the cloud, including the repo snapshot, selected HOME auth/config
paths, `RUDDER.md`, and saved `.rudder/runs/*/run.json` metadata. Press Down to
choose a scratch Fly microVM in a fresh cloud directory, then Enter to start.
`/cloud <name>` uses the same prompt but gives the scratch worker a specific
name. `/cloud help` shows the cloud command reference.

Cloud workers use Fly Machines by default. To bring your own workstation or
server instead, add it to `~/.ssh/config` and run:

```bash
rudder cloud byoc rudder-workstation
```

That SSH host should use key-based auth and have Docker available to your SSH
user. Rudder checks the host and stores it in `~/.rudder/cloud.json`.
After setup, `rudder cloud vm <task>` prepares a BYOC run and Rudder tries to
start it on that host over SSH. Dashboard `/cloud` and `/sail` always use Fly
for their scratch cloud-worker path so the main cloud shortcut behaves
consistently. Use `rudder cloud runtime [fly|byoc]` to inspect or change the
saved CLI runtime.
Set `RUDDER_BYOC_AUTOSTART=0` if you always want Rudder to print the Docker
command instead of starting it over SSH.

`rudder login` connects this machine to Rudder Cloud by opening the browser for
the control plane's Better Auth login. If that browser login endpoint is
unavailable, Rudder falls back to local GitHub CLI auth and then GitHub's
browser device flow. It is separate from Claude Code and Codex login: provider
auth still belongs to the official CLIs unless you explicitly configure
otherwise.

Cloud admins can attach existing OAuth clients without redeploying the control
plane:

```bash
rudder cloud login
rudder cloud setup-github <github-client-id>
rudder cloud setup-google <google-client-id>
```

Both setup commands prompt for the client secret without echoing it and persist
the provider credentials into Rudder Cloud's S3-backed state. For automation,
use `RUDDER_GITHUB_CLIENT_ID`, `RUDDER_GITHUB_CLIENT_SECRET`,
`RUDDER_GOOGLE_CLIENT_ID`, and `RUDDER_GOOGLE_CLIENT_SECRET`.

`rudder cloud list` will show cloud-capable runs and remote workers. `rudder
cloud onload` uploads the current Rudder workspace. `rudder cloud onload
<runId>` keeps the older single-run path for moving one local run to the cloud
from the same task context. `rudder cloud logs <id>` currently shows worker
status while full cloud log streaming is being wired up.

Cloud onload is designed around Rudder's existing worktree model. If a task is
already in a worktree, that worktree is the unit Rudder prepares for the cloud.
If a task is still in the main checkout, Rudder prepares a worktree first so the
cloud run does not mutate your local branch directly. Completed cloud work comes
back through the same review and merge path as local work.

Rudder includes selected HOME config paths in the launch snapshot so cloud
workers can behave like your local environment where that is useful: Claude,
Codex, GitHub CLI, git config, npm, Vercel, and Hunk config are considered. It
filters obvious high-risk material such as AWS credentials, `.env` files, SSH
keys, Docker auth, kube config, and private key directories.

The cloud control plane code lives in `cloud/`. It uses Better Auth for
Google/GitHub login, stores launch snapshots in an encrypted S3 bucket, and
starts Fly Machines workers with one-hour presigned snapshot URLs. The local CLI
package stays small; cloud state and worker orchestration run in the separate
control plane.

## Architecture

Rudder has three layers:

```text
┌────────────────────────────────────────────────────────────┐
│ Local Rudder CLI                                           │
│ native OpenTUI dashboard, task input, panes, worktrees     │
├────────────────────────────────────────────────────────────┤
│ Agent Process Layer                                        │
│ real claude or codex terminals, not mocked transcript UIs  │
├────────────────────────────────────────────────────────────┤
│ Optional Rudder Cloud                                      │
│ Better Auth, S3 snapshots, Fly Machines, BYOC Docker runs  │
└────────────────────────────────────────────────────────────┘
```

### Local Dashboard

`rudder` starts the native dashboard. The native binary owns the terminal, draws
the three-pane UI, creates PTYs for workers, and routes keyboard and mouse input.

```text
┌───────────────┬────────────────────────────────────────────┐
│ agents        │ worker                                     │
│ task list     │ live Claude Code or Codex PTY               │
│ status/model  │ scrollback, review view, copy selection     │
├───────────────┴────────────────────────────────────────────┤
│ task input: new tasks, slash commands, cloud launch prompts │
└────────────────────────────────────────────────────────────┘
```

The worker pane is an actual terminal process. When you focus it, keystrokes go
to Claude Code or Codex. Rudder only intercepts global controls such as
`Ctrl-C`, pane focus, review mode, merge, and delete.

### Worktrees And Context

Each normal task gets its own git worktree so agents do not compete inside the
same checkout.

```text
main checkout
  ├─ .rudder/runs/<run-id>/run.json       local run metadata
  ├─ RUDDER.md                            current agent map, gitignored
  └─ ~/.rudder-worktrees/<repo>/<run-id>  isolated agent checkout
```

`RUDDER.md` is regenerated as agents start, finish, restart, or delete. Rudder
injects a short prompt telling each agent to read it, so a new agent can see
what other agents are doing without Rudder rewriting the user task.

### Jujutsu Workspaces

Rudder can use Jujutsu workspaces instead of git worktrees. In auto mode,
Rudder selects jj when `jj root` succeeds or the checkout has a `.jj/`
directory and the `jj` binary is available. Git worktree behavior stays the
fallback everywhere else.

Force a backend by adding the top-level `vcs` field in
`~/.rudder/config.json`:

```json
{
  "version": 1,
  "vcs": "jj"
}
```

Use `"git"` to force git worktrees even inside a jj-managed checkout. Omit
`vcs` to keep auto-detection.

When jj mode is active, Rudder creates task isolation with:

```bash
jj workspace add <path> --name rudder-...
```

Active runs appear in `jj workspace list`. Cleanup forgets the workspace with
`jj workspace forget <name>` and removes the workspace directory. Merging a jj
run creates a merge change in the current workspace with `jj new @ <run-change>`;
if that produces conflicts, Rudder leaves the conflicted jj change in place for
manual resolution. If you manually integrate a completed run with commands such
as `jj squash`, `rudder cleanup` can still remove the completed workspace. If a
workspace is forgotten but its change remains, clean it up with the usual jj
commands such as `jj abandon`.

For git worktree runs, merging is intentionally git-native:

```text
agent worktree -> optional commit -> git merge --no-ff -> main checkout
```

If a merge conflicts, Rudder leaves the merge in place and offers to start an
AI resolver in the main checkout. It does not hide the conflicted git state.
If `mergeStrategy` is set to `"rebase"`, Rudder first rebases the worktree onto
the base branch and then fast-forward merges it. A rebase conflict is left in
the agent worktree so you can resolve it there, run `git rebase --continue`,
and retry sync or merge.

### Review Flow

Review mode uses Hunk when available, with git diff as the fallback path.

```text
selected run worktree
  └─ hunk diff --watch
       └─ embedded PTY in the worker/review pane
```

The review pane is also a real terminal. Mouse wheel and trackpad events scroll
Rudder's captured scrollback first; if there is no Rudder scrollback to move,
events can pass through to the inner TUI.

### Cloud Auth

Rudder Cloud auth is separate from Claude Code and Codex auth.

```text
/login or rudder login
  -> browser opens Rudder Cloud
  -> Better Auth handles Google or GitHub
  -> control plane issues a Rudder CLI token
  -> local CLI writes ~/.rudder/cloud.json
```

Claude Code and Codex credentials stay in their normal locations such as
`~/.claude`, `~/.codex`, Keychain, or existing environment variables. Cloud
workers receive a filtered snapshot of useful HOME config, excluding obvious
high-risk material such as SSH keys, AWS credentials, Docker auth, kube config,
and `.env` files.

### Fly Cloud Path

Dashboard `/cloud` always uses the Fly path for a fresh cloud worker. This keeps
the shortcut predictable even if the CLI default runtime was changed to BYOC.

```text
task pane /cloud
  -> confirmation pane
       highlighted: onload current Rudder workspace to cloud
       Down: start scratch in a fresh cloud directory
  -> local CLI creates repo snapshot
  -> control plane stores snapshot in S3
  -> control plane creates Fly Machine
  -> worker downloads snapshot and starts Rudder
```

The cloud indicator in the agents pane shows whether the local dashboard is
connected to Rudder Cloud.

### BYOC Path

BYOC is for a server you own, reachable through SSH.

```text
rudder cloud byoc <ssh-host>
  -> read ~/.ssh/config
  -> verify SSH and Docker
  -> save host in ~/.rudder/cloud.json

rudder cloud vm "task"
  -> upload snapshot to Rudder Cloud
  -> receive docker run bootstrap command
  -> run it over SSH with nohup
```

On ARM hosts, the bootstrap command switches from
`public.ecr.aws/exla/rudder-worker:latest` to
`public.ecr.aws/exla/rudder-worker:arm64`, so Jetson-style machines do not try
to run an amd64 worker image.

### Control Plane

The hosted control plane is in `cloud/`.

```text
cloud/src/server.ts
  ├─ Better Auth pages and API routes
  ├─ /api/rudder/sail launch/list/pause/resume/onload/bootstrap
  ├─ S3 snapshot storage and persisted state
  ├─ Fly Machines API client
  └─ BYOC bootstrap command generation

cloud/worker/entrypoint.sh
  ├─ download snapshot
  ├─ restore selected HOME config
  ├─ initialize git baseline if needed
  ├─ run Rudder/Codex task
  └─ heartbeat completion back to the control plane
```

## Dashboard

Start the native dashboard:

```bash
rudder
```

The dashboard has three panes:

- `agents`: the left pane with one row per task, its backend, model, effort, and
  completion status
- `worker`: the right pane with the actual Claude Code or Codex terminal
- `task`: the bottom input for starting a new agent

Type a task in the task pane and press `Enter`. Rudder creates a worktree,
writes the current agent context to `RUDDER.md`, and starts the selected backend
inside the worker pane.

When the worker pane is focused, your keystrokes go directly to Claude Code or
Codex. Their slash commands, cursor movement, copy/paste, and terminal UI
continue to work normally. `Ctrl-C` is reserved by Rudder and leaves the
dashboard from any pane.

Rudder owns mouse input inside the dashboard. Wheel or trackpad scrolling is
routed to the pane under the pointer. Over the worker or review pane, it scrolls
Rudder's captured pane output, not the underlying Claude Code, Codex, or Hunk
chat. Use the worker's up and down arrow keys when you want to move through the
agent's own prompt history or menus. Drag selection inside the worker pane
copies selected text, including when you drag upward past the top of the pane.
For full-screen alternate-screen workers, Rudder keeps its own pane history so
trackpad scrolling moves the pane instead of depending on the child app. If the
pane has no Rudder scrollback to move and the inner TUI has explicitly requested
mouse input, Rudder passes the wheel event through so Claude Code, Codex, Hunk,
or another full-screen app can scroll its own view.

Codex needs one extra guard. Rudder launches its pinned `rudder-codex` fork with
`--no-alt-screen`, `CODEX_RUDDER_SCROLLBACK_SAFE=1`, `-c notify=[]`,
`-c features.plugins=false`, and `-c features.computer_use=false`, so Codex keeps
its normal renderer without clearing the parent pane's scrollback and does not
inherit desktop-app notification hooks or bundled plugin helpers that expect the
official signed app launch chain. Codex still inserts finalized transcript rows with
top-origin terminal scroll regions rather than ordinary newlines. Rudder's
embedded `vt100` parser does not expose those rows as normal scrollback, so the
native worker pane mirrors rows that leave a top-origin scroll region into
Rudder-owned pane scrollback before applying the bytes to the parser. Wheel and
trackpad events then scroll that pane history first, which makes Codex and
Claude worker panes follow the same rule: Rudder scrolls the visible worker
transcript, and only forwards wheel events to an inner TUI when Rudder has no
pane scrollback to move.

Keep that behavior covered with:

```bash
npm run test:worker-scroll
```

That script includes the Codex-style top-origin scroll-region test plus the
worker wheel routing tests for normal screen, alternate screen, and edge
forwarding. If a local dashboard still behaves like an older build, check
`rudder --version`, upgrade with `npm install -g @viraatdas/rudder@latest`, and
restart existing Rudder dashboards so no stale `rudder-native` or managed Codex
process is still running.

## Dashboard Shortcuts

| Key | Action |
| --- | --- |
| `Enter` | Start the typed task, or focus the selected worker when the task input is empty |
| `Option-1` / `Option-2` / `Option-3` | Focus agents, worker, or task directly |
| `Tab` / `Shift+Tab` in worker | Forward to the focused agent TUI |
| `Ctrl-G` | Toggle Rudder nav mode while focused inside a worker |
| `Alt-v` | Toggle the selected agent's review view from any pane |
| `Shift+Enter` | Insert a new line in the focused worker prompt |
| `PageUp` / `PageDown` | Scroll the focused worker pane by roughly one page |
| `j` / `k` or arrows | Move through agents when the agents pane is focused |
| `Up` / `Down` | Browse task history when the task pane is focused |
| `Alt-Left` / `Alt-Right` | Move by word in the task pane and in supported worker prompts |
| `Alt-Backspace` / `Ctrl-W` | Delete the previous word in the task pane and in supported worker prompts |
| `Cmd-C` / `Meta-C` | Copy the active Rudder text selection without forwarding `c` to the worker |
| `Ctrl-C` | Leave Rudder from any pane |
| `v` | Toggle the selected agent's review view |
| `Esc` | Leave the review view when it is focused |
| `r` | Restart the selected stopped agent in its worktree |
| `u` | Sync the selected worktree by rebasing it onto its base branch without merging |
| `m` | Merge the selected completed worktree |
| `R` | Combine completed worktrees and start a Codex review-all agent when the agents pane or nav mode is active |
| `M` | Merge all completed worktrees when the agents pane or nav mode is active |
| `dd` | Delete the selected agent and remove its worktree; if it has changes, Rudder gives you a merge chance first |
| `q` | Quit when the worker is not consuming input |

## Task Pane Commands

Type `/` in the task pane to open command suggestions. Use `Up`/`Down` to move
through suggestions and `Enter` to choose one.

| Command | Action |
| --- | --- |
| `/model` | Open the provider-first model picker: choose Claude or Codex, then model, then effort when supported |
| `/main` or `/m` | Start a new main-branch agent pane |
| `/plan` | Toggle Rudder's read-only plan mode for task pane submissions |
| `/plan <task>` | Start one read-only planning session without toggling plan mode |
| `/rudder-plan <task>` | Start a planning coordinator that decomposes the task and spawns worker agents |
| `/run <task>` | Start an implementation run even when plan mode is on |
| `/review-all` | Combine completed worktrees and start a Codex review-all agent |
| `/merge-all` | Merge all completed worktrees |
| `/sync` | Rebase the selected worktree onto its base branch without merging |
| `/login` | Open browser login for Rudder Cloud |
| `/cloud` | Ask whether to onload the current Rudder workspace or start a fresh Fly worker |
| `/cloud <name>` | Ask the same question, using the name for the fresh Fly worker |
| `/cloud byoc <ssh-host>` | Use an SSH host from `~/.ssh/config` for BYOC cloud workers |
| `/cloud runtime [fly\|byoc]` | Show or set the saved cloud runtime |
| `/cloud list` | List cloud workers |
| `/cloud logs <id>` | Show cloud worker status while log streaming is pending |
| `/cloud help` | Show cloud command help |
| `/sail <name or task>` | Short alias for starting a cloud worker |
| `/help` | Show the short command hint |

Use `Ctrl-G` before a Rudder shortcut if the worker pane is focused and you want
the key handled by Rudder instead of by Claude Code or Codex.

If trackpad scrolling does not behave as expected, run
`rudder mouse-test parsed` to confirm your terminal is sending `ScrollUp` and
`ScrollDown` events. For lower-level escape bytes, run `rudder mouse-test raw`.
To inspect live dashboard routing, start Rudder with `RUDDER_MOUSE_DEBUG=1`.
Rudder scrolls one terminal row per wheel event by default so trackpad scrolling
feels smooth in worker and review panes. Override with
`RUDDER_WHEEL_SCROLL_ROWS=<n>` if your terminal is configured differently.

## Models

Run `/model` in the task pane. Rudder first asks for the provider, then the
model, then the effort level supported by that model.

- Claude models include Claude Code aliases such as `sonnet`, `sonnet[1m]`,
  `opus`, `opus[1m]`, and `haiku`, plus explicit Claude model IDs when
  discovered.
- Codex models include Codex-relevant OpenAI model IDs such as `gpt-5.5`,
  `gpt-5.4-codex`, and other discovered GPT-5/Codex models.
- `auto` effort means Rudder does not pass an effort override.

Rudder saves the last selected provider and model, plus effort when chosen, in
`~/.rudder/config.json`, so the same defaults are used when you open a new
dashboard or shell session.

Rudder refreshes model metadata from `https://models.dev/api.json` before the
dashboard starts and caches it in `~/.rudder/models-dev.json`. If the network is
unavailable, it falls back to local Claude session history and Codex's local
model cache.

## Agent Launch

Native dashboard workers launch Claude Code directly and launch Codex through
Rudder's pinned `rudder-codex` fork. Set `RUDDER_CODEX_BIN` to test another
Codex-compatible binary explicitly.

Claude Code:

```bash
CLAUDE_CODE_NO_FLICKER=0 claude \
  --permission-mode bypassPermissions \
  --model <model> \
  --effort <effort> "<task>"
```

Codex:

```bash
CODEX_RUDDER_SCROLLBACK_SAFE=1 rudder-codex --no-alt-screen \
  --dangerously-bypass-approvals-and-sandbox \
  -c model_reasoning_summary="detailed" \
  -c model_supports_reasoning_summaries=true \
  -c model_reasoning_effort="<effort>" \
  -m <model> "<task>"
```

The exact model and effort flags are omitted when set to `auto`.

## Plan Mode

Type `/plan` to toggle planning on or off. While it is on, pressing `Enter`
starts a planner instead of an
implementation run. You can also use `/plan <task>` for a one-off plan, or
`/run <task>` to bypass plan mode and start a normal worktree agent.

Planning sessions use the currently selected Claude or Codex model and lean on
the backend's native planning/read-only controls:

- The planner runs in the current checkout instead of creating a worktree.
- Codex planners launch with `--sandbox read-only`, `--ask-for-approval never`,
  `--enable goals`, and `--search`, so filesystem writes are blocked, Codex
  goals are available, and the native Responses `web_search` tool is available.
- Claude planners launch with Claude Code's native `--permission-mode plan`.

`/rudder-plan <task>` uses the same read-only planner controls, but asks the
planner to emit a structured `RUDDER_PLAN_TASKS` block. When Rudder sees that
block, it starts each listed task as a normal isolated worktree agent. For
larger tasks with a clear validation loop, the planner can attach a durable
Codex `/goal` objective; Rudder passes that into the worker prompt so Codex can
set the goal before implementation. If the planner asks follow-up questions
instead of emitting the block, Rudder does not spawn workers.
- Rudder only prefixes the task with a short planning request; it no longer
  injects a custom planner contract.
- Normal implementation runs are unchanged: they still create worktrees and use
  the full-permission worker launch described above.

## Worktrees And Merging

Every dashboard task runs in its own git worktree under
`~/.rudder-worktrees/...`, so parallel agents do not edit the same checkout.
Run records are saved under `.rudder/runs/`. If you exit Rudder, live worker
processes stop, but the agents remain listed next time you open Rudder in the
same repo.

Press `m` to merge the selected completed agent back into the original branch.
Press `M` to merge all completed agents. Rudder asks for confirmation before
merging. Clean merges become merge commits; if git reports conflicts, Rudder can
open an agent in the main checkout to help resolve them.

Set `mergeStrategy` in `~/.rudder/config.json` to choose the merge behavior:

```json
{ "mergeStrategy": "rebase" }
```

- `"merge"` is the default and keeps the existing `git merge --no-ff` flow.
- `"rebase"` rebases the worktree branch onto the latest base branch ref first,
  then merges with `git merge --ff-only`.

Press `u`, run `/sync`, or use `rudder sync <runId>` to do only the rebase
step. If the rebase hits conflicts, Rudder leaves the worktree mid-rebase and
prints the conflicted files. Resolve them inside that worktree, run
`git rebase --continue`, then retry `rudder sync <runId>` or
`rudder merge <runId>`.

Command-line equivalents:

```bash
rudder merge <runId>
rudder sync <runId>
rudder cleanup
```

## Steering

Before a task starts, Rudder writes `RUDDER.md` into the worktree and adds it to
`.gitignore` once. The file lists active agents, their worktrees, and what they
are doing. The worker prompt tells Claude Code or Codex to read it first.

After a worker appears to finish, Rudder can wait briefly and send a focused
follow-up asking the same agent to verify what remains. When an agent finishes
or fails, Rudder plays the bundled completion sound.

## Review

Press `v` on an agent to toggle a review view for that agent's worktree:

```bash
hunk diff --watch
```

Hunk provides the multi-file review UI, sidebar navigation, mouse support,
watch mode, inline agent notes, and untracked-file handling. Rudder forwards
keyboard input into Hunk while the review pane is focused and keeps wheel or
trackpad scrolling on Rudder's review scrollback. Press `v` or `Esc` to return
to the live Claude Code or Codex worker.

Press `R` to review all completed worktrees as one bundle. Rudder creates a new
aggregate worktree, commits pending source-worktree changes, merges the
completed agent branches into that aggregate branch, and starts a Codex
`gpt-5.5` review-all agent with a `/review` prompt. If the pre-merge hits a
conflict, that same review agent starts in the conflicted aggregate worktree and
is instructed to resolve it before continuing.

When the Codex review-all row is done, press `m` on that row to merge the
reviewed aggregate branch back into the main checkout. Rudder then marks the
source agent rows as merged too.

Rudder writes a per-worktree `.hunk/config.toml` in Hunk's light mode and
ignores that config through git's local info exclude, so it does not get merged.
Set `RUDDER_HUNK_THEME=paper` or another Hunk theme name to override it.

On dashboard startup, Rudder installs `hunkdiff@latest` automatically if neither
`hunk` nor `hunkdiff` is available. If the install fails, or if you set
`RUDDER_REVIEW_TOOL=git`, the review pane falls back to a live `git diff` view
instead of downloading anything when you first press `v`.

Rudder also injects Hunk review guidance into `RUDDER.md` and the worker prompt.
Agents are told to run `hunk skill path`, load the Hunk review skill, and use
`hunk session review --repo . --json` plus `hunk session comment ...` commands
against the live review.

## One-Shot Commands

```bash
rudder "fix the failing tests"
rudder claude "fix the auth redirect bug"
rudder codex --model gpt-5.5 "refactor the parser"
rudder run --worktree "try the alternate implementation"
```

## Run Management

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

## Legacy Interfaces

The native dashboard is the default. Older interfaces are still available:

```bash
rudder tmux
rudder tui
rudder --no-native
```

## Development

Clone the repo and run the setup script to bootstrap your environment:

```bash
git clone https://github.com/viraatdas/rudder.git
cd rudder
./setup.sh
```

`setup.sh` verifies prerequisites (**Node >=20**, **git**, **npm**, **Rust**
toolchain via `cargo`), installs npm dependencies (`npm ci` when a lockfile is
present, otherwise `npm install`), runs the full build (`tsc` + `cargo build
--release` + `copy-native`), runs `tsc --noEmit` as a type check, and smoke
tests the built CLI with `node dist/index.js --version`. Any failed step exits
non-zero with a message identifying which step failed. The script is
idempotent — re-run it after pulling new changes.

If you prefer to run the steps manually:

```bash
git clone https://github.com/viraatdas/rudder.git
cd rudder
npm ci                        # or: npm install
cargo test --manifest-path native/Cargo.toml
npm run check
npm run build
```

## License

MIT
