//! 敏感信息隔离 seam（service definition 层）。
//!
//! [`Redactor`] 是"扫描 / 脱敏 / 登记"能力的可替换契约：默认实现位于
//! `echo-sanitize` crate（字面值注册表 + 保守模式集），经 `Ctx` 服务定位器
//! [`Redactor`] 是"扫描 / 脱敏 / 登记"能力的可替换契约：默认实现位于
//! `echo-sanitize` crate（字面值注册表 + 保守模式集），经 `Ctx` 服务定位器
//! 以 `"sanitizer"` 键注册（与 `"llm"` / `"loop"` 同款机制；`echo-sanitize`
//! 另提供强类型 `echo_context::ServiceKey` 常量供消费方解析）。
//! # 定位
//!
//! 脱敏是**出口保证**：允许秘密被读到（同 uid 的 shell 总能读到），但保证
//! 它永不离开本机——工具结果出口、LLM 请求出口、QQ 外发出口三处卡口都调用
//! 本 seam 的实现。谁在什么位置调用见 `document/security-redaction-design.md`。
//!
//! # 实现者约定（硬性）
//!
//! - **绝不 panic**：脱敏运行在关键路径上，任何输入（含并发竞争、锁中毒）
//!   都必须安全返回；
//! - **[`SecretHit`] 不携带明文**：只有标签、类别与位置——审计与日志天然
//!   不泄漏；
//! - **幂等**：`redact(redact(x)) == redact(x)`（占位符本身不得再被识别）；
//! - **无命中快速路径**：`changed == false` 时行为等价于原样返回。

use std::fmt;

/// 一次脱敏的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactReport {
    /// 脱敏后的文本（无命中时 = 原文的副本）。
    pub text: String,
    /// 是否发生了改写（`false` = 与原文逐字节相同）。
    pub changed: bool,
    /// 命中的秘密清单（不含明文）。
    pub hits: Vec<SecretHit>,
}

/// 单个命中记录（安全审计用；**绝不携带明文片段**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretHit {
    /// 命中来源标签，如 `api_key:deepseek` / `pattern:openai_key`。
    pub label: String,
    /// 命中类别。
    pub kind: HitKind,
    /// 在原文本中的字节区间 `[start, end)`。
    pub span: (usize, usize),
}

/// 命中类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// 注册表精确值命中（登记过的秘密；最高优先级）。
    Registered,
    /// 模式命中（内置 / 追加的保守正则）。
    Pattern,
}

/// 模式注册失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// 名称非法（空 / 含空白或保留字符 `:`）。
    InvalidName(String),
    /// 正则语法非法（信息来自实现侧的正则引擎）。
    InvalidRegex(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::InvalidName(name) => write!(f, "非法模式名: {name:?}"),
            PatternError::InvalidRegex(error) => write!(f, "非法正则: {error}"),
        }
    }
}

impl std::error::Error for PatternError {}

/// 敏感信息隔离 seam：扫描 / 脱敏 / 登记。
///
/// 对象安全（`dyn Redactor`）：实现须自带内部可变性（登记是 `&self`）。
pub trait Redactor: Send + Sync {
    /// 实现名（诊断用，如 `"registry"`）。
    fn name(&self) -> &str;

    /// 脱敏：把命中秘密替换为占位符，返回改写文本与命中报告。
    ///
    /// 无命中时 `changed == false` 且 `text` 与输入逐字节相同。
    fn redact(&self, text: &str) -> RedactReport;

    /// 只扫描不修改（外发放行判定 / 审计）。
    fn scan(&self, text: &str) -> Vec<SecretHit>;

    /// 登记一个运行期秘密值（如 API key）。`label` 用于占位符展示。
    ///
    /// 约定：过短的值（实现自行设门槛）与重复登记是安全 no-op。
    fn register_secret(&self, value: &str, label: &str);

    /// 追加检测模式（`regex` 语法；实现可拒绝不支持的语法）。
    fn register_pattern(&self, name: &str, pattern: &str) -> Result<(), PatternError>;

    /// 当前登记条目数（秘密 + 模式；诊断 / 状态展示）。
    fn registered_count(&self) -> usize;
}
