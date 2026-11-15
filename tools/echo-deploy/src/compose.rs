//! Immutable Compose deployment planning.
//!
//! This module renders a Compose document and a non-executable deployment plan.
//! It never invokes Docker, never pulls or starts containers, never reads
//! configuration or credential files, and never writes to the filesystem.
//!
//! Every image must be pinned as `<repository>@sha256:<64 lowercase hex>`.
//! Tags (including `latest`) and tag-only references are rejected. The Compose
//! document is emitted as pretty JSON, which is a valid YAML 1.2 document; this
//! avoids hand-written YAML escaping.
//!
//! Activation is intentionally unsupported. The runtime ACK, maintenance,
//! readiness and rollback barriers are reported as required but not
//! implemented, so [`ComposePlan::activation_supported`] is always `false`.
//! This planner does not claim whole-stack rollback and never emits secrets.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

pub const COMPOSE_SCHEMA_VERSION: u32 = 1;
pub const CORE_STOP_GRACE_SECONDS: u32 = 300;
pub const PANEL_STOP_GRACE_SECONDS: u32 = 30;
pub const PANEL_PORT: u16 = 8080;
pub const NAPCAT_WEBUI_PORT: u16 = 6099;
/// The activation/runtime gate is intentionally absent from this phase.
pub const ACTIVATION_SUPPORTED: bool = false;

/// Serde default for inputs that omit the schema marker; validation still
/// requires the supported version.
pub fn default_schema_version() -> u32 {
    COMPOSE_SCHEMA_VERSION
}

const NETWORK_KEY: &str = "echo-agent";
const RESTART_POLICY: &str = "unless-stopped";

/// Deployment shape. `embedded` has no separate Panel service; `standalone`
/// requires one. The Compose service names stay `core`/`panel`/`napcat` so
/// in-container DNS matches the current npm Compose contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeTopology {
    CoreOnly,
    Embedded,
    Standalone,
}

impl ComposeTopology {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CoreOnly => "core_only",
            Self::Embedded => "embedded",
            Self::Standalone => "standalone",
        }
    }

    pub fn has_panel_service(self) -> bool {
        matches!(self, Self::Standalone)
    }
}

/// Strict input for one immutable Compose plan.
///
/// All paths are absolute, lexical, normal host paths; `~`, `.` and `..` are
/// never expanded. `panel_image` is required for `standalone` and forbidden for
/// `core_only`/`embedded`, matching the release topology table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeSpec {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub instance_id: String,
    pub core_image: String,
    #[serde(default)]
    pub panel_image: Option<String>,
    #[serde(default)]
    pub napcat_image: Option<String>,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub topology: ComposeTopology,
}

impl ComposeSpec {
    /// Validate the lexical contract. No filesystem or container access.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == COMPOSE_SCHEMA_VERSION,
            "unsupported compose schema"
        );
        validate_instance_id(&self.instance_id)?;
        parse_image_pin(&self.core_image).context("core_image")?;
        match self.topology {
            ComposeTopology::Standalone => {
                let panel = self
                    .panel_image
                    .as_deref()
                    .context("standalone topology requires panel_image")?;
                parse_image_pin(panel).context("panel_image")?;
            }
            ComposeTopology::CoreOnly | ComposeTopology::Embedded => ensure!(
                self.panel_image.is_none(),
                "panel_image is only valid with standalone topology"
            ),
        }
        if let Some(napcat) = &self.napcat_image {
            parse_image_pin(napcat).context("napcat_image")?;
        }
        validate_absolute_path(&self.config_dir, "config_dir")?;
        validate_absolute_path(&self.data_dir, "data_dir")?;
        ensure!(
            self.config_dir != self.data_dir,
            "config_dir and data_dir must differ"
        );
        ensure!(
            !self.config_dir.starts_with(&self.data_dir)
                && !self.data_dir.starts_with(&self.config_dir),
            "config_dir and data_dir must not overlap"
        );
        validate_volume_source(&self.config_dir, "config_dir")?;
        validate_volume_source(&self.data_dir, "data_dir")?;
        Ok(())
    }

    pub fn render(&self) -> Result<RenderedCompose> {
        render(self)
    }

    pub fn plan(&self) -> Result<ComposePlan> {
        Ok(render(self)?.plan)
    }

    pub fn stop_budget_seconds(&self) -> Result<u32> {
        stop_budget_seconds(self)
    }
}

/// Parsed digest pin for one image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePin {
    pub repository: String,
    /// Full digest, including the `sha256:` prefix.
    pub digest: String,
}

/// Image pin together with the Compose service that uses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedImage {
    pub service: String,
    pub image: String,
    pub repository: String,
    pub digest: String,
}

/// A bind mount that must survive stop/update operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedMount {
    pub service: String,
    pub host_path: PathBuf,
    pub container_path: String,
    pub mode: String,
}

/// Fixed container owner. Not part of the current input contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedUidGid {
    pub uid: u32,
    pub gid: u32,
}

/// One activation barrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Barrier {
    pub required: bool,
    pub implemented: bool,
    pub detail: String,
}

/// Barriers that must pass before a plan could ever be activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationPreconditions {
    pub runtime_ack: Barrier,
    pub maintenance_gate: Barrier,
    pub readiness: Barrier,
    pub rollback_barrier: Barrier,
}

/// Explicitly negative rollback statement. Per-image digest pinning is not an
/// atomic whole-stack rollback and does not prove data compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub whole_stack_rollback_supported: bool,
    pub data_compatibility_checked: bool,
    pub note: String,
}

/// Rendered Compose text plus the deployment plan. The plan is a preview; it
/// is not an approval and must not be executed by this tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedCompose {
    pub schema_version: u32,
    pub instance_id: String,
    pub topology: ComposeTopology,
    /// Pretty JSON, therefore a valid YAML 1.2 document.
    pub compose_yaml: String,
    pub plan: ComposePlan,
}

impl RenderedCompose {
    pub fn yaml(&self) -> &str {
        &self.compose_yaml
    }

    pub fn plan(&self) -> &ComposePlan {
        &self.plan
    }
}

/// Non-executable deployment plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposePlan {
    pub schema_version: u32,
    pub instance_id: String,
    pub topology: ComposeTopology,
    /// Compose service names in rendered order.
    pub services: Vec<String>,
    pub images: Vec<PinnedImage>,
    pub preserved_mounts: Vec<PreservedMount>,
    pub config_preserved: bool,
    pub data_preserved: bool,
    /// Core plus (when standalone) Panel. Optional NapCat is not included.
    pub stop_budget_seconds: u32,
    pub fixed_uid_gid: Option<FixedUidGid>,
    pub compose_sha256: String,
    pub plan_preview: bool,
    pub activation_supported: bool,
    pub execution_supported: bool,
    pub pull_supported: bool,
    pub up_supported: bool,
    pub preconditions: ActivationPreconditions,
    pub rollback: RollbackPlan,
    pub pending_activation_requirements: Vec<String>,
    pub notes: Vec<String>,
}

/// Runtime ACK, maintenance, readiness and rollback barriers are all required
/// and all unimplemented. This list is deliberately explicit.
pub const PENDING_ACTIVATION_REQUIREMENTS: &[&str] = &[
    "runtime ACK: Core drain and checked-save acknowledgement (not implemented)",
    "maintenance gate: coordinated Core/Panel/browser maintenance window (not implemented)",
    "application ready: per-service and application-level readiness checks (not implemented)",
    "rollback barrier: whole-stack rollback and data compatibility checks (not implemented)",
    "compose execution: pull/start/stop/remove is not performed by this planner (not implemented)",
    "fixed UID/GID: ownership mapping is not part of the current ComposeSpec (not implemented)",
];

/// Render a Compose document and its plan. No external command is executed.
pub fn render(spec: &ComposeSpec) -> Result<RenderedCompose> {
    spec.validate()?;

    let core_pin = parse_image_pin(&spec.core_image).context("core_image")?;
    let panel_pin = spec
        .panel_image
        .as_deref()
        .map(parse_image_pin)
        .transpose()
        .context("panel_image")?;
    let napcat_pin = spec
        .napcat_image
        .as_deref()
        .map(parse_image_pin)
        .transpose()
        .context("napcat_image")?;

    let mut service_names = vec!["core".to_string()];
    let mut images = vec![PinnedImage {
        service: "core".to_string(),
        image: spec.core_image.clone(),
        repository: core_pin.repository,
        digest: core_pin.digest,
    }];
    let mut preserved_mounts = vec![
        PreservedMount {
            service: "core".to_string(),
            host_path: spec.config_dir.clone(),
            container_path: "/config".to_string(),
            mode: "rw".to_string(),
        },
        PreservedMount {
            service: "core".to_string(),
            host_path: spec.data_dir.clone(),
            container_path: "/data".to_string(),
            mode: "rw".to_string(),
        },
    ];

    let core_service = ComposeService {
        image: spec.core_image.clone(),
        container_name: container_name(spec, "core"),
        restart: RESTART_POLICY.to_string(),
        stop_grace_period: Some(format!("{CORE_STOP_GRACE_SECONDS}s")),
        depends_on: Vec::new(),
        ports: Vec::new(),
        volumes: vec![
            bind_mount(&spec.config_dir, "/config")?,
            bind_mount(&spec.data_dir, "/data")?,
        ],
        environment: BTreeMap::from([("ECHO_MEDIA_DIR".to_string(), "/data/media".to_string())]),
        networks: vec![NETWORK_KEY.to_string()],
    };
    let mut panel_service = None;

    if spec.topology.has_panel_service() {
        let panel_image = spec
            .panel_image
            .as_ref()
            .context("standalone topology requires panel_image")?;
        let panel_pin = panel_pin
            .as_ref()
            .context("standalone topology requires a valid panel_image")?;
        service_names.push("panel".to_string());
        images.push(PinnedImage {
            service: "panel".to_string(),
            image: panel_image.clone(),
            repository: panel_pin.repository.clone(),
            digest: panel_pin.digest.clone(),
        });
        preserved_mounts.push(PreservedMount {
            service: "panel".to_string(),
            host_path: spec.config_dir.clone(),
            container_path: "/config".to_string(),
            mode: "rw".to_string(),
        });
        preserved_mounts.push(PreservedMount {
            service: "panel".to_string(),
            host_path: spec.data_dir.clone(),
            container_path: "/data".to_string(),
            mode: "rw".to_string(),
        });
        panel_service = Some(ComposeService {
            image: panel_image.clone(),
            container_name: container_name(spec, "panel"),
            restart: RESTART_POLICY.to_string(),
            stop_grace_period: Some(format!("{PANEL_STOP_GRACE_SECONDS}s")),
            depends_on: vec!["core".to_string()],
            ports: vec![format!("{PANEL_PORT}:{PANEL_PORT}")],
            volumes: vec![
                bind_mount(&spec.config_dir, "/config")?,
                bind_mount(&spec.data_dir, "/data")?,
            ],
            environment: BTreeMap::new(),
            networks: vec![NETWORK_KEY.to_string()],
        });
    }

    let mut named_volumes: BTreeMap<String, EmptyVolume> = BTreeMap::new();
    let mut napcat_service = None;
    if let (Some(napcat_image), Some(napcat_pin)) = (&spec.napcat_image, &napcat_pin) {
        service_names.push("napcat".to_string());
        images.push(PinnedImage {
            service: "napcat".to_string(),
            image: napcat_image.clone(),
            repository: napcat_pin.repository.clone(),
            digest: napcat_pin.digest.clone(),
        });
        let data_volume = volume_name(spec, "napcat-data");
        let config_volume = volume_name(spec, "napcat-config");
        named_volumes.insert(data_volume.clone(), EmptyVolume {});
        named_volumes.insert(config_volume.clone(), EmptyVolume {});
        napcat_service = Some(ComposeService {
            image: napcat_image.clone(),
            container_name: container_name(spec, "napcat"),
            restart: RESTART_POLICY.to_string(),
            stop_grace_period: None,
            depends_on: Vec::new(),
            ports: vec![format!("{NAPCAT_WEBUI_PORT}:{NAPCAT_WEBUI_PORT}")],
            volumes: vec![
                format!("{data_volume}:/app/napcat/data"),
                format!("{config_volume}:/app/napcat/config"),
            ],
            environment: BTreeMap::new(),
            networks: vec![NETWORK_KEY.to_string()],
        });
    }

    let document = ComposeDocument {
        services: ComposeServices {
            core: core_service,
            panel: panel_service,
            napcat: napcat_service,
        },
        networks: BTreeMap::from([(
            NETWORK_KEY,
            ComposeNetwork {
                name: network_name(spec),
            },
        )]),
        volumes: named_volumes,
    };

    let mut compose_yaml =
        serde_json::to_string_pretty(&document).context("serialize compose document")?;
    compose_yaml.push('\n');
    let compose_sha256 = format!("{:x}", Sha256::digest(compose_yaml.as_bytes()));

    let plan = ComposePlan {
        schema_version: COMPOSE_SCHEMA_VERSION,
        instance_id: spec.instance_id.clone(),
        topology: spec.topology,
        services: service_names,
        images,
        preserved_mounts,
        config_preserved: true,
        data_preserved: true,
        stop_budget_seconds: stop_budget(spec),
        fixed_uid_gid: None,
        compose_sha256,
        plan_preview: true,
        activation_supported: ACTIVATION_SUPPORTED,
        execution_supported: false,
        pull_supported: false,
        up_supported: false,
        preconditions: ActivationPreconditions {
            runtime_ack: Barrier {
                required: true,
                implemented: false,
                detail: "runtime ACK: Core drain/checked-save acknowledgement is not implemented"
                    .to_string(),
            },
            maintenance_gate: Barrier {
                required: true,
                implemented: false,
                detail:
                    "maintenance gate: coordinated Core/Panel/browser drain is not implemented"
                        .to_string(),
            },
            readiness: Barrier {
                required: true,
                implemented: false,
                detail: "readiness: per-service and application-level ready checks are not implemented"
                    .to_string(),
            },
            rollback_barrier: Barrier {
                required: true,
                implemented: false,
                detail:
                    "rollback barrier: atomic whole-stack rollback and data compatibility checks are not implemented"
                        .to_string(),
            },
        },
        rollback: RollbackPlan {
            whole_stack_rollback_supported: false,
            data_compatibility_checked: false,
            note: "Not implemented: changing image digests back is neither an atomic whole-stack rollback nor proof of data compatibility."
                .to_string(),
        },
        pending_activation_requirements: PENDING_ACTIVATION_REQUIREMENTS
            .iter()
            .map(|item| (*item).to_string())
            .collect(),
        notes: vec![
            "compose document is pretty JSON, a valid YAML 1.2 subset; no hand-written YAML escaping"
                .to_string(),
            "all images are pinned by sha256 digest; tags and latest are rejected".to_string(),
            "config_dir and data_dir are preserved as read-write bind mounts for core/panel"
                .to_string(),
            "no secrets, credentials, config file contents or host environment are read or rendered"
                .to_string(),
            "the Docker socket is not mounted; Docker-API-dependent NapCat features are outside this plan"
                .to_string(),
            "fixed UID/GID is not configured; containers use image defaults".to_string(),
            "no whole-stack rollback is claimed or implemented".to_string(),
        ],
    };

    Ok(RenderedCompose {
        schema_version: COMPOSE_SCHEMA_VERSION,
        instance_id: spec.instance_id.clone(),
        topology: spec.topology,
        compose_yaml,
        plan,
    })
}

/// Render only the Compose text.
pub fn render_yaml(spec: &ComposeSpec) -> Result<String> {
    Ok(render(spec)?.compose_yaml)
}

/// Build only the deployment plan. It still validates and renders the document
/// so the reported digest always matches the YAML returned by [`render`].
pub fn plan(spec: &ComposeSpec) -> Result<ComposePlan> {
    Ok(render(spec)?.plan)
}

/// Bounded stop budget: core 300 s plus panel 30 s when standalone. The
/// optional NapCat service has no explicit stop budget in this phase.
pub fn stop_budget_seconds(spec: &ComposeSpec) -> Result<u32> {
    spec.validate()?;
    Ok(stop_budget(spec))
}

fn stop_budget(spec: &ComposeSpec) -> u32 {
    CORE_STOP_GRACE_SECONDS
        + if spec.topology.has_panel_service() {
            PANEL_STOP_GRACE_SECONDS
        } else {
            0
        }
}

fn container_name(spec: &ComposeSpec, service: &str) -> String {
    format!("echo-agent-{}-{service}", spec.instance_id)
}

fn network_name(spec: &ComposeSpec) -> String {
    format!("echo-agent-{}", spec.instance_id)
}

fn volume_name(spec: &ComposeSpec, suffix: &str) -> String {
    format!("echo-agent-{}-{suffix}", spec.instance_id)
}

fn bind_mount(host: &Path, container: &str) -> Result<String> {
    let host = host.to_str().context("bind mount path must be UTF-8")?;
    Ok(format!("{host}:{container}"))
}

/// Parse `<repository>@sha256:<64 lowercase hex>`. Tags are not accepted, even
/// when a digest is also present.
pub fn parse_image_pin(image: &str) -> Result<ImagePin> {
    ensure!(
        !image.is_empty() && image.len() <= 512,
        "invalid image reference length"
    );
    ensure!(image.is_ascii(), "image reference must be ASCII");
    ensure!(
        !image
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace()),
        "image reference contains whitespace or a control character"
    );
    let (repository, digest) = image
        .split_once('@')
        .context("image must be pinned as <repository>@sha256:<64 lowercase hex>")?;
    ensure!(
        !digest.contains('@'),
        "image must contain exactly one @ digest separator"
    );
    let hex = digest
        .strip_prefix("sha256:")
        .context("image digest must use sha256")?;
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "image digest must be 64 lowercase hex characters"
    );
    validate_repository(repository)?;
    Ok(ImagePin {
        repository: repository.to_string(),
        digest: format!("sha256:{hex}"),
    })
}

/// Validate an image pin without returning the parsed parts.
pub fn validate_image_pin(image: &str) -> Result<()> {
    parse_image_pin(image).map(|_| ())
}

/// Validate a strict instance token used in container, network and volume
/// names. No path separators, whitespace or consecutive dots.
pub fn validate_instance_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 100,
        "invalid instance_id length"
    );
    ensure!(value.is_ascii(), "instance_id must be ASCII");
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

/// Absolute, lexical, normal path: no root, no `.`/`..`, no empty component,
/// no trailing slash, no control characters, UTF-8.
pub fn validate_absolute_path(path: &Path, field: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{field} must be absolute");
    let text = path
        .to_str()
        .with_context(|| format!("{field} must be UTF-8"))?;
    ensure!(
        text.len() <= 4096,
        "{field} exceeds the maximum supported length"
    );
    ensure!(!text.is_empty() && text != "/", "{field} must not be root");
    ensure!(
        !text.chars().any(char::is_control),
        "{field} contains a control character"
    );
    let mut parts = text.split('/');
    ensure!(parts.next() == Some(""), "{field} must be absolute");
    for part in parts {
        ensure!(!part.is_empty(), "{field} contains an empty component");
        ensure!(
            part != "." && part != "..",
            "{field} contains a non-normal component"
        );
    }
    ensure!(path.parent().is_some(), "{field} must have a parent");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "{field} contains a non-normal component"
    );
    Ok(())
}

/// Compose volume short syntax uses `:` as the host/container separator and
/// interpolates `$`; reject both so a rendered plan cannot be misparsed.
fn validate_volume_source(path: &Path, field: &str) -> Result<()> {
    let text = path
        .to_str()
        .with_context(|| format!("{field} must be UTF-8"))?;
    ensure!(
        !text.contains(':'),
        "{field} must not contain ':' (Compose volume separator)"
    );
    ensure!(
        !text.contains('$'),
        "{field} must not contain '$' (Compose interpolation)"
    );
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    ensure!(
        !repository.is_empty() && repository.len() <= 255,
        "invalid image repository length"
    );
    ensure!(repository.is_ascii(), "image repository must be ASCII");
    ensure!(
        !repository.bytes().any(|byte| byte.is_ascii_uppercase()),
        "image repository must be lowercase"
    );
    ensure!(
        !repository.starts_with('/') && !repository.ends_with('/'),
        "image repository must not start or end with '/'"
    );
    ensure!(
        !repository.contains("//"),
        "image repository contains an empty component"
    );
    let parts: Vec<&str> = repository.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        ensure!(
            !part.is_empty(),
            "image repository contains an empty component"
        );
        ensure!(
            *part != "." && *part != "..",
            "image repository contains a non-normal component"
        );
        if index == 0 && part.contains(':') {
            ensure!(
                parts.len() > 1,
                "a registry port requires a repository path"
            );
            let (host, port) = part
                .split_once(':')
                .context("invalid registry host and port")?;
            ensure!(!host.is_empty(), "registry host must not be empty");
            ensure!(
                port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit()),
                "registry port must be numeric"
            );
            let port: u32 = port.parse().context("invalid registry port")?;
            ensure!((1..=65535).contains(&port), "registry port is out of range");
            ensure!(
                host.as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                    && host
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric),
                "registry host must start and end with an alphanumeric character"
            );
            ensure!(
                host.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-')),
                "invalid registry host character"
            );
        } else {
            ensure!(
                !part.contains(':'),
                "image tags are not allowed; pin with @sha256:<64 lowercase hex>"
            );
            ensure!(
                part.as_bytes()[0].is_ascii_alphanumeric()
                    && part
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric),
                "image repository component must start and end with alphanumeric"
            );
            ensure!(
                part.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')),
                "invalid image repository character"
            );
            ensure!(!part.contains(".."), "invalid image repository component");
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ComposeDocument {
    services: ComposeServices,
    networks: BTreeMap<&'static str, ComposeNetwork>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, EmptyVolume>,
}

#[derive(Debug, Serialize)]
struct ComposeServices {
    core: ComposeService,
    #[serde(skip_serializing_if = "Option::is_none")]
    panel: Option<ComposeService>,
    #[serde(skip_serializing_if = "Option::is_none")]
    napcat: Option<ComposeService>,
}

#[derive(Debug, Serialize)]
struct ComposeNetwork {
    name: String,
}

#[derive(Debug, Serialize)]
struct EmptyVolume {}

#[derive(Debug, Serialize)]
struct ComposeService {
    image: String,
    container_name: String,
    restart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_grace_period: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    volumes: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
    networks: Vec<String>,
}
