---
id: kernel
title: "运行地基（kernel 模块）"
group: Core
x: 3200
y: 1200
link: []
---

# 运行地基（`kind = "kernel"`）

> **定位**：2026-11 取消「中间层 / 模块」二分之后新增的一类模块——提供其它模块的**运行地基**（词汇、线契约、注册表、事件日志、配置存储、循环驱动）。本文写它们共同的性质、为什么禁不掉、以及逐个的职责。读者：需要理解"为什么每个 crate 都是模块"以及"为什么有些模块关不掉"的开发者。

## 取消的二分

此前 `source/` 下的 crate 被分成两类：

| | 中间层 | 模块 |
|---|---|---|
| 例子 | `agent-module-tools` / `echo-session` / `agent-module-management` | `agent-module-tools` / `-shell` / `-session` |
| 有 `ModuleManifest` | ❌ | ✅ |
| 进 `catalog` | ❌ | ✅ |
| 出现在 Panel 清单 | ❌ | ✅ |
| 可 `mount` / `unmount` | ❌（只是库） | ✅ |
| 靠什么协作 | `Ctx` 服务键 | `Ctx` 服务键（**一样**） |

**这个区分只存在于人的脑子里。** 两者都是编译进同一份二进制的 crate，都靠同一套 `Ctx` 服务键协作，唯一的实质差别只是"有没有 manifest"——而那只是二三十行声明代码。

现在的划分只剩一条线：**`kind`**。

| `kind` | 含义 | 例子 |
|---|---|---|
| `tool` / `skill` / `provider` / `loop` / `session` / `federation` / `management` / `interaction` / `orchestration` / `instance` / `security` | 能力：可挂载、可配额、可关 | `echo-agent.tools.builtin` |

## 为什么 kernel 模块禁不掉

**判据是「禁用之后系统还能不能起来」，不是「想不想让它可配」。**

| 模块 | 禁用后的后果 |
|---|---|
| `echo-agent.kernel.defs` | 20 个 crate 的 `impl` 找不到 trait——**编译期**概念，不是运行期开关 |
| `echo-agent.kernel.protocol` | 线协议对不上，Panel 与模块进程全部断连 |
| `echo-agent.kernel.config` | `core.toml` 读不出来，进程起不来 |
| `echo-agent.kernel.session` | 读不出已有会话文件（用户的全部历史） |

在 `modules.toml` 里给它们写 `enabled = false` 时，**启动失败并点名那一条**：

```text
Error: kernel module cannot be disabled: modules.toml 试图禁用 kernel 模块
禁用后系统无法启动。
请删掉那一行的 `enabled = false`，或在 `[security.sanitize]` 这类
专属开关里关闭对应能力（脱敏）而不是禁用模块。
```

为什么不是"静默跳过"：那等于给了运维一个**假动作**——"我关掉了它，系统还在跑"。实际后果要么是启动到一半才发现缺东西，要么是运行期某个能力静默失效。假动作比不给开关更糟。

## 脱敏为什么不在这里

`echo-agent.kernel.sanitize` 曾是这个列表的第 9 项（crate `echo-sanitize`，2026-11 与其它中间层一起补了 manifest）。2026-11 二次收敛时撤销，两条理由：

1. **不符合判据**。kernel 的判据是「禁用之后系统还能不能起来」；脱敏关掉后内核照常启动，只是不再脱敏。它自己也一直在文档里被称作「唯一的例外」——一个需要例外的判据其实是判据没写对。
2. **没有消费者**。该 crate 在生产路径上的唯一引用是 `agent-module-sanitize` 的 `build()`，其余全在 `#[cfg(test)]` 里。按 [架构总览](../overview/architecture.md) 的判据（消费者 = 1 → 合并），它本就该是那个模块的实现细节。

现在：实现（`registry.rs`）+ 构造（`build`）+ 身份（`kind = "security"`）同处 `agent-module-sanitize`。关闭脱敏的唯一路径仍是 `core.toml` 的 `[security.sanitize].enabled = false`（需重启 + 启动日志 `warn!`），见 [敏感信息隔离](../core/security-redaction.md)。

## 四个成员

| id | crate | 职责 | 有运行期状态 |
|---|---|---|---|
| `echo-agent.kernel.defs` | `echo-defs` | 9 个能力接缝 trait 与全部跨边界词汇（**零 `echo-*` 依赖**） | ❌ 纯 trait + 纯函数 |
| `echo-agent.kernel.config` | `echo-config` | TOML 原子读改写（tmp+rename，不认识任何段语义） | ✅ `ConfigStore` |
| `echo-agent.kernel.protocol` | `agent-module-management` | `BackendCommand` / `BackendEvent` / bridge / `timeline_text` | ❌ 纯数据 |
| `echo-agent.kernel.session` | `echo-session` | `SessionEvent` 事件集 / `EventLog` / `derive_messages` / compaction | ✅ `EventLog` |

## `mount` 为什么是空的（纯身份声明）

所有 kernel 模块的 `mount` 都返回**空 disposer**。这不是偷懒，是归属问题：

| 模块 | 运行期实例归谁 | 为什么不在 `mount` 里构造 |
|---|---|---|
| `tool` | 每 persona 一份 `ToolRegistry`（工具白名单不同） | 在此 provide 会造出"全局工具表"，与 persona 门控矛盾 |
| `session` | 每 persona 一份 trunk（`agent-module-session` 持有） | 在此 provide 会造出"全局日志"的假象 |
| `shell` | `ShellManager` 要绑定宿主事件出口 | 那是组合根装配期的信息 |
| `federation` | 链路泵要 peers 表 + 出站通道 | 只有组合根知道 |
| `config` | `ConfigStore` 要配置路径（`--config` 参数） | 模块在挂载期拿不到 |
| `defs` / `protocol` | — | 纯编译期概念，没有运行期对象可注册 |
| `loop` | `TurnRunner` 由 `agent-module-loop` 在 mount 里构造 | 本 crate 只提供状态机**类型** |

也就是说：**kernel 模块的 manifest 表达"这个地基存在"，而它的运行期实例由需要它的人构造。** 这与 `echo-agent.subagent`（纯身份模块，工具实现在别处）是同一手法。

## 两个零依赖的例外

`echo-defs` 与 `echo-config` 各有一条铁律：**零 `echo-*` 依赖**。

`ModuleManifest` 类型来自框架仓的 `echo-module`，引进来就破了这条线。所以这两个 crate 只导出 `MODULE_ID` 常量，manifest 由组合根用 `push_builtin_identity` 声明（id 与描述在 `core/src/builtin_modules.rs`）。

代价是它们的信息散在两处；收益是"定义层不认识任何宿主 crate"这条架构基石不被动摇。

## 相关

- 模块体系总览（来源 / 运行形态 / 门控）：[模块化设计](./modules.md)
- 分层与依赖方向：[架构总览](../overview/architecture.md)