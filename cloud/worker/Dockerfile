FROM rust:1-bookworm AS native-builder
WORKDIR /src
COPY native ./native
# The binary embeds assets (the notifier icon) with include_bytes!.
COPY assets ./assets
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release --manifest-path native/Cargo.toml \
  && cp target/release/rudder-native /rudder-native \
  || cp native/target/release/rudder-native /rudder-native

FROM node:22-slim

ENV RUDDER_CLOUD_WORKER=1 \
    RUDDER_DISABLE_AUTO_UPDATE=1 \
    RUDDER_DISABLE_UPDATE_CHECK=1 \
    RUDDER_DISABLE_ONBOARD_INSTALL=1

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates curl git openssh-client tar gzip zsh \
    python3 build-essential \
  && rm -rf /var/lib/apt/lists/*

# GitHub CLI: clone-mode workspaces wire git credentials via `gh auth setup-git`
# using the vault-delivered ~/.config/gh, and agents get `gh pr create`.
RUN set -eux; \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg; \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends gh; \
    rm -rf /var/lib/apt/lists/*

# Agent CLIs are installed later, as the non-root user, into locations that
# user can write: Claude Code through its native installer (~/.local/bin, which
# `claude update` and its auto-updater can replace in place), the npm ones
# under an npm prefix in the user's home. The image used to `npm install -g`
# claude-code as root, so a worker could never actually update it: the
# auto-updater printed "Updated to latest" into ~/.local/share/claude while
# every launch kept running the root-owned npm copy the image was built with.

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
RUN RUDDER_SKIP_POSTINSTALL=1 npm ci --omit=dev
COPY dist ./dist
COPY assets ./assets
COPY --from=native-builder /rudder-native ./dist/native/rudder-native
RUN chmod +x ./dist/native/rudder-native && RUDDER_SKIP_POSTINSTALL=1 npm link

WORKDIR /opt/rudder-worker
COPY cloud/worker/package.json ./package.json
RUN npm install --omit=dev --no-audit --no-fund \
  && npm cache clean --force
COPY cloud/worker/supervisor.mjs ./supervisor.mjs
COPY cloud/worker/active-runs.mjs ./active-runs.mjs

COPY cloud/worker/entrypoint.sh /usr/local/bin/rudder-worker
RUN chmod +x /usr/local/bin/rudder-worker

# Claude refuses --permission-mode bypassPermissions as root, so we run the
# supervisor (and everything it spawns) as a non-root user.
RUN useradd --create-home --shell /bin/bash --uid 1500 rudder \
  && mkdir -p /workspace \
  && chown -R rudder:rudder /workspace /opt/rudder-worker

USER rudder
ENV NPM_CONFIG_PREFIX=/home/rudder/.npm-global \
    PATH=/home/rudder/.local/bin:/home/rudder/.npm-global/bin:${PATH}
RUN curl -fsSL https://claude.ai/install.sh | bash \
  && claude --version \
  && npm install -g @openai/codex@latest acpx@latest hunkdiff@latest \
  && npm cache clean --force \
  && codex --version
WORKDIR /workspace

ENTRYPOINT ["rudder-worker"]
