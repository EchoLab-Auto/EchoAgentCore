---
id: panel-settings-plugins
title: "设置 · 插件"
group: 前端
x: 2880
y: 1060
---

# 设置 · 插件

> **定位**：插件工作台——在哪个 Core 上管理（管理节点）、实现在不在 Core 进程内（内部 / 外部）、它门控了什么（门控语义），以及受保护插件与包规模统计。工作台骨架见[设置视图](./panel-settings.md)「资源工作区通用契约」。读者：维护插件相关界面的开发者；相关：[插件化设计](../plugins/core-plugins.md)（插件模型与 Package）、[设置 · 工具](./panel-settings-tools.md) / [设置 · 技能](./panel-settings-skills.md)（成员条目）。

## 管理节点

插件工作台回答三件事：在哪个 Core 上管理、这条插件的实现在不在 Core 进程内、它门控了什么——第一个决定列表内容，第二个是默认分组维度，第三个是可切换的语义维度。

- **管理节点**（筛选栏首项，`NeumorphismSelect`，`aria-label="管理节点"`）：选项 = 已接入上游名 ∪ 人格归属 core ∪ `state.pluginsByCore` 的键 ∪ `activeRegion`（`pluginCoreOptions`），标签经 `state.ts::regionLabel`（区域显示名，缺失回落节点 id）；切换即清空选中并定向 `RequestPluginsList`（带目标 core）
- **显示的 = 请求的 = 修改的**：列表、筛选计数、副标题统计、行内快速启停与 `TogglePlugin` 的目标 core 统一取 `pluginCoreEffective`
- **按节点归档的清单**（`state.pluginsByCore` / `pluginCatalog`）：各节点回推的插件按上游名归档（中继不加信封的单上游场景键为空串）；`pluginCatalog` 的取数顺位 = 显式选定 / `activeRegion` 的归档 → **唯一来源**（键对不上但只有一份数据）→ `state.plugins`（管理目标兼容视图）；多来源且该节点尚未回推时给空列表，**绝不回退**（否则串显别个节点）

## 分组维度

- **分组维度**（`PLUGIN_GROUP_OPTIONS` 分段控件）：`内部 / 外部`（默认）/ `按门控语义` / `按 kind` / `平铺`
- **内部 / 外部按运行位置判定**（`pluginSource`）：`runtime === 'subprocess'` → 外部，`in_process` → 内部；旧 Core 未回推 `runtime` 时回退 `builtin`（`false` → 外部）。判据是「实现在哪跑」而非「manifest 从哪来」——QQ 插件实例随 Core 发布（`builtin = true`）但实现在独立进程，因此归**外部插件**
- **组头提示**：外部组 = 「实现在独立进程（独立 PID / 生命周期）；随 Core 发布或来自插件目录」（讲清「随 Core 发布 ≠ 进程内」）；内部组 = 「实现编译在 Core 进程内；随 Core 发布」

**门控语义分组**（`pluginSemantic`；把影响面最大的组提到最上并带组头提示）：

| 分组 | 成员判定 | 组头提示 |
|---|---|---|
| 包维度门控 | `PACKAGE_GATED_PLUGIN_IDS`（非 instance / management / loop） | 禁用 = 其包内全部工具/技能对所有智能体不可见 |
| 平台通道 | `kind ∈ {instance, management}` | 平台连接与管理通道；管理面禁用会切断 Panel 通信（受保护） |
| 模式插件（互斥） | `kind === 'loop'` | 每智能体的循环模式在「智能体」配置里选；此处全局停用 = 卸载 echo-loop 驱动 |
| 其它 | 其余（如 provider.llm） | — |

## 行卡与受保护插件

- **行卡标记**：`独立进程`（`pluginIsSubprocess`，与外部组等价）· 包规模徽标（`N 工具 · M 技能`，经 `pluginMembers` → `packageMembers` 从 `state.plugins` 与 `state.tools`/`state.skills` 反查——这两份是单份状态、只随管理目标节点更新，故切到非管理目标节点时徽标仍按管理目标口径统计）· `🔒 受保护`（management.panel，**行内渲染锁标记而不给开关**）· `模式`（loop.*）
- **副标题随分组维度**：按来源 = `共 N 项 · E 外部插件 · I 内部插件 · D 已禁用`；语义分组 = `共 N 项 · G 包维度门控 · M 模式插件 · D 已禁用`
- **启停**：`TogglePlugin` 禁用即卸载注册；在选定管理节点上全局生效；**「管理面」插件禁用被保护**——core 拒绝 + UI 明示且行内不给开关，防 Panel 自锁断连。**包级聚合**：包内工具与技能随包一起启停（见[插件化设计](../plugins/core-plugins.md)「Package」）

## 详情分区

1. **这个插件带来什么**：包内**工具 / 技能 chips，点击跳转到对应分类并选中**
2. **被谁使用**：`enabled_plugins` 白名单反查（「已启用」/「未列入白名单」）
3. **启停影响**：按门控语义分派文案；受保护插件为警示色说明如何在 core.toml 真正禁用
4. **技术信息**（**折叠**）：承载 ID / Package / kind·版本 / Entry / **运行形态** / Author

头部左侧是语义标签（`PLUGIN_SEMANTIC_LABELS`），外部插件另加 `外部插件 · 独立进程` 标签。`PLUGIN_KIND_LABELS.loop` 为「循环驱动」（全站统一的中文 kind 名）。

## 协议契约

`PluginInfo.runtime`（`in_process` / `subprocess`，缺失 = 进程内）与 `PluginInfo.builtin`（manifest 是否编译进 Core，缺失按 `true` 兼容）均由 Core 回推、彼此正交；来源分组（`pluginSource`）与运行形态标签（`pluginRuntimeLabel`，`PLUGIN_RUNTIME_LABELS`：独立进程 / 进程内）同源于这两个字段，Panel 不做推断性改写。

## 交叉跳转

`jumpToCategory`：插件详情成员 chip → 工具 / 技能分类并选中该条目；工具详情「所属包」按钮 → 插件分类并选中该插件——把「包 ⇄ 工具/技能」三层关系做成双向可点链路。包规模徽标与成员 chip 的成员名单取自 `state.tools` / `state.skills`（管理目标视图），切到非管理目标的节点时仍按管理目标统计。
