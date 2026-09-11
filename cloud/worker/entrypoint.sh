#!/usr/bin/env sh
set -eu

# The image bakes whatever Claude Code was current on its build day, and a
# machine boots from the image, so without this a worker ran a stale binary
# for the life of the image tag (the auto-updater's "Updated to latest" only
# ever reached the NEXT launch, and a fresh boot had no next launch to reach).
# Bounded: a fresh image is a two-second no-op, a stale one downloads once.
# Skipped entirely with RUDDER_WORKER_SKIP_AGENT_UPDATE=1.
if [ "${RUDDER_WORKER_SKIP_AGENT_UPDATE:-0}" != "1" ] && command -v claude >/dev/null 2>&1; then
  echo "rudder-worker: checking for a Claude Code update ($(claude --version 2>/dev/null || echo unknown))" >&2
  if ! timeout 90 claude update >&2; then
    echo "rudder-worker: claude update did not complete; continuing with $(claude --version 2>/dev/null || echo unknown)" >&2
  fi
  # Codex refreshes in the background: it is not the default cloud backend, so
  # the first session need not wait on npm.
  (timeout 300 npm install -g @openai/codex@latest >/dev/null 2>&1 || true) &
fi

exec node /opt/rudder-worker/supervisor.mjs "$@"
