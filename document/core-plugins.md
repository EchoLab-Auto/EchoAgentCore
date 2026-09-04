---
id: plugins
title: "插件化设计"
group: 后端模块
link: ["adapter-qq-gating | QQ 适配器（插件）"]
x: 970
y: 1385
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

## 插件模型

- 核心类型：`PluginManifest`（id/name/version/kind/entry/description）+ `BuiltinPlugin` + `MountContext`
- 注册为**可逆**副作用：`register_and_mount` 返回 disposer，禁用即卸载注册
- 数据插件（skill/tool）支持热重载（插件目录 5s 轮询）；代码插件需二进制重载
- **启动顺序**：`apply_disabled` 先于挂载——禁用插件启动时只注册不挂载；persona 白名单的启动期门控按 `GATED_PLUGIN_IDS` 表逐人格批量禁用
- **mount 实化**：可安全逆注册的插件（management.panel / tools.builtin / adapter.qq / skills.dir）已把组合根装配搬进 mount 闭包，`TogglePlugin` 对它们有真实运行效果；`adapter.qq` 的启动期 mount 不抢跑（wired 标志在适配器接线完成后才置位），启动受 `[adapters.qq].enabled` 与插件状态双重门控

## 内置插件清单

| id | kind | 说明 |
| --- | --- | --- |
| `echo-agent.tools.builtin` | Tool | 内置工具集（计算/搜索/清单/编码/适配器管理） |
| `echo-agent.adapter.qq` | Adapter | QQ 适配器（OneBot v11 反向 WS，含 QQ 管理工具） |
| `echo-agent.skills.dir` | Skill | SKILL.md 技能目录（热重载） |
| `echo-agent.orchestration` | Orchestration | 后台任务/并行分支/子代理/定时器/自更新 |
| `echo-agent.provider.llm` | Provider | LLM 提供方工厂 |
| `echo-agent.loop.runner` | Loop | turn/step 状态机与工具管道 |
| `echo-agent.management.panel` | Management | Panel 桥接/sudo 授权通道 |
| `echo-agent.orchestration.single` | Orchestration | **编排模式（互斥）**：单任务编排——无会话管理 UI（会话卡/全局分组隐藏）、回执分支不可见 |
| `echo-agent.orchestration.chatbot` | Orchestration | **编排模式（互斥）**：多任务并行编排——会话列表/全局会话/可见回执分支（取代旧 `branch.reply`/`session.global`/`chatbot.sessions` 三插件） |

## 能力开关（per-persona）

- 每个插件 id 均可放入 agent 的 `enabled_plugins`（白名单）或 `disabled_plugins`（黑名单）
- 白名单非空 = 只启用列出的插件；黑名单优先
- 前端设置视图可按 kind 勾选（adapter/management 类），保存后写回 TOML
- **编排模式推导**（互斥，single 为兜底；单一来源 `TeamMember::orchestration_mode()`）：
  - `enabled_plugins` 为空（=全部启用）→ **chatbot**
  - 含 `orchestration.chatbot`（或旧特性 id，加载期自动迁移）→ **chatbot**；两者并含 chatbot 优先（迁移时 warn）
  - 非空但无任何模式 id → **single**
  - `disabled_plugins` 含 chatbot/旧 id → **single**；含 single id → no-op（禁用兜底无意义，warn）
  - chatbot：会话卡/全局分组/ReplyBranch* 事件发射全开；single：全部关闭（分支照常执行合并，仅不可见——可见性开关而非执行开关）
- **其余插件当前生效范围**：
  - `tools.builtin` / `skills.dir`：禁用 = 该包全部工具/技能对所有 persona 批量禁用（对 LLM 不可见），启用恢复
  - `adapter.qq`：禁用 = 停止 QQ 适配器进程 + QQ 工具包禁用；启用 = 启动 + 恢复
  - `management.panel`：禁用 = 关闭 management WS（**注意自锁**：Panel 将断连，恢复需编辑 core.toml 的 `disabled_plugins` 移除该 id 后重启 Core）。**防自锁保护**：经 `TogglePlugin` 禁用它会被 Core 拒绝（Error 事件明示，状态不变）——禁用与恢复都只能走 core.toml + 重启
  - `provider.llm` / `loop.runner` / `orchestration`：仍为名义挂载——运行中替换 provider/loop 涉及在途 turn，保持"重启生效"语义（禁用 = 下次重启不装配）；orchestration 的实化依赖 echo-loop 迁移完成度

## 动态编排工具

- 编排类工具（`schedule_timer`、`run_subagent`、`spawn_background_task`、`spawn_parallel_task`、`framework_update`、`run_sudo` 等）由 loop 内联调度，按 persona 白名单过滤（`allows_dynamic_tool`）
- 工具名表 `ORCHESTRATION_TOOL_NAMES` 由单元测试守护与 schema 一致（`framework_update`/`run_sudo` 因另有配置门控不在表内）

## 用户扩展方式

- **技能**：在 skills_dir 放置 `SKILL.md`（带 frontmatter：name/description/keywords/always/category/package），运行时自动发现、热重载
- **数据插件**：在 plugins_dir 放置 `plugins/{kind}/{id}/plugin.toml`（skill/tool kind），5s 轮询热加载；记录 manifest 哈希做内容 diff，变化即 unmount+remount；启用状态持久化于 `[agent].disabled_plugins`
- **代码插件**（provider/loop/adapter）：需修改源码并走自更新流程

## 待办

- `framework_update` 补 `action=plugins`（列出/启停插件，复用 TogglePlugin 授权语义，与 status 的插件摘要共用 `gather_plugin_summary`）
