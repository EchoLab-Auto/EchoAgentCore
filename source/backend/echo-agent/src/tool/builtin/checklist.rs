//! Checklist tool: maintain multi-step task checklists for the agent.
//!
//! State lives in-process (per agent run) and is keyed by checklist name,
//! with a `default` checklist for single-list usage. Supports create / add /
//! list / check / uncheck / remove / reset operations.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError};

#[derive(Debug, Clone)]
struct ChecklistItem {
    text: String,
    done: bool,
}

#[derive(Debug, Default)]
pub struct ChecklistTool {
    lists: Mutex<HashMap<String, Vec<ChecklistItem>>>,
}

impl ChecklistTool {
    pub fn new() -> Self {
        Self {
            lists: Mutex::new(HashMap::new()),
        }
    }

    /// Structured state snapshot for the TUI checklist panel:
    /// `{"lists": [{"name", "done", "total", "items": [{"text", "done"}]}]}`.
    pub fn state_snapshot(&self) -> Option<Value> {
        let lists = self.lists.lock().ok()?;
        let lists = lists
            .iter()
            .map(|(name, items)| {
                let done = items.iter().filter(|item| item.done).count();
                json!({
                    "name": name,
                    "done": done,
                    "total": items.len(),
                    "items": items.iter().map(|item| json!({
                        "text": item.text,
                        "done": item.done,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        Some(json!({ "lists": lists }))
    }
}

#[async_trait]
impl Tool for ChecklistTool {
    fn name(&self) -> &str {
        "checklist"
    }

    fn description(&self) -> &str {
        "管理任务检查清单。操作: create 创建清单, add 添加项(需要 item), list 查看, check 勾选完成(需要 index), uncheck 取消勾选(需要 index), remove 删除项(需要 index), reset 清空。name 为清单名称(默认 default)，index 从 1 开始。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["create", "add", "list", "check", "uncheck", "remove", "reset"],
                    "description": "要执行的操作"
                },
                "name": {
                    "type": "string",
                    "description": "清单名称，默认 default"
                },
                "item": {
                    "type": "string",
                    "description": "要添加的清单项文本（operation=add 时必填）"
                },
                "index": {
                    "type": "integer",
                    "description": "清单项序号，从 1 开始（check/uncheck/remove 时使用）"
                }
            },
            "required": ["operation"]
        })
    }

    fn snapshot(&self) -> Option<Value> {
        self.state_snapshot()
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let operation = arguments
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 operation".into()))?;
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let item = arguments.get("item").and_then(|v| v.as_str());
        let index = arguments.get("index").and_then(|v| v.as_u64());

        let mut lists = self
            .lists
            .lock()
            .map_err(|_| ToolError::Execution("checklist 状态锁损坏".into()))?;

        match operation {
            "create" => {
                lists.entry(name.clone()).or_default();
                Ok(format!("清单 '{name}' 已创建"))
            }
            "add" => {
                let text = item
                    .ok_or_else(|| ToolError::InvalidArguments("add 操作需要 item 参数".into()))?;
                if text.trim().is_empty() {
                    return Err(ToolError::InvalidArguments("item 不能为空".into()));
                }
                let list = lists.entry(name.clone()).or_default();
                list.push(ChecklistItem {
                    text: text.to_string(),
                    done: false,
                });
                Ok(render(&name, list))
            }
            "list" => {
                let list = lists.entry(name.clone()).or_default();
                if list.is_empty() {
                    return Ok(format!("清单 '{name}' 为空"));
                }
                Ok(render(&name, list))
            }
            "check" | "uncheck" => {
                let idx = index.ok_or_else(|| {
                    ToolError::InvalidArguments(format!("{operation} 操作需要 index 参数"))
                })?;
                let list = lists.entry(name.clone()).or_default();
                let pos = usize::try_from(idx)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .ok_or_else(|| ToolError::InvalidArguments("index 必须为正整数".into()))?;
                let len = list.len();
                let entry = list.get_mut(pos).ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "清单 '{name}' 没有第 {idx} 项（共 {len} 项）"
                    ))
                })?;
                entry.done = operation == "check";
                Ok(render(&name, list))
            }
            "remove" => {
                let idx = index.ok_or_else(|| {
                    ToolError::InvalidArguments("remove 操作需要 index 参数".into())
                })?;
                let list = lists.entry(name.clone()).or_default();
                let pos = usize::try_from(idx)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .ok_or_else(|| ToolError::InvalidArguments("index 必须为正整数".into()))?;
                if pos >= list.len() {
                    return Err(ToolError::InvalidArguments(format!(
                        "清单 '{name}' 没有第 {idx} 项（共 {} 项）",
                        list.len()
                    )));
                }
                let removed = list.remove(pos);
                Ok(format!(
                    "已删除 [{idx}] {}\n{}",
                    removed.text,
                    render(&name, list)
                ))
            }
            "reset" => {
                lists.remove(&name);
                Ok(format!("清单 '{name}' 已清空"))
            }
            other => Err(ToolError::InvalidArguments(format!(
                "未知操作: {other}（支持 create/add/list/check/uncheck/remove/reset）"
            ))),
        }
    }
}

fn render(name: &str, list: &[ChecklistItem]) -> String {
    let done = list.iter().filter(|i| i.done).count();
    let mut lines = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let mark = if item.done { "[x]" } else { "[ ]" };
        lines.push(format!("  {mark} {}. {}", i + 1, item.text));
    }
    format!(
        "清单 '{name}'（{done}/{} 完成）:\n{}",
        list.len(),
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> ChecklistTool {
        ChecklistTool::new()
    }

    #[tokio::test]
    async fn add_and_list() {
        let t = tool();
        let out = t
            .execute(json!({"operation": "add", "item": "写代码"}))
            .await
            .unwrap();
        assert!(out.contains("[ ] 1. 写代码"), "got: {out}");
        let out = t
            .execute(json!({"operation": "add", "item": "测试"}))
            .await
            .unwrap();
        assert!(out.contains("[ ] 2. 测试"), "got: {out}");
    }

    #[tokio::test]
    async fn check_uncheck_remove() {
        let t = tool();
        t.execute(json!({"operation": "add", "item": "a"}))
            .await
            .unwrap();
        t.execute(json!({"operation": "add", "item": "b"}))
            .await
            .unwrap();
        let out = t
            .execute(json!({"operation": "check", "index": 1}))
            .await
            .unwrap();
        assert!(out.contains("[x] 1. a"), "got: {out}");
        assert!(out.contains("1/2 完成"), "got: {out}");
        let out = t
            .execute(json!({"operation": "uncheck", "index": 1}))
            .await
            .unwrap();
        assert!(out.contains("[ ] 1. a"), "got: {out}");
        let out = t
            .execute(json!({"operation": "remove", "index": 1}))
            .await
            .unwrap();
        assert!(out.contains("已删除"), "got: {out}");
        assert!(!out.contains("1. a"), "got: {out}");
    }

    #[tokio::test]
    async fn named_lists_are_independent() {
        let t = tool();
        t.execute(json!({"operation": "add", "name": "部署", "item": "构建"}))
            .await
            .unwrap();
        let out = t
            .execute(json!({"operation": "list", "name": "default"}))
            .await
            .unwrap();
        assert!(out.contains("为空"), "default 应与命名清单隔离: {out}");
        let out = t
            .execute(json!({"operation": "list", "name": "部署"}))
            .await
            .unwrap();
        assert!(out.contains("1. 构建"), "got: {out}");
    }

    #[tokio::test]
    async fn errors() {
        let t = tool();
        assert!(t.execute(json!({})).await.is_err(), "缺少 operation");
        assert!(
            t.execute(json!({"operation": "add"})).await.is_err(),
            "add 缺少 item"
        );
        assert!(
            t.execute(json!({"operation": "check", "index": 5}))
                .await
                .is_err(),
            "越界 index"
        );
        assert!(
            t.execute(json!({"operation": "check", "index": 0}))
                .await
                .is_err(),
            "index 从 1 开始"
        );
        assert!(
            t.execute(json!({"operation": "bogus"})).await.is_err(),
            "未知操作"
        );
    }

    #[tokio::test]
    async fn reset_clears() {
        let t = tool();
        t.execute(json!({"operation": "add", "item": "a"}))
            .await
            .unwrap();
        let out = t.execute(json!({"operation": "reset"})).await.unwrap();
        assert!(out.contains("已清空"), "got: {out}");
        let out = t.execute(json!({"operation": "list"})).await.unwrap();
        assert!(out.contains("为空"), "got: {out}");
    }
}
