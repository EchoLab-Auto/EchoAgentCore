#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
# 路径按「装着这份安装的那个用户」推导，与 install.sh 的 derive_paths 同形。
# system 作用域下安装属于 root（install.sh 只允许 root），所以卸载也要从 root
# 的家目录算——否则以普通用户身份执行 `--scope system --dry-run` 会打印出
# 调用者自己的路径，看着像"要删错东西"。
derive_paths() {
    local home=$1 use_xdg=$2
    PREFIX=${ECHO_PREFIX:-"$home/.local"}
    if ((use_xdg)); then
        CONFIG_HOME=${XDG_CONFIG_HOME:-"$home/.config"}
        DATA_HOME=${XDG_DATA_HOME:-"$home/.local/share"}
        STATE_HOME=${XDG_STATE_HOME:-"$home/.local/state"}
    else
        CONFIG_HOME="$home/.config"
        DATA_HOME="$home/.local/share"
        STATE_HOME="$home/.local/state"
    fi
    CONFIG_DIR="$CONFIG_HOME/echo-agent-core"
    DATA_DIR="$DATA_HOME/echo-agent-core"
    STATE_DIR="$STATE_HOME/echo-agent-core"
    LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core"
    BIN_DIR="$PREFIX/bin"
    SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
}
derive_paths "$HOME" 1
# system 作用域（`install.sh --scope system` 的装法，以及存量手工装法）。两种
# 都要处理：只删自己那种、把另一种静默留下，会得到"卸载了却还在开机重启"。
SYSTEM_UNIT_DIR=/etc/systemd/system

# 本实例装在哪个作用域。显式 --scope > 环境变量 > 单元文件探测
# （文件存在才算：服务停着时 is-active 两个作用域都是 inactive，探不出来）。
SYSTEM_SCOPE_UNIT="$SYSTEM_UNIT_DIR/echo-agent-core.service"
SERVICE_SCOPE=${ECHO_SERVICE_SCOPE:-}
if [[ -z "$SERVICE_SCOPE" ]]; then
    if [[ -e "$SYSTEM_SCOPE_UNIT" ]]; then
        SERVICE_SCOPE=system
    else
        SERVICE_SCOPE=user
    fi
fi

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
  --scope <user|system> Which systemd scope the installation lives in
                        (default: auto-detected from where the unit files are;
                        ECHO_SERVICE_SCOPE is honoured too).

                        `system` needs root and removes the units from
                        /etc/systemd/system with plain `systemctl`.
                        Both scopes leave the *other* scope's leftover units
                        alone unless they are the detected one -- but a stale
                        unit from a previous layout is still cleaned up when
                        running as root, because it would keep trying to start
                        a binary that no longer exists.

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
        --scope)
            SERVICE_SCOPE=${2:-}
            shift 2
            ;;
        --scope=*)
            SERVICE_SCOPE=${1#--scope=}
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

case "$SERVICE_SCOPE" in
    user|system) ;;
    *)
        echo "error: --scope must be 'user' or 'system' (got: '$SERVICE_SCOPE')" >&2
        exit 2
        ;;
esac

if [[ "$SERVICE_SCOPE" == system ]]; then
    # 安装属于 root，路径也要从 root 的家目录算（见 derive_paths 注释）。
    SERVICE_HOME=$(getent passwd root 2>/dev/null | cut -d: -f6)
    derive_paths "${SERVICE_HOME:-/root}" 0
    # system 作用域的单元在 /etc/systemd/system，只有 root 能停/禁/删。
    # --dry-run 例外：只打印计划，不动系统。
    if ((EUID != 0)) && ((!DRY_RUN)); then
        echo "error: --scope system requires root (units live in $SYSTEM_UNIT_DIR)" >&2
        echo "       re-run as: sudo $0 --scope system" >&2
        exit 1
    fi
    systemd_available=1
    [[ -x /run/systemd/system ]] || systemd_available=0
else
    systemd_available=0
    if systemctl --user show-environment >/dev/null 2>&1; then
        systemd_available=1
    fi
fi

# ── 环境探测（dry-run 与真实卸载共用同一份判断）─────────────────────
# linger 状态：/var/lib/systemd/linger/<user> 存在即开启（与 install.sh 的
# 成功判据同源，不额外解析 loginctl 输出）。
current_user=$(id -un)
linger_on=0
if [[ -e "/var/lib/systemd/linger/$current_user" ]]; then
    linger_on=1
fi

# system 作用域单元：可能是本实例（--scope system 装的），也可能是历史残留
# （手工/旧脚本装的）。两种都得清——留着它们，下次开机 systemd 会去拉起一个
# 二进制已被删除的服务。
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
    if [[ "$SERVICE_SCOPE" == system ]]; then
        units="$SYSTEM_UNIT_DIR/{echo-agent-core.service,echo-agent-core-update.service} (system scope)"
    elif ((systemd_available)); then
        units="$SYSTEMD_DIR/echo-agent-core.service, echo-agent-core-update.service (user scope)"
    else
        units="systemd user manager unavailable (units skipped)"
    fi
    if [[ "$SERVICE_SCOPE" == system ]]; then
        system_units="$units"
        units="— (installed in system scope)"
    fi
    if ((${#system_scope_units[@]})); then
        [[ "$SERVICE_SCOPE" == system ]] || system_units="$SYSTEM_UNIT_DIR/{${system_scope_units[*]}} (leftover, system scope)"
    elif [[ "$SERVICE_SCOPE" != system ]]; then
        system_units="none"
    fi
    cat <<EOF
EchoAgentCore uninstall plan
  scope:     $SERVICE_SCOPE
  systemd:   $units
  system:    $system_units
  launcher:  $BIN_DIR/echo-agent-core
  libexec:   $LIBEXEC_DIR
  data:      $DATA_DIR ($( ((PURGE)) && echo "will be removed" || echo "kept: media, skills — removed: source, skills-builtin" ))
  state:     $STATE_DIR
  config:    $CONFIG_DIR ($( ((PURGE)) && echo "will be removed" || echo "kept — use --purge to remove" ))
  linger:    $( [[ "$SERVICE_SCOPE" == system ]] && echo "not applicable (system scope)" || linger_note )
EOF
    exit 0
fi

# ── user 作用域单元 ────────────────────────────────────────────────
# Stop and disable the services before removing their units and binary, so a
# running Core cannot hold the removed files open or restart mid-uninstall.
if [[ "$SERVICE_SCOPE" == user ]]; then
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
fi

# ── system 作用域单元（本实例，或历史装法的残留）──────────────────────
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

if [[ "$SERVICE_SCOPE" == user ]] && ((systemd_available)); then
    for unit in "${USERS_INSTALLED_UNITS[@]}"; do
        remove_file "$SYSTEMD_DIR/$unit" "systemd unit"
    done
    systemctl --user daemon-reload || true
fi

# ── linger ────────────────────────────────────────────────────────
# system 作用域与 linger 无关（单元挂 multi-user.target，开机即起、与会话无关），
# 此时既不改它、也不报告它——报一句"linger 仍开着"只会让人误以为要处理。
# 只在显式要求时关，且关之前再确认一次它确实属于"本用户"（永远如此，
# 因为 install 用的是 `id -un`）——不做跨用户操作。
if [[ "$SERVICE_SCOPE" == system ]]; then
    :
elif ((DISABLE_LINGER)); then
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
if [[ "$SERVICE_SCOPE" == user ]] && ((linger_on)) && ((!DISABLE_LINGER)); then
    echo "  linger still enabled for $current_user — it keeps /run/user/$UID alive"
    echo "  without a login session. Other user services may depend on it;"
    echo "  turn it off with: loginctl disable-linger $current_user"
fi
if [[ "$SERVICE_SCOPE" == system ]]; then
    echo "  Reinstall with: sudo scripts/install.sh --scope system"
else
    echo "  Reinstall with: scripts/install.sh"
fi
