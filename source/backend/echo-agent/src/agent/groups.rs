//! `Agent` 的字段分组（2026-11 S9：门面化）。
//!
//! ## 为什么分组
//!
//! `Agent` 曾经把 32 个字段平铺在一层。平铺的问题不是"看起来长"，而是
//! **不变式无处安放**：哪些字段必须一起变、哪个字段归哪个子系统，只能靠
//! 注释和记忆。历史上真实踩过的坑都长这个样子：
//!
//! - 切 provider 时忘了同步 persona 标记与会话覆盖（→ `ModelService`）；
//! - 插件停用时忘了撤销远程面声明（→ 现在由注册表打标）；
//! - 事件出口有三条路径（sink / handle / bus），漏改一条就"某类调用方
//!   看不到某些事件"（→ 现在四条出口同处 [`EventEgress`]）。
//!
//! 分组规则只有一条：**组内成员必须一起变**，否则不合并。所以这里看不见
//! `tools` / `trunk` / `instances`——它们是**真句柄**（独立所有权，组合根、
//! 联邦泵、管理面直接持有），不是某个子系统的内部状态。往定位器里塞真句柄
//! 只会多一层解引用。
//!
//! ## 分组的收益
//!
//! 32 → 12 个字段。而且每个组可以**独立构造**：`Identity::default()` 在测试
//! 与嵌入式用法里就是"空身份"，不必再写 8 个 `RwLock::new(None)`。
//!
//! 分组**不是**插件化：`Agent` 是 per-persona 作用域（对应 Cordis 的
//! `ctx.fork()`），不是能力。

use std::sync::Arc;

use dashmap::DashMap;

use crate::agent::tool_dispatch::ToolStateEventBuilder;

/// 「我是谁」：归属人格 / 节点身份 / 能力快照。
///
/// 三者都是**只读为主、装配期写入**的自我描述。放在一组是因为它们回答同一个
/// 问题，且共同构成事件标注（`annotate_team`）与线格式（`NodeHello`）的来源。
#[derive(Default)]
pub(crate) struct Identity {
    /// 归属人格 id（None = 进程级核心服务代理，不属于任何人格）。
    ///
    /// 写：组合根装配期一次；读：**每次 emit**（事件标注）、会话归属判定。
    pub(crate) team_id: std::sync::Mutex<Option<String>>,
    /// 节点身份（federation NodeId + 运行区域名）：组合根构造一份，装配期注入。
    ///
    /// 缺省为空 = 纯嵌入式用法（线格式里不带这两个字段）。**不是**进程级
    /// kernel cell——身份是构造参数。
    pub(crate) node_identity: std::sync::RwLock<crate::NodeIdentity>,
    /// 能力快照（模型 profile / 工具与技能白名单）：门控重算的输入。
    ///
    /// `None` = 未注入（单测与嵌入式用法），此时门控按"全开"处理。
    pub(crate) capabilities: std::sync::Mutex<Option<crate::config::AgentProfile>>,
}

/// 「事件怎么出去」：四条出口 + 状态事件绑定表。
///
/// 四条出口的存在是历史层积，但**顺序是契约**：插件事件通道 → bus（面板投影）
/// → 进程级汇聚点 → 单 agent 句柄。汇聚点优先于句柄（此前"谁是默认人格谁
/// 上报"的问题就是这么来的）。
///
/// 放在一组是为了让"我又加了一条发射路径"这件事**必须**在这里发生——而不是
/// 在别处偷偷 `self.egress.handle.read()`。
pub(crate) struct EventEgress {
    /// 进程级事件汇聚点（组合根注入）：所有 persona 的 `emit` 都投递到同一
    /// 汇聚点，Panel 单连接即可看到所有人格活动，无需求助"默认人格"镜像。
    /// 设置后优先于 [`Self::handle`]（`handle` 保留给单 agent 测试与旧路径）。
    pub(crate) sink: std::sync::RwLock<Option<crate::agent::EventSink>>,
    /// 单 agent 事件句柄。`tokio::RwLock`：`emit()` 从异步循环里调用，绝不能
    /// 阻塞 worker——所有访问都走 `try_*`，且不跨 await 持有。
    pub(crate) handle: tokio::sync::RwLock<Option<Arc<crate::BackendHandle>>>,
    /// 事件总线：`emit` 经它广播；消费者（时间线投影、前端桥）作为监听者订阅。
    pub(crate) bus: Arc<echo_context::EventBus>,
    /// 保持时间线投影的总线订阅存活到 agent 生命周期结束（drop = 取消订阅）。
    ///
    /// 从不读取——它存在的唯一意义就是"活到 agent 死"。删掉它，时间线投影
    /// 会在构造结束的那一刻退订（面板失去全部历史投影）。
    #[allow(dead_code)]
    pub(crate) timeline_projection: echo_context::Disposer,
    /// 状态快照 key → 线上事件构造器（**绑定时声明**，2026-11 S7）。
    ///
    /// 工具用 `Tool::state_key` 声明"我有可见状态"，而"这状态长什么线上事件"
    /// 是**协议形状**——它由绑定这个工具的插件给出，不由执行链按工具名猜。
    /// 例：`echo-plugin-tools` 注册 `checklist → ChecklistUpdated`。
    ///
    /// 与出口同组的原因：这张表**就是**出口的一部分（它决定"状态的出口形状"）。
    pub(crate) state_events: std::sync::RwLock<std::collections::HashMap<String, ToolStateEventBuilder>>,
}

/// 「谁在跑」：turn 驱动、并发闸、在飞表、序号、退出信号。
///
/// 六件事都是 turn 生命周期的不同刻面。此前"在飞 turn"与"序号"分居两处，
/// 出现过"序号分配了但 turn 没登记"的窗口（面板看到序号跳号）。
pub(crate) struct TurnState {
    /// Turn 驱动（echo-loop TurnRunner，唯一循环实现）。组合根注入共享
    /// runner；未注入时首个 turn 按本 agent 的 provider/配置惰性构建。
    pub(crate) runner: tokio::sync::RwLock<Option<Arc<echo_loop::runner::TurnRunner>>>,
    /// 并行回复分支上限（`MAX_CONCURRENT_REPLY_BRANCHES`）：限流而非排他。
    pub(crate) reply_branch_slots: tokio::sync::Semaphore,
    /// 在飞入站 turn（会话键 → 句柄）：并发去重与"同一会话串行"的判据。
    pub(crate) active_inbound_turns: DashMap<String, crate::agent::ActiveInboundTurn>,
    /// 消息序号（单调递增）：面板据此给流式片段排序。
    pub(crate) next_message_sequence: std::sync::atomic::AtomicU64,
    /// 优雅退出标记：置位后新 turn 一律拒绝，在飞的跑完
    /// （自更新连续性的前提）。
    pub(crate) draining: std::sync::atomic::AtomicBool,
    /// 后台任务取消令牌（身份淘汰 / 落盘）：drop = 停机。
    pub(crate) cancel: tokio_util::sync::CancellationToken,
}

impl TurnState {
    pub(crate) fn new(
        reply_slots: usize,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            runner: tokio::sync::RwLock::new(None),
            reply_branch_slots: tokio::sync::Semaphore::new(reply_slots),
            active_inbound_turns: DashMap::new(),
            next_message_sequence: std::sync::atomic::AtomicU64::new(1),
            draining: std::sync::atomic::AtomicBool::new(false),
            cancel,
        }
    }
}

/// 「看不见的横切」：脱敏 / 指标 / 门控日志去重。
///
/// 三者都不改变业务结果，但都必须**在正确的时机**参与：脱敏漏一处就是泄露，
/// 指标漏一处就是账单对不上，日志不去重就是 321 条/天。
#[derive(Default)]
pub(crate) struct Instrumentation {
    /// 敏感信息脱敏器（组合根注入；None = 透传）。
    ///
    /// 覆盖：入站内容、工具事件与会话落盘的参数、工具结果、LLM 请求出口、
    /// 插件实例出站。设计见 `document/core/security-redaction.md`。
    pub(crate) redactor: std::sync::RwLock<Option<Arc<dyn echo_defs::sanitize::Redactor>>>,
    /// API 指标存储（余额快照 / token 用量；组合根装配期注入）。
    /// None = 不记录（测试 / 未接线的旧路径）。
    pub(crate) metrics: std::sync::RwLock<Option<Arc<crate::metrics::MetricsStore>>>,
    /// 门控日志去重（`plugin_id` → 上次记录的最终启用态）。
    ///
    /// `apply_plugin_gating` 在每轮能力重算里被逐 persona 调用，绝大多数调用
    /// 是幂等的；只按「状态真的翻转」打日志，把默认级别下的
    /// `plugin tool/skill package toggled` 从 321 条/天降到仅剩真实开关。
    pub(crate) gating_log_state: DashMap<String, bool>,
}

/// 插件子系统：宿主 + 进程级策略 + 重载标记。
///
/// `global_policy` 与 `host` 同组不是巧合：**全局启停**与**插件装配**是同一个
/// 决策的两个时刻（策略决定装不装，宿主执行装）。`plugin_reload_started`
/// 是"重载进行中"的一次性闸，防止并发重入装两遍。
pub(crate) struct PluginSubsystem {
    /// 插件宿主：注册表 + 挂载上下文。组合根注入**同一个**进程级宿主
    /// （去主智能体 P1：插件宿主不再寄居在某个"默认人格"上）。
    pub(crate) host: std::sync::RwLock<Arc<crate::plugins::PluginHost>>,
    /// 全局禁用策略（`[agent].disabled_tools/disabled_skills`）：进程级开关
    /// 不属于任何人格，组合根注入同一份。None = 未接线（调用方回退本 agent
    /// 的配置快照，单元测试走这条）。
    pub(crate) global_policy: std::sync::RwLock<Option<Arc<crate::agent::GlobalPolicy>>>,
    /// 重载闸：`self_update` 触发的插件重载只允许起一次（并发点两次 = 装两遍）。
    pub(crate) reload_started: std::sync::atomic::AtomicBool,
}

impl PluginSubsystem {
    pub(crate) fn new(host: Arc<crate::plugins::PluginHost>) -> Self {
        Self {
            host: std::sync::RwLock::new(host),
            global_policy: std::sync::RwLock::new(None),
            reload_started: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// 「插件装上来的接口」：四个端口，全部由插件在 mount 时填入。
///
/// 共同点：**都没有插件就没它们**，且都必须"优雅缺省"——未挂载时命令回明确
/// 错误，而不是 panic 或静默换实现。这是 S1 定下的降级纪律。
pub(crate) struct PluginPorts {
    /// 工作区会话存储（`echo-agent.workspace` 插件；组合根按 persona 注入）。
    /// None = 未挂载（命令会回明确错误）。
    pub(crate) workspace_store:
        std::sync::RwLock<Option<Arc<crate::workspace::WorkspaceStore>>>,
    /// Shell 子系统句柄（`echo-agent.tools.builtin` 的 shell 工具族用它）。
    /// 句柄本身总是存在（内部空槽），bind 之后才有 manager。
    pub(crate) shell: echo_shell::ShellHandle,
    /// Subagent 插件运行态：子任务注册表 + spawn 执行闭包（组合根装配；
    /// None = 插件未启用，`spawn_subagent` 对模型不可见）。
    pub(crate) subagent: std::sync::RwLock<Option<crate::agent::SubagentRuntime>>,
    /// 联邦出口集合（querier / remote tool / subagent notifier / command
    /// handler）。P3 终态：构造一份、显式下发；不再是进程级 kernel cell。
    pub(crate) federation: std::sync::RwLock<Arc<crate::federation::FederationPorts>>,
}

impl PluginPorts {
    pub(crate) fn unlinked() -> Self {
        Self {
            workspace_store: std::sync::RwLock::new(None),
            shell: echo_shell::ShellHandle::new(),
            subagent: std::sync::RwLock::new(None),
            federation: std::sync::RwLock::new(Arc::new(
                crate::federation::FederationPorts::unlinked(),
            )),
        }
    }
}

/// 「我在哪个 manager 里」：反向弱引用 + 告警去重。
///
/// 弱引用而非强引用：manager 拥有全部 agent，agent 反向持有 manager 就是环
/// （drop 不掉 → 泄漏）。`warned` 让"没有 manager"这条告警只出现一次，
/// 而不是每轮 turn 一次。
#[derive(Default)]
pub(crate) struct ManagerLink {
    pub(crate) registry: std::sync::RwLock<Option<std::sync::Weak<dyn crate::agent::registry::AgentRegistry>>>,
    pub(crate) warned: std::sync::atomic::AtomicBool,
}
