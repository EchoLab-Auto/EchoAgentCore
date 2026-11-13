#!/usr/bin/env bash
# =============================================================================
# 校验 echo-fabric-arch 版本一致性（跨仓）
#
# ## 防的是什么
#
# `echo-fabric-arch` 提供 `echo_module::Module` trait——Core 与**进程内模块**
# 通过它握手。两边拉到不同版本的 `echo-module` 时，同一个 trait 在不同 crate
# 里是两个不同的类型，编译直接失败（"expected `Module`, found `Module`"）。
#
# 历史上靠人工同步，已经踩过：`5ccd12e` / `96d8b8e` 都是在手工对齐 Panel 的
# pin。更隐蔽的形态是 tag 一致但 `Cargo.lock` 的 rev 不同（有人跑过
# `cargo update`）——那会在下一次全量构建时突然炸开。
#
# ## 谁要跟，谁不用跟
#
# | 仓 | 形态 | 要不要跟 |
# |---|---|---|
# | Core / Panel / subagent / workspace / prodoc | **进程内**（同一二进制） | **必须一致** |
# | adapter-qq | **进程外**（独立二进制 + stdio JSON 帧） | 不需要 |
#
# 进程外模块与宿主之间是**线协议**而不是 Rust trait：两边可以各自升级
# fabric-arch，只要 `PROTOCOL_VERSION` 相同就能通。把 adapter-qq 也纳入门禁
# 只会制造无意义的强制升级。
#
# ## 用法
#
#   scripts/check-fabric-arch-sync.sh            # 自动发现同级/常见路径的仓
#   scripts/check-fabric-arch-sync.sh --quiet    # 只在失败时输出（自更新用）
#
# 退出码：0 = 一致；1 = 漂移。
# =============================================================================
set -uo pipefail

QUIET=0
if [[ "${1:-}" == "--quiet" ]]; then QUIET=1; fi

say() {
    if ((QUIET == 0)); then
        echo "$@"
    fi
}

CORE_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
PARENT=$(dirname -- "$CORE_DIR")
HOME_DIR=${HOME:-/root}

# 进程内模块仓（存在才纳入）。
IN_PROCESS=("$CORE_DIR")
for candidate in \
    "$HOME_DIR/.local/share/echo-agent-panel/source" \
    "$HOME_DIR/code_file/gitlab/echo-tech/agent-module-subagent" \
    "$HOME_DIR/code_file/gitlab/echo-tech/agent-module-workspace" \
    "$HOME_DIR/code_file/gitlab/echo-tech/agent-module-prodoc" \
    "$PARENT/agent-module-subagent" \
    "$PARENT/agent-module-workspace" \
    "$PARENT/agent-module-prodoc"; do
    if [[ -f "$candidate/Cargo.toml" ]]; then
        IN_PROCESS+=("$candidate")
    fi
done

# 进程外模块：只报版本，不参与判定。
OUT_OF_PROCESS=()
for candidate in \
    "$HOME_DIR/code_file/gitlab/echo-tech/agent-module-adapter-qq" \
    "$PARENT/agent-module-adapter-qq"; do
    if [[ -f "$candidate/Cargo.toml" ]]; then
        OUT_OF_PROCESS+=("$candidate")
    fi
done

# 该仓声明的 fabric-arch tag（可能多个，去重）。
tags_of() {
    local dir=$1
    local files=("$dir/Cargo.toml")
    local f
    for f in "$dir"/source/*/Cargo.toml "$dir"/crates/*/Cargo.toml; do
        if [[ -f "$f" ]]; then files+=("$f"); fi
    done
    grep -ho 'echo-fabric-arch", *tag *= *"[^"]*"' "${files[@]}" 2>/dev/null \
        | sed 's/.*tag *= *"//; s/"$//' | sort -u | tr '\n' ' '
}

# Cargo.lock 里真正锁定的 rev（比 tag 更真实：tag 可以被移动）。
rev_of() {
    local lock=$1/Cargo.lock
    if [[ ! -f "$lock" ]]; then
        echo ""
        return 0
    fi
    grep -A 3 '^name = "echo-module"$' "$lock" 2>/dev/null \
        | grep '^source = ' | head -1 \
        | sed 's/.*#//; s/"$//'
}

declare -A tag_seen=()
declare -A rev_seen=()
FAIL=0

say "== 进程内模块（必须同一版本）=="
for dir in "${IN_PROCESS[@]}"; do
    # Core 与 Panel 的目录名都叫 `source`——带上父目录才分得清。
    name="$(basename -- "$(dirname -- "$dir")")/$(basename -- "$dir")"
    tags=$(tags_of "$dir")
    rev=$(rev_of "$dir")
    printf '  %-30s tag=%-10s rev=%s\n' "$name" "${tags:-<none>}" "${rev:-<none>}"
    if [[ -n "$tags" ]]; then
        for t in $tags; do tag_seen["$t"]=1; done
    fi
    if [[ -n "$rev" ]]; then
        rev_seen["$rev"]=1
    fi
done

if (( ${#tag_seen[@]} > 1 )); then
    echo "✗ fabric-arch tag 漂移：${!tag_seen[*]}" >&2
    echo "  进程内模块会编进同一个二进制，Module trait 版本必须一致。" >&2
    FAIL=1
fi
if (( ${#rev_seen[@]} > 1 )); then
    echo "✗ fabric-arch lock rev 漂移：${!rev_seen[*]}" >&2
    echo "  tag 相同但锁到不同提交（可能有人跑过 cargo update）。" >&2
    FAIL=1
fi

if (( ${#OUT_OF_PROCESS[@]} > 0 )); then
    say ""
    say "== 进程外模块（只受 PROTOCOL_VERSION 约束，不参与判定）=="
    for dir in "${OUT_OF_PROCESS[@]}"; do
        name=$(basename -- "$dir")
        tags=$(tags_of "$dir")
        # PROTOCOL_VERSION 定义在 echo-module-api（框架仓），不在模块自己的源码
        # 里；这里报它依赖的 fabric-arch tag，实际协议版本由框架决定。
        api_ver=$(grep -A 2 '^name = "echo-module-api"$' "$dir/Cargo.lock" 2>/dev/null \
            | grep '^version' | head -1 | sed 's/.*"\(.*\)"/\1/')
        printf '  %-30s tag=%-10s echo-module-api=%s\n' "$name" "${tags:-<none>}" "${api_ver:-<none>}"
    done
fi

say ""
if ((FAIL)); then
    echo "→ 修法：把漂移的仓对齐到同一 tag，各自 cargo update 后提交 Cargo.lock。" >&2
    exit 1
fi
say "✓ 一致"
