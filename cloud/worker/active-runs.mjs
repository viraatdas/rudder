import fs from "node:fs";
import path from "node:path";

export const ACTIVE_RUN_STATUSES = new Set([
  "created",
  "running",
  "steering",
  "verifying",
]);

/** Count Rudder runs that are actively working below one repository root. */
export function countActiveAgents(repoRoot = process.cwd()) {
  const runsDir = path.join(repoRoot, ".rudder", "runs");
  try {
    let active = 0;
    for (const entry of fs.readdirSync(runsDir, { withFileTypes: true })) {
      if (!entry.isDirectory()) {
        continue;
      }
      try {
        const raw = fs.readFileSync(path.join(runsDir, entry.name, "run.json"), "utf8");
        const record = JSON.parse(raw);
        if (ACTIVE_RUN_STATUSES.has(String(record?.status ?? "").toLowerCase())) {
          active += 1;
        }
      } catch {
        // A missing/corrupt record is not evidence of active work.
      }
    }
    return active;
  } catch {
    return 0;
  }
}
