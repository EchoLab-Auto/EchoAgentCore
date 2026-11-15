//! Packaged manifest provenance and deployment planning, with opt-in transaction backends.
#[cfg(not(target_os = "linux"))]
compile_error!("echo-deploy v1 requires Linux descriptor-relative filesystem operations");
pub mod activation;
pub mod adapter;
pub mod compose;
pub mod configuration;
pub mod control_client;
pub mod execution;
pub mod install;
pub mod installation;
pub mod migration;
pub mod model;
pub mod operations;
pub mod pack;
pub mod request;
pub mod runner;
pub mod runtime_adapter;
pub mod safe_fs;
pub mod state_lock;
pub mod systemd;
pub mod systemd_activation;
pub mod systemd_transaction;
pub mod transaction;
pub mod trust;

use anyhow::{ensure, Context, Result};
use model::{Deployment, Release};
use safe_fs::{Dir, Lock};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct VerifiedBundle {
    root: Dir,
    release: Release,
    manifest: Vec<u8>,
    manifest_sha256: String,
}

impl VerifiedBundle {
    pub fn release(&self) -> &Release {
        &self.release
    }
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
    pub fn authenticate(
        &self,
        trust: &trust::TrustStore,
        key_id: &str,
        signature: &Path,
    ) -> Result<trust::VerifiedManifest> {
        trust.verify_manifest(key_id, &self.manifest, signature)
    }
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub schema_version: u32,
    pub instance_id: String,
    pub release_id: String,
    pub manifest_sha256: String,
    pub topology: model::Topology,
    pub scope: model::Scope,
    pub management_protocol: String,
    pub candidate_destination: PathBuf,
    pub artifact_count: usize,
    pub total_bytes: u64,
    pub activation_supported: bool,
    pub pending_activation_requirements: Vec<String>,
}

pub fn json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    model::absolute_path(path)?;
    let parent = Dir::absolute(path.parent().context("no parent")?)?;
    let leaf = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid filename")?;
    Ok(serde_json::from_slice(&safe_fs::read_json_bytes(
        parent.open_file(leaf, false)?,
    )?)?)
}

fn copy_verified(
    mut input: File,
    mut output: Option<&mut File>,
    size: u64,
    expected: &str,
) -> Result<()> {
    ensure!(input.metadata()?.len() == size, "artifact size mismatch");
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).context("size overflow")?;
        ensure!(count <= size, "artifact grew while reading");
        hash.update(&buffer[..n]);
        if let Some(file) = output.as_mut() {
            file.write_all(&buffer[..n])?;
        }
    }
    ensure!(
        count == size && format!("{:x}", hash.finalize()) == expected,
        "artifact digest/size mismatch"
    );
    Ok(())
}

pub fn verify(bundle: &Path) -> Result<VerifiedBundle> {
    let root = Dir::absolute(bundle)?;
    let manifest = safe_fs::read_json_bytes(root.open_file("release.json", false)?)?;
    let release: Release = serde_json::from_slice(&manifest)?;
    release.validate()?;
    for artifact in &release.artifacts {
        copy_verified(
            root.relative_file(&artifact.path)?,
            None,
            artifact.size,
            &artifact.sha256,
        )
        .with_context(|| format!("verify {}", artifact.id))?;
    }
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest));
    Ok(VerifiedBundle {
        root,
        release,
        manifest,
        manifest_sha256,
    })
}

pub fn plan(bundle: &VerifiedBundle, deployment: &Deployment) -> Result<Plan> {
    deployment.check_release(&bundle.release)?;
    Ok(Plan {
        schema_version: 1,
        instance_id: deployment.instance_id.clone(),
        release_id: bundle.release.release_id.clone(),
        manifest_sha256: bundle.manifest_sha256.clone(),
        topology: deployment.topology,
        scope: deployment.scope,
        management_protocol: bundle.release.management_protocol.clone(),
        candidate_destination: Path::new(&deployment.store).join(&bundle.release.release_id),
        artifact_count: bundle.release.artifacts.len(),
        total_bytes: bundle.release.artifacts.iter().map(|a| a.size).sum(),
        activation_supported: false,
        pending_activation_requirements: vec![
            "trusted release signatures (development staging is not authentication)".into(),
            "durable activation/recovery transaction executor".into(),
            "Core drain and checked-save acknowledgement".into(),
            "Core/Panel/browser coordinated maintenance gate".into(),
            format!(
                "application readiness, including required modules: {:?}",
                deployment.required_modules
            ),
            "data/config rollback compatibility".into(),
        ],
    })
}

/// Creates only the final component. Parent must exist and be trusted by the caller.
pub fn init_store(path: &Path) -> Result<()> {
    model::absolute_path(path)?;
    let parent = Dir::absolute(path.parent().context("missing parent")?)?;
    let leaf = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid store name")?;
    model::safe_id(leaf)?;
    let store = parent.mkdir(leaf)?;
    store.write_new("store-format", b"echo-deploy-candidates-v1\n", 0o600)?;
    store.write_new("store-lock", b"", 0o600)?;
    store.sync()?;
    parent.sync()?;
    Ok(())
}

fn descend(root: &Dir, parts: &[&str]) -> Result<Dir> {
    let mut dir = Dir(root.0.try_clone()?);
    for part in parts {
        dir = dir.child(part)?;
    }
    Ok(dir)
}

/// Does not install/activate anything. Pending directories are deliberately retained on
/// failure/crash: no recursive cleanup of potentially exchanged paths is attempted.
pub fn stage(
    bundle: &VerifiedBundle,
    deployment: &Deployment,
    allow_development: bool,
) -> Result<Plan> {
    ensure!(
        bundle.release.development && allow_development,
        "unsigned bundles require development=true and --allow-development"
    );
    stage_inner(bundle, deployment)
}

/// Signed bytes must be exactly those frozen by verify; publisher authorization is
/// provided by an explicit trust-store key, never by a key from the bundle.
pub fn stage_authenticated(
    bundle: &VerifiedBundle,
    deployment: &Deployment,
    authentication: &trust::VerifiedManifest,
) -> Result<Plan> {
    ensure!(
        authentication.bytes() == bundle.manifest.as_slice(),
        "signature does not cover frozen manifest"
    );
    stage_inner(bundle, deployment)
}

fn stage_inner(bundle: &VerifiedBundle, deployment: &Deployment) -> Result<Plan> {
    let plan = plan(bundle, deployment)?;
    let store = Dir::absolute(Path::new(&deployment.store))?;
    safe_fs::private_store(&store)?;
    let _lock = Lock::acquire(&store)?;
    // Revalidate every source before creating pending output; copy rehashes as well.
    for a in &bundle.release.artifacts {
        copy_verified(bundle.root.relative_file(&a.path)?, None, a.size, &a.sha256)?;
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let pending_name = format!("pending-{}-{nonce}", std::process::id());
    let pending = store.mkdir(&pending_name)?;
    let mut directories = BTreeSet::new();
    for a in &bundle.release.artifacts {
        let parts: Vec<_> = a.path.split('/').collect();
        for i in 1..parts.len() {
            let key = parts[..i].join("/");
            if directories.insert(key) {
                descend(&pending, &parts[..i - 1])?.mkdir(parts[i - 1])?;
            }
        }
        let dir = descend(&pending, &parts[..parts.len() - 1])?;
        let mut output = dir.create(parts[parts.len() - 1])?;
        copy_verified(
            bundle.root.relative_file(&a.path)?,
            Some(&mut output),
            a.size,
            &a.sha256,
        )?;
        output.set_permissions(std::fs::Permissions::from_mode(if a.executable {
            0o555
        } else {
            0o444
        }))?;
        output.sync_all()?;
    }
    pending.write_new("release.json", &bundle.manifest, 0o444)?;
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|p| std::cmp::Reverse(p.split('/').count()));
    for directory in directories {
        descend(&pending, &directory.split('/').collect::<Vec<_>>())?.seal()?;
    }
    pending.seal()?;
    store.publish(&pending_name, &bundle.release.release_id)?;
    Ok(plan)
}
