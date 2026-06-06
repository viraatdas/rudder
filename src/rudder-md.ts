const RUDDER_GENERATED_START = "<!-- RUDDER_GENERATED_START -->";
const RUDDER_GENERATED_END = "<!-- RUDDER_GENERATED_END -->";
const PLAN_START = "RUDDER_PLAN_TASKS_START";
const PLAN_END = "RUDDER_PLAN_TASKS_END";

export function mergeGeneratedRudderMd(existing: string, generated: string): string {
  const wrapped = `${RUDDER_GENERATED_START}\n${generated.trimEnd()}\n${RUDDER_GENERATED_END}\n`;
  const start = existing.indexOf(RUDDER_GENERATED_START);
  if (start >= 0) {
    const endMarker = existing.indexOf(RUDDER_GENERATED_END, start);
    if (endMarker >= 0) {
      const end = endMarker + RUDDER_GENERATED_END.length;
      const prefix = existing.slice(0, start).trimEnd();
      const suffix = existing.slice(end).trimStart();
      return [prefix, wrapped.trimEnd(), suffix].filter(Boolean).join("\n\n") + "\n";
    }
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
