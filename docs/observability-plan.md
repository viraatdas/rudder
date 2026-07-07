# Rudder Observability Plan: Logging & Perf Instrumentation Rework

*Written 2026-07-02. Investigation of the full logging/perf surface across `native/` (Rust TUI) and `src/` (TypeScript CLI/daemon), plus the actual on-disk state of `~/.rudder` on a real machine. No code was changed; this is the plan.*

---

## 1. Current state — what the deep dive found

### 1.1 The Rust perf tracer is a temporary probe that shipped, default-on

`native/src/perf.rs:1-4` says it itself:

> "Temporary native TUI perf diagnostics for the scroll-latency pass. Enabled by default unless `RUDDER_NATIVE_PERF=0`. Remove this module or make it opt-in once the worker/review scroll investigation is complete."

The struct comment at `native/src/main.rs:676` repeats "TEMPORARY". It was never removed, and its coverage grew far beyond scrolling:

| Event | Call site (main.rs) | Frequency |
|---|---|---|
| `frame_total` | 11056 | **every loop tick, unconditionally** (~30/s idle, ~90/s while a planner streams at `STREAM_TICK_RATE` 11ms) |
| `poll_agents` | 10645 | every tick, unconditionally |
| `pty_drain_parse` | 10300 → 10643 | per running agent per tick |
| `terminal_draw` | 11018 | per draw |
| `scroll_event` ×4 | 2968, 3005, 3036, 3055 | per mouse-wheel tick |
| `event_drain` ×3 | 11044, 11075, 11087 | per input burst |
| `worker_lines` / `review_lines` | render.rs:3252, 3332 | per rendered frame |
| `draw_deferred` | 11002 | per deferred draw |

Each event costs, on the hot path it was meant to *measure*: a `stat()` syscall (`perf.rs:75-78`), a `serde_json` serialize, a `write_all`, and a **`flush()` per line** (`perf.rs:97-99`), which defeats the `BufWriter` entirely. A scroll-latency tool is adding synchronous disk I/O to every scroll event and every frame.

**Measured impact on this machine:** the live `native-perf.ndjson` holds 26,836 events, of which 26,834 (99.99%) are idle-loop `frame_total`/`poll_agents` pairs with `agents: 0, any_dirty: false` — pure noise. ~520K events across all rotated files. Idle rudder writes ~58 flushed NDJSON lines/sec.

### 1.2 Rotation is racy and the cap does not hold

Config says 10 MiB cap, keep 3 rotations, 3-day TTL (`perf.rs:16-20`). Reality on disk:

```
native-perf.ndjson      2.9 MB
native-perf.ndjson.1     40 MB   ← 4× over cap, and NEWER than the live file
native-perf.ndjson.2     16 MB   ← also over cap
native-perf.ndjson.3     10 MB
total                   ~69 MB
```

The race: two concurrent `rudder-native` processes each hold an append fd to the same inode. Process A rotates (`rename` → `.1`). Process B's fd now points at the *renamed* inode, but its per-line size check stats **the path** (`self.path.metadata()`, perf.rs:75-78), which now resolves to the fresh, small file — so B never rotates and `.1` grows without bound. The `.1` file being newer than the live file is the fingerprint of exactly this.

### 1.3 Signal files are never pruned

`native/src/signals.rs:92-94` (its own comment): signal files are removed only "when a run is deleted: **nothing else ever prunes them**". Result: **1,449 orphaned files** in `~/.rudder/signals/`.

### 1.4 The TypeScript side has the opposite problem: it's blind

- **No logger, no levels, no log file, no debug env var anywhere.** ~179 raw `console.*` calls (cloud.ts 76, run-manager.ts 32, auth.ts 25, main.ts 24, …) that are really user-facing CLI output, unlevelled and untimestamped.
- **The daemon logs nowhere.** Its only diagnostics are `console.warn` on scheduler-tick failure (`daemon.ts:156`) and transition failure (`daemon.ts:191`) — but it is spawned detached with `stdio: "ignore"` (`board/daemon.ts:1628`), so those are discarded. A crashing or looping background scheduler leaves **zero trace on disk**. `board/daemon.ts` (the 56 KB HTTP+SSE server) emits nothing at all.
- **Detached workers discard their own stderr** (`run-manager.ts:339`, `stdio: "ignore"`): a worker that crashes before output capture is wired is invisible.
- **Run transcripts are double-written and unbounded**: `state.ts:538-547` appends every event as JSONL to `events.ndjson` *and* the raw text to `output.txt`, no cap, no rotation, cleaned only on run delete. In practice these are currently tiny (a sweep of the busiest registered project found ~500 B–1 KB per run, 396 KB total) — so this is a **latent** correctness risk that a single long-running agent would trip, not today's disk hog.
- **Run state is duplicated inside worktrees**: agent runs executing with cwd inside a `.rudder-worktrees/<ws>` checkout write their *own* nested `<worktree>/.rudder/runs/<id>/events.ndjson` — 24 such nested run dirs found under one project's worktrees. Run artifacts land both at repo root and inside worktree checkouts; needs a decision on whether state should always resolve to the root repo's `.rudder`, and gc coverage either way (§Phase 3). (The worktrees dir itself was 3.1 GB, but that is overwhelmingly source checkouts, not logs.)
- `printLogs` (`run-manager.ts:969`) reads the whole transcript into memory.
- **No perf measurement at all in TS** — no `performance.now()`, no durations, nothing.

### 1.5 Disk reality (`~/.rudder` = 539 MB)

| Item | Size | Verdict |
|---|---|---|
| `bin/` (vendored codex builds) | 462 MB | two full versions kept (`v0.1.0`, `v0.1.1`); old versions never pruned |
| `native-perf.ndjson*` | ~69 MB | the bug above |
| `signals/` | 5.7 MB / 1,449 files | never pruned |
| `models-dev.json` | 2.3 MB | fine (cache) |

### 1.6 What's already good (keep these patterns)

- `activity.jsonl` self-trims to its last 400 lines on every append (`main.rs:6955-6963`) — the right shape for an append-only stream.
- `writeJson`/`updateJson` atomic temp+rename with per-path locks (`util.ts:138-179`).
- `cleanup_old_logs` age-based TTL in perf.rs — right idea, just needs to be applied to `signals/` and `bin/` too.
- Credentials written `0o600`; no secrets found in any log output (audited).
- The `mouse_debug` in-memory overlay (`main.rs:3092`, gated by `RUDDER_MOUSE_DEBUG`) — diagnostics rendered in the TUI with **zero disk I/O** is exactly the right model for a terminal app.

### 1.7 Coverage is inverted

The frame/scroll/PTY loop is over-instrumented, while the subsystems that actually block the UI — `gitio.rs` (89 KB of git/jj shell-outs), `tasks.rs` (92 KB orchestration), `cloudio.rs`, `launch.rs` — have **no timing coverage at all**. `mirror_graph()` and `write_rudder_context()` shell out from inside the poll loop and are unmeasured.

---

## 2. Goals

1. **Idle rudder writes ~0 bytes/sec of diagnostics.** Perf capture is opt-in or threshold/aggregate-based.
2. **Every long-lived background process is debuggable post-mortem** (daemon, board server, detached workers) via a bounded log file.
3. **All diagnostic files are bounded and self-pruning** — a hard ceiling on `~/.rudder` diagnostic footprint (~25 MB), enforced without user action.
4. **Perf questions stay answerable** — scroll latency, frame budget misses, PTY drain cost, git shell-out cost — via cheap aggregates + a dev-time profiling workflow, not a firehose.
5. **User-facing output and diagnostics are separated** so diagnostics can be silenced or raised without touching UX text.

---

## 3. The plan

### Phase 0 — Stop the bleeding (small PRs, ship immediately)

**P0.1 Flip the perf tracer to opt-in.** `RUDDER_NATIVE_PERF=1` enables; absent/0 → fully disabled (no file open, no stat). Make test and prod semantics identical (today they're inverted: `perf.rs:30-34`). This one-line-ish change removes all idle I/O and honors the module's own "make it opt-in" TODO.

**P0.2 Fix the writer even when enabled:**
- Drop the per-line `stat()`: track bytes written in-process (the writer knows).
- Drop the per-line `flush()`: flush on a timer (~250 ms), on rotation, and on drop.
- Kill the multi-process rotation race the simple way: **per-process files** — `native-perf-<pid>.ndjson`. No locks, no rename races; the existing `cleanup_old_logs` TTL (extended to match the new pattern) handles removal. This is strictly simpler than flock + inode-recheck and fits the "N concurrent rudder instances" reality.

**P0.3 Delete the unconditional per-tick events.** `frame_total`/`poll_agents` should only log when the frame exceeded a threshold (e.g. > 2× `TICK_RATE`) — see P1.1 for where the aggregate view of normal frames goes.

**P0.4 TTL-prune `signals/`.** On native startup (same place `PerfLogger::new` runs cleanup), delete signal files older than ~7 days. The precedent (`cleanup_old_logs`) already exists in the codebase; apply it to the directory whose own comment says nothing prunes it.

**P0.5 Prune superseded `bin/` versions.** After a successful binary install, delete versions other than current (with the currently-running one exempted). Recovers ~230 MB on this machine alone.

*Acceptance: fresh idle session writes 0 diagnostic bytes; `~/.rudder` (minus `bin/` current version and caches) < 25 MB and shrinking on old machines.*

### Phase 1 — Rust: replace the firehose with aggregates + an in-TUI HUD

**P1.1 In-memory histograms, periodic summary lines.** Add the `hdrhistogram` crate (small, no deps, the standard for latency capture). Record `frame_total`, `render_build`, `poll_agents`, `pty_drain`, `scroll_event` into per-metric histograms in `App`. When perf logging is enabled, emit **one summary NDJSON line per metric per 60 s** (count, p50, p95, p99, max) instead of one line per event. This is a ~1000× volume cut with *better* signal for regression hunting — percentiles are what you actually compare between builds.

**P1.2 Threshold ("tail") events.** Alongside aggregates, log a raw event only when it breaches budget: frame > 66 ms, drain > 10 ms, scroll handler > 5 ms. These outliers with their context fields (`run_id`, `view`, `bytes`) are the lines a human actually reads.

**P1.3 A perf HUD instead of a file, for interactive debugging.** Follow the existing `mouse_debug` overlay pattern: `RUDDER_PERF_HUD=1` renders a one-line overlay (fps, frame p95/max over the last 5 s, drain µs, agent count) in the TUI. Zero disk I/O; the person chasing scroll latency sees the numbers live while scrolling. This replaces the primary use-case the NDJSON firehose was built for.

**P1.4 Instrument the actually-slow paths.** Wrap the unmeasured blocking operations — `mirror_graph`, `write_rudder_context`, `gitio` shell-outs, jj diff spawns in `tasks.rs` — with the same `log_duration`-style timing feeding the histograms. These are the operations that can actually stall a tick; today they're the only ones with no coverage.

**P1.5 (Optional, feature-gated) adopt `tracing` for deep dives.** Behind a cargo feature (`--features perf-trace`, never in release builds): `tracing` + `tracing-tracy`, and profile with the [Tracy](https://github.com/wolfpld/tracy) frame profiler — purpose-built for frame-loop applications, shows spans per frame in real time. Because it's feature-gated, the shipped binary keeps the current 8-crate dependency footprint. If you'd rather add zero crates, skip this and rely on P1.3 + samply (Phase 4).

*Deliberate non-choice:* adopting `tracing`/`tracing-subscriber` as the always-on logging substrate for the native TUI. The TUI owns the terminal (stderr logging would corrupt the screen), the bespoke NDJSON writer post-P0 is ~150 lines and fully understood, and the project's dependency minimalism is clearly intentional. `tracing` earns its weight only in the feature-gated dev profile.

### Phase 2 — TypeScript: give the CLI/daemon a real (tiny) logger

**P2.1 A ~100-line internal `src/logger.ts`** (no new npm dependency, matching the one-runtime-dep ethos):
- Levels `error|warn|info|debug`; env `RUDDER_LOG=debug` controls verbosity, default `warn`.
- Two sinks: stderr (only when attached to a TTY and not inside the native TUI) and an NDJSON file sink with size-capped rotation (reuse the perf.rs constants: 5 MB × 2).
- `logger.child({ component: "daemon" })`-style tagging so grep works.
- Timestamps + component on every line.

If you'd rather not hand-roll: **pino** is the standard choice (fast, NDJSON-native, `pino-roll` for rotation). It's a fine swap-in; the interface above should be shaped so either backs it. Recommendation: hand-roll first — the needs are small and the project already hand-rolls everything else well.

**P2.2 Daemon and board-server logs.** Spawn the detached daemon with stdio redirected to an fd on `~/.rudder/logs/daemon.log` (instead of `stdio: "ignore"`, `board/daemon.ts:1628`) so even uncaught crashes land somewhere, and route the scheduler-tick/transition warnings (`daemon.ts:156,191`) plus board request errors through `logger`. Rotate on daemon start (size check + rename, single-writer so no race).

**P2.3 Worker crash capture.** Detached workers (`run-manager.ts:339`): redirect stderr to `.rudder/runs/<id>/spawn-stderr.log` (capped small, e.g. 256 KB) instead of `ignore`. This makes "worker died before wiring output capture" diagnosable for the first time.

**P2.4 Bound the run transcripts.** `events.ndjson` + `output.txt` (`state.ts:538-547`). Priority note: measured run transcripts are currently tiny (§1.4), so this is a robustness fix, not a disk win — the perf tracer (Phase 0) is where the bytes are. Also resolve the worktree duplication: decide whether `projectStateDir` should resolve worktree cwds to the root repo's `.rudder` (`state.ts:50`), or keep nesting intentional and teach gc about it.
- Cap `output.txt` with the `activity.jsonl` self-trim pattern (keep last N MB) — it's a live-view convenience, not an archive.
- Keep `events.ndjson` complete per run (it's the replay record) but add retention: `cleanupRuns` should also fire on a schedule/startup, not only on explicit delete.
- Fix `printLogs` (`run-manager.ts:969`) to stream (`createReadStream → stdout`) instead of buffering the whole file.

**P2.5 Separate UX text from diagnostics.** Sweep the 179 `console.*` calls: keep genuine user-facing output (`Started…`, `Merged…`) as `console.log`, move everything that's really a diagnostic (retry notices, cloud polling chatter in `cloud.ts`, swallowed-error contexts like `surfaces.ts:81` `.catch(() => undefined)`) to `logger.debug/warn`. Swallowed errors should at least `logger.debug` the error before discarding.

### Phase 3 — Retention as a feature: `rudder gc` (+ auto-gc)

A single command that owns the disk story, with `--dry-run`:

```
rudder gc
  perf logs      keep ≤ 2 files × 5 MB, TTL 3 d      (~/.rudder)
  signals        TTL 7 d                              (~/.rudder/signals)
  daemon logs    keep ≤ 2 × 5 MB                      (~/.rudder/logs)
  old binaries   keep current version only            (~/.rudder/bin)
  finished runs  respect canCleanupRun, TTL 30 d      (<repo>/.rudder/runs)
  worktree runs  nested <worktree>/.rudder/runs from merged/abandoned
                 workspaces (see §1.4 duplication finding)
```

Auto-run the cheap subset (signals, perf, daemon logs) on TUI startup — the codebase already does exactly this for perf logs via `cleanup_old_logs`, so this generalizes an existing pattern rather than inventing one. `rudder gc --dry-run` doubles as the "why is my disk full" doctor output (prints per-category sizes, like the table in §1.5).

### Phase 4 — Dev profiling workflow + CI regression guards

The reason perf.rs exists is "we needed to chase a scroll-latency bug." Give that job real tools so the next investigation doesn't grow another firehose:

**Interactive profiling (no code changes needed, document in `native/RUDDER.md`):**
- [`samply`](https://github.com/mstange/samply) — `samply record ./target/release/rudder-native`; sampling profiler with the Firefox Profiler UI, excellent on macOS, shows exactly where frame time goes.
- [`cargo flamegraph`](https://github.com/flamegraph-rs/flamegraph) — one-shot flamegraphs for the same.
- Tracy via the P1.5 feature, when per-frame span timing is needed.
- [`dhat-rs`](https://docs.rs/dhat) if heap churn in the render path ever becomes a question.

**Micro-benchmarks:** [`divan`](https://github.com/nvzqz/divan) (or criterion) benches for the known-hot pure functions: `styled_line_window_snapshot`, `styled_terminal_line`, `plan_stream::ingest`, the `detect.rs` heuristics. These lock in the scroll-latency wins the perf pass achieved.

**End-to-end:** [`hyperfine`](https://github.com/sharkdp/hyperfine) for startup time (`rudder-native --smoke`-style runs) in CI; compare against main with a threshold.

**CI wiring:** a `bench` job that runs divan benches and `critcmp`/threshold-compares against the base branch; warn-only at first, gating later if it proves stable.

---

## 4. Tool summary

| Need | Tool | Why this one | Ship weight |
|---|---|---|---|
| Latency aggregation (Rust) | `hdrhistogram` crate | standard, tiny, no deps; percentiles are the comparable unit | 1 small crate |
| Live perf visibility | in-TUI HUD (own code) | TUI owns the screen; zero disk I/O; existing `mouse_debug` pattern | none |
| Deep frame profiling | `tracing` + `tracing-tracy` + Tracy | frame-oriented, real-time spans | feature-gated, not shipped |
| Sampling profiles | samply / cargo-flamegraph | no code changes at all | none |
| Micro-benchmarks | divan (+ critcmp) | simpler than criterion, good CI story | dev-dep |
| E2E timing | hyperfine | standard | none |
| TS logging | internal ~100-line logger (pino as the off-the-shelf alternative) | needs are small; matches one-dep ethos | none (or pino) |
| Retention | `rudder gc` (own code) | generalizes existing `cleanup_old_logs` pattern | none |

## 5. Acceptance criteria

1. Idle `rudder-native` session: **zero** diagnostic file writes (verify with `fs_usage`/`dtruss` or just file mtimes).
2. `RUDDER_NATIVE_PERF=1` session for 1 hour: perf output ≤ 1 MB (summaries + threshold events only).
3. Two concurrent rudder instances for a day: no diagnostic file exceeds its cap (the §1.2 race is gone by construction with per-PID files).
4. `kill -9` the daemon mid-scheduler-tick → `~/.rudder/logs/daemon.log` shows the last tick and the failure.
5. `rudder gc --dry-run` on this machine reports and (on real run) recovers ≈ 300+ MB (old codex binary, perf logs, signals).
6. Scroll-latency investigation runbook: HUD + samply reproduce everything the NDJSON firehose could answer, verified by re-answering one past question from the scroll pass.

## 6. Risks / notes

- **Losing the incident-forensics value of always-on capture.** Mitigation: the threshold events (P1.2) still fire when things are actually slow, and `activity.jsonl` remains the always-on behavioral trail. If a regression only reproduces on a user's machine, `RUDDER_NATIVE_PERF=1` is a one-env-var ask.
- **Per-PID perf files** mean a session's log is spread across files; fine for the debugging use-case (you know your PID / take the newest), and `rudder gc` owns the sprawl.
- **The console.* sweep (P2.5) touches user-visible strings** — keep it mechanical (same text, new sink) and diff the CLI output in tests (`tests/*.test.mjs`) to prove UX output unchanged.
- Sequencing: Phase 0 is independent and safe to ship alone. Phases 1–3 are independent of each other. Phase 4 is documentation + dev-deps only.
