//! Client for Core's deployment control socket (`ECHO_DEPLOY_CONTROL_SOCKET`).
//!
//! Core speaks a strict one-line JSON protocol: an absolute Unix socket, a
//! private parent directory, same-uid `SO_PEERCRED` peers, one request per line
//! (at most 64 KiB) and one response per line.  This module is the deployment
//! adapter's side of that protocol:
//!
//! * [`query_core`] validates the socket path and the connected peer *before*
//!   writing anything, sends exactly one request with bounded read/write
//!   timeouts, and returns the raw Core response as JSON.
//! * [`map_to_systemd_reply`] turns that response into the strict
//!   [`systemd::ControlReply`] shape.  It refuses failed acknowledgements and
//!   incomplete prepare acknowledgements.
//! * [`verify_required_modules`] rejects a `status` / `ready` reply whose
//!   required modules are not proven live-mounted and active by Core's
//!   `module_state` evidence.  Replies from an older Core remain compatible
//!   only when the deployment requires no modules.
//!
//! Identity is deliberately fail-closed.  Core's `release.identity_verified` is
//! always `false` because Core only reports the `ECHO_RELEASE_ID` declaration;
//! a declaration is **not** build/process verification.  A `ready` reply from
//! Core therefore cannot be mapped on its own.  The systemd-side caller must
//! verify the running process image itself and opt in through
//! [`map_to_systemd_reply_with_process_verification`].  Prepare replies are
//! allowed to succeed with the declaration-only identity, but callers must
//! surface [`core_identity`]'s limitations and must not present them as proof
//! of what was built.
//!
//! The systemd reply schema has no extension fields (`deny_unknown_fields` on
//! [`systemd::ControlReply`]), so identity limitations cannot be smuggled into
//! the reply.  They are exposed through [`core_identity`] instead.

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Component, Path},
    time::{Duration, Instant},
};

use crate::systemd;

/// Upper bound for the JSON body of one request, matching Core's line limit.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Upper bound for one response line.  A Core `status`/`prepare` reply can be
/// larger than a request because it enumerates agent acknowledgements.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Core rejects transaction ids and releases longer than this.
pub const MAX_ID_BYTES: usize = 256;
/// Refuse absurd deadlines so `Instant + timeout` and `SO_RCVTIMEO` stay sane.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

const MAX_SOCKET_PATH_BYTES: usize = 107;
const MAX_ERROR_TEXT_CHARS: usize = 512;
const DECLARATION_ONLY_VERIFICATION: &str = "env_echo_release_id_declaration_only";

/// The four operations Core's deployment control protocol accepts.
///
/// `cancel` requires the generation returned by the matching `prepare`; that
/// requirement is encoded in the variant so a cancel request cannot be built
/// without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreOperation {
    Prepare,
    Cancel { expected_generation: u64 },
    Status,
    Ready,
}

impl CoreOperation {
    /// Construct a cancel operation.
    pub fn cancel(expected_generation: u64) -> Self {
        Self::Cancel {
            expected_generation,
        }
    }

    /// Wire name used by Core's strict `operation` field.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Cancel { .. } => "cancel",
            Self::Status => "status",
            Self::Ready => "ready",
        }
    }

    fn expected_generation(self) -> Option<u64> {
        match self {
            Self::Cancel {
                expected_generation,
            } => Some(expected_generation),
            _ => None,
        }
    }
}

/// Identity facts reported by Core, plus explicit limitations.
///
/// `identity_verified` is copied from Core and is `false` for the shipping
/// implementation.  `limitations` is non-empty whenever the caller must not
/// treat the declared release as build identity (i.e. whenever
/// `identity_verified` is false, or no release was declared at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreIdentity {
    pub declared: Option<String>,
    pub identity_verified: bool,
    pub verification: String,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[derive(Serialize)]
struct WireRequest<'a> {
    operation: &'static str,
    transaction_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_release: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_generation: Option<u64>,
}

/// Query Core's deployment control socket once.
///
/// The path must be an absolute, normalized path to a Unix socket whose parent
/// is a private directory owned by the current effective uid.  The socket leaf
/// must be a real socket (not a symlink or any other file) owned by the current
/// effective uid.  After connecting, the peer's `SO_PEERCRED` uid is compared
/// with the current effective uid before a single byte is written, so a
/// same-path impostor socket cannot be queried.
///
/// `timeout` bounds the whole query.  It is applied to socket reads and writes
/// (and recomputed before every syscall) rather than only to the initial
/// connect; a stalled or progressively trickling peer is rejected as a timeout.
///
/// The returned [`Value`] is the parsed response object.  Envelope fields
/// (`ok`, `operation`, `transaction_id`) are checked here.  A well-formed
/// `{"ok":false,...}` acknowledgement is returned as `Ok(Value)`; business
/// rejection is handled by [`map_to_systemd_reply`].
pub fn query_core(
    socket: &Path,
    operation: CoreOperation,
    transaction_id: &str,
    expected_release: Option<&str>,
    timeout: Duration,
) -> Result<Value> {
    query_core_inner(
        socket,
        operation,
        transaction_id,
        expected_release,
        timeout,
        current_euid(),
    )
}

/// Test-only seam: run the real query path but expect a chosen peer uid.  This
/// lets tests exercise the `SO_PEERCRED` rejection without a second uid/root.
#[cfg(test)]
pub fn query_core_expect_peer_uid(
    socket: &Path,
    operation: CoreOperation,
    transaction_id: &str,
    expected_release: Option<&str>,
    timeout: Duration,
    expected_peer_uid: u32,
) -> Result<Value> {
    query_core_inner(
        socket,
        operation,
        transaction_id,
        expected_release,
        timeout,
        expected_peer_uid,
    )
}

fn query_core_inner(
    socket: &Path,
    operation: CoreOperation,
    transaction_id: &str,
    expected_release: Option<&str>,
    timeout: Duration,
    expected_peer_uid: u32,
) -> Result<Value> {
    validate_request(operation, transaction_id, expected_release)?;
    ensure!(!timeout.is_zero(), "control query timeout must be non-zero");
    ensure!(
        timeout <= MAX_TIMEOUT,
        "control query timeout exceeds {} seconds",
        MAX_TIMEOUT.as_secs()
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("control query deadline is too large")?;

    // Validate before connect, then re-check after connect.  The second check
    // catches a socket swapped between validation and connect (same-uid
    // attackers are outside the SO_PEERCRED protection, but this still catches
    // accidental stale-path races).
    let before = validate_socket_path(socket)?;
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to Core control socket {}", socket.display()))?;
    let after = validate_socket_path(socket)?;
    ensure!(
        before == after,
        "Core control socket {} was replaced while connecting",
        socket.display()
    );

    let peer_uid = peer_uid(&stream)?;
    ensure!(
        peer_uid == expected_peer_uid,
        "Core control socket peer uid {peer_uid} does not match expected uid \
         {expected_peer_uid}; refusing a possible impostor"
    );

    let request = WireRequest {
        operation: operation.wire_name(),
        transaction_id,
        expected_release,
        expected_generation: operation.expected_generation(),
    };
    let mut payload = serde_json::to_vec(&request).context("serialize Core control request")?;
    ensure!(
        payload.len() <= MAX_REQUEST_BYTES,
        "Core control request exceeds {} bytes",
        MAX_REQUEST_BYTES
    );
    payload.push(b'\n');

    write_all_deadline(&mut stream, &payload, deadline)?;
    // We only ever send one request per connection; tell Core no more lines
    // are coming (it still writes the ACK before observing EOF).
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let line = read_line_deadline(&mut stream, deadline)?;
    let response: Value = serde_json::from_slice(&line).with_context(|| {
        format!(
            "Core control response is not valid JSON ({} bytes)",
            line.len()
        )
    })?;
    validate_envelope(&response, operation, transaction_id)?;
    Ok(response)
}

/// Current effective uid via `geteuid(2)`.
pub fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Read `SO_PEERCRED` for a connected Unix stream.
///
/// Exposed for tests and diagnostics; `query_core` uses it internally before
/// writing.  A peer without a process id (pid 0) is rejected: a real Core
/// control server always has one.
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    // SAFETY: zeroed ucred is valid for getsockopt to fill in.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd belongs to `stream` for the duration of the call; the
    // pointers are valid and the length matches the ucred allocation.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("getsockopt(SO_PEERCRED)");
    }
    ensure!(
        length as usize >= std::mem::size_of::<libc::ucred>(),
        "SO_PEERCRED returned {} bytes, expected at least {}",
        length,
        std::mem::size_of::<libc::ucred>()
    );
    ensure!(
        credentials.pid > 0,
        "Core control socket peer has no process id (pid 0)"
    );
    Ok(credentials.uid)
}

fn validate_request(
    operation: CoreOperation,
    transaction_id: &str,
    expected_release: Option<&str>,
) -> Result<()> {
    ensure!(
        !transaction_id.is_empty() && transaction_id.len() <= MAX_ID_BYTES,
        "transaction_id must be 1..={MAX_ID_BYTES} bytes"
    );
    if let Some(release) = expected_release {
        ensure!(
            !release.is_empty() && release.len() <= MAX_ID_BYTES,
            "expected_release must be 1..={MAX_ID_BYTES} bytes"
        );
    }
    if let CoreOperation::Cancel {
        expected_generation,
    } = operation
    {
        ensure!(
            expected_generation != 0,
            "cancel requires a non-zero expected_generation"
        );
    }
    Ok(())
}

fn validate_socket_path(socket: &Path) -> Result<SocketIdentity> {
    ensure!(
        socket.is_absolute(),
        "Core control socket path must be absolute: {}",
        socket.display()
    );
    let raw = socket.as_os_str().as_bytes();
    ensure!(
        !raw.is_empty() && raw.len() <= MAX_SOCKET_PATH_BYTES,
        "Core control socket path must be 1..={MAX_SOCKET_PATH_BYTES} bytes: {}",
        socket.display()
    );
    ensure!(
        raw.last() != Some(&b'/'),
        "Core control socket path must not end in '/': {}",
        socket.display()
    );
    ensure!(
        socket
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "Core control socket path must be normalized (no '.' or '..'): {}",
        socket.display()
    );

    let euid = current_euid();
    let parent = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| {
            format!(
                "Core control socket path has no parent directory: {}",
                socket.display()
            )
        })?;
    let parent_meta = fs::metadata(parent)
        .with_context(|| format!("stat control socket parent {}", parent.display()))?;
    ensure!(
        parent_meta.is_dir(),
        "Core control socket parent is not a directory: {}",
        parent.display()
    );
    ensure!(
        parent_meta.uid() == euid,
        "Core control socket parent {} is owned by uid {}, expected current uid {}",
        parent.display(),
        parent_meta.uid(),
        euid
    );
    let parent_mode = parent_meta.permissions().mode() & 0o777;
    ensure!(
        parent_mode & 0o077 == 0,
        "Core control socket parent {} is not private: mode {:o} (group/other bits must be 0)",
        parent.display(),
        parent_mode
    );

    let meta = fs::symlink_metadata(socket)
        .with_context(|| format!("stat Core control socket {}", socket.display()))?;
    ensure!(
        !meta.file_type().is_symlink(),
        "refusing symlinked Core control socket {}",
        socket.display()
    );
    ensure!(
        meta.file_type().is_socket(),
        "Core control path {} is not a Unix socket",
        socket.display()
    );
    ensure!(
        meta.uid() == euid,
        "Core control socket {} is owned by uid {}, expected current uid {}",
        socket.display(),
        meta.uid(),
        euid
    );
    Ok(SocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

fn validate_envelope(
    response: &Value,
    operation: CoreOperation,
    transaction_id: &str,
) -> Result<()> {
    let object = response
        .as_object()
        .context("Core control response must be a JSON object")?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .context("Core control response is missing boolean `ok`")?;
    let echoed_operation = object
        .get("operation")
        .and_then(Value::as_str)
        .context("Core control response is missing `operation`")?;
    ensure!(
        echoed_operation == operation.wire_name(),
        "Core control response operation {echoed_operation:?} does not match requested {:?}",
        operation.wire_name()
    );
    let echoed_transaction = object
        .get("transaction_id")
        .and_then(Value::as_str)
        .context("Core control response is missing `transaction_id`")?;
    ensure!(
        echoed_transaction == transaction_id,
        "Core control response transaction_id does not match the request"
    );
    if !ok {
        ensure!(
            object.get("error").map(Value::is_object).unwrap_or(false),
            "failed Core control response lacks an `error` object"
        );
    }
    Ok(())
}

/// Parse and validate Core's `release` identity information.
///
/// Core's shipping implementation always reports
/// `identity_verified: false, verification: "env_echo_release_id_declaration_only"`.
/// Treating that declaration as build identity is forbidden here: the returned
/// limitations list says so explicitly, and an inconsistent response that
/// claims `identity_verified: true` while also claiming declaration-only
/// verification is rejected.
pub fn core_identity(response: &Value) -> Result<CoreIdentity> {
    let object = response
        .as_object()
        .context("Core control response must be a JSON object")?;
    let release = object
        .get("release")
        .and_then(Value::as_object)
        .context("Core control response is missing the `release` object")?;
    let declared = match release.get("declared") {
        Some(Value::Null) => None,
        Some(Value::String(value)) => {
            ensure!(
                !value.is_empty() && value.len() <= MAX_ID_BYTES,
                "release.declared must be 1..={MAX_ID_BYTES} bytes"
            );
            Some(value.clone())
        }
        Some(_) => bail!("release.declared must be null or a string"),
        None => bail!("release.declared is missing"),
    };
    let identity_verified = release
        .get("identity_verified")
        .and_then(Value::as_bool)
        .context("release.identity_verified must be a boolean")?;
    let verification = release
        .get("verification")
        .and_then(Value::as_str)
        .context("release.verification must be a string")?;
    ensure!(
        !verification.is_empty() && verification.len() <= MAX_ID_BYTES,
        "release.verification must be 1..={MAX_ID_BYTES} bytes"
    );
    if identity_verified {
        ensure!(
            verification != DECLARATION_ONLY_VERIFICATION,
            "inconsistent Core identity: identity_verified=true with declaration-only verification"
        );
    }

    let mut limitations = Vec::new();
    if !identity_verified {
        limitations.push(format!(
            "release identity is declared only (identity_verified=false, verification={verification:?}); \
             this is not build or running-process verification"
        ));
    }
    if declared.is_none() {
        limitations.push("Core did not declare ECHO_RELEASE_ID".into());
    }
    Ok(CoreIdentity {
        declared,
        identity_verified,
        verification: verification.to_owned(),
        limitations,
    })
}

/// One live module's facts, extracted from `module_state.modules[]` after its
/// shape and cross-field consistency have been checked.
struct ModuleFact {
    mounted: bool,
    active: bool,
    error: Option<String>,
    state: Option<String>,
}

/// Verify the live module readiness evidence in a Core `status` / `ready`
/// response.
///
/// `required` is the deployment's own `deployment.required_modules` list; Core
/// does not have to have received it in the request. Every requested module
/// must be present in the live `module_state.modules[]` snapshot **and** in the
/// `module_state.mounted` / `module_state.active` sets with
/// `mounted == true`, `active == true` and no `error`. `enabled` is the
/// registry's configuration switch, never proof of a mount, and
/// `module_state.required` is only an echo of the request — neither is trusted.
///
/// Compatibility and strictness:
///
/// * A response with **neither** `modules_verified` nor `module_state` (an old
///   Core) is accepted only when `required` is empty.
/// * As soon as module evidence is present, `modules_verified` and
///   `module_state.verified` must both be `true` and agree;
///   `module_state.modules` must be a non-empty list whose `mounted` / `active`
///   flags exactly match the `module_state.mounted` / `module_state.active`
///   sets. Duplicate ids, partial evidence, `active` without `mounted`,
///   `active` with an `error`, and `active` with a non-ready runtime state are
///   rejected as conflicts.
/// * For Ready replies, `checks.modules_verified` and
///   `checks.required_modules_mounted` must be present together and `true`;
///   a false check is rejected even when the top-level `ready` flag is `true`.
pub fn verify_required_modules(response: &Value, required: &[String]) -> Result<()> {
    let object = response
        .as_object()
        .context("Core control response must be a JSON object")?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .context("Core control response is missing boolean `ok`")?;
    ensure!(
        ok,
        "cannot verify module readiness from a failed Core response (`ok` is false)"
    );

    let mut required_seen = BTreeSet::new();
    for id in required {
        ensure!(
            !id.is_empty() && id.len() <= MAX_ID_BYTES,
            "required module id must be 1..={MAX_ID_BYTES} bytes"
        );
        ensure!(
            required_seen.insert(id.as_str()),
            "duplicate required module id {id:?}"
        );
    }

    let (modules_verified, module_state) =
        match (object.get("modules_verified"), object.get("module_state")) {
            (None, None) => {
                // Old Core: no module evidence was promised.  That is only
                // compatible with a deployment that requires no modules.
                ensure!(
                    required.is_empty(),
                    "Core response carries no live module evidence (old Core) but required modules were requested: {required:?}"
                );
                verify_ready_module_checks(object, None)?;
                return Ok(());
            }
            (Some(modules_verified), Some(module_state)) => (modules_verified, module_state),
            _ => bail!(
                "Core response has partial module evidence: `modules_verified` and `module_state` must be present together"
            ),
        };

    let modules_verified = modules_verified
        .as_bool()
        .context("`modules_verified` must be a boolean")?;
    let state = module_state
        .as_object()
        .context("`module_state` must be a JSON object")?;
    let verified = state
        .get("verified")
        .and_then(Value::as_bool)
        .context("`module_state.verified` must be a boolean")?;
    ensure!(
        modules_verified == verified,
        "conflicting module evidence: modules_verified={modules_verified} but module_state.verified={verified}"
    );
    ensure!(
        modules_verified,
        "Core could not verify live module state (`modules_verified` is false)"
    );
    ensure!(
        verified,
        "Core could not verify live module state (`module_state.verified` is false)"
    );

    if let Some(source) = state.get("source") {
        let source = source
            .as_str()
            .context("`module_state.source` must be a string")?;
        ensure!(
            !source.is_empty(),
            "`module_state.source` must not be empty"
        );
    }
    match state.get("reason") {
        None | Some(Value::Null) => {}
        Some(Value::String(_)) => bail!(
            "conflicting module evidence: `module_state.reason` is set even though verified=true"
        ),
        Some(_) => bail!("`module_state.reason` must be a string or null"),
    }

    let mounted = module_string_set(state, "mounted")?;
    let active = module_string_set(state, "active")?;
    ensure!(
        active.is_subset(&mounted),
        "conflicting module evidence: module_state.active is not a subset of module_state.mounted"
    );

    let modules = state
        .get("modules")
        .and_then(Value::as_array)
        .context("`module_state.modules` must be an array")?;
    ensure!(
        !modules.is_empty(),
        "conflicting module evidence: module_state.verified=true but module_state.modules is empty"
    );

    let mut facts: BTreeMap<String, ModuleFact> = BTreeMap::new();
    let mut live_mounted = BTreeSet::new();
    let mut live_active = BTreeSet::new();
    for module in modules {
        let module = module
            .as_object()
            .context("`module_state.modules` contains a non-object entry")?;
        let id = module
            .get("id")
            .and_then(Value::as_str)
            .context("`module_state.modules` entry is missing a string `id`")?;
        ensure!(
            !id.is_empty(),
            "`module_state.modules` entry has an empty `id`"
        );
        ensure!(
            facts
                .insert(id.to_owned(), module_facts(id, module)?)
                .is_none(),
            "duplicate module id {id:?} in `module_state.modules`"
        );
        let fact = facts.get(id).expect("module fact inserted above");
        if fact.mounted {
            live_mounted.insert(id.to_owned());
        }
        if fact.active {
            live_active.insert(id.to_owned());
        }
    }
    ensure!(
        live_mounted == mounted,
        "conflicting module evidence: module_state.mounted does not match modules[].mounted"
    );
    ensure!(
        live_active == active,
        "conflicting module evidence: module_state.active does not match modules[].active"
    );

    // `required` is an echo of the request, never evidence; `missing_required`
    // and `required_errors` are Core's own diagnostics.  Validate their shape
    // and refuse the obvious contradiction of "missing" + "live active".
    let _echoed_required = optional_module_string_set(state, "required")?;
    let missing_required = optional_module_string_set(state, "missing_required")?;
    let _required_errors = optional_module_string_list(state, "required_errors")?;
    let _failed = optional_module_string_list(state, "failed")?;
    for id in &missing_required {
        ensure!(
            !active.contains(id),
            "conflicting module evidence: missing_required contains {id:?} but the live active set contains it"
        );
    }

    for id in required {
        let fact = facts.get(id).with_context(|| {
            format!("required module {id:?} is not present in live module state")
        })?;
        if let Some(error) = &fact.error {
            bail!(
                "required module {id:?} reports error: {}",
                bounded_text(error, MAX_ERROR_TEXT_CHARS)
            );
        }
        ensure!(
            fact.mounted && mounted.contains(id),
            "required module {id:?} is registered but not mounted"
        );
        ensure!(
            fact.active && active.contains(id),
            "required module {id:?} is mounted but not active"
        );
        if let Some(state) = &fact.state {
            ensure!(
                state.is_empty() || state == "ready",
                "required module {id:?} is in state {state:?}, not active"
            );
        }
        ensure!(
            !missing_required.contains(id.as_str()),
            "required module {id:?} is reported missing by Core"
        );
    }

    verify_ready_module_checks(object, Some(modules_verified))?;
    Ok(())
}

/// Ready replies carry a second, derived copy of the module checks.  Old Core
/// has neither field; new Core always has both.  A false or missing half is a
/// contradiction, not a pass.
fn verify_ready_module_checks(
    object: &serde_json::Map<String, Value>,
    modules_verified: Option<bool>,
) -> Result<()> {
    let Some(checks) = object.get("checks") else {
        return Ok(());
    };
    let checks = checks
        .as_object()
        .context("`checks` must be a JSON object")?;
    let check_verified = match checks.get("modules_verified") {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .context("`checks.modules_verified` must be a boolean")?,
        ),
    };
    let check_mounted = match checks.get("required_modules_mounted") {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .context("`checks.required_modules_mounted` must be a boolean")?,
        ),
    };
    match (check_verified, check_mounted) {
        (None, None) => {
            ensure!(
                modules_verified.is_none(),
                "ready response has live module evidence but `checks` omits \
                 modules_verified/required_modules_mounted"
            );
        }
        (Some(check_verified), Some(check_mounted)) => {
            ensure!(
                modules_verified.is_some(),
                "ready checks claim module verification but the response has no live `module_state` evidence"
            );
            ensure!(
                check_verified,
                "ready response reports `checks.modules_verified=false`"
            );
            ensure!(
                check_mounted,
                "ready response reports `checks.required_modules_mounted=false`"
            );
            if let Some(modules_verified) = modules_verified {
                ensure!(
                    check_verified == modules_verified,
                    "conflicting module evidence: checks.modules_verified != modules_verified"
                );
            }
        }
        _ => bail!(
            "`checks` must carry both `modules_verified` and `required_modules_mounted`, or neither"
        ),
    }
    for key in ["missing_modules", "module_errors"] {
        if let Some(value) = checks.get(key) {
            let values = value
                .as_array()
                .with_context(|| format!("`checks.{key}` must be an array"))?;
            ensure!(
                values.is_empty(),
                "ready response reports non-empty `checks.{key}`: {values:?}"
            );
        }
    }
    Ok(())
}

fn module_facts(id: &str, module: &serde_json::Map<String, Value>) -> Result<ModuleFact> {
    let mounted =
        module_bool(module, "mounted").with_context(|| format!("module {id:?} is invalid"))?;
    let active =
        module_bool(module, "active").with_context(|| format!("module {id:?} is invalid"))?;
    let error = match module.get("error") {
        None | Some(Value::Null) => None,
        Some(Value::String(error)) => {
            ensure!(
                !error.is_empty(),
                "module {id:?} has an empty `error` string"
            );
            Some(error.clone())
        }
        Some(_) => bail!("module {id:?} `error` must be a string or null"),
    };
    let state = match module.get("state") {
        None => None,
        Some(Value::String(state)) => Some(state.clone()),
        Some(_) => bail!("module {id:?} `state` must be a string"),
    };
    if let Some(enabled) = module.get("enabled") {
        ensure!(
            enabled.is_boolean(),
            "module {id:?} `enabled` must be a boolean"
        );
    }
    if active {
        ensure!(
            mounted,
            "conflicting module evidence: module {id:?} is active but not mounted"
        );
        if let Some(error) = &error {
            bail!(
                "conflicting module evidence: module {id:?} is active but reports error: {}",
                bounded_text(error, MAX_ERROR_TEXT_CHARS)
            );
        }
    }
    if let Some(state) = &state {
        // Mirror Core's exact `active` derivation: mounted + no error +
        // state ∈ {"" (in-process), "ready"}.
        let expected_active = mounted && error.is_none() && (state.is_empty() || state == "ready");
        ensure!(
            active == expected_active,
            "conflicting module evidence: module {id:?} has active={active} but mounted={mounted}, \
             state={state:?}, error={error:?}"
        );
    }
    Ok(ModuleFact {
        mounted,
        active,
        error,
        state,
    })
}

fn module_bool(module: &serde_json::Map<String, Value>, key: &str) -> Result<bool> {
    module
        .get(key)
        .and_then(Value::as_bool)
        .with_context(|| format!("missing boolean `{key}`"))
}

fn module_string_set(
    state: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<BTreeSet<String>> {
    let values = state
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("`module_state.{key}` must be an array"))?;
    let mut set = BTreeSet::new();
    for value in values {
        let id = value
            .as_str()
            .with_context(|| format!("`module_state.{key}` entries must be strings"))?;
        ensure!(!id.is_empty(), "`module_state.{key}` contains an empty id");
        ensure!(
            set.insert(id.to_owned()),
            "duplicate id {id:?} in `module_state.{key}`"
        );
    }
    Ok(set)
}

fn optional_module_string_set(
    state: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<BTreeSet<String>> {
    match state.get(key) {
        None => Ok(BTreeSet::new()),
        Some(_) => module_string_set(state, key),
    }
}

fn optional_module_string_list(
    state: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Vec<String>> {
    let Some(values) = state.get(key) else {
        return Ok(Vec::new());
    };
    let values = values
        .as_array()
        .with_context(|| format!("`module_state.{key}` must be an array"))?;
    let mut list = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let text = value
            .as_str()
            .with_context(|| format!("`module_state.{key}` entries must be strings"))?;
        ensure!(
            !text.is_empty(),
            "`module_state.{key}` contains an empty string"
        );
        ensure!(
            seen.insert(text.to_owned()),
            "duplicate entry {text:?} in `module_state.{key}`"
        );
        list.push(text.to_owned());
    }
    Ok(list)
}
/// Map a Core response into the strict systemd adapter reply.
///
/// This is the fail-closed default: it never treats Core's declared release as
/// verified identity, so a successful `ready` ACK is rejected because Core
/// reports `identity_verified: false`.  Use
/// [`map_to_systemd_reply_with_process_verification`] only after the caller has
/// performed its own running-process image verification.
///
/// Supported systemd operations are `PrepareUpgrade` and `Ready`.  Core's
/// control protocol has no equivalent for `Observe`, `Drain`,
/// `MaintenanceEnter` or `MaintenanceLeave`; mapping those would require
/// fabricating state, so they are rejected rather than guessed.
///
/// A failed Core acknowledgement (`ok:false`) is rejected with Core's
/// `error.code`/`error.message`.  A successful prepare additionally requires:
///
/// * `prepared: true`, `maintenance_held: true`;
/// * `writer_scope.all_controlled_writers_paused: true` and
///   `management_commands_quiesced: true`;
/// * the `user_arbitrary_subprocesses` limitation to be explicitly present in
///   `writer_scope.not_covered` (this adapter must not silently claim that
///   arbitrary user subprocesses were stopped);
/// * every agent acknowledgement to be prepared with writers paused and no
///   per-agent error;
/// * clean off-register retirements.
pub fn map_to_systemd_reply(
    request: &systemd::ControlRequest,
    response: Value,
) -> Result<systemd::ControlReply> {
    map_to_systemd_reply_with_process_verification(request, response, false)
}

/// Map a Core response after the caller performed additional running-process
/// verification (for example SHA-256 over `/proc/<pid>/exe` and comparing it
/// with the release manifest).
///
/// `process_image_verified` must only be `true` when such verification actually
/// happened in the calling tool; it is not evidence by itself.  Core's
/// `release.identity_verified` remains the primary signal, but the shipping
/// Core deliberately leaves it `false`.
pub fn map_to_systemd_reply_with_process_verification(
    request: &systemd::ControlRequest,
    response: Value,
    process_image_verified: bool,
) -> Result<systemd::ControlReply> {
    ensure!(
        request.protocol == systemd::CONTROL_PROTOCOL,
        "refusing request with protocol {:?}, expected {:?}",
        request.protocol,
        systemd::CONTROL_PROTOCOL
    );
    ensure!(
        !request.instance.is_empty() && request.instance.len() <= MAX_ID_BYTES,
        "control request instance must be 1..={MAX_ID_BYTES} bytes"
    );
    if let Some(release) = request.release.as_deref() {
        ensure!(
            safe_systemd_release(release),
            "control request release is not a safe systemd identifier: {release:?}"
        );
    }

    let object = response
        .as_object()
        .context("Core control response must be a JSON object")?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .context("Core control response is missing boolean `ok`")?;
    let core_operation = object
        .get("operation")
        .and_then(Value::as_str)
        .context("Core control response is missing `operation`")?;
    let transaction_id = object
        .get("transaction_id")
        .and_then(Value::as_str)
        .context("Core control response is missing `transaction_id`")?;
    ensure!(
        !transaction_id.is_empty() && transaction_id.len() <= MAX_ID_BYTES,
        "Core control response transaction_id must be 1..={MAX_ID_BYTES} bytes"
    );

    if !ok {
        return Err(core_rejection(core_operation, &response));
    }

    let identity = core_identity(&response)?;
    let declared = identity.declared.as_deref();
    if let Some(expected) = request.release.as_deref() {
        let declared = declared.with_context(|| {
            format!(
                "Core response does not declare the release expected by the request ({expected:?})"
            )
        })?;
        ensure!(
            declared == expected,
            "Core declares release {declared:?}, request expected {expected:?}"
        );
    }

    match request.operation {
        systemd::ControlOperation::PrepareUpgrade => {
            ensure!(
                core_operation == "prepare",
                "Core operation {core_operation:?} cannot satisfy PrepareUpgrade"
            );
            require_bool(object, "prepared", true)?;
            require_bool(object, "maintenance_held", true)?;
            let writer_scope = object
                .get("writer_scope")
                .and_then(Value::as_object)
                .context("prepare response is missing writer_scope")?;
            require_bool(writer_scope, "all_controlled_writers_paused", true)?;
            require_bool(writer_scope, "management_commands_quiesced", true)?;
            let covered = writer_scope
                .get("covered")
                .and_then(Value::as_array)
                .context("writer_scope.covered must be an array")?;
            ensure!(
                !covered.is_empty(),
                "prepare response does not enumerate any covered writers"
            );
            let not_covered = writer_scope
                .get("not_covered")
                .and_then(Value::as_array)
                .context("writer_scope.not_covered must be an array")?;
            ensure!(
                not_covered
                    .iter()
                    .any(|entry| entry.as_str() == Some("user_arbitrary_subprocesses")),
                "prepare response does not mark user_arbitrary_subprocesses as uncovered; \
                 refusing to claim complete writer coverage"
            );

            let agents = object
                .get("agents")
                .and_then(Value::as_array)
                .context("prepare response is missing agent acknowledgements")?;
            ensure!(
                !agents.is_empty(),
                "prepare response contains no agent acknowledgements"
            );
            for agent in agents {
                let agent = agent
                    .as_object()
                    .context("prepare response contains a non-object agent acknowledgement")?;
                let id = agent
                    .get("id")
                    .and_then(Value::as_str)
                    .context("agent acknowledgement is missing id")?;
                require_bool(agent, "prepared", true)
                    .with_context(|| format!("agent {id:?} is not prepared"))?;
                require_bool(agent, "writers_paused", true)
                    .with_context(|| format!("agent {id:?} did not pause writers"))?;
                if let Some(error) = agent.get("error") {
                    ensure!(
                        error.is_null(),
                        "agent {id:?} reported an error: {}",
                        bounded_text(&error.to_string(), MAX_ERROR_TEXT_CHARS)
                    );
                }
            }

            let retirements = object
                .get("off_register_retirements")
                .and_then(Value::as_object)
                .context("prepare response is missing off_register_retirements")?;
            require_bool(retirements, "checked", true)?;
            require_empty_array(retirements, "failed")?;
            require_empty_array(retirements, "retiring_ids")?;

            let requires = object
                .get("requires")
                .and_then(Value::as_array)
                .context("prepare response is missing requires")?;
            ensure!(
                requires.iter().any(|entry| {
                    entry.as_str() == Some("external_stop_user_arbitrary_subprocesses")
                }),
                "prepare response does not require external stopping of arbitrary subprocesses"
            );

            let mut reply = systemd::ControlReply::ack(
                systemd::ControlOperation::PrepareUpgrade,
                request.instance.clone(),
            );
            reply.maintenance = Some(true);
            reply.current_release = declared
                .filter(|release| safe_systemd_release(release))
                .map(str::to_owned);
            Ok(reply)
        }
        systemd::ControlOperation::Ready => {
            ensure!(
                core_operation == "ready",
                "Core operation {core_operation:?} cannot satisfy Ready"
            );
            require_bool(object, "ready", true)?;
            // Even when Core says `ready: true`, a contradictory module check
            // (`modules_verified: false` / `required_modules_mounted: false`)
            // must not be mapped into a successful systemd Ready reply.
            verify_required_modules(&response, &[])?;
            let checks = object
                .get("checks")
                .and_then(Value::as_object)
                .context("ready response is missing checks")?;
            for check in [
                "configured_enabled_personas_running",
                "trunks_loaded",
                "no_retiring_personas",
                "release_matches_expected",
            ] {
                require_bool(checks, check, true)?;
            }
            let retirements = object
                .get("off_register_retirements")
                .and_then(Value::as_object)
                .context("ready response is missing off_register_retirements")?;
            require_bool(retirements, "checked", true)?;
            require_empty_array(retirements, "failed")?;
            require_empty_array(retirements, "retiring_ids")?;

            let expected = request
                .release
                .as_deref()
                .context("Ready request must name an expected release")?;
            ensure!(
                safe_systemd_release(expected),
                "Ready request release is not a safe systemd identifier: {expected:?}"
            );
            let declared = declared.context("Core ready response does not declare a release")?;
            ensure!(
                declared == expected,
                "Core ready declares release {declared:?}, expected {expected:?}"
            );
            ensure!(
                safe_systemd_release(declared),
                "Core ready release is not a safe systemd identifier: {declared:?}"
            );
            ensure!(
                identity.identity_verified || process_image_verified,
                "refusing Ready: Core reports identity_verified=false ({}); a declared \
                 ECHO_RELEASE_ID is not build or running-process verification. Verify the \
                 running process image in the tool first, then opt in explicitly. \
                 Limitations: {}",
                identity.verification,
                identity.limitations.join("; ")
            );

            let mut reply = systemd::ControlReply::ack(
                systemd::ControlOperation::Ready,
                request.instance.clone(),
            );
            reply.ready = Some(true);
            reply.current_release = Some(declared.to_owned());
            Ok(reply)
        }
        other => bail!(
            "systemd operation {other:?} has no Core deployment-control equivalent; \
             refusing to fabricate a reply"
        ),
    }
}

fn require_bool(object: &serde_json::Map<String, Value>, key: &str, expected: bool) -> Result<()> {
    let actual = object
        .get(key)
        .and_then(Value::as_bool)
        .with_context(|| format!("missing boolean `{key}`"))?;
    ensure!(
        actual == expected,
        "`{key}` is {actual}, expected {expected}"
    );
    Ok(())
}

fn require_empty_array(object: &serde_json::Map<String, Value>, key: &str) -> Result<()> {
    let values = object
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("`{key}` must be an array"))?;
    ensure!(values.is_empty(), "`{key}` must be empty, got {values:?}");
    Ok(())
}

fn safe_systemd_release(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn core_rejection(core_operation: &str, response: &Value) -> anyhow::Error {
    let error = response
        .as_object()
        .and_then(|object| object.get("error"))
        .and_then(Value::as_object);
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("no error detail");
    anyhow::anyhow!(
        "Core rejected {core_operation} ({}): {}",
        bounded_text(code, MAX_ERROR_TEXT_CHARS),
        bounded_text(message, MAX_ERROR_TEXT_CHARS)
    )
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn remaining_time(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Core control query timed out");
    Ok(remaining)
}

fn write_all_deadline(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        let remaining = remaining_time(deadline)?;
        stream
            .set_write_timeout(Some(remaining))
            .context("set Core control socket write timeout")?;
        match stream.write(bytes) {
            Ok(0) => bail!("Core control socket accepted zero bytes"),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_timeout(&error) => bail!("Core control query timed out while writing"),
            Err(error) => return Err(error).context("write Core control request"),
        }
    }
    Ok(())
}

fn read_line_deadline(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let remaining = remaining_time(deadline)?;
        stream
            .set_read_timeout(Some(remaining))
            .context("set Core control socket read timeout")?;
        match stream.read(&mut buffer) {
            Ok(0) => {
                if line.is_empty() {
                    bail!("Core control socket closed before a response line");
                }
                bail!(
                    "Core control socket closed before a complete response line ({} bytes)",
                    line.len()
                );
            }
            Ok(count) => {
                if let Some(newline) = buffer[..count].iter().position(|byte| *byte == b'\n') {
                    line.extend_from_slice(&buffer[..newline]);
                    ensure!(
                        line.len() <= MAX_RESPONSE_BYTES,
                        "Core control response exceeds {MAX_RESPONSE_BYTES} bytes"
                    );
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    ensure!(!line.is_empty(), "Core control response line is empty");
                    return Ok(line);
                }
                line.extend_from_slice(&buffer[..count]);
                ensure!(
                    line.len() <= MAX_RESPONSE_BYTES,
                    "Core control response exceeds {MAX_RESPONSE_BYTES} bytes"
                );
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_timeout(&error) => {
                bail!("Core control query timed out while reading the response")
            }
            Err(error) => return Err(error).context("read Core control response"),
        }
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
