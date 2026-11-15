use echo_deploy::{
    execution::{invoke, Execution},
    model::*,
    state_lock::StateLock,
};
use std::{fs, os::unix::fs::PermissionsExt};
fn setup(script: &str) -> (tempfile::TempDir, Execution) {
    let t = tempfile::tempdir().unwrap();
    let hook = t.path().join("hook");
    fs::write(&hook, format!("#!/usr/bin/python3\n{script}")).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let store = t.path().join("store");
    echo_deploy::init_store(&store).unwrap();
    let d = Deployment {
        schema_version: 1,
        instance_id: "local".into(),
        topology: Topology::CoreOnly,
        scope: Scope::User,
        target_os: "linux".into(),
        target_arch: std::env::consts::ARCH.into(),
        store: store.to_str().unwrap().into(),
        required_modules: vec![],
    };
    let e = Execution {
        schema_version: 1,
        deployment: d,
        state_dir: t.path().join("state"),
        adapter: hook,
        timeout_seconds: 2,
        reversible_data: true,
    };
    fs::create_dir(&e.state_dir).unwrap();
    fs::set_permissions(&e.state_dir, fs::Permissions::from_mode(0o700)).unwrap();
    (t, e)
}
#[test]
fn rejects_false_drain_and_false_context() {
    let (_t,e)=setup("import json,sys\nr=json.load(sys.stdin)\nr.update(success=True,evidence={'drained':True,'saved':True,'writers_stopped':False})\nprint(json.dumps(r))");
    assert!(invoke(&e, "prepare", "tx", Some("dev"), None).is_err());
    let (_t,e)=setup("import json,sys\nr=json.load(sys.stdin)\nr.update(success=True,transaction_id='wrong',evidence={})\nprint(json.dumps(r))");
    assert!(invoke(&e, "observe", "tx", None, None).is_err());
}
#[test]
fn rejects_readiness_for_another_manifest() {
    let (_t,e)=setup("import json,sys\nr=json.load(sys.stdin)\nr.update(success=True,evidence={'application_ready':True,'release_id':'dev','manifest_sha256':'wrong'})\nprint(json.dumps(r))");
    assert!(invoke(&e, "ready", "tx", Some("dev"), Some(&"a".repeat(64))).is_err());
}
#[test]
fn accepts_checked_ack_and_holds_exclusive_state_lock() {
    let (_t,e)=setup("import json,sys\nr=json.load(sys.stdin)\nr.update(success=True,evidence={'drained':True,'saved':True,'writers_stopped':True})\nprint(json.dumps(r))");
    assert!(invoke(&e, "prepare", "tx", Some("dev"), None).is_ok());
    let lock = StateLock::acquire(&e.state_dir).unwrap();
    assert!(StateLock::acquire(&e.state_dir).is_err());
    drop(lock);
    assert!(StateLock::acquire(&e.state_dir).is_ok());
}
