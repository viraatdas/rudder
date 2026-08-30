import os from "node:os";
import path from "node:path";
import { promises as fs } from "node:fs";
import { ensureDir, pathExists, runCommand, shortenHome } from "../util.js";
import { improveHome } from "./state.js";

const LAUNCHD_LABEL = "dev.rudder.improve";

function plistPath(): string {
  return path.join(os.homedir(), "Library", "LaunchAgents", `${LAUNCHD_LABEL}.plist`);
}

function launchdLogPath(): string {
  return path.join(improveHome(), "launchd.log");
}

/**
 * Scheduled batch, not a resident daemon (docs/continual-improvement.md §2).
 * The plist shells through a login shell so the globally installed `rudder`
 * resolves via the user's normal PATH. launchd fires missed
 * StartCalendarInterval jobs on wake, which is exactly the laptop behavior we
 * want.
 */
function renderPlist(): string {
  const log = launchdLogPath();
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/zsh</string>
    <string>-lc</string>
    <string>rudder improve run</string>
  </array>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key>
    <integer>3</integer>
    <key>Minute</key>
    <integer>30</integer>
  </dict>
  <key>StandardOutPath</key>
  <string>${log}</string>
  <key>StandardErrorPath</key>
  <string>${log}</string>
</dict>
</plist>
`;
}

export async function scheduleCommand(action: string): Promise<void> {
  if (process.platform !== "darwin") {
    throw new Error("rudder improve schedule currently supports macOS (launchd) only; run `rudder improve run` from cron/systemd on other platforms.");
  }
  switch (action) {
    case "install": {
      await ensureDir(path.dirname(plistPath()));
      await ensureDir(improveHome());
      await fs.writeFile(plistPath(), renderPlist(), "utf8");
      const uid = process.getuid?.() ?? 501;
      // Re-bootstrap idempotently: boot out any prior registration first.
      await runCommand("launchctl", ["bootout", `gui/${uid}/${LAUNCHD_LABEL}`], { allowFailure: true });
      const bootstrap = await runCommand("launchctl", ["bootstrap", `gui/${uid}`, plistPath()], {
        allowFailure: true,
      });
      if (bootstrap.code !== 0) {
        await runCommand("launchctl", ["load", "-w", plistPath()]);
      }
      console.log(`Installed ${shortenHome(plistPath())}: nightly \`rudder improve run\` at 03:30.`);
      console.log('Default autonomy is "propose": reviewed branches are pushed for you, but releases require a human.');
      console.log('Set improve.autonomy to "ship" only if you explicitly want unattended npm releases.');
      console.log(`Logs: ${shortenHome(launchdLogPath())}`);
      return;
    }
    case "uninstall": {
      const uid = process.getuid?.() ?? 501;
      await runCommand("launchctl", ["bootout", `gui/${uid}/${LAUNCHD_LABEL}`], { allowFailure: true });
      await runCommand("launchctl", ["unload", plistPath()], { allowFailure: true });
      await fs.rm(plistPath(), { force: true });
      console.log(`Removed ${shortenHome(plistPath())}.`);
      return;
    }
    case "status": {
      const installed = await pathExists(plistPath());
      const uid = process.getuid?.() ?? 501;
      const print = await runCommand("launchctl", ["print", `gui/${uid}/${LAUNCHD_LABEL}`], {
        allowFailure: true,
      });
      console.log(`plist: ${installed ? shortenHome(plistPath()) : "not installed"}`);
      console.log(`launchd: ${print.code === 0 ? "loaded" : "not loaded"}`);
      return;
    }
    default:
      throw new Error("Usage: rudder improve schedule install|uninstall|status");
  }
}
