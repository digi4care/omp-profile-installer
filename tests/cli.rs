use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_omp-topology"))
        .args(args)
        .output()
        .expect("failed to execute omp-topology")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("omp-topology-cli-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create isolated test directory");
        Self(path)
    }

    fn cli_command(&self, cwd: &Path) -> Command {
        let home = self.0.join("home");
        let xdg_config = self.0.join("xdg-config");
        fs::create_dir_all(&home).expect("failed to create isolated HOME");
        fs::create_dir_all(&xdg_config).expect("failed to create isolated XDG_CONFIG_HOME");

        let mut command = Command::new(env!("CARGO_BIN_EXE_omp-topology"));
        command
            .current_dir(cwd)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", xdg_config)
            .env("XDG_STATE_HOME", self.0.join("state"));
        command
    }

    fn configure_fake_omp(&self, command: &mut Command, agent_dir: &Path, create_dir: bool) {
        let bin_dir = self.0.join("bin");
        fs::create_dir_all(&bin_dir).expect("failed to create fake PATH");
        let fake_omp = bin_dir.join("omp");
        fs::write(
            &fake_omp,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--profile\" ]; then shift 2; fi\n\
             if [ \"$1\" != \"config\" ] || [ \"$2\" != \"path\" ]; then exit 64; fi\n\
             if [ \"$OMP_TEST_CREATE_DIR\" = 1 ]; then mkdir -p \"$OMP_TEST_AGENT_DIR\" || exit 1; fi\n\
             printf '%s\\n' \"$OMP_TEST_AGENT_DIR\"\n",
        )
        .expect("failed to write fake omp");
        fs::set_permissions(&fake_omp, fs::Permissions::from_mode(0o755))
            .expect("failed to make fake omp executable");

        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("failed to build isolated PATH");
        command
            .env("PATH", path)
            .env("OMP_TEST_AGENT_DIR", agent_dir)
            .env("OMP_TEST_CREATE_DIR", if create_dir { "1" } else { "0" });
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn help_shows_description_and_version_option() {
    let output = run_cli(&["--help"]);
    assert!(output.status.success(), "--help failed: {output:?}");

    let stdout = String::from_utf8(output.stdout).expect("--help output was not UTF-8");
    assert!(
        stdout.contains("Install and manage OMP topology profiles"),
        "--help omitted the command description: {stdout}"
    );
    assert!(
        stdout.contains("--version"),
        "--help omitted the --version option: {stdout}"
    );
}

#[test]
fn version_prints_exact_package_version() {
    let output = run_cli(&["--version"]);
    assert!(output.status.success(), "--version failed: {output:?}");

    let stdout = String::from_utf8(output.stdout).expect("--version output was not UTF-8");
    assert!(
        stdout == "omp-topology 0.2.2" || stdout == "omp-topology 0.2.2\n",
        "unexpected --version output: {stdout:?}"
    );
}

#[test]
fn unknown_argument_returns_clap_usage_error() {
    let output = run_cli(&["--unknown-argument"]);
    assert!(
        !output.status.success(),
        "unknown argument unexpectedly succeeded"
    );

    let stderr = String::from_utf8(output.stderr).expect("error output was not UTF-8");
    assert!(
        stderr.contains("error:") && stderr.contains("Usage:"),
        "expected a clap usage error, got: {stderr}"
    );
}

#[test]
fn install_dry_run_does_not_create_missing_profile_directory() {
    let temp = TestDir::new();
    let profile_agent = temp.0.join("home/.omp/profiles/regression/agent");
    let mut command = temp.cli_command(Path::new(env!("CARGO_MANIFEST_DIR")));
    command.args([
        "install",
        "--topology",
        "digital-product-studio",
        "--profile",
        "regression",
        "--dry-run",
    ]);
    temp.configure_fake_omp(&mut command, &profile_agent, true);

    let output = command.output().expect("failed to execute omp-topology");
    assert!(
        output.status.success(),
        "install --dry-run failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output was not UTF-8");
    assert!(
        stdout.contains(&profile_agent.display().to_string()),
        "dry-run did not resolve the requested profile target: {stdout}"
    );
    assert!(
        !profile_agent.exists(),
        "install --dry-run created the missing profile directory"
    );
}

#[test]
fn status_does_not_create_missing_profile_directory() {
    let temp = TestDir::new();
    let profile_agent = temp.0.join("home/.omp/profiles/regression/agent");
    let mut command = temp.cli_command(Path::new(env!("CARGO_MANIFEST_DIR")));
    command.args(["status", "--profile", "regression", "--json"]);
    temp.configure_fake_omp(&mut command, &profile_agent, true);

    let output = command.output().expect("failed to execute omp-topology");
    assert!(output.status.success(), "status failed: {output:?}");
    assert!(
        !profile_agent.exists(),
        "read-only status created the missing profile directory"
    );
}

#[test]
fn remove_dry_run_does_not_create_missing_profile_directory() {
    let temp = TestDir::new();
    let profile_agent = temp.0.join("home/.omp/profiles/regression/agent");
    let mut command = temp.cli_command(Path::new(env!("CARGO_MANIFEST_DIR")));
    command.args([
        "remove",
        "--topology",
        "digital-product-studio",
        "--profile",
        "regression",
        "--dry-run",
    ]);
    temp.configure_fake_omp(&mut command, &profile_agent, true);

    let output = command.output().expect("failed to execute omp-topology");
    assert!(
        output.status.success(),
        "remove --dry-run failed: {output:?}"
    );
    assert!(
        !profile_agent.exists(),
        "remove --dry-run created the missing profile directory"
    );
}
