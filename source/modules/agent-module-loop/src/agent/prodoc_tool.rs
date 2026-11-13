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
            Some(Ok(receipt)) => Ok(ToolResult::text(render_report(&receipt)?)),
        }
    }
}

/// 把命令回执排版成给人/模型读的文本。
///
/// ## 为什么参数是模块导出的类型，而不是 `Value`
///
/// 此前这里用 `receipt["documents"].as_u64().unwrap_or(0)` 逐字段取——**模块
/// 改一个字段名，工具就会安静地报告"0 个错误 / 0 条警告"**，而实际上有一堆
/// 问题。这种降级没有任何痕迹：模型看到的是"文档群很健康"，用户也找不到
/// 报警的入口。
///
/// 现在反序列化成 `agent_module_prodoc::check::CheckReceipt`——模块与宿主共用
/// 一个类型，改名会两侧一起编译失败。反序列化失败也给明确错误（而不是退化成
/// 一份"零问题"的报告）。
fn render_report(receipt: &Value) -> Result<String, ToolError> {
    let receipt: agent_module_prodoc::check::CheckReceipt =
        serde_json::from_value(receipt.clone()).map_err(|error| {
            // 这条错误信息是排查入口：它说明"模块与宿主的回执契约不一致"，
            // 而不是"检查没发现问题"。
            ToolError::Execution(format!(
                "prodoc 检查回执无法解析（模块与宿主的回执契约不一致）: {error}"
            ))
        })?;
    let agent_module_prodoc::check::CheckReceipt {
        root,
        documents,
        relations,
        errors,
        warnings,
        issues,
        ..
    } = receipt;

    let mut out = format!(
        "文档群 {root}\n{documents} 篇 · {relations} 条连线 · {errors} 个错误 / {warnings} 条警告\n"
    );
    if issues.is_empty() {
        out.push_str("\n没有发现问题。\n");
        return Ok(out);
    }
    for (level, title) in [
        ("error", "错误"),
        ("warning", "警告"),
        ("info", "提示"),
    ] {
        let group: Vec<&agent_module_prodoc::check::ReceiptIssue> =
            issues.iter().filter(|issue| issue.level == level).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n── {title} ──\n"));
        for issue in group {
            let rule = &issue.rule;
            let message = &issue.message;
            match issue.file.as_deref() {
                Some(file) if !file.is_empty() => {
                    out.push_str(&format!("- [{rule}] {message} ({file})\n"))
                }
                _ => out.push_str(&format!("- [{rule}] {message}\n")),
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
    Ok(out)
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
        ])))
        .expect("回执可解析");
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
        let text = render_report(&value).expect("回执可解析");
        assert!(text.contains("没有发现问题"), "{text}");
    }

    /// 只有警告：不能暗示"结构不可用"（草稿是常态）。
    #[test]
    fn render_warnings_only_says_usable() {
        let mut value = receipt(json!([
            {"level": "warning", "rule": "orphan", "message": "孤立", "file": "b.md"},
        ]));
        value["errors"] = json!(0);
        let text = render_report(&value).expect("回执可解析");
        assert!(text.contains("文档图结构可用"), "{text}");
        assert!(!text.contains("需要修"), "{text}");
    }
}

#[cfg(test)]
mod receipt_contract_tests {
    use super::*;

    /// 回执形状与模块的类型**必须**对得上——这条测试是那个静默降级的兜底。
    ///
    /// 此前宿主逐字段 `as_u64().unwrap_or(0)`：模块把 `errors` 改名成
    /// `error_count`，工具会安静地报告"0 个错误"。反序列化路径让这种情况变成
    /// 一条明确错误，本测试再把它提前到编译/测试阶段。
    #[test]
    fn receipt_shape_round_trips_through_the_module_type() {
        let receipt = json!({
            "root": "/srv/docs",
            "documents": 3,
            "relations": 2,
            "errors": 1,
            "warnings": 1,
            "ok": false,
            "issues": [
                {"level": "error", "rule": "broken-link", "message": "引用了不存在", "file": "a.md"},
                {"level": "info", "rule": "no-frame", "message": "没有参数区"},
            ],
        });
        let text = render_report(&receipt).expect("模块导出的类型必须能解析这份回执");
        assert!(text.contains("3 篇 · 2 条连线 · 1 个错误 / 1 条警告"));
        // `file` 缺失的问题不该渲染出空括号
        assert!(text.contains("- [no-frame] 没有参数区\n"), "{text}");
    }

    /// 契约不一致时给**明确错误**，而不是退化成"没有问题"。
    ///
    /// 这条比上一条更重要：上一条测"正常路径对得上"，这条测"对不上时会不会
    /// 假装一切正常"——那正是修复前的行为。
    #[test]
    fn mismatched_receipt_reports_the_contract_break() {
        let broken = json!({ "documents": 3 }); // 缺 root / errors / issues…
        let error = render_report(&broken).expect_err("缺字段必须报错");
        let message = format!("{error:?}");
        assert!(
            message.contains("回执契约不一致"),
            "错误信息要指向契约而不是数据: {message}"
        );
        assert!(
            !message.contains("没有发现问题"),
            "绝不能把契约错误渲染成一份健康的报告"
        );
    }
}
