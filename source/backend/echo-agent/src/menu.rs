//! Human-in-the-loop 选单（menu plugin）。
//!
//! `present_menu`（编排工具）向用户发起一个选单：向 [`MenuBroker`] 注册一个
//! 未决请求，发出 `BackendEvent::MenuRequest`，然后等待用户在 Panel 中选择。
//! 用户的应答经**专用通道**（`WsMessage::MenuAnswer` →
//! [`MenuBroker::submit`]）回传，绕过 agent 命令队列与会话日志。
//!
//! 与 sudo 密码通道的区别：选单内容与选择结果都不是秘密（会进入会话日志与
//! LLM 上下文——这正是它存在的意义）。不与命令队列混用的原因是**语义**：
//! 命令队列是"用户输入"（会开一个新 turn），而选单应答只是对既有 turn 的
//! 应答，必须精确落到等待它的那次工具调用上。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 等待用户选择的最长时间（秒）。
///
/// 超时后 `present_menu` 返回失败并把控制权交回模型，Panel 侧由
/// `MenuResolved` 事件关闭弹层——不会挂在死请求上。
pub const MENU_WAIT_TIMEOUT_SECS: u64 = 300;

/// 一个未决的选单请求。
///
/// 未被应答就 drop（例如外层工具守卫超时中止了 `present_menu` future）时会
/// 取消 broker 条目，避免条目泄漏；此后该请求不再可能被应答。
pub struct PendingMenu {
    pub request_id: u64,
    rx: Option<tokio::sync::oneshot::Receiver<Option<String>>>,
    broker: Arc<MenuBroker>,
}

impl PendingMenu {
    /// 取出应答接收端（只能取一次）。
    pub fn into_receiver(mut self) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.rx.take().expect("menu receiver taken exactly once")
    }
}

impl Drop for PendingMenu {
    fn drop(&mut self) {
        if self.rx.is_some() {
            self.broker.cancel(self.request_id);
        }
    }
}

/// 未决选单请求的注册表。
///
/// `request` 由 agent 的 `present_menu` 工具调用；`submit` 由管理面服务器在
/// 收到 `menu_answer` 帧时直接调用（不进命令队列）。
#[derive(Default)]
pub struct MenuBroker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Option<String>>>>,
}

impl MenuBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个未决选单并返回其接收端。
    pub fn request(self: &Arc<Self>) -> PendingMenu {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .expect("menu broker poisoned")
            .insert(request_id, tx);
        PendingMenu {
            request_id,
            rx: Some(rx),
            broker: Arc::clone(self),
        }
    }

    /// 应答一个未决选单：`Some(option_id)` = 选定该项，`None` = 用户取消。
    /// 返回 `false` 表示该请求已超时或被取消（重复应答同样返回 `false`）。
    pub fn submit(&self, request_id: u64, option_id: Option<String>) -> bool {
        let sender = self
            .pending
            .lock()
            .expect("menu broker poisoned")
            .remove(&request_id);
        match sender {
            Some(tx) => tx.send(option_id).is_ok(),
            None => false,
        }
    }

    /// 放弃一个未决请求（工具超时/中断），不作应答。
    pub fn cancel(&self, request_id: u64) {
        self.pending
            .lock()
            .expect("menu broker poisoned")
            .remove(&request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker() -> Arc<MenuBroker> {
        Arc::new(MenuBroker::new())
    }

    #[tokio::test]
    async fn submit_resolves_pending_request() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        assert!(broker.submit(id, Some("option-b".into())));
        let answer = pending.into_receiver().await.expect("request resolved");
        assert_eq!(answer.as_deref(), Some("option-b"));
    }

    #[tokio::test]
    async fn cancel_answer_resolves_with_none() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        assert!(broker.submit(id, None));
        assert_eq!(
            pending.into_receiver().await.expect("request resolved"),
            None
        );
    }

    #[test]
    fn submit_unknown_id_is_false() {
        let broker = broker();
        assert!(!broker.submit(12345, Some("x".into())));
    }

    #[tokio::test]
    async fn second_submit_is_rejected() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        assert!(broker.submit(id, Some("a".into())));
        assert!(
            !broker.submit(id, Some("b".into())),
            "a request resolves exactly once"
        );
        assert_eq!(
            pending.into_receiver().await.expect("resolved"),
            Some("a".into())
        );
    }

    #[tokio::test]
    async fn cancel_makes_submit_fail() {
        let broker = broker();
        let pending = broker.request();
        let id = pending.request_id;
        broker.cancel(id);
        assert!(!broker.submit(id, Some("x".into())));
        assert!(
            pending.into_receiver().await.is_err(),
            "receiver dropped on cancel"
        );
    }

    #[test]
    fn dropping_pending_request_cancels_entry() {
        let broker = broker();
        let id = {
            let pending = broker.request();
            pending.request_id
        };
        // 未应答即 drop → 条目取消，之后不可能再被应答。
        assert!(!broker.submit(id, Some("x".into())));
    }

    #[test]
    fn request_ids_are_unique_and_monotonic() {
        let broker = broker();
        let a = broker.request();
        let b = broker.request();
        assert_ne!(a.request_id, b.request_id);
        assert!(b.request_id > a.request_id);
    }
}
