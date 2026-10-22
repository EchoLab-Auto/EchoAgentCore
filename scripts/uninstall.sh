#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
PREFIX=${ECHO_PREFIX:-"$HOME/.local"}
CONFIG_HOME=${XDG_CONFIG_HOME:-"$HOME/.config"}
DATA_HOME=${XDG_DATA_HOME:-"$HOME/.local/share"}
STATE_HOME=${XDG_STATE_HOME:-"$HOME/.local/state"}
CONFIG_DIR="$CONFIG_HOME/echo-agent-core"
DATA_DIR="$DATA_HOME/echo-agent-core"
STATE_DIR="$STATE_HOME/echo-agent-core"
LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core"
BIN_DIR="$PREFIX/bin"
SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
# system 作用域（多节点/服务器装法）：install.sh 只写 user 作用域，但存量机器
# 可能是手工或旧脚本装的 system 单元（如 cloud-server-2）。只探测、不猜。
SYSTEM_UNIT_DIR=/etc/systemd/system

PURGE=0
DRY_RUN=0
# linger 默认**不动**：它是**用户级全局**设置，可能被这台机器上别的用户服务
# （用户自己跑的守护进程）共用——卸载本服务时顺手关掉，会连带让那些服务在
# 最后一个登录会话结束时静默死亡，而这里既不知道有没有这样的服务，也无法
# 事后补偿。默认只报告状态；要关必须显式 --disable-linger。
DISABLE_LINGER=0

USERS_INSTALLED_UNITS=(echo-agent-core.service echo-agent-core-update.service)

usage() {
    cat <<'EOF'
Usage: scripts/uninstall.sh [options]

Remove the EchoAgentCore installation for the current user: the systemd units,
the launcher and the runtime binary, the managed source checkout, and the
update state. The configuration directory (core.toml and session history) is
kept by default so a reinstall restores everything; pass --purge to remove it
as well. User data under the data directory (media library, user skills layer)
is always preserved unless --purge.

Options:
  --purge            Also remove the config directory ($CONFIG_HOME/echo-agent-core)
  --dry-run          Print resolved paths without changing the system
  --disable-linger   Also turn off linger for this user.

                     Installer enables linger so the service survives logout
                     and starts at boot. Uninstall leaves it ON by default:
                     linger is a **per-user, machine-wide** switch, so other
                     user services on this machine may rely on it. Turning it
                     off would make them stop when your last session ends.
                     Use this flag only if nothing else here needs it.

  -h, --help         Show this help
EOF
}

while (($#)); do
    case "$1" in
        --purge)
            PURGE=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --disable-linger)
            DISABLE_LINGER=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

systemd_available=0
if systemctl --user show-environment >/dev/null 2>&1; then
    systemd_available=1
fi

# ── 环境探测（dry-run 与真实卸载共用同一份判断）─────────────────────
# linger 状态：/var/lib/systemd/linger/<user> 存在即开启（与 install.sh 的
# 成功判据同源，不额外解析 loginctl 输出）。
current_user=$(id -un)
linger_on=0
if [[ -e "/var/lib/systemd/linger/$current_user" ]]; then
    linger_on=1
fi

# system 作用域单元：存在即说明这台机器不是按 install.sh 的默认路径装的。
system_scope_units=()
for unit in "${USERS_INSTALLED_UNITS[@]}"; do
    if [[ -e "$SYSTEM_UNIT_DIR/$unit" ]]; then
        system_scope_units+=("$unit")
    fi
done

linger_note() {
    if ((!linger_on)); then
        echo "not enabled — nothing to clean up"
    elif ((DISABLE_LINGER)); then
        echo "enabled — will be turned OFF (--disable-linger)"
    else
        echo "enabled — kept (per-user setting may serve other services; --disable-linger to turn off)"
    fi
}

if ((DRY_RUN)); then
    if ((systemd_available)); then
        units="$SYSTEMD_DIR/echo-agent-core.service, echo-agent-core-update.service"
    else
        units="systemd user manager unavailable (units skipped)"
    fi
    if ((${#system_scope_units[@]})); then
        system_units="$SYSTEM_UNIT_DIR/{${system_scope_units[*]}} (system scope)"
    else
        system_units="none"
    fi
    cat <<EOF
EchoAgentCore uninstall plan
  systemd:   $units
  system:    $system_units
  launcher:  $BIN_DIR/echo-agent-core
  libexec:   $LIBEXEC_DIR
  data:      $DATA_DIR ($( ((PURGE)) && echo "will be removed" || echo "kept: media, skills — removed: source, skills-builtin" ))
  state:     $STATE_DIR
  config:    $CONFIG_DIR ($( ((PURGE)) && echo "will be removed" || echo "kept — use --purge to remove" ))
  linger:    $(linger_note)
EOF
    exit 0
fi

# ── user 作用域单元 ────────────────────────────────────────────────
# Stop and disable the services before removing their units and binary, so a
# running Core cannot hold the removed files open or restart mid-uninstall.
if ((systemd_available)); then
    for unit in "${USERS_INSTALLED_UNITS[@]}"; do
        if systemctl --user is-active --quiet "$unit" 2>/dev/null; then
            echo "==> Stopping $unit"
            systemctl --user stop "$unit" || true
        fi
        if systemctl --user is-enabled --quiet "$unit" 2>/dev/null; then
            echo "==> Disabling $unit"
            systemctl --user disable "$unit" || true
        fi
    done
else
    echo "warning: systemd user manager unavailable; leaving any unit files in place" >&2
fi

# ── system 作用域单元（存量服务器装法）────────────────────────────
# 这里**不静默跳过**：留着它们，下次开机 systemd 会去拉起一个已经被删掉
# 二进制的服务，表现为"卸载了却还在崩溃重启"。以 root 运行则直接清；否则
# 打印出可直接复制的命令（不擅自 sudo，也不假装成功）。
if ((${#system_scope_units[@]})); then
    if ((EUID == 0)); then
        for unit in "${system_scope_units[@]}"; do
            echo "==> Stopping system unit $unit"
            systemctl stop "$unit" 2>/dev/null || true
            systemctl disable "$unit" 2>/dev/null || true
        done
        for unit in "${system_scope_units[@]}"; do
            if [[ -e "$SYSTEM_UNIT_DIR/$unit" ]]; then
                echo "==> Removing system unit: $SYSTEM_UNIT_DIR/$unit"
                rm -f -- "$SYSTEM_UNIT_DIR/$unit"
            fi
        done
        systemctl daemon-reload 2>/dev/null || true
    else
        echo "warning: system-scope units present but this script is not running as root:" >&2
        for unit in "${system_scope_units[@]}"; do
            echo "         $SYSTEM_UNIT_DIR/$unit" >&2
        done
        echo "         Clean them up with (otherwise systemd keeps trying to start them):" >&2
        for unit in "${system_scope_units[@]}"; do
            echo "           sudo systemctl disable --now $unit && sudo rm -f $SYSTEM_UNIT_DIR/$unit" >&2
        done
        echo "           sudo systemctl daemon-reload" >&2
    fi
fi

remove_dir() {
    local dir=$1 label=$2
    if [[ -n "$dir" && "$dir" != "/" && "$dir" != "$HOME" && -d "$dir" ]]; then
        echo "==> Removing $label: $dir"
        rm -rf -- "$dir"
    fi
}

remove_file() {
    local file=$1 label=$2
    if [[ -n "$file" && -e "$file" ]]; then
        echo "==> Removing $label: $file"
        rm -f -- "$file"
    fi
}

remove_file "$BIN_DIR/echo-agent-core" "launcher"
remove_dir "$LIBEXEC_DIR" "runtime libexec"
remove_dir "$STATE_DIR" "update state"

# 数据目录（2026-10 修复）：只删受管产物（源码检出 + 出厂技能层）。
# 用户数据——媒体库 media/ 与用户技能层 skills/——即使不带 --purge 也绝不
# 触碰（uninstall 的契约是"保留配置"，用户数据同理；此前 rm -rf 整个
# DATA_DIR 会连带清除它们，且 install 不会恢复）。--purge 才整体移除。
if ((PURGE)); then
    remove_dir "$DATA_DIR" "data directory (checkout, skills, media)"
else
    remove_dir "$DATA_DIR/source" "managed source checkout"
    remove_dir "$DATA_DIR/skills-builtin" "factory skills layer"
    if [[ -d "$DATA_DIR" ]]; then
        echo "==> Keeping user data: $DATA_DIR (media / skills; use --purge to remove)"
    fi
fi

if ((systemd_available)); then
    for unit in "${USERS_INSTALLED_UNITS[@]}"; do
        remove_file "$SYSTEMD_DIR/$unit" "systemd unit"
    done
    systemctl --user daemon-reload || true
fi

# ── linger ────────────────────────────────────────────────────────
# 只在显式要求时关，且关之前再确认一次它确实属于"本用户"（永远如此，
# 因为 install 用的是 `id -un`）——不做跨用户操作。
if ((DISABLE_LINGER)); then
    if ((linger_on)); then
        echo "==> Disabling linger for $current_user"
        if loginctl disable-linger "$current_user" 2>/dev/null &&
            [[ ! -e "/var/lib/systemd/linger/$current_user" ]]; then
            linger_on=0
        else
            echo "warning: could not disable linger for $current_user" >&2
            echo "         fix: loginctl disable-linger $current_user" >&2
        fi
    fi
else
    if ((linger_on)); then
        echo "==> Keeping linger for $current_user (per-user setting; --disable-linger to turn off)"
    fi
fi

if ((PURGE)); then
    remove_dir "$CONFIG_DIR" "config directory"
else
    echo "==> Keeping config: $CONFIG_DIR (re-run with --purge to remove)"
fi

echo
echo "Uninstall complete."
if ((!PURGE)); then
    echo "  Config and session history kept at: $CONFIG_DIR"
    echo "  User data (media, skills) kept at:  $DATA_DIR"
fi
if ((linger_on)) && ((!DISABLE_LINGER)); then
    echo "  linger still enabled for $current_user — it keeps /run/user/$UID alive"
    echo "  without a login session. Other user services may depend on it;"
    echo "  turn it off with: loginctl disable-linger $current_user"
fi
echo "  Reinstall with: scripts/install.sh"
