# 插件框架外迁（M0：边界冻结与版本策略）

本文是「把插件化框架从 Core 中分离为可独立复用的通用框架」的基线文档。
后续 M1–M5 的执行记录追加到本文末尾，**边界与判据以本文为准**。

- 背景与动机见 [decoupling-plan.md](./decoupling-plan.md) 第七节（与 dsh/Cordis 的对照）
- 当前状态见 [architecture.md](../overview/architecture.md) 的 crate 表

## 一、为什么需要这一步

`decoupling-plan.md` 第七节已达成的是**应用自身**的插件化程度（#1–#9）。
但那个口径不等于「分离出一个业务无关的框架」，两者的差距在**发行层面**：

| 维度 | 已达成的（应用内解耦） | 本计划要达成的（真框架） |
|---|---|---|
| 依赖方向 | 宿主不依赖业务 crate | 同左（已达成，需门禁锁死） |
| 业务词汇 | host/api 里无领域词 | 同左（**但 `MountContext` 仍有 `tools`/`skills`**，M2 清理） |
| 可独立编译 | 否（只在 Core workspace 内） | 是（外部 `git clone` 即编译） |
| 版本 | 共享 Core 的 `0.1.0` | 独立版本 + 稳定 tag |
| 使用者 | 零（第一个适配器被绝对路径卡住） | 外部作者可用；有跨语言插件示例 |

实际触发点：`plugin-adapter-qq`（独立仓库）依赖
`echo-plugin-sdk`，而 SDK 只存在于 Core 的源码树里，只能写本机绝对路径：

```toml
echo-plugin-sdk = { path = "/home/evence/.local/share/echo-agent-core/source/source/plugin/echo-plugin-sdk" }
```

任何人 `git clone` 该仓库都无法编译——**这就是「框架没分离出去」的可观测证据**。

## 二、边界冻结（M0 核心产物）

### 2.1 判据

一个 crate 属于**框架**，当且仅当同时满足：

1. **零领域依赖**：不依赖 `echo-defs` / `echo-protocol` / `echo-agent` / 任何 `echo-llm-*`；
2. **对第三方有意义**：外部作者写插件或写宿主时要直接依赖它；
3. **可独立编译**：脱离 Core workspace 后 `cargo test` 能过。

任一条不满足即留在应用层。这条判据同时是 M3 的验收口径。

### 2.2 框架六件套（迁出目标）

| crate | src 行 | 测试 | 职责 | 依赖 |
|---|---|---|---|---|
| `echo-context` | 1540 | 43 | 服务定位 `Ctx` + 强类型 `ServiceKey` + 四模式 `EventBus` + 可逆注册 `Disposer` + `ScopedRegistry` | tokio, dashmap, thiserror |
| `echo-plugin-api` | 816 | 10 | **冻结协议**：`Hello`/`Invoke`/`Register`/`Cancel`/`Drain`/`HostCall`；贡献类型；版本协商 | serde |
| `echo-plugin-loader` | 810 | 17 | 分层配置合成（profile → bundle → patch）+ provenance + 依赖排序 | serde, toml, thiserror |
| `echo-plugin` | 1184 | 18 | manifest + 生命周期 + registry（门控：挂载/卸载/启停） | echo-context |
| `echo-plugin-host` | 2563 | 51 | 运输层（inproc/stdio）+ supervisor（握手/调用/取消/排空/崩溃重启）+ conformance 套件 | echo-plugin-api, echo-plugin-loader |
| `echo-plugin-sdk` | 1620 | 14 | 插件侧运行时：帧读写 + 握手 + serve 循环 + 回呼 | echo-plugin-api |
| **合计** | **8533** | **153** | | |

> 注：`echo-plugin-host` 内含 `src/bin/echo-plugin-test-peer.rs`（stdio conformance 的对端二进制），随 crate 一起走，不是独立 crate。
> 注：`echo-plugin-sdk` 依赖 `echo-plugin-api`，因此两者**必须同批迁移**（M1 一次走两个）。

### 2.3 不属于框架（留在应用层）

| crate | 为什么不是框架 |
|---|---|
| `echo-defs` | 2747 行**应用领域词汇**（`ChatMessage`/`Tool`/`LlmProvider`/`Skill`/`PluginInstance`）。它是「一个 AI agent 应用长什么样」，不是「插件系统长什么样」 |
| `echo-plugin-adapter` | **认识** `echo-defs` 的 trait（`RemoteTool`/`RemoteInstance`）。存在的意义就是「把领域无关的贡献翻译成应用领域对象」——这是应用侧职责 |
| `echo-protocol` | Agent/Panel 之间的应用协议（时间线、会话、团队），与插件无关 |
| `echo-agent` | 业务内核 |
| `echo-plugin-{tools,skills,loop,subagent,workspace,provider-llm,management}` | **框架的使用者**（内置插件），是应用内容不是机制 |
| `echo-plugin-example` | 示例插件，依赖 host+sdk；作为框架仓的 `examples/` 更合适（M4 决定） |

### 2.4 依赖方向（外迁后）

```
        下游（应用侧，留在 Core）
  echo-agent / echo-plugin-*  / echo-protocol / core
                │
                ▼
  ┌───────────────────────────────────────┐
  │  框架仓 plugin-framework（六件套）      │
  │                                       │
  │  echo-plugin-host ──► echo-plugin-api │
  │        │                    ▲         │
  │        ▼                    │         │
  │  echo-plugin-loader   echo-plugin-sdk │
  │                                       │
  │  echo-plugin ──► echo-context         │
  └───────────────────────────────────────┘
                ▲
                │  其余全部零 echo-* 依赖（叶子）
```

铁律：**框架仓内的任何 crate 都不许出现指向 Core 的依赖**。反向依赖由框架仓自己的
`tests/no_core_dependency.rs`（M2 新增）与 Core 侧 `scripts/deps-policy.txt` 双向把守。

## 三、版本与兼容策略（M0 决策）

### 3.1 版本号

| 对象 | 策略 |
|---|---|
| 框架仓 | **单一版本**（workspace 共享，与现在 `version.workspace = true` 一致）。理由：六件套是一个整体契约，给第三方作者的认知成本是「一个版本号」而不是六个 |
| 初始版本 | `v0.1.0`（M1 首个 tag），与现状一致，不含任何承诺 |
| 何时进 1.0 | 达成 M4 全部验收（外部最小宿主示例可跑 + 跨语言插件示例 + 文档齐备）之后 |
| 0.x 期间 | **允许 breaking**，但必须在 `CHANGELOG.md` 记录，且 Core 侧升级时同步改一行依赖 |

### 3.2 协议版本与 crate 版本解耦

`PROTOCOL_VERSION`（当前 `1`，`echo-plugin-api/src/protocol.rs`）是**线上契约的版本**，
crate 版本是**API 表面版本**，两者独立：

- 改线上帧格式（新增字段除外，`#[serde(default)]` 保证向后兼容）→ bump `PROTOCOL_VERSION`，握手拒绝旧端
- 改 Rust API 表面（加 trait 方法、改泛型）→ bump crate 版本，bump `PROTOCOL_VERSION` 通常**不需要**
- 只加东西且两边都能解码 → 两个版本都不必动

这条规则已经在 `echo-plugin-api/src/lib.rs` 的模块文档里写着（"协议变更必须过 conformance
测试套件并按需 bump `PROTOCOL_VERSION`"），M0 只是把它确认为**跨仓的**承诺。

### 3.3 Core 如何钉框架版本

```toml
# Core 的 Cargo.toml（M1/M3 之后）
echo-context     = { git = "https://github.com/EchoLab-Auto/plugin-framework", tag = "v0.1.0" }
echo-plugin-api  = { git = "…", tag = "v0.1.0" }
# … 六件套同 tag
```

配合仓库内的 `Cargo.lock`（已跟踪）钉死 rev：tag 只作为**人读的版本标签**，
实际解析结果由 `Cargo.lock` 决定，因此「上游 tag 被移动」不会静默改变我们的构建。

### 3.4 插件作者如何钉版本

`plugin-adapter-qq` 这类**插件**仓库只需两件：

```toml
echo-plugin-sdk = { git = "https://github.com/EchoLab-Auto/plugin-framework", tag = "v0.1.0" }
```

`echo-plugin-sdk` 已 re-export `echo_plugin_api`，插件作者通常**不需要**单独声明 api。

## 四、基线（M0 记录，2026-10-19）

### 4.1 测试基线

`cargo test --workspace` 全绿，**1059 通过 / 0 失败**（含 doctest 3 项）。
框架六件套贡献 153 项：

| crate | 用例数 |
|---|---|
| echo-context | 43 |
| echo-plugin-api | 10 |
| echo-plugin-loader | 17 |
| echo-plugin | 18 |
| echo-plugin-host | 51（含 conformance 14 + conformance_stdio 10 + upgrade 10 + bootstrap 9 + host_call 3 + lib 5） |
| echo-plugin-sdk | 14 |

### 4.2 门禁基线（外迁后必须仍然生效）

| 门禁 | 现状（已实测其扫描口径） | 外迁后必须做的事 |
|---|---|---|
| `scripts/deps-policy.txt` | 30 个 crate 全部登记（与 `Cargo.toml` members 一一对应），方向违规即红 | **必须删掉六件套的 6 行**：`every_crate_depends_only_on_what_the_policy_allows` 会遍历策略表每一项并断言 `source/<dir>/Cargo.toml` 存在，留着会红 |
| `source/core/tests/plugin_dependency_direction.rs` | 读同一份策略文件；`discovered_crates()` 扫 `source/*/*/Cargo.toml`，未登记的 crate 报错 | 目录消失后扫描自动不再覆盖框架（无需改代码），但也**不再守护**它 → 对齐门禁由框架仓自有测试补上（M2） |
| `source/core/tests/no_global_state.rs` | 扫 `source/` 全树，跳过 `tests/` 与 `tests.rs`，`EXPECTED_MAX=0` | 同样自动不再覆盖框架 → 框架仓需自建等价门禁（否则「零全局态」在迁出后失去守护） |
| `source/core/tests/doc_catalog.rs` | 工具目录新鲜度 + 内置插件 id 覆盖 + `document/` 死链三查 | 框架仓文档不在 Core 的 `document/` 树内，需各自维护（M4）；Core 侧模板与工具目录不受影响 |

### 4.3 部署基线

release 二进制 `~/.local/libexec/echo-agent-core/echo-agent-core-bin`（md5 随每次构建变化），
插件目录 `~/.local/libexec/echo-agent-core/plugins/{echo-adapter-qq,echo-plugin-example}`。
外迁不得改变这两个路径与 `core.toml` 的 `[adapters.qq.plugin] command` 指向（M3 验收项）。

## 五、里程碑与验收

| 里程碑 | 内容 | 验收 |
|---|---|---|
| **M0** | 边界冻结 + semver 策略 + 基线 | ✅ 判据三条、六件套冻结、基线实测 |
| **M1** | api + sdk 迁出建仓，打 tag | ✅ 全新 clone 编译通过；adapter-qq 去掉绝对路径后 252 通过 |
| **M2** | 框架层去业务化 | ✅ `MountContext` 只剩 `ctx`；零 `echo-defs` 依赖；领域词门禁实测能报错 |
| **M3** | 六件套整体外迁 | ✅ Core 全量 903 / 框架仓 165；release 构建 + 部署 + QQ 插件端到端正常 |
| **M4** | 可用性与证伪 | ✅ 非 EchoAgent 最小宿主示例可跑（含 assert）；跨语言插件可跑；第三方文档 |
| **M5** | 协议去业务化 | **决策门**：需 2 个以上真实外部使用者才启动；当前建议不动 |

## 六、风险与已知边界

1. **`Contribution` 枚举仍是业务分类**（`Tool`/`Skill`/`PluginInstance`，wire tag 保留
   `"adapter"`）。M5 才处理。这意味着 0.x 的「业务无关」在**协议表面**上是有保留的：
   机制无关，但协议承认了应用领域的分类。
2. **「通用」无法被工程手段保证**。Cordis 的通用性来自真实第三方在用，本计划最多做到
   「外部可以独立编译并在非 EchoAgent 宿主里跑通」（M4 的证伪测试）。这一点不夸大。
3. **`echo-context` 的 scope 语义未经外部检验**。它是为 agent 场景设计的（按
   `agent-a`/`agent-b` 分域），外部使用者可能觉得多了一层。M4 若发现是负担，
   应改为可选 feature 而非删除。
4. **双仓发布耦合**。框架 tag 与 Core 的 `Cargo.lock` 必须同步升级，否则会出现
   「Core 用了框架 A 版、插件用 B 版」的握手不兼容——由 `PROTOCOL_VERSION` 兜底
   （fail-closed 拒绝连接，不会静默错乱）。

## 七、执行记录

### M1：api + sdk 迁出建仓（完成，2026-10-19）

新仓 [plugin-framework](https://github.com/EchoLab-Auto/plugin-framework)（Public），
tag `v0.1.0` = `8d4c11e`。四个仓库各自钉自己的版本：

| 仓库 | 依赖写法 |
|---|---|
| Core | `echo-plugin-{api,sdk} = { git = "https://github.com/EchoLab-Auto/plugin-framework", tag = "v0.1.0" }` |
| plugin-adapter-qq | `echo-plugin-sdk = { git = …, tag = "v0.1.0" }`（原来是指向本机的绝对路径） |

**代码零改动**（`diff -r` 逐字节核对 `src/`；仅 `Cargo.toml` 的 `description`
去掉 `for EchoAgentCore` 前缀、依赖改为独立版本）。

验收实测：

| 项 | 结果 |
|---|---|
| 框架仓独立测试 | 24 通过（api 10 + sdk 14）+ 1 doctest，与迁出前**逐项相同** |
| 框架仓质量 | `cargo fmt --check` 通过；`clippy --all-targets` 0 告警 |
| 全新 clone 编译 | `/tmp` 干净目录 clone 后 `cargo test --locked` 全绿 |
| Core 全量 | **1034 通过 / 0 失败**；`1034 + 25（api 10 + sdk 14 + sdk doctest 1）= 1059`，与基线逐项对齐 |
| plugin-adapter-qq | 252 通过 / 0 失败（与切依赖前一致）；**全新 clone（git 协议）编译通过**，本机路径零残留 |
| `scripts/check-deps.sh` | `Dependency-direction lint OK (28 crates)` |
| 离线构建 | `cargo build --locked --offline` 通过（git 依赖已缓存，自更新不受断网影响） |

两处计划外修正：

1. **基线数字订正**：M0 写的 1056 是手工加总一份被截断的测试清单得来的**错值**。
   M1 用 `git worktree` 检出迁出前提交重跑，实测 **1059**（本文 §4.1 与 M3 验收已改）。
   教训：这类"总计数"必须由命令算，不能靠人眼累加。
2. **`Cargo.lock` 是必要产物**：Core 与 adapter-qq 的 `Cargo.lock` 都记录了
   `source = "git+…?tag=v0.1.0#8d4c11e7"`，rev 钉死意味着上游移动 tag 不会静默
   改变本地构建；同时 `--locked` 让自更新的可复现性不依赖网络状态。

未做（属 M2/M3）：`MountContext` 的业务 sink、`echo-plugin` 的悬空 `echo-defs`
依赖、框架仓自建门禁（零全局态 / 依赖方向）——迁出的两个 crate 目前**没有**
继承 Core 那套 `no_global_state` / 依赖方向门禁，M2 需在框架仓补齐。

### M2：框架层去业务化（完成，2026-10-19）

三件事：删死代码、断悬空依赖、**把领域枚举换成开放标签**。最后一件是执行中
发现的，不在原清单里。

#### 删掉的东西

| 对象 | 证据 |
|---|---|
| `MountContext.tools` / `.skills` + `with_tools` / `with_skills` | `with_skills` 零调用；全仓 `mount` 闭包都是 `\|_ctx\| Ok(vec![])`；`ctx.tools` / `ctx.skills` 零读取点 |
| `PluginToolSink` / `PluginToolHandle` / `PluginSkillSink` | `register_tool` 零调用点、`register_skill_dir` 零调用点 |
| `ToolSink` / `RegisteredTool` / `SkillSink`（宿主侧适配） | `SkillSink::new` 从未被调用；`ToolSink` 只被 `with_tools` 用 |
| `echo-plugin` 的 `echo-defs` 依赖 | `grep echo_defs::` 在 `echo-plugin/src` 零命中——依赖只活在注释里 |

`MountContext` 现在只剩 `ctx: Option<Arc<Ctx>>`——**服务定位器是框架唯一认可
的注册途径**，认什么服务由宿主决定。

#### 真正的耦合：`PluginKind` 枚举

原清单没列它，是写领域词门禁时撞出来的。`echo-plugin` 里曾有一个 8 变体枚举
（skill / tool / provider / loop / instance / orchestration / management /
interaction）——**宿主应用的分类词汇，被写进了框架契约**。

关键审计结论：**框架从不按它分支**。56 处引用里，框架侧只有
`registry.rs` 的 `kind: m.kind.as_str().into()`（转成字符串塞进前端快照），
其余 `match` 全是 `io::ErrorKind` / `EntryKind`（运输形态）这类同名不同物的
东西。它纯粹是透传的元数据。

因此改成 `kind: String`（`builtin()` 收 `impl Into<String>`）：

- **wire 格式零变化**——单元枚举本来就序列化成 `"tool"` 这类纯字符串
- Panel 零改动（它读 `kind: string`，只对 `'loop'` / `'management'` 分支）
- 新增回归测试 `open_kind_vocabulary_roundtrips`：`kind = "widget"` 必须能
  解析、往返（改动前会当场解析失败）
- 词汇表归宿主：实例插件的 kind 变成 `echo_agent::plugins::INSTANCE_PLUGIN_KIND`
  常量，仍保留"不要硬编码字符串"的原始意图（`2d46b7d` 那次漏改的教训）

**代价（须知）**：manifest 里的未知 `kind` 不再报解析错误。这是框架化的必然
——框架不拥有词汇表就无法校验它。宿主若需要严格校验，应在自己那侧做。

顺带把框架生产代码里最后 8 处领域词（都在文档注释中）改写成中性表述：
`Agent::emit` → "hot paths"、`LlmProvider` 示例 → `Encoder`、
`echo-agent.tools.builtin` 示例 → `acme.things.widget`、`[agent].disabled_plugins`
→ "host-defined storage"。

#### 新增门禁

**宿主侧** `source/core/tests/framework_domain_agnostic.rs`（4 项）：
1. 框架 crate 生产代码（`#[cfg(test)]` 之前，跳过 `src/bin/`）零领域词
2. 框架 crate 不依赖 `echo-defs` / `echo-agent` / `echo-protocol` / `echo-session`
3. 整词匹配自检（`tools`/`EchoAgentCore` 不误报、`Session` 命中）
4. `#[cfg(test)]` 之后不算

**框架仓** `crates/framework-gates`（4 项）：不反向依赖宿主、库内零全局态，
各配一条自检。

**两条门禁都实测过能真报错**（否则等于没加）：
- 注入 `/// a live Session` 到 `echo-plugin/src/plugin.rs` → FAILED，指出行号
- 注入 `static PROBE: u8 = 0;` 到框架仓 `api/src/lib.rs` → FAILED
- 注入 `echo-defs = { path = … }` 到框架仓 `sdk/Cargo.toml` → FAILED，打印
  `echo-plugin-sdk → echo-defs`

写法上的一处取舍：门禁按"第一个 `#[cfg(test)]` 截断"取生产代码。这依赖
"测试模块在文件末尾"这个本仓一贯风格；若有人把代码写在测试模块之后，门禁
看不到。已在该测试文件里写明。

#### 验收

| 项 | 结果 |
|---|---|
| Core 全量 | **1038 通过 / 0 失败**（M1 基线 1034 + 4 项新门禁） |
| 框架仓 | **29 通过 / 0 失败**（api 10 + sdk 14 + gates 4 + doctest 1） |
| 领域词扫描 | 四个框架 crate 生产代码**零命中** |
| Panel | 零改动（`kind` 取值集合未变） |

### M3：六件套整体外迁（完成，2026-10-19）

框架仓 `v0.2.0`（`9116b44`）收下 `echo-context` / `echo-plugin` /
`echo-plugin-loader` / `echo-plugin-host`，Core 侧只剩应用层。

| 项 | 值 |
|---|---|
| Core workspace members | 30 → 24 |
| 迁出代码 | 6097 行（context 1540 + plugin 1184 + loader 810 + host 2563）|
| 迁出测试 | 135 项 |
| 框架仓总量 | 8533 行 / **165 项测试** |

**代码零改动**（逐字节搬迁）。但发现**五处依赖声明并不都走 `workspace = true`**——
`echo-sanitize` / `echo-loop` / `echo-protocol` / `echo-agent` / `core` 里还写着
`{ path = "../../context/echo-context" }` 这类直接相对路径。删目录后 cargo
立刻报 `failed to read .../echo-context/Cargo.toml`。全部改为
`{ workspace = true }` 后由 workspace 统一指向 git tag——**这就是"改名不动源码"
的前提**：依赖来源必须只有一处真值。

#### 两道门禁随迁移搬家

迁移前它们扫 `source/` 目录树，迁出后**自动不再覆盖框架，且不会报错**——
保护静默消失。M3 的实际处理：

| 门禁 | 处理 |
|---|---|
| 零全局态 | Core 侧白名单清空（扫描树里已无 kernel cell）；框架仓**精确豁免一个文件** `crates/echo-context/src/kernel.rs`，并用 `whitelist_stays_minimal` 把白名单锁死为 1 条 |
| 领域无关性 | `framework_domain_agnostic.rs` 从 Core 删除，改写为框架仓的 `domain_agnostic.rs`（覆盖 context/plugin/loader/host，另加一条"任意框架 crate 不得依赖领域/内核 crate"）|
| 依赖方向 | Core 的 `policy_shape_matches_the_layering_contract` 里 `context/echo-context` 那条底座断言随之删除（否则断言一个已不在本仓的 crate）|

#### 一个真问题：框架仓导出了进程级单例表

`echo-context::kernel` 是 `static KERNEL: OnceLock<Cell>`——一张
`TypeId → Box<dyn Any>` 的进程级表。M3 前它是 Core `no_global_state` 门禁的
唯一豁免（P1 阶段把散落各处的 `OnceLock` 静态收敛到这里）。

审计它的使用者：**全部 8 处都在应用层**（`agent_manager`、`plugin_host`、
`global_policy`、`shell_manager`、搜索客户端、联邦导入），框架自己一处都不用。

这意味着：迁完之后，**框架仓导出的 API 里带着一张进程级全局表**。对"同一进程
能起两套宿主"这个复用前提，它是直接冲突。

处理方式与理由：M3 的纪律是"只搬迁不重新设计"，改它需要新建一个宿主 crate
并改 8 个调用点，风险与本轮目标（大迁移）叠加不划算。因此：

- 门禁里**精确到文件**豁免，白名单数量锁死为 1，新增任何其它全局态仍红；
- 在框架仓 README / CHANGELOG 与本文档**同时写明**它该搬走；
- 列入后续任务（见 §八）。

这是本轮"六件套齐备"里唯一一处"结构上到位、归属上不到位"的地方，不掩饰。

#### 验收

| 项 | 结果 |
|---|---|
| Core 全量 | **903 通过 / 0 失败** |
| 框架仓全量 | **165 通过 / 0 失败** |
| 加总核对 | 1059（M0 基线）− 160（迁出并扣除 doctest）+ 4（M2 门禁）+ 5（M3 新增门禁自检）= 1068 = 903 + 165 ✓ |
| `cargo fmt` / `clippy` | 两仓均干净 / 0 告警 |
| release 构建 | 通过（Core + 进程外插件二进制） |
| 部署与端到端 | 见 §八 |

### M3.5：把进程级单例表搬回宿主侧（完成，2026-10-19）

框架仓 `v0.3.0`（`b162c95`）**移除** `echo_context::kernel`；Core 新建
`source/context/echo-bootstrap` 承载它（182 行，零依赖叶子）。

为什么不留在框架里——两条独立的理由：

1. **框架自己一处不用**：六个 crate 零调用，14 处调用点全在应用层
   （`echo-agent` 6 / `echo-shell` 3 / `echo-plugin-tools` 1 / `core` 1，
   其余为文档）。
2. **它破坏复用前提**：「同一进程能起两套宿主」要求框架不在进程级藏状态；
   留着这张表，第二套宿主会读到第一套的句柄。

搬迁是**纯改名**（`echo_context::kernel::set` → `echo_bootstrap::set`），
行为不变。这轮把两边的门禁都收紧了：

| 侧 | 处置 |
|---|---|
| 框架仓 | `no_global_state` 白名单 **清空为 `[]`**，自检改名 `whitelist_stays_empty` —— 「零全局态」从此无条件成立 |
| Core | `no_global_state` 白名单恢复为**精确一个文件** `context/echo-bootstrap/src/lib.rs`（M3 时曾清空） |

**终态不是这个**。`echo-bootstrap` 只是把全局态从框架搬到宿主，没有消灭它。
真正干净的形态是让宿主把句柄经 `echo_context::Ctx` 显式注入——`Ctx::provide`
已具备能力，且天然支持多份互不干扰。之所以没做：14 处调用点分布在多层构造
函数里，逐层下传是独立一轮改造，与「框架外迁」正交。`echo-bootstrap` 的
crate 文档把这一点写成了第一段而非脚注。

验收：Core **908 通过**（903 + bootstrap 5）、框架仓 **160 通过**
（原 165 − kernel 的 5 项）；`check-deps.sh` OK（25 crates）；框架仓门禁实测
仍能报错（清零后往 `ctx.rs` 注入 `static` → FAILED 并指出行号）。

### M4：第三方可用性（完成，2026-10-19）

框架仓 `v0.4.0` / `v0.4.1`。这一轮的目标是**证伪**：如果框架真有业务残留，
「在非本应用的宿主里跑通」就做不到。

| 产物 | 内容 |
|---|---|
| `examples/minimal-host` | 130 行：`Ctx` + 服务键、进程内插件挂载与**撤销验证**、supervisor 拉起子进程插件、invoke、drain |
| `examples/minimal-plugin` | 70 行：只依赖 `echo-plugin-sdk` |
| `examples/minimal-plugin-py` | 150 行纯 Python：手写帧、零 Rust 依赖 |
| `docs/getting-started.md` | 第三方快速上手（写插件 / 拉起来 / 同进程轨 / 崩溃隔离 / 跨语言 / 版本策略） |
| `tests/cross_language.rs` | 3 项：真 spawn Python、版本不兼容 fail-closed、直接对帧断言 |
| CI | 示例端到端 + 跨语言各一个独立步骤 |

两个示例都**不引入** `echo-defs` / `echo-agent` / `echo-protocol`。宿主示例内部带
`assert!`，跑通即证据，CI 里真跑。

#### 这轮撞出的真 bug（从 P0 就存在）

`Contribution::Service` **根本过不了 stdio 轨**：

```json
{"kind":"service","key":"greeter","kind":"greeter","version":1}
                                   ^^^^ 与枚举 tag 重复
```

`Contribution` 是内部标签枚举、tag 叫 `kind`，而 `ServiceContribution` 的载荷
字段也叫 `kind` → 序列化出重复键 → 宿主 `serde_json` 报 `duplicate field`，
整帧解析失败。

**为什么长期没被发现**：inproc 轨类型直连、零序列化，冲突不出现；而当时唯一声明
`Service` 贡献的插件（`echo-plugin-loop`）跑在 inproc 上。这两个示例是第一个
同时用「Service 贡献 + stdio 轨」的组合。

修法：wire 字段改名 `service_kind`。**零兼容成本**——此前这种帧根本解析不了，
不可能有在用的一端。

三道防线（都实测能报错）：
1. `every_contribution_variant_roundtrips_through_json`：所有变体过一遍 JSON
2. `service_contribution_wire_field_avoids_tag_collision`：断言 JSON 里
   `"kind"` 只出现一次
3. `cross_language.rs`：Python 插件真发这个帧；把 `service_kind` 改回 `kind`，
   两条测试立刻 FAILED

#### 验收

| 项 | 结果 |
|---|---|
| 框架仓 | **165 通过 / 0 失败** |
| Core | **908 通过 / 0 失败**（框架依赖升 v0.4.1，零 API 变化） |
| 全新 clone（`/tmp/fw-m5`） | **165 通过** + 示例打印 ✅ |
| plugin-adapter-qq | 升到 v0.4.1，**257 通过 / 0 失败**（与 Core 同 tag，避免多版本并存） |
| 部署 | 二进制 md5 `1a477c24…`，运行中进程与安装文件一致 |
| 端到端 | 插件 10 / QQ connected（self_id 2911759808）/ 工具 29 / 时间线 ord 严格递增 |

## 八、后续任务

1. M5：协议去业务化（有决策门：需 2 个以上真实外部使用者；当前建议不动）。
2. （可选，独立一轮）把 `echo-bootstrap` 的句柄改成 `Ctx` 注入，然后清空
   Core 侧那条白名单。做完之后整个仓库**零进程级全局态**。
3. （可选，低优先）插件模板仓——`examples/minimal-plugin` 已可直接当模板用，
   单独建仓目前只会多一份需要同步维护的副本。等有外部作者反馈起手麻烦时再做。
