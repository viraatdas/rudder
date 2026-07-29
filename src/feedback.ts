// ---------------------------------------------------------------------------
// `/feedback` (dashboard) and `rudder feedback "..."` (CLI).
//
// A one-line gripe is only actionable with a little context, so a report carries
// what was on screen in structure rather than in prose: version, platform, the
// backend/model in play, how many agents were running, the last few notices, the
// last error. It never carries prompts, diffs, or file contents.
//
// Three destinations, in order of durability:
//   1. a local JSON file — always written first, so a report is never lost to a
//      network failure or a missing `gh`;
//   2. a PostHog `feedback` event — puts the complaint next to the usage data
//      that produced it;
//   3. a GitHub issue via `gh`, when it is installed and authenticated — a queue
//      that gets worked through rather than a metric that gets admired.
//
// Free text is REDACTED before it leaves the machine: a GitHub issue is public
// and a notice routinely contains an absolute path.
// ---------------------------------------------------------------------------

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { capture, projectHash } from "./analytics.js";
import { dashboardRoot } from "./handoff.js";
import { commandExists, runCommand, runCommandSync } from "./util.js";

const DEFAULT_FEEDBACK_REPO = "viraatdas/rudder";

export interface FeedbackContext {
  /** Backend/model/effort the dashboard would use for a new agent. */
  backend?: string;
  model?: string;
  effort?: string;
  agents?: number;
  agentsRunning?: number;
  /** The most recent notice lines, oldest first. Redacted before sending. */
  notices?: string[];
  lastError?: string;
  /** Which pane was focused, and what the user was looking at. */
  focus?: string;
  view?: string;
  planActive?: boolean;
}

export interface FeedbackReport {
  localPath: string;
  posthog: boolean;
  issueUrl?: string;
  /** Why a destination was skipped, for an honest confirmation message. */
  skipped: string[];
}

/**
 * Strip anything that identifies a machine or a person from free text: home
 * directories, absolute paths, and token-shaped strings. A GitHub issue is
 * public, so this runs before every send — not only before PostHog.
 */
export function redactText(text: string, home = os.homedir()): string {
  let out = text.replace(/\r/g, "");
  if (home && home.length > 1) {
    out = out.split(home).join("~");
  }
  // Home-relative paths, collapsed BEFORE the absolute-path pass so the leading
  // "~" does not survive as "~<path>".
  out = out.replace(/~(?:\/[A-Za-z0-9._-]+)+\/?/g, "<path>");
  // Absolute paths (including the /private/var forms macOS hands out).
  out = out.replace(/(?:\/[A-Za-z0-9._~-]+){2,}\/?/g, (match) =>
    match.length > 3 ? "<path>" : match,
  );
  out = out.replace(/\b(sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{8,}|phc_[A-Za-z0-9]{8,}|xox[baprs]-[A-Za-z0-9-]{8,})\b/g, "<redacted>");
  return out.trim();
}

export function feedbackDir(repoRoot: string): string {
  return path.join(repoRoot, ".rudder", "feedback");
}

/**
 * Where issues get filed. A fork should collect its OWN feedback, so this reads
 * the checkout's GitHub origin and only falls back to Rudder upstream when there
 * is no GitHub remote to infer.
 */
export function feedbackRepo(repoRoot: string): string {
  const override = process.env.RUDDER_FEEDBACK_REPO?.trim();
  if (override) {
    return override;
  }
  const remote = runCommandSync("git", ["remote", "get-url", "origin"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return parseGithubSlug(remote.stdout) ?? DEFAULT_FEEDBACK_REPO;
}

/** `owner/repo` from any GitHub remote form (https, ssh, with or without .git). */
export function parseGithubSlug(remote: string): string | undefined {
  const match = /github\.com[:/]+([^/\s]+)\/([^/\s]+?)(?:\.git)?\s*$/.exec(remote.trim());
  if (!match) {
    return undefined;
  }
  return `${match[1]}/${match[2]}`;
}

export async function submitFeedback(options: {
  text: string;
  context?: FeedbackContext;
  repoRoot?: string;
  /** Skip the GitHub issue (used by tests and by --no-issue). */
  issue?: boolean;
}): Promise<FeedbackReport> {
  const repoRoot = options.repoRoot ?? dashboardRoot();
  const text = options.text.trim();
  if (!text) {
    throw new Error('Nothing to send. Usage: rudder feedback "merge asked twice and did nothing"');
  }
  const context = options.context ?? {};
  const redacted = redactText(text);
  const notices = (context.notices ?? []).slice(-3).map((notice) => redactText(notice));
  const lastError = context.lastError ? redactText(context.lastError) : undefined;
  const skipped: string[] = [];

  // 1. Local first: whatever else fails, the report exists.
  const dir = feedbackDir(repoRoot);
  fs.mkdirSync(dir, { recursive: true });
  const localPath = path.join(dir, `${Date.now()}.json`);
  fs.writeFileSync(
    localPath,
    `${JSON.stringify(
      {
        text,
        redacted,
        context: { ...context, notices, lastError },
        createdAt: new Date().toISOString(),
      },
      null,
      2,
    )}\n`,
  );

  // 2. PostHog: the aggregate view.
  const posthog = await capture("feedback", {
    text: redacted,
    backend: context.backend,
    model: context.model,
    effort: context.effort,
    agents: context.agents,
    agents_running: context.agentsRunning,
    focus: context.focus,
    view: context.view,
    plan_active: context.planActive,
    last_error: lastError,
    notice_count: notices.length,
    project: projectHash(repoRoot),
  });
  if (!posthog) {
    skipped.push("telemetry is off, so nothing was sent to the usage project");
  }

  // 3. GitHub: the triage queue. Optional by design — `gh` is not a dependency.
  let issueUrl: string | undefined;
  if (options.issue === false) {
    skipped.push("issue skipped (--no-issue)");
  } else if (!commandExists("gh")) {
    skipped.push("gh not installed, so no issue was filed");
  } else {
    const auth = await runCommand("gh", ["auth", "status"], { allowFailure: true });
    if (auth.code !== 0) {
      skipped.push("gh is not authenticated (`gh auth login`), so no issue was filed");
    } else {
      const body = issueBody(redacted, { ...context, notices, lastError });
      const created = await runCommand(
        "gh",
        [
          "issue",
          "create",
          "--repo",
          feedbackRepo(repoRoot),
          "--title",
          `feedback: ${issueTitle(redacted)}`,
          "--body",
          body,
        ],
        { allowFailure: true },
      );
      const url = created.stdout.trim().split(/\s+/).find((token) => token.startsWith("http"));
      if (created.code === 0 && url) {
        issueUrl = url;
      } else {
        skipped.push("gh could not create the issue");
      }
    }
  }

  return { localPath, posthog, issueUrl, skipped };
}

export function issueTitle(text: string): string {
  const firstLine = text.split("\n").find((line) => line.trim().length > 0) ?? "report";
  return firstLine.trim().slice(0, 72);
}

export function issueBody(text: string, context: FeedbackContext): string {
  const rows: string[] = [];
  const push = (label: string, value: string | number | boolean | undefined) => {
    if (value !== undefined && value !== "") {
      rows.push(`- ${label}: ${value}`);
    }
  };
  push("backend", context.backend);
  push("model", context.model);
  push("effort", context.effort);
  push("agents", context.agents);
  push("running", context.agentsRunning);
  push("focus", context.focus);
  push("view", context.view);
  push("plan active", context.planActive);
  push("last error", context.lastError);
  const notices = context.notices ?? [];
  const noticeBlock = notices.length
    ? `\n\nRecent notices:\n${notices.map((notice) => `- ${notice}`).join("\n")}`
    : "";
  return `${text}\n\nContext (auto-collected, paths redacted):\n${rows.join("\n")}${noticeBlock}\n\n_Filed by \`rudder feedback\`._`;
}
