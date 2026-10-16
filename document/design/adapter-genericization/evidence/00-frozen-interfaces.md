# 冻结接口（所有泛化设计文档的共同基线）

> 任何方案不得与本节冲突；确需变更时必须在文档中显式写「需修订冻结项：<条目>理由 <理由>」。
> 仓库���线（2026-10-16）：core @ 3541b46 / panel @ 4b7d99b / echo-adapter-qq @ 79de9ec。

## 一、线协议（echo-protocol；Core ↔ Panel）

- **命令** `BackendCommand::InvokeAdapterOp { adapter: String, method: String, payload: Value }`
  （command.rs:279-287；payload 缺省 `{}`；clearance = Frontend-only 类别判定，command.rs:512-530）
- **响应** `BackendEvent::AdapterOpResult { adapter, method, result: Value /*null 时线上省略*/, error?: Option<String> }`
  （event.rs:735-745）
  - ⚠️ **无请求 id / call_id**：Panel 的 pending 配对（现有 `sendPending(command, key)` 模式）需要方案——
    由 02 文档决策：「以 (core, adapter, method) 键配对」或「扩展线协议加 call_id」，决策后写进文档。
- **事件** `BackendEvent::AdapterState { adapter: String, state: Value }`（event.rs:750-756）
  - `state` = `AdapterManagement::cached_state` 同构文档（形状见 §三）；epoch 由宿主缓存管理、**不上线**。
- **事件** `BackendEvent::AdapterList { adapters: Vec<AdapterStatus> }`（event.rs:730-732）
- **`AdapterStatus`**（event.rs:20-46）：
  `name, display_name, connected, running, self_id, bind_address, started_at, persona?,`
  `metadata: Vec<{key,label,value}>`（W2.2 通用 KV），
  `container / webui_url / onebot_url`（**deprecated**，迁移期保留，随 Panel 阶段 1 删除）
  - ⚠️ **缺口（01 负责决策）**：`AdapterStatus` **没有 `ui` / `session_labels` 字段**——
    插件 UI 描述符到达 Panel 的载体未定。候选：a) AdapterStatus 增字段；b) 经 AdapterState 状态文档承载；
    c) 新增独立事件。01 文档决策并给出线上形状。
- **旧 QQ 命令 / 事件**（迁移期共存，deprecated；删除计划与升级指南由 01 文档给出）：
  - 命令 command.rs:290-306（GroupList/FriendList/LoginStatus/Qrcode）、378-430（Allowlist/Denylist/FilterConfig/GateMode/Owner）
  - 事件 event.rs:763-800（QqFilterConfig/QqGateMode/QqLoginStatus/QqQrcode/QqOwner）+ GroupList/FriendList
- **W2.4（未做，01 设计）**：`GroupInfo.group_id`（event.rs:1001-）与 `FriendInfo.user_id`（event.rs:1241-）`i64 → String`；
  连带命令侧 `user_ids: number[] / group_ids: number[]`（command.rs:378-430）与 QqOwner `owner_qq: number`。
  Panel 侧 number 依赖链（QqPanel 的比较/toggle、state.ts）必须与 02/03 同批——01 的切片需写明发布顺序。

## 二、插件侧（echo-plugin-api）

- **`AdapterContribution`**（contribution.rs:78-100）：`name, platform, display_name,`
  `capabilities{send_text,send_media,groups,friends,gating,login},`
  `config_schema?: Value, ui?: Value, session_labels?: Value`
- QQ 插件当前 `config_schema/ui/session_labels` 全为 `None`（plugin.rs:186-188）——01 负责设计补全。
- **UI 描述符规范 v1**（adapter-plugin.md §Panel UI 解耦, :274-325）：
  `{ "version": 1, "panels": [{ "id", "title", "nodes": [...] }] }`；
  节点类型（v1 冻结）：`status / text / link / select / list_editor / image / button`；**未知节点忽略**；
  节点字段：`bind`（状态文档路径）/ `label` / `options` / `action:{method, arg?}` / `fetch:{method, result_key}`；
  `$value` = 当前控件值占位。

## 三、状态文档形状（`cached_state`）

插件实现位置：echo-adapter-qq `crates/echo-adapter-qq/src/adapter/qq/mod.rs:1244-1267`：

```jsonc
{
  "instance": { "persona": "…", "container": "…", "webui_url": "…", "onebot_url": "…", "bind_address": "…" },
  "running": true,
  "online": true,
  "self_id": "123456789",
  "gate": {
    "mode": "none | allowlist | denylist",
    "allowlist_users": ["…"], "allowlist_groups": ["…"],
    "denylist_users": ["…"],  "denylist_groups": ["…"]
  },
  "owner": 10001
}
```

- id 一律**字符串**；消费端（Panel 渲染器 bind 路径）以本文档为唯一引用标准。

## 四、管理方法（custom op；= AdapterManagement 12 method + get_groups/get_friends）

`set_gate_mode{mode} / get_gate_mode / update_allowlist{user_ids,group_ids} / update_denylist{...} /
get_filter_config / set_owner{owner_qq} / get_owner / login_status / login_qrcode(→{png_base64}) /
get_groups / get_friends / instance_info`

## 五、Panel 现状锚点与既定目标

- 归并键 `scopedKey(core, adapter)`（state.ts / state_domains）；pending 键模式 `qq:<kind>:<instance>`（QqPanel.vue:106-110）
- `sendPending(command, key)`（connection.ts:221-229）+ `REQUEST_RESPONSES` 命令→事件映射（pending.ts:46-67）
- `resolveTargetCore(command)`（connection.ts:305+）
- **既定目标**（冻结）：
  1. `qqByAdapter` → **`adapterUiByKey[scopedKey(core, adapter)]`**（state.ts 归并逻辑泛化保留）；
  2. 通用适配器面板（多实例切换 / 状态行 / 启停重启）上移为 **Panel 内置**（Start/Stop/RestartAdapter 已中立）；
  3. 插件只贡献 schema 部分；会话分组按 platform 字符串 + `session_labels` 通用化。

## 六、测试与工具链基线

- core：`cargo test --workspace`（target 热）；基线包 echo-protocol/echo-defs/echo-adapter 全绿。
- panel：node 在 `~/.nvm/versions/node/v22.22.3/bin`；`npm test`（vitest）**56 文件 / 418 用例全绿**为基线。
- 插件：`cargo test --workspace` 全绿（135+63+24+…）。
- 设计文档产出目录：`document/design/adapter-genericization/`（core 仓库内）。
