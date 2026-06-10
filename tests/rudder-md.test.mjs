import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, stat, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import { loadInstructionFiles } from "../dist/brain.js";
import {
  mergeGeneratedRudderMd,
  rudderMdLockPath,
  withRudderMdLock,
} from "../dist/rudder-md.js";

const START = "<!-- RUDDER_GENERATED_START -->";
const END = "<!-- RUDDER_GENERATED_END -->";

function countOccurrences(haystack, needle) {
  return haystack.split(needle).length - 1;
}

// ---------------------------------------------------------------------------
// mergeGeneratedRudderMd: marker hygiene under corruption.
// ---------------------------------------------------------------------------

test("mergeGeneratedRudderMd: stray END inside the block region still yields one clean block", () => {
  const existing = [
    START,
    "old generated",
    END, // stray: the real block continues below
    "stale generated tail",
    END,
    "",
    "## Orchestrator notes",
    "- keep",
  ].join("\n");
  const merged = mergeGeneratedRudderMd(existing, "new generated\n");
  assert.equal(countOccurrences(merged, START), 1, "exactly one START marker");
  assert.equal(countOccurrences(merged, END), 1, "exactly one END marker");
  assert.match(merged, /new generated/);
  assert.doesNotMatch(merged, /old generated/);
  assert.match(merged, /## Orchestrator notes\n- keep/, "orchestrator content survives");
  // The rebuilt file must round-trip cleanly from here on.
  assert.equal(mergeGeneratedRudderMd(merged, "new generated\n"), merged);
});

test("mergeGeneratedRudderMd: duplicated generated blocks collapse to one", () => {
  const existing = [
    START,
    "old one",
    END,
    "",
    "## Notes between",
    "- keep middle",
    "",
    START,
    "old two",
    END,
    "",
    "## Notes after",
    "- keep tail",
  ].join("\n");
  const merged = mergeGeneratedRudderMd(existing, "new generated\n");
  assert.equal(countOccurrences(merged, START), 1, "exactly one START marker");
  assert.equal(countOccurrences(merged, END), 1, "exactly one END marker");
  assert.match(merged, /new generated/);
  assert.doesNotMatch(merged, /old one/);
  assert.doesNotMatch(merged, /old two/);
  assert.match(merged, /## Notes between\n- keep middle/);
  assert.match(merged, /## Notes after\n- keep tail/);
});

test("mergeGeneratedRudderMd: orphaned lone START is replaced, content preserved", () => {
  const existing = ["intro prose", "", START, "", "## Notes", "- keep"].join("\n");
  const merged = mergeGeneratedRudderMd(existing, "new generated\n");
  assert.equal(countOccurrences(merged, START), 1);
  assert.equal(countOccurrences(merged, END), 1);
  assert.match(merged, /^intro prose/);
  assert.match(merged, /## Notes\n- keep/);
});

test("mergeGeneratedRudderMd: single-block round-trip stays byte-identical", () => {
  const existing = [
    "preamble",
    "",
    START,
    "old generated",
    END,
    "",
    "## Orchestrator notes",
    "- keep",
  ].join("\n");
  const once = mergeGeneratedRudderMd(existing, "fresh\n");
  const twice = mergeGeneratedRudderMd(once, "fresh\n");
  assert.equal(twice, once);
});

// ---------------------------------------------------------------------------
// withRudderMdLock: cross-process advisory lock (mkdir-based).
// ---------------------------------------------------------------------------

test("withRudderMdLock serializes concurrent critical sections and releases the lock", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-mdlock-"));
  try {
    const events = [];
    const critical = (label) =>
      withRudderMdLock(dir, async () => {
        events.push(`${label}:start`);
        await new Promise((resolve) => setTimeout(resolve, 40));
        events.push(`${label}:end`);
        return label;
      });
    const results = await Promise.all([critical("a"), critical("b")]);
    assert.deepEqual(results.slice().sort(), ["a", "b"], "both callers get their return value");
    assert.equal(events.length, 4);
    // No interleaving: each start is immediately followed by its own end.
    assert.equal(events[0].split(":")[0], events[1].split(":")[0]);
    assert.equal(events[2].split(":")[0], events[3].split(":")[0]);
    // Released in finally: the lock dir is gone afterwards.
    await assert.rejects(stat(rudderMdLockPath(dir)));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("withRudderMdLock takes over a stale lock and still releases it", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-mdlock-"));
  try {
    const lockDir = rudderMdLockPath(dir);
    await mkdir(lockDir, { recursive: true });
    const old = new Date(Date.now() - 60_000);
    await utimes(lockDir, old, old);
    const started = Date.now();
    const result = await withRudderMdLock(dir, async () => "ran");
    assert.equal(result, "ran");
    assert.ok(Date.now() - started < 1_500, "stale lock is taken over, not waited out");
    await assert.rejects(stat(lockDir));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("withRudderMdLock proceeds (best-effort) when a fresh lock is never released", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-mdlock-"));
  try {
    const lockDir = rudderMdLockPath(dir);
    await mkdir(lockDir, { recursive: true });
    const result = await withRudderMdLock(dir, async () => "ran anyway");
    assert.equal(result, "ran anyway");
    // Not ours: a lock we failed to acquire must not be deleted on exit.
    const info = await stat(lockDir);
    assert.ok(info.isDirectory());
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// loadInstructionFiles: visible truncation marker.
// ---------------------------------------------------------------------------

test("loadInstructionFiles appends a visible marker only when a file is truncated", async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "rudder-instr-"));
  try {
    await writeFile(path.join(dir, "AGENTS.md"), "x".repeat(12_500), "utf8");
    await writeFile(path.join(dir, "README.md"), "short readme\n", "utf8");
    const files = await loadInstructionFiles(dir, dir);
    const agents = files.find((file) => file.path === "AGENTS.md");
    assert.ok(agents, "AGENTS.md is injected");
    assert.ok(
      agents.content.endsWith(
        "[Rudder: AGENTS.md truncated at 12000 of 12500 chars; read AGENTS.md in the workspace for the rest.]",
      ),
      `marker missing; tail was: ${agents.content.slice(-120)}`,
    );
    assert.ok(agents.content.startsWith("x".repeat(200)), "truncated body precedes the marker");
    const readme = files.find((file) => file.path === "README.md");
    assert.equal(readme.content, "short readme\n", "untruncated files carry no marker");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
