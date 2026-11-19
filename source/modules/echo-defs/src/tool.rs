//! The `Tool` capability seam — Service Definition role.
//!
//! A tool is a named, described, executable capability the LLM can call
//! during the agent loop. The definition is model-facing: schema, results,
//! and errors are plain text/JSON with no UI or transport vocabulary.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// Tool execution errors, all model-facing text.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("工具参数无效: {0}")]
    InvalidArguments(String),
    #[error("工具执行失败: {0}")]
    Execution(String),
    #[error("找不到工具: {0}")]
    NotFound(String),
}

/// Tool schema sent to the LLM (OpenAI function-calling format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

/// The result of a tool execution: model-visible text plus optional
/// multimodal media (image URLs or `data:` URIs).
///
/// `text` stays the primary channel; `images` lets a tool hand pictures to a
/// vision-capable model through the same caller (e.g. screenshots, generated
/// charts, search result thumbnails).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub images: Vec<String>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: vec![],
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

impl From<String> for ToolResult {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for ToolResult {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

/// 取消信号（定义层词汇：**不绑定**具体 cancellation 实现）。
///
/// 为什么需要它：编排类工具（`spawn_subagent`）要在主 turn 取消时一并停掉，
/// 而"主 turn 的取消令牌"是宿主的运行期对象。把 `tokio_util::CancellationToken`
/// 写进定义层会把一个运行时类型钉进词汇表；这里只描述**能力**：
/// 「能被问是否已取消」+「能等它取消」。
///
/// 实现方通常是 `CancellationToken` 的一层薄包装（见 `echo-agent`）。
pub trait Cancellation: Send + Sync {
    /// 是否已经取消（同步、廉价、可反复问）。
    fn is_cancelled(&self) -> bool;
    /// 取消完成时 resolve；**已取消则立即完成**。
    ///
    /// 返回 boxed future 是为了对象安全（`dyn Cancellation`）。
    fn when_cancelled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// 一次工具调用的**上下文**（工具可以要，但大多数不需要）。
///
/// 历史：这些信息此前经**魔术参数**注入——宿主往 JSON 里塞 `__session_id` /
/// `__team_id`，工具再从参数里读。两个问题：
///
/// 1. 参数是模型可见面的形状，塞宿主私有键意味着**契约里没有的东西混进了
///    参数 JSON**（远程调用 / 直接 registry 执行时还没有，于是工具得写
///    "缺失就兜底"的分支）；
/// 2. 每加一个维度（分支 id、取消信号）都要改一遍注入表和每个工具。
///
/// 现在它是显式入参：[`Tool::execute_in`]。不覆写该方法的工具完全看不到它。
#[derive(Default, Clone)]
pub struct ToolContext {
    /// 会话键（`local:tui::local_user` / `qq:group:1:2` …）。
    pub session_id: String,
    /// turn 分支 id（并发分支的归属渲染；空 = 无分支语义）。
    pub branch_id: String,
    /// persona id（`None` = 无归属，如进程级调用）。
    ///
    /// **不是授权凭据**：它来自宿主执行链的上下文，绝不从工具参数读取。
    /// 历史参数 `__team_id` 已废弃——参数是模型可见面，不可作为身份。
    pub team_id: Option<String>,
    /// 可信来源主体：跨节点调用时由**传输层**（联邦 `handle_invoke`）注入的
    /// 对端 `node_id`；`None` = 本机来源（本地 turn / 面板 / 单测）。
    ///
    /// 这是身份，不是数据：
    /// - 只能由传输/宿主注入，**永远不从 `arguments` 里读**（参数可由模型或
    ///   对端任意构造，拿它当授权等于把钥匙交给被授权者）；
    /// - 工具（如 shell 会话族）用它把会话归属绑定到**来源节点**，从而拒绝
    ///   跨 peer 与本地 persona 的默认访问；
    /// - 随机/不透明 id（如 `sh-1`）只是句柄，**不得**单独作为授权依据。
    pub peer: Option<String>,
    /// 主 turn 的取消信号（`None` = 不可取消 / 无 turn 上下文）。
    pub cancel: Option<std::sync::Arc<dyn Cancellation>>,
}

impl ToolContext {
    /// 是否已被取消（无取消信号 = 永不取消）。
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
    }

    /// 构造**远端来源**上下文：唯一合法的 `peer` 注入点（供传输层使用）。
    ///
    /// 远端调用没有本地 persona / 会话坐标：`team_id` / `session_id` 恒为空，
    /// 身份只有 `peer`。工具据此把资源归属绑定到来源节点。
    pub fn for_remote_peer(peer: impl Into<String>) -> Self {
        Self {
            peer: Some(peer.into()),
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("branch_id", &self.branch_id)
            .field("team_id", &self.team_id)
            .field("peer", &self.peer)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// UI grouping category (empty = "内置" on the frontend).
    fn category(&self) -> &'static str {
        "builtin"
    }
    /// 成功调用本工具是否**即产生平台可见输出**（发给用户的真实回复）。
    ///
    /// 循环用它决定「临时等待回复」要不要抑制——此前按工具名前缀/固定名单
    /// 判断（`send_private_msg` | `send_group_msg` | …），等于把平台语义写进
    /// 框架。默认 `false`；模块实例工具按声明给（见 `ToolSpec`）。
    fn produces_visible_output(&self) -> bool {
        false
    }
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    /// Execute the tool, returning model-visible text.
    async fn execute(&self, arguments: Value) -> Result<String, ToolError>;

    /// Self-declared execution timeout parsed from the tool's own arguments.
    /// The agent loop's outer guard uses `max(configured tool_timeout_secs,
    /// hint + grace)` so a tool that explicitly allows a long run (e.g.
    /// `bash` with `timeout_secs: 300`) is not cut short by the
    /// generic guard. `None` = no hint; the configured default applies.
    fn timeout_hint(&self, _arguments: &Value) -> Option<std::time::Duration> {
        None
    }

    /// Execute the tool with multimodal output. Defaults to [`execute`] with
    /// no images; tools that produce media override this to attach them.
    async fn execute_rich(&self, arguments: Value) -> Result<ToolResult, ToolError> {
        let text = self.execute(arguments).await?;
        Ok(ToolResult::text(text))
    }

    /// 带**调用上下文**的执行入口。
    ///
    /// 默认实现丢掉上下文、转调 [`Self::execute`]——绝大多数工具不需要会话 /
    /// 人格 / 取消信号。需要它们的工具（清单按会话隔离、shell 按人格归属、
    /// 子代理传播主 turn 取消）覆写本方法，于是不必再靠魔术参数或执行链里的
    /// 工具名特判。
    ///
    /// 覆盖者请**不要**假设 `cx` 一定有值：直接调用（面板 / 远程 / 单测）走的
    /// 是 [`Self::execute`]，那时上下文为空。
    async fn execute_in(
        &self,
        _cx: &ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        self.execute_rich(arguments).await
    }

    /// Optional structured state snapshot for UI visualization. Tools that
    /// expose a state panel (e.g. checklist) override this; the agent emits
    /// the returned value in a `ChecklistUpdated` event after each execution.
    /// This is a UI projection that will move out of the definition layer
    /// (Phase 4: state panels become event subscriptions).
    ///
    /// **Schema 约束**：返回对象的键必须是**常量 schema**，用户内容只能
    /// 出现在值里、**不得**做键。快照出口按工具声明
    /// （[`Self::snapshot_machine_fields`]）按键名豁免机器字段；用户可控
    /// 的键名会让脱敏无法区分机器字段与用户数据。
    fn snapshot(&self) -> Option<Value> {
        None
    }

    /// 按**会话**过滤的 UI 投影快照（2026-10）。工具状态带会话维度时
    /// 覆盖此方法（checklist）；默认退化为 [`Self::snapshot`]——无会话
    /// 维度的工具不受影响。事件侧（`ChecklistUpdated`）用本方法，避免把
    /// 其他会话的状态挂到当前会话上广播。
    fn snapshot_for_session(&self, _session_id: &str) -> Option<Value> {
        self.snapshot()
    }

    /// 快照里哪些**对象键**是机器字段（稳定 id / 版本号 / 会话键等），
    /// 出口脱敏时只跳过这些键的 `String` 值。
    ///
    /// 为什么需要它：注册表在快照出口递归脱敏所有字符串值，但状态面板 /
    /// 远程消费方要靠稳定 id（`item_id`、`list_id`、`epoch` 等）定位条目；
    /// 把 UUID 换成占位符，后续 `update_item` / `remove_item` 就再也对不上。
    /// 机器字段不是用户内容，不应按秘密处理。
    ///
    /// **安全约束（硬性）**：快照对象键必须是**常量 schema**，用户内容
    /// **不得**做键。脱敏按「键名」判定：若用户能控制键名，就能把秘密放进
    /// 声明为机器字段的键里绕过脱敏。用户内容只能出现在值里（`name` /
    /// `text` / `summary` / `display` …），这些键不得出现在本列表。
    ///
    /// 默认空 = 全部字符串值都脱敏（fail-closed；新工具不声明就得到最严格
    /// 的处理）。只跳过该键的**字符串**值；若机器键下意外出现对象 / 数组，
    /// 其中内容仍会递归脱敏。
    fn snapshot_machine_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// 本工具是否产生**可见状态**，以及该状态在线上的标识（2026-11）。
    ///
    /// 返回 `Some(key)` = "我执行完会有新状态，请广播"；`key` 是宿主侧
    /// 协议映射表的键（例如 `"checklist"` → `ChecklistUpdated`）。
    ///
    /// 为什么放在 trait 上：执行链此前用 `if call.name == "checklist"` 判断，
    /// 等于把"哪个工具有状态面板"写成执行链里的一张名单——新增一个有状态的
    /// 工具要改执行链、改宿主分派、再改面板。现在这是**工具自己的声明**，
    /// 执行链只问一句（见 `agent_module_tools::execute::tool_state`）。
    fn state_key(&self) -> Option<&'static str> {
        None
    }

    /// 别的节点能不能**直接调我**（联邦远程调用面，2026-11 S8）。
    ///
    /// 默认 `false`——**不声明就不可远程调用**。默认值刻意取 fail-closed：
    /// 远程调用跨的是机器边界，而"我的参数里有没有会话/人格依赖、我会不会
    /// 改宿主状态"只有工具自己知道。此前这件事由两处硬编码名单回答
    /// （组合根握手宣告 6 个名字 + 联邦模块里再写一份同样的 6 个名字取交集），
    /// 后果是模块新加的工具**永远**进不了远程面，而工具改名会静默掉出去。
    ///
    /// 声明 `true` 的效果：本工具名出现在握手 `NodeCaps.tools` 里
    /// （见 `agent_module_federation::Federation::hello`），对端据此注册代理工具；
    /// 同时本机作为执行端时，联邦入口会放行对它的 Invoke。
    fn remote_capable(&self) -> bool {
        false
    }

    /// 我的哪些参数是**文件系统路径**（远程执行端的沙箱裁决用，2026-11 S8）。
    ///
    /// 前身是 `router.rs` 里的一张 `const FILE_TOOLS: [&str; 5]`：那意味着
    /// 模块新加的、同样读写文件的工具**不受**沙箱保护（真漏洞），而工具
    /// 改名就从保护里掉出来（静默）。参数名是工具自己的知识，所以问工具。
    ///
    /// 返回参数名列表而不是布尔：同一个工具可能既有 `path` 又有 `dest`。
    fn path_arguments(&self) -> &'static [&'static str] {
        &[]
    }
}
