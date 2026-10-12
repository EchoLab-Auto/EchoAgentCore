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

/// install.sh / reinstall.sh 必须通过 `bash -n` 语法检查。
#[test]
fn install_scripts_are_bash_syntax_clean() {
    for name in ["install.sh", "reinstall.sh"] {
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
