#!/usr/bin/env node
// Mutation harness for the TUI suite — institutionalizes the loop that found
// undo (three bugs), the orphan-reap gap, and the visual gap. Each entry
// plants ONE known bug in the real source, rebuilds, runs the TUI suite, and
// records whether it was CAUGHT (some test went red) or SURVIVED (all green).
// A survivor is a coverage hole; the goal is a mutation SCORE anyone can
// regenerate, so "the suite has teeth" stays true instead of decaying.
//
//   node tests/tui/mutate.mjs            # run all, print the score
//   node tests/tui/mutate.mjs undo gate  # run only matching ids/names
//
// Every mutation is restored after its run; the tree is left clean.
import { execFileSync, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const NATIVE = path.join(repo, "native", "src");

// id, file, find -> replace. Each is a real bug in a real code path a test
// should guard. Keep these small and behavioral, not cosmetic.
const MUTATIONS = [
  {
    id: "undo-noop",
    file: "main.rs",
    find: "    fn undo_selected_merge(&mut self) {\n",
    replace: "    fn undo_selected_merge(&mut self) {\n        if true { return; } // MUTANT\n",
    guards: "undo",
  },
  {
    id: "review-gate-skipped",
    file: "main.rs",
    find: "        if run.reviewed_at.is_none() {",
    replace: "        if false { // MUTANT",
    guards: "merge gate",
  },
  {
    id: "restart-loads-nothing",
    file: "gitio.rs",
    find: "pub(crate) fn load_persisted_agents(repo_root: &Path) -> Vec<AgentRun> {",
    replace:
      "pub(crate) fn load_persisted_agents(repo_root: &Path) -> Vec<AgentRun> {\n    if !repo_root.as_os_str().is_empty() { return Vec::new(); } // MUTANT",
    guards: "restart / plan survive",
  },
  {
    id: "orphan-reap",
    file: "pty_terminal.rs",
    find: "                        libc::kill(-pgid, libc::SIGTERM);\n                        libc::kill(-pgid, libc::SIGKILL);",
    replace: "                        libc::kill(pgid, libc::SIGTERM);\n                        libc::kill(pgid, libc::SIGKILL); // MUTANT",
    guards: "process-group reap",
  },
  {
    id: "status-color",
    file: "theme.rs",
    find: "        AgentStatus::Running => ST_RUNNING,",
    replace: "        AgentStatus::Running => ST_MERGED, // MUTANT",
    guards: "visual status color",
  },
  {
    id: "plan-approval-noop",
    file: "main.rs",
    find:
      "    fn approve_planned_queue_for(&mut self, plan_index: usize) {\n        if !self.plans[plan_index].awaiting_approval {",
    replace:
      "    fn approve_planned_queue_for(&mut self, plan_index: usize) {\n        if true { return; } // MUTANT\n        if !self.plans[plan_index].awaiting_approval {",
    guards: "plan pipeline",
  },
  {
    id: "empty-cwd",
    file: "gitio.rs",
    // Anchor on the unique comment so the whitespace-sensitive multi-line
    // match is unambiguous (bare `.filter(...)` recurs all over the loader).
    find: "// main-checkout agent anyway.\n        .filter(|value| !value.trim().is_empty())\n        .map(PathBuf::from)",
    replace: "// main-checkout agent anyway.\n        .map(PathBuf::from)",
    guards: "empty-path agent loads a usable cwd",
    unit: "a_record_with_an_empty",
  },
  {
    id: "rename-heal",
    file: "gitio.rs",
    find: 'const WORKTREES_MARKER: &str = "/.rudder-worktrees/";',
    replace: 'const WORKTREES_MARKER: &str = "/.rudder-worktrees-NEVER/";',
    guards: "repo rename heal",
    // The marker-rebase rule fires only for a field-observed shape (repoRoot
    // rewritten, worktree.path stale) the e2e cannot construct — so it is
    // guarded by this native unit test, not the screen.
    unit: "a_renamed_repo_heals",
  },
];

const filter = process.argv.slice(2);
const selected = filter.length
  ? MUTATIONS.filter((m) => filter.some((f) => m.id.includes(f) || m.guards.includes(f)))
  : MUTATIONS;

function restore() {
  execSync("git checkout -- native/src", { cwd: repo, stdio: "ignore" });
}
function build() {
  try {
    execFileSync("cargo", ["build", "--manifest-path", "native/Cargo.toml"], {
      cwd: repo,
      stdio: "ignore",
    });
    return true;
  } catch {
    return false;
  }
}

const results = [];
for (const m of selected) {
  restore();
  const filePath = path.join(NATIVE, m.file);
  const src = fs.readFileSync(filePath, "utf8");
  if (!src.includes(m.find)) {
    results.push({ ...m, outcome: "STALE (find string not present)" });
    console.error(`⚠ ${m.id}: find string not found — mutation is stale, fix it`);
    continue;
  }
  fs.writeFileSync(filePath, src.replace(m.find, m.replace));
  if (!build()) {
    results.push({ ...m, outcome: "BUILD-FAILED" });
    console.error(`⚠ ${m.id}: mutant did not compile`);
    continue;
  }
  let caught = false;
  let by = "tui";
  try {
    execFileSync("npm", ["run", "test:tui"], { cwd: repo, stdio: "ignore" });
  } catch {
    caught = true; // non-zero exit = a test went red = the mutation was caught
  }
  // Some behavior is guarded by a NATIVE UNIT test, not the screen — e.g. the
  // heal's marker-rebase rule fires only for a field-observed corruption shape
  // the e2e can't produce. Count those as caught if their unit test catches
  // them, so the score reflects TOTAL coverage, not just the TUI layer.
  if (!caught && m.unit) {
    try {
      execFileSync("cargo", ["test", "--manifest-path", "native/Cargo.toml", m.unit], {
        cwd: repo,
        stdio: "ignore",
      });
      // unit test passed under the mutation = it does NOT guard this = survivor
    } catch {
      caught = true;
      by = "unit";
    }
  }
  results.push({ ...m, outcome: caught ? "CAUGHT" : "SURVIVED", by });
  console.error(`${caught ? `✓ CAUGHT[${by}]` : "✗ SURVIVED"} ${m.id} (${m.guards})`);
}

restore();
build();

const scored = results.filter((r) => r.outcome === "CAUGHT" || r.outcome === "SURVIVED");
const caught = scored.filter((r) => r.outcome === "CAUGHT").length;
console.error("\n=== mutation score ===");
console.error(`${caught}/${scored.length} caught`);
for (const r of results.filter((r) => r.outcome === "SURVIVED")) {
  console.error(`  SURVIVOR: ${r.id} — nothing guards "${r.guards}"`);
}
// Exit non-zero if any mutation survived, so CI can gate on it.
process.exit(results.some((r) => r.outcome === "SURVIVED" || r.outcome === "STALE") ? 1 : 0);
