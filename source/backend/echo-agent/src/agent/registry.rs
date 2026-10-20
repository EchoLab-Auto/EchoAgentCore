//! agent 集合的查询面（S4b）：`Agent` 认识"我的兄弟"，但不认识 `AgentManager`。
//!
//! ## 为什么要这一层
//!
//! 改造前 `Agent` 持 `manager: Weak<AgentManager>`，17 处调用点写
//! `if let Some(mgr) = self.manager() { … }`——**未绑定时静默跳过**。2026-10-20
//! 的两次漏绑（`__core` 与运行期新建人格）就是这么藏起来的：Panel 收到空
//! `teams: []`，日志里只有一行修复前加的 `manager_warned` 警告。
//!
//! 那一版用一个很漂亮的技巧补上了绑定纪律：`RunningAgent` 的字段私有 + 唯一
//! 构造入口 `bound()`，让"必须绑定"成为类型约束。**这一点保留不动**。
//!
//! 本模块解决的是另一半：**`Agent` 的类型面里不该出现 `AgentManager`**。
//! 于是把它压成一个只读 trait：
//!
//! - [`AgentRegistry::resolve`] —— 按 `team_id` 找兄弟（命令转发 / 跨人格操作）；
//! - [`AgentRegistry::all`] —— 列全部运行中人格（广播类命令）；
//! - [`AgentRegistry::infos`] —— 面板要的人格清单。
//!
//! 团队**管理动作**（保存 / 删除 / 启停 / profile 改名传播）不在这里：它们已经
//! 搬回 [`crate::agent_manager`] 的自由函数，`Agent` 不再持有那条能力
//! （见该模块 `handle_*_command` 的文档）。
//!
//! ## 为什么 `all()` 返回 [`RunningAgent`]
//!
//! 调用点要 `id` / `profile` / `agent` 三样（如"重载技能要覆盖所有人格"）。
//! `RunningAgent` 刻意留在 `crate::agent_manager`：它的 `_sealed` 私有字段是
//! "构造即绑定"的类型约束，搬走就得把构造器放宽成 `pub(crate)`，那条约束会
//! 从"编译期强制"退化成"注释约定"。**为了消掉一个类型名而削弱一条真实约束
//! 不划算**——所以这里只做 `use`，不做搬迁。

use std::sync::Arc;

use crate::agent::Agent;
use crate::agent_manager::RunningAgent;
use crate::config::TeamMember;
use echo_protocol::TeamInfo;

/// agent 集合的只读查询面（宿主注入；`AgentManager` 是唯一实现）。
///
/// 实现方必须保证：`resolve` 与 `all` 看到的是**同一份快照语义**——否则
/// "转发给 target"与"广播给 all"会命中不同集合，产生难以复现的行为差异。
pub trait AgentRegistry: Send + Sync {
    /// 按 `team_id` 找运行中的 agent（`None` = 该人格不存在或已禁用）。
    fn resolve(&self, team_id: &str) -> Option<Arc<Agent>>;

    /// 全部运行中的人格（顺序 = 注册序，调用方不应依赖字典序）。
    fn all(&self) -> Vec<RunningAgent>;

    /// 面板用的人格清单（含已禁用的——前端要能显示并重新启用）。
    fn infos(&self) -> Vec<TeamInfo>;
}

/// 团队**管理动作**（宿主注入；与只读查询面分开定义）。
///
/// 为什么单独一个 trait：这两件事的**关注点不同**——查询是"我看看兄弟们在
/// 干什么"，管理是"增删改人格"。合成一个 trait 等于把 `AgentManager` 的方法
/// 表原样搬过来，`Agent` 侧对管理的依赖一点没少。
///
/// **这是技术债，不是终态**：团队 CRUD 命令（`SaveTeam` / `DeleteTeam` /
/// `ToggleTeam`）本该由**进程级**处理器接掉，而不是让某个"恰好被命令泵选中的
/// agent"代办。那一步需要把 `BackendCommand` 的这几个变体从 `apply_command`
/// 的分支里迁到组合根（与 S1 的 `Contribution::Command` 同一套路），风险与
/// 工作量都独立。分成独立 trait 之后，那一步只要删掉本 trait 与 5 个调用点，
/// 编译器会把漏改的地方全部点出来。
pub trait TeamAdmin: Send + Sync {
    /// 保存���新建或覆盖）一个人格定义。返回 `Err` = 校验失败。
    ///
    /// 接收者是 `Arc<Self>` 而不是 `&self`：新建人格时要把 manager 自己交给
    /// 新 agent 做绑定，`&self` 拿不到 `Arc`。`Arc<Self>` 是**对象安全**的
    /// 接收者（稳定 Rust 支持），所以 `Arc<dyn TeamAdmin>` 上照常可调。
    fn save_profile(
        self: Arc<Self>,
        id: &str,
        profile: TeamMember,
        enabled: bool,
    ) -> Result<(), String>;

    /// 删除一个人格。
    fn delete_profile(&self, id: &str) -> Result<(), String>;

    /// 启用 / 禁用一个已存在的人格（禁用会卸下实例、启用会经工厂重建）。
    fn set_enabled(
        self: Arc<Self>,
        id: &str,
        enabled: bool,
        factory: crate::agent_manager::AgentFactory,
    ) -> Result<(), String>;

    /// 取重建实例用的工厂（`None` = 未注入，调用方应报可读错误）。
    fn factory_ref(&self) -> Option<crate::agent_manager::AgentFactory>;

    /// 取一个运行中的人格（含 profile）。
    fn get_running(&self, id: &str) -> Option<RunningAgent>;

    /// 哪些人格引用了某个 API profile 名。
    fn api_ref_owners(&self, name: &str) -> Vec<String>;

    /// profile 改名后传播引用（返回受影响的人数）。
    fn replace_api_refs(&self, from: &str, to: &str) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 最小 fake：证明"查询面"可以脱离 `AgentManager` 独立实现。
    ///
    /// 这是 S4b 的核心收益——改造前测一条"转发给兄弟 persona"的命令必须造
    /// 一整个 `AgentManager`（连工厂、持久化、禁用表一起）；现在只要一个能
    /// 回答三个问题的替身。
    struct FakeRegistry {
        agents: Vec<Arc<Agent>>,
        lookups: Arc<AtomicUsize>,
    }

    impl AgentRegistry for FakeRegistry {
        fn resolve(&self, team_id: &str) -> Option<Arc<Agent>> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            self.agents
                .iter()
                .find(|agent| agent.team_id().as_deref() == Some(team_id))
                .cloned()
        }

        fn all(&self) -> Vec<RunningAgent> {
            // fake 不构造 `RunningAgent`（它的构造被 `_sealed` 封住，只有
            // `AgentManager::bound` 能造）——这正是那条约束还在起作用的证据。
            Vec::new()
        }

        fn infos(&self) -> Vec<TeamInfo> {
            Vec::new()
        }
    }

    /// 只用 fake 就能测"管理面不可用"的路径：`TeamAdmin` 的替身什么都不做。
    struct FakeAdmin;

    impl TeamAdmin for FakeAdmin {
        fn save_profile(
            self: Arc<Self>,
            _id: &str,
            _profile: TeamMember,
            _enabled: bool,
        ) -> Result<(), String> {
            Ok(())
        }
        fn delete_profile(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        fn set_enabled(
            self: Arc<Self>,
            _id: &str,
            _enabled: bool,
            _factory: crate::agent_manager::AgentFactory,
        ) -> Result<(), String> {
            Ok(())
        }
        fn factory_ref(&self) -> Option<crate::agent_manager::AgentFactory> {
            None
        }
        fn get_running(&self, _id: &str) -> Option<RunningAgent> {
            None
        }
        fn api_ref_owners(&self, _name: &str) -> Vec<String> {
            Vec::new()
        }
        fn replace_api_refs(&self, _from: &str, _to: &str) -> usize {
            0
        }
    }

    fn make_agent(team: &str) -> Arc<Agent> {
        use crate::llm::LlmProvider;

        struct Noop;
        #[async_trait::async_trait]
        impl LlmProvider for Noop {
            fn name(&self) -> &str {
                "noop"
            }
            fn default_model(&self) -> &str {
                "m"
            }
            async fn chat(
                &self,
                _request: &crate::llm::ChatRequest,
            ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
                Err(crate::llm::LlmError::Api("unused".into()))
            }
            async fn chat_stream(
                &self,
                _request: &crate::llm::ChatRequest,
                _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
            ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
                Err(crate::llm::LlmError::Api("unused".into()))
            }
        }

        let agent = Arc::new(Agent::new(
            Arc::new(Noop),
            crate::config::AgentConfig::default(),
            crate::skill::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::plugin_instance::InstanceRegistry::new()),
        ));
        agent.set_team_id(Some(team.to_string()));
        agent
    }

    /// 未绑定时 `require_registry()` 是**明确错误**（fail-closed），
    /// 而不是静默跳过——这是与旧 `manager()` 的关键差别。
    #[tokio::test]
    async fn unbound_agent_reports_an_explicit_error() {
        let agent = make_agent("a");
        let err = agent.require_registry().err().expect("未绑定应报错");
        assert!(err.contains("registry unavailable"), "{err}");
        assert!(agent.registry().is_none());
        assert!(agent.require_admin().is_err(), "管理面同样 fail-closed");
    }

    /// 绑上 fake 之后能按 team_id 找到兄弟（不经过 `AgentManager`）。
    #[tokio::test]
    async fn bound_agent_resolves_siblings_through_the_trait() {
        let a = make_agent("a");
        let b = make_agent("b");

        let lookups = Arc::new(AtomicUsize::new(0));
        let registry: Arc<dyn AgentRegistry> = Arc::new(FakeRegistry {
            agents: vec![Arc::clone(&a), Arc::clone(&b)],
            lookups: lookups.clone(),
        });
        let admin: Arc<dyn TeamAdmin> = Arc::new(FakeAdmin);
        a.set_host(&registry, &admin);

        assert!(Arc::ptr_eq(
            &a.require_registry().unwrap().resolve("b").expect("找到 b"),
            &b
        ));
        assert!(a.require_registry().unwrap().resolve("ghost").is_none());
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
        // 管理面独立可绑：fake registry + fake admin（互不依赖）
        assert!(a.require_admin().is_ok());
    }
}
