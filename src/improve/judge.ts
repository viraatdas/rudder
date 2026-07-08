import { callAdvisedTextModel } from "./advisor.js";
import { gateSummary, type GateResult } from "./gate.js";
import type { SpendMeter } from "./state.js";
import type { Proposal } from "./propose.js";

export type JudgeVote = {
  lens: string;
  verdict: "approve" | "reject";
  regression: boolean;
  notes: string;
};

export type JudgePanelResult = {
  ship: boolean;
  votes: JudgeVote[];
};

/**
 * Three judges with distinct lenses, each prompted to REFUTE the change. A
 * proposal ships only when no judge flags a regression and a majority
 * approves. (The blind A/B eval-replay tier from the design doc arrives with
 * the eval corpus; until then the panel judges the diff adversarially.)
 */
const LENSES: Array<{ lens: string; charge: string }> = [
  {
    lens: "correctness",
    charge:
      "Does this diff actually address the finding at its root cause? Look for fixes that only paper over symptoms, dead code paths, or changes that cannot affect the reported behavior.",
  },
  {
    lens: "regression-risk",
    charge:
      "What else could this diff break? Hunt for violated invariants (atomic writes, signal wiring, scheduler locks, parity fixtures), changed behavior outside the finding's scope, and missing test coverage for the risky part.",
  },
  {
    lens: "simplicity",
    charge:
      "Is there a materially smaller or more conventional change that achieves the same fix? Flag scope creep, new dependencies, gratuitous refactors, and style that fights the surrounding code.",
  },
];

const JUDGE_SYSTEM = `You are one judge on the review panel of Rudder's continual improvement loop. An autonomous agent proposed a diff against the rudder repo to fix a telemetry-mined finding. Your default stance is skeptical: try to refute the change. Deterministic gates (typecheck + test suites) already passed; your job is what tests cannot see.
Return exactly one fenced json block: {"verdict": "approve" | "reject", "regression": true | false, "notes": "2-4 sentences"}.
Set regression=true when you can articulate a concrete way this diff makes Rudder worse for users, not for hypothetical style concerns.`;

export async function judgePanel(params: {
  proposal: Proposal;
  gate: GateResult;
  model: string;
  advisorModel: string;
  meter: SpendMeter;
}): Promise<JudgePanelResult> {
  const { proposal } = params;
  const user = [
    `FINDING: [${proposal.finding.class}, severity ${proposal.finding.severity}/5] ${proposal.finding.title}`,
    proposal.finding.detail,
    "",
    `AGENT'S OWN SUMMARY: ${proposal.resultNote?.summary ?? "(none provided)"}`,
    `AGENT'S SELF-DECLARED RISKS: ${proposal.resultNote?.risks ?? "(none provided)"}`,
    `GATES: ${gateSummary(params.gate)}`,
    "",
    `DIFF STAT:\n${proposal.diffStat}`,
    "",
    `FULL DIFF:\n${proposal.diffText}`,
  ].join("\n");

  const votes: JudgeVote[] = [];
  for (const { lens, charge } of LENSES) {
    if (!params.meter.canAfford(0.15)) {
      // Out of budget mid-panel: fail closed. An unjudged change never ships.
      votes.push({ lens, verdict: "reject", regression: false, notes: "budget exhausted before this vote" });
      continue;
    }
    const system = `${JUDGE_SYSTEM}\n\nYOUR LENS: ${lens}. ${charge}`;
    let output = "";
    try {
      output = await callAdvisedTextModel({
        executorModel: params.model,
        advisorModel: params.advisorModel,
        system,
        user,
        maxTokens: 1024,
        timeoutMs: 240000,
        meter: params.meter,
      });
    } catch (error) {
      votes.push({ lens, verdict: "reject", regression: false, notes: `judge call failed: ${String(error)}` });
      continue;
    }
    votes.push(parseVote(lens, output));
  }

  return { ship: panelDecision(votes), votes };
}

/**
 * Ship iff: no judge flags a concrete regression, at least 2/3 approve, AND
 * the correctness lens approved. Correctness carries veto weight alongside
 * regression flags: a change the correctness judge rejects "fixes" nothing,
 * and shipping it both wastes a release and marks the finding shipped, muting
 * it from future mining until an outcome check disproves it weeks later.
 * Simplicity alone stays outvotable (a working fix that could be smaller
 * still ships).
 */
export function panelDecision(votes: JudgeVote[]): boolean {
  const anyRegression = votes.some((vote) => vote.regression);
  const approvals = votes.filter((vote) => vote.verdict === "approve").length;
  const correctnessApproved = votes.some(
    (vote) => vote.lens === "correctness" && vote.verdict === "approve",
  );
  return !anyRegression && approvals >= 2 && correctnessApproved;
}

export function parseVote(lens: string, output: string): JudgeVote {
  const fenced = output.match(/```(?:json)?\s*([\s\S]*?)```/);
  const candidates = [fenced?.[1], output];
  for (const candidate of candidates) {
    if (!candidate) continue;
    const start = candidate.indexOf("{");
    const end = candidate.lastIndexOf("}");
    if (start < 0 || end <= start) continue;
    try {
      const value = JSON.parse(candidate.slice(start, end + 1)) as {
        verdict?: unknown;
        regression?: unknown;
        notes?: unknown;
      };
      return {
        lens,
        verdict: value.verdict === "approve" ? "approve" : "reject",
        regression: value.regression === true,
        notes: String(value.notes ?? "").slice(0, 600),
      };
    } catch {
      // try next candidate
    }
  }
  // Unparseable output fails closed.
  return { lens, verdict: "reject", regression: false, notes: "unparseable judge output" };
}

export function panelSummary(panel: JudgePanelResult): string {
  return panel.votes
    .map((vote) => `${vote.lens}=${vote.verdict}${vote.regression ? "(regression)" : ""}`)
    .join(", ");
}
