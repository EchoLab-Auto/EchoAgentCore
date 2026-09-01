---
group: 架构决策
x: 1756
y: 89
link: ["adr-0003 | 0003 服务定位与事件总线", "adr-0004 | 0004 事件溯源会话存储", "adr-0005 | 0005 echo-loop 轮次执行器", "adr-0006 | 0006 LLM provider 拆分", "adr-0007 | 0007 平台能力接缝", "adr-0008 | 0008 命令分发拆分", "adr-0009 | 0009 输入标记集中化", "adr-0010 | 0010 治理与文档", "adr-0012 | 0012 sudo 人机授权", "adr-0013 | 0013 插件化核心架构", "adr-0014 | 0014 多 Agent 系统", "adr-0015 | 0015 优雅排空", "adr-0016 | 0016 Web 面板无状态中继", "adr-0017 | 0017 Panel 时序动画规范", "adr-0018 | 0018 插件化 Phase 2"]
---

# 架构决策（ADR）

本文档群记录 EchoAgentCore / EchoAgentPanel 的架构决策。每条记录回答四问：
**问题 / 决策 / 备选方案 / 后果**，与代码同 PR 提交（对齐 dsh Agent Notes 纪律）。

## 规则

- 每条 ADR 是独立文件，命名 `NNNN-短横线标题.md`，编号单调递增
- 描述已落地的现实（现在时），不写迁移计划与"should"
- 决策被推翻时写新 ADR 并交叉链接，不修改旧 ADR
- 状态：`proposed`（评审中）/ `accepted`（已采纳）/ `superseded`（被新 ADR 取代）
- 0016/0017 合并自 echo-agent-panel 仓库文档群（原编号 0001/0004，文内有出处注记）
- 已并入主文档、不再单独立卷：0001（仓库拆分与协议契约 → [协议与数据流](./protocol.md)）、0002（echo-defs 定义层 → [架构总览](./architecture.md)）、0011（QQ 管理员运行时设置 → [QQ 适配器门控](./adapter-qq-gating.md)）

## 索引

| ADR | 标题 | 状态 |
|---|---|---|
| [0003](0003-echo-context-service-locator-and-event-bus.md) | echo-context：服务定位、事件总线、可逆注册、作用域 | accepted |
| [0004](0004-event-sourced-session-store.md) | 事件溯源会话存储（echo-session） | accepted |
| [0005](0005-echo-loop-turn-runner.md) | echo-loop TurnRunner（默认 agent 驱动，可替换） | accepted |
| [0006](0006-llm-provider-split.md) | LLM provider 拆分（echo-llm-* Service Provider crates） | accepted |
| [0007](0007-chat-capability-seam.md) | 平台能力接缝（DeliveryPolicy 解耦 QQ） | accepted |
| [0008](0008-command-dispatch-split.md) | 命令分发拆分与 CommandRegistry | accepted |
| [0009](0009-input-marker-centralization.md) | 结构化输入标记集中化（input_marker） | accepted |
| [0010](0010-governance-and-docs.md) | 治理与文档（architecture.md + CI 门禁） | accepted |
| [0012](0012-sudo-human-in-the-loop.md) | 人机交互 sudo 授权（run_sudo） | accepted |
| [0013](0013-plugin-architecture.md) | 插件化核心架构（PluginManifest/PluginHost） | accepted |
| [0014](0014-multi-agent.md) | 多 Agent 人格系统（teams/persona 隔离） | accepted |
| [0015](0015-graceful-drain.md) | 优雅排空（自更新不打断当前回复） | accepted |
| [0016](0016-web-relay-architecture.md) | Web 面板 = 无状态字节级 WS 中继 + 静态托管 | accepted |
| [0017](0017-panel-agent-visual-timeline.md) | Panel 思考/工具/回答的时序与动画规范 | accepted |
| [0018](0018-plugin-phase2-mount-materialization.md) | 插件化 Phase 2：mount 实化（禁用/启用真实生效） | proposed |
