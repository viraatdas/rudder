#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";

const nativeBinaryBase = "rudder-native";
const nativeBinaryName = process.platform === "win32" ? `${nativeBinaryBase}.exe` : nativeBinaryBase;
const platformKey = `${process.platform}-${process.arch}`;
const distNativeDir = path.join("dist", "native");

function copyExecutable(src, dest) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  if (process.platform !== "win32") {
    fs.chmodSync(dest, 0o755);
  }
}

function copyCurrentBuild() {
  const sources = [
    path.join("target", "release", nativeBinaryName),
    path.join("native", "target", "release", nativeBinaryName),
  ];
  const src = sources.find((candidate) => fs.existsSync(candidate));
  if (!src) {
    return false;
  }
  copyExecutable(src, path.join(distNativeDir, platformKey, nativeBinaryName));
  return true;
}

function copyPrebuiltArtifacts() {
  const root = process.env.RUDDER_NATIVE_PREBUILTS;
  if (!root || !fs.existsSync(root)) {
    return 0;
  }
  let copied = 0;
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const key = entry.name.replace(/^rudder-native-/, "");
    const dir = path.join(root, entry.name);
    const candidates = [
      path.join(dir, "rudder-native"),
      path.join(dir, "rudder-native.exe"),
      path.join(dir, key, "rudder-native"),
      path.join(dir, key, "rudder-native.exe"),
      path.join(dir, "native-dist", key, "rudder-native"),
      path.join(dir, "native-dist", key, "rudder-native.exe"),
    ];
    const src = candidates.find((candidate) => fs.existsSync(candidate)) ?? findNestedNativeBinary(dir);
    if (!src) {
      continue;
    }
    const name = src.endsWith(".exe") ? "rudder-native.exe" : "rudder-native";
    copyExecutable(src, path.join(distNativeDir, key, name));
    copied += 1;
  }
  return copied;
}

function findNestedNativeBinary(dir) {
  const pending = [{ dir, depth: 0 }];
  while (pending.length > 0) {
    const current = pending.pop();
    if (!current || current.depth > 4) {
      continue;
    }
    for (const entry of fs.readdirSync(current.dir, { withFileTypes: true })) {
      const full = path.join(current.dir, entry.name);
      if (entry.isFile() && (entry.name === "rudder-native" || entry.name === "rudder-native.exe")) {
        return full;
      }
      if (entry.isDirectory()) {
        pending.push({ dir: full, depth: current.depth + 1 });
      }
    }
  }
  return undefined;
}

function removeLegacyFlatBinary() {
  fs.rmSync(path.join(distNativeDir, "rudder-native"), { force: true });
  fs.rmSync(path.join(distNativeDir, "rudder-native.exe"), { force: true });
}

copyCurrentBuild();
copyPrebuiltArtifacts();
removeLegacyFlatBinary();
