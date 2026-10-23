ARG BASE_REGISTRY=
ARG RUST_VERSION=1.99
ARG NODE_VERSION=24
ARG CODEX_ACP_VERSION=2.1.1
ARG CLAUDE_AGENT_ACP_VERSION=0.88.0

FROM ${BASE_REGISTRY}rust:${RUST_VERSION}-bookworm AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src tests/support \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > tests/support/fake_agent.rs \
 && cargo build --release --bin lark-bot-agent \
 && rm -rf src tests

COPY src ./src
COPY tests ./tests
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --bin lark-bot-agent

FROM ${BASE_REGISTRY}node:${NODE_VERSION}-bookworm-slim
ARG CODEX_ACP_VERSION
ARG CLAUDE_AGENT_ACP_VERSION

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git ripgrep procps \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g --no-fund --no-audit \
      "@agentclientprotocol/codex-acp@${CODEX_ACP_VERSION}" \
      "@agentclientprotocol/claude-agent-acp@${CLAUDE_AGENT_ACP_VERSION}" \
 && npm cache clean --force \
 && test -x "$(command -v codex-acp)" \
 && test -x "$(command -v claude-agent-acp)"

COPY --from=builder /src/target/release/lark-bot-agent /usr/local/bin/lark-bot-agent

RUN mkdir -p /workspace
WORKDIR /workspace

ENV RUST_LOG=info \
    AGENT_CWD=/workspace \
    NO_BROWSER=1 \
    IS_SANDBOX=1

CMD ["lark-bot-agent"]
