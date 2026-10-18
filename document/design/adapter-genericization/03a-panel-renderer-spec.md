---
id: gen-03a
title: "03a · UI 描述符渲染器规范"
group: 规划
x: 1600
y: 3480
---

# 03a · Panel 适配器 UI 描述符通用渲染器规范（阶段 2）

> 状态：已实施（渲染器落地于 `web/src/components/adapter-ui/`）。依据：`evidence/00-frozen-interfaces.md`（下称 00）、`evidence/03-evidence-panel-ui.md`（下称 03E）。
> 仓库基线：panel @ 4b7d99b / core @ 3541b46。本文只规定 Panel 侧渲染器，不修改任何冻结项。
>
> **前置假设（撰写时 01 / 02 文档尚未落盘，以下按合理假设推进，待其定稿后对齐）**
> - **A1 描述符载体**（00 §一 缺口，01 决策）：渲染器只消费「已解析好的描述符对象」，不关心它来自
>   `AdapterStatus.ui`、`AdapterState.state` 内嵌字段还是独立事件。取用点集中在一个 selector（§4.1），载体定稿后只改 selector。
> - **A2 配对**（00 §一 ⚠️，02 决策）：渲染器通过一个 `invokeAdapterOp(core, adapter, method, payload)` 服务函数发出命令，
>   返回 Promise<AdapterOpResult>。函数内部如何配对（`(core,adapter,method)` 键或 `call_id`）完全由 02 文档的实现承担，
>   本文只依赖其语义：**同键并发时后发覆盖或排队由 02 定**，渲染器层以「同一 method 同时只允许一个在途」自我约束（§2.0）。
> - **A3 状态文档**：以 00 §三 为 bind 路径唯一标准；id 一律字符串（W2.4 落地后）。W2.4 未落地前 `owner` 可能为 number，渲染器统一 `String()` 展示。

---

## 1. 渲染器组件规格

### 1.1 目录与文件划分

建议放在 `web/src/components/adapter-ui/`，每个文件可独立提交：

| 文件 | 职责 |
|---|---|
| `types.ts` | 描述符 TS 类型：`UiDescriptor { version; panels: UiPanel[] }`、`UiPanel { id; title?; nodes: UiNode[] }`、`UiNode` 判别联合（7 种 + `UnknownNode`）、`UiAction { method; arg? }`、`UiFetch { method; result_key }` |
| `parse.ts` | 纯函数：`parseDescriptor(raw: unknown): ParsedDescriptor`——版本分支、节点白名单过滤、字段校验，产出 `{ panels, warnings[] }`；**不抛异常** |
| `bind.ts` | 纯函数：`resolveBind(state, path)`、`buildPayload(action, value)`（`$value` 替换） |
| `AdapterUiRenderer.vue` | 顶层：接收 descriptor/state/meta，遍历 panels → nodes，按 type 分发到节点组件 |
| `AdapterUiPanel.vue` | 单个 panel：`NeumorphismCard` 包裹 + 标题 + 节点列表 |
| `nodes/StatusNode.vue` … `nodes/ButtonNode.vue` | 七个节点组件，一类一文件 |
| `useAdapterOp.ts` | composable：封装在途/错误态（per node key），调用 A2 的 `invokeAdapterOp` |
| `__tests__/*.test.ts` | §5 测试 |

视觉组件全部复用 `web/vendor/ui-frame`（03E §B：Button / Select / Input / Card / List / StatusDot / Empty / Alert / Badge），渲染器不引入新 UI 库。

外层「通用适配器面板」（多实例切换 / 状态行 / 启停重启，00 §五 目标 2）**不属于本渲染器**，由 Panel 内置的 `AdapterPanel.vue`（03b 负责迁移）承载，渲染器作为其子组件嵌入。

### 1.2 Props

`AdapterUiRenderer.vue`：

| prop | 类型 | 说明 |
|---|---|---|
| `descriptor` | `unknown` | 原始描述符（未解析）；内部 `computed(() => parseDescriptor(props.descriptor))`。为 `null/undefined` 时渲染空（不显示任何 panel，也不报错） |
| `state` | `Record<string, unknown> \| null` | 状态文档，取自 `adapterUiByKey[scopedKey(core, adapter)]`（00 §五 目标 1）。`null` 表示尚未收到 `AdapterState` |
| `meta` | `{ core: string; adapter: string; displayName?: string; running: boolean; connected: boolean }` | 适配器元信息，来自 `AdapterStatus`（00 §一）；`core+adapter` 决定命令路由和 pending 键 |
| `disabled` | `boolean`（可选，默认 `false`） | 外层强制禁用（如核心断线、权限不足） |

不接收回调 prop；所有副作用通过 emits 或 `useAdapterOp` 完成。

### 1.3 Emits

| 事件 | 载荷 | 用途 |
|---|---|---|
| `op` | `{ method: string; payload: unknown; nodeKey: string }` | 每次发出 action/fetch 前触发（观测、埋点、测试断言用）；**不影响发送** |
| `op-result` | `{ method; ok: boolean; result?: unknown; error?: string; nodeKey }` | `AdapterOpResult` 回来后触发 |
| `warning` | `{ code: 'unknown_node' \| 'unknown_version' \| 'bad_field' \| 'unknown_action'; detail: string }` | parse 或运行期诊断；外层可在 devtools / 调试面板显示，生产 UI 不弹窗 |

设计取舍：命令由渲染器内部经 `useAdapterOp` 直接发（而不是 emit 给父组件代发），这样渲染器在任何挂载点都能独立工作；`op` emit 只是旁路通知。测试中 mock `invokeAdapterOp` 即可断言载荷。

### 1.4 节点 key

每个节点的稳定 key：`${panel.id}/${index}/${node.type}`。用于 v-for key、在途态表、错误态表。描述符变化（插件升级）后 index 变化会重置对应节点的局部状态，可接受。

### 1.5 bind 路径解析规则（`bind.ts`）

- 语法：点分路径，段为 `[A-Za-z0-9_]+`。`"gate.mode"` → `state.gate.mode`；`"online"` → `state.online`；`"instance.webui_url"` → `state.instance.webui_url`。
- 不支持数组下标、通配、表达式（v1 不需要；需要时随 version 2 扩展）。
- 解析：逐段下钻，遇 `null/undefined/非对象` 立即返回 `undefined`；**从不抛异常**。
- 段名为 `__proto__` / `constructor` / `prototype` 时直接返回 `undefined`（防原型链读取）。
- 路径非法（空串、含非法字符）→ parse 期产出 `bad_field` warning，该节点按「无 bind」处理。
- 结果类型由节点自己校验（如 select 要求 string，list_editor 要求 array），类型不符视为「值缺失」，不视为错误。
- 合法路径清单以 00 §三 为准：`online / running / self_id / owner / instance.* / gate.mode / gate.{allow,deny}list_{users,groups}`。路径不在清单内仍允许（其它插件有自己的文档字段），渲染器不做白名单。

### 1.6 `$value` 替换规则（`buildPayload`）

`action: { method, arg? }` → `InvokeAdapterOp.payload`：

| `arg` 形态 | payload |
|---|---|
| 缺省 | 节点默认载荷（各节点在 §2 定义；如 button 为 `{}`，list_editor 为 `{ [字段]: 新列表 }`） |
| 字符串 `"$value"` | 节点的当前值整体，**若不是对象则包为节点约定字段**（见下） |
| 对象 | 深拷贝后递归扫描，值严格等于 `"$value"` 的叶子替换为当前值；其它值原样保留 |
| 其他标量 | 原样作为 payload（不推荐，发 warning `bad_field`） |

「包为约定字段」：线协议 payload 需为对象（00 §一 缺省 `{}`），而 00 §四 方法均为具名参数（`set_gate_mode{mode}`）。v1 规定：`arg:"$value"` 且当前值为标量时，payload = `{ value: <当前值> }`，**同时**若节点声明了可选字段 `arg_key`（假设：v1 扩展字段，未冻结），则用 `{ [arg_key]: 当前值 }`。
> ⚠️ 与 00 冲突点提示：03E §A 示例 `{"method":"set_gate_mode","arg":"$value"}` 对应插件方法期望 `{mode}`。推荐插件 schema 写成对象形态 `"arg": {"mode": "$value"}`（无需任何扩展字段，完全落在 v1 冻结语义内）；01 文档补全 QQ 描述符时应采用对象形态。`arg_key` 仅为兜底，若 01 不采纳则删除此条。

替换只发生在叶子字符串完全等于 `"$value"` 时；`"prefix $value"` 不做插值。

---

## 2. 七种节点类型语义

### 2.0 共性约定

- **通用字段**：`type`（必填）、`label?`（缺省时用 bind 末段或空）、`bind?`、`action?`、`fetch?`、`hint?`（假设：可选说明文字，未知时忽略，不影响 v1 兼容）。
- **在途态**：`useAdapterOp` 按 nodeKey 维护 `pending: boolean`。在途时该节点控件 `disabled` + loading 指示；**同一 nodeKey 不允许并发**（A2 自我约束），用户再次触发被忽略。
- **错误态**：`AdapterOpResult.error` 非空、或 02 约定的超时到期 → 节点下方 `NeumorphismAlert`（type=error，单行，可关闭）显示错误文本；下次成功操作自动清除。错误不回滚 UI 以外的任何东西（UI 本身是状态文档投影，见下条）。
- **单向数据流**：可编辑节点（select / list_editor）的显示值**永远来自 state**；用户操作只发命令，不本地乐观修改。命令成功后由插件 `report_ui_state` → `AdapterState` 推新状态，界面随之刷新。在途期间控件显示「用户刚选的值」作为临时值，结果回来（成功或失败）后丢弃临时值、回到 state 投影。
  > 依据：现状 QqPanel 也依赖事件回推刷新名单（03E §B pending `qq:filter:<inst>`）。若某插件成功后不 report 状态，UI 会回弹——这是插件 bug，渲染器不兜底。
- **禁用态（通用）**：`props.disabled || !meta.running` 时所有带 action 的节点禁用；只读节点（status/text/link/image 的展示部分）照常显示。`meta.connected=false` 不禁用（离线时仍应能改门控、拉二维码）。
- **state 为 null**：只读节点显示占位「—」；可编辑节点禁用并显示 skeleton。

### 2.1 `status`

| 项 | 规定 |
|---|---|
| 数据来源 | `resolveBind(state, bind)` |
| 渲染 | `NeumorphismStatusDot` + label + 状态文字 |
| 值映射 | `true` → 绿点「在线/是」；`false` → 灰点「离线/否」；字符串 → 原样显示，点为中性色；`undefined` → 灰点「未知」 |
| 可选字段 | `labels?: { true?: string; false?: string }`（假设扩展；覆盖默认文字） |
| 交互 / action | 无。status 不定义 action；声明了则忽略并发 `bad_field` warning |
| 加载/错误/禁用 | 无请求，无加载态；state 为 null 时「未知」 |

### 2.2 `text`

| 项 | 规定 |
|---|---|
| 数据来源 | bind 值，`String()` 后显示；数组 `join(', ')`；对象显示 `JSON.stringify` 截断 80 字符 |
| 只读模式 | 无 action 时为纯展示（如 `self_id`），旁边提供复制按钮 |
| 可编辑模式 | 有 action 时为 `NeumorphismInput` + 「保存」按钮（用于 owner 设置，03E §C parity「owner 设置：text/input + action」） |
| action 构造 | 当前值 = 输入框字符串（trim 后）；按 §1.6 构造。QQ 推荐 schema：`{ "method": "set_owner", "arg": { "owner_qq": "$value" } }` |
| 校验 | 空串不允许提交（按钮禁用）；更多校验 v1 不做（插件端校验后返回 error） |
| 加载/错误/禁用 | 保存在途：输入框和按钮禁用 + 按钮 loading；错误显示在输入框下方；输入框初值随 state 变化同步，但用户正在编辑（脏）时不覆盖 |

> W2.4 提示：`set_owner{owner_qq}` 现为 number（00 §一）。W2.4 前插件需自行接受字符串，或 01 在描述符中注明；渲染器始终发字符串。

### 2.3 `link`

| 项 | 规定 |
|---|---|
| 数据来源 | bind 值（如 `instance.webui_url`，03E 示例写作 `webui_url`——对应状态文档实际路径为 `instance.webui_url`，01 写 QQ 描述符时以 00 §三 为准） |
| 渲染 | `<a target="_blank" rel="noopener noreferrer">`，文字为 label；值为空时不渲染该节点（或显示禁用文字「不可用」） |
| 安全 | 只允许 `http:` / `https:` 协议；其它（`javascript:`、`data:` 等）不渲染链接，发 `bad_field` warning |
| action | 不定义；声明了则忽略 + warning |
| 加载/错误/禁用 | 无 |

### 2.4 `select`

| 项 | 规定 |
|---|---|
| 数据来源 | 当前值 = bind 值（string）；选项 = `options` |
| options 形态 | `string[]`（值即标签），或 `{ value: string; label?: string; hint?: string }[]`（假设扩展，用于还原 GATE_HINTS，见 §6）。非法项过滤 |
| 渲染 | `NeumorphismSelect`；当前值不在 options 内时显示原值并标「未知选项」 |
| 交互 | change → 当前值 = 选中 value → 按 §1.6 构造 payload → 发送。选中值等于 state 现值时不发 |
| action 构造 | QQ 推荐：`{ "method": "set_gate_mode", "arg": { "mode": "$value" } }` → payload `{ "mode": "allowlist" }` |
| 无 action | 只读下拉（disabled） |
| 加载/错误/禁用 | 在途：Select 禁用并显示临时值；失败：回到 state 值 + 错误提示 |

### 2.5 `list_editor`

| 项 | 规定 |
|---|---|
| 数据来源 | bind 值（`string[]`；非数组视为 `[]`；元素统一 `String()`） |
| 渲染 | `NeumorphismCard` + label + 条目列表（`NeumorphismList`，每行带移除按钮）+ 新增输入框和「添加」按钮；空列表 `NeumorphismEmpty` |
| 交互 | 添加：trim、非空、去重（已存在则提示不发送）；移除：点行尾按钮。每次增/删立即发送**整表**（新列表），不做批量暂存 |
| action 构造 | 当前值 = 新的完整列表。`arg` 缺省时 payload = `{ value: 新列表 }`；**推荐插件显式写对象形态** |
| 与 00 §四 的差异 | `update_allowlist{user_ids, group_ids}` 一次同时带用户和群两表，而一个 list_editor 只管一表。处理口径：QQ 描述符对每卡写 `arg: { "user_ids": "$value", "group_ids": "$bind:gate.allowlist_groups" }` 需要引用另一 bind——**v1 无此语义**。推荐由 01 让插件接受「只给出的字段才更新」的部分载荷（如只发 `{user_ids:[...]}` 不动群表）。本文按此假设，**不在 v1 引入 `$bind:` 引用**；若 01 不接受，作为 version 2 候选（记为开放问题 Q1） |
| 加载/错误/禁用 | 在途：整卡输入与移除按钮禁用，列表显示临时新列表（半透明）；失败：回到 state 列表 + 错误提示 |

### 2.6 `image`

| 项 | 规定 |
|---|---|
| 数据来源 | 不走 bind；走 `fetch: { method, result_key }`：调 `invokeAdapterOp(core, adapter, fetch.method, {})`，从 `result[result_key]` 取 base64 PNG |
| 渲染 | `<img :src="'data:image/png;base64,' + b64" :alt="label">`；alt 必填（无 label 时用「适配器图片」），满足无障碍 |
| 触发时机 | **不自动拉取**（二维码会触发插件侧重新生成，有副作用）；显示「获取」按钮，用户点击后拉取；已有图时按钮变「刷新」。详见 §4.3 |
| 校验 | `result[result_key]` 非字符串或空 → 视为错误「未返回图片」；只拼接 `image/png`，不接受插件提供 MIME（避免 `data:` 注入其它类型） |
| 加载/错误/禁用 | 拉取中：按钮 loading + 占位框；失败：占位框内显示错误 + 重试按钮；禁用规则同 §2.0（`!running` 禁用） |
| bind 共存 | 若同时声明 `bind`，v1 忽略 bind（只用 fetch），发 `bad_field` warning |

### 2.7 `button`

| 项 | 规定 |
|---|---|
| 数据来源 | 无（label 为按钮文字） |
| 交互 | 点击 → 发送 action |
| action 构造 | `arg` 缺省 → payload `{}`；`arg` 为对象 → 原样（无 `$value` 可替换；出现 `"$value"` 叶子替换为 `null` 并 warning） |
| 可选字段 | `confirm?: string`（假设扩展；有值时先弹确认框，用于危险操作） |
| 无 action | 不渲染（无意义按钮），`bad_field` warning |
| 加载/错误/禁用 | 在途：按钮 loading + 禁用；错误：按钮下方提示；成功：无额外提示（状态由 AdapterState 回推体现）。若 `result` 非空且为字符串，以 `NeumorphismBadge`/toast 短暂显示（3s） |

> 关于 03E 示例中的「刷新」按钮（`method: refresh`）：00 §四 管理方法清单无 `refresh`。按 §3.3 未知 action 口径处理——渲染器照发，由插件返回错误；01 补全描述符时应改用实际存在的方法（如 `instance_info` / `get_filter_config`）或删除该按钮，外层通用面板已有刷新入口。

---

## 3. 版本与兼容

### 3.1 version 分支

`parseDescriptor` 第一步读 `raw.version`：

| version | 处理 |
|---|---|
| `1` | 走 v1 解析器（本文 §1–§2） |
| 缺省 / 非数字 | 按 `1` 处理，发 `unknown_version` warning（宽容：早期插件可能漏写） |
| `> 1`（未来版本） | **按 v1 尽力解析**：v1 认识的节点照常渲染，不认识的忽略；面板顶部显示一行 `NeumorphismAlert`(info)「该适配器的界面描述版本较新（vN），部分控件可能未显示，请升级 Panel」 |
| `< 1` / 负数 | 视为非法描述符：不渲染任何 panel，显示「界面描述无效」空态 + `unknown_version` warning |

实现上用 `switch(version)` 分派到 `parseV1`，留出 `parseV2` 位置；v2 解析器应向下兼容 v1 节点（v2 是 v1 超集，是对插件作者的约定，记入 adapter-plugin.md 时由 01 补充）。

### 3.2 结构容错

- `panels` 非数组 → 视为 `[]`。panel 缺 `id` → 用 `panel-${index}`；`id` 重复 → 后者加后缀 `-${index}`。
- `nodes` 非数组 → 该 panel 显示 `NeumorphismEmpty`。
- **未知节点类型**（`type` 不在七种之内，或缺 `type`）→ **静默跳过**，不占位、不报错弹窗，只发一次 `unknown_node` warning（同一 descriptor 同 type 去重）。这是 00 §二 冻结语义。
- 已知节点上的**未知字段**一律忽略（向前兼容）。
- 已知字段类型错误（如 `options` 非数组、`fetch.result_key` 非字符串）→ 该字段视为缺省；若导致节点无意义（button 无 action、image 无 fetch）→ 跳过该节点 + `bad_field` warning。
- 解析结果 memo 化（`computed` 依赖 descriptor 引用）；descriptor 引用不变时不重复解析和发 warning。

### 3.3 未知 action 的处理口径

「未知 action」= `action.method` / `fetch.method` 不在 Panel 已知的任何清单中。口径：

1. **渲染器不维护 method 白名单**。方法集合属于插件（00 §四 只是 QQ 的清单），渲染器对任意非空字符串 method 照常发送 `InvokeAdapterOp`。
2. 合法性判断交给 Core/插件：插件不认识时返回 `AdapterOpResult.error`，渲染器按 §2.0 错误态显示。
3. 渲染器只做**形状校验**：`method` 必须是非空字符串且匹配 `^[A-Za-z0-9_.:-]{1,64}$`（假设的保守字符集）；不合法 → 节点按「无 action」处理 + `unknown_action` warning。
4. 安全：`InvokeAdapterOp` 的 clearance 为 Frontend-only 类别（00 §一 command.rs:512-530），权限由 Core 把关；渲染器不因描述符来自插件就提升或绕过任何权限。
5. 节点类型不定义 action 却声明了（status/link/image）→ 忽略 + `bad_field`（见 §2）。

---

## 4. 接线

### 4.1 状态流入

```
插件 report_ui_state → Core 缓存(cached_state) → BackendEvent::AdapterState{adapter,state}
  → Panel 事件归并：adapterUiByKey[scopedKey(core, adapter)] = state   （00 §五 目标 1）
  → selector → <AdapterUiRenderer :state>
```

- 归并逻辑沿用现 `qqByAdapter` 在 state.ts 的 scopedKey 归并（00 §五），只是换键名和取消 QQ 专属字段拆解；**整文档替换**而非字段合并（AdapterState 每次携带完整 `cached_state`，00 §一）。
- selector（建议放 `web/src/state` 对应 domain，名称如 `selectAdapterUi(core, adapter)`）返回 `{ state, descriptor, meta }`：
  - `state` ← `adapterUiByKey[key] ?? null`
  - `descriptor` ← 按 A1 的载体取（未定前：先读 `AdapterStatus.ui`，再读 `state.ui`；都没有返回 `null`）
  - `meta` ← `state.adapters` 中对应 `AdapterStatus`（core, name, display_name, running, connected）
- 断线/核心移除时：清空该 core 的全部 `adapterUiByKey` 条目（与现有 scopedKey 清理一致），渲染器收到 `state=null` 进入 §2.0 的 null 态。
- 渲染器内部不缓存 state 副本；节点局部状态（临时值、输入框脏值、二维码图）见 §4.3。

### 4.2 命令流出

```
节点交互 → buildPayload → useAdapterOp.run(nodeKey, method, payload)
  → emit('op') → invokeAdapterOp(meta.core, meta.adapter, method, payload)
  → BackendCommand::InvokeAdapterOp{adapter, method, payload}  （00 §一 command.rs:279-287）
  → 等待 BackendEvent::AdapterOpResult{adapter, method, result?, error?}
  → emit('op-result') → 更新节点 pending/error
```

- **路由**：命令必须发往 `meta.core`（显式目标 core），不依赖 `resolveTargetCore` 的默认推断（connection.ts:305+），避免多 core 下误投。
- **配对与等待**：完全复用 02 文档的约定（A2）——包括配对键、超时时长、超时后的错误文案、断线时 pending 的清理。渲染器只消费 Promise 的 resolve（`{result, error}`）/ reject（超时、断线），**不在本文重新设计**。
- **pending 键**：交给 `sendPending` 的键建议为 `adapter-op:${scopedKey(core,adapter)}:${method}`，与现模式 `qq:<kind>:<instance>`（QqPanel.vue:106-110）同构；若 02 选择 `call_id` 方案则改为由服务层生成，渲染器无感。
- **渲染器层并发约束**：同 nodeKey 不并发（§2.0）。不同节点调同一 method（如两张卡都用 `update_allowlist`）在 `(core,adapter,method)` 配对方案下会冲突——渲染器在 `useAdapterOp` 中按 `${core}/${adapter}/${method}` **额外串行化**：同 method 的第二个请求排队等第一个结束。`call_id` 方案下此串行化可保留（无害）。
- **结果处理**：`error` 非空 → 失败；否则成功。`result` 只有 image(fetch) 和 button(字符串提示) 使用；select/list_editor/text 成功后只清临时值，等 AdapterState 回推。
  > 若 AdapterState 在 AdapterOpResult 之前到达（顺序不保证），临时值仍在显示到结果回来为止，随后切到已更新的 state——无闪烁。

### 4.3 二维码 image 的 fetch 刷新语义

现状问题：QqLoginSection 的二维码结果放在全局 `state.lastQrcode` 单槽（03E §B，02 证据 §A.2），多实例同时拉取时会互相覆盖、显示到错误实例上。渲染器规定：

1. **结果归属**：fetch 结果**不进全局 state**，只存在 image 节点组件局部（`ref<string|null>`），键为 `scopedKey(core, adapter) + nodeKey`。结果按 4.2 的配对机制送回发起方 Promise，天然归属到发起的 (core, adapter)；不同实例 = 不同 renderer 实例 = 不同局部槽，无交叉。
2. **实例切换**：外层通用面板切换实例时，renderer 以 `:key="scopedKey(core,adapter)"` 重新挂载，旧图丢弃；旧实例在途的 fetch 结果回来后因组件已卸载被丢弃（`useAdapterOp` 在 `onUnmounted` 标记作废，不写入已卸载组件）。
3. **触发**：用户点击「获取/刷新」才 fetch（§2.6）。不轮询（二维码过期由用户再点刷新；v1 不做定时刷新，记为开放问题 Q2）。
4. **自动失效**：
   - 绑定登录态的清理：若同一 panel 内有 `status` 节点 bind 到 `online`，当该值从 `false` 变为 `true`（登录成功）→ 清除本 panel 所有 image 节点的图，显示「已登录」占位。
     > 这是对 QQ 场景的启发式（需要知道哪个 status 代表登录）。v1 规则精确化为：**image 节点可选字段 `clear_when?: string`（bind 路径），该路径值为真时清图**。未声明则不自动清。QQ 描述符推荐 `"clear_when": "online"`。（假设扩展，需 01 确认）
   - `meta.running` 变为 false → 清图。
5. **并发**：同一 image 节点刷新在途时再次点击被忽略；不同实例互不影响（不同 core/adapter 键）。

### 4.4 挂载点

- 外层 `AdapterPanel.vue`（03b 负责，替代 QqPanel 的头部/实例选择器/状态行，03E §B）在实例选定后渲染 `<AdapterUiRenderer :key :descriptor :state :meta>`。
- 无 descriptor 的适配器（如未提供 `ui` 的插件）：外层面板照常显示通用部分，渲染器区域不显示。
- ChatView 原 QqPanel 挂载点（ChatView.vue:40,839）的替换由 03b 负责。

---

## 5. 测试规范

框架：vitest + @vue/test-utils（与现基线一致，00 §六：56 文件 / 418 用例全绿，新增后不得回退）。
mock 边界：只 mock `invokeAdapterOp`（A2 服务函数），返回可控 Promise；不 mock 节点组件和 ui-frame。
文件：`web/src/components/adapter-ui/__tests__/{parse,bind,renderer,nodes}.test.ts`。

### 5.1 纯函数（parse.test.ts / bind.test.ts）

| 用例 | 断言 |
|---|---|
| bind 基本 | `resolveBind({gate:{mode:'allowlist'}}, 'gate.mode') === 'allowlist'` |
| bind 缺段 | 中间段为 null/标量/缺失 → `undefined`，不抛 |
| bind 原型防护 | `'__proto__.x'`、`'constructor'` → `undefined` |
| bind 非法路径 | `''`、`'a..b'`、`'a[0]'` → parse 发 `bad_field` |
| `$value` 字符串 | `arg:"$value"` + 标量 → `{value: v}`；+ 数组 → `{value: [...]}` |
| `$value` 对象 | `arg:{mode:"$value", keep:1}` → `{mode:v, keep:1}`；嵌套对象深替换；原 arg 未被修改（深拷贝） |
| `$value` 不插值 | `"x $value"` 原样保留 |
| version | `1` 正常；缺省按 1 + warning；`2` 尽力解析 + 版本提示标志；`0`/`-1` 无 panel |
| 结构容错 | panels 非数组、panel 缺 id/重复 id、nodes 非数组均不抛 |

### 5.2 未知节点 / 未知 action（renderer.test.ts）

| 用例 | 断言 |
|---|---|
| 未知节点忽略 | descriptor 含 `{type:'slider'}` 与一个 `text`：只渲染 text；DOM 无占位元素；`warning` emit 一次 `unknown_node`（同 type 两个也只一次） |
| 缺 type 节点 | 同上忽略 |
| 已知节点未知字段 | `{type:'text', bind:'self_id', foo:1}` 正常渲染 |
| 未知 method 照发 | button `{method:'no_such_op'}` 点击 → `invokeAdapterOp` 被调用；mock 返回 `{error:'unknown method'}` → 按钮下显示错误 |
| 非法 method 形状 | `method:''` / `'a b'` → 按钮不渲染 + `unknown_action` warning；`invokeAdapterOp` 未调用 |
| 无 descriptor | `descriptor=null` → 渲染空，无 warning |
| state=null | 只读节点显示「—」，可编辑节点禁用 |

### 5.3 节点逐类（nodes.test.ts）

每类至少覆盖：渲染值、交互载荷、在途态、错误态、禁用态（`meta.running=false`）。

| 节点 | 关键用例 |
|---|---|
| status | true/false/字符串/undefined 四种映射；带 action 不发命令 |
| text | 只读显示 `self_id`；number owner 显示为字符串；可编辑：输入 `"10001"` 保存 → 载荷 `{owner_qq:"10001"}`（schema `arg:{owner_qq:"$value"}`）；空串保存按钮禁用；脏输入不被 state 更新覆盖 |
| link | https 渲染 `<a rel="noopener noreferrer" target="_blank">`；`javascript:` 不渲染 + warning；空值不渲染 |
| select | 显示 state 值；选 `denylist` → 载荷 **严格等于** `{mode:"denylist"}`、method `set_gate_mode`、adapter/core 为 meta 值；选当前值不发；在途禁用并显示临时值；失败回弹到 state 值 |
| list_editor | 添加 `"42"` → 载荷 `{user_ids:[...旧, "42"]}`；重复添加不发；trim 空不发；移除 → 载荷少一项；非数组 bind 视为空并显示 Empty；在途整卡禁用 |
| image | 初始不自动 fetch（mock 未被调用）；点击 → 调 `login_qrcode` 载荷 `{}`；`result.png_base64` → `img.src` 以 `data:image/png;base64,` 开头且 alt 非空；result_key 缺失 → 错误态；`clear_when:'online'` 由 false→true 后图被清除 |
| button | 载荷 `{}`；`arg` 对象原样；在途重复点击只调用一次；`confirm` 取消时不发 |

### 5.4 接线与并发（renderer.test.ts）

| 用例 | 断言 |
|---|---|
| 路由 | 所有调用的 `core`/`adapter` 参数等于 `meta`，而非默认 core |
| 同 method 串行 | 两个 list_editor 都用 `update_allowlist`，连续触发：第二次调用发生在第一次 resolve 之后 |
| 多实例二维码隔离 | 挂两个 renderer（同 adapter 名、不同 core 或不同 adapter）各自 fetch，mock 交错 resolve：各自 img 只显示自己的结果 |
| 卸载丢弃 | fetch 在途时卸载组件，随后 resolve 不报错、不写入 |
| state 回推 | 更新 `state` prop（模拟 AdapterState）→ select/list 显示新值，临时值在结果回来后清除 |
| emits | 每次发送前 emit `op`（method/payload 与调用参数一致），结果后 emit `op-result` |

### 5.5 QQ 描述符快照（集成）

以 01 定稿的 QQ 描述符 + 00 §三 样例状态文档为 fixture，渲染整棵树做结构快照，并逐项断言 §6 parity 表中的元素存在。01 未定稿前使用本文 §6 的推荐描述符写法作临时 fixture。

---

## 6. Parity 核对

对照 03E §C（来源 adapter-plugin.md:326-333），逐项说明渲染器如何覆盖：

| 现功能（现位置） | 目标归属 | 渲染器覆盖方式 | 结论 |
|---|---|---|---|
| 多实例切换 / 状态行 / 启停重启（QqPanel.vue:205-257） | Panel 内置通用面板 | **不由渲染器承担**；外层 AdapterPanel（03b）提供，渲染器以 `:key=scopedKey` 跟随实例切换（§4.3-2） | 范围外，接口已对齐 |
| 门控模式选择（QqPanel.vue:258-270，GATE_OPTIONS :72-76） | select + action | §2.4：`{type:select, bind:"gate.mode", options:[…], action:{method:"set_gate_mode", arg:{mode:"$value"}}}` | 覆盖 |
| 门控说明文字（GATE_HINTS :78-82） | 同上 | 依赖 options 对象形态的 `hint`（§2.4，假设扩展）；若 01 不采纳，则降级为每个 select 节点的 `hint` 文字 | **部分覆盖**，需 01 确认 |
| 白/黑名单四卡片（QqPanel.vue:271-416，toggle :137-183） | list_editor ×4 | §2.5：四个节点分别 bind `gate.allowlist_users / allowlist_groups / denylist_users / denylist_groups`，action `update_allowlist` / `update_denylist` | 覆盖，**前提 Q1**：插件接受部分载荷（只带 `user_ids` 或 `group_ids`） |
| 名单从好友/群列表勾选（toggle 依赖 get_groups/get_friends） | — | v1 list_editor 只支持手输 id；从列表选择需要「候选数据源」语义，v1 未冻结 | **缺口 Q3**，见下 |
| 登录状态（QqLoginSection.vue:38） | status | §2.1 bind `online` | 覆盖 |
| 二维码 PNG（QqLoginSection.vue:24,44） | image + fetch | §2.6 + §4.3：`fetch:{method:"login_qrcode", result_key:"png_base64"}`，结果局部归属，修复 `lastQrcode` 单槽串号 | 覆盖且修正现缺陷 |
| WebUI 链接（QqLoginSection.vue:108） | link | §2.3 bind `instance.webui_url`（注意非 03E 示例的 `webui_url`） | 覆盖 |
| owner 设置 | text/input + action | §2.2 可编辑 text，`set_owner{owner_qq:"$value"}`；W2.4 前插件需接受字符串 | 覆盖，依赖 W2.4/01 |
| 账号 self_id | text | §2.2 只读 + 复制 | 覆盖 |
| 头部刷新（QqPanel.vue:88-101） | 通用面板 | 外层面板提供；描述符中 `refresh` 按钮方法不存在（§2.7 注） | 范围外 |
| pending/加载态（QqPanel.vue:106-110，`qqListsPhase`） | 渲染器 | §2.0 per-node pending + §4.2 pending 键 | 覆盖 |
| 会话列表 QQ 分组（SessionSwitcher.vue:56-76） | platform + session_labels | 不属于渲染器（03b） | 范围外 |

### 开放问题

- **Q1** list_editor 与 `update_allowlist{user_ids, group_ids}` 双字段：推荐插件支持部分载荷；否则 v2 引入跨 bind 引用。
- **Q2** 二维码定时刷新 / 过期提示：v1 不做，用户手动刷新。
- **Q3** 名单「从好友/群列表选择」：v1 不覆盖，迁移期为功能退化项。候选方案（v2）：list_editor 增 `suggest:{method:"get_friends", value_key, label_key}`。是否接受该退化由 03b 的 parity 验收决定。
- **Q4** 本文标注的假设扩展字段（`arg_key` / `labels` / options 对象 `hint` / `confirm` / `clear_when`）：未知字段按 §3.2 会被旧 Panel 忽略，不破坏 v1 兼容；需 01 决定是否写入 adapter-plugin.md 的 v1 规范，未写入的在实现时删除。
