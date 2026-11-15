//! Behavioral tests for transactional file-only installation/uninstallation.
//!
//! The source modules are included directly (the same pattern as
//! `tests/install.rs`) so this test does not depend on library wiring changes.
//! No test invokes systemctl, systemd or any other service manager.

use echo_deploy::{install, installation};

use anyhow::Result;
use install::{InstallScope, InstallSpec, InstallTopology};
use installation::{
    install_files, install_files_with_ledger, install_files_with_ledger_and_hook, read_ledger,
    uninstall_files, write_ledger_new, LedgerPublishHook, ManagedFileKind, OwnershipLedger,
    ServiceStopped, FILES_INSTALLED_NOT_ACTIVATED,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, Permissions},
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn user_spec() -> InstallSpec {
    InstallSpec {
        instance_id: "local-dev".into(),
        scope: InstallScope::User,
        release_root: PathBuf::from("/srv/echo"),
        core_config: PathBuf::from("/etc/echo/core.toml"),
        panel_config: None,
        topology: InstallTopology::CoreOnly,
        deploy_executable: PathBuf::from("/opt/echo-deploy/bin/echo-deploy"),
        state_dir: PathBuf::from("/var/lib/echo"),
        deployment: None,
    }
}

fn standalone_spec() -> InstallSpec {
    InstallSpec {
        topology: InstallTopology::Standalone,
        panel_config: Some(PathBuf::from("/etc/echo/panel.toml")),
        ..user_spec()
    }
}

fn targets() -> (TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let units = root.path().join("units");
    let bin = root.path().join("bin");
    fs::create_dir(&units).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::set_permissions(&units, Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
    (root, units, bin)
}

fn sha256_file(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn core_unit_path(units: &Path) -> PathBuf {
    units.join("echo-agent-local-dev-core.service")
}

fn updater_unit_path(units: &Path) -> PathBuf {
    units.join("echo-agent-local-dev-update.service")
}

fn core_launcher_path(bin: &Path) -> PathBuf {
    bin.join("echo-agent-local-dev")
}

fn assert_no_staged_files(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.contains(".echo-deploy-"),
            "staged temporary file left behind: {name}"
        );
    }
}

#[test]
fn installs_new_files_with_hashes_and_reports_not_activated() {
    let (root, units, bin) = targets();
    let mut spec = user_spec();
    // Keep config/state inside the temp root so the test can prove they are
    // untouched without needing to write under /etc or /var.
    let config = root.path().join("config/core.toml");
    fs::create_dir(root.path().join("config")).unwrap();
    fs::write(&config, b"operator config\n").unwrap();
    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("data.json"), b"operator data\n").unwrap();
    spec.core_config = config.clone();
    spec.state_dir = state.clone();

    let report = install_files(&spec, &units, &bin, true).unwrap();

    assert_eq!(report.status, FILES_INSTALLED_NOT_ACTIVATED);
    assert_eq!(report.ledger.outcome, FILES_INSTALLED_NOT_ACTIVATED);
    assert!(!report.activation_supported);
    assert!(!report.services_started);
    assert!(!report.systemctl_invoked);
    assert!(!report.ledger.partial);
    assert_eq!(report.ledger.files.len(), 3);
    assert_eq!(report.created_files.len(), 3);
    assert!(core_unit_path(&units).is_file());
    assert!(updater_unit_path(&units).is_file());
    assert!(core_launcher_path(&bin).is_file());
    assert_no_staged_files(&units);
    assert_no_staged_files(&bin);

    for entry in &report.ledger.files {
        assert_eq!(sha256_file(&entry.path), entry.sha256);
        assert_eq!(fs::metadata(&entry.path).unwrap().len(), entry.size);
        let mode = fs::metadata(&entry.path).unwrap().mode() & 0o7777;
        assert_eq!(mode, entry.mode);
        assert_eq!(
            entry.mode,
            match entry.kind {
                ManagedFileKind::Unit => 0o644,
                ManagedFileKind::Launcher => 0o755,
            }
        );
    }
    assert!(report
        .ledger
        .files
        .iter()
        .any(|entry| entry.kind == ManagedFileKind::Unit));
    assert!(report
        .ledger
        .files
        .iter()
        .any(|entry| entry.kind == ManagedFileKind::Launcher));
    assert_eq!(fs::read(&config).unwrap(), b"operator config\n");
    assert_eq!(
        fs::read(state.join("data.json")).unwrap(),
        b"operator data\n"
    );

    // A ledger can be persisted and read back without replacing an old file.
    let ledger_path = root.path().join("ownership.json");
    write_ledger_new(&ledger_path, &report.ledger).unwrap();
    assert_eq!(read_ledger(&ledger_path).unwrap(), report.ledger);
    assert_eq!(fs::metadata(&ledger_path).unwrap().mode() & 0o7777, 0o600);
    assert!(write_ledger_new(&ledger_path, &report.ledger).is_err());
    assert_eq!(read_ledger(&ledger_path).unwrap(), report.ledger);
}

#[test]
fn standalone_installs_panel_unit_and_launcher() {
    let (_root, units, bin) = targets();
    let report = install_files(&standalone_spec(), &units, &bin, true).unwrap();
    assert_eq!(report.ledger.files.len(), 5);
    assert!(units.join("echo-agent-local-dev-panel.service").is_file());
    assert!(bin.join("echo-agent-local-dev-panel").is_file());
    assert_eq!(report.ledger.units.len(), 3);
}

#[test]
fn existing_target_is_never_overwritten_and_nothing_else_is_written() {
    let (_root, units, bin) = targets();
    let target = core_launcher_path(&bin);
    fs::write(&target, b"operator-owned file\n").unwrap();

    let failure = install_files(&user_spec(), &units, &bin, true).unwrap_err();

    assert!(format!("{failure:#}").contains("refusing to overwrite"));
    assert_eq!(fs::read(&target).unwrap(), b"operator-owned file\n");
    assert!(!core_unit_path(&units).exists());
    assert!(!updater_unit_path(&units).exists());
    assert!(failure.partial_ledger.partial);
    assert!(failure.partial_ledger.files.is_empty());
    assert!(failure.rolled_back_files.is_empty());
}

#[test]
fn a_symlink_collision_is_rejected_without_following_or_replacing_it() {
    let (_root, units, bin) = targets();
    let outside = units.parent().unwrap().join("outside");
    fs::write(&outside, b"outside\n").unwrap();
    let target = core_unit_path(&units);
    symlink(&outside, &target).unwrap();

    let failure = install_files(&user_spec(), &units, &bin, true).unwrap_err();
    assert!(format!("{failure:#}").contains("refusing to overwrite"));
    assert!(fs::symlink_metadata(&target)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&outside).unwrap(), b"outside\n");
    assert!(!core_launcher_path(&bin).exists());
}

#[test]
fn failure_after_partial_creation_rolls_back_matching_files_and_returns_partial_ledger() {
    // Root bypasses directory permission checks, so this deterministic
    // mid-transaction failure is only meaningful for a non-root test process.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (_root, units, bin) = targets();
    fs::set_permissions(&bin, Permissions::from_mode(0o500)).unwrap();

    let failure = install_files(&user_spec(), &units, &bin, true).unwrap_err();

    fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();

    assert!(failure.partial_ledger.partial);
    assert!(
        failure.partial_ledger.files.is_empty(),
        "matching files created before the failure must be rolled back"
    );
    assert!(!failure.rolled_back_files.is_empty());
    assert!(failure.kept_modified_files.is_empty());
    assert!(
        failure.rollback_errors.is_empty(),
        "{:?}",
        failure.rollback_errors
    );
    assert!(!core_unit_path(&units).exists());
    assert!(!updater_unit_path(&units).exists());
    assert!(!core_launcher_path(&bin).exists());
    assert_no_staged_files(&units);
    assert_no_staged_files(&bin);
}

#[test]
fn uninstall_without_proof_refuses_before_touching_files() {
    let (_root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();

    let error = uninstall_files(&report.ledger, true, false, None).unwrap_err();
    assert!(format!("{error:#}").contains("active state is unknown"));
    for entry in &report.ledger.files {
        assert!(entry.path.is_file());
    }
}

#[test]
fn offline_only_uninstall_removes_only_matching_managed_files() {
    let (root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();

    let unrelated = units.join("operator-note.conf");
    fs::write(&unrelated, b"keep me\n").unwrap();
    let data = root.path().join("user-data");
    fs::write(&data, b"do not purge\n").unwrap();

    let removed = installation::uninstall_offline_files(&report.ledger, true).unwrap();
    assert_eq!(removed.status, installation::FILES_REMOVED_NOT_DEACTIVATED);
    assert_eq!(removed.proof, "offline_only");
    assert!(removed.offline_only);
    assert!(!removed.systemctl_invoked);
    assert_eq!(removed.removed_files.len(), report.ledger.files.len());
    assert!(removed.removed_directories.is_empty());
    assert!(removed.already_absent.is_empty());
    for entry in &report.ledger.files {
        assert!(!entry.path.exists(), "{:?} must be removed", entry.path);
    }
    assert!(units.is_dir(), "shared/target directories must remain");
    assert!(bin.is_dir(), "shared/target directories must remain");
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep me\n");
    assert_eq!(fs::read(&data).unwrap(), b"do not purge\n");
}

#[test]
fn modified_file_blocks_the_whole_uninstall() {
    let (_root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();
    let core = core_unit_path(&units);
    let length = fs::metadata(&core).unwrap().len() as usize;
    fs::write(&core, vec![b'X'; length]).unwrap();

    let error = uninstall_files(&report.ledger, true, true, None).unwrap_err();
    assert!(format!("{error:#}").contains("content hash changed"));
    assert_eq!(fs::read(&core).unwrap(), vec![b'X'; length]);
    for entry in &report.ledger.files {
        if entry.path != core {
            assert!(entry.path.is_file(), "{:?} must not be removed", entry.path);
        }
    }
}

#[test]
fn symlink_replacement_blocks_uninstall() {
    let (root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();
    let core = core_unit_path(&units);
    fs::remove_file(&core).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"outside\n").unwrap();
    symlink(&outside, &core).unwrap();

    let error = uninstall_files(&report.ledger, true, true, None).unwrap_err();
    assert!(format!("{error:#}").contains("non-regular"));
    assert!(fs::symlink_metadata(&core)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&outside).unwrap(), b"outside\n");
    assert!(core_launcher_path(&bin).is_file());
}

struct StubStopped(bool);

impl ServiceStopped for StubStopped {
    fn services_stopped(&self, units: &[String]) -> Result<bool> {
        assert!(!units.is_empty(), "ledger must name its units");
        Ok(self.0)
    }
}

#[test]
fn service_stopped_backend_is_required_to_be_explicit_and_true() {
    let (_root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();

    let refused = StubStopped(false);
    let error = uninstall_files(&report.ledger, true, false, Some(&refused)).unwrap_err();
    assert!(format!("{error:#}").contains("did not prove"));
    assert!(core_unit_path(&units).is_file());

    let stopped = StubStopped(true);
    let removed = uninstall_files(&report.ledger, true, false, Some(&stopped)).unwrap();
    assert_eq!(removed.proof, "service_stopped_backend");
    assert!(!removed.offline_only);
    assert_eq!(removed.removed_files.len(), report.ledger.files.len());
    assert!(!core_unit_path(&units).exists());
}

#[test]
fn confirmation_is_required_for_both_operations() {
    let (_root, units, bin) = targets();
    let failure = install_files(&user_spec(), &units, &bin, false).unwrap_err();
    assert!(format!("{failure:#}").contains("confirmation"));
    assert!(failure.partial_ledger.files.is_empty());

    let report = install_files(&user_spec(), &units, &bin, true).unwrap();
    let error = uninstall_files(&report.ledger, false, true, None).unwrap_err();
    assert!(format!("{error:#}").contains("confirmation"));
    assert!(core_unit_path(&units).is_file());
}

#[test]
fn system_scope_requires_root_euid() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (_root, units, bin) = targets();
    let mut spec = user_spec();
    spec.scope = InstallScope::System;
    let failure = install_files(&spec, &units, &bin, true).unwrap_err();
    assert!(format!("{failure:#}").contains("system scope"));
    assert!(!core_unit_path(&units).exists());
}

#[test]
fn group_or_world_writable_target_directory_is_refused() {
    let (_root, units, bin) = targets();
    fs::set_permissions(&bin, Permissions::from_mode(0o777)).unwrap();
    let failure = install_files(&user_spec(), &units, &bin, true).unwrap_err();
    fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
    assert!(format!("{failure:#}").contains("group/world writable"));
    assert!(!core_unit_path(&units).exists());
}

#[test]
fn refusing_to_write_into_protected_state_or_config_directories() {
    let (_root, units, bin) = targets();
    let mut spec = user_spec();
    spec.state_dir = units.clone();
    let failure = install_files(&spec, &units, &bin, true).unwrap_err();
    assert!(format!("{failure:#}").contains("state_dir"));

    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    let units = config_dir.join("units");
    let bin = root.path().join("bin");
    fs::create_dir_all(&units).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::set_permissions(&units, Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
    let mut spec = user_spec();
    spec.core_config = config_dir.join("core.toml");
    let failure = install_files(&spec, &units, &bin, true).unwrap_err();
    assert!(format!("{failure:#}").contains("core_config"));
    assert!(!core_unit_path(&units).exists());
}

#[test]
fn ledger_whitelist_rejects_arbitrary_user_data_paths_before_deletion() {
    let (_root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();
    let mut ledger = report.ledger.clone();
    let arbitrary = units.join("operator-data.conf");
    fs::write(&arbitrary, b"user data\n").unwrap();
    let metadata = fs::metadata(&arbitrary).unwrap();
    ledger.files.push(installation::OwnedFile {
        path: arbitrary.clone(),
        kind: ManagedFileKind::Unit,
        sha256: format!("{:x}", Sha256::digest(b"user data\n")),
        size: 10,
        mode: 0o644,
        uid: metadata.uid(),
        gid: metadata.gid(),
    });

    let error = uninstall_files(&ledger, true, true, None).unwrap_err();
    assert!(format!("{error:#}").contains("not an expected managed unit"));
    assert_eq!(fs::read(&arbitrary).unwrap(), b"user data\n");
    for entry in &report.ledger.files {
        assert!(entry.path.is_file(), "no partial deletion is allowed");
    }
}

#[test]
fn missing_managed_file_is_reported_as_already_absent() {
    let (_root, units, bin) = targets();
    let report = install_files(&user_spec(), &units, &bin, true).unwrap();
    let missing = core_unit_path(&units);
    fs::remove_file(&missing).unwrap();

    let removed = uninstall_files(&report.ledger, true, true, None).unwrap();
    assert_eq!(removed.already_absent, vec![missing]);
    assert_eq!(removed.removed_files.len(), 2);
}

// ---------------------------------------------------------------------------
// install_files_with_ledger: ledger destination safety, pre-persisted intent,
// final publication failure and partial-ledger evidence.
// ---------------------------------------------------------------------------

fn private_ledger_dir(root: &TempDir) -> (PathBuf, PathBuf) {
    let dir = root.path().join("ledger");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("install-ledger.json");
    (dir, path)
}

fn assert_no_managed_targets(units: &Path, bin: &Path) {
    assert!(!core_unit_path(units).exists(), "core unit must not exist");
    assert!(
        !updater_unit_path(units).exists(),
        "updater unit must not exist"
    );
    assert!(
        !core_launcher_path(bin).exists(),
        "core launcher must not exist"
    );
}

fn assert_no_managed_temp_files(units: &Path, bin: &Path) {
    assert_no_staged_files(units);
    assert_no_staged_files(bin);
}

/// Final-publication fault injector: fails after all managed files exist and
/// before the intent ledger is replaced by the final ledger.
struct FailingPublisher;

impl LedgerPublishHook for FailingPublisher {
    fn before_final_publish(&self, _ledger_path: &Path, _ledger: &OwnershipLedger) -> Result<()> {
        anyhow::bail!("injected final ledger publish failure")
    }
}

/// Reads the ledger path from the hook to prove the intent is already durable
/// before the final ledger is published.
struct InspectingPublisher {
    ledger_path: PathBuf,
    expected_files: usize,
}

impl LedgerPublishHook for InspectingPublisher {
    fn before_final_publish(&self, path: &Path, ledger: &OwnershipLedger) -> Result<()> {
        assert_eq!(path, self.ledger_path.as_path());
        let intent = read_ledger(path).expect("intent ledger must already be readable");
        assert!(intent.partial, "intent ledger must be marked partial");
        assert_eq!(intent.files.len(), self.expected_files);
        for entry in &intent.files {
            assert!(
                entry.path.is_file(),
                "managed file {:?} exists before final ledger publication",
                entry.path
            );
            assert_eq!(sha256_file(&entry.path), entry.sha256);
        }
        assert!(!ledger.partial, "final ledger must not be partial");
        assert_eq!(ledger.files.len(), self.expected_files);
        Ok(())
    }
}

/// Corrupts one just-created file, then fails publication. Rollback must keep
/// that modified file and persist a partial ledger naming it.
struct CorruptingPublisher {
    victim: PathBuf,
}

impl LedgerPublishHook for CorruptingPublisher {
    fn before_final_publish(&self, _ledger_path: &Path, ledger: &OwnershipLedger) -> Result<()> {
        assert!(
            ledger.files.iter().any(|entry| entry.path == self.victim),
            "victim must be part of the final ledger"
        );
        fs::write(&self.victim, b"modified by the fault-injection hook\n")?;
        anyhow::bail!("injected final ledger publish failure after modifying a managed file")
    }
}

#[test]
fn install_with_ledger_persists_the_final_ledger_and_reports_its_path() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);

    let report = install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap();

    assert_eq!(report.status, FILES_INSTALLED_NOT_ACTIVATED);
    assert!(!report.ledger.partial);
    assert_eq!(report.ledger.files.len(), 3);
    assert_eq!(
        report.ledger_path.as_deref(),
        Some(ledger_path.as_path()),
        "the report must name the persisted ledger"
    );
    let report_json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        report_json["ledger_path"],
        serde_json::Value::String(ledger_path.display().to_string()),
        "the report must be directly JSON-printable"
    );

    let persisted = read_ledger(&ledger_path).unwrap();
    assert_eq!(persisted, report.ledger);
    assert_eq!(
        fs::metadata(&ledger_path).unwrap().mode() & 0o7777,
        0o600,
        "the ledger must be private"
    );
    assert_no_managed_temp_files(&units, &bin);

    // The persisted ledger is directly usable as the uninstall whitelist.
    let removed = uninstall_files(&persisted, true, true, None).unwrap();
    assert_eq!(removed.removed_files.len(), 3);
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn relative_ledger_path_is_refused_before_any_write() {
    let (_root, units, bin) = targets();

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, Path::new("ledger.json"), true)
            .unwrap_err();

    assert!(
        format!("{failure:#}").contains("must be absolute"),
        "{failure:#}"
    );
    assert!(failure.partial_ledger.partial);
    assert!(failure.partial_ledger.files.is_empty());
    assert!(failure.persisted_ledger.is_none());
    assert_no_managed_targets(&units, &bin);
    assert_no_managed_temp_files(&units, &bin);
}

#[test]
fn missing_ledger_parent_is_refused_before_any_write() {
    let (root, units, bin) = targets();
    let missing = root.path().join("missing").join("install-ledger.json");

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &missing, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("open ledger directory"),
        "{failure:#}"
    );
    assert!(failure.partial_ledger.files.is_empty());
    assert!(failure.persisted_ledger.is_none());
    assert_no_managed_targets(&units, &bin);
    assert!(!root.path().join("missing").exists());
}

#[test]
fn ledger_colliding_with_a_managed_output_is_refused_before_any_write() {
    let (_root, units, bin) = targets();
    let collision = core_unit_path(&units);

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &collision, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("collides with installation output"),
        "{failure:#}"
    );
    assert!(failure.partial_ledger.files.is_empty());
    assert!(failure.persisted_ledger.is_none());
    assert_no_managed_targets(&units, &bin);
    assert!(
        !collision.exists(),
        "the colliding ledger path must not be created as a managed output"
    );
}

#[test]
fn an_existing_ledger_is_never_overwritten_or_deleted() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);
    fs::write(&ledger_path, b"operator-owned ledger\n").unwrap();

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("refusing to overwrite existing ledger"),
        "{failure:#}"
    );
    assert_eq!(fs::read(&ledger_path).unwrap(), b"operator-owned ledger\n");
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn a_symlinked_ledger_path_is_refused_without_following_it() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);
    let outside = root.path().join("outside-ledger.json");
    fs::write(&outside, b"outside ledger\n").unwrap();
    symlink(&outside, &ledger_path).unwrap();

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("refusing to overwrite existing ledger"),
        "{failure:#}"
    );
    assert!(fs::symlink_metadata(&ledger_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&outside).unwrap(), b"outside ledger\n");
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn a_symlinked_ledger_parent_is_refused() {
    let (root, units, bin) = targets();
    let real = root.path().join("real-ledger");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, Permissions::from_mode(0o700)).unwrap();
    let link = root.path().join("ledger-link");
    symlink(&real, &link).unwrap();
    let ledger_path = link.join("install-ledger.json");

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("open ledger directory"),
        "{failure:#}"
    );
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!real.join("install-ledger.json").exists());
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn a_ledger_parent_that_is_not_private_is_refused_before_any_write() {
    let (root, units, bin) = targets();
    let ledger_dir = root.path().join("shared-ledger");
    fs::create_dir(&ledger_dir).unwrap();
    fs::set_permissions(&ledger_dir, Permissions::from_mode(0o755)).unwrap();
    let ledger_path = ledger_dir.join("install-ledger.json");

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap_err();

    assert!(
        format!("{failure:#}").contains("must be private"),
        "{failure:#}"
    );
    assert!(!ledger_path.exists());
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn the_intent_ledger_is_persisted_before_the_final_ledger_is_published() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);
    let inspector = InspectingPublisher {
        ledger_path: ledger_path.clone(),
        expected_files: 3,
    };

    let report = install_files_with_ledger_and_hook(
        &user_spec(),
        &units,
        &bin,
        &ledger_path,
        true,
        &inspector,
    )
    .unwrap();

    assert!(!report.ledger.partial);
    assert_eq!(report.ledger_path.as_deref(), Some(ledger_path.as_path()));
    assert_eq!(read_ledger(&ledger_path).unwrap(), report.ledger);
}

#[test]
fn final_publish_failure_rolls_back_every_created_file_and_removes_the_intent() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);

    let failure = install_files_with_ledger_and_hook(
        &user_spec(),
        &units,
        &bin,
        &ledger_path,
        true,
        &FailingPublisher,
    )
    .unwrap_err();

    assert!(
        format!("{failure:#}").contains("injected final ledger publish failure"),
        "{failure:#}"
    );
    assert!(failure.partial_ledger.partial);
    assert!(
        failure.partial_ledger.files.is_empty(),
        "matching files created before the failure must be rolled back"
    );
    assert_eq!(failure.rolled_back_files.len(), 3);
    assert!(failure.kept_modified_files.is_empty());
    assert!(
        failure.rollback_errors.is_empty(),
        "{:?}",
        failure.rollback_errors
    );
    assert!(
        failure.persisted_ledger.is_none(),
        "a clean rollback must remove the intent ledger"
    );
    assert!(!ledger_path.exists());
    assert_no_managed_targets(&units, &bin);
    assert_no_managed_temp_files(&units, &bin);

    let json: serde_json::Value =
        serde_json::from_str(&failure.partial_ledger_json().unwrap()).unwrap();
    assert_eq!(json["partial"], true);
    assert_eq!(json["files"].as_array().unwrap().len(), 0);
}

#[test]
fn final_publish_failure_keeps_a_partial_ledger_when_a_created_file_was_modified() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);
    let victim = core_unit_path(&units);
    let corrupting = CorruptingPublisher {
        victim: victim.clone(),
    };

    let failure = install_files_with_ledger_and_hook(
        &user_spec(),
        &units,
        &bin,
        &ledger_path,
        true,
        &corrupting,
    )
    .unwrap_err();

    assert!(!failure.is_clean_rollback());
    assert_eq!(
        failure.kept_modified_files,
        vec![victim.clone()],
        "the modified file must be kept, never deleted"
    );
    assert_eq!(failure.rolled_back_files.len(), 2);
    assert_eq!(failure.partial_ledger.files.len(), 1);
    assert!(failure.partial_ledger.partial);
    assert_eq!(failure.partial_ledger.files[0].path, victim);
    assert_eq!(
        failure.persisted_ledger.as_deref(),
        Some(ledger_path.as_path()),
        "the surviving file must keep ownership evidence"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"modified by the fault-injection hook\n"
    );

    // The persisted partial ledger must be exactly the returned partial ledger
    // and must itself be valid, readable JSON.
    let persisted = read_ledger(&ledger_path).unwrap();
    assert_eq!(persisted, *failure.partial_ledger);
    let json: serde_json::Value =
        serde_json::from_str(&failure.partial_ledger_json().unwrap()).unwrap();
    assert_eq!(json["partial"], true);
    assert_eq!(json["files"].as_array().unwrap().len(), 1);

    // Only the surviving (modified) file remains; the other two were rolled
    // back, and no temporary files were left behind.
    assert!(!updater_unit_path(&units).exists());
    assert!(!core_launcher_path(&bin).exists());
    assert_no_managed_temp_files(&units, &bin);

    // The partial ledger is still a safe whitelist: uninstall refuses because
    // the recorded hash no longer matches, and deletes nothing.
    let error = uninstall_files(&persisted, true, true, None).unwrap_err();
    assert!(format!("{error:#}").contains("modified"), "{error:#}");
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"modified by the fault-injection hook\n"
    );
}

/// Replaces the intent ledger with an operator file, then fails publication.
/// Rollback must not delete that foreign ledger file, even though the managed
/// files it created are cleanly rolled back.
struct ReplacingPublisher {
    ledger_path: PathBuf,
}

impl LedgerPublishHook for ReplacingPublisher {
    fn before_final_publish(&self, path: &Path, _ledger: &OwnershipLedger) -> Result<()> {
        assert_eq!(path, self.ledger_path.as_path());
        fs::remove_file(path).expect("remove our own intent ledger");
        fs::write(path, b"operator replacement ledger\n").expect("write replacement ledger");
        anyhow::bail!("injected final ledger publish failure after replacing the ledger file")
    }
}

#[test]
fn a_foreign_file_at_the_ledger_path_is_never_deleted_during_rollback() {
    let (root, units, bin) = targets();
    let (_ledger_dir, ledger_path) = private_ledger_dir(&root);
    let replacing = ReplacingPublisher {
        ledger_path: ledger_path.clone(),
    };

    let failure = install_files_with_ledger_and_hook(
        &user_spec(),
        &units,
        &bin,
        &ledger_path,
        true,
        &replacing,
    )
    .unwrap_err();

    // Managed files were all rolled back, but the ledger path now holds a
    // foreign file. The identity check must refuse to unlink it.
    assert!(failure.partial_ledger.files.is_empty());
    assert_eq!(failure.rolled_back_files.len(), 3);
    assert!(!failure.rollback_errors.is_empty());
    assert!(
        failure.persisted_ledger.is_none(),
        "the foreign replacement must not be reported as our persisted ledger"
    );
    assert_eq!(
        fs::read(&ledger_path).unwrap(),
        b"operator replacement ledger\n"
    );
    assert_no_managed_targets(&units, &bin);
}

#[test]
fn an_unwritable_ledger_parent_is_refused_before_any_write() {
    // Root bypasses directory permission checks.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (root, units, bin) = targets();
    let (ledger_dir, ledger_path) = private_ledger_dir(&root);
    fs::set_permissions(&ledger_dir, Permissions::from_mode(0o500)).unwrap();

    let failure =
        install_files_with_ledger(&user_spec(), &units, &bin, &ledger_path, true).unwrap_err();

    fs::set_permissions(&ledger_dir, Permissions::from_mode(0o700)).unwrap();

    assert!(
        format!("{failure:#}").contains("create staged file"),
        "{failure:#}"
    );
    assert!(failure.partial_ledger.files.is_empty());
    assert!(failure.persisted_ledger.is_none());
    assert!(!ledger_path.exists());
    assert_no_managed_targets(&units, &bin);
    assert_no_managed_temp_files(&units, &bin);
}

#[test]
fn deployment_launcher_directory_mismatch_is_rejected_before_any_file_is_created() {
    let (root, units, bin) = targets();
    let mut spec = user_spec();
    let state = root.path().join("transactions");
    let runtime = root.path().join("runtime");
    spec.state_dir = state.clone();
    spec.release_root = state.join("current");
    spec.core_config = root.path().join("config/core.toml");
    spec.deployment = Some(install::DeploymentRuntime {
        schema_version: 1,
        core_root: root.path().join("source"),
        panel_root: None,
        launcher_directory: root.path().join("different-bin"),
        release_store: root.path().join("store"),
        instance_state: runtime.clone(),
        core_socket: runtime.join("core.sock"),
        runtime_config: runtime.join("adapter.json"),
        panel_gate_file: None,
        required_modules: vec![],
        acknowledge_external_writers_stopped: false,
    });
    let error = install_files(&spec, &units, &bin, true).unwrap_err();
    assert!(format!("{error:#}").contains("deployment.launcher_directory"));
    assert_eq!(fs::read_dir(&units).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&bin).unwrap().count(), 0);
}
