---
id: session
title: "会话模块"
group: Core
x: 6080
y: 1200
---

# 会话模块

> **定位**：`echo-agent.session` **这个模块本身**——它为什么从宿主的 `session/` 模块搬出来、提供什么服务、三条写命令归谁处理、缺它会怎样。事件溯源 trunk 的**数据模型**（append-only 事件日志、按会话投影、压缩编排、显示历史派生）见 [多 Agent 与会话](../core/agents.md)与 [Agent 循环](../core/agent-loop.md)；持久化布局见 [配置持久化](../core/config-persistence.md)。读者：改会话存储、或排查「记忆没落盘 / 压缩不生效」的开发者。

## 为什么是独立模块

此前这 2268 行住在 `echo-agent/src/session/`：`TrunkStore` 与 `Agent` 同一个 crate。于是「会话怎么存」和「agent 怎么跑 turn」共用一次编译、一次发布，也无法在面板的模块清单里表达「这一份部署到底有没有会话子系统」。

外置后依赖方向变成：

```
echo-agent（宿主） ──依赖──► agent-module-session
                                  ├─ TrunkStore（日志 + 投影 + 持久化）
                                  ├─ catalog 里的一行 manifest
                                  └─ ServiceKey "echo-agent.session.factory"
```

**词汇不上移**：`SessionEvent` / `EventLog` / `derive_messages` 仍在 `echo-session`（机制层，谁都能 `use`，没有行为归属）；本 crate 只拥有**实例与其生命周期**。这与 [循环模块](./loop.md) 用 `echo-session` 的词汇、却自己 `provide` `loop` 服务是同一分工。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.session` |
| kind | `session`（persona 白名单参与） |
| 名称 / 路由 | 「会话」/ `session` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-session`（2 文件，约 760 行） |

| 贡献 | 内容 |
|---|---|
| 服务 `echo-agent.session.factory` | `Arc<dyn SessionFactory>`，`mount` 时 provide、卸载时撤销 |
| 命令域 × 3 | `ClearHistory`（清空）/ `ArchiveHistory`（归档）/ `CompactHistory`（压缩） |
| 服务依赖 | `echo-agent.host.session_commands`（`SESSION_COMMAND_HOST_KEY`） |

**为什么是工厂而不是单例**：每个 persona 一份独立上下文（多人格并存时各有自己的事件日志与 token 预算）。单例服务会让所有人格共享一份 trunk——那是多会话改造之前的行为。`create(memory_limit_tokens, persist_path)` 两个参数正好是「部署规格」的全部：预算来自 `core.toml` 的 `[agent]` / `[agent.teams.<id>]`，`persist_path = None` 即纯内存实例（测试与嵌入式用法）。

## 宿主接缝

- **宿主拿 trunk 的方式变了**：不再自己 `TrunkStore::new`，而是解析 `SESSION_FACTORY_SERVICE` 再 `create`。好处是**卸载即失效**——没有会话模块就造不出 trunk，报「会话模块未挂载」，而不是静默退化成「没有记忆也能跑」的半残状态（与 `MissingService` 同口径的 fail-closed）
- **`mount` 不加载任何会话**：只把工厂 provide 进 `Ctx`。trunk 由宿主在构造 persona 时按需 `create`——部署里可能一个 persona 都不启用，提前加载只会制造空文件
- **写命令要的宿主接缝**在 `mount` 时绑定、处理命令时取用（`SESSION_COMMAND_HOST_KEY`）：清理/归档/压缩要触碰的东西（会话目录、归档轮转、LLM 摘要调用）都在宿主侧，模块不持有路径

## 启停与门控

- `kind = session`，persona 白名单参与：`enabled_modules` 里去掉它，该人格的会话写命令（清理/归档/压缩）不再被派发
- **但 trunk 本身不会因此消失**——宿主构造 persona 时就要解析工厂。因此**全局**禁用会话模块等于启动失败（fail-closed），运行期 `ToggleModule` 对它应视为破坏性操作
- 三条命令的处理结果都以 `Error` 事件 toast 回推，与技能重载同一套可观测口径

## 边界

- 它**不做**摘要生成本身：压缩要调 LLM，那是宿主侧的编排（`agent-module-loop` 的 `compaction` 模块提供策略），本模块只负责「压缩这件事被触发时，trunk 怎么变」
- 它**不定义**事件词汇：`SessionEvent` 在 `echo-defs` / `echo-session`，本模块是这些词汇的**宿主实现**而不是定义者
- 显示面（时间线怎么投影、字段怎么映射）归 [显示层模块](./transcript.md)，不在这里
