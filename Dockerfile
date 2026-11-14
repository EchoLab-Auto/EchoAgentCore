# EchoAgentCore — agent core service image.
#
# 容器化部署（2026-09-24）：
# - 典型用法见 `npm/echo-agent`（npm 编排）或仓库根目录的 `docker-compose.yml`；
# - 关键挂载：
#     /config  宿主目录绑定：core.toml + 会话/工作区 JSON（Core 会写这里）
#     /data    持久数据：媒体库（ECHO_MEDIA_DIR）、下载文件、NapCat 编排文件
# - **镜像自带 QQ 适配器模块二进制**（`echo-adapter-qq`，见下方 adapter-builder
#   阶段）：适配器自 2026-10 起是**进程外模块**，不再是 Core 的一部分——
#   宿主按平台名派生 `echo-adapter-<平台>` 去 PATH 里找它。镜像不装它，
#   容器里 QQ 就起不来（`failed to spawn module process "echo-adapter-qq"`），
#   而 compose 模板默认启用 QQ + 起 NapCat 服务。
# - 内置 docker CLI + compose 模块（可选挂 /var/run/docker.sock）：
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
# ── 构建期网络（不落进最终镜像）──
# Core 的 Cargo.toml 有几个 git 依赖（echo-fabric-arch 等）。受限网络下
# 容器直连 github 会失败，典型报错：
#   unable to update https://github.com/.../echo-fabric-arch?tag=v0.18.0
#   Caused by: SSL error: unknown error; class=Ssl (16)
# 这时传代理即可（宿主代理监听 127.0.0.1 时还要加 `--network=host`——
# 默认 bridge 网络里的 127.0.0.1 是容器自己）：
#   docker build --network=host \
#     --build-arg HTTPS_PROXY=http://127.0.0.1:7897 -t echo-agent-core:local .
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG ALL_PROXY
ARG NO_PROXY
# 代理只属于构建期：ENV 只写在这两个 builder 阶段，最终镜像不带。
# `git-fetch-with-cli`：让 cargo 用系统 git 拉取（libgit2 对代理/CA 的
# 处理比 git CLI 挑剔，实测同样环境 CLI 能过、libgit2 报 SSL class=Ssl）。
ENV HTTP_PROXY=${HTTP_PROXY} \
    HTTPS_PROXY=${HTTPS_PROXY} \
    ALL_PROXY=${ALL_PROXY} \
    NO_PROXY=${NO_PROXY} \
    CARGO_NET_GIT_FETCH_WITH_CLI=true
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY source ./source
RUN cargo build --release --locked -p echo-agent-core

# ── QQ 适配器（进程外模块，独立仓）──────────────────────────────────────
#
# 为什么单独一个阶段：适配器住在 `agent-module-adapter-qq` 仓（自己的
# workspace + Cargo.lock），与 Core 分开演进；它通过 stdio 协议帧与 Core
# 通信（4 字节 LE 长度前缀 + JSON，PROTOCOL_VERSION=1），所以镜像里只要有
# 一个能跑的二进制即可，不需要链接进 Core。
#
# 装成 `echo-adapter-qq` 而不是原包名 `agent-module-adapter-qq`：组合根按
# 平台名派生可执行名（见 `source/modules/core/src/config.rs` 的
# `ModuleLaunchSection`），装成派生名后**容器模板一行配置都不用加**；
# 想在 core.toml 里显式指定路径的，两条路径都指向同一个二进制。
#
# 拉取与版本：默认取适配器仓 `main` 的最新提交（与 Core 同源演进）。
# 需要可复现构建时用 ARG 固定到某个 commit / tag：
#   docker build --build-arg ADAPTER_REF=<sha> -t echo-agent-core:local .
FROM rust:1.98-bookworm AS adapter-builder
# 构建期代理（同 builder 阶段；只在构建阶段生效，不进最终镜像）。
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG ALL_PROXY
ARG NO_PROXY
ENV HTTP_PROXY=${HTTP_PROXY} \
    HTTPS_PROXY=${HTTPS_PROXY} \
    ALL_PROXY=${ALL_PROXY} \
    NO_PROXY=${NO_PROXY} \
    CARGO_NET_GIT_FETCH_WITH_CLI=true
ARG ADAPTER_REPO=https://github.com/EchoLab-Auto/agent-module-adapter-qq
ARG ADAPTER_REF=main
WORKDIR /adapter
RUN git clone --depth 1 --branch "${ADAPTER_REF}" "${ADAPTER_REPO}" . \
    || { git clone "${ADAPTER_REPO}" . && git checkout "${ADAPTER_REF}"; }
# --locked 依赖适配器仓自带的 Cargo.lock；它缺失或过期时退化为不加锁构建
# （镜像构建不该因为上游 lock 漂移而整体失败）。
RUN cargo build --release --locked -p agent-module-adapter-qq \
    || cargo build --release -p agent-module-adapter-qq

FROM debian:bookworm-slim

# docker CLI + compose 模块：容器内需要调用宿主 Docker（socket 挂载）——
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
# Debian 源镜像（默认官方）。国内网络构建常用：
#   --build-arg APT_MIRROR=mirrors.aliyun.com
# 只写主机名即可——**一律按 http 使用**：apt 装 ca-certificates 之前没有
# 可信任根证书，https 源会因证书校验失败而装不上它（鸡生蛋）。
ARG APT_MIRROR=
# ── 为什么处处强制 IPv4 ──
# 容器里的 DNS 往往同时返回 A 与 AAAA。宿主没有 IPv6 出网时，工具会先试
# AAAA 并**卡到超时**（实测本机：apt 连 deb.debian.org 报 connection timed
# out，而宿主机 curl 同一域名正常，因为宿主解析到的是 IPv4）。容器内的
# apt / curl 默认会优先 IPv6，所以这里统一显式指定 IPv4。
RUN if [ -n "${APT_MIRROR}" ]; then \
        host="$(echo "${APT_MIRROR}" | sed -E 's|^https?://||; s|/+$||')"; \
        [ -n "$host" ] || { echo "APT_MIRROR 只写主机名（如 mirrors.aliyun.com）" >&2; exit 1; }; \
        for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources; do \
            [ -f "$f" ] && sed -i -E "s|https?://deb\\.debian\\.org|http://${host}|g" "$f"; \
        done; \
    fi \
    && apt-get -o Acquire::ForceIPv4=true update \
    && apt-get -o Acquire::ForceIPv4=true install -y --no-install-recommends ca-certificates curl \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete \
    && case "${TARGETARCH:-amd64}" in \
         amd64) arch=x86_64 ;; \
         arm64) arch=aarch64 ;; \
         *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && curl -4 -fsSL "${DOCKER_CLI_MIRROR}/linux/static/stable/${arch}/docker-${DOCKER_CLI_VERSION}.tgz" \
         | tar xz -C /tmp \
    && mv /tmp/docker/docker /usr/local/bin/docker \
    && find /tmp/docker -mindepth 1 -delete \
    && mkdir -p /usr/local/lib/docker/cli-modules \
    && curl -4 -fsSL "${COMPOSE_MIRROR}/${COMPOSE_VERSION}/docker-compose-linux-${arch}" \
         -o /usr/local/lib/docker/cli-modules/docker-compose \
    && chmod +x /usr/local/bin/docker /usr/local/lib/docker/cli-modules/docker-compose

WORKDIR /app
COPY --from=builder /app/target/release/echo-agent-core /usr/local/bin/echo-agent-core
# QQ 适配器模块：默认派生名 + 原包名（两者同一份二进制，谁先被找到都一样）。
COPY --from=adapter-builder /adapter/target/release/agent-module-adapter-qq /usr/local/bin/echo-adapter-qq
RUN ln -sf /usr/local/bin/echo-adapter-qq /usr/local/bin/agent-module-adapter-qq
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
