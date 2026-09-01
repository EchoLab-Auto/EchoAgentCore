---
id: dev-testing
title: "测试策略"
group: 开发指南
x: 970
y: 360
---
# Testing Strategy

400+ tests across four layers, covering every source file in all crates.

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
- **Security** (echo-agent tools): path-traversal guards for read/write/edit
  (canonicalize + workspace containment), dangerous command patterns,
  absolute-path rejection, 50-result search cap.
- **Persistence** (echo-agent): session save/load round-trips, lenient
  malformed-entry recovery, idle eviction, ConfigStore atomic patches.
- **LLM providers** (OpenAI / Anthropic): request body builders (tool-call
  grouping, system-message joining, malformed-argument fallback).
- **SSE streaming** (OpenAI / Anthropic): pure-function `parse_sse_event` /
  `parse_anthropic_event` — content deltas, parallel tool-call deltas,
  `[DONE]` termination, in-stream error payloads, malformed data skips.
- **Calculator**: recursive-descent parser: operator precedence/associativity,
  floating-point, unary minus, whitespace, 8 malformed patterns.
- **Skill system**: keyword matching (case-insensitive, empty keywords),
  deterministic multi-hit ordering, recursive SKILL.md discovery, runtime
  enable-state preservation, and hot reload for updates/additions/deletions.

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
- `echo-adapter-qq` — NapCat client against a `wiremock` HTTP server:
  login status detection, WebUI fallback, reverse-WS config, QR fetch.

### 4. Concurrency tests
Multi-threaded tokio runtime (`#[tokio::test(flavor = "multi_thread")]`):

- `SessionManager::get_or_create` — 32 concurrent callers on the same key
  produce exactly 1 session (DashMap entry API).
- `Agent::process_message` — two concurrent messages to the same session
  with a slow (50 ms) provider produce non-interleaved history
  (turn_lock serialization verified by role order).

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
```

`.github/workflows/ci.yml` runs all three on push/PR, stable + nightly.

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
