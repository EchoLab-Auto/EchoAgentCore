# 子代理委派 & 长程任务审计报告（2026-10）
> 审计对象：echo-agent（EchoCode persona）的 `spawn_subagent` 委派全流程与长程任务效率。
> 目标：以真实数据定位瓶颈，产出 `subagent-delegation` 技能优化稿 + 新技能 `long-task-efficiency` 草稿。
> 分析脚本与中间产物：`/tmp/aud/`（`extract.py`/`seg2.py`/`pat2.py`/`pat3.py`/`final.py`，本次会话临时目录，非仓库文件）。

## 0. 口径与方法（可复现）

| 项 | 说明 |
| --- | --- |
| 数据源 | 主档 `~/.config/echo-agent-core/echo-sessions-EchoCode.json` + `archives/echo-sessions-EchoCode-*.json`（20 个快照，2026-10-13 ~ 10-16）+ 补充档 `echo-sessions-alix.json`、`echo-sessions-self-coding.json`，共 23 个文件 |
| 去重 | **档案间事件大量重叠**：`user_message` 以 `(session, message_sequence, timestamp, content-md5)` 去重，`tool_call` 以 `id`、`tool_result` 以 `tool_call_id` 去重。跨档聚合前先入库，绝不相加 |
| 去重后规模 | tool_call **8262**（EchoCode 7620 / alix 551 / self-coding 91）、tool_result 8252、user_message **360**（EchoCode 289 / alix 68 / self-coding 3） |
| 会话范围 | 仅工作类：`local:workspace:*`（11 个）+ `local:tui:*`（alix/self-coding）。**qq:* 会话 0 条**，无需排除 |
| 任务切分 | "单次用户请求片段" = 同一 session 内一条 `<backend_message_hook>` user_message → 下一条之间的全部工具调用（按 `started_at_ms` 归属）。`<subagent_event>` 注入不新开片段，计入 `nsub` |
| 委派样本 | 跨档提取 `<subagent_event>` 原始记录 107 条 → 按 `subagent_id` 去重得 **58** 次委派（grep 计数会因源码被 grep 到、开闭标签重复而虚高，勿用 `grep -c`） |
| 已知缺口 | `spawn_subagent` 调用本身**未持久化**（7620 条 tool_call 中 0 条名为 `spawn_subagent`）、hook payload `task` 截断 500 字符、`assistant_message.tool_calls` 恒为空数组（见 §6） |

时间范围说明：仓库档案最早为 2026-10-13，因此本报告**不能**覆盖 2026-09 基线的原始 turn；基线数字（17 turn / 634 次调用 / 0 次委派）仅作为参照引用。

---

## 1. 数据表

### 1.1 委派统计（n=58，按 `subagent_id` 去重）

| 指标 | 数值 |
| --- | --- |
| 总委派 | **58**（10-13: 4 / 10-14: 7 / 10-15: 12 / **10-16: 35**） |
| 成功 / 失败 | **45 (77.6%) / 13 (22.4%)** |
| 按会话 | `echo-agent/conv` 27、`echo-agent` 15、`echo-agent/conv-2` 7、`systemx/vr` 4、`systemx` 3、`systemx/conv` 2 |
| 父分支数 | 39（1 子任务 25 个，2 子任务 11 个，3 子任务 3 个 ⇒ **14/39 为多子任务**） |
| task 字段长度 | **53/58 恰为 501 字符**（= 框架按 500 字符截断）→ task 质量只能部分审计 |
| result 长度 | min 11 / 中位 1641 / p90 3429 / max 6730；**0 条触及 8K 截断**（2 条文本含"截断"字样） |
| 失败原因 | HTTP 524 **5**、`uuapi.io` 网络层失败 **4**、子任务超时 900s **2**、超时 600s **2** ⇒ **13/13 全为基础设施故障，0 例"任务描述不合格"型失败** |
| profile 使用 | payload 无 profile 字段（缺口）；用 `echo-usage.jsonl` 的 profile 名反推（见 §1.4） |
| node 使用 | 至少 2 次：10-16 12:55「远程节点 a100-40g（你就在该节点的沙箱里执行命令）」→ 成功完成 isaaclab 环境安装；10-16 00:30 经 `node://A100-40g` 下发命令 |
| 并行性 | 14 个多子任务父分支中 **13 个**在"首个子任务完成 → 末个子任务完成"窗口内父 agent 仍在发工具调用（未空转），1 个空转；超时锚点可反推 spawn 时刻（如 `c4fb3bed` 900s 超时于 17:21 → spawn ≈ 17:06，同组另 2 个子任务 17:08/17:09 完成 ⇒ **确为并行**） |
| 回报后响应 | 子任务回报到达 → 父 agent 下一个工具调用的间隔：中位 **10s**、p90 110s；5/58 > 120s，最大 6383s（1.8h，属会话已结束/忽略）⇒ "等回报期间不空转"基本达标 |

### 1.2 长任务画像（EchoCode 工作会话，231 个请求片段 / 7522 次工具调用）

| 指标 | 数值 |
| --- | --- |
| 每片段工具数 | 中位 **14**、p90 **90**、max **230**；零工具片段 52/231（纯对话轮） |
| 调用集中度 | **Top 10% 片段（23 个）占 43.1% 调用**；≥50 调用片段 59 个占 **74.7%** 调用 |
| 工具构成 | `bash` **6568 (86.2%)**、`edit_file` 469、`checklist` 274、`read_file` 89、`write_file` 87、web_search 24、search_code 24、其余零星 |
| 时间跨度 | ≥50 调用片段合计 span **25.3h**，其中工具实际执行 **10.7h（42%）**，其余为模型往返与委派等待 |
| 工具耗时 | 中位 9ms、p90 4686ms；>30s **352** 次、>120s **139** 次、>300s 8 次；累计执行 13.4h |
| 响应轮次（估算） | 相邻调用间隙 <1s 的仅 **5.8%**（同轮串行特征），其余为跨轮 ⇒ 7522 次调用 ≈ **6100 轮响应，平均 1.24 次调用/轮**；轮间间隙 ≥2s 共 6066 次，中位 **7.1s**，累计 **21.3h** 纯模型等待 |
| 委派覆盖 | 有子代理回报的片段 **31/231 = 13.4%**；**≥90 调用的 25 个长片段中 19 个（76%）零委派**，≥50 调用的 59 个中 46 个（78%）零委派 |
| 错误信号 | `result` 以 `error:` 开头 **95/7620 = 1.2%**；类别：远程节点超时 27、`timeout after Ns` 21、**误报阻断 16**、文件不存在 9、`edit_file` 参数缺失 7、清单索引错 4 |

### 1.3 低效模式频次（EchoCode 口径）

| # | 模式 | 频次（实测） | 说明 |
| --- | --- | --- | --- |
| a | **每次响应只发 1 个工具调用**（独立探测没有同轮打包） | 同轮串行特征仅 **443/7343 对（6.0%）**；平均 1.24 调用/轮 | 每多一轮 = 多一次 7.1s（中位）模型等待 + 再发一遍上下文 |
| b | **同一文件/同一区域反复读取** | 读取型访问 4144 次；**111/231 片段**存在同段内同一文件被读 ≥3 次；超出 3 次的重复读取 **2096 次** | 最重案例：163 调用片段中 `mod.rs` 42 次、`session.rs` 23 次；221 调用片段 `BodyAxesDemo.cpp` 55 次 |
| c | **轮询等待（sleep 类）** | **380 次**调用落在 68 个片段（占全部调用 5.0%） | 含对后台安装/服务的反复 `sleep + 检查` |
| d | **该委派却没委派** | ≥90 调用长片段 **19/25（76%）零委派**；≥50 调用 **46/59（78%）零委派** | 对照现技能"预计 >5 条探查命令即委派"的口径，明显漏派 |
| e | **委派失败后原样/近似重发** | 13 次失败中 **9 次为基础设施**，其中 **8 次是同一任务的重发**（同一 task 前缀 3 连、2 连；另 2 次 524 亦属同题） | 见 §2 案例 2、3 |
| f | **命令失败后盲重试** | 错误后原样/近原样重发 **18 次**（14 个片段），其中 **16 次是 `rm -rf /...` 误报阻断后的重写重试** | 见 §2 案例 4 |
| g | 超时/长命令串行 | >30s 调用 352 次、>120s 139 次，均串行等待 | 属"应后台化或委派"的候选 |

### 1.4 模型 profile 实际使用（`echo-usage.jsonl` 24736 条记录反推）

| 日期 | 总 prompt tokens | non-`deepseek` 占比 | 明细（M tokens） |
| --- | --- | --- | --- |
| 10-13（首次委派） | 22.4M | 27.6% | kimi 6.19 |
| 10-14 | 10.8M | 100% | kimi 9.24、deepseek-self-1 1.6 |
| 10-15 | 35.8M | 97.6% | uu-api 23.7、kimi 5.66、deepseek-self-1 4.49、uuapi-gemini 1.12 |
| 10-16 | **69.6M** | 90.5% | **spacetime 34.79、uu-api 27.03**、kimi 0.66、uuapi 0.22 |

- 单次调用平均输入：`uu-api`（claude-opus-5-5）58.6K tokens/次、`spacetime`（gpt-6-sol）114K tokens/次 —— 委派出去的多是"读全仓"型审计，上下文极重。
- ⚠️ 归因警告：persona `Alice` 的**主模型**就是 `kimi`，故 kimi 用量不能全部算作子代理；`uu-api`/`spacetime`/`uuapi*` 无 persona 使用，可视为子代理专用。
- 失败窗口代价（`10-16 00:15–01:00`，conv-2 连续 4 次失败委派）：45 分钟内 `uu-api` 烧掉 **6.7M prompt tokens**，`uuapi` 0.19M；`10-16 12:00–12:30`（4 次 524）：`uu-api` **7.9M** prompt tokens。

---

## 2. 案例摘录

**案例 1（优秀 · worktree 三连并行）**：10-15 17:42/17:43/17:45，同一父分支下发 3 个子任务，各给独立 git worktree 与**只改文件白名单**（`s3-locks` 锁中毒、`s1-adapter` 依赖倒置、`s2-errors` 错误类型），3/3 成功，回报 2170–2269 字符。
> task 片段：「你的工作目录是一个 git worktree：/home/evence/.cache/echo-wt/s3-locks……其它人同时在别的 worktree 修改别的文件，所以你只能改下面列出的文件」

**案例 2（糟糕 · 死供应商上同一任务重发 4 次）**：10-16 00:22 / 00:26 / 00:34 / 00:44 / 00:49，conv-2 对"API profile 改名能力设计"同一任务连续委派，5 次全部失败（`HTTP 请求失败: error sending request for url (https://uuapi.io/v1/messages)`、`API 返回错误: HTTP 524`），task 文本仅在 HEAD 号等处微调。期间 `uu-api` 单窗口烧 6.7M prompt tokens。

**案例 3（糟糕→优秀 · 失败后重新切分才成功）**：10-16 12:10–12:20 同一"只读分析 + 设计文档"任务 4 次 524 失败；12:22 起父 agent 改为**按产出物切分**（`03a-panel-renderer-spec` / `03b-panel-migration` / `01a-core-uidescriptor` / `01b-core-protocol-lifecycle`），**4/4 成功**，产出 148–367 行文档，互不重叠。

**案例 4（框架误报 · 16 次无谓阻断）**：`bash` 危险模式是**子串匹配** `"rm -rf /"`（`source/backend/echo-agent/src/packages/tools_builtin/coding.rs:692`），于是 `rm -rf /tmp/fbx_test_out`、`rm -rf /home/evence/.cache/echo-wt` 这类正常清理**全部被拦**并回报 "blocked dangerous command pattern: rm -rf /"。实测 16 次全为误报，模型随后改写命令重试。

**案例 5（糟糕 · 超时型委派）**：10-15 17:21 `c4fb3bed`「评审 UI 组件库 ui-frame 架构」900s 超时；10-16 15:09 `d670ff59`/`ae5bd805` systemx/vr 两个子任务 600s 超时。同批次的 17:08/17:09 两个"单仓库架构评审"子任务成功，说明**问题在切分粒度（单任务覆盖整个仓库全量审查）**。

**案例 6（优秀 · 审计三连只要结论）**：10-13 16:13 三个子代理分别审"时间线/事件日志回放""压缩链""归档清理"，每份结论 1420/1441/1535 字符，主线程据此直接写汇总——委派产出**短、结论化、带文件:行号**。

---

## 3. 建议清单（按预期收益排序）

| # | 建议 | 数据依据 | 预期收益 |
| --- | --- | --- | --- |
| 1 | **同轮打包独立探测**：每次响应尽量发 2–4 个独立工具调用（或一条 bash 内 `&`+`wait`），把"读—再读—再 grep"合并成一轮 | 同轮串行特征仅 6.0%，平均 1.24 调用/轮；轮间间隙中位 7.1s、累计 21.3h；工具执行仅 13.4h | 若平均提到 2.5 调用/轮，响应轮次 6067→约 3010，省约 3000 轮 × 7.1s ≈ **6h 模型等待**（占 21.3h 的 28%） |
| 2 | **委派硬阈值**：预计 >40 步 / >15 分钟的探查型任务，至少切 1–3 个子代理；≥90 调用片段必须有委派理由 | 19/25 个长片段零委派；委派样本 45/58 成功、0 例"描述不合格"失败 | 长尾片段上下文不再污染主线程；按现技能估计省 40–50% 墙钟 |
| 3 | **委派失败停止条件**：同一 task 在同一 profile 上连续 2 次基础设施失败 ⇒ 立刻换 profile（`list_api_profiles` 取可用项）或缩小范围，**禁止第 3 次原样重发** | 8/13 次失败是同一任务重发；两个失败簇烧掉 6.7M + 7.9M prompt tokens、约 50 分钟 | 直接省掉约 14.6M prompt tokens 与 ~50min |
| 4 | **按产出物切分，而非按主题** | 12:22 起按 `03a/03b/01a/01b` 切分 → 4/4 成功 | 把 0/4 的失败簇变成 4/4 成功 |
| 5 | **超时预算显式化**：全仓级审查拆成"单仓库/单模块"；预计 >8 分钟必须传 `timeout_secs` | 4 次超时（900s×2、600s×2）均为跨仓库/全仓审查；成功案例均为单仓库 | 消除 4 次（约 40 分钟）无产出等待 |
| 6 | **读取去重**：同一片段内同一文件 ≤2 次；需要多次看就一次 `sed -n 'A,Bp'` 放大范围或一次 `grep -n` 全文 | 111/231 片段重复读；单片段 `mod.rs` 42 次、`BodyAxesDemo.cpp` 55 次 | 减少调用与上下文重复投喂 |
| 7 | **长命令后台化**：>30s 的命令改用 `shell_start`/后台 + 完成后一次检查，不用 `sleep` 轮询 | >30s 调用 352 次；sleep 类轮询 380 次 / 68 片段 | 减少 380 次无效往返 |
| 8 | **失败后不原样重发**（含误报阻断的绕行写法：`cd <dir> && rm -rf <name>` 规避子串匹配） | 18 次错误后近原样重发；16 次 `rm -rf /...` 误报 | 省 16+ 次往返；同时建议修复匹配为词边界/路径前缀 |
| 9 | **新技能落位**：`long-task-efficiency` 加入 `[agent.teams.EchoCode].enabled_skills`（当前白名单仅 6 项，新技能不加则不生效；`gating.rs:388`） | 技能启用依赖 persona 白名单 | 让 1/6/7/8 四条变成常驻约束 |

---

## 4. 技能 diff 摘要（`subagent-delegation`）

| 改动 | 理由（数据） |
| --- | --- |
| 保留：开工前扫"可委派块"、三种并行手段、task 模板、接口边界拆分、不空转、汇报口径 | 这些是 45/58 成功委派的实际写法基础（案例 1/6 直接对应） |
| **改**：把"3 条各 10s → 并发约 12s"的 bash 批处理例子升级为"同轮多 tool_call 也合法、且是省往返的主手段" | 框架逐个 `await` 串行执行属实，但同轮发出**只花一次 LLM 往返**；实测同轮占比仅 6.0%、平均 1.24 调用/轮、轮间中位 7.1s ⇒ 旧表述让人误以为"多发无用" |
| **新增 §「委派失败与重试」**：2 次基础设施失败即换 profile / 缩范围；timeout ⇒ 先缩范围或显式 `timeout_secs`；同 task 不得 3 连发 | 8/13 失败是同任务重发；两个失败簇 6.7M+7.9M prompt tokens；4 次超时 |
| **新增 §「task 尺寸与超时预算」**：单子任务 ≤1 个仓库/模块；产出物数量对齐子任务数；预计 >8min 传 `timeout_secs` | 12:22 的按产出物切分 4/4 成功；900s 全仓审查超时 |
| **新增 §「自检清单」**：发车前 5 问（自含？产出物唯一？profile 可用？timeout 够？结果只回结论？） | payload 显示 task 被截断 500 字符、result 中位 1641 字符，短结论型回报是当前主流且有效 |
| **改**：并行完成回报延迟数据写入技能（回报后中位 10s 有下一步动作） | 避免模型"等回报时反复查询"；实测 13/14 多子任务父分支未空转 |
| **新增**：profile/node 选择与失败映射（重活/装环境 → `node:` 远程；长上下文审计 → 明确 profile 并核对 `list_api_profiles`） | 10-16 12:55 远程 a100-40g 委派成功完成 isaaclab 安装；失败簇集中在 uuapi/uu-api |

## 5. 新技能草案（`long-task-efficiency`）

承载 §3 中与"委派"无关的执行效率规则（打包/去重读/后台化/失败处理/迭代预算），`metadata.always: true`，`package: echo-agent.tools.builtin`，需加入 EchoCode 的 `enabled_skills` 才生效。全文见 `drafts/long-task-efficiency/SKILL.md`。

---

## 6. 框架观测缺口清单

| # | 缺口 | 证据 | 影响 |
| --- | --- | --- | --- |
| 1 | **`spawn_subagent` 调用未持久化为工具事件** | 7620 条 EchoCode tool_call 中 0 条 `spawn_subagent`；只能从回灌的 `<subagent_event>`（含 `parent_branch_id`/`completed_at_ms`）倒推 | 无法直接算 spawn→完成的时延、无法审计 `profile`/`node`/`timeout_secs` 实参、无法做成本归因 |
| 2 | **hook payload `task` 截断 500 字符** | 53/58 条 task 恰 501 字符 | 委派"任务描述质量"无法完整审计（本次只能靠前 500 字符 + 结果反推） |
| 3 | **`assistant_message.tool_calls` 恒为空数组** | 抽样 331 条 assistant_message 全为 `[]` | 无法直接统计"每轮几个调用/多少轮"；本报告用 `started_at_ms` 差值 + `elapsed_ms` 间接推断（6% 同轮特征） |
| 4 | 子代理内部轨迹不入档 | 子会话无 tool_call 事件落库 | 无法评估子代理自身效率（是否也在一轮一发、是否重复读） |
| 5 | 委派无 spawn 时间戳 | 仅 `completed_at_ms` | "并行 vs 串行"只能借超时锚点（`completed - timeout`）反推 |
| 6 | token/成本无会话/任务维度归属 | `echo-usage.jsonl` 只有 `name`(profile)/`model`/tokens，无 session/turn/parent_branch_id | 只能按 profile+时间窗近似归因（§1.4 的告警即源于此） |

**Top 3 优先修复**：① `spawn_subagent` 落 tool_call（含 profile/node/timeout 实参）；② payload 不再截断 task（或落盘全文并回路径）；③ assistant_message 持久化 `tool_calls` 摘要（个数+名字即可）。

## 7. 待确认/假设

- 假设 1：`local:tui::local_user` 同时被 alix 与 self-coding 使用，故按"文件组"而非 session 字符串区分（alix 68 段 / self-coding 3 段几乎全是纯对话，未纳入长任务结论）。
- 假设 2：轮次推断阈值取"相邻调用间隙 <1s 视为同轮"；1–2s 的 834 对未计数（偏保守，真实同轮比例可能略高）。
- 假设 3：`kimi` 与 `uu-api/spacetime` 的 token 归因按"persona 主模型表"区分；kimi 含 Alice 主模型用量，已在 §1.4 标注。
- 数据限制：档案最早 10-13，2026-09 基线无法复核；`research/` 下仅本目录为新增。
