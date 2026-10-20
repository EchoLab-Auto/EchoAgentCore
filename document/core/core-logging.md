---
id: core-logging
title: "日志与可观测性"
group: 框架
x: 960
y: 1440
---

# 日志与可观测性

> **定位**：本文描述 **Core 的日志出口**——级别与格式、轮转文件、级别热更新、panic 兜底，以及「哪类信息该走日志、哪类该走事件」。读者：需要排障、接采集、或新增日志打点的开发者。
> 相关：[Core 后端](./core.md)（配置入口）、[会话记忆](./core-memory.md)（事件日志是另一条独立通道）、[敏感信息隔离](./security-redaction-design.md)（日志脱敏边界）、[Panel 日志视图](../panel/panel-settings-logs.md)（查看与过滤）。

## 两条通道，别混用

| 通道 | 载体 | 用途 | 消费者 |
|---|---|---|---|
| **运行日志** | `tracing` → journald / 轮转文件 | 排障：耗时、失败原因、状态翻转 | 人、采集器 |
| **事件日志** | `echo_session::EventLog` → 会话 JSON | 事实：消息、工具调用、结果 | 模型上下文、Panel 时间线 |

判据：**能重建对话的进事件，诊断过程进运行日志**。工具参数与结果属于事件（要回放给模型）；「工具跑了 1.3s」属于运行日志。二者都受脱敏闸门约束。

## 出口与格式

`[logging]` 段决定出口（`config/echo-agent-core.toml`）：

| 键 | 含义 |
|---|---|
| `level` | 过滤级别（`trace`/`debug`/`info`/`warn`/`error`）；`RUST_LOG` 环境变量优先 |
| `format` | `text`（人读）或 `json`（采集；事件自动携带当前 span） |
| `log_file` | 空 = stdout（systemd 下进 journald）；非空 = 轮转文件 |
| `max_file_size_mb` / `max_files` | 单文件上限、保留文件总数（含当前文件） |
| `rotate_by_day` | 跨天首条日志自动开新文件 |
| `compress` | 归档 gzip（失败降级保留明文） |

**缺省（`log_file` 为空）就是推荐配置**：journald 自带轮转与去重，`journalctl --user -u echo-agent-core` 直接查。

**ANSI 只在终端上开**。journald / 重定向到文件时若仍着色，结构化字段会被转义序列拆散（`grep session=` 会零命中）——判定走 `IsTerminal`，不靠配置。

## 打点约定

字段用结构化键值（不要拼进文案）：

```rust
tracing::info!(
    session = %session_id,
    branch_id = %branch_id,
    tool = %call.name,
    elapsed_ms,
    "tool call finished"
);
```

- **必备关联键**：`session`（会话 id）、`branch_id`（turn 分支）。工具再加 `tool`/`tool_call_id`，子代理再加 `task_id`/`parent_branch_id`。
- **级别纪律**：`error` = 需要人介入的失败；`warn` = 自动恢复但值得看见（重试、降级）；`info` = 状态翻转与耗时（默认级别可见）；`debug` = 高频幂等快照（每次重连的全量广播、每 5s 的门控重算都该在这里）。
- **持久失败要节流**：长期失联的对端、反复重试的链路，按次数或时间间隔打印（首次 + 每 N 次），否则 7 天窗口会被同一条 WARN 灌满。
- **状态变化才打 info**：幂等的重复计算（每轮能力重算、每次连接重发的快照）降 `debug`，否则真实开关动作被淹没。
- **不得输出秘密**：密钥、token、消息正文一律不进日志；工具结果走 `redact_text` 后再记录（见[敏感信息隔离](./security-redaction-design.md)）。

## span 与关联

三层 span 贯穿一次请求：

| span | 创建位置 | 携带 |
|---|---|---|
| `agent_turn` | `Agent::process_recorded_message_with_progress` | `session`、`branch_id` |
| `tool_call` | `Agent::run_tool` | `tool`、`tool_call_id`、`session`、`branch_id` |
| `subagent_task` | `Agent::spawn_subagent_execution` | `task_id`、`parent_branch_id`、`node`、`profile` |

`json` 格式下每个事件带 `current_span`（`with_current_span`），因此**不必在每条事件里手抄 session**——采集侧可直接按 span 归组。`text` 格式不打印 span，所以业务日志仍需显式带关联键（两者并行不冲突）。

`subagent_task` 是**独立 span**（跨 `tokio::spawn` 不继承父上下文），父子关系靠 `parent_branch_id` 字段显式串联；判定卡死看「有 `subagent started` 无 `subagent finished`」。

## 轮转与归档

仅在 `log_file` 非空时生效：

- **触发**：单文件将超过 `max_file_size_mb`，或跨天首条日志（`rotate_by_day`），二者取先到；
- **归档名**：`<log_file>.<YYYYMMDD-HHMMSS>[-<seq>][.gz]`（本地时间；同秒多次轮转加序号）；
- **保留**：只按时间保留最新 `max_files - 1` 个归档，与当前文件合计 `max_files`；
- **超长记录**：单条记录大于单文件上限时按剩余空间切开、跨文件续写——不会把文件顶到上限以上；
- **写入**：每条事件一次加锁 + 一次 `write(2)`（fmt 层一条事件会多次 `write`，写入器先攒后一次落盘）。

已知边界：轮转与压缩是同步 IO，发生在写日志的线程上（触发频率很低，量级是每个文件一次）；需要极致吞吐的场景请走 stdout + journald。

## 级别热更新与 panic

- **`SIGHUP` 重读 `[logging].level`**：`systemctl --user reload echo-agent-core`（或 `kill -HUP <pid>`）。热更新只换过滤器，不动 writer，因此在途请求不受影响；解析失败时保留原级别并打一条 WARN。改级别不必重启进程（重启要等在途回复 drain）。
- **panic 钩子**：panic 先以 `ERROR` 事件记一条（含位置与消息），再走默认钩子保留 stderr 与 backtrace。这样配了 `log_file` 时崩溃也能进日志文件，而不是只有 stderr 有。

## 排查入口

| 想知道 | 查什么 |
|---|---|
| 一次 turn 花了多久 | `agent turn completed` 的 `elapsed_ms` |
| 时间花在哪个工具 | 慢调用/失败恒 `INFO` 的 `tool call finished`（带 `elapsed_ms`） |
| 子代理卡在哪 | `subagent started` 是否有配对的 `subagent finished` |
| 联邦为什么不通 | `federation dial failing`（首次 + 每 60 次 WARN）/ `federation link up` |
| 插件为什么没生效 | `plugin tool package toggled`（只在状态翻转时出现） |
| 崩溃现场 | `panic caught by logging hook` |
