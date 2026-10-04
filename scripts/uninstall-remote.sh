#!/usr/bin/env bash
# curl|bash 一键卸载（2026-10）：与 install.sh 的 bootstrap 同款——
# 非 git 检出环境（curl 管道）运行时，浅克隆仓库接力其中的
# uninstall.sh（完整流程：停服务 → 删单元/二进制/启动器/受管检出，
# 默认保留配置与会话历史；--purge 全删）。
#
# 用法：
#   curl -fsSL <raw-url>/scripts/uninstall-remote.sh | bash
#   curl -fsSL <raw-url>/scripts/uninstall-remote.sh | bash -s -- --purge
set -Eeuo pipefail

DEFAULT_REPO="https://github.com/EchoLab-Auto/EchoAgentCore.git"

# 已在 git 检出里（本地 scripts/ 下运行）：直接转交 uninstall.sh。
_SCRIPT_PATH="${BASH_SOURCE[0]:-${0:-}}"
_LOCAL_ROOT=$(CDPATH= cd -- "$(dirname -- "$_SCRIPT_PATH")/.." 2>/dev/null && pwd -P || true)
if [[ -n "$_LOCAL_ROOT" && -f "$_LOCAL_ROOT/scripts/uninstall.sh" ]]; then
    exec bash "$_LOCAL_ROOT/scripts/uninstall.sh" "$@"
fi

if ! command -v git >/dev/null 2>&1; then
    echo "error: bootstrap 需要 git（请先安装 git）" >&2
    exit 1
fi

BOOTSTRAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/echo-agent-uninstall.XXXXXX")
trap 'rm -rf "$BOOTSTRAP_DIR"' EXIT
git clone --depth 1 "${ECHO_REPOSITORY_URL:-$DEFAULT_REPO}" "$BOOTSTRAP_DIR/repo"
exec bash "$BOOTSTRAP_DIR/repo/scripts/uninstall.sh" "$@"
