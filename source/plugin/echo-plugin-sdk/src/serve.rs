//! 服务循环：握手 + 消息分发 + 单写者帧输出。

use std::collections::HashSet;
use std::sync::Arc;

use echo_plugin_api::{
    compatible, Contribution, Failure, HostToPlugin, InvokeOutcome, InvokeResult, PluginToHost,
    Register, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::error::SdkError;
use crate::frame::{encode_frame, read_frame};
use crate::{InvokeRequest, PluginHandler, ServeOptions};

/// 在 stdio 上运行插件（读 stdin、写 stdout），直到排空 / 终止 / EOF 退出。
pub async fn serve_stdio(handler: impl PluginHandler) -> std::io::Result<()> {
    serve_stdio_with(handler, ServeOptions::default()).await
}

/// [`serve_stdio`] 的显式选项版（`plugin_id` / `capabilities`）。
pub async fn serve_stdio_with(
    handler: impl PluginHandler,
    options: ServeOptions,
) -> std::io::Result<()> {
    serve_io_with(handler, options, tokio::io::stdin(), tokio::io::stdout()).await
}

/// 泛型核心：在任意读写流上运行插件（测试经 `tokio::io::duplex` 复用）。
///
/// `writer` 由内部单写者任务独占（帧不会交错）；`reader` 在主循环中读取。
pub async fn serve_io<R, W>(
    handler: impl PluginHandler,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_io_with(handler, ServeOptions::default(), reader, writer).await
}

/// [`serve_io`] 的显式选项版。
pub async fn serve_io_with<R, W>(
    handler: impl PluginHandler,
    options: ServeOptions,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run(handler, options, reader, writer)
        .await
        .map_err(std::io::Error::from)
}

/// 退出路径：
/// - `Drain`：`on_drain` 后等待在途调用完成（结果全部写回）再退出；
/// - `Dispose` / stdin EOF：中止在途任务后退出。
async fn run<R, W>(
    handler: impl PluginHandler,
    options: ServeOptions,
    mut reader: R,
    writer: W,
) -> Result<(), SdkError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(handler);
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(write_frames(rx, writer));

    // 1) 首帧必须 Hello；Hello 前 EOF 视为干净退出（宿主已在关停）。
    let Some(first) = read_frame(&mut reader).await? else {
        drop(tx);
        return finish_writer(writer_task).await;
    };
    let hello = match first {
        HostToPlugin::Hello(hello) => hello,
        other => {
            let message = format!("first frame must be Hello, got {other:?}");
            tracing::warn!(%message, "rejecting plugin session");
            write_failure(&tx, "protocol_violation", &message);
            drop(tx);
            let _ = finish_writer(writer_task).await;
            return Err(SdkError::Protocol(message));
        }
    };

    // 2) 校验协议名与版本（与宿主 `start_and_handshake` 同口径）。
    if hello.protocol != PROTOCOL_NAME || !compatible(PROTOCOL_VERSION, hello.version) {
        let message = format!(
            "incompatible handshake: host protocol {:?} v{}, plugin supports {:?} v{}",
            hello.protocol, hello.version, PROTOCOL_NAME, PROTOCOL_VERSION
        );
        tracing::warn!(%message, "rejecting plugin session");
        write_failure(&tx, "incompatible_protocol", &message);
        drop(tx);
        let _ = finish_writer(writer_task).await;
        return Err(SdkError::Protocol(message));
    }

    // 3) Welcome + Register + Ready（顺序入队；单写者保证线上顺序）。
    let contributions = handler.contributions();
    let registered: Arc<HashSet<String>> = Arc::new(
        contributions
            .iter()
            .filter_map(contribution_target)
            .collect(),
    );
    enqueue(
        &tx,
        &PluginToHost::Welcome(Welcome {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            plugin_id: options.plugin_id.clone(),
            capabilities: options.capabilities.clone(),
        }),
    )?;
    enqueue(&tx, &PluginToHost::Register(Register { contributions }))?;
    enqueue(&tx, &PluginToHost::Ready)?;
    tracing::debug!(
        plugin_id = %options.plugin_id,
        contributions = registered.len(),
        "handshake complete"
    );

    // 4) 消息循环：Invoke 并发处理（结果经 mpsc 单写者串行写回）。
    let mut tasks: JoinSet<()> = JoinSet::new();
    loop {
        let Some(msg) = read_frame(&mut reader).await? else {
            // stdin EOF：宿主关停；中止在途调用后退出。
            tasks.abort_all();
            break;
        };
        match msg {
            HostToPlugin::Invoke(invoke) => {
                let handler = Arc::clone(&handler);
                let registered = Arc::clone(&registered);
                let tx = tx.clone();
                tasks.spawn(async move {
                    let call_id = invoke.call_id;
                    let known = registered.contains(&invoke.contribution);
                    let outcome = if known {
                        handler
                            .invoke(InvokeRequest {
                                call_id: call_id.clone(),
                                contribution: invoke.contribution,
                                ctx: invoke.ctx,
                                payload: invoke.payload,
                            })
                            .await
                    } else {
                        InvokeOutcome::Error {
                            code: "unknown_contribution".to_string(),
                            message: format!("unknown contribution: {}", invoke.contribution),
                        }
                    };
                    let _ = enqueue(
                        &tx,
                        &PluginToHost::InvokeResult(InvokeResult { call_id, outcome }),
                    );
                });
            }
            HostToPlugin::Cancel(cancel) => {
                let handler = Arc::clone(&handler);
                tasks.spawn(async move {
                    handler.on_cancel(&cancel.call_id).await;
                });
            }
            HostToPlugin::Drain(drain) => {
                handler.on_drain(drain.deadline_ms).await;
                break;
            }
            HostToPlugin::Dispose => {
                tasks.abort_all();
                break;
            }
            HostToPlugin::Event(_) => {
                // 未订阅的事件：忽略（SDK 暂不提供事件钩子）。
            }
            HostToPlugin::Hello(_) => {
                tracing::warn!("ignoring duplicate Hello");
            }
        }
    }

    // 5) 收尾：Drain 等待在途任务完成；Dispose / EOF 已中止。写者最后收口。
    drop(tx);
    while tasks.join_next().await.is_some() {}
    finish_writer(writer_task).await
}

/// 贡献名（`Invoke` 的寻址目标）：Tool / Skill 用 name，Service 用 key；
/// 事件订阅不参与寻址。
fn contribution_target(contribution: &Contribution) -> Option<String> {
    match contribution {
        Contribution::Tool(tool) => Some(tool.name.clone()),
        Contribution::Skill(skill) => Some(skill.name.clone()),
        Contribution::Service(service) => Some(service.key.clone()),
        Contribution::Events(_) => None,
    }
}

/// 编码并入队一帧（不等待写出；顺序由单写者保证）。
fn enqueue(tx: &mpsc::UnboundedSender<Vec<u8>>, msg: &PluginToHost) -> Result<(), SdkError> {
    let frame = encode_frame(msg)?;
    tx.send(frame)
        .map_err(|_| SdkError::Protocol("frame writer task is gone".to_string()))
}

/// 尽力写一条 `Failed`（初始化拒绝路径）。
fn write_failure(tx: &mpsc::UnboundedSender<Vec<u8>>, code: &str, message: &str) {
    let _ = enqueue(
        tx,
        &PluginToHost::Failed(Failure {
            code: code.to_string(),
            message: message.to_string(),
        }),
    );
}

/// 等待写者任务收口（通道关闭 = 全部帧写完）。
async fn finish_writer(
    writer_task: tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<(), SdkError> {
    match writer_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SdkError::Io(error)),
        Err(error) => Err(SdkError::Protocol(format!(
            "frame writer task failed: {error}"
        ))),
    }
}

/// 单写者任务：按到达顺序逐帧 `write_all` + `flush`（stdout 有行缓冲，
/// 必须逐帧 flush，握手帧才能及时送达宿主）。
async fn write_frames<W>(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut writer: W,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        writer.write_all(&frame).await?;
        writer.flush().await?;
    }
    Ok(())
}
