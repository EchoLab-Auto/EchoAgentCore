---
id: kernel
title: "运行地基（kernel 插件）"
group: Core
x: 7040
y: 640
link: ["plugins | 插件化设计", "architecture | 架构总览"]
---

# 运行地基（`kind = "kernel"`）

> **定位**：2026-11 取消「中间层 / 插件」二分之后新增的一类插件——提供其它插件的**运行地基**（词汇、线契约、注册表、事件日志、配置存储、循环驱动）。本文写它们共同的性质、为什么禁不掉、以及逐个的职责。读者：需要理解"为什么每个 crate 都是插件"以及"为什么有些插件关不掉"的开发者。

## 取消的二分

此前 `source/` 下的 crate 被分成两类：

| | 中间层 | 插件 |
|---|---|---|
| 例子 | `echo-tool` / `echo-session` / `echo-protocol` | `echo-plugin-tools` / `-shell` / `-session` |
| 有 `PluginManifest` | ❌ | ✅ |
| 进 `catalog` | ❌ | ✅ |
| 出现在 Panel 清单 | ❌ | ✅ |
| 可 `mount` / `unmount` | ❌（只是库） | ✅ |
| 靠什么协作 | `Ctx` 服务键 | `Ctx` 服务键（**一样**） |

**这个区分只存在于人的脑子里。** 两者都是编译进同一份二进制的 crate，都靠同一套 `Ctx` 服务键协作，唯一的实质差别只是"有没有 manifest"——而那只是二三十行声明代码。

现在的划分只剩一条线：**`kind`**。

| `kind` | 含义 | 例子 |
|---|---|---|
| `kernel` | 运行地基：其它插件靠它才能工作 | `echo-agent.kernel.tool` |
| `tool` / `skill` / `provider` / `loop` / `session` / `federation` / `management` / `interaction` / `orchestration` / `instance` / `security` | 能力：可挂载、可配额、可关 | `echo-agent.tools.builtin` |

## 为什么 kernel 插件禁不掉

**判据是「禁用之后系统还能不能起来」，不是「想不想让它可配」。**

| 插件 | 禁用后的后果 |
|---|---|
| `echo-agent.kernel.defs` | 20 个 crate 的 `impl` 找不到 trait——**编译期**概念，不是运行期开关 |
| `echo-agent.kernel.protocol` | 线协议对不上，Panel 与插件进程全部断连 |
| `echo-agent.kernel.config` | `core.toml` 读不出来，进程起不来 |
| `echo-agent.kernel.session` | 读不出已有会话文件（用户的全部历史） |
| `echo-agent.kernel.tool` | 工具无处注册，模型看不到任何工具 |
| `echo-agent.kernel.shell` | `bash-*` 工具全部失效 |
| `echo-agent.kernel.federation` | 对端解析不了帧（链路跨进程，格式是硬契约） |
| `echo-agent.kernel.sanitize` | 内核照常启动，但不再脱敏——见下方「唯一的例外」 |
| `echo-agent.kernel.loop` | 没有 turn/step 驱动，输入永不被消费 |

在 `plugins.toml` 里给它们写 `enabled = false` 时，**启动失败并点名那一条**：

```text
Error: kernel plugin cannot be disabled: plugins.toml 试图禁用 kernel 插件
"echo-agent.kernel.tool"——它们是其它插件的运行地基（kind = "kernel"），
禁用后系统无法启动。
```

为什么不是"静默跳过"：那等于给了运维一个**假动作**——"我关掉了它，系统还在跑"。实际后果要么是启动到一半才发现缺东西，要么是运行期某个能力静默失效。假动作比不给开关更糟。

### 唯一的例外：`kernel.sanitize`

脱敏技术上可关（内核照常起，只是不再脱敏），但它标为 kernel 的理由是**安全不变量不该出现在能力开关里**。真正的关闭路径是 `core.toml` 的 `[security.sanitize].enabled = false`——它需要重启，并在启动日志里发一条明确的 `warn!`。

## 九个成员

| id | crate | 职责 | 有运行期状态 |
|---|---|---|---|
| `echo-agent.kernel.defs` | `echo-defs` | 9 个能力接缝 trait 与全部跨边界词汇（**零 `echo-*` 依赖**） | ❌ 纯 trait + 纯函数 |
| `echo-agent.kernel.config` | `echo-config` | TOML 原子读改写（tmp+rename，不认识任何段语义） | ✅ `ConfigStore` |
| `echo-agent.kernel.protocol` | `echo-protocol` | `BackendCommand` / `BackendEvent` / bridge / `timeline_text` | ❌ 纯数据 |
| `echo-agent.kernel.session` | `echo-session` | `SessionEvent` 事件集 / `EventLog` / `derive_messages` / compaction | ✅ `EventLog` |
| `echo-agent.kernel.tool` | `echo-tool` | `ToolRegistry`：可逆注册 + 定义缓存 + 出口脱敏 | ✅ `ToolRegistry` |
| `echo-agent.kernel.shell` | `echo-shell` | bash 子进程会话：进程组生命周期 + 哨兵分帧 | ✅ `ShellManager` |
| `echo-agent.kernel.federation` | `echo-federation` | Core↔Core 线协议：帧 / Hello / 邀请 / 重连 | ✅ 链路 |
| `echo-agent.kernel.sanitize` | `echo-sanitize` | `RegistryRedactor`：Aho-Corasick + 保守模式集 | ✅ 脱敏器 |
| `echo-agent.kernel.loop` | `echo-loop` | `TurnRunner`：turn/step 状态机 + `ToolPipeline` | ✅ 状态机类型 |

## `mount` 为什么是空的（纯身份声明）

所有 kernel 插件的 `mount` 都返回**空 disposer**。这不是偷懒，是归属问题：

| 插件 | 运行期实例归谁 | 为什么不在 `mount` 里构造 |
|---|---|---|
| `tool` | 每 persona 一份 `ToolRegistry`（工具白名单不同） | 在此 provide 会造出"全局工具表"，与 persona 门控矛盾 |
| `session` | 每 persona 一份 trunk（`echo-plugin-session` 持有） | 在此 provide 会造出"全局日志"的假象 |
| `shell` | `ShellManager` 要绑定宿主事件出口 | 那是组合根装配期的信息 |
| `federation` | 链路泵要 peers 表 + 出站通道 | 只有组合根知道 |
| `sanitize` | 脱敏器必须在 `llm` 服务注册**之前**就绪 | 插件 mount 发生在那之后 |
| `config` | `ConfigStore` 要配置路径（`--config` 参数） | 插件在挂载期拿不到 |
| `defs` / `protocol` | — | 纯编译期概念，没有运行期对象可注册 |
| `loop` | `TurnRunner` 由 `echo-plugin-loop` 在 mount 里构造 | 本 crate 只提供状态机**类型** |

也就是说：**kernel 插件的 manifest 表达"这个地基存在"，而它的运行期实例由需要它的人构造。** 这与 `echo-agent.subagent`（纯身份插件，工具实现在别处）是同一手法。

## 两个零依赖的例外

`echo-defs` 与 `echo-config` 各有一条铁律：**零 `echo-*` 依赖**。

`PluginManifest` 类型来自框架仓的 `echo-plugin`，引进来就破了这条线。所以这两个 crate 只导出 `PLUGIN_ID` 常量，manifest 由组合根用 `push_builtin_identity` 声明（id 与描述在 `core/src/builtin_plugins.rs`）。

代价是它们的信息散在两处；收益是"定义层不认识任何宿主 crate"这条架构基石不被动摇。

## 相关

- 插件体系总览（来源 / 运行形态 / 门控）：[插件化设计](./plugins.md)
- 分层与依赖方向：[架构总览](../overview/architecture.md)
