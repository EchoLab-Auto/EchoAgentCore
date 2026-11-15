use echo_deploy::{model::*, pack::*, verify};
use std::fs;

fn template() -> Release {
    Release {
        schema_version: 1,
        release_id: "dev-pack".into(),
        target_os: "linux".into(),
        target_arch: std::env::consts::ARCH.into(),
        management_protocol: "p1".into(),
        development: true,
        sources: vec![Source {
            id: "source".into(),
            revision: "snapshot".into(),
            snapshot_sha256: "a".repeat(64),
            lock_sha256: "b".repeat(64),
        }],
        artifacts: vec![],
    }
}
#[test]
fn pack_copies_full_web_tree_and_hashes_actual_output() {
    let t = tempfile::tempdir().unwrap();
    let binary = t.path().join("core");
    fs::write(&binary, b"binary").unwrap();
    let web = t.path().join("frontend");
    fs::create_dir(&web).unwrap();
    fs::create_dir(web.join("assets")).unwrap();
    fs::write(web.join("index.html"), b"index").unwrap();
    fs::write(web.join("assets/main.js"), b"javascript").unwrap();
    let inputs = vec![
        BuildOutput {
            kind: Kind::Core,
            input: binary,
            source_id: "source".into(),
        },
        BuildOutput {
            kind: Kind::Web,
            input: web,
            source_id: "source".into(),
        },
    ];
    let output = t.path().join("bundle");
    let r = assemble(template(), &inputs, &output).unwrap();
    assert_eq!(r.artifacts.len(), 3);
    verify(&output).unwrap();
    require_complete(&output, &r).unwrap();
    assert!(assemble(template(), &inputs, &output).is_err());
    fs::write(output.join("extra"), b"extra").unwrap();
    assert!(require_complete(&output, &r).is_err());
}
#[test]
fn pack_rejects_links_and_never_executes_input() {
    let t = tempfile::tempdir().unwrap();
    let binary = t.path().join("binary");
    fs::write(&binary, b"#!/bin/sh\nexit 99\n").unwrap();
    let output = t.path().join("bundle");
    assemble(
        template(),
        &[BuildOutput {
            kind: Kind::Core,
            input: binary.clone(),
            source_id: "source".into(),
        }],
        &output,
    )
    .unwrap();
    let web = t.path().join("web");
    fs::create_dir(&web).unwrap();
    std::os::unix::fs::symlink(&binary, web.join("linked")).unwrap();
    assert!(assemble(
        template(),
        &[BuildOutput {
            kind: Kind::Web,
            input: web,
            source_id: "source".into()
        }],
        &t.path().join("bad")
    )
    .is_err());
    assert!(!t.path().join("bad").exists());
}
