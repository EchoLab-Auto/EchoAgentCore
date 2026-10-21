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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fn when_cancelled(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
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
    pub team_id: Option<String>,
    /// 主 turn 的取消信号（`None` = 不可取消 / 无 turn 上下文）。
    pub cancel: Option<std::sync::Arc<dyn Cancellation>>,
}

impl ToolContext {
    /// 只带会话（直接调用 / 测试用）。
    pub fn for_session(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Default::default()
        }
    }

    /// 命名分支的会话上下文。
    pub fn for_branch(session_id: impl Into<String>, branch_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            branch_id: branch_id.into(),
            ..Default::default()
        }
    }

    /// 是否已被取消（无取消信号 = 永不取消）。
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.is_cancelled())
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("branch_id", &self.branch_id)
            .field("team_id", &self.team_id)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
}

/// A capability the LLM can invoke during the agent loop.
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
    /// 框架。默认 `false`；插件实例工具按声明给（见 `ToolSpec`）。
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

    /// 本工具是否产生**可见状态**，以及该状态在线上的标识（2026-11）。
    ///
    /// 返回 `Some(key)` = "我执行完会有新状态，请广播"；`key` 是宿主侧
    /// 协议映射表的键（例如 `"checklist"` → `ChecklistUpdated`）。
    ///
    /// 为什么放在 trait 上：执行链此前用 `if call.name == "checklist"` 判断，
    /// 等于把"哪个工具有状态面板"写成执行链里的一张名单——新增一个有状态的
    /// 工具要改执行链、改宿主分派、再改面板。现在这是**工具自己的声明**，
    /// 执行链只问一句（见 `echo_tool::execute::tool_state`）。
    fn state_key(&self) -> Option<&'static str> {
        None
    }

    /// 别的节点能不能**直接调我**（联邦远程调用面，2026-11 S8）。
    ///
    /// 默认 `false`——**不声明就不可远程调用**。默认值刻意取 fail-closed：
    /// 远程调用跨的是机器边界，而"我的参数里有没有会话/人格依赖、我会不会
    /// 改宿主状态"只有工具自己知道。此前这件事由两处硬编码名单回答
    /// （组合根握手宣告 6 个名字 + 联邦插件里再写一份同样的 6 个名字取交集），
    /// 后果是插件新加的工具**永远**进不了远程面，而工具改名会静默掉出去。
    ///
    /// 声明 `true` 的效果：本工具名出现在握手 `NodeCaps.tools` 里
    /// （见 `echo_federation::Federation::hello`），对端据此注册代理工具；
    /// 同时本机作为执行端时，联邦入口会放行对它的 Invoke。
    fn remote_capable(&self) -> bool {
        false
    }

    /// 我的哪些参数是**文件系统路径**（远程执行端的沙箱裁决用，2026-11 S8）。
    ///
    /// 前身是 `router.rs` 里的一张 `const FILE_TOOLS: [&str; 5]`：那意味着
    /// 插件新加的、同样读写文件的工具**不受**沙箱保护（真漏洞），而工具
    /// 改名就从保护里掉出来（静默）。参数名是工具自己的知识，所以问工具。
    ///
    /// 返回参数名列表而不是布尔：同一个工具可能既有 `path` 又有 `dest`。
    fn path_arguments(&self) -> &'static [&'static str] {
        &[]
    }
}
