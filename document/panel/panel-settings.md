---
id: panel-settings
title: "Panel 设置视图"
group: 前端
x: 2880
y: 780
---

# Panel 设置视图

> **定位**：本文覆盖设置视图（左栏一级菜单 + 右侧分类工作区）的完整交互：视图结构、API 设置、资源工作区（技能 / 工具 / 插件（多 Core 管理节点、按运行位置的内部 / 外部分组）/ 智能体）、从 Git 安装技能（克隆进度、失败清场与残留清理）、Core 连接（含**并入侵的联邦网络**）、日志与系统。共**八个一级分类**（联邦不是独立分类，是同页内的下半区）。读者：维护 Panel 设置界面的开发者；相关：[交互定义](./panel-interaction.md)、[会话视图](./panel-chat.md)（Agent 配置弹层的 API 供应商引用）、[配置持久化](../core/core-config-persistence.md)。

## 设置视图结构

原「资源」「日志」视图与 API 设置弹窗合并为统一的设置视图：**左栏一级菜单 + 右侧分类工作区**。

- **一级菜单**（168px）：`API 设置`（徽标 = Profile 数）/ `技能` / `工具` / `插件` / `智能体`（徽标 = 条目数）/ `Core 连接`（`SettingsView.vue` 的 `categories`；**联邦已并入本页**：上游 Core + 联邦节点，副标题给 peers 计数）/ `系统`（自更新）/ `日志`（无徽标）——**共八分类**；菜单项 9px/12px 内边距、圆角 8px，选中 = 主色左边框 + `--panel-accent-soft` 底、徽标反白；底部「刷新」按钮拉取四类清单（Skills/Tools/Plugins/Teams，`refresh()`；插件按当前运行区域请求，工作台内切管理节点时另发定向请求，见「工具 / 插件工作台」）。切换分类**丢弃未保存的编辑态**；各分类的选中条目独立保持，切回时恢复
- **工具条**：标题 + 副标题（API = `当前：{provider} / {model}`（有激活 profile 时附 ` · Profile「{name}」`）；日志 = 来源说明；资源类 = `共 N 项 · K 已禁用`）+ 分类级操作（技能：Git 安装 / 新建技能；智能体：新建智能体）
- 打开视图时自动拉取四类清单（onMounted）；条目消失时选中态自动置空

## API 设置

`ApiSettings.vue`：**双层视图**（max-width 720px），配置表单不再常驻悬浮在页面顶部——默认只显示概览（当前配置 + Profiles 列表），表单仅在点击「编辑」或「添加 API 服务商」时展开。

**交互层级（两态）**

| 状态 | 触发 | 内容 |
|---|---|---|
| 概览视图（默认） | 进入 API 分类 / 表单收起/保存后 | Profiles 卡片网格、添加入口 |
| 编辑表单（展开态） | 点击 profile「编辑」/「+ 添加 API 服务商」 | 标题栏 + 快速模板 + 表单字段 + 操作行 |

**布局（自上而下）**

1. **编辑表单**（仅展开态，插入在列表上方）：标题栏（「编辑 Profile · {name}」+ 编辑中标签 /「新增 API 服务商」+ 收起按钮）→ 快速模板（6 个，点击填充）→ 表单字段 → 操作行（测试连接 / 保存 / 取消）
2. **API Profiles 卡片网格**（常驻，`repeat(auto-fill, minmax(240px, 1fr))`）：卡片本体用 **ui-frame `NeumorphismCard`**（elevation=2、radius=large、hoverable=bulge、no-padding），按槽位排版——header = 名称 +（编辑中标签）；body = `provider / model` + base_url + key/思考/推理摘要（min-height 62px 对齐）；footer = 余额行（DeepSeek）+ **操作行（测试 / 编辑 / 删除 三按钮等宽网格 `repeat(3, 1fr)`**，`DeleteApi` **无确认，立即生效**）；**编辑中的卡主色 outline 高亮**。**默认标记已移除**：卡片不再有「默认」标签与「设为默认」按钮——profiles 只是可供 persona 引用的供应商池，不再有"哪个是默认"的概念；模型选用在 persona 级（Agent 配置弹层）完成，API 设置页不提供「激活」「切换」
3. **DeepSeek 余额**：provider=deepseek 或 base_url 含 `deepseek.com` 的卡片显示「查余额」按钮——发 `QueryApiBalance{name}` 查官方 `/user/balance`（Anthropic/OpenAI/beta 端点自动归一到 `{root}/user/balance`），主金额下方附**分账行**（`充值 ¥xx · 赠送 ¥xx`，随 `granted`/`topped_up` 回推），结果显示为 `¥110.00`（CNY；USD 用 `$`，其他币种后缀显示），失败显示错误文本；查询加载态互斥、一次一个目标；查询成功即刷新该目标指标（新快照即时进图）
4. **余额与用量图表**（`ApiMetrics` 事件）：数据由 Core 本地积累——**余额快照**（周期任务默认 10 分钟采样；手动「查余额」成功也落一条）与 **token 用量**（每次 LLM 调用经计量装饰器记录，小时 × 模型聚合）；面板取**近 7 天**窗口，可视化三层：
   - **总览对比图**（Profiles 网格上方常驻，`formMode==='none'` 且有 profile 时显示）：「余额」与「用量（tokens / 小时）」两张 `NeumorphismChartLine`（height 220、smooth + area、**含坐标轴与横向网格**、legend 开；栅格 `minmax(420px, 1fr)`——宽屏并排各约 834px、窄屏堆叠。X 轴标签由 ui-frame 按宽度自动抽稀，168 个时间点渲染 6–11 个标签），多 profile 经**公共小时桶对齐**（余额前向填充、用量空桶补 0）；下方一行估算费用摘要（`近 7 天估算费用：<profile> 约 ¥x.xx …（按 [agent.pricing] 定价估算；缓存命中按命中价计，未配置命中价的模型按未命中价）`）。**余额采样**：手动「查余额」与周期任务互斥（审计发现：慢查询叠加周期 tick 造成同秒重复采样——共用锁、周期任务 try_lock 跳过）。
   - **卡片小图**：每张 profile 卡内嵌两张 sparkline（height 56、无轴无网格无点、legend 关；栅格 `minmax(150px, 1fr)`——卡宽 ≥约 330px 时两张并排）——「余额 · 近 7 天」曲线 + 「用量 · 近 7 天 N tokens · 约 ¥x」曲线；余额 ≥2 个采样点 / 有用量数据才显示；tooltip 金额与 token 缩写（K/M）格式化经 ui-frame `NeumorphismChartLine` 的 `valueFormatter` prop（本次新增，`(value, seriesName) => string`）
   - **页面布局**（已按满宽优化）：页面满宽（上限 1920px——此前硬限 720px，1920 视口闲置 58%）；profile 网格 `minmax(340px, 1fr)` + `grid-auto-rows: 1fr`（1920 下 4 列 × 413px、2560 下 5 列，同行卡片等高）；卡片 `flex` 纵向撑满、body 吃余高；footer 余额行与操作行 `flex-wrap`（220px 断点：单列宽卡并排、多列窄卡堆叠）。实测 1920 视口整页高 1719 → 1088（-37%）
   - **数据健壮性**：`api-metrics` 的 `plausibleBalances` 剔除 `total < 0` 的异常余额采样（上游偶发抖动，实测出现过 `-0.31`）——单点负值会把 Y 轴下限拽到负数（`-500…1500`），真实数据被压成窄带；0 视为合法（账户耗尽）
   - **刷新**：总览标题行「刷新」按钮（`QueryApiMetrics{name:''}` 全量）+ 每 5 分钟自动刷新；空数据显示「数据积累中」提示（说明采样周期）。工具函数与对齐算法在 `web/src/api-metrics.ts`（含单测）
5. **测试失败复制**：测试连接结果为失败（✗）时，结果行末尾附 **ui-frame `ChatCopyButton`**（`@echolab-auto/ui-frame/chat`，24px `nm-chat-copy` 图标钮，点击复制完整错误文本到剪贴板，复制定时反馈 ✓）——**仅 profile 卡**结果行（API 摘要卡已不存在，`.api-summary-*` 为残留样式）；结果文本整句 `title` 悬停可见（行内 ellipsis 截断时）。**非安全上下文兜底**：Panel 经局域网 IP 以 HTTP 访问时 `navigator.clipboard` 不可用（原本静默失败），`useClipboard` 已补 `textarea + document.execCommand('copy')` 兜底（已合入 ui-frame 上游 `4808f8b`，未发版期间由本地快照携带；安全上下文仍优先 Clipboard API）
6. **「+ 添加 API 服务商」按钮**（卡片网格底部常驻，primary）
7. **HTML 端点诊断**：base_url 指向网页根域而非 API 前缀时（响应 `text/html`），测试连接错误消息自动追加提示「该地址返回网页而非 JSON——请检查 Base URL 是否为 API 前缀（OpenAI 兼容端点通常以 /v1 结尾）」（OpenAI 客户端）；Anthropic 客户端文案为「…请检查 Base URL 是否正确（Anthropic 兼容端点通常以 /anthropic 结尾）」；解析错误与 HTTP 错误均适用

**表单字段**（展开态与字段规则同原设计）：

- **Profile 名称**：添加模式必填（trim 非空才可保存）；**编辑模式可修改**（原名预填，保存时走两步提交——先 `RenameApi` 原子改名，确认成功后字段有改动才按新名补发 `UpdateApiConfig`；纯改名不重保存、不误激活。空白/与现有 profile 撞名实时提示并禁用保存；确认前保存按钮锁定，10s 未确认提示失败并保留表单）
- **快捷模板**（6 个，点击填充 provider/base_url/model/思考/推理）：**DeepSeek**（合并原「Anthropic 兼容 / OpenAI 兼容」两种格式为单一预设——默认官方 Anthropic 端点 `https://api.deepseek.com/anthropic` + `deepseek-v4-flash`，需 OpenAI 格式时手工改为 `/v1` + `deepseek-chat`，提示文字已注明）、**Kimi For Coding**（Kimi Code 订阅的 Anthropic 兼容端点 `https://api.kimi.com/coding` + `k3`，Key 在 `www.kimi.com/code/console` 创建，推理档位 high；provider=`kimi`）、**中转站（OpenAI 兼容）**（provider=`third-party`，base_url/model **留空待填**，占位提示"你的网关/v1"与"中转站模型名"；请求为干净 OpenAI 格式，不携带 DeepSeek 专有 thinking 字段）、OpenAI、Anthropic、Ollama（本地）
- **Provider 下拉**：固定 deepseek/openai/kimi/third-party 四项，未知值保留为额外选项；选 deepseek/openai/kimi 自动填充对应 base_url/model；第三方需 OpenAI 兼容（有 /anthropic 端点自动识别）。**注意**：中转站=第三方时不会附加 DeepSeek 专有请求字段（`thinking`/`reasoning_effort`），兼容性最好；若要保留思考模式且后端是 DeepSeek，选 deepseek 预设
- Model / Base URL（手填）、**API Key**（密码框；标注 已设置/未设置，**留空保持不变**，不回显）、思考模式（启用/禁用）、推理强度（low/high/max）
- 添加模式初始为空表单（Provider 默认 third-party），快速模板可一键填充

**状态转换规则**

- 进 API 分类、或切走再切回：默认概览态（表单收起）；**切换一级分类丢弃未保存的编辑态**
- 点「编辑」→ 表单展开填充该 profile（apiKey 留空）；点「添加」→ 展开空表单
- **保存成功（`state.api` 回播后）→ 表单收起**，回到概览；编辑目标行取消「编辑中」，列表/摘要卡随回播刷新（新增/更新 profile 不改变任何 persona 的选用——persona 在各自 Agent 配置里引用）
- 「取消/收起」→ 丢弃表单修改，回到概览
- 测试连接：`TestApi`（表单目标 = 编辑中的 profile 名，添加模式为空 = 默认配置）；加载态互斥（一次仅测一个）；**每次表单展开时清空上次测试结果**

后端命令映射：`UpdateApiConfig`（name 空 = 全局默认配置；非空 = 供应商池增改）/ `RenameApi`（编辑态改名；原子重写全部按名引用——池名 / `active_api` / `teams[*].api_profile` / 运行中 persona、会话覆盖与工作区对话引用；语义见 [配置持久化](../core/core-config-persistence.md)§改名语义）/ `TestApi` / `DeleteApi` / `QueryApiBalance`（余额查询 + 快照落盘）/ `QueryApiMetrics`（余额/用量时间序列，回 `ApiMetrics`）；`SwitchApi` 保留协议但 UI 不再暴露（persona 级选用取代全局激活）。数据流见 [配置持久化](../core/core-config-persistence.md)。

### Persona 级 API（`api_profile` 引用）

**模型**：全局 `[agent].api_profiles` 是唯一供应商池；`[agent.teams.{id}].api_profile = "name"` 只做引用（None = 跟随全局默认配置）。不内嵌供应商值——增删改在设置页完成，所有 persona 共享最新池。

**入口**：聊天区入口行 →「配置」按钮 → Agent 配置弹层新增「API 供应商」下拉（首选「跟随全局默认配置」，其余为池中 profile，显示 `name（provider / model · key 状态）`）；保存 `SaveTeam{api_profile}`。

**运行期生效**：`SaveTeam` 成功后 target agent 立即重建自己的 provider（`apply_persona_api`：按引用从全局池解析 → `create_provider` 独立构建 → 更新 model）；启动期 make_agent 对配置了 `api_profile` 的 persona 直接构建独立 provider，不共享默认 provider。

**持久化**：写入 `[agent.teams.{id}].api_profile`（TOML）；`TeamInfo.api_profile` 回推 Panel；AgentSwitcher 卡片/列表显示当前 persona 生效模型（池解析，未引用 = 全局默认 model）。

## 资源工作区（技能 / 工具 / 插件 / 智能体）

**条目列表**（技能工作台 320px 可拖拽；其余分类 250px，与详情面板并排，组头可折叠）：

- 技能/工具/插件均为**工作台**（筛选栏 + 可拖宽列表 + 双行行卡）：技能与工具默认**按包（package）分组**（package 是真实门控单位，与智能体能力勾选区一致；无包条目归「独立技能」/「独立工具」），插件默认**按运行位置分组**（内部 / 外部，见下）；智能体平铺（**按 id 排序**，无置顶——「主智能体」概念已去除）
- 条目行：名称（超长省略）+ 行尾标记（Git 来源技能 `Git` 标签 / `已禁用`；**不再有「主」标签**）；选中 = 主色左边框 + 浅底；空列表显示「（暂无…）」
- 点选条目在右侧打开详情；点选智能体**直接进入编辑态**
- **清单加载三态**：四类清单各自独立跟踪（`settings:{skills,tools,plugins,teams}`）——列表区顶部 `LoadHint`（加载转圈 / >6s 慢提示 / >20s 错误+重试），加载中**计数徽标暂隐**、空态「（暂无…）」让位；顶部「刷新」按钮加载中转圈

| 对象 | 详情内容 | 操作 |
|---|---|---|
| 技能 | 见下方「技能工作台」小节 | 启停开关（`ToggleSkill`，列表行内 hover 亦有快速开关）、编辑（名称锁定）、删除（confirm）；**Git 来源技能追加**：更新（`UpdateSkillFromGit`，无确认）、移除来源（confirm「目录保留，可手动删除」→ `RemoveSkillSource`） |
| 工具 | 见下方「工具 / 插件工作台」小节 | 启停开关（`ToggleTool`，禁用后模型不可见；列表行内 hover 亦有快速开关） |
| 插件 | 见下方「工具 / 插件工作台」小节 | 启停开关（`TogglePlugin`，禁用即卸载注册；在选定管理节点上全局生效；**「管理面」插件禁用被保护**——core 拒绝 + UI 明示且行内不给开关，防 Panel 自锁断连）。**包级聚合**：包内工具与技能随包一起启停（见 [插件化设计](../plugins/core-plugins.md)「Package」） |
| 智能体 | N 会话/循环模式标签（单会话/并行多会话）、提示词、能力白名单、禁用能力（`disabled_tools`/`disabled_skills` 的 error 色标签组；插件黑名单已移除） | 选中即进编辑态；启停（`ToggleTeam`）；任意智能体可删（**至少保留一个**；无受保护成员） |

### 技能工作台

背景：技能全部没有 `category`（实测 9 个技能全落进单一「未分类」组）、列表只显示名字（启停要逐个点进详情）、「这技能什么时候会进来」「关掉它会影响谁」两个问题在界面上答不出来；工作台为此重排：

- **筛选栏**（技能分类专属，工具条下方一行）：搜索框（命中名称 / 描述 / 触发词）+ 状态 chips（`全部 / 已启用 / 已禁用 / 常驻 / 系统提示词 / Git 来源`，各带计数）+ 分组维度分段（`按包`（默认）/ `按分类` / `平铺`）；工具条副标题升级为构成统计（`共 N 项 · K 已禁用 · A 常驻 · S 系统提示词 · G Git 来源`）
- **列表行卡（双行）**：主行 = 状态点（实心=启用 / 空心=禁用，禁用整行降透明度）+ 名称 + 行尾标记（`★`常驻 / `⇧`系统提示词 / `Git` / `包已禁用`）+ **hover 浮出的快速启停开关**（不改变选中，`@click.stop`）；次行 = 描述（单行截断）+ 触发词前 3 个（`+N`）；所属包插件被禁用时组头追加提示「所属包已禁用，包内技能当前不可用」
- **分栏可拖拽**：列表与详情之间的分隔条拖动调宽（220–560px，`localStorage` 持久化 `echo-panel-caps-list-width`）
- **详情分区检查器**（头部 = 名称 + 包名 + 启停开关 + `⋯` 菜单「编辑 / 更新（Git）/ 移除来源 / 删除」）：
  1. **生效方式**：关键词触发（触发词 chips **可增删**——删除走 `closable` tag，添加走行内输入 + Enter；就地修改经 `SaveSkill` 补丁回写，保留其余字段）、常驻注入开关（`always`）、系统提示词开关（`system`，开启前 confirm 明示「注入 base 区，影响所有启用它的智能体」）
  2. **被谁使用**：按 `TeamsList` 反查每个智能体对本技能的状态——`系统提示词注入`（`system_skills`）/ `已禁用`（`disabled_skills`）/ `未列入白名单`（`enabled_skills` 非空且不含）/ `可用`
  3. **来源**：两态——**无来源记录**（「新建技能」创建或手工放置的目录；`SkillInfo.source` 为空，详情显示「内置技能（随 Core 分发的 skills 目录）」，文案不区分本地新建）与 **Git 来源**（`SkillInfo.source` 回推：仓库 / 版本（含分支）/ 安装时间 + 更新 / 移除来源按钮）；Git 安装的 URL 可为本机 / 网络路径，一律 clone 并记来源（见「从 Git 仓库安装技能」）
  4. **内容**：frontmatter 摘要条（分类 · 常驻 · 系统提示词 · 触发词数）+ Markdown **预览 / 源码**切换（预览复用 ui-frame `MarkdownRenderer`）
  - 组合并提示：所属包插件禁用、或技能自身禁用时，详情顶部显示对应横幅
- **技能编辑器**：名称（编辑时锁定）、描述、触发词（逗号分隔）、分类、常驻开关、Markdown 正文（12 行自适应）；**吸底操作条**显示保存状态（`● 有未保存的更改` / `○ 无更改`）+ 放弃更改 / 保存；切换条目或切换分类前若有脏改动会先 confirm；保存 `SaveSkill`

### 智能体编辑器

顶部**头部信息卡**（ui-frame `NeumorphismCard`：首字头像 + 名称 + 启用状态标签 + `ID · 启用态·循环模式` 摘要行；**无「主 Agent」标签**），下方用 `NeumorphismCollapse` 分三个折叠分区（原第四个「禁用的能力」随插件黑名单移除），**默认展开前两个**（避免超长滚动），标题行右侧带实时摘要：

1. **基础信息**（摘要 `启用 · 单会话`）：ID 必填 + `/^[A-Za-z0-9_-]+$/`（编辑时锁定，留空回退为名称）、名称必填、描述、启用开关、**循环模式分段单选**（单会话（默认）/ 并行多会话，互斥——写入 `enabled_plugins` 白名单的 `echo-agent.loop.{single,parallel}` 插件 id；空表即单会话）
2. **系统提示词**（摘要 `自定义提示词/继承全局 · N 个系统技能`）：提示词文本域 + 系统提示词 skills 复选（双列网格）
3. **启用的能力**（摘要 `N 插件 · N 工具 · N 技能`，空表显示「不限制（全部可用）」）：四个子块以 `NeumorphismDivider` 分隔——**插件**（`kind ∈ {adapter, management, interaction}` ∪ 包维度门控插件固定清单 tools.builtin / skills.dir / workspace / subagent（与 `capabilities.ts::PACKAGE_GATED_PLUGIN_IDS` 逐项镜像；适配器不经清单，按 `kind == "adapter"` 运行期推导，新增平台零改动），判定单一来源 = `capabilities.ts::isPluginCheckboxVisible`；两者之外的插件如 loop.* 不出现；标签显示中文 kind 名，单一来源 `capabilities.ts::PLUGIN_KIND_LABELS`）、**包（Package）**（一次切换整包工具+技能，附成员清单与项数徽标；显示名单一来源 = `capabilities.ts::PACKAGE_DISPLAY_NAMES`）、**工具**（按分类分组，自适应多列）、**技能**

> 原「禁用的能力」（插件黑名单复选）分区已移除——插件维度只保留白名单（空表 = 全部启用，取消勾选即物化），既有黑名单配置由 Core 加载期物化进白名单。

粘性页脚 删除/放弃更改/保存（`SaveTeam`）。

- **详情视图**：取消编辑后回到只读详情；头部新增「编辑」按钮（重新进入编辑器，无需再点列表项）
- `builtin` 类工具归入「内置工具」组

### 工具 / 插件工作台

与技能工作台同构（筛选栏 + 双行行卡 + 分栏可拖 + 详情分区），差异点：

**工具**

- **按包分组**（默认）：工具按 `package` 归组，组名取 `PACKAGE_DISPLAY_NAMES`（如「内置工具集」「QQ 适配器包」「Subagent 委派」「工作区会话」），与智能体能力勾选区同构。**原「按分类」已退化**：内置工具的 `category` 一律 `builtin`，全归一组；只有进程外插件贡献的工具带 `category = plugin`——分类维度因此没有区分度，按包才是有效维度
- 行卡主行 = 状态点 + 名称 + `包已禁用` 标记 + hover 快速启停；次行 = 描述（工具无触发词）
- 副标题：`共 N 项 · K 个包 · D 已禁用`
- **详情分区**：① 能力说明（启停是全局开关；启用时仍受各智能体白/黑名单约束——两层叠加）② 被谁使用（`disabled_tools` / `enabled_tools` 反查四态）③ 所属包（包 id + 「在「插件」中查看 →」跳转）④ 参数 Schema（**参数表**：参数 / 类型 / 必填（warning tag）/ 说明；右上角切 JSON 原文 + 复制；schema 无 `properties` 时回落 JSON 原文）

**插件**

插件工作台回答三件事：在哪个 Core 上管理、这条插件的实现在不在 Core 进程内、它门控了什么——第一个决定列表内容，第二个是默认分组维度，第三个是可切换的语义维度。

- **管理节点**（筛选栏首项，`NeumorphismSelect`，`aria-label="管理节点"`）：选项 = 已接入上游名 ∪ 人格归属 core ∪ `state.pluginsByCore` 的键 ∪ `activeRegion`（`pluginCoreOptions`），标签经 `state.ts::regionLabel`（区域显示名，缺失回落节点 id）；切换即清空选中并定向 `RequestPluginsList`（带目标 core）。列表、筛选计数、副标题统计、行内快速启停与 `TogglePlugin` 的目标 core 统一取 `pluginCoreEffective`——**显示的 = 请求的 = 修改的**
- **按节点归档的清单**（`state.pluginsByCore` / `pluginCatalog`）：各节点回推的插件按上游名归档（中继不加信封的单上游场景键为空串）；`pluginCatalog` 的取数顺位 = 显式选定 / `activeRegion` 的归档 → **唯一来源**（键对不上但只有一份数据）→ `state.plugins`（管理目标兼容视图）；多来源且该节点尚未回推时给空列表，**绝不回退**（否则串显别个节点）
- **分组维度**（`PLUGIN_GROUP_OPTIONS` 分段控件）：`内部 / 外部`（默认）/ `按门控语义` / `按 kind` / `平铺`
- **内部 / 外部按运行位置判定**（`pluginSource`）：`runtime === 'subprocess'` → 外部，`in_process` → 内部；旧 Core 未回推 `runtime` 时回退 `builtin`（`false` → 外部）。判据是「实现在哪跑」而非「manifest 从哪来」——QQ 适配器随 Core 发布（`builtin = true`）但实现在独立进程，因此归**外部插件**
- **组头提示**：外部组 = 「实现在独立进程（独立 PID / 生命周期）；随 Core 发布或来自插件目录」（讲清「随 Core 发布 ≠ 进程内」）；内部组 = 「实现编译在 Core 进程内；随 Core 发布」
- **行卡标记**：`独立进程`（`pluginIsSubprocess`，与外部组等价）· 包规模徽标（`N 工具 · M 技能`，经 `pluginMembers` → `packageMembers` 从 `state.plugins` 与 `state.tools`/`state.skills` 反查——这两份是单份状态、只随管理目标节点更新，故切到非管理目标节点时徽标仍按管理目标口径统计）· `🔒 受保护`（management.panel，**行内渲染锁标记而不给开关**）· `模式`（loop.*）
- **副标题随分组维度**：按来源 = `共 N 项 · E 外部插件 · I 内部插件 · D 已禁用`；语义分组 = `共 N 项 · G 包维度门控 · M 模式插件 · D 已禁用`
- **协议契约**：`PluginInfo.runtime`（`in_process` / `subprocess`，缺失 = 进程内）与 `PluginInfo.builtin`（manifest 是否编译进 Core，缺失按 `true` 兼容）均由 Core 回推、彼此正交；来源分组（`pluginSource`）与运行形态标签（`pluginRuntimeLabel`，`PLUGIN_RUNTIME_LABELS`：独立进程 / 进程内）同源于这两个字段，Panel 不做推断性改写
- **详情分区**：① 这个插件带来什么（包内**工具/技能 chips，点击跳转到对应分类并选中**）② 被谁使用（`enabled_plugins` 白名单反查：「已启用」/「未列入白名单」）③ 启停影响（按门控语义分派文案；受保护插件为警示色说明如何在 core.toml 真正禁用）④ 技术信息（**折叠**，承载 ID / Package / kind·版本 / Entry / **运行形态** / Author）。头部左侧是语义标签（`PLUGIN_SEMANTIC_LABELS`），外部插件另加 `外部插件 · 独立进程` 标签
- `PLUGIN_KIND_LABELS.loop` 为「循环驱动」（全站统一的中文 kind 名）

**门控语义分组**（`pluginSemantic`，可切换维度；把影响面最大的组提到最上并带组头提示）：

| 分组 | 成员判定 | 组头提示 |
|---|---|---|
| 包维度门控 | `PACKAGE_GATED_PLUGIN_IDS`（非 adapter/management/loop） | 禁用 = 其包内全部工具/技能对所有智能体不可见 |
| 平台通道 | `kind ∈ {adapter, management}` | 平台连接与管理通道；管理面禁用会切断 Panel 通信（受保护） |
| 模式插件（互斥） | `kind === 'loop'` | 每智能体的循环模式在「智能体」配置里选；此处全局停用 = 卸载 echo-loop 驱动 |
| 其它 | 其余（如 provider.llm） | — |

**交叉跳转**（`jumpToCategory`）：插件详情成员 chip → 工具/技能分类并选中该条目；工具详情「所属包」按钮 → 插件分类并选中该插件——把「包 ⇄ 工具/技能」三层关系做成双向可点链路。包规模徽标与成员 chip 的成员名单取自 `state.tools` / `state.skills`（管理目标视图），切到非管理目标的节点时仍按管理目标统计。

## 从 Git 仓库安装技能

技能按来源分两态：**无来源记录**（「新建技能」创建或手工放进 skills_dir 的目录；详情「来源」显示「内置技能（随 Core 分发的 skills 目录）」——文案不区分本地新建与随 Core 分发）与 **Git 来源技能**（`SkillInfo.source` 回推仓库 / 版本 / 分支 / 安装时间；列表行 `Git` 徽标、筛选 chip「Git 来源」据此判定）。Git 安装接受 https / ssh / 本机路径，一律走 `git clone` 并写入 `skills_dir/.sources.json`。

技能工具条「Git 安装」打开**弹层**（原内联表单会顶掉列表且安装后无反馈）：三字段——仓库 URL（**唯一必填**，trim 后为空时按钮禁用）、安装目录名（默认取仓库名）、分支（默认取仓库默认分支）；后两者留空传 `null`。提交下发 `InstallSkillFromGit{url, name?, branch?}`（`protocol.ts`）。Core 侧机制（shallow clone、目录提升、`.sources.json` 来源记录、更新 / 移除语义）见 [技能系统](../plugins/core-skills.md)「外部 Git 来源技能」。

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
- **成功判据**：以技能清单变化为准（`watch(state.skills)` 的名称序列）——Core 装完推 `SkillsList` 即自动关窗并清空表单，无需用户再操作
- **失败反馈**：结果由 Core `Error` 事件以 toast 呈现；界面**不回收安装态**——`gitInstalling` 只由技能清单变化复位（成功路径），失败与超时不会走到那里，于是进度继续累计、URL 仍禁用、关窗重开亦然，需刷新页面才能重试
- **失败清场与残留**（Core `skills_dir::install`）：clone 失败即删除目标目录，本次与后续重试都不会撞「目录已存在」；目标目录已存在时先按 `is_broken_clone`（有 `.git` 且无 `SKILL.md`）识别上次被超时 kill 的残留并清掉重装，正常技能目录（有 `SKILL.md`）不会被误删——报「目录已存在」并提示卸载 / 换名 / 移走手动放置的同名目录；clone 成功但仓库内找不到 `SKILL.md` 同样清场后报错。`GIT_TERMINAL_PROMPT=0` 让私有仓库缺凭据时立即报认证失败，而不是在无 TTY 的后台进程里挂满超时；硬超时 120s（`GIT_TIMEOUT`）之外另有低速中止（<1KB/s 持续 30s）
- **弹层静态提示**：浅克隆 + 目录提升说明；建议私有 / 内网仓库用 SSH 地址（`git@github.com:org/repo.git`）——HTTPS 被网络阻断时只能等 120s 超时失败
- **已装 Git 技能**：详情页「来源」分区显示仓库 / 版本 / 安装时间与「更新」（`UpdateSkillFromGit`，无确认、无进度面板，仅命令 + `SkillsList` / `Error` 反馈）、「移除来源」（confirm 明示目录保留）按钮；`⋯` 菜单同步提供「更新（Git）」「移除来源」入口。来源记录按技能名（frontmatter `name`）索引、目录名可不同，故更新 / 移除来源按技能名寻址

## Core 连接与联邦

- **Core 连接**（接入点管理）：在设置页在线增删接入点（写回 `[[cores]]` 段持久化，中继动态并入 / 摘除、无需重启）；单接入点兜底（`[core]` 或一条 `[[cores]]`）与访问令牌、分布式调度见 [Panel 概览](./panel.md)「配置与运维」
  - **接入方式**：直连 ｜ **SSH 隧道**——云服务器只开 SSH 端口时选后者：表单含 SSH 主机/端口（默认 22）/用户（须为运行 Core 的用户）/私钥文件（可空 → ssh-agent / 默认密钥）/口令或密码（可选，编辑留空 = 不变）/远端 management 端口（默认 3132）/远端媒体目录（可空 = 默认）。Panel 后端以 russh 直连 SSH 并把远端 management 端口**本地端口转发**给中继（隧道指数退避自动重连、本地端口稳定；主机密钥 known_hosts 校验 + 首连 TOFU，`accept_new_host_key=false` 关闭）；列表行展示 `ssh://user@host → :远端端口`、SSH 徽标与隧道阶段（连接中/已连接/离线 + 失败原因）。编辑既有 SSH 上下游时密文留空 = 保留旧值（与访问令牌同语义）
- **联邦网络**（同一分类下半区，已改为**组建向导**）：
  - **创建联邦 / 添加节点**：点按钮打开向导（顶部按钮随拓扑状态切换文案）——勾选要纳入的节点（默认全选**在线**的已接入 Core；离线节点禁选），显示「已互联 X 对 / 将新建 Y 对」（全互联补全），执行时逐对完成「一侧生成邀请 → 另一侧登记接入 → 等链路在线」；默认方向失败**自动换向重试一次**，两方向均失败则清理已登记条目并标记失败（可「重试失败项」）。已配置的对自动跳过（含离线链路——重连是链路自身行为，无需重新配对）
  - **拓扑视图**（无向导时的上半区）：节点间链路一览（`a ── b`，在线绿色加粗），数量摘要「N 个节点 · M/K 条链路已配置 · X 条在线」。互联判定：链路在线时按 Core 回传的对端 `node_id` 精确匹配；离线/旧 Core 缺失时按 peer 名兜底（Panel 配置名 / node_name / `node-<id 前 8>`）
  - **手动通道**（每个 Core 卡片内保留）：生成邀请串（一次性 token，含共享密钥按密钥级保密）、添加/编辑 peer（粘贴 `echofed://` 邀请串自动解析）、逐项收紧授权（工具/查询/委派/需确认）；编辑回填现有授权，不以默认覆盖
  - 联邦网络的架构、占位提升、配对无方向性与安全模型见 [联邦（多机）](../federation/federation.md)
## 日志与系统

- **日志**（整页查看器，`LogView.vue`）：Core/Panel 分段切换（默认 Core），拉取 `/api/logs/{core,panel}?lines=100` 原文显示（max-height 65vh 内滚动）；自动刷新默认开（**5s** 间隔），可切手动，另有手动「刷新」按钮；切换来源或自动开关立即重载并重启定时器；HTTP 错误显示状态码。**切换分类即卸载**——定时器随卸载清理，切回时重新挂载拉取。**SSH 上游日志**：存在 SSH 接入的上游时，Core 视图追加「本机 / 各 SSH 上游」选择器——选中 SSH 上游则请求带 `?core=<上游名>`，Panel 后端经隧道在远端执行 journalctl（两代单元名 `-fixed` 与现役**合并**取最新 N 行；失败给出可操作提示）；选中上游被删除/改直连自动回退「本机」
- **系统**（自更新）分类：更新服务与流程见 [部署与自更新](../ops/ops-deploy.md)
