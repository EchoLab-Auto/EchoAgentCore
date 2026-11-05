---
id: settings-modules
title: "设置 · 模块"
group: 前端
x: 3200
y: 2600
---

# 设置 · 模块

> **定位**：模块工作台——在哪个 Core 上管理（管理节点）、实现在不在 Core 进程内（内部 / 外部）、它门控了什么（门控语义），以及受保护模块与包规模统计。工作台骨架见[设置视图](./settings.md)「资源工作区通用契约」。读者：维护模块相关界面的开发者；相关：[模块化设计](../modules/modules.md)（模块模型与 Package）、[设置 · 工具](./settings-tools.md) / [设置 · 技能](./settings-skills.md)（成员条目）。

## 管理节点

模块工作台回答三件事：在哪个 Core 上管理、这条模块的实现在不在 Core 进程内、它门控了什么——第一个决定列表内容，第二个是默认分组维度，第三个是可切换的语义维度。

- **管理节点**（筛选栏首项，`NeumorphismSelect`，`aria-label="管理节点"`）：选项 = 已接入上游名 ∪ 人格归属 core ∪ `state.modulesByCore` 的键 ∪ `activeRegion`（`moduleCoreOptions`），标签经 `state.ts::regionLabel`（区域显示名，缺失回落节点 id）；切换即清空选中并定向 `RequestModulesList`（带目标 core）
- **显示的 = 请求的 = 修改的**：列表、筛选计数、副标题统计、行内快速启停与 `ToggleModule` 的目标 core 统一取 `moduleCoreEffective`
- **按节点归档的清单**（`state.modulesByCore` / `moduleCatalog`）：各节点回推的模块按上游名归档（中继不加信封的单上游场景键为空串）；`moduleCatalog` 的取数顺位 = 显式选定 / `activeRegion` 的归档 → **唯一来源**（键对不上但只有一份数据）→ `state.modules`（管理目标兼容视图）；多来源且该节点尚未回推时给空列表，**绝不回退**（否则串显别个节点）

## 分组维度

- **分组维度**（`MODULE_GROUP_OPTIONS` 分段控件）：`内部 / 外部`（默认）/ `按门控语义` / `按 kind` / `平铺`
- **内部 / 外部按运行位置判定**（`moduleSource`）：`runtime === 'subprocess'` → 外部，`in_process` → 内部；旧 Core 未回推 `runtime` 时回退 `builtin`（`false` → 外部）。判据是「实现在哪跑」而非「manifest 从哪来」——QQ 模块实例随 Core 发布（`builtin = true`）但实现在独立进程，因此归**外部模块**
- **组头提示**：外部组 = 「实现在独立进程（独立 PID / 生命周期）；随 Core 发布或来自模块目录」（讲清「随 Core 发布 ≠ 进程内」）；内部组 = 「实现编译在 Core 进程内；随 Core 发布」

**门控语义分组**（`moduleSemantic`；把影响面最大的组提到最上并带组头提示）：

| 分组 | 成员判定 | 组头提示 |
|---|---|---|
| 包维度门控 | Core 下发 `ModuleInfo.gating === "persona"`（`kind ∈ {tool, skill, instance}`） | 禁用 = 该 persona 下其包内全部工具/技能不可见（`enabled_modules` 白名单） |
| 进程级（只读展示） | `gating === "process"` 且 `kind ∈ {management, interaction}` | 恒定生效、勾不动；显示成置灰项 + 「· 进程级」标签，避免"这能力不存在"的误读 |
| 平台通道 | `kind ∈ {instance, management}` | 平台连接与管理通道；管理面禁用会切断 Panel 通信（受保护） |
| 模式模块（互斥） | `kind === 'loop'` | 每智能体的循环模式在「智能体」配置里选；此处全局停用 = 卸载 echo-loop 驱动 |
| 其它 | 其余（如 provider.llm） | — |

## 行卡与受保护模块

- **行卡标记**：`独立进程`（`moduleIsSubprocess`，与外部组等价）· 包规模徽标（`N 工具 · M 技能`，经 `moduleMembers` → `packageMembers` 从 `state.modules` 与 `state.tools`/`state.skills` 反查——这两份是单份状态、只随管理目标节点更新，故切到非管理目标节点时徽标仍按管理目标口径统计）· `🔒 受保护`（management.panel，**行内渲染锁标记而不给开关**）· `模式`（loop.*）
- **副标题随分组维度**：按来源 = `共 N 项 · E 外部模块 · I 内部模块 · D 已禁用`；语义分组 = `共 N 项 · G 包维度门控 · M 模式模块 · D 已禁用`
- **启停**：`ToggleModule` 禁用即卸载注册；在选定管理节点上全局生效；**「管理面」模块禁用被保护**——core 拒绝 + UI 明示且行内不给开关，防 Panel 自锁断连。**包级聚合**：包内工具与技能随包一起启停（见[模块化设计](../modules/modules.md)「Package」）
- **运行态可见**：`ModuleInfo.state`（`pending` / `ready` / `draining` / `stopped` / `failed`）+ `restarts` + `generation`。行卡只在**异常态**出徽标（`失败`红 / `启动中`/`停止中`中性，`ready` 不喊）；技术信息给出「运行态 · 已自动重启 N 次 · 代际 M」。这条与 `enabled` 正交：`enabled = true` + `state = failed` 是「配置开了、进程起不来」
- **操作回执**：`ModuleOpResult { id, op, ok, message }`——启停与重启的成功/失败按 `ok` 分流成 success / error toast。此前两者都走 `Error { message }`，界面只能靠文案猜
- **重启**：`RestartModule`（卸下 → 重新挂载）——**不改启用状态、不写 `disabled_modules`**，纯粹让模块按最新配置/外部状态重新初始化。行内是一个 hover 才出现的图标按钮（开关左侧），详情头部另有文字按钮；只对**已启用且不受保护**的模块出现（未挂载没有可重载的实例；管理面重启 = 先断 management WS，同禁用一样被拒）
- **装配失败可见**：`ModuleInfo.error` 非空 = 该模块挂载被跳过（command 不存在 / 握手失败），行卡出红色「失败」徽标、详情出失败原因块。与「用户主动禁用」区分开——两者 `enabled` 都是 false，但前者要人去修配置
- **外部条目现在真的在清单里**：`modules.toml` 里 `kind = "stdio"` 的条目由 Core 启动期拉起子进程，此前**从不注册进模块宿主**，于是「外部模块」组里只有实例门控壳、用户装的外部模块完全不可见。现在每个静态条目都会注册一个门控壳（`runtime = "subprocess"` + `builtin = false`）：`enabled = false` 的显示为已禁用（不消失）、启动失败的带 `error`。启停走**子进程控制面**（`ModuleHost::SubprocessModuleControl`）：禁用 = 排空并退出该模块进程、启用 = 重新拉起并握手，**立即生效**；重启 = stop + start。改 `command` / 换二进制仍需重启 Core（配置在启动期读取）

## 详情分区

1. **这个模块带来什么**：包内**工具 / 技能 chips，点击跳转到对应分类并选中**
2. **被谁使用**：`enabled_modules` 白名单反查（「已启用」/「未列入白名单」）
3. **启停影响**：按门控语义分派文案；受保护模块为警示色说明如何在 core.toml 真正禁用
4. **技术信息**（**折叠**）：承载 ID / Package / kind·版本 / Entry / **运行形态** / Author / **配置来源**（`config_origin`：`modules.toml` 或 `base.toml · layer 0`；空 = 配置未声明，即编译内置的缺省行为）

头部左侧是语义标签（`MODULE_SEMANTIC_LABELS`），外部模块另加 `外部模块 · 独立进程` 标签。`MODULE_KIND_LABELS.loop` 为「循环驱动」（全站统一的中文 kind 名）。

## 协议契约

`ModuleInfo.runtime`（`in_process` / `subprocess`，缺失 = 进程内）与 `ModuleInfo.builtin`（manifest 是否编译进 Core，缺失按 `true` 兼容）均由 Core 回推、彼此正交；来源分组（`moduleSource`）与运行形态标签（`moduleRuntimeLabel`，`MODULE_RUNTIME_LABELS`：独立进程 / 进程内）同源于这两个字段，Panel 不做推断性改写。

`builtin` 是**另一个维度**（manifest 从哪来），面板用 `moduleOrigin` 单独呈现：`false` → 行卡「外部来源」徽标 + 技术信息「来源：外部来源」；`true` → 技术信息「随二进制发布」（行卡不出徽标，默认情况不占视觉）。这条与分组正交——`builtin = false` + `runtime = in_process` 的**数据模块**（模块目录发现的 `module.toml`）留在「内部模块」组里，但来源一眼可见，不会被分组吞掉。

三个较新的可选字段（旧 Core 不发 → 空值，面板降级但不报错）：

| 字段 | 含义 | 面板落点 |
| --- | --- | --- |
| `error` | 挂载失败原因（`ModuleDescriptor::error`） | 行卡「失败」徽标 + 详情失败块 |
| `config_origin` | 声明该模块的配置层（来自 Core 的分层合成，与 `dump-config` 同源） | 技术信息「配置来源」 |
| `state` / `restarts` / `generation` | 子进程运行态（空 = 进程内实现） | 行卡异常态徽标 + 技术信息「运行态」行 |
| `source_path` / `command` / `args` / `env_keys` | 定义文件与启动方式（env 只给键名） | 技术信息「定义文件 / 启动命令 / 环境变量」 |
| — | `RestartModule { id }` 命令（**写命令**，不进可广播只读清单） | 行内图标按钮 / 详情按钮 |
| — | `ModuleOpResult { id, op, ok, message }` 事件（操作回执） | toast（success / error 分流） |

## 交叉跳转

`jumpToCategory`：模块详情成员 chip → 工具 / 技能分类并选中该条目；工具详情「所属包」按钮 → 模块分类并选中该模块——把「包 ⇄ 工具/技能」三层关系做成双向可点链路。包规模徽标与成员 chip 的成员名单取自 `state.tools` / `state.skills`（管理目标视图），切到非管理目标的节点时仍按管理目标统计。
