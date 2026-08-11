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
SOURCE_DIR="$DATA_DIR/source"
LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core"
BIN_DIR="$PREFIX/bin"
SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
CORE_CONFIG="$CONFIG_DIR/core.toml"
STATUS_FILE="$STATE_DIR/update-status"
REPOSITORY_URL=${ECHO_REPOSITORY_URL:-}
OWNER_QQ=""
START_SERVICE=1
DRY_RUN=0

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [options]

Install EchoAgentCore for the current user and run Core as a systemd service.
The Panel (TUI) frontend lives in the separate EchoAgentPanel repository and
is installed by its own installer.

Options:
  --owner-qq <number>   Set the QQ owner and authorize it for self-update
  --repository <url>    Upstream Git repository used by self-update
  --no-start            Install and enable units without starting Core
  --dry-run             Print resolved paths without changing the system
  -h, --help            Show this help
EOF
}

while (($#)); do
    case "$1" in
        --owner-qq)
            OWNER_QQ=${2:-}
            shift 2
            ;;
        --repository)
            REPOSITORY_URL=${2:-}
            shift 2
            ;;
        --no-start)
            START_SERVICE=0
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

if [[ -n "$OWNER_QQ" && ! "$OWNER_QQ" =~ ^[0-9]+$ ]]; then
    echo "error: --owner-qq must be a numeric QQ ID" >&2
    exit 2
fi

if [[ -z "$REPOSITORY_URL" ]]; then
    REPOSITORY_URL=$(git -C "$PROJECT_ROOT" remote get-url origin 2>/dev/null || true)
fi
if [[ "$REPOSITORY_URL" =~ ^git@([^:]+):(.+)$ ]]; then
    REPOSITORY_URL="https://${BASH_REMATCH[1]}/${BASH_REMATCH[2]}"
fi
if [[ -z "$REPOSITORY_URL" ]]; then
    REPOSITORY_URL="https://gitlab.com/echo_tech/EchoAgentCore.git"
fi

BRANCH=$(git -C "$PROJECT_ROOT" branch --show-current 2>/dev/null || true)
BRANCH=${BRANCH:-main}

if ((DRY_RUN)); then
    cat <<EOF
EchoAgentCore install plan
  project:    $PROJECT_ROOT
  source:     $SOURCE_DIR
  binary:     $LIBEXEC_DIR/echo-agent-core-bin
  config:     $CONFIG_DIR
  state:      $STATE_DIR
  systemd:    $SYSTEMD_DIR
  repository: $REPOSITORY_URL
  branch:     $BRANCH
  start Core: $START_SERVICE
EOF
    exit 0
fi

for command in cargo git install sed systemctl flock cp stat; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "error: required command not found: $command" >&2
        exit 1
    fi
done

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: the service installer currently supports Linux/systemd only" >&2
    exit 1
fi
if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "error: systemd user manager is unavailable for this user" >&2
    exit 1
fi

install -d -m 755 "$BIN_DIR" "$LIBEXEC_DIR" "$CONFIG_DIR" "$DATA_DIR" "$STATE_DIR" "$SYSTEMD_DIR"

if [[ ! -d "$SOURCE_DIR/.git" ]]; then
    echo "==> Creating managed source checkout"
    git clone --local --no-hardlinks --branch "$BRANCH" "$PROJECT_ROOT" "$SOURCE_DIR"
    git -C "$SOURCE_DIR" remote set-url origin "$REPOSITORY_URL"
else
    echo "==> Reusing managed source checkout: $SOURCE_DIR"
    if ! git -C "$SOURCE_DIR" diff --quiet || ! git -C "$SOURCE_DIR" diff --cached --quiet; then
        echo "error: managed source has tracked local changes: $SOURCE_DIR" >&2
        exit 1
    fi
    managed_revision=$(git -C "$SOURCE_DIR" rev-parse HEAD)
    project_revision=$(git -C "$PROJECT_ROOT" rev-parse HEAD)
    if git -C "$PROJECT_ROOT" merge-base --is-ancestor "$managed_revision" "$project_revision"; then
        git -C "$SOURCE_DIR" fetch "$PROJECT_ROOT" "$BRANCH"
        git -C "$SOURCE_DIR" merge --ff-only FETCH_HEAD
    elif [[ "$managed_revision" != "$project_revision" ]]; then
        echo "warning: managed source diverged from this checkout; preserving $managed_revision" >&2
    fi
    git -C "$SOURCE_DIR" remote set-url origin "$REPOSITORY_URL"
fi

echo "==> Building release binary"
core_was_active=0
if systemctl --user is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi
core_was_enabled=0
if systemctl --user is-enabled --quiet echo-agent-core.service 2>/dev/null; then
    core_was_enabled=1
fi
cargo build --release --locked -p echo-agent-core --manifest-path "$SOURCE_DIR/Cargo.toml"

# Keep every file that can affect a running installation until the new service
# has been reloaded and passes its health check. This makes re-running the
# installer recoverable even when activation fails after a successful build.
ROLLBACK_DIR="$STATE_DIR/install-rollback"
install -d -m 700 "$ROLLBACK_DIR"
for rollback_file in binary updater launcher core_unit update_unit core_config revision status; do
    rm -f "$ROLLBACK_DIR/$rollback_file"
done

snapshot_file() {
    local source=$1 destination=$2
    if [[ -e "$source" ]]; then
        cp -p -- "$source" "$destination"
    fi
}

restore_file() {
    local source=$1 backup=$2
    if [[ -e "$backup" ]]; then
        cp -p -- "$backup" "$source"
    else
        rm -f -- "$source"
    fi
}

snapshot_file "$LIBEXEC_DIR/echo-agent-core-bin" "$ROLLBACK_DIR/binary"
snapshot_file "$LIBEXEC_DIR/update.sh" "$ROLLBACK_DIR/updater"
snapshot_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
snapshot_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
snapshot_file "$SYSTEMD_DIR/echo-agent-core-update.service" "$ROLLBACK_DIR/update_unit"
snapshot_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
snapshot_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
snapshot_file "$STATUS_FILE" "$ROLLBACK_DIR/status"

INSTALL_TRANSACTION=1
INSTALL_PHASE=installing
install_rollback() {
    local code=$?
    trap - ERR
    set +e
    if [[ "${INSTALL_TRANSACTION:-0}" == 1 ]]; then
        restore_file "$LIBEXEC_DIR/echo-agent-core-bin" "$ROLLBACK_DIR/binary"
        restore_file "$LIBEXEC_DIR/update.sh" "$ROLLBACK_DIR/updater"
        restore_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
        restore_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
        restore_file "$SYSTEMD_DIR/echo-agent-core-update.service" "$ROLLBACK_DIR/update_unit"
        restore_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
        restore_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
        systemctl --user daemon-reload >/dev/null 2>&1 || true
        if [[ "${core_was_enabled:-0}" == 1 ]]; then
            systemctl --user enable echo-agent-core.service >/dev/null 2>&1 || true
        else
            systemctl --user disable echo-agent-core.service >/dev/null 2>&1 || true
        fi
        if [[ "${core_was_active:-0}" == 1 ]]; then
            systemctl --user restart echo-agent-core.service >/dev/null 2>&1 || true
        fi
        local temporary="$STATUS_FILE.tmp"
        {
            printf 'state=rolled_back\n'
            printf 'revision=%s\n' "$(cat "$STATE_DIR/installed-revision" 2>/dev/null || echo unknown)"
            printf 'message=installation failed during %s; restored previous installation\n' "${INSTALL_PHASE:-unknown}"
            printf 'updated_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        } >"$temporary"
        mv -f "$temporary" "$STATUS_FILE" || true
        INSTALL_TRANSACTION=0
    fi
    exit "$code"
}
trap install_rollback ERR

binary_temporary=$(mktemp "$LIBEXEC_DIR/echo-agent-core-bin.new.XXXXXX")
install -m 755 "$SOURCE_DIR/target/release/echo-agent-core" "$binary_temporary"
mv -f "$binary_temporary" "$LIBEXEC_DIR/echo-agent-core-bin"
updater_temporary=$(mktemp "$LIBEXEC_DIR/update.sh.new.XXXXXX")
install -m 755 "$PROJECT_ROOT/scripts/update.sh" "$updater_temporary"
mv -f "$updater_temporary" "$LIBEXEC_DIR/update.sh"

if [[ ! -f "$CORE_CONFIG" ]]; then
    install -m 600 "$PROJECT_ROOT/config/echo-agent-core.toml" "$CORE_CONFIG"
    escaped_source=${SOURCE_DIR//&/\\&}
    sed -i "s|^skills_dir = .*|skills_dir = \"$escaped_source/skills\"|" "$CORE_CONFIG"
    echo "==> Installed Core config: $CORE_CONFIG"
else
    echo "==> Preserved existing Core config: $CORE_CONFIG"
fi

if ! grep -q '^\[agent\.self_update\]$' "$CORE_CONFIG"; then
    cat >>"$CORE_CONFIG" <<'EOF'

[agent.self_update]
enabled = true
allow_local = true
allowed_qq_users = []
EOF
else
    sed -i '/^\[agent\.self_update\]$/,/^\[.*\]$/{s/^enabled = false$/enabled = true/;}' "$CORE_CONFIG"
fi
if [[ -n "$OWNER_QQ" ]]; then
    sed -i "s/^owner_qq = [0-9][0-9]*$/owner_qq = $OWNER_QQ/" "$CORE_CONFIG"
fi

INSTALL_PHASE=launcher
launcher_temporary=$(mktemp "$BIN_DIR/echo-agent-core.new.XXXXXX")
cat >"$launcher_temporary" <<EOF
#!/usr/bin/env sh
has_config=0
for argument in "\$@"; do
    case "\$argument" in
        -c|--config|--config=*) has_config=1 ;;
    esac
done
if [ "\$has_config" -eq 1 ]; then
    exec "$LIBEXEC_DIR/echo-agent-core-bin" "\$@"
fi
exec "$LIBEXEC_DIR/echo-agent-core-bin" --config "$CORE_CONFIG" "\$@"
EOF
chmod 755 "$launcher_temporary"
mv -f "$launcher_temporary" "$BIN_DIR/echo-agent-core"

SERVICE_PATH=$(printf '%s\n' \
    "$(dirname -- "$(command -v cargo)")" \
    "$(dirname -- "$(command -v git)")" \
    "$(dirname -- "$(command -v install)")" \
    "$(dirname -- "$(command -v flock)")" \
    "$BIN_DIR" /usr/local/bin /usr/bin /bin | awk '!seen[$0]++' | paste -sd: -)

render_unit() {
    local source=$1 destination=$2
    sed \
        -e "s|@BINARY@|$LIBEXEC_DIR/echo-agent-core-bin|g" \
        -e "s|@UPDATE_SCRIPT@|$LIBEXEC_DIR/update.sh|g" \
        -e "s|@SOURCE_DIR@|$SOURCE_DIR|g" \
        -e "s|@CORE_CONFIG@|$CORE_CONFIG|g" \
        -e "s|@STATUS_FILE@|$STATUS_FILE|g" \
        -e "s|@SERVICE_PATH@|$SERVICE_PATH|g" \
        "$source" >"$destination"
    chmod 644 "$destination"
}

INSTALL_PHASE=units
render_unit "$PROJECT_ROOT/packaging/systemd/echo-agent-core.service.in" \
    "$SYSTEMD_DIR/echo-agent-core.service"
render_unit "$PROJECT_ROOT/packaging/systemd/echo-agent-core-update.service.in" \
    "$SYSTEMD_DIR/echo-agent-core-update.service"

INSTALL_PHASE=state
revision=$(git -C "$SOURCE_DIR" rev-parse HEAD)
revision_temporary=$(mktemp "$STATE_DIR/installed-revision.new.XXXXXX")
printf '%s\n' "$revision" >"$revision_temporary"
mv -f "$revision_temporary" "$STATE_DIR/installed-revision"
status_temporary=$(mktemp "$STATUS_FILE.new.XXXXXX")
cat >"$status_temporary" <<EOF
state=installed
revision=$revision
message=installation completed
EOF
mv -f "$status_temporary" "$STATUS_FILE"

INSTALL_PHASE=activating
systemctl --user daemon-reload
systemctl --user enable echo-agent-core.service >/dev/null
if ((START_SERVICE)); then
    systemctl --user restart echo-agent-core.service
    core_healthy=0
    healthy_checks=0
    for _ in {1..15}; do
        sleep 1
        if systemctl --user is-active --quiet echo-agent-core.service; then
            ((healthy_checks += 1))
            if ((healthy_checks >= 3)); then
                core_healthy=1
                break
            fi
        else
            healthy_checks=0
        fi
    done
    if ((core_healthy == 0)); then
        echo "error: Core did not become active after installation" >&2
        false
    fi
fi

INSTALL_TRANSACTION=0
for rollback_file in "$ROLLBACK_DIR"/*; do
    if [[ -e "$rollback_file" ]]; then
        rm -f -- "$rollback_file"
    fi
done
rmdir "$ROLLBACK_DIR" 2>/dev/null || true

cat <<EOF

Installation complete.
  Launcher: $BIN_DIR/echo-agent-core
  Config:   $CORE_CONFIG
  Service:  systemctl --user status echo-agent-core.service
  Logs:     journalctl --user -u echo-agent-core.service -f

Install the Panel (TUI) frontend separately from the EchoAgentPanel repository.
EOF
