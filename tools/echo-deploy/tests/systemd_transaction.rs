//! Behavior tests for the systemd transaction backend.
//!
//! Everything below uses an in-memory [`Transport`] and fixture files. No real
//! systemctl, service, control adapter, or network is executed.

#[path = "../src/model.rs"]
#[allow(dead_code)]
mod model;
#[path = "../src/safe_fs.rs"]
#[allow(dead_code)]
mod safe_fs;
#[path = "../src/systemd.rs"]
#[allow(dead_code)]
mod systemd;
#[path = "../src/systemd_transaction.rs"]
#[allow(dead_code)]
mod systemd_transaction;
#[path = "../src/transaction.rs"]
#[allow(dead_code)]
mod transaction;

mod runtime_adapter {
    pub use echo_deploy::runtime_adapter::*;
}

use anyhow::{bail, Result};
use model::{Deployment, Topology};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use systemd::{
    ActiveState, ControlOperation, ControlReply, ControlRequest, SystemdSpec, Transport,
    CONTROL_PROTOCOL,
};
use systemd_transaction::{PreviousReleaseEvidence, SystemdExecution, SystemdTransactionBackend};
use tempfile::{tempdir, TempDir};
use transaction::{
    Backend, BeginSpec, Drain, Phase, RunOutcome, Transaction, TransactionStore,
    TRANSACTION_SCHEMA_VERSION,
};

const INSTANCE: &str = "local";

struct Fixture {
    _root: TempDir,
    base: PathBuf,
    deployment: Deployment,
    execution: SystemdExecution,
    store: PathBuf,
    state: PathBuf,
    selector: PathBuf,
    adapter: PathBuf,
    core: String,
    panel: Option<String>,
}

impl Fixture {
    fn new(topology: Topology) -> Self {
        let root = tempdir().unwrap();
        let base = root.path().to_path_buf();
        let store = base.join("store");
        let state = base.join("state");
        let units = base.join("units");
        fs::create_dir(&store).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(&units).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = base.join("adapter");
        fs::write(&adapter, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();

        let core = format!("echo-agent-{INSTANCE}-core.service");
        let panel = (topology == Topology::Standalone)
            .then(|| format!("echo-agent-{INSTANCE}-panel.service"));
        let deployment = Deployment {
            schema_version: 1,
            instance_id: INSTANCE.into(),
            topology,
            scope: model::Scope::User,
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            store: store.to_str().unwrap().to_owned(),
            required_modules: vec![],
        };
        let selector = state.join("current");
        let execution = SystemdExecution::new(
            deployment.clone(),
            state.clone(),
            units,
            selector.clone(),
            core.clone(),
            panel.clone(),
            adapter.clone(),
            1,
            true,
        )
        .unwrap();
        Self {
            _root: root,
            base,
            deployment,
            execution,
            store,
            state,
            selector,
            adapter,
            core,
            panel,
        }
    }

    fn release_manifest(&self, release_id: &str, revision: &str) -> Vec<u8> {
        let mut artifacts = vec![json!({
            "id": "core-bin",
            "kind": "core",
            "path": "bin/echo-agent-core",
            "sha256": hex_sha256(b"c"),
            "size": 1,
            "executable": true,
            "source_id": "core-source",
        })];
        if self.panel.is_some() {
            artifacts.push(json!({
                "id": "panel-bin",
                "kind": "panel",
                "path": "bin/echo-agent-panel",
                "sha256": hex_sha256(b"d"),
                "size": 1,
                "executable": true,
                "source_id": "panel-source",
            }));
            artifacts.push(json!({
                "id": "panel-web",
                "kind": "web",
                "path": "web/index.html",
                "sha256": hex_sha256(b"e"),
                "size": 1,
                "executable": false,
                "source_id": "panel-source",
            }));
        }
        let mut sources = vec![json!({
            "id": "core-source",
            "revision": revision,
            "snapshot_sha256": "a".repeat(64),
            "lock_sha256": "b".repeat(64),
        })];
        if self.panel.is_some() {
            sources.push(json!({
                "id": "panel-source",
                "revision": revision,
                "snapshot_sha256": "f".repeat(64),
                "lock_sha256": "0".repeat(64),
            }));
        }
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "release_id": release_id,
            "target_os": "linux",
            "target_arch": std::env::consts::ARCH,
            "management_protocol": "management-1",
            "development": true,
            "sources": sources,
            "artifacts": artifacts,
        }))
        .unwrap()
    }

    fn write_release(&self, release_id: &str) -> String {
        self.write_release_with_revision(release_id, "test")
    }

    fn write_release_with_revision(&self, release_id: &str, revision: &str) -> String {
        let directory = self.store.join(release_id);
        fs::create_dir(&directory).unwrap();
        fs::create_dir(directory.join("bin")).unwrap();
        fs::write(directory.join("bin/echo-agent-core"), b"c").unwrap();
        fs::set_permissions(
            directory.join("bin/echo-agent-core"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        if self.deployment.topology == Topology::Standalone {
            fs::write(directory.join("bin/echo-agent-panel"), b"d").unwrap();
            fs::set_permissions(
                directory.join("bin/echo-agent-panel"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        if self.deployment.topology != Topology::CoreOnly {
            fs::create_dir(directory.join("web")).unwrap();
            fs::write(directory.join("web/index.html"), b"e").unwrap();
        }
        let bytes = self.release_manifest(release_id, revision);
        fs::write(directory.join("release.json"), &bytes).unwrap();
        hex_sha256(&bytes)
    }

    fn rewrite_release(&self, release_id: &str, revision: &str) -> String {
        let bytes = self.release_manifest(release_id, revision);
        fs::write(self.store.join(release_id).join("release.json"), &bytes).unwrap();
        hex_sha256(&bytes)
    }

    fn set_selector(&self, release_id: Option<&str>) {
        match fs::remove_file(&self.selector) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove selector: {error}"),
        }
        if let Some(release_id) = release_id {
            symlink(self.store.join(release_id), &self.selector).unwrap();
        }
    }

    fn selector_release(&self) -> Option<String> {
        match fs::read_link(&self.selector) {
            Ok(target) => target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read selector: {error}"),
        }
    }

    fn evidence_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = fs::read_dir(&self.state)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("previous-") && name.ends_with(".json"))
            })
            .collect();
        paths.sort();
        paths
    }

    fn read_evidence(&self) -> PreviousReleaseEvidence {
        let paths = self.evidence_files();
        assert_eq!(paths.len(), 1, "expected exactly one evidence sidecar");
        serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap()
    }

    fn mock(&self, active: bool) -> Mock {
        Mock::new(
            self.core.clone(),
            self.panel.clone(),
            self.selector.clone(),
            active,
        )
    }

    fn backend(&self, mock: Mock) -> (SystemdTransactionBackend<SharedMock>, Rc<RefCell<Mock>>) {
        let shared = Rc::new(RefCell::new(mock));
        let backend =
            SystemdTransactionBackend::new(self.execution.clone(), SharedMock(shared.clone()))
                .unwrap();
        (backend, shared)
    }

    fn transaction(
        &self,
        id: &str,
        previous: Option<&str>,
        target: &str,
        target_sha: &str,
    ) -> Transaction {
        Transaction {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            id: id.into(),
            instance_id: INSTANCE.into(),
            previous_release: previous.map(str::to_owned),
            target_release: target.into(),
            manifest_sha256: target_sha.into(),
            phase: Phase::Prepared,
            error: None,
            steps: vec![],
        }
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Stop {
        unit: String,
        selector: Option<String>,
    },
    Start {
        unit: String,
        selector: Option<String>,
    },
    Control(ControlOperation),
}

struct Mock {
    core: String,
    selector: PathBuf,
    states: BTreeMap<String, ActiveState>,
    maintenance: bool,
    ready: bool,
    active_work: u32,
    reported_release_override: Option<Option<String>>,
    reject_control: Option<ControlOperation>,
    fail_control: Option<ControlOperation>,
    protocol_mismatch: bool,
    ready_ack: bool,
    ready_state_on_ack: bool,
    fail_ready_release: Option<String>,
    enter_maintenance_state: bool,
    enter_maintenance_ack: Option<bool>,
    leave_maintenance_state: bool,
    leave_maintenance_ack: Option<bool>,
    drain_effective: bool,
    drain_count: Option<u32>,
    fail_start: Option<String>,
    fail_stop: Option<String>,
    events: Vec<Event>,
    requests: Vec<ControlRequest>,
}

impl Mock {
    fn new(core: String, panel: Option<String>, selector: PathBuf, active: bool) -> Self {
        let mut states = BTreeMap::new();
        states.insert(
            core.clone(),
            if active {
                ActiveState::Active
            } else {
                ActiveState::Inactive
            },
        );
        if let Some(panel) = &panel {
            states.insert(
                panel.clone(),
                if active {
                    ActiveState::Active
                } else {
                    ActiveState::Inactive
                },
            );
        }
        Self {
            core,
            selector,
            states,
            maintenance: false,
            ready: active,
            active_work: 0,
            reported_release_override: None,
            reject_control: None,
            fail_control: None,
            protocol_mismatch: false,
            ready_ack: true,
            ready_state_on_ack: true,
            fail_ready_release: None,
            enter_maintenance_state: true,
            enter_maintenance_ack: Some(true),
            leave_maintenance_state: false,
            leave_maintenance_ack: Some(false),
            drain_effective: true,
            drain_count: None,
            fail_start: None,
            fail_stop: None,
            events: Vec::new(),
            requests: Vec::new(),
        }
    }

    fn selected_release(&self) -> Result<Option<String>> {
        match fs::read_link(&self.selector) {
            Ok(target) => Ok(target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn reported_release(&self) -> Result<Option<String>> {
        match &self.reported_release_override {
            Some(value) => Ok(value.clone()),
            None => self.selected_release(),
        }
    }

    fn start_unit(&mut self, unit: &str) -> Result<()> {
        let selector = self.selected_release()?;
        self.events.push(Event::Start {
            unit: unit.to_owned(),
            selector,
        });
        if self.fail_start.as_deref() == Some(unit) {
            bail!("mock start failure for {unit}");
        }
        self.states.insert(unit.to_owned(), ActiveState::Active);
        if unit == self.core {
            self.ready = false;
        }
        Ok(())
    }

    fn stop_unit(&mut self, unit: &str) -> Result<()> {
        let selector = self.selected_release()?;
        self.events.push(Event::Stop {
            unit: unit.to_owned(),
            selector,
        });
        if self.fail_stop.as_deref() == Some(unit) {
            bail!("mock stop failure for {unit}");
        }
        self.states.insert(unit.to_owned(), ActiveState::Inactive);
        if unit == self.core {
            self.ready = false;
        }
        Ok(())
    }

    fn active_state(&self, unit: &str) -> ActiveState {
        self.states
            .get(unit)
            .copied()
            .unwrap_or(ActiveState::Inactive)
    }

    fn control(&mut self, request: &ControlRequest) -> Result<ControlReply> {
        self.events.push(Event::Control(request.operation));
        self.requests.push(request.clone());
        if self.fail_control == Some(request.operation) {
            bail!("mock control transport failure for {:?}", request.operation);
        }
        let mut reply = match request.operation {
            ControlOperation::Observe => ControlReply::observed(
                INSTANCE,
                self.reported_release()?,
                self.maintenance,
                self.ready,
                self.active_work,
            ),
            ControlOperation::PrepareUpgrade => ControlReply::ack(request.operation, INSTANCE),
            ControlOperation::MaintenanceEnter => {
                self.maintenance = self.enter_maintenance_state;
                let mut reply = ControlReply::ack(request.operation, INSTANCE);
                reply.maintenance = self.enter_maintenance_ack;
                reply
            }
            ControlOperation::MaintenanceLeave => {
                self.maintenance = self.leave_maintenance_state;
                let mut reply = ControlReply::ack(request.operation, INSTANCE);
                reply.maintenance = self.leave_maintenance_ack;
                reply
            }
            ControlOperation::Drain => {
                if self.drain_effective {
                    self.active_work = 0;
                }
                let mut reply = ControlReply::ack(request.operation, INSTANCE);
                reply.active_work = Some(self.drain_count.unwrap_or(self.active_work));
                reply
            }
            ControlOperation::Ready => {
                let release_ok = self.fail_ready_release.as_deref() != request.release.as_deref();
                let acknowledged = self.ready_ack && release_ok;
                if acknowledged && self.ready_state_on_ack {
                    self.ready = true;
                }
                let mut reply = ControlReply::ack(request.operation, INSTANCE);
                reply.ready = Some(acknowledged);
                reply.current_release = request.release.clone();
                reply
            }
        };
        if self.reject_control == Some(request.operation) {
            reply.success = false;
            reply.error = Some("mock rejection".into());
        }
        if self.protocol_mismatch {
            reply.protocol = "echo-deploy-control/2".into();
        }
        Ok(reply)
    }
}

#[derive(Clone)]
struct SharedMock(Rc<RefCell<Mock>>);

impl Transport for SharedMock {
    fn verify_process_image(
        &mut self,
        spec: &SystemdSpec,
        unit: &str,
        expected: &str,
        _deadline: Duration,
    ) -> Result<()> {
        assert_eq!(spec.panel_unit(), Some(unit));
        assert_eq!(expected, hex_sha256(b"d"));
        Ok(())
    }

    fn start_unit(&mut self, _spec: &SystemdSpec, unit: &str, _deadline: Duration) -> Result<()> {
        self.0.borrow_mut().start_unit(unit)
    }
    fn stop_unit(&mut self, _spec: &SystemdSpec, unit: &str, _deadline: Duration) -> Result<()> {
        self.0.borrow_mut().stop_unit(unit)
    }
    fn active_state(
        &mut self,
        _spec: &SystemdSpec,
        unit: &str,
        _deadline: Duration,
    ) -> Result<ActiveState> {
        Ok(self.0.borrow().active_state(unit))
    }
    fn control(
        &mut self,
        _spec: &SystemdSpec,
        request: &ControlRequest,
        _deadline: Duration,
    ) -> Result<ControlReply> {
        self.0.borrow_mut().control(request)
    }
}

fn relevant_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| !matches!(event, Event::Control(ControlOperation::Observe)))
        .cloned()
        .collect()
}

#[test]
fn execution_dto_is_strict_and_validates_topology_and_unit_identity() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let value = serde_json::to_value(&fixture.execution).unwrap();
    let object = value.as_object().unwrap().clone();

    for field in [
        "deployment",
        "state_dir",
        "unit_directory",
        "current_selector",
        "core_unit",
        "panel_unit",
        "control_adapter",
        "timeout_seconds",
        "reversible_data",
    ] {
        let mut document = object.clone();
        document.remove(field);
        assert!(
            serde_json::from_value::<SystemdExecution>(Value::Object(document)).is_err(),
            "missing field {field} must be rejected"
        );
    }
    let mut document = object.clone();
    document.insert("extra".into(), json!(true));
    assert!(
        serde_json::from_value::<SystemdExecution>(Value::Object(document)).is_err(),
        "unknown field must be rejected"
    );
    let parsed: SystemdExecution = serde_json::from_value(value).unwrap();
    assert!(parsed.panel_unit.is_none());

    let make = |core: String,
                panel: Option<String>,
                topology: Topology,
                timeout: u64,
                reversible: bool| {
        let mut deployment = fixture.deployment.clone();
        deployment.topology = topology;
        SystemdExecution::new(
            deployment,
            fixture.state.clone(),
            fixture.base.join("units"),
            fixture.selector.clone(),
            core,
            panel,
            fixture.adapter.clone(),
            timeout,
            reversible,
        )
    };

    assert!(make(
        fixture.core.clone(),
        Some("echo-agent-local-panel.service".into()),
        Topology::CoreOnly,
        1,
        true
    )
    .is_err());
    assert!(make(fixture.core.clone(), None, Topology::Standalone, 1, true).is_err());
    assert!(make(
        "echo-agent-other-core.service".into(),
        None,
        Topology::CoreOnly,
        1,
        true
    )
    .is_err());
    assert!(make(fixture.core.clone(), None, Topology::CoreOnly, 0, true).is_err());
    assert!(make(fixture.core.clone(), None, Topology::CoreOnly, 601, true).is_err());
    assert!(make(fixture.core.clone(), None, Topology::CoreOnly, 1, false).is_err());
    make(
        fixture.core.clone(),
        Some(format!("echo-agent-{INSTANCE}-panel.service")),
        Topology::Standalone,
        1,
        true,
    )
    .unwrap();
}

#[test]
fn create_backend_constructs_spec_and_exec_transport_without_execution() {
    let fixture = Fixture::new(Topology::Standalone);
    let backend = fixture.execution.create_backend().unwrap();
    assert_eq!(backend.spec().instance(), INSTANCE);
    assert_eq!(backend.spec().scope(), systemd::Scope::User);
    assert_eq!(backend.spec().core_unit(), fixture.core);
    assert_eq!(backend.spec().panel_unit(), fixture.panel.as_deref());
    assert_eq!(backend.spec().state_directory(), fixture.state);
    assert_eq!(backend.spec().current_selector(), fixture.selector);
    assert_eq!(backend.spec().control_adapter(), fixture.adapter);
    assert_eq!(backend.spec().deadlines().systemctl, Duration::from_secs(1));
    assert_eq!(
        backend.transport().program(),
        Path::new(systemd::SYSTEMCTL_PROGRAM)
    );
}

#[test]
fn observe_rejects_current_selector_drift_and_tracks_verified_current_hash() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let old_sha = fixture.write_release("old");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));

    let observation = backend.observe(INSTANCE).unwrap();
    assert_eq!(observation.current_release.as_deref(), Some("old"));
    assert_eq!(
        backend.current_identity().unwrap().manifest_sha256(),
        old_sha
    );

    mock.borrow_mut().reported_release_override = Some(Some("other".into()));
    let error = backend.observe(INSTANCE).unwrap_err().to_string();
    assert!(error.contains("drift"), "{error}");
    mock.borrow_mut().reported_release_override = None;

    fixture.rewrite_release("old", "tampered");
    let error = backend.observe(INSTANCE).unwrap_err().to_string();
    assert!(error.contains("manifest changed"), "{error}");
}

#[test]
fn prepare_verifies_target_and_requires_real_control_preflight() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-prepare", Some("old"), "new", &new_sha);

    let bad_hash = fixture.transaction("tx-bad", Some("old"), "new", &"0".repeat(64));
    assert!(backend.prepare(&bad_hash).is_err());
    assert!(
        mock.borrow()
            .requests
            .iter()
            .all(|request| request.operation != ControlOperation::PrepareUpgrade),
        "invalid target must not reach the control adapter"
    );

    mock.borrow_mut().protocol_mismatch = true;
    let error = backend.prepare(&transaction).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("protocol mismatch"), "{error}");
    mock.borrow_mut().protocol_mismatch = false;

    mock.borrow_mut().reject_control = Some(ControlOperation::PrepareUpgrade);
    assert!(backend.prepare(&transaction).is_err());
    mock.borrow_mut().reject_control = None;

    backend.prepare(&transaction).unwrap();
    assert_eq!(
        backend.target_identity().unwrap().manifest_sha256(),
        new_sha
    );
    assert!(mock.borrow().requests.iter().any(|request| {
        request.operation == ControlOperation::PrepareUpgrade
            && request.protocol == CONTROL_PROTOCOL
            && request.release.as_deref() == Some("new")
            && request.manifest_sha256.as_deref() == Some(new_sha.as_str())
    }));
    let evidence = fixture.read_evidence();
    assert_eq!(evidence.transaction_id, "tx-prepare");
    assert_eq!(evidence.previous_release, "old");
    assert_eq!(evidence.target_release, "new");
    assert_eq!(evidence.target_manifest_sha256, new_sha);
}

#[test]
fn previous_release_evidence_is_immutable_and_never_rewritten() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, _mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-immutable", Some("old"), "new", &new_sha);
    backend.prepare(&transaction).unwrap();
    let path = fixture.evidence_files().pop().unwrap();
    let original = fs::read(&path).unwrap();
    let mut evidence: PreviousReleaseEvidence = serde_json::from_slice(&original).unwrap();
    evidence.target_manifest_sha256 = "d".repeat(64);
    let tampered = serde_json::to_vec(&evidence).unwrap();
    fs::write(&path, &tampered).unwrap();

    let error = backend.prepare(&transaction).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("immutable evidence"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), tampered);
    assert_ne!(fs::read(&path).unwrap(), original);
}

#[test]
fn maintenance_and_drain_acknowledgements_are_cross_checked() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-ack", Some("old"), "new", &new_sha);
    backend.prepare(&transaction).unwrap();

    mock.borrow_mut().enter_maintenance_state = false;
    assert!(backend.enter_maintenance(&transaction).is_err());
    mock.borrow_mut().enter_maintenance_state = true;
    backend.enter_maintenance(&transaction).unwrap();

    mock.borrow_mut().active_work = 2;
    mock.borrow_mut().drain_effective = false;
    mock.borrow_mut().drain_count = Some(0);
    assert!(backend.drain(&transaction).is_err());

    mock.borrow_mut().drain_count = None;
    assert_eq!(
        backend.drain(&transaction).unwrap(),
        Drain::Busy { remaining: 2 }
    );

    mock.borrow_mut().drain_effective = true;
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
}

#[test]
fn switch_requires_durable_old_evidence_and_stops_units_before_pointer() {
    // Missing evidence: a direct switch without prepare must refuse before any
    // unit or pointer side effect.
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    mock.borrow_mut().maintenance = true;
    let transaction = fixture.transaction("tx-switch", Some("old"), "new", &new_sha);
    let before = mock.borrow().events.clone();
    let error = backend.switch(&transaction).unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("begin-time old-manifest evidence"),
        "{error}"
    );
    let after = mock.borrow().events.clone();
    assert_eq!(
        after
            .iter()
            .filter(|event| matches!(event, Event::Stop { .. } | Event::Start { .. }))
            .count(),
        before
            .iter()
            .filter(|event| matches!(event, Event::Stop { .. } | Event::Start { .. }))
            .count(),
        "refusal must happen before any unit side effect"
    );
    assert_eq!(fixture.selector_release().as_deref(), Some("old"));

    // Successful path: prepare writes the sidecar, then stop Panel/Core while
    // the selector still names old, and only then move the pointer.
    let fixture = Fixture::new(Topology::Standalone);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-switch-2", Some("old"), "new", &new_sha);
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();

    let events = mock.borrow().events.clone();
    let stops: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Stop { unit, selector } => Some((unit.clone(), selector.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops,
        vec![
            (fixture.panel.clone().unwrap(), Some("old".to_owned())),
            (fixture.core.clone(), Some("old".to_owned())),
        ]
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Start { .. })),
        "switch must not start any unit"
    );
    assert_eq!(fixture.selector_release().as_deref(), Some("new"));
    assert_eq!(backend.current_identity().unwrap().release_id(), "new");
}

#[test]
fn full_transaction_commits_with_stop_then_switch_then_start_then_ready() {
    let fixture = Fixture::new(Topology::Standalone);
    let old_sha = fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    let store = TransactionStore::open(&fixture.state).unwrap();
    let mut transaction = store
        .begin(
            BeginSpec::new("tx-commit", INSTANCE, "new", &new_sha),
            &mut backend,
        )
        .unwrap();
    assert_eq!(transaction.previous_release.as_deref(), Some("old"));

    let outcome = store.execute(&mut transaction, &mut backend).unwrap();
    assert_eq!(outcome, RunOutcome::Committed);
    assert_eq!(transaction.phase, Phase::Committed);
    assert_eq!(fixture.selector_release().as_deref(), Some("new"));
    assert!(!mock.borrow().maintenance);
    assert!(mock.borrow().ready);
    assert_eq!(mock.borrow().active_work, 0);
    for unit in [&fixture.core, fixture.panel.as_ref().unwrap()] {
        assert_eq!(mock.borrow().active_state(unit), ActiveState::Active);
    }

    let events = relevant_events(&mock.borrow().events);
    let expected = vec![
        Event::Control(ControlOperation::PrepareUpgrade),
        Event::Control(ControlOperation::MaintenanceEnter),
        Event::Control(ControlOperation::Drain),
        Event::Stop {
            unit: fixture.panel.clone().unwrap(),
            selector: Some("old".into()),
        },
        Event::Stop {
            unit: fixture.core.clone(),
            selector: Some("old".into()),
        },
        Event::Start {
            unit: fixture.core.clone(),
            selector: Some("new".into()),
        },
        Event::Start {
            unit: fixture.panel.clone().unwrap(),
            selector: Some("new".into()),
        },
        Event::Control(ControlOperation::Ready),
        Event::Control(ControlOperation::Ready),
        Event::Control(ControlOperation::MaintenanceLeave),
    ];
    assert_eq!(events, expected);

    let evidence = fixture.read_evidence();
    assert_eq!(evidence.previous_release, "old");
    assert_eq!(evidence.previous_manifest_sha256, old_sha);
    assert_eq!(evidence.target_manifest_sha256, new_sha);
}

#[test]
fn existing_release_rollback_restores_old_hash_and_stops_before_pointer_back() {
    let fixture = Fixture::new(Topology::Standalone);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    mock.borrow_mut().fail_ready_release = Some("new".into());
    let store = TransactionStore::open(&fixture.state).unwrap();
    let mut transaction = store
        .begin(
            BeginSpec::new("tx-rollback", INSTANCE, "new", &new_sha),
            &mut backend,
        )
        .unwrap();

    let outcome = store.execute(&mut transaction, &mut backend).unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(transaction.phase, Phase::RolledBack);
    assert_eq!(
        transaction.error.as_ref().unwrap().code,
        transaction::ErrorCode::ResumeFailed
    );
    assert_eq!(fixture.selector_release().as_deref(), Some("old"));
    assert!(!mock.borrow().maintenance);
    assert!(mock.borrow().ready);

    let events = mock.borrow().events.clone();
    let stops: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Stop { unit, selector } => Some((unit.clone(), selector.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops,
        vec![
            (fixture.panel.clone().unwrap(), Some("old".to_owned())),
            (fixture.core.clone(), Some("old".to_owned())),
            (fixture.panel.clone().unwrap(), Some("new".to_owned())),
            (fixture.core.clone(), Some("new".to_owned())),
        ],
        "restore must stop the failed target before moving the pointer back"
    );
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Start { unit, selector } => Some((unit.clone(), selector.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            (fixture.core.clone(), Some("new".to_owned())),
            (fixture.panel.clone().unwrap(), Some("new".to_owned())),
            (fixture.core.clone(), Some("old".to_owned())),
            (fixture.panel.clone().unwrap(), Some("old".to_owned())),
        ]
    );
    assert!(fixture.evidence_files().len() == 1);
}

#[test]
fn crash_recovery_uses_persisted_sidecar_and_never_current_disk_value() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let old_sha = fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, _mock) = fixture.backend(fixture.mock(true));
    let store = TransactionStore::open(&fixture.state).unwrap();
    let mut transaction = store
        .begin(
            BeginSpec::new("tx-crash", INSTANCE, "new", &new_sha),
            &mut backend,
        )
        .unwrap();
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();
    assert_eq!(fixture.selector_release().as_deref(), Some("new"));
    transaction.phase = Phase::Switching;
    store.save(&transaction).unwrap();

    // A new process: caches are empty, units are stopped, and the adapter must
    // answer Observe from durable state.
    let mut restart = fixture.mock(false);
    restart.maintenance = true;
    let (mut restarted_backend, restarted_mock) = fixture.backend(restart);
    let outcome = store
        .recover(INSTANCE, &mut restarted_backend)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert_eq!(fixture.selector_release().as_deref(), Some("old"));
    assert!(!restarted_mock.borrow().maintenance);
    assert!(restarted_mock.borrow().ready);
    assert_eq!(
        restarted_backend
            .current_identity()
            .unwrap()
            .manifest_sha256(),
        old_sha
    );
    let evidence = fixture.read_evidence();
    assert_eq!(evidence.previous_manifest_sha256, old_sha);

    // If the old directory was rewritten after the switch, recovery must not
    // treat the new on-disk bytes as the old release hash.
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, _mock) = fixture.backend(fixture.mock(true));
    let store = TransactionStore::open(&fixture.state).unwrap();
    let mut transaction = store
        .begin(
            BeginSpec::new("tx-tampered", INSTANCE, "new", &new_sha),
            &mut backend,
        )
        .unwrap();
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();
    transaction.phase = Phase::Switching;
    store.save(&transaction).unwrap();
    fixture.rewrite_release("old", "tampered-after-switch");

    let mut restart = fixture.mock(false);
    restart.maintenance = true;
    let (mut restarted_backend, restarted_mock) = fixture.backend(restart);
    let outcome = store
        .recover(INSTANCE, &mut restarted_backend)
        .unwrap()
        .unwrap();
    assert_eq!(outcome, RunOutcome::RecoveryRequired);
    assert_eq!(transaction::ErrorCode::RollbackRestoreFailed, {
        store.load(INSTANCE).unwrap().unwrap().error.unwrap().code
    });
    assert_eq!(
        fixture.selector_release().as_deref(),
        Some("new"),
        "tampered old evidence must fail closed before any restore side effect"
    );
    assert!(!restarted_mock
        .borrow()
        .events
        .iter()
        .any(|event| matches!(event, Event::Stop { .. })));
}

#[test]
fn first_install_rollback_stops_units_and_removes_only_owned_selector() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let new_sha = fixture.write_release("new");
    let keep = fixture.state.join("keep");
    fs::write(&keep, b"unrelated").unwrap();
    let (mut backend, mock) = fixture.backend(fixture.mock(false));
    mock.borrow_mut().fail_ready_release = Some("new".into());
    let store = TransactionStore::open(&fixture.state).unwrap();
    let mut transaction = store
        .begin(
            BeginSpec::new("tx-first", INSTANCE, "new", &new_sha),
            &mut backend,
        )
        .unwrap();
    assert!(transaction.previous_release.is_none());

    let outcome = store.execute(&mut transaction, &mut backend).unwrap();
    assert_eq!(outcome, RunOutcome::RolledBack);
    assert!(fs::symlink_metadata(&fixture.selector).is_err());
    assert_eq!(
        mock.borrow().active_state(&fixture.core),
        ActiveState::Inactive
    );
    assert!(!mock.borrow().maintenance);
    assert!(!mock.borrow().ready);
    assert_eq!(fs::read(&keep).unwrap(), b"unrelated");
    assert!(fixture.evidence_files().is_empty());

    // A non-symlink or a foreign symlink at the selector path is not ours to
    // delete; restore must fail rather than remove an arbitrary path.
    let fixture = Fixture::new(Topology::CoreOnly);
    let new_sha = fixture.write_release("new");
    let (mut backend, _mock) = fixture.backend(fixture.mock(false));
    let transaction = fixture.transaction("tx-first-direct", None, "new", &new_sha);
    backend.observe(INSTANCE).unwrap();
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();
    fs::remove_file(&fixture.selector).unwrap();
    symlink(fixture.store.join("foreign"), &fixture.selector).unwrap();
    assert!(backend.restore(&transaction).is_err());
    assert_eq!(
        fs::read_link(&fixture.selector).unwrap(),
        fixture.store.join("foreign")
    );

    fs::remove_file(&fixture.selector).unwrap();
    fs::write(&fixture.selector, b"arbitrary file").unwrap();
    assert!(backend.restore(&transaction).is_err());
    assert_eq!(fs::read(&fixture.selector).unwrap(), b"arbitrary file");
}

#[test]
fn all_installs_refuse_when_private_evidence_cannot_be_written() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-no-evidence", Some("old"), "new", &new_sha);
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
    let error = backend.prepare(&transaction).unwrap_err().to_string();
    assert!(error.contains("private"), "{error}");
    assert!(mock
        .borrow()
        .requests
        .iter()
        .all(|request| request.operation != ControlOperation::PrepareUpgrade));
    assert_eq!(fixture.selector_release().as_deref(), Some("old"));
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o700)).unwrap();

    // First install has no old release to preserve, so it does not need the
    // previous-release sidecar.
    let fixture = Fixture::new(Topology::CoreOnly);
    let new_sha = fixture.write_release("new");
    let (mut backend, _mock) = fixture.backend(fixture.mock(false));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-first-no-sidecar", None, "new", &new_sha);
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(backend.prepare(&transaction).is_err());
    assert!(fixture.evidence_files().is_empty());
}

#[test]
fn release_maintenance_requires_real_state_and_identity() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let mut transaction = fixture.transaction("tx-release", Some("old"), "new", &new_sha);
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();
    backend.resume(&transaction).unwrap();
    transaction.phase = Phase::Committed;

    mock.borrow_mut().leave_maintenance_ack = Some(true);
    assert!(backend.release_maintenance(&transaction).is_err());
    mock.borrow_mut().leave_maintenance_ack = Some(false);
    mock.borrow_mut().leave_maintenance_state = true;
    assert!(backend.release_maintenance(&transaction).is_err());
    mock.borrow_mut().leave_maintenance_state = false;
    backend.release_maintenance(&transaction).unwrap();
    assert!(!mock.borrow().maintenance);
    assert!(mock.borrow().ready);
}

#[test]
fn control_adapter_failures_are_never_reported_as_success() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    let new_sha = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-control-fail", Some("old"), "new", &new_sha);
    mock.borrow_mut().fail_control = Some(ControlOperation::PrepareUpgrade);
    assert!(backend.prepare(&transaction).is_err());
    mock.borrow_mut().fail_control = None;

    mock.borrow_mut().fail_control = Some(ControlOperation::Observe);
    assert!(backend.observe(INSTANCE).is_err());
    assert!(
        !fixture
            .selector_release()
            .as_deref()
            .is_some_and(|release| release == "new"),
        "failed observation must not move the selector"
    );
}

#[test]
fn selector_target_validation_rejects_non_normal_and_outside_store_paths() {
    let fixture = Fixture::new(Topology::CoreOnly);
    fixture.write_release("old");
    fs::remove_file(&fixture.selector).ok();
    symlink(fixture.base.join("outside"), &fixture.selector).unwrap();
    let (mut backend, _mock) = fixture.backend(fixture.mock(false));
    assert!(backend.observe(INSTANCE).is_err());

    fs::remove_file(&fixture.selector).unwrap();
    symlink(Path::new("relative/old"), &fixture.selector).unwrap();
    assert!(backend.observe(INSTANCE).is_err());
}

#[test]
fn first_install_evidence_is_bound_to_real_inactive_transport_observations() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let sha = fixture.write_release("new");
    let (mut backend, _) = fixture.backend(fixture.mock(false));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-first-proof", None, "new", &sha);
    backend.prepare(&transaction).unwrap();
    let path = fixture.state.join("first-install-evidence.json");
    let evidence: echo_deploy::runtime_adapter::FirstInstallEvidence =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    evidence.validate_shape().unwrap();
    assert_eq!(evidence.instance_id, INSTANCE);
    assert_eq!(evidence.target_manifest_sha256, sha);
    assert_eq!(evidence.units.len(), 1);
    assert_eq!(evidence.units[0].unit, fixture.core);
    assert_eq!(evidence.units[0].active_state, "inactive");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let fixture = Fixture::new(Topology::CoreOnly);
    let sha = fixture.write_release("new");
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    mock.borrow_mut().ready = false;
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-first-active", None, "new", &sha);
    assert!(backend.prepare(&transaction).is_err());
    assert!(!fixture.state.join("first-install-evidence.json").exists());
    assert!(mock
        .borrow()
        .requests
        .iter()
        .all(|request| request.operation != ControlOperation::PrepareUpgrade));
}

#[test]
fn first_install_drain_refreshes_evidence_only_after_units_are_confirmed_inactive() {
    let fixture = Fixture::new(Topology::CoreOnly);
    let sha = fixture.write_release("new");
    let (mut backend, mock) = fixture.backend(fixture.mock(false));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-fresh-drain", None, "new", &sha);
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    let path = fixture.state.join("first-install-evidence.json");
    let mut evidence: echo_deploy::runtime_adapter::FirstInstallEvidence =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    evidence.observed_at_unix_ms = 0;
    fs::write(&path, serde_json::to_vec(&evidence).unwrap()).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    let fresh: echo_deploy::runtime_adapter::FirstInstallEvidence =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(fresh.observed_at_unix_ms > 0);
    mock.borrow_mut()
        .states
        .insert(fixture.core.clone(), ActiveState::Active);
    assert!(backend.drain(&transaction).is_err());
}

#[test]
fn target_artifact_tamper_is_rejected_before_prepare_or_service_side_effects() {
    for path in [
        "bin/echo-agent-core",
        "bin/echo-agent-panel",
        "web/index.html",
    ] {
        let fixture = Fixture::new(Topology::Standalone);
        fixture.write_release("old");
        let digest = fixture.write_release("new");
        fixture.set_selector(Some("old"));
        let (mut backend, mock) = fixture.backend(fixture.mock(true));
        backend.observe(INSTANCE).unwrap();
        let transaction = fixture.transaction("tx-tampered-bytes", Some("old"), "new", &digest);
        fs::write(fixture.store.join("new").join(path), b"x").unwrap();
        let error = backend.prepare(&transaction).unwrap_err();
        assert!(format!("{error:#}").contains(path), "{error:#}");
        assert!(mock
            .borrow()
            .requests
            .iter()
            .all(|request| request.operation != ControlOperation::PrepareUpgrade));
        assert_eq!(fixture.selector_release().as_deref(), Some("old"));
    }
}

#[test]
fn old_artifact_tamper_refuses_rollback_before_stop_or_selector_change() {
    let fixture = Fixture::new(Topology::Standalone);
    fixture.write_release("old");
    let digest = fixture.write_release("new");
    fixture.set_selector(Some("old"));
    let (mut backend, mock) = fixture.backend(fixture.mock(true));
    backend.observe(INSTANCE).unwrap();
    let transaction = fixture.transaction("tx-old-byte-drift", Some("old"), "new", &digest);
    backend.prepare(&transaction).unwrap();
    backend.enter_maintenance(&transaction).unwrap();
    assert_eq!(backend.drain(&transaction).unwrap(), Drain::Drained);
    backend.switch(&transaction).unwrap();
    fs::write(fixture.store.join("old/web/index.html"), b"x").unwrap();
    let count = mock.borrow().events.len();
    assert!(backend.restore(&transaction).is_err());
    assert_eq!(mock.borrow().events.len(), count);
    assert_eq!(fixture.selector_release().as_deref(), Some("new"));
}
