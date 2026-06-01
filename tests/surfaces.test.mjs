import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import { renderContract } from "../dist/brain.js";
import { parseDecisions } from "../dist/board/daemon.js";
import {
  DECISIONS_HEADER,
  PROPAGATION_RULES,
  appendDecision,
  ensureDecisionsFile,
} from "../dist/surfaces.js";
import { buildResolverPrompt, resolverShouldFinalize } from "../dist/scheduler.js";

// ---------------------------------------------------------------------------
// DECISIONS.md bullet parser (pure). text/owner/ts extraction.
// ---------------------------------------------------------------------------

test("parseDecisions: plain bullet yields text only", () => {
  const entries = parseDecisions("# Decisions\n\n- plain decision without metadata\n");
  assert.equal(entries.length, 1);
  assert.equal(entries[0].text, "plain decision without metadata");
  assert.equal(entries[0].owner, undefined);
  assert.equal(entries[0].ts, undefined);
});

test("parseDecisions: extracts owner and ts from (owner: X, <iso>) suffix", () => {
  const raw = "- the parser owns the token budget  (owner: r-a1, 2026-05-30T12:00:00.000Z)\n";
  const entries = parseDecisions(raw);
  assert.equal(entries.length, 1);
  assert.equal(entries[0].text, "the parser owns the token budget");
  assert.equal(entries[0].owner, "r-a1");
  assert.equal(entries[0].ts, "2026-05-30T12:00:00.000Z");
});

test("parseDecisions: owner-only suffix sets owner, no ts", () => {
  const entries = parseDecisions("* keep config flat  (owner: cli)\n");
  assert.equal(entries.length, 1);
  assert.equal(entries[0].text, "keep config flat");
  assert.equal(entries[0].owner, "cli");
  assert.equal(entries[0].ts, undefined);
});

test("parseDecisions: ignores the header and non-bullet lines", () => {
  const raw = `${DECISIONS_HEADER}\n\nsome prose line\n- a real decision  (owner: cli, 2026-01-01T00:00:00.000Z)\n`;
  const entries = parseDecisions(raw);
  assert.equal(entries.length, 1);
  assert.equal(entries[0].text, "a real decision");
});

test("parseDecisions: empty / absent content yields no entries", () => {
  assert.deepEqual(parseDecisions(""), []);
  assert.deepEqual(parseDecisions("\n\n"), []);
});

// ---------------------------------------------------------------------------
// ensureDecisionsFile + appendDecision (IO-light, temp dir).
// ---------------------------------------------------------------------------

test("ensureDecisionsFile creates a tracked DECISIONS.md with the header", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-decisions-"));
  try {
    const file = await ensureDecisionsFile(dir);
    assert.equal(file, path.join(dir, "DECISIONS.md"));
    const content = await readFile(file, "utf8");
    assert.ok(content.startsWith(DECISIONS_HEADER), "header is the first line");
    // It is NOT gitignored: a .gitignore should not be created listing it.
    const gitignore = await readFile(path.join(dir, ".gitignore"), "utf8").catch(() => "");
    assert.ok(!gitignore.includes("DECISIONS.md"), "DECISIONS.md must not be gitignored");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("ensureDecisionsFile is idempotent and never clobbers existing content", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-decisions-"));
  try {
    await ensureDecisionsFile(dir);
    await writeFile(path.join(dir, "DECISIONS.md"), `${DECISIONS_HEADER}\n\n- existing entry\n`, "utf8");
    await ensureDecisionsFile(dir);
    const content = await readFile(path.join(dir, "DECISIONS.md"), "utf8");
    assert.ok(content.includes("- existing entry"), "existing content preserved");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("appendDecision appends a parseable (owner, ts) bullet", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-decisions-"));
  try {
    await appendDecision(dir, "use jj for isolation");
    const content = await readFile(path.join(dir, "DECISIONS.md"), "utf8");
    const entries = parseDecisions(content);
    assert.equal(entries.length, 1);
    assert.equal(entries[0].text, "use jj for isolation");
    assert.equal(entries[0].owner, "cli");
    assert.ok(entries[0].ts, "ts is populated");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// renderContract includes the three propagation rules.
// ---------------------------------------------------------------------------

test("renderContract injects the propagation rules incl. the adapt-on-change rule", () => {
  const spec = {
    runId: "r1",
    task: "do the thing",
    createdAt: "2026-01-01T00:00:00.000Z",
    repo: { root: "/tmp/repo", branch: "main", baseCommit: "abc", status: [] },
    instructionsFiles: [],
    acceptanceCriteria: ["criterion"],
    suggestedTests: [],
  };
  const contract = renderContract(spec);
  assert.equal(PROPAGATION_RULES.length, 4);
  for (const rule of PROPAGATION_RULES) {
    assert.ok(contract.includes(rule), `contract is missing rule: ${rule}`);
  }
  // The anti-drift rule: agents adapt to a changed plan rather than straying.
  assert.ok(
    PROPAGATION_RULES.some((rule) => /ADAPT/.test(rule) && /does not stray/.test(rule)),
    "an adapt-on-plan-change rule must be present",
  );
  // No em dashes in the rules (project constraint).
  for (const rule of PROPAGATION_RULES) {
    assert.ok(!rule.includes("—"), "propagation rule must not contain an em dash");
  }
});

// ---------------------------------------------------------------------------
// Resolver: prompt builder + the pure finalize decision.
// ---------------------------------------------------------------------------

test("buildResolverPrompt names both titles, the change, and the conflicted files", () => {
  const prompt = buildResolverPrompt({
    mergeChangeId: "mzzz",
    intoTitle: "integration trunk",
    nodeTitle: "wire parser into CLI",
    conflictedFiles: ["src/a.ts", "src/b.ts"],
  });
  assert.ok(prompt.includes("mzzz"));
  assert.ok(prompt.includes("integration trunk"));
  assert.ok(prompt.includes("wire parser into CLI"));
  assert.ok(prompt.includes("src/a.ts, src/b.ts"));
  assert.ok(prompt.includes("preserving BOTH intents"));
  assert.ok(prompt.includes("no remaining conflicts"));
  assert.ok(!prompt.includes("—"), "no em dashes in the prompt");
  // The resolver agent is spawned in /goal format too: objective = resolve the
  // conflict preserving both intents; success = no markers and jj resolve empty.
  assert.equal(prompt.split("\n")[0].slice(0, 5), "/goal");
  assert.ok(prompt.split("\n")[0].includes("resolve the merge conflict"));
  assert.ok(prompt.includes("Done when: no conflict markers remain and `jj resolve --list` is empty"));
});

test("resolverShouldFinalize: true only for a resolver run with no remaining conflicts", () => {
  // Resolver run, conflicts cleared -> finalize.
  assert.equal(resolverShouldFinalize({ resolverFor: "r-a1", remainingConflicts: [] }), true);
  // Resolver run, conflicts remain -> do not finalize (held blocked).
  assert.equal(resolverShouldFinalize({ resolverFor: "r-a1", remainingConflicts: ["src/a.ts"] }), false);
  // Not a resolver run -> never finalize via this path.
  assert.equal(resolverShouldFinalize({ resolverFor: undefined, remainingConflicts: [] }), false);
});
