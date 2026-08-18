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

PURGE=0
DRY_RUN=0

usage() {
    cat <<'EOF'
Usage: scripts/uninstall.sh [options]

Remove the EchoAgentCore installation for the current user: the systemd units,
the launcher and the runtime binary, the managed source checkout, and the
update state. The configuration directory (core.toml and session history) is
kept by default so a reinstall restores everything; pass --purge to remove it
as well.

Options:
  --purge      Also remove the config directory ($CONFIG_HOME/echo-agent-core)
  --dry-run    Print resolved paths without changing the system
  -h, --help   Show this help
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

if ((DRY_RUN)); then
    if ((systemd_available)); then
        units="$SYSTEMD_DIR/echo-agent-core.service, echo-agent-core-update.service"
    else
        units="systemd user manager unavailable (units skipped)"
    fi
    cat <<EOF
EchoAgentCore uninstall plan
  systemd:   $units
  launcher:  $BIN_DIR/echo-agent-core
  libexec:   $LIBEXEC_DIR
  data:      $DATA_DIR
  state:     $STATE_DIR
  config:    $CONFIG_DIR ($( ((PURGE)) && echo "will be removed" || echo "kept — use --purge to remove" ))
EOF
    exit 0
fi

# Stop and disable the services before removing their units and binary, so a
# running Core cannot hold the removed files open or restart mid-uninstall.
if ((systemd_available)); then
    for unit in echo-agent-core.service echo-agent-core-update.service; do
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
remove_dir "$DATA_DIR" "managed source checkout"
remove_dir "$STATE_DIR" "update state"

if ((systemd_available)); then
    remove_file "$SYSTEMD_DIR/echo-agent-core.service" "systemd unit"
    remove_file "$SYSTEMD_DIR/echo-agent-core-update.service" "systemd unit"
    systemctl --user daemon-reload || true
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
fi
echo "  Reinstall with: scripts/install.sh"
