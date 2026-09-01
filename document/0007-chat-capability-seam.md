---
id: adr-0007
title: "ADR-0007 平台能力接缝"
group: 架构决策
x: 2076
y: 720
---
# ADR-0007: 平台能力接缝(DeliveryPolicy 解耦 QQ)

状态: accepted

## 问题

Agent 核心循环 `process_message_inner` 直接解析 QQ 语义:`DeliveryPlan`
从 `<qq_message_hook>` 载荷解析目标、`validate_delivery_call` 硬编码
`send_private_msg`/`send_group_msg`/`send_backend_message` 三个工具名、
reminder 文本拼接平台工具名。新增平台(Telegram 等)必须修改核心循环与
reminder 逻辑;平台词泄漏进 agent 框架。

## 决策

新建 `echo-chat-capability` crate,定义**平台能力接缝的 Service Definition
角色**:

- **`DeliveryTarget`**:平台无关的交付目标(`Direct{user_id}` /
  `Group{group_id}` / `Backend{session_id}`),不含平台工具名。
- **`DeliveryPolicy` trait**:`plan_from_input(content) -> Option<Vec<DeliveryTarget>>`
  (从输入解析所需交付)、`validate_delivery_call(plan, delivered, call) ->
  Result<Option<String>, String>`(校验工具调用满足一个目标,key 去重)、
  `delivery_reminder(pending) -> String`(模型可见的纠正文案)。
- `target_key` 辅助(去重键)。

`echo-agent` 内的 `QqDeliveryPolicy` 实现该 trait(解析 QQ hook 与
background-task deliveries、校验 send_* 调用、生成 reminder);`DeliveryPlan`
改为持有 `Vec<DeliveryTarget>` 并委托 policy;核心循环通过 trait 驱动,不再
直接引用 QQ 工具名。旧 `OutgoingTarget` 枚举与独立 `validate_delivery_call`
函数删除。

## 备选方案

- **不抽接缝,维持 QQ 内嵌**:新增平台改核心循环与 reminder。
- **trait 放 echo-defs**:定义层应保持零实现;`echo-chat-capability` 是
  专门的平台能力定义 crate,与 `ChatAdapter`(echo-defs)互补——后者管生命
  周期/收发,前者管交付策略。

## 后果

- 核心循环依赖 `DeliveryPolicy` trait,不 import 平台工具名:新增平台 =
  一个实现该 trait 的 provider(未来经 `ctx` 注册解析),循环不动。
- `QqDeliveryPolicy` 现为 agent 内实现(QQ 语义归属待 4.5 后半移到
  `echo-qq` provider crate);`apply_command` 的 QQ 分支与 `qq_tools.rs` 的
  工具注册仍待迁移(后续轮次)。
- 行为零变化:463 项测试全绿;reminder 文案与校验语义原样保留。
