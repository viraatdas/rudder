import { runCommand } from "../util.js";
import { execStep, tail, type ImproveSettings } from "./state.js";
import type { Proposal } from "./propose.js";

export type ShipResult =
  | { status: "shipped"; version: string }
  | { status: "branch-pushed"; branch: string }
  | { status: "push-conflict"; detail: string };

const MINUTE = 60 * 1000;

/**
 * Ship a judged proposal. In `ship` autonomy this follows the repo release
 * rule end to end: rebase onto the latest origin/main, re-typecheck, `npm
 * version patch` (commit + tag vX.Y.Z), and push commit + tag together so the
 * tag-driven release workflow publishes to npm. Non-force push: if main moved
 * between the rebase and the push, this fails cleanly and is recorded as a
 * push-conflict for the next cycle. In `propose` autonomy only the branch is
 * pushed for human review.
 */
export async function shipProposal(params: {
  settings: ImproveSettings;
  proposal: Proposal;
}): Promise<ShipResult> {
  const { settings, proposal } = params;
  const cwd = proposal.worktree;

  await assertAllowedRemote(settings);

  if (settings.autonomy === "propose") {
    const push = await runCommand("git", ["push", "-u", "origin", proposal.branch], {
      cwd,
      allowFailure: true,
    });
    if (push.code !== 0) {
      return { status: "push-conflict", detail: tail(push.stderr || push.stdout, 800) };
    }
    return { status: "branch-pushed", branch: proposal.branch };
  }

  // Rebase onto the freshest main so the version bump reads the latest
  // package.json (two winners in one cycle ship sequentially through here).
  const fetch = await runCommand("git", ["fetch", "origin", "main"], { cwd, allowFailure: true });
  if (fetch.code !== 0) {
    return { status: "push-conflict", detail: `fetch failed: ${tail(fetch.stderr, 400)}` };
  }
  const rebase = await runCommand("git", ["rebase", "origin/main"], { cwd, allowFailure: true });
  if (rebase.code !== 0) {
    await runCommand("git", ["rebase", "--abort"], { cwd, allowFailure: true });
    return { status: "push-conflict", detail: `rebase onto origin/main failed: ${tail(rebase.stderr, 800)}` };
  }

  // Cheap re-gate after the rebase; the full suites already passed pre-judge.
  const recheck = await execStep({
    command: "npm",
    args: ["run", "check"],
    cwd,
    timeoutMs: 10 * MINUTE,
  });
  if (recheck.code !== 0) {
    return { status: "push-conflict", detail: `post-rebase typecheck failed: ${tail(recheck.output, 800)}` };
  }

  const version = await runCommand(
    "npm",
    ["version", "patch", "-m", `improve: ${proposal.finding.title} (v%s)`],
    { cwd, allowFailure: true },
  );
  if (version.code !== 0) {
    return { status: "push-conflict", detail: `npm version patch failed: ${tail(version.stderr || version.stdout, 800)}` };
  }
  const tag = version.stdout.trim().split("\n").at(-1)?.trim() ?? "";
  if (!/^v\d+\.\d+\.\d+$/.test(tag)) {
    return { status: "push-conflict", detail: `unexpected npm version output: ${tail(version.stdout, 200)}` };
  }

  const push = await runCommand("git", ["push", "origin", `HEAD:main`, `refs/tags/${tag}`], {
    cwd,
    allowFailure: true,
  });
  if (push.code !== 0) {
    // Undo the local tag so a retry next cycle does not collide with it.
    await runCommand("git", ["tag", "-d", tag], { cwd, allowFailure: true });
    return { status: "push-conflict", detail: `push failed: ${tail(push.stderr || push.stdout, 800)}` };
  }
  return { status: "shipped", version: tag };
}

/**
 * The ship stage refuses to push anywhere but the configured rudder remote.
 * This is the guard that keeps an unattended loop from ever pushing another
 * project.
 */
async function assertAllowedRemote(settings: ImproveSettings): Promise<void> {
  const remote = await runCommand("git", ["remote", "get-url", "origin"], {
    cwd: settings.repoPath,
    allowFailure: true,
  });
  const url = remote.stdout.trim();
  if (remote.code !== 0 || !url.includes(settings.allowedRemote)) {
    throw new Error(
      `refusing to ship: origin (${url || "missing"}) does not match improve.allowedRemote (${settings.allowedRemote})`,
    );
  }
}
