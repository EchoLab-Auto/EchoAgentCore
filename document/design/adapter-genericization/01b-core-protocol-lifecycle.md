---
id: gen-01b
title: "01b · 线协议 id 去号码化"
group: 规划
x: 640
y: 3480
---

# 01b：线协议 id 去号码化（W2.4）· 旧 QQ 变体生命周期 · QQ 残留清理

> 状态：已实施（W2.4 与 `adapter_qq/`→`adapter_ops/` 改名等，落地见 [06 台账](./06-execution-progress.md)）；双读兼容层已随「全网同步更新」收敛，现状见 [协议与数据流](../../protocol/protocol.md)

> 基线：core @ 3541b46 / panel @ 4b7d99b / echo-adapter-qq @ 79de9ec；冻结接口见 `evidence/00-frozen-interfaces.md`。
> 下文 `C:` = core `source/`，`P:` = 插件 `crates/echo-adapter-qq/src/`。本文只读分析产出，未改代码。

## A. W2.4「i64 → String」全链路

**决策**：版本 N（= Panel 阶段 1 同批，遵守 adapter-plugin.md:381）把下表字段全部改为 `String`；
**命令侧双读**（number|string，旧客户端照常可发），**事件侧纯 breaking**（只输出 string）。插件**零改动**。

| 字段 | 现状（C:protocol/echo-protocol/src/） | 新形状 |
|---|---|---|
| `GroupInfo.group_id` | event.rs:1002 `i64` | `String` |
| `FriendInfo.user_id` | event.rs:1242 `i64` | `String` |
| `GroupList.groups` / `FriendList.friends` | event.rs:757 / :802（元素类型随上两行） | `Vec<GroupInfo>` / `Vec<FriendInfo>`（元素已 String） |
| `UpdateQqAllowlist.{user_ids,group_ids}` | command.rs:382-383 `Vec<i64>` | `Vec<String>` + 双读 |
| `UpdateQqDenylist.{user_ids,group_ids}` | command.rs:399-400 `Vec<i64>` | `Vec<String>` + 双读 |
| `SetQqOwner.owner_qq` | command.rs:417 `i64` | `String` + 双读；`""` = 清除（替代 `0`） |
| `QqFilterConfig` 四数组 | event.rs:764-767 `Vec<i64>` | `Vec<String>` |
| `QqOwner.owner_qq` | event.rs:797 `i64` | `String`（未设置 = `""`） |

**① serde 策略**：命令由 Core 反序列化，可在 echo-protocol 新增私有模块 `id_compat`
（`deserialize_with`：接受 JSON number 或 string，统一成 `String`；number 用 `to_string()`），
同时保留序列化为 string。事件由 Core 序列化、Panel 消费，无法「双写」同一字段 → 纯 breaking，靠同批发布兜底。
不引入 `#[serde(untagged)]` 枚举（会把类型噪声扩散到消费点）。线上测试：command.rs:585/628 现用 `owner_qq: 123`，改为断言 number 与 `"123"` 均可解析。

**② Core 消费点**（C:backend/echo-agent/src/packages/adapter_qq/commands.rs）：
- `id_name_list`（:526-545）现把字符串 id **反解析回 i64**；改为返回 `Vec<(String, String)>`，number 也 `to_string()`；
  GroupInfo/FriendInfo 构造点 :287 / :322 随之无须转换。
- `emit_adapter_state_events` 派生旧事件（:446-459 `parse_ids`）：i64 来源 = 对 cached_state 中**字符串**
  （P:adapter/qq/mod.rs:1260-1263 `stringify_ids`，:1464）做 `parse()`；改为直接收集字符串（兼容 number）。
- `owner`：插件 cached_state 的 `owner` **仍是 number**（P:mod.rs:1265），Core 在 :254 / :478 `as_i64()`。
  改为「number→to_string / string 原样 / 0 或缺省→`""`」。冻结 §三 示例也是 `"owner": 10001`——
  **不修订冻结项**，Core 宽容读取；建议（非阻塞）后续插件改 `"owner": "10001"|null`，届时冻结文档同步。
- SetQqOwner 臂（:208-230）：payload `{"owner_qq": owner_qq}` 透传字符串即可，插件 `parse_id` 接受 str（P:mod.rs:1493-1506）；
  `owner_qq > 0` 判断改 `!owner_qq.is_empty()`。Allow/Deny 臂同理透传字符串数组。
- `qq_tools.rs`（C:core/src）工具参数 :399/:429/:457/:460/:556/:584/:609/:631 `as_i64()`：属 **LLM 工具参数**而非线协议，
  不在 W2.4 breaking 范围；但为去号码化统一加 `arg_id()`（number|string→String）助手，宽容不 breaking。
  **待验证**：这些 id 之后传给 `Adapter::send_*`/management 的类型（验证：`sed -n 395,470p core/src/qq_tools.rs`）。

**③ 插件侧核对**：`parse_ids`/`parse_id`（P:mod.rs:1469-1506）已接受 number|string；`get_groups`/`get_friends`
输出 `{"id": id.to_string()}`（P:mod.rs:1326-1339, 1518-1523）；cached_state 门控数组已是字符串。
→ 插件内部仍以 i64 存储（平台私有），线上一律字符串，W2.4 **插件无需发版**；只补一条测试：`update_allowlist` 传 `["7"]` 等价 `[7]`（参考 P:plugin.rs:1038）。

**④ 同批发布**：版本 N = Core（protocol 改型 + 消费点）+ Panel 阶段 1（改走 InvokeAdapterOp/AdapterState，id 本就是 string）。
- 新 Panel × 旧 Core：新通道在 3541b46 已存在，可用 ✔；旧 Panel × 新 Core：命令双读可用，**旧事件 id 变 string → 旧 QqPanel 比较/toggle 失效** ✘ → 升级指南要求 Core/Panel 同升。
- 顺序：插件测试补齐（可先行）→ Core 合入（不发布）→ Panel 阶段 1 合入 → 同 tag 发布。不设「i64 兼容期」：旧事件本身已 deprecated，避免两次 breaking。

## B. 旧变体删除计划

> **后注（2026-10-16）**：本节原计划「N 标 deprecated → N+1 观察 → N+2 物理删除」。同日决策人指示**直接删除（不等 N+2）**——下述 S7/S8 合并执行：旧 10 命令 / 7 事件 / `AdapterStatus` 三字段 / `legacy.rs` / `id_compat.rs` / dual_run 测试已全部物理删除；升级指南改为「直接删除已生效」口径（见 adapter-plugin.md §升级指南）。本节其余内容保留为设计决策历史。

**时间线**：版本 N（W2.4 + Panel 阶段 1）= 旧变体标 `#[deprecated]`（仅文档/注释已标，代码属性待加）且 id 改 String；
版本 N+1 = 观察期（日志 warn 统计旧命令调用）；**版本 N+2 = 物理删除**（两版本周期，adapter-plugin.md:397）。
**删除前置条件**（全部满足）：① Panel 阶段 1 已发布且 `grep -rn 'RequestQq\|UpdateQq\|SetQq\|QqFilterConfig\|QqOwner' panel/src` 仅剩测试/兼容注释；
② 新通道等价性有替代测试（见下）；③ N+1 期间无已知第三方客户端依赖（发行说明公告）。

| 旧命令（command.rs） | 新通道 method | 旧事件（event.rs） | 新载体 |
|---|---|---|---|
| RequestGroupList :289 | `get_groups` | GroupList :757 | `AdapterOpResult.result.groups` |
| RequestFriendList :294 | `get_friends` | FriendList :802 | `AdapterOpResult.result.friends` |
| RequestQqLoginStatus :299 | `login_status` | QqLoginStatus :778 | AdapterOpResult / AdapterState.online |
| RequestQqQrcode :304 | `login_qrcode` | QqQrcode :787 | `result.png_base64` |
| UpdateQqAllowlist :378 / UpdateQqDenylist :395 | `update_allowlist` / `update_denylist` | QqFilterConfig :763 | AdapterState `gate.*` |
| RequestQqFilterConfig :403 | `get_filter_config` | 同上 | 同上 |
| SetQqGateMode :408 | `set_gate_mode` | QqGateMode :772 | AdapterState `gate.mode` |
| SetQqOwner :416 / RequestQqOwner :423 | `set_owner` / `get_owner` | QqOwner :796 | AdapterState `owner` |

**删除顺序**（同一 PR 内分提交，每步 `cargo test --workspace` 绿）：
1. Core 分发：agent/commands.rs:1495-1497 一带的旧 QQ 臂（起始行待核：`grep -n 'RequestGroupList' agent/commands.rs`）只留 `InvokeAdapterOp`；删 adapter_qq/commands.rs `apply_qq_command`（:47 起）内除 `InvokeAdapterOp`（:122-158）外各臂。
2. 派生：`emit_adapter_state_events`（:429-480）只保留 `AdapterState` 发射，删 :445-479 旧事件派生、`id_name_list`（:526）。
3. 协议：删 command.rs 10 变体 + 测试 :585/:628 中旧变体部分；删 event.rs 7 变体 + `GroupInfo`/`FriendInfo`（:1001/:1241，**待验证**无其它引用：`grep -rn 'GroupInfo\|FriendInfo' source --include=*.rs`，已知 echo-agent lib.rs:89 / event.rs:10 再导出需同删）。
4. `AdapterStatus` 三字段（event.rs:37-46）：删字段及 agent/mod.rs:1230-1255 的赋值；`metadata` 中同名三键**保留**（由插件 `instance` 文档派生，泛化后应改为插件自声明 metadata，见 D.4）。

**测试调整**：
- `dual_run_old_and_new_gate_commands_produce_equivalent_state`（commands.rs:760）：使命是锁定旧/新等价，旧路径删除即失去意义 → **N+2 删除**，
  以「新通道 golden 测试」替代：`set_gate_mode/update_allowlist/set_owner` 后 `AdapterState.state` 等于期望 JSON 快照（id 均 string）。
- `invoke_adapter_op_write_method_broadcasts_and_acks`（:861）：随 D.2 决策改写（不是删除时点的事）。
- Panel 旧事件 fixture 测试由 02/03 处理，本文不展开。
## C. W4.3 升级指南草案

> 发行说明草稿（版本 N 起，N+2 完成）。

**线协议 breaking（版本 N）**
- id 一律字符串：`GroupInfo.group_id`、`FriendInfo.user_id`、`QqFilterConfig` 四数组、`QqOwner.owner_qq` 输出改为 string；
  `QqOwner.owner_qq` 未设置由 `0` 变 `""`。命令侧 `user_ids/group_ids/owner_qq` 同时接受 number 与 string（N+2 起只保证 string 语义，随变体删除）。
- `AdapterStatus.container/webui_url/onebot_url` deprecated：改读 `metadata[]` 中 `key` 同名项。

**废弃时间表（两版本周期）**
| 版本 | 状态 |
|---|---|
| N | 旧 10 命令 / 7 事件 / 3 字段标 deprecated；Core 收到旧命令记 warn；id 改 String |
| N+1 | 行为不变；发行说明再次提示 |
| N+2 | 物理删除；旧命令反序列化失败（`unknown variant`），Core 对未知命令的回应**待验证**（验证：查 WS 入站反序列化失败分支，`grep -rn 'from_str::<BackendCommand>' source`） |

**用户侧操作**
1. Core 与 Panel **必须同时升级到 N**（旧 Panel 连新 Core 会出现名单开关失效）；QQ 插件无需升级。
2. 自建客户端：把管理命令改为 `InvokeAdapterOp{adapter, method, payload}`，监听 `AdapterOpResult` / `AdapterState`；方法映射见 B 表。
3. 配置文件不变（`[adapters.qq]` 中 owner_qq 等仍为整数，属插件私有）。

## D. QQ 残留清理（验收 #7）

**D.1 模块改名**：`packages/adapter_qq/` → `packages/adapter_ops/`；`apply_qq_command`→`apply_adapter_command`，
`resolve_qq_adapter`→`resolve_adapter`。N～N+1 期间旧臂留在 `adapter_ops/legacy.rs`（迁移兼容层，豁免 grep），N+2 随 B 删除。
缺省回退（commands.rs:28-45 按 `platform()=="qq"`）**决策：按「注册表全体」计数**——0 个→错误、恰 1 个→选它、≥2→要求显式 `adapter`。
理由：`InvokeAdapterOp.adapter` 是必填 String（冻结 §一），新通道本不走缺省回退；回退只服务旧 QQ 命令（`adapter: Option`），
旧臂在 legacy.rs 内保留 `platform()=="qq"` 过滤属兼容层豁免，N+2 一并消失。否决「按适配器类型记忆化」：增加状态且无新消费者。
风险：低——新通道恒带 adapter，泛化回退仅影响将来新增的 Option 寻址命令；legacy 内保持原 QQ 过滤，旧客户端行为不变。

**D.2 写方法广播**（commands.rs:134-150 `SHARED_WRITE_METHODS` + :484-505 `platform()!="qq"`）
- 案 a 插件自声明：`AdapterContribution` 增 `shared_write_methods: Vec<String>`，Core 对同 `platform` 的其它实例广播。
  需修订冻结项（contribution.rs:78-100 字段集）+ 插件发版；Core 仍持有「多实例同步」语义。
- 案 b 删广播、插件侧持久化：插件写共享段后其它实例需自行感知——但各实例是**独立进程**，插件侧感知要么文件 watch 要么 Core 转发，复杂度转移。
- **推荐 a 的无冻结变体 a′**：广播目标改为「`platform()` 与源实例相同」的其它实例（去掉 `"qq"` 字面量），方法集从
  `AdapterContribution.capabilities.gating` 推导：gating=true 的适配器其 `set_gate_mode/update_allowlist/update_denylist/set_owner`
  属「平台共享配置」。这四个方法名本身已是通用管理词汇（冻结 §四）。不改冻结接口、不需插件发版。
  风险：将来出现「每实例独立门控」的平台会被误广播 → 届时再升级为案 a（显式声明），本文记为已知限制。
  测试：:861 改写为「同 platform 两实例收到广播、异 platform 不收」。

**D.3 core/src 工具与实例**
- `qq_tools.rs`→`adapter_tools.rs`，`register_qq_tools_multi`→`register_adapter_tools_multi`，`QqInstanceSet`→`InstanceSet`；
  工具名（send_group_msg 等）已通用不变；注释/错误串中 "QQ" 改「适配器」。测试 mock 的 `"qq"`（:752-771）属测试豁免，建议改 `"mock"`。
- `qq_instances.rs`：端口分配/compose（:178-229 `render_compose` 渲染 NapCat）本质是 QQ 部署知识 → **移入插件仓库**
  （adapter-plugin.md:368「compose 渲染随插件」）；Core 只留通用 `PluginInstance{id, persona}`（:27 `QqInstance` 泛化）。
  风险：compose 移出需插件提供「部署钩子」，属可选收尾，可先改名、后搬迁（S6 标可选）。

**D.4 main.rs 泛化**
- :829-857：以「插件配置中声明的实例表」（通用 `[adapters.<platform>.instances]`，**待验证**现 TOML 透传形状，cfg.qq_instances 于 :665-667）
  替代 `qq_instances` 匹配；未匹配实例的插件适配器默认 `persona = default`，**仍注册入站 worker 与工具**（去掉 "non-QQ adapter plugin?" 分支）。
- :938-962：`factory_qq_by_persona`→`adapters_by_persona`，`qq_default2`→`default_adapters`；按 `capabilities.send_text` 过滤可出站适配器，不看 platform。
- 风险：非 QQ 插件首次获得工具注册，LLM 工具集变化 → 加单测「无 send 能力的适配器不注册 send_*」。

**D.5 豁免口径**（grep 判据 adapter-plugin.md:424）：允许保留 ① `adapter_ops/legacy.rs` 与 echo-protocol 中 `#[deprecated]` 变体（N+2 前）；
② 测试函数名/fixture 数据；③ `document/` 与 CHANGELOG；④ 插件仓库（不在 Core/Panel 全仓范围）。
验收命令：`grep -rniE 'qq|onebot|napcat' source --include=*.rs | grep -vE 'legacy\.rs|#\[cfg\(test|/tests/|deprecated'` 为空（N+2 时去掉 legacy/deprecated 豁免）。

## E. 实施切片

测试基线：core `cargo test --workspace`；插件 `cargo test --workspace`；Panel 由 02/03 负责。

| 片 | 文件 | 要点 | 验收 | 依赖 / 并行 |
|---|---|---|---|---|
| S1 | P:adapter/qq/mod.rs（tests）、P:plugin.rs | 补测试：string id 与 number id 等价（allowlist/owner）；断言 get_groups id 为 string | 插件 `cargo test --workspace` | 无；可最先、与全部并行 |
| S2 | C:protocol/echo-protocol/src/{command.rs,event.rs}，新 `id_compat.rs` | A 表改型；命令双读 deserialize_with；改 :585/:628 测试 + 新增 number/string 双读用例 | `cargo test -p echo-protocol` | 无；**合入不发布**，须与 S3 同 PR |
| S3 | C:backend/echo-agent/src/packages/adapter_qq/commands.rs | `id_name_list`/`parse_ids`/owner 转 String；SetQqOwner 空串语义；dual_run 断言改 string | `cargo test -p echo-agent` + workspace | S2 |
| S4 | C:core/src/qq_tools.rs | `arg_id()` 宽容解析（不 breaking） | `cargo test -p <core crate> qq_tools` | 无；可并行 S2 |
| S5 | C:packages/adapter_qq→adapter_ops（mod/commands/bridge + legacy.rs）、agent/commands.rs:1495 分发 | D.1 改名 + 回退泛化；D.2 a′ 广播（同 platform + gating），改写 :861 测试 | workspace 绿 + 新增「异 platform 不广播」用例 | S3 |
| S6 | C:core/src/qq_tools.rs→adapter_tools.rs、qq_instances.rs、main.rs:829-857/938-962 | D.3/D.4 改名与 persona 泛化；compose 搬迁（可选，独立 PR） | workspace 绿 + 「无 send 能力不注册」单测 | S5 后；compose 搬迁依赖插件仓 |
| S7 | echo-protocol + agent | 版本 N 发布：旧变体加 `#[deprecated]` + 收到旧命令 warn；发行说明写入 C 节内容 | workspace 绿；与 Panel 阶段 1 **同 tag** | S2-S5 + Panel 阶段 1 |
| S8 | B 节全部（command.rs/event.rs 变体、legacy.rs、agent/mod.rs:1230-1255、dual_run 测试→golden） | N+2 物理删除 | workspace 绿 + D.5 grep 去豁免为空 | S7 后两个版本；Panel 无旧引用 |

**发布顺序**：S1/S4 随时 → S2+S3 同 PR → S5 → S6 → S7（与 Panel 阶段 1 同批）→ N+1 观察 → S8。
**已知限制/假设**：版本号 N 待发布负责人指定；D.2 a′ 假设 gating 平台的门控配置跨实例共享（现仅 QQ 一例，commands.rs:482 注释）。
