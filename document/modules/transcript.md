---
id: transcript
title: "显示层模块"
group: Core
x: 3048
y: 216
---

# 显示层模块

> **定位**：`echo-agent.transcript` **这个模块本身**——显示投影与会话视图命令域的归属、它为什么从管理面里拆出来、宿主必须给它哪两处数据接缝。时间线投影的**具体顺序规则**（思考条目、工具占位名、推理瘦身阈值）见 [会话视图](../panel/chat.md)与 [主时间线时序与动画](../panel/chat-timeline.md)；连接面本身见 [管理面模块](./management-panel.md)。读者：改面板显示逻辑、或排查「面板看到的事件不对」的开发者。

## 为什么从管理面里拆出来

`agent-module-management` 的职责是**连接面**（WS 中继、鉴权、端口合并、心跳）。本模块关心的是**「面板看到什么」**——哪些事件进时间线、字段怎么映射、工具完成态怎么原地合并、推理发往前端怎么瘦身、旧会话历史怎么回填。

两类的变更轴不同：

| 变什么 | 改哪个 crate |
|---|---|
| WS 协议升级 / token 轮换 / 端口冲突 | [管理面模块](./management-panel.md) |
| 思考条目顺序 / 工具占位名 / 推理瘦身阈值 | **本模块** |

合在一起意味着两类改动共用一次编译与一次发布。更要紧的是语义会互相污染：管理面的「禁用 = 关闭 Panel 通道」会把显示层一起关掉——而显示层的数据源（`TrunkStore`）与面板通道并无必然耦合。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.transcript` |
| kind | `interaction`（人在环交互面） |
| 名称 / 路由 | 「显示层」/ `transcript` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-transcript`（3 文件，约 1200 行） |

| 贡献 | 内容 |
|---|---|
| 命令域 × 5 | `RequestState` / `RequestContext` / `RequestTrunkTimeline` / `RequestTimelineEarlier` / `RequestSessionHistory` |
| 服务依赖 | `echo-agent.host.session_view`（`SESSION_VIEW_HOST_KEY`） |
| 实时投影 | `timeline` 模块：订阅事件、写窗口、分配序号、原地合并工具完成态 |

`kind = interaction` 不是空置分类：它表示「模型或宿主发起、最终呈现给人看」的一类能力，与 `instance` / `management` 同属「整类能力开关」——在面板「启用模块」勾选区按 kind 即可见。

## 宿主接缝只有一件事：数据面

模块不拿 `TrunkStore` 引用。理由写在 crate 头部：那会让模块认识宿主的锁类型与窗口实现（「seam 变影子」的经典形状）。取而代之是两个**语义快照**接口：

| 接缝 | 方向 | 方法语义 |
|---|---|---|
| `timeline::TimelineHost` | 写 | `push`（追加时间线条目）/ `next_seq`（分配序号）/ `complete_running_tool`（工具完成后原地改写，不追加新行） |
| `session_view::SessionViewHost` | 读 | 读快照与派生历史（含推理正文的线上瘦身） |

每个方法都是一次语义操作，不是一次字段访问——所以宿主可以换掉底层实现而不动模块，模块也没法绕过接缝去偷看内部状态。

## 启停与门控

- `kind = interaction`，persona 白名单参与：某个 agent 不启用显示层，它的会话视图命令（快照 / 上下文 / 翻页 / 历史）不再被派发
- **与管理面解耦的实际收益**：禁用管理面 = Panel 通道关闭（自锁，见 [管理面模块](./management-panel.md)）；禁用显示层 = Panel 还能连上，但「看到什么」不再更新。两者不再是同一个开关
- 命令处理时若接缝未绑定，回明确错误「模块尚未挂载（会话视图接缝未绑定）」，而不是返回空数据

## 边界

- 它**不产生**事件：时间线的数据源是宿主的事件总线，本模块只决定「哪些进、怎么映射」
- 它**不做**持久化：写窗口落到 `TrunkStore`，持久化归 [会话模块](./session.md)
- 协议形状归 [协议与数据流](../protocol/protocol.md)：本模块产出的是 `agent-module-management` 的类型，帧编码不在这里
