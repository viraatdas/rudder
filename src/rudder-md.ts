import fsp from "node:fs/promises";
import path from "node:path";

const RUDDER_GENERATED_START = "<!-- RUDDER_GENERATED_START -->";
const RUDDER_GENERATED_END = "<!-- RUDDER_GENERATED_END -->";
const PLAN_START = "RUDDER_PLAN_TASKS_START";
const PLAN_END = "RUDDER_PLAN_TASKS_END";

// A well-formed generated block. Non-greedy + global so repeated blocks (from a
// prior corruption) are each removed instead of swallowing orchestrator content
// between them.
const GENERATED_BLOCK_RE = new RegExp(
  `${RUDDER_GENERATED_START.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}[\\s\\S]*?${RUDDER_GENERATED_END.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`,
  "g",
);

// Remove every well-formed generated block, then any orphaned single markers a
// stray literal (e.g. in orchestrator prose) or torn write left behind, so the
// rebuilt file always carries exactly one marker pair.
function stripGeneratedMarkers(text: string): string {
  return text
    .replace(GENERATED_BLOCK_RE, "")
    .split(RUDDER_GENERATED_START)
    .join("")
    .split(RUDDER_GENERATED_END)
    .join("");
}

export function mergeGeneratedRudderMd(existing: string, generated: string): string {
  const wrapped = `${RUDDER_GENERATED_START}\n${generated.trimEnd()}\n${RUDDER_GENERATED_END}\n`;
  const startIdx = existing.indexOf(RUDDER_GENERATED_START);
  const endIdx = existing.indexOf(RUDDER_GENERATED_END);
  if (startIdx >= 0 || endIdx >= 0) {
    // The fresh block goes where the first marker sat so it keeps its position
    // relative to orchestrator content; everything before that point cannot
    // contain markers and is preserved verbatim.
    const insertAt =
      startIdx >= 0 && (endIdx < 0 || startIdx < endIdx) ? startIdx : endIdx;
    const prefix = existing.slice(0, insertAt);
    const suffix = stripGeneratedMarkers(existing.slice(insertAt));
    // suffix.trim() (not trimStart) keeps repeated renders byte-identical:
    // trailing newlines would otherwise accumulate one per merge.
    return [prefix.trimEnd(), wrapped.trimEnd(), suffix.trim()].filter(Boolean).join("\n\n") + "\n";
  }

  const plan = latestRudderPlanBlock(existing);
  return plan ? `${wrapped}\n## Orchestrator-authored plan\n\n${plan}\n` : wrapped;
}

export function latestRudderPlanBlock(text: string): string | null {
  let current: string[] | null = null;
  let latest: string | null = null;
  for (const line of text.replace(/\r/g, "").split("\n")) {
    const trimmed = line.trim();
    if (trimmed === PLAN_START) {
      current = [PLAN_START];
    } else if (trimmed === PLAN_END) {
      if (current) {
        current.push(PLAN_END);
        latest = current.join("\n");
      }
      current = null;
    } else if (current) {
      current.push(line);
    }
  }
  return latest;
}

const LOCK_RETRY_MS = 50;
const LOCK_WAIT_MS = 2_000;
const LOCK_STALE_MS = 10_000;

export function rudderMdLockPath(repoRoot: string): string {
  return path.join(repoRoot, ".rudder", "rudder-md.lock");
}

/**
 * Cross-process advisory lock around RUDDER.md read-modify-write. The Rust
 * TUI, CLI invocations, the daemon, and the orchestrator agent all write the
 * file; mkdir is atomic across processes so it serializes the merge and keeps
 * interleavings from dropping a freshly written DAG or control marker.
 * Best-effort by design: a lock older than 10s is treated as stale and taken
 * over, and if acquisition never succeeds the caller proceeds unlocked, so a
 * crashed holder can never deadlock or fail a writer.
 */
export async function withRudderMdLock<T>(repoRoot: string, fn: () => Promise<T>): Promise<T> {
  const lockDir = rudderMdLockPath(repoRoot);
  let acquired = false;
  const deadline = Date.now() + LOCK_WAIT_MS;
  try {
    await fsp.mkdir(path.dirname(lockDir), { recursive: true }).catch(() => undefined);
    for (;;) {
      try {
        // recursive:false so an existing dir (= lock held) throws.
        await fsp.mkdir(lockDir);
        acquired = true;
        break;
      } catch {
        const age = await fsp
          .stat(lockDir)
          .then((st) => Date.now() - st.mtimeMs)
          .catch(() => null);
        if (age !== null && age > LOCK_STALE_MS) {
          // Holder likely crashed mid-write; take the lock over.
          await fsp.rm(lockDir, { recursive: true, force: true }).catch(() => undefined);
          continue;
        }
        if (Date.now() >= deadline) {
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, LOCK_RETRY_MS));
      }
    }
  } catch {
    // Lock machinery must never fail the caller; fall through unlocked.
  }
  try {
    return await fn();
  } finally {
    if (acquired) {
      await fsp.rm(lockDir, { recursive: true, force: true }).catch(() => undefined);
    }
  }
}
