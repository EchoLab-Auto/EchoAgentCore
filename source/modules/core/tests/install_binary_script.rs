//! Guard: 二进制安装器（`scripts/install-binary.sh`）与源码安装器
//! （`scripts/install.sh`）的**布局一致性**，以及 release 产物契约。
//!
//! 背景（2026-11）：二进制安装是**独立脚本**（决策：不往 install.sh 里塞
//! 第二个分支，那会让一个 1000 行的脚本同时背着两套依赖模型），代价是它必须
//! 与 install.sh 共享同一套安装布局——否则：
//!
//! - 两条路装出的部署 `/prefix` 结构不同 → `uninstall.sh` 只能删掉一种；
//! - unit 模板占位符漏渲染 → 装出来是个 `ExecStart="@BINARY@"` 的坏单元；
//! - release tarball 的目录结构漂移 → 装机到第 5 步才报"缺文件"。
//!
//! 这些都不是"跑一次就能发现"的（要真有台干净机器），所以在这里用静态断言
//! 兜住。与 [`install_script_flag.rs`] 同一模式：不执行安装，只校接线。

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("modules/")
        .parent()
        .expect("source/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_script(name: &str) -> String {
    read_file(&repo_root().join("scripts").join(name))
}

fn read_unit_template() -> String {
    read_file(&repo_root().join("packaging/systemd/echo-agent-core.service.in"))
}

/// 两条安装脚本都必须通过 `bash -n`。
///
/// release 里下载的 install-binary.sh 是 `curl | bash` 直接执行的，语法错误
/// 在现场只表现为"脚本什么都没做就退出"。
#[test]
fn install_binary_script_is_bash_syntax_clean() {
    let path = repo_root().join("scripts/install-binary.sh");
    let status = Command::new("bash")
        .arg("-n")
        .arg(&path)
        .status()
        .unwrap_or_else(|e| panic!("run bash -n {}: {e}", path.display()));
    assert!(status.success(), "bash -n failed for {}", path.display());
}

/// 抠出 `NAME() {` 到行首 `}` 的函数体。
fn function_body(script: &str, signature: &str) -> String {
    let start = script
        .find(signature)
        .unwrap_or_else(|| panic!("script should declare `{signature}`"));
    let rest = &script[start..];
    let begin = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
    let mut body = String::new();
    for line in rest[begin..].lines() {
        if line == "}" {
            return body;
        }
        body.push_str(line);
        body.push('\n');
    }
    panic!("unterminated body after `{signature}`");
}

/// 脚本里所有 `KEY="…"` 形式的赋值行（去掉前后空白，忽略注释）。
fn path_assignments(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return false;
            };
            !key.is_empty()
                && !key.contains(' ')
                && value.starts_with('"')
                && key.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// `derive_paths` 是两条安装路**共用布局的唯一实现**。
///
/// 断言的形状是"子集相等"：二进制安装允许**少**几个键（并显式写进
/// `allowed_missing`），也允许有 install.sh 没有的少量额外键（同样白名单），
/// 但 install.sh 里出现的每个路径赋值都必须在二进制安装器里**逐字出现**。
/// 改一条路径忘了改另一条 = 这里红。
#[test]
fn path_layout_is_shared_with_the_source_installer() {
    let install = read_script("install.sh");
    let binary = read_script("install-binary.sh");

    let source_body = function_body(&install, "derive_paths() {");
    let binary_body = function_body(&binary, "derive_paths() {");

    let source_assignments = path_assignments(&source_body);
    assert!(
        !source_assignments.is_empty(),
        "derive_paths in install.sh should assign install paths"
    );

    // 二进制安装**故意**不推导的键：它没有源码检出，`$DATA_DIR/source` 这个
    // 受管克隆目录不存在（工作目录直接用 `$DATA_DIR`）。除此之外一个都不许少。
    let allowed_missing = ["SOURCE_DIR"];
    // 反过来：二进制安装可以多出的键必须在这里点名，避免这里变成随手加目录的后门。
    let allowed_extra = ["MODULES_CONFIG"];

    let binary_assignments = path_assignments(&binary_body);
    for assignment in &source_assignments {
        if binary_assignments.contains(assignment) {
            continue;
        }
        let key = assignment.split('=').next().unwrap_or_default();
        assert!(
            allowed_missing.contains(&key),
            "install-binary.sh 的 derive_paths 缺少/改动了 install.sh 里的路径：\n  {assignment}\n\
             两条安装路必须落出同一套布局，否则 uninstall.sh 只能删掉其中一种。\
             （确实不适用于二进制安装的键要显式加进 allowed_missing 并写明理由。）"
        );
    }

    for assignment in &binary_assignments {
        if source_assignments.contains(assignment) {
            continue;
        }
        let key = assignment.split('=').next().unwrap_or_default();
        assert!(
            allowed_extra.contains(&key),
            "install-binary.sh 引入了 install.sh 没有的布局键 {key}；\
             共享布局是新键的唯一放行方式（先加到 install.sh，再同步过来）"
        );
    }
}

/// unit 模板里的每个 `@PLACEHOLDER@` 都必须被二进制安装器渲染。
///
/// 漏渲染的症状极隐蔽：单元文件里留着字面量 `@STATUS_FILE@`，systemd 能正常
/// 加载（它只是个字符串值），直到自更新/状态查询去碰那个路径才炸。
#[test]
fn every_unit_placeholder_is_rendered() {
    let template = read_unit_template();
    let binary = read_script("install-binary.sh");

    let mut placeholders: Vec<String> = Vec::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find('@') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('@') else { break };
        let name = &after[..end];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            let placeholder = format!("@{name}@");
            if !placeholders.contains(&placeholder) {
                placeholders.push(placeholder);
            }
        }
        rest = &after[end + 1..];
    }

    assert!(
        placeholders.len() >= 8,
        "unit template should carry the full placeholder set, got {placeholders:?}"
    );

    for placeholder in placeholders {
        // `@SCOPE_LINES@` 是**多行**占位符：sed 的替换串里嵌换行不可读，两条
        // 安装脚本都用 `awk` 整行替换（`$0 == "@SCOPE_LINES@"`）而不是 `s|…|`。
        // 两种形态都算"已渲染"，但必须恰好命中一种。
        let single_line = binary.contains(&format!("s|{placeholder}|"));
        let whole_line = binary.contains(&format!("$0 == \"{placeholder}\""));
        assert!(
            single_line || whole_line,
            "install-binary.sh 没有渲染 unit 占位符 {placeholder}；\
             装出来的单元会带着字面量占位符跑。"
        );
    }

    // 源码安装与二进制安装必须复用**同一份模板**（不另存一份 -binary 变体）。
    assert!(
        !repo_root()
            .join("packaging/systemd/echo-agent-core-binary.service.in")
            .exists(),
        "不应新增 echo-agent-core-binary.service.in：unit 模板只该有一份，\
         两份必然漂移（漂移后果是某种装法缺了某条 Restart/加固策略）。"
    );
    assert!(
        binary.contains("packaging/systemd/echo-agent-core.service.in")
            || binary.contains("systemd/echo-agent-core.service.in"),
        "install-binary.sh 应从产物里的 systemd/echo-agent-core.service.in 渲染单元"
    );
}

/// 二进制安装**不装 updater 单元**，并把自更新模块显式关掉。
///
/// 两者缺一都会留下一个"点了必然失败"的入口：没有源码检出，updater 里的
/// `cargo build` 无从执行。
#[test]
fn self_update_is_disabled_for_binary_installs() {
    let binary = read_script("install-binary.sh");

    assert!(
        binary.contains(r#"id = "echo-agent.selfupdate""#),
        "install-binary.sh 应在 modules.toml 里写 echo-agent.selfupdate 条目"
    );
    assert!(
        binary.contains("enabled = false"),
        "install-binary.sh 应把自更新显式关掉"
    );
    assert!(
        binary.contains("echo-agent-core-update.service"),
        "install-binary.sh 应清理残留的 updater 单元（此前可能是源码安装）"
    );
    assert!(
        !binary.contains("install -m 755 \"$PROJECT_ROOT/scripts/update.sh\""),
        "install-binary.sh 不应安装 update.sh 更新脚本（它依赖源码检出）"
    );
}

/// 内嵌 Panel 的前端目录必须落在 panel 仓的**自动发现候选**里。
///
/// 这个断言跨仓（候选列表在 EchoAgentPanel 的 `static_dir_candidates`），只能
/// 锁住"我们选的那个位置"确实是候选之一，而不是发明一个必须手写 `static_dir`
/// 的目录。
///
/// 选的是 `exe.parent()/web`（Core 二进制同级）。**不选** panel README 写着的
/// `<prefix>/libexec/echo-agent-panel/web`：2026-11 实测该位置在代码里被拼成了
/// `<prefix>/libexec/libexec/echo-agent-panel/web`（取 `exe.parent()` 之后又
/// join 了一次 `libexec/`），干净机器上必然找不到。同级 `web/` 不受该缺陷影响，
/// 且与二进制同版本淘汰——前端 API 契约随 Core 走。
#[test]
fn panel_web_dir_matches_the_documented_autodiscovery_layout() {
    let binary = read_script("install-binary.sh");
    assert!(
        binary.contains(r#"PANEL_WEB_DIR="$LIBEXEC_DIR/web""#),
        "Panel 前端应装到 Core 二进制同级 web/（内嵌模块候选列表的第一命中项）"
    );
    // 内嵌形态必须显式启用：模块默认不挂载、不监听。
    assert!(
        binary.contains(r#"id = "echo-agent.panel.web""#),
        "install-binary.sh 应在 modules.toml 里启用内嵌 Panel"
    );
    // 端口被占时要给人一条明确的出路：内嵌形态靠 bind 换端口，而不是停掉别的服务。
    assert!(
        binary.contains("bind = \"127.0.0.1:8081\"")
            || binary.contains(r#"bind = "127.0.0.1:8081""#),
        "modules.toml 模板应提示用 [module.config] bind 改端口"
    );
}

/// release 产物的目录结构是安装器的输入契约——两边必须逐项对上。
#[test]
fn release_artifact_layout_matches_the_installer() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    let binary = read_script("install-binary.sh");

    // 安装器要求的必需项（顺序无关，存在即可）。
    for required in [
        "bin/echo-agent-core-bin",
        "web/index.html",
        "manifest.json",
        "systemd/echo-agent-core.service.in",
        "config/echo-agent-core.toml",
        "skills",
    ] {
        assert!(binary.contains(required), "安装器应校验产物里的 {required}");
    }

    // workflow 必须真的产出这些路径。
    assert!(
        workflow.contains("stage/bin/echo-agent-core-bin"),
        "release.yml 应把二进制装成 bin/echo-agent-core-bin"
    );
    assert!(
        workflow.contains("stage/systemd/echo-agent-core.service.in"),
        "release.yml 应打包 unit 模板"
    );
    assert!(
        workflow.contains("stage/config/echo-agent-core.toml"),
        "release.yml 应打包 core.toml 模板"
    );
    assert!(
        workflow.contains("stage/manifest.json") || workflow.contains("stage/manifest.json <<EOF"),
        "release.yml 应生成 manifest.json"
    );
    assert!(
        workflow.contains(r#"tar -C stage -czf"#),
        "release.yml 应从 stage 目录打包（路径相对 stage，安装器按此解包）"
    );
    // 校验和边车文件是安装器的唯一完整性依据：release 必须产出它。
    assert!(
        workflow.contains("sha256sum"),
        "release.yml 应生成 .sha256 侧车文件（install-binary.sh 默认要求）"
    );
    // glibc 下限必须从产物符号表读，而不是直接抄构建机版本：虚报会拦掉本来能跑
    // 的老系统，漏报会在启动时炸 `GLIBC_2.xx not found`。
    assert!(
        workflow.contains("objdump -T stage/bin/echo-agent-core-bin"),
        "release.yml 应从产物符号表读 glibc 下限"
    );

    // 发版构建不能设 `-D warnings`：2026-11 实测 v0.1.0 首次构建两个架构同时
    // 失败在 `Build Core`，当前 main 有 40 条既有告警，一条就让构建变 error。
    // 告警门禁归 CI，发版只负责把已通过 CI 的代码打成产物。
    assert!(
        !workflow.contains(r#"RUSTFLAGS: "-D warnings""#),
        "release.yml 不应设 RUSTFLAGS=-D warnings（既有告警会让发版构建失败）"
    );

    // tag 与 Cargo.toml 版本必须一致，且在**构建前**失败。
    // 背景：v0.2.0 的产物装上去 `--version` 打印 `0.1.0`（`CARGO_PKG_VERSION`
    // 来自 Cargo.toml，与 tag 无关），用户第一反应是"装错了"。
    assert!(
        workflow.contains("Verify tag matches Cargo.toml version"),
        "release.yml 应在构建前核对 tag 与 Cargo.toml 版本"
    );
    assert!(
        workflow.contains("scripts/bump-version.sh --print"),
        "release.yml 应通过 scripts/bump-version.sh 读取版本（唯一真值）"
    );
    // 声明的版本对 != 编出来的东西对：再验一次二进制的自报版本。
    assert!(
        workflow.contains("Verify the built binary reports the tagged version"),
        "release.yml 应校验产物二进制的自报版本"
    );
}

/// aarch64 必须由原生 arm runner 构建（ring 含 C 代码，交叉编译要额外工具链）。
#[test]
fn aarch64_is_built_on_a_native_arm_runner() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    assert!(
        workflow.contains("ubuntu-24.04-arm"),
        "release.yml 应用 ubuntu-24.04-arm 原生 runner 构建 aarch64"
    );
    assert!(
        workflow.contains("arch: aarch64"),
        "release.yml 的 matrix 应包含 aarch64"
    );
    // x86_64 必须用 22.04：构建机 glibc 就是产物下限。实测在 24.04 上构建的
    // Core 需要 `pidfd_spawnp`（GLIBC_2.39），Ubuntu 22.04 的生产节点直接跑不起来。
    assert!(
        workflow.contains("runner: ubuntu-22.04"),
        "x86_64 应在 ubuntu-22.04（glibc 2.35）上构建，否则 22.04 机器装不上"
    );
    assert!(
        workflow.contains("arch: x86_64"),
        "release.yml 的 matrix 应包含 x86_64"
    );
}

/// Panel 前端版本必须与 Core 二进制**同源**：从 Cargo.lock 解析 pin 住的
/// commit，而不是用 `branch = main` 现取。
///
/// 否则同一个 tag 重跑会得到不同前端，前端与 Core 的 API 契约可能不匹配。
#[test]
fn panel_frontend_is_built_from_the_pinned_revision() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    assert!(
        workflow.contains("agent-module-panel"),
        "release.yml 应从 Cargo.lock 解析 agent-module-panel 的 commit"
    );
    assert!(
        workflow.contains("ref: ${{ steps.pin.outputs.panel_revision }}"),
        "release.yml 应把 panel 检出固定到 Cargo.lock 里 pin 的 commit"
    );
    assert!(
        !workflow.contains("ref: main"),
        "release.yml 不应以 main 检出 panel（不可复现）"
    );
}

/// 安装成功后的清理**不能**对目录用 `rm -f`。
///
/// 2026-11 实测的坑：回滚快照里除了文件还有一个 `web/` 目录，`rm -f <目录>`
/// 报 "Is a directory" 并返回非零；此刻 `set -e` 仍然生效、事务守卫已解除，
/// 于是**装成功了却以退出码 1 结束**，且不回滚——用户看到失败、脚本认为没事。
#[test]
fn rollback_cleanup_skips_directories() {
    let binary = read_script("install-binary.sh");
    // 成功路径是**最后一次** `INSTALL_TRANSACTION=0`（前面回滚函数里也有一次），
    // 其后紧跟清理循环——两者之间允许有注释，所以分段找。
    let flag = binary
        .rfind("INSTALL_TRANSACTION=0")
        .expect("install-binary.sh should clear the rollback dir after success");
    let start = flag
        + binary[flag..]
            .find("for rollback_file in")
            .expect("success path should iterate the rollback dir");
    let rest = &binary[start..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    let cleanup = &rest[..end];

    assert!(
        cleanup.contains(r#"if [[ -f "$rollback_file" ]]"#),
        "清理循环应只对普通文件 rm -f（目录会让 rm 失败并让整个安装谎报失败）\n{cleanup}"
    );
    assert!(
        cleanup.contains(r#"rm -rf -- "$ROLLBACK_DIR/web""#),
        "web 快照目录应单独 rm -rf\n{cleanup}"
    );
}

/// Panel 就绪探测必须能区分"我们的内嵌面板"和"恰好占用 8080 的其它服务"。
///
/// 只探 `GET /` 或只判端口会误报：本机跑着独立 Panel（开发实例）时，
/// 装到临时 prefix 的这次安装照样打印 "responding"。
#[test]
fn panel_readiness_probe_checks_embedded_identity() {
    let binary = read_script("install-binary.sh");
    assert!(
        binary.contains("/api/panel-identity"),
        "就绪探测应打 /api/panel-identity（这是判断形态与归属的唯一稳定接口）"
    );
    assert!(
        binary.contains(r#""embedded":true"#),
        "就绪探测应确认 embedded 为 true，而不是只看 8080 是否有人应答"
    );
}

/// `--dry-run` 在没有 systemd 的机器（容器 / CI）上必须能出计划。
///
/// 预演的意义正是"先看清要装到哪，再决定要不要装"；容器里没有 user manager
/// 是常态，硬拦截会让 dry-run 在最需要它的环境里失效。
#[test]
fn dry_run_survives_a_host_without_systemd() {
    let binary = read_script("install-binary.sh");
    let start = binary
        .find("systemd user manager is unavailable")
        .expect("install-binary.sh should check for a user manager");
    // 往回找最近的 `if`，确认同一条分支里带 DRY_RUN 降级。
    let window = &binary[start.saturating_sub(600)..start];
    assert!(
        window.contains("DRY_RUN"),
        "user manager 缺失的分支应在 --dry-run 下降级为告警而不是退出"
    );
}

/// `scripts/bump-version.sh` 是版本号的唯一写入口，它必须：
/// 改到 `[workspace.package] version`、带 `--check` / `--print` 两个只读模式、
/// 且刷新 Cargo.lock（否则 `cargo build --locked` 全线失败）。
#[test]
fn version_bump_script_is_wired() {
    let bump = read_script("bump-version.sh");
    let root = read_file(&repo_root().join("Cargo.toml"));

    assert!(
        bump.contains("workspace\\.package"),
        "bump-version.sh 应只改 [workspace.package] 段的版本"
    );
    assert!(
        bump.contains("--check"),
        "bump-version.sh 应支持 --check（release.yml 的构建前门禁用它）"
    );
    assert!(
        bump.contains("--print"),
        "bump-version.sh 应支持 --print（读取唯一真值）"
    );
    assert!(
        bump.contains("cargo metadata"),
        "bump-version.sh 应刷新 Cargo.lock（否则 --locked 构建失败）"
    );

    // 成员 crate 必须是"继承"而不是各写一份，否则 bump 只改一处会漏。
    let inherited = std::fs::read_to_string(repo_root().join("source/modules/core/Cargo.toml"))
        .expect("read core Cargo.toml");
    assert!(
        inherited.contains("version.workspace = true"),
        "member crates should inherit the workspace version, not hardcode it"
    );
    assert!(
        root.contains("version = "),
        "root Cargo.toml should declare the workspace version"
    );
}

/// `--mirror` 必须同时支持两种常见写法。
///
/// 2026-11 在 ubuntu:24.04 容器里实测踩到：`--mirror https://ghfast.top/https://github.com`
/// （ghfast / gh-proxy 的标准用法）被无条件剥掉 `https://` 再拼，拼出了
/// `https://ghfast.top/https://github.com/github.com/EchoLab-Auto/...`——域名重复，
/// 服务器回 404。而 help 文本与文档给的例子恰好就是这个写法。
#[test]
fn mirror_prefix_supports_both_written_forms() {
    let binary = read_script("install-binary.sh");
    assert!(
        binary.contains(r#"if [[ "$prefix" == *github.com ]]"#),
        "mirror_url 应区分「含域名的前缀」与「纯代理域」两种写法"
    );
    assert!(
        binary.contains(r#"printf '%s/%s\n' "$prefix" "$url""#),
        "纯代理域的写法应把完整 URL 接在前缀后面"
    );
    assert!(
        !binary.contains(r#"printf '%s/%s\n' "${DOWNLOAD_MIRROR%/}" "${url#https://}""#),
        "不应无条件剥掉 https://（含域名的前缀会因此拼出重复的 github.com）"
    );
}

/// 侧车下载失败不能报成"产物缺侧车"。
///
/// 2026-11 容器实测：镜像链路抖动（`Connection reset`）让 108 字节的 `.sha256`
/// 下载失败，而当时的报错是"no checksum sidecar found … expected next to the
/// artifact"——用户会去怀疑发布流程漏发文件，实际是网络。原来的 `2>/dev/null`
/// 把 curl 的真实错误吞掉了。
#[test]
fn checksum_download_failure_is_not_reported_as_missing() {
    let binary = read_script("install-binary.sh");
    assert!(
        binary.contains("checksum_failure=$(download"),
        "侧车下载应保留 curl 的错误输出（否则网络失败会被误报成产物缺失）"
    );
    assert!(
        binary.contains("this is a network failure, not a missing artifact"),
        "报错应明确区分「网络失败」与「产物没发侧车」"
    );
    assert!(
        !binary.contains(r#"if download "$checksum_url" "$WORK_DIR/artifact.sha256" 2>/dev/null"#),
        "不应再用 2>/dev/null 吞掉侧车下载的错误"
    );
}

/// `download` 必须有总超时，而不只是连接超时。
///
/// `--connect-timeout` 只管到"连接建立"；连上之后读数据慢会无限等——下载 15MB
/// 产物时表现为"看起来在下载、实际永不到头"。
#[test]
fn downloads_have_a_total_timeout() {
    let binary = read_script("install-binary.sh");
    assert!(
        binary.contains("--max-time"),
        "download 应设总超时（--connect-timeout 不覆盖读数据阶段）"
    );
}

/// 二进制安装链路必须带**外部模块二进制**，且打包路径与解包路径一致。
///
/// 三方必须一致：`release.yml` 打包路径、`install-binary.sh` 解包路径、
/// `modules.toml` 启用行指向的路径。任何一处不同，得到的都是"装完少了
/// 一个模块"，而那时离发布已经很远。
///
/// 焦点只在二进制链路：**源码安装刻意不带外部模块**（只带 Core 本体），
/// 那条决定与理由见 `install_script_flag.rs::source_install_ships_only_the_core_binary`。
#[test]
fn external_module_artifact_paths_agree() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    let binary = read_script("install-binary.sh");

    // 1) release.yml：构建 + 放进 stage/modules/<包名>/<包名> + 打进 tar
    assert!(
        workflow.contains("-p agent-module-checklist"),
        "release.yml 应构建外部模块二进制（否则产物里没有它）"
    );
    assert!(
        workflow.contains("stage/modules/agent-module-checklist/agent-module-checklist"),
        "release.yml 应把模块二进制放到 stage/modules/<包名>/<包名>"
    );
    assert!(
        workflow.contains("systemd modules manifest.json"),
        "release.yml 的 tar 应包含 modules/ 目录"
    );

    // 2) install-binary.sh：解包后装到 $MODULES_DIR/<包名>/<包名>，并列进必需项
    assert!(
        binary.contains("$MODULES_DIR/$module_name"),
        "install-binary.sh 应把模块装到 $MODULES_DIR/<包名>"
    );
    assert!(
        binary.contains("skills modules"),
        "install-binary.sh 应把 modules/ 列为产物必需项（缺了要报错，不能装完少功能）"
    );

    // 3) 落点约定与源码安装的 `modules/<包名>/<包名>` 相同——源码安装虽然
    //    不装外部模块，但它的本仓模块循环仍用同一套目录约定，两处路径一致
    //    才能让用户手工往 `modules/` 放二进制时不必区分安装方式。
    let install = read_script("install.sh");
    assert!(
        install.contains(r#"module_dir="$MODULES_DIR/$_module""#),
        "install.sh 与 install-binary.sh 的模块落点必须一致"
    );

    // 4) 二进制安装器必须知道该启用哪一个（写 modules.toml 的行）
    assert!(
        binary.contains("\"agent-module-checklist\""),
        "install-binary.sh 应知道模块名，才能写 modules.toml 的启用行"
    );
}
