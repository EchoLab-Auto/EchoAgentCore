# 本轮验收记录

日期：2026-11-15。工作树未提交，未发布，未替换生产服务。

## 结论

本地 Linux/user-systemd 的部署工具实现及隔离验收通过：文件级安装/卸载、离线签名候选、持久事务、完整 systemd 配置快照、首装、更新请求升级、失败回滚、快照恢复和 Core 部署控制协议。

这不是“所有部署形态均完成”的声明：旧安装自动接管、Docker/多节点执行、渠道下载、生产 Core+Panel 整组发布和真实中断下的整组恢复仍未验收。

## 1. 可复现门禁

在 Core 仓库的 `tools/echo-deploy` 下：

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release --locked
```

本机原始输出：`/tmp/echo-deploy-20261115-final18.log`，末尾 `EXIT=0`。普通测试中的 `core_live` 和 `systemd_live` 默认 ignored，不将它们计入常规门禁的运行成功。

release 二进制：`tools/echo-deploy/target/release/echo-deploy`。
上一轮 SHA256：`ac7df276d296e59891c4a164ab241e2d3a1cdce1896d5571d5eb2ec7fa6ab8e5`（已被第 7 节最终产物取代）。
该摘要只识别本机产物，不证明源码闭包或可复现构建，不是正式发布签名。

## 2. 真实 user-systemd 与 CLI

从独立 systemd 单元运行，不能从生产 Core/Panel 的 cgroup 执行：

```sh
cargo test --locked --test systemd_live -- --ignored --nocapture --test-threads=1
```

结果：5 个隔离场景通过。原始输出：`/tmp/echo-deploy-20261115-live-all.log`。
其中旧首装诊断场景仅记录结果，不把它作为首装成功证明。

首装硬断言用例：

```sh
cargo test --locked --test systemd_live live_cli_first_install -- --ignored --nocapture --test-threads=1
```

最终输出：`/tmp/echo-deploy-20261115-cli-live2.log`，`1 passed`、`EXIT=0`。
真实执行：

1. `systemd-apply`：全新实例无 selector、单元 inactive，生成首装证明，提交 fixture-r1。
2. `systemd-recover`：读取已存在授权快照，保持 committed。
3. `apply-request`：严格 SystemdExecution 路由，升级 fixture-r3。
4. `systemd-apply`：fixture-r2-bad 明确拒绝 Ready，命令非零，事务 `rolled_back`，运行实例回到 fixture-r3。
5. `systemd-recover`：回滚终态恢复成功。
6. 临时 unit 删除，残余 MainPID=0；生产单元 ActiveState/MainPID 快照保持不变。

执行单元示例：`echo-agent-echo-deploy-test-962375-18e810aedce4da5b-core.service`。

真实部分：systemctl、ELF 进程、selector、CLI、runtime-adapter、进程镜像摘要、持久事务及授权快照。
替身部分：Core 是 `tests/fixtures/core_fixture.rs`，不是出厂 Core；不得据此声称生产 Core 的全部写者和数据已验收。

## 3. 实际 Core 控制协议与隔离

```sh
ECHO_DEPLOY_RUN_CORE_LIVE=1 \
ECHO_DEPLOY_CORE_BINARY=/home/evence/.local/share/echo-agent-core/source/target/debug/echo-agent-core \
cargo test --locked --test core_live -- --ignored --nocapture --test-threads=1
```

从独立单元 `echo-deploy-realcore-final-20261115.service` 执行。输出：
`/tmp/echo-deploy-20261115-realcore.log`，`1 passed`、`EXIT=0`。

实际 Core SHA256：`f817c9ed0c722eec3a5ef840147180ae7cb22bd682bd5650b03bf9d85eb7b579`（该次运行的版本，后续重建摘要可能不同）。

观察：

- 两次 prepare/cancel，generation 1 → 2；错误 transaction/generation 不释放维护。
- 受控写者暂停和恢复回执真实来自 Core，取消后 idle/ready。
- `modules_verified=true`，20 个模块事实、17 个 mounted/active；required 为 `echo-agent.kernel.protocol`、`echo-agent.loop.single`。
- HOME/XDG/TMPDIR/配置/工作目录均为临时路径；env_clear，无模型凭据。
- 仅一条随机 127.0.0.1 管理监听，无外连 TCP/UDP，无子进程。
- 正常退出，临时树删除、socket 删除、子进程为零。

该测试只证明实际 Core 控制协议，不执行真实 Core 的 systemd 二进制升级，也不覆盖用户 arbitrary subprocess/远程写者。

## 4. Core/Panel 回归

Core 定向组合：

```sh
cargo test -p echo-agent-core -p agent-module-self-update -p agent-module-loop
cargo build -p echo-agent-core
```

`/tmp/echo-deploy-20261115-core-final2.log`，`EXIT=0`。

Panel：

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`/tmp/echo-deploy-20261115-panel.log`，`EXIT=0`。包含 gate 和旧 WebSocket 维护关闭回归。

Core 严格 Clippy **未通过**：本轮 `cargo clippy -p agent-module-self-update --all-targets -- -D warnings` 在依赖 `agent-module-federation` 的既有告警（包括 large enum/result）处阻断，日志 `/tmp/echo-deploy-20261115-selfupdate.log`。不得把部署工具的严格 Clippy 通过写成 Core 全仓门禁通过。

## 5. 保留边界

- 未提交、未发布、未重启生产 Core/Panel。
- 生产 Core PID=491058，Panel PID=491018，本轮结束检查均 active。
- 旧安装迁移仍需显式停机窗口；未接管/覆盖旧单元。
- system scope、standalone 完整组发布和真实进程中断后的组恢复未验收；当前行为测试不能代替这些记录。
- Docker/fleet 仅计划；不调用 Docker/远程编排。
- `reversible_data=true` 是管理员策略，不是工具实施数据快照/恢复；不支持不可逆迁移自动回滚。
- 同 UID/root、管理员适配器及其外部配置/解释器/部署工具是受信任边界；pin 适配器不等于冻结整个外部依赖闭包。
- 日志位于本机 `/tmp`，不是长期发布附件；正式发布前应归档必要脱敏证据并签名产物。

## 6. 最后顺序修复与复验

`/tmp/echo-deploy-20261115-pin-before-begin.log`：完整工具测试、严格 Clippy、fmt、release 构建及隔离 CLI 首装/升级/失败回滚/恢复串行执行，末尾 `EXIT=0`。

- 通用 apply 和 systemd-apply 都先根据 provisional transaction 冻结快照，再用 pinned adapter 执行 begin 的 observe；execute 复用同一 backend。快照失败不再先留下无快照事务。
- 通用 CLI 回归在 observe 中断言自身从 snapshot 路径执行，覆盖首次 observe。
- 首装 drain 及无旧 release 的 maintenance release 在确认真实配置单元 inactive、selector absent 后刷新证据；过期不靠放宽检查绕过。
- begin 失败可能留下未引用的快照，不自动清除；不把它当作已开始或已完成部署。
- 每实例仍须使用独立 state_dir，固定首装证据文件名不支持共享目录并行首装。
- 生产阻断项仍包括：独立 updater 读取 systemd 启动的真实 Core 镜像在目标 ptrace 权限策略下的验证；协议夹具允许测试读取不等于生产权限已满足。


## 7. 第二轮：真实 standalone、独立权限与接管

本节覆盖并更新前文的“未验证”边界。当前仍未提交、未发布、未切换生产实例。

### 最终工具门禁

`/tmp/echo-deploy-final-0951.log`：`cargo test --locked`、严格全 target Clippy、fmt、release build 全通过，`EXIT=0`。

当前工具 SHA256：`6449c54534af347af0ebea0c03d5ab2ff6abf3ba2501ba3f51f32889c7cc668f`。

### 独立 updater → 真实 systemd Core

`/tmp/echo-deploy-core-permission-0912.log`：1 passed，EXIT=0。
真实 Core 由独立 systemd unit 启动，另三个单元分别执行 Observe、错误摘要负例、Ready。
独立 probe 与真实 runtime-adapter 都读取并 hash `/proc/1049977/exe`，无 PR_SET_PTRACER/宿主策略修改。
错误摘要确实触发 mismatch；正例 Observe/Ready 通过，生产 PID 不变。

因此第一轮“完全未知”权限阻断已缩小并定位：
`/tmp/echo-deploy-test-perm-1139899.log` 的实际权限矩阵证明，本机 updater 的 PrivateTmp
引发 AppArmor `unprivileged_userns` profile，读取 sibling `/proc` 被拒；NoNewPrivileges=true
本身不阻断。最终 deployment updater 渲染为 `PrivateTmp=false` + `NoNewPrivileges=true`；
Core/Panel 保持 PrivateTmp=true，运行镜像哈希验证完全保留。同 UID 管理信任不变。
这是当前宿主已实测的安全/功能取舍，不宣称适用于所有宿主策略。

### 真实 Core+Panel 整组 user-systemd

最终日志：`/tmp/echo-deploy-standalone-release-0945.log`，1 passed，EXIT=0，约 61 秒。
运行的是实际 Core 与 Panel release ELF，不是协议夹具：

- Core：`2dc73f0084a3d82dca8f36b3bfe6a22929282ff7976b59e970c5fc8b1df16e07`
- Panel：`9dde6ec9ca03acf23a37575d0275b5faea1a8a2dab894e9451db5f8e70128eb1`

场景：真实 renderer/launcher/CLI 首装 r1 → 独立 updater apply-request 升级 r2 →
坏 Panel 版本拒绝就绪并回滚 r2。两进程镜像哈希、selector、HTTP deployment 身份、
Web index 字节切换、维护 503、新 WS 拒绝、旧 WS 1001 Close、服务停启顺序均有硬断言。
日志按 USER_UNIT（manager lifecycle）与进程 `_SYSTEMD_USER_UNIT` 分开解析；
init.scope 不冒充 updater 进程所属 cgroup。清理后所有临时 unit inactive/MainPID=0，生产 PID 不变。

边界：Web 页面是明确标记的测试 HTML 字节，不是完整 Vue UI/浏览器发布身份握手。
配置空 persona、QQ 关闭、无模型凭据；不覆盖真实聊天/外部模块/任意子进程写者。

### 真 SIGKILL 后恢复

`/tmp/echo-deploy-crash-check-0912.log`，1 passed，EXIT=0。
观察到 selector 已切目标、unit activating、目标启动标记出现且 socket 未 ready 后，
真实 apply 进程收到 SIGKILL。独立 systemd-recover 读取完整快照/旧证据，回旧 release、
`rolled_back/error=interrupted`、释放维护，临时进程清理。此测试 Core/adapter 是显式夹具，
不能把它等同真实 Core+Panel 在同一中断点的整组数据恢复。

### 显式旧单元接管前置与恢复

`/tmp/echo-deploy-migration-fixed-0950.log`：16 单测 + 4 个真实 CLI/user-systemd 场景通过，EXIT=0。
不是“记录恢复失败当通过”：成功用例必须出现 `RECOVER_RESTORED`。

- capture 保存标准文件 SHA256、单元状态与声明路径；无确认拒绝，不覆盖证据。
- takeover 停旧/禁旧/启用已安装新单元但不启动；无确认拒绝。
- recover 在新应用未启动前恢复旧 enabled/active，新进程 PID 与旧 PID 不同。
- 修改旧 fragment 后 takeover 拒绝且未停止旧实例。
- systemd ExecStart 的尾随运行结果不再造成假 drift；命令定义 drift 仍拒绝。
- 新单元从前启动过（即使目前 inactive）、已有实例部署事务时，恢复旧应用会拒绝。
- 新单元名限制到当前 instance 的 canonical core/panel/update；system scope 要求 root。

夹具是 sleep/true 临时服务；不覆盖真实生产旧配置/数据迁移。旧根脚本仍未自动替换。

### 仍未交付/验收的范围

- 生产实例迁移和正式签名发布；所有当前改动未提交。
- system scope/root 环境的真实端到端；当前验证为 user scope。
- Docker/fleet 实际事务执行、渠道拉取、构建发布流水线。
- 数据备份/格式回退、不可逆迁移；`reversible_data` 仍是管理员明确策略。
- 同一场景中真实 Core+Panel 的 SIGKILL 整组恢复；现有两个层次分别验证，不混称已覆盖。
- Core 全仓严格 Clippy 的既有依赖告警未在本任务中清理，不能混称全仓全绿。

最终生产核对：Core PID=491058、Panel PID=491018，均 active。只读权限探针曾读取其 exe
元数据/少量字节作基线，没有发管理请求、没有读配置或重启它们。


## 8. 迁移审查收尾

审查旧版本指出的新单元 FragmentPath、自停保护、失败提示已补齐：

- capture/takeover 核对 systemd 实际 FragmentPath 与受哈希新 unit 路径一致；更高优先级同名单元不接受。
- 生产 transport 在 capture/takeover/recover 前读取调用者 cgroup 与祖先 PID，拒绝涉及调用者/祖先的单元；关键宿主基础服务禁止列入旧应用停服清单。未实现安全验证的 transport 默认拒绝。
- 接管动作失败明确报“可能部分接管”，要求先 inspect journal / migration-recover，绝不自动重启旧应用或伪装无副作用。
- CLI 已有显式 spec、--confirm、独立 cgroup 守卫与 state lock；capture 的 preview 会列出 stop/disable/enable 清单。没有交互式确认提示。

`/tmp/echo-deploy-migration-review-fix.log`：18 单测、4 个真实 migration CLI/user-systemd 场景及严格 Clippy 通过。
`/tmp/echo-deploy-final-review-0956.log`：完整测试、严格 Clippy、fmt、release build 通过，EXIT=0。
最新工具 SHA256：`bbf9fc5e1388c11cd36ee42a5c30ca4a8a890de4068bfc2278777f1f6f88c047`。
生产 Core/Panel PID 仍为 491058/491018，均 active，未执行生产迁移。


## 9. 持久激活闸门与恢复终态

本节取代第 8 节工具摘要，并补正“当前 inactive + 启动时间戳为零即可证明未激活”的不足。

- deployment Core/Panel/updater 三个真实渲染单元均含 ExecStartPre：
  `migration-mark-activation --state-dir ... --instance-id ... --unit ...`。
  O_EXCL 创建 `migration-activated-<instance>.json`，文件和目录均 fsync 后才允许启动。
  无 migration evidence 也写入；既有部分内容是保守锁存，不要求 JSON 完整才拒绝恢复。
- migration spec 新增必填 guard_executable。capture/takeover/recover 要求每个新单元
  的 guard 与工具、state_dir、实例和 unit 精确一致；缺失、跨目录绑定均在停旧前拒绝。
- marker/部署事务的存在性检查对悬空符号链接和访问错误 fail-closed。
- recover 复核新 unit 的文件哈希和实际 FragmentPath；RolledBack 是终态，重复恢复
  不会重新拉起管理员后来停掉的旧应用。Failed 仍允许显式恢复部分停服现场。

### 本轮证据

- `/tmp/echo-deploy-guard-final2.log`：完整工具测试、严格 all-targets Clippy、fmt、
  release build 全通过，EXIT=0；migration fake-transport 单测增至 24 项。
  覆盖并发 guard、部分 marker、符号链接拒绝、错误目录绑定、恢复终态零变更。
- `/tmp/echo-deploy-guard-final.log`：5 个真实 migration CLI/user-systemd 场景通过，
  EXIT=0。新增场景不创建部署事务，人工 start 新 unit 执行真实 ExecStartPre，随后
  stop/reset-failed/daemon-reload；恢复明确拒绝且旧单元保持 inactive/disabled/PID=0。
  其余场景仍硬断言成功恢复旧 active 服务；临时服务/目录清理、生产 PID 不变。
- `/tmp/echo-deploy-guard-standalone.log`：真实 Core+Panel release ELF 整组 1 passed、
  EXIT=0，62.08 秒；带新 guard 的 renderer 首装、独立 updater 升级、坏 Panel 回滚、
  维护 WS 关闭和停启顺序重新通过。Core/Panel ELF 同第 7 节，不混称重建了它们。

最终工具 SHA256：`c08d211d7b427933f13f5ec9281a2b15783bffc8bc31f3d103e43b3308e118ae`。
本轮早期编译/fixture 接线失败日志不作为通过证据；最终日志如上。

边界：未实际重启宿主或断电，持久性依赖 Linux fsync 契约；drop-in、同 UID/root 主动
篡改单元/工具/标记及与运维手工 start 并发的恢复不在当前串行管理前提内。
不删除 marker、不复制证据绕过闸门；旧版生成单元必须重新生成并安装。
仍未提交、未发布、未迁移生产；最后只读核对 Core=491058、Panel=491018，均 active。


## 10. 终态审查复核

独立审查提出“第二次 recover 的前置拒绝覆盖 RolledBack”，按实际闭包边界复核不成立：
前置校验在 outcome 闭包之前，拒绝不会进入 Failed 回写；takeover 也在闭包外拒绝既有 journal。
但确认了相邻顺序问题：原 recover 先写 RolledBack，再执行可失败的最终观察；该观察失败
会将终态降成 Failed。已改为全部最终观察/状态检查成功后，再原子发布 RolledBack。

新增故障注入测试禁止终态发布后再观察，另增强连续两次 recover/takeover 重试断言：
无服务变更、journal 原字节不变、保持 RolledBack、管理员停掉的旧服务不被拉起。

`/tmp/echo-deploy-terminal-review.log`：完整工具测试（迁移单测 25 项）、严格 all-targets
Clippy、fmt、release 构建、5 个真实 migration CLI/user-systemd 场景全部通过，EXIT=0。
本轮只调整 migration 终态发布顺序；真实 Core+Panel 整组最近一次证据仍为第 9 节日志。
最终工具 SHA256：`264613bbba723b37bca3b47b20095cccd6fb706117b3e9e7d1267c50811e988c`。
生产 Core=491058、Panel=491018，均 active，未迁移/重启。仍未提交或正式发布。
