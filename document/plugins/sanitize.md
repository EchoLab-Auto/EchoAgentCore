---
id: sanitize
title: "脱敏（安全不变量）"
group: Core
x: 5760
y: 1200
link: []
---

# 脱敏（`echo-agent.security.sanitize`）

> **定位**：本文写脱敏**作为可插拔单位**长什么样（身份、贡献、宿主接缝、启停、失败模式）。脱敏的算法、卡口位置与威胁模型不在这里——见 [敏感信息隔离](../core/security-redaction.md)。读者：需要替换脱敏实现、或判断"关掉它会发生什么"的开发者。

## 身份

| 项 | 值 |
| --- | --- |
| 插件 id | `echo-agent.security.sanitize` |
| `kind` | `security` |
| 来源 / 运行形态 | 内置（`builtin = true`）/ 进程内 |
| 作用域 | **`Process`**（`KIND_SCOPE`）——人格白名单碰不到它 |
| 实现 crate | `source/plugin/echo-plugin-sanitize` |
| 默认 provider | `echo-sanitize` 的 `RegistryRedactor` |

## 贡献

一个服务：`Ctx["sanitizer"]` = `Arc<dyn echo_defs::sanitize::Redactor>`。

消费方三条，走的是**同一个实例**：

| 消费方 | 怎么拿 |
| --- | --- |
| 进程内插件 | `MountContext.ctx` → `echo_plugin_sanitize::service_key()` |
| 子进程插件 | `HostCall { service = "sanitizer", method = "redact" \| "scan" \| "register_secret" }` → 宿主 `SanitizerHostService` 转调 |
| 宿主自身（卡口） | 组合根持有的 `Arc<dyn Redactor>`（LLM 出口 / 工具出口 / 出站 / 入站） |

## 宿主接缝：秘密表进，脱敏器出

插件不认识 `core.toml`；宿主不认识 `RegistryRedactor`。两者只在**秘密表**这一份值上相遇：

```text
core/src/security.rs::redactor_spec(&cfg)   →  RedactorSpec { secrets, patterns }
                                                  ▲   只有宿主知道"哪些值是秘密"
echo-plugin-sanitize::build(&spec)          →  Arc<dyn Redactor>
                                                  ▲   只有插件知道"怎么脱敏"
```

秘密来源四类（全在宿主侧收集）：`[agent]` 与 `api_profiles` 的 `api_key`、env 回退（`DEEPSEEK_API_KEY` 等）、实例配置里**按键名形态**匹配的凭据字段（`token` / `secret` / `password` / `api_key`——不点名任何平台）、`[security.sanitize].extra_secrets` 与 `extra_patterns`。

## 为什么构造不在 `mount` 里（本插件唯一的特殊性）

| 能力 | 就绪时点 | 理由 |
| --- | --- | --- |
| `loop` / `tools.builtin` / `shell` | `mount` 期 | 消费者发生在挂载之后 |
| **脱敏** | **挂载之前** | 它是 LLM 出口的**装饰器**：`llm` 服务必须以「已脱敏」形态注册进 `Ctx`，而 `echo-plugin-loop` 声明 `llm` 为硬依赖、在挂载期解析。若脱敏器在 `mount` 里构造，`llm` 早已注册了未脱敏的那份，装饰永远追不上 |

于是分工是**实现归插件、时机归宿主**：组合根在启动早期调 `build(&spec)`，插件在 `mount` 里只做**存在性声明**（空 disposer，与 `echo-agent.subagent` 的纯身份插件同款），服务键在 `host_services::provided_keys()` 里静态登记。

> 要把构造搬回 `mount`，前提是先给 `Ctx` 一个可替换的 provide 语义（当前 `provide` 遇同名键即 `Conflict`、`register` 会静默覆盖），并让本插件在挂载顺序里排到 `loop` 之前。这两条没做，所以保持现状。

## 启停与失败模式

- **进目录的意义**：Panel 清单可见、`plugins.toml` 可用 `kind = "builtin"` 行参与组合、进文档门禁、**换实现只需换插件**（宿主只调 `build` 一个函数名）。
- **不做运行期一键开关**：与既有口径一致——关闭只能编辑 `core.toml` 的 `[security.sanitize].enabled = false` 并重启，让停机与告知成为明确动作（同管理面插件不可经命令禁用的理由）。
- **不可被 persona 关掉**：`kind = "security"` → `KIND_SCOPE` 里是 `Process`。安全是不变量，不是某个人的偏好。
- **失败模式**：单条 `extra_patterns` 是坏正则 → `build` 跳过它并 `warn`（启动可用性优先于一条配置错误的完整性）；脱敏器本身无外部依赖，不存在"启动时连不上"。
- **降级可见**：`enabled = false` 时组合根发一条 `warn!`（"敏感信息脱敏已关闭（不推荐）"），不是静默跳过。

## 相关

- 算法、卡口清单、威胁模型、运维排查：[敏感信息隔离](../core/security-redaction.md)
- 插件体系的通用语义（来源 / 运行形态 / 门控）：[插件化设计](./plugins.md)
