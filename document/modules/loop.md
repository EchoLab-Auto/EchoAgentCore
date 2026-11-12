---
id: loop
title: "循环模块"
group: Core
x: 5440
y: 1200
---

# 循环模块

> **定位**：`echo-agent.loop.single` **这个模块本身**——它提供什么服务、为什么纯策略模块也要在 `mount` 里做事、以及「删掉并行模式」留下了什么。一轮回复的完整流程（准入、step、工具管道、流式转发）见 [Agent 循环](../core/agent-loop.md)；压缩策略的算法见本篇末节引用。读者：改循环行为、或排查「启动报缺 loop 服务」的开发者。

## 只有一个模块，没有模式可切

| 历史 | 现状 |
|---|---|
| `loop.single` + `loop.parallel` 两个模块按 persona 二选一 | **只剩 `loop.single`**；并行模式已于 2026-11 删除 |

并行模式的代价写在 crate 头部：它让同一会话可以并发多个 turn 分支，换来的是「读到一半的上下文」和「两条回答互相看不见」——对一个人格来说这不是能力，是不确定性。删除后一条会话永远只有一条时间线，配套的 `ReplyBranch*` 可见性事件与 `LoopMode` 类型一并消失。

`echo-agent.loop.parallel` 这个 id **仍被识别但不产生行为**：用户 `core.toml` 的 `enabled_modules` 里可能还留着它，Core 读到要能明白「这是旧配置里的并行模式」并忽略，而不是当成未知模块报错。新配置不再写入它。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.loop.single`（**字面量冻结**：用户配置里可能还写着它） |
| kind | `loop`（**进程级**，persona 白名单不参与） |
| 名称 / 路由 | 「循环」/ 路由 `loop` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-loop`（2 文件，约 1150 行，含 compaction 策略） |

| 贡献 | 内容 |
|---|---|
| 服务 `"loop"` | `mount` 时构造 `TurnRunner` 并 provide；卸载时 `Disposer` 撤销 |
| 服务依赖 | `llm`（`echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`）——构造 runner 必需 |
| 配置 | `mount` 读 `MountContext.config` 的两个字段：`max_tool_iterations` / `max_tokens`，缺省回退 `LoopOptions::default()` |

## 为什么 `mount` 不是空的（v0.9 起）

早期它只声明服务键、runner 由组合根建好再借模块名义登记。现在反过来：**服务由模块提供**。

- `mount` 真的构造 `TurnRunner` 并 `provide` 到 `loop` 键，卸载时撤销——`loop` 键随之消失，订阅它的模块由 `reconcile` 自动卸下。这就是「服务提供方也是一个模块」该有的行为
- 构造所需的东西全从上下文来：`Ctx[llm]` 是硬依赖，`MountContext.config` 是可覆盖项
- **同名键不覆盖**：`Ctx::provide` 对同名键 fail-closed。persona 作用域会重新构造一批模块实例，同一个键只能 provide 一次，第二次拿到 `Conflict` 并明确失败，而不是悄悄顶掉

**但注入到人格仍靠宿主**：`TurnRunner` 要到 `AgentManager` 注册之后才能塞进各 persona，而模块挂载在 `AgentManager` 注册之前——所以由人格启动循环**回填注入**（见 [Agent 循环](../core/agent-loop.md)「注入与分派」）。

## 键的归属

`loop` 这个键的值是字面量 `"loop"`，而 `llm` 键来自**定义层**（`echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`）。这个不对称是刻意的：`scripts/deps-policy.txt` 不允许本 crate 依赖 provider 模块，所以 `llm` 不能抄一份字面量——此前正是两边各写一遍，改名一侧就静默少一个依赖。

## 启停与门控

- `kind = loop` 属**进程级**：`enabled_modules` 里写 loop 类 id 不产生任何效果，persona 白名单不参与这类判定
- **缺它 = 启动失败**：`Inject` 里 `loop` 与 `llm` 都是硬依赖，注册表在 `mount` 前逐个 `Ctx::contains` 校验，缺失即 `MissingService` 且 mount 闭包不执行
- 卸载没有运行期资源可撤销（`mount` 返回空 disposer 列表是历史形态，现在返回的是 provide 的撤销句柄）

## 压缩策略在同一个 crate

`compaction` 模块（约 520 行）归这里，不是因为压缩属于循环，而是因为**压缩的触发点在循环里**（上下文到预算阈值）。它守两条不变量：

- 压缩**永不因缺 LLM 失败**（无摘要能力时退回规则裁剪）
- 按预算**分层收缩**（先丢最旧、再裁长工具输出、最后才摘要）

转写渲染与分层裁剪的细节见 [Agent 循环](../core/agent-loop.md)「上下文压缩」。
