# Decision Records (ADR)

本目录记录 EchoAgentCore / EchoAgentPanel 的架构决策。每条记录回答四问：
**问题 / 决策 / 备选方案 / 后果**，与代码同 PR 提交（对齐 dsh Agent Notes 纪律）。

规则：

- 每条 ADR 是独立文件，命名 `NNNN-短横线标题.md`，编号单调递增。
- 描述已落地的现实（现在时），不写迁移计划与"should"。
- 决策被推翻时写新 ADR 并交叉链接，不修改旧 ADR。
- 状态：`proposed`（评审中）/ `accepted`（已采纳）/ `superseded`（被新 ADR 取代）。

| ADR | 标题 | 状态 |
|---|---|---|
| [0001](0001-repository-split-and-protocol-contract.md) | 仓库拆分与 echo-protocol 跨仓库契约 | accepted |
| [0002](0002-echo-defs-service-definition-layer.md) | echo-defs 定义层抽取(Service Definition 层) | accepted |
| [0003](0003-echo-context-service-locator-and-event-bus.md) | echo-context:服务定位、事件总线、可逆注册、作用域 | accepted |
| [0004](0004-event-sourced-session-store.md) | 事件溯源会话存储(echo-session) | accepted |
| [0005](0005-echo-loop-turn-runner.md) | echo-loop TurnRunner(默认 agent 驱动,可替换) | accepted |
| [0006](0006-llm-provider-split.md) | LLM provider 拆分(echo-llm-* Service Provider crates) | accepted |
| [0007](0007-chat-capability-seam.md) | 平台能力接缝(DeliveryPolicy 解耦 QQ) | accepted |
| [0008](0008-command-dispatch-split.md) | 命令分发拆分与 CommandRegistry | accepted |
| [0009](0009-input-marker-centralization.md) | 结构化输入标记集中化(input_marker) | accepted |
| [0010](0010-governance-and-docs.md) | 治理与文档(architecture.md + CI 门禁) | accepted |
| [0011](0011-qq-owner-runtime-setting.md) | QQ 管理员运行时设置(SetQqOwner) | accepted |
| [0012](0012-sudo-human-in-the-loop.md) | 人机交互 sudo 授权(run_sudo) | accepted |
