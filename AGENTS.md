# AGENTS.md

Engineering and implementation reference for Rudder. This is the internal map of
how the project is built. End users should read `README.md` instead (install and
usage). If you are an AI agent or a contributor working in this repo, read this
first.

> Note: `RUDDER.md` in the repo root is generated at runtime by Rudder to
> coordinate live agents in a checkout. It is not authored documentation. Do not
> hand-edit it; it is rewritten on the next run and git-excluded.

---

## 1. What Rudder is

Rudder is a terminal app for running coding agents (Claude Code and Codex) in
parallel. Each task gets an isolated jj (Jujutsu) workspace, colocated with git,
and runs a real agent CLI inside a native terminal pane. Rudder owns the outer
workflow around those agents: a three-pane dashboard, pane focus, scrollback,
task summaries, live review, and merge back to the base branch. Rudder Cloud is
an optional hosted worker mode that keeps the same local control surface.

It ships as the npm package `@viraatdas/rudder`, binary `rudder`
(`dist/index.js`).

Two big pieces, one product:

- A **TypeScript orchestrator** (the `rudder` CLI): argument parsing, run
  lifecycle, jj workspaces (colocated with git), backend adapters, state
  persistence, auth, cloud.
- A **Rust native dashboard** (`rudder-native`): the interactive three-pane TUI
  that hosts the agent PTYs, drawn with ratatui over crossterm, with real
  pseudo-terminals via portable-pty.

A third piece, **Rudder Cloud** (`cloud/`), is a standalone control-plane server
plus a worker image, used only when a task is handed off to the cloud.

---

## 2. Repository layout

```
.
├── src/                  TypeScript orchestrator (compiled to dist/)
├── native/               Rust native dashboard crate (rudder-native)
│   ├── src/main.rs        the App state machine + domain types + entry/run loop
│   ├── src/render.rs      ratatui rendering: panes, prompts, layout, styles
│   ├── src/selection.rs   mouse->coordinate mapping, selection, clipboard, cells
│   ├── src/detect.rs      worker-output heuristics (idle/permission/prompt)
│   ├── src/models.rs      model/effort tables, model picker, suggestion ranking
│   ├── src/launch.rs      agent launch/resume command building, review-all runs
│   ├── src/tasks.rs       prompt construction, task summaries, rudder-plan parsing
│   ├── src/gitio.rs       git/worktree/run-record persistence + fs helpers
│   ├── src/cloudio.rs     cloud command plumbing + Codex session-log scanning
│   ├── src/usage.rs       token-usage accounting, pricing, date helpers
│   ├── src/config.rs      ~/.rudder config + model defaults + update notices
│   ├── src/textedit.rs    input editing (drafts, history, cursor/word ops)
│   ├── src/keys.rs        key/mouse event -> terminal byte encoding
│   ├── src/app_tests.rs   #[cfg(test)] dashboard tests
│   └── src/pty_terminal.rs PTY wrapper + terminal emulation/scrollback (in lib.rs)
├── cloud/                Rudder Cloud control plane (Node http server) + worker image
│   ├── src/server.ts      auth, CLI login, sail (workers), workspace (snapshots)
│   └── worker/            the cloud worker container (entrypoint + supervisor)
├── site/                 Marketing site (static, deployed to rudder.viraat.dev)
├── tests/                Node integration tests (*.test.mjs against dist/)
├── dist/                 Build output (JS + copied native binary). Not tracked; built on publish.
├── assets/               Static assets bundled into the npm package
├── package.json          npm metadata; build/test scripts
├── Cargo.toml            workspace manifest for the native crate
├── setup.sh              one-shot installer helper
├── README.md             customer-facing install + usage
└── AGENTS.md             this file
```

`dist/` is build output and is NOT tracked in git (it was removed from the repo
in PR #15). The npm `files` allowlist still ships `dist/`, `assets/`, `README.md`,
`package.json`, and `prepack` runs the full build, so `npm publish` always packs
a fresh `dist/` (including `dist/native/rudder-native`). Rebuild before
publishing; there is nothing to commit under `dist/`.

---

## 3. Architecture overview

```
 user
  │  rudder <task> / rudder (no args)
  ▼
 src/index.ts ──> src/main.ts (arg parse + command dispatch)
  │                     │
  │  no args + TTY      │ subcommands (run, doctor, login, cloud, ...)
  ▼                     ▼
 native dashboard   startRun / startNativeRun / startNativePlan (run-manager.ts)
 (rudder-native)         │  creates jj workspace (jj.ts), writes run.json (state.ts)
  │  hosts PTYs          │  spawns a detached `rudder __worker --repo --run <id>`
  │  reads run.json      ▼
  │  directly        __worker -> backend adapter (backends.ts)
  │                      │  claude / codex(rudder-codex) / acpx, streamed as events
  ▼                      ▼
 panes: Agents | Worker | Task     .rudder/runs/<id>/{run.json,events.ndjson,output.txt}
```

Key idea: the TypeScript side is the orchestrator and source of truth on disk
(`run.json`). The native dashboard renders that state, hosts the real agent
terminals, and triggers TS operations (start/merge/sync/cleanup) by shelling back
into `rudder`. They communicate through the filesystem (`.rudder/runs/`) and
process spawns, not a socket.

---

## 4. The TypeScript orchestrator

### Entry: `src/index.ts`
- Shebang `#!/usr/bin/env node`. Dynamically imports `main.js` so the cwd-recovery
  guard runs first.
- `recoverCwdIfNeeded()`: if `process.cwd()` throws (deleted/unreadable dir, common
  after a worktree is removed), it walks `$PWD` ancestors, then `~`, `/tmp`, `/`,
  and `chdir`s to the first readable one. Surfaces a notice.
- Top-level try/catch turns any thrown error into `rudder: <message>` + `exit(1)`,
  with `MissingToolError` printed as-is (it carries an install hint).

### Dispatch: `src/main.ts`
- `parseArgs(argv)` builds `{ command, args[], flags{} }`. Flags include
  `--version/-v`, `--help/-h`, `--json`, `--quiet/-q`, `--detach/-d`, `--watch`,
  `--follow/-f`, `--worktree`, `--queue`, `--allow-dirty`, `--force`,
  `--non-interactive`, `--no-tmux`, `--no-native`, `--headless`, `--cwd`,
  `--repo`, `--run`, `--backend`, `--model`, `--tmux-session`.
- `main()` flow:
  - `--version`/`version`: print version, check npm for an update.
  - `--cwd <dir>`: chdir.
  - No command + args present: treat the args as a task and `startRun`.
  - No command + TTY: `openDashboard` (the native dashboard). This is the default
    `rudder` experience.
  - No command + not TTY: `printHelp`.

Public commands (the `switch (parsed.command)`):

| Command | Purpose |
|---|---|
| `run <task>` | Start a run (worktree-isolated by default policy) |
| `claude` / `codex` / `acpx [args]` | Start a run pinned to a backend |
| `dashboard` | Open the native dashboard (default when no args + TTY) |
| `tmux` | Open the legacy tmux dashboard |
| `tui` / `shell` / `interactive` | Ink-based interactive TUI (`src/tui.tsx`) |
| `legacy-shell` | Older interactive shell |
| `restart` | Reset the local Rudder session, then open the dashboard |
| `mouse-test` | Native mouse diagnostics |
| `onboard` / `doctor` | Setup wizard / environment check (`src/auth.ts`) |
| `login` | Browser auth for Rudder Cloud |
| `cloud` / `sail` | Cloud worker control (`src/cloud.ts`) |
| `watch` / `logs` / `status` / `runs` | Inspect runs |
| `stop` / `delete` / `merge` / `sync` / `cleanup` | Manage runs |

Internal commands (prefixed `__`, spawned by Rudder itself, not for users):
`__worker` (the per-run worker process), `__agents`, `__task`, `__worker-idle`
(tmux pane drivers).

### The native dashboard launcher
`openDashboard` resolves the native binary via
`src/native-binary.ts::resolveNativeBinaryPath()` which checks, in order:
`dist/native/rudder-native`, `target/release/rudder-native`,
`native/target/release/rudder-native`. `--no-native` falls back to the tmux or
Ink path.

---

## 5. Run lifecycle (the core)

Implemented in `src/run-manager.ts`.

1. **Start** (`startRun`, `startNativeRun`, `startNativePlan`):
   - Resolve repo root (`git.ts::findRepoRoot`, git `rev-parse --show-toplevel`,
     falling back to `path.resolve(cwd)`).
   - Load config, pick backend + model + effort.
   - `ensureJj()` + `ensureColocated(repoRoot)` (`jj.ts`): jj is hard-required and
     auto-colocated (`jj git init --colocate`) so the repo still externally looks
     like git.
   - Decide jj workspace vs current checkout. Policy: a second concurrent run on
     the same checkout forces a workspace (`activeRunsForCheckout`). `--worktree`
     forces one explicitly.
   - `baseCommit = currentJjChangeId(repoRoot)`. `targetBranch =
     currentJjChangeId(repoRoot)` (else `currentBranch`).
   - `createRunWorkspace` -> `jj.ts::createRunJjWorkspace`: `jj workspace add` at
     `../.rudder-worktrees/<repo>-<hash>/<task-slug>-<hash>` with name
     `rudder-<id..>-<hash>`. The run records `vcs:"jj"` and
     `worktree.jjChangeId`.
   - `createRunRecord` (`state.ts`) writes `.rudder/runs/<id>/run.json`.
   - `writeAgentContext` regenerates `RUDDER.md` and excludes it via
     `.git/info/exclude`.
   - Spawn a **detached** worker: `rudder __worker --repo <root> --run <id>`,
     `unref()`ed so the parent can exit. The run goes `created -> running`.

2. **Worker** (`__worker` in `run-manager.ts`):
   - Loads the run, builds a spec (`brain.ts::createSpec` ->
     `renderContract`), and drives the backend through `backends.ts`.
   - Streams `RudderEvent`s, appended to `.rudder/runs/<id>/events.ndjson`;
     `backend.output`/`backend.error` text is also appended to `output.txt`.
   - On each status/session change it calls `saveRunRecord`.
   - Optional auto-steer loop (`AUTO_STEER_DELAY_MS`, bounded by
     `run.autoSteer.max`) nudges an idle agent to continue.

3. **Backends** (`src/backends.ts`, `getBackend(id)` returns a `BackendAdapter`):
   - **claude**: spawns `claude`. New session: `--session-id <uuid>`. Resume:
     `--resume <id> --fork-session`. Effort mapped via `normalizeEffortForBackend`.
   - **codex**: spawns the wrapped `rudder-codex` binary (see
     `src/codex-binary.ts`: `ensureRudderCodexBinary`, `codexLaunchEnv`,
     `CODEX_RUDDER_CONFIG_ARGS`) so Rudder controls config/auth without touching
     the user's global Codex setup.
   - **acpx**: ensures a Codex session via `acpx codex sessions ensure --name`,
     then runs `acpx codex ...`.
   - All three funnel through `spawnAndStream(...)` which pipes child stdout/stderr
     into `RudderEvent`s and resolves with the exit code.
   - `backendEnv(provider)` injects the right `ANTHROPIC_*` / `OPENAI_*` env from
     the resolved auth profile.

4. **Verify** (`brain.ts::verifyRun`): produces a `VerificationResult`
   (`satisfied`, `missing`, `notes`, `shouldContinue`) written to `verifier.json`.

5. **Merge / sync** (jj-first; routed by `run.vcs` in `run-manager.ts`):
   - New runs are jj. `mergeJjRunIntoCurrentWorkspace(run, allowDirty)` (`jj.ts`):
     captures `currentOpId` first (records it on `MergeState.operationId`), then
     `jj new <into> <node>` to merge the run's change into the current change `@`.
     jj records any conflict in the merge change (`jj resolve --list`) rather than
     blocking; sets `MergeState.mergeChangeId` and `run.status` to `merged` or
     `merge-conflict`. After a clean merge, `run-manager` calls
     `exportToGit` (`jj git export`) so the colocated git refs stay current.
   - `syncRunWorkspace(run, baseBranch)` (`jj.ts`): `jj rebase -s <node> -o <trunk>`
     (jj records conflicts in place, never blocks; no rebase-in-progress dance).
   - `removeRunWorkspace(run)` (`jj.ts`): `jj workspace forget <name>` + rm dir.
   - **Global undo:** `currentOpId`/`undoToOp`/`undoLast` back `rudder undo [opId]`;
     `.rudder/undo-stack.json` records `UndoEntry`s. `jj op restore` is global - it
     rewinds all workspaces and refs at once.
   - **Legacy git runs** (`run.vcs === "git"`, created before the jj switch) stay
     mergeable via the git helpers kept in `git.ts`
     (`mergeGitRunIntoCurrentBranch`, `syncGitRunWorktree`, `rebaseWorktreeOntoBase`,
     `resolveRebaseBaseRef`, `removeGitWorktree`), reached only behind the vcs guard.

---

## 6. Data model and on-disk state

### Types: `src/types.ts`
The important shapes:
- `RunRecord`: the full per-run document. Status union is
  `created | running | steering | verifying | completed | failed | cancelled |
  merge-conflict | merged`. Holds
  `worktree{enabled,path,branch?,workspaceName?,jjChangeId?}`,
  `vcs:"jj"|"git"`, `process{pid,...}`, `turns[]`, `autoSteer`,
  `session{nativeSessionId,...}`, `terminal{kind:"tmux",...}`, `verification`,
  `merge` (with `operationId`/`mergeChangeId`), `sync`,
  `resolverFor?`/`resolverRunId?`, `taskSummary`/`taskSummaryLlm`.
- `RudderConfig`: `defaultBackend`, `lastUsedBackend`, `mergeStrategy`,
  `runPolicy`, `backends{claude,codex,acpx}` (each `BackendConfig` with
  `model`/`effort`/`reasoningEffort`/`profileId`).
- `AuthProfileStore`: provider credentials (`api_key` / `oauth` / `token`),
  plus `order`, `lastGood`, `usageStats` (cooldown/disable tracking).
- `RudderEvent`: the event-stream union appended to `events.ndjson`.
- `SpecContract` / `VerificationResult` / `RunRequest` / `BackendAdapter`.
- `CloudAuthState` / `CloudSail` for cloud.
- `VcsMode = "git" | "jj"`: new runs are always `"jj"`; `"git"` only appears on
  legacy records merged behind the vcs guard (section 12).
- `UndoEntry = {opId,label,ts,runIds[]}`: entries on `.rudder/undo-stack.json`
  backing `rudder undo`.

### File layout
Global (`~/.rudder/`, overridable via `RUDDER_HOME`):
- `config.json` (`RudderConfig`, written `0o600`)
- `auth-profiles.json` (`AuthProfileStore`, `0o600`)
- `cloud.json` (`CloudAuthState`)

Per repo (`<repo>/.rudder/`):
- `runs/<id>/run.json` (`RunRecord`)
- `runs/<id>/events.ndjson` (append-only event log)
- `runs/<id>/output.txt` (raw backend stdout/stderr)
- `runs/<id>/spec.json`, `runs/<id>/verifier.json`
- `undo-stack.json` (`UndoEntry[]`, atomic via `updateJson`)

jj workspaces live outside the repo at
`../.rudder-worktrees/<repoSlug>-<repoHash>/<taskSlug>-<runHash>` so they never
nest inside the checkout. `RUDDER.md` is generated at the repo root and inside
each active workspace, and excluded via each checkout's `.git/info/exclude`.

### Persistence helpers: `src/util.ts`
- `writeJson(path, value, {mode})`: atomic write (temp file + `rename`) under a
  **per-path async lock** (`withPathLock`). The temp name includes
  `process.pid + Date.now() + randomUUID()` so concurrent writers to the same
  file never collide on the temp path.
- `updateJson(path, transform)`: read-modify-write under the same lock; used by
  `saveRunRecord` so a foreground status write cannot clobber the background LLM
  title (`taskSummaryLlm`).
- `readJson<T>`: returns `null` on any read/parse failure.
- `commandExists`, `runCommand`/`runCommandSync` (with a sane default PATH for GUI
  launches), `MissingToolError` (carries `TOOL_INSTALL_HINTS`), `shortenHome`,
  `slugPrefix`, `shortHash`.

### State module: `src/state.ts`
Path helpers (`runRecordPath`, `eventsPath`, ...), `loadConfig`/`saveConfig`
(with `defaultConfig`/`normalizeConfig`), `createRunRecord`, `saveRunRecord`,
`loadRunRecord`/`listRuns`, and the **background LLM summarizer**
(`maybeBackgroundLlmSummarize`): fire-and-forget, gated by an in-flight Set and
the `taskSummaryLlm` flag, upgrades the naive task summary to an LLM-generated
title and persists it via the atomic path above.

---

## 7. Native dashboard (`native/src/main.rs`, `native/src/pty_terminal.rs`)

Stack: `ratatui` (rendering) over `crossterm` (input + raw mode + mouse), with
`portable-pty` for real pseudo-terminals. One `App` struct holds all state;
a render loop draws three panes and an input loop dispatches key/mouse events.

The crate is split into focused modules (see the layout in section 2).
`main.rs` keeps the domain types (the enums + structs like `AgentRun`) and the
whole `App` struct + `impl App`, plus the `main`/`run` entry. Free functions live
in topic modules (`render`, `selection`, `detect`, `models`, `launch`, `tasks`,
`gitio`, `cloudio`, `usage`, `config`, `textedit`, `keys`). Module convention:
every module is a child of the binary crate and pulls shared scope with
`use super::*`; the crate root re-exports each module (`mod x; use crate::x::*;`)
so call sites stay unchanged and modules can use each other. Because the modules
are descendants of the crate root, they read crate-root-private items (the `App`
struct's fields and methods, the domain types' fields) for free; only items moved
*out* of the root are marked `pub(crate)`. Keeping the entire `impl App` in
`main.rs` is deliberate: it avoids having to widen the visibility of `App`'s many
private methods. Splitting `App` itself out later would require that visibility
churn, so it was left whole.

### The run loop and the single-thread mutation model
`run()` (in `main.rs`) is the whole engine: one thread, one owned `App`. Each
iteration it (1) calls `poll_agents()` (the heartbeat), (2) redraws if `dirty`, then
(3) blocks on `event::poll(timeout)` for input. The timeout adapts: `TICK_RATE` =
33 ms normally, `STREAM_TICK_RATE` = 11 ms while a planner is streaming (so the live
transcript lands ~3x sooner). Up to `MAX_EVENTS_PER_FRAME` = 64 queued events drain
per frame.

**Single-writer invariant.** `App` is mutated ONLY from this loop, via `&mut self`
methods, so mutation is serialized: no locks, no races. There is no separate
"scheduler object" to poke; `run_scheduler`/`next_node_to_launch` are stateless logic
that *reads* `App` state, so changing scheduling behavior means changing the state
they read (e.g. `stop_agent_at` sets a node `Stopped`, which the next tick observes).
Exactly three things trigger a mutation: (a) your keystrokes (`handle_key`/
`handle_mouse`), (b) the heartbeat (`poll_agents` → on its tick cadence:
`run_scheduler`, `maybe_ingest_worker_followups`, `maybe_auto_merge`,
`finalize_merge_resolvers`, `maybe_handle_drift`), and (c) background results drained
from mpsc channels. Work that must not block the UI (the haiku task-summary and
completion-note summarizers, cloud status) runs on background threads that COMPUTE and
SEND a message; the loop drains the channel and applies the write. Threads never touch
`App`. The LLM agents are likewise pure data producers (plan blocks, `rudder done`
notes); they feed data that the loop ingests, they do not mutate `App`.

**PTY drain throttling.** `poll_agents` fully drains the FOCUSED agent's PTY every
frame, throttles unfocused agents to once per `UNFOCUSED_DRAIN_INTERVAL` = 500 ms (so
vt100 parse cost scales with focus, not agent count), and always drains a streaming
planner every tick. Completion of an interactive agent is declared once it has looked
idle for `READY_GRACE` = 3200 ms (or on clean process exit), which is robust against a
TUI that repaints while idle.

### Panes and focus
`FocusPane = Agents | Worker | Task`.
- **Agents**: the run list. Grouped: a `main`-branch agent section first, then
  worktree runs, then a merged section. Select with `j/k` or arrows; `Enter`
  focuses/starts; `m` merge, `d` delete, `r` rename, `v` review, `x` stop (a running
  plan worker; keeps its workspace), `g` toggle nested DAG view. (The `u` sync key was
  removed along with `/sync`; jj keeps workspaces current.)
- **Worker**: the real PTY of the focused agent (claude/codex). Keystrokes are
  forwarded to the child so its prompts, slash commands, selection, and `Tab`
  behave natively. Trackpad scroll moves scrollback like a terminal; alternate
  screen apps are handled in `pty_terminal.rs`.
- **Task**: an input line that starts the next agent. Supports history (`Up`/
  `Down`), readline-style editing (Ctrl+A/E/U/K/W/D/H, Alt/Ctrl+Backspace word
  delete, Alt+Left/Right word nav), and slash-command completion.

### Keybindings (the input model in `handle_key`)
Order of handling matters; the early returns gate everything else.
- **Ctrl+C**: quit, with a confirm guard if agents are still running.
- **Ctrl+W (leader)**: arms a one-shot leader. The *next* key runs a dashboard
  command and disarms: `1/2/3` focus panes, `v` review, `m` merge, `R` review-all,
  `M` merge-all, `r` rename, `j/k` move, `d` delete, `q` quit, `Esc`
  cancels. See `handle_leader_key`. This is the reliable cross-terminal way to
  drive the dashboard from inside the worker pane. Tradeoff: Ctrl+W no longer
  reaches the worker PTY as readline "delete word".
- **Ctrl+G (nav mode)**: a *sticky* mode (toggle on/off) with the same command
  set; `Esc` exits. Predates the leader; both are kept.
- **Option/Alt + 1/2/3 and v**: jump panes / toggle review directly. Many macOS
  terminals (Terminal.app, default iTerm2) do not send an Alt modifier for
  Option+key, so the dashboard *also* accepts the typographic characters Option
  produces on a US layout: Option+1=`¡` (U+00A1), Option+2=`™` (U+2122),
  Option+3=`£` (U+00A3), Option+v=`√` (U+221A). This is what makes the documented
  "Option-1/2/3" shortcuts actually work out of the box.
- Otherwise the key is dispatched to the focused pane's handler.

### Slash commands (parsed in `handle_command`)
`/model [backend] [model] [effort]`, `/main` or `/m` (start a main-branch agent),
`/goal` (forwarded to the focused agent), `/usage`, `/cloud [list]`, `/merge-all`,
`/review-all`. Planning is the default paradigm now: typing a task runs the
orchestrator, which decomposes it into a DAG you refine (type into the task pane)
and approve (Enter). `/plan` and `/sync` are retired no-ops that print a hint;
the `u` sync keybinding was removed.

### Review and merge-all
`v` opens a review pane showing the run's `jj diff` (`ensure_review_diff`). The old
Hunk (`hunk diff --watch`) integration was removed in favor of native jj diff
inspection. `R` (review-all) creates an aggregate Codex agent over all completed
worktrees; `M` (merge-all) opens a confirmation to merge them. Review-all spins up a
dedicated agent whose task is a `/review` over the combined diff.

### Tests
`native/src/app_tests.rs` holds the dashboard tests (231, plus one `#[ignore]`d live
conductor harness; declared `#[cfg(test)] mod app_tests;` in `main.rs`). Includes the
leader/Option-key coverage: `ctrl_w_leader_then_digit_focuses_pane`,
`ctrl_w_leader_is_one_shot`, `ctrl_w_leader_escape_cancels_without_action`,
`option_typographic_chars_focus_panes`, `alt_digit_still_focuses_pane`, plus the
existing nav-mode, worker-scroll, and rendering tests.

---

## 8. Auth (`src/auth.ts`)

- `detectEnvironment()` checks for `claude`, `codex`, `acpx` on PATH (+ acpx
  version and the latest npm acpx).
- Credentials are mirrored into `~/.rudder/auth-profiles.json` from whatever the
  user already has, in priority order:
  - Claude: macOS Keychain (`Claude Code-credentials`) or
    `~/.claude/.credentials.json`.
  - Codex: `~/.codex/auth.json`.
  - Env: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`.
- `runDoctor` prints a status report (also `--json`). `runOnboard` is the
  interactive setup. Rudder never requires API keys if you already log in through
  Claude Code or Codex.

---

## 9. Rudder Cloud (`src/cloud.ts` + `cloud/`)

Optional hosted worker mode: run the same agents on a remote machine while the local
dashboard stays the control surface. The CLI client is `src/cloud.ts`; the control
plane is a standalone deployable in `cloud/` (currently hosted at
`https://rudder-cloud-control.fly.dev`, override with `RUDDER_CLOUD_URL`). It is
optional and decoupled: the npm package stays light, and nothing here runs unless you
`rudder login` and hand off a task.

### 9.1 Two shapes: sails vs workspaces
Both use the same worker image and snapshot format; they differ in lifecycle + storage.
- **Sails** — ephemeral, task-oriented. The Fly machine lives only as long as the task;
  storage is transient. Statuses: `queued | running | paused | completed | failed`.
  Idle sails are PAUSED (suspended, not destroyed) after `RUDDER_IDLE_PAUSE_MS`
  (default ~2h) so they can resume without re-downloading the snapshot; a separate admin
  GC does true cleanup.
- **Workspaces** — persistent dev environments (Fly-only). Backed by a persistent volume
  (default ~3GB). Statuses: `queued | running | paused | stopped | failed`. Idle-swept
  (`sweepIdleWorkspaces`, every ~60s) and stopped after `RUDDER_WORKSPACE_IDLE_MS`
  (~30 min). Keyed by `(account_id, workspace_key)` so re-launch reuses the same volume.

### 9.2 CLI (`src/cloud.ts`)
Subcommands: `login`, `launch`, `sail`, `byoc`, `vm`/`byo-vm`, `list`/`ls`, `status`,
`logs`, `attach`, `workspace`, `onload`, `bootstrap`, `pause`, `resume`, `stop`,
`setup-github`, `setup-google`, `setup-byoc`, `setup-vm`, `setup-fly`, `setup`,
`runtime`.
- `login` runs a browser/device-style flow (polls `/api/cli/login/poll`) and stores
  `CloudAuthState` in `~/.rudder/cloud.json` (`cloudAuthPath()`, mode `0o600`): `token`,
  `cloudUrl`, `defaultRuntime`, `byocSshHost`, `accountId`, `email`, `expiresAt`.
- `onload` snapshots the current Rudder workspace (repo + selected HOME auth/config) and
  uploads it via an S3 presigned URL so a worker can continue the task.
- Error messages prefer the parsed server error, then the body, then `<status>
  <statusText>` (so empty gateway responses still report a code).

### 9.3 Auth + account isolation
Better Auth handles Google + GitHub OAuth; there is also a GitHub device flow and a
`gh auth token` exchange. The control plane issues a Rudder CLI token stored HASHED as
`rdr_<base64>` in `rudder_tokens`. Every query is keyed to an `account_id`
(`github:<id>` or `better-auth:<uuid>`), so tenants never see each other's sails or
workspaces. Worker-to-control-plane auth uses a separate `RUDDER_WORKER_TOKEN`
(`rdrw_*`); client streaming uses the user token. Admin endpoints are gated by
`RUDDER_ADMIN_EMAILS`.

### 9.4 Snapshots
`createSnapshot` stages the repo (via `git ls-files`) + a small set of HOME paths +
metadata, packaged as a `.tgz` (`repo/`, `home/`, `env/cloud-env.json`,
`manifest.json`) and stored to S3 at `snapshots/{accountId}/{date}/{uuid}.tgz`
(AES256, 1-hour presigned URLs). It deliberately EXCLUDES secrets and bulk: `.ssh`,
`.gnupg`, keychains, `.env*`, key files, and `.cache`/`node_modules`/worktrees. On
macOS it extracts Claude OAuth tokens from the Keychain into `.claude/.credentials.json`
so the worker can authenticate. Env capture is allow-listed (`*_API_KEY`/`*_TOKEN`/
`*_SECRET` plus `RUDDER_CLOUD_ENV_VARS`) and excludes `PATH`/`HOME`/`USER`/shell/Rudder
internals.

### 9.5 Control plane (`cloud/src/server.ts`)
A plain Node `http` server (no framework). Dependencies: `better-auth` +
`better-sqlite3` (auth + state), `@aws-sdk/client-s3` + `s3-request-presigner`
(snapshots), `ws` (streaming). Route groups: `/api/auth/*` (better-auth), `/api/cli/
login*`, `/api/rudder/setup/{github,google}`, `/api/rudder/sail*`,
`/api/rudder/workspace*`, `/api/admin/workspace/gc`. State is a SQLite db (`/tmp/
rudder-cloud/rudder-cloud.sqlite` locally, or restored from S3 at
`RUDDER_CLOUD_STATE_KEY`) with tables: `rudder_tokens`, `rudder_sails`,
`rudder_workspaces` (unique on `account_id + workspace_key`), `rudder_settings` (OAuth
client ids/secrets), plus the Better Auth tables. **Stateless control plane:** when
`RUDDER_CLOUD_PERSIST_STATE=1` and `RUDDER_S3_BUCKET` is set, SQLite is restored from S3
on boot and persisted after each request (debounced), so the Fly app can restart without
losing sails/workspaces. Deploy config: `cloud/fly.toml` + `cloud/Dockerfile`.

### 9.6 Runtimes: Fly Machines + BYOC
- **Fly Machines (managed):** `createFlyMachine` POSTs `/v1/apps/{app}/machines` with
  `RUDDER_WORKER_IMAGE` (`auto_destroy:false`). Idle pause via `shouldPauseStaleSail`;
  workspace idle-stop via `sweepIdleWorkspaces`; true cleanup via the admin GC.
- **BYOC (bring your own compute):** `byoVmBootstrapCommand` emits a `docker run` script
  (with `arm64` detection) that `startByoVmWorkerOverSsh` runs over SSH (under `nohup`
  unless `RUDDER_BYOC_AUTOSTART=0`). BYOC cannot pause/resume (only stop); its state
  comes from heartbeats only.

### 9.7 WebSocket streaming
Each sail/workspace has a `Channel`: one worker WS + N client WS + a ~256KB ring buffer
replayed on attach. The worker pipes PTY bytes as binary frames; the control plane
broadcasts them to clients (permessage-deflate for frames over ~1KB). A channel is
disposed when the worker and all clients disconnect. (This is a WebSocket bridge, not an
SSH tunnel: no key distribution, simpler firewall traversal, but Fly must not strip the
`Upgrade` header.)

### 9.8 Worker image (`cloud/worker/`)
`Dockerfile` (base `node:22-slim`, runs as non-root `rudder`) + `entrypoint.sh` +
`supervisor.mjs`. The supervisor installs the agent CLIs (claude-code, codex, acpx,
hunkdiff), fetches + restores the snapshot from `RUDDER_SNAPSHOT_URL`, handles migrated
agents (`migration.json`), spawns `rudder` (workspace) or `rudder codex --worktree
<task>` (sail) under a `node-pty` 120x32, bridges I/O to the control-plane WS, and
reports a final heartbeat on exit. On restart it checks `.rudder-staged.json` and
re-fetches a fresh signed URL from the snapshot-url endpoint.

### 9.9 Known limitations (current state, not action items)
Documented so they are not rediscovered: workspace re-use ignores a newer snapshot with
the same fingerprint; migrated-agent fresh-prompt fallback is silent on a corrupt
`run.json`; deleting a workspace SQLite row can orphan its Fly volume; worker tokens
(`rdrw_*`) and Rudder CLI tokens never expire/rotate; an expired presigned URL can hang
silently if the snapshot-url endpoint is unreachable; BYOC has no crash timeout (can sit
"running"); OAuth reconfig has no rollback; attach-during-restart replays the pre-pause
buffer; `includeRudderState` copies all `run.json` with no pruning. The cloud path is
functional but less battle-tested than the local dashboard; treat it as beta.

---

## 10. Models and effort

- `src/models.ts`: model discovery for the `/model` picker. Sources, each guarded
  so a corrupt cache degrades to a fallback rather than throwing:
  - `models.dev` cache (`readModelsDevCache`),
  - local Codex cache `~/.codex/models_cache.json` (`discoverCodexModelsLocal`),
  - Claude project models (`collectClaudeProjectModels`).
  `discoverModelOptions(backend, default)` merges these; callers fall back to
  `fallbackModelOptions`.
- `src/effort.ts`: `EffortLevel = low | medium | high | xhigh | max` and the
  per-backend mapping (Claude uses `effort`, Codex uses `reasoningEffort`).

---

## 11. Build, test, release

From `package.json`:
- `npm run build` = `tsc -p tsconfig.json` then `build-native`
  (`cargo build --release --manifest-path native/Cargo.toml`) then `copy-native`
  (copies the release binary to `dist/native/rudder-native`, `0o755`). `prepack`
  runs `build`, so publishing always rebuilds.
- `npm run check` = `tsc --noEmit` (fast typecheck).
- `npm test` = `tsc` then `node --test tests/*.test.mjs`.
- `npm run test:worker-scroll` = a focused subset of cargo tests for worker-pane
  scrollback behavior.
- Native tests: `cargo test --manifest-path native/Cargo.toml` (231 tests + one
  `#[ignore]`d live conductor harness, run with `-- --ignored --nocapture`).

`tests/` are integration tests that import from `dist/` (so build first):
`rebase-first.test.mjs` exercises merge/rebase/sync via `git.ts` + `state.ts`.

Always, before shipping a source change:
1. `npm run check` (or `npm run build`),
2. `cargo test --manifest-path native/Cargo.toml` if `native/` changed,
3. `node --test tests/*.test.mjs`,
4. rebuild before publishing so the packed `dist/` (incl.
   `dist/native/rudder-native`) is current. `prepack` does this automatically;
   `dist/` is not committed, so there is nothing to stage.

### Releasing (tag-driven, drift-proof)

Releases publish to npm from CI when a `vX.Y.Z` tag is pushed
(`.github/workflows/release.yml`), so the published version is always pinned to a
committed git tag. The workflow refuses to publish if the tag does not match
`package.json` `version`, so the repo and the registry can never drift apart (the
failure mode that left `1.0.70`–`1.0.73` on npm but never in git).

Cut a release from a clean, pushed `main`:

```bash
npm version patch          # bumps package.json + package-lock, commits, tags vX.Y.Z
git push --follow-tags     # pushes the commit and the tag; CI publishes
```

`npm publish` runs in CI via `prepack` (tsc + `cargo build --release` +
copy-native), with `--provenance` (hence the `repository` field and
`id-token: write` permission). One-time setup: add an npm "Automation" token as
the repo secret `NPM_TOKEN`.

Do NOT run `npm publish` by hand anymore. Hand-publishing is exactly what caused
the drift, and it also ties the bundled native binary to whatever machine you
publish from.

Cross-platform caveat (pre-existing, not solved by this workflow): the package
ships a single `dist/native/rudder-native` built for one platform. The release
runs on `macos-14`, so the published binary is macOS (arm64), matching what was
shipped before. Linux and Intel-mac users get a binary that will not exec; the
dashboard should fall back, but the native path is effectively macOS-arm64 only.
Fixing that properly means a per-platform build matrix publishing
`@viraatdas/rudder-<os>-<arch>` optional packages (the esbuild/swc model), which
is a separate piece of work.

---

## 12. Conventions and gotchas

- **jj is the required internal substrate.** Run isolation and merge run on jj
  (Jujutsu), colocated with git (`jj git init --colocate`) so the repo still
  externally looks like git and final output is normal git commits / a PR. jj is
  hard-required (`ensureJj` throws `MissingToolError("jj")`; `doctor`/`onboard`
  fail loudly if absent). The jj driver is `src/jj.ts` (all commands via
  `runCommand("jj", ...)`); `git.ts` keeps only the externally-git contract
  (root/branch/commit, status+diff for Hunk) plus the legacy git merge/sync
  helpers behind a `run.vcs === "git"` guard. New runs always record
  `vcs:"jj"` + `worktree.jjChangeId`. Use `exportToGit` after each merge. Global
  undo is free via `jj op restore` (`rudder undo`). jj 0.40 note: `jj rebase`
  uses `-o`/`--onto` (not `-d`); `jj git push --allow-new` is gone (plain
  `--bookmark` push handles new bookmarks) - both have fallbacks in `jj.ts`.
- **Atomic, serialized writes.** Anything that persists JSON should go through
  `writeJson`/`updateJson` so the per-path lock and unique temp names hold. Do not
  hand-roll temp-file writes for `run.json`/`config.json`.
- **Keybinding robustness on macOS.** When adding pane/global shortcuts, remember
  Option+key may arrive as a typographic char without a modifier. Prefer the
  Ctrl+W leader for new global actions, and add typographic fallbacks for any new
  Option chord.
- **The native dashboard reads `run.json` directly** and skips the TS load path,
  so it does not see the TS-side background summarizer's in-memory state. Persist
  through `saveRunRecord` and let the native side re-read.
- **`rudder-codex` wrapper.** Codex always runs through the wrapped binary
  (`src/codex-binary.ts`) so Rudder controls config/auth without mutating the
  user's global Codex install.
- **Style.** No em dashes in copy or UI strings. Avoid "massively parallel"
  framing in marketing copy.
- **`dist/` is build output, not committed** (removed from git in PR #15).
  `prepack` rebuilds it on publish; do not `git add dist/`. Forgetting to rebuild
  before publishing would ship stale JS or a stale native binary.

---

## 13. Where to start for common changes

- New CLI command: add a `case` in `src/main.ts` and the implementation in the
  matching module (`run-manager.ts` / `cloud.ts` / `auth.ts`).
- New backend behavior: `src/backends.ts` (`BackendAdapter`) + `src/codex-binary.ts`
  for Codex specifics.
- Run isolation/merge/sync behavior: `src/jj.ts` (jj substrate) + `src/run-manager.ts`
  (routing); `src/git.ts` for the externally-git surface + legacy git runs.
- Dashboard input/state/keys: `native/src/main.rs` (`App` + `impl App`).
  Rendering: `render.rs`. Terminal emulation/scroll: `pty_terminal.rs`. Output
  heuristics: `detect.rs`. Add coverage in `app_tests.rs`.
- Persistence/schema: `src/types.ts` + `src/state.ts` (+ `src/util.ts` for write
  primitives).
- Cloud: `src/cloud.ts` (client) + `cloud/src/server.ts` (control plane) +
  `cloud/worker/` (worker image).

---

## 14. Orchestrator: the DAG, the schedulers, and the conductor (v3)

Beyond single runs (sections 5–7), Rudder decomposes ONE goal into a DAG of tasks
and runs them as a fleet. This section is the map of that orchestrator.

### 14.1 The model
- A goal → a DAG of task nodes in `.rudder/graph.json`. Edges are typed: **hard**
  (consumer cannot succeed until producer MERGES), **soft** (advisory context, never
  gates — runs in parallel), **judge** (variant gates a judge on reaching review).
- Each node → a worker agent in its OWN isolated jj workspace. Because workspaces are
  isolated, two running agents NEVER collide on disk; the real risks are a future
  *merge* conflict (jj records it; the AI resolver fixes it) or *interface DRIFT*
  (isolated pieces that don't compose).
- `NodeStatus`: planned → running → review → merged (+ blocked / failed). The hard
  edge is the primary anti-drift defense: a consumer launches only after its producer
  is MERGED, so it builds on the real, landed interface.

### 14.2 Two schedulers, one brain (deliberate; deduped)
- **TUI brain** (`native/src/main.rs`): owns the plan in memory (`planned_nodes` +
  `agents`), spawns each worker's PTY **in-process** (that is what renders the live
  panes), and is the **sole dispatcher** in interactive use (`run_scheduler` →
  `next_node_to_launch` → `start_execute_task_node`). It MIRRORS its plan into
  `graph.json` one-way via `rudder __graph-mirror` (`mirror_graph`/`build_mirror_payload`).
- **Daemon** (`src/scheduler.ts`): dispatches **only headless** (`rudder board`/`serve`,
  `scheduler:true`), reading `graph.json` as source of truth and spawning detached
  `rudder __worker` subprocesses. When the TUI is up it starts the daemon
  **projector-only** (`scheduler:false`): the board serves HTTP+SSE and reflects
  `graph.json` but never launches — so there is no double-launch.
- `graph.json` is a **one-way mirror**: the TUI writes it (coalesced — the payload is
  hashed and the write skipped when unchanged), the daemon/board read it; the TUI never
  reads it back for scheduling. They are mutually-exclusive schedulers, not duplicates —
  one runtime for live panes (Rust), one for headless/web (TS).

**The scheduler: queue, gates, cadence.** `planned_nodes` IS the queue (the Todo
section); a node is "launched" when `run_scheduler` removes it and spawns a worker
(`AgentRun`, `Running`). `run_scheduler` runs on approval and on the
`SCHEDULER_TICK_INTERVAL = 8`-poll-tick cadence (~260 ms), and a node leaves the queue
only when **three gates** all pass: (1) **approval** — nothing launches while
`awaiting_approval`; (2) **readiness** — `next_node_to_launch` picks the first node
whose every hard dep is `merged` (`PlannedNode::is_ready`; a dep absent from the plan
counts satisfied, so dangling deps never deadlock; soft deps never gate); (3)
**parallelism** — `running_plan_agents()` is under `max_parallel()` (config
`orchestrator.maxParallel`, default 1000). At most `MAX_LAUNCH_PER_TICK = 2` start per
pass (each launch synchronously shells a jj-workspace setup, so launches are spread to
keep the UI responsive); readiness is recomputed before each pick. `next_node_to_launch`
is a **pure decision** (no side effects), which is why scheduling is deterministic and
unit-testable without spawning PTYs. A merge flips a node to `Merged`, which satisfies
its children's hard deps so they launch on the next tick — that ripple is how the DAG
flows. Stopping a node (`stop_agent_at`, `x`) keeps its jj workspace, frees its slot
(`Stopped` is not counted), and never enters the merged set (hard children stay blocked).

### 14.3 Readiness contract (the dedup)
A node is READY iff: it is still `planned`, every HARD parent is `merged` (a hard dep
absent from the plan counts as satisfied — permissive, never deadlock on a dangling
ref), and every JUDGE parent is `review`|`merged`. SOFT parents never gate.
- Single source of truth: **`tests/fixtures/readiness-cases.json`**, consumed by BOTH
  `app_tests::readiness_parity_fixture` (Rust, `include_str!`) and
  `tests/readiness-parity.test.mjs` (TS). Change the rule → edit the fixture once;
  both suites fail until both implementations agree. Judge gating is exercised TS-side
  only until the TUI grows fan-out (noted in the fixture).
- Rust: `PlannedNode::is_ready` (`native/src/tasks.rs`). TS: `isReady`/`readyNodes`
  (`src/graph.ts`).

### 14.4 Lifecycle: plan mode → ask → DAG → approve → conduct (headless, Rudder-owned)
The planner is HEADLESS and fully Rudder-owned: there is no interactive plan-mode TUI. The
user only ever talks to Rudder; the planner can never go off and implement on its own.

1. **Plan (research).** Type a goal → `start_rudder_plan_task` spawns the
   `AgentMode::RudderPlan` planner (`claude -p --output-format stream-json` read-only /
   `codex exec --json`), streamed live in the orchestrator command-center via `plan_stream`
   + `render_orchestrator`. It researches read-only and CANNOT implement (inspection-only
   tools). The pinned row is LABELED as the model's **plan mode** ("plan mode · Claude Code
   · researching the plan").
2. **Ask (hard gate on the first turn).** The planner prompt (`rudder_plan_prompt`) tells it
   to ALWAYS ask 1–4 clarifying/confirmation questions in a
   `RUDDER_QUESTIONS_START..END` block and STOP without a DAG on the first turn. Rudder also
   enforces this in code: `planner_question_round_done` starts false for a fresh plan,
   `maybe_detect_plan_ready` refuses streaming DAG capture before it is true, and
   `evaluate_completed_plan` pauses instead of queuing a first-turn DAG if the model skipped
   the question. If the model did not emit a question block, Rudder supplies a deterministic
   fallback question via `planner_questions_or_forced`. The pane shows a bold
   "❓ The planner needs your input" header + the questions as a NUMBERED list, and the task
   box hint becomes "type your ANSWER here". The user's free-text answer routes through
   `planner_awaiting_input()` → `refine_plan`, which RESUMES the same session with the
   clarification-answer framing (`build_clarification_answer_followup`, distinct from the
   refine framing) and sets `planner_question_round_done = true`. The planner then emits the
   DAG (or asks once more).
3. **DAG ready.** The instant a `RUDDER_PLAN_TASKS` block parses, the pane flips to the
   **pinned DAG** (`orchestrator_phase` Planning → PlanReady) and `awaiting_approval` holds.
   CAPTURE ROBUSTNESS: if the live PTY stream truncated a large block, `evaluate_completed_plan`
   falls back to the backend's authoritative session record (Claude's `~/.claude/projects`
   transcript via `claude_transcript_final_text`, or Codex's `~/.codex/sessions` rollout via
   `latest_codex_rudder_plan_output`), so a plan the model actually produced is never lost.
4. **Refine.** Type in the orchestrator pane while awaiting approval → `refine_plan` resumes
   the session and the DAG updates in place (no wipe).
5. **Approve → launch.** Empty-Enter → `approve_planned_queue` → `run_scheduler` dispatches
   ready nodes (todo → running), each in its own jj workspace. The queued plan + gate state
   persist to `.rudder/plan-queue.json`, so a mid-plan restart resumes instead of losing it.
6. **Conduct.** After launch the orchestrator stays live (not a dead view): it grows the
   DAG from completions, steers agents, and can rebase. Conductor actions are **autonomous
   (no confirm)** but **visible** (`activity_log`) and **undoable** (jj op-log).

### 14.4.1 Planner product decisions (canonical — do not re-litigate without cause)
The planner UX went through several iterations; these are the settled decisions and WHY.
- **Headless, not an interactive TUI.** An earlier "Path B" interactive front-end
  (`AgentMode::PlanFront`, a raw `claude --permission-mode plan` PTY the user typed into)
  was tried (1.4.0–1.5.1) and RETIRED (1.6.0). Its native ExitPlanMode menu handed control
  to a single in-process agent and bypassed the fleet, the user ended up talking to Claude
  instead of Rudder, and Rudder could only seize control via a racy transcript side-channel.
  Do NOT reintroduce an interactive plan-mode TUI.
- **Labeled "plan mode".** The headless decomposer is presented as the model's plan mode
  (the pinned row + header show "plan mode · Claude Code/Codex · …") because that is what it
  is to the user: the model planning read-only. This is a deliberate product framing.
- **Hard first question gate.** The planner asks first (step 2) rather than silently
  assuming, including for trivial or fully specified requests. This is no longer only
  prompt-driven: Rudder refuses to capture the first DAG until the user has answered one
  question round. The tradeoff is intentional: every fresh plan pauses once, but the "does it
  ask?" behavior is deterministic.
- **Free-text answers, not a selectable widget.** Questions render as a numbered list and
  the user answers in the task box with free text (e.g. "1: 6 months, 2: reuse"). Chosen over
  forcing the headless model to invent multiple-choice options, because free text is more
  flexible; the trade-off is it is not the arrow-select widget native plan mode shows.
- **One Enter to launch after the DAG.** The DAG is shown for a single explicit approval
  (empty-Enter) before the fleet runs; refining is just typing while it is shown.
- **Authoritative-record fallback over stream-only capture.** Three capture attempts
  (overlap splice 1.7.1, result-arm unification 1.8.0, transcript fallback 1.10.x); the
  transcript fallback is the robust one because it reads the backend's own session log and
  does not depend on the lossy live PTY stream.

### 14.5 Conductor capabilities (BUILT vs PLANNED)
- **Auto-expand from completion** (BUILT). A finished worker reports via **`rudder
  done`**, and `maybe_ingest_worker_followups` → `ingest_worker_followups` →
  `apply_worker_followups` grows the DAG from its recommended follow-ups. The report
  travels back over **three channels of decreasing robustness**, read in order so a
  real interactive Claude/Codex agent's report is not lost to terminal rendering:
  1. **Sidecar file (authoritative).** The launcher sets `RUDDER_DONE_FILE =
     <workspace>/.rudder/done/<node>.json` on the worker; `rudder done` writes the JSON
     note there (atomic temp+rename, `surfaces.writeCompletionNoteFile`). The conductor
     reads it straight off disk — it never passes through the agent's TUI, so it
     survives any boxing/truncation/wrapping. Keyed by node id; under the gitignored
     `.rudder/` so jj never merges it.
  2. **PTY-scrape (fallback).** `parse_worker_done_block` scans the worker's scrollback
     for the last `RUDDER_DONE_START..END` block (ANSI-stripped). The tail is flushed
     before reading so an unfocused worker that printed-and-exited isn't missed.
  3. **Haiku backstop (final).** If neither channel yielded structured follow-ups —
     including a FREEFORM prose report (`{summary: raw}` with no `followups` field) —
     `spawn_completion_backstop` runs a one-shot haiku summarizer over the worker's
     `jj_diff_text` (≤16k chars) to reconstruct the note, feeding it the agent's own
     prose. Cleanly-Done workers only (a Failed/Stopped worker's note is read but not
     backstopped, since its half-finished diff invites noise). The run is held
     `pending` (not ingested) until the summarizer returns, so it is never re-spawned.
  An explicit empty `followups: []` is TRUSTED (no backstop). Guards in
  `apply_worker_followups`: dedupe by normalized title (`followup_title_exists` checks
  queued + running), `MAX_FOLLOWUP_DEPTH = 3` via `followup_gen`, `MAX_PLAN_TASKS = 100`
  cap, soft-by-default deps (hard only with explicit deps), `scope:"out"`
  case-insensitive and recorded-not-injected. The "ingested" ledger
  (`followups_ingested`) is persisted to `.rudder/ingestion-ledger.json` and reloaded
  on startup so a restart never re-ingests/re-summarizes a handled worker; re-goaling a
  worker clears its entry so its NEXT report is ingested. CRITICAL ORDERING: ingest runs
  BEFORE `maybe_auto_merge` in the poll loop — a candidate is only ingested while still
  `Done`, and auto-merge flips `Done → Merged`, so merging first would drop follow-ups
  under `/automerge`. Every grow is logged to `activity_log`.
- **Reconcile** (BUILT). Typing a task post-launch injects ONE node
  (`reconcile_injection` → `evaluate_completed_reconcile`).
- **Steering** (BUILT: `stop_agent_at` wired to `x`; `regoal_agent_at`/`live_inject_at`
  are the toolkit the autonomous drift-fix uses and chat routing will use):
  re-goal-via-session-resume, live-inject, stop. (PLANNED: edge promote/demote,
  extract-shared-node, drop, reorder.)
- **Drift detection + fix** (BUILT). `maybe_handle_drift` (poll loop, ~5s throttle)
  predicts cross-agent collisions: agents are isolated so the signal is two RUNNING
  workers modifying the SAME file (`jj_touched_files` = `jj diff --name-only`).
  **Product decision — AUTONOMOUS, no confirm** (the user chose this; it is consistent
  with the no-confirm conductor bargain): on a newly-detected collision the conductor
  **live-injects a coordination note** into the later-launched agent so it adapts
  in-flight (integrate on top, coordinate via DECISIONS.md) — the LEAST-disruptive
  autonomous fix (no restart, no stop), with the merge-time AI resolver as the
  backstop if they still conflict. Each pair is nudged once (`surfaced_overlaps`) and
  every action is logged to `activity_log`. (Semantic/interface drift via an LLM
  classifier is a gated future enhancement; file-overlap is the shipped heuristic.)
- **Plan-rebase** (BUILT). A typed message while CONDUCTING is classified by
  `classify_new_direction` → `is_structural_direction` (replacement verbs like
  "instead/scrap/rewrite/pivot", OR a message that references a MAJORITY of node
  titles). ADDITIVE → reconcile one node; STRUCTURAL → `start_plan_rebase`, which
  RESUMES the orchestrator session with `build_rebase_request` (states the three zones)
  and sets `rebasing`. `maybe_detect_plan_ready` routes the revised block to
  `evaluate_completed_rebase`, which diffs it against the live zones with the pure
  `diff_plan` (id-join, title-overlap fallback) and applies build-forward, AUTONOMOUSLY:
  **MERGED = baseline** (untouched, referenced as satisfied deps), **RUNNING =
  keep / re-goal (objective changed) / stop (dropped, workspace kept)**, **TODO =
  replaced**. `maybe_auto_merge` is suppressed while `rebasing` so the zones stay stable.
  The diff is logged to `activity_log` (`+N added · M re-goaled · K stopped · J kept`);
  undo rides the jj op-log. A planner that emits no block leaves the current plan intact.

### 14.6 Coordination surfaces
- **`DECISIONS.md`** — jj-tracked, agent-authored shared decisions log. ONE canonical
  entry format (`renderDecisionEntry` in `src/surfaces.ts`): a `## title` heading, labeled
  body lines (**What** / **Did** / **Interfaces** / **Follow-ups** / optional **Why**), and
  a `- **By:** owner · <iso>` footer — uniform and scannable. Three writers, all that
  format: `rudder remember` → `appendDecision`; `rudder done` → `appendCompletionNote`
  (worker completion notes); and the CONDUCTOR → `append_conductor_decision`
  (`native/src/gitio.rs`, via `App::record_decision`) for plan-approval / follow-up
  depth+size-cap deferrals, so its plan/steer reasoning is durable and visible to the
  fleet, not just in the in-pane `activity_log`. Workers re-read it before each step (the
  propagation rules); the board parses it for the memory view (`parseDecisions` in
  `src/board/daemon.ts`, which reads the `## ` blocks and still tolerates legacy bullets).
- **`RUDDER.md`** — read-only, orchestrator-owned, `freshness:`-stamped projection of
  the plan/status that workers re-read (`renderLiveRudderMd`). Git-excluded.
- **`rudder done [--node <id>] '<json>'`** — `{summary, interfaces, followups:[{title,
  why, scope}]}` from stdin or args. `parseCompletionNoteArg` (`src/surfaces.ts`)
  accepts a bare object, a fenced ```json block, and falls back to `{summary: raw}` for
  prose/arrays/primitives. Writes ALL THREE completion channels: the sidecar file at
  `$RUDDER_DONE_FILE` (authoritative), a human bullet in `DECISIONS.md`, and the
  `RUDDER_DONE` stdout block (legacy scrape). Records only; no jj. See §14.5 for how the
  conductor consumes them.

### 14.7 Merge model
Manual by default (`m`/`M`); a hard-dep child launches only once its parent reaches
`Merged`. `/automerge` = hands-free. Clean merge is mechanical jj (`jj new`); a
conflict spawns the AI resolver (`start_conflict_resolution_agent` →
`finalize_merge_resolvers`, jj-worded prompt). Undo via `rudder undo` (op-log). See
section 5 / `src/jj.ts` for the run-level merge plumbing.

### 14.8 Caps
`MAX_PLAN_TASKS = 100` (per-plan parse cap AND the auto-expand backstop; overflow is
surfaced, never silently truncated). `MAX_FOLLOWUP_DEPTH = 3`. Concurrency:
`max_parallel()` (config `orchestrator.maxParallel`, default 1000), `MAX_LAUNCH_PER_TICK`.

### 14.9 Status
Built and shipped: cap=100; the `rudder done` completion report over three channels
(sidecar file, PTY scrape, haiku diff-backstop) with a persisted ingest ledger;
readiness dedup; auto-expand + activity log; steering primitives (re-goal/inject/stop,
`x`); autonomous drift handling (file-overlap to coordination nudge); and plan-rebase
(structural-vs-additive classify to build-forward diff/apply, §14.5). The conductor
loop (plan, approve, then CONDUCT: auto-expand, steer, drift, rebase) is functionally
complete and adversarially hardened across the completion pathway (freeform recovery,
ingest-before-merge ordering, restart durability via the ledger). Planned (approved v3
plan): an explicit `OrchestratorMode` enum/header (behaviour already derives from
`refining`/`rebasing`/`awaiting_approval`), and the remaining DAG-edit steering (edge
promote/demote, extract-shared, reorder).

### 14.10 Product decisions (the durable "why")
Recorded so future contributors do not relitigate them:
- **Autonomy without a confirm gate.** The conductor acts unilaterally (auto-expand,
  drift-fix, plan-rebase, merges). The bargain is VISIBLE (every action lands in
  `activity_log`) and UNDOABLE (jj op-log via `rudder undo`), not gated. The only
  explicit-intent action is reverting already-MERGED work (build-forward).
- **Worktree isolation.** Every run gets its own jj workspace, never nested in the repo,
  so concurrent agents never collide live; the only real cross-agent risks are a future
  merge conflict (jj records it, the AI resolver fixes it) or interface drift (the hard
  edge is the guard).
- **TUI-primary scheduler, daemon projector.** The Rust TUI is the sole dispatcher in
  interactive use and hosts the PTYs in-process; the daemon runs projector-only
  (`scheduler:false`) while the TUI is live, so there is never a double-launch. One
  one-way, coalesced `graph.json` mirror; the TUI never reads it back.
- **Determinism by construction.** Scheduling, merge, and lifecycle are a pure state
  machine over the DAG (`next_node_to_launch` has no side effects); the LLM
  non-determinism is quarantined in isolated workspaces and rejoins only at explicit
  gates. Readiness is a single shared spec (`tests/fixtures/readiness-cases.json`),
  parity-tested across Rust and TS.
- **Soft vs hard edges.** Hard edges (wait for MERGE) are the minimal blocking set; soft
  edges are context-only so siblings run in parallel.
- **Graceful completion capture.** Three channels plus a diff-backstop so even a silent
  or prose-only agent advances the plan; an explicit empty follow-up list is trusted.
- **Thin theme.** Emphasis by COLOR, not weight (uniform font). No em dashes in UI or
  copy; no "massively parallel" framing in marketing.
- **jj is the substrate.** Internally jj (Jujutsu), externally git; global undo via
  `jj op restore`. Never reintroduce the removed "R restart" agent feature.
