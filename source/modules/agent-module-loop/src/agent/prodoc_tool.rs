//! `prodoc_check` 工具的**宿主侧代理**。
//!
//! 检查逻辑在模块仓（`agent-module-prodoc` 的 `check` 模块，命令
//! `ProDocCheck`）。宿主这边只留一个转发壳，理由与 `workspace` 工具相同：
//!
//! 1. 工具清单是宿主的注册表（`ToolRegistry`），模型看到的是那里的条目；
//! 2. 工具调用带 [`ToolContext`]（`team_id`），模块侧的检查是**按人格**取
//!    store/配置的，上下文得由宿主补进载荷。
//!
//! ## 为什么把报告再排版一遍
//!
//! 命令回执里是结构化的 `issues`。直接把 JSON 丢给模型能省一层代码，但模型
//! 读一屏 JSON 再去数"有几条错误"是在浪费 token，而且容易漏掉中间的条目。
//! 所以按级别分组、每行一条，最后给一句结论——模型要的是"有没有问题、在哪"。
//!
//! 结论措辞刻意区分两件事：**检查失败**（目录打不开 / 不在白名单）与
//! **检查出问题**（文档图有断链）。前者是工具错误，后者是正常结果——文档
//! 里有未完成的草稿是常态，不该让模型以为工具坏了。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_defs::tool::{Tool, ToolContext, ToolError, ToolResult};

use crate::modules::ModuleHost;

/// 暴露给模型的工具名。
pub const TOOL_NAME: &str = "prodoc_check";

/// 转发到 prodoc 模块 `ProDocCheck` 命令的宿主代理。
pub struct ProDocCheckTool {
    host: Arc<ModuleHost>,
}

impl ProDocCheckTool {
    pub fn new(host: Arc<ModuleHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl Tool for ProDocCheckTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "检查 ProDoc 文档群（一棵 Markdown 目录）的文档图：断链、连线自身、\
         重复 id、循环连线、孤立文档、空正文、超长文档、缺参数区、只声明单轴坐标。\
         `root` 必须是 Core 配置允许的文档根（模块 `roots`，缺省只有家目录）。\
         返回按级别分组的清单与一句结论。"
    }

    fn category(&self) -> &'static str {
        "module"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "root": {
                    "type": "string",
                    "description": "文档根绝对路径（必须在模块配置的 roots 内）"
                }
            },
            "required": ["root"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        Ok(self.execute_rich(arguments).await?.text)
    }

    async fn execute_rich(&self, arguments: Value) -> Result<ToolResult, ToolError> {
        self.execute_in(&ToolContext::default(), arguments).await
    }

    async fn execute_in(
        &self,
        context: &ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let mut payload = arguments;
        if let (Some(team_id), Some(object)) = (context.team_id.as_deref(), payload.as_object_mut())
        {
            object.insert("team_id".into(), Value::String(team_id.to_string()));
        }
        let outcome = self
            .host
            .dispatch_raw(agent_module_prodoc::commands::CHECK_COMMAND, payload)
            .await;
        match outcome {
            None => Err(ToolError::Execution(
                "prodoc 模块未启用（该智能体未挂载 ProDoc 文档群）".into(),
            )),
            Some(Err(message)) => Err(ToolError::Execution(message)),
            Some(Ok(receipt)) => Ok(ToolResult::text(render_report(&receipt))),
        }
    }
}

/// 把命令回执排版成给人/模型读的文本。
fn render_report(receipt: &Value) -> String {
    let documents = receipt["documents"].as_u64().unwrap_or(0);
    let relations = receipt["relations"].as_u64().unwrap_or(0);
    let errors = receipt["errors"].as_u64().unwrap_or(0);
    let warnings = receipt["warnings"].as_u64().unwrap_or(0);
    let root = receipt["root"].as_str().unwrap_or("");

    let mut out = format!(
        "文档群 {root}\n{documents} 篇 · {relations} 条连线 · {errors} 个错误 / {warnings} 条警告\n"
    );
    let empty: Vec<Value> = Vec::new();
    let issues = receipt["issues"].as_array().unwrap_or(&empty);
    if issues.is_empty() {
        out.push_str("\n没有发现问题。\n");
        return out;
    }
    for (level, title) in [
        ("error", "错误"),
        ("warning", "警告"),
        ("info", "提示"),
    ] {
        let group: Vec<&Value> = issues
            .iter()
            .filter(|issue| issue["level"].as_str() == Some(level))
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n── {title} ──\n"));
        for issue in group {
            let rule = issue["rule"].as_str().unwrap_or("");
            let message = issue["message"].as_str().unwrap_or("");
            let file = issue["file"].as_str().unwrap_or("");
            if file.is_empty() {
                out.push_str(&format!("- [{rule}] {message}\n"));
            } else {
                out.push_str(&format!("- [{rule}] {message} ({file})\n"));
            }
        }
    }
    if errors == 0 {
        out.push_str("\n结论：只有警告/提示，文档图结构可用。\n");
    } else {
        out.push_str(&format!(
            "\n结论：{errors} 个错误需要修（断链 / 重复 id / 连线自身）。\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(issues: Value) -> Value {
        json!({
            "root": "/srv/docs",
            "documents": 3,
            "relations": 2,
            "errors": 1,
            "warnings": 1,
            "ok": false,
            "issues": issues,
        })
    }

    /// 按级别分组、每行一条、给结论——模型扫一眼就知道要改什么。
    #[test]
    fn render_groups_by_level() {
        let text = render_report(&receipt(json!([
            {"level": "error", "rule": "broken-link", "message": "引用了不存在的文档 \"ghost\"", "file": "a.md"},
            {"level": "warning", "rule": "orphan", "message": "「B」没有任何连线", "file": "b.md"},
            {"level": "info", "rule": "no-frame", "message": "「C」没有参数区", "file": "c.md"},
        ])));
        assert!(text.contains("3 篇 · 2 条连线 · 1 个错误 / 1 条警告"));
        let error_at = text.find("── 错误 ──").expect("错误分组");
        let warn_at = text.find("── 警告 ──").expect("警告分组");
        let info_at = text.find("── 提示 ──").expect("提示分组");
        assert!(error_at < warn_at && warn_at < info_at, "分组顺序: {text}");
        assert!(text.contains("[broken-link] 引用了不存在的文档 \"ghost\" (a.md)"));
        assert!(text.contains("1 个错误需要修"));
    }

    /// 全绿：明确说"没有发现问题"，而不是给一个空列表让人猜。
    #[test]
    fn render_clean_says_so() {
        let mut value = receipt(json!([]));
        value["errors"] = json!(0);
        value["warnings"] = json!(0);
        let text = render_report(&value);
        assert!(text.contains("没有发现问题"), "{text}");
    }

    /// 只有警告：不能暗示"结构不可用"（草稿是常态）。
    #[test]
    fn render_warnings_only_says_usable() {
        let mut value = receipt(json!([
            {"level": "warning", "rule": "orphan", "message": "孤立", "file": "b.md"},
        ]));
        value["errors"] = json!(0);
        let text = render_report(&value);
        assert!(text.contains("文档图结构可用"), "{text}");
        assert!(!text.contains("需要修"), "{text}");
    }
}
