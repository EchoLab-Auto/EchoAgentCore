# EchoAgentCore Development Documentation

## Index

| Document | Content |
|---|---|
| [architecture.md](../architecture.md) | Architecture spine: composition, crates, seams, events, session, extension points |
| [protocol.md](protocol.md) | Frontend ⇄ Core wire protocol (management WS contract) |
| [install.md](install.md) | One-click user installation, systemd Core service, controlled self-update |
| [config/persistence.md](config/persistence.md) | ConfigStore, agent & gate persistence, atomic writes, sessions |
| [agent/background-tasks.md](agent/background-tasks.md) | Detached tasks, parallel branches, ordered integration, delivery targets |
| [adapter/qq/gating.md](adapter/qq/gating.md) | QQ message gating pipeline (5-layer), runtime mutability |
| [testing.md](testing.md) | Test strategy: unit / proptest / integration / concurrency layers |
| [decisions/](decisions/README.md) | Architecture decision records (ADR) |

> 架构/Agent/Adapter/Config 的通用文档已删除——这些信息通过源码注释（`//! module doc`）和 README 维护，避免文档与代码分叉。
> TUI 前端文档在 EchoAgentPanel 仓库。

## Quick Reference

### Project Structure
```
EchoAgentCore/
├── config/                    # Core TOML configuration
├── skills/                    # SKILL.md skill definitions
├── source/
│   ├── defs/
│   │   └── echo-defs/         # Service Definition layer (vocabulary + traits, zero impl)
│   ├── protocol/
│   │   └── echo-protocol/     # Frontend ⇄ Core wire contract crate
│   ├── backend/
│   │   ├── echo-core/         # OneBot v11 protocol types
│   │   ├── echo-server/       # Reverse WebSocket server
│   │   ├── echo-adapter/      # Adapter trait + filter pipeline + ConfigStore
│   │   ├── echo-agent/        # Agent framework (agent/commands, adapter_bridge)
│   │   ├── echo-adapter-qq/   # QQ/OneBot adapter (adapter/filter, handler)
│   │   └── echo-test-utils/   # Shared test mocks (dev-dependency only)
│   └── core/                  # echo-agent-core binary (composition root)
├── doc/develop/               # This documentation
├── doc/decisions/             # Architecture decision records (ADR)
├── packaging/systemd/         # User service templates
├── scripts/                   # Installer and controlled updater
├── tools/                     # Utility scripts
├── napcat/                    # NapCat Docker config
├── docker-compose.yml         # NapCat container
├── Dockerfile                 # EchoAgentCore container
└── Cargo.toml                 # Workspace manifest
```

### Key Traits
- `LlmProvider` (`echo-defs`) — LLM backend seam; providers: OpenAI/Anthropic/Ollama (in echo-agent)
- `Tool` (`echo-defs`) — LLM-callable function (calculator, adapter mgmt, coding tools)
- `SkillProvider` / `Skill` (`echo-defs`) — skill seam; concrete `SkillRegistry` in echo-agent
- `ChatAdapter` (`echo-defs`) — platform seam (Message/Target/Channel vocabulary + capability words)
- `Adapter` (echo-adapter) — legacy platform integration trait (QQ/OneBot), to converge onto `ChatAdapter`
- `MessageFilter` — Inbound message filter (allowlist, denylist, rate limit, keyword, content length)
- `Handler` — OneBot event handler (priority-ordered chain, panic isolation)

### Key Abstractions
- `ConfigStore` — Atomic TOML read-modify-write (shared by Agent + QqAdapter, eliminates concurrent-write races)
- `GateMode` — Strongly-typed gating mode (`None` / `Allowlist` / `Denylist`), defined in echo-defs, re-exported by echo-protocol for wire sharing
- `AgentMessageHook` — echo-agent implements the one-way `InboundMessageHook`; platform output requires tools
- `FanoutHandle` — Multi-subscriber event fanout with stale subscriber pruning

### Key State
- `Agent` — Provider, tools, skills, sessions, adapters
- `Session` — Per-user conversation (history, turn_lock)
- `QqInner` — QQ adapter internal state (running, connected, active_context)

### Key Flows
1. **QQ input hook**: QqHandler → trigger gating → filter pipeline → AgentMessageHook → structured `qq_message` input
2. **QQ output tool**: Agent tool call → `send_private_msg` / `send_group_msg` → QqAdapter::send_message
3. **Frontend input**: WS command frame → BackendCommand::SendMessage → agent.process_message → BackendEvent::AgentOutput
4. **API config**: Panel /api → UpdateApiConfig → apply + ConfigStore persist → ApiConfigUpdated broadcast
5. **Adapter lifecycle**: QqAdapter::start() → bind port → spawn server task → accept connections → QqHandler
6. **Skill hot reload**: periodic scan → discover SKILL.md → preserve enabled state → replace registry → clear prompt cache
7. **Self-update**: authorized session → fixed framework_update tool → echo-agent-core-update.service → fast-forward/build → atomic replace → Core restart
8. **Background task**: context snapshot → detached branches → completion buffer → creation-order integration → explicit target delivery

### Build & Run

```bash
# Core（Agent + QQ 适配器 + management 服务，默认配置路径可省略 --config）
cargo run --release -- --config config/echo-agent-core.local.toml

# Development
cargo run -- --config config/echo-agent-core.local.toml

# Tests (400+ across all crates; unit + proptest + integration)
cargo test --workspace

# Quality gates (CI enforces all three)
cargo clippy --workspace --all-targets   # zero warnings required
cargo fmt --all --check

# Specific crate
cargo test -p echo-agent
```
