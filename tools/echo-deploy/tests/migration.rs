//! Fake-transport coverage for the legacy takeover bootstrap. No real
//! systemctl, unit directory, service or Core root is touched.
#![allow(dead_code)]

#[rustfmt::skip]
#[path = "../src/migration.rs"]
mod migration;
#[path = "../src/model.rs"]
mod model;
#[path = "../src/safe_fs.rs"]
mod safe_fs;

use anyhow::{bail, Context, Result};
use migration::*;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::PathBuf,
    time::Duration,
};
use tempfile::TempDir;

const CORE_ROOT: &str = "/home/evence/.local/share/echo-agent-core/source";
const PANEL_ROOT: &str = "/home/evence/.local/share/echo-agent-panel/source";
const CORE_UNIT: &str = "echo-agent-core.service";
const PANEL_UNIT: &str = "echo-agent-panel.service";
const UPDATE_UNIT: &str = "echo-agent-core-update.service";
const NEW_CORE: &str = "echo-agent-local-dev-core.service";
const NEW_PANEL: &str = "echo-agent-local-dev-panel.service";
const NEW_UPDATE: &str = "echo-agent-local-dev-update.service";

#[derive(Clone)]
struct UnitState {
    fragment: PathBuf,
    active: bool,
    enabled: bool,
    static_unit: bool,
    main_pid: u32,
    start_timestamp: u64,
    working_directory: String,
    exec_start: String,
}

#[derive(Clone, Default)]
struct Fake {
    units: BTreeMap<String, UnitState>,
    calls: Vec<String>,
    fail_stop: Option<String>,
    reject_observation_after_terminal: bool,
}

impl Fake {
    fn set_active(&mut self, unit: &str, active: bool) {
        let state = self.units.get_mut(unit).unwrap();
        state.active = active;
        state.main_pid = if active { 9001 } else { 0 };
    }
    fn state(&self, unit: &str) -> &UnitState {
        self.units.get(unit).unwrap()
    }
}

impl MigrationTransport for Fake {
    fn verify_caller_safety(&mut self, _spec: &MigrationSpec) -> Result<()> {
        Ok(())
    }

    fn show(
        &mut self,
        spec: &MigrationSpec,
        unit: &str,
        property: UnitProperty,
        _timeout: Duration,
    ) -> Result<String> {
        if self.reject_observation_after_terminal {
            if let Ok(bytes) = fs::read(migration::journal_path(spec)) {
                let journal: serde_json::Value = serde_json::from_slice(&bytes)?;
                if journal["phase"] == "rolled_back" {
                    bail!("injected observation failure after terminal publication");
                }
            }
        }
        let state = self
            .units
            .get(unit)
            .with_context(|| format!("unknown unit {unit}"))?;
        Ok(match property {
            UnitProperty::FragmentPath => state.fragment.display().to_string(),
            UnitProperty::MainPID => if state.active { state.main_pid } else { 0 }.to_string(),
            UnitProperty::ActiveState => if state.active { "active" } else { "inactive" }.into(),
            UnitProperty::UnitFileState => {
                if state.static_unit {
                    "static".into()
                } else if state.enabled {
                    "enabled".into()
                } else {
                    "disabled".into()
                }
            }
            UnitProperty::WorkingDirectory => state.working_directory.clone(),
            UnitProperty::ExecStart => state.exec_start.clone(),
            UnitProperty::ExecMainStartTimestampMonotonic => state.start_timestamp.to_string(),
        })
    }
    fn stop_unit(&mut self, _spec: &MigrationSpec, unit: &str, _timeout: Duration) -> Result<()> {
        self.calls.push(format!("stop:{unit}"));
        if self.fail_stop.as_deref() == Some(unit) {
            bail!("injected stop failure for {unit}");
        }
        let state = self.units.get_mut(unit).context("stop unknown unit")?;
        state.active = false;
        state.main_pid = 0;
        Ok(())
    }
    fn start_unit(&mut self, _spec: &MigrationSpec, unit: &str, _timeout: Duration) -> Result<()> {
        self.calls.push(format!("start:{unit}"));
        self.set_active(unit, true);
        Ok(())
    }
    fn enable_unit(&mut self, _spec: &MigrationSpec, unit: &str, _timeout: Duration) -> Result<()> {
        self.calls.push(format!("enable:{unit}"));
        self.units
            .get_mut(unit)
            .context("enable unknown unit")?
            .enabled = true;
        Ok(())
    }
    fn disable_unit(
        &mut self,
        _spec: &MigrationSpec,
        unit: &str,
        _timeout: Duration,
    ) -> Result<()> {
        self.calls.push(format!("disable:{unit}"));
        self.units
            .get_mut(unit)
            .context("disable unknown unit")?
            .enabled = false;
        Ok(())
    }
    fn daemon_reload(&mut self, _spec: &MigrationSpec, _timeout: Duration) -> Result<()> {
        self.calls.push("daemon-reload".into());
        Ok(())
    }
}

struct Fixture {
    _tmp: TempDir,
    spec: MigrationSpec,
    fake: Fake,
}

fn write(path: &PathBuf, contents: &str, mode: u32) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, PermissionsExt::from_mode(mode)).unwrap();
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let units = root.join("units");
    let legacy = root.join("legacy");
    let libexec = root.join("libexec");
    for directory in [&state, &units, &legacy, &libexec] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(
            directory,
            PermissionsExt::from_mode(if directory == &state { 0o700 } else { 0o755 }),
        )
        .unwrap();
    }
    let core_bin = libexec.join("echo-agent-core-bin");
    let panel_bin = libexec.join("echo-agent-panel-bin");
    write(&core_bin, "#!/bin/sh\nexit 0\n", 0o755);
    write(&panel_bin, "#!/bin/sh\nexit 0\n", 0o755);
    let core_fragment = legacy.join(CORE_UNIT);
    let panel_fragment = legacy.join(PANEL_UNIT);
    let update_fragment = legacy.join(UPDATE_UNIT);
    write(
        &core_fragment,
        "[Service]\nExecStart=/declared/core\n[Install]\nWantedBy=default.target\n",
        0o644,
    );
    write(
        &panel_fragment,
        "[Service]\nExecStart=/declared/panel\n[Install]\nWantedBy=default.target\n",
        0o644,
    );
    write(
        &update_fragment,
        "[Service]\nType=oneshot\nExecStart=/declared/update\n",
        0o644,
    );
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        write(
            &units.join(unit),
            "[Service]\nExecStart=/new\n[Install]\nWantedBy=default.target\n",
            0o644,
        );
    }
    let spec = MigrationSpec {
        schema_version: MIGRATION_SCHEMA_VERSION,
        instance_id: "local-dev".into(),
        scope: Scope::User,
        state_dir: state,
        legacy_units: vec![
            LegacyUnit {
                unit: CORE_UNIT.into(),
                fragment: core_fragment.clone(),
                binary: Some(core_bin.clone()),
                working_directory: Some(CORE_ROOT.into()),
            },
            LegacyUnit {
                unit: PANEL_UNIT.into(),
                fragment: panel_fragment.clone(),
                binary: Some(panel_bin.clone()),
                working_directory: Some(PANEL_ROOT.into()),
            },
            LegacyUnit {
                unit: UPDATE_UNIT.into(),
                fragment: update_fragment.clone(),
                binary: None,
                working_directory: Some(CORE_ROOT.into()),
            },
        ],
        new_units: vec![NEW_CORE.into(), NEW_PANEL.into(), NEW_UPDATE.into()],
        unit_directory: units.clone(),
        guard_executable: "/trusted/echo-deploy".into(),
    };
    for unit in &spec.new_units {
        write(
            &units.join(unit),
            &format!(
                "[Service]\n{}\nExecStart=/new\n[Install]\nWantedBy=default.target\n",
                activation_guard_line(
                    &spec.guard_executable,
                    &spec.state_dir,
                    &spec.instance_id,
                    unit
                )
                .unwrap()
            ),
            0o644,
        );
    }
    let mut fake = Fake::default();
    for (unit, fragment, active, enabled, static_unit, working_directory, exec_start) in [
        (
            CORE_UNIT,
            core_fragment,
            true,
            true,
            false,
            CORE_ROOT,
            r#""/tmp/core-bin" --config /tmp/core.toml"#,
        ),
        (
            PANEL_UNIT,
            panel_fragment,
            true,
            true,
            false,
            PANEL_ROOT,
            r#""/tmp/panel-bin" --config /tmp/panel.toml"#,
        ),
        (
            UPDATE_UNIT,
            update_fragment,
            false,
            false,
            true,
            CORE_ROOT,
            r#""/tmp/update.sh" --local"#,
        ),
    ] {
        fake.units.insert(
            unit.into(),
            UnitState {
                fragment,
                active,
                enabled,
                static_unit,
                main_pid: if active { 4242 } else { 0 },
                start_timestamp: 0,
                working_directory: working_directory.into(),
                exec_start: exec_start.into(),
            },
        );
    }
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        fake.units.insert(
            unit.into(),
            UnitState {
                fragment: units.join(unit),
                active: false,
                enabled: false,
                static_unit: false,
                main_pid: 0,
                start_timestamp: 0,
                working_directory: String::new(),
                exec_start: String::new(),
            },
        );
    }
    Fixture {
        _tmp: tmp,
        spec,
        fake,
    }
}

fn capture_fixture(fixture: &mut Fixture) -> Evidence {
    capture(&fixture.spec, &mut fixture.fake).unwrap()
}

fn journal_value(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_slice(&fs::read(migration::journal_path(&fixture.spec)).unwrap()).unwrap()
}

#[test]
fn capture_is_read_only_exact_core_root_and_previews_activation_false() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    assert!(
        fixture.fake.calls.is_empty(),
        "capture must not mutate systemd"
    );
    let core = evidence.old(CORE_UNIT).unwrap();
    assert_eq!(core.working_directory, CORE_ROOT);
    assert_eq!(core.active_state, "active");
    assert_eq!(core.main_pid, 4242);
    assert_eq!(
        core.exec_start_assurance,
        ExecStartAssurance::OpaqueEqualityOnly
    );
    assert_eq!(core.fragment_sha256.len(), 64);
    assert_eq!(core.binary_sha256.as_deref().map(str::len), Some(64));
    let preview = evidence.preview();
    assert!(preview.takeover_ready);
    assert!(!preview.activation_ready, "capture is not activation");
    assert_eq!(preview.stop_units, vec![CORE_UNIT, PANEL_UNIT, UPDATE_UNIT]);
    assert_eq!(preview.disable_units, vec![CORE_UNIT, PANEL_UNIT]);
    assert_eq!(preview.enable_units, vec![NEW_CORE, NEW_PANEL, NEW_UPDATE]);
    assert_eq!(preview.start_on_recovery, vec![CORE_UNIT, PANEL_UNIT]);
    assert_eq!(
        fs::metadata(&preview.evidence_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(Evidence::load(&fixture.spec).unwrap() == evidence);
    assert_eq!(
        evidence.digest().unwrap(),
        Evidence::load(&fixture.spec).unwrap().digest().unwrap()
    );
}

#[test]
fn whole_fake_flow_never_starts_new_then_recover_restores_old() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    let report = takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    assert!(
        !report.ready && !report.activated,
        "takeover is not activation"
    );
    assert!(report.activation_required && report.takeover_complete);
    assert_eq!(report.phase, JournalPhase::PreparedForActivation);
    for unit in [CORE_UNIT, PANEL_UNIT, UPDATE_UNIT] {
        assert!(!fixture.fake.state(unit).active, "{unit} must be stopped");
    }
    for unit in [CORE_UNIT, PANEL_UNIT] {
        assert!(!fixture.fake.state(unit).enabled, "{unit} must be disabled");
    }
    assert!(
        fixture.fake.state(UPDATE_UNIT).static_unit,
        "static unit is not disabled"
    );
    assert!(!fixture
        .fake
        .calls
        .contains(&format!("disable:{UPDATE_UNIT}")));
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        assert!(fixture.fake.state(unit).enabled, "{unit} must be enabled");
        assert!(
            !fixture.fake.state(unit).active,
            "{unit} must not be started"
        );
        assert!(
            !fixture.fake.calls.contains(&format!("start:{unit}")),
            "takeover started {unit}"
        );
    }
    assert!(fixture.fake.calls.contains(&"daemon-reload".to_string()));

    let recovery = recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    assert!(recovery.recovered && recovery.old_runtime_restored);
    assert!(
        !recovery.ready
            && !recovery.activated
            && !recovery.data_modified
            && !recovery.data_rolled_back
    );
    assert_eq!(recovery.phase, JournalPhase::RolledBack);
    for unit in [CORE_UNIT, PANEL_UNIT] {
        assert!(
            fixture.fake.state(unit).active,
            "{unit} must be running again"
        );
        assert!(
            fixture.fake.state(unit).enabled,
            "{unit} must be enabled again"
        );
    }
    assert!(!fixture.fake.state(UPDATE_UNIT).active);
    assert!(
        !fixture
            .fake
            .calls
            .iter()
            .any(|call| call == &format!("start:{UPDATE_UNIT}")),
        "originally inactive update unit must not be started"
    );
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        assert!(
            !fixture.fake.state(unit).active,
            "{unit} must stay inactive"
        );
        assert!(
            !fixture.fake.state(unit).enabled,
            "{unit} must be disabled again"
        );
        assert!(
            !fixture
                .fake
                .calls
                .iter()
                .any(|call| call == &format!("start:{unit}")),
            "new unit {unit} was started"
        );
    }
    let first_new_stop = fixture
        .fake
        .calls
        .iter()
        .position(|call| call == &format!("stop:{NEW_CORE}"))
        .unwrap();
    let first_old_start = fixture
        .fake
        .calls
        .iter()
        .position(|call| call == &format!("start:{CORE_UNIT}"))
        .unwrap();
    assert!(
        first_new_stop < first_old_start,
        "recover must stop new units before starting old ones"
    );
}

#[test]
fn takeover_requires_confirm_and_rejects_pre_active_new_unit() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    assert!(takeover(&fixture.spec, &evidence, &mut fixture.fake, false).is_err());
    assert!(fixture.fake.calls.is_empty());
    fixture.fake.set_active(NEW_PANEL, true);
    assert!(takeover(&fixture.spec, &evidence, &mut fixture.fake, true).is_err());
    assert!(
        fixture.fake.calls.is_empty(),
        "drift must be rejected before mutation"
    );
    assert!(!migration::journal_path(&fixture.spec).exists());
}

#[test]
fn drift_is_rejected_before_stop_and_without_overwriting_evidence() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    let evidence_path = evidence.preview().evidence_path;
    let before = fs::read(&evidence_path).unwrap();
    write(&evidence_path, "do not overwrite\n", 0o600);
    assert!(
        capture(&fixture.spec, &mut fixture.fake).is_err(),
        "capture must use O_EXCL"
    );
    assert_eq!(fs::read(&evidence_path).unwrap(), b"do not overwrite\n");
    fs::write(&evidence_path, &before).unwrap();
    fs::set_permissions(&evidence_path, PermissionsExt::from_mode(0o600)).unwrap();

    let fragment = fixture.spec.legacy_units[0].fragment.clone();
    write(&fragment, "changed\n", 0o644);
    assert!(takeover(&fixture.spec, &evidence, &mut fixture.fake, true).is_err());
    assert!(
        fixture.fake.calls.is_empty(),
        "fragment drift must be rejected before any stop"
    );
    assert!(!migration::journal_path(&fixture.spec).exists());
}

#[test]
fn exec_start_drift_and_active_state_drift_are_rejected() {
    for change_exec in [true, false] {
        let mut fixture = fixture();
        let evidence = capture_fixture(&mut fixture);
        if change_exec {
            fixture
                .fake
                .units
                .get_mut(CORE_UNIT)
                .unwrap()
                .exec_start
                .push_str(" --changed");
        } else {
            fixture.fake.set_active(CORE_UNIT, false);
        }
        let error = takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap_err();
        assert!(
            format!("{error:#}").contains("drift") || format!("{error:#}").contains("ActiveState"),
            "{error:#}"
        );
        assert!(fixture.fake.calls.is_empty());
    }
}

#[test]
fn failed_stop_leaves_durable_failed_journal_and_recover_restores_old_running_state() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    fixture.fake.fail_stop = Some(PANEL_UNIT.into());
    let error = takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap_err();
    assert!(format!("{error:#}").contains("injected stop failure"));
    assert_eq!(journal_value(&fixture)["phase"], "failed");
    assert!(!fixture.fake.state(CORE_UNIT).active);
    assert!(
        fixture.fake.state(PANEL_UNIT).active,
        "failed stop leaves the panel running"
    );

    fixture.fake.fail_stop = None;
    let recovery = recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    assert!(recovery.recovered && recovery.old_runtime_restored);
    for unit in [CORE_UNIT, PANEL_UNIT] {
        assert!(fixture.fake.state(unit).active, "{unit} must be restored");
        assert!(
            fixture.fake.state(unit).enabled,
            "{unit} must be re-enabled"
        );
    }
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        assert!(
            !fixture.fake.state(unit).enabled,
            "{unit} must be disabled after rollback"
        );
        assert!(!fixture.fake.state(unit).active);
    }
}

#[test]
fn recover_requires_ack_and_refuses_after_new_unit_activation() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    let calls_before = fixture.fake.calls.clone();
    let error = recover(&fixture.spec, &evidence, &mut fixture.fake, false).unwrap_err();
    assert!(format!("{error:#}").contains("ack_reversible"));
    assert_eq!(
        fixture.fake.calls, calls_before,
        "missing ack must fail before mutation"
    );

    fixture.fake.set_active(NEW_CORE, true);
    let calls_before = fixture.fake.calls.clone();
    let error = recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap_err();
    assert!(format!("{error:#}").contains("post-activation"));
    assert_eq!(
        fixture.fake.calls, calls_before,
        "active-new refusal must be read-only"
    );
    assert!(!fixture.fake.state(CORE_UNIT).active);
    assert!(fixture.fake.state(NEW_CORE).active);
}

#[test]
fn new_unit_symlink_and_unknown_units_are_rejected() {
    let mut fixture = fixture();
    let new_path = fixture.spec.unit_directory.join(NEW_CORE);
    fs::remove_file(&new_path).unwrap();
    symlink(fixture.spec.legacy_units[0].fragment.clone(), &new_path).unwrap();
    let error = capture(&fixture.spec, &mut fixture.fake).unwrap_err();
    assert!(
        format!("{error:#}").contains("new unit file"),
        "symlink must be rejected: {error:#}"
    );
    assert!(!fixture
        .spec
        .state_dir
        .join("migration-evidence-local-dev.json")
        .exists());

    let mut spec = fixture.spec.clone();
    spec.new_units = vec!["../escape.service".into()];
    assert!(spec.validate().is_err());
    assert!(migration::systemctl_argv(
        &spec,
        migration::SystemctlVerb::Stop,
        Some("other.service")
    )
    .is_err());
}

#[test]
fn fixed_systemctl_argv_is_exact_and_shell_free() {
    let fixture = fixture();
    assert_eq!(
        ExecTransport::new().program(),
        std::path::Path::new("/usr/bin/systemctl")
    );
    let stop = migration::systemctl_argv(
        &fixture.spec,
        migration::SystemctlVerb::Stop,
        Some(CORE_UNIT),
    )
    .unwrap();
    assert_eq!(
        stop,
        vec![
            "--user",
            "--no-pager",
            "--no-ask-password",
            "--plain",
            "stop",
            "--no-block",
            CORE_UNIT
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<std::ffi::OsString>>()
    );
    let show = migration::systemctl_argv(
        &fixture.spec,
        migration::SystemctlVerb::Show(UnitProperty::FragmentPath),
        Some(CORE_UNIT),
    )
    .unwrap();
    assert_eq!(
        show,
        vec![
            "--user",
            "--no-pager",
            "--no-ask-password",
            "--plain",
            "show",
            "--property=FragmentPath",
            "--value",
            CORE_UNIT
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<std::ffi::OsString>>()
    );
    let mut system = fixture.spec.clone();
    system.scope = Scope::System;
    let reload =
        migration::systemctl_argv(&system, migration::SystemctlVerb::DaemonReload, None).unwrap();
    assert_eq!(
        reload,
        vec![
            "--no-pager",
            "--no-ask-password",
            "--plain",
            "daemon-reload"
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<std::ffi::OsString>>()
    );
    assert!(migration::systemctl_argv(
        &fixture.spec,
        migration::SystemctlVerb::DaemonReload,
        Some(CORE_UNIT)
    )
    .is_err());
}

#[test]
fn recover_without_journal_is_refused() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    assert!(recover(&fixture.spec, &evidence, &mut fixture.fake, true).is_err());
    assert!(fixture.fake.calls.is_empty());
}

#[test]
fn recover_restores_legacy_disabled_state_too() {
    let mut fixture = fixture();
    fixture.fake.units.get_mut(UPDATE_UNIT).unwrap().static_unit = false; // now "disabled"
    let evidence = capture_fixture(&mut fixture);
    assert_eq!(
        evidence.old(UPDATE_UNIT).unwrap().unit_file_state,
        "disabled"
    );
    takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    assert!(fixture
        .fake
        .calls
        .iter()
        .any(|call| call == &format!("disable:{UPDATE_UNIT}")));
    assert!(!fixture.fake.state(UPDATE_UNIT).enabled);
}

#[test]
fn capture_rejects_already_enabled_new_unit() {
    let mut fixture = fixture();
    fixture.fake.units.get_mut(NEW_CORE).unwrap().enabled = true;
    assert!(capture(&fixture.spec, &mut fixture.fake).is_err());
    assert!(!fixture
        .spec
        .state_dir
        .join("migration-evidence-local-dev.json")
        .exists());
}

#[test]
fn binary_larger_than_one_mib_is_supported() {
    let mut fixture = fixture();
    let binary = fixture.spec.legacy_units[0].binary.clone().unwrap();
    fs::write(&binary, vec![0x5a; 2 * 1024 * 1024]).unwrap();
    fs::set_permissions(&binary, PermissionsExt::from_mode(0o755)).unwrap();
    let evidence = capture_fixture(&mut fixture);
    assert_eq!(
        evidence
            .old(CORE_UNIT)
            .unwrap()
            .binary_sha256
            .as_deref()
            .map(str::len),
        Some(64)
    );
}

#[test]
fn captured_file_hash_is_standard_sha256_not_a_hash_of_the_digest() {
    use sha2::{Digest, Sha256};
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    for old in evidence.old_units() {
        let bytes = fs::read(&old.fragment_path).unwrap();
        assert_eq!(old.fragment_sha256, format!("{:x}", Sha256::digest(&bytes)));
    }
}

#[test]
fn migration_recovery_rejects_new_units_that_ran_and_stopped() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    fixture
        .fake
        .units
        .get_mut(NEW_CORE)
        .unwrap()
        .start_timestamp = 123;
    let calls = fixture.fake.calls.len();
    assert!(recover(&fixture.spec, &evidence, &mut fixture.fake, true).is_err());
    assert_eq!(fixture.fake.calls.len(), calls);
}

#[test]
fn execstart_runtime_results_may_change_but_definition_drift_is_rejected() {
    let mut fixture = fixture();
    fixture.fake.units.get_mut(CORE_UNIT).unwrap().exec_start =
        "{ path=/bin/sleep ; argv[]=/bin/sleep 600 ; ignore_errors=no ; start_time=[now] ; stop_time=[n/a] ; pid=42 ; code=(null) ; status=0 }".into();
    let evidence = capture_fixture(&mut fixture);
    takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    fixture.fake.units.get_mut(CORE_UNIT).unwrap().exec_start =
        "{ path=/bin/sleep ; argv[]=/bin/sleep 600 ; ignore_errors=no ; start_time=[now] ; stop_time=[later] ; pid=42 ; code=killed ; status=15 }".into();
    recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    fixture.fake.units.get_mut(CORE_UNIT).unwrap().exec_start =
        "{ path=/bin/sleep ; argv[]=/bin/sleep 601 ; ignore_errors=no ; start_time=[now] ; stop_time=[later] ; pid=42 ; code=killed ; status=15 }".into();
    assert!(recover(&fixture.spec, &evidence, &mut fixture.fake, true).is_err());
}

#[test]
fn new_fragment_path_must_match_the_hashed_file_before_capture_or_takeover() {
    let mut fixture = fixture();
    fixture.fake.units.get_mut(NEW_CORE).unwrap().fragment =
        fixture.spec.state_dir.join("shadow.service");
    assert!(capture(&fixture.spec, &mut fixture.fake).is_err());
    assert!(fixture.fake.calls.is_empty());
    let mut second = self::fixture();
    let evidence = capture_fixture(&mut second);
    second.fake.units.get_mut(NEW_CORE).unwrap().fragment =
        second.spec.state_dir.join("shadow.service");
    assert!(takeover(&second.spec, &evidence, &mut second.fake, true).is_err());
    assert!(second.fake.calls.is_empty());
}

#[test]
fn infrastructure_units_are_not_legacy_application_stop_targets() {
    let fixture = fixture();
    for unit in [
        "dbus.service",
        "systemd-logind.service",
        "user@1000.service",
        "systemd-journald.service",
    ] {
        let mut spec = fixture.spec.clone();
        spec.legacy_units[0].unit = unit.into();
        assert!(spec.validate().is_err(), "accepted protected unit {unit}");
    }
}

#[test]
fn durable_activation_latch_survives_erased_systemd_history_and_blocks_capture() {
    let mut fixture = fixture();
    let evidence = capture_fixture(&mut fixture);
    takeover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap();
    mark_activation(&fixture.spec.state_dir, &fixture.spec.instance_id, NEW_CORE).unwrap();
    mark_activation(
        &fixture.spec.state_dir,
        &fixture.spec.instance_id,
        NEW_PANEL,
    )
    .unwrap();
    fixture
        .fake
        .units
        .get_mut(NEW_CORE)
        .unwrap()
        .start_timestamp = 0;
    let calls = fixture.fake.calls.clone();
    let error = recover(&fixture.spec, &evidence, &mut fixture.fake, true).unwrap_err();
    assert!(format!("{error:#}").contains("durable activation marker"));
    assert_eq!(fixture.fake.calls, calls);
    assert!(capture(&fixture.spec, &mut fixture.fake).is_err());
}

#[test]
fn activation_guard_rejects_wrong_state_binding_and_missing_guard() {
    for missing in [true, false] {
        let mut f = fixture();
        let contents = if missing {
            "[Service]\nExecStart=/new\n".into()
        } else {
            format!(
                "[Service]\n{}\nExecStart=/new\n",
                activation_guard_line(
                    &f.spec.guard_executable,
                    &f.spec.state_dir.join("other"),
                    &f.spec.instance_id,
                    NEW_CORE
                )
                .unwrap()
            )
        };
        write(&f.spec.unit_directory.join(NEW_CORE), &contents, 0o644);
        assert!(capture(&f.spec, &mut f.fake).is_err());
        assert!(f.fake.calls.is_empty());
    }
}

#[test]
fn terminal_recovery_does_not_restart_administratively_stopped_legacy() {
    let mut f = fixture();
    let evidence = capture_fixture(&mut f);
    takeover(&f.spec, &evidence, &mut f.fake, true).unwrap();
    recover(&f.spec, &evidence, &mut f.fake, true).unwrap();
    f.fake.set_active(CORE_UNIT, false);
    let calls = f.fake.calls.clone();
    let journal_before = fs::read(migration::journal_path(&f.spec)).unwrap();
    for _ in 0..2 {
        assert!(recover(&f.spec, &evidence, &mut f.fake, true).is_err());
        assert!(takeover(&f.spec, &evidence, &mut f.fake, true).is_err());
        assert_eq!(calls, f.fake.calls);
        assert_eq!(
            journal_before,
            fs::read(migration::journal_path(&f.spec)).unwrap()
        );
        assert_eq!(journal_value(&f)["phase"], "rolled_back");
        assert!(!f.fake.state(CORE_UNIT).active);
    }
}

#[test]
fn dangling_activation_marker_and_transaction_fail_closed() {
    for marker in [true, false] {
        let mut f = fixture();
        let evidence = capture_fixture(&mut f);
        takeover(&f.spec, &evidence, &mut f.fake, true).unwrap();
        let name = if marker {
            "migration-activated-local-dev.json"
        } else {
            "transaction-local-dev.json"
        };
        symlink("/nonexistent/guard", f.spec.state_dir.join(name)).unwrap();
        let calls = f.fake.calls.clone();
        assert!(recover(&f.spec, &evidence, &mut f.fake, true).is_err());
        assert_eq!(calls, f.fake.calls);
    }
}

#[test]
fn concurrent_activation_guards_accept_partial_latch_without_overwriting_it() {
    let f = fixture();
    let state = f.spec.state_dir.clone();
    let mut jobs = Vec::new();
    for unit in [NEW_CORE, NEW_PANEL, NEW_UPDATE] {
        let state = state.clone();
        jobs.push(std::thread::spawn(move || {
            mark_activation(&state, "local-dev", unit)
        }));
    }
    for job in jobs {
        job.join().unwrap().unwrap();
    }
    let marker = state.join("migration-activated-local-dev.json");
    let contents = fs::read(&marker).unwrap();
    assert_eq!(
        fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
        0o600
    );
    mark_activation(&state, "local-dev", NEW_CORE).unwrap();
    assert_eq!(contents, fs::read(&marker).unwrap());
    fs::write(&marker, b"").unwrap(); // Crash between O_EXCL and writing JSON.
    mark_activation(&state, "local-dev", NEW_CORE).unwrap();
    assert!(fs::read(&marker).unwrap().is_empty());
}

#[test]
fn activation_guard_does_not_follow_marker_symlink() {
    let f = fixture();
    let outside = f.spec.state_dir.join("untouched");
    fs::write(&outside, b"sentinel").unwrap();
    symlink(
        &outside,
        f.spec.state_dir.join("migration-activated-local-dev.json"),
    )
    .unwrap();
    assert!(mark_activation(&f.spec.state_dir, "local-dev", NEW_CORE).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
}

#[test]
fn recovery_publishes_terminal_only_after_final_observation() {
    let mut f = fixture();
    let evidence = capture_fixture(&mut f);
    takeover(&f.spec, &evidence, &mut f.fake, true).unwrap();
    f.fake.reject_observation_after_terminal = true;
    recover(&f.spec, &evidence, &mut f.fake, true).unwrap();
    assert_eq!(journal_value(&f)["phase"], "rolled_back");
    assert!(f.fake.state(CORE_UNIT).active);
}
