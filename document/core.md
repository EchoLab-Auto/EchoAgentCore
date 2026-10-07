---
id: core
title: "Core 后端"
group: 核心设施
x: 600
y: 1512
link: ["core-agents | 多 Agent 与会话 | r>l", "core-agent-loop | Agent 循环", "core-memory | 会话记忆", "core-multimodal | 多模态输入", "core-plugins | 插件化设计", "core-config-persistence | 配置持久化"]
---

# Core 框架

Core（EchoAgentCore）是 Agent 后端核心服务，Rust 实现。组合根在 `source/core/src/main.rs`，核心库为 `source/backend/echo-agent`（框架核心在 `agent/`，各插件实现按包分目录在 `packages/`，见 [插件化设计](./core-plugins.md)），协议定义在 `source/protocol/echo-protocol`。

## 进程结构

- Cargo 二进制名为 `echo-agent-core`（`source/core/Cargo.toml`），由 systemd 用户服务运行；`install.sh` 安装后落盘为 `$LIBEXEC_DIR/echo-agent-core-bin`（libexec 文件名，非 Cargo bin 名）
- 组合根负责：加载配置、构建 LLM provider、装配工具/技能/插件、创建多 agent 监督器、启动 QQ 适配器与 management WS
- 所有人格事件直投进程级事件汇聚点（`EventSink`），Panel 单连接即可看到全部活动；进程级职责由核心服务代理（非人格）承担（无「主智能体」，2026-09-13）

## 延伸主题（Agent 运行时）

原混在本页的两个 **agent 运行时**主题已拆分为专门文档（分类见 [文档总览](./index.md)§文档分类）：

- [会话记忆](./core-memory.md)：事件溯源日志与模型上下文投影、显示时间线、压缩与归档
- [多模态输入](./core-multimodal.md)：媒体库落盘引用、模型侧还原、文本/token 卫生

## 配置要点

`~/.config/echo-agent-core/core.toml`：

- `[agent]`：provider/model/api profile 池（api_profiles + 全局默认 active_api）、max_tokens 输出预算、memory_limit_tokens、tool_timeout_secs、skills_dir
- provider 取值：`openai` / `deepseek` / `third-party`（OpenAI 兼容，base_url 以 `/anthropic` 结尾时自动走 Messages 协议）、`anthropic` / `claude`、`kimi`（Kimi Code 订阅：Anthropic 兼容端点 `https://api.kimi.com/coding`，`x-api-key` = Kimi Code Console 创建，推理档位 `output_config.effort` low/high/max）、`ollama`
- `[agent.teams.*]`：多 agent 人格定义（name、description、system_prompt、能力白名单、api_profile 供应商引用）
- `[adapters.qq]`：QQ 适配器（OneBot v11 反向 WS :3131）
- `[plugins.system_prompt]`：全局系统提示词

## 常用运维命令

- `systemctl --user status echo-agent-core.service` 查看服务
- `systemctl --user restart echo-agent-core.service` 重启（原子操作）
- 更新走 `echo-agent-core-update.service`（oneshot，构建+替换+重启）
