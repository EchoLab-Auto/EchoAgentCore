#!/usr/bin/env bash
# 外置模块锁定表的读写工具——`packaging/external-modules.lock` 的唯一解析器。
#
# 谁在用：
#   - .github/workflows/release.yml（原生 tarball，正式入口）
#   - Dockerfile 的 `modules-official` 阶段（正式镜像）
#   - .github/workflows/docker.yml（把锁定摘要写进 OCI 标签，并记录镜像 digest）
#   - scripts/release/test-external-modules.sh（表本身的测试）
#
# 设计约束：
#   - 只用 bash + git（GitHub runner 与 rust:bookworm 都有），不引 jq/python；
#   - **正式路径的拉取一律按完整 SHA、匿名只读**：任何 tag / 分支名都不接受，
#     `git rev-parse HEAD` 必须等于表里的值，否则立即失败（fail-closed）；
#   - 打印到 stdout 的都是机器可读格式（key / 字段 / JSON / 汇总），给人看的话
#     走 stderr，免得污染 `$(...)`。
set -euo pipefail

# 表的默认位置：脚本位于 <repo>/scripts/release/，向上两级到仓库根。
_default_lock="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/packaging/external-modules.lock"
LOCK="${ECHO_EXTERNAL_MODULES_LOCK:-$_default_lock}"

die() {
  echo "error: $*" >&2
  exit 1
}

# 输出所有模块行（去掉注释与空行），字段顺序同文件内注释。
entries() {
  [[ -f "$LOCK" ]] || die "lock file not found: $LOCK"
  awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*schema=/ { next }
    NF == 0 { next }
    $1 != "module" { printf("error: unexpected line %d: %s\n", NR, $0) > "/dev/stderr"; exit 1 }
    NF != 6 { printf("error: line %d needs 6 fields (module key repo commit crate targets), got %d\n", NR, NF) > "/dev/stderr"; exit 1 }
    { print $2, $3, $4, $5, $6 }
  ' "$LOCK"
}

# 记录必须对应实际构建集合：不指定目标时才列整个锁定表。
entries_for() {
  local target="${1:-}"
  case "$target" in
    '') entries ;;
    image|tarball) entries | awk -v t="$target" 'index("," $5 ",", "," t ",")' ;;
    *) die "unknown release target: $target" ;;
  esac
}

# 单行查表：key -> 整行（找不到就空）。
line_of() {
  local key="$1"
  entries | awk -v k="$key" '$1 == k { print; exit }'
}

field_of() {
  local key="$1" field="$2" line
  line="$(line_of "$key")"
  [[ -n "$line" ]] || return 1
  case "$field" in
    key)     awk '{print $1}' <<<"$line" ;;
    repo)    awk '{print $2}' <<<"$line" ;;
    commit)  awk '{print $3}' <<<"$line" ;;
    crate)   awk '{print $4}' <<<"$line" ;;
    targets) awk '{print $5}' <<<"$line" ;;
    *) die "unknown field: $field" ;;
  esac
}

cmd_list_keys() {
  entries_for "${1:-}" | awk '{print $1}'
}

cmd_field() {
  [[ $# -eq 2 ]] || die "usage: field <key> <repo|commit|crate|targets>"
  local value
  value="$(field_of "$1" "$2")" || die "no such module in lock: $1"
  echo "$value"
}

# 表结构校验：正式入口在动手之前先跑这一遍，防止"看起来 pin 了其实没有"。
cmd_validate() {
  local count=0 key repo commit crate targets rows
  rows="$(entries)" || die "cannot parse lock file: $LOCK"
  [[ -n "$rows" ]] || die "lock file $LOCK has no module entries"
  local -A seen_key=() seen_crate=()
  while read -r key repo commit crate targets; do
    [[ "$key" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "bad key '$key' (want [a-z0-9_-]+)"
    [[ "$repo" =~ ^https://[^[:space:]]+$ ]] || die "module $key: repo must be a public https URL, got '$repo'"
    # 40 位小写 hex 才算锁定；tag / 分支名 / 短 SHA 一律拒绝（会漂移或不可追溯）。
    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] \
      || die "module $key: commit must be a full 40-hex SHA (not a tag/branch), got '$commit'"
    [[ -n "$crate" ]] || die "module $key: crate is empty"
    [[ -z "${seen_key[$key]:-}" ]] || die "duplicate module key: $key"
    [[ -z "${seen_crate[$crate]:-}" ]] || die "duplicate crate: $crate"
    seen_key[$key]=1 seen_crate[$crate]=1
    [[ "$targets" =~ ^(image|tarball)(,(image|tarball))*$ ]] \
      || die "module $key: targets must be image/tarball list, got '$targets'"
    count=$((count + 1))
  done <<<"$rows"
  # 兜底：任何残留的分支/标签 pin 都不许混进来。
  if entries | grep -Eq '(^|[[:space:]])(branch|tag)='; then
    die "lock file must pin commits, not branch=/tag= refs"
  fi
  echo "ok: $count module(s) pinned by full SHA in $LOCK"
}

# 按 SHA 严格拉取：匿名、浅克隆、只认一个提交。成功即证明该提交**公开可取**。
cmd_fetch() {
  [[ $# -eq 2 ]] || die "usage: fetch <key> <dir>"
  local key="$1" dir="$2" repo commit head
  repo="$(field_of "$key" repo)" || die "no such module in lock: $key"
  commit="$(field_of "$key" commit)" || die "no such module in lock: $key"
  # 空 credential.helper + 禁交互：拿不到就是拿不到，绝不弹凭据或挂起等待。
  export GIT_TERMINAL_PROMPT=0
  export GIT_ASKPASS=/bin/true
  rm -rf "$dir"
  mkdir -p "$dir"
  git -C "$dir" init -q
  git -C "$dir" remote add origin "$repo"
  # -c credential.helper= ：把凭据助手清空，匿名拉取；失败就是失败，不借用环境里的 token。
  git -C "$dir" -c credential.helper= fetch -q --depth 1 origin "$commit" \
    || die "module $key: commit $commit is not fetchable from $repo (公开可取?)"
  git -C "$dir" -c credential.helper= checkout -q --detach FETCH_HEAD
  head="$(git -C "$dir" rev-parse HEAD)"
  [[ "$head" == "$commit" ]] || die "module $key: fetched $head, lock says $commit"
  echo "fetched $key @ $commit" >&2
}

# 校验工作副本真的是锁定提交——防止 tag 被移动、或上游改了默认分支后悄悄拿到别的东西。
cmd_verify() {
  [[ $# -eq 2 ]] || die "usage: verify <key> <dir>"
  local key="$1" dir="$2" want got
  want="$(field_of "$key" commit)" || die "no such module in lock: $key"
  got="$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)"
  [[ "$got" == "$want" ]] || die "module $key: checkout is ${got:-<none>}, lock says $want"
  # `--locked` 要求该提交自带 Cargo.lock；缺了正式路径必然失败，提前说清楚。
  git -C "$dir" cat-file -e "${want}:Cargo.lock" 2>/dev/null \
    || die "module $key: commit $want has no Cargo.lock (--locked build impossible)"
  echo "verified $key @ $want" >&2
}

# 产物清单用的 JSON 数组（manifest.json 的 external_modules 字段）。
cmd_manifest_json() {
  local first=1 rows
  rows="$(entries_for "${1:-}")" || return 1
  printf '['
  while read -r key repo commit crate _targets; do
    [[ -n "$key" ]] || continue
    [[ $first -eq 1 ]] || printf ','
    first=0
    printf '\n    {"key": "%s", "crate": "%s", "repo": "%s", "commit": "%s"}' \
      "$key" "$crate" "$repo" "$commit"
  done <<<"$rows"
  # 结尾必须带换行：消费方用 heredoc 写 GITHUB_OUTPUT 时，缺换行会把结束符
  # 粘在 JSON 后面，整段输出的边界就没了。
  printf '\n  ]\n'
}

# OCI 标签用的单行摘要：key@sha,key@sha（全 SHA，不是 tag）。
cmd_oci_summary() {
  local out="" key _repo commit _crate _targets rows
  rows="$(entries_for "${1:-}")" || return 1
  while read -r key _repo commit _crate _targets; do
    [[ -n "$key" ]] || continue
    out+="${out:+,}${key}@${commit}"
  done <<<"$rows"
  echo "$out"
}

# 镜像内留痕文件的内容（无法查 registry 标签时也能在容器里看到真实来源）。
cmd_revisions() {
  local key repo commit crate _targets rows
  rows="$(entries_for "${1:-}")" || return 1
  while read -r key repo commit crate _targets; do
    [[ -n "$key" ]] || continue
    echo "${key} ${repo}@${commit} (crate ${crate})"
  done <<<"$rows"
}

usage() {
  cat >&2 <<'EOF'
usage: external-modules.sh <command> [args]
  validate                    校验锁定表结构（40 位 SHA / 唯一 key / https 仓）
  list-keys [target]          列出模块 key；给了 target（image|tarball）则只列进该产物的
  field <key> <name>          取字段：repo|commit|crate|targets
  fetch <key> <dir>           按完整 SHA 匿名浅拉取到 <dir>（证明公开可取）
  verify <key> <dir>          校验 <dir> 的 HEAD 就是锁定 SHA，且该提交带 Cargo.lock
  manifest-json [target]      产物清单用的 JSON 数组
  oci-summary [target]        OCI 标签摘要 key@sha,...
  revisions [target]          镜像内留痕（每行一个模块）
环境变量：ECHO_EXTERNAL_MODULES_LOCK 覆盖锁定表路径。
EOF
}

main() {
  local cmd="${1:-}"
  [[ $# -gt 0 ]] && shift
  case "$cmd" in
    validate)      cmd_validate "$@" ;;
    list-keys)     cmd_list_keys "$@" ;;
    field)         cmd_field "$@" ;;
    fetch)         cmd_fetch "$@" ;;
    verify)        cmd_verify "$@" ;;
    manifest-json) cmd_manifest_json "$@" ;;
    oci-summary)   cmd_oci_summary "$@" ;;
    revisions)     cmd_revisions "$@" ;;
    ""|-h|--help)  usage; [[ -n "$cmd" ]] || exit 2 ;;
    *)             usage; die "unknown command: $cmd" ;;
  esac
}

main "$@"
