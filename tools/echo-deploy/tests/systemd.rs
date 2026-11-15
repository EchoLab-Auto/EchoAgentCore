//! Behavior tests for the systemd execution primitives.
//!
//! No real systemctl, service, control adapter or network is used. The module is
//! included directly so its API can be exercised before library wiring lands.

#[path = "../src/model.rs"]
#[allow(dead_code)]
mod model;
#[path = "../src/systemd.rs"]
#[allow(dead_code)]
mod systemd;

use anyhow::{bail, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use systemd::*;
use tempfile::{tempdir, TempDir};

const INSTANCE: &str = "local-dev";
const CORE: &str = "echo-agent-local-dev-core.service";
const PANEL: &str = "echo-agent-local-dev-panel.service";

fn deadlines() -> Deadlines {
    Deadlines::new(
        Duration::from_secs(1),
        Duration::from_millis(120),
        Duration::from_millis(120),
        Duration::from_secs(1),
    )
}

fn artifact_bytes(id: &str, kind: &str) -> Vec<u8> {
    format!("{kind}-binary-{id}").into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Strict `crate::model::Release` shape: core + panel + web, one source.
fn release_manifest(id: &str, release_id: &str) -> Vec<u8> {
    let core = artifact_bytes(id, "core");
    let panel = artifact_bytes(id, "panel");
    let web = artifact_bytes(id, "web");
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "release_id": release_id,
        "target_os": "linux",
        "target_arch": "x86_64",
        "management_protocol": "management-1",
        "development": true,
        "sources": [{
            "id": "source",
            "revision": "test",
            "snapshot_sha256": "a".repeat(64),
            "lock_sha256": "b".repeat(64),
        }],
        "artifacts": [
            {
                "id": "core-bin",
                "kind": "core",
                "path": "bin/echo-agent-core",
                "sha256": sha256_hex(&core),
                "size": core.len(),
                "executable": true,
                "source_id": "source",
            },
            {
                "id": "panel-bin",
                "kind": "panel",
                "path": "bin/echo-agent-panel",
                "sha256": sha256_hex(&panel),
                "size": panel.len(),
                "executable": true,
                "source_id": "source",
            },
            {
                "id": "panel-web",
                "kind": "web",
                "path": "web/index.html",
                "sha256": sha256_hex(&web),
                "size": web.len(),
                "executable": false,
                "source_id": "source",
            },
        ],
    }))
    .unwrap()
}

fn write_artifact(directory: &Path, relative: &str, bytes: &[u8], mode: u32) {
    let path = directory.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
}

fn write_release_files(directory: &Path, id: &str) {
    write_artifact(
        directory,
        "bin/echo-agent-core",
        &artifact_bytes(id, "core"),
        0o755,
    );
    write_artifact(
        directory,
        "bin/echo-agent-panel",
        &artifact_bytes(id, "panel"),
        0o755,
    );
    write_artifact(
        directory,
        "web/index.html",
        &artifact_bytes(id, "web"),
        0o644,
    );
}

struct Fixture {
    _root: TempDir,
    base: PathBuf,
    spec: SystemdSpec,
}

impl Fixture {
    fn new() -> Self {
        Self::with_deadlines(deadlines())
    }

    fn with_deadlines(deadlines: Deadlines) -> Self {
        let root = tempdir().unwrap();
        let base = root.path().to_path_buf();
        fs::create_dir(base.join("store")).unwrap();
        fs::create_dir(base.join("state")).unwrap();
        fs::create_dir(base.join("units")).unwrap();
        let adapter = base.join("adapter");
        fs::write(&adapter, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        let spec = SystemdSpec::new(
            INSTANCE,
            Scope::User,
            CORE,
            Some(PANEL.into()),
            base.join("units"),
            base.join("state"),
            base.join("state/current"),
            base.join("store"),
            adapter,
            deadlines,
        )
        .unwrap();
        Self {
            _root: root,
            base,
            spec,
        }
    }

    fn store(&self) -> PathBuf {
        self.base.join("store")
    }
    fn state(&self) -> PathBuf {
        self.base.join("state")
    }
    fn selector(&self) -> PathBuf {
        self.base.join("state/current")
    }
    fn adapter(&self) -> PathBuf {
        self.base.join("adapter")
    }

    fn spec_with(
        &self,
        scope: Scope,
        adapter: PathBuf,
        deadlines: Deadlines,
    ) -> Result<SystemdSpec> {
        SystemdSpec::new(
            INSTANCE,
            scope,
            CORE,
            Some(PANEL.into()),
            self.base.join("units"),
            self.state(),
            self.selector(),
            self.store(),
            adapter,
            deadlines,
        )
    }

    fn release(&self, id: &str) -> String {
        self.release_with_id(id, id)
    }

    fn release_with_id(&self, directory_id: &str, manifest_id: &str) -> String {
        let directory = self.store().join(directory_id);
        fs::create_dir(&directory).unwrap();
        let bytes = release_manifest(directory_id, manifest_id);
        fs::write(directory.join("release.json"), &bytes).unwrap();
        write_release_files(&directory, directory_id);
        sha256_hex(&bytes)
    }

    fn panel_sha(&self, id: &str) -> String {
        sha256_hex(&artifact_bytes(id, "panel"))
    }

    fn manifest_release(&self, id: &str, manifest: &[u8]) -> String {
        let directory = self.store().join(id);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("release.json"), manifest).unwrap();
        write_release_files(&directory, id);
        sha256_hex(manifest)
    }

    fn backend<T: Transport>(&self, transport: T) -> SystemdBackend<T> {
        SystemdBackend::new(self.spec.clone(), transport).unwrap()
    }

    fn core_only_spec(&self) -> SystemdSpec {
        SystemdSpec::new(
            INSTANCE,
            Scope::User,
            CORE,
            None,
            self.base.join("units"),
            self.state(),
            self.selector(),
            self.store(),
            self.adapter(),
            deadlines(),
        )
        .unwrap()
    }

    fn core_only_backend<T: Transport>(&self, transport: T) -> SystemdBackend<T> {
        SystemdBackend::new(self.core_only_spec(), transport).unwrap()
    }
}

struct Mock {
    states: BTreeMap<String, VecDeque<ActiveState>>,
    default_state: ActiveState,
    observe: ControlReply,
    ready_reply: bool,
    ready_not_yet: usize,
    reject: Option<ControlOperation>,
    stop_error: Option<String>,
    image_error: Option<String>,
    calls: Vec<String>,
    requests: Vec<ControlRequest>,
    images: Vec<(String, String)>,
    events: Vec<String>,
}

impl Mock {
    fn new(current: Option<&str>) -> Self {
        Self {
            states: BTreeMap::new(),
            default_state: ActiveState::Active,
            observe: ControlReply::observed(INSTANCE, current.map(str::to_owned), true, false, 0),
            ready_reply: true,
            ready_not_yet: 0,
            reject: None,
            stop_error: None,
            image_error: None,
            calls: Vec::new(),
            requests: Vec::new(),
            images: Vec::new(),
            events: Vec::new(),
        }
    }

    fn queue(&mut self, unit: &str, states: &[ActiveState]) {
        self.states
            .entry(unit.to_owned())
            .or_default()
            .extend(states.iter().copied());
    }

    fn maintenance(&mut self, on: bool) {
        self.observe.maintenance = Some(on);
    }

    fn active_work(&mut self, count: u32) {
        self.observe.active_work = Some(count);
    }

    fn control_operations(&self) -> Vec<ControlOperation> {
        self.requests
            .iter()
            .map(|request| request.operation)
            .collect()
    }
}

impl Transport for Mock {
    fn start_unit(&mut self, _: &SystemdSpec, unit: &str, _: Duration) -> Result<()> {
        self.calls.push(format!("start:{unit}"));
        Ok(())
    }

    fn stop_unit(&mut self, _: &SystemdSpec, unit: &str, _: Duration) -> Result<()> {
        self.calls.push(format!("stop:{unit}"));
        if let Some(error) = &self.stop_error {
            bail!("{error}");
        }
        Ok(())
    }

    fn active_state(&mut self, _: &SystemdSpec, unit: &str, _: Duration) -> Result<ActiveState> {
        self.calls.push(format!("state:{unit}"));
        Ok(self
            .states
            .get_mut(unit)
            .and_then(VecDeque::pop_front)
            .unwrap_or(self.default_state))
    }

    fn verify_process_image(
        &mut self,
        _: &SystemdSpec,
        unit: &str,
        expected_sha256: &str,
        _: Duration,
    ) -> Result<()> {
        self.calls.push(format!("image:{unit}"));
        self.events.push(format!("image:{unit}"));
        self.images
            .push((unit.to_owned(), expected_sha256.to_owned()));
        if let Some(error) = &self.image_error {
            bail!("{error}");
        }
        Ok(())
    }

    fn control(
        &mut self,
        _: &SystemdSpec,
        request: &ControlRequest,
        _: Duration,
    ) -> Result<ControlReply> {
        self.requests.push(request.clone());
        self.events.push(format!("control:{:?}", request.operation));
        if self.reject == Some(request.operation) {
            let mut reply = ControlReply::ack(request.operation, request.instance.clone());
            reply.success = false;
            reply.error = Some("unsupported operation".into());
            return Ok(reply);
        }
        let reply = match request.operation {
            ControlOperation::Observe => self.observe.clone(),
            ControlOperation::PrepareUpgrade => {
                ControlReply::ack(request.operation, request.instance.clone())
            }
            ControlOperation::Ready => {
                let mut reply = ControlReply::ack(request.operation, request.instance.clone());
                reply.ready = Some(self.ready_reply && self.ready_not_yet == 0);
                self.ready_not_yet = self.ready_not_yet.saturating_sub(1);
                reply.current_release = request.release.clone();
                reply
            }
            ControlOperation::MaintenanceEnter => {
                let mut reply = ControlReply::ack(request.operation, request.instance.clone());
                reply.maintenance = Some(true);
                reply
            }
            ControlOperation::MaintenanceLeave => {
                let mut reply = ControlReply::ack(request.operation, request.instance.clone());
                reply.maintenance = Some(false);
                reply
            }
            ControlOperation::Drain => {
                let mut reply = ControlReply::ack(request.operation, request.instance.clone());
                reply.active_work = self.observe.active_work;
                reply
            }
        };
        Ok(reply)
    }
}

/// A transport that deliberately relies on the trait's default
/// `verify_process_image` (which must reject rather than pass silently).
struct UnsupportedImageTransport {
    inner: Mock,
}

impl Transport for UnsupportedImageTransport {
    fn start_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()> {
        self.inner.start_unit(spec, unit, deadline)
    }
    fn stop_unit(&mut self, spec: &SystemdSpec, unit: &str, deadline: Duration) -> Result<()> {
        self.inner.stop_unit(spec, unit, deadline)
    }
    fn active_state(
        &mut self,
        spec: &SystemdSpec,
        unit: &str,
        deadline: Duration,
    ) -> Result<ActiveState> {
        self.inner.active_state(spec, unit, deadline)
    }
    fn control(
        &mut self,
        spec: &SystemdSpec,
        request: &ControlRequest,
        deadline: Duration,
    ) -> Result<ControlReply> {
        self.inner.control(spec, request, deadline)
    }
}

#[test]
fn spec_rejects_relative_escaping_duplicate_and_untrusted_shapes() {
    let fixture = Fixture::new();
    let base = &fixture.base;
    let make = |store: PathBuf, state: PathBuf, selector: PathBuf, adapter: PathBuf| {
        SystemdSpec::new(
            INSTANCE,
            Scope::User,
            CORE,
            Some(PANEL.into()),
            base.join("units"),
            state,
            selector,
            store,
            adapter,
            deadlines(),
        )
    };
    assert!(make(
        base.join("store"),
        base.join("state"),
        PathBuf::from("state/current"),
        fixture.adapter()
    )
    .is_err());
    assert!(make(
        PathBuf::from("store"),
        base.join("state"),
        base.join("state/current"),
        fixture.adapter()
    )
    .is_err());
    assert!(make(
        base.join("store"),
        base.join("state"),
        base.join("other/current"),
        fixture.adapter()
    )
    .is_err());
    assert!(make(
        base.join("store"),
        base.join("state"),
        base.join("state/current"),
        base.join("store/adapter")
    )
    .is_err());
    assert!(make(
        base.join("store"),
        base.join("state"),
        base.join("state/current"),
        PathBuf::from("adapter")
    )
    .is_err());
    assert!(fixture
        .spec_with(
            Scope::User,
            fixture.adapter(),
            Deadlines::new(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1)
            )
        )
        .is_err());
    assert!(SystemdSpec::new(
        INSTANCE,
        Scope::User,
        CORE,
        Some(CORE.into()),
        base.join("units"),
        base.join("state"),
        base.join("state/current"),
        base.join("store"),
        fixture.adapter(),
        deadlines(),
    )
    .is_err());
}

#[test]
fn systemctl_argv_is_fixed_shell_free_and_instance_scoped() {
    let fixture = Fixture::new();
    assert_eq!(SYSTEMCTL_PROGRAM, "/usr/bin/systemctl");
    assert_eq!(ExecTransport::new().program(), Path::new(SYSTEMCTL_PROGRAM));

    let start = systemctl_args(&fixture.spec, SystemctlVerb::Start, CORE).unwrap();
    assert_eq!(start[0], OsString::from("--user"));
    assert_eq!(start.last(), Some(&OsString::from(CORE)));
    assert!(systemctl_args(&fixture.spec, SystemctlVerb::Start, "other.service").is_err());
    assert!(systemctl_args(&fixture.spec, SystemctlVerb::Start, "evil;rm.service").is_err());

    let stop = systemctl_args(&fixture.spec, SystemctlVerb::Stop, CORE).unwrap();
    assert!(stop.contains(&OsString::from("--no-block")));
    let query = systemctl_args(&fixture.spec, SystemctlVerb::ActiveState, PANEL).unwrap();
    assert!(query.contains(&OsString::from("--property=ActiveState")));
    assert!(query.contains(&OsString::from("--value")));

    let main_pid = systemctl_args(&fixture.spec, SystemctlVerb::MainPid, PANEL).unwrap();
    assert!(main_pid.contains(&OsString::from("show")));
    assert!(main_pid.contains(&OsString::from("--property=MainPID")));
    assert!(main_pid.contains(&OsString::from("--value")));
    assert_eq!(main_pid.last(), Some(&OsString::from(PANEL)));

    assert!(ActiveState::parse("maintenance").is_err());

    let system = fixture
        .spec_with(Scope::System, fixture.adapter(), deadlines())
        .unwrap();
    let system_args = systemctl_args(&system, SystemctlVerb::Start, CORE).unwrap();
    assert!(!system_args.contains(&OsString::from("--user")));
}

#[test]
fn backend_requires_present_trusted_adapter() {
    let fixture = Fixture::new();
    let missing = fixture.base.join("missing-adapter");
    assert!(SystemdBackend::new(
        fixture
            .spec_with(Scope::User, missing, deadlines())
            .unwrap(),
        Mock::new(None)
    )
    .is_err());

    let no_execute = fixture.base.join("no-execute");
    fs::write(&no_execute, b"x").unwrap();
    fs::set_permissions(&no_execute, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(SystemdBackend::new(
        fixture
            .spec_with(Scope::User, no_execute, deadlines())
            .unwrap(),
        Mock::new(None)
    )
    .is_err());

    let writable = fixture.base.join("writable");
    fs::write(&writable, b"x").unwrap();
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(SystemdBackend::new(
        fixture
            .spec_with(Scope::User, writable, deadlines())
            .unwrap(),
        Mock::new(None)
    )
    .is_err());

    let link = fixture.base.join("adapter-link");
    symlink(fixture.adapter(), &link).unwrap();
    assert!(SystemdBackend::new(
        fixture.spec_with(Scope::User, link, deadlines()).unwrap(),
        Mock::new(None)
    )
    .is_err());
}

#[test]
fn removing_adapter_after_construction_rejects_operation() {
    let fixture = Fixture::new();
    let mut backend = fixture.backend(Mock::new(None));
    fs::remove_file(fixture.adapter()).unwrap();
    let error = backend.enter_maintenance().unwrap_err().to_string();
    assert!(error.contains("control_adapter"), "{error}");
    assert!(backend.transport().requests.is_empty());
}

#[test]
fn unsupported_control_operation_is_rejected() {
    let fixture = Fixture::new();
    let digest = fixture.release("r1");
    let mut mock = Mock::new(None);
    mock.reject = Some(ControlOperation::PrepareUpgrade);
    let mut backend = fixture.backend(mock);
    let error = backend
        .prepare_upgrade("r1", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unsupported"), "{error}");
    assert!(!fixture.selector().exists());
}

#[test]
fn protocol_mismatch_fails_and_selector_disagreement_is_reported() {
    let fixture = Fixture::new();
    let digest = fixture.release("a");
    assert!(!digest.is_empty());
    symlink(fixture.store().join("a"), fixture.selector()).unwrap();

    let mut mock = Mock::new(Some("a"));
    mock.observe.protocol = "other-protocol".into();
    let mut backend = fixture.backend(mock);
    let error = backend.observe().unwrap_err().to_string();
    assert!(error.contains("protocol"), "{error}");

    let mut backend = fixture.backend(Mock::new(Some("b")));
    let observation = backend.observe().unwrap();
    assert_eq!(observation.current_release.as_deref(), Some("b"));
    assert_eq!(observation.selector_release.as_deref(), Some("a"));
    assert!(!observation.selector_matches());

    let mut backend = fixture.backend(Mock::new(Some("a")));
    let observation = backend.observe().unwrap();
    assert_eq!(observation.current_release.as_deref(), Some("a"));
    assert!(observation.selector_matches());
    assert!(observation.maintenance);
    assert!(!observation.ready);
    assert_eq!(observation.active_work, 0);
}

#[test]
fn core_only_spec_controls_only_the_core_unit() {
    let fixture = Fixture::new();
    let spec = SystemdSpec::new(
        INSTANCE,
        Scope::User,
        CORE,
        None,
        fixture.base.join("units"),
        fixture.state(),
        fixture.selector(),
        fixture.store(),
        fixture.adapter(),
        deadlines(),
    )
    .unwrap();
    let mut mock = Mock::new(None);
    mock.queue(CORE, &[ActiveState::Inactive, ActiveState::Active]);
    let mut backend = SystemdBackend::new(spec, mock).unwrap();
    backend.stop_units().unwrap();
    backend.start_units().unwrap();
    assert_eq!(
        backend.transport().calls,
        vec![
            format!("stop:{CORE}"),
            format!("state:{CORE}"),
            format!("start:{CORE}"),
            format!("state:{CORE}"),
        ]
    );
}

#[test]
fn stop_units_confirms_inactive_panel_before_core() {
    let fixture = Fixture::new();
    let mut mock = Mock::new(None);
    mock.queue(
        PANEL,
        &[
            ActiveState::Active,
            ActiveState::Deactivating,
            ActiveState::Inactive,
        ],
    );
    mock.queue(CORE, &[ActiveState::Active, ActiveState::Inactive]);
    let mut backend = fixture.backend(mock);
    backend.stop_units().unwrap();
    assert_eq!(
        backend.transport().calls,
        vec![
            format!("stop:{PANEL}"),
            format!("state:{PANEL}"),
            format!("state:{PANEL}"),
            format!("state:{PANEL}"),
            format!("stop:{CORE}"),
            format!("state:{CORE}"),
            format!("state:{CORE}"),
        ]
    );
}

#[test]
fn stop_units_never_turns_a_confirmation_timeout_into_success() {
    let fixture = Fixture::with_deadlines(Deadlines::new(
        Duration::from_secs(1),
        Duration::from_millis(40),
        Duration::from_secs(1),
        Duration::from_secs(1),
    ));
    let mut mock = Mock::new(None);
    mock.default_state = ActiveState::Active;
    let mut backend = fixture.backend(mock);
    let error = backend.stop_units().unwrap_err().to_string();
    assert!(error.contains("did not reach Inactive"), "{error}");
    assert!(backend
        .transport()
        .calls
        .iter()
        .any(|call| call == &format!("stop:{PANEL}")));
    assert!(!backend
        .transport()
        .calls
        .iter()
        .any(|call| call.contains("success")));
}

#[test]
fn stop_transport_error_is_not_success() {
    let fixture = Fixture::new();
    let mut mock = Mock::new(None);
    mock.stop_error = Some("systemctl stop timed out".into());
    let mut backend = fixture.backend(mock);
    let error = format!("{:#}", backend.stop_units().unwrap_err());
    assert!(error.contains("timed out"), "{error}");
    assert_eq!(backend.transport().calls, vec![format!("stop:{PANEL}")]);
}

#[test]
fn start_units_orders_core_then_panel_and_ready_is_explicit() {
    let fixture = Fixture::new();
    let mut backend = fixture.backend(Mock::new(None));
    backend.start_units().unwrap();
    assert_eq!(
        backend.transport().calls,
        vec![
            format!("start:{CORE}"),
            format!("state:{CORE}"),
            format!("start:{PANEL}"),
            format!("state:{PANEL}"),
        ]
    );
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    backend.verify_ready("r1").unwrap();
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Ready]
    );
    assert_eq!(
        backend.transport().requests[0].release.as_deref(),
        Some("r1")
    );
    assert_eq!(
        backend.transport().images,
        vec![(PANEL.to_owned(), fixture.panel_sha("r1"))]
    );
}

#[test]
fn standalone_readiness_verifies_panel_image_after_core_ready() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut backend = fixture.backend(Mock::new(Some("r1")));

    backend.verify_ready("r1").unwrap();

    // The Panel image proof runs only after the Core control adapter acked
    // Ready, and it receives the digest declared by the selected manifest.
    assert_eq!(
        backend.transport().events,
        vec![
            format!("control:{:?}", ControlOperation::Ready),
            format!("image:{PANEL}"),
        ]
    );
    assert_eq!(
        backend.transport().images,
        vec![(PANEL.to_owned(), fixture.panel_sha("r1"))]
    );
    assert_eq!(
        backend.transport().calls,
        vec![
            format!("state:{CORE}"),
            format!("state:{PANEL}"),
            format!("image:{PANEL}"),
        ]
    );
}

#[test]
fn wrong_panel_process_image_rejects_readiness() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut mock = Mock::new(Some("r1"));
    mock.image_error = Some("process image digest mismatch".into());
    let mut backend = fixture.backend(mock);

    let error = format!("{:#}", backend.verify_ready("r1").unwrap_err());
    assert!(
        error.contains("panel process image does not match the selected release"),
        "{error}"
    );
    assert!(error.contains("digest mismatch"), "{error}");
    assert_eq!(
        backend.transport().images,
        vec![(PANEL.to_owned(), fixture.panel_sha("r1"))]
    );
}

#[test]
fn readiness_rejects_tampered_staged_panel_bytes() {
    let fixture = Fixture::new();
    fixture.release("r1");
    let mut tampered = artifact_bytes("r1", "panel");
    tampered[0] ^= 1;
    fs::write(fixture.store().join("r1/bin/echo-agent-panel"), &tampered).unwrap();
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut backend = fixture.backend(Mock::new(Some("r1")));

    let error = format!("{:#}", backend.verify_ready("r1").unwrap_err());
    assert!(error.contains("panel artifact digest mismatch"), "{error}");
    assert!(backend.transport().images.is_empty());
}

#[test]
fn panel_main_pid_change_rejects_readiness() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut mock = Mock::new(Some("r1"));
    mock.image_error = Some(format!(
        "systemd MainPID changed while verifying {PANEL}: 41 -> 42"
    ));
    let mut backend = fixture.backend(mock);

    let error = format!("{:#}", backend.verify_ready("r1").unwrap_err());
    assert!(error.contains("MainPID changed"), "{error}");
    assert_eq!(
        backend.transport().images,
        vec![(PANEL.to_owned(), fixture.panel_sha("r1"))]
    );
}

#[test]
fn unsupported_process_image_transport_rejects_standalone_readiness() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut backend = fixture.backend(UnsupportedImageTransport {
        inner: Mock::new(Some("r1")),
    });

    let error = format!("{:#}", backend.verify_ready("r1").unwrap_err());
    assert!(
        error.contains("does not support process image verification"),
        "{error}"
    );
    assert_eq!(
        backend.transport().inner.control_operations(),
        vec![ControlOperation::Ready]
    );
}

#[test]
fn core_only_readiness_does_not_require_process_image_verification() {
    let fixture = Fixture::new();
    // No release, no selector: a Core-only spec must never read a Panel manifest
    // or call the (unsupported by default) process-image hook.
    let mut backend = fixture.core_only_backend(UnsupportedImageTransport {
        inner: Mock::new(None),
    });

    backend.verify_ready("r1").unwrap();
    assert_eq!(
        backend.transport().inner.calls,
        vec![format!("state:{CORE}")]
    );
    assert_eq!(
        backend.transport().inner.control_operations(),
        vec![ControlOperation::Ready]
    );
}

#[test]
fn standalone_readiness_requires_selector_to_name_the_release() {
    let fixture = Fixture::new();
    fixture.release("r1");
    fixture.release("r2");
    symlink(fixture.store().join("r2"), fixture.selector()).unwrap();
    let mut backend = fixture.backend(Mock::new(Some("r2")));

    let error = format!("{:#}", backend.verify_ready("r1").unwrap_err());
    assert!(
        error.contains("does not match readiness release"),
        "{error}"
    );
    assert!(backend.transport().images.is_empty());
}

#[test]
fn release_verification_requires_one_canonical_panel_artifact() {
    let fixture = Fixture::new();
    let manifest = |id: &str| -> serde_json::Value {
        serde_json::from_slice(&release_manifest(id, id)).unwrap()
    };

    let mut missing = manifest("missing");
    missing["artifacts"]
        .as_array_mut()
        .unwrap()
        .retain(|artifact| artifact["kind"] != "panel");
    let digest = fixture.manifest_release("missing", &serde_json::to_vec(&missing).unwrap());
    symlink(fixture.store().join("missing"), fixture.selector()).unwrap();
    let mut backend = fixture.backend(Mock::new(Some("missing")));
    backend.switch_release("missing", &digest).unwrap();
    let error = format!("{:#}", backend.verify_ready("missing").unwrap_err());
    assert!(error.contains("no panel artifact"), "{error}");

    let mut duplicate = manifest("duplicate");
    let mut second = duplicate["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "panel")
        .unwrap()
        .clone();
    second["id"] = json!("panel-bin-2");
    second["path"] = json!("bin/echo-agent-panel-2");
    duplicate["artifacts"].as_array_mut().unwrap().push(second);
    let digest = fixture.manifest_release("duplicate", &serde_json::to_vec(&duplicate).unwrap());
    let mut backend = fixture.backend(Mock::new(Some("duplicate")));
    let error = format!(
        "{:#}",
        backend.switch_release("duplicate", &digest).unwrap_err()
    );
    assert!(error.contains("exactly one panel artifact"), "{error}");

    let mut wrong_path = manifest("wrong-path");
    wrong_path["artifacts"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|artifact| artifact["kind"] == "panel")
        .unwrap()["path"] = json!("bin/echo-agent-panel-copy");
    let digest = fixture.manifest_release("wrong-path", &serde_json::to_vec(&wrong_path).unwrap());
    let mut backend = fixture.backend(Mock::new(Some("wrong-path")));
    let error = format!(
        "{:#}",
        backend.switch_release("wrong-path", &digest).unwrap_err()
    );
    assert!(error.contains("panel artifact path must be"), "{error}");
    assert!(backend.transport().images.is_empty());
}

#[test]
fn drain_reports_busy_without_forcing_work() {
    let fixture = Fixture::new();
    let mut backend = fixture.backend(Mock::new(None));
    backend.transport_mut().active_work(2);
    assert_eq!(
        backend.drain().unwrap(),
        DrainOutcome::Busy { remaining: 2 }
    );
    backend.transport_mut().active_work(0);
    assert_eq!(backend.drain().unwrap(), DrainOutcome::Drained);
    assert!(backend.transport().calls.is_empty());
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Drain, ControlOperation::Drain]
    );
}

#[test]
fn release_manifest_digest_id_and_strict_model_are_verified() {
    let fixture = Fixture::new();
    fixture.release("r1");
    let wrong = "0".repeat(64);
    let error = verify_release_directory(&fixture.spec, "r1", &wrong)
        .unwrap_err()
        .to_string();
    assert!(error.contains("digest mismatch"), "{error}");

    // A correctly hashed but minimal manifest is not a strict Release.
    let directory = fixture.store().join("minimal");
    fs::create_dir(&directory).unwrap();
    let minimal =
        serde_json::to_vec(&json!({"schema_version": 1, "release_id": "minimal"})).unwrap();
    fs::write(directory.join("release.json"), &minimal).unwrap();
    let digest = sha256_hex(&minimal);
    let error = verify_release_directory(&fixture.spec, "minimal", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("strict Release"), "{error}");

    // A fully valid manifest in the wrong directory is still rejected.
    let digest = fixture.release_with_id("wrong-id", "other");
    let error = verify_release_directory(&fixture.spec, "wrong-id", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("id does not match"), "{error}");
}

#[test]
fn switch_rejects_existing_non_symlink_and_out_of_store_target() {
    let fixture = Fixture::new();
    let digest = fixture.release("r1");
    fs::write(fixture.selector(), b"not a symlink").unwrap();
    let mut backend = fixture.backend(Mock::new(None));
    let error = backend
        .switch_release("r1", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not a symlink"), "{error}");
    assert!(fs::symlink_metadata(fixture.selector())
        .unwrap()
        .file_type()
        .is_file());

    fs::remove_file(fixture.selector()).unwrap();
    symlink("/etc", fixture.selector()).unwrap();
    let error = backend
        .switch_release("r1", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("escapes"), "{error}");
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        PathBuf::from("/etc")
    );

    fs::remove_file(fixture.selector()).unwrap();
    symlink(fixture.store().join("r1/extra"), fixture.selector()).unwrap();
    let error = backend
        .switch_release("r1", &digest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("exactly one release directory"), "{error}");
}

#[test]
fn switch_is_atomic_and_points_only_at_verified_release_directories() {
    let fixture = Fixture::new();
    let first = fixture.release("first");
    let second = fixture.release("second");
    let mut backend = fixture.backend(Mock::new(None));

    let switched = backend.switch_release("first", &first).unwrap();
    assert_eq!(switched.previous, None);
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("first")
    );
    let entries: Vec<String> = fs::read_dir(fixture.state())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["current".to_string()]);

    let switched = backend.switch_release("second", &second).unwrap();
    assert_eq!(switched.previous, Some(fixture.store().join("first")));
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("second")
    );
    assert_eq!(
        backend.current_release().unwrap().as_deref(),
        Some("second")
    );
}

#[test]
fn rollback_verifies_old_sha_before_any_side_effect() {
    let fixture = Fixture::new();
    let _old = fixture.release("old");
    fixture.release("new");
    symlink(fixture.store().join("new"), fixture.selector()).unwrap();
    let mut mock = Mock::new(Some("new"));
    mock.maintenance(true);
    mock.active_work(0);
    let mut backend = fixture.backend(mock);

    let error = backend
        .rollback_to("old", &"0".repeat(64))
        .unwrap_err()
        .to_string();
    assert!(error.contains("digest mismatch"), "{error}");
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("new")
    );
    assert!(backend.transport().calls.is_empty());
    assert!(backend.transport().requests.is_empty());
}

#[test]
fn rollback_requires_maintenance_and_drained_runtime() {
    let fixture = Fixture::new();
    let old = fixture.release("old");
    fixture.release("new");
    symlink(fixture.store().join("new"), fixture.selector()).unwrap();

    let mut mock = Mock::new(Some("new"));
    mock.maintenance(false);
    mock.active_work(0);
    let mut backend = fixture.backend(mock);
    let error = backend.rollback_to("old", &old).unwrap_err().to_string();
    assert!(error.contains("maintenance"), "{error}");
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("new")
    );
    assert!(backend.transport().calls.is_empty());

    let mut mock = Mock::new(Some("new"));
    mock.maintenance(true);
    mock.active_work(1);
    let mut backend = fixture.backend(mock);
    let error = backend.rollback_to("old", &old).unwrap_err().to_string();
    assert!(error.contains("drained"), "{error}");
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("new")
    );
    assert!(backend.transport().calls.is_empty());
}

#[test]
fn rollback_allows_transient_selector_runtime_mismatch_after_failed_start() {
    let fixture = Fixture::new();
    let old = fixture.release("old");
    fixture.release("new");
    symlink(fixture.store().join("new"), fixture.selector()).unwrap();
    let mut mock = Mock::new(None);
    mock.maintenance(true);
    mock.active_work(0);
    let mut backend = fixture.backend(mock);

    backend.rollback_to("old", &old).unwrap();
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("old")
    );
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Observe, ControlOperation::Ready]
    );
}

#[test]
fn rollback_switches_starts_and_confirms_old_release_ready() {
    let fixture = Fixture::new();
    let old = fixture.release("old");
    fixture.release("new");
    symlink(fixture.store().join("new"), fixture.selector()).unwrap();
    let mut mock = Mock::new(Some("new"));
    mock.maintenance(true);
    mock.active_work(0);
    let mut backend = fixture.backend(mock);

    backend.rollback_to("old", &old).unwrap();
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("old")
    );
    assert_eq!(
        backend.transport().calls,
        vec![
            format!("start:{CORE}"),
            format!("state:{CORE}"),
            format!("start:{PANEL}"),
            format!("state:{PANEL}"),
            format!("state:{CORE}"),
            format!("state:{PANEL}"),
            format!("image:{PANEL}"),
        ]
    );
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Observe, ControlOperation::Ready]
    );
    assert_eq!(
        backend.transport().requests[1].release.as_deref(),
        Some("old")
    );
}

#[test]
fn activate_requires_maintenance_and_drain_then_switches_and_verifies() {
    let fixture = Fixture::new();
    let digest = fixture.release("new");
    fixture.release("old");
    symlink(fixture.store().join("old"), fixture.selector()).unwrap();
    let mut mock = Mock::new(Some("old"));
    mock.maintenance(true);
    mock.active_work(0);
    let mut backend = fixture.backend(mock);

    backend.activate_release("new", &digest).unwrap();
    assert_eq!(
        fs::read_link(fixture.selector()).unwrap(),
        fixture.store().join("new")
    );
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Observe, ControlOperation::Ready]
    );
}

#[test]
fn ready_requires_an_explicit_true_ack() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut mock = Mock::new(None);
    mock.ready_reply = false;
    let mut backend = fixture.backend(mock);
    let error = backend.verify_ready("r1").unwrap_err().to_string();
    assert!(error.contains("readiness"), "{error}");
}

#[test]
fn maintenance_enter_and_leave_require_acknowledgement() {
    let fixture = Fixture::new();
    let mut backend = fixture.backend(Mock::new(None));
    backend.enter_maintenance().unwrap();
    backend.leave_maintenance().unwrap();
    assert_eq!(
        backend.transport().control_operations(),
        vec![
            ControlOperation::MaintenanceEnter,
            ControlOperation::MaintenanceLeave
        ]
    );
}

#[test]
fn control_requests_echo_protocol_instance_release_and_digest() {
    let fixture = Fixture::new();
    let digest = fixture.release("r1");
    let mut backend = fixture.backend(Mock::new(None));
    backend.prepare_upgrade("r1", &digest).unwrap();
    let request = &backend.transport().requests[0];
    assert_eq!(request.protocol, CONTROL_PROTOCOL);
    assert_eq!(request.operation, ControlOperation::PrepareUpgrade);
    assert_eq!(request.instance, INSTANCE);
    assert_eq!(request.release.as_deref(), Some("r1"));
    assert_eq!(request.manifest_sha256.as_deref(), Some(digest.as_str()));
}

#[test]
fn deadlines_are_bounded() {
    assert!(Deadlines::default().validate().is_ok());
    assert!(Deadlines::new(
        Duration::ZERO,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1)
    )
    .validate()
    .is_err());
    assert!(Deadlines::new(
        Duration::from_secs(3601),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1)
    )
    .validate()
    .is_err());
}

#[test]
fn readiness_retries_only_read_only_probe_until_socket_is_ready() {
    let fixture = Fixture::new();
    fixture.release("r1");
    symlink(fixture.store().join("r1"), fixture.selector()).unwrap();
    let mut mock = Mock::new(None);
    mock.ready_not_yet = 1;
    let mut backend = fixture.backend(mock);
    backend.verify_ready("r1").unwrap();
    assert_eq!(
        backend.transport().control_operations(),
        vec![ControlOperation::Ready, ControlOperation::Ready]
    );
    assert!(backend
        .transport()
        .calls
        .iter()
        .all(|call| !call.starts_with("start") && !call.starts_with("stop")));
}
