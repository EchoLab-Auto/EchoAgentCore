# 证据包：02（Panel 阶段 1：底层通道切换）

> 配套：`00-frozen-interfaces.md`。行号基于 panel @ 4b7d99b。

## A. 现状：Panel 侧 QQ 专属面（阶段 1 的改造对象）

### 1. `web/src/protocol.ts`
- 命令联合类型 QQ 段：`:515-548`
  （RequestQqLoginStatus / RequestQqQrcode / RequestGroupList / RequestFriendList / UpdateQqAllowlist / UpdateQqDenylist / RequestQqFilterConfig / SetQqGateMode / RequestQqOwner / SetQqOwner）
- 事件联合类型 QQ 段：`:732-750`（QqFilterConfig / QqGateMode / QqOwner / QqLoginStatus / QqQrcode / GroupList / FriendList）
- `AdapterStateChanged`：`:562`（**老连接态事件**，连接状态卡用）
- **现在完全没有** `InvokeAdapterOp` / `AdapterOpResult` / `AdapterState` 类型（grep 0 处）——阶段 1 补
- QQ 命令 `GroupMode` 类型：`protocol.ts` 内 `GateMode` 定义（`'none' | 'allowlist' | 'denylist'` 附近）

### 2. `web/src/state.ts`
- `QqFilterState`（:255-264）、`defaultQqInstanceState()`（:264+）、`qqByAdapter` 类型（:443-446）与初值（:584-587）
- 事件处理臂（**阶段 1 泛化重点**）：
  - `QqFilterConfig` :1373-1384（→ `qqByAdapter[key].filter`）
  - `QqGateMode` :1386-1393（→ `.gateMode`）
  - `QqOwner` :1395-1402（→ `.owner`，number）
  - `QqQrcode` :1404-1409（→ `state.lastQrcode`）
  - `QqLoginStatus` :1411-1421（→ `.online` / `.selfId`）
  - `GroupList` :1423-1436（→ `.groups`，`[group_id:number, name]`）
  - `FriendList` :1438+（→ `.friends`）
  - `AdapterStateChanged` :623-640（连接态；注意注释「only fires while adapter running」）
- 归并键函数 `scopedKey(core, id)`（`web/src/keys.ts:12`）；`evtCore(e)` 取事件来源 core；`state.lastQrcode` 现存单槽（多实例时分不清来源，阶段 1 注意）

### 3. `web/src/pending.ts`
- `REQUEST_RESPONSES` 命令→事件映射表（:46-67）含 6 条 QQ：`RequestQqFilterConfig: 'QqFilterConfig'` 等
- `sendPending(command, key)` 登记（connection.ts:221-229）；`resolvePendingEvent`（:173+）
- **AdapterOpResult 无请求 id**：配对方案由 02 决策（(core,adapter,method) 键 / 扩展协议加 call_id），并同步写进文档（别拖到 03）

### 4. 命令发送现状（QqPanel / QqLoginSection 直连点）
- `QqPanel.vue:88-101`（refreshLists）：RequestGroupList / RequestFriendList / RequestQqFilterConfig / RequestQqOwner / RequestQqLoginStatus
- `QqPanel.vue:133-183`：SetQqGateMode、UpdateQqAllowlist×2、UpdateQqDenylist×2（toggle 逻辑，number 比较）
- `QqPanel.vue:240-248`：Start/Stop/RestartAdapter（**已中立，迁移期不动**）
- `QqLoginSection.vue:38-49`：RequestQqLoginStatus、RequestQqQrcode；（:108 webuiUrl 显示）
- pending 键模式 `qq:filter:<inst>` / `qq:groups:<inst>` / `qq:friends:<inst>` / `qq:owner:<inst>`（QqPanel.vue:106-110，`pk()` 前缀模式）

### 5. 挂载与消费
- `ChatView.vue:40`（defineAsyncComponent）+ `:839`（`<QqPanel :team-id>`）
- `ConnectionStatusCard.vue:69-77`：`state.adapters` 行（display_name / self_id / running / connected）
- `SessionSwitcher.vue:56-64`：`platform === 'qq'` 分支（qq-group / qq-dm 分组）+ `:66-72` GROUP_LABEL（阶段 2 才动，这里仅登记）

## B. 阶段 1 的既定边界（冻结）

- **目标**：QqPanel / QqLoginSection 的**数据面**全部切 `InvokeAdapterOp` + `AdapterOpResult` + `AdapterState`（UI 暂留，功能不减）；`qqByAdapter` 的**数据来源**改从 `AdapterState.state`（状态文档，形状见 00 文件 §三）派生；阶段 2 删组件。
- **不做的**（留给 03）：schema 渲染器、QqPanel/QqLoginSection 组件删除、SessionSwitcher 通用化、`adapterUiByKey` 最终形态（02 可用过渡名，但归并键必须是 `scopedKey(core, adapter)`）。
- protocol.ts 旧 QQ 类型：**阶段 1 先并存**（QqPanel 老代码路径逐个替换后再删；文档给出「替换完毕即删」的序列与保留清单）——或论证后一次性切换，二选一写明理由。
- Multi-upstream：一切经 `resolveTargetCore(command)` 路由（connection.ts:305+），`InvokeAdapterOp.adapter` = 实例名，core 路由沿用。

## C. 交付物要求（02 文档）

1. 命令映射表：旧 9 命令 → `{ method, payload }`（与 00 文件 §四方法名严格一致）
2. 事件映射：`AdapterState.state` → `qqByAdapter` 各字段派生规则（对照 §A.2 现处理臂；含二维码 `lastQrcode` 的归属决策）
3. pending 配对方案（决策 + 理由 + 对 pending.ts 的改动点）
4. 改造文件清单（protocol.ts / state.ts / pending.ts / QqPanel.vue / QqLoginSection.vue / tests）与每个文件的「改什么、删什么、保什么」
5. 测试计划：现有 418 用例基线不破；QqPanel.test.ts（226 行）与相关用例的适配点
6. 切片 S1..Sn（可独立提交）+ 每片验收命令（`npm test` / `vue-tsc -b`）

## D. 补充核实（2026-10-16 12:25）

- **Panel 侧 `AdapterStatus` 缺 `metadata`**（protocol.ts:51-65）：只有 name/display_name/connected/running/self_id/bind_address/started_at/persona + QQ 三字段（container/webui_url/onebot_url 未标 deprecated）。Core 侧 W2.2 已加 `metadata: Vec<{key,label,value}>`——**02a 需补 Panel 类型**（建议同步标 deprecated 三字段），否则 metadata 永远到不了前端。
- `state.adapters: AdapterStatus[]`（state.ts:433）；`AdapterList` 事件处理在 state.ts:641。
- `AdapterStateChanged` 处理器在 state.ts:623-640（连接态专用；仅 adapter_name/connected/self_id）。
- QqLoginSection 的 lastQrcode 读取路径：QqLoginSection.vue:69（经 cast 读全局单槽）——03a §4.3 已决定由渲染器节点局部化修复；阶段 1 过渡期此单槽保留、阶段 2 删。
