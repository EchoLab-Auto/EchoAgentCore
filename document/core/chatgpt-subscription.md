---
id: chatgpt-subscription
title: "ChatGPT 订阅接入"
group: Core
x: 648
y: 560
---

# ChatGPT 订阅接入（`openai-chatgpt`）

> **定位**：说明 Core 如何用 ChatGPT Plus/Pro 订阅额度当作模型通道——登录流程、协议差异、事实来源与风险边界。读者：要配置或维护这条通道的人。
> 相关：[多模态输入](./multimodal.md)（`images` 字段在这条通道上的形态）、[协议与数据流](../protocol/protocol.md)（`SendMessage` 的载荷）、[配置与持久化](./config-persistence.md)。

## 一句话

`provider = "openai-chatgpt"` 的 profile 走**订阅额度**（不消耗 API 计费），
凭据是 OAuth 登录态而不是 API key。

## 登录

**从面板设置页登录（推荐）**：供应商下拉选「ChatGPT 订阅（Plus/Pro）」，登录块
就在选择器下方——点「登录 ChatGPT」，Core 回推授权链接，点「打开」或「复制」
到你自己的浏览器完成授权。回调会打到 **Core 那台** 的 1455 端口。

也可以走命令行（无面板 / 排障时）：

```bash
echo-agent-core login-chatgpt              # 起回调服务器并打印授权 URL
echo-agent-core login-chatgpt --print-url  # 只打印 URL（远端 Core 场景）
```

两条路走同一套代码（`chatgpt::auth` + Responses 客户端）。面板侧只多三件事：
`StartChatGptLogin` / `CancelChatGptLogin` / `RequestChatGptStatus` 三条命令，
以及两个回推事件（URL 与结果）——登录**必须**由 Core 发起，因为 OAuth 回调
只能落到 Core 的端口上；在浏览器里做 OAuth 拿到的是"只在这台浏览器有效"的
令牌，而真正要用的进程在 Core 那边。

- 授权 URL 打开后完成登录，浏览器跳回 `http://localhost:1455/auth/callback`
- 令牌写入 `~/.local/share/echo-agent-core/chatgpt-auth.json`（**0600**）
- `$ECHO_CHATGPT_AUTH` 可覆盖路径；`$ECHO_MEDIA_DIR` 同族的另一个数据根
- access token 到期前 2 分钟自动用 refresh_token 续（续完立即写回）
- 回调端口 **1455 不能改**：`redirect_uri` 随 OAuth client 注册，改端口授权服务器直接拒。
  该端口也是官方 codex CLI 登录用的端口，两边同时登录会有一个绑定失败（报错里会写明）

配置示例：

```toml
[agent.api_profiles.chatgpt]
provider = "openai-chatgpt"
model = "gpt-5-codex"     # Codex 可用模型
api_key = ""              # 故意留空：凭据是登录态，不是密钥
```

## 为什么必须另写一个客户端

Core 原有的 `openai` provider 只讲 `/chat/completions`。这条通道走的是
`https://chatgpt.com/backend-api/codex/responses`——**没有 chat-completions 形态**。

三处结构差异（`agent-module-provider-llm/src/chatgpt/responses.rs`）：

| | chat/completions | Responses |
|---|---|---|
| 系统提示词 | `messages[0].role=system` | 顶层 `instructions`，不在 input 里 |
| 工具调用 | 嵌在 assistant 消息的 `tool_calls` | **独立 item** `{type:"function_call"}` |
| 工具结果 | `{role:"tool", tool_call_id}` | `{type:"function_call_output", call_id}` |

系统提示词放错位置**不会报错**——回答还像样，只是不再遵守人格与规则。
这是本文件最需要记住的一处静默失败。

## 事实来源（实测，不是推测）

端点、client_id、参数名、claim 名全部取自**官方 Codex 二进制**
（`@openai/codex-linux-x64@0.169.0`，`strings` + 关键串定位）：

| 项 | 值 |
|---|---|
| client_id | `app_EMoamEEZ73f0CkXaXp7hrann` |
| authorize | `https://auth.openai.com/oauth/authorize` |
| token / revoke | `https://auth.openai.com/oauth/token` / `oauth/revoke` |
| scope | `openid profile email offline_access` |
| 回调 | `http://localhost:1455/auth/callback` |
| 额外参数 | `id_token_add_organizations` / `codex_cli_simplified_flow` |
| account claim | `https://api.openai.com/auth` → `chatgpt_account_id` |
| API base | `https://chatgpt.com/backend-api/codex` |
| 请求头 | `chatgpt-account-id`、`originator: codex_cli_rs` |

刻意**没有**加 `OpenAI-Beta` 这类常见头：二进制里找不到，多带一个可疑头
只会更容易被判异常。

## 风险边界（必须知情）

1. **client_id 是 Codex CLI 的，不是发给第三方程序的**。这是全生态的普遍做法
   （Cline / Roo Code / opencode / goose 都如此），但**不是公开 API**：端点、
   校验、client 可用性都由上游单方面决定，失败形态是登录/请求直接报错。
2. **非官方用法**，没有 SLA 与支持；账号层面的风险由使用者自担。所以它做成了
   **可选 provider**，默认不启用，也不影响其它 provider。
3. **额度与 ChatGPT 客户端共享**，不是无限的第三方通道。

## 网络要求

这条通道必须能直连 `openai.com` / `chatgpt.com`。不支持的地区会得到：

```json
{"error":{"code":"unsupported_country_region_territory"}}
```

Core 把这句翻译成「配 `HTTPS_PROXY` 后重试」（reqwest 默认读该环境变量），
而不是把裸 403 抛给用户——**这是本机实测到的真实失败形态**。

## 排错

| 现象 | 原因 | 处置 |
|---|---|---|
| `ChatGPT 订阅未登录` | 没有令牌文件 | `echo-agent-core login-chatgpt` |
| `绑定 127.0.0.1:1455 失败` | 本机 codex CLI 正在登录 | 关掉它再重试 |
| `state 不匹配` | 回调被伪造 / 用了旧的 URL | 重新发起登录 |
| `HTTP 401` | 令牌失效且 refresh 也失败 | 重新登录 |
| `unsupported_country_region_territory` | 地区限制 | 配代理 |
| 回答不遵守人格 | 系统提示词位置错了（回归） | 查 `build_request_body` 的单测 |
| 面板点「登录」没反应 | 面板发的命令名与 Core 注册的不一致（无载荷变体必须发裸字符串） | `typecheck-extensions` 之外，`ApiSettingsChatGpt` 测试盯住线格式 |
| 「已有登录在进行中」 | 上一次登录仍在等回调（占着 1455） | 点「取消登录」，或等它 5 分钟超时 |
