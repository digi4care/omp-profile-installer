//! Read-only status of one registry alias.
//!
//! Answers, without mutating anything: what the registry records for an
//! alias (target agent directory, profile, topology, source, installed
//! pin/digest, lifecycle state), whether the per-agent ownership manifest
//! is present and agrees with the registry, and whether the installed
//! content is current or an update is available.
//!
//! Freshness checks are side-effect free:
//!
//! * GitHub sources resolve the requested ref with
//!   [`source::resolve_remote_commit`] — `git ls-remote` only, never a
//!   clone or cache write — and compare the answer to the recorded
//!   `resolved_commit`.
//! * Local sources canonicalize the recorded path, locate the topology
//!   with [`topology::find_topology`], and hash its overlay with
//!   [`source::overlay_digest`], comparing to the recorded
//!   `overlay_digest`.
//!
//! A source that cannot be resolved (network failure, moved or deleted
//! local checkout, removed topology) is reported as
//! [`Freshness::Unknown`] with the reason, never as up-to-date. The
//! registry and manifest fail closed as elsewhere: a malformed document is
//! a hard error, not a guess.
//!
//! `state_anchor` is the stable state root anchor (separate from any
//! source checkout), so an installed binary can report status without the
//! topology repository or source being available.

use std::path::Path;

use crate::error::{CliError, io_err};
use crate::registry::{InstallState, RegistryEntry, SourceSpec, load_registry};
use crate::source::{self, TopologySource};
use crate::state;
use crate::topology;

/// Health of the per-agent ownership manifest relative to the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestStatus {
    /// Present, parseable, and its `topology` field matches the registry
    /// entry.
    Healthy,
    /// No manifest exists at the expected state location.
    Missing,
    /// Present, but its `topology` field disagrees with the registry entry
    /// (`None` when the manifest declares no topology at all).
    Mismatched { manifest_topology: Option<String> },
}

/// Whether the installed content is still current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// The source resolves to the recorded pin/digest.
    UpToDate { current: String },
    /// The source has moved on: `current` differs from the recorded
    /// pin/digest.
    UpdateAvailable { current: String },
    /// The source could not be resolved right now (network failure, moved
    /// or deleted local checkout, removed topology, or no recorded
    /// baseline to compare against); `reason` is the underlying message.
    Unknown { reason: String },
}

/// Typed, read-only status of one registry alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasStatus {
    /// The alias this status was requested for.
    pub alias: String,
    /// Installed/partial lifecycle state recorded in the registry.
    pub state: InstallState,
    /// Resolved absolute agent directory this install targets.
    pub agent_dir: std::path::PathBuf,
    /// OMP profile name, when the target was profile-selected.
    pub profile: Option<String>,
    /// Installed topology name.
    pub topology: String,
    /// Where the topology source came from.
    pub source: SourceSpec,
    /// Requested git ref (branch/tag) for GitHub sources.
    pub git_ref: Option<String>,
    /// Install timestamp in [`crate::fsx::timestamp`] format.
    pub installed_at: String,
    /// Commit the source resolved to at install time (GitHub only).
    pub installed_commit: Option<String>,
    /// Digest of the overlay inputs at install time, when an overlay was
    /// applied.
    pub installed_digest: Option<String>,
    /// Ownership-manifest presence/health for the target agent directory.
    pub manifest: ManifestStatus,
    /// Up-to-date/update-available result against the current source.
    pub freshness: Freshness,
}

/// Read-only status of one registry alias.
///
/// `state_anchor` locates the registry document and the per-agent
/// ownership manifest; it is intentionally separate from any source
/// checkout so installed binaries work without one. Nothing is written:
/// GitHub freshness uses [`source::resolve_remote_commit`] (`git
/// ls-remote` only), local freshness hashes the source overlay in place.
///
/// Errors: unknown alias, an unreadable/unsupported-version registry or
/// manifest. A present manifest whose topology disagrees, and an
/// unresolvable source, are reported in the returned data instead.
pub fn alias_status(alias: &str, state_anchor: &Path) -> Result<AliasStatus, CliError> {
    let registry = load_registry(state_anchor)?;
    let entry = registry
        .aliases
        .get(alias)
        .ok_or_else(|| CliError::Usage(format!("no install registered under alias '{alias}'")))?;
    status_for_entry(alias, entry, state_anchor)
}

/// Status for an already-loaded registry entry (shared with bulk callers).
///
/// Performs no registry load of its own; `state_anchor` is used only for
/// the ownership manifest.
pub fn status_for_entry(
    alias: &str,
    entry: &RegistryEntry,
    state_anchor: &Path,
) -> Result<AliasStatus, CliError> {
    let manifest = manifest_status(alias, entry, state_anchor)?;
    let freshness = freshness(entry);
    Ok(AliasStatus {
        alias: alias.to_string(),
        state: entry.state,
        agent_dir: entry.agent_dir.clone(),
        profile: entry.profile.clone(),
        topology: entry.topology.clone(),
        source: entry.source.clone(),
        git_ref: entry.git_ref.clone(),
        installed_at: entry.installed_at.clone(),
        installed_commit: entry.resolved_commit.clone(),
        installed_digest: entry.overlay_digest.clone(),
        manifest,
        freshness,
    })
}

/// Manifest presence/health: present and its topology matches the entry.
/// Reads the ownership manifest the alias owns (per-alias or legacy).
fn manifest_status(
    alias: &str,
    entry: &RegistryEntry,
    state_anchor: &Path,
) -> Result<ManifestStatus, CliError> {
    let manifest_path =
        state::manifest_path_for_slot(&entry.agent_dir, state_anchor, alias, entry.manifest_slot)?;
    let Some(manifest) = state::load_manifest_at(&manifest_path, state_anchor)? else {
        return Ok(ManifestStatus::Missing);
    };
    match topology::manifest_topology(&manifest) {
        Some(name) if name == entry.topology => Ok(ManifestStatus::Healthy),
        other => Ok(ManifestStatus::Mismatched {
            manifest_topology: other.map(str::to_string),
        }),
    }
}

/// Resolve the current source pin/digest and compare with the recorded
/// baseline. Resolution failures degrade to [`Freshness::Unknown`]; the
/// recorded data is never treated as current without a live comparison.
fn freshness(entry: &RegistryEntry) -> Freshness {
    match &entry.source {
        SourceSpec::GitHub { owner, repo } => {
            let source = TopologySource::GitHub {
                owner: owner.clone(),
                repo: repo.clone(),
            };
            let current = match source::resolve_remote_commit(&source, entry.git_ref.as_deref()) {
                Ok(commit) => commit,
                Err(err) => {
                    return Freshness::Unknown {
                        reason: format!("cannot resolve {}: {err}", entry.source.describe()),
                    };
                }
            };
            compare(
                entry.resolved_commit.as_deref(),
                current,
                "no commit recorded",
            )
        }
        SourceSpec::Local { path } => {
            let current = match local_overlay_digest(path, &entry.topology) {
                Ok(digest) => digest,
                Err(err) => {
                    return Freshness::Unknown {
                        reason: format!("cannot hash {}: {err}", entry.source.describe()),
                    };
                }
            };
            compare(
                entry.overlay_digest.as_deref(),
                current,
                "no overlay digest recorded",
            )
        }
    }
}

/// Canonicalize a local source path and hash the selected topology's
/// overlay. No writes anywhere; the source tree is only read.
fn local_overlay_digest(path: &Path, topology: &str) -> Result<String, CliError> {
    let root = std::fs::canonicalize(path).map_err(|err| io_err(path, err))?;
    let topology_dir = topology::find_topology(&root, topology, false)?;
    source::overlay_digest(&topology_dir)
}

/// Compare the recorded baseline against the freshly resolved value.
///
/// Both sides are lowercase-or-uppercase hex (commit SHA or SHA-256), so
/// the comparison is case-insensitive.
fn compare(baseline: Option<&str>, current: String, missing: &str) -> Freshness {
    match baseline {
        Some(installed) if installed.eq_ignore_ascii_case(&current) => {
            Freshness::UpToDate { current }
        }
        Some(_) => Freshness::UpdateAvailable { current },
        None => Freshness::Unknown {
            reason: missing.to_string(),
        },
    }
}
