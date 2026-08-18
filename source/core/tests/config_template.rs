//! Config template integrity: the shipped `config/echo-agent-core.toml` must
//! parse as valid TOML with the expected top-level sections, so a template
//! that drifts from the runtime schema fails CI instead of the first boot.

use std::path::Path;

#[test]
fn config_template_is_valid_toml_with_expected_sections() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("config/echo-agent-core.toml");
    assert!(
        path.exists(),
        "config template missing at {}",
        path.display()
    );
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // 1. The template is valid TOML.
    let root: toml::Table = toml::from_str(&content)
        .unwrap_or_else(|e| panic!("template {} is not valid TOML: {e}", path.display()));

    // 2. The sections the runtime reads are present.
    for section in ["logging", "agent", "core"] {
        assert!(
            root.contains_key(section),
            "template missing [{section}] section"
        );
    }
    assert!(
        root["agent"].get("provider").is_some(),
        "template [agent] must set a provider"
    );
}
