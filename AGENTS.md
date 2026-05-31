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
parallel. Each task gets an isolated git worktree and runs a real agent CLI
inside a native terminal pane. Rudder owns the outer workflow around those
agents: a three-pane dashboard, pane focus, scrollback, task summaries, live
review, and merge back to the base branch. Rudder Cloud is an optional hosted
worker mode that keeps the same local control surface.

It ships as the npm package `@viraatdas/rudder`, binary `rudder`
(`dist/index.js`).

Two big pieces, one product:

- A **TypeScript orchestrator** (the `rudder` CLI): argument parsing, run
  lifecycle, git worktrees, backend adapters, state persistence, auth, cloud.
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
 (rudder-native)         │  creates git worktree (git.ts), writes run.json (state.ts)
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
   - Decide worktree vs current checkout. Policy: a second concurrent run on the
     same checkout forces a worktree (`activeRunsForCheckout`). `--worktree`
     forces one explicitly.
   - `baseCommit = worktreeBaseCommit(repoRoot)` (the `main` ref, else HEAD) for
     worktree runs, else current HEAD. `targetBranch = currentBranch(repoRoot)`.
   - `createRunWorkspace` -> `git.ts::createRunWorktree`: creates branch
     `rudder/<task-slug>-<hash>` and `git worktree add` at
     `../.rudder-worktrees/<repo>-<hash>/<task-slug>-<hash>`.
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

5. **Merge / sync** (`src/git.ts`):
   - `mergeRunIntoCurrentBranch(run, allowDirty, strategy)`:
     - commits the worktree (`commitWorktreeChanges`, refuses on unresolved
       conflicts or an in-progress rebase),
     - `strategy === "merge"`: `git merge --no-ff <branch>` into the checkout,
     - `strategy === "rebase"`: rebase the worktree onto the target, then
       `git merge --ff-only`,
     - records `MergeState` (status / conflictKind / conflictedFiles / error) on
       the run and sets `run.status` to `merged` or `merge-conflict`.
   - `syncRunWorktree(run, baseBranch)`: rebases the worktree onto the latest base
     (`resolveRebaseBaseRef` prefers a local branch, then `origin/<branch>` after a
     fetch, then HEAD). Recovers a `merge-conflict` run to `completed` when a
     rebase conflict is resolved.
   - `removeRunWorkspace` / `removeWorktree`: `git worktree remove [--force]`.
   - `git.ts` is git-only. jj/Jujutsu support was removed; see section 12.

---

## 6. Data model and on-disk state

### Types: `src/types.ts`
The important shapes:
- `RunRecord`: the full per-run document. Status union is
  `created | running | steering | verifying | completed | failed | cancelled |
  merge-conflict | merged`. Holds `worktree{enabled,path,branch,workspaceName?}`,
  `process{pid,...}`, `turns[]`, `autoSteer`, `session{nativeSessionId,...}`,
  `terminal{kind:"tmux",...}`, `verification`, `merge`, `sync`,
  `taskSummary`/`taskSummaryLlm`.
- `RudderConfig`: `defaultBackend`, `lastUsedBackend`, `mergeStrategy`,
  `runPolicy`, `backends{claude,codex,acpx}` (each `BackendConfig` with
  `model`/`effort`/`reasoningEffort`/`profileId`).
- `AuthProfileStore`: provider credentials (`api_key` / `oauth` / `token`),
  plus `order`, `lastGood`, `usageStats` (cooldown/disable tracking).
- `RudderEvent`: the event-stream union appended to `events.ndjson`.
- `SpecContract` / `VerificationResult` / `RunRequest` / `BackendAdapter`.
- `CloudAuthState` / `CloudSail` for cloud.
- `VcsMode = "git" | "jj"`: retained only for backward-compatible deserialization
  of old records; runtime is always git (section 12).

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

Worktrees live outside the repo at
`../.rudder-worktrees/<repoSlug>-<repoHash>/<taskSlug>-<runHash>` so they never
nest inside the checkout. `RUDDER.md` is generated at the repo root and inside
each active worktree, and excluded via each checkout's `.git/info/exclude`.

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
`/model [backend] [model] [effort]`, `/plan`, `/rudder-plan`, `/main` or `/m`
(start a main-branch agent), `/sync`, `/goal` (forwarded to the focused agent),
`/usage`, `/cloud [list]`, `/merge-all`.

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

- **No jj.** jj/Jujutsu workspace support was removed; Rudder is git-worktree
  only. The `VcsMode = "git" | "jj"` type and the optional `RunRecord.vcs` /
  `worktree.workspaceName` / `RudderConfig.vcs` fields are kept ONLY so old
  persisted JSON still deserializes. Never write `"jj"`, never branch on vcs, do
  not reintroduce a jj backend.
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
- Run/merge/worktree behavior: `src/git.ts` + `src/run-manager.ts`.
- Dashboard input/state/keys: `native/src/main.rs` (`App` + `impl App`).
  Rendering: `render.rs`. Terminal emulation/scroll: `pty_terminal.rs`. Output
  heuristics: `detect.rs`. Add coverage in `app_tests.rs`.
- Persistence/schema: `src/types.ts` + `src/state.ts` (+ `src/util.ts` for write
  primitives).
- Cloud: `src/cloud.ts` (client) + `cloud/src/server.ts` (control plane) +
  `cloud/worker/` (worker image).
