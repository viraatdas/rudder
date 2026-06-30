FROM rust:1-bookworm AS native-builder
WORKDIR /src
COPY native ./native
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release --manifest-path native/Cargo.toml \
  && cp target/release/rudder-native /rudder-native \
  || cp native/target/release/rudder-native /rudder-native

FROM node:22-slim AS cli-builder
WORKDIR /src
COPY package.json package-lock.json tsconfig.json ./
COPY scripts ./scripts
RUN RUDDER_SKIP_POSTINSTALL=1 npm ci --no-audit --no-fund
COPY src ./src
# Build the TypeScript CLI and bundled board without requiring an untracked
# host dist/. The native binary comes from the target-platform Rust stage.
RUN npm run clean \
  && npx tsc -p tsconfig.json \
  && npm run build-board \
  && npm run chmod-cli

FROM node:22-slim

ENV RUDDER_CLOUD_WORKER=1 \
    RUDDER_DISABLE_AUTO_UPDATE=1 \
    RUDDER_DISABLE_UPDATE_CHECK=1 \
    RUDDER_DISABLE_ONBOARD_INSTALL=1

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates curl git openssh-client tar gzip zsh tini \
    python3 build-essential \
  && rm -rf /var/lib/apt/lists/*

ARG CLAUDE_CODE_VERSION=2.1.196
ARG CODEX_VERSION=0.142.4
ARG ACPX_VERSION=0.11.2
RUN npm install -g \
    "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    "@openai/codex@${CODEX_VERSION}" \
    "acpx@${ACPX_VERSION}" \
    --no-audit --no-fund \
  && npm cache clean --force

# jj (Jujutsu) is Rudder's required internal VCS substrate. node:22-slim ships no
# jj, and the postinstall auto-installer needs brew/cargo (absent here), so fetch
# the prebuilt static musl binary for the build's target arch directly.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) JJ_ARCH=x86_64-unknown-linux-musl ;; \
      arm64) JJ_ARCH=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}"; exit 1 ;; \
    esac; \
    JJ_VERSION=v0.40.0; \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-${JJ_ARCH}.tar.gz" -o /tmp/jj.tar.gz; \
    mkdir -p /tmp/jjx; \
    tar -xzf /tmp/jj.tar.gz -C /tmp/jjx; \
    mv /tmp/jjx/jj /usr/local/bin/jj; \
    chmod +x /usr/local/bin/jj; \
    rm -rf /tmp/jj.tar.gz /tmp/jjx; \
    jj --version

WORKDIR /opt/rudder
# Copy scripts BEFORE npm ci so the package's postinstall hook (scripts/postinstall.mjs)
# resolves; RUDDER_SKIP_POSTINSTALL makes it a no-op since jj is installed above.
COPY package.json package-lock.json ./
COPY scripts ./scripts
RUN RUDDER_SKIP_POSTINSTALL=1 npm ci --omit=dev --no-audit --no-fund
COPY --from=cli-builder /src/dist ./dist
COPY assets ./assets
COPY --from=native-builder /rudder-native ./dist/native/rudder-native
RUN chmod +x ./dist/native/rudder-native && RUDDER_SKIP_POSTINSTALL=1 npm link

WORKDIR /opt/rudder-worker
COPY cloud/worker/package.json cloud/worker/package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund \
  && npm cache clean --force
COPY cloud/worker/supervisor.mjs ./supervisor.mjs
COPY cloud/worker/supervisor-lib.mjs ./supervisor-lib.mjs

COPY cloud/worker/entrypoint.sh /usr/local/bin/rudder-worker
RUN chmod +x /usr/local/bin/rudder-worker

# The supervisor stays root so its worker token is not readable through
# /proc/$PPID/environ by the coding agent. supervisor.mjs drops only the PTY
# child to this uid/gid; Claude therefore still runs non-root.
RUN useradd --create-home --shell /bin/bash --uid 1500 rudder \
  && mkdir -p /workspace \
  && chmod 1777 /workspace

USER root
WORKDIR /workspace

ENTRYPOINT ["/usr/bin/tini", "--", "rudder-worker"]
