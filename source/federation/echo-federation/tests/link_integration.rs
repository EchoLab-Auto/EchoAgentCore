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
        active_turns: 0,
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
        None,
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
        None,
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

/// 远程可调用工具的真源是**注入的 reporter**（2026-11 S8）。
///
/// 构造期的 `caps.tools` 只是"还没接线时的快照"：生产装配里
/// `Federation::new` 早于任何工具注册表，真正能接的活只有握手那一刻才知道
/// （工具自己的 `remote_capable()` 声明，经注册表汇总）。这条测试锁两件事：
///   ① reporter 覆盖构造快照；
///   ② reporter 变了 → 下一次握手立刻反映（插件启停无需重启节点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_tools_come_from_injected_reporter() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (b_events, mut b_rx) = mpsc::channel::<LinkEvent>(16);
    let (sd_a, sd_b) = shutdown_pair();

    // B 构造时宣告一个假名字（模拟"启动早、注册表还没建"），真值由 reporter 给。
    let fed_b = Federation::new(
        "node-b".into(),
        None,
        caps(&["stale_placeholder"]),
        peers(&[("a", "", "secret")]),
        None,
        b_events,
        sd_b.clone(),
    );
    let live: Arc<std::sync::RwLock<Vec<String>>> =
        Arc::new(std::sync::RwLock::new(vec!["read_file".into(), "write_file".into()]));
    let live_writer = live.clone();
    fed_b
        .set_tool_reporter(Arc::new(move || live_writer.read().unwrap().clone()))
        .await;
    let fed_b2 = fed_b.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fed = fed_b2.clone();
            tokio::spawn(async move { fed.accept_one_pub(stream).await });
        }
    });

    // 第一个客户端：看到的是 reporter 的实时集合，而非构造快照。
    let (a_events, mut a_rx) = mpsc::channel::<LinkEvent>(16);
    let fed_a = Federation::new(
        "node-a".into(),
        None,
        caps(&[]),
        peers(&[("b", &addr, "secret")]),
        None,
        a_events,
        sd_a.clone(),
    );
    let fed_a2 = fed_a.clone();
    tokio::spawn(async move { fed_a2.run(None).await });
    let up_a = wait_up(&mut a_rx, Duration::from_secs(5)).await;
    let _ = wait_up(&mut b_rx, Duration::from_secs(5)).await;
    assert_eq!(
        up_a.caps.tools,
        vec!["read_file".to_string(), "write_file".to_string()],
        "握手必须用 reporter 的实时值，而不是构造快照"
    );
    drop(fed_a); // 断开第一个客户端，让 B 的连接表空出来
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 工具声明变化（插件停用）→ 新连接立刻看到新集合。
    *live.write().unwrap() = vec!["bash".into()];
    let (c_events, mut c_rx) = mpsc::channel::<LinkEvent>(16);
    let fed_c = Federation::new(
        "node-c".into(),
        None,
        caps(&["stale_placeholder"]),
        peers(&[("b", &addr, "secret")]),
        None,
        c_events,
        sd_a.clone(),
    );
    let fed_c2 = fed_c.clone();
    tokio::spawn(async move { fed_c2.run(None).await });
    let up_c = wait_up(&mut c_rx, Duration::from_secs(5)).await;
    assert_eq!(
        up_c.caps.tools,
        vec!["bash".to_string()],
        "reporter 变了，新连接必须看到新集合（无需重启节点）"
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
        None,
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
        None,
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
        None,
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
    let fed = Federation::new(
        "node-a".into(),
        None,
        NodeCaps::default(),
        vec![],
        None,
        tx,
        sd,
    );
    let mut hello = echo_federation::NodeHello {
        node_id: "node-b".into(),
        node_name: None,
        protocol_version: PROTOCOL_VERSION + 1,
        version: String::new(),
        caps: NodeCaps::default(),
        advertise: None,
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

/// 地址互告（配对无方向性基础）：Hello 携带本机 `advertise`，对端在
/// 链路 Up 的 PeerInfo 里可见——装配层据此学习对端地址、双方皆可重连。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_advertises_dial_address() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (b_events, mut b_rx) = mpsc::channel::<LinkEvent>(16);
    let (a_events, mut a_rx) = mpsc::channel::<LinkEvent>(16);
    let (sd_a, sd_b) = shutdown_pair();

    let fed_b = Federation::new(
        "node-b".into(),
        None,
        NodeCaps::default(),
        peers(&[("a", "", "secret")]),
        None,
        b_events,
        sd_b.clone(),
    );
    tokio::spawn({
        let fed = fed_b.clone();
        async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let fed = fed.clone();
                tokio::spawn(async move { fed.accept_one_pub(stream).await });
            }
        }
    });

    let fed_a = Federation::new(
        "node-a".into(),
        None,
        NodeCaps::default(),
        peers(&[("b", &addr, "secret")]),
        Some("ws://a.example:3133".into()),
        a_events,
        sd_a.clone(),
    );
    tokio::spawn({
        let fed = fed_a.clone();
        async move { fed.run(None).await }
    });

    let up_b = wait_up(&mut b_rx, Duration::from_secs(5)).await;
    let up_a = wait_up(&mut a_rx, Duration::from_secs(5)).await;
    // A 宣告 → B 看到 A 的地址；B 未宣告 → A 看到 None。
    assert_eq!(up_b.advertise.as_deref(), Some("ws://a.example:3133"));
    assert_eq!(up_a.advertise, None);

    let _ = sd_a.send(true);
    let _ = sd_b.send(true);
}

/// 双向 dial（配对无方向性稳态）：双方互配对方地址——连接收敛为**单条**
/// 且**静止**（无"互相顶掉 → 对方重连"的无限对拆），帧双向可达。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bidirectional_dial_converges_and_quiesces() {
    let la = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lb = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addra = format!("ws://{}", la.local_addr().unwrap());
    let addrb = format!("ws://{}", lb.local_addr().unwrap());
    let (a_events, mut a_rx) = mpsc::channel::<LinkEvent>(64);
    let (b_events, mut b_rx) = mpsc::channel::<LinkEvent>(64);
    let (sd_a, sd_b) = shutdown_pair();

    // 双方都监听 + 都配对方地址（双向学习的稳态）。
    let fed_a = Federation::new(
        "node-a".into(),
        None,
        NodeCaps::default(),
        peers(&[("b", &addrb, "secret")]),
        None,
        a_events,
        sd_a.clone(),
    );
    let fed_b = Federation::new(
        "node-b".into(),
        None,
        NodeCaps::default(),
        peers(&[("a", &addra, "secret")]),
        None,
        b_events,
        sd_b.clone(),
    );
    tokio::spawn({
        let fed = fed_a.clone();
        async move {
            loop {
                let (stream, _) = la.accept().await.unwrap();
                let fed = fed.clone();
                tokio::spawn(async move { fed.accept_one_pub(stream).await });
            }
        }
    });
    tokio::spawn({
        let fed = fed_b.clone();
        async move {
            loop {
                let (stream, _) = lb.accept().await.unwrap();
                let fed = fed.clone();
                tokio::spawn(async move { fed.accept_one_pub(stream).await });
            }
        }
    });
    tokio::spawn({
        let fed = fed_a.clone();
        async move { fed.run(None).await }
    });
    tokio::spawn({
        let fed = fed_b.clone();
        async move { fed.run(None).await }
    });

    // 收敛：双方各自 Up（可能伴随一次"被顶掉"型 Down——双向竞争）。
    let up_b = wait_up(&mut b_rx, Duration::from_secs(6)).await;
    let up_a = wait_up(&mut a_rx, Duration::from_secs(6)).await;
    assert_eq!(up_a.node_id, "node-b");
    assert_eq!(up_b.node_id, "node-a");

    // 静止断言：固定 8s 观察窗内 Down ≤ 2。正常收敛最多一次"被顶掉"
    // 替换（双向竞争）；若停车/优选去重失效发生无限对拆，两侧会交替
    // 顶掉对方链路，Down 在窗口内持续出现（退避 1→2→4s 仍 ≥4 次）。
    let mut downs = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            ev = a_rx.recv() => {
                if let Some(LinkEvent::Down { .. }) = ev {
                    downs += 1;
                }
            }
            ev = b_rx.recv() => {
                if let Some(LinkEvent::Down { .. }) = ev {
                    downs += 1;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    assert!(
        downs <= 2,
        "link churn detected: {downs} Down events in 8s (bidirectional takeover loop)"
    );

    // 单链路稳态：双方各持一条活跃链路。
    assert_eq!(fed_a.active_peers().await, vec!["node-b".to_string()]);
    assert_eq!(fed_b.active_peers().await, vec!["node-a".to_string()]);

    // 帧双向可达（幸存链路可用）。
    fed_a
        .send_to(
            "node-b",
            FedFrame::Cancel {
                call_id: "probe-a".into(),
            },
        )
        .await
        .map_err(|_| "A→B send failed")
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let LinkEvent::Frame {
                frame: FedFrame::Cancel { call_id },
                ..
            } = b_rx.recv().await.unwrap()
            {
                break call_id;
            }
        }
    })
    .await
    .expect("B should receive A frame");
    assert_eq!(got, "probe-a");

    let _ = sd_a.send(true);
    let _ = sd_b.send(true);
}

/// 同步令牌镜像（端口合并的升级回调数据源）：与运行时 peer 表
/// （add/update/remove）始终一致；空令牌恒不命中。
#[tokio::test]
async fn synced_token_mirror_tracks_peer_lifecycle() {
    let (events, _rx) = mpsc::channel::<LinkEvent>(16);
    let (shutdown, _sd) = watch::channel(false);
    let fed = Federation::new(
        "node-mirror".into(),
        None,
        caps(&[]),
        peers(&[("a", "", "tok-a")]),
        None,
        events,
        shutdown,
    );

    // 初始镜像来自构造时的 peers 表。
    assert!(fed.has_peer_token_sync("tok-a"));
    assert!(!fed.has_peer_token_sync("tok-unknown"));
    assert!(!fed.has_peer_token_sync(""), "空令牌永不算命中");

    // add_peer 更新（改名/换 token 场景）。
    fed.add_peer(PeerConfig {
        name: "a".into(),
        url: String::new(),
        token: "tok-b".into(),
    })
    .await;
    assert!(!fed.has_peer_token_sync("tok-a"), "旧 token 应随更新失效");
    assert!(fed.has_peer_token_sync("tok-b"));

    // 新增第二条。
    fed.add_peer(PeerConfig {
        name: "c".into(),
        url: String::new(),
        token: "tok-c".into(),
    })
    .await;
    assert!(fed.has_peer_token_sync("tok-b"));
    assert!(fed.has_peer_token_sync("tok-c"));

    // 删除后镜像同步移除。
    assert!(fed.remove_peer("a").await);
    assert!(!fed.has_peer_token_sync("tok-b"));
    assert!(fed.has_peer_token_sync("tok-c"));
    // 删除不存在的 peer：镜像保持。
    assert!(!fed.remove_peer("nope").await);
    assert!(fed.has_peer_token_sync("tok-c"));
}

/// 错误 token 的拒绝必须是标准 HTTP 401（而非静默断开）——与管理面的
/// 同类修复对齐（2026-10）：此前 `ErrorResponse::new` 默认状态 200 被
/// tungstenite 放弃写出，客户端只见「Handshake not finished」，令牌问题
/// 被误诊为网络/协议故障。
#[tokio::test]
async fn wrong_token_gets_http_401() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("ws://{}", listener.local_addr().unwrap());
    let (b_events, _b_rx) = mpsc::channel::<LinkEvent>(16);
    let (sd_b, _sd) = shutdown_pair();
    let fed_b = Federation::new(
        "node-401-b".into(),
        None,
        NodeCaps::default(),
        peers(&[("a", "", "right-token")]),
        None,
        b_events,
        sd_b,
    );
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fed = fed_b.clone();
            tokio::spawn(async move { fed.accept_one_pub(stream).await });
        }
    });

    // 1) 错误 token：标准 HTTP 401。
    let request = echo_federation::bearer_request(&addr, "wrong-token").unwrap();
    let error = tokio_tungstenite::connect_async(request)
        .await
        .expect_err("wrong token must be rejected");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status(),
                tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED,
                "wrong-token rejection must be standard HTTP 401"
            );
        }
        other => panic!("expected HTTP 401, got {other:?}"),
    }

    // 2) 正确 token：正常升级（101）。
    let request = echo_federation::bearer_request(&addr, "right-token").unwrap();
    let ws = tokio_tungstenite::connect_async(request)
        .await
        .expect("authorized handshake must succeed");
    drop(ws);
}
