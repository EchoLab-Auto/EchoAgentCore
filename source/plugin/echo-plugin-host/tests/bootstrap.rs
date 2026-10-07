//! `bootstrap` 模块集成测试：配置 → 启动封装的端到端行为。
//!
//! 对端是 stdio 测试插件进程（`src/bin/echo-plugin-test-peer.rs`，经
//! `env!("CARGO_BIN_EXE_echo-plugin-test-peer")` 定位）：注册 `echo` 工具、
//! Invoke 回显 `payload["text"]`、Drain 后退出。
//!
//! 覆盖场景：
//! 1. [`launch_specs`] 只取 enabled 的 stdio 条目，其余进跳过原因；
//! 2. 全成功启动 → 句柄 / 工具可用（`echo` 调用回显）；
//! 3. 单点失败 best-effort（坏 command 与好 command 并存）；
//! 4. [`start_from_file`]（真实 `plugins.toml`）+ 文件缺失 → Err；
//! 5. shutdown 排空到 Stopped。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use echo_defs::tool::Tool as _;
use echo_plugin_host::bootstrap::{launch_specs, start_from_file, start_plugins, LaunchSpec};
use echo_plugin_host::supervisor::{PluginState, PluginSupervisor};
use echo_plugin_loader::{EntryKind, LoadedTree, PluginEntry};
use serde_json::json;

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-plugin-test-peer");

// ── 辅助 ────────────────────────────────────────────────────────────────────

fn entry(id: &str, kind: EntryKind, enabled: bool) -> PluginEntry {
    PluginEntry {
        id: id.to_string(),
        kind,
        name: id.to_string(),
        command: if kind == EntryKind::Stdio {
            Some(PEER.to_string())
        } else {
            None
        },
        args: vec![],
        env: BTreeMap::new(),
        enabled,
        requires: vec![],
        config: json!({}),
        layer: 0,
        layer_source: "test.toml".to_string(),
    }
}

fn spec(plugin_id: &str, command: &str) -> LaunchSpec {
    LaunchSpec {
        plugin_id: plugin_id.to_string(),
        command: command.to_string(),
        args: vec![],
        env: BTreeMap::new(),
        config: json!({}),
    }
}

/// 唯一临时目录（不引入 tempfile 依赖；测试尾部由调用方清理）。
fn unique_temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "echo-plugin-host-bootstrap-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ))
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

// ── 1. launch_specs 筛选 ────────────────────────────────────────────────────

#[test]
fn launch_specs_filters_enabled_stdio_only() {
    let tree = LoadedTree {
        entries: vec![
            entry("peer-a", EntryKind::Stdio, true),
            entry("peer-b", EntryKind::Stdio, false),
            entry("websearch", EntryKind::Builtin, true),
        ],
        warnings: vec![],
    };
    let (specs, skipped) = launch_specs(&tree);
    assert_eq!(specs.len(), 1, "只有 enabled stdio 条目可启动");
    assert_eq!(specs[0].plugin_id, "peer-a");
    assert_eq!(specs[0].command, PEER);
    assert_eq!(skipped.len(), 2, "跳过原因应逐条记录: {skipped:?}");
    assert!(
        skipped.iter().any(|s| s == "peer-b: disabled"),
        "缺少 disabled 原因: {skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|s| s == "websearch: kind builtin skipped"),
        "缺少 builtin 原因: {skipped:?}"
    );
}

#[test]
fn launch_specs_skips_stdio_without_command() {
    // loader 校验过的树不会出现这种条目；手工构造模拟损坏树。
    let mut broken = entry("broken", EntryKind::Stdio, true);
    broken.command = None;
    let tree = LoadedTree {
        entries: vec![broken],
        warnings: vec![],
    };
    let (specs, skipped) = launch_specs(&tree);
    assert!(specs.is_empty());
    assert_eq!(skipped, vec!["broken: stdio without command skipped"]);
}

// ── 2. 全成功路径 ───────────────────────────────────────────────────────────

#[tokio::test]
async fn start_plugins_all_success_path() {
    let supervisor = PluginSupervisor::new();
    let started = start_plugins(&supervisor, vec![spec("peer-ok", PEER)]).await;
    assert_eq!(started.handles.len(), 1);
    assert!(started.errors.is_empty(), "errors: {:?}", started.errors);

    let tools = started.tools();
    assert_eq!(tools.len(), 1, "对端注册一个工具（echo）");
    assert_eq!(tools[0].tool_name(), "echo");
    let out = tools[0]
        .execute(json!({"text": "hi"}))
        .await
        .expect("echo execute");
    assert!(out.contains("hi"), "echo 应回显 text，实际: {out}");

    started.shutdown(Duration::from_secs(2)).await;
}

// ── 3. 单点失败 best-effort ────────────────────────────────────────────────

#[tokio::test]
async fn start_plugins_best_effort_on_single_failure() {
    let supervisor = PluginSupervisor::new();
    let started = start_plugins(
        &supervisor,
        vec![spec("bad", "/nonexistent/xyz"), spec("peer-ok", PEER)],
    )
    .await;
    assert_eq!(
        started.errors.len(),
        1,
        "只有一个失败: {:?}",
        started.errors
    );
    assert_eq!(started.errors[0].0, "bad");
    assert!(!started.errors[0].1.is_empty(), "错误描述非空");
    assert_eq!(started.handles.len(), 1, "其余插件应继续启动");
    assert!(!started.tools().is_empty(), "成功插件工具应可用");

    started.shutdown(Duration::from_secs(2)).await;
}

// ── 4. start_from_file ─────────────────────────────────────────────────────

#[tokio::test]
async fn start_from_file_loads_toml_and_missing_path_errors() {
    let dir = unique_temp_dir("file");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("plugins.toml");
    let toml = format!(
        "[[plugin]]\nid = \"peer\"\nkind = \"stdio\"\nname = \"echo-plugin-test-peer\"\n\
         command = {:?}\nenabled = true\nconfig = {{ mode = \"test\" }}\n",
        PEER
    );
    std::fs::write(&path, toml).expect("write plugins.toml");

    // 存在且合法 → Ok + 可启动。
    let supervisor = PluginSupervisor::new();
    let started = start_from_file(&path, &supervisor)
        .await
        .expect("plugins.toml 应装载成功");
    assert_eq!(started.handles.len(), 1);
    assert!(started.errors.is_empty(), "errors: {:?}", started.errors);
    // 排空干净退出。
    started.shutdown(Duration::from_secs(2)).await;

    // 文件不存在路径 → Err。
    let missing = dir.join("nope").join("plugins.toml");
    assert!(
        start_from_file(&missing, &supervisor).await.is_err(),
        "不存在的配置应返回 Err"
    );

    cleanup(&dir);
}

// ── 5. shutdown → Stopped ──────────────────────────────────────────────────

#[tokio::test]
async fn shutdown_drains_handles_to_stopped() {
    let supervisor = PluginSupervisor::new();
    let started = start_plugins(
        &supervisor,
        vec![spec("peer-shutdown-a", PEER), spec("peer-shutdown-b", PEER)],
    )
    .await;
    assert_eq!(started.handles.len(), 2);
    for handle in &started.handles {
        assert_eq!(handle.state(), PluginState::Ready);
    }

    started.shutdown(Duration::from_secs(2)).await;

    for handle in &started.handles {
        assert_eq!(
            handle.state(),
            PluginState::Stopped,
            "drain 后应为终态 Stopped（plugin={}）",
            handle.plugin_id()
        );
    }
}
