use echo_deploy::{
    model::*,
    pack::{assemble, BuildOutput},
};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
#[test]
fn cli_apply_commit_and_recover_use_frozen_candidate_and_durable_state() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("binary");
    fs::write(&source, b"not executed by fixture").unwrap();
    let output = t.path().join("bundle");
    let release = Release {
        schema_version: 1,
        release_id: "dev-new".into(),
        target_os: "linux".into(),
        target_arch: std::env::consts::ARCH.into(),
        management_protocol: "m1".into(),
        development: true,
        sources: vec![Source {
            id: "source".into(),
            revision: "test".into(),
            snapshot_sha256: "a".repeat(64),
            lock_sha256: "b".repeat(64),
        }],
        artifacts: vec![],
    };
    assemble(
        release,
        &[BuildOutput {
            kind: Kind::Core,
            input: source,
            source_id: "source".into(),
        }],
        &output,
    )
    .unwrap();
    let candidates = t.path().join("candidates");
    echo_deploy::init_store(&candidates).unwrap();
    let d = Deployment {
        schema_version: 1,
        instance_id: "local".into(),
        topology: Topology::CoreOnly,
        scope: Scope::User,
        target_os: "linux".into(),
        target_arch: std::env::consts::ARCH.into(),
        store: candidates.to_str().unwrap().into(),
        required_modules: vec![],
    };
    echo_deploy::stage(&echo_deploy::verify(&output).unwrap(), &d, true).unwrap();
    let state = t.path().join("transactions");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let actual = t.path().join("actual.json");
    fs::write(
        &actual,
        r#"{"current_release":"dev-old","maintenance":false,"ready":true,"active_work":0}"#,
    )
    .unwrap();
    let hook = t.path().join("hook");
    fs::write(&hook,format!(r#"#!/usr/bin/python3
import json,sys
p={}
r=json.load(sys.stdin)
s=json.load(open(p))
op=r['operation']
e={{}}
if op=='observe':
 assert '/snapshot-' in __file__, 'begin must use the pinned adapter'
 e=s
elif op=='maintenance': s['maintenance']=True; e={{'gate_closed':True}}
elif op=='prepare': e={{'drained':True,'saved':True,'writers_stopped':True}}
elif op=='switch': s['current_release']=r['release_id'];s['ready']=False
elif op=='start':s['ready']=True
elif op=='ready':e={{'application_ready':True,'release_id':r['release_id'],'manifest_sha256':r['manifest_sha256']}}
elif op=='resume':s['maintenance']=False;e={{'gate_open':True}}
elif op=='restore':s['current_release']=r['release_id'];s['ready']=True
json.dump(s,open(p,'w'))
r.update(success=True,evidence=e)
print(json.dumps(r))
"#,serde_json::to_string(actual.to_str().unwrap()).unwrap())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let config = t.path().join("execution.json");
    fs::write(&config,serde_json::to_vec(&json!({"schema_version":1,"deployment":d,"state_dir":state,"adapter":hook,"timeout_seconds":3,"reversible_data":true})).unwrap()).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_echo-deploy"))
            .args(args)
            .arg("--execution")
            .arg(&config)
            .output()
            .unwrap()
    };
    let rejected = run(&["apply", "--release", "dev-new", "--allow-development"]);
    assert!(!rejected.status.success());
    let applied = run(&[
        "apply",
        "--release",
        "dev-new",
        "--allow-development",
        "--confirm",
    ]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let status = run(&["status"]);
    assert!(status.status.success());
    let v: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(v["phase"], "committed");
    let recovered = run(&["recover", "--confirm"]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let a: Value = serde_json::from_slice(&fs::read(actual).unwrap()).unwrap();
    assert_eq!(a["current_release"], "dev-new");
    assert_eq!(a["maintenance"], false);
    // Test fixture cleanup only; production candidates deliberately remain read-only.
    fs::set_permissions(
        candidates.join("dev-new"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(
        candidates.join("dev-new/bin"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
}
