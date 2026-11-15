//! Behavioural tests for the explicit-scope deployment entry scripts.
//!
//! Each script is executed for real, with an isolated environment, against a
//! fake `echo-deploy` that only records its argv. Nothing is installed,
//! started, downloaded or built: the only observable effect is a log file in
//! a temporary directory.

use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{tempdir, TempDir};

const SCRIPTS: [&str; 5] = ["bootstrap", "install", "update", "repair", "uninstall"];
const WRAPPERS: [&str; 4] = ["install", "update", "repair", "uninstall"];

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
    /// here (plus a minimal PATH so bash can find sha256sum/cat).
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "script failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "script unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
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

#[test]
fn scripts_are_bash_syntax_clean_and_executable() {
    for name in SCRIPTS {
        let path = script_path(name);
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

        let mode = fs::metadata(&path)
            .expect("script metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "{name}.sh must be executable");
    }
}

#[test]
fn every_script_supports_help_without_a_tool() {
    for name in SCRIPTS {
        let fixture = Fixture::new();
        let output = fixture.run(name, &["--help"], &[]);
        assert_success(&output);
        let help = stdout(&output);
        assert!(help.contains("Usage:"), "{name} --help must print usage");
        if name == "bootstrap" {
            assert!(
                help.contains("--tool") && help.contains("--sha256"),
                "bootstrap --help must document its explicit options"
            );
        } else {
            assert!(
                help.contains("ECHO_DEPLOY_TOOL"),
                "{name} --help must document ECHO_DEPLOY_TOOL"
            );
        }
        fixture.assert_not_invoked();
    }
}

#[test]
fn install_forwards_only_install_plan_and_claims_plan_only() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "install",
        &[
            "--spec",
            "/absolute/install spec.json",
            "--output",
            "/absolute/new output",
        ],
        &[("ECHO_DEPLOY_TOOL", tool)],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&[
            "install-plan",
            "--spec",
            "/absolute/install spec.json",
            "--output",
            "/absolute/new output",
        ])
    );
    assert!(
        output.stdout.is_empty(),
        "the wrapper must not pollute the tool's stdout"
    );

    let message = stderr(&output).to_lowercase();
    assert!(message.contains("plan only"), "{message}");
    assert!(message.contains("pending install"), "{message}");
    assert!(message.contains("no service is installed"), "{message}");
}

#[test]
fn update_forwards_apply_arguments_verbatim() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "update",
        &[
            "--execution",
            "/absolute/execution.json",
            "--release",
            "dev-1",
            "--transaction",
            "tx-1",
            "--confirm",
            "--allow-development",
        ],
        &[("ECHO_DEPLOY_TOOL", tool)],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&[
            "apply",
            "--execution",
            "/absolute/execution.json",
            "--release",
            "dev-1",
            "--transaction",
            "tx-1",
            "--confirm",
            "--allow-development",
        ])
    );
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("echo-deploy apply"));
}

#[test]
fn repair_forwards_recover_arguments_verbatim() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "repair",
        &["--execution", "/absolute/execution.json", "--confirm"],
        &[("ECHO_DEPLOY_TOOL", tool)],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&[
            "recover",
            "--execution",
            "/absolute/execution.json",
            "--confirm"
        ])
    );
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("echo-deploy recover"));
}

#[test]
fn uninstall_forwards_only_uninstall_plan_and_claims_plan_only() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "uninstall",
        &["--execution", "/absolute/execution.json"],
        &[("ECHO_DEPLOY_TOOL", tool)],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&["uninstall-plan", "--execution", "/absolute/execution.json"])
    );
    assert!(output.stdout.is_empty());

    let message = stderr(&output).to_lowercase();
    assert!(message.contains("plan only"), "{message}");
    assert!(message.contains("no service is stopped"), "{message}");
    assert!(message.contains("nothing is removed"), "{message}");
    assert!(message.contains("user data is not deleted"), "{message}");
}

#[test]
fn wrappers_require_an_explicit_absolute_tool_and_never_use_path() {
    for name in WRAPPERS {
        let fixture = Fixture::new();

        // A fake tool exists in PATH and in the working directory, but there is
        // no default lookup: without ECHO_DEPLOY_TOOL the script must refuse.
        let bin = fixture.root.join("bin");
        fs::create_dir(&bin).expect("create bin");
        let in_path = bin.join("echo-deploy");
        fs::copy(&fixture.tool, &in_path).expect("copy PATH tool");
        executable(&in_path);
        let in_cwd = fixture.root.join("echo-deploy");
        fs::copy(&fixture.tool, &in_cwd).expect("copy cwd tool");
        executable(&in_cwd);
        let path_value = format!("{}:/usr/bin:/bin", bin.display());
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("PATH", &path_value)],
        );
        assert_failure(&output);
        assert!(
            stderr(&output).contains("ECHO_DEPLOY_TOOL"),
            "{name} must explain the missing environment variable:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();

        // A relative value is not an explicit absolute path.
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", "bin/echo-deploy")],
        );
        assert_failure(&output);
        assert!(
            stderr(&output).contains("absolute path"),
            "{name} must reject a relative tool path:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();

        // Missing file, directory, symlink and non-executable file all fail
        // closed before the tool is ever executed.
        let missing = fixture.root.join("missing-tool");
        let missing = missing.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", missing)],
        );
        assert_failure(&output);
        assert!(
            stderr(&output).contains("not a regular file"),
            "{name} must reject a missing tool:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();

        let directory = fixture.root.join("tool-directory");
        fs::create_dir(&directory).expect("create tool directory");
        let directory = directory.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", directory)],
        );
        assert_failure(&output);
        assert!(
            stderr(&output).contains("not a regular file"),
            "{name} must reject a directory:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();

        let link = fixture.root.join("tool-link");
        symlink(&fixture.tool, &link).expect("create tool symlink");
        let link = link.to_str().expect("UTF-8 temp path");
        let output = fixture.run(
            name,
            &["--spec", "/absolute/spec.json"],
            &[("ECHO_DEPLOY_TOOL", link)],
        );
        assert_failure(&output);
        assert!(
            stderr(&output).contains("symlink"),
            "{name} must reject a symlink tool:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();

        let non_executable = fixture.root.join("tool-not-executable");
        fs::copy(&fixture.tool, &non_executable).expect("copy non-executable tool");
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
        assert_failure(&output);
        assert!(
            stderr(&output).contains("not executable"),
            "{name} must reject a non-executable tool:\n{}",
            stderr(&output)
        );
        fixture.assert_not_invoked();
    }
}

#[test]
fn bootstrap_verifies_hash_and_execs_with_forwarded_arguments() {
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
            "--",
            "verify",
            "--bundle",
            "/absolute/candidate",
        ],
        &[],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&["verify", "--bundle", "/absolute/candidate"])
    );
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("verified"));
}

#[test]
fn bootstrap_accepts_tool_arguments_without_a_separator() {
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
            "stage",
            "--bundle",
            "/absolute/candidate",
        ],
        &[],
    );
    assert_success(&output);
    assert_eq!(
        fixture.recorded(),
        strings(&["stage", "--bundle", "/absolute/candidate"])
    );
}

#[test]
fn bootstrap_rejects_a_wrong_digest_before_exec() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let wrong = "0".repeat(64);
    let output = fixture.run(
        "bootstrap",
        &["--tool", tool, "--sha256", &wrong, "--", "verify"],
        &[],
    );
    assert_failure(&output);
    assert!(
        stderr(&output).contains("SHA-256 mismatch"),
        "wrong digest must be named:\n{}",
        stderr(&output)
    );
    fixture.assert_not_invoked();
}

#[test]
fn bootstrap_rejects_relative_missing_symlink_and_non_executable_tools() {
    let fixture = Fixture::new();
    let digest = sha256_hex(&fixture.tool);

    let output = fixture.run(
        "bootstrap",
        &["--tool", "fake-echo-deploy", "--sha256", &digest],
        &[],
    );
    assert_failure(&output);
    assert!(stderr(&output).contains("absolute path"));
    fixture.assert_not_invoked();

    let missing = fixture.root.join("missing-tool");
    let missing = missing.to_str().expect("UTF-8 temp path");
    let output = fixture.run("bootstrap", &["--tool", missing, "--sha256", &digest], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("not a regular file"));
    fixture.assert_not_invoked();

    let link = fixture.root.join("tool-link");
    symlink(&fixture.tool, &link).expect("create tool symlink");
    let link = link.to_str().expect("UTF-8 temp path");
    let output = fixture.run("bootstrap", &["--tool", link, "--sha256", &digest], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("symlink"));
    fixture.assert_not_invoked();

    let directory = fixture.root.join("tool-directory");
    fs::create_dir(&directory).expect("create tool directory");
    let directory = directory.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "bootstrap",
        &["--tool", directory, "--sha256", &digest],
        &[],
    );
    assert_failure(&output);
    assert!(stderr(&output).contains("not a regular file"));
    fixture.assert_not_invoked();

    let non_executable = fixture.root.join("tool-not-executable");
    fs::copy(&fixture.tool, &non_executable).expect("copy non-executable tool");
    let mut permissions = fs::metadata(&non_executable)
        .expect("metadata")
        .permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&non_executable, permissions).expect("remove execute bit");
    let non_executable = non_executable.to_str().expect("UTF-8 temp path");
    let output = fixture.run(
        "bootstrap",
        &["--tool", non_executable, "--sha256", &digest],
        &[],
    );
    assert_failure(&output);
    assert!(stderr(&output).contains("not executable"));
    fixture.assert_not_invoked();
}

#[test]
fn bootstrap_requires_both_options_and_a_valid_digest() {
    let fixture = Fixture::new();
    let tool = fixture.tool.to_str().expect("UTF-8 temp path");
    let digest = sha256_hex(&fixture.tool);

    let output = fixture.run("bootstrap", &["--sha256", &digest], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("--tool is required"));
    fixture.assert_not_invoked();

    let output = fixture.run("bootstrap", &["--tool", tool], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("--sha256 is required"));
    fixture.assert_not_invoked();

    let output = fixture.run("bootstrap", &["--tool", tool, "--sha256", "abc"], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("64 lowercase"));
    fixture.assert_not_invoked();

    let uppercase = "A".repeat(64);
    let output = fixture.run("bootstrap", &["--tool", tool, "--sha256", &uppercase], &[]);
    assert_failure(&output);
    assert!(stderr(&output).contains("lowercase hexadecimal"));
    fixture.assert_not_invoked();

    let output = fixture.run(
        "bootstrap",
        &["--tool", tool, "--sha256", &digest, "--bogus"],
        &[],
    );
    assert_failure(&output);
    assert!(stderr(&output).contains("unknown option"));
    fixture.assert_not_invoked();
}

#[test]
fn scripts_have_no_network_build_elevation_version_probe_or_shell_evaluation_path() {
    let forbidden = [
        "curl",
        "wget",
        "cargo",
        "sudo",
        "doas",
        "eval",
        "sh -c",
        "bash -c",
        "systemctl",
        "apt-get",
        "pacman",
        // Probing the executable with --version would execute it before the
        // bootstrap digest check; wrappers must go straight to the fixed call.
        "--version",
    ];
    for name in SCRIPTS {
        let text = fs::read_to_string(script_path(name))
            .expect("read script")
            .to_lowercase();
        for token in forbidden {
            assert!(
                !text.contains(token),
                "{name}.sh must not contain {token:?}"
            );
        }
    }
}
