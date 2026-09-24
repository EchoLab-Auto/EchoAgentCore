#!/bin/sh
# EchoAgentCore 容器入口：解析配置路径并启动。
#
# 优先级：$ECHO_CONFIG → /config/core.toml（compose 绑定挂载）→ 镜像内置默认。
# 这样：
#   - npm 编排的容器自然读到宿主编辑的 /config/core.toml；
#   - 裸 `docker run`（无挂载）也能用镜像自带配置起来做冒烟测试。
set -e

CONFIG="${ECHO_CONFIG:-/config/core.toml}"
if [ ! -f "$CONFIG" ]; then
  echo "[entrypoint] $CONFIG 不存在，回退镜像内置配置 /app/config/echo-agent-core.toml" >&2
  CONFIG=/app/config/echo-agent-core.toml
fi

# docker socket 可用性提示（可选依赖：缺失时 QR 取图与 docker cp 桥降级）。
if [ -S /var/run/docker.sock ]; then
  echo "[entrypoint] docker socket 可用：QR 取图 / 文件桥 / 容器管理走 Docker"
else
  echo "[entrypoint] 未挂载 /var/run/docker.sock：QQ 相关能力降级（QR 走 WebUI、文件走 HTTP 桥）"
fi

mkdir -p /data
exec echo-agent-core --config "$CONFIG"
