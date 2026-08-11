// Rudder-specific fixture glue for the screen-level TUI tests. The generic
// mechanics (PTY, keystrokes, polling, normalizers) live in the
// tui-integration-tests framework; this file only knows how to stand up a
// rudder-shaped world for it: a throwaway git+jj repo, fake agent backends,
// and the env that keeps the dashboard hermetic.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { launch, defaultNormalizers } from "tui-integration-tests";

const here = path.dirname(fileURLToPath(import.meta.url));
export const repoRoot = path.resolve(here, "..", "..");

/** The binary under test. CI points RUDDER_NATIVE_BIN at a prebuilt artifact. */
export const nativeBin =
  process.env.RUDDER_NATIVE_BIN ?? path.join(repoRoot, "target", "debug", "rudder-native");

function sh(cwd, cmd, args) {
  execFileSync(cmd, args, { cwd, stdio: "pipe" });
}

/**
 * Remove a scratch repo, tolerating the app's asynchronous shutdown: the
 * dashboard's children (jj, worker PTYs, the CLI) can still be flushing state
 * files while rm walks the tree, which surfaces as ENOTEMPTY. Deleting is
 * cleanup, not an assertion — retry briefly instead of failing a green test.
 */
export async function removeScratch(dir) {
  for (let attempt = 0; ; attempt += 1) {
    try {
      await fsp.rm(dir, { recursive: true, force: true });
      return;
    } catch (error) {
      if (attempt >= 5) throw error;
      await new Promise((resolve) => setTimeout(resolve, 250 * (attempt + 1)));
    }
  }
}

/**
 * A throwaway git repo with jj colocated — the same shape every dashboard
 * expects to sit in. Lives under the OS tmpdir; caller removes it via t.after.
 */
export async function scratchRepo(prefix = "rudder-tui-") {
  // realpath matters: macOS tmpdirs live behind the /var -> /private/var
  // symlink, and the dashboard canonicalizes its cwd. Fixtures keyed on the
  // uncanonicalized spelling (Claude project-dir encoding, origin labels)
  // would silently miss.
  const dir = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), prefix)));
  sh(dir, "git", ["init", "-q", "."]);
  sh(dir, "git", ["config", "user.email", "tui-test@rudder.local"]);
  sh(dir, "git", ["config", "user.name", "TUI Test"]);
  await fsp.writeFile(path.join(dir, "README.md"), "scratch repo for TUI tests\n");
  sh(dir, "jj", ["git", "init", "--colocate"]);
  sh(dir, "jj", ["describe", "-m", "base"]);
  return dir;
}

/**
 * Fake agent backends. `sleeper` stands in for a long-running conversation;
 * `completer` acts like an agent that does its work and exits cleanly — it
 * writes a real file into its cwd (the jj workspace), so the run has an
 * honest diff for the review gate and merge to operate on.
 */
export async function fakeBackends(dir) {
  const bin = path.join(dir, "fake-bin");
  await fsp.mkdir(bin, { recursive: true });
  const write = async (name, body) => {
    const file = path.join(bin, name);
    await fsp.writeFile(file, body);
    await fsp.chmod(file, 0o755);
    return file;
  };
  const sleeper = await write("claude-sleeper", "#!/bin/sh\nsleep 300\n");
  const completer = await write(
    "claude-completer",
    '#!/bin/sh\nprintf "doing the work\\n"\necho "done marker" > DONE.txt\nsleep 1\nprintf "finished\\n"\nexit 0\n',
  );
  return { sleeper, completer };
}

/** Run-id slugs like `verify-launch-works-856351af` churn per run. */
const rudderNormalizers = [
  ...defaultNormalizers,
  [/-[a-f0-9]{6,10}\b/g, "-<slug>"],
];

/**
 * Boot the real dashboard binary in a PTY against `repo`, hermetically:
 * state under a per-test RUDDER_HOME, backends faked, network off, and the
 * repo's own CLI (dist/index.js) rather than any globally installed rudder.
 */
export async function launchRudder(t, { repo, claudeBin, home, cols = 120, rows = 40, env = {} }) {
  const rudderHome = path.join(repo, ".tui-home");
  await fsp.mkdir(rudderHome, { recursive: true });
  const session = await launch({
    binary: nativeBin,
    cols,
    rows,
    cwd: repo,
    normalizers: rudderNormalizers,
    env: {
      RUDDER_HOME: rudderHome,
      RUDDER_OFFLINE: "1",
      RUDDER_CLI: path.join(repoRoot, "dist", "index.js"),
      RUDDER_CLAUDE_BIN: claudeBin,
      RUDDER_CODEX_BIN: claudeBin,
      TERM: "xterm-256color",
      ...(home ? { HOME: home } : {}),
      ...env,
    },
  }, t);
  return session;
}

/**
 * A planted Claude conversation transcript, shaped like the real ones under
 * ~/.claude/projects: project folder named after the encoded cwd, one JSONL
 * per session, interactive marker first, then the opening user prompt with
 * the cwd stamped on it (which is what the /resume picker's origin label
 * reads).
 */
export async function plantTranscript(home, sessionCwd, sessionId, title) {
  const encoded = sessionCwd.replaceAll(/[^A-Za-z0-9]/g, "-");
  const dir = path.join(home, ".claude", "projects", encoded);
  await fsp.mkdir(dir, { recursive: true });
  const lines = [
    JSON.stringify({ type: "mode", mode: "normal" }),
    JSON.stringify({
      cwd: sessionCwd,
      type: "user",
      message: { role: "user", content: title },
    }),
    JSON.stringify({
      type: "assistant",
      message: { role: "assistant", model: "claude-opus-4-5-20251101", content: "ok" },
    }),
  ];
  await fsp.writeFile(path.join(dir, `${sessionId}.jsonl`), lines.join("\n"));
}

export { defaultNormalizers };

/** True if jj is available; the whole suite is meaningless without it. */
export function jjAvailable() {
  try {
    execFileSync("jj", ["--version"], { stdio: "pipe" });
    return true;
  } catch {
    return false;
  }
}

/** The native binary must exist before the suite runs; fail loudly, not per-test. */
export function assertPrerequisites() {
  if (!fs.existsSync(nativeBin)) {
    throw new Error(
      `rudder-native not found at ${nativeBin}; run \`cargo build --manifest-path native/Cargo.toml\` or set RUDDER_NATIVE_BIN`,
    );
  }
  if (!jjAvailable()) {
    throw new Error("jj is required for the TUI test suite");
  }
}
