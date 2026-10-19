---
id: decoupling-plan
title: "完全解耦推进计划"
group: 规划
x: 320
y: 3160
---

# 完全解耦推进计划（动态库 / 子进程）

> **定位**：本页是「完全解耦」的推进路线图——验收口径、方案研究（动态库 / 子进程 / wasm）、目标架构、分阶段清单与执行附注。读者：内核与插件体系的规划 / 实施者。**状态：规划**——本页不是当前实现的描述，实施进度以本页清单为准。
> 前置阅读：[架构总览](../overview/architecture.md)（现状与五条原则）、[插件化设计](../plugins/core-plugins.md)。

## 一、验收标准（"完全解耦"的可测口径）

全部为自动化可断言项，达成即视为"完全解耦"：

1. **零全局态**：`grep -rn "OnceLock" source --include='*.rs' | grep -v tests` = 0——所有状态经 `Ctx` 服务键寻址。
2. **一切皆插件**：内核只含机制（服务注册表 / 事件总线 / 插件宿主 / 协议）；全部能力（循环、provider、工具、技能、插件实例、工作区、subagent、联邦）以插件装载，配置可禁用到"只剩内核仍能启动"。
3. **运行期热替换**：任一代码插件可"卸载 → 替换 → 重挂载"，无进程重启；与数据插件热重载语义统一。
4. **进程外插件端到端**：至少一个示例插件以**独立二进制**交付、经配置装载；`SIGKILL` 其进程后内核无感（该插件工具报错并按策略自动重启）。
5. **配置化组合**：`dump-config` 输出分层合成树（profile / bundle / patch 三层）；改组合不改代码。
6. **协议一致性**：同一套 conformance 用例覆盖全部运输层（inproc / subprocess；可选 dylib / wasm）。

## 二、方案研究：动态库 / 子进程 / wasm（生态核查）

### 2.1 动态库（dlopen）——生态核查后的现实

Rust **至今无官方稳定 ABI**（rust-project-goals 2026 清单中**无任何 ABI 稳定化目标**；社区 crABI 实验未落地）。跨 `dlopen` 只能走 C ABI（`extern "C"` + `#[repr(C)]`）或稳定 ABI 库。生态现状：

| 库 | 最新版本 | 维护状态 | 关键限制 |
|---|---|---|---|
| libloading | 0.9.0（2025-11） | 活跃 | 裸 dlopen，无隔离；`close()` 可能 no-op / 泄漏 |
| abi_stable | 0.11.3 | **2023-10 起停更** | README 明示**不支持卸载**（"without support for unloading"） |
| stabby | 72.1.16（2026-07） | 活跃 | 同进程限制仍在；**有 async 支持**（`stabby::future`）；**生产案例：Zenoh**（zenoh-plugin-trait 1.10.1） |
| hot-lib-reloader | 0.8.2（2025-08） | 开发工具 | 类型变更即 UB、`TypeId` 失效（README 明示）；**非生产方案** |

技术路线（若做）：

```text
插件编译为 cdylib → 宿主 libloading::Library::new + 取单一入口符号：
  #[no_mangle] pub extern "C" fn echo_plugin_v1(host: *const HostApiV1) -> *const PluginVtableV1
HostApiV1 / PluginVtableV1 = #[repr(C)] 函数指针表；数据过界一律 JSON 字符串
（*const c_char + len + echo_free），不假设任何 Rust 类型布局。
```

关键风险（决定了它只能当实验轨）：

- **panic = 全进程陪葬**：Rust 1.81 起，`extern "C"` 中未捕获 unwinding 一律 **abort**；`catch_unwind` 捕不到 abort，`panic=abort` 下完全失效——插件 panic 杀掉整个宿主。
- **卸载不安全（官方印证）**：abi_stable 明确不提供卸载；`dlclose` 时残留线程/任务/TLS → UB。安全卸载需插件完全静默 + 引用计数归零；生产只能"逻辑卸载"（停流量，不 dlclose）。
- **重复 crate 实例**使 `Any`/`TypeId` downcast 失效（hot-lib-reloader 同警告）。
- **版本地狱**：宿主与插件必须同工具链同依赖图编译；rustc/依赖图每次升级需重编全部插件。
- **异步只能消息化**（回调 + 取消令牌 + 插件侧 runtime，或 stabby::future）——即消息化是前提，不是选择。

### 2.2 子进程（stdio 协议）——现成先例最丰富

- **协议即 ABI**：跨进程无 ABI 问题，天然稳定，且语言无关。
- **崩溃隔离**：插件 panic / segfault / OOM 只死自己，宿主监督重启——装载外部代码的最大可靠性收益；**这是同进程方案（含 dsh 本身）做不到的**。
- **热替换简单**：新进程 Ready → 切流量 → 老进程 Drain → 退出（失败保老，可回滚）；状态迁移显式化（可选 `export_state/import_state`）。
- **异步天然**：插件是普通进程，自带运行时；宿主用 `tokio::process` 管理即可。
- **现成先例（协议语义直接借鉴）**：
  - **nushell**（`nu-plugin` 0.116.1）：启动先发 1 字节长度 + 编码名（`\x07msgpack` / `\x04json`）；双向 `Hello`（协议名 + semver + features 协商）；消息带 CallId，支持并发/流式/中断；插件→宿主 `EngineCall`；空闲 GC 回收。
  - **MCP**（官方 Rust SDK `rmcp` 3.5.1）：JSON-RPC 2.0 + stdio 换行分帧；已有 `#[tool]` 宏 + schemars schema 生成 + 取消通知——工具类插件复用可省 2-4 人周（见 §2.4 选型）。
  - **LSP**（rust-analyzer `lsp-server` 0.10.0 / `tower-lsp-server` 0.23.0）：Content-Length 分帧，长期生产验证。
- **开销实测量级**（本机 20 次实测中位，2026-10）：
  - 序列化单次：serde_json 1.24µs / msgpack 1.07µs / protobuf 0.85µs / bincode 0.09µs——**均纳秒~微秒级，序列化不是瓶颈**；
  - IPC 纯转发往返：stdio 5.3µs / Unix socket 3.2µs；计入 tokio 调度 + 解析 + 插件工作 ≈ **20-60µs/次**（~2-5 万次/秒）——对 agent 场景（工具调用秒级间隔）完全富余。

技术路线：

```text
插件 = 独立二进制；宿主 tokio::process spawn，管道 stdio。
帧编码：4 字节 LE 长度前缀 + JSON（默认）/ msgpack（feature），
  经 tokio-util LengthDelimitedCodec（成熟、防帧粘连、背压友好）；
  与 MCP 生态互通时用换行分帧 JSON 兼容模式。
帧协议沿用 FedFrame 惯例：externally-tagged enum、#[serde(default)] 前向兼容、
call_id 关联、Hello/Welcome 版本握手（semver + features，对齐 nushell）、显式 Cancel。
生命周期：握手（超时）→ Ready → 服务；SIGTERM 优雅退出（drain deadline）→ 超时 SIGKILL；
进程组整体清理；stderr → tracing 转发；环境白名单 + cwd 受控 +（可选）rlimit。
```

### 2.3 对比总表

| 维度 | inproc（协议化内联） | 子进程（stdio 协议） | 动态库（C ABI / stabby） | wasm（wasmtime / extism） |
|---|---|---|---|---|
| ABI 稳定性 | 同编译单元，无问题 | **协议=ABI，完全稳定** | 脆弱（无官方 ABI；stabby 有类型校验） | 稳定（组件模型 / WIT） |
| 崩溃隔离 | 无 | **完全（进程级）** | 无（panic=abort 全进程陪葬） | **完全（沙箱 + fuel/epoch 限额）** |
| 热替换 | 挂载/卸载（已支持） | **重启式，最简** | 不安全卸载（abi_stable 官方不支持） | 实例重建（旧实例可丢） |
| 性能开销 | 零（channel） | **20-60µs/次（实测）** | ~ns | µs 级 |
| unsafe 面积 | 零 | 零 | 大 | 宿主零 |
| 语言无关 | 否 | **是（任意语言）** | 否 | 是（WIT 多语言） |
| 生态成熟度 | — | nushell / MCP / LSP | Zenoh（stabby 生产案例） | wasmtime 49 / extism 1.30（Spin、Fastly） |
| 推荐用途 | 内核伴生 / 热路径 | **外部插件主路径** | 实验轨（不卸载）/ 可信快路径 | 不可信第三方沙箱（可选轨） |

### 2.4 结论：四轨制 + 一套协议

1. **inproc（主基座）**——现有内置插件迁移为"协议化内联插件"：接口按消息定义、物理编译内联，性能零损失；
2. **subprocess（扩展主路径 / 默认后端）**——任何插件可无改动"提升"为独立进程；第三方/外部插件一律走此轨；协议语义以 **nushell 为蓝本**（CallId 并发/流式/中断、Hello semver + features 协商、空闲 GC）；
3. **dylib（实验轨，可选，不卸载）**——开发期重编译重载；若做选 **stabby**（活跃 + async + Zenoh 生产验证），明确"加新不卸旧"，不进生产默认；
4. **wasm（可选沙箱轨，升级自"远期"）**——wasmtime 49.0.2 / extism 1.30.0 已具备 fuel/epoch 超时、跨线程取消、内存上限等生产级能力（Spin、Fastly 等案例）；适合不可信第三方插件，Phase 6 评估。

**协议选型补充（Phase 0 决策项）**：倾向**自研统一协议**保证一致性（工具/服务/事件/loop 贡献统一建模），并将 **MCP（rmcp 3.5.1）作为工具类插件的兼容导入通道**——任何 MCP 服务器即插件，等价于免费获得 MCP 生态。二者共享传输层（stdio + 长度前缀 / 换行分帧）。

> 本节数据来源（2026-10）：crates.io API 版本核查、rust-project-goals 2026 目标清单、nushell 贡献者书 Plugin protocol reference、MCP 规范与 rmcp README、wasmtime/extism/stabby/hot-lib-reloader/abi_stable 仓库文档；序列化与 IPC 基准为本机 20 次实测中位。

## 三、目标架构

```text
┌── echo-kernel（内核：只含机制）────────────────────────────┐
│ Ctx v2 服务注册表（强类型键 / 依赖声明 / scope）             │
│ EventBus（emit / waterfall / parallel / serial，已有）       │
│ PluginSupervisor（加载 / 卸载 / 健康 / 重启 / 限额 / 日志）   │
│ echo-plugin-api（协议类型：冻结契约）                        │
│ Transport：Inproc / Stdio / [Dylib] / [Wasm]               │
│ 贡献注册表（工具 / 技能 / 服务 / 事件——全部可逆）             │
│ 会话编排骨架（事件日志 + 投影 + 压缩编排）※                   │
└──────────────────────────────────────────────────────────┘
        ▲ 协议（同一组消息，多载体）
┌── 插件（每个独立 crate；可内联编译或独立二进制）──────────────┐
│ provider-llm  tools-*  skills-dir  workspace  adapter-qq   │
│ subagent  loop-*  federation  management-panel*             │
└──────────────────────────────────────────────────────────┘
```

※ 会话/trunk 归属：**物理内联的协议化插件**，而非出进程。dsh 的 `ctx.sessions` 本身就是 in-process 插件（Cordis 插件全部同进程）；对齐点是"以服务键暴露、可被替换/包装"，不是"必须跨进程"。我方投影/压缩与 agent 深度互锁，跨进程（或跨 dylib 边界）会引入每事件 RPC 与序列化——**不采纳**；Phase 5 后评估是否把 `SessionStore` 提炼为独立 inproc 插件 crate（契约边界不变，仍留进程内）。
※ management-panel*：**协议化 inproc 插件**（对齐 dsh 的 webserver 亦为插件），与内核同进程但经服务键与贡献注册表装载；仅"禁用后自锁"的开关语义保留在内核守卫（TogglePlugin 拒绝禁用）。

协议核心消息（草案，Phase 0 冻结）：

```text
Host→Plugin: Hello{version, config} | Invoke{call_id, kind, ctx, payload} | Cancel{call_id}
             | Event{name, payload} | Drain{deadline} | Dispose
Plugin→Host: Welcome{id, version, capabilities} | Register{contributions}
             | InvokeResult{call_id, result|error} | Emit{event} | Log{level, msg}
             | Ready | Failed{error}
```

- 贡献类型（Contribution）：`Tool{name, desc, schema}` / `Skill{…}` / `Service{key}` / `Subscribe{events}`。
- 调用上下文（Invoke.ctx）：`session_id / team_id / branch_id / timeout / cancel`——**替代现行 `__session_id`/`__team_id` 参数注入，顺带消灭循环里的工具名特判**（`"checklist"`、`"shell_*"` 字符串分支）。

## 四、分阶段计划与清单

### Phase 0：契约冻结（2-3 天）

目标：把插件协议定义为可独立演进的 crate；后续一切以其为准。

- [x] 新建 `source/plugin/echo-plugin-api`：消息类型 / 贡献类型 / 版本协商（已提交 `4e2f82e`；7 项契约测试全绿）
- [x] 新建 `source/plugin/echo-plugin-host` 完整落地（已提交 `cd0b345`）：Transport 抽象 + InprocTransport + PluginSupervisor（握手/注册/调用/取消/超时/排空/崩溃重启）+ 14 项 conformance 全绿
- [x] 全局态棘轮门禁 `source/core/tests/no_global_state.rs`（基线 17 个静态单元；全 source 扫描、`static` 行含 `OnceLock`/`LazyLock` 口径，只减不增）
- [x] 编码决策落地：**4 字节 LE 长度前缀 + JSON**（inproc 类型直连零序列化；stdio 实现进行中，见 Phase 2）
- [ ] 协议选型决策：自研统一协议为基线；评估 **MCP（rmcp 3.5.1）** 作为工具类插件的兼容导入通道（可省 2-4 人周 + 免费获得 MCP 生态）
- [x] 协议语义对齐 nushell 蓝本：CallId 并发/流式/中断、Hello 版本 + capabilities 协商（空闲回收随 P2 监督者完善）
- [x] conformance 测试套件：inproc 14 项全绿（stdio 参数化随 Phase 2）
- [x] **冻结**：协议变更需过 conformance + bump 版本；纳入 CI（`PROTOCOL_VERSION` + `compatible()` 契约测试）

**验收**：✅ conformance 在 inproc 参考实现上全绿（14/14）。

### Phase 1：内核化——激活 Ctx、杀死全局态（1-2 周）

目标：17 处全局静态归零；启动由"服务可用性"驱动。

- [x] Ctx v2（已提交 `26e3e72`）：
  - [x] 强类型服务键（`ServiceKey<T>`）替代字符串键
  - [x] 注册冲突检测（provide 同名拒绝）+ `Missing{available}` 可读错误
  - [x] `service_or_wait`（就绪等待，消除丢通知竞态——替代手工编排启动序）
- [x] 全局态迁移（17 处 → 0，已提交 `f0a8f9d`）：
  - [x] 唯一引导单元 `echo-context/kernel.rs`（kernel cell，TypeId 类型擦除；全 source 唯一白名单，门禁强制校验存在）
  - [x] `GLOBAL_PLUGIN_HOST` / `GLOBAL_POLICY` / `FEDERATION_COMMAND_HANDLER` → kernel cell
  - [x] `GLOBAL_MANAGER` / `AGENT_FACTORY` → kernel cell
  - [x] `GLOBAL_SHELL` / `SHELL_EMIT` → kernel cell
  - [x] `REMOTE_SUBAGENT_NOTIFIER` / `AGGREGATE_DELIVER` / `REMOTE_INVOKER` / `REMOTE_QUERIER` → kernel cell（**2026-10 已再进一步：见下方终态跟踪，收敛为 `FederationPorts` 参数**）
  - [x] `NODE_ID` / `REGION_NAME` → kernel cell（**2026-10 已再进一步：改为 `NodeIdentity` 参数下发**）
  - [x] `SESSION_IMPORT_ACKS` / `SELF_STOP_RE` / websearch `CLIENT` / adapter-qq `CLIENT` → kernel cell（缓存类 get_or_init）
- [ ] Agent 结构体瘦身（可选项，随 Phase 3 外迁进行）：`config_store` / `provider` / `event_sink` / `loop_runner` / `plugin_host` 改为经 Ctx 解析
- [x] 门禁：`source/core/tests/no_global_state.rs` 升级为相等断言（EXPECTED_MAX=0，白名单=kernel.rs）
- [x] 终态跟踪：federation 组（notifier / aggregate-deliver / invoker / querier / aggregator / command-handler）**已出 kernel cell**：收敛为 `echo_agent::federation::FederationPorts`（`Clone` 值，`Default` = 联邦未接线、`is_linked()` 可观测），组合根装配联邦时构造一份 → `AgentBuilder::federation` 下发给每个 persona + 联邦泵销账用同一个聚合器。顺带修掉一个真 bug：工具外迁后 `echo-plugin-tools` 自己那份 invoker cell **从未被 set**，`node://` 远程文件/命令分流恒报「联邦未接线」；现改为 `register_coding_tools(..., invoker)` 注册期参数
  - [x] `NODE_ID` / `REGION_NAME`（node_identity 组）已**出 kernel cell**：改为 `echo_agent::NodeIdentity` 值类型，组合根构造一份后显式下发——`AgentBuilder::node_identity`（每个 persona 一份实例字段）+ `AgentManager::set_node_identity`（`TeamInfo` 用）。`Session::info()` 改为接收 `&NodeIdentity` 参数（身份是参数，不是全局事实）。顺带不再 `Box::leak` 两个字符串。零行为变化：空身份仍不下发字段

**验收**：✅ 门禁绿（17 → 0）+ 全量测试通过；`echo-agent` 对具体实现的依赖收敛到定义层（部分随 P3）。

### Phase 2：插件宿主与运输层（2 周）

- [x] `PluginSupervisor`（已提交 `cd0b345`）：
  - [x] 生命周期状态机（pending → ready → draining → stopped；backoff 重启 200ms×2 上限 2s + 5 次熔断）
  - [x] in-flight 调用跟踪：卸载先 Drain（等待或取消到 deadline；在途调用以 plugin_crashed 收尾）
  - [x] 日志转发（stderr → tracing，带插件前缀）/ 事件收集（`take_events`）
  - [x] 崩溃策略：on-failure（default）/ 可配（`RestartPolicy`）
- [x] `InprocTransport`：类型直连（mpsc 双向通道；关停=丢 sender→等任务→超时 abort）
- [x] `StdioTransport`（已提交 `9cffd0c`）：spawn / 进程组 / **4 字节 LE 长度前缀帧** / env_clear 最小环境 / stderr 转发 / SIGTERM→SIGKILL 语义（10 项 conformance）
- [x] `RemoteTool`（已提交 `aedbf83`）：插件工具贡献 → `echo_defs::Tool` 适配（context 剥离桥 + `remote_tools()` 一键包装）
- [ ] `DylibTransport`（实验）：libloading + C ABI vtable + 双向 `catch_unwind` + 符号版本校验；默认"逻辑卸载"；**若做优先评估 stabby**（abi_stable 停更且不支持卸载，排除）
- [ ] `WasmTransport`（可选轨）：评估 wasmtime 49（组件模型 + fuel/epoch 限额）或 extism 1.30（cancel_handle / timeout）作为不可信第三方沙箱
- [x] 参考插件：测试对端 `echo-plugin-test-peer`（已提交 `9cffd0c`）；产品级示例 `echo-plugin-example`（已提交 `65c1c2a`）
- [x] conformance：inproc 14 项 + stdio 10 项全绿（dylib / wasm 可选轨随后）

**验收**：✅ 示例插件工具经 inproc / subprocess 两运输均可用（dylib / wasm 可选）；杀死进程后自动重启（backoff 测试覆盖）、内核无感。

### Phase 3：内置插件外迁（2-3 周，逐个）

- [x] ① provider-llm → **独立 crate 协议化**：`echo-plugin-provider-llm`（provider 工厂 + 计量/脱敏装饰器，只依赖 `echo-defs` 与三个 provider crate）。宿主侧只剩 5 行 glue（`echo-agent` 的 `llm` 模块把 `AgentConfig` 投影成 `ProviderConfig`、把 `MetricsStore` 实现成 `UsageRecorder`），旧 `packages/provider_llm/` 已删除、`echo_agent::llm::…` 路径与签名不变
- [x] ② 工具子集试点：websearch + calculator → **独立二进制**（已提交 `65c1c2a`：`echo-plugin-sdk` + `echo-plugin-example` + 5 项 e2e；内核 `plugins.toml` 装配集成进行中）
- [x] ③ skills-dir → **独立 crate**：`echo-plugin-skills`（`SkillRegistry` 发现/分层合并 + `loader` frontmatter + `install` Git 来源）。顺带把插件 id 归一词汇下沉到 `echo_defs::plugin_id`（`canonical_plugin_id` 与两个前缀常量），技能加载器与组合根共用同一口径；旧 `packages/skills_dir/` 已删除
- [x] ④ adapter-qq → subprocess（崩溃隔离收益最大；设计见 [实例插件化设计](./instance-plugin.md)）——**已完成**：QQ 插件实例以独立仓库插件、经 `plugins.toml` 由宿主拉起（P1.7 外迁 / P1.8 宿主装配 / P1.9 删内置）；泛化收尾轮完成 `instance_ops` / `instance_tools` 改名、UI 描述符链路与 Panel 通道切换、通用渲染器（记录见 [adapter-genericization/](./adapter-genericization/)）。
- [x] ⑤ workspace → **独立 crate**：`echo-plugin-workspace`（`WorkspaceStore` 会话文档 + `workspace` 工具 + 只读 git/文件采集与沙箱解析）。两处解阻：其一，跨机查询改为**注入 `RemoteWorkspaceQuery`**（三个语义方法 git_status / list_files / browse；组合根注入 `FederationWorkspaceQuery` 映射到 `QueryKind`，未注入 = 远程目录降级为「不可采集」而非报错）；其二，`SessionKey` 下沉到 `echo-session`（会话 id 是跨边界词汇——事件日志文件名、工作区通道、`node://` 引用、插件入站都用它）。**Agent 侧的命令域接线留在宿主** `agent/workspace_commands.rs`（发事件、读 persona 白名单、动会话通道都是宿主职责），原 `packages/workspace/` 已删除
- [x] ⑥ subagent → **独立 crate**：`echo-plugin-subagent`（`spawn_subagent` 工具 + `SubagentStore` + `run_subagent_turn`，只依赖 `echo-defs` / `echo-tool`）。两处解阻：其一，远程委派聚合改为**注入 `RemoteDelegationSink`**（组合根注入 `AggregatorDelegationSink` 转调进程级聚合器；不注入 = 降级为无聚合摘要，不报错）；其二，`TURN_CANCELLED` 与远程工具命名（`<peer>__<tool>` 的构造/剥前缀）下沉定义层（`echo_defs::llm` / `echo_defs::remote_tool`）——注册端、模型侧与远程子代理三方共用同一口径。旧 `packages/subagent/` 已删除
- [x] ⑦ loop.single / loop.parallel → **inproc 声明外迁**：`echo-plugin-loop`（两个纯策略插件的 id / 描述 / manifest 唯一事实来源 + `Service{key:"loop"}` 贡献声明），组合根改为 `echo_plugin_loop::plugins(version)` 装载；`echo-agent::plugins` 侧常量降为 re-export 别名，用户配置里的 `enabled_plugins` 字面量不变
- [x] ⑧ management-panel → **独立 crate**：`echo-plugin-management`（`EventBroker` 扇出 + WS 连接中继 + token 鉴权 + 端口合并规划与 fail-closed 校验 + 心跳保活）。**它本来就是协议驱动的**：入站帧 → `BackendCommand` 塞 bridge、`bridge` 事件 → 带 `seq`/`entry id` 序列化成帧，两侧只碰 `echo-protocol`；此前签名里的 `Arc<Agent>` 已标注 `_agent` 未使用，外迁时直接删掉——**管理面不需要 Agent 句柄**。顺带把 `EntryTracker`（390 行）下沉到 `echo-protocol`：它产出的是线上信封字段，与 `seq` 同层，不该埋在组合根里
- [x] 机制前置：**`echo-tool`**（工具子系统机制：可逆注册 + 定义缓存 + 出口脱敏）从 `echo-agent::packages::tool` 外迁为独立 crate；`echo_agent::tool::…` 经 shim 保持路径，`packages/tool/` 已删除。这是 ②/⑤/⑥ 及一切工具类插件只依赖机制层的前提
- [ ] 每插件模板：独立 crate 化 → 贡献声明 → 状态归属审计 → 双跑对照（新旧路径 diff）→ 删旧路径

**验收**：每个插件可独立禁用/启用/替换；禁用后内核正常降级。

### Phase 4：配置化组合（1-2 周）

- [x] `plugins.toml`：profile → bundles → patch 三层合成（已提交 `65c1c2a`：`echo-plugin-loader`，dsh 语义"按 id 定位、整行替换"；provenance 记录来源层）
- [x] 装载器：拓扑排序（按 requires）、环检测、未解析行可读报错（已提交 `65c1c2a`，13 项测试）
- [x] `dump-config`：CLI 已接线（`echo-agent-core dump-config`，只合成不启动；配置路径前置后置皆可）。做成**子命令**而非计划初稿的 `--dump-config` 标志——与既有 `token` / `port` 同类，`--help` 里可发现，且不会让服务二进制多一个"误触发即不启动服务"的开关。输出每行带 `<id> (layer n: <file>)` 来源注释；`plugins.toml` 缺失时退出码 0（休眠），合成失败时非零并打印与启动路径同一个错误。3 项合成测试 + 3 项 CLI 解析测试
- [x] 内核装配：`plugins.toml`（core.toml 同目录，缺省休眠）→ `PluginSupervisor` 启动 → `RemoteTool` 注册全 persona（`plugins_runtime::start_with_specs`，组合根已接线；`config/plugins.example.toml` 为模板）
- [x] 插件目录约定：`~/.local/libexec/echo-agent-core/plugins/<包名>/<二进制>`（目录名取 Cargo 包名——运行期插件 id 在二进制 manifest 里，安装器静态读不到）。安装器侧已接线：`install.sh` 建目录并按 `PLUGIN_BINARIES` 装二进制（与 Core 同一次 cargo 调用构建，纳入安装事务快照与回滚）；`update.sh` 用同一张表做**版本化替换**（旧版留 `<二进制>.rollback`，后续任一步失败——含健康检查——把全部插件还原，不留 Core 新/插件旧的半新组合）。两脚本清单一致性 + 「仓库里 `[[bin]] path = "src/main.rs"` 的插件 crate」三方比对由 `source/core/tests/install_script_flag.rs` 守护（含 `bash -n` 语法门禁，`update.sh` 一并纳入）
- [x] Panel 对接：插件清单页显示**来源 / 版本 / 状态 / 重启**。清单页本身早已存在（分组、搜索、启停、包规模、受保护锁），缺的是协议层断掉的三块：`PluginInfo.error`（挂载失败原因——`PluginDescriptor` 一直有、`PluginInfo` 曾丢失）、`PluginInfo.config_origin`（声明它的配置层，与 `dump-config` 同源）、`BackendCommand::RestartPlugin`（卸下 + 重挂载，**不改**启用状态；管理面拒绝）。面板侧：行卡「失败」徽标 + hover 重启图标、详情失败块 + 技术信息「配置来源」行 + 头部重启按钮；4 项 vitest 锁交互（重启不误发 Toggle、未启用/受保护不可重启、失败与主动禁用可区分、来源层展示）。**另一处更根本的缺口同批修掉**：`plugins.toml` 的 stdio 外部插件此前从不注册进插件宿主，面板「外部插件」组里只有实例门控壳——现在每个静态条目都注册门控壳（`subprocess` + `builtin = false`，带 `enabled` / `error`）。**再进一步（同批）**：supervisor 增可逆停止（`Control::Stop` / `Control::Start`，与终态 `Drain` 区分），组合根实现 `SubprocessPluginControl` 注入 `PluginHost`，于是 `TogglePlugin` 对外部条目**真的停/起子进程**、`RestartPlugin` = `stop` + `start`；运行态（`state` / `restarts` / `generation`）随 `PluginsList` 下发，操作结果走新的 `PluginOpResult` 结构化回执（不再与 `Error` 混用）；`descriptors()` 改为注册序（保住 `plugins.toml` 的行序 = 挂载顺序）；定义来源（`source_path` / `command` / `args` / `env_keys`——env 只给键名）随清单下发，排查「为什么不起来」不必翻配置文件
- [x] self-update 扩展：更新同步插件二进制（保留回滚）——`update.sh` 与 Core 同一次构建、按同一张 `PLUGIN_BINARIES` 做版本化替换，旧版留 `.rollback`，任一步失败连插件一起还原；详见上一条与 [部署](../ops/ops-deploy.md)「进程外插件二进制同步」

- [x] 删除编译期 mount 调用（P4 收口）：组合根不再逐个 `register_and_mount` 挂载内置插件，改为「构造进内置目录 → 一次 `mount_plan` 按计划挂载」。**构造仍在代码里**（mount 闭包要捕获端口 / token / 实例句柄 / broker 等运行期句柄），交给配置的是**集合 / 顺序 / 启用状态**：`plugins.toml` 的 `kind = "builtin"` 行按行序挂载、可 `enabled = false`；**未提及的内置插件一律照旧挂载**（现网 plugins.toml 多只登记子进程插件，缺省必须零变化）。未知 id 只告警不致命；装配失败仍 fail-fast（与逐插件 `?` 同语义）。5 项计划解析测试 + 3 项配置读取测试 + 真实启动冒烟（无 plugins.toml = 9 项全挂载，与改造前同集合）

**验收**：✅ 子进程插件经 `plugins.toml` 可达并注册工具（实例插件即走此路径）；✅ 内置插件的**组合**（集合/顺序/启用）也由配置决定，改组合不改代码。Phase 3 之后仍内联在 echo-agent 的只剩 `tools_builtin` / `instance_ops` / `federation` 三包——它们缺的是**实现**外迁，与"组合配置化"是两条线。

### Phase 5：热重载与版本治理（1-2 周）

- [x] 统一"重载 = 卸载 + 装载"语义（数据/代码插件同模型；inproc 挂载/卸载 + subprocess 重启式均就绪）
- [x] subprocess 热替换（已提交 `df83fdb`）：`PluginHandle::upgrade(config, timeout)`——新进程 Ready → 原子切流量 → 老进程 Drain → 退出（失败保老，可回滚）；5 项测试（含配置代际验证）
- [ ] dylib 热重载（实验；轨本身为可选，暂缓）
- [x] 协议版本治理：`compatible()` 版本协商 + capabilities 常量与双方声明（已提交 `4e2f82e`/`11c40fc`）；不兼容的可读拒绝
- [ ] 状态迁移协议（可选实现）：`export_state` / `import_state`
- [ ] （远期可选）蓝绿双实例切换

**验收（进行中）**：升级流程已可测（新实例 Ready → 切流量 → 老实例 Drain）；"改源码 → 重构建插件 → 热替换 → 会话不中断"的端到端演示随 P3 全量外迁收尾。

### Phase 6：安全与生态（持续）

- [x] 插件资源限额（rlimit）：`plugins.toml` 的 `[plugin.limits]` / `[instances.<platform>.plugin.limits]` → `StdioTransport::with_limits` → `exec` 前 `setrlimit`（CPU 时间 / 地址空间 / 单文件大小 / fd 数 / 进程数，五项各自可选，**缺省不限制**）。硬限额（软=硬）、继承给后代；`0` 被加载器拒绝（`setrlimit` 里 0 = 完全禁止，不是"不限制"）。cgroup 轨未做——rlimit 已覆盖"单个插件失控"这一类，cgroup 解决的是"多插件共享预算"，收益/成本不划算
- [ ] 消息大小/频率上限（**帧上限 16 MiB 已落地**；频率/数量限额待做）、调用超时强制（已落地：invoke 超时=deadline）
- [ ] 权限声明（capabilities：fs / net / proc）与宿主中介（远期）；**环境隔离已落地**（env_clear + 仅 PATH + 显式 env）
- [x] SDK 与脚手架：插件侧运行时（echo-plugin-sdk）+ 试点示例 + [插件开发指南](../plugins/plugin-authoring.md)（协议文档含在上手指南内；JSON Schema 导出待做）
- [x] 文档生成门禁（对齐 dsh 做法，取其**可执行的那一半**）：`source/core/tests/doc_catalog.rs` 三项校验——① **工具目录自动生成 + 新鲜度**（`document/plugins/tool-catalog.md` 由 `register_all` 的真实注册表渲染，加/改工具不同步即红；`UPDATE_DOCS=1` 刷新）；② **内置插件 id 不许漏**（`BUILTIN_PLUGIN_IDS` 每项必须出现在 `core-plugins.md`）；③ **相对链接可达**（`document/` 全树死链检查——重命名/拆章最容易留下的坑）。模块图 / 能力图 / 事件矩阵未做：前两者需要图形渲染器（ProDoc 侧职责），事件矩阵已由 Panel `protocol-sync.test.ts` 的变体计数快照跨仓兜住
- [x] 文档一致性（人工复核 + 门禁）：本轮把「外部条目重启被拒」等**已被实现推翻**的旧表述全部改掉（rlimit / 可逆停止 / 子进程控制面落地后，原文说"重启按钮会回请重启 Core"已不成立）；死链 0 条
- [x] CI：fmt / clippy `-D warnings` / 全量测试（stable+nightly）/ **依赖方向门禁已扩展到插件分层**（api→零依赖、sdk→api、host→api+loader、loader→零、example→sdk；已提交 `65c1c2a` 起）
- [x] 无全局态门禁（`no_global_state.rs`，随 workspace 测试自动纳入 CI）

## 五、风险、工作量与回退

- **subprocess 开销可忽略但非零**：实测往返 20-60µs/次（含调度与解析，~2-5 万次/秒）；~10 进程 × 几 MB；LLM 流式按帧批量发送（或 provider 留 inproc）。对 agent 场景（工具调用秒级间隔）富余。
- **破坏性改动**：`packages/` 全拆、`plugins.rs` / `main.rs` 装配重写、Panel 协议扩展——已获授权；以 357+ 测试为安全网 + 双跑对照降回归。
- **工作量**：单人 6-10 周（Phase 0-5；协议层单项调研口径：全子进程 3-6 人周、混合 8-15 人周）；Phase 6 持续。可并行：协议/宿主（内核侧）与插件迁移（插件侧）分线。
- **回退策略**：每阶段独立可回滚；每插件迁移保留开关直至验收。

## 六、改造前基线

> 下表是 **2026-09 改造启动前**的冻结快照，只用于对照「改造前后量级」，不要当现状读：`adapter_qq` 包与 `echo-adapter` crate 已于 2026-10 删除（现为独立插件仓库），技能数已增至 13，crate 数与测试数也随拆分变化。现状以 [架构总览](../overview/architecture.md) 为准。

| 项 | 数值 |
|---|---|
| crate 数 | 17（单向无环，已核） |
| `Ctx` 实际使用 | 2 次注册（llm / loop）；resolve 几乎未用 |
| 测试 | echo-agent 325（lib）+ 33（集成）；echo-session 38；echo-loop 9；echo-context 23 |
| 工具 / 技能 | 内置工具约 20 个；技能 9 个 |
| packages 体量（行） | tools_builtin 3079 / workspace 1806 / federation 1427 / adapter_qq 1289 / skills_dir 990 / subagent 622 / tool 582 / provider_llm 171 |

## 七、与 DeepSeek Harness 的对照（达成度边界）

"像 dsh 一样完全解耦"需要拆成两层看——dsh 的解耦一半来自**架构设计**（可移植），一半来自 **TypeScript/Node 动态语言**（不可移植，需要替代机制）：

| # | dsh 的解耦要素 | 来源 | 本计划 | 判定 |
|---|---|---|---|---|
| 1 | 一切皆插件、无特权内核 | 架构（Cordis） | P1-P3 全量插件化（session/panel 亦 inproc 协议化） | ✅ 可对齐 |
| 2 | 服务注册表 `ctx.<key>` | 架构 | Ctx v2 强类型键（P1） | ✅ 可对齐 |
| 3 | 可逆注册（Disposer / effect） | 架构 | 已有 Disposer，P2 统一 | ✅ 可对齐 |
| 4 | 类型化事件 + 4 种分发 | 架构 | EventBus 四模式已存在，扩展事件面 | ✅ 可对齐 |
| 5 | 配置化组合（profile/bundle/patch） | 架构 | P4（plugins.toml 三层合成） | ✅ 可对齐 |
| 6 | 运行期替换组件（不改代码） | 架构 | inproc 挂载/卸载 + subprocess 重启式 | ✅ 可对齐 |
| 7 | 零全局态 | 架构 | P1（24 处 → 0 + 门禁） | ✅ 可对齐 |
| 8 | 插件崩溃隔离 | 机制 | subprocess 轨（dsh 自身**没有**此项，同进程插件崩溃拖垮整个 Node 进程） | ✅ 超越 |
| 9 | 跨语言插件 | 机制 | 协议即 ABI（dsh 锁定 TS/JS 生态） | ✅ 超越 |
| 10 | **源码级 HMR**（编辑 .ts 即生效） | **语言**（动态 import） | ❌ Rust 静态编译无此能力；替代：subprocess 重启式替换（百毫秒级）、dylib 重编译+重载（秒级）、实验性函数级热补丁方案（如 hot-lib-reloader / Dioxus subsecond 一类，不进生产） | ⚠️ 功能等价、体验不同 |
| 11 | **零构建插件创作**（npm 包即插件） | **语言** | ❌ 需编译（Rust crate）或写独立进程（任意语言 + 协议，反而更开放） | ⚠️ 门槛略高 |
| 12 | **跨插件编译期类型安全**（TS declaration merging） | **语言** | inproc 保持强类型；跨进程边界降为协议 schema + conformance 测试 | ⚠️ 边界处弱化 |

**结论（写入验收口径）**：本计划可保证达成 #1-#9——即 **dsh 架构意义上的完全解耦**（可替换、装配化、无特权内核、配置组合、服务定位），并在崩溃隔离与跨语言两点**超出 dsh**；#10-#12 是语言本质差异，以"功能等价、体验不同"为验收（替换能力达成；"编辑即生效/零构建"不承诺）。若这 12 项判定中任何 ⚠️ 项被要求"必须 1:1"，则该目标在 Rust 上**不可达成**——这是需要在立项时明确的边界。

## 八、P1 迁移架构（执行附注）

### 8.1 静态 → 服务键映射（17 个进程级静态单元；全 source 扫描口径）

| 静态 | 位置 | 读取点（约） | 目标 |
|---|---|---|---|
| `GLOBAL_PLUGIN_HOST` | agent/mod.rs | 4 | `ctx.plugin_host` |
| `GLOBAL_POLICY` | agent/mod.rs | 6 | `ctx.global_policy` |
| `FEDERATION_COMMAND_HANDLER` | agent/mod.rs | 5 | `ctx.federation.command_handler` |
| `GLOBAL_MANAGER` | agent_manager.rs | 22 | `ctx.agent_manager` |
| `AGENT_FACTORY` | agent_manager.rs | 3 | `ctx.agent_factory` |
| `NODE_ID` / `REGION_NAME` | lib.rs | 132 / 18 | `ctx.node_identity`（`NodeIdentity { id, region }`） |
| `REMOTE_SUBAGENT_NOTIFIER` / `AGGREGATE_DELIVER` / `REMOTE_INVOKER` / `REMOTE_QUERIER` | packages/federation/mod.rs | 2 / 2 / 3 / 2 | `ctx.federation.*`（随联邦包整体迁移） |
| `GLOBAL_SHELL` / `SHELL_EMIT` | shell.rs | 若干 | `ctx.shell`（管理器 + 发射句柄） |
| `SESSION_IMPORT_ACKS` | core/federation_import.rs | 4 | 随联邦插件（`ctx.federation.import_acks`） |
| `SELF_STOP_RE` / `CLIENT` | tools_builtin | — | 降级为**实例级**惰性态（不再进程全局） |
| `CLIENT`（图片下载） | echo-adapter-qq/handler.rs | — | 同上（纯缓存，实例级） |

### 8.2 无 ctx 调用点的两条路径

getter 调用点（合计 ~200 处）多在深层代码、不持有 ctx：

- **A. 参数线程化（目标态，零静态）**：窄接口随构造期注入（联邦子系统装配时拿
  `NodeIdentity` 句柄、帧构造函数从参数取）。node_id 的 132 处集中在联邦包与状态上报，
  **随 P3 联邦外迁一并线程化**，不在 P1 内强推。
- **B. KernelCell 过渡（单一白名单，P5 消除）**：`echo-context` 增设唯一引导单元
  `kernel() -> Option<Arc<Ctx>>`（写一次 + `Arc` 持有）；其余 getter 过渡期经
  `kernel().resolve(KEY)` 读取。棘轮门禁对 `echo-context/src/kernel.rs` 单独白名单；
  其余文件恒为 0。终态以 A 消除本单元。

### 8.3 执行顺序（每步独立提交 + 棘轮下调）

1. Ctx v2（typed keys / conflict / `service_or_wait`）——进行中
2. KernelCell 引导单元 + 门禁白名单
3. setter 侧：组合根全部 `provide`（旧 setter 兼容期双写）
4. getter 侧逐个迁移：plugin_host → policy → manager/factory → shell → federation 组 → node_identity
5. 删除旧 static 与 setter（一次到位，破坏性）；棘轮 34 → 白名单级
6. 终态跟踪：node_identity / federation 的线程化（随 P3/P5）
