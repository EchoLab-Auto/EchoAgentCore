---
id: security-redaction
title: "敏感信息隔离（脱敏服务）"
group: 框架
x: 1840
y: 480
---

# 敏感信息隔离（脱敏服务）

> **定位**：本文描述敏感信息隔离服务（脱敏器）——威胁模型、保证边界、各出口卡口（工具结果 / LLM 请求 / QQ 外发 / 入站落盘）、配置与运维（存量扫描与清理）、插件接入点。读者：框架维护者、插件作者与部署运维者。
> 相关文档：[插件化设计](./core-plugins.md)（服务定位与插件体系）、[工具系统](./core-tools.md)（工具执行出口）、[QQ 适配器门控](./adapter-qq-gating.md)（外发语义）、[插件开发指南](./plugin-authoring.md)（插件侧接入）。

## 威胁模型与保证边界

同机同用户（`uid`）进程可以读配置目录里的一切——这做不到阻止（`bash` 总能 `cat core.toml`）。因此脱敏的**保证放在出口而不是读取面**：

| 保证（做到了） | 非目标（做不到 / 不在射程） |
|---|---|
| 密钥永不进入 LLM 上下文（第三方 API 零暴露） | 阻止 shell 直接读文件内容（同 uid 不可防御） |
| 密钥永不经 QQ 外发（文本拦内容 + 文件拦敏感目录） | 防御绕过本框架的通道（其他进程、网络侧信道、备份导出） |
| 密钥不进**新**会话日志 / 面板事件 / 归档 | 自动擦除**历史**已落盘内容（提供扫描脚本，见「运维」） |
| 未登记密钥的兜底模式扫描（`sk-…` / `ghp_…` / JWT / PEM 等） | 识别任意形态的自定义密钥（用 `register_secret` / 配置补齐） |

核心口径：**值可被读到，但永不离开本机出口。** 历史残留（构建本服务之前已落盘的明文）用 `scripts/scan-secrets.sh` 回扫与清理。

## 架构

三层，复用框架既有的 dsh 式接缝模式（与 `"llm"` / `"loop"` 服务同款）：

```
echo-defs::sanitize          契约：Redactor trait（redact / scan / register）
      ▲                       零实现、零依赖；SecretHit 不携带明文（审计安全）
echo-sanitize（crate）       默认实现：RegistryRedactor
      ▲                       字面值注册表（Aho-Corasick 最左最长）+ 保守模式集
core 组合根                  构建 + 注入 + 服务注册（"sanitizer" 键）
      ▲                       见 source/core/src/security.rs 与 main.rs
卡口                         工具出口 / LLM 出口 / QQ 外发 / 入站 / 落盘
```

- **契约**（`source/defs/echo-defs/src/sanitize.rs`）：`Redactor` 为对象安全 trait；实现约定「绝不 panic、命中不含明文、幂等（占位符不再被识别）」。占位符格式 `【已隐藏:<label>】`。
- **默认实现**（`source/security/echo-sanitize/`）：字面值进 Aho-Corasick 自动机（一次扫描全部命中），模式集用 `regex`；登记须 ≥8 字符且不含占位符括号（防空转与误报）；label 净化防注入。
- **服务定位**：组合根经 `Ctx::register::<Arc<dyn Redactor>>("sanitizer", …)` 注册（`echo_sanitize::REDACTOR` 为强类型 `ServiceKey`）；进程内插件在 `MountContext.ctx` 里解析（见「插件接入」）。
- **无运行期一键开关**：脱敏器是安全不变量（同管理面插件不可经命令禁用的口径）——关闭只能编辑 `core.toml` 并重启，让停机与告知成为明确动作。

## 出口卡口（调用点）

| # | 卡口 | 位置 | 覆盖 |
|---|---|---|---|
| 1 | 工具结果出口 | `ToolRegistry::execute` / `execute_rich` 出口（结果文本与错误消息） | 一切工具：内置 / 插件 / 远程 / **子代理直呼** / 联邦路由 |
| 2 | LLM 请求出口 | `llm::wrap_redacting`（装饰 provider，请求侧全文扫描） | 全部 LLM 出口：内置循环 / echo-loop / 子代理 / 等待回复 |
| 3 | QQ 外发出口 | `core::qq_tools` 出站闸门（`send_*`） | 文本命中按策略阻断/替换（`block` 默认 / `redact`）；`file_path`/`file`/`image` 落在敏感目录直接拒发 |
| 4 | 入站与落盘 | `Agent::process_message` / `process_inbound_branch` / `record_incoming_and_snapshot` / `run_tool` 参数 | 用户与 QQ 输入、工具调用参数（事件与会话日志）、工具结果事件 |
| 5 | 密钥登记 | 启动构建（`core::security::build_redactor`）+ `Agent::rebuild_provider` 热同步 | 顶层与 profile 的 `api_key`、env 回退、QQ `access_token`、用户追加值 |

设计取舍：

- **卡口放在注册表出口而非 `ToolPipeline` 中间件**：pipeline 仅 echo-loop 路径使用（生产零中间件），注册表出口覆盖全部执行路径（含子代理直呼与联邦）。
- **双保险**：`run_tool` 在编排工具（如 `spawn_subagent` 结论）路径再脱敏一次；幂等保证重复扫描无副作用。
- **请求侧脱敏不处理响应侧**：上下文已被保证干净，模型不可能复述未见的秘密；流式 chunk 逐段扫描会破坏跨 chunk 的值，故明确不做。
- **QQ 出站闸门用「文本全扫 + 文件路径拒发」**：`block` 时不回显任何片段（错误只说命中处数）；`redact` 时占位符替换后照发。

## 配置

```toml
[security.sanitize]
enabled = true            # 缺省开启；关闭仅能改文件 + 重启（安全不变量）
outbound = "block"        # QQ 出站命中策略：block（默认）| redact
extra_secrets = []        # 追加精确值（label 自动编号 extra:<序号>）
[security.sanitize.extra_patterns]
# 名字 → regex，命中替换为【已隐藏:pattern:<名字>】
# corp_token = "corp-[A-Z0-9]{8}"
```

配置在 `core.toml` 的 `[security]` 节；`ConfigStore` 的读改写不丢节（整文档 Table 语义）。运行期热更新：API 配置保存（`UpdateApiConfig`）后新密钥立即登记并重建 provider 包装。

## 运维

- **审计口径**：静默替换走 `tracing::debug`（每次登记）与工具结果命中的 `tracing::warn`（带 session / tool / 命中部数，**不含明文**）；QQ 出站阻断 `tracing::warn` 且工具返回失败结果（面板经工具结果可见）。不额外发明事件类型——避免每次脱敏都弹面板通知。
- **存量扫描**：`scripts/scan-secrets.sh [配置目录]`
  - 从 `core.toml` 提取已知密钥（含 `extra_secrets`；tomllib，缺失时正则回退）；
  - 回扫配置目录全部文件（跳过 `core.toml` 本体），只报 `文件 / label / 次数`，**绝不打印明文**；退出码 1 = 有残留；
  - `--fix`：**停 Core 后**离线运行，把残留替换为 `【已隐藏:<label>】`（严格 UTF-8 才改写，非 UTF-8 跳过）。
  - 残留视同泄漏：先在供应商侧轮换密钥，再清理文件。
- **部署建议**：core.toml 权限 0600（本机自更新脚本已按此安装）；Panel 管理端不在本机网络裸绑（见部署文档）。

## 插件接入与测试

**进程内插件**（`echo-plugin` 体系）：

```rust
// 挂载时经 MountContext 拿到的 Ctx 解析（与 "llm" / "loop" 同款）：
if let Some(redactor) = ctx.service(&echo_sanitize::REDACTOR) {
    let clean = redactor.redact(user_text).text;   // 或 scan() 只检查
}
```

**子进程插件**（stdio 协议）：插件经 `HostCall` 回呼宿主服务（`service = "sanitizer"`，方法 `redact` / `scan` / `register_secret`），宿主在 `PluginSupervisor.host_services()` 注册表路由（`echo-plugin-host/src/host_service.rs`；SDK 侧 `HostClient` + `PluginHandler::on_ready` 获取句柄）。协议新增变体不改变版本号（只增语义）。

**测试不变量**（改脱敏相关代码必须保持）：

- `echo-sanitize` 单测：字面值 / 模式 / 幂等 / 占位符不二次命中 / 短值忽略；
- `echo-agent`：工具出口（结果与错误）脱敏、请求侧永不泄漏（RecordingProvider 断言）；
- `core`：QQ 闸门（block 阻断不回显 / redact 替换 / 敏感目录拒发）、配置构建登记；
- 协议回呼：`echo-plugin-host/tests/host_call.rs`（HostCall 路由 / 未知服务错误）与 SDK 侧 `HostClient` 往返 / 超时测试（`echo-plugin-sdk`）；
- 端到端演练（`echo-agent`：`run_tool_redacts_secrets_from_real_bash_output_and_log`）：真实 bash 读含密钥文件 → 工具结果与会话事件日志零明文、参数日志记占位符而执行不受影响；QQ 零出站由出站闸门测试覆盖。
