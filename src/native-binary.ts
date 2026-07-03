import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const nativeBinaryBaseName = "rudder-native";

export function nativePlatformKey(platform = process.platform, arch = process.arch): string {
  return `${platform}-${arch}`;
}

export function resolveNativeBinaryPath(): string | undefined {
  const moduleDir = path.dirname(fileURLToPath(import.meta.url));
  return resolveNativeBinaryPathFrom(moduleDir);
}

export function resolveNativeBinaryPathFrom(
  moduleDir: string,
  platform = process.platform,
  arch = process.arch,
): string | undefined {
  const nativeBinaryName = platform === "win32" ? `${nativeBinaryBaseName}.exe` : nativeBinaryBaseName;
  const candidates = [
    path.resolve(moduleDir, "native", nativePlatformKey(platform, arch), nativeBinaryName),
    // Legacy package layout. Keep it as a fallback for local checkouts, but only
    // if the file header matches this host so a macOS binary is never launched
    // on Linux as /bin/sh input.
    path.resolve(moduleDir, "native", nativeBinaryName),
    path.resolve(moduleDir, "..", "target", "release", nativeBinaryName),
    path.resolve(moduleDir, "..", "native", "target", "release", nativeBinaryName),
  ];

  for (const candidate of candidates) {
    if (isExecutableFile(candidate) && isUsableNativeBinary(candidate, platform, arch)) {
      return candidate;
    }
  }
  return undefined;
}

function isExecutableFile(file: string): boolean {
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}

export function isUsableNativeBinary(
  file: string,
  platform = process.platform,
  arch = process.arch,
): boolean {
  if (process.env.RUDDER_SKIP_NATIVE_BINARY_CHECK === "1") {
    return true;
  }
  let header: Buffer;
  try {
    header = fs.readFileSync(file);
  } catch {
    return false;
  }
  if (platform === "linux") {
    return isElfForArch(header, arch);
  }
  if (platform === "darwin") {
    return isMachOForArch(header, arch);
  }
  if (platform === "win32") {
    return header.length >= 2 && header[0] === 0x4d && header[1] === 0x5a;
  }
  return true;
}

function isElfForArch(header: Buffer, arch: string): boolean {
  if (header.length < 20 || header[0] !== 0x7f || header[1] !== 0x45 || header[2] !== 0x4c || header[3] !== 0x46) {
    return false;
  }
  const elfClass = header[4];
  const littleEndian = header[5] !== 2;
  const machine = littleEndian ? header.readUInt16LE(18) : header.readUInt16BE(18);
  const expected = elfMachineForArch(arch);
  if (expected === undefined) {
    return true;
  }
  if ((arch === "x64" || arch === "arm64") && elfClass !== 2) {
    return false;
  }
  return machine === expected;
}

function elfMachineForArch(arch: string): number | undefined {
  switch (arch) {
    case "x64":
      return 62;
    case "arm64":
      return 183;
    case "arm":
      return 40;
    case "ia32":
      return 3;
    default:
      return undefined;
  }
}

function isMachOForArch(header: Buffer, arch: string): boolean {
  if (header.length < 8) {
    return false;
  }
  const expected = machCpuTypeForArch(arch);
  const magicLe = header.readUInt32LE(0);
  const magicBe = header.readUInt32BE(0);
  const thinLe = magicLe === 0xfeedface || magicLe === 0xfeedfacf;
  const thinBe = magicBe === 0xfeedface || magicBe === 0xfeedfacf;
  if (thinLe || thinBe) {
    if (expected === undefined) {
      return true;
    }
    const cpuType = thinLe ? header.readInt32LE(4) : header.readInt32BE(4);
    return cpuType === expected;
  }
  const fatBe = magicBe === 0xcafebabe || magicBe === 0xcafebabf;
  if (fatBe) {
    if (expected === undefined) {
      return true;
    }
    const archCount = Math.min(header.readUInt32BE(4), 32);
    const entrySize = magicBe === 0xcafebabf ? 32 : 20;
    for (let index = 0; index < archCount; index += 1) {
      const offset = 8 + index * entrySize;
      if (header.length < offset + 4) {
        return false;
      }
      if (header.readInt32BE(offset) === expected) {
        return true;
      }
    }
  }
  return false;
}

function machCpuTypeForArch(arch: string): number | undefined {
  switch (arch) {
    case "x64":
      return 0x01000007;
    case "arm64":
      return 0x0100000c;
    case "arm":
      return 12;
    case "ia32":
      return 7;
    default:
      return undefined;
  }
}
