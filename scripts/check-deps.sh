#!/usr/bin/env bash
# Dependency-direction lint —— 依赖方向的 CI 门禁（纯 bash，无需编译）。
#
# 白名单的**单一真值来源**是 `scripts/deps-policy.txt`；同一份文件也被
# `source/core/tests/plugin_dependency_direction.rs` 读取。这意味着不存在
# 「CI 的清单和 cargo test 的清单各说一套」的可能——此前正是如此：本脚本
# 硬编码 14 个 crate，Phase 3 新增的 7 个一个都没检查，脚本报 OK 而实际
# 越界无人发现。
#
# 校验三件事：
#   1. 每个 crate 的 echo-* 依赖（含 dev-deps）必须落在那份白名单内；
#   2. `echo-agent`（业务 crate）只允许 `core`（组合根）依赖；
#      `echo-agent-core` 是二进制名，任何 crate 都不许依赖；
#   3. 发现未登记的 crate 即失败（fail-closed）——新增 crate 忘登记不会
#      静默放行。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
POLICY="$ROOT/scripts/deps-policy.txt"
SRC="$ROOT/source"

[ -f "$POLICY" ] || { echo "missing policy file: $POLICY" >&2; exit 1; }

FAIL=0
fail() { echo "$*" >&2; FAIL=1; }

# 某 manifest 的 echo-* 依赖（[dependencies] 与 [dev-dependencies] 都算）。
# 只用行首键名，避免把注释或 bin name 误算进去。
deps_of() {
  awk '
    /^\[/ { in_deps = ($0 ~ /^\[(dev-)?dependencies\]/); next }
    in_deps && /^echo-/ {
      # 键名取行首 `echo-<name>`：`echo-defs.workspace = true` 也算 echo-defs。
      match($0, /^echo-[a-z0-9-]+/); print substr($0, RSTART, RLENGTH)
    }
  ' "$1" | sort -u
}

# ── 1. 逐条比对白名单 ────────────────────────────────────────────────────
declare -A declared=()

while read -r crate allowed rest; do
  case "$crate" in ''|\#*) continue ;; esac
  manifest="$SRC/$crate/Cargo.toml"
  if [ ! -f "$manifest" ]; then
    fail "VIOLATION: policy lists '$crate' but $manifest does not exist"
    continue
  fi
  declared["$crate"]=1

  for dep in $(deps_of "$manifest"); do
    if ! echo " ${allowed} ${rest} " | grep -qw -- "$dep"; then
      fail "VIOLATION: $crate depends on $dep (allowed: ${allowed} ${rest})"
    fi
  done
done < "$POLICY"

# ── 2. 业务 crate 的依赖限制 ─────────────────────────────────────────────
while read -r crate _; do
  case "$crate" in ''|\#*) continue ;; esac
  manifest="$SRC/$crate/Cargo.toml"
  [ -f "$manifest" ] || continue
  for dep in $(deps_of "$manifest"); do
    if [ "$dep" = "echo-agent-core" ]; then
      fail "VIOLATION: $crate depends on echo-agent-core (binary crate, never a dependency)"
    fi
    if [ "$dep" = "echo-agent" ] && [ "$crate" != "core" ]; then
      fail "VIOLATION: $crate depends on echo-agent (business crate; only 'core' may)"
    fi
  done
done < "$POLICY"

# ── 3. 未登记的 crate（fail-closed）──────────────────────────────────────
for manifest in "$SRC"/*/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  dir="${manifest%/Cargo.toml}"
  crate="${dir#"$SRC"/}"
  [ -n "${declared[$crate]:-}" ] || fail "VIOLATION: crate '$crate' is not registered in scripts/deps-policy.txt"
done

if [ "$FAIL" -ne 0 ]; then
  echo "Dependency-direction lint FAILED" >&2
  exit 1
fi

echo "Dependency-direction lint OK ($(grep -cvE '^[[:space:]]*(#|$)' "$POLICY") crates)"
