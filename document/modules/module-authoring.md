---
id: module-authoring
title: "模块开发指南"
group: Core
x: 4308
y: 216
---

# 模块开发指南（外部进程模块）

> **定位**：本指南面向**外部模块作者**——如何写一个独立进程模块、如何用 `modules.toml` 装载、以及协议与生命周期约定。读者：模块作者与 Core 宿主侧维护者。内置模块体系（清单、能力开关）见 [模块化设计](./modules.md)。

> **状态**：协议冻结、宿主（inproc/stdio）、模块 SDK、试点模块、`modules.toml` 装载**全部可用**，工具贡献已端到端跑通；**安全限额已落地**（`[module.limits]` 子进程 rlimit + 入站频率限额 + 可逆停止）；热替换仍在推进。技能/服务贡献为后续方向。

> **框架在哪**：`echo-module` / `echo-module-api` / `echo-module-host` / `echo-module-loader` /
> `echo-module-sdk` / `echo-context` 六件套住在**独立仓库**
> [echo-fabric-arch](https://github.com/EchoLab-Auto/echo-fabric-arch)，**不在本仓**。
> Core 通过 workspace 的 git 依赖按 tag 钉版本（`Cargo.toml` 的 `echo-module-* = { git = …, tag = "v0.14.0" }`），
> 所以在本仓 `cargo grep echo_module_sdk` 找不到源码——要读实现或改框架去那个仓。
> 本仓内 `source/modules/*` 是**用框架写的业务模块**，不是框架本身。

## 五分钟上手

新建一个 crate（依赖 `echo-module-sdk`），实现 `ModuleHandler` 并交给 `serve_stdio`：

```rust
use echo_module_sdk::{
    async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, ModuleHandler,
};
use echo_module_sdk::serde_json::json;

struct MyModule;

#[async_trait]
impl ModuleHandler for MyModule {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Custom {
            kind: "echo-agent.tool".into(),
            name: "hello".into(),
            payload: json!({
                "name": "hello",
                "description": "示例工具：回显问候",
                "parameters": {
                    "type": "object",
                    "properties": { "name": {"type": "string"} },
                    "required": ["name"]
                },
                "category": "module",
                "package": "my-module",
                "visible_reply": false
            }),
        }]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        match req.contribution.as_str() {
            "hello" => InvokeOutcome::Ok {
                text: format!("你好，{}", req.payload["name"].as_str().unwrap_or("世界")),
                images: vec![],
                // 结构化返回（工具面消费者可忽略）；`text` 是模型可见文本
                result: serde_json::Value::Null,
            },
            other => InvokeOutcome::Error {
                code: "unknown_contribution".into(),
                message: format!("未知工具: {other}"),
            },
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = serve_stdio(MyModule).await {
        eprintln!("module exited: {e}");
        std::process::exit(1);
    }
}
```

要点：

- **帧只走 stdout**；模块自身的日志请走 stderr（宿主会逐行转发到自己的日志，带模块前缀）。
- 模块 id 用环境变量 `ECHO_MODULE_ID` 覆盖（缺省 `stdio-module`）；与 `modules.toml` 的条目 id 一致最省事。
- `contributions()` 在握手时**全量上报**（`Register` 为整体替换语义）。

## 装载：`modules.toml`

Core 启动时读取 **core.toml 同目录**的 `modules.toml`；**文件不存在 = 特性休眠**（零行为变化）。
格式（三层组合，dsh 式"按 id 定位、整行替换"）：

```toml
# modules.toml —— 入口（profile）
[profile]
name = "default"
bundles = ["bundles/base"]   # 相对本文件目录，按序叠加
patches = ["local.patch"]    # 之后按序叠加

# 行内条目（最后应用；等价于末端 patch 的 insert）
[[module]]
id = "example"                        # 唯一 id（= 模块实例 id，握手比对）
kind = "stdio"                        # builtin | stdio | dylib | wasm
                                      # 当前可用：stdio 子进程 + builtin 内置行；dylib / wasm 未实现
name = "echo-module-demo"            # 说明性名称（stdio 下 command 才是真实路径）
command = "/home/me/.local/libexec/echo-agent-core/modules/echo-module-demo/echo-module-demo"
args = ["--stdio"]                    # 可选
env = { MY_MODE = "prod" }            # 可选（宿主环境默认不继承，仅透传 PATH）
enabled = true                        # 可选，缺省 true
requires = []                         # 可选：依赖的其它条目 id（拓扑排序）
[module.config]                       # 可选：原样进 Hello.config
mode = "fast"
```

组合语义：先按序应用每个 bundle，再按序应用每个 patch，最后应用行内条目；
已存在 id 的行会被**整体替换**（不做字段级合并）——patch 里只需重述要保留的字段。

校验：`kind=stdio` 必须有 `command`；`requires` 必须可解析；环依赖报可读错误。

**安装目录约定**（install.sh 建目录并安装，update.sh 同步刷新 + 回滚）：

```text
~/.local/libexec/echo-agent-core/modules/<包名>/<二进制>
```

- 目录名 = **Cargo 包名**（不是运行期模块 id——id 写在二进制里的 manifest 中，安装器静态读不到）；二进制名与包名一致。
- 随框架一同安装的模块集合由 `scripts/install.sh` 的 `MODULE_BINARIES` 决定（当前为空：仓库里只有 `echo-module-adapter` 这类**不装机**的进程内模块 crate，外部模块按需自建），`update.sh` 用同一张表同步升级——**两者必须一致**，由 `source/modules/core/tests/install_script_flag.rs` 守护（同时比对「仓库里 `[[bin]] path = "src/main.rs"` 的模块 crate」）。
- 自更新的模块替换是**版本化**的：旧二进制留成 `<二进制>.rollback`，任何后续步骤失败（含健康检查）都会把全部模块还原回上一版，不留"Core 新 / 模块旧"的半新组合。
- 自己的模块可以放在同一目录下（`modules/<你>/<二进制>`），`modules.toml` 的 `command` 指过去即可；自更新不会碰清单外的目录。

改完 `modules.toml` **重启 Core 生效**（热替换尚未落地，暂不免重启）。

### 同一个文件也管内置模块

内置模块的 manifest 还带一个宿主词汇 `kind`（`tool` / `skill` / `provider` /
`loop` / `federation` / `management` / `orchestration` / `instance` /
`interaction` / `session`）。它是**开放字符串**（框架不校验取值），宿主只用它
决定**这模块挂在一层还是一份**：

- `kind ∈ {tool, skill, provider}` → 每人格一份**独立实例**，挂在该人格的
  子作用域里（`Ctx` 是宿主服务表的 `child`，适配器绑该人格自己的工具表 /
  技能表 / 模型配置）。新增"按人格才说得通"的模块请归入这三类。
- 其余 → 进程级单例，挂在根作用域（管理面的监听端口、联邦链路表、updater
  单元都属于这一档；挂 N 份会真的开 N 个监听）。

`modules.toml` 不只登记子进程模块：写 `kind = "builtin"` 的行可以**重排 / 关闭**编译进 Core 的内置模块（工具集、技能目录、工作区、subagent、循环模式、管理面、provider、实例门控壳）。行序即挂载顺序；`enabled = false` = 注册但不挂载（Panel 清单仍可见、可就地启用）。

**没写进文件的内置模块一律照旧挂载**——这条缺省是为了让"只加一个外部模块"的现有部署行为零变化。内置模块 id 清单见 [模块化设计](./modules.md)「内置模块清单」。

### 资源限额（可选）

子进程模块可以加 `[module.limits]`——**缺省不限制**，因为限额是部署策略，没有普适默认值（Node / NapCat 那类模块需要很大的地址空间，随手设 256 MiB 会直接起不来）：

```toml
[[module]]
id = "example"
kind = "stdio"
name = "echo-module-demo"
command = "/opt/echo/modules/echo-module-demo"

[module.limits]
cpu_seconds = 3600     # CPU 时间（秒）；用尽 → SIGXCPU
memory_mb = 2048       # 地址空间（MiB）；用尽 → malloc/mmap 失败
file_size_mb = 256     # 单文件写入（MiB）；超限 → 写返回 EFBIG
open_files = 1024      # fd 数（含 socket）；用尽 → EMFILE
processes = 64         # 子进程数；超限 → fork 失败（按 uid 计，见下）
```

语义要点：

- **硬限额**（软=硬），不做"先警告再收紧"——模块没有义务处理信号，两段式只会让人误以为限额没生效；
- 在 `exec` **之前**由子进程自己 `setrlimit`，限额**继承给它的后代**；
- `processes` 用的是 `RLIMIT_NPROC`，它按 **uid** 统计而不是进程树：同 uid 的多个模块共享这个配额，所以值要给得够宽（或者干脆不给）；
- **`0` 会被加载器拒绝**：`setrlimit` 里 0 表示"完全禁止"而不是"不限制"，写 0 的人几乎一定是想要后者——报错里会提示删掉该行；
- 空 `[module.limits]` 表等价于不限制（归一成 `None`）；
- `dump-config` 会把生效的限额原样渲染出来。

### 写完怎么确认它在跑

外部条目（`kind = "stdio"`）会出现在面板「设置 → 模块 → 外部模块」组里，带运行形态 `独立进程`；没起来的话行卡有红色「失败」徽标，详情里给启动失败原因（命令不存在 / 握手失败）。**改 `command` 或换二进制必须重启 Core**（配置在启动期读取）；但**启停与重载进程**可以在面板直接做——「禁用」会排空并退出该模块的子进程，「启用」会重新拉起并握手，「重启」= stop + start，都立即生效、不需要重启 Core。

`echo-agent-core dump-config` 能看到分层合成后的最终样子（含每行来自哪一层）；两者同源，排查顺序建议：`dump-config` 看配置 → 面板看状态 → 日志看 `module start failed`。

### 先看一眼合成结果：`dump-config`

多层叠加后"最终这一行到底长什么样、来自哪一层"靠人推演容易出错，用子命令直接看：

```bash
echo-agent-core dump-config -c ~/.config/echo-agent-core/core.toml
```

输出是**合成后**的 TOML，每个条目上方带来源注释（`layer` 从 0 起：bundle 先、
patch 其次、本文件行内条目最后）：

```text
# --- example (layer 1: local.patch.toml) ---
[[module]]
command = "/opt/echo/modules/echo-module-demo"   # patch 整行替换后的值
enabled = true
id = "example"
kind = "stdio"
name = "echo-module-demo"
```

要点：

- **只合成不启动**——不拉起模块、不监听端口，可随时安全执行；
- 用的是**与启动完全同一个** `load()`，所以这里合成出来的树就是运行期装载的树，
  校验失败 / 环 / 未知 `requires` 在这里报**同一个**错（`dump-config` 非零退出）；
- `modules.toml` 不存在 → 打印一行说明并**退出码 0**（特性休眠不是错误）；
- stdout 只有 TOML 正文（`>` 落盘、`diff` 两份合成结果都干净）。
- `--config` 可写在子命令**前或后**（`dump-config -c x` 与 `-c x dump-config` 等价）。

## 框架贡献与 Core 自定义分类

`echo-fabric-arch` v0.14 的 `Contribution` 只保留领域中立的
`Service` / `Events` / `Command` / `Text` / `Custom` 五种形状。Core 的工具与聊天实例
都是业务词汇，使用 `Custom` 承载；DTO 与分类名定义在宿主适配层
`echo-module-adapter::contract`，而不是框架仓。

| 分类 / 变体 | Core 消费方式 |
| --- | --- |
| `Custom { kind: "echo-agent.tool", name, payload }` | 解码工具声明，适配为 `RemoteTool`；按 `name` 寻址 |
| `Custom { kind: "echo-agent.instance", name, payload }` | 解码实例声明，注册 `RemoteInstance`；操作按 `name:<op>` 寻址 |
| `Contribution::Command` | 仅进程内：按命令名分发给声明它的模块 |
| `Contribution::Text` | 仅进程内：按 order 收集文本块 |
| `Contribution::Service` / `Events` | 框架提供通用形状；Core 的外部进程消费方按业务需要另行接线 |

`Custom.payload` 的 schema 由 Core 与模块共同约定，框架不解释。工具声明至少包含
`name` / `description` / `parameters`，实例声明包含 `name` / `platform` /
`display_name` / `capabilities`。实例的 `Custom.name` 必须与 `payload.name` 一致，
否则宿主拒绝认领，避免把调用路由到另一个实例。

## 协议约定（宿主 ↔ 模块）

同一组消息覆盖全部运输层；模块作者只需理解这些语义（类型定义见 `echo-module-api`）：

| 阶段 | 宿主 → 模块 | 模块 → 宿主 | 说明 |
|---|---|---|---|
| 握手 | `Hello{protocol, version, module_id, config}` | `Welcome{...}` + `Register{contributions}` + `Ready` | 5s 超时；协议名/主版本/module_id 校验，不匹配拒绝 |
| 调用 | `Invoke{call_id, contribution, ctx, payload}` | `InvokeResult{call_id, outcome}` | 并发处理；`ctx` 带 `session_id`/`team_id`/`deadline_ms` |
| 取消 | `Cancel{call_id}` | （结果仍以 `InvokeResult` 为准） | 尽力送达；建议快回 `Error{code:"cancelled"}` |
| 排空 | `Drain{deadline_ms}` | （进程退出） | 停止收新调用，等在途完成，超时被强杀 |
| 终止 | `Dispose` | （进程退出） | 立即退出 |
| 主动上报 | — | `Emit{event, payload}` / `Log{level, message}` | 事件订阅（后续——宿主侧缓冲封顶 1024，超限丢新并计数告警）/ 日志 |
| 服务回呼 | `HostCallResult{call_id, outcome}` | `HostCall{call_id, service, method, payload}` | 模块回呼宿主注册服务（如 `"sanitizer"`）；未知服务/超时以 `Error` 收尾 |

帧编码（stdio）：**4 字节小端 u32 长度前缀 + JSON 帧体**；单帧上限 16 MiB。
崩溃语义：模块进程退出（panic/segfault/exit）→ 宿主视为崩溃，按 backoff（200ms×2、上限 2s、最多 5 次）自动重启；
在途调用以 `Error{code:"module_crashed"}` 收尾。

## 实例模块

实例模块不是「模块的一个变体」，而是**用同一套协议承载平台接入**：注册一个
`Custom { kind: "echo-agent.instance", .. }`，此后生命周期、收发、目录查询、管理动作、以及对模型
暴露的工具，全部经 `Invoke("<实例名>:<op>")` 路由到你实现的 `InstanceModule::invoke_op`。

SDK 侧实现 `InstanceModule`（`echo-module-sdk`）：`configure`（收配置，`Err` 即
拒绝启动）→ `instance()`（返回贡献）→ `invoke_op(op, payload)`（路由）→
`on_ready(host)`（拿 `HostClient` 回呼）→ `on_drain`。

### 贡献（`InstanceContribution`）

| 字段 | 作用 |
| --- | --- |
| `name` / `platform` / `display_name` | 实例名（= Invoke 前缀，全局唯一）/ 平台标识（会话作用域）/ 展示名 |
| `capabilities` | 能力位（`send_text` / `send_media` / `groups` / `friends` / `gating` / `login`）——**fail-closed**：未声明的调用宿主直接拒绝，不发起 Invoke |
| `config_schema` | 配置 JSON Schema（校验 / 面板表单） |
| `ui` | 管理界面描述符（Panel 通用渲染器按 `ui.version` 呈现，宿主零平台逻辑） |
| `session_labels` | 会话分组标签（如 `{"dm":"私聊","group":"群"}`） |
| **`tools`** | **对模型暴露的工具声明**（见下） |

### 工具声明（`tools`）

每个 `ToolContribution` 声明一个工具：`name`（同时是 `invoke_op` 的 op 名）、
`description`、`parameters`（JSON Schema）、`category`、`timeout_hint_secs`、
`package`、**`visible_reply`**。

宿主侧（`core/instance_tools.rs`）对声明做三件事，**不认识任何具体工具名**：

1. 跨实例合并（同名只注册一次）；
2. 多实例时给 schema 注入可选 `account`，执行时按它选实例——
   `account` 是宿主侧寻址参数，**不会出现在你的 payload 里**；
   多实例未指定时宿主直接报错列出可选项，不会替你猜；
3. 统一过 `OutboundGate`（文本命中脱敏器按策略阻断/替换；文件参数落在配置/
   凭据目录直接拒发）——**所以工具名不必也不该以 `send` 开头**。

执行时宿主回传 `invoke_custom(<工具名>, <原始参数>)`；返回值带
`success: false` 会被转成模型可见的工具错误（`error` 字段给原因），否则原样
交回模型。

**`visible_reply`**：成功调用是否即产生**平台可见输出**（真正把内容送到用户
眼前）。宿主的「临时等待回复」逻辑据此决定抑制与否——不声明就不抑制。

示例（QQ 模块的做法）：`send_group_msg` / `send_private_msg` / `send_file` /
`send_image` / `send_voice` / `send_json_card` 声明 `visible_reply: true`；
`get_group_list` / `get_friend_list` / `get_group_member_info` /
`get_group_msg_history` 为 `false`。完整 schema 见 `agent-module-adapter-qq` 仓的
`src/tools.rs`（与迁出前的 Core 内置版逐字一致）。

### 能力位 ≠ 工具声明

`capabilities` 是**宿主栅栏**（未声明 → 对应 Invoke 一律拒绝），`tools` 是
**模型可见面**。两者独立：可以声明工具而关闭某个能力位（该工具的调用会被
宿主拦下），但通常应保持一致。

## 实践与验证

- **最小报错即修正**：`InvokeOutcome::Error{code, message}` 的 `message` 是模型可见文本——写清"发生了什么、应该怎么做"（与内置工具的错误口径一致）。
- **超时自声明**：`ToolContribution.timeout_hint_secs` 与内置工具 `timeout_hint` 同义——宿主守卫取 `max(配置 base, hint + 宽限)`。
- **多模态**：`InvokeOutcome::Ok.images`（图片 URL / data URI）与内置工具 `execute_rich` 同口径。
- **零全局态纪律**：模块进程内不要持有进程级可变单例（`static OnceLock` 等）——用实例字段（见试点模块的 `WebSearch` 持 client 的做法）。
- **宿主服务回呼（HostCall）**：模块可回呼宿主注册的命名服务（当前默认提供 `"sanitizer"`：`redact` / `scan` / `register_secret`——处理敏感文本前先脱敏、登记模块自己的凭证）。SDK 侧在 `ModuleHandler::on_ready` 收到 `HostClient`（`call_with_timeout` 可调超时，默认 30s）；服务不可用时返回 `Service{code, message}` 错误而非挂死。
- **参考实现**：协议全消息测试对端 `echo-module-test-peer`（在**框架仓** `echo-fabric-arch` 的 `crates/echo-module-host/src/bin/echo-module-test-peer.rs`，不在本仓）；真实外部模块可参考 `echo-hpc-platform`。

### 本地验证

```bash
# 1) 构建模块
cargo build --release -p echo-module-demo

# 2) 安装到约定目录
install -Dm755 target/release/echo-module-demo \
  ~/.local/libexec/echo-agent-core/modules/echo-module-demo/echo-module-demo

# 3) 写 modules.toml（core.toml 同目录，格式见「装载」节）
# 4) 重启 Core：systemctl --user restart echo-agent-core.service
# 5) 在 Panel 里检查工具列表（分类"模块"）并调用
```

调试：模块 stderr 会进 Core 的 journal（`journalctl --user -u echo-agent-core.service`），带 `module = <id>` 字段。
