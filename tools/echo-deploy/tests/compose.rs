// The module is included directly so this test target does not require a
// `pub mod compose;` line in src/lib.rs; the CLI wiring is a separate change.
#[path = "../src/compose.rs"]
#[allow(dead_code)]
mod compose;

use compose::{
    parse_image_pin, plan, render, render_yaml, stop_budget_seconds, validate_image_pin,
    ComposeSpec, ComposeTopology, ACTIVATION_SUPPORTED, CORE_STOP_GRACE_SECONDS,
    PANEL_STOP_GRACE_SECONDS,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const CORE_REPO: &str = "ghcr.io/echolab-auto/echo-agent-core";
const PANEL_REPO: &str = "ghcr.io/echolab-auto/echo-agent-panel";
const NAPCAT_REPO: &str = "ghcr.io/echolab-auto/echo-agent-napcat";
const CONFIG_DIR: &str = "/srv/echo-agent/config";
const DATA_DIR: &str = "/srv/echo-agent/data";

fn pinned(repository: &str, hex: char) -> String {
    format!("{repository}@sha256:{}", hex.to_string().repeat(64))
}

fn base_spec(topology: ComposeTopology) -> ComposeSpec {
    ComposeSpec {
        schema_version: 1,
        instance_id: "local-dev".to_string(),
        core_image: pinned(CORE_REPO, 'a'),
        panel_image: topology
            .has_panel_service()
            .then(|| pinned(PANEL_REPO, 'b')),
        napcat_image: None,
        config_dir: PathBuf::from(CONFIG_DIR),
        data_dir: PathBuf::from(DATA_DIR),
        topology,
    }
}

fn rendered_document(spec: &ComposeSpec) -> (compose::RenderedCompose, Value) {
    let rendered = render(spec).unwrap();
    let document: Value = serde_json::from_str(&rendered.compose_yaml).unwrap();
    (rendered, document)
}

#[test]
fn images_must_be_digest_pinned_lowercase_sha256() {
    let digest = "a".repeat(64);
    let valid = format!("{CORE_REPO}@sha256:{digest}");
    assert!(validate_image_pin(&valid).is_ok());
    let pin = parse_image_pin(&valid).unwrap();
    assert_eq!(pin.repository, CORE_REPO);
    assert_eq!(pin.digest, format!("sha256:{digest}"));

    // Registry ports are not tags.
    assert!(validate_image_pin(&format!("localhost:5000/team/app@sha256:{digest}")).is_ok());

    let invalid = [
        format!("{CORE_REPO}:latest"),
        format!("{CORE_REPO}:v1.2.3"),
        format!("{CORE_REPO}:latest@sha256:{digest}"),
        format!("{CORE_REPO}@sha256:{}", "A".repeat(64)),
        format!("{CORE_REPO}@sha256:{}", "a".repeat(63)),
        format!("{CORE_REPO}@sha256:{}", "a".repeat(65)),
        format!("{CORE_REPO}@sha512:{digest}"),
        format!("{CORE_REPO}@"),
        CORE_REPO.to_string(),
        format!("GHCR.IO/Team/App@sha256:{digest}"),
        format!("ghcr.io/team//app@sha256:{digest}"),
        format!("ghcr.io/team/app:@sha256:{digest}"),
        format!("localhost:5000@sha256:{digest}"),
        format!("ghcr.io/team/app@sha256:{digest}@sha256:{digest}"),
        String::new(),
    ];
    for image in invalid {
        assert!(
            validate_image_pin(&image).is_err(),
            "image should have been rejected: {image:?}"
        );
    }

    let mut spec = base_spec(ComposeTopology::CoreOnly);
    spec.core_image = format!("{CORE_REPO}:latest");
    assert!(spec.validate().is_err());

    let mut spec = base_spec(ComposeTopology::CoreOnly);
    spec.napcat_image = Some(format!("{NAPCAT_REPO}:latest"));
    assert!(spec.validate().is_err());
}

#[test]
fn topology_controls_service_shape_and_stop_policy() {
    let (rendered, document) = rendered_document(&base_spec(ComposeTopology::CoreOnly));
    let services = document["services"].as_object().unwrap();
    assert_eq!(services.len(), 1);
    assert!(services.contains_key("core"));
    assert!(!services.contains_key("panel"));
    let core = &services["core"];
    assert_eq!(core["image"], json!(pinned(CORE_REPO, 'a')));
    assert_eq!(core["stop_grace_period"], json!("300s"));
    assert_eq!(
        core["volumes"],
        json!([format!("{CONFIG_DIR}:/config"), format!("{DATA_DIR}:/data")])
    );
    assert_eq!(
        core["environment"],
        json!({"ECHO_MEDIA_DIR": "/data/media"})
    );
    assert_eq!(core["networks"], json!(["echo-agent"]));
    assert!(core.get("user").is_none());
    assert_eq!(
        document["networks"]["echo-agent"]["name"],
        json!("echo-agent-local-dev")
    );
    assert_eq!(
        document["services"]["core"]["container_name"],
        json!("echo-agent-local-dev-core")
    );
    assert_eq!(rendered.plan.services, vec!["core".to_string()]);
    assert_eq!(rendered.plan.preserved_mounts.len(), 2);

    let (_, embedded) = rendered_document(&base_spec(ComposeTopology::Embedded));
    let embedded_services = embedded["services"].as_object().unwrap();
    assert_eq!(embedded_services.len(), 1);
    assert!(!embedded_services.contains_key("panel"));

    let (rendered, document) = rendered_document(&base_spec(ComposeTopology::Standalone));
    let services = document["services"].as_object().unwrap();
    assert_eq!(services.len(), 2);
    let panel = &services["panel"];
    assert_eq!(panel["image"], json!(pinned(PANEL_REPO, 'b')));
    assert_eq!(panel["stop_grace_period"], json!("30s"));
    assert_eq!(panel["depends_on"], json!(["core"]));
    assert_eq!(panel["ports"], json!(["8080:8080"]));
    assert_eq!(
        panel["volumes"],
        json!([format!("{CONFIG_DIR}:/config"), format!("{DATA_DIR}:/data")])
    );
    assert_eq!(rendered.plan.services, vec!["core", "panel"]);
    assert_eq!(rendered.plan.preserved_mounts.len(), 4);

    let mut core_only_with_panel = base_spec(ComposeTopology::CoreOnly);
    core_only_with_panel.panel_image = Some(pinned(PANEL_REPO, 'b'));
    assert!(core_only_with_panel.validate().is_err());
    let mut embedded_with_panel = base_spec(ComposeTopology::Embedded);
    embedded_with_panel.panel_image = Some(pinned(PANEL_REPO, 'b'));
    assert!(embedded_with_panel.validate().is_err());
    let mut standalone_without_panel = base_spec(ComposeTopology::Standalone);
    standalone_without_panel.panel_image = None;
    assert!(standalone_without_panel.validate().is_err());
}

#[test]
fn napcat_is_optional_and_pinned_when_present() {
    let mut spec = base_spec(ComposeTopology::Standalone);
    spec.napcat_image = Some(pinned(NAPCAT_REPO, 'c'));
    let (rendered, document) = rendered_document(&spec);
    let services = document["services"].as_object().unwrap();
    assert_eq!(services.len(), 3);
    let napcat = &services["napcat"];
    assert_eq!(napcat["image"], json!(pinned(NAPCAT_REPO, 'c')));
    assert_eq!(napcat["ports"], json!(["6099:6099"]));
    assert!(napcat.get("stop_grace_period").is_none());
    assert!(napcat.get("environment").is_none());
    let volumes = document["volumes"].as_object().unwrap();
    assert_eq!(volumes.len(), 2);
    assert!(volumes.contains_key("echo-agent-local-dev-napcat-data"));
    assert!(volumes.contains_key("echo-agent-local-dev-napcat-config"));
    assert_eq!(rendered.plan.services, vec!["core", "panel", "napcat"]);
    assert_eq!(rendered.plan.images.len(), 3);
    assert!(rendered
        .plan
        .images
        .iter()
        .all(|image| image.digest.starts_with("sha256:")));

    let without_napcat = base_spec(ComposeTopology::Standalone);
    let (_, document) = rendered_document(&without_napcat);
    assert!(document.get("volumes").is_none());
    assert!(!document["services"]
        .as_object()
        .unwrap()
        .contains_key("napcat"));
}

#[test]
fn paths_must_be_absolute_nonroot_normal_and_disjoint() {
    let bad_paths = [
        PathBuf::from(""),
        PathBuf::from("relative/path"),
        PathBuf::from("/"),
        PathBuf::from("/tmp/../escape"),
        PathBuf::from("/tmp/./escape"),
        PathBuf::from("/tmp//escape"),
        PathBuf::from("/tmp/escape/"),
        PathBuf::from("/tmp/escape\nnext"),
        PathBuf::from("/tmp/escape\tnext"),
        PathBuf::from("/srv/echo:agent/config"),
        PathBuf::from("/srv/$echo/config"),
    ];
    for path in bad_paths {
        let mut config = base_spec(ComposeTopology::CoreOnly);
        config.config_dir = path.clone();
        assert!(config.validate().is_err(), "config_dir {path:?}");

        let mut data = base_spec(ComposeTopology::CoreOnly);
        data.data_dir = path;
        assert!(data.validate().is_err(), "data_dir");
    }

    let mut same = base_spec(ComposeTopology::CoreOnly);
    same.config_dir = PathBuf::from("/srv/echo-agent");
    same.data_dir = PathBuf::from("/srv/echo-agent");
    assert!(same.validate().is_err());

    let mut nested = base_spec(ComposeTopology::CoreOnly);
    nested.config_dir = PathBuf::from("/srv/echo-agent");
    nested.data_dir = PathBuf::from("/srv/echo-agent/data");
    assert!(nested.validate().is_err());

    let mut reverse_nested = base_spec(ComposeTopology::CoreOnly);
    reverse_nested.config_dir = PathBuf::from("/srv/echo-agent/config");
    reverse_nested.data_dir = PathBuf::from("/srv/echo-agent");
    assert!(reverse_nested.validate().is_err());

    let mut siblings = base_spec(ComposeTopology::CoreOnly);
    siblings.config_dir = PathBuf::from("/srv/echo-agent/config");
    siblings.data_dir = PathBuf::from("/srv/echo-agent/data");
    assert!(siblings.validate().is_ok());

    let mut prefix_but_not_nested = base_spec(ComposeTopology::CoreOnly);
    prefix_but_not_nested.config_dir = PathBuf::from("/srv/echo-agent/config");
    prefix_but_not_nested.data_dir = PathBuf::from("/srv/echo-agent/config2");
    assert!(prefix_but_not_nested.validate().is_ok());
}

#[test]
fn rendered_compose_is_valid_json_yaml_and_contains_no_secrets() {
    let mut spec = base_spec(ComposeTopology::Standalone);
    spec.napcat_image = Some(pinned(NAPCAT_REPO, 'c'));
    let rendered = render(&spec).unwrap();
    assert!(rendered.compose_yaml.starts_with('{'));
    assert!(rendered.compose_yaml.ends_with("}\n"));

    // JSON is a valid YAML 1.2 document; parse it as data, not as text.
    let document: Value = serde_json::from_str(rendered.yaml()).unwrap();
    assert!(document.is_object());
    assert_eq!(
        document["services"]["core"]["image"],
        json!(pinned(CORE_REPO, 'a'))
    );

    let lower = rendered.compose_yaml.to_ascii_lowercase();
    for forbidden in [
        "latest",
        "password",
        "secret",
        "token",
        "api_key",
        "docker pull",
        "docker compose up",
        "docker up",
    ] {
        assert!(
            !lower.contains(forbidden),
            "compose document must not contain {forbidden:?}"
        );
    }

    let expected_sha = format!("{:x}", Sha256::digest(rendered.compose_yaml.as_bytes()));
    assert_eq!(rendered.plan.compose_sha256, expected_sha);
    assert_eq!(render_yaml(&spec).unwrap(), rendered.compose_yaml);
    assert_eq!(render(&spec).unwrap().compose_yaml, rendered.compose_yaml);
    assert_eq!(rendered.plan(), &rendered.plan);
}

#[test]
fn stop_budget_is_bounded_by_service_stop_grace_periods() {
    for (topology, expected) in [
        (ComposeTopology::CoreOnly, 300_u32),
        (ComposeTopology::Embedded, 300_u32),
        (ComposeTopology::Standalone, 330_u32),
    ] {
        let spec = base_spec(topology);
        assert_eq!(stop_budget_seconds(&spec).unwrap(), expected);
        assert_eq!(spec.stop_budget_seconds().unwrap(), expected);
        let (rendered, document) = rendered_document(&spec);
        assert_eq!(rendered.plan.stop_budget_seconds, expected);
        assert_eq!(
            document["services"]["core"]["stop_grace_period"],
            json!(format!("{CORE_STOP_GRACE_SECONDS}s"))
        );
        if topology.has_panel_service() {
            assert_eq!(
                document["services"]["panel"]["stop_grace_period"],
                json!(format!("{PANEL_STOP_GRACE_SECONDS}s"))
            );
            assert_eq!(
                rendered.plan.stop_budget_seconds,
                CORE_STOP_GRACE_SECONDS + PANEL_STOP_GRACE_SECONDS
            );
        }
    }

    let mut with_napcat = base_spec(ComposeTopology::Standalone);
    with_napcat.napcat_image = Some(pinned(NAPCAT_REPO, 'c'));
    assert_eq!(stop_budget_seconds(&with_napcat).unwrap(), 330);
}

#[test]
fn activation_and_barriers_are_explicitly_not_implemented() {
    let plan = plan(&base_spec(ComposeTopology::Standalone)).unwrap();
    assert!(plan.plan_preview);
    assert_eq!(plan.activation_supported, ACTIVATION_SUPPORTED);
    assert!(!plan.activation_supported);
    assert!(!plan.execution_supported);
    assert!(!plan.pull_supported);
    assert!(!plan.up_supported);
    assert!(plan.fixed_uid_gid.is_none());
    assert!(plan.config_preserved);
    assert!(plan.data_preserved);

    let barriers = [
        &plan.preconditions.runtime_ack,
        &plan.preconditions.maintenance_gate,
        &plan.preconditions.readiness,
        &plan.preconditions.rollback_barrier,
    ];
    for barrier in barriers {
        assert!(barrier.required);
        assert!(!barrier.implemented);
    }
    assert!(plan
        .preconditions
        .runtime_ack
        .detail
        .contains("runtime ACK"));
    assert!(plan
        .preconditions
        .maintenance_gate
        .detail
        .contains("maintenance"));
    assert!(plan.preconditions.readiness.detail.contains("ready"));
    assert!(plan
        .preconditions
        .rollback_barrier
        .detail
        .contains("rollback"));

    let requirements = plan
        .pending_activation_requirements
        .join(" ")
        .to_ascii_lowercase();
    for needle in ["runtime ack", "maintenance", "ready", "rollback"] {
        assert!(requirements.contains(needle), "missing {needle:?}");
    }
    assert!(!plan.rollback.whole_stack_rollback_supported);
    assert!(!plan.rollback.data_compatibility_checked);
    assert!(plan
        .notes
        .iter()
        .any(|note| note.contains("no whole-stack rollback")));

    let serialized = serde_json::to_string(&plan).unwrap().to_ascii_lowercase();
    for forbidden in ["docker pull", "docker compose up", "docker up"] {
        assert!(
            !serialized.contains(forbidden),
            "plan must not contain command {forbidden:?}"
        );
    }
}

#[test]
fn strict_contract_rejects_unknown_and_missing_fields() {
    let value = json!({
        "schema_version": 1,
        "instance_id": "local-dev",
        "core_image": pinned(CORE_REPO, 'a'),
        "config_dir": CONFIG_DIR,
        "data_dir": DATA_DIR,
        "topology": "core_only"
    });
    assert!(serde_json::from_value::<ComposeSpec>(value.clone()).is_ok());

    let without_schema = json!({
        "instance_id": "local-dev",
        "core_image": pinned(CORE_REPO, 'a'),
        "config_dir": CONFIG_DIR,
        "data_dir": DATA_DIR,
        "topology": "core_only"
    });
    let parsed: ComposeSpec = serde_json::from_value(without_schema).unwrap();
    assert_eq!(parsed.schema_version, 1);

    let mut unknown = value;
    unknown["unknown"] = json!(true);
    assert!(serde_json::from_value::<ComposeSpec>(unknown).is_err());

    let missing_core = json!({
        "schema_version": 1,
        "instance_id": "local-dev",
        "config_dir": CONFIG_DIR,
        "data_dir": DATA_DIR,
        "topology": "core_only"
    });
    assert!(serde_json::from_value::<ComposeSpec>(missing_core).is_err());

    let bad_topology = json!({
        "schema_version": 1,
        "instance_id": "local-dev",
        "core_image": pinned(CORE_REPO, 'a'),
        "config_dir": CONFIG_DIR,
        "data_dir": DATA_DIR,
        "topology": "swarm"
    });
    assert!(serde_json::from_value::<ComposeSpec>(bad_topology).is_err());

    let mut wrong_schema = base_spec(ComposeTopology::CoreOnly);
    wrong_schema.schema_version = 2;
    assert!(wrong_schema.validate().is_err());

    let mut empty_instance = base_spec(ComposeTopology::CoreOnly);
    empty_instance.instance_id.clear();
    assert!(empty_instance.validate().is_err());

    let mut path_instance = base_spec(ComposeTopology::CoreOnly);
    path_instance.instance_id = "../escape".to_string();
    assert!(path_instance.validate().is_err());
}
