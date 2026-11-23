# EchoAgentCore — agent core service image.
#
# 容器化部署（2026-09-24）：
# - 典型用法见 `npm/echo-agent`（npm 编排）或仓库根目录的 `docker-compose.yml`；
# - 关键挂载：
#     /config  宿主目录绑定：core.toml + 会话/工作区 JSON（Core 会写这里）
#     /data    持久数据：媒体库（ECHO_MEDIA_DIR）、下载文件、NapCat 编排文件
# - **镜像自带 QQ 适配器模块二进制**（`echo-adapter-qq`，见下方 modules-* 阶段）：适配器自 2026-10 起是**进程外模块**，不再是 Core 的一部分——
#   宿主按平台名派生 `echo-adapter-<平台>` 去 PATH 里找它。镜像不装它，
#   容器里 QQ 就起不来（`failed to spawn module process "echo-adapter-qq"`），
#   而 compose 模板默认启用 QQ + 起 NapCat 服务。
# - 内置 docker CLI + compose 模块（可选挂 /var/run/docker.sock）：
#     取 QQ 登录二维码（docker exec 读容器内 PNG）、发送文件时的 docker cp 桥接、
#     以及把 NapCat 容器交给 Core 管理（napcat_auto_start=true）时使用。
#     不挂 socket 时自动降级：二维码走 NapCat WebUI、文件走本地 HTTP 桥。
#
# 构建（需在仓库根目录）——**两条入口，语义不同**：
#
#   本地开发（默认）：
#     docker build -t echo-agent-core:local .
#     外置模块跟上游 main，允许无锁兜底。产物不可追溯，**不许发版**。
#
#   正式（与 native tarball 同一份锁定表 `packaging/external-modules.lock`）：
#     docker build \
#       --build-arg MODULE_SOURCE=official \
#       --build-arg EXTERNAL_MODULES_SUMMARY="$(bash scripts/release/external-modules.sh oci-summary image)" \
#       -t echo-agent-core:local .
#     模块按完整 SHA 拉取、`--locked` 构建、SHA 校验不过即失败；锁定摘要写进
#     OCI 标签与镜像内 /usr/local/share/echo-agent-core/external-modules.txt。
#     CI（.github/workflows/docker.yml）走的就是这条，tag 镜像只能从它产出。
#
# The Panel frontend is not part of this image; it lives in the
# EchoAgentPanel repository（同样有 Dockerfile）并以 WebSocket :3132 连接本服务。

# 外置模块来源：dev（本地开发，旧行为）/ official（正式，锁定表驱动）。
# 必须在第一个 FROM 之前声明——`FROM modules-${MODULE_SOURCE}` 要读它。
ARG MODULE_SOURCE=dev

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
# 只编 Core 本体。
#
# 任务清单（`echo-agent.checklist`）不在 Core 的依赖图里（根 `Cargo.toml`
# 刻意没有它），所以 `-p` 不可用——它的二进制由后面独立的 checklist 阶段
# 构建（同样是外部仓、独立 workspace）。镜像里装它但不启用：容器不写
# `modules.toml`，要用就在配置里加一行。
RUN cargo build --release --locked -p echo-agent-core

# ── 外置模块（独立仓、独立 workspace、独立 Cargo.lock）────────────────
#
# 两个模块——QQ 适配器（`agent-module-adapter-qq`）与任务清单
# （`agent-module-checklist`）——都不在 Core 的依赖图里（根 Cargo.toml 刻意
# 没有它们），所以镜像必须在这里单独克隆 + 构建。它们通过 stdio 协议帧与
# Core 通信（4 字节 LE 长度前缀 + JSON），镜像里只要有一个能跑的二进制即可。
#
# **两条互斥入口**：
#
#   modules-official  正式入口。提交从 `packaging/external-modules.lock` 取
#                     （完整 40 位 SHA），匿名 https 浅拉取，`cargo build
#                     --locked` 且**没有无锁兜底**，拉到的 HEAD 必须等于表里
#                     的 SHA，任何一步不符立即失败。构建完把锁定摘要写成镜像
#                     标签 + 镜像内文件，事后能从 registry / 容器里追到
#                     "这个镜像里是哪个模块提交"。
#
#   modules-dev       本地开发入口（旧行为）。`ADAPTER_REF` / `CHECKLIST_REF`
#                     默认 main，`--locked` 失败退化为无锁。跟上游当前主线
#                     联调很方便，但同一个 tag 重跑会得到不同内容，所以它
#                     **不产出可追溯的模块摘要**，也不属于发布流程。
#
# 为什么是两个 stage 而不是一个 stage 里 if/else：未选中的 stage 根本不会
# 执行，开发路径的"退回 main / 退回无锁"在正式构建里连跑的机会都没有——
# 隔离是结构性的，不依赖构建者记得传参数。选择在文件末尾的 `modules` 别名
# 阶段（`FROM modules-${MODULE_SOURCE}`）完成，非法值直接让 docker 报错。
#
# 装成 `echo-adapter-qq` 而不是原包名 `agent-module-adapter-qq`：组合根按
# 平台名派生可执行名（见 `source/modules/core/src/config.rs` 的
# `ModuleLaunchSection`），装成派生名后**容器模板一行配置都不用加**；想在
# core.toml 里显式指定路径的，两条路径都指向同一个二进制。
#
# 任务清单模块落点与源码/二进制安装一致：`<libexec>/modules/<包名>/<包名>`，
# 容器里 libexec 用 /usr/local/libexec（见 entrypoint 的 ECHO_PREFIX 约定）。
# 装进镜像 ≠ 默认启用：容器模板不写 modules.toml，要用就在配置里加一行：
#
#   [[module]]
#   id = "echo-agent.checklist"
#   kind = "stdio"
#   command = "/usr/local/libexec/echo-agent-core/modules/agent-module-checklist/agent-module-checklist"

# ── 开发入口：跟随上游 main（旧行为，仅供本地联调）──────────────────
FROM rust:1.98-bookworm AS modules-dev
# dash 不支持 `set -o pipefail`；这两个阶段的脚本都用 bash 写。
SHELL ["/bin/bash", "-c"]
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
ARG CHECKLIST_REPO=https://github.com/EchoLab-Auto/agent-module-checklist
ARG CHECKLIST_REF=main
WORKDIR /adapter
RUN git clone --depth 1 --branch "${ADAPTER_REF}" "${ADAPTER_REPO}" . \
    || { git clone "${ADAPTER_REPO}" . && git checkout "${ADAPTER_REF}"; }
# --locked 依赖对方自带的 Cargo.lock；缺失或过期时退化为不加锁构建
# （开发镜像不该因为上游 lock 漂移而整体失败——正式镜像不适用，见下）。
RUN cargo build --release --locked -p agent-module-adapter-qq \
    || cargo build --release -p agent-module-adapter-qq
WORKDIR /checklist
RUN git clone --depth 1 --branch "${CHECKLIST_REF}" "${CHECKLIST_REPO}" . \
    || { git clone "${CHECKLIST_REPO}" . && git checkout "${CHECKLIST_REF}"; }
RUN cargo build --release --locked -p agent-module-checklist \
    || cargo build --release -p agent-module-checklist
# 归一成正式阶段同样的落点，最终镜像只认这一组路径（两个阶段可互换）。
RUN set -euo pipefail \
    && mkdir -p /out/bin \
    && install -m 755 /adapter/target/release/agent-module-adapter-qq /out/bin/echo-adapter-qq \
    && install -m 755 /checklist/target/release/agent-module-checklist /out/bin/agent-module-checklist \
    && { echo "# 开发构建：外置模块跟随上游默认分支，**不是**锁定状态（无发布资格）"; \
         for d in /adapter /checklist; do \
           printf '%s %s@%s (UNLOCKED)\n' "$(basename "$d")" \
             "$(git -C "$d" config --get remote.origin.url)" "$(git -C "$d" rev-parse HEAD)"; \
         done; } > /out/revisions.txt

# ── 正式入口：锁定表驱动（与 native tarball 同一份表）────────────────
FROM rust:1.98-bookworm AS modules-official
# dash 不支持 `set -o pipefail`；这两个阶段的脚本都用 bash 写。
SHELL ["/bin/bash", "-c"]
ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG ALL_PROXY
ARG NO_PROXY
ENV HTTP_PROXY=${HTTP_PROXY} \
    HTTPS_PROXY=${HTTPS_PROXY} \
    ALL_PROXY=${ALL_PROXY} \
    NO_PROXY=${NO_PROXY} \
    CARGO_NET_GIT_FETCH_WITH_CLI=true
# 唯一真值：表 + 解析器。两者都来自本仓库，构建上下文里必须有。
COPY packaging/external-modules.lock /lock/external-modules.lock
COPY scripts/release/external-modules.sh /lock/external-modules.sh
# 镜像标签用的摘要，由构建方传入（CI 从 helper 取同一表达式）。
# 这里**不信任**它：下面会与锁定表的输出逐字比对，不一致就让构建失败——
# 否则标签会与镜像里的二进制对不上，比没有标签更糟。
ARG EXTERNAL_MODULES_SUMMARY=
WORKDIR /modules
RUN set -euo pipefail; \
    export ECHO_EXTERNAL_MODULES_LOCK=/lock/external-modules.lock; \
    script=/lock/external-modules.sh; \
    bash "$script" validate; \
    expected="$(bash "$script" oci-summary image)"; \
    if [ -z "${EXTERNAL_MODULES_SUMMARY}" ]; then \
      echo "error: 正式镜像必须传 --build-arg EXTERNAL_MODULES_SUMMARY=<摘要>" >&2; \
      echo "       取值：bash scripts/release/external-modules.sh oci-summary image" >&2; \
      echo "       （当前锁定表：${expected}）" >&2; \
      exit 1; \
    fi; \
    if [ "${EXTERNAL_MODULES_SUMMARY}" != "${expected}" ]; then \
      echo "error: EXTERNAL_MODULES_SUMMARY 与锁定表不一致" >&2; \
      echo "  build arg: ${EXTERNAL_MODULES_SUMMARY}" >&2; \
      echo "  lock     : ${expected}" >&2; \
      exit 1; \
    fi; \
    mkdir -p /out/bin; \
    for key in $(bash "$script" list-keys image); do \
      crate="$(bash "$script" field "$key" crate)"; \
      bash "$script" fetch "$key" "/modules/src-$key"; \
      bash "$script" verify "$key" "/modules/src-$key"; \
      cargo build --release --locked \
        --manifest-path "/modules/src-$key/Cargo.toml" \
        -p "$crate" --target-dir "/modules/target-$key"; \
      test -x "/modules/target-$key/release/$crate"; \
      install -m 755 "/modules/target-$key/release/$crate" "/out/bin/$crate"; \
    done; \
    install -m 755 /out/bin/agent-module-adapter-qq /out/bin/echo-adapter-qq; \
    bash "$script" revisions image > /out/revisions.txt; \
    cat /out/revisions.txt

# ── 选择器：dev / official 二选一（非法值由 docker 报错）────────────
FROM modules-${MODULE_SOURCE} AS modules

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
# 外置模块：来源由 `MODULE_SOURCE` 决定（official=锁定表 / dev=上游 main），
# 但落点固定——两个 stage 都归一成 /out/bin + /out/revisions.txt，所以下面的
# COPY 对两条路径完全一样，正式镜像与开发镜像的目录结构不会漂移。
#
# QQ 适配器：默认派生名 + 原包名（两者同一份二进制，谁先被找到都一样）。
COPY --from=modules /out/bin/echo-adapter-qq /usr/local/bin/echo-adapter-qq
RUN ln -sf /usr/local/bin/echo-adapter-qq /usr/local/bin/agent-module-adapter-qq
# 任务清单模块：**独立产物**（不是编进 echo-agent-core 的那份）。
# 落点与源码/二进制安装一致：`<libexec>/modules/<包名>/<包名>`。
# 容器里 libexec 用 /usr/local/libexec（见 entrypoint 的 ECHO_PREFIX 约定）。
COPY --from=modules /out/bin/agent-module-checklist \
     /usr/local/libexec/echo-agent-core/modules/agent-module-checklist/agent-module-checklist
# 模块来源留痕：正式镜像里是 `key repo@<40 位 SHA>`；开发镜像是 UNLOCKED。
# 与下面的 OCI 标签同源（标签给人看 registry，这个文件给进不去 registry 的人）。
COPY --from=modules /out/revisions.txt /usr/local/share/echo-agent-core/external-modules.txt
COPY config/echo-agent-core.toml /app/config/echo-agent-core.toml
COPY skills /app/skills
COPY docker/entrypoint.sh /usr/local/bin/echo-agent-entrypoint

# 全局 ARG 在 FROM 之后必须重新声明才能在指令里使用（docker 的 ARG 作用域规则）。
ARG MODULE_SOURCE=dev
ARG EXTERNAL_MODULES_SUMMARY=unlocked-dev
# OCI 可追溯元数据：**摘要里是完整 SHA，不是 tag**。dev 构建显式写
# `unlocked-dev`，避免把开发镜像误当成"有锁定来源"的产物。
# 正式构建由 modules-official 阶段核对过：这个值与模块实际提交逐字一致。
LABEL io.echo.external-modules.lock="packaging/external-modules.lock" \
      io.echo.external-modules.revisions="${EXTERNAL_MODULES_SUMMARY}" \
      io.echo.external-modules.source="${MODULE_SOURCE}"

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
