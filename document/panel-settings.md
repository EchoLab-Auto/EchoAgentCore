---
id: panel-settings
title: "Panel 设置视图"
group: 前端模块
x: 1186
y: 1025
---

# Panel 设置视图

设置视图（左栏一级菜单 + 右侧分类工作区）的完整交互：API 设置（概览 + 按需展开表单）、资源工作区（技能/工具/插件/智能体）、从 Git 安装技能、日志查看器。

## 九、设置视图（SettingsView）

2026-09-04 起，原「资源」「日志」视图与 API 设置弹窗合并为统一的设置视图：**左栏一级菜单 + 右侧分类工作区**。

- **一级菜单**（168px）：`API 设置`（徽标 = Profile 数）/ `技能` / `工具` / `插件` / `智能体`（徽标 = 条目数）/ `日志`（无徽标）；菜单项 9px/12px 内边距、圆角 8px，选中 = 主色左边框 + `--panel-accent-soft` 底、徽标反白；底部「刷新」拉取四类清单（Skills/Tools/Plugins/Teams，`SettingsView.vue:1290-1296`）。切换分类**丢弃未保存的编辑态**；各分类的选中条目独立保持，切回时恢复
- **工具条**：标题 + 副标题（API = 全局默认 `provider / model`；日志 = 来源说明；资源类 = `共 N 项 · K 已禁用`）+ 分类级操作（技能：Git 安装 / 新建技能；智能体：新建智能体）
- 打开视图时自动拉取四类清单（onMounted）；条目消失时选中态自动置空

### 9.1 API 设置（概览视图 + 按需展开表单，max-width 720px）

`ApiSettings.vue`：**双层视图**，配置表单不再常驻悬浮在页面顶部——默认只显示概览（当前配置 + Profiles 列表），表单仅在点击「编辑」或「添加 API 服务商」时展开。

**交互层级（两态）**

| 状态 | 触发 | 内容 |
|---|---|---|
| 概览视图（默认） | 进入 API 分类 / 表单收起/保存后 | Profiles 卡片网格、添加入口 |
| 编辑表单（展开态） | 点击 profile「编辑」/「+ 添加 API 服务商」 | 标题栏 + 快速模板 + 表单字段 + 操作行 |

**布局（自上而下）**

1. **编辑表单**（仅展开态，插入在列表上方）：标题栏（「编辑 Profile · {name}」+ 编辑中标签 /「新增 API 服务商」+ 收起按钮）→ 快速模板（6 个，点击填充）→ 表单字段 → 操作行（测试连接 / 保存 / 取消）
2. **API Profiles 卡片网格**（常驻，`repeat(auto-fill, minmax(240px, 1fr))`）：卡片本体用 **ui-frame `NeumorphismCard`**（elevation=2、radius=large、hoverable=bulge、no-padding），按槽位排版——header = 名称 +（编辑中标签）；body = `provider / model` + base_url + key/思考/推理摘要（min-height 62px 对齐）；footer = 余额行（DeepSeek）+ **操作行（测试 / 编辑 / 删除 三按钮等宽网格 `repeat(3, 1fr)`**，`DeleteApi` **无确认，立即生效**）；**编辑中的卡主色 outline 高亮**。**默认标记已移除**（2026-09-14）：卡片不再有「默认」标签与「设为默认」按钮——profiles 只是可供 persona 引用的供应商池，不再有"哪个是默认"的概念；模型选用在 persona 级（Agent 配置弹层）完成，API 设置页不提供「激活」「切换」
3. **DeepSeek 余额**（2026-09）：provider=deepseek 或 base_url 含 `deepseek.com` 的卡片（含默认配置卡）显示「查余额」按钮——发 `QueryApiBalance{name}` 查官方 `/user/balance`（Anthropic/OpenAI/beta 端点自动归一到 `{root}/user/balance`），结果显示为 `¥110.00`（CNY；USD 用 `$`，其他币种后缀显示），失败显示错误文本；查询加载态互斥、一次一个目标
4. **测试失败复制**（2026-09）：测试连接结果为失败（✗）时，结果行末尾附 **ui-frame `ChatCopyButton`**（`@echolab-auto/ui-frame/chat`，24px `nm-chat-copy` 图标钮，点击复制完整错误文本到剪贴板，复制定时反馈 ✓）——摘要卡与 profile 卡两处结果行均适用；结果文本整句 `title` 悬停可见（行内 ellipsis 截断时）。**非安全上下文兜底**：Panel 经局域网 IP 以 HTTP 访问时 `navigator.clipboard` 不可用（原本静默失败），`useClipboard` 已补 `textarea + document.execCommand('copy')` 兜底（vendor 本地补丁，安全上下文仍优先 Clipboard API）
6. **HTML 端点诊断**（2026-09）：base_url 指向网页根域而非 API 前缀时（响应 `text/html`），测试连接错误消息自动追加提示「该地址返回网页而非 JSON——请检查 Base URL 是否为 API 前缀（OpenAI 兼容端点通常以 /v1 结尾）」（OpenAI/Anthropic 两个客户端均实现，解析错误与 HTTP 错误均适用）
5. **「+ 添加 API 服务商」按钮**（卡片网格底部常驻，primary）

**表单字段**（展开态与字段规则同原设计）：

- **Profile 名称**：仅添加模式显示（必填，trim 非空才可保存）；编辑模式显示只读名称
- **快捷模板**（6 个，点击填充 provider/base_url/model/思考/推理）：**DeepSeek**（2026-09 合并原「Anthropic 兼容 / OpenAI 兼容」两种格式为单一预设——默认官方 Anthropic 端点 `https://api.deepseek.com/anthropic` + `deepseek-v4-flash`，需 OpenAI 格式时手工改为 `/v1` + `deepseek-chat`，提示文字已注明）、**Kimi For Coding**（2026-09 新增：Kimi Code 订阅的 Anthropic 兼容端点 `https://api.kimi.com/coding` + `k3`，Key 在 `www.kimi.com/code/console` 创建，推理档位 high；provider=`kimi`）、**中转站（OpenAI 兼容）**（provider=`third-party`，base_url/model **留空待填**，占位提示"你的网关/v1"与"中转站模型名"；请求为干净 OpenAI 格式，不携带 DeepSeek 专有 thinking 字段）、OpenAI、Anthropic、Ollama（本地）
- **Provider 下拉**：固定 deepseek/openai/kimi/third-party 四项，未知值保留为额外选项；选 deepseek/openai/kimi 自动填充对应 base_url/model；第三方需 OpenAI 兼容（有 /anthropic 端点自动识别）。**注意**：中转站=第三方时不会附加 DeepSeek 专有请求字段（`thinking`/`reasoning_effort`），兼容性最好；若要保留思考模式且后端是 DeepSeek，选 deepseek 预设
- Model / Base URL（手填）、**API Key**（密码框；标注 已设置/未设置，**留空保持不变**，不回显）、思考模式（启用/禁用）、推理强度（low/high/max）
- 添加模式初始为空表单（Provider 默认 third-party），快速模板可一键填充

**状态转换规则**

- 进 API 分类、或切走再切回：默认概览态（表单收起）；**切换一级分类丢弃未保存的编辑态**
- 点「编辑」→ 表单展开填充该 profile（apiKey 留空）；点「添加」→ 展开空表单
- **保存成功（`state.api` 回播后）→ 表单收起**，回到概览；编辑目标行取消「编辑中」，列表/摘要卡随回播刷新（新增/更新 profile 不改变任何 persona 的选用——persona 在各自 Agent 配置里引用）
- 「取消/收起」→ 丢弃表单修改，回到概览
- 测试连接：`TestApi`（表单目标 = 编辑中的 profile 名，添加模式为空 = 默认配置）；加载态互斥（一次仅测一个）；**每次表单展开时清空上次测试结果**

后端命令映射：`UpdateApiConfig`（name 空 = 全局默认配置；非空 = 供应商池增改）/ `TestApi` / `DeleteApi`；`SwitchApi` 保留协议但 UI 不再暴露（persona 级选用取代全局激活）。数据流见 [配置持久化](./core-config-persistence.md)。

### 9.1.1 Persona 级 API（`api_profile` 引用）

**模型**：全局 `[agent].api_profiles` 是唯一供应商池；`[agent.teams.{id}].api_profile = "name"` 只做引用（None = 跟随全局默认配置）。不内嵌供应商值——增删改在设置页完成，所有 persona 共享最新池。

**入口**：聊天区入口行 →「配置」按钮 → Agent 配置弹层新增「API 供应商」下拉（首选「跟随全局默认配置」，其余为池中 profile，显示 `name（provider / model · key 状态）`）；保存 `SaveTeam{api_profile}`。

**运行期生效**：`SaveTeam` 成功后 target agent 立即重建自己的 provider（`apply_persona_api`：按引用从全局池解析 → `create_provider` 独立构建 → 更新 model）；启动期 make_agent 对配置了 `api_profile` 的 persona 直接构建独立 provider，不共享默认 provider。

**持久化**：写入 `[agent.teams.{id}].api_profile`（TOML）；`TeamInfo.api_profile` 回推 Panel；AgentSwitcher 卡片/列表显示当前 persona 生效模型（池解析，未引用 = 全局默认 model）。

### 9.2 资源工作区（技能 / 工具 / 插件 / 智能体）

**条目列表**（技能工作台 320px 可拖拽；其余分类 250px，与详情面板并排，组头可折叠）：

- 技能/工具/插件均为**工作台**（筛选栏 + 可拖宽列表 + 双行行卡，2026-09-23）：技能与工具默认**按包（package）分组**（package 是真实门控单位，与智能体能力勾选区一致；无包条目归「独立技能」/「独立工具」），插件默认**按门控语义分组**（见下）；智能体平铺（**按 id 排序**，无置顶——去主智能体 2026-09-13）
- 条目行：名称（超长省略）+ 行尾标记（Git 来源技能 `Git` 标签 / `已禁用`；**不再有「主」标签**）；选中 = 主色左边框 + 浅底；空列表显示「（暂无…）」
- 点选条目在右侧打开详情；点选智能体**直接进入编辑态**

| 对象 | 详情内容 | 操作 |
|---|---|---|
| 技能 | 见下方「技能工作台」小节 | 启停开关（`ToggleSkill`，列表行内 hover 亦有快速开关）、编辑（名称锁定）、删除（confirm）；**Git 来源技能追加**：更新（`UpdateSkillFromGit`，无确认）、移除来源（confirm「目录保留，可手动删除」→ `RemoveSkillSource`） |
| 工具 | 见下方「工具 / 插件工作台」小节 | 启停开关（`ToggleTool`，禁用后模型不可见；列表行内 hover 亦有快速开关） |
| 插件 | 见下方「工具 / 插件工作台」小节 | 启停开关（`TogglePlugin`，禁用即卸载注册；全局生效；**「管理面」插件禁用被保护**——core 拒绝 + UI 明示且行内不给开关，防 Panel 自锁断连）。**包级聚合**：包内工具与技能随包一起启停（见 [插件化设计](./core-plugins.md)「Package」） |
| 智能体 | N 会话/循环模式标签（单会话/并行多会话）、提示词、能力白名单、禁用能力（`disabled_tools`/`disabled_skills` 的 error 色标签组；插件黑名单已移除） | 选中即进编辑态；启停（`ToggleTeam`）；任意智能体可删（**至少保留一个**；无受保护成员） |

#### 技能工作台（2026-09-23 重排版）

背景：技能全部没有 `category`（实测 9 个技能全落进单一「未分类」组）、列表只显示名字（启停要逐个点进详情）、「这技能什么时候会进来」「关掉它会影响谁」两个问题在界面上答不出来。

- **筛选栏**（技能分类专属，工具条下方一行）：搜索框（命中名称 / 描述 / 触发词）+ 状态 chips（`全部 / 已启用 / 已禁用 / 常驻 / 系统提示词 / Git 来源`，各带计数）+ 分组维度分段（`按包`（默认）/ `按分类` / `平铺`）；工具条副标题升级为构成统计（`共 N 项 · K 已禁用 · A 常驻 · S 系统提示词 · G Git 来源`）
- **列表行卡（双行）**：主行 = 状态点（实心=启用 / 空心=禁用，禁用整行降透明度）+ 名称 + 行尾标记（`★`常驻 / `⇧`系统提示词 / `Git` / `包已禁用`）+ **hover 浮出的快速启停开关**（不改变选中，`@click.stop`）；次行 = 描述（单行截断）+ 触发词前 3 个（`+N`）；所属包插件被禁用时组头追加提示「所属包已禁用，包内技能当前不可用」
- **分栏可拖拽**：列表与详情之间的分隔条拖动调宽（220–560px，`localStorage` 持久化 `echo-panel-caps-list-width`）
- **详情分区检查器**（头部 = 名称 + 包名 + 启停开关 + `⋯` 菜单「编辑 / 更新（Git）/ 移除来源 / 删除」）：
  1. **生效方式**：关键词触发（触发词 chips **可增删**——删除走 `closable` tag，添加走行内输入 + Enter；就地修改经 `SaveSkill` 补丁回写，保留其余字段）、常驻注入开关（`always`）、系统提示词开关（`system`，开启前 confirm 明示「注入 base 区，影响所有启用它的智能体」）
  2. **被谁使用**：按 `TeamsList` 反查每个智能体对本技能的状态——`系统提示词注入`（`system_skills`）/ `已禁用`（`disabled_skills`）/ `未列入白名单`（`enabled_skills` 非空且不含）/ `可用`
  3. **来源**：内置，或 Git（仓库 / 版本（含分支）/ 安装时间 + 更新 / 移除来源按钮）
  4. **内容**：frontmatter 摘要条（分类 · 常驻 · 系统提示词 · 触发词数）+ Markdown **预览 / 源码**切换（预览复用 ui-frame `MarkdownRenderer`）
  - 组合并提示：所属包插件禁用、或技能自身禁用时，详情顶部显示对应横幅
- **技能编辑器**：名称（编辑时锁定）、描述、触发词（逗号分隔）、分类、常驻开关、Markdown 正文（12 行自适应）；**吸底操作条**显示保存状态（`● 有未保存的更改` / `○ 无更改`）+ 放弃更改 / 保存；切换条目或切换分类前若有脏改动会先 confirm；保存 `SaveSkill`
- **智能体编辑器（2026-09 分区化重排）**：顶部**头部信息卡**（ui-frame `NeumorphismCard`：首字头像 + 名称 + 启用状态标签 + `ID · 启用态·循环模式` 摘要行；**无「主 Agent」标签**），下方用 `NeumorphismCollapse` 分三个折叠分区（原第四个「禁用的能力」随插件黑名单移除），**默认展开前两个**（避免超长滚动），标题行右侧带实时摘要：
  1. **基础信息**（摘要 `启用 · 单会话`）：ID 必填 + `/^[A-Za-z0-9_-]+$/`（编辑时锁定，留空回退为名称）、名称必填、描述、启用开关、**循环模式分段单选**（单会话（默认）/ 并行多会话，互斥——写入 `enabled_plugins` 白名单的 `echo-agent.loop.{single,parallel}` 插件 id；空表即单会话）
  2. **系统提示词**（摘要 `自定义提示词/继承全局 · N 个系统技能`）：提示词文本域 + 系统提示词 skills 复选（双列网格）
  3. **启用的能力**（摘要 `N 插件 · N 工具 · N 技能`，空表显示「不限制（全部可用）」）：四个子块以 `NeumorphismDivider` 分隔——**插件**（`kind ∈ {adapter, management, interaction}` ∪ 包维度门控插件固定清单 tools.builtin / skills.dir / workspace / adapter.qq，判定单一来源 = `capabilities.ts::isPluginCheckboxVisible`；两者之外的插件如 loop.* 不出现；标签显示中文 kind 名，单一来源 `capabilities.ts::PLUGIN_KIND_LABELS`）、**包（Package）**（一次切换整包工具+技能，附成员清单与项数徽标；显示名单一来源 = `capabilities.ts::PACKAGE_DISPLAY_NAMES`）、**工具**（按分类分组，自适应多列）、**技能**
  > 原「禁用的能力」（插件黑名单复选）分区已移除（2026-09-11）——插件维度只保留白名单（空表 = 全部启用，取消勾选即物化），既有黑名单配置由 Core 加载期物化进白名单。
  粘性页脚 删除/放弃更改/保存（`SaveTeam`）
- **详情视图**：取消编辑后回到只读详情；头部新增「编辑」按钮（重新进入编辑器，无需再点列表项）
- `builtin` 类工具归入「内置工具」组

#### 工具 / 插件工作台（2026-09-23 重排版）

与技能工作台同构（筛选栏 + 双行行卡 + 分栏可拖 + 详情分区），差异点：

**工具**

- **按包分组**（默认）：实测 24 个工具的 `category` 全是 `builtin`，原「按分类」只剩单一「内置工具」组；按包得到 `QQ 适配器包(6)` / `内置工具集(16)` / `Subagent 委派(1)` / `工作区会话(1)`，与智能体能力勾选区同构
- 行卡主行 = 状态点 + 名称 + `包已禁用` 标记 + hover 快速启停；次行 = 描述（工具无触发词）
- 副标题：`共 N 项 · K 个包 · D 已禁用`
- **详情分区**：① 能力说明（启停是全局开关；启用时仍受各智能体白/黑名单约束——两层叠加）② 被谁使用（`disabled_tools` / `enabled_tools` 反查四态）③ 所属包（包 id + 「在「插件」中查看 →」跳转）④ 参数 Schema（**参数表**：参数 / 类型 / 必填（warning tag）/ 说明；右上角切 JSON 原文 + 复制；schema 无 `properties` 时回落 JSON 原文）

**插件**

- **按门控语义分组**（默认；分组维度可切「按 kind」「平铺」），把影响面最大的组提到最上并带组头语义提示：

| 分组 | 成员判定 | 组头提示 |
|---|---|---|
| 包维度门控 | `PACKAGE_GATED_PLUGIN_IDS`（非 adapter/management/loop） | 禁用 = 其包内全部工具/技能对所有智能体不可见 |
| 平台通道 | `kind ∈ {adapter, management}` | 平台连接与管理通道；管理面禁用会切断 Panel 通信（受保护） |
| 模式插件（互斥） | `kind === 'loop'` | 每智能体的循环模式在「智能体」配置里选；此处全局停用 = 卸载 echo-loop 驱动 |
| 其它 | 其余（如 provider.llm） | — |

- 行卡标记：包规模徽标（`N 工具 · M 技能`，来自现有 tools/skills 列表）· `🔒 受保护`（management.panel，**行内渲染锁标记而不给开关**）· `模式`（loop.*）
- 副标题：`共 N 项 · G 包维度门控 · M 模式插件 · D 已禁用`
- **详情分区**：① 这个插件带来什么（包内**工具/技能 chips，点击跳转到对应分类并选中**）② 被谁使用（`enabled_plugins` 白名单反查：「已启用」/「未列入白名单」）③ 启停影响（按门控语义分派文案；受保护插件为警示色说明如何在 core.toml 真正禁用）④ 技术信息（**折叠**，承载 ID / Package / kind·版本 / Entry / Author）
- `PLUGIN_KIND_LABELS.loop` 由 `Turn Runner` 改为「循环驱动」（全站统一的中文 kind 名）

**交叉跳转**（`jumpToCategory`）：插件详情成员 chip → 工具/技能分类并选中该条目；工具详情「所属包」按钮 → 插件分类并选中该插件——把「包 ⇄ 工具/技能」三层关系做成双向可点链路。

### 9.3 从 Git 仓库安装技能

技能工具条「Git 安装」打开**弹层**（2026-09-23：原内联表单会顶掉列表且安装后无反馈）：三字段——仓库 URL（https/ssh/本地路径，**唯一必填**，trim 后为空时按钮禁用）、安装目录名（默认取仓库名）、分支（默认取仓库默认分支）；后两者留空传 `null`。提交下发 `InstallSkillFromGit{url, name?, branch?}`（`protocol.ts`），提交后弹层关闭。Core 侧机制（shallow clone、目录提升、`.sources.json` 来源记录、更新/移除语义）见 [技能系统](./core-skills.md)「外部 Git 来源技能」。

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
- **已装 Git 技能**：详情页「来源」分区显示仓库 / 版本 / 安装时间与「更新」（`UpdateSkillFromGit`，无确认）、「移除来源」（confirm 明示目录保留）按钮；`⋯` 菜单同步提供「更新（Git）」「移除来源」入口

### 9.4 日志（整页查看器）

原日志视图的平移（`LogView.vue`），交互不变并新增手动「刷新」按钮：Core/Panel 分段切换（默认 Core），拉取 `/api/logs/{core,panel}?lines=100` 原文显示（max-height 65vh 内滚动）；自动刷新默认开（**5s** 间隔），可切手动；切换来源或自动开关立即重载并重启定时器；HTTP 错误显示状态码。**切换分类即卸载**——定时器随卸载清理，切回时重新挂载拉取。

