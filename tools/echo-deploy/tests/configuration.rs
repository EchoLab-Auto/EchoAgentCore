//! Tests for cross-document configuration consistency.
//!
//! `src/configuration.rs` is included directly so the module can be exercised
//! before the library's `pub mod configuration;` wiring and the `check-config`
//! CLI land. The `crate::*` imports inside the module are backed by shims that
//! re-export the real library modules, so every type is the same type the
//! binary uses (no duplicate `model`/`runtime_adapter` definitions).
//!
//! Every fixture is a real temp directory because `RuntimeConfig::validate`
//! stats the private instance/store/evidence directories. No file is written by
//! the check itself, no service is started and no socket is connected.

mod install {
    pub use echo_deploy::install::*;
}
mod model {
    pub use echo_deploy::model::*;
}
mod runtime_adapter {
    pub use echo_deploy::runtime_adapter::*;
}
mod systemd_transaction {
    pub use echo_deploy::systemd_transaction::*;
}

#[path = "../src/configuration.rs"]
#[allow(dead_code)]
mod configuration;

use configuration::{check, Report, FIRST_INSTALL_EVIDENCE_FILE};
use echo_deploy::install::{
    DeploymentRuntime, InstallScope, InstallSpec, InstallTopology,
    DEPLOYMENT_RUNTIME_SCHEMA_VERSION,
};
use echo_deploy::model::{Deployment, Scope, Topology};
use echo_deploy::runtime_adapter::RuntimeConfig;
use echo_deploy::systemd_transaction::SystemdExecution;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const INSTANCE: &str = "local";

fn install_topology(topology: Topology) -> InstallTopology {
    match topology {
        Topology::CoreOnly => InstallTopology::CoreOnly,
        Topology::Embedded => InstallTopology::Embedded,
        Topology::Standalone => InstallTopology::Standalone,
    }
}

struct Fixture {
    _root: TempDir,
    base: PathBuf,
    deploy_executable: PathBuf,
    state: PathBuf,
    store: PathBuf,
    instance_state: PathBuf,
    launcher: PathBuf,
    install: InstallSpec,
    systemd: SystemdExecution,
    runtime: RuntimeConfig,
}

type Mutation = Box<dyn Fn(&mut Fixture)>;

impl Fixture {
    fn new(topology: Topology) -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().to_path_buf();
        let state = base.join("deploy");
        let store = base.join("store");
        let instance_state = base.join("runtime");
        let launcher = base.join("bin");
        let units = base.join("units");
        let core_root = base.join("core-source");
        let panel_root = base.join("panel-source");
        for dir in [
            &state,
            &store,
            &instance_state,
            &launcher,
            &units,
            &core_root,
            &panel_root,
        ] {
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let selector = state.join("current");
        let core_unit = format!("echo-agent-{INSTANCE}-core.service");
        let panel_unit = (topology == Topology::Standalone)
            .then(|| format!("echo-agent-{INSTANCE}-panel.service"));
        let core_socket = instance_state.join("core.sock");
        let runtime_config_path = instance_state.join("runtime-adapter.json");
        let panel_gate =
            (topology == Topology::Standalone).then(|| instance_state.join("panel-gate.json"));

        let deployment = Deployment {
            schema_version: 1,
            instance_id: INSTANCE.into(),
            topology,
            scope: Scope::User,
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            store: store.to_str().unwrap().to_owned(),
            required_modules: vec!["echo-agent.session".into()],
        };
        let control_adapter = launcher.join(format!("echo-agent-{INSTANCE}"));
        let systemd = SystemdExecution::new(
            deployment,
            state.clone(),
            units,
            selector.clone(),
            core_unit.clone(),
            panel_unit.clone(),
            control_adapter,
            30,
            true,
        )
        .unwrap();

        // Intentionally does not exist on disk: the pure check must not stat or
        // hash `deploy_executable`.
        let deploy_executable = base.join("libexec/echo-deploy");
        let install = InstallSpec {
            instance_id: INSTANCE.into(),
            scope: InstallScope::User,
            release_root: selector.clone(),
            core_config: base.join("core.toml"),
            panel_config: (topology == Topology::Standalone).then(|| base.join("panel.toml")),
            topology: install_topology(topology),
            deploy_executable: deploy_executable.clone(),
            state_dir: state.clone(),
            deployment: Some(DeploymentRuntime {
                schema_version: DEPLOYMENT_RUNTIME_SCHEMA_VERSION,
                core_root,
                panel_root: (topology == Topology::Standalone).then(|| panel_root.clone()),
                launcher_directory: launcher.clone(),
                release_store: store.clone(),
                instance_state: instance_state.clone(),
                core_socket: core_socket.clone(),
                runtime_config: runtime_config_path,
                panel_gate_file: panel_gate.clone(),
                required_modules: vec!["echo-agent.session".into()],
                acknowledge_external_writers_stopped: true,
            }),
        };

        let runtime_value = json!({
            "instance_id": INSTANCE,
            "core_socket": core_socket,
            "panel_gate_file": panel_gate,
            "instance_state": instance_state,
            "current_selector": selector,
            "release_store": store,
            "required_modules": ["echo-agent.session"],
            "acknowledge_external_writers_stopped": true,
            "first_install": {
                "evidence": state.join(FIRST_INSTALL_EVIDENCE_FILE),
                "core_unit": core_unit,
                "panel_unit": panel_unit,
            },
        });
        let runtime: RuntimeConfig = serde_json::from_value(runtime_value).unwrap();

        Self {
            _root: root,
            base,
            deploy_executable,
            state,
            store,
            instance_state,
            launcher,
            install,
            systemd,
            runtime,
        }
    }

    fn report_value(&self) -> Value {
        serde_json::to_value(check(&self.install, &self.systemd, &self.runtime).unwrap()).unwrap()
    }
}

fn expect_err(result: anyhow::Result<Report>) -> String {
    match result {
        Ok(report) => panic!("expected a cross-check error, got success: {report:?}"),
        Err(error) => format!("{error:#}"),
    }
}

fn assert_drift(fixture: &Fixture, needle: &str) {
    let error = expect_err(check(&fixture.install, &fixture.systemd, &fixture.runtime));
    assert!(
        error.contains(needle),
        "expected drift {needle:?}, got error: {error}"
    );
}

fn make_private_dir(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn core_only_configuration_is_consistent_and_reports_its_boundaries() {
    let fixture = Fixture::new(Topology::CoreOnly);
    assert!(
        !fixture.deploy_executable.exists(),
        "fixture deliberately leaves deploy_executable absent"
    );

    let report = check(&fixture.install, &fixture.systemd, &fixture.runtime).unwrap();

    assert_eq!(report.instance_id, INSTANCE);
    assert_eq!(report.topology, Topology::CoreOnly);
    assert_eq!(report.scope, "user");
    assert_eq!(report.state_dir, fixture.state);
    assert_eq!(report.current_selector, fixture.state.join("current"));
    assert_eq!(report.release_store, fixture.store);
    assert_eq!(
        report.core_unit,
        format!("echo-agent-{INSTANCE}-core.service")
    );
    assert_eq!(report.panel_unit, None);
    assert_eq!(
        report.control_adapter,
        fixture.launcher.join(format!("echo-agent-{INSTANCE}"))
    );
    assert_eq!(
        report.required_modules,
        vec!["echo-agent.session".to_owned()]
    );
    assert!(report.acknowledge_external_writers_stopped);
    assert!(report.first_install_configured);
    assert!(report.declared_paths_verified);
    assert!(!report.deploy_executable_file_hash_verified);
    assert!(!report.runtime_identity_verified);
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("deploy_executable") && note.contains("SHA256")));
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("runtime identity")));

    for expected in [
        "install.validate()",
        "systemd.validate()",
        "RuntimeConfig::validate()",
        "install.instance_id vs runtime.instance_id",
        "install.release_root vs systemd.current_selector",
        "install.release_root vs runtime.current_selector",
        "systemd.deployment.store vs runtime.release_store",
        "install.deployment.instance_state vs runtime.instance_state",
        "install.deployment.core_socket vs runtime.core_socket",
        "systemd.core_unit vs install.unit_names().core",
        "systemd.control_adapter vs install launcher path",
        "install.deployment.required_modules vs runtime.required_modules",
        "install.deployment.acknowledge_external_writers_stopped vs runtime.acknowledge_external_writers_stopped",
        "runtime.first_install.evidence vs state_dir/first-install-evidence.json",
        "runtime.first_install.core_unit vs systemd.core_unit",
    ] {
        assert!(
            report.checks.iter().any(|checked| checked.contains(expected)),
            "missing check {expected:?} in {:?}",
            report.checks
        );
    }

    let value = fixture.report_value();
    assert_eq!(value["declared_paths_verified"], json!(true));
    assert_eq!(value["deploy_executable_file_hash_verified"], json!(false));
    assert_eq!(value["runtime_identity_verified"], json!(false));
    assert_eq!(value["first_install_configured"], json!(true));

    // The validate probe must stop before StateLock::acquire.
    assert!(
        !fixture
            .instance_state
            .join("runtime-adapter-state.json")
            .exists(),
        "check must not create or lock adapter state"
    );
}

#[test]
fn standalone_configuration_checks_panel_gate_and_panel_units() {
    let fixture = Fixture::new(Topology::Standalone);
    let report = check(&fixture.install, &fixture.systemd, &fixture.runtime).unwrap();
    assert_eq!(report.topology, Topology::Standalone);
    assert_eq!(
        report.panel_unit.as_deref(),
        Some(format!("echo-agent-{INSTANCE}-panel.service").as_str())
    );
    assert!(report.first_install_configured);
    let gate = fixture
        .runtime
        .panel_gate_file
        .as_ref()
        .expect("standalone gate");
    assert_eq!(
        fixture.install.deployment.as_ref().unwrap().panel_gate_file,
        Some(gate.clone())
    );
    assert!(report
        .checks
        .iter()
        .any(|checked| checked.contains("panel_gate_file")));
    assert!(report
        .checks
        .iter()
        .any(|checked| checked.contains("runtime.first_install.panel_unit")));
}

#[test]
fn legacy_install_without_deployment_is_rejected_not_guessed() {
    let mut fixture = Fixture::new(Topology::CoreOnly);
    fixture.install.deployment = None;
    let error = expect_err(check(&fixture.install, &fixture.systemd, &fixture.runtime));
    assert!(error.contains("install.deployment"), "{error}");
    assert!(error.contains("refuses to guess"), "{error}");
}

#[test]
fn first_install_opt_out_is_allowed_and_reported_as_upgrade_only() {
    let mut fixture = Fixture::new(Topology::CoreOnly);
    fixture.runtime.first_install = None;

    let report = check(&fixture.install, &fixture.systemd, &fixture.runtime).unwrap();
    assert!(!report.first_install_configured);
    assert!(
        !report
            .checks
            .iter()
            .any(|checked| checked.contains("first_install")),
        "no first-install equation can be verified without the opt-in block: {:?}",
        report.checks
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("upgrade-only")
                && note.contains("never an empty installation")),
        "notes must say this is upgrade-only: {:?}",
        report.notes
    );
}

#[test]
fn every_cross_document_field_drift_is_rejected() {
    let cases: Vec<(&'static str, Mutation)> = vec![
        (
            "install.instance_id vs systemd.deployment.instance_id",
            Box::new(|fixture| {
                fixture.systemd.deployment.instance_id = "other".into();
                fixture.systemd.core_unit = "echo-agent-other-core.service".into();
            }),
        ),
        (
            "install.instance_id vs runtime.instance_id",
            Box::new(|fixture| {
                fixture.runtime.instance_id = "other".into();
                fixture
                    .runtime
                    .first_install
                    .as_mut()
                    .unwrap()
                    .core_unit = "echo-agent-other-core.service".into();
            }),
        ),
        (
            "install.topology vs systemd.deployment.topology",
            Box::new(|fixture| {
                fixture.install.topology = InstallTopology::Embedded;
            }),
        ),
        (
            "install.scope vs systemd.deployment.scope",
            Box::new(|fixture| {
                fixture.systemd.deployment.scope = Scope::System;
            }),
        ),
        (
            "install.state_dir vs systemd.state_dir",
            Box::new(|fixture| {
                let alt = fixture.base.join("deploy-alt");
                fixture.install.state_dir = alt.clone();
                fixture.install.release_root = alt.join("current");
            }),
        ),
        (
            "install.release_root vs systemd.current_selector",
            Box::new(|fixture| {
                fixture.systemd.current_selector = fixture.state.join("next");
            }),
        ),
        (
            "install.release_root vs runtime.current_selector",
            Box::new(|fixture| {
                fixture.runtime.current_selector = fixture.state.join("next");
            }),
        ),
        (
            "systemd.deployment.store vs install.deployment.release_store",
            Box::new(|fixture| {
                fixture.systemd.deployment.store = fixture
                    .base
                    .join("store2")
                    .to_str()
                    .unwrap()
                    .to_owned();
            }),
        ),
        (
            "systemd.deployment.store vs runtime.release_store",
            Box::new(|fixture| {
                let store2 = fixture.base.join("store2");
                make_private_dir(&store2);
                fixture.runtime.release_store = store2;
            }),
        ),
        (
            "install.deployment.instance_state vs runtime.instance_state",
            Box::new(|fixture| {
                let runtime2 = fixture.base.join("runtime2");
                make_private_dir(&runtime2);
                fixture.runtime.instance_state = runtime2;
            }),
        ),
        (
            "install.deployment.core_socket vs runtime.core_socket",
            Box::new(|fixture| {
                fixture.runtime.core_socket = fixture.instance_state.join("other.sock");
            }),
        ),
        (
            "install.deployment.panel_gate_file vs runtime.panel_gate_file",
            Box::new(|fixture| {
                fixture.runtime.panel_gate_file =
                    Some(fixture.instance_state.join("other-gate.json"));
            }),
        ),
        (
            "systemd.control_adapter vs install launcher path",
            Box::new(|fixture| {
                fixture.systemd.control_adapter = fixture.launcher.join("other-launcher");
            }),
        ),
        (
            "install.deployment.required_modules vs systemd.deployment.required_modules",
            Box::new(|fixture| {
                fixture.systemd.deployment.required_modules = vec!["other.module".into()];
            }),
        ),
        (
            "install.deployment.required_modules vs systemd.deployment.required_modules",
            Box::new(|fixture| {
                fixture
                    .install
                    .deployment
                    .as_mut()
                    .unwrap()
                    .required_modules = vec!["other.module".into()];
            }),
        ),
        (
            "install.deployment.required_modules vs runtime.required_modules",
            Box::new(|fixture| {
                fixture.runtime.required_modules = vec!["other.module".into()];
            }),
        ),
        (
            "install.deployment.acknowledge_external_writers_stopped vs runtime.acknowledge_external_writers_stopped",
            Box::new(|fixture| {
                fixture.runtime.acknowledge_external_writers_stopped = false;
            }),
        ),
        (
            "install.deployment.acknowledge_external_writers_stopped vs runtime.acknowledge_external_writers_stopped",
            Box::new(|fixture| {
                fixture
                    .install
                    .deployment
                    .as_mut()
                    .unwrap()
                    .acknowledge_external_writers_stopped = false;
            }),
        ),
        (
            "runtime.first_install.evidence vs state_dir/first-install-evidence.json",
            Box::new(|fixture| {
                fixture.runtime.first_install.as_mut().unwrap().evidence = fixture
                    .instance_state
                    .join("first-install-evidence.json");
            }),
        ),
        (
            "runtime.first_install.panel_unit vs systemd.panel_unit",
            Box::new(|fixture| {
                fixture.runtime.first_install.as_mut().unwrap().panel_unit =
                    Some(format!("echo-agent-{INSTANCE}-panel.service"));
            }),
        ),
    ];

    for (needle, mutate) in cases {
        let mut fixture = Fixture::new(Topology::CoreOnly);
        mutate(&mut fixture);
        assert_drift(&fixture, needle);
    }
}

#[test]
fn standalone_panel_gate_and_first_install_panel_unit_drift_are_rejected() {
    let mut fixture = Fixture::new(Topology::Standalone);
    fixture.runtime.panel_gate_file = None;
    assert_drift(
        &fixture,
        "install.deployment.panel_gate_file vs runtime.panel_gate_file",
    );

    let mut fixture = Fixture::new(Topology::Standalone);
    fixture.runtime.first_install.as_mut().unwrap().panel_unit = None;
    assert_drift(
        &fixture,
        "runtime.first_install.panel_unit vs systemd.panel_unit",
    );
}

#[test]
fn existing_runtime_config_validation_runs_before_cross_check() {
    // Non-private adapter state directory: only RuntimeConfig::validate can
    // reject this, and the probe must surface its real error.
    let fixture = Fixture::new(Topology::CoreOnly);
    fs::set_permissions(&fixture.instance_state, fs::Permissions::from_mode(0o755)).unwrap();
    let error = expect_err(check(&fixture.install, &fixture.systemd, &fixture.runtime));
    assert!(
        error.contains("runtime config failed RuntimeConfig::validate"),
        "{error}"
    );
    assert!(
        error.contains("not private") || error.contains("unsafe directory mode"),
        "{error}"
    );
}

#[test]
fn first_install_unit_validation_is_not_reimplemented() {
    let mut fixture = Fixture::new(Topology::CoreOnly);
    fixture.runtime.first_install.as_mut().unwrap().core_unit =
        "echo-agent-other-core.service".into();
    let error = expect_err(check(&fixture.install, &fixture.systemd, &fixture.runtime));
    assert!(error.contains("RuntimeConfig::validate"), "{error}");
    assert!(error.contains("first_install.core_unit"), "{error}");
}

#[test]
fn check_does_not_write_files_or_take_the_adapter_lock() {
    let fixture = Fixture::new(Topology::Standalone);
    let before = directory_entries(&fixture.base);
    check(&fixture.install, &fixture.systemd, &fixture.runtime).unwrap();
    let after = directory_entries(&fixture.base);
    assert_eq!(before, after, "pure check must not create or remove files");
    assert!(!fixture
        .instance_state
        .join("runtime-adapter-state.json")
        .exists());
}

fn directory_entries(root: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            entries.push(path);
        }
    }
    entries.sort();
    entries
}

#[test]
fn check_config_cli_routes_strict_documents_and_rejects_drift_without_side_effects() {
    let fixture = Fixture::new(Topology::Standalone);
    let install = fixture.base.join("install.json");
    let execution = fixture.base.join("execution.json");
    let runtime = fixture.base.join("runtime.json");
    fs::write(&install, serde_json::to_vec(&fixture.install).unwrap()).unwrap();
    fs::write(&execution, serde_json::to_vec(&fixture.systemd).unwrap()).unwrap();
    let runtime_value =
        serde_json::to_value(fixture.install.runtime_adapter_config().unwrap().unwrap()).unwrap();
    fs::write(&runtime, serde_json::to_vec(&runtime_value).unwrap()).unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_echo-deploy"))
            .arg("check-config")
            .arg("--install")
            .arg(&install)
            .arg("--systemd")
            .arg(&execution)
            .arg("--runtime")
            .arg(&runtime)
            .output()
            .unwrap()
    };
    let before = directory_entries(&fixture.base);
    let valid = run();
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let report: Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(report["declared_paths_verified"], true);
    assert_eq!(report["runtime_identity_verified"], false);
    assert_eq!(directory_entries(&fixture.base), before);
    let mut changed = runtime_value;
    changed["required_modules"] = json!([]);
    fs::write(&runtime, serde_json::to_vec(&changed).unwrap()).unwrap();
    let rejected = run();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("required_modules"));
    assert_eq!(directory_entries(&fixture.base), before);
}
