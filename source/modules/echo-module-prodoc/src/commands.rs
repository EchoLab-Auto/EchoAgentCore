//! ProDoc 命令域（P1：只读）。
//!
//! 三条命令：
//!
//! | 命令 | 用途 |
//! |---|---|
//! | `RequestProDocBundle` | 打开一个文档根，推回全量文件快照 |
//! | `BrowseProDocDirs` | 目录选择器的逐层浏览（只列子目录） |
//! | `ProDocRoots` | 可打开的根列表（配置 + 各人格工作区目录） |
//!
//! 命令本身**只回执成功/失败**，文档内容走模块事件（见 `lib.rs` 的事件协议）：
//! 一份快照可能几百 KB，塞进命令回执会和面板的轮询/重放逻辑缠在一起，而事件
//! 通道本来就为"服务器主动推"设计的。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::bundle::{self, ScanError};
use crate::{Caps, DirEntry, ModuleState, RootEntry};

/// 本模块处理的命令（`Contribution::Command` 的 `name`）。
pub const COMMANDS: [&str; 3] = [
    "RequestProDocBundle",
    "BrowseProDocDirs",
    "ProDocRoots",
];

/// 命令入口（返回 `None` = 不认领，不是错误）。
pub async fn handle(state: &ModuleState, name: &str, payload: Value) -> Option<Result<Value, String>> {
    if !COMMANDS.contains(&name) {
        return None;
    }
    let team_id = payload
        .get("team_id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string);

    let outcome = match name {
        "RequestProDocBundle" => request_bundle(state, team_id, &payload).await,
        "BrowseProDocDirs" => browse(state, &payload),
        "ProDocRoots" => roots(state),
        other => return Some(Err(format!("命令 {other} 被声明但处理器没有认领它"))),
    };
    Some(outcome)
}

/// 打开文档根并推回快照（`RequestProDocBundle`）。
async fn request_bundle(
    state: &ModuleState,
    team_id: Option<String>,
    payload: &Value,
) -> Result<Value, String> {
    let config = state.config();
    let raw = payload
        .get("root")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .ok_or_else(|| "RequestProDocBundle 缺少 root".to_string())?;
    let root = PathBuf::from(raw);

    // 沙箱：只允许配置里显式给出的根，或它们的子目录。
    // 不做"任意绝对路径"——那等于给面板开了一个读全盘 `.md` 的口子。
    if let Err(message) = check_allowed(&config.roots, &root) {
        state.emit_error(team_id.as_deref(), raw, &message);
        return Err(message);
    }

    // 扫描是阻塞 IO（几千个文件几百毫秒），必须在阻塞线程池里跑：
    // 这条命令走的是宿主单条命令通道，阻塞它会连累其他模块的命令。
    let scan_root = root.clone();
    let scan_config = config.clone();
    let scanned = tokio::task::spawn_blocking(move || bundle::scan(&scan_root, &scan_config))
        .await
        .map_err(|error| format!("文档扫描失败: {error}"))?;

    match scanned {
        Ok(mut bundle) => {
            bundle.team_id = team_id.clone();
            let documents = bundle.stats.documents;
            let bytes = bundle.stats.bytes;
            state.emit_bundle(&bundle);
            Ok(json!({
                "root": bundle.root,
                "documents": documents,
                "bytes": bytes,
                "truncated": bundle.stats.truncated,
            }))
        }
        Err(error) => {
            let message = match &error {
                ScanError::RootUnavailable(text) => text.clone(),
                ScanError::Empty => format!("{}里没有任何 .md 文件", raw),
            };
            state.emit_error(team_id.as_deref(), raw, &message);
            Err(message)
        }
    }
}

/// 目录选择器：列 `path` 下的子目录（`BrowseProDocDirs`）。
///
/// 刻意**不**复用工作区模块的浏览根：那套以"全部人格工作区目录 ∪ HOME"为根，
/// 面向的是"给会话选工作目录"；文档根是"我想看这棵文档" —— 边界由配置的
/// `roots` 决定，缺省时回退到工作区/家目录，语义不同。
fn browse(state: &ModuleState, payload: &Value) -> Result<Value, String> {
    let config = state.config();
    let requested = payload
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let allowed = root_entries(&config);

    // 空路径 = 根列表。
    let Some(requested) = requested else {
        state.emit_dirs(None, &allowed, &[]);
        return Ok(json!({"roots": allowed.len(), "entries": 0}));
    };

    let path = PathBuf::from(requested);
    check_allowed(&config.roots, &path)?;
    let entries = list_child_dirs(&path)?;
    state.emit_dirs(Some(requested), &[], &entries);
    Ok(json!({"path": requested, "entries": entries.len()}))
}

/// 可打开的根列表（`ProDocRoots`）。
///
/// 配置为空时回退到 HOME：模块刚装上、还没配 `roots` 时，面板至少能让人从家
/// 目录往下走，而不是一个空下拉框。
fn roots(state: &ModuleState) -> Result<Value, String> {
    let config = state.config();
    let entries = root_entries(&config);
    state.emit_roots(&entries, &Caps::of(&config));
    Ok(json!({"roots": entries.len(), "configured": !config.roots.is_empty()}))
}

/// 生效的根列表（面板渲染用）。
fn root_entries(config: &crate::Config) -> Vec<RootEntry> {
    allowed_roots(config)
        .into_iter()
        .map(|root| RootEntry {
            label: label_of(&root),
            path: root.to_string_lossy().into_owned(),
        })
        .collect()
}

/// 生效的根列表：配置优先，空则回退 HOME。
fn allowed_roots(config: &crate::Config) -> Vec<PathBuf> {
    let configured: Vec<PathBuf> = config
        .roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .filter(|root| root.is_dir())
        .collect();
    if !configured.is_empty() {
        return configured;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
        .into_iter()
        .collect()
}

/// `path` 是否落在某个允许根内（含根本身）。
///
/// 两边都先 `canonicalize`：符号链接的比较必须发生在真实路径上，否则
/// `/home/me/link-to-docs` 与 `/srv/docs` 会被判成两个不相干的位置。
fn check_allowed(configured: &[String], path: &Path) -> Result<(), String> {
    let config = crate::Config {
        roots: configured.to_vec(),
        ..Default::default()
    };
    let roots = allowed_roots(&config);
    if roots.is_empty() {
        return Err("没有可用的文档根（配置里没有 roots，且 HOME 不可用）".into());
    }
    let target = std::fs::canonicalize(path)
        .map_err(|error| format!("路径不可访问：{}（{error}）", path.display()))?;
    for root in &roots {
        let Ok(root_real) = std::fs::canonicalize(root) else {
            continue;
        };
        if target == root_real || target.starts_with(&root_real) {
            return Ok(());
        }
    }
    Err(format!(
        "文档根必须在允许范围内：{}",
        roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("、")
    ))
}

/// 列子目录（不列文件：选择器只关心"能不能进这一层"）。
fn list_child_dirs(path: &Path) -> Result<Vec<DirEntry>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("目录不可读：{}（{error}）", path.display()))?;
    let mut dirs: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        if crate::bundle::SKIP_DIRS.contains(&name.as_str()) || name == "target" {
            continue;
        }
        dirs.push((name, entry.path().to_string_lossy().into_owned()));
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(dirs
        .into_iter()
        .map(|(name, full)| DirEntry {
            name,
            path: full,
            is_dir: true,
        })
        .collect())
}

/// 显示名：路径最后一段（根目录回退到全路径）。
fn label_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-prodoc-cmd-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/guide")).expect("mkdir");
        std::fs::write(dir.join("docs/project.md"), "---\nid: p\n---\n# P\n").expect("write");
        std::fs::write(dir.join("docs/guide/a.md"), "# A\n").expect("write");
        dir
    }

    fn state_with(root: &Path) -> ModuleState {
        ModuleState::new(Config {
            roots: vec![root.to_string_lossy().into_owned()],
            ..Config::default()
        })
    }

    /// 认领语义：无关命令返回 `None`。
    #[tokio::test]
    async fn unknown_command_is_not_claimed() {
        let state = ModuleState::default();
        assert!(handle(&state, "RequestState", json!({})).await.is_none());
    }

    /// 打开文档根 → 推一份快照事件，回执带统计。
    #[tokio::test]
    async fn request_bundle_emits_snapshot() {
        let dir = temp_dir("bundle");
        let state = state_with(&dir.join("docs"));
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            state.set_event_sink(std::sync::Arc::new(move |event| {
                seen.lock().unwrap().push((event.name.clone(), event.payload.clone()));
            }));
        }

        let receipt = handle(
            &state,
            "RequestProDocBundle",
            json!({"team_id": "t", "root": dir.join("docs").to_string_lossy()}),
        )
        .await
        .expect("claimed")
        .expect("ok");
        assert_eq!(receipt["documents"], 2);

        let events = seen.lock().unwrap().clone();
        let (name, payload) = events.iter().find(|(name, _)| name == crate::events::BUNDLE).expect("bundle event");
        assert_eq!(name, crate::events::BUNDLE);
        assert_eq!(payload["team_id"], "t");
        assert!(payload["files"]["guide/a.md"].is_string());
        assert_eq!(payload["stats"]["documents"], 2);
    }

    /// 根不在允许范围 → 明确报错 + 错误事件（面板要能显示"为什么打不开"）。
    #[tokio::test]
    async fn root_outside_allowed_is_rejected() {
        let dir = temp_dir("outside");
        let state = state_with(&dir.join("docs"));
        let outside = std::env::temp_dir().join("echo-prodoc-elsewhere");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("x.md"), "# x\n").expect("write");

        let outcome = handle(
            &state,
            "RequestProDocBundle",
            json!({"team_id": "t", "root": outside.to_string_lossy()}),
        )
        .await
        .expect("claimed");
        assert!(outcome.unwrap_err().contains("允许范围"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 目录选择器：空路径给根列表，给路径给子目录。
    ///
    /// 断言走**事件**而不是命令回执：宿主把 `Ok(_)` 的返回值直接丢弃
    /// （`apply_command` 只认 `Err` 发错误），面板能看见的只有事件。
    #[tokio::test]
    async fn browse_emits_roots_then_children() {
        let dir = temp_dir("browse");
        let state = state_with(&dir.join("docs"));
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            state.set_event_sink(std::sync::Arc::new(move |event| {
                seen.lock().unwrap().push((event.name.clone(), event.payload.clone()));
            }));
        }

        handle(&state, "BrowseProDocDirs", json!({}))
            .await
            .expect("claimed")
            .expect("ok");
        let roots_event = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == crate::events::DIRS)
            .map(|(_, payload)| payload.clone())
            .expect("应有 prodoc.dirs 事件");
        assert_eq!(roots_event["roots"].as_array().unwrap().len(), 1);

        handle(
            &state,
            "BrowseProDocDirs",
            json!({"path": dir.join("docs").to_string_lossy()}),
        )
        .await
        .expect("claimed")
        .expect("ok");
        let events = seen.lock().unwrap().clone();
        let children_event = events
            .iter()
            .filter(|(name, _)| name == crate::events::DIRS)
            .map(|(_, payload)| payload.clone())
            .last()
            .expect("应有第二条 prodoc.dirs");
        let names: Vec<String> = children_event["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["guide".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 根列表命令：事件里带生效上限（面板要显示"最多 2000 篇"这类说明）。
    #[tokio::test]
    async fn roots_command_emits_caps() {
        let dir = temp_dir("roots");
        let state = state_with(&dir.join("docs"));
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            state.set_event_sink(std::sync::Arc::new(move |event| {
                seen.lock().unwrap().push((event.name.clone(), event.payload.clone()));
            }));
        }
        handle(&state, "ProDocRoots", json!({}))
            .await
            .expect("claimed")
            .expect("ok");
        let payload = seen
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == crate::events::ROOTS)
            .map(|(_, payload)| payload.clone())
            .expect("应有 prodoc.roots 事件");
        assert_eq!(payload["caps"]["max_files"], 2000);
        assert_eq!(payload["roots"][0]["label"], "docs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
