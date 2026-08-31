---
id: agent-loop
title: "Agent 循环与工具"
group: 后端模块
link: ["adr-0015 | 优雅排空", "core | Core 框架"]
x: 332
y: 552
---

# Agent 循环与工具

## Agent 循环（turn loop）

- 每条消息注册一个**临时分支**（可取消），拿历史快照后进入 `process_message_inner`
- 系统提示词按块构建：基础提示词、技能清单、常驻/触发技能、后台编排说明、输入边界规则
- 循环迭代（`max_tool_iterations`，默认 1024）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 外圈工具超时 = `max(tool_timeout_secs, 工具自声明超时 + 15s)`（`tool_timeout_secs` 默认 120s；`run_command` 经 `Tool::timeout_hint` 自声明最长 300s，`run_sudo` 特判为授权+执行双超时之和）
- 超时只中止单个工具调用（`sh -c` 整组进程被终止，不留孤儿），结果以 notice 文本喂回模型，**不中断 loop**

## 工具调度

- `run_tool` 统一分派：编排工具（定时器/子代理/后台任务/自更新/sudo）内联处理；普通工具走注册表
- 参数非法 JSON / 缺少必需字段时返回**纠正性错误**（说明发了什么、应该发什么）
- 工具调用与结果都写入事件日志（事件溯源），重启后可完整重放
