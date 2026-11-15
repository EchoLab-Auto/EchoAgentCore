use echo_deploy::{
    model::*,
    safe_fs::{Dir, Lock},
    *,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _temp: TempDir,
    bundle: PathBuf,
    deployment: Deployment,
}
impl Fixture {
    fn new(topology: Topology) -> Self {
        let temp = tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir(&bundle).unwrap();
        let store = temp.path().join("candidates");
        init_store(&store).unwrap();
        let kinds = match topology {
            Topology::CoreOnly => vec![(Kind::Core, "bin/core")],
            Topology::Embedded => vec![(Kind::Core, "bin/core"), (Kind::Web, "web/index.html")],
            Topology::Standalone => vec![
                (Kind::Core, "bin/core"),
                (Kind::Panel, "bin/panel"),
                (Kind::Web, "web/index.html"),
                (Kind::Web, "web/assets/main.js"),
            ],
        };
        let mut artifacts = vec![];
        for (i, (kind, path)) in kinds.into_iter().enumerate() {
            fs::create_dir_all(bundle.join(path).parent().unwrap()).unwrap();
            let bytes = format!("artifact {i}");
            fs::write(bundle.join(path), &bytes).unwrap();
            artifacts.push(Artifact {
                id: format!("a{i}"),
                kind,
                path: path.into(),
                sha256: hash(bytes.as_bytes()),
                size: bytes.len() as u64,
                executable: kind != Kind::Web,
                source_id: "source".into(),
            });
        }
        let release = Release {
            schema_version: 1,
            release_id: "dev-test-1".into(),
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            management_protocol: "mgmt-1".into(),
            development: true,
            sources: vec![Source {
                id: "source".into(),
                revision: "test+dirty".into(),
                snapshot_sha256: "a".repeat(64),
                lock_sha256: "b".repeat(64),
            }],
            artifacts,
        };
        fs::write(
            bundle.join("release.json"),
            serde_json::to_vec(&release).unwrap(),
        )
        .unwrap();
        let deployment = Deployment {
            schema_version: 1,
            instance_id: "isolated".into(),
            topology,
            scope: Scope::User,
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            store: store.to_str().unwrap().into(),
            required_modules: vec!["echo-agent.session".into()],
        };
        Self {
            _temp: temp,
            bundle,
            deployment,
        }
    }
    fn mutate(&self, change: impl FnOnce(&mut Value)) {
        let path = self.bundle.join("release.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        change(&mut value);
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only trusted test-owned directories; do not follow fixture symlinks.
        fn unlock(path: &Path) {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            unlock(&entry.path());
                        }
                    }
                }
            }
        }
        unlock(self._temp.path());
    }
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_echo-deploy"))
}

#[test]
fn all_topologies_verify_plan_and_stage_without_activation() {
    for topology in [Topology::CoreOnly, Topology::Embedded, Topology::Standalone] {
        let f = Fixture::new(topology);
        // Undeclared payload (including links) must never enter the frozen package.
        symlink("/does-not-exist", f.bundle.join("unlisted")).unwrap();
        let b = verify(&f.bundle).unwrap();
        let p = plan(&b, &f.deployment).unwrap();
        assert!(!p.activation_supported);
        assert!(stage(&b, &f.deployment, false).is_err());
        stage(&b, &f.deployment, true).unwrap();
        let staged = verify(&p.candidate_destination).unwrap();
        assert_eq!(staged.manifest_sha256(), b.manifest_sha256());
        fs::write(f.bundle.join("bin/core"), "source changed").unwrap();
        assert!(verify(&p.candidate_destination).is_ok());
        assert!(!Path::new(&f.deployment.store)
            .join("active-release.json")
            .exists());
        assert_eq!(
            fs::metadata(p.candidate_destination.join("bin/core"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        assert_eq!(
            fs::metadata(&p.candidate_destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
    }
}

#[test]
fn strict_schema_identity_paths_hashes_and_sources() {
    for bad in [
        "../escape",
        "/tmp/escape",
        "a/../b",
        "a/./b",
        "a//b",
        "a/",
        "a\\b",
        "release.json",
        "release.json/part",
        "./a",
    ] {
        let f = Fixture::new(Topology::CoreOnly);
        f.mutate(|v| v["artifacts"][0]["path"] = json!(bad));
        assert!(verify(&f.bundle).is_err(), "accepted {bad}");
    }
    let changes: Vec<fn(&mut Value)> = vec![
        |v| v["schema_version"] = json!(2),
        |v| v["unknown"] = json!(true),
        |v| v["release_id"] = json!("../escape"),
        |v| v["artifacts"][0]["sha256"] = json!("f".repeat(64)),
        |v| v["artifacts"][0]["size"] = json!(999),
        |v| v["artifacts"][0]["source_id"] = json!("missing"),
        |v| v["artifacts"][0]["executable"] = json!(false),
        |v| v["sources"][0]["lock_sha256"] = json!("bad"),
        |v| {
            let duplicate = v["artifacts"][0].clone();
            v["artifacts"].as_array_mut().unwrap().push(duplicate);
        },
    ];
    for change in changes {
        let f = Fixture::new(Topology::CoreOnly);
        f.mutate(change);
        assert!(verify(&f.bundle).is_err());
    }
}

#[test]
fn topology_target_and_module_contracts_fail_closed() {
    let f = Fixture::new(Topology::CoreOnly);
    let b = verify(&f.bundle).unwrap();
    let mut d = f.deployment.clone();
    d.topology = Topology::Embedded;
    assert!(plan(&b, &d).is_err());
    d = f.deployment.clone();
    d.target_arch = "invalid".into();
    assert!(plan(&b, &d).is_err());
    f.mutate(|v| {
        v["artifacts"][0]["kind"] = json!("module");
    });
    assert!(plan(&verify(&f.bundle).unwrap(), &f.deployment).is_err());
}

#[test]
fn rejects_symlinks_hardlinks_directories_and_fifo() {
    for kind in ["symlink", "hardlink", "directory", "fifo"] {
        let f = Fixture::new(Topology::CoreOnly);
        let path = f.bundle.join("bin/core");
        let original = f._temp.path().join("original");
        fs::rename(&path, &original).unwrap();
        match kind {
            "symlink" => symlink(&original, &path).unwrap(),
            "hardlink" => fs::hard_link(&original, &path).unwrap(),
            "directory" => fs::create_dir(&path).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                // SAFETY: valid path, creating FIFO only inside a temporary fixture.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        assert!(verify(&f.bundle).is_err(), "accepted {kind}");
    }
    let f = Fixture::new(Topology::CoreOnly);
    let moved = f._temp.path().join("moved");
    fs::rename(f.bundle.join("bin"), &moved).unwrap();
    symlink(moved, f.bundle.join("bin")).unwrap();
    assert!(verify(&f.bundle).is_err());
}

#[test]
fn ancestor_and_manifest_symlinks_are_refused() {
    let f = Fixture::new(Topology::CoreOnly);
    let link = f._temp.path().join("link");
    symlink(&f.bundle, &link).unwrap();
    assert!(verify(&link).is_err());
    let manifest = f.bundle.join("release.json");
    let old = f._temp.path().join("manifest");
    fs::rename(&manifest, &old).unwrap();
    symlink(old, manifest).unwrap();
    assert!(verify(&f.bundle).is_err());
}

#[test]
fn copy_rechecks_mutated_source_and_freezes_manifest_bytes() {
    let f = Fixture::new(Topology::CoreOnly);
    let b = verify(&f.bundle).unwrap();
    f.mutate(|v| v["release_id"] = json!("changed-id"));
    stage(&b, &f.deployment, true).unwrap();
    assert!(Path::new(&f.deployment.store).join("dev-test-1").exists());
    assert!(!Path::new(&f.deployment.store).join("changed-id").exists());
    let f = Fixture::new(Topology::CoreOnly);
    let b = verify(&f.bundle).unwrap();
    fs::write(f.bundle.join("bin/core"), "bad bytes!").unwrap();
    assert!(stage(&b, &f.deployment, true).is_err());
    assert!(!Path::new(&f.deployment.store).join("dev-test-1").exists());
}

#[test]
fn publish_never_overwrites_and_store_is_locked() {
    let f = Fixture::new(Topology::CoreOnly);
    let b = verify(&f.bundle).unwrap();
    let store = Dir::absolute(Path::new(&f.deployment.store)).unwrap();
    let guard = Lock::acquire(&store).unwrap();
    assert!(stage(&b, &f.deployment, true).is_err());
    drop(guard);
    let p = stage(&b, &f.deployment, true).unwrap();
    let manifest = fs::read(p.candidate_destination.join("release.json")).unwrap();
    assert!(stage(&b, &f.deployment, true).is_err());
    assert_eq!(
        manifest,
        fs::read(p.candidate_destination.join("release.json")).unwrap()
    );
}

#[test]
fn trust_and_store_permissions_are_fail_closed() {
    let f = Fixture::new(Topology::CoreOnly);
    f.mutate(|v| v["development"] = json!(false));
    let b = verify(&f.bundle).unwrap();
    assert!(stage(&b, &f.deployment, true).is_err());
    let f = Fixture::new(Topology::CoreOnly);
    fs::set_permissions(&f.deployment.store, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(stage(&verify(&f.bundle).unwrap(), &f.deployment, true).is_err());
    assert!(init_store(Path::new(&f.deployment.store)).is_err());
    fs::set_permissions(&f.deployment.store, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(Path::new(&f.deployment.store).join("store-format")).unwrap();
    assert!(stage(&verify(&f.bundle).unwrap(), &f.deployment, true).is_err());
}

#[test]
fn cli_json_and_unsupported_apply_are_honest() {
    let f = Fixture::new(Topology::CoreOnly);
    let deployment = f._temp.path().join("deployment.json");
    fs::write(&deployment, serde_json::to_vec(&f.deployment).unwrap()).unwrap();
    let out = cli()
        .args(["verify", "--bundle"])
        .arg(&f.bundle)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["publisher_authenticated"], false);
    for command in ["inspect", "plan"] {
        let mut cmd = cli();
        cmd.arg(command).arg("--deployment").arg(&deployment);
        if command == "plan" {
            cmd.arg("--bundle").arg(&f.bundle);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!cli().arg("apply").output().unwrap().status.success());
    assert!(!cli()
        .args(["verify", "--bundle", "relative"])
        .output()
        .unwrap()
        .status
        .success());
    let output = cli()
        .arg("stage")
        .arg("--deployment")
        .arg(deployment)
        .arg("--bundle")
        .arg(&f.bundle)
        .arg("--allow-development")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["state"], "staged_not_activated");
}

#[test]
fn panel_gate_cli_uses_verified_bundle_identity_and_writes_no_configuration() {
    let fixture = Fixture::new(Topology::Standalone);
    let root = fixture._temp.path();
    let state = root.join("transactions");
    let runtime = root.join("runtime");
    let spec = json!({
        "instance_id":"isolated", "scope":"user", "topology":"standalone",
        "release_root":state.join("current"),
        "core_config":root.join("config/core.toml"),
        "panel_config":root.join("config/panel.toml"),
        "deploy_executable":root.join("echo-deploy"), "state_dir":state,
        "deployment":{
            "schema_version":1, "core_root":root.join("core-source"),
            "panel_root":root.join("panel-source"), "launcher_directory":root.join("bin"),
            "release_store":fixture.deployment.store, "instance_state":runtime,
            "core_socket":runtime.join("core.sock"), "runtime_config":runtime.join("runtime.json"),
            "panel_gate_file":runtime.join("gate.json"), "required_modules":[],
            "acknowledge_external_writers_stopped":false
        }
    });
    let path = root.join("install.json");
    fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
    let run = || {
        cli()
            .args(["panel-gate", "--spec"])
            .arg(&path)
            .arg("--bundle")
            .arg(&fixture.bundle)
            .output()
            .unwrap()
    };
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let gate: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(gate["release_id"], "dev-test-1");
    assert_eq!(gate["instance_id"], "isolated");
    assert_eq!(gate["management_protocol"], "mgmt-1");
    assert!(!runtime.exists());
    fs::write(fixture.bundle.join("web/index.html"), b"tampered").unwrap();
    assert!(!run().status.success());
    assert!(!runtime.exists());
}
