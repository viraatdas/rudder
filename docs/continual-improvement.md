# Rudder Continual Improvement Loop (`rudder improve`)

*Design written 2026-07-07; core loop implemented the same day in
`src/improve/`. Status: BUILT (collect, mine, rank, propose, gate, judge,
auto-ship, outcome check, launchd schedule). The eval-replay tier (§4.6) is the
remaining unbuilt stage. AGENTS.md section 15 is the short engineering map that
points here.*

---

## 1. What this is

A closed loop that makes Rudder better from its own usage:

```
 real rudder sessions
        │  (run records, event logs, steers, merges, token usage)
        ▼
 COLLECT ─► MINE (LLM triage) ─► RANK ─► PROPOSE (agents in worktrees)
                                              │
        ┌─────────────────────────────────────┘
        ▼
 GATE (tsc + cargo test + node --test) ─► EVAL REPLAY ─► JUDGE PANEL (blind A/B)
        │
        ▼
 SHIP (rebase on origin/main ─► npm version patch ─► push main + tag)
        │                                   │
        │                                   └─► existing tag-driven CI publishes to npm
        ▼
 LEDGER + OUTCOME CHECK (did the friction metric actually move?) ─► next cycle
```

Every stage is resumable, budgeted, and leaves an auditable artifact. The loop
runs locally on the developer's machine, on a schedule, against the telemetry
Rudder already writes. Nothing here changes how Rudder runs for end users; the
loop is a dev-side consumer of existing on-disk state.

Two principles that shape everything below:

1. **Isolation.** Improvement agents run headless (`claude -p`) inside
   disposable git worktrees of the rudder checkout, on a branch per finding.
   The loop never edits the user's checkout and never force-pushes.
2. **Measure, don't vibe.** A proposal ships only after deterministic gates
   and an adversarial judge panel. And a shipped change is only *counted* as
   an improvement when later cycles show the source friction metric moved.

---

## 2. Daemon or not: scheduled batch, not a resident daemon

Decision: **no new resident daemon.** The loop is `rudder improve run`, a
one-shot batch command, scheduled by the OS.

Why not a daemon:

- The work is inherently periodic (hours of telemetry accumulate between useful
  cycles). A resident process would sit idle 99.9% of the time on a laptop,
  costing battery and adding a lifecycle to manage (start, crash, upgrade,
  orphan detection). The observability plan (docs/observability-plan.md §1.4)
  already documents how much trouble Rudder's one existing daemon has being
  debuggable; don't add a second.
- `launchd` on macOS already solves scheduling on a machine that sleeps:
  `StartCalendarInterval` jobs missed during sleep fire on wake, and launchd
  restarts nothing because there is nothing resident to restart.
- Crash isolation: a batch run that dies leaves a lock file and a partial
  report, and the next scheduled run picks up from the watermark. A daemon that
  dies silently stops learning.

Scheduling surface:

```bash
rudder improve schedule install     # writes ~/Library/LaunchAgents/dev.viraat.rudder.improve.plist
rudder improve schedule uninstall
rudder improve schedule status
```

The plist runs `rudder improve run` nightly (03:30, `StartCalendarInterval`),
with stdout/stderr appended to `~/.rudder/improve/launchd.log`. On other
platforms, run `rudder improve run` from cron/systemd directly. Manual runs
are always available and take the same lock.

Single-instance guard: `~/.rudder/improve/cycle.lock` (mkdir lock, stale
takeover after 6h, same pattern as `.rudder/integrate.lock`).

Kill switch: `RUDDER_IMPROVE=0` in the environment, or
`improve.enabled: false` in `~/.rudder/config.json`, makes `run` a no-op that
logs one line.

---

## 3. What it learns from (telemetry inventory)

All sources already exist; the loop only reads them.

| Source | Path | Signal |
|---|---|---|
| Run records | `<repo>/.rudder/runs/<id>/run.json` | status trajectory (`failed`, `cancelled`, `merge-conflict`), backend/model/effort, turns, autoSteer count, timing |
| Event logs | `<repo>/.rudder/runs/<id>/events.ndjson` | errors, retries, backend stderr, long silences |
| Verifier output | `<repo>/.rudder/runs/<id>/verifier.json` | `satisfied:false`, `missing[]` = the contract failed the user |
| Completion notes | `DECISIONS.md`, done sidecar files | what agents said they did vs what the diff shows |
| Steering | `.rudder/steer/` queue, `RUDDER_REGOAL`/`RUDDER_INJECT` markers in activity | every user redirect is a "Rudder got it wrong" vote |
| Activity log | `activity.jsonl` | conductor actions: drift nudges, integrations, rebase diffs |
| Graph history | `.rudder/graph.json`, plan-queue, ingestion ledger | DAG shape vs outcome: which plans stalled, which fan-ins conflicted |
| Token usage | usage accounting (`native/src/usage.rs` data) | cost per task class, cost regressions |
| Crashes | daemon/worker logs once observability P2.2/P2.3 land | silent failures |

Which repos: every project registered in `~/.rudder/projects.json`, minus any
matched by `improve.excludeProjects` globs in config. The corpus is built per
cycle as **session records**: one normalized JSON per run, containing derived
features (duration, steer count, outcome, error classes) plus *redacted*
excerpts of prompts and failures, never whole transcripts.

**Redaction is mandatory at collection time**, before any model call: strip
`*_TOKEN` / `*_KEY` / `*_SECRET`-shaped values (reuse the
`capture_shared_context_from_user_input` patterns), URLs with credentials, and
everything in `RUDDER_SHARED.md`. Raw session data never leaves the machine
except inside the same model API calls Rudder already makes.

Watermark: `~/.rudder/improve/watermark.json` records, per project, the newest
run mtime consumed. A cycle only reads newer runs, so cost scales with usage,
not history.

---

## 4. The pipeline

### 4.1 Collect (no LLM)

Pure code (`src/improve/collect.ts`). Run records are read raw
(`readRunsRaw`), never through `state.ts::loadRunRecord`, because that load
path fires the background LLM task summarizer, which costs model calls and
rewrites `run.json` (bumping `updatedAt` past the watermark). Walk projects,
build session records, redact, compute the cycle's
**metric snapshot**: steer rate (steers per run), failure rate, merge-conflict
rate, verifier-miss rate, median tokens per completed task, crash count. Append
the snapshot to `~/.rudder/improve/metrics.jsonl`. This series is both the
ranking input and the outcome check for previously shipped changes (§8).

### 4.2 Mine (LLM triage)

Map an inexpensive model (haiku-class, same `callTextModel` plumbing the
planner backstop uses) over session records to extract **findings**:

```json
{
  "id": "f-2026-07-07-a3",
  "class": "prompt | orchestration | ux | perf | crash",
  "title": "Workers re-implement merged parents when Depends-on block exceeds N chars",
  "evidence": [{ "project": "…", "run": "…", "excerpt": "…" }],
  "severity": 1-5,
  "frequency": 3,
  "suspectedSurface": ["native/src/tasks.rs::dependency_context"]
}
```

Findings are deduped two ways: mechanically against the ledger (normalized
title + surface key), then one LLM pass over near-misses. A finding whose
prior proposal was **rejected** (PR closed unmerged) is suppressed unless new
evidence doubles its frequency; the ledger carries the rejection reason so the
model sees why.

### 4.3 Rank (no LLM)

`score = severity × log(1+frequency) × tractability`, where tractability is a
static per-class weight (prompt changes are cheap and safe; native scheduler
changes are not). Take the top N findings (default 3) that fit the remaining
cycle budget. Everything not taken stays in the ledger as backlog with its
score; nothing is silently dropped.

### 4.4 Propose (isolated agents with a context pack)

For each selected finding, the harness creates a disposable git worktree of
the rudder checkout (`improve.repoPath`, auto-detected) on branch
`improve/<finding-id>` based on `origin/main`, and runs a headless improvement
agent (`claude -p --dangerously-skip-permissions`) inside it
(`src/improve/propose.ts`).

The agent's prompt is a **context pack** built for good context, not a bare
task line: the finding with severity/frequency, redacted evidence excerpts
from the real sessions, the suspected surfaces plus the full surface map, the
cycle's metric snapshot, prior failed attempts on the same finding from the
ledger (so it never repeats a rejected approach), and hard requirements: read
AGENTS.md end to end first, diagnose root cause before fixing, minimal diff,
add tests, run the checks itself, commit its work, write a structured
`.rudder-improve-result.json` self-report (summary, `prompt-only` vs `logic`
class, risks), and never touch versions/tags/pushes.

### 4.5 Gate (deterministic, cheap, first)

The repo's existing gates, in the worktree, before any judge tokens are spent:
`npm run check`, `cargo test --manifest-path native/Cargo.toml` (when
`native/` changed), `node --test tests/*.test.mjs`. Fail = the proposal is
recorded as gated-out with the log excerpt, and the worktree is kept for one
retry with the failure fed back; a second failure abandons the finding for
this cycle.

### 4.6 Eval replay (designed, not yet built)

The loop maintains a growing eval corpus at `~/.rudder/improve/evals/cases/`.
Each case is a distilled frictional session: the task text, a minimal repo
fixture (or a pointer to a generated one), the environment knobs, and an
expected-outcome rubric. Two execution tiers:

- **Deterministic tier** (preferred): drive the real CLIs with the repo's
  existing test hooks (`RUDDER_FAKE_MODEL_OUTPUT`, `RUDDER_FAKE_BACKEND=1`,
  the e2e-orchestrator harness) for orchestration/scheduling findings. Free
  and exact.
- **Live tier** (budgeted): re-run the task with the real backend at a small
  model/effort for prompt-class findings, K=2 trials per side to damp
  variance.

Every cycle, the miner also *emits* 0-2 new eval cases from the worst fresh
sessions, so the corpus compounds. The corpus is the loop's most valuable
asset; cases are only retired when the surface they test is deleted.

Candidate and baseline both run the affected cases. Output: per-case metric
deltas (success, steers, tokens, wall time).

### 4.7 Judge panel (adversarial)

Three judges (`src/improve/judge.ts`, advisor-backed calls per §5), each with
a distinct lens: **correctness** (does the diff fix the root cause),
**regression-risk** (what else could this break; violated invariants, missing
coverage), **simplicity** (is there a smaller, more conventional change).
Every judge is prompted to *refute* the change and returns a structured
verdict; unparseable output fails closed as a rejection.

Ship condition: **no judge flags a concrete regression, at least 2/3 approve,
AND the correctness lens approved** (`panelDecision`). Correctness carries
veto weight because a shipped-but-ineffective change wastes a release and
marks the finding shipped, muting it until the outcome check disproves it.
Simplicity alone is outvotable. Gate failures earlier get one revision round
with the failure fed back to the same agent, then abandon.

When the eval-replay tier (§4.6) lands, judges additionally get blind A/B
eval deltas (candidate/baseline randomly labeled per judge). Until then the
outcome check (§8) is the backstop that catches anything that gamed the
panel, because production metrics don't lie for long.

### 4.8 Ship (auto-push + auto-version)

A judged survivor ships itself, following the repo release rule end to end
(implemented in `src/improve/ship.ts`):

1. `git fetch origin main` and rebase the proposal branch onto it, so the
   version bump reads the latest `package.json` (two winners in one cycle
   ship sequentially through this step).
2. Re-run `npm run check` post-rebase (the full suites already gated the
   pre-rebase tree).
3. `npm version patch` in the worktree: commits and tags `vX.Y.Z`.
4. `git push origin HEAD:main refs/tags/vX.Y.Z` in one push, **never
   forced** — if main moved after the rebase, the push fails cleanly and the
   cycle records a `push-conflict` for retry. The existing tag-driven CI
   (`release.yml`) validates tag == package.json version and publishes to npm.

Guardrails on the unattended push: the ship stage refuses to run unless
`origin` matches `improve.allowedRemote` (default `viraatdas/rudder`); the
proposing agent is forbidden from touching versions/tags itself; a failed
push deletes the local tag so retries never collide.

Autonomy levels (`improve.autonomy` in config):

- `observe` — collect/mine/rank only; write the report, propose nothing.
- `propose` — full pipeline, then push branch `improve/<finding-id>` for
  human review instead of touching main.
- `ship` — explicit opt-in. The full auto-push + auto-version flow above.

The default is `propose`: a scheduled loop can do the expensive collection,
implementation, gates, and adversarial review unattended, but a person remains
the release boundary for a globally installed CLI.

### 4.9 Report

Every cycle writes `~/.rudder/improve/reports/<date>.md`: metric snapshot and
trend, findings mined (taken / banked / suppressed), proposals with outcomes,
spend. The dashboard surfaces one notice line when a new report exists
("improvement loop: 2 PRs proposed, report available").

---

## 5. Budget and cost control

Estimated per-cycle ceiling, checked before model calls and agent runs:
`improve.budgetUsd` (default 5). CLI-backed agent usage is charged as a flat
estimate because subscription authentication does not expose billable usage;
API-backed judge/miner calls use reported tokens when available. Order of spend is cheapest-first by
construction (collect is free; judges run last and only for survivors).
Exhausting the budget mid-cycle is normal: the cycle closes, banks the
remainder, and the report says where it stopped.

**Advisor pattern for judgment calls.** The mining and judging calls use the
Anthropic advisor tool (`src/improve/advisor.ts`, beta
`advisor-tool-2026-03-01`): the executor (`improve.minerModel` /
`improve.judgeModel`, default Sonnet) generates the output and consults a
higher-intelligence advisor (`improve.advisorModel`, default `claude-fable-5`,
capped at 2048 advisor tokens per consult) mid-generation. Most tokens bill at
the executor rate while the hard judgment gets frontier-model input. Spend is
metered exactly from `usage.iterations` (executor iterations at executor
rates, advisor iterations at advisor rates). Degradation is automatic: no API
key (CLI auth), a rejected model pair, or any advisor error falls back to a
plain executor-only call; setting `improve.advisorModel: ""` disables the
pattern.

---

## 6. State on disk

```
~/.rudder/improve/
  cycle.lock            single-instance mkdir lock
  watermark.json        per-project consumption high-water marks
  metrics.jsonl         one metric snapshot per cycle (the trend line)
  ledger.jsonl          findings, proposals, PR ids, outcomes, rejection reasons
  evals/cases/<id>.json distilled replayable cases
  reports/<date>.md     human-readable cycle reports
  launchd.log           scheduler stdout/stderr, self-capped
```

All JSON writes go through `writeJson`/`updateJson` (atomic temp+rename,
per-path lock), matching the repo convention. The ledger and metrics files are
append-only JSONL.

---

## 7. CLI surface

```
rudder improve run [--budget-usd N] [--dry-run]
rudder improve status          # last cycle, shipped versions, ledger tail
rudder improve report [date]   # print a cycle report
rudder improve schedule install|uninstall|status
```

`--dry-run` runs collect + mine + rank and prints what it *would* propose,
spending only mining tokens.

---

## 8. Closing the loop: outcome verification

Shipping is not learning; the loop must check its own work. Each ledger entry
for a merged PR records the metric it targeted and the metric snapshot at ship
time. Cycles that run ≥1 week after the fix's release version appears in
telemetry (run records carry the rudder version) compare the target metric
before/after. Three outcomes, all recorded:

- **confirmed** — metric moved the right way; the finding class's tractability
  weight nudges up.
- **no-effect** — metric flat; the ledger notes it, and a repeat finding on the
  same surface escalates to a bigger proposal rather than another tweak.
- **regressed** — metric moved the wrong way; the ledger records the regression
  and allows the finding to resurface for a new proposal. The loop does not
  silently unpublish or revert a released npm version.

This is the "continual" part: the judge panel approximates ground truth,
production telemetry *is* ground truth, and disagreements retrain the loop's
own ranking.

---

## 9. Implementation map (as built)

Module `src/improve/`, dispatched as the `improve` command in `src/main.ts`:

```
src/improve/
  index.ts        cycle orchestration + status/report subcommands
  state.ts        types, ~/.rudder/improve/* paths, JSONL IO, cycle lock,
                  SpendMeter, execStep (timeout-capable spawn)
  collect.ts      raw run.json reads, session records, redaction, metrics
  mine.ts         LLM triage + surface map + ledger dedupe/rejection memory
  rank.ts         pure scoring + class -> target-metric map
  advisor.ts      advisor-pattern API calls (executor + Fable 5 advisor)
  propose.ts      git worktree per finding, context pack, headless agent
  gate.ts         npm ci / check / cargo test / npm test in the worktree
  judge.ts        3-lens adversarial panel, fails closed
  ship.ts         rebase -> npm version patch -> push main + tag (guarded)
  schedule.ts     launchd plist install/uninstall/status
```

Reuse points honored: `callTextModel` fallback + `redactTaskSummarySecrets`
from `src/task-summary.ts`; `runCommand`/`writeJson`/`readJson` from
`src/util.ts`; the `RUDDER_FAKE_MODEL_OUTPUT` hook makes the whole observe
path deterministic for tests (`tests/improve.test.mjs`).

Remaining phases: **evals** (§4.6 case distillation + deterministic replay,
then blind A/B judging) and **tractability feedback** (outcome results
adjusting rank weights).

---

## 10. Risks and mitigations

- **A bad change auto-ships.** This is the accepted risk of explicitly opting
  into `ship` autonomy; the default `propose` mode keeps a human release gate.
  Defenses in order: deterministic gates (full test suites), refute-first
  judge panel that fails closed, non-forced push (main moving wins), remote
  allowlist, `rudder undo` / `git revert` for anything that lands, and the §8
  outcome check flags a shipped regression for revert. Drop to
  `improve.autonomy: "propose"` for branch-only shipping at any time.
- **Judge-gamed changes.** Refute-first prompting, fail-closed parsing, and
  §8 outcome checks; blind A/B evals strengthen this when §4.6 lands.
- **Telemetry contains user code/secrets.** Redaction at collect time, corpus
  never persisted unredacted, everything stays on-machine except inside model
  API calls the user already makes with the same providers.
- **Cost runaway.** Hard budget ceiling checked pre-call; the advisor pattern
  keeps bulk tokens at executor rates; cheapest-first stage order;
  watermarking bounds corpus size; findings that miss the budget window are
  banked, not dropped.
- **Release spam.** Max `improve.maxFindings` (3) proposals per cycle, one
  cycle per night, dedupe + rejection memory in the ledger.
- **The loop breaking rudder for itself.** Proposals run in disposable git
  worktrees; the loop never edits the user's checkout. The kill switch
  (`RUDDER_IMPROVE=0`) and `observe` mode short-circuit before any repo
  access.
