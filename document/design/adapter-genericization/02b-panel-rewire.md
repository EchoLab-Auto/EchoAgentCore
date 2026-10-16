# 02b · Panel 阶段 1：QqPanel / QqLoginSection 数据面重接线

> 基线：panel @ 4b7d99b / echo-adapter-qq @ 79de9ec。配套：evidence/00、01 §E、02、03；03a §4、03b §1.4-1.5。
> 范围：仅改两个组件的**发送路径与读取路径**；UI 结构、样式、交互不变；组件删除归 03b S6。

## §0 前提与依赖（以 02a 为准）

本文**消费**以下 02a 产物，不重新设计（名称/签名以 02a 最终稿为准，若 02a 改名只需机械替换）：

| 02a 产物 | 本文假设的形态 |
|---|---|
| 命令类型 | `{ InvokeAdapterOp: { adapter, method, payload } }`（00 §一，command.rs:279-287） |
| 发送 + 配对助手 | `invokeAdapterOp(core, adapter, method, payload): Promise<{ result: unknown; error: string \| null }>`；内部走 `sendPending`，超时/断线 reject |
| pending 键 | `adapter-op:${scopedKey(core, adapter)}:${method}`（03a §4.2 建议；配对按 (core,adapter,method)，若 02a 选 call_id，组件层不变） |
| 状态归并 | `state.adapterUiByKey[scopedKey(core, adapter)]` = `AdapterState.state` 整文档替换（03a §4.1），形状 = 00 §三 |
| 旧 QQ 路径 | state.ts 的 `qqByAdapter` / `lastQrcode` 及其处理臂、pending.ts 6 条 QQ 映射**本阶段保留不动**（删除归 03b S6） |

已核实的 Core / 插件事实（影响组件设计）：

- **名单写入是全量替换**：插件 `update_allowlist/update_denylist` 直接 `*runtime_* = user_ids / group_ids`（echo-adapter-qq filter.rs:54-62, 67-70）；`parse_ids` 对**缺省键返回空表**（mod.rs:1469-1472）——载荷必须同时带 `user_ids` 与 `group_ids`，漏一个即清空该表。`parse_ids` 接受数字或字符串（mod.rs:1479-1485）。
- **读结果形状**（mod.rs:1286-1336）：`get_filter_config → {allowlist_users,…}`（**string[]**）；`get_owner → {owner_qq: number}`；`login_status → {running, online, user_id}`；`login_qrcode → {png_base64}`；`get_groups → {groups:[{id:string,name}]}`；`get_friends → {friends:[{id:string,name}]}`。
- **读类不刷新状态**：`InvokeAdapterOp` 臂仅对 4 个写方法调 `emit_adapter_state_events`（core packages/adapter_qq/commands.rs:134-149，01 §E）；`AdapterState` 另在 `RequestState` 时逐实例下发（agent/commands.rs:809-814）。
- **失败不走 AdapterOpResult.error**：`invoke_adapter_op` 出错时发的是 `BackendEvent::Error`（commands.rs:418-421），实例解析失败同样（:151-154）。按 (core,adapter,method) 配对时失败请求只能**超时**收尾。→ 提交 02a/01：建议 Core 改发 `AdapterOpResult{error}`（本文按「可能只超时」兜底设计，见 §2.3）。
- **路由**：`InvokeAdapterOp` 无 `team_id/session_id`，`resolveTargetCore` 回落 `currentRegion(state)`（connection.ts:309），与旧 QQ 命令一致；本文显式传 `state.activeRegion`，与读取键同源。

组件层新增一个**过渡模块**（03b S6 随组件一起删除）：`web/src/components/qq/qqView.ts`，含：
1. `readOverlay`：`reactive<Record<scopedKey, Overlay>>`，存读类结果（groups/friends/filter/owner/login），QqPanel 与 QqLoginSection 共享；
2. 纯函数 `toQqView(doc, overlay)`：把状态文档 + 覆盖层投影成现组件模板用的形状（`filter/gateMode/owner/groups/friends/online/selfId`），模板零改动。

覆盖层有效性规则：每条 overlay 记录到达时的 `docRef`（当时 `adapterUiByKey[key]` 的对象引用）；`toQqView` 仅在 `overlay.docRef === 当前 doc` 时采用 filter/owner/login 覆盖值——新 `AdapterState` 到达（整文档替换 ⇒ 引用变化）即自动让旧读结果失效，无需 watcher。groups/friends 不在状态文档中，始终取 overlay。

## §1 QqPanel.vue 重接线

### 1.1 状态读取 :53-57 → adapterUiByKey + 覆盖层

```ts
// 现 :53-57：state.qqByAdapter[scopedKey(state.activeRegion, name)] ?? defaultQqInstanceState()
const key = computed(() => scopedKey(state.activeRegion, activeAdapter.value?.name ?? 'qq'));
const instanceState = computed(() =>
  toQqView(state.adapterUiByKey[key.value] ?? null, readOverlay[key.value]));
```

`toQqView` 投影规则（doc = 00 §三状态文档；ov = 覆盖层；`num = (s) => Number(s)`）：

| 视图字段（模板用） | 来源（优先级从左到右） | 备注 |
|---|---|---|
| `filter.{allowlist,denylist}_{users,groups}` | ov.filter（docRef 匹配）→ `doc.gate.*` → `[]` | string[] → `.map(num)`，保持 number[]，模板 `includes(id)` / `:key` 不变 |
| `gateMode` | `doc.gate.mode` → `'none'` | 写后有 AdapterState 回推，不需覆盖层 |
| `owner` | ov.owner（docRef 匹配）→ `doc.owner` → `0` | |
| `groups / friends` | ov.groups / ov.friends → `[]` | `[{id,name}] → [num(id), name]`，与现 `Array<[number,string]>` 同形 |
| `online / selfId` | ov.login（docRef 匹配）→ `doc.online` / `doc.self_id` → `false` / `null` | QqLoginSection 共用，见 §2.1 |

- `defaultQqInstanceState()`（import :12）由 `toQqView(null, undefined)` 取代，import 删除；state.ts 中该函数保留（phase1-management-core.test.ts 仍用）。
- `:85 gateHint`、模板 :256-409 只读 `instanceState.value.*`，**零改动**。
- number 化是 W2.4 之前的过渡：W2.4（id → String）同批时把 `num` 换成恒等函数、`toggle*` 参数类型改 `string`，模板仍不变（01 定发布顺序）。

### 1.2 refreshLists :88-101 → InvokeAdapterOp 调用表

循环结构、空实例守卫（:92）、逐实例刷新全部保留；五条 `sendPending` 换成 `invokeAdapterOp(...).then(ok => 写覆盖层)`：

| 行 | 旧命令（旧 pending 键） | 新 method / payload | 结果写入（§3） |
|---|---|---|---|
| :97 | `RequestGroupList`（`qq:groups:<i>`） | `get_groups` / `{}` | `ov.groups ← result.groups` |
| :98 | `RequestFriendList`（`qq:friends:<i>`） | `get_friends` / `{}` | `ov.friends ← result.friends` |
| :99 | `RequestQqFilterConfig`（`qq:filter:<i>`） | `get_filter_config` / `{}` | `ov.filter ← result`（四数组） |
| :100 | `RequestQqOwner`（`qq:owner:<i>`） | `get_owner` / `{}` | `ov.owner ← result.owner_qq` |
| :101 | `RequestQqLoginStatus`（`qq:login:<i>`） | `login_status` / `{}` | `ov.login ← {online, user_id}` |

```ts
const core = state.activeRegion ?? null;           // 与读取键同源
for (const [method, apply] of READS)               // READS = 上表 5 行 [method, (ov, r) => void]
  invokeAdapterOp(core, adapter, method, {}).then(({ result, error }) => {
    if (!error) applyRead(scopedKey(core, adapter), apply, result);
  }).catch(() => {});                              // 超时/断线：由 LoadHint 阶段展示，不额外提示
```

`:94` 的 `key = adapter || null` 与 `pk()` 删除（`instanceNames` 恒非空字符串，null 分支不可达）。

### 1.3 pending 键 :106-110 → 新键式

```ts
const opKey = (m: string) => `adapter-op:${scopedKey(state.activeRegion, activeName.value || '_')}:${m}`;
const { phase: qqFilterPhase } = usePending(() => opKey('get_filter_config'));
// groups → 'get_groups'，friends → 'get_friends'，owner → 'get_owner'；:110-121 聚合逻辑不变
```

键格式由 02a 的 `invokeAdapterOp` 生成，组件只复用同一构造函数（建议 02a 导出 `adapterOpKey(core, adapter, method)`，此处直接调用，避免手拼漂移）。若 02a 选 call_id，`usePending` 改读 02a 提供的「(core,adapter,method) 最近一次调用阶段」视图，组件代码形态不变。

### 1.4 setGateMode :133-135

```ts
// 现：sendCommand({ SetQqGateMode: { mode, adapter: activeName.value } })
invokeAdapterOp(state.activeRegion ?? null, activeName.value, 'set_gate_mode', { mode: value as GateMode });
```

成功后 Core 广播 + `emit_adapter_state_events` → `AdapterState` 到达 → `gate.mode` 更新（commands.rs:146-149）。不写覆盖层。返回 Promise 不 await（现为 fire-and-forget，交互不变）。

### 1.5 toggle 四函数 :137-183 → update_allowlist / update_denylist

现语义确认为**全量名单**：每次发送「被改那张表的 next 全量 + 同侧另一张表的当前全量」（如 :140-146 `user_ids: next, group_ids: 当前 allowlist_groups`），与插件全量替换语义一致（§0）。新载荷**保持全量、两键必带**：

| 函数 | method | payload |
|---|---|---|
| `toggleAllowUser` :137 | `update_allowlist` | `{ user_ids: next, group_ids: f.allowlist_groups }` |
| `toggleDenyUser` :149 | `update_denylist` | `{ user_ids: next, group_ids: f.denylist_groups }` |
| `toggleAllowGroup` :161 | `update_allowlist` | `{ user_ids: f.allowlist_users, group_ids: next }` |
| `toggleDenyGroup` :173 | `update_denylist` | `{ user_ids: f.denylist_users, group_ids: next }` |

```ts
function toggleAllowUser(id: number) {
  const f = instanceState.value.filter, next = f.allowlist_users.includes(id) ? f.allowlist_users.filter(x => x !== id) : [...f.allowlist_users, id];
  invokeAdapterOp(state.activeRegion ?? null, activeName.value, 'update_allowlist', { user_ids: next, group_ids: f.allowlist_groups });
}
```

- 发 number[] 即可（`parse_ids` 兼收数字/字符串）；W2.4 后自然变 string[]。
- **快速连点竞态**（现状已有，不放大）：两次点击间若 `AdapterState` 未回推，第二次基于旧 `f` 计算会覆盖第一次。(core,adapter,method) 配对下 02a 若对同键串行化（03a §4.2），第二次排队但仍用旧快照。本阶段不修（交互不变约束）；03a list_editor 临时值机制解决。
- 写成功后 `AdapterState` 到达 ⇒ doc 引用变化 ⇒ 覆盖层 filter 失效，视图回到 `doc.gate.*`（§0 有效性规则），无需手动清。

### 1.6 不动的部分

:29-50 实例筛选（仍按 `persona`/`qq*` 名识别；platform 化归 03b）、:240/:248 Start/Stop/Restart（已中立）、:62-70 实例回退 watch、:123-131 连接后刷新、全部模板与样式。

## §2 QqLoginSection.vue 重接线

### 2.1 登录态读取 :30-36

```ts
const key = computed(() => scopedKey(state.activeRegion, instanceName.value));
const view = computed(() => toQqView(state.adapterUiByKey[key.value] ?? null, readOverlay[key.value]));
const online = computed(() => view.value.online ?? props.adapter?.connected ?? false);
const selfId = computed(() => view.value.selfId ?? props.adapter?.self_id ?? null);
```

为保住现有 `?? props.adapter?.connected` 回落链，`toQqView` 的 `online/selfId` 在「无 doc 且无 ov.login」时返回 `undefined`（QqPanel 不读这两项，不受影响）。

### 2.2 requestStatus :38-42 → login_status

```ts
if (!instanceName.value) return;                                   // :40 守卫保留
const core = state.activeRegion ?? null, name = instanceName.value;
invokeAdapterOp(core, name, 'login_status', {}).then(({ result, error }) => {
  if (!error) applyRead(scopedKey(core, name), (ov, r) => (ov.login = { online: r.online, user_id: r.user_id ?? null }), result);
}).catch(() => {});
```

现为 `sendCommand`（无 pending）；新助手会登记 `…:login_status` 键，与 QqPanel :101 同键——后发覆盖先发（`sendPending` 同 key 覆盖语义，connection.ts:221-229），无界面消费该阶段，无影响。`:83-84` 触发点不变。

### 2.3 loadQr :44-57 + qrDataUrl :24 + lastQrcode watch :67-81

过渡期方案（对齐 03a §4.3「结果局部归属」，但不引入渲染器）：

- **结果直接经 Promise 写本组件局部 `qrDataUrl`**，删除 :67-81 对 `state.lastQrcode` 的 watch。发起时捕获 `(core, name)`，回来时校验仍是当前实例且组件未卸载，否则丢弃——修掉单槽串号。
- `state.lastQrcode` 及 state.ts:1404-1409 处理臂**保留不删**（新路径不再产生 QqQrcode，成为死写；03b S6 删除，03b §1.5「若阶段 1 已先行改为……直接删除」即此情形）。

```ts
const core = state.activeRegion ?? null, name = instanceName.value;
invokeAdapterOp(core, name, 'login_qrcode', {}).then(({ result, error: err }) => {
  if (disposed || name !== instanceName.value || core !== (state.activeRegion ?? null)) return;
  loading.value = false; error.value = err ?? null;
  const b64 = (result as { png_base64?: string } | null)?.png_base64;
  qrDataUrl.value = !err && b64 ? `data:image/png;base64,${b64}` : null;
}).catch((e) => { if (!disposed) { loading.value = false; error.value = String(e?.message ?? '获取二维码超时'); } });
```

- `:46-48` loading/error/qrRequested 置位、`:52-56` 8s 计时器、`:60-65` 卸载清理**保留**（按钮加载态时长与现状一致）；`onUnmounted` 追加 `disposed = true`。
- 错误展示差异（已知，需 02a/01 跟进）：旧路径失败经 `QqQrcode{error}` 内联显示（core commands.rs:382）；新路径失败当前发 `BackendEvent::Error`（commands.rs:418-421）→ 全局 toast，内联区只在超时 reject 后出文案。若 Core 改为 `AdapterOpResult{error}`，上面 `err` 分支即恢复内联原文，组件无需再改。
- `:87-89`（online→清图）、`:98-106`（immediate 自动拉码单次闸）不变；`:101` 条件中的 `loading/qrRequested` 语义不变。

### 2.4 webuiUrl :108 与 deprecated 字段读取

00 §一：`AdapterStatus.container/webui_url/onebot_url` deprecated，**随 Panel 阶段 1 删除**，故本阶段必须停止读取：

| 位置 | 现状 | 目标 |
|---|---|---|
| QqLoginSection.vue:108-110 | `props.adapter?.webui_url ?? \`http://${hostname}:6099\`` | `doc?.instance?.webui_url ?? props.adapter?.webui_url ?? \`…:6099\``（中间项在协议删字段的同批去掉） |
| QqPanel.vue:229-231 | `adapter.container` | `docOf(adapter.name)?.instance?.container ?? adapter.container`；`docOf = n => state.adapterUiByKey[scopedKey(state.activeRegion, n)]` |

模板 DOM 结构/class 不变，仅绑定表达式换源（:229 的 `v-if` 与 `:title` 同步换）。`owner` prop（:19，来自 QqPanel :256 `instanceState.owner`）随 §1.1 自动换源。

## §3 读类结果数据流（AdapterOpResult.result → 组件用值）

```
refreshLists/requestStatus → invokeAdapterOp(core, adapter, get_*/login_status, {})
  → [02a] InvokeAdapterOp → Core invoke_custom → AdapterOpResult{adapter,method,result}（不触发 AdapterState，01 §E）
  → [02a] 按 (core,adapter,method) resolve Promise → 组件 .then → applyRead(key, apply, result)
  → readOverlay[key] = { ...apply 后字段, docRef: state.adapterUiByKey[key] } → toQqView → 模板
```

- **为何需要覆盖层**：读类结果只经 `AdapterOpResult.result` 返回，若只读状态文档，「刷新列表」对 filter/owner/login 无可见效果，groups/friends 则根本无处落地。
- **新鲜度**：写方法后 `AdapterState` 整文档替换 ⇒ `docRef` 失配 ⇒ 覆盖值自动让位给最新文档（§0）。读结果晚于 AdapterState 到达时以读结果为准，直到下一次 AdapterState——两者同源 cached_state/内存，偏差窗口仅一次往返。
- **结果校验**：`applyRead` 对 `result` 做形状守卫（非对象/缺键 → 不写，静默），数组元素 `Number()` 结果为 NaN 的丢弃；不抛异常，避免 Promise 链未处理拒绝。
- **生命周期**：`readOverlay` 为模块级 reactive；断线/核心移除时随 02a 清理 `adapterUiByKey` 的同一钩子按 core 前缀清空（若 02a 不暴露钩子，则在 QqPanel `watch(state.connected)` 为 false 时清空当前 core 条目）。
- groups/friends 的 `[{id,name}]` → `[number,string]` 转换只在 `toQqView`，覆盖层存原始形状，便于 03b 的 fetch 节点直接复用。

## §4 测试适配（QqPanel.test.ts 逐用例）

mock 改造（:14-22）：增加 `invokeAdapterOp: vi.fn(() => new Promise(() => {}))`（挂起，避免触发 .then 写覆盖层），并保留 `sendCommand/sendPending`（Start/Stop 仍用）。若 02a 的助手不在 `connection` 模块，mock 路径随之改。引入断言助手 `ops = () => mocked(invokeAdapterOp).mock.calls.map(([, a, m, p]) => ({ a, m, p }))`。`seedInstances`（:42）及各用例的 `state.qqByAdapter = {}` 改为 `state.adapterUiByKey = {}`，并在 `beforeEach` 清空 `readOverlay`。

| 用例（行） | 适配 |
|---|---|
| 单实例无选择器 :50 | 不变 |
| 多实例分段 + persona :56 | 不变 |
| 无实例零命令 :63 | 追加 `expect(invokeAdapterOp).not.toHaveBeenCalled()` |
| persona 过滤 :77 | 不变；:108 `container` 反向断言继续成立（sc-qq 被过滤，不渲染） |
| 当前智能体无实例 :113 | 同 :63 追加断言 |
| 未登录自动拉码 :137 | `'RequestQqQrcode' in c` → `ops().some(o => o.m === 'login_qrcode' && o.a === 'qq')` |
| 刷新只寻址本智能体 :167 | 过滤条件改 `o.m === 'get_groups' \|\| o.m === 'get_friends'`；`payload.adapter` → `o.a`；`sc-qq` 反向断言改查 `ops()` |
| 门控带 adapter :212 | 改为 `ops().find(o => o.m === 'set_gate_mode')`，断言 `a === 'qq'`、`p.mode === 'allowlist'`（第 2 个选项） |

新增用例：
1. 状态文档投影：置 `adapterUiByKey['qq'] = { gate:{mode:'allowlist', allowlist_users:['7'],…}, online:true, self_id:'1', owner:9 }` → 文本含 `7`、白名单徽标为 1、显示「QQ 已连接：1」与「QQ 管理员：9」。
2. toggle 全量载荷：点白名单好友 → `update_allowlist` 载荷同时含 `user_ids` 与 `group_ids`（防漏键清表）。
3. 读结果落覆盖层：`get_friends` resolve `{friends:[{id:'5',name:'n'}]}` → 好友 chip 出现 `n`；`get_filter_config` resolve 后替换 `adapterUiByKey['qq']` 引用 → filter 回到文档值。
4. 二维码局部归属（可放 `QqLoginSection.test.ts` 新文件）：resolve 前切换 `adapterName` → 结果丢弃、无 `img`；正常 resolve → `img.src` 以 `data:image/png;base64,` 开头。
5. 纯函数 `toQqView`（新文件 `components/qq/__tests__/qqView.test.ts`）：null 文档默认值、string→number、docRef 失配回落、无 doc 时 online 为 undefined。

不受影响：`__tests__/phase1-management-core.test.ts`、`pending.test.ts`、`connection-bootstrap.test.ts`（覆盖 state/pending 旧 QQ 臂，本阶段保留）。

## §5 切片与验收

门禁：02a 已合入 `InvokeAdapterOp/AdapterOpResult/AdapterState` 类型、`adapterUiByKey` 归并、`invokeAdapterOp` 助手与键构造函数。每片提交后在 `web/` 下 `npm test` + `npx vue-tsc -b` 全绿（基线 418 用例不减）。

| 片 | 内容 | 额外验收 |
|---|---|---|
| R1 | 新增 `components/qq/qqView.ts`（`readOverlay` / `applyRead` / `toQqView`）+ `qqView.test.ts` | 纯新增，组件未接入 |
| R2 | QqPanel 读取切换（§1.1、§2.4 container）**连同**读请求（§1.2 refreshLists、§1.3 pending 键）；QqLoginSection 登录态读取 + login_status（§2.1、§2.2、§2.4 webuiUrl） | 改 :63/:113/:167 用例 + 新增用例 1、3。读取与读请求必须同片：只切读取则 groups/friends 无来源（旧 GroupList 仍写 qqByAdapter）。filter/gateMode/owner 依赖 Core 下发 AdapterState（RequestState 时已逐实例下发，agent/commands.rs:809-814） |
| R3 | QqPanel 写路径（§1.4 setGateMode、§1.5 toggle×4） | 改 :212 用例 + 新增用例 2 |
| R4 | QqLoginSection 二维码（§2.3：login_qrcode + 局部化，删 lastQrcode watch） | 改 :137 用例 + 新增用例 4 |

收尾检查（R4 后）：`grep -n "qqByAdapter\|lastQrcode\|sendCommand({ \(Request\|Set\|Update\)Qq\|RequestGroupList\|RequestFriendList" web/src/components/Qq*.vue` 应为 0（state.ts / pending.ts / protocol.ts 中的旧 QQ 段按 §0 保留，03b S6 删除）。

手工冒烟（需真实 Core + QQ 实例，本文未执行）：打开面板 → 名单/好友/群加载；切门控与点名单 → 状态回推；多实例切换下二维码不串号；断开 Core → 加载态 20s 后出重试。

回滚：每片独立 `git revert`，按 R4→R2 逆序回退；R3 单独回退亦安全（写命令新旧在 Core 侧等价，双跑测试已覆盖）。

待 02a/01 确认的假设：A1 助手签名与键格式（§0）；A2 Core 失败改发 `AdapterOpResult{error}`（§0、§2.3）；A3 断线时 `readOverlay` 清理钩子（§3）。
