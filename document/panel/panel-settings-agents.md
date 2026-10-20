---
id: panel-settings-agents
title: "设置 · 智能体"
group: 前端
x: 3840
y: 1060
---

# 设置 · 智能体

> **定位**：智能体（persona）编辑器——头部信息卡与三个折叠分区（基础信息 / 系统提示词 / 启用的能力）、粘性页脚与只读详情视图。清单与选中语义见[设置视图](./panel-settings.md)「资源工作区通用契约」。读者：维护智能体配置界面的开发者；相关：[多 Agent 与会话](../core/core-agents.md)（persona 装配与能力隔离）、[设置 · API](./panel-settings-api.md)（`api_profile` 引用）、[设置 · 插件](./panel-settings-plugins.md)（插件与包来源）。

## 头部信息卡

顶部 **ui-frame `NeumorphismCard`**：首字头像 + 名称 + 启用状态标签 + `ID · 启用态·循环模式` 摘要行（**无「主 Agent」标签**）。点选智能体列表项**直接进入编辑态**，不经只读详情。

## 基础信息

摘要 `启用 · 单会话`：

- **ID** 必填 + `/^[A-Za-z0-9_-]+$/`（编辑时锁定，留空回退为名称）
- **名称**必填、描述
- **启用开关**
- **循环模式分段单选**：单会话（默认）/ 并行多会话，互斥——写入 `enabled_plugins` 白名单的 `echo-agent.loop.{single,parallel}` 插件 id；空表即单会话

## 系统提示词

摘要 `自定义提示词/继承全局 · N 个系统技能`：提示词文本域 + 系统提示词 skills 复选（双列网格）。勾选即写入 `system_skills`——这些技能注入 base 区，影响所有启用它的智能体。

## 启用的能力

摘要 `N 插件 · N 工具 · N 技能`，空表显示「不限制（全部可用）」。四个子块以 `NeumorphismDivider` 分隔：

| 子块 | 候选与判定 |
| --- | --- |
| **插件** | `kind ∈ {instance, management, interaction}` ∪ 包维度门控插件固定清单（tools.builtin / skills.dir / workspace / subagent，与 `capabilities.ts::PACKAGE_GATED_PLUGIN_IDS` 逐项镜像；插件实例不经清单，按 `kind == "instance"` 运行期推导，新增平台零改动）；两者之外的插件如 loop.* 不出现。标签显示中文 kind 名，单一来源 `capabilities.ts::PLUGIN_KIND_LABELS` |
| **包（Package）** | 一次切换整包工具 + 技能，附成员清单与项数徽标；显示名单一来源 `capabilities.ts::PACKAGE_DISPLAY_NAMES` |
| **工具** | 按分类分组，自适应多列 |
| **技能** | 全部技能（含 Git 来源） |

判定单一来源 = `capabilities.ts::isPluginCheckboxVisible`——UI 不自行推断候选集，避免与 Core 的门控语义分叉。

> 原「禁用的能力」（插件黑名单复选）分区已移除——插件维度只保留白名单（空表 = 全部启用，取消勾选即物化），既有黑名单配置由 Core 加载期物化进白名单；`disabled_tools` / `disabled_skills` 仍可在详情视图看到（error 色标签组）。

## 保存、删除与详情视图

编辑器用 `NeumorphismCollapse` 分三个折叠分区，**默认展开前两个**（避免超长滚动），标题行右侧带实时摘要；粘性页脚 = 删除 / 放弃更改 / 保存（`SaveTeam`）。取消编辑后回到**只读详情**：头部有「编辑」按钮（重新进入编辑器，无需再点列表项）。

智能体对象级操作：启停 `ToggleTeam`；任意智能体可删（**至少保留一个**；无受保护成员）。
