//! Activation boundary tests. No real service, systemctl, network or signing
//! key is used; cgroups are supplied as fake text and all paths are temporary.

use echo_deploy::{
    activation::{verify_snapshot, ActivationContext, CgroupGuard, CORE_BINARY_PATH},
    execution::Execution,
    init_store,
    model::*,
    transaction::{Phase, Transaction, TRANSACTION_SCHEMA_VERSION},
    trust::{self, TrustStore, VerifiedManifest},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{tempdir, TempDir};

const INSTANCE: &str = "test-inst";
const RELEASE: &str = "rel-1";
const KEY: &str = "test-key";
const UPDATE: &str = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-test-inst-update.service";
const CORE: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-test-inst-core.service";
const PANEL: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-test-inst-panel.service";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn openssl(args: &[&str]) {
    let output = Command::new(trust::OPENSSL).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    store: PathBuf,
    state: PathBuf,
    adapter: PathBuf,
    trust: PathBuf,
    private_key: PathBuf,
    manifest: Vec<u8>,
    auth: VerifiedManifest,
    config: Execution,
}

impl Fixture {
    fn new() -> Self {
        Self::with_core(CORE_BINARY_PATH)
    }

    fn development() -> Self {
        Self::with_core_development(CORE_BINARY_PATH, true)
    }

    fn with_core(core_path: &str) -> Self {
        Self::with_core_development(core_path, false)
    }

    fn with_core_development(core_path: &str, development: bool) -> Self {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let store = root.join("store");
        init_store(&store).unwrap();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = root.join("bin/hook");
        fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        fs::write(&adapter, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();

        let core_bytes = b"core-binary";
        let bundle = store.join(RELEASE);
        let core_file = bundle.join(core_path);
        fs::create_dir_all(core_file.parent().unwrap()).unwrap();
        fs::write(&core_file, core_bytes).unwrap();
        fs::set_permissions(&core_file, fs::Permissions::from_mode(0o755)).unwrap();
        let release = Release {
            schema_version: 1,
            release_id: RELEASE.into(),
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            management_protocol: "mgmt-1".into(),
            development,
            sources: vec![Source {
                id: "s".into(),
                revision: "r".into(),
                snapshot_sha256: "a".repeat(64),
                lock_sha256: "b".repeat(64),
            }],
            artifacts: vec![Artifact {
                id: "core".into(),
                kind: Kind::Core,
                path: core_path.into(),
                sha256: hash(core_bytes),
                size: core_bytes.len() as u64,
                executable: true,
                source_id: "s".into(),
            }],
        };
        let manifest = serde_json::to_vec(&release).unwrap();
        fs::write(bundle.join("release.json"), &manifest).unwrap();
        fs::set_permissions(
            bundle.join("release.json"),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();

        let trust = root.join("trust");
        fs::create_dir(&trust).unwrap();
        fs::set_permissions(&trust, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(trust.join("trust-format"), trust::TRUST_FORMAT).unwrap();
        fs::set_permissions(
            trust.join("trust-format"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let private_key = root.join("signing.pem");
        openssl(&[
            "genpkey",
            "-algorithm",
            "Ed25519",
            "-out",
            private_key.to_str().unwrap(),
        ]);
        let public = trust.join(format!("{KEY}.ed25519.pem"));
        openssl(&[
            "pkey",
            "-in",
            private_key.to_str().unwrap(),
            "-pubout",
            "-out",
            public.to_str().unwrap(),
        ]);
        fs::set_permissions(&public, fs::Permissions::from_mode(0o600)).unwrap();

        let config = Execution {
            schema_version: 1,
            deployment: Deployment {
                schema_version: 1,
                instance_id: INSTANCE.into(),
                topology: Topology::CoreOnly,
                scope: Scope::User,
                target_os: "linux".into(),
                target_arch: std::env::consts::ARCH.into(),
                store: store.to_str().unwrap().into(),
                required_modules: vec![],
            },
            state_dir: state.clone(),
            adapter: adapter.clone(),
            timeout_seconds: 30,
            reversible_data: true,
        };
        let auth = Self::sign_with(&trust, &private_key, &manifest);
        Self {
            _tmp: tmp,
            root,
            store,
            state,
            adapter,
            trust,
            private_key,
            manifest,
            auth,
            config,
        }
    }

    fn sign_with(trust: &Path, private_key: &Path, manifest: &[u8]) -> VerifiedManifest {
        let root = private_key.parent().unwrap();
        let input = root.join("manifest.bin");
        let signature = root.join("manifest.sig");
        fs::write(&input, manifest).unwrap();
        let input = input.to_str().unwrap();
        openssl(&[
            "pkeyutl",
            "-sign",
            "-inkey",
            private_key.to_str().unwrap(),
            "-rawin",
            "-in",
            input,
            "-out",
            signature.to_str().unwrap(),
        ]);
        TrustStore::open(trust)
            .unwrap()
            .verify_manifest(KEY, manifest, &signature)
            .unwrap()
    }

    fn sign(&self, manifest: &[u8]) -> VerifiedManifest {
        Self::sign_with(&self.trust, &self.private_key, manifest)
    }

    fn tx(&self) -> Transaction {
        Transaction {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            id: "tx-1".into(),
            instance_id: INSTANCE.into(),
            previous_release: None,
            target_release: RELEASE.into(),
            manifest_sha256: self.auth.sha256().into(),
            phase: Phase::Prepared,
            error: None,
            steps: vec![],
        }
    }

    fn open(&self) -> ActivationContext {
        ActivationContext::open_with_cgroup(&self.config, RELEASE, &self.auth, UPDATE).unwrap()
    }

    fn open_err(&self) -> String {
        format!(
            "{:#}",
            ActivationContext::open_with_cgroup(&self.config, RELEASE, &self.auth, UPDATE)
                .unwrap_err()
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Sealed 0555 snapshot dirs need a writable directory before TempDir
        // can remove them. Only trusted test-owned paths are visited.
        fn unlock(path: &Path) {
            if let Ok(meta) = fs::symlink_metadata(path) {
                if meta.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            unlock(&entry.path());
                        }
                    }
                }
            }
        }
        unlock(&self.root);
    }
}

fn open_development_err(config: &Execution, release: &str, allow_development: bool) -> String {
    format!(
        "{:#}",
        ActivationContext::open_development_with_cgroup(config, release, allow_development, UPDATE)
            .unwrap_err()
    )
}

fn rewrite_manifest(bundle: &Path, value: &serde_json::Value) {
    let path = bundle.join("release.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
}

#[test]
fn fake_cgroup_parser_rejects_core_and_panel_but_allows_update_unit() {
    for text in [
        CORE,
        PANEL,
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-core.service",
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-panel.service",
    ] {
        assert!(CgroupGuard::parse(INSTANCE, text).is_err(), "{text}");
    }
    for text in [
        UPDATE,
        "4:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-test-inst-update.service",
        "0::/system.slice/unrelated.service",
        "0::/",
    ] {
        assert!(CgroupGuard::parse(INSTANCE, text).is_ok(), "{text}");
    }
    assert!(CgroupGuard::parse(INSTANCE, "").is_err());
    assert!(CgroupGuard::parse(INSTANCE, "malformed").is_err());
    assert!(CgroupGuard::parse(INSTANCE, "0::relative").is_err());
}

#[test]
fn open_rejects_real_service_cgroups_and_keeps_update_unit_working() {
    let fixture = Fixture::new();
    for cgroup in [CORE, PANEL] {
        assert!(ActivationContext::open_with_cgroup(
            &fixture.config,
            RELEASE,
            &fixture.auth,
            cgroup
        )
        .is_err());
    }
    assert!(
        ActivationContext::open_with_cgroup(&fixture.config, RELEASE, &fixture.auth, UPDATE)
            .is_ok()
    );
}

#[test]
fn state_and_candidate_store_must_be_private() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.open_err().contains("private"));

    let fixture = Fixture::new();
    fs::set_permissions(&fixture.store, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.open_err().contains("private"));
}

#[test]
fn same_uid_trusted_adapter_still_cannot_cross_boundaries() {
    let mut fixture = Fixture::new();
    // Same UID and executable, but inside the candidate store.
    let in_store = fixture.store.join("hook");
    fs::write(&in_store, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&in_store, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.config.adapter = in_store;
    assert!(fixture.open_err().contains("candidate store"));

    let mut fixture = Fixture::new();
    // Same UID but inside the transaction state directory.
    let in_state = fixture.state.join("hook");
    fs::write(&in_state, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&in_state, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.config.adapter = in_state;
    assert!(fixture.open_err().contains("transaction state"));

    let mut fixture = Fixture::new();
    // Bidirectional nesting: state inside store, then store inside state.
    let nested_state = fixture.store.join("state");
    fs::create_dir(&nested_state).unwrap();
    fs::set_permissions(&nested_state, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.config.state_dir = nested_state;
    assert!(fixture.open_err().contains("must not nest"));

    let mut fixture = Fixture::new();
    let nested_store = fixture.state.join("store");
    // A marker is needed so the failure is the nesting rule, not a missing store.
    init_store(&nested_store).unwrap();
    fixture.config.deployment.store = nested_store.to_str().unwrap().into();
    assert!(fixture.open_err().contains("must not nest"));
}

#[test]
fn snapshot_pins_adapter_and_rejects_a_changed_hook() {
    let fixture = Fixture::new();
    let context = fixture.open();
    let transaction = fixture.tx();
    let snapshot = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    // Idempotent reopen returns the exact same path and digest.
    let again = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    assert_eq!(snapshot.path(), again.path());
    assert_eq!(snapshot.digest(), again.digest());
    assert_eq!(
        verify_snapshot(
            snapshot.path(),
            &fixture.config,
            &transaction,
            &fixture.auth
        )
        .unwrap()
        .digest(),
        snapshot.digest()
    );
    assert!(snapshot.path().starts_with(&fixture.state));

    let pinned = snapshot.pinned_execution(&fixture.config).unwrap();
    let original = b"#!/bin/sh\nexit 0\n";
    assert_eq!(fs::read(&pinned.adapter).unwrap(), original);
    assert_eq!(
        fs::metadata(&pinned.adapter).unwrap().permissions().mode() & 0o777,
        0o500
    );
    // The original administrator hook changes mid-transaction.
    fs::write(&fixture.adapter, b"#!/bin/sh\nexit 1\n").unwrap();
    let error = verify_snapshot(
        snapshot.path(),
        &fixture.config,
        &transaction,
        &fixture.auth,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("adapter changed"));
    assert!(context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .is_err());
    // The pinned bytes were not touched by the mutable hook.
    assert_eq!(fs::read(snapshot.adapter_path()).unwrap(), original);
}

#[test]
fn snapshot_rejects_changed_config_raw_drift_and_changed_manifest() {
    let fixture = Fixture::new();
    let context = fixture.open();
    let transaction = fixture.tx();
    let raw = serde_json::to_vec_pretty(&fixture.config).unwrap();
    let snapshot = context
        .prepare_snapshot(&fixture.config, &transaction, Some(&raw))
        .unwrap();
    assert_eq!(
        fs::read(snapshot.path().join("execution.json")).unwrap(),
        raw
    );
    assert!(context
        .prepare_snapshot(&fixture.config, &transaction, Some(b"{}"))
        .is_err());

    let mut changed = fixture.config.clone();
    changed.timeout_seconds += 1;
    assert!(verify_snapshot(snapshot.path(), &changed, &transaction, &fixture.auth).is_err());
    assert!(context
        .prepare_snapshot(&changed, &transaction, None)
        .is_err());

    let mut value: serde_json::Value = serde_json::from_slice(&fixture.manifest).unwrap();
    value["management_protocol"] = json!("mgmt-2");
    let changed_auth = fixture.sign(&serde_json::to_vec(&value).unwrap());
    assert!(verify_snapshot(
        snapshot.path(),
        &fixture.config,
        &transaction,
        &changed_auth
    )
    .is_err());
}

#[test]
fn accurate_core_path_and_missing_core_fail_closed() {
    let fixture = Fixture::with_core("bin/core");
    assert!(fixture.open_err().contains(CORE_BINARY_PATH));

    let fixture = Fixture::new();
    fs::remove_file(fixture.store.join(RELEASE).join(CORE_BINARY_PATH)).unwrap();
    assert!(!fixture.open_err().is_empty());
}

#[test]
fn snapshot_symlink_is_rejected() {
    let fixture = Fixture::new();
    let context = fixture.open();
    let transaction = fixture.tx();
    let snapshot = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    let real = fixture.state.join("real-snapshot");
    fs::rename(snapshot.path(), &real).unwrap();
    symlink(&real, snapshot.path()).unwrap();
    assert!(verify_snapshot(
        snapshot.path(),
        &fixture.config,
        &transaction,
        &fixture.auth
    )
    .is_err());
}

#[test]
fn development_open_requires_flag_manifest_opt_in_and_cgroup() {
    let fixture = Fixture::development();
    let error = open_development_err(&fixture.config, RELEASE, false);
    assert!(error.contains("development=true"), "{error}");
    assert!(error.contains("--allow-development"), "{error}");

    let fixture = Fixture::new();
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("development=true"), "{error}");
    assert!(error.contains("--allow-development"), "{error}");

    let fixture = Fixture::development();
    for cgroup in [CORE, PANEL] {
        assert!(ActivationContext::open_development_with_cgroup(
            &fixture.config,
            RELEASE,
            true,
            cgroup
        )
        .is_err());
    }
    assert!(ActivationContext::open_development_with_cgroup(
        &fixture.config,
        RELEASE,
        true,
        UPDATE
    )
    .is_ok());
    // The deterministic development entry point also rejects legacy global
    // units; only the real `open_development` migration path tolerates them.
    assert!(ActivationContext::open_development_with_cgroup(
        &fixture.config,
        RELEASE,
        true,
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-core.service"
    )
    .is_err());
}

#[test]
fn development_context_reuses_boundary_and_adapter_checks() {
    let mut fixture = Fixture::development();
    let in_store = fixture.store.join("hook");
    fs::write(&in_store, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&in_store, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.config.adapter = in_store;
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("candidate store"), "{error}");

    let mut fixture = Fixture::development();
    let nested_state = fixture.store.join("state");
    fs::create_dir(&nested_state).unwrap();
    fs::set_permissions(&nested_state, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.config.state_dir = nested_state;
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("must not nest"), "{error}");
}

#[test]
fn development_context_prepares_snapshot_without_verified_manifest() {
    let fixture = Fixture::development();
    let context =
        ActivationContext::open_development_with_cgroup(&fixture.config, RELEASE, true, UPDATE)
            .unwrap();
    let transaction = fixture.tx();
    let snapshot = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    let again = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    assert_eq!(snapshot.path(), again.path());
    assert_eq!(snapshot.digest(), again.digest());
    assert!(snapshot.path().starts_with(&fixture.state));

    let pinned = snapshot.pinned_execution(&fixture.config).unwrap();
    assert_eq!(fs::read(&pinned.adapter).unwrap(), b"#!/bin/sh\nexit 0\n");
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(snapshot.path().join("snapshot.json")).unwrap()).unwrap();
    assert_eq!(meta["release_id"].as_str(), Some(RELEASE));
    assert_eq!(
        meta["manifest_sha256"].as_str(),
        Some(fixture.auth.sha256())
    );

    let mut changed = fixture.tx();
    changed.manifest_sha256 = "c".repeat(64);
    assert!(context
        .prepare_snapshot(&fixture.config, &changed, None)
        .is_err());
}

#[test]
fn development_context_rejects_wrong_dir_identity_arch_and_topology() {
    let fixture = Fixture::development();
    fs::rename(fixture.store.join(RELEASE), fixture.store.join("other-rel")).unwrap();
    let error = open_development_err(&fixture.config, "other-rel", true);
    assert!(error.contains("manifest release_id"), "{error}");

    let fixture = Fixture::development();
    let bundle = fixture.store.join(RELEASE);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.join("release.json")).unwrap()).unwrap();
    value["target_arch"] = json!(if std::env::consts::ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    });
    rewrite_manifest(&bundle, &value);
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("target mismatch"), "{error}");

    let mut fixture = Fixture::development();
    fixture.config.deployment.topology = Topology::Standalone;
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("topology"), "{error}");
}

#[test]
fn development_context_rejects_noncanonical_core_non_executable_and_tampered_bytes() {
    let fixture = Fixture::with_core_development("bin/core", true);
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains(CORE_BINARY_PATH), "{error}");

    let fixture = Fixture::development();
    let core = fixture.store.join(RELEASE).join(CORE_BINARY_PATH);
    fs::set_permissions(&core, fs::Permissions::from_mode(0o644)).unwrap();
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(error.contains("not executable"), "{error}");

    let fixture = Fixture::development();
    let core = fixture.store.join(RELEASE).join(CORE_BINARY_PATH);
    fs::write(&core, b"core-binari").unwrap();
    let error = open_development_err(&fixture.config, RELEASE, true);
    assert!(!error.is_empty());
}

#[test]
fn recovery_reuses_only_the_durable_manifest_and_config_hashes() {
    let fixture = Fixture::new();
    let context = fixture.open();
    let transaction = fixture.tx();
    let snapshot = context
        .prepare_snapshot(&fixture.config, &transaction, None)
        .unwrap();
    let recovery =
        ActivationContext::open_recovery_with_cgroup(&fixture.config, &transaction, UPDATE)
            .unwrap();
    let reopened = recovery
        .load_snapshot_recovery(&fixture.config, &transaction)
        .unwrap();
    assert_eq!(snapshot.path(), reopened.path());
    assert_eq!(snapshot.digest(), reopened.digest());

    // A different durable config hash is rejected even before touching the snapshot.
    let mut changed = fixture.config.clone();
    changed.timeout_seconds += 1;
    let changed_context =
        ActivationContext::open_recovery_with_cgroup(&changed, &transaction, UPDATE).unwrap();
    assert!(changed_context
        .load_snapshot_recovery(&changed, &transaction)
        .is_err());

    // The candidate manifest must still hash to the durable transaction hash.
    let bundle = fixture.store.join(RELEASE);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.join("release.json")).unwrap()).unwrap();
    value["management_protocol"] = json!("mgmt-2");
    rewrite_manifest(&bundle, &value);
    assert!(
        ActivationContext::open_recovery_with_cgroup(&fixture.config, &transaction, UPDATE)
            .is_err()
    );
}
