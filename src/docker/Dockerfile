# syntax=docker/dockerfile:1
# The image every bot's container runs. dumBot builds it on this computer (so it matches
# the computer's architecture) and rebuilds it when anything below or its build args change.

ARG CLI_SOURCE=release

# dumbot's command line from this checkout (development builds).
FROM rust:1-bookworm AS cli-local
WORKDIR /src
# The pinned toolchain, in a layer of its own so it's installed once.
COPY rust-toolchain.toml .
RUN rustup show active-toolchain || rustup toolchain install
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --no-default-features \
    && cp target/release/dumbot /dumbot

# dumbot's command line from the GitHub release of the running app's version.
FROM ubuntu:24.04 AS cli-release
ARG DUMBOT_VERSION
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in arm64) arch=aarch64 ;; *) arch=x86_64 ;; esac \
    && curl -fsSL "https://github.com/vangheem/dumbot/releases/download/v${DUMBOT_VERSION}/dumbot-cli-linux-${arch}.tar.gz" \
    | tar -xz -C / dumbot

FROM cli-${CLI_SOURCE} AS cli

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash ca-certificates curl git jq less openssh-client python3 python3-venv ripgrep \
        socat tzdata unzip xz-utils zsh \
    && rm -rf /var/lib/apt/lists/*

# Optional tools; each bot opts in to the ones it gets configured.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# BROWSE_VERSION is the host's: a new one rebuilds the image, which installs the current
# stable release (the installer can't pin versions on every platform).
ARG BROWSE_VERSION
RUN if [ -n "$BROWSE_VERSION" ]; then \
        curl -fsSL https://opencode.ai/browse/install \
        | HOME=/tmp/browse XDG_BIN_HOME=/usr/local/bin sh -s -- --no-setup \
        && rm -rf /tmp/browse; \
    fi

ARG TARGETARCH
# Node.js (npx) and uv (uvx), which most MCP servers run with; bots install more in their home.
ARG NODE_MAJOR=22
RUN case "$TARGETARCH" in arm64) arch=arm64 ;; *) arch=x64 ;; esac \
    && v=$(curl -fsSL https://nodejs.org/dist/index.json \
        | jq -r "[.[] | select(.version | startswith(\"v$NODE_MAJOR.\"))][0].version") \
    && curl -fsSL "https://nodejs.org/dist/$v/node-$v-linux-$arch.tar.xz" \
        | tar -xJ -C /usr/local --strip-components=1 --exclude CHANGELOG.md --exclude README.md --exclude LICENSE \
    && curl -fsSL https://astral.sh/uv/install.sh \
        | env UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh \
    && node --version && uv --version

ARG OPENCODE_VERSION
ARG TARGETARCH
RUN case "$TARGETARCH" in arm64) arch=arm64 ;; *) arch=x64 ;; esac \
    && base=https://github.com/anomalyco/opencode/releases \
    && if [ -n "$OPENCODE_VERSION" ]; then url="$base/download/v$OPENCODE_VERSION"; else url="$base/latest/download"; fi \
    && curl -fsSL "$url/opencode-linux-$arch.tar.gz" | tar -xz -C /usr/local/bin opencode \
    && chmod 755 /usr/local/bin/opencode

COPY --from=cli /dumbot /usr/local/bin/dumbot

# opencode's shell for every command a bot runs: adds the bot's environment variables and
# hides their values in the output.
COPY --chmod=755 <<'EOF' /usr/local/bin/dumbot-shell
#!/bin/sh
exec /usr/local/bin/dumbot shell "$DUMBOT_BOT" "$@"
EOF

COPY --chmod=755 <<'EOF' /usr/local/bin/dumbot-entrypoint
#!/bin/sh
set -e
# Docker Desktop: the browse relay listens on the host's loopback; bring it to this one.
if [ -n "$DUMBOT_BROWSE_FORWARD" ]; then
    socat "TCP-LISTEN:$DUMBOT_BROWSE_FORWARD,bind=127.0.0.1,fork,reuseaddr" \
        "TCP:host.docker.internal:$DUMBOT_BROWSE_FORWARD" &
fi
mkdir -p "$HOME"
exec opencode serve --hostname "${DUMBOT_HOSTNAME:-0.0.0.0}" --port "${DUMBOT_PORT:-4096}"
EOF

ENTRYPOINT ["/usr/local/bin/dumbot-entrypoint"]
