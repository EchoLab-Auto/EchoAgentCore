#!/usr/bin/env bash
# 版本号 bump：**一处改，全仓生效**（2026-11）
#
# ## 为什么需要这个脚本
#
# 版本号此前只存在于 `Cargo.toml` 的 `[workspace.package] version`，与 release
# tag 完全没有关联。后果实测过：v0.2.0 的产物装上去，`echo-agent-core --version`
# 打印 `0.1.0`——用户第一反应是"装错了"。这不是显示问题：`--version`、联邦
# Hello 的 `version` 字段、面板的模块清单全都读 `CARGO_PKG_VERSION`，它们描述
# 的是"这个二进制是哪一版"，而那个数字必须与 tag 一致。
#
# 所以发版流程变成：`scripts/bump-version.sh 0.2.0` → 提交 → 打 tag `v0.2.0`。
# `release.yml` 会在构建前核对两者相等，不等直接失败（fail-closed，宁可没
# 产物也不要一个自称别版的东西）。
#
# ## 用法
#
#   scripts/bump-version.sh 0.2.0          # 改 Cargo.toml 并刷新 Cargo.lock
#   scripts/bump-version.sh 0.2.0 --check  # 只检查当前版本是否已是它（不写）
#   scripts/bump-version.sh --print        # 打印当前版本
#
# ## 范围
#
# 只改 Core workspace 的版本（`[workspace.package] version` + `Cargo.lock`）。
# 不碰：
#   - `npm/echo-agent/package.json`：Docker 编排 CLI，独立包、独立 `npm-v*`
#     tag 节奏，跟 Core 的二进制版本无关；
#   - `tools/echo-deploy/Cargo.toml`：独立 workspace 的部署工具；
#   - 各模块仓（panel / subagent / workspace / prodoc / checklist）：它们自己
#     的版本，Core 只按 Cargo.lock 里 pin 的 commit 取用。
set -Eeuo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
CARGO_TOML="$ROOT/Cargo.toml"
CARGO_LOCK="$ROOT/Cargo.lock"

usage() {
    cat <<'EOF'
Usage: scripts/bump-version.sh [<version>] [--check|--print]

  <version>   New version (e.g. 0.2.0 or v0.2.0; the leading v is stripped).
              Updates [workspace.package] version in Cargo.toml and refreshes
              Cargo.lock in the same commit.
  --check     Do not write; exit 0 if the current version already equals
              <version>, exit 1 otherwise. Used by release.yml before building.
  --print     Print the current version and exit.

Release flow:
  scripts/bump-version.sh 0.2.0
  git commit -am "chore(release): 0.2.0"
  git tag -a v0.2.0 -m "..." && git push origin main v0.2.0

The tag version must match Cargo.toml; release.yml verifies it and fails closed.
EOF
}

read_current() {
    # 只认 `[workspace.package]` 段里的那一行，避免误读 dependencies 的版本。
    awk '
        /^\[workspace\.package\]/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && /^version *=/ {
            gsub(/.*= *"|".*/, "")
            print
            exit
        }
    ' "$CARGO_TOML"
}

normalize() {
    local raw=$1
    raw=${raw#v}
    if [[ ! "$raw" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
        echo "error: not a semantic version: '$1' (want X.Y.Z, optional -pre/+build)" >&2
        exit 2
    fi
    printf '%s\n' "$raw"
}

mode=write
target=""
for argument in "$@"; do
    case "$argument" in
        --check) mode=check ;;
        --print) mode=print ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "error: unknown option: $argument" >&2; usage >&2; exit 2 ;;
        *)
            if [[ -n "$target" ]]; then
                echo "error: only one version may be given" >&2
                exit 2
            fi
            target=$argument
            ;;
    esac
done

current=$(read_current)
if [[ -z "$current" ]]; then
    echo "error: could not read [workspace.package] version from $CARGO_TOML" >&2
    exit 1
fi

case "$mode" in
    print)
        printf '%s\n' "$current"
        exit 0
        ;;
    check)
        if [[ -z "$target" ]]; then
            echo "error: --check needs a version to compare against" >&2
            exit 2
        fi
        wanted=$(normalize "$target")
        if [[ "$current" == "$wanted" ]]; then
            printf 'version check ok: %s\n' "$current"
            exit 0
        fi
        {
            echo "error: version mismatch"
            echo "       Cargo.toml [workspace.package] version = $current"
            echo "       requested/tag version                   = $wanted"
            echo "       fix: scripts/bump-version.sh $wanted && git commit -am 'chore(release): $wanted'"
        } >&2
        exit 1
        ;;
    write) ;;
esac

if [[ -z "$target" ]]; then
    usage >&2
    exit 2
fi
wanted=$(normalize "$target")

if [[ "$current" == "$wanted" ]]; then
    echo "==> Version already $wanted (Cargo.toml unchanged)"
    exit 0
fi

# 只替换 `[workspace.package]` 段内的 version 行：这是唯一真值，17 个成员
# crate 全部 `version.workspace = true` 继承它，改一处全仓生效。
awk -v wanted="$wanted" '
    /^\[workspace\.package\]/ { in_section = 1; print; next }
    /^\[/ { in_section = 0 }
    in_section && /^version *=/ && !done {
        printf "version = \"%s\"\n", wanted
        done = 1
        next
    }
    { print }
' "$CARGO_TOML" >"$CARGO_TOML.tmp.$$"
mv -f "$CARGO_TOML.tmp.$$" "$CARGO_TOML"
echo "==> Cargo.toml: $current -> $wanted"

# Cargo.lock 里每个工作区成员都有一条带版本的记录（25 条），必须一起更新，
# 否则 `cargo build --locked` 直接失败——CI 与发版都用 --locked，漏刷等于
# 让所有构建挂掉，而且报错是 "the lock file needs to be updated" 这种不指向
# 版本号的措辞。
if [[ -f "$CARGO_LOCK" ]]; then
    before=$(sha256sum "$CARGO_LOCK" | awk '{print $1}')
    # 只用 metadata：它解析整棵树并写回 lock，不编译任何东西。
    if ! cargo metadata --format-version 1 --offline >/dev/null 2>"$CARGO_LOCK.err.$$"; then
        # 离线失败（本地 cache 不全）时退回联网解析；仍失败就明确报错，
        # 不留下"Cargo.toml 改了、lock 没改"的半成品。
        if ! cargo metadata --format-version 1 >/dev/null; then
            echo "error: could not refresh Cargo.lock after the version bump" >&2
            sed -n '1,5p' "$CARGO_LOCK.err.$$" >&2 || true
            rm -f "$CARGO_LOCK.err.$$"
            exit 1
        fi
    fi
    rm -f "$CARGO_LOCK.err.$$"
    after=$(sha256sum "$CARGO_LOCK" | awk '{print $1}')
    if [[ "$before" == "$after" ]]; then
        echo "warning: Cargo.lock unchanged after the bump — check whether every" >&2
        echo "         workspace member inherits version.workspace = true" >&2
    else
        echo "==> Cargo.lock refreshed"
    fi
fi

# 自检：lock 里不应再有旧版本号残留（同一版本可能本来就属于某个第三方
# crate，出现在别的 crate 名下是正常的；这里只在所有成员都继承工作区版本时
# 严格校验一次）。
stale=$(grep -c "version = \"$current\"" "$CARGO_LOCK" 2>/dev/null || true)
if [[ "${stale:-0}" != "0" ]]; then
    echo "note: Cargo.lock still has $stale entr(y/ies) at $current (may be third-party)" >&2
fi

echo "==> Done. Next:"
echo "      git commit -am 'chore(release): $wanted'"
echo "      git tag -a v$wanted -m '...' && git push origin main v$wanted"
