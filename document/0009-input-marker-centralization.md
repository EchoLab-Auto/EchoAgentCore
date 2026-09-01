---
id: adr-0009
title: "ADR-0009 输入标记集中化"
group: 架构决策
x: 1614
y: 929
---
# ADR-0009: 结构化输入标记集中化(input_marker)

状态: accepted

## 问题

`<qq_message_hook>`/`<backend_message_hook>`/`<timer_event>`/
`<background_task_event>` 四个结构化输入标记在 6 个文件中散落 37 处字面量:
adapter_bridge 构造 hook、agent/mod.rs 判定来源与解析 sequence、timeline.rs
解析 timer 摘要、session.rs 迁移、orchestration 构造定时任务、测试断言。同一
约定多处重复定义,改一处易漏其余。

## 决策

新建 `input_marker.rs` 作为这些标记的**单一事实来源**:

- 构造:`wrap_hook(platform, json)` / `wrap_timer` / `wrap_background`
  (接受 `impl Display`,serde Value 直接可用)。
- 判定:`STRUCTURED_INPUT_MARKERS` 常量表 + `is_structured_input`。
- 解析:`structured_message_sequence`(从标记输入提取 message_sequence,
  限制在标记开头,防普通文本误读)。

替换 6 个文件的散落字面量:`adapter_bridge` 的 hook 编码、agent/mod.rs 的
构造/判定/常量表/解析、timeline.rs 的 timer 解析。`structured_message_sequence`
在 agent/mod.rs 保留为薄委托。

## 备选方案

- **完整协议结构化(Phase 5.5 目标)**:把来源判定从 content 字符串改为
  `BackendEvent::MessageReceived` 结构化字段,是跨仓库 echo-protocol wire
  变更 + Core 循环 + Panel 解析的捆绑改造。先集中化标记(本 ADR)消除散落,
  为结构化铺路,风险递进。
- **维持散落字面量**:37 处重复,改一处易漏。

## 后果

- 标记约定单一定义:改格式只改 `input_marker.rs`。
- 行为零变化:469 项测试全绿(新增 4 个 input_marker 测试);
  hook 编码、sequence 解析、来源判定语义原样保留。
- 后续:协议结构化(MessageReceived 携带结构化 origin)在此模块基础上演进。
