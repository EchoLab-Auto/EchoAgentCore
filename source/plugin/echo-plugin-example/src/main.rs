//! # echo-plugin-example — Phase 3 试点插件
//!
//! 把内置工具 `calculator` 与 `web_search` 移植为**独立进程插件**（见
//! `document/decoupling-plan.md`）：实现 [`PluginHandler`] 后交给
//! [`serve_stdio`]，宿主经 stdio 帧协议（4 字节 LE 长度前缀 + JSON）调用。
//!
//! 工具逻辑自 `source/backend/echo-agent/src/packages/tools_builtin/`
//! （`calculator.rs` / `websearch.rs`）**复制移植**，原文件未改动：
//!
//! - `calculator`：递归下降解析 `+ - * / ( )` 与数字；参数
//!   `{"expression": "..."}`；成功返回数字字符串；缺参 / 解析失败 →
//!   `Error{code:"invalid_arguments"}`；
//! - `web_search`：Bing RSS 搜索（reqwest + regex）；参数
//!   `{"query", "max_results"}`；文本格式与内置工具一致；网络失败 →
//!   `Error{code:"upstream"}`。
//!
//! 运行：`ECHO_PLUGIN_ID=echo-plugin-example echo-plugin-example`（stdio 帧协议）。

mod calculator;
mod websearch;

use echo_plugin_sdk::{
    async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, PluginHandler,
};

/// 试点插件：注册 calculator 与 web_search 两个工具。
struct ExamplePlugin {
    /// web_search 的 HTTP 客户端（实例级；插件遵守零进程级全局态纪律）。
    web: websearch::WebSearch,
}

impl ExamplePlugin {
    fn new() -> Self {
        Self {
            web: websearch::WebSearch::new(),
        }
    }
}

#[async_trait]
impl PluginHandler for ExamplePlugin {
    fn contributions(&self) -> Vec<Contribution> {
        vec![
            Contribution::Tool(calculator::tool()),
            Contribution::Tool(websearch::tool()),
        ]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        match req.contribution.as_str() {
            "calculator" => calculator::invoke(&req.payload),
            "web_search" => self.web.invoke(&req.payload).await,
            other => InvokeOutcome::Error {
                code: "unknown_contribution".to_string(),
                message: format!("未知工具: {other}"),
            },
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = serve_stdio(ExamplePlugin::new()).await {
        eprintln!("echo-plugin-example: 插件退出（错误）: {error}");
        std::process::exit(1);
    }
}
