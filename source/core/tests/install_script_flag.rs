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
        .expect("source/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read_script(name: &str) -> String {
    let path = repo_root().join("scripts").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// install.sh / reinstall.sh / update.sh 必须通过 `bash -n` 语法检查。
#[test]
fn install_scripts_are_bash_syntax_clean() {
    for name in ["install.sh", "reinstall.sh", "update.sh"] {
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

/// 从脚本里抠出 `PLUGIN_BINARIES=(...)` 的条目。
fn plugin_binaries(script: &str) -> Vec<String> {
    let start = script
        .find("PLUGIN_BINARIES=(")
        .unwrap_or_else(|| panic!("script should declare PLUGIN_BINARIES"));
    let rest = &script[start + "PLUGIN_BINARIES=(".len()..];
    let end = rest
        .find(')')
        .unwrap_or_else(|| panic!("unterminated PLUGIN_BINARIES"));
    rest[..end].split_whitespace().map(str::to_string).collect()
}

/// 仓库里**可安装**的进程外插件 crate：`source/plugin/*/Cargo.toml` 中声明了
/// `[[bin]] path = "src/main.rs"` 的包。
///
/// `src/bin/*.rs` 的 bin（`echo-plugin-test-peer`）是测试夹具，装机没有意义，
/// 因此判定看 bin 的 path 而不是"有没有 `[[bin]]`"。
fn installable_plugin_packages() -> Vec<String> {
    let plugin_root = repo_root().join("source").join("plugin");
    let mut packages: Vec<String> = std::fs::read_dir(&plugin_root)
        .unwrap_or_else(|e| panic!("read {}: {e}", plugin_root.display()))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
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

/// 安装器与仓库实际一致：install.sh / update.sh 的 `PLUGIN_BINARIES` 必须
/// 等于「可安装的进程外插件 crate」集合，且两个脚本彼此一致。
///
/// 为什么要有这条：自更新与首装装的是**同一套**插件二进制（Core 与插件必须
/// 配套升级）。新增一个插件 crate 却忘了写进清单 → 装机后 `plugins.toml`
/// 指向一个不存在的路径；反过来删了 crate 却留着清单 → 每次 install 都
/// 报 "plugin binary missing after build"。
#[test]
fn plugin_binary_lists_match_the_workspace() {
    let mut expected = installable_plugin_packages();
    expected.sort();

    let install = read_script("install.sh");
    let update = read_script("update.sh");
    let mut from_install = plugin_binaries(&install);
    let mut from_update = plugin_binaries(&update);
    from_install.sort();
    from_update.sort();

    assert_eq!(
        from_install, expected,
        "install.sh 的 PLUGIN_BINARIES 与仓库可安装插件 crate 不一致"
    );
    assert_eq!(
        from_update, expected,
        "update.sh 的 PLUGIN_BINARIES 与仓库可安装插件 crate 不一致"
    );
}

/// 插件目录约定接线：install.sh 建目录并把二进制装到 `plugins/<包>/<包>`；
/// update.sh 做版本化替换 + 回滚副本，且失败时能还原。
#[test]
fn plugin_directory_install_and_rollback_are_wired() {
    let install = read_script("install.sh");
    assert!(
        install.contains(r#"PLUGINS_DIR="$LIBEXEC_DIR/plugins""#),
        "install.sh 应以 libexec/plugins 作为插件根"
    );
    assert!(
        install.contains(r#"install -d -m 755 "$plugin_dir""#),
        "install.sh 应按包名建子目录"
    );
    assert!(
        install.contains(r#"mv -f "$plugin_temporary" "$plugin_dir/$_plugin""#),
        "install.sh 应原子替换插件二进制"
    );
    assert!(
        install.contains(r#"snapshot_file "$PLUGINS_DIR/$_plugin/$_plugin""#),
        "install.sh 应把旧插件二进制纳入安装事务快照"
    );

    let update = read_script("update.sh");
    assert!(
        update.contains(r#"plugin_rollback="$plugin_dir/$_plugin.rollback""#),
        "update.sh 应保留上一版插件二进制（.rollback）"
    );
    assert!(
        update.contains(r#"PLUGIN_ROLLBACKS["$_plugin"]=$plugin_rollback"#),
        "update.sh 应记录回滚副本路径"
    );
    assert!(
        update.contains("PLUGINS_REPLACED=1"),
        "update.sh 应标记插件已替换（供 finalize_failure 触发回滚）"
    );
    assert!(
        update.contains(r#"mv -f "${PLUGIN_ROLLBACKS[$_p]}" "$PLUGINS_DIR/$_p/$_p""#),
        "回滚应把插件二进制还原回上一版"
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
        .find("systemctl --user enable echo-agent-core.service")
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
