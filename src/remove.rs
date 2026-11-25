//! Remove command: edit-aware, retryable uninstallation.
//!
//! Files the user edited after install are preserved and reported; config
//! keys the user changed are left alone; everything else is restored or
//! deleted, with a timestamped snapshot taken before any mutation and
//! rollback on failure. A `config.yml` the installer created from an
//! absent file is deleted outright once every topology-owned key is
//! resolved away and no other content remains. When anything is
//! preserved, the removal is still successful (exit 0) but the ownership
//! manifest keeps the unresolved state so a later retry can finish the
//! job.
//!
//! Complexity: planning reads each tracked file once, O(F) for F tracked
//! files; config planning is O(K * D) for K installed keys and path depth
//! D on an already-parsed model; the restore is a targeted CST edit per
//! resolved key.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value as Json};

use crate::anchored::{EntryKind, RootFd};
use crate::error::{CliError, io_err};
use crate::fsx;
use crate::install::{CONFIG_NEW_NAME, resolve_agent_dir};
use crate::registry;
use crate::state;
use crate::symlinks;
use crate::topology;
use crate::value::{self, Value};
use crate::yamlwork;

pub struct RemoveOptions<'a> {
    pub topology: &'a str,
    /// Stable XDG state anchor for manifest/state paths, independent of
    /// topology checkout availability; removal never needs a checkout.
    pub state_anchor: &'a Path,
    pub agent_dir: Option<&'a str>,
    pub skills_dir: Option<&'a str>,
    pub dry_run: bool,
    /// Ownership manifest this removal owns (a per-alias manifest when
    /// several aliases share the agent directory). `None` uses the
    /// legacy default `manifest.json`.
    pub manifest_path: Option<&'a Path>,
    /// Alias being removed, when invoked through one. Scopes recovery
    /// markers so a sibling alias's rollback snapshot is never cleared.
    pub alias: Option<&'a str>,
}

type PathKey = Vec<String>;

/// Per-target outcome of a guarded snapshot rollback: `restored` targets
/// were returned to their pre-remove bytes, `preserved` targets were
/// intentionally left alone because a concurrent change now occupies the
/// path (restoring would clobber it), and `failed` targets could not be
/// restored at all.
struct RollbackReport {
    restored: Vec<String>,
    preserved: Vec<String>,
    failed: Vec<String>,
}

impl RollbackReport {
    /// A rollback is complete only when nothing was skipped or failed; any
    /// `preserved` or `failed` entry means the pre-remove state was only
    /// partially recovered and the recovery snapshot must be kept.
    fn is_complete(&self) -> bool {
        self.preserved.is_empty() && self.failed.is_empty()
    }
}

/// Explicit error for an incomplete rollback: names the affected paths by
/// category, the manifest state, and the kept recovery snapshot. The
/// snapshot dir is deliberately NOT removed by this function. Because the
/// retained snapshot is the only pre-mutation recovery data, it is marked
/// recovery-required before the error is built so ordinary retention
/// cannot delete it until a later successful reconciliation clears the
/// marker; a marker failure is reported alongside, never in place of, the
/// rollback error.
fn incomplete_rollback_error(
    context: &str,
    manifest_state: &str,
    snapshot_dir: &Path,
    report: &RollbackReport,
    alias: Option<&str>,
) -> CliError {
    let mut msg = format!(
        "{context}; rollback is INCOMPLETE — the recovery snapshot at {} \
         was kept",
        snapshot_dir.display()
    );
    if !report.preserved.is_empty() {
        msg.push_str(&format!(
            "\n  left untouched (concurrent change kept): {}",
            report.preserved.join(", ")
        ));
    }
    if !report.failed.is_empty() {
        msg.push_str(&format!(
            "\n  could not be restored: {}",
            report.failed.join(", ")
        ));
    }
    msg.push_str(&format!("\n  manifest state: {manifest_state}"));
    msg.push_str("; restore the listed paths from the snapshot, then retry remove");
    if let Err(marker_exc) = registry::mark_recovery_snapshot(snapshot_dir, alias) {
        msg.push_str(&format!(
            "\nwarning: could not mark the retained snapshot as \
             recovery-required; ordinary retention may delete it: \
             {marker_exc}"
        ));
    }
    CliError::Usage(msg)
}

/// Manifest `files`-style map validation: every key/value must be a safe
/// relative path and a SHA-256 hex string.
fn validated_file_map(manifest: &Json, field: &str) -> Result<BTreeMap<String, String>, CliError> {
    let Some(raw) = manifest.get(field) else {
        return Ok(BTreeMap::new());
    };
    let Some(map) = raw.as_object() else {
        return Err(CliError::Manifest(format!(
            "ownership manifest '{field}' is malformed"
        )));
    };
    let mut files = BTreeMap::new();
    for (rel, recorded) in map {
        let Some(recorded) = recorded.as_str() else {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{field}' is malformed"
            )));
        };
        if !fsx::safe_rel(rel) {
            return Err(CliError::Manifest(
                "ownership manifest contains unsafe paths; refusing to act on it".to_string(),
            ));
        }
        files.insert(rel.clone(), recorded.to_string());
    }
    Ok(files)
}

fn validated_dir_list(manifest: &Json, field: &str) -> Result<Vec<String>, CliError> {
    let Some(raw) = manifest.get(field) else {
        return Ok(Vec::new());
    };
    let Some(list) = raw.as_array() else {
        return Err(CliError::Manifest(format!(
            "ownership manifest '{field}' is malformed"
        )));
    };
    let mut dirs = Vec::new();
    for entry in list {
        let Some(dir) = entry.as_str() else {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{field}' is malformed"
            )));
        };
        if !fsx::safe_rel(dir) {
            return Err(CliError::Manifest(
                "ownership manifest contains unsafe paths; refusing to act on it".to_string(),
            ));
        }
        dirs.push(dir.to_string());
    }
    Ok(dirs)
}

/// A recorded skills root is only ever used to locate files recorded in
/// the manifest, so it must be an absolute path made purely of root and
/// normal components: anything relative or containing `.`/`..` could
/// redirect deletions outside the recorded roots.
fn valid_skills_root(raw: &str) -> bool {
    let path = Path::new(raw);
    path.is_absolute()
        && !raw.contains('\\')
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

pub fn run(opts: &RemoveOptions, out: &mut Vec<String>) -> Result<i32, CliError> {
    let state_anchor = opts.state_anchor;
    let agent_dir = resolve_agent_dir(opts.agent_dir)?;
    // The ownership manifest this removal owns: a per-alias manifest when
    // several aliases share the agent directory, else the legacy default.
    let manifest_path = match opts.manifest_path {
        Some(path) => path.to_path_buf(),
        None => state::manifest_path(&agent_dir, state_anchor)?,
    };
    // Serialize against other state-mutating commands before reading the
    // ownership manifest: a remove that planned from a manifest another
    // remove already deleted would resurrect a stale manifest (or roll
    // back a completed removal) at commit time. Holding the same lock
    // update takes across its reinstall also keeps remove from
    // interleaving with an in-flight update of the same target. Dry-run
    // is read-only and takes no lock.
    let _lock = if opts.dry_run {
        None
    } else {
        Some(registry::RegistryLock::acquire(state_anchor)?)
    };
    let manifest = match state::load_manifest_at(&manifest_path, state_anchor)? {
        Some(manifest) => manifest,
        None => {
            if opts.dry_run {
                out.push(format!(
                    "dry-run: topology '{}' is not installed in {}; nothing to remove",
                    opts.topology,
                    agent_dir.display()
                ));
                return Ok(0);
            }
            return Err(CliError::Usage(format!(
                "topology '{}' is not installed in {} (no ownership \
                 manifest found)",
                opts.topology,
                agent_dir.display()
            )));
        }
    };
    if topology::manifest_topology(&manifest) != Some(opts.topology) {
        let installed = topology::manifest_topology(&manifest).unwrap_or("unknown");
        return Err(CliError::Usage(format!(
            "{} has '{installed}' installed, not '{}'",
            agent_dir.display(),
            opts.topology
        )));
    }

    // Ownership recorded by sibling aliases of this agent directory:
    // paths and config keys they still own are released from this
    // manifest without being deleted or restored.
    let cross_alias = registry::cross_alias_ownership(state_anchor, &agent_dir, &manifest_path)?;

    let mut manifest = manifest;
    let owned_files = validated_file_map(&manifest, "files")?;
    // Files flagged pre-existing reached their target through a
    // user-made main->topology link before install: ownership was
    // adopted, never created, so remove releases them from the manifest
    // without touching the file or the link — whether the file is still
    // present, was edited, or already went missing. They are not
    // conflicts and never count toward a partial removal.
    let preexisting: BTreeSet<String> = manifest
        .get("preexisting")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let created_dirs = validated_dir_list(&manifest, "created_dirs")?;
    let skill_files = validated_file_map(&manifest, "skills_files")?;
    let skill_dirs = validated_dir_list(&manifest, "skills_created_dirs")?;
    // Split-root bookkeeping (absent in v0.1 single-root manifests, which
    // keep their skills under <agent-dir>/skills inside 'files'). A present
    // 'skills_dir' must be a well-formed absolute path: a malformed or
    // relative value could redirect deletions outside the recorded root.
    let recorded_skills_dir: Option<String> = match manifest.get("skills_dir") {
        Some(raw) => {
            let Some(dir) = raw.as_str() else {
                return Err(CliError::Manifest(
                    "ownership manifest 'skills_dir' is malformed; refusing \
                     to act on it"
                        .to_string(),
                ));
            };
            if !valid_skills_root(dir) {
                return Err(CliError::Manifest(format!(
                    "ownership manifest 'skills_dir' '{dir}' is malformed \
                     (must be an absolute path without '.' or '..' \
                     components); refusing to act on it"
                )));
            }
            Some(dir.to_string())
        }
        None => None,
    };
    // Split-root entries without a recorded root cannot be safely located;
    // fail closed before reading or deleting anything.
    if recorded_skills_dir.is_none() && (!skill_files.is_empty() || !skill_dirs.is_empty()) {
        return Err(CliError::Manifest(
            "ownership manifest records split-root skill files/directories \
             but no 'skills_dir' root; refusing to act on it"
                .to_string(),
        ));
    }
    if let Some(raw) = opts.skills_dir {
        let requested = fsx::resolve_loose(&fsx::expanduser(Path::new(raw)));
        if recorded_skills_dir.is_none() {
            return Err(CliError::Usage(
                "refusing to remove: the recorded installation keeps skills \
                 under the agent directory, so --skills-dir does not apply"
                    .to_string(),
            ));
        }
        let recorded = recorded_skills_dir.as_deref().unwrap_or("");
        if requested.as_path() != Path::new(recorded) {
            return Err(CliError::Usage(format!(
                "refusing to remove: requested skills dir '{}' does \
                 not match the recorded skills root '{recorded}'; not deleting \
                 outside the recorded root",
                requested.display()
            )));
        }
    }
    // Without --skills-dir the recorded root is authoritative, so a
    // symlinked <agent-dir>/skills is never consulted or followed.
    let skills_root: PathBuf = match &recorded_skills_dir {
        Some(recorded) => PathBuf::from(recorded),
        None => agent_dir.clone(),
    };
    // Pin the agent directory and the OMP profiles/agent trees for the
    // whole removal, plus the recorded skills root: every read, re-verify,
    // and mutation below re-walks from these descriptors, so a swapped-in
    // symlink between check and use is observed instead of followed.
    let roots = symlinks::Roots::open(&agent_dir)?;
    // Pin the skills root with the no-symlink-root policy: a symlink at
    // the root itself fails closed before any planning or mutation,
    // instead of redirecting every later read and write. An absent root
    // pins as `None` (targets under it read as absent; mutations refuse).
    let skills_pin =
        match RootFd::open_strict_root(&skills_root).map_err(|err| io_err(&skills_root, err))? {
            crate::anchored::StrictRoot::Pinned(pin) => Some(pin),
            crate::anchored::StrictRoot::Absent => None,
            crate::anchored::StrictRoot::Symlink => {
                return Err(CliError::Topology(format!(
                    "refusing to continue: the skills root '{}' is a symlink",
                    skills_root.display()
                )));
            }
        };

    let config_path = agent_dir.join(yamlwork::CONFIG_NAME);
    let installed_values = state::decode_values(
        manifest
            .get("config")
            .and_then(|c| c.get("installed"))
            .unwrap_or(&Json::Null),
    );
    let prior_values = state::decode_values(
        manifest
            .get("config")
            .and_then(|c| c.get("prior"))
            .unwrap_or(&Json::Null),
    );
    // Set when install created the config file from an absent file: once
    // every topology-owned key is resolved away and nothing else remains,
    // remove deletes the file instead of leaving an empty shell behind.
    let config_created = manifest
        .get("config_created")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    // Profile-visible link identities recorded at install time, needed to
    // recheck the active config's link before its single snapshot read.
    let recorded_links = symlinks::links_from_manifest(&manifest);

    // ---- single config snapshot: read the live file exactly once ----
    //
    // The active config may live behind a supported main->topology link:
    // recheck the recorded link identity and resolve the target up front,
    // then read, rewrite, restore, and delete only the resolved target so
    // the visible symlink itself is never replaced with a regular file.
    // A drifted, dangling, escaping, or newly appeared link fails closed
    // here, before any mutation.
    let mut config_usable = true;
    let mut config_deleted = false;
    let mut original_config: Option<Vec<u8>> = None;
    let mut config_target = config_path.clone();
    let config_present = match &roots.agent_root {
        Some(pin) => pin.exists(yamlwork::CONFIG_NAME).unwrap_or(false),
        None => false,
    };
    if config_present {
        let resolved = symlinks::resolve_owned_at(
            &roots,
            yamlwork::CONFIG_NAME,
            opts.topology,
            recorded_links.get(yamlwork::CONFIG_NAME),
        )?;
        config_target = resolved.resolved().to_path_buf();
        if !resolved.is_file().unwrap_or(false) {
            config_usable = false;
        } else {
            original_config = Some(
                resolved
                    .read()?
                    .ok_or_else(|| io_err(&config_target, not_found_io()))?,
            );
        }
    } else if !installed_values.is_empty() {
        config_deleted = true;
    }

    let mut current_config_model: Value = Value::Map(BTreeMap::new());
    if config_usable && let Some(bytes) = &original_config {
        match yamlwork::parse_config(bytes, &format!("config '{}'", config_path.display()))
            .and_then(|doc| {
                yamlwork::migrated_model(&doc, &format!("config '{}'", config_path.display()))
            }) {
            Ok(model) => current_config_model = model,
            Err(_) => config_usable = false,
        }
    }

    // ---- plan files ----
    // Files recorded behind a supported main->topology link are planned
    // against their resolved target; any link drift (retargeted, escaped,
    // or replaced by an unrecorded link) aborts the planning before any
    // mutation instead of silently touching a new destination.
    let mut owned_resolved: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut kept_files: Vec<String> = Vec::new();
    let mut symlink_files: Vec<String> = Vec::new();
    let mut removed_files: Vec<String> = Vec::new();
    let mut missing_files: Vec<String> = Vec::new();
    let mut released_files: Vec<String> = Vec::new();
    for (rel, recorded) in &owned_files {
        // Released without mutation, but only after the recorded link
        // identity is rechecked: an adopted entry reached its target
        // through a user-made main->topology link, and a retargeted,
        // dangling, or unresolvable link must abort before any manifest
        // or registry mutation instead of silently releasing ownership
        // of a path that now points elsewhere. The resolved path is
        // discarded — release never mutates target bytes.
        if preexisting.contains(rel) {
            symlinks::resolve_owned_at(&roots, rel, opts.topology, recorded_links.get(rel))?;
            released_files.push(rel.clone());
            continue;
        }
        // A sibling alias still owning this path keeps it: release our
        // ownership record without touching target bytes.
        if cross_alias.files.contains_key(rel) {
            released_files.push(rel.clone());
            continue;
        }
        let resolved =
            symlinks::resolve_owned_at(&roots, rel, opts.topology, recorded_links.get(rel))?;
        let target = resolved.resolved().to_path_buf();
        owned_resolved.insert(rel.clone(), target.clone());
        if !resolved.exists().unwrap_or(false) {
            missing_files.push(rel.clone());
        } else if resolved.has_symlink_component().unwrap_or(false) {
            symlink_files.push(rel.clone());
        } else {
            let current = resolved
                .read()?
                .ok_or_else(|| io_err(&target, not_found_io()))?;
            if fsx::sha256_hex(&current) == *recorded {
                removed_files.push(rel.clone());
            } else {
                kept_files.push(rel.clone());
            }
        }
    }
    let mut kept_skill_files: Vec<String> = Vec::new();
    let mut symlink_skill_files: Vec<String> = Vec::new();
    let mut removed_skill_files: Vec<String> = Vec::new();
    let mut missing_skill_files: Vec<String> = Vec::new();
    for (rel, recorded) in &skill_files {
        // A path a sibling alias still owns is released without touching
        // target bytes (only reachable from a corrupt double-ownership
        // manifest; the installer refuses to record one).
        if cross_alias.skills_files.contains_key(rel) {
            released_files.push(rel.clone());
            continue;
        }
        let target = skills_root.join(rel);
        if !exists_under(&skills_pin, rel) {
            missing_skill_files.push(rel.clone());
        } else if symlink_under(&skills_pin, rel) {
            symlink_skill_files.push(rel.clone());
        } else {
            let current = read_under(&skills_pin, rel, &target)?
                .ok_or_else(|| io_err(&target, not_found_io()))?;
            if fsx::sha256_hex(&current) == *recorded {
                removed_skill_files.push(rel.clone());
            } else {
                kept_skill_files.push(rel.clone());
            }
        }
    }

    // ---- plan config ----
    let mut restored_keys: Vec<(PathKey, Value)> = Vec::new();
    let mut deleted_keys: Vec<PathKey> = Vec::new();
    // (path, reason): edited | missing | unknown
    let mut preserved_entries: Vec<(PathKey, &'static str)> = Vec::new();
    let mut sorted_installed: Vec<&PathKey> = installed_values.keys().collect();
    sorted_installed.sort_by_key(|parts| (parts.len(), (*parts).clone()));
    if config_usable && !config_deleted {
        // Restored paths shadow their descendants; a HashSet of restored
        // prefixes gives O(D) ancestor lookups per key (O(KD) total)
        // instead of scanning the growing restored list.
        let mut restored_set: HashSet<PathKey> = HashSet::new();
        for parts in sorted_installed {
            // A sibling alias still owns this key: leave the live value
            // untouched and simply drop it from this manifest.
            if cross_alias.config_installed.contains_key(parts) {
                continue;
            }
            let shadowed = (1..parts.len()).any(|depth| restored_set.contains(&parts[..depth]));
            if shadowed {
                continue;
            }
            match value::resolve_path(&current_config_model, parts) {
                None => preserved_entries.push((parts.clone(), "missing")),
                Some(current) => {
                    let installed = installed_values
                        .get(parts)
                        .and_then(|value| value.clone())
                        .unwrap_or(Value::Null);
                    if current != &installed {
                        preserved_entries.push((parts.clone(), "edited"));
                        continue;
                    }
                    restored_set.insert(parts.clone());
                    match prior_values.get(parts) {
                        // Key not recorded, or recorded as absent because the
                        // user did not have it at install time: delete it.
                        None | Some(None) => deleted_keys.push(parts.clone()),
                        Some(Some(prior)) => {
                            restored_keys.push((parts.clone(), prior.clone()));
                        }
                    }
                }
            }
        }
    } else if !installed_values.is_empty() {
        // Unreadable/symlinked/deleted config: keep every key unresolved.
        preserved_entries = sorted_installed
            .iter()
            .map(|parts| ((*parts).clone(), "unknown"))
            .collect();
    }

    // Plan the exact text the targeted restoration would produce, so the
    // whole-file decision below applies the same semantics as the live
    // edit: comments, document markers, and user-added keys or empty
    // mappings all survive in the planned text and keep the file alive.
    // A plan failure (a model path that cannot be matched in the document)
    // aborts here, before anything is mutated; a dry-run only loses the
    // delete prediction and still reports the would-restore keys.
    let planned_config_text =
        if (!restored_keys.is_empty() || !deleted_keys.is_empty()) && original_config.is_some() {
            let bytes = original_config.as_deref().expect("presence checked above");
            match plan_config_restore(
                bytes,
                &current_config_model,
                &restored_keys,
                &deleted_keys,
                &config_path,
            ) {
                Ok(text) => Some(text),
                Err(_) if opts.dry_run => None,
                Err(exc) => return Err(exc),
            }
        } else {
            None
        };

    // Installer-created config whose restoration empties it completely —
    // the planned text is a bare empty mapping with no comments, document
    // markers, or user-added keys/empty mappings left — is deleted
    // outright (mirroring install's rollback, which removes a config it
    // created from an absent file). Anything else keeps the file, with
    // only the topology keys removed.
    let planned_config_delete = config_created
        && config_usable
        && !config_deleted
        && original_config.is_some()
        && preserved_entries.is_empty()
        && restored_keys.is_empty()
        && !deleted_keys.is_empty()
        && planned_config_text
            .as_deref()
            .is_some_and(|text| text.trim() == "{}");

    // ---- plan replace-mode config restoration ----
    // A profile installed with --config-replace carries its previous
    // config verbatim in the manifest as a backup. Removal restores it
    // only while the live config is still byte-identical to the recorded
    // replacement; a later edit or deletion is preserved, and the backup
    // stays recorded in the manifest for manual recovery.
    let mut replace_restore: Option<Option<Vec<u8>>> = None;
    let mut replace_preserved = false;
    if let Some(replaced_hash) = manifest
        .get("config_replace")
        .and_then(|section| section.get("replaced_sha256"))
        .and_then(Json::as_str)
    {
        match &original_config {
            Some(original) if fsx::sha256_hex(original) == replaced_hash => {
                replace_restore = Some(
                    match manifest
                        .get("config_replace")
                        .and_then(|section| section.get("backup_text"))
                    {
                        Some(Json::String(text)) => Some(text.clone().into_bytes()),
                        _ => None,
                    },
                );
            }
            _ => replace_preserved = true,
        }
    }

    // ---- plan candidate cleanup ----
    // An installer-published config.yml.new is withdrawn only while its
    // bytes still match the recorded ownership hash; a user edit, a
    // swapped-in link, or any other replacement is preserved and
    // reported.
    let candidate_path = agent_dir.join(CONFIG_NEW_NAME);
    let mut planned_candidate_delete = false;
    let mut candidate_preserved = false;
    if let Some(recorded) = manifest
        .get("config_candidate_sha256")
        .and_then(Json::as_str)
        && target_exists(&roots, &candidate_path)
    {
        if target_has_symlink(&roots, &candidate_path) || !target_is_file(&roots, &candidate_path) {
            candidate_preserved = true;
        } else {
            let live = read_target(&roots, &candidate_path)?
                .ok_or_else(|| io_err(&candidate_path, not_found_io()))?;
            if fsx::sha256_hex(&live) == recorded {
                planned_candidate_delete = true;
            } else {
                candidate_preserved = true;
            }
        }
    }

    if opts.dry_run {
        out.push(format!(
            "dry-run: removing '{}' from {}",
            opts.topology,
            agent_dir.display()
        ));
        for rel in &removed_files {
            match owned_resolved.get(rel) {
                Some(resolved) if resolved != &agent_dir.join(rel) => out.push(format!(
                    "  would remove: {rel} (through profile link -> {})",
                    resolved.display()
                )),
                _ => out.push(format!("  would remove: {rel}")),
            }
        }
        for rel in kept_files
            .iter()
            .chain(symlink_files.iter())
            .chain(missing_files.iter())
        {
            match owned_resolved.get(rel) {
                Some(resolved) if resolved != &agent_dir.join(rel) => out.push(format!(
                    "  would preserve: {rel} (through profile link -> {})",
                    resolved.display()
                )),
                _ => out.push(format!("  would preserve: {rel}")),
            }
        }
        for rel in &released_files {
            out.push(format!("  would release: {rel} (pre-existing)"));
        }
        if !skill_files.is_empty() {
            out.push(format!(
                "dry-run: skills live under {}",
                skills_root.display()
            ));
            for rel in &removed_skill_files {
                out.push(format!("  would remove: skills/{rel}"));
            }
            for rel in kept_skill_files
                .iter()
                .chain(symlink_skill_files.iter())
                .chain(missing_skill_files.iter())
            {
                out.push(format!("  would preserve: skills/{rel}"));
            }
        }
        for (parts, _) in &restored_keys {
            out.push(format!("  would restore config key: {}", parts.join(".")));
        }
        for parts in &deleted_keys {
            out.push(format!("  would restore config key: {}", parts.join(".")));
        }
        if planned_config_delete {
            out.push(format!("  would remove: {}", yamlwork::CONFIG_NAME));
        }
        match &replace_restore {
            Some(None) => out.push(format!("  would remove: {}", yamlwork::CONFIG_NAME)),
            Some(Some(_)) => out.push(format!("  would restore: {}", yamlwork::CONFIG_NAME)),
            None => {}
        }
        if planned_candidate_delete {
            out.push(format!("  would remove: {CONFIG_NEW_NAME}"));
        } else if candidate_preserved {
            out.push(format!("  would preserve: {CONFIG_NEW_NAME}"));
        }
        return Ok(0);
    }

    // ---- live recheck before mutation (optimistic, single-writer races) ----
    if let Some(original) = &original_config {
        let live = if target_is_file(&roots, &config_target) {
            Some(
                read_target(&roots, &config_target)?
                    .ok_or_else(|| io_err(&config_target, not_found_io()))?,
            )
        } else {
            None
        };
        if live.as_deref() != Some(original.as_slice()) {
            return Err(CliError::Usage(format!(
                "'{}' changed while planning removal; aborting \
                 without clobbering it",
                config_path.display()
            )));
        }
    }

    // ---- snapshot before mutation ----
    let state_dir = state::agent_state_dir(&agent_dir, state_anchor)?;
    let snapshot_dir = state_dir
        .join("snapshots")
        .join(format!("remove-{}", fsx::timestamp()));
    // The state root lives under the XDG state directory, not the agent
    // tree: pin it directly with the strict no-symlink-root create so a
    // swapped state path cannot redirect snapshot creation.
    let state_pin =
        RootFd::open_or_create_private(&state_dir).map_err(|err| io_err(&state_dir, err))?;
    let snapshot_prefix = snapshot_dir
        .strip_prefix(&state_dir)
        .expect("the snapshot dir is always directly under the state dir")
        .to_string_lossy()
        .into_owned();
    // Snapshot writes go through the pinned state root; errors keep naming
    // the snapshot file the path-based helper named.
    let snapshot_write = |name: &str, data: &[u8]| -> Result<(), CliError> {
        state_pin
            .write_private(&format!("{snapshot_prefix}/{name}"), data)
            .map_err(|err| io_err(&snapshot_dir.join(name), err))
    };
    // Every failure in this block is pre-mutation, so nothing needs
    // restoring; report through the same user-facing contract as a
    // completed rollback (instead of leaking a raw io error naming an
    // internal temp file) and drop the partial snapshot this run created.
    let snapshot_result = (|| -> Result<(), CliError> {
        if let Some(original) = &original_config {
            snapshot_write(yamlwork::CONFIG_NAME, original)?;
        }
        for rel in owned_files.keys() {
            if preexisting.contains(rel) || cross_alias.files.contains_key(rel) {
                continue;
            }
            let target = owned_resolved
                .get(rel)
                .cloned()
                .unwrap_or_else(|| agent_dir.join(rel));
            if target_exists(&roots, &target) && !target_has_symlink(&roots, &target) {
                let data =
                    read_target(&roots, &target)?.ok_or_else(|| io_err(&target, not_found_io()))?;
                snapshot_write(rel, &data)?;
            }
        }
        if planned_candidate_delete {
            let data = read_target(&roots, &candidate_path)?
                .ok_or_else(|| io_err(&candidate_path, not_found_io()))?;
            snapshot_write(CONFIG_NEW_NAME, &data)?;
        }
        for rel in skill_files.keys() {
            // Never snapshot (and therefore never roll back) paths a
            // sibling alias owns: restoring our stale copy would
            // overwrite the sibling's file.
            if cross_alias.skills_files.contains_key(rel) {
                continue;
            }
            let target = skills_root.join(rel);
            if exists_under(&skills_pin, rel) && !symlink_under(&skills_pin, rel) {
                let data = read_under(&skills_pin, rel, &target)?
                    .ok_or_else(|| io_err(&target, not_found_io()))?;
                snapshot_write(&format!("skills-root/{rel}"), &data)?;
            }
        }
        Ok(())
    })();
    if let Err(exc) = snapshot_result {
        // Cleanup stays best-effort and must never obscure the original
        // snapshot failure: on success (or an already-absent dir) the
        // contract message is unchanged; on failure the original error
        // still leads, with the leftover partial snapshot and why it
        // could not be removed appended so it is never silent.
        let mut message = format!("remove failed and changes were rolled back: {exc}");
        if let Err(cleanup) = discard_partial_snapshot(&state_pin, &snapshot_prefix, &snapshot_dir)
        {
            message.push_str(&format!(
                "\nwarning: the live remove changes were NOT applied, but the \
                 partial snapshot at {cleanup} could not be removed; delete it \
                 manually before retrying"
            ));
        }
        return Err(CliError::Usage(message));
    }

    // ---- apply ----
    let mut removed_set: HashSet<String> = HashSet::new();
    let mut removed_skill_set: HashSet<String> = HashSet::new();
    // SHA-256 of the bytes this operation wrote to the config, for a
    // guarded rollback: the config is restored only when we actually
    // rewrote it and the live bytes still hash to the same digest, so a
    // concurrent edit made after our write is never clobbered and a file
    // we never touched is never "restored".
    let mut config_written: Option<String> = None;
    // Set when this remove deleted an installer-created config file; the
    // guarded rollback restores it from the snapshot only while the path
    // is still absent (a replacement is a concurrent change and is kept).
    let mut config_removed = false;
    // Set when this remove deleted an installer-published candidate; the
    // guarded rollback restores it from the snapshot only while the path
    // is still absent.
    let mut candidate_removed = false;
    let apply_result = (|| -> Result<(), CliError> {
        let mut late_kept: Vec<String> = Vec::new();
        for rel in &removed_files {
            let target = owned_resolved
                .get(rel)
                .cloned()
                .unwrap_or_else(|| agent_dir.join(rel));
            // Re-verify immediately before unlink — no symlink at any
            // component, still a regular file — so a concurrent swap
            // (including a symlink whose target has identical bytes) is
            // preserved instead of destroyed, and a file vanished by a
            // concurrent remover is kept rather than failing the run.
            if target_has_symlink(&roots, &target) || !target_is_file(&roots, &target) {
                late_kept.push(rel.clone());
                continue;
            }
            let current =
                read_target(&roots, &target)?.ok_or_else(|| io_err(&target, not_found_io()))?;
            if fsx::sha256_hex(&current) != owned_files[rel] {
                late_kept.push(rel.clone());
                continue;
            }
            remove_target(&roots, &target)?;
            // Record the deletion the moment it lands: a failure later in
            // the loop must still roll this file back from the snapshot,
            // and the partial-remove manifest must not claim it was kept.
            removed_set.insert(rel.clone());
        }
        kept_files.extend(late_kept.iter().cloned());
        let mut late_kept_skill: Vec<String> = Vec::new();
        for rel in &removed_skill_files {
            let target = skills_root.join(rel);
            // Same immediate re-verification as the agent loop: a
            // concurrent swap or replacement is preserved, never unlinked.
            if symlink_under(&skills_pin, rel) || !is_file_under(&skills_pin, rel) {
                late_kept_skill.push(rel.clone());
                continue;
            }
            let current = read_under(&skills_pin, rel, &target)?
                .ok_or_else(|| io_err(&target, not_found_io()))?;
            if fsx::sha256_hex(&current) != skill_files[rel] {
                late_kept_skill.push(rel.clone());
                continue;
            }
            remove_under(&skills_pin, rel, &target)?;
            // Record the deletion immediately so a later failure still
            // rolls it back from the snapshot.
            removed_skill_set.insert(rel.clone());
        }
        kept_skill_files.extend(late_kept_skill.iter().cloned());
        let mut dir_sort: Vec<&String> = created_dirs.iter().collect();
        dir_sort.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));
        for rel in dir_sort {
            remove_empty_dir_at(roots.agent_root.as_ref(), rel);
        }
        let mut skill_dir_sort: Vec<&String> = skill_dirs.iter().collect();
        skill_dir_sort.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));
        for rel in skill_dir_sort {
            remove_empty_dir_at(skills_pin.as_ref(), rel);
        }
        if planned_config_delete || replace_restore == Some(None) {
            // Re-verify immediately before unlink that every guard from
            // planning still holds — no symlink at any component, the
            // final component is still a regular file, and the live bytes
            // still match what planning read — so a concurrent swap
            // (including a symlink whose target has identical bytes)
            // aborts instead of being destroyed. All checks and the
            // unlink run at the resolved target so a supported
            // main->topology symlink at the visible path is never
            // replaced.
            if target_has_symlink(&roots, &config_target) || !target_is_file(&roots, &config_target)
            {
                return Err(CliError::Usage(format!(
                    "'{}' changed while removing; aborting \
                     without clobbering it",
                    config_path.display()
                )));
            }
            let live = read_target(&roots, &config_target)?
                .ok_or_else(|| io_err(&config_target, not_found_io()))?;
            let original = original_config
                .as_deref()
                .expect("planned delete implies a config");
            if fsx::sha256_hex(&live) != fsx::sha256_hex(original) {
                return Err(CliError::Usage(format!(
                    "'{}' changed while removing; aborting \
                     without clobbering it",
                    config_path.display()
                )));
            }
            remove_target(&roots, &config_target)?;
            config_removed = true;
        } else if let Some(new_text) = &planned_config_text
            && new_text.as_bytes() != original_config.as_deref().unwrap_or_default()
        {
            let live = if target_is_file(&roots, &config_target) {
                Some(
                    read_target(&roots, &config_target)?
                        .ok_or_else(|| io_err(&config_target, not_found_io()))?,
                )
            } else {
                None
            };
            if live.as_deref() != original_config.as_deref() {
                return Err(CliError::Usage(format!(
                    "'{}' changed while restoring; aborting \
                     without clobbering it",
                    config_path.display()
                )));
            }
            let mode = target_mode(&roots, &config_target);
            write_target(&roots, &config_target, new_text.as_bytes(), mode)?;
            config_written = Some(fsx::sha256_hex(new_text.as_bytes()));
        } else if let Some(Some(backup)) = &replace_restore
            && backup.as_slice() != original_config.as_deref().unwrap_or_default()
        {
            // Restore the pre-replacement config, guarded exactly like the
            // merge restoration above: the live bytes must still match
            // what planning read. The write goes to the resolved target
            // so a supported main->topology symlink stays intact.
            let live = if target_is_file(&roots, &config_target) {
                Some(
                    read_target(&roots, &config_target)?
                        .ok_or_else(|| io_err(&config_target, not_found_io()))?,
                )
            } else {
                None
            };
            if live.as_deref() != original_config.as_deref() {
                return Err(CliError::Usage(format!(
                    "'{}' changed while restoring; aborting \
                     without clobbering it",
                    config_path.display()
                )));
            }
            let mode = target_mode(&roots, &config_target);
            write_target(&roots, &config_target, backup, mode)?;
            config_written = Some(fsx::sha256_hex(backup));
        }
        // Withdraw an installer-published candidate only while it is
        // still a plain file whose bytes match the recorded ownership
        // hash; a concurrent edit aborts instead of being destroyed.
        if planned_candidate_delete {
            if target_has_symlink(&roots, &candidate_path)
                || !target_is_file(&roots, &candidate_path)
            {
                return Err(CliError::Usage(format!(
                    "'{}' changed while removing; aborting \
                     without clobbering it",
                    candidate_path.display()
                )));
            }
            let live = read_target(&roots, &candidate_path)?
                .ok_or_else(|| io_err(&candidate_path, not_found_io()))?;
            let recorded = manifest
                .get("config_candidate_sha256")
                .and_then(Json::as_str)
                .expect("planned candidate delete implies a recorded hash");
            if fsx::sha256_hex(&live) != recorded {
                return Err(CliError::Usage(format!(
                    "'{}' changed while removing; aborting \
                     without clobbering it",
                    candidate_path.display()
                )));
            }
            remove_target(&roots, &candidate_path)?;
            candidate_removed = true;
        }
        Ok(())
    })();

    // Guarded rollback from the recorded snapshot, shared by the apply and
    // commit failure paths. Every target is classified: restored (returned
    // to its pre-remove bytes), preserved (a concurrent change now occupies
    // the path and restoring would clobber it), or failed (the restore or
    // its snapshot read errored). The config is restored only when this
    // operation actually rewrote it and the live bytes still hash to the
    // recorded digest; removed files are restored only where they are
    // still absent.
    let rollback_from_snapshot = || -> RollbackReport {
        let mut report = RollbackReport {
            restored: Vec::new(),
            preserved: Vec::new(),
            failed: Vec::new(),
        };
        if let Some(written) = &config_written {
            let live_matches = read_target(&roots, &config_target)
                .map(|bytes| bytes.is_some_and(|bytes| fsx::sha256_hex(&bytes) == *written))
                .unwrap_or(false);
            if live_matches {
                let config_snapshot_rel = format!("{snapshot_prefix}/{}", yamlwork::CONFIG_NAME);
                let config_snapshot = snapshot_dir.join(yamlwork::CONFIG_NAME);
                match state_pin.read_file(&config_snapshot_rel) {
                    Ok(Some(data)) => match agent_restore_pin(&roots).and_then(|pin| {
                        write_under(
                            Some(&pin),
                            yamlwork::CONFIG_NAME,
                            &config_target,
                            &data,
                            None,
                        )
                    }) {
                        Ok(()) => report.restored.push(config_path.display().to_string()),
                        Err(err) => report
                            .failed
                            .push(format!("{} (restore failed: {err})", config_path.display())),
                    },
                    Ok(None) => report.failed.push(format!(
                        "{} (snapshot unreadable: {})",
                        config_path.display(),
                        io_err(&config_snapshot, not_found_io())
                    )),
                    Err(err) => report.failed.push(format!(
                        "{} (snapshot unreadable: {})",
                        config_path.display(),
                        io_err(&config_snapshot, err)
                    )),
                }
            } else {
                // The live config no longer matches what we wrote: a
                // concurrent edit replaced it, so restoring the snapshot
                // would clobber that edit.
                report.preserved.push(format!(
                    "{} (concurrent config edit kept)",
                    config_path.display()
                ));
            }
        } else if config_removed {
            // This remove deleted an installer-created config; restore the
            // pre-remove bytes only while the path is still absent — a
            // file appearing after our delete is a concurrent change and
            // is kept. Publication is an atomic no-replace create, so a
            // concurrent target (file, directory, or symlink) can never be
            // overwritten even if it appears mid-restore; an existing
            // target makes the rollback incomplete, which keeps and marks
            // the recovery snapshot.
            let config_snapshot_rel = format!("{snapshot_prefix}/{}", yamlwork::CONFIG_NAME);
            let config_snapshot = snapshot_dir.join(yamlwork::CONFIG_NAME);
            match state_pin.read_file(&config_snapshot_rel) {
                Ok(Some(data)) => match agent_restore_pin(&roots).and_then(|pin| {
                    let pin = Some(pin);
                    create_exclusive_under(&pin, yamlwork::CONFIG_NAME, &config_target, &data, None)
                }) {
                    Ok(true) => report.restored.push(config_path.display().to_string()),
                    Ok(false) => report.preserved.push(format!(
                        "{} (concurrent replacement kept)",
                        config_path.display()
                    )),
                    Err(err) => report
                        .failed
                        .push(format!("{} (restore failed: {err})", config_path.display())),
                },
                Err(err) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    config_path.display(),
                    io_err(&config_snapshot, err)
                )),
                Ok(None) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    config_path.display(),
                    io_err(&config_snapshot, not_found_io())
                )),
            }
        }
        if candidate_removed {
            let candidate_snapshot_rel = format!("{snapshot_prefix}/{CONFIG_NEW_NAME}");
            let candidate_snapshot = snapshot_dir.join(CONFIG_NEW_NAME);
            match state_pin.read_file(&candidate_snapshot_rel) {
                Ok(Some(data)) => match agent_restore_pin(&roots).and_then(|pin| {
                    let pin = Some(pin);
                    create_exclusive_under(&pin, CONFIG_NEW_NAME, &candidate_path, &data, None)
                }) {
                    Ok(true) => report.restored.push(candidate_path.display().to_string()),
                    Ok(false) => report.preserved.push(format!(
                        "{} (concurrent replacement kept)",
                        candidate_path.display()
                    )),
                    Err(err) => report.failed.push(format!(
                        "{} (restore failed: {err})",
                        candidate_path.display()
                    )),
                },
                Ok(None) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    candidate_path.display(),
                    io_err(&candidate_snapshot, not_found_io())
                )),
                Err(err) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    candidate_path.display(),
                    io_err(&candidate_snapshot, err)
                )),
            }
        }
        for rel in &removed_set {
            let target = owned_resolved
                .get(rel)
                .cloned()
                .unwrap_or_else(|| agent_dir.join(rel));
            let snap = snapshot_dir.join(rel);
            let snap_rel = format!("{snapshot_prefix}/{rel}");
            match state_pin.read_file(&snap_rel) {
                // Publication is an atomic no-replace create, so a
                // concurrent target (file, directory, or symlink) that
                // appears mid-restore is never overwritten; it makes the
                // rollback incomplete, which keeps and marks the recovery
                // snapshot.
                Ok(Some(data)) => match agent_restore_pin(&roots).and_then(|pin| {
                    let pin = Some(pin);
                    create_exclusive_under(&pin, rel, &target, &data, None)
                }) {
                    Ok(true) => report.restored.push(target.display().to_string()),
                    Ok(false) => report.preserved.push(format!(
                        "{} (concurrent replacement kept)",
                        target.display()
                    )),
                    Err(err) => report
                        .failed
                        .push(format!("{} (restore failed: {err})", target.display())),
                },
                Ok(None) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    target.display(),
                    io_err(&snap, not_found_io())
                )),
                Err(err) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    target.display(),
                    io_err(&snap, err)
                )),
            }
        }
        for rel in &removed_skill_set {
            let target = skills_root.join(rel);
            let snap = snapshot_dir.join("skills-root").join(rel);
            let snap_rel = format!("{snapshot_prefix}/skills-root/{rel}");
            match state_pin.read_file(&snap_rel) {
                Ok(Some(data)) => {
                    match skills_restore_pin(&skills_pin, &skills_root).and_then(|pin| {
                        let pin = Some(pin);
                        create_exclusive_under(&pin, rel, &target, &data, None)
                    }) {
                        Ok(true) => report.restored.push(target.display().to_string()),
                        Ok(false) => report.preserved.push(format!(
                            "{} (concurrent replacement kept)",
                            target.display()
                        )),
                        Err(err) => report
                            .failed
                            .push(format!("{} (restore failed: {err})", target.display())),
                    }
                }
                Ok(None) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    target.display(),
                    io_err(&snap, not_found_io())
                )),
                Err(err) => report.failed.push(format!(
                    "{} (snapshot unreadable: {})",
                    target.display(),
                    io_err(&snap, err)
                )),
            }
        }
        report
    };

    if let Err(exc) = apply_result {
        let report = rollback_from_snapshot();
        if report.is_complete() {
            let _ = state_pin.remove_tree(&snapshot_prefix);
            return Err(CliError::Usage(format!(
                "remove failed and changes were rolled back: {exc}"
            )));
        }
        let manifest_display = manifest_path.display().to_string();
        return Err(incomplete_rollback_error(
            &format!("remove failed: {exc}"),
            &format!(
                "{manifest_display} is intact and still records the \
                 pre-remove state"
            ),
            &snapshot_dir,
            &report,
            opts.alias,
        ));
    }

    // ---- report ----
    let mut conflicts: Vec<String> = Vec::new();
    released_files.sort();
    kept_files.sort();
    for rel in &kept_files {
        conflicts.push(format!("preserved (modified after install): {rel}"));
    }
    kept_skill_files.sort();
    for rel in &kept_skill_files {
        conflicts.push(format!(
            "preserved (modified after install): skills/{rel} under {}",
            skills_root.display()
        ));
    }
    symlink_files.sort();
    for rel in &symlink_files {
        conflicts.push(format!("preserved (symlink after install): {rel}"));
    }
    symlink_skill_files.sort();
    for rel in &symlink_skill_files {
        conflicts.push(format!(
            "preserved (symlink after install): skills/{rel} under {}",
            skills_root.display()
        ));
    }
    missing_files.sort();
    for rel in &missing_files {
        conflicts.push(format!("preserved (missing after install): {rel}"));
    }
    missing_skill_files.sort();
    for rel in &missing_skill_files {
        conflicts.push(format!(
            "preserved (missing after install): skills/{rel} under {}",
            skills_root.display()
        ));
    }
    for (reason, label) in [("edited", "edited"), ("missing", "removed")] {
        let keys: Vec<String> = preserved_entries
            .iter()
            .filter(|(_, why)| *why == reason)
            .map(|(parts, _)| parts.join("."))
            .collect();
        if !keys.is_empty() {
            let mut keys = keys;
            keys.sort();
            conflicts.push(format!(
                "preserved ({label} after install): {} ({})",
                yamlwork::CONFIG_NAME,
                keys.join(", ")
            ));
        }
    }
    let unknown_keys: Vec<&PathKey> = preserved_entries
        .iter()
        .filter(|(_, why)| *why == "unknown")
        .map(|(parts, _)| parts)
        .collect();
    if !unknown_keys.is_empty() && original_config.is_some() {
        conflicts.push(format!(
            "preserved: {} (unreadable; restore it manually from the snapshot \
             at {})",
            yamlwork::CONFIG_NAME,
            snapshot_dir.display()
        ));
    }
    if config_deleted {
        conflicts.push(format!(
            "preserved: {} (deleted after install; nothing to restore)",
            yamlwork::CONFIG_NAME
        ));
    }
    if replace_preserved {
        conflicts.push(format!(
            "preserved: {} (modified or deleted after a --config-replace \
             install; the pre-replacement backup remains recorded in the \
             ownership manifest)",
            yamlwork::CONFIG_NAME
        ));
    }
    if candidate_preserved {
        conflicts.push(format!(
            "preserved: {CONFIG_NEW_NAME} (modified after install; left in \
             place)"
        ));
    }
    if !config_usable && !config_deleted {
        conflicts.push(format!(
            "preserved: {} (not a regular file after install; left untouched)",
            yamlwork::CONFIG_NAME
        ));
    }

    let unresolved_files: BTreeMap<String, String> = owned_files
        .iter()
        .filter(|(rel, _)| !removed_set.contains(*rel) && !preexisting.contains(*rel))
        .map(|(rel, hash)| (rel.clone(), hash.clone()))
        .collect();
    let unresolved_skill_files: BTreeMap<String, String> = skill_files
        .iter()
        .filter(|(rel, _)| !removed_skill_set.contains(*rel))
        .map(|(rel, hash)| (rel.clone(), hash.clone()))
        .collect();
    let unresolved_parts: Vec<PathKey> = preserved_entries
        .iter()
        .filter(|(parts, _)| installed_values.contains_key(parts))
        .map(|(parts, _)| parts.clone())
        .collect();
    // Indexed prefix structure over the unresolved file paths: building it
    // costs O(total path components) and answers each dir-descendant check
    // in O(1), replacing the O(D*F) per-dir scan below.
    let unresolved_prefixes: HashSet<&str> = unresolved_files
        .keys()
        .flat_map(|rel| rel_prefixes(rel))
        .collect();
    let unresolved_skill_prefixes: HashSet<&str> = unresolved_skill_files
        .keys()
        .flat_map(|rel| rel_prefixes(rel))
        .collect();
    let mut unresolved_values: std::collections::BTreeMap<PathKey, Option<Value>> =
        std::collections::BTreeMap::new();
    for parts in &unresolved_parts {
        if let Some(value) = installed_values.get(parts) {
            unresolved_values.insert(parts.clone(), value.clone());
        }
    }

    if !conflicts.is_empty() {
        let manifest_obj = manifest.as_object_mut().expect("manifest is an object");
        manifest_obj.insert(
            "files".to_string(),
            Json::Object(
                unresolved_files
                    .iter()
                    .map(|(k, v)| (k.clone(), Json::String(v.clone())))
                    .collect(),
            ),
        );
        // Every recorded pre-existing entry is released by this remove
        // (validation guarantees each one keys a 'files' record), so the
        // rewritten manifest must not keep an emptied adoption list.
        manifest_obj.remove("preexisting");
        let kept_dirs: Vec<Json> = created_dirs
            .iter()
            .filter(|dir| {
                unresolved_prefixes.contains(dir.as_str())
                    || target_exists(&roots, &agent_dir.join(dir))
            })
            .map(|dir| Json::String(dir.clone()))
            .collect();
        manifest_obj.insert("created_dirs".to_string(), Json::Array(kept_dirs));
        if manifest_obj.contains_key("skills_files") || !skill_files.is_empty() {
            manifest_obj.insert(
                "skills_files".to_string(),
                Json::Object(
                    unresolved_skill_files
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::String(v.clone())))
                        .collect(),
                ),
            );
            let kept_skill_dirs: Vec<Json> = skill_dirs
                .iter()
                .filter(|dir| {
                    unresolved_skill_prefixes.contains(dir.as_str())
                        || exists_under(&skills_pin, dir)
                })
                .map(|dir| Json::String(dir.clone()))
                .collect();
            manifest_obj.insert(
                "skills_created_dirs".to_string(),
                Json::Array(kept_skill_dirs),
            );
        }
        let config = manifest_obj
            .entry("config".to_string())
            .or_insert_with(|| Json::Object(Map::new()));
        if !config.is_object() {
            *config = Json::Object(Map::new());
        }
        let config_obj = config.as_object_mut().expect("just ensured an object");
        config_obj.insert(
            "installed".to_string(),
            Json::Object(state::encode_values(&unresolved_values)),
        );
        let prior_subset: std::collections::BTreeMap<PathKey, Option<Value>> = unresolved_parts
            .iter()
            .filter_map(|parts| {
                prior_values
                    .get(parts)
                    .map(|prior| (parts.clone(), prior.clone()))
            })
            .collect();
        config_obj.insert(
            "prior".to_string(),
            Json::Object(state::encode_values(&prior_subset)),
        );
        // Replace-mode bookkeeping describes restoring the recorded
        // pre-replacement config (or deleting a replacement installed
        // over no prior config). Once that restoration or deletion has
        // committed during this run, the partial manifest must drop the
        // bookkeeping so a retry does not mistake the restored config for
        // an edited replacement and can converge. A preserved replacement
        // (edited or deleted by the user after install) keeps the backup
        // recorded for manual recovery and a later retry.
        if replace_restore.is_some() {
            manifest_obj.remove("config_replace");
        }
        if let Err(exc) = state::write_manifest_at(&manifest_path, state_anchor, &manifest) {
            // Commit failed after the file/config mutations succeeded.
            // The manifest write is atomic, so the pre-remove manifest on
            // disk is still intact and authoritative: rolling the
            // file/config changes back returns to a consistent state from
            // which a retry is safe.
            let manifest_display = manifest_path.display().to_string();
            let report = rollback_from_snapshot();
            if report.is_complete() {
                let _ = state_pin.remove_tree(&snapshot_prefix);
                return Err(CliError::Usage(format!(
                    "remove failed writing the ownership manifest at \
                     {manifest_display}: {exc}; the pre-remove state was \
                     restored from the snapshot, so retrying remove is safe"
                )));
            }
            return Err(incomplete_rollback_error(
                &format!(
                    "remove failed writing the ownership manifest at \
                     {manifest_display}: {exc}"
                ),
                &format!(
                    "{manifest_display} is intact and still records the \
                     pre-remove state"
                ),
                &snapshot_dir,
                &report,
                opts.alias,
            ));
        }
        for line in &conflicts {
            out.push(line.clone());
        }
        for rel in &released_files {
            out.push(format!("released pre-existing: {rel}"));
        }
        out.push(format!(
            "partially removed '{}'; ownership manifest kept at {} for retry",
            opts.topology,
            manifest_path.display()
        ));
        // Prune old snapshots after the successful mutation only; a prune
        // failure must never roll back or fail the removal, so it surfaces
        // as a warning line in the collected output.
        if let Err(exc) =
            registry::prune_snapshots(&agent_dir, state_anchor, registry::SNAPSHOT_KEEP)
        {
            out.push(format!("warning: could not prune old snapshots: {exc}"));
        }
        return Ok(0);
    }

    let manifest_rel = manifest_path
        .strip_prefix(&state_dir)
        .expect("the ownership manifest path is always under the state dir")
        .to_string_lossy()
        .into_owned();
    if let Err(exc) = state_pin.remove_file(&manifest_rel) {
        // The tracked files and config are already removed but the final
        // manifest deletion failed; the manifest is still intact on disk
        // and no longer matches the live state. Roll the file/config
        // changes back so the recorded state matches the live state again
        // and a retry starts from a consistent pre-remove state.
        let report = rollback_from_snapshot();
        if report.is_complete() {
            let _ = state_pin.remove_tree(&snapshot_prefix);
            return Err(CliError::Usage(format!(
                "remove could not remove the ownership manifest at {}: \
                 {exc}; the pre-remove state was restored from the \
                 snapshot, so retrying remove is safe",
                manifest_path.display()
            )));
        }
        return Err(incomplete_rollback_error(
            &format!(
                "remove deleted the tracked files but could not remove \
                 the ownership manifest at {}: {exc}",
                manifest_path.display()
            ),
            &format!(
                "{} is still present but stale: the tracked files were \
                 already removed",
                manifest_path.display()
            ),
            &snapshot_dir,
            &report,
            opts.alias,
        ));
    }
    out.push(format!(
        "removed topology '{}' from {}",
        opts.topology,
        agent_dir.display()
    ));
    for rel in &released_files {
        out.push(format!("released pre-existing: {rel}"));
    }
    // Recovery markers left by earlier failed removes/installs are
    // released only now: this remove fully completed (no preserved
    // conflicts and the manifest is gone), so a later successful
    // reconciliation has run for every retained snapshot. A partial
    // remove above must not reach this. Only this alias's markers (and,
    // for the legacy-manifest owner, pre-alias unscoped markers) are
    // cleared; a sibling alias's recovery snapshot stays pinned. Clearing
    // happens before ordinary pruning — marked snapshots survive until a
    // clean reconciliation clears them. Like pruning, a clear failure
    // must never fail the removal, so it surfaces as a warning line.
    let owns_legacy_manifest = manifest_path == state::manifest_path(&agent_dir, state_anchor)?;
    if let Err(exc) = registry::clear_recovery_snapshots(
        &agent_dir,
        state_anchor,
        opts.alias,
        owns_legacy_manifest,
    ) {
        out.push(format!("warning: could not clear recovery markers: {exc}"));
    }
    // Prune old snapshots after the successful mutation only; a prune
    // failure must never roll back or fail the removal, so it surfaces as
    // a warning line in the collected output.
    if let Err(exc) = registry::prune_snapshots(&agent_dir, state_anchor, registry::SNAPSHOT_KEEP) {
        out.push(format!("warning: could not prune old snapshots: {exc}"));
    }
    Ok(0)
}

/// Synthetic `NotFound` error for the "vanished between the probe and the
/// read" race, so a target that disappeared mid-plan fails with the same
/// message shape a plain read produced.
fn not_found_io() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::NotFound)
}

/// Pin the recorded root that owns `target` and the path of `target`
/// relative to it. Linked targets resolve under the pinned profiles
/// root; everything else under the pinned agent root. `None` means the
/// target is outside every configured root: reads treat it as absent,
/// mutations refuse — a configured root is always pinned here.
/// Restore-time agent pin: the planning pin when the agent root was
/// pinned, otherwise the strict descriptor-relative open-or-create. A
/// configured root absent at command start is recreated so a restore can
/// publish into it — the pre-migration restore behavior — while a
/// symlink swapped in at the root fails closed in the strict open.
fn agent_restore_pin(roots: &symlinks::Roots) -> Result<RootFd, CliError> {
    match &roots.agent_root {
        Some(pin) => Ok(pin.clone()),
        None => RootFd::open_or_create_private(&roots.agent_dir)
            .map_err(|err| io_err(&roots.agent_dir, err)),
    }
}

/// Restore-time skills pin: same policy as `agent_restore_pin` for the
/// configured skills root.
fn skills_restore_pin(skills_pin: &Option<RootFd>, skills_root: &Path) -> Result<RootFd, CliError> {
    match skills_pin {
        Some(pin) => Ok(pin.clone()),
        None => RootFd::open_or_create_private(skills_root).map_err(|err| io_err(skills_root, err)),
    }
}

fn pin_for<'a>(roots: &'a symlinks::Roots, target: &Path) -> Option<(&'a RootFd, String)> {
    if let Some(root) = &roots.agent_root
        && let Ok(rel) = target.strip_prefix(root.path())
    {
        return Some((root, rel.to_string_lossy().into_owned()));
    }
    if let Some(root) = &roots.profiles_root
        && let Ok(rel) = target.strip_prefix(root.path())
    {
        return Some((root, rel.to_string_lossy().into_owned()));
    }
    None
}

/// Read a resolved target through its pinned root; errors keep naming
/// the resolved path the path-based helper named.
fn read_target(roots: &symlinks::Roots, target: &Path) -> Result<Option<Vec<u8>>, CliError> {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin.read_file(&rel).map_err(|err| io_err(target, err)),
        None => Ok(None),
    }
}

fn target_exists(roots: &symlinks::Roots, target: &Path) -> bool {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin.exists(&rel).unwrap_or(false),
        None => false,
    }
}

fn target_has_symlink(roots: &symlinks::Roots, target: &Path) -> bool {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin.has_symlink_component(&rel).unwrap_or(true),
        None => false,
    }
}

fn target_is_file(roots: &symlinks::Roots, target: &Path) -> bool {
    match pin_for(roots, target) {
        Some((pin, rel)) => {
            matches!(pin.kind(&rel), Ok(Some(EntryKind::File)))
        }
        None => false,
    }
}

fn target_mode(roots: &symlinks::Roots, target: &Path) -> Option<u32> {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin.file_mode(&rel).unwrap_or(None),
        None => None,
    }
}

/// Atomic publish at the resolved target; a swapped final-component
/// symlink is replaced, never followed. A target outside every
/// configured root is refused, never written through a path string.
fn write_target(
    roots: &symlinks::Roots,
    target: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), CliError> {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin
            .write_atomic(&rel, data, mode)
            .map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: '{}' is outside every pinned root",
            target.display()
        ))),
    }
}

fn remove_target(roots: &symlinks::Roots, target: &Path) -> Result<(), CliError> {
    match pin_for(roots, target) {
        Some((pin, rel)) => pin.remove_file(&rel).map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: '{}' is outside every pinned root",
            target.display()
        ))),
    }
}

// Skills-root variants: every op re-resolves `rel` under the pinned
// root. An absent pin means the skills root does not exist: reads treat
// the target as absent, mutations refuse — never a lexical fallback.

fn read_under(pin: &Option<RootFd>, rel: &str, target: &Path) -> Result<Option<Vec<u8>>, CliError> {
    match pin {
        Some(pin) => pin.read_file(rel).map_err(|err| io_err(target, err)),
        None => Ok(None),
    }
}

fn exists_under(pin: &Option<RootFd>, rel: &str) -> bool {
    match pin {
        Some(pin) => pin.exists(rel).unwrap_or(false),
        None => false,
    }
}

fn symlink_under(pin: &Option<RootFd>, rel: &str) -> bool {
    match pin {
        Some(pin) => pin.has_symlink_component(rel).unwrap_or(true),
        None => false,
    }
}

fn is_file_under(pin: &Option<RootFd>, rel: &str) -> bool {
    match pin {
        Some(pin) => matches!(pin.kind(rel), Ok(Some(EntryKind::File))),
        None => false,
    }
}

fn create_exclusive_under(
    pin: &Option<RootFd>,
    rel: &str,
    target: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<bool, CliError> {
    match pin {
        Some(pin) => pin
            .write_exclusive(rel, data, mode)
            .map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: the skills root for '{}' is not pinned",
            target.display()
        ))),
    }
}

fn remove_under(pin: &Option<RootFd>, rel: &str, target: &Path) -> Result<(), CliError> {
    match pin {
        Some(pin) => pin.remove_file(rel).map_err(|err| io_err(target, err)),
        None => Err(CliError::Topology(format!(
            "refusing to continue: the skills root for '{}' is not pinned",
            target.display()
        ))),
    }
}

/// Atomic publish through the pinned root (`install::write_under`
/// semantics for remove's restore path): a swapped final-component
/// symlink is replaced, never followed, and an absent pin refuses —
/// never a lexical fallback.
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

/// Remove `rel` when it is an empty, non-symlink directory; ignore
/// errors. Every component is re-validated immediately before removal so
/// pruning never operates through a symlinked parent (which could delete
/// content outside the recorded roots). An absent pin is a no-op: the
/// root does not exist, so the directory cannot exist either.
fn remove_empty_dir_at(pin: Option<&RootFd>, rel: &str) {
    if let Some(pin) = pin {
        if pin.has_symlink_component(rel).unwrap_or(true) {
            return;
        }
        let _ = pin.try_remove_empty_dir(rel);
    }
}

/// Best-effort removal of the partial snapshot a failed pre-mutation
/// snapshot step created. `Ok` means the directory is gone (a `NotFound`
/// failure counts as already cleaned up); `Err` names the leftover
/// directory and the reason it could not be removed, so a partial
/// snapshot is never silently left behind.
fn discard_partial_snapshot(
    pin: &RootFd,
    snapshot_prefix: &str,
    snapshot_dir: &Path,
) -> Result<(), String> {
    match pin.remove_tree(snapshot_prefix) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("{}: {err}", snapshot_dir.display())),
    }
}

/// Every strict directory prefix of a relative POSIX path: `a/b/c`
/// yields `a` and `a/b`.
fn rel_prefixes(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').map(|(idx, _)| &rel[..idx])
}

/// Set `parts` to `value` in the predicted model tree, creating maps.
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

/// Plan the exact text the targeted restoration of `restored`/`deleted`
/// keys produces on `original`, mirroring the live edit path: predict the
/// result tree, prune only ancestor mappings that the deletions emptied
/// (never unrelated user-created empty mappings), and splice the edits
/// through the lossless CST. The whole-file delete decision is made from
/// this text, so any surviving user content — comments, document markers,
/// added keys — is visible to it.
fn plan_config_restore(
    original: &[u8],
    model: &Value,
    restored: &[(PathKey, Value)],
    deleted: &[PathKey],
    config_path: &Path,
) -> Result<String, CliError> {
    let mut predicted = model.clone();
    for (parts, prior) in restored {
        set_predicted(&mut predicted, parts, prior.clone());
    }
    for parts in deleted {
        delete_predicted(&mut predicted, parts);
    }
    let mut emptied: Vec<PathKey> = Vec::new();
    let mut seen: HashSet<PathKey> = HashSet::new();
    for parts in deleted {
        for depth in (1..parts.len()).rev() {
            let ancestor: PathKey = parts[..depth].to_vec();
            if seen.insert(ancestor.clone()) {
                emptied.push(ancestor);
            }
        }
    }
    emptied.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let mut actually_emptied: Vec<PathKey> = Vec::new();
    for parts in &emptied {
        if value::resolve_path(&predicted, parts).is_some_and(|v| v.is_empty_map()) {
            actually_emptied.push(parts.clone());
            delete_predicted(&mut predicted, parts);
        }
    }
    let doc = yamlwork::parse_config(original, &format!("config '{}'", config_path.display()))?;
    yamlwork::restore_config(&doc, restored, deleted, &actually_emptied)
}

#[cfg(test)]
mod tests {
    use super::{RollbackReport, discard_partial_snapshot};
    use crate::anchored::RootFd;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            loop {
                let path = std::env::temp_dir().join(format!(
                    "omp-remove-partial-snapshot-test-{}-{}",
                    std::process::id(),
                    NEXT_ID.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!(
                        "failed to create test temp directory {}: {error}",
                        path.display()
                    ),
                }
            }
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preserved_rollback_target_makes_rollback_incomplete() {
        let report = RollbackReport {
            restored: Vec::new(),
            preserved: vec!["config.yml (concurrent replacement kept)".to_string()],
            failed: Vec::new(),
        };

        assert!(!report.is_complete());
    }

    #[test]
    fn discard_partial_snapshot_reports_non_directory_cleanup_error() {
        let temp_dir = TestTempDir::new();
        let snapshot_path = temp_dir.0.join("snapshot");
        std::fs::write(&snapshot_path, "partial snapshot").expect("failed to create test file");
        let pin = RootFd::open(&temp_dir.0).expect("failed to pin test root");

        let error = discard_partial_snapshot(&pin, "snapshot", &snapshot_path)
            .expect_err("a regular file should not be removed as a snapshot directory");
        assert!(
            error.contains(&snapshot_path.display().to_string()),
            "cleanup error did not include the leftover path: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&snapshot_path).expect("cleanup removed the test file"),
            "partial snapshot"
        );
    }

    #[test]
    fn discard_partial_snapshot_treats_missing_path_as_cleaned() {
        let temp_dir = TestTempDir::new();
        let snapshot_path = temp_dir.0.join("missing-snapshot");
        let pin = RootFd::open(&temp_dir.0).expect("failed to pin test root");

        discard_partial_snapshot(&pin, "missing-snapshot", &snapshot_path)
            .expect("a missing snapshot path should count as already cleaned");

        assert!(!snapshot_path.exists());
    }
}
