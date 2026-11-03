//! # echo-plugin-sanitize — 脱敏服务插件（`echo-agent.security.sanitize`）
//!
//! [`echo_defs::sanitize::Redactor`] 的**插件化装配**：默认实现
//! [`RegistryRedactor`]（`echo-sanitize` crate）在这里被构造出来，宿主不再
//! 认识任何具体实现。
//!
//! ## 为什么需要它（2026-11，与全插件化其它能力对齐）
//!
//! 此前 `core/src/security.rs::build_redactor` 直接
//! `use echo_sanitize::RegistryRedactor`——脱敏是**唯一**一个「有定义层
//! trait、却没有插件绑定」的能力：其余七个接缝（`LlmProvider` / `Tool` /
//! `PluginInstance` / `SessionQuery` / `RemoteDelegationSink` /
//! `RemoteWorkspaceQuery` / `Redactor` 本身）都有对应插件，只有脱敏的实现
//! 长在组合根里。于是"替换脱敏实现"等于"改 Core 源码"。
//!
//! 现在反过来：**构造归本插件，宿主只提供秘密表**。
//!
//! ## 为什么构造不能在 `mount` 里做（与其它插件的关键差别）
//!
//! 这是本 crate 唯一"不像插件"的地方，值得写清楚：
//!
//! | 能力 | 就绪时点 | 为什么 |
//! |---|---|---|
//! | `loop` / `tools.builtin` / `shell` | **mount 期** | 消费者（agent turn、工具调用）发生在挂载之后 |
//! | 脱敏 | **挂载之前** | 它是 **LLM 出口的装饰器**：`llm` 服务必须以「已脱敏」的形态注册进 `Ctx`，而 `echo-plugin-loop` 声明 `llm` 为硬依赖——挂载期才解析。红actor 若在 mount 里构造，`llm` 就先注册了未脱敏的那份，装饰永远追不上 |
//!
//! 于是分工是「**实现归插件、时机归宿主**」：宿主在启动早期调
//! [`build`]，本插件在 `mount` 里做**存在性声明**（与 `echo-agent.subagent`
//! 的纯身份插件同款：`mount` 返回空 disposer，服务键在
//! `host_services::provided_keys()` 里静态登记）。这样换实现 = 换插件，
//! 而不用改装配顺序。
//!
//! > 若将来把「LLM 出口装饰」也做成插件（即 `RedactingProvider` 的装配由
//! > 本插件负责），构造就能搬回 `mount`。前提是先给 `Ctx` 一个**可替换**
//! > 的 provide 语义（当前 `provide` 遇同名键直接 `Conflict`，`register`
//! > 则会静默覆盖）+ 让插件在挂载顺序里排到 `loop` 之前。这条没做，
//! > 所以现在保持"实现归插件"这一半，另一半等语义就绪。
//!
//! ## 边界（安全不变量，不随插件化改变）
//!
//! - **不可被 persona 门控**：`kind = "security"` 在 `KIND_SCOPE` 里登记为
//!   `Process`——安全能力不该由某个人格的勾选决定。
//! - **无运行期一键开关**：与既有口径一致（见
//!   `document/core/security-redaction.md`）——关闭只能编辑 `core.toml`
//!   （`[security.sanitize].enabled = false`）并重启，让停机与告知成为明确动作。
//! - **占位符不含明文**、**命中幂等**、**绝不 panic**：实现约定见
//!   [`echo_defs::sanitize::Redactor`]。

use std::collections::BTreeMap;
use std::sync::Arc;

use echo_defs::sanitize::Redactor;
use echo_plugin::{BuiltinPlugin, Plugin, PluginManifest};

/// 插件 id（宿主 `plugins::SECURITY_SANITIZE_PLUGIN_ID` 与本常量同源字面量）。
pub const PLUGIN_ID: &str = "echo-agent.security.sanitize";

/// 服务键：脱敏器（`Arc<dyn Redactor>`）。
///
/// 与既有服务键同源（`"sanitizer"`）——进程内插件经 `MountContext.ctx`
/// 解析它，子进程插件经 HostCall `service = "sanitizer"` 走同一实例
/// （宿主 `SanitizerHostService` 转调）。
pub const SERVICE_KEY: &str = "sanitizer";

/// 构造参数：**宿主知道"要脱敏哪些值"，插件知道"怎么脱敏"**。
///
/// 这份切分是刻意的——`secrets` 的四类来源（`[agent].api_key`、
/// `[agent.api_profiles.*].api_key`、env 回退、实例配置里的凭据字段）全是
/// **宿主配置结构知识**，插件不该知道 `core.toml` 长什么样；反过来
/// 「Aho-Corasick 最左最长匹配」「占位符形状 `【已隐藏:<label>】`」
/// 「短于 8 字符不登记」是**实现知识**，宿主不该知道。
#[derive(Debug, Clone, Default)]
pub struct RedactorSpec {
    /// 要登记的字面值：`(value, label)`。label 进占位符，不得含明文。
    pub secrets: Vec<(String, String)>,
    /// 追加检测规则：名字 → regex 源串。
    pub patterns: BTreeMap<String, String>,
}

/// 用宿主给的秘密表构造脱敏器（默认实现：`RegistryRedactor`）。
///
/// **组合根在挂载之前调用它**（理由见模块文档）。`spec` 为空也合法——
/// 那只是"没有已知秘密"，模式集（`sk-…` / `ghp_…` / PEM 头等）照常生效。
pub fn build(spec: &RedactorSpec) -> Arc<dyn Redactor> {
    let redactor = echo_sanitize::RegistryRedactor::with_builtin_patterns();
    for (value, label) in &spec.secrets {
        redactor.register_secret(value, label);
    }
    for (name, pattern) in &spec.patterns {
        // 单条坏正则不该让整台 Core 起不来：跳过 + 大声 warn
        // （用户从日志里能看到具体是哪条规则）。
        if let Err(error) = redactor.register_pattern(name, pattern) {
            tracing::warn!(%name, %error, "sanitize: extra pattern rejected, skipping");
        }
    }
    Arc::new(redactor)
}

/// 插件清单（id / 名称 / 类别 / 路由键）。
///
/// `kind = "security"`：`plugins/scope.rs` 的 `KIND_SCOPE` 把它登记为
/// `Process`——安全能力不是人格级开关。
pub fn manifest(version: &str) -> PluginManifest {
    PluginManifest::builtin(
        PLUGIN_ID,
        "脱敏",
        version,
        "security",
        "security",
        "敏感信息隔离：LLM 出口 / 工具出口 / 出站 / 入站的统一脱敏卡口",
    )
}

/// 构造插件：**纯身份**（`mount` 返回空 disposer）。
///
/// 为什么不在这里 `provide` 脱敏器：见模块文档「为什么构造不能在 `mount`
/// 里做」——宿主在挂载之前就必须拿到它来装饰 `llm` 服务，服务键因此由宿主
/// 注册，本插件在 `host_services::provided_keys()` 里静态声明它由自己提供。
///
/// 保留成真插件（而不是像"未外置"那样什么都不做）的理由：它进目录、进
/// Panel 清单、进 `plugins.toml` 的 `kind = "builtin"` 组合、进文档门禁，
/// 并且**换实现只需换插件**（宿主只调 [`build`] 这一个函数名）。
pub fn plugin(version: &str) -> Arc<dyn Plugin> {
    Arc::new(BuiltinPlugin::new(manifest(version), |_ctx| Ok(Vec::new())))
}

/// 单插件族的入口（与 `echo_plugin_loop::plugins` 同形，供组合根统一循环）。
pub fn plugins(version: &str) -> Vec<Arc<dyn Plugin>> {
    vec![plugin(version)]
}

/// `"sanitizer"` 的强类型键（与 [`SERVICE_KEY`] 同源）。
///
/// 消费方一律用它而不是字面量：注册方（组合根）与解析方（进程内插件）
/// 各写一遍字面量时，改名一侧就静默失联。
pub fn service_key() -> echo_context::ServiceKey<Arc<dyn Redactor>> {
    echo_context::ServiceKey::new(SERVICE_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_frozen_configuration_contract() {
        // `plugins.toml` 里写的就是这个字面量；改动即破坏既有部署配置。
        assert_eq!(PLUGIN_ID, "echo-agent.security.sanitize");
        // 服务键与改造前的 `"sanitizer"` 逐字相同：进程内插件与子进程插件的
        // HostCall 都按这个名字寻址，改名 = 静默失联。
        assert_eq!(SERVICE_KEY, "sanitizer");
        assert_eq!(service_key().name(), SERVICE_KEY);
    }

    #[test]
    fn manifest_declares_a_process_scoped_kind() {
        let manifest = manifest("0.0.0-test");
        assert_eq!(manifest.id, PLUGIN_ID);
        // `security` 必须在 KIND_SCOPE 里登记为 Process（core 侧门禁守着，
        // 这里锁住"kind 字符串本身别被随手改名"）。
        assert_eq!(manifest.kind, "security");
    }

    #[test]
    fn build_registers_secrets_and_redacts_them() {
        let spec = RedactorSpec {
            secrets: vec![("sk-abcdefghijklmnop".into(), "api_key".into())],
            patterns: BTreeMap::new(),
        };
        let redactor = build(&spec);
        let report = redactor.redact("key=sk-abcdefghijklmnop end");
        assert!(report.changed, "已登记的秘密必须被替换");
        assert!(!report.text.contains("sk-abcdefghijklmnop"));
        assert!(report.text.contains("api_key"), "label 进占位符: {}", report.text);
    }

    #[test]
    fn build_with_empty_spec_still_applies_builtin_patterns() {
        let redactor = build(&RedactorSpec::default());
        // 内置保守模式集：`sk-` 长串是默认规则，不依赖任何宿主配置。
        let report = redactor.redact("sk-1234567890abcdefghij");
        assert!(report.changed, "空 spec 也必须保留内置模式集");
        assert!(!report.text.contains("1234567890abcdefghij"));
    }

    #[test]
    fn a_broken_extra_pattern_does_not_kill_the_redactor() {
        let spec = RedactorSpec {
            secrets: vec![],
            patterns: BTreeMap::from([("bad".to_string(), "([".to_string())]),
        };
        // 坏正则被跳过（warn），脱敏器照常可用——启动期的可用性优先于
        // 一条配置错误的完整性。
        let redactor = build(&spec);
        assert!(redactor.redact("sk-1234567890abcdefghij").changed);
    }

    #[test]
    fn redaction_is_idempotent() {
        let spec = RedactorSpec {
            secrets: vec![("ghp_abcdefghijklmnop".into(), "gh_token".into())],
            patterns: BTreeMap::new(),
        };
        let redactor = build(&spec);
        let once = redactor.redact("token ghp_abcdefghijklmnop").text;
        let twice = redactor.redact(&once).text;
        assert_eq!(once, twice, "占位符不得被二次替换");
    }

    #[test]
    fn placeholder_never_carries_the_secret() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let spec = RedactorSpec {
            secrets: vec![(secret.into(), "aws".into())],
            patterns: BTreeMap::new(),
        };
        let redactor = build(&spec);
        let report = redactor.redact(secret);
        assert!(!report.text.contains(secret), "占位符泄露了明文: {}", report.text);
        // 审计侧（SecretHit）同样不带明文。
        for hit in redactor.scan(secret) {
            assert!(!hit.label.contains(secret));
        }
    }
}
