//! 全局供应商池的读取助手（`list_api_profiles` 工具与子代理 profile 解析共用）。
//!
//! 供应商池（`[agent].api_profiles`）是**全局**配置，但两个消费场景运行在
//! 看不到最新状态的 agent 上：
//! - `spawn_subagent` 的 `profile` 参数解析与 `list_api_profiles` 工具都运行
//!   在 **persona** agent 上，而配置变更命令（`UpdateApiConfig` / `DeleteApi`）
//!   在**管理面** agent 上执行——各 persona 持有的 [`crate::config::AgentConfig`]
//!   快照可能落后于管理面（persona 组装时的克隆）；
//! - 因此这里以共享 [`ConfigStore`]（core.toml，管理面每次变更都会写回）为
//!   **首选事实源**读取；读不到时调用方回退自身配置快照（单元测试 / 未接线）。
//!
//! 输出**绝不含明文密钥**：`key_set` 仅是布尔"是否已配置"。

use echo_adapter::ConfigStore;

use crate::config::ApiProfile;

/// 供应商池 + 顶层默认的只读快照（工具输出与 profile 解析用）。
#[derive(Debug, Default)]
pub(crate) struct PoolSnapshot {
    /// 池中全部 profile（原序）。
    pub profiles: Vec<ApiProfile>,
    /// 全局激活 profile 名（`[agent].active_api`；空 = 顶层默认）。
    pub active_api: String,
    /// 顶层默认 provider（无 profile / 未命中的兜底）。
    pub default_provider: String,
    /// 顶层默认 model。
    pub default_model: String,
    /// 顶层默认是否已配置 api_key（仅布尔，不回显明文）。
    pub default_key_set: bool,
}

/// 从共享配置存储读取供应商池快照。
///
/// - `Ok` = 读到（文件不存在按空池处理——全新部署尚未落盘）；
/// - `Err(message)` = 文件存在但读取 / 解析失败（调用方决定回退或上报）。
pub(crate) fn read_snapshot(store: &ConfigStore) -> Result<PoolSnapshot, String> {
    if !store.path().exists() {
        return Ok(PoolSnapshot::default());
    }
    let doc = store
        .read()
        .map_err(|e| format!("读取配置失败（{}）: {e}", store.path().display()))?;
    let agent = doc.get("agent");
    let profiles = agent
        .and_then(|agent| agent.get("api_profiles"))
        .cloned()
        .unwrap_or_else(|| toml::Value::Array(Vec::new()))
        .try_into::<Vec<ApiProfile>>()
        .map_err(|e| format!("供应商池解析失败: {e}"))?;
    let get_str = |key: &str| -> String {
        agent
            .and_then(|agent| agent.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Ok(PoolSnapshot {
        profiles,
        active_api: get_str("active_api"),
        default_provider: get_str("provider"),
        default_model: get_str("model"),
        default_key_set: !get_str("api_key").trim().is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str, content: &str) -> (ConfigStore, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "echo-pool-{tag}-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        if !content.is_empty() {
            std::fs::write(&path, content).unwrap();
        }
        (ConfigStore::new(path.clone()), path)
    }

    #[test]
    fn reads_profiles_and_defaults_without_secrets() {
        let (store, path) = temp_store(
            "ok",
            "[agent]\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\napi_key = \"sk-secret\"\nactive_api = \"kimi\"\n\n\
             [[agent.api_profiles]]\nname = \"deepseek\"\nprovider = \"deepseek\"\nmodel = \"deepseek-flash\"\napi_key = \"sk-a\"\n\n\
             [[agent.api_profiles]]\nname = \"kimi\"\nprovider = \"kimi\"\nmodel = \"k3\"\nbase_url = \"https://api.kimi.com/coding\"\napi_key = \"sk-b\"\n",
        );
        let snap = read_snapshot(&store).expect("snapshot");
        assert_eq!(snap.profiles.len(), 2);
        assert_eq!(snap.profiles[0].name, "deepseek");
        assert_eq!(snap.profiles[1].name, "kimi");
        assert_eq!(snap.active_api, "kimi");
        assert_eq!(snap.default_provider, "deepseek");
        assert!(snap.default_key_set);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty_pool_not_error() {
        let (store, path) = temp_store("missing", "");
        assert!(!path.exists());
        let snap = read_snapshot(&store).expect("empty ok");
        assert!(snap.profiles.is_empty());
    }

    #[test]
    fn missing_section_is_empty_pool() {
        let (store, path) = temp_store("empty-doc", "[logging]\nlevel = \"info\"\n");
        let snap = read_snapshot(&store).expect("ok");
        assert!(snap.profiles.is_empty());
        assert!(!snap.default_key_set);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unreadable_file_reports_error() {
        let dir = std::env::temp_dir().join(format!(
            "echo-pool-invalid-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("core.toml");
        std::fs::write(&path, "not = valid = toml\n").unwrap();
        let store = ConfigStore::new(path);
        assert!(read_snapshot(&store).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
