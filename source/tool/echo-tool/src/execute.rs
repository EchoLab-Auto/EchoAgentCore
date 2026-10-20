//! 工具执行链（机制）：一次调用的完整生命周期，宿主只提供 4 个接缝。
//!
//! ## 为什么它在这里，而不在宿主里
//!
//! 2026-11 之前这段逻辑是 `Agent::run_tool` + `dispatch_tool`（374 行，两个
//! 文件）：它同时做**机制**（参数预检、超时口径、调用/结果成对落盘、出口
//! 脱敏）与**宿主策略**（面板事件长什么样、会话日志怎么存、编排工具走哪条
//! 路）。混在一起的结果是：任何宿主想复用同一条链（远程工具、子代理、测试
//! 驱动）都得复制它，而"哪个工具会产生状态变化"只能写成
//! `if call.name == "checklist"`。
//!
//! 现在两边分开：
//!
//! | 机制（本模块） | 宿主（[`ToolExecutionHost`]） |
//! |---|---|
//! | 参数 JSON 解析 + schema 预检 | 面板事件（`on_call` / `on_result`） |
//! | 执行 + 结果出口脱敏 | 会话事件日志的写法 |
//! | 耗时测量、span 归组 | 编排类工具（宿主自有实现） |
//! | 状态广播的**触发时机** | 状态广播的**线上形状** |
//!
//! ## 状态广播为什么不是特判
//!
//! 工具在 [`Tool::state_key`] 上声明"我有可见状态"，执行链在结果落盘后
//! 统一问一句、有就交给宿主。于是"哪个工具会广播状态"是**声明**，不是
//! 执行链里的一张名单——新增一个有状态面板的工具不需要改这里，也不需要
//! 改宿主的分派分支（宿主只维护 `state_key → 线上事件变体` 的协议映射表）。

use std::sync::Arc;

use echo_defs::tool::{Tool, ToolResult};
use serde_json::Value;

use crate::ToolRegistry;

/// 一次工具调用的**会话坐标**（执行链与工具自己都要的三件事）。
#[derive(Debug, Clone, Default)]
pub struct ToolCallContext {
    pub session_id: String,
    pub branch_id: String,
    /// 人格 id（编排类工具注入给工具的参数会用到；`None` = 无归属事件）。
    pub team_id: Option<String>,
}

/// 待执行的一次调用（机制层的词汇，故意不依赖任何 LLM 协议类型）。
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    /// 调用 id（事件成对、面板行、会话日志三处共用的键）。
    pub id: String,
    pub name: String,
    /// 原始参数**文本**（模型的输出）；解析失败时错误文案要把它回显给模型。
    pub arguments: String,
}

impl ToolInvocation {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// 执行链的宿主接缝：只有宿主知道怎么做的事，全在这里。
///
/// 刻意只有四个方法——每多一个都意味着执行链多知道一件宿主内部结构。
pub trait ToolExecutionHost: Send + Sync {
    /// 出口脱敏：返回（脱敏后文本，命中数）。
    ///
    /// 执行链用它处理**参数**（事件与落盘的记录面）与**结果**（模型可见面）；
    /// 执行本身用原始参数（功能正确性不受脱敏影响）。
    fn redact(&self, text: &str) -> (String, usize);

    /// 调用开始：面板事件 + 会话事件日志的 `ToolCall`。
    ///
    /// `logged_arguments` 是**已脱敏**的参数文本——两处记录面都不得出现明文
    /// 秘密，所以脱敏在执行链里做一次、由宿主写两处。
    fn on_call(
        &self,
        cx: &ToolCallContext,
        call: &ToolInvocation,
        logged_arguments: &str,
        started_at_ms: i64,
    );

    /// 结果落盘：面板事件 + 会话事件日志的 `ToolResult` + 状态广播。
    ///
    /// 状态广播放在这里而不是执行链里，是因为"状态快照怎么变成线上事件"是
    /// 协议问题；执行链只负责问 [`Tool::state_key`]。
    fn on_result(
        &self,
        cx: &ToolCallContext,
        call: &ToolInvocation,
        result: &ToolResult,
        elapsed_ms: u64,
    );

    /// 编排类工具的执行入口（宿主自有实现，不在注册表里）。
    ///
    /// 返回 `None` = 不是编排工具，走注册表。为什么不用 async：真实用例
    /// （`spawn_subagent`）是**发起**一个后台任务，同步返回文本即可；把签名
    /// 做成 async 只会逼机制层引入 `async_trait` 装箱。
    fn run_orchestration(
        &self,
        _cx: &ToolCallContext,
        _call: &ToolInvocation,
        _args: &Value,
    ) -> Option<Result<ToolResult, echo_defs::tool::ToolError>> {
        None
    }

    /// 执行前给工具补的**会话参数**（默认原样返回）。
    ///
    /// 历史上是执行链里按工具名注入 `__session_id` / `__team_id`；现在是宿主
    /// 的映射（它知道哪些工具按会话隔离状态），执行链不认识工具名。
    fn tool_arguments(&self, _cx: &ToolCallContext, _tool: &str, args: Value) -> Value {
        args
    }
}

/// 参数预检：schema 声明的必需字段缺失时返回纠正性错误文案。
///
/// 设计动机：模型在长工具循环中可能退化出空参调用（如 `bash {}`），
/// 若错误反馈只是模糊的"command required"，模型不知道错在哪，会原样重试
/// 形成退化循环，烧掉整段上下文预算。
pub fn invalid_tool_arguments(
    tool_name: &str,
    raw_arguments: &str,
    args: &Value,
    schema: &Value,
) -> Option<String> {
    let required: Vec<&str> = schema["required"]
        .as_array()?
        .iter()
        .filter_map(|field| field.as_str())
        .collect();
    if required.is_empty() {
        return None;
    }
    // 缺失 = 键不存在或值为 null（空字符串保留给各工具自己判定，
    // 避免把 write_file content:"" 这类合法调用误判为缺参）。
    let missing: Vec<&str> = match args.as_object() {
        Some(obj) => required
            .iter()
            .filter(|field| obj.get(**field).is_none_or(|v| v.is_null()))
            .copied()
            .collect(),
        None => required.clone(),
    };
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "工具参数无效: {tool_name} 缺少必需参数 {}（你发送的参数: {}）。\
         该工具的参数 schema: {}。请按 schema 携带全部必需参数重新调用。",
        missing.join(", "),
        truncate(raw_arguments, 200),
        schema,
    ))
}

/// 文本截断（错误文案回显用）——只按字符边界截，不破坏 UTF-8。
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

/// 工具执行器：一条工具调用从参数解析到结果脱敏的完整路径。
///
/// 只持有注册表；宿主接缝**按调用传入**（`&dyn ToolExecutionHost`）而不是
/// 存在字段里——于是宿主侧的实现可以是借用其自身状态的适配器（`&Agent`），
/// 不必先造一个 `Arc<Agent>` 塞进来（那会成引用环，或者要一个 "自引用 Arc"
/// 的补丁字段）。`&self` 可并发：一次 turn 里多个工具并行时共用一个执行器。
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// 执行一次工具调用。
    ///
    /// 顺序是**刻意的**：调用事件先落（模型可见的循环必须能从日志重建，
    /// 中途被取消也留得下配对），执行裹在 span 里（并发工具 / 多会话下
    /// "这几条 warn 属于哪个工具"靠它归组），结果最后落。
    pub async fn run(
        &self,
        host: &dyn ToolExecutionHost,
        cx: &ToolCallContext,
        call: &ToolInvocation,
    ) -> ToolResult {
        let started = std::time::Instant::now();
        let started_at_ms = chrono_millis();
        // 参数出口脱敏：事件 / 会话落盘 / 面板都不得出现明文秘密。
        let logged_arguments = host.redact(&call.arguments).0;
        host.on_call(cx, call, &logged_arguments, started_at_ms);

        let span = tracing::info_span!(
            "tool_call",
            tool = %call.name,
            session = %cx.session_id,
            branch_id = %cx.branch_id,
            tool_call_id = %call.id,
        );
        let result = self.dispatch(host, cx, call).instrument(span).await;

        let elapsed_ms = started.elapsed().as_millis() as u64;
        // 工具耗时是 turn 耗时的主要成分：慢调用与失败恒 INFO（默认级别下
        // 「时间花在哪」有据可查），快调用降 DEBUG（不给正常路径刷噪声）。
        let characters = result.text.chars().count();
        let failed = result.text.starts_with("error:");
        if elapsed_ms >= 1000 || failed {
            tracing::info!(
                elapsed_ms,
                chars = characters,
                failed,
                "tool finished"
            );
        } else {
            tracing::debug!(
                elapsed_ms,
                chars = characters,
                failed,
                "tool finished"
            );
        }
        host.on_result(cx, call, &result, elapsed_ms);
        result
    }

    /// 执行体：参数解析 → 预检 → （编排工具 | 注册表）→ 结果脱敏。
    ///
    /// 与 [`Self::run`] 拆开是为了让整段执行被 `tool_call` span 包裹：开头的
    /// 同步序言（发出调用事件 + 落盘）不应计入工具耗时。
    async fn dispatch(
        &self,
        host: &dyn ToolExecutionHost,
        cx: &ToolCallContext,
        call: &ToolInvocation,
    ) -> ToolResult {
        let outcome = match serde_json::from_str::<Value>(&call.arguments) {
            Err(error) => Err(format!(
                "error: 工具参数不是合法 JSON（{error}）。你发送的原始参数: {}。\
                 请修正为合法 JSON 后重新调用 {}。",
                truncate(&call.arguments, 200),
                call.name,
            )),
            Ok(args) => {
                // 编排工具优先：它们不在注册表里（宿主的内存实现），因此
                // 不能走 schema 预检的注册表查询。
                match host.run_orchestration(cx, call, &args) {
                    Some(Ok(result)) => Ok(result),
                    Some(Err(error)) => Err(error.to_string()),
                    None => match self.preflight(call, &args).await {
                        Some(message) => Err(message),
                        None => {
                            let args = host.tool_arguments(cx, &call.name, args);
                            self.registry
                                .execute_rich(&call.name, args)
                                .await
                                .map_err(|error| error.to_string())
                        }
                    },
                }
            }
        };
        let mut result = outcome
            .unwrap_or_else(|error| ToolResult::text(format!("error: {error}")));
        // 结果出口脱敏（第二道保险：工具自己也可能把秘密读进结果）。
        let (result_text, hit_count) = host.redact(&result.text);
        if hit_count > 0 {
            tracing::warn!(
                session = %cx.session_id,
                tool = %call.name,
                hits = hit_count,
                "tool result contained sensitive data; redacted at exit"
            );
        }
        result.text = result_text;
        result
    }

    async fn preflight(&self, call: &ToolInvocation, args: &Value) -> Option<String> {
        let schema = self.registry.parameters(&call.name).await?;
        invalid_tool_arguments(&call.name, &call.arguments, args, &schema)
    }
}

/// 状态广播：执行链在结果落盘后问一句"这个工具有可见状态吗"。
///
/// 参数是注册表与该工具名；返回 `(state_key, 该会话的快照)`。抽成自由函数
/// 是为了让宿主拿到它而不必再持有执行器的内部状态。
pub fn tool_state(
    registry: &ToolRegistry,
    tool_name: &str,
    session_id: &str,
) -> Option<(&'static str, Value)> {
    let tool: Arc<dyn Tool> = registry.get(tool_name)?;
    let key = tool.state_key()?;
    let snapshot = registry.snapshot_for_session(tool_name, session_id)?;
    Some((key, snapshot))
}

fn chrono_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

use tracing::Instrument as _;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use echo_defs::tool::ToolError;
    use std::sync::Mutex;

    /// 记录宿主侧收到的每一次回调（顺序即协议顺序）。
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
        redactions: Mutex<usize>,
    }

    impl ToolExecutionHost for Recorder {
        fn redact(&self, text: &str) -> (String, usize) {
            *self.redactions.lock().unwrap() += 1;
            (text.replace("SECRET", "***"), 1)
        }
        fn on_call(
            &self,
            _cx: &ToolCallContext,
            call: &ToolInvocation,
            logged_arguments: &str,
            _started_at_ms: i64,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(format!("call:{}:{logged_arguments}", call.name));
        }
        fn on_result(
            &self,
            _cx: &ToolCallContext,
            call: &ToolInvocation,
            result: &ToolResult,
            _elapsed_ms: u64,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(format!("result:{}:{}", call.name, result.text));
        }
        fn tool_arguments(&self, cx: &ToolCallContext, tool: &str, mut args: Value) -> Value {
            if tool == "echo" {
                args["__session_id"] = Value::String(cx.session_id.clone());
            }
            args
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "回显 text"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
        }
        async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
            Ok(format!(
                "text={} session={}",
                arguments["text"].as_str().unwrap_or_default(),
                arguments["__session_id"].as_str().unwrap_or("none"),
            ))
        }
    }

    fn executor(with_echo: bool) -> (ToolExecutor, Recorder) {
        let mut registry = ToolRegistry::new();
        if with_echo {
            registry.register(Arc::new(EchoTool));
        }
        (ToolExecutor::new(Arc::new(registry)), Recorder::default())
    }

    fn cx() -> ToolCallContext {
        ToolCallContext {
            session_id: "s1".into(),
            branch_id: "b1".into(),
            team_id: None,
        }
    }

    /// 一次成功调用：先 call 后 result，两处都过脱敏，工具拿到会话参数。
    #[tokio::test]
    async fn successful_call_records_paired_events_and_injects_session() {
        let (ex, rec) = executor(true);
        let call = ToolInvocation::new("c1", "echo", r#"{"text":"hi"}"#);
        let result = ex.run(&rec, &cx(), &call).await;
        assert_eq!(result.text, "text=hi session=s1");
        assert_eq!(
            *rec.events.lock().unwrap(),
            vec![
                r#"call:echo:{"text":"hi"}"#.to_string(),
                "result:echo:text=hi session=s1".to_string(),
            ]
        );
    }

    /// 参数里的秘密在**记录面**被脱敏，执行面不受影响。
    #[tokio::test]
    async fn arguments_are_redacted_for_records_but_not_for_execution() {
        let (ex, rec) = executor(true);
        let call = ToolInvocation::new("c1", "echo", r#"{"text":"SECRET"}"#);
        let result = ex.run(&rec, &cx(), &call).await;
        assert_eq!(result.text, "text=*** session=s1", "结果面也要脱敏");
        let events = rec.events.lock().unwrap();
        assert!(
            events[0].contains("***"),
            "记录面不得出现明文：{}",
            events[0]
        );
    }

    /// 缺必需参数 → 纠正性错误（说清缺什么），且**不进**注册表执行。
    #[tokio::test]
    async fn missing_required_argument_gets_a_corrective_error() {
        let (ex, rec) = executor(true);
        let result = ex
            .run(&rec, &cx(), &ToolInvocation::new("c1", "echo", "{}"))
            .await;
        assert!(result.text.starts_with("error: 工具参数无效"), "{}", result.text);
        assert!(result.text.contains("text"), "要指出缺哪个字段：{}", result.text);
    }

    /// 非法 JSON → 回显原始参数，错误以 `error:` 开头（模型的失败判据）。
    #[tokio::test]
    async fn malformed_json_reports_the_raw_arguments() {
        let (ex, rec) = executor(true);
        let result = ex
            .run(&rec, &cx(), &ToolInvocation::new("c1", "echo", "{not json"))
            .await;
        assert!(result.text.starts_with("error:"), "{}", result.text);
        assert!(result.text.contains("{not json"), "{}", result.text);
    }

    /// 未注册的工具 → 错误文本（不是 panic，也不悄悄成功）。
    #[tokio::test]
    async fn unknown_tool_reports_an_error() {
        let (ex, rec) = executor(false);
        let result = ex
            .run(&rec, &cx(), &ToolInvocation::new("c1", "nope", "{}"))
            .await;
        assert!(result.text.starts_with("error:"), "{}", result.text);
    }

    /// 编排工具：宿主实现的路径优先于注册表（即使注册表里有同名工具）。
    #[tokio::test]
    async fn orchestration_tool_takes_precedence_over_the_registry() {
        struct Orch;
        impl ToolExecutionHost for Orch {
            fn redact(&self, text: &str) -> (String, usize) {
                (text.to_string(), 0)
            }
            fn on_call(&self, _: &ToolCallContext, _: &ToolInvocation, _: &str, _: i64) {}
            fn on_result(&self, _: &ToolCallContext, _: &ToolInvocation, _: &ToolResult, _: u64) {}
            fn run_orchestration(
                &self,
                _cx: &ToolCallContext,
                call: &ToolInvocation,
                _args: &Value,
            ) -> Option<Result<ToolResult, ToolError>> {
                (call.name == "spawn").then(|| Ok(ToolResult::text("orchestrated")))
            }
        }
        let ex = ToolExecutor::new(Arc::new(ToolRegistry::new()));
        let result = ex
            .run(&Orch, &cx(), &ToolInvocation::new("c1", "spawn", "{}"))
            .await;
        assert_eq!(result.text, "orchestrated");
    }
}
