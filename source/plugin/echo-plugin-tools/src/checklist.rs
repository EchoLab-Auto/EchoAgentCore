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

/// 会话缺失时的兜底键（联邦远程调用 / 直接 registry 执行没有会话上下文；
/// 它们共享一份"全局"清单——本地会话均有注入的 `__session_id`）。
const FALLBACK_SESSION: &str = "__global";

#[derive(Debug, Default)]
pub struct ChecklistTool {
    /// **按会话隔离**（2026-10 修复）：session_id → 清单名 → 项。
    ///
    /// 此前是 `HashMap<清单名, 项>` 的进程内单实例状态——同一 persona 的
    /// 所有会话（local:tui / qq:group:…）共享一份清单，A 会话的条目会被
    /// B 会话看到并覆盖（工具 execute 不接收 session_id，无从隔离）。
    /// 现在会话键由 `Agent::run_tool` 在调度前注入 `__session_id`。
    lists: Mutex<HashMap<String, HashMap<String, Vec<ChecklistItem>>>>,
}

impl ChecklistTool {
    pub fn new() -> Self {
        Self {
            lists: Mutex::new(HashMap::new()),
        }
    }

    /// 本次调用的会话键（`Agent::run_tool` 注入；缺失 = 全局兜底键）。
    fn session_key(arguments: &Value) -> String {
        arguments
            .get("__session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(FALLBACK_SESSION)
            .to_string()
    }

    fn render_snapshot(lists: &HashMap<String, Vec<ChecklistItem>>) -> Value {
        let lists = lists
            .iter()
            .filter(|(_, items)| !items.is_empty())
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
        json!({ "lists": lists })
    }

    /// 全量调试快照（所有会话合并；`snapshot_for_session` 才是按会话视图）。
    pub fn state_snapshot(&self) -> Option<Value> {
        let all = self.lists.lock().ok()?;
        let merged: HashMap<String, Vec<ChecklistItem>> = all
            .values()
            .flat_map(|lists| lists.iter())
            .map(|(name, items)| (name.clone(), items.clone()))
            .collect();
        Some(Self::render_snapshot(&merged))
    }

    /// 某会话的清单快照（无该会话状态时 None——不发空事件）。
    pub fn snapshot_of(&self, session_id: &str) -> Option<Value> {
        let all = self.lists.lock().ok()?;
        let lists = all.get(session_id)?;
        Some(Self::render_snapshot(lists))
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

    fn snapshot_for_session(&self, session_id: &str) -> Option<Value> {
        self.snapshot_of(session_id)
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
        let session = Self::session_key(&arguments);

        let mut all = self
            .lists
            .lock()
            .map_err(|_| ToolError::Execution("checklist 状态锁损坏".into()))?;
        let lists = all.entry(session).or_default();

        match operation {
            "create" => {
                // 已存在不再重复报"已创建"（保留原条目；2026-10）。
                if lists.contains_key(&name) {
                    return Ok(format!(
                        "清单 '{name}' 已存在（保留原有条目；reset 可清空）"
                    ));
                }
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
                // 不造幽灵空清单（2026-10）：只读操作不得写状态——此前
                // `list` 对不存在的名字 `or_default` 会凭空创建空清单。
                match lists.get(&name) {
                    None => return Ok(format!("清单 '{name}' 不存在（先用 add/create 创建）")),
                    Some(list) if list.is_empty() => {
                        return Ok(format!("清单 '{name}' 为空"));
                    }
                    Some(list) => Ok(render(&name, list)),
                }
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
        // 只读 list 不再造幽灵空清单（2026-10）：不存在即报不存在——
        // 依然证明 default 与命名清单隔离（没有混入「构建」）。
        assert!(out.contains("不存在"), "default 应与命名清单隔离: {out}");
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
        assert!(out.contains("不存在"), "got: {out}");
    }

    /// 会话隔离（2026-10 P0 修复）：不同 `__session_id` 的清单互不可见、
    /// 互不覆盖；快照也按会话过滤。
    #[tokio::test]
    async fn sessions_are_isolated() {
        let t = tool();
        // A 会话：default 清单加两项
        t.execute(
            json!({"operation": "add", "item": "A-1", "__session_id": "local:tui::local_user"}),
        )
        .await
        .unwrap();
        t.execute(
            json!({"operation": "add", "item": "A-2", "__session_id": "local:tui::local_user"}),
        )
        .await
        .unwrap();
        // B 会话：同名 default 清单加一项
        t.execute(json!({"operation": "add", "item": "B-1", "__session_id": "qq:group:1:2"}))
            .await
            .unwrap();
        // 各看各的：互不可见
        let a = t
            .execute(json!({"operation": "list", "__session_id": "local:tui::local_user"}))
            .await
            .unwrap();
        assert!(a.contains("A-1") && a.contains("A-2"), "got: {a}");
        assert!(!a.contains("B-1"), "B 会话条目泄漏到 A: {a}");
        let b = t
            .execute(json!({"operation": "list", "__session_id": "qq:group:1:2"}))
            .await
            .unwrap();
        assert!(b.contains("B-1"), "got: {b}");
        assert!(!b.contains("A-1"), "A 会话条目泄漏到 B: {b}");
        // 快照按会话过滤（事件侧不再挂全量状态）
        let sa = t.snapshot_for_session("local:tui::local_user").unwrap();
        let sa_text = sa.to_string();
        assert!(
            sa_text.contains("A-1") && !sa_text.contains("B-1"),
            "snapshot: {sa_text}"
        );
        let sb = t.snapshot_for_session("qq:group:1:2").unwrap();
        let sb_text = sb.to_string();
        assert!(
            sb_text.contains("B-1") && !sb_text.contains("A-1"),
            "snapshot: {sb_text}"
        );
        // 未知会话：None（不发空事件）
        assert!(t.snapshot_for_session("unknown:session").is_none());
    }

    /// 缺失 `__session_id`（联邦远程调用/直接 registry 执行）→ 全局兜底
    /// 键，不与本地会话互相污染。
    #[tokio::test]
    async fn missing_session_falls_back_to_global_key() {
        let t = tool();
        t.execute(json!({"operation": "add", "item": "G-1"}))
            .await
            .unwrap();
        let out = t
            .execute(json!({"operation": "list", "__session_id": "local:tui::local_user"}))
            .await
            .unwrap();
        assert!(
            out.contains("不存在"),
            "本地会话不应看到全局兜底清单: {out}"
        );
        let g = t.execute(json!({"operation": "list"})).await.unwrap();
        assert!(g.contains("G-1"), "got: {g}");
    }
}
