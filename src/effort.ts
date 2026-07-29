import type { BackendId, EffortLevel } from "./types.js";
import { ensureRudderCodexBinary } from "./codex-binary.js";
import { runCommand } from "./util.js";

export type EffortOption = {
  label: string;
  value?: EffortLevel;
  detail: string;
};

const FALLBACK_EFFORTS: EffortLevel[] = ["low", "medium", "high", "xhigh"];

export async function discoverEffortOptions(backend: BackendId): Promise<EffortOption[]> {
  const values = backend === "claude"
    ? await discoverClaudeEfforts()
    : await discoverCodexEfforts();
  return [
    { label: "auto", value: undefined, detail: "let the selected model choose" },
    ...values.map((value) => ({
      label: value,
      value,
      detail: effortDetail(value),
    })),
  ];
}

export function fallbackEffortOptions(backend: BackendId): EffortOption[] {
  return [
    { label: "auto", value: undefined, detail: "let the selected model choose" },
    ...(backend === "claude" ? [...FALLBACK_EFFORTS, "max" as EffortLevel] : FALLBACK_EFFORTS)
      .map((value) => ({ label: value, value, detail: effortDetail(value) })),
  ];
}

export function normalizeEffortForBackend(backend: BackendId, effort: EffortLevel | undefined): EffortLevel | undefined {
  if (!effort) {
    return undefined;
  }
  // opencode exposes no reasoning-effort flag at all, so a stored effort is
  // dropped rather than passed to a CLI that would reject it.
  if (backend === "opencode") {
    return undefined;
  }
  if (backend === "codex" && effort === "max") {
    return "xhigh";
  }
  return effort;
}

async function discoverClaudeEfforts(): Promise<EffortLevel[]> {
  const help = await runCommand("claude", ["--help"], { allowFailure: true });
  const values = parseEfforts(help.stdout || help.stderr);
  return values.length ? values : [...FALLBACK_EFFORTS, "max"];
}

async function discoverCodexEfforts(): Promise<EffortLevel[]> {
  const codex = await ensureRudderCodexBinary();
  const help = await runCommand(codex, ["exec", "--help"], { allowFailure: true });
  const values = parseEfforts(help.stdout || help.stderr).filter((value) => value !== "max");
  return values.length ? values : FALLBACK_EFFORTS;
}

function parseEfforts(text: string): EffortLevel[] {
  const found = new Set<EffortLevel>();
  const effortLine = text
    .split(/\r?\n/)
    .find((line) => /effort|reasoning/i.test(line)) ?? "";
  for (const value of ["low", "medium", "high", "xhigh", "max"] as EffortLevel[]) {
    if (new RegExp(`\\b${value}\\b`, "i").test(effortLine)) {
      found.add(value);
    }
  }
  return [...found];
}

function effortDetail(value: EffortLevel): string {
  if (value === "low") return "fastest explicit override";
  if (value === "medium") return "balanced explicit override";
  if (value === "high") return "deeper explicit override";
  if (value === "xhigh") return "extra explicit override";
  return "maximum explicit override";
}
