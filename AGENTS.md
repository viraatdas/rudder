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

Release rule for pushes: when pushing Rudder product changes to `origin/main`,
ship them as an npm versioned release unless the user explicitly asks for a
non-release push. Run the relevant tests, commit the change, run `npm version
patch`/`minor`/`major` as appropriate, then push `main` and the created tag.
After pushing, verify the GitHub release workflow and `npm view
@viraatdas/rudder version` so the user knows which version contains the fix.

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
│   ├── src/detect.rs      worker-output heuristics for permission/prompt UX (not completion)
│   ├── src/signals.rs     official completion signals (Claude Stop hook / Codex notify) — PRIMARY
│   ├── src/models.rs      model/effort tables, model picker, suggestion ranking
│   ├── src/launch.rs      agent launch/resume command building, review-all runs
│   ├── src/tasks.rs       prompt construction, task summaries, rudder-plan parsing
│   ├── src/gitio.rs       git/worktree/run-record persistence + fs helpers
│   ├── src/cloudio.rs     cloud command plumbing + Codex session-log scanning
│   ├── src/usage.rs       token-usage accounting, pricing, date helpers
│   ├── src/lifecycle.rs   integration evidence + final repository-check discovery
│   ├── src/config.rs      ~/.rudder config + model defaults + update notices
│   ├── src/textedit.rs    input editing (drafts, history, cursor/word ops)
│   ├── src/keys.rs        key/mouse event -> terminal byte encoding
│   ├── src/plan_stream.rs streaming planner JSONL -> live transcript + plan capture
│   ├── src/theme.rs       color palette (paper/ink/accent + status colors)
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
 native dashboard   native Rust app (rudder-native)
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
  `--non-interactive`, `--cwd`, `--repo`, `--run`, `--backend`, and `--model`.
- `main()` flow:
  - Before user-facing command dispatch, `autoUpdateAndRerunIfNeeded` checks npm
    and runs `npm install -g @viraatdas/rudder@<latest>` when this global install
    is stale, then re-runs the original command with `RUDDER_SKIP_AUTO_UPDATE=1`.
    Internal `__*` commands, `rudder done` worker callbacks, local source checkouts,
    CI, npx temp installs, and `RUDDER_DISABLE_AUTO_UPDATE`/
    `RUDDER_DISABLE_UPDATE_CHECK` skip this.
  - `--version`/`version`: print version (after auto-update if applicable).
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
`native/target/release/rudder-native`. There is no tmux or Ink fallback; if the
native binary is unavailable, Rudder reports that directly.

---

## 5. Run lifecycle (the core)

Implemented in `src/run-manager.ts`.

1. **Start** (`startRun`):
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
     `merge-conflict`. A clean integration moves the target bookmark to the merge
     change, requires `exportToGit` (`jj git export`) to succeed, and records the
     exported Git commit plus whether `origin/<bookmark>` contains it. A resolver
     finalizes that existing merge change instead of creating another merge on top.
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
  orphaned | migrated | merge-conflict | merged`. Native also persists a
  `lifecyclePhase` (`working`, `verifying`, `integrating`, `resolving`,
  `merged-locally`, `pushed`, `orphaned`, or `cloud-owned`). Holds
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
`run_scheduler`, `maybe_ingest_worker_followups`, `integrate_ready_plan_nodes`,
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

**Temporary perf diagnostics.** During the worker/review scroll-latency pass, the native
TUI writes default-on NDJSON timing logs to `~/.rudder/native-perf.ndjson` unless
`RUDDER_NATIVE_PERF=0`. This logs scroll routing, queued event drain counts, `poll_agents`,
PTY drain/parse, worker/review line rendering, `terminal.draw`, and frame totals. Logs
rotate at 10MB, keep three rotated files, and startup deletes perf logs older than three
days. Treat this as temporary instrumentation to remove or make opt-in after the scroll
performance pass.

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

**Cross-referencing the panes (v2.4.0).** Plan node ids (`n0`, `n1`, …) are the
join key everywhere: the orchestrator DAG row shows `n2 <title>`, the matching
worker's agents-pane status line shows `run n2`, and the worker pane title shows
`worker · n2 · <task summary>`. A finished-but-unmerged workspace run reads
`done · needs merge` (`agent_awaits_merge`, render.rs) because a bare "done"
misled users into thinking dependents would launch — only `Merged` unblocks.

**Mouse hit-testing (v2.4.0).** Clicks resolve through `app.agent_row_map`, a
row→agent-index map recorded from the ACTUALLY rendered frame (a thread-local
span recorder in `render_agents`, harvested per frame). This replaced a
hardcoded "agents start at row 12" offset that silently broke whenever the
header grew. The map is cleared on every `agents` Vec insert/remove so a
same-tick click can never resolve to a shifted neighbor.

**Notices and prompts (v2.6.0).** The transient notice line is severity-styled
by content (`notice_style` in render.rs: errors red, pending confirmations
amber, routine status muted), and `Esc` dismisses it from any pane without
consuming Esc's pane action. Merge/conflict/cloud confirmations are modals with
accent-colored key hints; the merge modal carries the auto-commit caution
itself (no parallel notice), the conflict modal caps its file list at 8 with an
overflow line, and the delete confirm names the agent.

### Keybindings (the input model in `handle_key`)
Order of handling matters; the early returns gate everything else.
- **Ctrl+C / q**: quit, guarded by `confirm_or_quit` when agents are still
  running (first press asks; a second `q`/Ctrl+C confirms). Every pane's
  `q` routes through the same guard — `q` used to quit instantly.
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
  Option+3=`£` (U+00A3), Option+v=`√` (U+221A).
- Otherwise the key is dispatched to the focused pane's handler.

### Orchestrator skills
The bottom task pane remains the primary input surface for fresh requests,
slash commands, plan refinements, and empty-Enter approval. When the interactive
Claude orchestrator is running, it can also edit the DAG in `RUDDER.md` and use
generated project skills under `.claude/skills/rudder-*` for dashboard actions
(`model`, `main`, `goal`, `usage`, `cloud/login`, `review-all`, `merge-all`,
`plan/run/ask`, and DAG editing/approval). Rudder consumes the
corresponding one-shot `RUDDER_*` control markers from `RUDDER.md`.

### Review and merge-all
`v` opens a review pane showing the run's `jj diff` (`ensure_review_diff`). The old
Hunk (`hunk diff --watch`) integration was removed in favor of native jj diff
inspection. `R` (review-all) creates an aggregate Codex agent over all completed
jj workspaces; `M` (merge-all) opens a confirmation to merge them. Review-all creates
a jj aggregate change, combines each source revision into it, and runs a dedicated
agent whose task is a `/review` over the combined diff. Native no longer creates or
integrates Git worktree branches.

### Tests
`native/src/app_tests.rs` holds the dashboard tests (320, plus one `#[ignore]`d live
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
(AES256, 1-hour presigned URLs). Ordinary snapshots deliberately EXCLUDE secrets and
bulk: `.ssh`, `.gnupg`, keychains, `.env*`, key files, and
`.cache`/`node_modules`/worktrees. Explicit multi-agent `cloud workspace attach`
migration is the exception for project `.env*` files: it copies repo and nested-package
dotenv files into the staged repo and every migrated worker workspace. On
macOS it extracts Claude OAuth tokens from the Keychain into `.claude/.credentials.json`
so the worker can authenticate. Env capture is allow-listed (`*_API_KEY`/`*_TOKEN`/
`*_SECRET` plus `RUDDER_CLOUD_ENV_VARS`) and excludes `PATH`/`HOME`/`USER`/shell/Rudder
internals.
Explicit multi-agent migration calls `captureCloudEnv(true)`, which captures every inherited
process variable except the blocklist; ordinary snapshots retain the credential/suffix
allowlist. Variables exported only inside a child agent PTY cannot flow back to the parent,
so the dotenv overlay is the durable source for those project-specific values.

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
- `native/src/models.rs` mirrors the same discovery for the dashboard picker. Codex
  rows read `~/.codex/models_cache.json` before models.dev so new account-specific
  families (for example GPT-5.6 Sol/Terra/Luna) appear immediately in Codex's own order.
- `src/effort.ts`: `EffortLevel = low | medium | high | xhigh | max` and the
  per-backend mapping (Claude uses `effort`, Codex uses `reasoningEffort`).
- **`/fast` (v2.5.0, decided):** the fast preset is the FLAGSHIP model at LOW
  effort — claude `opus(low)`, codex `gpt-5.5(low)` (`fast_model_for`,
  `native/src/models.rs`) — never a downgrade to haiku/spark. This mirrors
  Claude Code's own fast mode (Opus with faster output, not a smaller model).
  The claude CLI has no flag to launch with its native fast output mode
  pre-enabled (verified against `--help` and the settings docs; only
  `CLAUDE_CODE_DISABLE_FAST_MODE` exists), so low effort is the closest
  launchable equivalent. The preset persists via the normal model-defaults path
  and applies to NEW agents only. Small tiers stay reachable via `/model`.

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
- Native tests: `cargo test --manifest-path native/Cargo.toml` (260+ tests + one
  `#[ignore]`d live conductor harness, run with `-- --ignored --nocapture`).
- **Native TUI harness** (`app_tests.rs`): drive the REAL `App` end-to-end with no
  auth/network. `render_screen(app, w, h)` renders the actual dashboard
  (`render::render`) to a `TestBackend` and returns the screen as text, so tests assert
  on what the USER SEES. `write_fake_bin` + `RUDDER_CLAUDE_BIN`/`RUDDER_CODEX_BIN`
  (`launch.rs` `claude_program()`/`codex_program()`) inject a fake backend: a planner
  fake emits a canned decomposer stream (a `result` event whose text streams the
  RUDDER_PLAN_TASKS block) so the full planner→parse→DAG→render path runs
  deterministically; a worker fake writes files + exits (process-exit completion). `env_guard()` serializes tests
  that mutate the process-global `RUDDER_*_BIN`. Pattern: build `App`, point a fake bin,
  `start_rudder_plan_task` / feed keys, loop `poll_agents()`, then `render_screen` +
  assert. See `tui_harness_drives_planner_to_dag_and_renders_it` and
  `tui_harness_renders_the_plan_mode_card_when_the_planner_asks`.

`tests/` are integration tests that import from `dist/` (so build first):
`rebase-first.test.mjs` exercises merge/rebase/sync via `git.ts` + `state.ts`.
`board-steer.test.mjs` covers both web-control owners: projector requests become
durable native inbox entries, and a real standalone board redirects a delayed fake
worker end-to-end before returning the same task/workspace to Review.
`e2e-orchestrator.test.mjs` is the WHOLE-WORKFLOW test: it drives the real
`rudder plan` + `rudder board` CLIs against a throwaway git+jj repo and verifies the
DAG drains end-to-end (schedule → isolated jj workspace → worker → verify → jj merge →
unblock dependents), covering both a hard-edge (serialized) and a parallel/fan-in DAG.
It is deterministic via TEST-ONLY env hooks baked into the production code:
`RUDDER_FAKE_MODEL_OUTPUT` (planner returns a canned plan block, see `callTextModel`),
`RUDDER_FAKE_BACKEND=1` (each worker applies `[[FAKE_FILE:..]]` edits from the node prompt
and exits 0, see `getBackend`/`fakeBackend`), and `RUDDER_AUTO_STEER_DELAY_MS` (shrinks the
post-pass steering wait). No auth or real model needed. Note: the `cloud/` subproject has its
own build — `cloud-relay`/`slack` tests need `npm --prefix cloud install` +
`npm --prefix cloud run build` first, and self-skip (rather than error) when
those artifacts are absent so `npm test` stays green in a fresh worktree.

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
  panes), and is the **sole worker scheduler** in interactive use (`run_scheduler` →
  `next_node_to_launch` → `start_execute_task_node`). It MIRRORS its plan into
  `graph.json` one-way via `rudder __graph-mirror` (`mirror_graph`/`build_mirror_payload`).
- **Daemon** (`src/scheduler.ts`): dispatches **only headless** (`rudder board`/`serve`,
  `scheduler:true`), reading `graph.json` as source of truth and spawning detached
  `rudder __worker` subprocesses. When the TUI is up it starts the daemon
  **projector-only** (`scheduler:false`): the board serves HTTP+SSE and reflects
  `graph.json` but never launches — so there is no double-launch.
- **Web control follows the owner.** The board daemon receives an explicit
  `scheduler | projector` control mode. In scheduler mode, task updates call
  `continueRun` under the scheduler lock (a running pass is redirected; Review is
  resumed) and a per-attempt id prevents the superseded worker from writing stale
  terminal state. In projector mode, updates and `+ Task` are queued in
  `.rudder/steer/`; the TUI consumes them through the same paths as worker PTY and
  task-pane input. Never mutate `graph.json` from a projector-mode browser action:
  the TUI deliberately does not read its mirror back.
- `graph.json` is a **one-way mirror**: the TUI writes it (coalesced — the payload is
  hashed and the write skipped when unchanged), the daemon/board read it; the TUI never
  reads it back for scheduling. They are mutually-exclusive schedulers, not duplicates —
  one runtime for live panes (Rust), one for headless/web (TS).

**The scheduler: queue, ledger, gates, cadence.** `planned_nodes` is the pending queue
(the Todo section), while `plan_launched_node_ids` and `plan_merged_node_ids` are the
durable at-most-once facts for the active plan. A launch records the node id and persists
the shrunken queue in one snapshot *before* spawning the worker. Startup reconciles any
older queue snapshot against persisted `AgentRun.node_id` records, so the crash window
between run creation and queue persistence cannot relaunch a node. Presentation cleanup
may delete merged rows without deleting the plan's completion facts. `run_scheduler` runs
on approval and on the
`SCHEDULER_TICK_INTERVAL = 8`-poll-tick cadence (~260 ms), and a node leaves the queue
only when **three gates** all pass: (1) **approval** — nothing launches while
`awaiting_approval`; (2) **readiness** — `next_node_to_launch` picks the first node
whose every hard dep is `merged` (`PlannedNode::is_ready`; a dep absent from the plan
counts satisfied, so dangling deps never deadlock; soft deps never gate); (3)
**parallelism** — `running_plan_agents()` is under `max_parallel()` (config
`orchestrator.maxParallel`, default 1000). At most `MAX_LAUNCH_PER_TICK = 2` start per
pass (each launch synchronously shells a jj-workspace setup, so launches are spread to
keep the UI responsive); readiness is recomputed before each pick. `next_node_to_launch`
is a **pure decision** and rejects any id already present in the durable launch ledger or
an agent row, which is why scheduling is deterministic and unit-testable without spawning
PTYs. A merge flips a node to `Merged`, which satisfies
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
   fallback question via `planner_questions_or_forced`. The pane renders the planner's
   inspection transcript first, then a bold "❓ The planner needs your input" header + the
   questions as a NUMBERED list anchored at the BOTTOM of the body (the body sticks to the
   bottom, so the questions stay in view above a long transcript instead of scrolling off).
   The raw `RUDDER_QUESTIONS_START..END` block is stripped from that transcript
   (`push_transcript_lines(strip_questions)`) so the questions are not shown twice. The user's
   free-text answer routes through
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
4. **Refine.** Type in the task pane while awaiting approval (or in the focused
   headless orchestrator chat) → `refine_plan` resumes the session and the DAG
   updates in place (no wipe).
5. **Approve → launch.** Empty-Enter → `approve_planned_queue` → `run_scheduler` dispatches
   ready nodes (todo → running), each in its own jj workspace. The queued plan + gate state
   plus launched/merged node ledgers persist to `.rudder/plan-queue.json`, so a mid-plan
   restart resumes without losing or duplicating work. Interactive plan-file capture is
   explicitly armed only by a fresh planning turn; an empty queue/all-merged fleet never
   causes an old `RUDDER_PLAN_TASKS` block to be interpreted as a new request.
   **Worker context at launch:** each worker gets its own checkout (hard-dep parents already
   merged into its base), `RUDDER.md` (the live agent roster) + `DECISIONS.md`, and a launch
   prompt carrying `Objective:` + `Done when:`, the original request, the node prompt, and a
   **`Depends on:` block** (`App::dependency_context`) naming each hard/soft parent by title +
   its `rudder done` interface summary so the worker builds on prerequisites instead of
   reimplementing them.
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
- **INTERACTIVE orchestrator — now the DEFAULT (1.20.0; PR #17/issue #16).** The Claude
  orchestrator is a normal interactive Claude Code PTY you converse with (real plan-mode feel
  + visible thinking, `launch.rs interactive_orchestrator()` now defaults TRUE; set
  `RUDDER_INTERACTIVE_ORCHESTRATOR=0` to opt back into the headless `claude -p` decomposer).
  Codex orchestrators stay headless regardless. Validated against real `claude` (it writes the
  plan file + emits the marker). It uses the orchestrator system prompt
  (`tasks.rs orchestrator_system_prompt`). It writes the DAG to `RUDDER.md`
  outside Rudder's generated block; `App::maybe_capture_orchestrator_plan` reads it into
  `planned_nodes`/`awaiting_approval`; `render_interactive_orchestrator` shows a DAG pane
  ABOVE the live PTY; keys forward to the PTY (converse). Unlike the retired PlanFront, the
  CC writes a DAG file so the DAG actually builds + renders. **SELF-LAUNCH (hardened, 1.21.0):**
  after the user approves in chat, the orchestrator signals approval and Rudder launches.
  `App::scan_orchestrator_markers` (poll_agents) checks, in order: (1) PRIMARY — the orchestrator
  WROTE a `RUDDER_APPROVE_PLAN` line INTO its plan file (a structured Write, exact content, no
  TUI-rendering fragility); (2) FALLBACK — the marker printed in the orchestrator PTY
  (`output_has_approve_marker`, exact full-line match after ANSI + markdown strip). Either calls
  `approve_planned_queue()` → launch; no dedup ledger needed because that fn is idempotent
  (early-returns once `awaiting_approval` flips false). The prompt tells the orchestrator to WRITE
  the marker into RUDDER.md (and may also print it), only after explicit user approval, never
  preemptively, quoting as `RUDDER_APPROVE_PLAN_TEMPLATE`. **APPROVE → GRAPH.JSON → LIVE CONDUCTOR:** on approval `approve_planned_queue`
  (1) mirrors the full DAG into `.rudder/graph.json` (`mirror_graph`), (2) launches the worker
  fleet (`run_scheduler`), then (3) keeps the interactive orchestrator PTY alive as a high-level
  conductor. The orchestrator still must not implement product code itself; its post-approval job is
  to talk with the user, inspect state read-only, and write one-shot `RUDDER_*` control markers into
  `RUDDER.md`. The task pane forwards messages to the live orchestrator while a plan is active, so
  high-level requests route through the conductor instead of bypassing it. The DAG pane survives the
  scheduler draining `planned_nodes` via `orchestrator_dag_tasks`, which reconstructs already-launched
  nodes from their `node_id` agents (deduped per id, latest agent wins) so the tree stays whole with
  live status badges from `orchestrator_task_status`. NOTE: `interactive_orchestrator()` is read ONCE into the `App.interactive_orchestrator`
  field at construction; render/poll/key read the FIELD (not the process-global env) so parallel
  tests don't race — tests set the field directly (the headless-view render helper pins it false).
  The bottom task bar remains available; the interactive orchestrator skills are an additional
  control path generated under `.claude/skills/rudder-*`, with one-shot `RUDDER_*` control
  markers consumed from `RUDDER.md`. Marker controls include add/replan (`RUDDER_ADD_TASK`,
  `RUDDER_REPLAN`), per-worker control (`RUDDER_MERGE`, `RUDDER_STOP`, `RUDDER_RESUME`,
  `RUDDER_REGOAL`, `RUDDER_INJECT`), and broad actions (`RUDDER_REVIEW_ALL`,
  `RUDDER_MERGE_ALL`, `RUDDER_CLOUD_MIGRATE`). `RUDDER_CLOUD_MIGRATE` freezes live
  isolated worker PTYs while leaving their records `running`, then launches `rudder cloud
  workspace attach`; this avoids local/cloud duplicate execution and keeps the records
  eligible for `findMigrationCandidates`.

  `RUDDER_RESUME <node-or-run-id> <claude|codex> <model> [effort] [direction]` is the
  provider/model-aware continuation primitive. It preserves the run's jj workspace and
  identity. Same-provider retargets resume the existing backend session with the new model;
  cross-provider retargets clear the provider-specific session id and start a fresh worker
  with a handoff that tells it to inspect and continue the existing diff. The generated
  `rudder-worker-control` skill maps natural-language pause/resume/model-switch requests to
  ordered `RUDDER_STOP` + `RUDDER_RESUME` markers for each matching live node.

### 14.4b Per-agent done/idle detection: official signals only (`native/src/signals.rs`)
The native TUI runs workers as INTERACTIVE `claude`/`codex` in a PTY (they idle between
turns instead of exiting), so "is this turn done?" can't use process-exit. The robust answer
is each backend's OWN lifecycle signal, NOT scraping its TUI chrome:
- **Claude Code:** a `Stop` hook (fires when the turn ends) + a `Notification` hook matched to
  `idle_prompt` (paused for input), injected via `claude --settings <file>`.
- **Codex:** the `notify` program (fires `agent-turn-complete`), injected via
  `-c notify=["<script>"]` (replacing the baked `notify=[]`). Codex's newer `[hooks]` are
  trust-gated, so unusable for a headless child; `notify` needs no trust.

Both write `<rudder_home>/signals/<run_id>.json` = `{"state":"done"|"input"}`. The poll loop's
interactive arm (`try_wait` Ok(None)) reads this as AUTHORITATIVE: `done` → `mark_run_done`
(one-shot, cleared on consume so a later turn re-fires); `input` → `needs_user_input`. The
`detect.rs` still recognizes permission and user-input prompts for ergonomics, but terminal
chrome is never a completion source. Missing hook wiring fails visibly instead of leaving a
worker stuck or guessing that a mid-tool lull is done. `augment_worker_command`
wires this at the worker spawn sites (Execute/Main/ReviewAll/restart); headless planner modes
(`-p` / `codex exec`) keep process-exit. Both hooks are validated live against the real CLIs
(claude 2.x `Stop`, codex `agent-turn-complete`). The daemon/DAG path (`src/backends.ts`)
already used official signals: `claude -p --output-format stream-json` and `codex exec --json`
(`turn.completed`) + process exit.

**The wiring INVARIANT (v2.5.0, learned the hard way):** every path that (re)spawns a
worker process MUST call `signals::augment_worker_command`. The per-run hook config file
persists across spawns. A relaunch that forgets the wiring is a lifecycle error, not a
different detection mode. Three paths have been bitten: the conflict-resolver respawn,
migration-resume, and re-goal. When adding a spawn path, wire it.
Signal hygiene: hook writes are ATOMIC (`printf > tmp && mv`, v2.6.x) so the poll loop
never reads a torn JSON, and `cleanup_run_signals` removes a run's three signal files on
agent delete.

### 14.4c Default task input: orchestrator by default, `/run` for one worker, `/ask` for direct one-off
Automatic local routing misclassified too many ordinary requests. A FRESH task
(no active plan) now has a deterministic contract:

- **Plain task input** goes to `start_rudder_plan_task`. It starts the interactive
  orchestrator planner (`AgentMode::RudderPlan`) and lets that model decide whether
  to answer, inspect, plan, spawn workers, merge, or ask follow-up questions through
  Rudder's normal control markers. There is no local one-off-vs-DAG router.
- **`/plan <text>`** is an explicit alias for the same orchestrator / DAG path.
  Use it when you want the command line to state the intent clearly.
- **`/run <task>`** is the explicit escape hatch for exactly one isolated worker
  with no DAG. It calls `start_execute_task_node(..., None)` and creates a normal
  **`AgentMode::Execute`** run. In a git repo, this uses the jj workspace launch
  path (`prepare_jj_workspace`), has no `node_id`, lands in Review when done, and
  is mergeable through selected `m` or `/merge-all` into the current integration
  checkout.
- **`/ask <text>`** is the explicit escape hatch for one-off work. It calls
  `start_oneoff_task`, spawning a single conversational **`AgentMode::OneOff`**
  agent in the MAIN checkout (`create_oneoff_agent`, cwd = repo root, NO jj
  worktree, NO `node_id`), with `oneoff_prompt` (no `/goal`, no `rudder done`,
  "edit directly; escalate to the planner if it's big") + bypassPermissions tools.
  Keys forward to its PTY. It is explicitly excluded from selected merge /
  merge-all / review-all, and implicitly excluded from plan integration / graph-mirror /
  scheduler by `node_id`/Execute gates; `mark_run_done` just marks it Done (no
  merge). It lives in its own **`Bucket::OneOff`** list section (`status_bucket`
  returns `OneOff` for it; leads `Bucket::ORDER`).
### 14.5 Conductor capabilities (BUILT vs PLANNED)
- **Auto-expand from completion** (BUILT). A finished worker reports via **`rudder
  done`** report, and `maybe_ingest_worker_followups` → `ingest_worker_followups` →
  `apply_worker_followups` grows the DAG from its recommended follow-ups. The report
  travels back through one machine-readable channel:
  1. **Sidecar file (authoritative).** The launcher sets `RUDDER_DONE_FILE =
     <workspace>/.rudder/done/<node>.json` on the worker; `rudder done` writes the JSON
     note there (atomic temp+rename, `surfaces.writeCompletionNoteFile`). The conductor
     reads it straight off disk — it never passes through the agent's TUI, so it
     survives any boxing/truncation/wrapping. Keyed by node id; under the gitignored
     `.rudder/` so jj never merges it.
  2. **Haiku backstop.** If no sidecar yielded structured follow-ups —
     including a FREEFORM prose report (`{summary: raw}` with no `followups` field) —
     `spawn_completion_backstop` runs a one-shot haiku summarizer over the worker's
     `jj_diff_text` (≤16k chars) to reconstruct the note, feeding it the agent's own
     prose. Cleanly-Done workers only (a Failed/Stopped worker's note is read but not
     backstopped, since its half-finished diff invites noise). The run is held
     `pending` (not ingested) until the summarizer returns, so it is never re-spawned.
  `rudder done` enriches the report; it does not determine completion. Backend lifecycle
  hooks or process exit finalize the turn automatically. An explicit empty
  `followups: []` is TRUSTED (no backstop). Guards in
  `apply_worker_followups`: dedupe by normalized title (`followup_title_exists` checks
  queued + running), `MAX_FOLLOWUP_DEPTH = 3` via `followup_gen`, `MAX_PLAN_TASKS = 100`
  cap, soft-by-default deps (hard only with explicit deps), `scope:"out"`
  case-insensitive and recorded-not-injected. The "ingested" ledger
  (`followups_ingested`) is persisted to `.rudder/ingestion-ledger.json` and reloaded
  on startup so a restart never re-ingests/re-summarizes a handled worker; re-goaling a
  worker clears its entry so its NEXT report is ingested. CRITICAL ORDERING: ingest runs
  BEFORE `integrate_ready_plan_nodes` in the poll loop — a candidate is only ingested
  while still `Done`, and integration flips `Done → Merged`, so integrating first would
  drop follow-ups. Every grow is logged to `activity_log`.
- **Reconcile** (BUILT). Typing a task post-launch injects ONE node
  (`reconcile_injection` → `evaluate_completed_reconcile`).
- **Steering** (BUILT: `stop_agent_at` wired to `x`; conductor markers route to
  `merge_agent_at`, `stop_agent_at`, `regoal_agent_at`, and `live_inject_at`):
  merge one, stop, re-goal-via-session-resume, and live-inject. (PLANNED: edge promote/demote,
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
  replaced**. Plan integration is suppressed while `rebasing` so the zones stay stable.
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
  Native dashboard context writes must mirror the same generated block to every
  known agent workspace, not only the root checkout or a pending worktree.
  The generated status sections are intentionally separate: Active means live or
  waiting, Ready means completed work awaiting review or merge, and Completed is
  terminal history.
- **`RUDDER_SHARED.md`** — gitignored local shared context for API tokens, private URLs,
  account ids, env vars, and setup details the user gives Rudder and expects every agent
  to use. It is intentionally separate from `DECISIONS.md` because it may contain
  credential values and must not enter jj/git history. Native task-bar input calls
  `capture_shared_context_from_user_input` for obvious `*_TOKEN=...` / API-key lines,
  `/share <text>` forces an append, and `rudder share "<text>"` does the same from the
  CLI. `write_rudder_context` / `writeLiveRudderMd` mirror the file into active worker
  workspaces next to `RUDDER.md`, and every worker/orchestrator prompt tells agents to
  read it when present. Do not store secret values in `RUDDER.md` or `DECISIONS.md`.
- **`rudder done [--node <id>] '<json>'`** — `{summary, interfaces, followups:[{title,
  why, scope}]}` from stdin or args. `parseCompletionNoteArg` (`src/surfaces.ts`)
  accepts a bare object, a fenced ```json block, and falls back to `{summary: raw}` for
  prose/arrays/primitives. Writes two durable completion channels: the sidecar file at
  `$RUDDER_DONE_FILE` (authoritative) and a human bullet in `DECISIONS.md`. There is no
  PTY-scrape channel. Records only; no jj. See §14.5 for how the
  conductor consumes them.

### 14.7 Merge model
Planned DAG nodes integrate automatically; manually started runs use `m`/`M`. A
hard-dep child launches only once its parent reaches `Merged`. Clean merge is
mechanical jj (`jj new`); a
conflict spawns the AI resolver (`start_conflict_resolution_agent` →
`finalize_merge_resolvers`, jj-worded prompt). Undo via `rudder undo` (op-log). See
section 5 / `src/jj.ts` for the run-level merge plumbing.

**Merge-state legibility + durability:**
- There is no auto-merge feature flag or retry-skip ledger. Planned work always
  integrates; a durable conflict state is the blocker until it is resolved.
- A CONFLICTED merge keeps the run `Done` (review bucket) because it is finished
  work awaiting integration. Same rule on
  reload: a run.json with `status:"merge-conflict"` loads as `Done`, not Stopped
  (`agent_status_from_record`).
- Integrated rows persist the jj merge change, target bookmark, exported Git commit,
  and whether `origin/<bookmark>` contains that commit. Native refreshes the remote
  containment state locally and renders `merged locally` versus `pushed`.
- Once every launched DAG node is integrated, the final gate runs `jj resolve --list`,
  `git diff --check`, discovered npm `check`/`test` scripts, and Cargo workspace tests.
  Its durable state is stored in `plan-queue.json`, rendered by the orchestrator, and
  mirrored into `RUDDER.md`; `/verify` reruns a failed gate.
- A persisted `running` row with no native PTY becomes `orphaned` on restart instead
  of being blindly relaunched. A successful fleet handoff becomes `migrated` /
  `cloud-owned`, so local restart cannot duplicate cloud execution.
- A merge REFUSES to start when the integration workspace `@` already has
  unresolved conflicts (`mergeJjRunIntoCurrentWorkspace` guard, unconditional even
  under `--allow-dirty`) — merging onto a conflicted `@` nests conflicts.
- The TUI's `rudder` CLI shell-out (`run_rudder_jj_command`) has a 120s
  timeout with kill+reap, so a hung jj can no longer freeze the dashboard forever.
- Mechanical conflicts (RUDDER.md/DECISIONS.md/RUDDER_SHARED.md, .gitignore
  union, package.json deep-merge, lockfiles) auto-resolve BEFORE any LLM resolver
  (`auto_resolve_mechanical_conflicts`).

**Serialization invariant (`withSchedulerLock`, `src/scheduler.ts`).** The daemon fires
`scheduleTick` (1s interval), `onRunTransition` (per-run `fs.watch`), and the board's
manual approve/merge endpoints — all in ONE process as un-awaited `void` calls. Every op
that reads then advances `integrationChangeId` MUST go through `withSchedulerLock(repoRoot, …)`,
which chains them onto a per-repo promise so they run one-at-a-time. Without it, two nodes
reaching `review` in the same instant get merged concurrently against the SAME integration
head, producing two sibling merges; the second `updateGraph` clobbers `integrationChangeId`
and one node is marked `merged` while its diff is orphaned off the trunk (silent data loss).
The public entry points lock; internal recursion uses the unlocked `*Core` variants to avoid
self-deadlock (the lock is NOT reentrant). This bug is invisible to unit tests and only
shows under near-simultaneous completions — it is covered by `tests/e2e-orchestrator.test.mjs`.

**Cross-process integration lock (v2.6.x).** `withSchedulerLock` serializes only one
process; a `rudder merge` CLI invocation (what the TUI shells out to) and a daemon tick
are SEPARATE processes sharing the same `@`. Both outer merge seams —
`mergeJjRunIntoCurrentWorkspace` (jj.ts) and `mergeNodeIntoIntegration` (scheduler.ts) —
therefore take `.rudder/integrate.lock` (`withIntegrationLock`, src/rudder-md.ts: mkdir
lock, 30s wait, 120s stale takeover, proceed-unlocked on timeout so a crashed holder
never deadlocks). NOT reentrant: `mergeNode` inside those seams must never re-take it.

### 14.7b Cross-process coordination and locking (the full map)
Multiple OS processes share state: the Rust TUI, each `rudder <cmd>` CLI invocation
(merge/done/share/…), the daemon, worker agent processes, and the orchestrator agent
editing files directly. The rules:
- **Atomic writes everywhere.** Every persisted JSON/state file is written temp+rename
  (TS `writeJson` in src/util.ts — unique temp per write + in-process `withPathLock`;
  Rust `write_config_atomically` / `save_native_run_record` / the ledger persisters;
  signal hooks' `printf > tmp && mv`). A reader can see an OLD file, never a torn one.
- **RUDDER.md** is read-modify-write from BOTH languages, so both take the SAME
  advisory mkdir lock `<repo>/.rudder/rudder-md.lock` (TS `withRudderMdLock`,
  Rust `acquire_rudder_md_lock` in gitio.rs; 50ms retry, 2s wait, 10s stale takeover,
  proceed-unlocked on timeout). The merged write itself is temp+rename on both sides,
  and `merge_generated_rudder_md` / `mergeGeneratedRudderMd` are PARITY
  implementations (duplicate-block collapse, orphan-marker strip, byte-stable
  re-render) — change one, change both, and keep their tests mirrored.
- **Integration into `@`** takes `.rudder/integrate.lock` (above).
- **DECISIONS.md** appends are a SINGLE `appendFile`/append call per entry
  (O_APPEND): concurrent writers can interleave entry ORDER but cannot tear an
  entry. Keep it that way — never split an entry across two appends.
- **`.rudder` ledgers** (ingestion-ledger, followup-gen, plan
  queue) are single-writer BY ASSUMPTION: one dashboard per checkout
  (`runPolicy.sameCheckout: "single-active"`). Two concurrent TUIs on one repo
  would last-writer-win each other's ledgers; that is out of contract.
- **jj** serializes its own op store; concurrent jj commands degrade to
  recoverable "concurrent operation" states, not corruption. The locks above
  exist to avoid stacking LOGICAL merges, not to protect jj internals.

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
- **TUI-primary scheduler, daemon projector.** The Rust TUI is the sole worker scheduler
  in interactive use and hosts the PTYs in-process; the daemon runs projector-only
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

---

## 15. Continual improvement loop (`rudder improve`)

Status: **BUILT** (2026-07-07) in `src/improve/`; the eval-replay tier is the one
designed-but-unbuilt stage. Full spec: `docs/continual-improvement.md`. This section is
the map an agent needs before touching it; the spec is the source of truth for stage
semantics. Do not re-derive the design from scratch.

### 15.1 Shape
A scheduled local batch loop that reads Rudder's own telemetry, mines it into ranked
friction findings, proposes fixes via headless improvement agents (`claude -p
--dangerously-skip-permissions`) in disposable git worktrees of the rudder checkout,
gates them with the repo's deterministic test suites, judges them with a three-lens
adversarial panel, and at `ship` autonomy (the default) pushes the fix to `origin/main`
with an `npm version patch` tag so the section-11 tag-driven CI publishes the release.

```
collect → mine → rank → propose (worktree + context pack) → gate (npm ci/check/
cargo test/npm test) → judge panel (refute-first, fails closed)
→ ship (rebase → npm version patch → push main + tag) → outcome check next cycles
```

### 15.2 Decisions (settled; do not relitigate without cause)
- **Scheduled batch, NOT a resident daemon.** `rudder improve run` under a launchd
  LaunchAgent (`rudder improve schedule install`, nightly 03:30). Reasons: the work is
  periodic, a laptop sleeps, batch runs are crash-isolated and resume from a watermark,
  and the existing board daemon's debuggability problems (docs/observability-plan.md
  §1.4) argue against a second resident process.
- **Auto-ship is the default** (user decision, 2026-07-07, superseding the earlier
  PR-only design): `improve.autonomy` is `observe | propose | ship(default)`. `ship`
  rebases onto origin/main, re-typechecks, `npm version patch`, and pushes main + tag
  in one non-forced push (`src/improve/ship.ts`). Guardrails: origin must match
  `improve.allowedRemote` (viraatdas/rudder), the proposing agent may never touch
  versions/tags, a failed push deletes the local tag, and a moved main wins (recorded
  as push-conflict, retried next cycle). `propose` pushes branch `improve/<id>` only.
- **Advisor pattern for judgment calls** (user decision, 2026-07-07, to conserve
  usage): mining and judging go through `src/improve/advisor.ts` — executor
  (`minerModel`/`judgeModel`, default sonnet) + advisor tool (`improve.advisorModel`,
  default `claude-fable-5`, beta `advisor-tool-2026-03-01`, advisor capped at 2048
  tokens/consult). Spend metered exactly from `usage.iterations`. Falls back to a
  plain `callTextModel` call on missing API key, invalid pair, or any advisor error;
  `advisorModel: ""` disables.
- **Refute-first judging + outcome verification.** Judges are prompted to refute and
  fail CLOSED (unparseable vote = reject; budget-exhausted vote = reject). Ship needs
  zero regression flags, 2/3 approvals, AND a correctness approval (`panelDecision` in
  judge.ts) — correctness rejections veto because an "approved but doesn't fix it"
  ship wastes a release and mutes the finding. Later cycles compare each shipped
  change's target metric before/after (≥7 days, ≥3 snapshots each side) and record
  confirmed/no-effect/regressed; a no-effect/regressed outcome LIFTS the shipped
  suppression in dedupe so the finding can resurface.
- **Hand-shipped fixes must be recorded**: `rudder improve record shipped "<title>"
  [--released vX.Y.Z]` appends the ledger entry that stops the miner re-proposing an
  issue already fixed outside the loop. The miner prompt also carries recent ledger
  history for semantic (paraphrase-proof) dedupe.
- **Redaction at collect time.** `redactTaskSummarySecrets` runs over tasks and event
  excerpts before anything reaches a model; raw transcripts are never shipped whole.
- **Hard budget.** `improve.budgetUsd` (default $5) per cycle, checked before every
  model call; over-budget findings are banked, and an unjudged proposal never ships.
  Kill switch `RUDDER_IMPROVE=0` or `improve.enabled:false`.

### 15.3 Where things live
- Module `src/improve/{index,state,collect,mine,rank,advisor,propose,gate,judge,ship,
  schedule}.ts`, dispatched as `improve` in `src/main.ts`; tests in
  `tests/improve.test.mjs` (pure logic, no model calls).
- State under `~/.rudder/improve/`: `cycle.lock` (mkdir lock, 6h stale takeover),
  `watermark.json` (per-project consumption high-water marks), `metrics.jsonl` (one
  snapshot per cycle), `ledger.jsonl` (findings/outcomes/rejection memory),
  `reports/<cycle>.md`, `logs/` (agent + gate output), `worktrees/`.
- Config: the optional `improve` block on `RudderConfig` (src/types.ts); defaults in
  `IMPROVE_DEFAULTS` (src/improve/state.ts).

### 15.4 Gotchas (learned building it)
- **Collect must read run.json RAW** (`readRunsRaw` in collect.ts), never via
  `state.ts::loadRunRecord`/`listRuns`: that load path fires the background LLM task
  summarizer, which costs a model call per record AND rewrites run.json, bumping
  `updatedAt` past the watermark so the same session is re-collected forever.
- The whole observe path is deterministic under `RUDDER_FAKE_MODEL_OUTPUT` (the
  advisor wrapper delegates to `callTextModel`, which honors the hook) — use it for
  smoke tests, never real tokens.
- `execStep` (state.ts) exists because `util.runCommand` has no timeout; gates and
  agents must never hang a nightly cycle.
- The improvement agent's `.rudder-improve-result.json` self-report is read and
  DELETED before the leftover-commit, so it never lands in history.

### 15.5 Remaining work
The eval-replay tier (spec §4.6): distill frictional sessions into replayable cases
under `~/.rudder/improve/evals/`, run candidate vs baseline (deterministic tier via
`RUDDER_FAKE_MODEL_OUTPUT`/`RUDDER_FAKE_BACKEND=1`, live tier budgeted), and feed
blind A/B deltas to the judge panel. Plus tractability feedback: outcome entries
adjusting the static weights in `rank.ts::TRACTABILITY`.
