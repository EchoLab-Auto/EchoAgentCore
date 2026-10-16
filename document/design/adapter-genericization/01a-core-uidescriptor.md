# 01a · Core 侧 UI 描述符链路 / 状态推送 / epoch 失效 / QQ 插件 UI 补全

> 基线：core @ 3541b46 / 插件 @ 79de9ec；不违背 00-frozen-interfaces（下称 00），需修订处显式标注。
> 范围：宿主 / 协议 / 插件；Panel 组件与文件级细节归 02 / 03a / 03b。行号经 grep/sed 局部核查（2026-10-16）。

## 0. 决策摘要

| # | 决策 | 一句话理由 |
|---|---|---|
| A | 描述符载体 = **AdapterStatus 增 `ui` / `session_labels`**（选项 a） | 静态数据随 AdapterList；状态文档 §三 保持纯动态 |
| B | `report_ui_state` → 宿主新增 `subscribe_ui_states` 广播 → 组合根发 `AdapterState` | 补齐「插件主动推送」，与连接态通道分离 |
| C | epoch = 插件 **generation**（重启/升级自增），读时比对惰性失效；失效后**等插件推送**，另提供 `get_state` 主动拉 | 无需 supervisor 回调 RemoteAdapter；插件 on_ready 已推初始快照 |
| D | QQ 插件补全 ui v1（7 类节点）+ session_labels + config_schema；修 `update_*list` 部分载荷清空缺陷 | parity + 03a Q1 前提 |
| E | Q4：采纳 options 对象 `hint` / `clear_when` / `confirm` 入 v1；否决 `arg_key` / `labels` | 后者已被冻结字段 `arg` / options.label 覆盖 |

## 1. 核心侧链路（A 载体 / B 推送 / C 失效）

### 1.1 A · 描述符载体：AdapterStatus 增字段（选 a）

- 否决 b（塞进状态文档）：§三 是 `cached_state` 唯一引用标准（00 §三），混入静态 ui 会让每次推送重复携带 KB 级描述符，且 03a §4.1 规定整文档替换，`bind` 路径会与 `ui` 键冲突。
- 否决 c（新事件）：Panel 多上游聚合，新事件需要新的「何时下发」规则；AdapterList 已在连接握手（agent/commands.rs:806）与启停（:1401/1420/1451/1459）时全量下发，正好覆盖「描述符随贡献注册、插件重启后可能变化」。
- 线上形状（event.rs:20-46 `AdapterStatus` 追加，均 `#[serde(default, skip_serializing_if = "Option::is_none")]`，旧 Panel 忽略）：

```jsonc
{ "name": "qq-default", "display_name": "QQ / OneBot", "connected": true, "running": true, "...": "...",
  "platform": "qq",                                  // 新增：03b 会话分组按 platform 字符串（00 §五-3）
  "ui": { "version": 1, "panels": [ /* §2.1 */ ] },  // 新增：AdapterContribution.ui 原样透传
  "session_labels": { "dm": "私聊", "group": "群聊" } } // 新增：AdapterContribution.session_labels 原样透传
```

- 需修订冻结项：00 §一 `AdapterStatus` 字段表，追加 `platform? / ui? / session_labels?`。理由：00 已把载体留给 01 决策；三者均可选，向后兼容。`platform` 现只在 `AdapterInfo.platform` 里（adapter.rs:201），线上没有，SessionSwitcher 通用化需要它。（待验证：Panel 是否已能从其它字段推出 platform；验证方法：grep `platform` web/src/protocol.ts）
- 改造点：
  1. echo-adapter `Adapter` trait（traits.rs:113-156 附近）加默认方法 `fn ui_descriptor(&self) -> Option<Value> { None }`、`fn session_labels(&self) -> Option<Value> { None }`。不放进 `AdapterInfo`，echo-defs 保持平台中立（证据 A.4）。
  2. `RemoteAdapter`（adapter.rs:34-62）：`contribution` 保持私有，`impl Adapter`（:152-270）里实现上面两个方法，返回 `self.contribution.ui.clone()` / `session_labels.clone()`。
  3. `emit_adapter_list`（agent/mod.rs:1207-1290）：`.map(|info| …)` 内用已取到的 `self.adapters.get(&info.name)` 填 `platform: Some(info.platform)`、`ui`、`session_labels`。
- 重启后描述符刷新：supervisor 重启会整体替换贡献（supervisor.rs:867-869 `set_contributions`），但 `RemoteAdapter` 持有构造时的副本（plugins_runtime.rs:155）。v1 规定**描述符在进程生命周期内静态**，插件升级改了 ui 就需要重建 RemoteAdapter（S6，可选）。

### 1.2 B · 插件主动上报 → AdapterState

现状：`report_ui_state` 臂（adapter.rs:581-591）只调 `note_management_state`，不广播；`AdapterState` 只在命令响应 / 握手时经 `emit_adapter_state_events`（packages/adapter_qq/commands.rs:429-445）发出。插件每次写操作和连接变化都会推送（插件 plugin.rs:87-107、:310-327），所以现在这些推送到不了 Panel。

1. host：`AdapterHostService` 加 `ui_state_sinks` + `pub fn subscribe_ui_states() -> UnboundedReceiver<AdapterUiStateChange{adapter, state: Value}>`，与 `subscribe_states`（:403-411）同构。不扩展 `AdapterStateChange`：它是 `Eq` 的连接态结构（:367-375），混入 Value 会破坏派生和既有测试（:867-890）。
2. `report_ui_state` 臂：`note_management_state(state.clone())` 之后调用 `broadcast_ui_state(..)`。payload 带 `epoch` 时，与当前 generation 不一致就丢弃（见 1.3）。
3. 组合根 main.rs:1584-1606：照 persona 映射再起一个 task，消费 `subscribe_ui_states`，执行 `entry.agent.emit(BackendEvent::AdapterState{adapter, state})`。映射找不到时**回退默认 persona** 而不是 `continue`；否则非 QQ 适配器（证据 A.3，main.rs:829-857 无实例概念）的状态全被丢掉。persona 也可以直接取 `state["instance"]["persona"]`（插件 mod.rs:1248 已上报），优先用它。
4. 旧 QQ 事件派生：迁移期仍需由同源派生（W4.1）。抽出 `emit_adapter_state_events` 里「state → 旧事件」的纯函数（commands.rs:446+），推送路径也调用它，旧 QqPanel 才能看到插件推送的变化。随 Panel 阶段 1 结束一并删除。

### 1.3 C · epoch 失效

- 现状：`invalidate_management_state`（adapter.rs:100-106）全仓零调用；重启（supervisor.rs:865-873）和升级（`Control::Upgrade` 臂 :692+）都不通知 RemoteAdapter。
- 设计：`Shared` 加 `generation: AtomicU64`，凡是 `set_conn(Some(新连接))` 成功握手的地方都自增（重启 :870；升级臂换连接处，待验证具体行，验证方法：`sed -n 692,760p supervisor.rs`）。`PluginHandle::generation()` 公开。`RemoteAdapter` 缓存改成 `Option<(u64, Value)>`；`note_management_state` 记录当前 generation，`management_state()` 读取时 generation 不同就返回 None（惰性失效，等价于「重启/升级后由宿主失效」）。`invalidate_management_state` 保留，由 `report_stopped` 臂（:574 附近）显式调用。
- 失效后**不主动 invoke 拉取**，等插件推送：插件 on_ready 后一定推一次初始快照（插件 plugin.rs:763-765 有测试），交接语义里新实例 start 之后会 report_state，后者又会触发快照推送（plugin.rs:310-327）。兜底：插件 custom 增 `get_state`（返回 `cached_state`），管理读路径遇到 None 时由 Core 发一次 `invoke_custom("get_state")`，结果走 `note_management_state` 并广播。这条兜底也是 03b §2.3-5 要的「请求重推状态」。
- 与交接语义一致：三源仲裁里 ③ 宿主缓存在 ①② 未就绪时不可信（adapter-plugin.md 交接节），generation 失效正好在 ① 换代那一刻生效。

## 2. QQ 插件 UI 补全（D）

### 2.1 ui 描述符 v1（bind 严格按 00 §三；扩展字段见 §3.1）

```jsonc
{ "version": 1, "panels": [
  { "id": "login", "title": "登录", "nodes": [
    { "type": "status", "bind": "online", "label": "QQ 连接" },
    { "type": "text",   "bind": "self_id", "label": "账号" },
    { "type": "image",  "label": "登录二维码", "clear_when": "online",
      "fetch": { "method": "login_qrcode", "result_key": "png_base64" } },
    { "type": "link",   "bind": "instance.webui_url", "label": "打开 NapCat WebUI" } ] },
  { "id": "gate", "title": "消息门控", "nodes": [
    { "type": "select", "bind": "gate.mode", "label": "门控模式",
      "options": [ { "value": "none", "label": "不限制", "hint": "所有消息均响应" },
                   { "value": "allowlist", "label": "白名单", "hint": "仅响应名单内用户/群" },
                   { "value": "denylist", "label": "黑名单", "hint": "忽略名单内用户/群" } ],
      "action": { "method": "set_gate_mode", "arg": { "mode": "$value" } } },
    { "type": "list_editor", "bind": "gate.allowlist_users",  "label": "白名单用户",
      "action": { "method": "update_allowlist", "arg": { "user_ids":  "$value" } } },
    { "type": "list_editor", "bind": "gate.allowlist_groups", "label": "白名单群",
      "action": { "method": "update_allowlist", "arg": { "group_ids": "$value" } } },
    { "type": "list_editor", "bind": "gate.denylist_users",   "label": "黑名单用户",
      "action": { "method": "update_denylist",  "arg": { "user_ids":  "$value" } } },
    { "type": "list_editor", "bind": "gate.denylist_groups",  "label": "黑名单群",
      "action": { "method": "update_denylist",  "arg": { "group_ids": "$value" } } } ] },
  { "id": "owner", "title": "主人", "nodes": [
    { "type": "text", "bind": "owner", "label": "主人 QQ", "editable": true,
      "action": { "method": "set_owner", "arg": { "owner_qq": "$value" } } },
    { "type": "button", "label": "刷新状态", "action": { "method": "get_state" } } ] } ] }
```

- 7 类节点齐全；parity 对照 03a §6。`text.editable` 按 03a §2.2「可编辑 text」口径（若 03a 字段名不同，以 03a 为准），回应 03b D1：**不新增节点，不修订冻结项**。
- 刷新按钮改用插件新增的 `get_state`（§1.3），不用示例里不存在的 `refresh`（03a §2.7 注）。
- 名单从好友/群列表勾选（03a Q3 / 03b D3）：v1 不覆盖，接受退化；v2 再加 `suggest`。

### 2.2 session_labels / config_schema 草案

```jsonc
"session_labels": { "dm": "QQ 私聊", "group": "QQ 群" }
"config_schema": { "type": "object", "required": ["bind_address"], "properties": {
  "bind_address":      { "type": "string", "title": "OneBot 反向 WS 监听地址", "default": "127.0.0.1:0" },
  "napcat_container":  { "type": "string", "title": "NapCat 容器名" },
  "napcat_webui_url":  { "type": "string", "format": "uri", "title": "NapCat WebUI" },
  "napcat_onebot_url": { "type": "string", "format": "uri", "title": "OneBot URL" },
  "persona":           { "type": "string", "title": "绑定人格" } } }
```

config_schema 目前仅作声明（JSON Schema draft-07 子集），Core 和 Panel v1 都不消费。字段对齐插件配置结构 `inner.config`（mod.rs:1249-1252）；`default` 值待验证，验证方法：grep `bind_address` 插件 config.rs。

### 2.3 插件代码点（echo-adapter-qq @ 79de9ec）

1. 新建 `crates/echo-adapter-qq/src/ui.rs`：`pub fn ui_descriptor() -> Value`、`session_labels()`、`config_schema()`，内容用 `json!` 常量写死，加快照测试。
2. `plugin.rs:168-189 fn adapter()`：把三个 `None` 换成 `Some(ui::…())`。
3. `adapter/qq/mod.rs` `invoke_custom`：
   - 新增 `"get_state" => self.cached_state()` 臂，返回后调用 `push_ui_state`（plugin.rs:87）。
   - **修部分载荷**（03a Q1）：`update_allowlist/denylist`（mod.rs:1325-1336 附近）调用 `parse_ids` 时缺键会得到空数组（mod.rs:1469-1472），而 `filter.rs:54-60` 会整体覆盖，结果只发 `user_ids` 就把群名单清空了。改为 `Option<Vec<i64>>`，缺键时保留现值。
   - `set_owner` 的 `parse_id` 接受字符串数字（W2.4 前置，00 §一）。待验证：现在是否已接受；验证方法：读 mod.rs `fn parse_id`。

## 3. 决策回应与实施切片（E / F）

### 3.1 E · 回应 03a §6 Q4 与 03b §1.6 / §2.3

| 扩展字段 | 建议 | 理由 |
|---|---|---|
| select `options` 对象形态 `{value,label?,hint?}` | **写入 v1**（03b D2 同意） | 覆盖 GATE_HINTS parity；字符串数组仍合法，旧渲染器按 §3.2 容错 |
| image `clear_when: <bind 路径>` | **写入 v1** | 替代「status=online」启发式（03a §4.3-4），通用 |
| text `editable` + `action` | **写入 v1**（03b D1） | owner 编辑只能靠它；冻结节点类型不变 |
| button `confirm?: string` | **写入 v1**（可选） | 纯展示层确认文案，不影响协议 |
| `arg_key` | **否决，实现时删** | 冻结的 `arg` + `$value` 嵌套对象（03a §1.6）已能表达 `{user_ids:"$value"}` |
| `labels` | **否决，实现时删** | options 对象的 `label` 已覆盖 |
| list_editor `fetch` 候选（03b D3）/ `visible_when`（03b §2.3-8） | **v2**，v1 不写 | 03a Q3 已判退化可接受；挂载即拉码用 `clear_when` 取反语义不够清晰，留给 v2 |

- 写入方式：由 adapter-plugin.md 维护者在 §Panel UI 解耦 节点字段行追加上表「写入 v1」四项（本文不改规范文件）。
- 需修订冻结项：00 §四 方法表追加 `get_state`（→ 返回完整 cached_state 并触发推送）。理由：03b §2.3-5「请求重推状态」与 §1.3 失效兜底共用这一个方法；它是 custom op，线协议不变。
- 03b §2.3-5：刷新 = 重请 AdapterList，再对选中实例 `InvokeAdapterOp{method:"get_state"}`；结果通过 `AdapterState` 推送回来，`AdapterOpResult` 只用于确认。
- 03b §2.3-4：`capabilities` 不上线，与 03b 一致；渲染器区是否显示只看 `AdapterStatus.ui`（§1.1）。03a §4.1 selector 的「先读 `AdapterStatus.ui`，再读 `state.ui`」回退可删掉 `state.ui` 一支。
- 03b §2.3-1 persona 过滤：`AdapterStatus.persona` 来源不变（agent/mod.rs:1229，取自 `instance.persona`）；非 QQ 插件没上报 `instance` 时为空，即全局实例，与 03b 假设一致。

### 3.2 F · 实施切片

基线命令（00 §六）：core `cargo test --workspace`；插件 `cargo test --workspace`；panel `npm test`。

| 片 | 文件 / 要点 | 验收 | 依赖 | 并行性 |
|---|---|---|---|---|
| S1 协议 | `echo-protocol/src/event.rs:20-46` 加 `platform?/ui?/session_labels?`（serde default + skip_none）；round-trip 与「旧 JSON 缺字段可解」测试 | `cargo test -p echo-protocol` | — | 与 01b/02/03 并行；Panel 先只读字段（03a selector） |
| S2 trait+宿主访问器 | echo-adapter traits.rs 两个默认方法；`RemoteAdapter` impl（adapter.rs:152-270）；单测：contribution 带 ui，访问器返回同值 | `cargo test -p echo-adapter -p echo-plugin-host` | S1 | 可与 S4 并行 |
| S3 AdapterList 填充 | `agent/mod.rs:1207-1290` 填三字段；测试：fake adapter 带 ui，AdapterList JSON 含 `ui.version==1` | `cargo test -p echo-agent` | S2 | — |
| S4 推送接线 | adapter.rs：`ui_state_sinks`、`subscribe_ui_states`、`broadcast_ui_state`，`report_ui_state` 臂（:581-591）调用广播；main.rs:1584-1606 新 task 发 `AdapterState`（persona 先取 `instance.persona`，否则回退默认）；抽出旧事件派生纯函数（commands.rs:446+）共用；测试套用 adapter.rs:898+ 的 report_ui_state 用例并断言 recv | `cargo test -p echo-plugin-host -p echo-agent` + `cargo build -p core` | — | 与 S1-S3 并行；02 的 Panel 归并依赖本片 |
| S5 generation 失效 | supervisor.rs：`generation` 在 :870 与升级换连接处自增；RemoteAdapter 缓存 `(gen, Value)`；`report_stopped` 臂调 `invalidate_management_state`；读路径 None 时 `invoke_custom("get_state")` 兜底；测试：模拟重启后 `cached_state()==None`，推送后恢复 | `cargo test -p echo-plugin-host` | S4 | 与插件 P1 并行 |
| P1 插件 UI | 插件 `src/ui.rs` 新建；`plugin.rs:186-188` 赋值；`get_state` 臂；`update_*list` 部分载荷（mod.rs + filter.rs:54）；快照测试锁定 §2.1 JSON | 插件 `cargo test --workspace` | 协议无依赖（字段早已存在于 contribution.rs:78-100） | 可最先做；03a §5.5 快照用同一份 JSON |
| S6（可选） | 重启后贡献变化时重建 RemoteAdapter 并重发 AdapterList | `cargo test -p echo-plugin-host` | S5 | v1 不阻塞 |

发布顺序：P1 → S1-S4（Core 一次发版）→ S5 → Panel 阶段 2（03a/03b，读 `AdapterStatus.ui`）。W2.4 i64→String 由 01b 承担；本文 `owner` bind 不受影响，因为 text 节点把数字和字符串都按字符串显示。

