---
id: plugins
title: "插件化设计"
group: Core
x: 5760
y: 920
link: ["kernel | 运行地基", "tools | 工具集", "shell | Shell 会话", "workspace | 工作区", "subagent | Subagent", "skills | 技能目录", "provider-llm | LLM Provider", "loop | 循环", "sanitize | 脱敏", "session | 会话", "transcript | 显示层", "management-panel | 管理面", "embedded-panel | 内嵌 Panel", "federation | 联邦", "selfupdate | 自更新", "qq-gating | QQ 实例", "plugin-authoring | 插件开发"]
---

# 插件化设计

> **定位**：本文是插件体系的入口——插件模型（**来源**与**运行形态**两个正交维度）与装载机制、**全部内置插件清单**、能力开关（per-persona 门控）与扩展方式。**每个插件有自己的一篇**，从下表点进去：那篇写「这个插件作为可插拔单位长什么样」（身份、贡献、宿主接缝、启停、失败模式），机制原理仍留在对应子系统文档里。读者：需要理解或扩展插件体系的开发者；内核机制（插件宿主、事件总线）见 [框架（内核）](../core/frame.md)。

## 设计原则

插件化是 Core 的组合方式：每个内置模块以 `PluginManifest` 注册进 `PluginHost`（`source/plugin/echo-agent/src/plugins.rs`），可启用/禁用/热加载，且纳入 persona 能力白名单语义。

1. **一切皆插件**：LLM provider、TurnRunner、工具、技能、插件实例、编排、management server 均为插件，无特权核心模块
2. 插件 = manifest + 生命周期钩子；注册是可逆副作用（disposer）
3. **热重载优先于重启**：数据类插件（技能/工具/清单）秒级热重载；代码类插件不热重载——provider / loop 经原子二进制替换 + 进程重启生效，插件实例实现作为进程外插件随 Core 重启重建
4. 插件**不 import `echo-agent`**（只有组合根可以），可按需依赖定义层（`echo-defs` / `echo-plugin` / `echo-context`）与机制层（`echo-tool` / `echo-shell` / `echo-protocol` / `echo-session` / `echo-federation`）；逐 crate 的完整白名单在 `scripts/deps-policy.txt`，方向只向下

### 为什么不用动态库（.so）插件

Rust ABI 不稳定；`libloading` + C ABI 要求每个插件手写 extern "C" 桥，维护成本高、崩溃诊断难。因此采用**源码级插件 + 进程级热替换**：插件以 crate/目录形式存在，更新 = 重新构建二进制 + restart（复用自更新的原子替换机制）；热重载范畴限定为数据类插件。另一条通道是**进程外插件**（stdio 帧协议，与 ABI 无关），见「外部进程插件」。

### 边界与不做

- 不做动态库加载、不做插件沙箱（源码级插件即本仓库内代码，信任边界 = 仓库；进程外插件是**故障隔离**——独立 PID / 生命周期，不是安全沙箱）
- 不做插件市场/远程安装（需签名与供应链考虑）

## 插件模型

- 核心类型：`PluginManifest`（id/name/version/kind/entry/description + `builtin` / `runtime` / `package`）+ `BuiltinPlugin` + `MountContext`
- **插件类型（`kind`，**开放字符串**）**：`kernel` / `skill` / `tool` / `provider` / `loop` / `instance` / `orchestration` / `management` / `panel` / `interaction` / `session` / `federation` / `security`
  - **没有 `PluginKind` 枚举**（框架 M2 去业务化：`echo-plugin` 只存 `kind: String`，不校验取值）。宿主只对 `instance` 做语义判定（`INSTANCE_PLUGIN_KIND`）；Panel 的中文显示名表 `PLUGIN_KIND_LABELS` 是**另一份**，新增 kind 时要记得同步它
  - `interaction`：**人在环交互面**——模型发起、用户在 Panel 应答（Panel 内的问答 UI + 专用应答通道），与 `instance` / `management` 同属「整类能力开关」，在 Panel「启用插件」勾选区按 kind 即可见（`capabilities.ts::isPluginCheckboxVisible` 的 kind 类判定）。原选单（`present_menu`）已废弃移除，但该 kind **并非空置**：`echo-agent.transcript`（显示投影 + 会话视图命令域）就是 `interaction`——它做的事是"把会话讲给人看"
- 注册为**可逆**副作用：`Plugin::mount` 返回 disposer（`PluginMountResult`，注册表持有、卸载时调用）；`register_and_mount` 注册并按启用态挂载（禁用插件只注册不挂载）
- 数据插件（skill/tool）支持热重载（插件目录 5s 轮询）；代码插件需二进制重载
- **启动顺序**：`apply_disabled` 先于挂载——禁用插件启动时只注册不挂载；persona 白名单的门控**启动期与运行期统一**由 `Agent::apply_capabilities` 承担（门控集合按 `kind` 派生——见 `plugins/scope.rs` 的 `KIND_SCOPE`；最终 = 全局启用 ∧ 白名单）；插件宿主由组合根注入为**进程级单例**（全体人格共享）
- **mount 实化**：可安全逆注册的插件（management.panel / tools.builtin / skills.dir / instance.qq / workspace / subagent）已把组合根装配搬进 mount 闭包，运行期 `TogglePlugin` 停/再启对它们有真实运行效果；`instance.qq` 的启动期 mount 不抢跑（wired 标志在插件实例接线完成后才置位），启动受 `[instances.qq].enabled` 与插件状态双重门控；`loop.single` 在 mount 里 `provide` 唯一的 `TurnRunner`，但**注入到人格**依赖 `AgentManager`（注册于挂载之后）——由人格启动循环**回填注入**（见 [Agent 循环](../core/agent-loop.md)「注入与分派」）
- **运行期热更新**：插件/工具/技能勾选在 `SaveTeam` 保存后**立即生效**，无需重启——工具/技能逐名双向应用（取消勾选即禁用、重新勾选即恢复）；全局 `TogglePlugin` 经 mount/unmount 闭包逐 persona 重评估（`reapply_plugin_gating`：全局启用 ∧ persona 名单，名单外不放开、unmount 对全员生效）；`ToggleTool`/`ToggleSkill` 同样逐 persona 重算（`reapply_tool_gating` / `reapply_skill_gating`：persona 黑名单不被全局启用覆盖）

### 来源与运行形态（两个正交维度）

`PluginManifest` 曾用一个布尔同时承担两件事，现在拆成两个独立字段，互不推导：

| 字段 | 回答的问题 | 取值 | 缺省 |
| --- | --- | --- | --- |
| `builtin` | 这条 manifest 从哪来 | 编译内置 / 插件目录发现的 `plugin.toml` | `PluginManifest.builtin` 反序列化缺省 `false`（磁盘 `plugin.toml` 即外部）；wire 侧 `PluginInfo.builtin` 缺省 `true`（旧 Core 兼容） |
| `runtime` | 插件的实现代码在哪跑 | 本进程内 / 独立子进程 | manifest 侧是 `PluginRuntime` 枚举、声明缺省 `InProcess`（进程内时序列化省略该键）；wire 侧 `PluginInfo.runtime` 是字符串（`PluginRuntime::as_str` → `in_process` / `subprocess`），旧 Core 不发 → 空串，Panel 按进程内兼容 |

- **来源维度**：`builtin` 由 manifest 自己声明，`PluginDescriptor::from(&PluginManifest)` 直取字段——不再硬编码为真，也不按装载路径猜。内置注册走构造器 `PluginManifest::builtin(...)`（显式 `builtin = true`），磁盘 `plugin.toml` 反序列化得到 `false`。`reload_data_plugins` 以 `!desc.builtin` 识别「发现型插件」，故删掉 `plugin.toml` 后热轮询能卸载已挂载项（此前恒为内置，卸载分支永不触发）。
- **运行形态维度**：由 manifest 显式声明（`with_runtime(PluginRuntime::Subprocess)`）。现行唯一 `subprocess` 成员是 `instance.qq`——门控 manifest 编译在 Core 里（`builtin = true`），实现却由独立进程 `echo-adapter-qq` 提供；描述文字不算数，前端读的是字段。
- **协议与 Panel**：`PluginsList` 逐项下发 `{ builtin, runtime }`；旧 Core 不发 `runtime`（空串），Panel 按进程内兼容。设置视图「内部 / 外部插件」分组只看**运行位置**（`runtime == 'subprocess'` → 外部组；`runtime` 缺失才回退按 `builtin === false` 推断），`builtin` 仅作旧 Core 兼容回退与详情标签。
- **术语**：本文「内置插件」指 `builtin`（来源），「进程内 / 进程外」指 `runtime`（运行位置）——两者不再同义，勿用其一代替另一。

### 内联分派工具

- `spawn_subagent` 由 loop 内联分派（异步受理），按 persona 白名单过滤（`allows_dynamic_tool`）；schema 由注册表提供（组合根装配时注册）
- 原 `run_sudo` / `present_menu` / `framework_update` 三个内联工具已废弃移除（见 [工具系统](./tools.md)「已废弃工具」）

### 外部进程插件

除源码级进程内插件外，系统支持**进程外插件**——插件以任意语言实现、经 stdio 帧协议与宿主通信（`runtime = subprocess`），装载源是 `plugins.toml`（文件不存在即特性休眠），崩溃自动重启（进程级隔离）。Core 自带的进程外组件走同一条路径：插件实例按 `[instances.<platform>.plugin]` 与实例表逐实例生成 `LaunchSpec`（`plugin_id = instance-<platform>-<instance>`、状态文件 `<platform>-<instance>.toml`，命令缺省 `echo-adapter-<platform>`），实例规格由 `instance_deploy` 解析；**平台部署细节（端口分配 / 容器名 / compose）在插件进程内**。

- **装载**：`plugins_runtime::start_with_specs` 把装载器筛出的每条 `LaunchSpec`（plugin_id / command / args / env / config）交给独立 `PluginSupervisor` 拉起，工具与实例贡献再接入 Core；Core 侧对这类插件只保留启停门控，不持有实现
- **进插件清单**：每个**静态**条目（动态的 QQ 实例 `LaunchSpec` 除外，它们另有实例门控壳）会在插件宿主里注册一个门控壳——`runtime = subprocess` + `builtin = false`，`enabled` 取配置值，启动失败原因填进 `error`。此前它们从不注册，面板的「外部插件」组因此只剩实例壳，用户装的外部插件完全不可见
- **启停真的停/起子进程**：门控壳的 mount 仍是 no-op，但 `TogglePlugin` 对这类条目走**子进程控制面**（独立 trait `SubprocessPluginControl`，经 `PluginHost::set_subprocess_control` 注入；组合根用 supervisor 的 `PluginHandle` 实现它）：禁用 = `stop`（排空后进程退出、监督任务留下可重启），启用 = `start`（重新握手、重新申报贡献）。`RestartPlugin` = `stop` + `start`
- **入站频率限额**（宿主保护）：`Emit` / `Log` 超每秒配额即丢弃并计数，`HostCall` 超限回 `rate_limited` 错误回执（插件能感知背压），`InvokeResult` 永不丢（丢了调用挂死）。缺省 2000 / 500 / 200 每秒——合法插件远够不到，失控循环会被挡住
- **与可逆停止的区别**：supervisor 的 `Control::Stop` 保留监督任务（可再 `Start`），`Control::Drain` 是终态（任务退出）。前者服务面板启停，后者服务关停/热替换。停止期间调用立刻失败（不排队、不挂死）
- **与来源维度无关**：进程外可以是「manifest 编译内置 + 实现独立进程」（`instance.qq`），也可以是 manifest 与实现都来自外部；判据是 `runtime`，不是 `builtin`
- **与进程内插件的差别**：进程内 = 实现编译进 Core（挂载/卸载即生效）；进程外 = 独立 PID 与生命周期（崩了不带走 Core，启停经门控）
- **实例插件贡献什么**：`InstanceContribution`（实例名 / 平台 / 能力位 / 配置 schema / UI 描述符 / 会话标签 / **模型工具声明**）。宿主侧不认识任何平台细节——管理界面按描述符渲染、管理动作经 `invoke_custom(method, payload)` 字符串透传、**模型工具由插件声明**（宿主只做合并 / `account` 寻址 / 出站闸门 / 透传，见 [工具系统](./tools.md)「工具从哪来」）
- 协议、SDK 与部署方式见 [插件开发指南](./plugin-authoring.md)（含「实例插件」一节）

## 内置插件清单

| 插件（id 即文档链接） | kind | 运行形态 | 一句话 |
| --- | --- | --- | --- |
| [`echo-agent.kernel.defs`](./kernel.md) | Kernel | 进程内 | 词汇与接缝：9 个能力 trait + 跨边界词汇（**零 echo-* 依赖**） |
| [`echo-agent.kernel.config`](./kernel.md) | Kernel | 进程内 | 配置存储：TOML 原子读改写 |
| [`echo-agent.kernel.protocol`](./kernel.md) | Kernel | 进程内 | 线契约：`BackendCommand` / `BackendEvent` / bridge |
| [`echo-agent.kernel.session`](./kernel.md) | Kernel | 进程内 | 事件溯源：`SessionEvent` / `EventLog` / 投影 / compaction |
| [`echo-agent.kernel.tool`](./kernel.md) | Kernel | 进程内 | 工具机制：`ToolRegistry`（可逆注册 + 定义缓存 + 出口脱敏） |
| [`echo-agent.kernel.shell`](./kernel.md) | Kernel | 进程内 | Shell 机制：进程组生命周期 + 哨兵分帧 |
| [`echo-agent.kernel.federation`](./kernel.md) | Kernel | 进程内 | 联邦线协议：帧 / Hello / 邀请 / 重连 |
| [`echo-agent.kernel.loop`](./kernel.md) | Kernel | 进程内 | 循环驱动：`TurnRunner` 状态机 + `ToolPipeline` |
| [`echo-agent.tools.builtin`](./tools.md) | Tool | 进程内 | 内置工具集（计算/搜索/编码/实例管理）；**不含 shell 会话** |
| [`echo-agent.shell`](./shell.md) | Tool | 进程内 | 后台 bash 会话 + Shell 面板命令域 |
| [`echo-agent.workspace`](./workspace.md) | Tool | 进程内 | 工作区会话（多目录管理 + git 状态 + 只读文件浏览） |
| [`echo-agent.subagent`](./subagent.md) | Tool | 进程内 | 委派隔离上下文的子任务 + 完成后回灌 |
| [`echo-agent.skills.dir`](./skills.md) | Skill | 进程内 | SKILL.md 技能目录（热重载） |
| [`echo-agent.provider.llm`](./provider-llm.md) | Provider | 进程内 | provider 工厂 + 计量/脱敏装饰 + API 命令域 |
| [`echo-agent.loop.single`](./loop.md) | Loop | 进程内 | 唯一循环插件：提供 `loop` 服务，同会话 turn 串行排队 |
| [`echo-agent.security.sanitize`](./sanitize.md) | Security | 进程内 | 脱敏（安全不变量，**人格关不掉**）；`kind = security` → 进程级 |
| [`echo-agent.session`](./session.md) | Session | 进程内 | 事件溯源 trunk、持久化、压缩编排 |
| [`echo-agent.transcript`](./transcript.md) | Interaction | 进程内 | 时间线投影 + 会话视图命令域 |
| [`echo-agent.management.panel`](./management-panel.md) | Management | 进程内 | Panel 连接面（**禁用即自锁**） |
| [`echo-agent.panel.web`](./embedded-panel.md) | Panel | 进程内 | 可选 Web Panel；**缺省不挂载**，独立 Panel 外壳仍可用 |
| [`echo-agent.federation`](../federation/federation.md) | Federation | 进程内 | Core↔Core 对等链路：远程工具 / 跨机委派 / 会话迁移 |
| [`echo-agent.selfupdate`](./selfupdate.md) | Orchestration | 进程内 | 自更新触发与状态查询（部署形状归插件） |
| [`echo-agent.instance.qq`](./qq-gating.md) | Instance | **进程外** | QQ 插件实例（OneBot v11 反向 WS，独立进程） |

> **`Kernel` 是什么**：2026-11 取消「中间层 / 插件」二分之后新增的类别——提供其它插件的**运行地基**（词汇 / 线契约 / 注册表 / 事件日志 / 配置存储 / 循环驱动）。它们与其它插件同形（有 manifest、进 catalog、出现在本表），但**不可禁用**：`plugins.toml` 写 `enabled = false` 会让启动失败并点名。逐条理由见 [运行地基](./kernel.md)。

表的来源分两类，别混算：**框架内置 22 项**（`echo_agent::plugins::BUILTIN_PLUGIN_IDS: [&str; 22]`，`builtin = true`，实现同在 Core 进程内）与**插件实例 1 项**（`echo-agent.instance.qq`）——后者的 id 不是硬编码，由组合根按前缀 `echo_agent.instance.<平台名>` 声明（`core::instance_manifests::PLUGIN_ID_PREFIX`），新增平台只加配置不改框架。运行形态一列独立于来源：`instance.qq` 即「框架声明 manifest + 进程外实现」的现行样本。

联邦（`echo-federation`）为对等链路包——跨机工具调用、委派与只读查询，机制见 [联邦（多机）](../federation/federation.md)。

> **框架侧实现位置**：每个内置插件都是一个独立 crate（`source/plugin/echo-plugin-*`），上表第一列的链接即它的契约文档。工具子系统的跨包机制（`ToolRegistry`）在 `echo-tool`，Shell 机制在 `echo-shell`——**机制层**放词汇与通用实现，**插件 crate** 放「这台部署上要不要装、装了之后给谁用」。
>
> **仍内联在 echo-agent 的**只剩三个包：`packages/tools_builtin`（**只剩端口接线**——工具清单的所有权已在插件，`build_registry` 只做「给端口、拿工具箱」）、`packages/instance_ops`（入站 hook 桥 + 通用实例命令域）、`packages/federation`（远程代理工具与查询适配）。公开路径保持原样（`echo_agent::subagent::…` 等经 lib.rs 兼容 re-export 指向新 crate）。
>
> QQ 工具（`send_*`/`get_*` 共 11 个）**不再由 Core 实现**：它们作为 `ToolContribution` 由插件进程申报（插件仓库 `crates/echo-adapter-qq/src/tools.rs`），Core 只做注册与门控（`packages/instance_ops` 的入站/出站桥）。去适配器化时 `source/plugin/core/src/qq_tools.rs` 已删除——插件实例本就能声明工具，没有留给组合根的特例。

### 内置插件也能由 `plugins.toml` 组合（P4）

内置插件的**实现**编译在 Core 里（mount 闭包要捕获端口 / token / 实例句柄等运行期句柄，构造只能在代码里发生），但**集合、顺序、启用状态**可以交给配置：在 `plugins.toml` 里写 `kind = "builtin"` 的行即可，与子进程插件条目共存于同一次分层合成（见 [插件开发指南](./plugin-authoring.md)）。

```toml
[[plugin]]
id = "echo-agent.subagent"        # 排到最前（行序 = 挂载顺序）
kind = "builtin"
name = "Subagent 委派"

[[plugin]]
id = "echo-agent.tools.builtin"
kind = "builtin"
name = "内置工具集"
enabled = false                   # 不挂载（仍注册，Panel 清单可见、可就地启用）
```

语义要点：

- **缺省不写内置行 = 全部照旧挂载**。计划里没提到的内置插件按目录顺序追加挂载——现网 `plugins.toml` 大多只登记子进程插件，绝不能因为加了一条外部插件就关掉工具集/技能/管理面
- **写错 id 只告警不致命**（启动日志 `builtin mount plan diagnostic=...`）：未知 id 被忽略，其余照常
- `enabled = false` 只影响**启动期挂载**，不进 `core.toml` 的持久化状态——要长期生效就留在 `plugins.toml` 里；运行期开关仍走 `TogglePlugin`（Panel）
- 每个条目可带 `[plugin.limits]`（子进程资源限额：CPU / 地址空间 / 单文件 / fd / 进程数，缺省不限制）——见 [插件开发指南](./plugin-authoring.md)「资源限额」
- 排查"最终这一行来自哪一层、值是什么"用 `echo-agent-core dump-config`（只合成不启动）

> 以上 id 也是更新器（`scripts/update.sh`）插件感知校验的核对清单——`expected_ids` 由**框架 22 项 + 前缀 `echo-agent.instance.`** 组成（不是逐个列实例 id；清单真值在 `scripts/update.sh` 的 `expected_ids` 一行，与 `BUILTIN_PLUGIN_IDS` 由单测守护）；选单（present_menu）已废弃移除，任务清单（checklist）为普通内置工具。

## 能力开关（per-persona）

- 每个插件 id 均可放入 agent 的 `enabled_plugins`（白名单；空表 = 全部启用）
- **插件黑名单 `disabled_plugins` 已移除**：与白名单语义重复（前端首次取消勾选即物化全量白名单），既有配置在加载期物化进白名单（见 [配置持久化](../core/config-persistence.md)）；运行期 `SaveTeam` 只写白名单
- **前端「启用插件」勾选区的成员规则**：**完全由 Core 判定，Panel 不再维护任何插件清单**。Core 按 manifest 的 `kind` 算作用域（`echo_agent::plugins::scope::KIND_SCOPE`），随 `PluginInfo.gating` 下发：
  - `gating = "persona"`（`kind ∈ {tool, skill, instance}`）→ 显示。勾选写 `enabled_plugins` 白名单，取消即真的关掉该包的工具/技能/实例；
  - `gating = "process"` 且 `kind ∈ {management, interaction}` → **显示但置灰**（标签带「· 进程级」）。它们对每个人格始终生效、勾不动；藏起来会让用户以为这个能力不存在，所以显示成"已勾且不可改"并附一行说明。判据是**能力类别**（用户心智里"这个人格有没有管理/交互能力"），不是"能不能关"；
  - `gating = "process"` 且其余 kind（`provider` / `loop` / `federation` / `orchestration` / `session`）→ 不显示（它们的开关在别处：API 设置 / `plugins.toml`）；
  - 旧 Core 不带 `gating` 字段 → Panel 退回 kind 类判定（`instance` / `management` / `panel` / `interaction` 可见），不让插件在 UI 里凭空消失。

  **为什么不再有清单**：此前 Core 的 `GATED_PLUGIN_IDS` 与 Panel 的 `PACKAGE_GATED_PLUGIN_IDS` 是同一件事的两份手写真相，跨仓库改一处忘另一处就出错。实测错过两轮——`shell` 从 `tools.builtin` 拆出后 Panel 没同步（**面板上没有 shell 开关**，用户无法通过 UI 给任何 persona 启用 `bash-*`）；`session`/`transcript` 进 Panel 清单而 Core 没有（两个**勾了不生效**的假开关）。现在判据只有一处：`kind`。

  **新增插件的开销**：`kind = tool/skill` 的插件**零门控改动**；只有引入**新 kind** 时才要在 `KIND_SCOPE` 登记，而未登记的 kind 会在装配期被 `every_catalog_kind_is_classified` 拦下并点名（fail-closed 到进程级，避免"用户勾了却用不了"）。
- **循环没有模式可切**：`echo-agent.loop.single`（`kind = loop`）是**进程级**插件，persona 白名单不管它——`enabled_plugins` 里写 loop 类 id 不产生任何效果；同一会话的 turn 串行排队是唯一行为（见 [Agent 循环](../core/agent-loop.md)§会话内准入）
- **包级聚合**：白名单里的插件 id 即包 id——一次勾选同时门控该插件的生命周期与同名包的工具、技能（QQ 包见「Package」节）
- **其余插件当前生效范围**：
  - `tools.builtin` / `skills.dir`：禁用 = 该包全部工具（skills.dir 为全部技能）对所有 persona 批量禁用（对 LLM 不可见），启用按各 persona 名单恢复——**仅作用于目标 persona 时用 Agent 配置弹层的勾选**（运行期双向、即时生效）
  - `instance.qq`：禁用 = 停止 QQ 插件实例进程 + QQ 工具包禁用；启用 = 启动 + 按名单恢复
  - `management.panel`：禁用 = 关闭 management WS（**注意自锁**：Panel 将断连，恢复需编辑 core.toml 的 `disabled_plugins` 移除该 id 后重启 Core）。**防自锁保护**：经 `TogglePlugin` 禁用它会被 Core 拒绝（Error 事件明示，状态不变）——禁用与恢复都只能走 core.toml + 重启
  - `loop.single`：**唯一循环插件**（`kind = loop`，进程级）——mount 时构造唯一的 echo-loop `TurnRunner` 并 `provide` 成 `loop` 服务，卸载撤销该服务；`loop` 服务缺失 = 启动失败。策略只有一条：同一会话的 turn 串行排队（每会话一条时间线），没有模式可切，persona 白名单不参与。详见 [Agent 循环](../core/agent-loop.md)「会话内准入」
  - `provider.llm`：**命令域随插件启停**（13 条 API / provider 命令禁用后回落到宿主兜底并明确报错），但 provider 的**构造**仍由宿主在配置变更时完成——运行中替换 provider 涉及在途 turn，不因插件挂载而改变
  - `provider.llm`：**命令域随插件启停**（13 条 API / provider 命令禁用后回落到宿主兜底并明确报错），但 provider 的**构造**仍由宿主在配置变更时完成——运行中替换 provider 涉及在途 turn，不因插件挂载而改变
  - `selfupdate`：进程级、`kind = orchestration`（按人格启停是假开关——挂载是进程级的，命令分发不看人格白名单）
- **优先级**：全局禁用（`TogglePlugin` 卸载 / `[agent].disabled_tools|skills`）> persona 名单；全局重新启用不会越过 persona 名单，hook 后由 `Agent::reapply_*` 重算
- **任务清单的会话隔离**：`ChecklistTool` 状态按**会话**隔离（session_id → 清单名 → 项）——同一 persona 的不同会话（local:tui / qq:group:…）的清单互不可见、互不覆盖；会话键由 `Agent::run_tool` 在参数校验后注入 `__session_id`（联邦远程/无会话上下文调用落 `__global` 兜底键）。事件 `ChecklistUpdated` 带 `team_id`，Panel 按 `(core, team_id, session_id)` 复合键存储并显示
- **内置工具包覆盖**：任务清单（checklist）已降级为普通内置工具（随 `tools.builtin` 包，见上表注）；关闭内置工具集 = checklist 一并不可用，单独停用请在「启用工具」里按名勾选（运行期双向、即时生效），Panel 入口行同步移除「清单」入口、关闭已打开浮层并清空徽标状态；`base` 人格演示了"纯对话"配置（禁用 tools.builtin + skills.dir）

### 同步清单

新增**包维度门控插件**时，三处必须同步（缺一即「配置里看不到/门控失效」）：

1. Core `plugins.rs` 的 `BUILTIN_PLUGIN_IDS`（门控**不需要**登记：按 `kind` 派生）
2. 更新器 `scripts/update.sh` 的插件校验 `expected_ids`
3. Panel `capabilities.ts::PACKAGE_DISPLAY_NAMES`（包显示名；勾选区可见性**不需要**同步——Core 下发）

三项均有测试/校验守护（`builtin_id_list_matches_the_catalog_exactly` / `instance_manifests` 单测 / `capabilities.test.ts`）。

新增**插件 kind** 时只改两处：
1. Core `plugins/scope.rs::KIND_SCOPE` 登记它的**作用域**（`Persona` = 可被 persona 白名单关掉 / `Process` = 进程级）——未登记会在装配期被 `every_catalog_kind_is_classified` 拦下；
2. Panel `capabilities.ts::PLUGIN_KIND_LABELS` 加中文显示名（测试覆盖）。

**不需要**改任何门控清单——`persona` 作用域的插件由 Core 按 kind 自动派生进勾选区，随 `PluginInfo.gating` 下发。

## Package（包）——横跨 plugin + tool + skill 的标签

**Package 是一等标签**（非实体、无独立 manifest），把三种异构成员聚合成一个可整体启停的能力单元。三种载体各自声明归属，按**同一个字符串**聚合：

| 维度 | 声明方式 | 示例（QQ 包） |
| --- | --- | --- |
| Plugin | `PluginManifest.package`（缺省 = 插件 id 自身，`package_id()` 归一） | `echo-agent.instance.qq` |
| Tool | `ToolRegistry::set_package(name, pkg)`（装配期打标） | QQ 工具（send_*/get_*） |
| Skill | `SKILL.md` frontmatter `package:` | qq-management / qq-transport |

**包级门控语义**（`Agent::apply_plugin_gating`，横跨两个注册表）：

- 禁用包 → 包内**工具与技能一起**关闭（对 LLM 不可见）+ 插件自身生命周期副作用（如 instance.qq 停进程）；包外成员不受影响
- 启用包 → 两者一起恢复，但按 persona 白名单**收紧**（名单外的成员保持禁用；全局禁用优先）
- `skills.dir` 插件是**整表**语义（全部技能），其余包按同名 `package:` 精确匹配
- persona 白名单里的插件 id 即包 id——勾选/取消勾选一个插件 = 勾选/取消整个包（plugin + tools + skills）

**可观测性**：`PluginInfo.package` 随 `PluginsList` 下发（未声明回退为插件 id），Panel 插件详情展示「包（Package）」字段；设置视图已按包分组展示工具/技能。

**多插件包（前瞻）**：`PluginManifest.with_package()` 允许一个包绑定多个插件；当前全部内置插件均为「插件 id = 包 id」的单插件包，门控按插件 id 传播即可（QQ 包为现行示例）。

## 用户扩展方式

- **技能**：在 skills_dir 放置 `SKILL.md`（带 frontmatter：name/description/keywords/always/category/package），运行时自动发现、热重载
- **数据插件**：在 plugins_dir 放置 `plugin.toml`（skill/tool kind）——`plugins/{kind}/{id}/plugin.toml` 只是约定布局，实现实际是 WalkDir 递归扫描 plugins_dir 下任意 `plugin.toml`（`agent/plugin_reload.rs` 的 `reload_data_plugins`），不限三层；磁盘 manifest 不带 `builtin` 键 → 解析为 `false`（来源 = 外部），运行形态默认进程内；5s 轮询热加载（按插件 id 挂载/卸载；**manifest 内容 diff（name/version/description/author/package 任一变化）触发重挂载**——unmount + 重注册 + mount，判定逻辑 `data_plugin_stale` 有单测）；启用状态持久化于 `[agent].disabled_plugins`
- **代码插件**（provider/loop/instance）：需修改源码并走自更新流程
