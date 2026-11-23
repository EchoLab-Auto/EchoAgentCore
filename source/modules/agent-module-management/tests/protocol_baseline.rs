//! 拆分前的**协议行为基线**（B1）：夹具驱动的线格式契约测试。
//!
//! 目的不是再测一遍「序列化能不能回读」（那已在 `src/protocol/*.rs` 的单测里
//! 覆盖），而是把**最容易在模块搬移/拆分中悄悄变形的三处语义**钉住：
//!
//! 1. **三个不同的"序号"**——它们都叫 seq，但语义互不替代：
//!
//!    | 位置 | 语义 | 消费方 |
//!    | --- | --- | --- |
//!    | 事件帧顶层 `seq` | 扇出序号（每进程单调递增） | Panel 事件流连续性校验 |
//!    | `TrunkTimeline.seq` | 时间线**同步游标** | `RequestTrunkTimeline.since_seq` |
//!    | `TimelineMessage.seq` / `.ord` | 条目序号 / **位置序号** | 增量去重 / 分页锚点 |
//!
//! 2. **全量快照 vs 增量**：`full` / `has_more` 必须显式上线（面板据此决定
//!    "替换缓存"还是"追加"），缺省语义只留给旧版 Core。
//!
//! 3. **通用模块调用/应答的配对键**：`InvokeModuleOp{module,method}` →
//!    `ModuleInvokeResult{module,method}` 必须逐字回显，否则并发调用的前端
//!    无法把应答配回请求。
//!
//! ## 约束
//!
//! - 夹具里的数据全部虚构（`persona-demo` / `vendor.demo.todo` /
//!   `demo:group:20002:10001`），不来自任何真实部署。
//! - 只锁**字段名与缺省语义**，不锁 JSON 键序：断言一律走解码后的字段或
//!   单键取值，从不比对整串序列化结果。
//! - 不引入跨版本分支：这里描述的就是当前线格式，没有"旧版/兼容"判定。

use agent_module_management::protocol::{
    deserialize_message, serialize_command, serialize_event, serialize_event_identified,
    BackendCommand, BackendEvent, TimelineMessage, WsMessage,
};

const FULL_SNAPSHOT: &str = include_str!("fixtures/trunk_timeline_full_snapshot.json");
const INCREMENTAL: &str = include_str!("fixtures/trunk_timeline_incremental.json");
const LEGACY_MINIMAL: &str = include_str!("fixtures/trunk_timeline_legacy_minimal.json");
const INVOKE_OP: &str = include_str!("fixtures/module_invoke_op.json");
const INVOKE_RESULT_OK: &str = include_str!("fixtures/module_invoke_result_ok.json");
const INVOKE_RESULT_ERR: &str = include_str!("fixtures/module_invoke_result_err.json");
const REQUEST_TIMELINE_INCREMENTAL: &str =
    include_str!("fixtures/request_trunk_timeline_incremental.json");
const REQUEST_EARLIER: &str = include_str!("fixtures/request_timeline_earlier.json");

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("fixture 必须是合法 JSON")
}

fn decode_event(text: &str) -> BackendEvent {
    match deserialize_message(text) {
        Some(WsMessage::Event(event)) => event,
        other => panic!("fixture 应是事件帧：{other:?}"),
    }
}

fn decode_command(text: &str) -> BackendCommand {
    match deserialize_message(text) {
        Some(WsMessage::Command(command)) => command,
        other => panic!("fixture 应是命令帧：{other:?}"),
    }
}

/// 顶层 `seq`（扇出序号）与 `TrunkTimeline.seq`（同步游标）是**两个独立的
/// 计数器**——面板用前者查漏帧、用后者拉增量；一旦被"顺手合并"成一个字段，
/// 症状是跳号判定误报 + 增量请求永远拉全量（或漏条目）。
#[test]
fn full_snapshot_keeps_envelope_seq_apart_from_timeline_cursor() {
    let frame = json(FULL_SNAPSHOT);
    let envelope_seq = frame["seq"].as_u64().expect("事件帧顶层 seq");
    assert_eq!(envelope_seq, 128, "扇出序号在帧顶层");

    let event = decode_event(FULL_SNAPSHOT);
    let BackendEvent::TrunkTimeline {
        messages,
        seq,
        team_id,
        full,
        has_more,
    } = &event
    else {
        panic!("应为 TrunkTimeline：{event:?}");
    };

    assert_eq!(*seq, 41, "时间线游标在 payload 里");
    assert_ne!(envelope_seq, *seq, "信封序号与时间线游标必须互不替代");
    assert_eq!(team_id.as_deref(), Some("persona-demo"));
    assert!(*full, "全量快照：面板据此替换缓存");
    assert!(*has_more, "被窗口上限截断时必须告诉面板还有更早的条目");

    assert_eq!(messages.len(), 2);
    // 条目序号（增量去重）与位置序号（分页锚点）各一套，都由服务端赋值。
    assert_eq!((messages[0].seq, messages[0].ord), (38, 11));
    assert_eq!((messages[1].seq, messages[1].ord), (41, 12));
    // 用户消息的来源块（平台/渠道/消息序号）是面板展示"谁在哪儿说的"的依据。
    let source = messages[0].source.as_ref().expect("user 条目带来源");
    assert_eq!(source.instance_name, "demo-bot");
    assert_eq!(source.platform, "demo");
    assert_eq!(source.channel, "group:20002");
    assert_eq!(source.group_name.as_deref(), Some("演示群"));
    assert_eq!(source.message_sequence, 7);
    // 推理正文随条目上线（面板折叠显示）。
    assert_eq!(
        messages[1].reasoning.as_deref(),
        Some(&["先调用 todo.list 看未完成项".to_string()][..])
    );
}

/// 增量帧必须**显式**写 `full:false`，并且只带缺口内的条目。
///
/// 面板分支是 `e.full === undefined ? 按 seq 猜 : e.full`：一旦 Core 把
/// `full:false` 省成"缺省不写"，前端就退回旧启发式，把增量当全量替换，
/// 表现为"列表被刷新、消息 key 全换、滚动回顶"。
#[test]
fn incremental_frame_is_explicitly_not_a_snapshot() {
    let event = decode_event(INCREMENTAL);
    let BackendEvent::TrunkTimeline {
        messages,
        seq,
        full,
        has_more,
        ..
    } = &event
    else {
        panic!("应为 TrunkTimeline：{event:?}");
    };
    assert!(!*full, "增量不是快照");
    assert!(!*has_more, "增量不涉及翻页");
    assert_eq!(*seq, 42);
    assert_eq!(messages.len(), 1, "只含 since_seq 之后的新条目");
    let entry = &messages[0];
    assert_eq!((entry.seq, entry.ord), (42, 13));
    assert_eq!(entry.kind, "tool");
    let tool = entry.tool.as_ref().expect("tool 条目带执行详情");
    assert_eq!(tool.tool_call_id, "call-demo-2");
    assert!(!tool.failed);
    assert!(!tool.timed_out, "timed_out 缺省为 false");
    assert_eq!(tool.output.as_deref(), Some("2 条未完成"));

    // 两个布尔字段都在线上出现（含 false），不是"缺省即省略"。
    let wire = json(&serialize_event(&event));
    let payload = &wire["payload"]["TrunkTimeline"];
    assert_eq!(payload["full"], serde_json::json!(false));
    assert_eq!(payload["has_more"], serde_json::json!(false));
    assert_eq!(payload["seq"], serde_json::json!(42));
    assert_eq!(wire["type"], serde_json::json!("event"));
}

/// 旧版/最小形态：只有 `messages` 时，游标与两个布尔都落到缺省值
/// （`0` / `false` / `false`），而不是解码失败。
#[test]
fn legacy_minimal_snapshot_decodes_to_zero_defaults() {
    let event = decode_event(LEGACY_MINIMAL);
    let BackendEvent::TrunkTimeline {
        messages,
        seq,
        team_id,
        full,
        has_more,
    } = &event
    else {
        panic!("应为 TrunkTimeline：{event:?}");
    };
    assert!(messages.is_empty());
    assert_eq!(*seq, 0);
    assert!(team_id.is_none());
    assert!(!*full);
    assert!(!*has_more);
}

/// 扇出层给同一条事件重打信封序号时，**不得**改写 payload 里的时间线游标。
///
/// `serialize_event_identified` 是把 `seq`/`entry` 加到帧顶层的唯一入口；
/// 它一旦误把顶层 `seq` 写进 payload（或反过来用 payload 的 `seq` 当扇出
/// 序号），面板的"跳号补拉"会与"增量游标"互相污染。
#[test]
fn re_enveloping_a_snapshot_does_not_rewrite_the_timeline_cursor() {
    let event = decode_event(FULL_SNAPSHOT);
    let reframed = serialize_event_identified(900, None, &event);
    let value = json(&reframed);
    assert_eq!(value["seq"], serde_json::json!(900), "扇出序号在顶层");
    assert!(
        value.get("entry").is_none(),
        "无条目身份时不写该键（TrunkTimeline 不是条目级事件）"
    );
    assert_eq!(
        value["payload"]["TrunkTimeline"]["seq"],
        serde_json::json!(41),
        "payload 里的游标不受扇出序号影响"
    );

    // 回读：顶层多余字段被忽略，事件仍是同一条、游标不变。
    let decoded = decode_event(&reframed);
    let BackendEvent::TrunkTimeline { seq, full, .. } = &decoded else {
        panic!("应为 TrunkTimeline：{decoded:?}");
    };
    assert_eq!((*seq, *full), (41, true));

    // 同一条事件再发一次（扇出序号推进）→ payload 完全一致：游标不是"发送次数"。
    let again = serialize_event_identified(901, None, &event);
    assert_eq!(
        json(&again)["payload"],
        value["payload"],
        "重打信封不改变载荷"
    );
}

/// 同步游标（`since_seq`）与分页锚点（`before_ord`）是**不同字段、不同语义**。
///
/// 为什么不用同一个：时间线的 `seq` 会因运行中工具条目就地更新而前进（位置
/// 不变、序号变大，实测 1024 条里有几十处非单调），按它翻页会过早截断并漏
/// 条目；分页只能用 push 时分配、此后不变的 `ord`。把两者写成同一个字段名，
/// serde 会把缺的那个当缺省 `0`——静默定位到时间线起点（重复拉全量）。
#[test]
fn sync_cursor_and_paging_anchor_are_different_fields() {
    let sync = decode_command(REQUEST_TIMELINE_INCREMENTAL);
    assert_eq!(
        sync,
        BackendCommand::RequestTrunkTimeline {
            team_id: Some("persona-demo".into()),
            since_seq: 38,
            limit: Some(200),
        }
    );

    let paging = decode_command(REQUEST_EARLIER);
    assert_eq!(
        paging,
        BackendCommand::RequestTimelineEarlier {
            team_id: Some("persona-demo".into()),
            before_ord: 11,
            limit: Some(50),
        }
    );

    // 线格式上互不出现对方的字段名。
    let sync_wire = json(&serialize_command(&sync));
    let sync_body = &sync_wire["payload"]["RequestTrunkTimeline"];
    assert_eq!(sync_body["since_seq"], serde_json::json!(38));
    assert!(sync_body.get("before_ord").is_none());

    let paging_wire = json(&serialize_command(&paging));
    let paging_body = &paging_wire["payload"]["RequestTimelineEarlier"];
    assert_eq!(paging_body["before_ord"], serde_json::json!(11));
    assert!(paging_body.get("since_seq").is_none(), "翻页不带同步游标");
}

/// 翻页响应（`TimelineEarlier`）**没有**同步游标字段：前端把这一页前插到
/// 已加载列表头部，同步游标仍停在最新进度。给它加一个 `seq`（或让面板按它
/// 更新游标）会让后续增量直接跳过整段历史。
#[test]
fn paging_response_never_carries_the_sync_cursor() {
    let event = BackendEvent::TimelineEarlier {
        team_id: Some("persona-demo".into()),
        messages: vec![TimelineMessage::user(
            "更早的一条（虚构）",
            "demo:group:20002:10001",
            1735689500,
            None,
            vec![],
        )],
        has_more: false,
    };
    let wired = serialize_event(&event);
    let value = json(&wired);
    let payload = &value["payload"]["TimelineEarlier"];
    assert!(
        payload.get("seq").is_none(),
        "翻页响应不带同步游标（前端前插，不动已缓存游标）"
    );
    assert_eq!(payload["has_more"], serde_json::json!(false));
    assert_eq!(payload["team_id"], serde_json::json!("persona-demo"));

    let decoded = decode_event(&wired);
    let BackendEvent::TimelineEarlier {
        messages, has_more, ..
    } = &decoded
    else {
        panic!("应为 TimelineEarlier：{decoded:?}");
    };
    assert!(!*has_more);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "更早的一条（虚构）");
}

/// 通用模块调用/应答：`module` + `method` 是配对键，必须逐字回显；
/// 成功回执省略 `error`，失败回执把 `data` 显式写成 `null`（面板类型里
/// `data` 是必填字段）。
///
/// 为什么值得一条夹具：宿主对 `method` / `payload` 完全不解释，唯一能让
/// 前端把应答配回请求的就是这两个回显字段——改名不会有编译错误，只会让
/// 并发调用互相串结果。
#[test]
fn generic_module_call_and_result_pair_on_module_and_method() {
    let command = decode_command(INVOKE_OP);
    let BackendCommand::InvokeModuleOp {
        module,
        method,
        payload,
    } = &command
    else {
        panic!("应为 InvokeModuleOp：{command:?}");
    };
    assert_eq!(module, "vendor.demo.todo");
    assert_eq!(method, "todo.list");
    assert_eq!(payload["scope"], serde_json::json!("today"));

    // ── 成功回执 ──
    let ok_event = decode_event(INVOKE_RESULT_OK);
    let BackendEvent::ModuleInvokeResult {
        module: ok_module,
        method: ok_method,
        ok,
        data,
        error,
    } = &ok_event
    else {
        panic!("应为 ModuleInvokeResult：{ok_event:?}");
    };
    assert_eq!(
        (ok_module.as_str(), ok_method.as_str()),
        (module.as_str(), method.as_str()),
        "应答必须回显请求的 module/method（并发调用的配对键）"
    );
    assert!(*ok, "fixture 是成功回执");
    assert_eq!(data["items"][0]["id"], serde_json::json!("t-1"));
    assert_eq!(data["total"], serde_json::json!(1));
    assert!(error.is_none());

    let ok_wire_value = json(&serialize_event(&ok_event));
    let ok_wire = &ok_wire_value["payload"]["ModuleInvokeResult"];
    assert!(
        ok_wire.get("error").is_none(),
        "成功回执省略 error 键（空值不上线）"
    );
    assert!(ok_wire.get("data").is_some(), "data 恒在（面板类型必填）");

    // ── 失败回执 ──
    let err_event = decode_event(INVOKE_RESULT_ERR);
    let BackendEvent::ModuleInvokeResult {
        ok, data, error, ..
    } = &err_event
    else {
        panic!("应为 ModuleInvokeResult：{err_event:?}");
    };
    assert!(!*ok);
    assert_eq!(*data, serde_json::Value::Null, "失败时 data 缺省为 null");
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("todo.create")),
        "失败原因必须点名方法：{error:?}"
    );

    let err_wire_value = json(&serialize_event(&err_event));
    let err_wire = &err_wire_value["payload"]["ModuleInvokeResult"];
    assert_eq!(
        err_wire["data"],
        serde_json::Value::Null,
        "data:null 显式上线，不是省键"
    );
    assert!(err_wire.get("error").is_some());
}
