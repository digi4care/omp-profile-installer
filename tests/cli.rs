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
        let invocation_log = self.0.join("omp-invocations.log");
        fs::write(
            &fake_omp,
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$OMP_TEST_OMP_LOG\"\n\
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
            .env("OMP_TEST_CREATE_DIR", if create_dir { "1" } else { "0" })
            .env("OMP_TEST_OMP_LOG", invocation_log);
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
fn cli_rejects_legacy_update_alias_before_state_or_target_access() {
    let temp = TestDir::new();
    let default_agent_dir = temp.0.join("default-agent");
    let blocked_state_home = temp.0.join("blocked-state-home");
    let state_sentinel = b"state access must not happen\n";
    fs::write(&blocked_state_home, state_sentinel).expect("failed to block state home");

    let mut command = temp.cli_command(Path::new(env!("CARGO_MANIFEST_DIR")));
    command.args(["update", "--alias", "omp-dps"]);
    command.env("XDG_STATE_HOME", &blocked_state_home);
    temp.configure_fake_omp(&mut command, &default_agent_dir, true);

    let output = command.output().expect("failed to execute omp-topology");
    assert_eq!(
        output.status.code(),
        Some(2),
        "legacy update option did not return clap exit 2: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Usage:")
            && stderr.contains("omp-topology update")
            && stderr.contains("unexpected argument")
            && stderr.contains("--alias"),
        "removed update --alias was not rejected as an unknown clap argument:\n{stderr}"
    );
    assert_eq!(
        fs::read(&blocked_state_home).expect("blocked state sentinel disappeared"),
        state_sentinel,
        "invalid update accessed or mutated the blocked state path"
    );
    assert!(
        !default_agent_dir.exists(),
        "invalid update resolved or created the target before clap rejected the option"
    );
}

#[test]
fn cli_guidance_install_help_documents_shell_alias_profile_requirement() {
    let output = run_cli(&["install", "--help"]);
    assert!(output.status.success(), "install --help failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("help output was not UTF-8");
    let lower = stdout.to_ascii_lowercase();
    assert!(
        lower.contains("requires --profile")
            && lower.contains("shell alias is independent of the registry install id")
            && lower.contains("never influences install selection"),
        "install help did not document the shell-alias requirements and independent install ID:\n{stdout}"
    );
    assert!(
        stdout.contains("--profile") && stdout.contains("--agent-dir"),
        "install help omitted --profile or --agent-dir target guidance:\n{stdout}"
    );
}

#[test]
fn version_prints_exact_package_version() {
    let output = run_cli(&["--version"]);
    assert!(output.status.success(), "--version failed: {output:?}");

    let stdout = String::from_utf8(output.stdout).expect("--version output was not UTF-8");
    assert!(
        stdout.trim_end() == concat!("omp-topology ", env!("CARGO_PKG_VERSION")),
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
fn install_dry_run_reports_shell_alias_without_creating_it() {
    let temp = TestDir::new();
    let profile_agent = temp.0.join("home/.omp/profiles/demo/agent");
    let home = temp.0.join("home");
    let xdg_config = temp.0.join("xdg-config");
    fs::create_dir_all(&home).expect("failed to create isolated HOME");
    fs::create_dir_all(&xdg_config).expect("failed to create isolated XDG_CONFIG_HOME");
    fs::write(home.join(".bashrc"), b"# existing shell configuration\n")
        .expect("failed to seed shell configuration");
    let home_before = cli_file_snapshot(&home);
    let xdg_config_before = cli_file_snapshot(&xdg_config);
    let mut command = temp.cli_command(Path::new(env!("CARGO_MANIFEST_DIR")));
    command.args([
        "install",
        "--topology",
        "digital-product-studio",
        "--profile",
        "demo",
        "--alias",
        "demo-alias",
        "--dry-run",
    ]);
    temp.configure_fake_omp(&mut command, &profile_agent, false);

    let output = command.output().expect("failed to execute omp-topology");
    assert!(
        output.status.success(),
        "install --dry-run failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output was not UTF-8");
    let invocation_log = fs::read_to_string(temp.0.join("omp-invocations.log"))
        .expect("fake OMP invocation log is missing");
    assert_eq!(
        invocation_log, "--profile demo config path\n",
        "dry-run invoked OMP shell-alias creation"
    );
    assert_eq!(
        cli_file_snapshot(&home),
        home_before,
        "install --dry-run mutated HOME shell configuration"
    );
    assert_eq!(
        cli_file_snapshot(&xdg_config),
        xdg_config_before,
        "install --dry-run mutated XDG shell configuration"
    );
    assert!(
        stdout.contains(&profile_agent.display().to_string()),
        "dry-run did not resolve the requested profile target: {stdout}"
    );
    assert!(
        !profile_agent.exists(),
        "install --dry-run created the missing profile directory"
    );
    assert!(
        !temp.0.join("state").exists(),
        "install --dry-run created installer state"
    );
    assert!(
        stdout.contains("Would create OMP shell alias 'demo-alias' for profile 'demo'"),
        "dry-run omitted the planned OMP shell alias: {stdout}"
    );
}

fn cli_file_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(directory).expect("failed to read snapshot directory") {
            let entry = entry.expect("failed to read snapshot entry");
            let path = entry.path();
            if entry
                .file_type()
                .expect("failed to inspect snapshot entry")
                .is_dir()
            {
                visit(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root)
                        .expect("snapshot path escaped its root")
                        .to_path_buf(),
                    fs::read(path).expect("failed to read snapshot file"),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
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
