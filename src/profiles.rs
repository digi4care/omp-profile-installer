//! OMP profile discovery and CLI target resolution.
//!
//! Discovery is pure filesystem inspection of the OMP home: the default
//! profile agent dir (`~/.omp/agent`) and every profile tree
//! (`~/.omp/profiles/<name>/agent`). It never invokes `omp config path`
//! and never creates profile directories; missing or symlinked locations
//! are skipped, not followed. Registry-managed agent dirs outside that
//! tree are appended from the install registry (loaded via the explicit
//! state anchor), so `profiles list` also surfaces installs into custom
//! `--agent-dir` locations. Output is deterministically sorted.
//!
//! Target resolution maps CLI flags to an agent directory: an explicit
//! `--profile NAME` is resolved by `omp --profile NAME config path` (argv
//! array, no shell), an explicit `--agent-dir` is expanded and canonical
//! like every other path in this installer, and the ambient default is
//! `omp config path` with the active profile inferred from `OMP_PROFILE`,
//! then `PI_PROFILE`, defaulting to `default`. Real `omp` creates a
//! missing profile directory when answering `config path`; read-only
//! commands (`status`, `install --dry-run`) resolve through
//! [`resolve_target_read_only`] instead, which never leaves a created
//! directory behind.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::anchored::RootFd;
use crate::error::CliError;
use crate::fsx;
use crate::registry::{self, SourceSpec};

/// Profile name used when no `OMP_PROFILE`/`PI_PROFILE` override is set.
pub const DEFAULT_PROFILE: &str = "default";

/// One registry-managed install attached to a discovered agent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedInstall {
    /// Stable registry install ID for the install.
    pub install_id: String,
    /// Installed topology name.
    pub topology: String,
    /// Where the topology source came from.
    pub source: SourceSpec,
}

/// One agent location known to this machine.
///
/// `name` is `Some` for OMP profile trees (`default` for `~/.omp/agent`,
/// otherwise the `~/.omp/profiles/<name>` directory name) and for
/// registry-only entries whose install recorded a profile; custom
/// registry-managed dirs without recorded profile list with `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProfile {
    /// Profile name, when the location belongs to a named profile.
    pub name: Option<String>,
    /// Resolved absolute agent directory.
    pub agent_dir: PathBuf,
    /// `true` when the directory currently exists; registry-only entries
    /// whose custom agent dir vanished list with `false`.
    pub exists: bool,
    /// Registry installs targeting this directory, sorted by install ID.
    pub managed: Vec<ManagedInstall>,
}

impl DiscoveredProfile {
    fn new(name: Option<String>, agent_dir: PathBuf, exists: bool) -> Self {
        DiscoveredProfile {
            name,
            agent_dir,
            exists,
            managed: Vec::new(),
        }
    }
}

/// The OMP home (`~/.omp`), `None` when `$HOME` is unset.
fn omp_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".omp"))
}

/// Classify `path` before traversal: `Some(())` when it exists as a real
/// (non-symlink) directory, `None` when it is missing, symlinked, or a
/// non-directory — symlinked roots are never traversed. I/O failures
/// other than `NotFound` surface as actionable errors naming the path.
fn probe_real_dir(path: &Path) -> Result<Option<()>, CliError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(Some(())),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(crate::error::io_err(path, err)),
    }
}

/// Attach registry metadata to a discovered profile or append a
/// registry-only entry for a custom agent dir outside the profile tree.
///
/// Discovered profiles are indexed by agent directory, so each of the R
/// registry entries attaches in O(1) instead of a linear scan: overall
/// O(P + R log R) plus the final deterministic sort. An I/O failure
/// while probing a registry-only agent dir surfaces as an error rather
/// than being reported as a vanished directory.
fn merge_registry(
    mut discovered: Vec<DiscoveredProfile>,
    state_anchor: &Path,
) -> Result<Vec<DiscoveredProfile>, CliError> {
    let registry = registry::load_registry(state_anchor)?;
    let mut by_agent_dir: HashMap<PathBuf, usize> = HashMap::new();
    for (index, profile) in discovered.iter().enumerate() {
        by_agent_dir.insert(profile.agent_dir.clone(), index);
    }
    for (install_id, entry) in &registry.aliases {
        let agent_dir = fsx::resolve_loose(&entry.agent_dir);
        let install = ManagedInstall {
            install_id: install_id.clone(),
            topology: entry.topology.clone(),
            source: entry.source.clone(),
        };
        match by_agent_dir.get(&agent_dir) {
            Some(&index) => discovered[index].managed.push(install),
            None => {
                let mut profile =
                    DiscoveredProfile::new(entry.profile.clone(), agent_dir.clone(), false);
                profile.exists = probe_real_dir(&profile.agent_dir)?.is_some();
                profile.managed.push(install);
                by_agent_dir.insert(agent_dir, discovered.len());
                discovered.push(profile);
            }
        }
    }
    for profile in &mut discovered {
        profile
            .managed
            .sort_by(|a, b| a.install_id.cmp(&b.install_id));
    }
    discovered.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.agent_dir.cmp(&b.agent_dir))
    });
    Ok(discovered)
}

/// Discover OMP profiles by filesystem inspection plus the install
/// registry; never invokes `omp` and never creates directories.
///
/// The default profile (`~/.omp/agent`) and every
/// `~/.omp/profiles/<name>/agent` tree that exists as a real directory
/// are listed — a missing, symlinked, or non-directory `~/.omp`,
/// `profiles` root, or `agent` dir is skipped, never followed or
/// traversed. Registry-managed agent dirs not found in that tree are
/// appended with their recorded profile name (if any) and managed
/// install-ID/topology/source metadata. Entries sort by profile name (unnamed
/// entries first), then by agent directory. I/O failures other than
/// `NotFound` are actionable errors naming the offending path.
///
/// Complexity: O(P log P + R log R) for P discovered profiles and R
/// registry entries — the profile scan is O(P) directory probes, the
/// registry merge uses an indexed agent-dir lookup (no quadratic
/// rescan), and the output sort dominates.
pub fn discover_profiles(state_anchor: &Path) -> Result<Vec<DiscoveredProfile>, CliError> {
    let mut discovered: Vec<DiscoveredProfile> = Vec::new();
    // A symlinked ~/.omp is never traversed: neither the default-agent
    // probe below nor the profiles enumeration may follow it.
    if let Some(omp_home) = omp_home() {
        if probe_real_dir(&omp_home)?.is_none() {
            return merge_registry(discovered, state_anchor);
        }
        let default_agent = omp_home.join("agent");
        if probe_real_dir(&default_agent)?.is_some() {
            discovered.push(DiscoveredProfile::new(
                Some(DEFAULT_PROFILE.to_string()),
                fsx::resolve_loose(&default_agent),
                true,
            ));
        }
        let profiles_root = omp_home.join("profiles");
        if probe_real_dir(&profiles_root)?.is_none() {
            return merge_registry(discovered, state_anchor);
        }
        let entries = std::fs::read_dir(&profiles_root)
            .map_err(|err| crate::error::io_err(&profiles_root, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| crate::error::io_err(&profiles_root, err))?;
            // DirEntry::file_type does not follow links: symlinked profile
            // trees are skipped entirely. An I/O failure here is a real
            // error (not absence), so it names the offending path.
            let file_type = entry
                .file_type()
                .map_err(|err| crate::error::io_err(&entry.path(), err))?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let agent_dir = entry.path().join("agent");
            if probe_real_dir(&agent_dir)?.is_none() {
                continue;
            }
            discovered.push(DiscoveredProfile::new(
                Some(name),
                fsx::resolve_loose(&agent_dir),
                true,
            ));
        }
    }
    merge_registry(discovered, state_anchor)
}

/// Validate an explicit `--profile` name before it is passed to `omp` or
/// used as a directory label: rejects empty names, option injection
/// (leading `-`), and path traversal (`/`, `\`, NUL, `.`/`..` components).
pub fn validate_profile_name(name: &str) -> Result<(), CliError> {
    if name.is_empty() {
        return Err(CliError::Usage(
            "--profile name must not be empty".to_string(),
        ));
    }
    if name.starts_with('-') {
        return Err(CliError::Usage(format!(
            "invalid --profile name '{name}': must not start with '-'"
        )));
    }
    if name == "." || name == ".." {
        return Err(CliError::Usage(format!(
            "invalid --profile name '{name}': reserved path component"
        )));
    }
    if name.contains(['/', '\\', '\0']) {
        return Err(CliError::Usage(format!(
            "invalid --profile name '{name}': must not contain path separators"
        )));
    }
    Ok(())
}

/// Run `omp [--profile NAME] config path` and take the first stdout line,
/// expanded and resolved exactly like every other installer path.
/// No shell is involved: the invocation is a plain argv array.
fn omp_config_path(profile: Option<&str>) -> Result<PathBuf, CliError> {
    let mut command = std::process::Command::new("omp");
    if let Some(name) = profile {
        command.args(["--profile", name]);
    }
    let invocation = match profile {
        Some(name) => format!("omp --profile {name} config path"),
        None => "omp config path".to_string(),
    };
    let output = command.args(["config", "path"]).output().map_err(|err| {
        CliError::Usage(format!(
            "could not determine the OMP agent directory: '{invocation}' \
             could not be executed ({err}); pass --agent-dir PATH explicitly"
        ))
    })?;
    if !output.status.success() {
        return Err(CliError::Usage(format!(
            "could not determine the OMP agent directory: '{invocation}' \
             failed; pass --agent-dir PATH explicitly"
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    match line {
        Some(path) => Ok(fsx::resolve_loose(&fsx::expanduser(Path::new(path)))),
        None => Err(CliError::Usage(format!(
            "could not determine the OMP agent directory: '{invocation}' \
             printed no path; pass --agent-dir PATH explicitly"
        ))),
    }
}

/// Run `omp --profile NAME --alias ALIAS` so the real omp creates the
/// shell alias inside its own profile, after a topology install has been
/// registered. No shell is involved: the invocation is a plain argv
/// array, matching [`omp_config_path`]. stdout/stderr are captured and
/// never leak into installer output (in particular not into JSON mode);
/// a spawn failure or non-zero exit is a `CliError` naming the exact
/// command for manual recovery. The caller decides whether a failed
/// alias creation rolls anything back — this helper never does.
pub fn create_profile_alias(profile: &str, alias: &str) -> Result<(), CliError> {
    let invocation = format!("omp --profile {profile} --alias {alias}");
    let output = std::process::Command::new("omp")
        .args(["--profile", profile, "--alias", alias])
        .output()
        .map_err(|err| {
            CliError::Usage(format!(
                "could not create the shell alias via '{invocation}' ({err})"
            ))
        })?;
    if !output.status.success() {
        return Err(CliError::Usage(format!(
            "could not create the shell alias via '{invocation}' \
             (exited {})",
            output.status
        )));
    }
    Ok(())
}

/// The conventional agent directory of a profile under `$HOME/.omp`:
/// `~/.omp/agent` for the ambient default and `~/.omp/profiles/<name>/agent`
/// for `--profile NAME` (the same layout [`discover_profiles`] assumes).
/// `None` when `$HOME` is unset or empty — resolution falls back to
/// `omp config path` exactly as it always did.
fn conventional_profile_agent(profile: Option<&str>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() {
        return None;
    }
    let omp = home.join(".omp");
    Some(match profile {
        Some(name) => omp.join("profiles").join(name).join("agent"),
        None => omp.join("agent"),
    })
}

/// The missing ancestors of `path`, deepest first — exactly the
/// directories a `mkdir -p path` would create. Existence is probed without
/// following symlinks, so a symlinked ancestor counts as present and stops
/// the walk.
fn missing_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    while current.symlink_metadata().is_err() {
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    missing
}

/// Remove each path in `missing` (deepest first) while it is still an
/// empty directory; the first removal failure (non-empty, already gone,
/// or not a directory) stops the walk, so only directories created on
/// this run are removed. Every removal is descriptor-relative: `home`
/// re-walks the recorded path under `$HOME` `O_NOFOLLOW`, so a parent
/// swapped for a symlink between `omp config path` and the cleanup fails
/// the removal instead of an `rmdir` outside the tree. The `.omp` root
/// itself goes through the home pin (`rmdir` never follows its final
/// component); entries below it go through `omp` — the canonical `.omp`
/// root pinned before the gap when it already existed (the documented
/// single follow of the ambient `~/.omp` link, same as `Roots::open`),
/// or, when `omp` created `.omp`, re-pinned strictly afterwards, so a
/// swapped-in link fails closed. Unpinnable entries simply are not
/// removed.
fn remove_created_dirs(
    missing: &[PathBuf],
    textual_home: &Path,
    home: &RootFd,
    omp: Option<&RootFd>,
) {
    let mut omp = omp.cloned();
    for dir in missing {
        let removed = match dir.strip_prefix(textual_home) {
            Ok(rel) if rel == Path::new(".omp") => home.remove_empty_dir(".omp").is_ok(),
            Ok(rel) => match rel.strip_prefix(".omp") {
                Ok(below) => {
                    if omp.is_none() {
                        // `.omp` did not exist before omp ran: pin it
                        // strictly now — a link here is never followed.
                        omp = home.open_subdir(Path::new(".omp")).ok();
                    }
                    match omp.as_ref() {
                        Some(omp) => omp.remove_empty_dir(&below.to_string_lossy()).is_ok(),
                        None => false,
                    }
                }
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !removed {
            return;
        }
    }
}

/// Non-mutating counterpart of [`omp_config_path`] for read-only commands
/// (`status`, `install --dry-run`): real `omp --profile NAME config path`
/// creates a missing profile directory as a side effect, which a read-only
/// command must never leave behind. When the conventional profile tree
/// already exists it is used directly and `omp` is not invoked at all.
/// Otherwise `omp` is still consulted for an authoritative answer, and
/// when that answer is the conventional tree, the directories its answer
/// created — the missing ancestors recorded before the call — are removed
/// again, but only while empty, so pre-existing or concurrently populated
/// locations are never touched.
fn omp_config_path_no_create(profile: Option<&str>) -> Result<PathBuf, CliError> {
    let conventional = conventional_profile_agent(profile);
    if let Some(path) = &conventional
        && probe_real_dir(path)?.is_some()
    {
        return Ok(fsx::resolve_loose(path));
    }
    // Pin $HOME — and the `.omp` root beneath it when it already exists —
    // before inspecting or invoking `omp`. Cleanup re-walks from these
    // descriptors with O_NOFOLLOW, so a parent swapped while `omp` runs
    // fails closed instead of escaping the tree.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home_pin = home.as_deref().and_then(|home| RootFd::open(home).ok());
    let omp_pin = home_pin
        .as_ref()
        .and_then(|pin| pin.open_subdir_follow(".omp").ok());
    let missing = conventional
        .as_deref()
        .map(missing_ancestors)
        .unwrap_or_default();
    let resolved = omp_config_path(profile)?;
    if !missing.is_empty()
        && resolved == fsx::resolve_loose(&missing[0])
        && let Some(home) = home.as_deref()
        && let Some(pin) = home_pin.as_ref()
    {
        remove_created_dirs(&missing, home, pin, omp_pin.as_ref());
    }
    Ok(resolved)
}

/// The active profile inferred from the environment: `OMP_PROFILE`, then
/// `PI_PROFILE`, defaulting to [`DEFAULT_PROFILE`]. Empty values are
/// ignored; the result is metadata only and never used as a path.
pub fn active_profile_name() -> String {
    for var in ["OMP_PROFILE", "PI_PROFILE"] {
        if let Some(value) = std::env::var_os(var) {
            let value = value.to_string_lossy().trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    DEFAULT_PROFILE.to_string()
}

/// How the caller selected the install target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOrigin {
    /// Explicit `--profile NAME`, resolved via `omp --profile NAME config path`.
    ExplicitProfile,
    /// Explicit `--agent-dir PATH`; the profile is unknown.
    AgentDirOverride,
    /// Ambient default from `omp config path`; `profile` holds the
    /// environment-inferred name ([`active_profile_name`]).
    Ambient,
}

/// A resolved CLI target: the agent directory plus the profile metadata
/// suitable for registry recording and `profiles list` display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Agent directory to operate on.
    pub agent_dir: PathBuf,
    /// Profile name when known: the explicit `--profile`, or the
    /// environment-inferred name for the ambient target (`None` for an
    /// explicit `--agent-dir`).
    pub profile: Option<String>,
    /// How the target was selected.
    pub origin: TargetOrigin,
}

/// Mutually exclusive CLI target selectors.
pub struct TargetOptions<'a> {
    /// Explicit `--profile NAME`.
    pub profile: Option<&'a str>,
    /// Explicit `--agent-dir PATH`.
    pub agent_dir: Option<&'a str>,
}

/// Resolve the CLI target: explicit profile, explicit agent dir, or the
/// ambient default. `--profile` and `--agent-dir` together are a usage
/// error, and a profile name is validated before it reaches `omp`
/// (see [`validate_profile_name`]).
///
/// This is the mutating resolution real installs and removals use: `omp
/// --profile NAME config path` may create a missing profile directory.
/// Read-only commands (`status`, `install --dry-run`) must use
/// [`resolve_target_read_only`] instead.
pub fn resolve_target(opts: &TargetOptions<'_>) -> Result<ResolvedTarget, CliError> {
    resolve_target_inner(opts, true)
}

/// Resolve the CLI target exactly like [`resolve_target`], but without
/// letting profile resolution create directories: real `omp --profile NAME
/// config path` creates a missing profile directory as a side effect,
/// which a read-only command must never leave behind (see
/// [`omp_config_path_no_create`]).
pub fn resolve_target_read_only(opts: &TargetOptions<'_>) -> Result<ResolvedTarget, CliError> {
    resolve_target_inner(opts, false)
}

fn resolve_target_inner(
    opts: &TargetOptions<'_>,
    allow_create: bool,
) -> Result<ResolvedTarget, CliError> {
    if opts.profile.is_some() && opts.agent_dir.is_some() {
        return Err(CliError::Usage(
            "--profile and --agent-dir are mutually exclusive; pass only one".to_string(),
        ));
    }
    if let Some(name) = opts.profile {
        validate_profile_name(name)?;
        let agent_dir = if allow_create {
            omp_config_path(Some(name))?
        } else {
            omp_config_path_no_create(Some(name))?
        };
        return Ok(ResolvedTarget {
            agent_dir,
            profile: Some(name.to_string()),
            origin: TargetOrigin::ExplicitProfile,
        });
    }
    if let Some(raw) = opts.agent_dir {
        return Ok(ResolvedTarget {
            agent_dir: fsx::resolve_loose(&fsx::expanduser(Path::new(raw))),
            profile: None,
            origin: TargetOrigin::AgentDirOverride,
        });
    }
    let agent_dir = if allow_create {
        omp_config_path(None)?
    } else {
        omp_config_path_no_create(None)?
    };
    Ok(ResolvedTarget {
        agent_dir,
        profile: Some(active_profile_name()),
        origin: TargetOrigin::Ambient,
    })
}

#[cfg(test)]
mod tests;
