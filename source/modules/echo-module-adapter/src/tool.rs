//! # 远程工具适配（`echo-module-adapter`）
//!
//! 把模块的工具贡献（[`ToolContribution`]）桥接为宿主侧
//! [`echo_defs::tool::Tool`]——无论模块是 inproc 还是子进程，经此适配后
//! 的用法与内置工具完全一致（注册进 `ToolRegistry` 即可被模型调用）。
//!
//! ## 为什么在 adapter crate 而不在 `echo-module-host`
//!
//! 本适配器认识 `echo-defs` 的 `Tool` trait——那是**agent 领域词汇**。
//! 宿主（`echo-module-host`）必须保持领域中立：它只认协议（
//! `echo-module-api`），因此能把「模块机制」整体复用到任何 Rust 服务上。
//! 适配器是「把远程调用伪装成本地 trait 对象」的那一层，属于 agent 框架侧；
//! 外迁后 `echo-module-host` 的 echo-* 依赖收窄到 `api` + `loader`，
//! 由 `scripts/deps-policy.txt` 与 `module_dependency_direction.rs` 双门禁守着。
//!
//! ## 调用上下文（`__session_id` 过渡桥）
//!
//! `Tool::execute` 签名不携带会话上下文；本模块实例按过渡约定从参数中读取
//! `__session_id` / `__team_id` / `__branch_id`（若存在），**从发给模块的
//! payload 中剥离**后放入 [`InvokeContext`]。当宿主循环后续改为向远程工具
//! 注入这些键（替代硬编码特判）时，本桥自动生效。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use echo_defs::tool::{Tool, ToolError, ToolResult};

use crate::contract::{tool_from_custom, ToolContribution};
use echo_module_host::api::{InvokeContext, InvokeOutcome};
use echo_module_host::ModuleHandle;

/// 模块工具在宿主侧的 `Tool` 模块实例。
pub struct RemoteTool {
    handle: ModuleHandle,
    contribution: ToolContribution,
}

impl RemoteTool {
    /// 以已启动的模块句柄 + 工具贡献构造模块实例。
    pub fn new(handle: ModuleHandle, contribution: ToolContribution) -> Self {
        Self {
            handle,
            contribution,
        }
    }
    /// 贡献声明的所属包 id（注册进 `ToolRegistry` 时用于前端按包分组）。
    pub fn package(&self) -> Option<&str> {
        self.contribution.package.as_deref()
    }
}

/// 从参数中剥离上下文键，返回（剥离后的 payload, 上下文）。
fn split_context(mut args: Value) -> (Value, InvokeContext) {
    let mut ctx = InvokeContext::default();
    if let Some(obj) = args.as_object_mut() {
        let take = |obj: &mut serde_json::Map<String, Value>, key: &str| {
            obj.remove(key).and_then(|v| v.as_str().map(str::to_string))
        };
        ctx.session_id = take(obj, "__session_id");
        ctx.team_id = take(obj, "__team_id");
        ctx.branch_id = take(obj, "__branch_id");
    }
    (args, ctx)
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.contribution.name
    }

    fn description(&self) -> &str {
        &self.contribution.description
    }

    fn category(&self) -> &'static str {
        // 贡献里的 category 是 String，本 trait 要求 &'static str；远程工具
        // 统一归入 "module" 分组（前端显示为「模块」类）。
        "module"
    }

    fn parameters(&self) -> Value {
        self.contribution.parameters.clone()
    }

    fn timeout_hint(&self, _arguments: &Value) -> Option<Duration> {
        self.contribution.timeout_hint_secs.map(Duration::from_secs)
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let result = self.execute_rich(arguments).await?;
        Ok(result.text)
    }

    async fn execute_rich(&self, arguments: Value) -> Result<ToolResult, ToolError> {
        let (payload, ctx) = split_context(arguments);
        let outcome = self
            .handle
            .invoke(&self.contribution.name, ctx, payload)
            .await;
        match outcome {
            InvokeOutcome::Ok {
                text,
                images,
                result,
            } => {
                // 结构化结果（W1.1，模块实例 op 场景）不进工具面：文本为空时以
                // JSON 文本兜底（防静默丢数据），非空文本保持原文。
                let text = if text.is_empty() && !result.is_null() {
                    result.to_string()
                } else {
                    text
                };
                Ok(ToolResult::with_images(text, images))
            }
            InvokeOutcome::Error { code, message } => {
                Err(ToolError::Execution(format!("{message} ({code})")))
            }
        }
    }
}

/// 模块句柄的工具视图（扩展 trait）。
///
/// 用扩展 trait 而非 `impl ModuleHandle`：`ModuleHandle` 定义在
/// `echo-module-host`，跨 crate 不能为它添加 inherent 方法——而「工具贡献
/// → `Tool` trait 对象」这层适配**必须**留在认识 `echo-defs` 的 crate 里。
pub trait ModuleHandleTools {
    /// 把本模块的全部工具贡献包装为模块实例（未注册工具贡献的模块返回空表）。
    fn remote_tools(&self) -> Vec<Arc<RemoteTool>>;
}

impl ModuleHandleTools for ModuleHandle {
    fn remote_tools(&self) -> Vec<Arc<RemoteTool>> {
        self.contributions()
            .into_iter()
            .filter_map(|c| {
                tool_from_custom(&c).map(|tool| Arc::new(RemoteTool::new(self.clone(), tool)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_module_host::api::{ModuleToHost, Register, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION};
    use echo_module_host::inproc::{InprocIo, InprocModule, InprocTransport};
    use echo_module_host::ModuleSpec;
    use echo_module_host::ModuleSupervisor;
    use serde_json::json;

    /// 测试模块：注册 `add` 工具（返回两数之和）；`boom` 触发错误；
    /// 其余调用回 `unknown: <name>` 错误。
    struct CalcModule;

    impl InprocModule for CalcModule {
        fn run(
            self: Box<Self>,
            mut io: InprocIo,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                while let Some(msg) = io.recv().await {
                    match msg {
                        echo_module_host::api::HostToModule::Hello(hello) => {
                            let _ = io
                                .send(ModuleToHost::Welcome(Welcome {
                                    protocol: PROTOCOL_NAME.into(),
                                    version: PROTOCOL_VERSION,
                                    module_id: hello.module_id,
                                    capabilities: vec!["tools".into()],
                                }))
                                .await;
                            let _ = io
                                .send(ModuleToHost::Register(Register {
                                    contributions: vec![ToolContribution {
                                        name: "add".into(),
                                        description: "两数之和".into(),
                                        parameters: json!({"type": "object"}),
                                        category: "module".into(),
                                        timeout_hint_secs: Some(5),
                                        package: None,
                                        visible_reply: false,
                                    }
                                    .into_custom()],
                                }))
                                .await;
                            let _ = io.send(ModuleToHost::Ready).await;
                        }
                        echo_module_host::api::HostToModule::Invoke(inv) => {
                            let outcome = match inv.contribution.as_str() {
                                "add" => {
                                    let a = inv.payload["a"].as_f64().unwrap_or(0.0);
                                    let b = inv.payload["b"].as_f64().unwrap_or(0.0);
                                    InvokeOutcome::ok((a + b).to_string())
                                }
                                "ctx" => InvokeOutcome::ok(format!(
                                    "session={}",
                                    inv.ctx.session_id.as_deref().unwrap_or("-")
                                )),
                                other => InvokeOutcome::Error {
                                    code: "unknown_tool".into(),
                                    message: format!("no such tool: {other}"),
                                },
                            };
                            let _ = io
                                .send(ModuleToHost::InvokeResult(
                                    echo_module_host::api::InvokeResult {
                                        call_id: inv.call_id,
                                        outcome,
                                    },
                                ))
                                .await;
                        }
                        echo_module_host::api::HostToModule::Drain(_) => return,
                        echo_module_host::api::HostToModule::Dispose => return,
                        _ => {}
                    }
                }
            })
        }
    }

    async fn start_handle() -> ModuleHandle {
        let transport =
            InprocTransport::new(|_spec: ModuleSpec| Box::new(CalcModule) as Box<dyn InprocModule>);
        let supervisor = ModuleSupervisor::new();
        supervisor
            .start(
                ModuleSpec {
                    module_id: "calc".into(),
                    config: json!({}),
                },
                Box::new(transport),
            )
            .await
            .expect("module start")
    }

    #[tokio::test]
    async fn remote_tool_implements_tool_trait() {
        let handle = start_handle().await;
        let tools = handle.remote_tools();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];

        assert_eq!(tool.name(), "add");
        assert_eq!(tool.description(), "两数之和");
        let hint = tool.timeout_hint(&json!({})).expect("timeout hint");
        assert_eq!(hint, Duration::from_secs(5));

        let out = tool.execute(json!({"a": 2, "b": 3})).await.expect("ok");
        assert_eq!(out, "5");
    }

    #[tokio::test]
    async fn remote_tool_maps_module_error() {
        let handle = start_handle().await;
        // 直接造一个指向不存在贡献的模块实例：错误码应透传进 message。
        let contributions = handle.contributions();
        let mut tc = tool_from_custom(&contributions.into_iter().next().unwrap())
            .expect("expected tool contribution");
        tc.name = "missing".into();
        let tool = RemoteTool::new(handle.clone(), tc);
        let err = tool.execute(json!({})).await.expect_err("error");
        let text = err.to_string();
        assert!(text.contains("unknown_tool"), "got: {text}");
        assert!(text.contains("no such tool: missing"), "got: {text}");
    }

    #[tokio::test]
    async fn context_keys_are_split_from_payload() {
        let handle = start_handle().await;
        let tc = tool_from_custom(&handle.contributions().into_iter().next().unwrap())
            .expect("expected tool contribution");
        let mut tc = tc;
        tc.name = "ctx".into();
        let tool = RemoteTool::new(handle.clone(), tc);
        // __session_id 应进 ctx 且不残留在 payload（模块将回显 session）。
        let out = tool
            .execute(json!({"__session_id": "s-1", "other": true}))
            .await
            .expect("ok");
        assert_eq!(out, "session=s-1");
    }

    #[tokio::test]
    async fn drain_shuts_module_down() {
        let handle = start_handle().await;
        handle
            .drain(Duration::from_secs(2))
            .await
            .expect("drain ok");
    }
}
