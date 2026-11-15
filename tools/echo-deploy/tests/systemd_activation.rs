//! Authorization and durable snapshot tests for systemd activation.
//!
//! No real service, `systemctl`, control adapter, network or signing key is
//! used at runtime: cgroups are fake text, transports are never polled, and all
//! paths are temporary. Signed fixtures use the same local OpenSSL key setup as
//! `tests/activation.rs`.

use echo_deploy::{
    init_store,
    model::*,
    systemd_activation::{
        mapped_execution, provisional_transaction, systemd_config_sha256, SystemdActivationContext,
        EVIDENCE_FILE, SYSTEMD_CONFIG_FILE,
    },
    systemd_transaction::SystemdExecution,
    transaction::{BeginSpec, Phase, Transaction, TRANSACTION_SCHEMA_VERSION},
    trust::{self, TrustStore, VerifiedManifest},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{tempdir, TempDir};

const INSTANCE: &str = "sd-test";
const RELEASE: &str = "rel-1";
const KEY: &str = "test-key";
const TX_ID: &str = "tx-1";
const CORE_UNIT: &str = "echo-agent-sd-test-core.service";
const CORE_CGROUP: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-sd-test-core.service";
const PANEL_CGROUP: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-sd-test-panel.service";
const LEGACY_CORE_CGROUP: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-core.service";
const UPDATE: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/app.slice/echo-agent-sd-test-update.service";

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

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn overwrite_file(path: &Path, bytes: &[u8]) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    store: PathBuf,
    state: PathBuf,
    units: PathBuf,
    selector: PathBuf,
    adapter: PathBuf,
    systemd: SystemdExecution,
    raw: Vec<u8>,
    manifest_sha256: String,
    auth: Option<VerifiedManifest>,
    trust: Option<PathBuf>,
}

impl Fixture {
    fn development() -> Self {
        Self::new(Topology::CoreOnly, false)
    }

    fn development_standalone() -> Self {
        Self::new(Topology::Standalone, false)
    }

    fn signed() -> Self {
        Self::new(Topology::CoreOnly, true)
    }

    fn new(topology: Topology, signed: bool) -> Self {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let store = root.join("store");
        init_store(&store).unwrap();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let units = root.join("units");
        fs::create_dir(&units).unwrap();
        fs::set_permissions(&units, fs::Permissions::from_mode(0o755)).unwrap();
        let adapter = root.join("bin/hook");
        write_file(&adapter, b"#!/bin/sh\nexit 0\n", 0o700);

        let core_bytes = b"core-binary";
        let panel_bytes = b"panel-binary";
        let web_bytes = b"<html></html>";
        let mut artifacts = vec![Artifact {
            id: "core".into(),
            kind: Kind::Core,
            path: "bin/echo-agent-core".into(),
            sha256: hash(core_bytes),
            size: core_bytes.len() as u64,
            executable: true,
            source_id: "s".into(),
        }];
        if topology == Topology::Standalone {
            artifacts.push(Artifact {
                id: "panel".into(),
                kind: Kind::Panel,
                path: "bin/echo-agent-panel".into(),
                sha256: hash(panel_bytes),
                size: panel_bytes.len() as u64,
                executable: true,
                source_id: "s".into(),
            });
            artifacts.push(Artifact {
                id: "web".into(),
                kind: Kind::Web,
                path: "web/index.html".into(),
                sha256: hash(web_bytes),
                size: web_bytes.len() as u64,
                executable: false,
                source_id: "s".into(),
            });
        }
        let release = Release {
            schema_version: 1,
            release_id: RELEASE.into(),
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            management_protocol: "mgmt-1".into(),
            development: !signed,
            sources: vec![Source {
                id: "s".into(),
                revision: "r".into(),
                snapshot_sha256: "a".repeat(64),
                lock_sha256: "b".repeat(64),
            }],
            artifacts: artifacts.clone(),
        };
        let manifest = serde_json::to_vec(&release).unwrap();
        let bundle = store.join(RELEASE);
        fs::create_dir(&bundle).unwrap();
        write_file(&bundle.join("bin/echo-agent-core"), core_bytes, 0o755);
        if topology == Topology::Standalone {
            write_file(&bundle.join("bin/echo-agent-panel"), panel_bytes, 0o755);
            write_file(&bundle.join("web/index.html"), web_bytes, 0o644);
        }
        write_file(&bundle.join("release.json"), &manifest, 0o444);
        let manifest_sha256 = hash(&manifest);

        let panel_unit = (topology == Topology::Standalone)
            .then(|| format!("echo-agent-{INSTANCE}-panel.service"));
        let deployment = Deployment {
            schema_version: 1,
            instance_id: INSTANCE.into(),
            topology,
            scope: Scope::User,
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            store: store.to_str().unwrap().into(),
            required_modules: vec![],
        };
        let selector = state.join("current");
        let systemd = SystemdExecution::new(
            deployment,
            state.clone(),
            units.clone(),
            selector.clone(),
            CORE_UNIT,
            panel_unit,
            adapter.clone(),
            30,
            true,
        )
        .unwrap();
        let raw = serde_json::to_vec_pretty(&systemd).unwrap();
        let config_path = root.join("execution.json");
        write_file(&config_path, &raw, 0o644);

        let (auth, trust) = if signed {
            let trust = root.join("trust");
            fs::create_dir(&trust).unwrap();
            fs::set_permissions(&trust, fs::Permissions::from_mode(0o700)).unwrap();
            write_file(&trust.join("trust-format"), trust::TRUST_FORMAT, 0o600);
            let private_key = root.join("release-private.pem");
            openssl(&[
                "genpkey",
                "-algorithm",
                "Ed25519",
                "-out",
                private_key.to_str().unwrap(),
            ]);
            fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
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
            let auth = sign_with(&trust, &private_key, &manifest);
            (Some(auth), Some(trust))
        } else {
            (None, None)
        };

        Self {
            _tmp: tmp,
            root,
            store,
            state,
            units,
            selector,
            adapter,
            systemd,
            raw,
            manifest_sha256,
            auth,
            trust,
        }
    }

    fn tx(&self) -> Transaction {
        Transaction {
            schema_version: TRANSACTION_SCHEMA_VERSION,
            id: TX_ID.into(),
            instance_id: INSTANCE.into(),
            previous_release: None,
            target_release: RELEASE.into(),
            manifest_sha256: self.manifest_sha256.clone(),
            phase: Phase::Prepared,
            error: None,
            steps: vec![],
        }
    }

    fn open(&self) -> SystemdActivationContext {
        match &self.auth {
            Some(auth) => {
                SystemdActivationContext::open_with_cgroup(&self.systemd, RELEASE, auth, UPDATE)
                    .unwrap()
            }
            None => SystemdActivationContext::open_development_with_cgroup(
                &self.systemd,
                RELEASE,
                true,
                UPDATE,
            )
            .unwrap(),
        }
    }

    fn open_err(&self) -> String {
        self.open_err_with(&self.systemd)
    }

    fn open_err_with(&self, systemd: &SystemdExecution) -> String {
        let result = match &self.auth {
            Some(auth) => {
                SystemdActivationContext::open_with_cgroup(systemd, RELEASE, auth, UPDATE)
            }
            None => SystemdActivationContext::open_development_with_cgroup(
                systemd, RELEASE, true, UPDATE,
            ),
        };
        format!("{:#}", result.unwrap_err())
    }

    fn open_recovery(&self, transaction: &Transaction) -> SystemdActivationContext {
        SystemdActivationContext::open_recovery_with_cgroup(&self.systemd, transaction, UPDATE)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Sealed 0555 snapshot/evidence directories need a writable directory
        // before TempDir can remove them. Only trusted test-owned paths are
        // visited.
        fn unlock(path: &Path) {
            if let Ok(meta) = fs::symlink_metadata(path) {
                if meta.file_type().is_symlink() {
                    return;
                }
                if meta.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            unlock(&entry.path());
                        }
                    }
                } else {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
                }
            }
        }
        unlock(&self.root);
    }
}

fn sign_with(trust: &Path, private_key: &Path, manifest: &[u8]) -> VerifiedManifest {
    let root = private_key.parent().unwrap();
    let input = root.join("manifest.bin");
    let signature = root.join("manifest.sig");
    fs::write(&input, manifest).unwrap();
    openssl(&[
        "pkeyutl",
        "-sign",
        "-inkey",
        private_key.to_str().unwrap(),
        "-rawin",
        "-in",
        input.to_str().unwrap(),
        "-out",
        signature.to_str().unwrap(),
    ]);
    TrustStore::open(trust)
        .unwrap()
        .verify_manifest(KEY, manifest, &signature)
        .unwrap()
}

fn state_entries(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn remove_sealed(path: &Path) {
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
    unlock(path);
    fs::remove_dir_all(path).unwrap();
}

fn assert_full_hash_drift(
    fixture: &Fixture,
    context: &SystemdActivationContext,
    label: &str,
    mutate: impl FnOnce(&mut SystemdExecution),
    mapped_changes: bool,
) {
    let original_mapped = serde_json::to_vec(&mapped_execution(&fixture.systemd).unwrap()).unwrap();
    let mut drifted = fixture.systemd.clone();
    mutate(&mut drifted);
    let drifted_mapped = serde_json::to_vec(&mapped_execution(&drifted).unwrap()).unwrap();
    if mapped_changes {
        assert_ne!(drifted_mapped, original_mapped, "{label}");
    } else {
        assert_eq!(
            drifted_mapped, original_mapped,
            "{label}: systemd-only field must not change the generic projection"
        );
    }
    assert_ne!(
        systemd_config_sha256(&drifted).unwrap(),
        context.systemd_config_sha256(),
        "{label}: full config hash must cover every field"
    );
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&drifted, &fixture.tx(), None)
            .unwrap_err()
    );
    assert!(
        error.contains("full systemd config changed"),
        "{label}: {error}"
    );
}

fn assert_recovery_drift(
    fixture: &Fixture,
    transaction: &Transaction,
    label: &str,
    mutate: impl FnOnce(&mut SystemdExecution),
    needle: &str,
) {
    let mut drifted = fixture.systemd.clone();
    mutate(&mut drifted);
    let recovery =
        SystemdActivationContext::open_recovery_with_cgroup(&drifted, transaction, UPDATE)
            .unwrap_or_else(|error| panic!("{label}: open recovery: {error:#}"));
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&drifted, transaction)
            .unwrap_err()
    );
    assert!(
        error.to_lowercase().contains(needle),
        "{label}: expected {needle:?} in {error}"
    );
}

#[test]
fn fake_cgroup_rejects_core_panel_and_legacy_units() {
    let signed = Fixture::signed();
    let auth = signed.auth.as_ref().unwrap();
    for cgroup in [CORE_CGROUP, PANEL_CGROUP, LEGACY_CORE_CGROUP] {
        let error = format!(
            "{:#}",
            SystemdActivationContext::open_with_cgroup(&signed.systemd, RELEASE, auth, cgroup)
                .unwrap_err()
        );
        assert!(error.contains("refusing activation"), "{cgroup}: {error}");
    }
    assert!(
        SystemdActivationContext::open_with_cgroup(&signed.systemd, RELEASE, auth, UPDATE).is_ok()
    );

    let development = Fixture::development();
    for cgroup in [CORE_CGROUP, PANEL_CGROUP, LEGACY_CORE_CGROUP] {
        assert!(
            SystemdActivationContext::open_development_with_cgroup(
                &development.systemd,
                RELEASE,
                true,
                cgroup
            )
            .is_err(),
            "{cgroup}"
        );
    }
    assert!(SystemdActivationContext::open_development_with_cgroup(
        &development.systemd,
        RELEASE,
        true,
        UPDATE
    )
    .is_ok());
}

#[test]
fn apply_requires_authenticated_manifest_or_explicit_development() {
    let development = Fixture::development();
    let error = format!(
        "{:#}",
        SystemdActivationContext::open_development_with_cgroup(
            &development.systemd,
            RELEASE,
            false,
            UPDATE
        )
        .unwrap_err()
    );
    assert!(error.contains("requires development=true"), "{error}");

    let signed = Fixture::signed();
    let error = format!(
        "{:#}",
        SystemdActivationContext::open_development_with_cgroup(
            &signed.systemd,
            RELEASE,
            true,
            UPDATE
        )
        .unwrap_err()
    );
    assert!(error.contains("development=true"), "{error}");
}

#[test]
fn full_config_hash_covers_every_systemd_field_not_only_the_mapping() {
    let fixture = Fixture::development_standalone();
    let context = fixture.open();
    let other_adapter = fixture.root.join("bin/other-hook");
    write_file(&other_adapter, b"#!/bin/sh\nexit 1\n", 0o700);
    let other_units = fixture.root.join("other-units");
    let other_selector = fixture.state.join("other-current");

    assert_full_hash_drift(
        &fixture,
        &context,
        "unit_directory",
        |systemd| systemd.unit_directory = other_units,
        false,
    );
    assert_full_hash_drift(
        &fixture,
        &context,
        "current_selector",
        |systemd| systemd.current_selector = other_selector,
        false,
    );
    assert_full_hash_drift(
        &fixture,
        &context,
        "core_unit",
        |systemd| systemd.core_unit = "echo-agent-sd-test-core2.service".into(),
        false,
    );
    assert_full_hash_drift(
        &fixture,
        &context,
        "panel_unit",
        |systemd| systemd.panel_unit = None,
        false,
    );
    assert_full_hash_drift(
        &fixture,
        &context,
        "timeout_seconds",
        |systemd| systemd.timeout_seconds = 31,
        true,
    );
    assert_full_hash_drift(
        &fixture,
        &context,
        "control_adapter",
        |systemd| systemd.control_adapter = other_adapter,
        true,
    );
}

#[test]
fn context_exposes_all_frozen_parameters_and_clones() {
    let fixture = Fixture::development();
    let context = fixture.open();
    assert_eq!(context.instance_id(), INSTANCE);
    assert_eq!(context.release_id(), RELEASE);
    assert_eq!(context.manifest_sha256(), fixture.manifest_sha256);
    assert_eq!(
        context.systemd_config_sha256(),
        systemd_config_sha256(&fixture.systemd).unwrap()
    );
    assert_eq!(
        context.mapped_config_sha256(),
        hash(&serde_json::to_vec(&mapped_execution(&fixture.systemd).unwrap()).unwrap())
    );
    assert_eq!(
        context.control_adapter_sha256(),
        hash(&fs::read(&fixture.adapter).unwrap())
    );
    assert_eq!(context.control_adapter(), fixture.adapter);
    assert_eq!(context.state_dir(), fixture.state);
    assert_eq!(context.store_dir(), fixture.store);
    assert_eq!(context.unit_directory(), fixture.units);
    assert_eq!(context.current_selector(), fixture.selector);
    assert_eq!(context.core_unit(), CORE_UNIT);
    assert_eq!(context.panel_unit(), None);
    assert_eq!(context.timeout_seconds(), 30);
    assert!(context.reversible_data());
    assert_eq!(context.deployment().instance_id, INSTANCE);
    assert_eq!(context.deployment().store, fixture.store.to_str().unwrap());

    let cloned = context.systemd_execution_clone();
    assert_eq!(
        systemd_config_sha256(&cloned).unwrap(),
        context.systemd_config_sha256()
    );
    assert_eq!(cloned.unit_directory, context.unit_directory());
    assert_eq!(cloned.current_selector, context.current_selector());
    assert_eq!(cloned.core_unit, context.core_unit());
    assert_eq!(cloned.panel_unit.as_deref(), context.panel_unit());
    assert_eq!(cloned.timeout_seconds, context.timeout_seconds());
    assert_eq!(cloned.reversible_data, context.reversible_data());
    let mapped = context.mapped_execution_clone();
    assert_eq!(mapped.adapter, fixture.adapter);
    assert_eq!(mapped.state_dir, fixture.state);
    assert_eq!(mapped.timeout_seconds, 30);
}

#[test]
fn provisional_transaction_supports_snapshot_before_begin() {
    let fixture = Fixture::development();
    let spec = BeginSpec::new(TX_ID, INSTANCE, RELEASE, fixture.manifest_sha256.clone());
    let provisional = provisional_transaction(&spec).unwrap();
    provisional.validate().unwrap();
    assert_eq!(provisional.id, TX_ID);
    assert_eq!(provisional.instance_id, INSTANCE);
    assert_eq!(provisional.target_release, RELEASE);
    assert_eq!(provisional.manifest_sha256, fixture.manifest_sha256);
    assert_eq!(provisional.previous_release, None);
    assert_eq!(provisional.phase, Phase::Prepared);
    assert!(provisional.steps.is_empty());

    let context = fixture.open();
    let snapshot = context
        .prepare_snapshot(&fixture.systemd, &provisional, None)
        .unwrap();
    assert_eq!(snapshot.transaction_id(), TX_ID);
    assert_eq!(snapshot.instance_id(), INSTANCE);
    assert_eq!(snapshot.release_id(), RELEASE);
    assert_eq!(snapshot.manifest_sha256(), fixture.manifest_sha256);
}

#[test]
fn prepare_snapshot_is_idempotent_and_pins_full_config_and_adapter() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let first = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    let second = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    assert_eq!(first.path(), second.path());
    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.evidence_digest(), second.evidence_digest());
    assert_eq!(first.evidence_path(), second.evidence_path());
    assert_eq!(context.snapshot_path(&transaction), first.path());
    assert_eq!(context.evidence_path(&transaction), first.evidence_path());
    assert!(first.path().starts_with(&fixture.state));
    assert!(first.evidence_path().starts_with(&fixture.state));
    assert_eq!(
        fs::symlink_metadata(first.path()).unwrap().mode() & 0o777,
        0o555
    );
    assert_eq!(
        fs::symlink_metadata(first.evidence_path()).unwrap().mode() & 0o777,
        0o555
    );

    let stored = fs::read(first.systemd_config_path()).unwrap();
    assert_eq!(stored, fixture.raw);
    assert_eq!(first.systemd_config_file_sha256(), hash(&fixture.raw));
    assert_eq!(
        first.systemd_config_path(),
        first.evidence_path().join(SYSTEMD_CONFIG_FILE)
    );
    assert_eq!(
        first.evidence_file_path(),
        first.evidence_path().join(EVIDENCE_FILE)
    );
    let value: Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(
        value["unit_directory"],
        json!(fixture.units.to_str().unwrap())
    );
    assert_eq!(
        value["current_selector"],
        json!(fixture.selector.to_str().unwrap())
    );
    assert_eq!(value["core_unit"], json!(CORE_UNIT));
    assert!(value.get("panel_unit").is_some());
    assert_eq!(value["panel_unit"], Value::Null);
    assert_eq!(
        value["control_adapter"],
        json!(fixture.adapter.to_str().unwrap())
    );
    assert_eq!(value["state_dir"], json!(fixture.state.to_str().unwrap()));
    assert_eq!(value["timeout_seconds"], json!(30));
    assert_eq!(value["reversible_data"], json!(true));
    assert_eq!(
        value["deployment"]["store"],
        json!(fixture.store.to_str().unwrap())
    );

    assert_eq!(
        fs::read(first.adapter_path()).unwrap(),
        fs::read(&fixture.adapter).unwrap()
    );
    assert_eq!(first.adapter_sha256(), context.control_adapter_sha256());
    assert_ne!(first.adapter_path(), fixture.adapter);
    assert_eq!(first.config_sha256(), context.mapped_config_sha256());
    assert_eq!(
        first.systemd_config_sha256(),
        context.systemd_config_sha256()
    );

    let pinned_mapped = first.pinned_execution().unwrap();
    assert_eq!(pinned_mapped.adapter, first.adapter_path());
    let pinned = first.pinned_systemd_execution().unwrap();
    assert_eq!(pinned.control_adapter, first.adapter_path());
    assert_eq!(pinned.unit_directory, fixture.units);
    assert_eq!(pinned.current_selector, fixture.selector);
    assert_eq!(pinned.core_unit, CORE_UNIT);
    assert_eq!(pinned.timeout_seconds, 30);
    assert_eq!(pinned.state_dir, fixture.state);
    assert_eq!(pinned.deployment.store, fixture.store.to_str().unwrap());
    // Construction verifies the pinned adapter but never runs systemctl.
    let _backend = first.create_transaction_backend().unwrap();

    // A crash between snapshot and evidence publish is repaired by an apply
    // retry; the deterministic evidence bytes are identical.
    remove_sealed(first.evidence_path());
    let repaired = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    assert_eq!(repaired.path(), first.path());
    assert_eq!(repaired.digest(), first.digest());
    assert_eq!(repaired.evidence_digest(), first.evidence_digest());
    assert_eq!(
        fs::read(repaired.systemd_config_path()).unwrap(),
        fixture.raw
    );

    // The symmetric crash (evidence exists, snapshot missing) is repaired by
    // recreating the deterministic activation snapshot; the evidence binds the
    // same snapshot digest.
    remove_sealed(repaired.path());
    let repaired_snapshot = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    assert_eq!(repaired_snapshot.path(), first.path());
    assert_eq!(repaired_snapshot.digest(), first.digest());
    assert_eq!(repaired_snapshot.evidence_digest(), first.evidence_digest());
}

#[test]
fn development_recovery_reuses_only_the_existing_snapshot() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();

    let recovery = fixture.open_recovery(&transaction);
    let loaded = recovery
        .load_snapshot_recovery(&fixture.systemd, &transaction)
        .unwrap();
    assert_eq!(loaded.path(), prepared.path());
    assert_eq!(loaded.digest(), prepared.digest());
    assert_eq!(loaded.evidence_digest(), prepared.evidence_digest());
    assert_eq!(
        loaded.systemd_config_sha256(),
        prepared.systemd_config_sha256()
    );
    assert_eq!(fs::read(loaded.systemd_config_path()).unwrap(), fixture.raw);
    let pinned = loaded.pinned_systemd_execution().unwrap();
    assert_eq!(pinned.control_adapter, loaded.adapter_path());
}

#[test]
fn signed_apply_and_recovery_work_without_a_live_trust_store() {
    let fixture = Fixture::signed();
    let auth = fixture.auth.as_ref().unwrap();
    let context =
        SystemdActivationContext::open_with_cgroup(&fixture.systemd, RELEASE, auth, UPDATE)
            .unwrap();
    let transaction = fixture.tx();
    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    let reloaded = context
        .load_snapshot(&fixture.systemd, &transaction, auth)
        .unwrap();
    assert_eq!(reloaded.digest(), prepared.digest());

    // Recovery is anchored in the durable transaction hash and the existing
    // snapshot, not in a trust store or a development flag.
    fs::remove_dir_all(fixture.trust.as_ref().unwrap()).unwrap();
    let recovery = fixture.open_recovery(&transaction);
    let loaded = recovery
        .load_snapshot_recovery(&fixture.systemd, &transaction)
        .unwrap();
    assert_eq!(loaded.digest(), prepared.digest());
    assert_eq!(loaded.evidence_digest(), prepared.evidence_digest());
}

#[test]
fn recovery_rejects_drift_in_unmapped_and_mapped_fields() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    context
        .prepare_snapshot(&fixture.systemd, &transaction, None)
        .unwrap();
    let other_units = fixture.root.join("other-units");
    let other_selector = fixture.state.join("other-current");
    assert_recovery_drift(
        &fixture,
        &transaction,
        "unit_directory",
        |systemd| systemd.unit_directory = other_units,
        "full systemd config",
    );
    assert_recovery_drift(
        &fixture,
        &transaction,
        "current_selector",
        |systemd| systemd.current_selector = other_selector,
        "full systemd config",
    );
    assert_recovery_drift(
        &fixture,
        &transaction,
        "timeout_seconds",
        |systemd| systemd.timeout_seconds = 31,
        "snapshot",
    );
    write_file(&fixture.adapter, b"#!/bin/sh\nexit 1\n", 0o700);
    assert_recovery_drift(
        &fixture,
        &transaction,
        "control_adapter_bytes",
        |_| {},
        "adapter",
    );
}

#[test]
fn recovery_never_creates_a_missing_snapshot_or_evidence() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let recovery = fixture.open_recovery(&transaction);

    let before = state_entries(&fixture.state);
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(
        error.contains("missing or unsafe activation snapshot"),
        "{error}"
    );
    assert_eq!(state_entries(&fixture.state), before);

    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, None)
        .unwrap();
    remove_sealed(prepared.evidence_path());
    let before = state_entries(&fixture.state);
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(error.to_lowercase().contains("evidence"), "{error}");
    assert_eq!(state_entries(&fixture.state), before);

    remove_sealed(prepared.path());
    let before = state_entries(&fixture.state);
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(
        error.contains("missing or unsafe activation snapshot"),
        "{error}"
    );
    assert_eq!(state_entries(&fixture.state), before);
}

#[test]
fn tampered_or_missing_evidence_is_rejected() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    let recovery = fixture.open_recovery(&transaction);

    // A syntactically valid but different full config is caught by its hash.
    let mut value: Value = serde_json::from_slice(&fixture.raw).unwrap();
    value["unit_directory"] = json!(fixture.root.join("tampered-units").to_str().unwrap());
    fs::set_permissions(prepared.evidence_path(), fs::Permissions::from_mode(0o700)).unwrap();
    overwrite_file(
        &prepared.systemd_config_path(),
        &serde_json::to_vec_pretty(&value).unwrap(),
    );
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(
        error.contains("pinned systemd config was modified"),
        "{error}"
    );

    // A modified binding document is rejected before any hash is trusted.
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    let recovery = fixture.open_recovery(&transaction);
    let mut evidence: Value =
        serde_json::from_slice(&fs::read(prepared.evidence_file_path()).unwrap()).unwrap();
    evidence["snapshot_digest"] = json!("d".repeat(64));
    fs::set_permissions(prepared.evidence_path(), fs::Permissions::from_mode(0o700)).unwrap();
    overwrite_file(
        &prepared.evidence_file_path(),
        &serde_json::to_vec(&evidence).unwrap(),
    );
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(error.contains("systemd snapshot evidence"), "{error}");

    // A missing full-config file is an error, never a reason to recreate it.
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let prepared = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    let recovery = fixture.open_recovery(&transaction);
    fs::set_permissions(prepared.evidence_path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(prepared.systemd_config_path()).unwrap();
    let before = state_entries(&fixture.state);
    let error = format!(
        "{:#}",
        recovery
            .load_snapshot_recovery(&fixture.systemd, &transaction)
            .unwrap_err()
    );
    assert!(error.contains(SYSTEMD_CONFIG_FILE), "{error}");
    assert_eq!(state_entries(&fixture.state), before);
}

#[test]
fn original_raw_config_bytes_are_pinned() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    let snapshot = context
        .prepare_snapshot(&fixture.systemd, &transaction, Some(&fixture.raw))
        .unwrap();
    assert_eq!(
        fs::read(snapshot.systemd_config_path()).unwrap(),
        fixture.raw
    );
    assert_eq!(snapshot.systemd_config_file_sha256(), hash(&fixture.raw));

    let compact = serde_json::to_vec(&fixture.systemd).unwrap();
    assert_ne!(compact, fixture.raw);
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &transaction, Some(&compact))
            .unwrap_err()
    );
    assert!(
        error.contains("raw systemd config differs from the pinned snapshot"),
        "{error}"
    );

    // Omitting raw bytes keeps the existing evidence and its pinned bytes.
    let again = context
        .prepare_snapshot(&fixture.systemd, &transaction, None)
        .unwrap();
    assert_eq!(again.evidence_digest(), snapshot.evidence_digest());
    assert_eq!(fs::read(again.systemd_config_path()).unwrap(), fixture.raw);

    let mut value: Value = serde_json::from_slice(&fixture.raw).unwrap();
    value["timeout_seconds"] = json!(31);
    let different = serde_json::to_vec(&value).unwrap();
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &transaction, Some(&different))
            .unwrap_err()
    );
    assert!(error.contains("does not match"), "{error}");
}

#[test]
fn state_store_ancestry_and_adapter_boundaries_are_enforced() {
    let fixture = Fixture::development();
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.open_err().contains("private"));

    let fixture = Fixture::development();
    fs::set_permissions(&fixture.store, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.open_err().contains("private"));

    let fixture = Fixture::development();
    let in_store = fixture.store.join("hook");
    write_file(&in_store, b"#!/bin/sh\nexit 0\n", 0o700);
    let mut systemd = fixture.systemd.clone();
    systemd.control_adapter = in_store;
    assert!(fixture.open_err_with(&systemd).contains("candidate store"));

    let fixture = Fixture::development();
    let in_state = fixture.state.join("hook");
    write_file(&in_state, b"#!/bin/sh\nexit 0\n", 0o700);
    let mut systemd = fixture.systemd.clone();
    systemd.control_adapter = in_state;
    assert!(fixture
        .open_err_with(&systemd)
        .contains("transaction state directory"));

    let fixture = Fixture::development();
    let link = fixture.root.join("bin/link-hook");
    symlink(&fixture.adapter, &link).unwrap();
    let mut systemd = fixture.systemd.clone();
    systemd.control_adapter = link;
    let error = fixture.open_err_with(&systemd);
    assert!(
        error.contains("control_adapter") || error.contains("trusted adapter"),
        "{error}"
    );

    let fixture = Fixture::development();
    let real_state = fixture.root.join("real-state");
    fs::create_dir(&real_state).unwrap();
    fs::set_permissions(&real_state, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_state = fixture.root.join("linked-state");
    symlink(&real_state, &linked_state).unwrap();
    let mut systemd = fixture.systemd.clone();
    systemd.state_dir = linked_state.clone();
    systemd.current_selector = linked_state.join("current");
    let error = fixture.open_err_with(&systemd);
    assert!(
        error.contains("state directory") || error.contains("symlink"),
        "{error}"
    );

    let fixture = Fixture::development();
    let linked_store = fixture.root.join("linked-store");
    symlink(&fixture.store, &linked_store).unwrap();
    let mut systemd = fixture.systemd.clone();
    systemd.deployment.store = linked_store.to_str().unwrap().into();
    let error = fixture.open_err_with(&systemd);
    assert!(
        error.contains("candidate store") || error.contains("symlink"),
        "{error}"
    );
}

#[test]
fn apply_rejects_an_adapter_changed_after_authorization() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    write_file(&fixture.adapter, b"#!/bin/sh\nexit 1\n", 0o700);
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &transaction, None)
            .unwrap_err()
    );
    assert!(error.contains("trusted adapter changed"), "{error}");
}

#[test]
fn transaction_and_manifest_binding_is_enforced() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();

    let mut wrong_manifest = transaction.clone();
    wrong_manifest.manifest_sha256 = "c".repeat(64);
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &wrong_manifest, None)
            .unwrap_err()
    );
    assert!(error.contains("manifest"), "{error}");

    let mut wrong_release = transaction.clone();
    wrong_release.target_release = "rel-2".into();
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &wrong_release, None)
            .unwrap_err()
    );
    assert!(error.contains("release"), "{error}");

    let mut wrong_instance = transaction.clone();
    wrong_instance.instance_id = "other-instance".into();
    let error = format!(
        "{:#}",
        context
            .prepare_snapshot(&fixture.systemd, &wrong_instance, None)
            .unwrap_err()
    );
    assert!(error.contains("instance"), "{error}");

    let error = format!(
        "{:#}",
        SystemdActivationContext::open_recovery_with_cgroup(
            &fixture.systemd,
            &wrong_manifest,
            UPDATE
        )
        .unwrap_err()
    );
    assert!(error.contains("manifest"), "{error}");
}

#[test]
fn candidate_tamper_blocks_recovery_without_a_trust_store() {
    let fixture = Fixture::development();
    let context = fixture.open();
    let transaction = fixture.tx();
    context
        .prepare_snapshot(&fixture.systemd, &transaction, None)
        .unwrap();
    write_file(
        &fixture.store.join(RELEASE).join("bin/echo-agent-core"),
        b"tampered-core",
        0o755,
    );
    let error = format!(
        "{:#}",
        SystemdActivationContext::open_recovery_with_cgroup(&fixture.systemd, &transaction, UPDATE)
            .unwrap_err()
    );
    assert!(
        error.contains("candidate") || error.contains("verify") || error.contains("digest"),
        "{error}"
    );
}
