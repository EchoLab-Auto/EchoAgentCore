#!/usr/bin/env bash
set -Eeuo pipefail

# ═══════════════════════════════════════════════════════════════════════
# EchoAgentCore 安装器（统一入口，2026-11 合并）
#
#   --mode binary  （**默认**）下载预构建 release 产物并落盘。不需要 rust /
#                  cargo / git / cc，只要 curl tar sha256sum systemd。
#                  没有源码检出 → 不装 updater 单元，并在 modules.toml 里把
#                  `echo-agent.selfupdate` 关掉；内嵌 Web Panel 前端一并安装
#                  并启用（`--no-panel` 可跳过）。更新方式 = 重跑本脚本。
#   --mode source  克隆受管检出 → `cargo build --release --locked` → 装。
#                  需要 rust / cargo / git / cc；装 updater 单元（可自更新）；
#                  **从不创建或改写 modules.toml**，因此内嵌 Panel 与外部模块
#                  都不会被启用。
#
# 两条路的**安装布局完全一致**（同一套 derive_paths、同一个 launcher、同一份
# systemd unit 模板、同一份 core.toml 模板），所以：
#   - `scripts/uninstall.sh` 对两者通用；
#   - 装过一种再装另一种 = 原地换装，不会得到两套并存的部署。
# 布局一致是硬约束，由 `source/modules/core/tests/install_binary_mode.rs`
# 守护（单一 derive_paths、单一 unit 模板、产物目录契约）。
#
# ## 用法
#
#   ./scripts/install.sh                       # 二进制安装，最新 release
#   ./scripts/install.sh --mode source         # 源码安装
#   ./scripts/install.sh --version v0.2.0      # 指定 release tag
#   ./scripts/install.sh --artifact ./x.tar.gz # 本地/直链 tarball
#   ./scripts/install.sh --dry-run             # 只打印计划
#   curl -fsSL <raw-url>/scripts/install.sh | bash
#   curl -fsSL <raw-url>/scripts/install.sh | bash -s -- --mode source
#
# ## Panel（内嵌形态，仅 binary 模式）
#
# Panel 的模块代码**编译在 Core 二进制里**（`agent-module-panel`，模块 id
# `echo-agent.panel.web`），本脚本不装第二个二进制。缺的只有前端产物
# `web/dist`，装到 **Core 二进制同级**的 `web/`——那是内嵌模块
# `static_dir_candidates` 的第一个必然可判定命中项，因此零配置即可用。
#
# 为什么不是 `<prefix>/libexec/echo-agent-panel/web`（独立 Panel 的布局）：
# 2026-11 实测 panel 仓代码把它拼成了
# `<prefix>/libexec/libexec/echo-agent-panel/web`（`exe.parent()` 之后又 join
# 了一次 `libexec/`），干净机器上必然找不到。同级 `web/` 不受该缺陷影响，
# 且与二进制同版本淘汰——前端 API 契约随 Core 走，分开存只会得到"Core 升级
# 了、前端还是旧的"这种最难查的组合。
# ═══════════════════════════════════════════════════════════════════════

# BASH_SOURCE 兜底：sh/dash 管道执行时不可用——退回 $0；两者都没有
# （纯 stdin）时 PROJECT_ROOT 定位失败会走 bootstrap 分支（按设计）。
_SCRIPT_PATH="${BASH_SOURCE[0]:-${0:-}}"
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$_SCRIPT_PATH")/.." 2>/dev/null && pwd -P || true)
PROJECT_ROOT=${PROJECT_ROOT:-/nonexistent}

# ── 预扫描：模式与镜像 ────────────────────────────────────────────────
# 这两项必须在 bootstrap 之前就知道：
#   * 模式决定**要不要 clone**——二进制安装不 clone 仓库，也不该为此要求 git
#     （只有源码安装需要脚本以外的仓库资产）；
#   * 镜像决定 clone 走直连还是代理。
# 真正的参数校验留给后面的主解析（这里只看不判，未知取值一律交给主解析报错）。
PRESCAN_MODE=${ECHO_INSTALL_MODE:-binary}
CN_MIRROR_REQUEST=${ECHO_CN_MIRROR:-0}
_prescan_prev=""
for _arg in "$@"; do
    case "$_arg" in
        --mode=*)           PRESCAN_MODE=${_arg#--mode=} ;;
        --binary)           PRESCAN_MODE=binary ;;
        --source)           PRESCAN_MODE=source ;;
        --cn-mirror)        CN_MIRROR_REQUEST=1 ;;
        --cn-mirror=*)      CN_MIRROR_REQUEST=${_arg#--cn-mirror=} ;;
        --no-cn-mirror)     CN_MIRROR_REQUEST=0 ;;
    esac
    # `--mode source`（两段式）——与 `--scope <value>` 同一手法。
    if [[ "$_prescan_prev" == --mode ]]; then PRESCAN_MODE=$_arg; fi
    _prescan_prev=$_arg
done
unset _arg _prescan_prev
: "${PRESCAN_MODE:=binary}"

# ── 国内镜像加速（2026-10）────────────────────────────────────────────
# 国内网络下 GitHub 克隆 / 产物下载 / rustup 安装 / crates.io 依赖拉取常年
# 缓慢或不可达；以下机制提供"灵活、可单点、失败自动回退"的加速：
#   开启（本地）：  ./scripts/install.sh --cn-mirror[=rsproxy|tuna|ustc]
#   开启（一键）：  curl -fsSL <脚本地址> | ECHO_CN_MIRROR=1 bash
#   关闭：          --no-cn-mirror / ECHO_CN_MIRROR=0
#   单环节覆盖（独立于总开关，只加速需要的环节）：
#     ECHO_GITHUB_PROXY=https://gh-proxy.com    GitHub 克隆/下载加速前缀
#     ECHO_RUSTUP_INIT_URL / ECHO_RUSTUP_DIST_SERVER /
#     ECHO_RUSTUP_UPDATE_ROOT / ECHO_CRATES_MIRROR（sparse+https://…）
#     ECHO_DOWNLOAD_MIRROR                     release 产物下载前缀
# 语义：默认全直连（原有行为不变）；每个环节失败自动回退官方端点；
# crates 镜像在构建前做可达性预检。详见 README「国内网络」一节。
#
# 二进制模式下的取值：`--cn-mirror` 顺带把产物下载指向同一个 GitHub 代理
# （`<proxy>/https://github.com`），因此两种模式只需要记一个开关，不必分别
# 记 `--mirror`。
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

# bootstrap 前置依赖：git 自动安装（curl|bash 在源码安装上的第一道门槛）。
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

# ── curl|bash 一键安装引导（2026-10）──────────────────────────────────
# **只有源码模式需要**：它要脚本以外的仓库资产（受管检出源头、update.sh、
# config 与 unit 模板都在仓库里）。二进制模式的全部输入都在 release 产物里，
# 因此不 clone、不要求 git。
#
# 通过管道运行时脚本所在目录不是 git 检出——此时先把仓库浅克隆到临时
# bootstrap 目录，再 exec 克隆里的 install.sh 接力（后续流程与本地运行完全
# 一致：依赖自动安装 → 受管检出 → 构建 → systemd 服务）。
DEFAULT_REPO="https://github.com/EchoLab-Auto/EchoAgentCore.git"
if [[ "$PRESCAN_MODE" == source ]] &&
    [[ ! -d "$PROJECT_ROOT/.git" ]] &&
    [[ -z "${ECHO_BOOTSTRAPPED:-}" ]]; then
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

# ── 布局：**唯一实现** ────────────────────────────────────────────────
# 全部安装路径都由「服务用户的 HOME」推导。
#
# 做成函数而不是一段直线代码：system 作用域下 HOME 可能**不是**当前 shell 的
# $HOME。典型情形是 `sudo scripts/install.sh --scope=system`——sudo 把 HOME
# 换成 /root，但服务该以谁的身份运行是另一回事（默认取 $SUDO_USER）。所以
# 参数解析完拿到 --service-user 之后要按对应用户的家目录重算一次。
#
# honor_xdg=1 表示沿用调用者的 XDG_* 覆盖（同一用户安装时的既有行为）；
# 跨用户安装必须用目标用户自己的默认布局，否则会把路径指到调用者的目录里。
#
# 两种安装模式共用这一份推导（二进制安装也有 SOURCE_DIR 这个键，只是不建
# 那个目录），所以 uninstall.sh 只认一套布局。守护测试会数这里的定义次数：
# 出现第二份 = 两种装法开始漂移。
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
    # 模块安装根（约定：`modules/<包名>/<二进制>`，见模块开发指南「安装目录约定」）。
    MODULES_DIR="$LIBEXEC_DIR/modules"
    BIN_DIR="$PREFIX/bin"
    CORE_CONFIG="$CONFIG_DIR/core.toml"
    MODULES_CONFIG="$CONFIG_DIR/modules.toml"
    STATUS_FILE="$STATE_DIR/update-status"
    # 内嵌 Panel 前端：Core 二进制同级 web/（见文件头说明）。
    PANEL_WEB_DIR="$LIBEXEC_DIR/web"
}
derive_paths "$HOME" 1

# 随**源码安装**一起落地的进程外模块二进制（`modules/<包名>/<包名>`）：
#
#   MODULE_BINARIES   本仓 `source/modules/*` 里带 `[[bin]] path = "src/main.rs"`
#                     的 crate（`src/bin/*` 是测试夹具，不装）。它们是源码树
#                     的一部分——从这份源码构建出来的东西就该跟着装。
#
# 外部仓的模块（当前只有 `agent-module-checklist`）**刻意不在这张表里**：
# 源码安装只带 Core 本体。理由是形态而非省事——外部模块一旦装成独立产物，
# 就得在 `modules.toml` 里写启用行才会真的跑起来，而 `modules.toml` 是用户
# 手写的计划文件（源码模式**从不创建或改写它**）。装了不写行 = 落一个永远
# 不执行的二进制，白占几 MB，还让人误以为清单已经进程外运行。
#
# 清单功能在源码安装下**完整可用**：它作为 Core 的 git 依赖编进二进制，
# 走内置形态（`main.rs` 的 `declares_external` 读不到启用行就返回 false）。
# 想把清单切成独立产物，请用 `--mode binary`（产物里带二进制，并在
# modules.toml 里留好注释示例），或自己往 `modules.toml` 加一行。
#
# 守护测试 source/modules/core/tests/install_script_flag.rs 盯住"这里必须是
# 空的"；新增/删除本仓模块时改本表即可。
MODULE_BINARIES=()

# ── 参数状态 ──────────────────────────────────────────────────────────
# 安装模式：binary（默认，预构建产物）/ source（源码构建）。
INSTALL_MODE=${ECHO_INSTALL_MODE:-}
# 二进制模式：产物来源。
ARTIFACT=""
VERSION=""
# 二进制模式：release 查找上下文（owner/repo）；源码模式：自更新上游 git URL。
DEFAULT_REPOSITORY="EchoLab-Auto/EchoAgentCore"
REPOSITORY=${ECHO_REPOSITORY:-$DEFAULT_REPOSITORY}
REPOSITORY_URL=${ECHO_REPOSITORY_URL:-}
DOWNLOAD_MIRROR=${ECHO_DOWNLOAD_MIRROR:-}
VERIFY_CHECKSUM=1
ALLOW_GLIBC_MISMATCH=0
INSTALL_PANEL=1
# Release 资产与脚本的公共前缀；`--mirror` 只替换这一段。
CANONICAL_PREFIX="https://github.com"
# 二进制模式从产物 manifest 读出的版本（源码模式恒空；收尾摘要与状态文件用）。
manifest_version=""

# 服务作用域（2026-10）：`user`（默认，`systemctl --user`）或 `system`
# （`/etc/systemd/system`，`systemctl` 无 --user）。
#
# 为什么需要这个开关：自更新链路（`agent-module-self-update` 启动 updater 单元、
# `update.sh` 重启 Core）此前把 `--user` 写死在代码里，于是**只有 user 作用域
# 的自更新是通的**。system 作用域的老装法只能靠一个"假 systemctl"（PATH shim
# 把 `--user` 抹掉）硬凑，core 自身那次调用还凑不上——按钮点了只会报错。
# 现在作用域是一等参数：单元里注入 `ECHO_SERVICE_SCOPE`，模块与 update.sh 都
# 读它，两条作用域都走真实路径，不再需要任何 shim。
SERVICE_SCOPE=${ECHO_SERVICE_SCOPE:-user}
# system 作用域下服务以谁的身份运行（本版本固定 root，见下方作用域解析）。
SERVICE_USER=${ECHO_SERVICE_USER:-}
# 显式初始化：脚本开了 set -u，而这两个变量只在 system 作用域分支里赋值。
SERVICE_HOME=""
ECHO_SKIP_LINGER=0
SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
SYSTEMCTL=(systemctl --user)
START_SERVICE=1
DRY_RUN=0
# 配置文件策略（2026-10）：默认保留已有 core.toml；--overwrite-config
# 以最新模板重写（旧文件先备份为 core.toml.bak-<epoch>；
# management_access_token 沿用旧值——重写配置不应顺带换掉 Panel/对端凭证）。
OVERWRITE_CONFIG=0
# 前置依赖策略（2026-10，仅源码模式）：默认自动安装缺失项（可识别包管理器 +
# sudo 可用时）；--no-deps 跳过，只做检查并提示。
INSTALL_DEPS=1

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [options]

Install EchoAgentCore. Two modes, selected with --mode:

  binary  (default)  Download a prebuilt release artifact and install it.
                     Needs no rust, cargo, git or cc. There is no source
                     checkout, so the in-app updater is disabled and the
                     embedded Web Panel frontend is installed and enabled.
                     Update by re-running this script (idempotent).

  source            Clone the managed source checkout, build with
                     `cargo build --release --locked`, and install.
                     Needs rust, cargo, git and cc. The updater unit is
                     installed (self-update works). modules.toml is never
                     created or modified, so neither the embedded Panel
                     nor external modules are enabled.

Mode:
  --mode <binary|source>  Installation mode. Default: binary (or
                          $ECHO_INSTALL_MODE).
  --binary                Alias for --mode binary.
  --source                Alias for --mode source.

Artifact source (binary mode; pick at most one):
  --artifact <path|url>  Install from this tarball (local path or http(s) URL).
                         Without it, the latest GitHub release is downloaded.
  --version <tag>        Release tag to download (e.g. v0.1.0 or 0.1.0).
                         Default: the repository's latest release.
  --repository <o/r>     GitHub owner/repo used for release lookup
                         (default: EchoLab-Auto/EchoAgentCore).
  --mirror <prefix>      Rewrite https://github.com to this prefix when
                         downloading (e.g. https://ghfast.top/https://github.com).
                         Also readable from ECHO_DOWNLOAD_MIRROR.
  --no-panel             Do not install/enable the embedded Web Panel.
  --skip-checksum        Do not require the `.sha256` sidecar of the artifact
                         (not recommended: it is the only integrity check).
  --allow-glibc-mismatch Continue when the host glibc is older than the build
                         baseline in the artifact manifest.

Source (source mode):
  --source-repo <url>    Upstream Git repository recorded for self-update
                         (default: origin of this checkout, else the public
                         repo). Env: ECHO_REPOSITORY_URL.
  --no-deps              Skip automatic installation of missing prerequisites
                         (only check and report).

Service:
  --scope <user|system>  Which systemd scope to install into (default: user).
                         `user` writes ~/.config/systemd/user units, drives them
                         with `systemctl --user`, and enables linger so the
                         service survives logout.
                         `system` writes /etc/systemd/system units, requires
                         root, and does not touch linger.
  --service-user <name>  Service account for system scope (currently root only).
  --no-start             Install and enable the unit without starting Core.

Installation:
  --overwrite-config     Replace an existing core.toml with the shipped template
                         (previous file kept as core.toml.bak-<epoch>; an existing
                         management_access_token is carried over).
  --prefix <dir>         Install prefix (default: ~/.local, or $ECHO_PREFIX).
  --dry-run              Print resolved paths and the install plan, then exit
                         (no network, no writes; works without systemd).
  -h, --help             Show this help.

Mirrors (slow networks):
  --cn-mirror[=PRESET]   Accelerate GitHub / rustup / crates.io / artifact
                         downloads (PRESET: rsproxy|tuna|ustc; default rsproxy).
                         Per-component overrides via environment variables:
                         ECHO_GITHUB_PROXY, ECHO_RUSTUP_INIT_URL,
                         ECHO_RUSTUP_DIST_SERVER, ECHO_RUSTUP_UPDATE_ROOT,
                         ECHO_CRATES_MIRROR, ECHO_DOWNLOAD_MIRROR. Every mirror
                         falls back to the official endpoint on failure.
  --no-cn-mirror         Disable mirrors (overrides ECHO_CN_MIRROR).
EOF
}

while (($#)); do
    case "$1" in
        --mode)
            INSTALL_MODE=${2:-}
            shift 2
            ;;
        --mode=*)
            INSTALL_MODE=${1#--mode=}
            shift
            ;;
        --binary)
            INSTALL_MODE=binary
            shift
            ;;
        --source)
            INSTALL_MODE=source
            shift
            ;;
        --source-repo)
            REPOSITORY_URL=${2:-}
            shift 2
            ;;
        --source-repo=*)
            REPOSITORY_URL=${1#--source-repo=}
            shift
            ;;
        --artifact)
            ARTIFACT=${2:-}
            shift 2
            ;;
        --artifact=*)
            ARTIFACT=${1#--artifact=}
            shift
            ;;
        --version)
            VERSION=${2:-}
            shift 2
            ;;
        --version=*)
            VERSION=${1#--version=}
            shift
            ;;
        --repository)
            REPOSITORY=${2:-}
            shift 2
            ;;
        --repository=*)
            REPOSITORY=${1#--repository=}
            shift
            ;;
        --mirror)
            DOWNLOAD_MIRROR=${2:-}
            shift 2
            ;;
        --mirror=*)
            DOWNLOAD_MIRROR=${1#--mirror=}
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
        --no-panel)
            INSTALL_PANEL=0
            shift
            ;;
        --skip-checksum)
            VERIFY_CHECKSUM=0
            shift
            ;;
        --allow-glibc-mismatch)
            ALLOW_GLIBC_MISMATCH=1
            shift
            ;;
        --prefix)
            ECHO_PREFIX=${2:-}
            shift 2
            ;;
        --prefix=*)
            ECHO_PREFIX=${1#--prefix=}
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

# ── 模式归一 ──────────────────────────────────────────────────────────
INSTALL_MODE=${INSTALL_MODE:-binary}
case "$INSTALL_MODE" in
    binary|source) ;;
    *)
        echo "error: --mode must be 'binary' or 'source' (got: '$INSTALL_MODE')" >&2
        exit 2
        ;;
esac

# 模式专属参数用错了**只告警不拒绝**：外部编排脚本常把一整组开关一起传进来，
# 硬拒绝会让"用二进制装但顺手带了 --no-deps"这种无害组合直接失败。
if [[ "$INSTALL_MODE" == binary ]]; then
    if ((INSTALL_DEPS == 0)); then
        echo "warning: --no-deps has no effect in binary mode (nothing is compiled)" >&2
    fi
    if [[ -n "$REPOSITORY_URL" ]]; then
        echo "warning: --source-repo only applies to --mode source; ignoring it" >&2
    fi
else
    if [[ -n "$ARTIFACT" || -n "$VERSION" ]]; then
        echo "warning: --artifact/--version only apply to --mode binary; ignoring them" >&2
    fi
    if ((INSTALL_PANEL == 0)); then
        echo "note: --no-panel is a no-op in source mode (the Panel is never installed)" >&2
    fi
    if ((VERIFY_CHECKSUM == 0)); then
        echo "warning: --skip-checksum only applies to --mode binary; ignoring it" >&2
    fi
fi

# `--prefix` 会在解析后被 derive_paths 重新读一次（ECHO_PREFIX）。
if [[ -n "${ECHO_PREFIX:-}" ]]; then
    derive_paths "$HOME" 1
fi

# 镜像开关 → 产物下载前缀：显式 `--mirror`/`ECHO_DOWNLOAD_MIRROR` 优先，
# 否则沿用 `--cn-mirror` 选出的 GitHub 代理（两种模式只记一个开关）。
if [[ -z "$DOWNLOAD_MIRROR" && -n "$GITHUB_PROXY" ]]; then
    DOWNLOAD_MIRROR="${GITHUB_PROXY%/}/https://github.com"
fi

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
    # 可用性检查在 `--dry-run` 下只告警不退出：预演的意义正是"先看清要装到哪，
    # 再决定要不要在这台机器上装"。何况容器 / CI 里根本没有 user manager，硬
    # 拦截会让 dry-run 在最需要它预览的环境里失效（2026-11 在 ubuntu:24.04
    # 容器里实测踩到）。
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        if ((DRY_RUN)); then
            echo "warning: systemd user manager is unavailable here; a real install" >&2
            echo "         would fail unless this host has one (or use --scope system)." >&2
        else
            echo "error: systemd user manager is unavailable for this user" >&2
            echo "       (user scope needs a running user manager; on servers that lack one," >&2
            echo "        use --scope system instead)" >&2
            exit 1
        fi
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

# ── 源码模式：上游仓库与分支 ──────────────────────────────────────────
if [[ "$INSTALL_MODE" == source ]]; then
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
fi

# ── 架构与版本推导（二进制模式）────────────────────────────────────────
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64) printf 'x86_64\n' ;;
        aarch64|arm64) printf 'aarch64\n' ;;
        *)
            echo "error: unsupported architecture: $(uname -m) (need x86_64 or aarch64)" >&2
            return 1
            ;;
    esac
}

normalize_tag() {
    # 接受 `0.1.0` 与 `v0.1.0`，统一成带 v 的 tag；文件名里去掉 v。
    local raw=$1
    raw=${raw#refs/tags/}
    if [[ "$raw" == v* ]]; then
        printf '%s\n' "$raw"
    else
        printf 'v%s\n' "$raw"
    fi
}

artifact_file=""
artifact_url=""
release_tag=""
artifact_arch=""
if [[ "$INSTALL_MODE" == binary ]]; then
    artifact_arch=$(detect_arch)
    if [[ -n "$VERSION" ]]; then
        release_tag=$(normalize_tag "$VERSION")
    fi
    if [[ -n "$ARTIFACT" ]]; then
        case "$ARTIFACT" in
            http://*|https://*)
                artifact_url=$ARTIFACT
                # 从 URL 里反推版本号，仅用于 manifest 交叉核对与状态文件。
                release_tag=${release_tag:-$(basename -- "${ARTIFACT%%\?*}" | sed -E 's/^echo-agent-core-(.*)-linux-[a-z0-9_]+\.tar\.gz$/\1/')}
                ;;
            *)
                if [[ ! -f "$ARTIFACT" ]]; then
                    echo "error: artifact not found: $ARTIFACT" >&2
                    exit 1
                fi
                artifact_file=$(CDPATH= cd -- "$(dirname -- "$ARTIFACT")" && pwd -P)/$(basename -- "$ARTIFACT")
                ;;
        esac
    fi
fi

mirror_url() {
    local url=$1
    if [[ -z "$DOWNLOAD_MIRROR" || "$url" != "$CANONICAL_PREFIX"/* ]]; then
        printf '%s\n' "$url"
        return 0
    fi
    local prefix="${DOWNLOAD_MIRROR%/}"
    # 两种 mirror 写法都要支持，判据是"前缀里有没有 github.com"：
    #
    #   1. 含域名（ghfast.top / gh-proxy.com 的标准用法）：
    #        https://ghfast.top/https://github.com
    #      前缀后面直接接**路径**，即 `/EchoLab-Auto/EchoAgentCore/...`。
    #   2. 纯代理域（自建反代常见）：https://proxy.example
    #      前缀后面接**完整 URL**，即 `/https://github.com/EchoLab-Auto/...`。
    #
    # 早先这里只处理第 2 种（无条件剥掉 `https://` 再拼），于是第 1 种拼出了
    # `https://ghfast.top/https://github.com/github.com/EchoLab-Auto/...`——
    # 域名重复，服务器回 404。2026-11 在 ubuntu:24.04 容器里实测踩到。
    if [[ "$prefix" == *github.com ]]; then
        printf '%s/%s\n' "$prefix" "${url#"$CANONICAL_PREFIX"/}"
    else
        printf '%s/%s\n' "$prefix" "$url"
    fi
}

# ── 计划输出（--dry-run 走这条，**不联网、不落盘**）──────────────────
if ((DRY_RUN)); then
    if [[ "$INSTALL_MODE" == binary ]]; then
        if [[ -n "$artifact_file" ]]; then
            artifact_desc=$artifact_file
        elif [[ -n "$artifact_url" ]]; then
            artifact_desc=$(mirror_url "$artifact_url")
        elif [[ -n "$release_tag" ]]; then
            artifact_desc=$(mirror_url "$CANONICAL_PREFIX/$REPOSITORY/releases/download/$release_tag/echo-agent-core-${release_tag#v}-linux-$artifact_arch.tar.gz")
        else
            artifact_desc="latest release of $REPOSITORY for linux-$artifact_arch (resolved at run time)"
        fi
        echo "EchoAgentCore install plan (mode: binary)"
        echo "  scope:      $SERVICE_SCOPE$( [[ "$SERVICE_SCOPE" == system ]] && echo " (root)" )"
        echo "  artifact:   $artifact_desc"
        echo "  prefix:     $PREFIX"
        echo "  binary:     $LIBEXEC_DIR/echo-agent-core-bin"
        echo "  launcher:   $BIN_DIR/echo-agent-core"
        echo "  panel web:  $PANEL_WEB_DIR$( ((INSTALL_PANEL)) || echo '  (panel disabled)' )"
        echo "  config:     $CONFIG_DIR"
        echo "  data:       $DATA_DIR"
        echo "  state:      $STATE_DIR"
        echo "  systemd:    $SYSTEMD_DIR ($SERVICE_SCOPE scope)"
        echo "  updater:    not installed (binary install has no source checkout)"
        exit 0
    fi
    # 镜像摘要（dry-run 也能核对配置是否正确）。
    mirror_summary="off"
    mirror_parts=()
    if [[ -n "$GITHUB_PROXY" ]]; then mirror_parts+=("github=$GITHUB_PROXY"); fi
    if [[ -n "$RUSTUP_DIST_SERVER" ]]; then mirror_parts+=("rustup=$RUSTUP_DIST_SERVER"); fi
    if [[ -n "$CRATES_MIRROR" ]]; then mirror_parts+=("crates=$CRATES_MIRROR"); fi
    if ((${#mirror_parts[@]})); then mirror_summary="on (${mirror_parts[*]})"; fi
    cat <<EOF
EchoAgentCore install plan (mode: source)
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
  modules.toml: never written (source mode leaves module enablement to you)
EOF
    exit 0
fi

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: the installer currently supports Linux/systemd only" >&2
    exit 1
fi

# ═══════════════════════════════════════════════════════════════════════
# 阶段一：取产物（模式相关）
#
# 两种模式在这一阶段结束时交出同一组变量，后续安装流程完全不区分模式：
#   BINARY_SOURCE   要装的 Core 二进制
#   SKILLS_SOURCE   出厂技能目录
#   CONFIG_TEMPLATE core.toml 模板
#   UNIT_TEMPLATE   core 单元模板
#   REVISION        写入 installed-revision 的版本标识
#   VERSION_INFO    收尾摘要里那行版本
# ═══════════════════════════════════════════════════════════════════════

if [[ "$INSTALL_MODE" == binary ]]; then
    # ── 前置工具（二进制模式的全部依赖）────────────────────────────────
    missing_tools=()
    for tool in tar install sha256sum sed awk grep; do
        command -v "$tool" >/dev/null 2>&1 || missing_tools+=("$tool")
    done
    if [[ -z "$artifact_file" ]]; then
        command -v curl >/dev/null 2>&1 || missing_tools+=("curl")
    fi
    if ((${#missing_tools[@]})); then
        echo "error: missing required tools: ${missing_tools[*]}" >&2
        echo "       install them with your package manager and re-run." >&2
        exit 1
    fi

    WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/echo-agent-install.XXXXXX")
    cleanup_work_dir() { rm -rf -- "$WORK_DIR"; }
    trap cleanup_work_dir EXIT

    download() { # url destination [max_time_secs]
        local url=$1 destination=$2 max_time=${3:-600}
        # `--connect-timeout` 只管到"连接建立"，连上之后读数据慢仍会无限等。发版产物
        # 15MB、镜像链路抖动时，没有 `--max-time` 就是"看起来在下载、实际永不到头"。
        curl -fL --retry 3 --retry-delay 2 --connect-timeout 15 --max-time "$max_time" \
            --proto '=https,http' --proto-redir '=https' \
            -o "$destination" "$url"
    }

    latest_release_tag() {
        # 不用 jq（新机器上可能没有）：扁平 JSON 里 `"tag_name": "v0.1.0"` 足够
        # 稳定地取出来。接口失败就明确要求 --version，不猜。
        local api="https://api.github.com/repos/$REPOSITORY/releases/latest" body
        if ! body=$(curl -fsSL --connect-timeout 15 "$api" 2>/dev/null); then
            return 1
        fi
        printf '%s\n' "$body" |
            grep -m1 '"tag_name"' |
            sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
    }

    json_string() { # file key
        tr -d '\n' <"$1" |
            sed -n -E "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"([^\"]*)\".*/\1/p" |
            head -1
    }

    acquire_artifact() {
        # 下载（或就地使用）→ 校验 → 解包 → manifest 校验。失败即退出，
        # 此时尚未动过任何系统路径。
        if [[ -z "$artifact_file" ]]; then
            if [[ -z "$release_tag" ]]; then
                echo "==> Resolving latest release of $REPOSITORY"
                if ! release_tag=$(latest_release_tag) || [[ -z "$release_tag" ]]; then
                    echo "error: could not resolve the latest release from the GitHub API." >&2
                    echo "       Pass --version <tag> explicitly, or --artifact <path|url>." >&2
                    exit 1
                fi
            fi
            release_tag=$(normalize_tag "$release_tag")
            if [[ -z "$artifact_url" ]]; then
                artifact_url="$CANONICAL_PREFIX/$REPOSITORY/releases/download/$release_tag/echo-agent-core-${release_tag#v}-linux-$artifact_arch.tar.gz"
            fi
            artifact_file="$WORK_DIR/$(basename -- "${artifact_url%%\?*}")"
            echo "==> Downloading $release_tag for linux-$artifact_arch"
            if ! download "$(mirror_url "$artifact_url")" "$artifact_file"; then
                echo "error: download failed: $artifact_url" >&2
                if [[ -n "$DOWNLOAD_MIRROR" ]]; then
                    echo "       mirror was: $(mirror_url "$artifact_url")" >&2
                fi
                echo "       Slow or blocked network? retry with --cn-mirror or --mirror <prefix>." >&2
                exit 1
            fi
        fi

        # ── 校验（tarball 旁的 .sha256 是唯一的完整性依据）──────────────
        if ((VERIFY_CHECKSUM)); then
            checksum_source=""
            checksum_failure=""
            checksum_url=""
            if [[ -f "$artifact_file.sha256" ]]; then
                checksum_source="$artifact_file.sha256"
            elif [[ -n "$artifact_url" ]]; then
                checksum_url="$(mirror_url "$artifact_url").sha256"
                if ! checksum_failure=$(download "$checksum_url" "$WORK_DIR/artifact.sha256" 120 2>&1); then
                    checksum_source=""
                else
                    checksum_source="$WORK_DIR/artifact.sha256"
                    checksum_failure=""
                fi
            fi
            if [[ -z "$checksum_source" ]]; then
                # **必须区分"产物没发侧车"和"侧车没下下来"**。2026-11 容器实测：
                # 镜像链路抖动（Connection reset）让一个 108 字节的 .sha256 下载
                # 失败，而当时的报错是"no checksum sidecar found … expected next
                # to the artifact"——用户会去怀疑发布流程漏发文件，实际是网络。
                echo "error: cannot verify $(basename -- "$artifact_file"): no .sha256 available" >&2
                if [[ -z "$checksum_failure" ]]; then
                    echo "       $(basename -- "$artifact_file").sha256 was not found next to the artifact." >&2
                    echo "       Pass --skip-checksum to install anyway (not recommended)." >&2
                elif [[ "$checksum_failure" == *404* || "$checksum_failure" == *"not found"* ]]; then
                    echo "       the release does not publish $(basename -- "$checksum_url")" >&2
                    echo "       Pass --skip-checksum to install anyway (not recommended)." >&2
                else
                    echo "       downloading it failed: $(printf '%s' "$checksum_failure" | tail -3 | tr '\n' ' ')" >&2
                    echo "       url: $checksum_url" >&2
                    echo "       this is a network failure, not a missing artifact — retry, or" >&2
                    echo "       pass --cn-mirror / --mirror <prefix> / --skip-checksum." >&2
                fi
                exit 1
            fi
            expected=$(awk 'NR==1{print $1}' "$checksum_source")
            actual=$(sha256sum "$artifact_file" | awk '{print $1}')
            if [[ -z "$expected" || "$expected" != "$actual" ]]; then
                echo "error: checksum mismatch for $(basename -- "$artifact_file")" >&2
                echo "       expected: ${expected:-<empty>}" >&2
                echo "       actual:   $actual" >&2
                exit 1
            fi
            echo "==> Checksum verified"
        else
            echo "warning: checksum verification skipped (--skip-checksum)" >&2
        fi

        # ── 解包 + manifest 校验 ────────────────────────────────────────
        UNPACK_DIR="$WORK_DIR/unpacked"
        install -d -m 700 "$UNPACK_DIR"
        echo "==> Unpacking artifact"
        tar -xzf "$artifact_file" -C "$UNPACK_DIR"
        # `modules/` 也列为必需：外部模块二进制是产物的一部分。缺了它不报错的话，
        # 装完只是"清单工具不见了"，而那时已经离发布很远，很难归因。
        for required in bin/echo-agent-core-bin manifest.json systemd/echo-agent-core.service.in config/echo-agent-core.toml skills modules; do
            if [[ ! -e "$UNPACK_DIR/$required" ]]; then
                echo "error: artifact is missing '$required' — not an EchoAgentCore release tarball" >&2
                exit 1
            fi
        done
        if ((INSTALL_PANEL)) && [[ ! -f "$UNPACK_DIR/web/index.html" ]]; then
            echo "error: artifact has no web/index.html (embedded Panel frontend)" >&2
            echo "       Re-build the release with the panel frontend, or pass --no-panel." >&2
            exit 1
        fi

        # manifest 是构建系统写的**事实来源**：架构与 glibc 基线都在里面。
        manifest="$UNPACK_DIR/manifest.json"
        manifest_arch=$(json_string "$manifest" arch)
        manifest_glibc=$(json_string "$manifest" glibc_min)
        manifest_version=$(json_string "$manifest" version)
        manifest_revision=$(json_string "$manifest" revision)

        if [[ -n "$manifest_arch" && "$manifest_arch" != "$artifact_arch" ]]; then
            echo "error: artifact is built for '$manifest_arch' but this host is '$artifact_arch'" >&2
            exit 1
        fi

        # glibc 基线：二进制在较新的发行版上构建，老发行版缺符号时的报错是
        # `version 'GLIBC_2.xx' not found`，那一行对用户毫无指向性——在安装阶段
        # 拦住并说清版本差，比装完启动失败好得多。
        if [[ -n "$manifest_glibc" ]]; then
            host_glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
            if [[ -n "$host_glibc" ]]; then
                lower=$(printf '%s\n%s\n' "$manifest_glibc" "$host_glibc" | sort -V | head -1)
                if [[ "$lower" != "$manifest_glibc" ]]; then
                    if ((ALLOW_GLIBC_MISMATCH)); then
                        echo "warning: host glibc $host_glibc is older than the build baseline $manifest_glibc;" >&2
                        echo "         continuing because --allow-glibc-mismatch was given" >&2
                    else
                        echo "error: host glibc $host_glibc is older than the build baseline $manifest_glibc" >&2
                        echo "       this binary will fail at startup with 'GLIBC_2.xx not found'." >&2
                        echo "       Upgrade the distribution, or install from source with --mode source." >&2
                        echo "       Pass --allow-glibc-mismatch to try anyway." >&2
                        exit 1
                    fi
                fi
            fi
        fi

        echo "==> Artifact: version=${manifest_version:-unknown} arch=${manifest_arch:-unknown} revision=${manifest_revision:-unknown}"

        BINARY_SOURCE="$UNPACK_DIR/bin/echo-agent-core-bin"
        SKILLS_SOURCE="$UNPACK_DIR/skills"
        CONFIG_TEMPLATE="$UNPACK_DIR/config/echo-agent-core.toml"
        UNIT_TEMPLATE="$UNPACK_DIR/systemd/echo-agent-core.service.in"
        REVISION=${manifest_revision:-unknown}
        VERSION_INFO="version=${manifest_version:-unknown}${manifest_revision:+ ($manifest_revision)}"
    }
    acquire_artifact

    # 单元模板里 @SOURCE_DIR@ 的唯一用途是 WorkingDirectory：二进制安装没有
    # 源码检出，数据目录是它合理的工作目录。
    UNIT_SOURCE_DIR="$DATA_DIR"

else
    # ── 源码模式：前置依赖（检查 + 自动安装，2026-10）─────────────────
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

    # ── 受管源码检出 ───────────────────────────────────────────────────
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

    # ── 构建 ───────────────────────────────────────────────────────────
    echo "==> Building release binary"
    build_args=(build --release --locked -p echo-agent-core)
    # 本仓的进程外模块二进制与 Core 同一次 cargo 调用构建（同一个 target 目录、
    # 同一套 --locked / 镜像参数），避免第二轮解析依赖。
    # 外部仓模块不在 `-p` 里 —— 见文件顶部 MODULE_BINARIES 的注释（源码安装
    # 只带 Core 本体；`-p` 一个模块会额外产出独立二进制，那是二进制安装的事）。
    for _module in "${MODULE_BINARIES[@]}"; do
        build_args+=(-p "$_module")
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
    unset _module

    BINARY_SOURCE="$SOURCE_DIR/target/release/echo-agent-core"
    SKILLS_SOURCE="$SOURCE_DIR/skills"
    CONFIG_TEMPLATE="$PROJECT_ROOT/config/echo-agent-core.toml"
    UNIT_TEMPLATE="$PROJECT_ROOT/packaging/systemd/echo-agent-core.service.in"
    UNIT_SOURCE_DIR="$SOURCE_DIR"
    REVISION=$(git -C "$SOURCE_DIR" rev-parse HEAD)
    VERSION_INFO="revision=$REVISION"
fi

# ═══════════════════════════════════════════════════════════════════════
# 阶段二：事务安装（两种模式共用）
# ═══════════════════════════════════════════════════════════════════════

# Keep every file that can affect a running installation until the new service
# has been reloaded and passes its health check. This makes re-running the
# installer recoverable even when activation fails.
core_was_active=0
if "${SYSTEMCTL[@]}" is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi
core_was_enabled=0
if "${SYSTEMCTL[@]}" is-enabled --quiet echo-agent-core.service 2>/dev/null; then
    core_was_enabled=1
fi

install -d -m 755 "$BIN_DIR" "$LIBEXEC_DIR" "$MODULES_DIR" "$CONFIG_DIR" "$DATA_DIR" "$STATE_DIR" "$SYSTEMD_DIR"

ROLLBACK_DIR="$STATE_DIR/install-rollback"
install -d -m 700 "$ROLLBACK_DIR"
# 两种模式的落盘集合取**并集**：多快照几个不存在的文件不花代价，漏一个就要在
# 事故现场手工拼回去。
for rollback_file in binary updater launcher core_unit update_unit core_config modules_config revision status; do
    rm -f "$ROLLBACK_DIR/$rollback_file"
done
rm -rf -- "$ROLLBACK_DIR/web" "$ROLLBACK_DIR/modules"

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
snapshot_file "$MODULES_CONFIG" "$ROLLBACK_DIR/modules_config"
snapshot_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
snapshot_file "$STATUS_FILE" "$ROLLBACK_DIR/status"
for _module in "${MODULE_BINARIES[@]}"; do
    snapshot_file "$MODULES_DIR/$_module/$_module" "$ROLLBACK_DIR/module-$_module"
done
unset _module
if [[ -d "$PANEL_WEB_DIR" ]]; then
    cp -a -- "$PANEL_WEB_DIR" "$ROLLBACK_DIR/web"
fi
# 模块二进制：装的时候是"逐个原子替换"，回滚必须整体还原——
# 新 Core + 旧清单模块（或反过来）会让协议版本对不上。
if [[ -d "$MODULES_DIR" ]]; then
    cp -a -- "$MODULES_DIR" "$ROLLBACK_DIR/modules"
fi

INSTALL_TRANSACTION=1
INSTALL_PHASE=installing
install_rollback() {
    local code=$?
    trap - ERR
    set +e
    if [[ "${INSTALL_TRANSACTION:-0}" == 1 ]]; then
        restore_file "$LIBEXEC_DIR/echo-agent-core-bin" "$ROLLBACK_DIR/binary"
        restore_file "$LIBEXEC_DIR/update.sh" "$ROLLBACK_DIR/updater"
        for _module in "${MODULE_BINARIES[@]}"; do
            restore_file "$MODULES_DIR/$_module/$_module" "$ROLLBACK_DIR/module-$_module"
        done
        restore_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
        restore_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
        restore_file "$SYSTEMD_DIR/echo-agent-core-update.service" "$ROLLBACK_DIR/update_unit"
        restore_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
        # modules.toml **只有二进制模式才归安装器管**：源码模式从不创建/改写它，
        # 回滚时删掉它等于替用户销毁一份手写计划文件。
        if [[ "$INSTALL_MODE" == binary ]]; then
            restore_file "$MODULES_CONFIG" "$ROLLBACK_DIR/modules_config"
        fi
        restore_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
        if [[ -d "$ROLLBACK_DIR/web" ]]; then
            rm -rf -- "$PANEL_WEB_DIR"
            cp -a -- "$ROLLBACK_DIR/web" "$PANEL_WEB_DIR"
        fi
        if [[ -d "$ROLLBACK_DIR/modules" ]]; then
            rm -rf -- "$MODULES_DIR"
            cp -a -- "$ROLLBACK_DIR/modules" "$MODULES_DIR"
        fi
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
        mv -f "$temporary" "$STATUS_FILE" 2>/dev/null || true
        INSTALL_TRANSACTION=0
        echo "error: installation failed during '$INSTALL_PHASE'; previous installation restored" >&2
    fi
    exit "$code"
}
trap install_rollback ERR

# ── Core 二进制 ───────────────────────────────────────────────────────
INSTALL_PHASE=binary
binary_temporary=$(mktemp "$LIBEXEC_DIR/echo-agent-core-bin.new.XXXXXX")
install -m 755 "$BINARY_SOURCE" "$binary_temporary"
mv -f "$binary_temporary" "$LIBEXEC_DIR/echo-agent-core-bin"
echo "==> Installed Core binary: $LIBEXEC_DIR/echo-agent-core-bin"

# ── Panel 前端（仅二进制模式）──────────────────────────────────────────
INSTALL_PHASE=panel
if [[ "$INSTALL_MODE" == binary ]] && ((INSTALL_PANEL)); then
    # 先装到同级临时目录再整体切换：半个前端目录（新 index.html + 旧 assets）
    # 会让浏览器拿到 404 的 chunk，比"没更新"难查得多。
    web_staging="$LIBEXEC_DIR/web.new.$$"
    web_previous="$LIBEXEC_DIR/web.previous.$$"
    install -d -m 755 "$LIBEXEC_DIR" "$web_staging"
    cp -a "$UNPACK_DIR/web/." "$web_staging/"
    if [[ -d "$PANEL_WEB_DIR" ]]; then
        mv -- "$PANEL_WEB_DIR" "$web_previous"
    fi
    mv -- "$web_staging" "$PANEL_WEB_DIR"
    rm -rf -- "$web_previous"
    echo "==> Installed Panel frontend: $PANEL_WEB_DIR"
elif [[ "$INSTALL_MODE" == binary ]]; then
    echo "==> Skipped Panel frontend (--no-panel)"
fi

# ── 出厂技能（模块无关的框架资产）──────────────────────────────────────
# 技能分层（2026-10 议题 5）：出厂技能 = 框架资产，拷到
# $DATA_DIR/skills-builtin/（只读层；update.sh 同步刷新——自更新只滚出厂
# 技能）；用户技能目录 = $DATA_DIR/skills/（读写层：Save/DeleteSkill 与
# Git 安装技能落这里，自更新不触碰）。同名技能用户层覆盖出厂版本。
INSTALL_PHASE=skills
BUILTIN_SKILLS_DIR="$DATA_DIR/skills-builtin"
USER_SKILLS_DIR="$DATA_DIR/skills"
install -d -m 755 "$BUILTIN_SKILLS_DIR" "$USER_SKILLS_DIR"
cp -a "$SKILLS_SOURCE/." "$BUILTIN_SKILLS_DIR/"
# 反向清理（2026-11，与 update.sh 同一段逻辑）：`cp -a` 只会覆盖/新增，
# **不会删**——重装到一个已存在的部署上，旧版本留下的技能目录会一直留着并
# 继续被加载（实测：仓库已把 qq 等四个技能迁给模块，部署里它们仍存活）。
# 判据只看还含 SKILL.md 的目录：那是发现逻辑会加载的东西，其它残留不归这里管。
for _skill_dst in "$BUILTIN_SKILLS_DIR"/*/; do
    [[ -d "$_skill_dst" ]] || continue
    _skill_name=$(basename -- "$_skill_dst")
    [[ -f "$SKILLS_SOURCE/$_skill_name/SKILL.md" ]] && continue
    [[ -n "$(find "$_skill_dst" -name SKILL.md -print -quit 2>/dev/null)" ]] || continue
    rm -rf "$_skill_dst"
    echo "    pruned stale factory skill: $_skill_name"
done
unset _skill_dst _skill_name

# ── 模块二进制 ────────────────────────────────────────────────────────
# 落点约定：`modules/<包名>/<二进制>`——modules.toml 的 command 直接指这里，
# 两种安装模式同一套约定（用户手工往 modules/ 放二进制时不必区分装法）。
install_one_module() { # <包名> <二进制路径>
    local module_name=$1 binary=$2 module_dir module_temporary
    module_dir="$MODULES_DIR/$module_name"
    install -d -m 755 "$module_dir"
    module_temporary=$(mktemp "$module_dir/$module_name.new.XXXXXX")
    install -m 755 "$binary" "$module_temporary"
    mv -f "$module_temporary" "$module_dir/$module_name"
    echo "==> Installed module binary: $module_dir/$module_name"
}

INSTALL_PHASE=modules
if [[ "$INSTALL_MODE" == binary ]]; then
    # 产物里的外部模块：与源码安装同一落点，逐个原子替换（失败由事务守卫
    # 整体回滚）。
    if [[ -d "$UNPACK_DIR/modules" ]]; then
        for module_src in "$UNPACK_DIR"/modules/*/; do
            [[ -d "$module_src" ]] || continue
            module_name=$(basename -- "$module_src")
            binary="$module_src$module_name"
            # 存在性用 `-f` 而不是 `-x`：产物是先解到 `$WORK_DIR`（默认 /tmp）
            # 再装的，而硬化过的机器常把 /tmp 挂成 noexec——那里 `[[ -x ]]`
            # 对所有文件都为假（实测踩到），会把完好的产物判成"缺可执行位"。
            # 真正的执行位由 `install -m 755` 落盘时统一给定。
            [[ -f "$binary" ]] || {
                echo "error: module directory '$module_name' has no '$module_name' binary" >&2
                exit 1
            }
            install_one_module "$module_name" "$binary"
        done
        unset module_src module_name binary
    fi
else
    for _module in "${MODULE_BINARIES[@]}"; do
        install_one_module "$_module" "$SOURCE_DIR/target/release/$_module"
    done
    unset _module
fi

# ── Core 配置 ─────────────────────────────────────────────────────────
# 旧配置的 management_access_token（--overwrite-config 重写时沿用——
# 换令牌会让 Panel/对端全部失联，重写配置不应顺带换凭证）。
INSTALL_PHASE=config
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
    install -m 600 "$CONFIG_TEMPLATE" "$CORE_CONFIG"
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

# ── modules.toml（仅二进制模式）────────────────────────────────────────
# 两条不可省的声明：
#   1. `echo-agent.selfupdate` 关掉 —— 本安装没有源码检出，updater 无从构建，
#      留着它等于给界面一个点了必然报错的按钮。
#   2. `echo-agent.panel.web` 打开 —— 内嵌 Panel 默认不挂载，必须显式启用。
# 只在**文件不存在**时整份写；已存在时只追加缺失的条目（追加到末尾对 TOML 合法，
# 因为都是顶层 `[[module]]` 数组元素），且绝不覆盖用户已有的同 id 行。
write_modules_config() {
    local panel_enabled=$1
    local need_self_update=0 need_panel=0 fresh=0
    if [[ -f "$MODULES_CONFIG" ]]; then
        grep -q 'echo-agent\.selfupdate' "$MODULES_CONFIG" || need_self_update=1
        if ((panel_enabled)); then
            grep -q 'echo-agent\.panel\.web' "$MODULES_CONFIG" || need_panel=1
        fi
        if ((need_self_update == 0 && need_panel == 0)); then
            echo "==> Preserved existing modules config: $MODULES_CONFIG"
            return 0
        fi
    else
        fresh=1
    fi
    # 先备好临时文件：首装是空的，升级是原文件的**拷贝**（只追加，绝不重写
    # 用户已有的行）。早先这里漏了重定向，内容被打印到终端而**没有落盘**——
    # 装完 Core 起来发现自更新仍启用、内嵌 Panel 仍不挂载，两个问题都不报错。
    local modtmp="$MODULES_CONFIG.tmp.$$"
    if ((fresh)); then
        : >"$modtmp"
    else
        cp -p -- "$MODULES_CONFIG" "$modtmp"
    fi
    {
        if ((fresh)); then
            printf '%s\n' \
                '# modules.toml —— 由 install.sh --mode binary 在首装时生成。' \
                '' \
                '# 本次是**二进制安装**：没有源码检出，因此关掉自更新模块（没有可构建的' \
                '# 源码，updater 单元也不存在，留着它只会给界面一个必然报错的按钮）。' \
                '# 要更新就重新运行 install.sh --mode binary（它幂等）。' \
                '# 要恢复源码安装的自更新能力，改用 install.sh --mode source。' \
                '[[module]]' \
                'id = "echo-agent.selfupdate"' \
                'kind = "builtin"' \
                'enabled = false' \
                '' \
                '# 内嵌 Web Panel：模块代码编译在 Core 二进制里（id 见 panel 仓' \
                '# source/agent-module-panel/module.toml），这里只负责启用。' \
                '# 前端产物在 <prefix>/libexec/echo-agent-panel/web，由内嵌模块' \
                '# 自动发现，不需要写 [module.config] static_dir。' \
                '# 改监听地址/端口：' \
                '#   [module.config]' \
                '#   bind = "127.0.0.1:8081"' \
                ''
            # 任务清单：产物里带着二进制，但**默认不启用**（2026-11-21）。
            #
            # 它是第一个"Core 完全不认识"的模块：不在 Core 的依赖图里、没有
            # 内置目录项，只有这一行配置能让它存在。默认关着而不是默认开着，
            # 是因为"装了什么"与"开了什么"该是两件事——用户可以先看到产物，
            # 再决定要不要这份能力。
            #
            # 真要启用，去掉下面五行的注释即可（`enabled = true` 那行是关键）。
            printf '%s\n' \
                '# 任务清单（可选）：产物里的独立二进制，默认不启用。' \
                '# 去掉注释即启用；数据目录按 persona 各一份 JSON。' \
                '# [[module]]' \
                '# id = "echo-agent.checklist"' \
                '# kind = "stdio"' \
                '# enabled = true' \
                "# command = \"$MODULES_DIR/agent-module-checklist/agent-module-checklist\"" \
                '# ' \
                '# [module.config]' \
                "# data_dir = \"$DATA_DIR/checklists\"" \
                ''
            if ((panel_enabled)); then
                printf '%s\n' \
                    '[[module]]' \
                    'id = "echo-agent.panel.web"' \
                    'kind = "builtin"' \
                    'enabled = true' \
                    ''
            else
                printf '%s\n' \
                    '# 本次安装用了 --no-panel，故不启用内嵌 Panel。' \
                    '# [[module]]' \
                    '# id = "echo-agent.panel.web"' \
                    '# kind = "builtin"' \
                    '# enabled = true' \
                    ''
            fi
        else
            if ((need_self_update)); then
                if [[ -s "$modtmp" ]]; then printf '\n'; fi
                printf '%s\n' \
                    '# 二进制安装：没有源码检出，自更新不可用（见 install.sh --mode binary）。' \
                    '[[module]]' \
                    'id = "echo-agent.selfupdate"' \
                    'kind = "builtin"' \
                    'enabled = false' \
                    ''
            fi
            if ((need_panel)); then
                if [[ -s "$modtmp" ]]; then printf '\n'; fi
                printf '%s\n' \
                    '[[module]]' \
                    'id = "echo-agent.panel.web"' \
                    'kind = "builtin"' \
                    'enabled = true' \
                    ''
            fi
        fi
    } >>"$modtmp"
    mv -f -- "$modtmp" "$MODULES_CONFIG"
    chmod 644 -- "$MODULES_CONFIG"
    echo "==> Wrote modules config: $MODULES_CONFIG"
}

remove_stale_updater_unit() {
    # 清理可能残留的 updater 单元（此前用 --mode source 装过、这次改用二进制
    # 装法）：留着它等于留一个"会去跑 cargo build 但源码已经不在"的定时炸弹。
    if [[ -e "$SYSTEMD_DIR/echo-agent-core-update.service" ]]; then
        rm -f -- "$SYSTEMD_DIR/echo-agent-core-update.service"
        echo "==> Removed stale updater unit: $SYSTEMD_DIR/echo-agent-core-update.service"
    fi
}

install_binary_extras() {
    # 外部模块**不再自动启用**（2026-11-21）。
    #
    # 此前这里按"二进制真的装上了"决定要不要写启用行——理由是配置指向不存在的
    # 可执行文件会让 Core "起来但模块没起"。那条理由仍然成立，但它只回答"能不
    # 能写"，不回答"该不该开"。任务清单现在是宿主完全不认识的产物，默认关着才
    # 是它的正常状态：想用的人自己加一行（write_modules_config 里留了注释示例），
    # 不想用的人不该在升级时被悄悄打开一个新模式。
    write_modules_config "$INSTALL_PANEL"
    remove_stale_updater_unit
}

install_source_extras() {
    # 自更新器：源码模式唯一的"多装一个东西"——它在新版本上跑 `cargo build`，
    # 所以必须有源码检出。二进制模式装它会立刻变成点了必炸的按钮。
    local updater_temporary
    updater_temporary=$(mktemp "$LIBEXEC_DIR/update.sh.new.XXXXXX")
    install -m 755 "$PROJECT_ROOT/scripts/update.sh" "$updater_temporary"
    mv -f "$updater_temporary" "$LIBEXEC_DIR/update.sh"
}

if [[ "$INSTALL_MODE" == binary ]]; then
    INSTALL_PHASE=modules-config
    install_binary_extras
else
    INSTALL_PHASE=updater
    install_source_extras
fi

# ── launcher（两种模式同一份）──────────────────────────────────────────
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

# ── systemd 单元 ──────────────────────────────────────────────────────
# 两种模式**复用同一份模板**（packaging/systemd/echo-agent-core.service.in），
# 只有 `@SOURCE_DIR@`（唯一用途是 WorkingDirectory）按模式取值。复用而不是
# 复制一份 `-binary.service.in`：两份模板必然漂移，而漂移的后果是"二进制装法
# 的 unit 缺了某条加固/重启策略"这种只在事故时才暴露的差异。
if [[ "$INSTALL_MODE" == source ]]; then
    SERVICE_PATH=$(printf '%s\n' \
        "$(dirname -- "$(command -v cargo)")" \
        "$(dirname -- "$(command -v git)")" \
        "$(dirname -- "$(command -v install)")" \
        "$(dirname -- "$(command -v flock)")" \
        "$BIN_DIR" /usr/local/bin /usr/bin /bin | awk '!seen[$0]++' | paste -sd: -)
else
    # 二进制安装不需要 cargo/git（没有构建步骤），剩下的是运行期与工具模块
    # 可能调用到的基础命令目录。
    SERVICE_PATH=$(printf '%s\n' \
        "$(dirname -- "$(command -v install)")" \
        "$(dirname -- "$(command -v flock 2>/dev/null || command -v install)")" \
        "$BIN_DIR" /usr/local/bin /usr/bin /bin | awk '!seen[$0]++' | paste -sd: -)
fi

# 作用域决定的单元片段。
#
# - `@SCOPE_LINES@`：system 作用域要显式声明运行身份与家目录（`User=`/
#   `Group=`/`Environment=HOME=...`）。user 作用域里这些由用户管理器本身
#   提供，写死反而会绑到错误的人。
# - `@SCOPE_ENV@`：两条作用域都注入 `ECHO_SERVICE_SCOPE`——这是自更新模块
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
        -e "s|@SOURCE_DIR@|$UNIT_SOURCE_DIR|g" \
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
render_unit "$UNIT_TEMPLATE" "$SYSTEMD_DIR/echo-agent-core.service"
if [[ "$INSTALL_MODE" == source ]]; then
    render_unit "$PROJECT_ROOT/packaging/systemd/echo-agent-core-update.service.in" \
        "$SYSTEMD_DIR/echo-agent-core-update.service"
fi

# ── 状态文件 ──────────────────────────────────────────────────────────
INSTALL_PHASE=state
revision_temporary=$(mktemp "$STATE_DIR/installed-revision.new.XXXXXX")
printf '%s\n' "$REVISION" >"$revision_temporary"
mv -f "$revision_temporary" "$STATE_DIR/installed-revision"
status_temporary=$(mktemp "$STATUS_FILE.new.XXXXXX")
cat >"$status_temporary" <<EOF
state=installed
revision=$REVISION
message=installation completed (mode ${INSTALL_MODE}${manifest_version:+, version "$manifest_version"})
EOF
mv -f "$status_temporary" "$STATUS_FILE"

# ── 激活 ──────────────────────────────────────────────────────────────
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
# 只清文件，目录单独处理：这里除了快照文件还有 `web/` 与 `modules/` 快照目录，
# `rm -f <目录>` 会以 "Is a directory" 失败，而此刻 `set -e` 仍然生效——
# 结果是**安装成功后脚本以退出码 1 结束**（陷阱已解除、不回滚，只是谎报了失败）。
for rollback_file in "$ROLLBACK_DIR"/*; do
    if [[ -f "$rollback_file" ]]; then
        rm -f -- "$rollback_file"
    fi
done
rm -rf -- "$ROLLBACK_DIR/web" "$ROLLBACK_DIR/modules"
rmdir "$ROLLBACK_DIR" 2>/dev/null || true

# ── 收尾 ──────────────────────────────────────────────────────────────
if [[ "$SERVICE_SCOPE" == system ]]; then
    scope_hint="systemctl status echo-agent-core.service"
    log_hint="journalctl -u echo-agent-core.service -f"
    update_hint="systemctl start echo-agent-core-update.service"
else
    scope_hint="systemctl --user status echo-agent-core.service"
    log_hint="journalctl --user -u echo-agent-core.service -f"
    update_hint="systemctl --user start echo-agent-core-update.service"
fi

if [[ "$INSTALL_MODE" == binary ]]; then
    panel_line="  Panel:    disabled (--no-panel)"
    if ((INSTALL_PANEL)) && ((START_SERVICE)); then
        # 尽力而为的探测：bind 可能被用户改过，探不到不算安装失败。
        #
        # 判据是 `/api/panel-identity` 的 `embedded: true`，不是"8080 有人应答"——
        # 后者会把**别的**服务（另一套独立 Panel、任何占着 8080 的程序）报成
        # "我们的面板起来了"。实测踩过：本机跑着开发实例时，装到临时 prefix 的
        # 这次安装照样打印 "responding"。
        if curl -fsS -m 3 "http://127.0.0.1:8080/api/panel-identity" 2>/dev/null |
            grep -q '"embedded":true'; then
            panel_line="  Panel:    http://127.0.0.1:8080/  (embedded, responding)"
        else
            panel_line="  Panel:    http://127.0.0.1:8080/  (embedded; not responding yet — check logs)"
        fi
    elif ((INSTALL_PANEL)); then
        panel_line="  Panel:    http://127.0.0.1:8080/  (embedded; service not started)"
    fi

    cat <<EOF

Installation complete (mode: binary, no source checkout).
  Scope:    $SERVICE_SCOPE$( [[ "$SERVICE_SCOPE" == system ]] && echo " (runs as $SERVICE_USER)" )
  Version:  $VERSION_INFO
  Launcher: $BIN_DIR/echo-agent-core
  Config:   $CORE_CONFIG
  Modules:  $MODULES_CONFIG
$panel_line
  Service:  $scope_hint
  Logs:     $log_hint

Self-update is disabled for this installation (no source checkout, no updater
unit). To update, re-run this script with --mode binary; it is idempotent and
keeps your config. For a self-updating deployment, re-run it with --mode source.
EOF
else
    cat <<EOF

Installation complete (mode: source).
  Scope:    $SERVICE_SCOPE$( [[ "$SERVICE_SCOPE" == system ]] && echo " (runs as $SERVICE_USER)" )
  Revision: $VERSION_INFO
  Launcher: $BIN_DIR/echo-agent-core
  Config:   $CORE_CONFIG
  Service:  $scope_hint
  Logs:     $log_hint
  Update:   $update_hint (or the self-update button in the UI)

modules.toml was left untouched: the embedded Panel and external modules must be
enabled explicitly there. Install the Panel (TUI) frontend separately from the
EchoAgentPanel repository, or re-run with --mode binary for the embedded one.
EOF
fi

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
