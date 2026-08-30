// `rudder cloud attach` has to set up the LOCAL terminal on the remote
// dashboard's behalf.
//
// The remote TUI asks for the kitty keyboard protocol exactly once, in its own
// setup_terminal(), and the worker supervisor only keeps the TAIL of its output
// (256KB). By the time you attach, that request is long evicted — it never
// reaches your terminal. The mouse modes already had this bug and were fixed by
// enabling them locally; the keyboard flags had not been.
//
// The user-visible symptom: ⌥h stops hiding panes in cloud. Without the
// protocol, macOS terminals report Option+h as the character it TYPES (˙ on a
// US layout), not as Alt+h, and the dashboard only honours the bare ˙ outside
// the task pane — which is the pane you start in. Alt+[ / Alt+] cannot be
// encoded at all: ESC-[ is the CSI introducer.
//
// So: assert the real CLI, in a real PTY, pushes the flags on connect and pops
// them on the way out.
import assert from "node:assert/strict";
import fs from "node:fs";
import fsp from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { test as nodeTest } from "node:test";

import { launch } from "tui-integration-tests";
import { WebSocketServer } from "ws";

import { repoRoot } from "./helpers.mjs";

const cli = path.join(repoRoot, "dist", "index.js");
const test = fs.existsSync(cli)
  ? nodeTest
  : (name, opts, fn) => nodeTest(name, { skip: "dist/index.js not built (npm run build)" }, fn ?? opts);

// crossterm's PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES |
// REPORT_ALTERNATE_KEYS) — the exact request native/src/main.rs makes.
const KITTY_PUSH = "\x1b[>5u";
const KITTY_POP = "\x1b[<u";

async function freePort() {
  return await new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

/**
 * A control plane that is only an attach endpoint: it accepts the socket, sends
 * one binary frame (which is what makes the client hand off from its splash),
 * and records every keystroke the client forwards.
 */
async function fakeRelay() {
  const port = await freePort();
  const keystrokes = [];
  let onConnect = () => {};
  const wss = new WebSocketServer({ port, host: "127.0.0.1" });
  wss.on("connection", (socket, request) => {
    if (!/\/api\/rudder\/sail\/[^/]+\/attach$/.test(request.url ?? "")) {
      socket.close(1008, "unexpected path");
      return;
    }
    socket.binaryType = "nodebuffer";
    socket.on("message", (data, isBinary) => {
      if (isBinary && Buffer.isBuffer(data)) keystrokes.push(data);
    });
    socket.send(JSON.stringify({ type: "status", state: "worker-connected", control: "active" }));
    // One frame of "screen", so the client leaves the splash and reaches the
    // steady state a human would be sitting in.
    socket.send(Buffer.from("REMOTE-DASHBOARD-FRAME\r\n", "utf8"), { binary: true });
    onConnect(socket);
  });
  await new Promise((resolve) => wss.once("listening", resolve));
  return {
    port,
    keystrokes,
    /** Hang up like a worker that exited, so the client runs its cleanup path. */
    hangUp: () => new Promise((resolve) => {
      onConnect = (socket) => { socket.close(1000, "worker-exit"); resolve(); };
      for (const socket of wss.clients) { socket.close(1000, "worker-exit"); resolve(); }
    }),
    workerDisconnected: () => {
      for (const socket of wss.clients) {
        socket.send(JSON.stringify({ type: "status", state: "worker-disconnected", control: "active" }));
      }
    },
    close: () => new Promise((resolve) => wss.close(resolve)),
  };
}

test("cloud attach asks the local terminal for the kitty keyboard protocol", { timeout: 60_000 }, async (t) => {
  const relay = await fakeRelay();
  t.after(() => relay.close());

  const session = await launch({
    binary: process.execPath,
    args: [cli, "cloud", "attach", "sail-key-test"],
    cwd: repoRoot,
    cols: 100,
    rows: 30,
    env: {
      RUDDER_CLOUD_URL: `http://127.0.0.1:${relay.port}`,
      RUDDER_CLOUD_TOKEN: "rdr_test_token",
      TERM: "xterm-256color",
    },
  }, t);

  await session.waitForText("REMOTE-DASHBOARD-FRAME", { timeout: 20_000 });

  // The push has to be on the wire to the terminal. It is a mode-setting
  // sequence, so it leaves no mark on the rendered screen — read the driver's
  // cast of the raw output instead.
  assert.ok(session.recordingPath, "the session was recorded");
  const beforeExit = await fsp.readFile(session.recordingPath, "utf8");
  assert.ok(
    beforeExit.includes(JSON.stringify(KITTY_PUSH).slice(1, -1)),
    "attach pushes the kitty keyboard flags onto the local terminal",
  );

  // Keystrokes still reach the remote: pushing a keyboard mode must not swallow
  // input on the way through.
  await session.type("x");
  await new Promise((resolve) => setTimeout(resolve, 500));
  assert.ok(
    Buffer.concat(relay.keystrokes).toString("utf8").includes("x"),
    "typed bytes are still forwarded to the worker",
  );

  // On the way out the terminal is handed back the way we found it. Leaving the
  // protocol pushed would make the user's SHELL report keys in CSI-u.
  await relay.hangUp();
  await session.waitForExit({ timeout: 20_000 });
  const afterExit = await fsp.readFile(session.recordingPath, "utf8");
  assert.ok(
    afterExit.includes(JSON.stringify(KITTY_POP).slice(1, -1)),
    "attach pops the flags again when it detaches",
  );
});

test("Ctrl+C detaches locally while the cloud worker is disconnected", { timeout: 60_000 }, async (t) => {
  const relay = await fakeRelay();
  t.after(() => relay.close());

  const session = await launch({
    binary: process.execPath,
    args: [cli, "cloud", "attach", "disconnected-worker-test"],
    cwd: repoRoot,
    cols: 100,
    rows: 30,
    env: {
      RUDDER_CLOUD_URL: `http://127.0.0.1:${relay.port}`,
      RUDDER_CLOUD_TOKEN: "rdr_test_token",
      TERM: "xterm-256color",
    },
  }, t);

  await session.waitForText("REMOTE-DASHBOARD-FRAME", { timeout: 20_000 });
  relay.workerDisconnected();
  await session.waitForText("Cloud worker disconnected", { timeout: 20_000 });
  // Rudder enables Kitty keyboard disambiguation, where Ctrl+C is CSI 99;5u
  // instead of the legacy one-byte ETX value.
  await session.write("\x1b[99;5u");
  await session.waitForExit({ timeout: 20_000 });

  assert.ok(
    !Buffer.concat(relay.keystrokes).includes(0x03),
    "disconnect Ctrl+C never enters the remote input stream",
  );
});
