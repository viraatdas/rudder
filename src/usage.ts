/**
 * Usage accounting for the web dashboard (`U` in the TUI, `/usage` on the board).
 *
 * Three independent sources, all local to this machine:
 *
 * - **Quota**: the providers' own usage endpoints, called with the sign-ins the
 *   CLIs already hold (Claude Code's OAuth token from the keychain or
 *   `.credentials.json`; Codex's `auth.json`). Rudder never sees a password and
 *   never stores a token; it reads what the CLI stored and forwards it once.
 * - **Token costs**: the CLIs' own session logs (`~/.claude/projects`,
 *   `~/.codex/sessions`), priced at API list rates. Subscription usage is not
 *   billed per token, so this is "what the same work would cost at API rates",
 *   plus how much prompt caching saved.
 * - **Machine**: `ps` for the agent processes and rudder itself, `os` for the
 *   system RAM split.
 */
import { execFile } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export type Provider = "claude" | "codex";
export type UsageRange = "7d" | "30d" | "90d";

/** USD per million tokens. */
export type ModelPrice = { input: number; output: number; cacheWrite: number; cacheRead: number };

export type Totals = {
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
  /** Estimated USD at API list rates. */
  cost: number;
  /** USD that cache reads saved versus paying full input price. */
  cacheSavings: number;
  /** API calls (Claude assistant messages / Codex responses). */
  calls: number;
};

export type UsageEvent = {
  ts: string;
  provider: Provider;
  model: string;
  cwd: string;
  sessionId: string;
  input: number;
  output: number;
  cacheWrite: number;
  cacheRead: number;
};

export type DayBucket = { day: string; claude: Totals; codex: Totals };
export type ModelRow = { provider: Provider; model: string; totals: Totals; sessions: number };
export type SessionRow = {
  id: string;
  provider: Provider;
  model: string;
  startedAt: string;
  lastAt: string;
  cwd: string;
  totals: Totals;
};
export type ProjectRow = {
  key: string;
  name: string;
  slug: string | null;
  totals: Totals;
  sessions: SessionRow[];
};

export type TokenUsageReport = {
  range: UsageRange;
  since: string;
  generatedAt: string;
  days: DayBucket[];
  providers: Record<Provider, Totals>;
  total: Totals;
  models: ModelRow[];
  projects: ProjectRow[];
  scannedFiles: number;
  pricingNote: string;
};

// ---------------------------------------------------------------------------
// Pricing. Mirrors native/src/usage.rs so the TUI's /usage and the dashboard
// agree; approximate list rates, not billing-grade.
// ---------------------------------------------------------------------------

export function priceFor(model: string): ModelPrice | null {
  const m = model.toLowerCase();
  if (m.includes("fable") || m.includes("opus")) {
    return { input: 15, output: 75, cacheWrite: 18.75, cacheRead: 1.5 };
  }
  if (m.includes("sonnet")) {
    return { input: 3, output: 15, cacheWrite: 3.75, cacheRead: 0.3 };
  }
  if (m.includes("haiku")) {
    return { input: 0.8, output: 4, cacheWrite: 1, cacheRead: 0.08 };
  }
  if (m.includes("claude")) {
    return { input: 3, output: 15, cacheWrite: 3.75, cacheRead: 0.3 };
  }
  if (m.startsWith("gpt-5") || m.startsWith("gpt-6") || m.startsWith("o3")) {
    return { input: 10, output: 30, cacheWrite: 0, cacheRead: 1 };
  }
  if (m.startsWith("gpt-4o-mini")) {
    return { input: 0.15, output: 0.6, cacheWrite: 0, cacheRead: 0.075 };
  }
  if (m.startsWith("gpt-4o")) {
    return { input: 2.5, output: 10, cacheWrite: 0, cacheRead: 1.25 };
  }
  if (m.startsWith("o1")) {
    return { input: 15, output: 60, cacheWrite: 0, cacheRead: 7.5 };
  }
  return null;
}

export function emptyTotals(): Totals {
  return { input: 0, output: 0, cacheWrite: 0, cacheRead: 0, cost: 0, cacheSavings: 0, calls: 0 };
}

export function addEvent(totals: Totals, event: UsageEvent): void {
  const price = priceFor(event.model) ?? { input: 0, output: 0, cacheWrite: 0, cacheRead: 0 };
  totals.input += event.input;
  totals.output += event.output;
  totals.cacheWrite += event.cacheWrite;
  totals.cacheRead += event.cacheRead;
  totals.calls += 1;
  totals.cost +=
    (event.input * price.input +
      event.output * price.output +
      event.cacheWrite * price.cacheWrite +
      event.cacheRead * price.cacheRead) /
    1_000_000;
  totals.cacheSavings += (event.cacheRead * Math.max(0, price.input - price.cacheRead)) / 1_000_000;
}

function mergeTotals(into: Totals, from: Totals): void {
  into.input += from.input;
  into.output += from.output;
  into.cacheWrite += from.cacheWrite;
  into.cacheRead += from.cacheRead;
  into.cost += from.cost;
  into.cacheSavings += from.cacheSavings;
  into.calls += from.calls;
}

// ---------------------------------------------------------------------------
// Session log locations.
// ---------------------------------------------------------------------------

export function claudeConfigDir(): string {
  return path.resolve(process.env.CLAUDE_CONFIG_DIR?.trim() || path.join(os.homedir(), ".claude"));
}

export function codexHomeDir(): string {
  return path.resolve(process.env.CODEX_HOME?.trim() || path.join(os.homedir(), ".codex"));
}

export function rangeSince(range: UsageRange, now = new Date()): Date {
  const days = range === "7d" ? 7 : range === "30d" ? 30 : 90;
  const since = new Date(now);
  since.setHours(0, 0, 0, 0);
  since.setDate(since.getDate() - (days - 1));
  return since;
}

/** Local calendar day for a timestamp, as YYYY-MM-DD. */
export function localDay(ts: string | Date): string {
  const d = typeof ts === "string" ? new Date(ts) : ts;
  if (Number.isNaN(d.getTime())) return "";
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

async function listJsonl(root: string, depth: number): Promise<string[]> {
  const out: string[] = [];
  const walk = async (dir: string, left: number): Promise<void> => {
    let entries: fs.Dirent[];
    try {
      entries = await fsp.readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (left > 0) await walk(full, left - 1);
      } else if (entry.isFile() && entry.name.endsWith(".jsonl")) {
        out.push(full);
      }
    }
  };
  await walk(root, depth);
  return out;
}

// ---------------------------------------------------------------------------
// Per-file parsing, cached by (mtime, size): the daemon is long-lived and the
// logs are append-only, so a file is re-read only when it grows.
// ---------------------------------------------------------------------------

type FileCacheEntry = { mtimeMs: number; size: number; events: UsageEvent[] };
const fileCache = new Map<string, FileCacheEntry>();

async function eachLine(file: string, onLine: (line: string) => void): Promise<void> {
  const stream = fs.createReadStream(file, { encoding: "utf8" });
  const rl = readline.createInterface({ input: stream, crlfDelay: Number.POSITIVE_INFINITY });
  try {
    for await (const line of rl) {
      onLine(line);
    }
  } finally {
    rl.close();
    stream.destroy();
  }
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

/**
 * Claude Code writes one line per assistant message, and a streamed message can
 * appear several times (same `message.id` + `requestId`) with growing usage; the
 * last line is the complete one, so later occurrences replace earlier ones.
 */
export async function parseClaudeLog(file: string): Promise<UsageEvent[]> {
  const byKey = new Map<string, UsageEvent>();
  let order = 0;
  const keyed: { key: string; order: number }[] = [];
  await eachLine(file, (line) => {
    if (!line.includes('"assistant"')) return;
    let value: any;
    try {
      value = JSON.parse(line);
    } catch {
      return;
    }
    if (value?.type !== "assistant") return;
    const message = value.message;
    const usage = message?.usage;
    if (!usage || typeof usage !== "object") return;
    const model = String(message.model ?? "unknown");
    if (model === "<synthetic>") return;
    const ts = String(value.timestamp ?? "");
    if (!ts) return;
    const id = String(message.id ?? "");
    const requestId = String(value.requestId ?? "");
    const key = id && requestId ? `${id}:${requestId}` : `line:${order}`;
    const event: UsageEvent = {
      ts,
      provider: "claude",
      model,
      cwd: String(value.cwd ?? ""),
      sessionId: String(value.sessionId ?? path.basename(file, ".jsonl")),
      input: num(usage.input_tokens),
      output: num(usage.output_tokens),
      cacheWrite: num(usage.cache_creation_input_tokens),
      cacheRead: num(usage.cache_read_input_tokens),
    };
    if (!byKey.has(key)) keyed.push({ key, order });
    byKey.set(key, event);
    order += 1;
  });
  return keyed.map(({ key }) => byKey.get(key)!).filter(Boolean);
}

/**
 * Codex writes `token_count` events carrying a cumulative `total_token_usage`;
 * each event's delta from the previous one is one response's usage, attributed
 * to that event's timestamp and the model of the latest `turn_context`.
 * Cached input is a subset of input, so billable input = input − cached.
 */
export async function parseCodexLog(file: string): Promise<UsageEvent[]> {
  const events: UsageEvent[] = [];
  let cwd = "";
  let sessionId = path.basename(file, ".jsonl");
  let model = "unknown-codex";
  let prev = { input: 0, cached: 0, output: 0 };
  await eachLine(file, (line) => {
    let value: any;
    try {
      value = JSON.parse(line);
    } catch {
      return;
    }
    const payload = value?.payload;
    switch (value?.type) {
      case "session_meta":
        if (typeof payload?.cwd === "string") cwd = payload.cwd;
        if (typeof payload?.id === "string") sessionId = payload.id;
        break;
      case "turn_context":
        if (typeof payload?.model === "string") model = payload.model;
        if (typeof payload?.cwd === "string" && !cwd) cwd = payload.cwd;
        break;
      case "event_msg": {
        if (payload?.type !== "token_count") return;
        const total = payload?.info?.total_token_usage;
        if (!total || typeof total !== "object") return;
        const cur = {
          input: num(total.input_tokens),
          cached: num(total.cached_input_tokens),
          output: num(total.output_tokens),
        };
        // A cumulative counter that went backwards is a fresh counter
        // (compaction / resumed thread): count the new total from zero.
        const base = cur.input < prev.input || cur.output < prev.output ? { input: 0, cached: 0, output: 0 } : prev;
        const dInput = cur.input - base.input;
        const dCached = cur.cached - base.cached;
        const dOutput = cur.output - base.output;
        prev = cur;
        if (dInput <= 0 && dOutput <= 0) return;
        events.push({
          ts: String(value.timestamp ?? ""),
          provider: "codex",
          model,
          cwd,
          sessionId,
          input: Math.max(0, dInput - Math.max(0, dCached)),
          output: Math.max(0, dOutput),
          cacheWrite: 0,
          cacheRead: Math.max(0, dCached),
        });
        break;
      }
      default:
        break;
    }
  });
  return events;
}

async function cachedEvents(file: string, parse: (f: string) => Promise<UsageEvent[]>): Promise<UsageEvent[]> {
  let stat: fs.Stats;
  try {
    stat = await fsp.stat(file);
  } catch {
    return [];
  }
  const hit = fileCache.get(file);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) return hit.events;
  const events = await parse(file);
  fileCache.set(file, { mtimeMs: stat.mtimeMs, size: stat.size, events });
  return events;
}

export type ProjectLookup = { slug: string; name: string; repoRoot: string }[];

const gitRootCache = new Map<string, string | null>();

/** Nearest ancestor (inclusive) that is a git/jj checkout, or null. Cached per dir. */
export function nearestRepoRoot(cwd: string, exists: (p: string) => boolean = fs.existsSync): string | null {
  if (!cwd || !path.isAbsolute(cwd)) return null;
  const chain: string[] = [];
  let dir = cwd;
  let found: string | null = null;
  for (;;) {
    const hit = gitRootCache.get(dir);
    if (hit !== undefined) {
      found = hit;
      break;
    }
    chain.push(dir);
    if (exists(path.join(dir, ".git")) || exists(path.join(dir, ".jj"))) {
      found = dir;
      break;
    }
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  for (const d of chain) gitRootCache.set(d, found);
  return found;
}

/** Rudder worker workspaces are jj workspaces INSIDE the repo, under this dir. */
const AGENT_WORKSPACES_SEGMENT = "/.rudder-workspaces/";

/**
 * Which workspace a session belongs to. The session's own checkout names it:
 * a Rudder worker workspace (`<repo>/.rudder-workspaces/<name>`) counts for
 * `<repo>`, any other cwd for its nearest git/jj root, a bare directory for
 * itself. A registered project wins only when its root IS that checkout —
 * a project registered at `~/code` must not swallow every repo beneath it —
 * or when the cwd is not inside any checkout at all.
 */
export function projectFor(
  cwd: string,
  projects: ProjectLookup,
  repoRootOf: (cwd: string) => string | null = nearestRepoRoot,
): { key: string; name: string; slug: string | null } {
  const resolved = (cwd || "").replace(/\/+$/, "") || "(unknown)";
  const wsAt = resolved.indexOf(AGENT_WORKSPACES_SEGMENT);
  const root = wsAt > 0 ? resolved.slice(0, wsAt) : repoRootOf(resolved);
  const within = (dir: string, parent: string) => dir === parent || dir.startsWith(`${parent}/`);
  let best: { key: string; name: string; slug: string | null } | null = null;
  for (const project of projects) {
    const projectRoot = project.repoRoot.replace(/\/+$/, "");
    const name = project.name || path.basename(projectRoot);
    if (root ? root === projectRoot : within(resolved, projectRoot)) {
      if (!best || projectRoot.length > best.key.length) best = { key: projectRoot, name, slug: project.slug };
    }
  }
  if (best) return best;
  if (root) return { key: root, name: path.basename(root) || root, slug: null };
  return { key: resolved, name: path.basename(resolved) || resolved, slug: null };
}

export async function scanTokenUsage(opts: {
  range: UsageRange;
  projects?: ProjectLookup;
  claudeProjectsRoot?: string;
  codexSessionsRoot?: string;
  now?: Date;
}): Promise<TokenUsageReport> {
  const now = opts.now ?? new Date();
  const since = rangeSince(opts.range, now);
  const sinceMs = since.getTime();
  const claudeRoot = opts.claudeProjectsRoot ?? path.join(claudeConfigDir(), "projects");
  const codexRoot = opts.codexSessionsRoot ?? path.join(codexHomeDir(), "sessions");
  const projects = opts.projects ?? [];

  const files: { file: string; provider: Provider }[] = [];
  for (const file of await listJsonl(claudeRoot, 3)) files.push({ file, provider: "claude" });
  for (const file of await listJsonl(codexRoot, 4)) files.push({ file, provider: "codex" });

  const events: UsageEvent[] = [];
  let scanned = 0;
  for (const { file, provider } of files) {
    let stat: fs.Stats;
    try {
      stat = await fsp.stat(file);
    } catch {
      continue;
    }
    // A log's mtime is at or after its last event: older files cannot hold
    // events in range and are skipped without being read.
    if (stat.mtimeMs < sinceMs) continue;
    scanned += 1;
    const parsed = await cachedEvents(file, provider === "claude" ? parseClaudeLog : parseCodexLog);
    for (const event of parsed) {
      const t = new Date(event.ts).getTime();
      if (Number.isNaN(t) || t < sinceMs) continue;
      events.push(event);
    }
  }

  const days = new Map<string, DayBucket>();
  for (let d = new Date(since); d.getTime() <= now.getTime(); d.setDate(d.getDate() + 1)) {
    const day = localDay(d);
    days.set(day, { day, claude: emptyTotals(), codex: emptyTotals() });
  }
  const providers: Record<Provider, Totals> = { claude: emptyTotals(), codex: emptyTotals() };
  const models = new Map<string, { row: ModelRow; sessions: Set<string> }>();
  const projectRows = new Map<string, { row: ProjectRow; sessions: Map<string, SessionRow> }>();
  const total = emptyTotals();

  for (const event of events) {
    const day = localDay(event.ts);
    const bucket = days.get(day);
    if (bucket) addEvent(bucket[event.provider], event);
    addEvent(providers[event.provider], event);
    addEvent(total, event);

    const modelKey = `${event.provider}:${event.model}`;
    let model = models.get(modelKey);
    if (!model) {
      model = { row: { provider: event.provider, model: event.model, totals: emptyTotals(), sessions: 0 }, sessions: new Set() };
      models.set(modelKey, model);
    }
    addEvent(model.row.totals, event);
    model.sessions.add(event.sessionId);

    const project = projectFor(event.cwd, projects);
    let entry = projectRows.get(project.key);
    if (!entry) {
      entry = { row: { ...project, totals: emptyTotals(), sessions: [] }, sessions: new Map() };
      projectRows.set(project.key, entry);
    }
    addEvent(entry.row.totals, event);
    let session = entry.sessions.get(event.sessionId);
    if (!session) {
      session = {
        id: event.sessionId,
        provider: event.provider,
        model: event.model,
        startedAt: event.ts,
        lastAt: event.ts,
        cwd: event.cwd,
        totals: emptyTotals(),
      };
      entry.sessions.set(event.sessionId, session);
    }
    if (event.ts < session.startedAt) session.startedAt = event.ts;
    if (event.ts > session.lastAt) {
      session.lastAt = event.ts;
      session.model = event.model;
    }
    addEvent(session.totals, event);
  }

  const byCost = (a: { totals: Totals }, b: { totals: Totals }) => b.totals.cost - a.totals.cost;
  const modelRows = [...models.values()]
    .map(({ row, sessions }) => ({ ...row, sessions: sessions.size }))
    .sort(byCost);
  const projectList = [...projectRows.values()]
    .map(({ row, sessions }) => ({ ...row, sessions: [...sessions.values()].sort(byCost).slice(0, 25) }))
    .sort(byCost);

  return {
    range: opts.range,
    since: since.toISOString(),
    generatedAt: now.toISOString(),
    days: [...days.values()],
    providers,
    total,
    models: modelRows,
    projects: projectList,
    scannedFiles: scanned,
    pricingNote:
      "Estimated at API list rates from the CLIs' local session logs. Subscription usage is not billed per token; this is what the same work would cost on the API.",
  };
}

// ---------------------------------------------------------------------------
// Provider quota, from the sign-ins the CLIs already hold.
// ---------------------------------------------------------------------------

export type QuotaWindow = {
  label: string;
  percent: number;
  resetsAt: string | null;
};

export type AccountQuota = {
  provider: Provider;
  email: string | null;
  plan: string | null;
  windows: QuotaWindow[];
  /** Human-readable reason the meters are missing, with the fix. */
  error: string | null;
  fetchedAt: string;
};

export type QuotaReport = { accounts: AccountQuota[]; fetchedAt: string; ttlSeconds: number };

const QUOTA_TTL_MS = 5 * 60 * 1000;
let quotaCache: { at: number; report: QuotaReport } | null = null;

type ClaudeCredentials = { accessToken: string; expiresAt: number | null; subscriptionType: string | null };

async function readClaudeCredentials(): Promise<ClaudeCredentials | null> {
  let raw: string | null = null;
  const file = path.join(claudeConfigDir(), ".credentials.json");
  try {
    raw = await fsp.readFile(file, "utf8");
  } catch {
    raw = null;
  }
  if (raw === null && process.platform === "darwin") {
    try {
      const { stdout } = await execFileAsync("security", ["find-generic-password", "-s", "Claude Code-credentials", "-w"], {
        timeout: 5000,
      });
      raw = stdout;
    } catch {
      raw = null;
    }
  }
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    const oauth = parsed?.claudeAiOauth;
    if (!oauth?.accessToken) return null;
    return {
      accessToken: String(oauth.accessToken),
      expiresAt: typeof oauth.expiresAt === "number" ? oauth.expiresAt : null,
      subscriptionType: typeof oauth.subscriptionType === "string" ? oauth.subscriptionType : null,
    };
  } catch {
    return null;
  }
}

function windowLabel(seconds: number): string {
  if (seconds >= 6 * 24 * 3600 && seconds <= 8 * 24 * 3600) return "Weekly";
  if (seconds === 5 * 3600) return "Session (5h)";
  if (seconds % 3600 === 0) return `${seconds / 3600}h`;
  return `${Math.round(seconds / 60)}m`;
}

function planLabel(tier: string | null | undefined, fallback: string | null): string | null {
  const t = (tier ?? "").toLowerCase();
  const m = t.match(/max_(\d+)x/);
  if (m) return `Max ${m[1]}x`;
  if (t.includes("max")) return "Max";
  if (t.includes("pro")) return "Pro";
  if (t.includes("team")) return "Team";
  if (t.includes("enterprise")) return "Enterprise";
  if (fallback) return fallback.charAt(0).toUpperCase() + fallback.slice(1);
  return null;
}

/** Pure: Claude's `/api/oauth/usage` body → meters. */
export function parseClaudeQuota(usage: any, profile: any, fallbackPlan: string | null): Omit<AccountQuota, "fetchedAt" | "provider"> {
  const windows: QuotaWindow[] = [];
  const push = (label: string, w: any) => {
    if (!w || typeof w !== "object" || typeof w.utilization !== "number") return;
    windows.push({ label, percent: Math.max(0, Math.min(100, Math.round(w.utilization))), resetsAt: w.resets_at ?? null });
  };
  push("Session (5h)", usage?.five_hour);
  push("Weekly", usage?.seven_day);
  push("Weekly · Opus", usage?.seven_day_opus);
  push("Weekly · Sonnet", usage?.seven_day_sonnet);
  const extra = usage?.extra_usage;
  if (extra?.is_enabled && typeof extra.utilization === "number") {
    windows.push({ label: "Extra usage", percent: Math.round(extra.utilization), resetsAt: null });
  }
  const email = typeof profile?.account?.email === "string" ? profile.account.email : null;
  const plan = planLabel(profile?.organization?.rate_limit_tier, fallbackPlan);
  return { email, plan, windows, error: windows.length ? null : "this plan does not expose limits" };
}

async function fetchClaudeQuota(): Promise<AccountQuota> {
  const fetchedAt = new Date().toISOString();
  const base = { provider: "claude" as const, email: null, plan: null, windows: [], fetchedAt };
  const creds = await readClaudeCredentials();
  if (!creds) return { ...base, error: "not signed in — run `claude` and log in" };
  if (creds.expiresAt && creds.expiresAt < Date.now()) {
    return { ...base, plan: planLabel(null, creds.subscriptionType), error: "sign-in expired — run `claude` once to refresh it" };
  }
  const headers = {
    Authorization: `Bearer ${creds.accessToken}`,
    "anthropic-beta": "oauth-2025-04-20",
    "User-Agent": "rudder-usage",
  };
  try {
    const [usageRes, profileRes] = await Promise.all([
      fetch("https://api.anthropic.com/api/oauth/usage", { headers, signal: AbortSignal.timeout(15_000) }),
      fetch("https://api.anthropic.com/api/oauth/profile", { headers, signal: AbortSignal.timeout(15_000) }),
    ]);
    if (usageRes.status === 401 || usageRes.status === 403) {
      return { ...base, error: "sign-in rejected — run `claude` once to refresh it" };
    }
    if (!usageRes.ok) return { ...base, error: `usage endpoint returned ${usageRes.status}` };
    const usage = await usageRes.json();
    const profile = profileRes.ok ? await profileRes.json() : null;
    return { ...base, ...parseClaudeQuota(usage, profile, creds.subscriptionType) };
  } catch (error) {
    return { ...base, error: `could not reach api.anthropic.com (${String((error as Error)?.message ?? error)})` };
  }
}

type CodexCredentials = { accessToken: string; accountId: string | null };

async function readCodexCredentials(): Promise<CodexCredentials | null> {
  try {
    const raw = await fsp.readFile(path.join(codexHomeDir(), "auth.json"), "utf8");
    const parsed = JSON.parse(raw);
    const token = parsed?.tokens?.access_token;
    if (typeof token !== "string" || !token) return null;
    return { accessToken: token, accountId: typeof parsed.tokens.account_id === "string" ? parsed.tokens.account_id : null };
  } catch {
    return null;
  }
}

/** Pure: Codex's `/backend-api/wham/usage` body → meters. */
export function parseCodexQuota(body: any): Omit<AccountQuota, "fetchedAt" | "provider"> {
  const windows: QuotaWindow[] = [];
  const pushWindow = (prefix: string, w: any) => {
    if (!w || typeof w !== "object" || typeof w.used_percent !== "number") return;
    const label = windowLabel(num(w.limit_window_seconds));
    windows.push({
      label: prefix ? `${prefix} · ${label}` : label,
      percent: Math.max(0, Math.min(100, Math.round(w.used_percent))),
      resetsAt: typeof w.reset_at === "number" ? new Date(w.reset_at * 1000).toISOString() : null,
    });
  };
  pushWindow("", body?.rate_limit?.primary_window);
  pushWindow("", body?.rate_limit?.secondary_window);
  for (const extra of Array.isArray(body?.additional_rate_limits) ? body.additional_rate_limits : []) {
    const name = typeof extra?.limit_name === "string" ? extra.limit_name : "extra";
    pushWindow(name, extra?.rate_limit?.primary_window);
    pushWindow(name, extra?.rate_limit?.secondary_window);
  }
  const email = typeof body?.email === "string" ? body.email : null;
  const plan = planLabel(null, typeof body?.plan_type === "string" ? body.plan_type : null);
  return { email, plan, windows, error: windows.length ? null : "this plan does not expose limits" };
}

async function fetchCodexQuota(): Promise<AccountQuota> {
  const fetchedAt = new Date().toISOString();
  const base = { provider: "codex" as const, email: null, plan: null, windows: [], fetchedAt };
  const creds = await readCodexCredentials();
  if (!creds) return { ...base, error: "not signed in — run `codex login`" };
  const headers: Record<string, string> = {
    Authorization: `Bearer ${creds.accessToken}`,
    "User-Agent": "rudder-usage",
  };
  if (creds.accountId) headers["chatgpt-account-id"] = creds.accountId;
  try {
    const res = await fetch("https://chatgpt.com/backend-api/wham/usage", { headers, signal: AbortSignal.timeout(15_000) });
    if (res.status === 401 || res.status === 403) return { ...base, error: "sign-in expired — run `codex login`" };
    if (!res.ok) return { ...base, error: `usage endpoint returned ${res.status}` };
    return { ...base, ...parseCodexQuota(await res.json()) };
  } catch (error) {
    return { ...base, error: `could not reach chatgpt.com (${String((error as Error)?.message ?? error)})` };
  }
}

/** A failed lookup is retried soon, not cached for the full five minutes. */
const QUOTA_ERROR_TTL_MS = 20 * 1000;

export async function fetchQuota(opts: { force?: boolean } = {}): Promise<QuotaReport> {
  if (!opts.force && quotaCache) {
    const ttl = quotaCache.report.accounts.some((a) => a.error) ? QUOTA_ERROR_TTL_MS : QUOTA_TTL_MS;
    if (Date.now() - quotaCache.at < ttl) return quotaCache.report;
  }
  const accounts = await Promise.all([fetchClaudeQuota(), fetchCodexQuota()]);
  const report: QuotaReport = { accounts, fetchedAt: new Date().toISOString(), ttlSeconds: QUOTA_TTL_MS / 1000 };
  quotaCache = { at: Date.now(), report };
  return report;
}

// ---------------------------------------------------------------------------
// Machine resources.
// ---------------------------------------------------------------------------

export type ProcessKind = "rudder" | "claude" | "codex" | "opencode" | "other";

export type ProcessRow = {
  pid: number;
  ppid: number;
  kind: ProcessKind;
  cpu: number;
  /** Resident set, bytes. */
  rss: number;
  elapsed: string;
  command: string;
};

export type MachineReport = {
  at: string;
  cpus: number;
  load: number[];
  totalMem: number;
  freeMem: number;
  rudder: { cpu: number; rss: number; pid: number };
  processes: ProcessRow[];
  byKind: Record<ProcessKind, { cpu: number; rss: number; count: number }>;
};

/**
 * Classify by the EXECUTABLE, not the whole command line: a Claude worker's
 * argv carries `--settings ~/.rudder/signals/...`, so matching "rudder"
 * anywhere in the line filed every agent under rudder. For interpreters
 * (`node`, `bun`) the script path is the executable.
 */
export function classifyCommand(command: string): ProcessKind {
  const argv = command.trim().split(/\s+/);
  let exe = (argv[0] ?? "").toLowerCase();
  if (/(^|\/)(node|bun|deno)(\d+)?$/.test(exe) && argv[1]) exe = argv[1].toLowerCase();
  const base = exe.split("/").pop() ?? "";
  if (base === "rudder-native" || base === "rudder" || exe.includes("/rudder/dist/") || exe.includes("@viraatdas/rudder/")) {
    return "rudder";
  }
  if (base === "claude" || exe.includes("/claude/versions/") || exe.includes("@anthropic-ai/claude-code")) return "claude";
  if (base === "codex" || base === "codex.js" || exe.includes("@openai/codex") || exe.includes("codex computer use.app") || exe.includes("codex-code-mode-host")) {
    return "codex";
  }
  if (base === "opencode" || exe.includes("/.opencode/")) return "opencode";
  return "other";
}

/** Pure: `ps -axo pid=,ppid=,pcpu=,rss=,etime=,args=` → agent-related rows. */
export function parsePsOutput(text: string): ProcessRow[] {
  const rows: ProcessRow[] = [];
  for (const line of text.split("\n")) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+([\d.]+)\s+(\d+)\s+(\S+)\s+(.*)$/);
    if (!m) continue;
    const command = m[6] ?? "";
    const kind = classifyCommand(command);
    if (kind === "other") continue;
    rows.push({
      pid: Number(m[1]),
      ppid: Number(m[2]),
      kind,
      cpu: Number(m[3]),
      rss: Number(m[4]) * 1024,
      elapsed: m[5] ?? "",
      command,
    });
  }
  return rows.sort((a, b) => b.cpu - a.cpu || b.rss - a.rss);
}

/**
 * `os.freemem()` on macOS counts only truly free pages, so a healthy Mac reads
 * as 99% used. vm_stat's inactive + speculative + purgeable pages are
 * reclaimable and count as available, which is what Activity Monitor shows.
 */
export function parseVmStat(text: string): number | null {
  const pageSize = Number(text.match(/page size of (\d+) bytes/)?.[1] ?? 0);
  if (!pageSize) return null;
  const page = (name: string) => Number(text.match(new RegExp(`${name}:\\s+(\\d+)`))?.[1] ?? 0);
  const available = page("Pages free") + page("Pages inactive") + page("Pages speculative") + page("Pages purgeable");
  return available * pageSize;
}

async function availableMemory(): Promise<number> {
  if (process.platform === "darwin") {
    try {
      const { stdout } = await execFileAsync("vm_stat", [], { timeout: 3000 });
      const available = parseVmStat(stdout);
      if (available !== null) return available;
    } catch {
      // fall through to os.freemem
    }
  }
  return os.freemem();
}

export async function machineResources(): Promise<MachineReport> {
  let processes: ProcessRow[] = [];
  try {
    const { stdout } = await execFileAsync("ps", ["-axo", "pid=,ppid=,pcpu=,rss=,etime=,args="], {
      timeout: 5000,
      maxBuffer: 16 * 1024 * 1024,
    });
    processes = parsePsOutput(stdout);
  } catch {
    processes = [];
  }
  const byKind: MachineReport["byKind"] = {
    rudder: { cpu: 0, rss: 0, count: 0 },
    claude: { cpu: 0, rss: 0, count: 0 },
    codex: { cpu: 0, rss: 0, count: 0 },
    opencode: { cpu: 0, rss: 0, count: 0 },
    other: { cpu: 0, rss: 0, count: 0 },
  };
  for (const row of processes) {
    byKind[row.kind].cpu += row.cpu;
    byKind[row.kind].rss += row.rss;
    byKind[row.kind].count += 1;
  }
  const self = processes.find((p) => p.pid === process.pid);
  return {
    at: new Date().toISOString(),
    cpus: os.cpus().length,
    load: os.loadavg(),
    totalMem: os.totalmem(),
    freeMem: await availableMemory(),
    rudder: { cpu: self?.cpu ?? 0, rss: self?.rss ?? process.memoryUsage().rss, pid: process.pid },
    processes: processes.slice(0, 60),
    byKind,
  };
}
