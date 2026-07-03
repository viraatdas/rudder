import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  isUsableNativeBinary,
  nativePlatformKey,
  resolveNativeBinaryPathFrom,
} from "../dist/native-binary.js";

async function writeExecutable(file, bytes) {
  await fsp.mkdir(path.dirname(file), { recursive: true });
  await fsp.writeFile(file, bytes);
  await fsp.chmod(file, 0o755);
}

function elfHeader(machine) {
  const header = Buffer.alloc(64);
  header[0] = 0x7f;
  header[1] = 0x45;
  header[2] = 0x4c;
  header[3] = 0x46;
  header[4] = 2;
  header[5] = 1;
  header.writeUInt16LE(machine, 18);
  return header;
}

function machOHeader(cpuType) {
  const header = Buffer.alloc(64);
  header.writeUInt32LE(0xfeedfacf, 0);
  header.writeInt32LE(cpuType, 4);
  return header;
}

test("nativePlatformKey includes platform and architecture", () => {
  assert.equal(nativePlatformKey("linux", "x64"), "linux-x64");
  assert.equal(nativePlatformKey("darwin", "arm64"), "darwin-arm64");
});

test("binary guard rejects macOS Mach-O on Linux", async (t) => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-native-bin-"));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  const macho = path.join(root, "rudder-native");
  await writeExecutable(macho, machOHeader(0x0100000c));

  assert.equal(isUsableNativeBinary(macho, "linux", "x64"), false);
  assert.equal(isUsableNativeBinary(macho, "darwin", "arm64"), true);
  assert.equal(isUsableNativeBinary(macho, "darwin", "x64"), false);
});

test("binary guard checks ELF architecture", async (t) => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-native-elf-"));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  const x64 = path.join(root, "rudder-native-x64");
  const arm64 = path.join(root, "rudder-native-arm64");
  await writeExecutable(x64, elfHeader(62));
  await writeExecutable(arm64, elfHeader(183));

  assert.equal(isUsableNativeBinary(x64, "linux", "x64"), true);
  assert.equal(isUsableNativeBinary(x64, "linux", "arm64"), false);
  assert.equal(isUsableNativeBinary(arm64, "linux", "arm64"), true);
  assert.equal(isUsableNativeBinary(arm64, "linux", "x64"), false);
});

test("resolver prefers platform-keyed binary and ignores mismatched legacy flat binary", async (t) => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-native-resolve-"));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  const moduleDir = path.join(root, "dist");
  const platformBinary = path.join(moduleDir, "native", "linux-x64", "rudder-native");
  const legacyBinary = path.join(moduleDir, "native", "rudder-native");

  await writeExecutable(legacyBinary, machOHeader(0x0100000c));
  assert.equal(resolveNativeBinaryPathFrom(moduleDir, "linux", "x64"), undefined);

  await writeExecutable(platformBinary, elfHeader(62));
  assert.equal(resolveNativeBinaryPathFrom(moduleDir, "linux", "x64"), platformBinary);
});
