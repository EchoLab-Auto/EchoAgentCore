//! Safe removal planning. Program removal is not data rollback and needs a separate
//! approved executor; this module deliberately never removes live paths.
use crate::{
    install::{InstallSpec, InstallTopology},
    model::absolute_path,
};
use anyhow::{ensure, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
#[derive(Debug, Serialize)]
pub struct UninstallPlan {
    pub instance_id: String,
    pub units_to_disable: Vec<String>,
    pub release_root: PathBuf,
    pub configurations_preserved: Vec<PathBuf>,
    pub user_data_preserved: bool,
    pub execution_supported: bool,
    pub reason: String,
}
pub fn plan(spec: &InstallSpec) -> Result<UninstallPlan> {
    spec.validate()?;
    let names = spec.unit_names();
    let mut units = vec![names.core, names.updater];
    if spec.topology == InstallTopology::Standalone {
        units.push(names.panel.expect("standalone panel"));
    }
    let mut configs = vec![spec.core_config.clone()];
    if let Some(p) = &spec.panel_config {
        configs.push(p.clone());
    }
    Ok(UninstallPlan{instance_id:spec.instance_id.clone(),units_to_disable:units,release_root:spec.release_root.clone(),configurations_preserved:configs,user_data_preserved:true,execution_supported:false,reason:"Removal requires ownership ledger and explicit running-service drain; no live deletion is implemented".into()})
}
#[derive(Debug, Serialize)]
pub struct NodePlan {
    pub node_id: String,
    pub execution_file: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct FleetPlan {
    pub nodes: Vec<NodePlan>,
    pub parallelism: u32,
    pub stop_on_failure: bool,
    pub execution_supported: bool,
}
pub fn fleet(nodes: Vec<(String, PathBuf)>) -> Result<FleetPlan> {
    ensure!(!nodes.is_empty(), "no deployment nodes");
    let mut seen = std::collections::BTreeSet::new();
    let mut out = vec![];
    for (id, path) in nodes {
        crate::model::safe_id(&id)?;
        absolute_path(Path::new(&path))?;
        ensure!(seen.insert(id.clone()), "duplicate node");
        out.push(NodePlan {
            node_id: id,
            execution_file: path,
        });
    }
    Ok(FleetPlan {
        nodes: out,
        parallelism: 1,
        stop_on_failure: true,
        execution_supported: false,
    })
}
