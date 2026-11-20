#!/usr/bin/env bash
# EchoAgentCore —— **二进制安装器**（2026-11）
#
# 与 `scripts/install.sh` 的分工：
#
#   install.sh          源码安装：克隆受管检出 → `cargo build --release` → 装。
#                       需要 rust / cargo / git / cc，产物可自更新（updater 单元
#                       在源码检出上跑 `cargo build`）。
#   install-binary.sh   **本脚本**：只下载预构建产物并落盘，不需要任何编译链。
#                       没有源码检出，因此**不装 updater 单元**，并在
#                       `modules.toml` 里关掉 `echo-agent.selfupdate`。
#
# 两条路的**安装布局完全一致**（同一套 `derive_paths`、同一个 launcher、
# 同一个 systemd unit 模板、同一份 core.toml 模板），所以：
#   - `scripts/uninstall.sh` 对两者通用；
#   - 装过一种再装另一种 = 原地升级/降级，不会得到两套并存的部署。
# 布局一致是硬约束，由 `source/modules/core/tests/install_binary_script.rs`
# 守护（两条脚本的路径推导、占位符、服务名必须逐项相等）。
#
# ## Panel（内嵌形态）
#
# Panel 的模块代码**编译在 Core 二进制里**（`agent-module-panel`，模块 id
# `echo-agent.panel.web`），本脚本不装第二个二进制。缺的只有前端产物
# `web/dist`，装到 `<prefix>/libexec/echo-agent-panel/web`——这正是内嵌模块
# 自动发现候选里的第三个位置（见 panel 仓 `source/agent-module-panel/src/lib.rs`
# 的 `static_dir_candidates`），因此**零配置**即可用。
#
# 默认不挂载、不监听：内嵌表单靠 `modules.toml` 显式启用：
#
#   [[module]]
#   id = "echo-agent.panel.web"
#   kind = "builtin"
#   enabled = true
#
# 本脚本在首装时会写这一行（`--no-panel` 可跳过），因此装完浏览器直接开
# `http://127.0.0.1:8080` 就是面板。要改端口在 `[module.config] bind = ...`。
#
# ## 用法
#
#   # 一键（自动取 latest release，按 uname -m 选 x86_64 / aarch64）
#   curl -fsSL <raw-url>/scripts/install-binary.sh | bash
#
#   # 指定版本
#   curl -fsSL <raw-url>/scripts/install-binary.sh | bash -s -- --version v0.1.0
#
#   # 本地 tarball / 直链
#   ./scripts/install-binary.sh --artifact ./echo-agent-core-0.1.0-linux-x86_64.tar.gz
#   ./scripts/install-binary.sh --artifact https://example.com/x.tar.gz
set -Eeuo pipefail

DEFAULT_REPOSITORY="EchoLab-Auto/EchoAgentCore"
# Release 资产与脚本的公共前缀；`--mirror` 只替换这一段。
CANONICAL_PREFIX="https://github.com"

# ── 参数状态 ─────────────────────────────────────────────────────────
ARTIFACT=""
VERSION=""
REPOSITORY=${ECHO_REPOSITORY:-$DEFAULT_REPOSITORY}
DOWNLOAD_MIRROR=${ECHO_DOWNLOAD_MIRROR:-}
SERVICE_SCOPE=${ECHO_SERVICE_SCOPE:-user}
SERVICE_USER=${ECHO_SERVICE_USER:-}
SERVICE_HOME=""
ECHO_SKIP_LINGER=0
START_SERVICE=1
OVERWRITE_CONFIG=0
DRY_RUN=0
VERIFY_CHECKSUM=1
ALLOW_GLIBC_MISMATCH=0
INSTALL_PANEL=1

# ── 路径推导：**必须与 install.sh 的 derive_paths 逐行同形** ─────────
# 守护测试 install_binary_script.rs 会从两条脚本里抠出这一段做字符串比对；
# 改这里就要同步改 install.sh，否则卸载器/自更新会按两套路径工作。
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
    LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core"
    MODULES_DIR="$LIBEXEC_DIR/modules"
    BIN_DIR="$PREFIX/bin"
    CORE_CONFIG="$CONFIG_DIR/core.toml"
    MODULES_CONFIG="$CONFIG_DIR/modules.toml"
    STATUS_FILE="$STATE_DIR/update-status"
}

derive_paths "$HOME" 1
# Panel 前端目录 = **Core 二进制同级**的 `web/`。
#
# 为什么不是 `<prefix>/libexec/echo-agent-panel/web`（那里是独立 Panel 的布局）：
# 内嵌模块的候选顺序里，`exe.parent()/web` 是第一个必然可判定的位置，而且与
# 二进制同日安装、同版本淘汰——前端 API 契约随 Core 走，两者分开存只会得到
# "Core 升级了、前端还是旧的"这种最难查的组合。复用独立 Panel 的前端仍然可以，
# 但那要显式配置：`[module.config] static_dir = "<那一份的路径>"`。
#
# 顺带记一笔 panel 仓的现状（2026-11 实测）：它 README 承诺的
# `<prefix>/libexec/echo-agent-panel/web` 在代码里被拼成了
# `<prefix>/libexec/libexec/echo-agent-panel/web`（`static_dir_candidates` 取
# `exe.parent()` 之后再 join 一次 `libexec/`）。本安装器不依赖那条路径。
PANEL_WEB_DIR="$LIBEXEC_DIR/web"

SYSTEMD_DIR="$CONFIG_HOME/systemd/user"
SYSTEMCTL=(systemctl --user)
BUILTIN_SKILLS_DIR="$DATA_DIR/skills-builtin"
USER_SKILLS_DIR="$DATA_DIR/skills"

usage() {
    cat <<'EOF'
Usage: install-binary.sh [options]

Install EchoAgentCore from a prebuilt release artifact. Needs no rust, cargo,
git, or source checkout. The embedded Web Panel frontend is installed too.

Artifact source (pick at most one):
  --artifact <path|url>  Install from this tarball (local path or http(s) URL).
                         Without it, the latest GitHub release is downloaded.
  --version <tag>        Release tag to download (e.g. v0.1.0 or 0.1.0).
                         Default: the repository's latest release.
  --repository <o/r>     GitHub owner/repo used for release lookup
                         (default: EchoLab-Auto/EchoAgentCore).
  --mirror <prefix>      Rewrite https://github.com to this prefix when
                         downloading (e.g. https://ghfast.top/https://github.com).
                         Also readable from ECHO_DOWNLOAD_MIRROR.

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
  --no-panel             Do not enable the embedded Web Panel in modules.toml.
  --skip-checksum        Do not require the `.sha256` sidecar of the artifact
                         (not recommended: it is the only integrity check).
  --allow-glibc-mismatch Continue when the host glibc is older than the build
                         baseline in the artifact manifest.
  --prefix <dir>         Install prefix (default: ~/.local, or $ECHO_PREFIX).
  --dry-run              Print resolved paths and the artifact plan, then exit.
  -h, --help             Show this help.

Self-update:
  This installation has no source checkout, so the in-app updater is disabled
  (echo-agent.selfupdate is turned off in modules.toml and no updater unit is
  written). To update, re-run this script. For the source installation with a
  working updater, use scripts/install.sh instead.
EOF
}

while (($#)); do
    case "$1" in
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

if [[ -n "${ECHO_PREFIX:-}" ]]; then
    derive_paths "$HOME" 1
    PANEL_WEB_DIR="$LIBEXEC_DIR/web"
fi

# ── 作用域解析（与 install.sh 同一套规则与拒绝理由）────────────────
case "$SERVICE_SCOPE" in
    user|system) ;;
    *)
        echo "error: --scope must be 'user' or 'system' (got: '$SERVICE_SCOPE')" >&2
        exit 2
        ;;
esac

if [[ "$SERVICE_SCOPE" == system ]]; then
    if ((EUID != 0)) && ((!DRY_RUN)); then
        echo "error: --scope system requires root (it writes /etc/systemd/system)" >&2
        echo "       re-run as: sudo $0 --scope system" >&2
        exit 1
    fi
    # system 作用域固定 root，理由同 install.sh：本版本不实现非 root 系统服务
    # 布局（那需要为单位开提权路径，比省下的隔离更贵）。
    SERVICE_USER=${SERVICE_USER:-root}
    if [[ "$SERVICE_USER" != root ]]; then
        echo "error: --scope system currently runs the service as root (got --service-user $SERVICE_USER)" >&2
        echo "       Use the default user scope (per-user install) for non-root services." >&2
        exit 1
    fi
    SERVICE_HOME=$(getent passwd root 2>/dev/null | cut -d: -f6)
    SERVICE_HOME=${SERVICE_HOME:-/root}
    derive_paths "$SERVICE_HOME" 1
    PANEL_WEB_DIR="$LIBEXEC_DIR/web"
    SYSTEMD_DIR=/etc/systemd/system
    SYSTEMCTL=(systemctl)
    ECHO_SKIP_LINGER=1
else
    if ((EUID == 0)) && [[ -n "${SUDO_USER:-}" ]]; then
        echo "warning: running as root via sudo but installing the **user** scope:" >&2
        echo "         units go to $SYSTEMD_DIR (root's home), not to $SUDO_USER's." >&2
        echo "         To install a system-wide service instead:" >&2
        echo "           sudo $0 --scope system" >&2
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
    if [[ -e /etc/systemd/system/echo-agent-core.service ]]; then
        echo "error: system-scope units already exist (/etc/systemd/system/echo-agent-core.service)" >&2
        echo "       refusing to add a user-scope installation next to them (both would" >&2
        echo "       bind the same ports). Run: sudo scripts/uninstall.sh --scope system" >&2
        echo "       or re-install in place with: sudo $0 --scope system" >&2
        exit 1
    fi
fi

# ── 架构与版本推导 ───────────────────────────────────────────────────
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
    if [[ -n "$artifact_file" ]]; then
        artifact_desc=$artifact_file
    elif [[ -n "$artifact_url" ]]; then
        artifact_desc=$(mirror_url "$artifact_url")
    elif [[ -n "$release_tag" ]]; then
        artifact_desc=$(mirror_url "$CANONICAL_PREFIX/$REPOSITORY/releases/download/$release_tag/echo-agent-core-${release_tag#v}-linux-$artifact_arch.tar.gz")
    else
        artifact_desc="latest release of $REPOSITORY for linux-$artifact_arch (resolved at run time)"
    fi
    echo "EchoAgentCore binary install plan"
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

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: the binary installer currently supports Linux/systemd only" >&2
    exit 1
fi

# ── 前置工具 ─────────────────────────────────────────────────────────
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

# ── 解析 artifact ───────────────────────────────────────────────────
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
        echo "       Slow or blocked network? retry with --mirror <prefix>." >&2
        exit 1
    fi
fi

# ── 校验（tarball 旁的 .sha256 是唯一的完整性依据）────────────────────
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
        # 镜像链路抖动（Connection reset）让一个 108 字节的 .sha256 下载失败，
        # 而当时的报错是"no checksum sidecar found … expected next to the
        # artifact"——用户会去怀疑发布流程漏发文件，实际是网络。
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
            echo "       pass --mirror <prefix> / --skip-checksum." >&2
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

# ── 解包 + manifest 校验 ────────────────────────────────────────────
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
json_string() { # file key
    tr -d '\n' <"$1" |
        sed -n -E "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"([^\"]*)\".*/\1/p" |
        head -1
}
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
                echo "       Upgrade the distribution, or install from source with scripts/install.sh." >&2
                echo "       Pass --allow-glibc-mismatch to try anyway." >&2
                exit 1
            fi
        fi
    fi
fi

echo "==> Artifact: version=${manifest_version:-unknown} arch=${manifest_arch:-unknown} revision=${manifest_revision:-unknown}"

# ── 回滚事务 ─────────────────────────────────────────────────────────
# 与 install.sh 同款：先快照所有可能被本次安装影响的文件，任一步失败就整体
# 还原（包括重启服务与状态文件）。二进制安装少了模块二进制与 updater，
# 多了 panel web 目录。
core_was_active=0
if "${SYSTEMCTL[@]}" is-active --quiet echo-agent-core.service 2>/dev/null; then
    core_was_active=1
fi
core_was_enabled=0
if "${SYSTEMCTL[@]}" is-enabled --quiet echo-agent-core.service 2>/dev/null; then
    core_was_enabled=1
fi

ROLLBACK_DIR="$STATE_DIR/install-rollback"
install -d -m 700 "$ROLLBACK_DIR"
for rollback_file in binary launcher core_unit core_config modules_config revision status; do
    rm -f "$ROLLBACK_DIR/$rollback_file"
done
rm -rf "$ROLLBACK_DIR/web"

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

install -d -m 755 "$BIN_DIR" "$LIBEXEC_DIR" "$MODULES_DIR" "$CONFIG_DIR" "$DATA_DIR" "$STATE_DIR" "$SYSTEMD_DIR"

snapshot_file "$LIBEXEC_DIR/echo-agent-core-bin" "$ROLLBACK_DIR/binary"
snapshot_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
snapshot_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
snapshot_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
snapshot_file "$MODULES_CONFIG" "$ROLLBACK_DIR/modules_config"
snapshot_file "$STATE_DIR/installed-revision" "$ROLLBACK_DIR/revision"
snapshot_file "$STATUS_FILE" "$ROLLBACK_DIR/status"
if [[ -d "$PANEL_WEB_DIR" ]]; then
    cp -a -- "$PANEL_WEB_DIR" "$ROLLBACK_DIR/web"
fi
# 外部模块二进制：装的时候是"逐个原子替换"，回滚必须整体还原——
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
        restore_file "$BIN_DIR/echo-agent-core" "$ROLLBACK_DIR/launcher"
        restore_file "$SYSTEMD_DIR/echo-agent-core.service" "$ROLLBACK_DIR/core_unit"
        restore_file "$CORE_CONFIG" "$ROLLBACK_DIR/core_config"
        restore_file "$MODULES_CONFIG" "$ROLLBACK_DIR/modules_config"
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

# ── 落盘 ─────────────────────────────────────────────────────────────
INSTALL_PHASE=binary
binary_temporary=$(mktemp "$LIBEXEC_DIR/echo-agent-core-bin.new.XXXXXX")
install -m 755 "$UNPACK_DIR/bin/echo-agent-core-bin" "$binary_temporary"
mv -f "$binary_temporary" "$LIBEXEC_DIR/echo-agent-core-bin"
echo "==> Installed Core binary: $LIBEXEC_DIR/echo-agent-core-bin"

INSTALL_PHASE=panel
if ((INSTALL_PANEL)); then
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
else
    echo "==> Skipped Panel frontend (--no-panel)"
fi

INSTALL_PHASE=skills
install -d -m 755 "$BUILTIN_SKILLS_DIR" "$USER_SKILLS_DIR"
cp -a "$UNPACK_DIR/skills/." "$BUILTIN_SKILLS_DIR/"
# 反向清理（与 install.sh / update.sh 同一判据）：`cp -a` 不删旧目录，
# 重装到既有部署上会让已下线的出厂技能一直存活并被加载。
for _skill_dst in "$BUILTIN_SKILLS_DIR"/*/; do
    [[ -d "$_skill_dst" ]] || continue
    _skill_name=$(basename -- "$_skill_dst")
    [[ -f "$UNPACK_DIR/skills/$_skill_name/SKILL.md" ]] && continue
    [[ -n "$(find "$_skill_dst" -name SKILL.md -print -quit 2>/dev/null)" ]] || continue
    rm -rf "$_skill_dst"
    echo "    pruned stale factory skill: $_skill_name"
done
unset _skill_dst _skill_name

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
    install -m 600 "$UNPACK_DIR/config/echo-agent-core.toml" "$CORE_CONFIG"
    escaped_builtin=${BUILTIN_SKILLS_DIR//&/\\&}
    escaped_user=${USER_SKILLS_DIR//&/\\&}
    sed -i "s|^skills_dir = .*|skills_dir = \"$escaped_user\"|" "$CORE_CONFIG"
    if grep -q '^skills_dirs' "$CORE_CONFIG"; then
        sed -i "s|^skills_dirs = .*|skills_dirs = [\"$escaped_builtin\"]|" "$CORE_CONFIG"
    else
        sed -i "/^skills_dir = /a skills_dirs = [\"$escaped_builtin\"]" "$CORE_CONFIG"
    fi
    if [[ -n "$previous_token" ]]; then
        escaped_token=${previous_token//&/\\&}
        sed -i "s|^management_access_token = \"\"|management_access_token = \"$escaped_token\"|" "$CORE_CONFIG"
        echo "==> Kept existing management_access_token (see $CORE_CONFIG)"
    elif grep -q '^management_access_token = ""' "$CORE_CONFIG"; then
        generated_token=$(openssl rand -hex 32 2>/dev/null \
            || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
        escaped_token=${generated_token//&/\\&}
        sed -i "s|^management_access_token = \"\"|management_access_token = \"$escaped_token\"|" "$CORE_CONFIG"
        echo "==> Generated management_access_token (see $CORE_CONFIG)"
    fi
    echo "==> Installed Core config: $CORE_CONFIG"
else
    echo "==> Preserved existing Core config: $CORE_CONFIG (pass --overwrite-config to replace it)"
fi

# modules.toml：两条不可省的声明。
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
                '# modules.toml —— 由 install-binary.sh 在首装时生成。' \
                '' \
                '# 这是**二进制安装**：没有源码检出，因此关掉自更新模块（没有可构建的' \
                '# 源码，updater 单元也不存在，留着它只会给界面一个必然报错的按钮）。' \
                '# 要更新就重新运行 install-binary.sh（它幂等）。' \
                '# 要恢复源码安装的自更新能力，改用 scripts/install.sh。' \
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
                    '# 二进制安装：没有源码检出，自更新不可用（见 install-binary.sh）。' \
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
INSTALL_PHASE=modules
# 外部模块二进制：与源码安装同一落点 `modules/<包名>/<包名>`。
# 逐个原子替换（`install` 到临时名再 `mv`），失败由事务守卫整体回滚。
if [[ -d "$UNPACK_DIR/modules" ]]; then
    for module_src in "$UNPACK_DIR"/modules/*/; do
        [[ -d "$module_src" ]] || continue
        module_name=$(basename -- "$module_src")
        binary="$module_src$module_name"
        [[ -x "$binary" ]] || {
            echo "error: module directory '$module_name' has no executable '$module_name'" >&2
            exit 1
        }
        module_dir="$MODULES_DIR/$module_name"
        install -d -m 755 "$module_dir"
        module_temporary=$(mktemp "$module_dir/$module_name.new.XXXXXX")
        install -m 755 "$binary" "$module_temporary"
        mv -f "$module_temporary" "$module_dir/$module_name"
        echo "==> Installed module binary: $module_dir/$module_name"
    done
    unset module_src module_name binary module_dir module_temporary
fi

# 外部模块**不再自动启用**（2026-11-21）。
#
# 此前这里按"二进制真的装上了"决定要不要写启用行——理由是配置指向不存在的
# 可执行文件会让 Core "起来但模块没起"。那条理由仍然成立，但它只回答"能不能
# 写"，不回答"该不该开"。任务清单现在是宿主完全不认识的产物，默认关着才是
# 它的正常状态：想用的人自己加一行（`write_modules_config` 里留了注释示例），
# 不想用的人不该在升级时被悄悄打开一个新模式。
write_modules_config "$INSTALL_PANEL"

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

# SERVICE_PATH：二进制安装不需要 cargo/git（没有构建步骤），剩下的是运行期
# 与工具模块可能调用到的基础命令目录。
SERVICE_PATH=$(printf '%s\n' \
    "$(dirname -- "$(command -v install)")" \
    "$(dirname -- "$(command -v flock 2>/dev/null || command -v install)")" \
    "$BIN_DIR" /usr/local/bin /usr/bin /bin | awk '!seen[$0]++' | paste -sd: -)

# ── systemd 单元 ─────────────────────────────────────────────────────
# **复用 install.sh 的同一份模板**（`packaging/systemd/echo-agent-core.service.in`），
# 只把 `@SOURCE_DIR@` 渲染成 `$DATA_DIR`：模板里那个占位符的唯一用途是
# WorkingDirectory，二进制安装没有源码检出，数据目录是它合理的工作目录。
# 复用而不是复制一份 `-binary.service.in`：两份模板必然漂移，而漂移的后果是
# "二进制装法的 unit 缺了某条加固/重启策略"这种只在事故时才暴露的差异。
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

INSTALL_PHASE=units
unit_source="$UNPACK_DIR/systemd/echo-agent-core.service.in"
sed \
    -e "s|@BINARY@|$LIBEXEC_DIR/echo-agent-core-bin|g" \
    -e "s|@UPDATE_SCRIPT@|$LIBEXEC_DIR/update.sh|g" \
    -e "s|@SOURCE_DIR@|$DATA_DIR|g" \
    -e "s|@CORE_CONFIG@|$CORE_CONFIG|g" \
    -e "s|@STATUS_FILE@|$STATUS_FILE|g" \
    -e "s|@SERVICE_PATH@|$SERVICE_PATH|g" \
    -e "s|@SCOPE_ENV@|$UNIT_SCOPE_ENV|g" \
    -e "s|@WANTED_BY@|$UNIT_WANTED_BY|g" \
    "$unit_source" >"$SYSTEMD_DIR/echo-agent-core.service.new"
if [[ -z "$UNIT_SCOPE_LINES" ]]; then
    sed -i '/@SCOPE_LINES@/d' "$SYSTEMD_DIR/echo-agent-core.service.new"
else
    ECHO_UNIT_SCOPE_LINES="$UNIT_SCOPE_LINES" awk '
        $0 == "@SCOPE_LINES@" { print ENVIRON["ECHO_UNIT_SCOPE_LINES"]; next }
        { print }
    ' "$SYSTEMD_DIR/echo-agent-core.service.new" >"$SYSTEMD_DIR/echo-agent-core.service.new.tmp"
    mv -f "$SYSTEMD_DIR/echo-agent-core.service.new.tmp" "$SYSTEMD_DIR/echo-agent-core.service.new"
fi
chmod 644 "$SYSTEMD_DIR/echo-agent-core.service.new"
mv -f "$SYSTEMD_DIR/echo-agent-core.service.new" "$SYSTEMD_DIR/echo-agent-core.service"

# 清理可能残留的 updater 单元（此前用 install.sh 装过、这次改用二进制装法）：
# 留着它等于留一个"会去跑 cargo build 但源码已经不在"的定时炸弹。
if [[ -e "$SYSTEMD_DIR/echo-agent-core-update.service" ]]; then
    rm -f -- "$SYSTEMD_DIR/echo-agent-core-update.service"
    echo "==> Removed stale updater unit: $SYSTEMD_DIR/echo-agent-core-update.service"
fi

INSTALL_PHASE=state
revision=${manifest_revision:-unknown}
revision_temporary=$(mktemp "$STATE_DIR/installed-revision.new.XXXXXX")
printf '%s\n' "$revision" >"$revision_temporary"
mv -f "$revision_temporary" "$STATE_DIR/installed-revision"
status_temporary=$(mktemp "$STATUS_FILE.new.XXXXXX")
cat >"$status_temporary" <<EOF
state=installed
revision=$revision
message=binary installation completed (version ${manifest_version:-unknown})
EOF
mv -f "$status_temporary" "$STATUS_FILE"

# ── 激活 ─────────────────────────────────────────────────────────────
INSTALL_PHASE=activating
"${SYSTEMCTL[@]}" daemon-reload
"${SYSTEMCTL[@]}" enable echo-agent-core.service >/dev/null

ensure_service_linger() {
    if [[ "${ECHO_SKIP_LINGER:-0}" == 1 ]]; then
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
# 只清文件，目录单独处理：这里除了快照文件还有一个 `web/` 快照目录，
# `rm -f <目录>` 会以 "Is a directory" 失败，而此刻 `set -e` 仍然生效——
# 结果是**安装成功后脚本以退出码 1 结束**（陷阱已解除、不回滚，只是谎报了失败）。
for rollback_file in "$ROLLBACK_DIR"/*; do
    if [[ -f "$rollback_file" ]]; then
        rm -f -- "$rollback_file"
    fi
done
rm -rf -- "$ROLLBACK_DIR/web" "$ROLLBACK_DIR/modules"
rmdir "$ROLLBACK_DIR" 2>/dev/null || true

if [[ "$SERVICE_SCOPE" == system ]]; then
    scope_hint="systemctl status echo-agent-core.service"
    log_hint="journalctl -u echo-agent-core.service -f"
else
    scope_hint="systemctl --user status echo-agent-core.service"
    log_hint="journalctl --user -u echo-agent-core.service -f"
fi

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

Installation complete (binary, no source checkout).
  Scope:    $SERVICE_SCOPE$( [[ "$SERVICE_SCOPE" == system ]] && echo " (runs as $SERVICE_USER)" )
  Version:  ${manifest_version:-unknown}${manifest_revision:+ ($manifest_revision)}
  Launcher: $BIN_DIR/echo-agent-core
  Config:   $CORE_CONFIG
  Modules:  $MODULES_CONFIG
$panel_line
  Service:  $scope_hint
  Logs:     $log_hint

Self-update is disabled for this installation (no source checkout, no updater
unit). To update, re-run this script; it is idempotent and keeps your config.
EOF

case ":${PATH:-}:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo
        echo "note: $BIN_DIR is not in PATH for this shell session —"
        echo "      'echo-agent-core' will not resolve until you do one of:"
        echo "        export PATH=\"$BIN_DIR:\$PATH\"     # current session, immediately"
        echo "        exec bash -l                      # or open a new terminal / re-login"
        ;;
esac
