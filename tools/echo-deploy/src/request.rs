//! Frozen local update requests consumed by independent oneshot units.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub schema_version: u32,
    pub execution: PathBuf,
    pub release: String,
    pub transaction: String,
    pub allow_development: bool,
    pub trust_store: Option<PathBuf>,
    pub key_id: Option<String>,
    pub signature: Option<PathBuf>,
}
impl UpdateRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported update request");
        crate::model::absolute_path(&self.execution)?;
        crate::model::safe_id(&self.release)?;
        crate::model::safe_id(&self.transaction)?;
        match (&self.trust_store, &self.key_id, &self.signature) {
            (Some(t), Some(k), Some(s)) => {
                crate::model::absolute_path(t)?;
                crate::model::absolute_path(s)?;
                crate::model::safe_id(k)?;
            }
            (None, None, None) => ensure!(
                self.allow_development,
                "unsigned request requires explicit development authorization"
            ),
            _ => anyhow::bail!("partial trust configuration"),
        }
        Ok(())
    }
}
