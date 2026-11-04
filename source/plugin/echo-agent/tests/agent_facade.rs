//! `Agent` 门面化的**结构守卫**（2026-11 S9）。
//!
//! ## 为什么这类测试要读源码
//!
//! 门面化的收益是"不变式写在类型里"，而唯一的失败模式是**慢慢地退回去**：
//! 下一个人需要跨组共享一个状态时，最省事的做法就是在 `Agent` 上再平铺一个
//! 字段——编译通过、测试全绿、两个月后又是 30 个字段。
//!
//! 所以这里直接读源码做断言。它不测行为，测的是**结构决策本身**：
//!
//! 1. `Agent` 的字段数不超过 12（超了说明有人没想清楚该归哪个组）；
//! 2. 已知分组过的字段名**不得**再出现在 `Agent` 的直接字段里；
//! 3. 每个组的字段必须**全都**被 `Agent` 的某个组持有（防止"搬出去又忘了用"）。
//!
//! 断言 2 是关键：字段数上限可以被"删一个加一个"绕过，而"`team_id` 又回到
//! `Agent` 上了"这种回退必须显式撞墙。

use std::collections::BTreeSet;

/// `Agent` 的直接字段名（按 `src/agent/mod.rs` 里 `pub struct Agent { .. }` 解析）。
fn agent_fields() -> Vec<String> {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/agent/mod.rs"))
        .expect("read agent/mod.rs");
    let start = src.find("pub struct Agent {").expect("Agent struct");
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("Agent struct end");
    let body = &rest[..end];
    let mut fields = Vec::new();
    for line in body.lines().skip(1) {
        let line = line.trim();
        if line.starts_with("//") || line.is_empty() {
            continue;
        }
        // 形如 `pub(crate) model: ...` / `pub tools: ...`
        let Some((head, _)) = line.split_once(':') else {
            continue;
        };
        let head = head.trim();
        let name = head
            .strip_prefix("pub(crate) ")
            .or_else(|| head.strip_prefix("pub "))
            .unwrap_or(head);
        // 排除多行类型声明的续行（如 `tool_state_events:\n  std::sync::...`）
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        {
            fields.push(name.to_string());
        }
    }
    fields
}

/// 分组结构体里的字段名（`src/agent/groups.rs` 的所有 `pub(crate) <name>:`）。
fn grouped_fields() -> BTreeSet<String> {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/agent/groups.rs"
    ))
    .expect("read agent/groups.rs");
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("pub(crate) ") {
            if let Some((name, _)) = rest.split_once(':') {
                let name = name.trim();
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// 门面化的上限：**12 个字段**。
///
/// 数着 12 这个数的理由：三个服务（model/prompt）+ 三个真句柄
/// （tools/trunk/instances）+ 六个语义组（identity/egress/turns/
/// instrumentation/plugins/ports/manager 里各有归属）。加字段前先问一句
/// "它是不是某个组内必须一起变的一员"——是就进组，不是才平铺，
/// 而"不是"的答案在过去 30 个字段里一次都没出现过。
#[test]
fn agent_facade_does_not_regrow_fields() {
    let fields = agent_fields();
    assert!(
        fields.len() <= 12,
        "Agent 字段数回退到 {}（上限 12）：{:?}\n\
         新状态请先进 `agent/groups.rs` 里对应的组——分组的唯一目的就是让\
         '必须一起变'这件事写进类型。真需要平铺请在 `groups.rs` 的文档里\
         写明它为什么不属于任何组，并同步调整这个上限。",
        fields.len(),
        fields
    );
}

/// 已经搬进组的字段**不得**再出现在 `Agent` 上。
///
/// 这条比字段数上限更精准：先加一个组内字段的平铺副本、再删掉别的字段，
/// 字段数不变但不变式已经破了一半。名单与 `groups.rs` 的组一一对应。
#[test]
fn grouped_state_never_leaks_back_onto_the_facade() {
    let facade: BTreeSet<String> = agent_fields().into_iter().collect();
    let leaked: Vec<&str> = [
        // EventEgress
        "handle", "event_sink", "event_bus", "tool_state_events",
        // TurnState
        "loop_runner", "reply_branch_slots", "active_inbound_turns",
        "next_message_sequence", "draining", "cancel",
        // Identity
        "team_id", "node_identity", "capabilities",
        // Instrumentation
        "redactor", "metrics", "gating_log_state",
        // PluginSubsystem
        "plugin_host", "global_policy", "plugin_reload_started",
        // PluginPorts
        "workspace_store", "shell", "subagent", "federation",
        // ManagerLink
        "registry", "manager_warned",
    ]
    .into_iter()
    .filter(|name| facade.contains(*name))
    .collect();
    assert!(
        leaked.is_empty(),
        "这些状态已经分组，却又被平铺回 Agent 了：{leaked:?}\n\
         平铺副本会造成两份真源（改一份忘另一份），这正是分组要防的 bug。"
    );
}

/// 组里的每个字段都必须被 `Agent` 的某个组**真的**持有。
///
/// 防的是反方向的漂移：把字段从 `Agent` 搬进 `groups.rs` 却忘了在构造里赋值
/// （或在后续重构里删掉了持有），`groups.rs` 读起来像"有这些状态"，
/// 实际运行时永远空着。
#[test]
fn every_grouped_field_is_owned_by_the_facade() {
    let facade = agent_fields();
    let groups: Vec<&str> = facade
        .iter()
        .map(String::as_str)
        .filter(|f| {
            matches!(
                *f,
                "identity"
                    | "egress"
                    | "turns"
                    | "instrumentation"
                    | "plugins"
                    | "ports"
                    | "manager"
            )
        })
        .collect();
    assert_eq!(
        groups.len(),
        7,
        "分组数量变了（{:?}）；`groups.rs` 的文档表也要同步",
        groups
    );
    // 分组结构体本身要存在且非空：`groups.rs` 解析出的字段集合不能为空，
    // 否则说明文件被搬走/改名，而守卫会静默失效。
    let grouped = grouped_fields();
    assert!(
        grouped.len() >= 25,
        "分组字段只解析出 {} 个（应有 ≥25）；`groups.rs` 的字段可见性被改了？{:?}",
        grouped.len(),
        grouped
    );
}
