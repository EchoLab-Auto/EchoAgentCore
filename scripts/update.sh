#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
DATA_HOME=${XDG_DATA_HOME:-"$HOME/.local/share"}
STATE_HOME=${XDG_STATE_HOME:-"$HOME/.local/state"}
PREFIX=${ECHO_PREFIX:-"$HOME/.local"}

# Defaults mirror install.sh's installed layout, so a bare `scripts/update.sh`
# rebuilds and replaces the running installation. `--source` prefers this
# checkout when it has a Cargo.toml (covers dev worktrees with uncommitted
# changes), falling back to the managed source checkout.
DEFAULT_BINARY="$PREFIX/libexec/echo-agent-core/echo-agent-core-bin"
DEFAULT_STATUS_FILE="$STATE_HOME/echo-agent-core/update-status"
DEFAULT_SOURCE_DIR="$DATA_HOME/echo-agent-core/source"

SOURCE_DIR=""
BINARY=""
STATUS_FILE=""
UPDATE_MODE=local

usage() {
    cat <<'EOF'
Usage: scripts/update.sh [options]

Rebuild EchoAgentCore and atomically replace the installed binary, then
restart the Core service if it is running. After the restart the updater
verifies the new binary's built-in plugin manifests (plugin-aware update):
every built-in plugin id must be present in the running binary.

Options:
  --source <dir>   Source tree to build. Default: this checkout when it
                   contains Cargo.toml, else $HOME/.local/share/echo-agent-core/source
  --binary <path>  Installed binary to replace. Default:
                   $HOME/.local/libexec/echo-agent-core/echo-agent-core-bin
  --status <path>  Update status file. Default:
                   $HOME/.local/state/echo-agent-core/update-status
  --local          Build the source tree as-is, including uncommitted changes
                   (default). No fetch or merge is performed.
  --remote         Fetch and fast-forward the clean managed checkout before
                   building (requires network access and a clean checkout).
  -h, --help       Show this help
EOF
}

while (($#)); do
    case "$1" in
        --source)
            SOURCE_DIR=${2:-}
            shift 2
            ;;
        --binary)
            BINARY=${2:-}
            shift 2
            ;;
        --status)
            STATUS_FILE=${2:-}
            shift 2
            ;;
        --local)
            UPDATE_MODE=local
            shift
            ;;
        --remote)
            UPDATE_MODE=remote
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            exit 2
            ;;
    esac
done

if [[ -z "$SOURCE_DIR" ]]; then
    if [[ -f "$PROJECT_ROOT/Cargo.toml" ]]; then
        SOURCE_DIR="$PROJECT_ROOT"
    elif [[ -f "$DEFAULT_SOURCE_DIR/Cargo.toml" ]]; then
        SOURCE_DIR="$DEFAULT_SOURCE_DIR"
    else
        echo "error: --source is required (no Cargo.toml in $PROJECT_ROOT or $DEFAULT_SOURCE_DIR)" >&2
        usage >&2
        exit 2
    fi
fi
[[ -n "$BINARY" ]] || BINARY="$DEFAULT_BINARY"
[[ -n "$STATUS_FILE" ]] || STATUS_FILE="$DEFAULT_STATUS_FILE"

STATE_DIR=$(dirname -- "$STATUS_FILE")
REVISION_FILE="$STATE_DIR/installed-revision"
LOCK_FILE="$STATE_DIR/update.lock"
PHASE=initializing
TARGET=unknown
ROLLBACK_ACTIVE=0
BIN_REPLACED=0
UPDATER_REPLACED=0
ROLLBACK_BINARY=""
ROLLBACK_UPDATER=""
temporary=""
FINALIZED=0

write_status() {
    local state=$1 message=$2 revision=${3:-unknown}
    local temporary="$STATUS_FILE.tmp"
    {
        printf 'state=%s\n' "$state"
        printf 'revision=%s\n' "$revision"
        printf 'message=%s\n' "$message"
        printf 'pid=%s\n' "$$"
        printf 'updated_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } >"$temporary"
    mv -f "$temporary" "$STATUS_FILE"
}

# Restore the previous binary / updater / revision and restart Core if it was
# active before this update. Best-effort: every step tolerates failure.
rollback_installation() {
    if [[ -n "$ROLLBACK_BINARY" && -x "$ROLLBACK_BINARY" ]]; then
        mv -f "$ROLLBACK_BINARY" "$BINARY" || true
    elif ((BIN_REPLACED)); then
        rm -f "$BINARY" || true
    fi
    if [[ -n "$ROLLBACK_UPDATER" && -f "$ROLLBACK_UPDATER" ]]; then
        mv -f "$ROLLBACK_UPDATER" "$(dirname -- "$BINARY")/update.sh" || true
    elif ((UPDATER_REPLACED)); then
        rm -f "$(dirname -- "$BINARY")/update.sh" || true
    fi
    if [[ -n "${installed:-}" ]]; then
        revision_restore="${REVISION_FILE}.restore.$$"
        printf '%s\n' "$installed" >"$revision_restore"
        mv -f "$revision_restore" "$REVISION_FILE" || true
    else
        rm -f "$REVISION_FILE" || true
    fi
    if (( ${core_was_active:-0} )); then
        systemctl --user restart echo-agent-core.service >/dev/null 2>&1 || true
    fi
}

# Record a definitive failure state: roll back when a binary/updater was
# already replaced, otherwise just mark the run as failed. Idempotent.
finalize_failure() {
    local code=$1 reason=$2
    if ((FINALIZED == 1)); then
        return
    fi
    FINALIZED=1
    trap - ERR EXIT
    set +e
    if ((BIN_REPLACED || UPDATER_REPLACED)); then
        ROLLBACK_ACTIVE=1
        rollback_installation
        write_status rolled_back "$reason; restored previous installation" "${installed:-unknown}" || true
    else
        write_status failed "$reason" "${TARGET:-unknown}" || true
    fi
}

on_error() {
    local code=$?
    finalize_failure "$code" "update failed during $PHASE"
    exit "$code"
}
trap on_error ERR

# EXIT trap: always leave a terminal state. Covers signals (SIGTERM from
# systemd timeouts, Ctrl+C, killed tool sessions) and any path that bypassed
# the ERR trap, so the status file can never stay stuck at "running" while
# systemd reports a failure.
on_exit() {
    local code=$?
    trap - EXIT ERR
    if [[ -n "$temporary" ]]; then
        rm -f "$temporary" 2>/dev/null || true
    fi
    if ((FINALIZED == 0)) && ((code != 0)); then
        set +e
        if ((BIN_REPLACED || UPDATER_REPLACED)); then
            ROLLBACK_ACTIVE=1
            rollback_installation
            write_status rolled_back "update interrupted (exit $code) during $PHASE; restored previous installation" "${installed:-unknown}" || true
        else
            write_status failed "update interrupted (exit $code) during $PHASE" "${TARGET:-unknown}" || true
        fi
    fi
}
trap on_exit EXIT

install -d -m 755 "$STATE_DIR" "$(dirname -- "$BINARY")"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
    write_status busy "another update is already running"
    FINALIZED=1
    exit 0
fi

# Holding the lock means no other update is running, so a leftover "running"
# state belongs to an interrupted attempt. Record it before continuing.
if [[ -f "$STATUS_FILE" ]] && grep -q '^state=running' "$STATUS_FILE" 2>/dev/null; then
    write_status interrupted "previous update was interrupted; starting a new attempt" "$(cat "$REVISION_FILE" 2>/dev/null || true)"
fi
installed=$(cat "$REVISION_FILE" 2>/dev/null || true)
core_was_active=0
if systemctl --user is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi

for command in cargo git install systemctl; do
    command -v "$command" >/dev/null 2>&1 || {
        write_status failed "required command not found: $command"
        FINALIZED=1
        exit 1
    }
done
if [[ ! -f "$SOURCE_DIR/Cargo.toml" ]]; then
    write_status failed "source Cargo.toml is missing"
    FINALIZED=1
    exit 1
fi
current=$(git -C "$SOURCE_DIR" rev-parse HEAD 2>/dev/null || echo unknown)
if [[ "$UPDATE_MODE" == local ]]; then
    TARGET="local:$current"
    if ! git -C "$SOURCE_DIR" diff --quiet 2>/dev/null \
        || ! git -C "$SOURCE_DIR" diff --cached --quiet 2>/dev/null \
        || [[ -n "$(git -C "$SOURCE_DIR" ls-files --others --exclude-standard 2>/dev/null)" ]]; then
        TARGET+="+dirty"
    fi
    write_status running "building local source tree" "$TARGET"
else
    if [[ ! -d "$SOURCE_DIR/.git" ]]; then
        write_status failed "managed source checkout is missing"
        FINALIZED=1
        exit 1
    fi
    if ! git -C "$SOURCE_DIR" diff --quiet || ! git -C "$SOURCE_DIR" diff --cached --quiet; then
        write_status failed "managed source has tracked local changes; refusing to overwrite"
        FINALIZED=1
        exit 1
    fi

    branch=$(git -C "$SOURCE_DIR" branch --show-current)
    if [[ -z "$branch" ]]; then
        write_status failed "managed source is on a detached HEAD"
        FINALIZED=1
        exit 1
    fi

    PHASE=fetching
    write_status running "fetching origin/$branch" "$current"
    git -C "$SOURCE_DIR" fetch --prune origin "$branch"
    TARGET=$(git -C "$SOURCE_DIR" rev-parse FETCH_HEAD)
    if ! git -C "$SOURCE_DIR" merge-base --is-ancestor "$current" "$TARGET"; then
        write_status failed "origin/$branch is not a fast-forward" "$current"
        FINALIZED=1
        exit 1
    fi
    git -C "$SOURCE_DIR" merge --ff-only "$TARGET"

    if [[ "$installed" == "$TARGET" && -x "$BINARY" ]]; then
        write_status current "already up to date" "$TARGET"
        FINALIZED=1
        exit 0
    fi
fi

PHASE=building
write_status running "building revision $TARGET" "$TARGET"
cargo build --release --locked -p echo-agent-core --manifest-path "$SOURCE_DIR/Cargo.toml"

PHASE=installing
temporary=$(mktemp "${BINARY}.new.XXXXXX")
install -m 755 "$SOURCE_DIR/target/release/echo-agent-core" "$temporary"
rollback="${BINARY}.rollback"
ROLLBACK_BINARY="$rollback"
if [[ -x "$BINARY" ]]; then
    rollback_temporary=$(mktemp "${rollback}.new.XXXXXX")
    install -m 755 "$BINARY" "$rollback_temporary"
    mv -f "$rollback_temporary" "$rollback"
fi
mv -f "$temporary" "$BINARY"
BIN_REPLACED=1
temporary=""

if ((core_was_active)); then
    PHASE=restarting
    if ! systemctl --user restart echo-agent-core.service; then
        restart_failed=1
    else
        restart_failed=1
        healthy_checks=0
        for _ in {1..15}; do
            sleep 1
            if systemctl --user is-active --quiet echo-agent-core.service; then
                ((healthy_checks += 1))
                if ((healthy_checks >= 3)); then
                    restart_failed=0
                    break
                fi
            else
                healthy_checks=0
            fi
        done
    fi
    if ((restart_failed)); then
        false
    fi

    # ── QQ/NapCat 恢复：方案 1 —— NapCat 自带重连（reconnectInterval 5s），
    #    Core 重启后让它自己重连，尽量不动 NapCat 进程（动它就等于动登录）。
    #    等待窗口 = 5 个重连周期（25s）+ 2 个心跳探测 + 余量 ≈ 40s；
    #    仅当超过窗口仍未连上才 docker restart（最后的兜底）。 ──
    PHASE=restoring_qq
    qq_connected() {
        command -v ss >/dev/null 2>&1 || return 1
        ss -Htn state established '( sport = :3131 )' 2>/dev/null | grep -q .
    }
    if command -v docker >/dev/null 2>&1 && docker ps --format '{{.Names}}' 2>/dev/null | grep -qx napcat; then
        qq_ok=0
        echo "等待 NapCat 自动重连（reconnectInterval=5s，窗口 40s）…"
        for _ in {1..40}; do
            if qq_connected; then
                qq_ok=1
                break
            fi
            sleep 1
        done
        if ((qq_ok == 0)); then
            echo "40s 内 NapCat 未自动重连，执行 docker restart 兜底（登录态保存在卷中，会自动恢复）" >&2
            docker restart napcat >/dev/null 2>&1 || true
            for _ in {1..90}; do
                if qq_connected; then
                    qq_ok=1
                    break
                fi
                sleep 1
            done
        fi
        if ((qq_ok == 1)); then
            echo "QQ 连接已恢复（NapCat 自动重连成功）"
        else
            echo "警告：等待 QQ 反向 WS 恢复超时，可能需要手动检查 napcat 容器" >&2
        fi
    fi
fi

if [[ -f "$SOURCE_DIR/scripts/update.sh" ]]; then
    PHASE=refreshing_updater
    updater="$(dirname -- "$BINARY")/update.sh"
    updater_temporary=$(mktemp "${updater}.new.XXXXXX")
    if [[ -f "$updater" ]]; then
        ROLLBACK_UPDATER="${updater}.rollback"
        updater_rollback_temporary=$(mktemp "${ROLLBACK_UPDATER}.new.XXXXXX")
        install -m 755 "$updater" "$updater_rollback_temporary"
        mv -f "$updater_rollback_temporary" "$ROLLBACK_UPDATER"
    fi
    install -m 755 "$SOURCE_DIR/scripts/update.sh" "$updater_temporary"
    mv -f "$updater_temporary" "$updater"
    UPDATER_REPLACED=1
fi

PHASE=committing
revision_temporary=$(mktemp "${REVISION_FILE}.new.XXXXXX")
printf '%s\n' "$TARGET" >"$revision_temporary"
mv -f "$revision_temporary" "$REVISION_FILE"
# ── 插件感知校验：确认新二进制内含全部内置插件 manifest（id/kind）。 ──
# 校验失败不视为更新失败（二进制已替换并运行），仅警告并记录。
PHASE=verifying_plugins
missing_plugins=""
if [[ -x "$BINARY" ]]; then
    expected_ids="echo-agent.tools.builtin echo-agent.adapter.qq echo-agent.skills.dir echo-agent.orchestration echo-agent.provider.llm echo-agent.loop.runner echo-agent.management.panel"
    for id in $expected_ids; do
        if ! strings "$BINARY" | grep -q "$id"; then
            missing_plugins="$missing_plugins $id"
        fi
    done
fi
if [[ -n "$missing_plugins" ]]; then
    echo "警告：新二进制缺少内置插件 manifest：$missing_plugins（请检查构建产物）" >&2
    echo "missing_plugins=$missing_plugins" >>"$STATUS_FILE"
fi

if [[ "$UPDATE_MODE" == local ]]; then
    write_status updated "installed successfully from local source" "$TARGET"
else
    write_status updated "updated successfully" "$TARGET"
fi
FINALIZED=1
BIN_REPLACED=0
UPDATER_REPLACED=0
rm -f "$rollback" "$ROLLBACK_UPDATER" || true
