//! Behavioural tests for the explicit-scope shell entrypoints.
//!
//! Every script is executed for real. Most tests use a fake tool that only
//! records argv; the remaining tests execute the compiled `echo-deploy` binary
//! with `--help`, malformed input, or a throwaway install spec whose unit,
//! launcher, ledger, config and data paths all live in a temporary directory.
//!
//! Nothing here touches a real service manager, container runtime, network,
//! production config or user data. The only filesystem effects are confined to
//! `tempfile` directories.

use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{tempdir, TempDir};

const SCRIPTS: [&str; 6] = [
    "bootstrap",
    "install",
    "update",
    "repair",
    "uninstall",
    "compose-plan",
];
const WRAPPERS: [&str; 5] = ["install", "update", "repair", "uninstall", "compose-plan"];

/// A stand-in for the real tool. It must not do anything except write argv.
const FAKE_TOOL: &str = r#"#!/usr/bin/env bash
set -eu
{
    printf '%s\n' "$#"
    for arg in "$@"; do
        printf '%s\n' "$arg"
    done
} > "${FAKE_TOOL_LOG:?FAKE_TOOL_LOG is required}"
"#;

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts")
}

fn script_path(name: &str) -> PathBuf {
    scripts_dir().join(format!("{name}.sh"))
}

fn real_tool() -> &'static str {
    env!("CARGO_BIN_EXE_echo-deploy")
}

fn executable(path: &Path) {
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable mode");
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    tool: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().expect("create temp directory");
        let root = temp.path().to_path_buf();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let tool = root.join("fake-echo-deploy");
        fs::write(&tool, FAKE_TOOL).expect("write fake tool");
        executable(&tool);
        let log = root.join("argv.log");
        Self {
            _temp: temp,
            root,
            tool,
            log,
        }
    }

    /// Run a script with an empty environment except for the entries listed
    /// here (plus a minimal PATH so bash can find sha256sum).
    fn run(&self, script: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command = Command::new("/bin/bash");
        command
            .arg(script_path(script))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("TMPDIR", &self.root)
            .env("LC_ALL", "C")
            .env("FAKE_TOOL_LOG", &self.log)
            .current_dir(&self.root);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("execute script")
    }

    fn recorded(&self) -> Vec<String> {
        let raw = fs::read_to_string(&self.log)
            .unwrap_or_else(|error| panic!("fake tool did not record argv: {error}"));
        let mut lines = raw.lines();
        let count: usize = lines
            .next()
            .expect("missing argv count")
            .parse()
            .expect("argv count is not a number");
        let args: Vec<String> = lines.map(str::to_owned).collect();
        assert_eq!(
            args.len(),
            count,
            "recorded argv does not match declared count"
        );
        args
    }

    fn assert_not_invoked(&self) {
        assert!(!self.log.exists(), "fake tool must not have been executed");
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("process terminated by a signal")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "script failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        exit_code(output),
        expected,
        "unexpected exit code\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn sha256_hex(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read tool for digest"))
    )
}

fn real_env() -> [(&'static str, &'static str); 1] {
    [("ECHO_DEPLOY_TOOL", real_tool())]
}

/// Files below `scripts/` that must be shell entrypoints. Any future `.sh`
/// added there is covered by the syntax and no-execution scans automatically.
fn shell_scripts() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(scripts_dir())
        .expect("read scripts directory")
        .map(|entry| entry.expect("read scripts entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sh"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn entrypoints_are_bash_syntax_clean_executable_and_set_eu() {
    let paths = shell_scripts();
    assert_eq!(paths.len(), SCRIPTS.len(), "unexpected entrypoint set");
    for name in SCRIPTS {
        let path = script_path(name);
        assert!(paths.contains(&path), "missing {name}.sh");

        let output = Command::new("/bin/bash")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("run bash -n");
        assert!(
            output.status.success(),
            "bash -n failed for {}:\n{}",
            path.display(),
            stderr(&output)
        );

        let text = fs::read_to_string(&path).expect("read script");
        assert!(
            text.starts_with("#!/usr/bin/env bash\n"),
            "{name}.sh must use the bash shebang"
        );
        assert!(
            text.lines().any(|line| line == "set -eu"),
            "{name}.sh must set -eu"
        );
        assert!(!text.contains("\r\n"), "{name}.sh must use LF line endings");

        let mode = fs::metadata(&path)
            .expect("script metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{name}.sh must be executable");
    }
}

#[test]
fn every_entrypoint_prints_help_without_a_tool() {
    let expectations: [(&str, &[&str]); 6] = [
        ("bootstrap", &["--tool", "--sha256", "--wrapper"]),
        ("install", &["install-plan", "install-files", "Plan mode"]),
        (
            "uninstall",
            &["uninstall-plan", "uninstall-files", "user data"],
        ),
        ("update", &["apply", "--confirm"]),
        ("repair", &["recover", "--confirm"]),
        (
            "compose-plan",
            &["compose-plan", "not a deployment", "a container"],
        ),
    ];
    for (name, needles) in expectations {
        let fixture = Fixture::new();
        let output = fixture.run(name, &["--help"], &[]);
        assert_success(&output);
        assert_exit(&output, 0);
        let help = stdout(&output);
        assert!(help.contains("Usage:"), "{name} --help must print usage");
        for needle in needles {
            assert!(
                help.contains(needle),
                "{name} --help must mention {needle:?}:\n{help}"
            );
        }
        assert!(
            stderr(&output).is_empty(),
            "{name} --help must not write to stderr"
        );
        fixture.assert_not_invoked();
    }
}

#[test]
fn file_modes_and_help_combinations_stay_in_help_and_plan_paths() {
    for name in ["install", "uninstall"] {
        let fixture = Fixture::new();
        let output = fixture.run(name, &["--files", "--help"], &[]);
        assert_success(&output);
        assert!(stdout(&output).contains("Usage:"));
        fixture.assert_not_invoked();
    }

    // A help flag anywhere before `--` must win over legacy/plan arguments.
    for name in WRAPPERS {
        let fixture = Fixture::new();
        let output = fixture.run(name, &["--dry-run", "--help"], &[]);
        assert_success(&output);
        assert!(stdout(&output).contains("Usage:"), "{name} help scan");
        fixture.assert_not_invoked();
    }
}

#[test]
fn fixed_subcommands_are_injected_and_forwarded_verbatim() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let envs = [("ECHO_DEPLOY_TOOL", tool)];

    let cases: [(&str, &[&str], &str, &[&str]); 6] = [
        (
            "install",
            &[
                "--spec",
                "/absolute/spec.json",
                "--output",
                "/absolute/plan",
            ],
            "install-plan",
            &[
                "--spec",
                "/absolute/spec.json",
                "--output",
                "/absolute/plan",
            ],
        ),
        (
            "install",
            &[
                "--files",
                "--spec",
                "/absolute/spec.json",
                "--unit-directory",
                "/absolute/units",
                "--launcher-directory",
                "/absolute/launchers",
                "--ledger",
                "/absolute/ledger.json",
                "--confirm",
            ],
            "install-files",
            &[
                "--spec",
                "/absolute/spec.json",
                "--unit-directory",
                "/absolute/units",
                "--launcher-directory",
                "/absolute/launchers",
                "--ledger",
                "/absolute/ledger.json",
                "--confirm",
            ],
        ),
        (
            "update",
            &[
                "--execution",
                "/absolute/execution.json",
                "--release",
                "dev-1",
                "--confirm",
            ],
            "apply",
            &[
                "--execution",
                "/absolute/execution.json",
                "--release",
                "dev-1",
                "--confirm",
            ],
        ),
        (
            "repair",
            &["--execution", "/absolute/execution.json", "--confirm"],
            "recover",
            &["--execution", "/absolute/execution.json", "--confirm"],
        ),
        (
            "uninstall",
            &["--spec", "/absolute/spec.json"],
            "uninstall-plan",
            &["--spec", "/absolute/spec.json"],
        ),
        (
            "uninstall",
            &[
                "--files",
                "--ledger",
                "/absolute/ledger.json",
                "--offline-only",
                "--confirm",
            ],
            "uninstall-files",
            &[
                "--ledger",
                "/absolute/ledger.json",
                "--offline-only",
                "--confirm",
            ],
        ),
    ];

    for (script, args, subcommand, forwarded) in cases {
        // Reset the argv log between cases.
        let _ = fs::remove_file(&fixture.log);
        let output = fixture.run(script, args, &envs);
        assert_success(&output);
        let mut expected = strings(&[subcommand]);
        expected.extend(strings(forwarded));
        assert_eq!(
            fixture.recorded(),
            expected,
            "{script} must inject exactly one fixed subcommand"
        );
        assert!(
            output.stdout.is_empty(),
            "{script} must not pollute the tool's stdout"
        );
    }

    // `--dry-run` is accepted only as a plan-mode alias and is not forwarded
    // (the plan modes are already non-mutating).
    for script in ["install", "uninstall"] {
        let _ = fs::remove_file(&fixture.log);
        let output = fixture.run(
            script,
            &["--dry-run", "--spec", "/absolute/spec.json"],
            &envs,
        );
        assert_success(&output);
        assert_eq!(
            fixture.recorded(),
            strings(&[
                if script == "install" {
                    "install-plan"
                } else {
                    "uninstall-plan"
                },
                "--spec",
                "/absolute/spec.json",
            ])
        );
    }

    // The compose entry point is the only way to reach compose-plan, and it
    // must not be usable through the install/update wrappers.
    let _ = fs::remove_file(&fixture.log);
    let output = fixture.run("compose-plan", &["--spec", "/absolute/compose.json"], &envs);
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&["compose-plan", "--spec", "/absolute/compose.json"])
    );
    for script in ["install", "update", "repair", "uninstall"] {
        let _ = fs::remove_file(&fixture.log);
        let output = fixture.run(script, &["compose-plan"], &envs);
        // The fake tool accepts anything, but the fixed subcommand remains
        // first; this proves a caller cannot replace it with compose-plan.
        assert_success(&output);
        assert_ne!(
            fixture.recorded().first().map(String::as_str),
            Some("compose-plan"),
            "{script} must not let the caller select compose-plan"
        );
    }
}

#[test]
fn plan_modes_claim_only_a_plan() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let envs = [("ECHO_DEPLOY_TOOL", tool)];

    let install = fixture.run("install", &["--spec", "/absolute/spec.json"], &envs);
    assert_success(&install);
    let message = stderr(&install).to_lowercase();
    for needle in ["plan only", "pending install", "no service is installed"] {
        assert!(
            message.contains(needle),
            "install claim: {needle}: {message}"
        );
    }
    assert!(install.stdout.is_empty());

    let uninstall = fixture.run("uninstall", &["--spec", "/absolute/spec.json"], &envs);
    assert_success(&uninstall);
    let message = stderr(&uninstall).to_lowercase();
    for needle in [
        "plan only",
        "no service is stopped",
        "nothing is removed",
        "user data is not deleted",
    ] {
        assert!(
            message.contains(needle),
            "uninstall claim: {needle}: {message}"
        );
    }

    let compose = fixture.run("compose-plan", &["--spec", "/absolute/compose.json"], &envs);
    assert_success(&compose);
    let message = stderr(&compose).to_lowercase();
    for needle in ["plan only", "not a deployment", "no container"] {
        assert!(
            message.contains(needle),
            "compose claim: {needle}: {message}"
        );
    }
}

#[test]
fn file_modes_are_explicit_and_do_not_claim_activation() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let envs = [("ECHO_DEPLOY_TOOL", tool)];

    let install = fixture.run(
        "install",
        &["--files", "--spec", "/absolute/spec.json"],
        &envs,
    );
    assert_success(&install);
    assert_eq!(
        fixture.recorded().first().map(String::as_str),
        Some("install-files")
    );
    let message = stderr(&install).to_lowercase();
    for needle in ["file-only", "existing files are refused", "config/data"] {
        assert!(
            message.contains(needle),
            "install files claim: {needle}: {message}"
        );
    }

    let uninstall = fixture.run(
        "uninstall",
        &[
            "--files",
            "--ledger",
            "/absolute/ledger.json",
            "--offline-only",
            "--confirm",
        ],
        &envs,
    );
    assert_success(&uninstall);
    assert_eq!(
        fixture.recorded().first().map(String::as_str),
        Some("uninstall-files")
    );
    let message = stderr(&uninstall).to_lowercase();
    for needle in [
        "file-only",
        "user data are not removed",
        "no service manager",
    ] {
        assert!(
            message.contains(needle),
            "uninstall files claim: {needle}: {message}"
        );
    }
}

#[test]
fn legacy_flags_are_rejected_with_exit_two_before_the_tool_runs() {
    let cases: [(&str, &[&str], &str); 13] = [
        ("install", &["--overwrite-config"], "overwrite-config"),
        ("install", &["--scope=user"], "scope"),
        ("install", &["--no-start"], "no-start"),
        ("install", &["--files", "--dry-run"], "dry-run"),
        ("uninstall", &["--purge"], "purge"),
        ("uninstall", &["--disable-linger"], "disable-linger"),
        ("uninstall", &["--scope", "user"], "scope"),
        ("uninstall", &["--files", "--dry-run"], "dry-run"),
        ("update", &["--remote"], "remote"),
        ("update", &["--source", "/absolute/source"], "source"),
        ("repair", &["--dry-run"], "dry-run"),
        ("repair", &["--overwrite-config"], "overwrite-config"),
        ("repair", &["--scope=user"], "scope"),
    ];

    for (script, args, needle) in cases {
        let fixture = Fixture::new();
        let tool = fixture.tool.to_str().expect("UTF-8 temp path");
        let output = fixture.run(script, args, &[("ECHO_DEPLOY_TOOL", tool)]);
        assert_exit(&output, 2);
        let message = stderr(&output).to_lowercase();
        assert!(
            message.contains(&needle.to_lowercase()),
            "{script} {args:?} must name {needle:?}: {message}"
        );
        fixture.assert_not_invoked();
    }

    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let output = fixture.run("compose-plan", &["--up"], &[("ECHO_DEPLOY_TOOL", tool)]);
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("container"));
    fixture.assert_not_invoked();
}

#[test]
fn tool_resolution_fails_closed_with_exit_two() {
    for name in WRAPPERS {
        let fixture = Fixture::new();

        let output = fixture.run(name, &["--spec", "/absolute/spec.json"], &[]);
        assert_exit(&output, 2);
        assert!(
            stderr(&output).contains("ECHO_DEPLOY_TOOL"),
            "{name} must explain the missing environment variable"
        );
        fixture.assert_not_invoked();

        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", "relative/echo-deploy")],
        );
        assert_exit(&output, 2);
        assert!(stderr(&output).contains("absolute path"));
        fixture.assert_not_invoked();

        let link = fixture.root.join("tool-link");
        symlink(&fixture.tool, &link).expect("create tool symlink");
        let link = link.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", link)],
        );
        assert_exit(&output, 2);
        assert!(stderr(&output).contains("symlink"));
        fixture.assert_not_invoked();

        let non_executable = fixture.root.join("tool-not-executable");
        fs::copy(&fixture.tool, &non_executable).expect("copy tool");
        let mut permissions = fs::metadata(&non_executable)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&non_executable, permissions).expect("remove execute bit");
        let non_executable = non_executable.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", non_executable)],
        );
        assert_exit(&output, 2);
        assert!(stderr(&output).contains("not executable"));
        fixture.assert_not_invoked();

        let directory = fixture.root.join("tool-directory");
        fs::create_dir(&directory).expect("create directory");
        let directory = directory.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", directory)],
        );
        assert_exit(&output, 2);
        assert!(stderr(&output).contains("not a regular file"));
        fixture.assert_not_invoked();
    }
}

#[test]
fn bootstrap_rejects_bad_digests_duplicates_and_bad_wrappers_with_exit_two() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let digest = sha256_hex(&fixture.tool);

    let wrong = "0".repeat(64);
    let output = fixture.run(
        "bootstrap",
        &["--tool", tool, "--sha256", &wrong, "--", "verify"],
        &[],
    );
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("SHA-256 mismatch"));
    fixture.assert_not_invoked();

    // An empty first value still counts as "specified"; a duplicate must be
    // rejected instead of silently taking the second value.
    let output = fixture.run(
        "bootstrap",
        &["--tool=", "--tool", tool, "--sha256", &digest],
        &[],
    );
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("more than once"));
    fixture.assert_not_invoked();

    let output = fixture.run(
        "bootstrap",
        &[
            "--tool",
            tool,
            "--sha256",
            &digest,
            "--wrapper",
            "install",
            "--wrapper",
            "update",
        ],
        &[],
    );
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("more than once"));
    fixture.assert_not_invoked();

    let output = fixture.run(
        "bootstrap",
        &[
            "--tool",
            tool,
            "--sha256",
            &digest,
            "--wrapper",
            "../install",
        ],
        &[],
    );
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("wrapper must be one of"));
    fixture.assert_not_invoked();

    let output = fixture.run(
        "bootstrap",
        &["--tool", tool, "--sha256", &digest, "--wrapper"],
        &[],
    );
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("--wrapper"));
    fixture.assert_not_invoked();
}

#[test]
fn bootstrap_wrapper_hands_the_verified_tool_to_a_sibling_entrypoint() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let digest = sha256_hex(&fixture.tool);

    let output = fixture.run(
        "bootstrap",
        &[
            "--tool",
            tool,
            "--sha256",
            &digest,
            "--wrapper",
            "install",
            "--",
            "--spec",
            "/absolute/spec.json",
            "--output",
            "/absolute/plan",
        ],
        &[],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&[
            "install-plan",
            "--spec",
            "/absolute/spec.json",
            "--output",
            "/absolute/plan",
        ])
    );
    let message = stderr(&output);
    assert!(message.contains("verified"), "{message}");
    assert!(message.to_lowercase().contains("plan only"), "{message}");

    fs::remove_file(&fixture.log).expect("reset argv log");
    let output = fixture.run(
        "bootstrap",
        &[
            "--tool",
            tool,
            "--sha256",
            &digest,
            "--wrapper",
            "compose-plan",
            "--",
            "--spec",
            "/absolute/compose.json",
        ],
        &[],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&["compose-plan", "--spec", "/absolute/compose.json"])
    );
    assert!(stderr(&output).to_lowercase().contains("not a deployment"));
}

#[test]
fn real_cli_help_and_rejection_exit_codes_are_propagated() {
    for subcommand in [
        "install-plan",
        "install-files",
        "uninstall-plan",
        "uninstall-files",
        "apply",
        "recover",
        "compose-plan",
    ] {
        let output = Command::new(real_tool())
            .arg(subcommand)
            .arg("--help")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .expect("run real CLI help");
        assert_success(&output);
        assert!(stdout(&output).contains("Usage:"));
    }

    let fixture = Fixture::new();
    let envs = real_env();

    // clap validation: missing required arguments is exit 2.
    let output = fixture.run("install", &[], &envs);
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("--spec"));

    // Runtime validation: a relative spec path is exit 1, not 2 and not 0.
    let output = fixture.run("install", &["--spec", "relative.json"], &envs);
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("absolute"));

    let output = fixture.run("uninstall", &["--spec", "relative.json"], &envs);
    assert_exit(&output, 1);

    let output = fixture.run("update", &[], &envs);
    assert_exit(&output, 2);
    assert!(stderr(&output).contains("--execution"));

    let output = fixture.run(
        "update",
        &[
            "--execution",
            "relative.json",
            "--release",
            "dev-1",
            "--confirm",
        ],
        &envs,
    );
    assert_exit(&output, 1);

    let output = fixture.run(
        "repair",
        &["--execution", "relative.json", "--confirm"],
        &envs,
    );
    assert_exit(&output, 1);

    let output = fixture.run("compose-plan", &["--spec", "relative.json"], &envs);
    assert_exit(&output, 1);
}

/// Build one throwaway install spec and exercise the real file-only paths.
/// This is the only test that writes through the install/uninstall entrypoints;
/// every path is inside the temp fixture, and the config/data sentinels prove
/// that neither operation touches them.
#[test]
fn real_file_install_and_uninstall_are_ledger_scoped_and_never_touch_config_or_data() {
    let fixture = Fixture::new();
    let root = fixture.root.clone();
    let units = root.join("units");
    let launchers = root.join("launchers");
    let state = root.join("state");
    let release = root.join("release");
    let config_dir = root.join("config");
    let data_dir = root.join("data");
    for directory in [&units, &launchers, &state, &release, &config_dir, &data_dir] {
        fs::create_dir(directory).expect("create fixture directory");
        let mut permissions = fs::metadata(directory)
            .expect("fixture directory metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(directory, permissions).expect("set fixture directory mode");
    }
    let core_config = config_dir.join("core.toml");
    let session_data = data_dir.join("session.db");
    fs::write(&core_config, "preserve-config\n").expect("write config sentinel");
    fs::write(&session_data, "preserve-data\n").expect("write data sentinel");

    let spec = root.join("install.json");
    fs::write(
        &spec,
        format!(
            concat!(
                "{{\n",
                "  \"instance_id\": \"local-dev\",\n",
                "  \"scope\": \"user\",\n",
                "  \"release_root\": \"{}\",\n",
                "  \"core_config\": \"{}\",\n",
                "  \"topology\": \"core_only\",\n",
                "  \"deploy_executable\": \"{}\",\n",
                "  \"state_dir\": \"{}\"\n",
                "}}\n"
            ),
            release.display(),
            core_config.display(),
            real_tool(),
            state.display()
        ),
    )
    .expect("write install spec");
    let spec = spec.to_str().expect("UTF-8 spec path");
    let units_arg = units.to_str().expect("UTF-8 units path");
    let launchers_arg = launchers.to_str().expect("UTF-8 launchers path");
    let ledger = root.join("ledger.json");
    let ledger_arg = ledger.to_str().expect("UTF-8 ledger path");
    let envs = real_env();

    // Collision preflight: a pre-existing managed file is never replaced.
    let core_unit = units.join("echo-agent-local-dev-core.service");
    fs::write(&core_unit, "sentinel-unit\n").expect("write collision sentinel");
    let output = fixture.run(
        "install",
        &[
            "--files",
            "--spec",
            spec,
            "--unit-directory",
            units_arg,
            "--launcher-directory",
            launchers_arg,
            "--ledger",
            ledger_arg,
            "--confirm",
        ],
        &envs,
    );
    assert_exit(&output, 1);
    assert_eq!(
        fs::read_to_string(&core_unit).expect("read collision sentinel"),
        "sentinel-unit\n"
    );
    assert!(!launchers.join("echo-agent-local-dev").exists());
    assert!(
        !ledger.exists(),
        "a failed preflight must not write a ledger"
    );

    fs::remove_file(&core_unit).expect("remove collision sentinel");

    let output = fixture.run(
        "install",
        &[
            "--files",
            "--spec",
            spec,
            "--unit-directory",
            units_arg,
            "--launcher-directory",
            launchers_arg,
            "--ledger",
            ledger_arg,
            "--confirm",
        ],
        &envs,
    );
    assert_success(&output);
    assert!(stdout(&output).contains("files_installed_not_activated"));
    assert!(core_unit.exists());
    assert!(units.join("echo-agent-local-dev-update.service").exists());
    assert!(launchers.join("echo-agent-local-dev").exists());
    assert!(ledger.exists());
    assert_eq!(
        fs::read_to_string(&core_config).expect("read config sentinel"),
        "preserve-config\n"
    );
    assert_eq!(
        fs::read_to_string(&session_data).expect("read data sentinel"),
        "preserve-data\n"
    );

    // A second install with a fresh ledger still hits the existing files and
    // must fail without overwriting them.
    let second_ledger = root.join("ledger-second.json");
    let second_ledger_arg = second_ledger.to_str().expect("UTF-8 second ledger path");
    let output = fixture.run(
        "install",
        &[
            "--files",
            "--spec",
            spec,
            "--unit-directory",
            units_arg,
            "--launcher-directory",
            launchers_arg,
            "--ledger",
            second_ledger_arg,
            "--confirm",
        ],
        &envs,
    );
    assert_exit(&output, 1);
    assert!(core_unit.exists());
    assert!(!second_ledger.exists());
    assert_eq!(
        fs::read_to_string(&core_config).expect("read config sentinel"),
        "preserve-config\n"
    );

    let output = fixture.run(
        "uninstall",
        &[
            "--files",
            "--ledger",
            ledger_arg,
            "--offline-only",
            "--confirm",
        ],
        &envs,
    );
    assert_success(&output);
    assert!(stdout(&output).contains("managed_files_removed_services_not_touched"));
    assert!(!core_unit.exists());
    assert!(!launchers.join("echo-agent-local-dev").exists());
    assert!(ledger.exists(), "the ledger is evidence and is not deleted");
    assert_eq!(
        fs::read_to_string(&core_config).expect("read config sentinel"),
        "preserve-config\n"
    );
    assert_eq!(
        fs::read_to_string(&session_data).expect("read data sentinel"),
        "preserve-data\n"
    );

    // The file-only entrypoint refuses a real uninstall without the explicit
    // offline attestation; the ledger must remain and the files stay gone.
    let output = fixture.run(
        "uninstall",
        &["--files", "--ledger", ledger_arg, "--confirm"],
        &envs,
    );
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("offline_only"));
    assert!(ledger.exists());
}

#[test]
fn compose_plan_renders_a_non_executable_plan_without_a_runtime_or_writes() {
    let fixture = Fixture::new();
    let root = fixture.root.clone();
    let config_dir = root.join("compose-config");
    let data_dir = root.join("compose-data");
    let hex = "a".repeat(64);
    let spec = root.join("compose.json");
    fs::write(
        &spec,
        format!(
            concat!(
                "{{\n",
                "  \"schema_version\": 1,\n",
                "  \"instance_id\": \"local-dev\",\n",
                "  \"core_image\": \"ghcr.io/echolab-auto/echo-agent-core@sha256:{}\",\n",
                "  \"config_dir\": \"{}\",\n",
                "  \"data_dir\": \"{}\",\n",
                "  \"topology\": \"core_only\"\n",
                "}}\n"
            ),
            hex,
            config_dir.display(),
            data_dir.display()
        ),
    )
    .expect("write compose spec");
    let spec = spec.to_str().expect("UTF-8 compose spec path");

    // A container runtime on PATH that can only record an invocation. If the
    // entrypoint (or the CLI) ever shells out, the marker appears.
    let bin = root.join("bin");
    fs::create_dir(&bin).expect("create fake runtime directory");
    let marker = root.join("container-invoked");
    for runtime in ["docker", "podman"] {
        let path = bin.join(runtime);
        fs::write(
            &path,
            format!(
                "#!/usr/bin/env bash\nprintf invoked > \"{}\"\n",
                marker.display()
            ),
        )
        .expect("write fake runtime");
        executable(&path);
    }
    let path_value = format!("{}:/usr/bin:/bin", bin.display());

    let output = fixture.run(
        "compose-plan",
        &["--spec", spec],
        &[("ECHO_DEPLOY_TOOL", real_tool()), ("PATH", &path_value)],
    );
    assert_success(&output);
    assert!(!marker.exists(), "compose-plan must not invoke a runtime");
    assert!(!config_dir.exists(), "compose-plan must not create config");
    assert!(!data_dir.exists(), "compose-plan must not create data");
    let plan = stdout(&output);
    for needle in [
        "\"plan_preview\": true",
        "\"activation_supported\": false",
        "\"execution_supported\": false",
        "\"pull_supported\": false",
        "\"up_supported\": false",
        "\"config_preserved\": true",
        "\"data_preserved\": true",
    ] {
        assert!(
            plan.contains(needle),
            "compose plan must say {needle}:\n{plan}"
        );
    }
}

#[test]
fn shell_entrypoints_have_no_service_container_network_or_shell_evaluation_path() {
    let forbidden = [
        "systemctl",
        "docker ",
        "docker\n",
        "podman",
        "curl",
        "wget",
        "cargo",
        "sudo",
        "doas",
        "eval",
        "sh -c",
        "bash -c",
        "--version",
    ];
    for path in shell_scripts() {
        let text = fs::read_to_string(&path)
            .expect("read script")
            .to_lowercase();
        for token in forbidden {
            assert!(
                !text.contains(token),
                "{} must not contain {token:?}",
                path.display()
            );
        }
    }
}
