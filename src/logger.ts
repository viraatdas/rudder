import fs from "node:fs";
import path from "node:path";
import { rudderHome } from "./util.js";

export type LogLevel = "error" | "warn" | "info" | "debug";

type LogFields = Record<string, unknown>;

const LEVELS: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

export const LOG_MAX_BYTES = 5 * 1024 * 1024;
export const LOG_KEEP_ROTATED = 2;

export class Logger {
  constructor(
    private readonly context: LogFields,
    private readonly fileName = "rudder.ndjson",
  ) {}

  child(context: LogFields): Logger {
    return new Logger({ ...this.context, ...context }, this.fileName);
  }

  error(message: string, fields?: LogFields): void {
    this.write("error", message, fields);
  }

  warn(message: string, fields?: LogFields): void {
    this.write("warn", message, fields);
  }

  info(message: string, fields?: LogFields): void {
    this.write("info", message, fields);
  }

  debug(message: string, fields?: LogFields): void {
    this.write("debug", message, fields);
  }

  private write(level: LogLevel, message: string, fields?: LogFields): void {
    if (LEVELS[level] > configuredLevel()) {
      return;
    }
    const record = {
      ts: new Date().toISOString(),
      level,
      message,
      ...this.context,
      ...(fields ?? {}),
    };
    writeLogRecord(this.fileName, record);
    if (process.stderr.isTTY && process.env.RUDDER_NATIVE_TUI !== "1") {
      process.stderr.write(`rudder ${level}: ${message}\n`);
    }
  }
}

export function createLogger(component: string, options?: { fileName?: string }): Logger {
  return new Logger({ component }, options?.fileName);
}

export function logsDir(): string {
  return path.join(rudderHome(), "logs");
}

export function diagnosticLogPath(fileName = "rudder.ndjson"): string {
  return path.join(logsDir(), fileName);
}

export function rotateLogFile(filePath: string): void {
  try {
    if (fs.statSync(filePath).size < LOG_MAX_BYTES) {
      return;
    }
  } catch {
    return;
  }
  for (let index = LOG_KEEP_ROTATED; index >= 1; index -= 1) {
    const from = `${filePath}.${index}`;
    const to = `${filePath}.${index + 1}`;
    try {
      if (index === LOG_KEEP_ROTATED) {
        fs.rmSync(from, { force: true });
      } else if (fs.existsSync(from)) {
        fs.renameSync(from, to);
      }
    } catch {
      // Best-effort diagnostics must never break user-facing commands.
    }
  }
  try {
    fs.renameSync(filePath, `${filePath}.1`);
  } catch {
    // ignore
  }
}

function writeLogRecord(fileName: string, record: Record<string, unknown>): void {
  const dir = logsDir();
  const filePath = path.join(dir, fileName);
  try {
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    rotateLogFile(filePath);
    fs.appendFileSync(filePath, `${JSON.stringify(record)}\n`, { encoding: "utf8", mode: 0o600 });
  } catch {
    // Logging is diagnostics only. Never make Rudder fail because the log path is unwritable.
  }
}

function configuredLevel(): number {
  const raw = process.env.RUDDER_LOG?.trim().toLowerCase();
  if (raw === "debug" || raw === "info" || raw === "warn" || raw === "error") {
    return LEVELS[raw];
  }
  return LEVELS.warn;
}
