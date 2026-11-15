use echo_deploy::{model::*, *};
use serde_json::json;
use std::path::Path;

fn release() -> Release {
    serde_json::from_value(json!({
        "schema_version":1,"release_id":"dev-1","target_os":"linux","target_arch":"x86_64",
        "management_protocol":"m1","development":true,
        "sources":[{"id":"s","revision":"abc","snapshot_sha256":"a".repeat(64),"lock_sha256":"b".repeat(64)}],
        "artifacts":[{"id":"a","kind":"core","path":"bin/core","sha256":"c".repeat(64),"size":1,"executable":true,"source_id":"s"}]
    })).unwrap()
}
#[test]
fn reserved_release_ids_and_file_directory_conflicts() {
    for id in ["pending-crash", "store-lock", "store-format"] {
        let mut r = release();
        r.release_id = id.into();
        assert!(r.validate().is_err());
    }
    let mut r = release();
    let mut conflict = r.artifacts[0].clone();
    conflict.id = "b".into();
    conflict.path = "bin/core/child".into();
    r.artifacts.push(conflict);
    assert!(r.validate().is_err());
}
#[test]
fn size_overflow_counts_deep_paths_and_unknown_fields() {
    let mut r = release();
    r.artifacts[0].size = u64::MAX;
    assert!(r.validate().is_err());
    r.artifacts.push({
        let mut a = r.artifacts[0].clone();
        a.id = "b".into();
        a.path = "bin/other".into();
        a
    });
    assert!(r.validate().is_err());
    assert!(relative_file(&vec!["a"; 65].join("/")).is_err());
    assert!(digest(&"A".repeat(64)).is_err());
    let mut v = serde_json::to_value(release()).unwrap();
    v["artifacts"][0]["unknown"] = json!(true);
    assert!(serde_json::from_value::<Release>(v).is_err());
    let repeated = serde_json::to_string(&release()).unwrap().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert!(serde_json::from_str::<Release>(&repeated).is_err());
}
#[test]
fn absolute_paths_and_existing_directories_fail_closed() {
    for path in ["/", "relative", "/tmp/../escape", "/tmp/./escape"] {
        assert!(absolute_path(Path::new(path)).is_err(), "{path}");
    }
    let tmp = tempfile::tempdir().unwrap();
    let sentinel = tmp.path().join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    assert!(init_store(tmp.path()).is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
}
#[test]
fn oversized_json_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("release.json"), vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
    assert!(verify(&bundle).is_err());
}
