import test from "node:test";
import assert from "node:assert/strict";

import { titleKey } from "../dist/improve/state.js";
import { parseFindingsJson, dedupeAgainstLedger, frictionScore } from "../dist/improve/mine.js";
import { rankFindings, scoreFinding, targetMetricFor } from "../dist/improve/rank.js";
import { parseVote } from "../dist/improve/judge.js";
import { computeMetrics, runHadMergeConflict } from "../dist/improve/collect.js";

const finding = (over = {}) => ({
  id: "f-test-0",
  class: "prompt",
  title: "Workers ignore the Depends-on block",
  detail: "detail",
  evidence: [],
  severity: 4,
  frequency: 3,
  suspectedSurfaces: ["native/src/tasks.rs"],
  ...over,
});

test("parseFindingsJson reads a fenced json array", () => {
  const output = 'Here are the findings:\n```json\n[{"title":"a","class":"ux","severity":2}]\n```\nDone.';
  const parsed = parseFindingsJson(output);
  assert.equal(parsed.length, 1);
  assert.equal(parsed[0].title, "a");
});

test("parseFindingsJson reads bare arrays and tolerates garbage", () => {
  assert.equal(parseFindingsJson('[{"title":"x"}]').length, 1);
  assert.equal(parseFindingsJson("no json here").length, 0);
  assert.equal(parseFindingsJson("```json\nnot valid\n```").length, 0);
});

test("dedupeAgainstLedger drops shipped titles and remembers rejections", () => {
  const key = titleKey("Workers ignore the Depends-on block");
  const shipped = [{ titleKey: key, status: "shipped", ts: "", cycleId: "", findingId: "", title: "", class: "" }];
  assert.equal(dedupeAgainstLedger([finding()], shipped).length, 0);

  const rejected = [
    { titleKey: key, status: "judge-rejected", detail: "frequency=3", ts: "", cycleId: "", findingId: "", title: "", class: "" },
  ];
  // Same frequency as the rejection: suppressed.
  assert.equal(dedupeAgainstLedger([finding({ frequency: 3 })], rejected).length, 0);
  // Frequency doubled since the rejection: resurfaces.
  assert.equal(dedupeAgainstLedger([finding({ frequency: 6 })], rejected).length, 1);
  // Unknown finding passes through.
  assert.equal(dedupeAgainstLedger([finding({ title: "something else entirely" })], shipped).length, 1);
});

test("rankFindings orders by severity x log-frequency x tractability and banks the rest", () => {
  const prompt = finding({ id: "a", title: "prompt issue", class: "prompt", severity: 3, frequency: 3 });
  const perf = finding({ id: "b", title: "perf issue", class: "perf", severity: 3, frequency: 3 });
  const crash = finding({ id: "c", title: "crash issue", class: "crash", severity: 5, frequency: 5 });
  const { selected, banked } = rankFindings([perf, prompt, crash], 2);
  assert.equal(selected.length, 2);
  assert.equal(selected[0].id, "c");
  assert.equal(selected[1].id, "a");
  assert.equal(banked.length, 1);
  assert.equal(banked[0].id, "b");
  assert.ok(scoreFinding(prompt) > scoreFinding(perf));
});

test("targetMetricFor maps classes to production metrics", () => {
  assert.equal(targetMetricFor("prompt"), "verifierMissRate");
  assert.equal(targetMetricFor("orchestration"), "mergeConflictRate");
  assert.equal(targetMetricFor("other"), "failedRate");
});

test("parseVote parses fenced verdicts and fails closed on garbage", () => {
  const vote = parseVote("correctness", '```json\n{"verdict":"approve","regression":false,"notes":"fine"}\n```');
  assert.equal(vote.verdict, "approve");
  assert.equal(vote.regression, false);

  const rejected = parseVote("correctness", "I cannot decide.");
  assert.equal(rejected.verdict, "reject");
});

test("computeMetrics computes rates over terminal sessions", () => {
  const base = {
    project: "p",
    repoRoot: "/tmp/p",
    runId: "r",
    task: "t",
    backend: "claude",
    status: "completed",
    createdAt: "",
    updatedAt: "",
    steerCount: 0,
    autoSteerCount: 0,
    tokensIn: 10,
    tokensOut: 20,
    errorExcerpts: [],
  };
  const snapshot = computeMetrics("c1", [
    { ...base },
    { ...base, status: "failed" },
    { ...base, status: "merge-conflict", steerCount: 2 },
    { ...base, verifierSatisfied: false },
  ]);
  assert.equal(snapshot.runsSeen, 4);
  assert.equal(snapshot.failedRate, 0.25);
  assert.equal(snapshot.mergeConflictRate, 0.25);
  assert.equal(snapshot.verifierMissRate, 0.25);
  assert.equal(snapshot.steerRate, 0.25);
  assert.equal(snapshot.totalTokensIn, 40);
});

test("runHadMergeConflict sees resolved conflicts, resolver runs, and legacy labels", () => {
  // Durable marker written by jj.ts and the native TUI.
  assert.equal(runHadMergeConflict({ status: "merged", hadMergeConflict: true }), true);
  // Live conflict state still counts.
  assert.equal(runHadMergeConflict({ status: "merge-conflict" }), true);
  assert.equal(runHadMergeConflict({ status: "completed", merge: { status: "conflict" } }), true);
  assert.equal(runHadMergeConflict({ status: "completed", sync: { status: "conflict" } }), true);
  // Daemon resolver runs carry resolverFor.
  assert.equal(runHadMergeConflict({ status: "completed", resolverFor: "n2" }), true);
  // Records written before hadMergeConflict existed: the native resolver labels
  // (start_conflict_resolution_agent) are the only durable trace.
  assert.equal(
    runHadMergeConflict({ status: "merged", taskSummary: "merge conflicts → Objective Create shared data model" }),
    true,
  );
  assert.equal(runHadMergeConflict({ status: "merged", taskSummary: "rebase conflicts → fix auth" }), true);
  assert.equal(runHadMergeConflict({ status: "merged", task: "Resolve merge conflicts: build auth" }), true);
  assert.equal(runHadMergeConflict({ status: "completed", task: "Resolve rebase conflicts" }), true);
  // Clean runs do not count.
  assert.equal(runHadMergeConflict({ status: "merged", task: "build auth", taskSummary: "build auth" }), false);
  assert.equal(runHadMergeConflict({ status: "completed", merge: { status: "merged" } }), false);
  // The label check is anchored: a task ABOUT merge conflicts is not a resolver.
  assert.equal(
    runHadMergeConflict({ status: "completed", taskSummary: "investigate why merge conflicts → happen" }),
    false,
  );
});

test("computeMetrics counts conflicts a resolver already fixed", () => {
  const base = {
    project: "p",
    repoRoot: "/tmp/p",
    runId: "r",
    task: "t",
    backend: "claude",
    status: "completed",
    createdAt: "",
    updatedAt: "",
    steerCount: 0,
    autoSteerCount: 0,
    tokensIn: 0,
    tokensOut: 0,
    errorExcerpts: [],
  };
  const snapshot = computeMetrics("c2", [
    // Resolved conflict: terminal status merged, live conflict state gone.
    { ...base, status: "merged", hadMergeConflict: true },
    { ...base },
  ]);
  assert.equal(snapshot.mergeConflictRate, 0.5);
});

test("frictionScore surfaces the worst sessions first", () => {
  const clean = { status: "completed", steerCount: 0, autoSteerCount: 0, errorExcerpts: [] };
  const bad = { status: "failed", steerCount: 3, autoSteerCount: 1, errorExcerpts: ["boom"], verifierSatisfied: false };
  assert.ok(frictionScore(bad) > frictionScore(clean));
});
