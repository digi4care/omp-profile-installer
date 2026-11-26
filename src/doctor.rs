//! Local, read-only diagnostics for the current OMP topology installation state.
//!
//! Doctor deliberately avoids source freshness checks: those can reach the
//! network. It reads only OMP's version, local profile discovery, the registry,
//! and ownership manifests. It never evaluates shell startup files or repairs
//! state.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value as Json, json};

use crate::error::CliError;
use crate::profiles::{self, TargetOptions};
use crate::registry::{self, InstallState, RegistryEntry};
use crate::{state, topology};

pub(crate) fn run(
    install_id: Option<&str>,
    profile: Option<&str>,
    agent_dir: Option<&str>,
    json_output: bool,
    state_anchor: &Path,
) -> Result<i32, CliError> {
    let mut report = json!({
        "command": "doctor",
        "status": "ok",
        "omp": {"status": "unknown"},
        "installs": [],
        "profiles": [],
        "unregistered": [],
        "findings": [],
    });

    inspect_omp(&mut report);

    if let Some(install_id) = install_id {
        match registry::load_registry(state_anchor) {
            Ok(registry) => match registry.aliases.get(install_id) {
                Some(entry) => inspect_entry(&mut report, install_id, entry, state_anchor),
                None => finding(
                    &mut report,
                    "error",
                    "unknown_install_id",
                    format!("install ID '{install_id}' is not registered"),
                ),
            },
            Err(error) => finding(
                &mut report,
                "error",
                "registry_error",
                format!("registry could not be read: {error}"),
            ),
        }
    } else if profile.is_some() || agent_dir.is_some() {
        let target = profiles::resolve_target_read_only(&TargetOptions { profile, agent_dir });
        match target {
            Err(error) => finding(
                &mut report,
                "error",
                "target_error",
                format!("target could not be resolved: {error}"),
            ),
            Ok(target) => match registry::load_registry(state_anchor) {
                Err(error) => finding(
                    &mut report,
                    "error",
                    "registry_error",
                    format!("registry could not be read: {error}"),
                ),
                Ok(registry) => {
                    let mut matched = false;
                    for (id, entry) in &registry.aliases {
                        if entry.agent_dir == target.agent_dir {
                            matched = true;
                            inspect_entry(&mut report, id, entry, state_anchor);
                        }
                    }
                    if !matched {
                        inspect_unregistered(
                            &mut report,
                            &target.agent_dir,
                            target.profile.as_deref(),
                            state_anchor,
                        );
                    }
                }
            },
        }
    } else {
        match registry::load_registry(state_anchor) {
            Err(error) => finding(
                &mut report,
                "error",
                "registry_error",
                format!("registry could not be read: {error}"),
            ),
            Ok(registry) => {
                for (id, entry) in &registry.aliases {
                    inspect_entry(&mut report, id, entry, state_anchor);
                }
                match profiles::discover_profiles(state_anchor) {
                    Ok(profiles) => {
                        let rows = profiles
                            .iter()
                            .map(|item| {
                                json!({
                                    "profile": item.name,
                                    "agent_dir": item.agent_dir.display().to_string(),
                                    "exists": item.exists,
                                    "install_ids": item.managed.iter().map(|install| install.install_id.clone()).collect::<Vec<_>>(),
                                })
                            })
                            .collect::<Vec<_>>();
                        report["profiles"] = json!(rows);
                        for item in profiles {
                            if item.managed.is_empty() && item.exists {
                                inspect_unregistered(
                                    &mut report,
                                    &item.agent_dir,
                                    item.name.as_deref(),
                                    state_anchor,
                                );
                            }
                        }
                    }
                    Err(error) => finding(
                        &mut report,
                        "error",
                        "profile_discovery_error",
                        format!("profiles could not be inspected: {error}"),
                    ),
                }
            }
        }
    }

    let errors = report["findings"]
        .as_array()
        .expect("doctor findings is always an array")
        .iter()
        .any(|item| item["severity"] == "error");
    let warnings = report["findings"]
        .as_array()
        .expect("doctor findings is always an array")
        .iter()
        .any(|item| item["severity"] == "warning");
    report["status"] = json!(if errors {
        "error"
    } else if warnings {
        "warning"
    } else {
        "ok"
    });

    if json_output {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| {
                CliError::Usage(format!("could not encode doctor report: {error}"))
            })?
        );
    } else {
        render_human(&report);
    }
    Ok(if errors { 1 } else { 0 })
}

fn inspect_omp(report: &mut Json) {
    let output = Command::new("omp")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            report["omp"] = json!({"status": "available", "version": version});
        }
        Ok(output) => {
            let message = format!("`omp --version` exited with {}", output.status);
            report["omp"] = json!({"status": "error", "message": message});
            finding(report, "error", "omp_unavailable", message);
        }
        Err(error) => {
            let message = format!("could not run `omp --version`: {error}");
            report["omp"] = json!({"status": "error", "message": message});
            finding(report, "error", "omp_unavailable", message);
        }
    }
}

fn inspect_entry(report: &mut Json, install_id: &str, entry: &RegistryEntry, state_anchor: &Path) {
    let mut issues = Vec::new();
    if entry.state != InstallState::Installed {
        issues.push((
            "partial_install",
            "install is recorded as partial".to_string(),
        ));
    }
    if !entry.agent_dir.is_dir() {
        issues.push((
            "missing_agent_dir",
            format!("agent directory '{}' is missing", entry.agent_dir.display()),
        ));
    }
    let manifest_path = match state::manifest_path_for_slot(
        &entry.agent_dir,
        state_anchor,
        install_id,
        entry.manifest_slot,
    ) {
        Ok(path) => path,
        Err(error) => {
            issues.push(("manifest_error", error.to_string()));
            append_install(report, install_id, entry, "error", None);
            for (code, message) in issues {
                finding(report, "error", code, message);
            }
            return;
        }
    };
    let manifest_topology = match state::load_manifest_at(&manifest_path, state_anchor) {
        Ok(Some(manifest)) => {
            let found = topology::manifest_topology(&manifest).map(str::to_string);
            if found.as_deref() != Some(entry.topology.as_str()) {
                issues.push((
                    "manifest_mismatch",
                    format!(
                        "ownership manifest for '{install_id}' declares topology '{}', expected '{}'",
                        found.as_deref().unwrap_or("none"),
                        entry.topology
                    ),
                ));
            }
            found
        }
        Ok(None) => {
            issues.push((
                "manifest_missing",
                format!("ownership manifest for '{install_id}' is missing"),
            ));
            None
        }
        Err(error) => {
            issues.push((
                "manifest_error",
                format!("ownership manifest is invalid: {error}"),
            ));
            None
        }
    };
    let status = if issues.is_empty() { "ok" } else { "error" };
    append_install(
        report,
        install_id,
        entry,
        status,
        manifest_topology.as_deref(),
    );
    for (code, message) in issues {
        finding(report, "error", code, message);
    }
}

fn append_install(
    report: &mut Json,
    install_id: &str,
    entry: &RegistryEntry,
    status: &str,
    manifest_topology: Option<&str>,
) {
    report["installs"]
        .as_array_mut()
        .expect("doctor installs is always an array")
        .push(json!({
            "install_id": install_id,
            "status": status,
            "state": if entry.state == InstallState::Installed { "installed" } else { "partial" },
            "topology": entry.topology,
            "profile": entry.profile,
            "agent_dir": entry.agent_dir.display().to_string(),
            "manifest_topology": manifest_topology,
        }));
}

fn inspect_unregistered(
    report: &mut Json,
    agent_dir: &Path,
    profile: Option<&str>,
    state_anchor: &Path,
) {
    match state::load_manifest(agent_dir, state_anchor) {
        Ok(Some(manifest)) => {
            let topology_name = topology::manifest_topology(&manifest).map(str::to_string);
            let item = json!({
                "profile": profile,
                "agent_dir": agent_dir.display().to_string(),
                "topology": topology_name,
                "status": "warning",
            });
            report["unregistered"]
                .as_array_mut()
                .expect("doctor unregistered is always an array")
                .push(item);
            finding(
                report,
                "warning",
                "unregistered_data",
                format!(
                    "unregistered ownership data{} exists for '{}' and was left untouched",
                    topology_name
                        .as_deref()
                        .map(|name| format!(" for topology '{name}'"))
                        .unwrap_or_default(),
                    agent_dir.display()
                ),
            );
        }
        Ok(None) => {}
        Err(error) => finding(
            report,
            "error",
            "manifest_error",
            format!(
                "ownership data at '{}' is invalid: {error}",
                agent_dir.display()
            ),
        ),
    }
}

fn finding(report: &mut Json, severity: &str, code: &str, message: String) {
    report["findings"]
        .as_array_mut()
        .expect("doctor findings is always an array")
        .push(json!({"severity": severity, "code": code, "message": message}));
}

fn render_human(report: &Json) {
    println!("doctor: {}", report["status"].as_str().unwrap_or("unknown"));
    println!(
        "OMP: {}",
        report["omp"]["status"].as_str().unwrap_or("unknown")
    );
    if let Some(version) = report["omp"]["version"].as_str() {
        println!("  {version}");
    }
    if let Some(installs) = report["installs"].as_array() {
        for install in installs {
            println!(
                "install {}: {} ({})",
                install["install_id"].as_str().unwrap_or("unknown"),
                install["topology"].as_str().unwrap_or("unknown"),
                install["status"].as_str().unwrap_or("unknown"),
            );
        }
    }
    if let Some(findings) = report["findings"].as_array() {
        for item in findings {
            println!(
                "{}: {}",
                item["severity"].as_str().unwrap_or("info"),
                item["message"].as_str().unwrap_or("unknown diagnostic"),
            );
        }
    }
}
