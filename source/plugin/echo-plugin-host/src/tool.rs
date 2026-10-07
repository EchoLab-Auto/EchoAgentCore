//! 远程工具适配：把插件的工具贡献（[`ToolContribution`]）桥接为宿主侧
//! [`echo_defs::tool::Tool`]——无论插件是 inproc 还是子进程，经此适配后
//! 的用法与内置工具完全一致（注册进 `ToolRegistry` 即可被模型调用）。
//!
//! ## 调用上下文（`__session_id` 过渡桥）
//!
//! `Tool::execute` 签名不携带会话上下文；本适配器按过渡约定从参数中读取
//! `__session_id` / `__team_id` / `__branch_id`（若存在），**从发给插件的
//! payload 中剥离**后放入 [`InvokeContext`]。当宿主循环后续改为向远程工具
//! 注入这些键（替代硬编码特判）时，本桥自动生效。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use echo_defs::tool::{Tool, ToolError, ToolResult};

use crate::api::{Contribution, InvokeContext, InvokeOutcome, ToolContribution};
use crate::supervisor::PluginHandle;

/// 插件工具在宿主侧的 `Tool` 适配器。
pub struct RemoteTool {
    handle: PluginHandle,
    contribution: ToolContribution,
}

impl RemoteTool {
    /// 以已启动的插件句柄 + 工具贡献构造适配器。
    pub fn new(handle: PluginHandle, contribution: ToolContribution) -> Self {
        Self {
            handle,
            contribution,
        }
    }

    /// 该贡献对应的工具名。
    pub fn tool_name(&self) -> &str {
        &self.contribution.name
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
        // 统一归入 "plugin" 分组（前端显示为「插件」类）。
        "plugin"
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
            InvokeOutcome::Ok { text, images } => Ok(ToolResult::with_images(text, images)),
            InvokeOutcome::Error { code, message } => {
                Err(ToolError::Execution(format!("{message} ({code})")))
            }
        }
    }
}

impl PluginHandle {
    /// 把本插件的全部工具贡献包装为适配器（未注册工具贡献的插件返回空表）。
    pub fn remote_tools(&self) -> Vec<Arc<RemoteTool>> {
        self.contributions()
            .into_iter()
            .filter_map(|c| match c {
                Contribution::Tool(tool) => Some(Arc::new(RemoteTool::new(self.clone(), tool))),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        Contribution, PluginToHost, Register, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
    };
    use crate::inproc::{InprocIo, InprocPlugin, InprocTransport};
    use crate::supervisor::PluginSupervisor;
    use crate::transport::PluginSpec;
    use serde_json::json;

    /// 测试插件：注册 `add` 工具（返回两数之和）；`boom` 触发错误；
    /// 其余调用回 `unknown: <name>` 错误。
    struct CalcPlugin;

    impl InprocPlugin for CalcPlugin {
        fn run(
            self: Box<Self>,
            mut io: InprocIo,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                while let Some(msg) = io.recv().await {
                    match msg {
                        crate::api::HostToPlugin::Hello(hello) => {
                            let _ = io
                                .send(PluginToHost::Welcome(Welcome {
                                    protocol: PROTOCOL_NAME.into(),
                                    version: PROTOCOL_VERSION,
                                    plugin_id: hello.plugin_id,
                                    capabilities: vec!["tools".into()],
                                }))
                                .await;
                            let _ = io
                                .send(PluginToHost::Register(Register {
                                    contributions: vec![Contribution::Tool(ToolContribution {
                                        name: "add".into(),
                                        description: "两数之和".into(),
                                        parameters: json!({"type": "object"}),
                                        category: "plugin".into(),
                                        timeout_hint_secs: Some(5),
                                        package: None,
                                    })],
                                }))
                                .await;
                            let _ = io.send(PluginToHost::Ready).await;
                        }
                        crate::api::HostToPlugin::Invoke(inv) => {
                            let outcome = match inv.contribution.as_str() {
                                "add" => {
                                    let a = inv.payload["a"].as_f64().unwrap_or(0.0);
                                    let b = inv.payload["b"].as_f64().unwrap_or(0.0);
                                    InvokeOutcome::Ok {
                                        text: (a + b).to_string(),
                                        images: vec![],
                                    }
                                }
                                "ctx" => InvokeOutcome::Ok {
                                    text: format!(
                                        "session={}",
                                        inv.ctx.session_id.as_deref().unwrap_or("-")
                                    ),
                                    images: vec![],
                                },
                                other => InvokeOutcome::Error {
                                    code: "unknown_tool".into(),
                                    message: format!("no such tool: {other}"),
                                },
                            };
                            let _ = io
                                .send(PluginToHost::InvokeResult(crate::api::InvokeResult {
                                    call_id: inv.call_id,
                                    outcome,
                                }))
                                .await;
                        }
                        crate::api::HostToPlugin::Drain(_) => return,
                        crate::api::HostToPlugin::Dispose => return,
                        _ => {}
                    }
                }
            })
        }
    }

    async fn start_handle() -> PluginHandle {
        let transport =
            InprocTransport::new(|_spec: PluginSpec| Box::new(CalcPlugin) as Box<dyn InprocPlugin>);
        let supervisor = PluginSupervisor::new();
        supervisor
            .start(
                PluginSpec {
                    plugin_id: "calc".into(),
                    config: json!({}),
                },
                Box::new(transport),
            )
            .await
            .expect("plugin start")
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
    async fn remote_tool_maps_plugin_error() {
        let handle = start_handle().await;
        // 直接造一个指向不存在贡献的适配器：错误码应透传进 message。
        let contributions = handle.contributions();
        let Contribution::Tool(mut tc) = contributions.into_iter().next().unwrap() else {
            panic!("expected tool contribution");
        };
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
        let Contribution::Tool(tc) = handle.contributions().into_iter().next().unwrap() else {
            panic!("expected tool contribution");
        };
        let mut tc = tc;
        tc.name = "ctx".into();
        let tool = RemoteTool::new(handle.clone(), tc);
        // __session_id 应进 ctx 且不残留在 payload（插件将回显 session）。
        let out = tool
            .execute(json!({"__session_id": "s-1", "other": true}))
            .await
            .expect("ok");
        assert_eq!(out, "session=s-1");
    }

    #[tokio::test]
    async fn drain_shuts_plugin_down() {
        let handle = start_handle().await;
        handle
            .drain(Duration::from_secs(2))
            .await
            .expect("drain ok");
    }
}
