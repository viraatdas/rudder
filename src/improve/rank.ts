import type { Finding, FindingClass } from "./state.js";

/**
 * Static tractability weights: how likely a one-shot autonomous agent is to
 * land a safe fix in that area. Prompt/text changes are cheap and low-risk;
 * native scheduler/merge changes are not. Phase 4 of the design feeds shipped
 * outcomes back into these; until then they are priors.
 */
export const TRACTABILITY: Record<FindingClass, number> = {
  prompt: 1.0,
  ux: 0.8,
  orchestration: 0.6,
  crash: 0.6,
  perf: 0.5,
  other: 0.4,
};

export function scoreFinding(finding: Finding): number {
  const tractability = TRACTABILITY[finding.class] ?? 0.4;
  return round2(finding.severity * Math.log(1 + finding.frequency) * tractability);
}

/** Rank findings best-first and return the top N; the rest is banked. */
export function rankFindings(findings: Finding[], maxFindings: number): {
  selected: Finding[];
  banked: Finding[];
} {
  const scored = findings
    .map((finding) => ({ ...finding, score: scoreFinding(finding) }))
    .sort((a, b) => (b.score ?? 0) - (a.score ?? 0));
  return {
    selected: scored.slice(0, Math.max(0, maxFindings)),
    banked: scored.slice(Math.max(0, maxFindings)),
  };
}

/** The production metric a finding class is expected to move (see §8). */
export function targetMetricFor(findingClass: FindingClass): string {
  switch (findingClass) {
    case "prompt":
      return "verifierMissRate";
    case "ux":
      return "steerRate";
    case "orchestration":
      return "mergeConflictRate";
    case "perf":
      return "medianDurationMs";
    case "crash":
      return "failedRate";
    default:
      return "failedRate";
  }
}

function round2(value: number): number {
  return Math.round(value * 100) / 100;
}
