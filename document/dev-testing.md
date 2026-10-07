---
id: dev-testing
title: "测试策略"
group: 工程
x: 2240
y: 1000
---

# Testing Strategy

**Core（cargo）** 700+ 条、**Panel 前端（vitest）** 200+ 条、**部署 CLI（node:test）**
27 条，覆盖各 crate 的源文件与关键交互契约（计数随开发增长，量级为本文件维护基线）。

## Layers

### 1. Unit tests (`#[test]` / `#[tokio::test]`)
Located next to the code in `#[cfg(test)] mod tests` blocks.

- **Protocol** (echo-core): OneBot event parsing for every variant family
  (message / notice / request / meta), message segments, action builders,
  unknown-type preservation, sender nickname (card vs nickname fallback).
- **Wire protocol** (echo-protocol): command/event WS round-trips, legacy
  payload decoding (missing `branch_id`), malformed frame handling, FanoutHandle
  subscriber pruning.
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
- **Media store** (echo-defs, 2026-09-24): reference validation (charset /
  `..` / path traversal), content-hash dedup + atomic write, data-URI spill
  round-trip, failure→elided-placeholder, inline-data-URI text rewrite
  (hook JSON), ref→data-URI inlining (model side), missing-file drop.
- **Media migration** (echo-agent session, 2026-09-24): 加载期把历史事件
  （content 内嵌 + images 字段）与时间线 data URI 落盘改写且幂等；投影出口
  把 `/media/<id>` 还原为 data URI（`persisted_refs_inline_on_projection_for_model`）。
- **Media download** (echo-adapter-qq): 远程图下载落盘（文件字节与源一致）、
  失败保留原 URL、data URI / 已有引用透传。
- **QQ 表情渲染** (echo-core + adapter): face id → 名称对照表有序性/未知回退、
  `readable_text` 混合渲染（`[表情:呲牙]`、`[骰子:6]`、纯表情消息不再整条丢弃）。

### 2. Property tests (proptest)
Generated inputs that must satisfy invariants:

- `SessionKey` round-trip for arbitrary field values.
- `ChatMessage` / `ToolCall` JSON round-trips.
- Message-event JSON parse → serialize → parse stability for arbitrary
  Unicode text (quotes, control chars, CJK).
- Rich segment (multi-message) round-trips.

### 3. Integration tests (`tests/` dirs)

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

### 4. Concurrency tests

Multi-threaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`):

- `TrunkStore::get_or_create` — 32 concurrent callers on the same key
  produce exactly 1 session (DashMap entry API).
- `Agent::process_message` — two concurrent messages to the same session
  with a slow (50 ms) provider produce non-interleaved history
  (single 模式 `turn_queue` 排队闸门串行化，按 role order 验证).

### 5. 面板前端与部署 CLI（非 cargo）

- **Panel web（vitest + @vue/test-utils，208 条）**：`ChatView` 图片渲染契约
  （`/media/<id>` 懒加载 / 空串省略占位 / data URI 兼容）、设置视图技能/工具/插件
  工作台（筛选、分组维度、详情分区、交叉跳转、脏状态）、右侧栏连接状态卡、
  智能体编辑器分区、协议编解码回归等。**加载态跟踪契约（2026-09-30）**：
  `pending.ts` 三态推进（150ms 延迟显示 / 6s 慢 / 20s 超时 / 可见后最短 400ms）、
  响应事件销账、动态 key 切换（`pending.test.ts`，10 用例）
- **部署 CLI（`npm/echo-agent`，node:test，27 条）**：PATH 注入假 `docker` 做
  CLI 端到端（init 幂等 / up 参数拼装与提示 / down/restart/update/logs/status /
  doctor 分级与阻断码），以及**模板跨文件契约**（compose 注入的 `ECHO_MEDIA_DIR`
  == panel.toml 的 `media_dir`、NapCat 容器名一致、生效配置行不得出现 localhost、
  `docker compose config` 语法校验）。无第三方依赖，CI 直接 `npm test`。

## Mocks & fixtures

- `echo-test-utils` crate (dev-only): `MockAdapter` for registry/tool tests,
  `temp_config_file` helper. Deliberately does **not** depend on echo-agent
  (a dev-dependency cycle would compile echo-agent twice and break type
  identity).
- Mock LLM providers (`StaticProvider`, `SlowProvider`, `MockProvider`,
  `ScriptedProvider`) live inside echo-agent's test module and the agent
  integration tests.

## Quality gates (CI)

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

> 环境提示：`agent::tests::update_api_config_persists` 断言空 API key 的回退行为，
> 若 shell 中设置了 `ANTHROPIC_API_KEY` 等环境变量会失败——这是既有行为，
> CI（干净环境）不受影响。

## History of notable fixes caught by tests

- Path traversal in `write_file` / `edit_file` (could escape workspace).
- NapCat `user_id` arriving as integer instead of string.
- OneBot `notify` notice type parsed as `Unknown` (wrong serde tag).
- Session dirty flag cleared before a successful write (data loss).
- Silent serialization / teardown failures now logged.
- OpenID SSE `error` events silently skipped (choices field missing `serde(default)`).
- Rate-limit per-user bucket isolation — u1 overflow didn't affect u2.
- Anthropic SSE tool-use content block — only Text was handled, ToolUse was silently dropped.
- QQ 表情消息被整条丢弃（content 只取 text 段，纯表情消息为空）——`readable_text` 修复。
- 入站图片内嵌 base64 导致时间线快照 8MB / 会话文件 24.6MB / 面板启动 802ms——媒体库落盘修复。
- `accept_private_file` 是死配置（定义了但从未被检查）——文件接收 gap 审查发现。
- 同名同毫秒下载互相覆盖——`create_new` + 序号兜底。
- 技能编辑中点选同一行静默丢弃未保存改动——先 confirm 修复。
- 压缩摘要前缀双写（`[历史摘要] [历史摘要]`）——投影期幂等渲染，测试锁定。
- 「重载技能」只作用于管理代理、人格侧不生效——广播全部运行人格修复（`reload_skills_into` 测试锁定）。
- 更新器 `expected_ids` 与 `BUILTIN_PLUGIN_IDS` 漂移（`orchestration` 移除后残留、更新误报缺 manifest）——`update_script_plugins` 守护测试补齐。
- QQ 图片预下载发生在触发门控之前（被丢弃的消息也白存图，媒体库积累无主文件）——下载下移至「门控与过滤通过后」，`gated_drop_does_not_download_images` 回归测试锁定（wiremock 断言零请求）。
- echo-loop 驱动路径缺工具超时守卫（挂死工具可永久拖住 turn）——`Agent::tool_guard_timeout` 统一两条路径口径，`echo_loop_path_guards_hung_tools_with_timeout` 测试锁定。
- run_sudo / present_menu / framework_update 三个内联工具在此前清理中丢失派发入口（配套 broker/协议/配置仍在但工具不可达）——2026-09 正式废弃：全配套移除、服务加固收紧（`NoNewPrivileges=true`）。
- `QqAdapterConfig::Default` 与 serde 字段默认分裂（`napcat_auto_stop` 一条 false 一条 true）——`default_matches_serde_field_defaults` 锁定两条默认路径一致。
- `list_files`/`search_code` 缺相对路径穿越防护（六个文件工具中只有四个有 guard）——补齐并测试（`list_and_search_reject_relative_traversal`）。
- shell 会话读循环交替顺序读 stdout/stderr——任一流空闲即死锁，**每条命令都假超时**（仅 stderr 持续输出才可能完成）；且 `ShellStop` 只杀 bash 本身、后台任务变孤儿续存。重写为两路专职读取任务 + 通道汇聚 + 哨兵 stdout/stderr 双写收齐语义；spawn 建独立进程组、`ShellStop`/意外退出 killpg 整组终止（`exec_returns_promptly_when_stderr_silent`、`large_stderr_burst_is_fully_captured`、`stop_kills_entire_process_group` 等 8 条测试锁定）。
