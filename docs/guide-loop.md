# Guide Loop (`scripts/guide-loop.mjs`)

*Built 2026-08-16 to babysit the WagerPals session. Status: BUILT and running.
Complements `rudder improve` (which makes Rudder better from past sessions);
this makes the CURRENT session better while it is still running.*

---

## 1. What this is

A Sonnet 5 supervisor that watches one live Rudder session, judges whether the
fleet is converging on a stated product goal, and steers it when it is not.

```
 live Rudder session (.rudder/*)                    goals.md
   run.json rows ─┐                                     │
   graph.json     ├─► DIGEST (deterministic, node) ─────┴─► SONNET GUIDE (claude -p, read-only tools)
   plan-queue     │                                             │
   signals        │                                             ▼  {assessment, risks, steers[], watchlist}
   transcripts ───┘                                        RAILS (targets, caps, cooldown, dedupe)
                                                                │
                                                                ▼
                                              .rudder/steer/*.json ─► native TUI poll (~1s)
                                                                │
                                                                ▼
                                          instruction injected into the target agent's live PTY
                                                                │
                                              .rudder/steer-receipts/*.json ─► next tick learns what landed
```

The model decides *what* to say. The script owns everything that could hurt:
which targets are addressable, how many steers per tick, per-target cooldown,
duplicate suppression, hourly ceiling, and the durable ledger.

## 2. What the digest contains

Statuses alone say an agent is busy; they never say it is building the right
thing. So each tick assembles, per row: id/node, status, lifecycle phase, mode,
backend, model, task, current prompt, done summary, worktree, idle age, the
agent's Rudder signal state (`input` = blocked on a question, `done` = finished
its turn), and a tail of what it actually did, read off its Claude Code
transcript (`~/.claude/projects/<slug>/<session>.jsonl`) — assistant text plus
tool calls. Around that: the DAG, plan-queue state (awaiting approval, final
gate), done reports, activity narration, git branch/dirty/recent commits, and
the guide's own last four ticks, watchlist, and steer receipts.

Rows untouched for 24h are treated as an earlier session and dropped.

## 3. The control path

`.rudder/steer/<ts>-<requestId>.json` is Rudder's browser→agent inbox, polled
about once a second by the running native TUI (`poll_steer_inbox`). The guide
writes the same shape:

```json
{"requestId": "guide-<ts>-<rand>", "taskId": "conductor|<run-id>|<node-id>",
 "instruction": "…", "kind": "steer", "source": "guide-loop"}
```

- `taskId: "conductor"` starts NEW work that folds into the plan (a missed
  requirement, an unowned area). Costs a planning pass, so it is rare.
- A run id or node id injects straight into that agent's live prompt; a `Done`
  row is re-goaled in place instead.

Requests are written to `.tmp` and renamed, so the poll never reads a torn file.
`requestId` is the at-most-once ledger: Rudder writes a receipt
(`processing` → `delivered`/`failed`) before consuming the request, and the next
tick folds that outcome back into the digest.

## 4. Rails

| Rail | Default | Why |
| --- | --- | --- |
| target must exist in the digest | — | no steering into the void |
| max steers per tick | 2 | one excellent steer beats two mediocre ones |
| per-target cooldown | 12 min | an agent mid-task should not be re-prompted |
| duplicate instruction (1h window) | dropped | stops the guide nagging |
| hourly budget across all targets | 6 | a confused guide cannot spam a fleet |
| guide tools | read-only | it reviews; the fleet writes |

`--dry-run` runs the whole loop and prints what it *would* send.

## 5. Usage

```bash
node scripts/guide-loop.mjs --repo ~/code/wagerpals --once --dry-run
nohup node scripts/guide-loop.mjs --repo ~/code/wagerpals --interval 420 \
  >> ~/.rudder/guide/wagerpals/loop.log 2>&1 &
```

Flags: `--repo`, `--goals` (default `<state-dir>/goals.md`), `--state-dir`
(default `~/.rudder/guide/<repo-name>`), `--interval` seconds, `--model`,
`--max-steers`, `--cooldown`, `--hourly-budget`, `--once`, `--dry-run`.

State dir holds `goals.md` (the bar, hand-written), `state.json` (rolling
memory: last ticks, watchlist, sent steers, pending receipts),
`ledger.jsonl` (every tick, steer, rejection, receipt), and `loop.log`.

Ticks skip themselves when no live session owns the repo, so the loop can sit
running across a restart of the TUI.

## 6. Why the goals file matters

The guide is only as good as the bar it holds the fleet to. `goals.md` should
state the product outcomes in user-visible terms, what "off track" looks like
concretely for this project, and the non-negotiables (e.g. *merged ≠ pushed ≠
deployed*). A vague goals file produces a guide that congratulates activity.
