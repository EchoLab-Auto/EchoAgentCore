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

`cargo test --workspace` 全绿，**1056 通过 / 0 失败**（含 1 项 doctest）。
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
| **M0**（本文） | 边界冻结 + semver 策略 + 基线 | 本文提交；判据明确 |
| **M1** | api + sdk 迁出建仓，打 tag | `git clone` 框架仓 → `cargo test` 通过；`plugin-adapter-qq` 去掉绝对路径后编译通过 |
| **M2** | 框架层去业务化 | `MountContext` 只剩 `ctx`；框架 crate 零 `echo-defs` 依赖；新增领域词门禁 |
| **M3** | 六件套整体外迁 | Core 全量测试 1056 仍全绿；release 构建 + 部署 + QQ 插件端到端正常 |
| **M4** | 可用性与证伪 | **非 EchoAgent 最小宿主示例**（50 行 + 2 插件，只用六件套）可跑；跨语言插件示例可跑；第三方文档 |
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
