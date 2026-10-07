#!/usr/bin/env bash
# 重装（2026-10）：uninstall（保留配置与用户数据）→ install——二进制/服务单元
# 全部换新，core.toml、历史会话、媒体库（media/）与用户技能层（skills/）不动。
# 用于升级到含新默认值的安装
# （如 management_address 0.0.0.0 / 自动 token），配置不重写——需要
# 新配置字段时自行编辑或先 uninstall --purge。
# 用法（与 install/uninstall 相同的 curl|bash 兼容）：
#   ./scripts/reinstall.sh [--no-start] [--no-deps]
#   curl -fsSL <raw-url>/scripts/reinstall.sh | bash
set -Eeuo pipefail

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)

# curl|bash bootstrap：非 git 检出环境浅克隆接力（与 install.sh 同款）。
DEFAULT_REPO="https://github.com/EchoLab-Auto/EchoAgentCore.git"
if [[ ! -d "$PROJECT_ROOT/.git" ]] && [[ -z "${ECHO_BOOTSTRAPPED:-}" ]]; then
    echo "==> curl|bash bootstrap: cloning repository for reinstall"
    if ! command -v git >/dev/null 2>&1; then
        echo "error: bootstrap 需要 git" >&2
        exit 1
    fi
    BOOTSTRAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/echo-agent-reinstall.XXXXXX")
    trap 'rm -rf "$BOOTSTRAP_DIR"' EXIT
    git clone --depth 1 "${ECHO_REPOSITORY_URL:-$DEFAULT_REPO}" "$BOOTSTRAP_DIR/repo"
    export ECHO_BOOTSTRAPPED=1
    exec bash "$BOOTSTRAP_DIR/repo/scripts/reinstall.sh" "$@"
fi

echo "==> Step 1/2: uninstall (keep config)"
bash "$PROJECT_ROOT/scripts/uninstall.sh"

echo "==> Step 2/2: install"
exec bash "$PROJECT_ROOT/scripts/install.sh" "$@"
