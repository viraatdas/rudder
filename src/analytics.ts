// ---------------------------------------------------------------------------
// USAGE TELEMETRY.
//
// Rudder emits a small set of anonymous product events so the parts people get
// stuck on are visible: how many installs ever start an agent, how many ever
// merge, which backend they pick, which errors fire most. npm downloads cannot
// answer any of that.
//
// The rules this file exists to enforce:
//   * NEVER send prompts, diffs, file contents, paths, repo names, or session
//     ids. Only the whitelisted properties each call site passes, plus a hashed
//     project id when a cardinality signal is genuinely needed.
//   * NEVER block the caller. A dashboard that stutters because analytics is
//     slow is a worse product than one with no analytics.
//   * ALWAYS be trivially switchable off, and say so on first run. Dev tools
//     that phone home silently get uninstalled, deservedly.
//
// Transport is PostHog's public capture endpoint: one POST with the PROJECT
// token (a write-only key, safe to ship) and `$process_person_profile: false`,
// which keeps events anonymous — no person profile per install.
// ---------------------------------------------------------------------------

import { createHash, randomUUID } from "node:crypto";

import fsp from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

import { loadConfig, saveConfig } from "./state.js";

const POSTHOG_HOST = "https://us.i.posthog.com";
/** Project write key. Public by design; overridable so forks can point elsewhere. */
const POSTHOG_PROJECT_KEY = "phc_2KfOzM4nSxjAgPDfgEikP7Kp7JVcU5aJ0kAyEQykkmn";
/** A hung network must never hold up a CLI command or the dashboard's shutdown. */
const CAPTURE_TIMEOUT_MS = 2_000;

export const TELEMETRY_NOTICE =
  "rudder sends anonymous usage events (which backend, which commands, merge outcomes) — never your prompts, code, paths, or repo names. Turn it off with `rudder telemetry off`.";

export type EventProperties = Record<string, string | number | boolean | undefined>;

function posthogKey(): string {
  return (process.env.RUDDER_POSTHOG_KEY ?? POSTHOG_PROJECT_KEY).trim();
}

function posthogHost(): string {
  return (process.env.RUDDER_POSTHOG_HOST ?? POSTHOG_HOST).trim().replace(/\/+$/, "");
}

/**
 * Off when the user said so, and off in the places where events would be noise
 * rather than signal: CI, the fake-backend test harness, and any run with no
 * project key configured.
 */
export async function telemetryEnabled(): Promise<boolean> {
  if (!posthogKey()) {
    return false;
  }
  const flag = (process.env.RUDDER_TELEMETRY ?? "").trim().toLowerCase();
  if (["0", "off", "false", "no"].includes(flag)) {
    return false;
  }
  if (process.env.CI || process.env.RUDDER_FAKE_BACKEND === "1") {
    return false;
  }
  const config = await loadConfig();
  return config.telemetry !== false;
}

export async function setTelemetryEnabled(enabled: boolean): Promise<void> {
  const config = await loadConfig();
  await saveConfig({ ...config, telemetry: enabled });
}

/**
 * A stable anonymous id per install, minted once and kept in the global config.
 * It identifies a machine's rudder, not a person: no email, no username, no
 * hostname. Without it every event would look like a new user and retention
 * would be meaningless.
 */
export async function installId(): Promise<string> {
  const config = await loadConfig();
  if (config.installId && /^[0-9a-f-]{8,}$/i.test(config.installId)) {
    return config.installId;
  }
  const minted = randomUUID();
  await saveConfig({ ...config, installId: minted });
  return minted;
}

/**
 * Show the telemetry notice exactly once per install and record that we did.
 * Returns the text to print, or undefined when it has already been shown (or
 * telemetry is off, in which case there is nothing to disclose).
 */
export async function pendingTelemetryNotice(): Promise<string | undefined> {
  if (!(await telemetryEnabled())) {
    return undefined;
  }
  const config = await loadConfig();
  if (config.telemetryNoticeShownAt) {
    return undefined;
  }
  await saveConfig({ ...config, telemetryNoticeShownAt: new Date().toISOString() });
  return TELEMETRY_NOTICE;
}

/**
 * A repository identity that cannot be turned back into a path or a name:
 * "how many distinct projects" without saying which. Truncated because a full
 * digest is no more useful here and reads as more identifying than it is.
 */
export function projectHash(repoRoot: string): string {
  return createHash("sha256").update(repoRoot).digest("hex").slice(0, 12);
}

/** The shipped version, read from the package rather than a baked constant. */
async function packageVersion(): Promise<string> {
  const packageFile = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "package.json");
  const raw = await fsp.readFile(packageFile, "utf8").catch(() => "");
  try {
    return (JSON.parse(raw) as { version?: string }).version ?? "unknown";
  } catch {
    return "unknown";
  }
}

async function baseProperties(): Promise<EventProperties> {
  return {
    $process_person_profile: false,
    rudder_version: await packageVersion(),
    platform: `${process.platform}-${process.arch}`,
    node_version: process.versions.node,
    ci: Boolean(process.env.CI),
  };
}

/** Drop undefined values and anything that smells like a path or a secret. */
export function sanitizeProperties(properties: EventProperties): EventProperties {
  const clean: EventProperties = {};
  for (const [key, value] of Object.entries(properties)) {
    if (value === undefined) {
      continue;
    }
    if (typeof value === "string") {
      // A property carrying an absolute path or a token is a bug at the call
      // site; drop it rather than shipping it and apologising later.
      if (value.startsWith("/") || value.startsWith("~/") || /^[A-Za-z]:\\/.test(value)) {
        continue;
      }
      if (/(sk-|ghp_|phc_|xox[baprs]-)/.test(value)) {
        continue;
      }
      clean[key] = value.length > 500 ? `${value.slice(0, 500)}…` : value;
      continue;
    }
    clean[key] = value;
  }
  return clean;
}

/**
 * Send one event. Resolves whether or not it worked: a failed capture is never
 * worth surfacing, and never worth retrying (the next session will tell the
 * same story). Returns true only when PostHog accepted it, for tests.
 */
export async function capture(event: string, properties: EventProperties = {}): Promise<boolean> {
  if (!(await telemetryEnabled())) {
    return false;
  }
  const payload = {
    api_key: posthogKey(),
    event,
    distinct_id: await installId(),
    properties: { ...(await baseProperties()), ...sanitizeProperties(properties) },
    timestamp: new Date().toISOString(),
  };
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), CAPTURE_TIMEOUT_MS);
  try {
    const response = await fetch(`${posthogHost()}/i/v0/e/`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      signal: controller.signal,
    });
    return response.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}
