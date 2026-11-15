//! Versioned, strict input contracts. No defaults for security-relevant fields.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub schema_version: u32,
    pub release_id: String,
    pub target_os: String,
    pub target_arch: String,
    pub management_protocol: String,
    pub development: bool,
    pub sources: Vec<Source>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    pub revision: String,
    pub snapshot_sha256: String,
    pub lock_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub kind: Kind,
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
    pub source_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Core,
    Panel,
    Web,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    CoreOnly,
    Embedded,
    Standalone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub schema_version: u32,
    pub instance_id: String,
    pub topology: Topology,
    pub scope: Scope,
    pub target_os: String,
    pub target_arch: String,
    /// Explicit, dedicated candidate store, NOT a configuration/data directory.
    pub store: String,
    pub required_modules: Vec<String>,
}

pub fn safe_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 100,
        "invalid identifier length"
    );
    ensure!(
        value.as_bytes()[0].is_ascii_alphanumeric(),
        "identifier must start with alphanumeric"
    );
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "unsafe identifier: {value:?}"
    );
    Ok(())
}

pub fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected lowercase SHA256"
    );
    Ok(())
}

pub fn relative_file(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 4096 && !value.is_empty(),
        "invalid path length"
    );
    ensure!(value.split('/').count() <= 64, "artifact path too deep");
    for part in value.split('/') {
        safe_id(part)?;
    }
    ensure!(value != "release.json", "release.json is reserved");
    Ok(())
}

pub fn absolute_path(path: &Path) -> Result<()> {
    use std::path::Component;
    ensure!(
        path.is_absolute() && path.parent().is_some(),
        "expected absolute non-root path"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "non-normal absolute path"
    );
    // Path::components normalizes interior '.', so reject its lexical spelling too.
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path must be UTF-8"))?;
    ensure!(
        !text.split('/').any(|s| s == "." || s == ".."),
        "non-normal absolute path"
    );
    Ok(())
}

impl Release {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported release schema");
        safe_id(&self.release_id)?;
        ensure!(
            self.release_id != "store-format"
                && self.release_id != "store-lock"
                && !self.release_id.starts_with("pending-"),
            "reserved release_id"
        );
        safe_id(&self.management_protocol)?;
        ensure!(self.target_os == "linux", "only linux releases supported");
        ensure!(
            ["x86_64", "aarch64"].contains(&self.target_arch.as_str()),
            "unsupported architecture"
        );
        ensure!(
            !self.sources.is_empty() && self.sources.len() <= 256,
            "invalid sources count"
        );
        ensure!(
            !self.artifacts.is_empty() && self.artifacts.len() <= 10000,
            "invalid artifacts count"
        );
        let mut sources = BTreeSet::new();
        for source in &self.sources {
            safe_id(&source.id)?;
            ensure!(sources.insert(&source.id), "duplicate source");
            ensure!(
                !source.revision.is_empty()
                    && source.revision.len() <= 256
                    && !source.revision.chars().any(char::is_control),
                "invalid revision"
            );
            digest(&source.snapshot_sha256)?;
            digest(&source.lock_sha256)?;
        }
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut total = 0_u64;
        for artifact in &self.artifacts {
            safe_id(&artifact.id)?;
            relative_file(&artifact.path)?;
            digest(&artifact.sha256)?;
            ensure!(
                ids.insert(&artifact.id) && paths.insert(artifact.path.as_str()),
                "duplicate artifact id/path"
            );
            ensure!(sources.contains(&artifact.source_id), "unknown source_id");
            ensure!(
                artifact.executable == (artifact.kind != Kind::Web),
                "invalid executable flag"
            );
            ensure!(
                artifact.size > 0 || artifact.kind == Kind::Web,
                "empty binary"
            );
            total = total
                .checked_add(artifact.size)
                .ok_or_else(|| anyhow::anyhow!("size overflow"))?;
        }
        ensure!(
            total <= 32 * 1024 * 1024 * 1024_u64,
            "bundle exceeds 32GiB limit"
        );
        for path in &paths {
            let mut parent = Path::new(path).parent();
            while let Some(p) = parent {
                if let Some(s) = p.to_str() {
                    ensure!(!paths.contains(s), "artifact file/directory conflict");
                }
                parent = p.parent();
            }
            ensure!(!path.starts_with("release.json/"), "reserved manifest path");
        }
        Ok(())
    }
}

impl Deployment {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported deployment schema");
        safe_id(&self.instance_id)?;
        ensure!(self.target_os == "linux", "unsupported deployment OS");
        ensure!(
            self.target_os == std::env::consts::OS && self.target_arch == std::env::consts::ARCH,
            "deployment does not target this host"
        );
        absolute_path(Path::new(&self.store))?;
        let mut modules = BTreeSet::new();
        for module in &self.required_modules {
            safe_id(module)?;
            ensure!(modules.insert(module), "duplicate required module");
        }
        Ok(())
    }

    pub fn check_release(&self, release: &Release) -> Result<()> {
        self.validate()?;
        release.validate()?;
        ensure!(
            release.target_os == self.target_os && release.target_arch == self.target_arch,
            "release target mismatch"
        );
        let count = |kind| release.artifacts.iter().filter(|a| a.kind == kind).count();
        ensure!(count(Kind::Core) == 1, "exactly one Core required");
        match self.topology {
            Topology::CoreOnly if count(Kind::Panel) == 0 && count(Kind::Web) == 0 => {}
            Topology::Embedded if count(Kind::Panel) == 0 && count(Kind::Web) > 0 => {}
            Topology::Standalone if count(Kind::Panel) == 1 && count(Kind::Web) > 0 => {}
            _ => bail!("artifact set does not match deployment topology"),
        }
        // v1 has no per-module protocol/capability contract yet. Do not imply support.
        ensure!(
            count(Kind::Module) == 0,
            "module bundles require a future protocol contract"
        );
        if self.topology != Topology::CoreOnly {
            ensure!(
                release
                    .artifacts
                    .iter()
                    .any(|a| a.kind == Kind::Web && a.path == "web/index.html"),
                "web/index.html required"
            );
        }
        Ok(())
    }
}
