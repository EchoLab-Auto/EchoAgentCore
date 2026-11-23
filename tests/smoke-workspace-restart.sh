#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# tests/smoke-workspace-restart.sh — Core Workspace **真正进程重启**的隔离验收
#
# ## 五问（写之前先回答）
#
# 1. 要证明什么？
#    Core 进程被真正杀掉再起一个新进程（不是同进程重连、不是客户端补拉）后，
#    只靠磁盘上已有的 `echo-workspaces-<team>.json` + `echo-sessions-<team>.json`
#    **启动期自己**恢复出激活工作区通道，并让面板看到该会话与历史。
#
# 2. 怎么判定失败？
#    ① 第二个 Core 进程里，客户端连接前日志没有出现冷启动补种；② 客户端
#    只发 `RequestState` 时看不到 `local:workspace:<ws>/<conv>:local_user`；
#    ③ `RequestSessionHistory` 读不到重启前写入的 user/assistant 标记；
#    ④ 工作区文档被恢复流程改写；⑤ 子进程/端口没清干净。任一即 FAIL。
#
# 3. 隔离边界在哪？
#    独立临时 HOME/XDG_*/TMPDIR、回环随机端口、无账号、无联邦 peer、无平台
#    实例、自更新与内置 Web Panel 显式关闭；LLM 用回环假服务。生产配置与
#    真实会话全程不读写。
#
# 4. 不测什么 / 不声称什么？
#    不测 Panel 前端渲染、不测跨机联邦、不测多实例适配器、不测工作区 store
#    的并发写；**不走任何客户端的 `RequestWorkspaceSessions`**——否则“恢复”
#    可能只是客户端补拉，证明不了启动恢复。也不把本地二进制当成正式锁复验。
#
# 5. 怎么复验？
#    见运行末尾打印的复验命令（以及本文件头的“正式复验”段）。默认用本机
#    `target/debug/echo-agent-core`（可用
#    `--core-bin` 指定）。临时的旧 target/本地覆盖**不能替代**正式 CI 的
#    Cargo.lock 复验；本脚本只证明“这枚二进制在这个隔离场景下的行为”。
#
# ## 四段
#
# ### ① 目标与验收
#    Core#1（PID A）启动后，通过管理 WS 往激活工作区对话发一条消息，等
#    `AgentCompleted`；SIGTERM 优雅退出并落盘。Core#2（PID B，全新进程）用
#    同一份隔离数据启动；**在客户端连接之前**检查日志出现
#    “workspace 镜像已由会话回执补种（冷启动恢复）”；随后客户端只发
#    `RequestState` + `RequestSessionHistory`，校验：
#      - 会话 id = `local:workspace:ws-restart/conv-restart:local_user`
#      - team = `team-a`、platform = `local`、scope = `workspace`
#      - 昵称 = `重启验收工作区 · 主对话`
#      - 历史含预置标记、Core#1 写入的 user 标记、假 LLM 的 assistant 标记
#    验收 = 退出码 0 且没有 FAIL。
#
# ### ② 事实与假设（写作前核对过的代码位置）
#    - 冷启动恢复：`source/modules/core/src/main.rs` 的
#      `crate::host_services::restore_workspace_sessions(...)`（模块挂载后、
#      人格能力应用后异步发起）。
#    - 补种落点：`source/modules/agent-module-loop/src/agent/workspace_commands.rs`
#      的 `adopt_workspace_sessions` + `workspace_after_change`；日志
#      “workspace 镜像已由会话回执补种（冷启动恢复）”只在真的补种时出现。
#    - 工作区落盘：模块 `Config::effective_state_dir()` 缺省
#      `$XDG_CONFIG_HOME/echo-agent-core`，本脚本用 modules.toml 的
#      `[module.config] state_dir` 显式指到临时目录。
#    - 会话落盘：`echo-sessions-<team>.json` 与 core.toml **同目录**。
#    - 工作区通道 id：`local:workspace:<scope>:local_user`；多对话时
#      scope = `<ws>/<conv>`（见模块 `store::channel_scope_id`）。
#    - `RequestState` 只读 trunk 的会话列表（`SessionUpdated`），不触发工作区
#      恢复；`RequestSessionHistory` 只读事件日志派生历史。
#    - 既有 `tests/smoke-panel-pair.sh` 只覆盖 WS 重连/配对，不覆盖进程重启。
#
# ### ③ 改动清单
#    - 仅新增 `tests/smoke-workspace-restart.sh` 与
#      `tests/smoke-workspace-restart.py`。不修改任何既有代码/测试，不提交、
#      不推送、不部署、不安装。
#    - 所有运行产物落在 `mktemp -d` 目录；成功后默认删除，失败或 `--keep`
#      时保留并打印路径。
#
# ### ④ 风险与回退
#    - 风险：本地二进制可能来自旧的 target/路径覆盖，**不能**替代正式
#      Cargo.lock 复验 → 输出里打印二进制路径、sha256、mtime，并明确标注。
#    - 风险：首次启动较慢 / 机器忙 → 所有等待都有超时；超时记 FAIL 而不是
#      静默 SKIP。
#    - 回退：删除这两个文件即可；脚本不在工作树留产物，不碰生产配置。
#
# ## 正式复验（长任务后台化）
#
#   CARGO_TARGET_DIR=/tmp/echo-smoke-target \
#       cargo build --locked --bin echo-agent-core >/tmp/echo-smoke-build.log 2>&1 &
#   # 构建结束后：
#   bash tests/smoke-workspace-restart.sh \
#       --core-bin /tmp/echo-smoke-target/debug/echo-agent-core
#
# 本地 target/ 或路径覆盖（例如 /tmp 下已有的旧构建）只能证明“这枚二进制在这个
# 隔离场景下的行为”，**不能**替代按 Cargo.lock 的正式复验。
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CORE_SOURCE=${CORE_SOURCE:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
CORE_BIN=${SMOKE_CORE_BIN:-}
PYTHON=${PYTHON:-python3}
KEEP=${SMOKE_KEEP:-0}
TIMEOUT_SECS=${SMOKE_TIMEOUT:-90}
HELPER="$SCRIPT_DIR/smoke-workspace-restart.py"

usage() {
    cat <<'EOF'
用法: bash tests/smoke-workspace-restart.sh [选项]

  --core-bin <path>   Core 二进制（缺省 $CORE/target/debug/echo-agent-core，
                      再退 target/release；也可用 SMOKE_CORE_BIN）
  --core <dir>        Core 源码目录（缺省 = 本脚本上一级，或 $CORE_SOURCE）
  --python <path>     Python 解释器（缺省 python3；只要求标准库）
  --timeout <secs>    单步超时（缺省 90）
  --keep              保留临时目录（排障用）
  -h, --help

安全边界（脚本强制）:
  * 只写 mktemp 目录；Core 的 HOME/XDG_*/TMPDIR 全部指到临时目录；
  * 只绑 127.0.0.1 + 随机端口；无联邦 peer、无平台实例、自更新/Web Panel 关闭；
  * LLM 是回环假服务；无真实 API key、无账号、无外部网络副作用；
  * 不发送 RequestWorkspaceSessions；不读不写生产配置或真实会话。
EOF
}

while (($#)); do
    case $1 in
        --core-bin) CORE_BIN=${2:-}; shift 2 ;;
        --core-bin=*) CORE_BIN=${1#*=}; shift ;;
        --core) CORE_SOURCE=${2:-}; shift 2 ;;
        --core=*) CORE_SOURCE=${1#*=}; shift ;;
        --python) PYTHON=${2:-}; shift 2 ;;
        --python=*) PYTHON=${1#*=}; shift ;;
        --timeout) TIMEOUT_SECS=${2:-}; shift 2 ;;
        --timeout=*) TIMEOUT_SECS=${1#*=}; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "未知参数: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z ${CORE_BIN:-} ]]; then
    for candidate in "$CORE_SOURCE/target/debug/echo-agent-core" \
                     "$CORE_SOURCE/target/release/echo-agent-core"; do
        if [[ -x $candidate ]]; then
            CORE_BIN=$candidate
            break
        fi
    done
fi
if [[ -z ${CORE_BIN:-} || ! -x $CORE_BIN ]]; then
    echo "✗ 找不到可执行的 Core 二进制；用 --core-bin 指定" >&2
    exit 2
fi
CORE_BIN=$(cd -- "$(dirname -- "$CORE_BIN")" && pwd)/$(basename -- "$CORE_BIN")
if [[ ! -f $HELPER ]]; then
    echo "✗ 缺少辅助脚本: $HELPER" >&2
    exit 2
fi

# 二进制能力预检：旧 target / 本地覆盖很容易“能启动但没有 C3c 冷启动恢复”。
# 这段日志字面量是恢复路径的唯一 info 证据；没有它就直接判二进制不合格，
# 免得把“旧构建”误报成“功能回归”。正式结论仍须按 Cargo.lock 重新构建。
if ! grep -a -q "workspace 镜像已由会话回执补种" "$CORE_BIN"; then
    echo "✗ 该 Core 二进制不含冷启动恢复日志标记（很可能是旧 target）: $CORE_BIN" >&2
    echo "  请用 --core-bin 指定包含 C3c 恢复路径的构建；本地旧 target/路径覆盖不能替代正式 Cargo.lock 复验。" >&2
    exit 2
fi
for tool in "$PYTHON" setsid timeout sha256sum mktemp; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "✗ 缺少命令: $tool" >&2
        exit 2
    fi
done
PYTHON_BIN=$(command -v "$PYTHON")

WORK=$(mktemp -d "${TMPDIR:-/tmp}/smoke-workspace-restart.XXXXXX") || exit 1
WORK=$(cd -- "$WORK" && pwd -P)
# 先挂一个最小兜底：即使后面定义函数/写夹具时意外退出，也不会把临时目录
# 留在 /tmp（完整 cleanup 会在函数定义后覆盖这个 trap）。
trap '[[ -n ${WORK:-} ]] && rm -rf -- "$WORK"' EXIT HUP INT TERM
HOME_DIR="$WORK/home"
XDG_CONFIG_DIR="$WORK/xdg-config"
XDG_DATA_DIR="$WORK/xdg-data"
XDG_STATE_DIR="$WORK/xdg-state"
XDG_CACHE_DIR="$WORK/xdg-cache"
TMP_DIR="$WORK/tmp"
RUN_DIR="$WORK/run"
STATE_DIR="$WORK/state"
WORKSPACE_DIR="$WORK/work"
LOG_DIR="$WORK/logs"
mkdir -p "$HOME_DIR" "$XDG_CONFIG_DIR" "$XDG_DATA_DIR" "$XDG_STATE_DIR" \
         "$XDG_CACHE_DIR" "$TMP_DIR" "$RUN_DIR" "$STATE_DIR" "$WORKSPACE_DIR" "$LOG_DIR"

CONFIG="$RUN_DIR/echo-agent-core.toml"
MODULES="$RUN_DIR/modules.toml"
SESSION_FILE="$RUN_DIR/echo-sessions-team-a.json"
WORKSPACE_FILE="$STATE_DIR/echo-workspaces-team-a.json"
CORE1_LOG="$LOG_DIR/core-1.log"
CORE2_LOG="$LOG_DIR/core-2.log"
LLM_LOG="$LOG_DIR/fake-llm.log"
CORE1_PIDFILE="$WORK/core-1.pid"
CORE2_PIDFILE="$WORK/core-2.pid"
LLM_PIDFILE="$WORK/fake-llm.pid"

TEAM=team-a
WS_ID=ws-restart
CONV_ID=conv-restart
WS_NAME=重启验收工作区
CONV_NAME=主对话
SESSION_ID="local:workspace:$WS_ID/$CONV_ID:local_user"
RUN_ID="$(date +%Y%m%d%H%M%S)-$$"
PRESEED_USER="SMOKE-PRESEED-USER-$RUN_ID"
PRESEED_ASSISTANT="SMOKE-PRESEED-ASSISTANT-$RUN_ID"
E2E_USER="SMOKE-E2E-USER-$RUN_ID"
FAKE_REPLY="SMOKE-FAKE-LLM-REPLY-$RUN_ID"
SAFE_PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
REAL_HOME=${HOME:-}

PASSED=0
FAILED=0
WARNED=0
SKIPPED=0
FINISHED=0
declare -a SUMMARY=()
declare -a SUPERVISORS=()
LAST_SUP=""
CORE1_SUP=""
CORE2_SUP=""
LLM_SUP=""

pass() { PASSED=$((PASSED + 1)); SUMMARY+=("PASS|$1|$2"); printf '  ✅ %s — %s\n' "$1" "$2"; }
fail() { FAILED=$((FAILED + 1)); SUMMARY+=("FAIL|$1|$2"); printf '  ❌ %s — %s\n' "$1" "$2"; }
warn() { WARNED=$((WARNED + 1)); SUMMARY+=("WARN|$1|$2"); printf '  ⚠️  %s — %s\n' "$1" "$2"; }
skip() { SKIPPED=$((SKIPPED + 1)); SUMMARY+=("SKIP|$1|$2"); printf '  ⏭️  %s — %s\n' "$1" "$2"; }
section() { printf '\n── %s ──\n' "$1"; }

finish() { # <exit-code>
    local code=${1:-0}
    if ((FINISHED)); then
        exit "$code"
    fi
    FINISHED=1
    printf '\n── 汇总 ──\n'
    local row status name detail
    for row in "${SUMMARY[@]}"; do
        IFS='|' read -r status name detail <<<"$row"
        printf '  %-4s %s — %s\n' "$status" "$name" "$detail"
    done
    printf '\nPASS=%d FAIL=%d WARN=%d SKIP=%d\n' "$PASSED" "$FAILED" "$WARNED" "$SKIPPED"
    if ((FAILED == 0 && code == 0)); then
        printf '结论: PASS — Core Workspace 真正进程重启的启动恢复已验证\n'
    else
        printf '结论: FAIL — 见上方 FAIL/日志\n'
    fi
    if ((KEEP)); then
        printf '临时目录（--keep）: %s\n' "$WORK"
    fi
    exit "$code"
}

fatal() { # <name> <detail>
    fail "$1" "$2"
    printf '\n临时目录保留用于排障: %s\n' "$WORK" >&2
    printf '复验: bash %s --core-bin %s --keep\n' "$0" "$CORE_BIN" >&2
    finish 1
}

# ── 进程 / 端口 / 检查工具 ─────────────────────────────────────────────────

free_port() {
    "$PYTHON" - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_tcp() { # <port> [secs]
    local port=$1 secs=${2:-30} i=0
    local limit=$((secs * 4))
    while ((i < limit)); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            exec 3>&- 2>/dev/null || true
            return 0
        fi
        sleep 0.25
        i=$((i + 1))
    done
    return 1
}

start_bg() { # <pidfile> <log> <cmd...>
    local pidfile=$1 log=$2
    shift 2
    : >"$log"
    setsid bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$pidfile" "$@" >"$log" 2>&1 &
    LAST_SUP=$!
    SUPERVISORS+=("$LAST_SUP")
    local i=0
    while ((i < 100)); do
        [[ -s $pidfile ]] && return 0
        kill -0 "$LAST_SUP" 2>/dev/null || break
        sleep 0.05
        i=$((i + 1))
    done
    return 1
}

proc_running() { # <pid>
    local pid=$1
    [[ -n ${pid:-} ]] || return 1
    if [[ -r /proc/$pid/stat ]]; then
        local state
        state=$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)
        [[ -n ${state:-} && $state != Z ]]
        return
    fi
    kill -0 "$pid" 2>/dev/null
}

signal_pid() { # <signal> <pid>
    local sig=$1 pid=$2 pgid
    pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ' || true)
    # 只在“该 pid 自己是进程组组长”时才杀整个组；否则只杀单进程，避免误伤
    # 调用方的进程组（setsid 正常工作时 pid == pgid）。
    if [[ -n ${pgid:-} && $pgid == "$pid" ]]; then
        kill -"$sig" -"$pid" 2>/dev/null || kill -"$sig" "$pid" 2>/dev/null || true
    else
        kill -"$sig" "$pid" 2>/dev/null || true
    fi
}

stop_group() { # <pidfile> [grace-secs]
    local pidfile=$1 grace=${2:-20}
    [[ -s $pidfile ]] || return 0
    local pid
    pid=$(cat "$pidfile" 2>/dev/null || true)
    [[ -n ${pid:-} ]] || return 0
    proc_running "$pid" || return 0
    signal_pid TERM "$pid"
    local i=0 limit=$((grace * 5))
    while ((i < limit)); do
        proc_running "$pid" || return 0
        sleep 0.2
        i=$((i + 1))
    done
    signal_pid KILL "$pid"
    sleep 0.2
    ! proc_running "$pid"
}

reap_sup() { # <supervisor-pid>
    local sup=${1:-}
    [[ -n $sup ]] || return 0
    wait "$sup" 2>/dev/null || true
}

cleanup() {
    local rc=${1:-$?}
    trap - EXIT INT TERM
    stop_group "$LLM_PIDFILE" 5 >/dev/null 2>&1 || true
    stop_group "$CORE2_PIDFILE" 30 >/dev/null 2>&1 || true
    stop_group "$CORE1_PIDFILE" 30 >/dev/null 2>&1 || true
    local sup
    if ((${#SUPERVISORS[@]})); then
        for sup in "${SUPERVISORS[@]}"; do
            kill -KILL "$sup" 2>/dev/null || true
        done
        for sup in "${SUPERVISORS[@]}"; do
            reap_sup "$sup"
        done
    fi
    if ((KEEP == 0 && rc == 0)); then
        # 只删本脚本 mktemp 出来的目录；绝不按空变量/任意路径 rm -rf。
        if [[ -n ${WORK:-} && $WORK == */smoke-workspace-restart.* ]]; then
            rm -rf -- "$WORK"
        else
            printf '\n拒绝删除非 smoke 临时目录: %s\n' "${WORK:-<empty>}" >&2
        fi
    else
        printf '\n临时目录保留: %s\n' "$WORK"
    fi
    exit "$rc"
}
trap 'cleanup $?' EXIT
trap 'cleanup 130' INT TERM
trap 'cleanup 129' HUP

sha256_file() { sha256sum -- "$1" | cut -d' ' -f1; }

wait_log() { # <file> <fixed-string> [secs]
    local file=$1 pattern=$2 secs=${3:-10} i=0
    local limit=$((secs * 10))
    while ((i < limit)); do
        if grep -Fq -- "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

check_env_isolation() { # <label> <pid>
    local label=$1 pid=$2
    local envfile="/proc/$pid/environ"
    if [[ ! -r $envfile ]]; then
        warn "$label" "读不到 /proc/$pid/environ（跳过环境核对）"
        return 0
    fi
    local envtext
    envtext=$(tr '\0' '\n' <"$envfile" 2>/dev/null || true)
    local entry missing=0
    for entry in "HOME=$HOME_DIR" "XDG_CONFIG_HOME=$XDG_CONFIG_DIR" \
                 "XDG_DATA_HOME=$XDG_DATA_DIR" "XDG_STATE_HOME=$XDG_STATE_DIR" \
                 "XDG_CACHE_HOME=$XDG_CACHE_DIR" "TMPDIR=$TMP_DIR"; do
        if ! grep -Fxq -- "$entry" <<<"$envtext"; then
            fail "$label" "进程环境缺少 $entry"
            missing=1
        fi
    done
    if [[ -n $REAL_HOME ]] && grep -Fxq -- "HOME=$REAL_HOME" <<<"$envtext"; then
        fail "$label" "进程仍继承真实 HOME=$REAL_HOME"
        missing=1
    fi
    if ((missing == 0)); then
        pass "$label" "HOME/XDG_*/TMPDIR 全部在 $WORK 内"
    fi
}

check_loopback_only() { # <label> <pid> <port>
    local label=$1 pid=$2 port=$3
    if ! command -v ss >/dev/null 2>&1; then
        warn "$label" "无 ss 命令，跳过监听核对"
        return 0
    fi
    local lines
    lines=$(ss -ltnp 2>/dev/null | grep -F "pid=$pid," || true)
    if [[ -z $lines ]]; then
        warn "$label" "ss 未显示 pid=$pid 的监听（权限或竞态；跳过）"
        return 0
    fi
    local bad=0 line addr
    while IFS= read -r line; do
        [[ -n $line ]] || continue
        addr=$(awk '{print $4}' <<<"$line")
        if [[ $addr != "127.0.0.1:$port" ]]; then
            fail "$label" "非预期监听 $addr（只允许 127.0.0.1:$port）"
            bad=1
        fi
    done <<<"$lines"
    if ((bad == 0)); then
        pass "$label" "唯一监听 127.0.0.1:$port（无 0.0.0.0 / 无联邦端口）"
    fi
}

assert_stopped() { # <label> <pid>
    local label=$1 pid=$2
    if proc_running "$pid"; then
        fail "$label" "进程 $pid 仍在运行"
        return 1
    fi
    pass "$label" "PID $pid 已退出"
}

run_helper() {
    PYTHONDONTWRITEBYTECODE=1 HOME="$HOME_DIR" TMPDIR="$TMP_DIR" \
        timeout --foreground "$((TIMEOUT_SECS + 30))" \
        "$PYTHON_BIN" "$HELPER" "$@"
}

# ── 夹具 ───────────────────────────────────────────────────────────────────

CORE_PORT=$(free_port)
LLM_PORT=$(free_port)
while [[ $LLM_PORT == "$CORE_PORT" ]]; do
    LLM_PORT=$(free_port)
done
TOKEN=$(PYTHONDONTWRITEBYTECODE=1 "$PYTHON" -c 'import secrets; print(secrets.token_hex(16))')

write_fixtures() {
    cat >"$MODULES" <<EOF
# 仅本 smoke 使用：回环随机端口、无联邦/平台实例。
# 自更新与内置 Web Panel 显式关闭；工作区模块显式指到临时 state_dir。
[[module]]
id = "echo-agent.selfupdate"
kind = "builtin"
name = "self-update"
enabled = false

[[module]]
id = "echo-agent.panel.web"
kind = "builtin"
name = "Web Panel"
enabled = false

[[module]]
id = "echo-agent.workspace"
kind = "builtin"
name = "workspace"
[module.config]
state_dir = "$STATE_DIR"
EOF

    cat >"$CONFIG" <<EOF
# 仅本 smoke 使用：隔离 HOME/XDG、回环端口、无账号、无联邦 peer。
[logging]
level = "debug"
format = "text"
log_file = ""

[agent]
provider = "openai"
model = "smoke-model"
base_url = "http://127.0.0.1:$LLM_PORT/v1"
api_key = "smoke-local-only-key"
stream_output = false
max_tool_iterations = 3
tool_timeout_secs = 5
balance_snapshot_secs = 0
skills_dir = "$RUN_DIR/skills"
modules_dir = "$RUN_DIR/modules-ext"

[agent.teams.$TEAM]
name = "重启验收假团队"
description = "只用于进程重启隔离验收，不连接任何平台"
system_prompt = "你是隔离验收用假团队，只回复固定文本。"
enabled = true

[core]
management_address = "127.0.0.1:$CORE_PORT"
management_access_token = "$TOKEN"

[federation]
# 显式 "" = 不监听联邦端口；peers 为空 = 不会连接任何节点。
listen = ""
peers = {}

[security.sanitize]
enabled = true
EOF

    cat >"$WORKSPACE_FILE" <<EOF
{
  "active": "$WS_ID",
  "active_conversation": "$CONV_ID",
  "sessions": [
    {
      "id": "$WS_ID",
      "name": "$WS_NAME",
      "description": "isolated smoke fixture",
      "directories": ["$WORKSPACE_DIR"],
      "conversations": [
        {"id": "$CONV_ID", "name": "$CONV_NAME", "legacy": false}
      ]
    }
  ]
}
EOF

    cat >"$SESSION_FILE" <<EOF
{
  "version": 6,
  "header": null,
  "identities": [],
  "timeline": [],
  "events": [
    {"kind":"user_message","session":"$SESSION_ID","content":"$PRESEED_USER","timestamp":1700000000,"message_sequence":null,"source":null,"images":[]},
    {"kind":"assistant_message","session":"$SESSION_ID","content":"$PRESEED_ASSISTANT","timestamp":1700000001,"tool_calls":[]}
  ]
}
EOF
}

start_core() { # <label-log> <pidfile>
    local log=$1 pidfile=$2
    start_bg "$pidfile" "$log" \
        env -i PATH="$SAFE_PATH" HOME="$HOME_DIR" \
        XDG_CONFIG_HOME="$XDG_CONFIG_DIR" XDG_DATA_HOME="$XDG_DATA_DIR" \
        XDG_STATE_HOME="$XDG_STATE_DIR" XDG_CACHE_HOME="$XDG_CACHE_DIR" \
        TMPDIR="$TMP_DIR" LANG=C.UTF-8 \
        "$CORE_BIN" -c "$CONFIG"
}

# ── 主流程 ─────────────────────────────────────────────────────────────────

section "P0 前置与隔离环境"
printf '  Core 源码 : %s\n' "$CORE_SOURCE"
printf '  Core 二进制: %s\n' "$CORE_BIN"
printf '  sha256     : %s\n' "$(sha256_file "$CORE_BIN")"
printf '  mtime      : %s\n' "$(stat -c '%y' "$CORE_BIN" 2>/dev/null || echo unknown)"
printf '  临时根     : %s\n' "$WORK"
printf '  管理端口   : 127.0.0.1:%s\n' "$CORE_PORT"
printf '  假 LLM     : 127.0.0.1:%s\n' "$LLM_PORT"
if ! "$CORE_BIN" --version >"$LOG_DIR/version.txt" 2>&1; then
    fatal "P0.1 Core 二进制可执行" "运行 --version 失败（见 $LOG_DIR/version.txt）"
fi
printf '  版本       : %s\n' "$(head -1 "$LOG_DIR/version.txt")"
pass "P0.1 Core 二进制可执行" "$(head -1 "$LOG_DIR/version.txt")"
pass "P0.2 二进制含冷启动恢复路径" "strings 中可见 C3c 补种日志标记"
pass "P0.3 隔离环境" "独立 HOME/XDG_*/TMPDIR + 回环随机端口 + 无账号/无联邦"

write_fixtures
WS_HASH_BEFORE=$(sha256_file "$WORKSPACE_FILE")
pass "P0.4 持久化假团队与工作区" "workspace=$WS_ID/$CONV_ID team=$TEAM"

section "P1 启动回环假 LLM（无外部副作用）"
if ! start_bg "$LLM_PIDFILE" "$LLM_LOG" \
        env -i PATH="$SAFE_PATH" HOME="$HOME_DIR" TMPDIR="$TMP_DIR" LANG=C.UTF-8 \
        PYTHONDONTWRITEBYTECODE=1 "$PYTHON_BIN" "$HELPER" \
        serve-llm --port "$LLM_PORT" --reply "$FAKE_REPLY"; then
    fatal "P1.1 假 LLM 启动" "进程未写出 pidfile"
fi
LLM_SUP=$LAST_SUP
if ! wait_tcp "$LLM_PORT" 15; then
    fatal "P1.1 假 LLM 启动" "127.0.0.1:$LLM_PORT 未在 15s 内就绪（日志 $LLM_LOG）"
fi
if ! grep -Fq "READY 127.0.0.1:$LLM_PORT" "$LLM_LOG"; then
    fatal "P1.1 假 LLM 启动" "缺少 READY 行（日志 $LLM_LOG）"
fi
pass "P1.1 假 LLM 启动" "127.0.0.1:$LLM_PORT 固定回复 $FAKE_REPLY"

section "P2 Core#1 启动（全新进程）"
if ! start_core "$CORE1_LOG" "$CORE1_PIDFILE"; then
    fatal "P2.1 Core#1 启动" "进程未写出 pidfile（日志 $CORE1_LOG）"
fi
CORE1_SUP=$LAST_SUP
CORE1_PID=$(cat "$CORE1_PIDFILE")
if ! wait_tcp "$CORE_PORT" 45; then
    fatal "P2.1 Core#1 启动" "管理端口未在 45s 内就绪（日志 $CORE1_LOG）"
fi
proc_running "$CORE1_PID" || fatal "P2.1 Core#1 启动" "进程 $CORE1_PID 启动后立即退出"
pass "P2.1 Core#1 启动" "PID=$CORE1_PID，管理端口已就绪"

check_env_isolation "P2.2 Core#1 环境隔离" "$CORE1_PID"
check_loopback_only "P2.3 Core#1 仅回环监听" "$CORE1_PID" "$CORE_PORT"

RESTORE_LINE='workspace 镜像已由会话回执补种（冷启动恢复）'
if wait_log "$CORE1_LOG" "$RESTORE_LINE" 10; then
    pass "P2.4 Core#1 启动恢复日志" "出现冷启动补种"
else
    fatal "P2.4 Core#1 启动恢复日志" "10s 内未见『$RESTORE_LINE』（日志 $CORE1_LOG）"
fi

section "P3 Core#1 写入一条真实历史（经管理 WS）"
if ! SEND_OUT=$(run_helper send --port "$CORE_PORT" --token "$TOKEN" \
        --team "$TEAM" --session "$SESSION_ID" --content "$E2E_USER" \
        --timeout "$TIMEOUT_SECS" 2>&1); then
    printf '%s\n' "$SEND_OUT" >&2
    fatal "P3.1 SendMessage" "未等到 AgentCompleted（见上方输出）"
fi
printf '%s\n' "$SEND_OUT" | sed 's/^/  /'
if grep -Fq '"commands": ["SendMessage"]' <<<"$SEND_OUT"; then
    pass "P3.1 SendMessage" "Core#1 已完成该会话的 turn（客户端只发了 SendMessage）"
else
    fatal "P3.1 SendMessage" "输出缺少 SEND_OK / 命令集合不符"
fi
if ! grep -Fq "served POST /v1/chat/completions" "$LLM_LOG"; then
    fatal "P3.2 假 LLM 被调用" "日志里没有 POST /v1/chat/completions（日志 $LLM_LOG）"
fi
pass "P3.2 假 LLM 被调用" "回环假服务收到 chat/completions"

section "P4 Core#1 优雅停机 + 持久化"
if ! stop_group "$CORE1_PIDFILE" 30; then
    fatal "P4.1 Core#1 停机" "SIGTERM 后 30s 仍未退出"
fi
reap_sup "$CORE1_SUP"
assert_stopped "P4.1 Core#1 停机" "$CORE1_PID" || true
if ! grep -Fq "$E2E_USER" "$SESSION_FILE"; then
    fatal "P4.2 历史落盘" "echo-sessions-team-a.json 缺 Core#1 user 标记"
fi
if ! grep -Fq "$FAKE_REPLY" "$SESSION_FILE"; then
    fatal "P4.2 历史落盘" "echo-sessions-team-a.json 缺假 LLM assistant 标记"
fi
if ! grep -Fq "$PRESEED_USER" "$SESSION_FILE"; then
    fatal "P4.2 历史落盘" "预置 user 标记被覆盖/丢失"
fi
if ! grep -Fq "$PRESEED_ASSISTANT" "$SESSION_FILE"; then
    fatal "P4.2 历史落盘" "预置 assistant 标记被覆盖/丢失"
fi
pass "P4.2 历史落盘" "预置 + 新写入的 user/assistant 标记都在 echo-sessions-team-a.json"
WS_HASH_AFTER=$(sha256_file "$WORKSPACE_FILE")
if [[ $WS_HASH_BEFORE == "$WS_HASH_AFTER" ]]; then
    pass "P4.3 工作区文档未被改写" "sha256 前后一致 ${WS_HASH_BEFORE:0:12}…"
else
    fail "P4.3 工作区文档未被改写" "sha256 变化 ${WS_HASH_BEFORE:0:12}… → ${WS_HASH_AFTER:0:12}…"
fi

section "P5 Core#2 启动（全新 PID，同一份隔离数据）"
if ! start_core "$CORE2_LOG" "$CORE2_PIDFILE"; then
    fatal "P5.1 Core#2 启动" "进程未写出 pidfile（日志 $CORE2_LOG）"
fi
CORE2_SUP=$LAST_SUP
CORE2_PID=$(cat "$CORE2_PIDFILE")
if [[ $CORE2_PID == "$CORE1_PID" ]]; then
    fatal "P5.1 Core#2 启动" "PID 与 Core#1 相同，不是真正的新进程"
fi
if ! wait_tcp "$CORE_PORT" 45; then
    fatal "P5.1 Core#2 启动" "管理端口未在 45s 内就绪（日志 $CORE2_LOG）"
fi
proc_running "$CORE2_PID" || fatal "P5.1 Core#2 启动" "进程 $CORE2_PID 启动后立即退出"
pass "P5.1 Core#2 启动" "PID=$CORE2_PID（≠ Core#1 PID=$CORE1_PID）"

check_env_isolation "P5.2 Core#2 环境隔离" "$CORE2_PID"
check_loopback_only "P5.3 Core#2 仅回环监听" "$CORE2_PID" "$CORE_PORT"

# 关键：此时脚本还没有建立任何 WS 连接，也从未发送 RequestWorkspaceSessions。
if ! wait_log "$CORE2_LOG" "$RESTORE_LINE" 15; then
    fatal "P5.4 客户端连接前的启动恢复" "15s 内未见『$RESTORE_LINE』（日志 $CORE2_LOG）"
fi
RESTORE_EVIDENCE=$(grep -F "$RESTORE_LINE" "$CORE2_LOG" | tail -1)
printf '  %s\n' "$RESTORE_EVIDENCE"
if [[ $RESTORE_EVIDENCE == *"team_id=\"$TEAM\""* && \
      $RESTORE_EVIDENCE == *"active=Some(\"$WS_ID\")"* && \
      $RESTORE_EVIDENCE == *"sessions=1"* ]]; then
    pass "P5.4 客户端连接前的启动恢复" "日志已证明 Core#2 自行补种 active=$WS_ID"
else
    fail "P5.4 客户端连接前的启动恢复" "补种日志内容不符（见上方）"
fi
if grep -Fq "panel link up" "$CORE2_LOG"; then
    fail "P5.4b 客户端连接前无 Panel 链路" "日志已出现 panel link up，启动恢复证据被客户端连接污染"
else
    pass "P5.4b 客户端连接前无 Panel 链路" "Core#2 日志尚无 panel link up（脚本还没有 WS 客户端）"
fi
if wait_log "$CORE2_LOG" "workspace sessions restored team_id=$TEAM" 5; then
    pass "P5.5 模块回执到达" "workspace sessions restored team_id=$TEAM"
else
    warn "P5.5 模块回执到达" "未见 debug 行 workspace sessions restored（info 补种行已存在）"
fi

section "P6/P7 客户端只读校验（绝不发 RequestWorkspaceSessions）"
if ! PROBE_OUT=$(run_helper probe --port "$CORE_PORT" --token "$TOKEN" \
        --team "$TEAM" --session "$SESSION_ID" \
        --expect-nickname "$WS_NAME · $CONV_NAME" \
        --expect-user-marker "$PRESEED_USER" \
        --expect-user-marker "$E2E_USER" \
        --expect-assistant-marker "$PRESEED_ASSISTANT" \
        --expect-assistant-marker "$FAKE_REPLY" \
        --timeout "$TIMEOUT_SECS" 2>&1); then
    printf '%s\n' "$PROBE_OUT" >&2
    fatal "P6.1 RequestState/RequestSessionHistory" "probe 失败（见上方输出）"
fi
printf '%s\n' "$PROBE_OUT" | sed 's/^/  /'
if grep -Fq "PROBE_OK" <<<"$PROBE_OUT"; then
    pass "P6.1 激活工作区会话" "RequestState 返回 $SESSION_ID（team=$TEAM/scope=workspace）"
else
    fatal "P6.1 激活工作区会话" "输出缺少 PROBE_OK"
fi
if grep -Fq '"commands": ["RequestState", "RequestSessionHistory"]' <<<"$PROBE_OUT" && \
   ! grep -Fq "RequestWorkspaceSessions" <<<"$PROBE_OUT"; then
    pass "P6.2 未用客户端补拉掩盖恢复" "客户端命令精确为 [RequestState, RequestSessionHistory]"
else
    fail "P6.2 未用客户端补拉掩盖恢复" "命令集合不符或出现 RequestWorkspaceSessions"
fi
pass "P7.1 历史跨进程保留" \
    "预置 user/assistant + Core#1 写入 user + 假 LLM assistant 四类标记均在历史中"

section "P8 清理与残留检查"
if ! stop_group "$CORE2_PIDFILE" 30; then
    fatal "P8.1 Core#2 停机" "SIGTERM 后 30s 仍未退出"
fi
reap_sup "$CORE2_SUP"
assert_stopped "P8.1 Core#2 停机" "$CORE2_PID" || true
PORT_STILL_OURS=0
if command -v ss >/dev/null 2>&1; then
    if ss -ltnp 2>/dev/null | grep -Fq "pid=$CORE2_PID,"; then
        PORT_STILL_OURS=1
    fi
elif wait_tcp "$CORE_PORT" 2; then
    PORT_STILL_OURS=1
fi
if ((PORT_STILL_OURS)); then
    fail "P8.2 管理端口已释放" "Core#2 退出后仍持有 127.0.0.1:$CORE_PORT"
else
    pass "P8.2 管理端口已释放" "Core#2 已不持有 127.0.0.1:$CORE_PORT"
fi
if ! stop_group "$LLM_PIDFILE" 5; then
    fail "P8.3 假 LLM 已停止" "进程仍在运行"
else
    reap_sup "$LLM_SUP"
    pass "P8.3 假 LLM 已停止" "回环假服务已退出"
fi
if [[ -n $REAL_HOME && ( $WORK == "$REAL_HOME/.config/echo-agent-core"* || $WORK == "$REAL_HOME/.local/share/echo-agent-core"* ) ]]; then
    fail "P8.4 未触碰生产配置目录" "临时根落在了生产目录内：$WORK"
else
    pass "P8.4 未触碰生产配置目录" "所有运行数据都在独立临时根 $WORK"
fi

printf '\n复验命令（同一脚本，可加 --keep 保留日志）:\n'
printf '  bash %s --core-bin %s\n' "$0" "$CORE_BIN"

if ((FAILED > 0)); then
    finish 1
else
    finish 0
fi
