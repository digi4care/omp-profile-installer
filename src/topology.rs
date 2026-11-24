//! Topology discovery rooted in a topology source tree.
//!
//! Topologies live in `topologies/<name>` of some source tree.
//! For the current repository checkout that tree is located with
//! [`repo_root`]; resolved remote or local sources (`source.rs`) yield any
//! other root containing the same `topologies/` layout, and the
//! lookup/list/source-file APIs below accept either. Source resolution and
//! caching itself lives in `source.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::error::{CliError, io_err};
use crate::fsx;
use crate::value::{self, Value};
use crate::yamlwork;

/// Locate the repository root by looking for `topologies/`.
///
/// Discovery never consults the current working directory, so a relocated
/// or installed binary run inside an unrelated project cannot pick up that
/// project's tree. Candidates, in order:
///
/// 1. The compile-time package root (`CARGO_MANIFEST_DIR`), which holds the
///    topology tree for `cargo run` / `cargo test` and checked-out builds.
/// 2. The executable's ancestry (the built binary lives in
///    `target/{debug,release}` inside the checkout).
///
/// A candidate is only accepted when the tree actually exists inside it;
/// otherwise a clear, actionable error is returned.
pub fn repo_root() -> Result<PathBuf, CliError> {
    fn root_from(start: &Path) -> Option<PathBuf> {
        let mut candidate = start;
        loop {
            if candidate.join("topologies").is_dir() {
                return Some(candidate.to_path_buf());
            }
            candidate = candidate.parent()?;
        }
    }

    let manifest_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = root_from(manifest_root) {
        return Ok(root);
    }
    let exe = std::env::current_exe().map_err(|err| {
        CliError::Usage(format!(
            "could not locate the omp-topologies source tree: {err}"
        ))
    })?;
    if let Some(exe_dir) = exe.parent()
        && let Some(root) = root_from(exe_dir)
    {
        return Ok(root);
    }
    Err(CliError::Usage(format!(
        "could not locate the omp-topologies source tree: neither the build-time \
         package root ('{}') nor any ancestor of this executable ('{}') contains \
         'topologies'; run the installer from within the omp-topologies \
         checkout or reinstall it from that repository",
        manifest_root.display(),
        exe.display()
    )))
}

/// The topology tree (`topologies/`) inside any source-tree root:
/// the current checkout ([`repo_root`]) or a resolved source checkout.
pub fn topologies_dir(tree_root: &Path) -> PathBuf {
    tree_root.join("topologies")
}

/// The topology tree with every component beneath `tree_root` validated as
/// a real, non-symlink directory entry.
///
/// `tree_root` itself arrives canonicalized (local sources, [`repo_root`],
/// validated cache checkouts), but `topologies` beneath it could still be
/// a planted symlink. A symlinked component is rejected with an error —
/// never followed — so listing and lookup can never be redirected outside
/// the source tree. A missing component is not a symlink and passes
/// through; downstream callers handle absent trees as before.
fn checked_topologies_dir(tree_root: &Path) -> Result<PathBuf, CliError> {
    let dir = tree_root.join("topologies");
    if fsx::is_symlink(&dir) {
        return Err(CliError::Topology(format!(
            "topology tree path {} is a symlink; refusing to follow it, \
             remove the link and retry",
            dir.display()
        )));
    }
    Ok(dir)
}

/// `true` for names like `digital-product-studio` (lowercase slug).
fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn model_field<'a>(model: &'a Value, key: &str) -> Option<&'a Value> {
    match model {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

/// Parse `topology.yml` of a topology directory into the strict model.
pub fn topology_metadata(topology_dir: &Path) -> Result<Value, CliError> {
    let meta_path = topology_dir.join("topology.yml");
    let meta = match std::fs::symlink_metadata(&meta_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::Topology(format!(
                "{} is missing",
                meta_path.display()
            )));
        }
        Err(err) => return Err(io_err(&meta_path, err)),
        Ok(meta) => meta,
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::Topology(format!(
            "{} is a symlink; refusing to follow it",
            meta_path.display()
        )));
    }
    if !meta.file_type().is_file() {
        return Err(CliError::Topology(format!(
            "{} is not a regular file",
            meta_path.display()
        )));
    }
    let bytes = std::fs::read(&meta_path).map_err(|err| io_err(&meta_path, err))?;
    let doc = yamlwork::parse_config(&bytes, "topology metadata")?;
    let model = yamlwork::migrated_model(&doc, "topology metadata")?;
    Ok(model)
}

/// Resolve a topology name to its directory inside a source-tree root
/// (current checkout via [`repo_root`] or a resolved source checkout),
/// validating the slug, the declared metadata name, and (optionally)
/// installability. The `topologies` component beneath `tree_root` and the
/// final topology directory are each validated as real,
/// non-symlink directories and rejected rather than followed, so a planted
/// link can never redirect reads outside the source tree. When no
/// directory is named exactly like the slug, resolution falls back to the
/// directory whose declared `name` metadata field matches, so every entry
/// [`list_topologies`] advertises stays selectable; a slug-named directory
/// always takes precedence over a declared-name match.
pub fn find_topology(
    tree_root: &Path,
    name: &str,
    require_installable: bool,
) -> Result<PathBuf, CliError> {
    if !is_slug(name) {
        return Err(CliError::Usage(format!(
            "invalid topology name '{name}': expected a simple slug like \
             'digital-product-studio'"
        )));
    }
    let topologies_dir = checked_topologies_dir(tree_root)?;
    let topology_dir = topologies_dir.join(name);
    if fsx::is_symlink(&topology_dir) {
        return Err(CliError::Topology(format!(
            "topology '{name}' resolves to a symlinked directory ({}); \
             refusing to follow it, remove the link and retry",
            topology_dir.display()
        )));
    }
    // A directory named exactly like the slug takes precedence. When no
    // such directory exists, fall back to the name `list_topologies`
    // advertises: the declared `name` metadata field, which may differ
    // from the directory name. A listed entry must stay selectable.
    let topology_dir = if !topology_dir.is_dir() {
        match topology_by_declared_name(&topologies_dir, name)? {
            Some(dir) => dir,
            None => {
                return Err(CliError::Usage(format!(
                    "unknown topology '{name}' (looked in {})",
                    topologies_dir.display()
                )));
            }
        }
    } else {
        topology_dir
    };
    let meta = topology_metadata(&topology_dir)?;
    // Accept exactly the name `list_topologies` displays for this
    // directory: the rendered `name` metadata field when present, else
    // the directory name. A topology that appears in `list` must be
    // resolvable here; only a genuinely different declared name is an
    // error.
    let declared = match model_field(&meta, "name") {
        Some(value) => value::display_metadata(Some(value)),
        None => topology_dir
            .file_name()
            .and_then(|dir_name| dir_name.to_str())
            .unwrap_or_default()
            .to_string(),
    };
    if declared != name {
        return Err(CliError::Topology(format!(
            "topology directory '{name}' declares name '{declared}' in topology.yml"
        )));
    }
    if require_installable && !matches!(model_field(&meta, "installable"), Some(Value::Bool(true)))
    {
        return Err(CliError::Usage(format!(
            "topology '{name}' is not installable"
        )));
    }
    Ok(topology_dir)
}

/// Find the directory whose `topology.yml` declares `name` in its `name`
/// metadata field — the fallback used by [`find_topology`] when no
/// directory is named exactly like the requested slug. Symlinked
/// directories are never followed and unparseable or nameless metadata is
/// skipped, mirroring [`list_topologies`]. Deterministically picks the
/// first match by directory name.
fn topology_by_declared_name(
    topologies_dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, CliError> {
    let entries = match std::fs::read_dir(topologies_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_err(topologies_dir, err)),
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir() && !fsx::is_symlink(path))
        .collect();
    dirs.sort();
    for dir in dirs {
        let Ok(meta) = topology_metadata(&dir) else {
            continue;
        };
        if let Some(value) = model_field(&meta, "name")
            && value::display_metadata(Some(value)) == name
        {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

/// All installable topologies inside a source-tree root, sorted by
/// directory name, as `(name, version, description)` with metadata-style
/// string rendering. The `topologies` component beneath
/// `tree_root` must be a real directory — a symlinked component is
/// rejected, not followed. Symlinked topology directories are skipped,
/// never followed.
pub fn list_topologies(tree_root: &Path) -> Result<Vec<(String, String, String)>, CliError> {
    let mut results = Vec::new();
    let root = checked_topologies_dir(tree_root)?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(results),
        Err(err) => return Err(io_err(&root, err)),
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        // Symlinked entries are never followed: a planted link must not
        // redirect metadata reads outside the source tree.
        .filter(|path| path.is_dir() && !fsx::is_symlink(path))
        .collect();
    dirs.sort();
    for dir in dirs {
        let meta_path = dir.join("topology.yml");
        if !meta_path.is_file() {
            continue;
        }
        let Ok(meta) = topology_metadata(&dir) else {
            continue;
        };
        if !matches!(model_field(&meta, "installable"), Some(Value::Bool(true))) {
            continue;
        }
        let fallback_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = match model_field(&meta, "name") {
            Some(value) => value::display_metadata(Some(value)),
            None => fallback_name,
        };
        if name.is_empty() {
            continue;
        }
        let version = model_field(&meta, "version")
            .map(|v| value::display_metadata(Some(v)))
            .unwrap_or_else(|| "?".to_string());
        let description = model_field(&meta, "description")
            .map(|v| value::display_metadata(Some(v)))
            .unwrap_or_default();
        results.push((name, version, description));
    }
    Ok(results)
}

/// The metadata `name` field as a string.
pub fn meta_name(meta: &Value) -> String {
    model_field(meta, "name")
        .map(|v| value::display_metadata(Some(v)))
        .unwrap_or_default()
}

/// The metadata `version` field, rendered like Python's `str()`.
pub fn meta_version(meta: &Value) -> String {
    model_field(meta, "version")
        .map(|v| value::display_metadata(Some(v)))
        .unwrap_or_default()
}

/// Sorted `(relative, absolute)` pairs for every file under the
/// topology's `agent/` overlay, `config.yml` included, rejecting symlinks
/// and unsafe relative paths.
///
/// The `agent/` root itself must be a real directory: a symlinked root is
/// rejected outright, and symlinks encountered during traversal are never
/// followed. Relative paths use forward slashes and must pass
/// [`fsx::safe_rel`]. Results are sorted by relative path so every caller
/// sees one deterministic order.
///
/// This is the shared listing behind [`source_agent_files`] and
/// [`crate::source::overlay_digest`]; the former drops `config.yml` (user
/// config, merged separately at install), the latter keeps it (config-only
/// edits must still change the drift fingerprint).
pub fn source_overlay_paths(topology_dir: &Path) -> Result<Vec<(String, PathBuf)>, CliError> {
    let agent_root = topology_dir.join("agent");
    if fsx::is_symlink(&agent_root) {
        return Err(CliError::Topology(format!(
            "topology overlay root {} is a symlink; refusing to follow it, \
             remove the link and retry",
            agent_root.display()
        )));
    }
    if !agent_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut pending = vec![agent_root.clone()];
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|err| io_err(&dir, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| io_err(&dir, err))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|err| io_err(&path, err))?;
            if file_type.is_symlink() {
                return Err(CliError::Topology(format!(
                    "topology overlay contains an unsafe source path: {} \
                     (symlinks are not installable)",
                    path.strip_prefix(&agent_root)
                        .map(|rel| rel.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string())
                )));
            }
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                paths.push(path);
            }
        }
    }
    // Sorting by absolute path sorts by relative path too: every entry
    // shares the `agent_root` prefix.
    paths.sort();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        // Render the relative path the way Python's `as_posix()` did: OS
        // separators become '/', so a nested overlay validates identically
        // on every platform and `safe_rel` sees the same string everywhere.
        let rel_path = path.strip_prefix(&agent_root).unwrap_or(&path);
        let mut rel = String::new();
        for component in rel_path.components() {
            // File names that are not valid UTF-8 are rejected rather than
            // mangled: `rel` is the manifest key and drift-digest input, and
            // a lossy replacement could collapse two distinct files into
            // one entry.
            let part = component.as_os_str().to_str().ok_or_else(|| {
                CliError::Topology(format!(
                    "topology overlay contains a non-UTF-8 file name, \
                     which cannot be addressed safely: {}",
                    path.display()
                ))
            })?;
            if !rel.is_empty() {
                rel.push('/');
            }
            rel.push_str(part);
        }
        // `safe_rel` must see the POSIX-style relative path: it rejects
        // backslashes, which Python's `as_posix()`+`safe_rel` pair
        // rejected as well.
        if !fsx::safe_rel(&rel) {
            return Err(CliError::Topology(format!(
                "topology overlay contains an unsafe source path: {rel}"
            )));
        }
        files.push((rel, path));
    }
    Ok(files)
}

/// Read every file under the topology's `agent/` overlay (sorted, with
/// `config.yml` excluded), rejecting symlinks and unsafe relative paths.
pub fn source_agent_files(topology_dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, CliError> {
    let mut files = BTreeMap::new();
    for (rel, path) in source_overlay_paths(topology_dir)? {
        if rel == yamlwork::CONFIG_NAME {
            continue;
        }
        let data = std::fs::read(&path).map_err(|err| io_err(&path, err))?;
        files.insert(rel, data);
    }
    Ok(files)
}

/// Manifest `topology` string field for comparisons.
pub fn manifest_topology(manifest: &Json) -> Option<&str> {
    manifest.get("topology").and_then(Json::as_str)
}
