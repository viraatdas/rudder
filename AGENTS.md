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

### Panes and focus
`FocusPane = Agents | Worker | Task`.
- **Agents**: the run list. Grouped: a `main`-branch agent section first, then
  worktree runs, then a merged section. Select with `j/k` or arrows; `Enter`
  focuses/starts; `m` merge, `d` delete, `r` rename, `u` sync, `v` review.
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
  `M` merge-all, `r` rename, `u` sync, `j/k` move, `d` delete, `q` quit, `Esc`
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
`v` opens a review pane. When `hunk diff --watch` is available Rudder opens a live
Hunk review there. `R` (review-all) creates an aggregate Codex agent over all
completed worktrees; `M` (merge-all) opens a confirmation to merge them. Review-all
spins up a dedicated agent whose task is a `/review` over the combined diff.

### Tests
`native/src/app_tests.rs` holds the dashboard tests (101; declared
`#[cfg(test)] mod app_tests;` in `main.rs`). Includes the
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

Optional hosted worker mode. The local dashboard stays the control surface; you
choose to hand a task to the cloud.

### CLI (`src/cloud.ts`)
Subcommands: `login`, `launch`, `sail`, `byoc`, `vm`/`byo-vm`, `list`/`ls`,
`status`, `logs`, `attach`, `workspace`, `onload`, `bootstrap`, `pause`,
`resume`, `stop`, `setup-github`, `setup-google`, `setup-byoc`, `setup-vm`,
`setup-fly`, `setup`, `runtime`.
- Defaults to the hosted control plane
  `https://rudder-cloud-control.fly.dev`; override with `RUDDER_CLOUD_URL`.
- `login` runs a browser/device-style flow (poll `/api/cli/login/poll`) and stores
  `CloudAuthState` in `~/.rudder/cloud.json`.
- `onload` snapshots the current Rudder workspace (repo snapshot + selected HOME
  auth/config) and uploads it (S3 presigned) so a cloud worker can continue the
  task.
- Error messages prefer the parsed server error, then the response body, then
  `<status> <statusText>` (so empty gateway responses still report a code).

### Control plane (`cloud/src/server.ts`)
A plain Node `http` server (no framework). Dependencies: `better-auth` +
`better-sqlite3` (sessions/accounts), `@aws-sdk/client-s3` +
`s3-request-presigner` (workspace snapshots), `ws` (streaming).
Route groups:
- `/api/auth/*` (better-auth)
- `/api/cli/login`, `/api/cli/login/github-token`, `/api/cli/login/poll`
- `/api/rudder/setup/{github,google}`
- `/api/rudder/sail*` (launch/list/manage cloud workers, backed by Fly Machines)
- `/api/rudder/workspace*` (onload, lookup, attach, snapshot presign)
- `/api/admin/workspace/gc`
Deploy config in `cloud/fly.toml` + `cloud/Dockerfile`.

### Worker image (`cloud/worker/`)
`Dockerfile` + `entrypoint.sh` + `supervisor.mjs`: the container that restores a
snapshot and runs the agent away from the laptop.

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
- Native tests: `cargo test --manifest-path native/Cargo.toml` (101 tests).

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
- `graph.json` is a **one-way mirror**: the TUI writes it, the daemon/board read it;
  the TUI never reads it back for scheduling. They are mutually-exclusive schedulers,
  not duplicates — one runtime for live panes (Rust), one for headless/web (TS).

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

### 14.4 Lifecycle: plan → approve → conduct
1. **Plan.** Type a goal → `start_rudder_plan_task` spawns a read-only decomposer
   (`claude -p --output-format stream-json` / `codex exec --json`). The orchestrator
   pane streams the transcript and flips to the **pinned DAG** the instant a
   `RUDDER_PLAN_TASKS` block parses (`orchestrator_phase`).
2. **Refine** (awaiting approval). Chat in the orchestrator pane → `refine_plan`
   (resumes the planner session) → the DAG updates in place, no wipe.
3. **Approve → launch.** Empty-Enter → `approve_planned_queue` → `run_scheduler`
   dispatches ready nodes (todo→running), each in its own jj workspace.
4. **Conduct.** After launch the orchestrator stays live (not a dead view): it grows
   the DAG from completions, steers agents, and can rebase. Conductor actions are
   **autonomous (no confirm)** but **visible** (`activity_log`, rendered in the
   orchestrator pane) and **undoable** (jj op-log via `rudder undo`).

### 14.5 Conductor capabilities (BUILT vs PLANNED)
- **Auto-expand from completion** (BUILT). A finished worker calls **`rudder done`**
  (it echoes a `RUDDER_DONE` block to its PTY and appends to `DECISIONS.md`); the poll
  loop's `maybe_ingest_worker_followups` → `parse_worker_done_block` →
  `apply_worker_followups` grows the DAG from the agent's recommended follow-ups
  (dedupe by title, `MAX_FOLLOWUP_DEPTH` via `followup_gen`, `MAX_PLAN_TASKS` cap,
  soft-by-default deps, `scope:"out"` recorded-not-injected). Logged to `activity_log`.
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
- **Plan-rebase** (PLANNED): a structural mid-flight change re-decomposes against the
  current repo; MERGED = baseline (build-forward, never auto-undo), RUNNING =
  keep/re-goal/stop, TODO = replace; applied as a reviewable diff.

### 14.6 Coordination surfaces
- **`DECISIONS.md`** — jj-tracked, agent-authored shared knowledge. Appended via
  `rudder remember` (a decision) or `rudder done` (a completion note). Siblings
  re-read it; the board renders it. `appendDecision`/`appendCompletionNote`
  (`src/surfaces.ts`).
- **`RUDDER.md`** — read-only, orchestrator-owned, `freshness:`-stamped projection of
  the plan/status that workers re-read (`renderLiveRudderMd`). Git-excluded.
- **`rudder done [--node <id>] '<json>'`** — `{summary, interfaces, followups:[{title,
  why, scope}]}` from stdin or args (freeform fallback). Records only; no jj.

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
Built so far: cap=100, `rudder done` + completion notes, readiness dedup, auto-expand +
activity log, steering primitives (re-goal/inject/stop, `x` keybinding), and
**autonomous drift handling** (file-overlap → coordination nudge). Planned (approved v3
plan): the `OrchestratorMode` plan→conduct state machine + chat routing, the remaining
DAG-edit steering (edge promote/demote, extract-shared, reorder), and plan-rebase.
