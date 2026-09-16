---
id: core
title: "Core 后端"
group: 后端模块
x: 600
y: 1759
link: ["core-agents | 多 Agent 与会话 | r>l", "core-plugins | 插件化设计", "core-agent-loop | Agent 循环", "core-config-persistence | 配置持久化 | r>l"]
---

# Core 框架

Core（EchoAgentCore）是 Agent 后端核心服务，Rust 实现。组合根在 `source/core/src/main.rs`，核心库为 `source/backend/echo-agent`，协议定义在 `source/protocol/echo-protocol`。

## 进程结构

- `echo-agent-core-bin` 是唯一二进制，由 systemd 用户服务运行
- 组合根负责：加载配置、构建 LLM provider、装配工具/技能/插件、创建多 agent 监督器、启动 QQ 适配器与 management WS
- 所有人格事件直投进程级事件汇聚点（`EventSink`），Panel 单连接即可看到全部活动；进程级职责由核心服务代理（非人格）承担（无「主智能体」，2026-09-13）



## 会话记忆

- 每个 agent 独立 `TrunkStore`：事件日志、显示时间线、会话持久化文件（`echo-sessions-{id}.json`）
- 模型上下文 = 事件日志的投影（token 预算裁剪）；显示时间线带来源/工具/推理元数据
- 时间线序号 `timeline_seq` 支持增量同步（`since_seq`），切换 agent 只传增量
- 本地对话支持**工作区通道**（`local:workspace:<id>:local_user`）：激活工作区 = 进入项目对话——本地上下文切换 + 提示词注入（2026-09-14，见 [多 Agent 与会话](./core-agents.md)§工作区会话与项目通道）

## 多模态输入（图片）

约定与主流多模态 API 一���：**图片只走独立的 image 内容块**（Anthropic `image` block / OpenAI `image_url` part），文本块里只留占位符。

- 入口：QQ 适配器把远程图下载为 data URI（`echo-adapter-qq`，单图 ≤10MB 解码字节）、Panel 把附图压成 data URL（最长边 1600px，超 1.5M 字符则转 JPEG 并逐级缩小）——都写进 hook JSON 的 `"images"` 字段
- 事件日志保存原始内容（`images` 字段 + 原文），**投影时才压**：`echo_defs::media::compact_embedded_media` 把文本里出现过的附带 data URI 换成 `[图片#n]`（序号与 image 块一致），其余长内联 data URI 换 `[图片数据已省略]`；`echo-session` 的 `user_message`/`tool_result_message` 以及两个 provider 各压一次（幂等）
- 为什么必须压：混在文本里的 base64 按**文本** token 计费。实测 DeepSeek `/anthropic`：4 万 base64 字符 ≈ 2.8 万输入 token，而同一张图走 image 块只要约 200 token——1.3MB 截图混在文本里就是约 125 万 token，足以单条打爆 1M 窗口（2026-09-10 线上 400）
- 估算器同源修正（`echo-defs::token`）：base64 连续段按 1 token/字符（原 1/3 少算 >2×）、图片按 `编码字节/250` 计（85–8192 封顶）、`tool_calls` 参数计入消息成本；`truncate_text_to_tokens` 改为按同一估算器二分切点，避免"截断后仍超预算"
- 单图入参兜底 `MAX_INPUT_IMAGE_CHARS = 16MB`（对齐各入口内嵌上限，只拦异常负载；图片体积不影响 token）

## 配置要点

`~/.config/echo-agent-core/core.toml`：

- `[agent]`：provider/model/api profile 池（api_profiles + 全局默认 active_api）、max_tokens 输出预算、memory_limit_tokens、tool_timeout_secs、skills_dir
- provider 取值：`openai` / `deepseek` / `third-party`（OpenAI 兼容，base_url 以 `/anthropic` 结尾时自动走 Messages 协议）、`anthropic` / `claude`、`kimi`（Kimi Code 订阅：Anthropic 兼容端点 `https://api.kimi.com/coding`，`x-api-key` = Kimi Code Console 创建，推理档位 `output_config.effort` low/high/max）、`ollama`
- `[agent.teams.*]`：多 agent 人格定义（name、description、system_prompt、能力白名单、api_profile 供应商引用）
- `[adapters.qq]`：QQ 适配器（OneBot v11 反向 WS :3131）
- `[plugins.system_prompt]`：全局系统提示词
- `[agent.self_update]` / `[agent.sudo]`：自更新与 sudo 授权策略

## 常用运维命令

- `systemctl --user status echo-agent-core.service` 查看服务
- `systemctl --user restart echo-agent-core.service` 重启（原子操作）
- 更新走 `echo-agent-core-update.service`（oneshot，构建+替换+重启）
