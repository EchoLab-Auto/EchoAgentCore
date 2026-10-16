# 02a · Panel 阶段 1：协议与状态层设计（通道切换）

> 状态：设计草案。依据：`evidence/00-frozen-interfaces.md`（下称 00）、`evidence/02-evidence-panel-channel.md`（下称 02E）、`03a-panel-renderer-spec.md`（下称 03a）。
> 基线：panel @ 4b7d99b / core @ 3541b46 / echo-adapter-qq @ 79de9ec。范围：protocol.ts / state.ts / pending.ts / connection.ts / store.ts + 新增服务模块；**不含**组件改造（02b）与渲染器（03）。
> 本文给出 03a/03b 引用的「02 的约定」：配对键、超时、错误文案、断线清理、`invokeAdapterOp` 语义（§5.3 汇总）。

章节骨架：
- §0 补充核实（影响决策的新事实）
- §1 决策 D1–D4
- §2 命令映射表（旧 10 命令 → {method, payload}）
- §3 事件映射（AdapterState → adapterUiByKey）
- §4 protocol.ts 类型增补与旧类型删除序列
- §5 pending.ts / connection.ts / store.ts / 新服务模块改动点
- §6 state.ts 改动点（逐处理臂）
- §7 切片、验收命令与测试适配清单

## §0 补充核实（证据包之外、直接影响决策的事实）

| # | 事实 | 出处 | 影响 |
|---|---|---|---|
| F1 | `InvokeAdapterOp` **失败路径发的是 `BackendEvent::Error{session_id:None}`**，不是 `AdapterOpResult{error}`；实例解析失败同样发 `Error`。Core 当前**从不**填 `AdapterOpResult.error` | core `packages/adapter_qq/commands.rs:403-422`、`:150-153` | 失败无法配对：Panel 只能看到一条 toast（state.ts:1470-1476）+ 请求挂到超时。D1 必须连带修 Core |
| F2 | `AdapterState` 只由 `emit_adapter_state_events` 发出；`InvokeAdapterOp` 仅在 4 个写方法后调用它（`SHARED_WRITE_METHODS`）；读方法（`get_*`/`login_status`）**不推** AdapterState；宿主 `report_ui_state` 只更新缓存不广播 | commands.rs:133-149、:426-479；01E §B.3 | Panel 首屏拿不到状态文档（唯一能触发的读路径是旧 `RequestQqFilterConfig`，commands.rs:263-273）。需 Core 改动 C2（§1-D2） |
| F3 | 写方法会 best-effort 广播到其它 QQ 实例，但 AdapterState 只为**目标实例**发出 | commands.rs:484-505 | 多实例下其它实例的 `adapterUiByKey` 过期到下次刷新（C3 可选修） |
| F4 | 插件 `parse_ids` 缺字段 = **空表**（不是「不改」）；接受 number/string 两种 id；`parse_id`（owner）同样两者皆可 | echo-adapter-qq `adapter/qq/mod.rs:1469-1506` | `update_allowlist` 必须**同时**带 `user_ids` 与 `group_ids`，否则清空另一表。**03a §2.5/Q1 的「部分载荷」假设与现实现冲突**，需 01 改插件或 03a 改为整对象发送 |
| F5 | 插件结果 id 已是字符串：`get_groups/get_friends` → `[{id:string,name}]`，`get_filter_config` 四表 `string[]`；但 `owner` 在状态文档中仍为 number | mod.rs:1244-1267、:1298-1340、:1518-1523 | 新通道下 Panel 名单/群/好友侧可直接全用 string，**不必等 W2.4**；owner 统一 `String()` |
| F6 | `store.ts::dispatch` 先 `resolvePendingEvent(event, core)` 再给载荷注入 `core`、再 `reduce` | web/src/store.ts:29-77 | 新事件的来源 core 可用 `evtCore(e)`（state.ts:131）；配对 tap 放在 dispatch 内 |
| F7 | `sendCommand(command, targetCore?)` 已支持显式 core；`sendPending` 不支持，且 `sendCommand` 返回 `void`（拒发只弹 toast） | connection.ts:221-229、:333-359 | 需小改：`sendPending` 增 `targetCore`、`sendCommand` 返回是否已发/已排队 |

## §1 决策 D1–D4

### D1 配对方案 = 键配对（不扩展线协议 call_id）

- 键：`adapter-op:${scopedKey(core, adapter)}:${method}`——与现 `qq:<kind>:<instance>`（QqPanel.vue:106-110）同构，即 03a §4.2 的建议键；`scopedKey` 见 `web/src/keys.ts:13-16`（core 为 null 时退化为裸 adapter，单上游零变化）。
- 服务层提供 `invokeAdapterOp(core, adapter, method, payload)` 返回 `Promise<{result, error}>`（签名见 §5.2）；内部 `trackRequest` + `sendCommand`，`AdapterOpResult` 到达时按同一键 resolve。
- 同 `(core, adapter, method)` 并发由服务层按键 FIFO 排队（第二个请求等第一个 settle）；与 03a §4.2 渲染器层串行化互为兜底，谁先命中都安全。
- 不选 call_id 的理由：线协议 `AdapterOpResult` 无请求 id（00 §一），扩展需同时改 core/echo-protocol/中继三处，成本与回归面远大于收益；同键并发在「同 nodeKey 不并发 + 单实例单卡」交互下本就罕见。
- 已知限制（接受）：超时后用户重试的在途窗口内，前一次请求的迟到响应会按同键配对到重试请求（无线协议 id 无法区分）。窗口≈用户重试决策时间，误配仅表现为一次陈旧结果；若将来造成可见问题，即触发 call_id 方案（需修订冻结项：00 §一）。

### D2 首屏状态获取 = get_state + Core 推送

- 现状（F2）：读方法不推 `AdapterState`，仅 4 个写方法会；QQ 之外没有通用首屏路径（旧 `RequestQqFilterConfig` 是 QQ 专属）。
- 方案：组件挂载 / 实例选定后调用 `invokeAdapterOp(core, adapter, 'get_state', {})`：
  1. Core 新增 custom 方法 `get_state`（返回完整 `cached_state`，01a §3.1 已定）；
  2. Core 前置修复 C2：读方法完成后也推送 `AdapterState`（至少覆盖 `get_state`）；
  3. Panel 只把 `AdapterState` 事件写入 `adapterUiByKey`（单一写入源）；`get_state` 的 `AdapterOpResult` 仅用于确认成功/错误提示（03b §2.3-5 同判）。
- 触发时机（挂载 / 切实例 / AdapterList 刷新后）由 03b 负责；02a 只保证「发得出、配得上、错得清」。重复 `get_state` 幂等：结果就是最新 `cached_state`，推送同样是整文档替换。

### D3 协议类型：增补、deprecated、删除序列

- 增补（Panel 侧 `protocol.ts`；core `echo-protocol` 已定义，见 00 §一）：命令 `InvokeAdapterOp`、事件 `AdapterOpResult` / `AdapterState`；`AdapterStatus` 增 4 字段（形状见 §4.2）。
- 旧 QQ 类型本阶段**全部保留**，加 `@deprecated` 注释（迁移期共存）：命令 10 条 `protocol.ts:513-516、:540-548`、事件 7 条 `:730-752、:776`、`AdapterStatus` 旧 3 字段 `:61-66`。删除序列：
  1. **阶段 2（03b S6）**：删 Panel 使用点（state.ts 7 个 QQ 臂、pending.ts 6 条 QQ 映射、QqPanel/QqLoginSection/`qqByAdapter`/`lastQrcode`）；旧类型保留（零引用，防跨分支冲突）。
  2. **阶段 3（03b 之后，Panel 独立 commit）**：删 `protocol.ts` 旧 QQ 类型；`protocol-sync.test.ts`（reduce case ⊆ `BackendEvent` 联合）自动兜底遗漏引用。
  3. **阶段 4（Core 侧，按 01b 发布节奏）**：删 echo-protocol 旧 QQ 命令/事件，必须晚于阶段 2 上线（旧 Panel 兼容窗），升级指南归 01b。
- 守护：`protocol-sync.test.ts` 的「归零棘轮」禁止生产代码再出现 `as unknown as BackendCommand` 双重断言；新增 reduce 臂必须先入联合类型（§7 的 S1 → S5 顺序）。

### D4 多上游路由（基于 F7 的小改）

- `sendPending(command, key, targetCore?)`：新增第三参；`trackRequest` 的 core = `targetCore ?? resolveTargetCore(command) ?? null`（等待目标与发送目标一致，沿用现注释语义 connection.ts:226-232）。
- `sendCommand(command, targetCore?)` 返回值 `void → boolean`：`true` = 已发送或握手期已排队；`false` = 拒发（多上游解析不出目标 / 未连接）。返回值加宽是兼容变更，现有调用点零改动。
- `invokeAdapterOp` 恒显式传 `meta.core`（03a §4.1 selector 的 meta）。core 非 null ⇒ 命令带 `{core, frame}` 信封；core 为 null（单上游）⇒ 裸命令，且当 `state.knownCores.size > 1` 时直接 reject（镜像 sendCommand 的防误投守卫）。
- `resolveTargetCore` 不动；`InvokeAdapterOp` 无 team_id 字段、不参与人格一致性守卫。
- F7 边界（暂不修）：握手期排队的 adapter-op 在补发时重解析 core（connection.ts:127-134），多上游下可能落到 activeRegion。适配器面板仅在 connected 后可交互，命中窗口≈0；若 03b 冒烟复现，按 S5 修复：`pendingCommands` 改存 `{command, core}` 对。

## §2 命令映射表（旧 10 命令 → InvokeAdapterOp）

统一形态：`invokeAdapterOp(core, adapter, method, payload)` → 线上 `{ InvokeAdapterOp: { adapter, method, payload } }`。旧命令的可选 `adapter?` 字段升级为**必填**的 `adapter` 顶层字段；method 名严格取自 00 §四（12 方法 + `get_state`）。

| # | 旧命令（panel 4b7d99b） | 新 method | payload | 结果消费（result） |
|---|---|---|---|---|
| 1 | `RequestGroupList{adapter?}` :513 | `get_groups` | `{}` | `{groups:[{id,name}]}`，id 已是 string（F5）→ 02b 覆盖层 / 03a list 节点 |
| 2 | `RequestFriendList{adapter?}` :514 | `get_friends` | `{}` | `{friends:[{id,name}]}`，同上 |
| 3 | `RequestQqLoginStatus{adapter?}` :515 | `login_status` | `{}` | `{running, online, user_id}`（02b §0 实测） |
| 4 | `RequestQqQrcode{adapter?}` :516 | `login_qrcode` | `{}` | `{png_base64}`；**只进 image 节点局部**（03a §4.3），不再进全局 state |
| 5 | `UpdateQqAllowlist{user_ids,group_ids}` :540 | `update_allowlist` | `{user_ids, group_ids}`（**整对象**，见下） | 写方法：无结果消费；AdapterState 回推 |
| 6 | `UpdateQqDenylist{user_ids,group_ids}` :541 | `update_denylist` | `{user_ids, group_ids}`（**整对象**） | 同上 |
| 7 | `RequestQqFilterConfig{adapter?}` :542 | `get_filter_config` | `{}` | `{allowlist_users,…}` 四表 `string[]`（F5） |
| 8 | `SetQqGateMode{mode}` :543 | `set_gate_mode` | `{mode}` | 写方法：AdapterState 回推 `gate.mode` |
| 9 | `RequestQqOwner{adapter?}` :547 | `get_owner` | `{}` | `{owner_qq: number}` |
| 10 | `SetQqOwner{owner_qq}` :548 | `set_owner` | `{owner_qq}` | 写方法：AdapterState 回推 `owner` |

- **F4 注记（重要）**：`update_allowlist/update_denylist` 必须**整对象**发送——`user_ids` 与 `group_ids` 同时给；插件 `parse_ids` 对缺省键返回空表（`mod.rs:1469-1506`），漏一个即**清空**该表。这是现实现行为（02b §0 亦核实），插件侧「部分载荷」修复计划在 01；02a/02b 一律按整对象发送，不依赖 01 的修复。
- id 类型：名单/群/好友侧统一 `string`（F5，插件接受 string 或 number；与状态文档同型）；`owner_qq` 沿用 number（`parse_id` 两者皆可，owner 在状态文档中仍为 number，F5）。
- 无旧命令对应的方法：`get_gate_mode`（新路径由 `get_state` / AdapterState 的 `gate.mode` 覆盖首屏；保留给需单独读取的场景）、`instance_info`（03b 通用面板用）、`get_state`（D2 首屏）。`SetQqGateMode` 的 GateMode 值域不变（`'none'|'allowlist'|'denylist'`，protocol.ts:6）。
- 命令名拼写由 TS 字面量联合防错（`AdapterOpMethod`，§4.1）；旧命令迁移期并存，Panel 新旧双轨互不影响（core 侧同一 `AdapterManagement` 分发）。
- 旧 `pending.ts` 映射：6 条 QQ 映射（`RequestQqFilterConfig/Owner/LoginStatus/Qrcode`、`GroupList/FriendList` 事件侧）本阶段保留（03b S6 随旧臂删除）；新路径不加入 `REQUEST_RESPONSES`（§5.1）。

## §3 事件映射：AdapterState → adapterUiByKey

### 3.1 归并规则

- 入：`BackendEvent::AdapterState { adapter, state }`（00 §一）；出：`state.adapterUiByKey[scopedKey(evtCore(e), e.adapter)] = e.state`——**整文档替换**，不做字段 merge（03a §4.1）。
- core 来源：`store.dispatch` 在 reduce 前给对象载荷注入 `core`（store.ts:72-77），reduce 内以 `evtCore`（state.ts:131）读取；单上游无信封 ⇒ core=null ⇒ 键退化为裸实例名（与 qqByAdapter 同构习惯）。
- 写入**不**经 `managementCoreCurrent` 过滤：多 core 聚合靠 scopedKey 隔离，过滤会让非活跃 core 的文档永远进不来（渲染器侧要按 core 展示）。
- 值类型 `unknown`：Panel 不校验形状（00 §三 是唯一引用标准），bind 取值时的容错由 03a §2.0 负责。

### 3.2 旧字段对照（02E §A.2 的 7 个处理臂，panel@4b7d99b 实测行号）

| qqByAdapter 旧字段（state.ts:443-453） | 旧来源臂 | 新文档路径（00 §三） | 迁移说明 |
|---|---|---|---|
| `filter.{allowlist,denylist}_{users,groups}`（number[]） | QqFilterConfig :1373-1385 | `gate.allowlist_users` 等四表（string[]） | 02b `toQqView` 做 string→number 投影；覆盖层按 docRef 失效 |
| `gateMode` | QqGateMode :1386-1394 | `gate.mode` | 值域一致（GateMode） |
| `owner`（number） | QqOwner :1395-1403 | `owner` | 保持 number（F5） |
| `online` | QqLoginStatus :1411-1422 | `online` | |
| `selfId` | QqLoginStatus（同上） | `self_id`（string） | |
| `groups`（[number,string][]） | GroupList :1423-1437 | **状态文档无此字段** | 读路径覆盖层（02b §3），事件不再写全局 |
| `friends` | FriendList :1438-1455 | 同上 | 同上 |
| `lastQrcode`（全局单槽） | QqQrcode :1404-1410 | 无 | 03a §4.3 判死：image 结果只进组件局部，S6 删 |

- 实例元数据：`instance.{persona,container,webui_url,onebot_url,bind_address}`、`running` 由通用面板 + AdapterStatus（03b §2）消费；`self_id/online` 状态行取状态文档。旧 `container/webui_url/onebot_url` 的组件读取（02b §2.4）过渡期继续读 `AdapterStatus` 旧字段（deprecated，S6 后删）。
- `AdapterStateChanged` / `AdapterList`（state.ts:623-644）**不动**：运行/连接态与 `state.adapters` 仍由它们维护，AdapterState 只管状态文档；新通用面板复用 `upsertAdapter`。
- 断线清理：core 离线/移除 ⇒ 清该 core 前缀的 `adapterUiByKey` 条目（§5.4 钩子）；渲染器收到 `state=null` 进 null 态（03a §4.1）。单上游（core=null）不适用该钩子（无 core 名可判离线），保持现状不主动清。
- 顺序性：`AdapterState` 与 `AdapterOpResult` 到达顺序无保证（03a §4.2 括注）——两臂互不依赖，各自独立幂等。

## §4 protocol.ts 类型增补

### 4.1 三个新类型（与 core `echo-protocol` 一一对应，00 §一）

```ts
// BackendCommand 联合新增：
| { InvokeAdapterOp: { adapter: string; method: string; payload: Record<string, unknown> } }
// BackendEvent 联合新增：
| { AdapterOpResult: { adapter: string; method: string; result?: unknown; error?: string | null } }
| { AdapterState: { adapter: string; state: unknown } }
// 方法名防拼写（建议）：
export type AdapterOpMethod = 'set_gate_mode' | 'get_gate_mode' | 'update_allowlist' | 'update_denylist'
  | 'get_filter_config' | 'set_owner' | 'get_owner' | 'login_status' | 'login_qrcode'
  | 'get_groups' | 'get_friends' | 'instance_info' | 'get_state';
```

- `payload` 缺省 `{}`（core command.rs:279-287）；`result` 线上 null 时省略 ⇒ 用可选 `result?: unknown`；`error` 缺省不存在，Panel 侧统一读 `error ?? null`。
- 序列化：`payload` 必须为 JSON 对象（`serializeCommand` 直出）；队列/重试不改变 payload 引用。
- 新类型**不**允许双重断言绕过（protocol-sync 归零棘轮）；`AdapterOpMethod` 是 Panel 侧便利类型，不要求 core 同步。

### 4.2 AdapterStatus 增 4 字段（形状以 01a §1.1 为准，E）

- `metadata: Array<{ key: string; label: string; value: string }>`（W2.2 通用 KV；core `AdapterStatus` 已有，**Panel 侧本次补齐**——现 protocol.ts:51-67 尚无）。
- `platform?: string`、`ui?: unknown`（描述符 v1，00 §二）、`session_labels?: Record<string, string>`——三者均可选，旧 core 不下发时 undefined，消费方容错（03a §4.1 selector：无 ui ⇒ descriptor 为 null，渲染器区域不显示）。
- 旧 3 字段 `container/webui_url/onebot_url`（protocol.ts:61-66）标 `@deprecated`：迁移期 02b §2.4 等仍读，删除归 S6→阶段 3；persona 不在删除列表（多实例归属仍用）。

### 4.3 旧类型 deprecated 与删除序列

- 标注方式：union 条目上方加注释块「`@deprecated 适配器泛化迁移期保留；使用点见 03b S6 删除清单；类型删除见 §1-D3 阶段 3`」——不改类型本身，避免测试/工具链噪声。
- 序列（重复 §1-D3 供 §7 引用）：阶段 2 删使用点（03b S6）→ 阶段 3 删 Panel 类型 → 阶段 4 删 core 旧命令/事件（01b 节奏，必须晚于阶段 2）。
- 反向守护：本阶段**禁止**新增对旧 QQ 类型的引用（新代码一律走 `InvokeAdapterOp` / 三个新类型）；迁移期内的 QqPanel 过渡代码（02b）是既有代码的机械替换，不算新增引用。

## §5 服务层改动（pending.ts / connection.ts / store.ts / adapterOps.ts）

### 5.1 pending.ts

- 现状：`trackRequest`（:111）、`resolvePendingEvent`（:173）、`releasePending`（:188）、私有 `finish`（:148）、超时常量 `PENDING_TIMEOUT_AFTER_MS = 20_000`（:34）。
- 改动 ①：`finish` 以 `settlePending(key)` 导出（语义不变：立即清未显示、已显示保最短 400ms），供服务层按精确键销账。
- 改动 ②：新增 `dropPendingOfCore(core: string | null)`——删除 `entry.core === core` 的条目（精确匹配；core 为 null 需调用方显式传入）。用于 §5.4 断线清理。
- **不**把 `AdapterOpResult` 加进 `REQUEST_RESPONSES`：通用销账按命令名匹配，会误销同命令的其它 (adapter,method) 键；新路径一律经 `settlePending` 显式销账。
- 加载态语义保留：组件可继续 `usePending('adapter-op:…')` 拿骨架/慢/超时三态；超时 phase 与 Promise reject **并行存在**（一个 UI 层、一个数据层），互不依赖。

### 5.2 新服务模块 `web/src/adapterOps.ts`

- 键构造函数：`adapterOpKey(core, adapter, method)` → `adapter-op:${scopedKey(core, adapter)}:${method}`（模板串；`scopedKey` 复用 `web/src/keys.ts`）。
- 正式签名（02b A1 以本文为准）：

```ts
export function invokeAdapterOp(
  core: string | null, adapter: string, method: AdapterOpMethod,
  payload: Record<string, unknown> = {},
): Promise<{ result: unknown; error: string | null }>;
```

- 语义：
  1. **排队**：内部 `Map<key, FIFO>`；同键第二个请求等第一个 settle 才 `trackRequest + sendCommand`（服务层串行化，03a §4.2 渲染器层再串行互为兜底）。不同键互不阻塞。
  2. **发送**：`trackRequest(key, cmd, core)` → `sendCommand(cmd, core ?? undefined)`；返回 false 或 `core==null && knownCores.size>1` ⇒ 立即 reject（文案沿用 sendCommand toast：`'未连接到 Core，命令未发送'` / `'尚未选择 agent（无法确定运行区域），命令未发送'`），不登记 pending。
  3. **超时**：`PENDING_TIMEOUT_AFTER_MS`（20s）后 `reject(new Error('操作超时，请重试'))`；pending 条目的 timeout 态保留（与现状一致：重试覆盖 / 组件卸载 release）。
  4. **结果**：`settleAdapterOp(key, {result, error})` resolve `{ result: result ?? null, error: error ?? null }`（业务失败也 resolve，03a §4.2），随后出队、释放键、启动下一个排队者。
  5. **清理**：`dropCoreAdapterChannel(core)`——清 `state.adapterUiByKey` 前缀键 + `dropPendingOfCore(core)` + 拒绝该 core 在途/排队 Promise（`Error('核心已离线，操作已取消')`，文案见假设）+ 通知订阅者。
  6. **钩子**：`onCoreDropped(fn): () => void` 订阅/退订（回应 02b A3：readOverlay 等 UI 侧缓存借此清理；钩子幂等，与组件 watch 兜底可并存）。
- 模块内部状态（队列/Promise/订阅表）用普通 Map/Set，不进 Vue 响应式；唯一响应式数据在 state.ts / pending.ts。

### 5.3 「02 的约定」汇总表（03a/03b 引用基准）

| 约定 | 值 | 出处 |
|---|---|---|
| 配对键 | `adapter-op:${scopedKey(core,adapter)}:${method}` | §5.2；03a §4.2 |
| 配对粒度 | `(core, adapter, method)`；同键 FIFO 排队（第二请求等待） | §1-D1 |
| 助手签名 | `invokeAdapterOp(core, adapter, method, payload) → Promise<{result, error}>` | §5.2（02b A1） |
| 超时 | 20s（`PENDING_TIMEOUT_AFTER_MS`）后 reject(`Error('操作超时，请重试')`) | pending.ts:34，文案沿用现状 |
| resolve/reject | 业务失败 resolve(`{error: string}`)；超时/拒发/离线 reject（`Error`） | 03a §4.2 |
| pending 销账 | 显式 `settlePending(key)`；不加入 `REQUEST_RESPONSES` | §5.1 |
| 断线清理 | 上游 core 离线/移除：清该 core 前缀 `adapterUiByKey` + pending + 在途 Promise + `onCoreDropped` 通知；WS 断线不触发（沿用 20s 超时 + `connected` 优先态） | §5.4；03a §4.1 |
| 多上游 | 显式 `meta.core` 路由；单上游 core=null 裸命令 | §1-D4 |
| 跨平台 | 同一套约定复用于所有适配器，无 QQ 分支 | 00 §五 |

### 5.4 connection.ts / store.ts / 清理钩子

- `connection.ts`：`sendPending(command, key, targetCore?)`（:226-232）；`sendCommand` 返回 boolean（:333-374）；`resolveTargetCore` 不动（:305-331）。握手期补发的边界见 §1-D4。
- `store.ts`：**无需改动**。F6 的顺序（:31 销账 → :72-77 注入 core → reduce）恰好让 `AdapterOpResult` 的 core 在 reduce 里可经 `evtCore` 读取；销账由 state.ts 臂调 `settleAdapterOp` 完成（非通用 `resolvePendingEvent`）。
- 清理钩子触发点（均非组件层，02a 范围内接线）：
  1. `upstreams.ts::refreshUpstreams`（:17-24）：diff 上一帧——条目从上游列表消失 或 `connectivity` online→offline ⇒ `dropCoreAdapterChannel(name)`；重新 online 后由 03b 的挂载时机重新 `get_state`（无需 02a 处理）。
  2. `state.ts::removeSessionsOfCore`（:1592-1611）：追加 adapterUiByKey 前缀清理（纯 state 本地，删除路径在轮询窗口内也即时生效）。
- 对 02b 两份假设的回应：**A1**（助手签名与键格式）按 02b §0 原样落定，模块名以本文 `adapterOps.ts` 为准（改名则机械替换）；**A3**（readOverlay 清理钩子）由 `onCoreDropped` 提供，QqPanel 挂载时订阅、卸载退订；若 02b 保持组件内 watch 兜底亦无冲突。

## §6 state.ts 改动（逐处理臂）

### 6.1 AppState 新字段

- `adapterUiByKey: Record<string, unknown>`，初值 `{}`，与 `qqByAdapter` 并列（类型区 :441-453、初值区 :586-587 旁）；注释锚 00 §三。值 = `AdapterState.state` 原始文档，Panel 不校验（§3.1）。
- `qqByAdapter` / `lastQrcode` 保留至 **03b S6** 删除（本阶段零新写入）；新代码不得再写 qqByAdapter（除既有臂外）。

### 6.2 两个新处理臂（switch 内，QQ 臂之前）

1. `case 'AdapterOpResult'` —— **结果配对 tap**：
   - `settleAdapterOp(adapterOpKey(evtCore(e), e.adapter, e.method), { result: e.result ?? null, error: e.error ?? null })`；由服务层 resolve 在途 Promise 并放行同键队列。
   - **不**经 `managementCoreCurrent` 过滤（op 可发往非活跃 core；失败也必须能配对）；无命中的迟到结果静默丢弃（§1-D1 限制）。
   - 空 `error` 亦视为成功（`error ?? null`）。
2. `case 'AdapterState'` —— 状态文档写入：
   - `state.adapterUiByKey[scopedKey(evtCore(e), e.adapter)] = e.state`（整文档替换，§3.1）；同样不过滤。
   - 与 `AdapterOpResult` 顺序无关、各自幂等（03a §4.2）。
- 两臂落地前必须先在 protocol.ts 联合类型中定义（protocol-sync.test.ts 守护，S1 → S5 顺序）；C2 未上线（读方法不推）期间，本臂仅由写方法回推 / 01a §1.2 的插件主动上报路径触达，首屏空态由 03b 的「重试」兜底。

### 6.3 现 7 个 QQ 臂：整段保留（行号与处置）

| 臂 | 行号 | 处置 |
|---|---|---|
| `QqFilterConfig` | :1373-1385 | 保留（旧面板读 `qqByAdapter.filter`）；S6 删除 |
| `QqGateMode` | :1386-1394 | 保留；S6 删除 |
| `QqOwner` | :1395-1403 | 保留；S6 删除 |
| `QqQrcode` | :1404-1410 | 保留（写 `lastQrcode` 全局单槽）；S6 删除，语义由 03a §4.3 组件局部取代 |
| `QqLoginStatus` | :1411-1422 | 保留；S6 删除 |
| `GroupList` | :1423-1437 | 保留；S6 删除（读路径改覆盖层，02b §3） |
| `FriendList` | :1438-1455 | 保留；S6 删除（同上） |

- 过渡期双轨不冲突：`qqByAdapter`（旧事件键）与 `adapterUiByKey`（新事件键）是独立键空间，同名实例互不干扰；QqPanel 经 02b 的 `toQqView` 叠加层消费两者。
- `AdapterStateChanged` / `AdapterList`（:623-644）：保留不动；`AdapterList` 后续在 03b 侧补 `metadata/ui/platform/session_labels` 消费（类型已在 §4.2 就绪）。

## §7 切片、验收与门禁

### 7.1 Panel 侧切片（每片可独立提交）

| 片 | 内容 | 验收 |
|---|---|---|
| S1 | protocol.ts：+3 类型（`InvokeAdapterOp`/`AdapterOpResult`/`AdapterState`）、`AdapterStatus` +4 字段、旧类型/旧字段 `@deprecated` 注释；不删任何东西 | `npm test` 全绿；`npx vue-tsc -b` 仍恰 1 条已知错误 |
| S2 | pending.ts：导出 `settlePending`、新增 `dropPendingOfCore`；pending.test.ts 增「显式销账 / 按 core 清理」用例 | 同上 |
| S3 | connection.ts：`sendPending` 三参、`sendCommand` 返回 boolean；适配 core-routing / connection-bootstrap 用例（拒发返回 false） | 同上 |
| S4 | 新模块 `adapterOps.ts` + `adapterOps.test.ts`（键格式、FIFO、超时文案、拒发 reject、dropCoreAdapterChannel 四联动作、onCoreDropped）；依赖 S2/S3 | 同上 |
| S5 | state.ts：`adapterUiByKey` 字段 + 两个新臂 + `removeSessionsOfCore` 扩展；upstreams.ts 离线/移除 diff 检测；集成用例（AdapterState→键写入；AdapterOpResult→Promise resolve） | 同上 |
| S6 | 收尾：文档回填（02b A1/A3 确认）、门禁自检 | §7.3 门禁 |

### 7.2 Core 前置修复清单（core 仓库，独立 PR，不属 Panel 代码）

| # | 修复 | 对应 | 验收 |
|---|---|---|---|
| C1 | `InvokeAdapterOp` 失败路径改发 `AdapterOpResult{error}`（现为 `BackendEvent::Error`，F1）；实例解析失败同理 | F1 | core `cargo test --workspace` 全绿；Panel 侧无需改动即受益（超时兜底已兜住旧行为） |
| C2 | 读方法（至少 `get_state`）完成后也推送 `AdapterState`（首屏路径，F2） | F2 | 同上；旧 `RequestQqFilterConfig` 兼容性回归（01a §1.2-4 的旧事件派生） |
| C3 | custom 方法 `get_state`（返回完整 `cached_state` 并触发推送，01a §3.1） | D2 | 同上；插件侧零改动（custom op 通道） |

- 与 Panel 的兼容矩阵：C1/C2/C3 均未上线时，D1 的超时兜底（20s reject）与 D2 的空态兜底即可工作；任一上线即渐进增强，无版本耦合。

### 7.3 验收命令与 03b G1 门禁对齐

- `cd web && npm test`：基线 **56 文件 / 418 用例不减**（00 §六）；S2/S4/S5 新增用例计入分子。
- `cd web && npx vue-tsc -b`：基线**恰 1 条**已知错误 `ChatView.vue:24 TS6133`（未使用导入）；出现第 2 条错误（新增错误或基线外 TS6133）即失败。
- 门禁自检：`grep -n "case 'AdapterState'" web/src/state.ts` 非空；`grep -n "InvokeAdapterOp" web/src/protocol.ts` 非空；`grep -rn "adapterUiByKey" web/src/state.ts web/src/adapterOps.ts` 命中。
- 与 03b G1 对齐：03b §3.1 前置依赖 =「02 验收全绿 + state.ts 有 AdapterState 处理臂」。本表 S1-S5 全绿 + 上述三条 grep 即满足 G1；02a **不**删 `qqByAdapter`/`lastQrcode`（03b S6 职责，D3 阶段 2），G1 通过后 03b 的组件重接线可与 C1-C3 并行。

## 假设与待定项（02a 侧）

1. **命名**：新模块名 `web/src/adapterOps.ts`、函数名 `invokeAdapterOp / adapterOpKey / settleAdapterOp / dropCoreAdapterChannel / onCoreDropped / AdapterOpMethod` 由本文首次落定；02b 已声明「名称/签名以 02a 最终稿为准，改名机械替换」，03a 只消费行为不消费名字。
2. **形状引用**：`AdapterStatus` 四字段与 `get_state` 的线上形状以 01a §1.1 / §3.1 为准，本文只引用不复述；若 01a 后续微调，按引用处对齐即可。
3. **「断线清理」语义**：指**上游 core 离线或移除**（按 core 前缀清理）；Panel↔后端 WS 断线（state.connected=false）**不**触发清理钩子，沿用现状（20s 超时 + connected 最高优先态由视图判断，pending.ts 头注释）。
4. **离线 reject 文案**：`'核心已离线，操作已取消'` 为本文拟定（03a 只钉了超时文案「操作超时，请重试」）；实现时可调，以 03a 渲染器测试为准。
5. **检测点**：core 离线转换的检测放在 `upstreams.ts` 轮询（8s 间隔，清理延迟 ≤ 一个轮询窗）；删除路径由 `removeSessionsOfCore` 扩展即时兜底（不等轮询）。
6. **迟到结果误配窗口**（§1-D1）：无线协议 id 可根治；当前按「接受 + 记录」处理，不引入生成器/时间戳启发式（避免误伤）。若 03a/03b 测试暴露可见问题，升级为协议修订项（需修订冻结项：00 §一）。
7. **`AdapterOpResult` 不入 `REQUEST_RESPONSES`**：防通用销账误伤同命令其他键；若实现发现需复用 pending 通用路径，必须补键粒度过滤后再加。
8. **行号基准**：panel 行号按工作树实测（`panel@4b7d99b` + 未提交的 ChatView/WorkspacePanel 改动，不影响本文引用行）；core 行号沿用 00/02E 证据包。
9. **test 基线口径**：`npm test` 56 文件/418 用例、`vue-tsc` 恰 1 条 `ChatView.vue:24 TS6133` 为 00 §六 + 工作树现状；若 S1 前基线因并行改动漂移，以「不新增失败/错误」为准（文内 418 为记录值）。
