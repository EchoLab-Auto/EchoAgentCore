use anyhow::ensure;
use clap::{Args, Parser, Subcommand};
use echo_deploy::{
    json_file,
    model::{Deployment, Release},
    transaction::{BeginSpec, RunOutcome, TransactionStore},
};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(
    version,
    about = "Release assembly, integrity/authentication, staging and durable deployment transactions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Args)]
struct TrustArgs {
    #[arg(long, requires_all=["key_id","signature"])]
    trust_store: Option<PathBuf>,
    #[arg(long, requires_all=["trust_store","signature"])]
    key_id: Option<String>,
    #[arg(long, requires_all=["trust_store","key_id"])]
    signature: Option<PathBuf>,
}
#[derive(Subcommand)]
enum Command {
    RuntimeAdapter {
        #[arg(long)]
        config: PathBuf,
    },
    Verify {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        require_complete: bool,
        #[command(flatten)]
        trust: TrustArgs,
    },
    Inspect {
        #[arg(long)]
        deployment: PathBuf,
    },
    Plan {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        deployment: PathBuf,
    },
    InitStore {
        #[arg(long)]
        store: PathBuf,
    },
    Stage {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        deployment: PathBuf,
        #[arg(long)]
        allow_development: bool,
        #[command(flatten)]
        trust: TrustArgs,
    },
    /// Assemble exact file outputs (including a complete web tree), never compile/execute them.
    Pack {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        inputs: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Install new unit/launcher files with ownership evidence; never reload/enable/start services.
    InstallFiles {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        unit_directory: PathBuf,
        #[arg(long)]
        launcher_directory: PathBuf,
        #[arg(long)]
        ledger: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    UninstallFiles {
        #[arg(long)]
        ledger: PathBuf,
        #[arg(long)]
        offline_only: bool,
        #[arg(long)]
        confirm: bool,
    },
    /// Render the initial standalone gate from an integrity-verified bundle; writes no files.
    PanelGate {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        bundle: PathBuf,
    },
    InstallPlan {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Invoke an explicit administrator-owned adapter, never one supplied by the release.
    Apply {
        #[arg(long)]
        execution: PathBuf,
        #[arg(long)]
        release: String,
        #[arg(long)]
        transaction: Option<String>,
        #[arg(long)]
        confirm: bool,
        #[arg(long)]
        allow_development: bool,
        #[command(flatten)]
        trust: TrustArgs,
    },
    SystemdApply {
        #[arg(long)]
        execution: PathBuf,
        #[arg(long)]
        release: String,
        #[arg(long)]
        transaction: Option<String>,
        #[arg(long)]
        confirm: bool,
        #[arg(long)]
        allow_development: bool,
        #[command(flatten)]
        trust: TrustArgs,
    },
    SystemdRecover {
        #[arg(long)]
        execution: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    ApplyRequest {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    Recover {
        #[arg(long)]
        execution: PathBuf,
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        trust: TrustArgs,
    },
    Status {
        #[arg(long)]
        execution: PathBuf,
    },
    ComposePlan {
        #[arg(long)]
        spec: PathBuf,
    },
    UninstallPlan {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Capture explicitly declared old/new units into immutable local evidence; never stops services.
    MigrationCapture {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    /// Stop/disable captured old units and enable installed new units; does not activate the new release.
    MigrationTakeover {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        confirm: bool,
    },
    /// Restore captured old service state before new-unit activation only; never rolls back data.
    MigrationRecover {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        confirm: bool,
        #[arg(long)]
        acknowledge_reversible: bool,
    },
    /// Internal ExecStartPre guard: durably forbid restoring legacy applications.
    MigrationMarkActivation {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        instance_id: String,
        #[arg(long)]
        unit: String,
    },
    /// Check install/execution/runtime declarations together; never activates a service.
    CheckConfig {
        #[arg(long)]
        install: PathBuf,
        #[arg(long)]
        systemd: PathBuf,
        #[arg(long)]
        runtime: PathBuf,
    },
    Doctor {
        #[arg(long)]
        execution: PathBuf,
    },
}
fn print(value: impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
fn authenticate(
    bundle: &echo_deploy::VerifiedBundle,
    args: &TrustArgs,
) -> anyhow::Result<Option<echo_deploy::trust::VerifiedManifest>> {
    match (&args.trust_store, &args.key_id, &args.signature) {
        (Some(store), Some(key), Some(sig)) => Ok(Some(bundle.authenticate(
            &echo_deploy::trust::TrustStore::open(store)?,
            key,
            sig,
        )?)),
        (None, None, None) => Ok(None),
        _ => anyhow::bail!("incomplete trust arguments"),
    }
}
fn config(path: &std::path::Path) -> anyhow::Result<echo_deploy::execution::Execution> {
    let e: echo_deploy::execution::Execution = json_file(path)?;
    e.validate()?;
    Ok(e)
}
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ExecutionDocument {
    Adapter(echo_deploy::execution::Execution),
    Systemd(echo_deploy::systemd_transaction::SystemdExecution),
}
impl ExecutionDocument {
    fn state_and_instance(&self) -> (&std::path::Path, &str) {
        match self {
            Self::Adapter(e) => (&e.state_dir, &e.deployment.instance_id),
            Self::Systemd(e) => (&e.state_dir, &e.deployment.instance_id),
        }
    }
}
fn execution_document(path: &std::path::Path) -> anyhow::Result<ExecutionDocument> {
    let document: ExecutionDocument = json_file(path)?;
    match &document {
        ExecutionDocument::Adapter(e) => e.validate()?,
        ExecutionDocument::Systemd(e) => e.validate()?,
    }
    Ok(document)
}

fn run() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::RuntimeAdapter { config } => {
            use std::io::Read;
            let c: echo_deploy::runtime_adapter::RuntimeConfig = json_file(&config)?;
            let mut bytes = Vec::new();
            std::io::stdin().take(65537).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65536, "control request too large");
            let req: echo_deploy::systemd::ControlRequest = serde_json::from_slice(&bytes)?;
            print(echo_deploy::runtime_adapter::handle(&c, req)?)
        }
        Command::Verify {
            bundle,
            require_complete,
            trust,
        } => {
            let b = echo_deploy::verify(&bundle)?;
            if require_complete {
                echo_deploy::pack::require_complete(&bundle, b.release())?;
            }
            let auth = authenticate(&b, &trust)?;
            print(
                serde_json::json!({"integrity_verified":true,"publisher_authenticated":auth.is_some(),"release_id":b.release().release_id,"manifest_sha256":b.manifest_sha256(),"development":b.release().development,"activation_supported":false}),
            )
        }
        Command::ComposePlan { spec } => {
            let s: echo_deploy::compose::ComposeSpec = json_file(&spec)?;
            print(s.render()?)
        }
        Command::UninstallPlan { spec } => {
            let s: echo_deploy::install::InstallSpec = json_file(&spec)?;
            print(echo_deploy::operations::plan(&s)?)
        }
        Command::Inspect { deployment } => {
            let d: Deployment = json_file(&deployment)?;
            d.validate()?;
            print(d)
        }
        Command::Plan { bundle, deployment } => {
            let d: Deployment = json_file(&deployment)?;
            print(echo_deploy::plan(&echo_deploy::verify(&bundle)?, &d)?)
        }
        Command::InitStore { store } => {
            echo_deploy::init_store(&store)?;
            print(serde_json::json!({"candidate_store_created":store}))
        }
        Command::Stage {
            bundle,
            deployment,
            allow_development,
            trust,
        } => {
            let d: Deployment = json_file(&deployment)?;
            let b = echo_deploy::verify(&bundle)?;
            let p = match authenticate(&b, &trust)? {
                Some(a) => echo_deploy::stage_authenticated(&b, &d, &a)?,
                None => echo_deploy::stage(&b, &d, allow_development)?,
            };
            print(serde_json::json!({"state":"staged_not_activated","plan":p}))
        }
        Command::Pack {
            template,
            inputs,
            output,
        } => {
            let release: Release = json_file(&template)?;
            let inputs: Vec<echo_deploy::pack::BuildOutput> = json_file(&inputs)?;
            print(echo_deploy::pack::assemble(release, &inputs, &output)?)
        }
        Command::InstallFiles {
            spec,
            unit_directory,
            launcher_directory,
            ledger,
            confirm,
        } => {
            let s: echo_deploy::install::InstallSpec = json_file(&spec)?;
            match echo_deploy::installation::install_files_with_ledger(
                &s,
                &unit_directory,
                &launcher_directory,
                &ledger,
                confirm,
            ) {
                Ok(report) => print(report),
                Err(failure) => {
                    eprintln!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "installation_failed",
                            "partial_ledger": failure.partial_ledger,
                            "persisted_ledger": failure.persisted_ledger,
                            "rolled_back_files": failure.rolled_back_files,
                            "kept_modified_files": failure.kept_modified_files,
                            "rollback_errors": failure.rollback_errors,
                        }))?
                    );
                    Err(failure.error)
                }
            }
        }
        Command::UninstallFiles {
            ledger,
            offline_only,
            confirm,
        } => {
            let ledger = echo_deploy::installation::read_ledger(&ledger)?;
            print(echo_deploy::installation::uninstall_files(
                &ledger,
                confirm,
                offline_only,
                None,
            )?)
        }
        Command::PanelGate { spec, bundle } => {
            let spec: echo_deploy::install::InstallSpec = json_file(&spec)?;
            let verified = echo_deploy::verify(&bundle)?;
            let topology = match spec.topology {
                echo_deploy::install::InstallTopology::CoreOnly => {
                    echo_deploy::model::Topology::CoreOnly
                }
                echo_deploy::install::InstallTopology::Embedded => {
                    echo_deploy::model::Topology::Embedded
                }
                echo_deploy::install::InstallTopology::Standalone => {
                    echo_deploy::model::Topology::Standalone
                }
            };
            let deployment = Deployment {
                schema_version: 1,
                instance_id: spec.instance_id.clone(),
                topology,
                scope: match spec.scope {
                    echo_deploy::install::InstallScope::User => echo_deploy::model::Scope::User,
                    echo_deploy::install::InstallScope::System => echo_deploy::model::Scope::System,
                },
                target_os: std::env::consts::OS.into(),
                target_arch: std::env::consts::ARCH.into(),
                store: spec
                    .deployment
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("panel-gate requires deployment runtime"))?
                    .release_store
                    .to_string_lossy()
                    .into_owned(),
                required_modules: vec![],
            };
            deployment.check_release(verified.release())?;
            print(
                spec.panel_gate_document(
                    &verified.release().release_id,
                    &verified.release().management_protocol,
                )?
                .ok_or_else(|| anyhow::anyhow!("panel-gate requires standalone deployment"))?,
            )
        }
        Command::InstallPlan { spec, output } => {
            let s: echo_deploy::install::InstallSpec = json_file(&spec)?;
            match output {
                Some(out) => print(s.write_install(&out)?),
                None => {
                    let mut rendered = serde_json::to_value(s.render()?)?;
                    rendered["runtime_adapter_config"] =
                        serde_json::to_value(s.runtime_adapter_config()?)?;
                    print(rendered)
                }
            }
        }
        Command::Status { execution } => {
            let document = execution_document(&execution)?;
            let (state, instance) = document.state_and_instance();
            let store = TransactionStore::open(state)?;
            print(store.load(instance)?)
        }
        Command::MigrationMarkActivation {
            state_dir,
            instance_id,
            unit,
        } => {
            echo_deploy::migration::mark_activation(&state_dir, &instance_id, &unit)?;
            print(serde_json::json!({"activation_may_have_started": true}))
        }
        Command::MigrationCapture { spec, confirm } => {
            ensure!(
                confirm,
                "capturing migration evidence requires --confirm (writes a new evidence file)"
            );
            let spec: echo_deploy::migration::MigrationSpec = json_file(&spec)?;
            echo_deploy::activation::CgroupGuard::current(&spec.instance_id)?;
            let _lock = echo_deploy::state_lock::StateLock::acquire(&spec.state_dir)?;
            let evidence = echo_deploy::migration::capture(
                &spec,
                &mut echo_deploy::migration::ExecTransport::new(),
            )?;
            print(evidence.preview())
        }
        Command::MigrationTakeover { spec, confirm } => {
            ensure!(confirm, "service takeover requires --confirm");
            let spec: echo_deploy::migration::MigrationSpec = json_file(&spec)?;
            echo_deploy::activation::CgroupGuard::current(&spec.instance_id)?;
            let _lock = echo_deploy::state_lock::StateLock::acquire(&spec.state_dir)?;
            let evidence = echo_deploy::migration::Evidence::load(&spec)?;
            print(echo_deploy::migration::takeover(
                &spec,
                &evidence,
                &mut echo_deploy::migration::ExecTransport::new(),
                true,
            )?)
        }
        Command::MigrationRecover {
            spec,
            confirm,
            acknowledge_reversible,
        } => {
            ensure!(confirm, "migration recovery requires --confirm");
            let spec: echo_deploy::migration::MigrationSpec = json_file(&spec)?;
            echo_deploy::activation::CgroupGuard::current(&spec.instance_id)?;
            let _lock = echo_deploy::state_lock::StateLock::acquire(&spec.state_dir)?;
            let evidence = echo_deploy::migration::Evidence::load(&spec)?;
            print(echo_deploy::migration::recover(
                &spec,
                &evidence,
                &mut echo_deploy::migration::ExecTransport::new(),
                acknowledge_reversible,
            )?)
        }
        Command::CheckConfig {
            install,
            systemd,
            runtime,
        } => {
            let install = json_file(&install)?;
            let systemd = json_file(&systemd)?;
            let runtime = json_file(&runtime)?;
            print(echo_deploy::configuration::check(
                &install, &systemd, &runtime,
            )?)
        }
        Command::Doctor { execution } => match execution_document(&execution)? {
            ExecutionDocument::Adapter(e) => {
                echo_deploy::safe_fs::private_store(&echo_deploy::safe_fs::Dir::absolute(
                    std::path::Path::new(&e.deployment.store),
                )?)?;
                print(echo_deploy::execution::invoke(
                    &e, "observe", "doctor", None, None,
                )?)
            }
            ExecutionDocument::Systemd(e) => {
                use echo_deploy::transaction::Backend;
                let mut backend = e.create_transaction_backend()?;
                let observed = backend.observe(&e.deployment.instance_id)?;
                print(serde_json::json!({
                    "current_release": observed.current_release,
                    "maintenance": observed.maintenance,
                    "ready": observed.ready,
                    "active_work": observed.active_work,
                }))
            }
        },
        Command::SystemdApply {
            execution,
            release,
            transaction,
            confirm,
            allow_development,
            trust,
        } => {
            ensure!(confirm, "systemd activation requires --confirm");
            systemd_apply(execution, release, transaction, allow_development, trust)
        }
        Command::SystemdRecover { execution, confirm } => {
            ensure!(confirm, "systemd recovery requires --confirm");
            let e: echo_deploy::systemd_transaction::SystemdExecution = json_file(&execution)?;
            e.validate()?;
            let store = TransactionStore::open(&e.state_dir)?;
            let _lock = echo_deploy::state_lock::StateLock::acquire(&e.state_dir)?;
            echo_deploy::activation::CgroupGuard::current(&e.deployment.instance_id)?;
            let saved = store
                .load(&e.deployment.instance_id)?
                .ok_or_else(|| anyhow::anyhow!("no transaction to recover"))?;
            let context = echo_deploy::systemd_activation::SystemdActivationContext::open_recovery(
                &e, &saved,
            )?;
            let snapshot = context.load_snapshot_recovery(&e, &saved)?;
            let mut backend = snapshot.create_transaction_backend()?;
            let outcome = store.recover(&e.deployment.instance_id, &mut backend)?;
            print(store.load(&e.deployment.instance_id)?)?;
            ensure!(
                !matches!(outcome, Some(RunOutcome::RecoveryRequired)),
                "systemd recovery requires operator intervention"
            );
            Ok(())
        }
        Command::ApplyRequest { request, confirm } => {
            ensure!(confirm, "activation requires --confirm");
            let r: echo_deploy::request::UpdateRequest = json_file(&request)?;
            r.validate()?;
            let operation = match execution_document(&r.execution)? {
                ExecutionDocument::Adapter(_) => apply,
                ExecutionDocument::Systemd(_) => systemd_apply,
            };
            operation(
                r.execution,
                r.release,
                Some(r.transaction),
                r.allow_development,
                TrustArgs {
                    trust_store: r.trust_store,
                    key_id: r.key_id,
                    signature: r.signature,
                },
            )
        }
        Command::Apply {
            execution,
            release,
            transaction,
            confirm,
            allow_development,
            trust,
        } => {
            ensure!(confirm, "activation requires --confirm");
            apply(execution, release, transaction, allow_development, trust)
        }
        Command::Recover {
            execution,
            confirm,
            trust,
        } => {
            ensure!(confirm, "recovery requires --confirm");
            let e = config(&execution)?;
            let store = TransactionStore::open(&e.state_dir)?;
            let _lock = echo_deploy::state_lock::StateLock::acquire(&e.state_dir)?;
            let id = e.deployment.instance_id.clone();
            let saved = store
                .load(&id)?
                .ok_or_else(|| anyhow::anyhow!("no transaction to recover"))?;
            let b = echo_deploy::verify(
                &std::path::Path::new(&e.deployment.store).join(&saved.target_release),
            )?;
            // A supplied signature remains an additional check. Offline recovery
            // is anchored in the durable transaction and existing snapshot, not
            // in a newly created development authorization.
            if let Some(authentication) = authenticate(&b, &trust)? {
                echo_deploy::activation::ActivationContext::open(
                    &e,
                    &saved.target_release,
                    &authentication,
                )?;
            }
            let context = echo_deploy::activation::ActivationContext::open_recovery(&e, &saved)?;
            let snapshot = context.load_snapshot_recovery(&e, &saved)?;
            let mut backend = echo_deploy::adapter::AdapterBackend {
                config: snapshot.pinned_execution(&e)?,
            };
            let result = store.recover(&id, &mut backend)?;
            print(store.load(&id)?)?;
            ensure!(
                !matches!(result, Some(RunOutcome::RecoveryRequired)),
                "manual recovery required"
            );
            Ok(())
        }
    }
}
fn systemd_apply(
    execution: PathBuf,
    release: String,
    transaction: Option<String>,
    allow_development: bool,
    trust: TrustArgs,
) -> anyhow::Result<()> {
    let e: echo_deploy::systemd_transaction::SystemdExecution = json_file(&execution)?;
    e.validate()?;
    echo_deploy::model::safe_id(&release)?;
    let b = echo_deploy::verify(&std::path::Path::new(&e.deployment.store).join(&release))?;
    e.deployment.check_release(b.release())?;
    ensure!(
        b.release().release_id == release,
        "candidate release identity mismatch"
    );
    let authentication = authenticate(&b, &trust)?;
    let context = match authentication {
        Some(authentication) => echo_deploy::systemd_activation::SystemdActivationContext::open(
            &e,
            &release,
            &authentication,
        )?,
        None => echo_deploy::systemd_activation::SystemdActivationContext::open_development(
            &e,
            &release,
            allow_development,
        )?,
    };
    echo_deploy::activation::CgroupGuard::current(&e.deployment.instance_id)?;
    let store = TransactionStore::open(&e.state_dir)?;
    let _lock = echo_deploy::state_lock::StateLock::acquire(&e.state_dir)?;
    let id = transaction.unwrap_or(format!(
        "tx-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let spec = BeginSpec::new(
        id,
        e.deployment.instance_id.clone(),
        release,
        b.manifest_sha256(),
    );
    let provisional = echo_deploy::systemd_activation::provisional_transaction(&spec)?;
    let snapshot = context.prepare_snapshot(&e, &provisional, None)?;
    let mut backend = snapshot.create_transaction_backend()?;
    let mut t = store.begin(spec, &mut backend)?;
    let outcome = store.execute(&mut t, &mut backend)?;
    print(t)?;
    ensure!(
        matches!(outcome, RunOutcome::Committed),
        "systemd transaction did not commit: {outcome:?}"
    );
    Ok(())
}
fn apply(
    execution: PathBuf,
    release: String,
    transaction: Option<String>,
    allow_development: bool,
    trust: TrustArgs,
) -> anyhow::Result<()> {
    echo_deploy::model::safe_id(&release)?;
    let e = config(&execution)?;
    let b = echo_deploy::verify(&std::path::Path::new(&e.deployment.store).join(&release))?;
    e.deployment.check_release(b.release())?;
    ensure!(
        b.release().release_id == release,
        "candidate directory and manifest release identity differ"
    );
    let authentication = authenticate(&b, &trust)?;
    ensure!(
        authentication.is_some() || (allow_development && b.release().development),
        "activation requires trusted signature or explicit development authorization"
    );
    let context = if let Some(authentication) = &authentication {
        echo_deploy::activation::ActivationContext::open(&e, &release, authentication)?
    } else {
        echo_deploy::activation::ActivationContext::open_development(
            &e,
            &release,
            allow_development,
        )?
    };
    let store = TransactionStore::open(&e.state_dir)?;
    let _lock = echo_deploy::state_lock::StateLock::acquire(&e.state_dir)?;
    let id = transaction.unwrap_or(format!(
        "tx-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let spec = BeginSpec::new(
        id,
        e.deployment.instance_id.clone(),
        release,
        b.manifest_sha256(),
    );
    let provisional = echo_deploy::systemd_activation::provisional_transaction(&spec)?;
    let snapshot = context.prepare_snapshot(&e, &provisional, None)?;
    let mut backend = echo_deploy::adapter::AdapterBackend {
        config: snapshot.pinned_execution(&e)?,
    };
    let mut t = store.begin(spec, &mut backend)?;
    let result = store.execute(&mut t, &mut backend)?;
    print(&t)?;
    ensure!(
        matches!(result, RunOutcome::Committed),
        "deployment did not commit: {result:?}"
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("echo-deploy: {e:#}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn systemd_document() -> serde_json::Value {
        json!({
            "deployment": {
                "schema_version": 1, "instance_id": "cli-test", "scope": "user",
                "topology": "core_only", "target_os": "linux",
                "target_arch": std::env::consts::ARCH, "store": "/tmp/cli-store",
                "required_modules": []
            },
            "state_dir": "/tmp/cli-state", "unit_directory": "/tmp/cli-units",
            "current_selector": "/tmp/cli-state/current",
            "core_unit": "echo-agent-cli-test-core.service", "panel_unit": null,
            "control_adapter": "/tmp/cli-adapter", "timeout_seconds": 30,
            "reversible_data": true
        })
    }

    #[test]
    fn execution_routing_accepts_only_complete_strict_backend_documents() {
        let systemd = systemd_document();
        assert!(matches!(
            serde_json::from_value::<ExecutionDocument>(systemd.clone()).unwrap(),
            ExecutionDocument::Systemd(_)
        ));
        let mut adapter = systemd.clone();
        let map = adapter.as_object_mut().unwrap();
        for key in [
            "unit_directory",
            "current_selector",
            "core_unit",
            "panel_unit",
            "control_adapter",
        ] {
            map.remove(key);
        }
        map.insert("schema_version".into(), json!(1));
        map.insert("adapter".into(), json!("/tmp/cli-hook"));
        assert!(matches!(
            serde_json::from_value::<ExecutionDocument>(adapter.clone()).unwrap(),
            ExecutionDocument::Adapter(_)
        ));
        adapter["control_adapter"] = json!("/tmp/mixed-adapter");
        assert!(serde_json::from_value::<ExecutionDocument>(adapter).is_err());
        let mut incomplete = systemd.clone();
        incomplete.as_object_mut().unwrap().remove("panel_unit");
        assert!(serde_json::from_value::<ExecutionDocument>(incomplete).is_err());
        let mut invalid = systemd;
        invalid["core_unit"] = json!("echo-agent-production-core.service");
        assert!(serde_json::from_value::<ExecutionDocument>(invalid).is_err());
    }
}
