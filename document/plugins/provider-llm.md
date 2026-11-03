---
id: provider-llm
title: "LLM Provider 插件"
group: Core
x: 4800
y: 1200
---

# LLM Provider 插件

> **定位**：`echo-agent.provider.llm` **这个插件本身**——provider 构建与两条装饰器的归属、13 条 API 命令域、以及「插件声明命令、宿主执行副作用」这条分工线为什么这么划。模型选择与供应商池的使用语义见 [设置 · API](../panel/settings-api.md)；provider 抽象与消息词汇见 [框架（内核）](../core/frame.md)。读者：加一家 provider、改装饰器、或排查「切模型没生效」的开发者。

## 它拥有什么、不拥有什么

| 拥有 | 不拥有 |
|---|---|
| `create_provider(config)`：按 `ProviderConfig` 构建 OpenAI 兼容 / Anthropic Messages / Ollama | `[agent]` 配置的解析与落盘（宿主读 `core.toml`） |
| `wrap_metering`：token 用量计量装饰器 | 指标存储（宿主实现 `UsageRecorder`） |
| `wrap_redacting`：请求出口脱敏装饰器 | 脱敏规则表（宿主装配） |
| 供应商池规则（DeepSeek `/anthropic` 端点自动路由、Kimi For Coding 的 `base_url` 归一化） | API key 的读取与安全存储 |
| 13 条 API / provider 命令的**认领与参数校验** | 命令的**副作用**（重建 provider 链、广播、失效缓存） |

分界线的判据只有一条：**这行代码需要认识 `Agent` 吗**。需要 → 宿主；不需要 → 插件。宿主那一侧被压成 `ProviderCommandHost`（一组 `BoxFuture` 方法：`switch_model` / `switch_provider` / `set_system_prompt` / `update_api` / `switch_api` / `test_api` / `query_balance` …），插件只调它，不碰任何宿主类型。

## 身份与贡献

| 项 | 值 |
|---|---|
| 插件 id | `echo-agent.provider.llm` |
| kind | `provider`（persona 白名单参与） |
| 名称 / 路由 | 「LLM Provider」/ `llm` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/plugin/echo-plugin-provider-llm`（10 文件，约 3400 行） |

| 贡献 | 内容 |
|---|---|
| 命令域 × 13 | `SwitchModel` / `SwitchProvider` / `SetSystemPrompt` / `RequestSystemPrompt` / `UpdateApiConfig` / `SwitchApi` / `SetSessionApi` / `ClearSessionApi` / `RequestRemoteApiProfiles` / `TestApi` / `QueryApiBalance` / `QueryApiMetrics` / `DeleteApi` |
| 服务依赖 | `echo-agent.host.provider`（`PROVIDER_HOST`） |
| 服务提供 | **无**——`mount` 不 provide 任何键 |
| re-export | `LlmProvider` / `ChatMessage` / `ChatRequest` / `Usage` / `ToolDefinition` / `ReasoningEffort` / token 估算与截断——调用方只需 `use echo_plugin_provider_llm::…` |

**`mount` 不注册运行期资源**，只把宿主适配器绑到 `PROVIDER_HOST`。所以「启用这个插件」的真实含义是**「13 条命令可被派发」**，而不是「多了一个 provider」——provider 实例本身由宿主在配置变更时按需构建。

## 两条装饰器

装饰器是 provider 的**包装层**，同一个 `LlmProvider` trait，套两次：

| 装饰器 | 作用 | 为什么放在插件侧 |
|---|---|---|
| `wrap_metering` | 记录每次请求的 token 用量（余额/用量图表的数据源） | 它包的是 provider，插件就是 provider 的归属者；换一家 provider 不该换一套计量 |
| `wrap_redacting` | 请求出口脱敏——秘密不出本机 | 它与 provider 无关、与配置无关，但**必须在最外层**：漏一层就等于漏一次出站 |

顺序有意义：脱敏在最外层（先脱再计量，避免计量日志里出现原文），计量在内层（计的是真实发出的量）。

## 依赖纪律

只依赖定义层（`echo-defs`）与传输依赖（`reqwest` / `futures-util` / `tokio`）——**不依赖 `echo-agent`，不依赖配置或指标等业务 crate**。三个协议的客户端是本 crate 的私有模块 `providers::{openai, anthropic, ollama}`（2026-11 由独立 crate 合并进来，判据见下文「为什么三家客户端不独立成 crate」）。宿主把 `[agent]` 配置投影成 `ProviderConfig`、把指标存储实现成 `UsageRecorder`，两者都是 5 行 glue，不进本 crate。

这条纪律的直接收益：加一家 provider 只改本 crate，不动宿主；换掉指标存储只改宿主，不动本 crate。

### 为什么三家客户端不独立成 crate

`providers/{openai,anthropic,ollama}.rs` 此前是 `source/llm/echo-llm-*` 三个独立 crate，被 `deps-policy.txt` 归在"中间层"。2026-11 合并进来，判据是**消费者数量**：

| 原 crate | 消费者（`grep -rl <name> source/*/*/Cargo.toml`） |
|---|---|
| `echo-llm-openai` | `echo-llm-ollama`（组内）、本插件 —— 仓外 0 |
| `echo-llm-anthropic` | 本插件 —— 仓外 0 |
| `echo-llm-ollama` | 本插件 —— 仓外 0 |

「中间层」的定义是**被多方复用的底座**（如 `echo-tool` 被 7 个 crate 用、`echo-sanitize` 被 3 处用）。这三个各自只有一个真消费者，crate 边界换来的不是"可替换点"而是目录层级——协议差异靠的是 `LlmProvider` trait（在 `echo-defs`），不是 crate 之间的抽象。

可用判据：**消费者 = 1 → 是那个消费者的实现细节，合并；消费者 > 1 → 是共用底座，独立**。合并后 `providers` 是**私有模块**：外部只认 `create_provider(&ProviderConfig)`，换实现不需要外部改任何代码——这正是把 `LlmProvider` 放在 `echo-defs` 的意义。

跨仓库的外部 provider 不受影响：实现 `echo_defs::LlmProvider` 的 crate 无需进本仓。

## 启停与门控

- `kind = provider`，persona 白名单参与：某个 agent 不启用它，`SwitchModel` / `UpdateApiConfig` 这类命令对该人格不再派发
- **但模型本身照常工作**——provider 实例由宿主持有，插件禁用不等于断网。这是刻意的：把「能不能改配置」和「能不能调模型」分成两个开关
- 命令处理时若 `PROVIDER_HOST` 未绑定，回明确错误而不是静默成功
