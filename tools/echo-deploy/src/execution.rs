//! Explicit administrator-owned execution adapter configuration. Never accepts shell text.
use crate::model::{absolute_path, safe_id, Deployment};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub schema_version: u32,
    pub deployment: Deployment,
    pub state_dir: PathBuf,
    /// Trusted executable implements the deployment hook protocol; not supplied by bundle.
    pub adapter: PathBuf,
    pub timeout_seconds: u64,
    /// Mandatory operator declaration until a migration planner exists.
    pub reversible_data: bool,
}
impl Execution {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported execution schema");
        self.deployment.validate()?;
        absolute_path(&self.state_dir)?;
        absolute_path(&self.adapter)?;
        ensure!(
            (1..=600).contains(&self.timeout_seconds),
            "adapter timeout outside 1..600s"
        );
        ensure!(
            self.reversible_data,
            "automatic rollback requires explicit reversible-data policy"
        );
        ensure!(
            self.state_dir != std::path::Path::new(&self.deployment.store),
            "state and candidate directories must differ"
        );
        Ok(())
    }
}

/// Each response must echo protocol, operation, transaction and instance, and return
/// success=true with structured evidence. Hooks are explicitly trusted administrator
/// code; this validates their acknowledgements, not their implementation truthfulness.
pub fn invoke(
    config: &Execution,
    operation: &str,
    transaction: &str,
    release: Option<&str>,
    expected_manifest: Option<&str>,
) -> Result<serde_json::Value> {
    config.validate()?;
    safe_id(operation)?;
    safe_id(transaction)?;
    if let Some(r) = release {
        safe_id(r)?;
    }
    let request = serde_json::json!({
        "protocol":"echo-deploy-hook-v1", "operation":operation,
        "transaction_id":transaction, "instance_id":config.deployment.instance_id,
        "release_id":release, "manifest_sha256":expected_manifest,
        "store":config.deployment.store, "scope":config.deployment.scope,
        "topology":config.deployment.topology,
        "required_modules":config.deployment.required_modules
    });
    let result = crate::runner::run_json(
        &config.adapter,
        &[],
        &request,
        Duration::from_secs(config.timeout_seconds),
    )?;
    for key in ["protocol", "operation", "transaction_id", "instance_id"] {
        ensure!(
            result.get(key) == request.get(key),
            "adapter acknowledgement context mismatch"
        );
    }
    ensure!(
        result.get("success").and_then(|v| v.as_bool()) == Some(true),
        "adapter rejected operation"
    );
    let evidence = result
        .get("evidence")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow::anyhow!("adapter omitted structured evidence"))?;
    match operation {
        "prepare" => ensure!(
            evidence["drained"] == true
                && evidence["saved"] == true
                && evidence["writers_stopped"] == true,
            "adapter did not acknowledge checked persistence"
        ),
        "ready" => ensure!(
            evidence["application_ready"] == true
                && evidence["release_id"].as_str() == release
                && evidence["manifest_sha256"].as_str() == expected_manifest,
            "adapter readiness does not match release"
        ),
        "maintenance" => ensure!(
            evidence["gate_closed"] == true,
            "adapter did not close maintenance gate"
        ),
        "resume" => ensure!(
            evidence["gate_open"] == true,
            "adapter did not reopen maintenance gate"
        ),
        _ => {}
    }
    Ok(evidence.clone())
}
