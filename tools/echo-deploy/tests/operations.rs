use echo_deploy::{operations, request::UpdateRequest};
use std::path::PathBuf;
#[test]
fn update_request_requires_explicit_authorization_and_all_trust_fields() {
    let mut r = UpdateRequest {
        schema_version: 1,
        execution: "/tmp/execution.json".into(),
        release: "r1".into(),
        transaction: "tx1".into(),
        allow_development: false,
        trust_store: None,
        key_id: None,
        signature: None,
    };
    assert!(r.validate().is_err());
    r.allow_development = true;
    r.validate().unwrap();
    r.key_id = Some("release".into());
    assert!(r.validate().is_err());
    r.trust_store = Some("/tmp/trust".into());
    r.signature = Some("/tmp/signature".into());
    r.validate().unwrap();
    r.execution = PathBuf::from("relative");
    assert!(r.validate().is_err());
}
#[test]
fn fleet_is_sequential_stop_on_failure_and_not_a_hidden_remote_executor() {
    let p = operations::fleet(vec![
        ("node-a".into(), "/tmp/a.json".into()),
        ("node-b".into(), "/tmp/b.json".into()),
    ])
    .unwrap();
    assert_eq!(p.parallelism, 1);
    assert!(p.stop_on_failure);
    assert!(!p.execution_supported);
    assert!(operations::fleet(vec![
        ("a".into(), "/tmp/a".into()),
        ("a".into(), "/tmp/b".into())
    ])
    .is_err());
}
