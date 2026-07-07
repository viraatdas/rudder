import path from "node:path";
import { execStep, logsDir, tail } from "./state.js";
import type { Proposal } from "./propose.js";

export type GateResult = {
  passed: boolean;
  steps: Array<{ name: string; code: number; timedOut: boolean; failureTail?: string }>;
};

const MINUTE = 60 * 1000;

/**
 * The repo's own deterministic gates, run inside the proposal worktree before
 * any judge tokens are spent. Order matches AGENTS.md section 11: typecheck,
 * native tests when native/ changed, then the node test suite (which builds).
 */
export async function runGates(proposal: Proposal): Promise<GateResult> {
  const logFile = path.join(logsDir(), `${proposal.finding.id}-gates.log`);
  const nativeChanged = proposal.changedFiles.some((file: string) => file.startsWith("native/"));
  const steps: Array<{ name: string; command: string; args: string[]; timeoutMs: number }> = [
    {
      name: "npm ci",
      command: "npm",
      args: ["ci", "--no-audit", "--no-fund"],
      timeoutMs: 15 * MINUTE,
    },
    { name: "npm run check", command: "npm", args: ["run", "check"], timeoutMs: 10 * MINUTE },
    ...(nativeChanged
      ? [
          {
            name: "cargo test",
            command: "cargo",
            args: ["test", "--manifest-path", "native/Cargo.toml"],
            timeoutMs: 40 * MINUTE,
          },
        ]
      : []),
    { name: "npm test", command: "npm", args: ["test"], timeoutMs: 40 * MINUTE },
  ];

  const result: GateResult = { passed: true, steps: [] };
  for (const step of steps) {
    const outcome = await execStep({
      command: step.command,
      args: step.args,
      cwd: proposal.worktree,
      timeoutMs: step.timeoutMs,
      logFile,
    });
    const entry = {
      name: step.name,
      code: outcome.code,
      timedOut: outcome.timedOut,
      ...(outcome.code !== 0 ? { failureTail: tail(outcome.output, 4000) } : {}),
    };
    result.steps.push(entry);
    if (outcome.code !== 0) {
      result.passed = false;
      break;
    }
  }
  return result;
}

export function gateSummary(gate: GateResult): string {
  return gate.steps
    .map((step) => `${step.name}: ${step.code === 0 ? "ok" : step.timedOut ? "TIMEOUT" : `exit ${step.code}`}`)
    .join(", ");
}
