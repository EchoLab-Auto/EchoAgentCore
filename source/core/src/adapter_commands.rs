//! 适配器命令域（P1.9）：`/echo` `/help` `/status` `/shutdown`。
//!
//! 平台中立：命令处理只操作 [`IncomingMessage`] 与 [`Arc<dyn Adapter>`]，
//! 回复经 `adapter.send_message`（内置实例与插件实例同一路径）。
//!
//! **owner 判定**：只认 `metadata.sender_role == "owner"`（插件侧标注，
//! 见 `document/design/adapter-plugin.md` §插件侧；信任边界 = 插件为用户自装的
//! 可信代码——宿主不重复读平台配置）。
//!
//! 接线：插件实例的入站 worker 在投递 agent 之前调用 [`try_handle`]；
//! 返回 `true` = 命令已消费（不再进 agent）。builtin 实例仍走
//! `echo_server` handler（`core/src/handlers/`，P1.9 收尾时统一到本模块）。

use std::sync::Arc;
use std::time::Instant;

use echo_adapter::traits::Adapter;
use echo_adapter::types::{IncomingMessage, MessageTarget};

/// 从文本开头解析 `prefix + name` 命令，返回参数（已去前导空白）。
///
/// 命令后必须是空白或结尾（`/echo` 不匹配 `/echofoo`）；空前缀（自动
/// 回退）时命令本身仍须词边界。语义移植自原 `handlers::parse_command`
/// （P1.9 命令域迁移，builtin handler 已移除）。
fn parse_command(text: &str, prefix: &str, name: &str) -> Option<String> {
    let expected = format!("{prefix}{name}");
    let rest = text.strip_prefix(&expected)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim_start().to_string())
}

/// 命令上下文（进程级共享；按实例复用）。
pub struct CommandContext {
    /// 命令前缀（`[adapters.qq] command_prefix`）。
    pub prefix: String,
    /// 进程启动时刻（`/status` 的运行时长基准）。
    pub started_at: Instant,
    /// 优雅停机信号（`/shutdown` 触发；与启动期 `shutdown_rx` 同一通道）。
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl CommandContext {
    /// 构造（`prefix` 空串回退 `/`，与配置加载同口径）。
    pub fn new(prefix: impl Into<String>, shutdown_tx: tokio::sync::watch::Sender<bool>) -> Self {
        let prefix = prefix.into();
        Self {
            prefix: if prefix.is_empty() {
                "/".to_string()
            } else {
                prefix
            },
            started_at: Instant::now(),
            shutdown_tx,
        }
    }
}

/// 处理一条入站消息里的命令；`true` = 已消费（worker 跳过 agent 投递）。
///
/// 命令匹配（`prefix + name` 后须空白或结尾，复用 `handlers::parse_command`
/// 的既有语义）：
/// - `echo <文本>`：复读；空文本回用法。
/// - `help`：命令列表。
/// - `status` / `shutdown`（仅 owner）：运行状态 / 优雅停机。
pub async fn try_handle(
    msg: &IncomingMessage,
    adapter: &Arc<dyn Adapter>,
    ctx: &CommandContext,
) -> bool {
    let text = msg.content.as_str();
    if text.is_empty() {
        return false;
    }

    if let Some(rest) = parse_command(text, &ctx.prefix, "echo") {
        let output = if rest.is_empty() {
            format!("用法: {}echo <文本>", ctx.prefix)
        } else {
            rest.to_string()
        };
        reply(adapter, msg, &output).await;
        return true;
    }
    if parse_command(text, &ctx.prefix, "help").is_some() {
        let help = format!(
            "EchoAgentPanel 命令列表:\n{}echo <文本> — 复读文本\n{}help — 显示本帮助\n{}status — 运行状态 (仅管理员)\n{}shutdown — 关闭机器人 (仅管理员)",
            ctx.prefix, ctx.prefix, ctx.prefix, ctx.prefix
        );
        reply(adapter, msg, &help).await;
        return true;
    }

    let is_status = parse_command(text, &ctx.prefix, "status").is_some();
    let is_shutdown = parse_command(text, &ctx.prefix, "shutdown").is_some();
    if !is_status && !is_shutdown {
        return false;
    }
    if !is_owner(msg) {
        reply(adapter, msg, "该命令仅限管理员使用").await;
        return true;
    }
    if is_shutdown {
        reply(adapter, msg, "正在关闭…").await;
        let _ = ctx.shutdown_tx.send(true);
        return true;
    }

    // status：进程态 ∧ 连接态（`Adapter::status_info` 的仲裁口径与
    // 管理面一致——插件 Pending/Failed 时不显示缓存连接态）。
    let info = adapter.status_info();
    let state = match info.status {
        echo_adapter::AdapterConnectionState::Connected => "connected",
        echo_adapter::AdapterConnectionState::Disconnected => "running (no client)",
        echo_adapter::AdapterConnectionState::Stopped => "stopped",
        echo_adapter::AdapterConnectionState::Starting => "starting",
        echo_adapter::AdapterConnectionState::Stopping => "stopping",
        echo_adapter::AdapterConnectionState::Error => "error",
    };
    let status = format!(
        "运行状态\n- 适配器: {} ({})\n- 机器人 QQ: {}\n- Core 运行时长: {}s",
        info.display_name,
        state,
        info.self_id.as_deref().unwrap_or("-"),
        ctx.started_at.elapsed().as_secs()
    );
    reply(adapter, msg, &status).await;
    true
}

/// owner 判定：只认插件标注的 `metadata.sender_role == "owner"`。
fn is_owner(msg: &IncomingMessage) -> bool {
    msg.metadata
        .get("sender_role")
        .and_then(|value| value.as_str())
        .is_some_and(|role| role == "owner")
}

/// 回复一条消息（best-effort：失败只记日志——命令消费语义不因发送失败回退）。
async fn reply(adapter: &Arc<dyn Adapter>, msg: &IncomingMessage, text: &str) {
    let target = MessageTarget {
        adapter_name: msg.adapter_name.clone(),
        channel: msg.channel.clone(),
        user_id: msg.user_id.clone(),
    };
    match adapter.send_message(&target, text).await {
        Ok(result) if result.success => {}
        Ok(result) => {
            tracing::warn!(
                adapter = %msg.adapter_name,
                error = result.error.as_deref().unwrap_or("unknown"),
                "command reply failed"
            );
        }
        Err(error) => {
            tracing::warn!(adapter = %msg.adapter_name, %error, "command reply failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use async_trait::async_trait;
    use echo_adapter::types::{AdapterEvent, ChannelType, SendResult};
    use echo_adapter::{AdapterConnectionState, AdapterError, AdapterInfo, AdapterManagement};

    /// 记录型适配器：记录 send_message 调用；可选 owner/连接态。
    struct RecordingAdapter {
        name: String,
        connected: bool,
        sent: Mutex<Vec<(MessageTarget, String)>>,
    }

    impl RecordingAdapter {
        fn new(name: &str, connected: bool) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                connected,
                sent: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Adapter for RecordingAdapter {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "mock"
        }
        fn is_configured(&self) -> bool {
            true
        }
        fn status_info(&self) -> AdapterInfo {
            AdapterInfo {
                name: self.name.clone(),
                display_name: self.name.clone(),
                status: if self.connected {
                    AdapterConnectionState::Connected
                } else {
                    AdapterConnectionState::Disconnected
                },
                self_id: Some("4242".into()),
                started_at: None,
                platform: "mock".into(),
                configured: true,
            }
        }
        async fn start(&self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn send_message(
            &self,
            target: &MessageTarget,
            content: &str,
        ) -> Result<SendResult, AdapterError> {
            self.sent
                .lock()
                .unwrap()
                .push((target.clone(), content.to_string()));
            Ok(SendResult {
                message_id: Some("m1".into()),
                success: true,
                error: None,
            })
        }
        fn subscribe(&self, _tx: tokio::sync::mpsc::UnboundedSender<AdapterEvent>) {}
        fn management(&self) -> Option<&dyn AdapterManagement> {
            None
        }
    }

    fn incoming(content: &str, role: Option<&str>) -> IncomingMessage {
        let metadata = match role {
            Some(role) => serde_json::json!({"sender_role": role}),
            None => serde_json::Value::Null,
        };
        IncomingMessage {
            adapter_name: "qq".into(),
            platform: "qq".into(),
            user_id: "123".into(),
            user_name: "Alice".into(),
            channel: ChannelType::Group {
                group_id: "456".into(),
            },
            group_name: None,
            content: content.to_string(),
            timestamp: 0,
            at_me: true,
            metadata,
            images: vec![],
            files: vec![],
        }
    }

    /// `Arc<RecordingAdapter>` → `Arc<dyn Adapter>`（`try_handle` 的入参形态）。
    fn dyn_adapter(adapter: &Arc<RecordingAdapter>) -> Arc<dyn Adapter> {
        adapter.clone()
    }

    fn context() -> (CommandContext, tokio::sync::watch::Receiver<bool>) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (CommandContext::new("/", tx), rx)
    }

    #[tokio::test]
    async fn non_command_passes_through() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, _rx) = context();
        assert!(!try_handle(&incoming("hello world", None), &dyn_adapter(&adapter), &ctx).await);
        assert!(adapter.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn echo_replies_and_consumes() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, _rx) = context();
        assert!(try_handle(&incoming("/echo hi", None), &dyn_adapter(&adapter), &ctx).await);
        {
            let sent = adapter.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].1, "hi");
            // 群聊回复带发送者 user_id（@ 语义）。
            assert_eq!(sent[0].0.user_id, "123");
        }
        // 空文本 → 用法提示。
        assert!(try_handle(&incoming("/echo", None), &dyn_adapter(&adapter), &ctx).await);
        let sent = adapter.sent.lock().unwrap();
        assert!(sent[1].1.contains("用法"));
    }

    #[tokio::test]
    async fn help_lists_commands() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, _rx) = context();
        assert!(try_handle(&incoming("/help", None), &dyn_adapter(&adapter), &ctx).await);
        let sent = adapter.sent.lock().unwrap();
        assert!(sent[0].1.contains("echo"));
        assert!(sent[0].1.contains("status"));
    }

    #[tokio::test]
    async fn admin_commands_require_owner_annotation() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, mut rx) = context();

        // 非 owner：拒绝，不触发停机。
        assert!(try_handle(&incoming("/shutdown", None), &dyn_adapter(&adapter), &ctx).await);
        assert!(!*rx.borrow_and_update());

        // owner：触发停机。
        assert!(
            try_handle(
                &incoming("/shutdown", Some("owner")),
                &dyn_adapter(&adapter),
                &ctx
            )
            .await
        );
        assert!(*rx.borrow_and_update());
    }

    #[tokio::test]
    async fn status_reports_adapter_state() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, _rx) = context();
        assert!(
            try_handle(
                &incoming("/status", Some("owner")),
                &dyn_adapter(&adapter),
                &ctx
            )
            .await
        );
        let sent = adapter.sent.lock().unwrap();
        assert!(sent[0].1.contains("connected"), "{}", sent[0].1);
        assert!(sent[0].1.contains("4242"), "{}", sent[0].1);
    }

    /// 命令前缀后必须空白/结尾（`/echoX` 不是命令）。
    #[tokio::test]
    async fn token_bounded_command_match() {
        let adapter = RecordingAdapter::new("qq", true);
        let (ctx, _rx) = context();
        assert!(!try_handle(&incoming("/echofoo", None), &dyn_adapter(&adapter), &ctx).await);
    }
}
