import assert from "node:assert/strict";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { auditContext } from "../dist/context-audit.js";

async function tempRepo() {
  return fsp.mkdtemp(path.join(os.tmpdir(), "rudder-context-audit-"));
}

test("auditContext flags secrets and duplicated context lines", async () => {
  const repo = await tempRepo();
  const repeated = "Always run the focused test command before claiming the task is done.";
  await fsp.writeFile(path.join(repo, "AGENTS.md"), `${repeated}\n`);
  await fsp.writeFile(path.join(repo, "CLAUDE.md"), `${repeated}\nAPI_KEY=sk-testtoken1234567890abcdef\n`);

  const report = await auditContext(repo);
  assert.equal(report.files.length, 2);
  assert.ok(report.findings.some((finding) => finding.severity === "high" && finding.message.includes("secret-like")));
  assert.ok(report.findings.some((finding) => finding.message.includes("duplicates")));
});
