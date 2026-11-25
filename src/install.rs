//! Install command: validate, plan, then apply in a single transaction with
//! timestamped backups and rollback on failure.
//!
//! The planning phase reads every target exactly once and refuses all
//! collisions before mutating anything; the apply phase re-verifies the
//! live files just before each replacement so a concurrent edit is never
//! clobbered. Updates also reconcile state the new topology no longer
//! ships: dropped source files and config leaves are released from
//! ownership, and unedited ones are deleted or restored to the user's
//! prior values. Rollback undoes only what the run actually changed,
//! re-verifying the live bytes first. Complexity: planning is O(S + F)
//! for S overlay bytes and F tracked files (one read and one hash per
//! candidate), the merge is O(topology config size), and rollback is
//! O(files written this run).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json, json};

use crate::anchored::RootFd;
use crate::config_mode::{ConfigMode, MergePrecedence};
use crate::error::{CliError, io_err};
use crate::fsx;
use crate::registry;
use crate::state;
use crate::symlinks::{self, ResolvedAt, Roots};
use crate::topology;
use crate::value::{self, Value};
use crate::yamlwork;

pub const CONFIG_NEW_NAME: &str = "config.yml.new";

/// Synthetic `NotFound` io error for a path probed present but unreadable
/// through its pinned descriptor by the time the read happens.
fn not_found_io() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::NotFound)
}

/// Read a topology-source config input lexically from its checkout.
/// Source inputs live outside the pinned agent/profiles target roots, so
/// `symlinks::read_pinned` does not apply to them: it is strict for
/// resolved install targets and returns `Ok(None)` for a path under a
/// configured root that is not pinned, which would silently treat the
/// topology config as empty and lose both its values and the type-conflict
/// detection. An absent file is `Ok(None)`, matching the pre-pin behavior.
fn read_source_config(path: &Path) -> Result<Option<Vec<u8>>, CliError> {
    if !fsx::lexists(path) {
        return Ok(None);
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|err| io_err(path, err))
}

/// Planned change to the candidate publication (`config.yml.new`) beside
/// an active config the candidate mode never touches.
enum CandidateAction {
    Publish(Vec<u8>),
    Delete,
}

pub struct InstallOptions<'a> {
    pub topology: &'a str,
    /// Explicit source-tree root to resolve the topology in; the CLI
    /// passes the resolved source root (the checkout root by default).
    pub topology_root: &'a Path,
    /// Stable XDG state anchor for manifest/state paths, independent of
    /// topology checkout availability.
    pub state_anchor: &'a Path,
    pub agent_dir: Option<&'a str>,
    pub skills_dir: Option<&'a str>,
    pub dry_run: bool,
    /// Config handling for this run only; config modes are never
    /// remembered between commands.
    pub config_mode: ConfigMode,
    /// Ownership manifest this install owns (a per-install-ID manifest
    /// when several install IDs share the agent directory). `None` uses
    /// the legacy default `manifest.json`.
    pub manifest_path: Option<&'a Path>,
    /// Install ID performing the install, when invoked through one. Scopes
    /// recovery markers so a sibling install's rollback snapshot is never
    /// cleared.
    pub install_id: Option<&'a str>,
    /// Exact topology metadata version assertion: when given, the
    /// resolved topology's declared `version` must equal this version
    /// exactly (never a range, never compared against a recorded
    /// version), checked before any target or registry mutation.
    pub expected_version: Option<&'a semver::Version>,
}

/// Resolve the target agent directory (explicit override or `omp config path`).
pub fn resolve_agent_dir(override_dir: Option<&str>) -> Result<PathBuf, CliError> {
    if let Some(raw) = override_dir {
        return Ok(fsx::resolve_loose(&fsx::expanduser(Path::new(raw))));
    }
    let proc = std::process::Command::new("omp")
        .args(["config", "path"])
        .output();
    let resolved: Option<String> = match proc {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
        }
        _ => None,
    };
    match resolved {
        Some(path) => Ok(fsx::resolve_loose(&fsx::expanduser(Path::new(&path)))),
        None => Err(CliError::Usage(
            "could not determine the active OMP agent directory: 'omp config \
             path' failed; pass --agent-dir PATH explicitly"
                .to_string(),
        )),
    }
}

/// Fail closed on malformed ownership manifests: the hash maps must map
/// every relative path to a string, and a recorded skills root must be a
/// string, otherwise collision protection and edit-preservation cannot be
/// trusted and the install refuses rather than risk clobbering edits.
fn validate_manifest_hashes(manifest: &Option<Json>) -> Result<(), CliError> {
    let Some(manifest) = manifest else {
        return Ok(());
    };
    for field in ["files", "skills_files"] {
        if let Some(map) = manifest.get(field) {
            let entries = map.as_object().ok_or_else(|| {
                CliError::Manifest(format!("ownership manifest '{field}' is malformed"))
            })?;
            for value in entries.values() {
                if value.as_str().is_none() {
                    return Err(CliError::Manifest(format!(
                        "ownership manifest '{field}' is malformed"
                    )));
                }
            }
        }
    }
    if let Some(skills_dir) = manifest.get("skills_dir")
        && skills_dir.as_str().is_none()
    {
        return Err(CliError::Manifest(
            "ownership manifest 'skills_dir' is malformed".to_string(),
        ));
    }
    Ok(())
}

/// Refuse incompatible scalar/map pairings between the topology config
/// model and the active config model. Shared by `preflight_config_types`
/// and `run` so the lock-free preflight and the locked revalidation
/// reject with the identical message.
fn refuse_config_type_conflicts(topo_model: &Value, user_model: &Value) -> Result<(), CliError> {
    let mut conflicts = Vec::new();
    value::type_conflicts(topo_model, user_model, &mut Vec::new(), &mut conflicts);
    if !conflicts.is_empty() {
        return Err(CliError::Usage(format!(
            "refusing to install: incompatible config types:\n  - {}",
            conflicts.join("\n  - ")
        )));
    }
    Ok(())
}

/// Replace mode swaps the whole active config for the verbatim topology
/// bytes, so every config key a sibling alias owns — disjoint from this
/// topology's config or not — is overwritten or removed. Fail closed;
/// merge mode is the way to share a config between aliases.
fn refuse_replace_over_sibling_ownership(
    cross_alias: &registry::CrossAliasOwnership,
    agent_dir: &Path,
) -> Result<(), CliError> {
    if let Some((leaf, owner)) = cross_alias.config_installed.iter().next() {
        return Err(CliError::Usage(format!(
            "refusing to install: config key '{}' is already managed by \
             install ID '{owner}' for {}; --config-replace replaces the whole \
             active config and would remove it (use --config-merge to share \
             the config, or remove that install first)",
            leaf.join("."),
            agent_dir.display()
        )));
    }
    Ok(())
}

/// Read-only preflight for a replace-mode install or update: refuse when
/// the topology ships a config whose verbatim replacement of the active
/// config would overwrite or remove config keys a sibling alias owns.
/// `update` stages a Partial registry entry before `run` repeats this
/// check under the lock, so it must refuse here for the update to leave
/// both the registry and the target untouched; `run` still re-checks
/// under the lock so ownership recorded in between fails closed. A
/// symlinked or absent topology config plans no replacement (`run`
/// refuses the symlink with its own wording), so there is nothing a
/// sibling could lose.
pub fn preflight_replace_ownership(
    topology_dir: &Path,
    state_anchor: &Path,
    agent_dir: &Path,
    manifest_path: &Path,
) -> Result<(), CliError> {
    let topo_config_path = topology_dir.join("agent").join(yamlwork::CONFIG_NAME);
    if fsx::is_symlink(&topo_config_path) || read_source_config(&topo_config_path)?.is_none() {
        return Ok(());
    }
    let cross_alias = registry::cross_alias_ownership(state_anchor, agent_dir, manifest_path)?;
    refuse_replace_over_sibling_ownership(&cross_alias, agent_dir)
}

/// Read-only preflight for `cmd_install`: refuse incompatible config
/// types before the caller acquires the registry lock, which creates the
/// state home — a visible side effect a refused install must not leave
/// behind. `run` repeats the check under the lock before mutating, so a
/// config edited between preflight and lock acquisition still fails
/// closed. Config shapes `run` refuses with their own messages (symlink
/// or non-file configs, symlinked topology configs) pass here so those
/// refusals keep their exact wording.
pub fn preflight_config_types(
    topology_root: &Path,
    topology: &str,
    agent_dir: Option<&str>,
) -> Result<(), CliError> {
    let topology_dir = topology::find_topology(topology_root, topology, true)?;
    let agent_dir = resolve_agent_dir(agent_dir)?;
    let roots = symlinks::Roots::open(&agent_dir)?;
    let topo_config_path = topology_dir.join("agent").join(yamlwork::CONFIG_NAME);
    if fsx::is_symlink(&topo_config_path) {
        return Ok(());
    }
    let topo_model = match read_source_config(&topo_config_path)? {
        Some(bytes) => {
            let doc = yamlwork::parse_config(&bytes, "topology config")?;
            yamlwork::migrate(&doc);
            yamlwork::model_of(&doc, "topology config")?
        }
        None => Value::Map(BTreeMap::new()),
    };
    let config_path = agent_dir.join(yamlwork::CONFIG_NAME);
    if !config_path.is_file() || fsx::is_symlink(&config_path) {
        // `run` refuses symlinked configs with its own exact wording; the
        // advisory type check here must not pre-empt that refusal.
        return Ok(());
    }
    let bytes = symlinks::read_pinned(&roots, &config_path)?
        .ok_or_else(|| io_err(&config_path, not_found_io()))?;
    let what = format!("config '{}'", config_path.display());
    let doc = yamlwork::parse_config(&bytes, &what)?;
    let user_model = yamlwork::migrated_model(&doc, &what)?;
    refuse_config_type_conflicts(&topo_model, &user_model)
}

/// Record an owner alias named by a collision diagnostic, deduplicated
/// in first-seen order so the recovery guidance built from it is
/// deterministic. Only collisions recorded in a sibling alias's
/// ownership manifest have an owner; symlink, link-drift, and unowned
/// collisions never call this.
fn note_collision_owner(owners: &mut Vec<String>, owner: &str) {
    if !owners.iter().any(|existing| existing == owner) {
        owners.push(owner.to_string());
    }
}

/// Refuse unsupported symlink topology and file-name collisions on the
/// install targets before the caller takes the registry lock: lock
/// acquisition creates the state home, and a refused install must leave
/// no side effects behind. Every refusal raised here is one `run` raises
/// as well (its per-file link resolution is a collision, the config link
/// resolution an immediate error, a symlinked skills root an immediate
/// error, and an unowned existing target a collision) before mutating
/// anything, so preflighting cannot reject an install `run` would have
/// accepted; `run` repeats the resolution under the lock, so a target
/// changed in between still fails closed.
pub fn preflight_target_links(
    topology_root: &Path,
    topology: &str,
    agent_dir: Option<&str>,
    skills_dir: Option<&str>,
    state_anchor: &Path,
    manifest_path: Option<&Path>,
) -> Result<(), CliError> {
    let topology_dir = topology::find_topology(topology_root, topology, true)?;
    let agent_dir = resolve_agent_dir(agent_dir)?;
    // Pin the agent directory and the OMP profiles/agent trees once for
    // the whole preflight: every read below is checked against these
    // descriptors, so a swapped-in symlink between check and use is
    // observed instead of followed.
    let roots = Roots::open(&agent_dir)?;
    let manifest_path = match manifest_path {
        Some(path) => path.to_path_buf(),
        None => state::manifest_path(&agent_dir, state_anchor)?,
    };
    let existing_manifest = state::load_manifest_at(&manifest_path, state_anchor)?;
    // Paths a sibling alias owns are collisions here as well as in `run`;
    // no shared file or skill ownership is allowed, even when bytes match.
    let cross_alias = registry::cross_alias_ownership(state_anchor, &agent_dir, &manifest_path)?;
    let recorded_skills_root: Option<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_dir"))
        .and_then(Json::as_str)
        .map(str::to_string);
    let meta = topology::topology_metadata(&topology_dir)?;
    let meta_name = topology::meta_name(&meta);

    // Mirror the skills-root validation from `run` (a symlinked --skills-dir
    // must refuse before the lock-created state home appears).
    let mut requested_skills_root: Option<PathBuf> = None;
    if let Some(raw) = skills_dir {
        // Refuse symlinks on the path as given, before resolving: resolving
        // first would follow the link and erase the evidence that the user
        // pointed us at a symlink.
        let requested_raw = fsx::expanduser(Path::new(raw));
        if fsx::has_symlink_component(&requested_raw) {
            return Err(CliError::Usage(format!(
                "refusing to install: skills root '{}' (or a \
                 parent directory) is a symlink",
                requested_raw.display()
            )));
        }
        requested_skills_root = Some(fsx::resolve_loose(&requested_raw));
    }
    let skills_root: Option<PathBuf> = match (&requested_skills_root, &recorded_skills_root) {
        (Some(resolved), _) => Some(resolved.clone()),
        (None, Some(recorded)) => Some(PathBuf::from(recorded)),
        (None, None) => None,
    };
    if let Some(root) = &skills_root
        && fsx::has_symlink_component(root)
    {
        return Err(CliError::Usage(format!(
            "refusing to install: skills root '{}' (or a parent \
             directory) is a symlink",
            root.display()
        )));
    }

    if let Some(root) = &skills_root {
        // Pin with the no-symlink-root policy: opening walks every
        // component `O_NOFOLLOW`, so a symlink at (or swapped in at) the
        // skills root fails closed here — before any planning or
        // mutation — instead of redirecting every later operation.
        let _ = open_skills_pin(root)?;
    }

    let owned_agent_files: HashSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("files"))
        .and_then(Json::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    let owned_skill_files: HashSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_files"))
        .and_then(Json::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    let existing_symlinks: BTreeMap<String, symlinks::LinkIdentity> = existing_manifest
        .as_ref()
        .map(symlinks::links_from_manifest)
        .unwrap_or_default();

    // Mirror the read-only half of `run`'s collision detection: every
    // unowned-file and symlink refusal `run` raises before mutating must
    // be raised here as well, ahead of the registry lock, so a refusal
    // leaves no state home behind. `run` repeats the detection under the
    // lock, so a target changed in between still fails closed.
    let sources = topology::source_agent_files(&topology_dir)?;
    let mut agent_sources: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut skill_sources: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, source_bytes) in sources {
        if skills_root.is_some() && rel.starts_with("skills/") {
            // With an explicit skills root, skills/* is routed there and
            // the agent directory's own skills/ path is never inspected.
            skill_sources.insert(rel["skills/".len()..].to_string(), source_bytes);
        } else {
            agent_sources.insert(rel, source_bytes);
        }
    }
    let agent_source_hashes: BTreeMap<String, String> = agent_sources
        .iter()
        .map(|(rel, bytes)| (rel.clone(), fsx::sha256_hex(bytes)))
        .collect();
    let mut collisions: Vec<String> = Vec::new();
    let mut collision_owners: Vec<String> = Vec::new();
    // Supported main->topology links seen at their resolved target, for the
    // same byte-identical adoption exception `run` grants.
    let mut linked_targets: BTreeSet<String> = BTreeSet::new();
    for rel in agent_sources.keys() {
        let declared = symlinks::resolve_declared_at(&roots, rel, &meta_name);
        let present = match &declared {
            Ok(resolved_at) => resolved_at.exists().unwrap_or(false),
            Err(_) => false,
        };
        let target = match declared {
            Ok(ResolvedAt::Direct { resolved, .. }) => {
                if existing_symlinks.contains_key(rel.as_str()) {
                    collisions.push(format!(
                        "{rel} (or a parent directory) changed since install: the recorded profile link is gone; refusing to touch it"
                    ));
                    continue;
                }
                resolved
            }
            Ok(ResolvedAt::Linked {
                resolved, identity, ..
            }) => {
                match existing_symlinks.get(rel.as_str()) {
                    Some(recorded) if *recorded == identity => {}
                    Some(recorded) => {
                        collisions.push(format!(
                            "{rel}: profile link points to '{}' instead of the recorded target '{}'; refusing to follow a retargeted link",
                            identity.target.display(),
                            recorded.target.display(),
                        ));
                        continue;
                    }
                    None => {}
                }
                linked_targets.insert(rel.clone());
                resolved
            }
            Err(err) => {
                collisions.push(format!("{rel}: {err}"));
                continue;
            }
        };
        if !present {
            continue;
        }
        // Read through the pinned descriptor that resolved `target`: the
        // read races no `open()`-time symlink swap, unlike `std::fs::read`.
        let current = match symlinks::read_pinned(&roots, &target)? {
            Some(bytes) => bytes,
            // Deleted between the probe and the read: no collision.
            None => continue,
        };
        if owned_agent_files.contains(rel) {
            continue;
        }
        if let Some(owner) = cross_alias.files.get(rel.as_str()) {
            note_collision_owner(&mut collision_owners, owner);
            collisions.push(format!(
                "{rel} already exists and is managed by install ID '{owner}'; \
                 refusing to overwrite it"
            ));
            continue;
        }
        // Any pre-existing file without our manifest is a collision, even
        // if byte-identical — the same rule `run` applies. The single
        // exception is a file reached through a supported main->topology
        // link whose live bytes exactly match the topology source.
        let adopted = linked_targets.contains(rel.as_str())
            && agent_source_hashes.get(rel.as_str()) == Some(&fsx::sha256_hex(&current));
        if !adopted {
            collisions.push(format!(
                "{rel} already exists and is not owned by this topology \
                 (unowned file-name collision)"
            ));
        }
    }
    if let Some(root) = &skills_root {
        let skills_pin = open_skills_pin(root)?;
        let root_string = root.display().to_string();
        for rel in skill_sources.keys() {
            let target = root.join(rel);
            let symlinked = match &skills_pin {
                Some(pin) => pin
                    .has_symlink_component(rel)
                    .map_err(|err| io_err(&target, err))?,
                None => false,
            };
            if symlinked {
                collisions.push(format!(
                    "{rel} under skills root '{}' (or a parent directory) \
                     is a symlink; refusing to touch it",
                    root.display()
                ));
                continue;
            }
            let Some(pin) = &skills_pin else {
                continue;
            };
            let bytes = match pin.read_file(rel) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => None,
                Err(err) => return Err(io_err(&target, err)),
            };
            let Some(bytes) = bytes else {
                continue;
            };
            // Read for the same io-error parity `run` keeps before the
            // ownership decision.
            let _current = bytes;
            if let Some(owner) = cross_alias
                .skills_files
                .get(&root_string)
                .and_then(|files| files.get(rel))
            {
                note_collision_owner(&mut collision_owners, owner);
                collisions.push(format!(
                    "{rel} already exists under skills root '{}' and is \
                     managed by install ID '{owner}'; refusing to overwrite it",
                    root.display()
                ));
                continue;
            }
            if !owned_skill_files.contains(rel) {
                collisions.push(format!(
                    "{rel} already exists under skills root '{}' and is \
                     not owned by this topology (unowned file-name collision)",
                    root.display()
                ));
            }
        }
    }
    if !collisions.is_empty() {
        let message = format!(
            "refusing to install: file-name collision:\n  - {}",
            collisions.join("\n  - ")
        );
        return Err(if collision_owners.is_empty() {
            // No colliding file has a recorded owner (only symlink or
            // unowned-file refusals): keep the plain usage diagnostic.
            CliError::Usage(message)
        } else {
            CliError::InstallCollision {
                message,
                owners: collision_owners,
            }
        });
    }
    Ok(())
}

/// Strict SemVer gate for a resolved topology's declared `version`,
/// shared by `cmd_install`'s preflight (run after source resolution but
/// before the registry lock, so a refused first-time install leaves the
/// previously absent state root absent) and [`run`] (repeated under the
/// lock, so a source edited in between still fails closed before any
/// mutation). `declared` is the metadata `version` string read from
/// `metadata_path`; an unparsable declaration fails closed instead of
/// being recorded or compared ad hoc, and a requested expectation must
/// match the declared version exactly (an assertion, never a range).
pub fn validate_declared_version(
    topology: &str,
    metadata_path: &Path,
    declared: &str,
    expected_version: Option<&semver::Version>,
) -> Result<semver::Version, CliError> {
    let declared_version = semver::Version::parse(declared).map_err(|err| {
        CliError::Topology(format!(
            "topology '{topology}' declares version '{declared}' in {}, which \
             is not valid SemVer ({err}); fix the topology's topology.yml \
             before installing",
            metadata_path.display(),
        ))
    })?;
    if let Some(expected) = expected_version
        && *expected != declared_version
    {
        return Err(CliError::Usage(format!(
            "refusing to install: requested version {expected} but topology \
             '{topology}' declares version {declared_version} in {}; pass the \
             declared version or drop --version to accept it",
            metadata_path.display(),
        )));
    }
    Ok(declared_version)
}

pub fn run(opts: &InstallOptions, out: &mut Vec<String>) -> Result<i32, CliError> {
    let topology_root = opts.topology_root;
    let state_anchor = opts.state_anchor;
    let topology_dir = topology::find_topology(topology_root, opts.topology, true)?;
    let agent_dir = resolve_agent_dir(opts.agent_dir)?;
    // The ownership manifest this install owns: a per-install-ID
    // manifest when several install IDs share the agent directory, else
    // the legacy default.
    let manifest_path = match opts.manifest_path {
        Some(path) => path.to_path_buf(),
        None => state::manifest_path(&agent_dir, state_anchor)?,
    };
    // Pin the agent directory and the OMP profiles/agent trees once for
    // the whole run: declared-link resolution, collision reads, and the
    // per-target re-verifications all go through these descriptors.
    let roots = Roots::open(&agent_dir)?;

    let meta = topology::topology_metadata(&topology_dir)?;
    let meta_name = topology::meta_name(&meta);
    let meta_version = topology::meta_version(&meta);
    // SemVer gate, before any preflight/target/registry mutation so every
    // refusal below leaves the target, the registry, and the state home
    // untouched; `cmd_install` runs the same check via
    // [`validate_declared_version`] before acquiring the registry lock,
    // and it is repeated here under the lock to close source-mutation
    // races. The manifest still records the original declared string.
    let metadata_path = topology_dir.join("topology.yml");
    validate_declared_version(
        opts.topology,
        &metadata_path,
        &meta_version,
        opts.expected_version,
    )?;
    let topo_config_path = topology_dir.join("agent").join(yamlwork::CONFIG_NAME);
    if fsx::is_symlink(&topo_config_path) {
        return Err(CliError::Topology(format!(
            "topology config '{}' is a symlink; refusing",
            topo_config_path.display()
        )));
    }
    // The topology document is the graft source: run it through the YAML
    // 1.1 -> 1.2 migration so values like `on` keep their historical
    // boolean meaning while injected spellings are unambiguous. The raw
    // bytes are kept as well: candidate publication and full replacement
    // copy the source file verbatim rather than a re-serialization.
    let (topo_doc, topo_model, topo_config_raw) = match read_source_config(&topo_config_path)? {
        Some(bytes) => {
            let doc = yamlwork::parse_config(&bytes, "topology config")?;
            yamlwork::migrate(&doc);
            let model = yamlwork::model_of(&doc, "topology config")?;
            (Some(doc), model, Some(bytes))
        }
        None => (None, Value::Map(BTreeMap::new()), None),
    };

    let config_path = agent_dir.join(yamlwork::CONFIG_NAME);
    // The active config snapshot is read further down, once the recorded
    // manifest link identities are loaded: the config may live behind a
    // supported main->topology symlink, which is resolved through the
    // same trust and identity validation as topology-declared files.

    let existing_manifest = state::load_manifest_at(&manifest_path, state_anchor)?;
    validate_manifest_hashes(&existing_manifest)?;
    if let Some(manifest) = &existing_manifest
        && topology::manifest_topology(manifest) != Some(meta_name.as_str())
    {
        let installed = topology::manifest_topology(manifest).unwrap_or("unknown");
        return Err(CliError::Usage(format!(
            "refusing to install: this install ID's ownership manifest for {} \
             records topology '{installed}'; one topology per install ID \
             (remove it first)",
            agent_dir.display()
        )));
    }
    let recorded_skills_root: Option<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_dir"))
        .and_then(Json::as_str)
        .map(str::to_string);

    // Ownership recorded by the *other* aliases of this agent directory.
    // Every overlap with another alias's files, skills, or config keys fails
    // closed before any mutation, including byte-identical content.
    let cross_alias = registry::cross_alias_ownership(state_anchor, &agent_dir, &manifest_path)?;

    let mut requested_skills_root: Option<PathBuf> = None;
    if let Some(raw) = opts.skills_dir {
        // Refuse symlinks on the path as given, before resolving: resolving
        // first would follow the link and erase the evidence that the user
        // pointed us at a symlink.
        let requested_raw = fsx::expanduser(Path::new(raw));
        if fsx::has_symlink_component(&requested_raw) {
            return Err(CliError::Usage(format!(
                "refusing to install: skills root '{}' (or a \
                 parent directory) is a symlink",
                requested_raw.display()
            )));
        }
        let resolved = fsx::resolve_loose(&requested_raw);
        if let Some(recorded) = &recorded_skills_root
            && Path::new(recorded) != resolved.as_path()
        {
            return Err(CliError::Usage(format!(
                "refusing to install: {} already has this \
                 topology installed with skills root '{recorded}'; remove \
                 it first or pass the recorded --skills-dir",
                agent_dir.display()
            )));
        }
        if existing_manifest.is_some() && recorded_skills_root.is_none() {
            return Err(CliError::Usage(
                "refusing to install: the existing installation keeps skills \
                 under the agent directory; remove it before installing with \
                 --skills-dir"
                    .to_string(),
            ));
        }
        requested_skills_root = Some(resolved);
    }
    let skills_root: Option<PathBuf> = match (&requested_skills_root, &recorded_skills_root) {
        (Some(resolved), _) => Some(resolved.clone()),
        (None, Some(recorded)) => Some(PathBuf::from(recorded)),
        (None, None) => None,
    };
    if let Some(root) = &skills_root
        && fsx::has_symlink_component(root)
    {
        return Err(CliError::Usage(format!(
            "refusing to install: skills root '{}' (or a parent \
             directory) is a symlink",
            root.display()
        )));
    }

    let owned_agent_files: HashSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("files"))
        .and_then(Json::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    let owned_skill_files: HashSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_files"))
        .and_then(Json::as_object)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();

    // Profile-visible main->topology links recorded by a previous run.
    // Their identity (target + link inode) is re-verified before any
    // mutation so a retargeted or replaced link fails closed instead of
    // silently writing through to a new destination.
    let existing_symlinks: BTreeMap<String, symlinks::LinkIdentity> = existing_manifest
        .as_ref()
        .map(symlinks::links_from_manifest)
        .unwrap_or_default();
    // The active config may itself live behind a supported main->topology
    // link (main `~/.omp/agent/config.yml` -> the selected profile's own
    // `agent/config.yml`). Resolve it through the same trust and identity
    // validation as topology-declared files: a drifted, dangling,
    // escaping, or newly appeared link fails closed before any mutation,
    // and every active-config read, write, restore, and rollback goes to
    // the resolved target so the visible symlink itself is never
    // replaced. The candidate publication always stays a plain file at
    // the visible main-agent path.
    let config_rel = yamlwork::CONFIG_NAME;
    let declared = symlinks::resolve_declared_at(&roots, config_rel, &meta_name)?;
    let config_present = declared.exists().unwrap_or(false);
    let config_is_file = declared.is_file().unwrap_or(false);
    let (config_target, config_identity) = match declared {
        ResolvedAt::Direct { resolved, .. } => {
            if existing_symlinks.contains_key(config_rel) {
                return Err(CliError::Usage(format!(
                    "refusing to install: '{}' was installed through a \
                         profile link, but that link is gone; refusing to \
                         touch the now-plain path",
                    config_path.display(),
                )));
            }
            (resolved, None)
        }
        ResolvedAt::Linked {
            resolved, identity, ..
        } => {
            match existing_symlinks.get(config_rel) {
                Some(recorded) if *recorded == identity => {}
                Some(recorded) => {
                    return Err(CliError::Usage(format!(
                        "refusing to install: '{}' is a symlink whose \
                             target changed since install (was '{}', now \
                             '{}'); refusing to follow a retargeted link",
                        config_path.display(),
                        recorded.target.display(),
                        identity.target.display(),
                    )));
                }
                None => {}
            }
            (resolved, Some(identity))
        }
    };
    let mut original_config: Option<Vec<u8>> = None;
    if config_present {
        if !config_is_file {
            return Err(CliError::Usage(format!(
                "refusing to install: '{}' exists and is not a \
                 regular file",
                config_path.display()
            )));
        }
        // Read the active config exactly once; everything below works from
        // this snapshot and the live file is re-verified before any
        // replacement so a concurrent edit can never be clobbered.
        original_config = Some(
            symlinks::read_pinned(&roots, &config_target)?
                .ok_or_else(|| io_err(&config_target, not_found_io()))?,
        );
    }
    let what = format!("config '{}'", config_path.display());
    let user_doc = match &original_config {
        Some(bytes) => Some(yamlwork::parse_config(bytes, &what)?),
        None => None,
    };
    let user_model = match &user_doc {
        Some(doc) => yamlwork::migrated_model(doc, &what)?,
        None => Value::Map(BTreeMap::new()),
    };
    // Same check as `preflight_config_types`, repeated under the registry
    // lock so a config edited between the lock-free preflight and this
    // run still fails closed before any mutation.
    refuse_config_type_conflicts(&topo_model, &user_model)?;
    // Files flagged pre-existing in the manifest reached their target
    // through a user-made main->topology link before install: ownership
    // was adopted, never created, so remove and source-drop release them
    // without deleting. The flag carries forward for as long as the
    // topology ships the file.
    let mut preexisting_files: BTreeSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("preexisting"))
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Resolve every owned agent file through the recorded link identity
    // (or as a plain path) up front: a drifted or replaced link aborts
    // the run before any mutation, per the safe-symlink contract.
    let mut owned_resolved: BTreeMap<String, PathBuf> = BTreeMap::new();
    for rel in &owned_agent_files {
        owned_resolved.insert(
            rel.clone(),
            symlinks::resolve_owned_at(
                &roots,
                rel,
                &meta_name,
                existing_symlinks.get(rel.as_str()),
            )?
            .resolved()
            .to_path_buf(),
        );
    }

    // ---- collision detection (before any mutation) ----
    let sources = topology::source_agent_files(&topology_dir)?;
    let mut agent_sources: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut skill_sources: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, source_bytes) in sources {
        if skills_root.is_some() && rel.starts_with("skills/") {
            // With an explicit skills root, skills/* is routed there and
            // the agent directory's own skills/ path is never inspected.
            skill_sources.insert(rel["skills/".len()..].to_string(), source_bytes);
        } else {
            agent_sources.insert(rel, source_bytes);
        }
    }
    let mut collisions: Vec<String> = Vec::new();
    let mut collision_owners: Vec<String> = Vec::new();
    // One digest per candidate file: computed once here and reused by the
    // update planning and the transaction record instead of re-hashing.
    let agent_source_hashes: BTreeMap<String, String> = agent_sources
        .iter()
        .map(|(rel, bytes)| (rel.clone(), fsx::sha256_hex(bytes)))
        .collect();
    let skill_source_hashes: BTreeMap<String, String> = skill_sources
        .iter()
        .map(|(rel, bytes)| (rel.clone(), fsx::sha256_hex(bytes)))
        .collect();
    let mut current_bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut current_hashes: BTreeMap<String, String> = BTreeMap::new();
    // Profile-visible link identities adopted (initial install) or
    // carried forward (update) for the files this topology ships.
    let mut link_identities: BTreeMap<String, symlinks::LinkIdentity> = BTreeMap::new();
    // Resolved write target per shipped relative path: the plain
    // agent-dir path, or the profile-visible target behind a supported
    // main->topology link.
    let mut agent_resolved: BTreeMap<String, PathBuf> = BTreeMap::new();
    // Modes of backed-up targets, captured together with the read that
    // retained their bytes, so a rollback can restore the original
    // permissions instead of the restrictive atomic-write default.
    let mut backup_modes: BTreeMap<PathBuf, u32> = BTreeMap::new();
    for rel in agent_sources.keys() {
        // Supported main->topology links are followed to their resolved
        // target (recording the identity for later rechecks); any other
        // link shape (dangling, escaping ~/.omp, retargeted, or a new
        // link where the manifest recorded a plain path) is a collision.
        let resolved = match symlinks::resolve_declared_at(&roots, rel, &meta_name) {
            Ok(ResolvedAt::Direct { resolved, .. }) => {
                if existing_symlinks.contains_key(rel.as_str()) {
                    collisions.push(format!(
                        "{rel} (or a parent directory) changed since install: the recorded profile link is gone; refusing to touch it"
                    ));
                    continue;
                }
                resolved
            }
            Ok(ResolvedAt::Linked {
                resolved, identity, ..
            }) => {
                match existing_symlinks.get(rel.as_str()) {
                    Some(recorded) if *recorded == identity => {}
                    Some(recorded) => {
                        collisions.push(format!(
                            "{rel}: profile link points to '{}' instead of the recorded target '{}'; refusing to follow a retargeted link",
                            identity.target.display(),
                            recorded.target.display(),
                        ));
                        continue;
                    }
                    None => {}
                }
                // Carried-forward and newly adopted links alike stay
                // recorded in the manifest.
                link_identities.insert(rel.clone(), identity);
                resolved
            }
            Err(err) => {
                collisions.push(format!("{rel}: {err}"));
                continue;
            }
        };
        agent_resolved.insert(rel.clone(), resolved.clone());
        // Read through the pinned descriptor that resolved `target` so
        // the read races no `open()`-time symlink swap.
        let current = match symlinks::read_pinned(&roots, &resolved)? {
            Some(bytes) => bytes,
            None => continue,
        };
        let current_hash = fsx::sha256_hex(&current);
        if !owned_agent_files.contains(rel) {
            // A sibling alias of this agent directory already managing
            // this path is a collision naming the owner — even for
            // byte-identical bytes; there is no shared file ownership.
            if let Some(owner) = cross_alias.files.get(rel) {
                note_collision_owner(&mut collision_owners, owner);
                collisions.push(format!(
                    "{rel} already exists and is managed by install ID '{owner}'; \
                     refusing to overwrite it"
                ));
            } else {
                // Any pre-existing file without our manifest is a collision,
                // even if byte-identical: remove must never delete a file the
                // user owned before install. The single exception is a file
                // reached through a supported main->topology link whose live
                // bytes exactly match the topology source — the hand-made
                // equivalent of what install would have written. It is
                // adopted and flagged pre-existing so remove and source-drop
                // release it without deleting; a differing unowned target,
                // linked or not, still refuses.
                let adopted = link_identities.contains_key(rel)
                    && agent_source_hashes.get(rel) == Some(&current_hash);
                if !adopted {
                    collisions.push(format!(
                        "{rel} already exists and is not owned by this topology \
                         (unowned file-name collision)"
                    ));
                } else {
                    preexisting_files.insert(rel.clone());
                }
            }
        } else {
            // Retain backup bytes only for owned, unedited files this
            // update will actually overwrite; preserved edits/deletions,
            // no-op files, and refused collisions need no retained bytes.
            let recorded = manifest_string(&existing_manifest, "files", rel);
            if recorded.as_deref() == Some(current_hash.as_str())
                && agent_source_hashes[rel] != current_hash
            {
                if let Ok(Some(mode)) = symlinks::file_mode_pinned(&roots, rel, &resolved) {
                    backup_modes.insert(resolved.clone(), mode);
                }
                current_bytes.insert(rel.clone(), current);
            }
        }
        current_hashes.insert(rel.clone(), current_hash);
    }
    let mut current_skill_bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut current_skill_hashes: BTreeMap<String, String> = BTreeMap::new();
    let skills_pin = match &skills_root {
        Some(root) => open_skills_pin(root)?,
        None => None,
    };
    if let Some(root) = &skills_root {
        let root_string = root.display().to_string();
        for rel in skill_sources.keys() {
            let target = root.join(rel);
            let symlinked = match &skills_pin {
                Some(pin) => pin
                    .has_symlink_component(rel)
                    .map_err(|err| io_err(&target, err))?,
                None => false,
            };
            if symlinked {
                collisions.push(format!(
                    "{rel} under skills root '{}' (or a parent directory) \
                     is a symlink; refusing to touch it",
                    root.display()
                ));
                continue;
            }
            let Some(pin) = &skills_pin else {
                continue;
            };
            let current = match pin.read_file(rel) {
                Ok(current) => current,
                // A non-directory parent has no readable leaf, matching
                // the previous path-based inventory. Apply will fail at
                // the pinned write and roll back any earlier mutations.
                Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => None,
                Err(err) => return Err(io_err(&target, err)),
            };
            let Some(current) = current else {
                continue;
            };
            let current_hash = fsx::sha256_hex(&current);
            if !owned_skill_files.contains(rel) {
                if let Some(owner) = cross_alias
                    .skills_files
                    .get(&root_string)
                    .and_then(|files| files.get(rel))
                {
                    // Sibling alias already manages this skill file: a
                    // collision naming the owner, even for byte-identical
                    // bytes — there is no shared file ownership.
                    note_collision_owner(&mut collision_owners, owner);
                    collisions.push(format!(
                        "{rel} already exists under skills root '{}' and is \
                         managed by install ID '{owner}'; refusing to overwrite it",
                        root.display()
                    ));
                } else {
                    // Owned skill files never collide: unedited copies are
                    // updated in place, while copies the user edited or
                    // deleted are preserved by the update planning below.
                    collisions.push(format!(
                        "{rel} already exists under skills root '{}' and is \
                         not owned by this topology (unowned file-name collision)",
                        root.display()
                    ));
                }
            } else {
                // Retain backup bytes only for owned, unedited skill
                // files this update will actually overwrite.
                let recorded = manifest_string(&existing_manifest, "skills_files", rel);
                if recorded.as_deref() == Some(current_hash.as_str())
                    && skill_source_hashes[rel] != current_hash
                {
                    if let Ok(Some(mode)) = pin.file_mode(rel) {
                        backup_modes.insert(target.clone(), mode);
                    }
                    current_skill_bytes.insert(rel.clone(), current);
                }
            }
            current_skill_hashes.insert(rel.clone(), current_hash);
        }
    }
    if !collisions.is_empty() {
        let message = format!(
            "refusing to install: file-name collision:\n  - {}",
            collisions.join("\n  - ")
        );
        return Err(if collision_owners.is_empty() {
            // No colliding file has a recorded owner (only symlink,
            // link-drift, or unowned-file refusals): keep the plain
            // usage diagnostic.
            CliError::Usage(message)
        } else {
            CliError::InstallCollision {
                message,
                owners: collision_owners,
            }
        });
    }

    // ---- files the new topology no longer ships ----
    // Ownership of a dropped file ends with this update: an unedited copy
    // (live bytes still match the recorded digest) is deleted outright
    // because the topology no longer provides it, while an edited copy is
    // preserved on disk but released from ownership so a later remove
    // never deletes a user edit. Link drift on a still-owned path fails
    // closed above; already-missing files are only released, never
    // recreated.
    let mut agent_deletions: BTreeMap<String, String> = BTreeMap::new();
    for rel in owned_agent_files.iter() {
        if agent_sources.contains_key(rel) {
            continue;
        }
        // Pre-existing files predate the install, so a source drop
        // releases them from ownership without deleting — however
        // closely the live bytes still match the recorded digest.
        if preexisting_files.contains(rel) {
            continue;
        }
        let Some(target) = owned_resolved.get(rel).cloned() else {
            continue;
        };
        // Read through the pinned descriptor, so a swapped link between
        // planning and this read is observed, not followed.
        let Some(current) = symlinks::read_pinned(&roots, &target)? else {
            continue;
        };
        let current_hash = fsx::sha256_hex(&current);
        if manifest_string(&existing_manifest, "files", rel).as_deref()
            == Some(current_hash.as_str())
        {
            agent_deletions.insert(rel.clone(), current_hash);
            if let Ok(Some(mode)) = symlinks::file_mode_pinned(&roots, rel, &target) {
                backup_modes.insert(target.clone(), mode);
            }
            current_bytes.insert(rel.clone(), current);
        }
    }
    let mut skill_deletions: BTreeMap<String, String> = BTreeMap::new();
    if let Some(root) = &skills_root
        && let Some(pin) = &skills_pin
    {
        for rel in owned_skill_files.iter() {
            if skill_sources.contains_key(rel) {
                continue;
            }
            let target = root.join(rel);
            if pin
                .has_symlink_component(rel)
                .map_err(|err| io_err(&target, err))?
            {
                continue;
            }
            let Some(current) = pin.read_file(rel).map_err(|err| io_err(&target, err))? else {
                continue;
            };
            let current_hash = fsx::sha256_hex(&current);
            if manifest_string(&existing_manifest, "skills_files", rel).as_deref()
                == Some(current_hash.as_str())
            {
                skill_deletions.insert(rel.clone(), current_hash);
                if let Ok(Some(mode)) = pin.file_mode(rel) {
                    backup_modes.insert(target.clone(), mode);
                }
                current_skill_bytes.insert(rel.clone(), current);
            }
        }
    }

    // ---- config handling (per command; modes are never remembered) ----
    let existing_config = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config"))
        .cloned()
        .unwrap_or(Json::Null);
    let mut prior_values: BTreeMap<Vec<String>, Option<Value>> = existing_config
        .get("prior")
        .map(state::decode_values)
        .unwrap_or_default();
    let existing_installed_values: BTreeMap<Vec<String>, Option<Value>> = existing_config
        .get("installed")
        .map(state::decode_values)
        .unwrap_or_default();
    // Config paths the profile precedence deliberately kept out of
    // topology ownership. Recorded so a later merge update keeps
    // preserving the live profile value over the graft even though no
    // `installed` value exists for it, while remove keeps ignoring the
    // key entirely.
    let existing_profile_preserved: BTreeSet<Vec<String>> = existing_config
        .get("profile_preserved")
        .map(state::decode_values)
        .unwrap_or_default()
        .into_keys()
        .collect();
    let existing_candidate_hash: Option<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config_candidate_sha256"))
        .and_then(Json::as_str)
        .map(str::to_string);
    // Whether a previous run recorded the active config's link identity
    // together with merge ownership. Candidate-mode updates record the
    // identity too, so the recorded link alone cannot stand in for merge
    // history: only this marker proves a merge run already reconciled
    // the profile's own values behind the link.
    let existing_link_merge: bool = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config_link_merge"))
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let topo_empty = topo_model.is_empty_map();
    let mut topo_leafs: Vec<Vec<String>> = Vec::new();
    value::leaf_paths(&topo_model, &mut Vec::new(), &mut topo_leafs);
    let topo_leaf_set: BTreeSet<Vec<String>> = topo_leafs.iter().cloned().collect();

    // Config keys another alias of this agent directory already owns are
    // off limits in every mode, even when the live value happens to
    // match: leaving them unrecorded here would let the sibling's remove
    // restore or delete a key this topology still needs, and there is no
    // shared config ownership.
    if let Some((leaf, owner)) = cross_alias
        .config_installed
        .iter()
        .find(|(parts, _)| topo_leaf_set.contains(*parts))
    {
        return Err(CliError::Usage(format!(
            "refusing to install: config key '{}' is already managed by \
             install ID '{owner}' for {}; refusing to overwrite it",
            leaf.join("."),
            agent_dir.display()
        )));
    }

    // Planned changes to the active config and the candidate publication.
    // `merged_bytes` replaces the active config; `candidate_action`
    // publishes or withdraws `config.yml.new` beside an untouched active
    // config; `planned_replace` swaps the active config for the verbatim
    // topology bytes (with the replaced bytes kept as the manifest
    // backup). At most one of the active-config changes is set.
    let mut merged_bytes: Option<Vec<u8>> = None;
    let mut planned_replace: Option<Vec<u8>> = None;
    let mut replace_replaced_hash: Option<String> = None;
    let mut replace_backup_text: Option<String> = None;
    let mut candidate_action: Option<CandidateAction> = None;
    let mut candidate_hash_kept: Option<String> = None;
    let mut candidate_backup_bytes: Option<Vec<u8>> = None;
    let mut config_created_by_run = false;
    // Candidate mode: an update refreshed an installer-created active
    // config (still byte-identical to the recorded creation bytes) with
    // the changed topology config, instead of publishing a candidate.
    let mut config_refreshed_by_run = false;
    let mut dropped_leafs: Vec<Vec<String>> = Vec::new();
    // Leafs the profile precedence keeps out of topology ownership (merge
    // mode). They must not be recorded as topology-owned in the manifest:
    // remove only restores what the topology owns, and claiming a
    // profile-owned value would make remove treat the preserved
    // divergence as a user edit.
    let mut profile_preserved_leafs: BTreeSet<Vec<String>> = BTreeSet::new();

    match &opts.config_mode {
        ConfigMode::Candidate => {
            let candidate_path = agent_dir.join(CONFIG_NEW_NAME);
            // config.yml.new is single-owner: publishing or deleting beside
            // another alias's candidate would corrupt that alias's rollback
            // bookkeeping, so a sibling-owned candidate always fails closed.
            if let Some(owner) = &cross_alias.candidate_owner {
                return Err(CliError::Usage(format!(
                    "refusing to install: '{}' is already managed by install ID \
                     '{owner}' for {}; remove that candidate (or that install) \
                     first",
                    candidate_path.display(),
                    agent_dir.display()
                )));
            }
            if original_config.is_none() && existing_manifest.is_none() {
                // Initial install: the active config is created from the
                // topology config only when absent. Serialize the already
                // migrated topology document — exactly like merge mode does
                // for an absent config — so the first-install spellings are
                // unambiguous YAML 1.2 (`true`, not `on`) and a reinstall
                // recognizes the created config as up to date.
                if !topo_empty {
                    let doc = topo_doc
                        .as_ref()
                        .expect("non-empty topology config parsed above");
                    merged_bytes = Some(doc.to_string().into_bytes());
                    config_created_by_run = true;
                } else if let Some(raw) = &topo_config_raw {
                    // An empty topology config still materializes an
                    // empty active file.
                    merged_bytes = Some(raw.clone());
                    config_created_by_run = true;
                }
            } else {
                // An existing active config is never modified. A deleted
                // active config is not recreated; the topology version is
                // published beside it instead.
                let recorded = existing_candidate_hash;
                // Read through the pinned agent root: a swapped candidate
                // path is observed as a changed file, never followed.
                let live = symlinks::read_pinned(&roots, &candidate_path)?;
                let live_hash = live.as_ref().map(|bytes| fsx::sha256_hex(bytes));
                match (recorded, topo_config_raw.clone()) {
                    (None, planned) => {
                        if live.is_some() {
                            return Err(CliError::Usage(format!(
                                "refusing to install: '{}' already exists and is not owned by this topology; remove it or rerun with --config-merge/--config-replace",
                                candidate_path.display()
                            )));
                        }
                        if let Some(bytes) = planned {
                            // An active config the installer created and
                            // the user has not since edited is owned by
                            // the installer, not the user: a changed
                            // topology config refreshes it in place, so a
                            // config-only source drift stays updatable
                            // with a plain update. Only a divergence from
                            // the recorded creation bytes proves a user
                            // edit — that (and any pre-existing user
                            // config, which never sets `config_created`)
                            // reverts candidate mode to publishing
                            // `config.yml.new` beside the untouched
                            // active config.
                            let created_config_unedited = existing_manifest
                                .as_ref()
                                .and_then(|manifest| manifest.get("config_created"))
                                .and_then(Json::as_bool)
                                .unwrap_or(false)
                                && match (
                                    existing_manifest
                                        .as_ref()
                                        .and_then(|manifest| manifest.get("config_created_sha256"))
                                        .and_then(Json::as_str),
                                    original_config.as_ref(),
                                ) {
                                    (Some(recorded), Some(live)) => {
                                        fsx::sha256_hex(live) == recorded
                                    }
                                    // Manifests predating the recorded
                                    // baseline fall back to the current
                                    // topology serialization — the same
                                    // reinstall no-op check as before the
                                    // baseline existed.
                                    (None, Some(live)) => {
                                        !topo_empty
                                            && Some(live.as_slice())
                                                == topo_doc
                                                    .as_ref()
                                                    .map(|doc| doc.to_string().into_bytes())
                                                    .as_deref()
                                    }
                                    _ => false,
                                };
                            if created_config_unedited {
                                // Serialize exactly like the creation
                                // branch (migrated document, not the raw
                                // bytes) so an unchanged topology config
                                // still hashes to the live file and the
                                // run stays a no-op.
                                let refreshed: Vec<u8> = if !topo_empty {
                                    topo_doc
                                        .as_ref()
                                        .expect("non-empty topology config parsed above")
                                        .to_string()
                                        .into_bytes()
                                } else {
                                    bytes.clone()
                                };
                                if Some(refreshed.as_slice()) != original_config.as_deref() {
                                    merged_bytes = Some(refreshed);
                                    config_refreshed_by_run = true;
                                }
                                // Else the created config is already
                                // current: nothing to publish, nothing to
                                // rewrite — the reinstall stays a no-op
                                // instead of dropping a redundant
                                // candidate beside the config it
                                // duplicates.
                            } else {
                                candidate_action = Some(CandidateAction::Publish(bytes));
                            }
                        }
                    }
                    (Some(recorded), None) => {
                        if live_hash.as_deref() == Some(recorded.as_str()) {
                            candidate_backup_bytes = live;
                            candidate_action = Some(CandidateAction::Delete);
                        } else if live.is_some() {
                            return Err(CliError::Usage(format!(
                                "refusing to install: '{}' was modified outside this topology's updates; refusing to overwrite it",
                                candidate_path.display()
                            )));
                        }
                    }
                    (Some(recorded), Some(bytes)) => {
                        if live_hash.as_deref() == Some(recorded.as_str()) {
                            if fsx::sha256_hex(&bytes) != recorded {
                                candidate_backup_bytes = live;
                                candidate_action = Some(CandidateAction::Publish(bytes));
                            } else {
                                // Unchanged owned candidate, unchanged
                                // topology config: no action, but the
                                // recorded hash must survive in the
                                // manifest so later runs keep recognizing
                                // the candidate as installer-owned.
                                candidate_hash_kept = Some(recorded);
                            }
                        } else if live.is_none() {
                            candidate_action = Some(CandidateAction::Publish(bytes));
                        } else {
                            return Err(CliError::Usage(format!(
                                "refusing to install: '{}' was modified outside this topology's updates; refusing to overwrite it",
                                candidate_path.display()
                            )));
                        }
                    }
                }
            }
        }
        ConfigMode::Replace => {
            // Full replacement of the active config with the verbatim
            // topology bytes; the replaced bytes are kept in the manifest
            // so remove can restore them (only while the active config
            // still matches the recorded replacement).
            if let Some(raw) = &topo_config_raw {
                // Replace swaps the whole active config, so a config key
                // a sibling alias owns — even one disjoint from this
                // topology's config — is overwritten or removed. Fail
                // closed before planning the replacement; merge mode is
                // the way to share a config between aliases.
                refuse_replace_over_sibling_ownership(&cross_alias, &agent_dir)?;
                planned_replace = Some(raw.clone());
                replace_replaced_hash = Some(fsx::sha256_hex(raw));
                replace_backup_text = original_config
                    .as_ref()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            }
        }
        ConfigMode::Merge { precedence } => {
            // Leaves the previous install recorded but the new topology
            // no longer ships are reconciled with the same edit-
            // preservation rules remove uses: a value still equal to the
            // recorded installed value is restored to the user's prior
            // value (or deleted when the user did not have one); an
            // edited or already-removed value stays as the user left it
            // and is simply released from ownership metadata below.
            let mut dropped_restore: Vec<(Vec<String>, Value)> = Vec::new();
            let mut dropped_delete: Vec<Vec<String>> = Vec::new();
            for parts in existing_installed_values.keys() {
                if topo_leaf_set.contains(parts) {
                    continue;
                }
                dropped_leafs.push(parts.clone());
                let installed = existing_installed_values[parts]
                    .clone()
                    .unwrap_or(Value::Null);
                if value::resolve_path(&user_model, parts) != Some(&installed) {
                    continue;
                }
                match prior_values.get(parts) {
                    Some(Some(prior)) => dropped_restore.push((parts.clone(), prior.clone())),
                    None | Some(None) => dropped_delete.push(parts.clone()),
                }
            }

            // Leaves the new topology still owns whose live value
            // diverged from what the previous install recorded are user
            // edits: the graft must not overwrite them. Each is written
            // back over the merged result with the same restore helper
            // remove uses. The manifest keeps recording the topology's
            // value as `installed`, so the divergence stays visible to
            // remove and to future updates, and `prior` stays the
            // pre-install snapshot.
            let mut preserved_values: Vec<(Vec<String>, Value)> = Vec::new();
            for parts in &topo_leafs {
                let old = match existing_installed_values.get(parts) {
                    Some(old_installed) => old_installed.clone().unwrap_or(Value::Null),
                    // Profile-owned paths stay preserved like an edit even
                    // though no `installed` value was ever recorded for
                    // them; a path the topology newly claims keeps the
                    // normal first-install graft semantics.
                    None if existing_profile_preserved.contains(parts) => Value::Null,
                    None => continue, // newly owned path: first-install graft semantics
                };
                let Some(live) = value::resolve_path(&user_model, parts) else {
                    continue; // absent live value: normal graft semantics
                };
                if live == &old {
                    continue; // unedited: the graft applies the topology update
                }
                if Some(live) == value::resolve_path(&topo_model, parts) {
                    continue; // already matches the new source: graft is a no-op
                }
                preserved_values.push((parts.clone(), live.clone()));
            }
            if matches!(precedence, MergePrecedence::Profile)
                && original_config.is_some()
                && !topo_empty
                && (existing_manifest.is_none()
                    || (config_identity.is_some() && !existing_link_merge))
            {
                // Under profile precedence the profile keeps winning over
                // the topology graft wherever the profile already carries a
                // value: on first install, and for an active config adopted
                // behind a main->profile link this run. The topology's
                // ownership was recorded for a plain agent config and does
                // not extend to the profile's own data, so only keys the
                // profile does not carry are adopted from the graft. A
                // config link the manifest recorded without the
                // `config_link_merge` marker (candidate mode records link
                // identities too) has no merge history behind it, so it
                // adopts exactly like a newly appeared link.
                for parts in &topo_leafs {
                    let Some(live) = value::resolve_path(&user_model, parts) else {
                        continue;
                    };
                    if Some(live) != value::resolve_path(&topo_model, parts) {
                        preserved_values.push((parts.clone(), live.clone()));
                        profile_preserved_leafs.insert(parts.clone());
                    }
                }
            }

            // The reconciliation splices into the user document before
            // the graft: any path the new topology still owns is
            // (re)written by the merge afterwards, never deleted by the
            // restore.
            if original_config.is_some() && (!topo_empty || !dropped_leafs.is_empty()) {
                let user_doc = user_doc.as_ref().expect("original config parsed");
                if !dropped_leafs.is_empty() {
                    // Predict the result tree so mappings emptied by the
                    // deletions can be pruned deepest-first, mirroring
                    // remove.
                    let mut predicted = user_model.clone();
                    for (parts, prior) in &dropped_restore {
                        set_predicted(&mut predicted, parts, prior.clone());
                    }
                    for parts in &dropped_delete {
                        delete_predicted(&mut predicted, parts);
                    }
                    let mut emptied: Vec<Vec<String>> = Vec::new();
                    let mut seen: HashSet<Vec<String>> = HashSet::new();
                    for parts in &dropped_delete {
                        for depth in (1..parts.len()).rev() {
                            let ancestor: Vec<String> = parts[..depth].to_vec();
                            if seen.insert(ancestor.clone()) {
                                emptied.push(ancestor);
                            }
                        }
                    }
                    emptied.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
                    let mut actually_emptied: Vec<Vec<String>> = Vec::new();
                    for parts in &emptied {
                        if value::resolve_path(&predicted, parts).is_some_and(|v| v.is_empty_map())
                        {
                            actually_emptied.push(parts.clone());
                            delete_predicted(&mut predicted, parts);
                        }
                    }
                    yamlwork::restore_config(
                        user_doc,
                        &dropped_restore,
                        &dropped_delete,
                        &actually_emptied,
                    )?;
                }
                // When the live root is an explicit `null`, `merge_config`
                // cannot splice into the concrete scalar and instead
                // merges into a fresh document, returning its text; the
                // live doc stays `null` behind us. A null root models as
                // an empty map, so no live path can resolve and
                // reconciliation/preserved lists are necessarily empty
                // here — the returned text is authoritative.
                let mut merge_replacement: Option<String> = None;
                if !topo_empty {
                    let merged_text = yamlwork::merge_config(
                        topo_doc.as_ref().expect("non-empty topology config parsed"),
                        user_doc,
                        &user_model,
                    )?;
                    if user_doc.as_mapping().is_none() {
                        merge_replacement = Some(merged_text);
                    }
                }
                if !preserved_values.is_empty() {
                    // Write the user-edited (or profile-owned) values
                    // back over the graft so an update never overwrites
                    // them.
                    yamlwork::restore_config(user_doc, &preserved_values, &[], &[])?;
                }
                let merged = merge_replacement.unwrap_or_else(|| user_doc.to_string());
                if merged.as_bytes() != original_config.as_deref().unwrap_or_default() {
                    merged_bytes = Some(merged.into_bytes());
                }
            } else if !topo_empty && original_config.is_none() {
                // Serialize the already-migrated topology document: the
                // first install then uses unambiguous 1.2 spellings
                // (`true`, not `on`), so reinstalling merges
                // byte-identical content and stays a no-op instead of
                // rewriting the config and creating a second backup.
                let doc = topo_doc.as_ref().expect("non-empty topology config parsed");
                merged_bytes = Some(doc.to_string().into_bytes());
            }
        }
    }
    let active_config_change = merged_bytes.is_some() || planned_replace.is_some();

    // ---- file writes needed ----
    let mut file_entries: Map<String, Json> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("files"))
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    let mut skill_file_entries: Map<String, Json> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_files"))
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    // Ownership of files the new topology no longer ships ends with this
    // update: the entries leave the manifest (the on-disk reconciliation
    // above decided whether each file is deleted or a user edit released).
    file_entries.retain(|rel, _| agent_sources.contains_key(rel));
    skill_file_entries.retain(|rel, _| skill_sources.contains_key(rel));
    // Files adopted behind pre-existing main->topology links have no
    // entry yet: seed the model with their live digest (which matched
    // the source at adoption) so the planning below treats them as
    // owned — writing through the link while the bytes stay unedited,
    // preserving user edits, and never overwriting a differing target.
    for rel in preexisting_files.iter() {
        if agent_sources.contains_key(rel)
            && !file_entries.contains_key(rel)
            && let Some(hash) = current_hashes.get(rel)
        {
            file_entries.insert(rel.clone(), Json::String(hash.clone()));
        }
    }
    let mut updates: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut preserved_edits: Vec<String> = Vec::new();
    let mut preserved_deletions: Vec<String> = Vec::new();
    for (rel, source_bytes) in &agent_sources {
        let recorded = file_entries.get(rel).and_then(Json::as_str);
        let current_hash = current_hashes.get(rel).map(String::as_str);
        let source_hash = agent_source_hashes
            .get(rel)
            .map(String::as_str)
            .unwrap_or("");
        match (recorded, current_hash) {
            // Fresh path: create it.
            (None, None) => {
                updates.insert(rel.clone(), source_bytes.clone());
            }
            // A pre-existing unowned file was already refused during
            // collision detection; fail closed rather than ever write a
            // file this topology does not own.
            (None, Some(_)) => {
                return Err(CliError::Usage(format!(
                    "refusing to install: {rel} exists and is not owned by \
                     this topology (unowned file-name collision)"
                )));
            }
            // Owned and unedited since install: rewrite only when the
            // topology itself changed the content. The new digest enters
            // the manifest model here (not only during apply) so a
            // file-only change marks the manifest changed — the dry-run
            // report and the up-to-date check both depend on it.
            (Some(recorded), Some(current_hash)) if recorded == current_hash => {
                if source_hash != recorded {
                    updates.insert(rel.clone(), source_bytes.clone());
                    file_entries.insert(rel.clone(), Json::String(source_hash.to_string()));
                }
            }
            // Owned and edited by the user to exactly the new source
            // bytes: adopt the content without rewriting the file.
            (Some(_), Some(current_hash)) if current_hash == source_hash => {
                file_entries.insert(rel.clone(), Json::String(source_hash.to_string()));
            }
            // Owned and edited by the user (differs from both the
            // recorded digest and the new source): preserve the edit.
            // The recorded digest stays in the manifest so remove and
            // future updates still see the divergence and never delete
            // or overwrite the user's content.
            (Some(_), Some(_)) => {
                preserved_edits.push(rel.clone());
            }
            // Owned and deleted by the user: the deletion is intent, so
            // the file is not silently recreated. The recorded entry
            // stays so a later remove still recognizes the path as an
            // installed one (and reports it missing) rather than as an
            // unowned file.
            (Some(_), None) => {
                preserved_deletions.push(rel.clone());
            }
        }
    }
    let mut skill_updates: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut preserved_skill_edits: Vec<String> = Vec::new();
    let mut preserved_skill_deletions: Vec<String> = Vec::new();
    for (rel, source_bytes) in &skill_sources {
        let recorded = skill_file_entries.get(rel).and_then(Json::as_str);
        let current_hash = current_skill_hashes.get(rel).map(String::as_str);
        let source_hash = skill_source_hashes
            .get(rel)
            .map(String::as_str)
            .unwrap_or("");
        match (recorded, current_hash) {
            (None, None) => {
                skill_updates.insert(rel.clone(), source_bytes.clone());
            }
            (None, Some(_)) => {
                return Err(CliError::Usage(format!(
                    "refusing to install: skills/{rel} exists and is not \
                     owned by this topology (unowned file-name collision)"
                )));
            }
            (Some(recorded), Some(current_hash)) if recorded == current_hash => {
                if source_hash != recorded {
                    skill_updates.insert(rel.clone(), source_bytes.clone());
                    skill_file_entries.insert(rel.clone(), Json::String(source_hash.to_string()));
                }
            }
            (Some(_), Some(current_hash)) if current_hash == source_hash => {
                skill_file_entries.insert(rel.clone(), Json::String(source_hash.to_string()));
            }
            // Owned and edited by the user: preserve the edit; the
            // recorded digest stays so remove keeps recognizing the
            // divergence.
            (Some(_), Some(_)) => {
                preserved_skill_edits.push(rel.clone());
            }
            // Owned and deleted by the user: preserve the deletion.
            (Some(_), None) => {
                preserved_skill_deletions.push(rel.clone());
            }
        }
    }

    // ---- manifest bookkeeping ----
    // Merge mode records the topology's installed values (divergences
    // stay visible to remove and future updates) and the user's prior
    // values for every owned leaf. Candidate and replace modes do not
    // reconcile the active config key by key: candidate mode records
    // values only for an installer-created config, and replace mode keeps
    // the replaced bytes as a whole in `config_replace` instead. Neither
    // mode drops bookkeeping recorded by an earlier mode: tracked
    // `installed` values, the `config_link_merge` marker, and a valid
    // `config_replace` backup all carry forward until remove resolves
    // them or a later replace supersedes them.
    let mut installed_values: BTreeMap<Vec<String>, Option<Value>> = BTreeMap::new();
    match &opts.config_mode {
        ConfigMode::Merge { .. } => {
            for parts in &topo_leafs {
                // A divergence deliberately preserved as the profile's own
                // value is not adopted from the topology: the manifest
                // must not claim topology ownership, or remove would
                // classify the preserved value as a user edit of a
                // topology-owned key and keep the manifest alive. The
                // exclusion carries forward across updates for paths a
                // previous profile-precedence merge recorded.
                if profile_preserved_leafs.contains(parts)
                    || existing_profile_preserved.contains(parts)
                {
                    continue;
                }
                let installed = value::resolve_path(&topo_model, parts).cloned();
                if !installed_values.contains_key(parts) {
                    installed_values.insert(parts.clone(), installed);
                }
                prior_values
                    .entry(parts.clone())
                    .or_insert_with(|| value::resolve_path(&user_model, parts).cloned());
            }
            // Leaves dropped by the new topology leave the metadata
            // entirely: each was either restored to the user's prior
            // value or released from ownership above, so a later remove
            // must not act on them.
            for parts in &dropped_leafs {
                prior_values.remove(parts);
            }
        }
        ConfigMode::Candidate => {
            if config_created_by_run || config_refreshed_by_run {
                // A run that creates or refreshes the installer-created
                // active config owns its full content: record the current
                // topology values so remove and later updates treat the
                // file consistently.
                for parts in &topo_leafs {
                    installed_values.insert(
                        parts.clone(),
                        value::resolve_path(&topo_model, parts).cloned(),
                    );
                    prior_values.entry(parts.clone()).or_insert(None);
                }
            } else {
                // Candidate mode never reconciles the active config, but a
                // previous merge install's tracking still describes it:
                // keep it so remove and later merge updates behave exactly
                // as if the candidate run had not happened.
                installed_values = existing_installed_values.clone();
            }
        }
        ConfigMode::Replace => {}
    }

    let existing_dirs: BTreeSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("created_dirs"))
        .and_then(Json::as_array)
        .map(|dirs| {
            dirs.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let existing_skill_dirs: BTreeSet<String> = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("skills_created_dirs"))
        .and_then(Json::as_array)
        .map(|dirs| {
            dirs.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // The config file is "installer-created" when this install writes it
    // over an absent file, or when a previous install already recorded it
    // as such: remove deletes the file outright once the topology-owned
    // keys are gone, instead of leaving an empty config behind.
    let config_created = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config_created"))
        .and_then(Json::as_bool)
        .unwrap_or(false)
        || config_created_by_run;
    let mut manifest = json!({
        "version": state::MANIFEST_VERSION,
        "topology": meta_name,
        "topology_version": meta_version,
        "files": Json::Object(file_entries),
        "created_dirs": Json::Array(existing_dirs.iter().map(|d| Json::String(d.clone())).collect()),
        "config_created": Json::Bool(config_created),
        "config": {
            "prior": Json::Object(state::encode_values(&prior_values)),
            "installed": Json::Object(state::encode_values(&installed_values)),
        },
    });
    if config_created_by_run || config_refreshed_by_run {
        if let Some(bytes) = &merged_bytes {
            // Baseline for "the installer-created config is unedited":
            // while the live file still hashes to the recorded creation
            // bytes, a candidate-mode update refreshes it in place; a
            // divergence proves a user edit and reverts to publishing
            // `config.yml.new`.
            manifest["config_created_sha256"] = Json::String(fsx::sha256_hex(bytes));
        }
    } else if let Some(recorded) = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config_created_sha256"))
    {
        // The created config survived this run untouched: keep the
        // baseline so a later update still recognizes it.
        manifest["config_created_sha256"] = recorded.clone();
    }
    if !profile_preserved_leafs.is_empty() || !existing_profile_preserved.is_empty() {
        // Profile-owned config paths stay unowned for the life of the
        // installation (or until the topology drops the leaf): later
        // merge updates keep preserving them, and remove never treats
        // them as topology-owned.
        let mut preserved_paths: Vec<&Vec<String>> = profile_preserved_leafs
            .iter()
            .chain(existing_profile_preserved.iter())
            .filter(|parts| !dropped_leafs.contains(parts))
            .collect();
        preserved_paths.sort();
        preserved_paths.dedup();
        if !preserved_paths.is_empty() {
            let preserved_values = preserved_paths
                .iter()
                .map(|parts| ((*parts).clone(), Some(value::Value::Bool(true))))
                .collect();
            manifest["config"]["profile_preserved"] =
                Json::Object(state::encode_values(&preserved_values));
        }
    }
    if !link_identities.is_empty() || config_identity.is_some() {
        // Profile-visible link identities for shipped files and the
        // active config: update and remove re-verify them (link path,
        // recorded target, canonical destination) before touching a
        // resolved path again.
        let mut links = Map::new();
        for (rel, identity) in &link_identities {
            links.insert(rel.clone(), symlinks::identity_json(identity));
        }
        if let Some(identity) = &config_identity {
            links.insert(config_rel.to_string(), symlinks::identity_json(identity));
        }
        manifest["symlinks"] = Json::Object(links);
    }
    if config_identity.is_some()
        && (matches!(&opts.config_mode, ConfigMode::Merge { .. }) || existing_link_merge)
    {
        // Record merge ownership through the config link explicitly: a
        // candidate-mode update also records the link identity, so the
        // identity alone cannot prove a merge ever reconciled the
        // profile's own values. Carried forward across candidate (and
        // replace) updates exactly like the tracked `installed` values.
        manifest["config_link_merge"] = Json::Bool(true);
    }
    if !preexisting_files.is_empty() {
        // Flag only files the topology still ships: a dropped pre-existing
        // file is released unowned, and the flag must not outlive the
        // ownership entry.
        let flagged: Vec<Json> = preexisting_files
            .iter()
            .filter(|rel| agent_sources.contains_key(*rel))
            .map(|rel| Json::String(rel.clone()))
            .collect();
        if !flagged.is_empty() {
            manifest["preexisting"] = Json::Array(flagged);
        }
    }
    match &candidate_action {
        Some(CandidateAction::Publish(bytes)) => {
            manifest["config_candidate_sha256"] = Json::String(fsx::sha256_hex(bytes));
        }
        Some(CandidateAction::Delete) => {}
        None => {
            // No candidate action but an unchanged installer-owned
            // candidate still on disk: keep recording its hash so the
            // ownership survives this manifest rewrite.
            if let Some(hash) = candidate_hash_kept {
                manifest["config_candidate_sha256"] = Json::String(hash);
            }
        }
    }
    if let ConfigMode::Replace = opts.config_mode
        && let Some(hash) = &replace_replaced_hash
    {
        manifest["config_replace"] = json!({
            "replaced_sha256": hash,
            "backup_text": replace_backup_text,
        });
    } else if let Some(existing_replace) = existing_manifest
        .as_ref()
        .and_then(|manifest| manifest.get("config_replace"))
    {
        // A candidate or merge update never reconciles the replaced
        // bytes, so dropping the section here would destroy the only
        // recovery copy of the pre-replacement config. Keep it until
        // remove restores or releases it, or a later explicit
        // --config-replace supersedes it.
        manifest["config_replace"] = existing_replace.clone();
    }
    if let Some(root) = &skills_root {
        // Split-root install: record the explicit skills root so remove can
        // find the skills again without ever consulting (or following) a
        // symlinked <agent-dir>/skills path.
        manifest["skills_dir"] = Json::String(root.display().to_string());
        manifest["skills_files"] = Json::Object(skill_file_entries.clone());
        manifest["skills_created_dirs"] = Json::Array(
            existing_skill_dirs
                .iter()
                .map(|d| Json::String(d.clone()))
                .collect(),
        );
    }
    let manifest_bytes = state::serialize_manifest(&manifest);
    let manifest_changed = match &existing_manifest {
        None => true,
        Some(existing) => manifest_bytes != state::serialize_manifest(existing),
    };

    if updates.is_empty()
        && skill_updates.is_empty()
        && agent_deletions.is_empty()
        && skill_deletions.is_empty()
        && !active_config_change
        && candidate_action.is_none()
        && !manifest_changed
    {
        out.push(format!(
            "topology '{}' is already up to date in {}",
            opts.topology,
            agent_dir.display()
        ));
        push_preservation_notes(
            out,
            false,
            &preserved_edits,
            &preserved_deletions,
            &preserved_skill_edits,
            &preserved_skill_deletions,
        );
        return Ok(0);
    }

    if opts.dry_run {
        out.push(format!(
            "dry-run: installing '{}' into {}",
            opts.topology,
            agent_dir.display()
        ));
        for rel in updates.keys() {
            match agent_resolved.get(rel) {
                Some(resolved) if resolved != &agent_dir.join(rel) => out.push(format!(
                    "  would write: {rel} (through profile link -> {})",
                    resolved.display()
                )),
                _ => out.push(format!("  would write: {rel}")),
            }
        }
        for rel in agent_deletions.keys() {
            // Dropped files are no longer shipped, so they were never
            // entered in `agent_resolved`; their link resolution lives in
            // `owned_resolved`, keyed by the ownership manifest.
            match owned_resolved.get(rel) {
                Some(resolved) if resolved != &agent_dir.join(rel) => out.push(format!(
                    "  would remove: {rel} (dropped from the new topology; \
                     through profile link -> {})",
                    resolved.display()
                )),
                _ => out.push(format!(
                    "  would remove: {rel} (dropped from the new topology)"
                )),
            }
        }
        if let Some(root) = &skills_root {
            out.push(format!("dry-run: skills go under {}", root.display()));
            for rel in skill_updates.keys() {
                out.push(format!("  would write: skills/{rel}"));
            }
            for rel in skill_deletions.keys() {
                out.push(format!(
                    "  would remove: skills/{rel} (dropped from the new topology)"
                ));
            }
        }
        if merged_bytes.is_some() {
            out.push(format!("  would merge: {}", yamlwork::CONFIG_NAME));
        }
        if let Some(bytes) = &planned_replace {
            out.push(format!(
                "  would replace: {} with:\n{}",
                yamlwork::CONFIG_NAME,
                String::from_utf8_lossy(bytes)
            ));
        }
        match &candidate_action {
            Some(CandidateAction::Publish(_)) => out.push(format!(
                "  would publish: {CONFIG_NEW_NAME} (active {} left unchanged)",
                yamlwork::CONFIG_NAME
            )),
            Some(CandidateAction::Delete) => out.push(format!(
                "  would remove: {CONFIG_NEW_NAME} (topology no longer ships a config)"
            )),
            None => {}
        }
        if manifest_changed {
            out.push("  would update: ownership manifest".to_string());
        }
        push_preservation_notes(
            out,
            true,
            &preserved_edits,
            &preserved_deletions,
            &preserved_skill_edits,
            &preserved_skill_deletions,
        );
        return Ok(0);
    }

    // ---- apply ----
    if let Some(original) = &original_config {
        let live = symlinks::read_pinned(&roots, &config_target).unwrap_or_default();
        if live.as_deref() != Some(original.as_slice()) {
            return Err(CliError::Usage(format!(
                "'{}' changed while the install was being planned; \
                 aborting without touching it",
                config_path.display()
            )));
        }
    }
    RootFd::open_or_create(&agent_dir).map_err(|err| io_err(&agent_dir, err))?;
    // The agent tree now exists: re-pin so every subsequent mutation and
    // re-verification walks descriptors opened against the real tree
    // (on a first install the planning pins opened before it existed).
    let roots = symlinks::Roots::open(&agent_dir)?;
    let state_dir = state::agent_state_dir(&agent_dir, state_anchor)?;
    let backup_dir = state_dir
        .join("backups")
        .join(format!("install-{}", fsx::timestamp()));
    // Backups carry their target root so agent and skills files never
    // collide inside the backup tree and rollback restores each file to
    // the root it came from. The byte buffers are MOVED into `backups`
    // (from `original_config` and the per-target read caches) so backup
    // content exists exactly once overall — no second O(B) copy.
    let original_hash = original_config.as_ref().map(|bytes| fsx::sha256_hex(bytes));
    let mut backups: Vec<(PathBuf, String, Vec<u8>)> = Vec::new();
    let config_backup_index = if original_config.is_some() && active_config_change {
        let index = backups.len();
        backups.push((
            agent_dir.clone(),
            yamlwork::CONFIG_NAME.to_string(),
            original_config.take().expect("checked above"),
        ));
        Some(index)
    } else {
        None
    };
    // Overwriting an installer-owned candidate retains its bytes so a
    // failed refresh can roll back to the prior publication.
    let candidate_backup_index = if let Some(bytes) = &candidate_backup_bytes {
        let index = backups.len();
        backups.push((
            agent_dir.clone(),
            CONFIG_NEW_NAME.to_string(),
            bytes.clone(),
        ));
        Some(index)
    } else {
        None
    };
    for rel in updates.keys() {
        if let Some(current) = current_bytes.remove(rel) {
            backups.push((agent_dir.clone(), rel.clone(), current));
        }
    }
    for rel in agent_deletions.keys() {
        if let Some(current) = current_bytes.remove(rel) {
            backups.push((agent_dir.clone(), rel.clone(), current));
        }
    }
    if let Some(root) = &skills_root {
        for rel in skill_updates.keys() {
            if let Some(current) = current_skill_bytes.remove(rel) {
                backups.push((root.clone(), rel.clone(), current));
            }
        }
        for rel in skill_deletions.keys() {
            if let Some(current) = current_skill_bytes.remove(rel) {
                backups.push((root.clone(), rel.clone(), current));
            }
        }
    }
    // Per-root lookup maps hold indices into `backups` so the backup bytes
    // exist exactly once (no duplicate O(B) copies in memory).
    let agent_backup_data: BTreeMap<String, usize> = backups
        .iter()
        .enumerate()
        .filter(|(_, (root, _, _))| root == &agent_dir)
        .map(|(index, (_, rel, _))| (rel.clone(), index))
        .collect();
    let skill_backup_data: BTreeMap<String, usize> = match &skills_root {
        Some(root) => backups
            .iter()
            .enumerate()
            .filter(|(_, (backup_root, _, _))| backup_root == root)
            .map(|(index, (_, rel, _))| (rel.clone(), index))
            .collect(),
        None => BTreeMap::new(),
    };

    let mut ctx = ApplyCtx {
        agent_dir: &agent_dir,
        skills_root: skills_root.as_deref(),
        config_path: &config_path,
        config_target: &config_target,
        manifest_path: &manifest_path,
        config_backup_index,
        original_hash,
        planned_config: merged_bytes.as_deref().or(planned_replace.as_deref()),
        candidate_action: candidate_action.as_ref(),
        candidate_backup_index,
        agent_resolved: &agent_resolved,
        owned_resolved: &owned_resolved,
        updates: &updates,
        skill_updates: &skill_updates,
        current_hashes: &current_hashes,
        current_skill_hashes: &current_skill_hashes,
        agent_source_hashes: &agent_source_hashes,
        skill_source_hashes: &skill_source_hashes,
        agent_backup_data: &agent_backup_data,
        skill_backup_data: &skill_backup_data,
        backups: &backups,
        backup_dir: &backup_dir,
        agent_deletions: &agent_deletions,
        skill_deletions: &skill_deletions,
        manifest: &mut manifest,
        written_files: Vec::new(),
        created_dirs: Vec::new(),
        created_skill_dirs: Vec::new(),
    };
    if let Err(exc) = apply(&mut ctx) {
        // Guarded rollback from the pre-mutation snapshot: undo exactly the
        // changes this run recorded in `written_files`, and only while the
        // live state still matches what the run left behind. Files the run
        // never reached keep their live bytes (their backups are never
        // restored over a concurrent edit), and anything edited or
        // recreated after our write is left untouched — such preserved
        // targets count as unrestored below, so the backup snapshot is
        // retained and the failure report stays truthful. The ownership
        // manifest is the last thing apply writes, so a manifest-write
        // failure lands here too: the pre-install manifest on disk is then
        // still the authoritative one and every target mutation is
        // recorded below, so the guarded restore brings targets and
        // manifest back in agreement.
        let backup_targets: BTreeMap<PathBuf, usize> = backups
            .iter()
            .enumerate()
            .map(|(index, (root, rel, _))| {
                // Agent files may live behind a recorded profile link:
                // rollback looks the written (resolved) target up, so the
                // backup key must be the resolved path, not agent_dir/rel.
                let target = if root == &agent_dir {
                    if rel == yamlwork::CONFIG_NAME {
                        // The config backup rolls back at the resolved
                        // target: writing at the visible path would
                        // replace a main->topology symlink with a regular
                        // file.
                        config_target.clone()
                    } else {
                        agent_resolved
                            .get(rel)
                            .cloned()
                            .or_else(|| owned_resolved.get(rel).cloned())
                            .unwrap_or_else(|| root.join(rel))
                    }
                } else {
                    root.join(rel)
                };
                (target, index)
            })
            .collect();
        // Re-derive the skills pin for rollback: apply() may have created
        // the configured skills root after the planning-time pin saw it
        // absent. The strict re-pin keeps the no-symlink-root policy; a
        // root still absent at restore time is recreated through the
        // strict open-or-create only when a restore actually publishes
        // into it (preserving the pre-migration restore behavior).
        let rollback_skills_pin = match &skills_pin {
            Some(pin) => Some(pin.clone()),
            None => match skills_root.as_deref() {
                Some(root) => match RootFd::open_strict_root(root) {
                    Ok(crate::anchored::StrictRoot::Pinned(pin)) => Some(pin),
                    _ => None,
                },
                None => None,
            },
        };
        let mut unrestored: Vec<String> = Vec::new();
        for op in &ctx.written_files {
            let undone = match op {
                WrittenOp::Write { target, hash } => {
                    let unchanged = rollback_read_target(
                        &roots,
                        &rollback_skills_pin,
                        skills_root.as_deref(),
                        target,
                    )
                    .is_some_and(|bytes| fsx::sha256_hex(&bytes) == *hash);
                    if !unchanged {
                        // Edited or removed after our write: a concurrent
                        // change this rollback must never clobber. It is
                        // also not undone, so record it as unrestored —
                        // deleting the backup now would destroy the only
                        // pre-mutation copy and falsely claim a complete
                        // rollback.
                        false
                    } else {
                        match backup_targets.get(target) {
                            Some(&index) => rollback_write_target(
                                &roots,
                                &rollback_skills_pin,
                                skills_root.as_deref(),
                                target,
                                &backups[index].2,
                                backup_modes.get(target).copied(),
                            )
                            .is_ok(),
                            None => remove_target(&roots, target).is_ok(),
                        }
                    }
                }
                WrittenOp::Delete { target } => {
                    if rollback_target_occupied(
                        &roots,
                        &rollback_skills_pin,
                        skills_root.as_deref(),
                        target,
                    ) {
                        // Recreated (or replaced by a symlink) after our
                        // delete: never clobber it, but record it as
                        // unrestored — the run's deletion is not undone
                        // and the backup must be retained.
                        false
                    } else {
                        match backup_targets.get(target) {
                            Some(&index) => rollback_write_target(
                                &roots,
                                &rollback_skills_pin,
                                skills_root.as_deref(),
                                target,
                                &backups[index].2,
                                backup_modes.get(target).copied(),
                            )
                            .is_ok(),
                            None => true,
                        }
                    }
                }
            };
            if !undone {
                unrestored.push(op.target().display().to_string());
            }
        }
        let mut directories = ctx.created_dirs.clone();
        directories.extend(ctx.created_skill_dirs.iter().cloned());
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            // Removal goes through the pin owning the directory, so a
            // swapped parent symlink cannot redirect the rmdir outside
            // the approved roots; a directory no pin covers is left in
            // place (its root's pin is absent, so the directory cannot
            // exist there either).
            if let Some(result) =
                remove_created_dir(&roots, rollback_skills_pin.as_ref(), &directory)
            {
                let _ = result;
            }
        }
        if unrestored.is_empty() {
            // Remove the whole transaction snapshot through a strict
            // re-pin of the state root: a swapped state path must not
            // redirect the removal outside the state tree, and an
            // unresolvable pin simply keeps the backup in place (it is
            // only a best-effort cleanup).
            if let crate::anchored::StrictRoot::Pinned(state_pin) =
                RootFd::open_strict_root(&state_dir).map_err(|err| io_err(&state_dir, err))?
                && let Ok(rel) = backup_dir.strip_prefix(&state_dir)
            {
                let _ = state_pin.remove_tree(&rel.to_string_lossy());
            }
            return Err(CliError::Usage(format!(
                "install failed and changes were rolled back: {exc}"
            )));
        }
        // Rollback could not restore everything: keep the backup snapshot
        // for manual recovery and say exactly what state may be left
        // behind instead of claiming a clean rollback. The retained
        // snapshot is the only pre-mutation recovery data, so mark it
        // recovery-required; ordinary retention must not delete it until
        // a later successful install clears the marker. A marker failure
        // is reported alongside, never in place of, the rollback error.
        let mut message = format!(
            "install failed: {exc}\nrollback could not restore every target; \
             these paths may still contain content written (or deleted) by \
             this run:\n  - {}\nThe ownership manifest was left unchanged \
             (still the pre-install manifest), so recorded ownership may not \
             match those targets; the pre-install backups and transaction \
             record are preserved at {} for manual recovery.",
            unrestored.join("\n  - "),
            backup_dir.display()
        );
        if let Err(marker_exc) = registry::mark_recovery_snapshot(&backup_dir, opts.install_id) {
            message.push_str(&format!(
                "\nwarning: could not mark the retained snapshot as \
                 recovery-required; ordinary retention may delete it: \
                 {marker_exc}"
            ));
        }
        return Err(CliError::Usage(message));
    }

    out.push(format!(
        "installed topology '{}' into {}",
        opts.topology,
        agent_dir.display()
    ));
    match &candidate_action {
        Some(CandidateAction::Publish(_)) => out.push(format!(
            "published {CONFIG_NEW_NAME} beside {} (active config left unchanged)",
            yamlwork::CONFIG_NAME
        )),
        Some(CandidateAction::Delete) => out.push(format!(
            "removed the previously published {CONFIG_NEW_NAME}"
        )),
        None => {}
    }
    if planned_replace.is_some() {
        out.push(format!(
            "replaced {} with the topology config; the previous config is \
             retained in the ownership manifest as a backup",
            yamlwork::CONFIG_NAME
        ));
    }
    if let Some(root) = &skills_root {
        out.push(format!("installed skills under {}", root.display()));
    }
    out.push(format!(
        "backed up pre-install state to {}",
        backup_dir.display()
    ));
    push_preservation_notes(
        out,
        false,
        &preserved_edits,
        &preserved_deletions,
        &preserved_skill_edits,
        &preserved_skill_deletions,
    );
    // Recovery markers left by earlier failed installs/releases are
    // released only now, after this install completed successfully, and
    // before ordinary pruning — marked snapshots survive until a clean
    // reconciliation clears them. Only this alias's markers (and, for the
    // legacy-manifest owner, pre-alias unscoped markers) are cleared; a
    // sibling alias's recovery snapshot stays pinned. Like pruning, a
    // clear failure must never fail the install, so it surfaces as a
    // warning line.
    let owns_legacy_manifest = manifest_path == state::manifest_path(&agent_dir, state_anchor)?;
    if let Err(exc) = registry::clear_recovery_snapshots(
        &agent_dir,
        state_anchor,
        opts.install_id,
        owns_legacy_manifest,
    ) {
        out.push(format!("warning: could not clear recovery markers: {exc}"));
    }
    // Prune old snapshots after the successful mutation only; a prune
    // failure must never roll back or fail the install, so it surfaces as
    // a warning line in the collected output.
    if let Err(exc) = registry::prune_snapshots(&agent_dir, state_anchor, registry::SNAPSHOT_KEEP) {
        out.push(format!("warning: could not prune old snapshots: {exc}"));
    }
    Ok(0)
}

fn manifest_string(manifest: &Option<Json>, field: &str, rel: &str) -> Option<String> {
    manifest
        .as_ref()?
        .get(field)?
        .as_object()?
        .get(rel)?
        .as_str()
        .map(str::to_string)
}

/// Surface owned targets whose live state diverged from the topology and
/// was therefore left untouched: user edits (kept on disk) and user
/// deletions (not recreated). These lines are informational — no write,
/// backup, or manifest entry was produced for them.
fn push_preservation_notes(
    out: &mut Vec<String>,
    dry_run: bool,
    edits: &[String],
    deletions: &[String],
    skill_edits: &[String],
    skill_deletions: &[String],
) {
    let (verb, pad) = if dry_run {
        ("would preserve", "  ")
    } else {
        ("preserved", "")
    };
    for rel in edits {
        out.push(format!("{pad}{verb}: {rel} (edited after install)"));
    }
    for rel in deletions {
        out.push(format!("{pad}{verb}: {rel} (deleted after install)"));
    }
    for rel in skill_edits {
        out.push(format!("{pad}{verb}: skills/{rel} (edited after install)"));
    }
    for rel in skill_deletions {
        out.push(format!("{pad}{verb}: skills/{rel} (deleted after install)"));
    }
}

/// One on-disk change made by this install run. Rollback undoes exactly
/// these operations, and only while the live state still matches what the
/// run left behind — a concurrent edit or recreation after our change is
/// never clobbered, and backups of files the run never reached are never
/// restored.
enum WrittenOp {
    /// A file overwritten or created; `hash` is the digest of the bytes
    /// the run wrote. Undo restores the backup (or removes the created
    /// file) only while the live bytes still hash to `hash`.
    Write { target: PathBuf, hash: String },
    /// A file deleted by the run. Undo restores the backup only while the
    /// path is still absent.
    Delete { target: PathBuf },
}

impl WrittenOp {
    fn target(&self) -> &Path {
        match self {
            WrittenOp::Write { target, .. } | WrittenOp::Delete { target } => target,
        }
    }
}

struct ApplyCtx<'a> {
    agent_dir: &'a Path,
    skills_root: Option<&'a Path>,
    config_path: &'a Path,
    /// Resolved active-config target: the plain agent-dir config path, or
    /// the profile-visible file behind a supported main->topology link.
    /// All config mutations go here so the visible symlink is never
    /// replaced with a regular file.
    config_target: &'a Path,
    manifest_path: &'a Path,
    /// Index of the pre-install config snapshot inside `backups`, when the
    /// config existed and a merge or replacement will overwrite it.
    config_backup_index: Option<usize>,
    original_hash: Option<String>,
    /// Planned active-config bytes (merge result or verbatim topology
    /// replacement); replaces the live config when set.
    planned_config: Option<&'a [u8]>,
    /// Planned candidate publication change, when candidate mode ran.
    candidate_action: Option<&'a CandidateAction>,
    /// Index of the prior candidate bytes inside `backups`, when a
    /// recorded candidate is being refreshed or withdrawn.
    candidate_backup_index: Option<usize>,
    /// Resolved write target per shipped agent relative path (plain
    /// agent-dir path, or the target behind a recorded profile link).
    agent_resolved: &'a BTreeMap<String, PathBuf>,
    /// Resolved target per still-owned agent relative path, including
    /// files the new topology dropped (used by deletions).
    owned_resolved: &'a BTreeMap<String, PathBuf>,
    updates: &'a BTreeMap<String, Vec<u8>>,
    skill_updates: &'a BTreeMap<String, Vec<u8>>,
    /// Pre-install digests of existing targets, computed once during
    /// collision planning and reused for the transaction record.
    current_hashes: &'a BTreeMap<String, String>,
    current_skill_hashes: &'a BTreeMap<String, String>,
    /// Topology source digests, computed once and reused for the manifest
    /// entries of written files.
    agent_source_hashes: &'a BTreeMap<String, String>,
    skill_source_hashes: &'a BTreeMap<String, String>,
    /// Indices into `backups`, keyed by relative path, so backup bytes are
    /// stored once.
    agent_backup_data: &'a BTreeMap<String, usize>,
    skill_backup_data: &'a BTreeMap<String, usize>,
    /// Files the new topology dropped whose live bytes still match the
    /// recorded digest, to be deleted during apply (rel → digest, used
    /// for the just-before-delete re-verification).
    agent_deletions: &'a BTreeMap<String, String>,
    skill_deletions: &'a BTreeMap<String, String>,
    backups: &'a [(PathBuf, String, Vec<u8>)],
    backup_dir: &'a Path,
    manifest: &'a mut Json,
    /// Every change this run made on disk, in the order it was made; the
    /// guarded rollback undoes exactly these.
    written_files: Vec<WrittenOp>,
    created_dirs: Vec<PathBuf>,
    created_skill_dirs: Vec<PathBuf>,
}

/// Record directories created while making `parent`, stopping at `stop_at`.
/// The actual creation happens through the pinned write/mkdir operations,
/// which descend `O_NOFOLLOW`; this only records which directories the
/// apply phase owes to the manifest (and to rollback cleanup).
fn track_created(parent: &Path, stop_at: &Path, sink: &mut Vec<PathBuf>) {
    let mut created: Vec<PathBuf> = Vec::new();
    let mut current = parent;
    while !fsx::lexists(current) && current != stop_at {
        created.push(current.to_path_buf());
        match current.parent() {
            Some(next) => current = next,
            None => break,
        }
    }
    sink.extend(created);
}

/// Pin a configured skills root with the no-symlink-root policy: a
/// symlink at the root itself fails closed before any planning or
/// mutation, instead of redirecting every later read and write. An
/// absent root pins as `None` (apply re-pins and creates it through the
/// descriptor walk when a write actually needs it).
/// Rollback read for a written target: identical dispatch to
/// `read_target`, except targets under the configured skills root go
/// through the rollback skills pin (the planning pin never covers that
/// root).
fn rollback_read_target(
    roots: &Roots,
    skills_pin: &Option<RootFd>,
    skills_root: Option<&Path>,
    target: &Path,
) -> Option<Vec<u8>> {
    let skills_rel = skills_root.and_then(|root| target.strip_prefix(root).ok());
    if let (Some(_), Some(rel)) = (skills_root, skills_rel) {
        let pin = skills_pin.clone()?;
        return read_under(Some(&pin), &rel.to_string_lossy());
    }
    read_target(roots, target)
}

/// Rollback occupancy check for a deleted target: a skills target under
/// the re-derived pin uses the pin's own probe; an unpinned skills root
/// is necessarily empty. Agent/profiles targets keep the `target_exists`
/// semantics (anything, including a dangling symlink, counts).
fn rollback_target_occupied(
    roots: &Roots,
    skills_pin: &Option<RootFd>,
    skills_root: Option<&Path>,
    target: &Path,
) -> bool {
    let skills_rel = skills_root.and_then(|root| target.strip_prefix(root).ok());
    if let (Some(_), Some(rel)) = (skills_root, skills_rel) {
        let rel = rel.to_string_lossy();
        return match skills_pin {
            Some(pin) => exists_under(Some(pin), &rel) || symlink_under(Some(pin), &rel),
            None => false,
        };
    }
    target_exists(roots, target) || target_has_symlink(roots, target)
}

/// Rollback restore for a written-or-deleted target: targets under the
/// configured skills root publish through the rollback skills pin; when
/// that root is still absent at restore time it is recreated through the
/// strict descriptor-relative open-or-create — the pre-migration
/// `fsx::atomic_write` behavior — and a symlink swapped in at the root
/// fails closed in the strict open.
fn rollback_write_target(
    roots: &Roots,
    skills_pin: &Option<RootFd>,
    skills_root: Option<&Path>,
    target: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    let skills_rel = skills_root.and_then(|root| target.strip_prefix(root).ok());
    if let (Some(root), Some(rel)) = (skills_root, skills_rel) {
        let rel = rel.to_string_lossy();
        let pin = match skills_pin {
            Some(pin) => pin.clone(),
            None => RootFd::open_or_create_private(root).map_err(|err| io_err(root, err))?,
        };
        return write_under(Some(&pin), &rel, target, data, mode);
    }
    write_target(roots, target, data, mode)
}

fn open_skills_pin(root: &Path) -> Result<Option<RootFd>, CliError> {
    match RootFd::open_strict_root(root).map_err(|err| io_err(root, err))? {
        crate::anchored::StrictRoot::Pinned(pin) => Ok(Some(pin)),
        crate::anchored::StrictRoot::Absent => Ok(None),
        crate::anchored::StrictRoot::Symlink => Err(CliError::Topology(format!(
            "refusing to continue: the skills root '{}' is a symlink",
            root.display()
        ))),
    }
}

/// Pin the descriptor-backed root a resolved target lives under and the
/// fd-relative path below it: agent-tree paths resolve through the agent
/// root, profile-canonical paths through the profiles root. `None` means
/// the target is outside every configured root — reads treat it as
/// absent, mutations refuse (a configured root is always pinned, so this
/// is unreachable for the roots the installer manages).
fn pin_for_target<'r>(roots: &'r Roots, target: &Path) -> Option<(&'r RootFd, String)> {
    if let Some(agent_root) = &roots.agent_root
        && let Ok(sub) = target.strip_prefix(&roots.agent_dir)
    {
        return Some((agent_root, sub.to_string_lossy().into_owned()));
    }
    if let Some(profiles_root) = &roots.profiles_root
        && let Ok(sub) = target.strip_prefix(profiles_root.path())
    {
        return Some((profiles_root, sub.to_string_lossy().into_owned()));
    }
    None
}

/// Read a resolved target through its pinned descriptor; `None` when the
/// file is absent, the read races a replacement, or the target is
/// outside every configured root (exactly like the plain read it
/// replaces, where any error means "treat as changed").
fn read_target(roots: &Roots, target: &Path) -> Option<Vec<u8>> {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin.read_file(&sub).ok().flatten(),
        None => None,
    }
}

/// `fsx::lexists` semantics (a dangling symlink counts as existing)
/// through the pinned descriptor; `false` outside every configured root.
fn target_exists(roots: &Roots, target: &Path) -> bool {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin.exists(&sub).unwrap_or(false),
        None => false,
    }
}

/// `fsx::has_symlink_component` through the pinned descriptor; `false`
/// outside every configured root.
fn target_has_symlink(roots: &Roots, target: &Path) -> bool {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin.has_symlink_component(&sub).unwrap_or(false),
        None => false,
    }
}

/// Permission bits of a resolved target through the pinned descriptor;
/// `None` outside every configured root.
fn target_mode(roots: &Roots, target: &Path) -> Option<u32> {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin.file_mode(&sub).ok().flatten(),
        None => None,
    }
}

/// Pinned analogue of `fsx::atomic_write`: the final component is never
/// followed (a swapped-in symlink is replaced, exactly like the rename
/// the path-based version performs). A target outside every configured
/// root is refused, never written through a path string.
fn write_target(
    roots: &Roots,
    target: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin
            .write_atomic(&sub, data, mode)
            .map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: '{}' is outside every pinned root",
            target.display()
        ))),
    }
}

/// Pinned analogue of `std::fs::remove_file` (never follows the final
/// component); refuses a target outside every configured root.
fn remove_target(roots: &Roots, target: &Path) -> Result<(), CliError> {
    match pin_for_target(roots, target) {
        Some((pin, sub)) => pin.remove_file(&sub).map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: '{}' is outside every pinned root",
            target.display()
        ))),
    }
}

/// Read a skills-root-relative target through the once-pinned skills
/// root; `None` semantics match [`read_target`]. An absent pin (the
/// skills root does not exist) means the target is absent.
fn read_under(pin: Option<&RootFd>, rel: &str) -> Option<Vec<u8>> {
    match pin {
        Some(pin) => pin.read_file(rel).ok().flatten(),
        None => None,
    }
}

/// [`target_exists`] for skills-root-relative targets.
fn exists_under(pin: Option<&RootFd>, rel: &str) -> bool {
    match pin {
        Some(pin) => pin.exists(rel).unwrap_or(false),
        None => false,
    }
}

/// [`target_has_symlink`] for skills-root-relative targets.
fn symlink_under(pin: Option<&RootFd>, rel: &str) -> bool {
    match pin {
        Some(pin) => pin.has_symlink_component(rel).unwrap_or(false),
        None => false,
    }
}

/// [`write_target`] for skills-root-relative targets; refuses when the
/// skills root is not pinned (apply pins and, when needed, creates it
/// through the descriptor walk before any skills write). `mode` is the
/// backed-up permission set a rollback restores instead of the
/// restrictive atomic-write default.
fn write_under(
    pin: Option<&RootFd>,
    rel: &str,
    target: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    match pin {
        Some(pin) => pin
            .write_atomic(rel, data, mode)
            .map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: the skills root for '{}' is not pinned",
            target.display()
        ))),
    }
}

/// [`remove_target`] for skills-root-relative targets; refuses when the
/// skills root is not pinned.
fn remove_under(pin: Option<&RootFd>, rel: &str, target: &Path) -> Result<(), CliError> {
    match pin {
        Some(pin) => pin.remove_file(rel).map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: the skills root for '{}' is not pinned",
            target.display()
        ))),
    }
}

/// Best-effort removal of a directory recorded during apply, through
/// the pin owning its root. Returns `None` when no pin covers
/// `directory` (the directory stays in place; a configured root whose
/// pin is absent cannot contain the directory) and `Some(result)` when
/// a descriptor-based `rmdir` was attempted.
fn remove_created_dir(
    roots: &symlinks::Roots,
    skills_pin: Option<&RootFd>,
    directory: &Path,
) -> Option<std::io::Result<()>> {
    if let Some(pin) = skills_pin
        && let Ok(rel) = directory.strip_prefix(pin.path())
    {
        return Some(pin.try_remove_empty_dir(rel.to_string_lossy().as_ref()));
    }
    directory
        .strip_prefix(&roots.agent_dir)
        .ok()
        .map(|rel| match roots.agent_root.as_ref() {
            Some(pin) => pin.try_remove_empty_dir(rel.to_string_lossy().as_ref()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "agent root is not pinned",
            )),
        })
}

fn apply(ctx: &mut ApplyCtx<'_>) -> Result<(), CliError> {
    // Descriptor pins for the whole apply phase: the agent tree (and the
    // profiles tree behind supported links) were re-pinned by run(), the
    // state root is pinned here, and every re-verification, write, and
    // delete below walks these pinned descriptors.
    let roots = symlinks::Roots::open(ctx.agent_dir)?;
    let state_dir = ctx
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .expect("manifest path sits below the state root");
    // The state root lives under the XDG state directory, not the agent
    // tree: pin it directly with the strict no-symlink-root create.
    let state_pin =
        RootFd::open_or_create_private(state_dir).map_err(|err| io_err(state_dir, err))?;
    let backup_dir_rel = ctx
        .backup_dir
        .strip_prefix(state_dir)
        .expect("backup dir sits below the state root")
        .to_string_lossy()
        .into_owned();
    // A timestamped private transaction snapshot exists for every mutating
    // install, even when there was nothing to back up.
    state_pin
        .makedirs(&backup_dir_rel, 0o700, true)
        .map_err(|err| io_err(ctx.backup_dir, err))?;
    for (root, rel, data) in ctx.backups {
        let backup_rel = if root == ctx.agent_dir {
            rel.clone()
        } else {
            format!("skills-root/{rel}")
        };
        let backup_file_rel = format!("{backup_dir_rel}/{backup_rel}");
        state_pin
            .write_private(&backup_file_rel, data)
            .map_err(|err| io_err(&ctx.backup_dir.join(backup_rel), err))?;
    }
    // The transaction record notes every path the install touched,
    // explicitly including absent ones (e.g. a config.yml that did not
    // exist yet), so a later audit can tell what was there before.
    let mut transaction = Map::new();
    transaction.insert(
        yamlwork::CONFIG_NAME.to_string(),
        match &ctx.original_hash {
            None => json!({ "absent": true }),
            Some(hash) => json!({ "sha256": hash }),
        },
    );
    for rel in ctx.updates.keys() {
        transaction.insert(
            rel.clone(),
            match ctx.current_hashes.get(rel) {
                None => json!({ "absent": true }),
                Some(hash) => json!({ "sha256": hash }),
            },
        );
    }
    for (rel, recorded) in ctx.agent_deletions {
        transaction.insert(rel.clone(), json!({ "deleted_sha256": recorded }));
    }
    if let Some(root) = ctx.skills_root {
        transaction.insert(
            "skills_root".to_string(),
            Json::String(root.display().to_string()),
        );
        let mut skills = Map::new();
        for rel in ctx.skill_updates.keys() {
            skills.insert(
                rel.clone(),
                match ctx.current_skill_hashes.get(rel) {
                    None => json!({ "absent": true }),
                    Some(hash) => json!({ "sha256": hash }),
                },
            );
        }
        for (rel, recorded) in ctx.skill_deletions {
            skills.insert(rel.clone(), json!({ "deleted_sha256": recorded }));
        }
        transaction.insert("skills".to_string(), Json::Object(skills));
    }
    let mut snapshot = serde_json::to_string_pretty(&Json::Object(transaction))
        .expect("transaction serialization cannot fail");
    snapshot.push('\n');
    let snapshot_rel = format!("{backup_dir_rel}/snapshot.json");
    state_pin
        .write_private(&snapshot_rel, snapshot.as_bytes())
        .map_err(|err| io_err(&ctx.backup_dir.join("snapshot.json"), err))?;

    // The skills root may not have existed at planning time; pin (and,
    // only when a write actually needs it, create) it here through the
    // descriptor walk — never through a path-based `create_dir_all`.
    let skills_pin = match ctx.skills_root {
        Some(root) => Some(RootFd::open_or_create_private(root).map_err(|err| io_err(root, err))?),
        None => None,
    };
    for (rel, data) in ctx.updates {
        // The resolved target is the profile-visible path when the file
        // ships behind a recorded main->topology link.
        let target = ctx
            .agent_resolved
            .get(rel)
            .cloned()
            .unwrap_or_else(|| ctx.agent_dir.join(rel));
        if let Some(&index) = ctx.agent_backup_data.get(rel) {
            let backup = &ctx.backups[index].2;
            let unchanged = read_target(&roots, &target)
                .is_some_and(|bytes| bytes.as_slice() == backup.as_slice());
            if !unchanged {
                return Err(CliError::Usage(format!(
                    "{rel} changed while the install was being planned; aborting"
                )));
            }
        } else if target_exists(&roots, &target) || target_has_symlink(&roots, &target) {
            return Err(CliError::Usage(format!(
                "{rel} appeared (or a path component became a symlink) while \
                 the install was being planned; aborting"
            )));
        }
        if !target.parent().is_some_and(|parent| parent.exists())
            && let Some(parent) = target.parent()
        {
            track_created(parent, ctx.agent_dir, &mut ctx.created_dirs);
        }
        write_target(&roots, &target, data, None)?;
        ctx.written_files.push(WrittenOp::Write {
            target: target.clone(),
            hash: ctx.agent_source_hashes[rel].clone(),
        });
        ctx.manifest["files"]
            .as_object_mut()
            .expect("manifest files is an object")
            .insert(
                rel.clone(),
                Json::String(ctx.agent_source_hashes[rel].clone()),
            );
    }
    // Delete files the new topology dropped; re-verify the exact installed
    // bytes just before removing so a concurrent edit aborts instead of
    // being destroyed.
    for (rel, recorded) in ctx.agent_deletions {
        let target = ctx
            .owned_resolved
            .get(rel)
            .cloned()
            .unwrap_or_else(|| ctx.agent_dir.join(rel));
        let unchanged =
            read_target(&roots, &target).is_some_and(|bytes| fsx::sha256_hex(&bytes) == *recorded);
        if !unchanged {
            return Err(CliError::Usage(format!(
                "{rel} changed while the install was being planned; aborting"
            )));
        }
        remove_target(&roots, &target)?;
        ctx.written_files.push(WrittenOp::Delete { target });
    }
    if let Some(root) = ctx.skills_root {
        for (rel, data) in ctx.skill_updates {
            let target = root.join(rel);
            if let Some(&index) = ctx.skill_backup_data.get(rel) {
                let backup = &ctx.backups[index].2;
                let unchanged = read_under(skills_pin.as_ref(), rel)
                    .is_some_and(|bytes| bytes.as_slice() == backup.as_slice());
                if !unchanged {
                    return Err(CliError::Usage(format!(
                        "{rel} under skills root '{}' changed while the \
                         install was being planned; aborting",
                        root.display()
                    )));
                }
            } else if exists_under(skills_pin.as_ref(), rel)
                || symlink_under(skills_pin.as_ref(), rel)
            {
                return Err(CliError::Usage(format!(
                    "{rel} appeared (or a path component became a symlink) \
                     under skills root '{}' while the install was being \
                     planned; aborting",
                    root.display()
                )));
            }
            if !target.parent().is_some_and(|parent| parent.exists())
                && let Some(parent) = target.parent()
            {
                track_created(parent, root, &mut ctx.created_skill_dirs);
            }
            write_under(skills_pin.as_ref(), rel, &target, data, None)?;
            ctx.written_files.push(WrittenOp::Write {
                target: target.clone(),
                hash: ctx.skill_source_hashes[rel].clone(),
            });
            ctx.manifest["skills_files"]
                .as_object_mut()
                .expect("manifest skills_files is an object")
                .insert(
                    rel.clone(),
                    Json::String(ctx.skill_source_hashes[rel].clone()),
                );
        }
        for (rel, recorded) in ctx.skill_deletions {
            let target = root.join(rel);
            let unchanged = read_under(skills_pin.as_ref(), rel)
                .is_some_and(|bytes| fsx::sha256_hex(&bytes) == *recorded);
            if !unchanged {
                return Err(CliError::Usage(format!(
                    "{rel} under skills root '{}' changed while the \
                     install was being planned; aborting",
                    root.display()
                )));
            }
            remove_under(skills_pin.as_ref(), rel, &target)?;
            ctx.written_files.push(WrittenOp::Delete { target });
        }
    }
    if let Some(planned) = ctx.planned_config {
        let original = ctx
            .config_backup_index
            .map(|index| ctx.backups[index].2.as_slice());
        if read_target(&roots, ctx.config_target).as_deref() != original {
            return Err(CliError::Usage(format!(
                "'{}' changed during install; aborting without clobbering it",
                ctx.config_path.display()
            )));
        }
        let mode = target_mode(&roots, ctx.config_target).or(Some(0o600));
        // The write goes to the resolved target: a supported main->profile
        // symlink at the visible config path is kept intact, never
        // replaced with a regular file.
        write_target(&roots, ctx.config_target, planned, mode)?;
        // Track the config like any other written file so rollback restores
        // it from the backup taken above (or removes it when this run
        // created it) — but only while the live bytes still match what we
        // wrote.
        ctx.written_files.push(WrittenOp::Write {
            target: ctx.config_target.to_path_buf(),
            hash: fsx::sha256_hex(planned),
        });
    }
    // Publish or withdraw the candidate config beside the untouched active
    // config. The candidate path is always created plain (never through a
    // link), and refreshing a recorded candidate re-verifies the backed-up
    // bytes first so a concurrent edit aborts instead of being clobbered.
    if let Some(action) = ctx.candidate_action {
        let candidate_path = ctx.agent_dir.join(CONFIG_NEW_NAME);
        match action {
            CandidateAction::Publish(bytes) => {
                if let Some(index) = ctx.candidate_backup_index {
                    let backup = &ctx.backups[index].2;
                    let unchanged = read_target(&roots, &candidate_path)
                        .is_some_and(|live| live.as_slice() == backup.as_slice());
                    if !unchanged {
                        return Err(CliError::Usage(format!(
                            "'{}' changed while the install was being planned; aborting",
                            candidate_path.display()
                        )));
                    }
                } else if target_exists(&roots, &candidate_path) {
                    return Err(CliError::Usage(format!(
                        "'{}' appeared while the install was being planned; aborting",
                        candidate_path.display()
                    )));
                }
                write_target(&roots, &candidate_path, bytes, None)?;
                ctx.written_files.push(WrittenOp::Write {
                    target: candidate_path.clone(),
                    hash: fsx::sha256_hex(bytes),
                });
            }
            CandidateAction::Delete => {
                if target_exists(&roots, &candidate_path) {
                    if target_has_symlink(&roots, &candidate_path) {
                        // A swapped-in link (or link parent) is a
                        // concurrent change; the candidate path is always
                        // created plain, so a symlink here means the file
                        // was replaced after planning.
                        return Err(CliError::Usage(format!(
                            "'{}' changed while the install was being planned; aborting",
                            candidate_path.display()
                        )));
                    }
                    let Some(index) = ctx.candidate_backup_index else {
                        return Err(CliError::Usage(format!(
                            "'{}' appeared while the install was being planned; aborting",
                            candidate_path.display()
                        )));
                    };
                    // Re-verify the live bytes against the planning
                    // snapshot immediately before unlink so a concurrent
                    // edit is preserved, never deleted.
                    let backup = &ctx.backups[index].2;
                    let unchanged = read_target(&roots, &candidate_path)
                        .is_some_and(|live| live.as_slice() == backup.as_slice());
                    if !unchanged {
                        return Err(CliError::Usage(format!(
                            "'{}' changed while the install was being planned; aborting",
                            candidate_path.display()
                        )));
                    }
                    remove_target(&roots, &candidate_path)?;
                    ctx.written_files.push(WrittenOp::Delete {
                        target: candidate_path,
                    });
                }
            }
        }
    }
    // Record directories this install created for later pruning on remove.
    let mut new_dirs: BTreeSet<String> = ctx
        .manifest
        .get("created_dirs")
        .and_then(Json::as_array)
        .map(|dirs| {
            dirs.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for directory in &ctx.created_dirs {
        if let Ok(rel) = directory.strip_prefix(ctx.agent_dir) {
            new_dirs.insert(rel.to_string_lossy().into_owned());
        }
    }
    ctx.manifest["created_dirs"] = Json::Array(
        new_dirs
            .iter()
            .map(|dir| Json::String(dir.clone()))
            .collect(),
    );
    if ctx.skills_root.is_some() {
        let mut new_skill_dirs: BTreeSet<String> = ctx
            .manifest
            .get("skills_created_dirs")
            .and_then(Json::as_array)
            .map(|dirs| {
                dirs.iter()
                    .filter_map(Json::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for directory in &ctx.created_skill_dirs {
            if let Some(root) = ctx.skills_root
                && let Ok(rel) = directory.strip_prefix(root)
            {
                new_skill_dirs.insert(rel.to_string_lossy().into_owned());
            }
        }
        ctx.manifest["skills_created_dirs"] = Json::Array(
            new_skill_dirs
                .iter()
                .map(|dir| Json::String(dir.clone()))
                .collect(),
        );
    }
    let manifest_bytes = state::serialize_manifest(ctx.manifest);
    let manifest_rel = ctx
        .manifest_path
        .strip_prefix(state_dir)
        .map_err(|_| {
            io_err(
                ctx.manifest_path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "ownership manifest path is not under the pinned state root",
                ),
            )
        })?
        .to_string_lossy()
        .into_owned();
    state_pin
        .write_private(&manifest_rel, &manifest_bytes)
        .map_err(|err| io_err(ctx.manifest_path, err))?;
    Ok(())
}

/// Set `parts` to `value` in the predicted model tree, creating maps.
/// Mirrors remove's helper so update-time reconciliation predicts the
/// same result tree remove would.
fn set_predicted(model: &mut Value, parts: &[String], value: Value) {
    if parts.is_empty() {
        *model = value;
        return;
    }
    if !matches!(model, Value::Map(_)) {
        *model = Value::Map(BTreeMap::new());
    }
    let Value::Map(map) = model else {
        unreachable!("just replaced with a map");
    };
    let entry = map
        .entry(parts[0].clone())
        .or_insert_with(|| Value::Map(BTreeMap::new()));
    set_predicted(entry, &parts[1..], value);
}

/// Delete `parts` from the predicted model tree when every step is a map.
fn delete_predicted(model: &mut Value, parts: &[String]) {
    if parts.is_empty() {
        return;
    }
    let Value::Map(map) = model else {
        return;
    };
    if parts.len() == 1 {
        map.remove(&parts[0]);
        return;
    }
    if let Some(child) = map.get_mut(&parts[0]) {
        delete_predicted(child, &parts[1..]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is before UNIX epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "omp-install-dropped-link-rollback-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("failed to create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn apply_failure_restores_dropped_file_behind_profile_directory_link() {
        let temp = TestDir::new();
        let home = temp.0.join("home");
        let omp_home = home.join(".omp");
        let profile_name = "rollback-profile";
        let agent_dir = omp_home.join("agent");
        let linked_target = omp_home
            .join("profiles")
            .join(profile_name)
            .join("agent")
            .join("sessions");
        std::fs::create_dir_all(&agent_dir).expect("failed to create main agent directory");
        std::fs::create_dir_all(&linked_target).expect("failed to create profile target");

        let visible_link = agent_dir.join("sessions");
        symlink(&linked_target, &visible_link).expect("failed to create profile directory link");
        let dropped_target = linked_target.join("dropped.md");
        let dropped_bytes = b"owned profile content\n";
        std::fs::write(&dropped_target, dropped_bytes).expect("failed to seed dropped target");
        let sibling = linked_target.join("unrelated.md");
        let sibling_bytes = b"user-owned sibling\n";
        std::fs::write(&sibling, sibling_bytes).expect("failed to seed unrelated sibling");

        let config_path = agent_dir.join(yamlwork::CONFIG_NAME);
        let original_config = b"setting: old\n";
        std::fs::write(&config_path, original_config).expect("failed to seed active config");

        let topology_root = temp.0.join("source");
        let topology_dir = topology_root.join("topologies").join(profile_name);
        let topology_agent = topology_dir.join("agent");
        std::fs::create_dir_all(&topology_agent).expect("failed to create topology fixture");
        std::fs::write(
            topology_dir.join("topology.yml"),
            format!(
                "name: {profile_name}\nversion: 1.0.0\ndescription: rollback fixture\ninstallable: true\n"
            ),
        )
        .expect("failed to write topology metadata");
        std::fs::write(
            topology_agent.join(yamlwork::CONFIG_NAME),
            b"setting: new\n",
        )
        .expect("failed to write changed topology config");

        let state_home = temp.0.join("state");
        let identity = symlinks::LinkIdentity {
            link: "sessions".to_string(),
            target: linked_target.clone(),
            canonical: std::fs::canonicalize(&linked_target)
                .expect("failed to canonicalize profile target"),
        };
        let existing_manifest = json!({
            "version": state::MANIFEST_VERSION,
            "topology": profile_name,
            "topology_version": "1.0.0",
            "files": {
                yamlwork::CONFIG_NAME: fsx::sha256_hex(original_config),
                "sessions/dropped.md": fsx::sha256_hex(dropped_bytes),
            },
            "created_dirs": [],
            "config_created": false,
            "symlinks": {
                "sessions/dropped.md": symlinks::identity_json(&identity),
            },
        });
        state::with_state_home_for_test(&state_home, || {
            let manifest_path = state::manifest_path(&agent_dir, &topology_root)
                .expect("failed to resolve manifest");
            std::fs::create_dir_all(manifest_path.parent().expect("manifest path has no parent"))
                .expect("failed to create state directory");
            std::fs::write(
                &manifest_path,
                state::serialize_manifest(&existing_manifest),
            )
            .expect("failed to seed ownership manifest");
        });

        let agent_dir_arg = agent_dir.to_str().expect("agent path is not UTF-8");
        let options = InstallOptions {
            topology: profile_name,
            topology_root: &topology_root,
            state_anchor: &topology_root,
            agent_dir: Some(agent_dir_arg),
            skills_dir: None,
            dry_run: false,
            config_mode: ConfigMode::Replace,
            manifest_path: None,
            install_id: None,
            expected_version: None,
        };
        let mut output = Vec::new();
        let result = state::with_state_home_for_test(&state_home, || {
            symlinks::with_home_for_test(&home, || run(&options, &mut output))
        });

        let error = result.expect_err("install should fail after deleting the dropped files");
        assert!(
            matches!(&error, CliError::Usage(message) if message.contains("changed during install")),
            "unexpected install failure: {error}"
        );
        assert_eq!(
            std::fs::read(&dropped_target).expect("rollback failed to restore dropped target"),
            dropped_bytes,
            "rollback did not restore the dropped profile target"
        );
        assert!(
            visible_link.is_symlink(),
            "rollback replaced the visible profile directory symlink"
        );
        assert_eq!(
            std::fs::read_link(&visible_link).expect("visible profile symlink disappeared"),
            linked_target
        );
        assert_eq!(
            std::fs::read(&sibling).expect("rollback removed the unrelated sibling"),
            sibling_bytes,
            "rollback changed an unrelated file in the symlink target"
        );
        assert_eq!(
            std::fs::read(&config_path).expect("rollback failed to restore active config"),
            original_config
        );
    }

    #[test]
    fn apply_delete_preserves_candidate_edited_after_planning() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before UNIX epoch")
            .as_nanos();
        let temp = std::env::temp_dir().join(format!(
            "omp-candidate-delete-{}-{unique}",
            std::process::id()
        ));
        let agent_dir = temp.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("failed to create agent directory");

        let candidate_path = agent_dir.join(CONFIG_NEW_NAME);
        let planned_bytes = b"candidate as planned\n";
        let edited_bytes = b"user edit after planning\n";
        std::fs::write(&candidate_path, edited_bytes).expect("failed to write edited candidate");
        let backups = vec![(
            agent_dir.clone(),
            CONFIG_NEW_NAME.to_string(),
            planned_bytes.to_vec(),
        )];
        let candidate_action = CandidateAction::Delete;
        let empty_paths: BTreeMap<String, PathBuf> = BTreeMap::new();
        let empty_updates: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let empty_indices: BTreeMap<String, usize> = BTreeMap::new();
        let empty_hashes: BTreeMap<String, String> = BTreeMap::new();
        let mut manifest = json!({});
        let config_path = agent_dir.join(yamlwork::CONFIG_NAME);
        let manifest_path = temp.join("manifest.json");
        let backup_dir = temp.join("transaction");
        let mut ctx = ApplyCtx {
            agent_dir: &agent_dir,
            skills_root: None,
            config_path: &config_path,
            config_target: &config_path,
            manifest_path: &manifest_path,
            config_backup_index: None,
            original_hash: None,
            planned_config: None,
            candidate_action: Some(&candidate_action),
            candidate_backup_index: Some(0),
            agent_resolved: &empty_paths,
            owned_resolved: &empty_paths,
            updates: &empty_updates,
            skill_updates: &empty_updates,
            current_hashes: &empty_hashes,
            current_skill_hashes: &empty_hashes,
            agent_source_hashes: &empty_hashes,
            skill_source_hashes: &empty_hashes,
            agent_backup_data: &empty_indices,
            skill_backup_data: &empty_indices,
            agent_deletions: &empty_hashes,
            skill_deletions: &empty_hashes,
            backups: &backups,
            backup_dir: &backup_dir,
            manifest: &mut manifest,
            written_files: Vec::new(),
            created_dirs: Vec::new(),
            created_skill_dirs: Vec::new(),
        };

        let error = apply(&mut ctx).expect_err("delete should abort after candidate bytes changed");
        assert!(
            matches!(
                &error,
                CliError::Usage(message)
                    if message
                        == &format!(
                            "'{}' changed while the install was being planned; aborting",
                            candidate_path.display()
                        )
            ),
            "unexpected error for changed candidate: {error}"
        );
        assert_eq!(
            std::fs::read(&candidate_path).expect("candidate disappeared after aborted delete"),
            edited_bytes,
            "apply deleted or changed the user's candidate edit"
        );
        std::fs::remove_dir_all(&temp).expect("failed to remove test directory");
    }

    #[test]
    fn rollback_write_target_recreates_absent_skills_root_and_rejects_symlink() {
        let temp = TestDir::new();
        let home = temp.0.join("home");
        let agent_dir = home.join(".omp/agent");
        std::fs::create_dir_all(&agent_dir).expect("failed to create agent directory");
        let roots = symlinks::with_home_for_test(&home, || {
            Roots::open(&agent_dir).expect("failed to pin test roots")
        });

        let skills_root = temp.0.join("skills");
        assert!(!skills_root.exists(), "skills root exists before rollback");
        let target = skills_root.join("nested/skill.md");
        let restored_bytes = b"restored skill bytes\n";
        let skills_pin = None;
        rollback_write_target(
            &roots,
            &skills_pin,
            Some(&skills_root),
            &target,
            restored_bytes,
            None,
        )
        .expect("rollback failed to recreate the absent skills root");

        assert!(target.is_file(), "rollback did not restore the skill path");
        assert_eq!(
            std::fs::read(&target).expect("failed to read restored skill"),
            restored_bytes,
            "rollback restored incorrect skill bytes"
        );

        let outside_root = temp.0.join("outside");
        std::fs::create_dir_all(&outside_root).expect("failed to create outside directory");
        let outside_skill = outside_root.join("skill.md");
        let outside_bytes = b"outside content\n";
        std::fs::write(&outside_skill, outside_bytes).expect("failed to seed outside skill");
        let linked_skills_root = temp.0.join("linked-skills");
        symlink(&outside_root, &linked_skills_root).expect("failed to create skills-root symlink");
        let linked_target = linked_skills_root.join("skill.md");

        assert!(
            rollback_write_target(
                &roots,
                &skills_pin,
                Some(&linked_skills_root),
                &linked_target,
                b"must not overwrite outside content\n",
                None,
            )
            .is_err(),
            "rollback accepted a symlinked skills root"
        );
        assert_eq!(
            std::fs::read(&outside_skill).expect("outside skill disappeared"),
            outside_bytes,
            "rollback wrote through the symlinked skills root"
        );
    }
}
