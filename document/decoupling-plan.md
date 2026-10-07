---
id: decoupling-plan
title: "完全解耦推进计划"
group: 规划
x: -900
y: 1600
---

# 完全解耦推进计划（动态库 / 子进程）

> 状态：**规划**（2026-10-07 立项）。本页是路线图，不是当前实现的描述；实施进度以本页清单为准。
> 前置阅读：[架构总览](./architecture.md)（现状与五条原则）、[插件化设计](./core-plugins.md)。

## 一、验收标准（"完全解耦"的可测口径）

全部为自动化可断言项，达成即视为"完全解耦"：

1. **零全局态**：`grep -rn "OnceLock" source --include='*.rs' | grep -v tests` = 0——所有状态经 `Ctx` 服务键寻址。
2. **一切皆插件**：内核只含机制（服务注册表 / 事件总线 / 插件宿主 / 协议）；全部能力（循环、provider、工具、技能、适配器、工作区、subagent、联邦）以插件装载，配置可禁用到"只剩内核仍能启动"。
3. **运行期热替换**：任一代码插件可"卸载 → 替换 → 重挂载"，无进程重启；与数据插件热重载语义统一。
4. **进程外插件端到端**：至少一个示例插件以**独立二进制**交付、经配置装载；`SIGKILL` 其进程后内核无感（该插件工具报错并按策略自动重启）。
5. **配置化组合**：`--dump-config` 输出分层合成树（profile / bundle / patch 三层）；改组合不改代码。
6. **协议一致性**：同一套 conformance 用例覆盖全部运输层（inproc / subprocess / dylib）。

## 二、方案研究：动态库 vs 子进程

### 2.1 动态库（dlopen）

Rust **没有稳定 ABI**——跨 `dlopen` 边界只能走 C ABI（`extern "C"` + `#[repr(C)]`），或引入稳定 ABI 库（`abi_stable` 的 `StableAbi`/`RootModule`、较新的 `stabby`）。技术路线：

```text
插件编译为 cdylib → 宿主 libloading::Library::new + 取单一入口符号：
  #[no_mangle] pub extern "C" fn echo_plugin_v1(host: *const HostApiV1) -> *const PluginVtableV1
HostApiV1 / PluginVtableV1 = #[repr(C)] 函数指针表；数据过界一律 JSON 字符串
（*const c_char + len + echo_free），不假设任何 Rust 类型布局。
```

关键风险（决定了它只能当实验轨）：

- **unsafe 面积大**：每处跨界调用都是 unsafe；panic 过 FFI 是 UB（`extern "C"` abort-on-unwind），必须**双向 `catch_unwind`**；要求 `panic=unwind`（当前 workspace 已是默认 unwind，兼容）。
- **卸载不安全**：`dlclose` 时若插件仍有线程/任务/TLS 存活 → UB/崩溃。安全卸载需要插件完全静默（quiesce）+ 引用计数归零；生产通常只做"逻辑卸载"（停流量，不 dlclose）。
- **版本地狱**：宿主与插件必须同工具链、同依赖图编译，否则类型布局漂移 = 崩溃；CI 必须锁死。
- **异步不可跨 FFI**：只能改成"双向消息 + call_id"（与子进程协议同构）——即消息化是前提，不是选择。

### 2.2 子进程（stdio 协议）

- **协议即 ABI**：跨进程无 ABI 问题，天然稳定，且语言无关。
- **崩溃隔离**：插件 panic / segfault / OOM 只死自己，宿主监督重启——装载外部代码的最大可靠性收益。
- **热替换简单**：停旧进程 → 起新进程；状态迁移显式化（可选 `export_state/import_state`）。
- **异步天然**：插件是普通进程，自带运行时；宿主用 `tokio::process` 管理即可。
- **代价**：序列化（JSON 微秒级）、进程开销（空载数 MB × ~10 进程）、管道延迟（本地百微秒级）。对 agent 场景（工具调用秒级间隔）可忽略；LLM 流式按帧批量发送。
- **工程先例**：nushell 插件（全部 out-of-process）、MCP stdio 服务器（JSON-RPC，dsh 的 `mcp-client` 即"外部进程提供工具"的一等公民）、LSP。dsh 自身即用此模式装载外部能力。

技术路线：

```text
插件 = 独立二进制；宿主 tokio::process spawn，管道 stdio。
帧编码：NDJSON（serde_json 至多产生转义换行，天然安全）+ 单行大小上限；
大消息优化（LSP 式 Content-Length 头）与二进制编码（MessagePack）留 feature。
帧协议沿用 FedFrame 既有惯例：externally-tagged enum、#[serde(default)] 前向兼容、
call_id 关联、Hello/Welcome 版本握手、显式 Cancel。
生命周期：握手（超时）→ Ready → 服务；SIGTERM 优雅退出（drain deadline）→ 超时 SIGKILL；
进程组整体清理；stderr → tracing 转发；环境白名单 + cwd 受控 +（可选）rlimit。
```

### 2.3 对比总表

| 维度 | inproc（协议化内联） | 子进程（stdio 协议） | 动态库（C ABI） | wasm（远期） |
|---|---|---|---|---|
| ABI 稳定性 | 同编译单元，无问题 | **协议=ABI，完全稳定** | 需 C ABI/稳定库，脆弱 | 稳定（组件模型） |
| 崩溃隔离 | 无 | **完全** | 无（同进程） | 完全（沙箱） |
| 热替换 | 挂载/卸载（已支持） | **重启式，最简** | 需安全卸载，难 | 实例重建，易 |
| 性能开销 | 零（channel） | µs~ms 级 | ~ns | µs 级 |
| unsafe 面积 | 零 | 零 | 大 | 宿主零 |
| 语言无关 | 否 | **是** | 否 | 是 |
| 推荐用途 | 内核伴生 / 热路径 | **外部插件主路径** | 开发期热重载（实验） | 未来不可信沙箱 |

### 2.4 结论：三轨制 + 一套协议

1. **inproc（主基座）**——现有内置插件迁移为"协议化内联插件"：接口按消息定义、物理编译内联，性能零损失；
2. **subprocess（扩展主路径）**——任何插件可无改动"提升"为独立进程；第三方/外部插件一律走此轨；
3. **dylib（实验轨，可选）**——仅用于开发期快速热重载，C ABI + JSON 载荷，明确标注 unstable，不进生产默认；
4. wasm 作为远期"不可信插件沙箱"预留（不在本期）。

## 三、目标架构

```text
┌── echo-kernel（内核：只含机制）────────────────────────────┐
│ Ctx v2 服务注册表（强类型键 / 依赖声明 / scope）             │
│ EventBus（emit / waterfall / parallel / serial，已有）       │
│ PluginSupervisor（加载 / 卸载 / 健康 / 重启 / 限额 / 日志）   │
│ echo-plugin-api（协议类型：冻结契约）                        │
│ Transport：Inproc / Stdio / [Dylib]                        │
│ 贡献注册表（工具 / 技能 / 服务 / 事件——全部可逆）             │
│ 会话编排骨架（事件日志 + 投影 + 压缩编排）※                   │
└──────────────────────────────────────────────────────────┘
        ▲ 协议（同一组消息，三种载体）
        ▼
┌── 插件（每个独立 crate；可内联编译或独立二进制）──────────────┐
│ provider-llm  tools-*  skills-dir  workspace  adapter-qq   │
│ subagent  loop-*  federation  …（management.panel 留内核）  │
└──────────────────────────────────────────────────────────┘
```

※ 会话/trunk 归属：**先留内核骨架、按插件语义管理**（dsh 将 session 也做成插件；我方投影/压缩与 agent 深度互锁，本期留内核、接口按服务键暴露，后续可再迁）。

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

- [ ] 新建 `source/plugin/echo-plugin-api`：消息类型 / 贡献类型 / 错误 / 协议版本常量 / 协议文档
- [ ] 新建 `source/plugin/echo-plugin-host`：`Transport` trait + `PluginSupervisor` 骨架
- [ ] 编码决策落地：跨进程 NDJSON + inproc 类型直连；Content-Length / MessagePack 留 feature
- [ ] conformance 测试套件骨架（握手 / 注册 / 调用 / 结果 / 取消 / 超时 / 事件 / 崩溃 / 卸载 九组用例，按 transport 参数化）
- [ ] **冻结**：协议变更需过 conformance + bump 版本；纳入 CI

**验收**：conformance 在 inproc 参考实现上全绿。

### Phase 1：内核化——激活 Ctx、杀死全局态（1-2 周）

目标：24 处全局 static 归零；启动由"服务可用性"驱动。

- [ ] Ctx v2：
  - [ ] 强类型服务键（`pub const LLM: ServiceKey<Arc<dyn LlmProvider>>`）替代字符串键
  - [ ] 注册冲突检测 + `provides/requires` 依赖声明（启动期拓扑校验）
  - [ ] `resolve_or_wait`（就绪等待——替代手工编排启动序）
- [ ] 全局态迁移（24 处 → 0）：
  - [ ] `GLOBAL_PLUGIN_HOST` → `ctx.plugin_host`
  - [ ] `GLOBAL_MANAGER` / `AGENT_FACTORY` → `ctx.agent_manager` / `ctx.agent_factory`
  - [ ] `GLOBAL_POLICY` → `ctx.global_policy`
  - [ ] `GLOBAL_SHELL` / `SHELL_EMIT` → `ctx.shell_manager`
  - [ ] `FEDERATION_*`（5 处）→ `ctx.federation.*`（随联邦插件走）
  - [ ] `NODE_ID` / `REGION_NAME` → `ctx.node_identity`
  - [ ] `FEDERATION_COMMAND_HANDLER` / `SESSION_IMPORT_ACKS` → 同上
  - [ ] websearch `CLIENT` / `SELF_STOP_RE` → 插件内惰性态（非进程全局）
- [ ] Agent 结构体瘦身：`config_store` / `provider` / `event_sink` / `loop_runner` / `plugin_host` 改为经 Ctx 解析
- [ ] 门禁：新增 `no_global_state.rs` 测试（扫描 src 断言零命中）

**验收**：门禁绿 + 既有 357/38/9/23 项测试全绿；`echo-agent` 对具体实现的依赖收敛到定义层。

### Phase 2：插件宿主与运输层（2 周）

- [ ] `PluginSupervisor`：
  - [ ] 生命周期状态机（pending → ready → draining → stopped；backoff 重启 + N 次熔断）
  - [ ] in-flight 调用跟踪：卸载先 Drain（等待或取消到 deadline）
  - [ ] 日志转发（stderr → tracing，带插件前缀）/ 指标
  - [ ] 崩溃策略按插件配置：always / on-failure / never
- [ ] `InprocTransport`：现有 `BuiltinPlugin` 挂载闭包升级为协议消息循环
- [ ] `StdioTransport`：spawn / 进程组杀死 / NDJSON 读写 / 背压 / 环境清理 /（可选）rlimit
- [ ] `DylibTransport`（实验）：libloading + C ABI vtable + 双向 `catch_unwind` + 符号版本校验；默认"逻辑卸载"
- [ ] 参考插件：`echo-plugin-example`（独立二进制 + inproc 双形态）
- [ ] conformance ×3 全绿 + 崩溃注入测试（SIGKILL）

**验收**：示例插件工具经三种运输均可用；杀死进程后 5s 内自动重启、内核无感。

### Phase 3：内置插件外迁（2-3 周，逐个）

顺序：先低耦合、先多收益。

- [ ] ① provider-llm（已单点装配，最易）→ 协议化
- [ ] ② 工具子集试点：websearch + calculator → **独立二进制**（全链路验证：schema 注册 → 调用 → 取消 → 崩溃重启）
- [ ] ③ skills-dir
- [ ] ④ adapter-qq → subprocess（崩溃隔离收益最大）
- [ ] ⑤ workspace
- [ ] ⑥ subagent（hook 接入改经协议事件）
- [ ] ⑦ loop.single / loop.parallel → inproc（保持内联；注入改经协议挂载）
- [ ] ⑧ management.panel：**留内核**（控制平面，自锁风险）
- [ ] 每插件模板：独立 crate 化 → 贡献声明 → 状态归属审计 → 双跑对照（新旧路径 diff）→ 删旧路径

**验收**：每个插件可独立禁用/启用/替换；禁用后内核正常降级。

### Phase 4：配置化组合（1-2 周）

- [ ] `plugins.toml`：profile → bundles → patch 三层合成（dsh `cordis.patch.yml` 语义：按 id 定位、整行替换）
- [ ] 装载器：拓扑排序（按 requires）、环检测、未解析行可读报错
- [ ] `--dump-config`：打印合成树（标注每行来源层）
- [ ] 插件目录约定：`~/.local/libexec/echo-agent-core/plugins/<id>/`（版本化路径）
- [ ] Panel 对接：插件清单页显示来源/版本/状态/重启（协议扩展走 echo-protocol）
- [ ] self-update 扩展：更新同步插件二进制（保留回滚）

**验收**：删除编译期 register 调用；纯配置可达与今日等价的功能集。

### Phase 5：热重载与版本治理（1-2 周）

- [ ] 统一"重载 = 卸载 + 装载"语义（数据/代码插件同模型）
- [ ] subprocess 热替换：新进程 Ready → 切流量 → 老进程 Drain → 退出（失败保老，可回滚）
- [ ] dylib 热重载（实验）：quiesce 检查 + 引用归零；不安全场景禁止卸载
- [ ] 协议版本治理：semver 协商 + capability 列表 + 不兼容的可读拒绝
- [ ] 状态迁移协议（可选实现）：`export_state` / `import_state`
- [ ] （远期可选）蓝绿双实例切换

**验收**：演示"改源码 → 重构建插件 → 热替换 → 会话不中断"全流程。

### Phase 6：安全与生态（持续）

- [ ] 消息大小/频率上限、调用超时强制、插件资源限额（rlimit / cgroup）
- [ ] 权限声明（capabilities：fs / net / proc）与宿主中介（远期）
- [ ] SDK 与脚手架：crate 模板、示例、协议 JSON Schema
- [ ] 文档生成门禁（对齐 dsh 做法）：模块图 / 能力图 / 事件矩阵 / 工具目录自动生成 + 新鲜度校验
- [ ] CI 矩阵：conformance × transport、无全局态门禁、协议兼容守护

## 五、风险与取舍（诚实清单）

1. **dylib 不是银弹**：Rust 无 ABI 承诺是本质约束；本计划将其限制在实验轨与开发期热重载，生产主路径为 inproc / subprocess。
2. **subprocess 开销**：~10 进程 × 几 MB；LLM 流式需批量帧优化（或 provider 留 inproc）。可接受。
3. **破坏性改动**：`packages/` 全拆、`plugins.rs` / `main.rs` 装配重写、Panel 协议扩展——已获授权；以 357+ 测试为安全网 + 双跑对照降回归。
4. **工作量**：单人 6-10 周（Phase 0-5）；Phase 6 持续。可并行：协议/宿主（内核侧）与插件迁移（插件侧）分线。
5. **回退策略**：每阶段独立可回滚；每插件迁移保留开关直至验收。

## 六、改造前基线（2026-10-07 盘点）

| 项 | 数值 |
|---|---|
| crate 数 | 17（单向无环，已核） |
| `Ctx` 实际使用 | 2 次注册（llm / loop）；resolve 几乎未用 |
| 全局 static | 24 处（8 文件） |
| 测试 | echo-agent 325（lib）+ 33（集成）；echo-session 38；echo-loop 9；echo-context 23 |
| 工具 / 技能 | 内置工具约 20 个；技能 9 个 |
| 测试 | echo-agent 357 + echo-session 38 + echo-loop 9 + echo-context 23 |
| packages 体量（行） | tools_builtin 3079 / workspace 1806 / federation 1427 / adapter_qq 1289 / skills_dir 990 / subagent 622 / tool 582 / provider_llm 171 |
