//! Installer state: XDG state isolation, the per-agent ownership manifest,
//! and the manifest's config-value encoding.
//!
//! State lives under `$XDG_STATE_HOME/omp-topology` (fallback
//! `~/.local/state/omp-topology`), keyed by percent-encoded resolved agent
//! directory, never inside the target agent directory or the repository.
//!
//! The manifest format is unchanged from installer v0.1/v0.2: version 1,
//! `files` maps relative paths to SHA-256 hex digests, and `config` records
//! `prior`/`installed` leaf values keyed by JSON-encoded path arrays. Legacy
//! single-root manifests (no `skills_dir`/`skills_files`) remain readable.
//! Two optional top-level config-mode keys extend the bookkeeping:
//! `config_link_merge` marks a config link identity that a merge run
//! recorded together with merge ownership (candidate-mode updates record
//! the identity without it), and `config_replace` holds the verbatim
//! pre-replacement config for restore on remove; both carry forward across
//! non-replacing updates until remove resolves them.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS, percent_encode};
use serde_json::{Map, Value as Json};

use crate::anchored::{EntryKind, RootFd};
use crate::error::{CliError, io_err};
use crate::fsx;
use crate::value::{self, Value};

/// Ownership manifest format version written by this installer.
pub const MANIFEST_VERSION: u64 = 1;

/// Percent-encoding set matching Python's `urllib.parse.quote(safe="")`:
/// everything except RFC 3986 unreserved characters is escaped.
const PYTHON_QUOTE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[cfg(test)]
thread_local! {
    static TEST_STATE_HOME: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn with_state_home_for_test<T>(state_home: &Path, run: impl FnOnce() -> T) -> T {
    let previous = TEST_STATE_HOME.with(|current| current.replace(Some(state_home.to_path_buf())));
    struct RestoreStateHome(Option<PathBuf>);
    impl Drop for RestoreStateHome {
        fn drop(&mut self) {
            TEST_STATE_HOME.with(|current| {
                current.replace(self.0.take());
            });
        }
    }
    let _restore = RestoreStateHome(previous);
    run()
}

/// The state root for this machine, refusing to live inside the repository.
pub fn state_root(repo_root: &Path) -> Result<PathBuf, CliError> {
    #[cfg(test)]
    if let Some(base) = TEST_STATE_HOME.with(|current| current.borrow().clone()) {
        let root = fsx::resolve_loose(&base.join("omp-topology"));
        let repo = fsx::resolve_loose(repo_root);
        if root == repo || root.starts_with(&repo) {
            return Err(CliError::Usage(format!(
                "refusing to use state directory '{}': it lies inside the \
                 omp-topologies repository; set XDG_STATE_HOME to a private \
                 location outside the repository",
                root.display()
            )));
        }
        return Ok(root);
    }
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(xdg) if !xdg.is_empty() => fsx::expanduser(Path::new(&xdg)),
        _ => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local").join("state"))
            .ok_or_else(|| {
                CliError::Usage(
                    "neither XDG_STATE_HOME nor HOME is set; export one to a \
                     private location outside the repository"
                        .to_string(),
                )
            })?,
    };
    let root = base.join("omp-topology");
    let root = fsx::resolve_loose(&root);
    let repo = fsx::resolve_loose(repo_root);
    if root == repo || root.starts_with(&repo) {
        return Err(CliError::Usage(format!(
            "refusing to use state directory '{}': it lies inside the \
             omp-topologies repository; set XDG_STATE_HOME to a private \
             location outside the repository",
            root.display()
        )));
    }
    Ok(root)
}

/// The per-agent state directory, keyed by percent-encoded resolved path.
///
/// The raw path bytes are encoded (not a lossy string rendering), so every
/// distinct agent directory maps to a distinct key; for valid UTF-8 paths
/// the result is byte-identical to Python's
/// `urllib.parse.quote(os.fspath(agent_dir), safe="")`, keeping manifests
/// written by the previous installer reachable.
pub fn agent_state_dir(agent_dir: &Path, repo_root: &Path) -> Result<PathBuf, CliError> {
    let key = percent_encode(agent_dir.as_os_str().as_bytes(), PYTHON_QUOTE).to_string();
    Ok(state_root(repo_root)?.join(key))
}

pub fn manifest_path(agent_dir: &Path, repo_root: &Path) -> Result<PathBuf, CliError> {
    Ok(agent_state_dir(agent_dir, repo_root)?.join("manifest.json"))
}

/// Load and validate the ownership manifest; `None` when none exists.
/// Symlinks and any existing non-regular object are rejected rather than
/// followed or silently treated as absent. The `config` ownership section
/// is strictly validated too: a malformed section fails closed instead of
/// decoding as "no recorded values", which remove/update would otherwise
/// read as license to clobber user config.
pub fn load_manifest(agent_dir: &Path, repo_root: &Path) -> Result<Option<Json>, CliError> {
    let path = manifest_path(agent_dir, repo_root)?;
    let root_path = state_root(repo_root)?;
    // Pin the state root with the no-symlink-root policy: the agent
    // directory is a configured root, so a symlink swapped in at it
    // fails closed here instead of redirecting the manifest read.
    let root = match RootFd::open_strict_root(&root_path).map_err(|err| io_err(&root_path, err))? {
        crate::anchored::StrictRoot::Pinned(root) => Some(root),
        crate::anchored::StrictRoot::Absent => None,
        crate::anchored::StrictRoot::Symlink => {
            return Err(CliError::Topology(format!(
                "refusing to continue: the state directory '{}' is a symlink",
                root_path.display()
            )));
        }
    };
    let Some(root) = root else {
        return Ok(None);
    };
    let key = percent_encode(agent_dir.as_os_str().as_bytes(), PYTHON_QUOTE).to_string();
    let rel = format!("{key}/manifest.json");
    match root.kind(&rel).map_err(|err| io_err(&path, err))? {
        None => return Ok(None),
        Some(EntryKind::Symlink) => {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' is a symlink and will not be followed",
                path.display()
            )));
        }
        Some(EntryKind::File) => {}
        Some(_) => {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' exists but is not a regular file",
                path.display()
            )));
        }
    }
    // The descriptor is re-walked with `O_NOFOLLOW` at use time, so a
    // swapped-in link or a retargeted state root fails closed instead of
    // being followed.
    let bytes = root
        .read_file(&rel)
        .map_err(|err| io_err(&path, err))?
        .ok_or_else(|| io_err(&path, std::io::Error::from(std::io::ErrorKind::NotFound)))?;
    let text = String::from_utf8(bytes).map_err(|err| {
        CliError::Manifest(format!(
            "ownership manifest '{}' is unreadable: {err}",
            path.display()
        ))
    })?;
    let data: Json = serde_json::from_str(&text).map_err(|err| {
        CliError::Manifest(format!(
            "ownership manifest '{}' is unreadable: {err}",
            path.display()
        ))
    })?;
    if data.get("version").and_then(Json::as_u64) != Some(MANIFEST_VERSION) {
        return Err(CliError::Manifest(format!(
            "ownership manifest '{}' has unsupported version {}",
            path.display(),
            data.get("version")
                .map(|version| version.to_string())
                .unwrap_or_else(|| "none".to_string())
        )));
    }
    validate_config(&data, &path)?;
    validate_config_modes(&data, &path)?;
    validate_symlinks(&data, &path)?;
    validate_preexisting(&data, &path)?;
    Ok(Some(data))
}

/// Reject malformed config-mode bookkeeping rather than letting it decode
/// as absence. `config_candidate_sha256` must be a string, and
/// `config_replace` must be an object whose `replaced_sha256` is a string
/// and whose `backup_text` is either a string or JSON null — the shapes
/// the installer writes for candidate and replace modes.
fn validate_config_modes(manifest: &Json, path: &Path) -> Result<(), CliError> {
    if let Some(hash) = manifest.get("config_candidate_sha256")
        && !hash.is_string()
    {
        return Err(CliError::Manifest(format!(
            "ownership manifest '{}' has a malformed 'config_candidate_sha256' \
             section: expected a string",
            path.display()
        )));
    }
    if let Some(replace) = manifest.get("config_replace") {
        let malformed = CliError::Manifest(format!(
            "ownership manifest '{}' has a malformed 'config_replace' \
             section: expected {{\"replaced_sha256\": <string>, \
             \"backup_text\": <string|null>}}",
            path.display()
        ));
        let Json::Object(fields) = replace else {
            return Err(malformed);
        };
        if !fields.get("replaced_sha256").is_some_and(Json::is_string) {
            return Err(malformed);
        }
        match fields.get("backup_text") {
            Some(Json::Null) | Some(Json::String(_)) => {}
            _ => return Err(malformed),
        }
    }
    Ok(())
}

/// Reject a malformed `preexisting` section rather than letting it decode
/// as absence. The section must be an array of safe relative string
/// paths, each one already recorded as a key under `files` — exactly the
/// shape the installer writes when it adopts a byte-identical file behind
/// a supported main->topology link. Anything else (a non-array section,
/// non-string or unsafe-path entries, or keys with no matching `files`
/// record) fails closed before any command mutates state; manifests
/// without the section (written before adoption existed) stay valid.
fn validate_preexisting(manifest: &Json, path: &Path) -> Result<(), CliError> {
    let Some(items) = manifest.get("preexisting") else {
        return Ok(());
    };
    let entries = items.as_array().ok_or_else(|| {
        CliError::Manifest(format!(
            "ownership manifest '{}' has a malformed 'preexisting' \
             section: expected an array of relative paths",
            path.display()
        ))
    })?;
    for item in entries {
        let Some(rel) = item.as_str() else {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' has a malformed 'preexisting' \
                 entry: expected a string",
                path.display()
            )));
        };
        if !fsx::safe_rel(rel) {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' has an unsafe 'preexisting' \
                 entry: {rel}",
                path.display()
            )));
        }
        let recorded = manifest
            .get("files")
            .and_then(Json::as_object)
            .is_some_and(|files| files.contains_key(rel));
        if !recorded {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' has an extraneous 'preexisting' \
                 entry '{rel}' with no matching 'files' record",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Reject malformed recorded main->topology link identities rather than
/// letting them decode as absence; each entry must be a full valid
/// identity (profile-visible path, target, content hash).
fn validate_symlinks(manifest: &Json, path: &Path) -> Result<(), CliError> {
    let Some(links) = manifest.get("symlinks") else {
        return Ok(());
    };
    let entries = links.as_object().ok_or_else(|| {
        CliError::Manifest(format!(
            "ownership manifest '{}' has a malformed 'symlinks' section: \
             expected an object",
            path.display()
        ))
    })?;
    for (rel, identity) in entries {
        if crate::symlinks::identity_from_json(identity).is_err() {
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' has a malformed 'symlinks[{rel}]' \
                 entry",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Reject malformed `config` section rather than letting it decode as
/// absence. Each section (`prior`, `installed`) must be an object keyed by
/// JSON-encoded string-path arrays (Python's `json.dumps(list(parts))`,
/// either separator spelling) whose entries are exactly
/// `{"absent": true}` or `{"value": <json>}` — the shapes
/// [`encode_values`] writes. Legacy manifests (installer v0.1/v0.2) also
/// record a top-level `config.file_existed` boolean; the value is obsolete
/// (the current installer never reads it), but the boolean form is
/// accepted, while a non-boolean `file_existed` fails closed like any
/// other malformed field. An absent `config` (legacy or freshly seeded
/// manifests) stays valid; anything else fails closed.
fn validate_config(manifest: &Json, path: &Path) -> Result<(), CliError> {
    let Some(config) = manifest.get("config") else {
        return Ok(());
    };
    let sections = config.as_object().ok_or_else(|| {
        CliError::Manifest(format!(
            "ownership manifest '{}' has a malformed 'config' section: \
             expected an object",
            path.display()
        ))
    })?;
    for (section, entries) in sections {
        // The legacy `file_existed` bookkeeping key is no longer used; its
        // boolean form is accepted, anything else fails closed.
        if section == "file_existed" {
            if entries.is_boolean() {
                continue;
            }
            return Err(CliError::Manifest(format!(
                "ownership manifest '{}' has a malformed \
                 'config.file_existed' value: expected a boolean",
                path.display()
            )));
        }
        let entries = entries.as_object().ok_or_else(|| {
            CliError::Manifest(format!(
                "ownership manifest '{}' has a malformed 'config.{section}' \
                 section: expected an object",
                path.display()
            ))
        })?;
        for (key, entry) in entries {
            serde_json::from_str::<Vec<String>>(key).map_err(|_| {
                CliError::Manifest(format!(
                    "ownership manifest '{}' has a malformed \
                     'config.{section}' key: {key}",
                    path.display()
                ))
            })?;
            let malformed = CliError::Manifest(format!(
                "ownership manifest '{}' has a malformed \
                 'config.{section}[{key}]' entry",
                path.display()
            ));
            let Json::Object(fields) = entry else {
                return Err(malformed);
            };
            let well_formed = fields.len() == 1
                && (fields.get("absent") == Some(&Json::Bool(true))
                    || fields.contains_key("value"));
            if !well_formed {
                return Err(malformed);
            }
        }
    }
    Ok(())
}

/// Canonical manifest bytes: sorted keys, two-space indent, trailing
/// newline — identical to the previous installer's serialization.
pub fn serialize_manifest(manifest: &Json) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(manifest)
        .expect("manifest serialization cannot fail: JSON value tree");
    text.push('\n');
    text.into_bytes()
}

/// Write the manifest with owner-only permissions. The state root is
/// pinned once and the write is descriptor-relative below it, so a
/// swapped-in symlink inside the state tree fails closed instead of being
/// followed.
pub fn write_manifest(agent_dir: &Path, repo_root: &Path, manifest: &Json) -> Result<(), CliError> {
    let path = manifest_path(agent_dir, repo_root)?;
    let root_path = state_root(repo_root)?;
    let root = RootFd::open_or_create_private(&root_path).map_err(|err| io_err(&root_path, err))?;
    let key = percent_encode(agent_dir.as_os_str().as_bytes(), PYTHON_QUOTE).to_string();
    let rel = format!("{key}/manifest.json");
    root.write_private(&rel, &serialize_manifest(manifest))
        .map_err(|err| io_err(&path, err))
}

/// Encode leaf values for the manifest: `None` records `{"absent": true}`,
/// `Some` records `{"value": <json>}`.
pub fn encode_values(values: &BTreeMap<Vec<String>, Option<Value>>) -> Map<String, Json> {
    let mut encoded = Map::new();
    for (parts, value) in values {
        // Key format matches Python's `json.dumps(list(parts))`, including
        // the ", " separator, so manifests stay byte-stable across the port.
        let keys: Vec<String> = parts
            .iter()
            .map(|part| serde_json::to_string(part).expect("string serialization cannot fail"))
            .collect();
        let key = format!("[{}]", keys.join(", "));
        let entry = match value {
            None => serde_json::json!({ "absent": true }),
            Some(value) => serde_json::json!({ "value": to_json(value) }),
        };
        encoded.insert(key, entry);
    }
    encoded
}

/// Decode manifest-encoded leaf values back to the strict model.
pub fn decode_values(encoded: &Json) -> BTreeMap<Vec<String>, Option<Value>> {
    let mut decoded = BTreeMap::new();
    let Some(map) = encoded.as_object() else {
        return decoded;
    };
    for (key, entry) in map {
        let Ok(parts) = serde_json::from_str::<Vec<String>>(key) else {
            continue;
        };
        let value = match entry.get("absent") {
            Some(absent) if absent.as_bool() == Some(true) => None,
            _ => entry.get("value").map(value::from_json),
        };
        decoded.insert(parts, value);
    }
    decoded
}

/// Manifest `str()`-style fallback for a missing topology version.
fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Float(number) => serde_json::Number::from_f64(*number)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Str(text) => Json::String(text.clone()),
        Value::Seq(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Map(map) => {
            let mut converted = Map::new();
            for (key, item) in map {
                converted.insert(key.clone(), to_json(item));
            }
            Json::Object(converted)
        }
    }
}
