//! 进程级部署控制：独立 Unix socket + 管理命令/写者闸。
//!
//! 只在 `ECHO_DEPLOY_CONTROL_SOCKET` 设置时启用；未设置时组合根完全不创建
//! socket，旧行为不变。路径必须是绝对路径，父目录必须 private（无 group/other
//! 权限）且 owner 与 socket 创建者（本进程）一致。连接只接受 SO_PEERCRED 与
//! socket owner uid 相同的对端。
//!
//! 协议是一行一个 JSON（`\n` 结尾，单行 ≤ 64 KiB），严格反序列化：
//! `{operation: prepare|cancel|status|ready, transaction_id, ...}`。未知字段、
//! 未知 operation、缺失 transaction_id 一律拒绝。
//!
//! `status` / `ready` 还返回 `modules_verified` + `module_state`：真实
//! `ModuleHost` 的 mounted/active 集合（不是 `module.toml` / `modules.toml`
//! 的配置存在）。`ready` 可按请求中的 `required_modules` 判定；无法可靠取得
//! live 证据时明确 `modules_verified=false` 并给原因，绝不把配置当挂载。
//!
//! `prepare` 的边界（ACK 里显式区分）：
//! - **控制内**：管理命令写（global gate 的 write guard）、在飞 turn
//!   （`prepare_upgrade_quiet` 的 draining 闸）、周期会话保存与模块热重载
//!   （`pause_upgrade_writers` guard）、以及 guard 下的最终 checked flush；
//! - **不覆盖**：用户通过 shell/子进程等启动的 arbitrary subprocess writers
//!   ——它们不受本进程锁控制，部署适配器必须自行停止并在 ACK 之外确认。
//!
//! 成功/失败都保留每个 agent 的 upgrade generation 直到显式 `cancel`；失败
//! 也可 cancel。全程不 `retire`、不 cancel 任何一次性 token。

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};
use tracing::{info, warn};

use agent_module_loop::{Agent, AgentManager};

pub const SOCKET_ENV: &str = "ECHO_DEPLOY_CONTROL_SOCKET";
pub const RELEASE_ENV: &str = "ECHO_RELEASE_ID";
/// 单行 JSON 上限（不含行尾换行）。
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// 单个请求从开始到 ACK 的总预算。
pub const TOTAL_BUDGET: Duration = Duration::from_secs(120);
/// 写 ACK 的上限：对端不读/半开连接时不能无限持着维护 guard。
const ACK_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Prepare,
    Cancel,
    Status,
    Ready,
}

/// 严格请求信封。`deny_unknown_fields` + 必填 `transaction_id`。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub operation: Operation,
    pub transaction_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_release: Option<String>,
    /// 仅 `cancel` 使用：prepare ACK 返回的部署层 generation。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    /// `status` / `ready` 可选：调用方要求必须处于真实 mounted+active 的模块 id。
    ///
    /// 缺省为空 = 不额外要求任何模块；`module_state` 仍会返回**完整**的
    /// live mounted/active 清单，供部署适配器按自己的 `required_modules`
    /// 筛选。旧客户端不带本字段，行为不变。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_modules: Vec<String>,
}

impl ControlRequest {
    fn validate(&self) -> Result<(), String> {
        if self.transaction_id.is_empty() {
            return Err("transaction_id must not be empty".into());
        }
        if self.transaction_id.len() > 256 {
            return Err("transaction_id too long (max 256 bytes)".into());
        }
        if let Some(release) = &self.expected_release {
            if release.is_empty() || release.len() > 256 {
                return Err("expected_release must be 1..=256 bytes".into());
            }
        }
        for id in &self.required_modules {
            if id.is_empty() || id.len() > 256 {
                return Err("required_modules entries must be 1..=256 bytes".into());
            }
        }
        if !self.required_modules.is_empty()
            && !matches!(self.operation, Operation::Status | Operation::Ready)
        {
            return Err("required_modules is only valid for status/ready".into());
        }
        match self.operation {
            Operation::Cancel if self.expected_generation.is_none() => {
                Err("cancel requires expected_generation".into())
            }
            Operation::Cancel => Ok(()),
            _ if self.expected_generation.is_some() => {
                Err("expected_generation is only valid for cancel".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorAck {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseInfo {
    pub declared: Option<String>,
    /// 恒 false：只认 `ECHO_RELEASE_ID` 的声明，不做发布身份验证。
    pub identity_verified: bool,
    pub verification: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentAck {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub prepared: bool,
    pub writers_paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelAck {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriterScope {
    /// 控制内写者是否全部暂停（不含 arbitrary subprocess writers）。
    pub all_controlled_writers_paused: bool,
    pub management_commands_quiesced: bool,
    pub covered: Vec<String>,
    pub not_covered: Vec<String>,
    pub note: String,
}

impl WriterScope {
    fn new(paused: bool, management_quiesced: bool) -> Self {
        Self {
            all_controlled_writers_paused: paused,
            management_commands_quiesced: management_quiesced,
            covered: vec![
                "management_command_writes".into(),
                "in_flight_turns".into(),
                "periodic_session_save".into(),
                "module_reload_watcher".into(),
            ],
            not_covered: vec!["user_arbitrary_subprocesses".into()],
            note: "user-spawned arbitrary subprocess writers are outside the process write locks; \
                   the deployment adapter must stop them separately"
                .into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RetirementAck {
    pub checked: bool,
    pub failed: Vec<String>,
    pub retiring_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一个模块在**本进程当前时刻**的真实状态（来自 live `ModuleHost`）。
///
/// `enabled` 是注册表的启用开关（配置/期望态），**不是**挂载证明：
/// `enabled = true && mounted = false` 是合法且常见的事实（定义已注册但
/// 从未挂载、挂载因依赖缺失失败、被人主动停用等）。部署就绪只认
/// `mounted` / `active`。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModuleState {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub runtime: String,
    pub builtin: bool,
    /// 配置/期望态（`ModuleRegistry` 的 enabled 开关），不是挂载证明。
    pub enabled: bool,
    /// 真实挂载事实：`ModuleRegistry::is_mounted(id)` 的 active 集合成员。
    pub mounted: bool,
    /// `mounted && error.is_none() && state ∈ {""(进程内), "ready"}`。
    pub active: bool,
    /// 子进程运行态（`pending` / `ready` / `draining` / `stopped` /
    /// `failed`）；空 = 进程内模块（没有独立生命周期）。
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_services: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_services: Vec<String>,
}

/// Core 对模块挂载事实的证据快照。
///
/// 关键区别：
/// - `modules` 是 live 注册表的完整清单（只来自 `ModuleHost::descriptors()`；
///   `ModuleDefinition` 里"配置提到过"的条目不会凭空出现）；
/// - `mounted` 只取 `ModuleRegistry::is_mounted` 的真实挂载集合；
/// - `active` 进一步排除 `error` / 非 ready 的子进程运行态。
///
/// `verified = false` 时表示**无法可靠取得**这份证据（例如 ModuleHost 未
/// 接线、注册表为空）。此时 `reason` 给出原因，`missing_required` 故意留空
/// ——不能把"没观测到"伪装成"确认缺失"或"确认挂载"。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModuleEvidence {
    /// 证据来源（固定 `module_host_registry`）。
    pub source: String,
    /// 本进程是否可靠取得 live 挂载态。
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 本次请求要求必须 active 的模块 id（空 = 调用方未声明）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    /// 真实挂载集合（registry active set 成员；mounted ≠ active）。
    pub mounted: Vec<String>,
    /// 真实 active 集合（mounted + 无 error + 运行态 ready）。
    pub active: Vec<String>,
    /// 有 error 或运行态为 failed/stopped 的模块（诊断用，含原因）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// 要求却未 active 的模块 id（`verified = false` 时为空）。
    pub missing_required: Vec<String>,
    /// `missing_required` 的可读原因（含"未注册"/"未挂载"/"failed"）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_errors: Vec<String>,
    /// live 注册表里的全部模块（注册序 = 配置行序）。
    pub modules: Vec<ModuleState>,
}

/// 内部用的 live 快照：与 [`ModuleEvidence`] 分开，避免"请求要求"混进
/// 观测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModuleSnapshot {
    verified: bool,
    reason: Option<String>,
    modules: Vec<ModuleState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyChecks {
    pub configured_enabled_personas_running: bool,
    pub trunks_loaded: bool,
    pub no_retiring_personas: bool,
    pub release_matches_expected: bool,
    /// live 模块挂载证据是否可靠取得。
    pub modules_verified: bool,
    /// 请求声明的 required_modules 是否全部处于真实 mounted+active。
    pub required_modules_mounted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_personas: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trunk_errors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_modules: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_errors: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepared: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance_held: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_cancel: Option<bool>,
    pub release: ReleaseInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_scope: Option<WriterScope>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentAck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cancel_results: Vec<CancelAck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_register_retirements: Option<RetirementAck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<ReadyChecks>,
    /// 本次响应里的 live 模块证据是否可靠取得（`status` / `ready` 均返回）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules_verified: Option<bool>,
    /// live mounted/active 模块清单（`status` / `ready` 均返回）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_state: Option<ModuleEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorAck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Default for ReleaseInfo {
    fn default() -> Self {
        ReleaseInfo {
            declared: None,
            identity_verified: false,
            verification: "env_echo_release_id_declaration_only",
        }
    }
}

impl ControlResponse {
    fn base(operation: Option<Operation>, transaction_id: Option<String>) -> Self {
        Self {
            ok: true,
            operation,
            transaction_id,
            ..Default::default()
        }
    }

    fn fail(
        operation: Option<Operation>,
        transaction_id: Option<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            ok: false,
            error: Some(ErrorAck {
                code: code.into(),
                message: message.into(),
            }),
            ..Self::base(operation, transaction_id)
        }
    }
}

/// 部署控制处理器。socket 层只依赖本 trait，便于用 stub 测 Unix 路径。
#[async_trait::async_trait]
pub trait DeploymentHandler: Send + Sync {
    async fn handle(&self, request: ControlRequest, deadline: Instant) -> ControlResponse;
}

/// 单个 agent 的升级准备结果（真实 `Agent` / 测试桩共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreparedUpgrade {
    generation: u64,
    memory_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrepareFailure {
    /// 未建立维护态：没有可 cancel 的代数。
    NoMaintenance(String),
    /// 维护态已建立并保留：必须用该代数 cancel 才能恢复接单。
    Maintenance { generation: u64, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelRelease {
    Released,
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CancelFailure {
    /// 这一代正在退场/停机：不能重开，也不需要重开。
    Retiring(String),
    /// 其它失败（测试桩 / 未来错误）：维护态可能仍在，保留以便重试。
    #[allow(dead_code)] // 生产实现只有 Retiring；测试桩覆盖"其它失败"分支。
    Other(String),
}

/// 从 live `ModuleHost` 取得真实模块态。
/// 关键点：**只认 `descriptors()`（live 注册表）+ `registry.is_mounted()`**。
/// `set_definitions()` 里的定义、`modules.toml` 的配置行都不会单独出现在
/// 这里。`descriptors().enabled` 只是启用开关，不能当挂载证明。
fn snapshot_module_host(host: &agent_module_loop::modules::ModuleHost) -> ModuleSnapshot {
    let modules: Vec<ModuleState> = host
        .descriptors()
        .into_iter()
        .map(|descriptor| {
            // `is_mounted` 查的是注册表的 active 集合（真实挂载），
            // 与 descriptor.enabled（配置开关）正交。
            let mounted = host.registry.is_mounted(&descriptor.id);
            let state = descriptor.state;
            let active =
                mounted && descriptor.error.is_none() && (state.is_empty() || state == "ready");
            ModuleState {
                id: descriptor.id,
                name: descriptor.name,
                version: descriptor.version,
                kind: descriptor.kind,
                runtime: descriptor.runtime,
                builtin: descriptor.builtin,
                enabled: descriptor.enabled,
                mounted,
                active,
                state,
                error: descriptor.error,
                required_services: descriptor.required_services,
                pending_services: descriptor.pending_services,
            }
        })
        .collect();
    // 空注册表 = 无法证明"挂载了哪些"：正常 Core 至少挂内置模块。宁可
    // verified=false，也不能把"没看到任何模块"报成"确认全部就绪"。
    let verified = !modules.is_empty();
    let reason = (!verified).then(|| {
        "module host registry contains no registered modules; a live mounted/active set \
         cannot be proven (ModuleHost not attached or empty catalog)"
            .to_string()
    });
    ModuleSnapshot {
        verified,
        reason,
        modules,
    }
}

impl ModuleEvidence {
    /// 把 live 快照与本请求的 `required_modules` 合成为响应证据。
    fn from_snapshot(snapshot: Option<ModuleSnapshot>, required: &[String]) -> Self {
        let Some(snapshot) = snapshot else {
            return Self {
                source: "module_host_registry".into(),
                verified: false,
                reason: Some("deployment agent did not expose a live module host snapshot".into()),
                required: required.to_vec(),
                mounted: Vec::new(),
                active: Vec::new(),
                failed: Vec::new(),
                missing_required: Vec::new(),
                required_errors: Vec::new(),
                modules: Vec::new(),
            };
        };
        let mounted = snapshot
            .modules
            .iter()
            .filter(|module| module.mounted)
            .map(|module| module.id.clone())
            .collect::<Vec<_>>();
        let active = snapshot
            .modules
            .iter()
            .filter(|module| module.active)
            .map(|module| module.id.clone())
            .collect::<Vec<_>>();
        let failed = snapshot
            .modules
            .iter()
            .filter_map(|module| {
                if module.error.is_none() && !matches!(module.state.as_str(), "failed" | "stopped")
                {
                    return None;
                }
                Some(match &module.error {
                    Some(error) => format!("{}: {error}", module.id),
                    None => format!("{}: state={}", module.id, module.state),
                })
            })
            .collect::<Vec<_>>();
        // `verified = false` 时观测本身不可靠：不要声称任何 required 模块
        // "缺失"（那会是另一种假证），只把原因放在 reason。
        let mut missing_required = Vec::new();
        let mut required_errors = Vec::new();
        if snapshot.verified {
            for id in required {
                match snapshot.modules.iter().find(|module| &module.id == id) {
                    None => {
                        missing_required.push(id.clone());
                        required_errors.push(format!("{id}: not registered in module host"));
                    }
                    Some(module) if !module.mounted => {
                        missing_required.push(id.clone());
                        required_errors.push(format!("{id}: registered but not mounted"));
                    }
                    Some(module) if !module.active => {
                        missing_required.push(id.clone());
                        required_errors.push(match &module.error {
                            Some(error) => format!("{id}: mounted but inactive: {error}"),
                            None => format!("{id}: mounted but state={}", module.state),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
        Self {
            source: "module_host_registry".into(),
            verified: snapshot.verified,
            reason: snapshot.reason,
            required: required.to_vec(),
            mounted,
            active,
            failed,
            missing_required,
            required_errors,
            modules: snapshot.modules,
        }
    }
}

/// 部署准备需要的单 agent 能力面。生产实现是 [`Agent`]；测试桩实现同一
/// 语义即可覆盖 prepare→pause→final flush→cancel，不需要真实 LLM/磁盘。
#[async_trait::async_trait]
trait DeploymentAgent: Send + Sync {
    async fn prepare_upgrade_quiet(
        &self,
        timeout: Duration,
    ) -> Result<PreparedUpgrade, PrepareFailure>;

    async fn pause_upgrade_writers(&self, timeout: Duration) -> Option<OwnedRwLockWriteGuard<()>>;

    async fn cancel_upgrade(&self, generation: u64) -> Result<CancelRelease, CancelFailure>;

    async fn save_now_checked(&self) -> Result<(), String>;

    async fn ensure_trunk_loaded(&self) -> Result<(), String>;

    /// live 模块挂载态快照。`None` = 该实现无法提供（测试桩 / 未接线），
    /// 部署层会明确报 `modules_verified=false`，不把它当成"空挂载"。
    fn module_snapshot(&self) -> Option<ModuleSnapshot>;
}

#[async_trait::async_trait]
impl DeploymentAgent for Agent {
    async fn prepare_upgrade_quiet(
        &self,
        timeout: Duration,
    ) -> Result<PreparedUpgrade, PrepareFailure> {
        match Agent::prepare_upgrade_quiet(self, timeout).await {
            Ok(prepared) => Ok(PreparedUpgrade {
                generation: prepared.generation,
                memory_only: matches!(
                    prepared.flush,
                    agent_module_loop::agent::upgrade::UpgradeFlushOutcome::CheckedOk {
                        memory_only: true
                    }
                ),
            }),
            Err(error) => {
                let message = error.to_string();
                // `AlreadyPrepared` 的活跃代数不在 `generation()` 里，但它是
                // 唯一可 cancel 的代数——必须带回给部署层。
                let generation = match &error {
                    agent_module_loop::agent::upgrade::UpgradeError::AlreadyPrepared { active } => {
                        Some(*active)
                    }
                    other => other.generation(),
                };
                Err(match generation {
                    Some(generation) => PrepareFailure::Maintenance {
                        generation,
                        message,
                    },
                    None => PrepareFailure::NoMaintenance(message),
                })
            }
        }
    }

    async fn pause_upgrade_writers(&self, timeout: Duration) -> Option<OwnedRwLockWriteGuard<()>> {
        Agent::pause_upgrade_writers(self, timeout).await
    }

    async fn cancel_upgrade(&self, generation: u64) -> Result<CancelRelease, CancelFailure> {
        match Agent::cancel_upgrade(self, generation).await {
            Ok(release) => Ok(match release {
                agent_module_loop::agent::upgrade::UpgradeRelease::Released => {
                    CancelRelease::Released
                }
                agent_module_loop::agent::upgrade::UpgradeRelease::NoMatch => {
                    CancelRelease::NoMatch
                }
            }),
            Err(error) => Err(CancelFailure::Retiring(error.to_string())),
        }
    }

    async fn save_now_checked(&self) -> Result<(), String> {
        self.trunk
            .save_now_checked()
            .await
            .map_err(|error| error.to_string())
    }

    async fn ensure_trunk_loaded(&self) -> Result<(), String> {
        self.trunk
            .ensure_loaded_or_err()
            .await
            .map_err(|error| error.to_string())
    }
    fn module_snapshot(&self) -> Option<ModuleSnapshot> {
        // `Agent::module_host()` 在未注入共享宿主时返回一个空的新宿主——
        // `snapshot_module_host` 会因此判 `verified=false`，不会假报"没有
        // 模块 = 全部就绪"。
        Some(snapshot_module_host(&self.module_host()))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaInfo {
    id: String,
    enabled: bool,
    running: bool,
}

/// 部署准备需要的 manager 能力面。生产实现是 [`AgentManager`]；测试桩
/// 可以直接构造空人格、已 Failed 退场等边界，不碰配置/生命周期。
#[async_trait::async_trait]
trait DeploymentManager: Send + Sync {
    fn retiring_ids(&self) -> Vec<String>;
    async fn await_all_retirements(&self) -> Vec<String>;
    fn infos(&self) -> Vec<PersonaInfo>;
    fn all(&self) -> Vec<(String, Arc<dyn DeploymentAgent>)>;
    fn get(&self, id: &str) -> Option<Arc<dyn DeploymentAgent>>;
}

#[async_trait::async_trait]
impl DeploymentManager for AgentManager {
    fn retiring_ids(&self) -> Vec<String> {
        AgentManager::retiring_ids(self)
    }

    async fn await_all_retirements(&self) -> Vec<String> {
        AgentManager::await_all_retirements(self).await
    }

    fn infos(&self) -> Vec<PersonaInfo> {
        AgentManager::infos(self)
            .into_iter()
            .map(|info| PersonaInfo {
                id: info.id,
                enabled: info.enabled,
                running: info.running,
            })
            .collect()
    }

    fn all(&self) -> Vec<(String, Arc<dyn DeploymentAgent>)> {
        AgentManager::all(self)
            .into_iter()
            .map(|running| (running.id, running.agent as Arc<dyn DeploymentAgent>))
            .collect()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn DeploymentAgent>> {
        AgentManager::get(self, id).map(|running| running.agent as Arc<dyn DeploymentAgent>)
    }
}

struct PendingAgent {
    ack: AgentAck,
    agent: Arc<dyn DeploymentAgent>,
    writer_guard: Option<OwnedRwLockWriteGuard<()>>,
}

struct Maintenance {
    generation: u64,
    transaction_id: String,
    records: Vec<PendingAgent>,
    /// 必须最后 drop：先恢复写者，再放管理命令进闸。
    _gate: OwnedRwLockWriteGuard<()>,
}

#[derive(Clone, Default)]
struct View {
    phase: String,
    generation: Option<u64>,
    transaction_id: Option<String>,
    prepared: bool,
    maintenance_held: bool,
    agents: Vec<AgentAck>,
    retirements: Option<RetirementAck>,
    writer_scope: Option<WriterScope>,
}

/// 真实 Core 的部署处理器：持有全局管理命令闸与各 agent 的写者 guard。
pub struct CoreDeploymentHandler {
    gate: Arc<RwLock<()>>,
    core_agent: Arc<dyn DeploymentAgent>,
    manager: Arc<dyn DeploymentManager>,
    release_id: Option<String>,
    next_generation: AtomicU64,
    maintenance: tokio::sync::Mutex<Option<Maintenance>>,
    /// 串行化 prepare/cancel；status 只读 View 不排队。
    op_lock: tokio::sync::Mutex<()>,
    view: Mutex<View>,
}

impl CoreDeploymentHandler {
    pub fn new(gate: Arc<RwLock<()>>, core_agent: Arc<Agent>, manager: Arc<AgentManager>) -> Self {
        Self::with_backends(gate, core_agent, manager)
    }

    fn with_backends(
        gate: Arc<RwLock<()>>,
        core_agent: Arc<dyn DeploymentAgent>,
        manager: Arc<dyn DeploymentManager>,
    ) -> Self {
        let release_id = std::env::var(RELEASE_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        Self {
            gate,
            core_agent,
            manager,
            release_id,
            next_generation: AtomicU64::new(1),
            maintenance: tokio::sync::Mutex::new(None),
            op_lock: tokio::sync::Mutex::new(()),
            view: Mutex::new(View {
                phase: "idle".into(),
                ..Default::default()
            }),
        }
    }

    fn release_info(&self) -> ReleaseInfo {
        ReleaseInfo {
            declared: self.release_id.clone(),
            ..Default::default()
        }
    }

    fn check_expected_release(&self, request: &ControlRequest) -> Result<(), String> {
        let Some(expected) = &request.expected_release else {
            return Ok(());
        };
        match &self.release_id {
            Some(declared) if declared == expected => Ok(()),
            Some(declared) => Err(format!(
                "release mismatch: expected {expected:?}, declared {declared:?}"
            )),
            None => Err(format!(
                "release identity not declared: set {RELEASE_ENV} before starting Core"
            )),
        }
    }

    fn view(&self) -> std::sync::MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 汇总 off-register 退场：Running 等完，Failed 如实返回。Total budget
    /// 约束在 deadline 内。
    async fn check_retirements(&self, deadline: Instant) -> RetirementAck {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut ack = if remaining.is_zero() {
            RetirementAck {
                checked: false,
                failed: Vec::new(),
                retiring_ids: self.manager.retiring_ids(),
                error: Some("timeout budget exhausted before retirement wait".into()),
            }
        } else {
            match tokio::time::timeout(remaining, self.manager.await_all_retirements()).await {
                Ok(failed) => RetirementAck {
                    checked: true,
                    failed,
                    retiring_ids: Vec::new(),
                    error: None,
                },
                Err(_) => RetirementAck {
                    checked: false,
                    failed: Vec::new(),
                    retiring_ids: self.manager.retiring_ids(),
                    error: Some("timed out waiting for off-register retirements".into()),
                },
            }
        };
        ack.retiring_ids = self.manager.retiring_ids();
        ack
    }

    async fn prepare(&self, request: ControlRequest, deadline: Instant) -> ControlResponse {
        let operation = Some(Operation::Prepare);
        let tx = Some(request.transaction_id.clone());
        if let Err(message) = self.check_expected_release(&request) {
            return ControlResponse::fail(operation, tx, "release_mismatch", message);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ControlResponse::fail(operation, tx, "timeout", "budget exhausted");
        }
        let _serial = match tokio::time::timeout(remaining, self.op_lock.lock()).await {
            Ok(guard) => guard,
            Err(_) => {
                return ControlResponse::fail(
                    operation,
                    tx,
                    "timeout",
                    "timed out waiting for another deployment operation",
                )
            }
        };
        {
            let active = self.maintenance.lock().await;
            if let Some(maintenance) = active.as_ref() {
                let view = self.view().clone();
                let mut response = ControlResponse::base(operation, tx);
                response.phase = Some(view.phase);
                response.prepared = Some(view.prepared);
                response.maintenance_held = Some(true);
                response.can_cancel = Some(true);
                response.generation = Some(maintenance.generation);
                response.release = self.release_info();
                response.writer_scope = view.writer_scope;
                response.agents = view.agents;
                response.off_register_retirements = view.retirements;
                response.ok = view.prepared;
                response.message = Some("another deployment preparation is already active".into());
                if !view.prepared {
                    response.error = Some(ErrorAck {
                        code: "maintenance_not_ready".into(),
                        message: "the active preparation is not fully prepared; \
                                  cancel it with expected_generation before switching binaries"
                            .into(),
                    });
                }
                return response;
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ControlResponse::fail(operation, tx, "timeout", "budget exhausted");
        }
        let gate_guard =
            match tokio::time::timeout(remaining, self.gate.clone().write_owned()).await {
                Ok(guard) => guard,
                Err(_) => {
                    return ControlResponse::fail(
                        operation,
                        tx,
                        "timeout",
                        "timed out waiting for management commands to quiesce",
                    )
                }
            };

        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut view = self.view();
            view.phase = "preparing".into();
            view.generation = Some(generation);
            view.transaction_id = Some(request.transaction_id.clone());
            view.prepared = false;
            view.maintenance_held = true;
            view.agents.clear();
            view.retirements = None;
            view.writer_scope = Some(WriterScope::new(false, true));
        }

        // 先等 off-register 退场收尾，修复此前只枚举 manager.all() 的 Failed
        // 漏报。Running 会等，Failed 如实汇总（不会自己变好）。
        let mut retirements = self.check_retirements(deadline).await;
        if !retirements.checked
            || !retirements.failed.is_empty()
            || !retirements.retiring_ids.is_empty()
        {
            drop(gate_guard);
            let mut view = self.view();
            view.phase = "idle".into();
            view.generation = None;
            view.transaction_id = None;
            view.maintenance_held = false;
            view.retirements = Some(retirements.clone());
            view.writer_scope = Some(WriterScope::new(false, false));
            let mut response = ControlResponse::fail(
                operation,
                tx,
                "retiring_in_progress",
                "off-register persona retirements are not clean; deploy refused",
            );
            response.off_register_retirements = Some(retirements);
            return response;
        }

        let infos = self.manager.infos();
        let mut targets: Vec<(String, Arc<dyn DeploymentAgent>)> =
            self.manager.all().into_iter().collect();
        let mut records: Vec<PendingAgent> = Vec::new();
        for info in infos.iter().filter(|info| info.enabled && !info.running) {
            // `infos` 与 `all()` 来自同一 manager，但快照有先后：若实例其实
            // 已在运行，交给下面的 all() 统一 prepare，避免同 id 两条记录。
            if self.manager.get(&info.id).is_some() {
                continue;
            }
            records.push(PendingAgent {
                ack: AgentAck {
                    id: info.id.clone(),
                    generation: None,
                    prepared: false,
                    writers_paused: false,
                    flush: None,
                    memory_only: None,
                    error: Some("configured enabled persona is not running".into()),
                },
                agent: Arc::clone(&self.core_agent),
                writer_guard: None,
            });
        }
        // core 永远参与（它承接管理面命令，不是 persona）。
        targets.push(("__core".into(), Arc::clone(&self.core_agent)));
        targets.sort_by(|a, b| a.0.cmp(&b.0));
        targets.dedup_by(|a, b| a.0 == b.0);

        for (id, agent) in targets {
            let pending = prepare_one(id, agent, deadline).await;
            records.push(pending);
            let mut view = self.view();
            view.agents = records.iter().map(|record| record.ack.clone()).collect();
            view.writer_scope = Some(WriterScope::new(
                !records.is_empty() && records.iter().all(|record| record.ack.prepared),
                true,
            ));
        }

        let retiring_ids = self.manager.retiring_ids();
        retirements.retiring_ids = retiring_ids.clone();
        let all_controlled =
            !records.is_empty() && records.iter().all(|record| record.ack.prepared);
        let prepared = all_controlled
            && retirements.checked
            && retirements.failed.is_empty()
            && retiring_ids.is_empty();

        {
            let mut view = self.view();
            view.phase = if prepared { "prepared" } else { "failed" }.into();
            view.prepared = prepared;
            view.maintenance_held = true;
            view.agents = records.iter().map(|record| record.ack.clone()).collect();
            view.retirements = Some(retirements.clone());
            view.writer_scope = Some(WriterScope::new(all_controlled, true));
        }
        {
            let mut active = self.maintenance.lock().await;
            *active = Some(Maintenance {
                generation,
                transaction_id: request.transaction_id.clone(),
                records,
                _gate: gate_guard,
            });
        }

        let mut response = ControlResponse::base(operation, tx);
        response.phase = Some(if prepared { "prepared" } else { "failed" }.into());
        response.generation = Some(generation);
        response.prepared = Some(prepared);
        response.maintenance_held = Some(true);
        response.can_cancel = Some(true);
        response.release = self.release_info();
        response.writer_scope = Some(WriterScope::new(all_controlled, true));
        response.agents = self.view().agents.clone();
        response.off_register_retirements = Some(retirements);
        response.requires = vec![
            "off_register_retirements_clean".into(),
            "external_stop_user_arbitrary_subprocesses".into(),
        ];
        response.message = Some(if prepared {
            "controlled writers paused, in-flight turns drained, checked flush confirmed; \
             maintenance held until cancel; user subprocesses are NOT covered"
                .into()
        } else if all_controlled {
            "controlled writers are paused, but off-register retirements are not clean; \
             call cancel with expected_generation to reopen"
                .into()
        } else {
            "maintenance recorded per agent; call cancel with expected_generation to reopen; \
             not all controlled writers could be paused"
                .into()
        });
        // `ok` 必须与 `prepared` 一致：否则部署适配器会把半准备（writer 未停/
        // final flush 失败/退场不干净）当成成功并切换二进制。失败时仍返回
        // generation + maintenance_held + can_cancel，让调用方能显式回滚。
        if !prepared {
            response.ok = false;
            response.error = Some(ErrorAck {
                code: "prepare_incomplete".into(),
                message: "not all controlled writers are prepared; maintenance is retained; \
                          cancel with expected_generation before switching binaries"
                    .into(),
            });
        }
        response
    }

    async fn cancel(&self, request: ControlRequest, deadline: Instant) -> ControlResponse {
        let operation = Some(Operation::Cancel);
        let tx = Some(request.transaction_id.clone());
        if let Err(message) = self.check_expected_release(&request) {
            return ControlResponse::fail(operation, tx, "release_mismatch", message);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ControlResponse::fail(operation, tx, "timeout", "budget exhausted");
        }
        let _serial = match tokio::time::timeout(remaining, self.op_lock.lock()).await {
            Ok(guard) => guard,
            Err(_) => {
                return ControlResponse::fail(
                    operation,
                    tx,
                    "timeout",
                    "timed out waiting for another deployment operation",
                )
            }
        };
        let mut active = self.maintenance.lock().await;
        let Some(maintenance) = active.as_ref() else {
            return ControlResponse::fail(
                operation,
                tx,
                "no_active_maintenance",
                "no deployment preparation is held",
            );
        };
        if request.transaction_id != maintenance.transaction_id {
            return ControlResponse::fail(
                operation,
                tx,
                "transaction_mismatch",
                format!("active transaction is {:?}", maintenance.transaction_id),
            );
        }
        if request.expected_generation != Some(maintenance.generation) {
            return ControlResponse::fail(
                operation,
                tx,
                "generation_mismatch",
                format!("active deploy generation is {}", maintenance.generation),
            );
        }
        let maintenance = active.as_ref().expect("checked above");
        {
            let mut view = self.view();
            view.phase = "cancelling".into();
        }
        let mut cancel_results = Vec::new();
        for record in &maintenance.records {
            let result = match record.ack.generation {
                Some(generation) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        CancelAck {
                            id: record.ack.id.clone(),
                            generation: Some(generation),
                            result: "timeout".into(),
                            error: Some("timeout budget exhausted before cancel".into()),
                        }
                    } else {
                        match tokio::time::timeout(
                            remaining,
                            record.agent.cancel_upgrade(generation),
                        )
                        .await
                        {
                            Ok(Ok(release)) => CancelAck {
                                id: record.ack.id.clone(),
                                generation: Some(generation),
                                result: match release {
                                    CancelRelease::Released => "released".into(),
                                    CancelRelease::NoMatch => "no_match".into(),
                                },
                                error: None,
                            },
                            Ok(Err(error)) => {
                                let (result, message) = match error {
                                    // 这一代正在退场/停机：不能也不需要重开，
                                    // guard 可以释放（进程随后退出）。
                                    CancelFailure::Retiring(message) => ("retiring", message),
                                    // 其它失败保留 maintenance，供幂等重试。
                                    CancelFailure::Other(message) => ("error", message),
                                };
                                CancelAck {
                                    id: record.ack.id.clone(),
                                    generation: Some(generation),
                                    result: result.into(),
                                    error: Some(message),
                                }
                            }
                            Err(_) => CancelAck {
                                id: record.ack.id.clone(),
                                generation: Some(generation),
                                result: "timeout".into(),
                                error: Some("cancel timed out".into()),
                            },
                        }
                    }
                }
                None => CancelAck {
                    id: record.ack.id.clone(),
                    generation: None,
                    result: "no_generation".into(),
                    error: None,
                },
            };
            cancel_results.push(result);
        }
        let fully_released = cancel_results.iter().all(|result| {
            matches!(
                result.result.as_str(),
                "released" | "no_match" | "no_generation"
            )
        });
        let all_cleared = cancel_results.iter().all(|result| {
            matches!(
                result.result.as_str(),
                "released" | "no_match" | "retiring" | "no_generation"
            )
        });
        if all_cleared {
            let released = active.take().expect("checked above");
            drop(active);
            drop(released); // records（writer guards）先 drop，gate guard 最后
            let mut view = self.view();
            view.phase = "idle".into();
            view.generation = None;
            view.transaction_id = None;
            view.prepared = false;
            view.maintenance_held = false;
            view.agents.clear();
            view.retirements = None;
            view.writer_scope = Some(WriterScope::new(false, false));
            let mut response = ControlResponse::base(operation, tx);
            response.phase = Some("idle".into());
            response.generation = None;
            response.prepared = Some(false);
            response.maintenance_held = Some(false);
            response.can_cancel = Some(false);
            response.release = self.release_info();
            response.writer_scope = Some(WriterScope::new(false, false));
            response.cancel_results = cancel_results;
            response.ok = fully_released;
            if fully_released {
                response.message =
                    Some("maintenance released; management commands and turns may resume".into());
            } else {
                response.error = Some(ErrorAck {
                    code: "agent_retiring".into(),
                    message: "maintenance released, but at least one agent is retiring/shutting \
                              down and refused to reopen its turn gate; do not reuse this process"
                        .into(),
                });
            }
            return response;
        }
        // 至少一个 cancel 超时：保留整个 maintenance（可幂等重试），不释放
        // gate/writer guard，避免留下没有代数可 cancel 的 draining 状态。
        let (generation, transaction_id, agents, paused) = {
            let maintenance = active.as_ref().expect("checked above");
            (
                maintenance.generation,
                maintenance.transaction_id.clone(),
                maintenance
                    .records
                    .iter()
                    .map(|record| record.ack.clone())
                    .collect::<Vec<_>>(),
                maintenance.records.iter().all(|record| record.ack.prepared),
            )
        };
        drop(active);
        {
            let mut view = self.view();
            view.phase = "cancel_pending".into();
            view.generation = Some(generation);
            view.transaction_id = Some(transaction_id);
            view.prepared = false;
            view.maintenance_held = true;
            view.agents = agents.clone();
            view.writer_scope = Some(WriterScope::new(paused, true));
        }
        let mut response = ControlResponse::base(operation, tx);
        response.phase = Some("cancel_pending".into());
        response.generation = Some(generation);
        response.prepared = Some(false);
        response.maintenance_held = Some(true);
        response.can_cancel = Some(true);
        response.release = self.release_info();
        response.writer_scope = Some(WriterScope::new(paused, true));
        response.agents = agents;
        response.cancel_results = cancel_results;
        response.ok = false;
        response.error = Some(ErrorAck {
            code: "cancel_incomplete".into(),
            message: "some cancels did not complete; maintenance and writer guards are retained; \
                      retry cancel with the same expected_generation"
                .into(),
        });
        response
    }

    fn status(&self, request: &ControlRequest) -> ControlResponse {
        let view = self.view().clone();
        let mut response = ControlResponse::base(
            Some(Operation::Status),
            Some(request.transaction_id.clone()),
        );
        response.phase = Some(view.phase);
        response.generation = view.generation;
        response.prepared = Some(view.prepared);
        response.maintenance_held = Some(view.maintenance_held);
        response.can_cancel = Some(view.maintenance_held);
        response.release = self.release_info();
        response.writer_scope = Some(
            view.writer_scope
                .clone()
                .unwrap_or_else(|| WriterScope::new(false, false)),
        );
        response.agents = view.agents;
        response.off_register_retirements = view.retirements;
        // `status` 不做 persona/trunk 检查（只读、无等待），但必须给出**真实**
        // 模块态证据：部署适配器在决定是否继续前可以先核对 required_modules，
        // 而不是只信配置文件里"声明过"。
        let module_evidence = ModuleEvidence::from_snapshot(
            self.core_agent.module_snapshot(),
            &request.required_modules,
        );
        response.modules_verified = Some(module_evidence.verified);
        response.module_state = Some(module_evidence);
        response
    }

    async fn ready(&self, request: &ControlRequest, deadline: Instant) -> ControlResponse {
        let operation = Some(Operation::Ready);
        let tx = Some(request.transaction_id.clone());
        let release_check = self.check_expected_release(request);
        let release_matches = release_check.is_ok();
        let infos = self.manager.infos();
        let mut missing = Vec::new();
        let mut trunk_errors = Vec::new();
        for info in infos.iter().filter(|info| info.enabled) {
            let Some(running) = self.manager.get(&info.id) else {
                missing.push(info.id.clone());
                continue;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                trunk_errors.push(format!("{}: timeout budget exhausted", info.id));
                continue;
            }
            match tokio::time::timeout(remaining, running.ensure_trunk_loaded()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => trunk_errors.push(format!("{}: {error}", info.id)),
                Err(_) => trunk_errors.push(format!("{}: trunk load timed out", info.id)),
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let core_trunk_ok = if remaining.is_zero() {
            trunk_errors.push("__core: timeout budget exhausted".into());
            false
        } else {
            match tokio::time::timeout(remaining, self.core_agent.ensure_trunk_loaded()).await {
                Ok(Ok(())) => true,
                Ok(Err(error)) => {
                    trunk_errors.push(format!("__core: {error}"));
                    false
                }
                Err(_) => {
                    trunk_errors.push("__core: trunk load timed out".into());
                    false
                }
            }
        };
        let retirements = self.check_retirements(deadline).await;
        let no_retiring = retirements.checked
            && retirements.failed.is_empty()
            && retirements.retiring_ids.is_empty();
        // 模块就绪检查：只认 live ModuleHost 的 mounted+active 事实。
        // `modules_verified=false` 直接判 not ready（无法证明 ≠ 通过）。
        let module_evidence = ModuleEvidence::from_snapshot(
            self.core_agent.module_snapshot(),
            &request.required_modules,
        );
        let modules_verified = module_evidence.verified;
        let required_modules_mounted =
            modules_verified && module_evidence.missing_required.is_empty();
        let ready = missing.is_empty()
            && trunk_errors.is_empty()
            && core_trunk_ok
            && no_retiring
            && release_matches
            && required_modules_mounted;
        let mut response = ControlResponse::base(operation, tx);
        response.release = self.release_info();
        response.ready = Some(ready);
        response.checks = Some(ReadyChecks {
            configured_enabled_personas_running: missing.is_empty(),
            trunks_loaded: trunk_errors.is_empty() && core_trunk_ok,
            no_retiring_personas: no_retiring,
            release_matches_expected: release_matches,
            modules_verified,
            required_modules_mounted,
            missing_personas: (!missing.is_empty()).then_some(missing),
            trunk_errors: (!trunk_errors.is_empty()).then_some(trunk_errors),
            missing_modules: (!module_evidence.missing_required.is_empty())
                .then_some(module_evidence.missing_required.clone()),
            module_errors: (!module_evidence.required_errors.is_empty())
                .then_some(module_evidence.required_errors.clone()),
        });
        response.off_register_retirements = Some(retirements);
        response.requires = vec!["external_release_identity_check".into()];
        response.modules_verified = Some(module_evidence.verified);
        response.module_state = Some(module_evidence);
        response.message = Some(match release_check {
            Err(message) => message,
            Ok(()) => "ready means configured enabled personas are running, trunks load, and \
                       every requested required_module is in the live module host active set; \
                       release identity is only the ECHO_RELEASE_ID declaration"
                .into(),
        });
        response
    }
}

#[async_trait::async_trait]
impl DeploymentHandler for CoreDeploymentHandler {
    async fn handle(&self, request: ControlRequest, deadline: Instant) -> ControlResponse {
        match request.operation {
            Operation::Prepare => self.prepare(request, deadline).await,
            Operation::Cancel => self.cancel(request, deadline).await,
            Operation::Status => self.status(&request),
            Operation::Ready => self.ready(&request, deadline).await,
        }
    }
}

async fn prepare_one(
    id: String,
    agent: Arc<dyn DeploymentAgent>,
    deadline: Instant,
) -> PendingAgent {
    let mut pending = PendingAgent {
        ack: AgentAck {
            id,
            generation: None,
            prepared: false,
            writers_paused: false,
            flush: None,
            memory_only: None,
            error: None,
        },
        agent: Arc::clone(&agent),
        writer_guard: None,
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        pending.ack.error = Some("timeout budget exhausted before prepare".into());
        return pending;
    }
    // 不套外层 timeout：`prepare_upgrade_quiet` 自己按 remaining 收敛，失败时
    // 仍会带回 generation（必须记录以便 cancel）。外层先超时会丢掉这个代数。
    let prepared = match agent.prepare_upgrade_quiet(remaining).await {
        Ok(prepared) => prepared,
        Err(PrepareFailure::Maintenance {
            generation,
            message,
        }) => {
            pending.ack.generation = Some(generation);
            pending.ack.error = Some(format!("prepare failed: {message}"));
            return pending;
        }
        Err(PrepareFailure::NoMaintenance(message)) => {
            pending.ack.error = Some(format!("prepare failed: {message}"));
            return pending;
        }
    };
    pending.ack.generation = Some(prepared.generation);
    pending.ack.memory_only = Some(prepared.memory_only);
    // 这是 pause writers 之前的中间 flush。只有 pause 之后的复检成功才把
    // flush 标成 `checked_ok`；否则 ACK 会把中间态当成最终态。
    pending.ack.flush = Some("checked_ok_before_pause".into());

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        pending.ack.error = Some("timeout budget exhausted before writer pause".into());
        return pending;
    }
    match agent.pause_upgrade_writers(remaining).await {
        Some(guard) => {
            pending.ack.writers_paused = true;
            pending.writer_guard = Some(guard);
        }
        None => {
            pending.ack.error =
                Some("writer pause timed out; background writers remain active".into());
            return pending;
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        pending.ack.error = Some("timeout budget exhausted before final checked flush".into());
        return pending;
    }
    match tokio::time::timeout(remaining, agent.save_now_checked()).await {
        Ok(Ok(())) => {
            pending.ack.flush = Some("checked_ok".into());
            pending.ack.prepared = true;
        }
        Ok(Err(error)) => {
            pending.ack.flush = Some("checked_failed".into());
            pending.ack.error = Some(format!("final checked flush failed: {error}"));
        }
        Err(_) => {
            pending.ack.flush = Some("checked_failed".into());
            pending.ack.error = Some("final checked flush timed out".into());
        }
    }
    pending
}

/// Core 退出时 abort；Drop 同时清理 socket 文件。
#[derive(Debug)]
pub struct DeploymentControl {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl DeploymentControl {
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for DeploymentControl {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 未设置 `ECHO_DEPLOY_CONTROL_SOCKET` 返回 `Ok(None)`（旧行为）。
pub async fn start_from_env(
    handler: Arc<dyn DeploymentHandler>,
) -> Result<Option<DeploymentControl>> {
    let Some(path) = std::env::var_os(SOCKET_ENV) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{SOCKET_ENV} must be an absolute path: {}", path.display());
    }
    Ok(Some(start_at(&path, handler).await?))
}

/// 显式路径入口（测试用）：校验父目录、清理 stale socket、bind、0600。
pub async fn start_at(
    path: &Path,
    handler: Arc<dyn DeploymentHandler>,
) -> Result<DeploymentControl> {
    if path.as_os_str().as_bytes().len() >= 108 {
        bail!(
            "unix socket path too long (max 107 bytes): {}",
            path.display()
        );
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("control socket path has no parent directory")?;
    validate_private_dir(parent)?;
    let parent_uid = std::fs::metadata(parent)?.uid();

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                bail!("refusing to replace non-socket path {}", path.display());
            }
            if meta.uid() != parent_uid {
                bail!(
                    "stale control socket {} is owned by uid {}, expected {}",
                    path.display(),
                    meta.uid(),
                    parent_uid
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("stat control socket {}", path.display()));
        }
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind deployment control socket {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    let socket_uid = std::fs::metadata(path)?.uid();
    if socket_uid != parent_uid {
        let _ = std::fs::remove_file(path);
        bail!(
            "parent dir owner uid {} does not match control socket creator uid {}",
            parent_uid,
            socket_uid
        );
    }
    info!(
        path = %path.display(),
        uid = socket_uid,
        "deployment control socket listening (same-uid SO_PEERCRED only)"
    );
    let task = tokio::spawn(accept_loop(listener, socket_uid, handler));
    Ok(DeploymentControl {
        path: path.to_path_buf(),
        task,
    })
}

fn validate_private_dir(parent: &Path) -> Result<()> {
    let meta = std::fs::metadata(parent)
        .with_context(|| format!("stat control socket parent {}", parent.display()))?;
    if !meta.is_dir() {
        bail!(
            "control socket parent is not a directory: {}",
            parent.display()
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "control socket parent {} is not private: mode {:o} (group/other bits must be 0)",
            parent.display(),
            mode
        );
    }
    Ok(())
}

async fn accept_loop(listener: UnixListener, owner_uid: u32, handler: Arc<dyn DeploymentHandler>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, owner_uid, handler).await {
                        warn!(%error, "deployment control connection ended with error");
                    }
                });
            }
            Err(error) => {
                warn!(%error, "deployment control accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    owner_uid: u32,
    handler: Arc<dyn DeploymentHandler>,
) -> std::io::Result<()> {
    let cred = stream.peer_cred()?;
    if cred.uid() != owner_uid {
        warn!(
            peer_uid = cred.uid(),
            owner_uid, "rejecting deployment control peer with different uid"
        );
        return Ok(());
    }
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let line = match read_line_limited(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let response = ControlResponse::fail(
                    None,
                    None,
                    "line_too_long",
                    format!("{error} (max {MAX_LINE_BYTES} bytes per line)"),
                );
                write_response(&mut write_half, &response).await?;
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_slice::<ControlRequest>(&line) {
            Ok(request) => {
                let operation = Some(request.operation);
                let tx = Some(request.transaction_id.clone());
                match request.validate() {
                    Ok(()) => {
                        let deadline = Instant::now() + TOTAL_BUDGET;
                        handler.handle(request, deadline).await
                    }
                    Err(message) => ControlResponse::fail(operation, tx, "bad_request", message),
                }
            }
            Err(error) => ControlResponse::fail(
                None,
                None,
                "invalid_json",
                format!("strict JSON envelope rejected: {error}"),
            ),
        };
        match tokio::time::timeout(
            ACK_WRITE_TIMEOUT,
            write_response(&mut write_half, &response),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // prepare 的 ACK 没送达（对端断开/写失败）：调用方拿不到
                // generation，永远无法 cancel；这里主动回滚，避免 maintenance、
                // gate 与 writer guard 在 detach 的连接任务里永久泄漏。
                release_unacknowledged_prepare(&handler, &response).await;
                return Err(error);
            }
            Err(_) => {
                // 对端不读导致 ACK 写不出去：同样视为未送达，回滚后断开。
                release_unacknowledged_prepare(&handler, &response).await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out writing deployment control ACK",
                ));
            }
        }
    }
    Ok(())
}

/// prepare ACK 无法送达时的兜底回滚（幂等：没有维护态/transaction 不匹配时
/// cancel 会安全失败）。只处理 prepare；status/ready 不持有维护状态。
async fn release_unacknowledged_prepare(
    handler: &Arc<dyn DeploymentHandler>,
    response: &ControlResponse,
) {
    if response.operation != Some(Operation::Prepare) || response.maintenance_held != Some(true) {
        return;
    }
    let (Some(transaction_id), Some(generation)) =
        (response.transaction_id.clone(), response.generation)
    else {
        return;
    };
    let request = ControlRequest {
        operation: Operation::Cancel,
        transaction_id,
        expected_release: None,
        expected_generation: Some(generation),
        required_modules: Vec::new(),
    };
    let cleanup = handler.handle(request, Instant::now() + TOTAL_BUDGET).await;
    if !cleanup.ok {
        warn!(
            error = ?cleanup.error,
            generation,
            "failed to release unacknowledged deployment preparation"
        );
    }
}

async fn write_response(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ControlResponse,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(response)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":{\"code\":\"serialize\"}}".to_vec());
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await
}

/// 有界逐行读：超过 `max`（不含换行）立即报错，不无限分配。
async fn read_line_limited<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        // 行尾换行不计入 64 KiB 正文；无换行时按已读字节计。
        let content_len = line.len() + take - usize::from(newline.is_some());
        if content_len > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "JSON line exceeds limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_module_loop::modules::ModuleHost;
    use std::os::unix::fs::DirBuilderExt;

    fn parse(raw: &str) -> Result<ControlRequest, serde_json::Error> {
        serde_json::from_str(raw)
    }

    #[test]
    fn request_parsing_is_strict() {
        let ok = parse(r#"{"operation":"prepare","transaction_id":"t1"}"#).unwrap();
        assert_eq!(ok.operation, Operation::Prepare);
        assert!(parse(r#"{"operation":"prepare","transaction_id":"t1","bogus":1}"#).is_err());
        assert!(parse(r#"{"operation":"deploy","transaction_id":"t1"}"#).is_err());
        assert!(parse(r#"{"operation":"prepare"}"#).is_err());
        assert!(parse(r#"{"operation":"cancel","transaction_id":"t1"}"#)
            .unwrap()
            .validate()
            .is_err());
        let cancel =
            parse(r#"{"operation":"cancel","transaction_id":"t1","expected_generation":7}"#)
                .unwrap();
        assert!(cancel.validate().is_ok());
        // 旧请求不带 required_modules 仍可解析；带上时只允许 status/ready。
        let ready = parse(
            r#"{"operation":"ready","transaction_id":"t1","required_modules":["echo-agent.session"]}"#,
        )
        .unwrap();
        assert_eq!(ready.required_modules, vec!["echo-agent.session"]);
        assert!(ready.validate().is_ok());
        let status = parse(
            r#"{"operation":"status","transaction_id":"t1","required_modules":["echo-agent.loop.single"]}"#,
        )
        .unwrap();
        assert!(status.validate().is_ok());
        let prepare_with_modules =
            parse(r#"{"operation":"prepare","transaction_id":"t1","required_modules":["x"]}"#)
                .unwrap();
        assert!(prepare_with_modules.validate().is_err());
    }

    #[tokio::test]
    async fn gate_blocks_new_readers_while_write_guard_held() {
        let gate = Arc::new(RwLock::new(()));
        let write = gate.clone().write_owned().await;
        assert!(gate.clone().try_read_owned().is_err());
        drop(write);
        assert!(gate.clone().try_read_owned().is_ok());
    }

    #[tokio::test]
    async fn line_reader_rejects_overlong_line_without_reading_it_all() {
        let data = vec![b'x'; MAX_LINE_BYTES + 2];
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let error = read_line_limited(&mut reader, MAX_LINE_BYTES)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    struct Stub;

    #[async_trait::async_trait]
    impl DeploymentHandler for Stub {
        async fn handle(&self, request: ControlRequest, _deadline: Instant) -> ControlResponse {
            let mut response =
                ControlResponse::base(Some(request.operation), Some(request.transaction_id));
            response.phase = Some("stub".into());
            response
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("echo-deploy-{tag}-{}-{nanos}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }

    #[tokio::test]
    async fn unix_socket_roundtrip_and_overlong_rejection() {
        let dir = temp_dir("unix");
        let path = dir.join("control.sock");
        let control = start_at(&path, Arc::new(Stub)).await.unwrap();

        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"{\"operation\":\"status\",\"transaction_id\":\"t1\"}\n")
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["phase"], "stub");

        let mut stream = UnixStream::connect(&path).await.unwrap();
        let overlong = vec![b'{'; MAX_LINE_BYTES + 8];
        stream.write_all(&overlong).await.unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "line_too_long");

        drop(control);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn insecure_parent_is_rejected() {
        let dir = temp_dir("insecure");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = start_at(&dir.join("control.sock"), Arc::new(Stub))
            .await
            .unwrap_err();
        assert!(format!("{error}").contains("not private"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    // ── CoreDeploymentHandler / stub manager 行为测试 ─────────────────────
    //
    // 这些测试不启动真实 LLM/磁盘：`StubAgent` 实现与 `Agent` 相同的
    // prepare→pause→checked-flush→cancel 语义，`StubManager` 直接给出
    // 退场/空人格等边界。重点钉住 P0：
    // - prepare ACK 的 ok 必须与 prepared 一致，不能把半准备报成成功；
    // - cancel 超时要保留 maintenance 供幂等重试，不能留下无代数可 cancel 的
    //   draining agent；
    // - 已 Failed 的退场必须拒绝 prepare，而不是被当成"干净"；
    // - 连接断开导致 prepare ACK 丢失时，服务端必须主动回滚。

    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PrepareMode {
        Ok,
        RetainedFailure,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SaveMode {
        Ok,
        Fail,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CancelMode {
        Ok,
        Fail,
        Slow,
    }

    #[derive(Default)]
    struct StubAgentState {
        generation: u64,
        draining: bool,
        prepare_calls: usize,
        cancel_calls: Vec<u64>,
        save_calls: usize,
    }

    struct StubAgent {
        writers: Arc<RwLock<()>>,
        state: Mutex<StubAgentState>,
        modules: Mutex<Option<ModuleSnapshot>>,
        prepare_mode: PrepareMode,
        save_mode: SaveMode,
        cancel_mode: CancelMode,
    }

    impl StubAgent {
        fn new(
            prepare_mode: PrepareMode,
            save_mode: SaveMode,
            cancel_mode: CancelMode,
        ) -> Arc<Self> {
            Arc::new(Self {
                writers: Arc::new(RwLock::new(())),
                state: Mutex::new(StubAgentState::default()),
                modules: Mutex::new(None),
                prepare_mode,
                save_mode,
                cancel_mode,
            })
        }

        fn set_modules(&self, snapshot: Option<ModuleSnapshot>) {
            *self.modules.lock().unwrap() = snapshot;
        }

        fn is_draining(&self) -> bool {
            self.state.lock().unwrap().draining
        }

        fn prepare_calls(&self) -> usize {
            self.state.lock().unwrap().prepare_calls
        }

        fn save_calls(&self) -> usize {
            self.state.lock().unwrap().save_calls
        }

        fn cancel_calls(&self) -> Vec<u64> {
            self.state.lock().unwrap().cancel_calls.clone()
        }

        fn release_generation(&self, generation: u64) -> Result<CancelRelease, CancelFailure> {
            let mut state = self.state.lock().unwrap();
            if state.generation == generation && state.draining {
                state.draining = false;
                Ok(CancelRelease::Released)
            } else {
                Ok(CancelRelease::NoMatch)
            }
        }
    }

    #[async_trait::async_trait]
    impl DeploymentAgent for StubAgent {
        async fn prepare_upgrade_quiet(
            &self,
            _timeout: Duration,
        ) -> Result<PreparedUpgrade, PrepareFailure> {
            let mut state = self.state.lock().unwrap();
            state.prepare_calls += 1;
            match self.prepare_mode {
                PrepareMode::RetainedFailure => {
                    state.generation += 1;
                    state.draining = true;
                    Err(PrepareFailure::Maintenance {
                        generation: state.generation,
                        message: "in-flight turns did not drain".into(),
                    })
                }
                PrepareMode::Ok => {
                    state.generation += 1;
                    state.draining = true;
                    Ok(PreparedUpgrade {
                        generation: state.generation,
                        memory_only: true,
                    })
                }
            }
        }

        async fn pause_upgrade_writers(
            &self,
            timeout: Duration,
        ) -> Option<OwnedRwLockWriteGuard<()>> {
            tokio::time::timeout(timeout, self.writers.clone().write_owned())
                .await
                .ok()
        }

        async fn cancel_upgrade(&self, generation: u64) -> Result<CancelRelease, CancelFailure> {
            self.state.lock().unwrap().cancel_calls.push(generation);
            match self.cancel_mode {
                CancelMode::Fail => Err(CancelFailure::Other("injected cancel failure".into())),
                CancelMode::Slow => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    self.release_generation(generation)
                }
                CancelMode::Ok => self.release_generation(generation),
            }
        }

        async fn save_now_checked(&self) -> Result<(), String> {
            self.state.lock().unwrap().save_calls += 1;
            match self.save_mode {
                SaveMode::Ok => Ok(()),
                SaveMode::Fail => Err("injected disk full".into()),
            }
        }

        async fn ensure_trunk_loaded(&self) -> Result<(), String> {
            Ok(())
        }

        fn module_snapshot(&self) -> Option<ModuleSnapshot> {
            self.modules.lock().unwrap().clone()
        }
    }

    struct StubManager {
        /// (id, enabled, running, agent)
        personas: Vec<(String, bool, bool, Arc<StubAgent>)>,
        retiring: Vec<String>,
        failed_retirements: Vec<String>,
        await_calls: AtomicUsize,
    }

    impl StubManager {
        fn new(personas: Vec<(String, bool, bool, Arc<StubAgent>)>) -> Self {
            Self {
                personas,
                retiring: Vec::new(),
                failed_retirements: Vec::new(),
                await_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl DeploymentManager for StubManager {
        fn retiring_ids(&self) -> Vec<String> {
            self.retiring.clone()
        }

        async fn await_all_retirements(&self) -> Vec<String> {
            self.await_calls.fetch_add(1, Ordering::SeqCst);
            self.failed_retirements.clone()
        }

        fn infos(&self) -> Vec<PersonaInfo> {
            self.personas
                .iter()
                .map(|(id, enabled, running, _)| PersonaInfo {
                    id: id.clone(),
                    enabled: *enabled,
                    running: *running,
                })
                .collect()
        }

        fn all(&self) -> Vec<(String, Arc<dyn DeploymentAgent>)> {
            self.personas
                .iter()
                .filter(|(_, _, running, _)| *running)
                .map(|(id, _, _, agent)| {
                    (id.clone(), Arc::clone(agent) as Arc<dyn DeploymentAgent>)
                })
                .collect()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn DeploymentAgent>> {
            self.personas
                .iter()
                .find(|(persona, _, running, _)| persona == id && *running)
                .map(|(_, _, _, agent)| Arc::clone(agent) as Arc<dyn DeploymentAgent>)
        }
    }

    fn prepare_request(transaction_id: &str) -> ControlRequest {
        ControlRequest {
            operation: Operation::Prepare,
            transaction_id: transaction_id.into(),
            expected_release: None,
            expected_generation: None,
            required_modules: Vec::new(),
        }
    }

    fn cancel_request(transaction_id: &str, generation: u64) -> ControlRequest {
        ControlRequest {
            operation: Operation::Cancel,
            transaction_id: transaction_id.into(),
            expected_release: None,
            expected_generation: Some(generation),
            required_modules: Vec::new(),
        }
    }

    fn status_request(transaction_id: &str) -> ControlRequest {
        ControlRequest {
            operation: Operation::Status,
            transaction_id: transaction_id.into(),
            expected_release: None,
            expected_generation: None,
            required_modules: Vec::new(),
        }
    }

    fn ready_request(transaction_id: &str, required_modules: Vec<String>) -> ControlRequest {
        ControlRequest {
            operation: Operation::Ready,
            transaction_id: transaction_id.into(),
            expected_release: None,
            expected_generation: None,
            required_modules,
        }
    }

    fn budget() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn test_module(id: &str) -> Arc<dyn echo_module::Module> {
        Arc::new(echo_module::BuiltinModule::new(
            echo_module::ModuleManifest::builtin(
                id,
                id,
                "0.0.0-test",
                "tool",
                "deployment readiness test module",
            ),
            |_| Ok(Vec::new()),
        ))
    }

    /// 真实 `ModuleHost`：一个已挂载、一个只注册未挂载、一个只有 definition。
    fn live_module_snapshot() -> ModuleSnapshot {
        let host = ModuleHost::new();
        host.register_and_mount(test_module("mounted.ok"))
            .expect("mount first test module");
        host.registry
            .register(test_module("registered.not_mounted"))
            .expect("register second test module");
        host.set_definitions([(
            "definition.only".to_string(),
            echo_module::ModuleDefinition {
                source_path: "/tmp/modules.toml".into(),
                command: "ignored-binary".into(),
                args: vec!["--ignored".into()],
                env_keys: vec!["IGNORED_TOKEN".into()],
            },
        )]);
        snapshot_module_host(&host)
    }

    fn handler_with_snapshot(snapshot: ModuleSnapshot) -> (Arc<StubAgent>, CoreDeploymentHandler) {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        core.set_modules(Some(snapshot));
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate, core.clone(), manager);
        (core, handler)
    }

    #[test]
    fn module_snapshot_uses_live_mount_set_not_definitions_or_config_enabled() {
        let snapshot = live_module_snapshot();
        assert!(snapshot.verified, "{:?}", snapshot.reason);
        let ids = snapshot
            .modules
            .iter()
            .map(|module| module.id.as_str())
            .collect::<Vec<_>>();
        // definition-only 不在 live 注册表里，绝不能被当成已挂载。
        assert_eq!(ids, vec!["mounted.ok", "registered.not_mounted"]);

        let mounted = snapshot
            .modules
            .iter()
            .find(|module| module.id == "mounted.ok")
            .expect("mounted module descriptor");
        assert!(mounted.mounted, "is_mounted(active set) must be true");
        assert!(mounted.active);
        assert!(mounted.enabled);

        let unmounted = snapshot
            .modules
            .iter()
            .find(|module| module.id == "registered.not_mounted")
            .expect("registered module descriptor");
        // 配置/启用开关为 true，但真实挂载事实是 false —— 二者绝不能混。
        assert!(unmounted.enabled);
        assert!(!unmounted.mounted);
        assert!(!unmounted.active);
    }

    #[tokio::test]
    async fn ready_accepts_true_mounted_required_module() {
        let (_core, handler) = handler_with_snapshot(live_module_snapshot());
        let response = handler
            .handle(
                ready_request("tx-module-ok", vec!["mounted.ok".into()]),
                budget(),
            )
            .await;
        assert_eq!(response.ready, Some(true), "{:?}", response.error);
        assert_eq!(response.modules_verified, Some(true));
        let checks = response.checks.as_ref().expect("checks");
        assert!(checks.modules_verified);
        assert!(checks.required_modules_mounted);
        assert!(checks.missing_modules.is_none());
        let evidence = response.module_state.as_ref().expect("module evidence");
        assert!(evidence.verified);
        assert_eq!(evidence.required, vec!["mounted.ok"]);
        assert!(evidence.mounted.contains(&"mounted.ok".to_string()));
        assert!(evidence.active.contains(&"mounted.ok".to_string()));
        assert!(evidence.missing_required.is_empty());
        // 明确证明不是"只回了 static module id"。
        let observed = evidence
            .modules
            .iter()
            .find(|module| module.id == "mounted.ok")
            .expect("module state for mounted.ok");
        assert!(observed.mounted);
        assert_eq!(observed.runtime, "in_process");
    }

    #[tokio::test]
    async fn missing_required_module_is_not_ready() {
        let (_core, handler) = handler_with_snapshot(live_module_snapshot());
        let response = handler
            .handle(
                ready_request("tx-module-missing", vec!["ghost.module".into()]),
                budget(),
            )
            .await;
        assert_eq!(response.ready, Some(false));
        assert_eq!(response.modules_verified, Some(true));
        let checks = response.checks.as_ref().expect("checks");
        assert!(checks.modules_verified);
        assert!(!checks.required_modules_mounted);
        assert_eq!(checks.missing_modules, Some(vec!["ghost.module".into()]));
        assert_eq!(
            checks.module_errors,
            Some(vec!["ghost.module: not registered in module host".into()])
        );
        let evidence = response.module_state.as_ref().expect("module evidence");
        assert_eq!(evidence.missing_required, vec!["ghost.module"]);
        assert!(!evidence.mounted.contains(&"ghost.module".to_string()));
    }

    #[tokio::test]
    async fn required_registered_but_unmounted_module_is_not_ready() {
        let (_core, handler) = handler_with_snapshot(live_module_snapshot());
        let response = handler
            .handle(
                ready_request("tx-module-unmounted", vec!["registered.not_mounted".into()]),
                budget(),
            )
            .await;
        assert_eq!(response.ready, Some(false));
        let checks = response.checks.as_ref().expect("checks");
        assert!(checks.modules_verified);
        assert!(!checks.required_modules_mounted);
        assert_eq!(
            checks.missing_modules,
            Some(vec!["registered.not_mounted".into()])
        );
        assert_eq!(
            checks.module_errors,
            Some(vec![
                "registered.not_mounted: registered but not mounted".into()
            ])
        );
        let evidence = response.module_state.as_ref().expect("module evidence");
        // 注册 + enabled 都存在，但真实 mounted 集合里没有它。
        assert!(!evidence
            .mounted
            .contains(&"registered.not_mounted".to_string()));
        assert!(!evidence
            .active
            .contains(&"registered.not_mounted".to_string()));
    }

    #[test]
    fn failed_runtime_state_is_not_active_even_when_mounted() {
        let snapshot = ModuleSnapshot {
            verified: true,
            reason: None,
            modules: vec![ModuleState {
                id: "ext.failed".into(),
                name: "external".into(),
                version: "0.0.0-test".into(),
                kind: "tool".into(),
                runtime: "subprocess".into(),
                builtin: false,
                enabled: true,
                mounted: true,
                active: false,
                state: "failed".into(),
                error: None,
                required_services: Vec::new(),
                pending_services: Vec::new(),
            }],
        };
        let evidence = ModuleEvidence::from_snapshot(Some(snapshot), &["ext.failed".to_string()]);
        assert!(evidence.mounted.contains(&"ext.failed".to_string()));
        assert!(!evidence.active.contains(&"ext.failed".to_string()));
        assert_eq!(evidence.missing_required, vec!["ext.failed"]);
        assert_eq!(
            evidence.required_errors,
            vec!["ext.failed: mounted but state=failed"]
        );
        assert_eq!(evidence.failed, vec!["ext.failed: state=failed"]);
    }
    #[tokio::test]
    async fn faulted_mounted_module_is_not_active_and_not_ready() {
        let host = ModuleHost::new();
        host.register_and_mount(test_module("faulted.module"))
            .expect("mount faulted module");
        // 真实挂载成功过，但运行态失败：mounted=true 不等于 active/ready。
        host.registry
            .set_module_error("faulted.module", "handshake failed");
        let snapshot = snapshot_module_host(&host);
        let (_core, handler) = handler_with_snapshot(snapshot);
        let response = handler
            .handle(
                ready_request("tx-module-faulted", vec!["faulted.module".into()]),
                budget(),
            )
            .await;
        assert_eq!(response.ready, Some(false));
        let checks = response.checks.as_ref().expect("checks");
        assert!(checks.modules_verified);
        assert!(!checks.required_modules_mounted);
        assert_eq!(checks.missing_modules, Some(vec!["faulted.module".into()]));
        assert_eq!(
            checks.module_errors,
            Some(vec![
                "faulted.module: mounted but inactive: handshake failed".into()
            ])
        );
        let evidence = response.module_state.as_ref().expect("module evidence");
        assert!(evidence.mounted.contains(&"faulted.module".to_string()));
        assert!(!evidence.active.contains(&"faulted.module".to_string()));
        assert!(evidence
            .failed
            .iter()
            .any(|failure| failure.contains("handshake failed")));
        let module = evidence
            .modules
            .iter()
            .find(|module| module.id == "faulted.module")
            .expect("faulted module state");
        assert!(module.mounted);
        assert!(!module.active);
        assert_eq!(module.error.as_deref(), Some("handshake failed"));
    }

    #[tokio::test]
    async fn unverified_module_snapshot_is_never_ready_and_says_why() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        // 不 set_modules：模拟部署代理无法提供 live ModuleHost。
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate, core, manager);
        let response = handler
            .handle(
                ready_request("tx-module-unknown", vec!["echo-agent.session".into()]),
                budget(),
            )
            .await;
        assert_eq!(
            response.ready,
            Some(false),
            "unverifiable module state must not pass"
        );
        assert_eq!(response.modules_verified, Some(false));
        let checks = response.checks.as_ref().expect("checks");
        assert!(!checks.modules_verified);
        assert!(!checks.required_modules_mounted);
        let evidence = response.module_state.as_ref().expect("module evidence");
        assert!(!evidence.verified);
        assert!(evidence
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("did not expose"));
        // 不能把"没观测到"伪装成"确认缺失"。
        assert!(evidence.missing_required.is_empty());
        assert!(evidence.required_errors.is_empty());
        assert!(evidence.modules.is_empty());
    }

    #[test]
    fn empty_module_host_is_unverified_with_reason() {
        let snapshot = snapshot_module_host(&ModuleHost::new());
        assert!(!snapshot.verified);
        assert!(snapshot
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("registry contains no registered modules"));
        assert!(snapshot.modules.is_empty());
    }

    #[tokio::test]
    async fn status_returns_live_module_evidence_without_gating() {
        let (_core, handler) = handler_with_snapshot(live_module_snapshot());
        let response = handler.handle(status_request("tx-status"), budget()).await;
        assert!(response.ok);
        assert_eq!(response.modules_verified, Some(true));
        let evidence = response.module_state.as_ref().expect("module evidence");
        assert!(evidence.verified);
        assert!(evidence.mounted.contains(&"mounted.ok".to_string()));
        assert!(evidence.active.contains(&"mounted.ok".to_string()));
        assert!(!evidence
            .mounted
            .contains(&"registered.not_mounted".to_string()));
        assert_eq!(
            evidence
                .modules
                .iter()
                .filter(|module| module.mounted)
                .map(|module| module.id.as_str())
                .collect::<Vec<_>>(),
            vec!["mounted.ok"]
        );
        // 线格式契约（runtime 适配器按这些字段筛选，不靠只回 id）。
        let json = serde_json::to_value(&response).expect("status response serializes");
        assert_eq!(json["modules_verified"], serde_json::json!(true));
        assert_eq!(json["module_state"]["verified"], serde_json::json!(true));
        assert_eq!(
            json["module_state"]["mounted"],
            serde_json::json!(["mounted.ok"])
        );
        assert_eq!(
            json["module_state"]["active"],
            serde_json::json!(["mounted.ok"])
        );
        assert_eq!(
            json["module_state"]["modules"][0]["mounted"],
            serde_json::json!(true)
        );
        // 配置开关 enabled=true，但真实 mounted=false：不能把配置当挂载。
        assert_eq!(
            json["module_state"]["modules"][1]["enabled"],
            serde_json::json!(true)
        );
        assert_eq!(
            json["module_state"]["modules"][1]["mounted"],
            serde_json::json!(false)
        );
        assert_eq!(
            json["module_state"]["modules"][1]["active"],
            serde_json::json!(false)
        );
    }

    #[tokio::test]
    async fn stub_prepare_cancel_really_pauses_writers_and_reopens_gate() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        let persona = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        let manager = Arc::new(StubManager::new(vec![(
            "persona".into(),
            true,
            true,
            Arc::clone(&persona),
        )]));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler.handle(prepare_request("tx-pnc"), budget()).await;
        assert!(prepared.ok, "prepare should succeed: {:?}", prepared.error);
        assert_eq!(prepared.prepared, Some(true));
        assert_eq!(prepared.phase.as_deref(), Some("prepared"));
        assert_eq!(prepared.agents.len(), 2, "persona + __core");

        // 新 turn 被两条 agent 维护闸挡住；后台写者拿到的是真写锁；
        // 管理命令闸也被 prepare 持有。
        assert!(core.is_draining());
        assert!(persona.is_draining());
        assert!(core.writers.try_read().is_err());
        assert!(persona.writers.try_read().is_err());
        assert!(gate.clone().try_read_owned().is_err());

        let generation = prepared.generation.expect("generation");
        let cancelled = handler
            .handle(cancel_request("tx-pnc", generation), budget())
            .await;
        assert!(cancelled.ok, "cancel should succeed: {:?}", cancelled.error);
        assert_eq!(cancelled.maintenance_held, Some(false));
        assert!(
            cancelled
                .cancel_results
                .iter()
                .all(|result| result.result == "released"),
            "{:?}",
            cancelled.cancel_results
        );

        assert!(!core.is_draining());
        assert!(!persona.is_draining());
        assert!(core.writers.try_read().is_ok());
        assert!(persona.writers.try_read().is_ok());
        assert!(gate.clone().try_read_owned().is_ok());
        assert_eq!(core.cancel_calls(), vec![generation]);
        assert_eq!(persona.cancel_calls(), vec![generation]);
    }

    #[tokio::test]
    async fn stub_final_flush_failure_is_not_ok_and_cancel_still_releases() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Fail, CancelMode::Ok);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler.handle(prepare_request("tx-save"), budget()).await;
        assert!(!prepared.ok, "half-prepared must not ACK ok");
        assert_eq!(
            prepared.error.as_ref().map(|error| error.code.as_str()),
            Some("prepare_incomplete")
        );
        assert_eq!(prepared.prepared, Some(false));
        let core_ack = prepared
            .agents
            .iter()
            .find(|ack| ack.id == "__core")
            .expect("core ack");
        assert!(core_ack.writers_paused);
        assert_eq!(core_ack.flush.as_deref(), Some("checked_failed"));
        assert_eq!(core_ack.generation, Some(1));
        assert_eq!(core.save_calls(), 1, "final checked flush was attempted");
        assert!(core.writers.try_read().is_err(), "guard must stay held");
        assert!(core.is_draining());

        let cancelled = handler
            .handle(
                cancel_request("tx-save", prepared.generation.expect("generation")),
                budget(),
            )
            .await;
        assert!(cancelled.ok, "cancel should recover: {:?}", cancelled.error);
        assert!(!core.is_draining());
        assert!(core.writers.try_read().is_ok());
        assert!(gate.clone().try_read_owned().is_ok());
    }

    #[tokio::test]
    async fn already_failed_retirements_refuse_prepare_without_gate_leak() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        let mut manager = StubManager::new(Vec::new());
        manager.failed_retirements = vec!["ghost: final save failed".into()];
        manager.retiring = vec!["ghost".into()];
        let gate = Arc::new(RwLock::new(()));
        let handler =
            CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), Arc::new(manager));

        let response = handler.handle(prepare_request("tx-retire"), budget()).await;
        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("retiring_in_progress")
        );
        let retirements = response
            .off_register_retirements
            .as_ref()
            .expect("retirement ack");
        assert_eq!(retirements.failed, vec!["ghost: final save failed"]);
        assert!(response.generation.is_none());
        assert_eq!(core.prepare_calls(), 0, "must not prepare any agent");
        assert!(
            gate.clone().try_read_owned().is_ok(),
            "rejected prepare must drop the gate"
        );

        // Failed 登记按设计保留在册：第二次 prepare 仍然拒绝，而不是时好时坏。
        let again = handler
            .handle(prepare_request("tx-retire-2"), budget())
            .await;
        assert!(!again.ok);
        assert_eq!(
            again.error.as_ref().map(|error| error.code.as_str()),
            Some("retiring_in_progress")
        );
    }

    #[tokio::test]
    async fn empty_personas_prepare_succeeds_with_core_only() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler.handle(prepare_request("tx-empty"), budget()).await;
        assert!(prepared.ok, "empty personas is valid: {:?}", prepared.error);
        assert_eq!(prepared.prepared, Some(true));
        assert_eq!(prepared.agents.len(), 1);
        assert_eq!(prepared.agents[0].id, "__core");
        assert_eq!(core.prepare_calls(), 1);

        let cancelled = handler
            .handle(
                cancel_request("tx-empty", prepared.generation.expect("generation")),
                budget(),
            )
            .await;
        assert!(cancelled.ok);
        assert!(gate.clone().try_read_owned().is_ok());
    }

    #[tokio::test]
    async fn cancel_timeout_keeps_maintenance_for_idempotent_retry() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Slow);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler
            .handle(prepare_request("tx-timeout"), budget())
            .await;
        assert!(prepared.ok);
        let generation = prepared.generation.expect("generation");

        // 1ms 预算 < stub 的 200ms cancel：必须保留 maintenance，不能把
        // draining agent 留成"没有代数可 cancel"的孤儿。
        let short = Instant::now() + Duration::from_millis(1);
        let first = handler
            .handle(cancel_request("tx-timeout", generation), short)
            .await;
        assert!(!first.ok);
        assert_eq!(
            first.error.as_ref().map(|error| error.code.as_str()),
            Some("cancel_incomplete")
        );
        assert_eq!(first.maintenance_held, Some(true));
        assert!(core.is_draining());
        assert!(core.writers.try_read().is_err(), "guard must be retained");
        assert!(
            gate.clone().try_read_owned().is_err(),
            "gate must be retained"
        );

        // 幂等重试同一代数：成功后释放 guard/gate。
        let second = handler
            .handle(cancel_request("tx-timeout", generation), budget())
            .await;
        assert!(second.ok, "retry should release: {:?}", second.error);
        assert_eq!(second.maintenance_held, Some(false));
        assert!(!core.is_draining());
        assert!(core.writers.try_read().is_ok());
        assert!(gate.clone().try_read_owned().is_ok());
    }

    /// 模拟"prepare ACK 写入时对端已断开"：handler 在放行前挂起，测试先
    /// drop client，再让 handler 返回，`write_response` 必然失败。服务端必须
    /// 自动发 cancel 回滚，否则客户端没拿到 generation，维护 guard 永远泄漏。
    struct PendingPrepareHandler {
        requests: Mutex<Vec<ControlRequest>>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl PendingPrepareHandler {
        fn new() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl DeploymentHandler for PendingPrepareHandler {
        async fn handle(&self, request: ControlRequest, _deadline: Instant) -> ControlResponse {
            self.requests.lock().unwrap().push(request.clone());
            if request.operation == Operation::Prepare {
                self.entered.notify_one();
                self.release.notified().await;
                let mut response =
                    ControlResponse::base(Some(Operation::Prepare), Some(request.transaction_id));
                response.generation = Some(7);
                response.prepared = Some(true);
                response.maintenance_held = Some(true);
                response.can_cancel = Some(true);
                return response;
            }
            ControlResponse::base(Some(Operation::Cancel), Some(request.transaction_id))
        }
    }

    #[tokio::test]
    async fn dropped_connection_releases_unacknowledged_prepare() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let owner_uid = client.peer_cred().unwrap().uid();
        let handler = Arc::new(PendingPrepareHandler::new());
        let task = tokio::spawn(handle_connection(server, owner_uid, handler.clone()));

        client
            .write_all(b"{\"operation\":\"prepare\",\"transaction_id\":\"tx-drop\"}\n")
            .await
            .unwrap();
        handler.entered.notified().await;
        // ACK 还没写：先把对端关掉，再放行 handler。
        drop(client);
        handler.release.notify_one();

        let joined = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("connection task should finish after write failure")
            .expect("connection task should not panic");
        assert!(
            joined.is_err(),
            "write to a closed peer should fail and trigger cleanup"
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            while handler.requests.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("server should send a cleanup cancel");

        let requests = handler.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].operation, Operation::Cancel);
        assert_eq!(requests[1].transaction_id, "tx-drop");
        assert_eq!(requests[1].expected_generation, Some(7));
    }

    #[tokio::test]
    async fn retained_prepare_failure_returns_cancellable_generation() {
        let core = StubAgent::new(PrepareMode::RetainedFailure, SaveMode::Ok, CancelMode::Ok);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler
            .handle(prepare_request("tx-retained"), budget())
            .await;
        assert!(!prepared.ok);
        assert_eq!(
            prepared.error.as_ref().map(|error| error.code.as_str()),
            Some("prepare_incomplete")
        );
        assert_eq!(prepared.prepared, Some(false));
        let core_ack = prepared
            .agents
            .iter()
            .find(|ack| ack.id == "__core")
            .expect("core ack");
        assert_eq!(
            core_ack.generation,
            Some(1),
            "failed prepare must expose its retained generation"
        );
        assert!(!core_ack.writers_paused);
        assert_eq!(core.prepare_calls(), 1);
        assert!(core.is_draining(), "failed prepare stays fail-closed");

        let cancelled = handler
            .handle(
                cancel_request("tx-retained", prepared.generation.expect("generation")),
                budget(),
            )
            .await;
        assert!(cancelled.ok, "cancel should recover: {:?}", cancelled.error);
        assert!(!core.is_draining());
        assert!(gate.clone().try_read_owned().is_ok());
    }

    #[tokio::test]
    async fn explicit_cancel_error_keeps_maintenance_for_retry() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Fail);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate.clone(), core.clone(), manager);

        let prepared = handler
            .handle(prepare_request("tx-cancel-err"), budget())
            .await;
        assert!(prepared.ok);
        let generation = prepared.generation.expect("generation");

        let failed = handler
            .handle(cancel_request("tx-cancel-err", generation), budget())
            .await;
        assert!(!failed.ok);
        assert_eq!(
            failed.error.as_ref().map(|error| error.code.as_str()),
            Some("cancel_incomplete")
        );
        assert_eq!(failed.maintenance_held, Some(true));
        assert_eq!(
            failed.cancel_results[0].result, "error",
            "non-retiring cancel failure must not be reported as released"
        );
        assert!(core.is_draining());
        assert!(core.writers.try_read().is_err());
        assert!(gate.clone().try_read_owned().is_err());
    }

    #[tokio::test]
    async fn release_info_is_env_declaration_not_binary_identity() {
        let core = StubAgent::new(PrepareMode::Ok, SaveMode::Ok, CancelMode::Ok);
        let manager = Arc::new(StubManager::new(Vec::new()));
        let gate = Arc::new(RwLock::new(()));
        let handler = CoreDeploymentHandler::with_backends(gate, core, manager);

        let response = handler.handle(status_request("tx-release"), budget()).await;
        assert!(response.ok);
        assert!(
            !response.release.identity_verified,
            "ECHO_RELEASE_ID is a declaration, never binary identity"
        );
        assert!(
            response.release.verification.contains("env"),
            "verification must say the source is the env declaration: {:?}",
            response.release.verification
        );
    }
}
