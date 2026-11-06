# 将通用联邦连接模块用于其他软件

`source/modules/echo-federation` 是独立、业务无关的 Rust crate。它只负责去中心化
节点之间的连接生命周期，不包含 Agent、工具、会话、工作区或 Panel 业务。

## 最小接入

应用定义自己的消息和节点能力：

```rust
use echo_federation::{Federation, Frame, LinkEvent, PeerConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum AppMessage {
    Ping { text: String },
    Pong { text: String },
}
impl Frame for AppMessage {}

let (events, mut event_rx) = mpsc::channel(64);
let (shutdown, _) = watch::channel(false);
let federation = Federation::<AppMessage>::new(
    "my-node-id".into(),
    Some("My node".into()),
    env!("CARGO_PKG_VERSION").into(),
    json!({"service": "my-service"}), // 任意 JSON，不是 Core 的 NodeCaps
    vec![PeerConfig {
        name: "peer-a".into(),
        url: "ws://127.0.0.1:9000".into(),
        token: "shared-secret".into(),
    }],
    Some("ws://127.0.0.1:9001".into()),
    events,
    shutdown,
);

federation.clone().run(Some("127.0.0.1:9001".into())).await?;
federation.send_to("peer-node-id", AppMessage::Ping { text: "hello".into() }).await?;

while let Some(event) = event_rx.recv().await {
    match event {
        LinkEvent::Up(peer) => { /* 节点就绪；peer.caps 是应用自定义 JSON */ }
        LinkEvent::Down { peer_node_id, reason } => { /* 连接状态 */ }
        LinkEvent::Frame { from, frame } => { /* 应用自行路由与裁决 */ }
    }
}
```

## 模块负责与不负责

| 通用模块负责 | 使用方负责 |
|---|---|
| 监听、拨出与双向对称链路 | 定义应用消息枚举并实现空 `Frame` |
| Bearer 认证与节点身份握手 | 分配节点 ID、保存与分发 Peer 密钥 |
| 协议版本检查与自连接拒绝 | 自定义请求处理、授权和业务路由 |
| 心跳、写超时与断线重连 | 业务超时、重试与持久化 |
| 双向拨号去重 | 应用消息幂等 |
| 邀请编解码与地址观测 | 邀请展示及配对后的配置落盘 |
| 连接诊断和节点能力元数据透传 | 解释能力字段及执行对应功能 |

完整可执行的双节点测试见 `source/modules/echo-federation/tests/reusable.rs`。
该测试使用库存业务消息，而不是任何 Core 类型。

## 既有协议适配

需要兼容已有应用协议时，可覆盖 `Frame::encode_wire` 和 `Frame::decode_wire`。
Core 的 `echo-agent-federation` 正是通过这种适配保留原来的 `FedFrame` 格式；
通用模块本身不依赖该适配层。
