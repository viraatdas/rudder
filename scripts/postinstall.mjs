#!/usr/bin/env node
// Ensures jj (Jujutsu) is installed. Rudder uses jj as its required internal
// version-control substrate, so a fresh `npm install -g @viraatdas/rudder`
// should leave the user ready to run. This runs on install and NEVER fails the
// install: on any problem it prints guidance and exits 0.
import { spawnSync } from "node:child_process";

const TAG = "[rudder postinstall]";
const log = (m) => console.log(`${TAG} ${m}`);
const warn = (m) => console.warn(`${TAG} ${m}`);

function has(cmd) {
  try {
    const probe = process.platform === "win32" ? "where" : "which";
    return spawnSync(probe, [cmd], { stdio: "ignore" }).status === 0;
  } catch {
    return false;
  }
}

function run(cmd, args) {
  log(`running: ${cmd} ${args.join(" ")}`);
  try {
    return spawnSync(cmd, args, { stdio: "inherit" }).status === 0;
  } catch {
    return false;
  }
}

function manualHelp() {
  warn("Could not auto-install jj. Rudder requires jj (Jujutsu).");
  warn("Install it with one of:");
  warn("  macOS:  brew install jj");
  warn("  Rust:   cargo install jj-cli");
  warn("  Other:  https://jj-vcs.github.io/jj/latest/install-and-setup/");
  warn("Then run: rudder doctor");
}

function main() {
  if (process.env.RUDDER_SKIP_POSTINSTALL) {
    return;
  }
  if (has("jj")) {
    log("jj already installed.");
    return;
  }
  log("jj not found. Rudder requires jj (Jujutsu) as its internal substrate; attempting to install it.");
  let ok = false;
  if (process.platform === "darwin" && has("brew")) {
    ok = run("brew", ["install", "jj"]);
  } else if (has("cargo")) {
    log("installing jj via cargo (compiles from source; this can take a few minutes)...");
    ok = run("cargo", ["install", "jj-cli"]);
  }
  if (ok && has("jj")) {
    log("jj installed. Run `rudder doctor` to verify.");
  } else {
    manualHelp();
  }
}

try {
  main();
} catch (err) {
  warn(`jj auto-install skipped: ${err && err.message ? err.message : err}`);
  manualHelp();
}
process.exit(0);
