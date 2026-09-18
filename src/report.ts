import path from "node:path";
import fs from "node:fs";
import fsp from "node:fs/promises";
import type { RunRecord, RunStatus, BackendId } from "./types.js";
import {
  readJson,
  nowIso,
  shortenHome,
  slugPrefix,
} from "./util.js";
import { projectStateDir } from "./state.js";

/**
 * Reporting and analytics for Rudder runs.
 *
 * Collects runs from .rudder/runs/, filters them, aggregates metrics,
 * and outputs in multiple formats (table, json, markdown, csv).
 */

export interface ReportOptions {
  /** Output format: table (default), json, markdown, csv */
  format?: "table" | "json" | "markdown" | "csv";
  /** Filter by status */
  status?: RunStatus | RunStatus[];
  /** Filter by backend */
  backend?: BackendId | BackendId[];
  /** Filter by model name (partial match) */
  model?: string;
  /** Filter by creation date range (ISO 8601) */
  after?: string;
  before?: string;
  /** Show summary statistics */
  summary?: boolean;
  /** Limit number of results */
  limit?: number;
  /** Sort by column: created, updated, duration, tokens, status */
  sortBy?: "created" | "updated" | "duration" | "tokens" | "status";
  /** Reverse sort order */
  reverse?: boolean;
  /** Detailed output (include full task text, etc.) */
  detailed?: boolean;
}

export interface RunMetrics {
  id: string;
  task: string;
  taskSummary?: string;
  backend: BackendId;
  model?: string;
  status: RunStatus;
  createdAt: string;
  updatedAt: string;
  duration?: number; // seconds
  durationReadable?: string;
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  mergeStatus?: string;
  workspacePath?: string;
  inWorkspace?: boolean;
}

export interface ReportSummary {
  totalRuns: number;
  statusCounts: Record<RunStatus, number>;
  backendCounts: Record<BackendId, number>;
  avgDuration?: number;
  totalTokens?: number;
  avgTokens?: number;
  mergeConflictRate?: number;
  successRate?: number;
  timeRange?: {
    earliest: string;
    latest: string;
  };
}

const RUDDER_DIR = ".rudder";
const RUNS_DIR = "runs";

/**
 * Load all run records from .rudder/runs/
 */
export async function loadAllRuns(repoRoot: string): Promise<RunRecord[]> {
  const runsPath = path.join(repoRoot, RUDDER_DIR, RUNS_DIR);

  try {
    const entries = await fsp.readdir(runsPath, { withFileTypes: true });
    const runs: RunRecord[] = [];

    for (const entry of entries) {
      if (!entry.isDirectory()) continue;

      const runJsonPath = path.join(runsPath, entry.name, "run.json");
      const run = await readJson<RunRecord>(runJsonPath);

      if (run) {
        runs.push(run);
      }
    }

    return runs.sort((a, b) => {
      const dateA = new Date(b.createdAt);
      const dateB = new Date(a.createdAt);
      return dateA.getTime() - dateB.getTime();
    });
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") {
      return [];
    }
    throw e;
  }
}

/**
 * Calculate metrics from a run record
 */
function runToMetrics(run: RunRecord): RunMetrics {
  const created = new Date(run.createdAt);
  const updated = new Date(run.updatedAt);
  const duration = (updated.getTime() - created.getTime()) / 1000;

  return {
    id: run.id,
    task: run.task,
    taskSummary: run.taskSummary || run.taskSummaryLlm ? "auto-summarized" : undefined,
    backend: run.backend,
    model: run.model,
    status: run.status,
    createdAt: run.createdAt,
    updatedAt: run.updatedAt,
    duration,
    durationReadable: formatDuration(duration),
    inputTokens: run.tokens?.input,
    outputTokens: run.tokens?.output,
    totalTokens: (run.tokens?.input || 0) + (run.tokens?.output || 0) || undefined,
    mergeStatus: run.merge?.status,
    workspacePath: run.workspace?.path,
    inWorkspace: run.workspace?.enabled,
  };
}

/**
 * Format duration in seconds to human-readable string
 */
function formatDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  const hours = Math.round(seconds / 3600);
  const mins = Math.round((seconds % 3600) / 60);
  return mins > 0 ? `${hours}h${mins}m` : `${hours}h`;
}

/**
 * Filter runs based on options
 */
function filterRuns(runs: RunRecord[], options: ReportOptions): RunRecord[] {
  let filtered = runs;

  // Status filter
  if (options.status) {
    const statuses = Array.isArray(options.status)
      ? options.status
      : [options.status];
    filtered = filtered.filter(r => statuses.includes(r.status));
  }

  // Backend filter
  if (options.backend) {
    const backends = Array.isArray(options.backend)
      ? options.backend
      : [options.backend];
    filtered = filtered.filter(r => backends.includes(r.backend));
  }

  // Model filter
  if (options.model) {
    filtered = filtered.filter(r =>
      r.model?.toLowerCase().includes(options.model!.toLowerCase())
    );
  }

  // Date range filter
  if (options.after) {
    filtered = filtered.filter(r => r.createdAt >= options.after!);
  }
  if (options.before) {
    filtered = filtered.filter(r => r.createdAt <= options.before!);
  }

  // Limit
  if (options.limit) {
    filtered = filtered.slice(0, options.limit);
  }

  return filtered;
}

/**
 * Sort runs by specified column
 */
function sortRuns(
  runs: RunRecord[],
  options: ReportOptions
): RunMetrics[] {
  const metrics = runs.map(runToMetrics);
  const sortBy = options.sortBy || "created";
  const reverse = options.reverse || false;

  const sortFn = (a: RunMetrics, b: RunMetrics): number => {
    let cmp = 0;

    switch (sortBy) {
      case "created":
        cmp = a.createdAt.localeCompare(b.createdAt);
        break;
      case "updated":
        cmp = a.updatedAt.localeCompare(b.updatedAt);
        break;
      case "duration":
        cmp = (a.duration || 0) - (b.duration || 0);
        break;
      case "tokens":
        cmp = (a.totalTokens || 0) - (b.totalTokens || 0);
        break;
      case "status":
        cmp = a.status.localeCompare(b.status);
        break;
      default:
        cmp = 0;
    }

    return reverse ? -cmp : cmp;
  };

  return metrics.sort(sortFn);
}

/**
 * Generate summary statistics
 */
function generateSummary(runs: RunRecord[]): ReportSummary {
  const statusCounts: Record<RunStatus, number> = {
    created: 0,
    running: 0,
    steering: 0,
    verifying: 0,
    completed: 0,
    failed: 0,
    cancelled: 0,
    paused: 0,
    orphaned: 0,
    migrated: 0,
    "merge-conflict": 0,
    merged: 0,
  };

  const backendCounts: Record<BackendId, number> = {
    claude: 0,
    codex: 0,
    acpx: 0,
    opencode: 0,
  };

  let totalDuration = 0;
  let durableRuns = 0;
  let totalTokens = 0;
  let tokensRuns = 0;
  let mergeConflicts = 0;
  let successful = 0;

  for (const run of runs) {
    statusCounts[run.status]++;
    backendCounts[run.backend]++;

    const duration =
      (new Date(run.updatedAt).getTime() -
        new Date(run.createdAt).getTime()) /
      1000;

    if (run.status === "completed" || run.status === "merged") {
      totalDuration += duration;
      durableRuns++;
      successful++;
    } else if (
      run.status === "failed" ||
      run.status === "cancelled" ||
      run.status === "orphaned"
    ) {
      // Counts in denominator but not numerator
    } else {
      // In-flight runs might finish, don't count yet
    }

    if (run.tokens) {
      totalTokens += (run.tokens.input || 0) + (run.tokens.output || 0);
      tokensRuns++;
    }

    if (run.hadMergeConflict || run.merge?.status === "conflict") {
      mergeConflicts++;
    }
  }

  const timeRange =
    runs.length > 0
      ? {
          earliest: runs[runs.length - 1].createdAt,
          latest: runs[0].createdAt,
        }
      : undefined;

  return {
    totalRuns: runs.length,
    statusCounts,
    backendCounts,
    avgDuration: durableRuns > 0 ? totalDuration / durableRuns : undefined,
    totalTokens: totalTokens > 0 ? totalTokens : undefined,
    avgTokens: tokensRuns > 0 ? totalTokens / tokensRuns : undefined,
    mergeConflictRate:
      runs.length > 0 ? (mergeConflicts / runs.length) * 100 : undefined,
    successRate:
      runs.length > 0
        ? ((successful / runs.length) * 100)
        : undefined,
    timeRange,
  };
}

/**
 * Format as table (default)
 */
function formatTable(metrics: RunMetrics[], summary?: ReportSummary): string {
  const lines: string[] = [];

  // Header
  lines.push("ID                  | Task (first 30)          | Backend | Model             | Status           | Duration    | Tokens");
  lines.push("-".repeat(145));

  // Rows
  for (const m of metrics) {
    const taskDisplay = (m.task || "").substring(0, 30).padEnd(24);
    const modelDisplay = (m.model || "").substring(0, 17).padEnd(17);
    const tokens = m.totalTokens ? String(m.totalTokens.toLocaleString()) : "—";

    const row = [
      slugPrefix(m.id, "id", 20).padEnd(20),
      taskDisplay,
      m.backend.padEnd(7),
      modelDisplay,
      m.status.padEnd(16),
      String(m.durationReadable || "—").padEnd(11),
      tokens,
    ].join(" | ");

    lines.push(row);
  }

  // Summary
  if (summary) {
    lines.push("");
    lines.push("SUMMARY");
    lines.push("-".repeat(145));
    lines.push(`Total runs: ${summary.totalRuns}`);
    lines.push(`Statuses: ` + Object.entries(summary.statusCounts)
      .filter(([_, count]) => count > 0)
      .map(([status, count]) => `${status}=${count}`)
      .join(", "));
    lines.push(`Backends: ` + Object.entries(summary.backendCounts)
      .filter(([_, count]) => count > 0)
      .map(([backend, count]) => `${backend}=${count}`)
      .join(", "));
    if (summary.avgDuration !== undefined) {
      lines.push(
        `Avg duration (completed): ${formatDuration(summary.avgDuration)}`
      );
    }
    if (summary.totalTokens !== undefined) {
      lines.push(`Total tokens: ${summary.totalTokens.toLocaleString()}`);
    }
    if (summary.avgTokens !== undefined) {
      lines.push(`Avg tokens/run: ${Math.round(summary.avgTokens).toLocaleString()}`);
    }
    if (summary.mergeConflictRate !== undefined) {
      lines.push(
        `Merge conflict rate: ${summary.mergeConflictRate.toFixed(1)}%`
      );
    }
    if (summary.successRate !== undefined) {
      lines.push(`Success rate: ${summary.successRate.toFixed(1)}%`);
    }
    if (summary.timeRange) {
      lines.push(`Time range: ${summary.timeRange.earliest} to ${summary.timeRange.latest}`);
    }
  }

  return lines.join("\n");
}

/**
 * Format as JSON
 */
function formatJson(
  metrics: RunMetrics[],
  summary?: ReportSummary
): string {
  const output = {
    runs: metrics,
    ...(summary && { summary }),
  };
  return JSON.stringify(output, null, 2);
}

/**
 * Format as Markdown
 */
function formatMarkdown(metrics: RunMetrics[], summary?: ReportSummary): string {
  const lines: string[] = [];

  lines.push("# Rudder Runs Report");
  lines.push("");
  lines.push(
    `Generated: ${nowIso()}`
  );
  lines.push("");

  if (summary) {
    lines.push("## Summary");
    lines.push("");
    lines.push(`- **Total runs:** ${summary.totalRuns}`);

    if (summary.avgDuration !== undefined) {
      lines.push(`- **Avg duration:** ${formatDuration(summary.avgDuration)}`);
    }
    if (summary.totalTokens !== undefined) {
      lines.push(
        `- **Total tokens:** ${summary.totalTokens.toLocaleString()}`
      );
    }
    if (summary.avgTokens !== undefined) {
      lines.push(
        `- **Avg tokens per run:** ${Math.round(summary.avgTokens).toLocaleString()}`
      );
    }
    if (summary.successRate !== undefined) {
      lines.push(`- **Success rate:** ${summary.successRate.toFixed(1)}%`);
    }
    if (summary.mergeConflictRate !== undefined) {
      lines.push(`- **Merge conflict rate:** ${summary.mergeConflictRate.toFixed(1)}%`);
    }

    lines.push("");
    lines.push("### Status Breakdown");
    lines.push("");
    for (const [status, count] of Object.entries(summary.statusCounts)) {
      if (count > 0) {
        lines.push(`- ${status}: ${count}`);
      }
    }

    lines.push("");
    lines.push("### Backend Breakdown");
    lines.push("");
    for (const [backend, count] of Object.entries(summary.backendCounts)) {
      if (count > 0) {
        lines.push(`- ${backend}: ${count}`);
      }
    }

    lines.push("");
  }

  lines.push("## Runs");
  lines.push("");
  lines.push(
    "| ID | Task | Backend | Model | Status | Duration | Tokens |"
  );
  lines.push("|---|---|---|---|---|---|---|");

  for (const m of metrics) {
    const id = slugPrefix(m.id, "id", 8);
    const task = (m.task || "").substring(0, 40).replace(/\|/g, "\\|");
    const model = (m.model || "—").substring(0, 20);
    const tokens = m.totalTokens
      ? String(m.totalTokens.toLocaleString())
      : "—";

    lines.push(
      `| ${id} | ${task} | ${m.backend} | ${model} | ${m.status} | ${String(m.durationReadable || "—")} | ${tokens} |`
    );
  }

  return lines.join("\n");
}

/**
 * Format as CSV
 */
function formatCsv(metrics: RunMetrics[]): string {
  const lines: string[] = [];

  // Header
  lines.push(
    "ID,Task,Backend,Model,Status,CreatedAt,UpdatedAt,Duration(s),InputTokens,OutputTokens,TotalTokens,MergeStatus,InWorkspace"
  );

  // Rows
  for (const m of metrics) {
    const task = (m.task || "").replace(/"/g, '""'); // CSV escape
    const row = [
      m.id,
      `"${task}"`,
      m.backend,
      m.model || "",
      m.status,
      m.createdAt,
      m.updatedAt,
      m.duration?.toFixed(0) || "",
      m.inputTokens || "",
      m.outputTokens || "",
      m.totalTokens || "",
      m.mergeStatus || "",
      m.inWorkspace ? "yes" : "no",
    ];
    lines.push(row.join(","));
  }

  return lines.join("\n");
}

/**
 * Generate and output a report
 */
export async function generateReport(
  repoRoot: string,
  options: ReportOptions = {}
): Promise<string> {
  const runs = await loadAllRuns(repoRoot);
  const filtered = filterRuns(runs, options);
  const sorted = sortRuns(filtered, options);
  const summary = options.summary !== false ? generateSummary(filtered) : undefined;

  const format = options.format || "table";

  switch (format) {
    case "json":
      return formatJson(sorted, summary);
    case "markdown":
      return formatMarkdown(sorted, summary);
    case "csv":
      return formatCsv(sorted);
    case "table":
    default:
      return formatTable(sorted, summary);
  }
}

/**
 * Print report to stdout
 */
export async function printReport(
  repoRoot: string,
  options: ReportOptions = {}
): Promise<void> {
  try {
    const report = await generateReport(repoRoot, options);
    console.log(report);
  } catch (e) {
    console.error(
      `Failed to generate report: ${e instanceof Error ? e.message : String(e)}`
    );
    process.exit(1);
  }
}
