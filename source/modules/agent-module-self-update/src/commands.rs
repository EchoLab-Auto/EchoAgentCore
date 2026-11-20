//! 自更新命令域：`RequestSelfUpdate` / `RequestSelfUpdateStatus`。
//!
//! ## 为什么这两条命令归模块
//!
//! 它们此前住在组合根 `federation_rt.rs`（约 120 行）：触发
//! `echo-agent-core-update.service`、读 `~/.local/state/echo-agent-core/update-status`。
//! 那是**部署形状**——systemd 单元名、状态文件的 `key=value` 字段、什么算
//! "进行中"。这些东西换一个部署方式（容器 / 别的 init 系统）就该换掉，正是
//! 模块该有的样子；而组合根本不该知道"updater 单元叫什么"。
//!
//! ## 顺手修掉的一个死路
//!
//! 两条命令此前在 `HOST_RESERVED_COMMANDS` 里（"模块不得接管的自我管理面"），
//! 于是命令泵**跳过模块分发**、落进宿主的兜底分支，而兜底分支的文案是
//! "联邦命令由 echo-agent.federation 模块处理，但该模块未启用"——**即使联邦
//! 模块正常运行**。实现（`ports.command_handler` → `handle_federation_command`）
//! 只有经模块分发才走得到，两条命令因此必失败。
//!
//! 现在它们不 reserved：本模块是内置模块、启动期即挂载，认领这两条命令；
//! 未挂载时宿主兜底会明确说"自更新模块未启用"。
//!
//! ## 与联邦的独立性
//!
//! 自更新曾与联邦命令共用入口，于是"联邦关闭"会连带把自更新报成"联邦未
//! 启用"。现在两件事在**两个模块**里，互不牵连。
//!
//! ## 接口可靠性（2026-11）
//!
//! - **状态路径以部署为准**：`ECHO_UPDATE_STATUS_FILE`（`install.sh` 渲染进
//!   Core 单元）是唯一可信来源，绝对路径优先。取值相对/非法时**不猜**：
//!   [`status_path_checked`] 返回错误，`read_status` 发 `Error` 事件——
//!   相对路径随进程 CWD 漂移，静默读它等于读另一个文件。
//! - **读失败不冒充空闲**：只有"文件不存在"才是 `idle`；权限 / IO /
//!   文件缺 `state` 都发 `Error`。把权限错误装成 `idle` 会让面板以为
//!   "从没更新过"，用户再点一次只会得到同一个错误。
//! - **触发不阻塞**：`systemctl start --no-block` 只把 job 排进 manager，
//!   不等几分钟的构建（oneshot 的 `start` 默认会等 job 结束）。
//! - **重入判定看 `ActiveState`**：oneshot 执行期间是 `activating`，
//!   `is-active --quiet` 对它返回 3（非 0），旧实现因此漏判、能重复启动
//!   第二个 updater。改用 `systemctl show -p ActiveState` 的固定调用。
//! - **工具命令短超时**：systemctl 卡死（manager 无响应）时不能把命令泵
//!   一起拖住；超时只覆盖"查询/投递"，不覆盖 updater 的构建。
//!
//! ## 新实例部署（2026-12）
//!
//! `tools/echo-deploy` 把单元渲染成 `echo-agent-<instance>-core.service` /
//! `echo-agent-<instance>-update.service`，并向 Core 注入
//! `ECHO_AGENT_INSTANCE` / `ECHO_AGENT_SCOPE` / `ECHO_AGENT_STATE_DIR`
//! （`write_common_environment`）。本模块据此自动推导：
//!
//! - updater 单元 = `echo-agent-<instance>-update.service`；
//! - 作用域 = `ECHO_AGENT_SCOPE`（旧的 `ECHO_SERVICE_SCOPE` 仍优先）；
//! - 状态 = `<state_dir>/transaction-<instance>.json`（部署工具的
//!   `TransactionStore` 布局，真实 `Phase` schema）。
//!
//! 事务状态只投影 `phase` / `error.code` / `error.step` 与 release id；
//! `error.message`、事务 id、哈希、step detail 不进事件（可能含后端输出或
//! 凭据，面板也用不到）。旧的 `ECHO_UPDATE_STATUS_FILE` 布局原样保留：
//! 没有新部署变量时行为不变。

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_module_management::protocol::{BackendCommand, BackendEvent};

/// 本模块处理的命令名。
pub const COMMANDS: [&str; 2] = ["RequestSelfUpdate", "RequestSelfUpdateStatus"];

/// 旧默认 updater 单元名（`install.sh` 布局）。
///
/// 仅在既没有 `ECHO_UPDATE_UNIT`、也没有 `ECHO_AGENT_INSTANCE` 时使用；
/// 新实例部署由 [`update_unit_name`] 推导
/// `echo-agent-<instance>-update.service`。
pub const UPDATE_UNIT: &str = "echo-agent-core-update.service";

/// 显式 updater 单元名覆盖（必须是受限安全的 `.service` 单元名）。
///
/// 新实例部署的单元名是 `echo-agent-<instance>-update.service`；通常由
/// [`AGENT_INSTANCE_ENV`] 自动推导，本变量用于非常规布局。
pub const UPDATE_UNIT_ENV: &str = "ECHO_UPDATE_UNIT";

/// 新部署工具（`tools/echo-deploy`）渲染进 Core 单元的实例标识。
pub const AGENT_INSTANCE_ENV: &str = "ECHO_AGENT_INSTANCE";

/// 新部署工具渲染进单元的作用域（`user` / `system`）。
pub const AGENT_SCOPE_ENV: &str = "ECHO_AGENT_SCOPE";

/// 新部署工具渲染进单元的部署状态目录（事务文件所在目录）。
pub const AGENT_STATE_DIR_ENV: &str = "ECHO_AGENT_STATE_DIR";

/// 旧部署脚本（`scripts/install.sh`）渲染进单元的作用域。
pub const SERVICE_SCOPE_ENV: &str = "ECHO_SERVICE_SCOPE";

/// 状态文件路径覆盖：`install.sh` 渲染进 Core 单元的绝对路径。
///
/// 这是**旧部署布局的唯一可信来源**：`XDG_STATE_HOME` / `HOME` 在不同启动
/// 方式下可能指向别处（system 作用域、`sudo`、容器），而单元里注入的值正是
/// `update.sh --status` 实际写入的那个文件。
pub const UPDATE_STATUS_FILE_ENV: &str = "ECHO_UPDATE_STATUS_FILE";

/// 新部署工具事务文件的显式路径覆盖（绝对路径）。
///
/// 未设置时由 [`AGENT_STATE_DIR_ENV`] + [`AGENT_INSTANCE_ENV`] 推导为
/// `<state_dir>/transaction-<instance_id>.json`（`echo-deploy` 的
/// `TransactionStore` 布局）；两者也没有时继续读旧的 update-status 文件。
pub const UPDATE_TRANSACTION_FILE_ENV: &str = "ECHO_UPDATE_TRANSACTION_FILE";

/// systemd 单元名上限（systemd 的 `UNIT_NAME_MAX`）。
const MAX_UNIT_NAME_BYTES: usize = 255;

/// 实例标识上限，与 `echo-deploy::model::safe_id` 一致。
const MAX_INSTANCE_ID_BYTES: usize = 100;

/// 事务文档上限，与 `echo-deploy::transaction` 一致（1 MiB）。
const MAX_TRANSACTION_BYTES: u64 = 1024 * 1024;

/// 单条 systemctl 调用的上限：只覆盖"查询/投递"，不覆盖 updater 的构建。
///
/// `start --no-block` 只等 job 入队，正常在毫秒级返回；5s 是 manager
/// 卡死（D-Bus 无响应）时的止损线。
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// `systemctl show` 的固定参数：只查 `ActiveState`，输出裸值。
///
/// 不用 `is-active --quiet`：oneshot 执行期间的状态是 `activating`，它对
/// `--quiet` 返回 3（非 0），与 `inactive` 无法区分——正是重复启动的根因。
fn active_state_args(unit: &str) -> [&str; 5] {
    ["show", "-p", "ActiveState", "--value", unit]
}

/// `systemctl start` 的固定参数：`--no-block` 是"不阻塞等构建"的关键。
fn start_args(unit: &str) -> [&str; 3] {
    ["start", "--no-block", unit]
}

/// systemd 作用域（部署形状）：决定用 `systemctl` 还是 `systemctl --user`。
///
/// 这两条命令连的是**两套互不可见的 manager**：单元装在
/// `~/.config/systemd/user` 就归当前用户的 `systemd --user` 管，装在
/// `/etc/systemd/system` 就归 PID 1 管。敲错不会报"权限不足"，而是
/// `Unit ... not found` ——在自更新这里表现为"按钮点了只弹一个错"，服务
/// 完全没动。
///
/// 取值来源：旧部署的 `ECHO_SERVICE_SCOPE`（`install.sh`），或新部署的
/// `ECHO_AGENT_SCOPE`（`echo-deploy` 的 `write_common_environment`）。旧的
/// 优先；**不用运行时探测**：两套单元同时存在（旧 system 装法残留 + 新
/// user 装法）时探测必然猜错一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceScope {
    /// 用户管理器（`systemctl --user`），install.sh 的历史默认作用域。
    User,
    /// 系统管理器（裸 `systemctl`），单元装在 `/etc/systemd/system`。
    System,
}

impl ServiceScope {
    /// 读环境：`ECHO_SERVICE_SCOPE` 优先，其次 `ECHO_AGENT_SCOPE`；都没有或
    /// 取值不认识时退回 `User`（install.sh 的历史默认作用域）。
    pub fn from_env() -> Self {
        let legacy = std::env::var(SERVICE_SCOPE_ENV).ok();
        let instance = std::env::var(AGENT_SCOPE_ENV).ok();
        resolve_service_scope(legacy.as_deref(), instance.as_deref())
    }

    /// 该作用域下 systemctl 需要的前缀参数。
    pub fn systemctl_scope_args(self) -> &'static [&'static str] {
        match self {
            Self::User => &["--user"],
            Self::System => &[],
        }
    }

    /// 错误文案里的作用域名（与 `--user` 的取舍同源）。
    fn describe(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

/// 纯解析作用域（测试可并行调用，不读进程环境）。
fn resolve_service_scope(legacy: Option<&str>, instance: Option<&str>) -> ServiceScope {
    match legacy {
        Some("system") => ServiceScope::System,
        Some("user") => ServiceScope::User,
        _ => match instance {
            Some("system") => ServiceScope::System,
            Some("user") => ServiceScope::User,
            _ => ServiceScope::User,
        },
    }
}

/// updater 单元名取值非法（显式配置错误**不**回退，避免启动另一个单元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateUnitError {
    /// 某个环境变量不可用。
    InvalidEnv {
        /// 变量名。
        name: &'static str,
        /// 原始取值（非 UTF-8 用 lossy 形式，仅用于报错）。
        value: String,
        /// 人类可读的原因。
        reason: &'static str,
    },
}

impl std::fmt::Display for UpdateUnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnv {
                name,
                value,
                reason,
            } => write!(
                f,
                "{name}={value:?} 不可用：{reason}；拒绝回退到旧单元\
                 （显式配置错误时静默启动另一个单元更危险）"
            ),
        }
    }
}

impl std::error::Error for UpdateUnitError {}

/// 校验实例标识，规则与 `echo-deploy::model::safe_id` 完全一致：
/// 1..=100 字节、首字符字母/数字、其余 `[A-Za-z0-9._-]`。
fn validate_instance_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("为空");
    }
    if value.len() > MAX_INSTANCE_ID_BYTES {
        return Err("过长（上限 100 字节）");
    }
    if !value.as_bytes()[0].is_ascii_alphanumeric() {
        return Err("必须以字母或数字开头");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("只能包含字母、数字、点、下划线、连字符");
    }
    Ok(())
}

/// 受限安全 systemd 单元名：必须是 `.service`、首字符字母/数字、其余
/// `[A-Za-z0-9._-]`、不含 `..`、长度不超过 255 字节。
///
/// 这排除了 `/`（路径）、空白、换行、`@`/`:` 与以 `-` 开头的名字；后者会被
/// `systemctl` 当成选项。updater 只需要普通实例单元名，不需要模板/路径单元。
fn validate_update_unit_name(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("为空");
    }
    if value.len() > MAX_UNIT_NAME_BYTES {
        return Err("过长（上限 255 字节）");
    }
    let Some(stem) = value.strip_suffix(".service") else {
        return Err("必须以 .service 结尾");
    };
    if stem.is_empty() {
        return Err("缺少单元名前缀");
    }
    if !value.as_bytes()[0].is_ascii_alphanumeric() {
        return Err("必须以字母或数字开头（避免被 systemctl 当作选项）");
    }
    if value.contains("..") {
        return Err("不允许 `..`");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("只能包含字母、数字、点、下划线、连字符");
    }
    Ok(())
}

fn env_utf8(value: &OsStr, name: &'static str) -> Result<String, UpdateUnitError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| UpdateUnitError::InvalidEnv {
            name,
            value: value.to_string_lossy().into_owned(),
            reason: "不是 UTF-8 字符串",
        })
}

/// 纯解析 updater 单元名：显式 `ECHO_UPDATE_UNIT` → `ECHO_AGENT_INSTANCE`
/// 推导 → 旧常量。显式取值非法时报错，**不**静默回退。
fn resolve_update_unit_from(
    explicit: Option<&OsStr>,
    instance: Option<&OsStr>,
) -> Result<String, UpdateUnitError> {
    if let Some(raw) = explicit {
        let unit = env_utf8(raw, UPDATE_UNIT_ENV)?;
        validate_update_unit_name(&unit).map_err(|reason| UpdateUnitError::InvalidEnv {
            name: UPDATE_UNIT_ENV,
            value: unit.clone(),
            reason,
        })?;
        return Ok(unit);
    }
    if let Some(raw) = instance {
        let instance = env_utf8(raw, AGENT_INSTANCE_ENV)?;
        validate_instance_id(&instance).map_err(|reason| UpdateUnitError::InvalidEnv {
            name: AGENT_INSTANCE_ENV,
            value: instance.clone(),
            reason,
        })?;
        let unit = format!("echo-agent-{instance}-update.service");
        debug_assert!(
            validate_update_unit_name(&unit).is_ok(),
            "safe instance id must produce a safe unit name"
        );
        return Ok(unit);
    }
    Ok(UPDATE_UNIT.to_string())
}

/// 读环境解析 updater 单元名（新实例部署自动推导；旧部署保持 legacy）。
pub fn update_unit_name() -> Result<String, UpdateUnitError> {
    resolve_update_unit_from(
        std::env::var_os(UPDATE_UNIT_ENV).as_deref(),
        std::env::var_os(AGENT_INSTANCE_ENV).as_deref(),
    )
}

/// 宿主接缝：本域只需要一个事件出口。
///
/// 刻意**不**给"读配置 / 拿句柄"之类的能力——自更新的全部知识（单元名、
/// 状态文件、重入判定）都在本模块里。
pub trait SelfUpdateHost: Send + Sync {
    /// 事件出口（进程级汇聚点：任何人格发都到同一处）。
    fn emit(&self, event: BackendEvent);
}

/// 状态文件路径无法确定（或取值非法）。
///
/// 存在的意义是**不静默误读**：旧实现无条件按 `XDG_STATE_HOME` / `HOME`
/// 拼路径，单元注入的 `ECHO_UPDATE_STATUS_FILE` 被忽略；两者不一致时
/// 面板读的是另一个文件，表现为"更新中"永远不出现。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusPathError {
    /// `ECHO_UPDATE_STATUS_FILE` 不可用（空 / 相对 / 含 NUL / 非 UTF-8）。
    InvalidOverride {
        /// 原始取值（非 UTF-8 时用 lossy 形式，仅用于报错）。
        value: String,
        /// 人类可读的原因。
        reason: &'static str,
    },
    /// `XDG_STATE_HOME` 不可用（相对 / 含 NUL / 非 UTF-8）。
    InvalidStateHome {
        /// 原始取值（非 UTF-8 时用 lossy 形式，仅用于报错）。
        value: String,
        /// 人类可读的原因。
        reason: &'static str,
    },
    /// 既没有合法的 `XDG_STATE_HOME`，也没有合法的 `HOME`。
    NoHome {
        /// 人类可读的原因（缺哪个变量 / 哪个取值非法）。
        reason: &'static str,
    },
}

impl std::fmt::Display for StatusPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOverride { value, reason } => write!(
                f,
                "{UPDATE_STATUS_FILE_ENV}={value:?} 不可用：{reason}；必须是绝对路径\
                 （相对路径会随进程工作目录漂移，宁可不读）"
            ),
            Self::InvalidStateHome { value, reason } => write!(
                f,
                "XDG_STATE_HOME={value:?} 不可用：{reason}；XDG 规范要求它是绝对路径"
            ),
            Self::NoHome { reason } => write!(f, "无法定位自更新状态目录：{reason}"),
        }
    }
}

impl std::error::Error for StatusPathError {}

/// 自更新状态文件路径（与 `install.sh` / `update.sh` 布局一致）。
///
/// 优先级：合法的 `ECHO_UPDATE_STATUS_FILE`（绝对路径）→ `XDG_STATE_HOME`
/// → `$HOME/.local/state`。
///
/// 返回值**只做兼容/展示**：本函数没有错误通道，取值非法时会退回历史默认
/// 布局，但**不会**把相对路径当成可读路径交给调用方。要错误感知（读状态
/// 用的就是它）请用 [`status_path_checked`]。
pub fn status_path() -> PathBuf {
    status_path_checked().unwrap_or_else(|_| legacy_status_path())
}

/// 自更新状态文件路径（带校验）。
///
/// 与 [`status_path`] 同样的优先级，但相对 / 空 / 非 UTF-8 / 含 NUL 的
/// `ECHO_UPDATE_STATUS_FILE` 会返回 [`StatusPathError`]，而不是静默退到
/// 另一个目录——那会把"路径配错"伪装成"服务没有状态"。
pub fn status_path_checked() -> Result<PathBuf, StatusPathError> {
    let override_value = std::env::var_os(UPDATE_STATUS_FILE_ENV);
    let state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    resolve_status_path_from(
        override_value.as_deref(),
        state_home.as_deref(),
        home.as_deref(),
    )
}

/// 状态来源：新部署工具的事务 JSON，或旧的 `update-status` 文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusSource {
    /// `echo-deploy` 的 `<state_dir>/transaction-<instance>.json`
    /// （真实 `Phase` schema；由 [`read_transaction_status_at`] 解析）。
    Transaction(PathBuf),
    /// 旧的 `update-status`（`key=value`；由 [`read_status_at`] 解析）。
    Legacy(PathBuf),
}

/// 状态来源解析失败。
///
/// 和 [`StatusPathError`] 一样**不回退**：显式配置了新布局却写错时，静默去
/// 读旧文件会把"路径配错"伪装成"从未更新过"。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusSourceError {
    /// 旧布局路径错误（`ECHO_UPDATE_STATUS_FILE` / XDG / HOME）。
    Legacy(StatusPathError),
    /// 新布局的某个环境变量不可用。
    InvalidEnv {
        /// 变量名。
        name: &'static str,
        /// 原始取值（非 UTF-8 时用 lossy 形式，仅用于报错）。
        value: String,
        /// 人类可读的原因。
        reason: &'static str,
    },
}

impl std::fmt::Display for StatusSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy(error) => write!(f, "{error}"),
            Self::InvalidEnv {
                name,
                value,
                reason,
            } => write!(
                f,
                "{name}={value:?} 不可用：{reason}；必须是绝对路径\
                 （相对路径会随进程工作目录漂移，宁可不读）"
            ),
        }
    }
}

impl std::error::Error for StatusSourceError {}

/// 纯解析事务文件路径。
///
/// 优先级：
/// 1. `ECHO_UPDATE_TRANSACTION_FILE`（显式绝对路径）；
/// 2. `ECHO_AGENT_STATE_DIR` + `ECHO_AGENT_INSTANCE` →
///    `<state_dir>/transaction-<instance>.json`（`echo-deploy` 布局）；
/// 3. 两者都没有 → `None`（调用方继续用旧 `update-status`）。
///
/// 只设置了一半（有 state_dir 没 instance，或反之）按配置错误处理：新布局
/// 已声明但无法定位文件，不能退回旧文件。
fn transaction_file_from(
    explicit: Option<&OsStr>,
    agent_state_dir: Option<&OsStr>,
    agent_instance: Option<&OsStr>,
) -> Result<Option<PathBuf>, StatusSourceError> {
    if let Some(raw) = explicit {
        let path = absolute_path(raw).map_err(|(reason, value)| StatusSourceError::InvalidEnv {
            name: UPDATE_TRANSACTION_FILE_ENV,
            value,
            reason,
        })?;
        return Ok(Some(path));
    }
    let Some(raw_dir) = agent_state_dir else {
        return Ok(None);
    };
    let state_dir =
        absolute_path(raw_dir).map_err(|(reason, value)| StatusSourceError::InvalidEnv {
            name: AGENT_STATE_DIR_ENV,
            value,
            reason,
        })?;
    let Some(raw_instance) = agent_instance else {
        return Err(StatusSourceError::InvalidEnv {
            name: AGENT_INSTANCE_ENV,
            value: String::new(),
            reason: "未设置（已设置 ECHO_AGENT_STATE_DIR，但无法定位事务文件）",
        });
    };
    let instance = raw_instance
        .to_str()
        .ok_or_else(|| StatusSourceError::InvalidEnv {
            name: AGENT_INSTANCE_ENV,
            value: raw_instance.to_string_lossy().into_owned(),
            reason: "不是 UTF-8 字符串",
        })?;
    validate_instance_id(instance).map_err(|reason| StatusSourceError::InvalidEnv {
        name: AGENT_INSTANCE_ENV,
        value: instance.to_owned(),
        reason,
    })?;
    Ok(Some(state_dir.join(format!("transaction-{instance}.json"))))
}

/// 纯解析状态来源（测试可并行调用，不读进程环境）。
fn status_source_from(
    transaction_override: Option<&OsStr>,
    agent_state_dir: Option<&OsStr>,
    agent_instance: Option<&OsStr>,
    status_override: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<StatusSource, StatusSourceError> {
    if let Some(path) =
        transaction_file_from(transaction_override, agent_state_dir, agent_instance)?
    {
        return Ok(StatusSource::Transaction(path));
    }
    resolve_status_path_from(status_override, xdg_state_home, home)
        .map(StatusSource::Legacy)
        .map_err(StatusSourceError::Legacy)
}

/// 读环境解析状态来源。
///
/// 新部署（`echo-deploy`）注入的 `ECHO_AGENT_STATE_DIR` / `ECHO_AGENT_INSTANCE`
/// 会优先走事务 JSON；旧部署（只有 `ECHO_UPDATE_STATUS_FILE`）继续走
/// `update-status`，行为不变。
pub fn status_source_checked() -> Result<StatusSource, StatusSourceError> {
    let transaction = std::env::var_os(UPDATE_TRANSACTION_FILE_ENV);
    let agent_state_dir = std::env::var_os(AGENT_STATE_DIR_ENV);
    let agent_instance = std::env::var_os(AGENT_INSTANCE_ENV);
    let status_override = std::env::var_os(UPDATE_STATUS_FILE_ENV);
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    status_source_from(
        transaction.as_deref(),
        agent_state_dir.as_deref(),
        agent_instance.as_deref(),
        status_override.as_deref(),
        xdg_state_home.as_deref(),
        home.as_deref(),
    )
}
/// 纯解析：给定三个环境取值 → 状态文件路径。
///
/// 不读进程环境，测试可并行调用（`set_var` 是进程级副作用，会与其它测试
/// 互相覆盖——2026-10 偶发失败的根因）。
fn resolve_status_path_from(
    override_value: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, StatusPathError> {
    // 部署注入的绝对路径优先：它必须与 update.sh --status 写入的文件一致。
    if let Some(raw) = override_value {
        return validate_override(raw);
    }
    let state_home = match xdg_state_home {
        Some(raw) if !raw.is_empty() => validate_state_home(raw)?,
        _ => match home {
            Some(raw) => validate_home(raw)?.join(".local/state"),
            None => {
                return Err(StatusPathError::NoHome {
                    reason: "HOME 与 XDG_STATE_HOME 均未设置",
                })
            }
        },
    };
    Ok(state_home.join("echo-agent-core/update-status"))
}

/// 历史默认布局：只给无错误通道的 [`status_path`] 兜底。
fn legacy_status_path() -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        });
    state_home.join("echo-agent-core/update-status")
}

/// 校验绝对路径取值；调用方把它映射到自己的错误变体。
fn absolute_path(value: &OsStr) -> Result<PathBuf, (&'static str, String)> {
    let text = value
        .to_str()
        .ok_or_else(|| ("不是 UTF-8 路径", value.to_string_lossy().into_owned()))?;
    if text.is_empty() {
        return Err(("为空", String::new()));
    }
    if text.contains('\0') {
        return Err(("含 NUL 字节", text.to_string()));
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(("不是绝对路径", text.to_string()));
    }
    Ok(path.to_path_buf())
}

fn validate_override(value: &OsStr) -> Result<PathBuf, StatusPathError> {
    absolute_path(value)
        .map_err(|(reason, value)| StatusPathError::InvalidOverride { value, reason })
}

fn validate_state_home(value: &OsStr) -> Result<PathBuf, StatusPathError> {
    absolute_path(value)
        .map_err(|(reason, value)| StatusPathError::InvalidStateHome { value, reason })
}

fn validate_home(value: &OsStr) -> Result<PathBuf, StatusPathError> {
    absolute_path(value).map_err(|(reason, _value)| StatusPathError::NoHome {
        reason: match reason {
            "为空" => "HOME 为空",
            "不是绝对路径" => "HOME 不是绝对路径",
            "含 NUL 字节" => "HOME 含 NUL 字节",
            _ => "HOME 不是 UTF-8 路径",
        },
    })
}

/// 状态文件的字段投影（解析成功后的中间形态）。
#[derive(Debug, Default, PartialEq, Eq)]
struct StatusFields {
    state: String,
    phase: Option<String>,
    revision: Option<String>,
    message: Option<String>,
    updated_at: Option<String>,
}

/// 状态文件格式错误（文件存在 ≠ 状态可用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusParseError {
    /// 没有 `state=` 行，或值为空。
    MissingState,
}

impl std::fmt::Display for StatusParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingState => write!(f, "缺少非空的 state 字段"),
        }
    }
}

/// 纯解析：`key=value` 行 → 字段投影。
///
/// 只把"有文件但缺 state"判为错误；空行与不含 `=` 的行忽略（update.sh
/// 的格式是每行一个 `key=value`，额外内容不该让整次查询失败）。
fn parse_status(text: &str) -> Result<StatusFields, StatusParseError> {
    let mut fields = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        fields.insert(key.to_string(), value.trim().to_string());
    }
    let state = fields.remove("state").unwrap_or_default();
    if state.is_empty() {
        return Err(StatusParseError::MissingState);
    }
    let take = |name: &str, fields: &mut HashMap<String, String>| {
        fields.remove(name).filter(|value| !value.is_empty())
    };
    Ok(StatusFields {
        state,
        phase: take("phase", &mut fields),
        revision: take("revision", &mut fields),
        message: take("message", &mut fields),
        updated_at: take("updated_at", &mut fields),
    })
}

// ── 新部署工具事务 JSON（真实 `Phase` schema）──

/// `echo-deploy::transaction::Phase` 的已知取值（serde `snake_case`）。
const TRANSACTION_PHASES: [&str; 9] = [
    "prepared",
    "maintenance",
    "drained",
    "switching",
    "verifying",
    "committed",
    "rolling_back",
    "rolled_back",
    "recovery_required",
];

/// 事务文档里对状态有用的字段投影。
///
/// 刻意**不**保存 `error.message` / `id` / `manifest_sha256` / step detail：
/// 这些可能含后端输出或凭据，事件与日志只需要稳定的 `phase` / `error.code`。
#[derive(Debug, Default, PartialEq, Eq)]
struct TransactionStatus {
    phase: String,
    target_release: Option<String>,
    previous_release: Option<String>,
    error_code: Option<String>,
    error_step: Option<String>,
    updated_at: Option<String>,
}

/// 事务文档格式错误（文件存在 ≠ 状态可用）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum TransactionParseError {
    /// 不是合法 JSON（只带解析器的位置描述，不携带原文）。
    InvalidJson(String),
    /// 顶层不是 JSON 对象。
    NotObject,
    /// `schema_version` 不是受支持的 1。
    UnsupportedSchema,
    /// 缺少非空 `phase`。
    MissingPhase,
    /// `phase` 不是 `echo-deploy` 的已知取值。
    UnknownPhase,
    /// 依赖的字段缺失 / 类型不对 / 取值不安全。
    InvalidField(&'static str),
    /// phase 与 error 的伴随关系违反部署工具的不变量。
    ErrorPhaseMismatch(&'static str),
}

impl std::fmt::Display for TransactionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(f, "不是合法 JSON（{error}）"),
            Self::NotObject => write!(f, "顶层不是 JSON 对象"),
            Self::UnsupportedSchema => write!(f, "schema_version 不是受支持的 1"),
            Self::MissingPhase => write!(f, "缺少非空 phase 字段"),
            Self::UnknownPhase => write!(f, "phase 不是 echo-deploy 的已知取值"),
            Self::InvalidField(field) => {
                write!(f, "字段 {field} 缺失、类型不对或取值不安全")
            }
            Self::ErrorPhaseMismatch(reason) => write!(f, "{reason}"),
        }
    }
}

/// error.code / error.step 的机器可读 token 约束（serde snake_case）。
fn validate_error_token(value: &str, field: &'static str) -> Result<(), TransactionParseError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(TransactionParseError::InvalidField(field));
    }
    Ok(())
}

/// 可选的安全 token（release id 与 `echo-deploy::model::safe_id` 同规则）。
fn optional_safe_token(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<Option<String>, TransactionParseError> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => {
            validate_instance_id(value).map_err(|_| TransactionParseError::InvalidField(field))?;
            Ok(Some(value.clone()))
        }
        Some(_) => Err(TransactionParseError::InvalidField(field)),
    }
}

/// 最后一条 step evidence 的观测时间（审计字段；缺失就省略）。
fn transaction_updated_at(object: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let steps = object.get("steps")?.as_array()?;
    let last = steps.last()?.as_object()?;
    let millis = last.get("at_unix_ms")?.as_u64()?;
    rfc3339_utc_from_unix_ms(millis)
}

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
///
/// 不引入 chrono：事务状态只是展示字段；Howard Hinnant 的 civil-from-days
/// 算法对 1970 年后的取值足够且可单测。
fn rfc3339_utc_from_unix_ms(millis: u64) -> Option<String> {
    let seconds = i64::try_from(millis / 1000).ok()?;
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (remainder / 3_600, (remainder % 3_600) / 60, remainder % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// 纯解析：`echo-deploy` 的事务 JSON → 状态投影。
///
/// 只依赖稳定契约：`schema_version` / `phase` / `error{code,step}`。未知
/// phase、schema 版本不符、phase-error 不变量冲突都判为**格式错误**，由调用
/// 方发 `Error`——不猜一个看起来合理的状态。
fn parse_transaction(text: &str) -> Result<TransactionStatus, TransactionParseError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| TransactionParseError::InvalidJson(error.to_string()))?;
    let object = value.as_object().ok_or(TransactionParseError::NotObject)?;
    if object
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err(TransactionParseError::UnsupportedSchema);
    }
    let phase = object
        .get("phase")
        .and_then(serde_json::Value::as_str)
        .ok_or(TransactionParseError::MissingPhase)?;
    if !TRANSACTION_PHASES.contains(&phase) {
        return Err(TransactionParseError::UnknownPhase);
    }
    let error = match object.get("error") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(error)) => {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_str)
                .ok_or(TransactionParseError::InvalidField("error.code"))?;
            validate_error_token(code, "error.code")?;
            let step = match error.get("step") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(step)) => {
                    validate_error_token(step, "error.step")?;
                    Some(step.to_owned())
                }
                Some(_) => return Err(TransactionParseError::InvalidField("error.step")),
            };
            Some((code.to_owned(), step))
        }
        // 部署工具的严格反序列化要求 `error` 键存在（可为 null）。
        None => return Err(TransactionParseError::InvalidField("error")),
        Some(_) => return Err(TransactionParseError::InvalidField("error")),
    };
    let needs_error = matches!(phase, "rolling_back" | "rolled_back" | "recovery_required");
    let forbids_error = matches!(
        phase,
        "prepared" | "maintenance" | "drained" | "switching" | "verifying" | "committed"
    );
    if needs_error && error.is_none() {
        return Err(TransactionParseError::ErrorPhaseMismatch(
            "该 phase 必须带 error",
        ));
    }
    if forbids_error && error.is_some() {
        return Err(TransactionParseError::ErrorPhaseMismatch(
            "该 phase 不允许带 error",
        ));
    }

    Ok(TransactionStatus {
        phase: phase.to_owned(),
        target_release: optional_safe_token(object, "target_release")?,
        previous_release: optional_safe_token(object, "previous_release")?,
        error_code: error.as_ref().map(|(code, _)| code.clone()),
        error_step: error.and_then(|(_, step)| step),
        updated_at: transaction_updated_at(object),
    })
}

/// 事务 phase → `SelfUpdateStatus`。
///
/// 映射到旧面板认识的 state：进行中 = `running`，提交 = `updated`，
/// 回滚完成 = `rolled_back`，恢复失败 = `failed`。文案只带 phase /
/// release id / error.code（+step），**不带** error.message。
fn transaction_status_event(fields: TransactionStatus) -> BackendEvent {
    let state = match fields.phase.as_str() {
        "committed" => "updated",
        "rolled_back" => "rolled_back",
        "recovery_required" => "failed",
        _ => "running",
    };
    let headline = match fields.phase.as_str() {
        "prepared" => "自更新事务已准备",
        "maintenance" => "自更新进行中（进入维护）",
        "drained" => "自更新进行中（已完成排空）",
        "switching" => "自更新进行中（切换版本）",
        "verifying" => "自更新进行中（验证新版本）",
        "committed" => "自更新完成",
        "rolling_back" => "自更新失败，正在回滚",
        "rolled_back" => "自更新已回滚",
        "recovery_required" => "自更新失败，需要人工恢复",
        _ => "自更新状态未知",
    };
    let mut message = format!("{headline}（phase={}）", fields.phase);
    if fields.phase == "rolled_back" {
        if let Some(previous) = &fields.previous_release {
            message.push_str(&format!("；已恢复到 {previous}"));
        }
    } else if let Some(target) = &fields.target_release {
        message.push_str(&format!("；目标版本 {target}"));
    }
    if let Some(code) = &fields.error_code {
        message.push_str(&format!("；错误 {code}"));
        if let Some(step) = &fields.error_step {
            message.push_str(&format!("（步骤 {step}）"));
        }
    }
    BackendEvent::SelfUpdateStatus {
        state: state.into(),
        phase: Some(fields.phase),
        revision: fields.target_release,
        message: Some(message),
        updated_at: fields.updated_at,
    }
}

fn idle_event() -> BackendEvent {
    BackendEvent::SelfUpdateStatus {
        state: "idle".into(),
        phase: None,
        revision: None,
        message: None,
        updated_at: None,
    }
}

fn error_event(message: impl Into<String>) -> BackendEvent {
    BackendEvent::Error {
        session_id: None,
        message: message.into(),
    }
}

/// 读状态 → `SelfUpdateStatus` 事件（无文件 = `idle`）。
///
/// 来源选择：
/// - 新部署（`echo-deploy`）的 `ECHO_UPDATE_TRANSACTION_FILE` /
///   `ECHO_AGENT_STATE_DIR`+`ECHO_AGENT_INSTANCE` → 事务 JSON；
/// - 否则走旧布局（`ECHO_UPDATE_STATUS_FILE` → XDG → `$HOME`）。
///
/// 任一来源的路径解析失败都发 `Error`，不静默读一个猜测出来的路径。
pub async fn read_status() -> BackendEvent {
    match status_source_checked() {
        Ok(StatusSource::Transaction(path)) => read_transaction_status_at(&path).await,
        Ok(StatusSource::Legacy(path)) => read_status_at(&path).await,
        Err(error) => error_event(format!("无法查询自更新状态：{error}")),
    }
}

/// 读**指定**旧状态文件 → `SelfUpdateStatus` 事件。
///
/// - 文件不存在 = `idle`（从未更新过，这是真实的"空闲"）；
/// - 其它读取失败（权限 / IO / 路径不是文件）或文件存在但缺 `state`
///   = `Error` 事件——**不冒充 idle**：那会让面板误以为可以再点一次。
///
/// 路径可注入是测试隔离的前提：此前测试靠 `set_var("XDG_STATE_HOME")` 改变
/// 进程级环境变量，两个测试并行时互相覆盖 → 偶发失败（2026-10 修）。
pub async fn read_status_at(path: &Path) -> BackendEvent {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return idle_event(),
        Err(error) => {
            return error_event(format!(
                "读取自更新状态文件失败（{}）：{error}",
                path.display()
            ))
        }
    };
    match parse_status(&text) {
        Ok(fields) => BackendEvent::SelfUpdateStatus {
            state: fields.state,
            phase: fields.phase,
            revision: fields.revision,
            message: fields.message,
            updated_at: fields.updated_at,
        },
        Err(error) => error_event(format!(
            "自更新状态文件 {} 格式无效：{error}",
            path.display()
        )),
    }
}

/// 读**指定**新部署事务文件 → `SelfUpdateStatus` 事件。
///
/// - 文件不存在 = `idle`（该实例还没跑过更新）；
/// - 大小超上限 / 不是普通文件 / 读取失败 / schema 不认识
///   = `Error`（**不冒充 idle**，也不猜成功状态）；
/// - `phase` 映射见 [`transaction_status_event`]。
///
/// 路径显式注入（而非内部读环境）让测试可并行，且不碰真实部署目录。
pub async fn read_transaction_status_at(path: &Path) -> BackendEvent {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return idle_event(),
        Err(error) => {
            return error_event(format!(
                "读取自更新事务文件失败（{}）：{error}",
                path.display()
            ))
        }
    };
    if !metadata.is_file() {
        return error_event(format!(
            "读取自更新事务文件失败（{}）：不是普通文件",
            path.display()
        ));
    }
    if metadata.len() > MAX_TRANSACTION_BYTES {
        return error_event(format!(
            "自更新事务文件 {} 超过大小上限（{MAX_TRANSACTION_BYTES} 字节）",
            path.display()
        ));
    }
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return idle_event(),
        Err(error) => {
            return error_event(format!(
                "读取自更新事务文件失败（{}）：{error}",
                path.display()
            ))
        }
    };
    if text.len() as u64 > MAX_TRANSACTION_BYTES {
        return error_event(format!(
            "自更新事务文件 {} 超过大小上限（{MAX_TRANSACTION_BYTES} 字节）",
            path.display()
        ));
    }
    match parse_transaction(&text) {
        Ok(fields) => transaction_status_event(fields),
        Err(error) => error_event(format!(
            "自更新事务文件 {} 格式无效：{error}",
            path.display()
        )),
    }
}
/// updater 单元当前的活跃度分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitActivity {
    /// 已有 updater 在跑（或正在启动/停止的临界区）。
    Busy,
    /// 可以启动新一次更新。
    Idle,
    /// 输出无法识别——fail-closed，不启动。
    Unknown,
}

/// 把 `systemctl show -p ActiveState` 的输出分类。
///
/// oneshot 执行期间是 `activating`（`is-active --quiet` 返回 3）；
/// `active` 只可能出现在 `RemainAfterExit=yes` 的变体上，也算忙。
/// `failed` / `inactive` 都允许重试。
fn classify_active_state(raw: &str) -> UnitActivity {
    match raw.trim() {
        "activating" | "active" | "reloading" | "deactivating" => UnitActivity::Busy,
        "inactive" | "failed" => UnitActivity::Idle,
        _ => UnitActivity::Unknown,
    }
}

/// 取 `systemctl show --value` 的最后一个非空行（个别版本会先打警告）。
fn active_state_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// systemctl 调用的最小封装：程序路径、前缀参数与超时可注入（测试用
/// `/bin/sh <fake-script>`，绝不碰真实 manager）。
struct Systemctl {
    program: OsString,
    /// 程序前的前缀参数；生产为空，测试用来注入假脚本。
    prefix_args: Vec<OsString>,
    timeout: Duration,
}

impl Systemctl {
    fn new() -> Self {
        Self {
            program: OsString::from("systemctl"),
            prefix_args: Vec::new(),
            timeout: SYSTEMCTL_TIMEOUT,
        }
    }

    /// 跑一条 systemctl 子命令；超时 / spawn 失败统一成错误文案。
    async fn run(
        &self,
        scope: ServiceScope,
        args: &[&str],
    ) -> Result<std::process::Output, String> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.prefix_args)
            .args(scope.systemctl_scope_args())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(self.timeout, command.output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err(format!("无法执行 systemctl {}：{error}", args.join(" "))),
            Err(_) => Err(format!(
                "systemctl {} 超时（{}s，manager 无响应）",
                args.join(" "),
                self.timeout.as_secs()
            )),
        }
    }
}

/// stderr 的短摘要（错误文案带证据，但别把整段 journal 塞进事件）。
fn stderr_snippet(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut snippet: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        snippet.push('…');
    }
    format!("：{snippet}")
}
/// 触发自更新：启动 oneshot 单元（它脱离本进程执行"构建 → 替换 → 重启"，
/// agent 自杀不了它）。Graceful Drain 由 Core 的 SIGTERM 处理保证（当前回复
/// 完整生成后再退）。
///
/// 单元名按部署解析：`ECHO_UPDATE_UNIT` 显式覆盖 → `ECHO_AGENT_INSTANCE`
/// 推导（新实例部署）→ 旧 `echo-agent-core-update.service`。显式取值非法时
/// 发 `Error` 且**不启动任何单元**（不静默换一个单元）。
///
/// 只负责**排队**：`systemctl start --no-block` 返回即回执，构建在后台继续。
pub async fn start_update(host: &dyn SelfUpdateHost) {
    start_update_in(host, ServiceScope::from_env()).await
}

/// 同上，作用域显式给出（测试与非常规部署用）。
///
/// 所有 systemctl 调用都有 `SYSTEMCTL_TIMEOUT` 上限；本函数不会等待
/// updater 的构建完成。
pub async fn start_update_in(host: &dyn SelfUpdateHost, scope: ServiceScope) {
    let unit = match update_unit_name() {
        Ok(unit) => unit,
        Err(error) => {
            host.emit(error_event(format!("无法确定自更新单元：{error}")));
            return;
        }
    };
    start_update_with_unit(host, scope, &unit, &Systemctl::new()).await
}

/// 旧单元名 + 可注入 systemctl（保留给既有测试；生产走
/// [`start_update_in`] 的部署解析）。
#[cfg(test)]
async fn start_update_with(host: &dyn SelfUpdateHost, scope: ServiceScope, systemctl: &Systemctl) {
    start_update_with_unit(host, scope, UPDATE_UNIT, systemctl).await
}

/// 触发流程本体（单元名与 systemctl 都可注入）。
async fn start_update_with_unit(
    host: &dyn SelfUpdateHost,
    scope: ServiceScope,
    unit: &str,
    systemctl: &Systemctl,
) {
    // 防重入：oneshot 在跑时 ActiveState=activating——必须用 show 读裸值，
    // `is-active --quiet` 会把 activating 与 inactive 一起归成"非 0"。
    let active_state = match systemctl.run(scope, &active_state_args(unit)).await {
        Ok(output) if output.status.success() => active_state_line(&output.stdout),
        Ok(output) => {
            host.emit(error_event(format!(
                "无法确认自更新单元状态（systemctl show {unit} {}）{}",
                output.status,
                stderr_snippet(&output)
            )));
            return;
        }
        Err(error) => {
            host.emit(error_event(format!("无法确认自更新单元状态：{error}")));
            return;
        }
    };
    match classify_active_state(&active_state) {
        UnitActivity::Busy => {
            host.emit(error_event(format!(
                "自更新已在进行中（{unit} ActiveState={active_state}）——未重复启动"
            )));
            return;
        }
        UnitActivity::Unknown => {
            host.emit(error_event(format!(
                "无法识别 {unit} 的 ActiveState={active_state:?}——为避免重复启动，本次不触发"
            )));
            return;
        }
        UnitActivity::Idle => {}
    }

    // --no-block：oneshot 的 start 默认会等 ExecStart（构建，最长 1800s）
    // 结束；那会把命令泵和面板请求一起挂住。只入队，构建进度靠状态文件。
    match systemctl.run(scope, &start_args(unit)).await {
        Ok(output) if output.status.success() => {
            // 受理回执：文件此刻可能还是上一轮的终态（或尚未创建），直接
            // 读会显示"已更新/空闲"。先发排队态，前端轮询随后接管。
            host.emit(BackendEvent::SelfUpdateStatus {
                state: "running".into(),
                phase: Some("initializing".into()),
                revision: None,
                message: Some(format!(
                    "自更新已排队（{unit}）；构建在后台继续，进度请轮询状态"
                )),
                updated_at: None,
            });
        }
        Ok(output) => {
            host.emit(error_event(format!(
                "自更新启动失败（systemctl start --no-block {unit} {}）{}——请检查 {unit} 是否装在 {} 作用域",
                output.status,
                stderr_snippet(&output),
                scope.describe(),
            )));
        }
        Err(error) => {
            host.emit(error_event(format!("自更新启动失败：{error}")));
        }
    }
}

/// 命令处理器。
pub struct SelfUpdateHandler {
    host: Arc<dyn SelfUpdateHost>,
    /// 旧状态文件覆盖（`None` = 按 `ECHO_UPDATE_STATUS_FILE` / XDG 解析）。
    ///
    /// 放在结构上而不是每次读环境变量：测试并行跑时环境变量是**进程级**
    /// 副作用，会互相覆盖（2026-10 偶发失败的根因）。
    status_file: Option<PathBuf>,
    /// 新部署事务文件覆盖（`None` = 按 `ECHO_UPDATE_TRANSACTION_FILE` /
    /// `ECHO_AGENT_STATE_DIR` + `ECHO_AGENT_INSTANCE` 解析）。
    transaction_file: Option<PathBuf>,
}

impl SelfUpdateHandler {
    /// 用宿主接缝构造处理器。
    pub fn new(host: Arc<dyn SelfUpdateHost>) -> Self {
        Self {
            host,
            status_file: None,
            transaction_file: None,
        }
    }

    /// 指定旧状态文件（测试 / 非常规部署布局用）。
    pub fn with_status_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.status_file = Some(path.into());
        self
    }

    /// 指定新部署事务文件（测试 / 非常规部署布局用）。
    ///
    /// 显式注入优先于环境解析；与 [`Self::with_status_path`] 同时设置时
    /// 事务文件优先（新布局是更具体的声明）。
    pub fn with_transaction_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.transaction_file = Some(path.into());
        self
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => {
                return Some(Err(format!(
                    "self-update 命令载荷解析失败（{name}）: {error}"
                )))
            }
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestSelfUpdate => {
                start_update(self.host.as_ref()).await;
                Ok(())
            }
            BackendCommand::RequestSelfUpdateStatus => {
                // 显式注入优先：事务（新 schema）→ 旧状态文件 → 环境解析。
                if let Some(path) = self.transaction_file.as_deref() {
                    self.host.emit(read_transaction_status_at(path).await);
                } else if let Some(path) = self.status_file.as_deref() {
                    self.host.emit(read_status_at(path).await);
                } else {
                    self.host.emit(read_status().await);
                }
                Ok(())
            }
            other => Err(format!(
                "self-update 命令域不处理 {:?}（注册关系与命令泵不一致）",
                other.kind_name()
            )),
        }
    }
}

/// 事件出口的**最小实现**（组合根只需要把 `EventSink` 包一层）。
pub struct SinkHost {
    sink: Arc<dyn Fn(BackendEvent) + Send + Sync>,
}

impl SinkHost {
    /// 用一个进程级事件出口构造。
    pub fn new(sink: Arc<dyn Fn(BackendEvent) + Send + Sync>) -> Self {
        Self { sink }
    }
}

impl SelfUpdateHost for SinkHost {
    fn emit(&self, event: BackendEvent) {
        (self.sink)(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        events: Mutex<Vec<BackendEvent>>,
    }

    impl SelfUpdateHost for FakeHost {
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn handler() -> (SelfUpdateHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (SelfUpdateHandler::new(host.clone()), host)
    }

    /// 指向**指定**状态文件的处理器（并行安全的测试隔离）。
    fn handler_at(path: impl Into<PathBuf>) -> (SelfUpdateHandler, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        (
            SelfUpdateHandler::new(host.clone()).with_status_path(path),
            host,
        )
    }

    /// 并行安全的临时目录：pid + 进程内序号，Drop 清理。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("echo-su-{tag}-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    // ── 状态路径解析（纯 helper；不碰进程环境）──

    #[test]
    fn absolute_override_wins_over_xdg_and_home() {
        let path = resolve_status_path_from(
            Some(OsStr::new("/run/echo/custom-status")),
            Some(OsStr::new("/xdg/state")),
            Some(OsStr::new("/home/user")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/run/echo/custom-status"));
    }

    #[test]
    fn relative_override_is_explicit_error_not_silent_fallback() {
        let error = resolve_status_path_from(
            Some(OsStr::new("relative/update-status")),
            Some(OsStr::new("/xdg/state")),
            Some(OsStr::new("/home/user")),
        )
        .unwrap_err();
        match error {
            StatusPathError::InvalidOverride { value, reason } => {
                assert_eq!(value, "relative/update-status");
                assert_eq!(reason, "不是绝对路径");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn empty_or_nul_override_is_rejected() {
        assert!(matches!(
            resolve_status_path_from(Some(OsStr::new("")), None, None),
            Err(StatusPathError::InvalidOverride {
                reason: "为空", ..
            })
        ));
        assert!(matches!(
            resolve_status_path_from(Some(OsStr::new("bad\0path")), None, None),
            Err(StatusPathError::InvalidOverride {
                reason: "含 NUL 字节",
                ..
            })
        ));
    }

    #[test]
    fn xdg_state_home_is_used_when_override_absent() {
        let path = resolve_status_path_from(
            None,
            Some(OsStr::new("/xdg/state")),
            Some(OsStr::new("/home/user")),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/xdg/state/echo-agent-core/update-status")
        );
    }

    #[test]
    fn empty_xdg_is_treated_as_unset() {
        let path =
            resolve_status_path_from(None, Some(OsStr::new("")), Some(OsStr::new("/home/user")))
                .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/state/echo-agent-core/update-status")
        );
    }

    #[test]
    fn relative_xdg_is_error_not_silent_home_fallback() {
        let error = resolve_status_path_from(
            None,
            Some(OsStr::new("state")),
            Some(OsStr::new("/home/user")),
        )
        .unwrap_err();
        assert!(matches!(error, StatusPathError::InvalidStateHome { .. }));
    }

    #[test]
    fn home_fallback_builds_the_documented_layout() {
        let path = resolve_status_path_from(None, None, Some(OsStr::new("/home/user"))).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/state/echo-agent-core/update-status")
        );
    }

    #[test]
    fn missing_or_relative_home_is_error() {
        assert!(matches!(
            resolve_status_path_from(None, None, None),
            Err(StatusPathError::NoHome { .. })
        ));
        assert!(matches!(
            resolve_status_path_from(None, None, Some(OsStr::new("home/user"))),
            Err(StatusPathError::NoHome { .. })
        ));
    }

    // ── 新部署工具的事务状态源（纯 helper；不碰进程环境）──

    #[test]
    fn explicit_transaction_file_wins_over_instance_derivation() {
        assert_eq!(
            transaction_file_from(
                Some(OsStr::new("/run/echo/transaction.json")),
                Some(OsStr::new("/state/echo")),
                Some(OsStr::new("demo")),
            )
            .unwrap(),
            Some(PathBuf::from("/run/echo/transaction.json"))
        );
    }

    /// `echo-deploy` 把事务写到 `state_dir/transaction-<instance_id>.json`
    /// （`TransactionStore::open(state_dir).load(instance_id)`），而
    /// `ECHO_AGENT_STATE_DIR` 正是渲染进 Core 单元的那个 `state_dir`。
    #[test]
    fn transaction_file_is_derived_from_agent_state_dir_and_instance() {
        assert_eq!(
            transaction_file_from(
                None,
                Some(OsStr::new("/state/echo")),
                Some(OsStr::new("demo")),
            )
            .unwrap(),
            Some(PathBuf::from("/state/echo/transaction-demo.json"))
        );
        // 老部署没有这两个变量：不推导，继续走 legacy update-status。
        assert_eq!(transaction_file_from(None, None, None).unwrap(), None);
        assert_eq!(
            transaction_file_from(None, None, Some(OsStr::new("demo"))).unwrap(),
            None
        );
    }

    /// 显式配了事务布局但取值非法：fail-closed，不静默退回 legacy 文件
    /// （那会把"新部署路径配错"伪装成"从未更新过"）。
    #[test]
    fn invalid_transaction_env_fails_closed() {
        assert!(matches!(
            transaction_file_from(Some(OsStr::new("transaction.json")), None, None),
            Err(StatusSourceError::InvalidEnv {
                name: UPDATE_TRANSACTION_FILE_ENV,
                ..
            })
        ));
        assert!(matches!(
            transaction_file_from(None, Some(OsStr::new("/state")), None),
            Err(StatusSourceError::InvalidEnv {
                name: AGENT_INSTANCE_ENV,
                ..
            })
        ));
        assert!(matches!(
            transaction_file_from(
                None,
                Some(OsStr::new("/state")),
                Some(OsStr::new("../demo"))
            ),
            Err(StatusSourceError::InvalidEnv {
                name: AGENT_INSTANCE_ENV,
                ..
            })
        ));
        assert!(matches!(
            transaction_file_from(None, Some(OsStr::new("state")), Some(OsStr::new("demo"))),
            Err(StatusSourceError::InvalidEnv {
                name: AGENT_STATE_DIR_ENV,
                ..
            })
        ));
    }

    #[test]
    fn status_source_prefers_transaction_then_legacy() {
        let transaction = status_source_from(
            Some(OsStr::new("/run/echo/tx.json")),
            None,
            None,
            Some(OsStr::new("/run/echo/update-status")),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            transaction,
            StatusSource::Transaction(PathBuf::from("/run/echo/tx.json"))
        );

        let legacy = status_source_from(
            None,
            None,
            None,
            Some(OsStr::new("/run/echo/update-status")),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            legacy,
            StatusSource::Legacy(PathBuf::from("/run/echo/update-status"))
        );
    }

    // ── 状态文件解析 / 读取 ──

    #[test]
    fn parse_status_projects_fields_and_ignores_noise() {
        let fields = parse_status(
            "\n# comment without equals\nstate=running\nphase=building\nrevision=abc123\nmessage=编译中\nupdated_at=2026-11-01T00:00:00Z\nempty=\n",
        )
        .unwrap();
        assert_eq!(fields.state, "running");
        assert_eq!(fields.phase.as_deref(), Some("building"));
        assert_eq!(fields.revision.as_deref(), Some("abc123"));
        assert_eq!(fields.message.as_deref(), Some("编译中"));
        assert_eq!(fields.updated_at.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    #[test]
    fn parse_status_requires_non_empty_state() {
        assert_eq!(
            parse_status("phase=building\n"),
            Err(StatusParseError::MissingState)
        );
        assert_eq!(
            parse_status("state=   \nphase=building\n"),
            Err(StatusParseError::MissingState)
        );
    }

    // ── 新部署工具事务 JSON（真实 schema：phase / error / steps）──

    /// 一个符合 `echo-deploy` `Transaction` 严格 schema 的最小文档。
    /// `error` 必须是显式 `null` 或对象（缺失会被部署工具拒绝）。
    fn transaction_doc(phase: &str, error: &str, target: &str, previous: &str) -> String {
        format!(
            concat!(
                "{{\"schema_version\":1,\"id\":\"tx-1\",\"instance_id\":\"demo\",",
                "\"previous_release\":{previous},\"target_release\":\"{target}\",",
                "\"manifest_sha256\":\"{manifest}\",\"phase\":\"{phase}\",",
                "\"error\":{error},\"steps\":[{{\"sequence\":1,",
                "\"step\":\"prepare\",\"outcome\":\"succeeded\",",
                "\"at_unix_ms\":1760000000000,\"detail\":null}}]}}"
            ),
            manifest = "a".repeat(64),
            previous = previous,
            target = target,
            phase = phase,
            error = error,
        )
    }

    #[test]
    fn transaction_phase_maps_to_running_state_and_release() {
        let fields =
            parse_transaction(&transaction_doc("switching", "null", "rel-2", "null")).unwrap();
        assert_eq!(fields.phase, "switching");
        assert_eq!(fields.target_release.as_deref(), Some("rel-2"));
        assert_eq!(fields.updated_at.as_deref(), Some("2025-10-09T08:53:20Z"));

        match transaction_status_event(fields) {
            BackendEvent::SelfUpdateStatus {
                state,
                phase,
                revision,
                message,
                updated_at,
            } => {
                assert_eq!(state, "running");
                assert_eq!(phase.as_deref(), Some("switching"));
                assert_eq!(revision.as_deref(), Some("rel-2"));
                assert!(message.unwrap().contains("切换版本"));
                assert_eq!(updated_at.as_deref(), Some("2025-10-09T08:53:20Z"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transaction_committed_maps_to_updated() {
        let fields =
            parse_transaction(&transaction_doc("committed", "null", "rel-2", "\"rel-1\"")).unwrap();
        match transaction_status_event(fields) {
            BackendEvent::SelfUpdateStatus {
                state,
                phase,
                revision,
                message,
                ..
            } => {
                assert_eq!(state, "updated");
                assert_eq!(phase.as_deref(), Some("committed"));
                assert_eq!(revision.as_deref(), Some("rel-2"));
                assert!(message.unwrap().contains("完成"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 回滚是失败后的终态：state=rolled_back，文案带机器可读的 error.code，
    /// 但 **不带** error.message（可能有后端输出/凭据，事件与日志不泄密）。
    #[test]
    fn transaction_rolled_back_maps_error_code_without_message_leak() {
        let error = concat!(
            r#"{"code":"readiness_failed","#,
            r#""message":"probe failed\nTOKEN=secret-value","#,
            r#""step":"verify_readiness"}"#
        );
        let fields =
            parse_transaction(&transaction_doc("rolled_back", error, "rel-2", "\"rel-1\""))
                .unwrap();
        match transaction_status_event(fields) {
            BackendEvent::SelfUpdateStatus { state, message, .. } => {
                let message = message.unwrap();
                assert_eq!(state, "rolled_back");
                assert!(message.contains("readiness_failed"), "{message}");
                assert!(message.contains("已恢复到 rel-1"), "{message}");
                assert!(!message.contains("secret-value"), "{message}");
                assert!(!message.contains("TOKEN"), "{message}");
                assert!(!message.contains('\n'), "{message}");
                assert!(!message.contains("schema_version"), "{message}");
                assert!(message.chars().count() < 500, "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transaction_recovery_required_maps_to_failed() {
        let error = r#"{"code":"interrupted","message":"process crashed","step":null}"#;
        let fields = parse_transaction(&transaction_doc(
            "recovery_required",
            error,
            "rel-2",
            "null",
        ))
        .unwrap();
        match transaction_status_event(fields) {
            BackendEvent::SelfUpdateStatus { state, message, .. } => {
                assert_eq!(state, "failed");
                assert!(message.unwrap().contains("需要人工恢复"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transaction_parse_rejects_unsupported_shape() {
        assert!(matches!(
            parse_transaction("not json"),
            Err(TransactionParseError::InvalidJson(_))
        ));
        assert!(matches!(
            parse_transaction("[]"),
            Err(TransactionParseError::NotObject)
        ));
        assert!(matches!(
            parse_transaction(
                &transaction_doc("switching", "null", "rel-2", "null")
                    .replace("\"schema_version\":1", "\"schema_version\":2")
            ),
            Err(TransactionParseError::UnsupportedSchema)
        ));
        assert!(matches!(
            parse_transaction(&transaction_doc("bogus", "null", "rel-2", "null")),
            Err(TransactionParseError::UnknownPhase)
        ));
        assert!(matches!(
            parse_transaction(&transaction_doc("switching", "null", "../bad", "null")),
            Err(TransactionParseError::InvalidField("target_release"))
        ));
    }

    /// phase 与 error 的伴随关系是部署工具 `Transaction::validate` 的硬约束；
    /// 违反即文件损坏，必须报错而不是猜一个状态。
    #[test]
    fn transaction_parse_enforces_phase_error_invariant() {
        assert!(matches!(
            parse_transaction(&transaction_doc("rolled_back", "null", "rel-2", "null")),
            Err(TransactionParseError::ErrorPhaseMismatch(_))
        ));
        assert!(matches!(
            parse_transaction(&transaction_doc(
                "committed",
                r#"{"code":"interrupted","message":"x","step":null}"#,
                "rel-2",
                "null"
            )),
            Err(TransactionParseError::ErrorPhaseMismatch(_))
        ));
    }

    #[test]
    fn rfc3339_conversion_is_utc_and_stable() {
        assert_eq!(
            rfc3339_utc_from_unix_ms(0).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            rfc3339_utc_from_unix_ms(1760000000000).as_deref(),
            Some("2025-10-09T08:53:20Z")
        );
        assert_eq!(
            rfc3339_utc_from_unix_ms(999).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, host) = handler();
        assert!(h
            .handle("RequestState", serde_json::Value::Null)
            .await
            .is_none());
        assert!(host.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_without_file_is_idle() {
        // 指向一个必然不存在的状态文件——**注入路径**而不是改环境变量：
        // 并行测试下 `set_var` 会与同进程其它测试互相覆盖（2026-10 修）。
        let dir = TempDir::new("none");
        let path = dir.join("echo-agent-core/update-status");
        let (h, host) = handler_at(&path);
        h.handle("RequestSelfUpdateStatus", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SelfUpdateStatus { state, phase, .. } => {
                assert_eq!(state, "idle");
                assert!(phase.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_file_fields_are_projected() {
        let dir = TempDir::new("fields");
        let path = dir.join("echo-agent-core");
        std::fs::create_dir_all(&path).unwrap();
        let status_file = path.join("update-status");
        std::fs::write(
            &status_file,
            "state=running\nphase=building\nrevision=abc123\nmessage=编译中\nupdated_at=2026-11-01T00:00:00Z\n",
        )
        .unwrap();
        let (h, host) = handler_at(&status_file);
        h.handle("RequestSelfUpdateStatus", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SelfUpdateStatus {
                state,
                phase,
                revision,
                message,
                updated_at,
            } => {
                assert_eq!(state, "running");
                assert_eq!(phase.as_deref(), Some("building"));
                assert_eq!(revision.as_deref(), Some("abc123"));
                assert_eq!(message.as_deref(), Some("编译中"));
                assert!(updated_at.is_some());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 文件存在但缺 state：不能装 idle（否则权限/格式错误看起来像"空闲"）。
    #[tokio::test]
    async fn malformed_status_file_is_error_not_idle() {
        let dir = TempDir::new("malformed");
        let status_file = dir.join("update-status");
        std::fs::write(&status_file, "phase=building\n").unwrap();
        match read_status_at(&status_file).await {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("格式无效"), "{message}");
                assert!(message.contains("state"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 读取失败（ENOTDIR，不是 NotFound）：必须发 Error，不能是 idle。
    #[tokio::test]
    async fn read_failure_is_error_not_idle() {
        let dir = TempDir::new("read-error");
        let regular_file = dir.join("not-a-directory");
        std::fs::write(&regular_file, "x").unwrap();
        let status_file = regular_file.join("update-status");
        match read_status_at(&status_file).await {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("读取自更新状态文件失败"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_unit_is_the_installed_oneshot_unit() {
        assert_eq!(UPDATE_UNIT, "echo-agent-core-update.service");
    }

    /// 作用域只影响 systemctl 的前缀参数；system 作用域不能带 `--user`
    /// （带了就落到另一套 manager 上，报 Unit not found）。
    #[test]
    fn scope_maps_to_systemctl_arguments() {
        assert_eq!(ServiceScope::User.systemctl_scope_args(), &["--user"]);
        assert!(
            ServiceScope::System.systemctl_scope_args().is_empty(),
            "system scope must invoke plain `systemctl`: --user addresses a different manager"
        );
    }

    /// 作用域来源：旧部署只注入 `ECHO_SERVICE_SCOPE`；新部署（`echo-deploy`）
    /// 注入 `ECHO_AGENT_SCOPE`，deployment 模式下两者都有且一致。旧的优先，
    /// 缺失/不认识时看新的，最后才退回历史默认 user。
    #[test]
    fn scope_resolution_prefers_legacy_env_then_instance_env() {
        assert_eq!(
            resolve_service_scope(Some("system"), Some("user")),
            ServiceScope::System
        );
        assert_eq!(
            resolve_service_scope(Some("user"), Some("system")),
            ServiceScope::User
        );
        assert_eq!(
            resolve_service_scope(None, Some("system")),
            ServiceScope::System
        );
        assert_eq!(
            resolve_service_scope(None, Some("user")),
            ServiceScope::User
        );
        assert_eq!(resolve_service_scope(None, None), ServiceScope::User);
        assert_eq!(
            resolve_service_scope(Some("bogus"), Some("system")),
            ServiceScope::System,
            "unknown legacy value must not mask the instance scope"
        );
        assert_eq!(
            resolve_service_scope(Some("bogus"), Some("bogus")),
            ServiceScope::User
        );
    }

    // ── updater 单元名（纯 helper；不碰进程环境）──

    #[test]
    fn explicit_update_unit_wins_and_must_be_safe() {
        assert_eq!(
            resolve_update_unit_from(
                Some(OsStr::new("echo-agent-demo-update.service")),
                Some(OsStr::new("ignored")),
            )
            .unwrap(),
            "echo-agent-demo-update.service"
        );
        for bad in [
            "",
            "-now.service",
            ".hidden.service",
            "demo.timer",
            "demo/update.service",
            "demo update.service",
            "demo..service",
            "demo.service\n",
            "demo@.service",
        ] {
            assert!(
                resolve_update_unit_from(Some(OsStr::new(bad)), None).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    /// 新部署工具只注入 `ECHO_AGENT_INSTANCE`：单元名必须与
    /// `InstallSpec::unit_names()`（`echo-agent-<instance>-update.service`）一致。
    #[test]
    fn instance_derives_the_installed_updater_unit() {
        assert_eq!(
            resolve_update_unit_from(None, Some(OsStr::new("demo"))).unwrap(),
            "echo-agent-demo-update.service"
        );
        assert_eq!(
            resolve_update_unit_from(None, Some(OsStr::new("local-dev"))).unwrap(),
            "echo-agent-local-dev-update.service"
        );
        for bad in ["", "-demo", "../demo", "demo/x", "demo x", "demo\n"] {
            assert!(
                resolve_update_unit_from(None, Some(OsStr::new(bad))).is_err(),
                "unsafe instance {bad:?} must be rejected"
            );
        }
        assert_eq!(resolve_update_unit_from(None, None).unwrap(), UPDATE_UNIT);
    }

    /// 非 UTF-8 的显式取值不能 panic，也不能被悄悄当成 legacy。
    #[cfg(unix)]
    #[test]
    fn non_utf8_update_unit_env_is_rejected() {
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"echo-agent-\xff-update.service");
        assert!(resolve_update_unit_from(Some(raw), None).is_err());
        assert!(resolve_update_unit_from(None, Some(raw)).is_err());
    }

    // ── ActiveState 分类（纯 helper）──

    #[test]
    fn oneshot_activating_is_busy_not_idle() {
        // 这是旧实现的核心 bug：is-active --quiet 对 activating 返回 3，
        // 与 inactive 无法区分。
        assert_eq!(classify_active_state("activating"), UnitActivity::Busy);
        assert_eq!(classify_active_state("active"), UnitActivity::Busy);
        assert_eq!(classify_active_state("reloading"), UnitActivity::Busy);
        assert_eq!(classify_active_state("deactivating"), UnitActivity::Busy);
        assert_eq!(classify_active_state("inactive"), UnitActivity::Idle);
        assert_eq!(classify_active_state("failed"), UnitActivity::Idle);
        assert_eq!(classify_active_state(""), UnitActivity::Unknown);
        assert_eq!(classify_active_state("bogus"), UnitActivity::Unknown);
    }

    #[test]
    fn active_state_output_takes_last_non_empty_line() {
        assert_eq!(
            active_state_line(b"warning: something\nactivating\n"),
            "activating"
        );
        assert_eq!(active_state_line(b""), "");
    }

    #[test]
    fn start_arguments_are_non_blocking() {
        assert_eq!(
            start_args(UPDATE_UNIT),
            ["start", "--no-block", "echo-agent-core-update.service"]
        );
        assert_eq!(
            active_state_args(UPDATE_UNIT),
            [
                "show",
                "-p",
                "ActiveState",
                "--value",
                "echo-agent-core-update.service"
            ]
        );
    }

    // ── 触发流程（独立 fake systemctl；不碰真实 systemd）──

    /// 写一个假 systemctl 脚本：把 argv 追加到 `log`，并按脚本内容响应。
    ///
    /// 脚本**不**直接执行（不需要 +x），而是经 `/bin/sh <script>` 跑：
    /// 并行测试里别的线程 fork/exec 可能短暂继承"正在写"的 fd，直接 exec
    /// 刚写完的脚本会偶发 ETXTBSY；交给 sh 读则完全避开这个竞态。
    #[cfg(unix)]
    fn fake_systemctl(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.join("systemctl.sh");
        std::fs::write(&path, format!("set -u\n{body}\n")).unwrap();
        path
    }

    #[cfg(unix)]
    fn test_systemctl(script: PathBuf, timeout: Duration) -> Systemctl {
        Systemctl {
            program: OsString::from("/bin/sh"),
            prefix_args: vec![script.into_os_string()],
            timeout,
        }
    }

    #[cfg(unix)]
    fn calls(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    /// 正常路径：show=inactive → start --no-block；回执是"已排队"。
    #[cfg(unix)]
    #[tokio::test]
    async fn start_update_queues_without_waiting_and_acknowledges() {
        let dir = TempDir::new("start-ok");
        let log = dir.join("calls.log");
        let script = fake_systemctl(
            &dir,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"show "*) echo inactive ;;
  *"start --no-block"*) ;;
  *) echo "unexpected: $*" >&2; exit 64 ;;
esac"#,
                log = log.display()
            ),
        );
        let host = Arc::new(FakeHost::default());
        start_update_with(
            &*host,
            ServiceScope::User,
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        {
            let events = host.events.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "accepted start must emit exactly one event"
            );
            match &events[0] {
                BackendEvent::SelfUpdateStatus {
                    state,
                    phase,
                    message,
                    ..
                } => {
                    assert_eq!(state, "running");
                    assert_eq!(phase.as_deref(), Some("initializing"));
                    assert!(
                        message.as_deref().unwrap_or_default().contains("排队"),
                        "{message:?}"
                    );
                }
                other => panic!("unexpected: {other:?}"),
            }
        }

        let log = calls(&log);
        assert!(
            log.contains("--user show -p ActiveState --value echo-agent-core-update.service"),
            "must query ActiveState (not is-active): {log}"
        );
        assert!(
            log.contains("--user start --no-block echo-agent-core-update.service"),
            "must queue with --no-block: {log}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn activating_unit_rejects_reentrancy_without_starting() {
        let dir = TempDir::new("busy");
        let log = dir.join("calls.log");
        let script = fake_systemctl(
            &dir,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"show "*) echo activating ;;
  *"start --no-block"*) echo "must not start" >&2; exit 65 ;;
  *) exit 64 ;;
esac"#,
                log = log.display()
            ),
        );
        let host = Arc::new(FakeHost::default());
        start_update_with(
            &*host,
            ServiceScope::User,
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        {
            let events = host.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                BackendEvent::Error { message, .. } => {
                    assert!(message.contains("已在进行中"), "{message}");
                    assert!(message.contains("activating"), "{message}");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        let log = calls(&log);
        assert!(
            !log.contains("start --no-block"),
            "reentrant call must not queue a second updater: {log}"
        );
    }

    /// 查不到 ActiveState：fail-closed（报错、不启动），不能盲启动。
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_active_state_fails_closed() {
        let dir = TempDir::new("show-error");
        let log = dir.join("calls.log");
        let script = fake_systemctl(
            &dir,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"show "*) echo "Failed to connect to bus" >&2; exit 1 ;;
  *"start --no-block"*) echo "must not start" >&2; exit 65 ;;
  *) exit 64 ;;
esac"#,
                log = log.display()
            ),
        );
        let host = Arc::new(FakeHost::default());
        start_update_with(
            &*host,
            ServiceScope::User,
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        {
            let events = host.events.lock().unwrap();
            match &events[0] {
                BackendEvent::Error { message, .. } => {
                    assert!(message.contains("无法确认"), "{message}");
                    assert!(message.contains("Failed to connect to bus"), "{message}");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert!(!calls(&log).contains("start --no-block"));
    }

    /// systemctl start 非 0：错误事件带 exit code / stderr / 作用域提示。
    #[cfg(unix)]
    #[tokio::test]
    async fn start_failure_reports_exit_and_scope() {
        let dir = TempDir::new("start-error");
        let script = fake_systemctl(
            &dir,
            r#"case "$*" in
  *"show "*) echo inactive ;;
  *"start --no-block"*) echo "Unit echo-agent-core-update.service not found." >&2; exit 5 ;;
  *) exit 64 ;;
esac"#,
        );
        let host = Arc::new(FakeHost::default());
        start_update_with(
            &*host,
            ServiceScope::System,
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("启动失败"), "{message}");
                assert!(
                    message.contains("Unit echo-agent-core-update.service not found."),
                    "{message}"
                );
                assert!(message.contains("system 作用域"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// system 作用域不能带 `--user`（带了会找另一套 manager）。
    #[cfg(unix)]
    #[tokio::test]
    async fn system_scope_invokes_plain_systemctl() {
        let dir = TempDir::new("system-scope");
        let log = dir.join("calls.log");
        let script = fake_systemctl(
            &dir,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"show "*) echo inactive ;;
  *"start --no-block"*) ;;
  *) exit 64 ;;
esac"#,
                log = log.display()
            ),
        );
        let host = Arc::new(FakeHost::default());
        start_update_with(
            &*host,
            ServiceScope::System,
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        let log = calls(&log);
        assert!(log.starts_with("show "), "no --user prefix expected: {log}");
        assert!(!log.contains("--user"), "{log}");
    }

    /// manager 卡死：短超时返回错误，命令泵不被拖住。
    #[cfg(unix)]
    #[tokio::test]
    async fn hung_systemctl_times_out_quickly() {
        let dir = TempDir::new("timeout");
        let script = fake_systemctl(&dir, "exec sleep 2");
        let host = Arc::new(FakeHost::default());
        let started = std::time::Instant::now();
        start_update_with(
            &*host,
            ServiceScope::User,
            &test_systemctl(script, Duration::from_millis(50)),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout must not wait for the full sleep"
        );
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("超时"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 新实例单元名必须同时用于 `show`（重入判定）与 `start --no-block`；
    /// 不能仍然只动 `echo-agent-core-update.service`。
    #[cfg(unix)]
    #[tokio::test]
    async fn instance_unit_is_used_for_query_and_start() {
        let dir = TempDir::new("instance-unit");
        let log = dir.join("calls.log");
        let script = fake_systemctl(
            &dir,
            &format!(
                r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"show "*) echo inactive ;;
  *"start --no-block"*) ;;
  *) echo "unexpected: $*" >&2; exit 64 ;;
esac"#,
                log = log.display()
            ),
        );
        let host = Arc::new(FakeHost::default());
        start_update_with_unit(
            &*host,
            ServiceScope::System,
            "echo-agent-demo-update.service",
            &test_systemctl(script, Duration::from_secs(2)),
        )
        .await;

        let log = calls(&log);
        assert!(
            log.contains("show -p ActiveState --value echo-agent-demo-update.service"),
            "must query the instance updater: {log}"
        );
        assert!(
            log.contains("start --no-block echo-agent-demo-update.service"),
            "must start the instance updater: {log}"
        );
        assert!(
            !log.contains("echo-agent-core-update.service"),
            "must not touch the legacy unit: {log}"
        );

        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            BackendEvent::SelfUpdateStatus { state, message, .. } => {
                assert_eq!(state, "running");
                assert!(
                    message
                        .as_deref()
                        .unwrap_or_default()
                        .contains("echo-agent-demo-update.service"),
                    "{message:?}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── 事务状态读取（显式路径注入；不碰进程环境）──

    #[tokio::test]
    async fn missing_transaction_file_is_idle() {
        let dir = TempDir::new("tx-missing");
        let path = dir.join("transaction-demo.json");
        match read_transaction_status_at(&path).await {
            BackendEvent::SelfUpdateStatus { state, phase, .. } => {
                assert_eq!(state, "idle");
                assert!(phase.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn transaction_file_is_projected_to_status() {
        let dir = TempDir::new("tx-ok");
        let path = dir.join("transaction-demo.json");
        std::fs::write(&path, transaction_doc("committed", "null", "rel-2", "null")).unwrap();
        match read_transaction_status_at(&path).await {
            BackendEvent::SelfUpdateStatus {
                state,
                phase,
                revision,
                message,
                updated_at,
            } => {
                assert_eq!(state, "updated");
                assert_eq!(phase.as_deref(), Some("committed"));
                assert_eq!(revision.as_deref(), Some("rel-2"));
                assert!(message.unwrap().contains("完成"));
                assert!(updated_at.is_some());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 文件存在但 schema 不认识：必须 Error，不能装 idle 或猜一个成功状态。
    #[tokio::test]
    async fn malformed_transaction_file_is_error_not_idle() {
        let dir = TempDir::new("tx-bad");
        let path = dir.join("transaction-demo.json");
        std::fs::write(&path, "{\"schema_version\":1,\"phase\":\"bogus\"}").unwrap();
        match read_transaction_status_at(&path).await {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("事务文件"), "{message}");
                assert!(message.contains("格式无效"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_transaction_file_is_error_not_idle() {
        let dir = TempDir::new("tx-big");
        let path = dir.join("transaction-demo.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_TRANSACTION_BYTES + 1).unwrap();
        match read_transaction_status_at(&path).await {
            BackendEvent::Error { message, .. } => {
                assert!(message.contains("上限"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 处理器显式注入事务文件时，`RequestSelfUpdateStatus` 走新 schema。
    #[tokio::test]
    async fn handler_reads_explicit_transaction_file() {
        let dir = TempDir::new("tx-handler");
        let path = dir.join("transaction-demo.json");
        std::fs::write(
            &path,
            transaction_doc(
                "rolled_back",
                r#"{"code":"switch_failed","message":"x","step":"switch"}"#,
                "rel-2",
                "\"rel-1\"",
            ),
        )
        .unwrap();
        let host = Arc::new(FakeHost::default());
        let h = SelfUpdateHandler::new(host.clone()).with_transaction_path(&path);
        h.handle("RequestSelfUpdateStatus", serde_json::Value::Null)
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        match &events[0] {
            BackendEvent::SelfUpdateStatus { state, message, .. } => {
                assert_eq!(state, "rolled_back");
                assert!(message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("switch_failed"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
