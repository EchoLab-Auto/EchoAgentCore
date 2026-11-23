//! AgentManager — 多 agent 人格实例管理与命令路由。
//!
//! Core 启动时根据 `[agent.profiles]` 为每个启用的人格实例化一个独立
//! `Agent`（独立 trunk / 会话日志 / 系统提示词）。前端 `SendMessage` 带
//! `agent_id` 时路由到对应实例；缺省走 default。`SessionInfo.agent_id`
//! 由各 Agent 在自身 `info()` 中填充（无需在此标注事件）。

use std::collections::HashMap;
use std::sync::Arc;

use agent_module_management::protocol::TeamInfo;

use crate::agent::Agent;
use crate::config::{AgentConfig, TeamMember};

/// A running persona: profile + its agent instance.
///
/// # 构造是封住的（2026-10-20 事故的根治）
///
/// 字段可读（调用方大量读取 `id` / `profile` / `agent`），但**不能从外部
/// 直接构造**——`_sealed` 是私有字段，因此模块外写不出 `RunningAgent { .. }`。
/// 唯一入口是 [`RunningAgent::bound`]，它在构造时**强制**把 manager 弱引用
/// 绑到 agent 上。
///
/// 为什么值得这样一个技巧：绑定漏了不会 panic、不会报错，只会让
/// `TeamsList` / 命令路由**静默降级为空**——2026-10-20 的两次漏绑
/// （`__core`、运行期新建人格）都是这样藏了很久。把"必须绑定"变成类型
/// 约束，比在调用点补 `set_manager` 靠得住：以后新增创建路径时，
/// 不经过 `bound` 就编译不过。
// 不用 clippy 建议的 `#[non_exhaustive]`：那个属性只封住 **crate 外** 的构造，
// 而这里的 `_sealed` 还要封住本 crate 其它模块——构造点必须只剩下面这两条
// （`bound` 与 `AgentManager::new` 的兜底），见类型文档里的事故复盘。
#[allow(clippy::manual_non_exhaustive)]
#[derive(Clone)]
pub struct RunningAgent {
    pub id: String,
    pub profile: TeamMember,
    pub agent: Arc<Agent>,
    /// 私有标记：封住结构体字面量构造（见类型文档）。恒为 `()`。
    _sealed: (),
}

impl RunningAgent {
    /// **唯一构造入口**：构造即绑定 manager。
    ///
    /// 内部做两件事，顺序不可换：先把 manager 弱引用写给 agent，再返回结构体。
    /// 调用方拿到的一定是"已绑定"的运行实例。
    fn bound(
        manager: &std::sync::Arc<AgentManager>,
        id: String,
        profile: TeamMember,
        agent: Arc<Agent>,
    ) -> Self {
        // 绑定**收窄后的查询面**（`AgentRegistry`），不是 manager 本身：
        // `Agent` 只需要"找兄弟 / 列全部 / 报清单"这三件事（S4b）。
        // 管理面（团队 CRUD）不经 `Agent`——它由 `process_commands` 在命令泵
        // 里直接处理（S4c 的前置：`Agent.admin` 已删除）。
        let registry: Arc<dyn crate::agent::registry::AgentRegistry> = manager.clone();
        agent.set_registry(&registry);
        // 构造**不**启动后台任务（2026-11 C 复审）：这里还在装配半路（loop
        // runner / 能力门控 / 配置注入都可能在后面），起来就是"半代写者"。
        // 启动时机是显式的：组合根在最终配置之后调 `start_background_tasks`，
        // 运行期重建由 `process_commands` 在应用能力之后调。
        Self {
            id,
            profile,
            agent,
            _sealed: (),
        }
    }
}

impl std::fmt::Debug for RunningAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningAgent")
            .field("id", &self.id)
            .field("name", &self.profile.name)
            .finish()
    }
}

/// Owns all persona agents and routes frontend commands to the right one.
/// 归一 + 去重 + 去空串：把用户写的名单变成消费方可直接查的形态。
///
/// **模块 id 走 `canonical_module_id`**（旧实例前缀归一为现役形态），工具名
/// 无归一规则、原样去重即可。两者共用这个函数是因为"清理噪音"这部分一样：
/// 配置里手写的空串与重复项不该变成前端的行为差异。
///
/// 顺序保持首次出现的位置（`Vec` 而非 `BTreeSet`）：名单顺序对前端无意义，
/// 但保持稳定能让快照对比不被无谓重排干扰。
fn dedup_non_empty<'a>(items: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in items {
        let id = crate::modules::canonical_module_id(raw.trim());
        if id.is_empty() || out.contains(&id) {
            continue;
        }
        out.push(id);
    }
    out
}

pub struct AgentManager {
    default_id: String,
    /// All team member definitions (including disabled ones), for rebuild.
    teams: parking_lot::RwLock<HashMap<String, TeamMember>>,
    /// Running instances（**纯运行态**：`agents` 里有没有 = 现在跑不跑）。
    agents: parking_lot::RwLock<HashMap<String, RunningAgent>>,
    /// Persistence hook: writes the profiles map back to TOML (or any store).
    config_writer: parking_lot::Mutex<Option<ConfigWriter>>,
    /// 本节点身份（NodeId + 运行区域名）：`infos()` 把它写进每条 `TeamInfo`，
    /// 供多节点聚合客户端区分同名人格。组合根装配期注入（P3 终态：参数，
    /// 非 kernel cell）。
    node_identity: parking_lot::RwLock<crate::NodeIdentity>,
    /// 人格重建工厂（组合根在 manager 构造完成后注入）：运行期启用被禁用
    /// 的人格时用它重建实例，与启动期同一装配闭包。
    ///
    /// 2026-10 技术债清理：此前是 `echo_bootstrap` 进程级槽
    /// （`set_agent_factory` / `agent_factory_for_toggle`）。它天然属于
    /// **某个** manager——现在直接挂在它自己身上，两套 manager 各持各的。
    /// None = 未注入（toggle 返回明确错误，而不是 panic）。
    /// None = 未注入（toggle 返回明确错误，而不是 panic）。
    factory: parking_lot::RwLock<Option<AgentFactoryInner>>,
    /// **已下线、收尾还没跑完**的实例（team id → 共享完成状态）。
    ///
    /// 禁用/删除是"摘实例 + 停后台任务 + 等 turn 排空 + 最终保存"：第一件
    /// 同步（`Agent::begin_retire`，绝不持锁 await），后面是异步的
    /// （`Agent::finish_retire`）。收尾任务登记在这里，重新启用前
    /// [`AgentManager::await_retirement`] 先等它——否则新一代会和上一代的
    /// 最终落盘交错（旧代后落地 = 把新代的数据盖回旧快照）。
    ///
    /// 值不是裸 `JoinHandle`（2026-11 C 复审）：等待者要 await 就得把句柄
    /// **移走**，等待被取消时句柄随之 drop → 任务 detach、第二个等待者还会
    /// 看到"没有在册收尾"而提前返回。现在值是一份共享完成状态
    /// （[`RetirementEntry`]）：等待者只订阅不移除，任务独立跑完；完成可清理
    /// （`Done` 在下一次访问时摘掉），失败**保留**（继续挡住重建）。
    retiring: parking_lot::Mutex<HashMap<String, Arc<RetirementEntry>>>,
    /// 生命周期串行闸（**锁序：lifecycle → agents / retiring**）。
    ///
    /// 把"摘实例 → `begin_retire` → 登记 retirement"做成一个不可插入的
    /// 临界区：否则 `set_enabled(false)` 刚把实例从 `agents` 摘掉、还没登记
    /// 收尾的那一瞬，另一个 `set_enabled(true)` 会看到"既没运行实例、也没有
    /// 在册收尾"，直接重建出第二代——两代写者（2026-11 C 复审）。
    lifecycle: parking_lot::Mutex<()>,
}

/// 工厂闭包的内部类型别名（对外仍用 [`AgentFactory`] 包装）。
type AgentFactoryInner = std::sync::Arc<dyn Fn(String, TeamMember) -> Arc<Agent> + Send + Sync>;

/// 一次人格退场的**共享完成状态**（多等待者 / 等待被取消都安全）。
struct RetirementEntry {
    state: parking_lot::Mutex<RetirementState>,
    done: tokio::sync::Notify,
}

/// 收尾的三种状态。
#[derive(Clone, Debug)]
enum RetirementState {
    /// 收尾任务还在跑（停后台任务 / 等 turn 排空 / 最终保存）。
    Running,
    /// 收尾完成：可以重建下一代。
    Done,
    /// 收尾**未完成**（turn 排不干净 / 后台任务 abort 后仍不退）。
    ///
    /// 登记**保留在册**：这就是"保持 retirement 不完成以禁止新代"的落点。
    Failed(String),
}

impl RetirementEntry {
    fn new() -> Self {
        Self {
            state: parking_lot::Mutex::new(RetirementState::Running),
            done: tokio::sync::Notify::new(),
        }
    }

    fn snapshot(&self) -> RetirementState {
        // parking_lot 不中毒：锁只保护一个三态枚举，不会有"半改"的中间态。
        self.state.lock().clone()
    }

    /// 收尾有结论：写状态并唤醒**所有**等待者。
    fn finish(&self, result: Result<(), String>) {
        {
            let mut state = self.state.lock();
            *state = match result {
                Ok(()) => RetirementState::Done,
                Err(error) => RetirementState::Failed(error),
            };
        }
        self.done.notify_waiters();
    }

    /// 等这次收尾有结论（`Running` → `Done` / `Failed`）。
    ///
    /// 等待者只订阅、不移除：**等待被取消不影响收尾任务**，其他等待者照常
    /// 等到同一个结论（2026-11 C 复审）。
    async fn wait(&self) -> Result<(), String> {
        loop {
            // 先注册再查状态，避免"查到 Running、通知刚好发完"的丢唤醒。
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.snapshot() {
                RetirementState::Running => notified.await,
                RetirementState::Done => return Ok(()),
                RetirementState::Failed(error) => return Err(error),
            }
        }
    }
}

/// Closure that persists the profiles map (composition root injects the
/// ConfigStore patch writer).
pub type ConfigWriter = Box<
    dyn Fn(&std::collections::BTreeMap<String, TeamMember>) -> Result<(), String> + Send + Sync,
>;

/// `AgentManager` 是 [`AgentRegistry`] 的**唯一实现**：agent 侧的"找兄弟 /
/// 列全部 / 报清单"全部经这个只读面（S4b）。
///
/// 为什么 trait 而不是让 `Agent` 直接用 `AgentManager`：`Agent` 的类型面里
/// 不该出现 manager——否则单测只能靠造一个 manager 才能测命令转发，且"新增
/// 一处 `self.manager().xxx()`"永远不会被察觉。收成 trait 之后，那类调用只能
/// 走 `require_registry()`（失败即显式错误），无法悄悄加回静默降级。
impl crate::agent::registry::AgentRegistry for AgentManager {
    fn resolve(&self, team_id: &str) -> Option<Arc<Agent>> {
        AgentManager::resolve(self, Some(team_id))
    }

    fn all(&self) -> Vec<RunningAgent> {
        AgentManager::all(self)
    }

    fn infos(&self) -> Vec<TeamInfo> {
        AgentManager::infos(self)
    }
}

/// 团队管理面（见 [`crate::agent::registry::TeamAdmin`] 的"技术债"说明）。
impl crate::agent::registry::TeamAdmin for AgentManager {
    fn save_profile(self: Arc<Self>, id: &str, profile: TeamMember) -> Result<(), String> {
        AgentManager::save_profile(&self, id, profile)
    }

    fn delete_profile(&self, id: &str) -> Result<(), String> {
        AgentManager::delete_profile(self, id)
    }

    fn set_enabled(
        self: Arc<Self>,
        id: &str,
        enabled: bool,
        factory: AgentFactory,
    ) -> Result<(), String> {
        AgentManager::set_enabled(&self, id, enabled, move |i, p| factory.call(i, p))
    }

    fn factory_ref(&self) -> Option<AgentFactory> {
        AgentManager::factory_ref(self)
    }

    fn get_running(&self, id: &str) -> Option<RunningAgent> {
        AgentManager::get(self, id)
    }

    fn await_retirement<'a>(
        &'a self,
        id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        // 收尾是异步的（停后台任务 + 等 turn 排空 + 最终保存）：调用方先等它，
        // 再重建。`Err` = 上一代没退干净（caller 必须放弃这次重建）。
        Box::pin(AgentManager::await_retirement(self, id))
    }

    fn api_ref_owners(&self, name: &str) -> Vec<String> {
        AgentManager::api_ref_owners(self, name)
    }

    fn replace_api_refs(&self, from: &str, to: &str) -> usize {
        AgentManager::replace_api_refs(self, from, to)
    }
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("default_id", &self.default_id)
            .field("agents", &self.agent_ids())
            .finish()
    }
}

impl AgentManager {
    /// Build the manager from config profiles + an agent factory.
    ///
    /// `factory(id, profile)` constructs the Agent for a persona (the
    /// composition root wires provider/config/session paths there).
    pub fn build(raw: &AgentConfig, factory: impl Fn(String, TeamMember) -> Arc<Agent>) -> Self {
        let mut profiles: HashMap<String, TeamMember> = HashMap::new();
        for (id, profile) in &raw.teams {
            let mut p = profile.clone();
            if p.name.is_empty() {
                p.name = id.clone();
            }
            // 旧配置迁移（2026-11）：`disabled_teams` 里的 id → `enabled = false`。
            // 两个字段此前表达同一件事（判定是 `!enabled || disabled.contains`），
            // 现在单轨到 `enabled`；旧文件写过的名单在这里折进去，行为不变。
            if raw.disabled_teams.iter().any(|disabled| disabled == id) {
                p.enabled = false;
            }
            profiles.insert(id.clone(), p);
        }
        // 未配置人格 = 零人格（2026-10 用户请求）：此前 `teams` 为空会合成
        // 一个 legacy "default" 人格——每台新节点/新装 Core 都会凭空多出
        // 一个"默认" agent（`agents=["default"]`），与"去主智能体"语义
        // （无默认兜底、命令须显式 team_id）相悖。现在空配置就是没有智能体，
        // 由 Panel「设置 → 智能体 → 新建智能体」（或手写 `[agent.teams.<id>]`）
        // 按需创建。
        let teams = parking_lot::RwLock::new(profiles);
        let mut agents = HashMap::new();
        for (id, profile) in teams.read().iter() {
            // 单轨：跑不跑只看 `enabled`（旧 `disabled_teams` 已在上面折进来）。
            if !profile.enabled {
                continue;
            }
            let p = profile.clone();
            // `build` 时 manager 尚未构造完成（`Self` 正在建），无法给出
            // `&Arc<Self>`。这里先构造、由 `AgentManager::new` 之后统一
            // `bind_all` 兜底——该函数是唯一能把"未绑定"变成"已绑定"的
            // 地方，且 `_sealed` 保证外部无法绕过。
            let agent = factory(id.clone(), p.clone());
            // 后台任务不在这里起（见 `RunningAgent::bound` 的注释）：
            // 启动期由组合根在 `apply_capabilities` / `set_loop_runner` 之后统一
            // 调 `start_background_tasks`（2026-11 C 复审：最终配置之后才起）。
            // 这里**不能**走 `RunningAgent::bound`：`Self` 正在构造中，
            // 拿不到 `&Arc<Self>`。改由 `AgentManager::bind_all` 在 Arc
            // 建立之后统一补绑（唯一能把"未绑定"变"已绑定"的地方）。
            agents.insert(
                id.clone(),
                RunningAgent {
                    id: id.clone(),
                    profile: p,
                    agent,
                    _sealed: (),
                },
            );
        }
        let default_id = agents
            .keys()
            .min()
            .cloned()
            .unwrap_or_else(|| "default".into());
        Self {
            default_id,
            teams,
            agents: parking_lot::RwLock::new(agents),
            config_writer: parking_lot::Mutex::new(None),
            node_identity: parking_lot::RwLock::new(crate::NodeIdentity::default()),
            factory: parking_lot::RwLock::new(None),
            retiring: parking_lot::Mutex::new(HashMap::new()),
            lifecycle: parking_lot::Mutex::new(()),
        }
    }

    /// 注入本节点身份（组合根装配期，一次）。`infos()` 的每条 `TeamInfo`
    /// 都会带上它。
    pub fn set_node_identity(&self, identity: crate::NodeIdentity) {
        *self.node_identity.write() = identity;
    }

    pub fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.agents.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// All known profile ids (running + disabled).
    pub fn team_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.read().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn team(&self, id: &str) -> Option<TeamMember> {
        self.teams.read().get(id).cloned()
    }

    /// Resolve the target agent for a command.
    ///
    /// 去主智能体（2026-09）：`None` 不再回退"默认人格"——返回 `None`，
    /// 由调用方给出明确错误；未知 id 同样返回 `None`（不再静默兜底）。
    pub fn resolve(&self, agent_id: Option<&str>) -> Option<Arc<Agent>> {
        let id = agent_id?;
        self.agents.read().get(id).map(|r| Arc::clone(&r.agent))
    }

    pub fn get(&self, id: &str) -> Option<RunningAgent> {
        self.agents.read().get(id).cloned()
    }

    pub fn all(&self) -> Vec<RunningAgent> {
        let mut list: Vec<RunningAgent> = self.agents.read().values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 引用 `api_profile == name` 的全部 team id（含未运行/禁用；升序）。
    /// API profile 改名时用于计算落盘补丁的改写范围。
    pub fn api_ref_owners(&self, name: &str) -> Vec<String> {
        let teams = self.teams.read();
        let mut ids: Vec<String> = teams
            .iter()
            .filter(|(_, t)| t.api_profile.as_deref() == Some(name))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// API profile 改名传播（内存侧）：把 teams 定义与运行中 persona 快照的
    /// `api_profile` 引用从 `from` 改为 `to`，返回 teams 改写数。
    ///
    /// 只做内存改写——持久化由调用方在同一次 ConfigStore patch 内完成；
    /// 不触发启停（区别于 [`Self::save_profile`] 的建销副作用）。
    pub fn replace_api_refs(&self, from: &str, to: &str) -> usize {
        let mut changed = 0usize;
        {
            let mut teams = self.teams.write();
            for t in teams.values_mut() {
                if t.api_profile.as_deref() == Some(from) {
                    t.api_profile = Some(to.to_string());
                    changed += 1;
                }
            }
        }
        {
            let mut agents = self.agents.write();
            for running in agents.values_mut() {
                if running.profile.api_profile.as_deref() == Some(from) {
                    running.profile.api_profile = Some(to.to_string());
                }
            }
        }
        changed
    }

    /// Runtime toggle. Disabling drops the manager's strong ref (agent memory
    /// unloads once last ref drops); enabling re-instantiates via factory.
    ///
    /// **入口自保（2026-11 C 复审）**：`enabled = true` 时先查这个 id 的上一代
    /// 是否还在收尾——在收尾就明确返回 `Err`（生产上层照旧先 `await_retirement`
    /// 再调，这里兜住"调用方没等"的直接调用，避免重建出两代写者）。检查和
    /// 重建在同一把 [`Self::lifecycle`] 闸内，`remove → begin_retire → 登记`
    /// 之间没有可插入的窗口。
    pub fn set_enabled(
        self: &std::sync::Arc<Self>,
        id: &str,
        enabled: bool,
        factory: impl Fn(String, TeamMember) -> Arc<Agent>,
    ) -> Result<(), String> {
        if !self.teams.read().contains_key(id) {
            return Err(format!("agent not found: {id}"));
        }
        // 锁内只做内存增删 + 收尾登记（`begin_retire` 是同步的，绝不 await）。
        {
            let _lifecycle = self.lifecycle.lock();
            if enabled {
                // 上一代没退干净就不许重建（不能只靠上层先 await）。
                self.warn_if_retiring(id);
                if let Some(reason) = self.retirement_barrier_locked(id) {
                    return Err(format!(
                        "agent {id} 的上一代还在收尾，暂不能重建，请稍后重试（{reason}）"
                    ));
                }
                let mut agents = self.agents.write();
                if !agents.contains_key(id) {
                    let profile = self
                        .teams
                        .read()
                        .get(id)
                        .cloned()
                        .ok_or_else(|| format!("agent not found: {id}"))?;
                    let agent = factory(id.to_string(), profile.clone());
                    // 走唯一入口：构造即绑定 manager（后台任务由调用方在
                    // 最终配置之后显式启动，见 `start_background_tasks`）。
                    let running = RunningAgent::bound(self, id.to_string(), profile, agent);
                    agents.insert(id.to_string(), running);
                }
            } else {
                // 卸下实例 = 释放它持有的上下文投影；磁盘上的会话文件不动
                // （收尾只补一次最终保存）。摘实例与收尾登记同一临界区。
                self.detach_and_retire_locked(id);
            }
        }
        // 单轨（2026-11）：`enabled` 就是唯一事实——配置里一份、运行时 map
        // 一份，两者不再需要第三个列表来"保持同步"。
        if let Some(profile) = self.teams.write().get_mut(id) {
            profile.enabled = enabled;
        }
        self.persist_profiles()?;
        Ok(())
    }

    /// 给注册表内**全部**运行实例补绑 manager 弱引用。
    ///
    /// 只在 `Arc<AgentManager>` 建立之后调用一次（`build` 阶段拿不到 Arc）。
    /// 这是"未绑定 → 已绑定"的唯一通道；`RunningAgent::bound` 覆盖其余
    /// 创建路径（`set_enabled` / `save_profile`）。
    pub fn bind_all(&self, manager: &std::sync::Arc<Self>) {
        // S4b：绑定收窄后的查询面（管理面不经 Agent，见 `process_commands`）。
        let registry: std::sync::Arc<dyn crate::agent::registry::AgentRegistry> = manager.clone();
        for running in self.agents.read().values() {
            running.agent.set_registry(&registry);
        }
    }

    // ── 下线收尾（2026-11 C）────────────────────────────────────────────

    /// 收尾登记的**统一清理**：`Done` 摘掉，`Running` / `Failed` 保留。
    ///
    /// 完成可清理（等待者各自持有 `Arc<RetirementEntry>`，清理不影响他们）；
    /// 失败保留——它就是"禁止新代"的那把锁。
    fn prune_retirements_locked(retiring: &mut HashMap<String, Arc<RetirementEntry>>) {
        retiring.retain(|_, entry| !matches!(entry.snapshot(), RetirementState::Done));
    }

    /// 有收尾在飞时**大声**记一笔（2026-11 C；复审后升级为硬闸）。
    ///
    /// 正确的顺序是"先 `await_retirement` 再重建"（`process_commands` 已照做）。
    /// 复审后这里不再只是日志：`set_enabled` / `save_profile` 会直接**拒绝**
    /// 重建（见 [`Self::retirement_barrier_locked`]）——两代写者的代价比一次
    /// 可重试的失败大。这条日志留给"上层绕过等待、但被入口闸挡住"的路径，
    /// 便于定位是哪个调用方没等。
    fn warn_if_retiring(&self, id: &str) {
        if self.retiring.lock().contains_key(id) {
            tracing::warn!(
                team = %id,
                "persona rebuild blocked: previous generation is still retiring; \
                 callers should await_retirement first"
            );
        }
    }

    /// 这个 id 上一代是否还挡着重启（`Running` / `Failed`）。
    ///
    /// 调用方必须持 [`Self::lifecycle`] 闸（与摘实例同一临界区）。
    fn retirement_barrier_locked(&self, id: &str) -> Option<String> {
        let mut retiring = self.retiring.lock();
        Self::prune_retirements_locked(&mut retiring);
        match retiring.get(id).map(|entry| entry.snapshot()) {
            Some(RetirementState::Running) => Some("上一代收尾进行中".into()),
            Some(RetirementState::Failed(error)) => Some(format!("上一代收尾未完成：{error}")),
            _ => None,
        }
    }

    /// **摘实例并登记收尾**（禁用 / 删除的唯一路径）。
    ///
    /// 调用方**必须**持 [`Self::lifecycle`] 闸：摘实例与登记在同一临界区，
    /// 外面看不到"已摘掉、还没登记"的窗口（那正是 `set_enabled(true)` 会
    /// 插进来重建第二代的时机）。
    ///
    /// 同步部分（绝不 await）：
    /// - 从 `agents` 摘掉 → 命令路由立即解析不到（不接新输入）；
    /// - [`Agent::begin_retire`]：置 retired/draining、取消在飞 turn 与后台任务；
    /// - 在 `retiring` 里登记共享完成状态。
    ///
    /// 异步部分（`Agent::finish_retire`：停后台任务 → 等 turn 排空 → 最终保存）
    /// 由独立任务执行，结论写回登记：`Done` 可重建、`Failed` 继续挡住重建
    /// （不允许"猜它跑完了"）。
    fn detach_and_retire_locked(&self, id: &str) -> Option<RunningAgent> {
        let removed = self.agents.write().remove(id);
        if let Some(running) = &removed {
            self.begin_retirement_locked(id, Arc::clone(&running.agent));
        }
        removed
    }

    /// 拉起收尾任务并登记（调用方持 [`Self::lifecycle`] 闸）。
    fn begin_retirement_locked(&self, id: &str, agent: Arc<Agent>) {
        if agent.is_retired() {
            return;
        }
        // 同步：这一代立刻不再接新输入、不再周期落盘（不持锁、不 await）。
        agent.begin_retire();
        let entry = Arc::new(RetirementEntry::new());
        {
            let mut retiring = self.retiring.lock();
            Self::prune_retirements_locked(&mut retiring);
            retiring.insert(id.to_string(), Arc::clone(&entry));
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // 没有运行时（纯同步装配 / 极端测试环境）：同步部分已经生效
            // （不接新输入、后台任务收到取消）。登记**保持 Failed**：不能
            // 假装收完，否则下一次重建会和旧代的两代写者撞上。
            tracing::warn!(
                team = %id,
                "no tokio runtime; persona retirement cannot finish (rebuild stays blocked)"
            );
            entry.finish(Err("没有 tokio 运行时，收尾无法完成".into()));
            return;
        };
        let team = id.to_string();
        runtime.spawn(async move {
            let result = agent.finish_retire().await;
            match &result {
                Ok(()) => tracing::info!(team = %team, "persona retirement finished"),
                Err(error) => tracing::error!(
                    team = %team,
                    %error,
                    "persona retirement incomplete: rebuild for this id stays blocked"
                ),
            }
            // 先写结论再唤醒等待者：`Done` 的登记由下一次访问清理。
            entry.finish(result);
        });
    }

    /// 等某个 id **上一代**的下线收尾跑完（禁用→启用之间必须先等）。
    ///
    /// 没有在册收尾 = 立即返回。**等待者不移除登记**：多个等待者拿到同一份
    /// 完成状态，任何一个被取消（future drop）都不影响收尾任务与其他等待者
    /// （2026-11 C 复审：旧实现 `remove` + `await JoinHandle`，取消即 detach，
    /// 第二个等待者还会提前返回）。
    ///
    /// 返回 `Err` = 上一代**没收干净**（turn 排不干净 / 后台任务停不下来）。
    /// 调用方必须放弃这次重建——`set_enabled` / `save_profile` 也会再挡一次。
    pub async fn await_retirement(&self, id: &str) -> Result<(), String> {
        let entry = {
            let mut retiring = self.retiring.lock();
            Self::prune_retirements_locked(&mut retiring);
            retiring.get(id).cloned()
        };
        match entry {
            Some(entry) => entry.wait().await,
            None => Ok(()),
        }
    }

    /// **所有**在册收尾（含已从 `agents` 摘掉、不在 supervisor 名单里的人）。
    ///
    /// 进程退出必须用它：只遍历"在册 persona"会漏掉运行期被禁用/删除的实例
    /// ——它们的最终保存在退出时可能还在跑，进程一走就丢（2026-11 C 复审）。
    ///
    /// 返回仍未成功的 `id: 原因`（**按 id 稳定排序、去重**；空 = 全干净）。
    /// `Running` 的会等，`Failed` 的如实报出来（不再等待——它不会自己变好）。
    ///
    /// **`Failed` 也算"没退干净"（2026-12 小修）**：旧实现只收 `Running`，
    /// 早已 `Failed` 的登记被漏掉、当成干净——进程升级据此判定"可以走了"，
    /// 实际那一次最终保存根本没落盘。现在 `Running` + `Failed` 一并汇总；
    /// `Failed` **不移除**（按设计保留在册继续挡重建），连续多次调用结果一致。
    ///
    /// 锁序不变：只在取快照的临界区里拿 `retiring`（`lifecycle → agents /
    /// retiring` 的下游）；**不**持 `lifecycle` 快照（也不能跨 await 持锁）。
    pub async fn await_all_retirements(&self) -> Vec<String> {
        // BTreeSet = 稳定排序 + 去重：HashMap 的迭代序不定，两次调用必须给出
        // 一致结果；同一个 id 也可能在"等到结论"与"下一轮快照"里各见一次。
        let mut failed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        loop {
            let pending: Vec<(String, Arc<RetirementEntry>)> = {
                let mut retiring = self.retiring.lock();
                Self::prune_retirements_locked(&mut retiring);
                retiring
                    .iter()
                    // `Done` 已由 prune 摘掉；这里收 `Running` + `Failed`。
                    .filter(|(_, entry)| !matches!(entry.snapshot(), RetirementState::Done))
                    .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
                    .collect()
            };
            if pending.is_empty() {
                break;
            }
            let mut saw_running = false;
            for (id, entry) in pending {
                match entry.snapshot() {
                    // 早已 Failed：结论不会再变，直接汇总（不移除登记）。
                    RetirementState::Failed(error) => {
                        failed.insert(format!("{id}: {error}"));
                    }
                    // 快照之后才收尾完的：`wait` 立刻给出同一个结论。
                    RetirementState::Done => {}
                    RetirementState::Running => {
                        saw_running = true;
                        if let Err(error) = entry.wait().await {
                            failed.insert(format!("{id}: {error}"));
                        }
                    }
                }
            }
            // 本轮没有 Running = 剩下的全是"不会自己变好"的 Failed；再转一圈
            // 只会原地打转（Failed 按设计永不清理）。
            if !saw_running {
                break;
            }
        }
        failed.into_iter().collect()
    }

    /// 正在收尾 / 收尾失败的 id 列表（可观察性 / 测试；`Done` 已清理）。
    pub fn retiring_ids(&self) -> Vec<String> {
        let mut retiring = self.retiring.lock();
        Self::prune_retirements_locked(&mut retiring);
        let mut ids: Vec<String> = retiring.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 注入人格重建工厂（组合根调用一次；见字段文档）。
    pub fn set_factory(
        &self,
        factory: impl Fn(String, TeamMember) -> Arc<Agent> + Send + Sync + 'static,
    ) {
        *self.factory.write() = Some(std::sync::Arc::new(factory));
    }

    /// 取重建工厂（未注入 = None）。
    pub fn factory_ref(&self) -> Option<AgentFactory> {
        self.factory.read().as_ref().map(|inner| AgentFactory {
            inner: std::sync::Arc::clone(inner),
        })
    }

    pub fn set_config_writer(&self, writer: ConfigWriter) {
        *self.config_writer.lock() = Some(writer);
    }

    /// Create or update a profile. When enabled and not running, instantiates
    /// via the factory; persists through the config writer.
    ///
    /// **入口自保（2026-11 C 复审）**：启用方向（新建 / 从禁用转启用）会重建
    /// 实例；上一代还在收尾时明确返回 `Err`，与 `set_enabled` 同一口径。
    /// 检查与重建同处 [`Self::lifecycle`] 闸内，不留 `remove → 登记` 的窗口。
    pub fn save_profile(
        self: &std::sync::Arc<Self>,
        id: &str,
        profile: TeamMember,
    ) -> Result<(), String> {
        let mut profile = profile;
        // 合并语义：传入 profile 的 per-agent 预算为 None 且旧 profile 已有值时
        // 保留旧值，避免前端（未编辑该字段）保存时擦除手工写入 TOML 的预算。
        if let Some(old) = self.teams.read().get(id) {
            if profile.memory_limit_tokens.is_none() {
                profile.memory_limit_tokens = old.memory_limit_tokens;
            }
            if profile.context_window_tokens.is_none() {
                profile.context_window_tokens = old.context_window_tokens;
            }
        }
        // 启停单轨（2026-11）：`profile.enabled` 是唯一事实，运行时 map 跟着它走。
        let enabled = profile.enabled;
        {
            let _lifecycle = self.lifecycle.lock();
            if enabled {
                // 上一代没退干净就不许重建（入口自保，不依赖调用方记得先 await）。
                self.warn_if_retiring(id);
                if let Some(reason) = self.retirement_barrier_locked(id) {
                    return Err(format!(
                        "agent {id} 的上一代还在收尾，暂不能重建，请稍后重试（{reason}）"
                    ));
                }
            }
            self.teams.write().insert(id.to_string(), profile.clone());
            let running = self.agents.read().contains_key(id);
            if enabled && !running {
                let agent = self
                    .factory_ref()
                    .ok_or_else(|| {
                        format!(
                            "agent factory not injected by composition root; cannot rebuild {id}"
                        )
                    })?
                    .call(id.to_string(), profile.clone());
                // 走唯一入口：构造即绑定 manager（漏绑会让 TeamsList 静默为空）。
                let running = RunningAgent::bound(self, id.to_string(), profile, agent);
                self.agents.write().insert(id.to_string(), running);
            } else if !enabled && running {
                // 保存即禁用：与 ToggleTeam(false) 同一条收尾路径（2026-11 C）。
                self.detach_and_retire_locked(id);
            } else if enabled && running {
                // 运行中保存：同步 RunningAgent.profile（2026-10 修复）。
                // SaveTeam 已把新能力应用到运行中 agent，但这里此前不同步——
                // 后续 ToggleTeam(id, true)（already running 分支跳过重建）
                // 会用**旧 profile** 重应用能力，把新能力回滚掉（profile 漂移）。
                if let Some(entry) = self.agents.write().get_mut(id) {
                    entry.profile = profile;
                }
            }
        }
        self.persist_profiles()?;
        Ok(())
    }

    /// Delete a profile.
    ///
    /// 去主智能体（2026-09）：不再有受保护的"主 agent"；唯一约束是
    /// **至少保留一个智能体**（否则前端/路由将无目标可选）。
    pub fn delete_profile(&self, id: &str) -> Result<(), String> {
        {
            let teams = self.teams.read();
            if teams.len() <= 1 {
                return Err("至少保留一个智能体".into());
            }
            if !teams.contains_key(id) {
                return Err(format!("智能体 {id} 不存在"));
            }
        }
        {
            let _lifecycle = self.lifecycle.lock();
            self.teams.write().remove(id);
            // 删除同样要收尾：停后台任务 + 等 turn 排空 + 最终保存（否则旧实例
            // 的周期任务会继续往这份人格的会话档里写，且删掉之后没人再管它）。
            // 摘实例与登记收尾同一临界区（与 set_enabled 共用）。
            self.detach_and_retire_locked(id);
        }
        self.persist_profiles()?;
        Ok(())
    }

    fn persist_profiles(&self) -> Result<(), String> {
        let writer = self.config_writer.lock();
        match writer.as_ref() {
            Some(w) => {
                let map: std::collections::BTreeMap<String, TeamMember> =
                    self.teams.read().clone().into_iter().collect();
                w(&map)
            }
            None => Ok(()),
        }
    }

    /// Snapshot for the Panel (AgentsList event) — all known profiles with
    /// running state and session counts.
    pub fn infos(&self) -> Vec<TeamInfo> {
        let identity = self.node_identity.read();
        let agents = self.agents.read();
        let teams = self.teams.read();
        let mut list: Vec<TeamInfo> = teams
            .iter()
            .map(|(id, p)| TeamInfo {
                id: id.clone(),
                node_id: identity.node_id_field(),
                region_name: identity.region_name_field(),
                name: p.name.clone(),
                description: p.description.clone(),
                // 配置态读 profile（不再拿运行时 map 冒充）：两个状态必须能
                // 分开看到，否则"启动失败"会被误显示成"被禁用"。
                enabled: p.enabled,
                running: agents.contains_key(id),
                system_skills: p.system_skills.clone(),
                sessions: agents.get(id).map(|r| r.agent.session_count()).unwrap_or(0),
                system_prompt: p.system_prompt.clone(),
                disabled_tools: p.disabled_tools.clone(),
                disabled_skills: p.disabled_skills.clone(),
                enabled_modules: p.enabled_modules.clone(),
                enabled_tools: p.enabled_tools.clone(),
                enabled_skills: p.enabled_skills.clone(),
                // 结论式名单（见 `TeamInfo` 的字段文档）：把"用户写的配置"
                // 归一成"消费方可以直接查的形态"。
                //
                // 归一只做**值的形态**，不改判据：空仍表示"不限制"。真正的
                // 收益是消除一处静默漂移——旧实例前缀（`echo-agent.adapter.<p>`）
                // 在 Core 侧会被 `profile_allows_module` 归一后才比较，而前端
                // 此前拿到的是**原始配置**，做字面量比较时不认旧 id。同一个
                // 模块，后端认为对这个人格生效、前端认为不生效，两边都不报错。
                available_modules: dedup_non_empty(p.enabled_modules.iter()),
                available_tools: dedup_non_empty(p.enabled_tools.iter()),
                memory_limit_tokens: p.memory_limit_tokens,
                context_window_tokens: p.context_window_tokens,
                api_profile: p.api_profile.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }
}

/// Process-wide manager & factory slots.
///
#[derive(Clone)]
pub struct AgentFactory {
    inner: AgentFactoryInner,
}

impl AgentFactory {
    pub fn call(&self, id: String, profile: TeamMember) -> Arc<Agent> {
        (self.inner)(id, profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_with_modules(enabled: &[&str]) -> TeamMember {
        TeamMember {
            name: "t".into(),
            enabled_modules: enabled.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// 构造 manager：调用方须保证成员都是 `enabled: false`（或 `disabled`），
    /// build 才不会调 factory（factory 缺失会 panic——确保测试不触碰实例化路径）。
    fn manager_with(raw: AgentConfig) -> AgentManager {
        AgentManager::build(&raw, |_id, _p| {
            panic!("factory must not be called in tests")
        })
    }

    /// 节点身份作为**参数**下发（P3 终态，不再经 kernel cell）：
    /// `set_node_identity` 之后每条 `TeamInfo` 都带上它。
    #[test]
    fn infos_carry_injected_node_identity() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "a".into(),
            TeamMember {
                enabled: false,
                ..member_with_modules(&[])
            },
        );
        let mgr = manager_with(raw);
        // 未注入 → 字段不下发（嵌入式/测试用法）。
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id, None);
        assert_eq!(info.region_name, None);

        mgr.set_node_identity(crate::NodeIdentity::new("node-01abc", "办公机"));
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id.as_deref(), Some("node-01abc"));
        assert_eq!(info.region_name.as_deref(), Some("办公机"));

        // 区域名为空 = 只下发 node_id（前端回退用它展示）。
        mgr.set_node_identity(crate::NodeIdentity::new("node-01abc", ""));
        let info = &mgr.infos()[0];
        assert_eq!(info.node_id.as_deref(), Some("node-01abc"));
        assert_eq!(info.region_name, None);
    }

    /// 未启用的人格不实例化；`TeamInfo` 里**配置态**与**运行态**分开上报。
    #[test]
    fn disabled_profile_is_not_instantiated() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "disabled".into(),
            TeamMember {
                enabled: false,
                ..Default::default()
            },
        );
        let manager = AgentManager::build(&raw, |_id, _profile| {
            panic!("disabled profiles must not instantiate an Agent")
        });
        assert!(manager.agent_ids().is_empty());
        assert_eq!(manager.team_ids(), vec!["disabled"]);
        let info = &manager.infos()[0];
        assert!(!info.enabled, "配置态 = 未启用");
        assert!(!info.running, "运行态 = 没有实例");
    }

    /// 旧配置迁移（2026-11）：`disabled_teams` 里的 id 折成 `enabled = false`。
    ///
    /// 两个字段此前表达同一件事（`!enabled || disabled.contains`）。迁移只在
    /// **读**时做一次，写回时把废字段清掉（见配置写回处），否则用户在面板里
    /// 重新启用后，下次启动会被这份旧名单再按回去。
    #[test]
    fn legacy_disabled_teams_migrate_into_enabled() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "legacy".into(),
            TeamMember {
                enabled: true,
                ..Default::default()
            },
        );
        raw.disabled_teams = vec!["legacy".into()];
        let manager = AgentManager::build(&raw, |_id, _profile| {
            panic!("legacy-disabled profiles must not instantiate an Agent")
        });
        assert!(manager.agent_ids().is_empty(), "必须按禁用处理");
        assert!(!manager.infos()[0].enabled);
    }

    /// 启停单轨：`set_enabled` 之后配置态与运行态同步，且**不需要**第三个
    /// 名单来保持一致（旧实现的 `disabled` 列表已删）。
    #[tokio::test]
    async fn set_enabled_is_the_single_source_of_truth() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "a".into(),
            TeamMember {
                enabled: true,
                ..Default::default()
            },
        );
        let manager = std::sync::Arc::new(AgentManager::build(&raw, stub_agent));
        assert!(manager.infos()[0].running);

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        let info = &manager.infos()[0];
        assert!(!info.enabled, "配置态跟着关");
        assert!(!info.running, "运行态跟着关");
        // 单轨 + 无两代写者（2026-11 C 复审）：重新启用前必须等上一代收尾
        // 跑完——直接调 `set_enabled(true)` 会被入口闸拒绝（见
        // `rebuild_is_refused_while_previous_generation_retires`）。
        manager.await_retirement("a").await.expect("retirement");

        manager.set_enabled("a", true, stub_agent).expect("enable");
        let info = &manager.infos()[0];
        assert!(info.enabled);
        assert!(info.running, "重新启用 = 经工厂重建实例");
    }

    /// 2026-10 用户请求回归：**未配置任何 `[agent.teams.*]` 时不得合成
    /// "default" 人格**——此前每台新节点/新装 Core 都会凭空多出一个
    /// "默认" agent（`agents=["default"]`）；空配置现在就是零人格，
    /// 由 Panel 按需新建。
    #[test]
    fn empty_teams_create_no_personas() {
        let raw = AgentConfig::default();
        let manager = AgentManager::build(&raw, |_id, _profile| {
            panic!("empty teams must not instantiate any Agent")
        });
        assert!(
            manager.agent_ids().is_empty(),
            "unconfigured Core must start with zero personas: {:?}",
            manager.agent_ids()
        );
        assert!(manager.team_ids().is_empty());
        assert!(manager.infos().is_empty());
        // 无兜底解析：None / "default" 都必须落空（去主智能体语义）。
        assert!(manager.resolve(None).is_none(), "no default fallback");
        assert!(manager.resolve(Some("default")).is_none());
    }

    // ── API profile 改名传播（2026-10）──

    struct StubProvider;

    #[async_trait::async_trait]
    impl crate::llm::LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn default_model(&self) -> &str {
            "stub"
        }
        async fn chat(
            &self,
            _request: &crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
        async fn chat_stream(
            &self,
            _request: &crate::llm::ChatRequest,
            _tx: tokio::sync::mpsc::UnboundedSender<crate::llm::ChatChunk>,
        ) -> Result<crate::llm::ChatResponse, crate::llm::LlmError> {
            Err(crate::llm::LlmError::Config("stub".into()))
        }
    }

    fn stub_agent(_id: String, _p: TeamMember) -> Arc<Agent> {
        Arc::new(Agent::new(
            Arc::new(StubProvider),
            AgentConfig::default(),
            crate::SkillRegistry::new(),
            crate::tool::ToolRegistry::new(),
            Arc::new(echo_defs::module_instance::InstanceRegistry::new()),
        ))
    }

    /// 改名传播：teams（含禁用）与运行中 persona 快照的引用一起改写；
    /// 无关引用不受影响；只做内存改写（无启停副作用）。
    ///
    /// （tokio runtime：`Agent::new` 装配期需要运行时上下文。）
    #[tokio::test]
    async fn replace_api_refs_updates_teams_and_running_profiles() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "alice".into(),
            TeamMember {
                name: "Alice".into(),
                api_profile: Some("alpha".into()),
                ..Default::default()
            },
        );
        raw.teams.insert(
            "bob".into(),
            TeamMember {
                name: "Bob".into(),
                api_profile: Some("beta".into()),
                ..Default::default()
            },
        );
        raw.teams.insert(
            "carol".into(),
            TeamMember {
                name: "Carol".into(),
                api_profile: Some("alpha".into()),
                // 未启用：不进运行时，但配置里的 API 引用仍要算进引用计数。
                enabled: false,
                ..Default::default()
            },
        );
        let mgr = AgentManager::build(&raw, stub_agent);

        assert_eq!(
            mgr.api_ref_owners("alpha"),
            vec!["alice".to_string(), "carol".to_string()]
        );
        assert_eq!(mgr.api_ref_owners("beta"), vec!["bob".to_string()]);
        assert!(mgr.api_ref_owners("gamma").is_empty());

        let changed = mgr.replace_api_refs("alpha", "gamma");
        assert_eq!(changed, 2, "alice + carol (disabled included)");

        assert!(mgr.api_ref_owners("alpha").is_empty());
        assert_eq!(
            mgr.api_ref_owners("gamma"),
            vec!["alice".to_string(), "carol".to_string()]
        );
        assert_eq!(
            mgr.team("alice").unwrap().api_profile.as_deref(),
            Some("gamma")
        );
        assert_eq!(
            mgr.team("bob").unwrap().api_profile.as_deref(),
            Some("beta"),
            "unrelated reference untouched"
        );
        // 运行中 persona 的快照同步更新——不重建实例。
        assert_eq!(
            mgr.get("alice").unwrap().profile.api_profile.as_deref(),
            Some("gamma")
        );
        assert_eq!(
            mgr.get("bob").unwrap().profile.api_profile.as_deref(),
            Some("beta")
        );
        // carol 未运行：无实例，但定义已更新。
        assert!(mgr.get("carol").is_none());
    }

    // ── 生命周期：禁用 = 停任务 + 最终保存；启用 = 任务齐全（2026-11 C）──

    fn temp_session_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "echo-agent-lifecycle-{name}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// 禁用：立刻不接新输入 → 后台任务停 → 最终保存落盘；
    /// 重新启用：后台任务起回来（不是"只有 runner"的半代实例）。
    #[tokio::test]
    async fn disable_retires_background_tasks_and_flushes_then_enable_restores_them() {
        use std::sync::atomic::Ordering;

        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "a".into(),
            TeamMember {
                enabled: true,
                ..Default::default()
            },
        );
        let manager = std::sync::Arc::new(AgentManager::build(&raw, stub_agent));
        let first = manager.get("a").expect("persona running");
        // `build` / `bound` **不**启动后台任务（2026-11 C 复审）：最终配置之后
        // 由组合根（启动期是 main 的人格循环、运行期是 process_commands）显式
        // 调 `start_background_tasks`。这里照做，模拟真实装配顺序。
        assert_eq!(first.agent.background_task_count(), 0, "构造阶段不起写者");
        first.agent.start_background_tasks();
        assert!(
            first.agent.background_task_count() > 0,
            "启用的人格必须有周期保存（显式启动之后）"
        );

        // 一条未落盘的改动 + 一个独立的会话档。
        let path = temp_session_path("retire");
        let _ = std::fs::remove_file(&path);
        first.agent.set_session_persist_path(&path);
        first
            .agent
            .trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    session: Some("local:tui::local_user".into()),
                    content: "禁用前最后一句".into(),
                    timestamp: 1700000000,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                    files: vec![],
                },
            ));
        assert!(first.agent.trunk.is_dirty());

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        // 同步部分：路由立刻解析不到、旧实例不再接新输入。
        assert!(manager.get("a").is_none(), "禁用即摘实例");
        assert!(first.agent.is_retired());
        assert!(
            first.agent.turns.draining.load(Ordering::Acquire),
            "禁用后不得再接新输入"
        );

        // 异步收尾：等它，然后核对"任务收走 + 最终保存"。
        manager.await_retirement("a").await.expect("retirement");
        assert_eq!(
            first.agent.background_task_count(),
            0,
            "禁用后后台任务必须收走（不能攥着 Arc<Agent> 永久驻留）"
        );
        assert!(path.exists(), "禁用必须做一次最终保存");
        assert!(!first.agent.trunk.is_dirty(), "最终保存应覆盖全部改动");
        assert!(
            manager.retiring_ids().is_empty(),
            "收尾完成后不该还挂在册上"
        );

        // 重新启用：任务齐全（周期保存 + 不再处于退场态）。
        manager.set_enabled("a", true, stub_agent).expect("enable");
        let second = manager.get("a").expect("persona re-created");
        assert!(!second.agent.is_retired());
        // 同样：重建方（process_commands）在应用完能力之后才启动后台任务。
        assert_eq!(
            second.agent.background_task_count(),
            0,
            "重建路径也不在构造阶段起写者"
        );
        second.agent.start_background_tasks();
        assert!(
            second.agent.background_task_count() > 0,
            "重新启用必须把周期保存起回来（否则数据没人落盘）"
        );
        // 新一代用的是新实例，不是被禁用的那一代。
        assert!(!std::sync::Arc::ptr_eq(&first.agent, &second.agent));
        let _ = std::fs::remove_file(&path);
    }

    /// 没有在册收尾时 `await_retirement` 立即返回（不能挂住命令泵）。
    #[tokio::test]
    async fn await_retirement_without_pending_returns_immediately() {
        let raw = AgentConfig::default();
        let manager = AgentManager::build(&raw, |_id, _p| panic!("no personas"));
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            manager.await_retirement("ghost"),
        )
        .await
        .expect("没有在册收尾时必须立即返回")
        .expect("没有在册收尾 = Ok");
        assert!(manager.retiring_ids().is_empty());
    }

    /// 删除人格同样要收尾（停后台任务 + 最终保存），而不是"摘掉就走"。
    #[tokio::test]
    async fn delete_profile_also_retires() {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "a".into(),
            TeamMember {
                enabled: true,
                ..Default::default()
            },
        );
        raw.teams.insert(
            "b".into(),
            TeamMember {
                enabled: false,
                ..Default::default()
            },
        );
        let manager = std::sync::Arc::new(AgentManager::build(&raw, stub_agent));
        let running = manager.get("a").expect("running");
        let path = temp_session_path("delete");
        let _ = std::fs::remove_file(&path);
        running.agent.set_session_persist_path(&path);
        running.agent.trunk.mark_dirty();

        manager.delete_profile("a").expect("delete");
        assert!(manager.get("a").is_none());
        assert!(running.agent.is_retired());
        manager.await_retirement("a").await.expect("retirement");
        assert!(path.exists(), "删除前必须把未落盘的改动写出去");
        assert_eq!(running.agent.background_task_count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    // ── 第二轮复审回归（2026-11 C）：真等待 / 共享等待 / 不提前重启 ──────

    /// 造一个 manager + 一个运行中人格（后台任务按真实装配顺序显式启动）。
    fn running_manager() -> (std::sync::Arc<AgentManager>, RunningAgent) {
        let mut raw = AgentConfig::default();
        raw.teams.insert(
            "a".into(),
            TeamMember {
                enabled: true,
                ..Default::default()
            },
        );
        let manager = std::sync::Arc::new(AgentManager::build(&raw, stub_agent));
        let running = manager.get("a").expect("persona running");
        running.agent.start_background_tasks();
        (manager, running)
    }

    /// 在飞 turn：禁用时只取消、**不等**它 commit 就写最终快照，就会把迟到的
    /// 那一笔丢掉（旧 `finish_retire` 的形状）。这里钉住"先等 turn 退出、再写"。
    ///
    /// 用登记表直接造"在飞 turn"（真 turn 要模型；本用例只需要"登记着、还没
    /// finish"这个事实——收尾的判据就是它）。
    #[tokio::test]
    async fn final_save_waits_for_in_flight_turn_commit() {
        let (manager, first) = running_manager();
        let path = temp_session_path("inflight");
        let _ = std::fs::remove_file(&path);
        first.agent.set_session_persist_path(&path);

        // 模拟"模型还在跑"的 turn：已登记，尚未 commit / finish。
        let turn = first
            .agent
            .try_register_inbound_turn("local:tui::local_user", 1)
            .expect("turn registered");

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        // 收尾必须**被挡住**：给足"如果没挡就会跑完"的窗口，断言它没跑完。
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            manager.await_retirement("a"),
        )
        .await;
        assert!(blocked.is_err(), "在飞 turn 退出前收尾不得完成");
        assert!(!path.exists(), "turn 未 commit 之前不得做最终保存");
        assert!(
            manager.retiring_ids().contains(&"a".to_string()),
            "收尾应仍在册"
        );

        // 迟到 commit（真实路径是 record_assistant_reply）+ turn 收尾。
        first
            .agent
            .trunk
            .append_event(echo_session::SessionEvent::UserMessage(
                echo_session::event::UserMessage {
                    session: Some("local:tui::local_user".into()),
                    content: "迟到的提交".into(),
                    timestamp: 1700000001,
                    message_sequence: None,
                    source: None,
                    images: vec![],
                    files: vec![],
                },
            ));
        first.agent.finish_inbound_turn(&turn.id);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.await_retirement("a"),
        )
        .await
        .expect("turn 退出后收尾必须完成")
        .expect("retirement ok");
        assert!(path.exists(), "turn 退出后必须做最终保存");
        let persisted = std::fs::read_to_string(&path).expect("persisted file");
        assert!(
            persisted.contains("迟到的提交"),
            "最终保存必须包含在飞 turn 的迟到 commit：{persisted}"
        );
        assert!(!first.agent.trunk.is_dirty());
        assert!(manager.retiring_ids().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// 多个等待者共享同一份完成状态；**等待被取消不影响收尾任务**，第二个
    /// 等待者也不会提前返回（旧实现 `remove` + `await JoinHandle`：取消即
    /// detach，第二人看到空表立刻返回）。
    #[tokio::test]
    async fn retirement_waiters_are_shared_and_cancellable() {
        let (manager, first) = running_manager();
        let path = temp_session_path("waiters");
        let _ = std::fs::remove_file(&path);
        first.agent.set_session_persist_path(&path);
        first.agent.trunk.mark_dirty();
        let turn = first
            .agent
            .try_register_inbound_turn("local:tui::local_user", 1)
            .expect("turn registered");
        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");

        // 等待者 1：跑到"在等结论"之后被取消（future drop）。
        let m1 = std::sync::Arc::clone(&manager);
        let waiter1 = tokio::spawn(async move { m1.await_retirement("a").await });
        tokio::task::yield_now().await;
        waiter1.abort();
        let _ = waiter1.await;

        // 等待者 2：仍然必须在等——不能因为"有人等过又走了"就提前返回。
        let m2 = std::sync::Arc::clone(&manager);
        let mut waiter2 = tokio::spawn(async move { m2.await_retirement("a").await });
        let early = tokio::time::timeout(std::time::Duration::from_millis(300), &mut waiter2).await;
        assert!(early.is_err(), "第二个等待者不得提前返回");

        // turn 退出 → 收尾完成 → 两个结论一致。
        first.agent.finish_inbound_turn(&turn.id);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiter2)
            .await
            .expect("等待者必须被唤醒")
            .expect("join waiter");
        assert!(result.is_ok(), "收尾结论应为成功：{result:?}");
        assert!(
            path.exists(),
            "收尾任务本身必须跑完（取消等待者不该 detach 它）"
        );
        assert!(manager.retiring_ids().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// 上一代没收完就重建 = 两代写者。`set_enabled(true)` / `save_profile`
    /// **自身**必须挡住（不能只靠上层记得先 await），且明确返回可重试错误。
    #[tokio::test]
    async fn rebuild_is_refused_while_previous_generation_retires() {
        let (manager, first) = running_manager();
        let turn = first
            .agent
            .try_register_inbound_turn("local:tui::local_user", 1)
            .expect("turn registered");
        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");

        // 不等、直接重建：入口自保必须拒绝。
        let error = manager
            .set_enabled("a", true, stub_agent)
            .expect_err("上一代收尾中不得重建");
        assert!(error.contains("收尾"), "错误要说清原因：{error}");
        assert!(manager.get("a").is_none(), "不得提前重建出第二代");

        let mut profile = manager.team("a").expect("profile");
        profile.enabled = true;
        let error = manager
            .save_profile("a", profile)
            .expect_err("上一代收尾中不得重建（save_profile 同口径）");
        assert!(error.contains("收尾"), "错误要说清原因：{error}");
        assert!(manager.get("a").is_none());
        assert!(
            !manager.team("a").expect("profile").enabled,
            "被拒绝的保存不得把配置态改成 enabled"
        );

        // 收尾完成之后才能重建（同一入口、同一把闸）。
        first.agent.finish_inbound_turn(&turn.id);
        manager.await_retirement("a").await.expect("retirement ok");
        manager
            .set_enabled("a", true, stub_agent)
            .expect("收尾完成后可以重建");
        assert!(manager.get("a").is_some());
    }

    /// 退出时必须等**已下线**（不在 supervisor 在册名单里）人格的收尾：
    /// `await_all_retirements` 覆盖它们，别只遍历 personas。
    #[tokio::test]
    async fn await_all_retirements_covers_off_register_personas() {
        let (manager, first) = running_manager();
        let path = temp_session_path("off-register");
        let _ = std::fs::remove_file(&path);
        first.agent.set_session_persist_path(&path);
        first.agent.trunk.mark_dirty();
        let turn = first
            .agent
            .try_register_inbound_turn("local:tui::local_user", 1)
            .expect("turn registered");
        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        assert!(manager.get("a").is_none(), "已下线：不在运行名单里");

        let m = std::sync::Arc::clone(&manager);
        let mut wait_all = tokio::spawn(async move { m.await_all_retirements().await });
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut wait_all).await;
        assert!(early.is_err(), "在飞 turn 还在时，退出等待不得完成");

        first.agent.finish_inbound_turn(&turn.id);
        let failed = tokio::time::timeout(std::time::Duration::from_secs(5), wait_all)
            .await
            .expect("退出等待必须在收尾完成后返回")
            .expect("join wait_all");
        assert!(failed.is_empty(), "不应有失败收尾：{failed:?}");
        assert!(path.exists(), "已下线人格的最终保存必须落盘");
        assert!(manager.retiring_ids().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// 停止状态不可重启：已退场的一代拒绝再启动后台任务（不给最终保存造
    /// 第二个写者）。
    #[tokio::test]
    async fn retired_persona_refuses_to_start_background_tasks() {
        let (manager, first) = running_manager();
        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        manager.await_retirement("a").await.expect("retirement ok");
        assert_eq!(first.agent.background_task_count(), 0);
        first.agent.start_background_tasks();
        assert_eq!(
            first.agent.background_task_count(),
            0,
            "退场后不得再起任何后台任务"
        );
    }

    // ── Strict 退休的保存结果（2026-11 跨组闭环）──────────────────────
    //
    // 旧链路 `save_now() -> ()` 无论拒写还是 IO 失败都返回"成功"，
    // RetirementEntry 于是记 Done、放开重建——重启后这段对话却不存在。
    // 这批用例钉住"失败必须留 Failed、阻止重建"。

    fn manager_user_event(session: &str, content: &str) -> echo_session::SessionEvent {
        echo_session::SessionEvent::UserMessage(echo_session::event::UserMessage {
            session: Some(session.into()),
            content: content.into(),
            timestamp: 1700000000,
            message_sequence: None,
            source: None,
            images: vec![],
            files: vec![],
        })
    }

    /// 坏档：Strict 退休必须 Err，登记保持 Failed，重建被挡，坏档不被覆盖。
    #[tokio::test]
    async fn corrupt_session_file_fails_retirement_and_blocks_rebuild() {
        let (manager, running) = running_manager();
        let path = temp_session_path("retire-corrupt");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "{ 坏档").unwrap();
        running.agent.set_session_persist_path(&path);
        assert!(
            running.agent.trunk.ensure_loaded_or_err().await.is_err(),
            "前置：坏档必须进入 Failed"
        );

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        let error = manager
            .await_retirement("a")
            .await
            .expect_err("保存失败必须让退休失败（登记留 Failed）");
        assert!(error.contains("加载失败"), "{error}");
        assert_eq!(
            manager.retiring_ids(),
            vec!["a".to_string()],
            "Failed 的收尾必须保留在册（继续挡重建）"
        );
        assert!(
            manager.set_enabled("a", true, stub_agent).is_err(),
            "上一代没收干净，重建必须被挡"
        );
        assert!(manager.get("a").is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ 坏档",
            "坏档不能被空快照覆盖"
        );

        // 管理修复出口不被入口门禁停掉：坏档确认/修好后仍可显式重试。
        running
            .agent
            .trunk
            .retry_failed_load()
            .expect("干净空 store 可显式重试（管理修复不被停摆）");
        let _ = std::fs::remove_file(&path);
    }

    /// 早已 `Failed` 的收尾不能被退出等待漏掉（2026-12 小修）：旧实现只收
    /// `Running`，坏档这类"结论已定"的失败被当成干净——进程升级据此判定
    /// "可以走了"，可它的最终保存根本没落盘。
    #[tokio::test]
    async fn await_all_retirements_reports_already_failed_entries() {
        let (manager, running) = running_manager();
        let path = temp_session_path("retire-corrupt-all");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "{ 坏档").unwrap();
        running.agent.set_session_persist_path(&path);
        assert!(
            running.agent.trunk.ensure_loaded_or_err().await.is_err(),
            "前置：坏档必须进入 Failed"
        );
        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        manager
            .await_retirement("a")
            .await
            .expect_err("前置：这次退休已经 Failed（结论已定）");

        let failed = manager.await_all_retirements().await;
        assert_eq!(
            failed.len(),
            1,
            "早已 Failed 的条目必须汇总，不能被漏成干净：{failed:?}"
        );
        assert!(
            failed[0].starts_with("a: ") && failed[0].contains("加载失败"),
            "必须带 id 与原因：{failed:?}"
        );
        assert_eq!(
            manager.retiring_ids(),
            vec!["a".to_string()],
            "汇总不得移除 Failed 登记（它继续挡重建）"
        );
        assert!(
            manager.set_enabled("a", true, stub_agent).is_err(),
            "Failed 仍在册：重建必须继续被挡"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ 坏档",
            "坏档不能被空快照覆盖"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 两次调用必须一致（稳定排序 + 去重，含全部 id）：`Failed` 登记永不
    /// 清理，若按 HashMap 迭代序返回或不去重，两次结果就会漂。
    #[tokio::test]
    async fn await_all_retirements_is_stable_and_deduped_across_calls() {
        let mut raw = AgentConfig::default();
        for id in ["b", "a"] {
            raw.teams.insert(
                id.into(),
                TeamMember {
                    enabled: true,
                    ..Default::default()
                },
            );
        }
        let manager = std::sync::Arc::new(AgentManager::build(&raw, stub_agent));
        let mut paths = Vec::new();
        for id in ["b", "a"] {
            let running = manager.get(id).expect("persona running");
            let path = temp_session_path(&format!("retire-corrupt-all-{id}"));
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, "{ 坏档").unwrap();
            running.agent.set_session_persist_path(&path);
            assert!(
                running.agent.trunk.ensure_loaded_or_err().await.is_err(),
                "前置：{id} 的坏档必须进入 Failed"
            );
            manager.set_enabled(id, false, stub_agent).expect("disable");
            manager
                .await_retirement(id)
                .await
                .expect_err("前置：这次退休已经 Failed（结论已定）");
            paths.push(path);
        }

        let first = manager.await_all_retirements().await;
        let second = manager.await_all_retirements().await;
        assert_eq!(first, second, "两次调用必须一致（稳定排序 + 去重）");
        assert_eq!(
            first.len(),
            2,
            "所有 Failed 的 id 都要在（且不重复）：{first:?}"
        );
        assert!(
            first[0].starts_with("a: ") && first[1].starts_with("b: "),
            "必须按 id 稳定排序：{first:?}"
        );
        assert_eq!(
            manager.retiring_ids(),
            vec!["a".to_string(), "b".to_string()],
            "两次汇总都不得移除 Failed 登记"
        );
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
    }
    /// 陈旧快照（外部写者已提交新档）：退休必须 Err 而不是 Done。
    #[tokio::test]
    async fn stale_snapshot_fails_retirement_instead_of_reporting_done() {
        let (manager, running) = running_manager();
        let path = temp_session_path("retire-stale");
        let _ = std::fs::remove_file(&path);

        // 先写一份有效档，并让人格加载它（建立指纹基线）。
        let seed = agent_module_session::TrunkStore::new(1000);
        seed.set_persist_path(&path);
        seed.append_event(manager_user_event("local:tui::local_user", "种子"));
        seed.save_now().await;
        assert!(path.exists(), "前置：种子档写出来");

        running.agent.set_session_persist_path(&path);
        running.agent.trunk.ensure_loaded().await;
        assert!(running.agent.trunk.is_loaded());

        // 外部写者（另一代 / 另一实例）提交了新快照。
        std::fs::write(&path, "外部写者替换后的新档").unwrap();
        running.agent.trunk.append_event(manager_user_event(
            "local:tui::local_user",
            "本代最后的改动",
        ));

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        let error = manager
            .await_retirement("a")
            .await
            .expect_err("陈旧快照必须让退休失败");
        assert!(
            error.contains("磁盘文件已变化") || error.contains("落盘被拒"),
            "{error}"
        );
        assert_eq!(manager.retiring_ids(), vec!["a".to_string()]);
        assert!(manager.set_enabled("a", true, stub_agent).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "外部写者替换后的新档",
            "旧快照不能覆盖新档"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 纯内存（无路径）：允许完成退休，不报"永久 dirty"。
    #[tokio::test]
    async fn memory_only_persona_retires_without_permanent_dirty() {
        let (manager, running) = running_manager();
        running.agent.trunk.mark_dirty();

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        manager
            .await_retirement("a")
            .await
            .expect("memory-only 必须允许完成退休（没有磁盘目标）");
        assert!(
            !running.agent.trunk.is_dirty(),
            "无路径 store 的代次要按成功对齐，不能留下永久 dirty"
        );
        assert!(manager.retiring_ids().is_empty());
    }

    /// 正常持久化路径：checked 保存成功 → 退休 Done、最终快照落盘。
    #[tokio::test]
    async fn persistent_persona_retires_after_checked_save() {
        let (manager, running) = running_manager();
        let path = temp_session_path("retire-checked-ok");
        let _ = std::fs::remove_file(&path);
        running.agent.set_session_persist_path(&path);
        running.agent.trunk.append_event(manager_user_event(
            "local:tui::local_user",
            "禁用前最后一句",
        ));
        assert!(running.agent.trunk.is_dirty());

        manager
            .set_enabled("a", false, stub_agent)
            .expect("disable");
        manager
            .await_retirement("a")
            .await
            .expect("正常持久化路径必须通过");
        assert!(path.exists(), "最终快照必须落盘");
        assert!(!running.agent.trunk.is_dirty());
        assert!(manager.retiring_ids().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod team_info_conclusion_fields {
    use super::*;

    /// 结论字段必须反映 Core 的归一规则，而不是原样转发配置。
    #[test]
    fn available_modules_are_canonicalized_and_deduped() {
        let raw: Vec<String> = vec![
            "echo-agent.adapter.qq".into(),     // 旧实例前缀
            "echo-agent.instance.qq".into(),    // 归一后与上一条相同
            "".into(),                          // 空串（手写配置里的噪音）
            "  echo-agent.skills.dir  ".into(), // 前后空格
            "echo-agent.skills.dir".into(),     // 重复
        ];
        let out = dedup_non_empty(raw.iter());
        assert_eq!(
            out,
            vec![
                "echo-agent.instance.qq".to_string(),
                "echo-agent.skills.dir".to_string()
            ],
            "旧前缀必须归一、空串与重复必须去掉"
        );
    }

    /// 空名单保持为空——消费方按「空 = 未限制」解读（协议契约）。
    #[test]
    fn empty_list_stays_empty() {
        let raw: Vec<String> = Vec::new();
        assert!(dedup_non_empty(raw.iter()).is_empty());
    }
}
