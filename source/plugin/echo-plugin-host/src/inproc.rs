//! in-proc 运输层：插件与宿主同进程，类型直连（零序列化）。
//!
//! [`InprocTransport`] 持有一个工厂（`Arc<dyn Fn(PluginSpec) -> Box<dyn InprocPlugin>>`），
//! 每次 [`Transport::start`] 构造一个新插件实例，并在独立 tokio 任务中执行其
//! `run`；两侧各经一条容量 [`CHANNEL_CAPACITY`] 的 mpsc 通道交换协议消息。
//!
//! `run` 返回（或任务被 abort）视为插件退出：宿主侧 `recv` 观察为
//! [`TransportError::Closed`]，`send` 同样返回 [`TransportError::Closed`]。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::api::{HostToPlugin, PluginToHost};
use crate::transport::{PluginConnection, PluginSpec, Transport, TransportError};

/// 每个方向的消息通道容量。
pub const CHANNEL_CAPACITY: usize = 64;

/// 插件实现体：`run` 结束时视为插件退出。
pub trait InprocPlugin: Send {
    /// 由宿主在 start / 重启时调用一次，在独立任务中执行插件消息循环。
    fn run(self: Box<Self>, io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// 插件侧端点。
pub struct InprocIo {
    rx: mpsc::Receiver<HostToPlugin>,
    tx: mpsc::Sender<PluginToHost>,
}

impl InprocIo {
    /// 接收宿主消息；返回 `None` 表示宿主侧已关闭（插件应结束 `run`）。
    pub async fn recv(&mut self) -> Option<HostToPlugin> {
        self.rx.recv().await
    }

    /// 发送消息给宿主；返回 `false` 表示宿主侧已关闭。
    pub async fn send(&self, msg: PluginToHost) -> bool {
        self.tx.send(msg).await.is_ok()
    }
}

/// 插件工厂：按 [`PluginSpec`] 构造插件实例（每次 start / 重启各调用一次）。
pub type InprocFactory = Arc<dyn Fn(PluginSpec) -> Box<dyn InprocPlugin> + Send + Sync>;

/// in-proc 运输层。
pub struct InprocTransport {
    factory: InprocFactory,
}

impl InprocTransport {
    /// 以插件工厂构造运输层。
    pub fn new(
        factory: impl Fn(PluginSpec) -> Box<dyn InprocPlugin> + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
        }
    }
}

/// 宿主侧连接。
struct InprocConnection {
    /// 宿主 → 插件；`None` = shutdown 已关闭该方向。
    to_plugin: Mutex<Option<mpsc::Sender<HostToPlugin>>>,
    /// 插件 → 宿主。
    from_plugin: Mutex<mpsc::Receiver<PluginToHost>>,
    /// 插件 `run` 任务句柄（shutdown 等待 / 超时强杀）。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl Transport for InprocTransport {
    async fn start(&self, spec: PluginSpec) -> Result<Box<dyn PluginConnection>, TransportError> {
        let plugin = (self.factory)(spec);
        let (to_plugin, plugin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (plugin_tx, from_plugin) = mpsc::channel(CHANNEL_CAPACITY);
        let io = InprocIo {
            rx: plugin_rx,
            tx: plugin_tx,
        };
        let task = tokio::spawn(plugin.run(io));
        Ok(Box::new(InprocConnection {
            to_plugin: Mutex::new(Some(to_plugin)),
            from_plugin: Mutex::new(from_plugin),
            task: Mutex::new(Some(task)),
        }))
    }
}

#[async_trait]
impl PluginConnection for InprocConnection {
    async fn send(&self, msg: HostToPlugin) -> Result<(), TransportError> {
        let guard = self.to_plugin.lock().await;
        match guard.as_ref() {
            Some(tx) => tx.send(msg).await.map_err(|_| TransportError::Closed),
            None => Err(TransportError::Closed),
        }
    }

    async fn recv(&self) -> Result<PluginToHost, TransportError> {
        let mut rx = self.from_plugin.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }

    /// 关停语义：
    /// 1. 丢弃宿主 → 插件 sender（插件侧 `recv` 得到 `None`，应自行结束）；
    /// 2. 等待插件任务结束，最多 `deadline`；
    /// 3. 超时 → `abort()` 强杀任务并返回 [`TransportError::Timeout`]（类比子进程轨
    ///    的 SIGKILL；此后插件视为已退出）。
    ///
    /// 重复调用：第二次直接 `Ok(())`（无任务可等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        self.to_plugin.lock().await.take();
        let mut task = match self.task.lock().await.take() {
            Some(task) => task,
            None => return Ok(()),
        };
        let res = tokio::time::timeout(deadline, &mut task).await;
        match res {
            Ok(_) => Ok(()),
            Err(_) => {
                task.abort();
                Err(TransportError::Timeout)
            }
        }
    }
}
