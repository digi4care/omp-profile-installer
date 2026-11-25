//! Supported `~/.omp/agent/**` → `~/.omp/profiles/<name>/agent/**` symlinks.
//!
//! The default-to-topology pattern links a directory inside the main agent
//! directory (for example `~/.omp/agent/sessions`) into the installed
//! profile's own agent tree (`~/.omp/profiles/<name>/agent/sessions`). The
//! installer follows such a link ONLY for paths the topology declares, and
//! only when the link still points inside `~/.omp/profiles/**`. Everything
//! else — dangling links, links escaping `~/.omp/`, links retargeted after
//! install — is rejected fail-closed before any mutation.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::anchored::{self, EntryKind, RootFd, SubdirError};
use crate::error::{CliError, io_err};
use crate::fsx;

/// Identity of one supported main-agent-dir → profile-dir symlink. The
/// triple is recorded in the ownership manifest so update and remove can
/// recheck the link before touching anything behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkIdentity {
    /// Path of the symlink itself, relative to the main agent dir
    /// (for example `sessions` or `nested/dir`).
    pub link: String,
    /// The link's literal target, absolutized against the link's parent
    /// and lexically normalized.
    pub target: PathBuf,
    /// The canonical target; equivalent lexical paths behind different
    /// symlink hops collapse to the same directory here.
    pub canonical: PathBuf,
}

/// Where mutations for one topology-declared agent-dir path must go.
///
/// Every operation is descriptor-anchored: the variant pins the root
/// descriptor (`agent_root`/`profiles_root`) the resolved path lives
/// under, and the methods re-walk the final relative path component-by-
/// component with `O_NOFOLLOW` from that descriptor at use time, so a
/// path swapped between resolution and use fails closed instead of
/// being followed. `resolved` is informational (messages, manifest
/// recording, dry-run output); it is never used for a filesystem
/// operation.
#[derive(Clone)]
pub enum ResolvedAt {
    /// A plain path inside the agent directory. `agent_root` is `None`
    /// when the agent directory does not exist yet (nothing can be
    /// symlinked below it; mutations create it on first write).
    Direct {
        agent_dir: PathBuf,
        agent_root: Option<RootFd>,
        rel: String,
        resolved: PathBuf,
    },
    /// A path reached through a supported profile symlink; mutations go
    /// inside the profile's own agent tree, below `profiles_root`.
    Linked {
        profiles_root: RootFd,
        rel: String,
        resolved: PathBuf,
        identity: LinkIdentity,
    },
}

impl ResolvedAt {
    /// The resolved (already link-followed) display path.
    pub fn resolved(&self) -> &Path {
        match self {
            ResolvedAt::Direct { resolved, .. } | ResolvedAt::Linked { resolved, .. } => resolved,
        }
    }

    /// The recorded-link identity for a linked target.
    pub fn identity(&self) -> Option<&LinkIdentity> {
        match self {
            ResolvedAt::Direct { .. } => None,
            ResolvedAt::Linked { identity, .. } => Some(identity),
        }
    }

    /// The pinned root descriptor every operation re-walks from. A plain
    /// path whose agent directory does not exist yet is created on first
    /// write (mode `0o700` parents, like `fsx::makedirs_private`).
    fn root(&self) -> Result<RootFd, CliError> {
        match self {
            ResolvedAt::Direct {
                agent_root: Some(root),
                ..
            } => Ok(root.clone()),
            ResolvedAt::Direct {
                agent_dir,
                agent_root: None,
                ..
            } => RootFd::open_or_create_private(agent_dir).map_err(|err| io_err(agent_dir, err)),
            ResolvedAt::Linked { profiles_root, .. } => Ok(profiles_root.clone()),
        }
    }

    fn rel(&self) -> &str {
        match self {
            ResolvedAt::Direct { rel, .. } | ResolvedAt::Linked { rel, .. } => rel,
        }
    }

    fn io(&self, err: std::io::Error) -> CliError {
        io_err(self.resolved(), err)
    }

    /// `true` when the target exists (any entry kind).
    pub fn exists(&self) -> Result<bool, CliError> {
        match self {
            ResolvedAt::Direct {
                agent_root: None, ..
            } => Ok(false),
            _ => self.root()?.exists(self.rel()).map_err(|err| self.io(err)),
        }
    }

    /// `true` when the target is a regular file (a final-component
    /// symlink is never followed).
    pub fn is_file(&self) -> Result<bool, CliError> {
        match self {
            ResolvedAt::Direct {
                agent_root: None, ..
            } => Ok(false),
            _ => Ok(matches!(
                self.root()?.kind(self.rel()).map_err(|err| self.io(err))?,
                Some(EntryKind::File)
            )),
        }
    }

    /// `true` when any component of the resolved target path is a
    /// symlink — the pinned analogue of running
    /// `fsx::has_symlink_component` on the resolved path.
    pub fn has_symlink_component(&self) -> Result<bool, CliError> {
        match self {
            ResolvedAt::Direct {
                agent_root: None, ..
            } => Ok(false),
            _ => self
                .root()?
                .has_symlink_component(self.rel())
                .map_err(|err| self.io(err)),
        }
    }

    /// The target's bytes, or `None` when absent. A swapped-in symlink
    /// is an error, never something to read through.
    pub fn read(&self) -> Result<Option<Vec<u8>>, CliError> {
        match self {
            ResolvedAt::Direct {
                agent_root: None, ..
            } => Ok(None),
            _ => self
                .root()?
                .read_file(self.rel())
                .map_err(|err| self.io(err)),
        }
    }
}

/// Pinned descriptors and display paths for one command's symlink
/// resolution. Constructed once (every pin is a read-only open);
/// resolution itself is per-path.
pub struct Roots {
    /// The agent directory as resolved by the caller (display only).
    pub(crate) agent_dir: PathBuf,
    /// Pinned agent directory, or `None` when it does not exist yet.
    pub(crate) agent_root: Option<RootFd>,
    /// The main `~/.omp/profiles` directory (display only).
    pub(crate) profiles_dir: PathBuf,
    /// Pinned profiles root, or `None` when it does not exist.
    pub(crate) profiles_root: Option<RootFd>,
    /// The main `~/.omp/agent` tree (display only). `None` when $HOME
    /// is unset.
    main_agent: Option<PathBuf>,
    /// Pinned main agent tree, or `None` when it does not exist.
    main_agent_root: Option<RootFd>,
}

impl Roots {
    /// Pin the agent directory plus the main `~/.omp` trust roots derived
    /// from $HOME. `$HOME/.omp` itself is pinned ONCE, following only the
    /// documented `.omp` root symlink; `agent` and `profiles` below it
    /// are pinned `O_NOFOLLOW`, and a custom `--agent-dir` root is pinned
    /// with the same no-symlink-root policy — a symlink swapped in at a
    /// configured root fails closed instead of redirecting the command.
    /// A missing directory pins as `None` and only becomes an error when
    /// a symlink actually requires it; a symlinked `profiles` directory
    /// also pins as `None` (link resolution refuses it with its own
    /// message), while a symlinked agent root is refused outright.
    pub fn open(agent_dir: &Path) -> Result<Roots, CliError> {
        let home = home_dir().map(PathBuf::from);
        let omp_home = home.as_ref().map(|home| home.join(".omp"));
        let profiles_dir = omp_home
            .as_ref()
            .map(|omp| omp.join("profiles"))
            .unwrap_or_default();
        let main_agent = omp_home.as_ref().map(|omp| omp.join("agent"));
        let omp_root = match &home {
            Some(home) => {
                let home_pin = RootFd::open(home).map_err(|err| io_err(home, err))?;
                match home_pin.open_subdir_follow(".omp") {
                    Ok(pin) => Some(pin),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                    Err(err) => return Err(io_err(&home.join(".omp"), err)),
                }
            }
            None => None,
        };
        let profiles_root = match &omp_root {
            Some(omp) => strict_profiles_subdir(omp, &profiles_dir)?,
            None => None,
        };
        let main_agent_root = match (&omp_root, &main_agent) {
            (Some(omp), Some(path)) => strict_agent_subdir(omp, path)?,
            _ => None,
        };
        let agent_root = if Some(agent_dir) == main_agent.as_deref() {
            main_agent_root.clone()
        } else {
            match RootFd::open_strict_root(agent_dir).map_err(|err| io_err(agent_dir, err))? {
                anchored::StrictRoot::Pinned(pin) => Some(pin),
                anchored::StrictRoot::Absent => None,
                anchored::StrictRoot::Symlink => {
                    return Err(CliError::Topology(format!(
                        "refusing to continue: the agent directory '{}' is a symlink",
                        agent_dir.display()
                    )));
                }
            }
        };
        Ok(Roots {
            agent_dir: agent_dir.to_path_buf(),
            agent_root,
            profiles_dir,
            profiles_root,
            main_agent,
            main_agent_root,
        })
    }
}

/// Open `profiles` nofollow below the pinned `.omp` root. A symlinked
/// profiles directory pins as `None`: link resolution refuses it later
/// with the dangling-link message, while plain installs that never
/// resolve a link keep working.
fn strict_profiles_subdir(omp: &RootFd, display: &Path) -> Result<Option<RootFd>, CliError> {
    match omp.open_subdir(Path::new("profiles")) {
        Ok(sub) => Ok(Some(sub)),
        Err(SubdirError::NotFound) => Ok(None),
        Err(SubdirError::Io(err)) if err.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        Err(err) => Err(io_err(display, err.into())),
    }
}

/// Open `agent` nofollow below the pinned `.omp` root, refusing a
/// symlinked configured root outright (only the `.omp` directory itself
/// may be replaced by a symlink).
fn strict_agent_subdir(omp: &RootFd, display: &Path) -> Result<Option<RootFd>, CliError> {
    match omp.open_subdir(Path::new("agent")) {
        Ok(sub) => Ok(Some(sub)),
        Err(SubdirError::NotFound) => Ok(None),
        Err(SubdirError::Io(err)) if err.raw_os_error() == Some(libc::ELOOP) => {
            Err(CliError::Topology(format!(
                "refusing to continue: '{}' is a symlink; only the ~/.omp \
                 directory itself may be replaced by a symlink",
                display.display()
            )))
        }
        Err(err) => Err(io_err(display, err.into())),
    }
}

#[cfg(not(test))]
fn home_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME")
}

#[cfg(test)]
thread_local! {
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn home_dir() -> Option<std::ffi::OsString> {
    TEST_HOME
        .with(|slot| slot.borrow().clone())
        .map(|home| home.into_os_string())
        .or_else(|| std::env::var_os("HOME"))
}

/// Run `f` with $HOME redirected to `home`. The in-file install tests
/// drive the main-tree symlink checks through this hook because the
/// real $HOME is not writable during `cargo test`.
#[cfg(test)]
pub fn with_home_for_test<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    TEST_HOME.with(|slot| {
        let previous = slot.replace(Some(home.to_path_buf()));
        let result = f();
        slot.replace(previous);
        result
    })
}

/// Read the bytes of an already-resolved target through the matching
/// pinned descriptor: agent-tree paths through the agent root, link
/// targets under the profiles root through that pin. An absent file —
/// or a configured root that is not pinned because it does not exist
/// yet — is `Ok(None)`; nothing under a configured root is ever read
/// through a path string. Only paths outside every configured root
/// (read-only install inputs such as the topology repo's config) use
/// the lexical read, exactly as before this layer existed.
pub fn read_pinned(roots: &Roots, target: &Path) -> Result<Option<Vec<u8>>, CliError> {
    if target.strip_prefix(&roots.agent_dir).is_ok() {
        return match &roots.agent_root {
            Some(agent_root) => agent_root
                .read_file(
                    target
                        .strip_prefix(&roots.agent_dir)
                        .expect("prefix checked")
                        .to_string_lossy()
                        .as_ref(),
                )
                .map_err(|err| io_err(target, err)),
            None => Ok(None),
        };
    }
    if let Some(profiles_root) = &roots.profiles_root {
        if let Ok(sub) = target.strip_prefix(profiles_root.path()) {
            return profiles_root
                .read_file(&sub.to_string_lossy())
                .map_err(|err| io_err(target, err));
        }
    } else if !roots.profiles_dir.as_os_str().is_empty()
        && target.strip_prefix(&roots.profiles_dir).is_ok()
    {
        // Configured profiles root that is not pinned: fail closed.
        return Ok(None);
    }
    if !fsx::lexists(target) {
        return Ok(None);
    }
    let bytes = std::fs::read(target).map_err(|err| io_err(target, err))?;
    Ok(Some(bytes))
}

/// File permission bits of an already-resolved target, read through the
/// pinned descriptors exactly like [`read_pinned`]: `Ok(None)` when the
/// file is absent or the covering configured root is not pinned.
pub fn file_mode_pinned(roots: &Roots, rel: &str, target: &Path) -> io::Result<Option<u32>> {
    if target.strip_prefix(&roots.agent_dir).is_ok() {
        return match &roots.agent_root {
            Some(agent_root) => agent_root.file_mode(
                target
                    .strip_prefix(&roots.agent_dir)
                    .expect("prefix checked")
                    .to_string_lossy()
                    .as_ref(),
            ),
            None => Ok(None),
        };
    }
    if let Some(profiles_root) = &roots.profiles_root {
        if target.strip_prefix(profiles_root.path()).is_ok() {
            return profiles_root.file_mode(rel);
        }
    } else if !roots.profiles_dir.as_os_str().is_empty()
        && target.strip_prefix(&roots.profiles_dir).is_ok()
    {
        return Ok(None);
    }
    match std::fs::metadata(target) {
        Ok(meta) => Ok(Some(meta.permissions().mode() & 0o7777)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Resolve one topology-declared agent-dir path.
///
/// `rel` must be a manifest-safe relative path (`fsx::safe_rel`). Without
/// a symlink the result is the plain agent-dir path. The first symlinked
/// component below the agent dir is honored only when the agent dir IS
/// the real main `~/.omp/agent` tree (compared by pinned-descriptor
/// device/inode identity, the descriptor analogue of the old
/// canonicalization comparison) and the link's (lexical) target sits
/// inside the selected profile tree `~/.omp/profiles/<profile>/agent/**`;
/// the target is then opened component-by-component below the pinned
/// profiles descriptor, and every intermediate symlink hop must stay
/// inside the profiles root — a `profiles` entry symlinked outside the
/// OMP root refuses because the pin itself is opened `O_NOFOLLOW`, and a
/// nested redirect (e.g. `profiles/<selected>/agent` ->
/// `profiles/<other>/agent`) fails the canonical containment check.
/// Dangling links, links escaping `~/.omp/`, links retargeted after
/// install, and links that would escape through a nested symlink are all
/// rejected before any caller mutates anything.
///
/// The returned [`ResolvedAt`] pins the descriptor its operations
/// re-walk from, so the resolution result cannot be subverted by a
/// path swap between resolution and use.
pub fn resolve_declared_at(
    roots: &Roots,
    rel: &str,
    profile: &str,
) -> Result<ResolvedAt, CliError> {
    debug_assert!(fsx::safe_rel(rel));
    let direct = ResolvedAt::Direct {
        agent_dir: roots.agent_dir.clone(),
        agent_root: roots.agent_root.clone(),
        rel: rel.to_string(),
        resolved: roots.agent_dir.join(rel),
    };
    let Some(agent_root) = &roots.agent_root else {
        return Ok(direct);
    };

    // Find the first symlinked component below the agent dir, descending
    // through real directories pinned one at a time. A vanished or
    // replaced component simply ends the probe: the mutation itself
    // re-verifies everything at use time.
    let parts: Vec<&str> = rel.split('/').collect();
    let mut probe = roots.agent_dir.clone();
    let mut current = agent_root.clone();
    let mut link: Option<(usize, PathBuf)> = None;
    for (index, part) in parts.iter().enumerate() {
        probe.push(part);
        match current.kind(part).map_err(|err| io_err(&probe, err))? {
            Some(EntryKind::Symlink) => {
                let raw = current.read_link(part).map_err(|err| io_err(&probe, err))?;
                let parent_display = if index == 0 {
                    roots.agent_dir.clone()
                } else {
                    roots.agent_dir.join(parts[..index].join("/"))
                };
                link = Some((
                    index,
                    anchored::absolutize(&parent_display, raw.as_os_str()),
                ));
                break;
            }
            Some(EntryKind::Directory) => match current.open_subdir(Path::new(part)) {
                Ok(next) => current = next,
                Err(SubdirError::NotFound) => break,
                Err(err) => return Err(io_err(&probe, err.into())),
            },
            _ => break,
        }
    }
    let Some((link_index, link_target)) = link else {
        return Ok(direct);
    };

    // A symlink earns the supported main->profile treatment only when the
    // agent dir IS the real main `~/.omp/agent` tree. Deriving "profiles"
    // from `<agent-dir>/../profiles` would let an arbitrary explicit
    // --agent-dir bring its own trust root; every symlink under a custom
    // agent dir therefore fails closed here.
    let Some(main_agent) = &roots.main_agent else {
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' is a symlink, but $HOME is unset so \
             the main '~/.omp/agent' tree cannot be verified; refusing to \
             follow the link",
            probe.display(),
        )));
    };
    let Some(main_root) = &roots.main_agent_root else {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' is a symlink, but the main agent \
             tree '{}' could not be canonicalized ({err}); refusing to \
             follow the link",
            probe.display(),
            main_agent.display(),
        )));
    };
    let same_tree = agent_root.same_dir(main_root).map_err(|err| {
        CliError::Usage(format!(
            "refusing to install: '{}' is a symlink, but the agent dir \
             '{}' could not be canonicalized ({err}); refusing to follow \
             the link",
            probe.display(),
            roots.agent_dir.display(),
        ))
    })?;
    if !same_tree {
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' is a symlink and the agent dir '{}' \
             is not the main '~/.omp/agent' tree ('{}'); profile symlinks \
             are only supported for the default agent dir",
            probe.display(),
            roots.agent_dir.display(),
            main_root.path().display(),
        )));
    }

    // The link's lexical target must sit inside the real ~/.omp/profiles/
    // tree, in the supported `profiles/<profile>/agent/**` shape for the
    // selected topology profile.
    if !link_target.starts_with(&roots.profiles_dir) {
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' is a symlink pointing to '{}' \
             outside '{}'; only links into ~/.omp/profiles/ are supported",
            probe.display(),
            link_target.display(),
            roots.profiles_dir.display()
        )));
    }
    let rel_under_profiles = link_target
        .strip_prefix(&roots.profiles_dir)
        .expect("lexical containment checked above");
    let mut shape_parts = rel_under_profiles.components();
    let matches_profile = shape_parts
        .next()
        .is_some_and(|part| part.as_os_str() == profile)
        && shape_parts
            .next()
            .is_some_and(|part| part.as_os_str() == "agent");
    if !matches_profile {
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' points to '{}', which is not inside \
             the selected profile's 'profiles/{profile}/agent/' tree; \
             refusing to follow the link",
            probe.display(),
            link_target.display(),
        )));
    }
    // A dangling link has nothing behind it to write to; a nested
    // symlink redirecting outside the profiles root fails closed in the
    // bounded walk below. The opened descriptor's path is the link
    // target's canonical location, exactly what the manifest records.
    let Some(profiles_root) = &roots.profiles_root else {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' is a symlink with no usable target \
             '{}' ({err}); refusing to follow a dangling link",
            probe.display(),
            link_target.display()
        )));
    };
    let canonical =
        match profiles_root.canonical_entry_path(rel_under_profiles.to_str().ok_or_else(|| {
            CliError::Usage(format!(
                "refusing to install: '{}' resolves to a non-UTF-8 path; \
                 refusing to follow the link",
                probe.display(),
            ))
        })?) {
            Ok(path) => path,
            Err(SubdirError::NotFound) => {
                let err = std::io::Error::from(std::io::ErrorKind::NotFound);
                return Err(CliError::Usage(format!(
                    "refusing to install: '{}' is a symlink with no usable \
                 target '{}' ({err}); refusing to follow a dangling link",
                    probe.display(),
                    link_target.display()
                )));
            }
            Err(SubdirError::Escapes(escaped)) => {
                let canonical_omp = profiles_root
                    .path()
                    .parent()
                    .unwrap_or_else(|| profiles_root.path())
                    .to_path_buf();
                return Err(CliError::Usage(format!(
                    "refusing to install: '{}' resolves to '{}' outside the \
                 canonical OMP root '{}'; refusing to follow the link",
                    probe.display(),
                    escaped.display(),
                    canonical_omp.display()
                )));
            }
            Err(SubdirError::Io(err)) => {
                return Err(CliError::Usage(format!(
                    "refusing to install: '{}' is a symlink with no usable \
                 target '{}' ({err}); refusing to follow a dangling link",
                    probe.display(),
                    link_target.display()
                )));
            }
        };
    // The canonical target must live inside the selected profile's agent
    // tree: an intermediate component of profiles/ that is itself a
    // symlink (e.g. `profiles/<selected>/agent` -> `profiles/<other>/agent`)
    // must not redirect the write into another profile. `starts_with` is
    // component-aware, so a sibling prefix such as `.../agent2` cannot
    // smuggle past the check either.
    let canonical_agent_root = profiles_root.path().join(profile).join("agent");
    if !canonical.starts_with(&canonical_agent_root) {
        return Err(CliError::Usage(format!(
            "refusing to install: '{}' resolves to '{}' outside the \
             selected profile's 'profiles/{profile}/agent/' tree ('{}')",
            probe.display(),
            canonical.display(),
            canonical_agent_root.display()
        )));
    }

    // Mutations go to the link's canonical target joined with the
    // remaining components; every operation re-walks that path from the
    // pinned profiles descriptor, so a nested symlink inside the target
    // tree cannot redirect a write outside the profiles root and a
    // swapped-in link fails closed at use time.
    let remaining = parts[link_index + 1..].join("/");
    let resolved = if remaining.is_empty() {
        canonical.clone()
    } else {
        canonical.join(&remaining)
    };
    let tail = resolved
        .strip_prefix(profiles_root.path())
        .expect("resolved is built from the profiles root path");
    let tail = tail.to_str().ok_or_else(|| {
        CliError::Usage(format!(
            "refusing to install: '{}' resolves to a non-UTF-8 path; \
             refusing to follow the link",
            probe.display(),
        ))
    })?;
    let link_rel = parts[..=link_index].join("/");
    Ok(ResolvedAt::Linked {
        profiles_root: profiles_root.clone(),
        rel: tail.to_string(),
        resolved,
        identity: LinkIdentity {
            link: link_rel,
            target: link_target,
            canonical,
        },
    })
}

/// Manifest shape of the recorded identity: `{"link", "target",
/// "canonical"}` — all UTF-8 strings.
pub fn identity_json(identity: &LinkIdentity) -> serde_json::Value {
    serde_json::json!({
        "link": identity.link,
        "target": identity.target.display().to_string(),
        "canonical": identity.canonical.display().to_string(),
    })
}

/// Parse a recorded identity back from the manifest, validating the shape.
pub fn identity_from_json(value: &serde_json::Value) -> Result<LinkIdentity, CliError> {
    use serde_json::Value as Json;
    let object = value.as_object().ok_or_else(malformed)?;
    let text = |key: &str| -> Result<String, CliError> {
        object
            .get(key)
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(malformed)
    };
    Ok(LinkIdentity {
        link: text("link")?,
        target: PathBuf::from(text("target")?),
        canonical: PathBuf::from(text("canonical")?),
    })
}

fn malformed() -> CliError {
    CliError::Manifest("ownership manifest 'symlinks' is malformed".to_string())
}

/// Read the recorded link identities from a loaded ownership manifest.
/// A manifest without a `symlinks` section yields an empty map; the
/// manifest loader (`state::load_manifest`) has already validated the
/// section's shape, so only genuine identity entries decode here.
pub fn links_from_manifest(
    manifest: &serde_json::Value,
) -> std::collections::BTreeMap<String, LinkIdentity> {
    let mut links = std::collections::BTreeMap::new();
    let Some(section) = manifest
        .get("symlinks")
        .and_then(serde_json::Value::as_object)
    else {
        return links;
    };
    for (rel, value) in section {
        if let Ok(identity) = identity_from_json(value) {
            links.insert(rel.clone(), identity);
        }
    }
    links
}

/// Resolve a path the ownership manifest already covers. `recorded` is
/// the link identity stored at install time (`None` for a plain path). A
/// link where none was recorded, or a missing/changed recorded link,
/// fails closed instead of silently writing through a new destination.
pub fn resolve_owned_at(
    roots: &Roots,
    rel: &str,
    profile: &str,
    recorded: Option<&LinkIdentity>,
) -> Result<ResolvedAt, CliError> {
    let agent_dir = roots.agent_dir.clone();
    match resolve_declared_at(roots, rel, profile) {
        Ok(target @ ResolvedAt::Direct { .. }) => match recorded {
            None => Ok(target),
            Some(identity) => Err(CliError::Usage(format!(
                "refusing to continue: '{}' was installed through the profile \
                 link '{}', but '{}' is no longer a symlink; refusing to \
                 touch the now-plain path",
                agent_dir.join(rel).display(),
                agent_dir.join(&identity.link).display(),
                agent_dir.join(&identity.link).display(),
            ))),
        },
        Ok(target) => {
            let resolved = target.resolved().to_path_buf();
            let identity = target
                .identity()
                .cloned()
                .expect("linked targets always carry an identity");
            match recorded {
                Some(expected) if *expected == identity => Ok(target),
                Some(_) => Err(CliError::Usage(format!(
                    "refusing to continue: '{}' is a symlink whose target changed \
                     since install (was '{}', now '{}'); refusing to follow a \
                     retargeted link",
                    agent_dir.join(rel).display(),
                    expected_target(recorded),
                    identity.target.display(),
                ))),
                None => Err(CliError::Usage(format!(
                    "refusing to continue: '{}' gained a profile symlink where \
                     the ownership manifest recorded a plain path; refusing to \
                     follow it",
                    resolved.display(),
                ))),
            }
        }
        Err(err) => Err(CliError::Usage(format!(
            "refusing to continue: '{}' no longer resolves as installed: \
             {err}",
            agent_dir.join(rel).display()
        ))),
    }
}

fn expected_target(recorded: Option<&LinkIdentity>) -> String {
    recorded
        .map(|identity| identity.target.display().to_string())
        .unwrap_or_default()
}
