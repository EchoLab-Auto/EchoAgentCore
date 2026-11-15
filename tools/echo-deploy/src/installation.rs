//! Transactional installation and removal of rendered unit/launcher **files**.
//!
//! This module is deliberately narrower than a deployment transaction:
//!
//! [`crate::install::write_install`] remains a render-only preview writer. This
//! module is the only path in `echo-deploy` that writes rendered files into real
//! target directories.
//!
//! * it installs the [`crate::install::RenderedInstall`] output into explicit,
//!   pre-existing, owner-trusted unit and launcher directories;
//! * it creates new files only (`O_EXCL` + `RENAME_NOREPLACE`); an existing
//!   regular file, symlink, directory or device at any target path is a hard
//!   error and is never replaced;
//! * every successfully published file is recorded in an ownership ledger with
//!   its SHA-256, size, mode and uid/gid; files and directories are fsynced
//!   before success is reported;
//! * [`install_files_with_ledger`] additionally **pre-persists an intent
//!   ledger before the first managed file exists** and only then replaces it
//!   with the final ledger. The ledger path is validated (absolute, private
//!   caller-owned parent, no symlink, no existing entry, no output collision)
//!   before anything is written. If final publication fails, created files are
//!   rolled back; if anything survives, a partial ledger is left at the ledger
//!   path as ownership evidence;
//! * a failure removes only files created by this invocation whose on-disk
//!   hash still matches the ledger entry, and returns the surviving partial
//!   ledger. Files that were modified or replaced after creation are kept;
//! * it never invokes `systemctl`, `systemd`, `kill`, `enable`, `start`,
//!   `daemon-reload` or any other service manager. Installing files is not
//!   activating them: the report status is
//!   [`FILES_INSTALLED_NOT_ACTIVATED`].
//!
//! Uninstall is similarly file-only. It does not know whether a unit is
//! active, so it refuses unless the caller either:
//!
//! * supplies an explicit [`ServiceStopped`] backend that confirms every unit
//!   named in the ledger is stopped; or
//! * passes `offline_only = true`, an explicit operator attestation made after
//!   stopping the services out of band (the CLI flag is `--offline-only`).
//!
//! There is no default proof, no "assume stopped" helper and no shell-out.
//! Uninstall removes only ordinary, single-link files named in the ledger whose
//! content hash, size, mode and uid/gid still match. It never removes
//! directories, configuration files, state/data files, release trees or
//! unrelated files, and it never purges user data.
//!
//! # Five questions
//!
//! 1. **What is the unit of transaction?** One [`InstallSpec`] render, written
//!    to exactly two explicit directories, with one ownership ledger stored at
//!    one explicit absolute path (when using [`install_files_with_ledger`]).
//! 2. **What happens on a collision?** Nothing is written. Target and ledger
//!    collisions, relative/existing/symlinked ledger paths, missing or
//!    non-private ledger parents and ledger/output overlap are all checked
//!    before the intent is persisted and before the first create. Target
//!    publication additionally uses `RENAME_NOREPLACE`, so overwriting is
//!    impossible even under a race.
//! 3. **What happens on failure?** The intent ledger is already durable before
//!    the first managed file exists, so there is no window in which a created
//!    file has no ownership evidence. On error, only files created by this
//!    invocation and still matching their ledger hash are unlinked; any
//!    survivor is recorded in a partial ledger persisted at the ledger path
//!    (or returned in memory if even that write fails). A clean rollback
//!    removes the intent ledger again.
//! 4. **Who may run it?** `system` scope requires effective uid 0 and root-owned
//!    target directories; `user` scope requires target directories owned by the
//!    current effective uid. Group/world-writable target directories, a ledger
//!    parent that is not private (mode `& 0o077 != 0`), a ledger parent owned
//!    by another user and well-known shared roots are refused.
//! 5. **Does this activate anything?** No. No service manager is invoked and
//!    no config/data path from the spec is touched.
//!
//! # Four boundaries
//!
//! * **Files, not services.** Activation/enable/start belongs to the deployment
//!   transaction, not here.
//! * **New files, no overwrite.** The target directories are pinned with
//!   descriptor-relative, no-symlink opens; every target is created with
//!   `O_EXCL` and published with `RENAME_NOREPLACE`. An existing ledger or an
//!   existing target is never replaced.
//! * **Intent first, final ledger last.** A partial ledger listing the planned
//!   paths/hashes/modes/owners is fsynced before the first managed file is
//!   created, and atomically replaced by the final ledger only after every file
//!   and directory is fsynced. This is deliberately **not** a fake
//!   all-or-nothing transaction: a crash can leave a partial install, but it
//!   cannot leave a managed file outside the ownership evidence.
//! * **Ledger whitelist and explicit proof, never purge.** Uninstall deletes
//!   only ordinary single-link files whose recorded hash/size/mode/uid/gid
//!   still match, never directories or user data, and only after a
//!   [`ServiceStopped`] backend or `offline_only` attestation.
//!   must explicitly opt into offline-only removal.

use crate::{
    install::{
        render_install, validate_absolute_path, InstallScope, InstallSpec, InstallTopology,
        RenderedInstall, ACTIVATION_SUPPORTED,
    },
    model::digest,
    safe_fs::Dir,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fmt,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    mem::MaybeUninit,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Schema version of [`OwnershipLedger`] and the reports in this module.
pub const INSTALLATION_SCHEMA_VERSION: u32 = 1;

/// The only successful installation status. Files are placed on disk but no
/// service has been enabled, started or reloaded.
pub const FILES_INSTALLED_NOT_ACTIVATED: &str = "files_installed_not_activated";

/// Successful uninstall status: managed files were removed, services were not
/// touched (because this module has no service backend).
pub const FILES_REMOVED_NOT_DEACTIVATED: &str = "managed_files_removed_services_not_touched";

/// Maximum size of a generated unit or launcher. Existing files above this
/// bound are refused instead of being read into memory.
pub const MAX_MANAGED_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum size of an ownership ledger JSON document.
pub const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;

const UNIT_MODE: u32 = 0o644;
const LAUNCHER_MODE: u32 = 0o755;
const MAX_FILES: usize = 64;
const MAX_UNITS: usize = 16;
const TEMP_ATTEMPTS: usize = 32;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Kind of file recorded in an [`OwnershipLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedFileKind {
    Unit,
    Launcher,
}

/// One file owned by an installation ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedFile {
    /// Absolute final path. Its parent must be the ledger's unit or launcher
    /// directory and its file name must be one of the expected managed names.
    pub path: PathBuf,
    pub kind: ManagedFileKind,
    /// Lowercase SHA-256 of the exact file contents at installation time.
    pub sha256: String,
    pub size: u64,
    /// Permission bits only (for example `0o644` or `0o755`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Files installed by one invocation. This is also the uninstall whitelist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnershipLedger {
    pub schema_version: u32,
    /// Always [`FILES_INSTALLED_NOT_ACTIVATED`], including partial ledgers.
    pub outcome: String,
    /// True only for a ledger returned on a failed install. A partial ledger
    /// contains files that survived rollback (for example because they were
    /// modified after creation).
    pub partial: bool,
    pub instance_id: String,
    pub scope: InstallScope,
    pub topology: InstallTopology,
    pub unit_directory: PathBuf,
    pub launcher_directory: PathBuf,
    /// Expected unit identities, in deterministic order. Uninstall uses these
    /// names only for the stopped-service proof; it never enables/disables
    /// units.
    pub units: Vec<String>,
    pub files: Vec<OwnedFile>,
}

impl OwnershipLedger {
    fn skeleton(spec: &InstallSpec, unit_directory: &Path, launcher_directory: &Path) -> Self {
        Self {
            schema_version: INSTALLATION_SCHEMA_VERSION,
            outcome: FILES_INSTALLED_NOT_ACTIVATED.to_owned(),
            partial: false,
            instance_id: spec.instance_id.clone(),
            scope: spec.scope,
            topology: spec.topology,
            unit_directory: unit_directory.to_path_buf(),
            launcher_directory: launcher_directory.to_path_buf(),
            units: expected_unit_names(&spec.instance_id, spec.topology),
            files: Vec::new(),
        }
    }

    /// Paths in the ledger, in ledger order.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|file| file.path.as_path())
    }

    /// Validate the ledger shape and whitelist. This is a pure check; it does
    /// not touch the filesystem.
    pub fn validate(&self) -> Result<()> {
        validate_ledger(self)
    }
}

/// Successful installation report. It intentionally says "files installed",
/// not "service activated".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationReport {
    pub schema_version: u32,
    /// Exactly [`FILES_INSTALLED_NOT_ACTIVATED`].
    pub status: String,
    pub activation_supported: bool,
    pub services_started: bool,
    pub systemctl_invoked: bool,
    pub ledger: OwnershipLedger,
    /// Absolute ledger path when the ledger was persisted by
    /// [`install_files_with_ledger`]. `None` for the render-and-return
    /// [`install_files`] variant, which does not persist anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_path: Option<PathBuf>,
    pub created_files: Vec<PathBuf>,
    pub notes: Vec<String>,
}

/// Typed failure with the partial ledger required for diagnostics and later
/// safe cleanup.
#[derive(Debug)]
pub struct InstallFailure {
    pub error: anyhow::Error,
    /// Partial ledger. `partial == true`; `files` contains files that survived
    /// rollback (normally empty after a clean rollback).
    pub partial_ledger: Box<OwnershipLedger>,
    /// Absolute path of a partial ledger still on disk, if any. Present when
    /// rollback kept a created file (modified after creation) or could not
    /// verify/remove one, so the surviving files still have ownership evidence.
    /// A clean rollback removes the intent ledger and leaves `None`.
    ///
    /// If rewriting the file with only the surviving entries also failed, this
    /// points at the pre-persisted intent, which is a **superset** of the
    /// survivors (it lists every planned file); `rollback_errors` records the
    /// failed rewrite. Uninstall reports absent entries as `already_absent`.
    pub persisted_ledger: Option<PathBuf>,
    /// Files created by this invocation and removed by rollback.
    pub rolled_back_files: Vec<PathBuf>,
    /// Files created by this invocation but kept because their hash no longer
    /// matched the planned content.
    pub kept_modified_files: Vec<PathBuf>,
    /// Rollback problems. A non-empty vector means the partial ledger may still
    /// own files that could not be inspected or removed.
    pub rollback_errors: Vec<String>,
}

impl InstallFailure {
    fn new(error: anyhow::Error, mut partial_ledger: OwnershipLedger) -> Self {
        partial_ledger.partial = true;
        Self {
            error,
            partial_ledger: Box::new(partial_ledger),
            persisted_ledger: None,
            rolled_back_files: Vec::new(),
            kept_modified_files: Vec::new(),
            rollback_errors: Vec::new(),
        }
    }

    pub fn is_clean_rollback(&self) -> bool {
        self.kept_modified_files.is_empty() && self.rollback_errors.is_empty()
    }

    /// The partial ledger as pretty JSON, suitable for a CLI error report.
    pub fn partial_ledger_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self.partial_ledger.as_ref())
    }
}

impl fmt::Display for InstallFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:#} (partial ledger: {} file(s) remain, {} rolled back, {} kept modified, {} rollback error(s)",
            self.error,
            self.partial_ledger.files.len(),
            self.rolled_back_files.len(),
            self.kept_modified_files.len(),
            self.rollback_errors.len()
        )?;
        match &self.persisted_ledger {
            Some(path) => write!(formatter, ", persisted at {path:?})"),
            None => write!(formatter, ")"),
        }
    }
}

impl std::error::Error for InstallFailure {}

/// Fault-injection seam for the **final** ownership-ledger publication.
///
/// [`install_files_with_ledger_and_hook`] calls
/// [`LedgerPublishHook::before_final_publish`] after every managed file and
/// directory has been created and fsynced, but before the intent ledger is
/// atomically replaced by the final ledger. Returning an error aborts the
/// publication and runs the normal rollback path; tests use this to prove that
/// a failed final ledger write cannot leave unowned files behind.
///
/// The default [`NoLedgerPublishHook`] does nothing, so production behaviour is
/// exactly "publish the final ledger atomically".
pub trait LedgerPublishHook {
    fn before_final_publish(&self, ledger_path: &Path, ledger: &OwnershipLedger) -> Result<()>;
}

/// Default [`LedgerPublishHook`]: no injected failure.
pub struct NoLedgerPublishHook;

impl LedgerPublishHook for NoLedgerPublishHook {
    fn before_final_publish(&self, _ledger_path: &Path, _ledger: &OwnershipLedger) -> Result<()> {
        Ok(())
    }
}

/// Explicit, external proof that all units named by a ledger are stopped.
///
/// There is deliberately no built-in implementation: this module has no
/// service manager backend and must not pretend to know active state. Tests can
/// provide a stub; CLI wiring can provide an adapter owned by the deployment
/// transaction. Returning an error or `false` makes uninstall refuse before any
/// file is deleted.
pub trait ServiceStopped {
    fn services_stopped(&self, units: &[String]) -> Result<bool>;
}

/// Successful uninstall report. No service was stopped, started, disabled or
/// reloaded by this operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallReport {
    pub schema_version: u32,
    /// Exactly [`FILES_REMOVED_NOT_DEACTIVATED`].
    pub status: String,
    pub offline_only: bool,
    /// `"offline_only"` or `"service_stopped_backend"`.
    pub proof: String,
    pub removed_files: Vec<PathBuf>,
    pub already_absent: Vec<PathBuf>,
    /// Always empty: this module never removes directories.
    pub removed_directories: Vec<PathBuf>,
    pub systemctl_invoked: bool,
    pub notes: Vec<String>,
}

struct PlannedFile {
    dir_index: usize,
    name: String,
    kind: ManagedFileKind,
    contents: Vec<u8>,
    mode: u32,
    sha256: String,
    size: u64,
    path: PathBuf,
}

struct CreatedFile {
    file: OwnedFile,
    dir_index: usize,
}

struct PendingRemoval {
    dir_index: usize,
    name: String,
    path: PathBuf,
}

struct RollbackOutcome {
    remaining: Vec<OwnedFile>,
    rolled_back: Vec<PathBuf>,
    kept_modified: Vec<PathBuf>,
    errors: Vec<String>,
}

/// Install rendered unit and launcher files into two explicit, pre-existing
/// directories.
///
/// The directories are opened with descriptor-relative `O_NOFOLLOW` traversal,
/// checked for the scope's owner and against group/world-writable modes, and
/// pinned for the whole transaction. The function never creates directories and
/// never overwrites anything. On success the returned [`InstallationReport`] says
/// [`FILES_INSTALLED_NOT_ACTIVATED`].
pub fn install_files(
    spec: &InstallSpec,
    unit_directory: &Path,
    launcher_directory: &Path,
    confirm: bool,
) -> std::result::Result<InstallationReport, InstallFailure> {
    let ledger = OwnershipLedger::skeleton(spec, unit_directory, launcher_directory);
    let prepared = prepare_install(spec, unit_directory, launcher_directory, confirm);
    let (dirs, plan) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(InstallFailure::new(error, ledger)),
    };

    let mut created: Vec<CreatedFile> = Vec::with_capacity(plan.len());
    let result = (|| -> Result<()> {
        for planned in &plan {
            let file = create_managed_file(&dirs[planned.dir_index], planned)?;
            created.push(file);
        }
        sync_directories(&dirs).context("fsync installation directories")?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            let mut ledger = ledger;
            ledger.files = created.iter().map(|created| created.file.clone()).collect();
            let created_files = ledger.files.iter().map(|file| file.path.clone()).collect();
            Ok(InstallationReport {
                schema_version: INSTALLATION_SCHEMA_VERSION,
                status: FILES_INSTALLED_NOT_ACTIVATED.to_owned(),
                activation_supported: ACTIVATION_SUPPORTED,
                services_started: false,
                systemctl_invoked: false,
                ledger,
                ledger_path: None,
                created_files,
                notes: vec![
                    "files were installed only; no systemctl/systemd operation was performed"
                        .to_owned(),
                    "installing files is not service activation; enable/start/reload belongs to the deployment transaction"
                        .to_owned(),
                    "the ownership ledger is the whitelist for a later uninstall; it was not persisted by this function"
                        .to_owned(),
                ],
            })
        }
        Err(error) => {
            let mut rollback = rollback_created(&dirs, &created);
            let mut partial = ledger;
            partial.files = std::mem::take(&mut rollback.remaining);
            partial.partial = true;
            let mut failure = InstallFailure::new(error, partial);
            failure.rolled_back_files = rollback.rolled_back;
            failure.kept_modified_files = rollback.kept_modified;
            failure.rollback_errors = rollback.errors;
            Err(failure)
        }
    }
}

/// Install rendered unit/launcher files **and persist the ownership ledger**
/// at an explicit absolute path, with no window in which a created file has no
/// ownership evidence.
///
/// Protocol, in order:
///
/// 1. Validate the spec, the two existing target directories and every target
///    collision *without writing anything*.
/// 2. Validate the ledger destination: absolute, normalized, UTF-8, a valid
///    single file name, an existing **private** parent directory (mode
///    `& 0o077 == 0`) owned by the current effective uid (root for `system`
///    scope), no symlink in any ledger path component, no existing entry at
///    the ledger path (regular file, directory, symlink or dangling symlink),
///    and no overlap with any planned output or target directory.
/// 3. Build the planned (intent) ledger from the rendered hashes/sizes/modes
///    and expected owners, and create it at the ledger path with `O_EXCL` +
///    `RENAME_NOREPLACE`, fsyncing both the file and its directory.
/// 4. Create every managed file with `O_EXCL` + `RENAME_NOREPLACE`, fsync each
///    file, then fsync both target directories.
/// 5. Call [`LedgerPublishHook::before_final_publish`] (no-op by default), then
///    atomically replace the intent with the final ledger (verified to be the
///    exact file this invocation created) and fsync the ledger directory.
///
/// This is deliberately **not** an all-or-nothing transaction across files. On
/// any error after the intent exists, files created by this invocation whose
/// on-disk hash/size still match are removed. If a survivor remains (or could
/// not be verified), the intent/partial ledger is kept at `ledger_path`, its
/// path is returned in [`InstallFailure::persisted_ledger`], and the surviving
/// entries are returned in [`InstallFailure::partial_ledger`] (JSON-printable
/// via [`InstallFailure::partial_ledger_json`]). A clean rollback removes the
/// intent ledger again. No pre-existing file or directory is ever deleted or
/// replaced.
///
/// Validation failures happen before any write, so a bad ledger path cannot
/// leave target files behind.
pub fn install_files_with_ledger(
    spec: &InstallSpec,
    unit_directory: &Path,
    launcher_directory: &Path,
    ledger_path: &Path,
    confirm: bool,
) -> std::result::Result<InstallationReport, InstallFailure> {
    install_files_with_ledger_and_hook(
        spec,
        unit_directory,
        launcher_directory,
        ledger_path,
        confirm,
        &NoLedgerPublishHook,
    )
}

/// Same as [`install_files_with_ledger`], with an explicit final-publication
/// hook for fault injection. Production callers use
/// [`install_files_with_ledger`] and [`NoLedgerPublishHook`].
pub fn install_files_with_ledger_and_hook(
    spec: &InstallSpec,
    unit_directory: &Path,
    launcher_directory: &Path,
    ledger_path: &Path,
    confirm: bool,
    hook: &dyn LedgerPublishHook,
) -> std::result::Result<InstallationReport, InstallFailure> {
    let skeleton = OwnershipLedger::skeleton(spec, unit_directory, launcher_directory);

    // Phase 0: every check that can fail without writing anything. The ledger
    // parent is opened once here and reused, so the trust decision and the file
    // creation cannot be separated by a path swap.
    let prepared = prepare_install(spec, unit_directory, launcher_directory, confirm).and_then(
        |(dirs, plan)| {
            let (ledger_parent, ledger_leaf) = open_ledger_destination(ledger_path, spec.scope)?;
            validate_ledger_not_output(ledger_path, unit_directory, launcher_directory, &plan)?;
            let intent = planned_ledger(&skeleton, &dirs, &plan)?;
            Ok((dirs, plan, ledger_parent, ledger_leaf, intent))
        },
    );
    let (dirs, plan, ledger_parent, ledger_leaf, intent) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(InstallFailure::new(error, skeleton)),
    };

    // Phase 1: persist the intent before the first managed file can exist.
    let mut slot = match LedgerSlot::create(ledger_parent, ledger_path, ledger_leaf, &intent) {
        Ok(slot) => slot,
        Err(error) => return Err(InstallFailure::new(error, skeleton)),
    };

    // Phase 2: create and fsync every managed file. Until the final ledger is
    // published, the on-disk intent is the ownership evidence for these files.
    let mut created: Vec<CreatedFile> = Vec::with_capacity(plan.len());
    let result = (|| -> Result<()> {
        for planned in &plan {
            created.push(create_managed_file(&dirs[planned.dir_index], planned)?);
        }
        sync_directories(&dirs).context("fsync installation directories")?;
        Ok(())
    })();
    if let Err(error) = result {
        return Err(finish_install_failure(
            error, skeleton, &dirs, &created, &mut slot,
        ));
    }

    // Phase 3: publish the final ledger atomically over our own intent.
    let mut final_ledger = intent;
    final_ledger.partial = false;
    final_ledger.files = created.iter().map(|created| created.file.clone()).collect();
    let published = hook
        .before_final_publish(ledger_path, &final_ledger)
        .context("final ownership-ledger publish hook failed")
        .and_then(|()| {
            slot.replace(&final_ledger)
                .context("publish final ownership ledger")
        });
    if let Err(error) = published {
        return Err(finish_install_failure(
            error, skeleton, &dirs, &created, &mut slot,
        ));
    }

    let created_files = final_ledger
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    Ok(InstallationReport {
        schema_version: INSTALLATION_SCHEMA_VERSION,
        status: FILES_INSTALLED_NOT_ACTIVATED.to_owned(),
        activation_supported: ACTIVATION_SUPPORTED,
        services_started: false,
        systemctl_invoked: false,
        ledger: final_ledger,
        ledger_path: Some(ledger_path.to_path_buf()),
        created_files,
        notes: vec![
            "files were installed only; no systemctl/systemd operation was performed".to_owned(),
            "installing files is not service activation; enable/start/reload belongs to the deployment transaction".to_owned(),
            "an intent ledger was fsynced at the ledger path before the first managed file was created and atomically replaced by the final ledger".to_owned(),
            "the ownership ledger is the whitelist for a later uninstall".to_owned(),
        ],
    })
}

/// Roll back everything created by this invocation and leave the strongest
/// available ownership evidence on disk.
///
/// * Nothing survives and rollback is clean: remove the intent ledger (verified
///   by device/inode, so a foreign file at the ledger path is never deleted).
/// * Something survives: atomically rewrite the ledger with only the surviving
///   entries. If even that rewrite fails, the original intent (a superset of
///   the surviving files) stays in place and the error is recorded.
fn finish_install_failure(
    error: anyhow::Error,
    skeleton: OwnershipLedger,
    dirs: &[Dir],
    created: &[CreatedFile],
    slot: &mut LedgerSlot,
) -> InstallFailure {
    let mut rollback = rollback_created(dirs, created);
    let mut partial = skeleton;
    partial.files = std::mem::take(&mut rollback.remaining);
    partial.partial = true;

    let mut failure = InstallFailure::new(error, partial);
    failure.rolled_back_files = rollback.rolled_back;
    failure.kept_modified_files = rollback.kept_modified;
    failure.rollback_errors = rollback.errors;

    if failure.partial_ledger.files.is_empty() && failure.rollback_errors.is_empty() {
        match slot.remove() {
            Ok(()) => {}
            Err(remove_error) => {
                if slot.path_still_names_our_file() {
                    failure.persisted_ledger = Some(slot.path.clone());
                }
                failure.rollback_errors.push(format!(
                    "could not remove intent ledger {:?}: {remove_error:#}",
                    slot.path
                ));
            }
        }
        return failure;
    }

    failure.persisted_ledger = Some(slot.path.clone());
    if let Err(replace_error) = slot.replace(&failure.partial_ledger) {
        if !slot.path_still_names_our_file() {
            failure.persisted_ledger = None;
        }
        failure.rollback_errors.push(format!(
            "could not rewrite partial ledger {:?} with surviving files: {replace_error:#}",
            slot.path
        ));
    }
    failure
}

/// Remove only regular files named by the ownership ledger whose contents
/// still match the recorded hash/size/mode/uid/gid.
///
/// # Service proof
///
/// Because a ledger records files, not active state, this function refuses to
/// delete anything when `offline_only == false` and `services_stopped` is
/// `None`. Callers must choose one explicit proof:
///
/// * pass a [`ServiceStopped`] implementation and receive `true`, or
/// * pass `offline_only = true` after stopping services out of band
///   (CLI `--offline-only`).
///
/// The whole uninstall is preflighted before the first unlink: one modified,
/// replaced, missing-directory or non-regular ledger target aborts the
/// operation with no files removed. Missing entries are reported as
/// `already_absent` and do not fail an otherwise valid uninstall.
pub fn uninstall_files(
    ledger: &OwnershipLedger,
    confirm: bool,
    offline_only: bool,
    services_stopped: Option<&dyn ServiceStopped>,
) -> Result<UninstallReport> {
    ensure!(confirm, "uninstall requires explicit confirmation");
    validate_ledger(ledger)?;

    let proof = match services_stopped {
        Some(backend) => {
            let stopped = backend
                .services_stopped(&ledger.units)
                .context("service-stopped backend failed")?;
            ensure!(
                stopped,
                "refusing uninstall: service-stopped backend did not prove all units stopped"
            );
            "service_stopped_backend"
        }
        None => {
            ensure!(
                offline_only,
                "refusing uninstall: unit active state is unknown; pass offline_only=true only after stopping services, or provide a ServiceStopped backend"
            );
            "offline_only"
        }
    };

    validate_managed_directory_location(&ledger.unit_directory, "unit_directory")?;
    validate_managed_directory_location(&ledger.launcher_directory, "launcher_directory")?;
    let unit_dir = open_trusted_directory(&ledger.unit_directory, ledger.scope, "unit_directory")?;
    let launcher_dir = open_trusted_directory(
        &ledger.launcher_directory,
        ledger.scope,
        "launcher_directory",
    )?;
    let dirs = vec![unit_dir, launcher_dir];

    let mut pending: Vec<PendingRemoval> = Vec::with_capacity(ledger.files.len());
    let mut already_absent = Vec::new();

    // Preflight every entry before deleting anything. This is the core safety
    // property: a modified file cannot cause a partially deleted installation.
    for entry in &ledger.files {
        let dir_index = match entry.kind {
            ManagedFileKind::Unit => 0,
            ManagedFileKind::Launcher => 1,
        };
        let dir = &dirs[dir_index];
        let name = file_name(&entry.path)?;
        match stat_at(dir, name)? {
            None => {
                already_absent.push(entry.path.clone());
                continue;
            }
            Some(metadata) => {
                ensure!(
                    metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
                    "refusing to remove non-regular ledger target {:?}",
                    entry.path
                );
            }
        }

        let mut file = dir
            .open_file(name, false)
            .with_context(|| format!("open managed file {:?}", entry.path))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "refusing to remove non-ordinary ledger target {:?}",
            entry.path
        );
        ensure!(
            metadata.len() == entry.size,
            "refusing to remove modified managed file {:?}: size changed",
            entry.path
        );
        ensure!(
            metadata.mode() & 0o7777 == entry.mode,
            "refusing to remove modified managed file {:?}: mode changed",
            entry.path
        );
        ensure!(
            metadata.uid() == entry.uid && metadata.gid() == entry.gid,
            "refusing to remove modified managed file {:?}: owner changed",
            entry.path
        );
        let (hash, size) = hash_reader(&mut file)?;
        ensure!(
            size == entry.size && hash == entry.sha256,
            "refusing to remove modified managed file {:?}: content hash changed",
            entry.path
        );

        pending.push(PendingRemoval {
            dir_index,
            name: name.to_owned(),
            path: entry.path.clone(),
        });
    }

    let mut removed_files = Vec::with_capacity(pending.len());
    for item in &pending {
        let dir = &dirs[item.dir_index];
        unlink_file(dir, &item.name)
            .with_context(|| format!("remove managed file {:?}", item.path))?;
        removed_files.push(item.path.clone());
    }
    sync_directories(&dirs).context("fsync uninstall directories")?;

    Ok(UninstallReport {
        schema_version: INSTALLATION_SCHEMA_VERSION,
        status: FILES_REMOVED_NOT_DEACTIVATED.to_owned(),
        offline_only,
        proof: proof.to_owned(),
        removed_files,
        already_absent,
        removed_directories: Vec::new(),
        systemctl_invoked: false,
        notes: vec![
            "only regular files named in the ownership ledger were removed".to_owned(),
            "directories, configuration, state, data and release paths were not removed or purged"
                .to_owned(),
            "no systemctl/systemd operation was performed; service activation state was not changed"
                .to_owned(),
        ],
    })
}

/// Convenience for CLI `--offline-only`: require confirmation, no backend.
pub fn uninstall_offline_files(ledger: &OwnershipLedger, confirm: bool) -> Result<UninstallReport> {
    uninstall_files(ledger, confirm, true, None)
}

/// Open and validate the ledger's parent directory. The returned [`Dir`] is
/// pinned (no symlink in any component) and reused for all ledger writes, so
/// the trust checks and the actual writes refer to the same directory.
fn open_ledger_destination(path: &Path, scope: InstallScope) -> Result<(Dir, String)> {
    validate_absolute_path(path, "ledger path")?;
    let leaf = file_name(path)?.to_owned();
    validate_file_name(&leaf)?;
    crate::model::safe_id(&leaf).with_context(|| format!("unsafe ledger file name {leaf:?}"))?;
    let parent_path = path.parent().context("ledger path must have a parent")?;
    let parent = Dir::absolute(parent_path)
        .with_context(|| format!("open ledger directory {parent_path:?}"))?;
    let metadata = parent.0.metadata()?;
    ensure!(
        metadata.is_dir(),
        "ledger parent {parent_path:?} must be a directory"
    );
    let mode = metadata.mode() & 0o7777;
    ensure!(
        mode & 0o077 == 0,
        "ledger directory {parent_path:?} must be private (no group/other permissions; mode {mode:04o})"
    );
    let euid = unsafe { libc::geteuid() };
    ensure!(
        metadata.uid() == euid,
        "ledger directory {parent_path:?} must be owned by the current effective uid {euid}"
    );
    if scope == InstallScope::System {
        ensure!(euid == 0, "system scope requires effective uid 0");
    }
    ensure!(
        !entry_exists(&parent, &leaf)?,
        "refusing to overwrite existing ledger {path:?}"
    );
    Ok((parent, leaf))
}

/// Refuse a ledger path that is (or names) a managed output or target
/// directory. This is a pure path check performed before the intent is written.
fn validate_ledger_not_output(
    ledger_path: &Path,
    unit_directory: &Path,
    launcher_directory: &Path,
    plan: &[PlannedFile],
) -> Result<()> {
    ensure!(
        ledger_path != unit_directory && ledger_path != launcher_directory,
        "ledger path {ledger_path:?} must not be a managed target directory"
    );
    for planned in plan {
        ensure!(
            ledger_path != planned.path,
            "ledger path {ledger_path:?} collides with installation output {:?}",
            planned.path
        );
    }
    Ok(())
}

/// Build the intent ledger: the exact planned whitelist, before any file is
/// created. Owner/group are the values a newly created file is expected to
/// receive (`euid`, and the pinned directory's gid when it has setgid).
fn planned_ledger(
    skeleton: &OwnershipLedger,
    dirs: &[Dir],
    plan: &[PlannedFile],
) -> Result<OwnershipLedger> {
    let euid = unsafe { libc::geteuid() } as u32;
    let egid = unsafe { libc::getegid() } as u32;
    let mut ledger = skeleton.clone();
    ledger.partial = true;
    let mut files = Vec::with_capacity(plan.len());
    for planned in plan {
        let dir_metadata = dirs[planned.dir_index].0.metadata()?;
        let gid = if dir_metadata.mode() & libc::S_ISGID != 0 {
            dir_metadata.gid()
        } else {
            egid
        };
        files.push(OwnedFile {
            path: planned.path.clone(),
            kind: planned.kind,
            sha256: planned.sha256.clone(),
            size: planned.size,
            mode: planned.mode,
            uid: euid,
            gid,
        });
    }
    ledger.files = files;
    ledger.validate().context("planned intent ledger")?;
    Ok(ledger)
}

/// A ledger file created by this invocation. Device/inode identity is pinned so
/// replacement and cleanup can never touch a foreign file at the same path.
struct LedgerSlot {
    dir: Dir,
    path: PathBuf,
    leaf: String,
    identity: (u64, u64),
    /// Open descriptor to the current ledger inode. Keeping it open prevents
    /// the kernel from reusing that inode number after an unlink, so a foreign
    /// replacement can never masquerade as our ledger through inode reuse.
    pinned: File,
}

impl LedgerSlot {
    fn create(dir: Dir, path: &Path, leaf: String, ledger: &OwnershipLedger) -> Result<Self> {
        let bytes = ledger_bytes(ledger)?;
        ensure!(
            !entry_exists(&dir, &leaf)?,
            "refusing to overwrite existing ledger {path:?}"
        );
        let (mut temp, mut file) = open_temp(&dir, &leaf)?;
        write_ledger_contents(&mut file, &bytes, path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "staged ledger {path:?} is not a new regular file"
        );
        let identity = (metadata.dev(), metadata.ino());
        temp.publish(&leaf)?;
        drop(temp);
        dir.sync()
            .with_context(|| format!("fsync ledger directory of {path:?}"))?;
        Ok(Self {
            dir,
            path: path.to_path_buf(),
            leaf,
            identity,
            pinned: file,
        })
    }

    fn replace(&mut self, ledger: &OwnershipLedger) -> Result<()> {
        let bytes = ledger_bytes(ledger)?;
        self.ensure_current()?;
        let (mut temp, mut file) = open_temp(&self.dir, &self.leaf)?;
        write_ledger_contents(&mut file, &bytes, &self.path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "staged ledger {:?} is not a new regular file",
            self.path
        );
        let identity = (metadata.dev(), metadata.ino());
        temp.publish_replace(&self.leaf)?;
        self.identity = identity;
        self.pinned = file;
        self.dir
            .sync()
            .with_context(|| format!("fsync ledger directory of {:?}", self.path))?;
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        self.ensure_current()?;
        unlink_file(&self.dir, &self.leaf)?;
        self.dir
            .sync()
            .with_context(|| format!("fsync ledger directory of {:?}", self.path))?;
        Ok(())
    }

    /// True when the ledger path still names the exact regular-file inode this
    /// slot created. Used to report persisted evidence truthfully after a failed
    /// rewrite or cleanup attempt.
    fn path_still_names_our_file(&self) -> bool {
        match stat_at(&self.dir, &self.leaf) {
            Ok(Some(stat)) => {
                stat.st_mode & libc::S_IFMT == libc::S_IFREG
                    && ((stat.st_dev, stat.st_ino) == self.identity)
            }
            _ => false,
        }
    }

    fn ensure_current(&self) -> Result<()> {
        let stat = stat_at(&self.dir, &self.leaf)?
            .with_context(|| format!("ledger {:?} disappeared", self.path))?;
        let pinned = self.pinned.metadata()?;
        ensure!(
            (pinned.dev(), pinned.ino()) == self.identity,
            "internal ledger pin for {:?} no longer matches its recorded identity",
            self.path
        );
        ensure!(
            stat.st_mode & libc::S_IFMT == libc::S_IFREG
                && stat.st_nlink == 1
                && ((stat.st_dev, stat.st_ino) == self.identity),
            "ledger {:?} is no longer the file created by this invocation; refusing to replace or remove it",
            self.path
        );
        Ok(())
    }
}

fn ledger_bytes(ledger: &OwnershipLedger) -> Result<Vec<u8>> {
    ledger.validate().context("invalid ownership ledger")?;
    let mut bytes = serde_json::to_vec_pretty(ledger).context("serialize ownership ledger")?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "ownership ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    Ok(bytes)
}

fn write_ledger_contents(file: &mut File, bytes: &[u8], path: &Path) -> Result<()> {
    file.write_all(bytes)
        .with_context(|| format!("write ownership ledger {path:?}"))?;
    file.set_permissions(PermissionsExt::from_mode(0o600))
        .with_context(|| format!("set mode on ownership ledger {path:?}"))?;
    file.sync_all()
        .with_context(|| format!("fsync ownership ledger {path:?}"))?;
    Ok(())
}

/// Persist a ledger into a **new absolute path** (`O_EXCL` + `RENAME_NOREPLACE`).
pub fn write_ledger_new(path: &Path, ledger: &OwnershipLedger) -> Result<()> {
    validate_ledger(ledger)?;
    validate_absolute_path(path, "ledger path")?;
    let parent_path = path.parent().context("ledger path must have a parent")?;
    let leaf = file_name(path)?;
    validate_file_name(leaf)?;
    let parent = Dir::absolute(parent_path)
        .with_context(|| format!("open ledger directory {parent_path:?}"))?;
    ensure!(
        !entry_exists(&parent, leaf)?,
        "refusing to overwrite existing ledger {path:?}"
    );

    let mut bytes = serde_json::to_vec_pretty(ledger).context("serialize ownership ledger")?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "ownership ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    let (mut temp, mut file) = open_temp(&parent, leaf)?;
    file.write_all(&bytes)
        .with_context(|| format!("write ledger {path:?}"))?;
    file.set_permissions(PermissionsExt::from_mode(0o600))?;
    file.sync_all().context("fsync ledger")?;
    temp.publish(leaf)?;
    parent.sync().context("fsync ledger directory")?;
    Ok(())
}

/// Read and validate an ownership ledger from an absolute path. The file must
/// be an ordinary single-link file opened without following symlinks.
pub fn read_ledger(path: &Path) -> Result<OwnershipLedger> {
    validate_absolute_path(path, "ledger path")?;
    let parent_path = path.parent().context("ledger path must have a parent")?;
    let leaf = file_name(path)?;
    let parent = Dir::absolute(parent_path)
        .with_context(|| format!("open ledger directory {parent_path:?}"))?;
    let file = parent
        .open_file(leaf, false)
        .with_context(|| format!("open ledger {path:?}"))?;
    ensure!(
        file.metadata()?.len() <= MAX_LEDGER_BYTES,
        "ownership ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read ownership ledger")?;
    ensure!(
        bytes.len() as u64 <= MAX_LEDGER_BYTES,
        "ownership ledger exceeds {MAX_LEDGER_BYTES} bytes"
    );
    let ledger: OwnershipLedger =
        serde_json::from_slice(&bytes).context("parse ownership ledger")?;
    validate_ledger(&ledger)?;
    Ok(ledger)
}

fn prepare_install(
    spec: &InstallSpec,
    unit_directory: &Path,
    launcher_directory: &Path,
    confirm: bool,
) -> Result<(Vec<Dir>, Vec<PlannedFile>)> {
    ensure!(confirm, "installation requires explicit confirmation");
    spec.validate().context("invalid install spec")?;
    if let Some(deployment) = &spec.deployment {
        ensure!(
            deployment.launcher_directory == launcher_directory,
            "launcher_directory must match deployment.launcher_directory used by unit ExecStart"
        );
    }
    validate_absolute_path(unit_directory, "unit_directory")?;
    validate_absolute_path(launcher_directory, "launcher_directory")?;
    validate_managed_directory_location(unit_directory, "unit_directory")?;
    validate_managed_directory_location(launcher_directory, "launcher_directory")?;
    validate_destination_not_protected(spec, unit_directory, "unit_directory")?;
    validate_destination_not_protected(spec, launcher_directory, "launcher_directory")?;

    let unit_dir = open_trusted_directory(unit_directory, spec.scope, "unit_directory")?;
    let launcher_dir =
        open_trusted_directory(launcher_directory, spec.scope, "launcher_directory")?;
    let dirs = vec![unit_dir, launcher_dir];
    let rendered = render_install(spec).context("render installation files")?;
    let plan = plan_files(&rendered, unit_directory, launcher_directory)?;

    let mut targets = BTreeSet::new();
    for planned in &plan {
        ensure!(
            targets.insert(planned.path.clone()),
            "duplicate installation target {:?}",
            planned.path
        );
        ensure_protected_file_not_targeted(spec, &planned.path)?;
        ensure!(
            !entry_exists(&dirs[planned.dir_index], &planned.name)?,
            "refusing to overwrite existing target {:?}",
            planned.path
        );
    }
    Ok((dirs, plan))
}

fn plan_files(
    rendered: &RenderedInstall,
    unit_directory: &Path,
    launcher_directory: &Path,
) -> Result<Vec<PlannedFile>> {
    let names = &rendered.unit_names;
    let mut plan = Vec::with_capacity(rendered.files.len());
    for file in &rendered.files {
        let is_unit = file.name == names.core
            || names.panel.as_deref() == Some(file.name.as_str())
            || file.name == names.updater;
        let is_launcher = file.name == names.core_launcher
            || names.panel_launcher.as_deref() == Some(file.name.as_str());
        let (kind, dir_index, directory, mode) = if is_unit {
            (ManagedFileKind::Unit, 0, unit_directory, UNIT_MODE)
        } else if is_launcher {
            (
                ManagedFileKind::Launcher,
                1,
                launcher_directory,
                LAUNCHER_MODE,
            )
        } else {
            bail!(
                "rendered file {:?} is neither an expected unit nor an expected launcher",
                file.name
            );
        };
        ensure!(
            file.mode == mode,
            "rendered file {:?} has unexpected mode {:04o}",
            file.name,
            file.mode
        );
        validate_file_name(&file.name)?;
        let contents = file.contents.as_bytes().to_vec();
        ensure!(
            contents.len() as u64 <= MAX_MANAGED_FILE_BYTES,
            "rendered file {:?} exceeds {MAX_MANAGED_FILE_BYTES} bytes",
            file.name
        );
        let sha256 = sha256_hex(&contents);
        let size = contents.len() as u64;
        plan.push(PlannedFile {
            dir_index,
            name: file.name.clone(),
            kind,
            contents,
            mode,
            sha256,
            size,
            path: directory.join(&file.name),
        });
    }
    ensure!(!plan.is_empty(), "render produced no files");
    ensure!(plan.len() <= MAX_FILES, "too many installation files");
    Ok(plan)
}

fn create_managed_file(dir: &Dir, planned: &PlannedFile) -> Result<CreatedFile> {
    let (mut temp, mut file) = open_temp(dir, &planned.name)?;
    file.write_all(&planned.contents)
        .with_context(|| format!("write staged file {:?}", planned.path))?;
    file.set_permissions(PermissionsExt::from_mode(planned.mode))
        .with_context(|| format!("set mode on staged file {:?}", planned.path))?;
    file.sync_all()
        .with_context(|| format!("fsync staged file {:?}", planned.path))?;

    file.seek(SeekFrom::Start(0))?;
    let (hash, size) = hash_reader(&mut file)?;
    ensure!(
        size == planned.size && hash == planned.sha256,
        "staged file {:?} does not match rendered content",
        planned.path
    );
    let metadata = file.metadata()?;
    let mode = metadata.mode() & 0o7777;
    ensure!(
        mode == planned.mode,
        "staged file {:?} has mode {:04o}, expected {:04o}",
        planned.path,
        mode,
        planned.mode
    );
    let owned = OwnedFile {
        path: planned.path.clone(),
        kind: planned.kind,
        sha256: hash,
        size,
        mode,
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    temp.publish(&planned.name)?;
    Ok(CreatedFile {
        file: owned,
        dir_index: planned.dir_index,
    })
}

fn rollback_created(dirs: &[Dir], created: &[CreatedFile]) -> RollbackOutcome {
    let mut rolled_back = Vec::new();
    let mut kept_modified = Vec::new();
    let mut errors = Vec::new();
    let mut keep: BTreeSet<PathBuf> = BTreeSet::new();

    for created_file in created.iter().rev() {
        let entry = &created_file.file;
        let dir = &dirs[created_file.dir_index];
        let name = match file_name(&entry.path) {
            Ok(name) => name,
            Err(error) => {
                keep.insert(entry.path.clone());
                errors.push(format!("{}: {error:#}", entry.path.display()));
                continue;
            }
        };
        let stat = match stat_at(dir, name) {
            Ok(stat) => stat,
            Err(error) => {
                keep.insert(entry.path.clone());
                errors.push(format!("{}: {error:#}", entry.path.display()));
                continue;
            }
        };
        let Some(stat) = stat else {
            // The file is already gone. It is not owned by the partial ledger.
            continue;
        };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            keep.insert(entry.path.clone());
            errors.push(format!(
                "{}: target is no longer a regular file",
                entry.path.display()
            ));
            continue;
        }
        let mut file = match dir.open_file(name, false) {
            Ok(file) => file,
            Err(error) => {
                keep.insert(entry.path.clone());
                errors.push(format!("{}: {error:#}", entry.path.display()));
                continue;
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                keep.insert(entry.path.clone());
                errors.push(format!("{}: {error:#}", entry.path.display()));
                continue;
            }
        };
        if metadata.len() != entry.size {
            kept_modified.push(entry.path.clone());
            keep.insert(entry.path.clone());
            continue;
        }
        match hash_reader(&mut file) {
            Ok((hash, size)) if size == entry.size && hash == entry.sha256 => {
                match unlink_file(dir, name) {
                    Ok(()) => rolled_back.push(entry.path.clone()),
                    Err(error) => {
                        keep.insert(entry.path.clone());
                        errors.push(format!("{}: {error:#}", entry.path.display()));
                    }
                }
            }
            Ok(_) => {
                kept_modified.push(entry.path.clone());
                keep.insert(entry.path.clone());
            }
            Err(error) => {
                keep.insert(entry.path.clone());
                errors.push(format!("{}: {error:#}", entry.path.display()));
            }
        }
    }

    if let Err(error) = sync_directories(dirs) {
        errors.push(format!("fsync after rollback: {error:#}"));
    }

    RollbackOutcome {
        remaining: created
            .iter()
            .filter(|created_file| keep.contains(&created_file.file.path))
            .map(|created_file| created_file.file.clone())
            .collect(),
        rolled_back,
        kept_modified,
        errors,
    }
}

fn validate_ledger(ledger: &OwnershipLedger) -> Result<()> {
    ensure!(
        ledger.schema_version == INSTALLATION_SCHEMA_VERSION,
        "unsupported ownership ledger schema"
    );
    ensure!(
        ledger.outcome == FILES_INSTALLED_NOT_ACTIVATED,
        "unexpected ownership ledger outcome {:?}",
        ledger.outcome
    );
    validate_instance_id(&ledger.instance_id)?;
    validate_absolute_path(&ledger.unit_directory, "unit_directory")?;
    validate_absolute_path(&ledger.launcher_directory, "launcher_directory")?;
    ensure!(
        ledger.units.len() <= MAX_UNITS,
        "ownership ledger contains too many units"
    );

    let expected_units = expected_unit_names(&ledger.instance_id, ledger.topology);
    ensure!(
        ledger.units == expected_units,
        "ownership ledger units do not match instance/topology"
    );
    let expected_launchers = expected_launcher_names(&ledger.instance_id, ledger.topology);

    ensure!(
        ledger.files.len() <= MAX_FILES,
        "ownership ledger contains too many files"
    );
    let mut paths = BTreeSet::new();
    for file in &ledger.files {
        validate_absolute_path(&file.path, "ledger file path")?;
        ensure!(
            paths.insert(file.path.clone()),
            "duplicate ledger file path {:?}",
            file.path
        );
        let name = file_name(&file.path)?;
        validate_file_name(name)?;
        let parent = file.path.parent().context("ledger file has no parent")?;
        match file.kind {
            ManagedFileKind::Unit => {
                ensure!(
                    parent == ledger.unit_directory,
                    "unit ledger file {:?} is not directly inside unit_directory",
                    file.path
                );
                ensure!(
                    expected_units.iter().any(|unit| unit == name),
                    "unit ledger file {:?} is not an expected managed unit",
                    file.path
                );
                ensure!(
                    file.mode == UNIT_MODE,
                    "unit ledger file {:?} has unexpected mode",
                    file.path
                );
            }
            ManagedFileKind::Launcher => {
                ensure!(
                    parent == ledger.launcher_directory,
                    "launcher ledger file {:?} is not directly inside launcher_directory",
                    file.path
                );
                ensure!(
                    expected_launchers.iter().any(|launcher| launcher == name),
                    "launcher ledger file {:?} is not an expected managed launcher",
                    file.path
                );
                ensure!(
                    file.mode == LAUNCHER_MODE,
                    "launcher ledger file {:?} has unexpected mode",
                    file.path
                );
            }
        }
        digest(&file.sha256).with_context(|| format!("invalid hash for {:?}", file.path))?;
        ensure!(
            file.size <= MAX_MANAGED_FILE_BYTES,
            "ledger file {:?} exceeds {MAX_MANAGED_FILE_BYTES} bytes",
            file.path
        );
        ensure!(
            file.mode & !0o7777 == 0,
            "ledger file {:?} has invalid mode bits",
            file.path
        );
    }
    Ok(())
}

fn expected_unit_names(instance_id: &str, topology: InstallTopology) -> Vec<String> {
    let base = format!("echo-agent-{instance_id}");
    let mut units = vec![format!("{base}-core.service")];
    if topology == InstallTopology::Standalone {
        units.push(format!("{base}-panel.service"));
    }
    units.push(format!("{base}-update.service"));
    units
}

fn expected_launcher_names(instance_id: &str, topology: InstallTopology) -> Vec<String> {
    let base = format!("echo-agent-{instance_id}");
    let mut launchers = vec![base.clone()];
    if topology == InstallTopology::Standalone {
        launchers.push(format!("{base}-panel"));
    }
    launchers
}

fn validate_instance_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 100,
        "invalid instance_id length"
    );
    ensure!(
        value.as_bytes()[0].is_ascii_alphanumeric(),
        "instance_id must start with an ASCII alphanumeric character"
    );
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "unsafe instance_id: {value:?}"
    );
    ensure!(
        !value.contains(".."),
        "instance_id must not contain consecutive dots"
    );
    Ok(())
}

fn validate_file_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "empty file name");
    ensure!(name.len() <= 255, "file name too long");
    ensure!(name != "." && name != "..", "invalid file name");
    ensure!(!name.contains('/'), "file name must not contain '/'");
    ensure!(
        !name.chars().any(char::is_control),
        "file name contains a control character"
    );
    Ok(())
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("path {:?} has no UTF-8 file name", path))
}

fn open_trusted_directory(path: &Path, scope: InstallScope, field: &str) -> Result<Dir> {
    let dir = Dir::absolute(path).with_context(|| format!("open {field} {path:?}"))?;
    let metadata = dir.0.metadata()?;
    ensure!(metadata.is_dir(), "{field} must be a directory");
    let mode = metadata.mode() & 0o7777;
    ensure!(
        mode & 0o022 == 0,
        "{field} {path:?} must not be group/world writable (mode {mode:04o})"
    );
    let euid = unsafe { libc::geteuid() };
    match scope {
        InstallScope::System => {
            ensure!(euid == 0, "system scope requires effective uid 0");
            ensure!(
                metadata.uid() == 0,
                "{field} {path:?} must be owned by root"
            );
        }
        InstallScope::User => {
            ensure!(
                metadata.uid() == euid,
                "{field} {path:?} must be owned by the current effective uid {euid}"
            );
        }
    }
    Ok(dir)
}

fn validate_managed_directory_location(path: &Path, field: &str) -> Result<()> {
    const SHARED_ROOTS: &[&str] = &[
        "/",
        "/bin",
        "/boot",
        "/dev",
        "/etc",
        "/home",
        "/lib",
        "/lib64",
        "/opt",
        "/proc",
        "/root",
        "/run",
        "/sbin",
        "/sys",
        "/tmp",
        "/usr",
        "/usr/bin",
        "/usr/local",
        "/usr/sbin",
        "/var",
    ];
    for shared in SHARED_ROOTS {
        ensure!(
            path != Path::new(shared),
            "{field} {path:?} is a shared system directory, not a managed unit/launcher directory"
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        ensure!(
            path != Path::new(&home),
            "{field} {path:?} must not be the user's home directory"
        );
    }
    Ok(())
}

fn validate_destination_not_protected(spec: &InstallSpec, dir: &Path, field: &str) -> Result<()> {
    for (label, protected) in [
        ("release_root", spec.release_root.as_path()),
        ("state_dir", spec.state_dir.as_path()),
    ] {
        ensure!(
            !same_or_descendant(dir, protected),
            "{field} must not be inside {label}"
        );
        ensure!(
            !same_or_descendant(protected, dir),
            "{field} must not contain {label}"
        );
    }
    let configs = [
        ("core_config", Some(spec.core_config.as_path())),
        ("panel_config", spec.panel_config.as_deref()),
    ];
    for (label, config) in configs {
        let Some(config) = config else {
            continue;
        };
        let parent = config.parent().context("config path has no parent")?;
        ensure!(
            !same_or_descendant(dir, parent),
            "{field} must not be inside the {label} directory"
        );
        ensure!(
            !same_or_descendant(parent, dir),
            "{field} must not contain the {label} directory"
        );
    }
    Ok(())
}

fn ensure_protected_file_not_targeted(spec: &InstallSpec, target: &Path) -> Result<()> {
    ensure!(
        target != spec.release_root,
        "installation target {:?} is the release root",
        target
    );
    ensure!(
        target != spec.core_config,
        "installation target {:?} is the core config",
        target
    );
    ensure!(
        spec.panel_config.as_deref() != Some(target),
        "installation target {:?} is the panel config",
        target
    );
    ensure!(
        target != spec.deploy_executable,
        "installation target {:?} is the deployment executable",
        target
    );
    Ok(())
}

fn same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
}

fn entry_exists(dir: &Dir, name: &str) -> Result<bool> {
    Ok(stat_at(dir, name)?.is_some())
}

fn stat_at(dir: &Dir, name: &str) -> Result<Option<libc::stat>> {
    validate_file_name(name)?;
    let name = CString::new(name)?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory fd is live, `name` is one NUL-terminated component,
    // and AT_SYMLINK_NOFOLLOW prevents following a final symlink. `stat` is
    // initialized only when the call succeeds.
    let result = unsafe {
        libc::fstatat(
            dir.0.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: fstatat returned 0, so it initialized the whole struct.
        Ok(Some(unsafe { stat.assume_init() }))
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(error).with_context(|| format!("inspect directory entry {name:?}"))
        }
    }
}

fn unlink_file(dir: &Dir, name: &str) -> Result<()> {
    validate_file_name(name)?;
    let name = CString::new(name)?;
    // SAFETY: directory fd and a single validated component; flags 0 means
    // unlink a non-directory entry without following a symlink.
    let result = unsafe { libc::unlinkat(dir.0.as_raw_fd(), name.as_ptr(), 0) };
    ensure!(
        result == 0,
        "unlink {name:?}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn sync_directories(dirs: &[Dir]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for dir in dirs {
        let metadata = dir.0.metadata()?;
        if seen.insert((metadata.dev(), metadata.ino())) {
            dir.sync()?;
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_reader(reader: &mut impl Read) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("file size overflow")?;
        ensure!(
            total <= MAX_MANAGED_FILE_BYTES,
            "file exceeds {MAX_MANAGED_FILE_BYTES} bytes"
        );
        hash.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hash.finalize()), total))
}

fn checked_fd(fd: libc::c_int) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: a successful open/openat returns a new owned descriptor.
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
}

struct TempGuard<'a> {
    dir: &'a Dir,
    name: CString,
    published: bool,
}

impl TempGuard<'_> {
    fn publish(&mut self, final_name: &str) -> Result<()> {
        let final_name_c = CString::new(final_name)?;
        // SAFETY: both names are single validated components in the same live
        // directory fd. RENAME_NOREPLACE makes overwriting impossible.
        let result = unsafe {
            libc::renameat2(
                self.dir.0.as_raw_fd(),
                self.name.as_ptr(),
                self.dir.0.as_raw_fd(),
                final_name_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return Err(error).with_context(|| {
                format!("refusing to overwrite or publish existing target {final_name:?}")
            });
        }
        self.published = true;
        Ok(())
    }
    /// Atomically replace `final_name` with the temporary file.
    ///
    /// Only used for the ownership ledger after [`LedgerSlot::ensure_current`]
    /// proved that `final_name` is the exact inode created by this invocation.
    /// Target files never use this path.
    fn publish_replace(&mut self, final_name: &str) -> Result<()> {
        let final_name_c = CString::new(final_name)?;
        // SAFETY: both names are single validated components in the same live
        // directory fd. The caller proved ownership of the replaced inode.
        let result = unsafe {
            libc::renameat(
                self.dir.0.as_raw_fd(),
                self.name.as_ptr(),
                self.dir.0.as_raw_fd(),
                final_name_c.as_ptr(),
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return Err(error).with_context(|| format!("replace owned ledger {final_name:?}"));
        }
        self.published = true;
        Ok(())
    }
}
impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if !self.published {
            // SAFETY: best-effort cleanup of the temporary entry created by this
            // guard. Ignoring the result is intentional: the primary error is
            // already being returned.
            unsafe {
                libc::unlinkat(self.dir.0.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

fn open_temp<'a>(dir: &'a Dir, final_name: &str) -> Result<(TempGuard<'a>, File)> {
    validate_file_name(final_name)?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".echo-deploy-{pid}-{nanos}-{sequence}.tmp");
        ensure!(temp_name.len() <= 255, "temporary file name too long");
        let temp_name_c = CString::new(temp_name.as_str())?;
        // SAFETY: single component with O_EXCL/O_NOFOLLOW; a new 0600 file is
        // created inside the pinned directory. The returned fd is owned.
        let fd = unsafe {
            libc::openat(
                dir.0.as_raw_fd(),
                temp_name_c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            return Ok((
                TempGuard {
                    dir,
                    name: temp_name_c,
                    published: false,
                },
                checked_fd(fd)?,
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            continue;
        }
        return Err(error).with_context(|| format!("create staged file for {final_name:?}"));
    }
    bail!("could not allocate a unique staged file name for {final_name:?}")
}
