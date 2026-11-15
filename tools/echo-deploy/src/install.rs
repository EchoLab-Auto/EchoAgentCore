//! Render-only systemd installation plan text.
//!
//! This module deliberately separates rendering from activation:
//!
//! * [`render_install`] produces deterministic unit and launcher text. It does
//!   not require the release tree or config files to exist.
//! * [`write_install`] writes that text into a *new* directory as a plan
//!   preview. It never loads units, never invokes `systemctl`, never writes a
//!   unit into a live unit directory, never writes into the configured release
//!   or state trees, and never modifies core/panel config files.
//!
//! The runtime activation gate is **not** implemented. Every rendered plan
//! reports `activation_supported: false`; the updater unit is text only and
//! must not be enabled or started by this module.
//!
//! # Two rendering modes
//!
//! * The legacy/default mode only fixes a `release_root`. Its output is kept
//!   byte-identical to previous versions so existing plan consumers do not see
//!   a drive-by contract change.
//! * [`InstallSpec::deployment`] opts into a real deployment runtime. It makes
//!   the units track a durable `current` selector instead of a frozen release
//!   directory, injects `ECHO_DEPLOY_CONTROL_SOCKET`, and renders trusted
//!   administrator launchers that derive `ECHO_RELEASE_ID` from that selector.
//!   The declaration is never presented as a build ID or process identity.
//!
//! Even in deployment mode this module only renders text. Creating the private
//! state directories, installing the files, reloading systemd, starting units
//! and verifying `/proc/<pid>/exe` remain separate, explicitly confirmed steps.
//!
//! # Input contract
//!
//! [`InstallSpec`] is strict serde (`deny_unknown_fields`). All paths are
//! absolute, lexical, normal paths; no `~`, `.` or `..` expansion is done.
//! `panel_config` is required for `standalone` and forbidden for `core_only`
//! and `embedded`, matching the release topology table. Adding
//! `deployment` is optional and defaults to `None`, so existing JSON specs keep
//! parsing and rendering exactly as before.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fmt::Write as _,
    fs::File,
    io::Write as _,
    os::fd::{AsRawFd, FromRawFd},
    path::{Component, Path, PathBuf},
};

pub const INSTALL_SCHEMA_VERSION: u32 = 1;
pub const DEPLOYMENT_RUNTIME_SCHEMA_VERSION: u32 = 1;
pub const CORE_TIMEOUT_STOP_SEC: u32 = 300;
pub const PANEL_TIMEOUT_STOP_SEC: u32 = 30;
pub const UPDATER_TIMEOUT_START_SEC: u32 = 1800;
pub const UPDATER_TIMEOUT_STOP_SEC: u32 = 30;
/// The activation/runtime gate is intentionally absent from this phase.
pub const ACTIVATION_SUPPORTED: bool = false;

/// Name of the strict request consumed by `echo-deploy apply-request`.
pub const UPDATE_REQUEST_FILE: &str = "update-request.json";
/// Core's strict one-line deployment-control protocol name.
pub const CONTROL_PROTOCOL: &str = "echo-deploy-control/1";

const CORE_BINARY_RELATIVE: &str = "bin/echo-agent-core";
const PANEL_BINARY_RELATIVE: &str = "bin/echo-agent-panel";
const UNIT_MODE: u32 = 0o644;
const LAUNCHER_MODE: u32 = 0o755;
const PLAN_DIR_MODE: u32 = 0o700;
/// Core rejects Unix socket paths at or above this many bytes (sun_path limit).
const MAX_CONTROL_SOCKET_BYTES: usize = 107;
/// Mirrors `model::safe_id` for release ids and management protocols.
const MAX_RELEASE_ID_BYTES: usize = 100;
const MAX_MANAGEMENT_PROTOCOL_BYTES: usize = 64;

/// Scope of the generated units. It also decides owner metadata and
/// `[Install] WantedBy=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    User,
    System,
}

impl InstallScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }

    pub fn is_system(self) -> bool {
        matches!(self, Self::System)
    }
}

/// Deployment shape. `embedded` has no separate Panel service; `standalone`
/// requires one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallTopology {
    CoreOnly,
    Embedded,
    Standalone,
}

impl InstallTopology {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CoreOnly => "core_only",
            Self::Embedded => "embedded",
            Self::Standalone => "standalone",
        }
    }
}

/// Opt-in deployment runtime wiring for one instance.
///
/// Without this block the renderer keeps the historical frozen-`release_root`
/// output. With it:
///
/// * `release_root` is the durable **current selector** and must live below
///   `state_dir`; the units never embed one concrete release directory.
/// * Core gets `ECHO_DEPLOY_CONTROL_SOCKET`, while `ECHO_RELEASE_ID` is derived
///   by the trusted launcher from that selector at exec time.
/// * The strict `echo-deploy runtime-adapter` config is built from
///   [`InstallSpec::runtime_adapter_config`].
///
/// This is a rendering/validation contract only. It does not create any of the
/// configured paths, load a unit, or start a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentRuntime {
    pub schema_version: u32,
    /// Managed Core source checkout, used as Core's `WorkingDirectory`
    /// (the real deployment root is
    /// `/home/evence/.local/share/echo-agent-core/source`).
    pub core_root: PathBuf,
    /// Managed Panel source checkout; required for `standalone`, forbidden for
    /// `core_only`/`embedded` (mirrors `panel_config`).
    #[serde(default)]
    pub panel_root: Option<PathBuf>,
    /// Directory that holds the installed trusted launchers. It must be the
    /// same directory passed to `install-files --launcher-directory`.
    pub launcher_directory: PathBuf,
    /// Candidate store. Each direct child is one verified release directory.
    pub release_store: PathBuf,
    /// Private adapter state directory. Must differ from (and not nest with)
    /// the transaction `state_dir`, because both take the same flock name.
    pub instance_state: PathBuf,
    /// Absolute Core deployment-control socket, directly inside
    /// `instance_state` so its parent is private.
    pub core_socket: PathBuf,
    /// Strict `runtime-adapter` JSON config path, directly inside
    /// `instance_state`.
    pub runtime_config: PathBuf,
    /// Panel maintenance gate JSON; required for `standalone`, forbidden for
    /// `core_only`/`embedded`. The adapter refuses to prepare while it is
    /// missing or invalid.
    #[serde(default)]
    pub panel_gate_file: Option<PathBuf>,
    /// Exact modules the runtime adapter must report during readiness.
    #[serde(default)]
    pub required_modules: Vec<String>,
    /// Must be set explicitly by the operator only after external (shell)
    /// writers are actually stopped. Defaults to `false` so an opt-in
    /// deployment cannot silently claim that acknowledgement.
    #[serde(default)]
    pub acknowledge_external_writers_stopped: bool,
}

impl DeploymentRuntime {
    fn validate(&self, spec: &InstallSpec) -> Result<()> {
        ensure!(
            self.schema_version == DEPLOYMENT_RUNTIME_SCHEMA_VERSION,
            "unsupported deployment runtime schema {}",
            self.schema_version
        );
        for (label, path) in [
            ("deployment.core_root", &self.core_root),
            ("deployment.launcher_directory", &self.launcher_directory),
            ("deployment.release_store", &self.release_store),
            ("deployment.instance_state", &self.instance_state),
            ("deployment.core_socket", &self.core_socket),
            ("deployment.runtime_config", &self.runtime_config),
        ] {
            validate_absolute_path(path, label)?;
        }
        // This path becomes ExecStart's first token (joined with the launcher
        // name), so it must obey systemd's stricter executable rules.
        validate_executable_path(&self.launcher_directory, "deployment.launcher_directory")?;

        // The selector must be a descendant of the transaction state directory,
        // matching `SystemdSpec::new`; otherwise the systemd backend rejects it.
        ensure!(
            self.release_store != spec.state_dir,
            "deployment.release_store must differ from state_dir"
        );
        ensure!(
            !same_or_descendant(&spec.state_dir, &self.release_store),
            "deployment.release_store must not contain state_dir"
        );
        ensure!(
            self.release_store != spec.release_root,
            "deployment.release_store must differ from the current selector"
        );
        ensure!(
            !overlaps(&self.release_store, &spec.release_root),
            "deployment.release_store and the current selector must not overlap"
        );
        ensure!(
            spec.release_root.starts_with(&spec.state_dir) && spec.release_root != spec.state_dir,
            "deployment mode requires release_root to be the current selector below state_dir"
        );
        ensure!(
            !overlaps(&self.instance_state, &spec.state_dir),
            "deployment.instance_state must not overlap state_dir (the state lock names collide)"
        );
        ensure!(
            !overlaps(&self.instance_state, &self.release_store),
            "deployment.instance_state must not overlap release_store"
        );
        ensure!(
            !overlaps(&self.launcher_directory, &spec.release_root)
                && !overlaps(&self.launcher_directory, &spec.state_dir)
                && !overlaps(&self.launcher_directory, &self.release_store)
                && !overlaps(&self.launcher_directory, &self.instance_state)
                && !overlaps(&self.launcher_directory, &self.core_root),
            "deployment.launcher_directory must stay outside the release, state, runtime and source trees"
        );
        ensure!(
            !overlaps(&self.core_root, &self.release_store)
                && !overlaps(&self.core_root, &self.instance_state),
            "deployment.core_root must stay outside the release store and instance state"
        );
        ensure!(
            !same_or_descendant(&spec.deploy_executable, &self.release_store),
            "deploy_executable must not live inside the candidate release store"
        );
        let socket = path_text(&self.core_socket, "deployment.core_socket")?;
        ensure!(
            socket.len() <= MAX_CONTROL_SOCKET_BYTES,
            "deployment.core_socket exceeds Core's {MAX_CONTROL_SOCKET_BYTES}-byte limit"
        );
        ensure_socket_child(&self.core_socket, &self.instance_state, "core_socket")?;
        ensure_socket_child(&self.runtime_config, &self.instance_state, "runtime_config")?;
        ensure!(
            self.core_socket != self.runtime_config,
            "deployment.core_socket and runtime_config must differ"
        );

        for module in &self.required_modules {
            validate_release_id(module, "deployment.required_modules entry")?;
        }

        match spec.topology {
            InstallTopology::Standalone => {
                let panel_root = self
                    .panel_root
                    .as_ref()
                    .context("topology standalone requires deployment.panel_root")?;
                validate_absolute_path(panel_root, "deployment.panel_root")?;
                ensure!(
                    !overlaps(panel_root, &self.release_store)
                        && !overlaps(panel_root, &self.instance_state)
                        && !overlaps(panel_root, &self.launcher_directory),
                    "deployment.panel_root must stay outside the release, runtime and launcher trees"
                );
                let gate = self.panel_gate_file.as_ref().context(
                    "topology standalone requires deployment.panel_gate_file for maintenance truth",
                )?;
                ensure_socket_child(gate, &self.instance_state, "panel_gate_file")?;
                ensure!(
                    gate != &self.core_socket && gate != &self.runtime_config,
                    "deployment.panel_gate_file must differ from core_socket and runtime_config"
                );
            }
            InstallTopology::CoreOnly | InstallTopology::Embedded => {
                ensure!(
                    self.panel_root.is_none(),
                    "topology {} must not specify deployment.panel_root",
                    spec.topology.as_str()
                );
                ensure!(
                    self.panel_gate_file.is_none(),
                    "topology {} must not specify deployment.panel_gate_file",
                    spec.topology.as_str()
                );
            }
        }
        Ok(())
    }
}

fn ensure_socket_child(path: &Path, parent: &Path, field: &str) -> Result<()> {
    ensure!(
        path.parent() == Some(parent),
        "deployment.{field} must be a direct child of deployment.instance_state"
    );
    Ok(())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    same_or_descendant(a, b) || same_or_descendant(b, a)
}

/// Strict JSON document understood by `echo-deploy runtime-adapter --config`.
///
/// Field names and shape must stay byte-compatible with
/// `runtime_adapter::RuntimeConfig` (`deny_unknown_fields`). The installer
/// writes this document with mode 0600; it is not rendered into the systemd
/// unit itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeAdapterConfig {
    pub instance_id: String,
    pub core_socket: PathBuf,
    pub panel_gate_file: Option<PathBuf>,
    pub instance_state: PathBuf,
    pub current_selector: PathBuf,
    pub release_store: PathBuf,
    pub required_modules: Vec<String>,
    pub acknowledge_external_writers_stopped: bool,
    /// Opt-in first-install proof contract. The systemd transaction backend
    /// writes this exact file before asking the adapter to prepare a first
    /// install, so a missing socket is never mistaken for an empty install.
    pub first_install: FirstInstallRuntimeConfig,
}

/// Mirror of `runtime_adapter::FirstInstallConfig`; `max_evidence_age_seconds`
/// is omitted so the adapter's own default applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirstInstallRuntimeConfig {
    pub evidence: PathBuf,
    pub core_unit: String,
    pub panel_unit: Option<String>,
}

/// Panel maintenance gate document. The adapter requires this file to exist
/// before the first `prepare`; `release_id` is a deployment declaration, not a
/// build ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PanelGateDocument {
    pub schema_version: u32,
    pub instance_id: String,
    pub maintenance: bool,
    pub release_id: String,
    pub management_protocol: String,
}

/// Path/identity metadata for an opted-in deployment runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentRuntimePlan {
    pub current_selector: PathBuf,
    pub release_store: PathBuf,
    pub instance_state: PathBuf,
    pub core_socket: PathBuf,
    pub runtime_config: PathBuf,
    pub panel_gate_file: Option<PathBuf>,
    /// Fixed path written by `SystemdTransactionBackend` for a first install.
    pub first_install_evidence: PathBuf,
    pub core_launcher: PathBuf,
    pub panel_launcher: Option<PathBuf>,
    /// The same trusted launcher core `ExecStart` uses. It multiplexes the
    /// control protocol on `ECHO_DEPLOY_CONTROL_PROTOCOL`, so the systemd
    /// backend can invoke it with no arguments.
    pub control_adapter: PathBuf,
    /// Fixed reminder that `ECHO_RELEASE_ID` is selector-derived declaration.
    pub release_identity: &'static str,
}

/// Strict input for one installation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSpec {
    /// Stable instance token used in unit and launcher names.
    pub instance_id: String,
    pub scope: InstallScope,
    /// Root of the candidate/release tree containing `bin/echo-agent-core`
    /// (and, for standalone, `bin/echo-agent-panel` and `web/`).
    ///
    /// In deployment mode this is the durable **current selector** and must be
    /// a descendant of `state_dir`; the selected symlink target is resolved at
    /// service start by the trusted launcher.
    pub release_root: PathBuf,
    pub core_config: PathBuf,
    /// Required for `standalone`; rejected for `core_only`/`embedded`.
    #[serde(default)]
    pub panel_config: Option<PathBuf>,
    pub topology: InstallTopology,
    /// Absolute path to the deployment tool used by the updater unit.
    /// Serde also accepts `executable` and `deployment_tool`.
    #[serde(alias = "executable", alias = "deployment_tool")]
    pub deploy_executable: PathBuf,
    pub state_dir: PathBuf,
    /// Opt-in real deployment runtime. `None` keeps the legacy frozen-root
    /// rendering byte-identical and never enables the control socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentRuntime>,
}

impl InstallSpec {
    /// Validate the lexical contract. No filesystem access is performed.
    pub fn validate(&self) -> Result<()> {
        validate_instance_id(&self.instance_id)?;
        validate_absolute_path(&self.release_root, "release_root")?;
        validate_executable_path(&self.release_root, "release_root")?;
        validate_absolute_path(&self.core_config, "core_config")?;
        if let Some(panel_config) = &self.panel_config {
            validate_absolute_path(panel_config, "panel_config")?;
            ensure!(
                panel_config != &self.core_config,
                "panel_config must differ from core_config"
            );
        }
        validate_absolute_path(&self.deploy_executable, "deploy_executable")?;
        validate_executable_path(&self.deploy_executable, "deploy_executable")?;
        validate_absolute_path(&self.state_dir, "state_dir")?;
        match self.topology {
            InstallTopology::CoreOnly | InstallTopology::Embedded => ensure!(
                self.panel_config.is_none(),
                "topology {} must not specify panel_config",
                self.topology.as_str()
            ),
            InstallTopology::Standalone => ensure!(
                self.panel_config.is_some(),
                "topology standalone requires panel_config"
            ),
        }
        if let Some(deployment) = &self.deployment {
            deployment.validate(self)?;
        }
        Ok(())
    }

    pub fn unit_names(&self) -> UnitNames {
        let base = format!("echo-agent-{}", self.instance_id);
        let standalone = self.topology == InstallTopology::Standalone;
        UnitNames {
            core: format!("{base}-core.service"),
            panel: standalone.then(|| format!("{base}-panel.service")),
            updater: format!("{base}-update.service"),
            core_launcher: base.clone(),
            panel_launcher: standalone.then(|| format!("{base}-panel")),
        }
    }

    /// Current release root. In deployment mode this is the durable selector
    /// symlink; the launcher resolves its target at exec time.
    pub fn current_selector(&self) -> Option<&Path> {
        self.deployment
            .as_ref()
            .map(|_| self.release_root.as_path())
    }

    pub fn core_launcher_path(&self) -> Option<PathBuf> {
        self.deployment.as_ref().map(|deployment| {
            deployment
                .launcher_directory
                .join(&self.unit_names().core_launcher)
        })
    }

    pub fn panel_launcher_path(&self) -> Option<PathBuf> {
        let names = self.unit_names();
        let deployment = self.deployment.as_ref()?;
        names
            .panel_launcher
            .map(|name| deployment.launcher_directory.join(name))
    }

    /// The systemd backend invokes this path with no arguments and a JSON
    /// request on stdin. The rendered core launcher multiplexes on
    /// `ECHO_DEPLOY_CONTROL_PROTOCOL`.
    pub fn control_adapter_path(&self) -> Option<PathBuf> {
        self.core_launcher_path()
    }

    pub fn update_request_path(&self) -> PathBuf {
        self.state_dir.join(UPDATE_REQUEST_FILE)
    }

    /// Exact strict JSON document for `echo-deploy runtime-adapter --config`.
    /// The caller writes it with mode 0600; this module never creates it.
    pub fn runtime_adapter_config(&self) -> Result<Option<RuntimeAdapterConfig>> {
        self.validate()?;
        let Some(deployment) = &self.deployment else {
            return Ok(None);
        };
        let names = self.unit_names();
        Ok(Some(RuntimeAdapterConfig {
            instance_id: self.instance_id.clone(),
            core_socket: deployment.core_socket.clone(),
            panel_gate_file: deployment.panel_gate_file.clone(),
            instance_state: deployment.instance_state.clone(),
            current_selector: self.release_root.clone(),
            release_store: deployment.release_store.clone(),
            required_modules: deployment.required_modules.clone(),
            acknowledge_external_writers_stopped: deployment.acknowledge_external_writers_stopped,
            first_install: FirstInstallRuntimeConfig {
                // Fixed by SystemdTransactionBackend::persist_first_install_evidence.
                evidence: self.state_dir.join("first-install-evidence.json"),
                core_unit: names.core,
                panel_unit: names.panel,
            },
        }))
    }

    /// Initial Panel maintenance gate document. `release_id` and
    /// `management_protocol` must come from the verified release manifest.
    pub fn panel_gate_document(
        &self,
        release_id: &str,
        management_protocol: &str,
    ) -> Result<Option<PanelGateDocument>> {
        self.validate()?;
        let Some(deployment) = &self.deployment else {
            return Ok(None);
        };
        if deployment.panel_gate_file.is_none() {
            return Ok(None);
        }
        validate_release_id(release_id, "panel gate release_id")?;
        ensure!(
            !release_id.starts_with("pending-"),
            "panel gate must not declare a pending release"
        );
        validate_release_id(management_protocol, "panel gate management_protocol")?;
        ensure!(
            management_protocol.len() <= MAX_MANAGEMENT_PROTOCOL_BYTES,
            "panel gate management_protocol exceeds {MAX_MANAGEMENT_PROTOCOL_BYTES} bytes"
        );
        Ok(Some(PanelGateDocument {
            schema_version: 1,
            instance_id: self.instance_id.clone(),
            maintenance: false,
            release_id: release_id.to_owned(),
            management_protocol: management_protocol.to_owned(),
        }))
    }

    fn deployment_plan(&self) -> Result<Option<DeploymentRuntimePlan>> {
        let Some(deployment) = &self.deployment else {
            return Ok(None);
        };
        let core_launcher = self
            .core_launcher_path()
            .context("deployment core launcher path")?;
        let panel_launcher = self.panel_launcher_path();
        Ok(Some(DeploymentRuntimePlan {
            current_selector: self.release_root.clone(),
            release_store: deployment.release_store.clone(),
            instance_state: deployment.instance_state.clone(),
            core_socket: deployment.core_socket.clone(),
            runtime_config: deployment.runtime_config.clone(),
            panel_gate_file: deployment.panel_gate_file.clone(),
            first_install_evidence: self.state_dir.join("first-install-evidence.json"),
            core_launcher: core_launcher.clone(),
            panel_launcher,
            control_adapter: core_launcher,
            release_identity: "selector_derived_declaration_not_build_id",
        }))
    }

    pub fn core_binary(&self) -> PathBuf {
        self.release_root.join(CORE_BINARY_RELATIVE)
    }

    pub fn panel_binary(&self) -> PathBuf {
        self.release_root.join(PANEL_BINARY_RELATIVE)
    }

    /// Render without touching the filesystem.
    pub fn render(&self) -> Result<RenderedInstall> {
        render_install(self)
    }

    /// Write a plan preview into a new directory. See [`write_install`].
    pub fn write_install(&self, output: &Path) -> Result<InstallReport> {
        write_install(self, output)
    }
}

/// Names derived from `instance_id` and topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitNames {
    pub core: String,
    pub panel: Option<String>,
    pub updater: String,
    pub core_launcher: String,
    pub panel_launcher: Option<String>,
}

/// Planned file ownership. `write_install` cannot chown a non-root process;
/// this is plan metadata for the eventual privileged installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOwner {
    #[serde(rename = "invoking-user")]
    InvokingUser,
    Root,
}

impl InstallOwner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvokingUser => "invoking-user",
            Self::Root => "root",
        }
    }
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallFile {
    /// File name inside the plan directory; no directory components.
    pub name: String,
    pub contents: String,
    pub mode: u32,
    pub owner: InstallOwner,
}

/// Rendered, not installed.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedInstall {
    pub schema_version: u32,
    pub instance_id: String,
    pub scope: InstallScope,
    pub topology: InstallTopology,
    pub unit_names: UnitNames,
    pub files: Vec<InstallFile>,
    pub plan_preview: bool,
    pub activation_supported: bool,
    /// Present only when the spec opted into a deployment runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentRuntimePlan>,
    pub pending_activation_requirements: Vec<String>,
}

impl RenderedInstall {
    pub fn file(&self, name: &str) -> Option<&InstallFile> {
        self.files.iter().find(|file| file.name == name)
    }

    pub fn core_unit(&self) -> Option<&InstallFile> {
        self.file(&self.unit_names.core)
    }

    pub fn panel_unit(&self) -> Option<&InstallFile> {
        self.unit_names
            .panel
            .as_deref()
            .and_then(|name| self.file(name))
    }

    pub fn updater_unit(&self) -> Option<&InstallFile> {
        self.file(&self.unit_names.updater)
    }

    /// Live unit directory is only a hint for the future privileged installer.
    pub fn unit_dir_hint(&self) -> &'static str {
        if self.scope.is_system() {
            "/etc/systemd/system"
        } else {
            "~/.config/systemd/user"
        }
    }

    /// Launcher directory hint; the plan directory itself is not a PATH entry.
    pub fn launcher_dir_hint(&self) -> &'static str {
        if self.scope.is_system() {
            "/usr/local/bin"
        } else {
            "~/.local/bin"
        }
    }
}

/// Result of writing a plan preview. `plan_preview` is always true.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub schema_version: u32,
    pub output_dir: PathBuf,
    pub written_files: Vec<PathBuf>,
    pub activation_supported: bool,
    pub plan_preview: bool,
    pub notes: Vec<String>,
}

/// Render unit and launcher text. No filesystem access.
pub fn render_install(spec: &InstallSpec) -> Result<RenderedInstall> {
    spec.validate()?;
    let names = spec.unit_names();
    let owner = match spec.scope {
        InstallScope::User => InstallOwner::InvokingUser,
        InstallScope::System => InstallOwner::Root,
    };

    let mut files = Vec::with_capacity(if spec.topology == InstallTopology::Standalone {
        5
    } else {
        3
    });
    files.push(render_core_unit(spec, &names, owner)?);
    if spec.topology == InstallTopology::Standalone {
        files.push(render_panel_unit(spec, &names, owner)?);
    }
    files.push(render_updater_unit(spec, &names, owner)?);
    files.push(render_core_launcher(spec, &names, owner)?);
    if spec.topology == InstallTopology::Standalone {
        files.push(render_panel_launcher(spec, &names, owner)?);
    }

    let mut pending_activation_requirements = vec![
        "runtime activation gate (not implemented)".into(),
        "live systemd unit installation, daemon-reload, enable and start (not implemented)".into(),
        "release signature trust, health check and rollback transaction (not implemented)".into(),
        "config/data migration and compatibility checks (not implemented)".into(),
    ];
    if let Some(deployment) = &spec.deployment {
        pending_activation_requirements.push(format!(
            "create {} and {} with mode 0700 before starting units",
            deployment.instance_state.display(),
            spec.state_dir.display()
        ));
        let plan = spec.deployment_plan()?.context("deployment plan")?;
        pending_activation_requirements.push(format!(
            "install launchers at {} (and {}) with mode 0755; ExecStart and the control adapter point there",
            plan.core_launcher.display(),
            plan.panel_launcher
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "no panel launcher".into())
        ));
        pending_activation_requirements.push(format!(
            "write the strict runtime adapter config {} (mode 0600) from InstallSpec::runtime_adapter_config",
            deployment.runtime_config.display()
        ));
        if let Some(gate) = &deployment.panel_gate_file {
            pending_activation_requirements.push(format!(
                "create Panel maintenance gate {} before the first prepare",
                gate.display()
            ));
        }
        pending_activation_requirements.push(
            "ECHO_RELEASE_ID is derived from the current selector by the launcher; it is a declaration, not a build ID, and Ready still hashes /proc/<pid>/exe".into(),
        );
        pending_activation_requirements.push(
            "the updater consumes update-request.json via apply-request; the execution document selects a strict administrator adapter or systemd backend"
                .into(),
        );
    }

    Ok(RenderedInstall {
        schema_version: INSTALL_SCHEMA_VERSION,
        instance_id: spec.instance_id.clone(),
        scope: spec.scope,
        topology: spec.topology,
        unit_names: names,
        files,
        plan_preview: true,
        activation_supported: ACTIVATION_SUPPORTED,
        deployment: spec.deployment_plan()?,
        pending_activation_requirements,
    })
}

/// Alias for callers that prefer a shorter name.
pub fn render(spec: &InstallSpec) -> Result<RenderedInstall> {
    render_install(spec)
}

/// Write a render-only plan preview into a new directory.
///
/// The directory must not exist. Its parent must exist and be reachable without
/// following a symlink. The output is rejected if it overlaps the release root,
/// core/panel config paths, state dir, or deployment executable. Existing unit
/// files, config files and data files are never modified. On an I/O failure a
/// partially written new directory may remain; it is never loaded or activated.
pub fn write_install(spec: &InstallSpec, output: &Path) -> Result<InstallReport> {
    let rendered = render_install(spec)?;
    validate_output_path(spec, output)?;
    let (parent, dir) = create_new_directory(output)?;
    let mut written_files = Vec::with_capacity(rendered.files.len());
    for file in &rendered.files {
        write_new_file(&dir, &file.name, file.contents.as_bytes(), file.mode)?;
        written_files.push(output.join(&file.name));
    }
    sync_dir(&dir).context("fsync plan directory")?;
    sync_dir(&parent).context("fsync plan parent")?;
    let mut notes = vec![
        "render-only plan preview: no unit was loaded, enabled or started".into(),
        "no service-manager invocation and no modification of configured config/data files".into(),
        "owner metadata is for the future privileged installer; write_install does not chown"
            .into(),
    ];
    if spec.deployment.is_some() {
        notes.push(
            "deployment state/instance directories, the runtime adapter config and the Panel gate file are validated but never created by write_install"
                .into(),
        );
        notes.push(
            "ECHO_DEPLOY_CONTROL_SOCKET is enabled only for this explicitly opted-in deployment spec"
                .into(),
        );
    }
    Ok(InstallReport {
        schema_version: INSTALL_SCHEMA_VERSION,
        output_dir: output.to_path_buf(),
        written_files,
        activation_supported: ACTIVATION_SUPPORTED,
        plan_preview: true,
        notes,
    })
}

/// Deployment-only `ExecStartPre` migration activation guard.
///
/// Unit of work: one fsynced `migration-activated-<instance>.json` write before
/// every Core/Panel/updater start. The write is unconditional, without requiring
/// migration evidence, so once any deployment unit has started,
/// `migration-recover` can never restore the legacy units. Repeated starts
/// rewrite the same marker; the three units need no coordination beyond it.
///
/// A failed guard blocks that unit's start; a crash after fsync leaves the
/// marker behind, which fails closed against legacy recovery. The generated line
/// invokes the explicit `deploy_executable`; this renderer never runs it. The
/// marker proves the command succeeded, not that any data migration is complete.
///
/// The legacy `deployment = None` render emits no `ExecStartPre` at all and
/// keeps its byte-identical output.
fn write_activation_guard(out: &mut String, spec: &InstallSpec, unit: &str) -> Result<()> {
    if spec.deployment.is_none() {
        return Ok(());
    }
    writeln!(
        out,
        "{}",
        crate::migration::activation_guard_line(
            &spec.deploy_executable,
            &spec.state_dir,
            &spec.instance_id,
            unit,
        )?
    )?;
    Ok(())
}

fn render_core_unit(
    spec: &InstallSpec,
    names: &UnitNames,
    owner: InstallOwner,
) -> Result<InstallFile> {
    let mut out = String::new();
    writeln!(out, "[Unit]")?;
    writeln!(out, "Description=EchoAgent Core ({})", spec.instance_id)?;
    writeln!(
        out,
        "Documentation=https://github.com/EchoLab-Auto/EchoAgentCore"
    )?;
    writeln!(out, "Wants=network-online.target")?;
    writeln!(out, "After=network-online.target")?;
    writeln!(out)?;
    writeln!(out, "[Service]")?;
    writeln!(out, "Type=simple")?;
    write_scope_lines(&mut out, spec.scope)?;
    if let Some(deployment) = &spec.deployment {
        writeln!(
            out,
            "WorkingDirectory={}",
            systemd_path_value(path_text(&deployment.core_root, "deployment.core_root")?)?
        )?;
        write_activation_guard(&mut out, spec, &names.core)?;
        writeln!(
            out,
            "ExecStart={}",
            systemd_executable_quote(path_text(
                &spec
                    .core_launcher_path()
                    .context("deployment core launcher")?,
                "core launcher"
            )?)?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment(
                "ECHO_DEPLOY_CONTROL_SOCKET",
                path_text(&deployment.core_socket, "deployment.core_socket")?
            )?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment("ECHO_SERVICE_SCOPE", spec.scope.as_str())?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment(
                "ECHO_UPDATE_STATUS_FILE",
                path_text(&spec.state_dir.join("update-status"), "update status")?
            )?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment("ECHO_UPDATE_UNIT", &names.updater,)?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment(
                "ECHO_UPDATE_TRANSACTION_FILE",
                path_text(
                    &spec
                        .state_dir
                        .join(format!("transaction-{}.json", spec.instance_id)),
                    "update transaction"
                )?,
            )?
        )?;
        writeln!(
            out,
            "# ECHO_RELEASE_ID is derived from the current selector by the trusted launcher."
        )?;
        writeln!(
            out,
            "# It is a declaration, never a build ID or proof of the running executable."
        )?;
    } else {
        writeln!(
            out,
            "WorkingDirectory={}",
            systemd_path_value(path_text(&spec.release_root, "release_root")?)?
        )?;
        writeln!(
            out,
            "ExecStart={} {} {}",
            systemd_executable_quote(path_text(&spec.core_binary(), "core binary")?)?,
            systemd_quote("--config")?,
            systemd_quote(path_text(&spec.core_config, "core_config")?)?
        )?;
    }
    write_common_environment(&mut out, spec)?;
    writeln!(out, "Restart=always")?;
    writeln!(out, "RestartSec=5s")?;
    writeln!(out, "TimeoutStopSec={CORE_TIMEOUT_STOP_SEC}")?;
    writeln!(out, "KillSignal=SIGTERM")?;
    writeln!(
        out,
        "# mixed: SIGTERM only the main process first; child tasks are not killed in parallel."
    )?;
    writeln!(out, "KillMode=mixed")?;
    writeln!(out, "NoNewPrivileges=true")?;
    writeln!(out, "PrivateTmp=true")?;
    writeln!(out, "UMask=0077")?;
    writeln!(out)?;
    writeln!(out, "[Install]")?;
    writeln!(out, "WantedBy={}", wanted_by(spec.scope))?;

    Ok(unit_file(names.core.clone(), out, owner))
}

fn render_panel_unit(
    spec: &InstallSpec,
    names: &UnitNames,
    owner: InstallOwner,
) -> Result<InstallFile> {
    let panel_config = spec
        .panel_config
        .as_ref()
        .context("standalone panel_config")?;
    let core_unit = names.core.as_str();

    let mut out = String::new();
    writeln!(out, "[Unit]")?;
    writeln!(out, "Description=EchoAgent Panel ({})", spec.instance_id)?;
    writeln!(
        out,
        "Documentation=https://github.com/EchoLab-Auto/EchoAgentCore"
    )?;
    writeln!(out, "Wants=network-online.target")?;
    writeln!(out, "After=network-online.target {core_unit}")?;
    writeln!(out)?;
    writeln!(out, "[Service]")?;
    writeln!(out, "Type=simple")?;
    write_scope_lines(&mut out, spec.scope)?;
    if let Some(deployment) = &spec.deployment {
        writeln!(
            out,
            "WorkingDirectory={}",
            systemd_path_value(path_text(
                deployment
                    .panel_root
                    .as_ref()
                    .context("deployment panel_root")?,
                "deployment.panel_root"
            )?)?
        )?;
        write_activation_guard(
            &mut out,
            spec,
            names.panel.as_deref().context("panel unit")?,
        )?;
        writeln!(
            out,
            "ExecStart={}",
            systemd_executable_quote(path_text(
                &spec
                    .panel_launcher_path()
                    .context("deployment panel launcher")?,
                "panel launcher"
            )?)?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment("ECHO_SERVICE_SCOPE", spec.scope.as_str())?
        )?;
        writeln!(
            out,
            "Environment={}",
            systemd_env_assignment(
                "ECHO_DEPLOY_GATE_FILE",
                path_text(
                    deployment
                        .panel_gate_file
                        .as_ref()
                        .context("standalone deployment gate")?,
                    "deployment.panel_gate_file"
                )?,
            )?
        )?;
    } else {
        writeln!(
            out,
            "WorkingDirectory={}",
            systemd_path_value(path_text(&spec.release_root, "release_root")?)?
        )?;
        writeln!(
            out,
            "ExecStart={} {} {}",
            systemd_executable_quote(path_text(&spec.panel_binary(), "panel binary")?)?,
            systemd_quote("--config")?,
            systemd_quote(path_text(panel_config, "panel_config")?)?
        )?;
    }
    write_common_environment(&mut out, spec)?;
    writeln!(out, "Restart=always")?;
    writeln!(out, "RestartSec=5s")?;
    writeln!(out, "TimeoutStopSec={PANEL_TIMEOUT_STOP_SEC}")?;
    writeln!(out, "KillSignal=SIGTERM")?;
    writeln!(
        out,
        "# mixed: SIGTERM only the main process first; child tasks are not killed in parallel."
    )?;
    writeln!(out, "KillMode=mixed")?;
    writeln!(out, "NoNewPrivileges=true")?;
    writeln!(out, "PrivateTmp=true")?;
    writeln!(out, "UMask=0077")?;
    writeln!(out)?;
    writeln!(out, "[Install]")?;
    writeln!(out, "WantedBy={}", wanted_by(spec.scope))?;

    Ok(unit_file(
        names.panel.clone().context("panel unit")?,
        out,
        owner,
    ))
}

fn render_updater_unit(
    spec: &InstallSpec,
    names: &UnitNames,
    owner: InstallOwner,
) -> Result<InstallFile> {
    // Real CLI contract (verified against `src/main.rs`): the updater runs
    // `echo-deploy apply-request --request <absolute update-request.json>
    // --confirm`. There is no `echo-deploy update` subcommand. The request is a
    // strict `UpdateRequest`; the caller must freeze it in state_dir first.
    let request = spec.update_request_path();
    let command = [
        path_text(&spec.deploy_executable, "deploy_executable")?.to_owned(),
        "apply-request".into(),
        "--request".into(),
        path_text(&request, "update_request")?.to_owned(),
        "--confirm".into(),
    ];
    let exec_start = command
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            if index == 0 {
                systemd_executable_quote(argument)
            } else {
                systemd_quote(argument)
            }
        })
        .collect::<Result<Vec<_>>>()?
        .join(" ");

    let mut out = String::new();
    writeln!(out, "[Unit]")?;
    writeln!(out, "Description=EchoAgent Updater ({})", spec.instance_id)?;
    writeln!(
        out,
        "Documentation=https://github.com/EchoLab-Auto/EchoAgentCore"
    )?;
    writeln!(out, "Wants=network-online.target")?;
    writeln!(out, "After=network-online.target")?;
    writeln!(
        out,
        "# Independent service: systemd places this unit in its own cgroup. It is not"
    )?;
    writeln!(
        out,
        "# a child of Core/Panel; restarting them cannot kill an in-flight updater."
    )?;
    writeln!(
        out,
        "# No PartOf or BindsTo lifecycle binding is emitted on purpose."
    )?;
    writeln!(out)?;
    writeln!(out, "[Service]")?;
    writeln!(out, "Type=oneshot")?;
    write_scope_lines(&mut out, spec.scope)?;
    writeln!(
        out,
        "# PLAN PREVIEW ONLY: write_install does not load, enable, or start this unit."
    )?;
    writeln!(
        out,
        "# Requires an explicit signed/development update request and a working runtime control adapter."
    )?;
    if spec.deployment.is_some() {
        writeln!(
            out,
            "# apply-request consumes an administrator-owned Execution hook document."
        )?;
        writeln!(
            out,
            "# apply-request validates the strict execution document and selects its adapter or systemd backend."
        )?;
    }
    writeln!(
        out,
        "WorkingDirectory={}",
        systemd_path_value(path_text(
            spec.deployment
                .as_ref()
                .map(|deployment| &deployment.core_root)
                .unwrap_or(&spec.release_root),
            "updater working directory"
        )?)?
    )?;
    write_activation_guard(&mut out, spec, &names.updater)?;
    writeln!(out, "ExecStart={exec_start}")?;
    write_common_environment(&mut out, spec)?;
    writeln!(out, "TimeoutStartSec={UPDATER_TIMEOUT_START_SEC}")?;
    writeln!(out, "TimeoutStopSec={UPDATER_TIMEOUT_STOP_SEC}")?;
    writeln!(out, "KillSignal=SIGTERM")?;
    writeln!(out, "KillMode=mixed")?;
    writeln!(out, "NoNewPrivileges=true")?;
    if spec.deployment.is_some() {
        // On systems that restrict unprivileged mount namespaces, PrivateTmp
        // transitions the updater into an AppArmor/userns profile that cannot
        // inspect the sibling service's /proc executable. Do not weaken the
        // image check or NoNewPrivileges to hide that denial.
        writeln!(
            out,
            "# Deployment updater must inspect sibling Core/Panel process images."
        )?;
        writeln!(out, "PrivateTmp=false")?;
    } else {
        writeln!(out, "PrivateTmp=true")?;
    }
    writeln!(out, "UMask=0077")?;

    Ok(unit_file(names.updater.clone(), out, owner))
}

fn render_core_launcher(
    spec: &InstallSpec,
    names: &UnitNames,
    owner: InstallOwner,
) -> Result<InstallFile> {
    let contents = if let Some(deployment) = &spec.deployment {
        render_deployment_core_launcher(spec, deployment)?
    } else {
        let mut out = String::new();
        writeln!(out, "#!/bin/sh")?;
        writeln!(
            out,
            "# Generated by echo-deploy as render-only plan text; not installed or started."
        )?;
        writeln!(out, "set -eu")?;
        writeln!(
            out,
            "exec {} --config {} \"$@\"",
            shell_quote(path_text(&spec.core_binary(), "core binary")?)?,
            shell_quote(path_text(&spec.core_config, "core_config")?)?
        )?;
        out
    };
    Ok(InstallFile {
        name: names.core_launcher.clone(),
        contents,
        mode: LAUNCHER_MODE,
        owner,
    })
}

fn render_deployment_core_launcher(
    spec: &InstallSpec,
    deployment: &DeploymentRuntime,
) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "#!/bin/sh")?;
    writeln!(
        out,
        "# Generated by echo-deploy as render-only plan text; not installed or started."
    )?;
    writeln!(
        out,
        "# Trusted administrator launcher: resolves the durable current selector at"
    )?;
    writeln!(
        out,
        "# exec time, so systemd never embeds a concrete release directory."
    )?;
    writeln!(out, "set -eu")?;
    writeln!(out)?;
    writeln!(
        out,
        "selector={}",
        shell_quote(path_text(&spec.release_root, "release_root")?)?
    )?;
    writeln!(
        out,
        "release_store={}",
        shell_quote(path_text(
            &deployment.release_store,
            "deployment.release_store"
        )?)?
    )?;
    writeln!(
        out,
        "core_config={}",
        shell_quote(path_text(&spec.core_config, "core_config")?)?
    )?;
    writeln!(
        out,
        "runtime_config={}",
        shell_quote(path_text(
            &deployment.runtime_config,
            "deployment.runtime_config"
        )?)?
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "# The systemd backend invokes this same trusted launcher with no arguments"
    )?;
    writeln!(
        out,
        "# and clears the environment except for this protocol marker."
    )?;
    writeln!(
        out,
        "if [ \"${{ECHO_DEPLOY_CONTROL_PROTOCOL:-}}\" = {} ]; then",
        shell_quote(CONTROL_PROTOCOL)?
    )?;
    writeln!(
        out,
        "    exec {} runtime-adapter --config \"$runtime_config\"",
        shell_quote(path_text(&spec.deploy_executable, "deploy_executable")?)?
    )?;
    writeln!(out, "fi")?;
    writeln!(out)?;
    write_selector_resolution(&mut out, "echo-agent-core")?;
    writeln!(out)?;
    writeln!(
        out,
        "# ECHO_RELEASE_ID is a declaration derived from the durable selector."
    )?;
    writeln!(
        out,
        "# It is not a build ID and does not prove which executable is running."
    )?;
    writeln!(out, "ECHO_RELEASE_ID=$release")?;
    writeln!(out, "export ECHO_RELEASE_ID")?;
    writeln!(out)?;
    writeln!(out, "has_config=0")?;
    writeln!(out, "for argument in \"$@\"; do")?;
    writeln!(out, "    case \"$argument\" in")?;
    writeln!(out, "        -c|--config|--config=*) has_config=1 ;;")?;
    writeln!(out, "    esac")?;
    writeln!(out, "done")?;
    writeln!(out, "if [ \"$has_config\" -eq 1 ]; then")?;
    writeln!(out, "    exec \"$target/bin/echo-agent-core\" \"$@\"")?;
    writeln!(out, "fi")?;
    writeln!(
        out,
        "exec \"$target/bin/echo-agent-core\" --config \"$core_config\" \"$@\""
    )?;
    Ok(out)
}

fn render_panel_launcher(
    spec: &InstallSpec,
    names: &UnitNames,
    owner: InstallOwner,
) -> Result<InstallFile> {
    let panel_config = spec
        .panel_config
        .as_ref()
        .context("standalone panel_config")?;
    let contents = if let Some(deployment) = &spec.deployment {
        render_deployment_panel_launcher(spec, deployment, panel_config)?
    } else {
        let mut out = String::new();
        writeln!(out, "#!/bin/sh")?;
        writeln!(
            out,
            "# Generated by echo-deploy as render-only plan text; not installed or started."
        )?;
        writeln!(out, "set -eu")?;
        writeln!(
            out,
            "exec {} --config {} \"$@\"",
            shell_quote(path_text(&spec.panel_binary(), "panel binary")?)?,
            shell_quote(path_text(panel_config, "panel_config")?)?
        )?;
        out
    };
    Ok(InstallFile {
        name: names.panel_launcher.clone().context("panel launcher")?,
        contents,
        mode: LAUNCHER_MODE,
        owner,
    })
}

fn render_deployment_panel_launcher(
    spec: &InstallSpec,
    deployment: &DeploymentRuntime,
    panel_config: &Path,
) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "#!/bin/sh")?;
    writeln!(
        out,
        "# Generated by echo-deploy as render-only plan text; not installed or started."
    )?;
    writeln!(
        out,
        "# Trusted administrator launcher: resolves the durable current selector at"
    )?;
    writeln!(
        out,
        "# exec time, so systemd never embeds a concrete release directory."
    )?;
    writeln!(out, "set -eu")?;
    writeln!(out)?;
    writeln!(
        out,
        "selector={}",
        shell_quote(path_text(&spec.release_root, "release_root")?)?
    )?;
    writeln!(
        out,
        "release_store={}",
        shell_quote(path_text(
            &deployment.release_store,
            "deployment.release_store"
        )?)?
    )?;
    writeln!(
        out,
        "panel_config={}",
        shell_quote(path_text(panel_config, "panel_config")?)?
    )?;
    writeln!(out)?;
    write_selector_resolution(&mut out, "echo-agent-panel")?;
    writeln!(out)?;
    writeln!(out, "if [ ! -d \"$target/web\" ]; then")?;
    writeln!(
        out,
        "    echo \"$0: selected release has no web/ directory: $target\" >&2"
    )?;
    writeln!(out, "    exit 1")?;
    writeln!(out, "fi")?;
    writeln!(
        out,
        "exec \"$target/bin/echo-agent-panel\" --config \"$panel_config\" --static-dir \"$target/web\" \"$@\""
    )?;
    Ok(out)
}

/// Emit the shared selector resolution. The release id is validated exactly
/// like `model::safe_id`; the target must remain a direct child of the release
/// store, so a tampered selector cannot escape into an arbitrary executable.
fn write_selector_resolution(out: &mut String, binary: &str) -> Result<()> {
    writeln!(out, "if ! target=$(readlink \"$selector\"); then")?;
    writeln!(
        out,
        "    echo \"$0: cannot read current selector: $selector\" >&2"
    )?;
    writeln!(out, "    exit 1")?;
    writeln!(out, "fi")?;
    writeln!(out, "case \"$target\" in")?;
    writeln!(out, "    \"$release_store\"/*) ;;")?;
    writeln!(out, "    *)")?;
    writeln!(
        out,
        "        echo \"$0: selector target is outside release_store: $target\" >&2"
    )?;
    writeln!(out, "        exit 1")?;
    writeln!(out, "        ;;")?;
    writeln!(out, "esac")?;
    writeln!(out, "release=${{target#\"$release_store\"/}}")?;
    writeln!(out, "case \"$release\" in")?;
    writeln!(out, "    ''|.*|*[!A-Za-z0-9._-]*)")?;
    writeln!(
        out,
        "        echo \"$0: unsafe release id from selector: $release\" >&2"
    )?;
    writeln!(out, "        exit 1")?;
    writeln!(out, "        ;;")?;
    writeln!(out, "esac")?;
    writeln!(
        out,
        "if [ \"${{#release}}\" -gt {MAX_RELEASE_ID_BYTES} ]; then"
    )?;
    writeln!(
        out,
        "    echo \"$0: release id from selector is too long\" >&2"
    )?;
    writeln!(out, "    exit 1")?;
    writeln!(out, "fi")?;
    writeln!(
        out,
        "if [ ! -f \"$target/bin/{binary}\" ] || [ ! -x \"$target/bin/{binary}\" ]; then"
    )?;
    writeln!(
        out,
        "    echo \"$0: selected release has no executable bin/{binary}: $target\" >&2"
    )?;
    writeln!(out, "    exit 1")?;
    writeln!(out, "fi")?;
    Ok(())
}

fn unit_file(name: String, contents: String, owner: InstallOwner) -> InstallFile {
    InstallFile {
        name,
        contents,
        mode: UNIT_MODE,
        owner,
    }
}

fn write_scope_lines(out: &mut String, scope: InstallScope) -> Result<()> {
    match scope {
        InstallScope::User => {
            writeln!(
                out,
                "# Scope: user. Runs as the invoking user via systemd --user; no User=/Group= is rendered."
            )?;
        }
        InstallScope::System => {
            writeln!(
                out,
                "# Scope: system. System units default to root; User=/Group= are explicit."
            )?;
            writeln!(out, "User=root")?;
            writeln!(out, "Group=root")?;
        }
    }
    Ok(())
}

fn write_common_environment(out: &mut String, spec: &InstallSpec) -> Result<()> {
    writeln!(
        out,
        "Environment={}",
        systemd_env_assignment("ECHO_AGENT_INSTANCE", &spec.instance_id)?
    )?;
    writeln!(
        out,
        "Environment={}",
        systemd_env_assignment("ECHO_AGENT_SCOPE", spec.scope.as_str())?
    )?;
    writeln!(
        out,
        "Environment={}",
        systemd_env_assignment(
            "ECHO_AGENT_STATE_DIR",
            path_text(&spec.state_dir, "state_dir")?
        )?
    )?;
    Ok(())
}

fn wanted_by(scope: InstallScope) -> &'static str {
    match scope {
        InstallScope::User => "default.target",
        InstallScope::System => "multi-user.target",
    }
}

/// Quote one systemd command-line argument.
///
/// The result is one double-quoted token with `\`, `"`, `%` and `$` escaped for
/// systemd's parser. Newlines and all other control characters are rejected so
/// a path can never inject a new unit directive or command line. This is used
/// for every `ExecStart=` argument, including the executable itself.
pub fn systemd_quote(value: &str) -> Result<String> {
    validate_no_control(value, "systemd argument")?;
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            _ => out.push(character),
        }
    }
    out.push('"');
    Ok(out)
}

/// Quote the first `ExecStart=` argument (the executable path).
///
/// systemd applies stricter rules to this token than to ordinary arguments:
/// variable expansion is not performed, and the parsed executable name may not
/// contain `"`, `'` or `\`. Spaces, `%`, `$`, `;`, `#` and similar characters
/// are accepted. A path containing one of the rejected characters fails closed
/// instead of producing a unit that `systemd-analyze verify` would refuse.
pub fn systemd_executable_quote(value: &str) -> Result<String> {
    validate_no_control(value, "systemd executable")?;
    ensure!(!value.is_empty(), "empty executable path");
    ensure!(
        !value
            .chars()
            .any(|character| matches!(character, '"' | '\'' | '\\')),
        "executable path contains a character systemd rejects in ExecStart's first argument"
    );
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '%' => out.push_str("%%"),
            _ => out.push(character),
        }
    }
    out.push('"');
    Ok(out)
}

/// Escape a value for settings such as `WorkingDirectory=`. These settings
/// accept C-style escapes but not surrounding shell quotes. Spaces and quotes
/// are emitted as `\x20`/`\x22`; backslashes are doubled; `%` is doubled for
/// systemd specifier expansion. `$` is literal for path settings.
pub fn systemd_path_value(value: &str) -> Result<String> {
    validate_no_control(value, "systemd path")?;
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("\\x20"),
            '"' => out.push_str("\\x22"),
            '%' => out.push_str("%%"),
            _ => out.push(character),
        }
    }
    Ok(out)
}

/// Render `Environment="KEY=value"` with systemd-safe escaping.
pub fn systemd_env_assignment(key: &str, value: &str) -> Result<String> {
    validate_env_key(key)?;
    validate_no_control(value, "environment value")?;
    let mut out = String::with_capacity(key.len() + value.len() + 4);
    out.push('"');
    out.push_str(key);
    out.push('=');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            _ => out.push(character),
        }
    }
    out.push('"');
    Ok(out)
}

/// Quote one POSIX shell word (used by generated launcher scripts).
pub fn shell_quote(value: &str) -> Result<String> {
    validate_no_control(value, "shell argument")?;
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for character in value.chars() {
        if character == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(character);
        }
    }
    out.push('\'');
    Ok(out)
}

fn validate_instance_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 100,
        "invalid instance_id length"
    );
    ensure!(
        value.as_bytes()[0].is_ascii_alphanumeric(),
        "instance_id must start with an ASCII alphanumeric character"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') }),
        "unsafe instance_id: {value:?}"
    );
    ensure!(
        !value.contains(".."),
        "instance_id must not contain consecutive dots"
    );
    Ok(())
}

/// Mirror of `model::safe_id` for release ids. The launcher applies the same
/// rule in shell before deriving `ECHO_RELEASE_ID` from the selector.
fn validate_release_id(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_RELEASE_ID_BYTES,
        "{field} has invalid length"
    );
    ensure!(
        value.as_bytes()[0].is_ascii_alphanumeric(),
        "{field} must start with an ASCII alphanumeric character"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "{field} contains an unsafe character: {value:?}"
    );
    Ok(())
}

/// Absolute, lexical, normal path: no root, no `.`/`..`, no empty component,
/// no trailing slash, no control characters, UTF-8.
pub fn validate_absolute_path(path: &Path, field: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{field} must be absolute");
    let text = path
        .to_str()
        .with_context(|| format!("{field} must be UTF-8"))?;
    ensure!(
        text.len() <= 4096,
        "{field} exceeds the maximum supported length"
    );
    ensure!(!text.is_empty() && text != "/", "{field} must not be root");
    ensure!(
        !text.chars().any(char::is_control),
        "{field} contains a control character"
    );
    let mut parts = text.split('/');
    ensure!(parts.next() == Some(""), "{field} must be absolute");
    for part in parts {
        ensure!(!part.is_empty(), "{field} contains an empty component");
        ensure!(
            part != "." && part != "..",
            "{field} contains a non-normal component"
        );
    }
    ensure!(path.parent().is_some(), "{field} must have a parent");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "{field} contains a non-normal component"
    );
    Ok(())
}

fn validate_no_control(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.chars().any(char::is_control),
        "{field} contains a newline or other control character"
    );
    Ok(())
}

fn validate_executable_path(path: &Path, field: &str) -> Result<()> {
    let text = path_text(path, field)?;
    ensure!(
        !text
            .chars()
            .any(|character| matches!(character, '"' | '\'' | '\\')),
        "{field} contains a character systemd rejects in ExecStart's first argument"
    );
    Ok(())
}

fn validate_env_key(key: &str) -> Result<()> {
    ensure!(!key.is_empty(), "empty environment key");
    ensure!(
        key.as_bytes()[0].is_ascii_alphabetic() || key.as_bytes()[0] == b'_',
        "invalid environment key"
    );
    ensure!(
        key.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "invalid environment key: {key:?}"
    );
    Ok(())
}

fn path_text<'a>(path: &'a Path, field: &str) -> Result<&'a str> {
    let text = path
        .to_str()
        .with_context(|| format!("{field} must be UTF-8"))?;
    validate_no_control(text, field)?;
    Ok(text)
}

fn validate_output_path(spec: &InstallSpec, output: &Path) -> Result<()> {
    validate_absolute_path(output, "output directory")?;
    let mut protected = vec![
        ("release_root", spec.release_root.as_path()),
        ("state_dir", spec.state_dir.as_path()),
        ("deploy_executable", spec.deploy_executable.as_path()),
    ];
    if let Some(deployment) = &spec.deployment {
        protected.extend([
            ("deployment.core_root", deployment.core_root.as_path()),
            (
                "deployment.launcher_directory",
                deployment.launcher_directory.as_path(),
            ),
            (
                "deployment.release_store",
                deployment.release_store.as_path(),
            ),
            (
                "deployment.instance_state",
                deployment.instance_state.as_path(),
            ),
            ("deployment.core_socket", deployment.core_socket.as_path()),
            (
                "deployment.runtime_config",
                deployment.runtime_config.as_path(),
            ),
        ]);
        if let Some(panel_root) = &deployment.panel_root {
            protected.push(("deployment.panel_root", panel_root.as_path()));
        }
        if let Some(gate) = &deployment.panel_gate_file {
            protected.push(("deployment.panel_gate_file", gate.as_path()));
        }
    }
    for (label, path) in protected {
        ensure!(
            !same_or_descendant(output, path),
            "output directory must not overlap {label}"
        );
        ensure!(
            !same_or_descendant(path, output),
            "output directory must not contain {label}"
        );
    }
    for (label, config) in [
        ("core_config", Some(spec.core_config.as_path())),
        ("panel_config", spec.panel_config.as_deref()),
    ] {
        let Some(config) = config else { continue };
        ensure!(
            !same_or_descendant(output, config),
            "output directory must not overlap {label}"
        );
        ensure!(
            !same_or_descendant(config, output),
            "output directory must not contain {label}"
        );
    }
    Ok(())
}

fn same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
}

fn create_new_directory(path: &Path) -> Result<(File, File)> {
    let parent_path = path.parent().context("output must have a parent")?;
    let parent = open_dir_nofollow(parent_path)?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("output directory name must be UTF-8")?;
    validate_component(leaf)?;
    let leaf_c = CString::new(leaf)?;
    // SAFETY: valid directory fd and NUL-terminated single component. mkdirat
    // does not follow symlinks in the final component.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), leaf_c.as_ptr(), PLAN_DIR_MODE) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("refusing to reuse output directory {leaf:?}"));
    }
    // SAFETY: the directory was just created; open it without following links.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let dir = checked_fd(fd).with_context(|| format!("open new plan directory {leaf:?}"))?;
    // SAFETY: valid fd; fchmod only changes permission bits.
    let result = unsafe { libc::fchmod(dir.as_raw_fd(), PLAN_DIR_MODE) };
    ensure!(
        result == 0,
        "fchmod plan directory: {}",
        std::io::Error::last_os_error()
    );
    Ok((parent, dir))
}

fn write_new_file(dir: &File, name: &str, contents: &[u8], mode: u32) -> Result<()> {
    validate_component(name)?;
    let name_c = CString::new(name)?;
    // SAFETY: valid directory fd, single component, O_EXCL prevents overwrite,
    // O_NOFOLLOW prevents a symlink race in the new directory.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    let mut file = checked_fd(fd).with_context(|| format!("create plan file {name:?}"))?;
    file.write_all(contents)?;
    // SAFETY: valid fd; set the planned mode explicitly instead of relying on
    // the process umask.
    let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    ensure!(
        result == 0,
        "fchmod plan file {name:?}: {}",
        std::io::Error::last_os_error()
    );
    file.sync_all()?;
    Ok(())
}

fn validate_component(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "empty file name");
    ensure!(name != "." && name != "..", "invalid file name");
    ensure!(!name.contains('/'), "file name must not contain '/'");
    ensure!(
        !name.chars().any(char::is_control),
        "file name contains a control character"
    );
    Ok(())
}

fn open_dir_nofollow(path: &Path) -> Result<File> {
    // The output itself may not be root, but its parent can legitimately be
    // "/" (for example a plan directory directly below the filesystem root).
    if path != Path::new("/") {
        validate_absolute_path(path, "directory")?;
    }
    let root = CString::new("/")?;
    // SAFETY: valid C string and open flags; returned fd is owned.
    let mut dir = checked_fd(unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            bail!("directory contains a non-normal component");
        };
        let name = name.to_str().context("directory component must be UTF-8")?;
        let name_c = CString::new(name)?;
        // SAFETY: single component with O_NOFOLLOW, so ancestor symlinks are
        // rejected instead of silently redirecting the plan write.
        let next = checked_fd(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        });
        dir = next.with_context(|| format!("open directory component {name:?}"))?;
    }
    let dot = CString::new(".")?;
    // O_PATH descriptors cannot be fsynced; reopen the pinned directory read-only.
    // SAFETY: valid directory fd and single "." component.
    checked_fd(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
    .with_context(|| format!("open directory {path:?}"))
}

fn checked_fd(fd: libc::c_int) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful open/openat returns a new owned fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn sync_dir(dir: &File) -> Result<()> {
    // SAFETY: valid read-only directory fd.
    let result = unsafe { libc::fsync(dir.as_raw_fd()) };
    ensure!(
        result == 0,
        "fsync directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
