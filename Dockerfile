# EchoAgentCore — agent core service image.
#
# 容器化部署（2026-09-24）：
# - 典型用法见 `npm/echo-agent`（npm 编排）或仓库根目录的 `docker-compose.yml`；
# - 关键挂载：
#     /config  宿主目录绑定：core.toml + 会话/工作区 JSON（Core 会写这里）
#     /data    持久数据：媒体库（ECHO_MEDIA_DIR）、下载文件、NapCat 编排文件
# - 内置 docker CLI + compose 插件（可选挂 /var/run/docker.sock）：
#     取 QQ 登录二维码（docker exec 读容器内 PNG）、发送文件时的 docker cp 桥接、
#     以及把 NapCat 容器交给 Core 管理（napcat_auto_start=true）时使用。
#     不挂 socket 时自动降级：二维码走 NapCat WebUI、文件走本地 HTTP 桥。
#
# 构建（需在仓库根目录）：
#   docker build -t echo-agent-core:local .
#
# The Panel frontend is not part of this image; it lives in the
# EchoAgentPanel repository（同样有 Dockerfile）并以 WebSocket :3132 连接本服务。

FROM rust:1.98-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY source ./source
RUN cargo build --release --locked -p echo-agent-core

FROM debian:bookworm-slim

# docker CLI + compose 插件：容器内需要调用宿主 Docker（socket 挂载）——
# QR 取图、docker cp 文件桥、NapCat 容器生命周期管理都依赖它。
ARG TARGETARCH
ARG DOCKER_CLI_VERSION=27.3.1
ARG COMPOSE_VERSION=v2.29.7
# 国内网络构建时可用镜像源覆盖（默认官方源；CI 用默认即可）：
#   docker build \
#     --build-arg DOCKER_CLI_MIRROR=https://mirrors.aliyun.com/docker-ce \
#     --build-arg COMPOSE_MIRROR=https://ghfast.top/https://github.com/docker/compose/releases/download \
#     -t echo-agent-core:local .
ARG DOCKER_CLI_MIRROR=https://download.docker.com
ARG COMPOSE_MIRROR=https://github.com/docker/compose/releases/download
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete \
    && case "${TARGETARCH:-amd64}" in \
         amd64) arch=x86_64 ;; \
         arm64) arch=aarch64 ;; \
         *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "${DOCKER_CLI_MIRROR}/linux/static/stable/${arch}/docker-${DOCKER_CLI_VERSION}.tgz" \
         | tar xz -C /tmp \
    && mv /tmp/docker/docker /usr/local/bin/docker \
    && find /tmp/docker -mindepth 1 -delete \
    && mkdir -p /usr/local/lib/docker/cli-plugins \
    && curl -fsSL "${COMPOSE_MIRROR}/${COMPOSE_VERSION}/docker-compose-linux-${arch}" \
         -o /usr/local/lib/docker/cli-plugins/docker-compose \
    && chmod +x /usr/local/bin/docker /usr/local/lib/docker/cli-plugins/docker-compose

WORKDIR /app
COPY --from=builder /app/target/release/echo-agent-core /usr/local/bin/echo-agent-core
COPY config/echo-agent-core.toml /app/config/echo-agent-core.toml
COPY skills /app/skills
COPY docker/entrypoint.sh /usr/local/bin/echo-agent-entrypoint

# 运行期目录（宿主绑定；不挂载时数据留在容器内）：
#   /config  配置与会话 JSON
#   /data    媒体库 / 下载 / NapCat 编排
RUN mkdir -p /config /data && chmod +x /usr/local/bin/echo-agent-entrypoint
ENV ECHO_MEDIA_DIR=/data/media

# 3131: OneBot reverse-WS (NapCat connects here)
# 3132: management WS (Panels connect here)
EXPOSE 3131 3132

# 配置解析顺序：$ECHO_CONFIG → /config/core.toml → 镜像内置默认。
# 容器模式的完整配置模板见 npm/echo-agent/templates/core.toml。
ENTRYPOINT ["/usr/local/bin/echo-agent-entrypoint"]
