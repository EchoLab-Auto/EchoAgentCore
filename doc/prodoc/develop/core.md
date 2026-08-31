---
id: core
title: "Core 框架"
order: 1
parent: index
x: 400
y: 40
group: 架构
link: [
    "panel | Panel 前端",
    "protocol | 协议与数据流",
    "agents | 多 Agent 与会话",
    "plugins | 插件系统",
    "deploy | 部署与自更新",
]
---

# Core 框架

Core（EchoAgentCore）是 Agent 后端核心服务，Rust 实现。组合根在 `source/core/src/main.rs`，核心库为 `source/backend/echo-agent`，协议定义在 `source/protocol/echo-protocol`。

## 进程结构

- `echo-agent-core-bin` 是唯一二进制，由 systemd 用户服务运行
- 组合根负责：加载配置、构建 LLM provider、装配工具/技能/插件、创建多 agent 监督器、启动 QQ 适配器与 management WS
- 非默认 agent 事件经 `event_bus` 镜像到主 agent 的 handle，Panel 单连接即可看到所有人格活动

## Agent 循环（turn loop）

- 每条消息注册一个**临时分支**（可取消），拿历史快照后进入 `process_message_inner`
- 系统提示词按块构建：基础提示词、技能清单、常驻/触发技能、后台编排说明、输入边界规则
- 循环迭代（`max_tool_iterations`，默认 1024）：发 LLM 请求 → 有工具调用则逐个执行并回填结果 → 直至产出最终回复或达上限
- 工具超时（`tool_timeout_secs`，当前 300s）只中止单个工具，超时以 notice 文本喂回模型，**不中断 loop**

## 工具调度

- `run_tool` 统一分派：编排工具（定时器/子代理/后台任务/自更新/sudo）内联处理；普通工具走注册表
- 参数非法 JSON / 缺少必需字段时返回**纠正性错误**（说明发了什么、应该发什么）
- 工具调用与结果都写入事件日志（事件溯源），重启后可完整重放

## 会话记忆

- 每个 agent 独立 `TrunkStore`：事件日志、显示时间线、会话持久化文件（`echo-sessions-{id}.json`）
- 模型上下文 = 事件日志的投影（token 预算裁剪）；显示时间线带来源/工具/推理元数据
- 时间线序号 `timeline_seq` 支持增量同步（`since_seq`），切换 agent 只传增量

## 配置要点

`~/.config/echo-agent-core/core.toml`：

- `[agent]`：provider/model/api profile、memory_limit_tokens、tool_timeout_secs、skills_dir
- `[agent.teams.*]`：多 agent 人格定义（name、description、system_prompt、能力白名单）
- `[adapters.qq]`：QQ 适配器（OneBot v11 反向 WS :3131）
- `[plugins.system_prompt]`：全局系统提示词
- `[agent.self_update]` / `[agent.sudo]`：自更新与 sudo 授权策略

## 常用运维命令

- `systemctl --user status echo-agent-core.service` 查看服务
- `systemctl --user restart echo-agent-core.service` 重启（原子操作）
- 更新走 `echo-agent-core-update.service`（oneshot，构建+替换+重启）
