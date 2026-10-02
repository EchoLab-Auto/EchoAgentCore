//! 联邦集成测试（Phase 1 验收）：真实 TCP 上的两节点握手、认证、回环拒绝。

use std::sync::Arc;
use std::time::Duration;

use echo_federation::{FedFrame, Federation, LinkEvent, NodeCaps, PeerConfig, PROTOCOL_VERSION};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

fn caps(tools: &[&str]) -> NodeCaps {
    NodeCaps {
        tools: tools.iter().map(|s| s.to_string()).collect(),
        subagent: true,
        workspaces: vec![],
    }
}

fn peers(list: &[(&str, &str, &str)]) -> Vec<PeerConfig> {
    list.iter()
        .map(|(name, url, token)| PeerConfig {
            name: name.to_string(),
            url: url.to_string(),
            token: token.to_string(),
        })
        .collect()
}

fn shutdown_pair() -> (watch::Sender<bool>, watch::Sender<bool>) {
    (watch::channel(false).0, watch::channel(false).0)
}

/// 起两个节点：B 监听 + A 连出到 B（共享 token）→ 双方链路 Up。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_handshake_and_exchange_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (b_events, mut b_rx) = mpsc::channel::<LinkEvent>(16);
    let (a_events, mut a_rx) = mpsc::channel::<LinkEvent>(16);
    let (sd_a, sd_b) = shutdown_pair();

    let fed_b = Federation::new(
        "node-b".into(),
        None,
        caps(&["bash"]),
        peers(&[("a", "", "secret")]),
        b_events,
        sd_b.clone(),
    );
    // B：手动 accept 循环（run() 自带 bind；测试里用预绑 listener 需内联）。
    let fed_b2 = fed_b.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fed = fed_b2.clone();
            tokio::spawn(async move { fed.accept_one_pub(stream).await });
        }
    });

    let fed_a = Federation::new(
        "node-a".into(),
        Some("alpha".into()),
        caps(&["read_file"]),
        peers(&[("b", &addr, "secret")]),
        a_events,
        sd_a.clone(),
    );
    let fed_a2 = fed_a.clone();
    tokio::spawn(async move { fed_a2.run(None).await });

    // 双方都应在数秒内 Up。
    let up_a = wait_up(&mut a_rx, Duration::from_secs(5)).await;
    let up_b = wait_up(&mut b_rx, Duration::from_secs(5)).await;
    assert_eq!(up_a.node_id, "node-b");
    assert_eq!(up_b.node_id, "node-a");
    assert_eq!(up_b.node_name.as_deref(), Some("alpha"));
    assert_eq!(up_a.caps.tools, vec!["bash".to_string()]);

    // A → B 发一帧业务帧（Cancel 作载体）。
    fed_a
        .send_to(
            "node-b",
            FedFrame::Cancel {
                call_id: "node-a:1-0".into(),
            },
        )
        .await
        .map_err(|_| "send failed")
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let LinkEvent::Frame { from, frame } = b_rx.recv().await.unwrap() {
                break (from, frame);
            }
        }
    })
    .await
    .expect("B should receive frame");
    assert_eq!(got.0, "node-a");
    assert_eq!(
        got.1,
        FedFrame::Cancel {
            call_id: "node-a:1-0".into()
        }
    );

    let _ = sd_a.send(true);
    let _ = sd_b.send(true);
}

/// 认证失败：token 不符 → 握手前即被拒，不会 Up。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_token_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (b_events, mut b_rx) = mpsc::channel::<LinkEvent>(16);
    let (a_events, mut a_rx) = mpsc::channel::<LinkEvent>(16);
    let (sd_a, sd_b) = shutdown_pair();

    let fed_b = Federation::new(
        "node-b".into(),
        None,
        NodeCaps::default(),
        peers(&[("a", "", "right-token")]),
        b_events,
        sd_b.clone(),
    );
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fed = fed_b.clone();
            tokio::spawn(async move { fed.accept_one_pub(stream).await });
        }
    });

    let fed_a = Federation::new(
        "node-a".into(),
        None,
        NodeCaps::default(),
        peers(&[("b", &addr, "wrong-token")]),
        a_events,
        sd_a.clone(),
    );
    let fed_a2 = fed_a.clone();
    tokio::spawn(async move { fed_a2.run(None).await });

    // 2 秒内双方都不得 Up。
    assert!(wait_up_opt(&mut a_rx, Duration::from_secs(2))
        .await
        .is_none());
    assert!(wait_up_opt(&mut b_rx, Duration::from_millis(200))
        .await
        .is_none());

    let _ = sd_a.send(true);
    let _ = sd_b.send(true);
}

/// 回环拒绝：节点连自己（相同 node_id）→ 握手校验拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_dial_is_rejected_as_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (events, mut rx) = mpsc::channel::<LinkEvent>(16);
    let (sd, _) = shutdown_pair();

    let fed = Federation::new(
        "node-self".into(),
        None,
        NodeCaps::default(),
        peers(&[("self", &addr, "secret")]),
        events,
        sd.clone(),
    );
    tokio::spawn({
        let fed = fed.clone();
        async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let fed = fed.clone();
                tokio::spawn(async move { fed.accept_one_pub(stream).await });
            }
        }
    });
    let fed2 = fed.clone();
    tokio::spawn(async move { fed2.run(None).await });

    // node_id 相同 → check_peer 拒绝 → 不得 Up。
    assert!(wait_up_opt(&mut rx, Duration::from_secs(3)).await.is_none());
    assert!(fed.active_peers().await.is_empty());

    let _ = sd.send(true);
}

/// 版本不符拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_version_mismatch_is_rejected() {
    // 直接测 check_peer 逻辑（走真实链路意义相同但需伪造帧，成本高）。
    let (tx, _rx) = mpsc::channel(1);
    let (sd, _) = shutdown_pair();
    let fed = Federation::new("node-a".into(), None, NodeCaps::default(), vec![], tx, sd);
    let mut hello = echo_federation::NodeHello {
        node_id: "node-b".into(),
        node_name: None,
        protocol_version: PROTOCOL_VERSION + 1,
        version: String::new(),
        caps: NodeCaps::default(),
    };
    assert!(fed.check_peer_pub(&hello).is_err());
    hello.protocol_version = PROTOCOL_VERSION;
    assert!(fed.check_peer_pub(&hello).is_ok());
    hello.node_id = "node-a".into();
    assert!(fed.check_peer_pub(&hello).is_err(), "self must be rejected");
}

async fn wait_up(
    rx: &mut mpsc::Receiver<LinkEvent>,
    within: Duration,
) -> echo_federation::PeerInfo {
    wait_up_opt(rx, within).await.expect("link should come up")
}

async fn wait_up_opt(
    rx: &mut mpsc::Receiver<LinkEvent>,
    within: Duration,
) -> Option<echo_federation::PeerInfo> {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Some(LinkEvent::Up(info)) => return Some(info),
                Some(_) => continue,
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[allow(dead_code)]
fn assert_fed_arc(_: Arc<Federation>) {}
