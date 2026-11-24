//! Guard: 统一安装器（`scripts/install.sh`）的**两种安装模式**与 release 产物契约。
//!
//! 背景（2026-11 合并）：二进制安装原本是独立脚本 `scripts/install-binary.sh`。
//! 契约定型后合并成单一入口 `install.sh --mode binary|source`（**默认 binary**），
//! 理由是：
//!
//! - 两条路共享布局（`derive_paths` / launcher / unit 模板 / core.toml 模板 /
//!   技能分层），共享的东西写两遍必然漂移；
//! - 用户只需记一个脚本与一个开关，不必先判断"这台机器要不要装编译链"再决定
//!   curl 哪个地址；
//! - 模式差异被收敛到少数几个函数（`acquire_artifact` / 依赖+构建段 /
//!   `install_binary_extras` / `install_source_extras` / 收尾摘要），其余路径
//!   完全相同，于是 `scripts/uninstall.sh` 仍只认一套布局。
//!
//! 这里守的正是合并后最容易回归的几件事：
//!
//! - 布局只能有**一份** `derive_paths`（出现第二份 = 两种模式开始漂移）；
//! - unit 模板只有一份，且每个 `@PLACEHOLDER@` 都被渲染；
//! - binary 模式**不装** updater 单元/脚本，source 模式**不写** modules.toml；
//! - release tarball 的目录结构与安装器的校验项逐项对上。
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

/// `needle` 之前最近一条 `if [[ "$INSTALL_MODE" == <mode> ]]` 的 mode。
///
/// 用来证明"某件事只在某个模式下发生"，而不必把整段流程重写成可调用的函数。
fn enclosing_mode_guard(script: &str, needle: &str) -> Option<String> {
    let at = script.find(needle)?;
    let before = &script[..at];
    let marker = r#"if [[ "$INSTALL_MODE" == "#;
    let idx = before.rfind(marker)?;
    let rest = &before[idx + marker.len()..];
    let end = rest.find(']')?;
    Some(rest[..end].trim().trim_matches('"').to_string())
}

fn occurrences(script: &str, needle: &str) -> usize {
    script.matches(needle).count()
}

/// 以 `signature` 开头的**行**数（定义次数）。
///
/// 不能用子串计数：`mirror_url() {` 是 `github_mirror_url() {` 的后缀，
/// 子串计数会把一份实现数成两份。
fn definitions(script: &str, signature: &str) -> usize {
    script
        .lines()
        .filter(|line| line.trim_start().starts_with(signature))
        .count()
}

/// `--mode` 是唯一入口开关，默认 **binary**，且取值 fail-closed。
///
/// 合并后的第一契约就是这条：新机器上什么都不带跑 `install.sh`，得到的是二进制
/// 安装（不需要编译链）；要源码构建必须显式 `--mode source` 或 `--source`。
#[test]
fn install_mode_flag_selects_the_shape() {
    let script = read_script("install.sh");

    // 解析：三段式写法 + 两种别名。
    //
    // 断言必须连**分支体**一起看：顶部预扫描（bootstrap 之前要判断要不要
    // clone）也写着 `--binary)`，只匹配目录会漏掉"主解析里少了一个 case"
    // 这种回归——2026-11 合并时真踩到过（`--binary` 被预扫描兜住、主解析
    // 里其实缺了，于是 `--binary` 直接报 unknown option）。
    for (arm, body) in [
        ("        --mode)\n", "INSTALL_MODE=${2:-}"),
        ("        --mode=*)\n", "INSTALL_MODE=${1#--mode=}"),
        ("        --binary)\n", "INSTALL_MODE=binary"),
        ("        --source)\n", "INSTALL_MODE=source"),
    ] {
        assert!(
            script.contains(&format!("{arm}            {body}")),
            "install.sh 主解析应把 {arm} 映射到 `{body}`（--mode <binary|source> \
             与 --binary/--source 别名）"
        );
    }

    // 取值校验：拼错时 fail-closed，而不是静默退回默认。
    assert!(
        script.contains("    binary|source) ;;"),
        "--mode 取值必须校验（拼错时 fail-closed）"
    );
    assert!(
        script.contains("--mode must be 'binary' or 'source'"),
        "非法 --mode 应给出明确报错"
    );

    // 默认是 binary——这是本次合并的决定（新机器不用装编译链）。
    assert!(
        script.contains(r#"INSTALL_MODE=${INSTALL_MODE:-binary}"#),
        "install.sh 的默认安装模式必须是 binary"
    );

    // 依赖自动安装只属于源码模式；二进制模式拿到 --no-deps 只告警。
    assert!(
        script.contains("--no-deps has no effect in binary mode"),
        "binary 模式下 --no-deps 应显式告警（而不是静默无视）"
    );
}

/// `derive_paths` 是**唯一**的布局实现，且关键路径一个不缺。
///
/// 合并前这条测试做的是"两条脚本的 derive_paths 逐字比对"。合并后同样的漂移
/// 只会以另一种形式回来：有人为了 binary 模式的需要，在别处再写一份路径推导
/// （或就地 `PREFIX=...` 覆盖）。所以这里改成数定义次数 + 钉住落点。
#[test]
fn installer_has_a_single_path_layout() {
    let script = read_script("install.sh");

    assert_eq!(
        definitions(&script, "derive_paths() {"),
        1,
        "derive_paths 只能有一份实现：两种安装模式共用同一套布局，出现第二份=\
         开始漂移（uninstall.sh 只能删掉其中一种装法）"
    );

    let body = function_body(&script, "derive_paths() {");
    for assignment in [
        r#"PREFIX=${ECHO_PREFIX:-"$home/.local"}"#,
        r#"CONFIG_DIR="$CONFIG_HOME/echo-agent-core""#,
        r#"DATA_DIR="$DATA_HOME/echo-agent-core""#,
        r#"STATE_DIR="$STATE_HOME/echo-agent-core""#,
        r#"SOURCE_DIR="$DATA_DIR/source""#,
        r#"LIBEXEC_DIR="$PREFIX/libexec/echo-agent-core""#,
        r#"MODULES_DIR="$LIBEXEC_DIR/modules""#,
        r#"BIN_DIR="$PREFIX/bin""#,
        r#"CORE_CONFIG="$CONFIG_DIR/core.toml""#,
        r#"MODULES_CONFIG="$CONFIG_DIR/modules.toml""#,
        r#"STATUS_FILE="$STATE_DIR/update-status""#,
        r#"PANEL_WEB_DIR="$LIBEXEC_DIR/web""#,
    ] {
        assert!(
            body.contains(assignment),
            "derive_paths 缺少/改动了路径：\n  {assignment}\n\
             两种模式必须落出同一套布局，否则 uninstall.sh 只能删掉其中一种。"
        );
    }

    // 除布局外，这些"装一次就会出现两份"的基础设施也只能有一份实现：
    // 复制一份到模式分支里 = 下次改一处漏一处。
    for signature in [
        "render_unit() {",
        "ensure_service_linger() {",
        "snapshot_file() {",
        "restore_file() {",
        "install_rollback() {",
        "mirror_url() {",
        "install_one_module() {",
    ] {
        assert_eq!(
            definitions(&script, signature),
            1,
            "{signature} 只能定义一次（两种模式共用同一份实现）"
        );
    }

    // launcher 也是共用的：它在两种模式下必须完全一致。
    assert_eq!(
        occurrences(&script, r#"exec "$LIBEXEC_DIR/echo-agent-core-bin" --config "$CORE_CONFIG""#),
        1,
        "launcher 只应生成一次（两种模式同一份）"
    );
}

/// unit 模板里的每个 `@PLACEHOLDER@` 都必须被渲染。
///
/// 漏渲染的症状极隐蔽：单元文件里留着字面量 `@STATUS_FILE@`，systemd 能正常
/// 加载（它只是个字符串值），直到自更新/状态查询去碰那个路径才炸。
#[test]
fn every_unit_placeholder_is_rendered() {
    let template = read_unit_template();
    let script = read_script("install.sh");

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
        // `@SCOPE_LINES@` 是**多行**占位符：sed 的替换串里嵌换行不可读，安装器
        // 用 `awk` 整行替换（`$0 == "@SCOPE_LINES@"`）而不是 `s|…|`。
        // 两种形态都算"已渲染"，但必须恰好命中一种。
        let single_line = script.contains(&format!("s|{placeholder}|"));
        let whole_line = script.contains(&format!("$0 == \"{placeholder}\""));
        assert!(
            single_line || whole_line,
            "install.sh 没有渲染 unit 占位符 {placeholder}；\
             装出来的单元会带着字面量占位符跑。"
        );
    }

    // 两种模式必须复用**同一份**模板（不另存一份 -binary 变体）。
    assert!(
        !repo_root()
            .join("packaging/systemd/echo-agent-core-binary.service.in")
            .exists(),
        "不应新增 echo-agent-core-binary.service.in：unit 模板只该有一份，\
         两份必然漂移（漂移后果是某种装法缺了某条 Restart/加固策略）。"
    );
    assert!(
        script.contains("packaging/systemd/echo-agent-core.service.in"),
        "源码模式应从仓库里的 packaging/systemd/echo-agent-core.service.in 渲染单元"
    );
    assert!(
        script.contains("systemd/echo-agent-core.service.in"),
        "二进制模式应从产物里的 systemd/echo-agent-core.service.in 渲染单元"
    );
    // 只有 `@SOURCE_DIR@` 按模式取值（WorkingDirectory）：源码模式指向受管
    // 检出，二进制模式指向数据目录。
    assert!(
        script.contains(r#"UNIT_SOURCE_DIR="$DATA_DIR""#)
            && script.contains(r#"UNIT_SOURCE_DIR="$SOURCE_DIR""#),
        "@SOURCE_DIR@ 应只在两种模式间取值不同（WorkingDirectory），其余占位符共用"
    );
}

/// binary 模式**不装** updater；source 模式**装**。
///
/// 两侧缺一都会留下一个"点了必然失败"的入口：没有源码检出，updater 里的
/// `cargo build` 无从执行；反过来 source 模式不装 updater 就等于关掉了自更新。
#[test]
fn self_update_is_split_correctly_between_modes() {
    let script = read_script("install.sh");

    let modules_config = function_body(&script, "write_modules_config() {");
    assert!(
        modules_config.contains(r#"id = "echo-agent.selfupdate""#),
        "binary 模式应在 modules.toml 里写 echo-agent.selfupdate 条目"
    );
    assert!(
        modules_config.contains("enabled = false"),
        "binary 模式应把自更新显式关掉"
    );

    let remove_stale = function_body(&script, "remove_stale_updater_unit() {");
    assert!(
        remove_stale.contains("echo-agent-core-update.service"),
        "binary 模式应清理残留的 updater 单元（此前可能是源码安装）"
    );

    let binary_extras = function_body(&script, "install_binary_extras() {");
    assert!(
        binary_extras.contains(r#"write_modules_config "$INSTALL_PANEL""#),
        "binary 专属步骤应调用 write_modules_config"
    );
    assert!(
        binary_extras.contains("remove_stale_updater_unit"),
        "binary 专属步骤应清理残留 updater 单元"
    );
    assert!(
        !binary_extras.contains("scripts/update.sh"),
        "binary 模式不应安装 update.sh 更新脚本（它依赖源码检出）"
    );

    let source_extras = function_body(&script, "install_source_extras() {");
    assert!(
        source_extras.contains(r#"install -m 755 "$PROJECT_ROOT/scripts/update.sh""#),
        "source 模式应安装 update.sh（自更新在源码检出上跑 cargo build）"
    );
    assert!(
        !source_extras.contains("write_modules_config"),
        "source 模式不得改写 modules.toml（那是用户手写的计划文件）"
    );

    // update.sh 的安装点全脚本只有一处，且必须落在源码侧函数里。
    assert_eq!(
        occurrences(&script, r#"install -m 755 "$PROJECT_ROOT/scripts/update.sh""#),
        1,
        "update.sh 应只在一处安装（否则两种模式会各自装一份、口径分叉）"
    );

    // 分派：两半各自被一个模式守卫调用。
    assert!(
        script.contains("    INSTALL_PHASE=modules-config\n    install_binary_extras\nelse"),
        "binary 专属步骤应只在 binary 分支里执行"
    );
    assert!(
        script.contains("    INSTALL_PHASE=updater\n    install_source_extras"),
        "源码专属步骤应只在 source 分支里执行"
    );
}

/// binary 模式不得渲染 updater 单元；source 模式必须渲染。
#[test]
fn updater_unit_is_rendered_only_for_source_installs() {
    let script = read_script("install.sh");
    assert_eq!(
        occurrences(&script, "echo-agent-core-update.service.in"),
        1,
        "updater 单元模板只应被引用一次（在源码模式分支里）"
    );
    assert_eq!(
        enclosing_mode_guard(&script, "echo-agent-core-update.service.in").as_deref(),
        Some("source"),
        "updater 单元只应在 --mode source 下渲染：binary 模式没有源码检出，\
         装出来的 updater 一跑就炸"
    );
}

/// source 模式**不得**创建或改写 modules.toml——回滚也不例外。
///
/// 反过来是最容易引入的回归：为了"统一",把 modules.toml 快照/还原也做进共用
/// 回滚函数里。那样一次失败的源码安装会删掉用户手写的计划文件。
#[test]
fn source_mode_never_touches_modules_config() {
    let script = read_script("install.sh");

    assert_eq!(
        occurrences(&script, r#"write_modules_config "$INSTALL_PANEL""#),
        1,
        "modules.toml 的写入点只应有一处"
    );
    assert_eq!(
        enclosing_mode_guard(&script, r#"write_modules_config "$INSTALL_PANEL""#).as_deref(),
        Some("binary"),
        "modules.toml 只应在 binary 分支里写"
    );

    // 回滚里的还原同样受模式守卫限制。
    assert_eq!(
        occurrences(&script, r#"restore_file "$MODULES_CONFIG""#),
        1,
        "modules.toml 的还原点只应有一处"
    );
    assert_eq!(
        enclosing_mode_guard(&script, r#"restore_file "$MODULES_CONFIG""#).as_deref(),
        Some("binary"),
        "modules.toml 的还原必须只在 binary 模式里发生：源码模式从不创建它，\
         还原就等于替用户删掉手写文件"
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
    let script = read_script("install.sh");
    assert!(
        script.contains(r#"PANEL_WEB_DIR="$LIBEXEC_DIR/web""#),
        "Panel 前端应装到 Core 二进制同级 web/（内嵌模块候选列表的第一命中项）"
    );
    // 内嵌形态必须显式启用：模块默认不挂载、不监听。
    assert!(
        script.contains(r#"id = "echo-agent.panel.web""#),
        "binary 模式应在 modules.toml 里启用内嵌 Panel"
    );
    // 端口被占时要给人一条明确的出路：内嵌形态靠 bind 换端口，而不是停掉别的服务。
    assert!(
        script.contains(r#"bind = "127.0.0.1:8081""#),
        "modules.toml 模板应提示用 [module.config] bind 改端口"
    );
    // Panel 只属于二进制模式：源码模式没有前端产物，也不该写 modules.toml。
    assert_eq!(
        enclosing_mode_guard(&script, r#"cp -a "$UNPACK_DIR/web/." "$web_staging/""#).as_deref(),
        Some("binary"),
        "Panel 前端只应在 binary 分支里落盘"
    );
}

/// release 产物的目录结构是安装器的输入契约——两边必须逐项对上。
#[test]
fn release_artifact_layout_matches_the_installer() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    let script = read_script("install.sh");

    // 安装器要求的必需项（顺序无关，存在即可）。
    for required in [
        "bin/echo-agent-core-bin",
        "web/index.html",
        "manifest.json",
        "systemd/echo-agent-core.service.in",
        "config/echo-agent-core.toml",
        "skills",
    ] {
        assert!(script.contains(required), "安装器应校验产物里的 {required}");
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
        "release.yml 应生成 .sha256 侧车文件（安装器默认要求）"
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
/// 2026-11 实测的坑：回滚快照里除了文件还有 `web/` 目录，`rm -f <目录>`
/// 报 "Is a directory" 并返回非零；此刻 `set -e` 仍然生效、事务守卫已解除，
/// 于是**装成功了却以退出码 1 结束**，且不回滚——用户看到失败、脚本认为没事。
///
/// 合并后快照集合是两种模式的**并集**（多快照几个不存在的文件不花代价），
/// 所以这里同时盯 web/ 与 modules/ 两个目录。
#[test]
fn rollback_cleanup_skips_directories() {
    let script = read_script("install.sh");
    // 成功路径是**最后一次** `INSTALL_TRANSACTION=0`（前面回滚函数里也有一次），
    // 其后紧跟清理循环——两者之间允许有注释，所以分段找。
    let flag = script
        .rfind("INSTALL_TRANSACTION=0")
        .expect("install.sh should clear the rollback dir after success");
    let start = flag
        + script[flag..]
            .find("for rollback_file in")
            .expect("success path should iterate the rollback dir");
    let rest = &script[start..];
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
    assert!(
        cleanup.contains(r#"rm -rf -- "$ROLLBACK_DIR/web" "$ROLLBACK_DIR/modules""#),
        "modules 快照目录应与 web 一起 rm -rf\n{cleanup}"
    );
}

/// Panel 就绪探测必须能区分"我们的内嵌面板"和"恰好占用 8080 的其它服务"。
///
/// 只探 `GET /` 或只判端口会误报：本机跑着独立 Panel（开发实例）时，
/// 装到临时 prefix 的这次安装照样打印 "responding"。
#[test]
fn panel_readiness_probe_checks_embedded_identity() {
    let script = read_script("install.sh");
    assert!(
        script.contains("/api/panel-identity"),
        "就绪探测应打 /api/panel-identity（这是判断形态与归属的唯一稳定接口）"
    );
    assert!(
        script.contains(r#""embedded":true"#),
        "就绪探测应确认 embedded 为 true，而不是只看 8080 是否有人应答"
    );
}

/// `--dry-run` 在没有 systemd 的机器（容器 / CI）上必须能出计划。
///
/// 预演的意义正是"先看清要装到哪，再决定要不要装"；容器里没有 user manager
/// 是常态，硬拦截会让 dry-run 在最需要它的环境里失效。
#[test]
fn dry_run_survives_a_host_without_systemd() {
    let script = read_script("install.sh");
    let start = script
        .find("systemd user manager is unavailable")
        .expect("install.sh should check for a user manager");
    // 往回找最近的窗口，确认同一条分支里带 DRY_RUN 降级。
    let window = &script[start.saturating_sub(600)..start];
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
    let script = read_script("install.sh");
    assert!(
        script.contains(r#"if [[ "$prefix" == *github.com ]]"#),
        "mirror_url 应区分「含域名的前缀」与「纯代理域」两种写法"
    );
    assert!(
        script.contains(r#"printf '%s/%s\n' "$prefix" "$url""#),
        "纯代理域的写法应把完整 URL 接在前缀后面"
    );
    assert!(
        !script.contains(r#"printf '%s/%s\n' "${DOWNLOAD_MIRROR%/}" "${url#https://}""#),
        "不应无条件剥掉 https://（含域名的前缀会因此拼出重复的 github.com）"
    );
    // `--cn-mirror` 在二进制模式下顺带把产物下载指向同一个 GitHub 代理：
    // 两种模式只记一个开关，不必分别记 --mirror。
    assert!(
        script.contains(r#"DOWNLOAD_MIRROR="${GITHUB_PROXY%/}/https://github.com""#),
        "--cn-mirror 应同时覆盖产物下载前缀（否则二进制模式还要单独记 --mirror）"
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
    let script = read_script("install.sh");
    assert!(
        script.contains("checksum_failure=$(download"),
        "侧车下载应保留 curl 的错误输出（否则网络失败会被误报成产物缺失）"
    );
    assert!(
        script.contains("this is a network failure, not a missing artifact"),
        "报错应明确区分「网络失败」与「产物没发侧车」"
    );
    assert!(
        !script.contains(r#"if download "$checksum_url" "$WORK_DIR/artifact.sha256" 2>/dev/null"#),
        "不应再用 2>/dev/null 吞掉侧车下载的错误"
    );
}

/// `download` 必须有总超时，而不只是连接超时。
///
/// `--connect-timeout` 只管到"连接建立"；连上之后读数据慢会无限等——下载 15MB
/// 产物时表现为"看起来在下载、实际永不到头"。
#[test]
fn downloads_have_a_total_timeout() {
    let script = read_script("install.sh");
    assert!(
        script.contains("--max-time"),
        "download 应设总超时（--connect-timeout 不覆盖读数据阶段）"
    );
}

/// 二进制链路必须带**外部模块二进制**，且打包路径与解包路径一致。
///
/// 三方必须一致：`release.yml` 打包路径、安装器解包后的落点、以及用户要在
/// `modules.toml` 里照抄的注释示例路径。任何一处不同，得到的都是"装完少了
/// （或指错了）一个模块"，而那时离发布已经很远。
///
/// 焦点只在二进制链路：**源码安装刻意不带外部模块**（只带 Core 本体），
/// 那条决定与理由见 `install_script_flag.rs::source_install_ships_only_the_core_binary`。
#[test]
fn external_module_artifact_paths_agree() {
    let workflow = read_file(&repo_root().join(".github/workflows/release.yml"));
    let script = read_script("install.sh");

    // 1) release.yml：产物落进 stage/modules/<包名>/<包名> 并打进 tar
    assert!(
        workflow.contains("stage/modules/agent-module-checklist/agent-module-checklist"),
        "release.yml 应把模块二进制放到 stage/modules/<包名>/<包名>"
    );
    assert!(
        workflow.contains("systemd modules manifest.json"),
        "release.yml 的 tar 应包含 modules/ 目录"
    );

    // 2) 安装器：解包后装到 $MODULES_DIR/<包名>/<包名>，并列进产物必需项
    let module_helper = function_body(&script, "install_one_module() {");
    assert!(
        module_helper.contains(r#"module_dir="$MODULES_DIR/$module_name""#),
        "模块落点必须是 modules/<包名>/<包名>（两种模式共用一个落盘函数）"
    );
    assert!(
        script.contains("skills modules"),
        "安装器应把 modules/ 列为产物必需项（缺了要报错，不能装完少功能）"
    );

    // 3) 落点约定与源码安装的本仓模块循环相同——用户手工往 modules/ 放二进制
    //    时不必区分安装方式。
    //
    // MODULE_BINARIES 在脚本里出现三次循环（构建参数 / 事务快照 / 落盘），
    // 这里只要求其中一个循环经同一落盘函数——落盘点唯一是关键，位置不关键。
    let landed = script
        .split("for _module in \"${MODULE_BINARIES[@]}\"; do")
        .skip(1)
        .any(|chunk| {
            chunk.contains(r#"install_one_module "$_module" "$SOURCE_DIR/target/release/$_module""#)
        });
    assert!(
        landed,
        "源码模式的本仓模块也必须经同一落盘函数（modules/<包名>/<包名>）"
    );

    // 4) 注释示例里的路径与真实落点同源（否则用户照着写会指向不存在的文件）
    assert!(
        script.contains("$MODULES_DIR/agent-module-checklist/agent-module-checklist"),
        "modules.toml 注释示例应指向 modules/<包名>/<包名>"
    );
}
