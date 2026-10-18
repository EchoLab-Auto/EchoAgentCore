---
id: plugin-authoring
title: "插件开发指南"
group: 插件
x: 960
y: 2620
---

# 插件开发指南（外部进程插件）

> **定位**：本指南面向**外部插件作者**——如何写一个独立进程插件、如何用 `plugins.toml` 装载、以及协议与生命周期约定。读者：插件作者与 Core 宿主侧维护者。系统设计与路线图见 [完全解耦推进计划](../design/decoupling-plan.md)；内置插件体系（清单、能力开关）见 [插件化设计](./core-plugins.md)。

> **状态**：协议冻结、宿主（inproc/stdio）、插件 SDK、试点插件、`plugins.toml` 装载**全部可用**，工具贡献已端到端跑通；热替换与安全限额仍在推进。技能/服务贡献为后续方向。

## 五分钟上手

新建一个 crate（依赖 `echo-plugin-sdk`），实现 `PluginHandler` 并交给 `serve_stdio`：

```rust
use echo_plugin_sdk::{
    async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, PluginHandler,
    ToolContribution,
};
use echo_plugin_sdk::serde_json::json;

struct MyPlugin;

#[async_trait]
impl PluginHandler for MyPlugin {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Tool(ToolContribution {
            name: "hello".into(),
            description: "示例工具：回显问候".into(),
            parameters: json!({
                "type": "object",
                "properties": { "name": {"type": "string"} },
                "required": ["name"]
            }),
            category: "plugin".into(),
            timeout_hint_secs: None,
            package: Some("my-plugin".into()),
            // 成功调用是否即产生平台可见输出（普通插件工具填 false）
            visible_reply: false,
        })]
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
    if let Err(e) = serve_stdio(MyPlugin).await {
        eprintln!("plugin exited: {e}");
        std::process::exit(1);
    }
}
```

要点：

- **帧只走 stdout**；插件自身的日志请走 stderr（宿主会逐行转发到自己的日志，带插件前缀）。
- 插件 id 用环境变量 `ECHO_PLUGIN_ID` 覆盖（缺省 `stdio-plugin`）；与 `plugins.toml` 的条目 id 一致最省事。
- `contributions()` 在握手时**全量上报**（`Register` 为整体替换语义）。

## 装载：`plugins.toml`

Core 启动时读取 **core.toml 同目录**的 `plugins.toml`；**文件不存在 = 特性休眠**（零行为变化）。
格式（三层组合，dsh 式"按 id 定位、整行替换"）：

```toml
# plugins.toml —— 入口（profile）
[profile]
name = "default"
bundles = ["bundles/base"]   # 相对本文件目录，按序叠加
patches = ["local.patch"]    # 之后按序叠加

# 行内条目（最后应用；等价于末端 patch 的 insert）
[[plugin]]
id = "example"                        # 唯一 id（= 插件实例 id，握手比对）
kind = "stdio"                        # builtin | stdio | dylib | wasm（当前仅 stdio 可装载）
name = "echo-plugin-example"          # 说明性名称（stdio 下 command 才是真实路径）
command = "/home/me/.local/libexec/echo-agent-core/plugins/echo-plugin-example/echo-plugin-example"
args = ["--stdio"]                    # 可选
env = { MY_MODE = "prod" }            # 可选（宿主环境默认不继承，仅透传 PATH）
enabled = true                        # 可选，缺省 true
requires = []                         # 可选：依赖的其它条目 id（拓扑排序）
[plugin.config]                       # 可选：原样进 Hello.config
mode = "fast"
```

组合语义：先按序应用每个 bundle，再按序应用每个 patch，最后应用行内条目；
已存在 id 的行会被**整体替换**（不做字段级合并）——patch 里只需重述要保留的字段。

校验：`kind=stdio` 必须有 `command`；`requires` 必须可解析；环依赖报可读错误。

**安装目录约定**（自更新将按此同步，进行中）：

```text
~/.local/libexec/echo-agent-core/plugins/<id>/<二进制>
```

改完 `plugins.toml` **重启 Core 生效**（热替换尚未落地，暂不免重启）。

## 协议约定（宿主 ↔ 插件）

同一组消息覆盖全部运输层；插件作者只需理解这些语义（类型定义见 `echo-plugin-api`）：

| 阶段 | 宿主 → 插件 | 插件 → 宿主 | 说明 |
|---|---|---|---|
| 握手 | `Hello{protocol, version, plugin_id, config}` | `Welcome{...}` + `Register{contributions}` + `Ready` | 5s 超时；协议名/主版本/plugin_id 校验，不匹配拒绝 |
| 调用 | `Invoke{call_id, contribution, ctx, payload}` | `InvokeResult{call_id, outcome}` | 并发处理；`ctx` 带 `session_id`/`team_id`/`deadline_ms` |
| 取消 | `Cancel{call_id}` | （结果仍以 `InvokeResult` 为准） | 尽力送达；建议快回 `Error{code:"cancelled"}` |
| 排空 | `Drain{deadline_ms}` | （进程退出） | 停止收新调用，等在途完成，超时被强杀 |
| 终止 | `Dispose` | （进程退出） | 立即退出 |
| 主动上报 | — | `Emit{event, payload}` / `Log{level, message}` | 事件订阅（后续——宿主侧缓冲封顶 1024，超限丢新并计数告警）/ 日志 |
| 服务回呼 | `HostCallResult{call_id, outcome}` | `HostCall{call_id, service, method, payload}` | 插件回呼宿主注册服务（如 `"sanitizer"`）；未知服务/超时以 `Error` 收尾 |

帧编码（stdio）：**4 字节小端 u32 长度前缀 + JSON 帧体**；单帧上限 16 MiB。
崩溃语义：插件进程退出（panic/segfault/exit）→ 宿主视为崩溃，按 backoff（200ms×2、上限 2s、最多 5 次）自动重启；
在途调用以 `Error{code:"plugin_crashed"}` 收尾。

## 实例插件

实例插件不是「插件的一个变体」，而是**用同一套协议承载平台接入**：注册一个
`Contribution::Adapter`，此后生命周期、收发、目录查询、管理动作、以及对模型
暴露的工具，全部经 `Invoke("<实例名>:<op>")` 路由到你实现的 `AdapterPlugin::invoke_op`。

SDK 侧实现 `InstancePlugin`（`echo-plugin-sdk`）：`configure`（收配置，`Err` 即
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

示例（QQ 插件的做法）：`send_group_msg` / `send_private_msg` / `send_file` /
`send_image` / `send_voice` / `send_json_card` 声明 `visible_reply: true`；
`get_group_list` / `get_friend_list` / `get_group_member_info` /
`get_group_msg_history` 为 `false`。完整 schema 见 `echo-adapter-qq` 仓的
`src/tools.rs`（与迁出前的 Core 内置版逐字一致）。

### 能力位 ≠ 工具声明

`capabilities` 是**宿主栅栏**（未声明 → 对应 Invoke 一律拒绝），`tools` 是
**模型可见面**。两者独立：可以声明工具而关闭某个能力位（该工具的调用会被
宿主拦下），但通常应保持一致。

## 模式与参考

- **最小报错即修正**：`InvokeOutcome::Error{code, message}` 的 `message` 是模型可见文本——写清"发生了什么、应该怎么做"（与内置工具的错误口径一致）。
- **超时自声明**：`ToolContribution.timeout_hint_secs` 与内置工具 `timeout_hint` 同义——宿主守卫取 `max(配置 base, hint + 宽限)`。
- **多模态**：`InvokeOutcome::Ok.images`（图片 URL / data URI）与内置工具 `execute_rich` 同口径。
- **零全局态纪律**：插件进程内不要持有进程级可变单例（`static OnceLock` 等）——用实例字段（见试点插件的 `WebSearch` 持 client 的做法）。
- **宿主服务回呼（HostCall）**：插件可回呼宿主注册的命名服务（当前默认提供 `"sanitizer"`：`redact` / `scan` / `register_secret`——处理敏感文本前先脱敏、登记插件自己的凭证）。SDK 侧在 `PluginHandler::on_ready` 收到 `HostClient`（`call_with_timeout` 可调超时，默认 30s）；服务不可用时返回 `Service{code, message}` 错误而非挂死。
- **参考实现**：`source/plugin/echo-plugin-example/`（websearch + calculator，含 e2e）；协议全消息测试对端 `echo-plugin-host/src/bin/echo-plugin-test-peer.rs`。

## 本地验证

```bash
# 1) 构建插件
cargo build --release -p echo-plugin-example

# 2) 安装到约定目录
install -Dm755 target/release/echo-plugin-example \
  ~/.local/libexec/echo-agent-core/plugins/echo-plugin-example/echo-plugin-example

# 3) 写 plugins.toml（core.toml 同目录，格式见「装载」节）
# 4) 重启 Core：systemctl --user restart echo-agent-core.service
# 5) 在 Panel 里检查工具列表（分类"插件"）并调用
```

调试：插件 stderr 会进 Core 的 journal（`journalctl --user -u echo-agent-core.service`），带 `plugin = <id>` 字段。
