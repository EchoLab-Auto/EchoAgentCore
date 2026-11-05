//! Agent-facing, read-only session context and execution-audit tools.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_defs::session_query::{SessionQuery, SessionQueryView};
use echo_defs::tool::ToolContext;
use echo_tool::{Tool, ToolError, ToolResult};

const MAX_LIMIT: usize = 200;

fn limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(50)
        .clamp(1, MAX_LIMIT)
}

pub struct ListSessionsTool {
    query: Arc<dyn SessionQuery>,
}

impl ListSessionsTool {
    pub fn new(query: Arc<dyn SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for ListSessionsTool {
    fn name(&self) -> &str {
        "list_sessions"
    }
    fn description(&self) -> &str {
        "列出当前人格的历史会话及最近消息。只读；结果按最近活跃排序，支持 limit（默认50，最大200）。"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT}}})
    }
    async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
        Err(ToolError::Execution("list_sessions 需要会话上下文".into()))
    }
    async fn execute_in(
        &self,
        cx: &ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let value = self
            .query
            .list_sessions(cx.team_id.as_deref(), limit(&arguments))
            .map_err(ToolError::Execution)?;
        Ok(ToolResult::text(value.to_string()))
    }
}

pub struct ReadSessionTool {
    query: Arc<dyn SessionQuery>,
}

impl ReadSessionTool {
    pub fn new(query: Arc<dyn SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for ReadSessionTool {
    fn name(&self) -> &str {
        "read_session"
    }
    fn description(&self) -> &str {
        "只读查看历史会话。view=model 返回模型实际可见的压缩上下文；view=audit 返回消息、工具调用/耗时及模型步骤/token/耗时。默认当前会话；limit 默认50、最大200；工具结果正文默认不返回，需 include_tool_results=true。"
    }
    fn parameters(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "session_id":{"type":"string","description":"目标会话；缺省为当前会话"},
                "view":{"type":"string","enum":["model","audit"],"default":"audit"},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT},
                "include_tool_results":{"type":"boolean","default":false}
            }
        })
    }
    async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
        Err(ToolError::Execution("read_session 需要会话上下文".into()))
    }
    async fn execute_in(
        &self,
        cx: &ToolContext,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let session_id = arguments
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(&cx.session_id);
        if session_id.is_empty() {
            return Err(ToolError::InvalidArguments(
                "缺少 session_id，且调用上下文没有当前会话".into(),
            ));
        }
        let view = match arguments
            .get("view")
            .and_then(Value::as_str)
            .unwrap_or("audit")
        {
            "model" => SessionQueryView::Model,
            "audit" => SessionQueryView::Audit,
            other => {
                return Err(ToolError::InvalidArguments(format!(
                    "未知 view: {other}；仅支持 model / audit"
                )))
            }
        };
        let include_tool_results = arguments
            .get("include_tool_results")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let value = self
            .query
            .read_session(
                cx.team_id.as_deref(),
                session_id,
                view,
                limit(&arguments),
                include_tool_results,
            )
            .map_err(ToolError::Execution)?;
        Ok(ToolResult::text(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeQuery {
        last_team: Mutex<Option<String>>,
        last_limit: Mutex<usize>,
    }
    impl SessionQuery for FakeQuery {
        fn list_sessions(&self, team_id: Option<&str>, limit: usize) -> Result<Value, String> {
            *self.last_team.lock().unwrap() = team_id.map(str::to_string);
            *self.last_limit.lock().unwrap() = limit;
            Ok(json!({"sessions":[]}))
        }
        fn read_session(
            &self,
            team_id: Option<&str>,
            session_id: &str,
            view: SessionQueryView,
            limit: usize,
            _include: bool,
        ) -> Result<Value, String> {
            *self.last_team.lock().unwrap() = team_id.map(str::to_string);
            *self.last_limit.lock().unwrap() = limit;
            Ok(json!({"session_id":session_id,"view":format!("{view:?}")}))
        }
    }

    #[tokio::test]
    async fn tools_use_calling_persona_and_current_session_by_default() {
        let query = Arc::new(FakeQuery::default());
        let tool = ReadSessionTool::new(query.clone());
        let cx = ToolContext {
            session_id: "s1".into(),
            team_id: Some("alice".into()),
            ..Default::default()
        };
        let result = tool.execute_in(&cx, json!({"limit":500})).await.unwrap();
        assert!(result.text.contains("s1"));
        assert_eq!(query.last_team.lock().unwrap().as_deref(), Some("alice"));
        assert_eq!(*query.last_limit.lock().unwrap(), MAX_LIMIT);
    }

    #[tokio::test]
    async fn list_is_read_only_and_scoped_to_the_calling_persona() {
        let query = Arc::new(FakeQuery::default());
        let tool = ListSessionsTool::new(query.clone());
        let cx = ToolContext {
            team_id: Some("bob".into()),
            ..Default::default()
        };
        tool.execute_in(&cx, json!({})).await.unwrap();
        assert_eq!(query.last_team.lock().unwrap().as_deref(), Some("bob"));
    }
}
