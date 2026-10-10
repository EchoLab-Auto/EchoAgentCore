---
id: dev-testing
title: "测试策略"
group: 工程
x: 2240
y: 2050
---

# 测试策略

> **定位**：本仓库的测试体系说明与质量门禁清单——单元 / property / 集成 / 并发 / 前端与 CLI 各层覆盖哪些契约。读者：贡献代码的开发者与 Agent。
> 相关文档：[开发指南](./dev-guide.md)（构建与测试入口）、[文档规范](./documentation-guide.md)。

**Core（cargo）** 700+ 条、**Panel 前端（vitest）** 200+ 条、**部署 CLI（node:test）**
27 条，覆盖各 crate 的源文件与关键交互契约（计数随开发增长，量级为本文件维护基线）。

## 测试分层

### 单元测试（`#[test]` / `#[tokio::test]`）
Located next to the code in `#[cfg(test)] mod tests` blocks.

- **Protocol** (echo-core): OneBot event parsing for every variant family
  (message / notice / request / meta), message segments, action builders,
  unknown-type preservation, sender nickname (card vs nickname fallback).
- **Wire protocol** (echo-protocol): command/event WS round-trips,
  malformed frame handling, FanoutHandle subscriber pruning.
- **Filtering** (echo-adapter): allowlist / denylist / rate limit (sliding
  window with real-time expiry + per-user/per-group/global bucket isolation) /
  keyword / content length / admin bypass, pipeline ordering and short-circuit.
- **Security** (echo-agent tools): 相对路径的穿越防护（read/write/edit 共用
  `guard_relative_path`：canonicalize 目标或最近已存在祖先，拦 `../..` 与
  区外符号链接；写工具在创建目录前校验，区外不留空目录）、绝对路径按显式
  意图放行（多仓库工作流，三个 ops 同一约定，均有正向用例）、危险命令模式、
  search 的 50 条上限。
- **Persistence** (echo-agent): session save/load round-trips, lenient
  malformed-entry recovery, idle eviction, ConfigStore atomic patches.
- **Teamless multi-agent & QQ instances** (echo-agent + core): session
  commands reject a missing `team_id` (no default agent), the process-level
  `EventSink` fans in every persona's events without double delivery, and
  `source/core/src/qq_instances.rs` covers instance resolution — legacy ports
  (3131/3000/6099) + shared container kept, auto-provisioning per QQ-enabled
  persona, distinct ports/containers per instance, compose rendering.
- **LLM providers** (OpenAI / Anthropic): request body builders (tool-call
  grouping, system-message joining, malformed-argument fallback).
- **SSE streaming** (OpenAI / Anthropic): pure-function `parse_sse_event` /
  `parse_anthropic_event` — content deltas, parallel tool-call deltas,
  `[DONE]` termination, in-stream error payloads, malformed data skips.
- **Calculator**: recursive-descent parser: operator precedence/associativity,
  floating-point, unary minus, whitespace, 8 malformed patterns.
- **Skill system**: keyword matching (case-insensitive, empty keywords),
  deterministic multi-hit ordering, recursive SKILL.md discovery, runtime
  enable-state preservation, on-demand reload for updates/additions/deletions
  (broadcast to every running persona — `reload_skills_into`), and real-file
  parse smoke over the shipped `skills/` directory (`#[ignore]` manual).
- **Media store** (echo-defs): reference validation (charset /
  `..` / path traversal), content-hash dedup + atomic write, data-URI spill
  round-trip, failure→elided-placeholder, inline-data-URI text rewrite
  (hook JSON), ref→data-URI inlining (model side), missing-file drop.
- **Media migration** (echo-agent session): 加载期把历史事件
  （content 内嵌 + images 字段）与时间线 data URI 落盘改写且幂等；投影出口
  把 `/media/<id>` 还原为 data URI（`persisted_refs_inline_on_projection_for_model`）。
- **Media download** (echo-adapter-qq): 远程图下载落盘（文件字节与源一致）、
  失败保留原 URL、data URI / 已有引用透传。
- **QQ 表情渲染** (echo-core + adapter): face id → 名称对照表有序性/未知回退、
  `readable_text` 混合渲染（`[表情:呲牙]`、`[骰子:6]`、纯表情消息不再整条丢弃）。

### 属性测试（proptest）
Generated inputs that must satisfy invariants:

- `SessionKey` round-trip for arbitrary field values.
- `ChatMessage` / `ToolCall` JSON round-trips.
- Message-event JSON parse → serialize → parse stability for arbitrary
  Unicode text (quotes, control chars, CJK).
- Rich segment (multi-message) round-trips.

### 集成测试（`tests/` 目录）

- `echo-server/tests/reverse_ws.rs` — real WS server + raw WebSocket client:
  event dispatch → handler → action → response correlation, token rejection,
  handler-priority chain fall-through, connection lifecycle (tracker insert/remove).
- `echo-agent/tests/agent_integration.rs` — real Agent driven through the backend:
  SendMessage → LLM → AgentOutput event, session creation, clear, system prompt
  propagation, model switch, API profile switch/delete, skill toggle, **skill
  keyword → prompt injection**, **non-trigger caching**, **concurrent turn
  serialization** (slow provider → no interleaving).
- `source/core/src/management.rs` (inline integration tests) — management WS
  server on ephemeral port: Panel command → agent backend, agent event → Panel
  forward.
- `echo-adapter-qq`（src 内 `#[cfg(test)]` 内联测试——`adapter/mod.rs`、
  `napcat/mod.rs`、`handler.rs`；`tests/` 目录仅 `owner_qq.rs`）— NapCat client
  against a `wiremock` HTTP server:
  login status detection, WebUI fallback, reverse-WS config, QR fetch.
  Multi-instance adapters (`QqAdapter::with_instance(id, persona, cfg)`)
  carry their instance name/display name and persona metadata.
- `echo-web-server/tests/media.rs`（Panel 仓库）— `/media/{name}` 路由端到端：
  文件字节与 Content-Type 正确、`immutable` 强缓存头、目录穿越 / 未知文件 404。

### 并发测试

Multi-threaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`):

- `TrunkStore::get_or_create` — 32 concurrent callers on the same key
  produce exactly 1 session (DashMap entry API).
- `Agent::process_message` — two concurrent messages to the same session
  with a slow (50 ms) provider produce non-interleaved history
  (single 模式 `turn_queue` 排队闸门串行化，按 role order 验证).

### 面板前端与部署 CLI（非 cargo）

- **Panel web（vitest + @vue/test-utils，208 条）**：`ChatView` 图片渲染契约
  （`/media/<id>` 懒加载 / 空串省略占位 / data URI 兼容）、设置视图技能/工具/插件
  工作台（筛选、分组维度、详情分区、交叉跳转、脏状态）、右侧栏连接状态卡、
  智能体编辑器分区、协议编解码回归等。**加载态跟踪契约**：
  `pending.ts` 三态推进（150ms 延迟显示 / 6s 慢 / 20s 超时 / 可见后最短 400ms）、
  响应事件销账、动态 key 切换（`pending.test.ts`，10 用例）
- **部署 CLI（`npm/echo-agent`，node:test，27 条）**：PATH 注入假 `docker` 做
  CLI 端到端（init 幂等 / up 参数拼装与提示 / down/restart/update/logs/status /
  doctor 分级与阻断码），以及**模板跨文件契约**（compose 注入的 `ECHO_MEDIA_DIR`
  == panel.toml 的 `media_dir`、NapCat 容器名一致、生效配置行不得出现 localhost、
  `docker compose config` 语法校验）。无第三方依赖，CI 直接 `npm test`。

## Mock 与夹具

- `echo-test-utils` crate (dev-only): `MockAdapter` for registry/tool tests,
  `temp_config_file` helper. Deliberately does **not** depend on echo-agent
  (a dev-dependency cycle would compile echo-agent twice and break type
  identity).
- Mock LLM providers (`StaticProvider`, `SlowProvider`, `MockProvider`,
  `ScriptedProvider`) live inside echo-agent's test module and the agent
  integration tests.

## 质量门禁（CI）

```bash
cargo test --workspace
cargo clippy --workspace --all-targets   # -D warnings
cargo fmt --all --check
# 部署 CLI（core 仓库）：模板契约 + 假 docker 端到端
(cd npm/echo-agent && npm test)
# Panel 前端（panel 仓库）：组件契约
(cd web && npm run test)   # vitest run
```

`.github/workflows/ci.yml`（两仓库）on push/PR：core 侧 fmt / clippy / deps-lint / 测试
（stable + nightly 矩阵）+ `npm-cli` 作业；panel 侧 fmt / clippy / cargo test + `web` 作业
（`npm run build` + `npm test` = vitest）。**Docker 镜像工作流**：PR 只构建
（守护 Dockerfile），main 推送 `latest`、版本 tag 推送该 tag。

这套测试曾捕获多起真实回归（迁移幂等、锁竞争、发布事务、路径穿越防护、工具超时守卫等），是后续重构与迁移的安全网。

> 环境提示：`agent::tests::update_api_config_persists` 断言空 API key 的回退行为，
> 若 shell 中设置了 `ANTHROPIC_API_KEY` 等环境变量会失败——这是既有行为，
> CI（干净环境）不受影响。
