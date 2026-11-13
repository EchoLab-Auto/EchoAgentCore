---
id: management-panel
title: "管理面模块"
group: Core
x: 3348
y: 216
---

# 管理面模块

> **定位**：`echo-agent.management.panel` **这个模块本身**——Panel 的连接面（监听、鉴权、中继、端口合并）与它的**自锁语义**。面板看到什么内容由 [显示层模块](./transcript.md) 决定；线协议帧格式见 [协议与数据流](../protocol/protocol.md)。读者：改连接行为、排查「Panel 连不上 / 令牌无效 / 端口冲突」的开发者。

## 它为什么本来就是协议驱动的

中继两侧都只碰 `agent-module-management` 的类型：入站帧 → `BackendCommand` 塞进 bridge；bridge 的事件 → 带 `seq` + entry id 序列化成帧发回。**没有一处需要 `Agent` 句柄**——服务化后这一点被显式化：函数签名里不再有 `Arc<Agent>` 参数（此前它已标注 `_agent` 未使用）。

所以它的外置成本极低：原 `source/modules/core/management.rs` 整体搬成 crate，加一个 `mount` 闭包起监听、返回 `abort()` 作 disposer。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.management.panel` |
| kind | `management`（persona 白名单参与） |
| 名称 / 路由 | 「管理面」/ 路由 `panel` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-management`（2 文件，约 1950 行） |

| 贡献 | 内容 |
|---|---|
| 服务依赖 | `echo-agent.host.management`（`MANAGEMENT_HOST`） |
| 命令域 | **不声明任何命令**——会话视图 5 条归 [显示层模块](./transcript.md) |
| 运行期资源 | `mount` 里 `tokio::spawn` 一个 WS server，返回的 disposer = `abort()` |
| 配置 | **只接受 `address`**（`MountContext.config_str("address")`） |

`token` 刻意**不接受模块配置覆盖**：它会进模块清单接口与日志，从 `modules.toml` 注入等于把入口写在明面上。它只从 `core.toml` 的 `[core]` 段读。

## 两种启动形态

| 形态 | 入口 | 何时用 |
|---|---|---|
| 独立端口 | `serve_with_token` | 管理面独占一个地址 |
| 与联邦入站共用端口 | `serve_with_federation` | 同址多路复用，靠令牌区分 |

合并端口需要**规划 + 校验**两道：`plan_federation_listen` 给出缺省值（`0.0.0.0:3133`，与组合根 `port_cmd::DEFAULT_FEDERATION_LISTEN` 同值），`validate_merged` 做 fail-closed 校验——合并 = 管理通道与联邦入站同址，必须令牌可分；分不开就直接拒绝启动，而不是起来之后被陌生人当管理面用。

`EventBroker` 是内部结构（不导出）：它持有**唯一**的事件接收者，为每个连接分配一份订阅通道。为什么必须这样：`BackendBridge` 只有一个事件接收者，若每个 WS 连接直接排空它，一个 Panel 标签页就能饿死其它连接。序号在**持锁投递之前**分配——与订阅者收到事件的顺序严格一致，多窗口看到同一套编号，Panel 据此做流连续性校验（跳号 = 中间有帧没到）。

## 自锁语义

这是本模块最需要知道的一条：**禁用管理面 = 关掉 Panel 通道本身**。

- 关掉之后没有任何 UI 能再把它打开（Panel 连不上）
- 因此 `ToggleModule` 对它**专门拒绝**，`RestartModule` 同样不适用——重启的指令也得经这条通道送达
- 这条语义是模块自己的（「我的能力是提供通道，关我就没人能开」），不是宿主的特例分支

## 它与显示层的分工

| 变什么 | 改哪个 crate |
|---|---|
| WS 协议升级 / token 轮换 / 端口冲突 / 心跳与重连 | **本模块** |
| 时间线条目顺序 / 工具占位名 / 推理瘦身阈值 / 历史回填 | [显示层模块](./transcript.md) |

拆开的收益是可观测的：禁用管理面 = Panel 连不上；禁用显示层 = Panel 连得上但内容不再更新。合在一起时这两件事是同一个开关，调试时分不清「连不上」还是「没内容」。
