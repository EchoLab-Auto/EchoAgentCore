---
id: panel-settings-skills
title: "设置 · 技能"
group: 前端
x: 2880
y: 1060
---

# 设置 · 技能

> **定位**：技能工作台——筛选与分组、双行行卡、详情四分区检查器、技能编辑器，以及从 Git 仓库安装技能（克隆进度、失败清场与残留清理）与来源管理。工作台骨架与清单三态见[设置视图](./panel-settings.md)「资源工作区通用契约」。读者：维护技能相关界面的开发者；相关：[技能系统](../plugins/core-skills.md)（Core 侧的克隆、`.sources.json` 与热重载机制）、[插件化设计](../plugins/core-plugins.md)（包门控）。

## 工作台结构

背景：技能全部没有 `category`（实测 13 个技能全落进单一「未分类」组）、列表只显示名字（启停要逐个点进详情）、「这技能什么时候会进来」「关掉它会影响谁」两个问题在界面上答不出来——工作台为此重排：

- **筛选栏**（技能分类专属，工具条下方一行）：搜索框（命中名称 / 描述 / 触发词）+ 状态 chips（`全部 / 已启用 / 已禁用 / 常驻 / 系统提示词 / Git 来源`，各带计数）+ 分组维度分段（`按包`（默认）/ `按分类` / `平铺`）；工具条副标题升级为构成统计（`共 N 项 · K 已禁用 · A 常驻 · S 系统提示词 · G Git 来源`）
- **列表行卡（双行）**：主行 = 状态点（实心=启用 / 空心=禁用，禁用整行降透明度）+ 名称 + 行尾标记（`★`常驻 / `⇧`系统提示词 / `Git` / `包已禁用`）+ **hover 浮出的快速启停开关**（不改变选中，`@click.stop`）；次行 = 描述（单行截断）+ 触发词前 3 个（`+N`）；所属包插件被禁用时组头追加提示「所属包已禁用，包内技能当前不可用」
- **分栏可拖拽**：列表与详情之间的分隔条拖动调宽（220–560px，`localStorage` 持久化 `echo-panel-caps-list-width`）
- **组合并提示**：所属包插件禁用、或技能自身禁用时，详情顶部显示对应横幅
- **行内操作**：启停开关（`ToggleSkill`）、编辑（名称锁定）、删除（confirm）

## 详情分区

详情头部 = 名称 + 包名 + 启停开关 + `⋯` 菜单「编辑 / 更新（Git）/ 移除来源 / 删除」，下方四个分区：

1. **生效方式**：关键词触发（触发词 chips **可增删**——删除走 `closable` tag，添加走行内输入 + Enter；就地修改经 `SaveSkill` 补丁回写，保留其余字段）、常驻注入开关（`always`）、系统提示词开关（`system`，开启前 confirm 明示「注入 base 区，影响所有启用它的智能体」）
2. **被谁使用**：按 `TeamsList` 反查每个智能体对本技能的状态——`系统提示词注入`（`system_skills`）/ `已禁用`（`disabled_skills`）/ `未列入白名单`（`enabled_skills` 非空且不含）/ `可用`
3. **来源**：两态——**无来源记录**（「新建技能」创建或手工放置的目录；`SkillInfo.source` 为空，详情显示「内置技能（随 Core 分发的 skills 目录）」，文案不区分本地新建）与 **Git 来源**（`SkillInfo.source` 回推：仓库 / 版本（含分支）/ 安装时间 + 更新 / 移除来源按钮）
4. **内容**：frontmatter 摘要条（分类 · 常驻 · 系统提示词 · 触发词数）+ Markdown **预览 / 源码**切换（预览复用 ui-frame `MarkdownRenderer`）

## 技能编辑器

名称（编辑时锁定）、描述、触发词（逗号分隔）、分类、常驻开关、Markdown 正文（12 行自适应）；**吸底操作条**显示保存状态（`● 有未保存的更改` / `○ 无更改`）+ 放弃更改 / 保存；切换条目或切换分类前若有脏改动会先 confirm；保存 `SaveSkill`。

## 从 Git 仓库安装

技能按来源分两态：**无来源记录**（「新建技能」创建或手工放进 skills_dir 的目录）与 **Git 来源技能**（列表行 `Git` 徽标、筛选 chip「Git 来源」据此判定）。Git 安装接受 https / ssh / 本机路径，一律走 `git clone` 并写入 `skills_dir/.sources.json`。

技能工具条「Git 安装」打开**弹层**（原内联表单会顶掉列表且安装后无反馈）：三字段——仓库 URL（**唯一必填**，trim 后为空时按钮禁用）、安装目录名（默认取仓库名）、分支（默认取仓库默认分支）；后两者留空传 `null`。提交下发 `InstallSkillFromGit{url, name?, branch?}`。

```prodoc-flow
graph LR
  Form[弹层提交] -->|InstallSkillFromGit| Clone[core: git clone --depth 1]
  Form --> Wait[弹层保持 + 进度计时]
  Clone -->|失败: 网络 / 超时 / 无 SKILL.md| Clean[失败清场: 删除目标目录]
  Clean --> Err[Error 事件]
  Clone -->|校验/目录提升| Reload[reload_skills 热重载]
  Reload -->|SkillsList 回推| List[列表刷新: Git 徽标 + 自动关窗]
  Reload -->|Error 事件| Toast[toast 通知结果]
  Err --> Toast
```

- **安装期间（可见等待）**：弹层**保持打开**，进度行（`git-modal__progress`，`role="status"`）= 旋转图标 + 「正在克隆…已等待 Ns」（`gitInstallElapsed`，500ms 计时）；超过 20s 追加「已超过 20s，网络慢或不可达；Core 侧最长等 120s」（`gitInstallSlow`）。URL 输入与取消 / 安装按钮禁用，主按钮文案「安装中…」，安装中重复提交被短路（`installGitSkill` 守卫）。关闭弹层不中断 Core 侧克隆
- **成功判据**：以技能清单变化为准（`watch(state.skills)` 的名称序列）——Core 装完推 `SkillsList` 即自动关窗并清空表单
- **失败反馈**：结果由 Core `Error` 事件以 toast 呈现；界面**不回收安装态**——`gitInstalling` 只由技能清单变化复位（成功路径），失败与超时不会走到那里，于是进度继续累计、URL 仍禁用、关窗重开亦然，需刷新页面才能重试
- **失败清场与残留**（Core `skills_dir::install`）：clone 失败即删除目标目录，本次与后续重试都不会撞「目录已存在」；目标目录已存在时先按 `is_broken_clone`（有 `.git` 且无 `SKILL.md`）识别上次被超时 kill 的残留并清掉重装，正常技能目录（有 `SKILL.md`）不会被误删——报「目录已存在」并提示卸载 / 换名 / 移走手动放置的同名目录；clone 成功但仓库内找不到 `SKILL.md` 同样清场后报错。`GIT_TERMINAL_PROMPT=0` 让私有仓库缺凭据时立即报认证失败，而不是在无 TTY 的后台进程里挂满超时；硬超时 120s（`GIT_TIMEOUT`）之外另有低速中止（<1KB/s 持续 30s）
- **弹层静态提示**：浅克隆 + 目录提升说明；建议私有 / 内网仓库用 SSH 地址（`git@github.com:org/repo.git`）——HTTPS 被网络阻断时只能等 120s 超时失败

## 来源与更新

已装 Git 技能在详情「来源」分区显示仓库 / 版本 / 安装时间与「更新」（`UpdateSkillFromGit`，无确认、无进度面板，仅命令 + `SkillsList` / `Error` 反馈）、「移除来源」（confirm 明示「目录保留，可手动删除」→ `RemoveSkillSource`）按钮；`⋯` 菜单同步提供「更新（Git）」「移除来源」入口。来源记录按技能名（frontmatter `name`）索引、目录名可不同，故更新 / 移除来源按技能名寻址。
