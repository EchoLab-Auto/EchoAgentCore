//! Behavior tests for the first-install path of the runtime control adapter.
//!
//! A brand-new instance has no Core socket and no selector. These tests prove
//! that the adapter refuses to infer that state from socket absence alone and
//! only proceeds with explicit, fresh, backend-owned evidence that the durable
//! selector is absent and the canonical systemd units are inactive.
//!
//! Everything uses temporary directories and fake evidence files; no real
//! systemd unit, Core process or service is started.

use echo_deploy::runtime_adapter::{
    self, write_first_install_evidence, FirstInstallConfig, FirstInstallEvidence, FirstInstallUnit,
    RuntimeConfig,
};
use echo_deploy::systemd::{ControlOperation, ControlReply, ControlRequest, CONTROL_PROTOCOL};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const INSTANCE: &str = "test-instance";
const CORE_UNIT: &str = "echo-agent-test-instance-core.service";
const PANEL_UNIT: &str = "echo-agent-test-instance-panel.service";
const MAX_AGE_SECONDS: u64 = 300;

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn assert_error<T>(result: anyhow::Result<T>, expected: &str) {
    match result {
        Ok(_) => panic!("expected an error containing {expected:?}, but the call succeeded"),
        Err(error) => {
            let text = format!("{error:#}");
            assert!(text.contains(expected), "expected {expected:?}, got {text}");
        }
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    base: PathBuf,
    config: RuntimeConfig,
    target: String,
    target_manifest: String,
    evidence_path: PathBuf,
}

impl Fixture {
    fn new(with_panel: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let base = root.path().to_path_buf();
        for dir in ["state", "store", "run"] {
            let path = base.join(dir);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exe = fs::read(std::env::current_exe().unwrap()).unwrap();
        let _r0_manifest = make_release(&base, "r0", &exe);
        let target = "r1".to_owned();
        let target_manifest = make_release(&base, "r1", &exe);
        let evidence_path = base.join("state/first-install-evidence.json");
        let config = RuntimeConfig {
            instance_id: INSTANCE.into(),
            core_socket: base.join("run/core.sock"),
            panel_gate_file: None,
            instance_state: base.join("state"),
            current_selector: base.join("state/current"),
            release_store: base.join("store"),
            required_modules: vec![],
            acknowledge_external_writers_stopped: true,
            first_install: Some(FirstInstallConfig {
                evidence: evidence_path.clone(),
                core_unit: CORE_UNIT.into(),
                panel_unit: with_panel.then(|| PANEL_UNIT.to_owned()),
                max_evidence_age_seconds: MAX_AGE_SECONDS,
            }),
        };
        Self {
            _root: root,
            base,
            config,
            target,
            target_manifest,
            evidence_path,
        }
    }

    fn request(
        &self,
        operation: ControlOperation,
        release: Option<&str>,
        manifest: Option<&str>,
    ) -> ControlRequest {
        ControlRequest {
            protocol: CONTROL_PROTOCOL.into(),
            operation,
            instance: INSTANCE.into(),
            release: release.map(str::to_owned),
            manifest_sha256: manifest.map(str::to_owned),
        }
    }

    fn prepare(&self) -> anyhow::Result<ControlReply> {
        runtime_adapter::handle(
            &self.config,
            self.request(
                ControlOperation::PrepareUpgrade,
                Some(&self.target),
                Some(&self.target_manifest),
            ),
        )
    }

    fn enter_maintenance(&self) -> anyhow::Result<ControlReply> {
        runtime_adapter::handle(
            &self.config,
            self.request(ControlOperation::MaintenanceEnter, None, None),
        )
    }

    fn drain(&self) -> anyhow::Result<ControlReply> {
        runtime_adapter::handle(
            &self.config,
            self.request(ControlOperation::Drain, None, None),
        )
    }

    fn leave_maintenance(&self) -> anyhow::Result<ControlReply> {
        runtime_adapter::handle(
            &self.config,
            self.request(ControlOperation::MaintenanceLeave, None, None),
        )
    }

    fn observe(&self) -> anyhow::Result<ControlReply> {
        runtime_adapter::handle(
            &self.config,
            self.request(ControlOperation::Observe, None, None),
        )
    }

    fn evidence(&self, observed_at_unix_ms: u64) -> Value {
        self.evidence_with(
            INSTANCE,
            &self.target,
            &self.target_manifest,
            &[(CORE_UNIT, "inactive")],
            observed_at_unix_ms,
        )
    }

    fn evidence_with(
        &self,
        instance: &str,
        release: &str,
        manifest: &str,
        units: &[(&str, &str)],
        observed_at_unix_ms: u64,
    ) -> Value {
        json!({
            "schema_version": 1,
            "source": "systemd",
            "instance_id": instance,
            "target_release": release,
            "target_manifest_sha256": manifest,
            "selector_absent": true,
            "units": units
                .iter()
                .map(|(unit, state)| json!({"unit": unit, "active_state": state}))
                .collect::<Vec<_>>(),
            "observed_at_unix_ms": observed_at_unix_ms,
        })
    }

    fn write_evidence(&self, value: &Value) {
        write_private(&self.evidence_path, &serde_json::to_vec(value).unwrap());
    }

    fn write_valid_evidence(&self) {
        self.write_evidence(&self.evidence(now_ms()));
    }

    fn state_path(&self) -> PathBuf {
        self.config.instance_state.join(runtime_adapter::STATE_FILE)
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap()
    }

    fn selector(&self) -> PathBuf {
        self.config.current_selector.clone()
    }

    fn socket(&self) -> PathBuf {
        self.config.core_socket.clone()
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn make_release(base: &Path, release: &str, exe: &[u8]) -> String {
    let dir = base.join("store").join(release);
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("bin/core"), exe).unwrap();
    fs::set_permissions(dir.join("bin/core"), fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "release_id": release,
        "target_os": "linux",
        "target_arch": std::env::consts::ARCH,
        "management_protocol": "management-contract-1",
        "development": true,
        "sources": [{
            "id": "src",
            "revision": "rev",
            "snapshot_sha256": "a".repeat(64),
            "lock_sha256": "b".repeat(64)
        }],
        "artifacts": [{
            "id": "core",
            "kind": "core",
            "path": "bin/core",
            "sha256": sha(exe),
            "size": exe.len(),
            "executable": true,
            "source_id": "src"
        }]
    });
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(dir.join("release.json"), &bytes).unwrap();
    fs::set_permissions(dir.join("release.json"), fs::Permissions::from_mode(0o444)).unwrap();
    sha(&bytes)
}

#[test]
fn first_install_config_is_strict_and_canonical() {
    // Unknown nested fields are rejected by the config schema.
    let text = r#"{"instance_id":"test-instance","core_socket":"/tmp/s",
        "panel_gate_file":null,"instance_state":"/tmp/i","current_selector":"/tmp/c",
        "release_store":"/tmp/r","first_install":{"evidence":"/tmp/e",
        "core_unit":"echo-agent-test-instance-core.service","panel_unit":null,
        "max_evidence_age_seconds":300,"extra":1}}"#;
    let error = serde_json::from_str::<RuntimeConfig>(text).unwrap_err();
    assert!(error.to_string().contains("unknown field"), "{error}");

    // A unit name that does not match the instance is refused before any I/O.
    let fixture = Fixture::new(false);
    let mut config = fixture.config.clone();
    config.first_install.as_mut().unwrap().core_unit = "echo-agent-other-core.service".into();
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(ControlOperation::Observe, None, None),
        ),
        "first_install.core_unit",
    );

    // Evidence age must be bounded.
    let mut config = fixture.config.clone();
    config
        .first_install
        .as_mut()
        .unwrap()
        .max_evidence_age_seconds = 0;
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(ControlOperation::Observe, None, None),
        ),
        "max_evidence_age_seconds",
    );

    // The evidence path must never alias the adapter's own durable state.
    let mut config = fixture.config.clone();
    config.first_install.as_mut().unwrap().evidence = fixture.state_path();
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(ControlOperation::Observe, None, None),
        ),
        "must not be the adapter state file",
    );
}

#[test]
fn first_install_evidence_parent_must_be_private() {
    let fixture = Fixture::new(false);
    let public = fixture.base.join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = fixture.config.clone();
    config.first_install.as_mut().unwrap().evidence = public.join("evidence.json");
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(ControlOperation::Observe, None, None),
        ),
        "unsafe directory mode",
    );
}

#[test]
fn missing_socket_without_first_install_contract_is_refused() {
    let fixture = Fixture::new(false);
    let mut config = fixture.config.clone();
    config.first_install = None;
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(
                ControlOperation::PrepareUpgrade,
                Some(&fixture.target),
                Some(&fixture.target_manifest),
            ),
        ),
        "first-install evidence is not configured",
    );
    assert!(
        !fixture.state_path().exists(),
        "a refused first install must not write prepared state"
    );
}

#[test]
fn missing_socket_without_evidence_file_is_refused() {
    let fixture = Fixture::new(false);
    assert_error(fixture.prepare(), "first-install evidence");
    assert!(!fixture.state_path().exists());
}

#[test]
fn first_install_prepare_requires_explicit_writer_acknowledgement() {
    let fixture = Fixture::new(false);
    let mut config = fixture.config.clone();
    config.acknowledge_external_writers_stopped = false;
    fixture.write_valid_evidence();
    assert_error(
        runtime_adapter::handle(
            &config,
            fixture.request(
                ControlOperation::PrepareUpgrade,
                Some(&fixture.target),
                Some(&fixture.target_manifest),
            ),
        ),
        "acknowledge_external_writers_stopped",
    );
}

#[test]
fn first_install_prepare_accepts_fresh_backend_evidence() {
    let fixture = Fixture::new(false);
    let evidence = fixture.evidence(now_ms());
    let bytes = serde_json::to_vec(&evidence).unwrap();
    fixture.write_evidence(&evidence);

    let reply = fixture.prepare().unwrap();
    assert!(reply.success);
    assert_eq!(reply.operation, ControlOperation::PrepareUpgrade);

    let state = fixture.state();
    assert_eq!(state["prepared"]["previous"], Value::Null);
    assert_eq!(state["prepared"]["release"], "r1");
    assert_eq!(
        state["prepared"]["manifest_sha256"],
        fixture.target_manifest
    );
    assert_eq!(state["prepared"]["expected_generation"], 1);
    assert_eq!(
        state["prepared"]["first_install"]["evidence_sha256"],
        sha(&bytes)
    );
    assert_eq!(
        state["prepared"]["first_install"]["units"][0]["unit"],
        CORE_UNIT
    );
    assert_eq!(
        state["prepared"]["first_install"]["units"][0]["active_state"],
        "inactive"
    );
    assert!(!fixture.socket().exists(), "tests must not create a Core");

    let observed = fixture.observe().unwrap();
    assert_eq!(observed.current_release, None);
    assert_eq!(observed.maintenance, Some(false));
    assert_eq!(observed.ready, Some(false));
}

#[test]
fn first_install_prepare_rejects_selector_present_even_with_evidence() {
    let fixture = Fixture::new(false);
    symlink(fixture.base.join("store/r0"), fixture.selector()).unwrap();
    fixture.write_valid_evidence();
    // The socket is absent, so the normal Core path fails closed. The evidence
    // must not be used when a durable selector exists.
    assert_error(fixture.prepare(), "Core control socket");
    assert!(!fixture.state_path().exists());
}

#[test]
fn first_install_prepare_rejects_wrong_unit_set_and_states() {
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (vec![(CORE_UNIT, "active")], "not inactive"),
        (vec![(CORE_UNIT, "failed")], "not inactive"),
        (vec![], "unit list"),
        (
            vec![(CORE_UNIT, "inactive"), (CORE_UNIT, "inactive")],
            "duplicate",
        ),
        (
            vec![(CORE_UNIT, "inactive"), (PANEL_UNIT, "inactive")],
            "unit set",
        ),
        (
            vec![("echo-agent-other-core.service", "inactive")],
            "missing unit",
        ),
    ];
    for (units, expected) in cases {
        let fixture = Fixture::new(false);
        let evidence = fixture.evidence_with(
            INSTANCE,
            &fixture.target,
            &fixture.target_manifest,
            &units,
            now_ms(),
        );
        fixture.write_evidence(&evidence);
        assert_error(fixture.prepare(), expected);
        assert!(!fixture.state_path().exists());
    }
}

#[test]
fn first_install_with_panel_requires_panel_unit_inactive() {
    let fixture = Fixture::new(true);
    let core_only = fixture.evidence_with(
        INSTANCE,
        &fixture.target,
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive")],
        now_ms(),
    );
    fixture.write_evidence(&core_only);
    assert_error(fixture.prepare(), "unit set");

    let panel_active = fixture.evidence_with(
        INSTANCE,
        &fixture.target,
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive"), (PANEL_UNIT, "active")],
        now_ms(),
    );
    fixture.write_evidence(&panel_active);
    assert_error(fixture.prepare(), "not inactive");

    let both_inactive = fixture.evidence_with(
        INSTANCE,
        &fixture.target,
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive"), (PANEL_UNIT, "inactive")],
        now_ms(),
    );
    fixture.write_evidence(&both_inactive);
    fixture.prepare().unwrap();
    assert_eq!(
        fixture.state()["prepared"]["first_install"]["units"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn first_install_prepare_rejects_stale_future_and_mismatched_evidence() {
    let fixture = Fixture::new(false);
    let now = now_ms();

    let stale = fixture.evidence(now.saturating_sub((MAX_AGE_SECONDS + 1) * 1000));
    fixture.write_evidence(&stale);
    assert_error(fixture.prepare(), "stale");

    let future = fixture.evidence(now + 60_000);
    fixture.write_evidence(&future);
    assert_error(fixture.prepare(), "future");

    let wrong_release = fixture.evidence_with(
        INSTANCE,
        "other-release",
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive")],
        now,
    );
    fixture.write_evidence(&wrong_release);
    assert_error(fixture.prepare(), "release mismatch");

    let wrong_manifest = fixture.evidence_with(
        INSTANCE,
        &fixture.target,
        &"0".repeat(64),
        &[(CORE_UNIT, "inactive")],
        now,
    );
    fixture.write_evidence(&wrong_manifest);
    assert_error(fixture.prepare(), "manifest mismatch");

    let wrong_instance = fixture.evidence_with(
        "other-instance",
        &fixture.target,
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive")],
        now,
    );
    fixture.write_evidence(&wrong_instance);
    assert_error(fixture.prepare(), "instance mismatch");

    let mut selector_present = fixture.evidence(now);
    selector_present["selector_absent"] = json!(false);
    fixture.write_evidence(&selector_present);
    assert_error(fixture.prepare(), "selector_absent");

    let mut wrong_source = fixture.evidence(now);
    wrong_source["source"] = json!("manual");
    fixture.write_evidence(&wrong_source);
    assert_error(fixture.prepare(), "source is not systemd");

    let mut wrong_schema = fixture.evidence(now);
    wrong_schema["schema_version"] = json!(2);
    fixture.write_evidence(&wrong_schema);
    assert_error(fixture.prepare(), "schema");
}

#[test]
fn first_install_evidence_must_be_private_regular_single_link() {
    let fixture = Fixture::new(false);
    let evidence = fixture.evidence(now_ms());

    // Group/world-readable evidence is refused.
    fixture.write_evidence(&evidence);
    fs::set_permissions(&fixture.evidence_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_error(fixture.prepare(), "not private");

    // A symlink is refused even when its target is valid.
    fs::remove_file(&fixture.evidence_path).unwrap();
    let real = fixture.base.join("state/real-evidence.json");
    write_private(&real, &serde_json::to_vec(&evidence).unwrap());
    symlink(&real, &fixture.evidence_path).unwrap();
    assert_error(fixture.prepare(), "not a regular file");

    // A hard link means the file is not solely owned by this contract.
    fs::remove_file(&fixture.evidence_path).unwrap();
    fs::hard_link(&real, &fixture.evidence_path).unwrap();
    assert_error(fixture.prepare(), "one link");
}

#[test]
fn first_install_core_socket_must_be_a_socket() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fs::write(fixture.socket(), b"not a socket").unwrap();
    assert_error(fixture.prepare(), "not a socket");
}

#[test]
fn first_install_drain_requires_fresh_backend_evidence() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    assert_error(fixture.drain(), "maintenance mode");
    fixture.enter_maintenance().unwrap();

    let reply = fixture.drain().unwrap();
    assert_eq!(reply.active_work, Some(0));
    assert_eq!(reply.current_release, None);

    fs::remove_file(&fixture.evidence_path).unwrap();
    assert_error(fixture.drain(), "first-install evidence");

    let stale = fixture.evidence(now_ms().saturating_sub((MAX_AGE_SECONDS + 1) * 1000));
    fixture.write_evidence(&stale);
    assert_error(fixture.drain(), "stale");
}

#[test]
fn first_install_drain_refuses_selector_or_live_core() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();

    symlink(fixture.base.join("store/r1"), fixture.selector()).unwrap();
    assert_error(fixture.drain(), "absent current_selector");
    fs::remove_file(fixture.selector()).unwrap();

    let _listener = UnixListener::bind(fixture.socket()).unwrap();
    assert_error(fixture.drain(), "socket to be absent");
}

#[test]
fn first_install_drain_accepts_newer_evidence_but_not_regression() {
    let fixture = Fixture::new(false);
    let base = now_ms().saturating_sub(60_000);
    fixture.write_evidence(&fixture.evidence(base));
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();

    // A fresh backend rewrite (newer observation) is stronger evidence and is
    // accepted.
    fixture.write_evidence(&fixture.evidence(base + 10_000));
    fixture.drain().unwrap();

    // An older observation cannot replace the one the prepare was bound to.
    fixture.write_evidence(&fixture.evidence(base - 10_000));
    assert_error(fixture.drain(), "regressed");
}

#[test]
fn first_install_maintenance_leave_clears_state_without_selector() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();
    fixture.drain().unwrap();

    let reply = fixture.leave_maintenance().unwrap();
    assert_eq!(reply.maintenance, Some(false));
    assert_eq!(reply.current_release, None);
    let state = fixture.state();
    assert_eq!(state["maintenance"], false);
    assert_eq!(state["prepared"], Value::Null);

    let observed = fixture.observe().unwrap();
    assert_eq!(observed.maintenance, Some(false));
    assert_eq!(observed.current_release, None);
    assert_eq!(observed.ready, Some(false));

    // Rollback retries are idempotent once the durable state is released.
    let retry = fixture.leave_maintenance().unwrap();
    assert_eq!(retry.maintenance, Some(false));
    assert_eq!(retry.current_release, None);
}

#[test]
fn first_install_maintenance_leave_requires_evidence_and_stopped_core() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();
    fixture.drain().unwrap();

    fs::remove_file(&fixture.evidence_path).unwrap();
    assert_error(fixture.leave_maintenance(), "first-install evidence");
    assert_eq!(fixture.state()["maintenance"], true);

    fixture.write_valid_evidence();
    let _listener = UnixListener::bind(fixture.socket()).unwrap();
    assert_error(fixture.leave_maintenance(), "stopped Core");
}

#[test]
fn first_install_observe_without_core_is_not_ready() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    let reply = fixture.observe().unwrap();
    assert_eq!(reply.current_release, None);
    assert_eq!(reply.maintenance, Some(false));
    assert_eq!(reply.ready, Some(false));
    assert_eq!(reply.active_work, Some(0));
}

#[test]
fn prepared_state_requires_consistent_first_install_evidence() {
    let fixture = Fixture::new(false);
    let mut state = json!({
        "schema_version": 1,
        "instance_id": INSTANCE,
        "maintenance": false,
        "prepared": {
            "transaction_id": "tx-1",
            "expected_generation": 1,
            "release": "r1",
            "manifest_sha256": fixture.target_manifest.clone(),
            "previous": null
        }
    });
    write_private(&fixture.state_path(), &serde_json::to_vec(&state).unwrap());
    assert_error(
        fixture.observe(),
        "neither a previous release nor first-install evidence",
    );

    state["prepared"]["first_install"] = json!({
        "evidence_sha256": "0".repeat(64),
        "observed_at_unix_ms": now_ms(),
        "units": [{"unit": CORE_UNIT, "active_state": "inactive"}]
    });
    state["prepared"]["previous"] = json!("r0");
    write_private(&fixture.state_path(), &serde_json::to_vec(&state).unwrap());
    assert_error(
        fixture.observe(),
        "both a previous release and first-install evidence",
    );
}

#[test]
fn first_install_evidence_writer_round_trips_through_the_adapter() {
    let fixture = Fixture::new(false);
    let evidence = FirstInstallEvidence {
        schema_version: runtime_adapter::FIRST_INSTALL_EVIDENCE_SCHEMA_VERSION,
        source: runtime_adapter::FIRST_INSTALL_EVIDENCE_SOURCE.into(),
        instance_id: INSTANCE.into(),
        target_release: fixture.target.clone(),
        target_manifest_sha256: fixture.target_manifest.clone(),
        selector_absent: true,
        units: vec![FirstInstallUnit {
            unit: CORE_UNIT.into(),
            active_state: "inactive".into(),
        }],
        observed_at_unix_ms: now_ms(),
    };
    write_first_install_evidence(&fixture.evidence_path, &evidence).unwrap();
    let mode = fs::metadata(&fixture.evidence_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    fixture.prepare().unwrap();
}

#[test]
fn first_install_evidence_writer_refuses_non_quiescent_documents() {
    let fixture = Fixture::new(false);
    let mut evidence = FirstInstallEvidence {
        schema_version: runtime_adapter::FIRST_INSTALL_EVIDENCE_SCHEMA_VERSION,
        source: runtime_adapter::FIRST_INSTALL_EVIDENCE_SOURCE.into(),
        instance_id: INSTANCE.into(),
        target_release: fixture.target.clone(),
        target_manifest_sha256: fixture.target_manifest.clone(),
        selector_absent: true,
        units: vec![FirstInstallUnit {
            unit: CORE_UNIT.into(),
            active_state: "active".into(),
        }],
        observed_at_unix_ms: now_ms(),
    };
    assert_error(
        write_first_install_evidence(&fixture.evidence_path, &evidence),
        "inactive units",
    );

    evidence.units[0].active_state = "inactive".into();
    evidence.selector_absent = false;
    assert_error(
        write_first_install_evidence(&fixture.evidence_path, &evidence),
        "selector_absent=true",
    );
    assert!(!fixture.evidence_path.exists());
}

#[test]
fn first_install_drain_rejects_unit_set_changed_since_prepare() {
    let fixture = Fixture::new(false);
    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();

    // The prepared record names only the core unit. A later config/evidence
    // that silently expands the observed unit set must not be accepted.
    let mut expanded = fixture.config.clone();
    expanded.first_install.as_mut().unwrap().panel_unit = Some(PANEL_UNIT.into());
    let evidence = fixture.evidence_with(
        INSTANCE,
        &fixture.target,
        &fixture.target_manifest,
        &[(CORE_UNIT, "inactive"), (PANEL_UNIT, "inactive")],
        now_ms(),
    );
    fixture.write_evidence(&evidence);
    assert_error(
        runtime_adapter::handle(
            &expanded,
            fixture.request(ControlOperation::Drain, None, None),
        ),
        "units changed since prepare",
    );
}

#[test]
fn first_install_maintenance_release_updates_and_checks_panel_gate() {
    let mut fixture = Fixture::new(false);
    let gate_path = fixture.base.join("gate.json");
    let gate = |maintenance: bool| {
        json!({
            "schema_version": 1,
            "instance_id": INSTANCE,
            "maintenance": maintenance,
            "release_id": "r1",
            "management_protocol": "management-contract-1"
        })
    };
    write_private(&gate_path, &serde_json::to_vec(&gate(false)).unwrap());
    fixture.config.panel_gate_file = Some(gate_path.clone());

    fixture.write_valid_evidence();
    fixture.prepare().unwrap();
    fixture.enter_maintenance().unwrap();
    let entered: Value = serde_json::from_slice(&fs::read(&gate_path).unwrap()).unwrap();
    assert_eq!(entered["maintenance"], true);

    fixture.drain().unwrap();
    fixture.leave_maintenance().unwrap();
    let released: Value = serde_json::from_slice(&fs::read(&gate_path).unwrap()).unwrap();
    assert_eq!(released["maintenance"], false);
    assert_eq!(released["release_id"], "r1");

    // A gate left in maintenance without prepared state is not silently
    // cleared by an idempotent retry.
    write_private(&gate_path, &serde_json::to_vec(&gate(true)).unwrap());
    assert_error(
        fixture.leave_maintenance(),
        "panel gate still reports maintenance",
    );

    write_private(&gate_path, &serde_json::to_vec(&gate(false)).unwrap());
    fixture.leave_maintenance().unwrap();
}

#[test]
fn first_install_refuses_when_adapter_state_records_an_installed_release() {
    let fixture = Fixture::new(false);
    let state = json!({
        "schema_version": 1,
        "instance_id": INSTANCE,
        "maintenance": false,
        "installed": true
    });
    write_private(&fixture.state_path(), &serde_json::to_vec(&state).unwrap());
    fixture.write_valid_evidence();
    assert_error(fixture.prepare(), "previously installed release");
    assert_eq!(fixture.state()["installed"], true);
    assert_eq!(fixture.state()["prepared"], Value::Null);
}

#[test]
fn legacy_adapter_config_without_first_install_still_parses() {
    let text = r#"{"instance_id":"test-instance","core_socket":"/tmp/s",
        "panel_gate_file":null,"instance_state":"/tmp/i","current_selector":"/tmp/c",
        "release_store":"/tmp/r","required_modules":[],
        "acknowledge_external_writers_stopped":true}"#;
    let config: RuntimeConfig = serde_json::from_str(text).unwrap();
    assert!(config.first_install.is_none());
}

#[test]
fn installer_generated_first_install_config_shape_is_accepted() {
    // Mirrors install.rs::FirstInstallRuntimeConfig: no max age key, panel_unit
    // present as null for a topology without a panel.
    let text = r#"{
        "instance_id":"local-dev",
        "core_socket":"/home/u/state/run/core.sock",
        "panel_gate_file":null,
        "instance_state":"/home/u/state/runtime",
        "current_selector":"/home/u/state/deploy/current",
        "release_store":"/home/u/state/store",
        "required_modules":[],
        "acknowledge_external_writers_stopped":false,
        "first_install":{
            "evidence":"/home/u/state/deploy/first-install-evidence.json",
            "core_unit":"echo-agent-local-dev-core.service",
            "panel_unit":null
        }
    }"#;
    let config: RuntimeConfig = serde_json::from_str(text).unwrap();
    let first = config.first_install.unwrap();
    assert_eq!(
        first.evidence,
        PathBuf::from("/home/u/state/deploy/first-install-evidence.json")
    );
    assert_eq!(first.core_unit, "echo-agent-local-dev-core.service");
    assert_eq!(first.panel_unit, None);
    assert_eq!(
        first.max_evidence_age_seconds,
        runtime_adapter::DEFAULT_FIRST_INSTALL_MAX_AGE_SECONDS
    );
}
