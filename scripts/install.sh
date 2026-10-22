#!/usr/bin/env bash
set -Eeuo pipefail

# BASH_SOURCE 兜底：sh/dash 管道执行时不可用——退回 $0；两者都没有
# （纯 stdin）时 PROJECT_ROOT 定位失败会走 bootstrap 分支（按设计）。
_SCRIPT_PATH="${BASH_SOURCE[0]:-${0:-}}"
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$_SCRIPT_PATH")/.." 2>/dev/null && pwd -P || true)
PROJECT_ROOT=${PROJECT_ROOT:-/nonexistent}

# ── 国内镜像加速（2026-10）──────────────────────────────────────────
# 国内网络下 GitHub 克隆 / rustup 安装 / crates.io 依赖拉取常年缓慢或
# 不可达；以下机制提供"灵活、可单点、失败自动回退"的加速：
#   开启（本地）：  ./scripts/install.sh --cn-mirror[=rsproxy|tuna|ustc]
#   开启（一键）：  curl -fsSL <脚本地址> | ECHO_CN_MIRROR=1 bash
#                   curl -fsSL <脚本地址> | bash -s -- --cn-mirror
#   关闭：          --no-cn-mirror / ECHO_CN_MIRROR=0
#   单环节覆盖（独立于总开关，只加速需要的环节）：
#     ECHO_GITHUB_PROXY=https://gh-proxy.com    GitHub 克隆加速前缀
#     ECHO_RUSTUP_INIT_URL / ECHO_RUSTUP_DIST_SERVER /
#     ECHO_RUSTUP_UPDATE_ROOT / ECHO_CRATES_MIRROR（sparse+https://…）
# 语义：默认全直连（原有行为不变）；每个环节失败自动回退官方端点；
# crates 镜像在构建前做可达性预检。详见 README「国内网络」一节。
CN_MIRROR_REQUEST=${ECHO_CN_MIRROR:-0}
for _arg in "$@"; do
    case "$_arg" in
        --cn-mirror)    CN_MIRROR_REQUEST=1 ;;
        --cn-mirror=*)  CN_MIRROR_REQUEST=${_arg#--cn-mirror=} ;;
        --no-cn-mirror) CN_MIRROR_REQUEST=0 ;;
    esac
done

# 把请求值（0/1/preset）解析成各环节的镜像 URL：专用环境变量逐项优先
# 于预设；未设置 = 该环节直连官方。
resolve_cn_mirror() {
    local request=$1 preset=
    case "$request" in
        0|off|no|false|"") preset= ;;
        1|on|yes|true)      preset=rsproxy ;;
        rsproxy|tuna|ustc)  preset=$request ;;
        *)
            echo "error: unknown --cn-mirror preset: $request (expected rsproxy|tuna|ustc)" >&2
            exit 2
            ;;
    esac
    CN_MIRROR=$preset
    GITHUB_PROXY=${ECHO_GITHUB_PROXY:-}
    RUSTUP_INIT_URL=${ECHO_RUSTUP_INIT_URL:-}
    RUSTUP_DIST_SERVER=${ECHO_RUSTUP_DIST_SERVER:-}
    RUSTUP_UPDATE_ROOT=${ECHO_RUSTUP_UPDATE_ROOT:-}
    CRATES_MIRROR=${ECHO_CRATES_MIRROR:-}
    [[ -n "$preset" ]] || return 0
    # 默认组合：GitHub 走 gh-proxy.com（2026-10 实测最稳）；Rust 生态
    # 走 rsproxy.cn。tuna / ustc 预设替换 rustup 与 crates 两个环节。
    : "${GITHUB_PROXY:=https://gh-proxy.com}"
    : "${RUSTUP_INIT_URL:=https://rsproxy.cn/rustup-init.sh}"
    case "$preset" in
        rsproxy)
            : "${RUSTUP_DIST_SERVER:=https://rsproxy.cn}"
            : "${RUSTUP_UPDATE_ROOT:=https://rsproxy.cn/rustup}"
            : "${CRATES_MIRROR:=sparse+https://rsproxy.cn/index/}"
            ;;
        tuna)
            : "${RUSTUP_DIST_SERVER:=https://mirrors.tuna.tsinghua.edu.cn/rustup}"
            : "${CRATES_MIRROR:=sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/}"
            ;;
        ustc)
            : "${RUSTUP_DIST_SERVER:=https://mirrors.ustc.edu.cn/rust-static}"
            : "${CRATES_MIRROR:=sparse+https://mirrors.ustc.edu.cn/crates.io-index/}"
            ;;
    esac
}
resolve_cn_mirror "$CN_MIRROR_REQUEST"

# https://github.com/… → 镜像前缀形式（如
# https://gh-proxy.com/https://github.com/…）；未启用代理时原样返回。
github_mirror_url() {
    local url=$1
    if [[ -n "$GITHUB_PROXY" && "$url" == https://github.com/* ]]; then
        printf '%s/%s' "${GITHUB_PROXY%/}" "$url"
    else
        printf '%s' "$url"
    fi
}

# sparse+URL → 快速可达性预检（curl 不可用时按"可达"处理直接尝试）。
mirror_reachable() {
    local url=$1
    command -v curl >/dev/null 2>&1 || return 0
    url=${url#sparse+}
    curl -fsS -o /dev/null --connect-timeout 2 --max-time 4 "${url%/}/config.json"
}

# bootstrap 前置依赖：git 自动安装（curl|bash 在新机器上的第一道门槛）。
# 与主流程的 ensure_command 同源策略（可识别包管理器 + root/sudo）。
try_install_git() {
    local sudo=""
    if [[ "$(id -u)" -ne 0 ]]; then
        if command -v sudo >/dev/null 2>&1; then
            sudo="sudo"
        else
            echo "error: 自动安装 git 需要 root 或 sudo（均不可用）" >&2
            return 1
        fi
    fi
    echo "==> Installing git via the system package manager"
    if command -v apt-get >/dev/null 2>&1; then
        $sudo apt-get update -qq &&
            DEBIAN_FRONTEND=noninteractive $sudo apt-get install -y git
    elif command -v dnf >/dev/null 2>&1; then
        $sudo dnf install -y git
    elif command -v yum >/dev/null 2>&1; then
        $sudo yum install -y git
    elif command -v pacman >/dev/null 2>&1; then
        $sudo pacman -Sy --noconfirm --needed git
    elif command -v zypper >/dev/null 2>&1; then
        $sudo zypper --non-interactive install git
    else
        echo "error: 未找到可识别的包管理器（apt-get/dnf/yum/pacman/zypper）" >&2
        return 1
    fi
    command -v git >/dev/null 2>&1
}

# ── curl|bash 一键安装引导（2026-10）────────────────────────────────
# 用法：curl -fsSL <raw-url>/scripts/install.sh | bash
# 通过管道运行时，脚本所在目录不是 git 检出——此时先把仓库浅克隆到
# 临时 bootstrap 目录，再 exec 克隆里的 install.sh 接力（后续流程与
# 本地运行完全一致：依赖自动安装 → 受管检出 → 构建 → systemd 服务）。
DEFAULT_REPO="https://github.com/EchoLab-Auto/EchoAgentCore.git"
if [[ ! -d "$PROJECT_ROOT/.git" ]] && [[ -z "${ECHO_BOOTSTRAPPED:-}" ]]; then
    echo "==> curl|bash bootstrap: cloning repository for installer assets"
    if ! command -v git >/dev/null 2>&1; then
        if ! try_install_git; then
            echo "error: bootstrap 需要 git，且自动安装未成功——请手动安装 git 后重跑本命令" >&2
            exit 1
        fi
    fi
    BOOTSTRAP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/echo-agent-bootstrap.XXXXXX")
    trap 'rm -rf "$BOOTSTRAP_DIR"' EXIT
    bootstrap_repo=${ECHO_REPOSITORY_URL:-$DEFAULT_REPO}
    bootstrap_cloned=0
    if [[ -n "$GITHUB_PROXY" && "$bootstrap_repo" == https://github.com/* ]]; then
        echo "==> Using GitHub mirror: $GITHUB_PROXY"
        if git clone --depth 1 "$(github_mirror_url "$bootstrap_repo")" "$BOOTSTRAP_DIR/repo"; then
            bootstrap_cloned=1
        else
            echo "warning: mirror clone failed; retrying without mirror" >&2
            rm -rf -- "$BOOTSTRAP_DIR/repo"
        fi
    fi
    if ((bootstrap_cloned == 0)); then
        if ! git clone --depth 1 "$bootstrap_repo" "$BOOTSTRAP_DIR/repo"; then
            if [[ -z "$GITHUB_PROXY" ]]; then
                echo "note: 克隆失败。国内/慢网络可启用镜像后重试（见 README「国内网络」）：" >&2
                echo "      curl -fsSL <本脚本地址> | ECHO_CN_MIRROR=1 bash" >&2
            else
                echo "note: 镜像与直连均克隆失败——请检查网络，或用 ECHO_GITHUB_PROXY 指定其他可用代理后重试。" >&2
            fi
            exit 1
        fi
    fi
    export ECHO_BOOTSTRAPPED=1
    exec bash "$BOOTSTRAP_DIR/repo/scripts/install.sh" "$@"
fi
# 全部安装路径都由「服务用户的 HOME」推导。
#
# 做成函数而不是一段直线代码：system 作用域下 HOME 可能**不是**当前 shell 的
# $HOME。典型情形是 `sudo scripts/install.sh --scope=system`——sudo 把 HOME
# 换成 /root，但服务该以谁的身份运行是另一回事（默认取 $SUDO_USER）。所以
# 参数解析完拿到 --service-user 之后要按对应用户的家目录重算一次。
#
# honor_xdg=1 表示沿用调用者的 XDG_* 覆盖（同一用户安装时的既有行为）；
# 跨用户安装必须用目标用户自己的默认布局，否则会把路径指到调用者的目录里。
derive_paths() {
    local home=$1 honor_xdg=$2
    PREFIX=${ECHO_PREFIX:-"$home/.local"}
    if ((honor_xdg)); then
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
    SOURCE_DIR="$DATA_DIR/source"
    LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core"
    # 插件安装根（约定：`plugins/<包名>/<二进制>`，见插件开发指南「安装目录约定」）。
    PLUGINS_DIR="$LIBEXEC_DIR/plugins"
    BIN_DIR="$PREFIX/bin"
    CORE_CONFIG="$CONFIG_DIR/core.toml"
    STATUS_FILE="$STATE_DIR/update-status"
}
derive_paths "$HOME" 1

# 随框架一起安装的**进程外插件二进制**：仓库内 `source/plugin/*` 中
# `[[bin]] path = "src/main.rs"` 的 crate（`src/bin/*` 是测试夹具，不装）。
# 守护测试 source/core/tests/install_script_flag.rs 比对本表与仓库实际；
# 新增/删除可安装插件二进制时，本表与 update.sh 的 PLUGIN_BINARIES 同改。
PLUGIN_BINARIES=(echo-plugin-example)

# 服务作用域（2026-10）：`user`（默认，`systemctl --user`）或 `system`
# （`/etc/systemd/system`，`systemctl` 无 --user）。
#
# 为什么需要这个开关：自更新链路（`echo-plugin-self-update` 启动 updater 单元、
# `update.sh` 重启 Core）此前把 `--user` 写死在代码里，于是**只有 user 作用域
# 的自更新是通的**。system 作用域的老装法只能靠一个"假 systemctl"（PATH shim
# 把 `--user` 抹掉）硬凑，core 自身那次调用还凑不上——按钮点了只会报错。
# 现在作用域是一等参数：单元里注入 `ECHO_SERVICE_SCOPE`，插件与 update.sh 都
# 读它，两条作用域都走真实路径，不再需要任何 shim。
SERVICE_SCOPE=${ECHO_SERVICE_SCOPE:-user}
# system 作用域下服务以谁的身份运行（本版本固定 root，见下方作用域解析）。
SERVICE_USER=${ECHO_SERVICE_USER:-}
# 显式初始化：脚本开了 set -u，而这两个变量只在 system 作用域分支里赋值。
SERVICE_HOME=""
ECHO_SKIP_LINGER=0
SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
SYSTEMCTL=(systemctl --user)
REPOSITORY_URL=${ECHO_REPOSITORY_URL:-}
START_SERVICE=1
DRY_RUN=0
# 配置文件策略（2026-10）：默认保留已有 core.toml；--overwrite-config
# 以最新模板重写（旧文件先备份为 core.toml.bak-<epoch>；
# management_access_token 沿用旧值——重写配置不应顺带换掉 Panel/对端凭证）。
OVERWRITE_CONFIG=0
# 前置依赖策略（2026-10）：默认自动安装缺失项（可识别包管理器 + sudo
# 可用时）；--no-deps 跳过，只做检查并提示。
INSTALL_DEPS=1

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [options]

Options:
  --repository <url>    Upstream Git repository used by self-update
  --no-start            Install and enable units without starting Core
  --no-deps             Skip automatic installation of missing prerequisites
                        (only check and report)
  --overwrite-config    Replace an existing core.toml with the shipped template
                        (the previous file is kept as core.toml.bak-<epoch>;
                        an existing management_access_token is carried over)
  --dry-run             Print resolved paths without changing the system
  --scope <user|system> Which systemd scope to install into (default: user).

                        `user` (default) writes ~/.config/systemd/user units and
                        drives them with `systemctl --user`; it enables linger so
                        the service survives logout.
                        `system` writes /etc/systemd/system units and drives them
                        with plain `systemctl`; it requires root (run under sudo)
                        and does not touch linger (linger is a user-scope
                        lifetime switch and is meaningless here).

                        Both scopes are fully supported, including self-update:
                        the scope is recorded in the units as ECHO_SERVICE_SCOPE
                        and read back by the self-update plugin and update.sh.
  --service-user <name> Service account for system scope. Currently must be
                        root (default): the updater is a oneshot unit that has to
                        `systemctl restart` a system-wide unit, and only root can.
                        Non-root services are the user scope's job.
  --cn-mirror[=PRESET]  Accelerate GitHub clone / rustup / crates.io on slow
                        networks (PRESET: rsproxy|tuna|ustc; default rsproxy).
                        Per-component overrides via environment variables:
                        ECHO_GITHUB_PROXY, ECHO_RUSTUP_INIT_URL,
                        ECHO_RUSTUP_DIST_SERVER, ECHO_RUSTUP_UPDATE_ROOT,
                        ECHO_CRATES_MIRROR. Every mirror falls back to the
                        official endpoint on failure.
  --no-cn-mirror        Disable mirrors (overrides ECHO_CN_MIRROR)
  -h, --help            Show this help
EOF
}

while (($#)); do
    case "$1" in
        --repository)
            REPOSITORY_URL=${2:-}
            shift 2
            ;;
        --no-start)
            START_SERVICE=0
            shift
            ;;
        --no-deps)
            INSTALL_DEPS=0
            shift
            ;;
        --overwrite-config)
            OVERWRITE_CONFIG=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
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
        --service-user)
            SERVICE_USER=${2:-}
            shift 2
            ;;
        --service-user=*)
            SERVICE_USER=${1#--service-user=}
            shift
            ;;
        --cn-mirror|--cn-mirror=*|--no-cn-mirror)
            # 已在脚本顶部统一解析（bootstrap 阶段也需要），这里仅消费。
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            case "$1" in
                rsproxy|tuna|ustc)
                    echo "error: unknown option: $1（镜像预设请写成 --cn-mirror=$1）" >&2
                    ;;
                *)
                    echo "error: unknown option: $1" >&2
                    ;;
            esac
            usage >&2
            exit 2
            ;;
    esac
done

# ── 作用域解析 ─────────────────────────────────────────────────────────
case "$SERVICE_SCOPE" in
    user|system) ;;
    *)
        echo "error: --scope must be 'user' or 'system' (got: '$SERVICE_SCOPE')" >&2
        exit 2
        ;;
esac

if [[ "$SERVICE_SCOPE" == system ]]; then
    # system 作用域要写 /etc/systemd/system 并操作 PID 1 的 manager——必须 root。
    # 不自动 re-exec sudo：提权失败点埋在执行流中段时，回滚会非常难看。
    # --dry-run 例外：它只打印解析后的路径，不碰系统，没必要提权。
    if ((EUID != 0)) && ((!DRY_RUN)); then
        echo "error: --scope system requires root (it writes /etc/systemd/system)" >&2
        echo "       re-run as: sudo $0 --scope system" >&2
        exit 1
    fi
    # 本版本 system 作用域固定以 root 运行。原因不是"图省事"：updater 是
    # oneshot（Type=oneshot, ExecStart=update.sh），它最后要 `systemctl restart`
    # 一个**系统级**单元，非 root 没有这个权限；而在 systemd 里给 updater 开
    # 提权路径（polkit 规则 / sudo NOPASSWD）等于把"任意构建脚本可 restart
    # 系统服务"写成配置，比省下来的隔离更贵。要跑非 root 服务，用 user 作用域。
    SERVICE_USER=${SERVICE_USER:-root}
    if [[ "$SERVICE_USER" != root ]]; then
        echo "error: --scope system currently runs the service as root (got --service-user $SERVICE_USER)" >&2
        echo "       The updater unit must be able to 'systemctl restart' a system unit," >&2
        echo "       which only root can do; a non-root layout is not implemented." >&2
        echo "       Use the default user scope (per-user install) for non-root services." >&2
        exit 1
    fi
    SERVICE_HOME=$(getent passwd root 2>/dev/null | cut -d: -f6)
    SERVICE_HOME=${SERVICE_HOME:-/root}
    derive_paths "$SERVICE_HOME" 1
    SYSTEMD_DIR=/etc/systemd/system
    SYSTEMCTL=(systemctl)
    # system 作用域没有 linger 概念：单元挂在 multi-user.target 上，开机即起、
    # 与会话无关。此时仍给用户开 linger 只是无谓地常驻一个用户管理器。
    ECHO_SKIP_LINGER=1
else
    # 默认作用域：拒绝在 `sudo` 下静默按 root 装（会让单元落进 /root/.config
    # 而服务以 root 运行，运维以为装到了自己名下）。要 root 就显式 --scope system。
    if ((EUID == 0)) && [[ -n "${SUDO_USER:-}" ]]; then
        echo "warning: running as root via sudo but installing the **user** scope:" >&2
        echo "         units go to $SYSTEMD_DIR (root's home), not to $SUDO_USER's." >&2
        echo "         To install a system-wide service instead:" >&2
        echo "           sudo $0 --scope system --service-user $SUDO_USER" >&2
    fi
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        echo "error: systemd user manager is unavailable for this user" >&2
        echo "       (user scope needs a running user manager; on servers that lack one," >&2
        echo "        use --scope system instead)" >&2
        exit 1
    fi
    # 已经有 system 作用域的单元时不要静默覆盖布局：两套会同时抢端口。
    if [[ -e /etc/systemd/system/echo-agent-core.service ]]; then
        echo "error: system-scope units already exist (/etc/systemd/system/echo-agent-core.service)" >&2
        echo "       refusing to add a user-scope installation next to them (both would" >&2
        echo "       bind the same ports). Run: sudo scripts/uninstall.sh --scope system" >&2
        echo "       or re-install in place with: sudo scripts/install.sh --scope system" >&2
        exit 1
    fi
fi

if [[ -z "$REPOSITORY_URL" ]]; then
    REPOSITORY_URL=$(git -C "$PROJECT_ROOT" remote get-url origin 2>/dev/null || true)
fi
if [[ "$REPOSITORY_URL" =~ ^git@([^:]+):(.+)$ ]]; then
    REPOSITORY_URL="https://${BASH_REMATCH[1]}/${BASH_REMATCH[2]}"
fi
if [[ -z "$REPOSITORY_URL" ]]; then
    REPOSITORY_URL="$DEFAULT_REPO"
fi

BRANCH=$(git -C "$PROJECT_ROOT" branch --show-current 2>/dev/null || true)
BRANCH=${BRANCH:-main}

if ((DRY_RUN)); then
    # 镜像摘要（dry-run 也能核对配置是否正确）。
    mirror_summary="off"
    mirror_parts=()
    if [[ -n "$GITHUB_PROXY" ]]; then mirror_parts+=("github=$GITHUB_PROXY"); fi
    if [[ -n "$RUSTUP_DIST_SERVER" ]]; then mirror_parts+=("rustup=$RUSTUP_DIST_SERVER"); fi
    if [[ -n "$CRATES_MIRROR" ]]; then mirror_parts+=("crates=$CRATES_MIRROR"); fi
    if ((${#mirror_parts[@]})); then mirror_summary="on (${mirror_parts[*]})"; fi
    cat <<EOF
EchoAgentCore install plan
  project:    $PROJECT_ROOT
  source:     $SOURCE_DIR
  binary:     $LIBEXEC_DIR/echo-agent-core-bin
  config:     $CONFIG_DIR
  state:      $STATE_DIR
  systemd:    $SYSTEMD_DIR ($SERVICE_SCOPE scope$( [[ "$SERVICE_SCOPE" == system ]] && echo ", user=$SERVICE_USER" ))
  repository: $REPOSITORY_URL
  branch:     $BRANCH
  start Core: $START_SERVICE
  overwrite config: $( ((OVERWRITE_CONFIG)) && echo "yes (backup kept)" || echo no )
  cn mirror:  $mirror_summary
EOF
    exit 0
fi

# ── 前置依赖：检查 + 自动安装（2026-10）──────────────────────────────
# 依赖分两类：
#   系统命令：git / cc（编译器，部分 crate 用 cc 编译 C 段）/ pkg-config
#     以及 util-linux（flock）等基础工具——走发行版包管理器安装；
#   Rust 工具链：cargo——走官方 rustup（用户级，无需 root）。
# 已识别的包管理器：apt / dnf / yum / pacman / zypper；识别不到则只报
# 缺失清单让用户手动装。

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    fi
fi

detect_pkg_manager() {
    for pm in apt-get dnf yum pacman zypper; do
        if command -v "$pm" >/dev/null 2>&1; then
            echo "$pm"
            return 0
        fi
    done
    return 1
}

# 系统包安装：传入包名列表（按检测到的包管理器翻译）。
pkg_install() {
    local pm
    pm=$(detect_pkg_manager) || return 1
    local pkgs=("$@")
    echo "==> Installing system packages via $pm: ${pkgs[*]}"
    case "$pm" in
        apt-get)
            $SUDO apt-get update -qq
            DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y "${pkgs[@]}"
            ;;
        dnf)    $SUDO dnf install -y "${pkgs[@]}" ;;
        yum)    $SUDO yum install -y "${pkgs[@]}" ;;
        pacman) $SUDO pacman -Sy --noconfirm --needed "${pkgs[@]}" ;;
        zypper) $SUDO zypper --non-interactive install "${pkgs[@]}" ;;
    esac
}

# 命令 → 各包管理器下的包名（多数同名；差异项单独映射）。
pkg_name_for() {
    local pm=$1 cmd=$2
    case "$cmd" in
        cc|gcc)
            case "$pm" in
                apt-get) echo "build-essential" ;;
                dnf|yum) echo "gcc" ;;
                pacman)  echo "base-devel" ;;
                zypper)  echo "gcc" ;;
            esac
            ;;
        pkg-config)
            case "$pm" in
                apt-get|dnf|yum|zypper) echo "pkg-config" ;;
                pacman) echo "pkgconf" ;;
            esac
            ;;
        flock)
            case "$pm" in
                pacman|apt-get|dnf|yum|zypper) echo "util-linux" ;;
            esac
            ;;
        *) echo "$cmd" ;;
    esac
}

missing_report=()

ensure_command() {
    local cmd=$1
    if command -v "$cmd" >/dev/null 2>&1; then
        return 0
    fi
    if ((INSTALL_DEPS == 0)); then
        missing_report+=("$cmd")
        return 1
    fi
    local pm
    if ! pm=$(detect_pkg_manager); then
        missing_report+=("$cmd (无可识别的包管理器，请手动安装)")
        return 1
    fi
    if [[ -z "$SUDO" && "$(id -u)" -ne 0 ]]; then
        missing_report+=("$cmd (需要 root/sudo 安装，当前不可用)")
        return 1
    fi
    pkg_install "$(pkg_name_for "$pm" "$cmd")"
    command -v "$cmd" >/dev/null 2>&1
}

ensure_rust_toolchain() {
    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi
    # 常见位置兜底（rustup 装了但 PATH 未加载——非交互 SSH 常见）。
    if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
        export PATH="$HOME/.cargo/bin:$PATH"
        return 0
    fi
    if ((INSTALL_DEPS == 0)); then
        missing_report+=("cargo (Rust 工具链；https://rustup.rs)")
        return 1
    fi
    echo "==> Installing Rust toolchain via rustup (user-level, no root)"
    if ! command -v curl >/dev/null 2>&1; then
        ensure_command curl || {
            missing_report+=("curl (rustup 安装需要)")
            return 1
        }
    fi

    # rustup-init 安装器：启用镜像时先走镜像下载，失败回退官方（2026-10）。
    local init_url=${RUSTUP_INIT_URL:-https://sh.rustup.rs}
    local init_file fetched=0
    init_file=$(mktemp "${TMPDIR:-/tmp}/rustup-init.XXXXXX")
    if [[ "$init_url" != "https://sh.rustup.rs" ]]; then
        echo "==> Downloading rustup-init via mirror: $init_url"
    fi
    if curl --proto '=https' --tlsv1.2 -fsSL "$init_url" -o "$init_file" && [[ -s "$init_file" ]]; then
        fetched=1
    elif [[ "$init_url" != "https://sh.rustup.rs" ]]; then
        echo "warning: rustup-init mirror download failed; falling back to https://sh.rustup.rs" >&2
        if curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$init_file" && [[ -s "$init_file" ]]; then
            fetched=1
        fi
    fi
    if ((fetched == 0)); then
        rm -f -- "$init_file"
        missing_report+=("cargo（rustup-init 下载失败——检查网络；国内网络可加 --cn-mirror）")
        return 1
    fi

    # 工具链下载：RUSTUP_DIST_SERVER / RUSTUP_UPDATE_ROOT 交由 rustup-init
    # 使用；镜像失败时去掉这两个变量重试一次（回退官方）。
    local mir_env=() installed=0
    if [[ -n "$RUSTUP_DIST_SERVER" ]]; then mir_env+=("RUSTUP_DIST_SERVER=$RUSTUP_DIST_SERVER"); fi
    if [[ -n "$RUSTUP_UPDATE_ROOT" ]]; then mir_env+=("RUSTUP_UPDATE_ROOT=$RUSTUP_UPDATE_ROOT"); fi
    if ((${#mir_env[@]})); then
        if env "${mir_env[@]}" sh "$init_file" -y --quiet; then
            installed=1
        else
            echo "warning: rustup with mirror failed; retrying without mirror" >&2
            if sh "$init_file" -y --quiet; then
                installed=1
            fi
        fi
    elif sh "$init_file" -y --quiet; then
        installed=1
    fi
    rm -f -- "$init_file"
    export PATH="$HOME/.cargo/bin:$PATH"
    if ((installed == 0)); then
        missing_report+=("cargo（rustup 安装未成功——检查网络；国内网络可加 --cn-mirror）")
        return 1
    fi
    command -v cargo >/dev/null 2>&1
}

check_and_install_deps() {
    local failed=0
    # 编译期：cc（crate `cc` 编译 C 段）；运行期/安装期基础工具。
    for cmd in git cc pkg-config install sed flock cp stat; do
        ensure_command "$cmd" || failed=1
    done
    ensure_rust_toolchain || failed=1
    if ((failed)); then
        if ((${#missing_report[@]})); then
            echo "error: 以下前置依赖缺失且未能自动安装：" >&2
            printf '  - %s\n' "${missing_report[@]}" >&2
            echo "请手动安装后重试，或去掉 --no-deps 让脚本自动安装。" >&2
        else
            echo "error: 前置依赖自动安装失败（详见上方输出）。" >&2
        fi
        exit 1
    fi
}

check_and_install_deps

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: the service installer currently supports Linux/systemd only" >&2
    exit 1
fi
install -d -m 755 "$BIN_DIR" "$LIBEXEC_DIR" "$PLUGINS_DIR" "$CONFIG_DIR" "$DATA_DIR" "$STATE_DIR" "$SYSTEMD_DIR"

if [[ ! -d "$SOURCE_DIR/.git" ]]; then
    echo "==> Creating managed source checkout"
    if [[ -d "$PROJECT_ROOT/.git" ]]; then
        # 本地检出：--local 硬链接加速（同机）。
        git clone --local --no-hardlinks --branch "$BRANCH" "$PROJECT_ROOT" "$SOURCE_DIR"
    else
        # bootstrap 浅克隆（无 --local 意义）：完整克隆受管检出（自更新
        # 需要历史与远端跟踪）。启用镜像时先走镜像、失败回退直连。
        managed_cloned=0
        if [[ -n "$GITHUB_PROXY" && "$REPOSITORY_URL" == https://github.com/* && ! -e "$SOURCE_DIR" ]]; then
            echo "==> Using GitHub mirror for managed checkout: $GITHUB_PROXY"
            if git clone --branch "$BRANCH" "$(github_mirror_url "$REPOSITORY_URL")" "$SOURCE_DIR"; then
                managed_cloned=1
            else
                echo "warning: mirror clone failed; retrying without mirror" >&2
                rm -rf -- "$SOURCE_DIR"
            fi
        fi
        if ((managed_cloned == 0)); then
            git clone --branch "$BRANCH" "$REPOSITORY_URL" "$SOURCE_DIR"
        fi
    fi
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
if "${SYSTEMCTL[@]}" is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi
core_was_enabled=0
if "${SYSTEMCTL[@]}" is-enabled --quiet echo-agent-core.service 2>/dev/null; then
    core_was_enabled=1
fi
build_args=(build --release --locked -p echo-agent-core)
# 进程外插件二进制与 Core 同一次 cargo 调用构建（同一个 target 目录、
# 同一套 --locked / 镜像参数），避免第二轮解析依赖。
for _plugin in "${PLUGIN_BINARIES[@]}"; do
    build_args+=(-p "$_plugin")
done
build_args+=(--manifest-path "$SOURCE_DIR/Cargo.toml")
if [[ -n "$CRATES_MIRROR" ]]; then
    # crates.io 镜像：--config 注入源替换（不落盘、不影响用户全局配置）；
    # 使用前预检镜像可达性，不可达则直接用官方源。
    if mirror_reachable "$CRATES_MIRROR"; then
        echo "==> Using crates.io mirror: $CRATES_MIRROR"
        build_args=(
            --config 'source.crates-io.replace-with="cn-mirror"'
            --config "source.cn-mirror.registry=\"$CRATES_MIRROR\""
            "${build_args[@]}"
        )
    else
        echo "warning: crates mirror unreachable; using the official registry" >&2
    fi
fi
if ! cargo "${build_args[@]}"; then
    if [[ -n "$CRATES_MIRROR" ]]; then
        echo "note: 构建失败——若怀疑镜像问题，可不带 --cn-mirror 重跑本脚本。" >&2
    else
        echo "note: 构建失败——国内网络可加 --cn-mirror 启用镜像加速后重试。" >&2
    fi
    exit 1
fi

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
for _plugin in "${PLUGIN_BINARIES[@]}"; do
    snapshot_file "$PLUGINS_DIR/$_plugin/$_plugin" "$ROLLBACK_DIR/plugin-$_plugin"
done
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
        for _plugin in "${PLUGIN_BINARIES[@]}"; do
            restore_file "$PLUGINS_DIR/$_plugin/$_plugin" "$ROLLBACK_DIR/plugin-$_plugin"
        done
        restore_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
        restore_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
        restore_file "$SYSTEMD_DIR/echo-agent-core-update.service" "$ROLLBACK_DIR/update_unit"
        restore_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
        restore_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
        "${SYSTEMCTL[@]}" daemon-reload >/dev/null 2>&1 || true
        if [[ "${core_was_enabled:-0}" == 1 ]]; then
            "${SYSTEMCTL[@]}" enable echo-agent-core.service >/dev/null 2>&1 || true
        else
            "${SYSTEMCTL[@]}" disable echo-agent-core.service >/dev/null 2>&1 || true
        fi
        if [[ "${core_was_active:-0}" == 1 ]]; then
            "${SYSTEMCTL[@]}" restart echo-agent-core.service >/dev/null 2>&1 || true
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
# 进程外插件二进制：`plugins/<包名>/<二进制>`——plugins.toml 的 command
# 直接指这里。目录一次建好，便于用户往同一处放自己的插件。
for _plugin in "${PLUGIN_BINARIES[@]}"; do
    plugin_dir="$PLUGINS_DIR/$_plugin"
    install -d -m 755 "$plugin_dir"
    plugin_temporary=$(mktemp "$plugin_dir/$_plugin.new.XXXXXX")
    install -m 755 "$SOURCE_DIR/target/release/$_plugin" "$plugin_temporary"
    mv -f "$plugin_temporary" "$plugin_dir/$_plugin"
    echo "==> Installed plugin binary: $plugin_dir/$_plugin"
done
unset _plugin plugin_dir plugin_temporary
updater_temporary=$(mktemp "$LIBEXEC_DIR/update.sh.new.XXXXXX")
install -m 755 "$PROJECT_ROOT/scripts/update.sh" "$updater_temporary"
mv -f "$updater_temporary" "$LIBEXEC_DIR/update.sh"

# 技能分层（2026-10 议题 5）：出厂技能 = 框架资产，从受管检出拷到
# $DATA_DIR/skills-builtin/（只读层；update.sh 同步刷新——自更新只滚出厂
# 技能）；用户技能目录 = $DATA_DIR/skills/（读写层：Save/DeleteSkill 与
# Git 安装技能落这里，自更新不触碰）。同名技能用户层覆盖出厂版本。
BUILTIN_SKILLS_DIR="$DATA_DIR/skills-builtin"
USER_SKILLS_DIR="$DATA_DIR/skills"
install -d -m 755 "$BUILTIN_SKILLS_DIR" "$USER_SKILLS_DIR"
cp -a "$SOURCE_DIR/skills/." "$BUILTIN_SKILLS_DIR/"

# 旧配置的 management_access_token（--overwrite-config 重写时沿用——
# 换令牌会让 Panel/对端全部失联，重写配置不应顺带换凭证）。
previous_token=""
if [[ -f "$CORE_CONFIG" ]]; then
    previous_token=$(awk -F'"' '/^management_access_token/{print $2; exit}' "$CORE_CONFIG")
fi

if [[ -f "$CORE_CONFIG" ]] && ((OVERWRITE_CONFIG)); then
    config_backup="$CORE_CONFIG.bak-$(date +%s)"
    cp -p -- "$CORE_CONFIG" "$config_backup"
    echo "==> Overwriting Core config (previous copy kept at $config_backup)"
fi

if [[ ! -f "$CORE_CONFIG" ]] || ((OVERWRITE_CONFIG)); then
    install -m 600 "$PROJECT_ROOT/config/echo-agent-core.toml" "$CORE_CONFIG"
    escaped_builtin=${BUILTIN_SKILLS_DIR//&/\\&}
    escaped_user=${USER_SKILLS_DIR//&/\\&}
    sed -i "s|^skills_dir = .*|skills_dir = \"$escaped_user\"|" "$CORE_CONFIG"
    # 出厂层作为只读附加目录写入（skills_dirs 数组）。
    if grep -q '^skills_dirs' "$CORE_CONFIG"; then
        sed -i "s|^skills_dirs = .*|skills_dirs = [\"$escaped_builtin\"]|" "$CORE_CONFIG"
    else
        sed -i "/^skills_dir = /a skills_dirs = [\"$escaped_builtin\"]" "$CORE_CONFIG"
    fi
    # management_access_token：有旧值优先沿用（重写配置不换凭证）；无
    # 旧值（首装）时生成 32 字节随机 hex——暴露 management WS 到局域网
    # 时开箱即有认证。
    if [[ -n "$previous_token" ]]; then
        escaped_token=${previous_token//&/\&}
        sed -i "s|^management_access_token = \"\"|management_access_token = \"$escaped_token\"|" "$CORE_CONFIG"
        echo "==> Kept existing management_access_token (see $CORE_CONFIG)"
    elif grep -q '^management_access_token = ""' "$CORE_CONFIG"; then
        generated_token=$(openssl rand -hex 32 2>/dev/null \
            || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
        escaped_token=${generated_token//&/\&}
        sed -i "s|^management_access_token = \"\"|management_access_token = \"$escaped_token\"|" "$CORE_CONFIG"
        echo "==> Generated management_access_token (see $CORE_CONFIG)"
    fi
    echo "==> Installed Core config: $CORE_CONFIG"
else
    echo "==> Preserved existing Core config: $CORE_CONFIG (pass --overwrite-config to replace it)"
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

# 作用域决定的单元片段。
#
# - `@SCOPE_LINES@`：system 作用域要显式声明运行身份与家目录（`User=`/
#   `Group=`/`Environment=HOME=...`）。user 作用域里这些由用户管理器本身
#   提供，写死反而会绑到错误的人。
# - `@SCOPE_ENV@`：两条作用域都注入 `ECHO_SERVICE_SCOPE`——这是自更新插件
#   与 update.sh 判断该用 `systemctl` 还是 `systemctl --user` 的唯一依据，
#   比起"运行时探测"更不容易猜错（探测在单元文件残留时会误判）。
# - `@WANTED_BY@`：user=`default.target`，system=`multi-user.target`。
if [[ "$SERVICE_SCOPE" == system ]]; then
    UNIT_SCOPE_LINES="User=$SERVICE_USER
Group=$(id -gn "$SERVICE_USER" 2>/dev/null || echo "$SERVICE_USER")
Environment=HOME=$SERVICE_HOME
Environment=XDG_CONFIG_HOME=$SERVICE_HOME/.config
Environment=XDG_DATA_HOME=$SERVICE_HOME/.local/share
Environment=XDG_STATE_HOME=$SERVICE_HOME/.local/state"
    UNIT_WANTED_BY=multi-user.target
else
    UNIT_SCOPE_LINES=""
    UNIT_WANTED_BY=default.target
fi
UNIT_SCOPE_ENV="Environment=\"ECHO_SERVICE_SCOPE=$SERVICE_SCOPE\""

render_unit() {
    local source=$1 destination=$2
    sed \
        -e "s|@BINARY@|$LIBEXEC_DIR/echo-agent-core-bin|g" \
        -e "s|@UPDATE_SCRIPT@|$LIBEXEC_DIR/update.sh|g" \
        -e "s|@SOURCE_DIR@|$SOURCE_DIR|g" \
        -e "s|@CORE_CONFIG@|$CORE_CONFIG|g" \
        -e "s|@STATUS_FILE@|$STATUS_FILE|g" \
        -e "s|@SERVICE_PATH@|$SERVICE_PATH|g" \
        -e "s|@SCOPE_ENV@|$UNIT_SCOPE_ENV|g" \
        -e "s|@WANTED_BY@|$UNIT_WANTED_BY|g" \
        "$source" >"$destination"
    # @SCOPE_LINES@ 替换成**多行**片段（或整行删掉）。sed 的替换串里嵌换行
    # 可读性太差，改用 awk 整行替换；值经环境传入（awk -v 传不了换行）。
    if [[ -z "$UNIT_SCOPE_LINES" ]]; then
        sed -i '/@SCOPE_LINES@/d' "$destination"
    else
        ECHO_UNIT_SCOPE_LINES="$UNIT_SCOPE_LINES" awk '
            $0 == "@SCOPE_LINES@" { print ENVIRON["ECHO_UNIT_SCOPE_LINES"]; next }
            { print }
        ' "$destination" >"$destination.tmp"
        mv -f "$destination.tmp" "$destination"
    fi
    chmod 644 "$destination"
}

# 常驻用户管理器（linger）：user 作用域服务的**生命周期前提**。
#
# 不加这一步会发生什么（2026-10 实测，A100 节点）：unit 是 `--user` 作用域，
# 挂在 `default.target` 上，而 `default.target` 属于 `user@<uid>.service`。
# 该用户管理器平时**不存在**——它由第一个登录会话拉起、最后一个会话消失后
# 十秒被 logind 停掉，`user@` 一停，其下所有单元（含 Core）一并被停止。
#
# 后果不是"偶发"而是"必然"：运维 SSH 登出 → Core 死；下次有人登录 → Core
# 又活了。表现为"服务神秘掉线、重启时间与某人登录时间精确重合"。更糟的是
# `Restart=always` 在这里**完全无效**——unit 不是失败退出，是被上层正常
# 停止，systemd 语义里显式 stop 之后不自动重启。
#
# `loginctl enable-linger` 让该用户管理器开机即起、无人登录也保留，顺带修掉
# 另一个不易察觉的缺陷：**重启后无人登录时 Core 不会自动启动**。
#
# 权限：`set-self-linger` 的 polkit 默认 `allow_any = yes`，普通用户给自己
# 开免密；只有给别人开才需要授权。所以这步在 root 与普通用户安装下都能成功。
#
# 失败**不中断安装**（服务已经装好了，缺 linger 只是会在登出时停），但必须
# 显式告警并给出修复命令——静默降级正是这次排查花掉一小时的原因。
ensure_service_linger() {
    if [[ "${ECHO_SKIP_LINGER:-0}" == 1 ]]; then
        # system 作用域单元挂 multi-user.target，开机即起、与会话无关，
        # 没有"用户管理器随会话死掉"这个问题，也就不需要 linger。
        return 0
    fi
    local user
    user=$(id -un)
    if loginctl enable-linger "$user" 2>/dev/null &&
        [[ -e "/var/lib/systemd/linger/$user" ]]; then
        echo "==> Enabled linger for $user (service survives logout and starts at boot)"
        return 0
    fi
    {
        echo "warning: could not enable linger for $user."
        echo "         This service runs in the systemd **user** scope: without linger it"
        echo "         stops as soon as the last login session ends, and it will NOT start"
        echo "         at boot until someone logs in."
        echo "         fix: loginctl enable-linger $user"
    } >&2
    return 0
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
"${SYSTEMCTL[@]}" daemon-reload
"${SYSTEMCTL[@]}" enable echo-agent-core.service >/dev/null
# 生命周期前提（必须在 enable 之后：linger 只影响"用户管理器是否常驻"，
# 与单元自身的 enable 状态无关，但放在同一处便于对照阅读）。
# 见 ensure_service_linger 的注释：缺这一步，登出即停、重启不自启。
ensure_service_linger
if ((START_SERVICE)); then
    "${SYSTEMCTL[@]}" restart echo-agent-core.service
    core_healthy=0
    healthy_checks=0
    for _ in {1..15}; do
        sleep 1
        if "${SYSTEMCTL[@]}" is-active --quiet echo-agent-core.service; then
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

if [[ "$SERVICE_SCOPE" == system ]]; then
    scope_hint="systemctl status echo-agent-core.service"
    log_hint="journalctl -u echo-agent-core.service -f"
else
    scope_hint="systemctl --user status echo-agent-core.service"
    log_hint="journalctl --user -u echo-agent-core.service -f"
fi
cat <<EOF

Installation complete.
  Scope:    $SERVICE_SCOPE$( [[ "$SERVICE_SCOPE" == system ]] && echo " (runs as $SERVICE_USER)" )
  Launcher: $BIN_DIR/echo-agent-core
  Config:   $CORE_CONFIG
  Service:  $scope_hint
  Logs:     $log_hint

Install the Panel (TUI) frontend separately from the EchoAgentPanel repository.
EOF

# PATH 提示（2026-10）：launcher 装在 $BIN_DIR；若当前会话的 PATH 不含该
# 目录，装完直接敲 `echo-agent-core` 会 command not found——安装后最常见的
# 困惑（~/.profile 里的 PATH 片段只在**登录时**加载、且要求目录登录时已
# 存在，同一会话不会自动刷新）。这里显式检测并给出一行修复。
case ":${PATH:-}:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo
        echo "note: $BIN_DIR is not in PATH for this shell session —"
        echo "      'echo-agent-core' will not resolve until you do one of:"
        echo "        export PATH=\"$BIN_DIR:\$PATH\"     # current session, immediately"
        echo "        exec bash -l                      # or open a new terminal / re-login"
        echo "      or call it directly: $BIN_DIR/echo-agent-core"
        echo "      If it still fails after re-login, add the export line to ~/.profile."
        ;;
esac
