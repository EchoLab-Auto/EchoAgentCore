//! 节点身份（federation Phase 0）：NodeId 的生成与持久化。
//!
//! 每个 Core 节点在首次启动时生成一个 [`NodeId`] 并写入配置同目录的
//! `echo-node.json`（tmp + rename 原子写，与 `echo-workspaces-{id}.json`
//! 同策略）；此后稳定，重启不变。Phase 0 仅加载/生成并记录日志，联邦
//! 链路（Phase 1）才会消费它。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

/// 持久化文件名（与配置 TOML 同目录）。
pub const NODE_FILE: &str = "echo-node.json";

/// 持久化文档。`node_name` 预留人类可读别名（Phase 1 配置段消费）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeDocument {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
}

/// 加载或生成节点身份。
///
/// - 文件存在且含非空 `node_id` → 直接采用（`node_name` 缺失不视为错误）
/// - 文件缺失/损坏 → 生成新 NodeId 并原子写入
pub fn load_or_create(config_dir: &Path) -> Result<NodeDocument> {
    let path = config_dir.join(NODE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<NodeDocument>(&text) {
            Ok(doc) if !doc.node_id.trim().is_empty() => {
                info!(node_id = %doc.node_id, path = %path.display(), "node identity loaded");
                Ok(doc)
            }
            Ok(_) => {
                anyhow::bail!("{}: node_id 为空，请删除该文件以重新生成", path.display())
            }
            Err(e) => {
                anyhow::bail!(
                    "{}: 节点身份文件损坏（{e}），请修复或删除以重新生成",
                    path.display()
                )
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let doc = NodeDocument {
                node_id: echo_defs::NodeId::generate().to_string(),
                node_name: None,
            };
            write_atomic(&path, &doc)
                .with_context(|| format!("写入节点身份文件 {}", path.display()))?;
            info!(node_id = %doc.node_id, path = %path.display(), "node identity generated");
            Ok(doc)
        }
        Err(e) => Err(e).with_context(|| format!("读取节点身份文件 {}", path.display())),
    }
}

/// tmp + rename 原子写。
fn write_atomic(path: &Path, doc: &NodeDocument) -> std::io::Result<()> {
    let tmp: PathBuf = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_then_reloads_stable_id() {
        let dir = std::env::temp_dir().join(format!("echo-node-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = load_or_create(&dir).unwrap();
        assert!(first.node_id.starts_with("node-"));
        assert!(dir.join(NODE_FILE).exists());

        let second = load_or_create(&dir).unwrap();
        assert_eq!(first.node_id, second.node_id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_errors() {
        let dir = std::env::temp_dir().join(format!("echo-node-test-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(NODE_FILE), "{ not json").unwrap();

        assert!(load_or_create(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
