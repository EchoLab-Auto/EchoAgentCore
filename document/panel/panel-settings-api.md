---
id: panel-settings-api
title: "设置 · API"
group: 前端
x: 2560
y: 1060
---

# 设置 · API

> **定位**：`ApiSettings.vue` 的双层视图——供应商池（profiles）的增删改与测试、DeepSeek 余额查询、近 7 天余额与用量图表，以及 persona 级的 `api_profile` 引用。设置视图外壳、菜单与切换语义见[设置视图](./panel-settings.md)。读者：维护 API 设置界面的开发者；相关：[配置持久化](../core/core-config-persistence.md)、[桌面端模态与覆盖层](./panel-desktop-overlays.md)（Agent 配置弹层的供应商下拉）。

## 双层视图

页面满宽（上限 1920px，见下「页面布局」），配置表单**不再常驻**悬浮在页面顶部——默认只显示概览（当前配置 + Profiles 列表），表单仅在点击「编辑」或「添加 API 服务商」时展开。

| 状态 | 触发 | 内容 |
|---|---|---|
| 概览视图（默认） | 进入 API 分类 / 表单收起/保存后 | Profiles 卡片网格、添加入口 |
| 编辑表单（展开态） | 点击 profile「编辑」/「+ 添加 API 服务商」 | 标题栏 + 快速模板 + 表单字段 + 操作行 |

布局自上而下：① 编辑表单（仅展开态，插入在列表上方：标题栏「编辑 Profile · {name}」+ 编辑中标签 /「新增 API 服务商」+ 收起按钮 → 快速模板（6 个，点击填充）→ 表单字段 → 操作行「测试连接 / 保存 / 取消」）② 总览对比图 ③ Profiles 卡片网格 ④ 「+ 添加 API 服务商」按钮（网格底部常驻，primary）。

## Profiles 卡片网格

网格 `repeat(auto-fill, minmax(340px, 1fr))` + `grid-auto-rows: 1fr`（1920 视口 4 列 × 413px、2560 下 5 列，同行等高）；卡片本体用 ui-frame `NeumorphismCard`（elevation=2、radius=large、hoverable=bulge、no-padding），按槽位排版：

- **header** = 名称 +（编辑中标签）
- **body** = `provider / model` + base_url + key/思考/推理摘要（min-height 62px 对齐）
- **footer** = 余额行（DeepSeek）+ **操作行（测试 / 编辑 / 删除三按钮等宽网格 `repeat(3, 1fr)`**，`DeleteApi` **无确认，立即生效**）
- **编辑中的卡主色 outline 高亮**；卡片 `flex` 纵向撑满、body 吃余高；footer 余额行与操作行 `flex-wrap`（220px 断点：单列宽卡并排、多列窄卡堆叠）

**默认标记已移除**：卡片不再有「默认」标签与「设为默认」按钮——profiles 只是可供 persona 引用的供应商池，不再有「哪个是默认」的概念；模型选用在 persona 级（Agent 配置弹层）完成，API 设置页不提供「激活」「切换」。

配套交互：

- **测试连接**：`TestApi`（表单目标 = 编辑中的 profile 名，添加模式为空 = 默认配置）；加载态互斥（一次仅测一个）；**每次表单展开时清空上次测试结果**；结果为失败（✗）时结果行末尾附 ui-frame `ChatCopyButton`（`@echolab-auto/ui-frame/chat`，24px `nm-chat-copy` 图标钮，点击复制完整错误文本，复制定时反馈 ✓）——**仅 profile 卡**结果行（API 摘要卡已不存在，`.api-summary-*` 为残留样式）；结果文本整句 `title` 悬停可见。**非安全上下文兜底**：Panel 经局域网 IP 以 HTTP 访问时 `navigator.clipboard` 不可用，`useClipboard` 已补 `textarea + document.execCommand('copy')` 兜底（安全上下文仍优先 Clipboard API）
- **HTML 端点诊断**：base_url 指向网页根域而非 API 前缀时（响应 `text/html`），测试连接错误消息自动追加提示「该地址返回网页而非 JSON——请检查 Base URL 是否为 API 前缀（OpenAI 兼容端点通常以 /v1 结尾）」（OpenAI 客户端）；Anthropic 客户端文案为「…请检查 Base URL 是否正确（Anthropic 兼容端点通常以 /anthropic 结尾）」；解析错误与 HTTP 错误均适用

## 余额与用量图表

数据由 Core 本地积累——**余额快照**（周期任务默认 10 分钟采样；手动「查余额」成功也落一条）与 **token 用量**（每次 LLM 调用经计量装饰器记录，小时 × 模型聚合）；面板取**近 7 天**窗口，经 `ApiMetrics` 事件可视化三层：

- **总览对比图**（Profiles 网格上方常驻，`formMode==='none'` 且有 profile 时显示）：「余额」与「用量（tokens / 小时）」两张 `NeumorphismChartLine`（height 220、smooth + area、**含坐标轴与横向网格**、legend 开；栅格 `minmax(420px, 1fr)`——宽屏并排各约 834px、窄屏堆叠；X 轴标签由 ui-frame 按宽度自动抽稀），多 profile 经**公共小时桶对齐**（余额前向填充、用量空桶补 0）；下方一行估算费用摘要（`近 7 天估算费用：<profile> 约 ¥x.xx …`，按 `[agent.pricing]` 定价估算，缓存命中按命中价计）
- **卡片小图**：每张 profile 卡内嵌两张 sparkline（height 56、无轴无网格无点、legend 关；栅格 `minmax(150px, 1fr)`——卡宽 ≥约 330px 时两张并排）——「余额 · 近 7 天」+「用量 · 近 7 天 N tokens · 约 ¥x」；余额 ≥2 个采样点 / 有用量数据才显示；tooltip 金额与 token 缩写（K/M）格式化经 ui-frame `NeumorphismChartLine` 的 `valueFormatter` prop
- **DeepSeek 余额**：provider=deepseek 或 base_url 含 `deepseek.com` 的卡片显示「查余额」按钮——发 `QueryApiBalance{name}` 查官方 `/user/balance`（Anthropic/OpenAI/beta 端点自动归一到 `{root}/user/balance`），主金额下方附**分账行**（`充值 ¥xx · 赠送 ¥xx`，随 `granted`/`topped_up` 回推），结果显示为 `¥110.00`（CNY；USD 用 `$`，其他币种后缀显示），失败显示错误文本；查询加载态互斥、一次一个目标；查询成功即刷新该目标指标（新快照即时进图）。**余额采样互斥**：手动「查余额」与周期任务共用锁，周期任务 `try_lock` 跳过（避免慢查询叠加 tick 造成同秒重复采样）
- **数据健壮性**：`api-metrics` 的 `plausibleBalances` 剔除 `total < 0` 的异常余额采样（上游偶发抖动，实测出现过 `-0.31`）——单点负值会把 Y 轴下限拽到负数，真实数据被压成窄带；0 视为合法（账户耗尽）
- **刷新**：总览标题行「刷新」按钮（`QueryApiMetrics{name:''}` 全量）+ 每 5 分钟自动刷新；空数据显示「数据积累中」提示（说明采样周期）。工具函数与对齐算法在 `api-metrics.ts`

## 配置表单与快捷模板

- **Profile 名称**：添加模式必填（trim 非空才可保存）；**编辑模式可修改**（原名预填，保存时走两步提交——先 `RenameApi` 原子改名，确认成功后字段有改动才按新名补发 `UpdateApiConfig`；纯改名不重保存、不误激活）。空白/与现有 profile 撞名实时提示并禁用保存；确认前保存按钮锁定，10s 未确认提示失败并保留表单
- **快捷模板**（8 个，点击填充 provider/base_url/model/思考/推理）：**DeepSeek**（默认官方 Anthropic 端点 `https://api.deepseek.com/anthropic` + `deepseek-v4-flash`；需 OpenAI 格式时手工改为 `/v1` + `deepseek-chat`）、**Kimi For Coding**（Anthropic 兼容端点 `https://api.kimi.com/coding` + `k3`，Key 在 Kimi Code 控制台创建，推理档位 high；provider=`kimi`）、**中转站（OpenAI 兼容）**（provider=`third-party`，base_url/model **留空待填**，占位提示「你的网关/v1」与「中转站模型名」；请求为干净 OpenAI 格式，不携带 DeepSeek 专有 thinking 字段）、**OpenCode Go**（订阅制，`https://opencode.ai/inference/go/openai/v1` + `kimi-k3`；Key 在 opencode.ai Console 创建，形如 `oc_sk_…`；provider=`opencode-go`，密钥走 `OPENCODE_API_KEY`）、**OpenCode Zen**（按量付费，`https://opencode.ai/inference/openai/v1`，与 Go 共用同一个 Console Key）、OpenAI、Anthropic、Ollama（本地）
- **OpenCode 的两种形态**：Go/Zen 同时提供 Chat Completions（`/openai/v1`，Kimi K3 / GLM-5.x / DeepSeek V4 等）与 Anthropic Messages（`/anthropic`，Claude Haiku / Qwen / MiniMax 等）两套端点——**把 base_url 写成 `/anthropic` 结尾即自动切 Messages 客户端**，provider 名不必改。注意 GPT-5.6/6、Grok、Muse Spark 只提供 OpenAI **Responses** API，本框架未实现，接不了；Gemini 原生端点同理
- **Provider 下拉**：固定 deepseek / openai / kimi / opencode-go / opencode / third-party 六项，未知值保留为额外选项；选已知项自动填充对应 base_url/model；第三方需 OpenAI 兼容（有 /anthropic 端点自动识别）。**注意**：中转站=第三方时不会附加 DeepSeek 专有请求字段（`thinking`/`reasoning_effort`），兼容性最好；若要保留思考模式且后端是 DeepSeek，选 deepseek 预设

- Model / Base URL（手填）、**API Key**（密码框；标注 已设置/未设置，**留空保持不变**，不回显）、思考模式（启用/禁用）、推理强度（low/high/max）
- 添加模式初始为空表单（Provider 默认 third-party），快速模板可一键填充

## 状态转换与命令映射

- 点「编辑」→ 表单展开填充该 profile（apiKey 留空）；点「添加」→ 展开空表单
- **保存成功（`state.api` 回播后）→ 表单收起**，回到概览；编辑目标行取消「编辑中」，列表随回播刷新（新增/更新 profile 不改变任何 persona 的选用——persona 在各自 Agent 配置里引用）
- 「取消/收起」→ 丢弃表单修改，回到概览

后端命令映射：

| 命令 | 语义 |
| --- | --- |
| `UpdateApiConfig` | name 空 = 全局默认配置；非空 = 供应商池增改 |
| `RenameApi` | 编辑态改名；原子重写全部按名引用（池名 / `active_api` / `teams[*].api_profile` / 运行中 persona、会话覆盖与工作区对话引用），见[配置持久化](../core/core-config-persistence.md)「改名语义」 |
| `TestApi` | 测试连接（表单目标；添加模式为空 = 默认配置） |
| `DeleteApi` | 删除 profile（UI 无确认，立即生效） |
| `QueryApiBalance` | 余额查询 + 快照落盘 |
| `QueryApiMetrics` | 余额/用量时间序列，回 `ApiMetrics` |

`SwitchApi` 保留协议但 UI 不再暴露（persona 级选用取代全局激活）。

## Persona 级引用

**模型**：全局 `[agent].api_profiles` 是唯一供应商池；`[agent.teams.{id}].api_profile = "name"` 只做引用（None = 跟随全局默认配置）。不内嵌供应商值——增删改在设置页完成，所有 persona 共享最新池。

**入口**：聊天区入口行 →「配置」按钮 → Agent 配置弹层「API 供应商」下拉（首选「跟随全局默认配置」，其余为池中 profile，显示 `name（provider / model · key 状态）`）；保存 `SaveTeam{api_profile}`。

**运行期生效**：`SaveTeam` 成功后 target agent 立即重建自己的 provider（`apply_persona_api`：按引用从全局池解析 → `create_provider` 独立构建 → 更新 model）；启动期 `make_agent` 对配置了 `api_profile` 的 persona 直接构建独立 provider，不共享默认 provider。

**持久化**：写入 `[agent.teams.{id}].api_profile`（TOML）；`TeamInfo.api_profile` 回推 Panel；AgentSwitcher 卡片/列表显示当前 persona 生效模型（池解析，未引用 = 全局默认 model）。
