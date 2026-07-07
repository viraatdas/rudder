const DEFAULT_MAX_CHARS = 56;

const LEADING_PATTERNS = [
  /^(?:ok(?:ay)?|hey)[, ]+/i,
  /^(?:also\s+)?(?:please\s+)?(?:can|could|would)\s+(?:you|u)\s+/i,
  /^(?:also\s+)?(?:please\s+)?(?:can|could|would)\s+we\s+/i,
  /^(?:please\s+)+/i,
  /^(?:i\s+)?(?:need|want)\s+(?:you\s+)?to\s+/i,
  /^(?:we\s+)?(?:need|should|have)\s+to\s+/i,
  /^another thing(?: for you to work on)? is\s+/i,
  /^the task is\s+/i,
];

const STOP_WORDS = new Set([
  "a",
  "an",
  "and",
  "are",
  "as",
  "at",
  "be",
  "but",
  "by",
  "for",
  "from",
  "gets",
  "have",
  "in",
  "is",
  "it",
  "its",
  "just",
  "of",
  "on",
  "or",
  "put",
  "puts",
  "putting",
  "right",
  "so",
  "than",
  "that",
  "the",
  "then",
  "this",
  "to",
  "user",
  "what",
  "when",
  "where",
  "with",
  "you",
  "your",
]);

export function summarizeTask(task: string, maxChars = DEFAULT_MAX_CHARS): string {
  const original = normalizeTaskText(redactTaskSummarySecrets(task));
  if (!original) {
    return "agent";
  }

  let summary = stripLeadingScaffolding(original);
  summary = normalizeTaskText(summary)
    .replace(/\blsited\b/gi, "listed")
    .replace(/\brihgt\b/gi, "right")
    .replace(/\bthe task that (?:the )?user (?:puts|types|enters|entered)\b/gi, "the user task")
    .replace(/\btask that (?:the )?user (?:puts|types|enters|entered)\b/gi, "user task")
    .replace(/\band then (?:that's|that is) what gets (?:listed|shown|displayed) on\b/gi, "for")
    .replace(/\s+(?:right now|currently|at the moment|for now)\b.*$/i, "")
    .replace(/\s+/g, " ")
    .trim();

  summary = firstSentence(summary) || summary || original;
  summary = stripTerminalPunctuation(summary);

  if (summary.length <= maxChars) {
    return summary;
  }

  const compact = compactTitle(summary, maxChars);
  return compact || truncate(summary, maxChars);
}

export function taskDisplayLabel(run: { task: string; taskSummary?: string }, maxChars = DEFAULT_MAX_CHARS): string {
  const stored = normalizeTaskText(run.taskSummary ?? "");
  return truncate(stored || summarizeTask(run.task, maxChars), maxChars);
}

function stripLeadingScaffolding(value: string): string {
  let current = value;
  let changed = true;
  while (changed) {
    changed = false;
    for (const pattern of LEADING_PATTERNS) {
      const next = current.replace(pattern, "").trim();
      if (next !== current) {
        current = next;
        changed = true;
      }
    }
  }
  return current;
}

function firstSentence(value: string): string {
  const match = value.match(/^(.{12,}?[.!?])(?:\s|$)/);
  return match ? match[1] : value;
}

function compactTitle(value: string, maxChars: number): string {
  const words = value.match(/[A-Za-z0-9_./-]+/g) ?? [];
  const selected: string[] = [];
  for (const word of words) {
    const normalized = word.toLowerCase();
    if (STOP_WORDS.has(normalized)) {
      continue;
    }
    selected.push(word);
    const joined = selected.join(" ");
    if (joined.length >= maxChars - 1 || selected.length >= 8) {
      break;
    }
  }

  const compact = stripTerminalPunctuation(selected.join(" "));
  return compact && compact.length < value.length ? truncate(compact, maxChars) : "";
}

function normalizeTaskText(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function stripTerminalPunctuation(value: string): string {
  return value.replace(/[.!?]+$/g, "").trim();
}

function truncate(value: string, maxChars: number): string {
  if (value.length <= maxChars) {
    return value;
  }
  return `${value.slice(0, Math.max(0, maxChars - 1)).trimEnd()}…`;
}

const LLM_TITLE_MAX = 80;

function cleanLlmTitle(raw: string): string | null {
  const text = raw.trim();
  if (!text) return null;
  const jsonTitle = titleFromJsonOutput(text);
  if (jsonTitle) return jsonTitle;
  return cleanLlmTitleText(text);
}

function cleanLlmTitleText(raw: string): string | null {
  let text = raw;
  // Strip enclosing quotes (straight + smart) and trailing punctuation.
  text = text.replace(/^["'`“”‘’]+/, "");
  text = text.replace(/["'`“”‘’]+$/, "");
  text = text.replace(/[.!?]+$/g, "").trim();
  if (!text) return null;
  // Some models prefix with "Title:". Drop common label prefixes.
  text = text.replace(/^(?:title|summary)\s*:\s*/i, "").trim();
  if (!text) return null;
  // Take only the first line to avoid multi-paragraph responses.
  text = text.split(/\r?\n/)[0]?.trim() ?? "";
  if (!text) return null;
  if (text.length > LLM_TITLE_MAX) {
    text = text.slice(0, LLM_TITLE_MAX).trimEnd();
  }
  return text;
}

function titleFromJsonOutput(raw: string): string | null {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start < 0 || end < start) {
    return null;
  }
  try {
    const value = JSON.parse(raw.slice(start, end + 1)) as { title?: unknown };
    return typeof value.title === "string" ? cleanLlmTitleText(value.title.trim()) : null;
  } catch {
    return null;
  }
}

const SUMMARY_PROMPT_PREFIX =
  "Summarize this coding agent task for a compact sidebar label. Return exactly one JSON object and no markdown: {\"title\":\"5-8 word imperative title\"}. No quotes inside the title, no trailing punctuation.\n\nTask: ";

async function summarizeViaApiKey(task: string, apiKey: string): Promise<string | null> {
  try {
    const response = await fetch("https://api.anthropic.com/v1/messages", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": apiKey,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: "claude-haiku-4-5-20251001",
        max_tokens: 50,
        system:
          "Summarize coding tasks for compact sidebar labels. Return exactly one JSON object with a title string and no markdown.",
        messages: [{ role: "user", content: task }],
      }),
    });
    if (!response.ok) {
      return null;
    }
    const data = (await response.json()) as {
      content?: Array<{ type?: string; text?: string }>;
    };
    const first = data.content?.find((block) => typeof block?.text === "string");
    return cleanLlmTitle(first?.text ?? "");
  } catch {
    return null;
  }
}

async function summarizeViaClaudeCli(task: string): Promise<string | null> {
  const { spawn } = await import("node:child_process");
  return await new Promise<string | null>((resolve) => {
    let settled = false;
    const finish = (val: string | null) => {
      if (!settled) {
        settled = true;
        resolve(val);
      }
    };
    let child;
    try {
      child = spawn(
        "claude",
        ["-p", "--model", "claude-haiku-4-5-20251001"],
        { stdio: ["pipe", "pipe", "ignore"] },
      );
    } catch {
      finish(null);
      return;
    }
    let out = "";
    const timer = setTimeout(() => {
      try {
        child.kill("SIGTERM");
      } catch {
        // ignore
      }
      finish(null);
    }, 20000);
    child.stdout?.on("data", (d: Buffer) => {
      out += d.toString("utf8");
    });
    child.on("exit", () => {
      clearTimeout(timer);
      finish(cleanLlmTitle(out));
    });
    child.on("error", () => {
      clearTimeout(timer);
      finish(null);
    });
    try {
      child.stdin?.write(`${SUMMARY_PROMPT_PREFIX}${task}`);
      child.stdin?.end();
    } catch {
      // ignore — child error handler will fire
    }
  });
}

/**
 * Resolve an Anthropic API key from the environment or the saved anthropic auth
 * profile. Returns "" when none is available.
 */
export async function resolveAnthropicApiKey(): Promise<string> {
  let apiKey = process.env.ANTHROPIC_API_KEY ?? "";
  if (!apiKey) {
    try {
      const { backendEnv } = await import("./backends.js");
      const env = await backendEnv("anthropic");
      apiKey = env.ANTHROPIC_API_KEY ?? "";
    } catch {
      // ignore — caller falls through to the claude CLI path
    }
  }
  return apiKey;
}

async function callModelViaApiKey(params: {
  apiKey: string;
  model: string;
  system: string;
  user: string;
  maxTokens: number;
}): Promise<string | null> {
  try {
    const response = await fetch("https://api.anthropic.com/v1/messages", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": params.apiKey,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: params.model,
        max_tokens: params.maxTokens,
        system: params.system,
        messages: [{ role: "user", content: params.user }],
      }),
    });
    if (!response.ok) {
      return null;
    }
    const data = (await response.json()) as {
      content?: Array<{ type?: string; text?: string }>;
    };
    return (data.content ?? [])
      .filter((block) => typeof block?.text === "string")
      .map((block) => block.text)
      .join("")
      .trim();
  } catch {
    return null;
  }
}

async function callModelViaClaudeCli(params: {
  model: string;
  system: string;
  user: string;
  timeoutMs: number;
}): Promise<string | null> {
  const { spawn } = await import("node:child_process");
  return await new Promise<string | null>((resolve) => {
    let settled = false;
    const finish = (val: string | null) => {
      if (!settled) {
        settled = true;
        resolve(val);
      }
    };
    let child;
    try {
      child = spawn(
        "claude",
        ["-p", "--model", params.model, "--append-system-prompt", params.system],
        { stdio: ["pipe", "pipe", "ignore"] },
      );
    } catch {
      finish(null);
      return;
    }
    let out = "";
    const timer = setTimeout(() => {
      try {
        child.kill("SIGTERM");
      } catch {
        // ignore
      }
      finish(null);
    }, params.timeoutMs);
    child.stdout?.on("data", (d: Buffer) => {
      out += d.toString("utf8");
    });
    child.on("exit", () => {
      clearTimeout(timer);
      finish(out.trim() || null);
    });
    child.on("error", () => {
      clearTimeout(timer);
      finish(null);
    });
    try {
      child.stdin?.write(params.user);
      child.stdin?.end();
    } catch {
      // ignore — child error handler will fire
    }
  });
}

/**
 * Generic single-shot LLM text call reused by the planner. Tries the Anthropic
 * API (when a key is available) then falls back to the `claude` CLI, mirroring
 * llmSummarizeTask's degradation path. Throws only when neither path is usable
 * AND no output was produced, so callers can surface a clear error.
 */
export async function callTextModel(params: {
  model: string;
  system: string;
  user: string;
  maxTokens?: number;
  timeoutMs?: number;
}): Promise<string> {
  // TEST-ONLY deterministic hook: when RUDDER_FAKE_MODEL_OUTPUT is set, return it
  // verbatim (or, if it begins with "@", the contents of that file) instead of
  // calling any real model. This lets the end-to-end orchestrator test drive the
  // real planner code path (planTask -> parsePlanBlock -> scaffoldPlan) with a
  // canned RUDDER_PLAN_TASKS block, so a run is repeatable and needs no auth.
  const fake = process.env.RUDDER_FAKE_MODEL_OUTPUT;
  if (fake !== undefined && fake !== "") {
    if (fake.startsWith("@")) {
      const { readFile } = await import("node:fs/promises");
      return await readFile(fake.slice(1), "utf8");
    }
    return fake;
  }
  const maxTokens = params.maxTokens ?? 2048;
  const timeoutMs = params.timeoutMs ?? 60000;
  const apiKey = await resolveAnthropicApiKey();
  if (apiKey) {
    const text = await callModelViaApiKey({
      apiKey,
      model: params.model,
      system: params.system,
      user: params.user,
      maxTokens,
    });
    if (text) {
      return text;
    }
  }
  const { commandExists } = await import("./util.js");
  if (commandExists("claude")) {
    const text = await callModelViaClaudeCli({
      model: params.model,
      system: params.system,
      user: params.user,
      timeoutMs,
    });
    if (text) {
      return text;
    }
  }
  throw new Error(
    "No planning model available: set ANTHROPIC_API_KEY (or sign in so a key is in your auth profile), or install the `claude` CLI.",
  );
}

export async function llmSummarizeTask(task: string): Promise<string | null> {
  const trimmed = normalizeTaskText(redactTaskSummarySecrets(task));
  if (!trimmed) {
    return null;
  }

  // Fast path: direct Anthropic API if an API key is sitting in env or in the
  // user's saved anthropic auth profile. ~1s per call.
  let apiKey = process.env.ANTHROPIC_API_KEY ?? "";
  if (!apiKey) {
    try {
      const { backendEnv } = await import("./backends.js");
      const env = await backendEnv("anthropic");
      apiKey = env.ANTHROPIC_API_KEY ?? "";
    } catch {
      // ignore — fall through to CLI fallback
    }
  }
  if (apiKey) {
    const title = await summarizeViaApiKey(trimmed, apiKey);
    if (title) return title;
  }

  // Fallback: shell out to the `claude` CLI itself. Slower (~3-5s) but works
  // for users who authenticate via the macOS Keychain / claude login flow
  // instead of a raw API key. Runs in the background relative to the dashboard
  // so the user does not feel the latency.
  return await summarizeViaClaudeCli(trimmed);
}

/**
 * Redact token/key/secret-shaped values from free text. Exported for the
 * improvement loop, which must strip secrets from session telemetry before
 * any model call (docs/continual-improvement.md §3).
 */
export function redactTaskSummarySecrets(value: string): string {
  return value
    .split(/\s+/)
    .filter(Boolean)
    .map(redactSecretToken)
    .join(" ");
}

function redactSecretToken(raw: string): string {
  const trimmed = raw.replace(/^[`"'([{]+|[`"',;)\]}]+$/g, "");
  const eq = trimmed.indexOf("=");
  if (eq > 0) {
    const key = trimmed.slice(0, eq);
    const secret = trimmed.slice(eq + 1);
    if (
      looksLikeSecretKey(key) &&
      (looksLikeSecretValue(secret) || looksLikePlausibleSecretValue(secret))
    ) {
      return raw.replace(secret, "[redacted]");
    }
  }
  if (looksLikeSecretValue(trimmed)) {
    return raw.replace(trimmed, "[redacted]");
  }
  return raw;
}

function looksLikeSecretKey(key: string): boolean {
  return /token|api_key|apikey|secret|password/i.test(key);
}

function looksLikeSecretValue(value: string): boolean {
  const lower = value.toLowerCase();
  const hasSecretPrefix =
    lower.includes("api_") ||
    lower.startsWith("sk-") ||
    lower.startsWith("xox") ||
    lower.startsWith("ghp_") ||
    lower.startsWith("gho_") ||
    lower.startsWith("github_pat_");
  const longMixed =
    value.length >= 24 &&
    /[A-Za-z]/.test(value) &&
    /[0-9_.-]/.test(value);
  return hasSecretPrefix || longMixed;
}

function looksLikePlausibleSecretValue(value: string): boolean {
  return (
    value.length >= 10 &&
    /[A-Za-z0-9]/.test(value) &&
    /[0-9_.\-/]/.test(value)
  );
}
