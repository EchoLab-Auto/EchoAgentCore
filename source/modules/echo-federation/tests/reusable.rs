//! 通用连接层的独立可复用性验证：消息类型与能力字段均由测试应用定义。

use std::time::Duration;

use echo_federation::{Federation, Frame, LinkEvent, PeerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum InventoryMessage {
    Reserve { sku: String, quantity: u32 },
    Reserved { sku: String, ok: bool },
}
impl Frame for InventoryMessage {}

async fn free_addr() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().to_string()
}

#[tokio::test]
async fn arbitrary_application_messages_cross_two_nodes() {
    let address = free_addr().await;
    let token = "inventory-test-token";
    let (server_tx, mut server_rx) = mpsc::channel(16);
    let (client_tx, mut client_rx) = mpsc::channel(16);
    let (server_shutdown, _) = watch::channel(false);
    let (client_shutdown, _) = watch::channel(false);

    let server = Federation::<InventoryMessage>::new(
        "inventory-server".into(),
        Some("Warehouse".into()),
        "3.2.1".into(),
        json!({"service": "inventory", "regions": ["east", "west"]}),
        vec![PeerConfig {
            name: "client".into(),
            url: String::new(),
            token: token.into(),
        }],
        Some(format!("ws://{address}")),
        server_tx,
        server_shutdown,
    );
    let client = Federation::<InventoryMessage>::new(
        "inventory-client".into(),
        None,
        "0.8.0".into(),
        json!({"client_kind": "mobile"}),
        vec![PeerConfig {
            name: "server".into(),
            url: format!("ws://{address}"),
            token: token.into(),
        }],
        None,
        client_tx,
        client_shutdown,
    );

    let server_task = tokio::spawn({
        let server = server.clone();
        let address = address.clone();
        async move { server.run(Some(address)).await }
    });
    tokio::spawn({
        let client = client.clone();
        async move { client.run(None).await }
    });

    let server_info = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match server_rx.recv().await.unwrap() {
                LinkEvent::Up(info) => break info,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(server_info.node_id, "inventory-client");
    assert_eq!(server_info.caps["client_kind"], "mobile");

    let client_info = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client_rx.recv().await.unwrap() {
                LinkEvent::Up(info) => break info,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(client_info.caps["service"], "inventory");
    assert_eq!(client_info.version, "3.2.1");

    client
        .send_to(
            "inventory-server",
            InventoryMessage::Reserve {
                sku: "sku-42".into(),
                quantity: 2,
            },
        )
        .await
        .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match server_rx.recv().await.unwrap() {
                LinkEvent::Frame { from, frame } => break (from, frame),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(received.0, "inventory-client");
    assert_eq!(
        received.1,
        InventoryMessage::Reserve {
            sku: "sku-42".into(),
            quantity: 2
        }
    );

    server
        .send_to(
            "inventory-client",
            InventoryMessage::Reserved {
                sku: "sku-42".into(),
                ok: true,
            },
        )
        .await
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client_rx.recv().await.unwrap() {
                LinkEvent::Frame { frame, .. } => break frame,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        response,
        InventoryMessage::Reserved {
            sku: "sku-42".into(),
            ok: true
        }
    );

    drop(server_task);
}

#[test]
fn wire_protocol_does_not_impose_business_fields() {
    let frame = echo_federation::WireFrame::Application(InventoryMessage::Reserved {
        sku: "x".into(),
        ok: true,
    });
    let encoded = InventoryMessage::encode_wire(&frame).unwrap();
    assert!(encoded.contains("Reserved"));
    let decoded = InventoryMessage::decode_wire(&encoded).unwrap();
    assert_eq!(decoded, frame);

    // 能力只是 JSON；不存在 Core 的 NodeCaps 或固定工具/会话结构。
    let caps: Value = json!({"anything": [1, 2, 3]});
    assert_eq!(caps["anything"][1], 2);
}
