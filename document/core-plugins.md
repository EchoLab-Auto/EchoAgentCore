---
id: plugins
title: "插件化设计"
group: 扩展系统
link: ["adapter-qq-gating | QQ 适配器（插件）", "core-skills | 技能系统 | r>l", "tools | r>l", "agent-loop | r>l", "subagent | Subagent 插件"]
x: 955
y: 1900
---

# 插件化设计

插件化是 Core 的组合方式：每个内置模块以 `PluginManifest` 注册进 `PluginHost`（`source/backend/echo-agent/src/plugins.rs`），有权启用/禁用/热加载，且纳入 persona 能力白名单语义。

## 设计原则

1. **一切皆插件**：LLM provider、TurnRunner、工具、技能、适配器、编排、management server 均为插件，无特权核心模块
2. 插件 = manifest + 生命周期钩子；注册是可逆副作用（disposer）
3. **热重载优先于重启**：数据类插件（技能/工具/清单）秒级热重载；代码类插件（provider/loop/适配器）经原子二进制替换 + 进程重启生效
4. 插件**只依赖定义层**（echo-defs / echo-plugin / echo-context），不 import echo-agent，依赖方向单一

### 为什么不用动态库（.so）插件

Rust ABI 不稳定；`libloading` + C ABI 要求每个插件手写 extern "C" 桥，维护成本高、崩溃诊断难。因此采用**源码级插件 + 进程级热替换**：插件以 crate/目录形式存在，更新 = 重新构建二进制 + restart（复用自更新的原子替换机制）；热重载范畴限定为数据类插件。

### 边界与不做

- 不做动态库加载、不做插件沙箱（源码级插件即本仓库内代码，信任边界 = 仓库）
- 不做插件市场/远程安装（需签名与供应链考虑）

> **外部进程插件（2026-10 起）**：除上述源码级内置插件外，系统现已支持
> **独立进程插件**——插件以任意语言实现、经 stdio 帧协议与宿主通信，由
> `plugins.toml` 配置装载（缺省休眠），崩溃自动重启（进程级隔离）。
> 协议、SDK 与部署方式见 [插件开发指南](./plugin-authoring.md)；系统设计
> 与路线图见 [完全解耦推进计划](./decoupling-plan.md)。两者关系：内置
> 插件 = 编译期内联（性能零开销、随二进制更新）；外部插件 = 运行期装载
> （隔离、独立演进、免重编译 Core）。

## 插件模型

- 核心类型：`PluginManifest`（id/name/version/kind/entry/description）+ `BuiltinPlugin` + `MountContext`
- **插件类型（`PluginKind`，wire 名以 `as_str` 为准）**：`skill` / `tool` / `provider` / `loop` / `adapter` / `orchestration` / `management` / `interaction`。
  - `interaction`（2026-09-14 新增）：**人在环交互面**——模型发起、用户在 Panel 应答（Panel 内的问答 UI + 专用应答通道），与 `adapter` / `management` 同属「整类能力开关」，在 Panel「启用插件」勾选区按 kind 即可见（`capabilities.ts::isPluginCheckboxVisible` 的 kind 类判定）。原选单（`present_menu`）已于 2026-09 整体废弃移除，该 kind 目前无内置成员，保留给未来人在环交互插件
  - 新增类型时同步：Core `PluginKind::as_str` 的 wire 名（测试守护）+ Panel `capabilities.ts::PLUGIN_KIND_LABELS` 中文名（测试守护覆盖全部 wire 名）
- 注册为**可逆**副作用：`Plugin::mount` 返回 disposer（`PluginMountResult`，注册表持有、卸载时调用）；`register_and_mount` 注册并按启用态挂载（禁用插件只注册不挂载）
- 数据插件（skill/tool）支持热重载（插件目录 5s 轮询）；代码插件需二进制重载
- **启动顺序**：`apply_disabled` 先于挂载——禁用插件启动时只注册不挂载；persona 白名单的门控**启动期与运行期统一**由 `Agent::apply_capabilities` 承担（`GATED_PLUGIN_IDS` 表逐人格计算：全局启用 ∧ 白名单）；插件宿主由组合根注入为**进程级单例**（所有人格共享，不再寄居某个“默认人格”）
- **mount 实化**：可安全逆注册的插件（management.panel / tools.builtin / skills.dir / adapter.qq / workspace / subagent）已把组合根装配搬进 mount 闭包，运行期 `TogglePlugin` 停/再启对它们有真实运行效果；`adapter.qq` 的启动期 mount 不抢跑（wired 标志在适配器接线完成后才置位），启动受 `[adapters.qq].enabled` 与插件状态双重门控；`loop.{single,parallel}` 的按人格注入依赖 `AgentManager`（注册于挂载之后），启动期 mount 为 no-op——由人格启动循环**回填注入**（2026-10 接线；见 [Agent 循环](./core-agent-loop.md)「实化与分派」）
- **运行期热更新（2026-09）**：插件/工具/技能勾选在 `SaveTeam` 保存后**立即生效**，无需重启——工具/技能逐名双向应用（取消勾选即禁用、重新勾选即恢复）；全局 `TogglePlugin` 经 mount/unmount 闭包逐 persona 重评估（`reapply_plugin_gating`：全局启用 ∧ persona 名单，名单外不放开、unmount 对全员生效）；`ToggleTool`/`ToggleSkill` 同样逐 persona 重算（`reapply_tool_gating` / `reapply_skill_gating`：persona 黑名单不被全局启用覆盖）

## 内置插件清单

| id | kind | 说明 |
| --- | --- | --- |
| `echo-agent.tools.builtin` | Tool | 内置工具集（计算/搜索/编码/适配器管理） |
| `echo-agent.adapter.qq` | Adapter | QQ 适配器（OneBot v11 反向 WS，含 QQ 管理工具） |
| `echo-agent.skills.dir` | Skill | SKILL.md 技能目录（热重载） |
| `echo-agent.workspace` | Tool | 工作区会话管理（workspace 工具包：多会话/多目录管理、git 状态、**只读文件浏览器**（`RequestWorkspaceFiles`，2026-09-15）；**激活 = 进入项目对话通道**——本地对话切换 + 系统提示词注入，见 [多 Agent 与会话](./core-agents.md)§工作区会话与项目通道；Panel 入口行「工作区」面板） |
| `echo-agent.provider.llm` | Provider | LLM 提供方工厂（名义挂载：重启生效） |
| `echo-agent.loop.single` | Loop | 单会话循环（默认）：启用时 echo-loop 驱动接管普通输入（启动期回填；生效口径见 [Agent 循环](./core-agent-loop.md)）；会话内 turn 串行排队、无会话管理 UI |
| `echo-agent.loop.parallel` | Loop | 并行多会话循环：同一 `TurnRunner`（生效口径同 `loop.single`）；会话内可并发分支、显示会话管理 UI（与 single 互斥） |
| `echo-agent.subagent` | Tool | Subagent 委派：spawn_subagent 工具（隔离上下文子任务）+ 完成后 `<subagent_event>` hook 回灌 + subagent-delegation 技能（见 [Subagent 插件](./core-subagent.md)） |
| `echo-agent.management.panel` | Management | 管理面：management WS 桥接（禁用即 Panel 自锁，TogglePlugin 拒绝禁用） |

> **框架侧实现位置（2026-09-28 重组，2026-09-29 收编 tool/）**：echo-agent crate 内
> 按插件分包到 `src/packages/<name>/`，**第一层分类 = 包归属**——`tools_builtin`
> （内置工具）/ `skills_dir`（技能注册表 + Git 安装）/ `adapter_qq`（入站 hook 桥 +
> QQ 命令域）/ `workspace`（会话存储/工具 + 命令域）/ `subagent` / `provider_llm`
> （provider 工厂）；另有特例 `packages/tool/` = 工具子系统的**跨包机制**
> （`ToolRegistry`：注册/可逆注销/逐名与按包启停/schema 缓存，被各包共用，非插件）。`loop.{single,parallel}` 的
> 实现是 `echo-loop` crate（echo-agent 只做驱动接线）；`management.panel` 是
> echo-protocol（线协议，见 [协议与数据流](./protocol.md)）+ echo-agent-core
> （`source/core/src/management.rs`）。目录只服务物理区分，公开路径保持原样
> （`echo_agent::subagent::…` 等经 lib.rs 兼容 re-export）。
>
> QQ 工具（`send_*`/`get_*`）例外地实现于组合根 `source/core/src/qq_tools.rs`——
> 工具须实现 `echo_agent::Tool`，而适配器 crate 刻意不依赖 echo-agent。

> 以上 9 个 id 也是更新器（`scripts/update.sh`）插件感知校验的核对清单（`BUILTIN_PLUGIN_IDS` 为 `[&str; 9]`）。选单（present_menu）已废弃移除（2026-09）；任务清单（checklist）为普通内置工具。

## 能力开关（per-persona）

- 每个插件 id 均可放入 agent 的 `enabled_plugins`（白名单；空表 = 全部启用）
- **插件黑名单 `disabled_plugins` 已于 2026-09-11 移除**：与白名单语义重复（前端首次取消勾选即物化全量白名单），既有配置在加载期物化进白名单（见 [配置持久化](./core-config-persistence.md)）；运行期 `SaveTeam` 只写白名单
- **前端「启用插件」勾选区的成员规则**（两处 UI：智能体配置弹层 ⚙ 配置、
  设置视图智能体编辑器；判定单一来源 = Panel `capabilities.ts::isPluginCheckboxVisible`）：
  `kind ∈ {adapter, management, interaction}` **∪** 下列包维度门控插件固定清单
  （`capabilities.ts::PACKAGE_GATED_PLUGIN_IDS`，与本节 `GATED_PLUGIN_IDS` 逐项镜像）：
  `tools.builtin` / `skills.dir` / `workspace` / `adapter.qq` / `subagent`。
  其余插件（`loop.*` 由循环模式分段控件管理、`provider.llm`
  名义挂载重启生效）**不出现**在该勾选区。保存后写回 TOML
- ⚠️ **新增包维度门控插件时的同步清单**（缺一即"配置里看不到/门控失效"）：
  ① Core `plugins.rs` 的 `GATED_PLUGIN_IDS` 与 `BUILTIN_PLUGIN_IDS`；
  ② 更新器 `scripts/update.sh` 的插件校验 `expected_ids`；
  ③ Panel `capabilities.ts::PACKAGE_GATED_PLUGIN_IDS`（勾选区可见性）
  与 `PACKAGE_DISPLAY_NAMES`（包显示名）。
  ①②③ 均有测试/校验守护；历史事故：workspace 插件仅同步了 ①②，
  导致智能体配置的「启用插件」中不可见（2026-09-13 修复）。
  另：新增**插件类型**（`PluginKind`）时同步 Panel `PLUGIN_KIND_LABELS`
  （中文显示名，测试覆盖全部 wire 名）与 `isPluginCheckboxVisible`
  的 kind 类判定（若属「整类能力开关」）——见「插件模型」
- **循环模式推导**（互斥，默认与兜底都是单会话）：见[Agent 循环](./core-agent-loop.md)§循环模式（单一来源 `TeamMember::loop_mode()`）
- **包级聚合**：白名单里的插件 id 即包 id——一次勾选同时门控该插件的
  生命周期与同名包的工具、技能（QQ 包见下「Package」章节）
- **其余插件当前生效范围**：
  - `tools.builtin` / `skills.dir`：禁用 = 该包全部工具（skills.dir 为全部技能）对所有 persona 批量禁用（对 LLM 不可见），启用按各 persona 名单恢复——**仅作用于目标 persona 时用 Agent 配置弹层的勾选**（运行期双向、即时生效）
  - `adapter.qq`：禁用 = 停止 QQ 适配器进程 + QQ 工具包禁用；启用 = 启动 + 按名单恢复
  - `management.panel`：禁用 = 关闭 management WS（**注意自锁**：Panel 将断连，恢复需编辑 core.toml 的 `disabled_plugins` 移除该 id 后重启 Core）。**防自锁保护**：经 `TogglePlugin` 禁用它会被 Core 拒绝（Error 事件明示，状态不变）——禁用与恢复都只能走 core.toml + 重启
  - `loop.single` / `loop.parallel`：**已实化**——mount 注入 TurnRunner 并置位 echo-loop 驱动；两者 mount 同一驱动（模式只改策略），全部卸载才回退内置循环。**注入的生效口径**（2026-10 接线后）：启动期由人格启动循环按注册表启用态**回填注入**（mount 闭包依赖的 `AgentManager` 在挂载时尚未注册，故 mount 为 no-op）；运行期新建人格由 `set_agent_factory` 包装同口径注入；插件停/再启按原 mount/Disposer 联动（全部卸载回退内置循环）。注入生效后普通输入走 turn/step 状态机（QQ hook/定时器/QQ 会话仍走内置循环）。详见 [Agent 循环](./core-agent-loop.md)「实化与分派」
  - `provider.llm`：仍为名义挂载——运行中替换 provider 涉及在途 turn，保持"重启生效"语义（禁用 = 下次重启不装配）
- **优先级**：全局禁用（`TogglePlugin` 卸载 / `[agent].disabled_tools|skills`）> persona 名单；全局重新启用不会越过 persona 名单，hook 后由 `Agent::reapply_*` 重算
- **任务清单的会话隔离（2026-10）**：`ChecklistTool` 状态按**会话**隔离（session_id → 清单名 → 项）——同一 persona 的不同会话（local:tui / qq:group:…）的清单互不可见、互不覆盖；会话键由 `Agent::run_tool` 在参数校验后注入 `__session_id`（联邦远程/无会话上下文调用落 `__global` 兜底键）。事件 `ChecklistUpdated` 带 `team_id`，Panel 按 `(core, team_id, session_id)` 复合键存储并显示。此前状态是进程内单实例（不分会话），A 会话的条目会被 B 会话看到并覆盖。
- **内置工具包覆盖**：任务清单（checklist）已于 2026-09 降级为普通内置工具（随 `tools.builtin` 包，见上表注）；关闭内置工具集 = checklist 一并不可用，单独停用请在「启用工具」里按名勾选（运行期双向、即时生效），Panel 入口行同步移除「清单」入口、关闭已打开浮层并清空徽标状态；`base` 人格演示了"纯对话"配置（禁用 tools.builtin + skills.dir）

## Package（包）——横跨 plugin + tool + skill 的标签

**Package 是一等标签**（非实体、无独立 manifest），把三种异构成员聚合成
一个可整体启停的能力单元。三种载体各自声明归属，按**同一个字符串**聚合：

| 维度 | 声明方式 | 示例（QQ 包） |
| --- | --- | --- |
| Plugin | `PluginManifest.package`（缺省 = 插件 id 自身，`package_id()` 归一） | `echo-agent.adapter.qq` |
| Tool | `ToolRegistry::set_package(name, pkg)`（装配期打标） | QQ 工具（send_*/get_*） |
| Skill | `SKILL.md` frontmatter `package:` | qq-management / qq-transport |

**包级门控语义**（`Agent::apply_plugin_gating`，横跨两个注册表）：

- 禁用包 → 包内**工具与技能一起**关闭（对 LLM 不可见）+ 插件自身生命周期
  副作用（如 adapter.qq 停进程）；包外成员不受影响
- 启用包 → 两者一起恢复，但按 persona 白名单**收紧**（名单外的
  成员保持禁用；全局禁用优先）
- `skills.dir` 插件是**整表**语义（全部技能），其余包按同名 `package:` 精确匹配
- persona 白名单里的插件 id 即包 id——勾选/取消勾选一个插件 = 勾选/取消
  整个包（plugin + tools + skills）

**可观测性**：`PluginInfo.package` 随 `PluginsList` 下发（未声明回退为插件
id），Panel 插件详情展示「包（Package）」字段；设置视图已按包分组展示
工具/技能。

**多插件包（前瞻）**：`PluginManifest.with_package()` 允许一个包绑定多个
插件；当前全部内置插件均为「插件 id = 包 id」的单插件包，门控按插件 id
传播即可（QQ 包为现行示例）。

## 内联分派工具

- `spawn_subagent` 由 loop 内联分派（异步受理），按 persona 白名单过滤
  （`allows_dynamic_tool`）；schema 由注册表提供（组合根装配时注册）。
  原 `run_sudo` / `present_menu` / `framework_update` 三个内联工具已于
  2026-09 废弃移除（见 [工具系统](./core-tools.md)「已废弃工具」）。

## 用户扩展方式

- **技能**：在 skills_dir 放置 `SKILL.md`（带 frontmatter：name/description/keywords/always/category/package），运行时自动发现、热重载
- **数据插件**：在 plugins_dir 放置 `plugin.toml`（skill/tool kind）——`plugins/{kind}/{id}/plugin.toml` 只是约定布局，实现实际是 WalkDir 递归扫描 plugins_dir 下任意 `plugin.toml`（`agent/mod.rs` `reload_data_plugins`），不限三层；5s 轮询热加载（按插件 id 挂载/卸载；**manifest 内容 diff（name/version/description/author/package 任一变化）触发重挂载**——unmount + 重注册 + mount，2026-10 落地，判定逻辑 `data_plugin_stale` 有单测）；启用状态持久化于 `[agent].disabled_plugins`
- **代码插件**（provider/loop/adapter）：需修改源码并走自更新流程
