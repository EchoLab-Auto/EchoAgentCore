---
id: tools
title: "工具系统"
group: 后端模块
x: 970
y: 1673
---

# 工具系统

## 注册与分发

- `Tool` trait 定义在 echo-defs（Service Definition 层）；`ToolRegistry`（echo-agent）按名注册，启停热切换（`disabled_tools` 持久化于 `[agent]`）
- 注册是可逆副作用：`register_reversible` 返回 `Disposer`，插件卸载即撤销
- `run_tool` 统一分派：编排工具（定时器/子代理/后台任务/自更新/sudo）由 agent 内联处理；普通工具走注册表

## 参数预检

- 非法 JSON / 缺少必需字段时返回**纠正性错误**（说清"你发了什么、应该发什么"），让模型自我纠正而不是盲目重试

## 超时治理

- `Tool::timeout_hint`：工具从自己的参数自声明执行超时（如 `run_command` 的 `timeout_secs`，默认 120s、上限 300s）
- 外圈守卫 = `max(tool_timeout_secs（默认 120s）, hint + 15s)`（自声明硬上限 600s；`run_sudo` 特判为授权+执行双超时之和 + 30s）
- 超时只中止单个调用：`run_command` 的 `sh -c` 整组进程被 SIGKILL（不留孤儿）；结果以 notice 文本喂回模型，**不中断 turn**，模型可重试或带已有信息继续作答

## 事件与持久化

- `ToolCall` / `ToolResult` 都写入事件日志（事件溯源），重启后可完整重放
- 协议事件携带 `tool_call_id` + `timed_out`：前端精确配对（同名并行调用不再配错对），超时渲染为失败；显示时间线就地更新会重新投递完成态（见 [协议与数据流](./protocol.md)）
