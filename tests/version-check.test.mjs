import assert from "node:assert/strict";
import test from "node:test";

import { compareSemver, shouldAutoUpdateFromPackageRoot } from "../dist/version-check.js";

test("compareSemver orders patch/minor/major versions", () => {
  assert.equal(compareSemver("2.2.4", "2.2.5"), -1);
  assert.equal(compareSemver("2.3.0", "2.2.9"), 1);
  assert.equal(compareSemver("3.0.0", "3.0.0"), 0);
  assert.equal(compareSemver("3.0.0-beta.1", "3.0.0"), 0);
});

test("auto-update eligibility skips dev, npx, CI, and disabled environments", (t) => {
  const oldDisable = process.env.RUDDER_DISABLE_AUTO_UPDATE;
  const oldSkip = process.env.RUDDER_SKIP_AUTO_UPDATE;
  const oldCheck = process.env.RUDDER_DISABLE_UPDATE_CHECK;
  const oldCi = process.env.CI;
  t.after(() => {
    restoreEnv("RUDDER_DISABLE_AUTO_UPDATE", oldDisable);
    restoreEnv("RUDDER_SKIP_AUTO_UPDATE", oldSkip);
    restoreEnv("RUDDER_DISABLE_UPDATE_CHECK", oldCheck);
    restoreEnv("CI", oldCi);
  });

  delete process.env.RUDDER_DISABLE_AUTO_UPDATE;
  delete process.env.RUDDER_SKIP_AUTO_UPDATE;
  delete process.env.RUDDER_DISABLE_UPDATE_CHECK;
  delete process.env.CI;

  assert.equal(shouldAutoUpdateFromPackageRoot("/usr/local/lib/node_modules/@viraatdas/rudder"), true);
  assert.equal(shouldAutoUpdateFromPackageRoot(process.cwd()), false, "source checkout with src/main.ts is skipped");
  assert.equal(shouldAutoUpdateFromPackageRoot("/tmp/.npm/_npx/abc/node_modules/@viraatdas/rudder"), false);

  process.env.RUDDER_DISABLE_AUTO_UPDATE = "1";
  assert.equal(shouldAutoUpdateFromPackageRoot("/usr/local/lib/node_modules/@viraatdas/rudder"), false);
});

function restoreEnv(key, value) {
  if (value === undefined) {
    delete process.env[key];
  } else {
    process.env[key] = value;
  }
}
