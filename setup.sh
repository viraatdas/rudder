#!/usr/bin/env bash
#
# Bootstrap a local development environment for @viraatdas/rudder.
# Checks prerequisites, installs dependencies, builds the TypeScript + Rust
# native binary, type-checks, and smoke-tests the built CLI.
#
# Safe to re-run: every step is idempotent.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

info()  { printf '[rudder setup] %s\n' "$*"; }
warn()  { printf '[rudder setup] WARN: %s\n' "$*" >&2; }
error() { printf '[rudder setup] ERROR: %s\n' "$*" >&2; exit 1; }

require_cmd() {
  local cmd="$1"; local hint="$2"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    error "$cmd not found. $hint"
  fi
}

# --- Prerequisites ----------------------------------------------------------

require_cmd git "Install from https://git-scm.com"
require_cmd node "Install Node >=20 from https://nodejs.org or via nvm: https://github.com/nvm-sh/nvm"
require_cmd npm "npm ships with Node.js; reinstall Node from https://nodejs.org if it's missing."
require_cmd cargo "Install the Rust toolchain via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

# Read the required Node major version from package.json so this stays in sync
# with the engines field (currently ">=20").
REQUIRED_NODE_MAJOR="$(
  node -e '
    const pkg = require("./package.json");
    const range = (pkg.engines && pkg.engines.node) || "";
    const m = range.match(/(\d+)/);
    if (!m) { process.exit(2); }
    process.stdout.write(m[1]);
  '
)" || error "Could not read engines.node from package.json."

CURRENT_NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
if [ "$CURRENT_NODE_MAJOR" -lt "$REQUIRED_NODE_MAJOR" ]; then
  error "Node.js >= ${REQUIRED_NODE_MAJOR} required. Current: $(node --version). Upgrade via https://nodejs.org or nvm."
fi

if ! command -v rustup >/dev/null 2>&1; then
  warn "rustup not found. cargo is present, so build will proceed, but if it fails try installing rustup: https://rustup.rs"
fi

# --- Install dependencies ---------------------------------------------------

if [ -f package-lock.json ]; then
  info "Running npm ci (lockfile detected)..."
  npm ci || error "npm ci failed."
else
  info "Running npm install (no lockfile)..."
  npm install || error "npm install failed."
fi

# --- Build (tsc + cargo + copy-native) --------------------------------------

info "Running npm run build..."
if ! npm run build; then
  warn "If cargo build failed, try: rustup update"
  error "npm run build failed. Check TypeScript and Rust compiler output above."
fi

# --- Type-check -------------------------------------------------------------

info "Running npm run check..."
npm run check || error "npm run check failed. See output above."

# --- Smoke test -------------------------------------------------------------
# Skip the npm-registry update probe so a fresh setup does not require network
# access just to verify the built CLI starts.

info "Smoke testing built CLI (node dist/index.js --version)..."
RUDDER_DISABLE_UPDATE_CHECK=1 node dist/index.js --version >/dev/null \
  || error "Smoke test failed: node dist/index.js --version did not exit cleanly."

# --- Done -------------------------------------------------------------------

info "Setup complete!"
info "  Try: node dist/index.js --help"
info "  Or:  npm link && rudder --help"
