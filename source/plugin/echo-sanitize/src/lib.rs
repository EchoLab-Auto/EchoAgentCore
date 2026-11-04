//! # echo-sanitize — 默认敏感信息脱敏服务
//!
//! [`echo_defs::sanitize::Redactor`] 的默认实现（[`RegistryRedactor`]）：
//!
//! - **字面值注册表**：把运行期登记的秘密值（API key / QQ access_token /
//!   用户追加值）编译为 [Aho-Corasick] 自动机，一次扫描全部命中（多 needle
//!   线性时长，取最左最长匹配）；
//! - **保守模式集**：`sk-…` / `ghp_…` / `AKIA…` / JWT / PEM 私钥头等常见
//!   凭证形态（低误报优先；可 `register_pattern` 追加自定义规则）；
//! - **占位符**：`【已隐藏:<label>】`——labels 不含明文，再扫幂等。
//!
//! # 装配（组合根）
//!
//! 进程内：`RegistryRedactor::with_builtin_patterns()` 交给宿主的
//! `SanitizerHostService`（`core/src/security.rs`），再以**名字** `"sanitizer"`
//! 注册进宿主服务表（`core/src/host_services.rs`）——子进程插件经
//! `HostCall { service = "sanitizer", method = "redact" | "scan" |
//! "register_secret" }` 调用，不共享 `Arc`（跨进程拿不到）。
//!
//! 卡口位置与保证边界见 `document/core/security-redaction.md`。

mod registry;

pub use registry::{placeholder, RegistryRedactor};

pub mod plugin;

