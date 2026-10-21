//! 联邦管理面的**宿主状态迁移实现**（2026-11 S11）。
//!
//! ## 这层为什么存在、为什么这么薄
//!
//! 命令域的处理（判定 + 顺序 + 文案）已经搬进 `echo-plugin-federation::admin`
//! 的纯函数里。留在宿主的是**只有它能做的那部分**：
//!
//! | 动作 | 为什么只有组合根能做 |
//! |---|---|
//! | 落 peer（`Federation::add_peer`） | 链路管理器是组合根构造的 |
//! | 配置原子写回（`ConfigStore::patch`） | 配置层由组合根持有 |
//! | 删 peer（断链 + 在飞调用终结 + 反查表清理） | 要同时动 router / 映射 / 保活桶 |
//! | 邀请占位（运行时表 + 过期清扫） | 占位登记表是 `FederationRuntime` 的字段 |
//! | 会话迁移 | 实现跨 `federation_import` / trunk / 链路三层 |
//!
//! 每个方法都是**原子动作**，不含任何"先判一判"——规则已在插件里判完。这条
//! 纪律让本文件可以一眼读完（对照 S11 之前 300 行的 `handle_federation_command`）。

use std::sync::Arc;

use echo_federation::PeerConfig;
use echo_plugin_federation::admin::{Emit, FederationAdminHost, PeerSave, PeerSummary};
use echo_protocol::BackendEvent;

use crate::federation_rt::FederationRuntime;

/// `FederationRuntime` 的管理面视图。
pub struct RuntimeAdminHost {
    rt: Arc<FederationRuntime>,
}

impl RuntimeAdminHost {
    pub fn new(rt: Arc<FederationRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl FederationAdminHost for RuntimeAdminHost {
    async fn peers(&self) -> Vec<PeerSummary> {
        self.rt
            .federation
            .peer_configs()
            .await
            .into_iter()
            .map(|peer| PeerSummary {
                name: peer.name,
                token: peer.token,
            })
            .collect()
    }

    fn merged_management_token(&self) -> Option<String> {
        self.rt.merged_management_token.clone()
    }

    fn listen(&self) -> String {
        self.rt.listen.clone()
    }

    fn advertise(&self) -> String {
        self.rt.advertise.clone()
    }

    fn observed_host(&self) -> Option<String> {
        // 入站连接观测到的「对端/面板访问本机用的主机名」（2026-11）：
        // 邀请串优先用它——手工 advertise 常停在只有本机视角成立的地址。
        self.rt.federation.observed_host_sync()
    }

    fn node_name(&self) -> Option<String> {
        self.rt.node_name.clone()
    }

    async fn status_event(&self) -> BackendEvent {
        self.rt.status_event().await
    }

    async fn directory_event(&self) -> BackendEvent {
        self.rt.directory_event().await
    }

    async fn apply_peer(&self, peer: PeerSave) {
        let rt = &self.rt;
        rt.federation
            .add_peer(PeerConfig {
                name: peer.name.clone(),
                url: peer.url.clone(),
                token: peer.token.clone(),
            })
            .await;
        // 配置原子写回。
        let PeerSave {
            name,
            url,
            token,
            allow_tools,
            require_confirm,
            allow_queries,
            allow_subagent,
        } = peer;
        // 策略要留一份给运行时表（配置写回会把它们 move 进闭包）。
        let policy = echo_plugin_federation::ExecutorPolicy {
            allow_tools: allow_tools.clone(),
            require_confirm: require_confirm.clone(),
            allow_queries: allow_queries.clone(),
            allow_subagent,
        };
        let write_name = name.clone();
        if let Err(error) = rt.config_store.patch(move |root| {
            let federation = echo_config::ensure_table(root, "federation");
            let peers = echo_config::ensure_table(federation, "peers");
            let entry = echo_config::ensure_table(peers, &write_name);
            entry.insert("url".into(), toml::Value::String(url));
            entry.insert("token".into(), toml::Value::String(token));
            entry.insert(
                "allow_tools".into(),
                toml::Value::Array(allow_tools.into_iter().map(toml::Value::String).collect()),
            );
            entry.insert(
                "allow_subagent".into(),
                toml::Value::Boolean(allow_subagent),
            );
            entry.insert(
                "require_confirm".into(),
                toml::Value::Array(
                    require_confirm
                        .into_iter()
                        .map(toml::Value::String)
                        .collect(),
                ),
            );
            entry.insert(
                "allow_queries".into(),
                toml::Value::Array(
                    allow_queries.into_iter().map(toml::Value::String).collect(),
                ),
            );
            Ok(())
        }) {
            tracing::error!(target: "federation", peer = %name, %error, "peer 配置写回失败");
        }
        // 运行时策略同步（allow_tools/queries/subagent 立即生效，无需重启）
        // ——此前仅启动装配期填充，Panel 保存的白名单静默失效。
        rt.peer_policies.write().await.insert(name.clone(), policy);
        // 链路已 Up 时立即刷新路由策略。
        if let Some(node) = rt.peer_names.read().await.get(&name).cloned() {
            let policy = rt
                .peer_policies
                .read()
                .await
                .get(&name)
                .cloned()
                .unwrap_or_default();
            rt.router.set_policy(&node, policy);
        }
    }

    async fn remove_peer(&self, name: &str) {
        let rt = &self.rt;
        let node_id = rt.peer_names.read().await.get(name).cloned();
        rt.federation.remove_peer(name).await;
        if let Some(node) = node_id {
            // Down 等价清理（2026-10 审计 P0-2）：drop_link 移除链路句柄后，
            // 链路自身退出时 still_mine=false **不再发 Down**——此前这里仅
            // drop_link，导致 router 在飞调用、工具包禁用、映射与保活桶全部
            // 跳过（删除在线 peer 后工具残留可调用、反查文案错、在飞调用悬挂
            // 至超时）。
            rt.router.drop_peer(&node);
            // 保活桶移除 = 注销该 peer 全部远程工具（删除语义强于 Down 的
            // "整包禁用"：peer 已从配置消失，不设重新启用路径）。
            {
                let mut slot = rt
                    .remote_tool_keepers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slot.remove(&node);
            }
            rt.node_to_peer.write().await.remove(&node);
            rt.peer_names.write().await.remove(name);
            rt.federation.drop_link(&node).await;
            tracing::info!(
                target: "federation",
                peer = %node, name = %name,
                "federation peer deleted (router pending failed; remote tools unregistered; mappings cleaned)"
            );
        }
        // 邀请占位登记表同步移除（运行时-only；2026-10）。
        {
            let mut registry = rt
                .invite_placeholders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.remove(name);
        }
        let write_name = name.to_string();
        if let Err(error) = rt.config_store.patch(move |root| {
            if let Some(peers) = root
                .get_mut("federation")
                .and_then(|f| f.get_mut("peers"))
                .and_then(|p| p.as_table_mut())
            {
                peers.remove(&write_name);
            }
            Ok(())
        }) {
            tracing::error!(target: "federation", peer = %name, %error, "联邦 peer 配置移除失败");
        }
    }

    async fn register_invite(&self, placeholder: &str, token: &str) -> Vec<String> {
        let rt = &self.rt;
        // 待对端回连：占位 peer 仅注册进运行时，**不写配置**——配对成功即
        // 清理；未配对的邀请在重启后自然失效（一次性邀请语义），避免配置
        // 文件堆积垃圾占位条目。
        rt.federation
            .add_peer(PeerConfig {
                name: placeholder.to_string(),
                url: String::new(),
                token: token.to_string(),
            })
            .await;
        // 占位登记 + 存量清扫（2026-10 审计 P1-2）：未配对占位此前只增不减
        // （面板幽灵 peer、无取消入口）——生成新邀请时顺带清理超过 1 小时且
        // 未配对的旧占位。
        let stale: Vec<String> = {
            let mut registry = rt
                .invite_placeholders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stale: Vec<String> = registry
                .iter()
                .filter(|(_, created)| created.elapsed() > std::time::Duration::from_secs(3600))
                .map(|(name, _)| name.clone())
                .collect();
            for name in &stale {
                registry.remove(name);
            }
            stale
        }; // 锁在 await 前释放（MutexGuard 非 Send）
        for name in &stale {
            rt.federation.remove_peer(name).await;
        }
        rt.invite_placeholders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(placeholder.to_string(), std::time::Instant::now());
        stale
    }

    async fn migrate_session(
        &self,
        session_id: String,
        team_id: Option<String>,
        target_peer: String,
        emit: Emit,
    ) {
        crate::federation_import::handle_migrate_session(
            emit,
            &self.rt,
            session_id,
            team_id,
            target_peer,
        )
        .await;
    }
}
