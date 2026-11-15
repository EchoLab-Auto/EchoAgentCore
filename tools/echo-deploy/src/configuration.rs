//! Cross-document configuration consistency for one echo-deploy installation.
//!
//! Three administrator-owned documents describe the same deployment from three
//! angles:
//!
//! * [`InstallSpec`] (`install.json`) renders units and launchers,
//! * [`SystemdExecution`] (`systemd-execution.json`) drives the transaction
//!   backend,
//! * [`RuntimeConfig`] (`runtime-adapter.json`) is what the trusted launcher
//!   hands to `echo-deploy runtime-adapter`.
//!
//! Each document is validated on its own, but the equalities *between* them are
//! exactly where a split-brain deployment hides ("transaction points at A,
//! adapter reads B"). This module checks those equalities. It is the
//! implementation behind the manual table in `DESIGN.md` §2.5.
//!
//! # What is checked
//!
//! * `instance_id`, topology and scope across install and systemd deployment;
//! * `install.state_dir == systemd.state_dir`, and transaction state must not
//!   overlap the adapter's private `instance_state` (both take the same state
//!   lock name);
//! * `install.release_root == systemd.current_selector == runtime.current_selector`;
//! * `systemd.deployment.store == install.deployment.release_store == runtime.release_store`;
//! * `install.deployment.instance_state / core_socket / panel_gate_file` equal
//!   the matching [`RuntimeConfig`] fields;
//! * canonical unit names of install and systemd equal
//!   `runtime.first_install.{core,panel}_unit` when first install is configured;
//! * `systemd.control_adapter == <launcher_directory>/<core launcher>`;
//! * `required_modules` and `acknowledge_external_writers_stopped` are identical
//!   in install deployment, systemd deployment and runtime adapter;
//! * `runtime.first_install.evidence == <state_dir>/first-install-evidence.json`,
//!   the fixed path written by
//!   `SystemdTransactionBackend::persist_first_install_evidence`.
//!
//! # Strictness
//!
//! This is a *strict production* cross-check: `install.deployment` must be
//! present. A legacy [`InstallSpec`] without a deployment runtime renders a
//! frozen `release_root` and has no systemd/runtime declaration to compare; it
//! is rejected instead of guessing that the other two documents are "probably
//! the same". `deploy_executable` is only checked as an absolute executable
//! *path*: this module never stats or hashes the real file, so a
//! `deploy_executable_file_hash_verified=false` result is explicit, not an
//! omission.
//!
//! # First install is optional, never assumed
//!
//! [`RuntimeConfig::first_install`] is an opt-in document. The check accepts
//! `None` and reports `first_install_configured=false`: that deployment is
//! upgrade-only, and a missing Core socket must never be read as an empty
//! installation. When the block is present, its fixed evidence path and unit
//! names are cross-checked. It is never promoted to a mandatory field here.
//!
//! # Purity
//!
//! [`check`] does not write files, lock state, connect to the Core socket,
//! invoke `systemctl`, read a release manifest, or hash any executable. The
//! only filesystem work is what the existing validators already do:
//! `InstallSpec::validate` is lexical, `SystemdExecution::validate` is lexical,
//! and `RuntimeConfig::validate` stats the private runtime/store/evidence
//! directories (so tests need real temp fixtures with owner-only modes).
//!
//! `RuntimeConfig::validate` is a public, read-only entry point. `check`
//! calls it directly; no unsupported-protocol probe or lock/socket side effect
//! is needed.
//!
//! A successful [`Report`] therefore means "declared configuration documents
//! are mutually consistent and each passed its own validator". It does **not**
//! mean the services are running, the selected release content matches a
//! manifest, or `/proc/<pid>/exe` is the release binary: runtime identity
//! remains unverified (`runtime_identity_verified=false`).

use crate::{
    install::{InstallScope, InstallSpec, InstallTopology},
    model::{Scope, Topology},
    runtime_adapter::RuntimeConfig,
    systemd_transaction::SystemdExecution,
};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::{fmt::Debug, path::Path, path::PathBuf};

/// Schema version of [`Report`].
pub const CONFIGURATION_CHECK_SCHEMA_VERSION: u32 = 1;

/// Fixed name written by `SystemdTransactionBackend::persist_first_install_evidence`
/// directly in the transaction `state_dir`.
pub const FIRST_INSTALL_EVIDENCE_FILE: &str = "first-install-evidence.json";

/// Result of a successful cross-document check.
///
/// Serialized by the future `check-config` command. `declared_paths_verified`
/// is always `true` on success; the two `*_verified=false` fields make the
/// boundaries of this pure check explicit instead of implied.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub instance_id: String,
    pub topology: Topology,
    pub scope: String,
    pub state_dir: PathBuf,
    pub current_selector: PathBuf,
    pub release_store: PathBuf,
    pub core_unit: String,
    pub panel_unit: Option<String>,
    pub control_adapter: PathBuf,
    pub required_modules: Vec<String>,
    pub acknowledge_external_writers_stopped: bool,
    /// `true` only when `runtime.first_install` is present and matched the
    /// fixed evidence path and canonical units. `false` means upgrade-only.
    pub first_install_configured: bool,
    /// Declared path/identity contracts were validated by the three existing
    /// validators. This does not claim the paths exist unless the validators
    /// themselves require it.
    pub declared_paths_verified: bool,
    /// Always `false`: the real `deploy_executable` file hash is out of scope.
    pub deploy_executable_file_hash_verified: bool,
    /// Always `false`: configuration consistency is not runtime identity.
    pub runtime_identity_verified: bool,
    /// Human-readable equations/validations that were checked, in order.
    pub checks: Vec<String>,
    pub notes: Vec<String>,
}

/// Cross-check `install.json`, `systemd-execution.json` and
/// `runtime-adapter.json` for one instance.
///
/// On success every declared equality listed in the module documentation holds
/// and [`Report::declared_paths_verified`] is `true`. On error the message names
/// the drifting field pair and both values. No file is written and no service
/// is contacted.
pub fn check(
    install: &InstallSpec,
    systemd: &SystemdExecution,
    runtime: &RuntimeConfig,
) -> Result<Report> {
    install
        .validate()
        .context("install spec failed its own validation")?;
    let deployment = install.deployment.as_ref().context(
        "configuration cross-check requires install.deployment; \
         a legacy frozen release_root has no systemd/runtime declaration to compare, \
         and this strict production check refuses to guess",
    )?;
    systemd
        .validate()
        .context("systemd execution document failed its own validation")?;
    validate_runtime_config(runtime)?;

    let mut checks = vec![
        "install.validate()".to_owned(),
        "systemd.validate()".to_owned(),
        "RuntimeConfig::validate()".to_owned(),
    ];

    // Identity and shape.
    check_eq(
        &mut checks,
        "install.instance_id vs systemd.deployment.instance_id",
        &install.instance_id,
        &systemd.deployment.instance_id,
    )?;
    check_eq(
        &mut checks,
        "install.instance_id vs runtime.instance_id",
        &install.instance_id,
        &runtime.instance_id,
    )?;
    let topology = topology_of(install.topology);
    check_eq(
        &mut checks,
        "install.topology vs systemd.deployment.topology",
        &topology,
        &systemd.deployment.topology,
    )?;
    let install_scope = scope_name(install.scope).to_owned();
    let systemd_scope = model_scope_name(&systemd.deployment.scope).to_owned();
    check_eq(
        &mut checks,
        "install.scope vs systemd.deployment.scope",
        &install_scope,
        &systemd_scope,
    )?;

    // Transaction state and selector.
    check_eq(
        &mut checks,
        "install.state_dir vs systemd.state_dir",
        &install.state_dir,
        &systemd.state_dir,
    )?;
    check_eq(
        &mut checks,
        "install.release_root vs systemd.current_selector",
        &install.release_root,
        &systemd.current_selector,
    )?;
    check_eq(
        &mut checks,
        "install.release_root vs runtime.current_selector",
        &install.release_root,
        &runtime.current_selector,
    )?;

    // Candidate store.
    let deployment_store = PathBuf::from(&systemd.deployment.store);
    check_eq(
        &mut checks,
        "systemd.deployment.store vs install.deployment.release_store",
        &deployment_store,
        &deployment.release_store,
    )?;
    check_eq(
        &mut checks,
        "systemd.deployment.store vs runtime.release_store",
        &deployment_store,
        &runtime.release_store,
    )?;

    // Adapter-private paths.
    check_eq(
        &mut checks,
        "install.deployment.instance_state vs runtime.instance_state",
        &deployment.instance_state,
        &runtime.instance_state,
    )?;
    check_eq(
        &mut checks,
        "install.deployment.core_socket vs runtime.core_socket",
        &deployment.core_socket,
        &runtime.core_socket,
    )?;
    check_eq(
        &mut checks,
        "install.deployment.panel_gate_file vs runtime.panel_gate_file",
        &deployment.panel_gate_file,
        &runtime.panel_gate_file,
    )?;
    ensure!(
        !overlaps(&systemd.state_dir, &runtime.instance_state),
        "configuration drift: systemd.state_dir {:?} overlaps runtime.instance_state {:?}; \
         the transaction and the adapter would use the same state-lock name",
        systemd.state_dir,
        runtime.instance_state
    );
    checks.push("systemd.state_dir does not overlap runtime.instance_state".to_owned());

    // Unit names and the trusted launcher used as control adapter.
    let names = install.unit_names();
    check_eq(
        &mut checks,
        "systemd.core_unit vs install.unit_names().core",
        &systemd.core_unit,
        &names.core,
    )?;
    check_eq(
        &mut checks,
        "systemd.panel_unit vs install.unit_names().panel",
        &systemd.panel_unit,
        &names.panel,
    )?;
    let expected_control_adapter = install
        .control_adapter_path()
        .context("deployment mode must expose a core launcher path")?;
    check_eq(
        &mut checks,
        "systemd.control_adapter vs install launcher path",
        &systemd.control_adapter,
        &expected_control_adapter,
    )?;

    // Module contract and operator acknowledgement.
    check_eq(
        &mut checks,
        "install.deployment.required_modules vs systemd.deployment.required_modules",
        &deployment.required_modules,
        &systemd.deployment.required_modules,
    )?;
    check_eq(
        &mut checks,
        "install.deployment.required_modules vs runtime.required_modules",
        &deployment.required_modules,
        &runtime.required_modules,
    )?;
    check_eq(
        &mut checks,
        "install.deployment.acknowledge_external_writers_stopped vs runtime.acknowledge_external_writers_stopped",
        &deployment.acknowledge_external_writers_stopped,
        &runtime.acknowledge_external_writers_stopped,
    )?;

    // First install is opt-in. `None` is accepted and reported as upgrade-only;
    // when present, the fixed evidence path and canonical units must match.
    let first_install_configured = runtime.first_install.is_some();
    let mut notes = vec![
        "declared paths were validated by the install/systemd/runtime validators; \
         this does not prove a service is running or that the selected release content matches a manifest"
            .to_owned(),
        "deploy_executable was checked only as an absolute executable path; \
         its real file existence and SHA256 are outside this pure cross-check"
            .to_owned(),
        "configuration consistency is not runtime identity; \
         Ready/MaintenanceLeave must still hash /proc/<pid>/exe against the active manifest"
            .to_owned(),
    ];
    if let Some(first) = &runtime.first_install {
        let expected_evidence = install.state_dir.join(FIRST_INSTALL_EVIDENCE_FILE);
        check_eq(
            &mut checks,
            "runtime.first_install.evidence vs state_dir/first-install-evidence.json",
            &first.evidence,
            &expected_evidence,
        )?;
        check_eq(
            &mut checks,
            "runtime.first_install.core_unit vs systemd.core_unit",
            &first.core_unit,
            &systemd.core_unit,
        )?;
        check_eq(
            &mut checks,
            "runtime.first_install.panel_unit vs systemd.panel_unit",
            &first.panel_unit,
            &systemd.panel_unit,
        )?;
    } else {
        notes.push(
            "runtime.first_install is not configured: this deployment is upgrade-only and \
             cannot perform a first install; a missing Core socket is never an empty installation"
                .to_owned(),
        );
    }

    Ok(Report {
        schema_version: CONFIGURATION_CHECK_SCHEMA_VERSION,
        instance_id: install.instance_id.clone(),
        topology,
        scope: install_scope,
        state_dir: install.state_dir.clone(),
        current_selector: install.release_root.clone(),
        release_store: deployment.release_store.clone(),
        core_unit: systemd.core_unit.clone(),
        panel_unit: systemd.panel_unit.clone(),
        control_adapter: systemd.control_adapter.clone(),
        required_modules: deployment.required_modules.clone(),
        acknowledge_external_writers_stopped: deployment.acknowledge_external_writers_stopped,
        first_install_configured,
        declared_paths_verified: true,
        deploy_executable_file_hash_verified: false,
        runtime_identity_verified: false,
        checks,
        notes,
    })
}

/// Validate directly, without depending on an intentionally rejected command
/// or its error text. This is a public read-only validator.
fn validate_runtime_config(runtime: &RuntimeConfig) -> Result<()> {
    runtime
        .validate()
        .context("runtime config failed RuntimeConfig::validate")
}

fn check_eq<T: PartialEq + Debug>(
    checks: &mut Vec<String>,
    label: &'static str,
    left: &T,
    right: &T,
) -> Result<()> {
    ensure!(
        left == right,
        "configuration drift: {label} ({left:?} != {right:?})"
    );
    checks.push(label.to_owned());
    Ok(())
}

fn topology_of(topology: InstallTopology) -> Topology {
    match topology {
        InstallTopology::CoreOnly => Topology::CoreOnly,
        InstallTopology::Embedded => Topology::Embedded,
        InstallTopology::Standalone => Topology::Standalone,
    }
}

fn scope_name(scope: InstallScope) -> &'static str {
    scope.as_str()
}

fn model_scope_name(scope: &Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::System => "system",
    }
}

/// `true` when one path equals or contains the other. Mirrors the private
/// helper in `install.rs`; used here for the defensive transaction-state /
/// adapter-state disjointness assertion.
fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}
