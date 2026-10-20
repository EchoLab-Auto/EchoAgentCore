---
id: mcp-import
title: "MCP 作为工具插件导入通道（评估）"
group: 规划
x: 620
y: 3300
---

# MCP 作为工具插件导入通道（评估）

> **定位**：评估把 **MCP（Model Context Protocol）** 作为「工具类插件」的兼容导入通道——让现成的 MCP server 不必改写成 echo-plugin 就能提供工具。含实测证据、能力映射、三种接入形态的成本与风险、以及**建议与升级条件**。读者：决定要不要开这条轨的维护者；相关：[完全解耦推进计划](./decoupling-plan.md)「Phase 6」、[插件开发指南](../plugins/plugin-authoring.md)。
> **状态**：**评估完成（2026-10-19），结论为「先桥接、按需原生」**——本轮不实施，落项待排期。

## 一、结论

| 问题 | 结论 |
| --- | --- |
| 技术上能不能接 | **能**。spike 已跑通「rmcp 客户端 + 我们自己的加固 spawn + 列工具 + 调工具」全链路（见 §二） |
| 该怎么做 | **两个阶段**：先写一个**桥接插件**（`echo-plugin-mcp-bridge`，对 Core 零改动），用真实使用验证价值；只有当 MCP 成为一等公民（动态工具集 / 远程 HTTP / 进度通知都要）时，才升为 **Core 原生 client** |
| 现在要不要动 Core | **不要**。桥接方案复用全部既有机制（崩溃重启 / 可逆停止 / 频率限额 / 资源限额 / 面板清单），Core 不引入 rmcp 依赖 |
| 计划里「省 2-4 人周」成立吗 | **对插件作者成立，对我们不成立**。省下的是「每接一个外部能力就写一个 Rust 插件」的成本；我们自己的接入成本见 §四.4/§四.5 |

### 1.1 为什么值得评估

[完全解耦推进计划](./decoupling-plan.md) Phase 6 留了一项「协议选型决策：评估 MCP 作为工具类插件的兼容导入通道」。它的价值主张是**生态复用**：MCP 已是事实标准，社区有大量 server（数据库 / 浏览器 / 文件系统 / SaaS）。对这些能力，「写一个 echo-plugin」与「复用现成 MCP server」的成本差一个量级。

但代价是引入**第二个协议**——这正是本仓库一直在对抗的东西（自研统一协议、单一 `Contribution` 面）。所以这个决策不能只看"能接"，要看**接了之后双协议并存的长期成本**。

## 二、实测证据（rmcp 3.5.1）

### 2.1 基本信息

| 项 | 值 |
| --- | --- |
| crate | `rmcp` 3.5.1（[官方 rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)） |
| 许可 | **Apache-2.0**（与我们的 MIT 兼容，保留声明即可） |
| MSRV | **1.88**（我们 `rust-version = 1.98`，无压力） |
| 体积 | 最小特性集 `client` + `transport-async-rw`：**51** 个传递依赖，其中 **47 个本仓库已有**，只新增 **4 个**：`rmcp` / `futures` / `futures-executor` / `tokio-stream` |
| HTTP 栈 | **不引入**（`reqwest` / `hyper` / TLS 只在 `transport-streamable-http-*` 特性下才拉，本轮不需要） |
| 构建 | 最小集 release 构建 ~8s（本机，含 rmcp 自身与兄弟依赖） |

### 2.2 三个关键发现

**① 我们的进程加固可以完整保留。**
rmcp 自带的 `TokioChildProcess` 走 `process-wrap`，**不暴露任意 `pre_exec` 钩子**——这意味着用它就装不上我们刚做的 rlimit（CPU / 内存 / fd / 文件 / 进程数），也拿不到 `env_clear` + `process_group(0)` 的显式控制。

但 rmcp 把「分帧」与「进程」拆开了：`AsyncRwTransport::new_client(read, write)` 是**公开**的，接受任意 `AsyncRead`/`AsyncWrite`。因此正确姿势是——

> **自己 spawn（沿用 `echo-plugin-host::stdio` 那套加固），只把 stdin/stdout 交给 rmcp 做 JSON-RPC 分帧。**

spike 里这条路径已跑通：`env_clear` + 仅透传 `PATH` + `process_group(0)` + `pre_exec` 装 `RLIMIT_NOFILE`，子进程照常完成 MCP 握手并响应 `tools/call`。

**② 帧格式不通用，只能复用 spawn，不能复用帧层。**
MCP stdio 用**换行分隔的 JSON（NDJSON）**，我们自己的插件协议用 **4 字节小端长度前缀 + JSON**。两套帧层互不兼容——「复用我们的 stdio 运输层」这个想法在帧层就断了，只能复用**进程管理**那半边。

**③ 动态工具集在我们这一侧还没有承接面。**
MCP 支持 `notifications/tools/list_changed`（server 随时增删工具）。而我们的插件工具是**启动期一次性注册**进 `ToolRegistry` 的（`core/src/main.rs` 的 `register_plugin_tools`），运行期插件重新 `Register` 也**不会**刷新已注册的工具集。

这是**既有机制的缺口**（跟 MCP 无关，只是 MCP 让它变得显眼）：哪怕接的是 echo-plugin，运行期换工具集同样不生效。要接 MCP 的动态工具集，得先补这条面。

### 2.3 spike 结论

同一份 spike 验证了两件事：

1. rmcp 客户端能正常发现并调用 MCP server 的工具（`list_all_tools` → `call_tool` → `Text` 内容）；
2. **把 spawn 换成我们自己的加固版本，协议层完全照常工作**（`AsyncRwTransport::new_client(stdout, stdin)`）。

> 证据（spike 输出）：
> ```
> [client] 发现 1 个工具
>   - get_weather | 查天气 | schema keys: ["properties", "required", "type"]
> [client] 调用结果: [Text(TextContent { text: "Beijing: 22°C 晴", ... })] is_error=Some(false)
> [own-spawn] 用自家 spawn（含 rlimit）拉到 1 个工具
> [own-spawn] 调用结果: [Text(TextContent { text: "Shanghai: 22°C 晴", ... })]
> ```
> spike 是 `/tmp` 下的一次性程序，未入库（结论与最小复现代码已摘录在本文）。

## 三、能力映射

### 3.1 工具定义

| MCP `Tool` | 我们的 `ToolContribution` | 缺口 |
| --- | --- | --- |
| `name` | `name` | 需**命名空间隔离**（见 §四.4） |
| `title` | — | 无对应；丢弃（面板已有 `name`） |
| `description` | `description` | ✅ |
| `inputSchema`（JSON Schema） | `parameters` | ✅ 形状一致，可直接搬 |
| `outputSchema` | — | 无对应；丢弃（我们的工具结果以文本为主） |
| `annotations`（只读/幂等/破坏性等提示） | — | **规范明确要求视为不可信**，只可作 UI 提示；v1 丢弃 |
| — | `category` / `package` | 由桥/接入层**填**：`package = "mcp:<server>"`，面板按包分组 |
| — | `timeout_hint_secs` | 由接入层给缺省（MCP 无超时概念） |
| — | `visible_reply` | 缺省 `false` |

### 3.2 调用与结果

| MCP | 我们 | 缺口 |
| --- | --- | --- |
| `tools/call {name, arguments}` | `Invoke { call_id, contribution, ctx, payload }` | MCP 无会话上下文 → 放进 `_meta`（`session_id` / `team_id` / `branch_id`）；**我们这侧工具拿不到 ctx 就只能靠全局态**，这是必须转发的 |
| `CallToolResult.content[]`：`text` / `image` | `InvokeOutcome.text` + `images` | ✅ 可映射 |
| `content`：`audio` / `resource` / `resource_link` | — | 丢弃或降级成文本链接（v1 建议**降级为文本**并在结果里注明） |
| `structuredContent` | — | 无对应（我们只有文本 + 图片），降级为 JSON 文本 |
| `isError` | `InvokeOutcome::Error{code,message}` | ✅ 语义可对齐 |
| `notifications/cancelled` | `Cancel { call_id }` | ✅ 语义相近（都把 `call_id` 映射成 JSON-RPC `id`） |
| `notifications/progress` | — | **缺**：长任务进度没有承接面（v1 丢弃） |
| `notifications/tools/list_changed` | — | **缺**：见 §二.3③（运行期工具集不刷新） |

### 3.3 传输与生命周期

| MCP | 我们 | 说明 |
| --- | --- | --- |
| stdio（NDJSON） | stdio（长度前缀） | 帧层不通用；**进程管理可复用** |
| Streamable HTTP | — | v1 不做（要引入 HTTP 栈 + OAuth 2.1 令牌管理，安全面完全另一回事） |
| `initialize` 版本协商（多版本列表） | `Hello` 单值相等 | rmcp 自己处理 MCP 侧协商，我们不掺和 |
| 断线/崩溃恢复 | supervisor 重启预算 | 桥接方案可直接复用；原生方案要再写一套 |

## 四、三种形态与成本

### 4.1 桥接插件（**建议**）

一个独立的 stdio 插件进程：**对 Core 说 echo-plugin 协议，对 MCP server 说 MCP**。

```
Core ──(echo-plugin/长度前缀)──> mcp-bridge ──(MCP/NDJSON)──> MCP server 进程
```

- **Core 零改动**：今天就能用，`plugins.toml` 加一行即可
- 复用全部既有机制：崩溃重启、可逆停止、频率限额、资源限额、面板清单/启停/重启、来源层与命令展示
- 每个 MCP server 一个桥进程（一进程一 server）：崩溃隔离，`plugins.toml` 行视图天然对应
- 代价：多一跳进程 + 两次 JSON 序列化往返（< 1ms 量级，工具调用本就秒级间隔，可忽略）
- 代价：桥自身要维护；`list_changed` / `progress` 仍然丢（与 4.2 同等受限，直到 §二.3③ 补上）

### 4.2 Core 原生 client

新增 crate `echo-plugin-mcp`：配置段（`[[mcp_server]]`）、spawn（复用加固代码）、rmcp 客户端、生命周期与重启预算、频率限额包装、descriptor 集成。

- 少的是一跳进程；换工具时不用起桥
- 代价：**第二套插件生命周期**（supervisor 与 rmcp service 各一份，重启预算 / 排空 / 状态机要各写一遍并保持同步——这是长期成本的主要来源）
- 代价：Core 依赖 rmcp（+4 crate，可接受但不免费）；`no_global_state` / 依赖方向门禁都要加白名单条目

### 4.3 不接

保持现状：每个外部能力写一个 echo-plugin（SDK 成熟，`echo-plugin-example` 是模板）。

- 优点：单一协议、单一生命周期、单一安全模型
- 缺点：MCP 生态的现成 server 用不上；对"接一个冷门 SaaS"这类需求，作者要写 Rust

### 4.4 三者对比

| 维度 | 桥接插件 | 原生 client | 不接 |
| --- | --- | --- | --- |
| 启动成本 | **3-4 人日** | 6-8 人日 | 0 |
| Core 改动 | **0**（可选：动态工具集） | 大（新 crate + 配置 + 第二生命周期） | 0 |
| 新增依赖 | 桥 crate 内（不进 Core） | rmcp 进 Core（+4 crate） | 0 |
| 复用既有加固 | ✅ 全部 | ⚠️ spawn 可复用，生命周期要重写 | — |
| 动态工具集 / 进度通知 | ❌（同原生，除非先补 Core） | ❌（同样要补 Core） | — |
| 长期维护面 | 一个插件 crate | 协议 + 生命周期 + 协议演进跟进 | 无 |
| 协议并存成本 | **隔离在插件边界内** | **渗进 Core** | 无 |

> 关键点：**动态工具集与进度通知的缺口在 Core，不在接入形态**。选原生并不能绕过它——所以「原生更彻底」这个直觉在这一项上不成立。

### 4.5 成本估算（人日）

| 工作 | 桥接 | 原生 |
| --- | --- | --- |
| MCP 客户端接线（rmcp + 自家 spawn） | 1 人日（spike 已证明） | 1 人日 |
| 工具映射 + 结果降级 + cancel | 1 人日 | 1 人日 |
| 配置 / 清单 / 面板对接 | 0.5 人日（复用插件机制） | 2 人日（新配置段 + 新 descriptor 来源） |
| 生命周期（重启预算 / 排空 / 状态） | 0（复用 supervisor） | 2-3 人日（第二套） |
| 测试（协议对端 + 一致性） | 0.5-1 人日（rmcp 可当 test server） | 1-2 人日 |
| **合计** | **3-4 人日** | **6-8 人日 + 持续跟进 rmcp 演进** |

## 五、安全面

MCP 引入的攻击面与现有插件不同，逐条对照：

| 风险 | 现状 | 建议 |
| --- | --- | --- |
| **工具描述即 prompt 注入** | 插件描述本来就进模型上下文 | 不新增风险；但 MCP server 来源（第三方 npm/pip）比"自己写的插件"更不可信 → **描述落盘与展示要标明来源**（已有 `source_path` 字段可承载） |
| **`annotations` 不可信** | 规范原文要求视为不可信 | v1 直接丢弃，不做任何"只读则免确认"的自动化 |
| **远程 server + OAuth** | 无 | **v1 不做 HTTP 传输**。远程 MCP 意味着令牌管理、egress 白名单、TLS 校验——安全面等于再做一个子项目 |
| **工具名冲突** | 我们的 registry 是扁平的，宿主保留名优先（`register_plugin_tools` 已挡） | 用**命名空间前缀** `mcp__<server>__<tool>`（与联邦 `<peer>__<tool>` 同一惯例，`sanitize_peer_component` 已有现成清洗） |
| **子进程失控** | supervisor：rlimit + 频率限额 + 重启预算 | **必须走我们自己的 spawn**（§二.2①），不能用 rmcp 的 `TokioChildProcess` |
| **工具返回不可信内容** | 出口脱敏（redactor）已覆盖工具结果 | ✅ 复用；MCP 内容同样过一遍 |
| **供应链** | MCP server 常以 `npx -y` / `uvx` 拉取 | 桥接方案的 `command` 由用户在 `plugins.toml` 显式写（与今天一致），**不做自动拉取** |

## 六、建议与后续

### 6.1 建议

**做桥接，不做原生；先把「运行期动态工具集」这条面补上（与 MCP 无关，是既有缺口）。**

理由：

1. 桥接用 **3-4 人日**换到 MCP 生态，且**零 Core 改动**——风险与成本都在可回退的一侧（删掉一行配置就退出）；
2. 原生方案的额外收益（少一跳进程）远小于其成本（第二套生命周期 + 协议演进跟进）；
3. **真正的能力瓶颈在 Core**（动态工具集 / 进度通知），这两个缺口不解决，原生方案一样残缺——所以先把它们当成**独立议题**评估，不被 MCP 绑架。

### 6.2 升级为原生的条件（触发式，不是排期）

满足任一条件再考虑把 MCP 升为一等公民：

- 桥接方案在生产里跑通，且**同时**接 ≥ 3 个 MCP server（「插件行数增长」不再是问题，而"多一跳"开始变得碍眼）；
- 需要 **Streamable HTTP 远程 server**（此时要引入 HTTP 栈 + 令牌管理，安全面已独立成项，原生与桥接的成本差距拉平）；
- rmcp 的 `Transport` 接口稳定且我们**已经**为别的目的引入它（例如将来做"对外暴露 MCP server"反向能力——那时复用同一 crate 更划算）。

### 6.3 明确不做

- 不做远程 MCP（HTTP + OAuth）——安全面另立项；
- 不做 MCP **server** 方向（把我们的工具暴露给外部 agent）——与当前"多 agent 编排"路线冲突，且涉及权限边界重设计；
- 不因为 MCP 而放宽 `annotations` / 自动批准类语义。

### 6.4 遗留与关联

- **运行期动态工具集**（`tools/list_changed` 的承接面）：既有缺口，建议单独评估——它同时影响 echo-plugin（运行期 `Register` 不刷新）与 MCP；
- **进度通知**（`notifications/progress`）：我们的工具管线没有进度通道，与 MCP 无关；若将来做长任务 UI，两边都需要；
- 桥 crate 的落点：独立仓库（与 `echo-adapter-qq` / `echo-plugin-example` 同侧），不进本仓库 workspace——它是**消费者**，不是框架。
