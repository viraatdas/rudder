import path from "node:path";

const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

// These values belong to the worker supervisor or affect Node before Rudder
// starts. A captured laptop environment must not be able to replace them.
const CAPTURED_ENV_BLOCKLIST = new Set([
  "HOME",
  "PATH",
  "SHELL",
  "USER",
  "LOGNAME",
  "NODE_OPTIONS",
  "NODE_PATH",
  "LD_PRELOAD",
  "LD_LIBRARY_PATH",
  "RUDDER_WORKER_TOKEN",
  "RUDDER_SNAPSHOT_URL",
  "RUDDER_CLOUD_TOKEN",
  "RUDDER_CLOUD_URL",
  "RUDDER_SAIL_ID",
  "RUDDER_WORKSPACE_ID",
  "FLY_API_TOKEN",
]);

export function sanitizeRepoName(value) {
  const base = path.posix.basename(String(value || "repo").replaceAll("\\", "/"));
  const safe = base
    .normalize("NFKC")
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^\.+/, "")
    .replace(/-+/g, "-")
    .slice(0, 80)
    .replace(/[.-]+$/g, "");
  return safe || "repo";
}

export function isPathInside(root, candidate) {
  const relative = path.relative(path.resolve(root), path.resolve(candidate));
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

export function safePathSegment(value, maxLength = 128) {
  const text = typeof value === "string" ? value.trim() : "";
  return text.length > 0
    && text.length <= maxLength
    && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(text)
    ? text
    : null;
}

export function filterCapturedEnv(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return {};
  }
  const out = {};
  let totalBytes = 0;
  for (const [key, entry] of Object.entries(value)) {
    if (
      typeof entry !== "string"
      || entry.includes("\0")
      || !ENV_NAME_RE.test(key)
      || CAPTURED_ENV_BLOCKLIST.has(key)
      || key.startsWith("DYLD_")
    ) {
      continue;
    }
    const entryBytes = Buffer.byteLength(key) + Buffer.byteLength(entry) + 2;
    if (entryBytes > 64 * 1024 || totalBytes + entryBytes > 512 * 1024) continue;
    out[key] = entry;
    totalBytes += entryBytes;
  }
  return out;
}

export function buildChildEnv(baseEnv, capturedEnv, overrides = {}) {
  const env = {
    ...baseEnv,
    ...filterCapturedEnv(capturedEnv),
    ...overrides,
  };
  for (const secret of [
    "RUDDER_WORKER_TOKEN",
    "RUDDER_SNAPSHOT_URL",
    "RUDDER_CLOUD_TOKEN",
    "FLY_API_TOKEN",
  ]) {
    delete env[secret];
  }
  return env;
}

export function parseControlMessage(text) {
  let payload;
  try {
    payload = JSON.parse(text);
  } catch {
    return null;
  }
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    return null;
  }
  if (payload.type === "resize" && Number.isFinite(payload.cols) && Number.isFinite(payload.rows)) {
    return {
      type: "resize",
      cols: Math.max(20, Math.min(500, Math.floor(payload.cols))),
      rows: Math.max(5, Math.min(200, Math.floor(payload.rows))),
    };
  }
  if (
    payload.type === "signal"
    && ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"].includes(payload.name)
  ) {
    return { type: "signal", name: payload.name };
  }
  return null;
}

export function reconnectDelay(attempt, random = Math.random) {
  const exponent = Math.max(0, Math.min(5, Number(attempt) || 0));
  const base = Math.min(30_000, 1_000 * (2 ** exponent));
  return Math.floor(base * (0.8 + Math.max(0, Math.min(1, random())) * 0.4));
}

/** A bounded FIFO for PTY output waiting on WebSocket backpressure. */
export class BoundedByteQueue {
  constructor(maxBytes) {
    this.maxBytes = Math.max(1, Math.floor(maxBytes));
    this.items = [];
    this.bytes = 0;
    this.droppedBytes = 0;
  }

  get length() {
    return this.items.length;
  }

  push(value) {
    let chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
    if (chunk.length > this.maxBytes) {
      this.droppedBytes += chunk.length - this.maxBytes;
      chunk = chunk.subarray(chunk.length - this.maxBytes);
    }
    this.items.push(chunk);
    this.bytes += chunk.length;
    this.#trim();
  }

  unshift(value) {
    let chunk = Buffer.isBuffer(value) ? value : Buffer.from(value);
    if (chunk.length > this.maxBytes) {
      this.droppedBytes += chunk.length - this.maxBytes;
      chunk = chunk.subarray(0, this.maxBytes);
    }
    this.items.unshift(chunk);
    this.bytes += chunk.length;
    this.#trim(true);
  }

  shift() {
    const chunk = this.items.shift();
    if (chunk) {
      this.bytes -= chunk.length;
    }
    return chunk;
  }

  #trim(fromTail = false) {
    while (this.bytes > this.maxBytes && this.items.length > 0) {
      const chunk = fromTail ? this.items.pop() : this.items.shift();
      if (!chunk) break;
      this.bytes -= chunk.length;
      this.droppedBytes += chunk.length;
    }
  }
}
