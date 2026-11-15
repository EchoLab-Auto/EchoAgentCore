//! Transaction backend for an explicitly trusted, administrator-installed adapter.
use crate::{
    execution::{invoke, Execution},
    transaction::{Backend, Drain, Observation, Transaction},
};
use anyhow::{ensure, Result};

pub struct AdapterBackend {
    pub config: Execution,
}
impl AdapterBackend {
    fn call(&self, t: &Transaction, op: &str, release: Option<&str>) -> Result<serde_json::Value> {
        let hash = if release == Some(t.target_release.as_str()) {
            Some(t.manifest_sha256.as_str())
        } else {
            None
        };
        invoke(&self.config, op, &t.id, release, hash)
    }
}
impl Backend for AdapterBackend {
    fn observe(&mut self, instance_id: &str) -> Result<Observation> {
        ensure!(
            instance_id == self.config.deployment.instance_id,
            "wrong installation instance"
        );
        let v = invoke(&self.config, "observe", "observation", None, None)?;
        let current_release = match v.get("current_release") {
            Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => {
                crate::model::safe_id(s)?;
                Some(s.clone())
            }
            _ => anyhow::bail!("adapter missing current release"),
        };
        Ok(Observation::new(
            current_release,
            v["maintenance"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("missing maintenance state"))?,
            v["ready"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("missing readiness state"))?,
            v["active_work"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| anyhow::anyhow!("missing active work count"))?,
        ))
    }
    fn prepare(&mut self, t: &Transaction) -> Result<()> {
        let bundle = crate::verify(
            &std::path::Path::new(&self.config.deployment.store).join(&t.target_release),
        )?;
        ensure!(
            bundle.manifest_sha256() == t.manifest_sha256,
            "staged manifest changed"
        );
        self.config.deployment.check_release(bundle.release())?;
        self.call(t, "preflight", Some(&t.target_release))?;
        Ok(())
    }
    fn enter_maintenance(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "maintenance", Some(&t.target_release))?;
        Ok(())
    }
    fn drain(&mut self, t: &Transaction) -> Result<Drain> {
        self.call(t, "prepare", Some(&t.target_release))?;
        let observed = self.observe(&t.instance_id)?;
        ensure!(
            observed.maintenance,
            "maintenance gate disappeared during drain"
        );
        if observed.active_work == 0 {
            Ok(Drain::Drained)
        } else {
            Ok(Drain::Busy {
                remaining: observed.active_work,
            })
        }
    }
    fn switch(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "switch", Some(&t.target_release))?;
        Ok(())
    }
    fn resume(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "start", Some(&t.target_release))?;
        Ok(())
    }
    fn verify_ready(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "ready", Some(&t.target_release))?;
        Ok(())
    }
    fn release_maintenance(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "resume", Some(&t.target_release))?;
        Ok(())
    }
    fn restore(&mut self, t: &Transaction) -> Result<()> {
        self.call(t, "restore", t.previous_release.as_deref())?;
        Ok(())
    }
}
