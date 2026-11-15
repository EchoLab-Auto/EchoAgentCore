//! Release assembly from explicit build outputs. No compilation, download or source mutation.
use crate::{
    model::{Artifact, Kind, Release},
    safe_fs::Dir,
    verify,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOutput {
    pub kind: Kind,
    pub source_id: String,
    /// Binary file for core/panel/module; directory for web, expanded without ignore rules.
    pub input: PathBuf,
}

/// Inputs retain the source provenance from the supplied release template. Artifacts
/// are generated from actual bytes; an existing artifacts list must be empty.
pub fn assemble(template: Release, inputs: &[BuildOutput], output: &Path) -> Result<Release> {
    ensure!(
        template.artifacts.is_empty(),
        "pack template artifacts must be empty"
    );
    ensure!(!inputs.is_empty(), "no build outputs");
    crate::model::absolute_path(output)?;
    let parent = Dir::absolute(output.parent().context("missing output parent")?)?;
    let name = output
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid output name")?;
    crate::model::safe_id(name)?;
    // Gather all files before creating output. Complete web traversal, no silent ignores.
    let mut files = Vec::new();
    let mut kinds = BTreeSet::new();
    for input in inputs {
        crate::model::absolute_path(&input.input)?;
        crate::model::safe_id(&input.source_id)?;
        match input.kind {
            Kind::Web => {
                ensure!(kinds.insert("web"), "duplicate web output");
                Dir::absolute(&input.input)?;
                walk_web(&input.input, &input.input, &input.source_id, &mut files)?;
            }
            kind => {
                let name = match kind {
                    Kind::Core => "bin/echo-agent-core",
                    Kind::Panel => "bin/echo-agent-panel",
                    Kind::Module => {
                        anyhow::bail!("module pack requires a module protocol contract")
                    }
                    Kind::Web => unreachable!(),
                };
                ensure!(kinds.insert(name), "duplicate binary output");
                files.push((
                    input.input.clone(),
                    name.to_string(),
                    kind,
                    input.source_id.clone(),
                ));
            }
        }
    }
    let mut release = template;
    for (_, path, kind, source) in &files {
        crate::model::relative_file(path)?;
        release.artifacts.push(Artifact {
            id: format!("file-{}", release.artifacts.len()),
            kind: *kind,
            path: path.clone(),
            sha256: "0".repeat(64),
            size: 1,
            executable: *kind != Kind::Web,
            source_id: source.clone(),
        });
    }
    release.validate()?;
    let temporary_name = format!(
        "pack-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let target = parent.mkdir(&temporary_name)?;
    let mut created = BTreeSet::new();
    for (index, (input, path, _, _)) in files.iter().enumerate() {
        let srcdir = Dir::absolute(input.parent().context("source has no parent")?)?;
        let leaf = input
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid source filename")?;
        let mut source = srcdir.open_file(leaf, false)?;
        let parts: Vec<_> = path.split('/').collect();
        let mut dir = Dir(target.0.try_clone()?);
        let mut prefix = String::new();
        for part in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            dir = if created.insert(prefix.clone()) {
                dir.mkdir(part)?
            } else {
                dir.child(part)?
            };
        }
        let mut dest = dir.create(parts[parts.len() - 1])?;
        let mut hash = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = [0; 65536];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes = bytes.checked_add(n as u64).context("artifact overflow")?;
            ensure!(bytes <= 32 * 1024 * 1024 * 1024_u64, "artifact too large");
            hash.update(&buffer[..n]);
            dest.write_all(&buffer[..n])?;
        }
        release.artifacts[index].sha256 = format!("{:x}", hash.finalize());
        release.artifacts[index].size = bytes;
        dest.set_permissions(fs::Permissions::from_mode(
            if release.artifacts[index].executable {
                0o555
            } else {
                0o444
            },
        ))?;
        dest.sync_all()?;
        dir.sync()?;
    }
    release.validate()?;
    target.write_new("release.json", &serde_json::to_vec_pretty(&release)?, 0o444)?;
    target.sync()?;
    // Verify copied bytes before publishing. The output parent must be trusted.
    verify(&output.parent().unwrap().join(&temporary_name))?;
    require_complete(&output.parent().unwrap().join(&temporary_name), &release)?;
    parent.publish(&temporary_name, name)?;
    Ok(release)
}

fn walk_web(
    root: &Path,
    dir: &Path,
    source: &str,
    out: &mut Vec<(PathBuf, String, Kind, String)>,
) -> Result<()> {
    ensure!(out.len() <= 10000, "too many web files");
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("non UTF-8 web path")?
            .to_string();
        crate::model::relative_file(&format!("web/{relative}"))?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "web tree contains symlink"
        );
        if metadata.is_dir() {
            walk_web(root, &path, source, out)?;
        } else {
            ensure!(metadata.is_file(), "web tree contains special file");
            out.push((path, format!("web/{relative}"), Kind::Web, source.into()));
        }
    }
    Ok(())
}

/// Completeness check is optional for manually authored input bundles; generated packs
/// always enumerate the entire web tree. This check rejects extra files/links.
pub fn require_complete(bundle: &Path, release: &Release) -> Result<()> {
    fn visit(root: &Path, dir: &Path, actual: &mut BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let p = entry.path();
            let m = fs::symlink_metadata(&p)?;
            ensure!(!m.file_type().is_symlink(), "unexpected symlink in bundle");
            if m.is_dir() {
                visit(root, &p, actual)?;
            } else {
                ensure!(m.is_file(), "special file in bundle");
                actual.insert(
                    p.strip_prefix(root)?
                        .to_str()
                        .context("non UTF-8 bundle path")?
                        .into(),
                );
            }
        }
        Ok(())
    }
    let mut actual = BTreeSet::new();
    visit(bundle, bundle, &mut actual)?;
    let expected: BTreeSet<String> = release
        .artifacts
        .iter()
        .map(|a| a.path.clone())
        .chain(["release.json".to_string()])
        .collect();
    ensure!(
        actual == expected,
        "bundle includes missing or undeclared files"
    );
    Ok(())
}
