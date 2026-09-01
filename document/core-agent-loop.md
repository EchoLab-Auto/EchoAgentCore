---
id: agent-loop
title: "Agent 循环"
group: 后端模块
x: 970
y: 1241
---

# Agent 循环

## Turn 循环

- 每条消息注册一个**临时分支**（可取消），拿历史快照后进入 `process_message_inner`
- 系统提示词按块构建：基础提示词、技能清单、常驻/触发技能、后台编排说明、输入边界规则
- 循环迭代（`max_tool_iterations`，默认 1024）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 达上限未完成时报错收尾；工具的超时/失败不中断 loop（见 [工具系统](./core-tools.md)）

## 取消与中断

- 临时分支持有 `CancellationToken`：用户取消（`CancelRequestedWork`）时正在执行的工具被外层 select 中止，turn 以取消收尾
- 优雅排空（ADR-0015）：收到 SIGTERM 后排空模式拒绝新消息、等待活跃 turn 完成（最多 120s）
- 重启后 turn 不恢复；事件日志的悬空调用在加载时由 `repair_tool_pairing` 补合成结果，显示时间线的悬空 running 条目标注为"已中断"
