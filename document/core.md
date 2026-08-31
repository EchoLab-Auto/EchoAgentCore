---
id: core
title: "Core 后端"
group: 后端模块
link: ["core-agent-loop | Agent 循环与工具", "core-agents | 多 Agent 与会话", "core-plugins | 插件系统", "protocol | 协议与数据流", "ops-deploy | 部署与自更新"]
x: 48
y: 1728
---

# Core 框架

Core（EchoAgentCore）是 Agent 后端核心服务，Rust 实现。组合根在 `source/core/src/main.rs`，核心库为 `source/backend/echo-agent`，协议定义在 `source/protocol/echo-protocol`。

## 进程结构

- `echo-agent-core-bin` 是唯一二进制，由 systemd 用户服务运行
- 组合根负责：加载配置、构建 LLM provider、装配工具/技能/插件、创建多 agent 监督器、启动 QQ 适配器与 management WS
- 非默认 agent 事件经 `event_bus` 镜像到主 agent 的 handle，Panel 单连接即可看到所有人格活动



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
