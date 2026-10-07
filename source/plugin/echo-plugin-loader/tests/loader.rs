//! Integration tests for `echo-plugin-loader`.
//!
//! Fixtures live in a unique subdirectory of the system temp dir and are
//! removed on drop — no external tempfile dependency.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use echo_plugin_loader::{dump, load, EntryKind, LoadError, LoadedTree, PluginEntry};
use serde_json::json;

// ---------------------------------------------------------------- helpers --

/// A temp directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "echo-plugin-loader-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// Write a fixture file; returns its path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(&path, contents).expect("write fixture");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn load_ok(profile: &Path) -> LoadedTree {
    load(profile).expect("profile should load")
}

fn load_err(profile: &Path) -> LoadError {
    load(profile).expect_err("profile should fail to load")
}

fn entry<'a>(tree: &'a LoadedTree, id: &str) -> &'a PluginEntry {
    tree.entries
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("entry `{id}` missing"))
}

fn ids(tree: &LoadedTree) -> Vec<&str> {
    tree.entries.iter().map(|entry| entry.id.as_str()).collect()
}

/// base.toml + qq.toml bundles, local.toml patch (replaces `websearch`),
/// profile inline `extra` row — the canonical three-layer fixture.
fn three_layer_fixture(tag: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(tag);
    dir.write(
        "base.toml",
        r#"[[plugin]]
id = "tools"
kind = "builtin"
name = "tools"

[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
command = "/opt/echo/plugins/websearch"
args = ["--stdio"]
"#,
    );
    dir.write(
        "qq.toml",
        r#"[[plugin]]
id = "qq"
kind = "stdio"
name = "echo-plugin-qq"
command = "/opt/echo/plugins/qq"
env = { QQ_ONE_BOT = "1" }
"#,
    );
    dir.write(
        "local.toml",
        r#"[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
command = "/usr/local/bin/echo-plugin-websearch"
enabled = false
config = { api_key = "new" }
"#,
    );
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "default"
bundles = ["base", "qq"]
patches = ["local.toml"]

[[plugin]]
id = "extra"
kind = "builtin"
name = "extra"

[plugin.config]
api_key = "inline"
"#,
    );
    (dir, profile)
}

// ------------------------------------------------------------------ tests --

#[test]
fn three_layer_composition() {
    let (_dir, profile) = three_layer_fixture("compose");
    let tree = load_ok(&profile);

    // `websearch` keeps its bundle position (replace in place); `extra` is
    // appended by the profile's inline layer.
    assert_eq!(ids(&tree), vec!["tools", "websearch", "qq", "extra"]);
    assert!(tree.warnings.is_empty(), "warnings: {:?}", tree.warnings);

    let websearch = entry(&tree, "websearch");
    assert_eq!(websearch.kind, EntryKind::Stdio);
    assert_eq!(
        websearch.command.as_deref(),
        Some("/usr/local/bin/echo-plugin-websearch")
    );
    assert!(!websearch.enabled);
    assert!(
        websearch.args.is_empty(),
        "patch omitted args: old args must not survive"
    );
    assert_eq!(websearch.config, json!({ "api_key": "new" }));

    assert_eq!(
        entry(&tree, "qq").env.get("QQ_ONE_BOT").map(String::as_str),
        Some("1")
    );
    assert_eq!(entry(&tree, "extra").config, json!({ "api_key": "inline" }));
    assert_eq!(entry(&tree, "tools").kind, EntryKind::Builtin);
}

#[test]
fn provenance_records_layer_and_source() {
    let (_dir, profile) = three_layer_fixture("provenance");
    let tree = load_ok(&profile);

    let expected = [
        ("tools", 0usize, "base.toml"),
        ("qq", 1, "qq.toml"),
        ("websearch", 2, "local.toml"),
        ("extra", 3, "plugins.toml"),
    ];
    for (id, layer, source) in expected {
        let row = entry(&tree, id);
        assert_eq!(row.layer, layer, "layer of `{id}`");
        assert_eq!(row.layer_source, source, "layer source of `{id}`");
    }
}

#[test]
fn patch_replaces_whole_row_without_field_residue() {
    let dir = TempDir::new("replace");
    dir.write(
        "bundle.toml",
        r#"[[plugin]]
id = "tools"
kind = "builtin"
name = "tools"

[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
command = "/old/path"
args = ["--stdio", "--verbose"]
env = { TOKEN = "old" }
requires = ["tools"]
enabled = true

[plugin.config]
api_key = "old"
retries = 3
"#,
    );
    dir.write(
        "patch.toml",
        r#"[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
command = "/new/path"
enabled = false
config = { api_key = "new" }
"#,
    );
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "replace"
bundles = ["bundle"]
patches = ["patch"]
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["tools", "websearch"]);

    let websearch = entry(&tree, "websearch");
    assert_eq!(websearch.command.as_deref(), Some("/new/path"));
    assert!(!websearch.enabled);
    assert!(websearch.args.is_empty(), "old args must not survive");
    assert!(websearch.env.is_empty(), "old env must not survive");
    assert!(
        websearch.requires.is_empty(),
        "old requires must not survive"
    );
    assert_eq!(
        websearch.config,
        json!({ "api_key": "new" }),
        "config is replaced whole, not merged"
    );
    assert_eq!(
        websearch.layer, 1,
        "provenance moves to the replacing patch"
    );
    assert_eq!(websearch.layer_source, "patch.toml");
}

#[test]
fn stdio_without_command_is_a_validation_error() {
    let dir = TempDir::new("stdio-command");
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "broken"

[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
"#,
    );
    match load_err(&profile) {
        LoadError::Validation { message } => {
            assert!(
                message.contains("websearch"),
                "message names the entry: {message}"
            );
            assert!(
                message.contains("command"),
                "message explains the requirement: {message}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }

    let blank = dir.write(
        "blank.toml",
        r#"[profile]
name = "broken"

[[plugin]]
id = "blank-cmd"
kind = "stdio"
name = "echo-plugin-blank"
command = ""
"#,
    );
    assert!(matches!(load_err(&blank), LoadError::Validation { .. }));
}

#[test]
fn invalid_ids_are_validation_errors() {
    let dir = TempDir::new("bad-ids");
    let empty = dir.write(
        "empty-id.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = ""
kind = "builtin"
name = "x"
"#,
    );
    match load_err(&empty) {
        LoadError::Validation { message } => assert!(message.contains("empty id"), "{message}"),
        other => panic!("expected Validation, got {other:?}"),
    }

    let spaced = dir.write(
        "spaced-id.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "bad id"
kind = "builtin"
name = "x"
"#,
    );
    match load_err(&spaced) {
        LoadError::Validation { message } => {
            assert!(message.contains("whitespace"), "{message}")
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn missing_bundle_reports_the_file_name() {
    let dir = TempDir::new("missing");
    let missing_bundle = dir.write(
        "bundles.toml",
        r#"[profile]
name = "t"
bundles = ["ghost", "base"]
"#,
    );
    dir.write(
        "base.toml",
        "[[plugin]]\nid = \"base\"\nkind = \"builtin\"\nname = \"base\"\n",
    );
    match load_err(&missing_bundle) {
        LoadError::MissingBundle { name } => assert_eq!(name, "ghost"),
        other => panic!("expected MissingBundle, got {other:?}"),
    }

    let missing_patch = dir.write(
        "patches.toml",
        r#"[profile]
name = "t"
patches = ["ghost-patch.toml"]
"#,
    );
    match load_err(&missing_patch) {
        LoadError::MissingBundle { name } => assert_eq!(name, "ghost-patch.toml"),
        other => panic!("expected MissingBundle, got {other:?}"),
    }
}

#[test]
fn unknown_requires_lists_all_missing_ids() {
    let dir = TempDir::new("unknown-requires");
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "gateway"
kind = "builtin"
name = "gateway"
requires = ["tools", "nope1", "nope2", "nope1"]

[[plugin]]
id = "tools"
kind = "builtin"
name = "tools"
"#,
    );
    match load_err(&profile) {
        LoadError::UnknownRequires { id, missing } => {
            assert_eq!(id, "gateway");
            assert_eq!(missing, vec!["nope1", "nope2"]);
        }
        other => panic!("expected UnknownRequires, got {other:?}"),
    }
}

#[test]
fn dependency_cycles_report_a_readable_chain() {
    let dir = TempDir::new("cycle");
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "a"
kind = "builtin"
name = "a"
requires = ["b"]

[[plugin]]
id = "b"
kind = "builtin"
name = "b"
requires = ["c"]

[[plugin]]
id = "c"
kind = "builtin"
name = "c"
requires = ["a"]
"#,
    );
    match load_err(&profile) {
        LoadError::Cycle { chain } => assert_eq!(chain, "a -> b -> c -> a"),
        other => panic!("expected Cycle, got {other:?}"),
    }

    let self_cycle = dir.write(
        "self.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "loop"
kind = "builtin"
name = "loop"
requires = ["loop"]
"#,
    );
    match load_err(&self_cycle) {
        LoadError::Cycle { chain } => assert_eq!(chain, "loop -> loop"),
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[test]
fn topological_sort_is_stable() {
    let dir = TempDir::new("topo");
    let dag = dir.write(
        "dag.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "websearch"
kind = "builtin"
name = "websearch"
requires = ["tools"]

[[plugin]]
id = "render"
kind = "builtin"
name = "render"
requires = ["websearch", "tools"]

[[plugin]]
id = "tools"
kind = "builtin"
name = "tools"

[[plugin]]
id = "solo"
kind = "builtin"
name = "solo"
"#,
    );
    let tree = load_ok(&dag);
    assert_eq!(ids(&tree), vec!["tools", "websearch", "render", "solo"]);

    // Without dependency relations, insertion order wins even when a chain
    // is declared after an unrelated row.
    let stable = dir.write(
        "stable.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "a"
kind = "builtin"
name = "a"

[[plugin]]
id = "b"
kind = "builtin"
name = "b"

[[plugin]]
id = "c"
kind = "builtin"
name = "c"
requires = ["a"]
"#,
    );
    assert_eq!(ids(&load_ok(&stable)), vec!["a", "b", "c"]);
}

#[test]
fn dump_marks_layers_and_lists_ids() {
    let (_dir, profile) = three_layer_fixture("dump");
    let tree = load_ok(&profile);
    let text = dump(&tree);

    for comment in [
        "# --- tools (layer 0: base.toml) ---",
        "# --- websearch (layer 2: local.toml) ---",
        "# --- qq (layer 1: qq.toml) ---",
        "# --- extra (layer 3: plugins.toml) ---",
    ] {
        assert!(
            text.contains(comment),
            "missing provenance comment `{comment}` in:\n{text}"
        );
    }
    for id in ["tools", "websearch", "qq", "extra"] {
        assert!(
            text.contains(&format!("id = \"{id}\"")),
            "missing id `{id}` in:\n{text}"
        );
    }
    assert_eq!(text.matches("[[plugin]]").count(), 4);

    let reparsed: toml::Value = toml::from_str(&text).expect("dump output must be valid TOML");
    let rows = reparsed
        .get("plugin")
        .and_then(toml::Value::as_array)
        .expect("[[plugin]] rows");
    assert_eq!(rows.len(), tree.entries.len());
    let inline = rows
        .iter()
        .find(|row| row.get("id").and_then(toml::Value::as_str) == Some("extra"))
        .expect("extra row");
    assert_eq!(
        inline
            .get("config")
            .and_then(|config| config.get("api_key"))
            .and_then(toml::Value::as_str),
        Some("inline")
    );
}

#[test]
fn duplicate_id_in_one_layer_last_row_wins_with_warning() {
    let dir = TempDir::new("duplicate");
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "t"

[[plugin]]
id = "dup"
kind = "builtin"
name = "first"

[[plugin]]
id = "dup"
kind = "builtin"
name = "second"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(tree.entries.len(), 1);
    assert_eq!(entry(&tree, "dup").name, "second");
    assert_eq!(tree.warnings.len(), 1, "warnings: {:?}", tree.warnings);
    assert!(tree.warnings[0].contains("duplicate"));
    assert!(tree.warnings[0].contains("dup"));
}

#[test]
fn patch_can_repair_a_bundle_row() {
    // Validation runs on the final composed tree, so a patch may fix a
    // bundle row that would not validate on its own.
    let dir = TempDir::new("repair");
    dir.write(
        "bundle.toml",
        r#"[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
"#,
    );
    dir.write(
        "patch.toml",
        r#"[[plugin]]
id = "websearch"
kind = "stdio"
name = "echo-plugin-websearch"
command = "/fixed"
"#,
    );
    let profile = dir.write(
        "plugins.toml",
        r#"[profile]
name = "t"
bundles = ["bundle"]
patches = ["patch"]
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(entry(&tree, "websearch").command.as_deref(), Some("/fixed"));
    assert_eq!(entry(&tree, "websearch").layer, 1);
}

#[test]
fn file_without_profile_section_still_loads_its_inline_rows() {
    let dir = TempDir::new("no-profile");
    let profile = dir.write(
        "plugins.toml",
        r#"[[plugin]]
id = "solo"
kind = "builtin"
name = "solo"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["solo"]);
    assert_eq!(tree.entries[0].layer, 0);
    assert_eq!(tree.entries[0].layer_source, "plugins.toml");
}
