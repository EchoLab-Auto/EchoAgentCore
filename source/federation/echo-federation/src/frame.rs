//! 联邦线协议（federation Phase 1）：`FedFrame` 帧类型与兼容性约定。
//!
//! Core↔Core 链路上的唯一事实类型来源。设计约束（见
//! `document/federation-rfc.md` §3）：
//!
//! - externally-tagged enum（`{"type": ..., "payload": ...}`），变体/字段名为
//!   线上契约，只增不改
//! - 全网同步更新（无旧版本兼容）：字段变更直接收紧；`serde(default)` 仅用于
//!   语义上真正可省略的字段（如 `Option` 且带 `skip_serializing_if`），
//!   不再为「旧端点缺字段」保留容忍
//! - `call_id = <origin_node>:<毫秒时间戳>-<进程内序号>`：全局唯一 + 因果溯源，跨机回环可检测

use serde::{Deserialize, Serialize};

/// 当前协议版本。Hello 握手时比对：主版本不一致拒绝连接。
pub const PROTOCOL_VERSION: u32 = 1;

/// 联邦链路帧（Core↔Core）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum FedFrame {
    // ── 连接生命周期 ──
    /// 主动方握手：宣告节点身份/能力/协议版本。
    Hello(NodeHello),
    /// 被动方回握：接受连接并宣告自身。
    Welcome(NodeHello),
    /// 应用层心跳（WS Ping/Pong 之外的存活证据，携带本端时间戳）。
    Ping {
        sent_at_ms: i64,
    },
    Pong {
        sent_at_ms: i64,
        echo_ms: i64,
    },

    // ── 工具调用 RPC（Phase 2 消费，帧类型先行冻结） ──
    Invoke(InvokeRequest),
    /// 受理回执（含对端裁决结果：Accepted/Rejected/NeedsConfirm）。
    InvokeAccepted {
        call_id: String,
        verdict: InvokeVerdict,
    },
    /// 流式中间输出（bash/shell 类工具；可多次）。
    InvokeOutput {
        call_id: String,
        chunk: String,
        #[serde(default)]
        stream: OutputStream,
    },
    /// 终态（成功或失败均经此帧；之后该 call_id 的帧一律忽略）。
    InvokeResult(InvokeResult),
    /// 取消传播（尽力送达；对端可能已完成）。
    Cancel {
        call_id: String,
    },

    // ── 远程委派（Phase 3 消费） ──
    SubagentSpawn(SubagentSpawnRequest),
    SubagentEvent(SubagentEventFrame),

    // ── 只读状态查询（Phase 5 消费） ──
    Query(QueryRequest),
    QueryResult(QueryResultFrame),

    // ── 会话迁移分块导入（P3-2b 消费） ──
    /// 源 → 目标：会话事件日志文本的分块。
    SessionImport(SessionImportFrame),
    /// 目标 → 源：按序确认（流控 + 超时重发用）。
    SessionImportAck(SessionImportAckFrame),
    /// 目标 → 源：导入结果（终态）。
    SessionImportResult(SessionImportResultFrame),

    /// 通用错误（无法归入某个 call_id 时 call_id 为 None）。
    Error {
        #[serde(default)]
        call_id: Option<String>,
        code: FedError,
        message: String,
    },
}

/// 握手载荷：节点身份 + 能力声明。
/// 握手载荷：节点身份 + 能力声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeHello {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub protocol_version: u32,
    /// Core 版本（`env!("CARGO_PKG_VERSION")`），观测用。
    pub version: String,
    pub caps: NodeCaps,
    /// 本机**对外可拨地址**（`ws://host:port`；None = 未监听/不宣告）。
    ///
    /// 配对无方向性（2026-10）：链路建立后对端据此学习本机地址，
    /// 由此双方都能主动重连——不再只有"接入方"单向保持。
    /// 取值与邀请串同源（`[federation] listen/advertise` 推导）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise: Option<String>,
}

/// 节点能力声明（供大脑侧路由决策与代理工具注册）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCaps {
    /// 可被远程调用的工具名（本机工具注册表的子集）。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 是否接受远程 subagent 委派。
    #[serde(default)]
    pub subagent: bool,
    /// 声明开放的工作区目录（本机绝对路径）。
    #[serde(default)]
    pub workspaces: Vec<String>,
    /// 当前活跃 turn 数（调度负载指标；2026-10 P2 调度器）。
    /// 握手时快照——负载随时间变化，精确值经 Query(NodeStatus) 拉取。
    #[serde(default)]
    pub active_turns: u32,
}

/// 工具调用请求。`args` 为该工具本机 schema 的 JSON。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeRequest {
    /// `<origin_node>:<ulid>`（见 [`new_call_id`]）。
    pub call_id: String,
    /// 本机工具名（不带节点前缀——前缀只存在于大脑侧注册表）。
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
    /// 调用方期望的工作目录上下文（对端按自身工作区根裁决）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// 超时提示（秒）；对端可在自身上限内收紧。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// 对端裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvokeVerdict {
    Accepted,
    /// 白名单拒绝/门控拒绝。
    Rejected,
    /// 需人工/门控确认（后续仍经 InvokeAccepted(Accepted|Rejected) 终裁）。
    NeedsConfirm,
}

/// 工具调用终态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResult {
    pub call_id: String,
    pub success: bool,
    /// 文本结果（成功）或错误描述（失败）。
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// 流式输出通道。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    #[default]
    Stdout,
    Stderr,
}

/// 远程 subagent 委派（Phase 3：观测与取消用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentSpawnRequest {
    pub call_id: String,
    /// 自含任务描述（同本地 spawn_subagent 的约束）。
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// 远程 subagent 生命周期事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentEventFrame {
    pub call_id: String,
    pub status: SubagentStatus,
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 只读查询（Phase 5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    pub call_id: String,
    pub kind: QueryKind,
    /// 查询目标（如 session_id / 工作区目录路径）；含义按 kind 定。
    #[serde(default)]
    pub subject: String,
    /// 人格维度（`SessionSnapshot` 必填）：同名会话（每个 persona 都有
    /// `local:tui::local_user`）必须按人格限定，不限定即歧义。
    pub team_id: String,
    /// 快照分页（Phase 5）：只回 seq 大于该值的条目（0 = 最近窗口）。
    #[serde(default)]
    pub since_seq: u64,
    /// 返回条目上限（0 = 默认 50）。
    #[serde(default)]
    pub limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryKind {
    SessionSnapshot,
    WorkspaceFiles,
    NodeStatus,
    /// 远程 git 状态采集（跨机工作区，2026-10）：`subject` = 目录绝对路径，
    /// 执行端复用 workspace 插件的 `collect_dir_git`（限本机工作区目录并集
    /// 沙箱）。旧对端不认识本变体——externally-tagged 反序列化失败整帧
    /// 丢弃，大脑侧按「远程不可采集」降级（与旧行为一致）。
    WorkspaceGitStatus,
    /// 供应商池查询（分布式共享，2026-10）：返回**脱敏**的
    /// `[agent.api_profiles]`（provider/model/base_url/key_set 布尔——
    /// **api_key 明文永不过线**，导入端需手动补填）。默认放行
    /// （脱敏后无敏感信息），无需 allow_queries 显式开启。
    ApiProfiles,
    /// 目录选择器浏览（2026-10）：`subject` = 目录绝对路径（空 = 根列表
    /// 请求）；执行端限"浏览根"（工作区目录并集 ∪ HOME），返回子目录
    /// 或根列表。授权走 `allow_queries` 的 `browse_directories` 项
    /// （目录结构隐私敏感，默认不放行——与 workspace_files 同级）。
    BrowseDirectories,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResultFrame {
    pub call_id: String,
    pub success: bool,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// 会话迁移分块导入（P3-2b）：把源会话的事件日志文本按块送到目标节点。
///
/// 迁移保持同一 `session_id`；目标按 `team_id` 找到 persona 后把事件追加进
/// 该 persona 的事实来源日志（`import_session_events`）。WebSocket 链路有序
/// 可靠，块按 `chunk_index` 顺序送达；链路中断则整次迁移失败可重试。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionImportFrame {
    /// 一次迁移的关联 id（源生成；用于目标侧聚合分块与回执）。
    pub transfer_id: String,
    /// 会话 id（迁移前后一致）。
    pub session_id: String,
    /// 目标人格 id（目标节点必须存在该 persona）。
    pub team_id: String,
    /// 源侧对目标的 peer 配置名（目标原样回执，便于源侧发结果事件）。
    #[serde(default)]
    pub target_peer: String,
    /// 分块序号（从 0 起，按序）。
    pub chunk_index: u32,
    /// 总分块数（`chunk_index + 1 == chunk_total` 即最后一块）。
    pub chunk_total: u32,
    /// 事件日志 JSON（`Vec<SessionEvent>`）的 UTF-8 文本块。
    pub data: String,
}

/// 会话迁移按序确认（目标 → 源）：`acked_upto` = 连续收到的块数
/// （0..=chunk_total）。源据此推进窗口；源超时未收到推进则从 `acked_upto`
/// 起重发（目标对重复块幂等覆盖）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionImportAckFrame {
    pub transfer_id: String,
    pub acked_upto: u32,
}

/// 会话迁移导入结果（目标 → 源）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionImportResultFrame {
    pub transfer_id: String,
    /// 原样回带导入的会话/目标标签，源侧无需本地 pending 表即可发结果事件。
    pub session_id: String,
    #[serde(default)]
    pub target_peer: String,
    pub success: bool,
    #[serde(default)]
    pub imported_events: usize,
    #[serde(default)]
    pub message: String,
}

/// 联邦错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FedError {
    /// 协议版本不兼容。
    VersionMismatch,
    /// 认证失败。
    Unauthorized,
    /// 白名单/门控拒绝。
    Forbidden,
    /// 帧来自本节点 origin（回环）。
    LoopDetected,
    /// call_id 未知（重连后的迟到帧）。
    UnknownCall,
    /// 对端内部错误。
    Internal,
}

/// 生成全局唯一 call_id：`<origin_node>:<时间戳ms>-<进程内序号>`。
///
/// 只需「节点内唯一 + 可读 + 大致时间序」——时间戳 + 原子序号足够（不复用
/// NodeId 的 ULID 生成器）。
pub fn new_call_id(origin_node: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!(
        "{origin_node}:{millis:x}-{:x}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// 取出 call_id 的 origin 节点段（回环检测用）。
pub fn call_origin(call_id: &str) -> &str {
    call_id.split_once(':').map(|(o, _)| o).unwrap_or(call_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_envelope_shape() {
        let frame = FedFrame::Cancel {
            call_id: "node-a:1-0".into(),
        };
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "Cancel");
        assert_eq!(v["payload"]["call_id"], "node-a:1-0");
        // roundtrip
        let back: FedFrame = serde_json::from_value(v).unwrap();
        assert_eq!(back, frame);
    }

    /// P3-2b：会话迁移分块帧往返 + 旧大脑缺 team_id 的查询可解码。
    #[test]
    fn session_import_frames_roundtrip() {
        let frame = FedFrame::SessionImport(SessionImportFrame {
            transfer_id: "mig-1".into(),
            session_id: "local:tui::local_user".into(),
            team_id: "alix".into(),
            target_peer: "gpu-box".into(),
            chunk_index: 1,
            chunk_total: 3,
            data: "\"events\"".into(),
        });
        let text = serde_json::to_string(&frame).unwrap();
        let back: FedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, frame);

        let ack = FedFrame::SessionImportAck(SessionImportAckFrame {
            transfer_id: "mig-1".into(),
            acked_upto: 2,
        });
        let text = serde_json::to_string(&ack).unwrap();
        let back: FedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, ack);

        let result = FedFrame::SessionImportResult(SessionImportResultFrame {
            transfer_id: "mig-1".into(),
            session_id: "local:tui::local_user".into(),
            target_peer: "gpu-box".into(),
            success: true,
            imported_events: 42,
            message: "ok".into(),
        });
        let text = serde_json::to_string(&result).unwrap();
        let back: FedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, result);
    }

    /// Invoke 可省略字段：`workdir`/`timeout_secs` 为 None 时发送方省略
    ///（`skip_serializing_if`），接收方按缺省解码；`args` 缺省 = 空参数。
    #[test]
    fn invoke_request_optional_fields_decode() {
        let minimal = r#"{"call_id": "n:1-0", "tool": "bash"}"#;
        let req: InvokeRequest = serde_json::from_str(minimal).unwrap();
        assert_eq!(req.tool, "bash");
        assert!(req.args.is_null());
        assert!(req.workdir.is_none());
        assert!(req.timeout_secs.is_none());
    }

    #[test]
    fn hello_roundtrip_with_caps() {
        let hello = NodeHello {
            node_id: "node-abc".into(),
            node_name: Some("gpu-box".into()),
            protocol_version: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            caps: NodeCaps {
                tools: vec!["bash".into(), "read_file".into()],
                subagent: true,
                workspaces: vec!["/srv/proj".into()],
                active_turns: 0,
            },
            advertise: Some("ws://10.0.0.5:3133".into()),
        };
        let frame = FedFrame::Hello(hello.clone());
        let text = serde_json::to_string(&frame).unwrap();
        assert!(text.contains("advertise"));
        let back: FedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn call_id_origin_extraction() {
        let id = new_call_id("node-a");
        assert!(id.starts_with("node-a:"));
        assert_eq!(call_origin(&id), "node-a");
        assert_ne!(new_call_id("node-a"), new_call_id("node-a"));
        assert_eq!(call_origin("no-colon"), "no-colon");
    }
}
