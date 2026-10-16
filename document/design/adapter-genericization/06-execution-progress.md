# 执行进度台账（父级维护）

> 最后更新：2026-10-16 13:28。

## 第一波（✅ 全部完成，且全部经父级独立复核）

| 任务 | 仓库 | 提交 | 父级复核结果 |
|---|---|---|---|
| **Plugin-1** UI 描述符 + get_state + 部分载荷 | echo-adapter-qq | `4fdc50c` | ✅ cargo test --workspace 全绿（146+2+3/63/24+5；基线 135+2+3，+11 用例） |
| **Core-1** AdapterStatus 三字段 + 访问器 + AdapterList + W2.4 | core | `be94746` | ✅ cargo test --workspace **918 passed / 0 failed**（EXIT 0） |
| **Panel-1** 02a S1-S6 | panel | `cd93c8c..8ced31c`（6 提交） | ✅ vitest **59 文件 / 443 用例**全绿（基线 56/418）；vue-tsc 恰 1 条已知错误（无新增）；17 条 EnvironmentTeardown 为存量噪声（溯源均在他人 WIP 的 ChatView 测试） |

细节备忘：
- Plugin-1 遗留（不阻塞）：`config_schema.bind_address.default` 值待统一（01a 草案 "127.0.0.1:0" vs 运行缺省 "0.0.0.0:3131"）
- Core-1 覆盖：01a S1-S3 + 01b S2/S3/S4 部分（id_compat.rs 双读；qq_tools arg_id 宽容解析）
- Panel-1 切片：S1 protocol / S2 pending / S3 connection / S4 adapterOps / S5 state+upstreams / S6 门禁
- 踩坑记录：`find -name A -o -name B -mmin` 有优先级坑，必须加 `\( \)`

## 第二波（🚀 已派发 13:28）

| 任务 | 仓库 | 模型 | 范围 | 依赖 |
|---|---|---|---|---|
| **Core-2** | core | deepseek | ① C1 失败回执（Error→AdapterOpResult{error}）② C2 get_state 推送 ③ S4 宿主推送接线（subscribe_ui_states→AdapterState）④ S5 epoch generation 失效 | Core-1 ✅ |
| **Panel-2** | panel | deepseek | 02b R1-R4：qqView.ts 读覆盖层 + QqPanel/QqLoginSection 数据面重接线 | Panel-1 ✅ |
| **Panel-3** | panel | **kimi**（UI 前端图形规范） | 03a 渲染器：adapter-ui/ 解析+bind+7 节点+useAdapterOp+测试（含 QQ 描述符快照 fixture） | Panel-1 ✅ |

（Panel-2 与 Panel-3 文件零交集可并行；均已叮嘱 git add 只用自定路径 + index.lock 重试。）

## 后续波次（计划）

- **Panel-4**：03b S1-S5（useAdapterStatusView + AdapterPanel + SessionSwitcher 通用化 + 挂载切换）——**等 panel 他人 WIP（ChatView.vue）落定**后派发
- **终波**：03b S6-S7 删除 + 01b S5 改名（adapter_ops）+ 文档收口 + 三仓库全量验收

## 环境基线（执行验收口径）

- core：`cargo test --workspace` 918/0（13:26 复核）；target 热
- panel：`npx vitest run` 59 文件/443；`npx vue-tsc -b` 恰 1 条 `ChatView.vue(24,10) TS6133`（他人 WIP，允许）
- plugin：146+2+3/63/24+5
- node：`export PATH="$HOME/.nvm/versions/node/v22.22.3/bin:$PATH"`
- 竞争约束：core 他会话（install.sh 线）；panel WIP（ChatView/WorkspacePanel/PanelPopover）

## ⚠️ 事件：kimi(Panel-3) 停滞与改派（14:27）

- kimi 任务（03a 渲染器）13:28 派发；13:46 提交解析层（78b4cc2）；13:47–13:51 写出渲染器 + 7 节点组件（未提交）；**13:51 起完全静默（文件零变化、无进程、无提交）**至 14:27 仍未回报。
- 父级诊断：WIP 状态有 6 条 vue-tsc 错误（renderer null 赋值 / 测试取属性未收窄 / ButtonNode 引用路径错误 '../../confirm' / ListEditorNode unknown→string）；无 vitest 进程=非测试卡死；判定为 API 侧挂起（与早前 uuapi 524 同类故障模式）。
- **改派决策**（避免无期限等待，注意并发保护）：新任务 f7031380（deepseek）接手收尾——修 6 错 + 补节点/渲染器/QQ 快照测试 + 提交；并叮嘱「若文件被外部修改即停止报告」（防 kimi 复活冲突）。
- 同时并行派发 **Panel-4a（9b05b67d，deepseek）**：03b 之 S1（useAdapterStatusView）+ S2（AdapterPanel 骨架，不挂载）+ S4（SessionSwitcher 通用化）——与 adapter-ui/** 及他人 WIP 零交集，可安全并行。
- 备注：Core-2（7c2517d）与 Panel-2（ea50f07 终）已完成且经父级独立复核；其正式回报事件亦未到达——疑似事件投递延迟，工作不阻塞。

## 第二波收尾（14:42 核实）

| 任务 | 提交 | 父级复核 |
|---|---|---|
| **Renderer**（f7031380 接手 kimi 收尾） | `78811dc`：7 节点 + 渲染器 + nodes.test(35)/renderer.test(15) | ✅ 全量 vitest **68 文件 / 561 用例**全绿；vue-tsc 恰 1 条已知错误 |
| **Panel-4a**（9b05b67d） | `2f34377`（S1 useAdapterStatusView + S2 AdapterPanel 骨架 + 10 测试）+ `b24a7c0`（S4 SessionSwitcher 通用化 + session-labels.ts + 12 测试） | ✅ 同上 |

（原 kimi 任务 78b4cc2 解析层 + deepseek 78811dc 组件层，接力完成；kimi 未再复活。）

## 第三波（🚀 已派发 14:42）

| 任务 | 仓库 | 范围 |
|---|---|---|
| **Core-3** | core | 01b S5+S6：① `adapter_qq/`→`adapter_ops/`（legacy.rs 隔离旧臂；`resolve_adapter` 回退改注册表全员计数）② 广播 a′（同 platform + gating，去 `"qq"` 字面量）③ `qq_tools.rs`→`adapter_tools.rs`（`register_adapter_tools_multi`/`InstanceSet`）④ `qq_instances.rs`→ 通用实例（compose 留驻=可选）⑤ main.rs persona 泛化 + `send_text` 能力过滤 |
| **Panel-4b** | panel | 03b S3：AdapterPanel 插槽接入 AdapterUiRenderer（descriptor=AdapterStatus.ui；state=adapterUiByKey[scopedKey]；meta 五字段；无 ui 占位）+ fixture 集成测试 |

## 剩余（后续）

- 03b S5（ChatView 挂载切换）——**阻塞**：他人 WIP（ChatView.vue 未提交，12:08 后无变化，已 stale 2.5h）；等落定或用户裁决
- 03b S6-S7（删除 QqPanel/QqLoginSection/qqByAdapter/protocol QQ 段 + 注释收尾）——依赖 S5
- 01b S7（旧变体 deprecated 标记）+ S8（N+2 删除）——发布计划项，非本轮
- 01a S6（可选：重启后贡献变化重建 RemoteAdapter）
- 文档收口：01b §C 升级指南写入 adapter-plugin.md；06 台账归档

## 环境基线（14:42）

- core：`cargo test --workspace` 923/0（7c2517d）；panel：vitest 68/561、vue-tsc 恰 1 错；plugin：146+2+3/63/24+5

## 第三波收尾（15:35 核实）

| 任务 | 提交 | 父级复核 |
|---|---|---|
| **Panel-4b（S3）** b0424fe3 | `f823972`：AdapterPanel 内嵌渲染器（descriptor=ui / state=adapterUiByKey / meta 五字段 / 无 ui 占位 / :key 重挂）+ QQ fixture 集成测试（select→gate.mode、change→InvokeAdapterOp 寻址、list_editor 载荷、多实例跟随） | ✅ vitest **68 文件 / 565 用例**全绿；vue-tsc 恰 1 已知错误（+4 用例） |
| **Core-3 S5** 63e9cfa3 | `cea95bd`：adapter_qq→adapter_ops（legacy.rs 隔离 10 旧臂 + test_support.rs）+ 回退泛化（注册表计数）+ 广播 a′（同 platform ∧ gating_capabilities 新 trait 默认方法） | ✅（承接方 S6 全量复跑 925/0） |
| **Core-3 S6（原任务超时中断→改派 e8136a0b 承接）** | `ac2c7b9`：qq_tools→adapter_tools + 实例/persona 泛化 + sends_text 能力过滤 + main.rs 中性化 | ✅ **cargo test --workspace 925/0**（父级独立复跑 EXIT:0）；D.5 grep 994→880 行，分类：豁免(配置键/插件标识/CLI/compose 留驻) + 测试域 + 少量历史命名遗留(③) |

- 事件：Core-3 原任务在 S5 提交后达 1200s 超时（S6 中途被切，工作树留下 staged rename）；父级核实后以「断点续作」任务书改派，15:28 收口。
- 第三波全部完成。**Panel S5/S6/S7 仍阻塞于他人 ChatView.vue WIP（12:08 后无变化）**——已向用户提出裁决请求。

## 剩余清单（截至 15:35）

1. 03b S5（ChatView 挂载切换）→ S6（删 QqPanel/QqLoginSection/qqByAdapter/lastQrcode/protocol QQ 段）→ S7（注释死引用）：**阻塞（他人 WIP）**
2. 01b S7（旧变体 #[deprecated] 标记 + 收到旧命令 warn）：发版项，随 N 发布做
3. 01b S8（N+2 物理删除）：发布计划
4. 01a S6（可选：重启后贡献变化重建 RemoteAdapter）
5. 文档收口：adapter-plugin.md（W2.4/W4.3 + 进度）与 decoupling-plan.md ④ 状态；泛化设计文档集（本目录）提交入库
6. 残留 ③（核心局部命名 qq_adapters/QQ 文案/session 示例）——低优先，随插件仓迁移切片

## S5/S6 预检（父级，15:36；供裁决后立即派发）

**S5 替换点（ChatView.vue，2 处）**：
1. `:40` `const QqPanel = defineAsyncComponent(() => import('./QqPanel.vue'));` → AdapterPanel
2. `:839` `<QqPanel :team-id="props.activeTeamId ?? null" />` → `<AdapterPanel :team-id="…" :core="state.activeRegion ?? null" />`
   （core 传法参照 TasksPanel `:842` 先例；AdapterPanel props = `{ teamId?, core? }`，已就绪）
   注：ChatView 当前为他人 WIP 版（已包 PanelPopover），行号按此版预检。

**S6 删除面（已核实）**：
- 组件：`QqPanel.vue`(613) / `QqLoginSection.vue`(148) / `qq/qqView.ts`(239) / `qq/__tests__/qqView.test.ts`
- 测试：`QqPanel.test.ts`（5 条已迁 AdapterPanel.test.ts）/ `QqLoginSection.test.ts`
- state.ts：`qqByAdapter` / `lastQrcode` / `QqFilterState` / `defaultQqInstanceState` + 7 个处理臂（:1373-1441 一带）
- pending.ts：`REQUEST_RESPONSES` 6 条 QQ 映射
- protocol.ts：QQ 命令/事件段
- S7 注释：ApiSettings.vue:465、connection.ts:93、connection-bootstrap.test.ts:7、pending.test.ts:117 等

**S5/S6/S7 完成后终验 grep**：`grep -rn "qqByAdapter\|lastQrcode\|QqPanel\|QqLoginSection\|RequestQq\|SetQq\|UpdateQq" web/src` 应为 0（测试注释豁免）。

## 15:42–15:47 观察：Panel 仓库他会话仍活跃，ChatView WIP 持续停滞

- 15:42:18 他会话提交 `774b60a`（ApiSettings.vue 费用文案，与泛化无关）；**ChatView.vue WIP 仍停留在 12:08（已 3.5h+）**。
- 判读：ChatView/PanelPopover/WorkspacePanel 三件 WIP 疑似**被中断/搁置的弹层重构**（另有用例证明面板工作链正常：新提交均不含这三文件）。
- 处置：维持等待用户裁决（A 整理提交 / B 原会话处理 / C 跳过）；无论哪种，**S5/S6/S7 的执行素材已备好**（见上节预检），且 S6 不可逆点在 Core 侧 N+2 发版（01b S8），当前不构成风险。

## 全部迟到回报对账（15:42 前收齐）

本轮收到 8 条迟到完成通知（Plugin-1 / Core-1 / Core-2 / Panel-1 / Panel-2 / 02a 续写 / 03a / 03b），**全部已在先前轮次核实并消费**（工作树 + 测试数字独立复核），无需任何补做。

## 父级抽查发现 + 修复（15:48）

**缺陷**（我们本轮代码引入）：`AdapterPanel.vue` 启停/重启用 `sendPending` 登记 `adapter:<op>:<core>:<name>`，但全仓无结算路径（start/stop/restart 不在 REQUEST_RESPONSES；settlePending 仅 adapterOps 对 `adapter-op:` 前缀调用）→ 每次点击条目永久滞留（20s 后仅置 timedOut）。
**修复依据**（已核实）：Core Start/Stop/Restart 完成后必重发 `AdapterList`（agent/commands.rs:1386/1408/1427 三臂）→ 收到 AdapterList = 该 core 启停完成。
**修复任务** a57b9c32（deepseek）：pending.ts 增 `settleAdapterOpsOfCore(core)`（匹配 `adapter:` 前缀 + core 段，不误伤 `adapter-op:`）；state.ts AdapterList case 调用；pending 单测 + 集成断言。不碰他人 WIP。

## 终验全量测试（15:57，父级独立执行）

| 仓库 | HEAD | 结果 |
|---|---|---|
| core | 770c954 | **927 passed / 0 failed**（EXIT:0；较 ac2c7b9 的 925 +2，为他会话 metrics 提交的测试） |
| panel | 3d34ff9 | **68 文件 / 568 用例全绿**；vue-tsc 恰 1 条已知错误（他人 WIP ChatView TS6133） |
| plugin | 4fdc50c | **243 passed / 0 failed** |

**17 条迟到通知全部对账完毕**：均对应已核实消费（或已现场处置）的工作；无补做项。
**剩余唯一事项**：Panel S5/S6/S7（等用户就他人 ChatView.vue WIP 裁决 A/B/C）。

## Panel S5/S6/S7 收尾完成（2026-10-16 16:0x，用户裁决 A：整理 WIP 后继续）

按用户指示，先整理搁置 WIP（弹层重构）为独立提交，后执行 S5→S6→S7：

| 片 | 提交 | 内容 | 验收 |
|---|---|---|---|
| WIP 整理 | `67136a3` | PanelPopover 统一弹层（5 个入口弹层迁移，-202 行几何逻辑）；**恢复被 WIP 漏掉的 3 个安全关闭 watcher**（checklist/workspace 不可用 + 切 Agent）；修 checklistKey TS6133 | vitest 68/568 全绿；vue-tsc **零错误**（首次消除） |
| S5 | `8a46878` | ChatView 适配器入口 QqPanel → AdapterPanel（含 `:core` 显式路由） | 68/568 全绿 |
| S6 | `2fcfa10` | 删除 QqPanel/QqLoginSection/qqView（含 3 个测试文件 24 用例）+ state 7 臂/qqByAdapter/lastQrcode/QqFilterState + pending 6 映射 + protocol QQ 命令/事件段/GateMode/GroupInfo/FriendInfo/3 deprecated 字段；测试改写（phase1→adapterUiByKey、bootstrap→InvokeAdapterOp、protocol-sync 58→51） | **65 文件/541 用例全绿**；vue-tsc 零错误 |
| S7 | `d8a7779` | 注释死引用清理（9 文件）+ pending.test 示例键更新 | 65/541 全绿；零错误 |

**父级终验**：
- 生产构建 `npm run build` ✓（产出 AdapterPanel chunk，无 QqPanel chunk）
- 死引用 grep（V16 判据）：仅剩 protocol-sync.test.ts:74 的有意变更说明注释
- panel 工作树全干净（含他人 WIP 已并入 `67136a3`）

**偏离记录**：03b 决策建议 D4「S5/S6 间留发布周期」未执行（本地连做）——理由：全部提交未 push 未部署，回滚 = 逐片 revert（d8a7779→2fcfa10→8a46878），无生产风险；发布时如需灰度可在 push 前拆分。parity「挂载即拉二维码」按 03a §2.6 手动化口径（V11 据此修订）。

**至此本工程全部切片完成**：01a P1+S1-S5、01b S1-S6、02a S1-S6、02b R1-R4、03a 全套、03b S1-S7。剩余仅发版流程项（01b S7/S8：deprecated 标记与 N+2 删除）。

## 旧协议变体直接删除（2026-10-16 16:25，用户指示：不等 N+2）

**提交 `c5301f5`**（核心仓库，14 文件，+39/−1255）：
- echo-protocol：10 命令变体 + 7 事件变体 + `AdapterStatus` 三字段（container/webui_url/onebot_url）删除；`id_compat.rs` 双读层移除；相关测试精简
- echo-agent：`legacy.rs`（739 行）删除；命令域仅剩 `InvokeAdapterOp`；`emit_adapter_state_events` 仅发 `AdapterState`（单源）；`agent/mod.rs` 弃字段填充删除（metadata 派生保留）
- core：`main.rs` 旧事件派生循环删除；`management.rs` 测试改写为 `InvokeAdapterOp` 往返
- 测试：927 → **918 passed / 0 failed**（净减 9 个旧测试；父级独立复跑确认）
- 四条验收 grep 全空（旧命令 / 旧事件 / GroupInfo+FriendInfo / legacy+id_compat）

**文档层同步**（同批，另有提交）：
- `adapter-plugin.md`：§变体处置与升级指南改写为「直接删除已生效」；W4.1-W4.4 勾选注释更新；实施进度「无剩余发版项」
- `01b-core-protocol-lifecycle.md`：B 节加后注（S7/S8 合并直接执行；设计历史保留）
- `protocol.md`：命令/事件段改写为统一通道；旧变体注明已删除
- `adapter-qq-gating.md` / `panel-qq-tasks.md` / `panel-system.md`：旧命令引用更新为新通道
- Panel 仓库：`protocol-sync.test.ts` 注释更新（「Core 侧保留至 N+2」→「同批直接删除」）
