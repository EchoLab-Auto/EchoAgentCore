---
id: plugins
title: "插件化设计"
group: 插件
x: 640
y: 1620
link: ["tools | 工具系统", "skills | 技能系统", "subagent | Subagent 插件 | b>l", "qq-gating | QQ 实例门控 | b>l", "federation | 联邦（多机） | t>b", "plugin-authoring | 插件开发指南 | b>l"]
---

# 插件化设计

> **定位**：本文是插件体系的入口——插件模型（**来源**与**运行形态**两个正交维度）与装载机制、**全部内置插件清单**、能力开关（per-persona 门控）与扩展方式。各插件的机制细节见子篇：[工具系统](./core-tools.md)、[技能系统](./core-skills.md)、[Subagent 插件](./core-subagent.md)、[QQ 实例门控](./qq-gating.md)、[联邦（多机）](../federation/federation.md)、[插件开发指南](./plugin-authoring.md)；系统设计与路线图见 [完全解耦推进计划](../design/decoupling-plan.md)。读者：需要理解或扩展插件体系的开发者；内核机制（插件宿主、事件总线）见 [框架（内核）](../core/frame.md)。

## 设计原则

插件化是 Core 的组合方式：每个内置模块以 `PluginManifest` 注册进 `PluginHost`（`source/backend/echo-agent/src/plugins.rs`），可启用/禁用/热加载，且纳入 persona 能力白名单语义。

1. **一切皆插件**：LLM provider、TurnRunner、工具、技能、插件实例、编排、management server 均为插件，无特权核心模块
2. 插件 = manifest + 生命周期钩子；注册是可逆副作用（disposer）
3. **热重载优先于重启**：数据类插件（技能/工具/清单）秒级热重载；代码类插件不热重载——provider / loop 经原子二进制替换 + 进程重启生效，插件实例实现作为进程外插件随 Core 重启重建
4. 插件**只依赖定义层**（echo-defs / echo-plugin / echo-context），不 import echo-agent，依赖方向单一

### 为什么不用动态库（.so）插件

Rust ABI 不稳定；`libloading` + C ABI 要求每个插件手写 extern "C" 桥，维护成本高、崩溃诊断难。因此采用**源码级插件 + 进程级热替换**：插件以 crate/目录形式存在，更新 = 重新构建二进制 + restart（复用自更新的原子替换机制）；热重载范畴限定为数据类插件。另一条通道是**进程外插件**（stdio 帧协议，与 ABI 无关），见「外部进程插件」。

### 边界与不做

- 不做动态库加载、不做插件沙箱（源码级插件即本仓库内代码，信任边界 = 仓库；进程外插件是**故障隔离**——独立 PID / 生命周期，不是安全沙箱）
- 不做插件市场/远程安装（需签名与供应链考虑）

## 插件模型

- 核心类型：`PluginManifest`（id/name/version/kind/entry/description + `builtin` / `runtime` / `package`）+ `BuiltinPlugin` + `MountContext`
- **插件类型（`PluginKind`，wire 名以 `as_str` 为准）**：`skill` / `tool` / `provider` / `loop` / `instance` / `orchestration` / `management` / `interaction`
  - `interaction`：**人在环交互面**——模型发起、用户在 Panel 应答（Panel 内的问答 UI + 专用应答通道），与 `instance` / `management` 同属「整类能力开关」，在 Panel「启用插件」勾选区按 kind 即可见（`capabilities.ts::isPluginCheckboxVisible` 的 kind 类判定）。原选单（`present_menu`）已废弃移除，该 kind 目前无内置成员，保留给未来人在环交互插件
- 注册为**可逆**副作用：`Plugin::mount` 返回 disposer（`PluginMountResult`，注册表持有、卸载时调用）；`register_and_mount` 注册并按启用态挂载（禁用插件只注册不挂载）
- 数据插件（skill/tool）支持热重载（插件目录 5s 轮询）；代码插件需二进制重载
- **启动顺序**：`apply_disabled` 先于挂载——禁用插件启动时只注册不挂载；persona 白名单的门控**启动期与运行期统一**由 `Agent::apply_capabilities` 承担（`GATED_PLUGIN_IDS` 表逐人格计算：全局启用 ∧ 白名单）；插件宿主由组合根注入为**进程级单例**（全体人格共享）
- **mount 实化**：可安全逆注册的插件（management.panel / tools.builtin / skills.dir / instance.qq / workspace / subagent）已把组合根装配搬进 mount 闭包，运行期 `TogglePlugin` 停/再启对它们有真实运行效果；`instance.qq` 的启动期 mount 不抢跑（wired 标志在插件实例接线完成后才置位），启动受 `[instances.qq].enabled` 与插件状态双重门控；`loop.{single,parallel}` 的按人格注入依赖 `AgentManager`（注册于挂载之后），启动期 mount 为 no-op——由人格启动循环**回填注入**（见 [Agent 循环](../core/core-agent-loop.md)「实化与分派」）
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
- 原 `run_sudo` / `present_menu` / `framework_update` 三个内联工具已废弃移除（见 [工具系统](./core-tools.md)「已废弃工具」）

### 外部进程插件

除源码级进程内插件外，系统支持**进程外插件**——插件以任意语言实现、经 stdio 帧协议与宿主通信（`runtime = subprocess`），装载源是 `plugins.toml`（文件不存在即特性休眠），崩溃自动重启（进程级隔离）。Core 自带的进程外组件走同一条路径：插件实例按 `[instances.<platform>.plugin]` 与实例表逐实例生成 `LaunchSpec`（`plugin_id = instance-<platform>-<instance>`、状态文件 `<platform>-<instance>.toml`，命令缺省 `echo-adapter-<platform>`），实例规格由 `instance_deploy` 解析；**平台部署细节（端口分配 / 容器名 / compose）在插件进程内**。

- **装载**：`plugins_runtime::start_with_specs` 把装载器筛出的每条 `LaunchSpec`（plugin_id / command / args / env / config）交给独立 `PluginSupervisor` 拉起，工具与实例贡献再接入 Core；Core 侧对这类插件只保留启停门控，不持有实现
- **与来源维度无关**：进程外可以是「manifest 编译内置 + 实现独立进程」（`instance.qq`），也可以是 manifest 与实现都来自外部；判据是 `runtime`，不是 `builtin`
- **与进程内插件的差别**：进程内 = 实现编译进 Core（挂载/卸载即生效）；进程外 = 独立 PID 与生命周期（崩了不带走 Core，启停经门控）
- **实例插件贡献什么**：`InstanceContribution`（实例名 / 平台 / 能力位 / 配置 schema / UI 描述符 / 会话标签 / **模型工具声明**）。宿主侧不认识任何平台细节——管理界面按描述符渲染、管理动作经 `invoke_custom(method, payload)` 字符串透传、**模型工具由插件声明**（宿主只做合并 / `account` 寻址 / 出站闸门 / 透传，见 [工具系统](./core-tools.md)「工具从哪来」）
- 协议、SDK 与部署方式见 [插件开发指南](./plugin-authoring.md)（含「实例插件」一节）；系统设计与路线图见 [完全解耦推进计划](../design/decoupling-plan.md)

## 内置插件清单

| id | kind | 运行形态 | 说明 |
| --- | --- | --- | --- |
| `echo-agent.tools.builtin` | Tool | 进程内 | 内置工具集（计算/搜索/编码/实例管理/供应商清单查询 `list_api_profiles`） |
| `echo-agent.instance.qq` | Instance | **进程外**（`subprocess`） | QQ 插件实例（OneBot v11 反向 WS，实现为独立进程 `echo-adapter-qq`，Core 侧只是门控壳；含 QQ 管理工具；门控机制见 [QQ 实例门控](./qq-gating.md)） |
| `echo-agent.skills.dir` | Skill | 进程内 | SKILL.md 技能目录（热重载） |
| `echo-agent.workspace` | Tool | 进程内 | 工作区会话管理（workspace 工具包：多会话/多目录管理、git 状态、**只读文件浏览器**（`RequestWorkspaceFiles`）；**激活 = 进入项目对话通道**——本地对话切换 + 系统提示词注入，见 [多 Agent 与会话](../core/core-agents.md)§工作区会话与项目通道；Panel 入口行「工作区」面板） |
| `echo-agent.provider.llm` | Provider | 进程内 | LLM 提供方工厂（名义挂载：重启生效） |
| `echo-agent.loop.single` | Loop | 进程内 | 单会话循环（默认）：启用时 echo-loop 驱动接管普通输入（启动期回填；生效口径见 [Agent 循环](../core/core-agent-loop.md)）；会话内 turn 串行排队、无会话管理 UI |
| `echo-agent.loop.parallel` | Loop | 进程内 | 并行多会话循环：同一 `TurnRunner`（生效口径同 `loop.single`）；会话内可并发分支、显示会话管理 UI（与 single 互斥） |
| `echo-agent.subagent` | Tool | 进程内 | Subagent 委派：spawn_subagent 工具（隔离上下文子任务）+ 完成后 `<subagent_event>` hook 回灌 + subagent-delegation 技能（见 [Subagent 插件](./core-subagent.md)） |
| `echo-agent.management.panel` | Management | 进程内 | 管理面：management WS 桥接（禁用即 Panel 自锁，TogglePlugin 拒绝禁用） |

表的来源分两类，别混算：**框架内置 8 项**（`echo_agent::plugins::BUILTIN_PLUGIN_IDS: [&str; 8]`，`builtin = true`，实现同在 Core 进程内）与**插件实例 1 项**（`echo-agent.instance.qq`）——后者的 id 不是硬编码，由组合根按前缀 `echo_agent.instance.<平台名>` 声明（`core::instance_manifests::PLUGIN_ID_PREFIX`），新增平台只加配置不改框架。运行形态一列独立于来源：`instance.qq` 即「框架声明 manifest + 进程外实现」的现行样本。

联邦（`echo-federation`）为对等链路包——跨机工具调用、委派与只读查询，机制见 [联邦（多机）](../federation/federation.md)。

> **框架侧实现位置**：解耦计划 Phase 3 已把绝大多数内置插件外迁为独立 crate——`echo-plugin-provider-llm`（provider 工厂 + 计量/脱敏装饰）/ `echo-plugin-skills`（技能注册表 + Git 安装）/ `echo-plugin-workspace`（工作区会话与工具）/ `echo-plugin-subagent` / `echo-plugin-loop`（两个纯策略模式）/ `echo-plugin-management`（Panel 连接面）；工具子系统的跨包机制（`ToolRegistry`）在 `echo-tool`。**仍内联在 echo-agent 的**只剩 `packages/tools_builtin`（内置工具集）、`packages/instance_ops`（入站 hook 桥 + 通用实例命令域）、`packages/federation`（远程代理工具与查询适配）。公开路径保持原样（`echo_agent::subagent::…` 等经 lib.rs 兼容 re-export 指向新 crate）。完整清单与判定见 [完全解耦推进计划](../design/decoupling-plan.md) §Phase 3。
>
> QQ 工具（`send_*`/`get_*` 共 10 个）**不再由 Core 实现**：它们作为 `ToolContribution` 由插件进程申报（插件仓库 `crates/echo-adapter-qq/src/tools.rs`），Core 只做注册与门控（`packages/instance_ops` 的入站/出站桥）。2026-10 去适配器化时 `source/core/src/qq_tools.rs` 已删除——插件实例本就能声明工具，没有留给组合根的特例。

> 以上 id 也是更新器（`scripts/update.sh`）插件感知校验的核对清单——`expected_ids` 由**框架 8 项 + 前缀 `echo-agent.instance.`** 组成（不是逐个列实例 id）；选单（present_menu）已废弃移除，任务清单（checklist）为普通内置工具。

## 能力开关（per-persona）

- 每个插件 id 均可放入 agent 的 `enabled_plugins`（白名单；空表 = 全部启用）
- **插件黑名单 `disabled_plugins` 已移除**：与白名单语义重复（前端首次取消勾选即物化全量白名单），既有配置在加载期物化进白名单（见 [配置持久化](../core/core-config-persistence.md)）；运行期 `SaveTeam` 只写白名单
- **前端「启用插件」勾选区的成员规则**（两处 UI：智能体配置弹层 ⚙ 配置、设置视图智能体编辑器；判定单一来源 = Panel `capabilities.ts::isPluginCheckboxVisible`）：`kind ∈ {instance, management, interaction}` **∪** 下列包维度门控插件固定清单（`capabilities.ts::PACKAGE_GATED_PLUGIN_IDS`，与本表 `GATED_PLUGIN_IDS` 的 4 项逐项镜像）：`tools.builtin` / `skills.dir` / `workspace` / `subagent`。**插件实例不在清单内**——两侧都按 `kind == "instance"` 运行期推导（Core `Agent::gated_plugin_ids()` 遍历插件注册表；Panel 同判定的 `isPluginCheckboxVisible`），因此新增平台插件无需改任何清单。其余插件（`loop.*` 由循环模式分段控件管理、`provider.llm` 名义挂载重启生效）**不出现**在该勾选区。保存后写回 TOML
- **循环模式推导**（互斥，默认与兜底都是单会话）：见 [Agent 循环](../core/core-agent-loop.md)§循环模式（单一来源 `TeamMember::loop_mode()`）
- **包级聚合**：白名单里的插件 id 即包 id——一次勾选同时门控该插件的生命周期与同名包的工具、技能（QQ 包见「Package」节）
- **其余插件当前生效范围**：
  - `tools.builtin` / `skills.dir`：禁用 = 该包全部工具（skills.dir 为全部技能）对所有 persona 批量禁用（对 LLM 不可见），启用按各 persona 名单恢复——**仅作用于目标 persona 时用 Agent 配置弹层的勾选**（运行期双向、即时生效）
  - `instance.qq`：禁用 = 停止 QQ 插件实例进程 + QQ 工具包禁用；启用 = 启动 + 按名单恢复
  - `management.panel`：禁用 = 关闭 management WS（**注意自锁**：Panel 将断连，恢复需编辑 core.toml 的 `disabled_plugins` 移除该 id 后重启 Core）。**防自锁保护**：经 `TogglePlugin` 禁用它会被 Core 拒绝（Error 事件明示，状态不变）——禁用与恢复都只能走 core.toml + 重启
  - `loop.single` / `loop.parallel`：**纯策略插件**——只表达循环模式（单会话串行 / 并行多会话，由白名单推导 `LoopMode`），mount 无运行期资源。驱动本体（echo-loop `TurnRunner`）是唯一循环实现，组合根在人格启动与运行期新建时无条件注入，不随插件启停切换（单轨化后内置循环已删除）。详见 [Agent 循环](../core/core-agent-loop.md)「注入与分派」
  - `provider.llm`：仍为名义挂载——运行中替换 provider 涉及在途 turn，保持"重启生效"语义（禁用 = 下次重启不装配）
- **优先级**：全局禁用（`TogglePlugin` 卸载 / `[agent].disabled_tools|skills`）> persona 名单；全局重新启用不会越过 persona 名单，hook 后由 `Agent::reapply_*` 重算
- **任务清单的会话隔离**：`ChecklistTool` 状态按**会话**隔离（session_id → 清单名 → 项）——同一 persona 的不同会话（local:tui / qq:group:…）的清单互不可见、互不覆盖；会话键由 `Agent::run_tool` 在参数校验后注入 `__session_id`（联邦远程/无会话上下文调用落 `__global` 兜底键）。事件 `ChecklistUpdated` 带 `team_id`，Panel 按 `(core, team_id, session_id)` 复合键存储并显示
- **内置工具包覆盖**：任务清单（checklist）已降级为普通内置工具（随 `tools.builtin` 包，见上表注）；关闭内置工具集 = checklist 一并不可用，单独停用请在「启用工具」里按名勾选（运行期双向、即时生效），Panel 入口行同步移除「清单」入口、关闭已打开浮层并清空徽标状态；`base` 人格演示了"纯对话"配置（禁用 tools.builtin + skills.dir）

### 同步清单

新增**包维度门控插件**时，三处必须同步（缺一即「配置里看不到/门控失效」）：

1. Core `plugins.rs` 的 `GATED_PLUGIN_IDS` 与 `BUILTIN_PLUGIN_IDS`
2. 更新器 `scripts/update.sh` 的插件校验 `expected_ids`
3. Panel `capabilities.ts::PACKAGE_GATED_PLUGIN_IDS`（勾选区可见性）与 `PACKAGE_DISPLAY_NAMES`（包显示名）

三项均有测试/校验守护。新增**插件类型**（`PluginKind`）时同理：Core `PluginKind::as_str` 的 wire 名、Panel `capabilities.ts::PLUGIN_KIND_LABELS` 中文名（测试守护覆盖全部 wire 名）、`isPluginCheckboxVisible` 的 kind 类判定（若属「整类能力开关」）。

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
