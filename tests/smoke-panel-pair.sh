#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# tests/smoke-panel-pair.sh — Core ↔ Panel 组合验收 smoke（A4/B4 本地演练）
#
# ## 目标与验收
#
#      Panel 固定的协议生成物自洽（`web/package.json` 的 `protocolCore`
#      与 `web/src/protocol.generated.ts` 逐字一致）。
#   ① 配对静态证据（不构建、不联网）
#      只核对“固定记录自洽 + 提交级静态协议来源未变”；它**不**证明 Core 生成的
#      绑定逐字等价——那必须跑 `--stage generate` 实际生成后比对。
#   ② Core 侧协议行为（真实回环 listener）
#      管理面 WS 在独立形态（`accept_standalone_ws`）与嵌入形态（合并端口 +
#      令牌分流 + Panel 身份分级）下的集成测试：命令送达、事件扇出、增量
#      合批保序、坏帧回显式错误、未认证 401 语义。
#   ③ Panel 侧代理行为（独立外壳）
#      `/ws` 中继双向、断线/半死链清理、晚连补投、静态资源与子路径形态。
#
# 验收 = 退出码 0 且末尾汇总没有 FAIL（SKIP 必须带原因）。WARN 不阻塞，
# 但要在结论里引用。
#
# ## 事实与假设（写作前核对过）
#
# - 独立形态 = 管理面独立监听：`agent-module-management/src/lib.rs`
#   （`accept_standalone_ws` / `serve_management_ws`）。
# - 嵌入形态有两层含义，这里都覆盖：
#   a) Core 内嵌 Panel 服务（进程内模块 `echo-agent.panel.web`，实现住在
#      Panel 仓 `source/agent-module-panel/`，与独立外壳共用 echo-web-server
#      运行库）—— 由 e2e 阶段验证；
#   b) 管理面合并端口（联邦与控制面共用一个监听，按令牌分流）—— 由 core
#      阶段的 `merged_*` 测试验证。
# - 配对记录：Panel `web/package.json` 的 `protocolCore.commit` /
#   `protocolCore.generatedSha256`；比较器语义与 `scripts/check-panel-protocol.sh`
#   一致（本脚本**不修改也不调用**它，只做同一件事的更早一层）。
# - **不假设并行任务的文件稳定**：`Cargo.toml` / `Cargo.lock`（B2）与
#   `.github/workflows/release.yml` / `Dockerfile`（A2）只读、不写；也绝不
#   使用 Core 根 `target/`（B2 可能在跑），构建一律落在隔离 target。
#
# ## 改动清单
#
# - 仅本文件（新增）。不修改任何已存在文件，不提交、不安装、不发版。
#
# ## 风险与回退
#
# - 风险：隔离 target 的首次构建较慢 → 一律后台运行 + 超时，超时记 SKIP
#   而不是 FAIL（不阻塞 B2）。
# - 风险：e2e 用**预发布既有二进制**做自测，只证明"这一对二进制能互通"，
#   不能作为最终结论 → 输出里逐行标注 `[预构建自测]`。
# - 回退：删除本文件即可；脚本不在工作树里留任何产物（全部落在 mktemp 目录，
#   默认退出即删）。
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DEFAULT_CORE=$(cd -- "$SCRIPT_DIR/.." && pwd)

CORE=${CORE_SOURCE:-$DEFAULT_CORE}
PANEL=${PANEL_SOURCE:-$HOME/.local/share/echo-agent-panel/source}
EXPECT_REV=${PANEL_REV:-}
CORE_PIN=${CORE_PIN:-}
STAGES=""
CORE_TARGET=${SMOKE_CORE_TARGET:-${TMPDIR:-/tmp}/smoke-panel-pair/core-target}
PANEL_TARGET=${SMOKE_PANEL_TARGET:-}
TIMEOUT_SECS=${SMOKE_TIMEOUT:-1800}
KEEP=${SMOKE_KEEP:-0}
CORE_BIN=${SMOKE_CORE_BIN:-}
PANEL_BIN=${SMOKE_PANEL_BIN:-}

FAILED=0
PASSED=0
SKIPPED=0
WARNED=0
declare -a SUMMARY=()

usage() {
    cat <<'EOF'
用法: bash tests/smoke-panel-pair.sh [选项]

  --stage <pair|core|panel|generate|e2e>   可重复；缺省 "pair core panel"；all = 前三个
  --rev <sha40>          期望的 Panel 候选提交（缺省只记录，不比对）
  --core-pin <sha40>     覆盖「Core 协议源基线提交」（缺省取 Panel 的 protocolCore.commit）
  --core <dir>           Core 源码目录（缺省 = 本脚本上一级，或 $CORE_SOURCE）
  --panel <dir>          Panel 源码目录（缺省 $PANEL_SOURCE 或 ~/.local/share/echo-agent-panel/source）
  --core-target-dir <d>  Core 的隔离 cargo target（**不允许**是 Core 仓的 target/）
  --panel-target-dir <d>Panel 的 cargo target（缺省 = Panel 仓自己的 target/）
  --timeout <secs>       单个构建/测试的超时（缺省 1800）
  --core-bin <path>      e2e 用的预构建 Core 二进制（缺省 <core>/target/release/echo-agent-core）
  --panel-bin <path>     e2e 用的预构建 Panel 二进制（缺省 <panel>/target/release/echo-agent-panel）
  --keep                 保留临时目录（排障用；里面只有 smoke 自己的配置与日志）
  -h, --help

安全边界（脚本强制）:
  * 只写 mktemp 目录；HOME/XDG_* 全部隔离到临时目录；
  * 只绑 127.0.0.1 + 随机端口；不配联邦 peer、不启平台实例、关闭自更新模块；
  * 不含任何真实帐号/凭据（令牌是现生成的随机值）；
  * 不重启、不安装、不连接任何现役实例；不写 Core 根 target、不改已存在文件。
EOF
}

while (($#)); do
    case $1 in
        --stage) STAGES="${STAGES:+$STAGES }${2:-}"; shift 2 ;;
        --stage=*) STAGES="${STAGES:+$STAGES }${1#*=}"; shift ;;
        --rev) EXPECT_REV=${2:-}; shift 2 ;;
        --rev=*) EXPECT_REV=${1#*=}; shift ;;
        --core-pin) CORE_PIN=${2:-}; shift 2 ;;
        --core-pin=*) CORE_PIN=${1#*=}; shift ;;
        --core) CORE=${2:-}; shift 2 ;;
        --panel) PANEL=${2:-}; shift 2 ;;
        --core-target-dir) CORE_TARGET=${2:-}; shift 2 ;;
        --panel-target-dir) PANEL_TARGET=${2:-}; shift 2 ;;
        --timeout) TIMEOUT_SECS=${2:-}; shift 2 ;;
        --core-bin) CORE_BIN=${2:-}; shift 2 ;;
        --panel-bin) PANEL_BIN=${2:-}; shift 2 ;;
        --keep) KEEP=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "未知参数: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[[ -n $STAGES ]] || STAGES="pair core panel"
[[ $STAGES == all ]] && STAGES="pair core panel"
[[ -n $PANEL_TARGET ]] || PANEL_TARGET="$PANEL/target"
[[ -n $CORE_BIN ]] || CORE_BIN="$CORE/target/release/echo-agent-core"
[[ -n $PANEL_BIN ]] || PANEL_BIN="$PANEL/target/release/echo-agent-panel"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/smoke-panel-pair.XXXXXX") || exit 1
E2E_DIR="$WORK/e2e"
cleanup() {
    local rc=$?
    command -v stop_pid >/dev/null 2>&1 && { stop_pid "$E2E_DIR/core.pid"; stop_pid "$E2E_DIR/panel.pid"; }
    if [[ $KEEP == 1 ]]; then
        printf '\n临时目录已保留: %s\n' "$WORK"
    else
        if (( FAILED > 0 )); then
            # 失败时把日志留档：否则排障只能靠重跑（下面 rm 会把证据一起删）。
            local dest="${TMPDIR:-/tmp}/smoke-panel-pair/last-failure"
            rm -rf -- "$dest"
            mkdir -p -- "$dest"
            cp -a -- "$WORK"/. "$dest"/ 2>/dev/null || true
            printf '\n失败现场已留档（日志/配置）: %s\n' "$dest"
        fi
        rm -rf -- "$WORK"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

# ── 结果记录 ────────────────────────────────────────────────────────────────

pass() { PASSED=$((PASSED + 1)); SUMMARY+=("PASS|$1|$2"); printf '  ✅ %s — %s\n' "$1" "$2"; }
fail() { FAILED=$((FAILED + 1)); SUMMARY+=("FAIL|$1|$2"); printf '  ❌ %s — %s\n' "$1" "$2"; }
skip() { SKIPPED=$((SKIPPED + 1)); SUMMARY+=("SKIP|$1|$2"); printf '  ⏭️  %s — %s\n' "$1" "$2"; }
warn() { WARNED=$((WARNED + 1)); SUMMARY+=("WARN|$1|$2"); printf '  ⚠️  %s — %s\n' "$1" "$2"; }

die() { printf '✗ %s\n' "$1" >&2; exit 2; }

# ── 小工具 ──────────────────────────────────────────────────────────────────

json_field() { # <json-file> <key> [<key>...]
    python3 - "$@" <<'PY' 2>/dev/null || true
import json, sys
path, keys = sys.argv[1], sys.argv[2:]
try:
    with open(path, encoding='utf-8') as fh:
        value = json.load(fh)
except Exception:
    print('')
    raise SystemExit(0)
for key in keys:
    if not isinstance(value, dict) or key not in value:
        print('')
        raise SystemExit(0)
    value = value[key]
print(value if isinstance(value, str) else '')
PY
}

json_field_stdin() { # <key> [<key>...]  （从 stdin 读 JSON）
    python3 - "$@" <<'PY' 2>/dev/null || true
import json, sys
keys = sys.argv[1:]
try:
    value = json.load(sys.stdin)
except Exception:
    print('')
    raise SystemExit(0)
for key in keys:
    if not isinstance(value, dict) or key not in value:
        print('')
        raise SystemExit(0)
    value = value[key]
print(value if isinstance(value, str) else '')
PY
}

sha256_file() { sha256sum -- "$1" | cut -d' ' -f1; }
sha256_stdin() { sha256sum | cut -d' ' -f1; }

find_node() {
    if command -v node >/dev/null 2>&1; then
        command -v node
        return 0
    fi
    local candidate
    candidate=$(ls -1d "$HOME"/.nvm/versions/node/*/bin/node 2>/dev/null | sort -V | tail -1)
    [[ -n ${candidate:-} ]] && printf '%s\n' "$candidate"
}

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_tcp() { # <port> [secs]
    local port=$1 secs=${2:-40} i=0
    while (( i < secs * 4 )); do
        (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0
        sleep 0.25
        i=$((i + 1))
    done
    return 1
}

http_code() { # <url> [<save-body-to>]
    local url=$1 body=${2:-/dev/null}
    curl -sS -o "$body" -w '%{http_code}' --max-time 5 "$url" 2>/dev/null || true
}

ws_upgrade_code() { # <ws-url-as-http>
    curl -sS -o /dev/null -w '%{http_code}' --http1.1 --max-time 4 \
        -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
        -H 'Sec-WebSocket-Version: 13' \
        -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
        "$1" 2>/dev/null || true
}

stop_pid() {
    local file=$1 pid
    [[ -f $file ]] || return 0
    pid=$(cat "$file" 2>/dev/null || true)
    [[ -n ${pid:-} ]] || return 0
    kill -TERM "$pid" 2>/dev/null || return 0
    local i=0
    while (( i < 40 )); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.25
        i=$((i + 1))
    done
    kill -KILL "$pid" 2>/dev/null || true
    return 0
}

# run_bg: **一律后台**执行一条可能很慢的命令，带超时与日志。
# 用法: run_bg <log> <timeout-secs> <label> <cmd...>
# 返回: 命令退出码；超时 124。
run_bg() {
    local log=$1 to=$2 label=$3
    shift 3
    rm -f "$log.rc"
    ( "$@" >"$log" 2>&1; echo $? >"$log.rc" ) &
    local watcher=$! waited=0
    printf '  … %s（后台，日志 %s）' "$label" "$log" >&2
    while [[ ! -s "$log.rc" ]]; do
        if (( waited >= to )); then
            printf '\n  超时 %ss，终止：%s\n' "$to" "$label" >&2
            pkill -TERM -P "$watcher" 2>/dev/null || true
            kill -TERM "$watcher" 2>/dev/null || true
            sleep 2
            pkill -KILL -P "$watcher" 2>/dev/null || true
            kill -KILL "$watcher" 2>/dev/null || true
            wait "$watcher" 2>/dev/null
            return 124
        fi
        sleep 5
        waited=$((waited + 5))
        printf '.' >&2
    done
    wait "$watcher" 2>/dev/null
    printf '\n' >&2
    cat "$log.rc"
}

run_cargo() { # <repo-dir> <target-dir> <log> <label> <cargo-args...>
    local dir=$1 target=$2 log=$3 label=$4
    shift 4
    run_bg "$log" "$TIMEOUT_SECS" "$label" \
        bash -c 'cd "$1" && t=$2 && shift 2 && exec env CARGO_TARGET_DIR="$t" cargo "$@"' \
        _ "$dir" "$target" "$@"
}

report_cargo() { # <rc> <log> <check-name>
    local rc=$1 log=$2 name=$3 summary
    summary=$(grep -E '^test result:' "$log" 2>/dev/null \
        | sed -E 's/^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed.*/\2 passed \3 failed/' \
        | paste -sd ', ' -)
    case $rc in
        0) pass "$name" "${summary:-无测试汇总行}" ;;
        124) skip "$name" "超时 ${TIMEOUT_SECS}s（隔离 target 首建较慢；不阻塞，稍后重跑）" ;;
        *)
            fail "$name" "退出码 $rc；${summary:-无测试汇总行}"
            printf '     ── 日志尾部（%s）──\n' "$log"
            tail -15 "$log" | sed 's/^/     /'
            ;;
    esac
}

# ── 前置校验 ────────────────────────────────────────────────────────────────

preflight() {
    [[ -f $CORE/Cargo.toml ]] || die "找不到 Core 源码（$CORE/Cargo.toml）"
    [[ -f $PANEL/web/package.json ]] || die "找不到 Panel 源码（$PANEL/web/package.json）"
    local core_target_abs core_root_abs
    core_target_abs=$(realpath -m -- "$CORE_TARGET")
    core_root_abs=$(realpath -m -- "$CORE/target")
    case "$core_target_abs" in
        "$core_root_abs"|"$core_root_abs"/*)
            die "拒绝使用 Core 根 target（$core_target_abs）：B2 可能正在用它；请给 --core-target-dir"
            ;;
    esac
    mkdir -p -- "$core_target_abs" || die "无法创建隔离 target: $core_target_abs"
    mkdir -p -- "$E2E_DIR"
    git -C "$CORE" rev-parse --git-dir >/dev/null 2>&1 || die "Core 不是 git 工作树：$CORE"
    git -C "$PANEL" rev-parse --git-dir >/dev/null 2>&1 || die "Panel 不是 git 工作树：$PANEL"
}

# ── 阶段 1：配对静态证据 ────────────────────────────────────────────────────

stage_pair() {
    echo "── 阶段：配对静态证据（不构建、不联网）"
    local gen="$PANEL/web/src/protocol.generated.ts"
    local pkg="$PANEL/web/package.json"
    [[ -f $gen ]] || { fail "P0 Panel 生成物存在" "缺 $gen"; return; }

    local head_pin_sha head_pin_commit work_sha panel_head
    head_pin_sha=$(json_field "$pkg" protocolCore generatedSha256)
    head_pin_commit=$(json_field "$pkg" protocolCore commit)
    work_sha=$(sha256_file "$gen")
    panel_head=$(git -C "$PANEL" rev-parse HEAD)

    # P1：固定记录与文件逐字一致（防"手改生成物/只更新一半"）。
    if [[ -n $head_pin_sha && $work_sha == "$head_pin_sha" ]]; then
        pass "P1 Panel 协议生成物自洽" "sha256=${work_sha:0:12}… 与 package.json 固定值一致"
    elif [[ -z $head_pin_sha ]]; then
        warn "P1 Panel 协议生成物自洽" "worktree 的 package.json 无 protocolCore 记录（老版本 Panel？）"
    else
        fail "P1 Panel 协议生成物自洽" "文件 ${work_sha:0:12}… ≠ 固定 ${head_pin_sha:0:12}…"
    fi

    # P2/P3：候选提交（若指定）必须在本地历史里，且生成物与工作树逐字一致。
    if [[ -n $EXPECT_REV ]]; then
        local rev rev_sha rev_count
        if ! rev=$(git -C "$PANEL" rev-parse --verify "${EXPECT_REV}^{commit}" 2>/dev/null); then
            fail "P2 候选 Panel 提交存在" "本地 Panel 仓没有 $EXPECT_REV"
            rev=""
        else
            [[ $rev == "$EXPECT_REV" ]] || warn "P2 候选 Panel 提交" "短 SHA 展开为 $rev"
            if git -C "$PANEL" merge-base --is-ancestor "$rev" "$panel_head"; then
                rev_count=$(git -C "$PANEL" rev-list --count "$rev..$panel_head")
                pass "P2 候选 Panel 提交在本地历史内" "$rev 之后还有 $rev_count 个提交（本地 HEAD=$panel_head）"
            else
                warn "P2 候选 Panel 提交在本地历史内" "$rev 不是 HEAD 的祖先（分叉候选；e2e 与打包需注明）"
            fi
        fi
        if [[ -n $rev ]]; then
            rev_sha=$(git -C "$PANEL" show "$rev:web/src/protocol.generated.ts" 2>/dev/null | sha256_stdin)
            if [[ $rev_sha == "$work_sha" ]]; then
                pass "P3 候选提交的协议生成物与工作树逐字一致" "sha256=${rev_sha:0:12}…"
            else
                fail "P3 候选提交的协议生成物与工作树逐字一致" "候选 ${rev_sha:0:12}… ≠ 工作树 ${work_sha:0:12}…"
            fi
            local rev_pin
            rev_pin=$(git -C "$PANEL" show "$rev:web/package.json" 2>/dev/null | json_field_stdin protocolCore commit)
            [[ -n $rev_pin ]] || warn "P3b 候选提交的配对记录" "该提交的 package.json 还没有 protocolCore 记录（协议固定是后续提交加的）"
        fi
    else
        skip "P2 候选 Panel 提交" "未指定 --rev（只记录工作树 HEAD=$panel_head）"
    fi

    # P4：Core 自基线提交以来没动过协议源 → 只证明静态来源未变；生成物是否
    # 逐字等价必须由 `--stage generate` 实际生成后比对。旧联邦来源
    # （`agent-module-federation/src/protocol.rs`）也属于会进管理面线格式的
    # DTO，不能漏。
    local base_pin=${CORE_PIN:-$head_pin_commit}
    local core_head
    core_head=$(git -C "$CORE" rev-parse HEAD)
    if [[ -n $base_pin ]]; then
        if ! git -C "$CORE" cat-file -e "${base_pin}^{commit}" 2>/dev/null; then
            fail "P4 Core 协议源基线可达" "Core 仓没有提交 $base_pin"
        elif ! git -C "$CORE" merge-base --is-ancestor "$base_pin" "$core_head"; then
            warn "P4 Core 协议源基线可达" "$base_pin 不是 Core HEAD 的祖先（基线过期）"
        else
            local changed n
            changed=$(git -C "$CORE" diff --name-only "$base_pin..$core_head" -- \
                source/modules/agent-module-management/src/protocol \
                source/modules/agent-module-federation/src/protocol.rs \
                source/modules/echo-protocol 2>/dev/null)
            n=$(git -C "$CORE" rev-list --count "$base_pin..$core_head")
            if [[ -z $changed ]]; then
                pass "P4 Core 提交级协议源静态未变（管理面 + 旧联邦来源 + echo-protocol）" \
                    "pin=${base_pin:0:12}… HEAD=${core_head:0:12}… (+$n 提交) → 提交级静态来源未变；工作树与逐字等价需 --stage generate"
            else
                warn "P4 Core 提交级协议源静态未变（管理面 + 旧联邦来源 + echo-protocol）" \
                    "有 $(printf '%s\n' "$changed" | wc -l) 个协议文件变了 → 必须跑 --stage generate 重新比对"
            fi
        fi
    else
        skip "P4 Core 协议源基线" "Panel 未记录 protocolCore.commit，且未给 --core-pin"
    fi

    # P5：工作树仍未提交的协议改动 —— 提交级证据不能代表"现在编译出来的东西"。
    local dirty
    dirty=$(git -C "$CORE" status --porcelain -- \
        source/modules/agent-module-management/src/protocol \
        source/modules/agent-module-federation/src/protocol.rs \
        source/modules/echo-protocol 2>/dev/null)
    if [[ -z $dirty ]]; then
        pass "P5 Core 工作树协议文件干净" "提交级证据 == 工作树（含旧联邦来源）"
    else
        warn "P5 Core 工作树协议文件干净" \
            "$(printf '%s\n' "$dirty" | wc -l) 个文件有未提交改动 → 只证明提交配对；工作树配对需 --stage generate"
    fi

    # P6：Panel 工作树的生成物不得被改动（否则 P1 的比对对象被污染）。
    if [[ -n $(git -C "$PANEL" status --porcelain -- web/src/protocol.generated.ts 2>/dev/null) ]]; then
        fail "P6 Panel 生成物未被手改" "web/src/protocol.generated.ts 有未提交改动"
    else
        pass "P6 Panel 生成物未被手改" "git status 干净"
    fi

    # P7：只读观察并行任务的范围（不做任何断言，仅让结论可解释）。
    local busy
    busy=$(git -C "$CORE" status --porcelain -- Cargo.toml Cargo.lock .github/workflows/release.yml Dockerfile 2>/dev/null | awk '{print $2}' | paste -sd ' ' -)
    if [[ -n $busy ]]; then
        warn "P7 并行任务范围（只读观察）" "这些文件有未提交改动，本 harness 不碰：$busy"
    else
        pass "P7 并行任务范围（只读观察）" "B2/A2 的文件当前干净"
    fi

    # P8：e2e 需要的 Panel 前端产物（嵌入/独立两种形态都用它）。
    if [[ -f $PANEL/web/dist/index.html ]]; then
        pass "P8 Panel 前端产物存在" "web/dist/index.html（mtime $(date -r "$PANEL/web/dist/index.html" '+%m-%d %H:%M')）"
    else
        warn "P8 Panel 前端产物存在" "缺 web/dist/index.html → e2e 阶段会 SKIP"
    fi
}

# ── 阶段 2：Core 侧协议行为（独立 + 嵌入/合并）─────────────────────────────

stage_core() {
    echo "── 阶段：Core 管理面集成测试（隔离 target: $CORE_TARGET）"
    local rc
    rc=$(run_cargo "$CORE" "$CORE_TARGET" "$WORK/core-lib.log" \
        "cargo test -p agent-module-management --lib" \
        test -p agent-module-management --lib)
    report_cargo "$rc" "$WORK/core-lib.log" "C1 管理面 WS（独立 + 合并端口 + Panel 身份）"

    rc=$(run_cargo "$CORE" "$CORE_TARGET" "$WORK/core-baseline.log" \
        "cargo test -p agent-module-management --test protocol_baseline" \
        test -p agent-module-management --test protocol_baseline)
    report_cargo "$rc" "$WORK/core-baseline.log" "C2 管理面协议基线"

    # 把"哪些用例覆盖了两种形态"写进结论，避免"跑过了但没人知道跑了什么"。
    if [[ -f $WORK/core-lib.log ]]; then
        local n_standalone n_merged
        n_standalone=$(grep -cE '^test (tests::)?(panel_command_reaches_agent_backend|agent_events_reach_panel|malformed_frame_gets_an_explicit_error|unauthorized_panel_gets_http_401_not_silent_close|stream_deltas_are_batched)' \
            "$WORK/core-lib.log" 2>/dev/null || true)
        n_merged=$(grep -cE '^test (tests::)?merged_port_' "$WORK/core-lib.log" 2>/dev/null || true)
        if [[ ${n_standalone:-0} -gt 0 || ${n_merged:-0} -gt 0 ]]; then
            pass "C3 形态覆盖可解释" "独立形态用例 ${n_standalone:-0} 条、嵌入（合并端口）形态 ${n_merged:-0} 条出现在结果里"
        fi
    fi
}

# ── 阶段 3：Panel 侧代理行为（独立外壳）────────────────────────────────────

stage_panel() {
    echo "── 阶段：Panel 独立外壳测试（target: $PANEL_TARGET）"
    local rc
    rc=$(run_cargo "$PANEL" "$PANEL_TARGET" "$WORK/panel-proxy.log" \
        "cargo test -p echo-web-server --test proxy" \
        test -p echo-web-server --test proxy)
    report_cargo "$rc" "$WORK/panel-proxy.log" "A1 /ws 中继（协议初始化 + 断线/半死链恢复）"

    rc=$(run_cargo "$PANEL" "$PANEL_TARGET" "$WORK/panel-static.log" \
        "cargo test -p echo-web-server --test static_files --test base_path" \
        test -p echo-web-server --test static_files --test base_path)
    report_cargo "$rc" "$WORK/panel-static.log" "A2 静态资源 + 子路径（独立形态）"

    local node_bin
    node_bin=$(find_node)
    if [[ -n $node_bin && -f $PANEL/web/scripts/sync-protocol.test.mjs ]]; then
        rc=$(run_bg "$WORK/panel-protocol-sync.log" 300 "node --test sync-protocol.test.mjs" \
            bash -c 'cd "$1" && shift && exec "$@" --test scripts/sync-protocol.test.mjs' _ "$PANEL/web" "$node_bin")
        if [[ $rc -eq 0 ]]; then
            pass "A3 协议同步脚本自身行为" "$(grep -E '^# (pass|fail)' "$WORK/panel-protocol-sync.log" | paste -sd ' ' -)"
        elif [[ $rc -eq 124 ]]; then
            skip "A3 协议同步脚本自身行为" "超时"
        else
            fail "A3 协议同步脚本自身行为" "退出码 $rc"
        fi
    else
        skip "A3 协议同步脚本自身行为" "无 node 或缺 scripts/sync-protocol.test.mjs"
    fi
}

# ── 阶段 4：生成物比对（需构建；B2 未稳定时允许 SKIP）──────────────────────

stage_generate() {
    echo "── 阶段：用 Core 当前源码重新生成 TS 绑定并比对（隔离 target）"
    local out="$WORK/protocol.generated.ts"
    local rc
    rc=$(run_cargo "$CORE" "$CORE_TARGET" "$WORK/generate.log" \
        "cargo run -p echo-agent-core --features ts-bindings -- dump-protocol-ts" \
        run --quiet -p echo-agent-core --features ts-bindings -- dump-protocol-ts --out "$out")
    if [[ $rc -ne 0 ]]; then
        if [[ $rc -eq 124 ]]; then
            skip "G1 Core 现场生成物与 Panel 固定值一致" "超时：隔离 target 首建太慢（B2 未稳定时不阻塞）"
        else
            fail "G1 Core 现场生成物与 Panel 固定值一致" "生成失败（退出码 $rc）；日志尾部见下"
            tail -12 "$WORK/generate.log" | sed 's/^/     /'
        fi
        return
    fi
    local live_sha pinned_sha
    live_sha=$(sha256_file "$out")
    pinned_sha=$(json_field "$PANEL/web/package.json" protocolCore generatedSha256)
    if [[ -n $pinned_sha && $live_sha == "$pinned_sha" ]]; then
        pass "G1 Core 现场生成物与 Panel 固定值一致" "sha256=${live_sha:0:12}…（逐字相等）"
    else
        fail "G1 Core 现场生成物与 Panel 固定值一致" \
            "Core 现场 ${live_sha:0:12}… ≠ Panel 固定 ${pinned_sha:0:12}… → Panel 需 npm run sync-protocol"
    fi
}

# ── 阶段 5：两种形态的端到端（预构建二进制自测，明确标记）──────────────────

write_core_config() { # <dir> <core-port> <token>
    cat >"$1/core.toml" <<EOF
# 仅 smoke 用：回环随机端口、随机令牌、无 peer、无平台实例。
[logging]
level = "info"
format = "text"

[agent]
provider = "openai"

[core]
management_address = "127.0.0.1:$2"
management_access_token = "$3"

[federation]
# 显式 "" = 不监听联邦端口、没有 peer → 不可能与其他节点建立链路。
listen = ""
EOF
}

write_embedded_modules() { # <dir> <panel-port> <static-dir>
    cat >"$1/modules.toml" <<EOF
# 嵌入形态：Core 进程内起 Panel（实现来自 Panel 仓 agent-module-panel）。
# 自更新模块显式关掉：smoke 不允许任何自更新行为。
[[module]]
id = "echo-agent.selfupdate"
kind = "builtin"
name = "self-update"
enabled = false

[[module]]
id = "echo-agent.panel.web"
kind = "builtin"
name = "Web Panel"
enabled = true

[module.config]
bind = "127.0.0.1:$2"
static_dir = "$3"
require_token = false
EOF
}

write_panel_config() { # <dir> <panel-port> <core-port> <token>
    cat >"$1/panel.toml" <<EOF
# 仅 smoke 用：独立外壳连回环上的临时 Core，随机令牌。
[logging]
level = "info"

[server]
bind_address = "127.0.0.1:$2"
static_dir = "$4"

[core]
connect_url = "ws://127.0.0.1:$3"
access_token = "$5"
EOF
}

start_core_bg() { # <dir> <bin>
    (
        cd "$1" || exit 1
        setsid env -i PATH="$PATH" HOME="$1/home" \
            XDG_CONFIG_HOME="$1/xdg-config" XDG_DATA_HOME="$1/xdg-data" \
            XDG_CACHE_HOME="$1/xdg-cache" XDG_STATE_HOME="$1/xdg-state" \
            "$2" -c "$1/core.toml" >>"$1/core.log" 2>&1 &
        echo $! >"$1/core.pid"
    )
}

start_panel_bg() { # <dir> <bin>
    (
        cd "$1" || exit 1
        setsid env -i PATH="$PATH" HOME="$1/home" \
            XDG_CONFIG_HOME="$1/xdg-config" XDG_DATA_HOME="$1/xdg-data" \
            XDG_CACHE_HOME="$1/xdg-cache" XDG_STATE_HOME="$1/xdg-state" \
            "$2" -c "$1/panel.toml" >>"$1/panel.log" 2>&1 &
        echo $! >"$1/panel.pid"
    )
}

check_http_shell() { # <label> <port>
    local label=$1 port=$2 code
    local body="$WORK/${label}-index.html"
    code=$(http_code "http://127.0.0.1:$port/" "$body")
    if [[ $code == 200 ]] && grep -q 'EchoAgent Panel' "$body" && grep -q '<div id="app"' "$body"; then
        pass "$label 静态壳" "GET / → 200，index.html 含标题与挂载点"
    else
        fail "$label 静态壳" "GET / → ${code:-无响应}；$(head -c 120 "$body" 2>/dev/null)"
        return
    fi
    local asset
    asset=$(grep -oE 'src="/[^"]+\.js"' "$body" | head -1 | sed -E 's/^src="//; s/"$//')
    if [[ -n $asset ]]; then
        code=$(http_code "http://127.0.0.1:$port$asset")
        [[ $code == 200 ]] && pass "$label 静态资源" "GET $asset → 200" \
            || fail "$label 静态资源" "GET $asset → ${code:-无响应}"
    else
        warn "$label 静态资源" "index.html 里没找到 JS 入口（构建形态变了？）"
    fi
}

check_ws_relay() { # <label> <port> <expect-extra-links>
    local label=$1 port=$2 want=${3:-1} code links
    code=$(ws_upgrade_code "http://127.0.0.1:$port/ws")
    [[ $code == 101 ]] && pass "$label WS 升级" "GET /ws → 101" \
        || fail "$label WS 升级" "GET /ws → ${code:-无响应}（期望 101）"
    # Core 侧出现 "panel link up" = Panel → Core 管理面握手通过（真令牌）。
    links=$(grep -c 'panel link up' "$E2E_DIR/core.log" 2>/dev/null || true)
    if [[ ${links:-0} -ge $want ]]; then
        pass "$label 到 Core 的协议初始化" "Core 日志 panel link up ×${links}（merged=false access=admin）"
    else
        fail "$label 到 Core 的协议初始化" "Core 日志只有 ${links:-0} 次 panel link up（期望 ≥$want）"
        tail -8 "$E2E_DIR/core.log" | sed 's/^/     /'
    fi
}

check_application() {
    local label=$1 port=$2 node_bin detail
    node_bin=$(find_node)
    if [[ -z $node_bin ]]; then
        fail "$label 应用协议" "需要支持内置 WebSocket 的 Node.js 22 或以上"
    elif detail=$(timeout 25s "$node_bin" "$SCRIPT_DIR/smoke-panel-application.mjs" "$port" 2>&1); then
        pass "$label 应用协议" "$detail"
    else
        fail "$label 应用协议" "$detail"
    fi
}

stage_e2e() {
    echo "── 阶段：端到端（**预构建二进制自测，非最终结果**）"
    if [[ ! -x $CORE_BIN ]]; then
        skip "E0 预构建二进制" "缺 $CORE_BIN（不现场构建：B2 未稳定时不做整包构建）"
        return
    fi
    if [[ ! -f $PANEL/web/dist/index.html ]]; then
        skip "E0 预构建二进制" "缺 Panel 前端产物 web/dist/index.html"
        return
    fi
    mkdir -p "$E2E_DIR"/{home,xdg-config,xdg-data,xdg-cache,xdg-state}

    local core_port panel_port standalone_port token
    core_port=$(free_port)
    panel_port=$(free_port)
    standalone_port=$(free_port)
    token=$(python3 -c 'import secrets; print(secrets.token_hex(16))')

    printf '  · 预构建 Core: %s（%s）\n' "$CORE_BIN" "$(date -r "$CORE_BIN" '+%Y-%m-%d %H:%M')"
    printf '  · 预构建 Panel: %s（%s）\n' "$PANEL_BIN" "$(date -r "$PANEL_BIN" '+%Y-%m-%d %H:%M' 2>/dev/null || echo '缺')"
    printf '  · 回环端口: core=%s embedded=%s standalone=%s；令牌随机、无真实凭据\n' \
        "$core_port" "$panel_port" "$standalone_port"

    write_core_config "$E2E_DIR" "$core_port" "$token"
    write_embedded_modules "$E2E_DIR" "$panel_port" "$PANEL/web/dist"

    # ── 嵌入形态 ──
    start_core_bg "$E2E_DIR" "$CORE_BIN"
    if ! wait_tcp "$core_port" 40; then
        fail "[预构建自测] 嵌入形态 Core 启动" "管理端口 $core_port 未监听"
        tail -10 "$E2E_DIR/core.log" | sed 's/^/     /'
        return
    fi
    pass "[预构建自测] 嵌入形态 Core 启动" "管理面 127.0.0.1:$core_port（federation listen=\"\"、selfupdate disabled）"

    if ! wait_tcp "$panel_port" 40; then
        fail "[预构建自测] 嵌入形态 Panel 模块起来" "端口 $panel_port 未监听"
        tail -12 "$E2E_DIR/core.log" | sed 's/^/     /'
    else
        pass "[预构建自测] 嵌入形态 Panel 模块起来" "进程内模块监听 127.0.0.1:$panel_port"
        check_http_shell "[预构建自测] 嵌入形态" "$panel_port"
        check_ws_relay "[预构建自测] 嵌入形态" "$panel_port" 1
        check_application "嵌入形态" "$panel_port"
    fi

    # ── 独立形态（同一个 Core 不动，另起独立外壳）──
    if [[ ! -x $PANEL_BIN ]]; then
        skip "[预构建自测] 独立形态" "缺 $PANEL_BIN"
    else
        write_panel_config "$E2E_DIR" "$standalone_port" "$core_port" "$PANEL/web/dist" "$token"
        start_panel_bg "$E2E_DIR" "$PANEL_BIN"
        if wait_tcp "$standalone_port" 40; then
            pass "[预构建自测] 独立形态 Panel 启动" "127.0.0.1:$standalone_port"
            check_http_shell "[预构建自测] 独立形态" "$standalone_port"
            check_ws_relay "[预构建自测] 独立形态" "$standalone_port" 2
        check_application "独立形态" "$standalone_port"
        else
            fail "[预构建自测] 独立形态 Panel 启动" "端口 $standalone_port 未监听"
            tail -10 "$E2E_DIR/panel.log" 2>/dev/null | sed 's/^/     /'
        fi
    fi

    # 收尾：明确杀掉两个临时进程（trap 也会兜底）。
    stop_pid "$E2E_DIR/panel.pid"
    stop_pid "$E2E_DIR/core.pid"
    pass "[预构建自测] 临时实例清理" "两个临时进程已终止（非现役实例，未重启/未安装任何东西）"
}

# ── 主流程 ──────────────────────────────────────────────────────────────────

printf 'Core : %s\nPanel: %s\n临时 : %s\n阶段 : %s\n\n' "$CORE" "$PANEL" "$WORK" "$STAGES"
preflight

for stage in $STAGES; do
    case $stage in
        pair) stage_pair ;;
        core) stage_core ;;
        panel) stage_panel ;;
        generate) stage_generate ;;
        e2e) stage_e2e ;;
        *) die "未知阶段: $stage" ;;
    esac
    echo
done

echo "── 汇总 ──────────────────────────────────────────────────────────────────"
for row in "${SUMMARY[@]}"; do
    IFS='|' read -r status name detail <<<"$row"
    printf '%-4s %-34s %s\n' "$status" "$name" "$detail"
done
printf '\nPASS=%d FAIL=%d WARN=%d SKIP=%d\n' "$PASSED" "$FAILED" "$WARNED" "$SKIPPED"

if (( FAILED > 0 )); then
    echo "结论：组合验收**未通过**（见上面 FAIL 行）。"
    exit 1
fi
if (( PASSED == 0 )); then
    echo "结论：没有任何检查被真正执行——请检查 --stage 与路径。"
    exit 1
fi
echo "结论：组合验收通过（WARN/SKIP 需在交付记录里逐条引用）。"
exit 0
