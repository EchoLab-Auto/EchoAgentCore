//! End-to-end signed candidate staging through the real CLI.
//!
//! Every Ed25519 key material file below is generated on the fly inside a
//! `tempfile::TempDir`. These keys are test fixtures only: they are never
//! credentials, never installed, and are deleted with the temporary directory.

use echo_deploy::{
    model::{Deployment, Kind, Release, Scope, Source, Topology},
    pack::{assemble, require_complete, BuildOutput},
    trust::{self, TrustStore},
    verify,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{tempdir, TempDir};

const KEY_ID: &str = "release-1";
const RELEASE_ID: &str = "signed-release-1";
const DIFFERENT_RELEASE_ID: &str = "different-release-1";

struct Fixture {
    temp: TempDir,
    bundle: PathBuf,
    store: PathBuf,
    deployment_path: PathBuf,
    execution_path: PathBuf,
    adapter_marker: PathBuf,
    trust: PathBuf,
    signature: PathBuf,
    build_binary: PathBuf,
    manifest: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().expect("create ephemeral test root");
        let root = temp.path().to_path_buf();
        assert_exact_path(&root, "ephemeral test root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

        // The trust directory, the ephemeral signing key, the signature and the
        // candidate store are deliberately separate private directories.
        let keys = root.join("keys");
        fs::create_dir(&keys).unwrap();
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o700)).unwrap();
        assert_exact_path(&keys, "ephemeral key directory");
        assert_eq!(mode(&keys), 0o700);

        let trust_dir = root.join("trust");
        fs::create_dir(&trust_dir).unwrap();
        fs::set_permissions(&trust_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_exact_path(&trust_dir, "trust directory");
        assert_eq!(mode(&trust_dir), 0o700);
        let marker = trust_dir.join("trust-format");
        fs::write(&marker, trust::TRUST_FORMAT).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        assert_exact_path(&marker, "trust-format marker");

        assert_exact_path(Path::new(trust::OPENSSL), "fixed openssl binary");
        let private_key = keys.join("ephemeral-release-signing.pem");
        openssl_genpkey(&private_key);
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        assert_exact_path(&private_key, "ephemeral signing key");
        let public_key = trust_dir.join(format!("{KEY_ID}.ed25519.pem"));
        openssl_pubout(&private_key, &public_key);
        fs::set_permissions(&public_key, fs::Permissions::from_mode(0o600)).unwrap();
        assert_exact_path(&public_key, "ephemeral public key");
        TrustStore::open(&trust_dir).expect("trust directory is well formed");

        let build = root.join("build");
        fs::create_dir(&build).unwrap();
        fs::set_permissions(&build, fs::Permissions::from_mode(0o700)).unwrap();
        let built_binary = PathBuf::from(env!("CARGO_BIN_EXE_echo-deploy"));
        assert_exact_path(&built_binary, "built echo-deploy binary");
        let binary = build.join("echo-agent-core");
        let copied = fs::copy(&built_binary, &binary).expect("copy the actual built binary");
        assert!(copied > 0, "pack input binary must not be empty");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        assert_exact_path(&binary, "pack input binary");

        let bundle = root.join("bundle");
        let release = assemble(
            template(),
            &[BuildOutput {
                kind: Kind::Core,
                input: binary.clone(),
                source_id: "core".into(),
            }],
            &bundle,
        )
        .expect("pack a production (development=false) bundle");
        assert!(
            !release.development,
            "signed staging fixture must be a production release"
        );
        assert_eq!(release.artifacts.len(), 1);
        assert_eq!(release.artifacts[0].path, "bin/echo-agent-core");
        assert!(release.artifacts[0].executable);
        assert_exact_path(&bundle, "packed bundle");
        let manifest_path = bundle.join("release.json");
        assert_exact_path(&manifest_path, "signed manifest");
        let manifest = fs::read(&manifest_path).unwrap();
        let verified = verify(&bundle).expect("packed bundle verifies");
        assert_eq!(verified.release().release_id, RELEASE_ID);
        assert!(!verified.release().development);
        require_complete(&bundle, &release).expect("packed bundle has no extra files");

        let store = root.join("candidates");
        echo_deploy::init_store(&store).expect("create private candidate store");
        assert_exact_path(&store, "candidate store");
        assert_eq!(mode(&store), 0o700);

        let deployment = Deployment {
            schema_version: 1,
            instance_id: "signed-stage-test".into(),
            topology: Topology::CoreOnly,
            scope: Scope::User,
            target_os: "linux".into(),
            target_arch: std::env::consts::ARCH.into(),
            store: store.to_str().expect("UTF-8 store path").into(),
            required_modules: vec![],
        };
        deployment.validate().expect("deployment contract");
        let deployment_path = root.join("deployment.json");
        fs::write(&deployment_path, serde_json::to_vec(&deployment).unwrap()).unwrap();
        assert_exact_path(&deployment_path, "deployment contract");

        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(mode(&state), 0o700);
        let adapter_marker = root.join("adapter-was-invoked");
        let adapter = root.join("adapter");
        let script = format!(
            "#!/bin/sh\n: > {:?}\nexit 0\n",
            adapter_marker.to_str().expect("UTF-8 marker path")
        );
        fs::write(&adapter, script).unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        let execution_path = root.join("execution.json");
        fs::write(
            &execution_path,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "deployment": deployment.clone(),
                "state_dir": state,
                "adapter": adapter,
                "timeout_seconds": 3,
                "reversible_data": true
            }))
            .unwrap(),
        )
        .unwrap();
        assert_exact_path(&execution_path, "execution contract");

        let signing = root.join("signing");
        fs::create_dir(&signing).unwrap();
        fs::set_permissions(&signing, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(mode(&signing), 0o700);
        let signing_input = signing.join("release.json");
        fs::write(&signing_input, &manifest).unwrap();
        fs::set_permissions(&signing_input, fs::Permissions::from_mode(0o600)).unwrap();
        let signature = signing.join("release.sig");
        openssl_sign(&private_key, &signing_input, &signature);
        fs::set_permissions(&signature, fs::Permissions::from_mode(0o600)).unwrap();
        assert_exact_path(&signature, "detached test signature");
        assert_eq!(
            fs::metadata(&signature).unwrap().len() as usize,
            trust::ED25519_SIGNATURE_BYTES
        );

        Self {
            temp,
            bundle,
            store,
            deployment_path,
            execution_path,
            adapter_marker,
            trust: trust_dir,
            signature,
            build_binary: binary,
            manifest,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Staged candidates are intentionally read-only (0555/0444). Restore
        // write access only inside this test-owned temporary tree, never
        // following symlinks, so TempDir cleanup cannot escape or fail.
        unlock_for_cleanup(self.temp.path());
    }
}

fn template() -> Release {
    Release {
        schema_version: 1,
        release_id: RELEASE_ID.into(),
        target_os: "linux".into(),
        target_arch: std::env::consts::ARCH.into(),
        management_protocol: "management-contract-1".into(),
        development: false,
        sources: vec![Source {
            id: "core".into(),
            revision: "signed-stage-test".into(),
            snapshot_sha256: "a".repeat(64),
            lock_sha256: "b".repeat(64),
        }],
        artifacts: vec![],
    }
}

fn openssl_genpkey(path: &Path) {
    let mut command = Command::new(trust::OPENSSL);
    command
        .args(["genpkey", "-algorithm", "Ed25519", "-out"])
        .arg(path);
    run_openssl(&mut command);
}

fn openssl_pubout(private_key: &Path, public_key: &Path) {
    let mut command = Command::new(trust::OPENSSL);
    command
        .args(["pkey", "-in"])
        .arg(private_key)
        .args(["-pubout", "-out"])
        .arg(public_key);
    run_openssl(&mut command);
}

fn openssl_sign(private_key: &Path, input: &Path, signature: &Path) {
    let mut command = Command::new(trust::OPENSSL);
    command
        .args(["pkeyutl", "-sign", "-inkey"])
        .arg(private_key)
        .args(["-rawin", "-in"])
        .arg(input)
        .arg("-out")
        .arg(signature);
    run_openssl(&mut command);
}

fn run_openssl(command: &mut Command) {
    let output = command.output().expect("execute openssl");
    assert!(
        output.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_echo-deploy"))
}

fn trust_args(command: &mut Command, fixture: &Fixture) {
    command
        .arg("--trust-store")
        .arg(&fixture.trust)
        .arg("--key-id")
        .arg(KEY_ID)
        .arg("--signature")
        .arg(&fixture.signature);
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON stdout ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stage_signed(fixture: &Fixture) -> Output {
    let mut command = cli();
    command
        .arg("stage")
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--deployment")
        .arg(&fixture.deployment_path);
    trust_args(&mut command, fixture);
    let output = command.output().expect("execute signed stage");
    assert_success(&output);
    output
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn assert_exact_path(path: &Path, label: &str) {
    assert!(
        path.is_absolute(),
        "{label} must be an absolute path: {}",
        path.display()
    );
    assert!(
        !path.starts_with("/root"),
        "{label} must not live under /root: {}",
        path.display()
    );
    assert!(
        path.exists(),
        "{label} must exist exactly at {}",
        path.display()
    );
}

fn has_pending_candidate(path: &Path) -> bool {
    fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
                && entry.file_name().to_string_lossy().starts_with("pending-")
        })
}

fn unlock_for_cleanup(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    if metadata.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                unlock_for_cleanup(&entry.path());
            }
        }
    } else if metadata.is_file() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

#[test]
fn signed_pack_verifies_authenticated_and_stages_through_cli() {
    let fixture = Fixture::new();

    let mut verify_command = cli();
    verify_command
        .arg("verify")
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--require-complete");
    trust_args(&mut verify_command, &fixture);
    let verified = verify_command.output().expect("execute CLI verify");
    assert_success(&verified);
    let verified = json_output(&verified);
    assert_eq!(verified["integrity_verified"], true);
    assert_eq!(verified["publisher_authenticated"], true);
    assert_eq!(verified["release_id"], RELEASE_ID);
    assert_eq!(verified["development"], false);
    assert_eq!(verified["manifest_sha256"], sha256_hex(&fixture.manifest));

    let staged = stage_signed(&fixture);
    let staged = json_output(&staged);
    assert_eq!(staged["state"], "staged_not_activated");
    assert_eq!(staged["plan"]["release_id"], RELEASE_ID);
    assert_eq!(
        staged["plan"]["manifest_sha256"],
        sha256_hex(&fixture.manifest)
    );
    let candidate = fixture.store.join(RELEASE_ID);
    assert_eq!(
        staged["plan"]["candidate_destination"],
        candidate.to_str().unwrap()
    );

    assert_exact_path(&candidate, "staged final candidate");
    let staged_manifest = candidate.join("release.json");
    let artifact = candidate.join("bin/echo-agent-core");
    assert_exact_path(&staged_manifest, "staged manifest");
    assert_exact_path(&artifact, "staged artifact");
    assert_eq!(fs::read(&staged_manifest).unwrap(), fixture.manifest);
    assert_eq!(
        fs::read(&artifact).unwrap(),
        fs::read(&fixture.build_binary).unwrap()
    );
    assert_eq!(mode(&candidate), 0o555);
    assert_eq!(mode(&candidate.join("bin")), 0o555);
    assert_eq!(mode(&artifact), 0o555);
    assert_eq!(mode(&staged_manifest), 0o444);
    verify(&candidate).expect("the CLI staged candidate remains verifiable");
    assert!(
        !has_pending_candidate(&fixture.store),
        "staging must expose only the final candidate, with no pending leftovers"
    );

    // The candidate tree is deliberately read-only; fixture cleanup must still
    // remove it without touching anything outside the temporary root.
    let root = fixture.temp.path().to_path_buf();
    drop(fixture);
    assert!(
        !root.exists(),
        "read-only staged fixture cleanup must succeed"
    );
}

#[test]
fn tampered_signature_is_refused_and_no_final_candidate_is_written() {
    let fixture = Fixture::new();
    let mut signature = fs::read(&fixture.signature).unwrap();
    signature[0] ^= 0x01;
    fs::write(&fixture.signature, &signature).unwrap();
    fs::set_permissions(&fixture.signature, fs::Permissions::from_mode(0o600)).unwrap();

    // Package integrity is untouched; only authentication must fail.
    let plain = cli()
        .arg("verify")
        .arg("--bundle")
        .arg(&fixture.bundle)
        .output()
        .expect("execute unauthenticated verify");
    assert_success(&plain);
    assert_eq!(json_output(&plain)["publisher_authenticated"], false);

    let mut verified_command = cli();
    verified_command
        .arg("verify")
        .arg("--bundle")
        .arg(&fixture.bundle);
    trust_args(&mut verified_command, &fixture);
    let verified = verified_command
        .output()
        .expect("execute authenticated verify");
    assert!(
        !verified.status.success(),
        "tampered signature was accepted"
    );
    let stderr = String::from_utf8_lossy(&verified.stderr);
    assert!(
        stderr.contains("signature verification failed"),
        "unexpected verify error: {stderr}"
    );

    let mut stage_command = cli();
    stage_command
        .arg("stage")
        .arg("--bundle")
        .arg(&fixture.bundle)
        .arg("--deployment")
        .arg(&fixture.deployment_path);
    trust_args(&mut stage_command, &fixture);
    let staged = stage_command.output().expect("execute tampered stage");
    assert!(
        !staged.status.success(),
        "tampered signature stage succeeded"
    );
    let stderr = String::from_utf8_lossy(&staged.stderr);
    assert!(
        stderr.contains("signature verification failed"),
        "unexpected stage error: {stderr}"
    );
    assert!(
        !fixture.store.join(RELEASE_ID).exists(),
        "a rejected signature must not write the final candidate"
    );
    assert!(
        !has_pending_candidate(&fixture.store),
        "a rejected signature must not leave a pending candidate"
    );
}

#[test]
fn apply_requires_authorization_and_matching_candidate_directory() {
    let fixture = Fixture::new();
    stage_signed(&fixture);
    assert!(!fixture.adapter_marker.exists());

    let denied = cli()
        .arg("apply")
        .arg("--execution")
        .arg(&fixture.execution_path)
        .arg("--release")
        .arg(RELEASE_ID)
        .arg("--confirm")
        .output()
        .expect("execute unauthorized apply");
    assert!(
        !denied.status.success(),
        "apply without authorization succeeded"
    );
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains(
            "activation requires trusted signature or explicit development authorization"
        ),
        "unexpected apply error: {stderr}"
    );
    assert!(
        !fixture.adapter_marker.exists(),
        "the administrator adapter must not run for an unauthorized apply"
    );

    // --allow-development is not a substitute for a signature on a production release.
    let development = cli()
        .arg("apply")
        .arg("--execution")
        .arg(&fixture.execution_path)
        .arg("--release")
        .arg(RELEASE_ID)
        .arg("--allow-development")
        .arg("--confirm")
        .output()
        .expect("execute development-authorized apply");
    assert!(
        !development.status.success(),
        "development authorization must not activate development=false release"
    );
    let stderr = String::from_utf8_lossy(&development.stderr);
    assert!(
        stderr.contains(
            "activation requires trusted signature or explicit development authorization"
        ),
        "unexpected apply error: {stderr}"
    );
    assert!(!fixture.adapter_marker.exists());

    // The candidate directory name must match release.json's release_id.
    let staged = fixture.store.join(RELEASE_ID);
    let mismatched = fixture.store.join(DIFFERENT_RELEASE_ID);
    fs::rename(&staged, &mismatched).expect("rename staged candidate for mismatch case");
    assert_exact_path(&mismatched, "mismatched candidate directory");
    let mut mismatch = cli();
    mismatch
        .arg("apply")
        .arg("--execution")
        .arg(&fixture.execution_path)
        .arg("--release")
        .arg(DIFFERENT_RELEASE_ID)
        .arg("--confirm");
    trust_args(&mut mismatch, &fixture);
    let mismatch = mismatch.output().expect("execute mismatched apply");
    assert!(
        !mismatch.status.success(),
        "candidate directory / manifest release mismatch was accepted"
    );
    let stderr = String::from_utf8_lossy(&mismatch.stderr);
    assert!(
        stderr.contains("candidate directory and manifest release identity differ"),
        "unexpected apply error: {stderr}"
    );
    assert!(
        !fixture.adapter_marker.exists(),
        "mismatch must be rejected before activation"
    );
}

#[test]
fn signed_apply_recovers_offline_without_recreating_authorization() {
    let fixture = Fixture::new();
    stage_signed(&fixture);
    let root = fixture.temp.path();
    let actual = root.join("actual.json");
    fs::write(
        &actual,
        br#"{"current_release":null,"maintenance":false,"ready":false,"active_work":0}"#,
    )
    .unwrap();
    let config: echo_deploy::execution::Execution =
        echo_deploy::json_file(&fixture.execution_path).unwrap();
    // Administrator hook fixture only; no real Core or systemd is involved.
    fs::write(&config.adapter, format!(r#"#!/usr/bin/python3
import json,sys
p={}
r=json.load(sys.stdin)
s=json.load(open(p))
op=r['operation']
e={{}}
if op=='observe': e=s
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
"#, serde_json::to_string(&actual).unwrap())).unwrap();
    fs::set_permissions(&config.adapter, fs::Permissions::from_mode(0o700)).unwrap();
    let mut apply = cli();
    apply
        .args(["apply", "--release", RELEASE_ID, "--confirm"])
        .arg("--execution")
        .arg(&fixture.execution_path);
    trust_args(&mut apply, &fixture);
    assert_success(&apply.output().unwrap());

    // The production package cannot be authorized as development; recovery
    // instead reuses the already authorized, frozen snapshot.
    let unavailable = root.join("trust-unavailable");
    fs::rename(&fixture.trust, &unavailable).unwrap();
    let recover = || {
        cli()
            .args(["recover", "--confirm"])
            .arg("--execution")
            .arg(&fixture.execution_path)
            .output()
            .unwrap()
    };
    assert_success(&recover());
    let durable = echo_deploy::transaction::TransactionStore::open(&config.state_dir)
        .unwrap()
        .load(&config.deployment.instance_id)
        .unwrap()
        .unwrap();
    let context =
        echo_deploy::activation::ActivationContext::open_recovery(&config, &durable).unwrap();
    let snapshot = context.snapshot_path(&durable);
    let removed = config.state_dir.join("removed-snapshot");
    fs::rename(&snapshot, &removed).unwrap();
    assert!(
        !recover().status.success(),
        "recovery invented a missing snapshot"
    );
    assert!(!snapshot.exists());
    fs::rename(&removed, &snapshot).unwrap();
    fs::write(&config.adapter, b"#!/bin/sh\nexit 77\n").unwrap();
    assert!(
        !recover().status.success(),
        "recovery accepted adapter hash drift"
    );
}
