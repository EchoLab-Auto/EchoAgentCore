# EchoAgentCore — agent core service image.
# The Panel (TUI) frontend is not part of this image; it lives in the
# EchoAgentPanel repository and connects to this service over WebSocket :3132.

FROM rust:1.92-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY source ./source
RUN cargo build --release --locked -p echo-agent-core

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/echo-agent-core /usr/local/bin/echo-agent-core
COPY config/echo-agent-core.toml /app/config/echo-agent-core.toml
COPY skills /app/skills

# 3131: OneBot reverse-WS (NapCat connects here)
# 3132: management WS (Panels connect here)
EXPOSE 3131 3132

ENTRYPOINT ["echo-agent-core", "--config", "/app/config/echo-agent-core.toml"]
