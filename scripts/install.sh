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
  systemd:    $SYSTEMD_DIR
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
if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "error: systemd user manager is unavailable for this user" >&2
    exit 1
fi

install -d -m 755 "$BIN_DIR" "$LIBEXEC_DIR" "$CONFIG_DIR" "$DATA_DIR" "$STATE_DIR" "$SYSTEMD_DIR"

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
if systemctl --user is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi
core_was_enabled=0
if systemctl --user is-enabled --quiet echo-agent-core.service 2>/dev/null; then
    core_was_enabled=1
fi
build_args=(build --release --locked -p echo-agent-core --manifest-path "$SOURCE_DIR/Cargo.toml")
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
