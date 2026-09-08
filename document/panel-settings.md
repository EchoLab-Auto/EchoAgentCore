---
id: panel-settings
title: "Panel 设置视图"
group: 前端模块
x: 1186
y: 1025
---

# Panel 设置视图

设置视图（左栏一级菜单 + 右侧分类工作区）的完整交互：API 设置（整页表单）、资源工作区（技能/工具/插件/智能体）、从 Git 安装技能、日志查看器。

## 九、设置视图（SettingsView）

2026-09-04 起，原「资源」「日志」视图与 API 设置弹窗合并为统一的设置视图：**左栏一级菜单 + 右侧分类工作区**。

- **一级菜单**（168px）：`API 设置`（徽标 = Profile 数）/ `技能` / `工具` / `插件` / `智能体`（徽标 = 条目数）/ `日志`（无徽标）；菜单项 9px/12px 内边距、圆角 8px，选中 = 主色左边框 + `--panel-accent-soft` 底、徽标反白；底部「刷新」拉取四类清单（Skills/Tools/Plugins/Teams，`SettingsView.vue:574-579`）。切换分类**丢弃未保存的编辑态**；各分类的选中条目独立保持，切回时恢复
- **工具条**：标题 + 副标题（API = 当前 `provider / model · Profile`；日志 = 来源说明；资源类 = `共 N 项 · K 已禁用`）+ 分类级操作（技能：Git 安装 / 新建技能；智能体：新建智能体）
- 打开视图时自动拉取四类清单（onMounted）；条目消失时选中态自动置空

### 9.1 API 设置（整页表单，max-width 720px）

原 SettingsModal 内容的去弹窗化（`ApiSettings.vue`），交互不变：

- **快捷模板**（5 个：DeepSeek-Anthropic / DeepSeek-OpenAI / OpenAI / Anthropic / Ollama）：点击填充 provider/base_url/model/思考/推理
- **表单**：Provider（下拉固定 deepseek/openai/third-party 三项，未知值保留为额外选项；选 deepseek/openai 自动填充对应 base_url/model）、Model、Base URL、API Key（密码框；标注 已设置/未设置，留空保持不变）、思考模式、推理强度
- **测试连接**：`TestApi`（默认或指定 profile）；加载态互斥；结果显示 ✓/✗ + 消息 + 延迟；每次进入页面清空上次结果
- **Profile 管理**：每行显示 激活/编辑中 标签与 `provider / model · key` 摘要；操作 测试 / 编辑 / 切换（`SwitchApi`）/ 删除（`DeleteApi`，**无确认，立即生效**）；新建 = 命名 + 保存为新 profile；编辑已有 profile 时保存钮文案变为「保存到 {name}」，保存后保持编辑态
- 进入页面或 `state.api` 更新时表单重置为当前配置

### 9.2 资源工作区（技能 / 工具 / 插件 / 智能体）

**条目列表**（250px，与详情面板并排，组头可折叠）：

- 技能/工具按分类分组、插件按 kind 分组（组头 caret + 计数徽标）；智能体平铺（**主 Agent 置顶**，其余按 id 排序）
- 条目行：名称（超长省略）+ 行尾标记（Git 来源技能 `Git` 标签 / 主 Agent `主` 标签 / `已禁用`）；选中 = 主色左边框 + 浅底；空列表显示「（暂无…）」
- 点选条目在右侧打开详情；点选智能体**直接进入编辑态**

| 对象 | 详情内容 | 操作 |
|---|---|---|
| 技能 | 分类/常驻标签、Git 来源徽标（`Git · {短URL}`，悬停显示 `{url} @ {rev}`，URL 超 42 字符截断）、描述、触发词、SKILL.md 原文（≤420px 滚动） | 启停开关（`ToggleSkill`）、编辑（名称锁定）、删除（confirm）；**Git 来源技能追加**：更新（`UpdateSkillFromGit`，无确认）、移除来源（confirm「目录保留，可手动删除」→ `RemoveSkillSource`） |
| 工具 | 分类、描述、JSON 参数 schema | 启停开关（`ToggleTool`，禁用后模型不可见） |
| 插件 | kind/版本/外部标签、描述、ID/Entry/Author | 启停开关（`TogglePlugin`，禁用即卸载注册；全局生效；**「管理面」插件禁用被保护**——core 拒绝 + UI 明示，防 Panel 自锁断连） |
| 智能体 | 主 Agent/N 会话/编排模式标签、提示词、能力白名单、禁用能力（error 色标签组） | 选中即进编辑态；启停（`ToggleTeam`）；非主 Agent 可删除 |

- **技能编辑器**：名称（编辑时锁定）、描述、触发词（逗号分隔）、分类、常驻开关、Markdown 正文（12 行自适应）；保存 `SaveSkill`
- **智能体编辑器**：ID 必填 + `/^[A-Za-z0-9_-]+$/`（编辑时锁定，留空回退为名称）、名称必填；**编排模式分段单选**（单任务/多任务并行，互斥——写入 `enabled_plugins` 白名单的 `orchestration.single`/`chatbot` 子插件 id：空表显示 chatbot，切 single 先物化全量再替换，保存时剔除旧特性 id）；启用包（Package 级主控，一次切换整包工具+技能）、启用插件/工具/技能复选组；粘性页脚 删除/放弃更改/保存（`SaveTeam`）
- `builtin` 类工具归入「内置工具」组

### 9.3 从 Git 仓库安装技能

技能工具条「Git 安装」展开内联表单（虚线边框卡片），三字段 + 提交钮：仓库 URL（https/ssh/本地路径，**唯一必填**，trim 后为空直接不提交）、安装目录名（默认取仓库名）、分支（默认取仓库默认分支）；后两者留空传 `null`。提交下发 `InstallSkillFromGit{url, name?, branch?}`（`SettingsView.vue:460-472`，`protocol.ts:262`）。Core 侧机制（shallow clone、目录提升、`.sources.json` 来源记录、更新/移除语义）见 [技能系统](./core-skills.md)「外部 Git 来源技能」。

```prodoc-flow
graph LR
  Form[表单提交] -->|InstallSkillFromGit| Clone[core: git clone --depth 1]
  Clone -->|校验/目录提升| Reload[reload_skills 热重载]
  Clone -->|失败: 目录已存在/无SKILL.md/网络| Err[Error 事件]
  Reload -->|SkillsList 回推| List[列表刷新: Git 徽标]
  Reload -->|Error 事件| Toast[toast 通知结果]
  Err --> Toast
```

- **结果反馈约定**：提交后立即清空表单，本地**无加载态、无成败提示**——安装结果完全依赖 Core 回推 `SkillsList` 事件刷新列表；成功与失败都经 Core `Error` 事件以 toast 呈现（消息文本区分）。这是与"编辑即生效"原则一致的单向命令模式（panel 不预测 core 侧耗时操作的结果）
- **已装 Git 技能**：详情页头部追加「更新」（`UpdateSkillFromGit`，无确认）与「移除来源」（confirm 明示目录保留）按钮；来源徽标 `Git · {短URL}` 悬停显示 `{url} @ {rev}`

### 9.4 日志（整页查看器）

原日志视图的平移（`LogView.vue`），交互不变并新增手动「刷新」按钮：Core/Panel 分段切换（默认 Core），拉取 `/api/logs/{core,panel}?lines=100` 原文显示（max-height 65vh 内滚动）；自动刷新默认开（**5s** 间隔），可切手动；切换来源或自动开关立即重载并重启定时器；HTTP 错误显示状态码。**切换分类即卸载**——定时器随卸载清理，切回时重新挂载拉取。

