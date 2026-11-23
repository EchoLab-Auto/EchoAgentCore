//! Guard: `--overwrite-config`（install/reinstall 的配置重置开关）与脚本
//! 语法完整性。
//!
//! 背景（2026-10）：重装默认保留 core.toml；需要以最新模板重置配置时用
//! `reinstall.sh --overwrite-config`（旧文件备份为 core.toml.bak-<epoch>；
//! `management_access_token` 沿用旧值，Panel/对端无需换令牌）。脚本在 CI
//! 中没有执行覆盖——这里用 `bash -n` 做语法门禁、并对 flag 接线做存在性
//! 守护（重命名/删码后文档与帮助不脱节）。

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

fn read_script(name: &str) -> String {
    let path = repo_root().join("scripts").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// install.sh / reinstall.sh / uninstall.sh / update.sh 必须通过 `bash -n`。
#[test]
fn install_scripts_are_bash_syntax_clean() {
    for name in ["install.sh", "reinstall.sh", "uninstall.sh", "update.sh"] {
        let path = repo_root().join("scripts").join(name);
        let status = Command::new("bash")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap_or_else(|e| panic!("run bash -n {}: {e}", path.display()));
        assert!(status.success(), "bash -n failed for {}", path.display());
    }
}

/// `--overwrite-config` 接线：install.sh 解析并实现（备份 + 令牌沿用）；
/// reinstall.sh 帮助说明并通过 `"$@"` 原样转发。
#[test]
fn overwrite_config_flag_is_wired() {
    let install = read_script("install.sh");
    assert!(
        install.contains("--overwrite-config"),
        "install.sh 应接受 --overwrite-config"
    );
    assert!(
        install.contains("OVERWRITE_CONFIG=1"),
        "install.sh 应在解析到该 flag 时置位 OVERWRITE_CONFIG"
    );
    assert!(
        install.contains("(pass --overwrite-config to replace it)"),
        "默认保留路径应提示该 flag（可发现性）"
    );
    assert!(
        install.contains(".bak-$(date +%s)"),
        "重写前应把旧配置备份为 .bak-<epoch>"
    );
    assert!(
        install.contains("Kept existing management_access_token"),
        "重写应沿用既有 management_access_token"
    );

    let reinstall = read_script("reinstall.sh");
    assert!(
        reinstall.contains("--overwrite-config"),
        "reinstall.sh 帮助应说明该 flag（原样转发给 install.sh）"
    );
    assert!(
        reinstall.contains("-h|--help"),
        "reinstall.sh 应在执行卸载前拦截帮助（帮助/预演不产生系统改动）"
    );
}

/// 从脚本里抠出 `MODULE_BINARIES=(...)` 的条目。
fn module_binaries(script: &str) -> Vec<String> {
    let start = script
        .find("MODULE_BINARIES=(")
        .unwrap_or_else(|| panic!("script should declare MODULE_BINARIES"));
    let rest = &script[start + "MODULE_BINARIES=(".len()..];
    let end = rest
        .find(')')
        .unwrap_or_else(|| panic!("unterminated MODULE_BINARIES"));
    rest[..end].split_whitespace().map(str::to_string).collect()
}

/// 仓库里**可安装**的进程外模块 crate：`source/modules/*/Cargo.toml` 中声明了
/// `[[bin]] path = "src/main.rs"` 的包；`core` 是宿主二进制，显式排除。
///
/// `src/bin/*.rs` 的 bin（`echo-module-test-peer`）是测试夹具，装机没有意义，
/// 因此判定看 bin 的 path 而不是"有没有 `[[bin]]`"。
fn installable_module_packages() -> Vec<String> {
    let module_root = repo_root().join("source").join("modules");
    let mut packages: Vec<String> = std::fs::read_dir(&module_root)
        .unwrap_or_else(|e| panic!("read {}: {e}", module_root.display()))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| entry.file_name() != "core")
        .filter_map(|entry| {
            let manifest = entry.path().join("Cargo.toml");
            let text = std::fs::read_to_string(&manifest).ok()?;
            // 逐行粗解析：`[[bin]]` 之后出现的 `path = "src/main.rs"` 才算
            // 可安装的主二进制（避免引入 toml 依赖只为一个断言）。
            let mut in_bin = false;
            let mut has_main_bin = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with("[[") {
                    in_bin = line == "[[bin]]";
                    continue;
                }
                if in_bin && line.replace(' ', "") == "path=\"src/main.rs\"" {
                    has_main_bin = true;
                }
            }
            if !has_main_bin {
                return None;
            }
            text.lines()
                .find_map(|line| line.strip_prefix("name = "))
                .map(|value| value.trim_matches('"').to_string())
        })
        .collect();
    packages.sort();
    packages
}

/// 安装器与仓库实际一致：install.sh / update.sh 的 `MODULE_BINARIES` 必须
/// 等于「本仓可安装的进程外模块 crate」集合，且两个脚本彼此一致。
///
/// 为什么要有这条：自更新与首装装的是**同一套**模块二进制（Core 与模块必须
/// 配套升级）。新增一个模块 crate 却忘了写进清单 → 装机后 `modules.toml`
/// 指向一个不存在的路径；反过来删了 crate 却留着清单 → 每次 install 都
/// 报 "module binary missing after build"。
#[test]
fn module_binary_lists_match_the_workspace() {
    let mut expected = installable_module_packages();
    expected.sort();

    let install = read_script("install.sh");
    let update = read_script("update.sh");
    let mut from_install = module_binaries(&install);
    let mut from_update = module_binaries(&update);
    from_install.sort();
    from_update.sort();

    assert_eq!(
        from_install, expected,
        "install.sh 的 MODULE_BINARIES 与仓库可安装模块 crate 不一致"
    );
    assert_eq!(
        from_update, expected,
        "update.sh 的 MODULE_BINARIES 与仓库可安装模块 crate 不一致"
    );
}

/// **源码安装只带 Core 本体**：外部仓模块二进制不在这条链路上。
///
/// 决定与理由（2026-11-21，用户明确要求）：源码安装装出来的东西应当只有
/// "从这份源码构建的产物"。外部仓模块装成独立产物后，必须在 `modules.toml`
/// 里写启用行才会真的跑；而 `modules.toml` 是用户手写的计划文件，`install.sh`
/// / `update.sh` 从不创建或改写它。装了不写行 = 落一个永远不执行的二进制，
/// 白占几 MB，还让人误以为清单已经进程外运行。
///
/// 清单功能不受影响：它作为 Core 的 git 依赖编进二进制，走内置形态
/// （Core 不再认识这个模块：没有依赖、没有内置目录项，只有 modules.toml）。
/// 想要独立产物请用二进制安装（`install-binary.sh` 会写启用行）。
///
/// 这条测试是**反向**的：断言两个脚本都不再有这张表、也不再把外部模块放进
/// `cargo build -p`。将来若有人（包括我）顺手把表加回来，这里会立刻红。
#[test]
fn source_install_ships_only_the_core_binary() {
    for name in ["install.sh", "update.sh"] {
        let script = read_script(name);
        assert!(
            !script.contains("EXTERNAL_MODULE_BINARIES"),
            "{name} 不应再有 EXTERNAL_MODULE_BINARIES：源码安装只带 Core 本体。\
             外部模块二进制只随二进制安装（release.yml + install-binary.sh）落地。\
             清单在源码安装下走内置形态，功能不受影响。"
        );
        assert!(
            !script.contains("-p agent-module-checklist"),
            "{name} 不应把外部模块放进 `cargo build -p`：那会额外产出一个\
             永远不执行的独立二进制。Core 仍会把它作为 git 依赖编成库链进来，\
             内置清单照常工作。"
        );
    }
}

/// 外部仓模块二进制只属于**二进制安装**链路：release.yml 打包 +
/// `install-binary.sh` 解包。
///
/// 与上一条的分工：源码安装刻意不带它，所以"谁真的会用它"必须在这里钉死。
///
/// **2026-11-21 起两件事变了**（任务清单彻底外置）：
///
/// 1. 它不再在 Core 的依赖图里 —— 所以 release.yml 不能再用 `-p`，必须
///    单独克隆 + 构建。断言盯的是"独立构建"，不是某个 `-p` 字面量：
///    把依赖加回来反而会让这条设计失效。
/// 2. `install-binary.sh` 不再自动写启用行 —— 产物里带着二进制，但能力
///    默认关。断言反向盯这一点：出现 `enabled = true` 的启用行就红。
#[test]
fn binary_install_chain_ships_external_modules_without_enabling_them() {
    let workflow = std::fs::read_to_string(repo_root().join(".github/workflows/release.yml"))
        .expect("read release.yml");
    let binary = read_script("install-binary.sh");

    // 1) release.yml：独立拉取 + 独立 workspace 构建。
    assert!(
        workflow.contains("Build external modules"),
        "release.yml 应有独立的外部模块构建步骤（它不在 Core 的依赖图里）"
    );
    // 2026-11（A2）：提交来源从"clone 默认分支 HEAD"改成唯一锁定表
    // `packaging/external-modules.lock`（完整 40 位 SHA），与正式镜像同源。
    // 这条断言盯的是"仍然独立于 Core 的依赖图"，而不是某个克隆写法：
    // 绑具体命令会把下次改锁机制变成一次性改测试。
    assert!(
        workflow.contains("scripts/release/external-modules.sh")
            && workflow.contains("list-keys tarball"),
        "外部模块必须按锁定表逐个独立拉取/构建（Core 的依赖图里没有它们）"
    );
    assert!(
        workflow.contains("--manifest-path \"${src}/Cargo.toml\""),
        "外部模块用**它自己的** Cargo.toml / Cargo.lock 构建"
    );
    assert!(
        workflow.contains("stage/modules/agent-module-checklist/agent-module-checklist"),
        "release.yml 应把模块二进制放到 stage/modules/<包名>/<包名>"
    );
    assert!(
        !workflow.contains("-p agent-module-checklist"),
        "release.yml **不能**再 `-p` 它：那意味着它又回到了 Core 的依赖图里"
    );

    // 2) install-binary.sh：装二进制，但**不**自动启用。
    assert!(
        binary.contains("agent-module-checklist"),
        "install-binary.sh 仍应知道产物里的模块目录名（解包时校验）"
    );
    // 清单那一行必须是**注释掉的**（其他模块的启用行不受这条约束：
    // 内嵌 Panel 仍默认启用）。
    assert!(
        !binary.contains(r#"'id = "echo-agent.checklist"'"#),
        "install-binary.sh 不应再写未注释的清单启用行：默认不启用是这次的决定"
    );
    assert!(
        binary.contains(r#"'# id = "echo-agent.checklist"'"#),
        "移除自动启用后必须留下注释示例，否则用户不知道该怎么打开"
    );

    // 3) Core 的依赖表里刻意没有它。
    let manifest =
        std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        !manifest.contains("agent-module-checklist = {"),
        "agent-module-checklist 不该是 Core 的依赖：\"宿主完全不认识它\"是这次设计的目标"
    );
}

/// 模块目录约定接线：install.sh 建目录并把二进制装到 `modules/<包>/<包>`；
/// update.sh 做版本化替换 + 回滚副本，且失败时能还原。
#[test]
fn module_directory_install_and_rollback_are_wired() {
    let install = read_script("install.sh");
    assert!(
        install.contains(r#"MODULES_DIR="$LIBEXEC_DIR/modules""#),
        "install.sh 应以 libexec/modules 作为模块根"
    );
    assert!(
        install.contains(r#"install -d -m 755 "$module_dir""#),
        "install.sh 应按包名建子目录"
    );
    assert!(
        install.contains(r#"mv -f "$module_temporary" "$module_dir/$_module""#),
        "install.sh 应原子替换模块二进制"
    );
    // 路径是 `$MODULES_DIR/$_module/$_module`（`modules/<包名>/<包名>`）。
    // 曾经这里是 `$_modules`（复数，从未定义）——当时 `MODULE_BINARIES` 恒空、
    // 循环从不执行，`set -u` 也炸不出来，断言反而把这个笔误锁住了。表非空
    // 之后它会立刻让安装与回滚失败，所以这条断言必须盯住正确的那一份。
    assert!(
        install.contains(r#"snapshot_file "$MODULES_DIR/$_module/$_module""#),
        "install.sh 应把旧模块二进制纳入安装事务快照（路径 modules/<包名>/<包名>）"
    );

    let update = read_script("update.sh");
    assert!(
        update.contains(r#"module_rollback="$module_dir/$_module.rollback""#),
        "update.sh 应保留上一版模块二进制（.rollback）"
    );
    assert!(
        update.contains(r#"MODULE_ROLLBACKS["$_module"]=$module_rollback"#),
        "update.sh 应记录回滚副本路径"
    );
    assert!(
        update.contains("MODULES_REPLACED=1"),
        "update.sh 应标记模块已替换（供 finalize_failure 触发回滚）"
    );
    assert!(
        update.contains(r#"mv -f "${MODULE_ROLLBACKS[$_p]}" "$MODULES_DIR/$_p/$_p""#),
        "回滚应把模块二进制还原回上一版"
    );
}

/// 安装器必须开启 **linger**：user 作用域服务的生命周期前提。
///
/// 背景（2026-10-22，A100 节点实测两次）：unit 装成 `--user` 作用域、挂在
/// `default.target`，而 `default.target` 属于 `user@<uid>.service`。该用户
/// 管理器由第一个登录会话拉起、最后一个会话消失后十秒被 logind 停掉——
/// `user@` 一停，其下所有单元（含 Core）一并停止。于是：
///
/// - 运维 SSH 登出 3 秒后 Core 死（实测 23:20:00、03:12:35 两次）；
/// - 下次有人登录 3 秒后 Core 又活（重启时间与登录时间精确重合，
///   看起来像"远端自动重启"，实际是登录把它带起来的）；
/// - **机器重启后无人登录时 Core 根本不会起来**（更隐蔽的那一半）。
///
/// `Restart=always` 在这里救不了：unit 不是失败退出，是被上层正常停止，
/// 而 systemd 语义里显式 stop 之后不自动重启。安装器此前全文没有一处
/// `linger`，因此**每一台按默认路径装出来的机器都带这个缺陷**。
///
/// 这条测试守两件事：调用点存在，且失败路径会告警（而不是静默降级）。
#[test]
fn installer_enables_linger_for_the_user_unit() {
    let install = read_script("install.sh");
    assert!(
        install.contains("loginctl enable-linger"),
        "install.sh 必须调用 loginctl enable-linger：\n\
         缺它则 Core 在最后一个登录会话结束时被停止，且开机不自启"
    );
    assert!(
        install.contains("ensure_service_linger"),
        "应通过 ensure_service_linger 统一处理（定义 + 调用点各一处）"
    );
    // 调用点必须在使用它的单元 enable 之后、restart 之前——顺序反了会在
    // 服务已启动后才补 linger，掩盖"装完就掉"的窗口。
    let enable_at = install
        .find("\"${SYSTEMCTL[@]}\" enable echo-agent-core.service")
        .expect("install.sh 应 enable 单元");
    let linger_at = install
        .find("ensure_service_linger\n")
        .expect("install.sh 应调用 ensure_service_linger");
    assert!(
        linger_at > enable_at,
        "ensure_service_linger 应在单元 enable 之后调用（当前顺序反了）"
    );
    // 失败不得中断安装，但必须显式告警并给出修复命令。
    assert!(
        install.contains("loginctl enable-linger $user"),
        "失败告警里应给出可直接复制的修复命令"
    );
    assert!(
        install.contains("without linger it"),
        "失败告警应说明后果（否则用户会当作可忽略的噪音）"
    );
}

/// 卸载器与安装器在 **linger** 上必须对称：装时开、卸时**不擅自关**。
///
/// install.sh 现在会开 linger（见 `installer_enables_linger_for_the_user_unit`），
/// 于是卸载时产生一个诱人的"顺手清理"：把 linger 关回去。这是错的——
/// linger 是**用户级、机器级**开关，不是本服务的私有属性：
///
/// - 这台机器上可能还有别的 user 作用域服务（用户自己跑的守护进程）；
/// - 关掉 linger 会让它们在最后一个登录会话结束时静默死亡；
/// - 卸载器既不知道有没有这样的服务，也无法事后补偿。
///
/// 所以正确行为是：**默认保留并明确告知**，关必须走显式 `--disable-linger`。
/// 这条测试同时钉住 dry-run 的可见性（用户运行前就该看到 linger 会怎样）。
#[test]
fn uninstaller_reports_linger_and_only_disables_it_on_request() {
    let uninstall = read_script("uninstall.sh");

    // ① 默认不关：不能出现无条件的 disable-linger。
    assert!(
        uninstall.contains("--disable-linger"),
        "uninstall.sh 应提供 --disable-linger（显式开关）"
    );
    assert!(
        uninstall.contains("DISABLE_LINGER=0"),
        "默认必须是「不关」——DISABLE_LINGER 初始为 0"
    );
    // disable-linger 只能出现在 DISABLE_LINGER 分支里：
    // 数一遍，调用点必须被 `if ((DISABLE_LINGER))` 包住。
    let disable_at = uninstall
        .find("loginctl disable-linger")
        .expect("应支持关闭 linger");
    let guard_at = uninstall
        .find("if ((DISABLE_LINGER)); then")
        .expect("disable-linger 必须被 DISABLE_LINGER 守卫");
    assert!(
        guard_at < disable_at,
        "loginctl disable-linger 必须位于 DISABLE_LINGER 守卫之内"
    );

    // ② 不关的时候必须**说出来**，不能静默保留一个常驻用户管理器。
    assert!(
        uninstall.contains("Keeping linger for"),
        "默认保留 linger 时应明确告知（否则用户以为卸干净了）"
    );
    assert!(
        uninstall.contains("loginctl disable-linger $current_user"),
        "保留时应给出关闭方式（可发现性）"
    );

    // ③ dry-run 里也要显示 linger 的去向——这是「运行前就能看清」的契约。
    assert!(
        uninstall.contains("linger:"),
        "dry-run 计划里应有一行 linger 状态"
    );
    assert!(
        uninstall.contains("linger_note"),
        "linger 状态应经单一函数渲染（dry-run 与收尾报告口径一致）"
    );
}

/// 卸载器必须能清掉 **system 作用域** 的存量单元（服务器装法）。
///
/// install.sh 只写 user 作用域，但存量机器可能是手工或旧脚本装的
/// `/etc/systemd/system/echo-agent-core*.service`（多节点部署常见）。对此
/// 静默跳过是最坏的：二进制被删了，systemd 下次开机仍会去拉起它，表现成
/// "卸载了却还在崩溃重启"。所以这里要求：探测到就处理，无权处理就**说出
/// 该怎么处理**（打印可直接复制的命令）。
#[test]
fn uninstaller_handles_system_scope_units() {
    let uninstall = read_script("uninstall.sh");
    assert!(
        uninstall.contains("SYSTEM_UNIT_DIR=/etc/systemd/system"),
        "应探测 /etc/systemd/system"
    );
    assert!(
        uninstall.contains("system-scope units present but this script is not running as root"),
        "非 root 时应告警，而不是静默跳过"
    );
    assert!(
        uninstall.contains("sudo systemctl disable --now"),
        "非 root 时应给出可直接复制的清理命令"
    );
    // 有系统单元时，dry-run 要能看见（否则用户跑完才发现还有残留）。
    assert!(
        uninstall.contains("system:    "),
        "dry-run 计划里应有一行 system 作用域单元"
    );
}

/// `--scope` 必须是**一等参数**，而不是"user 装法 + 补丁"。
///
/// 背景（2026-10）：自更新链路两端都把 `--user` 写死——
/// `agent-module-self-update` 启动 updater 单元、`update.sh` 重启 Core。
/// 于是 system 作用域的老装法只能靠一个 PATH shim（假 `systemctl` 把 `--user`
/// 抹掉）硬凑，而 core 自身那次调用连 shim 都看不到（它不在 core 单元的 PATH
/// 里），**面板上的自更新按钮从来没成功过**。
///
/// 这条测试钉住三件事：
/// 1. 两条作用域都解析出正确的 systemctl 形态（不是一处 user、一处 system）；
/// 2. system 作用域写 `/etc/systemd/system`、挂 `multi-user.target`、带运行身份；
/// 3. 作用域经 `ECHO_SERVICE_SCOPE` 传给模块与 update.sh（唯一可信来源）。
#[test]
fn installer_supports_both_service_scopes() {
    let install = read_script("install.sh");

    // ① 作用域是一等参数，有两个合法取值。
    assert!(
        install.contains("--scope)") && install.contains("--scope=*)"),
        "install.sh 应解析 --scope <user|system> 与 --scope=system 两种写法"
    );
    assert!(
        install.contains("user|system) ;;"),
        "--scope 取值必须校验（拼错时 fail-closed，而不是静默退回默认）"
    );

    // ② 形态切换只发生在两处：单元目录与 systemctl 前缀。
    assert!(
        install.contains("SYSTEMD_DIR=/etc/systemd/system"),
        "system 作用域应写 /etc/systemd/system"
    );
    assert!(
        install.contains("SYSTEMCTL=(systemctl --user)")
            && install.contains("SYSTEMCTL=(systemctl)"),
        "两条作用域各有一套 systemctl 前缀"
    );
    // 所有操作都走数组：漏一处就会在另一条作用域里操作到错误的 manager。
    assert!(
        !install.contains("systemctl --user daemon-reload\n")
            || install.contains("\"${SYSTEMCTL[@]}\" daemon-reload"),
        "daemon-reload 应经 SYSTEMCTL 数组"
    );
    assert!(
        install.contains("\"${SYSTEMCTL[@]}\" enable echo-agent-core.service"),
        "enable 应经 SYSTEMCTL 数组"
    );
    assert!(
        install.contains("\"${SYSTEMCTL[@]}\" restart echo-agent-core.service"),
        "restart 应经 SYSTEMCTL 数组"
    );
    assert!(
        install.contains("\"${SYSTEMCTL[@]}\" is-active --quiet echo-agent-core.service"),
        "健康检查应经 SYSTEMCTL 数组"
    );

    // ③ linger 只属于 user 作用域：system 作用域必须跳过。
    assert!(
        install.contains("ECHO_SKIP_LINGER"),
        "system 作用域不得给用户开 linger（它与会话无关，开了只是常驻多余管理器）"
    );
    let linger_guard_at = install
        .find("ECHO_SKIP_LINGER:-0")
        .expect("ensure_service_linger 应检查 ECHO_SKIP_LINGER");
    // 注意匹配**命令行**而不是注释里的同名字样（注释在函数定义之前）。
    let enable_linger_at = install
        .find("loginctl enable-linger \"$user\"")
        .expect("user 作用域仍应 enable-linger");
    assert!(
        linger_guard_at < enable_linger_at,
        "跳过判断必须在调用 loginctl 之前"
    );

    // ④ 单元模板的三个作用域占位都要被渲染。
    for placeholder in ["@SCOPE_LINES@", "@SCOPE_ENV@", "@WANTED_BY@"] {
        assert!(
            install.contains(placeholder),
            "render_unit 应替换 {placeholder}"
        );
    }
    assert!(
        install.contains("multi-user.target") && install.contains("default.target"),
        "两条作用域的 WantedBy 都要有"
    );

    // ⑤ 作用域经环境变量下发（模块与 update.sh 据此选 systemctl 形态）。
    assert!(
        install.contains("ECHO_SERVICE_SCOPE=$SERVICE_SCOPE"),
        "单元里应注入 ECHO_SERVICE_SCOPE，供自更新模块与 update.sh 读取"
    );
}

/// 单元模板必须真的带占位；否则 install.sh 的 sed 静默替换不到任何东西，
/// 生成出一份"user 作用域却挂 multi-user.target"这种自相矛盾的单元。
#[test]
fn unit_templates_carry_scope_placeholders() {
    for name in [
        "packaging/systemd/echo-agent-core.service.in",
        "packaging/systemd/echo-agent-core-update.service.in",
    ] {
        let path = repo_root().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            text.contains("@SCOPE_LINES@"),
            "{name} 缺少 @SCOPE_LINES@（system 作用域的运行身份没有落点）"
        );
        assert!(
            text.contains("@SCOPE_ENV@"),
            "{name} 缺少 @SCOPE_ENV@（ECHO_SERVICE_SCOPE 注入不到）"
        );
    }
    let core =
        std::fs::read_to_string(repo_root().join("packaging/systemd/echo-agent-core.service.in"))
            .expect("read core unit template");
    assert!(
        core.contains("@WANTED_BY@"),
        "core 单元模板应把 WantedBy 留给安装期决定"
    );
}

/// update.sh 也要按作用域选 systemctl：它是 updater 单元的主体，重启失败
/// 会被判成"更新失败"，而实际上二进制已经换新了——状态与事实错位最难查。
#[test]
fn updater_respects_service_scope() {
    let update = read_script("update.sh");
    assert!(
        update.contains("resolve_service_scope"),
        "update.sh 应解析服务作用域"
    );
    assert!(
        update.contains("ECHO_SERVICE_SCOPE"),
        "update.sh 应优先采用单元注入的 ECHO_SERVICE_SCOPE"
    );
    assert!(
        update.contains("/etc/systemd/system/echo-agent-core.service"),
        "缺少 ECHO_SERVICE_SCOPE 时应按单元文件位置探测（手工在 shell 里跑的场景）"
    );
    for call in [
        "\"${SYSTEMCTL[@]}\" restart echo-agent-core.service",
        "\"${SYSTEMCTL[@]}\" is-active --quiet echo-agent-core.service",
    ] {
        assert!(
            update.contains(call),
            "update.sh 应经 SYSTEMCTL 调用：{call}"
        );
    }
    // 不允许再有裸 `systemctl --user restart`（两条作用域里必有一条是错的）。
    assert!(
        !update.contains("\n        systemctl --user restart echo-agent-core.service"),
        "update.sh 不应把 --user 写死在调用点上"
    );
}

/// 外部模块更新的默认路径必须是热替换；任何无法证明的范围都回退完整更新。
#[test]
fn external_module_hot_update_is_fail_closed_and_core_restart_free() {
    let update = read_script("update.sh");
    for required in [
        "UPDATE_SCOPE=auto",
        "--modules-only)",
        "select_update_scope",
        "refusing module-only update: non-module path changed",
        "module-upgrade --binary \"$module_dir/$_module\" --timeout 30",
        "upgrade_preserving_contributions",
        "HOT_SWAPPED",
        "core restart skipped",
    ] {
        // upgrade_preserving_contributions 在 Rust 装配端；其它在脚本端。
        if required == "upgrade_preserving_contributions" {
            let runtime = repo_root().join("source/modules/core/src/modules_runtime.rs");
            let runtime = std::fs::read_to_string(runtime).expect("read modules runtime");
            assert!(runtime.contains(required), "外部模块必须使用兼容热替换");
        } else {
            assert!(
                update.contains(required),
                "update.sh 缺少热更新接线：{required}"
            );
        }
    }
    assert!(
        update.contains("if [[ \"$UPDATE_SCOPE\" == modules ]] && ((core_was_active)); then"),
        "只有模块范围才允许走热更新分支"
    );
    assert!(
        update.contains("elif ((core_was_active)); then\n    PHASE=restarting"),
        "Core/内置模块变化必须保留完整重启路径"
    );
    assert!(
        update.contains("BIN_REPLACED || UPDATER_REPLACED || MODULES_REPLACED"),
        "只替换模块二进制后失败也必须进入回滚事务"
    );
    assert!(
        update.contains("BIN_REPLACED || UPDATER_REPLACED || MODULES_REPLACED"),
        "只替换模块二进制后失败也必须进入回滚事务"
    );
}

/// 自更新必须能"交棒给自己"：`--remote` 合并到的版本若改了 `scripts/update.sh`，
/// 磁盘上是新脚本、内存里是旧逻辑。旧逻辑不知道新版本要装什么，于是升级当场
/// 少做一步，要等下**一次**更新才补上。
///
/// 实测（2026-11-20，0.1.0 → 0.3.0）：`EXTERNAL_MODULE_BINARIES` 从 0 条变 3 处，
/// 升级当场 `libexec/modules` 连目录都没建，清单模块继续以内置那份挂载。
///
/// 上半段是接线断言，下半段是真跑一遍：造一个两 commit 的远端，第二个 commit
/// 把脚本换成"写标记后退出"的版本，跑旧脚本，断言标记出现——证明交棒发生了。
#[test]
fn updater_hands_over_to_the_merged_version_of_itself() {
    let update = read_script("update.sh");
    for required in [
        r#"ORIGINAL_ARGS=("$@")"#,
        "ECHO_UPDATER_REEXEC",
        r#"exec bash "$SOURCE_DIR/scripts/update.sh" "${ORIGINAL_ARGS[@]}""#,
        r#"git -C "$SOURCE_DIR" diff --quiet "$current" "$TARGET" -- scripts/update.sh"#,
        // 快路径必须连带核对模块二进制：修订号一致但模块没装，等于跑内置那份。
        r#"[[ -x "$MODULES_DIR/$_p/$_p" ]]"#,
    ] {
        assert!(
            update.contains(required),
            "update.sh 缺少交棒接线：{required}"
        );
    }
    assert!(
        update.contains(r#"if [[ "${ECHO_UPDATER_REEXEC:-0}" != 1 ]]"#),
        "交棒必须有单次守卫（否则两版对同一差异判读不一致时会无限重入）"
    );

    let tmp = std::env::temp_dir().join(format!("echo-updater-reexec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let git = |dir: &Path, args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=guard",
                "-c",
                "user.email=guard@example.invalid",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // 远端：第一个 commit 就是仓库里这份脚本（带交棒逻辑），第二个 commit 改到它。
    let origin = tmp.join("origin");
    std::fs::create_dir_all(origin.join("scripts")).expect("create origin");
    git(&origin, &["-c", "init.defaultBranch=main", "init", "-q"]);
    std::fs::write(origin.join("Cargo.toml"), "[workspace]\n").expect("write manifest");
    std::fs::write(origin.join("scripts/update.sh"), &update).expect("write update.sh");
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-qm", "the updater under test"]);

    let handoff = update.replacen(
        "set -Eeuo pipefail\n",
        "set -Eeuo pipefail\nprintf 'handed-over\\n' >\"${ECHO_TEST_MARKER:?}\"\nexit 0\n",
        1,
    );
    assert_ne!(handoff, update, "应能注入标记钩子");
    std::fs::write(origin.join("scripts/update.sh"), &handoff).expect("write new update.sh");
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-qm", "a newer updater"]);

    let work = tmp.join("src");
    git(
        &tmp,
        &[
            "clone",
            "-q",
            origin.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
        ],
    );
    git(&work, &["checkout", "-q", "-B", "main", "HEAD~1"]);

    // 脚本要求 cargo / systemctl 在 PATH 上；交棒发生在构建之前，桩函数不干活。
    let stubs = tmp.join("stubs");
    std::fs::create_dir_all(&stubs).expect("create stubs");
    for name in ["cargo", "systemctl"] {
        let path = stubs.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").expect("write stub");
        Command::new("chmod")
            .arg("+x")
            .arg(&path)
            .status()
            .expect("chmod stub");
    }

    let marker = tmp.join("marker");
    let status = tmp.join("status");
    let path_env = format!(
        "{}:{}",
        stubs.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(work.join("scripts/update.sh"))
        .args(["--remote", "--source"])
        .arg(&work)
        .arg("--binary")
        .arg(tmp.join("prefix/echo-agent-core-bin"))
        .arg("--status")
        .arg(&status)
        .env("PATH", &path_env)
        .env("ECHO_TEST_MARKER", &marker)
        .env("ECHO_SERVICE_SCOPE", "user")
        .env("XDG_STATE_HOME", tmp.join("state"))
        .env("XDG_DATA_HOME", tmp.join("data"))
        .output()
        .expect("run update.sh");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        marker.exists(),
        "合并到含新脚本的版本后没有交棒（标记文件未生成）；stderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read marker")
            .trim(),
        "handed-over"
    );
    assert!(
        stderr.contains("handing over to the new version"),
        "交棒应留下可诊断的一行；stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
