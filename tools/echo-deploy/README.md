# echo-deploy：发布打包、候选库与可恢复部署事务

独立的 Linux Rust workspace（自带 `Cargo.toml`/`Cargo.lock`），不依赖 Core 的 agent/Fabric
依赖图。不联网、不推断 `$HOME`/XDG、不执行发布包里的任何内容——除非管理员把某个文件显式配成
受信任适配器。

> **状态声明**：这是**实现中**的部署工具，不是"已交付的生产发布器"。文件级安装、持久事务、
> 离线恢复、运行时控制适配器都已有代码与测试，但**尚未在这台机器上执行过真实生产升级**，
> 隔离 systemd 下的 CLI 首装/升级/失败回滚/恢复已通过，详见 [ACCEPTANCE.md](ACCEPTANCE.md)；另已通过真实 Core+Panel release 二进制的隔离整组测试；生产实例仍未迁移。`install-plan` / `uninstall-plan` / `compose-plan`
> 只输出计划文本，**不是**执行结果；不要把计划当安装完成。
>
> 设计契约、五问、信任边界与完整 runbook 见 [DESIGN.md](DESIGN.md)。

## 能力状态（以代码为准）

| 能力 | 状态 | 已验证范围 / 注意 |
|---|---|---|
| 严格 Release/Deployment 描述、逐文件 SHA256、三种拓扑检查 | 已实现 | 单测；`module` 类型仍被规划拒绝 |
| `pack` 由显式构建产物生成清单，完整枚举 web 树 | 已实现 | 不编译、不测试、不证明源码闭包 |
| Ed25519 离线签名验证、显式可信公钥目录 | 已实现 | 固定 `/usr/bin/openssl`；密钥轮换/撤销/防回退未实现 |
| 私有候选库、复制中重校验、`RENAME_NOREPLACE` 发布 | 已实现 | 单库 flock；同 UID/root 不在威胁模型内 |
| `install-plan` 渲染 unit/launcher/updater 文本 | 已实现 | 无 `--output` 只打印；有则写**新计划目录**；不加载 systemd |
| `install-files` 文件安装 + 所有权台账（intent → final） | 已实现 | 只写新文件；不 `systemctl`、不 enable/start/reload |
| `uninstall-files` 按台账删除受管文件 | 已实现 | 只删台账白名单内、内容仍匹配的普通文件；需 `--offline-only` 或 `ServiceStopped` 后端 |
| 通用适配器事务 `apply`/`recover`（hook 协议 `echo-deploy-hook-v1`） | 已实现 | 假适配器测试；适配器是**管理员受信代码**，契约校验不等于它没撒谎 |
| systemd 事务原语（`systemctl` 启停/状态/selector 切换） | 已实现，可注入 transport | 真实 systemctl 与真实 Core/Panel 的隔离首装/升级/回滚通过（opt-in 测试） |
| `systemd_activation` 全量 systemd 配置 hash + 适配器字节 pin + 离线恢复 | 已实现 | `systemd-recover` 已接入；`systemd-apply` 用同一上下文，见 DESIGN 变更点 |
| `runtime-adapter` 控制协议 `echo-deploy-control/1` | 已实现 | 协议 smoke、独立 updater→真实 systemd Core 权限/负例、真实 standalone 验收通过 |
| 首装证据链（`first-install-evidence.json` + 单元 inactive） | 已实现 | 证据由 systemd 后端在 `state_dir` 写；适配器独立复核 |
| `required_modules` 对照 Core `module_state` 校验 | 已实现并验证 | `control_client::verify_required_modules` 校验 mounted+active；旧 Core 无证据时仅 `required` 为空才通过 |
| `install.rs` `DeploymentRuntime` 渲染（真实 runtime env/selector/launcher） | 已实现渲染 | 只渲染；目录创建、配置落盘、启动仍由操作者执行 |
| 跨配置一致性 `check-config` / gate 生成 `panel-gate` | 已实现 | 检查声明一致性；不冒充运行身份或签名授权 |
| 旧单元接管前置 `migration-*` | 已实现并隔离验收 | 停旧/禁用旧/启用新；不启动新应用，不迁移数据；只允许激活前恢复旧态 |
| Docker / 多节点 | **仅计划** | `compose-plan` 不调 docker；`operations::fleet` 仅库接口且 `execution_supported=false` |
| 无人值守渠道下载 / 自更新闭环 | 未实现 | 更新请求仍需管理员冻结并 `--confirm` |

**不要把 `install-plan`/`install-files`/`compose-plan` 的输出当作"服务已升级"或"容器已更新"。**

## 构建与测试

```bash
cd tools/echo-deploy
cargo test --locked                      # 普通单测/集成测试（临时目录 + 假 Core/adapter）
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release --locked           # 产物 target/release/echo-deploy
```

真实 systemd 测试是**显式 opt-in**，默认 `#[ignore]`，会改动本机 user manager：

```bash
systemd-run --user --scope cargo test --offline --test systemd_live -- --ignored --nocapture --test-threads=1
```

它使用临时实例名与 `tests/fixtures/core_fixture.rs`（fixture Core，不是出厂 Core），
结论只覆盖"部署事务 + 适配器协议 + 真实 systemctl"，**不构成生产 Core 验收**；
首装路径在该文件里目前只做记录，不是断言。

另有对**真实已构建 Core 二进制**的协议 smoke（仍需显式 opt-in，且不做部署）：

```bash
systemd-run --user --scope \
  --setenv=ECHO_DEPLOY_RUN_CORE_LIVE=1 \
  --setenv=ECHO_DEPLOY_CORE_BINARY=/abs/target/debug/echo-agent-core \
  cargo test --locked --test core_live -- --ignored --nocapture --test-threads=1
```

它只证明控制 socket 的 `status/prepare/ready/cancel`、generation、`/proc/<pid>/exe` 哈希与
"无外部连接"等协议事实；跑到的可能是旧二进制（无 `module_state` 时记录
`absent_legacy_binary`），**不是**一次首装/升级验收。

部署工具的完整门禁、隔离测试结果及 Core 既有 Clippy 阻断项分别记录在 [ACCEPTANCE.md](ACCEPTANCE.md)，不混称全仓全绿。

## 代码地图

| 文件 | 职责 |
|---|---|
| `src/lib.rs` | 入口：`verify`/`plan`/`init-store`/`stage` |
| `src/pack.rs` | 从构建产物组装 Release（不编译） |
| `src/trust.rs` | 离线 Ed25519 验证、trust 目录 |
| `src/install.rs` | unit/launcher/updater 渲染；`DeploymentRuntime` opt-in 模式 |
| `src/installation.rs` | 真实写 unit/launcher 文件 + 所有权台账；卸载 |
| `src/systemd.rs` | 有界 systemctl 原语 + 控制适配器协议 |
| `src/control_client.rs` | Core Unix socket 客户端、身份限制、`required_modules` 校验 |
| `src/adapter.rs`、`src/execution.rs` | 通用 `echo-deploy-hook-v1` 适配器与执行配置 |
| `src/systemd_transaction.rs` | `SystemdExecution` + 事务后端（selector/单元/证据） |
| `src/systemd_activation.rs` | 全量配置 hash、适配器 pin、快照与离线恢复 |
| `src/activation.rs` | 通用快照 / cgroup 守卫 / 恢复锚点 |
| `src/transaction.rs` | 持久状态机（Phase/step evidence/recover） |
| `src/runtime_adapter.rs` | 运行时控制适配器（含首装证据校验） |
| `src/compose.rs`、`src/operations.rs` | 只读计划（Compose、卸载、fleet） |
| `scripts/` | 新薄入口（见下），**旧的仓库根 `scripts/install.sh` 等尚未替换** |

## 命令一览

```
pack --template --inputs --output                 verify --bundle [--require-complete] [trust]
inspect --deployment                              plan --bundle --deployment
init-store --store                                stage --bundle --deployment [--allow-development] [trust]
install-plan --spec [--output]                    install-files --spec --unit-directory --launcher-directory --ledger --confirm
uninstall-plan --spec                            uninstall-files --ledger --offline-only --confirm
apply --execution --release [--transaction] --confirm [--allow-development] [trust]
systemd-apply --execution --release [--transaction] --confirm [--allow-development] [trust]
apply-request --request --confirm                 systemd-recover --execution --confirm
recover --execution --confirm [trust]             status --execution
doctor --execution                                compose-plan --spec
runtime-adapter --config                          （stdin 一个 JSON 请求，stdout 一个 JSON 回复）
```

`trust` = `--trust-store --key-id --signature`（三者必须同时给或同时不给）。

## 候选、签名、暂存

所有路径必须是**绝对、规范化**路径。

```bash
echo-deploy pack --template /abs/template.json --inputs /abs/outputs.json --output /abs/bundle
echo-deploy verify --bundle /abs/bundle --require-complete
echo-deploy init-store --store /abs/state/echo-agent-core/releases   # 目录必须尚不存在
echo-deploy stage --bundle /abs/bundle --deployment /abs/deployment.json --allow-development
```

pack 模板是 Release JSON，`artifacts:[]`，来源由构建系统声明：

```json
{"schema_version":1,"release_id":"dev-20261114-01","target_os":"linux","target_arch":"x86_64",
 "management_protocol":"management-contract-1","development":true,
 "sources":[{"id":"core","revision":"explicit-source-snapshot",
             "snapshot_sha256":"<64 hex>","lock_sha256":"<64 hex>"}],"artifacts":[]}
```

outputs.json（`kind` 只接受 `core`/`panel`/`web`；`module` 明确报错）：

```json
[{"kind":"core","source_id":"core","input":"/abs/build/echo-agent-core"},
 {"kind":"web","source_id":"core","input":"/abs/build/web"}]
```

`deployment.json`（`Deployment`；`stage`/`plan`/事务共用同一份候选库路径）：

```json
{"schema_version":1,"instance_id":"local-dev","topology":"core_only","scope":"user",
 "target_os":"linux","target_arch":"x86_64",
 "store":"/abs/state/echo-agent-core/releases",
 "required_modules":["echo-agent.session"]}
```

`Deployment.store` 必须与 `deployment.release_store`、`RuntimeConfig.release_store` 是同一路径
（跨文档不校验，见 DESIGN 2.5）。

- `pack` 的输出目录必须尚不存在（`RENAME_NOREPLACE` 发布，已存在即失败）。
- `verify --require-complete` 要求包内文件集合与清单**完全相等**（无多余文件），
  所以 `release.sig` 必须放在包外。
- 候选库标记：`store-format`（内容 `echo-deploy-candidates-v1\n`）与 `store-lock`；
  库目录必须 0700 且属当前用户。完成候选为 0444/0555 + 目录封存——这是纪律上的只读，
  不抵御同 UID/root。
- `stage` 发布到 `<store>/<release_id>`（`RENAME_NOREPLACE`，已存在即失败）；
  崩溃可能留下 `pending-*`，本工具**不递归清理**。

签名是对 `release.json` **原始字节**的 64 字节 Ed25519 分离签名；签名私钥不进 bundle：

```bash
# 受信任发行机
openssl pkeyutl -sign -rawin -inkey /secure/release-private.pem \
  -in /abs/bundle/release.json -out /abs/release.sig
# 目标机预先建 trust 目录：0700、属当前用户、含 trust-format 与公钥
#   trust-format 内容必须精确为：echo-deploy-trust-v1\n
#   公钥文件名必须为：<key-id>.ed25519.pem（去掉 .ed25519.pem 后缀就是 --key-id）
echo-deploy verify --bundle /abs/bundle \
  --trust-store /abs/trust --key-id release --signature /abs/release.sig
```

未给 trust 时 `verify` 输出 `publisher_authenticated=false`（且 `activation_supported=false`）。
签名密钥轮换/撤销/渠道防回退仍需发布策略，不能由摘要替代。

## 安装描述（InstallSpec）

```json
{
  "instance_id": "local-dev",
  "scope": "user",
  "topology": "core_only",
  "release_root": "/abs/state/echo-agent-core/deploy/current",
  "core_config": "/abs/config/echo-agent-core/core.toml",
  "deploy_executable": "/abs/libexec/echo-deploy/echo-deploy",
  "state_dir": "/abs/state/echo-agent-core/deploy",
  "deployment": null
}
```

`deployment` 为 `null` 时是历史模式：此时 `release_root` 应是**具体的 release 目录**
（如 `/abs/releases/<release_id>`）。要启用真实运行时，把它换成下面的 `deployment` 对象，
`release_root` 随之变成 selector。

- `topology`：`core_only`（无 panel/web）、`embedded`（web，无独立 panel）、`standalone`（panel + web）。
  `panel_config` 仅 `standalone` 必填，其余必须缺省。
- 没有 `deployment` 时按历史模式渲染：`ExecStart=<release_root>/bin/echo-agent-core --config <core_config>`，
  **冻结一个 release 目录**。
- 有 `deployment` 时 `release_root` 变成**持久 selector**（必须是 `state_dir` 的子路径），
  unit 不内嵌具体 release；`ExecStart` 指向受信 launcher，`ECHO_RELEASE_ID` 由 launcher
  在 exec 时从 selector 推导——它是**声明**，不是 build ID，也不是进程身份证明。

### `deployment`（opt-in 真实运行时）

```json
{
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
```

约束（渲染期校验）：`core_socket`/`runtime_config`/`panel_gate_file` 必须是
`instance_state` 的**直接子文件**；`instance_state` 与 `state_dir`、`release_store` 不得重叠
（它们共用同一把 flock 名）；`launcher_directory` 不得落在 release/state/runtime/source 树内；
socket 路径 ≤107 字节；`standalone` 必须有 `panel_gate_file`，其余拓扑必须为 `null`。
`acknowledge_external_writers_stopped` 默认 `false`，只有在真的停掉外部写者后才允许置 `true`。

渲染产物（`install-plan`/`install-files` 共用）：`echo-agent-<instance>-core.service`、
`...-panel.service`（standalone）、`...-update.service`、launcher `<instance>`/`<instance>-panel`。
updater 单元固定执行 `echo-deploy apply-request --request <state_dir>/update-request.json --confirm`。

## 文件安装与卸载

```bash
echo-deploy install-files --spec /abs/install.json \
  --unit-directory /abs/.config/systemd/user --launcher-directory /abs/.local/bin \
  --ledger /abs/state/echo-agent-core/deploy/ledger-<ts>.json --confirm
```

- 两个目录必须**已存在**、属主正确、不可 group/other 可写；台账父目录必须存在且私有（`& 077 == 0`）、
  属当前 euid。
- 只创建新文件（`O_EXCL` + `RENAME_NOREPLACE`）；任何已存在的文件/链接/目录都是硬错误，绝不覆盖。
- 失败时只回滚本次创建且哈希仍匹配的文件；幸存文件会写进 partial 台账（`partial:true`），
  所以不存在"创建了文件却没有所有权证据"的窗口。成功状态固定为
  `files_installed_not_activated`——**装文件不是激活**。

```bash
echo-deploy uninstall-files --ledger /abs/.../ledger-<ts>.json --offline-only --confirm
```

只删台账白名单内、内容/大小/模式/uid/gid 仍匹配的普通单链接文件；不删目录、配置、数据、
release 树；`--offline-only` 是操作者在带外停服后的显式 attestation，CLI 目前没有
`ServiceStopped` 后端，只有这个选项。

## 部署运行时（`runtime-adapter`）

`runtime-adapter --config <runtime-adapter.json>` 由受信 launcher 在
`ECHO_DEPLOY_CONTROL_PROTOCOL=echo-deploy-control/1` 时分发；它读 stdin 一行 JSON、
只回一行 JSON。配置必须 0600、严格字段：

```json
{
  "instance_id": "local-dev",
  "core_socket": "/abs/state/echo-agent-core/runtime/core.sock",
  "panel_gate_file": null,
  "instance_state": "/abs/state/echo-agent-core/runtime",
  "current_selector": "/abs/state/echo-agent-core/deploy/current",
  "release_store": "/abs/state/echo-agent-core/releases",
  "required_modules": ["echo-agent.session"],
  "acknowledge_external_writers_stopped": true,
  "first_install": {
    "evidence": "/abs/state/echo-agent-core/deploy/first-install-evidence.json",
    "core_unit": "echo-agent-local-dev-core.service",
    "panel_unit": null,
    "max_evidence_age_seconds": 300
  }
}
```

- `first_install` 是首装唯一入口：没有它，缺 socket 一定是失败，绝不当成"空安装"。
  证据文件由 systemd 后端在 `state_dir` 里写（固定名 `first-install-evidence.json`），
  适配器会独立复核 selector 不存在、单元名、时间戳与摘要。
- `panel_gate_file`（standalone 必填）是维护门 JSON：
  `{"schema_version":1,"instance_id":...,"maintenance":false,"release_id":"<声明>","management_protocol":"<声明>"}`。
  适配器会翻转 `maintenance`；`release_id`/`management_protocol` 是部署声明，**不是**自证。
- 没有配置 `panel_gate_file` 的拓扑，维护状态只记录在适配器私有 state 文件里。

## 薄入口（`scripts/`）

这些是**新加**的薄入口，只转交固定子命令；需要 `ECHO_DEPLOY_TOOL` 指向绝对、非符号链接的
可执行文件（没有 `PATH` 兜底）：

```bash
export ECHO_DEPLOY_TOOL=/abs/libexec/echo-deploy/echo-deploy
scripts/install.sh --spec /abs/install.json --output /abs/plan-<ts>     # install-plan
scripts/install.sh --files --spec /abs/install.json \
  --unit-directory /abs/.config/systemd/user --launcher-directory /abs/.local/bin \
  --ledger /abs/state/echo-agent-core/deploy/ledger-<ts>.json --confirm  # install-files
scripts/update.sh --execution /abs/execution.json --release <id> --confirm   # 只走 apply
scripts/repair.sh --execution /abs/execution.json --confirm                  # 只走 recover
scripts/uninstall.sh --files --ledger /abs/.../ledger-<ts>.json --offline-only --confirm
scripts/compose-plan.sh --spec /abs/compose.json
scripts/bootstrap.sh --tool /opt/echo-deploy --sha256 <64 hex> --wrapper install -- --help
```

`update.sh` 固定 `apply`（管理员适配器路径）；systemd 事务用 `systemd-apply` 或 updater 单元的
`apply-request`，**不**由这些入口覆盖。旧旗标（`--overwrite-config`/`--purge`/`--scope`/
`--repository`/`--cn-mirror` 等）一律 exit 2，拒绝转发。

## Runbook 摘要

完整步骤（含目录创建、权限、`daemon-reload`、system/user 差异、失败处理）见
[DESIGN.md](DESIGN.md#四运行手册)。摘要：

```bash
# 0) 候选：pack → verify（正式包带 trust）→ init-store → stage
# 1) 渲染单元（只写新计划目录，不装到 unit 目录）
echo-deploy install-plan --spec /abs/install.json --output /abs/plan-<ts>
# 2) 落文件 + 台账（不启动；仍需人工 daemon-reload）
echo-deploy install-files --spec /abs/install.json \
  --unit-directory /abs/.config/systemd/user --launcher-directory /abs/.local/bin \
  --ledger /abs/state/echo-agent-core/deploy/ledger-<ts>.json --confirm
systemctl --user daemon-reload        # 人工动作；install-files 自身不做 daemon-reload
systemctl --user enable echo-agent-local-dev-core.service   # 人工；工具从不 enable
# 3) 首装：单元必须 inactive、selector 必须不存在
systemd-run --user --scope --unit echo-deploy-<ts> \
  /abs/libexec/echo-deploy/echo-deploy systemd-apply \
  --execution /abs/systemd-execution.json --release <release_id> --confirm
  # 正式（签名）包再追加 --trust-store /abs/trust --key-id release --signature /abs/release-<rev>.sig
# 4) 升级（同一命令换 release；新 release 需先 stage）与状态
echo-deploy status --execution /abs/systemd-execution.json
# 5) 中断/断电恢复（无 trust store，锚定 durable transaction + 既有快照）
echo-deploy systemd-recover --execution /abs/systemd-execution.json --confirm
# 6) 卸载（先人工停服）
echo-deploy uninstall-files --ledger /abs/.../ledger-<ts>.json --offline-only --confirm
```

`systemd-apply`/`systemd-recover` 会拒绝从 Core/Panel 自己的 cgroup 里运行；生产（签名）路径
同样拒绝旧 `echo-agent-core.service`，只有显式 `--allow-development` 的迁移入口才容忍旧单元。
所以请从独立 updater 单元或 `systemd-run --scope` 启动。

## 用户（user）与系统（system）作用域

| | `scope":"user"` | `scope":"system"` |
|---|---|---|
| unit 目录 | `~/.config/systemd/user` | `/etc/systemd/system` |
| launcher 目录 | `~/.local/bin` | `/usr/local/bin` |
| 运行身份 | 调用用户（不渲染 `User=`） | `User=root`/`Group=root` |
| `WantedBy` | `default.target` | `multi-user.target` |
| 前置权限 | 目标目录属当前 euid | 必须 euid 0，目标目录 root 所有 |
| systemctl | `--user` | 系统 manager |

表中的目录是 `RenderedInstall::unit_dir_hint()`/`launcher_dir_hint()` 给出的**提示**；实际写入
以 `install-files --unit-directory/--launcher-directory` 为准（工具从不推断 `$HOME`）。

system 作用域下所有 state/ledger 目录同样要求私有且属 root；`install-files` 会拒绝
非 root 的 system 安装。

## 安全与已知边界

- 逐层 fd 相对 `NOFOLLOW`；普通文件/nlink 检查；复制时哈希，保留清单原始字节。
- 候选库与 trust 目录必须本地、可信、私有；不保证 NFS/恶意挂载/同 UID 攻击。
- 适配器、launcher、ledger 都是**受信任管理员边界**：工具校验回执契约，不证明适配器没谎报。
- Core 的 `prepare` ACK 只覆盖它自己控制的写者；用户 shell/子进程写者必须由操作者停掉并
  `acknowledge_external_writers_stopped=true`——这是 attestation，不是工具实测。
- `ECHO_RELEASE_ID`/`release_id` 是声明；真正校验运行中 Core 字节的是 `ready` 对
  `/proc/<pid>/exe` 的 SHA-256 比对（pid 来自 `SO_PEERCRED`）。
- 同包 `management_protocol` 是组合声明，不能证明 Core/Panel 运行时兼容；Panel 侧读取维护门
  的契约在**外部仓库**，本仓库未验证。
- 回滚是**组合回滚**（回到旧 release + 旧单元），不是数据回滚：配置/数据库/会话/媒体
  不做逆向迁移；`reversible_data` 是操作者声明。
- 本工具不写 `update-status`，只注入路径；内容契约属于 Core 自更新模块。
- 旧的仓库根 `scripts/install.sh`/`update.sh`/`uninstall.sh` **尚未被替换**，
  旧安装也没有自动接管路径（见 DESIGN）。`tools/echo-deploy/scripts/` 里的入口只转交固定子命令，
  不联网、不提权、不直接调 systemctl/docker。

最终接线与保留边界见 [DESIGN.md](DESIGN.md#附录最终接线与保留边界)；本轮实际输出见 [ACCEPTANCE.md](ACCEPTANCE.md)。


## 补充入口与运行边界

```sh
# 三份严格配置的交叉检查：不调用服务、不写文件
 echo-deploy check-config --install /abs/install.json --systemd /abs/systemd-execution.json --runtime /abs/runtime-adapter.json
# 根据完整性已验证的包生成初始 Panel gate JSON（输出到 stdout，不写配置）
 echo-deploy panel-gate --spec /abs/install.json --bundle /abs/bundle
# 显式旧单元接管：capture 写不可覆盖证据，takeover 停旧并切换开机启用状态
 echo-deploy migration-capture --spec /abs/migration.json --confirm
 echo-deploy migration-takeover --spec /abs/migration.json --confirm
# 此时新单元未启动；另执行 systemd-apply 完成新发布激活。
# 仅新单元从未启动、且无部署事务的前置回退：
 echo-deploy migration-recover --spec /abs/migration.json --confirm --acknowledge-reversible
```

migration 文档结构见 `src/migration.rs::MigrationSpec`：所有旧 unit/fragment/binary/workdir、
新 canonical 实例单元、unit_directory/state_dir、guard_executable 都显式给出。它不发现、不重写旧配置，
不做数据迁移；capture 会写证据，必须确认。旧根目录脚本仍保留，不能把接管前置说成自动完整迁移。

部署模式 updater 使用 `PrivateTmp=false`、`NoNewPrivileges=true`：本机 unprivileged
mount namespace 会触发 AppArmor `unprivileged_userns` 对 sibling `/proc/<pid>/exe` 的拒绝。
只取消 updater 的临时目录 namespace，不取消 Core/Panel 隔离、不放宽镜像验证、不调用 PR_SET_PTRACER。
同 UID 管理信任边界依旧；其他宿主安全策略必须实际验收，失败保持 fail-closed。


### 持久激活闸门

部署模式的 Core、Panel、updater 每次启动都先执行 `migration-mark-activation`，
在与部署相同的私有 state_dir 中创建并 fsync `migration-activated-<instance>.json`。
即使没有 migration evidence 也写入；只有标记持久化成功才允许 ExecStart。
这是保守的“可能激活过”锁存，不证明应用就绪，也不证明数据迁移完成。

migration spec 必须给出 `guard_executable`；所有新单元的 `[Service]` 必须含与该工具、
state_dir、instance_id、unit 精确绑定的 `ExecStartPre`。缺失或路径不一致会在停旧前拒绝。
旧版生成单元须重新生成/安装，不能只补一个 JSON 字段。恢复成功是终态，重复 recover
不会重新启动管理员后来停掉的旧服务。marker 存在、不可读或为悬空符号链接均拒绝恢复；
清空 systemd 的运行时间戳不解除限制，禁止通过删除标记或复制证据来重试旧态恢复。

drop-in 覆盖、同 UID/root 主动修改工具/单元/证据/标记仍在管理员信任边界内。
真实主机重启与断电持久性尚未做破坏性验收；当前依赖文件与目录 fsync 的 Linux 契约。


发布顺序约束：先安装支持 `migration-mark-activation` 的工具，再加载带 guard 的新单元；
不得用旧工具配新单元，否则 ExecStartPre 会阻止 Core/Panel/updater 启动。
新单元 drop-in 中的空 `ExecStartPre=` 可清除 guard，属于管理员受信任修改，当前不验证
systemd 合并后的 drop-in 配置。换 state_dir 并重新生成安装相当于新的管理边界，不继承旧
目录的激活历史，禁止将其用作旧数据回退手段。当前仅在本机 systemd 255.4 实测。
