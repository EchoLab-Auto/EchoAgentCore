---
id: panel-settings
title: "Panel 设置视图"
group: 前端
x: 2560
y: 780
link: ["panel-settings-api | API 设置 | b>t", "panel-settings-skills | 技能 | b>t", "panel-settings-tools | 工具 | b>t", "panel-settings-plugins | 插件 | b>t", "panel-settings-agents | 智能体 | b>t", "panel-settings-cores | Core 与联邦 | b>t", "panel-settings-logs | 日志与系统 | b>t"]
---

# Panel 设置视图

> **定位**：设置视图的**外壳与共用契约**——左栏一级菜单、工具条、分类切换与清单加载语义、资源工作区的通用布局，以及八分类的界面元素树。每个分类的内容与操作细节归对应子文档（见「分类清单」）。**分界线**：本篇写给「菜单怎么切、切换时怎么拦未保存、清单三态怎么呈现」；7 篇分类子文档是各分类的**功能与操作参考**（字段清单、命令映射、协议契约），不改交互语义时不必读它们。读者：维护 Panel 设置界面的开发者；相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)（Agent 配置弹层的 API 供应商引用）、[配置持久化](../core/core-config-persistence.md)。

原「资源」「日志」两个独立视图与 API 设置弹窗合并为统一的设置视图：**左栏一级菜单 + 右侧分类工作区**——八个一级分类，每个分类是画布上的一个文档节点。

## 分类清单

| 分类 | 徽标 | 覆盖内容 | 文档 |
| --- | --- | --- | --- |
| API 设置 | Profile 数 | 供应商池增删改、测试与余额查询、近 7 天余额与用量图表、persona 级引用 | [设置 · API](./panel-settings-api.md) |
| 技能 | 条目数 | 技能工作台、详情分区、编辑器、从 Git 安装与来源管理 | [设置 · 技能](./panel-settings-skills.md) |
| 工具 | 条目数 | 按包分组的工具工作台、参数 Schema、启停的两层语义 | [设置 · 工具](./panel-settings-tools.md) |
| 插件 | 条目数 | 多 Core 管理节点、内部/外部与门控语义分组、受保护插件 | [设置 · 插件](./panel-settings-plugins.md) |
| 智能体 | 条目数 | persona 编辑器：基础信息、系统提示词、能力白名单 | [设置 · 智能体](./panel-settings-agents.md) |
| Core 连接 | — | 接入点管理（直连 / SSH 隧道）+ 联邦网络（同页下半区） | [设置 · Core 与联邦](./panel-settings-cores.md) |
| 系统 | — | 自更新 | [设置 · 日志与系统](./panel-settings-logs.md) |
| 日志 | — | Core / Panel / SSH 上游日志查看器 | [设置 · 日志与系统](./panel-settings-logs.md) |

分类与文档并非一一对应：**联邦不是独立分类**（它是「Core 连接」页内的下半区），「系统」与「日志」同属一篇。

## 视图结构

- **一级菜单**（168px）：`API 设置`（徽标 = Profile 数）/ `技能` / `工具` / `插件` / `智能体`（徽标 = 条目数）/ `Core 连接`（副标题给上游与 peers 计数）/ `系统`（自更新）/ `日志`（无徽标）——**共八分类**（`SettingsView.vue` 的 `categories` computed）；菜单项 9px/12px 内边距、圆角 8px，选中 = 主色左边框 + `--panel-accent-soft` 底、徽标反白；底部「刷新」按钮拉取四类清单（Skills/Tools/Plugins/Teams，`refresh()`；插件按当前运行区域请求，工作台内切管理节点时另发定向请求，见[设置 · 插件](./panel-settings-plugins.md)）
- **工具条**：标题 + 副标题（API = `当前：{provider} / {model}`（有激活 profile 时附 ` · Profile「{name}」`）；日志 = 来源说明；资源类 = `共 N 项 · K 已禁用`）+ 分类级操作（技能：Git 安装 / 新建技能；智能体：新建智能体）
- **清单拉取时机**：打开视图时自动拉取四类清单（`onMounted`）；条目消失时选中态自动置空

## 资源工作区通用契约

技能 / 工具 / 插件三类共用同一套工作台骨架，各自文档只写差异点：

- **条目列表**（技能工作台 320px 可拖拽；工具与插件 250px，与详情面板并排，组头可折叠）：筛选栏 + 可拖宽列表 + **双行行卡**；技能与工具默认**按包（package）分组**（package 是真实门控单位，与智能体能力勾选区一致；无包条目归「独立技能」/「独立工具」），插件默认**按运行位置分组**（内部 / 外部），智能体平铺（**按 id 排序**，无置顶——「主智能体」概念已去除）
- **条目行**：名称（超长省略）+ 行尾标记（Git 来源技能 `Git` 标签 / `已禁用`；**不再有「主」标签**）；选中 = 主色左边框 + 浅底；空列表显示「（暂无…）」
- **点选语义**：点选条目在右侧打开详情；点选智能体**直接进入编辑态**
- **清单加载三态**：四类清单各自独立跟踪（`settings:{skills,tools,plugins,teams}`）——列表区顶部 `LoadHint`（加载转圈 / >6s 慢提示 / >20s 错误+重试），加载中**计数徽标暂隐**、空态「（暂无…）」让位；顶部「刷新」按钮加载中转圈

## 切换与状态语义

- 进入 API 分类、或切走再切回：默认概览态（表单收起）
- **切换一级分类先确认是否放弃未保存的编辑态**：技能与智能体分类有脏检查，选「取消」则**不切换**——不是无条件丢弃
- 各分类的选中条目独立保持，切回时恢复
- **切换分类即卸载**：日志定时器等随之清理，切回时重新挂载拉取
- **交叉跳转**（`jumpToCategory`）：插件详情成员 chip → 工具/技能分类并选中该条目；工具详情「所属包」按钮 → 插件分类并选中该插件——把「包 ⇄ 工具/技能」三层关系做成双向可点链路（见[设置 · 插件](./panel-settings-plugins.md)）

## 界面元素树

```prodoc-flow
graph TB
  Set[设置视图] --> Nav[左栏一级菜单]
  Set --> Work[右侧分类工作区]
  Nav --> Nav1[八分类 · 徽标 + 底部刷新]
  Work --> Api[API 设置|/panel/panel-settings-api.md]
  Work --> Sk[技能|/panel/panel-settings-skills.md]
  Work --> To[工具|/panel/panel-settings-tools.md]
  Work --> Pl[插件|/panel/panel-settings-plugins.md]
  Work --> Ag[智能体|/panel/panel-settings-agents.md]
  Work --> Co[Core 连接与联邦|/panel/panel-settings-cores.md]
  Work --> Lo[日志与系统|/panel/panel-settings-logs.md]
  Api --> Api1[编辑表单与快捷模板]
  Api --> Api2[Profiles 卡片网格]
  Api --> Api3[余额与用量图表]
  Sk --> Sk1[筛选栏与双行行卡]
  Sk --> Sk2[详情四分区检查器]
  Sk --> Sk3[Git 安装弹层]
  To --> To1[按包分组列表]
  To --> To2[参数 Schema 详情]
  Pl --> Pl1[管理节点选择]
  Pl --> Pl2[分组维度: 内外 / 门控]
  Pl --> Pl3[受保护插件与交叉跳转]
  Ag --> Ag1[头部信息卡]
  Ag --> Ag2[三个折叠分区]
  Ag --> Ag3[粘性页脚与只读详情]
  Co --> Co1[接入点与 SSH 隧道]
  Co --> Co2[联邦组建向导]
  Co --> Co3[拓扑视图与手动配对]
  Lo --> Lo1[日志查看器]
  Lo --> Lo2[自更新面板]
```

节点数 29（含元素层 18 个），符合单图 ≤30 的建议；框内链接可直接跳转到对应分类文档。
