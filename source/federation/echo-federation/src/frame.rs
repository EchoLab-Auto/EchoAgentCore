//! 联邦线协议（federation Phase 1）：`FedFrame` 帧类型与兼容性约定。
//!
//! Core↔Core 链路上的唯一事实类型来源。设计约束（见
//! `document/federation-rfc.md` §3）：
//!
//! - externally-tagged enum（`{"type": ..., "payload": ...}`），变体/字段名为
//!   线上契约，只增不改
//! - 新增字段一律 `#[serde(default)]`——新旧节点混部时旧端点可解码新帧
//! - `call_id = <origin_node>:<ulid>`：全局唯一 + 因果溯源，跨机回环可检测

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

    /// 通用错误（无法归入某个 call_id 时 call_id 为 None）。
    Error {
        #[serde(default)]
        call_id: Option<String>,
        code: FedError,
        message: String,
    },
}

/// 握手载荷：节点身份 + 能力声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeHello {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub protocol_version: u32,
    /// Core 版本（`env!("CARGO_PKG_VERSION")`），观测用。
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub caps: NodeCaps,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResultFrame {
    pub call_id: String,
    pub success: bool,
    #[serde(default)]
    pub payload: serde_json::Value,
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
/// ULID 语义等价（时间序 + 唯一），但复用 NodeId 的生成器过重——call_id
/// 只需「节点内唯一 + 可读」，时间戳 + 原子序号足够。
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

    /// 兼容策略：新字段带 serde(default) 时，旧端点（缺字段的 JSON）可解码。
    #[test]
    fn invoke_request_decodes_without_new_fields() {
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
            },
        };
        let frame = FedFrame::Hello(hello.clone());
        let text = serde_json::to_string(&frame).unwrap();
        let back: FedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, frame);
        // 老端点缺 caps/version/node_name 也可解码
        let legacy = r#"{"node_id": "node-abc", "protocol_version": 1}"#;
        let parsed: NodeHello = serde_json::from_str(legacy).unwrap();
        assert!(parsed.caps.tools.is_empty());
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
