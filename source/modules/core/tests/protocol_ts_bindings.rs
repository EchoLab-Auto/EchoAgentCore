//! Guard: TypeScript 绑定必须**可生成且形状正确**。
//!
//! ## 背景（2026-11 根因重构）
//!
//! Panel 仓此前手抄了一份管理面协议（`web/src/protocol.ts`，1116 行、
//! 55 个事件变体、73 个命令变体）。三种漂移形态**都不报错**：
//!
//! | 漂移 | 症状 |
//! |---|---|
//! | Core 加事件、Panel 没加 | `switch` 落到 `default`，静默忽略 |
//! | Core 改字段名 | `JSON.parse` 得到 `undefined`，渲染成空 |
//! | Core 改语义（文案/阈值） | 同一份数据刷新前后不一致 |
//!
//! 第三种真实发生过（定时消息文案，2026-11 已修）。根治手段是让 Panel 的
//! 类型成为**生成物**：Core 用 `ts-rs` 从 serde 类型导出，Panel 只消费。
//!
//! 本测试守三件事：
//! 1. 生成能力本身没退化（`declarations()` 出得出东西）；
//! 2. 关键 serde 形状被如实反映（tag+content 信封、内部标记 enum、
//!    单元变体、untagged）；
//! 3. 手写的导出清单**没有漏掉**根类型（漏了 Panel 会缺声明）。
//!
//! 第 3 条是这里最容易被破坏的：`ts-rs` 不做传递闭包收集，清单是人维护的。

#![cfg(feature = "ts-bindings")]

/// 生成结果里的信封形状：`{"type":"command|event","payload":…}`。
///
/// 这是 Panel 的 `sendCommand` / `parseEvent` 依赖的**第一层**约定。ts-rs 对
/// `#[serde(tag, content)]` 的支持一旦退化，所有帧的编解码都会错位，而
/// Rust 侧的序列化测试照旧通过——所以这条必须在 TS 侧钉住。
#[test]
fn ws_envelope_uses_tag_and_content() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    assert!(
        ts.contains(r#"export type WsMessage = { "type": "command", "payload": BackendCommand }"#),
        "WsMessage 的信封形状变了：{ts}"
    );
    assert!(
        ts.contains(r#"{ "type": "event", "payload": BackendEvent }"#),
        "WsMessage 缺少 event 分支"
    );
}

/// 内部标记枚举：变体名作为单键对象（`{"SendMessage": {...}}`）。
#[test]
fn commands_and_events_are_internally_tagged() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    assert!(ts.contains(r#"{ "SendMessage":"#), "BackendCommand 的构造器变了");
    assert!(ts.contains(r#"{ "MessageReceived":"#), "BackendEvent 的构造器变了");
    // 单元变体必须是字符串字面量，不能变成 `{ "RequestSkillsList": null }`
    assert!(
        ts.contains(r#""RequestSkillsList""#),
        "单元变体（无载荷命令）应当生成为字符串字面量"
    );
}

/// `untagged` 枚举（`WorkspaceDirectory`）要生成为 union 而不是带标签对象。
#[test]
fn untagged_enum_stays_a_plain_union() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    let line = ts
        .lines()
        .find(|line| line.starts_with("export type WorkspaceDirectory"))
        .expect("WorkspaceDirectory 未导出");
    assert!(
        line.contains("| { path:"),
        "untagged 应当生成为裸 union（本机目录 = string / 跨机 = 对象）：{line}"
    );
    assert!(!line.contains(r#""Local""#), "untagged 不该出现变体标签");
}

/// 大整数按 JSON 实际形态生成为 `number`，不是 `bigint`。
///
/// ts-rs 默认 `bigint`（怕超 2^53），但这些字段走 JSON——`JSON.parse` 只会
/// 产出 `number`。标成 `bigint` 会逼调用方写无意义的转换，而且类型与运行期
/// 值不符。
#[test]
fn large_ints_map_to_number_because_wire_is_json() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    assert!(!ts.contains("bigint"), "生成结果里出现了 bigint（应为 number）");
}

/// 根类型必须都在导出清单里。
///
/// 清单是手写的（ts-rs 不做传递闭包），漏一个的后果是生成文件缺声明 →
/// Panel 的 `tsc` 报错。这条测试让"漏了"在 Core 的 CI 里就暴露。
#[test]
fn every_root_type_is_exported() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    for name in [
        "WsMessage",
        "BackendCommand",
        "BackendEvent",
        "SequencedEvent",
        "Emit",
        // 载荷结构抽查（覆盖命令侧、事件侧、联邦侧、定义层）
        "ModuleInfo",
        "TimelineMessage",
        "WorkspaceSessionInfo",
        "ThinkingMode",
        "FederationPeerInfo",
    ] {
        assert!(
            ts.contains(&format!("export type {name}")),
            "导出清单漏了 {name}"
        );
    }
}

/// 生成结果必须以文件头开头（Panel 侧靠它确认"这是生成物"）。
#[test]
fn header_marks_the_file_as_generated() {
    let ts = agent_module_management::protocol::ts_bindings::declarations();
    assert!(ts.starts_with("// @generated"), "缺少 @generated 头：{ts:.80}");
}
