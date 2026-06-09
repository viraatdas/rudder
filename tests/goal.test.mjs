import assert from "node:assert/strict";
import test from "node:test";

import { DEFAULT_SUCCESS, deriveGoal, deriveSuccess, extractSuccess, formatGoalPrompt } from "../dist/goal.js";
import { renderContract } from "../dist/brain.js";

// ---------------------------------------------------------------------------
// The canonical objective-format convention: every spawned agent's launch prompt
// leads with `Objective: <objective>` then `Done when: <success>` then the body.
// ---------------------------------------------------------------------------

test("formatGoalPrompt caps the Objective + Done-when lines under the backend limit", () => {
  // Regression: launched prompts must keep the goal/success lines under the
  // backend's goal-condition limit; the body stays full.
  const prompt = formatGoalPrompt({
    goal: "G".repeat(4808),
    success: "S".repeat(4808),
    body: "the full task detail goes here and is never truncated",
  });
  const lines = prompt.split("\n");
  const goalLine = lines.find((l) => l.startsWith("Objective: "));
  const doneLine = lines.find((l) => l.startsWith("Done when: "));
  assert.ok([...goalLine.slice("Objective: ".length)].length <= 4000, "objective arg <= 4000 chars");
  assert.ok([...doneLine.slice("Done when: ".length)].length <= 4000, "done-when <= 4000 chars");
  assert.ok(prompt.includes("the full task detail goes here and is never truncated"), "body preserved");
  assert.ok(!prompt.startsWith("/goal"), "launch prompt must not start with slash command");
  assert.ok(!prompt.startsWith("Goal:"), "launch prompt must not start with Goal: either");
});

test("formatGoalPrompt emits the Objective + Done-when block, then the body", () => {
  const prompt = formatGoalPrompt({
    goal: "implement the parser",
    success: "cargo test passes",
    body: "Write the parser in src/parser.rs.",
  });
  assert.ok(prompt.startsWith("Objective: implement the parser\n"));
  assert.ok(prompt.includes("\nDone when: cargo test passes\n"));
  assert.ok(prompt.includes("Write the parser in src/parser.rs."));
  // Objective must be the very first line, without invoking a slash command/goal parser.
  assert.equal(prompt.split("\n")[0], "Objective: implement the parser");
  assert.equal(prompt.split("\n")[1], "Done when: cargo test passes");
});

test("formatGoalPrompt keeps a very long body out of slash-command parsing", () => {
  const prompt = formatGoalPrompt({
    goal: "Produce the seed JSON and Apify ingestion script.",
    success: "seed data and ingest script exist and run.",
    body: "Build the talent seed-data layer. ".repeat(280),
  });
  assert.ok(prompt.length > 8000, "test prompt should match the long-body failure");
  assert.ok(prompt.startsWith("Objective: Produce the seed JSON"));
  assert.ok(!prompt.startsWith("/goal"));
  assert.ok(!prompt.startsWith("Goal:"));
  assert.ok(prompt.includes("Build the talent seed-data layer."));
});

test("formatGoalPrompt collapses multiline objective/success to single lines", () => {
  const prompt = formatGoalPrompt({
    goal: "do\nthe\nthing",
    success: "tests\npass",
    body: "body",
  });
  assert.equal(prompt.split("\n")[0], "Objective: do the thing");
  assert.equal(prompt.split("\n")[1], "Done when: tests pass");
});

test("formatGoalPrompt falls back to defaults when goal/success are empty", () => {
  const prompt = formatGoalPrompt({ goal: "", success: "", body: "ship it" });
  assert.equal(prompt.split("\n")[0], "Objective: ship it");
  assert.equal(prompt.split("\n")[1], `Done when: ${DEFAULT_SUCCESS}`);
});

test("deriveGoal takes the first non-empty line", () => {
  assert.equal(deriveGoal("\n\nfix the login bug\nmore detail"), "fix the login bug");
});

test("deriveGoal recovers the objective from an already /goal-formatted task", () => {
  assert.equal(
    deriveGoal("/goal build the cache\nDone when: it works\n\nbody"),
    "build the cache",
  );
  assert.equal(
    deriveGoal("Goal: build the cache\nDone when: it works\n\nbody"),
    "build the cache",
  );
  assert.equal(
    deriveGoal("Objective: build the cache\nDone when: it works\n\nbody"),
    "build the cache",
  );
  assert.equal(
    deriveGoal("/goal\tship the cache\nDone when: it works\n\nbody"),
    "ship the cache",
  );
});

test("deriveGoal does not treat goal-prefixed words as the /goal command", () => {
  assert.equal(
    deriveGoal("/goalkeeper route should be documented"),
    "/goalkeeper route should be documented",
  );
});

test("deriveSuccess joins acceptance criteria, else the default", () => {
  assert.equal(deriveSuccess(["a", "b"]), "a; b");
  assert.equal(deriveSuccess([]), DEFAULT_SUCCESS);
  assert.equal(deriveSuccess(undefined), DEFAULT_SUCCESS);
});

test("extractSuccess reads the Done-when line, else undefined", () => {
  assert.equal(extractSuccess("/goal x\nDone when: tests pass\n\nbody"), "tests pass");
  assert.equal(extractSuccess("no goal block here"), undefined);
});

test("renderContract leads with the Objective + Done-when header", () => {
  const spec = {
    runId: "r1",
    task: "add a dark mode toggle",
    goal: "add a dark mode toggle",
    success: "the toggle works and tests pass",
    createdAt: "2026-01-01T00:00:00.000Z",
    repo: { root: "/tmp/repo", branch: "main", baseCommit: "abc", status: [] },
    instructionsFiles: [],
    acceptanceCriteria: ["Address the user's requested task directly.", "Keep the change scoped and reviewable."],
    suggestedTests: ["npm test"],
  };
  const contract = renderContract(spec);
  assert.equal(contract.split("\n")[0], "Objective: add a dark mode toggle");
  assert.equal(contract.split("\n")[1], "Done when: the toggle works and tests pass");
  // The existing body still follows the header.
  assert.ok(contract.includes("Task: add a dark mode toggle"));
  assert.ok(contract.includes("Acceptance criteria:"));
  assert.ok(contract.includes("Working rules:"));
});

test("renderContract derives goal/success when the spec omits them", () => {
  const spec = {
    runId: "r1",
    task: "refactor the scheduler",
    goal: "",
    success: "",
    createdAt: "2026-01-01T00:00:00.000Z",
    repo: { root: "/tmp/repo", branch: "main", baseCommit: "abc", status: [] },
    instructionsFiles: [],
    acceptanceCriteria: ["Address the user's requested task directly."],
    suggestedTests: [],
  };
  const contract = renderContract(spec);
  assert.equal(contract.split("\n")[0], "Objective: refactor the scheduler");
  assert.ok(contract.split("\n")[1].startsWith("Done when: "));
});
