# 证据包：01（core / 协议 / 插件侧泛化）

> 配套：`00-frozen-interfaces.md`。行号基于 core @ 3541b46 / 插件 @ 79de9ec。

## A. 代码残留盘点（验收 #7「零平台残留」相关）

1. **线协议 QQ 词汇**（迁移期保留，deprecated；删除周期 + 升级指南 = 01 任务）
   - 命令：`command.rs:290-306`（RequestGroupList / RequestFriendList / RequestQqLoginStatus / RequestQqQrcode）、`:378-430`（UpdateQqAllowlist / UpdateQqDenylist / RequestQqFilterConfig / SetQqGateMode / SetQqOwner / RequestQqOwner）
   - 事件：`event.rs:763-800`（QqFilterConfig / QqGateMode / QqLoginStatus / QqQrcode / QqOwner）
   - `AdapterStatus` 的 deprecated 三字段（container / webui_url / onebot_url，event.rs:41-53）
   - clearance 已改类别判定（command.rs:512-530，`adapter_management_commands_are_frontend_only_by_category`）——**已完成，不要重做**
2. **backend/echo-agent**（QQ 命名 vs 通用内容）
   - `src/packages/adapter_qq/{mod.rs,commands.rs,bridge.rs}`：模块名 QQ，内含通用命令分发（`apply_qq_command` 含 `InvokeAdapterOp` 臂）；`resolve_qq_adapter`（commands.rs:22-46）按 `platform() == "qq"` 过滤
   - 分发入口 `agent/commands.rs:1495-1497`：`InvokeAdapterOp | <8 个旧 QQ 命令> => apply_qq_command`
   - `emit_adapter_state_events`（packages/adapter_qq/commands.rs:426+）：cached_state 单源派生 `AdapterState` + 旧 QQ 事件（W3.4 已单源化）
3. **core/src**（平台硬编码点）
   - `qq_tools.rs`（1054 行；`register_qq_tools_multi`；工具��� send_*/get_* 已是通用词汇但注册逻辑按 platform=="qq"）
   - `qq_instances.rs`（端口分配 / compose / 写回）
   - `main.rs:829-857`：plugin adapter 注册循环——`platform` 变量取了但只用于日志；`matches qq_instances` 找实例，未匹配日志 "non-QQ adapter plugin?"（**非 QQ 适配器当前无实例概念**）
   - `main.rs:938-962`：per-persona 工具注册（`factory_qq_by_persona`，default 回退 `qq_default2`）
   - `main.rs:1540-1580`：入站 worker（命令域 + AgentMessageHook）
   - `main.rs:1596-1625`：`subscribe_states` → `AdapterStateChanged`（仅连接态，不含状态文档）
   - `adapter_commands.rs`：命令域（/echo 等），`try_handle` 通用
4. **echo-defs / echo-adapter**：已平台中立（AdapterInfo 注释明示 2026-10 抽象泄漏重构后无平台字段）——**勿重做**

## B. UI 描述符链路缺口（01 任务核心之一）

1. 插件贡献：`AdapterContribution.ui / config_schema / session_labels`（contribution.rs:78-100）——**QQ 插件全 None**（plugin.rs:186-188）
2. 宿主 `RemoteAdapter`：`contribution` 为**私有字段**；公开面只有 `Adapter` trait（name/display_name/platform/status_info/…）——**无 ui / session_labels / config_schema 访问器**（adapter.rs:34-62, 152-270）
3. 宿主 `report_ui_state`（adapter.rs:581-595）：`note_management_state(state)` 更新缓存后直接 Ok——**无广播**（不发 AdapterStateChanged，也不推送状态文档）
4. `invalidate_management_state`（adapter.rs:100-106）：**全仓零调用**（grep 证实）——epoch 失效未接线
5. Core `emit_adapter_list`（agent/mod.rs:1207-1290）：从 `management().cached_state()["instance"]` 派生 persona/metadata（container/webui_url/onebot_url 三键），与 deprecated 字段同源并存
6. `AdapterStatus` **无 ui / session_labels 字段** → 描述符到 Panel 的载体缺失（01 决策：a. AdapterStatus 增字段 / b. 经 AdapterState 状态文档 / c. 新事件；给出理由与线上形状，注意 Panel 是**多上游聚合**，一切只能走 Core 转发）
7. epoch / 失效语义：计划复用「宿主缓存 + epoch 失效」（参考 adapter-plugin.md §交接与崩溃恢复语义 + 风险表 #1）；实现接不了要写清切片

## C. W2.4（i64 → String）涉及面

- `GroupInfo.group_id`（event.rs:1001-）、`FriendInfo.user_id`（event.rs:1241-）
- 连带的命令/事件字段：`UpdateQqAllowlist/Denylist { user_ids: Vec<i64>, group_ids: Vec<i64> }`（command.rs:378-402）、`QqFilterConfig` 四数组（event.rs:763-770）、`SetQqOwner { owner_qq: i64 }`（command.rs:416）、`GroupList.groups` / `FriendList.friends`
- Panel 侧依赖 number 的链路（比较/toggle/表单）在 02/03 范围；01 文档只需给出「同批发布顺序 + 迁移口径」

## D. 其他已知事实

- 管理方法 12 个已在插件实现（见 00 文件 §四）；`get_groups/get_friends` 经 `qq:<op>` 直通（plugin.rs:221）
- 双跑测���已入库：`dual_run_old_and_new_gate_commands_produce_equivalent_state`
- 交接语义（先停后切）已定稿（adapter-plugin.md §交接与崩溃恢复语义）——协议/宿主侧勿改
- `echo-plugin-host/tests/conformance_stdio.rs` 14 项（a-j + 扩展）、`conformance.rs` 14 项（a-n）；新增协议字段时按既有模式补用例

## E. 补充核实（2026-10-16 第二轮）

- `InvokeAdapterOp` 处理臂（packages/adapter_qq/commands.rs:122-158）内嵌 **QQ 专属广播**：
  `SHARED_WRITE_METHODS = [set_gate_mode, update_allowlist, update_denylist, set_owner]` 写方法先
  `broadcast_adapter_op_to_others`（:484-505，**内部过滤 `platform() != "qq"` 跳过**）再执行；
  广播存在原因：QQ 门控态持久化在共享 `[adapters.qq]` 段、多实例内存值需对齐。泛化设计需处理：
  a) 广播语义改为插件/平台自声明（或删，交插件侧）；b) 名称/模块去 QQ 化。
- 读类方法（get_*）经 `InvokeAdapterOp` 后**不触发** `emit_adapter_state_events`（仅写类刷新）——
  Panel 通道切换设计需注意：读结果只经 `AdapterOpResult.result` 返回。
- `resolve_qq_adapter`（adapter_qq/commands.rs:22-46）：显式实例名走名称查找（平台无关）；
  **缺省回退路径过滤 `platform() == "qq"`**（QQ-only）——泛化时需按适配器类型/注册表记忆化。
