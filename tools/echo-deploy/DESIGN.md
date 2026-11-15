# echo-deploy 设计说明：契约、五问、信任边界与运行手册

本文描述 `tools/echo-deploy` **当前代码里存在的契约**，用于评审与操作，不作为交付声明。

- 代码是唯一权威；本文若与代码冲突，以代码为准，并在完成时同步本文。
- 文档把状态分三档：**已实现**（有代码 + 测试覆盖）、**已接线/待接线**（代码存在但 CLI 主流程
  尚未调用，或反过来）、**仅计划**（只渲染 JSON，不执行）。
- **没有**"全部已交付"这种结论。真实首装/升级/回滚的验收记录仍待补；`systemd_live` 是
  opt-in 的临时实例测试，用的 Core 是 fixture。
- 全文四段：一 范围与现状、二 契约与五问、三 信任边界、四 运行手册；附录是收尾清单。
  第二段每个机制都回答同一组**五问**：事务单位 / 冲突 / 失败与崩溃 / 谁能运行 / 不能证明什么。

---

## 一、范围与现状

### 1.1 目标

1. 用**显式输入**（不带默认值、不猜 `$HOME`）描述一次发布：文件、摘要、拓扑、实例；
2. 把已构建产物变成可校验的候选，并放进私有候选库；
3. 把"装文件"、"切换服务"、"验证就绪"、"失败回退"拆成可观察、可恢复的步骤；
4. 任何时刻都能回答：**现在磁盘上是什么、上次事务走到哪、剩余证据在哪**。

### 1.2 非目标（明确不做）

- 不编译、不测试、不下载、不安装系统包、不 clone 仓库；
- 不调用 docker / docker compose；
- 不写 Core/Panel 的配置，不迁移数据，不删除用户数据；
- 不把 release 里的任何文件当可执行适配器（适配器必须由管理员单独提供）；
- 不提供"一键无值守升级"；更新请求必须被显式冻结并 `--confirm`；
- 不做多节点编排执行（只有 `operations::fleet` 的计划结构，且没有 CLI 入口）。

### 1.3 术语

| 术语 | 含义 |
|---|---|
| Release / 发布包 | 一个含 `release.json` + 产物的目录；`release.json` 是严格 JSON，逐文件 SHA256 |
| 候选库（store） | 私有目录，每个直接子目录是一个已校验 release；含 `store-format`/`store-lock` |
| selector | 指向 `<store>/<release_id>` 的符号链接；unit 不内嵌 release 目录，只认它 |
| 事务（transaction） | `<state_dir>/transaction-<instance>.json` 描述的状态机 |
| 适配器 | 管理员拥有的可执行文件；协议 `echo-deploy-hook-v1`（通用）或 `echo-deploy-control/1`（systemd/runtime） |
| 证据 | 由工具写出的、可复核的 JSON：previous-release、first-install、快照、evidence |
| attestation | 操作者声明（例如"外部写者已停"），工具无法独立证明 |

### 1.4 当前状态矩阵

| 面 | 实现 | 测试证据 | 未覆盖 |
|---|---|---|---|
| pack/verify/trust/stage | 完整 | 单测 + 真实 OpenSSL 路径 | 密钥轮换、防回退、渠道签名策略 |
| install-plan 渲染 | 完整 | 逐字节快照断言 | 渲染≠安装；`--output` 只写新计划目录 |
| install-files/uninstall-files | 完整 | 临时目录 + 失败注入 | 仓库内测试不碰真实 unit 目录；CLI 无 `ServiceStopped` 后端 |
| 通用事务 apply/recover | 完整 | 假 adapter、假 transport | 适配器实现质量不可证 |
| systemd 事务 | 完整 | `systemd_live`（`#[ignore]`，临时实例） | 生产 Core、生产单元；首装仅记录 |
| systemd_activation 快照/恢复 | 完整，已接 `systemd-recover` | 单测 | `systemd-apply` 传 `raw_config=None`（见附录） |
| runtime-adapter | 完整（含首装证据、`required_modules` 校验） | fake socket/evidence；opt-in `core_live` 对真实已构建 Core 做协议 smoke | 未做真实部署验收；旧 Core 无 `module_state` 时仅 `required` 为空可用 |
| Docker/多节点 | 仅渲染计划 | 计划字段断言 | 无执行、无回滚屏障 |

---

## 二、契约与五问

### 2.1 发布包与候选库

#### 契约要点

- `Release`（`deny_unknown_fields`）：`schema_version=1`、`release_id`、`target_os=linux`、
  `target_arch∈{x86_64,aarch64}`、`management_protocol`、`development`、`sources[]`、`artifacts[]`。
- `artifacts[].path` 是包内相对路径（`safe_id` 每段、≤64 段、非 `release.json`）；
  `executable` 必须等于 `kind != web`；web 允许 0 字节；总量 ≤32GiB。
- 文件/目录不许冲突（`bin/core` 与 `bin/core/x` 同时出现即失败）。
- `Deployment.check_release`：恰好 1 个 Core；`core_only` 无 panel/web；`embedded` 无 panel、
  至少 1 个 web；`standalone` 恰好 1 个 panel + 至少 1 个 web。
- `pack`：模板 `artifacts` 必须为空；`core/panel` 输入是单个文件，落到
  `bin/echo-agent-core`/`bin/echo-agent-panel`；`web` 输入是目录，**完整递归枚举**（无忽略规则）；
  `module` 直接报错（模块协议契约未定）。
- `verify --require-complete`：包内实际文件集合必须等于 `release.json + artifacts`（无多余文件），
  因此签名文件必须在包外。
- `stage`：未签名路径要求 `release.development && --allow-development`；签名路径要求
  `VerifiedManifest.bytes() == bundle manifest bytes`（签名覆盖的就是冻结字节）。
- 候选发布用 `renameat2(RENAME_NOREPLACE)` 到 `<store>/<release_id>`；已存在即失败；
  完成候选文件 0444/0555、目录封存；崩溃可能留下 `pending-*`，工具不递归清理。

#### 五问

| 问题 | 回答 |
|---|---|
| 事务单位是什么 | 一次 `pack`（新目录）/ 一次 `stage`（一个候选目录 + 一次库内 rename）；没有跨库事务 |
| 冲突怎么办 | 目标已存在即失败（NOREPLACE）；`init-store` 目标存在即失败；不覆盖、不合并 |
| 失败/崩溃怎么办 | 新目录可能残留但从未被引用；`pending-*` 保留待人工核对；已发布候选不可变（纪律上只读） |
| 谁能运行 | 能读写库目录的本用户；库必须 0700、属当前 euid、本地可信 FS |
| 不能证明什么 | 源码闭包完整、构建可复现、`revision/snapshot_sha256/lock_sha256` 的真实性；同 UID/root 篡改 |

### 2.2 安装渲染与文件落地

#### `InstallSpec`（严格 JSON）

```json
{
  "instance_id": "local-dev",
  "scope": "user",
  "topology": "core_only",
  "release_root": "/abs/state/echo-agent-core/deploy/current",
  "core_config": "/abs/config/echo-agent-core/core.toml",
  "panel_config": null,
  "deploy_executable": "/abs/libexec/echo-deploy/echo-deploy",
  "state_dir": "/abs/state/echo-agent-core/deploy",
  "deployment": {
    "schema_version": 1,
    "core_root": "/abs/share/echo-agent-core/source",
    "launcher_directory": "/abs/.local/bin",
    "release_store": "/abs/state/echo-agent-core/releases",
    "instance_state": "/abs/state/echo-agent-core/runtime",
    "core_socket": "/abs/state/echo-agent-core/runtime/core.sock",
    "runtime_config": "/abs/state/echo-agent-core/runtime/runtime-adapter.json",
    "panel_gate_file": null,
    "required_modules": ["echo-agent.session"],
    "acknowledge_external_writers_stopped": false
  }
}
```

`deploy_executable` 兼容别名 `executable`/`deployment_tool`。所有路径必须绝对、规范化、
UTF-8、无控制字符；`release_root` 与 `deploy_executable` 还要满足 systemd
`ExecStart` 首 token 规则（拒绝 `"`、`'`、`\`）。

两种渲染模式：

| | 无 `deployment`（历史模式） | 有 `deployment`（opt-in） |
|---|---|---|
| Core `WorkingDirectory` | `release_root`（具体 release 目录） | `core_root`（受管源码检出） |
| Core `ExecStart` | `<release_root>/bin/echo-agent-core --config <core_config>` | 受信 launcher（`<launcher_directory>/<instance>`） |
| release 身份 | 目录即身份 | launcher 从 selector 推导 `ECHO_RELEASE_ID`（**声明**） |
| 控制 socket | 不注入 | `ECHO_DEPLOY_CONTROL_SOCKET=<core_socket>` |
| 升级方式 | 换目录 + 重写单元 | 换 selector，不重写单元 |

两种模式都注入 `ECHO_AGENT_INSTANCE`/`ECHO_AGENT_SCOPE`/`ECHO_AGENT_STATE_DIR`；
deployment 模式额外注入 `ECHO_SERVICE_SCOPE`（Core 自更新模块读它决定 systemctl 作用域）
与 `ECHO_UPDATE_STATUS_FILE=<state_dir>/update-status`。

渲染文件：`<instance>-core.service`、`<instance>-panel.service`（standalone）、
`<instance>-update.service`（`Type=oneshot`，`ExecStart=echo-deploy apply-request
--request <state_dir>/update-request.json --confirm`，`TimeoutStartSec=1800`）、
launcher（0755，`#!/bin/sh`，`set -eu`）。

deployment launcher 的顺序很关键：

1. `ECHO_DEPLOY_CONTROL_PROTOCOL=echo-deploy-control/1` → `exec echo-deploy runtime-adapter
   --config <runtime_config>`（**先于** selector 解析，所以首装也能 Observe）；
2. 解析 selector，校验目标是 store 下唯一 release 目录、`bin/echo-agent-core` 可执行；
3. 导出 selector 推导的 `ECHO_RELEASE_ID`（声明，不是 build ID）；
4. 参数含 `-c`/`--config`/`--config=…` 就透传，否则补 `--config <core_config>`。

#### 文件落地：`install-files`

```json
{ "schema_version":1,"outcome":"files_installed_not_activated","partial":false,
  "instance_id":"local-dev","scope":"user","topology":"core_only",
  "unit_directory":"/abs/.config/systemd/user","launcher_directory":"/abs/.local/bin",
  "units":["echo-agent-local-dev-core.service","echo-agent-local-dev-update.service"],
  "files":[{"path":"/abs/.config/systemd/user/echo-agent-local-dev-core.service","kind":"unit",
            "sha256":"<64 hex>","size":1234,"mode":420,"uid":1000,"gid":1000}] }
```

（上面是**台账** `OwnershipLedger` 的形状：`schema_version/outcome/partial/instance_id/scope/
topology/unit_directory/launcher_directory/units/files[]`；`outcome` 恒为
`files_installed_not_activated`，`partial=true` 表示失败后幸存文件。）

流程：校验 spec 与两个已存在目录 → 校验台账目标（绝对、单文件名、父目录私有且属当前 euid、
不与输出重叠、不存在）→ **先 fsync 一份 intent 台账** → 逐个 `O_EXCL` 创建 + 校验哈希 →
fsync 文件与目录 → 原子替换成 final 台账。任何一步失败：只回滚本次创建且哈希仍匹配的文件，
幸存文件写回 partial 台账（写失败则保留 intent，它是最小证据的超集）。

- `user`：两个目标目录必须属当前 euid、不可 group/other 写；`system`：必须 euid 0、目录 root 所有。
- 永不覆盖：目标已存在（普通文件、链接、目录、设备）都是硬错误。
- 本模块永不调用 `systemctl`/`systemd-analyze`/`kill`；`daemon-reload`、`enable`/`disable` 都是操作者动作。
  （`systemd-apply` 也只调用 `start`/`stop`/`show ActiveState`，从不让本工具 enable 单元。）

#### 文件卸载：`uninstall-files`

- 只接受台账；`--offline-only`（CLI 唯一选项）或库里的 `ServiceStopped` 后端，二者必选其一。
- 先整体预检：任一目标被修改/替换/父目录缺失/非普通文件 → 一个文件都不删。
- 只删台账内、普通单链接、内容/大小/模式/uid/gid 仍匹配的文件；
  不删目录、配置、数据、release 树；缺失项记为 `already_absent` 不算失败；
  状态恒为 `managed_files_removed_services_not_touched`。

#### 五问

| 问题 | 回答 |
|---|---|
| 事务单位是什么 | 一次 `install-files` = 一个 spec 渲染 + 两个目标目录 + 一个台账文件；一次 `uninstall-files` = 一个台账 |
| 冲突怎么办 | 任何目标或台账冲突都在写第一个文件前失败；目标发布还有 `RENAME_NOREPLACE` 兜底 |
| 失败/崩溃怎么办 | 崩溃可能留下部分安装，但**不存在没有所有权证据的受管文件**（intent 先持久化）；回滚只删仍匹配的文件 |
| 谁能运行 | user：目标目录属当前 euid；system：euid 0 且目录 root 所有；台账父目录必须私有 |
| 不能证明什么 | 文件已加载/已启用/已运行；外部同名文件的来源；同 UID/root 后续修改 |

### 2.3 部署事务

#### 两种执行文档（二选一，由 JSON 形状判定）

**A. 通用适配器 `Execution`**（`deny_unknown_fields`）：

```json
{"schema_version":1,
 "deployment":{"schema_version":1,"instance_id":"local-dev","topology":"core_only",
               "scope":"user","target_os":"linux","target_arch":"x86_64",
               "store":"/abs/state/echo-agent-core/releases","required_modules":[]},
 "state_dir":"/abs/state","adapter":"/abs/admin-adapter","timeout_seconds":120,
 "reversible_data":true}
```

**B. systemd `SystemdExecution`**（顶层恰好 9 个字段，**没有** `schema_version`，
`panel_unit` 必须显式出现，未知字段拒绝）：

```json
{"deployment":{"schema_version":1,"instance_id":"local-dev","topology":"core_only",
               "scope":"user","target_os":"linux","target_arch":"x86_64",
               "store":"/abs/state/echo-agent-core/releases","required_modules":[]},
 "state_dir":"/abs/state/echo-agent-core/deploy",
 "unit_directory":"/abs/.config/systemd/user",
 "current_selector":"/abs/state/echo-agent-core/deploy/current",
 "core_unit":"echo-agent-local-dev-core.service",
 "panel_unit":null,
 "control_adapter":"/abs/.local/bin/local-dev",
 "timeout_seconds":120,
 "reversible_data":true}
```

校验：unit 名必须由 `instance_id` 推导；`current_selector` 是 `state_dir` 后代且文件名是
`safe_id`；`unit_directory != state_dir` 且在候选库外；`control_adapter` 不在候选库内；
`timeout_seconds ∈ 1..=600`；`reversible_data` 必须为 `true`（否则拒绝自动回滚路径）。

`status --execution` 同时接受两种文档（`untagged`），只读事务记录。

#### 状态机与提交边界

`Phase`：`Prepared → Maintenance → Drained → Switching → Verifying → Committed`，
外加 `RollingBack → RolledBack` 与 `RecoveryRequired`。每个 phase 都是**先持久化意图，
再调用后端**；崩溃后由 `recover` 观察真实后端来消歧，而不是猜测。

顺序（systemd 后端）：

1. `observe`（preflight，要求 current==previous、非维护）；
2. `prepare`：读候选 `release.json`、校验摘要/拓扑；已有安装必须先持久化旧 release 的
   `previous-<hex>.json` 证据（`hex` = SHA-256(instance\0transaction_id)）；首装则先跑"selector 不存在 + 单元全部 inactive"并写
   `first-install-evidence.json`；随后让控制适配器 `PrepareUpgrade` 并重新 observe 确认真实状态未变；
3. `enter_maintenance`（Panel gate / 适配器状态）；
4. `drain`（检查式排空，报告剩余工作数，从不 kill）；
5. `switch`：**先停单元并观察到 inactive，再原子替换 selector**（首装用 NOREPLACE 创建）；
6. `start`：selector 已指向目标后再启动；
7. `ready`：向适配器发 `Ready`，并用 `/proc/<pid>/exe` 的 SHA-256 对清单核验；
   随后再 observe 一次，要求 `current_release==target && ready`；
8. **提交边界**：就绪观测通过后先把 `Committed` 持久化，再 `release_maintenance`
   （适配器在这一步仍会要求 Core ready）。该步失败记 `RecoveryRequired`，
   **不回滚数据**；提交前的任何中断才走幂等回滚到 `previous_release`。

误判说明：`Committed` 只代表"目标 release 已被观测为 ready"，不代表数据迁移可逆。

#### 快照与离线恢复（`systemd_activation`）

`systemd-apply` 在 begin 之后、执行之前构造 `SystemdActivationContext`：

- 用通用 `ActivationContext` 承担所有既有边界（私有 state/store、无符号链接祖先、受信适配器、
  cgroup 守卫、签名或显式 development 授权）；
- 额外对**完整** `SystemdExecution` 做 canonical（紧凑 serde）字节的 SHA-256，因此
  `unit_directory`/`current_selector`/`core_unit`/`panel_unit` 这些"通用投影里没有"的字段
  一旦漂移就被发现；
- `prepare_snapshot` 落两类目录：`snapshot-<32hex>/`（`execution.json`、`snapshot.json`、
  受信适配器的**不可变副本** `adapter`）与 `systemd-snapshot-<32hex>/`
  （`systemd.json` 全量配置、`evidence.json` 绑定哈希）；
- 后续事务后端从 `pinned_systemd_execution()` 构造，`control_adapter` 指向快照里的副本，
  **不会**执行管理员可变路径。

`systemd-recover`：

1. 读 `SystemdExecution`（严格校验）；
2. 打开事务存储并取 `transaction-lock`（flock 非阻塞，忙即失败）；
3. `CgroupGuard::current`（拒绝从 Core/Panel cgroup 运行）；
4. `open_recovery(&execution, &saved)`：**不需要 trust store**，锚点是 durable transaction 的
   `manifest_sha256` + 既有快照/证据哈希；
5. `load_snapshot_recovery`：缺失/被改的快照或证据直接失败，**绝不重建**；
6. 用 pin 后端 `store.recover(...)`；结果若为 `RecoveryRequired` → 打印事务并以非零码退出。

通用 `recover` 同理，但可额外带 `--trust-store/--key-id/--signature` 做一次额外认证；
不提供签名时仍可恢复，因为恢复不是"重新授权一个候选"，而是完成一个已经持久化的事务。

#### 证据文件清单（`state_dir` 内，除注明外均 0600）

| 文件 | 内容 |
|---|---|
| `transaction-<instance>.json` | 事务状态机（phase、target/previous、step 证据、错误） |
| `previous-<hex>.json` | 旧 release 的不可变身份；`hex`=SHA-256(instance\0transaction_id)（首装无） |
| `first-install-evidence.json` | 首装观测：selector 缺失 + 单元 inactive + 时间戳 |
| `snapshot-<32hex>/` | 通用快照 + `adapter` 副本 + `execution.json` |
| `systemd-snapshot-<32hex>/` | `systemd.json` + `evidence.json` |
| `transaction-lock` | 部署事务 flock |
| `<instance_state>/runtime-adapter-state.json` | 适配器维护/已准备状态 |

#### 五问

| 问题 | 回答 |
|---|---|
| 事务单位是什么 | 一个 instance 一个事务文档；`systemd-apply` 一次 = 一个 target release 的完整切换 |
| 冲突怎么办 | 事务进行中再启动：state flock 直接失败；selector/target 身份不符：preflight 失败并进入恢复；旧证据不一致：拒绝覆盖 |
| 失败/崩溃怎么办 | phase 是意图，恢复靠 observe 真实后端；提交前幂等回滚，提交后只重试解除维护；证据缺失即 fail-closed |
| 谁能运行 | 持有 state/store 权限的管理员；必须不在 Core/Panel cgroup；`system` 需 root |
| 不能证明什么 | 适配器 ACK 的真实性；数据可逆性；Panel 二进制身份；"错误就代表没写入"（rename 后 fsync 失败需现场核对） |

### 2.4 运行时控制适配器

#### 协议

`runtime-adapter --config <config>`：stdin 一行 JSON（≤64 KiB），stdout 一行 JSON。
`protocol=echo-deploy-control/1`，`instance` 必须匹配，`operation` 属于
`Observe|PrepareUpgrade|Drain|MaintenanceEnter|MaintenanceLeave|Ready`
（线上值是 snake_case：`observe`/`prepare_upgrade`/`drain`/`maintenance_enter`/
`maintenance_leave`/`ready`）。
回复是严格 `ControlReply`（`deny_unknown_fields`）：`protocol/operation/instance/success` +
可选 `current_release/maintenance/ready/active_work/error`。

- `Observe`：selector 读到什么就报什么；socket 缺失 → `ready=false`（但 selector 声明仍返回）。
  Core 在跑时还要比对"Core 声明 == selector"，并核验运行中可执行文件哈希。
- `PrepareUpgrade`：先独立校验候选（目录非链接、`release.json` 摘要、Core 产物大小/可执行位/哈希），
  再要求 Core `prepare`；只有"selector 不存在且缺 socket"时才可能走首装证据路径。
- `Drain`：要求 Core 已 prepare、持有维护、generation 匹配；报告剩余工作数，不 kill。
- `MaintenanceEnter/Leave`：维护真相来自 Panel gate（若有）否则适配器状态；`Leave` 要求
  Core idle、声明==selector、并再次 `ready` 且核验 `/proc/<pid>/exe`。
- `Ready`：selector == 请求 release；请求若带 `manifest_sha256` 必须匹配；
  `/proc/<pid>/exe` 哈希 == 清单 core 哈希，再要求 Core `ready`，最后复核进程未变。

#### 首装

没有 selector、没有 socket 并不等于"空安装"。入口条件：

1. `first_install` 配置存在（否则缺 socket 一律失败）；
2. 后端写出 `first-install-evidence.json`：`source=systemd`、`selector_absent=true`、
   每个规范单元 `active_state=inactive`、`observed_at_unix_ms`、目标 release/摘要；
3. 适配器校验 schema/source/instance/unit 名/时间新鲜度（≤600s，默认 300s，
   允许 ≤30s 时钟偏移），并**独立复读**真实 selector 与 socket；
4. 适配器状态里记录 `installed`；一旦记录过安装，后续 selector 消失也不能再走首装。

首装回滚只移除本事务创建、且仍指向本事务目标的 selector 符号链接；并释放维护门/状态；
它绝不删除任意路径。

#### Panel 维护门

`panel-gate.json`：`schema_version=1`、`instance_id`、`maintenance`、`release_id`、
`management_protocol`。适配器会翻转 `maintenance`，成功解除维护时把 `release_id` 更新为
当前 selector 的 release。`release_id`/`management_protocol` 都是**部署声明**，适配器只校验
形状与 instance，不把 release id 当自证。Panel 侧是否真的消费该文件属于**外部仓库契约**，
本仓库没有验证。

#### `required_modules` 校验（已实现并验证）

`control_client::verify_required_modules` 在 `runtime_adapter::query` 的 Status/Ready 分支被调用，规则是 fail-closed：

- 响应里既无 `modules_verified` 也无 `module_state`（旧 Core）：只在 `required` 为空时接受；
  一旦要求模块就拒绝——"没有证据"不等于"通过"。
- 只有一半字段：直接拒绝（不接受部分证据）。
- `modules_verified` 与 `module_state.verified` 必须都为 `true` 且一致；
  `modules[]` 的 `mounted`/`active` 必须与集合字段逐项吻合；`active ⊆ mounted`；
  重复 id、`active` 带 `error`、`active` 但 runtime state 非 `ready` 都算冲突。
- 每个 `required` 模块必须在 live facts 中出现、`mounted && active`、state 为空或 `ready`，
  且不在 `missing_required` 里；`missing_required` 与 live active 矛盾也算冲突。
- Ready 的 `checks.modules_verified` / `checks.required_modules_mounted` 必须成对且 `true`；
  顶层 `ready=true` 不能覆盖一个 `false` 的 check。

仍未覆盖：适配器不在请求里发送 `required_modules`（它用 Core 返回的完整 `module_state` 本地比对），
因此适配器本地核对完整证据，不依赖 Core 按请求筛选；旧 Core 缺证据、半字段、false/冲突、缺模块均有行为测试。实际 Core 的 required 模块证据另见 core_live；完整跨版本矩阵仍未覆盖。

#### 五问

| 问题 | 回答 |
|---|---|
| 事务单位是什么 | 一次控制请求（一行）；持久状态在 `<instance_state>/runtime-adapter-state.json` |
| 冲突怎么办 | 请求与配置 instance 不符、release 已选中、维护未释放、状态里已 prepare 别的目标：全部拒绝 |
| 失败/崩溃怎么办 | 状态先写 0600 临时文件再 rename；维护状态可从 gate 重读；重复的 `Leave` 在完全静止时才幂等成功 |
| 谁能运行 | 由受信 launcher 在控制协议环境变量下 exec；socket 父目录私有、同 uid `SO_PEERCRED`；state 目录私有 |
| 不能证明什么 | Core 的 `ECHO_RELEASE_ID` 声明（只有 `/proc/<pid>/exe` 哈希算证据）；外部写者已停；Panel 是否真的进入维护 |

### 2.5 跨文档一致性（工具**不**校验的等式）

这些字段分属不同文档，代码各自校验，彼此不交叉核对。运维必须保证一致，否则会出现
"事务指向 A、适配器读 B"的分裂：

| 等式 | 涉及文档 |
|---|---|
| `InstallSpec.state_dir == SystemdExecution.state_dir` | install / systemd execution |
| `InstallSpec.release_root == SystemdExecution.current_selector == RuntimeConfig.current_selector` | 三处 |
| `InstallSpec.instance_id == SystemdExecution.deployment.instance_id == RuntimeConfig.instance_id` | 三处 |
| `Deployment.store == DeploymentRuntime.release_store == RuntimeConfig.release_store` | 三处 |
| `DeploymentRuntime.instance_state == RuntimeConfig.instance_state`；socket/config/gate 是其直接子文件 | 两处 |
| `RuntimeConfig.first_install.evidence == <state_dir>/first-install-evidence.json` | 后端固定名 |
| `SystemdExecution.control_adapter == <launcher_directory>/<instance>` | 渲染与执行 |
| `SystemdExecution.core_unit/panel_unit == RuntimeConfig.first_install.core_unit/panel_unit` | 两处 |
| `DeploymentRuntime.launcher_directory == install-files --launcher-directory` | 两处 |

建议后续加一个 `echo-deploy doctor --spec/--execution/--runtime` 的一致性检查命令；
当前只能人工对照本表（见附录）。

---

## 三、信任边界

### 3.1 信任链上有谁

1. **发布签名密钥**：离线 Ed25519，公钥在显式 trust 目录（0700、属当前用户、`trust-format`
   精确为 `echo-deploy-trust-v1\n`、公钥名 `<key-id>.ed25519.pem`）。工具只读公钥，
   私钥永不进入 bundle 或目标机。
2. **管理员适配器**：通用 `adapter` 或 systemd `control_adapter`。它们**不是**发布包的一部分；
   工具校验"回执契约"（回显上下文、`success=true`、结构化 evidence），但**无法证明**
   适配器没有谎报。禁止用伪 ACK 接生产。
3. **操作者 attestation**：`acknowledge_external_writers_stopped`、`--offline-only`、
   `reversible_data` 都是声明，不是实测。
4. **CGroup 守卫**：生产激活/恢复拒绝从 `echo-agent-<instance>-core/panel.service` 或旧
   `echo-agent-core/panel.service` 的 cgroup 里运行——升级器必须与 Core/Panel 处于独立 cgroup。
5. **Core 进程**：唯一能被工具用字节核验的对象（`/proc/<pid>/exe` 的 SHA-256 vs 清单）。

### 3.2 `release_id` / `management_protocol` 不是自证

- `Release.management_protocol` 是整个包（Core + Panel + Web）的**组合声明**；
  同包不等于运行时真的互通，兼容性需要握手/集成测试，工具不提供证明。
- `ECHO_RELEASE_ID` 由 launcher 从 selector 推导，只是**声明**；Core 的 `release.identity_verified`
  恒为 `false`。真正的证据是控制器对运行中可执行文件的哈希核验。
- Panel 维护门的 `release_id`/`management_protocol` 同样是声明；适配器只校验形状与 instance。
- 因此："selector 指向 X + Core 声明 X + 适配器 ACK" 仍**不是**"运行的是 X 的字节"；
  后者只在 `ready`/`Observe` 完成 `/proc/<pid>/exe` 哈希比对后成立。

### 3.3 外部写者：Core ACK 覆盖不到的地方

Core 的 `prepare` ACK 明确只覆盖它自己控制的写者（global gate 的管理命令写守卫、在飞 turn、
周期会话保存与模块热重载、guard 下的最终 checked flush）；
**用户通过 shell/子进程启动的任意写者不在锁内**。因此：

- 适配器在 `PrepareUpgrade`/首装 `Drain` 处强制要求
  `acknowledge_external_writers_stopped=true`，否则直接拒绝；
- 这个字段只该在操作者**真的停掉**这些写者之后置 `true`；
- 工具无法枚举或杀死这些进程，也不会尝试。

### 3.4 回滚是组合回滚，不是数据回滚

- 事务回滚 = 停单元 → selector 指回旧 release → 启动 → 用旧清单摘要核验 → 解除维护。
- 不做数据库/会话/媒体/配置的逆向迁移；`reversible_data=true` 只是"允许自动回滚"的策略声明。
- 提交后（`Committed`）的失败**不会**自动恢复旧数据，只重试幂等的解除维护。
- 不可逆的数据迁移必须走人工流程：不要依赖本工具的自动回滚，也不要把它写进 runbook。

### 3.5 其它边界

- 候选库、trust 目录、state、instance_state 都要求私有（0700）且属当前 euid；
  同 UID 或 root 的替换/读取不在威胁模型内，NFS/恶意挂载也不保证。
- 候选完成后的 0444/0555 是纪律性只读；`RENAME_NOREPLACE` 只防覆盖，不防同 UID 改写。
- `rename` 后 `fsync` 失败意味着"不确定"，必须现场核对，不能宣称"报错就等于没写"。
- 测试全部使用临时目录、假 transport、假 adapter、fixture Core：
  **不接触生产配置、模型和网络**，也不能替代生产验收。

---

## 四、运行手册

下面以 `instance_id=local-dev`、`scope=user` 为例。所有路径替换为你的绝对路径。
任何一步失败都不要跳步，先看 `status` 与证据文件。

### 4.0 前置检查

```bash
/abs/libexec/echo-deploy/echo-deploy --version   # 二进制存在且可执行
umask                                            # 期望 077
systemctl --user is-system-running               # user 作用域；system 用系统 manager
# 确认当前不在生产 Core/Panel 单元里（输出应为空；否则 systemd-apply 会拒绝）
grep -E 'echo-agent-(core|panel)\.service' /proc/self/cgroup || true
```

`user` 作用域还需 `loginctl enable-linger $USER` 才能开机自启（工具不执行）。
`system` 作用域所有后续命令都要 root。

### 4.1 目录与配置文件准备

```bash
umask 077
install -d -m 0700 /abs/state/echo-agent-core/deploy \
                   /abs/state/echo-agent-core/runtime
# 候选库必须由 init-store 创建：目标目录创建前不能存在（父目录已存在即可）
echo-deploy init-store --store /abs/state/echo-agent-core/releases
# 确认目录权限（期望 drwx------ + 属当前用户；system 作用域属 root）
ls -ld /abs/state/echo-agent-core/deploy \
       /abs/state/echo-agent-core/runtime \
       /abs/state/echo-agent-core/releases \
       /abs/trust
```

需要人工写的四个文档（工具只渲染/校验，不代写）：

1. `install.json`（`InstallSpec`，见 2.2）
2. `systemd-execution.json`（`SystemdExecution`，见 2.3；**不要**加 `schema_version`）
3. `/abs/state/echo-agent-core/runtime/runtime-adapter.json`（0600，见 README 示例）
4. `panel-gate.json`（仅 standalone）：`{"schema_version":1,"instance_id":"local-dev",
   "maintenance":false,"release_id":"<当前或目标 release>","management_protocol":"<声明>"}`
   —— 首次准备时可先写目标 release 的声明；成功解除维护后适配器会更新 `release_id`。

其它必须由你准备、工具不代劳的东西：

- `core_config`（以及 standalone 的 `panel_config`）必须已存在且内容有效——工具不改写配置；
  两者路径不得相同。
- `deploy_executable` 必须是已存在的可执行文件（launcher 用它分发 `runtime-adapter`，
  updater 单元也用它跑 `apply-request`）。
- `launcher_directory` 必须在 PATH 中（旧安装的 `~/.local/bin` 同理），否则用户命令找不到入口；
  但 systemd 单元用的是绝对路径，不依赖 PATH。
- `instance_state`、`state_dir`、候选库都必须已存在且 0700；候选库由 `init-store` 创建
  （它不改 mode，靠 `umask 077`，之后每次打开都会再次校验 0700）。

### 4.2 首装

```bash
# 1) 候选：pack → verify → stage
echo-deploy pack --template /abs/template.json --inputs /abs/outputs.json --output /abs/bundle-<rev>
echo-deploy verify --bundle /abs/bundle-<rev> --require-complete
echo-deploy stage --bundle /abs/bundle-<rev> --deployment /abs/deployment.json \
  --trust-store /abs/trust --key-id release --signature /abs/release-<rev>.sig
#    开发包去掉 trust 三件套，改用 --allow-development（要求 release.development=true）

# 2) 渲染 + 落文件（不启动）
echo-deploy install-plan --spec /abs/install.json --output /abs/plan-<ts>
echo-deploy install-files --spec /abs/install.json \
  --unit-directory /abs/.config/systemd/user \
  --launcher-directory /abs/.local/bin \
  --ledger /abs/state/echo-agent-core/deploy/ledger-<ts>.json --confirm
systemctl --user daemon-reload          # 人工！不 reload 的话 systemctl show 找不到单元
systemctl --user enable echo-agent-local-dev-core.service   # 人工；本工具从不 enable
# standalone 追加 enable ...-panel.service；updater 是 oneshot，是否 enable 由你决定

# 3) 首装前自检：单元必须真的 inactive，selector 必须不存在
systemctl --user show -p ActiveState echo-agent-local-dev-core.service
test ! -e /abs/state/echo-agent-core/deploy/current && echo "selector absent: OK"
test -f /abs/state/echo-agent-core/runtime/runtime-adapter.json && \
  stat -c '%a %U' /abs/state/echo-agent-core/runtime/runtime-adapter.json   # 期望 600

# 4) 首装事务：从 Core cgroup 之外运行
systemd-run --user --scope --unit echo-deploy-first-<ts> \
  /abs/libexec/echo-deploy/echo-deploy systemd-apply \
  --execution /abs/systemd-execution.json --release <release_id> \
  --trust-store /abs/trust --key-id release --signature /abs/release-<rev>.sig --confirm

# 5) 核对
echo-deploy status --execution /abs/systemd-execution.json
readlink -f /abs/state/echo-agent-core/deploy/current
systemctl --user is-active echo-agent-local-dev-core.service
cat /abs/state/echo-agent-core/deploy/first-install-evidence.json   # 后端写、适配器已复核
```

首装判成功的最低证据集合：

- `status` 的 phase 为 `committed`，且没有残留 error；
- `current` 指向 `<releases>/<release_id>`；
- Core 单元 `active`，`ECHO_DEPLOY_CONTROL_SOCKET` 指向的 socket 存在；
- 适配器 `Observe` 返回 `ready=true`（可由后续 `systemd-apply` 的 preflight 间接验证）；
- `first-install-evidence.json` 存在且 `units[].active_state` 全为 `inactive`（这是**事前**观测）。

常见失败：

| 现象 | 原因 | 处理 |
|---|---|---|
| `first-install evidence requires an absent selector` | selector 已存在 | 不是首装，改用升级流程；或确认旧的 selector 是否误建 |
| `first install requires inactive unit …` | 单元还在跑/在 activating | 人工停机并确认 inactive；不要用 `reset-failed` 掩盖 |
| `first_install.evidence parent` / `private` | 证据父目录不是 0700 或属主错 | 修目录权限（state_dir 本身） |
| `control adapter …` 相关 | launcher 不可执行/可写/属主错 | 修 mode/owner；launcher 必须是 0755 且不可 group/other 写 |
| `refusing activation from … cgroup` | 在 Core/Panel 单元里执行 | 改用 `systemd-run --scope` 或独立 updater 单元 |

`system` 作用域差异：目录/台账/配置全部 root 0700；`daemon-reload` 用
`systemctl daemon-reload`（不带 `--user`）；执行同样要 `systemd-run --system --scope`。

### 4.3 升级

```bash
echo-deploy pack … && verify … && stage …        # 新 release 进候选库
echo-deploy install-plan --spec /abs/install.json  # 若单元内容没变可跳过
echo-deploy systemd-apply --execution /abs/systemd-execution.json \
  --release <new_id> --transaction tx-<ts> \
  --trust-store /abs/trust --key-id release --signature /abs/release-<new>.sig --confirm
echo-deploy status --execution /abs/systemd-execution.json
```

- 升级**不需要**重写单元：selector 变了，launcher 重新解析。只有渲染字段（超时、路径、
  环境变量）变化时才需要重新 `install-files`（注意：目标文件已存在 → 必须先人工删除旧文件
  或换目录；工具绝不覆盖）。
- 旧 release 的身份在 prepare 阶段被固化成 `previous-<hex>.json`；之后即使候选库里的
  旧目录被改，回滚也按固化的摘要核验，不会"按磁盘现状猜"。
- 升级失败但已回滚：`status` 显示 `rolled_back`，`current` 指回旧 release，服务应为旧版本。
- 失败且无法证明安全终态：`recovery_required` → 走 4.4，不要手工改 selector 假装成功。

### 4.4 中断恢复

```bash
echo-deploy status --execution /abs/systemd-execution.json    # 先看 phase 与错误
echo-deploy systemd-recover --execution /abs/systemd-execution.json --confirm
```

- **不需要** trust store：恢复锚定在持久事务的 `manifest_sha256` 与既有快照/证据上。
  带签名只是额外校验；恢复不是"重新授权一个新候选"。
- `systemd-recover` 会打开 `transaction-lock`（忙则失败，说明另一个操作在进行）、
  检查 cgroup、加载快照（缺失/被改即失败，绝不重建）、然后用 pin 的后端恢复。
- 提交前中断 → 幂等回滚；提交后中断 → 只重试解除维护，**不**恢复旧数据。
- 输出若仍是 `recovery_required`，命令以非零码退出：此时需要人工判断（例如维护门被外部改动、
  Core 无法 ready）。**不得**伪造"恢复成功"，也不得用开发授权掩盖生产失败。

通用适配器路径同理：`echo-deploy recover --execution /abs/execution.json --confirm`
（可带 trust 三件套做额外校验）。

### 4.5 卸载

```bash
# 1) 人工停服并确认 inactive（uninstall-files 不碰 systemctl；standalone 另加 ...-panel.service）
systemctl --user stop echo-agent-local-dev-core.service echo-agent-local-dev-update.service
systemctl --user show -p ActiveState echo-agent-local-dev-core.service

# 2) 只删台账受管文件
echo-deploy uninstall-files --ledger /abs/state/echo-agent-core/deploy/ledger-<ts>.json \
  --offline-only --confirm
systemctl --user daemon-reload      # 人工
```

- 只删台账内、仍匹配的文件；配置（`core.toml`/`panel.toml`）、会话、媒体、skills、
  候选库、release 树、state、证据都**保留**。
- 没有 `--purge`：要删数据请单独、显式、可审计地做。
- 想彻底清理时，先保留台账与证据目录作为审计材料，再人工决定。

### 4.6 Docker 与多节点（**仅计划**）

```bash
echo-deploy compose-plan --spec /abs/compose.json
```

- 镜像必须 `<repository>@sha256:<64 hex>`；tag/`latest` 一律拒绝。
- `config_dir`/`data_dir` 必须不同且互不包含；Compose 渲染为 pretty JSON（合法 YAML 1.2）。
- 计划里 `activation_supported=false`、`execution_supported=false`，四个屏障
  （runtime ACK / 维护门 / 就绪 / 回滚）都标 `implemented=false`；
  `whole_stack_rollback_supported=false`、`data_compatibility_checked=false`。
- 多节点：库里只有 `operations::fleet` 的计划结构（`parallelism=1`、`stop_on_failure=true`、
  `execution_supported=false`），**没有 CLI 子命令**。不要把它当编排器。

### 4.7 旧安装迁移（无自动接管）

旧安装（仓库根 `scripts/install.sh`）的布局与新工具不同：

| | 旧安装 | 新工具（deployment 模式） |
|---|---|---|
| 单元 | `echo-agent-core.service` / `echo-agent-panel.service` | `echo-agent-<instance>-core/panel.service` |
| 二进制 | `~/.local/libexec/echo-agent-core/echo-agent-core-bin` | `<store>/<release>/bin/echo-agent-core`（selector 指向） |
| 更新 | `scripts/update.sh` 构建后原子替换 | `systemd-apply` 事务 + selector |
| 回滚 | 安装脚本自己的快照目录 | 事务 + 证据文件 + selector |
| 配置 | `~/.config/echo-agent-core/core.toml` | 同一路径可继续用（本工具不改写） |

**没有自动接管**：`echo-deploy` 不会发现、停止、禁用或替换旧单元，也不会导入旧状态/回滚快照。
迁移必须是一个显式停机窗口：

1. 人工停用旧单元（`systemctl --user disable --now echo-agent-core.service` 等）；
2. 保留配置与数据；按 4.2 建新目录、跑首装；
3. 确认新实例 ready 后，再人工决定是否删除旧单元与旧二进制。

注意：生产 `CgroupGuard` 会拒绝从旧 `echo-agent-core.service` 里运行激活（development 入口
在迁移时容忍旧单元，但那是显式开发授权，不能拿来冒充生产）。所以旧服务在跑时，
必须从独立 cgroup（`systemd-run --scope` 或新的 updater 单元）执行。

### 4.8 生产发布前验收清单

本轮已通过的隔离场景见 [ACCEPTANCE.md](ACCEPTANCE.md)；下列清单保留给真实生产组发布，不将夹具结果充当该清单已完成。

必须留下原始证据：

- [ ] `cargo test --locked` / clippy / fmt 在**最终树**上的完整输出（不是口头"全绿"）；
- [ ] `systemd_live --ignored` 的完整输出（临时实例、fixture Core），并注明它不代表出厂 Core；
- [ ] 一次真实首装：`status`、`readlink current`、`first-install-evidence.json`、
      `systemctl show` 的原始输出；
- [ ] 一次真实升级：`previous-<hex>.json`、`systemd-snapshot-<hex>/evidence.json`、
      升级前后 `status`；
- [ ] 一次真实中断恢复：断电/`SIGKILL` 后的 `systemd-recover` 输出与最终 `status`；
- [ ] 一次真实卸载：台账、`uninstall-files` 输出、剩余配置/数据仍在的证明；
- [ ] 若声明"签名生产包"：`verify` 的 `publisher_authenticated=true` 与 key fingerprint。

在以上记录齐全前，**不得**写"部署闭环已完成/全部已交付"。

---

## 附录：最终接线与保留边界

截至本轮验收（详见 [ACCEPTANCE.md](ACCEPTANCE.md)）：

- `install.rs::RuntimeAdapterConfig.first_install` 已渲染，证据固定为事务状态目录的
  `first-install-evidence.json`；后端真实查询所有配置单元 inactive 后才写证据。
- `runtime_adapter::query` 对 Status/Ready 调用 `verify_required_modules`；
  老 Core 无证据时仅允许 required 为空。缺模块、未挂载、inactive、错误和矛盾证据均拒绝。
- `apply-request` 根据严格执行文档选通用适配器或 systemd 后端；updater 注释已同步。
- `systemd-apply` 与 `systemd-recover` 已绑定完整 SystemdExecution 快照及控制适配器副本；
  恢复只加载已有证据，不现场创建授权。完整字段以 canonical serde JSON 哈希绑定，
  不声称保留管理员原文件的空白与键序。
- `install-plan` 无 `--output` 时附带 `runtime_adapter_config`；目录模式仍只输出计划文件。
- 实例级 self-update 命令会选择 `echo-agent-<instance>-update.service`，状态读取真实
  `transaction-<instance>.json`；旧部署的默认单元和 key=value 状态保持兼容。
- `install-files` 先持久化意向账本，最终发布失败时安全回滚或保留残余证据；
  显式 launcher 目录必须等于 runtime 渲染所用目录。
- `Type=simple` 的 socket 启动竞态由有总时限的只读 Ready 重试处理，不重放修改操作。
- 隔离 CLI 首装、升级、失败回滚、快照恢复已有硬断言；旧首装诊断测试仍只记录，
  不拿它充当成功证明。

仍不宣称完成的范围：

1. 真实 Core+Panel 的 user-scope 隔离整组首装/升级/回滚已通过；生产实例仍未替换，system scope 未实际验收。
2. 显式 migration-* 接管前置与激活前恢复已实现并实测；自动发现/完整数据迁移未实现，旧脚本不能直接删除。
3. Docker、多节点执行、渠道下载、数据备份和数据库回滚；Compose/fleet 仅计划。
4. Panel 初始 gate 可通过 panel-gate 根据验证后的包生成；仍不由 install-files 自动写配置。
5. check-config 已交叉核对 install/execution/runtime 三文档；只证明声明一致，不证明运行身份或部署工具签名。
6. `RuntimeConfig` 当前没有 schema_version 字段；严格输入不可自行添加。
7. 任意用户子进程/远程写者不在 Core 写锁覆盖范围，必须显式处理后再确认部署。
8. 管理员 launcher 引用的部署工具/runtime 配置不是发布包内的自认证材料；
   单 UID/root 管理信任和停止外部写者的声明仍是明确边界。

生产未迁移、容器/多节点只是计划，因此本轮结果是本地部署链路的实现与隔离验收，
不是“所有部署形态和生产发布均已交付”。

### 快照与 begin 顺序补充

两种 apply 都在 state lock 内先冻结 provisional transaction 的快照，再用 pinned adapter 执行 begin/execute。begin 取得的 previous_release 不参与快照身份绑定；仍由实际 observe 捕获并由事务持久化。begin 失败可能留下未引用快照，不自动删除审计证据。

首装 drain、失败恢复后的空安装 maintenance release 会重新查询 inactive 后刷新证据。每实例须有独立 state_dir；共享目录并行首装不受支持。最终复验见 ACCEPTANCE.md 第 6 节。


## 第二轮真实组合与接管验收

见 ACCEPTANCE.md 第 7 节。新增 Panel gate 环境注入、Panel MainPID 镜像哈希、事务每个身份屏障重哈希所有 artifacts（含 Web），以及 check-config/panel-gate CLI。已确认的宿主权限失败原因是 updater PrivateTmp 引发受限 user namespace/AppArmor profile，而非仅 NoNewPrivileges；deployment updater 有意保留 NoNewPrivileges、取消 PrivateTmp，Core/Panel 不变。

`migration-capture` 持久化显式旧/新单元证据；`migration-takeover` 对照漂移证据停旧、daemon-reload、禁旧启新，但不启动新发布；`migration-recover` 仅激活前恢复旧 enable/运行态，须确认可逆。新单元必须 canonical 且从未启动，存在实例部署事务则拒绝恢复旧应用。ExecStart 比较忽略 systemd 尾随的执行结果字段（pid/time/code/status），命令定义前缀仍须相同；raw 值留作审计，不把它当完整 ExecStart 语法解析器。此模块不是数据回滚引擎，也不含自动旧安装发现。


### 接管审查补充

新单元必须与当前实例 canonical 名称和 systemd 的 FragmentPath 同时匹配。
生产 migration transport 核对调用者 cgroup/祖先 PID，不能停掉自身执行环境；关键宿主服务不允许列为旧应用。失败保留持久日志并明确提示人工恢复，不自动重启旧应用。最新审查与门禁记录见 ACCEPTANCE.md 第 8 节。


### 持久激活闸门（覆盖易失 systemd 时间戳的边界）

部署模式三个单元均执行 ExecStartPre migration-mark-activation，先 O_EXCL 创建私有
migration-activated-<instance>.json，再 fsync 文件和目录，之后 systemd 才可执行应用。
并发启动接受既有同属主私有普通文件（包括崩溃留下的部分内容），并再次 fsync；
恢复判断只看存在性，不依赖 JSON 完整性。写入失败阻止启动，假阳性宁可人工处理。

migration spec 的必填 guard_executable 与每个新单元中的 exact guard 行绑定；
state_dir/instance 不同不能静默绕过部署事务检查。capture/takeover/recover 全部拒绝
已有 marker，recover 另查部署事务与当前运行状态/时间戳，并拒绝已 RolledBack 终态。
Failed 保持可恢复以支持部分停服失败；不因重复恢复而重启已终结的旧应用。
新单元启动后即使退出、reset-failed/卸载重载或重启，也不能靠遗失运行时属性回旧态。
此性质依赖受信任 unit/工具和持久 state_dir；drop-in 与同 UID/root 修改仍不在防篡改范围。
