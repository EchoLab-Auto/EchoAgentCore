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
# 二进制目录里保留多少个历史备份（`<binary>.bak-*` / `<binary>.rollback*`）。
# 每次安装/更新都会留下上一版二进制，多年累积会白占数百 MiB——实测一天内
# 多次部署后单目录堆到 308 MiB / 12 个文件。默认保留最近 4 个（够回滚几版），
# 可用 ECHO_KEEP_BACKUPS 覆盖（0 = 只留本次的 .rollback；非数字或负数回落默认 4）。
KEEP_BACKUPS=${ECHO_KEEP_BACKUPS:-4}

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
        printf 'phase=%s\n' "${PHASE:-unknown}"
        printf 'revision=%s\n' "$revision"
        printf 'message=%s\n' "$message"
        printf 'pid=%s\n' "$$"
        printf 'updated_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } >"$temporary"
    mv -f "$temporary" "$STATUS_FILE"
}

# Keep only the N most recent binary backups in the install directory.
#
# Why: every install/update keeps a copy of the previous binary
# (`<binary>.bak-*` from manual deploys, `<binary>.rollback*` from failed runs).
# Nothing pruned them, so the directory had grown to 12 files / 308 MiB in a
# single day of iterating. Retention default = 4 (env ECHO_KEEP_BACKUPS).
#
# Never touches the binary itself, the running rollback of THIS run
# (`$ROLLBACK_BINARY`), the revision file, or `update.sh`. Sorting is by mtime
# with a name tie-break so runs in the same second stay deterministic.
prune_backups() {
    local keep=$KEEP_BACKUPS
    local dir
    dir=$(dirname -- "$BINARY")
    [[ -d "$dir" ]] || return 0
    if ! [[ "$keep" =~ ^[0-9]+$ ]]; then
        keep=4
    fi
    local -a candidates=()
    local f
    # glob may not match anything: `nullglob`-style manual expansion.
    shopt -s nullglob
    # 覆盖三类残留（都实测过会堆积）：
    #   <binary>.bak-*        手工部署/历史安装留下的二进制副本
    #   <binary>.rollback*    失败运行留下的回滚副本
    #   <binary>.old          更早期的命名习惯（不在 bak/rollback 之列，
    #                         Panel 侧实测残留 4.8 MiB 未被任何规则覆盖；
    #                         Core 侧当前无此文件，一并纳入以免将来漏）
    for f in "$BINARY".bak-* "$BINARY".rollback* "$BINARY".old; do
        # 保护本次运行仍在用的回滚文件（失败时还要靠它还原）
        if [[ -n "${ROLLBACK_BINARY:-}" && "$f" == "$ROLLBACK_BINARY" ]]; then
            continue
        fi
        candidates+=("$f")
    done
    shopt -u nullglob
    local total=${#candidates[@]}
    (( total > keep )) || return 0
    # 新 → 旧：mtime 倒序，同 mtime 用名字倒序兜底（时间戳后缀保证可比）
    local -a sorted=()
    # `ls -1t` 按 mtime 新→旧排序（同一秒内按名字，时间戳后缀保证可比）。
    # mapfile 直接吃 NUL/换行分隔都行，避免 xargs 的引号/空参数坑。
    mapfile -t sorted < <(ls -1t -- "${candidates[@]}" 2>/dev/null || true)
    local removed=0
    local i
    for (( i = keep; i < ${#sorted[@]}; i++ )); do
        rm -f -- "${sorted[$i]}" && removed=$((removed + 1))
    done
    if (( removed > 0 )); then
        echo "pruned $removed old binary backup(s); kept $keep (ECHO_KEEP_BACKUPS)"
    fi
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
# 出厂技能同步（2026-10 技能分层）：自更新只滚出厂层——把新检出里的
# skills/ 刷新到只读层 skills-builtin/；用户技能层（skills/ 数据目录）
# 绝不触碰。目录不存在（旧部署未迁移过）时静默跳过，由下次 install/
# reinstall 补齐。
BUILTIN_SKILLS_DIR="$DATA_HOME/echo-agent-core/skills-builtin"
if [[ -d "$BUILTIN_SKILLS_DIR" && -d "$SOURCE_DIR/skills" ]]; then
    # 逐技能目录同步，且**绝不带 .git**（2026-10-18 实况：一次 git 安装的
    # 技能让自更新整体失败）：
    # ① `git clone` 出来的 `.git/objects/pack/*` 是 0400 只读位，`cp -a`
    #    第二次覆盖同一路径时打不开写 → `Permission denied` → 脚本 exit 1，
    #    二进制已构建却没装上，更新半途而废；
    # ② 运行期只用 SKILL.md；git 历史属于 `skills/`（用户层），更新/回滚
    #    都在那一侧做，出厂层留一份拷贝没有意义。
    # 顺带只同步带 SKILL.md 的目录：`.sources.json` 等来源记录不该进只读层。
    for _skill_src in "$SOURCE_DIR"/skills/*/; do
        [[ -f "$_skill_src/SKILL.md" ]] || continue
        _skill_name=$(basename -- "$_skill_src")
        _skill_dst="$BUILTIN_SKILLS_DIR/$_skill_name"
        if command -v rsync >/dev/null 2>&1; then
            rsync -a --delete --exclude='/.git/' "$_skill_src" "$_skill_dst/"
        else
            rm -rf "$_skill_dst"
            cp -a "$_skill_src" "$_skill_dst"
        fi
        rm -rf "$_skill_dst/.git"
    done
    unset _skill_src _skill_name _skill_dst
fi

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
# 替换成功后立即修剪历史备份（保留最近 N 个，见 prune_backups）。
prune_backups || true

if ((core_was_active)); then
    PHASE=restarting
    # 重启前先落一个"即将重启"的中间终态：正在轮询 update-status 的 agent
    # （self-update 流程）看到 restarting 就应立即结束当前 turn，而不是等
    # state=updated——updated 在重启+QQ 恢复之后才写入，等它的 turn 会被
    # 自己的重启杀死（排空窗口耗尽后 SIGKILL，遗留僵尸 running 工具条目）。
    write_status restarting "binary installed; restarting core (active turns drain first)" "$TARGET"
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
    #
    #    仅当 QQ 确实启用（[adapters.qq].enabled = true）且存在 NapCat 容器
    #    时才做恢复：QQ 关闭的部署没有反向 WS 可等，重启容器只会制造噪音。
    PHASE=restoring_qq
    qq_connected() {
        command -v ss >/dev/null 2>&1 || return 1
        # 多实例：反向 WS 端口为 3131（legacy）或 3140-3399（自动分配）。
        # 注意不要匹配 3132（管理面端口）。
        ss -Htn state established 2>/dev/null \
            | grep -qE ':(3131|31[4-9][0-9]|3[2-3][0-9][0-9])\b'
    }
    qq_enabled=1
    qq_config="${XDG_CONFIG_HOME:-$HOME/.config}/echo-agent-core/core.toml"
    if [[ -f "$qq_config" ]] && ! awk '
        /^\[adapters\.qq\]/{f=1;next}
        /^\[/{f=0}
        f && /^enabled[[:space:]]*=/ {gsub(/[[:space:]]/,"");if ($0=="enabled=true") found=1}
        END{exit !found}' "$qq_config"; then
        qq_enabled=0
    fi
    qq_containers=""
    if command -v docker >/dev/null 2>&1; then
        qq_containers=$(docker ps --format '{{.Names}}' 2>/dev/null \
            | grep -E '^(napcat|echo-napcat-.+)$' || true)
    fi
    if (( qq_enabled )) && [[ -n "$qq_containers" ]]; then
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
            for container in $qq_containers; do
                docker restart "$container" >/dev/null 2>&1 || true
            done
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
    else
        echo "QQ 未启用（适配器禁用或无 NapCat 容器），跳过 NapCat 恢复"
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
    # 先落盘再 grep：pipefail 下 `strings | grep -q` 会因 grep -q 提前退出
    # 触发 strings SIGPIPE（退出码 141）而误判全部 missing——与二进制内容无关。
    strings_tmp=$(mktemp "${BINARY}.strings.XXXXXX")
    strings "$BINARY" >"$strings_tmp" || true
    # 与 `echo_agent::plugins::BUILTIN_PLUGIN_IDS` 必须一致（守护测试：
    # source/core/tests/update_script_plugins.rs）；移除插件时两处同改。
    # 实例插件 id 自 2026-10 由平台名派生（`echo-agent.instance.<platform>`），
    # 无法静态枚举——校验**前缀**即可确认派生逻辑进了二进制。其余为框架内置。
    expected_ids="echo-agent.instance. echo-agent.tools.builtin echo-agent.skills.dir echo-agent.workspace echo-agent.subagent echo-agent.provider.llm echo-agent.loop.single echo-agent.loop.parallel echo-agent.management.panel"
    for id in $expected_ids; do
        if ! grep -q "$id" "$strings_tmp"; then
            missing_plugins="$missing_plugins $id"
        fi
    done
    rm -f "$strings_tmp"
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
# 成功收尾后再修剪一次：本次的 .rollback 已删除，名额回滚给更早的备份。
ROLLBACK_BINARY=""
ROLLBACK_UPDATER=""
prune_backups || true
