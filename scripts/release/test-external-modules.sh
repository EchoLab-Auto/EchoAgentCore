#!/usr/bin/env bash
# 外置模块锁定机制的测试。
#
# 守的是三条容易悄悄退化的性质：
#   1. 表本身是锁（只认完整 40 位 SHA，不接受 tag / 分支名 / 短 SHA）；
#   2. 两个正式入口（release.yml 的 tarball、Dockerfile 的镜像）**都从这张表取**，
#      而且都走 helper 的 fetch/verify，不各自 `git clone ... main`；
#   3. 正式路径没有"失败了就退回不加锁"的兜底（那等于把版本漂移藏起来）。
#
# 用法：
#   ./scripts/release/test-external-modules.sh            # 离线：结构 + 接线 + 负例
#   ./scripts/release/test-external-modules.sh --remote   # 追加：SHA 公开可取 + 带 Cargo.lock
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER="$ROOT/scripts/release/external-modules.sh"
LOCK="$ROOT/packaging/external-modules.lock"
DOCKERFILE="$ROOT/Dockerfile"
RELEASE_YML="$ROOT/.github/workflows/release.yml"
DOCKER_YML="$ROOT/.github/workflows/docker.yml"

REMOTE=0
for arg in "$@"; do
  case "$arg" in
    --remote) REMOTE=1 ;;
    -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

pass=0 fail=0
ok() { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf '  FAIL %s\n' "$1" >&2; }
expect_ok() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi; }
expect_fail() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then bad "$name (should have failed)"; else ok "$name"; fi; }

# ── 1. 表结构 ────────────────────────────────────────────────────────────
echo "1) lock structure"
expect_ok "lock validates" bash "$HELPER" validate
expect_ok "tarball target non-empty" bash -c "test -n \"\$($HELPER list-keys tarball)\""
expect_ok "image target non-empty" bash -c "test -n \"\$($HELPER list-keys image)\""
# 最终镜像按固定路径 COPY 这两个模块（见 Dockerfile 末段），表里少了谁都装不出来。
expect_ok "image includes checklist" grep -qx checklist <<<"$(bash "$HELPER" list-keys image)"
expect_ok "image includes adapter-qq" grep -qx adapter-qq <<<"$(bash "$HELPER" list-keys image)"

# 每个模块都必须有 40 位 SHA（再独立核一遍，不只信 validate 的实现）。
for key in $(bash "$HELPER" list-keys); do
  commit="$(bash "$HELPER" field "$key" commit)"
  if [[ "$commit" =~ ^[0-9a-f]{40}$ ]]; then ok "commit is full SHA: $key"; else bad "commit is full SHA: $key ($commit)"; fi
done

# ── 2. 负例：这些写法必须被拒 ────────────────────────────────────────────
echo "2) negative cases"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

mutate() { # $1=sed 表达式，产出坏表
  sed "$1" "$LOCK" >"$tmp/bad.lock"
  echo "$tmp/bad.lock"
}
first_sha="$(awk '$1=="module" {print $4; exit}' "$LOCK")"
first_key="$(awk '$1=="module" {print $2; exit}' "$LOCK")"

bad_lock="$(mutate "s/$first_sha/main/")"
expect_fail "branch name rejected" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate

bad_lock="$(mutate "s/$first_sha/${first_sha:0:12}/")"
expect_fail "short SHA rejected" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate

bad_lock="$(mutate "s|https://github.com/EchoLab-Auto|git@github.com:EchoLab-Auto|")"
expect_fail "non-https repo rejected" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate

bad_lock="$(mutate "s/ image,tarball/ image,napcat/")"
expect_fail "unknown target rejected" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate

# 重复 key：把第一条模块行原样再追加一次
bad_lock="$tmp/dup.lock"
{ cat "$LOCK"; awk '$1 == "module" { print; exit }' "$LOCK"; } >"$bad_lock"
expect_fail "duplicate key rejected" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate

# Parser errors must reach the validated release command, even after valid rows.
bad_lock="$tmp/malformed.lock"
{ cat "$LOCK"; printf 'module malformed row\n'; } >"$bad_lock"
expect_fail "malformed row rejected after valid rows" env ECHO_EXTERNAL_MODULES_LOCK="$bad_lock" bash "$HELPER" validate
expect_fail "unknown target rejected by metadata" bash "$HELPER" manifest-json invalid
native_manifest="$(bash "$HELPER" manifest-json tarball)"
image_summary="$(bash "$HELPER" oci-summary image)"
expect_ok "native manifest records the packaged checklist" grep -q '"key": "checklist"' <<<"$native_manifest"
expect_fail "native manifest omits the image-only adapter" grep -q '"key": "adapter-qq"' <<<"$native_manifest"
expect_ok "image summary records the adapter" grep -q 'adapter-qq@' <<<"$image_summary"

# ── 3. 接线：两个正式入口都从这张表取 ────────────────────────────────────
echo "3) consumers wired to the lock"
expect_ok "Dockerfile references the lock" grep -q 'packaging/external-modules.lock' "$DOCKERFILE"
expect_ok "release.yml references the lock" grep -q 'packaging/external-modules.lock' "$RELEASE_YML"
expect_ok "Dockerfile uses the helper" grep -q 'scripts/release/external-modules.sh' "$DOCKERFILE"
expect_ok "release.yml uses the helper" grep -q 'scripts/release/external-modules.sh' "$RELEASE_YML"
expect_ok "release.yml does not clone a floating ref" bash -c "! grep -qE 'git clone( --depth 1)? .*(agent-module-(checklist|adapter-qq))' '$RELEASE_YML'"

# 正式阶段不许有 `--locked ... || 无锁重试`；开发阶段允许（明确隔离）。
official_stage="$(awk '/FROM .* AS modules-official/{f=1} /^FROM /{if(f && $0 !~ /modules-official/) exit} f' "$DOCKERFILE")"
dev_stage="$(awk '/FROM .* AS modules-dev/{f=1} /^FROM /{if(f && $0 !~ /modules-dev/) exit} f' "$DOCKERFILE")"
if [[ -z "$official_stage" ]]; then
  bad "Dockerfile has a modules-official stage"
else
  ok "Dockerfile has a modules-official stage"
  expect_ok "official stage builds --locked" grep -q -- '--release --locked' <<<"$official_stage"
  if grep -qE -- '--locked[^\n]*\|\|' <<<"$official_stage"; then
    bad "official stage has a no-lock fallback"
  else
    ok "official stage has no no-lock fallback"
  fi
  expect_ok "official stage verifies the checkout" grep -q 'verify' <<<"$official_stage"
  expect_ok "official stage fails on label mismatch" grep -q 'EXTERNAL_MODULES_SUMMARY' <<<"$official_stage"
fi
if [[ -n "$dev_stage" ]]; then ok "dev entry isolated in its own stage"; else bad "dev entry isolated in its own stage"; fi

# OCI 摘要必须是 SHA，不许把 tag 当身份。
summary="$(bash "$HELPER" oci-summary)"
if [[ "$summary" =~ @[0-9a-f]{40}(,|\$) ]]; then ok "oci summary carries full SHAs"; else bad "oci summary carries full SHAs ($summary)"; fi

# docker.yml 必须记录 digest（不是 tag）。
expect_ok "docker.yml records image digest" grep -q 'outputs.digest' "$DOCKER_YML"
expect_ok "docker.yml builds the official module set" grep -q 'MODULE_SOURCE=official' "$DOCKER_YML"
expect_ok "docker.yml passes the lock summary" grep -q 'EXTERNAL_MODULES_SUMMARY=' "$DOCKER_YML"
# 产物清单里要带模块提交，否则 tarball 无法回答"这一版用的是哪个模块提交"。
expect_ok "release.yml records external modules in manifest" grep -q '"external_modules"' "$RELEASE_YML"

# Dockerfile 的 dev 入口必须仍然存在且明确标注（旧行为保留，但隔离）。
expect_ok "dev entry documents it is not for release" grep -qi '不准\|不许发版\|UNLOCKED' "$DOCKERFILE"

# ── 4. 可选：网络侧证明（公开可取 + 可锁定构建）─────────────────────────
if [[ "$REMOTE" == 1 ]]; then
  echo "4) remote reachability (public fetch + Cargo.lock)"
  for key in $(bash "$HELPER" list-keys); do
    dir="$tmp/src-$key"
    if bash "$HELPER" fetch "$key" "$dir" 2>/dev/null && bash "$HELPER" verify "$key" "$dir" 2>/dev/null; then
      ok "$key: commit fetchable anonymously and has Cargo.lock"
    else
      bad "$key: commit not fetchable / no Cargo.lock"
    fi
  done
fi

echo
echo "passed $pass, failed $fail"
[[ "$fail" -eq 0 ]]
