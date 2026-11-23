#!/usr/bin/env bash
# 契约产物（B3）的自动测试：装配成功路径 + 错误 hash / 缺夹具 / 夹带文件 /
# SHA256SUMS 残缺 / 生成物为空 等失败路径。
#
# 用法：
#   bash scripts/tests/protocol-contract-artifact.test.sh                 # 真跑 cargo 生成（CI）
#   bash scripts/tests/protocol-contract-artifact.test.sh --ts-from F.ts   # 用预生成 TS（快，B2 未稳定时）
#   bash scripts/tests/protocol-contract-artifact.test.sh --allow-dirty-worktree
#
# 只用 bash + python3 + coreutils；不写 Core 源码，所有中间产物在 mktemp 目录里。
set -uo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
builder="$repo_root/scripts/build-protocol-contract.py"
verifier="$repo_root/scripts/verify-protocol-contract.py"
expected=${EXPECTED_TS_SHA256:-bbab13dbb65b1286a7396c545d58cf20e346a1b7f64381aa1b7cc7d13fc4f982}
ts_from=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ts-from) ts_from=$2; shift 2 ;;
        --ts-from=*) ts_from=${1#--ts-from=}; shift ;;
        --expected) expected=$2; shift 2 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) printf '未知参数：%s\n' "$1" >&2; exit 2 ;;
    esac
done

for tool in python3 sha256sum cmp; do
    command -v "$tool" >/dev/null 2>&1 || { printf '缺少工具：%s\n' "$tool" >&2; exit 2; }
done

work=$(mktemp -d "${TMPDIR:-/tmp}/protocol-contract-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
pass=0
fail=0

ok() { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf '  FAIL %s\n' "$1" >&2; }

# expect_fail <名称> <期望错误码> <命令...>
expect_fail() {
    local name=$1 code=$2
    shift 2
    local output
    if output=$("$@" 2>&1); then
        bad "$name（预期失败但成功了）"
        printf '%s\n' "$output" | sed 's/^/       /' >&2
        return
    fi
    if [[ "$code" == "-" ]] || printf '%s' "$output" | grep -q "\[$code\]"; then
        ok "$name → $code"
    else
        bad "$name（失败但错误码不是 $code）"
        printf '%s\n' "$output" | tail -5 | sed 's/^/       /' >&2
    fi
}

# ── 1. 组装成功路径 ────────────────────────────────────────────────────────
printf '== 组装 ==\n'
build_args=(--out "$work/dist" --allow-dirty --expect-ts-sha256 "$expected"
            --generated-at 2026-01-01T00:00:00Z --force)
if [[ -n "$ts_from" ]]; then
    build_args+=(--ts-from "$ts_from")
fi
if ! python3 "$builder" "${build_args[@]}" >"$work/build.log" 2>&1; then
    printf '组装失败：\n' >&2
    tail -20 "$work/build.log" >&2
    exit 1
fi
artifact=$(find "$work/dist" -maxdepth 1 -mindepth 1 -type d -name 'protocol-contract-*' | head -1)
if [[ -z "$artifact" ]]; then
    printf '没有找到产物目录\n' >&2
    exit 1
fi

if python3 "$verifier" "$artifact" --expect-ts-sha256 "$expected" --require-fixtures 8 >"$work/verify.log" 2>&1; then
    ok "产物通过独立核验（8 夹具，TS 摘要匹配）"
else
    bad "产物没通过核验"
    cat "$work/verify.log" >&2
fi

if [[ "$(sha256sum "$artifact/protocol.generated.ts" | cut -d' ' -f1)" == "$expected" ]]; then
    ok "TS 字节与 Panel 固定摘要一致"
else
    bad "TS 摘要与预期不符"
fi

if [[ "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["contract"]["fixtures"]["count"])' "$artifact/manifest.json")" == 8 ]]; then
    ok "manifest 记录 8 个夹具"
else
    bad "manifest 夹具计数不对"
fi

# 可复现：同样的输入 + 同样的 --generated-at 必须逐字节相同（不含机器绝对路径）
repro_args=()
for ((i = 0; i < ${#build_args[@]}; i++)); do
    case "${build_args[$i]}" in
        --out) repro_args+=(--out "$work/dist-repro"); i=$((i + 1)) ;;
        *) repro_args+=("${build_args[$i]}") ;;
    esac
done
if python3 "$builder" "${repro_args[@]}" >/dev/null 2>&1; then
    repro_artifact=$(find "$work/dist-repro" -maxdepth 1 -mindepth 1 -type d -name 'protocol-contract-*' | head -1)
    if cmp -s "$artifact/manifest.json" "$repro_artifact/manifest.json" &&
       cmp -s "$artifact/SHA256SUMS" "$repro_artifact/SHA256SUMS"; then
        ok "同输入重跑逐字节一致（manifest/SHA256SUMS 无机器路径）"
    else
        bad "重跑结果不一致（产物不可复现）"
    fi
else
    bad "第二次组装失败"
fi

# ── 2. 产物侧失败路径 ──────────────────────────────────────────────────────
printf '== 产物被破坏 ==\n'
case_no=0
mutate() { # mutate <名字> <期望错误码> <破坏命令>
    local name=$1 code=$2; shift 2
    local script=$1; shift
    case_no=$((case_no + 1))
    local copy="$work/case-$case_no"
    cp -a "$artifact" "$copy"
    ( cd "$copy" && eval "$script" )
    expect_fail "$name" "$code" python3 "$verifier" "$copy"
}

mutate "改一个字节（错 hash）" E_MANIFEST_HASH \
    'printf "// x\n" >> protocol.generated.ts'
mutate "删掉一个夹具（缺夹具）" E_FILE_SET \
    'rm fixtures/trunk_timeline_full_snapshot.json'
mutate "夹具内容被改" E_MANIFEST_HASH \
    'printf " " >> fixtures/module_invoke_op.json'
mutate "夹带未登记文件" E_FILE_SET \
    'printf "{}" > fixtures/extra.json'
mutate "SHA256SUMS 少一行" E_SUMS_MISMATCH \
    'grep -v "^[0-9a-f]\{64\}  fixtures/module_invoke_op.json$" SHA256SUMS > SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS'
mutate "删掉 TS" E_LAYOUT \
    'rm protocol.generated.ts'
mutate "manifest 聚合摘要被改" E_AGGREGATE \
    "python3 - <<'PY'
import hashlib, json
p = 'manifest.json'
m = json.load(open(p))
m['contract']['aggregate_sha256'] = '0' * 64
open(p, 'w').write(json.dumps(m, ensure_ascii=False, indent=2, sort_keys=True) + '\n')
# 同步 SHA256SUMS 里的 manifest 行，确保失败点落在「聚合摘要」而不是清单哈希
new = hashlib.sha256(open(p, 'rb').read()).hexdigest()
lines = open('SHA256SUMS').read().splitlines()
lines = [new + '  manifest.json' if line.endswith('  manifest.json') else line for line in lines]
open('SHA256SUMS', 'w').write('\n'.join(lines) + '\n')
PY"

# 预期摘要不符（产物自身完好，只是与调用方的钉子不一致）
expect_fail "验证器给错预期摘要" E_TS_DIGEST \
    python3 "$verifier" "$artifact" --expect-ts-sha256 "$(printf '0%.0s' $(seq 1 64))"
# 提交不符
expect_fail "验证器给错提交" E_SOURCE_COMMIT \
    python3 "$verifier" "$artifact" --expect-commit "$(printf '1%.0s' $(seq 1 40))"
# 夹具数要求过严
expect_fail "要求 9 个夹具（只有 8 个）" E_FIXTURE_MISSING \
    python3 "$verifier" "$artifact" --require-fixtures 9

# ── 3. 组装侧失败路径 ──────────────────────────────────────────────────────
printf '== 组装被拒绝 ==\n'
expect_fail "TS 摘要与预期不符时不产出" "-" \
    python3 "$builder" --out "$work/bad" --allow-dirty --expect-ts-sha256 "$(printf '0%.0s' $(seq 1 64))" \
    --ts-from "$artifact/protocol.generated.ts" --force
if [[ -d "$work/bad" ]] && find "$work/bad" -maxdepth 1 -name 'protocol-contract-*' | grep -q .; then
    bad "摘要不符时仍然写出了产物目录"
else
    ok "摘要不符时没有残留产物"
fi

expect_fail "工作树未加 --allow-dirty 时拒绝" "-" \
    python3 "$builder" --out "$work/bad2" --expect-ts-sha256 "$expected" \
    --ts-from "$artifact/protocol.generated.ts"

expect_fail "缺 --expect-ts-sha256 时拒绝盲产" "-" \
    python3 "$builder" --out "$work/bad3" --allow-dirty \
    --ts-from "$artifact/protocol.generated.ts"

# ── 4. 归档可搬运性：解包到全新目录（模拟 Panel 侧无源码核验）──────────────
printf '== 归档可搬运 ==\n'
tar -C "$(dirname "$artifact")" -czf "$work/contract.tar.gz" "$(basename "$artifact")"
mkdir -p "$work/unpacked"
tar -C "$work/unpacked" -xzf "$work/contract.tar.gz"
cp "$verifier" "$work/unpacked/verify-protocol-contract.py"
if ( cd "$work/unpacked" && python3 verify-protocol-contract.py "$(basename "$artifact")" \
        --expect-ts-sha256 "$expected" --require-fixtures 8 >"$work/unpacked.log" 2>&1 ); then
    ok "解包后仅凭产物 + 单文件验证器即可核验（不需要 Core 源码）"
else
    bad "解包后的独立核验失败"
    cat "$work/unpacked.log" >&2
fi

# 两个提交的版本和夹具不同。固定到旧提交时，归档不可混入当前检出。
printf '== 固定提交溯源 ==\n'
fixture_path=source/modules/agent-module-management/tests/fixtures
example_path=source/modules/echo-protocol/examples
historical="$work/source-history"
mkdir -p "$historical/$fixture_path" "$historical/$example_path" "$work/fake-bin"
git -C "$historical" init -q
printf '[workspace.package]\nversion = "1.0.0"\n' >"$historical/Cargo.toml"
touch "$historical/$example_path/protocol_contract_ts.rs"
cp "$artifact/fixtures/module_invoke_op.json" "$historical/$fixture_path/old.json"
git -C "$historical" add .
git -C "$historical" -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm old
old_commit=$(git -C "$historical" rev-parse HEAD)
printf '[workspace.package]\nversion = "2.0.0"\n' >"$historical/Cargo.toml"
mv "$historical/$fixture_path/old.json" "$historical/$fixture_path/new.json"
git -C "$historical" add -A
git -C "$historical" -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm new

# 组合验证只替换生成器进程：仍经 git archive 和正常的正式模式。
cat >"$work/fake-bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --version ]]; then printf 'cargo fixture\n'; exit; fi
[[ " $* " == *' --locked '* ]]
while [[ $# -gt 0 && $1 != --out ]]; do shift; done
cp "$ECHO_TEST_BINDING" "$2"
EOF
printf '#!/usr/bin/env bash\nprintf "rustc fixture\\n"\n' >"$work/fake-bin/rustc"
chmod +x "$work/fake-bin/cargo" "$work/fake-bin/rustc"
old_archive="$work/old/protocol-contract-1.0.0"
if PATH="$work/fake-bin:$PATH" ECHO_TEST_BINDING="$artifact/protocol.generated.ts" \
    python3 "$builder" --source "$historical" --commit "$old_commit" --out "$work/old" \
        --expect-ts-sha256 "$expected" >"$work/history.log" 2>&1 \
    && [[ -f "$old_archive/fixtures/old.json" && ! -e "$old_archive/fixtures/new.json" ]] \
    && python3 "$verifier" "$old_archive" --expect-commit "$old_commit" >>"$work/history.log" 2>&1; then
    ok '版本、生成器与夹具都取指定提交，不受当前检出影响'
else
    bad '固定提交归档混入了当前检出或无法核验'
    tail -10 "$work/history.log" >&2
fi

printf '\n== 结果 ==\n通过 %d，失败 %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
