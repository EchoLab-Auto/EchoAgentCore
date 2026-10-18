---
id: gen-03b
title: "03b · Panel UI 迁移与删除"
group: 规划
x: 1920
y: 3480
---

# 03b · Panel 阶段 2：适配器 UI 迁移与删除方案

> 状态：已实施（旧 `QqPanel` / `QqLoginSection` 已删除）；现状见 [Panel 设置视图](../../panel/panel-settings.md) 与 [QQ 适配器门控](../../plugins/qq-gating.md)

> 基线：`evidence/00-frozen-interfaces.md`（下称「00」）、`evidence/03-evidence-panel-ui.md`（下称「03-ev」）。
> 仓库基线：panel @ 4b7d99b。行号均引用该基线；落地前需按当时 HEAD 复核。
> 依赖文档：**01**（插件描述符 / `ui` 载体 / W2.4 id 字符串化）、**02**（阶段 1 通道切换 + `AdapterOpResult` 配对）、**03a**（schema 渲染器规范）。
> 本文只定义「从哪删、搬到哪、按什么顺序、怎么验收、怎么回滚」，渲染器节点语义一律引用 03a，不重设计。

术语：
- **通用面板** = Panel 内置 `AdapterPanel`（暂定名，替代 QqPanel 外壳），与插件无关。
- **渲染器** = 03a 定义的 `web/src/components/adapter-ui/` schema 渲染器，消费 `AdapterContribution.ui`（00 §二）。
- **状态文档** = `adapterUiByKey[scopedKey(core, adapter)]`，形状以 00 §三为准。

---

## 1. 逐 UI 元素迁移映射

归属取值：**内置**（通用面板）/ **schema**（插件描述符 + 渲染器）/ **删除** / **保留**（已通用，不动）。

### 1.1 QqPanel.vue（613 行）

| # | 现状元素（file:line） | 归属 | 目标形态 | 备注 |
|---|---|---|---|---|
| P1 | 头部标题「适配器管理」+ 刷新按钮（QqPanel.vue:88-101） | 内置 | 通用面板头部；刷新 = 重新拉 `AdapterList` + 对当前实例触发状态刷新 | 语义变化见 §2.4 |
| P2 | 多实例选择器（:205-217，`multiInstance` :60）+ persona 过滤 | 内置 | 通用面板实例切换器，数据源 `state.adapters`，不按 platform 过滤，persona 过滤保留 | 语义变化见 §2.3-1 |
| P3 | 无实例空态（:218） | 内置 | 通用空态「暂无已注册适配器」 | 文案泛化 |
| P4 | 适配器状态行（:224-257：连接点、self_id、运行时长等） | 内置 | 读 `AdapterStatus`（00 §一）：`connected/running/self_id/bind_address/started_at/persona` + `metadata[]` KV 通用渲染 | 不读状态文档，保证插件无 ui 时仍可用 |
| P5 | Start/Stop 按钮（:240） | 内置 | 直接复用 `StartAdapter/StopAdapter`（已中立） | 逻辑原样上移 |
| P6 | Restart 按钮（:248） | 内置 | 复用 `RestartAdapter` | 原样上移 |
| P7 | 门控模式选择（:258-270；`GATE_OPTIONS` :72-76，`GATE_HINTS` :78-82） | schema | `select` 节点，`bind: gate.mode`，`action:{method:set_gate_mode, arg:{mode:"$value"}}` | `GATE_HINTS` 提示文案：v1 无 hint 字段 → 见 §1.6 决策 D2 |
| P8 | 白名单用户卡（:271-416 段内） | schema | `list_editor`，`bind: gate.allowlist_users`，`action: update_allowlist` | 载荷形状（全量替换 vs 增删）由 03a 定义 |
| P9 | 黑名单用户卡 | schema | `list_editor`，`bind: gate.denylist_users`，`action: update_denylist` | 同上 |
| P10 | 白名单群卡 | schema | `list_editor`，`bind: gate.allowlist_groups`，`action: update_allowlist` | 群/好友候选来源：`fetch:{method:get_groups}` 见 D3 |
| P11 | 黑名单群卡 | schema | `list_editor`，`bind: gate.denylist_groups`，`action: update_denylist` | 同上 |
| P12 | toggle 函数（:137-183，含 number 比较） | 删除 | 由渲染器 `list_editor` 内部实现 | number 依赖随 W2.4 消失（00 §一） |
| P13 | pending 键 `qq:filter|groups|friends|owner:<inst>`（:106-110）+ `qqListsPhase`（:110+） | 删除 | 由 02 定义的 `InvokeAdapterOp` 配对键替代（如 `adapter-op:<core>:<adapter>:<method>`，以 02 为准） | 加载态由渲染器节点级展示（03a） |
| P14 | `instanceState`（:53）/ `activeAdapter`（:46） | 内置（拆分） | `activeAdapter` → 通用面板选中实例；`instanceState` → 渲染器入参 `state = adapterUiByKey[key]` | |
| P15 | owner 读写（pending `owner` 键；现 UI 入口以 HEAD 为准） | schema | `text` 展示 `bind: owner` + 可编辑输入 + `action: set_owner{owner_qq:"$value"}` | v1 无 input 节点 → 决策 D1 |

### 1.2 QqLoginSection.vue（148 行）

| # | 现状元素 | 归属 | 目标形态 | 备注 |
|---|---|---|---|---|
| L1 | 登录态展示 + `requestStatus()`（:38） | schema | `status` 节点 `bind: online`；状态推送走 `AdapterState`，不再主动 RequestQqLoginStatus | 若需「手动刷新登录态」：`button` `action: login_status`，结果回写由插件 `report_ui_state` 完成 |
| L2 | 二维码 PNG `qrDataUrl`（:24）+ `loadQr()`（:44，RequestQqQrcode） | schema | `image` 节点 `fetch:{method:login_qrcode, result_key:png_base64}` | 结果仅入节点本地态，不入全局 state（见 L4） |
| L3 | WebUI 链接 `webuiUrl`（:108） | schema | `link` 节点 `bind: instance.webui_url` | ⚠️ 03-ev §A 示例写 `bind: webui_url`，与 00 §三不符；**以 00 §三 `instance.webui_url` 为准**，需 01 描述符照此书写 |
| L4 | `instanceState` 从 `qqByAdapter` 读 | 删除 | 渲染器统一读 `adapterUiByKey` | |

### 1.3 SessionSwitcher.vue

| # | 现状元素 | 归属 | 目标形态 |
|---|---|---|---|
| S1 | qq 分支（SessionSwitcher.vue:55-57，GROUP_LABEL :64-65） | 内置（通用化） | 按会话 `platform` 字符串分组，组键 `<platform>-dm / <platform>-group` |
| S2 | `GROUP_LABEL` 中 `qq-dm / qq-group` 硬编码文案 | 内置 + 插件数据 | 文案取 `session_labels.{dm,group}`（00 §二）；缺省回退 `<display_name> 私聊 / 群聊` |
| S3 | 组排序 order（SessionSwitcher.vue:73）+ 标签色 `.tag-qq-dm/.tag-qq-group`（:268-269） | 内置 | 固定内置组（本地/系统）在前，平台组按 platform 字典序，dm 先于 group |

`session_labels` 到达 Panel 的载体由 01 决定（00 §一缺口）；SessionSwitcher 只依赖一个查询函数（建议 `sessionLabelsFor(core, platform)`），屏蔽载体差异。

### 1.4 state.qqByAdapter 各字段（state.ts:443-453，实测）

| 旧字段（写入臂 state.ts） | 新位置（00 §三） | 归属 |
|---|---|---|
| `filter: QqFilterState`（QqFilterConfig 臂 :1373） | `gate.{allowlist,denylist}_{users,groups}`（**string[]**） | schema 消费 |
| `gateMode: GateMode`（QqGateMode 臂 :1386） | `gate.mode` | schema 消费 |
| `owner: number`（QqOwner 臂 :1395） | `owner` | schema 消费；类型随 W2.4 |
| `online / selfId`（QqLoginStatus 臂 :1411） | `online / self_id` | schema 消费 |
| `groups / friends: [number,string][]`（GroupList :1423 / FriendList :1438） | 不入状态文档；渲染器 `fetch:get_groups/get_friends` 节点本地缓存 | schema（D3） |
| 默认值工厂 `defaultQqInstanceState()`（:264）、`QqFilterState`（:256） | 删除 | — |
| 归并键 `scopedKey(core, adapter)` | **保留**为 `adapterUiByKey` 键（00 §五 目标 1） | 内置 |

### 1.5 state.lastQrcode

| 现状 | 归属 | 说明 |
|---|---|---|
| 全局单槽 `state.lastQrcode`（03-ev §B） | **删除** | 多实例下单槽会串号；改为渲染器 `image` 节点按 `(core, adapter, nodeId)` 本地持有 fetch 结果。若阶段 1（02）已先行改为按键存储，本阶段直接删除该过渡结构 |

### 1.6 需决策点

- **D1 owner 编辑**：v1 冻结节点无 input。决策建议：**不修订冻结项**，owner 用 `list_editor`（单元素上限）或 `text` + `button`（弹出输入）表达，由 03a 选定；理由：v1 冻结表已被 01/03a 共同依赖，新增节点需走版本 2。若 03a 判定无法表达，则写「需修订冻结项：节点类型 v1 增 `input`」。
- **D2 门控提示文案 GATE_HINTS**：决策建议：`select.options` 允许对象形态 `{value,label,hint}`（未知字段忽略，兼容 v1 字符串数组）；由 03a 确认。退路：parity 中提示文案降级为「可接受差异」。
- **D3 群/好友候选**：决策建议：`list_editor` 增可选 `fetch:{method:get_groups|get_friends}` 作为候选来源（复用已冻结 `fetch` 字段，不新增节点）；由 03a 定。

---

## 2. 通用适配器面板规格（Panel 内置）

### 2.1 组件与位置

- 新文件 `web/src/components/AdapterPanel.vue`（暂定名），挂载点复用 `ChatView.vue:40,839` 现 QqPanel 位置（直接替换 import 与模板标签）。
- 结构：头部（标题 + 刷新）→ 实例切换器 → 空态 → 状态行 + 启停重启 → **渲染器插槽**（`<AdapterUiRenderer :ui :state :core :adapter/>`，props 以 03a 为准）。
- 组件库：ui-frame（03-ev §B：Select / Button / StatusDot / Card / Empty）。

### 2.2 从 QqPanel 上移（逻辑原样）

| 元素 | 原位置 | 上移后变化 |
|---|---|---|
| Start/Stop/Restart 按钮 | QqPanel.vue:240,248 | 无（命令已中立）；pending 键改为 `adapter:<op>:<core>:<adapter>` 通用形式 |
| 状态行 dot/文案 | :224-257 | 数据源从 `qqByAdapter` 切到 `AdapterStatus`；与 `ConnectionStatusCard.vue:69-77` 共用 stateText/dot 计算（建议抽 `useAdapterStatusView()`） |
| 多实例选择器 | :205-217 | 见 2.3 |

### 2.3 语义变化

1. **实例范围**：QqPanel 只列 QQ 实例且按 `teamId`（ChatView.vue:839 传入）做 persona 过滤（QqPanel.test.ts:77,113）。通用面板列 `state.adapters` 全部平台，**persona 过滤保留**：`persona` 非空且 ≠ 当前 teamId 的条目不显示；`persona` 为空的适配器视为全局实例、始终显示（假设，原 QQ 无此情形）。选项文本 = `display_name` + persona 标签（:56 用例）。
1b. **零噪音**：无可见实例时打开面板不发任何命令（QqPanel.test.ts:63,113），通用面板必须保持。
2. **切换器显示条件**：原「仅多实例时显示」(:205)；新规则保持「≥2 个适配器才显示」，单个时直接显示该实例标题。
3. **无 ui 描述符的适配器**：只显示内置部分（状态行 + 启停重启），渲染器区显示「该适配器未提供管理界面」。这是新增能力，非 parity 项。
4. **能力门控**：渲染器区是否显示由 `ui` 是否存在决定，不看 `capabilities.gating/login`（capabilities 仅供 core 路由用，避免双源）。
5. **刷新（P1）**：原刷新会并发发 QQ 专属 Request*（filter/groups/friends/owner）；新刷新 = 重新请求 AdapterList；状态文档由 `AdapterState` 推送，不需主动拉。若 02 提供「请求重推状态」方法，则额外调用（以 02 为准）。
6. **多 core**：选中项身份 = `(core, adapter)`，命令一律经 `resolveTargetCore`（connection.ts:305+）路由，与现行一致。
7. **选中持久化**：沿用 QqPanel 现有选中逻辑（`activeAdapter` :46）；被选实例消失时回退到第一项。

8. **挂载即拉二维码**：现 QqLoginSection 未登录时挂载即 RequestQqQrcode（QqPanel.test.ts:137）。通用化后由渲染器 `image` 节点的「挂载即 fetch」语义承担（03a 定义；建议条件 = 状态文档 `online === false`，即 `image` 节点支持可选 `visible_when`/依 03a）。

---

## 3. 分阶段执行序列

### 3.1 前置依赖（门禁）

| 门禁 | 来源 | 判定方式 |
|---|---|---|
| G1 通道切换完成：Panel 能发 `InvokeAdapterOp`、收 `AdapterOpResult`、`AdapterState` 写入 `adapterUiByKey` | 02 | 02 的验收用例全绿；`grep -n "AdapterState" web/src/state.ts` 有处理臂 |
| G2 插件描述符上线：QQ 插件 `ui` / `session_labels` 非 None（plugin.rs:186-188），且载体到达 Panel | 01 | 连真实 core，DevTools 中能看到 ui 文档 |
| G3 渲染器可用：`web/src/components/adapter-ui/` 7 节点 + 测试 | 03a | `npm test -- adapter-ui` 全绿 |
| G4 W2.4 id 字符串化与 Panel number 链同批 | 01 | protocol.ts:540-548 `number[]` 已改 `string[]` 或确认随 S6 一并删除 |

依赖图：`02(G1) ─┬─> S1 ─> S2 ─┐`；`03a(G3) ─> S3 ─┤`；`01(G2) ─> S4 ─┴─> S5 ─> S6 ─> S7`。
S1/S3 可并行；S4 只需 G2；S5 起需全部门禁。

### 3.2 切片（每片可独立提交，提交后 `npm test` 与 `vue-tsc` 必须全绿）

通用验收命令（在 `echo-agent-panel/source/web`，node v22 见 00 §六）：
`npm test`（基线 56 文件 / 418 用例，删测后数量变化需在提交说明列明）；`npx vue-tsc --noEmit`。

| 片 | 内容 | 改动文件 | 额外验收 |
|---|---|---|---|
| S1 | 抽 `useAdapterStatusView()`（dot/stateText），ConnectionStatusCard 改用 | 新 `web/src/composables/useAdapterStatusView.ts`；`ConnectionStatusCard.vue:69-77` | 新增单测；ConnectionStatusCard 既有测试不变 |
| S2 | 新建 `AdapterPanel.vue`（头部/实例切换/persona 过滤/空态/状态行/启停重启 + 渲染器占位插槽）；**不挂载** | 新 `components/AdapterPanel.vue`、`components/__tests__/AdapterPanel.test.ts` | 迁入 QqPanel.test.ts:50,56,63,77,113 五条用例（见 §4.2） |
| S3 | AdapterPanel 接入 03a 渲染器（插槽替换为 `<AdapterUiRenderer>`）；无 ui 时显示「未提供管理界面」 | `AdapterPanel.vue`、其测试 | 用 fixture 描述符（00 §三 状态 + 03-ev §A 描述符）跑通 select/list_editor/image 动作载荷断言 |
| S4 | SessionSwitcher 按 platform + session_labels 通用化；颜色类改 `tag-dm/tag-group` | `SessionSwitcher.vue:55-73,268-269`、`SessionSwitcher.test.ts`、（`sessionLabelsFor` 所在 state 模块，载体按 01） | §4.3 用例改写后全绿；无 session_labels 时回退文案用例 |
| S5 | **切换挂载点**：ChatView 用 AdapterPanel 替代 QqPanel；QqPanel/QqLoginSection 文件暂留不引用 | `ChatView.vue:40,839` | 手工 parity 走查（§5 全表）；回滚 = revert 本片 |
| S6 | **删除**：QqPanel.vue、QqLoginSection.vue、QqPanel.test.ts；state.ts qqByAdapter/lastQrcode 及 7 个处理臂；pending.ts QQ 映射；protocol.ts QQ 段 | 见 §4.1 | `grep -rn "qqByAdapter\|lastQrcode\|QqPanel\|QqLoginSection\|RequestQq\|SetQq\|UpdateQq" web/src` 为空 |
| S7 | 清理注释与死引用（connection.ts:93、connection-bootstrap.test.ts:7、pending.test.ts:117、ApiSettings.vue:465 注释中的 QqPanel 字样） | 同左 | 仅注释改动，测试数不变 |

决策建议 **D4**：S5 与 S6 之间留至少一个发布周期（灰度窗口）。理由：S5 可一键 revert 回 QqPanel；S6 删除了旧事件处理臂后，回滚需同时恢复 state/protocol，成本高。若旧 QQ 命令/事件的 core 侧删除（01 计划）先于 S6，则 S6 必须紧随 S5，否则 QqPanel 回滚路径失效（见 §6）。

---

## 4. 删除清单

### 4.1 逐文件 / 逐段

| 位置 | 内容 | 处理 | 片 |
|---|---|---|---|
| `components/QqPanel.vue`（613 行） | 整文件 | 删；功能分流见 §1.1 | S6 |
| `components/QqLoginSection.vue`（148 行） | 整文件 | 删；功能分流见 §1.2 | S6 |
| `components/__tests__/QqPanel.test.ts`（226 行） | 整文件 | 用例迁移见 §4.2 后删 | S2/S3 迁、S6 删 |
| `components/ChatView.vue:40,839` | QqPanel 异步 import 与挂载 | 替换为 AdapterPanel | S5 |
| `state.ts:255-270` 附近 | `QqFilterState`（:256）、`defaultQqInstanceState()`（:264） | 删 | S6 |
| `state.ts:438-453` | `lastQrcode`（:441）、`qqByAdapter`（:443-453）类型 + 注释 | 删 | S6 |
| `state.ts:584-587` | 初始化 `lastQrcode: null` / `qqByAdapter: {}` | 删 | S6 |
| `state.ts:1373-1460` 附近 | 处理臂 `QqFilterConfig` :1373 / `QqGateMode` :1386 / `QqOwner` :1395 / `QqQrcode` :1404 / `QqLoginStatus` :1411 / `GroupList` :1423 / `FriendList` :1438 | 删全部 7 臂。⚠️ GroupList/FriendList 删前确认无非 QQ 消费方（实测仅写入 qqByAdapter） | S6 |
| `pending.ts:54-57` | `RequestQqFilterConfig/Owner/LoginStatus/Qrcode` → 事件映射 | 删；`RequestGroupList/FriendList` 若有映射一并删 | S6 |
| `protocol.ts:59-65` | AdapterStatus 的 `persona` 注释 + deprecated `container/webui_url/onebot_url` | `persona` **保留**（改注释为通用）；三个 deprecated 字段由 02 阶段 1 删，此处只核对已删 | S6 核对 |
| `protocol.ts:6` | `GateMode` 类型 | 删（渲染器不需要强类型） | S6 |
| `protocol.ts:165-174` | `GroupInfo / FriendInfo` | 删（随 GroupList/FriendList） | S6 |
| `protocol.ts:513-516, 540-548` | 命令 `RequestGroupList/FriendList/QqLoginStatus/QqQrcode`、`UpdateQqAllowlist/Denylist`、`RequestQqFilterConfig`、`SetQqGateMode`、`RequestQqOwner`、`SetQqOwner` | 删 | S6 |
| `protocol.ts:730-776` | 事件 `GroupList`、`QqFilterConfig`、`QqGateMode`、`QqOwner`、`QqLoginStatus`、`QqQrcode`、`FriendList` | 删 | S6 |
| `__tests__/phase1-management-core.test.ts:35,67-68,83-137` | 断言 qqByAdapter 按 (core, adapter) 隔离 | 改写为断言 `AdapterState` 写入 `adapterUiByKey[scopedKey]` 的跨 core 隔离（若 02 已有同类用例，则直接删除本段，避免重复） | S6 |
| 注释残留 | connection.ts:93、connection-bootstrap.test.ts:7、pending.test.ts:117、ApiSettings.vue:465 | 改注释 | S7 |

假设：protocol.ts 行段按 4b7d99b 实测（`grep -n Qq protocol.ts`）；02 阶段 1 若已重排 protocol.ts，以 grep 结果为准。

### 4.2 QqPanel.test.ts（226 行，8 用例，实测 :45 describe 下）迁移去向

| 原用例（行） | 去向 |
|---|---|
| :50 单实例不显示选择器 | AdapterPanel.test.ts（S2），断言对象改为「≥2 个可见适配器才显示」 |
| :56 多实例显示分段选择与 persona 标签 | AdapterPanel.test.ts（S2） |
| :63 无实例打开面板零命令 | AdapterPanel.test.ts（S2），断言 `send` 未被调用（含 InvokeAdapterOp） |
| :77 persona 过滤 | AdapterPanel.test.ts（S2），追加「persona 为空 → 显示」用例 |
| :113 当前智能体无实例 → 空态且零命令 | AdapterPanel.test.ts（S2） |
| :137 未登录挂载即拉二维码 | 渲染器 image 节点测试（03a 测试规范）+ AdapterPanel 集成用例 1 条（S3）：断言发 `InvokeAdapterOp{method:"login_qrcode"}` |
| :167 刷新只寻址当前实例 | AdapterPanel.test.ts（S3）：刷新/动作的 `adapter` 字段 = 选中实例，经 resolveTargetCore 路由到其 core |
| :212 多实例门控切换带 adapter | AdapterPanel 集成（S3）：切换实例后 select 动作 → `InvokeAdapterOp{adapter:<选中>, method:"set_gate_mode", payload:{mode}}` |

### 4.3 SessionSwitcher.test.ts 受影响点

- :39-54 fixture（`platform:'qq'`、id `qq:dm::…/qq:group:…`）：**保留**，id 是会话 id 不是 UI 分组键。
- :59「按平台分组展示」：断言的分组标题由「QQ 私聊/QQ 群」改为取自 `session_labels`；需在 fixture 注入 qq 的 labels，并新增「无 labels 回退 `<display_name> 私聊/群聊`」与「第二平台（如 `telegram`）独立分组」两条用例。
- :71,82,144：会话 id 未变，不受影响。
- 若有断言 `.tag-qq-dm/.tag-qq-group` 类名的用例（HEAD 复核），改为 `.tag-dm/.tag-group`。

迁移原则：S6 删除 QqPanel.test.ts 前，§4.2 八条用例必须已在新位置存在且通过；S6 提交说明列出「旧用例 → 新用例」对照，用例总数净变化需可解释。

---

## 5. parity 逐项验收清单（对照 03-ev §C）

验收方式代号：**U** = vitest 自动化（mock connection）；**F** = fixture 描述符 + 状态文档的组件测试；**M** = 连真实 core + QQ 插件手工走查（S5 后执行，记录截图/日志）；**G** = grep 静态检查。

| # | 现功能（03-ev §C） | 目标归属 | 验收项 | 方式 |
|---|---|---|---|---|
| V1 | 多实例切换 | 内置 | ≥2 实例显示切换器；单实例隐藏；persona 过滤；空态零命令（原 :50/56/63/77/113） | U |
| V2 | 状态行 | 内置 | connected/running/self_id/started_at/metadata 渲染与 ConnectionStatusCard 一致 | U + M |
| V3 | 启停重启 | 内置 | Start/Stop/RestartAdapter 带正确 adapter，路由到其 core；运行态切换后按钮文案翻转 | U + M |
| V4 | 门控模式选择 | schema（select + action） | 选中值 = `gate.mode`；切换发 `InvokeAdapterOp{set_gate_mode}`，`AdapterState` 回推后 UI 更新；多实例寻址正确（原 :212） | F + M |
| V5 | 门控提示文案 | schema（D2） | 若 03a 采纳 options 对象形态：hint 显示；否则记为可接受差异 | F |
| V6 | 白名单用户 | schema（list_editor） | 增/删一项 → `update_allowlist` 载荷正确（**string id**）；回推后列表一致 | F + M |
| V7 | 黑名单用户 | 同上 | `update_denylist` | F + M |
| V8 | 白名单群 | 同上 | 候选来自 `get_groups`（D3）；载荷 `group_ids` 为 string | F + M |
| V9 | 黑名单群 | 同上 | 同 V8 | F + M |
| V10 | 登录状态 | schema（status） | `online` true/false 显示对应状态；`self_id` 文本 | F + M |
| V11 | 二维码 PNG | schema（image + fetch） | 未登录挂载即 fetch `login_qrcode`（原 :137）；`png_base64` 渲染为 data URL；error 态可见；多实例下二维码不串号（原 lastQrcode 单槽问题） | F + M |
| V12 | WebUI 链接 | schema（link） | `bind: instance.webui_url`，新窗口打开，含 `rel="noopener"` | F |
| V13 | owner 设置 | schema（D1） | 显示 `owner`；修改发 `set_owner` | F + M |
| V14 | 刷新只寻址当前实例 | 内置 + schema | 原 :167 语义保持 | U |
| V15 | 会话列表 QQ 分组 | platform + session_labels | qq 会话按 dm/group 分组，标题来自 session_labels；无 labels 回退；第二平台独立分组 | U |
| V16 | 无残留 | — | S6 grep 清单为空；`vue-tsc` 无错 | G |
| V17 | 可访问性 | 内置 | 实例切换器/按钮有可读 label，键盘可达（自动化只能覆盖 aria 属性存在，完整验证需辅助技术人工测试） | U + M |

V4-V13 的 F 类测试由 03a 节点测试覆盖节点语义；本文只要求在 AdapterPanel 层加一条「QQ 真实描述符 fixture 全量渲染快照」集成用例，fixture 取 01 定稿的 QQ `ui` 文档（未定稿前用 03-ev §A 示例，`webui_url` 路径按 00 §三修正）。

---

## 6. 风险与回滚

| 风险 | 影响 | 缓解 | 回滚 |
|---|---|---|---|
| R1 01 描述符未覆盖全部 parity（缺 owner/hint/候选） | 功能减少 | S5 前以 §5 V4-V13 逐项核对 QQ 描述符；缺项不进 S5 | 不进 S5 即无需回滚 |
| R2 `AdapterOpResult` 无 call_id，同 (core,adapter,method) 并发结果错配（00 §一） | 二维码/列表错位 | 按 02 决策；渲染器同 key 串行化或以最后一次为准（03a） | 不涉及删除，修 02/03a |
| R3 旧 QQ 命令/事件 core 侧先删 | S5 后无法 revert 回 QqPanel | 01 删除计划须排在 Panel S6 之后；发布说明写明最低 Panel 版本 | 若已发生：只能前滚修复 AdapterPanel |
| R4 W2.4 id 类型切换与 Panel 不同批 | 名单比较失败（number vs string） | G4 门禁；S6 删除 number 链 | revert 对应 core 发布 |
| R5 persona 为空的非 QQ 适配器出现在每个智能体面板 | 视觉噪音 | §2.3-1 假设，S5 走查确认；必要时改为「仅当前 team 有绑定时显示」 | 修改过滤规则，单片改动 |
| R6 SessionSwitcher 通用化改变组顺序/颜色 | 用户感知变化 | 默认 order 保持 local → qq → other 等效 | revert S4 |
| R7 状态文档未推送（插件未 report_ui_state） | 面板空白 | 渲染器显示「等待状态」占位（03a）；内置状态行不依赖状态文档 | 无需回滚 |
| R8 删除 GroupList/FriendList 误伤其他消费方 | 运行时缺数据 | S6 前 `grep -rn "GroupList\|FriendList\|GroupInfo\|FriendInfo" web/src` 复核 | revert S6 |

回滚粒度：
- S1-S4：均未改用户可见入口（S4 除外），单片 revert 即可。
- S5：revert 即恢复 QqPanel（前提 R3 未发生）。
- S6：revert 恢复全部旧代码；若 core 已删旧事件，revert 无意义，只能前滚。因此 **S6 与 core 旧协议删除的顺序是本方案唯一不可逆点**，需在 01 的发布顺序中显式登记（D4）。

---

## 附：假设与待定项

- 行号基于 panel @ 4b7d99b 实测（state.ts / protocol.ts / pending.ts / SessionSwitcher.vue / QqPanel.test.ts / ChatView.vue）；QqPanel.vue / QqLoginSection.vue 行号引用 03-ev §B，未逐行复核。
- `AdapterPanel.vue`、`useAdapterStatusView`、`sessionLabelsFor`、`tag-dm/tag-group` 为暂定命名。
- 待定：D1（owner 编辑节点）、D2（select hint）、D3（list_editor 候选 fetch）→ 03a；`ui`/`session_labels` 载体 → 01；结果配对 → 02；D4（S5/S6 间隔与 core 删除顺序）→ 01 发布计划。
