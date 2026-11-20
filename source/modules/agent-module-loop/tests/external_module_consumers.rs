//! 外部模块（独立产物：`kind = "stdio"` / `"dylib"`）的三个消费面。
//!
//! 这三条路径此前都是断的：模块能声明 `Contribution::Command` /
//! `Contribution::Text` / 发 `Emit`，但 Core 只问进程内注册表——
//! 于是外部模块声明的命令没人分发、声明的文本块永远不进提示词、
//! `take_events` 队列零消费方（框架侧封顶 1024 就是为防无人读时堆积）。
//!
//! 被测的是"**两种承载在消费面上一视同仁**"：进程内模块走同步 trait 方法，
//! 外部模块走协议 `Invoke`，但调用方看到的是一条统一的路径。
//!
//! 用 `InprocTransport` 起假模块而不是真子进程：这里验的是宿主侧的接线
//! （命令路由 / 文本块合并 / 事件出口），不是运输层本身——后者由
//! `echo-module-host` 的 conformance 套件覆盖（含 dylib 的真实 `.so`）。

use std::sync::Arc;

use echo_module_host::api::{
    Contribution, HostToModule, InvokeOutcome, ModuleToHost, Register, TextBlockContribution,
    Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use echo_module_host::inproc::{InprocIo, InprocModule, InprocTransport};
use echo_module_host::{ModuleSpec, ModuleSupervisor};
use serde_json::{json, Value};

const MODULE_ID: &str = "external-probe";

/// 假外部模块：声明一条命令 + 一个文本块，并在 `Ready` 后发一条事件。
struct ProbeModule;

impl InprocModule for ProbeModule {
    fn run(
        self: Box<Self>,
        mut io: InprocIo,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            while let Some(msg) = io.recv().await {
                match msg {
                    HostToModule::Hello(hello) => {
                        let _ = io
                            .send(ModuleToHost::Welcome(Welcome {
                                protocol: PROTOCOL_NAME.into(),
                                version: PROTOCOL_VERSION,
                                module_id: hello.module_id,
                                capabilities: vec!["tools".into()],
                            }))
                            .await;
                        let _ = io
                            .send(ModuleToHost::Register(Register {
                                contributions: vec![
                                    Contribution::Command(
                                        echo_module_host::api::CommandContribution {
                                            name: "ProbeDo".into(),
                                            description: "探针命令".into(),
                                        },
                                    ),
                                    Contribution::Text(TextBlockContribution {
                                        key: "probe:summary".into(),
                                        label: "探针摘要".into(),
                                        order: 30,
                                    }),
                                ],
                            }))
                            .await;
                        let _ = io.send(ModuleToHost::Ready).await;
                        // 就绪后主动上报一条事件——宿主侧的 `take_events`
                        // 队列要靠消费端取走，否则只会堆到封顶丢新。
                        let _ = io
                            .send(ModuleToHost::Emit(echo_module_host::api::Emit {
                                event: "probe.tick".into(),
                                payload: json!({"n": 1}),
                            }))
                            .await;
                    }
                    HostToModule::Invoke(inv) => {
                        let outcome = match inv.contribution.as_str() {
                            // 命令：回显收到的载荷，证明参数确实过了边界。
                            "ProbeDo" => InvokeOutcome::ok(format!(
                                "command ok: {}",
                                serde_json::to_string(&inv.payload).unwrap_or_default()
                            )),
                            // 文本块：回一段文本，内容是调用上下文里的会话 id
                            // ——证明 `session_id` 真的透传到了模块侧。
                            "probe:summary" => InvokeOutcome::ok(format!(
                                "probe summary for {}",
                                inv.ctx.session_id.as_deref().unwrap_or("-")
                            )),
                            other => InvokeOutcome::Error {
                                code: "unknown_contribution".into(),
                                message: format!("no such contribution: {other}"),
                            },
                        };
                        let _ = io
                            .send(ModuleToHost::InvokeResult(
                                echo_module_host::api::InvokeResult {
                                    call_id: inv.call_id,
                                    outcome,
                                },
                            ))
                            .await;
                    }
                    HostToModule::Drain(_) | HostToModule::Dispose => return,
                    _ => {}
                }
            }
        })
    }
}

/// 起一个假外部模块，把句柄挂到 `ModuleHost` 上。
async fn host_with_external() -> (
    Arc<agent_module_loop::modules::ModuleHost>,
    ModuleSupervisor,
) {
    let supervisor = ModuleSupervisor::new();
    let handle = supervisor
        .start(
            ModuleSpec {
                module_id: MODULE_ID.to_string(),
                config: json!({}),
            },
            Box::new(InprocTransport::new(|_| Box::new(ProbeModule))),
        )
        .await
        .expect("probe module should register");

    let host = Arc::new(agent_module_loop::modules::ModuleHost::new());
    host.set_external_modules(vec![handle]);
    (host, supervisor)
}

/// 外部模块声明的命令能被分发到（走协议 `Invoke`，而不是被静默丢弃）。
#[tokio::test]
async fn external_module_command_is_dispatched() {
    let (host, _supervisor) = host_with_external().await;

    let outcome = host
        .dispatch_raw("ProbeDo", json!({"list": "x"}))
        .await
        .expect("命令应由外部模块处理");
    let value = outcome.expect("模块应回成功");
    let text = value.as_str().expect("回包应是字符串");
    assert!(
        text.contains("command ok") && text.contains("list"),
        "模块应收到并回显载荷，实际 {text:?}"
    );
}

/// 未声明的命令仍然返回 `None`（不能让外部模块吞掉所有未知命令）。
#[tokio::test]
async fn unknown_command_still_returns_none() {
    let (host, _supervisor) = host_with_external().await;
    assert!(
        host.dispatch_raw("NopeNotMine", json!({})).await.is_none(),
        "没声明过的命令应落到默认分支"
    );
}

/// 命令归属查询能指出是哪个外部模块在处理（面板诊断用）。
#[tokio::test]
async fn command_owner_reports_the_external_module() {
    let (host, _supervisor) = host_with_external().await;
    assert_eq!(host.command_owner("ProbeDo").as_deref(), Some(MODULE_ID));
    assert_eq!(host.command_owner("NopeNotMine"), None);
}

/// 外部模块声明的文本块会进提示词，且 `session_id` 透传到模块侧。
#[tokio::test]
async fn external_text_block_is_collected() {
    let (host, _supervisor) = host_with_external().await;

    let blocks = host
        .collect_text_blocks_async(&json!({"session_id": "sess-42"}), |_| true)
        .await;

    let found = blocks
        .iter()
        .find(|(block, _)| block.key == "probe:summary")
        .map(|(_, text)| text.clone());
    let text = found.expect("外部模块声明的块应被收集");
    assert!(
        text.contains("sess-42"),
        "会话 id 应透传到模块侧（模块据此决定给哪个会话的内容），实际 {text:?}"
    );
}

/// 白名单过滤对**外部模块**同样生效（不是只过滤进程内模块）。
#[tokio::test]
async fn text_block_respects_the_persona_filter() {
    let (host, _supervisor) = host_with_external().await;
    let blocks = host
        .collect_text_blocks_async(&json!({"session_id": "s"}), |_| false)
        .await;
    assert!(
        blocks.is_empty(),
        "过滤钩子返回 false 时不该收集任何块，实际 {blocks:?}"
    );
}

/// 外部模块的 `Emit` 能被取走（此前队列零消费方，只会堆到封顶丢新）。
#[tokio::test]
async fn external_module_events_are_drained() {
    let (host, _supervisor) = host_with_external().await;

    // 给模块的 Emit 一点时间穿过通道。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut events = Vec::new();
    while events.is_empty() && std::time::Instant::now() < deadline {
        events = host.take_module_events();
        if events.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    assert_eq!(events.len(), 1, "应取到一条事件，实际 {events:?}");
    let (module, name, payload) = &events[0];
    assert_eq!(module, MODULE_ID);
    assert_eq!(name, "probe.tick");
    assert_eq!(payload, &json!({"n": 1}));

    // 取走即清空：第二次取不到（否则面板会看到重复事件）。
    assert!(host.take_module_events().is_empty(), "重复取用不应重复投递");
}

/// 没有外部模块时，三个消费面都要正常工作（不能因为缺句柄而报错/挂住）。
#[tokio::test]
async fn host_without_external_modules_stays_quiet() {
    let host = agent_module_loop::modules::ModuleHost::new();
    assert!(host.dispatch_raw("ProbeDo", json!({})).await.is_none());
    assert!(host
        .collect_text_blocks_async(&json!({}), |_| true)
        .await
        .is_empty());
    assert!(host.take_module_events().is_empty());
    assert!(host.external_module_ids().is_empty());
}

/// 卸载/替换外部模块句柄时，旧的声明不再生效。
#[tokio::test]
async fn replacing_handles_drops_the_old_declarations() {
    let (host, _supervisor) = host_with_external().await;
    assert!(host.dispatch_raw("ProbeDo", json!({})).await.is_some());

    host.set_external_modules(Vec::new());
    assert!(
        host.dispatch_raw("ProbeDo", json!({})).await.is_none(),
        "句柄清空后旧命令不该还能分发"
    );
    assert!(host.external_module_ids().is_empty());
}

/// 保证 `Value` / `Arc` 的导入被用到（避免 unused 警告时不必回头改）。
#[allow(dead_code)]
fn _type_anchors(_: Value, _: Arc<()>) {}
