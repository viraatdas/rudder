import fs from "node:fs";
import path from "node:path";

export const ACTIVE_RUN_STATUSES = new Set(["created", "running", "steering", "verifying"]);

export function countActiveAgents(repoRoot = process.cwd()) {
  const runsDir = path.join(repoRoot, ".rudder", "runs");
  try {
    let active = 0;
    for (const entry of fs.readdirSync(runsDir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      try {
        const record = JSON.parse(fs.readFileSync(path.join(runsDir, entry.name, "run.json"), "utf8"));
        if (ACTIVE_RUN_STATUSES.has(String(record?.status ?? "").toLowerCase())) active += 1;
      } catch {
        // A missing or corrupt record is not evidence of active work.
      }
    }
    return active;
  } catch {
    return 0;
  }
}
