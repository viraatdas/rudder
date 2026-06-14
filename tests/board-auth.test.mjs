import assert from "node:assert/strict";
import test from "node:test";

import { getBoardToken, hasValidToken, isLoopbackHost } from "../dist/board/daemon.js";

// The guards only read req.headers, so a plain object stands in for IncomingMessage.
const reqWith = (headers) => ({ headers });

test("isLoopbackHost accepts loopback hosts and rejects everything else (anti-DNS-rebinding)", () => {
  for (const host of ["127.0.0.1:4774", "localhost:4774", "127.0.0.1", "localhost", "[::1]:4774"]) {
    assert.equal(isLoopbackHost(reqWith({ host })), true, `loopback: ${host}`);
  }
  for (const host of ["evil.com", "evil.com:4774", "rudder.example.com", "192.168.1.5:4774", ""]) {
    assert.equal(isLoopbackHost(reqWith({ host })), false, `non-loopback: ${host}`);
  }
  assert.equal(isLoopbackHost(reqWith({})), false, "missing Host is rejected");
});

test("hasValidToken accepts only the daemon's secret token (CSRF/forgery guard)", () => {
  const token = getBoardToken();
  assert.ok(token.length >= 32, "token is a non-trivial secret");
  assert.equal(hasValidToken(reqWith({ "x-rudder-token": token })), true, "correct token");
  assert.equal(hasValidToken(reqWith({ "x-rudder-token": "wrong" })), false, "wrong token");
  assert.equal(hasValidToken(reqWith({ "x-rudder-token": "" })), false, "empty token");
  assert.equal(hasValidToken(reqWith({})), false, "missing token header");
  // A length-matched but different token must still fail (no prefix/partial match).
  const near = `${token.slice(0, -1)}${token.endsWith("0") ? "1" : "0"}`;
  assert.equal(hasValidToken(reqWith({ "x-rudder-token": near })), false, "near-miss token");
});
