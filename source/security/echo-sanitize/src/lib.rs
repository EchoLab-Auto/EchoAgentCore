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
//! ```no_run
//! # use std::sync::Arc;
//! # use echo_defs::sanitize::Redactor;
//! let redactor: Arc<dyn Redactor> = Arc::new(echo_sanitize::RegistryRedactor::with_builtin_patterns());
//! redactor.register_secret("sk-xxxxxxxxxxxxxxxx", "api_key:deepseek");
//! # let ctx: Arc<echo_context::Ctx> = Arc::new(echo_context::Ctx::default());
//! // 进程级注册（与 "llm" / "loop" 同款）：
//! let _keep = ctx.register::<Arc<dyn Redactor>>("sanitizer", redactor.clone());
//! // 消费方（含进程内插件）：
//! let resolved = ctx.service(&echo_sanitize::REDACTOR);
//! # let _ = resolved;
//! ```
//!
//! 卡口位置与保证边界见 `document/security-redaction-design.md`。

mod registry;

pub use registry::{placeholder, RegistryRedactor};

use echo_context::ServiceKey;
use echo_defs::sanitize::Redactor;
use std::sync::Arc;

/// 服务定位器键（`"sanitizer"`）。
///
/// 注册方：`ctx.register::<Arc<dyn Redactor>>("sanitizer", redactor)`；
/// 消费方：`ctx.service(&echo_sanitize::REDACTOR)` 或
/// `ctx.resolve::<Arc<dyn Redactor>>("sanitizer")`——两者类型必须一致
/// （存储的正是 `Arc<dyn Redactor>`）。
pub static REDACTOR: ServiceKey<Arc<dyn Redactor>> = ServiceKey::new("sanitizer");
