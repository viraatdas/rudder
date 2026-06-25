import fsp from "node:fs/promises";
import path from "node:path";

import { findRepoRoot } from "./git.js";
import { pathExists } from "./util.js";

export type ContextAuditSeverity = "low" | "medium" | "high";

export type ContextAuditFinding = {
  severity: ContextAuditSeverity;
  file: string;
  message: string;
};

export type ContextAuditReport = {
  repoRoot: string;
  generatedAt: string;
  findings: ContextAuditFinding[];
  files: Array<{ path: string; bytes: number; lines: number }>;
};

const CANDIDATE_FILES = [
  "AGENTS.md",
  "CLAUDE.md",
  ".claude/CLAUDE.md",
  ".claude/settings.json",
  ".claude/settings.local.json",
  "RUDDER.md",
  "DECISIONS.md",
  "RUDDER_SHARED.md",
];

export async function auditContext(repoRoot = findRepoRoot()): Promise<ContextAuditReport> {
  const files = await collectContextFiles(repoRoot);
  const findings: ContextAuditFinding[] = [];
  const seenLines = new Map<string, string>();
  const summaries: Array<{ path: string; bytes: number; lines: number }> = [];

  for (const rel of files) {
    const abs = path.join(repoRoot, rel);
    const text = await fsp.readFile(abs, "utf8").catch(() => "");
    const stat = await fsp.stat(abs).catch(() => null);
    const lines = text.split(/\r?\n/);
    summaries.push({ path: rel, bytes: stat?.size ?? Buffer.byteLength(text), lines: lines.length });
    if (lines.length > 500) {
      findings.push({ severity: "medium", file: rel, message: "context file is over 500 lines; consider splitting or pruning" });
    }
    if (Buffer.byteLength(text) > 64_000) {
      findings.push({ severity: "medium", file: rel, message: "context file is over 64KB; always-on context may be noisy" });
    }
    if (secretPattern().test(text)) {
      findings.push({ severity: rel === "RUDDER_SHARED.md" ? "low" : "high", file: rel, message: "secret-like token found in a context file" });
    }
    if (promptInjectionPattern().test(text)) {
      findings.push({ severity: "high", file: rel, message: "prompt-injection phrase found in a context file" });
    }
    for (const line of lines) {
      const normalized = line.trim().replace(/\s+/g, " ").toLowerCase();
      if (normalized.length < 48 || normalized.startsWith("#")) {
        continue;
      }
      const previous = seenLines.get(normalized);
      if (previous && previous !== rel) {
        findings.push({ severity: "low", file: rel, message: `duplicates a rule also present in ${previous}` });
        break;
      }
      seenLines.set(normalized, rel);
    }
  }

  return {
    repoRoot,
    generatedAt: new Date().toISOString(),
    findings,
    files: summaries.sort((a, b) => a.path.localeCompare(b.path)),
  };
}

export async function printContextAudit(opts: { json?: boolean; repoRoot?: string } = {}): Promise<void> {
  const report = await auditContext(opts.repoRoot);
  if (opts.json) {
    console.log(JSON.stringify(report, null, 2));
    return;
  }
  console.log(`Context audit for ${report.repoRoot}`);
  if (!report.findings.length) {
    console.log("No context issues found.");
  } else {
    for (const finding of report.findings) {
      console.log(`${finding.severity.toUpperCase()} ${finding.file}: ${finding.message}`);
    }
  }
  console.log("");
  console.log("Files:");
  for (const file of report.files) {
    console.log(`  ${file.path} (${file.lines} lines, ${file.bytes} bytes)`);
  }
}

async function collectContextFiles(repoRoot: string): Promise<string[]> {
  const files = new Set<string>();
  for (const rel of CANDIDATE_FILES) {
    if (await pathExists(path.join(repoRoot, rel))) {
      files.add(rel);
    }
  }
  for (const dir of [".claude/skills", ".claude/commands", ".claude/agents"]) {
    const abs = path.join(repoRoot, dir);
    const entries = await fsp.readdir(abs, { withFileTypes: true }).catch(() => []);
    for (const entry of entries) {
      if (entry.isFile() && entry.name.endsWith(".md")) {
        files.add(path.join(dir, entry.name));
      }
      if (entry.isDirectory()) {
        const nested = path.join(abs, entry.name, "SKILL.md");
        if (await pathExists(nested)) {
          files.add(path.join(dir, entry.name, "SKILL.md"));
        }
      }
    }
  }
  return [...files].sort();
}

function secretPattern(): RegExp {
  return /(sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,}|xox[baprs]-[A-Za-z0-9-]{16,}|api[_-]?key\s*=\s*[A-Za-z0-9_.-]{16,})/i;
}

function promptInjectionPattern(): RegExp {
  return /(ignore (all )?(previous|prior) instructions|you are now|developer mode|<\/system>|<<SYS>>|\[INST\])/i;
}
