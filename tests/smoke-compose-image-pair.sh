#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# tests/smoke-compose-image-pair.sh — npm Compose 模板的「真实 x86_64 OCI
# digest 配对」验收。
#
# ## 这个脚本要证明什么
#
# npm 模板（`npm/echo-agent/templates/docker-compose.yml` + `.env`）默认引用
# 公共发布镜像 `ghcr.io/...:latest`。`latest` 是**可变标签**，不能作为一对
# 「已经验证过能互通」的 Core/Panel 的身份。本脚本要证明的是：
#
#   存在一对**内容寻址**的 x86_64 镜像（`repo@sha256:...`），
#   通过 npm 模板的 `.env` 覆盖注入后，
#   docker compose 按 digest 拉取/解析，
#   真实起一对隔离的 Core + Panel，
#   并且 Panel→Core 的**应用层管理 WebSocket**（不是静态文件、不是健康检查）
#   能完成一次完整往返，浏览器重连后仍能，Core 重启后仍能。
#
# ## 四段计划（脚本本身就是这四段）
#
#   ① 构建候选镜像（可跳过：--core-image/--panel-image）
#      Core：官方 Dockerfile 的正式入口——`MODULE_SOURCE=official` +
#      `EXTERNAL_MODULES_SUMMARY="$(scripts/release/external-modules.sh
#      oci-summary image)"`；模块按锁定表里的 40 位 SHA 拉取并 `--locked`
#      构建，镜像标签里写锁定摘要。
#      Panel：独立 Dockerfile（前端 + Rust 后端，单镜像）。
#      两侧都从**快照**构建（`--core-context`/`--panel-context` 可复用），
#      并用 `docker build --label` 把源码提交 / 上下文摘要写进镜像——不改
#      Dockerfile 也能让镜像回到源码。
#
#   ② 临时回环 registry + digest 取证
#      在 127.0.0.1 的**随机端口**起一个临时 `registry:2`（容器名带本次
#      run id；退出即删）。两张镜像 push 进去，取 registry 返回的
#      `Docker-Content-Digest`，并**独立复算** manifest 的 sha256 与
#      `docker manifest inspect` 的平台（必须含 linux/amd64）。
#      然后删掉本地 digest 引用，确保后面 compose 的 pull 是**真的按 digest
#      去 registry 取**，而不是吃本地缓存。
#
#   ③ npm 模板 + 隔离编排
#      `node npm/echo-agent/bin/echo-agent.js init` 生成部署目录（模板原样：
#      生成文件与模板逐字节相同），随后只做三类覆盖：
#        - `.env`：ECHO_CORE_IMAGE / ECHO_PANEL_IMAGE → 两个 digest 引用；
#        - `config/*.toml`：随机 management token、关闭 QQ 适配器
#          （本测试不跑 NapCat）、Panel 指回 compose 服务名 `core`；
#        - `docker-compose.override.yml`：专用项目名/容器名/网络名、
#          panel 只绑 127.0.0.1 随机端口、core 不发布端口、**去掉
#          docker.sock**、NapCat 服务挂到永不启用的 profile、restart=no。
#      用 `docker compose config --format json` 静态断言：镜像就是那对
#      digest、没有 :latest、没有 docker.sock、没有生产名字/端口。
#
#   ④ 运行态 + 应用层 WS/重连
#      `docker compose pull`（按 digest）→ `up -d --no-build core panel`。
#      断言两个运行容器的 `.Image` 等于记录的 digest/镜像 Id。
#      等 Panel HTTP 就绪后，跑仓库里**既有**的
#      `tests/smoke-panel-application.mjs <panel 宿主端口>`：
#        - 初始连接：RequestTeamsList → TeamsList；InvokeModuleOp 到不存在的
#          模块 → ModuleInvokeResult(ok=false)；
#        - 浏览器重连：同样两帧再来一遍。
#      再把 Core 容器重启，重跑同一个脚本，证明 Panel→Core 的上游 WS 在
#      Core 重启后能重新接上（仍然只测正常应用行为）。
#
# ## 验收 = 退出码 0 且汇总里没有 FAIL
#
#   PASS  断言成立
#   WARN  不影响结论的环境差异（例如 docker manifest inspect 不可用）
#   SKIP  带原因的不适用项（本脚本尽量不 SKIP；做不到就是 FAIL）
#   FAIL  契约被破坏 → 退出码 1
#
# ## 强制安全边界（脚本自己执行，不依赖调用者记得）
#
#   * 只写 mktemp 目录；配置/数据都在临时目录，不碰任何现役部署目录；
#   * compose 项目名、容器名、网络名都带本次 run id；**不含** echo-agent /
#     echo-agent-core / echo-agent-panel / napcat 这些生产名字；
#   * 不发布 core 端口；panel 只绑 127.0.0.1 的随机端口；不碰 8080/6099；
#   * **不挂 /var/run/docker.sock**（模板默认挂，override 里显式去掉）；
#   * **不启动 NapCat**（服务被 profile 排除），不连 LLM、不连任何外部服务；
#   * 不 push 公共 registry，不碰生产镜像 tag；临时 registry 只在 127.0.0.1；
#   * 结束前后比对宿主机运行中容器 ID 集合，任何生产容器被影响即 FAIL；
#   * 只新增 `tests/smoke-compose-image-pair.*`，不改任何既有文件、不提交。
#
# ## 前置条件
#
#   * linux/amd64 宿主（脚本会核对镜像 Os/Arch，不对直接 FAIL）；
#   * docker（含 compose v2.24.4+，`!override`/`!reset` 需要）+ curl + tar；
#   * Node.js ≥ 22（既有 smoke 脚本用全局 WebSocket；npm 模板 init 也用它）。
#     不在 PATH 时可用 `--node` 或 `$NODE_BIN` 指定；脚本也会找 ~/.nvm。
#   * 构建段需要能访问 crates.io / github（Core 的 git 依赖）/ npm registry。
#     受限网络可用 `--core-context` 复用已构建过的快照。
#
# ## 用法
#
#   bash tests/smoke-compose-image-pair.sh                 # 全流程（构建+验收）
#   bash tests/smoke-compose-image-pair.sh --keep          # 保留现场排障
#   bash tests/smoke-compose-image-pair.sh \
#     --core-image echo-agent-wave2/core:<tag> \
#     --panel-image echo-agent-wave2/panel:<tag>           # 复用候选镜像
#
#   可选：--core/--panel 源码目录、--core-context/--panel-context 构建上下文、
#         --registry host:port（用现成 registry，不自己起）、
#         --registry-image、--node、--timeout、--app-timeout、--keep、
#         --clean-images、--report、-h。
#
# ## 事实与假设（写作前核对过）
#
#   * 模板的镜像变量确实来自 `.env`：compose 文件里是
#     `${ECHO_CORE_IMAGE:-ghcr.io/...:latest}`；`init` 生成的 `.env` 就是
#     模板 `templates/env` 的逐字副本。
#   * Core 官方镜像的模块来源只认 `packaging/external-modules.lock`；
#     Dockerfile 会拿 `EXTERNAL_MODULES_SUMMARY` 与锁定表逐字比对，不一致
#     直接构建失败。所以构建成功本身就是「official 模块」的证据，脚本再读
#     一次 OCI 标签做二次确认。
#   * Panel 容器入口固定 `--static-dir /app/web`，配置走 `/config/panel.toml`；
#     模板里 `[core] connect_url = "ws://core:3132"` 正是 compose 服务名。
#   * 本仓库既有的 `tests/smoke-panel-application.mjs` 只依赖一个宿主端口，
#     不假设任何生产实例——原样复用，不改它。
#   * 镜像↔源码的对应关系**不是** Dockerfile 自带的（两个 Dockerfile 都没有
#     ARG REV / LABEL VCS_REF）。脚本用 `docker build --label` 补这一层，
#     不改 Dockerfile；这是新增的、只在候选镜像上出现的标签。
#
# ## 已知限制
#
#   * 这是**应用层**互通验收：只覆盖管理面 WS 的 TeamsList / 未知模块调用，
#     不覆盖 QQ 适配器、NapCat、LLM、联邦、媒体、前端交互。
#   * Docker Hub 在本机不可直连时，基础镜像需事先用镜像站拉好并打上
#     Dockerfile 里的名字（`rust:1.98-bookworm` / `node:22-bookworm-slim` /
#     `debian:bookworm-slim` / `registry:2`）。脚本不负责改基础镜像来源。
#   * digest 指向的 manifest list 里可能带 BuildKit 的 attestation 清单
#     （unknown/unknown）；断言只要求「含 linux/amd64 且不含 arm」。
#
# ## 回退
#
#   删掉本文件与 `tests/smoke-compose-image-pair.verify.mjs` 即可；脚本不在
#   源码树里留任何产物（构建上下文是 /tmp 里的快照，默认退出即删）。
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DEFAULT_CORE=$(cd -- "$SCRIPT_DIR/.." && pwd)

CORE=${CORE_SOURCE:-$DEFAULT_CORE}
PANEL=${PANEL_SOURCE:-$HOME/.local/share/echo-agent-panel/source}
CORE_CONTEXT_ARG=${SMOKE_CORE_CONTEXT:-}
PANEL_CONTEXT_ARG=${SMOKE_PANEL_CONTEXT:-}
CORE_IMAGE_ARG=${SMOKE_CORE_IMAGE:-}
PANEL_IMAGE_ARG=${SMOKE_PANEL_IMAGE:-}
REGISTRY_ARG=${SMOKE_REGISTRY:-}
REGISTRY_IMAGE=${SMOKE_REGISTRY_IMAGE:-registry:2}
NODE_BIN=${NODE_BIN:-}
BUILD_TIMEOUT=${SMOKE_BUILD_TIMEOUT:-3600}
APP_TIMEOUT=${SMOKE_APP_TIMEOUT:-120}
KEEP=${SMOKE_KEEP:-0}
CLEAN_IMAGES=0
REPORT_FILE=${SMOKE_REPORT:-}
NO_BUILD=0

RUN_ID=$(date +%Y%m%d-%H%M%S)-$RANDOM
PROJECT_NAME="wave2pair-$RUN_ID"
UNIQ="$PROJECT_NAME"

PASSED=0
FAILED=0
WARNED=0
SKIPPED=0
declare -a SUMMARY=()

SCRATCH=""
PROJECT=""
COMPOSE=()
COMPOSE_UP=0
REGISTRY_CONTAINER=""
REGISTRY_OWNED=0
REG_HOST=""
REG_PORT=""
RESOLVED_DIGEST=""
PANEL_PORT=""
PROD_BEFORE=""
CORE_LOCAL=""
PANEL_LOCAL=""
CORE_REF=""
PANEL_REF=""
CORE_DIGEST=""
PANEL_DIGEST=""
CORE_ID=""
PANEL_ID=""
CORE_REV=""
PANEL_REV=""
CORE_DIRTY=""
PANEL_DIRTY=""
CORE_CONTEXT_SHA=""
PANEL_CONTEXT_SHA=""
MODULES_SUMMARY=""
APP_SMOKE_RESULT=""

usage() {
    cat <<'EOF'
用法: bash tests/smoke-compose-image-pair.sh [选项]

  --core <dir>              Core 源码目录（缺省 = 本脚本上一级，或 $CORE_SOURCE）
  --panel <dir>             Panel 源码目录（缺省 $PANEL_SOURCE 或 ~/.local/share/echo-agent-panel/source）
  --core-context <dir>      复用已有 Core 构建上下文（跳过快照）
  --panel-context <dir>     复用已有 Panel 构建上下文（跳过快照）
  --core-image <ref>        复用已构建的 Core 候选镜像（不给则构建）
  --panel-image <ref>       复用已构建的 Panel 候选镜像（不给则构建）
  --no-build                等同两个 --*-image 都必须给
  --registry <host:port>    用现成 registry（缺省在 127.0.0.1 随机端口起临时 registry:2）
  --registry-image <ref>    临时 registry 用的镜像（缺省 registry:2）
  --node <path>             Node.js ≥22 可执行文件
  --timeout <secs>          构建超时（缺省 3600）
  --app-timeout <secs>      应用层 WS 验收超时（缺省 120）
  --report <path>           汇总报告落盘路径
  --keep                    保留临时目录（排障用）
  --clean-images            结束删除本次构建的候选镜像
  -h, --help

安全边界（脚本强制）：只写临时目录；专用项目/容器/网络名；panel 仅绑
127.0.0.1 随机端口；不挂 docker.sock；不启动 NapCat；不连 LLM/外部服务；
不 push 公共 registry；结束核对生产容器 ID 集合未变。
EOF
}

while (($#)); do
    case $1 in
        --core) CORE=${2:-}; shift 2 ;;
        --core=*) CORE=${1#*=}; shift ;;
        --panel) PANEL=${2:-}; shift 2 ;;
        --panel=*) PANEL=${1#*=}; shift ;;
        --core-context) CORE_CONTEXT_ARG=${2:-}; shift 2 ;;
        --core-context=*) CORE_CONTEXT_ARG=${1#*=}; shift ;;
        --panel-context) PANEL_CONTEXT_ARG=${2:-}; shift 2 ;;
        --panel-context=*) PANEL_CONTEXT_ARG=${1#*=}; shift ;;
        --core-image) CORE_IMAGE_ARG=${2:-}; shift 2 ;;
        --core-image=*) CORE_IMAGE_ARG=${1#*=}; shift ;;
        --panel-image) PANEL_IMAGE_ARG=${2:-}; shift 2 ;;
        --panel-image=*) PANEL_IMAGE_ARG=${1#*=}; shift ;;
        --no-build) NO_BUILD=1; shift ;;
        --registry) REGISTRY_ARG=${2:-}; shift 2 ;;
        --registry=*) REGISTRY_ARG=${1#*=}; shift ;;
        --registry-image) REGISTRY_IMAGE=${2:-}; shift 2 ;;
        --registry-image=*) REGISTRY_IMAGE=${1#*=}; shift ;;
        --node) NODE_BIN=${2:-}; shift 2 ;;
        --node=*) NODE_BIN=${1#*=}; shift ;;
        --timeout) BUILD_TIMEOUT=${2:-}; shift 2 ;;
        --timeout=*) BUILD_TIMEOUT=${1#*=}; shift ;;
        --app-timeout) APP_TIMEOUT=${2:-}; shift 2 ;;
        --app-timeout=*) APP_TIMEOUT=${1#*=}; shift ;;
        --report) REPORT_FILE=${2:-}; shift 2 ;;
        --report=*) REPORT_FILE=${1#*=}; shift ;;
        --keep) KEEP=1; shift ;;
        --clean-images) CLEAN_IMAGES=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "未知选项：$1（-h 查看帮助）" >&2; exit 2 ;;
    esac
done

if ((NO_BUILD == 1)) && [[ -z $CORE_IMAGE_ARG || -z $PANEL_IMAGE_ARG ]]; then
    echo "error: --no-build 需要同时给 --core-image 与 --panel-image" >&2
    exit 2
fi
if [[ -n $CORE_IMAGE_ARG && -n $CORE_CONTEXT_ARG ]] || [[ -n $PANEL_IMAGE_ARG && -n $PANEL_CONTEXT_ARG ]]; then
    echo "error: 给了 --*-image 就不该再给对应的 --*-context（不会构建）" >&2
    exit 2
fi

# ── 输出与记录 ─────────────────────────────────────────────────────────────
say() { printf '%s\n' "$*"; }
hdr() { printf '\n──── %s ────\n' "$*"; }

record() {
    local status=$1 phase=$2 message=$3
    SUMMARY+=("$status|$phase|$message")
    case $status in
        PASS) PASSED=$((PASSED + 1)) ;;
        FAIL) FAILED=$((FAILED + 1)) ;;
        WARN) WARNED=$((WARNED + 1)) ;;
        SKIP) SKIPPED=$((SKIPPED + 1)) ;;
    esac
    printf '[%s] %-10s %s\n' "$status" "$phase" "$message"
}

die() {
    record FAIL "$1" "$2"
    exit 1
}

# ── 收尾：down / 删 registry / 生产容器核对 / 汇总 / 报告 ─────────────────
cleanup() {
    local code=$?
    trap - EXIT INT TERM

    if ((COMPOSE_UP == 1)) && ((${#COMPOSE[@]})); then
        # 先留证据再拆：应用层验收失败时，这两份日志是唯一能解释「谁先断」的东西。
        "${COMPOSE[@]}" ps >"${SCRATCH:-/tmp}/logs/compose-ps.log" 2>&1 || true
        "${COMPOSE[@]}" logs --no-color --timestamps \
            >"${SCRATCH:-/tmp}/logs/compose.log" 2>&1 || true
        timeout 180 "${COMPOSE[@]}" down -v --remove-orphans \
            >"${SCRATCH:-/tmp}/logs/compose-down.log" 2>&1 || true
    fi
    if [[ -n $REGISTRY_CONTAINER ]]; then
        docker rm -f "$REGISTRY_CONTAINER" >/dev/null 2>&1 || true
    fi

    # 生产容器必须原样还在（本测试只允许动自己项目名下的容器）。
    # 并行任务可能新建无关容器，所以判定是「开始时的每个 ID 都还在」，不是
    # 集合完全相等；多出来的只 WARN。
    if [[ -n $PROD_BEFORE ]]; then
        local prod_after missing=0 id
        prod_after=$(docker ps -q 2>/dev/null | sort | tr '\n' ' ')
        for id in $PROD_BEFORE; do
            if ! grep -qx "$id" <<<"$(docker ps -q 2>/dev/null)"; then
                missing=1
            fi
        done
        if ((missing == 0)); then
            record PASS "safety" "开始时的 $(wc -w <<<"$PROD_BEFORE") 个生产容器全部仍在运行（未触碰）"
        else
            record FAIL "safety" "有生产容器消失：before=[$PROD_BEFORE] after=[$prod_after]"
        fi
        if [[ "$prod_after" != "$PROD_BEFORE" ]]; then
            record WARN "safety" "运行中容器集合有变化（可能是并行任务）：before=[$PROD_BEFORE] after=[$prod_after]"
        fi
    fi
    if [[ -n $PROJECT_NAME ]]; then
        local leftover
        leftover=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT_NAME" 2>/dev/null | tr '\n' ' ')
        if [[ -n $leftover ]]; then
            record FAIL "safety" "本次项目仍有容器残留：$leftover"
        else
            record PASS "safety" "本次项目的容器/网络已清理干净"
        fi
    fi

    # 临时 registry 已经删了，本地那些指向它的引用没有保留价值。
    if [[ -n $REG_HOST ]]; then
        [[ -n $CORE_LOCAL && -n $CORE_DIGEST ]] && \
            docker image rm -f "$REG_HOST/echo-agent-wave2/core@$CORE_DIGEST" >/dev/null 2>&1 || true
        [[ -n $PANEL_LOCAL && -n $PANEL_DIGEST ]] && \
            docker image rm -f "$REG_HOST/echo-agent-wave2/panel@$PANEL_DIGEST" >/dev/null 2>&1 || true
    fi

    if ((CLEAN_IMAGES == 1)); then
        [[ -n $CORE_LOCAL ]] && docker image rm -f "$CORE_LOCAL" >/dev/null 2>&1 || true
        [[ -n $PANEL_LOCAL ]] && docker image rm -f "$PANEL_LOCAL" >/dev/null 2>&1 || true
    fi

    if ((FAILED > 0)); then
        code=1
    fi

    print_summary
    write_report

    if ((KEEP == 1)); then
        say ""
        say "现场保留：$SCRATCH"
        say "  构建日志：$SCRATCH/logs/*.log"
        say "  compose 项目：$PROJECT（已 down）"
    elif [[ -n $SCRATCH && -d $SCRATCH ]]; then
        rm -rf "$SCRATCH"
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

print_summary() {
    hdr "汇总"
    say "PASS=$PASSED  WARN=$WARNED  SKIP=$SKIPPED  FAIL=$FAILED"
    local line
    for line in "${SUMMARY[@]}"; do
        say "  $line"
    done
    if [[ -n $CORE_DIGEST && -n $PANEL_DIGEST ]]; then
        say ""
        say "digest pairing:"
        say "  core  = $CORE_DIGEST"
        say "  panel = $PANEL_DIGEST"
        say "  refs  = $CORE_REF / $PANEL_REF"
    fi
    if [[ -n $APP_SMOKE_RESULT ]]; then
        say "app smoke = $APP_SMOKE_RESULT"
    fi
    if [[ -n $REPORT_FILE ]]; then
        say "报告：$REPORT_FILE"
    fi
}

write_report() {
    [[ -n $REPORT_FILE ]] || return 0
    {
        echo "# smoke-compose-image-pair report"
        echo "run_id=$RUN_ID"
        echo "timestamp=$(date -Is)"
        echo "core_source=$CORE"
        echo "panel_source=$PANEL"
        echo "core_rev=$CORE_REV"
        echo "panel_rev=$PANEL_REV"
        echo "core_dirty=$CORE_DIRTY"
        echo "panel_dirty=$PANEL_DIRTY"
        echo "core_context_sha256=$CORE_CONTEXT_SHA"
        echo "panel_context_sha256=$PANEL_CONTEXT_SHA"
        echo "modules_summary=$MODULES_SUMMARY"
        echo "core_image_local=$CORE_LOCAL"
        echo "panel_image_local=$PANEL_LOCAL"
        echo "core_image_id=$CORE_ID"
        echo "panel_image_id=$PANEL_ID"
        echo "core_ref=$CORE_REF"
        echo "panel_ref=$PANEL_REF"
        echo "core_digest=$CORE_DIGEST"
        echo "panel_digest=$PANEL_DIGEST"
        echo "project=$PROJECT_NAME"
        echo "app_smoke=$APP_SMOKE_RESULT"
        echo "passed=$PASSED"
        echo "warned=$WARNED"
        echo "skipped=$SKIPPED"
        echo "failed=$FAILED"
        echo "# results"
        local line
        for line in "${SUMMARY[@]}"; do
            echo "$line"
        done
    } >"$REPORT_FILE" 2>/dev/null || true
}

# ── 基础工具 ───────────────────────────────────────────────────────────────
need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die preflight "缺少命令：$1"
}

detect_node() {
    if [[ -n $NODE_BIN ]]; then
        printf '%s' "$NODE_BIN"
        return 0
    fi
    if command -v node >/dev/null 2>&1; then
        command -v node
        return 0
    fi
    local candidate
    candidate=$(printf '%s\n' "$HOME"/.nvm/versions/node/*/bin/node 2>/dev/null | sort -V | tail -1)
    if [[ -n $candidate && -x $candidate ]]; then
        printf '%s' "$candidate"
        return 0
    fi
    return 1
}

choose_port() {
    "$NODE" -e 'const net=require("node:net");const s=net.createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close();});'
}

snapshot_tree() {
    local src=$1 dest=$2
    mkdir -p "$dest" || return 1
    tar -C "$src" -cf - \
        --exclude=.git --exclude=target --exclude=.ruff_cache \
        --exclude=web/node_modules --exclude=web/dist \
        . 2>/dev/null | tar -C "$dest" -xf - || return 1
}

tree_sha() {
    local dir=$1
    ( cd "$dir" && find . -type f -exec sha256sum {} + 2>/dev/null \
        | sort | sha256sum | cut -d' ' -f1 )
}

git_rev() {
    git -C "$1" rev-parse HEAD 2>/dev/null || echo "unknown"
}

git_dirty() {
    if [[ -n $(git -C "$1" status --porcelain 2>/dev/null) ]]; then
        echo 1
    else
        echo 0
    fi
}

# ── ① 构建候选镜像 ────────────────────────────────────────────────────────
build_core() {
    local context=$1
    local log="$SCRATCH/logs/core.build.log"
    say "构建 Core 候选镜像（official 模块）… 日志：$log"
    say "  context=$context"
    say "  modules=$MODULES_SUMMARY"
    local start=$SECONDS
    if ! timeout "$BUILD_TIMEOUT" docker build --progress=plain \
        --build-arg "MODULE_SOURCE=official" \
        --build-arg "EXTERNAL_MODULES_SUMMARY=$MODULES_SUMMARY" \
        --label "io.echo.smoke.core.rev=$CORE_REV" \
        --label "io.echo.smoke.core.context=$CORE_CONTEXT_SHA" \
        --label "io.echo.smoke.dirty=$CORE_DIRTY" \
        -f "$context/Dockerfile" \
        -t "$CORE_LOCAL" \
        "$context" >"$log" 2>&1; then
        tail -40 "$log" >&2
        die build "Core 镜像构建失败（日志：$log）"
    fi
    record PASS "build-core" "official 模块正式入口构建成功（$((SECONDS - start))s，$CORE_LOCAL）"
}

build_panel() {
    local context=$1
    local log="$SCRATCH/logs/panel.build.log"
    say "构建 Panel 候选镜像… 日志：$log"
    say "  context=$context"
    local start=$SECONDS
    if ! timeout "$BUILD_TIMEOUT" docker build --progress=plain \
        --label "io.echo.smoke.panel.rev=$PANEL_REV" \
        --label "io.echo.smoke.panel.context=$PANEL_CONTEXT_SHA" \
        --label "io.echo.smoke.dirty=$PANEL_DIRTY" \
        -f "$context/Dockerfile" \
        -t "$PANEL_LOCAL" \
        "$context" >"$log" 2>&1; then
        tail -40 "$log" >&2
        die build "Panel 镜像构建失败（日志：$log）"
    fi
    record PASS "build-panel" "独立 Dockerfile 构建成功（$((SECONDS - start))s，$PANEL_LOCAL）"
}

# ── ② 临时回环 registry + digest 取证 ─────────────────────────────────────
start_registry() {
    if [[ -n $REGISTRY_ARG ]]; then
        REG_HOST=$REGISTRY_ARG
        REGISTRY_OWNED=0
        REG_PORT=${REG_HOST##*:}
        if ! curl -fsS --max-time 5 "http://$REG_HOST/v2/" >/dev/null 2>&1; then
            die registry "现成 registry 不可用：$REG_HOST"
        fi
        record PASS "registry" "使用现成 registry：$REG_HOST"
        return 0
    fi

    docker image inspect "$REGISTRY_IMAGE" >/dev/null 2>&1 \
        || die registry "本机没有 $REGISTRY_IMAGE；请先准备临时 registry 镜像"
    REGISTRY_CONTAINER="$UNIQ-registry"
    docker rm -f "$REGISTRY_CONTAINER" >/dev/null 2>&1 || true
    if ! docker run -d --rm \
        --name "$REGISTRY_CONTAINER" \
        --label "io.echo.smoke=compose-image-pair" \
        -p 127.0.0.1::5000 \
        "$REGISTRY_IMAGE" >/dev/null 2>"$SCRATCH/logs/registry.err"; then
        cat "$SCRATCH/logs/registry.err" >&2
        die registry "临时 registry 启动失败"
    fi
    REGISTRY_OWNED=1
    local i
    for i in $(seq 1 60); do
        REG_PORT=$(docker port "$REGISTRY_CONTAINER" 5000/tcp 2>/dev/null | head -1 | sed 's/.*://')
        if [[ -n $REG_PORT ]] && curl -fsS --max-time 2 "http://127.0.0.1:$REG_PORT/v2/" >/dev/null 2>&1; then
            REG_HOST="127.0.0.1:$REG_PORT"
            record PASS "registry" "临时回环 registry 就绪：$REG_HOST（仅本次 run）"
            return 0
        fi
        sleep 1
    done
    die registry "临时 registry 在 60s 内未就绪（日志：$SCRATCH/logs/registry.err）"
}

push_and_resolve() {
    local local_ref=$1 repo=$2 label=$3
    local push_ref="$REG_HOST/$repo:$RUN_ID"
    local log="$SCRATCH/logs/push-${label}.log"
    local accept="application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json"
    local body="$SCRATCH/logs/manifest-${label}.json"
    local headers header_digest body_digest media_type

    docker tag "$local_ref" "$push_ref" || die "push-$label" "docker tag 失败"
    if ! timeout 300 docker push "$push_ref" >"$log" 2>&1; then
        tail -20 "$log" >&2
        die "push-$label" "docker push 失败（$push_ref）"
    fi
    record PASS "push-$label" "已推入临时 registry：$push_ref"

    headers=$(curl -fsS -D - -o "$body" -H "Accept: $accept" \
        "http://$REG_HOST/v2/$repo/manifests/$RUN_ID" 2>"$SCRATCH/logs/registry-${label}.err" | tr -d '\r') \
        || die "digest-$label" "registry manifest 拉取失败"
    header_digest=$(printf '%s\n' "$headers" | awk -F': ' 'tolower($1)=="docker-content-digest"{print $2}' | head -1)
    media_type=$(printf '%s\n' "$headers" | awk -F': ' 'tolower($1)=="content-type"{print $2}' | head -1)
    body_digest="sha256:$(sha256sum "$body" | cut -d' ' -f1)"

    [[ -n $header_digest ]] || die "digest-$label" "registry 未返回 Docker-Content-Digest"
    [[ "$header_digest" == "$body_digest" ]] \
        || die "digest-$label" "manifest 复算不符：header=$header_digest body=$body_digest"
    record PASS "digest-$label" "manifest 内容寻址一致（$media_type）"

    # 本机 docker 记录与 registry 返回值互证
    local repo_digest
    repo_digest=$(docker image inspect "$push_ref" --format '{{json .RepoDigests}}' 2>/dev/null \
        | tr ',' '\n' | grep -o "$repo@sha256:[0-9a-f]\{64\}" | head -1)
    if [[ -n $repo_digest ]]; then
        [[ "$repo_digest" == "$repo@$header_digest" ]] \
            || die "digest-$label" "本地 RepoDigests 与 registry 不一致：$repo_digest vs $repo@$header_digest"
        record PASS "digest-$label" "本地 RepoDigests 与 registry 互证：$repo_digest"
    else
        record WARN "digest-$label" "本地 image inspect 没给出 RepoDigests（以 registry 返回为准）"
    fi

    verify_platform "$REG_HOST/$repo@$header_digest" "$label"
    # 注意：record 会往 stdout 写，所以 digest 用全局变量带回，不能 echo。
    RESOLVED_DIGEST=$header_digest
}

verify_platform() {
    local ref=$1 label=$2
    local out="$SCRATCH/logs/platform-$label.json"
    local err="$SCRATCH/logs/platform-$label.err"
    if docker manifest inspect --insecure "$ref" >"$out" 2>"$err"; then
        if "$NODE" -e '
            const fs = require("node:fs");
            const doc = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
            const platforms = Array.isArray(doc.manifests)
              ? doc.manifests.filter((m) => m.platform).map((m) => `${m.platform.os}/${m.platform.architecture}`)
              : [];
            if (platforms.some((p) => p.includes("arm"))) {
              console.error(`unexpected arm platform: ${platforms.join(",")}`);
              process.exit(2);
            }
            if (Array.isArray(doc.manifests) && !platforms.includes("linux/amd64")) {
              console.error(`linux/amd64 missing: ${platforms.join(",") || "(none)"}`);
              process.exit(1);
            }
        ' "$out"; then
            record PASS "platform-$label" "registry manifest 是 x86_64（linux/amd64，无 arm）"
        else
            record FAIL "platform-$label" "registry manifest 平台不符（见 $out）"
        fi
    else
        record WARN "platform-$label" "docker manifest inspect 不可用：$(head -1 "$err" 2>/dev/null)"
    fi
}

# ── ③ npm 模板 + 隔离编排 ─────────────────────────────────────────────────
patch_core_toml() {
    local file=$1 token=$2
    awk -v token="$token" '
        /^\[adapters\.qq\][[:space:]]*$/ { in_qq = 1 }
        /^\[/ && !/^\[adapters\.qq\][[:space:]]*$/ { in_qq = 0 }
        /^management_access_token[[:space:]]*=/ {
            print "management_access_token = \"" token "\""
            next
        }
        in_qq && /^enabled[[:space:]]*=[[:space:]]*true[[:space:]]*$/ {
            print "enabled = false"
            next
        }
        { print }
    ' "$file" >"$file.tmp" && mv "$file.tmp" "$file"
}

setup_project() {
    mkdir -p "$PROJECT" || die project "无法创建临时部署目录"
    if ! "$NODE" "$CORE/npm/echo-agent/bin/echo-agent.js" init --dir "$PROJECT" \
        >"$SCRATCH/logs/npm-init.log" 2>&1; then
        cat "$SCRATCH/logs/npm-init.log" >&2
        die project "npm CLI init 失败"
    fi
    record PASS "project" "npm CLI init 生成部署目录（模板原样）"

    # 生成文件必须与模板逐字节一致（证明我们只做了覆盖，没有改模板）。
    local tpl_sha gen_sha
    tpl_sha=$(sha256sum "$CORE/npm/echo-agent/templates/docker-compose.yml" | cut -d' ' -f1)
    gen_sha=$(sha256sum "$PROJECT/docker-compose.yml" | cut -d' ' -f1)
    if [[ "$tpl_sha" == "$gen_sha" ]]; then
        record PASS "project" "生成的 docker-compose.yml 与 npm 模板逐字节一致（$gen_sha）"
    else
        die project "生成的 compose 与模板不一致：$gen_sha != $tpl_sha"
    fi

    # 记录模板默认值：它是公共 :latest（证明 :latest 不计入本次验收）。
    local default_core default_panel
    default_core=$(grep -E '^ECHO_CORE_IMAGE=' "$PROJECT/.env" | cut -d= -f2-)
    default_panel=$(grep -E '^ECHO_PANEL_IMAGE=' "$PROJECT/.env" | cut -d= -f2-)
    if [[ "$default_core" == *":latest" && "$default_panel" == *":latest" ]]; then
        record PASS "project" "模板默认镜像确为公共 :latest（$default_core），本次已用 digest 覆盖"
    else
        record WARN "project" "模板默认镜像不是预期的公共 :latest：$default_core / $default_panel"
    fi

    cat >"$PROJECT/.env" <<EOF
# 本次验收的 digest 覆盖：两个镜像都由本地候选构建 + 临时回环 registry 提供。
ECHO_CORE_IMAGE=$CORE_REF
ECHO_PANEL_IMAGE=$PANEL_REF
EOF

    local token
    token=$("$NODE" -e 'console.log(require("node:crypto").randomBytes(32).toString("hex"))')

    patch_core_toml "$PROJECT/config/core.toml" "$token"
    sed -i.bak -E "s|^access_token[[:space:]]*=.*$|access_token = \"$token\"|" "$PROJECT/config/panel.toml"
    rm -f "$PROJECT/config/panel.toml.bak"

    grep -q '^enabled = false$' "$PROJECT/config/core.toml" \
        || die project "未能关闭 core.toml 的 QQ 适配器"
    grep -q "management_access_token = \"$token\"" "$PROJECT/config/core.toml" \
        || die project "未能写入 core management token"
    grep -q '^connect_url = "ws://core:3132"$' "$PROJECT/config/panel.toml" \
        || die project "panel.toml 的 connect_url 不是 compose 服务名 core"
    grep -q "access_token = \"$token\"" "$PROJECT/config/panel.toml" \
        || die project "未能写入 panel access_token"
    grep -q '^api_key = ""$' "$PROJECT/config/core.toml" \
        || die project "core.toml 意外带上了 LLM api_key（本测试不连 LLM）"
    record PASS "project" "隔离配置就绪：随机 management token、QQ 关闭、Panel→ws://core:3132、无 LLM key"

    cat >"$PROJECT/docker-compose.override.yml" <<EOF
# 本文件由 tests/smoke-compose-image-pair.sh 生成：只做隔离覆盖，不改模板。
# !override / !reset 需要 compose v2.24.4+。
services:
  core:
    container_name: $UNIQ-core
    restart: "no"
    # 模板默认挂 /var/run/docker.sock —— 本测试显式去掉，只留临时 config/data。
    volumes: !override ["./config:/config", "./data:/data"]
    ports: !reset []
  panel:
    container_name: $UNIQ-panel
    restart: "no"
    # 模板默认 "8080:8080"（生产端口）——只绑回环随机端口。
    ports: !override ["127.0.0.1:$PANEL_PORT:8080"]
  # 模板里的 NapCat：挂到永不启用的 profile，连 up 不带服务名也不会起。
  napcat:
    profiles: ["wave2-pair-never"]
networks:
  echo-agent:
    name: $UNIQ-net
EOF

    COMPOSE=(
        docker compose
        -p "$PROJECT_NAME"
        --project-directory "$PROJECT"
        -f "$PROJECT/docker-compose.yml"
        -f "$PROJECT/docker-compose.override.yml"
    )
    record PASS "project" "隔离 override 写入（项目 $PROJECT_NAME，网络 $UNIQ-net，panel 127.0.0.1:$PANEL_PORT）"
}

compose_config_checks() {
    local cfg="$SCRATCH/compose.config.json"
    if ! "${COMPOSE[@]}" config --format json >"$cfg" 2>"$SCRATCH/logs/compose-config.err"; then
        cat "$SCRATCH/logs/compose-config.err" >&2
        die config "docker compose config 失败"
    fi
    docker image inspect "$CORE_LOCAL" >"$SCRATCH/core.image.json" 2>/dev/null \
        || die config "无法 inspect Core 候选镜像"
    docker image inspect "$PANEL_LOCAL" >"$SCRATCH/panel.image.json" 2>/dev/null \
        || die config "无法 inspect Panel 候选镜像"

    if "$NODE" "$CORE/tests/smoke-compose-image-pair.verify.mjs" \
        --compose "$cfg" \
        --core-image "$SCRATCH/core.image.json" \
        --panel-image "$SCRATCH/panel.image.json" \
        --expect-core-ref "$CORE_REF" \
        --expect-panel-ref "$PANEL_REF" \
        --expect-core-digest "$CORE_DIGEST" \
        --expect-panel-digest "$PANEL_DIGEST" \
        --expect-core-id "$CORE_ID" \
        --expect-panel-id "$PANEL_ID" \
        --project "$PROJECT_NAME" \
        --net-name "$UNIQ-net" \
        --panel-port "$PANEL_PORT" \
        --project-dir "$PROJECT" \
        --modules-summary "$MODULES_SUMMARY" \
        --core-rev "$CORE_REV" \
        --panel-rev "$PANEL_REV" \
        --core-context-sha256 "$CORE_CONTEXT_SHA" \
        --panel-context-sha256 "$PANEL_CONTEXT_SHA" \
        >"$SCRATCH/logs/verify-config.log" 2>&1; then
        record PASS "config" "compose 解析结果满足 digest/隔离契约（详见 verify-config.log）"
    else
        cat "$SCRATCH/logs/verify-config.log" >&2
        die config "compose 静态契约断言失败"
    fi
}

# ── ④ 运行态 + 应用层 WS/重连 ─────────────────────────────────────────────
force_pull_by_digest() {
    local label=$1 ref=$2 repo=$3 digest=$4 tag_ref=$5
    docker image rm -f "$tag_ref" >/dev/null 2>&1 || true
    docker image rm -f "$ref" >/dev/null 2>&1 || true
    if docker image inspect "$ref" >/dev/null 2>&1; then
        record WARN "pull-$label" "删除本地引用后 digest 引用仍在（存储实现差异，pull 仍会走 registry）"
    else
        record PASS "pull-$label" "本地 digest 引用已删除，接下来只能从 registry 按 digest 取"
    fi
}

compose_pull_and_up() {
    local log="$SCRATCH/logs/compose-pull.log"
    if ! timeout 300 "${COMPOSE[@]}" pull core panel >"$log" 2>&1; then
        tail -30 "$log" >&2
        die pull "docker compose pull 失败（应按 digest 从临时 registry 拉取）"
    fi
    record PASS "pull" "docker compose pull 按 digest 成功（core/panel）"

    if docker image inspect "$CORE_REF" >/dev/null 2>&1 && docker image inspect "$PANEL_REF" >/dev/null 2>&1; then
        record PASS "pull" "两个 digest 引用现在都能被 docker 解析"
    else
        die pull "pull 之后仍无法解析 digest 引用"
    fi

    local core_id_pulled panel_id_pulled
    core_id_pulled=$(docker image inspect "$CORE_REF" --format '{{.Id}}')
    panel_id_pulled=$(docker image inspect "$PANEL_REF" --format '{{.Id}}')
    [[ "$core_id_pulled" == "$CORE_ID" ]] \
        || die pull "按 digest 拉到的 Core 镜像 Id 与候选不一致：$core_id_pulled != $CORE_ID"
    [[ "$panel_id_pulled" == "$PANEL_ID" ]] \
        || die pull "按 digest 拉到的 Panel 镜像 Id 与候选不一致：$panel_id_pulled != $PANEL_ID"
    record PASS "pull" "digest 拉取到的镜像 Id 与候选完全一致（core=$CORE_ID panel=$PANEL_ID）"

    local up_log="$SCRATCH/logs/compose-up.log"
    if ! timeout 300 "${COMPOSE[@]}" up -d --no-build core panel >"$up_log" 2>&1; then
        tail -30 "$up_log" >&2
        "${COMPOSE[@]}" logs --no-color --tail 80 core panel >&2 2>/dev/null || true
        die up "docker compose up 失败"
    fi
    COMPOSE_UP=1
    record PASS "up" "隔离项目已启动：只起 core/panel（无 NapCat、无 docker.sock）"

    local names
    names=$(docker ps -a --filter "label=com.docker.compose.project=$PROJECT_NAME" --format '{{.Names}}' | sort | tr '\n' ' ')
    if [[ "$names" == "$UNIQ-core $UNIQ-panel " ]]; then
        record PASS "up" "项目内只有两个容器：$names"
    else
        die up "项目容器集合异常：$names"
    fi
    local images
    images=$(docker ps -a --filter "label=com.docker.compose.project=$PROJECT_NAME" --format '{{.Image}}' | sort)
    if printf '%s\n' "$images" | grep -qi 'napcat'; then
        die up "项目里出现了 NapCat 容器：$images"
    fi
    record PASS "up" "项目内没有任何 NapCat 镜像/容器"
}

container_checks() {
    local core_cid panel_cid
    core_cid=$("${COMPOSE[@]}" ps -q core)
    panel_cid=$("${COMPOSE[@]}" ps -q panel)
    [[ -n $core_cid && -n $panel_cid ]] || die runtime "compose 没有给出两个容器 ID"

    docker inspect "$core_cid" >"$SCRATCH/core.container.json" 2>/dev/null || die runtime "inspect core 失败"
    docker inspect "$panel_cid" >"$SCRATCH/panel.container.json" 2>/dev/null || die runtime "inspect panel 失败"
    "$NODE" -e '
        const fs = require("node:fs");
        const out = process.argv[1], core = process.argv[2], panel = process.argv[3];
        fs.writeFileSync(out, JSON.stringify({
          core: JSON.parse(fs.readFileSync(core, "utf8")),
          panel: JSON.parse(fs.readFileSync(panel, "utf8")),
        }));
    ' "$SCRATCH/containers.json" "$SCRATCH/core.container.json" "$SCRATCH/panel.container.json"

    if "$NODE" "$CORE/tests/smoke-compose-image-pair.verify.mjs" \
        --compose "$SCRATCH/compose.config.json" \
        --core-image "$SCRATCH/core.image.json" \
        --panel-image "$SCRATCH/panel.image.json" \
        --containers "$SCRATCH/containers.json" \
        --expect-core-ref "$CORE_REF" \
        --expect-panel-ref "$PANEL_REF" \
        --expect-core-digest "$CORE_DIGEST" \
        --expect-panel-digest "$PANEL_DIGEST" \
        --expect-core-id "$CORE_ID" \
        --expect-panel-id "$PANEL_ID" \
        --project "$PROJECT_NAME" \
        --net-name "$UNIQ-net" \
        --panel-port "$PANEL_PORT" \
        --project-dir "$PROJECT" \
        --modules-summary "$MODULES_SUMMARY" \
        --core-rev "$CORE_REV" \
        --panel-rev "$PANEL_REV" \
        --core-context-sha256 "$CORE_CONTEXT_SHA" \
        --panel-context-sha256 "$PANEL_CONTEXT_SHA" \
        >"$SCRATCH/logs/verify-runtime.log" 2>&1; then
        record PASS "runtime" "运行中容器的镜像 Id / 挂载 / 端口 / 环境满足契约"
    else
        cat "$SCRATCH/logs/verify-runtime.log" >&2
        die runtime "运行态断言失败"
    fi

    # 正式镜像的模块锁定摘要：OCI 标签 + 镜像内文件双证据
    local label_source label_rev modfile
    label_source=$(docker image inspect "$CORE_REF" --format '{{index .Config.Labels "io.echo.external-modules.source"}}')
    label_rev=$(docker image inspect "$CORE_REF" --format '{{index .Config.Labels "io.echo.external-modules.revisions"}}')
    [[ "$label_source" == "official" && "$label_rev" == "$MODULES_SUMMARY" ]] \
        || die runtime "Core OCI 标签不是 official/锁定摘要（$label_source / $label_rev）"
    record PASS "runtime" "Core OCI 标签：source=official，revisions=$label_rev"

    modfile=$("${COMPOSE[@]}" exec -T core cat /usr/local/share/echo-agent-core/external-modules.txt 2>/dev/null)
    local tag key repo sha
    while read -r tag key repo sha _; do
        [[ "$tag" == "module" ]] || continue
        if ! grep -q "$sha" <<<"$modfile"; then
            die runtime "镜像内 external-modules.txt 缺少 $key@$sha"
        fi
    done <"$CORE/packaging/external-modules.lock"
    if grep -q 'UNLOCKED' <<<"$modfile"; then
        die runtime "镜像内 external-modules.txt 出现 UNLOCKED（不是正式构建）"
    fi
    record PASS "runtime" "镜像内 external-modules.txt 与锁定表逐条一致且无 UNLOCKED"
}

wait_for_panel_http() {
    local i
    for i in $(seq 1 90); do
        if curl -fsS --max-time 2 "http://127.0.0.1:$PANEL_PORT/" >/dev/null 2>&1; then
            record PASS "http" "Panel HTTP 就绪：http://127.0.0.1:$PANEL_PORT/（第 ${i} 次探测）"
            return 0
        fi
        sleep 1
    done
    "${COMPOSE[@]}" logs --no-color --tail 60 panel >&2 2>/dev/null || true
    die http "Panel HTTP 在 90s 内未就绪"
}

run_app_smoke() {
    local label=$1 tries=$2
    local out="$SCRATCH/logs/app-$label.log"
    local i
    for i in $(seq 1 "$tries"); do
        if "$NODE" "$CORE/tests/smoke-panel-application.mjs" "$PANEL_PORT" >"$out" 2>&1; then
            record PASS "app-$label" "Panel→Core 应用 WS 往返成功（初始+重连；第 ${i} 次尝试）"
            return 0
        fi
        sleep 2
    done
    tail -20 "$out" >&2
    record FAIL "app-$label" "smoke-panel-application.mjs 失败（$tries 次尝试，日志 $out）"
    return 1
}

app_and_reconnect_checks() {
    wait_for_panel_http

    if run_app_smoke initial 45; then
        APP_SMOKE_RESULT="pass(initial+browser-reconnect)"
    else
        APP_SMOKE_RESULT="fail(initial)"
        return 1
    fi

    # 真实的上游重连：重启 Core 容器，Panel 的 /ws 中继必须能重新接上。
    say "重启 Core 容器，验证 Panel→Core 上游 WS 重连…"
    if ! timeout 120 "${COMPOSE[@]}" restart core >"$SCRATCH/logs/compose-restart-core.log" 2>&1; then
        tail -20 "$SCRATCH/logs/compose-restart-core.log" >&2
        die reconnect "docker compose restart core 失败"
    fi
    local core_cid i
    for i in $(seq 1 60); do
        core_cid=$("${COMPOSE[@]}" ps -q core)
        if [[ -n $core_cid ]] && [[ "$(docker inspect "$core_cid" --format '{{.State.Running}}')" == "true" ]]; then
            break
        fi
        sleep 1
    done
    [[ -n $core_cid ]] || die reconnect "重启后找不到 core 容器"

    local core_image_after
    core_image_after=$(docker inspect "$core_cid" --format '{{.Image}}')
    [[ "$core_image_after" == "$CORE_ID" ]] \
        || die reconnect "重启后 core 容器镜像变了：$core_image_after != $CORE_ID"
    record PASS "reconnect" "core 重启后仍是同一 digest 镜像（$CORE_ID）"

    if run_app_smoke reconnect 60; then
        APP_SMOKE_RESULT="pass(initial+browser-reconnect+core-restart)"
        record PASS "reconnect" "Core 重启后 Panel→Core 应用 WS 重新建立并完成往返"
    else
        APP_SMOKE_RESULT="fail(reconnect)"
        return 1
    fi
}

# ── 主流程 ────────────────────────────────────────────────────────────────
main() {
    hdr "① 预检"
    need_cmd docker
    need_cmd curl
    need_cmd tar
    need_cmd git
    need_cmd timeout
    docker info >/dev/null 2>&1 || die preflight "docker daemon 不可用"
    docker compose version >/dev/null 2>&1 || die preflight "docker compose 不可用"

    local compose_version
    compose_version=$(docker compose version --short 2>/dev/null | sed 's/^v//')
    if [[ -z $compose_version ]] || [[ "$(printf '%s\n2.24.4\n' "$compose_version" | sort -V | head -1)" != "2.24.4" ]]; then
        die preflight "docker compose 需要 ≥2.24.4（!override/!reset），当前：$compose_version"
    fi
    record PASS "preflight" "docker $(docker version --format '{{.Server.Version}}') / compose $compose_version"

    [[ -d $CORE ]] || die preflight "Core 源码目录不存在：$CORE"
    [[ -f $CORE/Dockerfile ]] || die preflight "Core Dockerfile 不存在"
    [[ -f $CORE/npm/echo-agent/bin/echo-agent.js ]] || die preflight "npm CLI 不存在"
    [[ -f $CORE/tests/smoke-panel-application.mjs ]] || die preflight "既有应用层 smoke 脚本不存在"
    [[ -f $CORE/tests/smoke-compose-image-pair.verify.mjs ]] || die preflight "verify 辅助脚本不存在"
    [[ -d $PANEL ]] || die preflight "Panel 源码目录不存在：$PANEL"
    [[ -f $PANEL/Dockerfile ]] || die preflight "Panel Dockerfile 不存在"

    NODE=$(detect_node) || die preflight "找不到 Node.js ≥22（可用 --node 指定）"
    "$NODE" -e 'process.exit(Number(process.versions.node.split(".")[0]) >= 22 ? 0 : 1)' \
        || die preflight "Node.js 版本过低（需要 ≥22 才有全局 WebSocket）：$("$NODE" --version)"
    record PASS "preflight" "Node.js $("$NODE" --version)（$NODE）"

    SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/smoke-compose-image-pair.XXXXXX") || die preflight "mktemp 失败"
    PROJECT="$SCRATCH/project"
    mkdir -p "$SCRATCH/logs"
    REPORT_FILE=${REPORT_FILE:-${TMPDIR:-/tmp}/smoke-compose-image-pair-$RUN_ID.report.txt}

    PROD_BEFORE=$(docker ps -q | sort | tr '\n' ' ')
    record PASS "preflight" "生产容器基线：$(wc -w <<<"$PROD_BEFORE") 个运行中容器（结束时逐 ID 比对）"

    CORE_LOCAL=${CORE_IMAGE_ARG:-"echo-agent-wave2/core:$RUN_ID"}
    PANEL_LOCAL=${PANEL_IMAGE_ARG:-"echo-agent-wave2/panel:$RUN_ID"}
    CORE_REV=$(git_rev "$CORE")
    PANEL_REV=$(git_rev "$PANEL")
    CORE_DIRTY=$(git_dirty "$CORE")
    PANEL_DIRTY=$(git_dirty "$PANEL")

    hdr "① 构建候选镜像（可复用）"
    if [[ -n $CORE_IMAGE_ARG ]]; then
        docker image inspect "$CORE_LOCAL" >/dev/null 2>&1 || die build "指定的 Core 镜像不存在：$CORE_LOCAL"
        CORE_CONTEXT_SHA=$(docker image inspect "$CORE_LOCAL" --format '{{index .Config.Labels "io.echo.smoke.core.context"}}')
        CORE_REV=$(docker image inspect "$CORE_LOCAL" --format '{{index .Config.Labels "io.echo.smoke.core.rev"}}')
        MODULES_SUMMARY=$(docker image inspect "$CORE_LOCAL" --format '{{index .Config.Labels "io.echo.external-modules.revisions"}}')
        [[ -n $CORE_CONTEXT_SHA && "$CORE_CONTEXT_SHA" != "<no value>" \
            && -n $CORE_REV && "$CORE_REV" != "<no value>" \
            && -n $MODULES_SUMMARY && "$MODULES_SUMMARY" != "<no value>" ]] \
            || die build "复用镜像缺少候选标签（context/rev/revisions）：$CORE_LOCAL"
        record PASS "build-core" "复用已有 Core 镜像：$CORE_LOCAL"
    else
        local core_context=${CORE_CONTEXT_ARG:-"$SCRATCH/context/core"}
        if [[ -n $CORE_CONTEXT_ARG ]]; then
            [[ -f $CORE_CONTEXT_ARG/Dockerfile ]] || die build "Core 构建上下文无效：$CORE_CONTEXT_ARG"
        else
            say "快照 Core 源码到隔离构建上下文…"
            snapshot_tree "$CORE" "$core_context" || die build "Core 源码快照失败"
        fi
        CORE_CONTEXT_SHA=$(tree_sha "$core_context")
        # 摘要必须来自**构建上下文里的同一份锁定表**（不是当前工作树）——
        # 否则并行改动会让标签值与镜像里实际构建的模块对不上。
        MODULES_SUMMARY=$(cd "$core_context" && bash scripts/release/external-modules.sh oci-summary image 2>/dev/null) \
            || die build "无法从锁定表取 external-modules oci-summary"
        [[ -n $MODULES_SUMMARY ]] || die build "external-modules oci-summary 为空"
        record PASS "build-core" "构建上下文 sha256=$CORE_CONTEXT_SHA（快照，不受并行改动影响）"
        build_core "$core_context"
    fi

    if [[ -n $PANEL_IMAGE_ARG ]]; then
        docker image inspect "$PANEL_LOCAL" >/dev/null 2>&1 || die build "指定的 Panel 镜像不存在：$PANEL_LOCAL"
        PANEL_CONTEXT_SHA=$(docker image inspect "$PANEL_LOCAL" --format '{{index .Config.Labels "io.echo.smoke.panel.context"}}')
        PANEL_REV=$(docker image inspect "$PANEL_LOCAL" --format '{{index .Config.Labels "io.echo.smoke.panel.rev"}}')
        [[ -n $PANEL_CONTEXT_SHA && "$PANEL_CONTEXT_SHA" != "<no value>" \
            && -n $PANEL_REV && "$PANEL_REV" != "<no value>" ]] \
            || die build "复用镜像缺少候选标签（context/rev）：$PANEL_LOCAL"
        record PASS "build-panel" "复用已有 Panel 镜像：$PANEL_LOCAL"
    else
        local panel_context=${PANEL_CONTEXT_ARG:-"$SCRATCH/context/panel"}
        if [[ -n $PANEL_CONTEXT_ARG ]]; then
            [[ -f $PANEL_CONTEXT_ARG/Dockerfile ]] || die build "Panel 构建上下文无效：$PANEL_CONTEXT_ARG"
        else
            say "快照 Panel 源码到隔离构建上下文…"
            snapshot_tree "$PANEL" "$panel_context" || die build "Panel 源码快照失败"
        fi
        PANEL_CONTEXT_SHA=$(tree_sha "$panel_context")
        record PASS "build-panel" "构建上下文 sha256=$PANEL_CONTEXT_SHA"
        build_panel "$panel_context"
    fi

    CORE_ID=$(docker image inspect "$CORE_LOCAL" --format '{{.Id}}' 2>/dev/null) || die build "无法读取 Core 镜像 Id"
    PANEL_ID=$(docker image inspect "$PANEL_LOCAL" --format '{{.Id}}' 2>/dev/null) || die build "无法读取 Panel 镜像 Id"
    local arch_core arch_panel
    arch_core=$(docker image inspect "$CORE_LOCAL" --format '{{.Os}}/{{.Architecture}}')
    arch_panel=$(docker image inspect "$PANEL_LOCAL" --format '{{.Os}}/{{.Architecture}}')
    [[ "$arch_core" == "linux/amd64" ]] || die build "Core 镜像不是 linux/amd64：$arch_core"
    [[ "$arch_panel" == "linux/amd64" ]] || die build "Panel 镜像不是 linux/amd64：$arch_panel"
    record PASS "build" "两张候选镜像都是 linux/amd64（core=$CORE_ID panel=$PANEL_ID）"

    hdr "② 临时回环 registry + digest 取证"
    start_registry
    push_and_resolve "$CORE_LOCAL" "echo-agent-wave2/core" core
    CORE_DIGEST=$RESOLVED_DIGEST
    push_and_resolve "$PANEL_LOCAL" "echo-agent-wave2/panel" panel
    PANEL_DIGEST=$RESOLVED_DIGEST
    [[ -n $CORE_DIGEST && -n $PANEL_DIGEST ]] || die digest "digest 解析为空"
    CORE_REF="$REG_HOST/echo-agent-wave2/core@$CORE_DIGEST"
    PANEL_REF="$REG_HOST/echo-agent-wave2/panel@$PANEL_DIGEST"
    record PASS "pair" "digest 配对成立：core=$CORE_DIGEST panel=$PANEL_DIGEST"
    PANEL_PORT=$(choose_port) || die project "无法选择空闲端口"
    [[ -n $PANEL_PORT ]] || die project "无法选择空闲端口"
    setup_project
    compose_config_checks

    hdr "④ 按 digest 拉取、启动、应用层 WS/重连"
    force_pull_by_digest core "$CORE_REF" "echo-agent-wave2/core" "$CORE_DIGEST" "$REG_HOST/echo-agent-wave2/core:$RUN_ID"
    force_pull_by_digest panel "$PANEL_REF" "echo-agent-wave2/panel" "$PANEL_DIGEST" "$REG_HOST/echo-agent-wave2/panel:$RUN_ID"
    compose_pull_and_up
    container_checks
    app_and_reconnect_checks
}

main
